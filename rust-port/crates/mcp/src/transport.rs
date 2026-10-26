//! Transports: raw send/recv of one JSON-RPC message each way.
//!
//! - `StdioTransport` — line-delimited JSON over a spawned child process
//!   (the MCP stdio transport; the Python SDK's `stdio_client`).
//! - `HttpTransport` — Streamable HTTP: each message is a POST to the MCP
//!   endpoint; responses come back as `application/json` or a
//!   `text/event-stream` body.

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use std::collections::HashMap;
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Child;

/// Where a transport's stderr/diagnostics go for error messages.
pub type SharedLog = std::sync::Arc<tokio::sync::Mutex<String>>;

#[async_trait]
pub trait Transport: Send {
    /// Send one request line and make its response the next `recv` payload.
    async fn send(&mut self, line: &str) -> Result<()>;
    /// Send one notification (no response expected).
    async fn notify(&mut self, line: &str) -> Result<()>;
    /// Receive the next response payload; EOF => transport closed.
    async fn recv(&mut self) -> Result<String>;
    /// Best-effort shutdown (kill the child, drop the session).
    async fn shutdown(&mut self);
    /// Collected child stderr, for error diagnostics.
    fn diagnostics(&self) -> String {
        String::new()
    }
}

/// stdio transport over `tokio::process::Child` (stdin piped for writes,
/// stdout piped for reads, stderr buffered for crash diagnostics — the same
/// "child stderr says why" trick the Python transport uses).
pub struct StdioTransport {
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
    child: std::sync::Arc<tokio::sync::Mutex<Child>>,
    stderr_log: SharedLog,
}

impl StdioTransport {
    /// Spawn the MCP server child. `env` entries are layered over the
    /// inherited environment (Python `_build_safe_env` + `StdioServerParameters.env`).
    pub fn spawn(
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<Self> {
        let mut cmd = tokio::process::Command::new(command);
        cmd.args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        for (k, v) in env {
            cmd.env(k, v);
        }
        if let Some(dir) = cwd {
            cmd.current_dir(dir);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("spawning MCP server {command:?}"))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| anyhow!("MCP server {command:?} has no stdin"))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| anyhow!("MCP server {command:?} has no stdout"))?;
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow!("MCP server {command:?} has no stderr"))?;
        let stderr_log: SharedLog = std::sync::Arc::new(tokio::sync::Mutex::new(String::new()));
        let log_handle = stderr_log.clone();
        tokio::spawn(async move {
            let mut reader = BufReader::new(stderr);
            let mut buf = Vec::new();
            // Child stderr is diagnostics, not wire; cap it to keep memory sane.
            let _ = tokio::io::AsyncReadExt::read_to_end(&mut reader, &mut buf).await;
            let mut slot = log_handle.lock().await;
            *slot = String::from_utf8_lossy(&buf[..buf.len().min(64 * 1024)]).into_owned();
        });
        Ok(Self {
            stdin,
            stdout: BufReader::new(stdout),
            child: std::sync::Arc::new(tokio::sync::Mutex::new(child)),
            stderr_log,
        })
    }
}

#[async_trait]
impl Transport for StdioTransport {
    async fn send(&mut self, line: &str) -> Result<()> {
        self.stdin
            .write_all(line.as_bytes())
            .await
            .context("writing request line to MCP server stdin")?;
        self.stdin
            .write_all(b"\n")
            .await
            .context("writing line terminator to MCP server stdin")?;
        self.stdin
            .flush()
            .await
            .context("flushing MCP server stdin")
    }

    async fn notify(&mut self, line: &str) -> Result<()> {
        self.send(line).await
    }

    async fn recv(&mut self) -> Result<String> {
        let mut line = String::new();
        let n = self
            .stdout
            .read_line(&mut line)
            .await
            .context("reading MCP server stdout")?;
        if n == 0 {
            let log = self.stderr_log.lock().await.clone();
            let detail = if log.trim().is_empty() {
                String::new()
            } else {
                format!(" (stderr: {})", log.trim())
            };
            return Err(anyhow!("MCP server closed the stdio transport{}", detail));
        }
        Ok(line.trim_end_matches(['\r', '\n']).to_string())
    }

    async fn shutdown(&mut self) {
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }

    fn diagnostics(&self) -> String {
        try_lock_string(&self.stderr_log)
    }
}

fn try_lock_string(log: &SharedLog) -> String {
    log.try_lock().map(|s| s.clone()).unwrap_or_default()
}

/// Streamable HTTP transport: every JSON-RPC message is a POST; the server
/// answers each with either a JSON body or a `text/event-stream` whose
/// `data:` lines carry the JSON. Session continuity via the
/// `MCP-Session-Id` response header, protocol era via `MCP-Protocol-Version`.
pub struct HttpTransport {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: Option<String>,
    protocol_version: Option<String>,
    pending: Option<String>,
}

impl HttpTransport {
    pub fn new(url: &str, headers: HashMap<String, String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .context("building MCP HTTP client")?;
        Ok(Self {
            client,
            url: url.to_string(),
            headers,
            session_id: None,
            protocol_version: None,
            pending: None,
        })
    }

    async fn post_json(&mut self, body: &str, expect_reply: bool) -> Result<Option<String>> {
        let mut req = self
            .client
            .post(&self.url)
            .header("Content-Type", "application/json")
            .header("Accept", "application/json, text/event-stream");
        if let Some(sid) = &self.session_id {
            req = req.header("Mcp-Session-Id", sid);
        }
        if let Some(pv) = &self.protocol_version {
            req = req.header("MCP-Protocol-Version", pv);
        }
        for (k, v) in &self.headers {
            req = req.header(k, v);
        }
        let resp = req
            .body(body.to_string())
            .send()
            .await
            .with_context(|| format!("POSTing to MCP endpoint {}", self.url))?;
        // Capture session id + negotiated protocol version for later requests.
        if let Some(sid) = resp
            .headers()
            .get("Mcp-Session-Id")
            .or_else(|| resp.headers().get("mcp-session-id"))
        {
            self.session_id = Some(sid.to_str().unwrap_or_default().to_string());
        }
        let status = resp.status();
        if !status.is_success() {
            // 202-style empty acks for notifications are success at the HTTP layer;
            // anything else non-2xx is a connect/request failure.
            if expect_reply {
                return Err(anyhow!(
                    "MCP endpoint {} returned HTTP {}",
                    self.url,
                    status
                ));
            }
            return Err(anyhow!(
                "MCP endpoint {} rejected a notification with HTTP {}",
                self.url,
                status
            ));
        }
        let ct = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_ascii_lowercase();
        let text = resp.text().await.context("reading MCP HTTP response")?;
        let body = if ct.contains("text/event-stream") {
            sse_payload(&text).ok_or_else(|| anyhow!("no data: payload in SSE response"))?
        } else {
            text
        };
        if expect_reply {
            Ok(Some(body))
        } else {
            Ok(None)
        }
    }
}

/// Extract the first `data:` line payload from an SSE body.
fn sse_payload(body: &str) -> Option<String> {
    body.lines()
        .find_map(|l| l.strip_prefix("data:").map(|d| d.trim_start().to_string()))
}

#[async_trait]
impl Transport for HttpTransport {
    async fn send(&mut self, line: &str) -> Result<()> {
        self.pending = self.post_json(line, true).await?;
        Ok(())
    }

    async fn notify(&mut self, line: &str) -> Result<()> {
        // Notifications: accept any 2xx; a stray response body is ignored.
        self.post_json(line, false).await?;
        Ok(())
    }

    async fn recv(&mut self) -> Result<String> {
        self.pending
            .take()
            .ok_or_else(|| anyhow!("no buffered MCP HTTP response"))
    }

    async fn shutdown(&mut self) {
        // HTTP DELETE closes a Streamable HTTP session per the MCP spec.
        if let Some(sid) = &self.session_id {
            let _ = self
                .client
                .delete(&self.url)
                .header("Mcp-Session-Id", sid)
                .send()
                .await;
        }
        self.session_id = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sse_payload_extracts_data_line() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\"}\n\n";
        assert_eq!(sse_payload(body).unwrap(), "{\"jsonrpc\":\"2.0\"}");
        assert!(sse_payload("event: ping\n\n").is_none());
    }
}
