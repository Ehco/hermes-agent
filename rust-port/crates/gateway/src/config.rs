//! Gateway configuration and the CORS/security-header tables
//! (port of `_CORS_HEADERS`, `_SECURITY_HEADERS`, `_origin_allowed` and
//! `_cors_headers_for_origin` from `gateway/platforms/api_server.py`).

use axum::http::{HeaderName, HeaderValue};

/// 10 MB — accommodates long agent conversations with tool calls
/// (`api_server.py::MAX_REQUEST_BYTES`).
pub const MAX_REQUEST_BYTES: usize = 10_000_000;

#[derive(Clone, Debug)]
pub struct GatewayConfig {
    /// Explicitly allowed browser origins (Python `cors_origins`; `*` allowed).
    pub cors_origins: Vec<String>,
    /// Request body cap applied by the body-limit middleware.
    pub max_body_bytes: usize,
}

impl Default for GatewayConfig {
    fn default() -> Self {
        Self {
            cors_origins: Vec::new(),
            max_body_bytes: MAX_REQUEST_BYTES,
        }
    }
}

impl GatewayConfig {
    /// Allow non-browser clients (empty `Origin`) and explicitly configured
    /// browser origins (`_origin_allowed`).
    pub fn origin_allowed(&self, origin: &str) -> bool {
        origin.is_empty()
            || self.cors_origins.iter().any(|o| o == "*")
            || self.cors_origins.iter().any(|o| o == origin)
    }

    /// CORS headers for an allowed browser origin
    /// (`_cors_headers_for_origin`): `None` when the request is not from a
    /// configured browser origin or none are configured.
    pub fn cors_headers_for_origin(&self, origin: &str) -> Option<Vec<(HeaderName, HeaderValue)>> {
        if origin.is_empty() || self.cors_origins.is_empty() {
            return None;
        }
        let mut headers = base_cors_headers();
        if self.cors_origins.iter().any(|o| o == "*") {
            headers.push((
                HeaderName::from_static("access-control-allow-origin"),
                HeaderValue::from_static("*"),
            ));
            headers.push((
                HeaderName::from_static("access-control-max-age"),
                HeaderValue::from_static("600"),
            ));
            return Some(headers);
        }
        if !self.cors_origins.iter().any(|o| o == origin) {
            return None;
        }
        headers.push((
            HeaderName::from_static("access-control-allow-origin"),
            HeaderValue::from_str(origin).ok()?,
        ));
        headers.push((
            HeaderName::from_static("vary"),
            HeaderValue::from_static("Origin"),
        ));
        headers.push((
            HeaderName::from_static("access-control-max-age"),
            HeaderValue::from_static("600"),
        ));
        Some(headers)
    }
}

/// `_CORS_HEADERS`: the methods/headers half of the CORS answer.
fn base_cors_headers() -> Vec<(HeaderName, HeaderValue)> {
    vec![
        (
            HeaderName::from_static("access-control-allow-methods"),
            HeaderValue::from_static("GET, POST, DELETE, OPTIONS"),
        ),
        (
            HeaderName::from_static("access-control-allow-headers"),
            HeaderValue::from_static(
                "Authorization, Content-Type, Idempotency-Key, X-Hermes-Session-Id",
            ),
        ),
    ]
}

/// `_SECURITY_HEADERS`, added to every response (including errors) by the
/// security-headers middleware.
pub fn security_headers() -> Vec<(HeaderName, HeaderValue)> {
    vec![
        (
            HeaderName::from_static("content-security-policy"),
            HeaderValue::from_static("default-src 'none'; frame-ancestors 'none'"),
        ),
        (
            HeaderName::from_static("permissions-policy"),
            HeaderValue::from_static("camera=(), microphone=(), geolocation=()"),
        ),
        (
            HeaderName::from_static("strict-transport-security"),
            HeaderValue::from_static("max-age=31536000; includeSubDomains"),
        ),
        (
            HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        ),
        (
            HeaderName::from_static("x-frame-options"),
            HeaderValue::from_static("DENY"),
        ),
        (
            HeaderName::from_static("x-xss-protection"),
            HeaderValue::from_static("0"),
        ),
        (
            HeaderName::from_static("referrer-policy"),
            HeaderValue::from_static("no-referrer"),
        ),
    ]
}
