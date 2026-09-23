//! Conversation account handoff is independent of shared pool preferences.

use super::*;
use core_test_support::responses;
use pretty_assertions::assert_eq;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn account_pool_fresh_thread_inherits_its_source_despite_other_sessions_success() -> Result<()>
{
    let backend = MockServer::start().await;
    let home = TempDir::new()?;
    account_pools::setup(&home, &backend).await?;
    std::fs::write(
        home.path().join("config.toml"),
        format!(
            "model = \"gpt-5.5\"\ncli_auth_credentials_store = \"file\"\nchatgpt_base_url = {:?}\nopenai_base_url = {:?}\n",
            backend.uri(),
            format!("{}/v1", backend.uri()),
        ),
    )?;
    let response = responses::sse(vec![
        responses::ev_assistant_message("answer", "Done."),
        responses::ev_completed("done"),
    ]);
    let responses = responses::mount_sse_sequence(&backend, vec![response.clone(), response]).await;
    let mut server = TestAppServer::builder()
        .with_codex_home(home.path())
        .build_initialized()
        .await?;
    let id = server
        .send_request(
            "pool/manage",
            Some(json!({"action":"select", "name":"work"})),
        )
        .await?;
    let _: codex_app_server_protocol::ManagedPoolResponse = server.read_response(id).await?;
    let first = server.start_thread(ThreadStartParams::default()).await?;
    let second = server.start_thread(ThreadStartParams::default()).await?;
    for (thread, expected) in [(&first, "b"), (&second, "a")] {
        if expected == "a" {
            account_pools::mount_initial_available_usage(&backend).await;
        }
        let completed = server
            .start_turn_and_wait_for_completion(TurnStartParams {
                thread_id: thread.thread.id.clone(),
                input: vec![UserInput::Text {
                    text: "Finish successfully.".into(),
                    text_elements: Vec::new(),
                }],
                ..Default::default()
            })
            .await?;
        assert_eq!(completed.turn.status, TurnStatus::Completed);
    }
    assert_eq!(
        responses
            .requests()
            .iter()
            .map(|request| request.header("chatgpt-account-id"))
            .collect::<Vec<_>>(),
        vec![Some("workspace-b".into()), Some("workspace-a".into())]
    );
    for (source, selection, expected, pool) in [
        (Some(first.thread.id.clone()), None, "b", Some("work")),
        (None, None, "a", Some("work")),
        (
            Some(first.thread.id.clone()),
            Some(AccountSelection::Account("a".into())),
            "a",
            None,
        ),
    ] {
        let started = server
            .start_thread(ThreadStartParams {
                account_selection: selection,
                account_selection_source_thread_id: source,
                ..Default::default()
            })
            .await?;
        assert!(
            started.thread.turns.is_empty(),
            "account handoff must not copy conversation history"
        );
        let id = server
            .send_request(
                "account/manage",
                Some(json!({"action":"resolve", "threadId":started.thread.id})),
            )
            .await?;
        let resolved: ManagedAccountResponse = server.read_response(id).await?;
        assert_eq!(
            (
                resolved.selected_account.map(|a| a.alias),
                resolved.selected_pool.map(|p| p.name)
            ),
            (Some(expected.to_owned()), pool.map(str::to_owned))
        );
    }
    server.shutdown_gracefully().await?;
    Ok(())
}
