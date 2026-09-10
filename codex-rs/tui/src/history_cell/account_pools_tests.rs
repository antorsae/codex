use super::*;
use codex_protocol::account_pool::AccountPool;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

fn accounts() -> ManagedAccountResponse {
    let account = ManagedAccount {
        alias: "acct1".to_owned(),
        user_id: "user-1".to_owned(),
        workspace_id: "00000000-0000-0000-0000-000000000001".to_owned(),
        email: Some("a@example.com".to_owned()),
        plan: Some("pro".to_owned()),
    };
    ManagedAccountResponse {
        resolved: None,
        models: None,
        data: vec![account.clone()],
        next_cursor: None,
        usage: vec![ManagedAccountUsage {
            account,
            pools: vec!["work".to_owned()],
            model: None,
            model_supported: None,
            ordinary_usage_allowed: Some(true),
            windows: vec![
                AccountQuotaWindow {
                    limit_id: "codex_other".to_owned(),
                    model: Some("another-model".to_owned()),
                    remaining_percent: 12.0,
                    window_minutes: 10_080,
                    resets_at: Some(NOW + 3600),
                },
                AccountQuotaWindow {
                    limit_id: "codex".to_owned(),
                    model: None,
                    remaining_percent: 80.0,
                    window_minutes: 300,
                    resets_at: Some(NOW + 7200),
                },
                AccountQuotaWindow {
                    limit_id: "codex".to_owned(),
                    model: None,
                    remaining_percent: 66.0,
                    window_minutes: 10_080,
                    resets_at: Some(NOW + 76 * 3600),
                },
            ],
            available_resets: Some(0),
            resets: Some(vec![]),
            checked_at: NOW,
            error: None,
        }],
        login: None,
        default_selection: Some(AccountSelection::Account("acct1".to_owned())),
    }
}

fn render(report: &AccountPoolReport, width: u16) -> String {
    report
        .display_lines(width)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n")
}

#[test]
fn weekly_account_table_reflows_and_preserves_detailed_windows() {
    let mut response = accounts();
    response.data[0].plan = Some("plus".to_owned());
    let report = AccountPoolReport::accounts(&response, AccountReportView::Summary, NOW);
    insta::assert_snapshot!("accounts_weekly", render(&report, /*width*/ 110));
    insta::assert_snapshot!("accounts_weekly_narrow", render(&report, /*width*/ 48));
    let report = AccountPoolReport::accounts(&response, AccountReportView::Usage, NOW);
    insta::assert_snapshot!("accounts_usage", render(&report, /*width*/ 110));
}

#[test]
fn failed_reads_hide_partial_quota_and_unknown_values_remain_unknown() {
    let mut response = accounts();
    response.usage[0].error =
        Some("Authentication or usage lookup failed; availability is unknown".to_owned());
    response.usage[0].available_resets = Some(7);
    let mut unknown = response.usage[0].clone();
    unknown.account.alias = "acct2".to_owned();
    unknown.account.user_id = "user-2".to_owned();
    unknown.account.workspace_id = "workspace-2".to_owned();
    unknown.account.email = None;
    unknown.account.plan = None;
    unknown.error = None;
    unknown
        .windows
        .retain(|window| window.limit_id != "codex" || window.window_minutes == 300);
    unknown.available_resets = None;
    response.data.push(unknown.account.clone());
    response.usage.push(unknown);
    let report = AccountPoolReport::accounts(&response, AccountReportView::Usage, NOW);
    insta::assert_snapshot!("accounts_unknown", render(&report, /*width*/ 110));
}

#[test]
fn pools_show_empty_state_and_ordered_members() {
    let mut response = ManagedPoolResponse {
        data: vec![],
        next_cursor: None,
        default_selection: None,
    };
    insta::assert_snapshot!(
        "pools_empty",
        render(&AccountPoolReport::pools(&response), /*width*/ 90)
    );
    response.data = vec![
        AccountPool {
            name: "work".to_owned(),
            accounts: vec!["acct2".to_owned(), "acct1".to_owned()],
            redeem_weekly_resets: true,
        },
        AccountPool {
            name: "personal".to_owned(),
            accounts: vec!["acct1".to_owned()],
            redeem_weekly_resets: false,
        },
    ];
    response.default_selection = Some(AccountSelection::Pool("work".to_owned()));
    insta::assert_snapshot!(
        "pools_table",
        render(&AccountPoolReport::pools(&response), /*width*/ 90)
    );
}

#[test]
fn quota_formatting_handles_deadlines_and_invalid_values() {
    let countdowns: Vec<_> = [
        None,
        Some(i64::MAX),
        Some(NOW - 1),
        Some(NOW),
        Some(NOW + 30),
        Some(NOW + 120),
        Some(NOW + 3600),
        Some(NOW + 76 * 3600),
    ]
    .into_iter()
    .map(|deadline| reset_in(deadline, NOW))
    .collect();
    assert_eq!(
        countdowns,
        [
            "Unknown", "Unknown", "Due", "Due", "<1m", "2m", "1h", "3d 4h"
        ]
    );
    assert_eq!(
        [f64::NAN, f64::INFINITY, -1.0, 101.0, 0.0, 66.0, 100.0].map(percent),
        [
            "Unknown", "Unknown", "Unknown", "Unknown", "0%", "66%", "100%"
        ]
    );
}

#[test]
fn labels_cannot_add_markdown_rows_or_styles() {
    let mut response = accounts();
    response.data[0].email = Some("a|*literal*\n@example.com".to_owned());
    response.usage[0].account = response.data[0].clone();
    let report = AccountPoolReport::accounts(&response, AccountReportView::Summary, NOW);
    let text = render(&report, /*width*/ 140);
    assert!(text.contains("a|*literal* @example.com"));
    assert_eq!(
        report
            .markdown
            .lines()
            .filter(|line| line.starts_with("| acct1 |"))
            .count(),
        1
    );
}
