use anyhow::Result;
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
async fn account_pool_remote_compaction_recovery_clears_account_routing(
    compaction: Compaction,
) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    account_pools::mount_initial_available_usage(&server).await;
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
    // Remote compaction streams through the Responses endpoint, so samples and compaction
    // requests arrive in one ordered sequence: sample, rejected compaction, compaction on the
    // next account, sample.
    let requests = responses::mount_response_sequence(
        &server,
        vec![
            responses::sse_response(responses::sse(first))
                .insert_header("x-codex-turn-state", "account-a-sticky"),
            ResponseTemplate::new(429).set_body_json(json!({
                "error": {"type": "usage_limit_reached", "plan_type": "pro"},
            })),
            responses::sse_response(responses::sse(vec![
                json!({
                    "type": "response.output_item.done",
                    "item": {"type": "compaction", "encrypted_content": "pool-summary"},
                }),
                responses::ev_completed("compact"),
            ]))
            .insert_header("x-codex-turn-state", "account-b-sticky"),
            responses::sse_response(responses::sse(vec![
                responses::ev_assistant_message("after", "Continued after compaction."),
                responses::ev_completed_with_tokens("last", /*total_tokens*/ 80),
            ])),
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
    let requests = requests.requests();
    assert_eq!(requests.len(), 4);
    let [
        sample_before,
        rejected_compact,
        recovered_compact,
        sample_after,
    ] = &requests[..]
    else {
        unreachable!("four requests were asserted above");
    };
    assert_eq!(
        [
            sample_before,
            rejected_compact,
            recovered_compact,
            sample_after
        ]
        .map(|request| request.header("chatgpt-account-id")),
        [
            Some("workspace-a".to_owned()),
            Some("workspace-a".to_owned()),
            Some("workspace-b".to_owned()),
            Some("workspace-b".to_owned()),
        ]
    );
    // The rejected compaction is replayed unchanged on the next account, on the same model.
    assert_eq!(
        rejected_compact.body_json()["input"],
        recovered_compact.body_json()["input"]
    );
    assert_eq!(recovered_compact.body_json()["model"], "gpt-5.5");
    // Sticky routing learned on the rejected account never follows the session to the next one.
    assert_eq!(recovered_compact.header("x-codex-turn-state"), None);
    if let Compaction::MidTurn = compaction {
        assert_eq!(
            rejected_compact.header("x-codex-turn-state").as_deref(),
            Some("account-a-sticky")
        );
        assert_eq!(
            recovered_compact
                .inputs_of_type("function_call_output")
                .len(),
            1
        );
        // The turn continues with the routing state the next account's compaction returned.
        assert_eq!(
            sample_after.header("x-codex-turn-state").as_deref(),
            Some("account-b-sticky")
        );
    }
    assert!(
        sample_after
            .input()
            .iter()
            .any(|item| item["encrypted_content"] == "pool-summary")
    );
    Ok(())
}
