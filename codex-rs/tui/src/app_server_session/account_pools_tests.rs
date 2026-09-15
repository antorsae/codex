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
    for (selection, thread_id, alias) in [
        (Some(AccountSelection::Account("b".to_owned())), None, "b"),
        (None, Some(saved_id), "b"),
        (Some(AccountSelection::Pool("work".to_owned())), None, "a"),
    ] {
        session.set_account_selection(selection, thread_id);
        let account = session.read_account().await?;
        assert!(session.managed_accounts_active);
        let Some(codex_app_server_protocol::Account::Chatgpt { email, .. }) = account.account
        else {
            panic!("named login must satisfy TUI onboarding");
        };
        assert_eq!(email, Some(format!("{alias}@example.com")));
        assert_eq!(
            session.selected_managed_account,
            Some(codex_protocol::account_pool::ManagedAccount {
                alias: alias.to_owned(),
                user_id: format!("user-{alias}"),
                workspace_id: format!("workspace-{alias}"),
                email: Some(format!("{alias}@example.com")),
                plan: Some("pro".to_owned()),
            })
        );
    }
    session.shutdown().await?;
    Ok(())
}
