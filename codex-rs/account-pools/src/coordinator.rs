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
use codex_protocol::account_pool::AccountPoolConfig;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::PoolWaitReason;
use futures::StreamExt;
use serde::Deserialize;
use serde::Serialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::RwLock;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

#[path = "continuity.rs"]
mod continuity;

#[derive(Clone, Debug, Deserialize, Serialize)]
struct RecoveryState {
    selection: AccountSelection,
    account: String,
    waiting: bool,
}

struct SessionAuth {
    current: Arc<AuthManager>,
}

impl ExternalAuth for SessionAuth {
    fn resolve(&self) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async move {
            let manager = &self.current;
            manager.reload().await;
            manager
                .auth()
                .await
                .ok_or_else(|| std::io::Error::other("Account requires login"))
        })
    }
    fn refresh(&self, context: ExternalAuthRefreshContext) -> ExternalAuthFuture<'_, CodexAuth> {
        Box::pin(async move {
            let manager = &self.current;
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

#[derive(Clone, PartialEq, Eq)]
struct PoolMembership {
    accounts: Vec<ManagedAccount>,
    redeem_weekly: bool,
}

impl PoolMembership {
    fn from_config(config: &AccountPoolConfig, selection: &AccountSelection) -> Result<Self> {
        // Accounts and pools come from the same validated, atomically replaced file.
        let (aliases, redeem_weekly) = match selection {
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
        let accounts = aliases
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
        Ok(Self {
            accounts,
            redeem_weekly,
        })
    }
}

/// A session owns its selection; only credential refresh and reset transactions are shared.
pub struct PoolSession {
    store: AccountStore,
    selection: AccountSelection,
    membership: RwLock<PoolMembership>,
    waiting: AtomicBool,
    needs_preflight: AtomicBool,
    // Metadata reads remain available while recovery holds `denied_until` to wait for quota.
    current: RwLock<ManagedAccount>,
    denied_until: Mutex<HashMap<(String, String), i64>>,
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
            .or_else(|| config.default_selection.clone())
        else {
            return Ok(None);
        };
        let membership = PoolMembership::from_config(&config, &selection)?;
        let accounts = &membership.accounts;
        let current = saved
            .as_ref()
            .filter(|saved| saved.selection == selection)
            .and_then(|saved| {
                accounts
                    .iter()
                    .position(|account| account.alias == saved.account)
            })
            .or_else(|| continuity::preferred_account(&store, &selection, accounts))
            .unwrap_or(0);
        let manager = store.manager(&accounts[current]).await?;
        let selected_auth = Arc::new(SessionAuth { current: manager });
        let auth = AuthManager::managed_from_auth_config(store.auth_config.clone()).await;
        auth.set_external_auth(selected_auth).await?;
        let current = accounts[current].clone();
        let waiting = AtomicBool::new(
            saved
                .as_ref()
                .is_some_and(|state| state.selection == selection && state.waiting),
        );
        Ok(Some(Arc::new(Self {
            store,
            selection,
            membership: RwLock::new(membership),
            denied_until: Mutex::new(HashMap::new()),
            waiting,
            needs_preflight: AtomicBool::new(/*v*/ true),
            current: RwLock::new(current),
            auth,
            recovery_path: RwLock::new(None),
        })))
    }

    pub fn auth_manager(&self) -> Arc<AuthManager> {
        self.auth.clone()
    }

    pub async fn selected_account(&self) -> ManagedAccount {
        let mut account = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        if let Some(auth) = self.auth.auth_cached().filter(|auth| {
            auth.get_chatgpt_user_id().as_deref() == Some(&account.user_id)
                && auth.get_account_id().as_deref() == Some(&account.workspace_id)
        }) {
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

    /// The last valid pool membership and policy observed by this session.
    pub fn pool(&self) -> Option<codex_protocol::account_pool::AccountPool> {
        let membership = self
            .membership
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match &self.selection {
            AccountSelection::Pool(name) => Some(codex_protocol::account_pool::AccountPool {
                name: name.clone(),
                accounts: membership
                    .accounts
                    .iter()
                    .map(|account| account.alias.clone())
                    .collect(),
                redeem_weekly_resets: membership.redeem_weekly,
            }),
            AccountSelection::Account(_) => None,
        }
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
        self.persist(&self.selected_account().await, self.is_waiting())
    }

    fn persist(&self, current: &ManagedAccount, waiting: bool) -> Result<()> {
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
                    account: current.alias.clone(),
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
        .await?;
        self.needs_preflight.store(/*val*/ false, Ordering::Release);
        Ok(())
    }

    #[expect(
        clippy::await_holding_invalid_type,
        reason = "A session must serialize recovery and keep its rejection evidence stable until recovery completes or is cancelled; this lock is never shared between sessions"
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
                // A model rejection is newer evidence than an eventually consistent usage read.
                let mut denied_until = self.denied_until.lock().await;
                let current = self.selected_account().await;
                self.waiting.store(/*val*/ true, Ordering::Release);
                denied_until.insert((current.user_id.clone(), current.workspace_id.clone()), now() + 60);
                self.persist(&current, /*waiting*/ true)?;
                let mut failures = 0u32;
                let mut redemption_guard = None;
                let read_membership = || PoolMembership::from_config(&self.store.read()?, &self.selection);
                loop {
                    let membership = match read_membership() {
                        Ok(membership) => membership,
                        Err(_) => {
                            // External editors can bypass atomic replacement. A missing, partial,
                            // or invalid definition must pause recovery, never end the turn or
                            // authorize switching/resetting from stale configuration.
                            drop(redemption_guard.take());
                            notify(AccountPoolEvent::Waiting {
                                account: current.alias.clone(),
                                reason: PoolWaitReason::UnknownAvailability,
                                next_check_at: now() + 60,
                            });
                            tokio::time::sleep(Duration::from_secs(/*secs*/ 60)).await;
                            continue;
                        }
                    };
                    *self.membership.write().unwrap_or_else(std::sync::PoisonError::into_inner) = membership.clone();
                    let accounts = &membership.accounts;
                    // Account identities, rather than changing pool indexes, own rejection evidence.
                    denied_until.retain(|(user, workspace), _| accounts.iter().any(|account| {
                        &account.user_id == user && &account.workspace_id == workspace
                    }));
                    let current_index = accounts.iter().position(|account| {
                        account.user_id == current.user_id && account.workspace_id == current.workspace_id
                    }).unwrap_or(accounts.len());
                    // At most 128 pool members and 32 concurrent reads keep all observations
                    // within the 90-second freshness bound (each read has a 20-second timeout).
                    let mut reads = Vec::with_capacity(accounts.len());
                    for account in accounts { reads.push(backend.usage(account, Some(model))); }
                    let mut usages: Vec<_> = futures::stream::iter(reads).buffered(32).collect().await;
                    // A pool update during network reads invalidates the whole decision, including
                    // removals and weekly-reset policy changes. No config lock spans network I/O.
                    if read_membership().ok().as_ref() != Some(&membership) {
                        drop(redemption_guard.take());
                        continue;
                    }
                    let now = now();
                    for (index, usage) in usages.iter_mut().enumerate() {
                        let account = &accounts[index];
                        if denied_until.get(&(account.user_id.clone(), account.workspace_id.clone())).is_some_and(|until| *until > now)
                            && usage.ordinary_usage_allowed == Some(true) {
                            usage.ordinary_usage_allowed = Some(false);
                        }
                    }
                    let trigger = if usages.get(current_index).is_some_and(|usage| crate::policy::blocked(usage, WindowKind::Weekly)) {
                        WindowKind::Weekly
                    } else { WindowKind::Short };
                    let mut decision = decide(&usages, current_index, trigger, membership.redeem_weekly, now);
                    if !matches!(decision, Decision::Use(_)) {
                        for (index, account) in accounts.iter().enumerate() {
                            if let Some(credit) = crate::redemption::pending_credit(&self.store, account)? {
                                // An old intent is not new permission to spend a credit.
                                // Reconcile only while the automatic weekly policy still applies.
                                if membership.redeem_weekly && usages[index].model_supported == Some(true)
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
                            let fresh = backend.usage(&accounts[index], Some(model)).await;
                            if usable(&fresh, chrono::Utc::now().timestamp())
                                && read_membership().ok().as_ref() == Some(&membership)
                                && self.activate(&accounts[index]).await.is_ok()
                            {
                                crate::redemption::observe_usable(&self.store, &accounts[index]).await?;
                                if current.alias != accounts[index].alias {
                                    notify(AccountPoolEvent::Switched { account: accounts[index].alias.clone(), previous_account: current.alias.clone() });
                                }
                                self.persist(&accounts[index], /*waiting*/ false)?;
                                self.waiting.store(/*val*/ false, Ordering::Release);
                                return Ok(());
                            }
                            (PoolWaitReason::UnknownAvailability, now + 60)
                        }
                        Decision::Redeem(index, credit) => {
                            match crate::redemption::redeem(&self.store, backend, &accounts[index], Some(model), &credit, crate::redemption::RedemptionMode::Automatic).await {
                                Ok(fresh) if usable(&fresh, chrono::Utc::now().timestamp()) => {
                                    denied_until.remove(&(accounts[index].user_id.clone(), accounts[index].workspace_id.clone()));
                                    notify(AccountPoolEvent::Redeemed { account: accounts[index].alias.clone() });
                                    if read_membership().ok().as_ref() != Some(&membership)
                                        || self.activate(&accounts[index]).await.is_err() {
                                        drop(redemption_guard.take());
                                        continue;
                                    }
                                    if current.alias != accounts[index].alias {
                                        notify(AccountPoolEvent::Switched { account: accounts[index].alias.clone(), previous_account: current.alias.clone() });
                                    }
                                    self.persist(&accounts[index], /*waiting*/ false)?;
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
                    notify(AccountPoolEvent::Waiting { account: current.alias.clone(), reason, next_check_at: next });
                    tokio::time::sleep(Duration::from_secs((next - now).max(1) as u64)).await;
                }
            } => result,
        }
    }

    async fn activate(&self, account: &ManagedAccount) -> Result<()> {
        let current = self.selected_account().await;
        if account.user_id != current.user_id || account.workspace_id != current.workspace_id {
            let manager = self.store.manager(account).await?;
            // Resolve successfully before replacing the session's provider and cached credentials.
            self.auth
                .set_external_auth(Arc::new(SessionAuth { current: manager }))
                .await?;
        }
        *self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = account.clone();
        Ok(())
    }
}
