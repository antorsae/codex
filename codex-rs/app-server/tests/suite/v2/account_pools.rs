use anyhow::Result;
use app_test_support::TestAppServer;
use codex_app_server_protocol::ManagedAccountResponse;
use codex_app_server_protocol::ThreadAccountPoolNotification;
use codex_app_server_protocol::ThreadStartParams;
use codex_app_server_protocol::TurnStartParams;
use codex_app_server_protocol::TurnStatus;
use codex_app_server_protocol::UserInput;
use codex_protocol::account_pool::AccountPoolEvent;
use codex_protocol::account_pool::AccountSelection;
use core_test_support::account_pools;
use core_test_support::account_pools::QuotaFailure;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_management_and_partial_stream_recovery() -> Result<()> {
    let backend = MockServer::start().await;
    let home = TempDir::new()?;
    account_pools::setup(&home, &backend).await?;
    let requests = account_pools::mount_recovery(&backend, QuotaFailure::PartialStream).await;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\nopenai_base_url = {:?}\n",
            backend.uri(),
            format!("{}/v1", backend.uri())
        ),
    )?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let request = server
        .send_request("account/manage", Some(json!({"action":"list", "limit":1})))
        .await?;
    let first: ManagedAccountResponse = server.read_response(request).await?;
    assert_eq!(
        first
            .data
            .iter()
            .map(|account| account.alias.as_str())
            .collect::<Vec<_>>(),
        vec!["a"]
    );
    assert_eq!(first.next_cursor.as_deref(), Some("a"));
    let request = server
        .send_request(
            "account/manage",
            Some(json!({"action":"list", "cursor":first.next_cursor})),
        )
        .await?;
    let second: ManagedAccountResponse = server.read_response(request).await?;
    assert_eq!(
        second
            .data
            .iter()
            .map(|account| account.alias.as_str())
            .collect::<Vec<_>>(),
        vec!["b"]
    );
    assert!(!serde_json::to_string(&second)?.contains("secret-"));
    let request = server
        .send_request(
            "account/manage",
            Some(json!({"action":"resolve", "accountSelection":{"type":"account","name":"b"}})),
        )
        .await?;
    let resolved: ManagedAccountResponse = server.read_response(request).await?;
    assert_eq!(resolved.selected_account, second.data.first().cloned());
    assert!(!serde_json::to_string(&resolved)?.contains("secret-"));
    assert!(matches!(
        resolved.resolved.unwrap().account,
        Some(codex_app_server_protocol::Account::Chatgpt { .. })
    ));
    let request = server
        .send_request(
            "pool/manage",
            Some(json!({"action":"create", "limit":0,
        "pool":{"name":"invalid-page","accounts":["a"],"redeemWeeklyResets":true}})),
        )
        .await?;
    let error = server
        .read_stream_until_error_message(codex_app_server_protocol::RequestId::Integer(request))
        .await?;
    assert_eq!(error.error.code, -32602);
    let request = server
        .send_request("pool/manage", Some(json!({"action":"list"})))
        .await?;
    let pools: codex_app_server_protocol::ManagedPoolResponse =
        server.read_response(request).await?;
    assert_eq!(
        pools
            .data
            .iter()
            .map(|pool| pool.name.as_str())
            .collect::<Vec<_>>(),
        vec!["work"]
    );
    let thread = server
        .start_thread(ThreadStartParams {
            account_selection: Some(AccountSelection::Pool("work".to_owned())),
            approval_policy: Some(codex_app_server_protocol::AskForApproval::Never),
            sandbox: Some(codex_app_server_protocol::SandboxMode::DangerFullAccess),
            ..Default::default()
        })
        .await?;
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Check once and finish.".to_owned(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let switched = loop {
        let notification: ThreadAccountPoolNotification = server
            .read_notification("thread/accountPool/updated")
            .await?;
        if matches!(notification.event, AccountPoolEvent::Switched { .. }) {
            break notification;
        }
    };
    assert_eq!(
        switched,
        ThreadAccountPoolNotification {
            thread_id: thread.thread.id,
            event: AccountPoolEvent::Switched {
                account: "b".to_owned(),
                previous_account: "a".to_owned()
            }
        }
    );
    account_pools::assert_recovery(&requests);
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_waiting_interrupt_and_explicit_resume_restore_selection() -> Result<()> {
    use codex_app_server_protocol::ThreadResumeParams;
    use codex_app_server_protocol::ThreadResumeResponse;
    use codex_app_server_protocol::TurnCompletedNotification;
    use codex_app_server_protocol::TurnInterruptParams;
    use codex_app_server_protocol::TurnStartResponse;
    use core_test_support::responses;
    let backend = MockServer::start().await;
    let home = TempDir::new()?;
    account_pools::setup(&home, &backend).await?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\nopenai_base_url = {:?}\n",
            backend.uri(),
            format!("{}/v1", backend.uri())
        ),
    )?;
    let blocked = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
    let blocking = blocked.clone();
    wiremock::Mock::given(move |_: &wiremock::Request| blocking.load(std::sync::atomic::Ordering::SeqCst))
        .and(wiremock::matchers::method("GET"))
        .and(wiremock::matchers::path("/api/codex/usage"))
        .respond_with(wiremock::ResponseTemplate::new(200).set_body_json(json!({"plan_type":"pro",
            "rate_limit":{"allowed":false,"limit_reached":true,
                "primary_window":{"used_percent":100,"limit_window_seconds":18000,"reset_after_seconds":3600,"reset_at":2_000_000_000},
                "secondary_window":{"used_percent":100,"limit_window_seconds":604800,"reset_after_seconds":3600,"reset_at":2_000_000_000}},
            "rate_limit_reset_credits":{"available_count":0}}))).with_priority(1)
        .mount(&backend).await;
    let requests = responses::mount_response_sequence(
        &backend,
        vec![
            wiremock::ResponseTemplate::new(429)
                .set_body_json(json!({"error":{"type":"usage_limit_reached","plan_type":"pro"}})),
            responses::sse_response(responses::sse(vec![
                responses::ev_response_created("resumed"),
                responses::ev_assistant_message("answer", "Resumed."),
                responses::ev_completed("resumed"),
            ])),
        ],
    )
    .await;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let thread = server
        .start_thread(ThreadStartParams {
            account_selection: Some(AccountSelection::Pool("work".to_owned())),
            ..Default::default()
        })
        .await?;
    let id = server
        .send_turn_start_request(TurnStartParams {
            thread_id: thread.thread.id.clone(),
            input: vec![UserInput::Text {
                text: "Remember this request while waiting.".to_owned(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        })
        .await?;
    let turn: TurnStartResponse = server.read_response(id).await?;
    loop {
        let event: ThreadAccountPoolNotification = server
            .read_notification("thread/accountPool/updated")
            .await?;
        if matches!(event.event, AccountPoolEvent::Waiting { .. }) {
            break;
        }
    }
    let id = server
        .send_turn_interrupt_request(TurnInterruptParams {
            thread_id: thread.thread.id.clone(),
            turn_id: turn.turn.id,
        })
        .await?;
    let _: serde_json::Value = server.read_response(id).await?;
    let completed: TurnCompletedNotification = server.read_notification("turn/completed").await?;
    assert_eq!(completed.turn.status, TurnStatus::Interrupted);
    server.shutdown_gracefully().await?;
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(
        home.path()
            .join("accounts/sessions")
            .join(format!("{}.json", thread.thread.id)),
    )?)?;
    assert_eq!(saved["waiting"], true);
    blocked.store(/*val*/ false, std::sync::atomic::Ordering::SeqCst);
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let id = server
        .send_thread_resume_request(ThreadResumeParams {
            thread_id: thread.thread.id.clone(),
            ..Default::default()
        })
        .await?;
    let _: ThreadResumeResponse = server.read_response(id).await?;
    assert_eq!(
        requests.requests().len(),
        1,
        "resume alone must not restart monitoring or inference"
    );
    let id = server
        .send_request(
            "account/manage",
            Some(json!({"action":"usage", "model":"gpt-5.5"})),
        )
        .await?;
    let usage: ManagedAccountResponse = server.read_response(id).await?;
    assert!(
        usage.usage.iter().all(|entry| entry.error.is_none()),
        "{usage:?}"
    );
    let completed = tokio::time::timeout(
        std::time::Duration::from_secs(40),
        server.start_turn_and_wait_for_completion(TurnStartParams {
            thread_id: thread.thread.id,
            input: vec![UserInput::Text {
                text: "Continue.".to_owned(),
                text_elements: Vec::new(),
            }],
            ..Default::default()
        }),
    )
    .await??;
    assert_eq!(completed.turn.status, TurnStatus::Completed);
    let requests = requests.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].header("chatgpt-account-id").as_deref(),
        Some("workspace-b")
    );
    assert!(
        serde_json::to_string(&requests[1].input())?
            .contains("Remember this request while waiting.")
    );
    Ok(())
}
