//! JSON-RPC 2.0 framing helpers: request/response envelopes and the typed
//! error ladder the desktop client's shared channel reads (`-32601` unknown
//! method, `-32602` invalid params, `-32700` parse error, `-32603` internal),
//! plus the refusal `data` payloads the canonical protocol layer keys on
//! (`reason: "revision_conflict"` / `"stale_generation"` carry the owner's
//! current fences).

use serde_json::{json, Value};

pub fn response(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "result": result, "id": id })
}

/// `code`/`message` plus the optional `data` object (refusal reasons ride it).
pub fn error(id: Value, code: i64, message: &str, data: Option<Value>) -> Value {
    let mut error = json!({ "code": code, "message": message });
    if let Some(d) = data {
        error["data"] = d;
    }
    json!({ "jsonrpc": "2.0", "error": error, "id": id })
}

pub const PARSE_ERROR: i64 = -32700;
pub const INVALID_REQUEST: i64 = -32600;
pub const METHOD_NOT_FOUND: i64 = -32601;
pub const INVALID_PARAMS: i64 = -32602;
pub const INTERNAL_ERROR: i64 = -32603;
/// Session-layer refusals (busy / revision fences / not found), past the JSON-RPC base.
pub const SESSION_REFUSED: i64 = -32001;

/// A typed refusal whose `data.reason` the client's `protocol.failure()` reads.
pub fn refused(id: Value, reason: &str, message: &str, fences: Option<(u64, u64)>) -> Value {
    let mut data = json!({ "reason": reason });
    if let Some((revision, generation)) = fences {
        data["revision"] = json!(revision);
        data["execution_generation"] = json!(generation);
    }
    error(id, SESSION_REFUSED, message, Some(data))
}
