use super::*;
use codex_protocol::account_pool::AccountSelection;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn account_selection_refreshes_all_quota_and_pool_list_renders_server_data() -> Result<()> {
    let home = tempfile::tempdir()?;
    let backend = wiremock::MockServer::start().await;
    account_pools::setup(&home, &backend)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\n",
            backend.uri()
        ),
    )?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    let mut session = crate::start_embedded_app_server_for_picker(&config).await?;
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    for (command, label) in [
        ("", "current login"),
        ("select b", "account b"),
        ("clear", "current login"),
    ] {
        app.manage_accounts(&session, command);
        let AppEvent::ManagedAccountOutput { result } =
            events.recv().await.expect("account output")
        else {
            panic!("expected account report");
        };
        let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
        let quota: Vec<_> = report
            .markdown
            .lines()
            .filter(|line| line.starts_with("| a |") || line.starts_with("| b |"))
            .map(|row| {
                let columns: Vec<_> = row.split('|').map(str::trim).collect();
                (columns[1], columns[4], columns[6])
            })
            .collect();
        assert_eq!(quota, [("a", "0%", "0"), ("b", "90%", "0")]);
        assert!(
            report
                .markdown
                .contains(&format!("Default for new sessions: {label}"))
        );
    }
    let requests = backend.received_requests().await.expect("usage requests");
    let mut identities: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/codex/usage")
        .map(|request| request.headers["chatgpt-account-id"].to_str().unwrap())
        .collect();
    // Each report reads its accounts concurrently, so only each report's set is ordered.
    for report_reads in identities.chunks_mut(/*chunk_size*/ 2) {
        report_reads.sort_unstable();
    }
    assert_eq!(identities, ["workspace-a", "workspace-b"].repeat(/*n*/ 3));
    app.manage_pools(&session, "");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("pool output")
    else {
        panic!("expected pool report");
    };
    let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    assert!(report.markdown.contains("| work | a\\, b | Enabled |"));

    let account = codex_protocol::account_pool::ManagedAccount {
        alias: "b".to_owned(),
        user_id: "user-b".to_owned(),
        workspace_id: "workspace-b".to_owned(),
        email: Some("b@example.com".to_owned()),
        plan: Some("pro".to_owned()),
    };
    let mut selected_config = config.clone();
    selected_config.account_selection = Some(AccountSelection::Account("b".to_owned()));
    let thread_id = session
        .start_thread(&selected_config)
        .await?
        .session
        .thread_id;
    let request_id = uuid::Uuid::new_v4();
    app.refresh_account_status(
        &session, request_id, thread_id, /*include_pool_usage*/ false,
    );
    let AppEvent::AccountStatusLoaded {
        request_id: received_id,
        result,
    } = events.recv().await.expect("footer usage")
    else {
        panic!("expected selected account footer usage");
    };
    let usage = result
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?
        .usage
        .pop()
        .expect("account usage");
    assert_eq!(
        (
            received_id,
            usage.account.clone(),
            usage
                .windows
                .iter()
                .map(|window| window.remaining_percent)
                .collect::<Vec<_>>()
        ),
        (request_id, account, vec![90.0, 90.0])
    );
    let mut identities: Vec<_> = backend
        .received_requests()
        .await
        .unwrap()
        .into_iter()
        .filter(|request| request.url.path() == "/api/codex/usage")
        .map(|request| {
            request.headers["chatgpt-account-id"]
                .to_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    for report_reads in identities.chunks_mut(/*chunk_size*/ 2) {
        report_reads.sort_unstable();
    }
    // The reports above just observed both accounts, so the footer reuses the shared observation
    // instead of asking the backend again.
    assert_eq!(identities, ["workspace-a", "workspace-b"].repeat(/*n*/ 3));

    selected_config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
    let thread_id = session
        .start_thread(&selected_config)
        .await?
        .session
        .thread_id;
    app.refresh_account_status(
        &session,
        uuid::Uuid::new_v4(),
        thread_id,
        /*include_pool_usage*/ true,
    );
    let AppEvent::AccountStatusLoaded { result, .. } =
        events.recv().await.expect("pool footer usage")
    else {
        panic!("expected pool footer usage");
    };
    let mut aliases: Vec<_> = result
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?
        .usage
        .into_iter()
        .map(|usage| usage.account.alias)
        .collect();
    aliases.sort();
    assert_eq!(aliases, ["a", "b"]);

    // A successful selection must survive unavailable usage and show its error explicitly.
    backend.reset().await;
    wiremock::Mock::given(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/codex/usage"))
        .respond_with(wiremock::ResponseTemplate::new(/*s*/ 400))
        .mount(&backend)
        .await;
    app.manage_accounts(&session, "select b");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("account output")
    else {
        panic!("expected account report");
    };
    let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    assert!(
        report
            .markdown
            .contains("Default for new sessions: account b")
    );
    assert!(
        report
            .markdown
            .contains("| b | b\\@example\\.com | pro | Unknown | Unknown | Unknown |")
    );
    assert_eq!(report.markdown.matches("Usage unavailable:").count(), 2);
    session.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn manual_account_usage_and_redemption_refresh_inactive_footer_members() -> Result<()> {
    use wiremock::Mock;
    use wiremock::ResponseTemplate;
    use wiremock::matchers::header;
    use wiremock::matchers::method;
    use wiremock::matchers::path;

    let home = tempfile::tempdir()?;
    let backend = wiremock::MockServer::start().await;
    account_pools::setup(&home, &backend)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "cli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\n",
            backend.uri()
        ),
    )?;
    let mut config = core_test_support::load_default_config_for_test(&home).await;
    config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
    let redeemed = Arc::new(AtomicBool::new(false));
    let observed = Arc::clone(&redeemed);
    Mock::given(method("GET")).and(path("/api/codex/usage"))
        .and(header("chatgpt-account-id", "workspace-a"))
        .respond_with(move |_: &wiremock::Request| {
            let recovered = observed.load(Ordering::Relaxed);
            let used = if recovered { 10 } else { 100 };
            ResponseTemplate::new(/*s*/ 200).set_body_json(serde_json::json!({
                "plan_type": "pro", "user_id": "user-a", "account_id": "workspace-a",
                "rate_limit": {"allowed": recovered, "limit_reached": !recovered,
                    "primary_window": {"used_percent": used, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": 2_000_000_000i64},
                    "secondary_window": {"used_percent": used, "limit_window_seconds": 604800, "reset_after_seconds": 3600, "reset_at": 2_000_000_000i64}},
                "rate_limit_reset_credits": {"available_count": if recovered { 0 } else { 1 }},
            }))
        }).with_priority(/*p*/ 1).mount(&backend).await;
    Mock::given(method("GET")).and(path("/api/codex/rate-limit-reset-credits"))
        .respond_with(ResponseTemplate::new(/*s*/ 200).set_body_json(serde_json::json!({
            "available_count": 1, "credits": [{"id": "credit", "reset_type": "codex_rate_limits", "status": "available", "granted_at": "2026-01-01T00:00:00Z"}]
        }))).mount(&backend).await;
    Mock::given(method("POST"))
        .and(path("/api/codex/rate-limit-reset-credits/consume"))
        .respond_with(move |_: &wiremock::Request| {
            redeemed.store(/*val*/ true, Ordering::Relaxed);
            ResponseTemplate::new(/*s*/ 200).set_body_json(serde_json::json!({"code": "reset"}))
        })
        .expect(/*n*/ 1)
        .mount(&backend)
        .await;

    let mut session = crate::start_embedded_app_server_for_picker(&config).await?;
    let (mut app, mut events, _ops) = make_test_app_with_channels().await;
    app.manage_pools(&session, "update work b a");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("pool report")
    else {
        panic!("expected pool report");
    };
    result.map_err(color_eyre::eyre::Report::msg)?;
    let thread_id = session.start_thread(&config).await?.session.thread_id;
    let handle = session.request_handle();
    for (command, remaining) in [
        (None, 0.0),
        (Some("usage a"), 0.0),
        (Some("redeem a"), 90.0),
    ] {
        if let Some(command) = command {
            app.manage_accounts(&session, command);
            let AppEvent::ManagedAccountOutput { result } =
                time::timeout(Duration::from_secs(/*secs*/ 15), events.recv())
                    .await?
                    .expect("manual account report")
            else {
                panic!("expected manual account report");
            };
            result.map_err(color_eyre::eyre::Report::msg)?;
        }
        let before = backend.received_requests().await.expect("requests").len();
        let snapshot = crate::app_server_session::account_status::load(
            &handle,
            &session.account_status_cache,
            &session.account_status_changed,
            &session.account_status_invalidations,
            thread_id,
            /*include_pool_usage*/ true,
        )
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
        assert_eq!(
            snapshot
                .account
                .as_ref()
                .map(|account| account.alias.as_str()),
            Some("b")
        );
        let a = snapshot
            .usage
            .iter()
            .find(|usage| usage.account.alias == "a")
            .expect("inactive quota");
        assert_eq!(
            a.windows
                .iter()
                .map(|window| window.remaining_percent)
                .collect::<Vec<_>>(),
            vec![remaining, remaining]
        );
        let requests = backend.received_requests().await.expect("requests");
        let mut refreshed: Vec<_> = requests[before..]
            .iter()
            .filter(|request| request.url.path() == "/api/codex/usage")
            .map(|request| {
                (
                    request.url.path(),
                    request.headers["chatgpt-account-id"]
                        .to_str()
                        .expect("account header"),
                )
            })
            .collect();
        refreshed.sort();
        // A manual command stores the observation that the invalidated footer then displays, so
        // only the first load reaches the backend.
        assert_eq!(
            refreshed,
            if command.is_none() {
                vec![
                    ("/api/codex/usage", "workspace-a"),
                    ("/api/codex/usage", "workspace-b"),
                ]
            } else {
                Vec::new()
            }
        );
    }
    drop(handle);
    session.shutdown().await?;
    Ok(())
}
