//! Capacity and transport failures must retain ordinary request recovery inside a pool.

use anyhow::Result;
use codex_account_pools::AccountStore;
use codex_core::TurnInputRequest;
use codex_login::AuthCredentialsStoreMode;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use codex_protocol::protocol::CodexErrorInfo;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
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
enum Failure {
    HttpCapacity(&'static str),
    SseCapacity(&'static str),
    WebsocketCapacity(&'static str),
    StreamDisconnected,
}

#[test_case(Failure::HttpCapacity("server_is_overloaded"); "http_overload")]
#[test_case(Failure::HttpCapacity("slow_down"); "http_slow_down")]
#[test_case(Failure::SseCapacity("server_is_overloaded"); "sse_overload")]
#[test_case(Failure::SseCapacity("slow_down"); "sse_slow_down")]
#[test_case(Failure::WebsocketCapacity("server_is_overloaded"); "websocket_overload")]
#[test_case(Failure::WebsocketCapacity("slow_down"); "websocket_slow_down")]
#[test_case(Failure::StreamDisconnected; "stream_disconnect")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_non_quota_failures_preserve_account(failure: Failure) -> Result<()> {
    let server = MockServer::start().await;
    let home = Arc::new(TempDir::new()?);
    account_pools::setup(&home, &server).await?;
    // Only the preflight reports A as available. An erroneous recovery would observe
    // A's exhausted quota and switch to B, making it visible in both events and HTTP.
    account_pools::mount_initial_available_usage(&server).await;
    let mut websocket = None;
    let response_mock = match failure {
        Failure::HttpCapacity(code) => Some(
            responses::mount_response_once(
                &server,
                ResponseTemplate::new(/*s*/ 503).set_body_json(json!({"error": {"code": code}})),
            )
            .await,
        ),
        Failure::SseCapacity(code) => Some(
            responses::mount_sse_once(
                &server,
                responses::sse_failed("capacity", code, "Backend requested a slowdown."),
            )
            .await,
        ),
        Failure::WebsocketCapacity(code) => {
            websocket = Some(
                responses::start_websocket_server(vec![vec![
                    vec![
                        responses::ev_response_created("prewarm"),
                        responses::ev_completed("prewarm"),
                    ],
                    vec![json!({"type": "error", "status": 503, "error": {"code": code}})],
                ]])
                .await,
            );
            None
        }
        Failure::StreamDisconnected => Some(
            responses::mount_response_sequence(
                &server,
                vec![
                    // EOF before response.completed is a transport failure, not quota or capacity.
                    responses::sse_response(responses::sse(vec![responses::ev_response_created(
                        "disconnected",
                    )])),
                    responses::sse_response(responses::sse(vec![responses::ev_completed(
                        "recovered",
                    )])),
                ],
            )
            .await,
        ),
    };
    let backend = server.uri();
    let websocket_url = websocket
        .as_ref()
        .map(|server| format!("{}/v1", server.uri()));
    let test = test_codex()
        .with_home(home)
        .with_config(move |config| {
            config.account_selection = Some(AccountSelection::Pool("work".to_owned()));
            config.chatgpt_base_url = backend;
            config.cli_auth_credentials_store_mode = AuthCredentialsStoreMode::File;
            config.model_provider.request_max_retries = Some(0);
            config.model_provider.stream_max_retries = Some(1);
            if let Some(url) = websocket_url {
                config.model_provider.base_url = Some(url);
                config.model_provider.supports_websockets = true;
            }
        })
        .build_with_auto_env(&server)
        .await?;
    let selected_account = AccountStore::from_config(&test.config)
        .read()?
        .accounts
        .into_iter()
        .find(|account| account.alias == "a")
        .expect("fixture account A");
    test.codex
        .start_or_steer_turn(TurnInputRequest::user_input(vec![UserInput::Text {
            text: "Keep the selected account when inference fails.".to_owned(),
            text_elements: Vec::new(),
        }]))
        .await?;
    let mut pool_events = Vec::new();
    let mut errors = Vec::new();
    let mut stream_errors = Vec::new();
    let terminal_error = loop {
        match wait_for_event(&test.codex, |_| true).await {
            EventMsg::AccountPool(event) => pool_events.push(event),
            EventMsg::Error(event) => errors.push(event.codex_error_info),
            EventMsg::StreamError(event) => stream_errors.push(event.codex_error_info),
            EventMsg::TurnComplete(event) => {
                break event.error.and_then(|error| error.codex_error_info);
            }
            _ => {}
        }
    };
    let disconnected = matches!(failure, Failure::StreamDisconnected);
    // Upstream retries `slow_down` as a rate limit, so only its terminal outcome is stable.
    let rate_limited = matches!(
        failure,
        Failure::HttpCapacity("slow_down")
            | Failure::SseCapacity("slow_down")
            | Failure::WebsocketCapacity("slow_down")
    );
    let expected = if rate_limited {
        // Once the scripted response is used up, the terminal error only reflects the mock; what
        // matters here is that it was not treated as the account's quota.
        assert!(!stream_errors.is_empty(), "slow_down should be retried");
        assert_ne!(terminal_error, Some(CodexErrorInfo::UsageLimitExceeded));
        (errors.clone(), stream_errors.clone(), errors[0].clone())
    } else if disconnected {
        (
            Vec::new(),
            vec![Some(CodexErrorInfo::ResponseStreamDisconnected {
                http_status_code: None,
            })],
            None,
        )
    } else {
        (
            vec![Some(CodexErrorInfo::ServerOverloaded)],
            Vec::new(),
            Some(CodexErrorInfo::ServerOverloaded),
        )
    };
    assert_eq!((errors, stream_errors, terminal_error), expected);
    assert_eq!(
        pool_events,
        vec![AccountPoolEvent::Selected {
            account: selected_account,
        }]
    );
    if let Some(response_mock) = response_mock
        && !rate_limited
    {
        assert_eq!(
            response_mock
                .requests()
                .iter()
                .map(|request| request.header("chatgpt-account-id"))
                .collect::<Vec<_>>(),
            vec![Some("workspace-a".to_owned()); if disconnected { 2 } else { 1 }]
        );
    }
    let http_requests = server.received_requests().await.expect("recorded requests");
    assert_eq!(
        http_requests
            .iter()
            .filter(|request| request.url.path() == "/api/codex/usage")
            .map(|request| {
                request
                    .headers
                    .get("chatgpt-account-id")
                    .expect("request should identify the selected account")
                    .to_str()
                    .expect("account header should contain valid text")
            })
            .collect::<Vec<_>>(),
        vec!["workspace-a"]
    );
    assert!(
        http_requests
            .iter()
            .all(|request| { request.method != "POST" || request.url.path() == "/v1/responses" })
    );
    // Retries of any kind stay on the selected account.
    assert!(
        http_requests
            .iter()
            .filter(|request| request.url.path() == "/v1/responses")
            .all(|request| request
                .headers
                .get("chatgpt-account-id")
                .is_some_and(|value| value == "workspace-a"))
    );
    if let Some(websocket) = websocket {
        if rate_limited {
            assert!(
                websocket
                    .handshakes()
                    .iter()
                    .all(|request| request.header("chatgpt-account-id").as_deref()
                        == Some("workspace-a"))
            );
            websocket.shutdown().await;
            return Ok(());
        }
        assert_eq!(
            websocket
                .connections()
                .iter()
                .map(Vec::len)
                .collect::<Vec<_>>(),
            vec![2]
        );
        assert_eq!(
            websocket
                .handshakes()
                .iter()
                .map(|request| request.header("chatgpt-account-id"))
                .collect::<Vec<_>>(),
            vec![Some("workspace-a".to_owned())]
        );
        websocket.shutdown().await;
    }
    Ok(())
}
