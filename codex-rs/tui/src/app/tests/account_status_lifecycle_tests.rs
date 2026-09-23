use super::*;
use crate::app::session_lifecycle::ThreadAttachPresentation;
use codex_protocol::account_pool::AccountSelection;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;

async fn load_footer_status(
    app: &mut App,
    tui: &mut crate::tui::Tui,
    server: &mut AppServerSession,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<AppEvent>,
) -> Result<AppEvent> {
    app.chat_widget.pre_draw_tick();
    let request = std::iter::from_fn(|| events.try_recv().ok())
        .find(|event| matches!(event, AppEvent::RefreshAccountStatus { .. }))
        .expect("an idle thread must refresh its account and pool without a Selected notification");
    let AppEvent::RefreshAccountStatus {
        thread_id,
        include_pool_usage,
        ..
    } = &request
    else {
        unreachable!();
    };
    assert_eq!(
        (Some(*thread_id), *include_pool_usage),
        (app.chat_widget.thread_id(), true)
    );
    app.handle_event(tui, server, request).await?;
    loop {
        let mut event = time::timeout(Duration::from_secs(/*secs*/ 15), events.recv())
            .await?
            .expect("footer event channel must remain open");
        if let AppEvent::AccountStatusLoaded { result, .. } = &mut event {
            let snapshot = result
                .as_mut()
                .map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
            // Countdown rendering is covered separately; keep this lifecycle snapshot timeless.
            for usage in &mut snapshot.usage {
                for window in &mut usage.windows {
                    window.resets_at = None;
                }
            }
            return Ok(event);
        }
    }
}

#[tokio::test]
async fn replacement_footer_resolves_destination_pool_and_rejects_previous_widget_reads()
-> Result<()> {
    let home = tempdir()?;
    let backend = wiremock::MockServer::start().await;
    account_pools::setup(&home, &backend)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\n\
             [tui]\nstatus_line = [\"account-weekly\", \"pool-weekly\"]\n",
            backend.uri()
        ),
    )?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let mut server = crate::start_embedded_app_server_for_picker(&config).await?;
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    app.config = config;
    app.local_settings = crate::local_settings::LocalSettings::from(&app.config);
    let mut tui = crate::tui::test_support::make_test_tui()?;

    // New-session defaults deliberately differ from this thread's explicit pool selection.
    app.manage_accounts(&server, "select b");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("account output")
    else {
        panic!("expected account selection report");
    };
    result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;

    let mut selected_config = app.config.clone();
    selected_config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
    let started = server.start_thread(&selected_config).await?;
    let pool_thread_id = started.session.thread_id;
    let snapshot = ThreadEventSnapshot {
        session: Some(started.session.clone()),
        delegated_turns: Vec::new(),
        turns: started.turns.clone(),
        events: Vec::new(),
        active_reasoning_item: None,
        input_state: None,
    };
    // /new, /clear, resume and fork all attach through this replacement path.
    app.replace_chat_widget_with_app_server_thread(
        &mut tui,
        started,
        ThreadAttachPresentation::SessionLineage,
        /*initial_user_message*/ None,
    )
    .await?;
    let event = load_footer_status(&mut app, &mut tui, &mut server, &mut events).await?;
    app.handle_event(&mut tui, &mut server, event).await?;
    let mut footers = vec![app.chat_widget.status_line_text().expect("pool footer")];

    // Agent navigation reconstructs the widget too. Hold its response across another switch.
    app.render_thread_snapshot(
        &mut tui,
        &server,
        pool_thread_id,
        snapshot.clone(),
        /*resume_restored_queue*/ false,
    )?;
    let stale = load_footer_status(&mut app, &mut tui, &mut server, &mut events).await?;
    selected_config.account_selection = Some(AccountSelection::Account("b".to_owned()));
    let started = server.start_thread(&selected_config).await?;
    app.replace_chat_widget_with_app_server_thread(
        &mut tui,
        started,
        ThreadAttachPresentation::SessionLineage,
        /*initial_user_message*/ None,
    )
    .await?;
    let current = load_footer_status(&mut app, &mut tui, &mut server, &mut events).await?;
    app.handle_event(&mut tui, &mut server, stale).await?;
    assert_eq!(app.chat_widget.status_line_text(), None);
    app.handle_event(&mut tui, &mut server, current).await?;
    footers.push(app.chat_widget.status_line_text().expect("account footer"));

    // Returning to the pool must not retain the account-only destination's selection.
    app.render_thread_snapshot(
        &mut tui,
        &server,
        pool_thread_id,
        snapshot.clone(),
        /*resume_restored_queue*/ false,
    )?;
    let event = load_footer_status(&mut app, &mut tui, &mut server, &mut events).await?;
    app.handle_event(&mut tui, &mut server, event).await?;
    footers.push(
        app.chat_widget
            .status_line_text()
            .expect("restored pool footer"),
    );
    assert_snapshot!(footers.join("\n"), @"
    a 0% · work 90%
    b 90%
    a 0% · work 90%
    ");

    *server.account_status_cache.lock().await = Default::default();
    backend.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/codex/usage"))
        .respond_with(wiremock::ResponseTemplate::new(/*s*/ 400))
        .mount(&backend)
        .await;
    app.render_thread_snapshot(
        &mut tui,
        &server,
        pool_thread_id,
        snapshot,
        /*resume_restored_queue*/ false,
    )?;
    let event = load_footer_status(&mut app, &mut tui, &mut server, &mut events).await?;
    app.handle_event(&mut tui, &mut server, event).await?;
    // Observations shared on disk outlive the TUI cache, so a backend failure inside the display
    // freshness window keeps the last known quota instead of blanking the footer.
    assert_snapshot!(
        app.chat_widget.status_line_text().expect("pool remains visible"),
        @"a 0% · work 90%"
    );
    backend.reset().await;
    while events.try_recv().is_ok() {}
    for mark_unavailable in [
        ThreadEventChannel::mark_replay_only,
        ThreadEventChannel::mark_external_writer,
    ] {
        mark_unavailable(app.ensure_thread_channel(pool_thread_id));
        let request_id = uuid::Uuid::new_v4();
        app.refresh_account_status(
            &server,
            request_id,
            pool_thread_id,
            /*include_pool_usage*/ true,
        );
        let AppEvent::AccountStatusLoaded {
            request_id: received,
            result,
        } = events
            .try_recv()
            .expect("unavailable threads fail without an RPC")
        else {
            panic!("expected account status response");
        };
        assert_eq!(
            (received, result.err()),
            (
                request_id,
                Some("Account status requires a live thread".to_owned())
            )
        );
    }
    assert!(
        backend
            .received_requests()
            .await
            .expect("recorded requests")
            .is_empty()
    );
    server.shutdown().await?;
    Ok(())
}
