use super::*;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn account_list_fetches_quota_and_pool_list_renders_server_data() -> Result<()> {
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
    app.manage_accounts(&session, "");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("account output")
    else {
        panic!("expected account report");
    };
    let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    let percentages: Vec<_> = report
        .markdown
        .lines()
        .filter(|line| line.starts_with("| a |") || line.starts_with("| b |"))
        .map(|row| row.split('|').nth(4).unwrap().trim())
        .collect();
    assert_eq!(percentages, ["0%", "90%"]);
    let requests = backend.received_requests().await.expect("usage requests");
    let identities: Vec<_> = requests
        .iter()
        .filter(|request| request.url.path() == "/api/codex/usage")
        .map(|request| request.headers["chatgpt-account-id"].to_str().unwrap())
        .collect();
    assert_eq!(identities, ["workspace-a", "workspace-b"]);
    app.manage_pools(&session, "");
    let AppEvent::ManagedAccountOutput { result } = events.recv().await.expect("pool output")
    else {
        panic!("expected pool report");
    };
    let report = result.map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    assert!(report.markdown.contains("| work | a\\, b | Enabled |"));
    session.shutdown().await?;
    Ok(())
}
