//! JSON-RPC 2.0 framing and envelope types.
//!
//! The MCP wire is JSON-RPC 2.0: requests carry a numeric id, notifications
//! carry no id, and every response echoes the request id. Over stdio the
//! messages are line-delimited JSON (one object per "\n"); over Streamable
//! HTTP each request is one POST body.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};

/// JSON-RPC / MCP protocol version this client offers in `initialize`
/// (Python `mcp_tool.py`: `LATEST_PROTOCOL_VERSION = "2025-03-26"`,
/// also the `_PROBE_INITIALIZE_BODY` version).
pub const PROTOCOL_VERSION: &str = "2025-03-26";

/// Client identity advertised in the handshake (Python: `client_info`
/// defaults to Implementation(name="hermes-agent", ...)).
pub const CLIENT_NAME: &str = "hermes-agent";
pub const CLIENT_VERSION: &str = "0.1.0";

/// A JSON-RPC error object (`{"code": -32601, "message": "..."}`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct JsonRpcError {
    pub code: i64,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data: Option<Value>,
}

impl std::fmt::Display for JsonRpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "JSON-RPC error {}: {}", self.code, self.message)
    }
}

impl std::error::Error for JsonRpcError {}

/// Build a JSON-RPC request line as a `serde_json::Value`.
pub fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params})
}

/// Build a JSON-RPC notification (no id — never answered).
pub fn notification(method: &str, params: Value) -> Value {
    json!({"jsonrpc": "2.0", "method": method, "params": params})
}

/// Interpret one inbound message. Returns:
/// - `Ok(Some((id, result)))` — a successful response for the expected id
/// - `Ok(None)` — a server notification/request we skip (logging, progress…)
/// - `Err(jsonrpc)` — an explicit JSON-RPC error response
/// - `Err(anyhow)` — malformed framing
pub fn parse_response(line: &str, expected_id: u64) -> Result<Option<(u64, Value)>> {
    let msg: Value = serde_json::from_str(line)
        .with_context(|| format!("malformed JSON-RPC message: {}", truncate(line)))?;
    // Explicit error for our id (or a parse error with a null id — the wire's
    // own complaint about something we sent) is always fatal for the call.
    if let Some(err) = msg.get("error") {
        let err: JsonRpcError =
            serde_json::from_value(err.clone()).context("malformed JSON-RPC error object")?;
        let id = msg.get("id").and_then(Value::as_u64);
        if id.is_none() || id == Some(expected_id) {
            return Err(anyhow!(err.to_string()));
        }
        return Ok(None);
    }
    let id = msg.get("id").and_then(Value::as_u64);
    let id = match id {
        Some(id) => id,
        None => {
            // No id: a server notification (progress, logging, …) is skipped —
            // the Python client never answers those. A message with neither id
            // nor method is genuinely malformed framing.
            if msg.get("method").is_some() {
                return Ok(None);
            }
            return Err(anyhow!("JSON-RPC response without id: {}", truncate(line)));
        }
    };
    if id != expected_id {
        return Ok(None); // stale/duplicate response for another id — skip
    }
    let result = msg
        .get("result")
        .cloned()
        .ok_or_else(|| anyhow!("JSON-RPC response without result"))?;
    Ok(Some((id, result)))
}

fn truncate(s: &str) -> String {
    let mut out = String::new();
    for (i, ch) in s.chars().enumerate() {
        if i >= 120 {
            out.push('…');
            break;
        }
        out.push(ch);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_envelope_shape() {
        let req = request(7, "tools/list", json!({"cursor": ""}));
        assert_eq!(req["jsonrpc"], "2.0");
        assert_eq!(req["id"], 7);
        assert_eq!(req["method"], "tools/list");
        assert_eq!(req["params"]["cursor"], "");
    }

    #[test]
    fn notification_has_no_id() {
        let n = notification("notifications/initialized", json!({}));
        assert!(n.get("id").is_none());
        assert_eq!(n["method"], "notifications/initialized");
    }

    #[test]
    fn parses_matching_response() {
        let line = r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[]}}"#;
        let (id, result) = parse_response(line, 7).unwrap().unwrap();
        assert_eq!(id, 7);
        assert_eq!(result["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn skips_foreign_ids_and_notifications() {
        let line = r#"{"jsonrpc":"2.0","id":3,"result":{}}"#;
        assert!(parse_response(line, 7).unwrap().is_none());
        let note = r#"{"jsonrpc":"2.0","method":"notifications/progress"}"#;
        assert!(parse_response(note, 7).unwrap().is_none());
    }

    #[test]
    fn surfaces_jsonrpc_error() {
        let line = r#"{"jsonrpc":"2.0","id":7,"error":{"code":-32601,"message":"Method not found"}}"#;
        let err = parse_response(line, 7).unwrap_err();
        assert!(err.to_string().contains("-32601"));
        assert!(err.to_string().contains("Method not found"));
    }

    #[test]
    fn rejects_malformed_json() {
        assert!(parse_response("not json at all", 1).is_err());
    }
}
