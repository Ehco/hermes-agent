//! web_extract — fetch URLs directly and return cleaned page content.
//!
//! Port of `tools/web_tools.py::web_extract_tool` + `tools/web_tools_extract.py`
//! + `tools/web_tools_truncate.py` from the Python hermes-agent, with the
//! vendor backends replaced by a direct reqwest GET + local HTML→text pass
//! (the Rust crate has no Firecrawl SDK; the tool-level contract is what must
//! stay identical for the model).
//!
//! Pipeline (each a gate, never skipped): model-supplied URL normalization →
//! secret-in-URL refusal (whole call) → SSRF filter (per entry) → fetch + HTML
//! → text → base64-image cleanup → binary-payload detection → truncate-and-store.
//!
//! Output contract: `{"results": [{url, title, content, error}]}` — entries
//! carry only those keys (`_trim_results`); a whole-call failure is
//! `{"success": false, "error": ...}`; when nothing was extracted the Python
//! emits `tool_error("Content was inaccessible or not found")`.

use regex::Regex;
use serde_json::{json, Value};
use std::net::{IpAddr, ToSocketAddrs};
use std::time::Duration;

use crate::html_text::{extract_title, html_to_text};
use crate::read_file::path_display;
use crate::util::tool_error;

use super::web_common::{
    base64_images_to_links, hermes_cache_web_dir, host_slug, page_error, result_entry, sha256_hex10,
};

/// `web_tools_truncate.DEFAULT_EXTRACT_CHAR_LIMIT` / `MAX_STORED_TEXT_CHARS`.
pub const DEFAULT_EXTRACT_CHAR_LIMIT: usize = 15_000;
const MAX_STORED_TEXT_CHARS: usize = 2_000_000;
const CHAR_LIMIT_FLOOR: usize = 2000;
const CHAR_LIMIT_CEILING: usize = 500_000;
/// `web_tools_extract._DEFAULT_EXTRACT_TIMEOUT_S`.
const DEFAULT_EXTRACT_TIMEOUT_S: u64 = 120;
const MAX_URLS: usize = 5;
const NO_RESULT_ERROR: &str = "Extract backend returned no result for this URL";

/// `url_safety._SENSITIVE_QUERY_PARAM_NAMES` — deliberately narrow: bare
/// English words (`code`, `key`, `session`) are excluded so ordinary browsing
/// is not blocked.
const SENSITIVE_QUERY_PARAM_NAMES: &[&str] = &[
    "access_token", "api_key", "apikey", "auth_token", "authorization", "awsaccesskeyid",
    "client_secret", "credential", "credentials", "jwt", "password", "passwd", "secret",
    "session_id", "signature", "token", "x_amz_security_token", "x_amz_signature",
    "x-amz-security-token", "x-amz-signature",
];

/// `agent/redact._PREFIX_PATTERNS` subset that can appear inside a URL: the
/// well-known credential token shapes. (`_PREFIX_RE` is applied to the URL
/// verbatim; a full port of every vendor prefix is unnecessary here.)
const SECRET_URL_PREFIXES: &[&str] = &[
    "sk-", "ghp_", "ghs_", "github_pat_", "glpat-", "xoxb-", "xoxp-", "AKIA", "Bearer ",
    "eyJhb", // bare JWT bodies
];

/// `web_tools_truncate._clamp_char_limit`: clamp to [2k, 500k]; `None` or
/// non-numeric input falls back to the default.
fn clamp_or_default(value: Option<i64>) -> usize {
    match value {
        Some(v) if v >= 0 => CHAR_LIMIT_FLOOR.max((v as usize).min(CHAR_LIMIT_CEILING)),
        _ => DEFAULT_EXTRACT_CHAR_LIMIT,
    }
}

/// `web_tools_extract._web_extract_url`: URL from a model-supplied item
/// (string, or object with a string `url`/`href` field).
fn web_extract_url(value: &Value) -> Option<String> {
    let candidate = match value {
        Value::String(s) => Some(s.as_str()),
        Value::Object(map) => map
            .get("url")
            .or_else(|| map.get("href"))
            .and_then(Value::as_str),
        _ => None,
    }?;
    let trimmed = candidate.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// `web_tools_extract._validate_extract_urls`: normalize items and refuse the
/// whole call when any URL carries a secret (percent-encoded forms included).
/// Returns `(urls, indices, invalid)`, or a whole-call refusal JSON.
fn validate_extract_urls(urls: &[Value]) -> Result<(Vec<String>, Vec<usize>, std::collections::BTreeMap<usize, Value>), String> {
    let mut normalized = Vec::new();
    let mut indices = Vec::new();
    let mut invalid = std::collections::BTreeMap::new();
    for (index, item) in urls.iter().enumerate() {
        let Some(url) = web_extract_url(item) else {
            invalid.insert(
                index,
                result_entry(
                    "",
                    &format!("Invalid URL item at index {index}: expected a URL string or an object with a string 'url' or 'href' field"),
                ),
            );
            continue;
        };
        let decoded = percent_decode_url(&url);
        if sensitive_url(&url) || sensitive_url(&decoded) {
            return Err(
                "Blocked: URL contains what appears to be an API key or token. \
                 Secrets must not be sent in URLs."
                    .to_string(),
            );
        }
        normalized.push(url);
        indices.push(index);
    }
    Ok((normalized, indices, invalid))
}

/// `%XX` / `+` in URLs, ASCII-only (mirrors `urllib.parse.unquote` for gates).
pub fn percent_decode_url(url: &str) -> String {
    let mut out = String::with_capacity(url.len());
    let bytes = url.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let Ok(b) = u8::from_str_radix(&url[i + 1..i + 3], 16) {
                out.push(b as char); // ASCII gates only; lossless for the check
                i += 3;
                continue;
            }
        }
        // Re-push the raw byte run up to the next multi-byte char boundary.
        let ch_len = utf8_len(bytes[i]);
        out.push_str(&String::from_utf8_lossy(&bytes[i..(i + ch_len).min(bytes.len() + 1).min(bytes.len()).max(i + ch_len)]));
        i += ch_len;
    }
    out
}

fn utf8_len(first: u8) -> usize {
    match first {
        b if b < 0x80 => 1,
        b if b >> 5 == 0b110 => 2,
        b if b >> 4 == 0b1110 => 3,
        _ => 4,
    }
}

/// `url_safety.sensitive_query_param_name` + a `_PREFIX_RE` subset: true when
/// the URL appears to carry a credential.
fn sensitive_url(url: &str) -> bool {
    let lower = url.to_lowercase();
    if SECRET_URL_PREFIXES.iter().any(|p| lower.contains(&p.to_lowercase())) {
        return true;
    }
    // Query parameter names (split on & / ? so a name inside a word isn't hit).
    let query = lower.split(['?', '&']).skip(1);
    for chunk in query {
        let name = chunk.split(['=', ';']).next().unwrap_or("");
        if SENSITIVE_QUERY_PARAM_NAMES.contains(&name) {
            return true;
        }
    }
    false
}

/// `url_safety.async_is_safe_url` (simplified): block loopback, RFC1918 /
/// link-local / CGNAT / ULA ranges, the unspecified address, and hosts that
/// resolve to them. Cloud metadata (169.254.169.254) is link-local and so
/// covered. Literal IPs are checked directly; hostnames are resolved first.
fn is_safe_url(url: &str) -> bool {
    let Some(host) = url_host(url) else { return false; };
    let host_lower = host.to_lowercase();
    if host_lower == "localhost" || host_lower.ends_with(".localhost") || host_lower == "metadata" {
        return false;
    }
    // Literal IP?
    if let Ok(ip) = host.parse::<IpAddr>() {
        return is_public_ip(ip);
    }
    match (host.as_str(), 443u16).to_socket_addrs() {
        Ok(mut iter) => iter.all(|sa| is_public_ip(sa.ip())),
        // DNS failure is a fetch-time error, not an SSRF block (Python only
        // blocks on private resolution).
        Err(_) => true,
    }
}

/// Parse the host out of an http(s) URL (works for the odd shapes models emit).
pub fn url_host(url: &str) -> Option<String> {
    let re = Regex::new("(?i)^[a-z][a-z0-9+.-]*://([^/?#\\s]+)").unwrap();
    let authority = re.captures(url)?.get(1)?.as_str();
    let authority = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = match authority.rsplit_once(':') {
        // IPv6 literal in brackets or host:port.
        Some((h, port)) if port.chars().all(|c| c.is_ascii_digit()) && !h.is_empty() => h,
        Some(_) => authority, // bare [::1] style or missing port
        None => authority,
    };
    let host = host.trim_matches(['[', ']']);
    (!host.is_empty()).then(|| host.to_string())
}

fn is_public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            !matches!(o[0], 0 | 10 | 127 | 169) // 0/8, 10/8, 127/8, 169.254/16
                && !(o[0] == 172 && (16..=31).contains(&o[1])) // 172.16/12
                && !(o[0] == 192 && o[1] == 168) // 192.168/16
                && !(o[0] == 192 && o[1] == 0 && (o[2] == 0 || o[2] == 2)) // IETF testnets
                && !(o[0] == 100 && (64..=127).contains(&o[1])) // 100.64/10 CGNAT
                && !(o[0] == 198 && (o[1] == 18 || o[1] == 19)) // benchmarking
                && !(o[0] == 203 && o[1] == 113) // TEST-NAT-3
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments();
            !(v6.is_loopback()
                || v6.is_unspecified()
                || (seg[0] & 0xfe00) == 0xfe00 // fe80::/10 link-local
                || (seg[0] & 0xfc00) == 0xfc00) // fc00::/7 ULA
        }
    }
}

/// `web_tools_truncate._binary_payload_kind` for the unambiguous set: name the
/// binary kind when the (NUL-stripped) head matches a known magic signature.
fn binary_payload_kind(text: &str) -> Option<&'static str> {
    let head_nul_stripped: Vec<u8> = text
        .as_bytes()
        .iter()
        .copied()
        .filter(|b| *b != 0)
        .take(32)
        .collect();
    const SIGS: &[(&[u8], &str)] = &[
        // Signatures compared against the NUL-stripped head of the decoded
        // text, so only UTF-8-safe (ASCII) magics are reliable here: the
        // non-ASCII ones (gzip/PNG/JPEG/...) get mangled by the charset
        // decode and are covered by the vendor backends instead.
        (b"SQLite format 3", "SQLite"),
        (b"PK\x03\x04", "ZIP"),
        (b"BZh", "bzip2"),
        (b"\x7fELF", "ELF"),
        (b"GIF8", "GIF"),
        (b"FLAC", "FLAC"),
        (b"OggS", "Ogg"),
    ];
    for (sig, name) in SIGS {
        if head_nul_stripped.starts_with(sig) {
            return Some(name);
        }
    }
    None
}

/// `web_tools_truncate._store_full_text`: write the full page to
/// `cache/web/{host}-{hash10}.md` (best-effort; None on failure). Python
/// names it with `sha256(url)[:10]` — any deterministic short digest keeps the
/// same property, so a tiny local SHA-256 stands in for the hex crate.
fn store_full_text(url: &str, content: &str) -> Option<String> {
    let dir = hermes_cache_web_dir()?;
    std::fs::create_dir_all(&dir).ok()?;
    let slug = host_slug(url);
    let path = dir.join(format!("{}-{}.md", slug, sha256_hex10(url.as_bytes())));
    let mut stored = content.to_string();
    if stored.len() > MAX_STORED_TEXT_CHARS {
        let cut = &stored[..stored.floor_char_boundary(MAX_STORED_TEXT_CHARS.min(stored.len()))];
        stored = format!(
            "{cut}\n\n[... stored copy truncated at {MAX_STORED_TEXT_CHARS} chars; re-extract a more specific URL for the rest ...]"
        );
    }
    std::fs::write(&path, stored).ok()?;
    Some(path_display(&path))
}

/// `web_tools_truncate._truncate_with_footer`: pages at/under the budget are
/// returned whole; larger pages become a ~75/25 head+tail window cut on line
/// boundaries plus a footer with the stored full-text path and the read_file
/// call that pages the omitted middle. Returns `(model_text, was_truncated)`.
fn truncate_with_footer(content: &str, url: &str, char_limit: usize) -> (String, bool) {
    if content.len() <= char_limit {
        return (content.to_string(), false);
    }
    let head_budget = char_limit * 75 / 100;
    let tail_budget = char_limit - head_budget;
    let head_boundary = content.floor_char_boundary(head_budget.min(content.len()));
    let mut head = &content[..head_boundary];
    let mut tail = &content[content.len() - content
        .ceil_char_boundary(content.len() - tail_budget.min(content.len()))
        .max(0)
        .min(content.len())..];
    // Snap both cuts to line boundaries (head back, tail forward).
    if let Some(nl) = head.rfind('\n') {
        if nl as f32 > head.len() as f32 * 0.5 {
            head = &head[..nl];
        }
    }
    if let Some(nl) = tail.find('\n') {
        if (nl as f32) < tail.len() as f32 * 0.5 {
            tail = &tail[nl + 1..];
        }
    }

    let stored_path = store_full_text(url, content);
    let head_len = head.len();
    let tail_len = tail.len();
    let mut footer_lines = vec![
        String::new(),
        "──────── [TRUNCATED] ────────".to_string(),
        format!(
            "Showing {head_len} chars (head) + {tail_len} chars (tail) of {len} total clean characters.",
            len = content.len()
        ),
    ];
    if let Some(path) = &stored_path {
        let middle_start_line = head.matches('\n').count() + 2;
        footer_lines.push(format!("Full text saved to: {path}"));
        footer_lines.push(format!(
            "To read the omitted middle: read_file path=\"{path}\" offset={middle_start_line} limit=200  (the file is the complete page; raise/lower offset to page through it)."
        ));
    } else {
        footer_lines.push(
            "Full text could not be stored; re-run web_extract on a more specific URL \
             or use browser_navigate for the complete page."
                .to_string(),
        );
    }
    footer_lines.push("─────────────────────────────".to_string());
    let model_text = format!(
        "{head}\n\n[... middle omitted — see footer ...]\n\n{tail}\n{}",
        footer_lines.join("\n")
    );
    (model_text, true)
}

/// Execute `web_extract` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    let urls = args.get("urls").and_then(Value::as_array).cloned().unwrap_or_default();
    let urls: Vec<Value> = urls.into_iter().take(MAX_URLS).collect();
    let char_limit = clamp_or_default(args.get("char_limit").and_then(Value::as_i64));

    let (normalized_urls, indices, mut fixed) = match validate_extract_urls(&urls) {
        Ok(v) => v,
        Err(error) => return json!({ "success": false, "error": error }).to_string(),
    };

    // SSRF gate per entry, before any fetch.
    let mut fetch_urls = Vec::new();
    let mut fetch_indices = Vec::new();
    for (url, index) in normalized_urls.iter().zip(indices.iter()) {
        if is_safe_url(url) {
            fetch_urls.push(url.clone());
            fetch_indices.push(*index);
        } else {
            fixed.insert(
                *index,
                page_error(url, "Blocked: URL targets a private or internal network address"),
            );
        }
    }

    let timeout = Duration::from_secs(DEFAULT_EXTRACT_TIMEOUT_S);
    let mut results: std::collections::BTreeMap<usize, Value> = std::collections::BTreeMap::new();
    for (url, index) in fetch_urls.iter().zip(fetch_indices.iter()) {
        results.insert(*index, fetch_and_convert(url, timeout));
    }

    // Reconstruct input order across invalid, blocked and fetched entries.
    let mut ordered = Vec::with_capacity(urls.len());
    let mut any_content = false;
    for i in 0..urls.len() {
        let entry = if let Some(v) = results.remove(&i).or_else(|| fixed.remove(&i)) {
            v
        } else {
            page_error(
                normalized_urls
                    .get(indices.iter().position(|&x| x == i).unwrap_or(usize::MAX))
                    .map(String::as_str)
                    .unwrap_or(""),
                NO_RESULT_ERROR,
            )
        };
        // `_trim_results`: keep only url/title/content/error (+ truncate).
        let obj = entry.as_object().unwrap();
        let url = obj.get("url").and_then(Value::as_str).unwrap_or("").to_string();
        let title = obj.get("title").and_then(Value::as_str).unwrap_or("").to_string();
        let mut error = obj
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string);
        let raw_content = obj
            .get("raw_content")
            .or_else(|| obj.get("content"))
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        let mut content = String::new();
        if error.is_none() && !raw_content.is_empty() {
            if let Some(kind) = binary_payload_kind(&raw_content) {
                // A backend that fetched a raw file (SQLite, archive,
                // executable) hands back its bytes as "text"; name the type
                // and point at the tool that reads it.
                error = Some(format!(
                    "URL returned binary content ({kind}), not a page. Download it with the terminal \
                     (curl -L -o) and use read_file (SQLite/Office/PDF auto-extract) or terminal \
                     utilities on the file."
                ));
            } else {
                let clean = base64_images_to_links(&raw_content);
                let (model_text, _) = truncate_with_footer(&clean, &url, char_limit);
                content = model_text;
            }
        }
        let mut trimmed = serde_json::Map::new();
        trimmed.insert("url".into(), json!(url));
        trimmed.insert("title".into(), json!(title));
        trimmed.insert("content".into(), json!(content));
        trimmed.insert("error".into(), match &error {
            Some(e) => json!(e),
            None => Value::Null,
        });
        if !content.is_empty() && error.is_none() {
            any_content = true;
        }
        ordered.push(Value::Object(trimmed));
    }

    if !any_content {
        return tool_error("Content was inaccessible or not found");
    }
    json!({ "results": ordered }).to_string()
}

/// Fetch one URL and convert it — the direct-GET extract backend.
fn fetch_and_convert(url: &str, timeout: Duration) -> Value {
    let client = crate::web_common::http_client();
    let response = match client.get(url).timeout(timeout).send() {
        Ok(resp) => resp,
        Err(e) => return page_error(url, &format!("Could not reach the URL: {e}")),
    };
    let status = response.status();
    if !status.is_success() {
        return page_error(url, &format!("HTTP {status}"));
    }
    let body = match response.text() {
        Ok(b) => b,
        Err(e) => return page_error(url, &format!("Could not read response body: {e}")),
    };
    let title = extract_title(&body).unwrap_or_default();
    let content = html_to_text(&body);
    if content.is_empty() {
        page_error(url, "Page contained no extractable text")
    } else {
        // `document()`: raw_content mirrors content for the truncate pipeline.
        json!({
            "url": url,
            "title": title,
            "content": content,
            "raw_content": content,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn char_limit_clamps() {
        assert_eq!(clamp_or_default(None), DEFAULT_EXTRACT_CHAR_LIMIT);
        assert_eq!(clamp_or_default(Some(50)), CHAR_LIMIT_FLOOR);
        assert_eq!(clamp_or_default(Some(10_000_000)), CHAR_LIMIT_CEILING);
        assert_eq!(clamp_or_default(Some(-5)), DEFAULT_EXTRACT_CHAR_LIMIT);
    }

    #[test]
    fn url_items_normalize() {
        assert_eq!(
            web_extract_url(&json!(" https://a.com ")),
            Some("https://a.com".into())
        );
        assert_eq!(
            web_extract_url(&json!({"url": "https://b.com"})),
            Some("https://b.com".into())
        );
        assert_eq!(
            web_extract_url(&json!({"href": "https://c.com"})),
            Some("https://c.com".into())
        );
        assert_eq!(web_extract_url(&json!({"href": "  "})), None);
        assert_eq!(web_extract_url(&json!(42)), None);
    }

    #[test]
    fn secret_urls_refuse_whole_call() {
        for url in [
            "https://a.com?token=abc123",
            "https://a.com?%74oken=abc",
            "https://a.com?api_key=x",
            "https://a.com/path?apikey=y",
            "https://a.com/#sk-abcdef",
        ] {
            let err = validate_extract_urls(&[json!(url)]).unwrap_err();
            assert!(err.contains("Blocked"), "{url}: {err}");
        }
        // Ordinary params must not be blocked (narrow name list).
        assert!(validate_extract_urls(&[json!("https://a.com?q=code&key=1")]).is_ok());
    }

    #[test]
    fn invalid_items_get_typed_entries() {
        let (urls, indices, invalid) =
            validate_extract_urls(&[json!("https://a.com"), json!(7)]).unwrap();
        assert_eq!(urls, vec!["https://a.com"]);
        assert_eq!(indices, vec![0]);
        assert!(invalid.get(&1).is_some());
        let entry = invalid.get(&1).unwrap();
        assert!(entry["error"].as_str().unwrap().contains("Invalid URL item at index 1"));
    }

    #[test]
    fn ssrf_blocks_private_addresses() {
        assert!(!is_safe_url("http://127.0.0.1:8080/x"));
        assert!(!is_safe_url("http://10.1.2.3/x"));
        assert!(!is_safe_url("http://192.168.1.1/x"));
        assert!(!is_safe_url("http://172.16.0.9/x"));
        assert!(!is_safe_url("http://169.254.169.254/latest/meta-data"));
        assert!(!is_safe_url("http://localhost:3000/admin"));
        assert!(!is_safe_url("http://[::1]/x"));
        assert!(!is_safe_url("http://0.0.0.0/x"));
        // Public literals pass. (A hostname here would depend on the local
        // resolver — sandboxed DNS can answer with private addresses, which
        // the SSRF gate then legitimately blocks.)
        assert!(is_safe_url("https://93.184.216.34/x"));
    }

    #[test]
    fn url_host_parses() {
        assert_eq!(url_host("https://a.com/p?q").as_deref(), Some("a.com"));
        assert_eq!(url_host("http://user:pw@h.example:8080").as_deref(), Some("h.example"));
        assert_eq!(url_host("http://[::1]:9000").as_deref(), Some("::1"));
        assert_eq!(url_host("not a url"), None);
    }

    #[test]
    fn binary_kinds_detected() {
        assert_eq!(binary_payload_kind("SQLite format 3\u{0}..."), Some("SQLite"));
        assert_eq!(binary_payload_kind("PK\u{3}\u{4}rest"), Some("ZIP"));
        assert_eq!(binary_payload_kind("\u{7f}ELFpayload"), Some("ELF"));
        assert_eq!(binary_payload_kind("<html><body>text"), None);
    }

    #[test]
    fn truncation_footer_includes_read_file_call() {
        let content: String = (0..2000).map(|i| format!("line{i}\n")).collect();
        let (text, truncated) = truncate_with_footer(&content, "https://example.com/big", 4000);
        assert!(truncated);
        assert!(text.contains("TRUNCATED"));
        assert!(text.contains("read_file path="));
        assert!(text.contains("offset="));
        assert!(text.contains("[... middle omitted — see footer ...]"));
        // Whole pages pass through untouched.
        let (text, truncated) = truncate_with_footer("short", "https://example.com", 4000);
        assert!(!truncated);
        assert_eq!(text, "short");
    }

    #[test]
    fn truncation_snaps_to_line_boundaries() {
        let long_line = "x".repeat(10_000);
        let content = format!("{long_line}\n{}\n{long_line}", "y".repeat(10_000));
        let (text, truncated) = truncate_with_footer(&content, "https://example.com", 3000);
        assert!(truncated);
        let body = text.split("\n\n[... middle omitted").next().unwrap();
        for line in body.lines() {
            assert!(
                line.starts_with('x') || line.starts_with('y'),
                "mid-line cut: {:?}",
                &line[..line.len().min(20)]
            );
        }
    }

    #[tokio::test]
    async fn private_url_blocks_entry_and_whole_call_errors_when_nothing_fetched() {
        // Direct entry-level: the execute path with only private URLs yields
        // the tool_error fallback (nothing accessible).
        let out = execute(&json!({"urls": ["http://127.0.0.1:9/secret=none"]}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_some(), "{out}");
    }

    #[tokio::test]
    async fn secret_url_is_a_whole_call_refusal() {
        let out = execute(&json!({"urls": ["https://a.com?token=secretvalue"]}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("Blocked"));
    }

    #[tokio::test]
    async fn missing_urls_is_error() {
        let out = execute(&json!({}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert!(v.get("error").is_some());
    }

    // Plain test thread, not #[tokio::test]: the blocking reqwest client's
    // per-request wait drops a runtime, which panics inside an async context.
    // The registry's dispatch routes execute() through spawn_blocking, so
    // real callers never hit this.
    #[test]
    #[ignore = "network"]
    fn live_fetch() {
        let out = execute(&json!({"urls": ["https://example.com"]}));
        println!("{out}");
    }
}
