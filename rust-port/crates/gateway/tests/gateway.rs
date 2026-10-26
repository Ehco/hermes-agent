//! Integration tests: spawn the router with tokio, drive it with reqwest
//! against a `ScriptedTransport`-style stub (injected through the builder),
//! and assert the SSE stream shape, the error paths, and the live tool bridge.

use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use futures::Future;
use hermes_core::agent::ToolHost;
use hermes_core::test_transport::ScriptedTransport;
use hermes_core::transport::{StreamItem, Transport};
use hermes_gateway::{Gateway, RegistryHost};
use serde_json::{json, Value};

/// Serve `router` on an ephemeral loopback port; return its base URL.
async fn spawn_app(router: Router) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, router).await.unwrap();
    });
    format!("http://{addr}")
}

fn user_message(text: &str) -> Value {
    json!({ "role": "user", "content": [{ "type": "text", "text": text }] })
}

/// Parse an SSE body into `(event, data_json)` pairs.
fn parse_sse(body: &str) -> Vec<(String, Value)> {
    body.split("\n\n")
        .filter(|frame| !frame.trim().is_empty())
        .map(|frame| {
            let mut event = None;
            let mut data = None;
            for line in frame.lines() {
                if let Some(v) = line.strip_prefix("event: ") {
                    event = Some(v.to_string());
                } else if let Some(v) = line.strip_prefix("data: ") {
                    data = Some(v.to_string());
                }
            }
            (
                event.expect("frame without event:"),
                serde_json::from_str::<Value>(&data.expect("frame without data:")).unwrap(),
            )
        })
        .collect()
}

/// A ToolHost that must never be called; panics if the gateway routes a tool
/// call to it.
struct NeverHost;

impl ToolHost for NeverHost {
    fn execute(
        &self,
        _name: &str,
        _arguments: &str,
    ) -> impl Future<Output = anyhow::Result<String>> + Send {
        Box::pin(std::future::pending::<anyhow::Result<String>>())
    }
}

#[tokio::test]
async fn health_returns_ok_true() {
    let gw = Gateway::with_default_tools(Arc::new(ScriptedTransport::new(Vec::new())));
    let base = spawn_app(gw.router()).await;

    let res = reqwest::get(format!("{base}/health")).await.unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(res.json::<Value>().await.unwrap(), json!({ "ok": true }));

    // Same handler is served under the /v1 prefix too (Python route table).
    let res = reqwest::get(format!("{base}/v1/health")).await.unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn chat_streams_sse_with_documented_event_shape() {
    let transport = ScriptedTransport::new(vec![vec![
        StreamItem::TextDelta("Hel".into()),
        StreamItem::TextDelta("lo!".into()),
        StreamItem::Done,
    ]]);
    let gw = Gateway::new(Arc::new(transport), Arc::new(NeverHost));
    let base = spawn_app(gw.router()).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{base}/v1/chat"))
        .json(&json!({ "messages": [user_message("hi")] }))
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .unwrap();

    assert_eq!(res.status(), 200);
    // _sse_headers contract.
    assert_eq!(
        res.headers().get("content-type").unwrap(),
        "text/event-stream"
    );
    assert_eq!(res.headers().get("cache-control").unwrap(), "no-cache");

    let frames = parse_sse(&res.text().await.unwrap());
    let names: Vec<&str> = frames.iter().map(|(e, _)| e.as_str()).collect();
    assert_eq!(names, ["text_delta", "text_delta", "done"]);

    assert_eq!(frames[0].1, json!({ "type": "text_delta", "text": "Hel" }));
    assert_eq!(frames[1].1, json!({ "type": "text_delta", "text": "lo!" }));
    assert_eq!(frames[2].1, json!({ "type": "done", "text": "Hello!" }));
}

#[tokio::test]
async fn tool_bridge_executes_registry_tools_through_the_gateway() {
    let path = std::env::temp_dir().join(format!("hermes-gw-bridge-{}.txt", std::process::id()));
    let path_str = path.to_string_lossy().to_string();

    let transport = ScriptedTransport::new(vec![
        vec![
            StreamItem::TextDelta("Working.".into()),
            StreamItem::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: json!({ "path": path_str, "content": "bridge-ok" }).to_string(),
            },
            StreamItem::ToolCall {
                id: "c2".into(),
                name: "read_file".into(),
                arguments: json!({ "path": path_str }).to_string(),
            },
        ],
        vec![StreamItem::TextDelta("All set.".into())],
    ]);
    let gw = Gateway::with_default_tools(Arc::new(transport));
    let base = spawn_app(gw.router()).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{base}/v1/chat"))
        .json(&json!({
            "messages": [user_message("use the tools")],
            "tools": [],
        }))
        .timeout(Duration::from_secs(15))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let frames = parse_sse(&res.text().await.unwrap());
    let names: Vec<&str> = frames.iter().map(|(e, _)| e.as_str()).collect();
    // Tool calls stream as the model emits them; results follow in order.
    assert_eq!(
        names,
        [
            "text_delta",
            "tool_call",
            "tool_call",
            "tool_result",
            "tool_result",
            "text_delta",
            "done",
        ]
    );

    // Tool calls are streamed as issued.
    assert_eq!(
        frames[1].1,
        json!({
            "type": "tool_call",
            "id": "c1",
            "name": "write_file",
            "arguments": json!({ "path": path_str, "content": "bridge-ok" }).to_string(),
        })
    );

    // The registry executed the writes/reads for real.
    let (ev, data) = &frames[3];
    assert_eq!(ev, "tool_result");
    assert_eq!(data["call_id"], "c1");
    assert!(
        data["output"].as_str().unwrap().contains("success"),
        "got: {data}"
    );

    let (ev, data) = &frames[4];
    assert_eq!(ev, "tool_result");
    assert_eq!(data["call_id"], "c2");
    assert!(
        data["output"].as_str().unwrap().contains("bridge-ok"),
        "got: {data}"
    );

    assert_eq!(frames[6].1["text"], "All set.");

    let _ = std::fs::remove_file(&path);
}

#[tokio::test]
async fn unknown_path_returns_the_json_error_envelope() {
    let gw = Gateway::with_default_tools(Arc::new(ScriptedTransport::new(Vec::new())));
    let base = spawn_app(gw.router()).await;

    let res = reqwest::get(format!("{base}/definitely/not/here"))
        .await
        .unwrap();
    assert_eq!(res.status(), 404);
    let body = res.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("/definitely/not/here"));
}

#[tokio::test]
async fn chat_validates_the_request_body() {
    let gw = Gateway::with_default_tools(Arc::new(ScriptedTransport::new(Vec::new())));
    let base = spawn_app(gw.router()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/chat");

    // Not JSON at all.
    let res = client.post(&url).body("not json").send().await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"]["message"],
        "Request body must be valid JSON."
    );

    // JSON but not an object.
    let res = client.post(&url).json(&json!([1, 2])).send().await.unwrap();
    assert_eq!(res.status(), 400);
    assert_eq!(
        res.json::<Value>().await.unwrap()["error"]["message"],
        "Request body must be a JSON object."
    );

    // Missing messages.
    let res = client
        .post(&url)
        .json(&json!({ "tools": [] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    let body = res.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["message"], "Missing 'messages' field");
    assert_eq!(body["error"]["code"], "missing_messages");

    // Bad message shape.
    let res = client
        .post(&url)
        .json(&json!({ "messages": [{}] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400);
    assert!(res.json::<Value>().await.unwrap()["error"]["message"]
        .as_str()
        .unwrap()
        .starts_with("Invalid messages payload"));
}

#[tokio::test]
async fn oversized_request_body_is_rejected_with_413() {
    let transport = ScriptedTransport::new(Vec::new());
    let gw = Gateway::builder(Arc::new(transport), Arc::new(RegistryHost::default()))
        .max_body_bytes(64)
        .build();
    let base = spawn_app(gw.router()).await;

    let client = reqwest::Client::new();
    let res = client
        .post(format!("{base}/v1/chat"))
        .json(&json!({ "messages": [user_message(&"x".repeat(200))] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 413);
    let body = res.json::<Value>().await.unwrap();
    assert_eq!(body["error"]["message"], "Request body too large.");
    assert_eq!(body["error"]["code"], "body_too_large");
}

#[tokio::test]
async fn invalid_content_length_is_rejected_with_400() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt;

    let gw = Gateway::with_default_tools(Arc::new(ScriptedTransport::new(Vec::new())));
    let app = gw.router();

    // Drive the router directly (no hyper): hyper pre-validates Content-Length
    // with a bare 400, so the middleware's own 400 must be asserted at the
    // router level.
    let req = Request::builder()
        .method("POST")
        .uri("/v1/chat")
        .header("Content-Type", "application/json")
        .header("Content-Length", "abc")
        .body(Body::from("{}"))
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), 400);
    let body = serde_json::from_slice::<Value>(
        &axum::body::to_bytes(res.into_body(), 8192).await.unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["message"], "Invalid Content-Length header.");
    assert_eq!(body["error"]["code"], "invalid_content_length");
}

#[tokio::test]
async fn origin_guard_and_cors_headers() {
    let gw = Gateway::builder(
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(RegistryHost::default()),
    )
    .cors_origins(["https://good.example"])
    .build();
    let base = spawn_app(gw.router()).await;
    let client = reqwest::Client::new();
    let url = format!("{base}/v1/chat");

    // Non-browser client (no Origin): allowed, no CORS headers.
    let res = client
        .post(&url)
        .json(&json!({ "messages": [user_message("hi")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert!(res.headers().get("access-control-allow-origin").is_none());

    // Configured browser origin: allowed, echo CORS headers (incl. Vary: Origin).
    let res = client
        .post(&url)
        .header("Origin", "https://good.example")
        .json(&json!({ "messages": [user_message("hi")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers().get("access-control-allow-origin").unwrap(),
        "https://good.example"
    );
    assert_eq!(res.headers().get("vary").unwrap(), "Origin");

    // Unconfigured browser origin: bare 403.
    let res = client
        .post(&url)
        .header("Origin", "https://evil.example")
        .json(&json!({ "messages": [user_message("hi")] }))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    assert!(res.headers().get("access-control-allow-origin").is_none());

    // Preflight with an allowed origin: 200 + the _CORS_HEADERS base set.
    let res = client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Origin", "https://good.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    assert_eq!(
        res.headers().get("access-control-allow-methods").unwrap(),
        "GET, POST, DELETE, OPTIONS"
    );

    // Preflight with a disallowed origin: 403.
    let res = client
        .request(reqwest::Method::OPTIONS, &url)
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn security_headers_are_applied_to_every_response() {
    let gw = Gateway::with_default_tools(Arc::new(ScriptedTransport::new(Vec::new())));
    let base = spawn_app(gw.router()).await;

    for path in ["/health", "/no/such/path"] {
        let res = reqwest::get(format!("{base}{path}")).await.unwrap();
        let headers = res.headers().clone();
        assert_eq!(
            headers.get("x-content-type-options").unwrap(),
            "nosniff",
            "{path}"
        );
        assert_eq!(headers.get("x-frame-options").unwrap(), "DENY", "{path}");
        assert!(
            headers
                .get("content-security-policy")
                .unwrap()
                .to_str()
                .unwrap()
                .contains("frame-ancestors"),
            "{path}"
        );
    }
}

/// Guard: the injected transport is actually the one the handler drives —
/// a second scripted turn is consumed when the client asks again.
#[tokio::test]
async fn chat_uses_the_injected_transport_per_request() {
    let transport = ScriptedTransport::new(vec![
        vec![StreamItem::TextDelta("first".into()), StreamItem::Done],
        vec![StreamItem::TextDelta("second".into()), StreamItem::Done],
    ]);
    let gw = Gateway::new(Arc::new(transport), Arc::new(NeverHost));
    let base = spawn_app(gw.router()).await;
    let client = reqwest::Client::new();

    for expected in ["first", "second"] {
        let res = client
            .post(format!("{base}/v1/chat"))
            .json(&json!({ "messages": [user_message("again")] }))
            .send()
            .await
            .unwrap();
        let frames = parse_sse(&res.text().await.unwrap());
        assert_eq!(frames[0].1["text"], expected);
        assert_eq!(frames.last().unwrap().0, "done");
    }
}
