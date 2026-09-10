use anyhow::Result;
use codex_features::Feature;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::Op;
use core_test_support::account_pools;
use core_test_support::responses;
use core_test_support::test_codex::test_codex;
use core_test_support::wait_for_event;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use tempfile::TempDir;
use test_case::test_case;
use wiremock::MockServer;
use wiremock::ResponseTemplate;

#[derive(Clone, Copy)]
enum Compaction {
    Manual,
    MidTurn,
}

#[test_case(Compaction::Manual; "manual")]
#[test_case(Compaction::MidTurn; "mid_turn")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_legacy_compaction_recovery_clears_account_routing(
    compaction: Compaction,
) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    let first = match compaction {
        Compaction::Manual => vec![
            responses::ev_assistant_message("before", "Remember this observation."),
            responses::ev_completed_with_tokens("first", /*total_tokens*/ 80),
        ],
        Compaction::MidTurn => vec![
            responses::ev_function_call("completed-tool", "missing_tool", "{}"),
            responses::ev_completed_with_tokens("first", /*total_tokens*/ 500),
        ],
    };
    let samples = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(first))
                .insert_header("x-codex-turn-state", "account-a-sticky"),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("after", "Continued after compaction."),
                responses::ev_completed_with_tokens("last", /*total_tokens*/ 80),
            ])),
        ],
    )
    .await;
    let compacts = responses::mount_compact_response_sequence(
        &server,
        vec![
            ResponseTemplate::new(429).set_body_json(json!({
                "error": {"type": "usage_limit_reached", "plan_type": "pro"},
            })),
            ResponseTemplate::new(200)
                .insert_header("x-codex-turn-state", "account-b-sticky")
                .set_body_json(json!({
                    "output": [{"type": "compaction", "encrypted_content": "pool-summary"}],
                })),
        ],
    )
    .await;
    let backend = server.uri();
    let test = test_codex()
        .with_home(home)
        .with_config(move |config| {
            config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
            config.chatgpt_base_url = backend;
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            let _ = config.features.disable(Feature::RemoteCompactionV2);
            if let Compaction::MidTurn = compaction {
                config.model_auto_compact_token_limit = Some(200);
            }
        })
        .build_with_auto_env(&server)
        .await?;
    test.submit_turn("Keep the same model across compaction.")
        .await?;
    if let Compaction::Manual = compaction {
        test.codex.submit(Op::Compact).await?;
        wait_for_event(&test.codex, |event| {
            matches!(event, EventMsg::TurnComplete(_))
        })
        .await;
        test.submit_turn("Continue with the compacted observation.")
            .await?;
    }
    let samples = samples.requests();
    let compacts = compacts.requests();
    assert_eq!((samples.len(), compacts.len()), (2, 2));
    assert_eq!(
        [&samples[0], &compacts[0], &compacts[1], &samples[1]]
            .map(|request| request.header("chatgpt-account-id")),
        [
            Some("workspace-a".to_owned()),
            Some("workspace-a".to_owned()),
            Some("workspace-b".to_owned()),
            Some("workspace-b".to_owned()),
        ]
    );
    assert_eq!(compacts[0].body_json(), compacts[1].body_json());
    assert_eq!(compacts[1].body_json()["model"], "gpt-5.5");
    assert_eq!(compacts[1].header("x-codex-turn-state"), None);
    if let Compaction::MidTurn = compaction {
        assert_eq!(
            compacts[0].header("x-codex-turn-state").as_deref(),
            Some("account-a-sticky")
        );
        assert_eq!(
            samples[1].header("x-codex-turn-state").as_deref(),
            Some("account-b-sticky")
        );
        assert_eq!(compacts[1].inputs_of_type("function_call_output").len(), 1);
    }
    assert!(
        samples[1]
            .input()
            .iter()
            .any(|item| item["encrypted_content"] == "pool-summary")
    );
    Ok(())
}
