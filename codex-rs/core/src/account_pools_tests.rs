use crate::compact::InitialContextInjection;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::tasks::CompactTask;
use crate::tasks::SessionTask;
use anyhow::Result;
use codex_analytics::CompactionPhase;
use codex_analytics::CompactionReason;
use codex_features::Feature;
use codex_login::AuthCredentialsStoreMode;
use codex_login::CodexAuth;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::user_input::UserInput;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use test_case::test_case;
use tokio_util::sync::CancellationToken;
use wiremock::Mock;
use wiremock::MockServer;
use wiremock::ResponseTemplate;
use wiremock::matchers::method;
use wiremock::matchers::path;

#[derive(Clone, Copy)]
enum Compaction {
    ManualRemote,
    InlineRemote,
    Local,
}

#[test_case(Compaction::ManualRemote; "manual_remote")]
#[test_case(Compaction::InlineRemote; "inline_remote")]
#[test_case(Compaction::Local; "local")]
#[tokio::test]
async fn account_pool_compaction_cancellation_prevents_quota_reads_and_retry(
    compaction: Compaction,
) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    account_pools::mount_initial_available_usage(&server).await;
    let uri = server.uri();
    let config_home = Arc::clone(&home);
    let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::create_dummy_chatgpt_auth_for_testing(),
        Vec::new(),
        move |config| {
            config.codex_home =
                codex_utils_absolute_path::AbsolutePathBuf::from_absolute_path(config_home.path())
                    .expect("temporary home is absolute");
            config.chatgpt_base_url = uri.clone();
            config.model_provider.base_url = Some(format!("{uri}/v1"));
            config.model_provider.supports_websockets = false;
            config.model = Some("gpt-5.5".to_owned());
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
            let _ = config.features.disable(Feature::TokenBudget);
        },
    )
    .await;
    let pool = super::initialize(
        &turn.config,
        &session.services.auth_manager,
        /*resume_id*/ None,
    )
    .await?
    .expect("managed pool selected");
    session.services.thread_extension_data.insert(pool);
    session
        .record_conversation_items(
            &turn,
            turn.model_info(),
            &[ResponseItem::Message {
                id: None,
                role: "user".to_owned(),
                content: vec![ContentItem::InputText {
                    text: "Keep this observation.".to_owned(),
                }],
                phase: None,
                internal_chat_message_metadata_passthrough: None,
            }],
        )
        .await;
    let cancellation = CancellationToken::new();
    let request_cancellation = cancellation.clone();
    // Remote compaction streams through the Responses endpoint, like local compaction.
    let endpoint = "/v1/responses";
    // Cancel at the quota response boundary, before recovery can poll or redeem. This
    // avoids depending on the task abort grace period or scheduling a timed interrupt.
    Mock::given(method("POST"))
        .and(path(endpoint))
        .respond_with(move |_: &wiremock::Request| {
            request_cancellation.cancel();
            ResponseTemplate::new(429).set_body_json(json!({
                "error": {"type": "usage_limit_reached", "plan_type": "pro"},
            }))
        })
        .expect(1)
        .mount(&server)
        .await;
    let result = match compaction {
        Compaction::ManualRemote => Arc::new(CompactTask)
            .run(session, turn, Vec::new(), cancellation)
            .await
            .map(|_| ()),
        Compaction::InlineRemote => {
            let step = session.capture_step_context(turn, &cancellation).await?;
            let client = &mut session.services.model_client.new_session();
            crate::compact_remote_v2::run_inline_remote_auto_compact_task(
                session,
                step,
                /*fallback_step_context*/ None,
                client,
                InitialContextInjection::DoNotInject,
                CompactionReason::ContextLimit,
                CompactionPhase::MidTurn,
                &cancellation,
            )
            .await
        }
        Compaction::Local => {
            crate::compact::run_compact_task(
                session,
                turn,
                vec![UserInput::Text {
                    text: "Summarize this observation.".to_owned(),
                    text_elements: Vec::new(),
                }],
                &cancellation,
            )
            .await
        }
    };
    assert!(matches!(
        result.unwrap_err().details(),
        CodexErrorDetails::TurnAborted
    ));
    assert_eq!(
        server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .skip_while(|request| request.url.path() != endpoint)
            .map(|request| request.url.path().to_owned())
            .collect::<Vec<_>>(),
        vec![endpoint.to_owned()],
        "cancelled compaction must not start quota reads, redemption or another request",
    );
    Ok(())
}
