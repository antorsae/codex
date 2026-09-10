//! Local account reports reuse the transcript's adaptive Markdown tables.
//! Values and countdowns are captured together; resizing never implies a fresh quota read.

use super::HistoryCell;
use super::raw_lines_from_source;
use crate::markdown::append_markdown;
use codex_app_server_protocol::LoginAccountResponse;
use codex_app_server_protocol::ManagedAccountResponse;
use codex_app_server_protocol::ManagedPoolResponse;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use ratatui::text::Line;

#[derive(Clone, Copy)]
pub(crate) enum AccountReportView {
    Summary,
    Usage,
}

#[derive(Debug)]
pub(crate) struct AccountPoolReport {
    pub(crate) markdown: String,
}

impl AccountPoolReport {
    pub(crate) fn accounts(
        response: &ManagedAccountResponse,
        view: AccountReportView,
        now: i64,
    ) -> Self {
        let mut lines = vec![format!("**Accounts ({})**", response.data.len())];
        lines.push(default_selection(response.default_selection.as_ref()));
        if let Some(login) = &response.login {
            lines.push(match login {
                LoginAccountResponse::Chatgpt { auth_url, .. } => {
                    format!("Complete sign-in in your browser: {}", literal(auth_url))
                }
                LoginAccountResponse::ChatgptDeviceCode {
                    verification_url,
                    user_code,
                    ..
                } => format!(
                    "Open {} and enter code **{}**.",
                    literal(verification_url),
                    literal(user_code)
                ),
                LoginAccountResponse::ApiKey {}
                | LoginAccountResponse::ChatgptAuthTokens {}
                | LoginAccountResponse::AmazonBedrock {} => "Sign-in completed.".to_owned(),
            });
            lines.push("Run /accounts after sign-in to refresh.".to_owned());
        } else if response.data.is_empty() {
            lines.push("No managed accounts. Use /accounts add ALIAS or /accounts import ALIAS to add one.".to_owned());
        }
        if !response.data.is_empty() {
            let mut table = vec![
                "| Alias | Email | Plan | Weekly left | Reset in | Available resets |".to_owned(),
                "| --- | --- | --- | --- | --- | --- |".to_owned(),
            ];
            for account in &response.data {
                let usage = account_usage(response, account);
                let account = usage.map_or(account, |usage| &usage.account);
                let known = usage.filter(|usage| usage.error.is_none());
                // Only the account-wide weekly window belongs in this column. A model-specific
                // or monthly allowance must not be presented as the account's weekly quota.
                let weekly = known.and_then(|usage| {
                    usage
                        .windows
                        .iter()
                        .filter(|window| {
                            window.limit_id == "codex" && window.window_minutes == 7 * 24 * 60
                        })
                        .min_by(|a, b| a.remaining_percent.total_cmp(&b.remaining_percent))
                });
                let remaining = weekly.map_or_else(
                    || "Unknown".to_owned(),
                    |window| percent(window.remaining_percent),
                );
                let reset = reset_in(weekly.and_then(|window| window.resets_at), now);
                let resets = known
                    .and_then(|usage| usage.available_resets)
                    .filter(|count| *count >= 0)
                    .map_or_else(|| "Unknown".to_owned(), |count| count.to_string());
                table.push(format!(
                    "| {} | {} | {} | {remaining} | {reset} | {resets} |",
                    literal(&account.alias),
                    literal(account.email.as_deref().unwrap_or("Unknown")),
                    literal(account.plan.as_deref().unwrap_or("Unknown"))
                ));
            }
            lines.push(table.join("\n"));
            lines.push("Refresh with /accounts for current usage and banked resets.".to_owned());
        }
        for account in &response.data {
            let alias = literal(&account.alias);
            lines.push(format!(
                "**{alias}** · Workspace ID: {}",
                literal(&account.workspace_id)
            ));
            let Some(usage) = account_usage(response, account) else {
                lines.push("Usage not loaded. Run /accounts to refresh.".to_owned());
                continue;
            };
            let pools = if usage.pools.is_empty() {
                "None".to_owned()
            } else {
                literal(&usage.pools.join(", "))
            };
            lines.push(format!(
                "Pools: {pools} · Checked: {}",
                timestamp(usage.checked_at)
            ));
            if let Some(error) = &usage.error {
                lines.push(format!("Usage unavailable: {}", literal(error)));
            } else if matches!(view, AccountReportView::Usage) {
                let mut table = vec![
                    "| Limit | Window | Left | Reset in | Resets at |".to_owned(),
                    "| --- | --- | --- | --- | --- |".to_owned(),
                ];
                for window in &usage.windows {
                    table.push(format!(
                        "| {} | {} min | {} | {} | {} |",
                        literal(&window.limit_id),
                        window.window_minutes,
                        percent(window.remaining_percent),
                        reset_in(window.resets_at, now),
                        window
                            .resets_at
                            .map_or_else(|| "Unknown".to_owned(), timestamp)
                    ));
                }
                if !usage.windows.is_empty() {
                    lines.push(table.join("\n"));
                }
            }
        }
        Self {
            markdown: lines.join("\n\n"),
        }
    }

    pub(crate) fn pools(response: &ManagedPoolResponse) -> Self {
        let mut lines = vec![
            format!("**Pools ({})**", response.data.len()),
            default_selection(response.default_selection.as_ref()),
        ];
        if response.data.is_empty() {
            lines.push(
                "No pools configured. Create one with /pools create NAME ALIAS...".to_owned(),
            );
        } else {
            let mut table = vec![
                "| Name | Accounts (in order) | Automatic weekly resets |".to_owned(),
                "| --- | --- | --- |".to_owned(),
            ];
            for pool in &response.data {
                let resets = if pool.redeem_weekly_resets {
                    "Enabled"
                } else {
                    "Disabled"
                };
                table.push(format!(
                    "| {} | {} | {resets} |",
                    literal(&pool.name),
                    literal(&pool.accounts.join(", "))
                ));
            }
            lines.push(table.join("\n"));
            lines.push("Use /accounts for quota by account and /pools select NAME to set the default for new sessions.".to_owned());
        }
        Self {
            markdown: lines.join("\n\n"),
        }
    }
}

fn account_usage<'a>(
    response: &'a ManagedAccountResponse,
    account: &ManagedAccount,
) -> Option<&'a ManagedAccountUsage> {
    // Plan metadata can change during a usage refresh; join only on the account's identity.
    response.usage.iter().find(|usage| {
        usage.account.alias == account.alias
            && usage.account.user_id == account.user_id
            && usage.account.workspace_id == account.workspace_id
    })
}

impl HistoryCell for AccountPoolReport {
    fn display_lines(&self, width: u16) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        append_markdown(
            &self.markdown,
            Some(usize::from(width)),
            /*cwd*/ None,
            &mut lines,
        );
        lines
    }

    fn raw_lines(&self) -> Vec<Line<'static>> {
        raw_lines_from_source(&self.markdown)
    }
}

fn default_selection(selection: Option<&AccountSelection>) -> String {
    let selection = match selection {
        Some(AccountSelection::Account(alias)) => format!("account {}", literal(alias)),
        Some(AccountSelection::Pool(name)) => format!("pool {}", literal(name)),
        None => "current login".to_owned(),
    };
    format!("Default for new sessions: {selection}")
}

fn percent(remaining: f64) -> String {
    if remaining.is_finite() && (0.0..=100.0).contains(&remaining) {
        format!("{remaining:.0}%")
    } else {
        "Unknown".to_owned()
    }
}

fn reset_in(resets_at: Option<i64>, now: i64) -> String {
    let Some(resets_at) =
        resets_at.filter(|value| chrono::DateTime::from_timestamp(*value, /*nsecs*/ 0).is_some())
    else {
        return "Unknown".to_owned();
    };
    let seconds = resets_at.saturating_sub(now);
    if seconds <= 0 {
        return "Due".to_owned();
    }
    let hours = seconds / 3600;
    if hours >= 24 {
        format!("{}d {}h", hours / 24, hours % 24)
    } else if hours > 0 {
        format!("{hours}h")
    } else if seconds >= 60 {
        format!("{}m", seconds / 60)
    } else {
        "<1m".to_owned()
    }
}

fn timestamp(value: i64) -> String {
    chrono::DateTime::from_timestamp(value, /*nsecs*/ 0).map_or_else(
        || "Unknown".to_owned(),
        |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

/// Keep server-provided labels literal, including pipes, line breaks, and Markdown delimiters.
fn literal(value: &str) -> String {
    let mut escaped = String::new();
    for ch in value.chars() {
        if ch.is_control() || ch.is_whitespace() {
            escaped.push(' ');
        } else {
            if ch.is_ascii_punctuation() {
                escaped.push('\\');
            }
            escaped.push(ch);
        }
    }
    escaped
}

#[cfg(test)]
#[path = "account_pools_tests.rs"]
mod tests;
