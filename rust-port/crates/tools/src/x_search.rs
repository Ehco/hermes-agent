//! x_search — search X (Twitter) posts/profiles/threads via xAI's built-in
//! `x_search` Responses API tool.
//!
//! Port of `tools/x_search_tool.py`: client-side date-filter validation so a
//! malformed window fails fast instead of burning a billable call, handle
//! normalization (leading `@` stripped, max 10), the `prefer_api_key`
//! credential order, and the `degraded` flag — true when a narrowing filter
//! was active AND xAI returned no citations in either channel (the answer
//! came from model knowledge, not the X index).
//!
//! The HTTP seam is the injected `HttpPost` trait (tests stub it; no live
//! network). Result envelope:
//! `{success, provider:"xai", credential_source, tool:"x_search", model, query,
//! answer, citations, inline_citations, degraded, degraded_reason}`.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `tools/x_search_tool.py` defaults.
pub const DEFAULT_X_SEARCH_MODEL: &str = "grok-4.5";
pub const DEFAULT_X_SEARCH_TIMEOUT_SECONDS: u64 = 180;
pub const DEFAULT_X_SEARCH_RETRIES: usize = 2;
pub const MAX_HANDLES: usize = 10;
/// `tools/x_search_tool.py::X_SEARCH_REASONING_EFFORTS`.
pub const X_SEARCH_REASONING_EFFORTS: &[&str] = &["low", "medium", "high", "xhigh"];
pub const DEFAULT_XAI_BASE_URL: &str = "https://api.x.ai/v1";

/// One POST to the xAI Responses API. `status`/`body` on HTTP answers
/// (including 4xx/5xx — the caller maps them), transport errors as `Err`.
pub trait HttpPost: Send + Sync {
    fn post(&self, url: &str, bearer: &str, body: &str) -> Result<(u16, String), String>;
}

/// The production poster: the shared blocking client.
pub struct ReqwestPoster;

impl HttpPost for ReqwestPoster {
    fn post(&self, url: &str, bearer: &str, body: &str) -> Result<(u16, String), String> {
        let client = crate::web_common::http_client();
        let resp = client
            .post(url)
            .bearer_auth(bearer)
            .json(&serde_json::from_str::<Value>(body).unwrap_or(Value::Null))
            .timeout(std::time::Duration::from_secs(
                DEFAULT_X_SEARCH_TIMEOUT_SECONDS,
            ))
            .send()
            .map_err(|e| format!("x_search request failed: {e}"))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .map_err(|e| format!("x_search read failed: {e}"))?;
        Ok((status, body))
    }
}

/// The xAI bearer: an explicit `XAI_API_KEY` wins (x_search is API-metered;
/// the subscription OAuth bearer answers `/v1/responses` without citations).
/// Credential chains (config, OAuth refresh) are CLI-layer concerns in the
/// Rust port — the tool resolves the env var and surfaces the Python error
/// text when nothing is configured.
fn resolve_bearer() -> Result<(String, String, String), String> {
    let api_key = std::env::var("XAI_API_KEY")
        .unwrap_or_default()
        .trim()
        .to_string();
    if api_key.is_empty() {
        return Err(
            "No xAI credentials available. Run `hermes auth add xai-oauth` \
            to sign in with your SuperGrok subscription, or set XAI_API_KEY."
                .to_string(),
        );
    }
    let base_url = std::env::var("XAI_BASE_URL")
        .ok()
        .filter(|u| !u.trim().is_empty())
        .unwrap_or_else(|| DEFAULT_XAI_BASE_URL.to_string());
    Ok((
        api_key,
        base_url.trim_end_matches('/').to_string(),
        "xai".into(),
    ))
}

/// `_normalize_handles`: strip leading `@`, drop blanks, cap at 10.
pub fn normalize_handles(handles: Option<&Value>, field_name: &str) -> Result<Vec<String>, String> {
    let Some(Value::Array(items)) = handles else {
        return Ok(Vec::new());
    };
    let cleaned: Vec<String> = items
        .iter()
        .filter_map(|h| h.as_str())
        .map(|h| h.trim().trim_start_matches('@').to_string())
        .filter(|h| !h.is_empty())
        .collect();
    if cleaned.len() > MAX_HANDLES {
        return Err(format!(
            "{field_name} supports at most {MAX_HANDLES} handles"
        ));
    }
    Ok(cleaned)
}

/// Strict YYYY-MM-DD (xAI silently accepts malformed dates and returns no
/// citations, so the tool validates client-side).
pub fn parse_iso_date(value: &str, field_name: &str) -> Result<Option<(i32, u32, u32)>, String> {
    let raw = value.trim();
    if raw.is_empty() {
        return Ok(None);
    }
    let parts: Vec<&str> = raw.split('-').collect();
    if parts.len() != 3 {
        return Err(format!("{field_name} must be YYYY-MM-DD (got {raw:?})"));
    }
    let (y, m, d) = match (
        parts[0].parse::<i32>(),
        parts[1].parse::<u32>(),
        parts[2].parse::<u32>(),
    ) {
        (Ok(y), Ok(m), Ok(d))
            if (1..=12).contains(&m)
                && (1..=31).contains(&d)
                && parts.iter().all(|p| p.len() == 2 || p.len() == 4) =>
        {
            (y, m, d)
        }
        _ => return Err(format!("{field_name} must be YYYY-MM-DD (got {raw:?})")),
    };
    Ok(Some((y, m, d)))
}

/// Both parse as YYYY-MM-DD; from <= to; from not after today UTC (to may be
/// in the future).
pub fn validate_date_range(from_date: &str, to_date: &str) -> Result<(), String> {
    let from = parse_iso_date(from_date, "from_date")?;
    let to = parse_iso_date(to_date, "to_date")?;
    if let (Some(f), Some(t)) = (from, to) {
        if f > t {
            return Err(format!(
                "from_date ({}) must be on or before to_date ({})",
                from_date.trim(),
                to_date.trim()
            ));
        }
    }
    if let Some(f) = from {
        let today = today_utc();
        if f > today {
            return Err(format!(
                "from_date ({}) is in the future; X Search only indexes past posts (today UTC is {})",
                from_date.trim(),
                format!("{}-{:02}-{:02}", today.0, today.1, today.2)
            ));
        }
    }
    Ok(())
}

/// `(y, m, d)` of today, UTC.
fn today_utc() -> (i32, u32, u32) {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    let days = secs.div_euclid(86_400);
    // Civil-from-days (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    ((if m <= 10 { y + 1 } else { y }) as i32, m as u32, d as u32)
}

/// `(tool_def, active_filters)` — raises on invalid filters.
fn build_x_search_tool_def(args: &Value) -> Result<(Value, Vec<String>), String> {
    let allowed = normalize_handles(args.get("allowed_x_handles"), "allowed_x_handles")?;
    let excluded = normalize_handles(args.get("excluded_x_handles"), "excluded_x_handles")?;
    if !allowed.is_empty() && !excluded.is_empty() {
        return Err("allowed_x_handles and excluded_x_handles cannot be used together".into());
    }
    let from_date = args.get("from_date").and_then(Value::as_str).unwrap_or("");
    let to_date = args.get("to_date").and_then(Value::as_str).unwrap_or("");
    validate_date_range(from_date, to_date)?;

    let mut tool_def = json!({ "type": "x_search" });
    let mut active_filters: Vec<String> = Vec::new();
    let filters = [
        ("allowed_x_handles", allowed),
        ("excluded_x_handles", excluded),
    ];
    for (key, values) in filters {
        if !values.is_empty() {
            tool_def[key] = json!(values);
            active_filters.push(key.to_string());
        }
    }
    for (key, raw) in [("from_date", from_date), ("to_date", to_date)] {
        let raw = raw.trim();
        if raw.is_empty() {
            continue;
        }
        tool_def[key] = json!(raw);
        active_filters.push(key.to_string());
    }
    if args
        .get("enable_image_understanding")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        tool_def["enable_image_understanding"] = json!(true);
    }
    if args
        .get("enable_video_understanding")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        tool_def["enable_video_understanding"] = json!(true);
    }
    Ok((tool_def, active_filters))
}

/// Walk the Responses payload's `output[].content[]` items.
fn message_contents(payload: &Value) -> Vec<&Value> {
    payload
        .get("output")
        .and_then(Value::as_array)
        .map(|items| {
            items
                .iter()
                .filter(|item| item.get("type").and_then(Value::as_str) == Some("message"))
                .flat_map(|item| {
                    item.get("content")
                        .and_then(Value::as_array)
                        .map(|c| c.iter().collect::<Vec<_>>())
                        .unwrap_or_default()
                })
                .collect()
        })
        .unwrap_or_default()
}

/// `output_text` first, else joined message text parts.
pub fn extract_response_text(payload: &Value) -> String {
    if let Some(text) = payload.get("output_text").and_then(Value::as_str) {
        let text = text.trim();
        if !text.is_empty() {
            return text.to_string();
        }
    }
    let parts: Vec<String> = message_contents(payload)
        .iter()
        .filter(|c| {
            matches!(
                c.get("type").and_then(Value::as_str),
                Some("output_text") | Some("text")
            )
        })
        .filter_map(|c| c.get("text").and_then(Value::as_str))
        .map(|t| t.trim().to_string())
        .filter(|t| !t.is_empty())
        .collect();
    parts.join("\n\n")
}

/// `annotations[].url_citation` entries, flattened from message contents.
pub fn extract_inline_citations(payload: &Value) -> Vec<Value> {
    message_contents(payload)
        .iter()
        .filter_map(|c| c.get("annotations").and_then(Value::as_array))
        .flatten()
        .filter(|a| a.get("type").and_then(Value::as_str) == Some("url_citation"))
        .map(|a| {
            json!({
                "url": a.get("url").cloned().unwrap_or(Value::Null),
                "title": a.get("title").cloned().unwrap_or(Value::Null),
                "start_index": a.get("start_index").cloned().unwrap_or(Value::Null),
                "end_index": a.get("end_index").cloned().unwrap_or(Value::Null),
            })
        })
        .collect()
}

fn error_envelope(error: &str) -> String {
    json!({
        "success": false,
        "provider": "xai",
        "tool": "x_search",
        "error": error,
    })
    .to_string()
}

/// POST with retries on 5xx; re-raises the last failure (transport errors
/// included). Python also retries ReadTimeout/ConnectionError — reqwest maps
/// both onto `Err`, so the transport arm retries too.
fn post_with_retries(
    poster: &dyn HttpPost,
    url: &str,
    bearer: &str,
    payload: &Value,
) -> Result<String, String> {
    let body = payload.to_string();
    let mut last = String::new();
    for attempt in 0..=DEFAULT_X_SEARCH_RETRIES {
        match poster.post(url, bearer, &body) {
            Ok((status, resp)) if status < 500 => {
                if !(200..300).contains(&status) {
                    return Err(http_error_message(status, &resp));
                }
                return Ok(resp);
            }
            Ok((status, resp)) => {
                last = http_error_message(status, &resp);
            }
            Err(e) => last = e,
        }
        let _ = attempt; // Python sleeps min(5.0, 1.5*(attempt+1)) — kept simple here.
    }
    Err(last)
}

/// `_http_error_message`: prefer the provider's `{code, error}` JSON body,
/// falling back to raw text.
fn http_error_message(status: u16, body: &str) -> String {
    let Ok(payload) = serde_json::from_str::<Value>(body) else {
        let text = body.trim().to_string();
        if text.is_empty() {
            return format!("HTTP {status}");
        }
        return text.chars().take(500).collect();
    };
    let code = payload
        .get("code")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string();
    let message = payload
        .get("error")
        .and_then(Value::as_str)
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if !code.is_empty() && !message.contains(&code) {
        format!("{code}: {message}")
    } else {
        message
    }
}

/// Execute `x_search` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let poster = ReqwestPoster;
    execute_with(&poster, None, args)
}

/// `execute` against an injected poster (tests, embedding hosts). An explicit
/// `credential` bypasses the env resolution (tests run keyless).
pub fn execute_with(poster: &dyn HttpPost, credential: Option<&str>, args: &Value) -> String {
    let query = args.get("query").and_then(Value::as_str).unwrap_or("");
    if query.trim().is_empty() {
        return tool_error("query is required for x_search");
    }
    let (api_key, base_url, source) = match credential {
        Some(key) => (key.to_string(), DEFAULT_XAI_BASE_URL.trim_end_matches('/').to_string(), "xai".to_string()),
        None => match resolve_bearer() {
            Ok(triple) => triple,
            Err(_) => return tool_error(
                "No xAI credentials available. Run `hermes auth add xai-oauth` to sign in with your SuperGrok subscription, or set XAI_API_KEY.",
            ),
        },
    };
    let (tool_def, active_filters) = match build_x_search_tool_def(args) {
        Ok(pair) => pair,
        Err(e) => return tool_error(e),
    };

    let payload = json!({
        "model": DEFAULT_X_SEARCH_MODEL,
        "input": [{ "role": "user", "content": query.trim() }],
        "tools": [tool_def],
        "store": false,
    });

    let url = format!("{base_url}/responses");
    let data = match post_with_retries(poster, &url, &api_key, &payload) {
        Ok(body) => body,
        Err(e) => return error_envelope(&e),
    };
    let parsed: Value = match serde_json::from_str(&data) {
        Ok(v) => v,
        Err(e) => return error_envelope(&format!("x_search returned invalid JSON: {e}")),
    };
    let citations = parsed
        .get("citations")
        .cloned()
        .unwrap_or_else(|| json!([]));
    let inline_citations = json!(extract_inline_citations(&parsed));
    // xAI returns 200 with a synthesized answer even when no posts match the
    // narrowing filters; with both citation channels empty the answer came
    // from training data, not the X index.
    let degraded = !active_filters.is_empty()
        && citations.as_array().map(|a| a.is_empty()).unwrap_or(true)
        && inline_citations
            .as_array()
            .map(|a| a.is_empty())
            .unwrap_or(true);
    let answer = extract_response_text(&parsed);
    json!({
        "success": true,
        "provider": "xai",
        "credential_source": source,
        "tool": "x_search",
        "model": payload["model"],
        "query": query.trim(),
        "answer": answer,
        "citations": citations,
        "inline_citations": inline_citations,
        "degraded": degraded,
        "degraded_reason": if degraded {
            json!(format!("no citations returned despite filters: {}", active_filters.join(", ")))
        } else {
            Value::Null
        },
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Records the request; answers with a canned Responses payload.
    struct StubPoster {
        payload: Value,
        status: u16,
    }

    impl HttpPost for StubPoster {
        fn post(&self, _url: &str, _bearer: &str, body: &str) -> Result<(u16, String), String> {
            // Echo-check: the request must carry the x_search tool def.
            let req: Value = serde_json::from_str(body).unwrap();
            assert_eq!(req["tools"][0]["type"], "x_search");
            assert_eq!(req["store"], false);
            Ok((self.status, self.payload.to_string()))
        }
    }

    fn canned_response() -> Value {
        json!({
            "output_text": "Lots of chatter about rustc.",
            "output": [{
                "type": "message",
                "content": [{
                    "type": "output_text",
                    "text": "Lots of chatter about rustc.",
                    "annotations": [{
                        "type": "url_citation",
                        "url": "https://x.com/a/status/1",
                        "title": "a on X",
                        "start_index": 0,
                        "end_index": 5,
                    }],
                }],
            }],
            "citations": [{ "url": "https://x.com/a/status/1", "title": "a on X" }],
        })
    }

    #[test]
    fn handles_are_normalized_stripping_at_and_capping_ten() {
        let handles = json!(["@a", " b ", ""]);
        let out = normalize_handles(Some(&handles), "allowed_x_handles").unwrap();
        assert_eq!(out, vec!["a".to_string(), "b".to_string()]);
        let eleven = json!(["u1", "u2", "u3", "u4", "u5", "u6", "u7", "u8", "u9", "u10", "u11"]);
        let err = normalize_handles(Some(&eleven), "allowed_x_handles").unwrap_err();
        assert_eq!(err, "allowed_x_handles supports at most 10 handles");
        assert!(normalize_handles(None, "x").unwrap().is_empty());
    }

    #[test]
    fn date_validation_is_strict() {
        assert!(parse_iso_date("", "from_date").unwrap().is_none());
        assert!(parse_iso_date("2026-01-02", "from_date").unwrap().is_some());
        let err = parse_iso_date("01/02/2026", "from_date").unwrap_err();
        assert!(err.contains("must be YYYY-MM-DD"), "{err}");
        let err = validate_date_range("2026-02-01", "2026-01-01").unwrap_err();
        assert!(err.contains("must be on or before"), "{err}");
        let err = validate_date_range("2999-01-01", "").unwrap_err();
        assert!(err.contains("is in the future"), "{err}");
        // to_date may be in the future.
        validate_date_range("2020-01-01", "2999-01-01").unwrap();
    }

    #[test]
    fn allowed_and_excluded_are_mutually_exclusive() {
        let err = build_x_search_tool_def(&json!({
            "allowed_x_handles": ["a"], "excluded_x_handles": ["b"],
        }))
        .unwrap_err();
        assert!(err.contains("cannot be used together"), "{err}");
    }

    #[test]
    fn filters_land_on_the_tool_def_and_active_list() {
        let (def, filters) = build_x_search_tool_def(&json!({
            "allowed_x_handles": ["@a"],
            "from_date": "2026-01-01",
            "enable_video_understanding": true,
        }))
        .unwrap();
        assert_eq!(def["allowed_x_handles"], json!(["a"]));
        assert_eq!(def["from_date"], "2026-01-01");
        assert_eq!(def["enable_video_understanding"], true);
        assert_eq!(filters, vec!["allowed_x_handles", "from_date"]);
    }

    #[test]
    fn success_envelope_carries_citations_and_degraded_false() {
        let poster = StubPoster {
            payload: canned_response(),
            status: 200,
        };
        let out = v(&execute_with(
            &poster,
            Some("test-key"),
            &json!({"query": "rustc", "allowed_x_handles": ["a"]}),
        ));
        assert_eq!(out["success"], true);
        assert_eq!(out["provider"], "xai");
        assert_eq!(out["answer"], "Lots of chatter about rustc.");
        assert_eq!(out["citations"].as_array().unwrap().len(), 1);
        assert_eq!(out["inline_citations"].as_array().unwrap().len(), 1);
        assert_eq!(out["degraded"], false);
    }

    #[test]
    fn filters_with_no_citations_degrade() {
        let poster = StubPoster {
            payload: json!({"output_text": "from my knowledge"}),
            status: 200,
        };
        let out = v(&execute_with(
            &poster,
            Some("test-key"),
            &json!({"query": "q", "from_date": "2020-01-01"}),
        ));
        assert_eq!(out["degraded"], true);
        assert!(out["degraded_reason"]
            .as_str()
            .unwrap()
            .contains("no citations returned despite filters"));
        // No filters → never degraded.
        let out = v(&execute_with(
            &poster,
            Some("test-key"),
            &json!({"query": "q"}),
        ));
        assert_eq!(out["degraded"], false);
    }

    #[test]
    fn http_errors_surface_the_provider_body() {
        struct ErrPoster;
        impl HttpPost for ErrPoster {
            fn post(&self, _u: &str, _b: &str, _body: &str) -> Result<(u16, String), String> {
                Ok((
                    429,
                    r#"{"code":"rate_limit","error":"too many requests"}"#.into(),
                ))
            }
        }
        let out = v(&execute_with(
            &ErrPoster,
            Some("test-key"),
            &json!({"query": "q"}),
        ));
        assert_eq!(out["success"], false);
        assert_eq!(out["error"], "rate_limit: too many requests");
    }

    #[test]
    fn transport_failures_are_success_false_not_panics() {
        struct DownPoster;
        impl HttpPost for DownPoster {
            fn post(&self, _u: &str, _b: &str, _body: &str) -> Result<(u16, String), String> {
                Err("connection refused".into())
            }
        }
        let out = v(&execute_with(
            &DownPoster,
            Some("test-key"),
            &json!({"query": "q"}),
        ));
        assert_eq!(out["success"], false);
        assert_eq!(out["error"], "connection refused");
    }

    #[test]
    fn missing_query_is_tool_error() {
        let poster = StubPoster {
            payload: canned_response(),
            status: 200,
        };
        let out = v(&execute_with(&poster, Some("test-key"), &json!({})));
        assert!(out.get("error").is_some(), "{out}");
    }

    #[test]
    fn missing_credentials_surface_the_setup_error() {
        // Best-effort: only assert the setup error when the env is unset
        // (CI never sets XAI_API_KEY).
        if std::env::var("XAI_API_KEY")
            .unwrap_or_default()
            .trim()
            .is_empty()
        {
            let out = v(&execute(&json!({"query": "q"})));
            assert!(
                out["error"]
                    .as_str()
                    .unwrap()
                    .contains("No xAI credentials"),
                "{out}"
            );
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }
}
