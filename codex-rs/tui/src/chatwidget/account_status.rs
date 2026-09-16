//! Account and pool quota for the configurable footer. Reads are independent of pool recovery and
//! never redeem credits. Request IDs discard responses from an earlier account or session.

use super::*;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use uuid::Uuid;

const REFRESH_INTERVAL: Duration = Duration::from_secs(/*secs*/ 60);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

/// Account and pool resolved for the displayed thread, with its requested quota observations.
#[derive(Debug, Default)]
pub(crate) struct AccountStatusSnapshot {
    pub(crate) account: Option<ManagedAccount>,
    pub(crate) pool: Option<AccountPool>,
    pub(crate) usage: Vec<ManagedAccountUsage>,
    /// Display-only cache deadlines; observations retain their original checked_at timestamps.
    pub(crate) valid_until: std::collections::HashMap<String, i64>,
}

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
    valid_until: i64,
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
                valid_until: now.saturating_add(15 * 60),
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

    fn record_usage(
        &mut self,
        usages: &[ManagedAccountUsage],
        valid_until: &std::collections::HashMap<String, i64>,
        now: i64,
    ) {
        let quota = |usage: &ManagedAccountUsage| {
            WeeklyQuota::from_usage(usage).map(|mut quota| {
                if let Some(deadline) = valid_until.get(&usage.account.alias) {
                    quota.valid_until = *deadline;
                }
                quota
            })
        };
        self.weekly = usages
            .iter()
            .find(|usage| {
                self.account
                    .as_ref()
                    .is_some_and(|account| same_identity(account, &usage.account))
            })
            .and_then(quota);
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
                        quota(usage).filter(|quota| quota.is_fresh(now))
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
                    valid_until: quotas.iter().map(|quota| quota.valid_until).min()?,
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
            valid_until: usage.checked_at.saturating_add(15 * 60),
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
        self.checked_at <= now && now < self.valid_until
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
        let Some(thread_id) = self.thread_id() else {
            return;
        };
        let status = &mut self.account_status;
        let now = Instant::now();
        if status.pending_request.is_some() || status.next_check.is_some_and(|next| now < next) {
            return;
        }
        let request_id = Uuid::new_v4();
        status.pending_request = Some(request_id);
        status.next_check = Some(now + REFRESH_INTERVAL);
        self.app_event_tx.send(AppEvent::RefreshAccountStatus {
            request_id,
            thread_id,
            include_pool_usage: items.iter().any(|item| item == "pool-weekly"),
        });
    }

    pub(crate) fn finish_account_status(
        &mut self,
        request_id: Uuid,
        result: Result<AccountStatusSnapshot, String>,
    ) {
        if self.account_status.pending_request != Some(request_id) {
            return;
        }
        self.account_status.pending_request = None;
        match result {
            Ok(snapshot) => {
                let was_managed = self.managed_accounts_active;
                self.managed_accounts_active = snapshot.account.is_some();
                if let Some(account) = &snapshot.account {
                    self.status_account_display = Some(StatusAccountDisplay::ChatGpt {
                        email: account.email.clone(),
                        plan: account.plan.clone(),
                    });
                }
                self.account_status.account = snapshot.account;
                self.account_status.pool = snapshot.pool;
                // Legacy sessions receive their weekly quota through account/rateLimits/read.
                if self.managed_accounts_active || was_managed {
                    self.account_status.record_usage(
                        &snapshot.usage,
                        &snapshot.valid_until,
                        Local::now().timestamp(),
                    );
                }
            }
            Err(_) if self.managed_accounts_active => {
                self.account_status
                    .record_usage(&[], &Default::default(), Local::now().timestamp())
            }
            Err(_) => {}
        }
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
