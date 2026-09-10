use super::*;
use codex_protocol::account_pool::AccountSelection;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn account_pool_bootstrap_resolves_named_and_saved_accounts_without_legacy_login()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let backend = wiremock::MockServer::start().await;
    account_pools::setup(&home, &backend)
        .await
        .map_err(|error| color_eyre::eyre::eyre!("{error}"))?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\n",
            backend.uri()
        ),
    )?;
    let config = core_test_support::load_default_config_for_test(&home).await;
    assert!(!home.path().join("auth.json").exists());
    let mut explicit_config = config.clone();
    explicit_config.account_selection = Some(AccountSelection::Account("b".to_owned()));
    let mut session = crate::start_embedded_app_server_for_picker(&config).await?;
    session.start_thread(&explicit_config).await?;
    let saved_id = session.account_thread_id.clone().unwrap();
    for (selection, thread_id) in [
        (Some(AccountSelection::Account("b".to_owned())), None),
        (None, Some(saved_id)),
    ] {
        session.set_account_selection(selection, thread_id);
        let account = session.read_account().await?;
        assert!(session.managed_accounts_active);
        let Some(codex_app_server_protocol::Account::Chatgpt { email, .. }) = account.account
        else {
            panic!("named login must satisfy TUI onboarding");
        };
        assert_eq!(email.as_deref(), Some("b@example.com"));
    }
    session.shutdown().await?;
    Ok(())
}
