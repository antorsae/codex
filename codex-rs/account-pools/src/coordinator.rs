use crate::AccountBackend;
use crate::AccountStore;
use crate::ManagedBackend;
use crate::policy::Decision;
use crate::policy::WindowKind;
use crate::policy::decide;
use crate::policy::usable;
use crate::storage::read_json;
use crate::storage::write_json;
use anyhow::Context;
use anyhow::Result;
use anyhow::bail;
use codex_login::AuthManager;
use codex_login::CodexAuth;
use codex_login::ExternalAuth;
use codex_login::ExternalAuthFuture;
use codex_login::ExternalAuthRefreshContext;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::PoolWaitReason;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecoveryState {
    selection: AccountSelection,
    account: String,
    waiting: bool,
}

struct SessionAuth {
    current: RwLock<Arc<AuthManager>>,
}

impl ExternalAuth for SessionAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async move {
            let manager = self
                .current
                .read()
                .map_err(|_| std::io::Error::other("Account selection lock failed"))?
                .clone();
            manager.reload().await;
            manager
                .auth()
                .await
                .ok_or_else(|| std::io::Error::other("Account requires login"))
        })
    }
    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async move {
            let manager = self
                .current
                .read()
                .map_err(|_| std::io::Error::other("Account selection lock failed"))?
                .clone();
            if manager.auth_cached().and_then(|auth| auth.get_account_id())
                != context.previous_account_id
            {
                return self.resolve().await;
            }
            manager.refresh_token().await?;
            manager
                .auth()
                .await
                .ok_or_else(|| std::io::Error::other("Account requires login"))
        })
    }
}

/// A session owns its selection; only credential refresh and reset transactions are shared.
pub struct PoolSession {
    store: AccountStore,
    selection: AccountSelection,
    accounts: Vec<ManagedAccount>,
    redeem_weekly: bool,
    waiting: AtomicBool,
    current: Mutex<usize>,
    denied_until: Mutex<Vec<i64>>,
    selected_auth: Arc<SessionAuth>,
    auth: Arc<AuthManager>,
    recovery_path: RwLock<Option<PathBuf>>,
}

impl PoolSession {
    pub fn saved_selection(
        store: &AccountStore,
        thread_id: &str,
    ) -> Result<Option<AccountSelection>> {
        crate::storage::validate_name(thread_id)?;
        Ok(read_json::<RecoveryState>(
            &store
                .root()
                .join("sessions")
                .join(format!("{thread_id}.json")),
        )?
        .map(|state| state.selection))
    }

    pub async fn open(
        store: AccountStore,
        explicit: Option<AccountSelection>,
        resume_id: Option<&str>,
    ) -> Result<Option<Arc<Self>>> {
        let saved = match resume_id {
            Some(id) => {
                crate::storage::validate_name(id)?;
                read_json::<RecoveryState>(
                    &store.root().join("sessions").join(format!("{id}.json")),
                )?
            }
            None => None,
        };
        let config = store.read()?;
        let Some(selection) = explicit
            .or_else(|| saved.as_ref().map(|state| state.selection.clone()))
            .or(config.default_selection)
        else {
            return Ok(None);
        };
        let (aliases, redeem_weekly) = match &selection {
            AccountSelection::Account(alias) => (vec![alias.clone()], false),
            AccountSelection::Pool(name) => {
                let pool = config
                    .pools
                    .iter()
                    .find(|pool| &pool.name == name)
                    .context("Unknown pool")?;
                (pool.accounts.clone(), pool.redeem_weekly_resets)
            }
        };
        let accounts: Vec<_> = aliases
            .iter()
            .map(|alias| {
                config
                    .accounts
                    .iter()
                    .find(|account| &account.alias == alias)
                    .cloned()
                    .context("Unknown account")
            })
            .collect::<Result<_>>()?;
        let current = saved
            .as_ref()
            .filter(|saved| saved.selection == selection)
            .and_then(|saved| {
                accounts
                    .iter()
                    .position(|account| account.alias == saved.account)
            })
            .unwrap_or(0);
        let manager = store.manager(&accounts[current]).await?;
        let selected_auth = Arc::new(SessionAuth {
            current: RwLock::new(manager),
        });
        let auth = AuthManager::managed_from_auth_config(store.auth_config.clone()).await;
        auth.set_external_auth(selected_auth.clone()).await?;
        let denied_until = Mutex::new(vec![0; accounts.len()]);
        let waiting = AtomicBool::new(
            saved
                .as_ref()
                .is_some_and(|state| state.selection == selection && state.waiting),
        );
        Ok(Some(Arc::new(Self {
            store,
            selection,
            accounts,
            redeem_weekly,
            denied_until,
            waiting,
            current: Mutex::new(current),
            selected_auth,
            auth,
            recovery_path: RwLock::new(None),
        })))
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.auth.clone()
    }

    pub async fn selected_account(&self) -> ManagedAccount {
        let mut account = self.accounts[*self.current.lock().await].clone();
        if let Some(auth) = self.auth.auth_cached() {
            account.email = auth.get_account_email();
            account.plan = auth
                .account_plan_type()
                .and_then(|plan| serde_json::to_value(plan).ok())
                .and_then(|value| value.as_str().map(str::to_owned));
        }
        account
    }

    pub fn is_waiting(&self) -> bool {
        self.waiting.load(Ordering::Acquire)
    }

    pub fn selection(&self) -> &AccountSelection {
        &self.selection
    }

    pub async fn bind_thread(&self, thread_id: &str) -> Result<()> {
        crate::storage::validate_name(thread_id)?;
        *self
            .recovery_path
            .write()
            .map_err(|_| anyhow::anyhow!("Recovery lock failed"))? = Some(
            self.store
                .root()
                .join("sessions")
                .join(format!("{thread_id}.json")),
        );
        self.persist(*self.current.lock().await, self.is_waiting())
    }

    fn persist(&self, current: usize, waiting: bool) -> Result<()> {
        if let Some(path) = self
            .recovery_path
            .read()
            .map_err(|_| anyhow::anyhow!("Recovery lock failed"))?
            .as_ref()
        {
            write_json(
                path,
                &RecoveryState {
                    selection: self.selection.clone(),
                    account: self.accounts[current].alias.clone(),
                    waiting,
                },
            )?;
        }
        Ok(())
    }

    pub async fn recover(
        &self,
        model: &str,
        cancel: &CancellationToken,
        notify: impl FnMut(AccountPoolEvent),
    ) -> Result<()> {
        self.recover_with(
            &ManagedBackend::new(self.store.clone()),
            model,
            cancel,
            notify,
        )
        .await
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "A session must serialize recovery and keep its selected identity and rejection evidence stable until recovery completes or is cancelled; these locks are never shared between sessions"
    )]
    pub(crate) async fn recover_with(
        &self,
        backend: &impl AccountBackend,
        model: &str,
        cancel: &CancellationToken,
        mut notify: impl FnMut(AccountPoolEvent),
    ) -> Result<()> {
        tokio::select! {
            biased;
            _ = cancel.cancelled() => bail!("Account recovery cancelled"),
            result = async {
                let started = tokio::time::Instant::now();
                let wall_start = chrono::Utc::now().timestamp_millis();
                let now = || ((wall_start + started.elapsed().as_millis() as i64) / 1000).max(chrono::Utc::now().timestamp());
                let mut current = self.current.lock().await;
                self.waiting.store(/*val*/ true, Ordering::Release);
                // A model rejection is newer evidence than an eventually consistent usage read.
                let mut denied_until = self.denied_until.lock().await;
                denied_until[*current] = now() + 60;
                self.persist(*current, /*waiting*/ true)?;
                let mut failures = 0u32;
                let mut redemption_guard = None;
                loop {
                    // At most 128 pool members and 32 concurrent reads keep all observations
                    // within the 90-second freshness bound (each read has a 20-second timeout).
                    let mut reads = Vec::with_capacity(self.accounts.len());
                    for account in &self.accounts { reads.push(backend.usage(account, Some(model))); }
                    let mut usages: Vec<_> = futures::stream::iter(reads).buffered(32).collect().await;
                    let now = now();
                    for (index, usage) in usages.iter_mut().enumerate() {
                        if denied_until[index] > now && usage.ordinary_usage_allowed == Some(true) {
                            usage.ordinary_usage_allowed = Some(false);
                        }
                    }
                    let trigger = if crate::policy::blocked(&usages[*current], WindowKind::Weekly) {
                        WindowKind::Weekly
                    } else { WindowKind::Short };
                    let mut decision = decide(&usages, *current, trigger, self.redeem_weekly, now);
                    if !matches!(decision, Decision::Use(_)) {
                        for (index, account) in self.accounts.iter().enumerate() {
                            if let Some(credit) = crate::redemption::pending_credit(&self.store, account)? {
                                // An old intent is not new permission to spend a credit.
                                // Reconcile only while the automatic weekly policy still applies.
                                if self.redeem_weekly && usages[index].model_supported == Some(true)
                                    && matches!(decision, Decision::Redeem(..) | Decision::Wait(PoolWaitReason::WeeklyQuota, _))
                                {
                                    decision = Decision::Redeem(index, credit);
                                } else if matches!(decision, Decision::Redeem(..)) {
                                    decision = Decision::Wait(PoolWaitReason::RedemptionPending, now + 60);
                                }
                                break;
                            }
                        }
                    }
                    if matches!(decision, Decision::Redeem(..)) && redemption_guard.is_none() {
                        redemption_guard = Some(crate::storage::lock(&self.store.root().join("recovery.lock")).await?);
                        // Another pool may have reset a shared account while this session waited.
                        continue;
                    }
                    let (reason, mut next) = match decision {
                        Decision::Use(index) => {
                            let fresh = backend.usage(&self.accounts[index], Some(model)).await;
                            if usable(&fresh, chrono::Utc::now().timestamp()) {
                                let previous = self.accounts[*current].alias.clone();
                                self.activate(index, &mut current).await?;
                                crate::redemption::observe_usable(&self.store, &self.accounts[index]).await?;
                                if previous != self.accounts[index].alias {
                                    notify(AccountPoolEvent::Switched { account: self.accounts[index].alias.clone(), previous_account: previous });
                                }
                                self.persist(index, /*waiting*/ false)?;
                                self.waiting.store(/*val*/ false, Ordering::Release);
                                return Ok(());
                            }
                            (PoolWaitReason::UnknownAvailability, now + 60)
                        }
                        Decision::Redeem(index, credit) => {
                            match crate::redemption::redeem(&self.store, backend, &self.accounts[index], Some(model), &credit, crate::redemption::RedemptionMode::Automatic).await {
                                Ok(fresh) if usable(&fresh, chrono::Utc::now().timestamp()) => {
                                    denied_until[index] = 0;
                                    notify(AccountPoolEvent::Redeemed { account: self.accounts[index].alias.clone() });
                                    let previous = self.accounts[*current].alias.clone();
                                    self.activate(index, &mut current).await?;
                                    if previous != self.accounts[index].alias {
                                        notify(AccountPoolEvent::Switched { account: self.accounts[index].alias.clone(), previous_account: previous });
                                    }
                                    self.persist(index, /*waiting*/ false)?;
                                    self.waiting.store(/*val*/ false, Ordering::Release);
                                    return Ok(());
                                }
                                Ok(_) | Err(_) => (PoolWaitReason::RedemptionPending, now + 60),
                            }
                        }
                        Decision::Wait(reason, next) => (reason, next),
                    };
                    drop(redemption_guard.take());
                    if reason == PoolWaitReason::UnknownAvailability {
                        failures = (failures + 1).min(3);
                        // Back off failed reads, but still honor an earlier advertised reset.
                        if next == now + 60 {
                            let backoff = now + (60i64 << failures).min(300);
                            next = usages.iter().flat_map(|usage| &usage.windows)
                                .filter_map(|window| window.resets_at).filter(|reset| *reset > now)
                                .min().unwrap_or(backoff).min(backoff);
                        }
                    } else { failures = 0; }
                    notify(AccountPoolEvent::Waiting { account: self.accounts[*current].alias.clone(), reason, next_check_at: next });
                    tokio::time::sleep(Duration::from_secs((next - now).max(1) as u64)).await;
                }
            } => result,
        }
    }

    async fn activate(&self, index: usize, current: &mut usize) -> Result<()> {
        if index != *current {
            let manager = self.store.manager(&self.accounts[index]).await?;
            *self
                .selected_auth
                .current
                .write()
                .map_err(|_| anyhow::anyhow!("Account selection lock failed"))? = manager;
            self.auth.reload().await;
            *current = index;
        }
        Ok(())
    }
}
