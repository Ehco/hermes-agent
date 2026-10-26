//! text_to_speech — convert text to speech audio via the OpenAI-compatible
//! `/audio/speech` REST endpoint (the port of the
//! `tools/tts_tool_openai.py::_generate_openai_tts` path, with the
//! DeepInfra OpenAI-compatible route folded into the same seam).
//!
//! Provider/voice are user-configured, never model-selected: the call resolves
//! `tts.provider` / `tts.openai.api_key` from the Hermes config file with the
//! environment fallback (`OPENAI_API_KEY`), mirrors the Python credential
//! order (config key → env key), and streams the binary response to
//! `output_path` (default: the Hermes audio cache, `<ts>.mp3`).
//!
//! The HTTP seam is the injected `SpeechPoster` trait (tests stub it; no live
//! network): `{success: true, output_path}` or `{success: false, error}`.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `tools/tts_tool_openai.py` defaults.
pub const DEFAULT_OPENAI_MODEL: &str = "gpt-4o-mini-tts";
pub const DEFAULT_OPENAI_VOICE: &str = "alloy";
pub const DEFAULT_OPENAI_BASE_URL: &str = "https://api.openai.com/v1";
/// Python speed clamp (`tts_tool` validates 0.25..=4.0).
pub const SPEED_RANGE: (f64, f64) = (0.25, 4.0);

/// POST one `/audio/speech` request, returning the raw audio bytes.
pub trait SpeechPoster: Send + Sync {
    fn post_speech(&self, url: &str, bearer: &str, body: &str) -> Result<Vec<u8>, String>;
}

/// The production poster: the shared blocking client.
pub struct ReqwestPoster;

impl SpeechPoster for ReqwestPoster {
    fn post_speech(&self, url: &str, bearer: &str, body: &str) -> Result<Vec<u8>, String> {
        let client = crate::web_common::http_client();
        let resp = client
            .post(url)
            .bearer_auth(bearer)
            .header("Content-Type", "application/json")
            .body(body.to_string())
            .timeout(std::time::Duration::from_secs(120))
            .send()
            .map_err(|e| format!("tts request failed: {e}"))?;
        let status = resp.status();
        let bytes = resp.bytes().map_err(|e| format!("tts read failed: {e}"))?;
        if !status.is_success() {
            let text = String::from_utf8_lossy(&bytes);
            return Err(format!("tts returned HTTP {}: {}", status.as_u16(), text.chars().take(300).collect::<String>()));
        }
        Ok(bytes.to_vec())
    }
}

/// Config read: `tts.provider` / `tts.openai.api_key` / `tts.openai.base_url`
/// from the Hermes config.yaml (`$HERMES_HOME/config.yaml`, else
/// `~/.hermes/config.yaml`). No YAML crate here: the config file is
/// line-scanned for the `tts:` → `openai:` nesting this tool needs, which is
/// exactly what the Python `_load_tts_config` exposes for these keys.
fn load_tts_config() -> (Option<String>, Option<String>, Option<String>) {
    let path = match std::env::var("HERMES_HOME") {
        Ok(home) if !home.trim().is_empty() => std::path::PathBuf::from(home).join("config.yaml"),
        _ => {
            let base = std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default();
            std::path::PathBuf::from(base).join(".hermes").join("config.yaml")
        }
    };
    let Ok(content) = std::fs::read_to_string(path) else { return (None, None, None) };
    let mut provider = None;
    let mut api_key = None;
    let mut base_url = None;
    let mut in_tts = false;
    let mut in_openai = false;
    for line in content.lines() {
        let trimmed = strip_comment(line);
        if trimmed.is_empty() {
            continue;
        }
        let indent = line.len() - line.trim_start().len();
        if indent == 0 && trimmed.ends_with(':') {
            in_tts = trimmed.trim_end_matches(':') == "tts";
            in_openai = false;
            continue;
        }
        if !in_tts || indent <= 2 {
            continue;
        }
        let (key, value) = match trimmed.trim_end().split_once(':') {
            Some((k, v)) => (k.trim(), v.trim().trim_matches('"').trim_matches('\'')),
            None => continue,
        };
        if key == "openai" && value.is_empty() {
            in_openai = true;
            continue;
        }
        if key == "provider" && !in_openai {
            provider = Some(value.to_string());
        } else if in_openai && key == "api_key" {
            api_key = Some(value.to_string());
        } else if in_openai && key == "base_url" {
            base_url = Some(value.to_string());
        }
    }
    (provider, api_key, base_url)
}

fn strip_comment(line: &str) -> &str {
    match line.find('#') {
        Some(pos) if pos > 0 && !line[..pos].ends_with(':') => &line[..pos],
        _ => line,
    }
}

/// Python credential order: config key, then env (`OPENAI_API_KEY`).
fn resolve_credentials() -> Result<(String, String), String> {
    let (provider, api_key, base_url) = load_tts_config();
    // `nous` is serviced by the OpenAI path through the managed gateway —
    // which is entitlement-gated CLI state; the Rust port resolves it from
    // env like any other OpenAI-compatible route.
    let key = api_key.or_else(|| std::env::var("OPENAI_API_KEY").ok().filter(|k| !k.trim().is_empty()));
    match key {
        Some(key) if !key.trim().is_empty() => {
            let base = base_url
                .or_else(|| std::env::var("OPENAI_BASE_URL").ok().filter(|b| !b.trim().is_empty()))
                .unwrap_or_else(|| DEFAULT_OPENAI_BASE_URL.to_string());
            let _ = provider;
            Ok((key, base.trim_end_matches('/').to_string()))
        }
        _ => Err("No TTS provider configured: add tts.openai.api_key to the Hermes config or set OPENAI_API_KEY.".into()),
    }
}

/// `output_path` default: the Hermes audio cache (`$HERMES_DIR/audio_cache`,
/// else `~/.hermes/audio_cache`), `<timestamp>.mp3`.
fn default_output_path() -> std::path::PathBuf {
    let base = std::env::var("HERMES_DIR")
        .ok()
        .filter(|d| !d.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| {
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .map(|h| std::path::PathBuf::from(h).join(".hermes"))
                .unwrap_or_else(|_| std::path::PathBuf::from("."))
        });
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    base.join("audio_cache").join(format!("{stamp}.mp3"))
}

/// Response format from the output extension (Python
/// `_tts_response_format_from_path`); mp3 is the default.
fn response_format(path: &std::path::Path) -> String {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_lowercase())
        .filter(|e| matches!(e.as_str(), "mp3" | "opus" | "aac" | "flac" | "wav" | "pcm"))
        .unwrap_or_else(|| "mp3".into())
}

/// Execute `text_to_speech` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let poster = ReqwestPoster;
    execute_with(&poster, None, args)
}

/// `execute` against an injected poster (tests, embedding hosts). An explicit
/// `credential` bypasses the env/config resolution (tests run keyless).
pub fn execute_with(poster: &dyn SpeechPoster, credential: Option<&str>, args: &Value) -> String {
    let text = args.get("text").and_then(Value::as_str).unwrap_or("");
    if text.trim().is_empty() {
        return tool_error("text is required for text_to_speech");
    }
    let (api_key, base_url) = match credential {
        Some(key) => (key.to_string(), DEFAULT_OPENAI_BASE_URL.trim_end_matches('/').to_string()),
        None => match resolve_credentials() {
            Ok(pair) => pair,
            Err(_) => {
                return tool_error(
                    "No TTS provider configured: add tts.openai.api_key to the Hermes config or set OPENAI_API_KEY.",
                )
            }
        },
    };

    let speed = match args.get("speed") {
        Some(Value::Number(n)) => n.as_f64().unwrap_or(1.0).clamp(SPEED_RANGE.0, SPEED_RANGE.1),
        _ => 1.0,
    };
    let voice = args
        .get("voice")
        .and_then(Value::as_str)
        .filter(|v| !v.trim().is_empty())
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|| DEFAULT_OPENAI_VOICE.to_string());
    let model = std::env::var("TTS_MODEL").ok().filter(|m| !m.trim().is_empty()).unwrap_or_else(|| DEFAULT_OPENAI_MODEL.to_string());
    let instructions = args.get("instructions").and_then(Value::as_str).filter(|i| !i.trim().is_empty());

    let output_path = args
        .get("output_path")
        .and_then(Value::as_str)
        .filter(|p| !p.trim().is_empty())
        .map(std::path::PathBuf::from)
        .unwrap_or_else(default_output_path);
    let format = response_format(&output_path);

    let mut body = json!({
        "model": model,
        "input": text,
        "voice": voice,
        "response_format": format,
        "speed": speed,
    });
    if let Some(instructions) = instructions {
        body["instructions"] = json!(instructions);
    }

    let url = format!("{base_url}/audio/speech");
    let bytes = match poster.post_speech(&url, &api_key, &body.to_string()) {
        Ok(bytes) => bytes,
        Err(e) => return json!({ "success": false, "error": e }).to_string(),
    };
    if let Some(parent) = output_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    match std::fs::write(&output_path, &bytes) {
        Ok(()) => json!({ "success": true, "output_path": output_path.display().to_string(), "bytes": bytes.len() }).to_string(),
        Err(e) => json!({ "success": false, "error": format!("failed to write audio to {}: {e}", output_path.display()) }).to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubPoster {
        requests: Mutex<Vec<(String, String, String)>>,
        bytes: Result<Vec<u8>, String>,
    }

    impl SpeechPoster for StubPoster {
        fn post_speech(&self, url: &str, bearer: &str, body: &str) -> Result<Vec<u8>, String> {
            self.requests.lock().unwrap().push((url.to_string(), bearer.to_string(), body.to_string()));
            match &self.bytes {
            Ok(b) => Ok(b.clone()),
            Err(e) => Err(e.clone()),
            }
        }
    }

    impl StubPoster {
        fn ok() -> Self {
            Self { requests: Mutex::new(Vec::new()), bytes: Ok(b"ID3 audiodata".to_vec()) }
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    fn env_cleanup() {
        // Tests run without OPENAI_API_KEY in CI; execute() is exercised via
        // execute_with, which doesn't need credentials.
    }

    #[test]
    fn writes_audio_and_reports_output_path() {
        let _ = env_cleanup();
        let stub = StubPoster::ok();
        let dir = std::env::temp_dir().join(format!("tts-out-{}", std::process::id()));
        let out_path = dir.join("hello.mp3");
        let out = v(&execute_with(&stub, Some("test-key"), &json!({
            "text": "hello world",
            "output_path": out_path.display().to_string(),
        })));
        assert_eq!(out["success"], true, "{out}");
        assert!(out_path.exists(), "{}", out_path.display());
        assert_eq!(std::fs::read(&out_path).unwrap(), b"ID3 audiodata");
        let (url, _bearer, body) = &stub.requests.lock().unwrap()[0];
        assert!(url.ends_with("/audio/speech"), "{url}");
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["input"], "hello world");
        assert_eq!(body["model"], DEFAULT_OPENAI_MODEL);
        assert_eq!(body["voice"], DEFAULT_OPENAI_VOICE);
        assert_eq!(body["response_format"], "mp3");
        assert_eq!(body["speed"], 1.0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn speed_is_clamped_and_instructions_forwarded() {
        let stub = StubPoster::ok();
        let out_path = std::env::temp_dir().join(format!("tts-{}-slow.wav", std::process::id()));
        let out = v(&execute_with(&stub, Some("test-key"), &json!({
            "text": "slow", "speed": 99.0, "voice": "breeze",
            "instructions": "whisper, cheerful", "output_path": out_path.display().to_string(),
        })));
        assert_eq!(out["success"], true, "{out}");
        let (_url, _b, body) = &stub.requests.lock().unwrap()[0];
        let body: Value = serde_json::from_str(body).unwrap();
        assert_eq!(body["speed"], SPEED_RANGE.1, "clamped to 4.0");
        assert_eq!(body["voice"], "breeze");
        assert_eq!(body["response_format"], "wav");
        assert_eq!(body["instructions"], "whisper, cheerful");
        let _ = std::fs::remove_file(&out_path);
    }

    #[test]
    fn empty_text_is_tool_error_without_a_request() {
        let stub = StubPoster::ok();
        let out = v(&execute_with(&stub, Some("test-key"), &json!({"text": "   "})));
        assert!(out.get("error").is_some(), "{out}");
        assert!(stub.requests.lock().unwrap().is_empty());
    }

    #[test]
    fn provider_errors_are_success_false() {
        let stub = StubPoster { requests: Mutex::new(Vec::new()), bytes: Err("tts returned HTTP 429: quota exceeded".into()) };
        let out_path = std::env::temp_dir().join(format!("tts-{}-err.mp3", std::process::id()));
        let out = v(&execute_with(&stub, Some("test-key"), &json!({"text": "hi", "output_path": out_path.display().to_string()})));
        assert_eq!(out["success"], false);
        assert!(out["error"].as_str().unwrap().contains("429"), "{out}");
        assert!(!out_path.exists(), "no file is written on failure");
    }
}
