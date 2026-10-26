//! The ticketed `/api/ws` upgrade + the newline-delimited JSON-RPC socket loop
//! (port of `hermes_cli/web_routers/chat_ws.py::gateway_ws` +
//! `tui_gateway/ws.py::handle_ws`): `gateway.ready` right after accept,
//! `gateway.ping` answered inline, every other frame dispatched, hub events
//! fanned out between replies.

use std::collections::HashMap;

use axum::extract::ws::{Message as WsMessage, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use futures::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;

use hermes_core::agent::ToolHost;
use hermes_core::transport::Transport;

use super::{dispatch, event_frame, DesktopApi};

/// The canonical gateway protocol the desktop offers as its first subprotocol
/// (`CANONICAL_GATEWAY_PROTOCOL` in canonical-protocol.ts).
pub const GATEWAY_WS_PROTOCOL: &str = "hermes-gateway-v1";
/// Ticket-in-subprotocol prefix (the desktop sends
/// `["hermes-gateway-v1", "hermes-gateway-ticket.<ticket>]`).
const TICKET_PROTOCOL_PREFIX: &str = "hermes-gateway-ticket.";

/// Pre-accept gates, then the socket loop. Credential shapes: `?ticket=` (or
/// the ticket subprotocol, consumed once) → `?token=` (static bearer, remote
/// gateways) → unauthenticated loopback (the Python "loopback" auth mode when
/// no token is configured). Anything else answers 401 before the upgrade.
pub async fn upgrade<T, H>(
    State(api): State<DesktopApi<T, H>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let offered: Vec<String> = headers
        .get("sec-websocket-protocol")
        .map(|v| v.to_str().unwrap_or("").split(',').map(str::trim).map(str::to_string).collect())
        .unwrap_or_default();
    let subprotocol_ticket = offered
        .iter()
        .find(|p| p.starts_with(TICKET_PROTOCOL_PREFIX))
        .map(|p| p[TICKET_PROTOCOL_PREFIX.len()..].to_string());
    let canonical = params.contains_key("native_dial") || offered.iter().any(|p| p == GATEWAY_WS_PROTOCOL);

    // 1. Ticket auth (one-use, 30 s TTL) — query string or subprotocol.
    let ticket = params.get("ticket").filter(|t| !t.is_empty()).or(subprotocol_ticket.as_ref());
    let mut identity = None;
    if let Some(ticket) = ticket {
        match api.inner.tickets.consume(ticket) {
            Ok(info) => identity = Some(info),
            Err(e) => {
                return auth_reject(StatusCode::UNAUTHORIZED, &format!("auth: {e}"));
            }
        }
    } else if let Some(token) = &api.inner.token {
        // 2. Static token auth for gated remote gateways.
        let ok = params.get("token").is_some_and(|t| constant_time_eq(t, token));
        if !ok {
            return auth_reject(StatusCode::UNAUTHORIZED, "auth: missing or bad token");
        }
    }
    // 3. No token configured: loopback mode, no credential required.

    let (handle, event_rx) = api.inner.hub.subscribe();
    let socket = ws
        .protocols([GATEWAY_WS_PROTOCOL])
        .on_upgrade(move |socket| socket_loop(api, handle, identity, canonical, event_rx, socket));
    socket
}

fn auth_reject(status: StatusCode, reason: &str) -> Response {
    // Mirrors the Python gate's close code the reconnect banner keys on.
    let mut res = (status, reason.to_string()).into_response();
    res.headers_mut()
        .insert("x-hermes-close-code", "4401".parse().unwrap());
    res
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// One connected socket: dispatch requests in arrival order and forward hub
/// events between replies (`handle_ws`'s read loop + dispatcher).
async fn socket_loop<T, H>(
    api: DesktopApi<T, H>,
    handle: u64,
    identity: Option<Value>,
    canonical: bool,
    mut events: mpsc::UnboundedReceiver<Value>,
    socket: WebSocket,
) where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    let (mut sink, mut stream) = socket.split();
    let _ = identity; // stamped for browser-controller-grade RPCs on the Python side
    let _ = canonical;

    // gateway.ready right after accept (`handle_ws`).
    let ready = json!({
        "type": "gateway.ready",
        "payload": {
            "skin": {},
            "change_events": true,
            "heartbeat": true,
            "replay_epoch": api.inner.epoch,
        },
    });
    if sink
        .send(WsMessage::Text(event_frame(ready).to_string().into()))
        .await
        .is_err()
    {
        api.inner.hub.unsubscribe(handle);
        return;
    }

    loop {
        let text: String = tokio::select! {
            // Ordered event fanout (hub events interleave with replies).
            Some(event) = events.recv() => {
                if sink.send(WsMessage::Text(event_frame(event).to_string().into())).await.is_err() {
                    break;
                }
                continue;
            }
            incoming = stream.next() => match incoming {
                Some(Ok(WsMessage::Text(text))) => text.as_str().to_owned(),
                Some(Ok(WsMessage::Close(_))) | None => break,
                Some(Ok(_)) => continue, // binary/ping/pong: ignored
                Some(Err(_)) => break,
            },
            // Hub dropped: server shutting down.
            else => break,
        };
        let Ok(req) = serde_json::from_str::<Value>(&text) else {
            let resp = super::jsonrpc::error(
                Value::Null,
                super::jsonrpc::PARSE_ERROR,
                "parse error",
                None,
            );
            if sink.send(WsMessage::Text(resp.to_string().into())).await.is_err() {
                break;
            }
            continue;
        };
        if !req.is_object() {
            continue;
        }
        let id = req.get("id").cloned().unwrap_or(Value::Null);
        let method = req.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let params = req.get("params").cloned().unwrap_or(json!({}));

        // Heartbeat: answered inline without the dispatcher (`handle_ws`).
        let response = if method == "gateway.ping" {
            Some(super::jsonrpc::response(id, json!({ "ok": true })))
        } else {
            dispatch::dispatch(&api, handle, id, &method, params).await
        };
        if let Some(resp) = response {
            if sink.send(WsMessage::Text(resp.to_string().into())).await.is_err() {
                break;
            }
        }
    }
    api.inner.hub.unsubscribe(handle);
}
