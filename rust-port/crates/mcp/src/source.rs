//! Config-driven MCP server source (port of the Python `mcp_servers` config
//! schema from config.yaml + `tools/mcp_tool.py` bring-up).
//!
//! Config file shape (JSON; the same keys the Python `config.yaml`
//! `mcp_servers:` entries accept):
//!
//! ```json
//! {
//!   "mcp_servers": {
//!     "github": {
//!       "command": "npx",              // stdio server: spawn this…
//!       "args": ["-y", "@github/mcp"], // …with these args
//!       "env": {"GITHUB_TOKEN": "..."},
//!       "cwd": "/optional/working/dir",
//!       "enabled": true,               // default true (mcp_server_enabled)
//!       "timeout": 120,                // per-call seconds (default 300)
//!       "tools": {"include": ["a"], "exclude": ["b*"]}
//!     },
//!     "remote": {
//!       "url": "https://host/mcp",     // Streamable HTTP server
//!       "headers": {"Authorization": "Bearer ..."},
//!       "transport": "http"            // informational; presence of `url` selects HTTP
//!     }
//!   }
//! }
//! ```
//!
//! Tools are namespaced `mcp__<server>__<tool>` (see `crate::naming`) and
//! optionally filtered with fnmatch-style include/exclude globs, matching
//! `tools/mcp_tool_registration.py::_make_tool_filter` (include wins over
//! exclude; `include: []` registers nothing).

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use hermes_core::model::ToolSpec;
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;

use crate::client::McpClient;
use crate::content::{error_result, render_call_result};
use crate::naming::{prefixed_tool_name, sanitize_name_component, MCP_TOOL_NAME_PREFIX};
use crate::tool_spec;

/// One `mcp_servers.<name>` entry (Python config schema subset).
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServerConfig {
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    /// `stdio` | `sse` | `http` — informational in this port; a `url` selects
    /// the Streamable HTTP transport, everything else spawns a child.
    #[serde(default = "default_transport")]
    pub transport: String,
    /// The ONE reader of `enabled` (Python `mcp_server_enabled`): default on.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Per-call timeout seconds (Python `_resolve_tool_timeout`).
    #[serde(default)]
    pub timeout: Option<u64>,
    /// `tools.include` whitelist / `tools.exclude` blacklist (globs).
    #[serde(default)]
    pub tools: Option<ToolsFilter>,
}

fn default_transport() -> String {
    "stdio".into()
}
fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Deserialize, Default)]
pub struct ToolsFilter {
    #[serde(default)]
    pub include: Option<Vec<String>>,
    #[serde(default)]
    pub exclude: Vec<String>,
}

impl ToolsFilter {
    /// Python `_make_tool_filter`: include-mode whitelist (`[]` = register
    /// nothing), else exclude blacklist; include wins over exclude.
    fn accepts(&self, tool_name: &str) -> bool {
        if let Some(include) = &self.include {
            return matches_name_filter(tool_name, include);
        }
        !self.exclude.iter().any(|p| glob_match(p, tool_name))
    }
}

fn matches_name_filter(name: &str, patterns: &[String]) -> bool {
    patterns.iter().any(|p| glob_match(p, name))
}

/// fnmatch-style glob: `*`, `?`, and literal segments.
fn glob_match(pattern: &str, name: &str) -> bool {
    if pattern == name {
        return true;
    }
    let pat: Vec<char> = pattern.chars().collect();
    let nam: Vec<char> = name.chars().collect();
    glob_match_inner(&pat, &nam)
}

fn glob_match_inner(pat: &[char], name: &[char]) -> bool {
    match (pat.first(), name.first()) {
        (Some('*'), _) => {
            glob_match_inner(&pat[1..], name) || (!name.is_empty() && glob_match_inner(pat, &name[1..]))
        }
        (Some('?'), Some(_)) => glob_match_inner(&pat[1..], &name[1..]),
        (Some(p), Some(n)) if p == n => glob_match_inner(&pat[1..], &name[1..]),
        (None, None) => true,
        _ => false,
    }
}

/// The parsed `{"mcp_servers": {...}}` config file.
#[derive(Debug, Clone, Deserialize, Default)]
pub struct McpServersFile {
    #[serde(default)]
    pub mcp_servers: HashMap<String, McpServerConfig>,
}

impl McpServersFile {
    pub fn from_json_str(text: &str) -> Result<Self> {
        serde_json::from_str(text).context("parsing MCP servers config")
    }
}

/// Outcome of connecting to one configured server during discovery.
#[derive(Debug)]
pub struct ServerTools {
    pub server: String,
    pub tools: Vec<ToolSpec>,
    /// Set when the server failed to connect/handshake (Python parks the
    /// server and reports the connect error instead of aborting the batch).
    pub error: Option<String>,
}

/// What a tool source provides to feed the registry.
#[async_trait]
pub trait ToolSource: Send + Sync {
    /// Discover the tools this source contributes (best-effort per server).
    async fn discover(&self) -> Vec<ServerTools>;
    /// Dispatch a call to the server owning `registry_name`, normalizing the
    /// result into the agent-loop tool-result string.
    async fn call_by_registry_name(&self, registry_name: &str, arguments: &Value) -> Result<String>;
}

/// A directory of configured MCP servers (stdio + Streamable HTTP).
///
/// Discovery records the exact raw tool name behind every registry name —
/// Python handlers "close over the original unprefixed tool name" — so
/// `call_by_registry_name` never has to guess through sanitization.
pub struct McpServerSource {
    pub file: McpServersFile,
    tool_owners: Mutex<HashMap<String, (String, String)>>, // registry_name -> (server, raw tool name)
}

impl McpServerSource {
    /// Load `{"mcp_servers": {...}}` from a JSON config file.
    pub fn from_config_file(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading MCP config {}", path.display()))?;
        Self::from_json_str(&text)
    }

    pub fn from_json_str(text: &str) -> Result<Self> {
        Ok(Self {
            file: McpServersFile::from_json_str(text)?,
            tool_owners: Mutex::new(HashMap::new()),
        })
    }

    /// Enabled server names, sorted (deterministic discovery order).
    pub fn server_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .file
            .mcp_servers
            .iter()
            .filter(|(_, cfg)| cfg.enabled)
            .map(|(k, _)| k.clone())
            .collect();
        names.sort();
        names
    }

    pub fn config(&self, server: &str) -> Option<&McpServerConfig> {
        self.file.mcp_servers.get(server)
    }

    /// Connect + handshake one configured server (`name` = config key).
    pub async fn connect(&self, name: &str) -> Result<McpClient> {
        let cfg = self
            .config(name)
            .ok_or_else(|| anyhow!("MCP server '{name}' not configured"))?;
        let mut client = if let Some(url) = &cfg.url {
            McpClient::http(url, cfg.headers.clone())?
        } else {
            let command = cfg
                .command
                .as_deref()
                .ok_or_else(|| anyhow!("MCP server '{name}' has no 'command' in config"))?;
            McpClient::stdio(command, &cfg.args, &cfg.env, cfg.cwd.as_deref())?
        };
        client = client
            .with_request_timeout(Duration::from_secs(cfg.timeout.unwrap_or(300).min(3600)));
        client.handshake().await?;
        Ok(client)
    }

    /// `ToolSpec`s from every enabled server that connects; failed servers
    /// are reported per-row but never abort the batch (Python behaviour).
    pub async fn discover_tool_specs(&self) -> Vec<ServerTools> {
        let mut rows = Vec::new();
        for name in self.server_names() {
            let cfg = match self.config(&name) {
                Some(cfg) => cfg,
                None => continue,
            };
            let row = match self.connect(&name).await {
                Ok(mut client) => {
                    let listed = if client.advertises_tools() {
                        client.list_tools().await.unwrap_or_default()
                    } else {
                        // Prompt-/resource-only server: tools/list would raise
                        // -32601 (Python `_advertises_tools` gate).
                        Vec::new()
                    };
                    let _ = client.shutdown().await;
                    let mut specs = Vec::new();
                    for tool in &listed {
                        if cfg.tools.as_ref().map_or(true, |f| f.accepts(&tool.name)) {
                            let registry = prefixed_tool_name(&name, &tool.name);
                            specs.push(tool_spec(&name, tool));
                            let mut owners = self
                                .tool_owners
                                .lock()
                                .unwrap_or_else(|e| e.into_inner());
                            owners.insert(registry, (name.clone(), tool.name.clone()));
                        }
                    }
                    ServerTools { server: name.clone(), tools: specs, error: None }
                }
                Err(e) => ServerTools {
                    server: name.clone(),
                    tools: Vec::new(),
                    error: Some(format!("{e:#}")),
                },
            };
            rows.push(row);
        }
        rows
    }

    fn owner_of(&self, registry_name: &str) -> Option<(String, String)> {
        self.tool_owners
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(registry_name)
            .cloned()
    }

    /// Best-effort reverse mapping without a prior discovery: the sanitized
    /// remainder after `mcp__<server>__` (exact when the raw tool name had
    /// no characters that sanitization folded).
    fn fallback_raw_name(&self, registry_name: &str, server: &str) -> String {
        let prefix = format!("{}{}__", MCP_TOOL_NAME_PREFIX, sanitize_name_component(server));
        registry_name
            .strip_prefix(&prefix)
            .unwrap_or(registry_name)
            .to_string()
    }

    fn owning_server(&self, registry_name: &str) -> Option<String> {
        self.server_names().into_iter().find(|s| {
            registry_name.starts_with(&format!("{}{}__", MCP_TOOL_NAME_PREFIX, sanitize_name_component(s)))
        })
    }
}

#[async_trait]
impl ToolSource for McpServerSource {
    async fn discover(&self) -> Vec<ServerTools> {
        self.discover_tool_specs().await
    }

    async fn call_by_registry_name(&self, registry_name: &str, arguments: &Value) -> Result<String> {
        let (server, raw) = match self.owner_of(registry_name) {
            Some(found) => found,
            None => {
                let server = self
                    .owning_server(registry_name)
                    .ok_or_else(|| anyhow!("no configured MCP server owns tool {registry_name:?}"))?;
                (server.clone(), self.fallback_raw_name(registry_name, &server))
            }
        };
        let mut client = self.connect(&server).await?;
        let result = client.call_tool(&raw, arguments).await;
        let rendered = match result {
            Ok(result) => render_call_result(&result, registry_name),
            Err(e) => error_result(&format!("{e:#}"), registry_name),
        };
        let _ = client.shutdown().await;
        Ok(rendered)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_config_schema() {
        let src = McpServerSource::from_json_str(
            r#"{
                "mcp_servers": {
                    "github": {"command": "npx", "args": ["-y", "gh"],
                               "env": {"T": "x"}, "enabled": true,
                               "timeout": 42,
                               "tools": {"include": ["create_*"]}},
                    "remote": {"url": "https://host/mcp", "transport": "http",
                               "headers": {"Authorization": "Bearer t"}},
                    "off": {"command": "true", "enabled": false}
                }
            }"#,
        )
        .unwrap();
        assert_eq!(src.server_names(), vec!["github".to_string(), "remote".to_string()]);
        let gh = src.config("github").unwrap();
        assert_eq!(gh.args, vec!["-y".to_string(), "gh".to_string()]);
        assert_eq!(gh.timeout, Some(42));
        assert!(!src.config("off").unwrap().enabled);
        assert!(src.config("remote").unwrap().command.is_none());
    }

    #[test]
    fn disabled_servers_excluded() {
        let src = McpServerSource::from_json_str(
            r#"{"mcp_servers": {"a": {"command": "x", "enabled": false},
                                 "b": {"command": "y"}}}"#,
        )
        .unwrap();
        assert_eq!(src.server_names(), vec!["b".to_string()]);
    }

    #[test]
    fn filters_include_exclude_globs() {
        let f: ToolsFilter = serde_json::from_str(r#"{"include": ["create_*", "read"]}"#).unwrap();
        assert!(f.accepts("create_issue"));
        assert!(f.accepts("read"));
        assert!(!f.accepts("delete_repo"));
        // Empty include list = register nothing (#690 "explicit block-all").
        let f: ToolsFilter = serde_json::from_str(r#"{"include": []}"#).unwrap();
        assert!(!f.accepts("anything"));
        let f: ToolsFilter = serde_json::from_str(r#"{"exclude": ["*_radar_*"]}"#).unwrap();
        assert!(f.accepts("search_repos"));
        assert!(!f.accepts("x_radar_y"));
    }

    #[test]
    fn glob_star_and_question_marks() {
        assert!(glob_match("*", "anything"));
        assert!(glob_match("a?c", "abc"));
        assert!(!glob_match("a?c", "ac"));
        assert!(glob_match("a*c*e", "abcde"));
        assert!(!glob_match("exact", "exac"));
    }
}
