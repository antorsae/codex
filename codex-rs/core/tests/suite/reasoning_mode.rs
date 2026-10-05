//! Verifies that `reasoning.mode` follows config and thread-settings updates.

use anyhow::Result;
use codex_protocol::config_types::ReasoningMode;
use codex_protocol::protocol::ThreadSettingsOverrides;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::submit_thread_settings;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;

fn completed(id: &str) -> String {
    sse(vec![
        ev_response_created(id),
        ev_assistant_message(&format!("{id}-message"), "done"),
        ev_completed(id),
    ])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn configured_pro_mode_is_sent_until_thread_settings_change_it() -> Result<()> {
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(
        &server,
        vec![completed("pro-turn"), completed("standard-turn")],
    )
    .await;
    let test = test_codex()
        .with_pre_build_hook(|home| {
            std::fs::write(home.join("config.toml"), "model_reasoning_mode = \"pro\"\n")
                .expect("write test config");
        })
        .build_with_auto_env(&server)
        .await?;

    test.submit_text_turn("first turn").await?;
    submit_thread_settings(
        &test.codex,
        ThreadSettingsOverrides {
            reasoning_mode: Some(ReasoningMode::Standard),
            ..Default::default()
        },
    )
    .await?;
    test.submit_text_turn("second turn").await?;

    let modes: Vec<Value> = responses
        .requests()
        .iter()
        .map(|request| request.body_json()["reasoning"]["mode"].clone())
        .collect();
    assert_eq!(
        modes,
        vec![
            Value::String("pro".to_string()),
            Value::String("standard".to_string()),
        ]
    );
    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn unset_reasoning_mode_is_omitted_from_requests() -> Result<()> {
    let server = start_mock_server().await;
    let responses = mount_sse_sequence(&server, vec![completed("default-turn")]).await;
    let test = test_codex().build_with_auto_env(&server).await?;

    test.submit_text_turn("only turn").await?;

    let reasoning = responses.single_request().body_json()["reasoning"].clone();
    assert_eq!(reasoning.get("mode"), None);
    Ok(())
}
