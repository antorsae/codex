//! Shared, per-account quota observations. Every process that reads an account's usage stores the
//! result next to that account's credentials, so concurrent sessions, footers and preflights do not
//! repeat the same backend requests. Recovery only trusts observations produced by a usage read;
//! display surfaces also accept the quota windows that inference responses already carry.

use crate::AccountStore;
use crate::storage::read_json;
use crate::storage::write_json_cache;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use codex_protocol::protocol::RateLimitSnapshot;
use serde::Deserialize;
use serde::Serialize;
use std::time::Duration;

const USAGE_FILE: &str = "usage.json";
const MODEL_SLUGS_FILE: &str = "model-slugs.json";

/// Recovery and preflight accept a usage read this old instead of repeating it. Waiting sessions
/// re-read every ten minutes, so this bound lets several of them share one read; the account
/// they finally activate is always confirmed with a fresh read first.
pub const RECOVERY_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 300);
/// A model request rejected for quota marks the account unusable for other sessions this long,
/// since usage reads are eventually consistent with the rejection.
pub const REJECTION_HOLD: Duration = Duration::from_secs(/*secs*/ 60);
/// Response recording is best effort and never waits long for a reader holding the account lock.
const RECORD_LOCK_WAIT: Duration = Duration::from_millis(/*millis*/ 250);
/// Footers and other display surfaces tolerate older observations.
pub const DISPLAY_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 600);
/// A weekly-exhausted account regains quota only at its reset or through a redemption this
/// coordinator performs, so it is re-read at most hourly before then.
pub const EXHAUSTED_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 3600);
/// Model catalogs change rarely; a requested model missing from a recent catalog is re-checked
/// once the catalog is older than [`MODEL_SLUGS_MISS_MAX_AGE`].
pub const MODEL_SLUGS_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 24 * 3600);
pub const MODEL_SLUGS_MISS_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 600);
/// Failed reads are not repeated on every tick; this matches the shortest footer retry cadence
/// so several TUIs retry a failing account once per interval between them.
pub const ERROR_MAX_AGE: Duration = Duration::from_secs(/*secs*/ 300);

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredUsage {
    /// The last successful usage read, with model-specific fields stripped.
    #[serde(default)]
    pub(crate) usage: Option<ManagedAccountUsage>,
    /// Whether `usage` includes banked reset details.
    #[serde(default)]
    pub(crate) usage_has_credit_details: bool,
    /// Quota windows carried by inference responses, merged by limit id.
    #[serde(default)]
    pub(crate) response: Option<ManagedAccountUsage>,
    /// When the last usage read failed, so unavailable accounts are not retried on every tick.
    #[serde(default)]
    pub(crate) error_at: Option<i64>,
    /// Until when a quota rejection overrides `ordinary_usage_allowed` for every session.
    #[serde(default)]
    pub(crate) rejected_until: Option<i64>,
}

impl StoredUsage {
    pub(crate) fn rejected(&self, now: i64) -> bool {
        self.rejected_until.is_some_and(|until| until > now)
    }
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(crate) struct StoredModelSlugs {
    pub(crate) fetched_at: i64,
    pub(crate) client_version: String,
    pub(crate) slugs: Vec<String>,
}

/// A reader waits this long for another reader of the same account before reading on its own,
/// slightly longer than a read's network deadline so it usually finds the result stored.
pub(crate) const READ_LOCK_WAIT: Duration = Duration::from_secs(/*secs*/ 25);
/// Rejections are recorded promptly; a reader holding the lock stores a newer read anyway.
const REJECTION_LOCK_WAIT: Duration = Duration::from_secs(/*secs*/ 2);

/// Serialize readers and writers of one account's observation across sessions and processes.
/// A reader holds it across its network read, so concurrent readers find the result stored.
/// Waiting is bounded: a stalled holder must not pin the account for every session.
pub(crate) async fn lock_usage(
    store: &AccountStore,
    account: &ManagedAccount,
    wait: Duration,
) -> Option<std::fs::File> {
    let path = store.credential_home(account).join("usage.lock");
    match tokio::time::timeout(wait, crate::storage::lock(&path)).await {
        Ok(Ok(file)) => Some(file),
        Ok(Err(error)) => {
            tracing::debug!(alias = %account.alias, %error, "Could not lock account usage observation");
            None
        }
        Err(_) => {
            tracing::debug!(alias = %account.alias, "Account usage lock is busy; reading without it");
            None
        }
    }
}

pub(crate) fn load_usage(store: &AccountStore, account: &ManagedAccount) -> StoredUsage {
    // A damaged cache must never prevent a read; it is rewritten by the next observation.
    read_json(&store.credential_home(account).join(USAGE_FILE))
        .ok()
        .flatten()
        .unwrap_or_default()
}

pub(crate) fn store_usage(store: &AccountStore, account: &ManagedAccount, stored: &StoredUsage) {
    if let Err(error) = write_json_cache(&store.credential_home(account).join(USAGE_FILE), stored) {
        tracing::debug!(alias = %account.alias, %error, "Could not store account usage observation");
    }
}

pub(crate) fn load_model_slugs(
    store: &AccountStore,
    account: &ManagedAccount,
) -> Option<StoredModelSlugs> {
    read_json(&store.credential_home(account).join(MODEL_SLUGS_FILE))
        .ok()
        .flatten()
}

pub(crate) fn store_model_slugs(
    store: &AccountStore,
    account: &ManagedAccount,
    slugs: &StoredModelSlugs,
) {
    if let Err(error) = write_json_cache(
        &store.credential_home(account).join(MODEL_SLUGS_FILE),
        slugs,
    ) {
        tracing::debug!(alias = %account.alias, %error, "Could not store account model catalog");
    }
}

/// A cached catalog answers whether `model` is supported unless it is stale, or too old to trust
/// a miss for a model that may have been released since.
pub(crate) fn cached_model_support(
    slugs: Option<&StoredModelSlugs>,
    model: &str,
    client_version: &str,
    now: i64,
) -> Option<bool> {
    let slugs = slugs?;
    if slugs.client_version != client_version {
        return None;
    }
    let age = now.saturating_sub(slugs.fetched_at);
    if age < 0 || age > MODEL_SLUGS_MAX_AGE.as_secs() as i64 {
        return None;
    }
    let supported = slugs.slugs.iter().any(|slug| slug == model);
    if !supported && age > MODEL_SLUGS_MISS_MAX_AGE.as_secs() as i64 {
        return None;
    }
    Some(supported)
}

fn weekly_windows(usage: &ManagedAccountUsage) -> impl Iterator<Item = &AccountQuotaWindow> {
    usage
        .windows
        .iter()
        .filter(|window| window.limit_id == "codex" && window.window_minutes > 24 * 60)
}

/// The reset that ends an account's weekly exhaustion, when the observation shows one.
pub(crate) fn weekly_exhausted_until(usage: &ManagedAccountUsage) -> Option<i64> {
    if usage.error.is_some() {
        return None;
    }
    weekly_windows(usage)
        .filter(|window| window.remaining_percent <= 0.0)
        .filter_map(|window| window.resets_at)
        .filter(|reset| *reset > usage.checked_at)
        .max()
}

/// An exhausted window that reset between the observation and now no longer blocks the account,
/// so the observation no longer describes its usability. Resets of windows with quota left only
/// add capacity and do not invalidate it.
pub(crate) fn reset_since(usage: &ManagedAccountUsage, now: i64) -> bool {
    usage.windows.iter().any(|window| {
        window.remaining_percent <= 0.0
            && window
                .resets_at
                .is_some_and(|reset| reset > usage.checked_at && reset <= now)
    })
}

/// Whether an observation may still stand in for a new read.
pub(crate) fn fresh_enough(usage: &ManagedAccountUsage, now: i64, max_age: Duration) -> bool {
    let age = now.saturating_sub(usage.checked_at);
    if age < 0 || usage.error.is_some() {
        return false;
    }
    if age <= max_age.as_secs() as i64 && !reset_since(usage, now) {
        return true;
    }
    // Weekly exhaustion outlasts any short-window reset until its own reset.
    age <= EXHAUSTED_MAX_AGE.as_secs() as i64
        && weekly_exhausted_until(usage).is_some_and(|reset| reset > now)
}

/// Slack for a read that finished just before its consumer checked the clock.
const FRESHNESS_GRACE_SECS: i64 = 90;

/// Recovery evidence: a shared usage read within [`RECOVERY_MAX_AGE`], or an hourly re-read of a
/// weekly-exhausted account. Activation still requires a fresh confirming read.
pub(crate) fn fresh_for_recovery(usage: &ManagedAccountUsage, now: i64) -> bool {
    let age = now.saturating_sub(usage.checked_at);
    if age < 0 {
        return false;
    }
    if age <= RECOVERY_MAX_AGE.as_secs() as i64 + FRESHNESS_GRACE_SECS && !reset_since(usage, now) {
        return true;
    }
    age <= EXHAUSTED_MAX_AGE.as_secs() as i64 + FRESHNESS_GRACE_SECS
        && weekly_exhausted_until(usage).is_some_and(|reset| reset > now)
}

/// Quota windows from an inference response, keyed like `/wham/usage` windows.
pub(crate) fn snapshot_windows(snapshot: &RateLimitSnapshot) -> Vec<AccountQuotaWindow> {
    let limit_id = snapshot
        .limit_id
        .clone()
        .unwrap_or_else(|| "codex".to_owned());
    [snapshot.primary.as_ref(), snapshot.secondary.as_ref()]
        .into_iter()
        .flatten()
        .filter_map(|window| {
            let minutes = window.window_minutes.filter(|minutes| *minutes > 0)?;
            if !window.used_percent.is_finite() || !(0.0..=100.0).contains(&window.used_percent) {
                return None;
            }
            Some(AccountQuotaWindow {
                limit_id: limit_id.clone(),
                model: snapshot.normal_model_slug.clone(),
                remaining_percent: 100.0 - window.used_percent,
                window_minutes: minutes,
                resets_at: window.resets_at,
            })
        })
        .collect()
}

/// Mark the account rejected for quota: the next recovery read is fresh, and shared reads treat
/// the account as unusable until the hold expires even if usage still lags the rejection.
pub(crate) async fn record_rejection(store: &AccountStore, account: &ManagedAccount, now: i64) {
    let _guard = lock_usage(store, account, REJECTION_LOCK_WAIT).await;
    let mut stored = load_usage(store, account);
    stored.usage = None;
    stored.usage_has_credit_details = false;
    stored.rejected_until = Some(now + REJECTION_HOLD.as_secs() as i64);
    store_usage(store, account, &stored);
}

/// Merge the windows a response reported for one limit into the account's response observation.
pub(crate) async fn record_response(
    store: &AccountStore,
    account: &ManagedAccount,
    snapshot: &RateLimitSnapshot,
    now: i64,
) {
    let windows = snapshot_windows(snapshot);
    if windows.is_empty() {
        return;
    }
    let path = store.credential_home(account).join("usage.lock");
    let Ok(Ok(_guard)) = tokio::time::timeout(RECORD_LOCK_WAIT, crate::storage::lock(&path)).await
    else {
        // A reader is refreshing this account; its result supersedes this snapshot anyway.
        return;
    };
    let mut stored = load_usage(store, account);
    let mut response = stored
        .response
        .take()
        .unwrap_or_else(|| ManagedAccountUsage {
            account: account.clone(),
            pools: Vec::new(),
            model: None,
            model_supported: None,
            ordinary_usage_allowed: None,
            windows: Vec::new(),
            available_resets: None,
            resets: None,
            checked_at: now,
            error: None,
        });
    // Snapshots can be sparse: replace only the windows this one reports, by limit and duration.
    response.windows.retain(|window| {
        !windows.iter().any(|updated| {
            updated.limit_id == window.limit_id && updated.window_minutes == window.window_minutes
        })
    });
    response.windows.extend(windows);
    response.checked_at = now;
    response.account = account.clone();
    stored.response = Some(response);
    store_usage(store, account, &stored);
}

/// The best display observation: the last usage read, overlaid with newer response windows.
/// Response-only data counts once it covers the ordinary `codex` limit.
pub(crate) fn display_usage(stored: &StoredUsage) -> Option<ManagedAccountUsage> {
    match (&stored.usage, &stored.response) {
        (Some(usage), Some(response)) if response.checked_at > usage.checked_at => {
            let mut merged = usage.clone();
            merged.windows.retain(|window| {
                !response
                    .windows
                    .iter()
                    .any(|updated| updated.limit_id == window.limit_id)
            });
            merged.windows.extend(response.windows.iter().cloned());
            merged.checked_at = response.checked_at;
            Some(merged)
        }
        (Some(usage), _) => Some(usage.clone()),
        (None, Some(response)) => response
            .windows
            .iter()
            .any(|window| window.limit_id == "codex")
            .then(|| response.clone()),
        (None, None) => None,
    }
}

#[cfg(test)]
#[path = "observations_tests.rs"]
mod tests;
