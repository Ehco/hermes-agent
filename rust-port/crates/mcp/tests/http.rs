//! Integration tests for the Streamable HTTP transport: a handcrafted local
//! HTTP server (std::net only — nothing leaves the loopback) answers the
//! JSON-RPC POSTs the way a Streamable HTTP MCP endpoint does.

use hermes_mcp::McpClient;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::time::Duration;

/// Canned endpoint: replies to each POST with the next entry of `script`
/// (a JSON body for requests, an empty 202 for notifications), capturing
/// every request body for request-shape assertions. Adds `Mcp-Session-Id`
/// on the first request-bearing reply.
fn spawn_mock_http(script: Vec<Value>) -> (String, std::sync::mpsc::Receiver<Value>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind loopback");
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut script = script.into_iter();
        // Enough accepts for every connection the client opens. Each
        // connection may carry several requests (HTTP keep-alive), so loop
        // reading until the peer closes or the read fails.
        for _ in 0..64 {
            let (mut stream, _) = match listener.accept() {
                Ok(s) => s,
                Err(_) => break,
            };
            loop {
                let (msg, is_notification) = match read_request(&mut stream) {
                    Some(m) => m,
                    None => break,
                };
                let _ = tx.send(msg.clone());
                let (status, body) = match msg.get("method").and_then(Value::as_str) {
                    Some("notifications/initialized") => ("202 Accepted", String::new()),
                    _ => {
                        // Echo the request's id back: a real JSON-RPC server
                        // answers with the id the request carried, whatever
                        // the canned body's script author guessed.
                        let mut body = script
                            .next()
                            .unwrap_or(json!({"jsonrpc":"2.0","id":0,"result":{}}));
                        if let Some(id) = msg.get("id").cloned() {
                            body["id"] = id;
                        }
                        ("200 OK", body.to_string())
                    }
                };
                let extra = if is_notification {
                    String::new()
                } else {
                    "Mcp-Session-Id: sess-42\r\n".to_string()
                };
                let http = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\n\r\n{}",
                    body.len(),
                    body
                );
                let _ = stream.write_all(http.as_bytes());
                let _ = stream.flush();
            }
        }
    });
    (format!("http://127.0.0.1:{port}/mcp"), rx)
}

/// Read one HTTP/1.1 request (headers + content-length body); return the
/// parsed JSON body and whether it is a notification (no id).
fn read_request(stream: &mut std::net::TcpStream) -> Option<(Value, bool)> {
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 4096];
    let mut headers_end = 0usize;
    let mut content_length = 0usize;
    loop {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&chunk[..n]);
        let text = String::from_utf8_lossy(&buf);
        if let Some(pos) = text.find("\r\n\r\n") {
            if headers_end == 0 {
                headers_end = pos + 4;
                content_length = text
                    .lines()
                    .find_map(|l| {
                        let l = l.to_ascii_lowercase();
                        l.strip_prefix("content-length:")
                            .and_then(|v| v.trim().parse::<usize>().ok())
                    })
                    .unwrap_or(0);
            }
            if buf.len() >= headers_end + content_length {
                break;
            }
        }
    }
    if headers_end == 0 {
        return None;
    }
    let body = String::from_utf8_lossy(&buf[headers_end..]).to_string();
    let msg: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let is_notification = msg.get("id").is_none();
    Some((msg, is_notification))
}

#[tokio::test(flavor = "multi_thread")]
async fn http_handshake_list_call_ping_round_trip() {
    let (url, rx) = spawn_mock_http(vec![
        json!({"jsonrpc":"2.0","id":1,"result":{"protocolVersion":"2025-03-26","capabilities":{"tools":{}},"serverInfo":{"name":"http-mock","version":"1"}}}),
        json!({"jsonrpc":"2.0","id":3,"result":{"tools":[{"name":"greeter","description":"says hi","inputSchema":{"type":"object","properties":{}}}]}}),
        json!({"jsonrpc":"2.0","id":4,"result":{"content":[{"type":"text","text":"hi from http"}]}}),
        json!({"jsonrpc":"2.0","id":5,"result":{}}),
    ]);
    let mut client = McpClient::http(&url, HashMap::new())
        .unwrap()
        .with_request_timeout(Duration::from_secs(15));
    client
        .handshake()
        .await
        .expect("handshake over streamable http");
    assert_eq!(client.negotiated_version().unwrap(), "2025-03-26");

    let tools = client.list_tools().await.expect("tools/list over http");
    assert_eq!(tools[0].name, "greeter");

    let result = client
        .call_tool("greeter", &json!({}))
        .await
        .expect("tools/call over http");
    assert_eq!(result.content[0].text.as_deref(), Some("hi from http"));
    client.ping().await.expect("ping over http");

    // Request-shape assertions on what the server actually received.
    let init = rx.recv().unwrap();
    assert_eq!(init["jsonrpc"], "2.0");
    assert_eq!(init["method"], "initialize");
    assert_eq!(init["params"]["protocolVersion"], "2025-03-26");
    assert_eq!(init["params"]["clientInfo"]["name"], "hermes-agent");
    let initialized = rx.recv().unwrap();
    assert_eq!(initialized["method"], "notifications/initialized");
    let list = rx.recv().unwrap();
    assert_eq!(list["method"], "tools/list");
    let call = rx.recv().unwrap();
    assert_eq!(call["method"], "tools/call");
    assert_eq!(call["params"]["name"], "greeter");
    let ping = rx.recv().unwrap();
    assert_eq!(ping["method"], "ping");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn http_jsonrpc_error_is_surfaced() {
    let (url, _rx) = spawn_mock_http(vec![
        json!({"jsonrpc":"2.0","id":1,"error":{"code":-32000,"message":"handshake refused"}}),
    ]);
    let mut client = McpClient::http(&url, HashMap::new())
        .unwrap()
        .with_request_timeout(Duration::from_secs(15));
    let err = client.handshake().await.expect_err("server refused");
    assert!(err.to_string().contains("-32000"), "got: {err}");
    client.shutdown().await;
}
