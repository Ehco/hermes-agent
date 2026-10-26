//! tool_search — progressive tool disclosure ("tool search") bridge machinery.
//!
//! Port of `tools/tool_search.py` + `tools/tool_search_catalog.py` +
//! `tools/tool_search_validation.py` from the Python hermes-agent: deferred
//! tools are replaced in the model-visible array by three bridge tools —
//! `tool_search` / `tool_describe` / `tool_call` — backed by a stateless,
//! rebuilt-every-assembly BM25 catalog over the deferrable tool-defs.
//!
//! Contract parity with Python (kept faithful):
//! - `ToolSearchConfig::from_raw` clamps every field; unknown values fall back
//!   to safe defaults (a config typo must not break the agent).
//! - Catalog search is BM25 with the rarest-token gate + term-coverage floor;
//!   an exact (case-insensitive) name match ranks first.
//! - The embedded listing degrades per server, largest first: full ->
//!   names-only -> summary; byte-stable ordering keeps the prefix cacheable.
//! - Bridge-call argument errors carry the Python error text verbatim.
//!
//! Documented divergences (see PORTING.md):
//! - The remote **connectors** subsystem (Nous tool gateway:
//!   `connectors__*` search/describe, `CONNECTOR_BATCH_SENTINEL`, hosted
//!   failure reporting) does not exist in the Rust port; the local catalog is
//!   the whole corpus and batching beyond one local call is rejected exactly
//!   like Python's `local_batch_error`.
//! - Stemming is a light built-in suffix stripper, not Snowball; index and
//!   query paths share it so recall parity is preserved.
//! - `hidden_declared_sources()` (platform-declared MCP servers hidden by
//!   their liveness check) has no Rust counterpart and returns empty.
//! - `validate_deferred_call_args` keeps Python's required-field probe and
//!   fail-open posture but performs no full jsonschema validation (no
//!   jsonschema crate); coercion is likewise out of scope.
//! - Observability (`record_tool_search`) is not ported.

use regex::Regex;
use serde_json::{json, Map, Value};
use std::sync::OnceLock;

use crate::util::{tool_error, tool_error_extra};

// ---------------------------------------------------------------------------
// Reserved bridge names (tool_search_catalog.py)
// ---------------------------------------------------------------------------

pub const TOOL_SEARCH_NAME: &str = "tool_search";
pub const TOOL_DESCRIBE_NAME: &str = "tool_describe";
pub const TOOL_CALL_NAME: &str = "tool_call";

/// Reserved bridge names: a user/plugin/MCP tool may not take them.
pub const BRIDGE_TOOL_NAMES: &[&str] = &[TOOL_SEARCH_NAME, TOOL_DESCRIBE_NAME, TOOL_CALL_NAME];

pub fn is_bridge_tool(name: &str) -> bool {
    BRIDGE_TOOL_NAMES.contains(&name)
}

/// Chars-per-token rule of thumb; 4.0 slightly underestimates (fewer false
/// activations). Python `CHARS_PER_TOKEN`.
const CHARS_PER_TOKEN: f64 = 4.0;

/// Bound the work one bridge call requests (Python `_MAX_QUERIES_PER_CALL` /
/// `_MAX_DESCRIBE_NAMES_PER_CALL`).
const MAX_QUERIES_PER_CALL: usize = 7;
const MAX_DESCRIBE_NAMES_PER_CALL: usize = 10;

/// Event-triggered tools deferred BY DEFAULT (Python `_DEFAULT_DEFERRED_TOOLS`,
/// the curated `defer` list from config_defaults). An explicit list replaces it
/// wholesale; `[]` keeps every tool eager. Names absent from the Rust registry
/// (computer_use, browser_vault, the desktop GUI surface) simply never match.
pub const DEFAULT_DEFERRED_TOOLS: &[&str] = &[
    "computer_use",
    "session_search",
    "image_generate",
    "todo_list",
    "process_manage",
    "cronjob_manage",
    "browser_vault",
    "drive_preview",
    "gui_tour",
    "desktop_preview",
    "annotate_preview",
    "manage_catalog",
    "show_tip",
    "desktop_project",
    "close_terminal",
    "set_theme",
    "read_terminal",
    "read_window_below",
    "focus_pane",
];

// ---------------------------------------------------------------------------
// Configuration (tool_search.py)
// ---------------------------------------------------------------------------

/// Resolved, validated tool-search configuration for a single assembly.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolSearchConfig {
    /// "auto" | "on" | "off" — "auto" is an alias of "on" today.
    pub enabled: String,
    /// Listing budget as % of context; does NOT gate activation, only bounds
    /// how much the embedded manifest may consume before it degrades.
    pub threshold_pct: f64,
    pub search_default_limit: i64,
    pub max_search_limit: i64,
    /// "auto"/"on" = embed the manifest when it fits; "off" = bare bridge.
    pub listing: String,
    /// budget = min(this, threshold_pct% of context).
    pub listing_max_tokens: i64,
    /// None = curated default; an explicit list replaces it wholesale
    /// ([] = defer no core tools).
    pub defer_tools: Option<Vec<String>>,
}

impl Default for ToolSearchConfig {
    fn default() -> Self {
        Self::from_raw(&Value::Null)
    }
}

fn tri_state(value: &Value) -> String {
    // Normalize an `auto`/`on`/`off` setting (bool-ish aliases accepted).
    let text = match value {
        Value::String(s) => s.trim().to_lowercase(),
        Value::Bool(b) => (if *b { "on" } else { "off" }).to_string(),
        _ => String::new(),
    };
    match text.as_str() {
        "true" | "1" | "yes" => "on".into(),
        "false" | "0" | "no" => "off".into(),
        "auto" | "on" | "off" => text,
        _ => "auto".into(),
    }
}

fn clamped_int(value: &Value, fallback: i64, lo: i64, hi: i64) -> i64 {
    // `int(value)` (or `fallback` when unparseable) clamped to `[lo, hi]`.
    let parsed = match value {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().map(|f| f as i64)),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        Value::Bool(_) => None,
        _ => None,
    };
    parsed.unwrap_or(fallback).clamp(lo, hi)
}

fn safe_float(value: &Value, fallback: f64) -> f64 {
    match value {
        Value::Number(n) => n.as_f64().unwrap_or(fallback),
        Value::String(s) => s.trim().parse::<f64>().unwrap_or(fallback),
        _ => fallback,
    }
}

impl ToolSearchConfig {
    /// Build from a raw dict / legacy bool / null; every field is clamped and
    /// unknown values fall back to safe defaults — a config typo must not
    /// break the agent (Python `ToolSearchConfig.from_raw`).
    pub fn from_raw(raw: &Value) -> Self {
        // Legacy bool / None / any non-dict: the enabled flag is the only
        // field the legacy shape carries ("off" if false, else "auto").
        let legacy_enabled: Option<&str> = match raw {
            Value::Bool(false) => Some("off"),
            Value::Bool(true) => Some("auto"),
            other if !other.is_object() => Some("auto"),
            _ => None,
        };
        let obj = if raw.is_object() { raw } else { &Value::Null };
        let enabled = match legacy_enabled {
            Some(enabled) => enabled.to_string(),
            None => tri_state(&obj["enabled"]),
        };
        let max_search_limit = clamped_int(&obj["max_search_limit"], 25, 1, 50);
        // A scalar `defer` is warned about in Python (loud, then the curated
        // default); only list-shaped values become an explicit override.
        let defer_raw = obj.get("defer");
        let defer_tools = match defer_raw {
            Some(Value::Array(items)) => Some(
                items
                    .iter()
                    .map(|v| match v {
                        Value::String(s) => s.trim().to_string(),
                        other => other.to_string().trim().trim_matches('"').to_string(),
                    })
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>(),
            ),
            _ => None,
        };
        Self {
            enabled,
            threshold_pct: safe_float(&obj["threshold_pct"], 5.0).clamp(0.0, 100.0),
            search_default_limit: clamped_int(
                &obj["search_default_limit"],
                5,
                1,
                max_search_limit,
            ),
            max_search_limit,
            listing: tri_state(&obj["listing"]),
            listing_max_tokens: clamped_int(&obj["listing_max_tokens"], 4000, 200, 60000),
            defer_tools,
        }
    }

    pub fn effective_defer_tools(&self) -> Vec<String> {
        self.defer_tools
            .clone()
            .unwrap_or_else(|| DEFAULT_DEFERRED_TOOLS.iter().map(|s| (*s).into()).collect())
    }
}

// ---------------------------------------------------------------------------
// Catalog entry + tokenization (tool_search_catalog.py)
// ---------------------------------------------------------------------------

/// One deferrable tool, in a form the bridge tools can search and serve.
#[derive(Debug, Clone)]
pub struct CatalogEntry {
    pub name: String,
    pub description: String,
    /// The full {"type":"function", "function": {...}} tool-def.
    pub schema: Value,
    /// "mcp" | "plugin" | "other"
    pub source: String,
    /// Toolset name, e.g. "mcp-github" or "" for the built-ins.
    pub source_name: String,
    /// Pre-tokenized for BM25.
    tokens: Vec<String>,
}

fn token_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| Regex::new(r"[A-Za-z0-9]+").unwrap())
}

/// Light English suffix stripper replacing Python's Snowball stemmer (see
/// module docs). Only self-consistency matters: index and query paths share
/// it, so "issues" matches `create_issue`.
fn stem(token: &str) -> String {
    let mut t = token.to_string();
    if t.ends_with("sses") && t.len() > 4 {
        t.truncate(t.len() - 2); // "processes" -> "process"
    } else if t.ends_with("ies") && t.len() > 3 {
        let stem = t[..t.len() - 3].to_string();
        t = format!("{stem}y"); // "parties" -> "party"
    } else if t.ends_with("ss") || t.ends_with("us") {
        // keep
    } else if t.ends_with('s') && t.len() > 2 {
        t.truncate(t.len() - 1); // "issues" -> "issue"
    }
    if t.ends_with("ing") && t.len() > 5 {
        t.truncate(t.len() - 3); // "creating" -> "creat"
    } else if t.ends_with("ed") && t.len() > 4 {
        t.truncate(t.len() - 2); // "created" -> "creat"
    }
    if t.ends_with('e') && t.len() > 3 {
        t.truncate(t.len() - 1); // "issue" -> "issu", "create" -> "creat"
    }
    t
}

/// Lowercase alphanumeric tokens, stemmed (English); shared by the index and
/// query paths so "issues" matches `create_issue`.
pub fn tokenize(text: &str) -> Vec<String> {
    if text.is_empty() {
        return Vec::new();
    }
    token_re()
        .find_iter(text)
        .map(|m| stem(&m.as_str().to_lowercase()))
        .collect()
}

/// The `function` block of a tool-def (empty object when absent/null).
pub fn function_block(td: &Value) -> &Value {
    static EMPTY: OnceLock<Value> = OnceLock::new();
    td.get("function").unwrap_or(EMPTY.get_or_init(|| json!({})))
}

/// The model-facing name of a tool-def (`""` for a nameless def).
fn fn_name(td: &Value) -> String {
    function_block(td)
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string()
}

/// Search-text blob: split name words + source label + description + top-level
/// parameter names. The `mcp__` prefix is dropped — it is in every MCP
/// document, so its IDF is ~0. The source label lets a service-name query
/// ("linear") reach a tool whose own name omits the vendor.
fn entry_search_text(td: &Value, source_label: &str) -> String {
    let fn_block = function_block(td);
    let raw_name = fn_block.get("name").and_then(Value::as_str).unwrap_or("");
    let name = raw_name.strip_prefix("mcp__").unwrap_or(raw_name);
    let name_words = name.replace(['_', '.', ':', '-'], " ");
    let extra = if !source_label.is_empty() && !name_words.split(' ').any(|w| w == source_label) {
        source_label
    } else {
        ""
    };
    let params = fn_block
        .get("parameters")
        .and_then(|p| p.get("properties"))
        .and_then(Value::as_object)
        .map(|props| props.keys().cloned().collect::<Vec<_>>().join(" "))
        .unwrap_or_default();
    let desc = fn_block.get("description").and_then(Value::as_str).unwrap_or("");
    format!("{name_words} {extra} {desc} {params}")
}

/// Return (source_kind, source_name) for a tool name. The Rust registry has
/// no toolset column: MCP-prefixed names classify as "mcp", everything else
/// (built-ins and unknown plugins alike) as "other" with an empty source_name.
fn classify_source(name: &str) -> (String, String) {
    if let Some(rest) = name
        .strip_prefix("mcp__")
        .map(|r| r.split("__").next().unwrap_or(r))
        .or_else(|| name.strip_prefix("mcp-"))
    {
        return ("mcp".into(), format!("mcp-{rest}"));
    }
    ("other".into(), String::new())
}

/// Human-facing group heading for a toolset, e.g. `mcp-github` -> `github`.
fn listing_group_label(source_name: &str) -> String {
    let label = if source_name.is_empty() { "other" } else { source_name };
    label.strip_prefix("mcp-").unwrap_or(label).to_string()
}

/// Build the deferred-tool catalog from the deferrable subset of tool-defs.
pub fn build_catalog(tool_defs: &[Value]) -> Vec<CatalogEntry> {
    let mut catalog = Vec::new();
    for td in tool_defs {
        let fn_block = function_block(td);
        let name = fn_block.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let (source, source_name) = classify_source(name);
        // Index the human-facing label ("linear", not "mcp-linear").
        let source_label = if source_name.is_empty() {
            String::new()
        } else {
            listing_group_label(&source_name)
        };
        catalog.push(CatalogEntry {
            name: name.to_string(),
            description: fn_block
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
            schema: td.clone(),
            source,
            source_name,
            tokens: tokenize(&entry_search_text(td, &source_label)),
        });
    }
    catalog
}

// ---------------------------------------------------------------------------
// BM25 retrieval (tool_search_catalog.py)
// ---------------------------------------------------------------------------

/// (doc_lengths, avg_dl, doc_freq, n_docs) shared by every query.
type CorpusStats = (Vec<usize>, f64, std::collections::HashMap<String, usize>, usize);

fn corpus_stats(catalog: &[CatalogEntry]) -> CorpusStats {
    let doc_lengths: Vec<usize> = catalog.iter().map(|e| e.tokens.len()).collect();
    let avg_dl =
        doc_lengths.iter().sum::<usize>() as f64 / doc_lengths.len().max(1) as f64;
    let mut doc_freq: std::collections::HashMap<String, usize> = Default::default();
    for entry in catalog {
        for tok in entry.tokens.iter().collect::<std::collections::BTreeSet<_>>() {
            *doc_freq.entry(tok.clone()).or_insert(0) += 1;
        }
    }
    (doc_lengths, avg_dl, doc_freq, catalog.len())
}

/// Standard BM25 for one query against one document.
#[allow(clippy::too_many_arguments)]
fn bm25_score(
    query_tokens: &[String],
    doc_tokens: &[String],
    avg_dl: f64,
    doc_freq: &std::collections::HashMap<String, usize>,
    n_docs: usize,
    k1: f64,
    b: f64,
) -> f64 {
    let mut score = 0.0;
    let dl = doc_tokens.len() as f64;
    let mut doc_tf: std::collections::HashMap<&str, usize> = Default::default();
    for t in doc_tokens {
        *doc_tf.entry(t.as_str()).or_insert(0) += 1;
    }
    for q in query_tokens {
        let df = doc_freq.get(q).copied().unwrap_or(0);
        let tf = doc_tf.get(q.as_str()).copied().unwrap_or(0);
        if df > 0 && tf > 0 {
            let idf = (1.0 + (n_docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln();
            score += idf * tf as f64 * (k1 + 1.0)
                / (tf as f64 + k1 * (1.0 - b + b * dl / avg_dl.max(1.0)));
        }
    }
    score
}

/// The query token with the highest IDF: the word that names the intent.
/// `send`, `read`, `create` sit in dozens of tool documents and separate
/// nothing; `gmail`, `github`, `incident` sit in a few and separate everything.
fn gate_token(
    query_tokens: &[String],
    doc_freq: &std::collections::HashMap<String, usize>,
    n_docs: usize,
) -> String {
    let idf = |token: &String| -> f64 {
        let df = doc_freq.get(token).copied().unwrap_or(0);
        (1.0 + (n_docs as f64 - df as f64 + 0.5) / (df as f64 + 0.5)).ln()
    };
    query_tokens
        .iter()
        .max_by(|a, b| idf(a).partial_cmp(&idf(b)).unwrap_or(std::cmp::Ordering::Equal))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Relevance floor (Python constants).
const MIN_QUERY_TERM_COVERAGE: f64 = 0.5;
const MIN_ANSWERABLE_TERMS_FOR_COVERAGE: usize = 4;

/// How many of a query's ANSWERABLE unique terms a document must match to be
/// offered: one below the engagement floor, else at least half, rounded up.
fn required_term_coverage(answerable_term_count: usize) -> usize {
    if answerable_term_count < MIN_ANSWERABLE_TERMS_FOR_COVERAGE {
        return 1;
    }
    (answerable_term_count as f64 * MIN_QUERY_TERM_COVERAGE).ceil() as usize
}

/// Top-`limit` catalog entries for `query` by BM25 (exact name match ranks
/// first). Admission is by the query's rarest token, not by `score > 0`; long
/// queries additionally need their answerable-term coverage floor.
pub fn search_catalog(
    catalog: &[CatalogEntry],
    query: &str,
    limit: usize,
    precomputed: Option<&CorpusStats>,
) -> Vec<CatalogEntry> {
    let query_tokens: Vec<String> = if !catalog.is_empty() && limit > 0 {
        tokenize(query)
    } else {
        Vec::new()
    };
    if query_tokens.is_empty() {
        return Vec::new();
    }
    let owned_stats;
    let stats = match precomputed {
        Some(s) => s,
        None => {
            owned_stats = corpus_stats(catalog);
            &owned_stats
        }
    };
    let doc_freq = &stats.2;
    let n_docs = stats.3;
    let gate = gate_token(&query_tokens, doc_freq, n_docs);
    let answerable: std::collections::BTreeSet<String> = query_tokens
        .iter()
        .filter(|t| doc_freq.get(*t).copied().unwrap_or(0) > 0)
        .cloned()
        .collect();
    let required_terms = required_term_coverage(answerable.len());
    let exact_name = query.trim().to_lowercase();

    let admitted = |entry: &CatalogEntry| -> bool {
        // Carries the intent word AND enough of the answerable terms (exact
        // name is exempt).
        if entry.name.to_lowercase() == exact_name {
            return true;
        }
        let tokens: std::collections::BTreeSet<&String> = entry.tokens.iter().collect();
        tokens.contains(&gate)
            && answerable.iter().filter(|t| tokens.contains(*t)).count() >= required_terms
    };

    let mut scored: Vec<(f64, &CatalogEntry)> = catalog
        .iter()
        .filter(|e| admitted(e))
        .map(|entry| {
            let score = if entry.name.to_lowercase() == exact_name {
                f64::INFINITY
            } else {
                bm25_score(&query_tokens, &entry.tokens, stats.1, doc_freq, n_docs, 1.5, 0.75)
            };
            (score, entry)
        })
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored.into_iter().take(limit).map(|(_, e)| e.clone()).collect()
}

// ---------------------------------------------------------------------------
// Catalog listing (tool_search_catalog.py)
// ---------------------------------------------------------------------------

/// First sentence of a tool description, clipped to `max_chars` (60) on a word
/// boundary. `e.g.`/`i.e.`/`etc.` do not end a sentence.
fn short_desc(description: &str, max_chars: usize) -> String {
    let text: String = description.split_whitespace().collect::<Vec<_>>().join(" ");
    let end = first_sentence_end(&text);
    let text = match end {
        Some(i) => text[..i + 1].to_string(),
        None => text,
    };
    let count = text.chars().count();
    if count <= max_chars {
        return text;
    }
    let clipped: String = text.chars().take(max_chars).collect();
    let clipped = match clipped.rfind(' ') {
        Some(i) => clipped[..i].to_string(),
        None => clipped,
    };
    format!("{}…", clipped.trim_end_matches([';', ':', ',', ' ']))
}

/// Byte offset of the first sentence-ending `.`,`!`,`?` followed by whitespace
/// or EOS, not preceded by the literal words `e.g`, `i.e`, `etc`. The Rust
/// regex crate has no lookaround, so this is a hand scan.
fn first_sentence_end(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    for i in 0..bytes.len() {
        if !matches!(bytes[i], b'.' | b'!' | b'?') {
            continue;
        }
        let next_ok = i + 1 >= bytes.len() || (bytes[i + 1] as char).is_whitespace();
        if !next_ok {
            continue;
        }
        // Walk back over the word (alnum and inner dots) that ends here.
        let mut start = i;
        while start > 0 {
            let c = text[..start].chars().last();
            match c {
                Some(c) if c.is_alphanumeric() || c == '.' => start -= c.len_utf8(),
                _ => break,
            }
        }
        let bare: String = text[start..i]
            .trim_end_matches(['.', '!', '?'])
            .to_lowercase();
        if !matches!(bare.as_str(), "e.g" | "i.e" | "etc") {
            return Some(i);
        }
    }
    None
}

fn render_group(label: &str, tools: &[(String, String)], mode: &str) -> String {
    // Render one server's block. mode: "full" | "names" | "summary".
    let tools: Vec<&(String, String)> = tools.iter().collect();
    match mode {
        "summary" => format!(
            "{label} ({} tools — names not listed; discover via `{TOOL_SEARCH_NAME}`)",
            tools.len()
        ),
        _ => {
            let mut lines = vec![format!("{label} tools ({}):", tools.len())];
            if mode == "full" {
                for (name, desc) in tools {
                    if desc.is_empty() {
                        lines.push(format!("- {name}"));
                    } else {
                        lines.push(format!("- {name}: {desc}"));
                    }
                }
            } else {
                lines.push(tools.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>().join(", "));
            }
            lines.join("\n")
        }
    }
}

/// Render the deferred-catalog manifest: `- name: short desc` lines grouped per
/// source. Returns `(text, form)`; form is "full", "names", "mixed" (oversized
/// servers collapsed to a name + count line), "groups" (every server
/// summarized) or "none" (over budget even summarized -> text is None).
/// Ordering is deterministic (sorted groups and tools) so the block is
/// byte-stable — the request prefix stays cacheable. Degradation is PER
/// SERVER, largest first: one huge server must not cost a small one its
/// listing.
pub fn build_catalog_listing_with_form(
    deferrable: &[Value],
    max_tokens: usize,
) -> (Option<String>, String) {
    let mut groups: std::collections::BTreeMap<String, Vec<(String, String)>> = Default::default();
    for td in deferrable {
        let fn_block = function_block(td);
        let name = fn_block.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() {
            continue;
        }
        let label = {
            let (_kind, source_name) = classify_source(name);
            listing_group_label(&source_name)
        };
        let desc = short_desc(fn_block.get("description").and_then(Value::as_str).unwrap_or(""), 60);
        groups.entry(label).or_default().push((name.to_string(), desc));
    }
    if groups.is_empty() {
        return (None, "none".into());
    }

    let header = format!(
        "Deferred tool catalog (call schemas via `{TOOL_DESCRIBE_NAME}`, invoke via `{TOOL_CALL_NAME}`):"
    );

    let fits = |modes: &std::collections::BTreeMap<String, String>| -> Option<String> {
        let blocks: Vec<String> = groups
            .keys()
            .map(|label| render_group(label, &groups[label], &modes[label]))
            .collect();
        let text = std::iter::once(header.clone())
            .chain(blocks)
            .collect::<Vec<_>>()
            .join("\n");
        let tokens = (text.chars().count() as f64 / CHARS_PER_TOKEN).ceil();
        (tokens <= max_tokens as f64).then_some(text)
    };

    // 1. everything full; 2. everything names-only
    for mode in ["full", "names"] {
        let modes = groups.keys().map(|k| (k.clone(), mode.into())).collect();
        if let Some(text) = fits(&modes) {
            return (Some(text), mode.into());
        }
    }
    // 3. Collapse the LARGEST rendered groups first (deterministic: size then
    // label).
    let mut modes = groups.keys().map(|k| (k.clone(), "names".into())).collect::<std::collections::BTreeMap<_, _>>();
    let mut order: Vec<&String> = groups.keys().collect();
    order.sort_by(|a, b| {
        let sa = render_group(a, &groups[a.as_str()], "names").len();
        let sb = render_group(b, &groups[b.as_str()], "names").len();
        sb.cmp(&sa).then_with(|| a.cmp(b))
    });
    for lbl in order {
        modes.insert(lbl.clone(), "summary".into());
        if let Some(text) = fits(&modes) {
            let all_summary = modes.values().all(|m| m == "summary");
            return (Some(text), if all_summary { "groups".into() } else { "mixed".into() });
        }
    }
    (None, "none".into())
}

// ---------------------------------------------------------------------------
// Classification / assembly (tool_search.py)
// ---------------------------------------------------------------------------

/// True if a tool is *eligible* for deferral: named in `defer_tools` (curated
/// set or user override), OR an MCP tool. Bridge names never defer; Rust
/// registry built-ins (the core set) never defer unless named.
pub fn is_deferrable_tool_name(name: &str, defer_tools: Option<&[String]>) -> bool {
    if is_bridge_tool(name) {
        return false;
    }
    if let Some(defer) = defer_tools {
        if defer.iter().any(|d| d == name) {
            return true;
        }
    }
    if crate::registry::spec_exists(name) {
        return false;
    }
    // Unregistered names defer only when they carry an MCP prefix (the rest —
    // malformed or plain-unknown — never defer, matching Python's registry
    // lookup returning None).
    name.starts_with("mcp__") || name.starts_with("mcp-")
}

/// Every tool name in the session's `tool_defs` — what this session can
/// actually reach.
pub fn session_tool_names(tool_defs: &[Value]) -> Vec<String> {
    tool_defs.iter().map(|td| fn_name(td)).filter(|n| !n.is_empty()).collect()
}

/// Split a tool-defs list into (visible, deferrable); bridge tools are dropped.
pub fn classify_tools<'a>(
    tool_defs: &[Value],
    defer_tools: Option<&[String]>,
) -> (Vec<Value>, Vec<Value>) {
    let mut visible = Vec::new();
    let mut deferrable = Vec::new();
    for td in tool_defs {
        let name = fn_name(td);
        if is_bridge_tool(&name) {
            continue;
        }
        if is_deferrable_tool_name(&name, defer_tools) {
            deferrable.push(td.clone());
        } else {
            visible.push(td.clone());
        }
    }
    (visible, deferrable)
}

/// Token cost via the chars/4 rule (order-of-magnitude precision suffices).
pub fn estimate_tokens_from_schemas(tool_defs: &[Value]) -> i64 {
    let chars: usize = tool_defs
        .iter()
        .map(|td| serde_json::to_string(td).map(|s| s.len()).unwrap_or(0))
        .sum();
    (chars as f64 / CHARS_PER_TOKEN).ceil() as i64
}

/// "off" never activates; "on"/"auto" activate whenever any deferrable tool
/// exists (`context_length` is kept for caller compatibility).
pub fn should_activate(config: &ToolSearchConfig, deferrable_tokens: i64, connections_granted: bool) -> bool {
    if config.enabled == "off" {
        return false;
    }
    if deferrable_tokens > 0 {
        return true;
    }
    connections_granted
}

/// `min(listing_max_tokens, threshold_pct% of context)`; unknown context uses a
/// 10K percentage leg (5% of a typical 200K window).
pub fn listing_token_budget(config: &ToolSearchConfig, context_length: Option<i64>) -> i64 {
    let pct_leg = match context_length {
        Some(len) if len > 0 => (len as f64 * (config.threshold_pct / 100.0)) as i64,
        _ => 10_000,
    };
    config.listing_max_tokens.min(pct_leg).max(0)
}

fn bridge_schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    // One OpenAI-style function schema (key order is part of the frozen bytes).
    json!({
        "type": "function",
        "function": {
            "name": name,
            "description": description,
            "parameters": {
                "type": "object",
                "properties": properties,
                "required": required,
            }
        }
    })
}

const CONNECTIONS_HINT: &str = " `connectors__` names are tools of the user's app accounts (Gmail, Linear, ...); `manage_connections` checks or authorizes an account.";

fn search_description(
    deferred_count: usize,
    listing: Option<&str>,
    listing_form: &str,
    connections_granted: bool,
) -> String {
    // tool_search bridge description with the listing embedded (framing per
    // `listing_form`).
    let mut desc = format!(
        "{}Send one keyword query per capability you need; returns matching tool names and descriptions. Then `{TOOL_DESCRIBE_NAME}` loads schemas and `{TOOL_CALL_NAME}` invokes. Tools already in your tool list need no search.",
        if deferred_count == 0 {
            "Search remote connector tools (email, calendars, issue trackers, and more). "
        } else {
            "Search additional tools that are loaded on demand. "
        }
    );
    if connections_granted {
        desc.push_str(CONNECTIONS_HINT);
    }
    let Some(listing) = listing else {
        return desc;
    };
    if listing_form == "groups" {
        desc.push_str(&format!(
            "\n\nThe servers below are connected and their tools ARE available through this bridge. For any request in these domains, search here FIRST — do not claim the capability is unavailable and do not substitute a generic tool (terminal/browser) without searching.\n\n{listing}"
        ));
        return desc;
    }
    desc += &format!(
        "\n\nEvery tool listed below exists: never call it unavailable, and go straight to `{TOOL_DESCRIBE_NAME}` when you see the exact name."
    );
    if listing_form == "mixed" {
        desc += &format!(
            " For servers marked 'names not listed', the tools exist too — find them with `{TOOL_SEARCH_NAME}` before concluding anything is missing."
        );
    }
    format!("{desc}\n\n{listing}")
}

/// Bridge tool schemas injected in place of deferred tools; kept short — every
/// byte is paid every turn.
pub fn bridge_tool_schemas(
    deferred_count: usize,
    listing: Option<&str>,
    listing_form: &str,
    connections_granted: bool,
) -> Vec<Value> {
    vec![
        bridge_schema(
            TOOL_SEARCH_NAME,
            &search_description(deferred_count, listing, listing_form, connections_granted),
            json!({
                "queries": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "One query per capability: service + action + object keywords (e.g. ['gmail send email', 'github create issue']), never a question; every word must match tool text. Searched in parallel.",
                },
                "limit": {
                    "type": "integer",
                    "description": "Max matches per query (default 5).",
                },
            }),
            &["queries"],
        ),
        bridge_schema(
            TOOL_DESCRIBE_NAME,
            &format!(
                "Load the full JSON schemas for tools returned by `{TOOL_SEARCH_NAME}`. Required before `{TOOL_CALL_NAME}` if a tool's parameters are unknown. Batch every schema you need into one call."
            ),
            json!({
                "names": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "Exact tool names (as returned by tool_search). A single string is accepted and treated as one name.",
                },
            }),
            &["names"],
        ),
        bridge_schema(
            TOOL_CALL_NAME,
            &format!(
                "Invoke deferred tools: `calls` is an array of {{name, arguments}}, one entry per invocation (batch independent calls; results come back in order). Arguments match the schema from `{TOOL_DESCRIBE_NAME}`; approvals and hooks apply as usual."
            ),
            json!({
                "calls": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Exact tool name to invoke."},
                            "arguments": {"type": "object", "description": "Arguments matching the tool schema."},
                        },
                        "required": ["name", "arguments"],
                    },
                    "description": "One or more independent deferred tool invocations.",
                },
            }),
            &["calls"],
        ),
    ]
}

/// Outcome of one assembly (tests and observability).
#[derive(Debug, Clone, PartialEq)]
pub struct AssemblyResult {
    pub tool_defs: Vec<Value>,
    pub activated: bool,
    pub deferred_count: usize,
    pub deferred_tokens: i64,
    pub threshold_tokens: i64,
    /// 0 = passthrough; 1 = bridge + per-tool listing; 2 = bare bridge.
    pub tier: i32,
    /// "full" | "names" | "mixed" | "groups" | "none"
    pub listing_form: String,
}

/// Tool-defs the model should see: passthrough when inactive, else deferrable
/// tools replaced by the three bridge tools. Idempotent — existing bridge tools
/// are stripped first.
pub fn assemble_tool_defs(
    tool_defs: &[Value],
    context_length: Option<i64>,
    config: Option<ToolSearchConfig>,
) -> AssemblyResult {
    let config = config.unwrap_or_default();
    let incoming: Vec<Value> = tool_defs
        .iter()
        .filter(|td| !is_bridge_tool(&fn_name(td)))
        .cloned()
        .collect();
    let defer = config.effective_defer_tools();
    let (visible, deferrable) = classify_tools(&incoming, Some(&defer));
    if deferrable.is_empty() {
        if should_activate(&config, 0, false) {
            return AssemblyResult {
                tool_defs: incoming
                    .into_iter()
                    .chain(bridge_tool_schemas(0, None, "", false))
                    .collect(),
                activated: true,
                deferred_count: 0,
                deferred_tokens: 0,
                threshold_tokens: 0,
                tier: 2,
                listing_form: "none".into(),
            };
        }
        return AssemblyResult {
            tool_defs: incoming,
            activated: false,
            deferred_count: 0,
            deferred_tokens: 0,
            threshold_tokens: 0,
            tier: 0,
            listing_form: "none".into(),
        };
    }
    let deferrable_tokens = estimate_tokens_from_schemas(&deferrable);
    if !should_activate(&config, deferrable_tokens, false) {
        return AssemblyResult {
            tool_defs: incoming,
            activated: false,
            deferred_count: deferrable.len(),
            deferred_tokens: deferrable_tokens,
            threshold_tokens: (context_length.unwrap_or(0) as f64 * (config.threshold_pct / 100.0))
                as i64,
            tier: 0,
            listing_form: "none".into(),
        };
    }
    let mut listing = None;
    let mut listing_form = "none".to_string();
    let listing_budget = listing_token_budget(&config, context_length);
    if config.listing != "off" {
        let (text, form) = build_catalog_listing_with_form(&deferrable, listing_budget.max(0) as usize);
        listing = text;
        listing_form = form;
    }
    let tier = if ["full", "names", "mixed"].contains(&listing_form.as_str()) { 1 } else { 2 };
    AssemblyResult {
        tool_defs: visible
            .into_iter()
            .chain(bridge_tool_schemas(
                deferrable.len(),
                listing.as_deref(),
                &listing_form,
                false,
            ))
            .collect(),
        activated: true,
        deferred_count: deferrable.len(),
        deferred_tokens: deferrable_tokens,
        threshold_tokens: listing_budget,
        tier,
        listing_form,
    }
}

// ---------------------------------------------------------------------------
// Validation helpers (tool_search_validation.py)
// ---------------------------------------------------------------------------

/// Normalize `tool_call` arguments into a `calls[]` list of (name, arguments).
/// Accepts the advertised batch shape `{"calls": [{"name", "arguments"}, ...]}`
/// and, tolerantly, the legacy single shape `{"name": ..., "arguments": ...}`.
/// Each entry's `arguments` is coerced to a dict (JSON strings parsed, null/`""`
/// -> `{}`).
pub fn normalize_tool_call_entries(args: &Value) -> Result<Vec<(String, Value)>, String> {
    let mut raw_calls = args.get("calls").cloned();
    if raw_calls.is_none() {
        // Legacy single shape.
        let name = args.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() {
            return Err("tool_call requires 'calls' (an array of {name, arguments})".into());
        }
        raw_calls = Some(json!([{
            "name": args.get("name"),
            "arguments": args.get("arguments"),
        }]));
    }
    let raw_calls = match raw_calls.unwrap() {
        Value::String(s) => serde_json::from_str::<Value>(&s)
            .map_err(|e| format!("tool_call 'calls' is not valid JSON: {e}"))?,
        other => other,
    };
    let raw_calls: Vec<Value> = match &raw_calls {
        Value::Array(items) => items.clone(),
        Value::Object(_) => vec![raw_calls.clone()],
        _ => return Err("tool_call 'calls' must be a non-empty array of {name, arguments}".into()),
    };
    if raw_calls.is_empty() {
        return Err("tool_call 'calls' must be a non-empty array of {name, arguments}".into());
    }
    let mut entries = Vec::new();
    for (position, raw) in raw_calls.iter().enumerate() {
        let obj = match raw.as_object() {
            Some(obj) => obj,
            None => {
                return Err(format!(
                    "tool_call calls[{position}] must be an object with 'name' and 'arguments'"
                ))
            }
        };
        let name = obj.get("name").and_then(Value::as_str).unwrap_or("").trim();
        if name.is_empty() {
            return Err(format!("tool_call calls[{position}] requires a 'name'"));
        }
        if is_bridge_tool(name) {
            return Err(format!("tool_call cannot invoke '{name}' (it is itself a bridge tool)"));
        }
        let raw_args = obj.get("arguments").cloned().unwrap_or(Value::Null);
        let raw_args = match raw_args {
            Value::Null => json!({}),
            Value::String(s) if s.trim().is_empty() => json!({}),
            Value::String(s) => serde_json::from_str::<Value>(&s)
                .map_err(|e| format!("tool_call calls[{position}].arguments is not valid JSON: {e}"))?,
            other => other,
        };
        if !raw_args.is_object() {
            return Err(format!(
                "tool_call calls[{position}].arguments must be an object"
            ));
        }
        entries.push((name.to_string(), raw_args));
    }
    Ok(entries)
}

/// Rejection for a multi-entry batch that names a local tool. Restates the
/// valid shape with the caller's OWN first entry: small models re-send an
/// identical batch when told only the constraint, and the echoed payload is
/// what gets them unstuck.
fn local_batch_error(entries: &[(String, Value)]) -> String {
    const ECHO_ARGS_MAX_CHARS: usize = 1500;
    let (name, args) = &entries[0];
    let mut args_json = serde_json::to_string(args).unwrap_or_else(|_| "{}".into());
    if args_json.len() > ECHO_ARGS_MAX_CHARS {
        args_json = "{...}".into(); // keep the correction readable
    }
    let remaining = if entries.len() > 1 {
        format!(
            " then issue the remaining {} call(s) as separate tool_call invocations",
            entries.len() - 1
        )
    } else {
        String::new()
    };
    tool_error(format!(
        "tool_call takes exactly one entry for local tools; you sent {}. Retry with only: \
        {{\"calls\":[{{\"name\":{},\"arguments\":{}}}]}}{remaining}. Only connectors__ names may \
        be batched together.",
        entries.len(),
        serde_json::to_string(name).unwrap_or_else(|_| "\"\"".into()),
        args_json,
    ))
}

/// Why a registered tool absent from THIS session cannot be reached, whatever
/// its door.
fn unavailable_in_session_reason(name: &str) -> String {
    format!(
        "'{name}' is not available in this execution context. \
        Don't retry it — state the assumption and proceed with the work."
    )
}

/// The unavailability message when a core tool is missing from the session's
/// own tool list, whether or not `defer:` moved it behind the bridge; None
/// without session context, when the session carries it, or for MCP/plugin/
/// unregistered names.
fn session_unavailable_error(name: &str, session_tool_names: Option<&[String]>) -> Option<String> {
    let names = session_tool_names?;
    if names.iter().any(|n| n == name) {
        return None;
    }
    if !crate::registry::spec_exists(name) {
        return None;
    }
    Some(unavailable_in_session_reason(name))
}

/// Rejection for a `tool_call` naming something that is not a deferred tool.
/// Two different mistakes reach here and need opposite corrections: a
/// directly-listed tool (call it without the bridge) vs. an unknown name —
/// typically a deferred MCP tool cited by its bare suffix instead of the full
/// `mcp__<server>__{tool}` name.
fn not_deferrable_error(name: &str, session_tool_names: Option<&[String]>) -> String {
    if crate::registry::spec_exists(name) {
        if let Some(names) = session_tool_names {
            if !names.iter().any(|n| n == name) {
                return unavailable_in_session_reason(name);
            }
        }
        return format!(
            "'{name}' is a directly-listed tool, not a deferred one. Call it directly instead of via tool_call."
        );
    }
    let suffix = format!("__{name}");
    let mut candidates: Vec<String> = crate::registry::specs()
        .iter()
        .map(|s| s.name.clone())
        .filter(|n| n.ends_with(&suffix))
        .collect();
    candidates.sort();
    let hint = if !candidates.is_empty() {
        let list = candidates
            .iter()
            .map(|c| format!("'{c}'"))
            .collect::<Vec<_>>()
            .join(", ");
        format!(" Did you mean {list}?")
    } else {
        " Use tool_search to find the exact name.".into()
    };
    format!(
        "'{name}' is not a known tool name. Deferred tools must be invoked through tool_call by \
        the exact name tool_search returns (e.g. mcp__<server>__{{tool}}).{hint}"
    )
}

/// Validate `tool_call` arguments against the deferred tool's schema — the
/// required-field probe only (see module docs for the divergence); missing/
/// malformed schemas fail OPEN. Returns a JSON error string when invalid,
/// `None` when the call should dispatch.
pub fn validate_deferred_call_args(name: &str, args: &Value) -> Option<String> {
    let spec = crate::registry::specs()
        .into_iter()
        .find(|s| s.name == name)?;
    let params = spec.schema;
    if !params.is_object() {
        return None;
    }
    let required = params.get("required").and_then(Value::as_array)?;
    let missing: Vec<&str> = required
        .iter()
        .filter_map(Value::as_str)
        .filter(|r| args.get(*r).is_none() || args.get(*r) == Some(&Value::Null))
        .collect();
    if missing.is_empty() {
        return None;
    }
    let missing = missing.join(", ");
    Some(tool_error_extra(
        format!(
            "tool_call to '{name}' is missing required argument(s): {missing}. The tool was NOT invoked."
        ),
        Some(json!({
            "path": "arguments",
            "constraint": "required",
            "parameters": params,
            "hint": "Retry tool_call with 'arguments' matching the parameters schema above.",
        })),
    ))
}

// ---------------------------------------------------------------------------
// Bridge dispatch (tool_search.py)
// ---------------------------------------------------------------------------

/// Cap a record description, marking the cut so it reads as deliberate
/// (Python `_clip_description`, cap 500).
fn clip_description(text: &str, cap: usize) -> String {
    if text.chars().count() <= cap {
        text.to_string()
    } else {
        format!("{}…", text.chars().take(cap).collect::<String>())
    }
}

/// One record for the shared `tools` map; `required` lets the model attempt a
/// trivial call without a `tool_describe` round-trip.
fn shared_tool_record(entry: &CatalogEntry) -> Value {
    let required: Vec<String> = entry
        .schema
        .get("function")
        .and_then(|f| f.get("parameters"))
        .and_then(|p| p.get("required"))
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(|r| r.chars().take(64).collect())
                .take(32)
                .collect()
        })
        .unwrap_or_default();
    json!({
        "source": entry.source,
        "source_name": entry.source_name,
        "description": clip_description(&entry.description, 500),
        "required": required,
    })
}

/// Deterministic summaries of the catalog's sources.
fn available_source_summary(catalog: &[CatalogEntry]) -> Vec<Value> {
    let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
    for entry in catalog {
        *counts.entry(listing_group_label(&entry.source_name)).or_insert(0) += 1;
    }
    counts
        .into_iter()
        .map(|(name, tool_count)| json!({"name": name, "tool_count": tool_count}))
        .collect()
}

/// Read a list-of-strings bridge argument -> `Ok(items)` or `Err(error_json)`.
/// A bare string (a common model slip) is a one-item list; rejects non-lists,
/// all-blank lists, > `max_items`.
fn string_list_arg(
    args: &Value,
    key: &str,
    dedupe: bool,
    max_items: usize,
    retry_hint: &str,
) -> Result<Vec<String>, String> {
    let raw = args.get(key);
    let raw = match raw {
        Some(Value::String(s)) => json!([s]),
        Some(other) => other.clone(),
        None => Value::Null,
    };
    let items = raw.as_array().ok_or_else(|| {
        tool_error(format!("{key} is required and must be an array of strings"))
    })?;
    let mut out: Vec<String> = Vec::new();
    for item in items {
        let text = match item {
            Value::Null => String::new(),
            other => other.to_string().trim_matches('"').trim().to_string(),
        };
        if !text.is_empty() && (!dedupe || !out.contains(&text)) {
            out.push(text);
        }
    }
    if out.is_empty() {
        return Err(tool_error(format!(
            "{key} is required and must contain at least one non-empty string"
        )));
    }
    if out.len() > max_items {
        return Err(tool_error(format!(
            "too many {key}: {} > max {max_items}. {retry_hint}",
            out.len()
        )));
    }
    Ok(out)
}

/// `tool_search` bridge dispatch: BM25 search of the deferrable subset of
/// `current_tool_defs` (one catalog per call — the catalog is stateless).
/// Connectors are not ported, so `connector_search` is always local-only.
pub fn dispatch_tool_search(
    args: &Value,
    current_tool_defs: &[Value],
    config: Option<ToolSearchConfig>,
) -> String {
    let config = config.unwrap_or_default();
    let queries = match string_list_arg(
        args,
        "queries",
        false,
        MAX_QUERIES_PER_CALL,
        "Retry with fewer, more targeted queries.",
    ) {
        Ok(q) => q,
        Err(err) => return err,
    };
    let limit = match args.get("limit") {
        None | Some(Value::Null) => config.search_default_limit,
        Some(raw) => clamped_int(raw, config.search_default_limit, 1, config.max_search_limit),
    } as usize;
    let defer = config.effective_defer_tools();
    let deferrable = classify_tools(current_tool_defs, Some(&defer)).1;
    let catalog = build_catalog(&deferrable);
    let available_sources = available_source_summary(&catalog);
    let stats = corpus_stats(&catalog);

    let mut results = Vec::new();
    let mut tools_map: Map<String, Value> = Map::new();
    for query in &queries {
        let hits = search_catalog(&catalog, query, limit, Some(&stats));
        for h in &hits {
            tools_map
                .entry(h.name.clone())
                .or_insert_with(|| shared_tool_record(h));
        }
        let matches: Vec<&str> = hits.iter().map(|h| h.name.as_str()).collect();
        let mut group = json!({"query": query, "matches": matches});
        if group["matches"].as_array().unwrap().is_empty() && !available_sources.is_empty() {
            group["available_sources"] = serde_json::to_value(&available_sources).unwrap();
            group["hint"] = json!(
                "This query returned no lexical matches, but the sources above are connected \
                 and their tools remain available. Retry tool_search with the service name plus \
                 a concrete action or object before concluding the capability is unavailable."
            );
        }
        results.push(group);
    }
    let payload = json!({
        "queries": queries,
        "total_available": catalog.len(),
        "results": results,
        "tools": Value::Object(tools_map),
    });
    serde_json::to_string(&payload).unwrap_or_else(|_| "{}".into())
}

/// `tool_describe` bridge dispatch: full schemas for deferred tools by exact
/// name. Found entries carry {description, parameters}; the rest land in
/// not_found (with a refresh hint) or errors (core tool absent from this
/// session / registered-but-not-deferrable).
pub fn dispatch_tool_describe(
    args: &Value,
    current_tool_defs: &[Value],
    config: Option<ToolSearchConfig>,
) -> String {
    let config = config.unwrap_or_default();
    let names = match string_list_arg(
        args,
        "names",
        true,
        MAX_DESCRIBE_NAMES_PER_CALL,
        "Retry with fewer names per call.",
    ) {
        Ok(n) => n,
        Err(err) => return err,
    };
    let defer = config.effective_defer_tools();
    let deferrable = classify_tools(current_tool_defs, Some(&defer)).1;
    let by_name: std::collections::HashMap<String, &Value> =
        deferrable.iter().map(|td| (fn_name(td), td)).collect();
    let session_names = session_tool_names(current_tool_defs);
    let session_names: Option<Vec<String>> = if session_names.is_empty() {
        None
    } else {
        Some(session_names)
    };

    let mut tools: Map<String, Value> = Map::new();
    let mut not_found: Vec<Value> = Vec::new();
    let mut errors: Map<String, Value> = Map::new();
    for name in &names {
        if let Some(td) = by_name.get(name) {
            let fn_block = function_block(td);
            tools.insert(
                name.clone(),
                json!({
                    "description": fn_block.get("description").and_then(Value::as_str).unwrap_or(""),
                    "parameters": fn_block.get("parameters").cloned().unwrap_or(json!({})),
                }),
            );
        } else if let Some(reason) = session_unavailable_error(name, session_names.as_deref()) {
            // A core tool absent here, deferred or not: tool_search cannot surface it.
            errors.insert(name.clone(), Value::String(reason));
        } else if crate::registry::spec_exists(name)
            && !is_deferrable_tool_name(name, Some(&config.effective_defer_tools()))
        {
            // Registered but bridge/core/GUI-surface: a real name, wrong door.
            errors.insert(
                name.clone(),
                Value::String(not_deferrable_error(name, session_names.as_deref())),
            );
        } else {
            not_found.push(Value::String(name.clone()));
        }
    }
    let mut result = json!({ "tools": Value::Object(tools) });
    if !not_found.is_empty() {
        result["not_found"] = Value::Array(not_found);
        result["hint"] = json!(
            "Names in not_found are not currently available. Re-run tool_search to refresh."
        );
    }
    if !errors.is_empty() {
        result["errors"] = Value::Object(errors);
    }
    serde_json::to_string(&result).unwrap_or_else(|_| "{}".into())
}

/// Parse a `tool_call` invocation into `(underlying_name, args)`. Used by the
/// dispatcher, the display layer and the trajectory recorder. Without the
/// connector subsystem there is no batch sentinel: a multi-entry batch naming
/// a local tool is rejected with `local_batch_error`.
pub fn resolve_underlying_call(
    args: &Value,
    session_names: Option<&[String]>,
    config: Option<ToolSearchConfig>,
) -> Result<(String, Value), String> {
    let entries = normalize_tool_call_entries(args)?;
    if entries.len() > 1 {
        return Err(local_batch_error(&entries));
    }
    let (name, raw_args) = &entries[0];
    if let Some(unavailable) = session_unavailable_error(name, session_names) {
        return Err(tool_error(unavailable));
    }
    let config = config.unwrap_or_default();
    if !is_deferrable_tool_name(name, Some(&config.effective_defer_tools())) {
        return Err(tool_error(not_deferrable_error(
            name,
            session_names,
        )));
    }
    Ok((name.clone(), raw_args.clone()))
}

// ---------------------------------------------------------------------------
// Registry entry points
// ---------------------------------------------------------------------------

/// The registry's specs rendered as the OpenAI-style tool-defs the bridge
/// operates on (the Rust "session tool_defs").
pub fn registry_tool_defs() -> Vec<Value> {
    crate::registry::specs()
        .iter()
        .map(|s| {
            json!({
                "type": "function",
                "function": {
                    "name": s.name,
                    "description": s.description,
                    "parameters": s.schema,
                }
            })
        })
        .collect()
}

/// `tool_search` from the registry — the catalog is the registry's deferrable
/// subset (default defer list intersected with the built-ins).
pub fn execute_search(args: &Value) -> String {
    let defs = registry_tool_defs();
    dispatch_tool_search(args, &defs, None)
}

/// `tool_describe` from the registry.
pub fn execute_describe(args: &Value) -> String {
    let defs = registry_tool_defs();
    dispatch_tool_describe(args, &defs, None)
}

/// `tool_call` from the registry: validate, then dispatch through the normal
/// sync registry path (approvals/hooks apply there, as in Python).
pub fn execute_call(args: &Value) -> String {
    let defs = registry_tool_defs();
    let names: Vec<String> = defs.iter().map(|td| fn_name(td)).filter(|n| !n.is_empty()).collect();
    let (name, call_args) = match resolve_underlying_call(args, Some(&names), None) {
        Ok(pair) => pair,
        Err(err) => return err,
    };
    if let Some(err) = validate_deferred_call_args(&name, &call_args) {
        return err;
    }
    crate::registry::execute_sync(&name, &call_args)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::from_str;

    fn v(s: &str) -> Value {
        from_str::<Value>(s).unwrap()
    }

    fn def(name: &str, desc: &str, params: Value) -> Value {
        json!({"type": "function", "function": {"name": name, "description": desc, "parameters": params}})
    }

    fn gmail_like() -> Vec<Value> {
        vec![
            def(
                "mcp__gmail__send",
                "Send an email through the user's Gmail account.",
                json!({"type": "object", "properties": {"to": {"type": "string"}}, "required": ["to"]}),
            ),
            def(
                "mcp__incidents__list",
                "List incident reports by email notification tag.",
                json!({"type": "object", "properties": {"tag": {"type": "string"}}}),
            ),
            def(
                "mcp__github__create_issue",
                "Create a new issue in a GitHub repository.",
                json!({"type": "object", "properties": {"repo": {"type": "string"}, "title": {"type": "string"}}, "required": ["repo", "title"]}),
            ),
        ]
    }

    #[test]
    fn config_defaults_and_legacy_bool() {
        let cfg = ToolSearchConfig::from_raw(&Value::Null);
        assert_eq!(cfg.enabled, "auto");
        assert_eq!(cfg.threshold_pct, 5.0);
        assert_eq!(cfg.search_default_limit, 5);
        assert_eq!(cfg.max_search_limit, 25);
        assert_eq!(cfg.listing, "auto");
        assert_eq!(cfg.listing_max_tokens, 4000);
        // Legacy bool: false -> off, true/non-bool -> auto.
        assert_eq!(ToolSearchConfig::from_raw(&json!(false)).enabled, "off");
        assert_eq!(ToolSearchConfig::from_raw(&json!(true)).enabled, "auto");
        // Clamps: threshold 0..100, listing_max_tokens 200..60000.
        let cfg = ToolSearchConfig::from_raw(&json!({
            "threshold_pct": 500, "listing_max_tokens": 1, "max_search_limit": 0
        }));
        assert_eq!(cfg.threshold_pct, 100.0);
        assert_eq!(cfg.listing_max_tokens, 200);
        assert_eq!(cfg.max_search_limit, 1);
        // tri-state aliases.
        assert_eq!(tri_state(&json!("YES")), "on");
        assert_eq!(tri_state(&json!("0")), "off");
        assert_eq!(tri_state(&json!("banana")), "auto");
    }

    #[test]
    fn effective_defer_uses_curated_default_and_explicit_override() {
        let cfg = ToolSearchConfig::from_raw(&Value::Null);
        let defer = cfg.effective_defer_tools();
        assert!(defer.contains(&"session_search".to_string()));
        assert!(defer.contains(&"todo_list".to_string()));
        // [] keeps everything eager; a scalar falls back to the default.
        let cfg = ToolSearchConfig::from_raw(&json!({"defer": []}));
        assert!(cfg.effective_defer_tools().is_empty());
        let cfg = ToolSearchConfig::from_raw(&json!({"defer": "todo_list"}));
        assert_eq!(cfg.effective_defer_tools(), DEFAULT_DEFERRED_TOOLS.iter().map(|s| (*s).into()).collect::<Vec<String>>());
    }

    #[test]
    fn tokenize_stems_consistently() {
        // Index/query share the stemmer: "issues" must match create_issue.
        let doc = tokenize("create_issue repo title");
        assert!(doc.contains(&tokenize("issues")[0]), "{doc:?}");
        assert_eq!(tokenize("Issues"), tokenize("issue"));
        assert_eq!(tokenize("creating"), tokenize("created"));
        assert_eq!(tokenize("processes"), tokenize("process"));
        assert!(tokenize("").is_empty());
        // Non-word chars split tokens.
        assert_eq!(tokenize("send-email fast!").len(), 3);
    }

    #[test]
    fn bm25_gate_and_exact_name_ranking() {
        let defs = gmail_like();
        let catalog = build_catalog(&defs);
        // The gate: "send gmail email" must NOT fill slots with the incident
        // tool that only shares "email".
        let hits = search_catalog(&catalog, "send gmail email", 5, None);
        assert!(!hits.is_empty());
        assert!(hits.iter().all(|h| h.name != "mcp__incidents__list"));
        assert_eq!(hits[0].name, "mcp__gmail__send");
        // Exact (case-insensitive) name match ranks first regardless of BM25.
        let hits = search_catalog(&catalog, "MCP__GitHub__Create_Issue", 5, None);
        assert_eq!(hits[0].name, "mcp__github__create_issue");
        // A token no document carries admits nothing.
        assert!(search_catalog(&catalog, "nonexistentwidget", 5, None).is_empty());
        // limit=0 / empty catalog -> empty.
        assert!(search_catalog(&catalog, "gmail", 0, None).is_empty());
        assert!(search_catalog(&[], "gmail", 5, None).is_empty());
    }

    #[test]
    fn short_desc_first_sentence_and_word_boundary_clip() {
        assert_eq!(short_desc("  Send   an email. Then more. ", 60), "Send an email.");
        // e.g. does not end a sentence.
        assert_eq!(
            short_desc("Uses e.g. Gmail. Second sentence.", 60),
            "Uses e.g. Gmail."
        );
        // Over-budget clips on a word boundary with an ellipsis.
        let text = (0..40).map(|i| format!("w{i} ")).collect::<String>();
        let out = short_desc(&text, 60);
        assert!(out.ends_with('…'));
        assert!(!out.ends_with("w39…"));
        assert!(out.chars().count() <= 61);
    }

    #[test]
    fn listing_degrades_full_names_then_summary() {
        // Fits: full form with per-tool lines.
        let (text, form) = build_catalog_listing_with_form(&gmail_like(), 4000);
        assert_eq!(form, "full");
        let text = text.unwrap();
        assert!(text.starts_with(&format!(
            "Deferred tool catalog (call schemas via `{TOOL_DESCRIBE_NAME}`, invoke via `{TOOL_CALL_NAME}`):"
        )));
        assert!(text.contains("- mcp__gmail__send: Send an email through the user's"));
        // Tiny budget: full doesn't fit (~102 tokens) but names-only does.
        let (_, form) = build_catalog_listing_with_form(&gmail_like(), 60);
        assert_eq!(form, "names");
        // Zero budget: nothing fits.
        let (text, form) = build_catalog_listing_with_form(&gmail_like(), 0);
        assert_eq!(form, "none");
        assert!(text.is_none());
    }

    #[test]
    fn classify_and_estimate_tokens() {
        let defs = gmail_like();
        let (visible, deferrable) = classify_tools(&defs, None);
        // All three defer (MCP names, not Rust registry built-ins).
        assert!(visible.is_empty());
        assert_eq!(deferrable.len(), 3);
        // Bridge tools never defer, even when named — and classify DROPS them
        // (re-added after classification).
        let defs = vec![def("tool_call", "bridge", json!({}))];
        let (visible, deferrable) = classify_tools(&defs, Some(&["tool_call".into()]));
        assert!(visible.is_empty());
        assert!(deferrable.is_empty());
        assert!(!is_deferrable_tool_name("tool_call", Some(&["tool_call".into()])));
        // Core built-ins stay eager unless named in the (explicit or curated
        // default) defer list; with the default config todo_list defers.
        let defer = ToolSearchConfig::default().effective_defer_tools();
        let defs = vec![
            def("read_file", "core", json!({})),
            def("todo_list", "deferred core", json!({})),
        ];
        let (visible, deferrable) = classify_tools(&defs, Some(&defer));
        assert_eq!(visible.len(), 1);
        assert_eq!(fn_name(&visible[0]), "read_file");
        assert_eq!(fn_name(&deferrable[0]), "todo_list");
        // Token estimate is the chars/4 rule.
        assert!(estimate_tokens_from_schemas(&gmail_like()) > 0);
    }

    #[test]
    fn activation_and_budget() {
        let cfg = ToolSearchConfig::from_raw(&json!({"enabled": "off"}));
        assert!(!should_activate(&cfg, 100, false));
        let cfg = ToolSearchConfig::from_raw(&json!({"enabled": "on"}));
        assert!(should_activate(&cfg, 1, false));
        assert!(!should_activate(&cfg, 0, false));
        // Budget: min(listing_max_tokens, 5% of context); unknown context -> 10K leg.
        let cfg = ToolSearchConfig::from_raw(&json!({"threshold_pct": 5, "listing_max_tokens": 4000}));
        assert_eq!(listing_token_budget(&cfg, Some(200_000)), 4000);
        assert_eq!(listing_token_budget(&cfg, Some(20_000)), 1000);
        assert_eq!(listing_token_budget(&cfg, None), 4000);
    }

    #[test]
    fn assembly_replaces_deferrable_with_bridge_and_is_idempotent() {
        let defs = gmail_like();
        let out = assemble_tool_defs(&defs, Some(200_000), None);
        assert!(out.activated);
        assert_eq!(out.deferred_count, 3);
        assert_eq!(out.listing_form, "full");
        assert_eq!(out.tier, 1);
        let names: Vec<String> = out.tool_defs.iter().map(|td| fn_name(td)).collect();
        // Only the 3 bridge schemas (no visible tools remain).
        assert_eq!(names.len(), 3);
        assert!(names.iter().all(|n| is_bridge_tool(n)));
        // Idempotent: re-assembly strips existing bridge tools first; nothing
        // deferrable remains, so the passthrough wins.
        let again = assemble_tool_defs(&out.tool_defs, Some(200_000), None);
        assert!(!again.activated);
        assert!(again.tool_defs.is_empty());
        assert_eq!(again.deferred_count, 0);
        // "off" passes everything through untouched.
        let cfg = ToolSearchConfig::from_raw(&json!({"enabled": "off"}));
        let out = assemble_tool_defs(&defs, Some(200_000), Some(cfg));
        assert!(!out.activated);
        assert_eq!(out.tool_defs.len(), 3);
    }

    #[test]
    fn tool_search_rejects_bad_arguments() {
        let defs = gmail_like();
        // Missing queries.
        let out = dispatch_tool_search(&json!({}), &defs, None);
        assert!(out.contains("queries is required and must be an array of strings"), "{out}");
        // A bare string is a one-item list (common model slip).
        let out = dispatch_tool_search(&json!({"queries": "gmail send email"}), &defs, None);
        let parsed = v(&out);
        assert_eq!(parsed["queries"].as_array().unwrap().len(), 1);
        // Too many queries.
        let queries: Vec<&str> = (0..8).map(|i| "q").collect();
        let out = dispatch_tool_search(&json!({"queries": queries}), &defs, None);
        assert!(out.contains("too many queries: 8 > max 7"), "{out}");
        // All-blank list.
        let out = dispatch_tool_search(&json!({"queries": ["", "  "]}), &defs, None);
        assert!(out.contains("at least one non-empty string"), "{out}");
    }

    #[test]
    fn tool_search_payload_shape_and_empty_group_hint() {
        let defs = gmail_like();
        let out = dispatch_tool_search(
            &json!({"queries": ["gmail send email", "zzz-nonexistent"], "limit": 2}),
            &defs,
            None,
        );
        let parsed = v(&out);
        assert_eq!(parsed["total_available"], 3);
        let results = parsed["results"].as_array().unwrap();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0]["matches"][0], "mcp__gmail__send");
        assert!(results[0]["matches"].as_array().unwrap().len() <= 2);
        // Empty group carries available_sources + the retry hint.
        assert!(results[1]["matches"].as_array().unwrap().is_empty());
        assert!(results[1]["available_sources"].is_array());
        assert!(results[1]["hint"].as_str().unwrap().contains("Retry tool_search"));
        // Shared tools map records carry source, clipped description, required.
        let rec = &parsed["tools"]["mcp__gmail__send"];
        assert_eq!(rec["source"], "mcp");
        assert_eq!(rec["source_name"], "mcp-gmail");
        assert_eq!(rec["required"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn tool_describe_returns_schemas_and_errors() {
        let defs = gmail_like();
        // Bare string name accepted.
        let out = dispatch_tool_describe(&json!({"names": "mcp__gmail__send"}), &defs, None);
        let parsed = v(&out);
        let schema = &parsed["tools"]["mcp__gmail__send"];
        assert_eq!(schema["parameters"]["properties"]["to"]["type"], "string");
        // Unknown names -> not_found + hint.
        let out = dispatch_tool_describe(&json!({"names": ["mcp__gmail__send", "mcp__nope"]}), &defs, None);
        let parsed = v(&out);
        assert_eq!(parsed["not_found"][0], "mcp__nope");
        assert!(parsed["hint"].as_str().unwrap().contains("Re-run tool_search"));
        // Too many DISTINCT names (dedupe collapses repeats first).
        let names: Vec<String> = (0..11).map(|i| format!("n{i}")).collect();
        let out = dispatch_tool_describe(&json!({"names": names}), &defs, None);
        assert!(out.contains("too many names: 11 > max 10"), "{out}");
        // A core (Rust-registry) tool absent from this session's defs: it is
        // unreachable through the bridge, whatever its door.
        let out = dispatch_tool_describe(&json!({"names": ["read_file"]}), &defs, None);
        assert!(out.contains("not available in this execution context"), "{out}");
        // But when the session's own defs DO carry it, it is a wrong-door call.
        let all_defs = registry_tool_defs();
        let out = dispatch_tool_describe(&json!({"names": ["read_file"]}), &all_defs, None);
        assert!(out.contains("directly-listed tool"), "{out}");
        // Missing/invalid arg shapes.
        let out = dispatch_tool_describe(&json!({"names": {}}), &defs, None);
        assert!(out.contains("names is required"), "{out}");
    }

    #[test]
    fn normalize_tool_call_entries_shapes() {
        // Batch shape.
        let entries = normalize_tool_call_entries(&json!({
            "calls": [
                {"name": "a", "arguments": {"x": 1}},
                {"name": "b", "arguments": "{\"y\": 2}"}
            ]
        })).unwrap();
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[1].0, "b");
        assert_eq!(entries[1].1["y"], 2);
        // Legacy single shape.
        let entries = normalize_tool_call_entries(&json!({"name": "a", "arguments": {}})).unwrap();
        assert_eq!(entries.len(), 1);
        // calls as a JSON string envelope.
        let entries = normalize_tool_call_entries(&json!({
            "calls": "[{\"name\": \"a\", \"arguments\": {}}]"
        })).unwrap();
        assert_eq!(entries.len(), 1);
        // Empty/null arguments coerce to {}.
        let entries = normalize_tool_call_entries(&json!({"calls": [{"name": "a", "arguments": ""}]})).unwrap();
        assert_eq!(entries[0].1, json!({}));
        // Errors.
        assert!(normalize_tool_call_entries(&json!({}))
            .unwrap_err()
            .contains("requires 'calls'"));
        assert!(normalize_tool_call_entries(&json!({"calls": []})).is_err());
        assert!(normalize_tool_call_entries(&json!({"calls": [{"arguments": {}}]})).is_err());
        assert!(normalize_tool_call_entries(&json!({"calls": [{"name": "tool_search", "arguments": {}}]}))
            .unwrap_err()
            .contains("it is itself a bridge tool"));
        assert!(normalize_tool_call_entries(&json!({"calls": [{"name": "a", "arguments": "{bad"}]}))
            .unwrap_err()
            .contains("is not valid JSON"));
    }

    #[test]
    fn resolve_underlying_call_local_batch_rejected_with_echo() {
        let args = json!({
            "calls": [
                {"name": "mcp__gmail__send", "arguments": {}},
                {"name": "mcp__gmail__send", "arguments": {}}
            ]
        });
        let err = resolve_underlying_call(&args, None, None).unwrap_err();
        assert!(err.contains("tool_call takes exactly one entry for local tools; you sent 2"), "{err}");
        assert!(err.contains("Retry with only:"));
    }

    #[test]
    fn resolve_underlying_call_scope_errors() {
        let args = json!({"calls": [{"name": "read_file", "arguments": {}}]});
        // A directly-listed core tool through the bridge.
        let err = resolve_underlying_call(&args, Some(&["read_file".into()]), None).unwrap_err();
        assert!(err.contains("directly-listed tool"), "{err}");
        // A core tool the session doesn't carry: unavailable, not "call it directly".
        let err = resolve_underlying_call(&args, Some(&["terminal".into()]), None).unwrap_err();
        assert!(err.contains("not available in this execution context"), "{err}");
        // Unknown non-MCP name (an mcp__ name defers by prefix, so use a bare
        // unknown name to reach the unknown-tool rejection).
        let args = json!({"calls": [{"name": "frobnicate", "arguments": {}}]});
        let err = resolve_underlying_call(&args, None, None).unwrap_err();
        assert!(err.contains("is not a known tool name"), "{err}");
        // Happy path.
        let args = json!({"calls": [{"name": "mcp__gmail__send", "arguments": {"to": "x"}}]});
        let (name, a) = resolve_underlying_call(&args, None, None).unwrap();
        assert_eq!(name, "mcp__gmail__send");
        assert_eq!(a["to"], "x");
    }

    #[test]
    fn validate_deferred_call_args_required_probe() {
        // Missing required -> concrete error naming the field (maps requires
        // `action`).
        let err = validate_deferred_call_args("maps", &json!({}));
        let err = err.unwrap();
        assert!(err.contains("missing required argument(s): action"), "{err}");
        assert!(err.contains("\"constraint\":\"required\""));
        assert!(err.contains("The tool was NOT invoked."));
        // Satisfied -> dispatch (None).
        assert!(validate_deferred_call_args("maps", &json!({"action": "search"})).is_none());
        // Unknown tool fails open.
        assert!(validate_deferred_call_args("nope", &json!({})).is_none());
    }

    #[test]
    fn registry_entry_points_work() {
        let out = execute_search(&json!({"queries": ["schedule a cron job"]}));
        let parsed = v(&out);
        assert_eq!(parsed["results"][0]["matches"][0], "cronjob_manage", "{out}");
        let out = execute_describe(&json!({"names": ["cronjob_manage"]}));
        assert!(v(&out)["tools"]["cronjob_manage"]["parameters"].is_object(), "{out}");
        // tool_call through the bridge executes the underlying tool.
        let out = execute_call(&json!({
            "calls": [{"name": "todo_list", "arguments": {"todos": [{"id": "1", "content": "wire it", "status": "pending"}]}}]
        }));
        assert!(out.contains("\"revision\""), "{out}");
        // tool_call validates required args before dispatching.
        let out = execute_call(&json!({"calls": [{"name": "todo_list", "arguments": {}}]}));
        // todo_list has no required args -> dispatches; use one with required.
        let out = execute_call(&json!({"calls": [{"name": "session_search", "arguments": {"around_message_id": "x"}}]}));
        assert!(!out.is_empty());
    }
}
