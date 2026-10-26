//! REST tests for the desktop dashboard surface (`apps/desktop/src/api/*.ts`
//! helpers): status, the sessions sidebar, skills, MCP config and the
//! ws-ticket mint — all driven with plain reqwest against the same router.

use std::sync::Arc;

use serde_json::{json, Value};

use hermes_core::test_transport::ScriptedTransport;
use hermes_core::transport::StreamItem;
use hermes_gateway::{DesktopApi, RegistryHost};

async fn spawn_app(api: &DesktopApi<ScriptedTransport, RegistryHost>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn api(skills_root: Option<std::path::PathBuf>) -> DesktopApi<ScriptedTransport, RegistryHost> {
    let base = DesktopApi::new(
        Arc::new(ScriptedTransport::new(vec![vec![StreamItem::Done]])),
        Arc::new(RegistryHost::default()),
    );
    match skills_root {
        Some(dir) => base.with_skills_root(dir),
        None => base,
    }
}

fn http() -> reqwest::Client {
    reqwest::Client::new()
}

#[tokio::test]
async fn status_reports_ok_and_version() {
    let api = api(None);
    let base = spawn_app(&api).await;
    let status: Value = http().get(format!("{base}/api/status")).send().await.unwrap().json().await.unwrap();
    assert_eq!(status["ok"], json!(true));
    assert!(status["version"].is_string());
    assert!(status["replay_epoch"].is_string());
}

#[tokio::test]
async fn sessions_list_rows_and_delete() {
    let api = api(None);
    let base = spawn_app(&api).await;
    let client = http();

    // Create one session over the WS surface (the sidebar lists REST rows).
    let url = format!("{}/api/ws", base.replace("http://", "ws://"));
    let (mut ws, _) = tokio_tungstenite::connect_async(&url).await.unwrap();
    use futures::{SinkExt, StreamExt};
    ws.send(tokio_tungstenite::tungstenite::Message::Text(
        json!({ "jsonrpc": "2.0", "id": 1, "method": "session.create", "params": { "title": "Side chat" } }).to_string(),
    ))
    .await
    .unwrap();
    // Drain until the reply for id 1.
    let mut snap = None;
    for i in 0..10 {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ws.next()).await;
        let msg = match msg {
            Ok(Some(Ok(m))) => m,
            _ => break,
        };
        let frame: Value = serde_json::from_str(&match msg {
            tokio_tungstenite::tungstenite::Message::Text(t) => t,
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

    // GET /api/sessions lists the row with the fields the sidebar renders.
    let listed: Value = client.get(format!("{base}/api/sessions")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["total"], json!(1));
    let row = &listed["sessions"][0];
    assert_eq!(row["session_id"], json!(sid));
    assert_eq!(row["title"], json!("Side chat"));
    for key in ["created_at", "last_active_at", "message_count", "preview", "archived", "is_active", "profile"] {
        assert!(row.get(key).is_some(), "session row missing {key}");
    }

    // GET /api/sessions/{id} and DELETE.
    let one: Value = client.get(format!("{base}/api/sessions/{sid}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(one["session_id"], json!(sid));
    let deleted: Value = client.delete(format!("{base}/api/sessions/{sid}")).send().await.unwrap().json().await.unwrap();
    assert_eq!(deleted["ok"], json!(true));
    let listed: Value = client.get(format!("{base}/api/sessions")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["total"], json!(0));
}

#[tokio::test]
async fn skills_list_content_toggle() {
    // Fixture skills root: one SKILL.md with frontmatter (the discover_skills
    // contract in crates-skills).
    let dir = std::env::temp_dir().join(format!("hermes-gw-skills-{}", std::process::id()));
    let skill = dir.join("greeter");
    std::fs::create_dir_all(&skill).unwrap();
    std::fs::write(
        skill.join("SKILL.md"),
        "---\nname: greeter\ndescription: Says hello\n---\n\n# Greeter\n\nHello body\n",
    )
    .unwrap();

    let api = api(Some(dir));
    let base = spawn_app(&api).await;
    let client = http();

    let listed: Value = client.get(format!("{base}/api/skills")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["count"], json!(1));
    assert_eq!(listed["skills"][0]["name"], json!("greeter"));
    assert_eq!(listed["skills"][0]["enabled"], json!(true));

    let raw = client
        .get(format!("{base}/api/skills/content?name=greeter"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    let content: Value = serde_json::from_str(&raw).unwrap();
    assert!(content["content"].as_str().unwrap().contains("Hello body"));

    let toggled: Value = client
        .post(format!("{base}/api/skills/toggle"))
        .json(&json!({ "name": "greeter", "enabled": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(toggled["enabled"], json!(false));
    let listed: Value = client.get(format!("{base}/api/skills")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["skills"][0]["enabled"], json!(false));
}

#[tokio::test]
async fn mcp_servers_list_and_enable() {
    let api = api(None).with_mcp_servers(vec![json!({
        "name": "fs", "enabled": true, "transport": "stdio", "command": "echo", "args": []
    })]);
    let base = spawn_app(&api).await;
    let client = http();

    let listed: Value = client.get(format!("{base}/api/mcp/servers")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["servers"][0]["name"], json!("fs"));

    let resp: Value = client
        .put(format!("{base}/api/mcp/servers/fs/enabled"))
        .json(&json!({ "enabled": false }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(resp["enabled"], json!(false));
    let listed: Value = client.get(format!("{base}/api/mcp/servers")).send().await.unwrap().json().await.unwrap();
    assert_eq!(listed["servers"][0]["enabled"], json!(false));

    let unknown: reqwest::Response = client
        .put(format!("{base}/api/mcp/servers/nope/enabled"))
        .json(&json!({ "enabled": true }))
        .send()
        .await
        .unwrap();
    assert_eq!(unknown.status(), reqwest::StatusCode::NOT_FOUND);
}

#[tokio::test]
async fn ws_ticket_mint_is_single_use() {
    let api = api(None);
    let base = spawn_app(&api).await;
    let client = http();
    let minted: Value = client.post(format!("{base}/api/auth/ws-ticket")).send().await.unwrap().json().await.unwrap();
    assert!(minted["ticket"].as_str().unwrap().len() > 8);
    assert_eq!(minted["expires_in"], json!(30));
}
