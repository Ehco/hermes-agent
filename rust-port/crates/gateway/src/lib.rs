//! hermes-gateway: the HTTP gateway (port of `gateway/platforms/api_server.py`
//! from the Python hermes-agent).
//!
//! Exposes the agent turn loop as a streaming HTTP API:
//!
//! - `POST /v1/chat` — body `{"messages": [...], "tools": [ToolSpec...]}`.
//!   The response is an SSE stream (`content-type: text/event-stream`,
//!   `cache-control: no-cache`, `x-accel-buffering: no`, mirroring the
//!   Python `_sse_headers`). Each frame carries an `event:` name and a
//!   JSON `data:` payload, exactly the original's `_sse_frame` encoding:
//!
//!   | event         | data                                                  |
//!   |---------------|-------------------------------------------------------|
//!   | `text_delta`  | `{"type":"text_delta","text":"..."}`                  |
//!   | `tool_call`   | `{"type":"tool_call","id","name","arguments"}`        |
//!   | `tool_result` | `{"type":"tool_result","call_id","output"}`           |
//!   | `done`        | `{"type":"done","text":"<final assistant text>"}`     |
//!   | `error`       | `{"type":"error","message":"..."}`                    |
//!
//! - `GET /health` (and `/v1/health`) — `{"ok":true}`.
//! - Unknown paths → 404 with the original's OpenAI-style error envelope
//!   `{"error":{"message":...,"type":"invalid_request_error","param":...,"code":...}}`.
//!
//! Ported guards (`api_server.py` middlewares):
//! - body limit: `MAX_REQUEST_BYTES` = 10 MB; `POST`/`PUT`/`PATCH` with a
//!   `Content-Length` above it → 413 `code:"body_too_large"`; an unparseable
//!   `Content-Length` → 400 `code:"invalid_content_length"`; a body that
//!   overflows mid-read (chunked, no `Content-Length`) → 413 as well.
//! - origin guard: empty `Origin` (non-browser clients) is always allowed;
//!   browser origins must be configured (`cors_origins`, `*` supported).
//!   Disallowed origins get a bare 403; `OPTIONS` preflight answers 200 with
//!   the CORS headers or 403.
//! - security headers on every response (CSP, nosniff, frame options, ...).

pub mod config;
pub mod error;
pub mod host;
pub mod middleware;
pub mod server;
pub mod turn;

pub use config::{GatewayConfig, MAX_REQUEST_BYTES};
pub use host::RegistryHost;
pub use server::{Gateway, GatewayBuilder};
pub use turn::{run_streaming_turn, TurnEvent};
