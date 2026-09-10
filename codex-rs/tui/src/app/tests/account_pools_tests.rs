use super::*;
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
    let session = crate::start_embedded_app_server_for_picker(&config).await?;
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
    let identities: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/codex/usage")
        .map(|request| request.headers["chatgpt-account-id"].to_str().unwrap())
        .collect();
    assert_eq!(identities, ["workspace-a", "workspace-b"].repeat(/*n*/ 3));
    app.manage_pools(&session, "");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("pool output")
    else {
        panic!("expected pool report");
    };
    let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    assert!(report.markdown.contains("| work | a\\, b | Enabled |"));

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
