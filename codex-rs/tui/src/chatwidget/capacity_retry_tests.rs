use super::*;
use codex_app_server_protocol::AgentMessageDeltaNotification;
use codex_app_server_protocol::PlanDeltaNotification;
use codex_app_server_protocol::ReasoningTextDeltaNotification;
use pretty_assertions::assert_eq;

fn capacity_failure(chat: &ChatWidget, turn_id: &str) -> TurnCompletedNotification {
    TurnCompletedNotification {
        thread_id: chat.thread_id.unwrap().to_string(),
        turn: app_server_turn(
            turn_id,
            AppServerTurnStatus::Failed,
            /*duration_ms*/ None,
            Some(AppServerTurnError {
                message: "Selected model is at capacity. Please try a different model.".into(),
                codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
                additional_details: None,
                misalignment: None,
            }),
        ),
    }
}

fn fail_at_capacity(chat: &mut ChatWidget, turn_id: &str) {
    handle_turn_started(chat, turn_id);
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(capacity_failure(chat, turn_id)),
        /*replay_kind*/ None,
    );
}

#[tokio::test]
async fn capacity_retry_waits_then_submits_empty_turn_and_stops_on_success() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    handle_turn_started(&mut chat, "original");
    let failure = capacity_failure(&chat, "original");
    chat.handle_server_notification(
        ServerNotification::Error(ErrorNotification {
            thread_id: failure.thread_id.clone(),
            turn_id: failure.turn.id.clone(),
            error: failure.turn.error.clone().unwrap(),
            will_retry: false,
        }),
        /*replay_kind*/ None,
    );
    assert!(chat.capacity_retry.pending.is_none());
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(failure.clone()),
        /*replay_kind*/ None,
    );
    let deadline = chat.capacity_retry.pending.as_ref().unwrap().deadline;
    let delay = deadline - tokio::time::Instant::now();
    assert!((Duration::from_secs(10)..=Duration::from_secs(60)).contains(&delay));
    tokio::time::advance(delay - Duration::from_millis(1)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);

    // Duplicate terminal notifications must neither add an attempt nor move the timer.
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(failure),
        /*replay_kind*/ None,
    );
    assert_eq!(
        chat.capacity_retry.pending.as_ref().unwrap().deadline,
        deadline
    );
    tokio::time::advance(Duration::from_millis(1)).await;
    chat.pre_draw_tick();
    let Op::UserTurn { items, model, .. } = next_submit_op(&mut ops) else {
        unreachable!();
    };
    assert_eq!(
        (items, model),
        (Vec::new(), chat.current_model().to_string(),)
    );
    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<String>()
        .replace(&format!("{}s", delay.as_secs()), "[delay]s");
    insta::assert_snapshot!("capacity_retry_native", rendered);
    assert!(chat.is_user_turn_pending_or_running());
    // Drawing again while turn/start is pending must not dispatch another retry.
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);

    handle_turn_started(&mut chat, "retry-1");
    handle_turn_completed(&mut chat, "retry-1", /*duration_ms*/ None);
    tokio::time::advance(Duration::from_secs(600)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
    assert_eq!(chat.capacity_retry.attempts, 0);
}

#[tokio::test]
async fn capacity_retry_restarts_after_progress_before_turn_completion() {
    enum Progress {
        FileChange,
        Command,
        Message,
        Reasoning,
        RawReasoning,
        Plan,
        CompletedMessage,
    }

    for progress in [
        Progress::FileChange,
        Progress::Command,
        Progress::Message,
        Progress::Reasoning,
        Progress::RawReasoning,
        Progress::Plan,
        Progress::CompletedMessage,
    ] {
        let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.thread_id = Some(ThreadId::new());
        tokio::time::pause();
        for attempt in 0..6 {
            fail_at_capacity(&mut chat, &format!("failed-{attempt}"));
            tokio::time::advance(Duration::from_secs(60)).await;
            chat.pre_draw_tick();
            next_submit_op(&mut ops);
        }
        handle_turn_started(&mut chat, "working");
        assert_eq!(chat.capacity_retry.attempts, 6);
        let thread_id = chat.thread_id.unwrap().to_string();
        match progress {
            Progress::FileChange => handle_patch_apply_end(
                &mut chat,
                "patch",
                "working",
                HashMap::from([(
                    PathBuf::from("example.py"),
                    FileChange::Add {
                        content: "print('working')\n".into(),
                    },
                )]),
                AppServerPatchApplyStatus::Completed,
            ),
            Progress::Command => {
                begin_exec_with_source(&mut chat, "command", "pwd", ExecCommandSource::Agent);
            }
            Progress::Message => handle_agent_message_delta(&mut chat, "I found the issue."),
            Progress::Reasoning => handle_agent_reasoning_delta(&mut chat, "Checking the fix."),
            Progress::RawReasoning => chat.handle_server_notification(
                ServerNotification::ReasoningTextDelta(ReasoningTextDeltaNotification {
                    thread_id,
                    turn_id: "working".into(),
                    item_id: "reasoning".into(),
                    delta: "Checking the fix.".into(),
                    content_index: 0,
                }),
                /*replay_kind*/ None,
            ),
            Progress::Plan => chat.handle_server_notification(
                ServerNotification::PlanDelta(PlanDeltaNotification {
                    thread_id,
                    turn_id: "working".into(),
                    item_id: "plan".into(),
                    delta: "Update the retry counter.".into(),
                }),
                /*replay_kind*/ None,
            ),
            Progress::CompletedMessage => chat.handle_server_notification(
                ServerNotification::ItemCompleted(ItemCompletedNotification {
                    thread_id,
                    turn_id: "working".into(),
                    completed_at_ms: 0,
                    item: AppServerThreadItem::AgentMessage {
                        id: "message".into(),
                        text: "I found the issue.".into(),
                        phase: Some(MessagePhase::Commentary),
                        memory_citation: None,
                        delivery: None,
                        questions: None,
                    },
                }),
                /*replay_kind*/ None,
            ),
        }
        assert_eq!(chat.capacity_retry.attempts, 0);
        assert!(chat.is_agent_turn_running());
        drain_insert_history(&mut rx);

        // The same turn can hit capacity on its next inference request after doing useful work.
        chat.handle_server_notification(
            ServerNotification::TurnCompleted(capacity_failure(&chat, "working")),
            /*replay_kind*/ None,
        );
        let delay =
            chat.capacity_retry.pending.as_ref().unwrap().deadline - tokio::time::Instant::now();
        tokio::time::advance(delay).await;
        chat.pre_draw_tick();
        let Op::UserTurn { items, .. } = next_submit_op(&mut ops) else {
            unreachable!();
        };
        assert_eq!(items, Vec::<UserInput>::new());
        assert_eq!(chat.capacity_retry.attempts, 1);
        if matches!(progress, Progress::FileChange) {
            let rendered = drain_insert_history(&mut rx)
                .into_iter()
                .map(|lines| lines_to_single_string(&lines))
                .collect::<String>()
                .replace(&format!("{}s", delay.as_secs()), "[delay]s");
            insta::assert_snapshot!("capacity_retry_restarts_after_progress", rendered);
        }
        tokio::time::resume();
    }
}

#[tokio::test]
async fn capacity_retry_ignores_non_progress_replay_and_unrelated_turns() {
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    let thread_id = ThreadId::new();
    chat.thread_id = Some(thread_id);
    tokio::time::pause();
    fail_at_capacity(&mut chat, "original");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    next_submit_op(&mut ops);
    handle_turn_started(&mut chat, "retry");
    complete_user_message(&mut chat, "echo", "continue");
    for item in [
        AppServerThreadItem::Reasoning {
            id: "empty-reasoning".into(),
            summary: Vec::new(),
            content: Vec::new(),
        },
        AppServerThreadItem::ContextCompaction {
            id: "compaction".into(),
        },
    ] {
        chat.handle_server_notification(
            ServerNotification::ItemStarted(ItemStartedNotification {
                thread_id: thread_id.to_string(),
                turn_id: "retry".into(),
                started_at_ms: 0,
                item,
            }),
            /*replay_kind*/ None,
        );
    }
    assert_eq!(chat.capacity_retry.attempts, 1);
    let progress = AgentMessageDeltaNotification {
        thread_id: thread_id.to_string(),
        turn_id: "retry".into(),
        item_id: "message".into(),
        delta: "Making progress.".into(),
    };
    for replay_kind in [
        ReplayKind::ResumeInitialMessages,
        ReplayKind::ThreadSnapshot,
    ] {
        chat.handle_server_notification(
            ServerNotification::AgentMessageDelta(progress.clone()),
            Some(replay_kind),
        );
        assert_eq!(chat.capacity_retry.attempts, 1);
    }
    for notification in [
        AgentMessageDeltaNotification {
            delta: " \n".into(),
            ..progress.clone()
        },
        AgentMessageDeltaNotification {
            turn_id: "old-turn".into(),
            ..progress.clone()
        },
        AgentMessageDeltaNotification {
            thread_id: ThreadId::new().to_string(),
            ..progress.clone()
        },
    ] {
        chat.handle_server_notification(
            ServerNotification::AgentMessageDelta(notification),
            /*replay_kind*/ None,
        );
        assert_eq!(chat.capacity_retry.attempts, 1);
    }
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(capacity_failure(&chat, "retry")),
        /*replay_kind*/ None,
    );
    let deadline = chat.capacity_retry.pending.as_ref().unwrap().deadline;
    // A delayed output event after failure must not clear the count or cancel its timer.
    chat.handle_server_notification(
        ServerNotification::AgentMessageDelta(progress),
        /*replay_kind*/ None,
    );
    assert_eq!(
        (
            chat.capacity_retry.attempts,
            chat.capacity_retry.pending.as_ref().unwrap().deadline,
        ),
        (1, deadline)
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    next_submit_op(&mut ops);
    assert_eq!(chat.capacity_retry.attempts, 2);
}

#[tokio::test]
async fn capacity_retry_stops_after_ten_native_attempts() {
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    let mut inputs = Vec::new();
    for attempt in 0..10 {
        fail_at_capacity(&mut chat, &format!("turn-{attempt}"));
        tokio::time::advance(Duration::from_secs(60)).await;
        chat.pre_draw_tick();
        let Op::UserTurn { items, .. } = next_submit_op(&mut ops) else {
            unreachable!();
        };
        inputs.push(items);
    }
    assert_eq!(inputs, vec![Vec::<UserInput>::new(); 10]);
    drain_insert_history(&mut rx);
    fail_at_capacity(&mut chat, "turn-10");
    tokio::time::advance(Duration::from_secs(600)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
    assert!(chat.capacity_retry.pending.is_none());
    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<String>();
    insta::assert_snapshot!("capacity_retry_exhausted", rendered);

    chat.submit_user_message("try again".into());
    next_submit_op(&mut ops);
    fail_at_capacity(&mut chat, "manual-turn");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    next_submit_op(&mut ops);
    assert_eq!(chat.capacity_retry.attempts, 1);
    handle_turn_started(&mut chat, "retry-after-manual-turn");
    handle_turn_interrupted(&mut chat, "retry-after-manual-turn");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
}

#[tokio::test]
async fn capacity_retry_dispatches_empty_input_without_recording_a_prompt() {
    let (mut chat, mut rx, _ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    chat.codex_op_target = CodexOpTarget::AppEvent;
    tokio::time::pause();
    fail_at_capacity(&mut chat, "original");
    drain_insert_history(&mut rx);
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    let mut observed = Vec::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AppEvent::InsertHistoryCell(cell) if cell.as_any().is::<UserHistoryCell>() => {
                observed.push((
                    "visible",
                    lines_to_single_string(&cell.display_lines(/*width*/ 80)),
                ));
            }
            AppEvent::CodexOp(Op::UserTurn { items, .. }) => {
                assert_eq!(items, Vec::<UserInput>::new());
                observed.push(("dispatch", String::new()));
            }
            AppEvent::AppendMessageHistoryEntry { text, .. } => observed.push(("history", text)),
            _ => {}
        }
    }
    assert_eq!(observed, vec![("dispatch", String::new())]);
}

#[tokio::test]
async fn capacity_retry_start_rejection_releases_pending_turn_and_preserves_draft() {
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    fail_at_capacity(&mut chat, "original");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    next_submit_op(&mut ops);
    assert!(chat.is_user_turn_pending_or_running());
    chat.handle_paste("my next request".into());
    assert!(chat.handle_turn_start_rejection("Failed to start turn".into()));
    assert!(!chat.is_user_turn_pending_or_running());
    assert_eq!(chat.bottom_pane.composer_text(), "my next request");

    chat.handle_key_event(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
    let Op::UserTurn { items, .. } = next_submit_op(&mut ops) else {
        unreachable!();
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "my next request".into(),
            text_elements: Vec::new(),
        }]
    );
    assert!(chat.input_queue.queued_user_messages.is_empty());
}

#[tokio::test]
async fn capacity_retry_cancels_on_typing_paste_escape_or_ctrl_c() {
    for (index, key) in [
        KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE),
        KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL),
    ]
    .into_iter()
    .enumerate()
    {
        let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
        chat.thread_id = Some(ThreadId::new());
        tokio::time::pause();
        let turn_id = format!("turn-{index}");
        fail_at_capacity(&mut chat, &turn_id);
        assert!(chat.capacity_retry.pending.is_some());
        chat.handle_key_event(key);
        assert!(chat.capacity_retry.pending.is_none());
        chat.bottom_pane
            .set_composer_text(String::new(), Vec::new(), Vec::new());
        // Clearing a draft and receiving a duplicate failure must not re-arm the retry.
        chat.handle_server_notification(
            ServerNotification::TurnCompleted(capacity_failure(&chat, &turn_id)),
            /*replay_kind*/ None,
        );
        tokio::time::advance(Duration::from_secs(60)).await;
        chat.pre_draw_tick();
        assert_no_submit_op(&mut ops);
        tokio::time::resume();
    }
    let (mut chat, mut rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    fail_at_capacity(&mut chat, "paste");
    assert!(chat.capacity_retry.pending.is_some());
    drain_insert_history(&mut rx);
    chat.handle_paste("my draft".into());
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
    assert_eq!(chat.bottom_pane.composer_text(), "my draft");
    let rendered = drain_insert_history(&mut rx)
        .into_iter()
        .map(|lines| lines_to_single_string(&lines))
        .collect::<String>();
    insta::assert_snapshot!("capacity_retry_canceled", rendered);
}

#[tokio::test]
async fn capacity_retry_preserves_existing_drafts_and_queued_input() {
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    chat.handle_paste("my draft".into());
    fail_at_capacity(&mut chat, "draft");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
    assert_eq!(chat.bottom_pane.composer_text(), "my draft");

    chat.bottom_pane
        .set_composer_text(String::new(), Vec::new(), Vec::new());
    handle_turn_started(&mut chat, "queued");
    chat.queue_user_message("my next request".into());
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(capacity_failure(&chat, "queued")),
        /*replay_kind*/ None,
    );
    let Op::UserTurn { items, .. } = next_submit_op(&mut ops) else {
        unreachable!();
    };
    assert_eq!(
        items,
        vec![UserInput::Text {
            text: "my next request".into(),
            text_elements: Vec::new(),
        }]
    );
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
}

#[tokio::test]
async fn capacity_retry_rechecks_draft_thread_model_and_active_turn_before_sending() {
    enum Change {
        Draft,
        Thread,
        Model,
        Turn,
        Disconnect,
    }
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    for (index, change) in [
        Change::Draft,
        Change::Thread,
        Change::Model,
        Change::Turn,
        Change::Disconnect,
    ]
    .into_iter()
    .enumerate()
    {
        fail_at_capacity(&mut chat, &format!("turn-{index}"));
        assert!(chat.capacity_retry.pending.is_some());
        match change {
            Change::Draft => {
                chat.bottom_pane
                    .set_composer_text("restored draft".into(), Vec::new(), Vec::new())
            }
            Change::Thread => chat.thread_id = Some(ThreadId::new()),
            Change::Model => chat.set_model("other-model"),
            Change::Turn => handle_turn_started(&mut chat, "other-client-turn"),
            Change::Disconnect => chat.pause_for_disconnect(),
        }
        tokio::time::advance(Duration::from_secs(60)).await;
        chat.pre_draw_tick();
        assert_no_submit_op(&mut ops);
        assert!(chat.capacity_retry.pending.is_none());
        chat.bottom_pane
            .set_composer_text(String::new(), Vec::new(), Vec::new());
    }
}

#[tokio::test]
async fn capacity_retry_ignores_replay_and_other_errors() {
    let (mut chat, _rx, mut ops) = make_chatwidget_manual(/*model_override*/ None).await;
    chat.thread_id = Some(ThreadId::new());
    tokio::time::pause();
    for replay in [
        ReplayKind::ResumeInitialMessages,
        ReplayKind::ThreadSnapshot,
    ] {
        let failure = capacity_failure(&chat, "historical");
        chat.replay_thread_turns(vec![failure.turn], replay);
        tokio::time::advance(Duration::from_secs(60)).await;
        chat.pre_draw_tick();
        assert_no_submit_op(&mut ops);
    }
    fail_at_capacity(&mut chat, "original");
    tokio::time::advance(Duration::from_secs(60)).await;
    chat.pre_draw_tick();
    next_submit_op(&mut ops);
    handle_turn_started(&mut chat, "retry-1");
    let mut failure = capacity_failure(&chat, "retry-1");
    failure.turn.error.as_mut().unwrap().codex_error_info =
        Some(CodexErrorInfo::InternalServerError);
    chat.handle_server_notification(
        ServerNotification::TurnCompleted(failure),
        /*replay_kind*/ None,
    );
    tokio::time::advance(Duration::from_secs(600)).await;
    chat.pre_draw_tick();
    assert_no_submit_op(&mut ops);
    assert_eq!(chat.capacity_retry.attempts, 0);
}
