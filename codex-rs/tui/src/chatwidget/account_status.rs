//! Account and pool quota for the configurable footer. Reads are independent of pool recovery and
//! never redeem credits. Request IDs discard responses from an earlier account or session.

use super::*;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use uuid::Uuid;

const REFRESH_INTERVAL: Duration = Duration::from_secs(/*secs*/ 60);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

#[derive(Default)]
pub(super) struct AccountStatus {
    account: Option<ManagedAccount>,
    pool: Option<AccountPool>,
    pub(super) weekly: Option<WeeklyQuota>,
    pool_weekly: Option<WeeklyQuota>,
    pending_request: Option<Uuid>,
    next_check: Option<Instant>,
}

pub(super) struct WeeklyQuota {
    remaining_percent: f64,
    resets_at: Option<i64>,
    checked_at: i64,
}

impl AccountStatus {
    pub(super) fn select(&mut self, account: ManagedAccount) {
        if !self
            .account
            .as_ref()
            .is_some_and(|current| same_identity(current, &account))
        {
            self.weekly = None;
            self.pool_weekly = None;
            self.pending_request = None;
            self.next_check = None;
        }
        self.account = Some(account);
    }

    pub(super) fn record_snapshot(&mut self, snapshot: &RateLimitSnapshot, now: i64) {
        self.weekly = snapshot
            .primary
            .iter()
            .chain(snapshot.secondary.iter())
            .filter(|window| window.window_duration_mins == Some(WEEKLY_MINUTES))
            .max_by_key(|window| window.used_percent)
            .map(|window| WeeklyQuota {
                remaining_percent: 100.0 - f64::from(window.used_percent),
                resets_at: window.resets_at,
                checked_at: now,
            });
    }

    pub(super) fn weekly_display(&self, now: i64) -> Option<String> {
        let quota = self.weekly.as_ref()?;
        if !(0.0..=100.0).contains(&quota.remaining_percent) {
            return None;
        }
        quota.display(now)
    }

    pub(super) fn account_display(&self, now: i64) -> Option<String> {
        let alias = history_cell::sanitize_user_text(self.account.as_ref()?.alias.as_str().into());
        let quota = self
            .weekly_display(now)
            .unwrap_or_else(|| "unknown".to_owned());
        Some(format!("{alias} {quota}"))
    }

    pub(super) fn pool_display(&self, now: i64) -> Option<String> {
        let name = history_cell::sanitize_user_text(self.pool.as_ref()?.name.as_str().into());
        let quota = self
            .pool_weekly
            .as_ref()
            .and_then(|quota| quota.display(now))
            .unwrap_or_else(|| "unknown".to_owned());
        Some(format!("{name} {quota}"))
    }

    fn record_usage(&mut self, usages: &[ManagedAccountUsage], now: i64) {
        self.weekly = usages
            .iter()
            .find(|usage| {
                self.account
                    .as_ref()
                    .is_some_and(|account| same_identity(account, &usage.account))
            })
            .and_then(WeeklyQuota::from_usage);
        self.pool_weekly = self
            .pool
            .as_ref()
            .filter(|_| self.weekly.is_some())
            .and_then(|pool| {
                let mut identities = std::collections::HashSet::new();
                let quotas = pool
                    .accounts
                    .iter()
                    .map(|alias| {
                        let usage = usages.iter().find(|usage| &usage.account.alias == alias)?;
                        if !identities.insert((&usage.account.user_id, &usage.account.workspace_id))
                        {
                            return None;
                        }
                        WeeklyQuota::from_usage(usage).filter(|quota| quota.is_fresh(now))
                    })
                    .collect::<Option<Vec<_>>>()?;
                Some(WeeklyQuota {
                    remaining_percent: quotas.iter().map(|quota| quota.remaining_percent).sum(),
                    // An unknown reset cannot be skipped when finding the earliest pool reset.
                    resets_at: quotas
                        .iter()
                        .map(|quota| quota.resets_at)
                        .collect::<Option<Vec<_>>>()
                        .and_then(|resets| resets.into_iter().min()),
                    checked_at: quotas.iter().map(|quota| quota.checked_at).min()?,
                })
            });
    }
}

impl WeeklyQuota {
    fn from_usage(usage: &ManagedAccountUsage) -> Option<Self> {
        if usage.error.is_some() {
            return None;
        }
        let window = usage
            .windows
            .iter()
            .filter(|window| window.limit_id == "codex" && window.window_minutes == WEEKLY_MINUTES)
            .min_by(|a, b| a.remaining_percent.total_cmp(&b.remaining_percent))?;
        if !(0.0..=100.0).contains(&window.remaining_percent) {
            return None;
        }
        Some(Self {
            remaining_percent: window.remaining_percent,
            resets_at: window.resets_at.filter(|timestamp| {
                chrono::DateTime::from_timestamp(*timestamp, /*nsecs*/ 0).is_some()
            }),
            checked_at: usage.checked_at,
        })
    }

    fn display(&self, now: i64) -> Option<String> {
        let remaining = self.remaining_percent;
        if !remaining.is_finite() || remaining < 0.0 || !self.is_fresh(now) {
            return None;
        }
        let Some(reset) = self
            .resets_at
            .and_then(|timestamp| chrono::DateTime::from_timestamp(timestamp, /*nsecs*/ 0))
        else {
            return Some(format!("{remaining:.0}%"));
        };
        let seconds = reset.timestamp().saturating_sub(now);
        let countdown = if seconds <= 0 {
            "due".to_owned()
        } else if seconds >= 86_400 {
            format!("{}d{}h", seconds / 86_400, seconds % 86_400 / 3600)
        } else if seconds >= 3600 {
            format!("{}h{}m", seconds / 3600, seconds % 3600 / 60)
        } else if seconds >= 60 {
            format!("{}m", seconds / 60)
        } else {
            "<1m".to_owned()
        };
        Some(format!("{remaining:.0}% {countdown}"))
    }

    fn is_fresh(&self, now: i64) -> bool {
        (0..15 * 60).contains(&now.saturating_sub(self.checked_at))
    }
}

impl ChatWidget {
    pub(crate) fn initialize_managed_account_status(
        &mut self,
        account: Option<ManagedAccount>,
        pool: Option<AccountPool>,
    ) {
        self.account_status = AccountStatus {
            account,
            pool,
            ..Default::default()
        };
    }

    pub(super) fn refresh_account_status_if_due(&mut self) {
        let items = self.configured_status_line_items();
        if !items.iter().any(|item| {
            matches!(
                item.as_str(),
                "weekly-limit-with-reset" | "account-weekly" | "pool-weekly"
            )
        }) {
            self.account_status.pending_request = None;
            return;
        }
        // Redraw the countdown while idle as well as during model output.
        self.refresh_status_line();
        self.frame_requester.schedule_frame_in(REFRESH_INTERVAL);
        if !self.managed_accounts_active {
            return;
        }
        let status = &mut self.account_status;
        let Some(account) = &status.account else {
            return;
        };
        let now = Instant::now();
        if status.pending_request.is_some() || status.next_check.is_some_and(|next| now < next) {
            return;
        }
        let request_id = Uuid::new_v4();
        status.pending_request = Some(request_id);
        status.next_check = Some(now + REFRESH_INTERVAL);
        self.app_event_tx.send(AppEvent::RefreshAccountStatus {
            request_id,
            account: account.clone(),
            pool: if items.iter().any(|item| item == "pool-weekly") {
                status.pool.clone()
            } else {
                None
            },
        });
    }

    pub(crate) fn finish_account_status(
        &mut self,
        request_id: Uuid,
        result: Result<Vec<ManagedAccountUsage>, String>,
    ) {
        if self.account_status.pending_request != Some(request_id) {
            return;
        }
        self.account_status.pending_request = None;
        self.account_status
            .record_usage(&result.unwrap_or_default(), Local::now().timestamp());
        self.refresh_status_line();
        self.request_redraw();
    }
}

fn same_identity(a: &ManagedAccount, b: &ManagedAccount) -> bool {
    a.alias == b.alias && a.user_id == b.user_id && a.workspace_id == b.workspace_id
}

#[cfg(test)]
#[path = "account_status_tests.rs"]
mod tests;
