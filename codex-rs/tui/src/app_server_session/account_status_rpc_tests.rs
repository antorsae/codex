use super::*;
use crate::app_server_session::AppServerSession;
use crate::app_server_session::ThreadParamsMode;
use codex_app_server_protocol::JSONRPCMessage;
use color_eyre::Result;
use futures::SinkExt;
use futures::StreamExt;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;
use tokio_tungstenite::tungstenite::Message;

type Requests = Vec<(String, Option<String>)>;

async fn connect(
    mut respond: impl FnMut(&Value) -> Value + Send + 'static,
) -> Result<(AppServerSession, JoinHandle<Result<Requests>>)> {
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut requests = Vec::new();
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let mut reply = match request.method.as_str() {
                "initialize" => json!({"result": {"userAgent": "account-status-test/1.0"}}),
                "account/manage" => {
                    let params = request.params.unwrap();
                    requests.push((
                        params["action"].as_str().unwrap().to_owned(),
                        params["alias"].as_str().map(str::to_owned),
                    ));
                    respond(&params)
                }
                method => panic!("unexpected request: {method}"),
            };
            reply["id"] = json!(request.id);
            socket.send(Message::Text(reply.to_string().into())).await?;
        }
        Ok(requests)
    });
    let session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        ThreadParamsMode::Embedded,
    );
    Ok((session, server))
}

fn account(alias: &str) -> Value {
    json!({"alias": alias, "userId": alias, "workspaceId": "workspace"})
}

fn quota(account: Value) -> Value {
    json!({
        "account": account, "pools": ["work"], "checkedAt": chrono::Utc::now().timestamp(),
        "windows": [{"limitId": "codex", "remainingPercent": 50.0, "windowMinutes": 10080}]
    })
}

#[tokio::test]
async fn footer_rpc_reuses_quota_and_invalidates_changed_identities_and_membership() -> Result<()> {
    let mut resolves = 0;
    let mut accounts = vec![account("a"), account("b"), account("c")];
    let (session, server) = connect(move |params| {
        let mut usage = Vec::new();
        match params["action"].as_str().unwrap() {
            "resolve" => {
                resolves += 1;
                if resolves == 3 {
                    accounts[0]["userId"] = json!("replacement");
                } else if resolves == 4 {
                    accounts.pop();
                }
            }
            "quota" => usage.push(quota(accounts.iter().find(|a| a["alias"] == params["alias"]).unwrap().clone())),
            action => panic!("unexpected action: {action}"),
        }
        json!({"result": {
            "data": accounts, "usage": usage, "selectedAccount": account("b"),
            "selectedPool": {"name": "work", "accounts": accounts.iter().map(|a| &a["alias"]).collect::<Vec<_>>(), "redeemWeeklyResets": false}
        }})
    }).await?;
    let thread_id = ThreadId::new();
    let handle = session.request_handle();
    for expected_count in [3, 3, 3, 2] {
        let snapshot = load(
            &handle,
            &session.account_status_cache,
            &session.account_status_changed,
            &session.account_status_invalidations,
            thread_id,
            /*include_pool_usage*/ true,
        )
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
        assert_eq!(snapshot.usage.len(), expected_count);
    }
    {
        let cache = session.account_status_cache.lock().await;
        let mut identities: Vec<_> = cache
            .entries
            .values()
            .map(|entry| (entry.account.alias.clone(), entry.account.user_id.clone()))
            .collect();
        identities.sort();
        assert_eq!(
            identities,
            vec![
                ("a".to_owned(), "replacement".to_owned()),
                ("b".to_owned(), "b".to_owned())
            ]
        );
    }
    drop(handle);
    session.shutdown().await?;
    assert_eq!(
        server.await??,
        vec![
            ("resolve".to_owned(), None),
            ("quota".to_owned(), Some("b".to_owned())),
            ("quota".to_owned(), Some("a".to_owned())),
            ("quota".to_owned(), Some("c".to_owned())),
            ("resolve".to_owned(), None),
            ("resolve".to_owned(), None),
            ("quota".to_owned(), Some("a".to_owned())),
            ("resolve".to_owned(), None),
        ]
    );
    Ok(())
}

#[tokio::test]
async fn footer_rpc_falls_back_only_for_legacy_quota_and_remembers_support() -> Result<()> {
    for (code, message, fallback) in [
        (-32600, "unknown variant `quota`", true),
        (-32602, "unknown variant `quota`", true),
        (-32603, "unknown variant `quota`", false),
        (-32602, "invalid alias", false),
    ] {
        let (session, server) = connect(move |params| match params["action"].as_str().unwrap() {
            "resolve" => json!({"result": {"data": [account("a")], "usage": [], "selectedAccount": account("a")}}),
            "quota" => json!({"error": {"code": code, "message": message}}),
            "usage" => json!({"result": {"data": [], "usage": [quota(account("a"))]}}),
            action => panic!("unexpected action: {action}"),
        }).await?;
        let thread_id = ThreadId::new();
        let handle = session.request_handle();
        for _ in 0..2 {
            let snapshot = load(
                &handle,
                &session.account_status_cache,
                &session.account_status_changed,
                &session.account_status_invalidations,
                thread_id,
                /*include_pool_usage*/ true,
            )
            .await
            .map_err(color_eyre::eyre::Report::msg)?;
            assert_eq!(snapshot.usage.len(), usize::from(fallback));
            session.account_status_cache.lock().await.entries.clear();
        }
        drop(handle);
        session.shutdown().await?;
        let actions: Vec<_> = server
            .await??
            .into_iter()
            .map(|(action, _)| action)
            .collect();
        let expected = if fallback {
            vec!["resolve", "quota", "usage", "resolve", "usage"]
        } else {
            vec!["resolve", "quota", "resolve", "quota"]
        };
        assert_eq!(actions, expected);
    }
    Ok(())
}

#[tokio::test]
async fn footer_rpc_refreshes_exhausted_selection_after_live_account_change() -> Result<()> {
    let (session, server) = connect(|params| match params["action"].as_str().unwrap() {
        "resolve" => json!({"result": {
            "data": [account("a")], "usage": [], "selectedAccount": account("a")
        }}),
        "quota" => {
            let mut usage = quota(account("a"));
            usage["windows"][0]["remainingPercent"] = json!(0.0);
            usage["windows"][0]["resetsAt"] = json!(chrono::Utc::now().timestamp() + 172800);
            json!({"result": {"data": [], "usage": [usage]}})
        }
        action => panic!("unexpected action: {action}"),
    })
    .await?;
    let thread_id = ThreadId::new();
    let handle = session.request_handle();
    for changed in [false, false, true] {
        session
            .account_status_changed
            .store(changed, Ordering::Relaxed);
        load(
            &handle,
            &session.account_status_cache,
            &session.account_status_changed,
            &session.account_status_invalidations,
            thread_id,
            /*include_pool_usage*/ true,
        )
        .await
        .map_err(color_eyre::eyre::Report::msg)?;
    }
    drop(handle);
    session.shutdown().await?;
    assert_eq!(
        server.await??,
        vec![
            ("resolve".to_owned(), None),
            ("quota".to_owned(), Some("a".to_owned())),
            ("resolve".to_owned(), None),
            ("resolve".to_owned(), None),
            ("quota".to_owned(), Some("a".to_owned())),
        ]
    );
    Ok(())
}
