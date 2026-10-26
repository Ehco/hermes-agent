//! Integration tests for the webapp surface: the listener routes (driven with
//! plain reqwest against the spawned router, like the desktop REST tests),
//! the sign-in/access token flow (valid / expired / replayed tokens), static
//! serving with a fixture dist dir, and the launcher's port selection.

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::{json, Value};

use hermes_gateway::webapp::settings::{self, WebappSettings, DEFAULT_PORT};
use hermes_gateway::webapp::{launcher, WebappApi};

// ── fixtures ────────────────────────────────────────────────────────────────

fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("hermes-webapp-it-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

/// A home with an enabled webapp block + a fixture dist dir.
fn fixture(tag: &str) -> (PathBuf, PathBuf) {
    let root = temp_root(tag);
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "webapp:\n  enabled: true\n  host: 127.0.0.1\n  port: 9120\n",
    )
    .unwrap();
    let dist = root.join("dist-webapp");
    std::fs::create_dir_all(&dist).unwrap();
    std::fs::write(
        dist.join("index.html"),
        "<!doctype html><title>Hermes Webapp</title>",
    )
    .unwrap();
    std::fs::write(dist.join("app.js"), "console.log('webapp');").unwrap();
    (home, dist)
}

fn api(home: &std::path::Path, dist: Option<&std::path::Path>) -> WebappApi {
    let state = hermes_gateway::webapp::WebappState::for_home(
        home,
        dist.map(|d| d.to_path_buf()),
    )
    .unwrap();
    WebappApi::new(state)
}

async fn spawn_app(api: &WebappApi) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let router = api.router();
    tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
    format!("http://{addr}")
}

fn client() -> reqwest::Client {
    reqwest::Client::new()
}

/// The current browser credential (as `hermes webapp`'s flow would hand it).
/// Raw token: `bearer_auth` adds the `Bearer ` prefix itself.
fn bearer(api: &WebappApi) -> String {
    api.inner.credential.lock().unwrap().token().to_string()
}

/// Serializes the tests that bind the launcher's search ports: probing and
/// occupying them races between otherwise-parallel tests.
static PORT_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

// ── settings: the `webapp:` block contract ──────────────────────────────────

#[test]
fn missing_block_reads_as_off() {
    let root = temp_root("off");
    let settings = settings::webapp_settings(&root);
    assert_eq!(settings, WebappSettings::default());
    assert_eq!(settings.bind(), ("127.0.0.1".to_string(), 0));
}

#[test]
fn enabled_block_binds_the_configured_address() {
    let (home, _) = fixture("enabled");
    let settings = settings::webapp_settings(&home);
    assert!(settings.enabled);
    assert_eq!(settings.host, "127.0.0.1");
    assert_eq!(settings.port, 9120);
    assert!(settings.reason.is_none());
    assert_eq!(settings.bind(), ("127.0.0.1".to_string(), 9120));
}

#[test]
fn malformed_block_reads_as_off_with_a_reason() {
    let root = temp_root("malformed");
    std::fs::write(
        root.join("config.yaml"),
        "webapp:\n  enabled: yes-please\n  host: 127.0.0.1\n",
    )
    .unwrap();
    let settings = settings::webapp_settings(&root);
    assert!(!settings.enabled);
    assert!(settings.reason.as_deref().unwrap().contains("webapp.enabled must be true or false"));
}

#[test]
fn invalid_host_and_port_carry_the_listeners_reason() {
    let root = temp_root("badhost");
    std::fs::write(
        root.join("config.yaml"),
        "webapp:\n  enabled: true\n  host: 10.0.0.1\n  port: 9120\n",
    )
    .unwrap();
    let settings = settings::webapp_settings(&root);
    assert!(!settings.enabled);
    assert!(settings.reason.as_deref().unwrap().contains("webapp.host must be"));
    // The valid port is still reported, so status shows the address.
    assert_eq!(settings.port, 9120);

    let root = temp_root("badport");
    std::fs::write(
        root.join("config.yaml"),
        "webapp:\n  enabled: true\n  port: 70000\n",
    )
    .unwrap();
    let settings = settings::webapp_settings(&root);
    assert!(!settings.enabled);
    assert!(settings.reason.as_deref().unwrap().contains("webapp.port must be"));
    // The default port stands in for an invalid one.
    assert_eq!(settings.port, DEFAULT_PORT);
}

#[test]
fn public_url_validation_and_wildcards() {
    assert_eq!(settings::public_url("https://example.com/"), Some("https://example.com".into()));
    assert_eq!(settings::public_url("http://lan.local:9120"), Some("http://lan.local:9120".into()));
    assert_eq!(settings::public_url(""), Some(String::new()));
    assert_eq!(settings::public_url("https://user:pw@example.com"), None);
    assert_eq!(settings::public_url("https://example.com/?x=1"), None);
    assert_eq!(settings::public_url("ftp://example.com"), None);
    assert_eq!(settings::public_url("https://"), None);
    assert!(settings::is_wildcard("0.0.0.0"));
    assert!(settings::is_wildcard("::"));
    assert!(settings::local_only("localhost"));
    assert!(settings::local_only("::1"));
    assert!(!settings::local_only("192.168.1.5"));
    assert_eq!(settings::canonical_host("0:0:0:0:0:0:0:1"), "::1");
    assert_eq!(settings::bare_host("[::1]"), "::1");
    assert_eq!(settings::dial_host("0.0.0.0"), "127.0.0.1");
    assert_eq!(settings::dial_host("::"), "::1");
    assert_eq!(settings::dial_origin("::1", 9120), "http://[::1]:9120");
}

#[test]
fn bundle_reasons_track_the_fixture_dir() {
    let (_, dist) = fixture("bundle");
    assert!(settings::bundle_missing_reason(&dist).is_none());
    let missing = temp_root("nobundle");
    let reason = settings::bundle_missing_reason(&missing).unwrap();
    assert!(reason.starts_with("The Webapp is not built"));
    assert!(reason.contains("index.html"));
    // A stale mark makes the built bundle unservable.
    std::fs::write(dist.join(settings::STALE_MARKER), "stale\n").unwrap();
    let reason = settings::bundle_missing_reason(&dist).unwrap();
    assert!(reason.starts_with("The Webapp is not built"));
    assert!(reason.ends_with("Run hermes webapp to rebuild it."));
}

// ── static serving with a fixture dir ───────────────────────────────────────

#[tokio::test]
async fn index_and_assets_are_served_from_the_bundle() {
    let (home, dist) = fixture("static");
    let base = spawn_app(&api(&home, Some(&dist))).await;
    let client = client();
    let index = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.status(), 200);
    assert!(index.text().await.unwrap().contains("Hermes Webapp"));
    let js = client.get(format!("{base}/app.js")).send().await.unwrap();
    assert_eq!(js.status(), 200);
    assert_eq!(
        js.headers().get("content-type").unwrap(),
        "text/javascript; charset=utf-8"
    );
    // A path outside the bundle is a plain 404, never a traversal.
    let missing = client.get(format!("{base}/nope.css")).send().await.unwrap();
    assert_eq!(missing.status(), 404);
    let traversal = client.get(format!("{base}/../secret.txt")).send().await.unwrap();
    assert_ne!(traversal.status(), 200);
}

#[tokio::test]
async fn a_missing_bundle_answers_unavailable_with_the_reason() {
    let (home, _) = fixture("nodist");
    // An empty dist dir: no index.html, so the reason names it.
    let empty_dist = home.parent().unwrap().join("empty-dist");
    std::fs::create_dir_all(&empty_dist).unwrap();
    let base = spawn_app(&api(&home, Some(&empty_dist))).await;
    let client = client();
    let index = client.get(format!("{base}/")).send().await.unwrap();
    assert_eq!(index.status(), 503);
    let body: Value = index.json().await.unwrap();
    assert_eq!(body["state"], "unavailable");
    assert!(body["reason"].as_str().unwrap().contains("index.html"));
    // The status endpoint reports the same.
    let status: Value = client
        .get(format!("{base}/api/webapp/status"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(status["state"], "unavailable");
}

#[tokio::test]
async fn stale_bundle_is_unavailable_until_rebuilt() {
    let (home, dist) = fixture("stale");
    std::fs::write(dist.join(settings::STALE_MARKER), "stale\n").unwrap();
    let base = spawn_app(&api(&home, Some(&dist))).await;
    let body: Value = client()
        .get(format!("{base}/"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(body["state"], "unavailable");
    assert!(body["reason"]
        .as_str()
        .unwrap()
        .contains("hermes update could not rebuild it"));
}

// ── the sign-in/access token flow ───────────────────────────────────────────

#[tokio::test]
async fn mint_requires_the_browser_credential() {
    let (home, _) = fixture("auth");
    let app = api(&home, None);
    let base = spawn_app(&app).await;
    let client = client();
    // No credential: 401.
    let res = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    // Wrong credential: 401.
    let res = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .bearer_auth("not-the-token")
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);
    // The real one mints.
    let res = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .bearer_auth(bearer(&app))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["ticket"].as_str().unwrap().len() >= 32);
}

#[tokio::test]
async fn window_session_exactly_once_per_ticket() {
    let (home, _) = fixture("redeem");
    let app = api(&home, None);
    let base = spawn_app(&app).await;
    let client = client();
    let token = bearer(&app);
    let ticket: String = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["ticket"]
        .as_str()
        .unwrap()
        .into();

    // Valid: the current session token comes back.
    let res = client
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", &ticket)
        .header("Origin", &base)
        .header(reqwest::header::HOST, base.trim_start_matches("http://"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(hermes_gateway::webapp::credential::valid_token(body["token"].as_str().unwrap()));

    // Replayed: already used.
    let res = client
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", &ticket)
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

#[tokio::test]
async fn cross_origin_window_session_is_refused() {
    let (home, _) = fixture("origin");
    let app = api(&home, None);
    let base = spawn_app(&app).await;
    let ticket = app.inner.mint_launch_link(&base).unwrap();
    let fragment = ticket.split('#').nth(1).unwrap().strip_prefix("ticket=").unwrap();
    // A foreign Origin never redeems — and does not burn the ticket.
    let res = client()
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", fragment)
        .header("Origin", "http://evil.example")
        .header(reqwest::header::HOST, base.trim_start_matches("http://"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
    // The same ticket still redeems from its own origin.
    let res = client()
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", fragment)
        .header("Origin", &base)
        .header(reqwest::header::HOST, base.trim_start_matches("http://"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn handoff_pages_are_credential_free_html() {
    let (home, dist) = fixture("pages");
    let base = spawn_app(&api(&home, Some(&dist))).await;
    for path in ["/webapp/launch", "/webapp/window"] {
        let res = client().get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(res.status(), 200);
        let csp = res
            .headers()
            .get("content-security-policy")
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        let html = res.text().await.unwrap();
        assert!(html.contains("sessionStorage"));
        assert!(csp.contains("script-src 'nonce-"));
    }
}

#[tokio::test]
async fn launch_link_mint_spend_roundtrip() {
    let (home, _) = fixture("launch");
    let app = api(&home, None);
    let base = spawn_app(&app).await;
    let client = client();
    let link = app.inner.mint_launch_link(&base).unwrap();
    let fragment = link.split('#').nth(1).unwrap().strip_prefix("ticket=").unwrap();
    let res = client
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", fragment)
        .header("Origin", &base)
        .header(reqwest::header::HOST, base.trim_start_matches("http://"))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn sign_out_rotates_the_credential_and_ends_old_grants() {
    let (home, dist) = fixture("rotate");
    let app = api(&home, Some(&dist));
    let base = spawn_app(&app).await;
    let client = client();
    let old_token = bearer(&app);
    let ticket: String = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .bearer_auth(&old_token)
        .send()
        .await
        .unwrap()
        .json::<Value>()
        .await
        .unwrap()["ticket"]
        .as_str()
        .unwrap()
        .into();

    // Sign out: the stored token is replaced, like `hermes webapp --sign-out`.
    let old = app.inner.credential.lock().unwrap().token().to_string();
    app.inner.credential.lock().unwrap().rotate().unwrap();
    assert_ne!(app.inner.credential.lock().unwrap().token(), old);

    // The old credential no longer authenticates.
    let res = client
        .post(format!("{base}/api/webapp/window-ticket"))
        .bearer_auth(old_token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 401);

    // The ticket minted before the sign-out opens nothing after it.
    let res = client
        .post(format!("{base}/webapp/window-session"))
        .header("X-Hermes-Window-Ticket", &ticket)
        .header("Origin", &base)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 403);
}

// ── socket tickets ──────────────────────────────────────────────────────────

#[tokio::test]
async fn socket_tickets_are_named_and_single_use() {
    let (home, _) = fixture("socket");
    let app = api(&home, None);
    let base = spawn_app(&app).await;
    let client = client();
    let token = bearer(&app);

    let res = client
        .post(format!("{base}/api/webapp/socket-ticket"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 400, "no socket named → Unknown socket");

    let res = client
        .post(format!("{base}/api/webapp/socket-ticket?socket=host-terminal"))
        .bearer_auth(&token)
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), 200);
    let body: Value = res.json().await.unwrap();
    assert!(body["ticket"].as_str().unwrap().len() >= 32);
    assert_eq!(body["ttl_seconds"], json!(30));

    // The shared store redeems it exactly once (the /api/ws gate's shape).
    let ticket = body["ticket"].as_str().unwrap().to_string();
    assert!(app.socket_ticket_store().consume(&ticket).is_ok());
    assert!(app.socket_ticket_store().consume(&ticket).is_err());
}

// ── the launcher ────────────────────────────────────────────────────────────

#[test]
fn a_configured_port_is_kept_unwritten() {
    let (home, _) = fixture("portkept");
    let (port, write) = launcher::port_to_serve(&home, "127.0.0.1");
    assert_eq!(port, 9120);
    assert_eq!(write, None, "a configured webapp.port is the user's");
}

#[test]
fn a_home_without_a_port_gets_the_first_free_one_above_default() {
    let _guard = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = temp_root("portsearch");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.yaml"),
        "webapp:\n  enabled: true\n  host: 127.0.0.1\n",
    )
    .unwrap();
    let (port, write) = launcher::port_to_serve(&home, "127.0.0.1");
    assert_eq!(Some(port), write, "the chosen port is written once");
    assert!(port > DEFAULT_PORT);
    assert!(port <= DEFAULT_PORT + launcher::PORT_SEARCH);
}

#[test]
fn an_occupied_port_is_skipped() {
    let _guard = PORT_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let root = temp_root("portbusy");
    let home = root.join("home");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.yaml"), "webapp:\n  enabled: true\n").unwrap();
    // Occupy the first port the search would take, then expect the next one.
    let listener = std::net::TcpListener::bind(("127.0.0.1", DEFAULT_PORT + 1)).unwrap();
    let (port, write) = launcher::port_to_serve(&home, "127.0.0.1");
    assert_eq!(port, DEFAULT_PORT + 2);
    assert_eq!(write, Some(DEFAULT_PORT + 2));
    drop(listener);
}

#[test]
fn listener_plan_tracks_the_config() {
    let (home, dist) = fixture("plan");
    let plan = launcher::listener_plan(&home, Some(dist));
    assert!(plan.enabled);
    assert_eq!(plan.bind(), ("127.0.0.1".to_string(), 9120));
    assert_eq!(plan.native_origin(), "http://127.0.0.1:9120");
    assert!(plan.bundle_missing_reason().is_none());

    // Off: a private loopback listener on an ephemeral port.
    let root = temp_root("planoff");
    let plan = launcher::listener_plan(&root, None);
    assert!(!plan.enabled);
    assert_eq!(plan.bind(), ("127.0.0.1".to_string(), 0));
}

#[test]
fn auth_required_follows_the_bind_and_public_url() {
    let mut settings = WebappSettings { enabled: true, ..Default::default() };
    assert!(!launcher::auth_required(&settings), "127.0.0.1 with no public URL is private");
    settings.host = "0.0.0.0".into();
    assert!(launcher::auth_required(&settings), "a wildcard bind is gated");
    settings.host = "127.0.0.1".into();
    settings.public_url = "https://tunnel.example".into();
    assert!(launcher::auth_required(&settings), "a public URL gates the loopback bind");
}

#[test]
fn cli_status_and_sign_out_reports() {
    let (home, _) = fixture("cli");
    // Status: serving, with the address and mode.
    let out = launcher::cli_entrypoint(&home, launcher::WebappCommand::Status);
    assert_eq!(out.code, 0);
    assert!(out.message.contains("Serving at http://127.0.0.1:9120"));
    // Sign out: the credential rotates.
    let before = hermes_gateway::webapp::BrowserCredential::load(&home)
        .unwrap()
        .token()
        .to_string();
    let out = launcher::cli_entrypoint(&home, launcher::WebappCommand::SignOut);
    assert_eq!(out.code, 0);
    let after = hermes_gateway::webapp::BrowserCredential::load(&home)
        .unwrap()
        .token()
        .to_string();
    assert_ne!(before, after);

    // An off Webapp reports why and exits 1.
    let root = temp_root("clioff");
    let out = launcher::cli_entrypoint(&root, launcher::WebappCommand::Status);
    assert_eq!(out.code, 1);
    assert!(out.message.contains("webapp.enabled: true"));
}
