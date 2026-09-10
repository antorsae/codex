//! Active-account quota for the configurable footer. Reads are independent of pool recovery and
//! never redeem credits. Request IDs discard responses from an earlier account or session.

use super::*;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use uuid::Uuid;

const REFRESH_INTERVAL: Duration = Duration::from_secs(/*secs*/ 60);
const WEEKLY_MINUTES: i64 = 7 * 24 * 60;

#[derive(Default)]
pub(super) struct AccountStatus {
    account: Option<ManagedAccount>,
    pub(super) weekly: Option<WeeklyQuota>,
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
            *self = Self::default();
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
        let remaining = quota.remaining_percent;
        if !(0.0..=100.0).contains(&remaining) || now.saturating_sub(quota.checked_at) >= 15 * 60 {
            return None;
        }
        let Some(reset) = quota
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
}

impl ChatWidget {
    pub(crate) fn initialize_managed_account_status(&mut self, account: Option<ManagedAccount>) {
        self.account_status = AccountStatus {
            account,
            ..Default::default()
        };
    }

    pub(super) fn refresh_account_status_if_due(&mut self) {
        if !self
            .configured_status_line_items()
            .iter()
            .any(|item| item == "weekly-limit-with-reset")
        {
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
        });
    }

    pub(crate) fn finish_account_status(
        &mut self,
        request_id: Uuid,
        result: Result<ManagedAccountUsage, String>,
    ) {
        if self.account_status.pending_request != Some(request_id) {
            return;
        }
        self.account_status.pending_request = None;
        self.account_status.weekly = result
            .ok()
            .filter(|usage| {
                usage.error.is_none()
                    && self
                        .account_status
                        .account
                        .as_ref()
                        .is_some_and(|account| same_identity(account, &usage.account))
            })
            .and_then(|usage| {
                usage
                    .windows
                    .iter()
                    .filter(|window| {
                        window.limit_id == "codex" && window.window_minutes == WEEKLY_MINUTES
                    })
                    .min_by(|a, b| a.remaining_percent.total_cmp(&b.remaining_percent))
                    .map(|window| WeeklyQuota {
                        remaining_percent: window.remaining_percent,
                        resets_at: window.resets_at,
                        checked_at: usage.checked_at,
                    })
            });
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
