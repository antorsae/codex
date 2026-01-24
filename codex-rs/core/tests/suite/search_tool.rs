#![cfg(not(target_os = "windows"))]
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::time::Duration;

use anyhow::Result;
use codex_core::config::types::McpServerConfig;
use codex_core::config::types::McpServerTransportConfig;
use codex_core::features::Feature;
use codex_core::protocol::AskForApproval;
use codex_core::protocol::SandboxPolicy;
use core_test_support::responses::ev_assistant_message;
use core_test_support::responses::ev_completed;
use core_test_support::responses::ev_function_call;
use core_test_support::responses::ev_response_created;
use core_test_support::responses::mount_sse_sequence;
use core_test_support::responses::sse;
use core_test_support::responses::start_mock_server;
use core_test_support::skip_if_no_network;
use core_test_support::stdio_server_bin;
use core_test_support::test_codex::test_codex;
use pretty_assertions::assert_eq;
use serde_json::Value;
use serde_json::json;

const SEARCH_TOOL_INSTRUCTIONS: &str =
    include_str!("../../templates/search_tool/developer_instructions.md");

fn tool_names(body: &Value) -> Vec<String> {
    body.get("tools")
        .and_then(Value::as_array)
        .map(|tools| {
            tools
                .iter()
                .filter_map(|tool| {
                    tool.get("name")
                        .or_else(|| tool.get("type"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn developer_messages(body: &Value) -> Vec<String> {
    body.get("input")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter_map(|item| {
                    if item.get("role").and_then(Value::as_str) != Some("developer") {
                        return None;
                    }
                    let content = item.get("content").and_then(Value::as_array)?;
                    let texts: Vec<&str> = content
                        .iter()
                        .filter_map(|entry| entry.get("text").and_then(Value::as_str))
                        .collect();
                    if texts.is_empty() {
                        None
                    } else {
                        Some(texts.join("\n"))
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tool_flag_adds_tool() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ])],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::SearchTool);
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_policies(
        "list tools",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let body = mock.single_request().body_json();
    let tools = tool_names(&body);
    assert!(
        tools.iter().any(|name| name == "search_tool_bm25"),
        "tools list should include search_tool_bm25 when enabled: {tools:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tool_adds_developer_instructions() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ])],
    )
    .await;

    let mut builder = test_codex().with_config(|config| {
        config.features.enable(Feature::SearchTool);
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_policies(
        "list tools",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let body = mock.single_request().body_json();
    let developer_texts = developer_messages(&body);
    let expected = SEARCH_TOOL_INSTRUCTIONS.trim();
    assert!(
        developer_texts.iter().any(|text| text.trim() == expected),
        "developer instructions should include search tool workflow: {developer_texts:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tool_hides_mcp_tools_without_search() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let mock = mount_sse_sequence(
        &server,
        vec![sse(vec![
            ev_response_created("resp-1"),
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-1"),
        ])],
    )
    .await;

    let rmcp_test_server_bin = stdio_server_bin()?;
    let mut builder = test_codex().with_config(move |config| {
        config.features.enable(Feature::SearchTool);
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            "rmcp".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::Stdio {
                    command: rmcp_test_server_bin,
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                enabled: true,
                disabled_reason: None,
                startup_timeout_sec: Some(Duration::from_secs(10)),
                tool_timeout_sec: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
            },
        );
        config
            .mcp_servers
            .set(servers)
            .expect("test mcp servers should accept any configuration");
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_policies(
        "hello tools",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let body = mock.single_request().body_json();
    let tools = tool_names(&body);
    assert!(
        tools.iter().any(|name| name == "search_tool_bm25"),
        "tools list should include search_tool_bm25 when enabled: {tools:?}"
    );
    assert!(
        !tools.iter().any(|name| name == "mcp__rmcp__echo"),
        "tools list should not include MCP tools before search: {tools:?}"
    );
    assert!(
        !tools.iter().any(|name| name == "mcp__rmcp__image"),
        "tools list should not include MCP tools before search: {tools:?}"
    );

    Ok(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn search_tool_selects_tools_for_next_request_only() -> Result<()> {
    skip_if_no_network!(Ok(()));

    let server = start_mock_server().await;
    let call_id = "tool-search";
    let args = json!({
        "query": "echo",
        "limit": 1,
    });
    let responses = vec![
        sse(vec![
            ev_response_created("resp-1"),
            ev_function_call(call_id, "search_tool_bm25", &serde_json::to_string(&args)?),
            ev_completed("resp-1"),
        ]),
        sse(vec![
            ev_assistant_message("msg-1", "done"),
            ev_completed("resp-2"),
        ]),
        sse(vec![
            ev_assistant_message("msg-2", "done again"),
            ev_completed("resp-3"),
        ]),
    ];
    let mock = mount_sse_sequence(&server, responses).await;

    let rmcp_test_server_bin = stdio_server_bin()?;
    let mut builder = test_codex().with_config(move |config| {
        config.features.enable(Feature::SearchTool);
        let mut servers = config.mcp_servers.get().clone();
        servers.insert(
            "rmcp".to_string(),
            McpServerConfig {
                transport: McpServerTransportConfig::Stdio {
                    command: rmcp_test_server_bin,
                    args: Vec::new(),
                    env: None,
                    env_vars: Vec::new(),
                    cwd: None,
                },
                enabled: true,
                disabled_reason: None,
                startup_timeout_sec: Some(Duration::from_secs(10)),
                tool_timeout_sec: None,
                enabled_tools: None,
                disabled_tools: None,
                scopes: None,
            },
        );
        config
            .mcp_servers
            .set(servers)
            .expect("test mcp servers should accept any configuration");
    });
    let test = builder.build(&server).await?;

    test.submit_turn_with_policies(
        "find the echo tool",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;
    test.submit_turn_with_policies(
        "hello again",
        AskForApproval::Never,
        SandboxPolicy::DangerFullAccess,
    )
    .await?;

    let requests = mock.requests();
    assert_eq!(
        requests.len(),
        3,
        "expected 3 requests, got {}",
        requests.len()
    );

    let first_tools = tool_names(&requests[0].body_json());
    assert!(
        !first_tools.iter().any(|name| name == "mcp__rmcp__echo"),
        "first request should not include MCP tools before search: {first_tools:?}"
    );

    let second_tools = tool_names(&requests[1].body_json());
    assert!(
        second_tools.iter().any(|name| name == "mcp__rmcp__echo"),
        "second request should include selected MCP tool: {second_tools:?}"
    );
    assert!(
        !second_tools.iter().any(|name| name == "mcp__rmcp__image"),
        "second request should only include selected MCP tool: {second_tools:?}"
    );

    let third_tools = tool_names(&requests[2].body_json());
    assert!(
        !third_tools.iter().any(|name| name == "mcp__rmcp__echo"),
        "third request should not include MCP tools after selection consumed: {third_tools:?}"
    );

    Ok(())
}
