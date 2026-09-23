//! Credential-free contracts for locally managed ChatGPT accounts and session pools.

use schemars::JsonSchema;
use serde::Deserialize;
use serde::Serialize;
use ts_rs::TS;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", content = "name", rename_all = "camelCase")]
#[ts(
    tag = "type",
    content = "name",
    rename_all = "camelCase",
    export_to = "v2/"
)]
pub enum AccountSelection {
    Account(String),
    Pool(String),
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedAccount {
    pub alias: String,
    pub user_id: String,
    pub workspace_id: String,
    pub email: Option<String>,
    pub plan: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountPool {
    pub name: String,
    /// Ordered aliases. The order breaks quota and reset-credit ties.
    #[schemars(length(min = 1, max = 128))]
    pub accounts: Vec<String>,
    /// Use existing banked credits only when every eligible account is weekly exhausted.
    pub redeem_weekly_resets: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
#[ts(export_to = "v2/")]
pub struct AccountPoolConfig {
    pub accounts: Vec<ManagedAccount>,
    pub pools: Vec<AccountPool>,
    pub default_selection: Option<AccountSelection>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct AccountQuotaWindow {
    pub limit_id: String,
    pub model: Option<String>,
    pub remaining_percent: f64,
    #[ts(type = "number")]
    pub window_minutes: i64,
    #[ts(type = "number | null")]
    pub resets_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct BankedReset {
    pub id: String,
    #[ts(type = "number | null")]
    pub expires_at: Option<i64>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(export_to = "v2/")]
pub struct ManagedAccountUsage {
    pub account: ManagedAccount,
    pub pools: Vec<String>,
    pub model: Option<String>,
    pub model_supported: Option<bool>,
    pub ordinary_usage_allowed: Option<bool>,
    pub windows: Vec<AccountQuotaWindow>,
    #[ts(type = "number | null")]
    pub available_resets: Option<i64>,
    /// Null means details are unknown, including failed reads.
    pub resets: Option<Vec<BankedReset>>,
    #[ts(type = "number")]
    pub checked_at: i64,
    pub error: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(rename_all = "camelCase")]
#[ts(rename_all = "camelCase", export_to = "v2/")]
pub enum PoolWaitReason {
    UnknownAvailability,
    ShortWindow,
    WeeklyQuota,
    RedemptionPending,
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq, JsonSchema, TS)]
#[serde(tag = "type", rename_all = "camelCase")]
#[ts(tag = "type", rename_all = "camelCase", export_to = "v2/")]
pub enum AccountPoolEvent {
    Selected {
        account: ManagedAccount,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Switched {
        account: String,
        previous_account: String,
    },
    Redeemed {
        account: String,
    },
    #[serde(rename_all = "camelCase")]
    #[ts(rename_all = "camelCase")]
    Waiting {
        account: String,
        reason: PoolWaitReason,
        #[ts(type = "number")]
        next_check_at: i64,
    },
}

impl std::fmt::Display for AccountPoolEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selected { account } => write!(f, "Using account {}", account.alias),
            Self::Switched {
                account,
                previous_account,
            } => write!(
                f,
                "Account switched from {previous_account} to {account}; continuing with the same model"
            ),
            Self::Redeemed { account } => write!(
                f,
                "Used a banked reset for {account}; quota recovery verified"
            ),
            Self::Waiting {
                account,
                reason,
                next_check_at,
            } => {
                let reason = match reason {
                    PoolWaitReason::UnknownAvailability => "availability is unknown",
                    PoolWaitReason::ShortWindow => "short-window quota is exhausted",
                    PoolWaitReason::WeeklyQuota => "weekly quota is exhausted",
                    PoolWaitReason::RedemptionPending => "reset recovery is pending",
                };
                write!(
                    f,
                    "Account {account}: waiting because {reason}. Next check: {}. Cancel to stop monitoring.",
                    display_time(*next_check_at)
                )
            }
        }
    }
}

impl ManagedAccountUsage {
    pub fn display_summary(&self) -> String {
        let mut lines = vec![format!(
            "{} | {} | pools: {} | checked {}",
            self.account.alias,
            self.account.plan.as_deref().unwrap_or("plan unknown"),
            self.pools.join(", "),
            display_time(self.checked_at)
        )];
        for window in &self.windows {
            lines.push(format!(
                "  {}: {:.0}% remaining ({} minutes), resets {}",
                window.limit_id,
                window.remaining_percent,
                window.window_minutes,
                window
                    .resets_at
                    .map(display_time)
                    .unwrap_or_else(|| "unknown".to_owned())
            ));
        }
        lines.push(format!(
            "  Banked resets: {} | {}",
            self.available_resets
                .map(|n| n.to_string())
                .unwrap_or_else(|| "unknown".to_owned()),
            self.error.as_deref().unwrap_or("authenticated")
        ));
        if self.windows.is_empty() {
            lines.push("  Quota windows: unknown".to_owned());
        }
        lines.join("\n")
    }
}

fn display_time(timestamp: i64) -> String {
    chrono::DateTime::from_timestamp(timestamp, /*nsecs*/ 0)
        .map(|time| time.to_rfc3339())
        .unwrap_or_else(|| timestamp.to_string())
}
