use super::*;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use codex_protocol::account_pool::PoolWaitReason;

#[tokio::test]
async fn native_account_pool_controls_and_recovery_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.dispatch_command(SlashCommand::Accounts);
    assert_matches!(rx.try_recv(), Ok(AppEvent::ManagedAccounts { args }) if args.is_empty());
    chat.dispatch_command(SlashCommand::Pools);
    assert_matches!(rx.try_recv(), Ok(AppEvent::ManagedPools { args }) if args.is_empty());
    let account = ManagedAccount {
        alias: "work-b".to_owned(),
        user_id: "user-b".to_owned(),
        workspace_id: "workspace-b".to_owned(),
        email: Some("b@example.com".to_owned()),
        plan: Some("pro".to_owned()),
    };
    let usage = ManagedAccountUsage {
        account: account.clone(),
        pools: vec!["work".to_owned()],
        model: Some("gpt-5.5".to_owned()),
        model_supported: Some(true),
        ordinary_usage_allowed: Some(false),
        windows: vec![
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: 0.0,
                window_minutes: 300,
                resets_at: Some(1_800_000_060),
            },
            AccountQuotaWindow {
                limit_id: "codex".to_owned(),
                model: None,
                remaining_percent: 40.0,
                window_minutes: 10080,
                resets_at: Some(1_800_604_800),
            },
        ],
        available_resets: Some(2),
        resets: None,
        checked_at: 1_800_000_000,
        error: None,
    };
    chat.add_info_message(usage.display_summary(), /*hint*/ None);
    for event in [
        AccountPoolEvent::Waiting {
            account: "work-a".to_owned(),
            reason: PoolWaitReason::ShortWindow,
            next_check_at: 1_800_000_060,
        },
        AccountPoolEvent::Switched {
            account: "work-b".to_owned(),
            previous_account: "work-a".to_owned(),
        },
        AccountPoolEvent::Selected { account },
    ] {
        chat.handle_server_notification(
            ServerNotification::ThreadAccountPool(ThreadAccountPoolNotification {
                thread_id: "thread-1".to_owned(),
                event,
            }),
            /*replay_kind*/ None,
        );
    }
    let history: Vec<_> = drain_insert_history(&mut rx)
        .into_iter()
        .flatten()
        .collect();
    assert_chatwidget_snapshot!(
        "native_account_pool_recovery",
        lines_to_single_string(&history)
    );
    assert!(chat.managed_accounts_active);
    chat.dispatch_command(SlashCommand::Usage);
    assert_matches!(rx.try_recv(), Ok(AppEvent::ManagedAccounts { args }) if args == "usage");
    assert_eq!(
        chat.status_account_display,
        Some(StatusAccountDisplay::ChatGpt {
            email: Some("b@example.com".to_owned()),
            plan: Some("pro".to_owned()),
        })
    );
}
