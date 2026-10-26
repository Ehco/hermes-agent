//! RetainDB dialectic modeling — the portable core of
//! `plugins/memory/retaindb/__init__.py` (Honcho's successor memory plugin;
//! the deferred "Honcho dialectic user modeling" subsystem).
//!
//! What ports (everything with a genuinely portable core behind an injected
//! seam): the `_Client` HTTP contracts for the dialectic/agent-self-model
//! routes (`ask_user`, `get_agent_model`, `seed_agent_identity`), the
//! reasoning-level ladder, the agent-model gating, the prefetch-cache
//! semantics (`queue_prefetch` fills, `prefetch` consumes and clears), the
//! agent self-model block formatting, and the context-overlay builder
//! (`_build_overlay` + `_compact`/`_norm`).
//!
//! The ten `retaindb_*` agent tools (the plugin's `get_tool_schemas` /
//! `handle_tool_call` surface) live in [`tools`], with their exact Python
//! schemas, validation order and error strings, delegating to the
//! [`RetainDbClient`] routes and the injected [`MemoryContext`] lifecycle.
//!
//! What stays unported (the original deferral reasons hold for them): the
//! plugin registration/lifecycle and the SQLite write-behind queue — those
//! need the memory-provider lifecycle the Rust host doesn't run (see
//! `crates/tools/port_notes.md` for the documented gaps). HTTP itself is
//! behind [`RetainDbTransport`], so tests stub the wire (no live network).

use serde_json::{json, Value};

/// `_DEFAULT_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.retaindb.com";

/// The injected seam: one JSON request; `method` is uppercased by the caller
/// contract, `path` already carries the query string, `json_body` is `None`
/// for GET/DELETE. `Err` = transport failure (the `requests` exception).
///
/// The two raw methods are `_Client._raw`: multipart upload (the parsed JSON
/// response comes back) and binary download (file content). Host-side auth
/// headers, timeouts and error shaping are the transport's business.
pub trait RetainDbTransport: Send + Sync {
    fn request(&self, method: &str, path: &str, json_body: Option<&Value>)
        -> Result<Value, String>;

    /// Non-JSON request (multipart upload); returns the parsed JSON response.
    fn request_raw(
        &self,
        method: &str,
        path: &str,
        content_type: &str,
        body: Vec<u8>,
    ) -> Result<Value, String>;

    /// Binary download (`read_file_content`); returns the response body.
    fn download(&self, path: &str) -> Result<Vec<u8>, String>;
}

/// `urllib.parse.quote(s, safe="")` — every non-unreserved character escaped.
pub fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            other => out.push_str(&format!("%{other:02X}")),
        }
    }
    out
}

/// The dialectic-relevant routes of `_Client` (headers/auth are host-side:
/// the Bearer/X-API-Key resolution reads scoped secrets).
pub struct RetainDbClient<T: RetainDbTransport> {
    pub transport: T,
    /// `base_url` with trailing slashes stripped (`_Client::__init__`).
    pub base_url: String,
    /// The project is the data partition (`_Client.project`).
    pub project: String,
}

impl<T: RetainDbTransport> RetainDbClient<T> {
    pub fn new(transport: T, base_url: &str, project: &str) -> Self {
        RetainDbClient {
            transport,
            base_url: base_url.trim_end_matches('/').to_string(),
            project: project.to_string(),
        }
    }

    /// `ask_user` — the dialectic session call: POST
    /// `/v1/memory/profile/{user}/ask` with
    /// `{"project", "query", "reasoning_level"}` (8s timeout host-side).
    pub fn ask_user(&self, user_id: &str, query: &str, reasoning_level: &str) -> Result<Value, String> {
        self.transport.request(
            "POST",
            &format!("/v1/memory/profile/{}/ask", percent_encode(user_id)),
            Some(&json!({ "project": self.project, "query": query, "reasoning_level": reasoning_level })),
        )
    }

    /// `get_agent_model` — the agent self-model fetch: GET
    /// `/v1/memory/agent/{agent}/model?project=…`.
    pub fn get_agent_model(&self, agent_id: &str) -> Result<Value, String> {
        self.transport.request(
            "GET",
            &format!(
                "/v1/memory/agent/{}/model?project={}",
                percent_encode(agent_id),
                percent_encode(&self.project)
            ),
            None,
        )
    }

    /// `seed_agent_identity` — seed the agent self-model (e.g. from SOUL.md).
    pub fn seed_agent_identity(&self, agent_id: &str, content: &str, source: &str) -> Result<Value, String> {
        self.transport.request(
            "POST",
            &format!("/v1/memory/agent/{}/seed", percent_encode(agent_id)),
            Some(&json!({ "project": self.project, "content": content, "source": source })),
        )
    }

    /// `get_profile` — the profile half of the context overlay.
    pub fn get_profile(&self, user_id: &str) -> Result<Value, String> {
        self.transport.request(
            "GET",
            &format!(
                "/v1/memory/profile/{}?project={}&include_pending=true",
                percent_encode(user_id),
                percent_encode(&self.project)
            ),
            None,
        )
    }

    /// `_scoped` — the `{project, user_id, session_id, …extra}` body prefix.
    fn scoped(&self, user_id: &str, session_id: &str, extra: &Value) -> Value {
        let mut obj = serde_json::Map::new();
        obj.insert("project".into(), self.project.as_str().into());
        obj.insert("user_id".into(), user_id.into());
        obj.insert("session_id".into(), session_id.into());
        if let Value::Object(map) = extra {
            for (k, v) in map {
                obj.insert(k.clone(), v.clone());
            }
        }
        Value::Object(obj)
    }

    /// `_Client.get_profile` — current route, with the legacy `/v1/memories`
    /// retry (`_with_fallback`): any error on the primary retries the fallback
    /// whose own errors propagate.
    pub fn get_profile_with_fallback(&self, user_id: &str) -> Result<Value, String> {
        self.get_profile(user_id).or_else(|_| {
            self.transport.request(
                "GET",
                &format!(
                    "/v1/memories?project={}&user_id={}&limit=200",
                    percent_encode(&self.project),
                    percent_encode(user_id)
                ),
                None,
            )
        })
    }

    /// `query_context` — POST `/v1/context/query` (context-overlay half).
    pub fn query_context(
        &self,
        user_id: &str,
        session_id: &str,
        query: &str,
        max_tokens: i64,
    ) -> Result<Value, String> {
        self.transport.request(
            "POST",
            "/v1/context/query",
            Some(&self.scoped(
                user_id,
                session_id,
                &json!({ "query": query, "include_memories": true, "max_tokens": max_tokens }),
            )),
        )
    }

    /// `search` — POST `/v1/memory/search`.
    pub fn search(
        &self,
        user_id: &str,
        session_id: &str,
        query: &str,
        top_k: i64,
    ) -> Result<Value, String> {
        self.transport.request(
            "POST",
            "/v1/memory/search",
            Some(&self.scoped(
                user_id,
                session_id,
                &json!({ "query": query, "top_k": top_k, "include_pending": true }),
            )),
        )
    }

    /// `add_memory` — POST `/v1/memory` (sync write) with the legacy
    /// `/v1/memories` fallback.
    pub fn add_memory(
        &self,
        user_id: &str,
        session_id: &str,
        content: &str,
        memory_type: &str,
        importance: f64,
    ) -> Result<Value, String> {
        let body = self.scoped(
            user_id,
            session_id,
            &json!({ "content": content, "memory_type": memory_type, "importance": importance }),
        );
        let primary = {
            let mut obj = body.clone();
            if let Value::Object(map) = &mut obj {
                map.insert("write_mode".into(), "sync".into());
            }
            obj
        };
        self.transport
            .request("POST", "/v1/memory", Some(&primary))
            .or_else(|_| self.transport.request("POST", "/v1/memories", Some(&body)))
    }

    /// `delete_memory` — DELETE `/v1/memory/{id}` with the legacy plural
    /// fallback.
    pub fn delete_memory(&self, memory_id: &str) -> Result<Value, String> {
        let path = format!("/v1/memory/{}", percent_encode(memory_id));
        let fallback = format!("/v1/memories/{}", percent_encode(memory_id));
        self.transport
            .request("DELETE", &path, None)
            .or_else(|_| self.transport.request("DELETE", &fallback, None))
    }

    /// `list_files` — GET `/v1/files`; the prefix joins the query only when
    /// set (`{"prefix": prefix} if prefix else {}`).
    pub fn list_files(&self, prefix: Option<&str>, limit: i64) -> Result<Value, String> {
        let mut path = format!("/v1/files?limit={limit}");
        if let Some(prefix) = prefix.filter(|p| !p.is_empty()) {
            path.push_str(&format!("&prefix={}", percent_encode(prefix)));
        }
        self.transport.request("GET", &path, None)
    }

    /// `get_file` — GET `/v1/files/{id}` (the metadata envelope).
    pub fn get_file(&self, file_id: &str) -> Result<Value, String> {
        self.transport.request(
            "GET",
            &format!("/v1/files/{}", percent_encode(file_id)),
            None,
        )
    }

    /// `read_file_content` — binary GET `/v1/files/{id}/content`.
    pub fn read_file_content(&self, file_id: &str) -> Result<Vec<u8>, String> {
        self.transport
            .download(&format!("/v1/files/{}/content", percent_encode(file_id)))
    }

    /// `upload_file` — multipart POST `/v1/files` (`_raw`): text fields
    /// first (`path`, `scope`, optional `project_id`), then the file part.
    pub fn upload_file(
        &self,
        data: &[u8],
        filename: &str,
        remote_path: &str,
        mime_type: &str,
        scope: &str,
        project_id: Option<&str>,
    ) -> Result<Value, String> {
        let scope = scope.to_uppercase();
        let mut fields: Vec<(&str, String)> =
            vec![("path", remote_path.to_string()), ("scope", scope)];
        if let Some(project_id) = project_id {
            fields.push(("project_id", project_id.to_string()));
        }
        let boundary = format!(
            "hermes-rdb-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let mut body: Vec<u8> = Vec::new();
        for (name, value) in &fields {
            body.extend_from_slice(
                format!(
                    "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
                )
                .as_bytes(),
            );
        }
        body.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{filename}\"\r\nContent-Type: {mime_type}\r\n\r\n"
            )
            .as_bytes(),
        );
        body.extend_from_slice(data);
        body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
        self.transport.request_raw(
            "POST",
            "/v1/files",
            &format!("multipart/form-data; boundary={boundary}"),
            body,
        )
    }

    /// `ingest_file` — POST `/v1/files/{id}/ingest`; empty ids are dropped
    /// from the body (`{k: v for k, v in (...) if v}`).
    pub fn ingest_file(
        &self,
        file_id: &str,
        user_id: Option<&str>,
        agent_id: Option<&str>,
    ) -> Result<Value, String> {
        let mut body = serde_json::Map::new();
        if let Some(user_id) = user_id.filter(|u| !u.is_empty()) {
            body.insert("user_id".into(), user_id.into());
        }
        if let Some(agent_id) = agent_id.filter(|a| !a.is_empty()) {
            body.insert("agent_id".into(), agent_id.into());
        }
        self.transport.request(
            "POST",
            &format!("/v1/files/{}/ingest", percent_encode(file_id)),
            Some(&Value::Object(body)),
        )
    }

    /// `delete_file` — DELETE `/v1/files/{id}`.
    pub fn delete_file(&self, file_id: &str) -> Result<Value, String> {
        self.transport.request(
            "DELETE",
            &format!("/v1/files/{}", percent_encode(file_id)),
            None,
        )
    }
}

/// `_reasoning_level` — query length picks the dialectic reasoning budget.
pub fn reasoning_level(query: &str) -> &'static str {
    let len = query.chars().count();
    if len < 120 {
        "low"
    } else if len < 400 {
        "medium"
    } else {
        "high"
    }
}

/// `_agent_model_or_none` — a self-model with no memories is no model.
pub fn agent_model_or_none(model: &Value) -> Option<&Value> {
    (model.get("memory_count").and_then(Value::as_u64).unwrap_or(0) > 0).then_some(model)
}

/// The prefetch cache (`queue_prefetch` fills `_context_result`,
/// `_dialectic_result`, `_agent_model`; `prefetch()` consumes and clears).
/// The background threads stay host-side; this is the shared-state contract.
#[derive(Debug, Default, Clone)]
pub struct DialecticPrefetch {
    pub context: String,
    pub dialectic: String,
    pub agent_model: Option<Value>,
}

impl DialecticPrefetch {
    /// Queue one result (the `_store` cache-under-lock step; `None` leaves
    /// the previous value untouched — failures log at debug host-side).
    pub fn store(&mut self, context: Option<String>, dialectic: Option<String>, agent_model: Option<Value>) {
        if let Some(context) = context {
            self.context = context;
        }
        if let Some(dialectic) = dialectic {
            self.dialectic = dialectic;
        }
        if let Some(agent_model) = agent_model {
            self.agent_model = Some(agent_model);
        }
    }

    /// `prefetch` — consume the cached results as the context block, clearing
    /// the cache (each prefetched value is used exactly once, next turn).
    pub fn prefetch(&mut self) -> String {
        let context = std::mem::take(&mut self.context);
        let dialectic = std::mem::take(&mut self.dialectic);
        let agent_model = std::mem::take(&mut self.agent_model);
        let mut parts: Vec<String> = Vec::new();
        if !context.is_empty() {
            parts.push(context);
        }
        if !dialectic.is_empty() {
            parts.push(format!("[RetainDB User Synthesis]\n{dialectic}"));
        }
        if let Some(model) = agent_model {
            let lines = agent_self_model_lines(&model);
            if !lines.is_empty() {
                parts.push(format!("[RetainDB Agent Self-Model]\n{}", lines.join("\n")));
            }
        }
        parts.join("\n\n")
    }
}

/// `_AGENT_MODEL_FIELDS` — self-model keys in display order with their line
/// formatting; empty fields are skipped.
pub fn agent_self_model_lines(agent_model: &Value) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(persona) = agent_model.get("persona").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        lines.push(format!("Persona: {persona}"));
    }
    if let Some(instructions) = agent_model.get("persistent_instructions").and_then(Value::as_array) {
        let items: Vec<String> = instructions
            .iter()
            .filter_map(Value::as_str)
            .map(|i| format!("- {i}"))
            .collect();
        if !items.is_empty() {
            lines.push(format!("Instructions:\n{}", items.join("\n")));
        }
    }
    if let Some(style) = agent_model.get("working_style").and_then(Value::as_str).filter(|v| !v.is_empty()) {
        lines.push(format!("Working style: {style}"));
    }
    lines
}

// ── context overlay (`_compact` / `_norm` / `_build_overlay`) ───────────────

/// `_compact` — collapse whitespace, cap at 320 chars.
pub fn compact(value: &str) -> String {
    let collapsed: String = value
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    collapsed.chars().take(320).collect()
}

/// `_norm` — the dedupe key: lowercased, compacted, non-`[a-z0-9 ]` stripped.
fn norm(value: &str) -> String {
    compact(&value.to_lowercase())
        .chars()
        .filter(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == ' ')
        .collect()
}

/// `_build_overlay` — profile + query memories (5 each, compacted, deduped
/// against each other and `local_entries`): the `[RetainDB Context]` block.
pub fn build_overlay(profile: &Value, query_result: &Value, local_entries: &[&str]) -> String {
    let mut seen: std::collections::HashSet<String> = local_entries
        .iter()
        .map(|e| norm(e))
        .filter(|n| !n.is_empty())
        .collect();

    let mut dedupe = |items: Option<&Vec<Value>>| -> Vec<String> {
        let mut out = Vec::new();
        for m in items.into_iter().flatten().take(5) {
            let content = compact(m.get("content").and_then(Value::as_str).unwrap_or(""));
            let key = norm(&content);
            if !content.is_empty() && !seen.contains(&key) {
                seen.insert(key);
                out.push(content);
            }
        }
        out
    };

    let profile_items = dedupe(profile.get("memories").and_then(Value::as_array));
    let query_items = dedupe(query_result.get("results").and_then(Value::as_array));
    if profile_items.is_empty() && query_items.is_empty() {
        return String::new();
    }
    let mut lines = vec!["[RetainDB Context]".to_string(), "Profile:".to_string()];
    if profile_items.is_empty() {
        lines.push("- None".to_string());
    } else {
        for item in &profile_items {
            lines.push(format!("- {item}"));
        }
    }
    lines.push("Relevant memories:".to_string());
    if query_items.is_empty() {
        lines.push("- None".to_string());
    } else {
        for item in &query_items {
            lines.push(format!("- {item}"));
        }
    }
    lines.join("\n")
}

// ── memory-provider lifecycle (`RetainDBMemoryProvider` lifecycle fields) ──

/// What the Python tools read off the initialized `RetainDBMemoryProvider`:
/// the scoped secret (`is_available`), the resolved `base_url`/`project`
/// (`initialize`), and the user/session/agent ids the dispatch binds every
/// call to. The host injects one per session; the default impl resolves the
/// standalone-CLI case from the environment (see `EnvMemoryContext`).
pub trait MemoryContext: Send + Sync {
    /// Scoped secret `RETAINDB_API_KEY` (`get_secret`); empty = unavailable.
    fn api_key(&self) -> String;
    /// Resolved API base URL, trailing slashes stripped (`re.sub(r"/+$", "")`).
    fn base_url(&self) -> String;
    /// The data partition (`_Client.project`).
    fn project(&self) -> String;
    /// `kwargs.get("user_id", "default") or "default"` (standalone: "default").
    fn user_id(&self) -> String;
    /// The `initialize(session_id=…)` value (standalone: "").
    fn session_id(&self) -> String;
    /// `kwargs.get("agent_id", "hermes") or "hermes"` (standalone: "hermes").
    fn agent_id(&self) -> String;
}

fn basename(value: &str) -> String {
    value
        .trim_end_matches(['/', '\\'])
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or("")
        .to_string()
}

fn env_var(name: &str) -> String {
    std::env::var(name).unwrap_or_default().trim().to_string()
}

/// The default `MemoryContext`: how `initialize` resolves for the
/// standalone CLI. Non-secret fields read the profile-scoped env
/// (`RETAINDB_BASE_URL`, `RETAINDB_PROJECT`); Python also reads the
/// Dashboard-written `memory.retaindb` config.yaml block — that leg stays
/// host-side (documented in port_notes). The project falls back to the
/// profile partition: `hermes-<basename(HERMES_HOME)>`, or "default" when the
/// home is bare/absent (`""`/`".hermes"`); user/session/agent take the
/// `initialize` defaults ("default" / "" / "hermes").
pub struct EnvMemoryContext;

impl MemoryContext for EnvMemoryContext {
    fn api_key(&self) -> String {
        env_var("RETAINDB_API_KEY")
    }

    fn base_url(&self) -> String {
        let raw = match env_var("RETAINDB_BASE_URL") {
            v if v.is_empty() => DEFAULT_BASE_URL.to_string(),
            v => v,
        };
        raw.trim_end_matches('/').to_string()
    }

    fn project(&self) -> String {
        let explicit = env_var("RETAINDB_PROJECT");
        if !explicit.is_empty() {
            return explicit;
        }
        let profile = std::env::var("HERMES_HOME")
            .map(|home| basename(&home))
            .unwrap_or_default();
        if profile.is_empty() || profile == ".hermes" {
            "default".to_string()
        } else {
            format!("hermes-{profile}")
        }
    }

    fn user_id(&self) -> String {
        match env_var("HERMES_USER_ID") {
            v if v.is_empty() => "default".to_string(),
            v => v,
        }
    }

    fn session_id(&self) -> String {
        env_var("HERMES_SESSION_ID")
    }

    fn agent_id(&self) -> String {
        match env_var("HERMES_AGENT_ID") {
            v if v.is_empty() => "hermes".to_string(),
            v => v,
        }
    }
}

// ── the ten retaindb_* agent tools (`_SCHEMAS` + `_TOOLS` + dispatch) ───────

pub mod tools {
    use super::*;
    use crate::util::tool_error;

    /// Installed host seams. `TRANSPORT` unset = the provider was never
    /// initialized (`handle_tool_call` → "RetainDB not initialized").
    /// `MEMORY_CONTEXT` unset = the standalone-CLI env default.
    static TRANSPORT: std::sync::RwLock<Option<std::sync::Arc<dyn RetainDbTransport>>> =
        std::sync::RwLock::new(None);
    static MEMORY_CONTEXT: std::sync::RwLock<Option<std::sync::Arc<dyn MemoryContext>>> =
        std::sync::RwLock::new(None);

    /// Install (or with None, clear) the injected HTTP transport.
    pub fn set_transport(transport: Option<std::sync::Arc<dyn RetainDbTransport>>) {
        *TRANSPORT.write().unwrap() = transport;
    }

    /// Install (or with None, clear) the injected memory lifecycle context.
    pub fn set_memory_context(context: Option<std::sync::Arc<dyn MemoryContext>>) {
        *MEMORY_CONTEXT.write().unwrap() = context;
    }

    fn transport() -> Option<std::sync::Arc<dyn RetainDbTransport>> {
        TRANSPORT.read().unwrap().clone()
    }

    fn context() -> std::sync::Arc<dyn MemoryContext> {
        MEMORY_CONTEXT
            .read()
            .unwrap()
            .clone()
            .unwrap_or_else(|| std::sync::Arc::new(EnvMemoryContext))
    }

    fn client(
        transport: std::sync::Arc<dyn RetainDbTransport>,
        ctx: &dyn MemoryContext,
    ) -> RetainDbClient<std::sync::Arc<dyn RetainDbTransport>> {
        RetainDbClient::new(transport, &ctx.base_url(), &ctx.project())
    }

    /// `Arc<dyn RetainDbTransport>` forwards to the trait object, so the
    /// generic `RetainDbClient` can run over the injected seam.
    impl RetainDbTransport for std::sync::Arc<dyn RetainDbTransport> {
        fn request(
            &self,
            method: &str,
            path: &str,
            json_body: Option<&Value>,
        ) -> Result<Value, String> {
            (**self).request(method, path, json_body)
        }
        fn request_raw(
            &self,
            method: &str,
            path: &str,
            content_type: &str,
            body: Vec<u8>,
        ) -> Result<Value, String> {
            (**self).request_raw(method, path, content_type, body)
        }
        fn download(&self, path: &str) -> Result<Vec<u8>, String> {
            (**self).download(path)
        }
    }

    /// `_TEXT_EXTS` — the stored names `retaindb_read_file` decodes as text.
    const TEXT_EXTS: [&str; 8] = [
        ".txt", ".md", ".json", ".csv", ".yaml", ".yml", ".xml", ".html",
    ];

    /// `mimetypes.guess_type(name)[0] or "application/octet-stream"` for the
    /// extensions the tool store actually sees (an approximation of the
    /// stdlib table; see port_notes).
    fn guess_mime(filename: &str) -> String {
        let ext = filename
            .rsplit_once('.')
            .map(|(_, e)| e.to_ascii_lowercase())
            .unwrap_or_default();
        match ext.as_str() {
            "txt" | "text" => "text/plain",
            "md" | "markdown" => "text/markdown",
            "html" | "htm" => "text/html",
            "css" => "text/css",
            "csv" => "text/csv",
            "xml" => "text/xml",
            "json" => "application/json",
            "yaml" | "yml" => "application/yaml",
            "pdf" => "application/pdf",
            "zip" => "application/zip",
            "png" => "image/png",
            "jpg" | "jpeg" => "image/jpeg",
            "gif" => "image/gif",
            "webp" => "image/webp",
            "svg" => "image/svg+xml",
            "mp3" => "audio/mpeg",
            "wav" => "audio/wav",
            "mp4" => "video/mp4",
            "webm" => "video/webm",
            _ => "application/octet-stream",
        }
        .to_string()
    }

    fn is_text_name(name: &str) -> bool {
        TEXT_EXTS.iter().any(|ext| name.ends_with(ext))
    }

    /// `_dispatch`'s required-arg probe: `args.get(required, "")` then Python
    /// truthiness — absent, null, `""`, `False` and `0` all read as missing.
    fn required(args: &Value, key: &str) -> Option<String> {
        match args.get(key) {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => (!s.is_empty()).then(|| s.clone()),
            Some(Value::Bool(b)) => (*b).then(|| b.to_string()),
            Some(Value::Number(n)) => (n.as_f64() != Some(0.0)).then(|| n.to_string()),
            Some(other) => Some(other.to_string()),
        }
    }

    /// `args.get("ingest")` Python truthiness.
    fn truthy(args: &Value, key: &str) -> bool {
        match args.get(key) {
            None | Some(Value::Null) | Some(Value::Bool(false)) => false,
            Some(Value::Bool(_)) => true,
            Some(Value::String(s)) => !s.is_empty(),
            Some(Value::Number(n)) => n.as_f64().map_or(false, |f| f != 0.0),
            Some(_) => true,
        }
    }

    /// `int(...)` for `top_k`/`limit`: missing → default; a float truncates
    /// toward zero; a string parses (Python's `ValueError` text on failure);
    /// a bare null is Python's `int(None)` TypeError.
    fn py_int(args: &Value, key: &str, default: i64) -> Result<i64, String> {
        match args.get(key) {
            None => Ok(default),
            Some(Value::Null) => Err("int() argument must be a string, a bytes-like object or a real number, not 'NoneType'".into()),
            Some(Value::Number(n)) => Ok(n.as_f64().map(|f| f.trunc()).unwrap_or(0.0) as i64),
            Some(Value::String(s)) => s
                .trim()
                .parse::<i64>()
                .map_err(|_| format!("invalid literal for int() with base 10: '{s}'")),
            Some(Value::Bool(b)) => Ok((*b).into()),
            Some(_) => Err(format!("int() argument must be a string, a bytes-like object or a real number, not '{key}'")),
        }
    }

    /// `float(...)` for `importance`.
    fn py_float(args: &Value, key: &str, default: f64) -> Result<f64, String> {
        match args.get(key) {
            None => Ok(default),
            Some(Value::Null) => Err("float() argument must be a string or a real number, not 'NoneType'".into()),
            Some(Value::Number(n)) => Ok(n.as_f64().unwrap_or(0.0)),
            Some(Value::String(s)) => s
                .trim()
                .parse::<f64>()
                .map_err(|_| format!("could not convert string to float: '{s}'")),
            Some(Value::Bool(b)) => Ok(if *b { 1.0 } else { 0.0 }),
            Some(_) => Err(format!("float() argument must be a string or a real number, not '{key}'")),
        }
    }

    /// Run one handler; a failed HTTP contract (`Err`) is the Python
    /// exception path → `tool_error(str(exc))`; success is the raw
    /// `json.dumps` envelope.
    fn run(inner: impl FnOnce() -> Result<Value, String>) -> String {
        match inner() {
            Ok(v) => v.to_string(),
            Err(e) => tool_error(e),
        }
    }

    /// The tool names, in the Python `_SCHEMAS` order.
    pub const TOOL_NAMES: [&str; 10] = [
        "retaindb_profile",
        "retaindb_search",
        "retaindb_context",
        "retaindb_remember",
        "retaindb_forget",
        "retaindb_upload_file",
        "retaindb_list_files",
        "retaindb_read_file",
        "retaindb_ingest_file",
        "retaindb_delete_file",
    ];

    /// The `_SCHEMAS` description for one tool.
    pub fn spec_description(name: &str) -> &'static str {
        match name {
            "retaindb_profile" => "Get the user's stable profile — preferences, facts, and patterns recalled from long-term memory.",
            "retaindb_search" => "Semantic search across stored memories. Returns ranked results with relevance scores.",
            "retaindb_context" => "Synthesized context block — what matters most for the current task, pulled from long-term memory.",
            "retaindb_remember" => "Persist an explicit fact, preference, or decision to long-term memory.",
            "retaindb_forget" => "Delete a specific memory by ID.",
            "retaindb_upload_file" => "Upload a file to the shared RetainDB file store. Returns an rdb:// URI any agent can reference.",
            "retaindb_list_files" => "List files in the shared file store.",
            "retaindb_read_file" => "Read the text content of a stored file by its file ID.",
            "retaindb_ingest_file" => "Chunk, embed, and extract memories from a stored file. Makes its contents searchable.",
            "retaindb_delete_file" => "Delete a stored file.",
            _ => "",
        }
    }

    /// The `_SCHEMAS` parameter schema for one tool (same parameter names,
    /// types, enums, defaults and `required` lists as the Python original).
    pub fn spec_schema(name: &str) -> Value {
        match name {
            "retaindb_profile" => json!({ "type": "object", "properties": {}, "required": [] }),
            "retaindb_search" => json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "What to search for." },
                    "top_k": { "type": "integer", "description": "Max results (default: 8, max: 20)." }
                },
                "required": ["query"]
            }),
            "retaindb_context" => json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Current task or question." }
                },
                "required": ["query"]
            }),
            "retaindb_remember" => json!({
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The fact to remember." },
                    "memory_type": { "type": "string", "enum": ["factual", "preference", "goal", "instruction", "event", "opinion"], "description": "Category (default: factual)." },
                    "importance": { "type": "number", "description": "Importance 0-1 (default: 0.7)." }
                },
                "required": ["content"]
            }),
            "retaindb_forget" => json!({
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string", "description": "Memory ID to delete." }
                },
                "required": ["memory_id"]
            }),
            "retaindb_upload_file" => json!({
                "type": "object",
                "properties": {
                    "local_path": { "type": "string", "description": "Local file path to upload." },
                    "remote_path": { "type": "string", "description": "Destination path, e.g. /reports/q1.pdf" },
                    "scope": { "type": "string", "enum": ["USER", "PROJECT", "ORG"], "description": "Access scope (default: PROJECT)." },
                    "ingest": { "type": "boolean", "description": "Also extract memories from file after upload (default: false)." }
                },
                "required": ["local_path"]
            }),
            "retaindb_list_files" => json!({
                "type": "object",
                "properties": {
                    "prefix": { "type": "string", "description": "Path prefix to filter by, e.g. /reports/" },
                    "limit": { "type": "integer", "description": "Max results (default: 50)." }
                },
                "required": []
            }),
            "retaindb_read_file" => json!({
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "File ID returned from upload or list." }
                },
                "required": ["file_id"]
            }),
            "retaindb_ingest_file" => json!({
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "File ID to ingest." }
                },
                "required": ["file_id"]
            }),
            "retaindb_delete_file" => json!({
                "type": "object",
                "properties": {
                    "file_id": { "type": "string", "description": "File ID to delete." }
                },
                "required": ["file_id"]
            }),
            _ => Value::Null,
        }
    }

    /// `handle_tool_call` — dispatch one tool. Order is Python's:
    /// initialized-client check, unknown tool, required arg, handler.
    pub fn execute(name: &str, args: &Value) -> String {
        let Some(transport) = transport() else {
            return tool_error("RetainDB not initialized");
        };
        let ctx = context();
        let user = ctx.user_id();
        let session = ctx.session_id();
        run(|| {
            let c = client(transport, &*ctx);
            match name {
                "retaindb_profile" => c.get_profile_with_fallback(&user),
                "retaindb_search" => {
                    let query = required(args, "query").ok_or_else(|| "query is required".to_string())?;
                    let top_k = py_int(args, "top_k", 8)?;
                    c.search(&user, &session, &query, top_k.min(20))
                }
                "retaindb_context" => {
                    let query = required(args, "query").ok_or_else(|| "query is required".to_string())?;
                    let raw = c.query_context(&user, &session, &query, 1200)?;
                    let profile = c.get_profile_with_fallback(&user)?;
                    let overlay = build_overlay(&profile, &raw, &[]);
                    Ok(json!({ "context": overlay, "raw": raw }))
                }
                "retaindb_remember" => {
                    let content = required(args, "content").ok_or_else(|| "content is required".to_string())?;
                    let memory_type = args.get("memory_type").and_then(Value::as_str).unwrap_or("factual");
                    let importance = py_float(args, "importance", 0.7)?;
                    c.add_memory(&user, &session, &content, memory_type, importance)
                }
                "retaindb_forget" => {
                    let memory_id = required(args, "memory_id").ok_or_else(|| "memory_id is required".to_string())?;
                    c.delete_memory(&memory_id)
                }
                "retaindb_upload_file" => {
                    let local_path = required(args, "local_path").ok_or_else(|| "local_path is required".to_string())?;
                    let path = std::path::Path::new(&local_path);
                    if !path.exists() {
                        return Ok(json!({ "error": format!("File not found: {local_path}") }));
                    }
                    // (file_safety's raise_if_read_blocked gate stays host-side; see port_notes.)
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let default_remote = format!("/{name}");
                    let remote_path = args
                        .get("remote_path")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .unwrap_or(&default_remote);
                    let scope = args.get("scope").and_then(Value::as_str).unwrap_or("PROJECT");
                    let mime_type = guess_mime(&name);
                    let data = std::fs::read(path).map_err(|e| e.to_string())?;
                    let mut result = c.upload_file(&data, &name, remote_path, &mime_type, scope, None)?;
                    if truthy(args, "ingest") {
                        if let Some(file_id) = result["file"]["id"].as_str().map(str::to_string) {
                            result["ingest"] = c.ingest_file(&file_id, Some(&user), Some(&ctx.agent_id()))?;
                        }
                    }
                    Ok(result)
                }
                "retaindb_list_files" => {
                    let limit = py_int(args, "limit", 50)?;
                    let prefix = args.get("prefix").and_then(Value::as_str).filter(|p| !p.is_empty());
                    c.list_files(prefix, limit)
                }
                "retaindb_read_file" => {
                    let file_id = required(args, "file_id").ok_or_else(|| "file_id is required".to_string())?;
                    let info = c.get_file(&file_id)?;
                    let file = info.get("file").cloned().unwrap_or(Value::Null);
                    let out = json!({
                        "file_id": file_id,
                        "rdb_uri": file.get("rdb_uri").cloned().unwrap_or(Value::Null),
                        "name": file.get("name").cloned().unwrap_or(Value::Null),
                    });
                    let mime = file.get("mime_type").and_then(Value::as_str).unwrap_or("").to_lowercase();
                    let name = file.get("name").and_then(Value::as_str).unwrap_or("");
                    if !mime.starts_with("text/") && !is_text_name(name) {
                        return Ok(json!({
                            "file_id": file_id,
                            "rdb_uri": out["rdb_uri"],
                            "name": out["name"],
                            "content": Value::Null,
                            "note": "Binary file — use retaindb_ingest_file to extract text into memory.",
                        }));
                    }
                    let raw = c.read_file_content(&file_id)?;
                    let text = String::from_utf8_lossy(&raw);
                    let chars: Vec<char> = text.chars().collect();
                    let truncated = chars.len() > 32_000;
                    let content: String = chars.into_iter().take(32_000).collect();
                    Ok(json!({
                        "file_id": file_id,
                        "rdb_uri": out["rdb_uri"],
                        "name": out["name"],
                        "content": content,
                        "truncated": truncated,
                    }))
                }
                "retaindb_ingest_file" => {
                    let file_id = required(args, "file_id").ok_or_else(|| "file_id is required".to_string())?;
                    c.ingest_file(&file_id, Some(&user), Some(&ctx.agent_id()))
                }
                "retaindb_delete_file" => {
                    let file_id = required(args, "file_id").ok_or_else(|| "file_id is required".to_string())?;
                    c.delete_file(&file_id)
                }
                other => Err(format!("Unknown tool: {other}")),
            }
        })
    }

    // --- Visibility (Python `memory_provider_tools_enabled` + `is_available`)

    /// The `memory` toolset leg (`memory_provider_tools_enabled`): the Rust
    /// registry is static, so toolset enablement pins to
    /// `HERMES_MEMORY_TOOLSET` — unset/true/1/yes exposes the tools (Python's
    /// default: the full agent always carries the memory tool);
    /// false/0/no withholds them (the `disabled_toolsets` leg).
    pub fn memory_toolset_enabled() -> bool {
        match std::env::var("HERMES_MEMORY_TOOLSET") {
            Ok(v) => !matches!(v.trim().to_lowercase().as_str(), "false" | "0" | "no"),
            Err(_) => true,
        }
    }

    /// Python `is_available`: the scoped API secret must be set.
    pub fn provider_available() -> bool {
        !context().api_key().is_empty()
    }

    /// Both legs of the Python exposure gate: the memory toolset enabled AND
    /// the provider available (`memory_provider_tools_exposed` ∧
    /// `is_available`). The static registry always advertises the ten specs;
    /// hosts filter with this.
    pub fn retaindb_tools_visible() -> bool {
        memory_toolset_enabled() && provider_available()
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use serde_json::Value;

        /// Serializes the global seams and process env across tests. The env
        /// half also takes kanban's `ENV_LOCK` — `HERMES_HOME` is shared.
        pub(super) static TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

        fn lock() -> std::sync::MutexGuard<'static, ()> {
            TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner())
        }

        /// Hold both locks: the seam lock, then kanban's env lock (this
        /// ordering is the only one used, so no cycle is possible).
        fn env_lock() -> (std::sync::MutexGuard<'static, ()>, std::sync::MutexGuard<'static, ()>) {
            let seam = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            let env = crate::kanban::tests::ENV_LOCK
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            (seam, env)
        }

        /// Records calls; answers are scripted per method.
        struct StubTransport {
            calls: std::sync::Mutex<Vec<(String, String, Option<Value>)>>,
            json_responses: std::sync::Mutex<Vec<Result<Value, String>>>,
            raw_responses: std::sync::Mutex<Vec<Result<Value, String>>>,
            downloads: std::sync::Mutex<Vec<Result<Vec<u8>, String>>>,
        }

        impl StubTransport {
            fn new(responses: Vec<Result<Value, String>>) -> Self {
                StubTransport {
                    calls: std::sync::Mutex::new(Vec::new()),
                    json_responses: std::sync::Mutex::new(responses),
                    raw_responses: std::sync::Mutex::new(Vec::new()),
                    downloads: std::sync::Mutex::new(Vec::new()),
                }
            }

            fn with_raw(mut self, raw: Vec<Result<Value, String>>) -> Self {
                self.raw_responses = std::sync::Mutex::new(raw);
                self
            }
        }

        impl RetainDbTransport for StubTransport {
            fn request(
                &self,
                method: &str,
                path: &str,
                json_body: Option<&Value>,
            ) -> Result<Value, String> {
                self.calls.lock().unwrap().push((
                    method.to_string(),
                    path.to_string(),
                    json_body.cloned(),
                ));
                self.json_responses.lock().unwrap().remove(0)
            }
            fn request_raw(
                &self,
                method: &str,
                path: &str,
                _content_type: &str,
                body: Vec<u8>,
            ) -> Result<Value, String> {
                self.calls.lock().unwrap().push((
                    method.to_string(),
                    path.to_string(),
                    Some(Value::String(String::from_utf8_lossy(&body).to_string())),
                ));
                self.raw_responses.lock().unwrap().remove(0)
            }
            fn download(&self, path: &str) -> Result<Vec<u8>, String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(("GET-DOWNLOAD".to_string(), path.to_string(), None));
                self.downloads.lock().unwrap().remove(0)
            }
        }

        fn calls(stub: &StubTransport) -> Vec<(String, String, Option<Value>)> {
            stub.calls.lock().unwrap().clone()
        }

        fn set_env(kv: &[(&str, &str)]) {
            for (k, v) in kv {
                std::env::set_var(k, v);
            }
        }

        fn clear_env(keys: &[&str]) {
            for k in keys {
                std::env::remove_var(k);
            }
        }

        #[test]
        fn not_initialized_beats_required_validation() {
            let _guard = lock();
            set_transport(None);
            // Python: the client check precedes _dispatch's required probe.
            let out = execute("retaindb_search", &json!({}));
            assert_eq!(
                out,
                tool_error("RetainDB not initialized"),
                "{out}"
            );
        }

        #[test]
        fn unknown_tool_reports_the_name() {
            let _guard = lock();
            set_transport(Some(std::sync::Arc::new(StubTransport::new(vec![]))));
            let out = execute("retaindb_frobnicate", &json!({}));
            assert_eq!(out, tool_error("Unknown tool: retaindb_frobnicate"));
            set_transport(None);
        }

        #[test]
        fn required_args_are_probed_with_python_truthiness() {
            let _guard = lock();
            let stub = StubTransport::new(vec![]);
            set_transport(Some(std::sync::Arc::new(stub)));
            for (name, key) in [
                ("retaindb_search", "query"),
                ("retaindb_context", "query"),
                ("retaindb_remember", "content"),
                ("retaindb_forget", "memory_id"),
                ("retaindb_upload_file", "local_path"),
                ("retaindb_read_file", "file_id"),
                ("retaindb_ingest_file", "file_id"),
                ("retaindb_delete_file", "file_id"),
            ] {
                let out = execute(name, &json!({}));
                assert!(out.contains(&format!("{key} is required")), "{name}: {out}");
                let out = execute(name, &json!({ (key): "" }));
                assert!(out.contains(&format!("{key} is required")), "{name}: {out}");
            }
            // The unprofiled tools (profile, list_files) take no required arg.
            set_transport(None);
        }

        struct StubContext {
            base_url: String,
            project: String,
            user_id: String,
            session_id: String,
            agent_id: String,
        }

        impl MemoryContext for StubContext {
            fn api_key(&self) -> String {
                "rdb-key".into()
            }
            fn base_url(&self) -> String {
                self.base_url.clone()
            }
            fn project(&self) -> String {
                self.project.clone()
            }
            fn user_id(&self) -> String {
                self.user_id.clone()
            }
            fn session_id(&self) -> String {
                self.session_id.clone()
            }
            fn agent_id(&self) -> String {
                self.agent_id.clone()
            }
        }

        /// Install a stub transport + context, run `body`, tear the seams down.
        fn with_seams(
            stub: StubTransport,
            body: impl FnOnce(&StubTransport) -> (),
        ) {
            let arc = std::sync::Arc::new(stub);
            let stub_ref = arc.clone();
            set_transport(Some(arc));
            set_memory_context(Some(std::sync::Arc::new(StubContext {
                base_url: "https://api.retaindb.com/".into(),
                project: "hermes-main".into(),
                user_id: "u1".into(),
                session_id: "s1".into(),
                agent_id: "hermes".into(),
            })));
            body(&*stub_ref);
            set_transport(None);
            set_memory_context(None);
        }

        #[test]
        fn search_posts_the_scoped_body_with_a_clamped_top_k() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({ "results": ["x"] }))]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_search", &json!({ "query": "q", "top_k": 25 }));
                assert_eq!(out, json!({ "results": ["x"] }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].0, "POST");
                assert_eq!(calls[0].1, "/v1/memory/search");
                assert_eq!(
                    calls[0].2.clone().unwrap(),
                    json!({
                        "project": "hermes-main", "user_id": "u1", "session_id": "s1",
                        "query": "q", "top_k": 20, "include_pending": true,
                    }),
                );
            });
        }

        #[test]
        fn search_defaults_top_k_to_eight() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({ "results": [] }))]);
            with_seams(stub, |stub| {
                execute("retaindb_search", &json!({ "query": "q" }));
                let calls = calls(stub);
                assert_eq!(calls[0].2.clone().unwrap()["top_k"], 8);
            });
        }

        #[test]
        fn remember_posts_a_sync_write_and_defaults() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({ "id": "m1" }))]);
            with_seams(stub, |stub| {
                let out = execute(
                    "retaindb_remember",
                    &json!({ "content": "Prefers Rust.", "memory_type": "preference", "importance": 0.9 }),
                );
                assert_eq!(out, json!({ "id": "m1" }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 1, "no fallback on success");
                assert_eq!(calls[0].1, "/v1/memory");
                assert_eq!(
                    calls[0].2.clone().unwrap(),
                    json!({
                        "project": "hermes-main", "user_id": "u1", "session_id": "s1",
                        "content": "Prefers Rust.", "memory_type": "preference",
                        "importance": 0.9, "write_mode": "sync",
                    }),
                );
            });
        }

        #[test]
        fn remember_falls_back_to_the_legacy_route() {
            let _guard = lock();
            let stub = StubTransport::new(vec![
                Err("RetainDB POST /v1/memory failed (500): nope".into()),
                Ok(json!({ "id": "m2" })),
            ]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_remember", &json!({ "content": "fact" }));
                assert_eq!(out, json!({ "id": "m2" }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[1].1, "/v1/memories");
                let body = calls[1].2.clone().unwrap();
                // The fallback carries the bare body (no write_mode)…
                assert_eq!(body["write_mode"], Value::Null);
                // …and the defaults: memory_type=factual, importance=0.7.
                assert_eq!(body["memory_type"], "factual");
                assert_eq!(body["importance"], 0.7);
            });
        }

        #[test]
        fn forget_falls_back_to_the_legacy_route() {
            let _guard = lock();
            let stub = StubTransport::new(vec![
                Err("RetainDB DELETE /v1/memory/x failed (404): gone".into()),
                Ok(json!({ "ok": true })),
            ]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_forget", &json!({ "memory_id": "m #7" }));
                assert_eq!(out, json!({ "ok": true }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[0].0, "DELETE");
                assert_eq!(calls[0].1, "/v1/memory/m%20%237");
                assert_eq!(calls[1].1, "/v1/memories/m%20%237");
                assert!(calls[0].2.is_none() && calls[1].2.is_none());
            });
        }

        #[test]
        fn profile_falls_back_to_the_legacy_memories_route() {
            let _guard = lock();
            let stub = StubTransport::new(vec![
                Err("RetainDB GET /v1/memory/profile/u1 failed (404): none".into()),
                Ok(json!({ "memories": [] })),
            ]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_profile", &json!({}));
                assert_eq!(out, json!({ "memories": [] }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 2);
                assert_eq!(
                    calls[0].1,
                    "/v1/memory/profile/u1?project=hermes-main&include_pending=true"
                );
                assert_eq!(
                    calls[1].1,
                    "/v1/memories?project=hermes-main&user_id=u1&limit=200"
                );
            });
        }

        #[test]
        fn context_returns_the_overlay_envelope() {
            let _guard = lock();
            let stub = StubTransport::new(vec![
                Ok(json!({ "results": [{ "content": "Ships daily." }] })),
                Ok(json!({ "memories": [{ "content": "Prefers Rust." }] })),
            ]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_context", &json!({ "query": "how do I ship?" }));
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(parsed["raw"]["results"][0]["content"], "Ships daily.");
                assert_eq!(
                    parsed["context"].as_str().unwrap().split('\n').next().unwrap(),
                    "[RetainDB Context]"
                );
                let block = parsed["context"].as_str().unwrap();
                assert!(block.contains("- Prefers Rust."), "{block}");
                assert!(block.contains("- Ships daily."), "{block}");
                let calls = calls(stub);
                assert_eq!(calls[0].1, "/v1/context/query");
                assert_eq!(
                    calls[0].2.clone().unwrap(),
                    json!({
                        "project": "hermes-main", "user_id": "u1", "session_id": "s1",
                        "query": "how do I ship?", "include_memories": true, "max_tokens": 1200,
                    }),
                );
            });
        }

        #[test]
        fn list_files_carries_the_default_limit_and_optional_prefix() {
            let _guard = lock();
            let stub = StubTransport::new(vec![
                Ok(json!({ "files": [] })),
                Ok(json!({ "files": ["a"] })),
                Ok(json!({ "files": ["b"] })),
            ]);
            with_seams(stub, |stub| {
                execute("retaindb_list_files", &json!({}));
                execute("retaindb_list_files", &json!({ "prefix": "/reports/" }));
                execute("retaindb_list_files", &json!({ "prefix": "", "limit": 5 }));
                let calls = calls(stub);
                assert_eq!(calls[0].1, "/v1/files?limit=50");
                assert_eq!(calls[1].1, "/v1/files?limit=50&prefix=%2Freports%2F");
                assert_eq!(calls[2].1, "/v1/files?limit=5");
            });
        }

        #[test]
        fn delete_file_uses_the_plain_route() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({ "ok": true }))]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_delete_file", &json!({ "file_id": "f/1" }));
                assert_eq!(out, json!({ "ok": true }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].0, "DELETE");
                assert_eq!(calls[0].1, "/v1/files/f%2F1");
            });
        }

        #[test]
        fn ingest_file_binds_user_and_agent() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({ "memories": 3 }))]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_ingest_file", &json!({ "file_id": "f9" }));
                assert_eq!(out, json!({ "memories": 3 }).to_string(), "{out}");
                let calls = calls(stub);
                assert_eq!(calls[0].0, "POST");
                assert_eq!(calls[0].1, "/v1/files/f9/ingest");
                assert_eq!(
                    calls[0].2.clone().unwrap(),
                    json!({ "user_id": "u1", "agent_id": "hermes" }),
                );
            });
        }

        #[test]
        fn upload_errors_for_a_missing_local_file() {
            let _guard = lock();
            let stub = StubTransport::new(vec![]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_upload_file", &json!({ "local_path": "nope-xyz.txt" }));
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(parsed["error"], "File not found: nope-xyz.txt");
                assert!(calls(stub).is_empty(), "no HTTP for a missing file");
            });
        }

        #[test]
        fn upload_posts_multipart_with_the_defaulting_fields() {
            let _guard = lock();
            let path = std::env::temp_dir().join("hermes-rdb-upload-test.txt");
            std::fs::write(&path, b"hello retained world").unwrap();
            let stub = StubTransport::new(vec![]).with_raw(vec![Ok(json!({ "file": { "id": "f1", "rdb_uri": "rdb://f1" } }))]);
            with_seams(stub, |stub| {
                let local = path.to_string_lossy().to_string();
                let out = execute("retaindb_upload_file", &json!({ "local_path": local }));
                std::fs::remove_file(&path).unwrap();
                assert_eq!(
                    out,
                    json!({ "file": { "id": "f1", "rdb_uri": "rdb://f1" } }).to_string(),
                    "{out}"
                );
                let calls = calls(stub);
                assert_eq!(calls.len(), 1);
                assert_eq!(calls[0].0, "POST");
                assert_eq!(calls[0].1, "/v1/files");
                let body = calls[0].2.clone().unwrap().as_str().unwrap().to_string();
                assert!(body.contains("name=\"path\""), "{body}");
                // remote_path defaults to /<basename>; scope defaults to PROJECT.
                assert!(body.contains("/hermes-rdb-upload-test.txt"), "{body}");
                assert!(body.contains("PROJECT"), "{body}");
                assert!(body.contains("filename=\"hermes-rdb-upload-test.txt\""), "{body}");
                assert!(body.contains("hello retained world"), "{body}");
            });
        }

        #[test]
        fn upload_with_ingest_chains_the_file_ingest() {
            let _guard = lock();
            let path = std::env::temp_dir().join("hermes-rdb-upload-ingest.txt");
            std::fs::write(&path, b"ingest me").unwrap();
            let stub = StubTransport::new(vec![Ok(json!({ "memories": 2 }))])
                .with_raw(vec![Ok(json!({ "file": { "id": "f1" } }))]);
            with_seams(stub, |stub| {
                let local = path.to_string_lossy().to_string();
                let out = execute(
                    "retaindb_upload_file",
                    &json!({ "local_path": local, "ingest": true }),
                );
                std::fs::remove_file(&path).unwrap();
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(parsed["ingest"]["memories"], 2, "{out}");
                let calls = calls(stub);
                assert_eq!(calls.len(), 2);
                assert_eq!(calls[1].1, "/v1/files/f1/ingest");
                assert_eq!(
                    calls[1].2.clone().unwrap(),
                    json!({ "user_id": "u1", "agent_id": "hermes" }),
                );
            });
        }

        #[test]
        fn read_file_returns_text_with_the_truncation_flag() {
            let _guard = lock();
            let long_body = "x".repeat(40_000);
            let stub = StubTransport::new(vec![Ok(json!({
                "file": { "rdb_uri": "rdb://f2", "name": "notes.txt", "mime_type": "text/plain" }
            }))]);
            with_seams(stub, |stub| {
                stub.downloads.lock().unwrap().push(Ok(long_body.clone().into_bytes()));
                let out = execute("retaindb_read_file", &json!({ "file_id": "f2" }));
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(parsed["file_id"], "f2");
                assert_eq!(parsed["rdb_uri"], "rdb://f2");
                assert_eq!(parsed["name"], "notes.txt");
                assert_eq!(parsed["truncated"], true);
                assert_eq!(parsed["content"].as_str().unwrap().len(), 32_000);
                let calls = calls(stub);
                assert_eq!(calls[1].0, "GET-DOWNLOAD");
                assert_eq!(calls[1].1, "/v1/files/f2/content");
            });
        }

        #[test]
        fn read_file_flags_binary_files_instead_of_decoding() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Ok(json!({
                "file": { "rdb_uri": "rdb://f3", "name": "chart.png", "mime_type": "image/png" }
            }))]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_read_file", &json!({ "file_id": "f3" }));
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(parsed["content"], Value::Null);
                assert_eq!(
                    parsed["note"],
                    "Binary file — use retaindb_ingest_file to extract text into memory."
                );
                let calls = calls(stub);
                assert_eq!(calls.len(), 1, "no download for a binary file");
            });
        }

        #[test]
        fn transport_failures_surface_as_error_envelopes() {
            let _guard = lock();
            let stub = StubTransport::new(vec![Err("RetainDB POST /v1/memory/search failed (401): bad key".into())]);
            with_seams(stub, |stub| {
                let out = execute("retaindb_search", &json!({ "query": "q" }));
                let parsed: Value = serde_json::from_str(&out).unwrap();
                assert_eq!(
                    parsed["error"],
                    "RetainDB POST /v1/memory/search failed (401): bad key"
                );
                assert_eq!(calls(stub).len(), 1);
            });
        }

        #[test]
        fn int_coercion_matches_python_edges() {
            let _guard = lock();
            // A float truncates toward zero (int(3.9) == 3).
            let stub = StubTransport::new(vec![Ok(json!({}))]);
            with_seams(stub, |stub| {
                execute("retaindb_search", &json!({ "query": "q", "top_k": 3.9 }));
                assert_eq!(calls(stub)[0].2.clone().unwrap()["top_k"], 3);
            });
            // An unparseable string is Python's ValueError text.
            let stub = StubTransport::new(vec![]);
            with_seams(stub, |_| {
                assert_eq!(
                    py_int(&json!({ "top_k": "abc" }), "top_k", 8).unwrap_err(),
                    "invalid literal for int() with base 10: 'abc'"
                );
                assert_eq!(
                    py_float(&json!({ "importance": "high" }), "importance", 0.7).unwrap_err(),
                    "could not convert string to float: 'high'"
                );
                assert_eq!(py_int(&json!({ "top_k": null }), "top_k", 8).unwrap_err().contains("not 'NoneType'"), true);
            });
        }

        #[test]
        fn the_ten_schemas_match_the_python_surface() {
            assert_eq!(TOOL_NAMES.len(), 10);
            for name in TOOL_NAMES {
                let description = spec_description(name);
                assert!(!description.is_empty(), "{name} description");
                let schema = spec_schema(name);
                assert_eq!(schema["type"], "object", "{name}");
                assert!(schema["properties"].is_object(), "{name}");
                assert!(schema["required"].is_array(), "{name}");
            }
            // Spot-check the enum/default-bearing ones against _SCHEMAS.
            assert_eq!(
                spec_schema("retaindb_remember")["properties"]["memory_type"]["enum"],
                json!(["factual", "preference", "goal", "instruction", "event", "opinion"])
            );
            assert_eq!(
                spec_schema("retaindb_upload_file")["properties"]["scope"]["enum"],
                json!(["USER", "PROJECT", "ORG"])
            );
            assert_eq!(
                spec_schema("retaindb_search")["required"],
                json!(["query"])
            );
        }

        #[test]
        fn env_context_resolves_the_standalone_cli_case() {
            let (_seam, _env) = env_lock();
            clear_env(&["RETAINDB_BASE_URL", "RETAINDB_PROJECT", "HERMES_HOME", "HERMES_USER_ID", "HERMES_AGENT_ID", "HERMES_SESSION_ID"]);
            let ctx = EnvMemoryContext;
            // Bare/absent home -> the default partition; user/agent take the
            // initialize defaults; session starts empty.
            assert_eq!(ctx.project(), "default");
            assert_eq!(ctx.user_id(), "default");
            assert_eq!(ctx.agent_id(), "hermes");
            assert_eq!(ctx.session_id(), "");
            // base_url: default, trailing slashes stripped when set.
            assert_eq!(ctx.base_url(), DEFAULT_BASE_URL);
            set_env(&[("RETAINDB_BASE_URL", "https://api.retaindb.com///")]);
            assert_eq!(ctx.base_url(), "https://api.retaindb.com");
            // A profiled home names the partition.
            set_env(&[("HERMES_HOME", "/home/u/.hermes/profiles/work")]);
            assert_eq!(ctx.project(), "hermes-work");
            set_env(&[("HERMES_HOME", "/home/u/.hermes")]);
            assert_eq!(ctx.project(), "default");
            // Explicit project wins over the profile fallback.
            set_env(&[("RETAINDB_PROJECT", "acme")]);
            assert_eq!(ctx.project(), "acme");
            clear_env(&["RETAINDB_BASE_URL", "RETAINDB_PROJECT", "HERMES_HOME"]);
        }

        #[test]
        fn visibility_gates_mirror_the_python_exposure() {
            let (_seam, _env) = env_lock();
            // The toolset leg: on by default (the memory tool is always
            // present in the full agent), pinned off by HERMES_MEMORY_TOOLSET.
            clear_env(&["HERMES_MEMORY_TOOLSET"]);
            assert!(memory_toolset_enabled());
            set_env(&[("HERMES_MEMORY_TOOLSET", "false")]);
            assert!(!memory_toolset_enabled());
            set_env(&[("HERMES_MEMORY_TOOLSET", "1")]);
            assert!(memory_toolset_enabled());
            clear_env(&["HERMES_MEMORY_TOOLSET"]);
            // Availability: the scoped API secret must be set (is_available).
            clear_env(&["RETAINDB_API_KEY"]);
            set_memory_context(None);
            assert!(!provider_available());
            assert!(!retaindb_tools_visible());
            set_env(&[("RETAINDB_API_KEY", "rdb-key")]);
            assert!(provider_available());
            assert!(retaindb_tools_visible());
            // The toolset gate can withhold an available provider.
            set_env(&[("HERMES_MEMORY_TOOLSET", "no")]);
            assert!(!retaindb_tools_visible());
            clear_env(&["RETAINDB_API_KEY", "HERMES_MEMORY_TOOLSET"]);
            set_memory_context(None);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Records (method, path, body) and answers from a script.
    struct RecordingTransport {
        calls: std::sync::Mutex<Vec<(String, String, Option<Value>)>>,
        responses: std::sync::Mutex<Vec<Result<Value, String>>>,
    }

    impl RetainDbTransport for RecordingTransport {
        fn request(
            &self,
            method: &str,
            path: &str,
            json_body: Option<&Value>,
        ) -> Result<Value, String> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                json_body.cloned(),
            ));
            self.responses.lock().unwrap().remove(0)
        }

        fn request_raw(
            &self,
            method: &str,
            path: &str,
            _content_type: &str,
            _body: Vec<u8>,
        ) -> Result<Value, String> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                None,
            ));
            self.responses.lock().unwrap().remove(0)
        }

        fn download(&self, _path: &str) -> Result<Vec<u8>, String> {
            let value = self.responses.lock().unwrap().remove(0)?;
            Ok(value.to_string().into_bytes())
        }
    }

    #[test]
    fn ask_user_posts_the_dialectic_session_contract() {
        let transport = RecordingTransport {
            calls: std::sync::Mutex::new(Vec::new()),
            responses: std::sync::Mutex::new(vec![Ok(json!({ "answer": "You prefer bullet replies." }))]),
        };
        let client = RetainDbClient::new(transport, "https://api.retaindb.com/", "hermes-main");
        let out = client
            .ask_user("user@example.com", "how do I ship?", "low")
            .unwrap();
        assert_eq!(out["answer"], "You prefer bullet replies.");
        let (method, path, body) = {
            let calls = client.transport.calls.lock().unwrap();
            assert_eq!(calls.len(), 1);
            calls[0].clone()
        };
        assert_eq!(method, "POST");
        // Path encodes the user id; base_url trailing slash stripped.
        assert_eq!(path, "/v1/memory/profile/user%40example.com/ask");
        assert_eq!(
            body.unwrap(),
            json!({ "project": "hermes-main", "query": "how do I ship?", "reasoning_level": "low" })
        );
    }

    #[test]
    fn agent_model_routes_carry_the_project_partition() {
        let transport = RecordingTransport {
            calls: std::sync::Mutex::new(Vec::new()),
            responses: std::sync::Mutex::new(vec![
                Ok(json!({ "memory_count": 3, "persona": "terse" })),
                Ok(json!({ "ok": true })),
            ]),
        };
        let client = RetainDbClient::new(transport, "https://api.retaindb.com", "default");
        let model = client.get_agent_model("hermes agent#1").unwrap();
        assert_eq!(model["memory_count"], 3);
        client.seed_agent_identity("hermes agent#1", "Be terse.", "soul_md").unwrap();
        let calls = client.transport.calls.lock().unwrap();
        assert_eq!(
            calls[0].1,
            "/v1/memory/agent/hermes%20agent%231/model?project=default"
        );
        assert_eq!(calls[0].0, "GET");
        assert!(calls[0].2.is_none(), "GET carries no JSON body");
        assert_eq!(
            calls[1].1,
            "/v1/memory/agent/hermes%20agent%231/seed"
        );
        assert_eq!(
            calls[1].2.clone().unwrap(),
            json!({ "project": "default", "content": "Be terse.", "source": "soul_md" })
        );
    }

    #[test]
    fn reasoning_level_ladder_matches_query_length() {
        assert_eq!(reasoning_level(&"a".repeat(119)), "low");
        assert_eq!(reasoning_level(&"a".repeat(120)), "medium");
        assert_eq!(reasoning_level(&"a".repeat(399)), "medium");
        assert_eq!(reasoning_level(&"a".repeat(400)), "high");
    }

    #[test]
    fn agent_model_gating_and_self_model_lines() {
        // memory_count == 0 -> no model (the prefetch leaves the cache alone).
        assert!(agent_model_or_none(&json!({ "memory_count": 0 })).is_none());
        let model = json!({
            "memory_count": 2,
            "persona": "terse",
            "persistent_instructions": ["ship daily", "no fluff"],
            "working_style": "async",
        });
        assert_eq!(
            agent_self_model_lines(&model),
            vec!["Persona: terse", "Instructions:\n- ship daily\n- no fluff", "Working style: async"]
        );
        // A model with none of the three keys renders no lines.
        assert!(agent_self_model_lines(&json!({ "memory_count": 1 })).is_empty());
    }

    #[test]
    fn prefetch_consumes_once_and_clears() {
        let mut cache = DialecticPrefetch::default();
        // queue_prefetch ran: all three fetches succeeded.
        cache.store(
            Some("[RetainDB Context]\nProfile:\n- None".to_string()),
            Some("You prefer bullet replies.".to_string()),
            Some(json!({ "memory_count": 2, "persona": "terse" })),
        );
        let block = cache.prefetch();
        assert_eq!(
            block,
            "[RetainDB Context]\nProfile:\n- None\n\n[RetainDB User Synthesis]\nYou prefer bullet replies.\n\n[RetainDB Agent Self-Model]\nPersona: terse"
        );
        // Second turn: the cache drained — nothing left, empty block.
        assert_eq!(cache.prefetch(), "");
        // A failed fetch stores None and leaves the previous value untouched…
        cache.store(Some("kept context".to_string()), None, None);
        // …but a consume of an empty dialectic/agent-model block renders only
        // the parts that exist.
        assert_eq!(cache.prefetch(), "kept context");
    }

    #[test]
    fn overlay_dedupes_against_locals_and_caps_at_five() {
        let profile = json!({ "memories": [
            { "content": "Prefers Rust." },
            { "content": "  uses\n   windows " },
            { "content": "Drinks tea." },
        ]});
        let query = json!({ "results": [
            { "content": "PREFERS RUST." },          // deduped against profile (case/space-insensitive key)
            { "content": "Hates tabs." },
            { "content": "" },                        // empty entries dropped
            { "content": "One." }, { "content": "Two." }, { "content": "Three." },
        ]});
        // 3 profile + 5 query entries -> the cap keeps the FIRST 5 raw query
        // entries, so the 6th ("Three.") is never considered.
        let out = build_overlay(&profile, &query, &["hates tabs"]);
        assert!(out.starts_with("[RetainDB Context]\nProfile:\n"));
        assert!(out.contains("- Prefers Rust."));
        assert!(out.contains("- uses windows"));
        // The query entry deduped against the profile entry is gone…
        let relevant = out.split("Relevant memories:").nth(1).unwrap();
        assert!(!relevant.contains("PREFERS RUST"));
        // …and the local entry is excluded from the query half too.
        assert!(!relevant.contains("Hates tabs."));
        assert!(relevant.contains("One.") && relevant.contains("Two."));
        assert!(!relevant.contains("Three."));
        // Everything deduped to nothing -> empty overlay (no block emitted).
        assert_eq!(build_overlay(&json!({}), &json!({}), &[]), "");
    }

    #[test]
    fn compaction_caps_and_normalizes() {
        assert_eq!(compact("  a\n\tb   c "), "a b c");
        let long = "x ".repeat(300); // 600 chars
        assert_eq!(compact(&long).len(), 320);
        assert_eq!(norm("Drinks TEA!"), "drinks tea");
    }
}
