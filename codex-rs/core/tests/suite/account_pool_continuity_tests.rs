//! First-request quota checks and successful-account persistence.

use super::*;
use core_test_support::responses;
use pretty_assertions::assert_eq;
use serde_json::json;
use wiremock::Mock;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_preflight_skips_exhausted_account_but_keeps_unknown_or_available()
-> Result<()> {
    for (availability, expected) in [("exhausted", "b"), ("unknown", "a"), ("available", "a")] {
        let server = MockServer::start().await;
        let home = Arc::new(TempDir::new()?);
        account_pools::setup(&home, &server).await?;
        if availability == "unknown" {
            Mock::given(method("GET"))
                .and(path("/api/codex/usage"))
                .respond_with(ResponseTemplate::new(/*s*/ 503))
                .with_priority(/*p*/ 1)
                .mount(&server)
                .await;
        } else if availability == "available" {
            account_pools::mount_initial_available_usage(&server).await;
        }
        let response = responses::sse(vec![
            responses::ev_assistant_message("answer", "Done."),
            responses::ev_completed("done"),
        ]);
        let requests =
            responses::mount_sse_sequence(&server, vec![response.clone(), response]).await;
        let url = server.uri();
        let test = test_codex()
            .with_home(home.clone())
            .with_config(move |config| {
                config.account_selection = Some(AccountSelection::Pool("work".into()));
                config.chatgpt_base_url = url;
                config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            })
            .build_with_auto_env(&server)
            .await?;
        test.submit_turn("Start without a rejected request.")
            .await?;
        let request = requests.single_request();
        assert_eq!(
            request.header("chatgpt-account-id"),
            Some(format!("workspace-{expected}"))
        );
        let preference: serde_json::Value = serde_json::from_slice(&std::fs::read(
            home.path().join("accounts/pool-state.json"),
        )?)?;
        assert_eq!(preference["work"]["alias"], json!(expected));
        let reads = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|request| request.url.path() == "/api/codex/usage")
            .count();
        test.submit_turn("Keep the same account.").await?;
        assert_eq!(requests.requests().len(), 2);
        assert_eq!(
            server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .filter(|request| request.url.path() == "/api/codex/usage")
                .count(),
            reads
        );
    }
    Ok(())
}
