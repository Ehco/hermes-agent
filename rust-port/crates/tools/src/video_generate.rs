//! video_generate — one tool dispatching to a plugin-registered video-gen
//! provider (port of `tools/video_generation_tool.py`).
//!
//! Ships no in-tree provider: the tool layer does lightweight validation and
//! coercion, then hands the call to the injected `VideoGenProvider` seam (the
//! plugin host installs the active one). Without a provider the faithful
//! Python errors come back: `provider_not_registered` when a provider name is
//! configured but absent, `no_provider_configured` when none is.

use serde_json::{json, Value};

use crate::util::tool_error;
use std::sync::RwLock;

pub const COMMON_ASPECT_RATIOS: &[&str] = &["16:9", "1:1", "9:16", "4:3", "3:2"];
pub const COMMON_RESOLUTIONS: &[&str] = &["720p", "1080p"];
pub const DEFAULT_ASPECT_RATIO: &str = "16:9";
pub const DEFAULT_RESOLUTION: &str = "1080p";

/// The active video-gen backend (Python `agent/video_gen_registry.py`). The
/// plugin host injects one; bare registry execution has none.
pub trait VideoGenProvider: Send + Sync {
    fn name(&self) -> String;
    fn default_model(&self) -> String;
    /// `kwargs` carries model, image_url, reference_image_urls, duration,
    /// aspect_ratio, resolution, negative_prompt, audio, seed, upscale (None
    /// entries dropped). Returns the provider's result JSON.
    fn generate(&self, prompt: &str, kwargs: &Value) -> Result<Value, String>;
}

/// `video_gen.provider` / `video_gen.model` read seam (Python config.yaml).
/// Hosts set it; the default reads HERMES_VIDEO_PROVIDER / HERMES_VIDEO_MODEL.
pub fn read_video_gen_config() -> (Option<String>, Option<String>) {
    let provider = std::env::var("HERMES_VIDEO_PROVIDER")
        .ok()
        .filter(|v| !v.trim().is_empty());
    let model = std::env::var("HERMES_VIDEO_MODEL")
        .ok()
        .filter(|v| !v.trim().is_empty());
    (provider, model)
}

static PROVIDER: RwLock<Option<std::sync::Arc<dyn VideoGenProvider>>> = RwLock::new(None);

/// Install (or with None, clear) the active provider.
pub fn set_provider(provider: Option<std::sync::Arc<dyn VideoGenProvider>>) {
    *PROVIDER.write().unwrap() = provider;
}

fn active_provider() -> Option<std::sync::Arc<dyn VideoGenProvider>> {
    PROVIDER.read().unwrap().clone()
}

/// Execute `video_generate` — the registry entry point.
pub fn execute(args: &Value) -> String {
    let prompt = args
        .get("prompt")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    let image_url = args
        .get("image_url")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());
    let reference_image_urls: Option<Vec<String>> =
        args.get("reference_image_urls").and_then(|v| {
            let list = match v {
                Value::String(s) => vec![s.clone()],
                Value::Array(items) => items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                    .map(str::to_string)
                    .collect(),
                _ => return None,
            };
            if list.is_empty() {
                None
            } else {
                Some(list)
            }
        });

    // Coerced BEFORE validation (ordering parity: a bad value raises before a
    // missing prompt).
    let optional = json!({
        "duration": coerce_int(args.get("duration")),
        "aspect_ratio": args.get("aspect_ratio").and_then(Value::as_str).map(str::trim)
            .filter(|v| !v.is_empty()).unwrap_or(DEFAULT_ASPECT_RATIO),
        "resolution": args.get("resolution").and_then(Value::as_str).map(str::trim)
            .filter(|v| !v.is_empty()).unwrap_or(DEFAULT_RESOLUTION),
        "negative_prompt": args.get("negative_prompt").and_then(Value::as_str).map(str::trim)
            .filter(|v| !v.is_empty()),
        "audio": coerce_bool(args.get("audio")),
        "seed": coerce_int(args.get("seed")),
        "upscale": coerce_bool(args.get("upscale")),
    });

    // Soft validation — providers do their own; our surface never accepts image-only.
    if prompt.is_empty() {
        return tool_error("prompt is required for video generation");
    }
    if args.get("operation").is_some() || args.get("video_url").is_some() {
        return tool_error(
            "video_generate only supports text-to-video, image-to-video, and \
             reference-to-video; use a provider-specific tool for video edit/extend",
        );
    }
    let (configured_provider, configured_model) = read_video_gen_config();
    let Some(provider) = active_provider() else {
        return match &configured_provider {
            Some(configured) => json!({
                "success": false,
                "error": format!(
                    "video_gen.provider='{configured}' is set but no plugin registered that name. \
                     Run `hermes plugins list` to see installed video gen backends, or \
                     `hermes tools` → Video Generation to pick one."
                ),
                "error_type": "provider_not_registered",
                "provider": configured,
            })
            .to_string(),
            None => json!({
                "success": false,
                "error": "No video generation backend is configured. Run `hermes tools` → \
                          Video Generation to enable one (xAI, FAL, OpenRouter, or DeepInfra).",
                "error_type": "no_provider_configured",
            })
            .to_string(),
        };
    };

    // Config, then provider default; a `model` in args is ignored (models do not choose models).
    let model = configured_model.unwrap_or_else(|| provider.default_model());
    let mut kwargs = optional;
    if let Some(image_url) = image_url {
        kwargs["image_url"] = json!(image_url);
    }
    if let Some(refs) = reference_image_urls {
        kwargs["reference_image_urls"] = json!(refs);
    }
    kwargs["model"] = json!(model);

    let pname = provider.name();
    match provider.generate(prompt, &kwargs) {
        Err(e) => json!({
            "success": false,
            "error": format!("Provider '{pname}' error: {e}"),
            "error_type": "provider_exception",
            "provider": pname,
            "model": model,
            "prompt": prompt,
        })
        .to_string(),
        Ok(result) => result.to_string(),
    }
}

fn coerce_int(value: Option<&Value>) -> Option<i64> {
    match value? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) if !s.trim().is_empty() => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn coerce_bool(value: Option<&Value>) -> Option<bool> {
    match value? {
        Value::Bool(b) => Some(*b),
        Value::String(s) => match s.trim().to_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubProvider {
        calls: Mutex<Vec<(String, Value)>>,
        answer: Value,
    }

    impl VideoGenProvider for StubProvider {
        fn name(&self) -> String {
            "stub".into()
        }
        fn default_model(&self) -> String {
            "stub-1".into()
        }
        fn generate(&self, prompt: &str, kwargs: &Value) -> Result<Value, String> {
            self.calls
                .lock()
                .unwrap()
                .push((prompt.to_string(), kwargs.clone()));
            Ok(self.answer.clone())
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn validation_errors() {
        let _g = guard();
        set_provider(None);
        assert_eq!(
            execute(&json!({"prompt": " "})),
            r#"{"error":"prompt is required for video generation"}"#
        );
        let out = execute(&json!({"prompt": "x", "operation": "edit"}));
        assert!(out.contains("video edit/extend"), "{out}");
        let out = execute(&json!({"prompt": "x", "video_url": "https://x"}));
        assert!(out.contains("video edit/extend"), "{out}");
        // Bad coercions happen BEFORE the missing-prompt check (ordering parity).
        let out = execute(&json!({"duration": "soon"}));
        assert!(out.contains("prompt is required"), "{out}");
    }

    #[test]
    fn no_provider_errors_faithfully() {
        let _g = guard();
        set_provider(None);
        let out = execute(&json!({"prompt": "a dog"}));
        assert!(out.contains("no_provider_configured"), "{out}");
        std::env::set_var("HERMES_VIDEO_PROVIDER", "fal");
        let out = execute(&json!({"prompt": "a dog"}));
        std::env::remove_var("HERMES_VIDEO_PROVIDER");
        assert!(out.contains("provider_not_registered"), "{out}");
        assert!(
            out.contains("'fal' is set but no plugin registered"),
            "{out}"
        );
    }

    #[test]
    fn provider_receives_coerced_kwargs() {
        let _g = guard();
        let stub = std::sync::Arc::new(StubProvider {
            calls: Mutex::new(vec![]),
            answer: json!({"success": true, "video": "https://cdn/v.mp4"}),
        });
        set_provider(Some(stub.clone()));
        let out = execute(&json!({
            "prompt": "a dog",
            "duration": "5",
            "audio": "on",
            "seed": 7,
            "aspect_ratio": "",
        }));
        assert_eq!(out, r#"{"success":true,"video":"https://cdn/v.mp4"}"#);
        let (prompt, kwargs) = &stub.calls.lock().unwrap()[0];
        assert_eq!(prompt, "a dog");
        assert_eq!(kwargs["duration"], 5);
        assert_eq!(kwargs["audio"], true);
        assert_eq!(kwargs["seed"], 7);
        assert_eq!(kwargs["aspect_ratio"], DEFAULT_ASPECT_RATIO);
        assert_eq!(kwargs["model"], "stub-1");
        set_provider(None);
    }

    #[test]
    fn provider_errors_are_shaped() {
        let _g = guard();
        struct Failing;
        impl VideoGenProvider for Failing {
            fn name(&self) -> String {
                "failing".into()
            }
            fn default_model(&self) -> String {
                "f-1".into()
            }
            fn generate(&self, _p: &str, _k: &Value) -> Result<Value, String> {
                Err("over quota".into())
            }
        }
        set_provider(Some(std::sync::Arc::new(Failing)));
        let out = execute(&json!({"prompt": "x"}));
        assert!(out.contains("provider_exception"), "{out}");
        assert!(
            out.contains("Provider 'failing' error: over quota"),
            "{out}"
        );
        set_provider(None);
    }
}
