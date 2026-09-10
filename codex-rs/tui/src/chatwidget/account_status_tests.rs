use super::*;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use codex_app_server_protocol::RateLimitWindow;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountQuotaWindow;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

#[test]
fn pool_weekly_sums_members_until_earliest_reset_including_exhausted_accounts() {
    let mut usages: Vec<_> = [
        ("oa", 14.0, 6 * 86_400 + 19 * 3600),
        ("ob", 0.0, 4 * 86_400 + 7 * 3600),
        ("oc", 100.0, 6 * 86_400 + 23 * 3600),
    ]
    .into_iter()
    .map(|(alias, remaining, reset)| {
        let mut value = usage(alias);
        value.windows[0].remaining_percent = remaining;
        value.windows[0].resets_at = Some(NOW + reset);
        // Neither banked resets nor short/model-specific limits enter the weekly sum.
        value.available_resets = Some(10);
        value.windows.push(AccountQuotaWindow {
            window_minutes: 300,
            remaining_percent: 0.0,
            resets_at: Some(NOW + 60),
            ..value.windows[0].clone()
        });
        value.windows.push(AccountQuotaWindow {
            limit_id: "other-model".to_owned(),
            remaining_percent: 0.0,
            resets_at: Some(NOW + 120),
            ..value.windows[0].clone()
        });
        value
    })
    .collect();
    let mut status = AccountStatus {
        account: Some(usages[0].account.clone()),
        pool: Some(AccountPool {
            name: "nano".to_owned(),
            accounts: vec!["oa".to_owned(), "ob".to_owned(), "oc".to_owned()],
            redeem_weekly_resets: true,
        }),
        ..Default::default()
    };
    usages.push(usage("outside"));
    status.record_usage(&usages, NOW);
    assert_eq!(
        (status.account_display(NOW), status.pool_display(NOW)),
        (
            Some("oa 14% 6d19h".to_owned()),
            Some("nano 114% 4d7h".to_owned())
        )
    );
    assert_eq!(
        status.pool_display(NOW + 15 * 60),
        Some("nano unknown".to_owned())
    );
    let mut invalid = vec![usages.clone(); 7];
    invalid[0][1].error = Some("offline".to_owned());
    invalid[1][1].windows.clear();
    invalid[2][1].windows[0].remaining_percent = f64::NAN;
    invalid[3][1].checked_at = NOW - 15 * 60;
    invalid[4].remove(1);
    invalid[5][1].account.user_id = usages[0].account.user_id.clone();
    invalid[5][1].account.workspace_id = usages[0].account.workspace_id.clone();
    invalid[6][1].checked_at = NOW + 1;
    for (failure, values) in invalid.into_iter().enumerate() {
        status.record_usage(&values, NOW);
        assert_eq!(
            (status.account_display(NOW), status.pool_display(NOW)),
            (
                Some("oa 14% 6d19h".to_owned()),
                Some("nano unknown".to_owned())
            ),
            "failure {failure}"
        );
    }
    usages[1].windows[0].resets_at = None;
    status.record_usage(&usages, NOW);
    assert_eq!(status.pool_display(NOW), Some("nano 114%".to_owned()));
    usages[1].windows[0].resets_at = Some(NOW - 1);
    status.record_usage(&usages, NOW);
    assert_eq!(status.pool_display(NOW), Some("nano 114% due".to_owned()));
    status.pool = None;
    status.record_usage(&usages, NOW);
    assert_eq!(status.pool_display(NOW), None);
}

fn usage(alias: &str) -> ManagedAccountUsage {
    ManagedAccountUsage {
        account: ManagedAccount {
            alias: alias.to_owned(),
            user_id: format!("user-{alias}"),
            workspace_id: format!("workspace-{alias}"),
            email: Some(format!("{alias}@example.com")),
            plan: Some("pro".to_owned()),
        },
        pools: vec!["work".to_owned()],
        model: None,
        model_supported: None,
        ordinary_usage_allowed: Some(true),
        windows: vec![AccountQuotaWindow {
            limit_id: "codex".to_owned(),
            model: None,
            remaining_percent: 50.0,
            window_minutes: WEEKLY_MINUTES,
            resets_at: Some(NOW + 6 * 86_400 + 21 * 3600),
        }],
        available_resets: Some(1),
        resets: None,
        checked_at: NOW,
        error: None,
    }
}

#[test]
fn weekly_countdown_handles_boundaries_unknown_times_and_stale_data() {
    let mut status = AccountStatus::default();
    let values: Vec<_> = [
        None,
        Some(i64::MAX),
        Some(NOW - 1),
        Some(NOW + 30),
        Some(NOW + 60),
        Some(NOW + 5400),
        Some(NOW + 6 * 86_400 + 21 * 3600),
    ]
    .into_iter()
    .map(|resets_at| {
        status.weekly = Some(WeeklyQuota {
            remaining_percent: 50.0,
            resets_at,
            checked_at: NOW,
        });
        status.weekly_display(NOW)
    })
    .collect();
    assert_eq!(
        values,
        [
            "50%",
            "50%",
            "50% due",
            "50% <1m",
            "50% 1m",
            "50% 1h30m",
            "50% 6d21h"
        ]
        .map(|value| Some(value.to_owned()))
    );
    assert_eq!(status.weekly_display(NOW + 15 * 60), None);
    let invalid: Vec<_> = [f64::NAN, f64::INFINITY, -1.0, 101.0]
        .into_iter()
        .map(|remaining_percent| {
            status.weekly = Some(WeeklyQuota {
                remaining_percent,
                resets_at: None,
                checked_at: NOW,
            });
            status.weekly_display(NOW)
        })
        .collect();
    assert_eq!(invalid, [None, None, None, None]);
}

#[tokio::test]
async fn managed_footer_refresh_is_scoped_throttled_and_rejects_previous_account_reads() {
    let (mut chat, _tx, mut events, _ops) = make_chatwidget_manual_with_sender().await;
    chat.managed_accounts_active = true;
    chat.local_settings.tui.status_line =
        Some(vec!["account-weekly".to_owned(), "pool-weekly".to_owned()]);
    let a = usage("a");
    let mut b = usage("b");
    b.windows[0].remaining_percent = 75.0;
    let pool = AccountPool {
        name: "work".to_owned(),
        accounts: vec!["a".to_owned(), "b".to_owned()],
        redeem_weekly_resets: true,
    };
    chat.initialize_managed_account_status(Some(a.account.clone()), Some(pool.clone()));
    chat.refresh_account_status_if_due();
    let AppEvent::RefreshAccountStatus {
        request_id: first,
        account,
        pool: requested_pool,
    } = events.try_recv().unwrap()
    else {
        panic!("expected selected account read");
    };
    assert_eq!(
        (account, requested_pool),
        (a.account.clone(), Some(pool.clone()))
    );
    chat.refresh_account_status_if_due();
    assert!(events.try_recv().is_err());

    chat.handle_server_notification(
        ServerNotification::ThreadAccountPool(ThreadAccountPoolNotification {
            thread_id: "thread-test".to_owned(),
            event: AccountPoolEvent::Selected {
                account: b.account.clone(),
            },
        }),
        /*replay_kind*/ None,
    );
    assert_eq!(
        chat.status_line_value_for_item(StatusLineItem::AccountEmail),
        Some("b@example.com".to_owned())
    );
    chat.refresh_account_status_if_due();
    let AppEvent::RefreshAccountStatus {
        request_id: second,
        account,
        pool: requested_pool,
    } = events.try_recv().unwrap()
    else {
        panic!("expected new account read");
    };
    assert_eq!((account, requested_pool), (b.account.clone(), Some(pool)));
    chat.finish_account_status(first, Ok(vec![a]));
    assert_eq!(chat.account_status.weekly_display(NOW), None);
    chat.finish_account_status(second, Ok(vec![b.clone()]));
    assert_eq!(
        chat.account_status.weekly_display(NOW),
        Some("75% 6d21h".to_owned())
    );
    chat.refresh_account_status_if_due();
    assert!(events.try_recv().is_err());

    chat.account_status.next_check = Some(Instant::now());
    chat.refresh_account_status_if_due();
    let AppEvent::RefreshAccountStatus { request_id, .. } = events.try_recv().unwrap() else {
        panic!("expected next scheduled read");
    };
    b.error = Some("usage unavailable".to_owned());
    chat.finish_account_status(request_id, Ok(vec![b]));
    assert_eq!(chat.account_status.weekly_display(NOW), None);
    assert_eq!(chat.rate_limit_refresh_interval(), None);
}

#[tokio::test]
async fn legacy_footer_uses_account_wide_weekly_quota_and_clears_on_identity_change() {
    let (mut chat, _tx, _events, _ops) = make_chatwidget_manual_with_sender().await;
    let now = Local::now().timestamp();
    let snapshot = RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        normal_model_slug: None,
        primary: Some(RateLimitWindow {
            used_percent: 50,
            window_duration_mins: Some(WEEKLY_MINUTES),
            resets_at: Some(now + 5400),
        }),
        secondary: Some(RateLimitWindow {
            used_percent: 99,
            window_duration_mins: Some(300),
            resets_at: Some(now + 600),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    };
    chat.on_rate_limit_snapshot(Some(snapshot.clone()));
    assert_eq!(
        chat.account_status.weekly_display(now),
        Some("50% 1h30m".to_owned())
    );
    chat.on_rate_limit_snapshot(Some(RateLimitSnapshot {
        limit_id: Some("codex_other".to_owned()),
        ..snapshot.clone()
    }));
    assert_eq!(
        chat.account_status.weekly_display(now),
        Some("50% 1h30m".to_owned())
    );
    // A late legacy read must not replace quota once the managed-account poller owns it.
    chat.managed_accounts_active = true;
    chat.on_rate_limit_snapshot(Some(RateLimitSnapshot {
        primary: Some(RateLimitWindow {
            used_percent: 99,
            window_duration_mins: Some(WEEKLY_MINUTES),
            resets_at: Some(now + 600),
        }),
        ..snapshot
    }));
    assert_eq!(
        chat.account_status.weekly_display(now),
        Some("50% 1h30m".to_owned())
    );
    chat.on_rate_limit_snapshot(None);
    assert_eq!(
        chat.account_status.weekly_display(now),
        Some("50% 1h30m".to_owned())
    );
    chat.update_account_state(
        Some(StatusAccountDisplay::ApiKey),
        /*plan_type*/ None,
        /*has_chatgpt_account*/ false,
        /*has_codex_backend_auth*/ false,
    );
    assert_eq!(
        (
            chat.status_line_value_for_item(StatusLineItem::AccountEmail),
            chat.account_status.weekly_display(now)
        ),
        (None, None)
    );
}
