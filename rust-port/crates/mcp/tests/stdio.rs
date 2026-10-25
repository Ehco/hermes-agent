//! Integration tests: drive the full stdio MCP flow (handshake → tools/list
//! with pagination → tools/call → ping) against a mock MCP server child —
//! the same wire the Python MCP machinery speaks. No network involved.

use hermes_mcp::content::render_call_result;
use hermes_mcp::McpClient;
use serde_json::json;
use std::collections::HashMap;
use std::time::Duration;

fn mock_path() -> String {
    // Compiled-in path to the checked-in mock server script.
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_mcp_server.js").to_string()
}

fn short_timeout() -> Duration {
    Duration::from_secs(30)
}

/// Spawn the mock and complete the handshake.
async fn connected_mock() -> McpClient {
    let mut client = McpClient::stdio("node", &[mock_path()], &HashMap::new(), None)
        .expect("spawn mock MCP server")
        .with_request_timeout(short_timeout());
    client.handshake().await.expect("handshake with mock");
    client
}

#[tokio::test(flavor = "multi_thread")]
async fn handshake_negotiates_and_records_server_info() {
    let mut client = connected_mock().await;
    let info = client.initialize_result.clone().expect("handshake ran");
    assert_eq!(info.protocol_version, "2025-03-26");
    assert_eq!(info.server_info.as_ref().unwrap().name, "mock-mcp");
    assert!(client.advertises_tools(), "mock advertises the tools capability");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_list_paginates_via_cursor() {
    let mut client = connected_mock().await;
    let tools = client.list_tools().await.expect("list_tools");
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["echo", "fail", "pingme"]);
    // The mock serves one tool per page: the client followed the cursors.
    let (page, cursor) = client.list_tools_page(None).await.unwrap();
    assert_eq!(page.len(), 1);
    assert_eq!(cursor, Some("1".to_string()));
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_call_success_renders_text() {
    let mut client = connected_mock().await;
    let result = client.call_tool("echo", &json!({"message": "hi"})).await.unwrap();
    assert!(!result.is_error);
    let rendered = render_call_result(&result, "mcp__mock__echo");
    assert_eq!(rendered, "you said: hi");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn tools_call_error_flag_becomes_error_payload() {
    let mut client = connected_mock().await;
    let result = client.call_tool("fail", &json!({})).await.unwrap();
    assert!(result.is_error, "isError flag must survive parsing");
    let rendered = render_call_result(&result, "mcp__mock__fail");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["error"], "boom");
    assert_eq!(parsed["tool"], "mcp__mock__fail");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_tool_raises_jsonrpc_error() {
    let mut client = connected_mock().await;
    let err = client
        .call_tool("no_such_tool", &json!({}))
        .await
        .err()
        .expect("unknown tool must raise a JSON-RPC error");
    assert!(err.to_string().contains("-32602"), "got: {err}");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn ping_round_trips() {
    let mut client = connected_mock().await;
    client.ping().await.expect("ping answered");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn crashed_server_surfaces_eof() {
    let mut client = McpClient::stdio(
        "node",
        &["-e".to_string(), "process.exit(3)".to_string()],
        &HashMap::new(),
        None,
    )
    .unwrap()
    .with_request_timeout(short_timeout());
    let err = client
        .handshake()
        .await
        .expect_err("handshake must fail on a dead child");
    let msg = err.to_string();
    assert!(
        msg.contains("closed the stdio transport") || msg.contains("writing to MCP server stdin"),
        "got: {msg}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn spawn_failure_is_reported() {
    let err = McpClient::stdio("definitely-not-a-real-binary-xyz", &[], &HashMap::new(), None)
        .err()
        .expect("nonexistent command must fail to spawn");
    assert!(err.to_string().contains("definitely-not-a-real-binary-xyz"));
}
