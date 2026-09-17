use super::*;
use codex_app_server_protocol::JSONRPCMessage;
use futures::SinkExt;
use futures::StreamExt;
use serde_json::json;
use std::time::Duration;
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;

#[tokio::test]
async fn account_pool_picker_cancellation_returns_reusable_session_after_stalled_read() -> Result<()>
{
    let listener = TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = crate::resolve_remote_addr(&format!("ws://{}", listener.local_addr()?))?;
    let (started_tx, started_rx) = oneshot::channel();
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await?;
        let mut socket = tokio_tungstenite::accept_async(stream).await?;
        let mut started_tx = Some(started_tx);
        while let Some(Ok(Message::Text(text))) = socket.next().await {
            let JSONRPCMessage::Request(request) = serde_json::from_str(&text)? else {
                continue;
            };
            let mut reply = match request.method.as_str() {
                "initialize" => json!({"result":{"userAgent":"picker-test/1.0.0"}}),
                "thread/list" => {
                    let _ = started_tx.take().unwrap().send(());
                    continue;
                }
                "account/manage" => json!({"error":{"code":-32601,"message":"Unknown method"}}),
                "account/read" => json!({"result":{"account":null,"requiresOpenaiAuth":false}}),
                method => panic!("unexpected request: {method}"),
            };
            reply["id"] = json!(request.id);
            socket.send(Message::Text(reply.to_string().into())).await?;
        }
        Ok::<_, color_eyre::Report>(())
    });
    let session = AppServerSession::new(
        crate::connect_remote_app_server(endpoint).await?,
        crate::app_server_session::ThreadParamsMode::Remote,
    );
    let handle = session.request_handle();
    let (bg_tx, bg_rx) = mpsc::unbounded_channel();
    let (loader, worker) = spawn_app_server_page_loader(
        /*uses_remote_filesystem*/ true,
        session,
        handle,
        /*include_non_interactive*/ false,
        RawReasoningVisibility::Hidden,
        /*config*/ None,
        bg_tx,
    );
    loader(PickerLoadRequest::Page(PageLoadRequest {
        cursor: None,
        request_token: 1,
        search_token: None,
        mode: PageLoadMode::StoreDefault,
        cwd_filter: None,
        status: SessionStatus::Active,
        provider_filter: ProviderFilter::Any,
        sort_key: ThreadSortKey::UpdatedAt,
    }));
    tokio::time::timeout(Duration::from_secs(5), started_rx).await??;
    drop(bg_rx);
    drop(loader);
    let mut session = tokio::time::timeout(Duration::from_secs(1), worker).await??;
    session.read_account().await?;
    session.shutdown().await?;
    server.await??;
    Ok(())
}
