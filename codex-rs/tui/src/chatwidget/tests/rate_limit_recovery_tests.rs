use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn replayed_quota_errors_preserve_input_and_live_recovery_state() {
    for replay_kind in [
        ReplayKind::ResumeInitialMessages,
        ReplayKind::ThreadSnapshot,
    ] {
        for recovery_pending in [false, true] {
            let (mut chat, mut events, mut ops) =
                make_chatwidget_manual(/*model_override*/ None).await;
            set_chatgpt_auth(&mut chat);
            chat.thread_id = Some(ThreadId::new());
            chat.set_queue_autosend_suppressed(/*suppressed*/ true);
            chat.queue_user_message("queued follow-up".into());
            chat.set_queue_autosend_suppressed(/*suppressed*/ false);
            chat.input_queue.rate_limit_recovery_pending = recovery_pending;
            chat.handle_paste("unfinished draft".to_string());
            let error = AppServerTurnError {
                message: "You've hit your usage limit.".to_string(),
                codex_error_info: Some(CodexErrorInfo::UsageLimitExceeded),
                additional_details: None,
                misalignment: None,
            };
            if replay_kind == ReplayKind::ThreadSnapshot {
                chat.handle_server_notification(
                    ServerNotification::Error(ErrorNotification {
                        thread_id: chat.thread_id.unwrap().to_string(),
                        turn_id: "old-quota-error".to_string(),
                        error: error.clone(),
                        will_retry: false,
                    }),
                    Some(replay_kind),
                );
            }
            chat.replay_thread_turns(
                vec![app_server_turn(
                    "old-quota-error",
                    AppServerTurnStatus::Failed,
                    /*duration_ms*/ None,
                    Some(error),
                )],
                replay_kind,
            );
            assert_eq!(
                (
                    chat.input_queue.rate_limit_recovery_pending,
                    chat.queued_user_message_texts(),
                    chat.composer_text_with_pending(),
                ),
                (
                    recovery_pending,
                    vec!["queued follow-up".to_string()],
                    "unfinished draft".to_string()
                ),
            );
            assert_no_submit_op(&mut ops);
            let mut history = String::new();
            while let Ok(event) = events.try_recv() {
                match event {
                    AppEvent::InsertHistoryCell(cell) => {
                        history.push_str(&lines_to_single_string(&cell.display_lines(/*width*/ 80)))
                    }
                    AppEvent::RefreshRateLimits { .. } => {
                        panic!("replay must not start quota recovery")
                    }
                    _ => {}
                }
            }
            insta::assert_snapshot!("replayed_quota_error", history);
        }
    }
}

#[tokio::test]
async fn managed_quota_errors_leave_recovery_to_the_pool() {
    let (mut chat, mut events, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    set_chatgpt_auth(&mut chat);
    chat.thread_id = Some(ThreadId::new());
    chat.managed_accounts_active = true;
    handle_turn_started(&mut chat, "failed-turn");
    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    chat.submit_user_message("continue".into());
    let Op::UserTurn { items, .. } = next_submit_op(&mut ops) else {
        panic!("expected a new turn for pool recovery");
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "continue".to_string(),
            text_elements: Vec::new(),
        }]
    );
    assert_no_submit_op(&mut ops);
    assert!(!chat.input_queue.rate_limit_recovery_pending);
    assert!(chat.queued_user_message_texts().is_empty());
    assert!(
        !std::iter::from_fn(|| events.try_recv().ok())
            .any(|event| matches!(event, AppEvent::RefreshRateLimits { .. }))
    );
}

#[tokio::test]
async fn rate_limit_recovery_holds_submissions_until_model_change() {
    let (mut chat, mut events, mut ops) = make_chatwidget_manual(Some("test-model-a")).await;
    set_chatgpt_auth(&mut chat);
    chat.thread_id = Some(ThreadId::new());
    handle_turn_started(&mut chat, "failed-turn");
    chat.queue_user_message(UserMessage::from("queued follow-up"));
    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    assert!(
        std::iter::from_fn(|| events.try_recv().ok()).any(|event| matches!(
            event,
            AppEvent::RefreshRateLimits {
                origin: crate::app_event::RateLimitRefreshOrigin::Recovery
            }
        ))
    );
    chat.submit_user_message(UserMessage::from("submitted during recovery"));
    assert_no_submit_op(&mut ops);
    assert_eq!(
        chat.queued_user_message_texts(),
        vec!["queued follow-up", "submitted during recovery"]
    );

    chat.set_model("test-model-b");
    chat.finish_rate_limit_recovery();
    let Op::UserTurn { model, items, .. } = next_submit_op(&mut ops) else {
        panic!("expected queued follow-up on the fallback model");
    };
    assert_eq!(model, "test-model-b");
    assert!(
        matches!(items.as_slice(), [UserInput::Text { text, .. }] if text == "queued follow-up")
    );
    assert_eq!(
        chat.queued_user_message_texts(),
        vec!["submitted during recovery"]
    );
    chat.finish_rate_limit_recovery();
    assert_no_submit_op(&mut ops);
}

#[tokio::test]
async fn rate_limit_recovery_preserves_settings_hold_and_clears_on_account_change() {
    let (mut chat, _events, mut ops) = make_chatwidget_manual(Some("test-model-a")).await;
    set_chatgpt_auth(&mut chat);
    chat.thread_id = Some(ThreadId::new());
    chat.set_queue_autosend_suppressed(/*suppressed*/ true);
    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    chat.queue_user_message(UserMessage::from("queued follow-up"));
    chat.finish_rate_limit_recovery();
    assert_no_submit_op(&mut ops);
    assert!(chat.input_queue.suppress_queue_autosend);
    assert!(!chat.input_queue.rate_limit_recovery_pending);

    chat.on_rate_limit_error(RateLimitErrorKind::UsageLimit, "Usage exhausted".into());
    chat.update_account_state(
        /*status_account_display*/ None, /*plan_type*/ None,
        /*has_chatgpt_account*/ false, /*has_codex_backend_auth*/ false,
    );
    assert!(!chat.input_queue.rate_limit_recovery_pending);
    assert_eq!(chat.queued_user_message_texts(), vec!["queued follow-up"]);
    assert_no_submit_op(&mut ops);
}
