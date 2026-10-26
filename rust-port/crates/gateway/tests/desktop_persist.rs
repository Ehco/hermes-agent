//! Transcript persistence: desktop sessions survive a simulated restart via
//! the hermes-session JSONL store. Round-trip (turn → reload → resume history
//! matches), restored rows in `GET /api/sessions` with on-disk counts, and the
//! reconnect replay contract across a reload (a fresh epoch forces the
//! client's `session.events.since` to report `snapshot_required`).

use std::sync::Arc;
use std::time::Duration;

use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};

use hermes_core::test_transport::ScriptedTransport;
use hermes_core::transport::StreamItem;
use hermes_gateway::{DesktopApi, RegistryHost};

/// Serve the desktop router on an ephemeral loopback port; returns http:// URL.
async fn spawn_app<T, H>(api: &DesktopApi<T, H>) -> String
where
    T: hermes_core::transport::Transport + Send + Sync + 'static,
    H: hermes_core::agent::ToolHost + Send + Sync + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn ws_url(base: &str) -> String {
    format!("{}/api/ws", base.replace("http://", "ws://"))
}

/// One scripted assistant reply of `text`.
fn reply_transport(text: &str) -> Arc<ScriptedTransport> {
    Arc::new(ScriptedTransport::new(vec![vec![
        StreamItem::TextDelta(text.into()),
        StreamItem::Done,
    ]]))
}

fn fresh_store_dir(tag: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermes-gw-persist-{tag}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos(),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// Connect + create one session + submit a prompt, draining events until the
/// turn completes; returns the session id and the snapshot reply.
async fn run_one_turn(base: &str, text: &str) -> (String, Value) {
    let (mut ws, _) = tokio_tungstenite::connect_async(ws_url(base))
        .await
        .unwrap();
    use tokio_tungstenite::tungstenite::Message;
    // Drain gateway.ready.
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("method").and_then(Value::as_str) == Some("event") {
            break;
        }
    }
    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session.create", "params": {} }).to_string(),
    ))
    .await
    .unwrap();
    let mut snap = None;
    for _ in 0..20 {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("id") == Some(&json!(1)) {
            snap = Some(frame["result"].clone());
            break;
        }
    }
    let snap = snap.expect("no session.create reply");
    let sid = snap["session_id"].as_str().unwrap().to_string();

    ws.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 2, "method": "prompt.submit",
                "params": { "session_id": sid, "text": text } })
        .to_string(),
    ))
    .await
    .unwrap();
    // Drain until message.complete (the turn is then fully persisted).
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    while tokio::time::Instant::now() < deadline {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("method").and_then(Value::as_str) == Some("event")
            && frame["params"]["type"] == "message.complete"
        {
            break;
        }
    }
    (sid, snap)
}

#[tokio::test]
async fn transcript_round_trip_through_store_reload() {
    let dir = fresh_store_dir("roundtrip");

    // Instance 1: create a session, run one turn, learn the ids.
    let api1 = DesktopApi::new(
        reply_transport("Persisted reply"),
        Arc::new(RegistryHost::default()),
    )
    .with_store_dir(dir.clone());
    let base1 = spawn_app(&api1).await;
    let (sid, _snap) = run_one_turn(&base1, "Persisted question").await;
    let stored_id = api1
        .inner
        .registry
        .lock()
        .unwrap()
        .get(&sid)
        .map(|s| s.stored_id.clone())
        .unwrap();

    // The transcript file exists on disk with both sides of the exchange.
    let file = dir.join(format!("{stored_id}.jsonl"));
    let on_disk = std::fs::read_to_string(&file).unwrap();
    assert_eq!(
        on_disk.lines().count(),
        2,
        "user + assistant line: {on_disk}"
    );

    // Instance 2: a "restarted" server over the SAME store dir.
    let api2 = DesktopApi::new(reply_transport("Later"), Arc::new(RegistryHost::default()))
        .with_store_dir(dir.clone());
    let base2 = spawn_app(&api2).await;

    // The restored session is listed with its on-disk message count.
    let listed: Value = reqwest::Client::new()
        .get(format!("{base2}/api/sessions"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(listed["total"], json!(1), "restored session must be listed");
    let row = &listed["sessions"][0];
    assert_eq!(row["id"], json!(sid));
    assert_eq!(row["message_count"], json!(2));
    assert_eq!(row["preview"], json!("Persisted reply"));

    // Resume over WS: the snapshot replays the stored transcript exactly.
    let (mut ws2, _) = tokio_tungstenite::connect_async(ws_url(&base2))
        .await
        .unwrap();
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("method").and_then(Value::as_str) == Some("event") {
            break;
        }
    }
    ws2.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session.resume",
                "params": { "session_id": sid } })
        .to_string(),
    ))
    .await
    .unwrap();
    let mut resumed = None;
    for _ in 0..20 {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("id") == Some(&json!(1)) {
            resumed = Some(frame["result"].clone());
            break;
        }
    }
    let resumed = resumed.expect("no session.resume reply");
    let messages = resumed["messages"].as_array().unwrap();
    assert_eq!(
        messages.len(),
        2,
        "resume must replay the stored transcript"
    );
    assert_eq!(messages[0]["role"], json!("user"));
    assert_eq!(
        messages[0]["content"][0]["text"],
        json!("Persisted question")
    );
    assert_eq!(messages[1]["role"], json!("assistant"));
    assert_eq!(messages[1]["content"][0]["text"], json!("Persisted reply"));
    assert_eq!(resumed["message_count"], json!(2));
    // The stored-id alias resolves too (the client's remembered id).
    assert_eq!(resumed["stored_session_id"], json!(stored_id));

    // The next turn continues on top of the restored history: the runtime
    // holds the reloaded transcript, not a fresh one.
    let history_sent = {
        let registry = api2.inner.registry.lock().unwrap();
        let session = registry.get(&sid).cloned().unwrap();
        let len = session.history.lock().unwrap().len();
        len
    };
    assert_eq!(history_sent, 2);
}

#[tokio::test]
async fn reconnect_replay_after_reload_reports_snapshot_required() {
    let dir = fresh_store_dir("replay");

    let api1 = DesktopApi::new(
        reply_transport("Hi again"),
        Arc::new(RegistryHost::default()),
    )
    .with_store_dir(dir.clone());
    let base1 = spawn_app(&api1).await;
    let (sid, snap1) = run_one_turn(&base1, "hello").await;
    let old_epoch = snap1["replay_epoch"].as_str().unwrap().to_string();

    // Reload: a fresh server over the same store dir has a FRESH epoch and an
    // empty replay ring — a reconnecting client must be told to re-resume.
    let api2 = DesktopApi::new(reply_transport("Later"), Arc::new(RegistryHost::default()))
        .with_store_dir(dir.clone());
    let base2 = spawn_app(&api2).await;
    let (mut ws2, _) = tokio_tungstenite::connect_async(ws_url(&base2))
        .await
        .unwrap();
    use tokio_tungstenite::tungstenite::Message;
    loop {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("method").and_then(Value::as_str) == Some("event") {
            break;
        }
    }
    ws2.send(Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session.events.since",
                "params": { "session_id": sid, "last_seen": 0, "replay_epoch": old_epoch } })
        .to_string(),
    ))
    .await
    .unwrap();
    let mut replay = None;
    for _ in 0..20 {
        let msg = tokio::time::timeout(Duration::from_secs(10), ws2.next())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        let frame: Value = serde_json::from_str(&match msg {
            Message::Text(t) => t,
            _ => continue,
        })
        .unwrap();
        if frame.get("id") == Some(&json!(1)) {
            replay = Some(frame["result"].clone());
            break;
        }
    }
    let replay = replay.expect("no session.events.since reply");
    assert_ne!(
        replay["replay_epoch"].as_str().unwrap(),
        old_epoch,
        "a reload mints a new epoch"
    );
    assert_eq!(replay["truncated"], json!(true));
    assert_eq!(replay["snapshot_required"], json!(true));
    assert_eq!(
        replay["events"].as_array().unwrap().len(),
        0,
        "no live ring survives a restart"
    );
}

#[tokio::test]
async fn delete_removes_the_transcript_file() {
    let dir = fresh_store_dir("delete");
    let api1 = DesktopApi::new(reply_transport("bye"), Arc::new(RegistryHost::default()))
        .with_store_dir(dir.clone());
    let base = spawn_app(&api1).await;
    let (sid, _snap) = run_one_turn(&base, "one").await;
    let stored_id = api1
        .inner
        .registry
        .lock()
        .unwrap()
        .get(&sid)
        .map(|s| s.stored_id.clone())
        .unwrap();
    let file = dir.join(format!("{stored_id}.jsonl"));
    assert!(file.exists());

    let deleted: Value = reqwest::Client::new()
        .delete(format!("{base}/api/sessions/{sid}"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(deleted["ok"], json!(true));
    assert!(!file.exists(), "DELETE must remove the JSONL transcript");
}
