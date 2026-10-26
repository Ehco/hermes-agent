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
//! What stays unported (the original deferral reasons hold for them): the
//! plugin registration/lifecycle, the SQLite write-behind queue, the shared
//! file store, and the ten `retaindb_*` tool schemas — those need the
//! memory-provider abstraction and plugin host the Rust port doesn't have.
//! HTTP itself is behind [`RetainDbTransport`], so tests stub the wire (no
//! live network).

use serde_json::{json, Value};

/// `_DEFAULT_BASE_URL`.
pub const DEFAULT_BASE_URL: &str = "https://api.retaindb.com";

/// The injected seam: one JSON request; `method` is uppercased by the caller
/// contract, `path` already carries the query string, `json_body` is `None`
/// for GET/DELETE. `Err` = transport failure (the `requests` exception).
pub trait RetainDbTransport {
    fn request(&self, method: &str, path: &str, json_body: Option<&Value>)
        -> Result<Value, String>;
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
