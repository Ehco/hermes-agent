//! xai_video_edit / xai_video_extend — xAI Imagine video edit/extend
//! (port of `tools/xai_video_tools.py`).
//!
//! The Python tool validates, gates on `video_gen.provider == "xai"` plus xAI
//! credentials, then hands off to the plugin's `run_xai_video_edit/extend`. The
//! Rust port keeps validation and the gate verbatim; the plugin call sits
//! behind the injected `XaiVideoRunner` seam. Bare registry execution has no
//! runner and `video_gen.provider` is never "xai", so the faithful
//! `provider_not_configured` envelope comes back.

use serde_json::{json, Value};

use crate::util::tool_error;

/// `xai_video_tools._clean_string`.
fn clean_string(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `xai_video_tools._coerce_int` — bool is rejected so duration=true never
/// becomes 1.
pub fn coerce_int(value: Option<&Value>) -> Option<i64> {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(_)) => None,
        Some(Value::Number(n)) => n.as_i64(),
        Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

/// `_configured_for_xai_video` (config.yaml `video_gen.provider`; the Rust
/// port reads the env seam).
pub fn configured_for_xai_video() -> bool {
    std::env::var("HERMES_VIDEO_PROVIDER")
        .ok()
        .map(|v| v.trim() == "xai")
        .unwrap_or(false)
}

/// The plugin runner (Python `plugins.video_gen.xai.run_xai_video_*`). `extra`
/// carries `duration` for extend (None entries dropped).
pub trait XaiVideoRunner: Send + Sync {
    fn run(&self, op: &str, prompt: &str, video_url: &str, duration: Option<i64>) -> Result<Value, String>;
}

static RUNNER: std::sync::RwLock<Option<std::sync::Arc<dyn XaiVideoRunner>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the active xAI video runner.
pub fn set_runner(runner: Option<std::sync::Arc<dyn XaiVideoRunner>>) {
    *RUNNER.write().unwrap() = runner;
}

fn active_runner() -> Option<std::sync::Arc<dyn XaiVideoRunner>> {
    RUNNER.read().unwrap().clone()
}

/// `_run_xai_video_tool`.
fn run_tool(args: &Value, op: &str, duration: Option<i64>) -> String {
    let prompt = clean_string(args.get("prompt"));
    let video_url = clean_string(args.get("video_url"));
    let Some(prompt) = prompt else {
        return tool_error(format!("prompt is required for xAI video {op}"));
    };
    let Some(video_url) = video_url else {
        return tool_error(
            "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` \
             from a prior Imagine result)",
        );
    };
    let lower = video_url.to_lowercase();
    if !lower.starts_with("http://") && !lower.starts_with("https://") {
        return tool_error(
            "video_url must be a public HTTPS MP4 URL (the `video`/`public_url` \
             from a prior Imagine result)",
        );
    }
    if !configured_for_xai_video() {
        return json!({
            "success": false,
            "error": (
                "xAI video edit/extend tools require `video_gen.provider` to be \
                 configured as `xai` via `hermes tools` -> Video Generation."
            ),
            "error_type": "provider_not_configured",
            "provider": "xai",
        })
        .to_string();
    }
    match active_runner() {
        None => json!({
            "success": false,
            "error": format!("xAI video {op} runner is not available in this execution context"),
            "error_type": "provider_not_registered",
            "provider": "xai",
        })
        .to_string(),
        Some(runner) => match runner.run(op, &prompt, &video_url, duration) {
            Ok(result) => result.to_string(),
            Err(e) => json!({
                "success": false,
                "error": e,
                "error_type": "provider_exception",
                "provider": "xai",
            })
            .to_string(),
        },
    }
}

/// Execute `xai_video_edit`.
pub fn execute_edit(args: &Value) -> String {
    run_tool(args, "edit", None)
}

/// Execute `xai_video_extend`.
pub fn execute_extend(args: &Value) -> String {
    run_tool(args, "extend", coerce_int(args.get("duration")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn coerce_int_rejects_bools_but_parses_numeric_strings() {
        assert_eq!(coerce_int(Some(&json!(true))), None);
        assert_eq!(coerce_int(Some(&json!("8"))), Some(8));
        assert_eq!(coerce_int(Some(&json!(8))), Some(8));
        assert_eq!(coerce_int(Some(&json!("soon"))), None);
        assert_eq!(coerce_int(None), None);
    }

    #[test]
    fn validation_order_matches_python() {
        let _g = guard();
        set_runner(None);
        // prompt first, then video_url, then the https check, then the gate.
        assert!(execute_edit(&json!({})).contains("prompt is required for xAI video edit"));
        assert!(execute_extend(&json!({"prompt": "x"}))
            .contains("video_url must be a public HTTPS MP4 URL"));
        assert!(execute_extend(&json!({"prompt": "x", "video_url": "ftp://v.mp4"}))
            .contains("video_url must be a public HTTPS MP4 URL"));
        let out = execute_edit(&json!({"prompt": "x", "video_url": "https://cdn/v.mp4"}));
        assert!(out.contains("provider_not_configured"), "{out}");
        assert!(out.contains("hermes tools` -> Video Generation"), "{out}");
    }

    #[test]
    fn extend_passes_coerced_duration_to_the_runner() {
        let _g = guard();
        struct Stub(std::sync::Mutex<Vec<(String, String, String, Option<i64>)>>);
        impl XaiVideoRunner for Stub {
            fn run(
                &self,
                op: &str,
                prompt: &str,
                video_url: &str,
                duration: Option<i64>,
            ) -> Result<Value, String> {
                self.0.lock().unwrap().push((
                    op.into(),
                    prompt.into(),
                    video_url.into(),
                    duration,
                ));
                Ok(json!({"success": true, "video": "https://files-cdn/v.mp4"}))
            }
        }
        let stub = std::sync::Arc::new(Stub(Mutex::new(vec![])));
        set_runner(Some(stub.clone()));
        std::env::set_var("HERMES_VIDEO_PROVIDER", "xai");
        let out = execute_extend(&json!({
            "prompt": "keep going",
            "video_url": "https://cdn/v.mp4",
            "duration": "5"
        }));
        std::env::remove_var("HERMES_VIDEO_PROVIDER");
        assert!(out.contains("files-cdn"), "{out}");
        let (op, prompt, url, duration) = &stub.0.lock().unwrap()[0];
        assert_eq!((op.as_str(), prompt.as_str(), url.as_str()), ("extend", "keep going", "https://cdn/v.mp4"));
        assert_eq!(*duration, Some(5));
        set_runner(None);
    }
}
