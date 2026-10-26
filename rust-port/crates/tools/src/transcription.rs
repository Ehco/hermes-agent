//! Cloud STT provider HTTP contracts — the portable slice of
//! `tools/transcription_cloud.py` + `tools/transcription_common.py`.
//!
//! What ports (the provider HTTP contracts, with every local-audio dependency
//! left behind): the shared result envelopes (`_ok_result`/`_error_result`/
//! `no_speech`), the transcript-text normalizer (`_extract_transcript_text` —
//! string | `{"text": ..}` | `{"error": ..}` | Voxtral `<asr_text>` prefix),
//! the language-hint ladder (`_resolve_stt_language`), the xAI model
//! normalization and `/v1/stt` request contract, the ElevenLabs
//! `/speech-to-text` request contract, and the shared `_rest_provider` flow —
//! all over an injected [`MultipartTransport`], so tests stub the wire.
//!
//! What stays Python-side: the local backends (faster-whisper, local command,
//! streaming), audio transcoding, and the OpenAI/groq/mistral SDK-shaped paths
//! (they speak the SDK, not raw HTTP); the xAI OAuth credential resolution is
//! host-side and only the resolved bearer reaches the transport.

use serde_json::{json, Value};

/// Base URLs (`transcription_common.py`; env overrides resolved by the host).
pub const XAI_STT_BASE_URL: &str = "https://api.x.ai/v1";
pub const ELEVENLABS_STT_BASE_URL: &str = "https://api.elevenlabs.io/v1";
/// `transcription_common.py::DEFAULT_GROQ_STT_MODEL` (env override resolved by
/// the host).
pub const DEFAULT_GROQ_STT_MODEL: &str = "whisper-large-v3-turbo";
/// `transcription_common.py::DEFAULT_STT_MODEL`.
pub const DEFAULT_STT_MODEL: &str = "whisper-1";
/// `transcription_common.py::LEGACY_XAI_STT_MODEL`.
pub const LEGACY_XAI_STT_MODEL: &str = "grok-stt";
/// `transcription_common.py::STT_MODEL_CATALOG["xai"][0]` — xAI moved the
/// server default from 1.0 to 2.0 mid-September 2026, so the model is always
/// named on the wire.
pub const DEFAULT_XAI_STT_MODEL: &str = "grok-voice-transcribe-2.0";

/// `utils.py::TRUTHY_STRINGS` — the shared truthy string set.
const TRUTHY_STRINGS: &[&str] = &["1", "true", "yes", "on"];

/// `utils.py::is_truthy_value` over a config entry (JSON has no bool-string
/// distinction the YAML subset can't carry, so str | bool | null covers it).
fn is_truthy_value(value: Option<&Value>, default: bool) -> bool {
    match value {
        None | Some(Value::Null) => default,
        Some(Value::String(s)) => TRUTHY_STRINGS.contains(&s.trim().to_lowercase().as_str()),
        Some(other) => match other {
            Value::Bool(b) => *b,
            Value::Number(n) => n.as_f64().map_or(false, |f| f != 0.0),
            Value::Array(a) => !a.is_empty(),
            Value::Object(o) => !o.is_empty(),
            _ => false,
        },
    }
}

// ── result envelopes (`transcription_common.py`) ────────────────────────────

/// `_ok_result` — `{"success": True, "transcript": .., "provider": ..}`.
pub fn stt_ok_result(transcript: &str, provider: &str) -> Value {
    json!({ "success": true, "transcript": transcript, "provider": provider })
}

/// `_error_result` — `{"success": False, "transcript": "", "error": ..}`, with
/// the optional `no_speech` rider (silence is non-fatal in the voice loop).
pub fn stt_error_result(error: &str, no_speech: bool) -> Value {
    let mut out = json!({ "success": false, "transcript": "", "error": error });
    if no_speech {
        out.as_object_mut().unwrap().insert("no_speech".into(), Value::Bool(true));
    }
    out
}

/// `transcription_common.py::STTResponseError` — a provider answered with a
/// structured object carrying no usable transcript; the message is the
/// provider's own so failure paths surface it verbatim (#78098).
pub const STT_RESPONSE_ERROR: &str = "Transcription response contained no text";

/// Voxtral-style `language xx <asr_text> ...` prefix some SDK responses carry
/// (`transcription_cloud.py::_ASR_TEXT_RE`, verbatim — DOTALL + IGNORECASE,
/// anchored like `re.match`).
fn strip_asr_text_prefix(text: &str) -> String {
    use std::sync::OnceLock;
    static ASR_TEXT_RE: OnceLock<regex::Regex> = OnceLock::new();
    let re = ASR_TEXT_RE.get_or_init(|| {
        regex::Regex::new(r"(?is)^\s*language\s+[\w.\-]+(?:\s*<audio_language>[^<]*</audio_language>)?\s*<asr_text>\s*(.*)$")
            .expect("ASR_TEXT_RE")
    });
    match re.captures(text) {
        Some(caps) => caps.get(1).map(|m| m.as_str().trim().to_string()).unwrap_or_default(),
        None => text.trim().to_string(),
    }
}

/// `_extract_transcript_text` — normalize a text / structured transcription
/// response to a plain string. A *structured* response whose `text` is missing
/// or non-string must never be stringified (that rendered the object repr as
/// the user's message, #78098): it raises its provider `error` instead. A
/// structured response carrying neither text nor error is an error in its own
/// right. Unrecognized scalars keep the legacy stringification.
pub fn extract_transcript_text(response: &Value) -> Result<String, String> {
    let text = match response {
        Value::String(s) => s.trim().to_string(),
        Value::Object(obj) => {
            let has_text_key = obj.contains_key("text");
            match obj.get("text") {
                Some(Value::String(s)) => s.trim().to_string(),
                Some(_) | None if has_text_key || obj.contains_key("error") => {
                    let error = obj.get("error").and_then(Value::as_str).unwrap_or("");
                    if error.is_empty() {
                        return Err(STT_RESPONSE_ERROR.to_string());
                    }
                    return Err(error.to_string());
                }
                _ => return Err(STT_RESPONSE_ERROR.to_string()),
            }
        }
        other => other.to_string().trim().to_string(),
    };
    Ok(strip_asr_text_prefix(&text))
}

// ── language hint ladder (`transcription_tools.py::_resolve_stt_language`) ──

/// Language hint for an STT provider, first non-empty wins (never `""`):
/// `stt.<provider>.language` (plus `extra_keys` aliases, e.g. `language_code`)
/// > `stt.language` > the `HERMES_LOCAL_STT_LANGUAGE` env value (passed in —
/// env stays host-side) > `None` (provider auto-detects).
pub fn resolve_stt_language(
    provider_key: &str,
    stt_config: &Value,
    extra_keys: &[&str],
    env_language: Option<&str>,
) -> Option<String> {
    let section = |key: &str| stt_config.get(key).cloned().unwrap_or(Value::Null);
    // `_get_stt_section` coalesces a null section to {}.
    let provider_cfg = match section(provider_key) {
        Value::Null => Value::Object(Default::default()),
        other => other,
    };
    let mut candidates: Vec<Option<&str>> = Vec::new();
    for key in ["language"].iter().chain(extra_keys.iter()) {
        candidates.push(provider_cfg.get(*key).and_then(Value::as_str));
    }
    candidates.push(stt_config.get("language").and_then(Value::as_str));
    candidates.push(env_language);
    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|c| !c.is_empty())
        .map(str::to_string)
}

// ── xAI `/v1/stt` contract (`transcription_cloud.py::_transcribe_xai`) ──────

/// `normalize_xai_stt_model` — blank or the old `grok-stt` alias → the
/// `STT_XAI_MODEL` env value (passed in; read per call host-side), else the
/// catalog default.
pub fn normalize_xai_stt_model(model: &str, env_model: Option<&str>) -> String {
    let value = model.trim();
    if value.is_empty() || value == LEGACY_XAI_STT_MODEL {
        return env_model
            .map(str::trim)
            .filter(|m| !m.is_empty())
            .unwrap_or(DEFAULT_XAI_STT_MODEL)
            .to_string();
    }
    value.to_string()
}

/// The `data` fields of the xAI STT multipart POST: the model always named,
/// `language` only when resolved, `format` (inverse text normalization) only
/// with a language — /v1/stt answers HTTP 400 "Field 'language' is required
/// when 'format' is true" otherwise — and `diarize` defaulting off.
pub fn xai_stt_request_data(model: &str, language: Option<&str>, xai_config: &Value) -> Vec<(String, String)> {
    let get = |key: &str| xai_config.get(key);
    let mut data = vec![("model".to_string(), normalize_xai_stt_model(model, None))];
    if let Some(language) = language.map(str::trim).filter(|l| !l.is_empty()) {
        data.push(("language".to_string(), language.to_string()));
        if is_truthy_value(get("format"), true) {
            data.push(("format".to_string(), "true".to_string()));
        }
    }
    if is_truthy_value(get("diarize"), false) {
        data.push(("diarize".to_string(), "true".to_string()));
    }
    data
}

// ── ElevenLabs `/speech-to-text` contract (`_transcribe_elevenlabs`) ────────

/// The ElevenLabs Scribe multipart POST fields (ElevenLabs historically uses
/// `model_id` instead of `model`); bools go out lowercase-stringified.
pub fn elevenlabs_stt_request_data(
    model_name: &str,
    language_code: Option<&str>,
    elevenlabs_config: &Value,
) -> Vec<(String, String)> {
    let get = |key: &str| elevenlabs_config.get(key);
    let mut data = vec![
        ("model_id".to_string(), model_name.to_string()),
        (
            "tag_audio_events".to_string(),
            is_truthy_value(get("tag_audio_events"), false).to_string(),
        ),
        (
            "diarize".to_string(),
            is_truthy_value(get("diarize"), false).to_string(),
        ),
    ];
    if let Some(code) = language_code.map(str::trim).filter(|c| !c.is_empty()) {
        data.push(("language_code".to_string(), code.to_string()));
    }
    data
}

/// `_elevenlabs_error_detail` — the provider's own message out of
/// `detail`/`error`, stringified when the value is a dict.
pub fn elevenlabs_error_detail(err_body: &Value) -> String {
    let error_value = err_body
        .get("detail")
        .filter(|v| !v.is_null())
        .or_else(|| err_body.get("error"));
    match error_value {
        Some(Value::Object(obj)) => match obj.get("message").and_then(Value::as_str) {
            Some(message) if !message.is_empty() => message.to_string(),
            _ => Value::Object(obj.clone()).to_string(),
        },
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(other) => other.to_string(),
    }
}

// ── the shared `_rest_provider` flow ────────────────────────────────────────

/// One canned HTTP response as the stubbed transport hands it back.
#[derive(Debug, Clone)]
pub struct RestResponse {
    pub status: u16,
    /// Parsed JSON body when the response is JSON, else `Null`.
    pub body: Value,
    /// Raw body text (the JSON-detail fallback reads its first 300 chars).
    pub text: String,
}

/// The injected seam: one multipart POST of the audio file to *url* with
/// *headers* and the provider's *fields*. `Err` = transport failure (the
/// `requests` exception in the Python).
pub trait MultipartTransport {
    fn post_multipart(
        &self,
        url: &str,
        headers: &[(String, String)],
        file_name: &str,
        file_bytes: Vec<u8>,
        fields: &[(String, String)],
    ) -> Result<RestResponse, String>;
}

/// `_cloud_failure` — map a transport exception to the shared envelope
/// (`label` e.g. `"xAI STT"`).
fn cloud_failure(exc: &str, label: &str) -> Value {
    stt_error_result(&format!("{label} transcription failed: {exc}"), false)
}

/// `_rest_provider` — POST -> on non-200 `"<label> API error (HTTP n):
/// <detail>"` (JSON detail via `extract_detail`, else the first 300 body
/// chars); on 200 `extract_text(body)`: empty → the `no_speech` envelope
/// (silence is non-fatal), a provider `error` surfaced verbatim, otherwise the
/// ok envelope. Transport failures → `_cloud_failure`.
pub fn rest_provider<D, T>(
    transport_result: Result<RestResponse, String>,
    label: &str,
    provider: &str,
    extract_detail: D,
    extract_text: T,
) -> Value
where
    D: Fn(&Value) -> String,
    T: Fn(&Value) -> Result<String, String>,
{
    let response = match transport_result {
        Ok(resp) => resp,
        Err(exc) => return cloud_failure(&exc, label),
    };
    if response.status != 200 {
        let fallback_detail = || response.text.chars().take(300).collect::<String>();
        let detail = extract_detail(&response.body);
        let detail = if detail.is_empty() { fallback_detail() } else { detail };
        return stt_error_result(
            &format!("{label} API error (HTTP {}): {detail}", response.status),
            false,
        );
    }
    match extract_text(&response.body) {
        Err(provider_error) => stt_error_result(&provider_error, false),
        Ok(text) if text.is_empty() => {
            stt_error_result(&format!("{label} returned empty transcript"), true)
        }
        Ok(text) => stt_ok_result(&text, provider),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn envelopes_match_the_python_shapes() {
        assert_eq!(
            stt_ok_result("hello", "xai"),
            json!({ "success": true, "transcript": "hello", "provider": "xai" })
        );
        assert_eq!(
            stt_error_result("boom", false),
            json!({ "success": false, "transcript": "", "error": "boom" })
        );
        // no_speech is an extra rider (silence is non-fatal).
        let err = stt_error_result("silence", true);
        assert_eq!(err["no_speech"], true);
        assert_eq!(stt_error_result("boom", false).get("no_speech"), None);
    }

    #[test]
    fn transcript_normalizer_handles_text_error_and_scalars() {
        // Plain string trims.
        assert_eq!(extract_transcript_text(&json!("  hi  ")).unwrap(), "hi");
        // `{"text": ...}` is the transcript…
        assert_eq!(extract_transcript_text(&json!({ "text": "hi" })).unwrap(), "hi");
        // …but a structured response with a non-string text carries its error.
        assert_eq!(
            extract_transcript_text(&json!({ "text": null, "error": "bad file" })).unwrap_err(),
            "bad file"
        );
        // Neither text nor error is an error in its own right (#78098: the
        // object repr must never become the transcript).
        assert_eq!(
            extract_transcript_text(&json!({ "logprobs": null })).unwrap_err(),
            STT_RESPONSE_ERROR
        );
        // Unrecognized scalars keep the legacy stringification.
        assert_eq!(extract_transcript_text(&json!(42)).unwrap(), "42");
    }

    #[test]
    fn transcript_normalizer_strips_voxtral_asr_prefix() {
        let with_prefix = "language en <asr_text> hello there ";
        assert_eq!(
            extract_transcript_text(&json!(with_prefix)).unwrap(),
            "hello there"
        );
        // With the optional audio-language tag (properly closed).
        let tagged = " language en <audio_language>EN</audio_language> <asr_text>bonjour";
        assert_eq!(extract_transcript_text(&json!(tagged)).unwrap(), "bonjour");
        // Plain text containing the word "language" elsewhere is untouched.
        assert_eq!(
            extract_transcript_text(&json!("the language of things")).unwrap(),
            "the language of things"
        );
    }

    #[test]
    fn language_ladder_first_non_empty_wins() {
        let cfg = json!({
            "xai": { "language": "en" },
            "language": "fr",
        });
        // stt.<provider>.language > stt.language > env > None.
        assert_eq!(
            resolve_stt_language("xai", &cfg, &[], Some("de")).as_deref(),
            Some("en")
        );
        assert_eq!(
            resolve_stt_language("mistral", &cfg, &[], Some("de")).as_deref(),
            Some("fr")
        );
        assert_eq!(
            resolve_stt_language("openai", &json!({}), &[], Some("de")).as_deref(),
            Some("de")
        );
        assert_eq!(resolve_stt_language("openai", &json!({}), &[], None), None);
        // extra_keys aliases (ElevenLabs `language_code`) and blank-skip.
        let el = json!({ "elevenlabs": { "language_code": "  " }, "language": "en" });
        assert_eq!(
            resolve_stt_language("elevenlabs", &el, &["language_code"], None).as_deref(),
            Some("en")
        );
        let el = json!({ "elevenlabs": { "language_code": "it" } });
        assert_eq!(
            resolve_stt_language("elevenlabs", &el, &["language_code"], Some("en")).as_deref(),
            Some("it")
        );
    }

    #[test]
    fn xai_model_normalization_names_the_model() {
        // Blank and the legacy alias fall to the env pin, else the catalog
        // default (xAI moved the server default to 2.0 mid-September 2026).
        assert_eq!(normalize_xai_stt_model("", Some(" grok-stt-1 ")), "grok-stt-1");
        assert_eq!(normalize_xai_stt_model("grok-stt", None), DEFAULT_XAI_STT_MODEL);
        assert_eq!(
            normalize_xai_stt_model("  ", None),
            "grok-voice-transcribe-2.0"
        );
        // An explicit model passes through verbatim (no env override).
        assert_eq!(
            normalize_xai_stt_model("grok-voice-transcribe-1.0", Some("other")),
            "grok-voice-transcribe-1.0"
        );
    }

    #[test]
    fn xai_request_data_gates_format_on_language() {
        let cfg = json!({});
        let data = xai_stt_request_data("grok-stt", Some("en"), &cfg);
        // Model always named; language set; format defaults on WITH language;
        // diarize defaults off.
        assert_eq!(
            data,
            vec![
                ("model".to_string(), "grok-voice-transcribe-2.0".to_string()),
                ("language".to_string(), "en".to_string()),
                ("format".to_string(), "true".to_string()),
            ]
        );
        // Auto-detect ("") drops the language and, with it, `format` —
        // /v1/stt 400s "Field 'language' is required when 'format' is true".
        let data = xai_stt_request_data("m", Some(""), &cfg);
        assert_eq!(
            data,
            vec![("model".to_string(), "m".to_string())]
        );
        // diarize=true in config is stringified onto the wire.
        let cfg = json!({ "diarize": true });
        let data = xai_stt_request_data("m", Some("en"), &cfg);
        assert!(data.contains(&("diarize".to_string(), "true".to_string())));
        // diarize=false (explicit) and a false-y string both stay off.
        for value in [json!({ "diarize": false }), json!({ "diarize": "off" })] {
            let data = xai_stt_request_data("m", Some("en"), &value);
            assert!(!data.iter().any(|(k, _)| k == "diarize"));
        }
    }

    #[test]
    fn elevenlabs_request_data_and_error_detail() {
        let cfg = json!({ "tag_audio_events": "yes", "diarize": "no" });
        let data = elevenlabs_stt_request_data("scribe_v2", Some("en"), &cfg);
        assert_eq!(
            data,
            vec![
                ("model_id".to_string(), "scribe_v2".to_string()),
                ("tag_audio_events".to_string(), "true".to_string()),
                ("diarize".to_string(), "false".to_string()),
                ("language_code".to_string(), "en".to_string()),
            ]
        );
        // Blank language code is omitted (auto-detect).
        let data = elevenlabs_stt_request_data("scribe_v2", Some("  "), &json!({}));
        assert!(!data.iter().any(|(k, _)| k == "language_code"));

        // Error detail: nested dict -> its message; string -> itself; missing -> "".
        assert_eq!(
            elevenlabs_error_detail(&json!({ "detail": { "message": "bad key", "status": 401 } })),
            "bad key"
        );
        assert_eq!(elevenlabs_error_detail(&json!({ "error": "rate limited" })), "rate limited");
        assert_eq!(elevenlabs_error_detail(&json!({})), "");
        // A dict without a message stringifies (never an empty detail).
        assert_eq!(
            elevenlabs_error_detail(&json!({ "detail": { "status": 401 } })),
            r#"{"status":401}"#
        );
    }

    #[test]
    fn rest_provider_maps_the_full_flow() {
        let ok = |body: Value| {
            let text = body.to_string();
            Ok(RestResponse { status: 200, text, body })
        };
        let text_of = |body: &Value| {
            body.get("text")
                .and_then(Value::as_str)
                .map(str::trim)
                .map(str::to_string)
                .ok_or_else(|| "bad".to_string())
        };
        // 200 + text -> ok envelope.
        let out = rest_provider(
            ok(json!({ "text": " hi " })),
            "xAI STT",
            "xai",
            |_| String::new(),
            text_of,
        );
        assert_eq!(out["success"], true);
        assert_eq!(out["transcript"], "hi");
        assert_eq!(out["provider"], "xai");

        // 200 + empty text -> no_speech (silence is non-fatal).
        let out = rest_provider(
            ok(json!({ "text": "  " })),
            "xAI STT",
            "xai",
            |_| String::new(),
            text_of,
        );
        assert_eq!(out["success"], false);
        assert_eq!(out["no_speech"], true);
        assert_eq!(out["error"], "xAI STT returned empty transcript");

        // 200 + a structured body whose string `error` surfaces verbatim
        // (ElevenLabs detail-as-string; the strict normalizer raises it).
        let out = rest_provider(
            ok(json!({ "error": "usage cap" })),
            "ElevenLabs STT",
            "elevenlabs",
            |_| String::new(),
            |body| extract_transcript_text(body),
        );
        assert_eq!(out["error"], "usage cap");
        assert_eq!(out.get("no_speech"), None);

        // Non-200 with a JSON detail extracts the provider message…
        let out = rest_provider(
            Ok(RestResponse {
                status: 401,
                body: json!({ "error": { "message": "bad key" } }),
                text: r#"{"error":{"message":"bad key"}}"#.to_string(),
            }),
            "xAI STT",
            "xai",
            |body| body["error"]["message"].as_str().unwrap_or("").to_string(),
            text_of,
        );
        assert_eq!(
            out["error"],
            "xAI STT API error (HTTP 401): bad key"
        );
        // …and falls back to the first 300 chars of the raw body otherwise.
        let long: String = "x".repeat(400);
        let out = rest_provider(
            Ok(RestResponse {
                status: 500,
                body: Value::Null,
                text: long.clone(),
            }),
            "xAI STT",
            "xai",
            |_| String::new(),
            text_of,
        );
        assert_eq!(out["error"], format!("xAI STT API error (HTTP 500): {}", "x".repeat(300)));
        assert!(out["error"].as_str().unwrap().len() < long.len() + 40);

        // Transport failure -> "<label> transcription failed: …".
        let out = rest_provider(
            Err("connection reset".to_string()),
            "xAI STT",
            "xai",
            |_: &_| String::new(),
            text_of,
        );
        assert_eq!(
            out["error"],
            "xAI STT transcription failed: connection reset"
        );
    }
}
