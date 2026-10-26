//! hermes-mcp: Model Context Protocol client + tool source
//! (port of the Python hermes-agent MCP machinery: `tools/mcp_tool*.py`,
//! `agent/transports/hermes_tools_mcp_server.py`).
//!
//! Wire contract (JSON-RPC 2.0 over line-delimited stdio, or Streamable HTTP):
//! `initialize` handshake -> `notifications/initialized` -> `tools/list`
//! (cursor-paginated) -> `tools/call` (content blocks + `isError`) -> `ping`.
//!
//! Tool names are namespaced `mcp__<server>__<tool>` exactly like the Python
//! `mcp_prefixed_tool_name`, clamped to 64 chars with a sha256-derived suffix.

pub mod client;
pub mod content;
pub mod jsonrpc;
pub mod naming;
pub mod protocol;
pub mod server;
pub mod source;
pub mod transport;

pub use client::McpClient;
pub use content::render_call_result;
pub use naming::prefixed_tool_name;
pub use protocol::{CallToolResult, ContentBlock, InitializeResult, Tool};
pub use source::{McpServerConfig, McpServerSource, ToolSource};

use hermes_core::model::ToolSpec;

/// Convert an MCP `Tool` into a registry `ToolSpec` with the Python-compatible
/// `mcp__<server>__<tool>` name (tools/mcp_tool_schema.py::_convert_mcp_schema).
pub fn tool_spec(server_name: &str, tool: &Tool) -> ToolSpec {
    ToolSpec {
        name: prefixed_tool_name(server_name, &tool.name),
        description: tool
            .description
            .clone()
            .unwrap_or_else(|| format!("MCP tool {} from {}", tool.name, server_name)),
        schema: tool.input_schema.clone(),
    }
}
