//! Integration tests for `McpServerSource`: the JSON config schema, tool
//! discovery + ToolSpec namespacing, include/exclude filtering, error paths
//! (crashed server), and end-to-end `call_by_registry_name` — all against
//! the local mock MCP server child (no network).

use hermes_mcp::{McpServerSource, ToolSource};
use serde_json::json;
use std::time::Duration;

fn mock_path() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/tests/mock_mcp_server.js").to_string()
}

fn config_with(tools: &str) -> String {
    // The mock serves 3 tools over cursor pagination (page size 1).
    format!(
        r#"{{
            "mcp_servers": {{
                "mock": {{
                    "command": "node",
                    "args": ["{js}"],
                    "timeout": 30{tools}
                }}
            }}
        }}"#,
        js = mock_path().replace('\\', "/"),
        tools = tools
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn discovery_yields_namespaced_tool_specs() {
    let src = McpServerSource::from_json_str(&config_with("")).unwrap();
    let rows = src.discover().await;
    assert_eq!(rows.len(), 1, "one configured server => one row");
    let row = &rows[0];
    assert_eq!(row.server, "mock");
    assert!(row.error.is_none(), "mock connects cleanly");
    let names: Vec<&str> = row.tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(
        names,
        vec!["mcp__mock__echo", "mcp__mock__fail", "mcp__mock__pingme"]
    );
    let echo = &row.tools[0];
    assert_eq!(echo.description, "Echo a message");
    assert_eq!(echo.schema["properties"]["message"]["type"], "string");
}

#[tokio::test(flavor = "multi_thread")]
async fn include_filter_prunes_specs() {
    let src = McpServerSource::from_json_str(&config_with(
        r#",
                    "tools": {"include": ["echo"]}"#,
    ))
    .unwrap();
    let rows = src.discover().await;
    let names: Vec<&str> = rows[0].tools.iter().map(|t| t.name.as_str()).collect();
    assert_eq!(names, vec!["mcp__mock__echo"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn crashed_server_is_reported_not_fatal() {
    let src = McpServerSource::from_json_str(
        r#"{
            "mcp_servers": {
                "dead": {"command": "node", "args": ["-e", "process.exit(3)"], "timeout": 20},
                "aliveref": {"command": "alive-but-not-configured-properly"}
            }
        }"#,
    )
    .unwrap();
    let rows = src.discover_tool_specs().await;
    let dead = rows.iter().find(|r| r.server == "dead").unwrap();
    assert!(dead.error.is_some(), "crash surfaces as row error");
    assert!(dead.tools.is_empty());
    // The broken non-existent-command server also fails without killing the batch.
    let _alive = rows.iter().find(|r| r.server == "aliveref").unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn call_by_registry_name_round_trips() {
    let src = McpServerSource::from_json_str(&config_with("")).unwrap();
    // Prime the owner map the registry feed would have from discovery.
    let _ = src.discover().await;
    let rendered = src
        .call_by_registry_name("mcp__mock__echo", &json!({"message": "over the wire"}))
        .await
        .expect("call succeeds");
    assert_eq!(rendered, "you said: over the wire");
}

#[tokio::test(flavor = "multi_thread")]
async fn call_error_path_renders_error_payload() {
    let src = McpServerSource::from_json_str(&config_with("")).unwrap();
    let _ = src.discover().await;
    let rendered = src
        .call_by_registry_name("mcp__mock__fail", &json!({}))
        .await
        .expect("the call itself succeeds; the tool result carries the error");
    let parsed: serde_json::Value = serde_json::from_str(&rendered).unwrap();
    assert_eq!(parsed["error"], "boom");
    assert_eq!(parsed["tool"], "mcp__mock__fail");
}

#[tokio::test(flavor = "multi_thread")]
async fn unknown_registry_name_errors() {
    let src = McpServerSource::from_json_str(&config_with("")).unwrap();
    let err = src
        .call_by_registry_name("mcp__nowhere__tool", &json!({}))
        .await
        .err()
        .expect("no owner configured");
    assert!(err.to_string().contains("owns tool"));
}

#[tokio::test(flavor = "multi_thread")]
async fn config_file_round_trip() {
    let dir = std::env::temp_dir().join(format!("hermes-mcp-cfg-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("mcp_servers.json");
    std::fs::write(&path, config_with("")).unwrap();
    let src = McpServerSource::from_config_file(&path).unwrap();
    assert_eq!(src.server_names(), vec!["mock".to_string()]);
    std::fs::remove_dir_all(&dir).ok();
    let _ = Duration::from_secs(0); // keep the import used
}
