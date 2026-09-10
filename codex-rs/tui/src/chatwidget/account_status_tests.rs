use super::*;
use crate::chatwidget::tests::make_chatwidget_manual_with_sender;
use codex_app_server_protocol::RateLimitWindow;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountQuotaWindow;
use pretty_assertions::assert_eq;

const NOW: i64 = 1_800_000_000;

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
    chat.local_settings.tui.status_line = Some(vec!["weekly-limit-with-reset".to_owned()]);
    let a = usage("a");
    let mut b = usage("b");
    b.windows[0].remaining_percent = 75.0;
    chat.initialize_managed_account_status(Some(a.account.clone()));
    chat.refresh_account_status_if_due();
    let AppEvent::RefreshAccountStatus {
        request_id: first,
        account,
    } = events.try_recv().unwrap()
    else {
        panic!("expected selected account read");
    };
    assert_eq!(account, a.account);
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
    } = events.try_recv().unwrap()
    else {
        panic!("expected new account read");
    };
    assert_eq!(account, b.account);
    chat.finish_account_status(first, Ok(a));
    assert_eq!(chat.account_status.weekly_display(NOW), None);
    chat.finish_account_status(second, Ok(b.clone()));
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
    chat.finish_account_status(request_id, Ok(b));
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
