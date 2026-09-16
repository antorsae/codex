use anyhow::Result;
use app_test_support::TestAppServer;
use axum::Json;
use axum::Router;
use axum::http::HeaderMap;
use axum::routing::get;
use codex_app_server_protocol::ManagedAccountResponse;
use core_test_support::account_pools;
use pretty_assertions::assert_eq;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tempfile::TempDir;
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tokio::sync::mpsc;
use tokio::time::timeout;
use wiremock::MockServer;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_usage_reads_overlap_and_account_mutations_wait() -> Result<()> {
    let fixtures = MockServer::start().await;
    let home = TempDir::new()?;
    account_pools::setup(&home, &fixtures).await?;
    let (entered_tx, mut entered_rx) = mpsc::unbounded_channel();
    let release = Arc::new(Semaphore::new(/*permits*/ 0));
    let usage_release = Arc::clone(&release);
    let router = Router::new().route(
        "/api/codex/usage",
        get(move |headers: HeaderMap| {
            let entered_tx = entered_tx.clone();
            let release = Arc::clone(&usage_release);
            async move {
                entered_tx
                    .send(headers["chatgpt-account-id"].to_str().unwrap().to_owned())
                    .unwrap();
                release.acquire().await.unwrap().forget();
                Json(json!({
                    "plan_type": "pro",
                    "rate_limit": {"allowed": true, "limit_reached": false},
                    "rate_limit_reset_credits": {"available_count": 0}
                }))
            }
        }),
    );
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let address = listener.local_addr()?;
    let http = tokio::spawn(async move { axum::serve(listener, router).await });
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = \"http://{address}\"\nopenai_base_url = {:?}\n",
            format!("{}/v1", fixtures.uri())
        ),
    )?;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized_with_timeout(Duration::from_secs(/*secs*/ 30))
        .await?;
    let mut reads = Vec::new();
    for alias in ["a", "b"] {
        reads.push(
            server
                .send_request(
                    "account/manage",
                    Some(json!({"action": "usage", "alias": alias})),
                )
                .await?,
        );
    }
    let mut entered = Vec::new();
    for _ in 0..2 {
        entered.push(
            timeout(Duration::from_secs(/*secs*/ 5), entered_rx.recv())
                .await?
                .unwrap(),
        );
    }
    entered.sort();
    assert_eq!(entered, vec!["workspace-a", "workspace-b"]);
    let resolve = server
        .send_request(
            "account/manage",
            Some(
                json!({"action": "resolve", "accountSelection": {"type": "account", "name": "a"}}),
            ),
        )
        .await?;
    let resolved: ManagedAccountResponse = timeout(
        Duration::from_secs(/*secs*/ 5),
        server.read_response(resolve),
    )
    .await??;
    assert_eq!(
        resolved.selected_account.map(|account| account.alias),
        Some("a".to_owned())
    );
    let mutation = server
        .send_request(
            "account/manage",
            Some(json!({"action": "select", "alias": "b"})),
        )
        .await?;
    assert!(
        timeout(
            Duration::from_millis(/*millis*/ 100),
            server.read_response::<ManagedAccountResponse>(mutation)
        )
        .await
        .is_err()
    );
    release.add_permits(/*n*/ 2);
    for request in reads {
        let response: ManagedAccountResponse = server.read_response(request).await?;
        assert_eq!(response.usage.len(), 1);
    }
    let selected: ManagedAccountResponse = server.read_response(mutation).await?;
    assert_eq!(
        selected.default_selection,
        Some(codex_protocol::account_pool::AccountSelection::Account(
            "b".to_owned()
        ))
    );

    // A paginated bulk read must also start both backend requests before either completes.
    let bulk = server
        .send_request("account/manage", Some(json!({"action": "usage"})))
        .await?;
    for _ in 0..2 {
        timeout(Duration::from_secs(/*secs*/ 5), entered_rx.recv())
            .await?
            .unwrap();
    }
    release.add_permits(/*n*/ 2);
    let response: ManagedAccountResponse = server.read_response(bulk).await?;
    assert_eq!(
        response
            .usage
            .into_iter()
            .map(|usage| usage.account.alias)
            .collect::<Vec<_>>(),
        vec!["a", "b"]
    );
    http.abort();
    Ok(())
}
