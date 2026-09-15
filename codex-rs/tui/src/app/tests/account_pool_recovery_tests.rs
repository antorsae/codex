use super::*;
use codex_app_server_protocol::CodexErrorInfo;
use codex_app_server_protocol::ErrorNotification;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::ManagedAccount;
use pretty_assertions::assert_eq;

fn quota_failure() -> Turn {
    let mut turn = test_turn("quota-failure", TurnStatus::Failed, Vec::new());
    turn.error = Some(AppServerTurnError {
        message: "You've hit your usage limit.".to_string(),
        codex_error_info: Some(CodexErrorInfo::UsageLimitExceeded),
        additional_details: None,
        misalignment: None,
    });
    turn
}

fn select_pool_account(app: &mut App, thread_id: ThreadId) {
    app.chat_widget.handle_server_notification(
        ServerNotification::ThreadAccountPool(ThreadAccountPoolNotification {
            thread_id: thread_id.to_string(),
            event: AccountPoolEvent::Selected {
                account: ManagedAccount {
                    alias: "available".to_string(),
                    user_id: "user-available".to_string(),
                    workspace_id: "workspace-available".to_string(),
                    email: None,
                    plan: Some("pro".to_string()),
                },
            },
        }),
        /*replay_kind*/ None,
    );
}

fn submitted_prompts(events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>) -> Vec<String> {
    std::iter::from_fn(|| events.try_recv().ok())
        .filter_map(|event| match event {
            AppEvent::CodexOp(Op::UserTurn { items, .. }) => {
                let [UserInput::Text { text, .. }] = items.as_slice() else {
                    panic!("expected one text prompt, got {items:?}");
                };
                Some(text.clone())
            }
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn account_pool_recovery_resume_accepts_initial_and_typed_prompts() -> Result<()> {
    for initial_prompt in ["continue", ""] {
        let (mut app, mut events, _ops) = make_test_app_with_channels().await;
        set_test_initial_prompt(&mut app, initial_prompt.to_string());
        set_chatgpt_auth(&mut app.chat_widget);
        app.chat_widget.managed_accounts_active = true;
        let thread_id = ThreadId::new();
        app.enqueue_primary_thread_session(
            test_thread_session(thread_id, app.config.cwd.to_path_buf()),
            vec![quota_failure()],
        )
        .await?;
        select_pool_account(&mut app, thread_id);
        if initial_prompt.is_empty() {
            app.chat_widget.handle_paste("continue".to_string());
            app.chat_widget
                .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        let mut prompts = Vec::new();
        let mut history = String::new();
        while let Ok(event) = events.try_recv() {
            match event {
                AppEvent::InsertHistoryCell(cell) => {
                    history.push_str(&lines_to_single_string(
                        &cell.transcript_lines(/*width*/ 80),
                    ));
                }
                AppEvent::CodexOp(Op::UserTurn { items, .. }) => {
                    assert!(history.contains("You've hit your usage limit."));
                    assert!(history.contains("› continue"));
                    prompts.push(items);
                }
                AppEvent::RefreshRateLimits {
                    origin: RateLimitRefreshOrigin::Recovery,
                } => panic!("historical quota errors must not start live recovery"),
                _ => {}
            }
        }
        assert_eq!(
            prompts,
            vec![vec![UserInput::Text {
                text: "continue".to_string(),
                text_elements: Vec::new(),
            }]],
        );
        assert!(app.chat_widget.queued_user_message_texts().is_empty());
        // Repeated account metadata must not resubmit the prompt.
        select_pool_account(&mut app, thread_id);
        assert_eq!(submitted_prompts(&mut events), Vec::<String>::new());
    }
    Ok(())
}

#[tokio::test]
async fn account_pool_recovery_releases_skipped_legacy_refresh() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    set_test_initial_prompt(&mut app, String::new());
    set_chatgpt_auth(&mut app.chat_widget);
    let thread_id = ThreadId::new();
    app.enqueue_primary_thread_session(
        test_thread_session(thread_id, app.config.cwd.to_path_buf()),
        Vec::new(),
    )
    .await?;
    let server = crate::start_embedded_app_server_for_picker(app.chat_widget.config_ref()).await?;
    let failure = quota_failure();
    app.chat_widget.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            thread_id: thread_id.to_string(),
            turn_id: failure.id,
            error: failure.error.unwrap(),
            will_retry: false,
        }),
        /*replay_kind*/ None,
    );
    app.chat_widget.handle_paste("continue".to_string());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    assert_eq!(
        app.chat_widget.queued_user_message_texts(),
        vec!["continue"]
    );
    let recovery = std::iter::from_fn(|| events.try_recv().ok())
        .find_map(|event| match event {
            AppEvent::RefreshRateLimits { origin } => Some(origin),
            _ => None,
        })
        .expect("legacy quota recovery request");
    // Pool discovery can finish after the error queued its legacy recovery request.
    app.chat_widget.managed_accounts_active = true;
    app.refresh_rate_limits(&server, recovery);
    assert_eq!(submitted_prompts(&mut events), vec!["continue"]);
    app.refresh_rate_limits(&server, recovery);
    assert_eq!(submitted_prompts(&mut events), Vec::<String>::new());
    assert!(app.chat_widget.queued_user_message_texts().is_empty());
    server.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn account_pool_recovery_selection_releases_only_the_quota_hold() -> Result<()> {
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    set_test_initial_prompt(&mut app, String::new());
    set_chatgpt_auth(&mut app.chat_widget);
    let thread_id = ThreadId::new();
    app.enqueue_primary_thread_session(
        test_thread_session(thread_id, app.config.cwd.to_path_buf()),
        Vec::new(),
    )
    .await?;
    app.chat_widget.hold_rate_limit_recovery();
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ true);
    app.chat_widget.handle_paste("continue".to_string());
    app.chat_widget
        .handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    app.chat_widget.handle_paste("unfinished draft".to_string());
    select_pool_account(&mut app, thread_id);
    assert_eq!(submitted_prompts(&mut events), Vec::<String>::new());
    assert_eq!(
        app.chat_widget.queued_user_message_texts(),
        vec!["continue"]
    );
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "unfinished draft"
    );
    app.chat_widget
        .set_queue_autosend_suppressed(/*suppressed*/ false);
    app.chat_widget.maybe_send_next_queued_input();
    assert_eq!(submitted_prompts(&mut events), vec!["continue"]);
    assert_eq!(
        app.chat_widget.composer_text_with_pending(),
        "unfinished draft"
    );
    Ok(())
}
