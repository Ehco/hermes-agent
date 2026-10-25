//! `McpClient` — the JSON-RPC/MCP session over any transport.
//!
//! Mirrors the Python `ClientSession` usage in `tools/mcp_tool_transport.py`:
//! spawn/dial, `initialize` handshake, `notifications/initialized`,
//! capability-gated `tools/list` (cursor-paginated), `tools/call`, `ping`.

use crate::jsonrpc::{self, notification, request, PROTOCOL_VERSION, CLIENT_NAME, CLIENT_VERSION};
use crate::protocol::{
    CallToolParams, CallToolResult, InitializeParams, InitializeResult, ListToolsResult, Tool,
};
use crate::transport::{HttpTransport, StdioTransport, Transport};
use anyhow::{anyhow, Context, Result};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use tokio::time::timeout;

/// Default per-request timeout (Python `_DEFAULT_TOOL_TIMEOUT` = 300s and
/// `_DEFAULT_CONNECT_TIMEOUT` for the handshake).
pub const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A live MCP session. Not generic: either transport plugs in as a
/// `Box<dyn Transport>`, so a hub can hold stdio and HTTP servers alike.
pub struct McpClient {
    transport: Box<dyn Transport>,
    next_id: u64,
    request_timeout: Duration,
    /// Result of the completed handshake; None until `handshake()`.
    pub initialize_result: Option<InitializeResult>,
}

impl McpClient {
    /// Spawn a stdio MCP server child (Python `StdioServerParameters`:
    /// command + args + layered env + optional cwd).
    pub fn stdio(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<Self> {
        Ok(Self::from_transport(Box::new(StdioTransport::spawn(
            command, args, env, cwd,
        )?)))
    }

    /// Connect to a Streamable HTTP MCP endpoint (Python `_run_http`).
    pub fn http(url: &str, headers: HashMap<String, String>) -> Result<Self> {
        Ok(Self::from_transport(Box::new(HttpTransport::new(
            url, headers,
        )?)))
    }

    pub fn from_transport(transport: Box<dyn Transport>) -> Self {
        Self {
            transport,
            next_id: 1,
            request_timeout: DEFAULT_REQUEST_TIMEOUT,
            initialize_result: None,
        }
    }

    /// Override the per-request timeout (handshake, tools/call, …).
    pub fn with_request_timeout(mut self, timeout: Duration) -> Self {
        self.request_timeout = timeout;
        self
    }

    /// One JSON-RPC round-trip: send, read lines until the matching response.
    async fn rpc(&mut self, method: &str, params: Value) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let line = serde_json::to_string(&request(id, method, params))?;
        self.transport.send(&line).await?;
        loop {
            let reply = timeout(self.request_timeout, self.transport.recv())
                .await
                .map_err(|_| {
                    anyhow!(
                        "MCP request {method:?} timed out after {}s",
                        self.request_timeout.as_secs()
                    )
                })??;
            match jsonrpc::parse_response(&reply, id) {
                Ok(Some((_, result))) => return Ok(result),
                Ok(None) => continue, // server notification / foreign id
                Err(e) => return Err(e),
            }
        }
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<()> {
        let line = serde_json::to_string(&notification(method, params))?;
        self.transport.notify(&line).await
    }

    /// `initialize` + `notifications/initialized` — the full handshake.
    pub async fn handshake(&mut self) -> Result<InitializeResult> {
        let params = InitializeParams {
            client_name: CLIENT_NAME.into(),
            client_version: CLIENT_VERSION.into(),
        };
        let raw = self.rpc("initialize", params.to_json()).await?;
        let result: InitializeResult =
            serde_json::from_value(raw).context("malformed initialize result")?;
        // The Python client pins the era to the version WE offered; a server
        // echoing an unsupported version still completes the handshake at the
        // offered version (#113359). Record what the server answered.
        let _ = &result.protocol_version;
        self.notify("notifications/initialized", json!({})).await?;
        // Later HTTP requests carry the negotiated version.
        self.initialize_result = Some(result.clone());
        Ok(result)
    }

    /// One `tools/list` page (cursor pagination per the MCP spec).
    pub async fn list_tools_page(&mut self, cursor: Option<&str>) -> Result<(Vec<Tool>, Option<String>)> {
        let raw = self
            .rpc("tools/list", json!({"cursor": cursor.unwrap_or("")}))
            .await?;
        let page: ListToolsResult =
            serde_json::from_value(raw).context("malformed tools/list result")?;
        Ok((page.tools, page.next_cursor))
    }

    /// The full tool list, following `nextCursor` until exhaustion
        /// (Python `_paginate_full_list`).
    pub async fn list_tools(&mut self) -> Result<Vec<Tool>> {
        let mut all = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let (mut tools, next) = self.list_tools_page(cursor.as_deref()).await?;
            all.append(&mut tools);
            match next {
                Some(c) if !c.is_empty() => cursor = Some(c),
                _ => return Ok(all),
            }
        }
    }

    /// `tools/call` — raw result; use `crate::content::render_call_result`
    /// to turn it into the agent-loop tool-result string.
    pub async fn call_tool(&mut self, name: &str, arguments: &Value) -> Result<CallToolResult> {
        let raw = self
            .rpc("tools/call", CallToolParams { name: name.into(), arguments: arguments.clone() }.to_json())
            .await?;
        let result: CallToolResult =
            serde_json::from_value(raw).context("malformed tools/call result")?;
        Ok(result)
    }

    /// `ping` — must answer an empty result.
    pub async fn ping(&mut self) -> Result<()> {
        let raw = self.rpc("ping", json!({})).await?;
        let _: Value = raw;
        Ok(())
    }

    /// Kill the child / close the HTTP session.
    pub async fn shutdown(&mut self) {
        self.transport.shutdown().await;
    }

    /// Transport diagnostics (child stderr for stdio servers).
    pub fn diagnostics(&self) -> String {
        self.transport.diagnostics()
    }

    /// The protocol version the server answered with (recorded at handshake).
    pub fn negotiated_version(&self) -> Option<String> {
        self.initialize_result
            .as_ref()
            .map(|r| r.protocol_version.clone())
    }

    /// True when the server advertises the `tools` capability — gate before
    /// `tools/list` on prompt-/resource-only servers (Python
    /// `_advertises_tools`, ported from anomalyco/opencode#31271).
    pub fn advertises_tools(&self) -> bool {
        match self.initialize_result.as_ref().map(|r| &r.capabilities) {
            Some(Value::Object(caps)) => caps.get("tools").is_some(),
            // No capability info at all => legacy fallback: assume yes.
            _ => true,
        }
    }

    /// Reference to the negotiated protocol version constant we offer.
    pub fn offered_version(&self) -> &'static str {
        PROTOCOL_VERSION
    }
}
