//! Protocol tests for the desktop `/api/ws` surface. The assertions are the
//! canned server shapes from the desktop UI's own fixtures
//! (`apps/desktop/src/api/canonical-*.test.ts` + `apps/shared/src/
//! gateway-contract.generated.ts`): the `gateway.ready` notification, the
//! `CanonicalSnapshot` keys, `message.delta` / `message.complete` envelopes,
//! `session.events.since` replay, and the `revision_conflict` refusal the
//! client's `protocol.failure` keys on.

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio_tungstenite::tungstenite::Message;

use hermes_core::agent::ToolHost;
use hermes_core::test_transport::ScriptedTransport;
use hermes_core::transport::StreamItem;
use hermes_gateway::{DesktopApi, RegistryHost};

/// A tool host that always answers `ok` (the tests never hit the registry).
struct EchoHost;

impl ToolHost for EchoHost {
    fn execute(
        &self,
        _name: &str,
        _arguments: &str,
    ) -> impl std::future::Future<Output = anyhow::Result<String>> + Send {
        Box::pin(async { Ok("ok".to_string()) })
    }
}

fn text_tool() -> Arc<ScriptedTransport> {
    Arc::new(ScriptedTransport::new(vec![
        vec![
            StreamItem::ToolCall {
                id: "call-1".into(),
                name: "terminal".into(),
                arguments: "{}".into(),
            },
            StreamItem::Done,
        ],
        vec![StreamItem::TextDelta("done".into()), StreamItem::Done],
    ]))
}

/// Serve the desktop router on an ephemeral loopback port; returns ws:// URL.
async fn spawn_app<T, H>(api: &DesktopApi<T, H>) -> String
where
    T: hermes_core::transport::Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("ws://{addr}/api/ws")
}

/// Connect a raw WS client (the desktop's `native_dial` shape: canonical
/// subprotocol + ticket in the query string).
async fn connect(
    url: &str,
) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await.unwrap();
    ws
}

/// Next server frame (skipping pings), parsed as JSON.
async fn next_frame(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
        .await
        .expect("timed out waiting for a frame")
        .expect("socket closed")
        .unwrap();
    serde_json::from_str(&match msg {
        Message::Text(text) => text,
        other => panic!("unexpected frame: {other:?}"),
    })
    .unwrap_or_else(|e| panic!("bad JSON frame: {e}"))
}

/// Next `event` notification (`method: "event"`).
async fn next_event(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
) -> Value {
    loop {
        let frame = next_frame(ws).await;
        if frame.get("method").and_then(Value::as_str) == Some("event") {
            return frame["params"].clone();
        }
    }
}

/// Next JSON-RPC reply for `id` (events interleave on the wire).
async fn next_result(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
) -> Value {
    loop {
        let frame = next_frame(ws).await;
        if frame.get("id") == Some(&json!(id)) && !frame.get("result").is_none() {
            return frame["result"].clone();
        }
        if frame.get("id") == Some(&json!(id)) && frame.get("error").is_some() {
            return frame.clone();
        }
    }
}

async fn request(
    ws: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    method: &str,
    params: Value,
) -> Value {
    let req = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
    ws.send(Message::Text(req.to_string())).await.unwrap();
    next_result(ws, id).await
}

fn api() -> DesktopApi<ScriptedTransport, RegistryHost> {
    DesktopApi::new(
        Arc::new(ScriptedTransport::new(vec![vec![
            StreamItem::TextDelta("Hello ".into()),
            StreamItem::TextDelta("world".into()),
            StreamItem::Done,
        ]])),
        Arc::new(RegistryHost::default()),
    )
}

#[tokio::test]
async fn ready_snapshot_ping_and_turn_stream() {
    let api = api();
    let url = spawn_app(&api).await;
    let mut ws = connect(&url).await;

    // gateway.ready arrives right after accept (Python `handle_ws`).
    let ready = next_event(&mut ws).await;
    assert_eq!(ready["type"], "gateway.ready");
    assert_eq!(ready["payload"]["heartbeat"], json!(true));
    assert!(ready["payload"]["replay_epoch"].is_string());

    // session.create answers the canonical snapshot (fixture:
    // canonical-attach-snapshot.test.ts asserts these exact keys).
    let snap = request(&mut ws, 1, "session.create", json!({})).await;
    for key in [
        "session_id",
        "stored_session_id",
        "messages",
        "revision",
        "execution_generation",
        "running",
        "prompts",
        "replay_epoch",
        "last_sequence",
    ] {
        assert!(snap.get(key).is_some(), "snapshot missing {key}");
    }
    assert_eq!(snap["info"]["desktop_protocol"], "hermes-gateway-v1");

    // Heartbeat answered inline.
    let pong = request(&mut ws, 2, "gateway.ping", json!({})).await;
    assert_eq!(pong, json!({ "ok": true }));

    // Setup readiness: both probes answer even without a configured provider
    // (the client's onboarding overlay holds until `configured` is a bool).
    let status = request(&mut ws, 3, "setup.status", json!({})).await;
    assert_eq!(status["provider_configured"], false);
    let check = request(&mut ws, 4, "setup.runtime_check", json!({})).await;
    assert_eq!(check["ok"], false);
    assert!(check["error"].is_string());
    assert!(check["provider"].is_string());

    // prompt.submit → the turn streams as events, bracketed by
    // session.updated {running} and closed by message.complete. The receipt is
    // the canonical AdmissionReceipt: the client validates ref.session_id.
    let receipt = request(
        &mut ws,
        5,
        "prompt.submit",
        json!({ "session_id": snap["session_id"], "text": "hi" }),
    )
    .await;
    assert_eq!(receipt["status"], "started");
    assert!(receipt["admission_id"].is_string());
    assert_eq!(receipt["ref"]["session_id"], snap["session_id"]);
    assert_eq!(receipt["ref"]["profile_id"], "default");
    assert_eq!(receipt["authority_epoch"], 1);

    let mut deltas = Vec::new();
    let mut completed = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let event =
            tokio::time::timeout(deadline - tokio::time::Instant::now(), next_event(&mut ws)).await;
        let event = match event {
            Ok(e) => e,
            Err(_) => break,
        };
        match event["type"].as_str().unwrap() {
            "message.delta" => {
                if let Some(text) = event["payload"]["text"].as_str() {
                    deltas.push(text.to_string());
                }
            }
            "message.complete" => {
                completed = Some(event["payload"]["text"].clone());
                break;
            }
            _ => {}
        }
    }
    assert_eq!(deltas, vec!["Hello ", "world"]);
    assert_eq!(completed.unwrap(), json!("Hello world"));
}

#[tokio::test]
async fn reconnect_replay_carries_stamped_events() {
    let api = api();
    let url = spawn_app(&api).await;
    let mut ws = connect(&url).await;
    next_event(&mut ws).await; // gateway.ready

    let snap = request(&mut ws, 1, "session.create", json!({})).await;
    let sid = snap["session_id"].clone();
    let epoch = snap["replay_epoch"].clone();
    let _ = request(
        &mut ws,
        2,
        "prompt.submit",
        json!({ "session_id": sid, "text": "hi" }),
    )
    .await;

    // Drain live frames until the turn completes (message.complete).
    loop {
        let event = next_event(&mut ws).await;
        if event["type"] == "message.complete" {
            break;
        }
    }

    // A reconnecting client replays through session.events.since: every frame
    // carries the per-session seq + replay_epoch (the client's seq watermark
    // contract in apps/shared/src/json-rpc-gateway.ts).
    let replay = request(
        &mut ws,
        3,
        "session.events.since",
        json!({ "session_id": sid, "last_seen": 0, "replay_epoch": epoch }),
    )
    .await;
    assert_eq!(replay["replay_epoch"], epoch);
    assert_eq!(replay["truncated"], json!(false));
    let events = replay["events"].as_array().unwrap();
    assert!(
        !events.is_empty(),
        "expected stamped events in the replay ring"
    );
    let mut last = 0;
    for event in events {
        let seq = event["seq"].as_u64().unwrap();
        assert!(seq > last, "seq must advance monotonically");
        last = seq;
        assert_eq!(event["session_id"], sid);
        assert_eq!(event["replay_epoch"], epoch);
    }
    assert_eq!(replay["latest_seq"], json!(last));
}

#[tokio::test]
async fn mutate_revision_conflict_then_success() {
    let api = api();
    let url = spawn_app(&api).await;
    let mut ws = connect(&url).await;
    next_event(&mut ws).await;

    let snap = request(&mut ws, 1, "session.create", json!({})).await;
    let sid = snap["session_id"].clone();
    let revision = snap["revision"].as_u64().unwrap();

    // A stale expected_revision refuses with the typed reason + fences.
    let refused = request(
        &mut ws,
        2,
        "session.mutate",
        json!({ "session_id": sid, "operation": "rename", "payload": {"title": "x"},
               "expected_revision": revision + 7, "request_id": "r1" }),
    )
    .await;
    assert_eq!(refused["error"]["data"]["reason"], "revision_conflict");
    assert_eq!(refused["error"]["data"]["revision"], json!(revision));

    // The correct revision commits and bumps the CAS value.
    let receipt = request(
        &mut ws,
        3,
        "session.mutate",
        json!({ "session_id": sid, "operation": "rename", "payload": {"title": "New"},
               "expected_revision": revision, "request_id": "r2" }),
    )
    .await;
    assert_eq!(receipt["revision"], json!(revision + 1));
}

#[tokio::test]
async fn approval_prompt_lifecycle() {
    // ask-mode session: the tool call raises approval.request, answered by
    // approval.respond (fixture: canonical-clarify-prompt.test.ts lifecycle —
    // request event keyed by prompt_id, respond RPC, settled event).
    let api = DesktopApi::new(text_tool(), Arc::new(RegistryHost::default()));
    let url = spawn_app(&api).await;
    let mut ws = connect(&url).await;
    next_event(&mut ws).await;

    let snap = request(
        &mut ws,
        1,
        "session.create",
        json!({ "approval_mode": "ask" }),
    )
    .await;
    let sid = snap["session_id"].clone();
    let _ = request(
        &mut ws,
        2,
        "prompt.submit",
        json!({ "session_id": sid, "text": "run it" }),
    )
    .await;

    // The approval request event carries the canonical pending-prompt shape.
    let mut prompt = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let event = next_event(&mut ws).await;
        if event["type"] == "approval.request" {
            prompt = Some(event["payload"].clone());
            break;
        }
    }
    let prompt = prompt.expect("approval.request never arrived");
    let prompt_id = prompt["prompt_id"].as_str().unwrap().to_string();
    assert_eq!(prompt["choices"], json!(["allow", "always", "deny"]));

    let receipt = request(
        &mut ws,
        3,
        "approval.respond",
        json!({ "session_id": sid, "prompt_id": prompt_id, "choice": "allow" }),
    )
    .await;
    assert_eq!(receipt["status"], "resolved");

    // The gate passes → tool runs → tool.complete → eventual message.complete.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut saw_tool_complete = false;
    while tokio::time::Instant::now() < deadline {
        let event = next_event(&mut ws).await;
        if event["type"] == "tool.complete" {
            saw_tool_complete = true;
            break;
        }
    }
    assert!(
        saw_tool_complete,
        "tool.complete never arrived after approval"
    );
}

#[tokio::test]
async fn ticket_auth_single_use_and_reject() {
    let api = api();
    let url = spawn_app(&api).await;
    let mut ws = connect(&url).await;
    next_event(&mut ws).await;

    // Mint over REST (the desktop's `POST /api/auth/ws-ticket` → getWsTicket).
    let http = format!(
        "http://{}",
        url.trim_start_matches("ws://").split('/').next().unwrap()
    );
    let minted: Value = reqwest::Client::new()
        .post(format!("{http}/api/auth/ws-ticket"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ticket = minted["ticket"].as_str().unwrap().to_string();

    // First upgrade consumes it.
    let ticketed = format!("{url}?native_dial=1&ticket={ticket}");
    let (mut ws2, _) = tokio_tungstenite::connect_async(&ticketed).await.unwrap();
    next_event(&mut ws2).await;

    // Second upgrade with the SAME ticket is refused (one-use): the handshake
    // answers HTTP 401 instead of upgrading.
    let reused = tokio_tungstenite::connect_async(&ticketed).await;
    match reused {
        Err(err) => {
            let status = match &err {
                tokio_tungstenite::tungstenite::Error::Http(resp) => Some(resp.status()),
                _ => None,
            };
            assert_eq!(status, Some(reqwest::StatusCode::UNAUTHORIZED));
        }
        Ok(_) => panic!("a consumed ticket must not upgrade twice"),
    }
}
