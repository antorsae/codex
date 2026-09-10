use super::*;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountQuotaWindow;
use codex_protocol::account_pool::ManagedAccount;
use codex_protocol::account_pool::ManagedAccountUsage;
use codex_protocol::account_pool::PoolWaitReason;

#[tokio::test]
async fn configured_account_footer_and_picker_snapshot() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    let (mut chat, mut events, _ops) = make_chatwidget_manual(Some("gpt-5.4")).await;
    set_chatgpt_auth(&mut chat);
    set_fast_mode_test_catalog(&mut chat);
    chat.show_welcome_banner = false;
    chat.config.cwd = test_project_path().abs();
    chat.local_settings.tui.status_line = Some(
        [
            "model-with-reasoning",
            "current-dir",
            "git-branch",
            "weekly-limit-with-reset",
            "account-email",
        ]
        .map(str::to_owned)
        .to_vec(),
    );
    chat.set_reasoning_effort(Some(ReasoningEffortConfig::XHigh));
    chat.set_service_tier(Some(ServiceTier::Fast.request_value().to_owned()));
    chat.status_line_branch_cwd = Some(chat.config.cwd.to_path_buf());
    chat.status_line_branch = Some("feature/account-footer".to_owned());
    chat.status_line_branch_lookup_complete = true;
    chat.status_account_display = Some(StatusAccountDisplay::ChatGpt {
        email: Some("a@example.com".to_owned()),
        plan: Some("pro".to_owned()),
    });
    chat.on_rate_limit_snapshot(Some(RateLimitSnapshot {
        limit_id: None,
        limit_name: None,
        normal_model_slug: None,
        primary: None,
        secondary: Some(RateLimitWindow {
            used_percent: 50,
            window_duration_mins: Some(7 * 24 * 60),
            // Mid-hour keeps the UI snapshot stable; exact clock boundaries are tested separately.
            resets_at: Some(Local::now().timestamp() + 6 * 86_400 + 21 * 3600 + 1800),
        }),
        credits: None,
        individual_limit: None,
        spend_control_reached: None,
        plan_type: None,
        rate_limit_reached_type: None,
    }));
    drain_insert_history(&mut events);
    for width in [140, 80] {
        let mut terminal =
            Terminal::new(TestBackend::new(width, chat.desired_height(width))).unwrap();
        terminal
            .draw(|frame| chat.render(frame.area(), frame.buffer_mut()))
            .unwrap();
        assert_chatwidget_snapshot!(
            format!("configured_account_footer_{width}"),
            normalized_backend_snapshot(terminal.backend())
        );
    }
    chat.open_status_line_setup();
    assert_chatwidget_snapshot!(
        "configured_account_footer_picker",
        render_bottom_popup(&chat, /*width*/ 100)
    );
}

#[tokio::test]
async fn native_account_pool_controls_and_recovery_snapshot() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.dispatch_command(SlashCommand::Accounts);
    assert_matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_)));
    assert_matches!(rx.try_recv(), Ok(AppEvent::ManagedAccounts { args }) if args.is_empty());
    chat.dispatch_command(SlashCommand::Pools);
    assert_matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_)));
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
    chat.add_account_pool_report(history_cell::AccountPoolReport::accounts(
        &codex_app_server_protocol::ManagedAccountResponse {
            resolved: None,
            selected_account: None,
            models: None,
            data: vec![account.clone()],
            next_cursor: None,
            usage: vec![usage],
            login: None,
            default_selection: None,
        },
        history_cell::AccountReportView::Summary,
        /*now*/ 1_800_000_000,
    ));
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
    assert_matches!(rx.try_recv(), Ok(AppEvent::InsertHistoryCell(_)));
    assert_matches!(rx.try_recv(), Ok(AppEvent::ManagedAccounts { args }) if args == "usage");
    assert_eq!(
        chat.status_account_display,
        Some(StatusAccountDisplay::ChatGpt {
            email: Some("b@example.com".to_owned()),
            plan: Some("pro".to_owned()),
        })
    );
}

#[tokio::test]
async fn account_pool_commands_stay_visible_and_recallable_without_starting_a_turn() {
    let (mut chat, mut rx, mut op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.managed_accounts_active = true;
    chat.has_chatgpt_account = true;
    let mut history = Vec::new();
    for command in [
        "/accounts",
        "/accounts select oc",
        "/pools",
        "/pools select work",
        "/usage",
    ] {
        chat.bottom_pane
            .set_composer_text(command.to_owned(), Vec::new(), Vec::new());
        chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        let AppEvent::InsertHistoryCell(cell) = rx.try_recv().expect("command echo") else {
            panic!("expected the submitted command before its request");
        };
        history.extend(cell.display_lines(/*width*/ 80));
        assert_eq!(crate::app_backtrack::user_count(&[Arc::from(cell)]), 0);
        // The echo is local transcript output; the command still follows its usual RPC route.
        let expected_args = command.split_once(' ').map_or("", |(_, args)| args);
        match rx.try_recv().expect("account or pool request") {
            AppEvent::ManagedAccounts { args } => {
                assert_eq!(
                    args,
                    if command == "/usage" {
                        "usage"
                    } else {
                        expected_args
                    }
                );
            }
            AppEvent::ManagedPools { args } => assert_eq!(args, expected_args),
            event => panic!("unexpected event: {event:?}"),
        }
        assert_eq!(chat.bottom_pane.composer_text(), "");
        chat.handle_key_event(KeyEvent::new(KeyCode::Up, KeyModifiers::NONE));
        assert_eq!(chat.bottom_pane.composer_text(), command);
        while rx.try_recv().is_ok() {}
        assert_matches!(op_rx.try_recv(), Err(TryRecvError::Empty));
    }
    assert_chatwidget_snapshot!(
        "account_pool_command_history",
        lines_to_single_string(&history)
    );
}

#[tokio::test]
async fn account_pool_reports_are_copyable_until_the_next_response() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let report =
        history_cell::AccountPoolReport::pools(&codex_app_server_protocol::ManagedPoolResponse {
            data: vec![],
            next_cursor: None,
            default_selection: None,
        });
    let expected = report.markdown.clone();
    chat.add_account_pool_report(report);
    drain_insert_history(&mut rx);
    chat.copy_last_agent_markdown_with(|source| {
        assert_eq!(source, expected);
        Ok(None)
    });
    drain_insert_history(&mut rx);
    assert_eq!(chat.transcript.last_agent_markdown, None);
    chat.dispatch_command(SlashCommand::Copy);
    assert_chatwidget_snapshot!(
        "account_pool_report_copy_picker",
        render_bottom_popup(&chat, /*width*/ 80)
    );
    chat.handle_key_event(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
    chat.transcript
        .record_agent_markdown("Next response".to_owned(), "Next response".to_owned());
    chat.copy_last_agent_markdown_with(|source| {
        assert_eq!(source, "Next response");
        Ok(None)
    });
}
