//! Hermes-tools-over-MCP **server** — port of
//! `agent/transports/hermes_tools_mcp_server.py` (the Python exposes a curated
//! subset of Hermes tools over stdio MCP so *other* runtimes can drive them;
//! the Rust serve mode exposes the crates-tools registry).
//!
//! Wire contract (JSON-RPC 2.0, line-delimited stdio — mirrors what
//! [`crate::client`] and the Python client speak):
//! - `initialize` → server info + capabilities (`tools`), `ping` → `{}`,
//! - `notifications/initialized` → nothing (notifications never answered),
//! - `tools/list` → the registry specs (`name`/`description`/`inputSchema`),
//! - `tools/call` → `registry.dispatch(name, arguments)`; the registry's
//!   `{"error": ...}` JSON-string contract becomes `isError: true` content,
//!   exactly the Python server's "catch → `{"error", "tool"}` text" behavior,
//! - unknown method → JSON-RPC error `-32601`, malformed line → `-32700`.
//!
//! Run with `hermes --serve-mcp` (stdio) — logs must stay on stderr, stdout is
//! the MCP wire, like the Python `main()`.

use std::sync::Arc;

use anyhow::Result;
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use hermes_tools::registry::ToolRegistry;

use crate::jsonrpc::{JsonRpcError, PROTOCOL_VERSION};

/// Identity the server advertises in `initialize` (the Python
/// `HERMES_TOOLS_MCP_SERVER_NAME`).
pub const SERVER_NAME: &str = "hermes-tools";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Convert one dispatched registry result (always a JSON string; failures are
/// `{"error": ...}`) into the MCP `tools/call` result. The Python server
/// returns the error JSON as the tool's text output, so `isError` is flagged
/// for the caller while the text stays verbatim.
pub fn call_result(result: &str) -> Value {
    let is_error = serde_json::from_str::<Value>(result)
        .ok()
        .and_then(|v| {
            v.get("error")
                .map(|e| e.is_string() || e.is_object())
        })
        .unwrap_or(false);
    json!({
        "content": [{ "type": "text", "text": result }],
        "isError": is_error,
    })
}

/// Handle one decoded JSON-RPC message. `Ok(Some(response))` — a reply to
/// write back; `Ok(None)` — a notification (never answered). Requests that run
/// a tool await the registry dispatch, so the caller's runtime drives any
/// `spawn_blocking` hop (the multi-threaded CLI runtime, as in the REPL).
pub async fn handle_message(line: &str, registry: &ToolRegistry) -> Option<Value> {
    let msg: Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => {
            return Some(error_response(
                Value::Null,
                JsonRpcError {
                    code: -32700,
                    message: "Parse error".into(),
                    data: None,
                },
            ));
        }
    };
    // Requests carry an id; notifications never get a reply.
    let Some(id) = msg.get("id").cloned() else {
        return None;
    };
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": { "listChanged": false } },
            "serverInfo": { "name": SERVER_NAME, "version": SERVER_VERSION },
            "instructions": "Hermes Agent's tool surface, exposed for use inside another MCP client.",
        })),
        "ping" => Ok(json!({})),
        "tools/list" => {
            let tools: Vec<Value> = registry
                .specs()
                .iter()
                .map(|s| {
                    json!({
                        "name": s.name,
                        "description": s.description,
                        "inputSchema": s.schema,
                    })
                })
                .collect();
            Ok(json!({ "tools": tools, "nextCursor": Value::Null }))
        }
        "tools/call" => {
            let name = msg
                .pointer("/params/name")
                .and_then(Value::as_str)
                .unwrap_or("");
            let arguments = msg
                .pointer("/params/arguments")
                .cloned()
                .unwrap_or_else(|| json!({}));
            match registry.spec(name) {
                Some(_) => {
                    // dispatch always answers (failures are `{"error": ...}`
                    // strings) — it never panics on unknown tools or args.
                    let result = registry.dispatch(name, &arguments).await;
                    Ok(call_result(&result))
                }
                None => Err(JsonRpcError {
                    code: -32602,
                    message: format!("unknown tool: {name}"),
                    data: None,
                }),
            }
        }
        _ => Err(JsonRpcError {
            code: -32601,
            message: format!("Method not found: {method}"),
            data: None,
        }),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err(e) => error_response(id, e),
    })
}

fn error_response(id: Value, e: JsonRpcError) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": e.code,
            "message": e.message,
            "data": e.data,
        },
    })
}

/// Serve one connection: read one JSON-RPC message per line from `read`,
/// answer requests on `write`, until EOF. Generic over the stream so tests
/// drive it over an in-memory duplex instead of real stdio.
pub async fn serve<R, W>(read: R, mut write: W, registry: Arc<ToolRegistry>) -> Result<()>
where
    R: tokio::io::AsyncRead + Unpin,
    W: tokio::io::AsyncWrite + Unpin,
{
    let mut lines = BufReader::new(read).lines();
    while let Some(line) = lines.next_line().await? {
        if let Some(response) = handle_message(&line, &registry).await {
            write
                .write_all(response.to_string().as_bytes())
                .await?;
            write.write_all(b"\n").await?;
            write.flush().await?;
        }
    }
    Ok(())
}

/// Read one JSON-RPC message per line, answer requests, until EOF. Stdio entry
/// (`hermes --serve-mcp`): other MCP clients spawn this process and speak the
/// same wire the Python `hermes_tools_mcp_server` served.
pub async fn serve_stdio(registry: Arc<ToolRegistry>) -> Result<()> {
    serve(tokio::io::stdin(), tokio::io::stdout(), registry).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn registry() -> Arc<ToolRegistry> {
        Arc::new(ToolRegistry::new())
    }

    #[tokio::test]
    async fn initialize_advertises_tools_capability() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
            &registry(),
        )
        .await
        .unwrap();
        assert_eq!(resp["id"], 1);
        assert_eq!(resp["result"]["protocolVersion"], PROTOCOL_VERSION);
        assert_eq!(resp["result"]["serverInfo"]["name"], SERVER_NAME);
        assert!(resp["result"]["capabilities"]["tools"].is_object());
    }

    #[tokio::test]
    async fn notifications_are_never_answered() {
        assert!(handle_message(
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            &registry(),
        )
        .await
        .is_none());
    }

    #[tokio::test]
    async fn tools_list_exposes_the_full_registry() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            &registry(),
        )
        .await
        .unwrap();
        let tools = resp["result"]["tools"].as_array().unwrap();
        // The registry is the full parity set, not a curated subset.
        assert_eq!(tools.len(), registry().specs().len());
        assert!(tools.len() > 80, "registry closed at 89 tools");
        for t in tools {
            assert!(t["name"].is_string());
            assert!(t["description"].is_string());
            assert_eq!(t["inputSchema"]["type"], "object");
        }
        let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
        assert!(names.contains(&"read_file"));
        assert!(names.contains(&"web_search"));
    }

    #[tokio::test]
    async fn tools_call_round_trips_through_the_registry() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"Cargo.toml","limit":1}}}"#,
            &registry(),
        )
        .await
        .unwrap();
        assert_eq!(resp["id"], 3);
        assert_eq!(resp["result"]["isError"], false);
        let text = resp["result"]["content"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(parsed.is_object(), "content is the registry JSON string");
    }

    #[tokio::test]
    async fn tools_call_unknown_tool_is_a_jsonrpc_error() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":4,"method":"tools/call","params":{"name":"nope","arguments":{}}}"#,
            &registry(),
        )
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32602);
        assert!(resp["error"]["message"].as_str().unwrap().contains("nope"));
    }

    #[test]
    fn registry_errors_surface_as_is_error_content() {
        let result = call_result(r#"{"error": "boom", "tool": "x"}"#);
        assert_eq!(result["isError"], true);
        assert_eq!(result["content"][0]["text"], r#"{"error": "boom", "tool": "x"}"#);
        // A plain tool result stays non-error.
        assert_eq!(call_result(r#"{"listing": []}"#)["isError"], false);
    }

    #[tokio::test]
    async fn unknown_methods_and_garbage_are_jsonrpc_errors() {
        let resp = handle_message(
            r#"{"jsonrpc":"2.0","id":5,"method":"bogus"}"#,
            &registry(),
        )
        .await
        .unwrap();
        assert_eq!(resp["error"]["code"], -32601);
        let resp = handle_message("not json at all", &registry())
            .await
            .unwrap();
        assert_eq!(resp["error"]["code"], -32700);
    }

    /// Full stdio round-trip over an in-memory duplex: exactly the lines a
    /// foreign MCP client writes, one response per request line, none for the
    /// notification, then EOF ends the server cleanly.
    #[tokio::test]
    async fn stdio_round_trip() {
        let (mut client, server) = tokio::io::duplex(64 * 1024);
        let (read, write) = tokio::io::split(server);
        let served = tokio::spawn(async move { serve(read, write, registry()).await });

        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        client
            .write_all(
                concat!(
                    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
                    "\n",
                    r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"read_file","arguments":{"path":"Cargo.toml","limit":1}}}"#,
                    "\n",
                )
                .as_bytes(),
            )
            .await
            .unwrap();
        client.flush().await.unwrap();

        // Read until all three replies have arrived (bounded — no hangs).
        let mut out = String::new();
        let mut buf = vec![0u8; 64 * 1024];
        let deadline = std::time::Duration::from_secs(10);
        let read_all = tokio::time::timeout(deadline, async {
            loop {
                let n = client.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break;
                }
                out.push_str(&String::from_utf8_lossy(&buf[..n]));
                let replies = out.lines().filter(|l| !l.trim().is_empty()).count();
                if replies >= 3 {
                    break;
                }
            }
        });
        if tokio::time::timeout(deadline, read_all).await.is_err() {
            panic!("no complete round-trip within 10s: {out}");
        }

        let lines: Vec<Value> = out
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        // Exactly three replies — the notification was never answered.
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0]["id"], 1);
        assert_eq!(lines[0]["result"]["serverInfo"]["name"], "hermes-tools");
        assert_eq!(lines[1]["id"], 2);
        assert!(!lines[1]["result"]["tools"].as_array().unwrap().is_empty());
        assert_eq!(lines[2]["id"], 3);
        assert_eq!(lines[2]["result"]["isError"], false);

        // EOF (dropped client half) ends the server without error.
        client.shutdown().await.unwrap();
        drop(client);
        let finished = tokio::time::timeout(std::time::Duration::from_secs(5), served)
            .await
            .unwrap();
        assert!(finished.is_ok());
    }
}
