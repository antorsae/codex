//! Exercise automatic capacity recovery through the app-server submission path.

use super::*;
use codex_app_server_protocol::CodexErrorInfo;
use codex_app_server_protocol::TurnStartParams;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn capacity_retry_sends_native_turn_with_current_settings() -> Result<()> {
    for mode in [ModeKind::Default, ModeKind::Plan] {
        let (mut app, _events, mut ops) = make_test_app_with_channels().await;
        let (mut server, requests, proxy) =
            backend_banner_fallback_tests::start_fallback_thread(&mut app).await?;
        let thread_id = app.active_thread_id.expect("active thread");
        app.chat_widget
            .set_feature_enabled(Feature::FastMode, /*enabled*/ true);
        let service_tier = ServiceTier::Fast.request_value().to_string();
        app.chat_widget.set_service_tier(Some(service_tier.clone()));
        app.chat_widget
            .set_reasoning_effort(Some(ReasoningEffortConfig::Medium));
        if mode == ModeKind::Plan {
            app.chat_widget
                .handle_key_event(KeyEvent::from(KeyCode::BackTab));
        }
        let expected_mode = app.chat_widget.effective_collaboration_mode();
        tokio::time::pause();
        app.chat_widget.handle_server_notification(
            ServerNotification::TurnStarted(TurnStartedNotification {
                thread_id: thread_id.to_string(),
                turn: test_turn("original", TurnStatus::InProgress, Vec::new()),
            }),
            /*replay_kind*/ None,
        );
        let mut failed = test_turn("original", TurnStatus::Failed, Vec::new());
        failed.error = Some(AppServerTurnError {
            message: "Selected model is at capacity. Please try a different model.".into(),
            codex_error_info: Some(CodexErrorInfo::ServerOverloaded),
            additional_details: None,
            misalignment: None,
        });
        app.chat_widget.handle_server_notification(
            ServerNotification::TurnCompleted(TurnCompletedNotification {
                thread_id: thread_id.to_string(),
                turn: failed,
            }),
            /*replay_kind*/ None,
        );
        // A cached active turn must not route empty retry input through turn/steer.
        app.ensure_thread_channel(thread_id)
            .store
            .lock()
            .await
            .set_active_turn_id("original".into());
        tokio::time::advance(Duration::from_secs(60)).await;
        app.chat_widget.pre_draw_tick();
        let retry = next_user_turn_op(&mut ops);
        tokio::time::resume();
        requests.lock().unwrap().clear();
        let mut tui = crate::tui::test_support::make_test_tui()?;
        app.handle_event(&mut tui, &mut server, AppEvent::CodexOp(retry))
            .await?;

        let sent = requests.lock().unwrap().clone();
        assert_eq!(
            sent.iter()
                .map(|request| request.method.as_str())
                .collect::<Vec<_>>(),
            ["turn/start"],
        );
        let request: TurnStartParams = serde_json::from_value(sent[0].params.clone().unwrap())?;
        assert_eq!(
            (
                request.thread_id,
                request.input,
                request.client_user_message_id,
                request.turn_trigger,
                request.model,
                request.effort,
                request.service_tier,
                request.collaboration_mode,
            ),
            (
                thread_id.to_string(),
                Vec::new(),
                None,
                Some("retry".into()),
                Some(expected_mode.model().to_string()),
                expected_mode.reasoning_effort(),
                Some(Some(service_tier)),
                Some(expected_mode),
            ),
        );
        server.shutdown().await?;
        proxy.await??;
    }
    Ok(())
}
