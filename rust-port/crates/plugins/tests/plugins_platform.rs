//! Integration tests for the plugin platform: fixture-dir loading, manifest
//! validation, the desktop event envelope over a real WebSocket, the
//! enable/disable lifecycle, and the REST routes (axum test clients, no
//! network beyond loopback sockets the tests own).

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use axum::body::Body;
use futures::StreamExt;
use hermes_plugins::{
    AchievementsPlugin, ExamplePlugin, KanbanPlugin, Plugin, PluginFactory, PluginHost,
};
use serde_json::{json, Value};
use tower::ServiceExt;

fn builtin_factories() -> std::collections::HashMap<String, PluginFactory> {
    std::collections::HashMap::from([
        (
            "kanban".to_string(),
            (|bus| Some(Arc::new(KanbanPlugin::new(bus)) as Arc<dyn Plugin>)) as PluginFactory,
        ),
        (
            "hermes-achievements".to_string(),
            (|bus| Some(Arc::new(AchievementsPlugin::new(bus)) as Arc<dyn Plugin>))
                as PluginFactory,
        ),
        (
            "example".to_string(),
            (|bus| Some(Arc::new(ExamplePlugin::new(bus)) as Arc<dyn Plugin>)) as PluginFactory,
        ),
    ])
}

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "hermes-plugins-it-{tag}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn write_plugin(root: &std::path::Path, dir_name: &str, manifest: &str) {
    let dir = root.join(dir_name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("manifest.json"), manifest).unwrap();
    // The declared api file must exist inside the plugin dir for validation.
    std::fs::write(dir.join("plugin_api.py"), b"# plugin api\n").unwrap();
}

/// Serve the host router on 127.0.0.1:0; returns (base_url, join handle).
async fn serve(host: Arc<PluginHost>) -> String {
    let router = host.router();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

/// The next frame off a tungstenite socket, parsed as the JSON envelope.
fn frame_value(msg: tokio_tungstenite::tungstenite::Message) -> Value {
    let bytes = msg.into_data();
    serde_json::from_slice(&bytes).expect("JSON frame")
}

// -- Fixture-dir loading -------------------------------------------------------

#[tokio::test]
async fn loads_plugins_from_a_fixture_directory() {
    let root = temp_root("fixture-dir");
    write_plugin(
        &root,
        "kanban",
        r#"{"name": "kanban", "version": "1.0.0", "api": "plugin_api.py"}"#,
    );
    write_plugin(
        &root,
        "achievements",
        r#"{"name": "hermes-achievements", "version": "0.4.0", "api": "plugin_api.py"}"#,
    );
    write_plugin(
        &root,
        "unknown-plugin",
        r#"{"name": "mystery", "version": "0.1.0"}"#,
    );
    write_plugin(&root, "bad-manifest", r#"{"version": "1.0.0"}"#); // no name
    std::fs::create_dir_all(root.join("_prefixed")).unwrap();
    std::fs::create_dir_all(root.join(".hidden")).unwrap();

    let host = PluginHost::default();
    let (mounted, errors) = host.load_from_dir(&root, &builtin_factories());
    assert_eq!(
        mounted,
        vec!["hermes-achievements", "kanban"],
        "sorted; skipped dirs not mounted"
    );
    assert_eq!(
        errors.len(),
        2,
        "unknown name + invalid manifest recorded, never fatal: {errors:?}"
    );

    // Nothing is enabled until the consent gate says so (Python: plugins.enabled).
    assert!(!host.list().iter().any(|p| p.enabled));
    std::fs::remove_dir_all(&root).unwrap();
}

#[tokio::test]
async fn manifest_validation_rejects_traversal_and_bad_names() {
    let root = temp_root("manifest");
    write_plugin(
        &root,
        "traversal",
        r#"{"name": "evil", "api": "../outside.py"}"#,
    );
    let host = PluginHost::default();
    let (mounted, errors) = host.load_from_dir(&root, &builtin_factories());
    assert!(mounted.is_empty());
    let joined = errors.join("\n");
    assert!(
        joined.contains("evil") || joined.contains("traversal"),
        "{joined}"
    );
    std::fs::remove_dir_all(&root).unwrap();
}

// -- Lifecycle -----------------------------------------------------------------

#[tokio::test]
async fn enable_disable_lifecycle_gates_the_routes() {
    let host = Arc::new(PluginHost::default());
    host.register_factories(&builtin_factories());

    // Disabled: the plugin's router is not mounted.
    let app = Arc::clone(&host).router();
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get("/api/plugins/example/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 404);

    assert!(host.enable("example"));
    let app = Arc::clone(&host).router();
    let res = app
        .clone()
        .oneshot(
            axum::http::Request::get("/api/plugins/example/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    assert!(host.disable("example"));
    let app = Arc::clone(&host).router();
    let res = app
        .oneshot(
            axum::http::Request::get("/api/plugins/example/hello")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        res.status(),
        404,
        "disabled plugins unmount (Python: runtime gate)"
    );
}

// -- REST routes ----------------------------------------------------------------

#[tokio::test]
async fn framework_rest_routes_list_and_toggle_plugins() {
    let host = Arc::new(PluginHost::default());
    host.register_factories(&builtin_factories());
    let url = serve(Arc::clone(&host)).await;

    let client = reqwest::Client::new();
    let list: Value = client
        .get(format!("{url}/api/plugins"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let names: Vec<&str> = list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    let mut sorted = names.clone();
    sorted.sort();
    assert_eq!(sorted, vec!["example", "hermes-achievements", "kanban"]);
    assert!(list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["label"] == "Kanban"));

    // Enable over REST, then the plugin's own REST answers under its namespace.
    let res = client
        .post(format!("{url}/api/plugins/kanban/enable"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);

    let res = client
        .get(format!("{url}/api/plugins/kanban/board"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let board: Value = res.json().await.unwrap();
    assert_eq!(board["tasks"], json!([]));
    assert_eq!(board["cursor"], 0);

    // POST through the plugin namespace, then re-list: state persisted in host.
    let created: Value = client
        .post(format!("{url}/api/plugins/kanban/tasks"))
        .json(&json!({"title": "Ship plugins crate"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(created["id"], 1);

    let list: Value = client
        .get(format!("{url}/api/plugins"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let kanban = list["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .find(|p| p["name"] == "kanban")
        .unwrap();
    assert_eq!(kanban["enabled"], true);

    // Disable over REST; the plugin routes go away; control routes remain.
    assert_eq!(
        client
            .post(format!("{url}/api/plugins/kanban/disable"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );
    assert_eq!(
        client
            .get(format!("{url}/api/plugins/kanban/board"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
    assert_eq!(
        client
            .get(format!("{url}/api/plugins"))
            .send()
            .await
            .unwrap()
            .status(),
        200
    );

    // Unknown plugin: 404 (Python refuses unknown names as "not found").
    assert_eq!(
        client
            .post(format!("{url}/api/plugins/nope/enable"))
            .send()
            .await
            .unwrap()
            .status(),
        404
    );
}

// -- Event fanout over a real WebSocket (the desktop envelope) ------------------

#[tokio::test]
async fn kanban_events_ws_fanout_matches_the_desktop_envelope() {
    let host = Arc::new(PluginHost::default());
    host.register_factories(&builtin_factories());
    host.enable("kanban");
    let url = serve(Arc::clone(&host)).await;

    let ws_url = format!(
        "{}/api/plugins/kanban/events",
        url.replacen("http", "ws", 1)
    );
    let (mut ws, _resp) = tokio_tungstenite::connect_async(&ws_url).await.unwrap();

    let client = reqwest::Client::new();
    let board: Value = client
        .get(format!("{url}/api/plugins/kanban/board"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(
        board["cursor"], 0,
        "tail start: the board cursor is the resume point"
    );

    // Let the server-side subscription attach before the first publish (the
    // upgrade handshake returning and the handler subscribing are not one
    // atomic step).
    tokio::time::sleep(Duration::from_millis(100)).await;

    let created: Value = client
        .post(format!("{url}/api/plugins/kanban/tasks"))
        .json(&json!({"title": "Card A"}))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let moved_res = client
        .post(format!("{url}/api/plugins/kanban/tasks/1/move"))
        .json(&json!({"column": "doing"}))
        .send()
        .await
        .unwrap();
    eprintln!("move status: {}", moved_res.status());
    let moved_text = moved_res.text().await.unwrap();
    eprintln!("move body: {moved_text:?}");
    let moved: Value = serde_json::from_str(&moved_text).unwrap();
    assert_eq!(moved["cursor"], 2);

    // Frames stream per publish (direct fanout, unlike the Python 300 ms
    // poll), so collect until the frame cursor reaches the REST-observed
    // cursor, then assert the envelope shape on what arrived.
    let mut events: Vec<Value> = Vec::new();
    let mut last_cursor;
    loop {
        let raw = tokio::time::timeout(Duration::from_secs(5), ws.next())
            .await
            .expect("frame within 5s")
            .unwrap()
            .unwrap();
        let frame = frame_value(raw);
        let batch = frame["events"].as_array().unwrap().clone();
        assert!(!batch.is_empty(), "empty batches emit no frame");
        for event in &batch {
            events.push(event.clone());
        }
        last_cursor = frame["cursor"].as_u64().unwrap();
        if last_cursor >= moved["cursor"].as_u64().unwrap() {
            break;
        }
    }

    assert_eq!(last_cursor, 2, "cursor is the id of the LAST event so far");
    assert_eq!(events.len(), 2, "both live events fan out");
    assert_eq!(events[0]["id"], 1);
    assert_eq!(events[0]["kind"], "created");
    assert_eq!(events[0]["task_id"], created["id"]);
    assert_eq!(events[0]["payload"], json!({}));
    assert!(
        events[0]["created_at"].as_u64().is_some(),
        "task_event row shape"
    );
    assert_eq!(events[1]["id"], 2);
    assert_eq!(events[1]["kind"], "moved");

    // A second client with ?since=1 replays what it is missing (the move,
    // id 2) FIRST, then goes live — the `?since=` resume protocol.
    let (mut ws2, _resp2) = tokio_tungstenite::connect_async(format!("{ws_url}?since=1"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(100)).await;
    client
        .post(format!("{url}/api/plugins/kanban/tasks"))
        .json(&json!({"title": "Card B"}))
        .send()
        .await
        .unwrap();

    let replay = tokio::time::timeout(Duration::from_secs(5), ws2.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let replay_frame = frame_value(replay);
    let replay_events = replay_frame["events"].as_array().unwrap();
    assert_eq!(replay_frame["cursor"], 2);
    assert_eq!(
        replay_events.len(),
        1,
        "replay after since=1: the created event is NOT resent"
    );
    assert_eq!(replay_events[0]["kind"], "moved");

    let live = tokio::time::timeout(Duration::from_secs(5), ws2.next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let live_frame = frame_value(live);
    assert_eq!(live_frame["cursor"], 3);
    assert_eq!(live_frame["events"][0]["kind"], "created");
    assert_eq!(live_frame["events"][0]["task_id"], 2);

    ws.close(None).await.unwrap();
    ws2.close(None).await.unwrap();
}
