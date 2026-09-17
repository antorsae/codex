//! A bounded, display-only quota cache. Recovery and explicit usage requests always bypass it.
//! Server-side, each read is also answered from observations shared across sessions, so several
//! TUIs watching the same pool cost one backend request per interval, not one each.

use crate::chatwidget::AccountStatusSnapshot;
use codex_app_server_client::AppServerRequestHandle;
use codex_app_server_client::TypedRequestError;
use codex_app_server_protocol::ClientRequest;
use codex_app_server_protocol::ManagedAccountAction;
use codex_app_server_protocol::ManagedAccountParams;
use codex_app_server_protocol::ManagedAccountResponse;
use codex_app_server_protocol::RequestId;
use codex_protocol::ThreadId;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use std::collections::HashMap;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::atomic::Ordering;
use std::time::Duration;
use tokio::sync::Mutex;

#[cfg(test)]
#[path = "account_status_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "account_status_rpc_tests.rs"]
mod rpc_tests;

#[derive(Default)]
pub(crate) struct AccountStatusCache {
    entries: HashMap<String, Entry>,
    last_reset_sweep: Option<i64>,
    legacy_quota: Arc<AtomicBool>,
    // Network reads release the lock; a replaced worker must not write into its successor's cache.
    generation: u64,
}

/// Manual reads and mutations invalidate named observations without waiting on footer work.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct AccountStatusInvalidations {
    aliases: HashSet<String>,
    all_accounts: bool,
}

impl AccountStatusInvalidations {
    pub(crate) fn extend<'a>(&mut self, aliases: impl Iterator<Item = &'a str>) {
        for alias in aliases {
            if self.all_accounts {
                break;
            }
            if self.aliases.len() == 129 && !self.aliases.contains(alias) {
                self.aliases.clear();
                self.all_accounts = true;
                break;
            }
            self.aliases.insert(alias.to_owned());
        }
    }
}

/// Replacing the displayed widget cancels obsolete work; dropping its server does the same.
#[derive(Default)]
pub(crate) struct AccountStatusTask(pub(crate) Option<tokio::task::JoinHandle<()>>);

impl Drop for AccountStatusTask {
    fn drop(&mut self) {
        if let Some(task) = &self.0 {
            task.abort();
        }
    }
}

struct Entry {
    account: ManagedAccount,
    usage: Option<ManagedAccountUsage>,
    attempted_at: i64,
    refresh_requested: bool,
}

fn weekly(usage: &ManagedAccountUsage) -> Option<&AccountQuotaWindow> {
    if usage.error.is_some() {
        return None;
    }
    usage
        .windows
        .iter()
        .filter(|window| {
            window.limit_id == "codex"
                && window.window_minutes == 7 * 24 * 60
                && (0.0..=100.0).contains(&window.remaining_percent)
        })
        .min_by(|left, right| left.remaining_percent.total_cmp(&right.remaining_percent))
}

fn future_reset(window: &AccountQuotaWindow, after: i64) -> Option<i64> {
    window.resets_at.filter(|reset| {
        *reset > after && chrono::DateTime::from_timestamp(*reset, /*nsecs*/ 0).is_some()
    })
}

/// Footer reads are display-only. Inference responses already refresh the selected account's
/// quota through the server-side observation store, so these intervals only bound how stale a
/// footer can get while a session is idle.
const ACTIVE_REFRESH_SECS: i64 = 300;
const INACTIVE_REFRESH_SECS: i64 = 1800;
const ACTIVE_ERROR_RETRY_SECS: i64 = 300;
const INACTIVE_ERROR_RETRY_SECS: i64 = 900;
const EXHAUSTED_REFRESH_SECS: i64 = 3600;
/// A pending read gets this long to finish before its predecessor stops being displayed.
const GRACE_SECS: i64 = 300;

impl Entry {
    fn refresh_at(&self, selected: &str) -> i64 {
        let active = self.account.alias == selected;
        let Some((usage, weekly)) = self
            .usage
            .as_ref()
            .and_then(|usage| weekly(usage).map(|weekly| (usage, weekly)))
        else {
            // Retry the selected account sooner; unavailable inactive accounts get backoff.
            return self.attempted_at
                + if active {
                    ACTIVE_ERROR_RETRY_SECS
                } else {
                    INACTIVE_ERROR_RETRY_SECS
                };
        };
        // The server may answer from an observation older than this request; count the interval
        // from the request so a shared observation is not re-requested on every tick.
        let since = usage.checked_at.max(self.attempted_at);
        if weekly.remaining_percent == 0.0
            && let Some(reset) = future_reset(weekly, since)
        {
            // A short-window reset cannot restore exhausted weekly quota.
            return since.saturating_add(EXHAUSTED_REFRESH_SECS).min(reset);
        }
        let interval = if active {
            ACTIVE_REFRESH_SECS
        } else {
            INACTIVE_REFRESH_SECS
        };
        // A reset that already passed when the observation was served is not a reason to ask
        // again on every tick; the served observation is re-read once its own age warrants it.
        usage
            .windows
            .iter()
            .filter(|window| window.limit_id == "codex")
            .filter_map(|window| future_reset(window, since))
            .min()
            .unwrap_or(i64::MAX)
            .min(since.saturating_add(interval))
    }

    fn valid_until(&self, selected: &str) -> Option<i64> {
        let usage = self.usage.as_ref()?;
        let weekly = weekly(usage)?;
        let since = usage.checked_at.max(self.attempted_at);
        if weekly.remaining_percent == 0.0
            && let Some(reset) = future_reset(weekly, since)
        {
            // Allow a pending hourly read five minutes to finish, but never cross a reset.
            return Some(
                since
                    .saturating_add(EXHAUSTED_REFRESH_SECS + GRACE_SECS)
                    .min(reset),
            );
        }
        let interval = if self.account.alias == selected {
            ACTIVE_REFRESH_SECS
        } else {
            INACTIVE_REFRESH_SECS
        };
        Some(since.saturating_add(interval + GRACE_SECS))
    }
}

impl AccountStatusCache {
    fn invalidate(&mut self, pending: AccountStatusInvalidations) {
        for (alias, entry) in &mut self.entries {
            if pending.all_accounts || pending.aliases.contains(alias) {
                entry.refresh_requested = true;
            }
        }
    }

    fn begin_reset_sweep(&mut self, refreshed: &HashSet<String>, now: i64) {
        self.last_reset_sweep = Some(now);
        // Persist remaining members so cancellation cannot restart completed batches forever.
        for (alias, entry) in &mut self.entries {
            if !refreshed.contains(alias) {
                entry.refresh_requested = true;
            }
        }
    }

    fn retain_members(&mut self, accounts: &[ManagedAccount]) {
        self.entries.retain(|alias, entry| {
            accounts.iter().any(|account| {
                &account.alias == alias
                    && account.user_id == entry.account.user_id
                    && account.workspace_id == entry.account.workspace_id
            })
        });
    }

    fn due(&self, account: &ManagedAccount, selected: &str, now: i64) -> bool {
        self.entries.get(&account.alias).is_none_or(|entry| {
            entry.refresh_requested || entry.attempted_at > now || entry.refresh_at(selected) <= now
        })
    }

    fn record(
        &mut self,
        account: ManagedAccount,
        usage: Option<ManagedAccountUsage>,
        now: i64,
    ) -> bool {
        let usage = usage.filter(|usage| usage.checked_at <= now);
        let increased_early = self
            .entries
            .get(&account.alias)
            .and_then(|entry| entry.usage.as_ref())
            .zip(usage.as_ref())
            .is_some_and(|(before, after)| {
                weekly(before).zip(weekly(after)).is_some_and(|(old, new)| {
                    after.checked_at > before.checked_at
                        && after.checked_at <= now
                        && future_reset(old, now + 60).is_some()
                        && new.remaining_percent >= old.remaining_percent + 1.0
                        && before
                            .available_resets
                            .zip(after.available_resets)
                            .is_none_or(|(old, new)| new >= old)
                })
            });
        self.entries.insert(
            account.alias.clone(),
            Entry {
                account,
                usage,
                attempted_at: now,
                refresh_requested: false,
            },
        );
        increased_early
            && self
                .last_reset_sweep
                .is_none_or(|last| now.saturating_sub(last) >= 600)
    }
}

async fn request(
    handle: &AppServerRequestHandle,
    params: ManagedAccountParams,
) -> Result<ManagedAccountResponse, TypedRequestError> {
    tokio::time::timeout(
        Duration::from_secs(/*secs*/ 30),
        handle.request_typed(ClientRequest::ManagedAccount {
            request_id: RequestId::String(uuid::Uuid::new_v4().to_string()),
            params,
        }),
    )
    .await
    .map_err(|_| TypedRequestError::Transport {
        method: "account/manage".to_owned(),
        source: std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Account status request timed out",
        ),
    })?
}

pub(crate) async fn load(
    handle: &AppServerRequestHandle,
    cache: &Mutex<AccountStatusCache>,
    changed: &AtomicBool,
    invalidations: &std::sync::Mutex<AccountStatusInvalidations>,
    thread_id: ThreadId,
    include_pool_usage: bool,
) -> Result<AccountStatusSnapshot, String> {
    let mut params = ManagedAccountParams {
        action: ManagedAccountAction::Resolve,
        thread_id: Some(thread_id.to_string()),
        account_selection: None,
        alias: None,
        device_auth: None,
        model: None,
        cursor: None,
        limit: Some(100),
    };
    // Resolution and List read the server's local metadata, never upstream quota endpoints.
    let response = request(handle, params.clone())
        .await
        .map_err(|error| error.to_string())?;
    let mut snapshot = AccountStatusSnapshot {
        account: response.selected_account,
        pool: response.selected_pool,
        ..Default::default()
    };
    let Some(selected) = &snapshot.account else {
        let mut cache = cache.lock().await;
        cache.generation = cache.generation.wrapping_add(/*rhs*/ 1);
        cache.entries.clear();
        return Ok(snapshot);
    };
    let mut aliases = vec![selected.alias.clone()];
    if let Some(pool) = snapshot.pool.as_ref().filter(|_| include_pool_usage) {
        aliases.extend(
            pool.accounts
                .iter()
                .filter(|alias| *alias != &selected.alias)
                .cloned(),
        );
    }
    // Pools have at most 128 members; a live selection may have just been removed from one.
    aliases.truncate(/*len*/ 129);
    let wanted: HashSet<_> = aliases.iter().collect();
    let mut metadata: HashMap<_, _> = response
        .data
        .into_iter()
        .filter(|account| wanted.contains(&account.alias))
        .map(|account| (account.alias.clone(), account))
        .collect();
    params.action = ManagedAccountAction::List;
    params.thread_id = None;
    params.cursor = response.next_cursor;
    while metadata.len() < wanted.len() && params.cursor.is_some() {
        let response = request(handle, params.clone())
            .await
            .map_err(|error| error.to_string())?;
        metadata.extend(
            response
                .data
                .into_iter()
                .filter(|account| wanted.contains(&account.alias))
                .map(|account| (account.alias.clone(), account)),
        );
        params.cursor = response.next_cursor;
    }
    metadata
        .entry(selected.alias.clone())
        .or_insert_with(|| selected.clone());
    let accounts: Vec<_> = aliases
        .iter()
        .filter_map(|alias| metadata.remove(alias))
        .collect();
    let (due, legacy_quota, generation) = {
        let mut cache = cache.lock().await;
        cache.generation = cache.generation.wrapping_add(/*rhs*/ 1);
        cache.retain_members(&accounts);
        cache.invalidate(std::mem::take(
            &mut *invalidations
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        ));
        let now = chrono::Utc::now().timestamp();
        if changed.swap(/*val*/ false, Ordering::Relaxed)
            && let Some(entry) = cache.entries.get_mut(&selected.alias)
            && entry
                .usage
                .as_ref()
                .and_then(weekly)
                .is_some_and(|weekly| weekly.remaining_percent == 0.0)
        {
            // Keep invalidation in the cache if a widget switch cancels this refresh in flight.
            entry.refresh_requested = true;
        }
        let due: Vec<_> = accounts
            .iter()
            .filter(|account| cache.due(account, &selected.alias, now))
            .cloned()
            .collect();
        (due, Arc::clone(&cache.legacy_quota), cache.generation)
    };
    let mut refreshed = HashSet::new();
    let mut reset_sweep = false;
    // At most one extra sweep, so observations from the sweep cannot recursively create work.
    for batch in [Some(due), None] {
        let accounts = match batch {
            Some(accounts) => accounts,
            None if reset_sweep => accounts
                .iter()
                .filter(|account| !refreshed.contains(&account.alias))
                .cloned()
                .collect(),
            None => break,
        };
        for batch in accounts.chunks(/*chunk_size*/ 4) {
            let mut reads = tokio::task::JoinSet::new();
            for account in batch.iter().cloned() {
                let handle = handle.clone();
                let mut params = params.clone();
                params.alias = Some(account.alias.clone());
                params.cursor = None;
                let legacy_quota = Arc::clone(&legacy_quota);
                reads.spawn(async move {
                    params.action = if legacy_quota.load(Ordering::Relaxed) {
                        ManagedAccountAction::Usage
                    } else {
                        ManagedAccountAction::Quota
                    };
                    let mut response = request(&handle, params.clone()).await;
                    if let Err(TypedRequestError::Server { source, .. }) = &response
                        && matches!(source.code, -32600 | -32602)
                        && source.message.contains("unknown variant `quota`")
                    {
                        legacy_quota.store(/*val*/ true, Ordering::Relaxed);
                        params.action = ManagedAccountAction::Usage;
                        response = request(&handle, params).await;
                    }
                    let usage = response
                        .ok()
                        .and_then(|response| response.usage.into_iter().next())
                        .filter(|usage| {
                            usage.account.alias == account.alias
                                && usage.account.user_id == account.user_id
                                && usage.account.workspace_id == account.workspace_id
                        });
                    (account, usage)
                });
            }
            while let Some(result) = reads.join_next().await {
                let Ok((account, usage)) = result else {
                    continue;
                };
                let mut cache = cache.lock().await;
                if cache.generation != generation {
                    return Err("Account status refresh was superseded".to_owned());
                }
                refreshed.insert(account.alias.clone());
                if cache.record(account, usage, chrono::Utc::now().timestamp()) {
                    reset_sweep = true;
                    cache.begin_reset_sweep(&refreshed, chrono::Utc::now().timestamp());
                }
            }
        }
    }
    let cache = cache.lock().await;
    if cache.generation != generation {
        return Err("Account status refresh was superseded".to_owned());
    }
    for account in accounts {
        if let Some(entry) = cache.entries.get(&account.alias) {
            if let Some(valid_until) = entry.valid_until(&selected.alias) {
                snapshot.valid_until.insert(account.alias, valid_until);
            }
            snapshot.usage.extend(entry.usage.iter().cloned());
        }
    }
    Ok(snapshot)
}
