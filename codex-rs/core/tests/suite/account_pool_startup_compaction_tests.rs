//! Managed child catalogs and terminal errors from local compaction.

use anyhow::Result;
use codex_core::StartThreadOptions;
use codex_core::TurnInputRequest;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::openai_models::ModelsResponse;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::SubAgentSource;
use codex_protocol::user_input::UserInput;
use core_test_support::account_pools;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_child_fetches_selected_account_model_metadata() -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    let backend = server.uri();
    let test = test_codex()
        .with_home(home)
        .with_config(move |config| {
            config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
            config.chatgpt_base_url = backend;
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        })
        .build_with_auto_env(&server)
        .await?;
    let mut model = codex_models_manager::bundled_models_response()?
        .models
        .into_iter()
        .find(|model| model.slug == "gpt-5.5")
        .expect("bundled model");
    model.slug = "managed-catalog-model".to_owned();
    model
        .model_messages
        .as_mut()
        .expect("model messages")
        .instructions_template =
        Some("Instructions from the managed account's remote catalog.".to_owned());
    let models = responses::mount_models_once(
        &server,
        ModelsResponse {
            models: vec![model],
        },
    )
    .await;
    let requests = responses::mount_sse_once(
        &server,
        responses::sse(vec![responses::ev_completed("child-done")]),
    )
    .await;
    let mut child_config = test.config.clone();
    child_config.model = Some("managed-catalog-model".to_owned());
    let child = test
        .thread_manager
        .start_thread(StartThreadOptions {
            session_source: Some(SessionSource::SubAgent(SubAgentSource::Review)),
            environments: Some(vec![test.executor_environment().selection().clone()]),
            ..StartThreadOptions::new(child_config)
        })
        .await?;
    child
        .thread
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Use the selected account's model metadata.".to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    wait_for_event(&child.thread, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    let request = requests.single_request();
    assert_eq!(
        (
            models.requests().len(),
            request.header("chatgpt-account-id"),
            request.body_json()["instructions"].clone()
        ),
        (
            1,
            Some("workspace-a".to_owned()),
            json!("Instructions from the managed account's remote catalog.")
        )
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_compaction_reports_terminal_quota_error_once() -> Result<()> {
    let server = MockServer::start().await;
    let requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(vec![responses::ev_completed("before")])),
            ResponseTemplate::new(/*s*/ 429).set_body_json(json!({"error": {
                "type": "usage_limit_reached", "plan_type": "pro"
            }})),
        ],
    )
    .await;
    let test = test_codex()
        .with_config(|config| {
            config.model_provider.name = "Local Responses".to_owned();
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(0);
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep this observation for compaction.")
        .await?;
    test.codex.submit(Op::Compact).await?;
    let mut errors = Vec::new();
    wait_for_event(&test.codex, |event| {
        if let EventMsg::Error(error) = event {
            errors.push(error.codex_error_info.clone());
        }
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    assert_eq!(
        (requests.requests().len(), errors),
        (2, vec![Some(CodexErrorInfo::UsageLimitExceeded)])
    );
    Ok(())
}
