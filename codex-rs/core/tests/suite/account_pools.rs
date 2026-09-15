use anyhow::Result;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountSelection;
use core_test_support::account_pools;
use core_test_support::account_pools::QuotaFailure;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use tempfile::TempDir;
use wiremock::MockServer;

#[path = "account_pool_continuity_tests.rs"]
mod continuity;

async fn recovery_preserves_completed_tools(failure: QuotaFailure) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    let partial = matches!(failure, QuotaFailure::PartialStream);
    let requests = account_pools::mount_recovery(&server, failure).await;
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
    test.codex
        .start_or_steer_turn(
            codex_core::TurnInputRequest::user_input(vec![
                codex_protocol::user_input::UserInput::Text {
                    text: "Check once and finish.".to_owned(),
                    text_elements: Vec::new(),
                },
            ])
            .with_thread_settings(codex_protocol::protocol::ThreadSettingsOverrides {
                approval_policy: Some(codex_protocol::protocol::AskForApproval::Never),
                permission_profile: Some(codex_protocol::models::PermissionProfile::Disabled),
                ..Default::default()
            }),
        )
        .await?;
    let mut observed = Vec::new();
    loop {
        let event =
            tokio::time::timeout(std::time::Duration::from_secs(15), test.codex.next_event()).await;
        let event = match event {
            Ok(event) => event?,
            Err(_) => anyhow::bail!(
                "Recovery stalled; events={observed:?}; HTTP paths={:?}",
                server
                    .received_requests()
                    .await
                    .expect("request recording enabled")
                    .iter()
                    .map(|request| (request.method.as_str(), request.url.path()))
                    .collect::<Vec<_>>()
            ),
        };
        let json = serde_json::to_value(&event.msg)?;
        observed.push(
            if matches!(
                event.msg,
                codex_protocol::protocol::EventMsg::AccountPool(_)
                    | codex_protocol::protocol::EventMsg::Error(_)
            ) {
                json.to_string()
            } else {
                json["type"].to_string()
            },
        );
        if matches!(
            event.msg,
            codex_protocol::protocol::EventMsg::TurnComplete(_)
        ) {
            break;
        }
    }
    account_pools::assert_recovery(&requests);
    if partial {
        assert!(
            requests
                .requests()
                .last()
                .expect("recovered request")
                .input()
                .iter()
                .any(|item| item
                    .to_string()
                    .contains("A partial observation before quota recovery."))
        );
    }
    assert_eq!(
        test.fs()
            .read_file_text(
                &test.workspace_path_uri("pool-tool-count.txt")?,
                Default::default(),
                /*sandbox*/ None
            )
            .await?
            .matches("pool-tool-once")
            .count(),
        1
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_http_quota_recovery_keeps_completed_tool_results() -> Result<()> {
    recovery_preserves_completed_tools(QuotaFailure::Http).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_partial_stream_recovery_settles_tools_before_switching() -> Result<()> {
    recovery_preserves_completed_tools(QuotaFailure::PartialStream).await
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_remote_compaction_recovers_on_the_same_model() -> Result<()> {
    use codex_protocol::protocol::EventMsg;
    use codex_protocol::protocol::Op;
    use core_test_support::responses;
    use core_test_support::wait_for_event;
    use serde_json::json;
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    account_pools::mount_initial_available_usage(&server).await;
    let requests = responses::mount_response_sequence(&server, vec![
        responses::sse_response(responses::sse(vec![responses::ev_completed("before")])),
        wiremock::ResponseTemplate::new(429).set_body_json(json!({"error":{"type":"usage_limit_reached","plan_type":"pro"}})),
        responses::sse_response(responses::sse(vec![json!({"type":"response.output_item.done","item":{"type":"compaction","encrypted_content":"pool-summary"}}), responses::ev_completed("compact")])),
        responses::sse_response(responses::sse(vec![responses::ev_completed("after")])),
    ]).await;
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
    test.submit_turn("Remember this while compacting.").await?;
    test.codex.submit(Op::Compact).await?;
    wait_for_event(&test.codex, |event| {
        matches!(event, EventMsg::TurnComplete(_))
    })
    .await;
    test.submit_turn("Continue after compaction.").await?;
    let requests = requests.requests();
    assert_eq!(requests.len(), 4);
    assert_eq!(
        requests
            .iter()
            .map(|r| r.header("chatgpt-account-id"))
            .collect::<Vec<_>>(),
        vec![
            Some("workspace-a".to_owned()),
            Some("workspace-a".to_owned()),
            Some("workspace-b".to_owned()),
            Some("workspace-b".to_owned())
        ]
    );
    assert_eq!(
        requests[1].body_json()["model"],
        requests[2].body_json()["model"]
    );
    assert!(
        requests[3]
            .input()
            .iter()
            .any(|item| item["encrypted_content"] == "pool-summary")
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_websocket_recovery_clears_previous_response_and_sticky_headers() -> Result<()>
{
    use core_test_support::responses;
    use serde_json::json;
    let usage_server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &usage_server).await?;
    account_pools::mount_initial_available_usage(&usage_server).await;
    let websocket = responses::start_websocket_server_with_headers(vec![
        responses::WebSocketConnectionConfig {
            requests: vec![vec![responses::ev_response_created("prewarm"), responses::ev_completed("prewarm")],
                vec![json!({"type":"error","status":429,"error":{"type":"usage_limit_reached","plan_type":"pro"}})]],
            response_headers: vec![("x-codex-turn-state".to_owned(), "account-a-sticky".to_owned())],
            accept_delay: None, close_after_requests: true,
        },
        responses::WebSocketConnectionConfig {
            requests: vec![vec![responses::ev_response_created("recovered"), responses::ev_assistant_message("answer", "Done."), responses::ev_completed("recovered")]],
            response_headers: Vec::new(), accept_delay: None, close_after_requests: true,
        },
    ]).await;
    let backend = usage_server.uri();
    let websocket_url = format!("{}/v1", websocket.uri());
    let test = test_codex()
        .with_home(home)
        .with_config(move |config| {
            config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
            config.chatgpt_base_url = backend;
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.model_provider.base_url = Some(websocket_url);
            config.model_provider.supports_websockets = true;
        })
        .build_with_auto_env(&usage_server)
        .await?;
    tokio::time::timeout(
        std::time::Duration::from_secs(20),
        test.submit_turn("Retain this request across account recovery."),
    )
    .await??;
    let handshakes = websocket.handshakes();
    assert_eq!(
        handshakes
            .iter()
            .map(|h| h.header("chatgpt-account-id"))
            .collect::<Vec<_>>(),
        vec![
            Some("workspace-a".to_owned()),
            Some("workspace-b".to_owned())
        ]
    );
    assert_eq!(handshakes[1].header("x-codex-turn-state"), None);
    let connections = websocket.connections();
    let recovered = connections[1][0].body_json();
    assert!(
        recovered
            .get("previous_response_id")
            .is_none_or(serde_json::Value::is_null)
    );
    assert_eq!(recovered["model"], "gpt-5.5");
    assert!(
        recovered["input"]
            .to_string()
            .contains("Retain this request across account recovery.")
    );
    websocket.shutdown().await;
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_defaults_preserve_other_auth_and_explicit_selection_wins() -> Result<()> {
    use codex_account_pools::AccountStore;
    use codex_login::CodexAuth;
    use core_test_support::responses;
    for (legacy_chatgpt, explicit, provider_bearer, expected) in [
        (true, false, false, Some("workspace-b")),
        (false, false, false, None),
        (false, true, false, Some("workspace-b")),
        (true, false, true, None),
    ] {
        let backend = MockServer::start().await;
        let home = Arc::new(TempDir::new()?);
        account_pools::setup(&home, &backend).await?;
        let mut storage_config = core_test_support::load_default_config_for_test(&home).await;
        storage_config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
        AccountStore::from_config(&storage_config)
            .select_default(Some(AccountSelection::Pool("work".to_owned())))
            .await?;
        let requests = responses::mount_sse_once(
            &backend,
            responses::sse(vec![responses::ev_completed("done")]),
        )
        .await;
        let url = backend.uri();
        let test = test_codex()
            .with_home(home)
            .with_auth(if legacy_chatgpt {
                CodexAuth::create_dummy_chatgpt_auth_for_testing()
            } else {
                CodexAuth::from_api_key("legacy-key")
            })
            .with_config(move |config| {
                config.chatgpt_base_url = url;
                config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
                if explicit {
                    config.account_selection = Some(AccountSelection::Account("b".to_owned()));
                }
                if provider_bearer {
                    config.model_provider.experimental_bearer_token = Some("provider-token".into());
                }
            })
            .build_with_auto_env(&backend)
            .await?;
        test.submit_turn("Use the configured credential authority.")
            .await?;
        let request = requests.single_request();
        assert_eq!(request.header("chatgpt-account-id").as_deref(), expected);
        if provider_bearer {
            assert_eq!(
                request.header("authorization").as_deref(),
                Some("Bearer provider-token")
            );
        } else if !legacy_chatgpt && !explicit {
            assert_eq!(
                request.header("authorization").as_deref(),
                Some("Bearer legacy-key")
            );
        }
        assert_eq!(
            backend
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|request| request.url.path() == "/api/codex/usage"),
            !provider_bearer && (legacy_chatgpt || explicit),
        );
    }
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_resume_precedence_uses_saved_auth_before_legacy_or_default() -> Result<()> {
    use codex_account_pools::AccountStore;
    use codex_login::CodexAuth;
    use core_test_support::responses;
    let backend = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &backend).await?;
    account_pools::mount_initial_available_usage(&backend).await;
    let requests = responses::mount_sse_sequence(
        &backend,
        vec![responses::sse(vec![responses::ev_completed("done")]); 3],
    )
    .await;
    let url = backend.uri();
    let first = test_codex()
        .with_home(home.clone())
        .with_config(move |config| {
            config.chatgpt_base_url = url;
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.account_selection = Some(AccountSelection::Account("b".to_owned()));
        })
        .build_with_auto_env(&backend)
        .await?;
    first.submit_turn("Remember account B.").await?;
    let path = first.codex.rollout_path().unwrap();
    first.codex.shutdown_and_wait().await?;
    let mut config = core_test_support::load_default_config_for_test(&home).await;
    config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
    AccountStore::from_config(&config)
        .select_default(Some(AccountSelection::Account("a".to_owned())))
        .await?;
    for explicit in [false, true] {
        let url = backend.uri();
        let resumed = test_codex()
            .with_auth(CodexAuth::from_api_key("legacy-key"))
            .with_config(move |config| {
                config.chatgpt_base_url = url;
                config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
                if explicit {
                    config.account_selection = Some(AccountSelection::Account("a".to_owned()));
                }
            })
            .resume(&backend, home.clone(), path.clone())
            .await?;
        resumed
            .submit_turn("Continue with the selected account.")
            .await?;
        let requests = requests.requests();
        assert_eq!(
            requests
                .last()
                .unwrap()
                .header("chatgpt-account-id")
                .as_deref(),
            Some(if explicit {
                "workspace-a"
            } else {
                "workspace-b"
            })
        );
        resumed.codex.shutdown_and_wait().await?;
    }
    Ok(())
}
