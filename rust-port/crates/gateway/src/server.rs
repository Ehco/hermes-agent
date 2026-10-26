//! The axum router: `POST /v1/chat` (SSE), `GET /health` / `GET /v1/health`,
//! and a JSON-error fallback for unknown paths. The transport and the
//! `ToolHost` are injected, so tests drive the whole stack with a
//! `ScriptedTransport` and no network.

use std::sync::Arc;

use axum::body::Bytes;
use axum::extract::{Request, State};
use axum::http::header;
use axum::http::StatusCode;
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures::StreamExt;
use hermes_core::agent::ToolHost;
use hermes_core::model::{Message, ToolSpec};
use hermes_core::transport::Transport;
use serde_json::{json, Value};
use tokio::sync::mpsc;

use crate::config::GatewayConfig;
use crate::error::{bad_request, error_response};
use crate::host::RegistryHost;
use crate::middleware::{body_limit_middleware, cors_middleware, security_headers_middleware};
use crate::turn::{run_streaming_turn, TurnEvent};

/// The gateway: one transport (provider wire layer) + one tool host, behind a
/// shared config. Clone-cheap; the router holds a clone as its state.
pub struct Gateway<T, H> {
    pub transport: Arc<T>,
    pub host: Arc<H>,
    pub config: GatewayConfig,
}

impl<T, H> Clone for Gateway<T, H> {
    fn clone(&self) -> Self {
        Self {
            transport: Arc::clone(&self.transport),
            host: Arc::clone(&self.host),
            config: self.config.clone(),
        }
    }
}

impl<T, H> Gateway<T, H>
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    pub fn new(transport: Arc<T>, host: Arc<H>) -> Self {
        Self {
            transport,
            host,
            config: GatewayConfig::default(),
        }
    }

    /// Builder form: same pair, plus configurable guards
    /// (`cors_origins`, `max_body_bytes`).
    pub fn builder(transport: Arc<T>, host: Arc<H>) -> GatewayBuilder<T, H> {
        GatewayBuilder {
            transport,
            host,
            config: GatewayConfig::default(),
        }
    }

    /// The fully-layered router. Layer order mirrors the Python middleware
    /// stack `cors_middleware → body_limit_middleware → security_headers_middleware`.
    pub fn router(&self) -> Router {
        let app = Router::new()
            .route("/health", get(health))
            .route("/v1/health", get(health))
            .route("/v1/chat", post(chat::<T, H>))
            .fallback(fallback_404)
            .with_state(self.clone());
        app.layer(axum::middleware::from_fn_with_state(
            self.config.clone(),
            security_headers_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            self.config.clone(),
            body_limit_middleware,
        ))
        .layer(axum::middleware::from_fn_with_state(
            self.config.clone(),
            cors_middleware,
        ))
    }
}

impl<T> Gateway<T, RegistryHost>
where
    T: Transport + Send + Sync + 'static,
{
    /// The production wiring: tool calls execute on the hermes-tools registry.
    pub fn with_default_tools(transport: Arc<T>) -> Self {
        Self::new(transport, Arc::new(RegistryHost::default()))
    }
}

/// Builder for a `Gateway` — the injection seam tests use to supply a
/// `ScriptedTransport`.
pub struct GatewayBuilder<T, H> {
    transport: Arc<T>,
    host: Arc<H>,
    config: GatewayConfig,
}

impl<T, H> GatewayBuilder<T, H>
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    /// Configure the allowed browser origins (Python `cors_origins`).
    pub fn cors_origins(mut self, origins: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.config.cors_origins = origins.into_iter().map(Into::into).collect();
        self
    }

    /// Override the request-body cap (default `MAX_REQUEST_BYTES`, 10 MB).
    pub fn max_body_bytes(mut self, bytes: usize) -> Self {
        self.config.max_body_bytes = bytes;
        self
    }

    pub fn build(self) -> Gateway<T, H> {
        Gateway {
            transport: self.transport,
            host: self.host,
            config: self.config,
        }
    }
}

// -- Handlers ----------------------------------------------------------------

async fn health() -> Response {
    Json(json!({ "ok": true })).into_response()
}

async fn fallback_404(req: Request) -> Response {
    let message = format!("Unknown path: {} {}", req.method(), req.uri().path());
    error_response(StatusCode::NOT_FOUND, &message, Some("not_found"))
}

/// `POST /v1/chat`: validate → run the agent turn loop → stream SSE events.
async fn chat<T, H>(State(gw): State<Gateway<T, H>>, body: Bytes) -> Response
where
    T: Transport + Send + Sync + 'static,
    H: ToolHost + Send + Sync + 'static,
{
    // Validation ladder, mirroring the Python handlers' order: JSON parses,
    // then it is an object, then the required field is present and well-typed.
    let value: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(_) => return bad_request("Request body must be valid JSON."),
    };
    let Some(obj) = value.as_object() else {
        return bad_request("Request body must be a JSON object.");
    };
    let Some(messages_val) = obj.get("messages") else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "Missing 'messages' field",
            Some("missing_messages"),
        );
    };
    let Some(messages_arr) = messages_val.as_array() else {
        return bad_request("'messages' must be an array.");
    };
    if messages_arr.is_empty() {
        return bad_request("'messages' must not be empty.");
    }
    let messages: Vec<Message> = match serde_json::from_value(Value::Array(messages_arr.clone())) {
        Ok(m) => m,
        Err(e) => return bad_request(&format!("Invalid messages payload: {e}")),
    };
    let tools: Vec<ToolSpec> = match obj.get("tools") {
        Some(v) => match serde_json::from_value(v.clone()) {
            Ok(t) => t,
            Err(e) => return bad_request(&format!("Invalid tools payload: {e}")),
        },
        None => Vec::new(),
    };

    let (tx, rx) = mpsc::channel::<TurnEvent>(32);
    let mut history = messages;
    let host = Arc::clone(&gw.host);
    let transport = Arc::clone(&gw.transport);
    tokio::spawn(async move {
        match run_streaming_turn(transport, tools, &mut history, host, tx.clone()).await {
            Ok(text) => {
                let _ = tx.send(TurnEvent::Done { text }).await;
            }
            Err(e) => {
                let _ = tx
                    .send(TurnEvent::Error {
                        message: format!("{e:#}"),
                    })
                    .await;
            }
        }
        // `tx` drops here: the SSE stream closes after the last frame.
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx).map(|ev| {
        let name = ev.event_name();
        let data = serde_json::to_string(&ev).unwrap_or_else(|_| "{}".into());
        Ok::<_, std::convert::Infallible>(Event::default().event(name).data(data))
    });

    let mut res = Sse::new(stream).into_response();
    // `_sse_headers`: the CORS middleware only decorates the response after the
    // handler returns, so these ride on the SSE response itself.
    res.headers_mut()
        .insert(header::CACHE_CONTROL, "no-cache".parse().unwrap());
    res.headers_mut().insert(
        axum::http::HeaderName::from_static("x-accel-buffering"),
        "no".parse().unwrap(),
    );
    res
}
