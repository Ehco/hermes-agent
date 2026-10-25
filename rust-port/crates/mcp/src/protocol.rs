//! MCP protocol message types (initialize / tools/list / tools/call / ping).

use serde::Deserialize;
use serde_json::{json, Value};

/// `initialize` request params. We advertise an empty capability set — the
/// Python client does the same (`ClientCapabilities()` / `{"capabilities":{}}`).
#[derive(Debug, Clone)]
pub struct InitializeParams {
    pub client_name: String,
    pub client_version: String,
}

impl InitializeParams {
    pub fn to_json(&self) -> Value {
        json!({
            "protocolVersion": crate::jsonrpc::PROTOCOL_VERSION,
            "capabilities": {},
            "clientInfo": {"name": self.client_name, "version": self.client_version},
        })
    }
}

/// `initialize` result.
#[derive(Debug, Clone, Deserialize)]
pub struct InitializeResult {
    #[serde(rename = "protocolVersion", default)]
    pub protocol_version: String,
    #[serde(default)]
    pub capabilities: Value,
    #[serde(rename = "serverInfo", default)]
    pub server_info: Option<Implementation>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Implementation {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

/// One tool advertised by `tools/list` (`inputSchema` is the JSON Schema for
/// the arguments object — passed through to the registry as-is).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(rename = "inputSchema", default)]
    pub input_schema: Value,
}

/// One page of `tools/list` (`nextCursor` present iff more pages remain).
#[derive(Debug, Clone, Deserialize)]
pub struct ListToolsResult {
    #[serde(default)]
    pub tools: Vec<Tool>,
    #[serde(rename = "nextCursor", default)]
    pub next_cursor: Option<String>,
}

/// One content block of a `tools/call` result. Only `text` blocks carry
/// model-facing payload; image/audio/resource blocks are described in place
/// (the Python renderer drops unsupported types with a warning, and images
/// only matter for multimodal transports this crate does not serve).
#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(default)]
    pub text: Option<String>,
}

/// `tools/call` result: content blocks plus the `isError` execution flag.
#[derive(Debug, Clone, Deserialize)]
pub struct CallToolResult {
    #[serde(default)]
    pub content: Vec<ContentBlock>,
    #[serde(rename = "isError", default)]
    pub is_error: bool,
    #[serde(rename = "structuredContent", default)]
    pub structured_content: Option<Value>,
}

/// Parameters of `tools/call` (`arguments` defaults to `{}`).
#[derive(Debug, Clone)]
pub struct CallToolParams {
    pub name: String,
    pub arguments: Value,
}

impl CallToolParams {
    pub fn to_json(&self) -> Value {
        json!({"name": self.name, "arguments": self.arguments})
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn initialize_params_shape() {
        let p = InitializeParams {
            client_name: "hermes-agent".into(),
            client_version: "0.1.0".into(),
        }
        .to_json();
        assert_eq!(p["protocolVersion"], "2025-03-26");
        assert_eq!(p["clientInfo"]["name"], "hermes-agent");
        assert_eq!(p["capabilities"], json!({}));
    }

    #[test]
    fn parses_initialize_result() {
        let r: InitializeResult = serde_json::from_value(json!({
            "protocolVersion": "2025-03-26",
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "mock", "version": "1"}
        }))
        .unwrap();
        assert_eq!(r.protocol_version, "2025-03-26");
        assert_eq!(r.server_info.unwrap().name, "mock");
    }

    #[test]
    fn parses_tool_defaults() {
        let t: Tool = serde_json::from_value(json!({"name": "echo"})).unwrap();
        assert_eq!(t.name, "echo");
        assert!(t.description.is_none());
        assert!(t.input_schema.is_null());
    }

    #[test]
    fn parses_call_result_is_error() {
        let r: CallToolResult = serde_json::from_value(json!({
            "content": [{"type": "text", "text": "boom"}],
            "isError": true
        }))
        .unwrap();
        assert!(r.is_error);
        assert_eq!(r.content[0].text.as_deref(), Some("boom"));
        assert!(r.structured_content.is_none());
    }

    #[test]
    fn call_params_defaults_arguments() {
        let p = CallToolParams {
            name: "ping_tool".into(),
            arguments: json!({}),
        };
        assert_eq!(p.to_json(), json!({"name": "ping_tool", "arguments": {}}));
    }
}
