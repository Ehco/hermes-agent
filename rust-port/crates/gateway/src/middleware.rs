//! Request guards ported from the `api_server.py` middlewares:
//! `cors_middleware`, `body_limit_middleware` and `security_headers_middleware`.

use axum::body::{to_bytes, Body};
use axum::extract::{Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

use crate::config::{security_headers, GatewayConfig};
use crate::error::error_response;

/// Innermost: add the `_SECURITY_HEADERS` set to every handler response.
pub async fn security_headers_middleware(
    _cfg: State<GatewayConfig>,
    req: Request,
    next: Next,
) -> Response {
    let mut res = next.run(req).await;
    for (name, value) in security_headers() {
        // setdefault semantics: an existing header wins.
        if res.headers().contains_key(&name) {
            continue;
        }
        res.headers_mut().insert(name, value);
    }
    res
}

/// Middle: reject oversized request bodies early based on `Content-Length`,
/// then buffer the body (which also catches chunked bodies that overflow
/// mid-read, mirroring the `HTTPRequestEntityTooLarge` catch in the Python
/// middleware).
pub async fn body_limit_middleware(
    State(cfg): State<GatewayConfig>,
    req: Request,
    next: Next,
) -> Response {
    let has_body = req.method() == Method::POST
        || req.method() == Method::PUT
        || req.method() == Method::PATCH;
    if !has_body {
        return next.run(req).await;
    }

    if let Some(cl) = req.headers().get(header::CONTENT_LENGTH) {
        let parsed = cl.to_str().ok().and_then(|v| v.trim().parse::<usize>().ok());
        match parsed {
            Some(n) if n > cfg.max_body_bytes => {
                return error_response(
                    StatusCode::PAYLOAD_TOO_LARGE,
                    "Request body too large.",
                    Some("body_too_large"),
                );
            }
            // A present but unparseable Content-Length is a bad request,
            // never a silent ignore.
            None => {
                return error_response(
                    StatusCode::BAD_REQUEST,
                    "Invalid Content-Length header.",
                    Some("invalid_content_length"),
                );
            }
            _ => {}
        }
    }

    let (parts, body) = req.into_parts();
    let bytes = match to_bytes(body, cfg.max_body_bytes).await {
        Ok(bytes) => bytes,
        Err(_) => {
            // client_max_size tripped mid-read (chunked bodies carry no
            // Content-Length): a proper 413, not the handler's 400.
            return error_response(
                StatusCode::PAYLOAD_TOO_LARGE,
                "Request body too large.",
                Some("body_too_large"),
            );
        }
    };
    next.run(Request::from_parts(parts, Body::from(bytes))).await
}

/// Outermost: origin/behavior guard plus CORS answer. Empty `Origin`
/// (non-browser clients) is always allowed; a browser origin must be
/// configured. Disallowed origins get a bare 403, exactly like the Python
/// `web.Response(status=403)`.
pub async fn cors_middleware(
    State(cfg): State<GatewayConfig>,
    req: Request,
    next: Next,
) -> Response {
    let origin = req
        .headers()
        .get(header::ORIGIN)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if !cfg.origin_allowed(&origin) {
        return StatusCode::FORBIDDEN.into_response();
    }

    let cors_headers = cfg.cors_headers_for_origin(&origin);

    if req.method() == Method::OPTIONS {
        return match cors_headers {
            Some(headers) => {
                let mut res = StatusCode::OK.into_response();
                for (name, value) in headers {
                    res.headers_mut().insert(name, value);
                }
                res
            }
            None => StatusCode::FORBIDDEN.into_response(),
        };
    }

    let mut res = next.run(req).await;
    if let Some(headers) = cors_headers {
        for (name, value) in headers {
            res.headers_mut().insert(name, value);
        }
    }
    res
}
