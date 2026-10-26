//! vision_analyze / video_analyze — multimodal analysis via the auxiliary
//! vision LLM (port of `tools/vision_tools.py`).
//!
//! The Python tool resolves the media (URL / local path / data: URL), normalizes
//! it, base64-encodes it and sends it to the auxiliary vision LLM router
//! (`agent.auxiliary_client`). The Rust port keeps the exact arg surface,
//! validation ordering and the `{"success": bool, "analysis": str}` envelope;
//! the actual multimodal call sits behind the injected `VisionLlm` seam (the
//! host installs the aux client). Without one, the faithful failure envelope is
//! returned — no network is attempted.
//!
//! Faithful bits kept here: the video extension whitelist + "Unsupported video
//! format" error, the 50 MB base64 hard-cap message, the region shape check,
//! and `_run_analysis`'s error envelope `Error analyzing <kind>: ...`.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `_VIDEO_MIME_TYPES` (avi/mkv fall back to mp4; extension → MIME).
pub const VIDEO_MIME_TYPES: &[(&str, &str)] = &[
    (".mp4", "video/mp4"),
    (".webm", "video/webm"),
    (".mov", "video/mov"),
    (".avi", "video/mp4"),
    (".mkv", "video/mp4"),
    (".mpeg", "video/mpeg"),
    (".mpg", "video/mpeg"),
];

/// `_MAX_VIDEO_BASE64_BYTES` — the 50 MB hard cap on the base64 payload.
pub const MAX_VIDEO_BASE64_BYTES: u64 = 50 * 1024 * 1024;
/// `_VIDEO_SIZE_WARN_BYTES`.
pub const VIDEO_SIZE_WARN_BYTES: u64 = 20 * 1024 * 1024;

/// The auxiliary vision/video LLM call (Python `async_call_llm`). `kind` is
/// "image" or "video"; `prompt` is the caller-formatted full prompt. The host
/// injects the active aux client; bare registry execution has none.
pub trait VisionLlm: Send + Sync {
    fn analyze(&self, kind: &str, prompt: &str, model: Option<&str>) -> Result<String, String>;
}

static VISION_LLM: std::sync::RwLock<Option<std::sync::Arc<dyn VisionLlm>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the active aux vision client.
pub fn set_vision_llm(llm: Option<std::sync::Arc<dyn VisionLlm>>) {
    *VISION_LLM.write().unwrap() = llm;
}

fn active_vision_llm() -> Option<std::sync::Arc<dyn VisionLlm>> {
    VISION_LLM.read().unwrap().clone()
}

/// `_configured_aux_model` (config.yaml → env; the Rust port reads the env
/// legacy overrides only).
fn configured_aux_model(env_vars: &[&str]) -> Option<String> {
    env_vars
        .iter()
        .find_map(|e| std::env::var(e).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty()))
}

/// `_detect_video_mime_type`: suffix (lowercased) → MIME, or None.
pub fn detect_video_mime_type(suffix: &str) -> Option<&'static str> {
    VIDEO_MIME_TYPES
        .iter()
        .find(|(ext, _)| *ext == suffix.to_lowercase())
        .map(|(_, mime)| *mime)
}

/// `_unsupported_video_format`.
pub fn unsupported_video_format(suffix: &str) -> String {
    let mut exts: Vec<&str> = VIDEO_MIME_TYPES.iter().map(|(e, _)| *e).collect();
    exts.sort();
    exts.dedup();
    format!(
        "Unsupported video format: '{}'. Supported: {}",
        suffix,
        exts.join(", ")
    )
}

/// `_video_to_base64_data_url`'s oversize error (`video_analyze_tool`).
pub fn video_too_large(base64_len_bytes: u64) -> String {
    format!(
        "Video too large for API: base64 payload is {:.1} MB (limit {} MB). Compress or trim the video and retry.",
        base64_len_bytes as f64 / (1024.0 * 1024.0),
        MAX_VIDEO_BASE64_BYTES / (1024 * 1024)
    )
}

/// The `_run_analysis` failure envelope.
fn analysis_error(kind: &str, msg: &str) -> String {
    json!({ "success": false, "analysis": format!("Error analyzing {kind}: {msg}") }).to_string()
}

/// The success envelope (`_run_analysis`'s `finish`).
fn analysis_success(analysis: &str, scale_note: Option<&str>) -> String {
    let mut result = json!({ "success": true });
    if let Some(note) = scale_note {
        result["analysis"] = json!(format!("[{note}] {analysis}"));
        result["scale_note"] = json!(note);
    } else {
        result["analysis"] = json!(analysis);
    }
    result.to_string()
}

/// File extension of a URL / path (lowercased, with dot), or "" when none.
fn url_suffix(source: &str) -> String {
    let bare = source.split(['?', '#']).next().unwrap_or(source);
    match bare.rsplit_once('.') {
        Some((_, ext)) if !ext.contains('/') && !ext.contains('\\') => {
            format!(".{}", ext.to_lowercase())
        }
        _ => String::new(),
    }
}

/// Execute `vision_analyze` — image_url + question (+ optional 4-int region).
pub fn execute_vision(args: &Value) -> String {
    let image_url = args.get("image_url").and_then(Value::as_str).unwrap_or("");
    let question = args.get("question").and_then(Value::as_str).unwrap_or("");
    if image_url.trim().is_empty() || question.trim().is_empty() {
        return tool_error("image_url and question are required");
    }
    let region = args.get("region");
    if let Some(region) = region {
        let ok = region
            .as_array()
            .map(|a| a.len() == 4 && a.iter().all(Value::is_i64))
            .unwrap_or(false);
        if !ok {
            return tool_error(
                "region must be [x1, y1, x2, y2] in ORIGINAL-image pixel coordinates",
            );
        }
    }
    // The legacy path's full prompt (`_handle_vision_analyze`).
    let prompt = format!(
        "Fully describe and explain everything about this image, then answer the \
         following question:\n\n{question}"
    );
    run_analysis("image", &prompt, configured_aux_model(&["AUXILIARY_VISION_MODEL"]).as_deref())
}

/// Execute `video_analyze` — video_url + question.
pub fn execute_video(args: &Value) -> String {
    let video_url = args.get("video_url").and_then(Value::as_str).unwrap_or("");
    let question = args.get("question").and_then(Value::as_str).unwrap_or("");
    if video_url.trim().is_empty() || question.trim().is_empty() {
        return tool_error("video_url and question are required");
    }
    // Extension → MIME gate (`_materialize_video` + `_detect_video_mime_type`).
    let suffix = url_suffix(video_url);
    if let Some(mime) = detect_video_mime_type(&suffix) {
        let _ = mime;
    } else if !suffix.is_empty() && suffix.len() <= 8 {
        // Only reject explicit-but-unknown suffixes (extension-less CDN URLs pass).
        return analysis_error("video", &unsupported_video_format(&suffix));
    }
    let prompt = format!(
        "Fully describe and explain everything happening in this video, including \
         visual content, motion, audio cues, text overlays, and scene transitions. \
         Then answer the following question:\n\n{question}"
    );
    run_analysis(
        "video",
        &prompt,
        configured_aux_model(&["AUXILIARY_VIDEO_MODEL", "AUXILIARY_VISION_MODEL"]).as_deref(),
    )
}

fn run_analysis(kind: &str, prompt: &str, model: Option<&str>) -> String {
    match active_vision_llm() {
        None => analysis_error(
            kind,
            "the auxiliary vision model is not available in this execution context",
        ),
        Some(llm) => match llm.analyze(kind, prompt, model) {
            Ok(analysis) => analysis_success(&analysis, None),
            Err(e) => analysis_error(kind, &e),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Recording {
        calls: Mutex<Vec<(String, String, Option<String>)>>,
        answer: String,
    }
    impl VisionLlm for Recording {
        fn analyze(&self, kind: &str, prompt: &str, model: Option<&str>) -> Result<String, String> {
            self.calls.lock().unwrap().push((
                kind.to_string(),
                prompt.to_string(),
                model.map(str::to_string),
            ));
            Ok(self.answer.clone())
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn video_mime_table_and_errors() {
        assert_eq!(detect_video_mime_type(".MP4"), Some("video/mp4"));
        assert_eq!(detect_video_mime_type(".avi"), Some("video/mp4")); // falls back to mp4
        assert_eq!(detect_video_mime_type(".mpg"), Some("video/mpeg"));
        assert!(detect_video_mime_type(".gif").is_none());
        assert_eq!(
            unsupported_video_format(".xyz"),
            "Unsupported video format: '.xyz'. Supported: .avi, .mkv, .mov, .mp4, .mpeg, .mpg, .webm"
        );
        assert!(video_too_large(60 * 1024 * 1024).contains("base64 payload is 60.0 MB"));
        assert!(video_too_large(60 * 1024 * 1024).contains("limit 50 MB"));
    }

    #[test]
    fn vision_requires_url_and_question() {
        let _g = guard();
        set_vision_llm(None);
        let out = execute_vision(&json!({}));
        assert!(out.contains("error"), "{out}");
        let out = execute_video(&json!({"video_url": "https://x/v.mp4"}));
        assert!(out.contains("error"), "{out}");
    }

    #[test]
    fn region_must_be_four_ints() {
        let _g = guard();
        set_vision_llm(None);
        let out = execute_vision(&json!({
            "image_url": "https://e.com/i.png",
            "question": "what?",
            "region": [1, 2, 3]
        }));
        assert!(out.contains("region must be [x1, y1, x2, y2]"), "{out}");
    }

    #[test]
    fn video_rejects_unknown_extension_before_the_model() {
        let _g = guard();
        set_vision_llm(None);
        let out = execute_video(&json!({
            "video_url": "https://e.com/clip.gif",
            "question": "what?"
        }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(
            v["analysis"]
                .as_str()
                .unwrap()
                .starts_with("Error analyzing video: Unsupported video format: '.gif'"),
            "{out}"
        );
    }

    #[test]
    fn extension_less_urls_pass_to_the_model() {
        let _g = guard();
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            answer: "a cat".into(),
        });
        set_vision_llm(Some(stub.clone()));
        let out = execute_video(&json!({
            "video_url": "https://cdn.example.com/redirect/42",
            "question": "what?"
        }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["analysis"], "a cat");
        let (kind, prompt, model) = &stub.calls.lock().unwrap()[0];
        assert_eq!(kind, "video");
        assert!(prompt.contains("Fully describe and explain everything happening"));
        assert!(prompt.ends_with("what?"));
        set_vision_llm(None);
    }

    #[test]
    fn image_prompt_shape_and_success_envelope() {
        let _g = guard();
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            answer: "a cat".into(),
        });
        set_vision_llm(Some(stub.clone()));
        let out = execute_vision(&json!({
            "image_url": "https://e.com/i.png",
            "question": "what?"
        }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["analysis"], "a cat");
        let (kind, prompt, _) = &stub.calls.lock().unwrap()[0];
        assert_eq!(kind, "image");
        assert!(prompt.contains("Fully describe and explain everything about this image"));
        set_vision_llm(None);
    }

    #[test]
    fn model_errors_become_the_analysis_envelope() {
        let _g = guard();
        struct Failing;
        impl VisionLlm for Failing {
            fn analyze(&self, _k: &str, _p: &str, _m: Option<&str>) -> Result<String, String> {
                Err("quota exceeded".into())
            }
        }
        set_vision_llm(Some(std::sync::Arc::new(Failing)));
        let out = execute_vision(&json!({
            "image_url": "https://e.com/i.png",
            "question": "q"
        }));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert_eq!(v["analysis"], "Error analyzing image: quota exceeded");
        set_vision_llm(None);
    }
}
