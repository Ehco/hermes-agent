//! One error shape for every failure path: the OpenAI-style envelope of
//! `api_server.py::_openai_error` / `_error_response` —
//! `{"error": {"message", "type", "param", "code"}}`.

use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde_json::json;

/// 400 with the bare `{message, type}` envelope (`_invalid_request`).
pub fn bad_request(message: &str) -> Response {
    error_response(StatusCode::BAD_REQUEST, message, None)
}

/// `_error_response`: status + JSON error envelope. `code` narrows the
/// failure kind the way the Python `code=` keyword does (`body_too_large`,
/// `invalid_content_length`, ...).
pub fn error_response(status: StatusCode, message: &str, code: Option<&str>) -> Response {
    let payload = json!({
        "error": {
            "message": message,
            "type": "invalid_request_error",
            "param": null,
            "code": code,
        }
    });
    (status, Json(payload)).into_response()
}
