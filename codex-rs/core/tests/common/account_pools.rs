//! Shared account-pool fixtures for core, exec and app-server recovery tests.

use crate::load_default_config_for_test;
use crate::responses;
use anyhow::Result;
use base64::Engine;
use codex_account_pools::AccountStore;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountPool;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::header;
use wiremock::matchers::method;
use wiremock::matchers::path;

pub async fn setup(home: &TempDir, server: &MockServer) -> Result<()> {
    let mut config = load_default_config_for_test(home).await;
    config.chatgpt_base_url = server.uri();
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    let store = AccountStore::from_config(&config);
    for alias in ["a", "b"] {
        let source = TempDir::new()?;
        let claims = json!({"exp": 4_102_444_800i64, "email": format!("{alias}@example.com"), "https://api.openai.com/auth": {
            "chatgpt_user_id": format!("user-{alias}"), "chatgpt_account_id": format!("workspace-{alias}"), "chatgpt_plan_type": "pro",
        }});
        let payload =
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(serde_json::to_vec(&claims)?);
        let token = format!("e30.{payload}.c2ln");
        std::fs::write(
            source.path().join("auth.json"),
            serde_json::to_vec(&json!({
                "auth_mode": "chatgpt", "tokens": {"id_token": token, "access_token": token,
                    "refresh_token": format!("secret-{alias}"), "account_id": format!("workspace-{alias}")},
                "last_refresh": "2026-09-10T00:00:00Z",
            }))?,
        )?;
        // Avoid a refresh independent of when this fixture runs.
        let mut raw = codex_login::load_auth_dot_json(
            source.path(),
            AuthCredentialsStoreMode::File,
            config.auth_keyring_backend_kind(),
        )?
        .expect("fixture login exists");
        raw.last_refresh = Some(std::time::SystemTime::now().into());
        codex_login::save_auth(
            source.path(),
            &raw,
            AuthCredentialsStoreMode::File,
            config.auth_keyring_backend_kind(),
        )?;
        store.import(alias, source.path()).await?;
        let used = if alias == "a" { 100 } else { 10 };
        Mock::given(method("GET")).and(path("/api/codex/usage"))
            .and(header("chatgpt-account-id", format!("workspace-{alias}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "plan_type": "pro", "user_id": format!("user-{alias}"), "account_id": format!("workspace-{alias}"),
                "rate_limit": {"allowed": alias == "b", "limit_reached": alias == "a",
                    "primary_window": {"used_percent": used, "limit_window_seconds": 18000, "reset_after_seconds": 3600, "reset_at": 2_000_000_000i64},
                    "secondary_window": {"used_percent": used, "limit_window_seconds": 604800, "reset_after_seconds": 3600, "reset_at": 2_000_000_000i64}},
                "rate_limit_reset_credits": {"available_count": 0},
            }))).mount(server).await;
    }
    store
        .put_pool(
            AccountPool {
                name: "work".to_owned(),
                accounts: vec!["a".to_owned(), "b".to_owned()],
                redeem_weekly_resets: true,
            },
            /*create*/ true,
        )
        .await?;
    Mock::given(method("GET"))
        .and(path("/api/codex/models"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"models":[{"slug":"gpt-5.5"}]})),
        )
        .mount(server)
        .await;
    Ok(())
}

pub enum QuotaFailure {
    Http,
    PartialStream,
}

pub async fn mount_recovery(server: &MockServer, failure: QuotaFailure) -> responses::ResponseMock {
    let mut first = vec![
        responses::ev_response_created("first"),
        responses::ev_assistant_message("partial", "I am checking the file."),
        responses::ev_function_call(
            "pool-tool",
            "exec_command",
            &json!({
                "cmd": "echo pool-tool-once >> pool-tool-count.txt", "max_output_tokens": 100,
            })
            .to_string(),
        ),
    ];
    let mut steps = Vec::new();
    match failure {
        QuotaFailure::Http => {
            first.push(responses::ev_completed("first"));
            steps.push(responses::sse_response(responses::sse(first)));
            steps.push(ResponseTemplate::new(429).set_body_json(json!({"error": {
                "type": "usage_limit_reached", "plan_type": "pro", "message": "limit reached",
            }})));
        }
        QuotaFailure::PartialStream => {
            first.push(responses::ev_message_item_added("unfinished", ""));
            first.push(responses::ev_output_text_delta(
                "A partial observation before quota recovery.",
            ));
            first.push(json!({"type": "response.failed", "response": {"id": "first", "error": {
                "type": "usage_limit_reached", "code": "usage_limit_reached", "plan_type": "pro", "message": "limit reached",
            }}}));
            steps.push(responses::sse_response(responses::sse(first)));
        }
    }
    steps.push(responses::sse_response(responses::sse(vec![
        responses::ev_response_created("second"),
        responses::ev_assistant_message("final", "Recovered on the same model."),
        responses::ev_completed("second"),
    ])));
    responses::mount_response_sequence(server, steps).await
}

pub fn assert_recovery(requests: &responses::ResponseMock) {
    let requests = requests.requests();
    assert_eq!(
        requests
            .first()
            .expect("initial request")
            .header("chatgpt-account-id")
            .as_deref(),
        Some("workspace-a")
    );
    let last = requests.last().expect("recovered request");
    assert_eq!(
        last.header("chatgpt-account-id").as_deref(),
        Some("workspace-b")
    );
    assert_eq!(last.body_json()["model"], "gpt-5.5");
    assert_eq!(last.inputs_of_type("function_call_output").len(), 1);
    assert!(
        last.input()
            .iter()
            .any(|item| item.to_string().contains("I am checking the file."))
    );
}
