//! The Webapp listener's routes (port of `gateway/webapp_listener.py`'s mount
//! + `hermes_cli/web_routers/webapp.py`): the built bundle's static files, the
//! one-use ticket each browser dial mints (`POST /api/webapp/socket-ticket`
//! for `/api/ws` on this same origin) and the one-use handoffs (child window,
//! auto-opened launch tab) for the private Webapp session.
//!
//! Auth contract (documented for the crates/cli wiring): browsers hold the
//! gateway home's browser credential (`webapp/session-token`,
//! `credential::BrowserCredential`) and present it as `Authorization: Bearer`;
//! minted tickets are single-use, TTL-bounded and generation-bound, so a
//! sign-out (`--sign-out`, a rotate) ends every grant made before it. Socket
//! tickets are minted through the desktop `TicketStore` (the same one-use,
//! 30-second-TTL shape the `/api/ws` upgrade redeems); a caller that serves
//! `/api/ws` shares this store via `WebappApi::socket_ticket_store`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::extract::{Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::Json;
use serde_json::{json, Value};

use super::credential::BrowserCredential;
use super::settings::{
    bundle_missing_reason, dial_origin, webapp_settings, DEFAULT_PORT,
};
use super::tickets::{
    HandoffTickets, LAUNCH_TICKET_TTL_SECONDS, WINDOW_TICKET_TTL_SECONDS,
};
use crate::desktop::tickets::TicketStore;

/// Every socket a Webapp page dials with a one-use ticket, by its
/// `web_session_sockets` name (`SOCKET_TICKETS`): `gateway` for `/api/ws`
/// (also offered as the `hermes-gateway-ticket.` subprotocol),
/// `host-terminal` for `/api/host-terminal`, `speak-stream` and
/// `transcribe-stream` for `/api/audio/<name>`.
pub const SOCKET_NAMES: [&str; 4] = ["gateway", "host-terminal", "speak-stream", "transcribe-stream"];

/// The webapp listener's shared state.
pub struct WebappState {
    /// The gateway home the browser credential and status read.
    pub home: std::path::PathBuf,
    /// The built bundle to serve (`WebappMount.dist`); `None` serves the
    /// bundle-missing page on every static request.
    pub dist: Option<std::path::PathBuf>,
    /// The listener's loopback dial origin (`status['url']`).
    pub url: String,
    /// Whether the sign-in gate engages on this bind (`auth_required`).
    pub auth_required: bool,
    /// The `webapp.public_url`, `""` for none.
    pub public_url: String,
    /// The Webapp's mode (`access_status.mode`): public, lan or local.
    pub mode: Option<&'static str>,
    pub credential: std::sync::Mutex<BrowserCredential>,
    /// The one-use window/launch handoff tickets.
    pub handoff: HandoffTickets,
    /// The one-use socket tickets (`POST /api/webapp/socket-ticket`), the
    /// same shape the `/api/ws` upgrade redeems.
    pub socket_tickets: TicketStore,
}

impl WebappState {
    /// Build from the gateway home: settings from `config.yaml`, the browser
    /// credential from `webapp/session-token` (created on first use).
    pub fn for_home(home: &std::path::Path, dist: Option<std::path::PathBuf>) -> Result<Self, super::credential::CredentialError> {
        let settings = webapp_settings(home);
        let url = if settings.enabled {
            dial_origin(&settings.host, settings.port)
        } else {
            dial_origin(super::settings::DEFAULT_HOST, 0)
        };
        let mode = super::launcher::access_mode(&settings);
        let auth_required = super::launcher::auth_required(&settings);
        Ok(Self {
            home: home.into(),
            dist,
            url,
            auth_required,
            public_url: settings.public_url.clone(),
            mode,
            credential: std::sync::Mutex::new(BrowserCredential::load(home)?),
            handoff: HandoffTickets::default(),
            socket_tickets: TicketStore::new(),
        })
    }

    /// The status the control-socket `webapp-status` verb reports: serving
    /// while the bundle is built, unavailable with its reason otherwise.
    pub fn status(&self) -> Value {
        let missing = self
            .dist
            .as_ref()
            .and_then(|d| bundle_missing_reason(d));
        let state = if self.mode.is_none() {
            "off"
        } else if missing.is_some() {
            "unavailable"
        } else {
            "serving"
        };
        json!({
            "state": state,
            "url": if state == "serving" { json!(self.url) } else { Value::Null },
            "reason": missing.map(|m| json!(m)).unwrap_or(Value::Null),
            "auth_required": self.auth_required,
            "mode": self.mode,
            "public_url": self.public_url,
        })
    }

    /// The one-use launch link `webapp-launch` hands the CLI's browser
    /// (`mint_launch_ticket` + the `#{ticket}` fragment): the launcher's argv
    /// carries this, never the token, and it works once within
    /// [`super::tickets::LAUNCH_TICKET_TTL_SECONDS`]. `Err` at the pending
    /// cap.
    pub fn mint_launch_link(&self, base_url: &str) -> Result<String, String> {
        let credential = self.credential.lock().unwrap();
        let ticket = self
            .handoff
            .mint(
                Duration::from_secs(LAUNCH_TICKET_TTL_SECONDS),
                true,
                &credential,
            )
            .map_err(|e| e.0)?;
        Ok(format!("{base_url}/webapp/launch#ticket={ticket}"))
    }
}

/// The webapp listener: clone-cheap, the router holds a clone as its state.
#[derive(Clone)]
pub struct WebappApi {
    pub inner: Arc<WebappState>,
}

impl WebappApi {
    pub fn new(inner: WebappState) -> Self {
        Self { inner: Arc::new(inner) }
    }

    /// The shared socket-ticket store: a `/api/ws` upgrade gate consumes from
    /// the same store `POST /api/webapp/socket-ticket` mints into.
    pub fn socket_ticket_store(&self) -> &TicketStore {
        &self.inner.socket_tickets
    }

    /// The listener's router under the same security-header middleware as the
    /// rest of the gateway.
    pub fn router(&self) -> axum::Router {
        let app = axum::Router::new()
            .route("/", get(static_index))
            .route("/{*rest}", get(static_file))
            .route("/webapp/launch", get(launch_page))
            .route("/webapp/window", get(window_page))
            .route("/webapp/window-session", post(redeem_window_ticket))
            .route("/api/webapp/window-ticket", post(issue_window_ticket))
            .route("/api/webapp/socket-ticket", post(issue_socket_ticket))
            .route("/api/webapp/status", get(webapp_status))
            .fallback(fallback_404)
            .with_state(self.clone());
        app.layer(axum::middleware::from_fn_with_state(
            crate::config::GatewayConfig::default(),
            crate::middleware::security_headers_middleware,
        ))
    }
}

// ── auth helpers ─────────────────────────────────────────────────────────────

/// The Bearer-presented browser credential, or a 401.
fn require_token(state: &WebappState, headers: &HeaderMap) -> Result<(), Response> {
    let authorization = headers.get(header::AUTHORIZATION).and_then(|v| v.to_str().ok());
    let credential = state.credential.lock().unwrap();
    if credential.presented(authorization).is_some() {
        return Ok(());
    }
    Err((StatusCode::UNAUTHORIZED, Json(json!({"detail": "Unauthorized"}))).into_response())
}

// ── ticket mints ─────────────────────────────────────────────────────────────

/// `POST /api/webapp/window-ticket`: the authorized parent tab's one-use
/// ticket for a child-window handoff (`issue_window_ticket`).
async fn issue_window_ticket(
    State(api): State<WebappApi>,
    headers: HeaderMap,
) -> Response {
    if let Err(res) = require_token(&api.inner, &headers) {
        return res;
    }
    let credential = api.inner.credential.lock().unwrap();
    match api.inner.handoff.mint(
        Duration::from_secs(WINDOW_TICKET_TTL_SECONDS),
        false,
        &credential,
    ) {
        Ok(ticket) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "ticket": ticket })),
        )
            .into_response(),
        Err(e) => (
            StatusCode::TOO_MANY_REQUESTS,
            Json(json!({ "detail": e.0 })),
        )
            .into_response(),
    }
}

/// `POST /api/webapp/socket-ticket`: the browser's one-use ticket for one dial
/// of `?socket=` on this same origin (`issue_socket_ticket`). Minted locally:
/// the server that serves this page runs the socket's sessions. `socket` rides
/// the query string (the Python route's form) or a JSON body.
async fn issue_socket_ticket(
    State(api): State<WebappApi>,
    headers: HeaderMap,
    query: Query<HashMap<String, String>>,
    body: Option<Json<Value>>,
) -> Response {
    if let Err(res) = require_token(&api.inner, &headers) {
        return res;
    }
    let named = query.get("socket").cloned().unwrap_or_default();
    let socket = body
        .as_ref()
        .and_then(|Json(b)| b.get("socket").and_then(Value::as_str))
        .unwrap_or(&named);
    if !SOCKET_NAMES.contains(&socket) {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "detail": "Unknown socket" })),
        )
            .into_response();
    }
    let identity = json!({
        "user_id": "legacy-token-owner",
        "provider": "session-token",
        "issuer": "",
        "socket": socket,
    });
    let ticket = api.inner.socket_tickets.mint(identity);
    let ttl = crate::desktop::tickets::TTL_SECONDS;
    (
        [(header::CACHE_CONTROL, "no-store")],
        Json(json!({ "ticket": ticket, "ttl_seconds": ttl })),
    )
        .into_response()
}

// ── handoff redemption ───────────────────────────────────────────────────────

/// Whether the single `Origin` header matches this request's own origin
/// (`_http_origin` + the same-origin check on `/webapp/window-session`). A
/// request with no `Origin` is a non-browser and passes; two `Origin` headers
/// are refused.
fn same_origin(headers: &HeaderMap) -> bool {
    let origins: Vec<&str> = headers
        .get("origin")
        .iter()
        .filter_map(|v| v.to_str().ok())
        .collect();
    match origins.len() {
        0 => true,
        1 => {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("");
            format!("http://{host}") == origins[0].trim_end_matches('/')
                || format!("https://{host}") == origins[0].trim_end_matches('/')
        }
        _ => false,
    }
}

/// `POST /webapp/window-session`: spend the one-use handoff ticket from the
/// `X-Hermes-Window-Ticket` header for this start's session token. The
/// endpoint deliberately sits outside `/api`: the child window or launch tab
/// has no session yet, and only a purpose-specific ticket can exchange for
/// the existing one.
async fn redeem_window_ticket(
    State(api): State<WebappApi>,
    headers: HeaderMap,
) -> Response {
    if !same_origin(&headers) {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"detail": "Webapp window handoff requires a same-origin request"})),
        )
            .into_response();
    }
    let ticket = headers
        .get("x-hermes-window-ticket")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let credential = api.inner.credential.lock().unwrap();
    // A launch ticket crossed a browser launcher's argv; the Rust listener
    // runs where the launcher runs (same OS user), so the redeemer is
    // vouched for the way Python's Windows path is (`_peer_is_server_user`):
    // per-user proof is the ticket's privacy, checked per platform there.
    let launch_vouched = true;
    match api.inner.handoff.redeem(&ticket, &credential, launch_vouched) {
        Ok(()) => (
            [(header::CACHE_CONTROL, "no-store")],
            Json(json!({ "token": credential.token() })),
        )
            .into_response(),
        Err(_) => (
            StatusCode::FORBIDDEN,
            Json(json!({ "detail": "Webapp window link expired or was already used; open a new window" })),
        )
            .into_response(),
    }
}

// ── handoff pages ────────────────────────────────────────────────────────────

/// The launch page's script (`_LAUNCH_SCRIPT`, `__BASE_PATH__` bound to `""` —
/// the Rust listener serves the Webapp at its root): spends the one-use launch
/// ticket from the fragment (never sent to the server), keeps the credential
/// it receives in this tab's sessionStorage and enters the Webapp at an
/// address that carries neither.
const LAUNCH_SCRIPT: &str = r#"
const basePath = "";
const params = new URLSearchParams(location.hash.slice(1));
history.replaceState(null, '', location.pathname);
const ticket = params.get('ticket') || '';
const show = message => { document.getElementById('status').textContent = message; };
const fail = () => show('This launch link was already used or has expired. Run hermes webapp again for a new one.');
if (!/^[A-Za-z0-9_-]{43}$/.test(ticket)) {
  fail();
} else {
  fetch(`${basePath}/webapp/window-session`, {
    method: 'POST', credentials: 'same-origin', headers: {'X-Hermes-Window-Ticket': ticket}
  }).then(response => response.ok ? response.json() : Promise.reject()).then(({token}) => {
    if (typeof token !== 'string' || !/^[A-Za-z0-9_-]{43}$/.test(token)) throw new Error();
    try {
      sessionStorage.setItem(`hermes.webapp.session.v1:${JSON.stringify([location.origin, basePath])}`, token);
    } catch {
      show('This browser does not let the Webapp keep its sign-in in this tab. Allow site data for this '
        + 'address, then run hermes webapp again.');
      return;
    }
    const target = new URL(`${basePath}/`, location.origin);
    target.search = params.get('query') || '';
    location.replace(target.href);
  }).catch(fail);
}
"#;

/// The window page's script (`_WINDOW_SCRIPT`): a BroadcastChannel handoff an
/// authorized parent tab mints a one-use ticket through.
const WINDOW_SCRIPT: &str = r#"
const basePath = "";
const params = new URLSearchParams(location.hash.slice(1));
history.replaceState(null, '', location.pathname);
const id = params.get('channel') || '';
const status = document.getElementById('status');
if (!/^[a-f0-9-]{36}$/.test(id)) {
  status.textContent = 'Open this window from an authorized Hermes Webapp tab.';
} else {
  const channel = new BroadcastChannel(`hermes.webapp.window:${id}`);
  let receiving = false;
  let finished = false;
  const controller = new AbortController();
  const fail = message => {
    if (finished) return;
    finished = true;
    controller.abort();
    status.textContent = message;
    channel.postMessage({type: 'error'});
    channel.close();
    clearTimeout(timeout);
  };
  const timeout = setTimeout(() => fail('This window link expired. Open a new window from the original tab.'), 30000);
  channel.onmessage = async event => {
    if (receiving) return;
    if (event.data?.error) { fail(event.data.error); return; }
    const ticket = event.data?.ticket;
    if (typeof ticket !== 'string' || !/^[A-Za-z0-9_-]{43}$/.test(ticket)) return;
    receiving = true;
    try {
      const response = await fetch(`${basePath}/webapp/window-session`, {
        method: 'POST', credentials: 'same-origin',
        headers: {'X-Hermes-Window-Ticket': ticket}, signal: controller.signal
      });
      if (!response.ok) throw new Error('Window authorization expired. Open a new window from the original tab.');
      const {token} = await response.json();
      if (finished) return;
      if (typeof token !== 'string' || !/^[A-Za-z0-9_-]{43}$/.test(token)) throw new Error('Invalid window authorization.');
      sessionStorage.setItem(`hermes.webapp.session.v1:${JSON.stringify([location.origin, basePath])}`, token);
      const target = new URL(`${basePath}/`, location.origin);
      target.search = params.get('query') || '';
      target.hash = params.get('route') || '/';
      channel.postMessage({type: 'done'});
      finished = true;
      clearTimeout(timeout);
      channel.close();
      location.replace(target.href);
    } catch (error) { fail(error.message || 'Could not authorize this window. Open it again from the original tab.'); }
  };
  channel.postMessage({type: 'ready'});
}
"#;

/// The credential-free, uncached handoff page (`_handoff_page`): no script but
/// *script* (nonce-bound), no-store, no referrer, a CSP of `default-src
/// 'none'` plus the page's own nonce.
fn handoff_page(title: &str, status_text: &str, script: &str) -> Response {
    let nonce = super::new_token_id();
    let html = format!(
        r#"<!doctype html><meta charset="utf-8"><meta name="referrer" content="no-referrer">
<title>{title}</title><p id="status">{status_text}</p>
<script nonce="{nonce}">{script}</script>"#
    );
    let csp = format!(
        "default-src 'none'; script-src 'nonce-{nonce}'; connect-src 'self'; \
         frame-ancestors 'none'; base-uri 'none'; form-action 'none'"
    );
    (
        [
            (header::CONTENT_SECURITY_POLICY, csp),
            (header::CACHE_CONTROL, "no-store".to_string()),
            (header::REFERRER_POLICY, "no-referrer".to_string()),
        ],
        axum::response::Html(html),
    )
        .into_response()
}

/// `GET /webapp/launch`: where the server's own browser-open lands.
async fn launch_page(State(api): State<WebappApi>) -> Response {
    if api.inner.mode.is_none() {
        return bundle_unavailable(&api).into_response();
    }
    handoff_page("Opening Hermes", "Opening Hermes Webapp…", LAUNCH_SCRIPT)
}

/// `GET /webapp/window`: the authorized child window's handoff page.
async fn window_page(State(api): State<WebappApi>) -> Response {
    if api.inner.mode.is_none() {
        return bundle_unavailable(&api).into_response();
    }
    handoff_page("Opening Hermes", "Authorizing this Hermes window…", WINDOW_SCRIPT)
}

// ── status + static files ────────────────────────────────────────────────────

/// `GET /api/webapp/status`: the control-socket `webapp-status` verb over
/// HTTP, what the CLI's `--status` and the Web Access pane read.
async fn webapp_status(State(api): State<WebappApi>) -> Response {
    Json(api.inner.status()).into_response()
}

fn bundle_unavailable(api: &WebappApi) -> Response {
    let missing = api
        .inner
        .dist
        .as_ref()
        .and_then(|d| bundle_missing_reason(d))
        .unwrap_or_else(|| "The Webapp is not served here.".into());
    unavailable_with(missing)
}

fn unavailable_with(reason: String) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        Json(json!({ "state": "unavailable", "url": Value::Null, "reason": reason })),
    )
        .into_response()
}

/// Content-type by extension — the bundle's own files, nothing else.
fn content_type(path: &std::path::Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?;
    Some(match ext {
        "html" | "htm" => "text/html; charset=utf-8",
        "js" | "mjs" => "text/javascript; charset=utf-8",
        "css" => "text/css; charset=utf-8",
        "json" | "map" => "application/json",
        "svg" => "image/svg+xml",
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "ico" => "image/x-icon",
        "woff" => "font/woff",
        "woff2" => "font/woff2",
        "txt" => "text/plain; charset=utf-8",
        "wasm" => "application/wasm",
        _ => return None,
    })
}

/// Read *rel* under the dist dir, guarding traversal (`..`, absolute roots,
/// backslashes): the browser gets exactly the bundle's files.
fn read_dist(dist: &std::path::Path, rel: &str) -> Option<(Vec<u8>, &'static str)> {
    let rel = rel.trim_start_matches('/');
    if rel.is_empty() || rel.contains("..") || rel.contains('\\') || rel.contains(':') {
        return None;
    }
    let path = dist.join(rel);
    let bytes = std::fs::read(&path).ok()?;
    content_type(&path).map(|ct| (bytes, ct))
}

/// `GET /`: the bundle's `index.html`, or the bundle-missing answer. A stale
/// bundle is not served (`NOT_REBUILT`: why a bundle `hermes update` could not
/// rebuild is not served against the updated backend).
async fn static_index(State(api): State<WebappApi>) -> Response {
    let Some(dist) = api.inner.dist.clone() else {
        return bundle_unavailable(&api);
    };
    if let Some(reason) = bundle_missing_reason(&dist) {
        return unavailable_with(reason);
    }
    match read_dist(&dist, "index.html") {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct)], bytes).into_response(),
        None => bundle_unavailable(&api),
    }
}

/// `GET /{*rest}`: any other bundle file. The listener serves a bundle the
/// moment it appears or is rebuilt, so building it needs no gateway restart.
async fn static_file(State(api): State<WebappApi>, axum::extract::Path(rest): axum::extract::Path<String>) -> Response {
    let Some(dist) = api.inner.dist.clone() else {
        return bundle_unavailable(&api);
    };
    if let Some(reason) = bundle_missing_reason(&dist) {
        return unavailable_with(reason);
    }
    match read_dist(&dist, &rest) {
        Some((bytes, ct)) => ([(header::CONTENT_TYPE, ct)], bytes).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

/// Unknown paths answer the gateway's JSON 404, not a bundle page.
async fn fallback_404() -> Response {
    (
        StatusCode::NOT_FOUND,
        Json(json!({
            "error": {
                "message": format!("Unknown path: the Webapp serves its bundle at {} (port {} default)",
                    "/", DEFAULT_PORT),
                "type": "invalid_request_error",
            }
        })),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn same_origin_requires_a_matching_origin_header() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, "127.0.0.1:9120".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "http://127.0.0.1:9120".parse().unwrap());
        assert!(same_origin(&headers));
        headers.insert(header::ORIGIN, "http://evil.example:80".parse().unwrap());
        assert!(!same_origin(&headers));
        headers.insert(header::ORIGIN, "http://a".parse().unwrap());
        headers.append(header::ORIGIN, "http://b".parse().unwrap());
        assert!(!same_origin(&headers));
    }
}
