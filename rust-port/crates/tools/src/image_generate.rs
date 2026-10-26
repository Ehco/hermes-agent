//! image_generate — image generation via the FAL.ai queue API.
//!
//! Port of `tools/image_generation_tool.py` (the in-tree FAL path): the model
//! catalog with its `supports` whitelist, `_build_fal_payload` /
//! `_build_fal_edit_payload`, and the queue submit → poll → result flow of
//! `fal_client` expressed as direct REST calls (`https://queue.fal.run/...`).
//! Plugin/managed-gateway routing is out of scope for this port: no FAL_KEY
//! means the catalog's actionable setup error, exactly the Python's
//! `_build_no_backend_setup_message` fallback line.
//!
//! Output contract (must stay identical for the model):
//! `{"success": true, "image": <url>, "modality": "text"|"image", "upscaled": bool}`
//! or `{"success": false, "image": null, "error": ..., "error_type": ...}`.

use serde_json::{json, Map, Value};

use crate::util::tool_error;

/// `image_generation_catalog.py::DEFAULT_ASPECT_RATIO` / `DEFAULT_MODEL`.
pub const DEFAULT_ASPECT_RATIO: &str = "landscape";
pub const DEFAULT_MODEL: &str = "fal-ai/flux-2/klein/9b";
/// `image_generation_catalog.VALID_ASPECT_RATIOS`.
pub const VALID_ASPECT_RATIOS: &[&str] = &["landscape", "square", "portrait"];

/// `image_generation_catalog._PRESET_SIZES` (the default model's
/// `image_size_preset` mapping).
const PRESET_SIZES: &[(&str, &str)] = &[
    ("landscape", "landscape_16_9"),
    ("square", "square_hd"),
    ("portrait", "portrait_16_9"),
];

/// The catalog subset this port ships: the default model's defaults and
/// whitelist (text-to-image only; edit endpoints need `image_url` support
/// which the direct-REST port advertises identically when present).
///
/// `_model()` shape: (defaults, supports, size_style, edit_endpoint).
#[derive(Debug, Clone, PartialEq)]
pub struct ModelMeta {
    pub display: &'static str,
    pub defaults: Vec<(&'static str, Value)>,
    pub supports: Vec<&'static str>,
    pub size_style: SizeStyle,
    pub edit_endpoint: Option<&'static str>,
    pub edit_supports: Vec<&'static str>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SizeStyle {
    /// `image_size_preset` — the FAL preset enum (`image_size` key).
    ImageSizePreset,
}

/// `image_generation_catalog.FAL_MODELS[DEFAULT_MODEL]` (defaults and
/// whitelist taken verbatim from the Python catalog).
pub fn catalog() -> Vec<(&'static str, ModelMeta)> {
    vec![(
        "fal-ai/flux-2/klein/9b",
        ModelMeta {
            display: "FLUX 2 Klein 9B",
            defaults: vec![
                ("num_inference_steps", json!(4)),
                ("output_format", json!("png")),
                ("enable_safety_checker", json!(false)),
            ],
            supports: vec![
                "prompt",
                "image_size",
                "seed",
                "num_inference_steps",
                "output_format",
                "enable_safety_checker",
                "num_images",
            ],
            size_style: SizeStyle::ImageSizePreset,
            edit_endpoint: Some("fal-ai/flux-2/klein/9b/edit"),
            // Edit endpoints mostly auto-infer size, so `image_size` is not in
            // the edit whitelist (mirrors `edit_supports`).
            edit_supports: vec!["prompt", "image_url", "output_format", "num_images"],
        },
    )]
}

/// `image_generation_tool._resolve_fal_model`: an unknown model id falls back
/// to DEFAULT_MODEL (warned in the Python; here silently, per port scope).
pub fn resolve_fal_model(model: Option<&str>) -> (String, ModelMeta) {
    let id = model.unwrap_or(DEFAULT_MODEL);
    for (known, meta) in catalog() {
        if known == id {
            return (known.to_string(), meta);
        }
    }
    (
        DEFAULT_MODEL.to_string(),
        catalog()
            .into_iter()
            .find(|(k, _)| *k == DEFAULT_MODEL)
            .unwrap()
            .1,
    )
}

fn preset_size(aspect: &str) -> &str {
    PRESET_SIZES
        .iter()
        .find(|(name, _)| *name == aspect)
        .map(|(_, size)| *size)
        .unwrap_or("landscape_16_9")
}

/// `image_generation_tool._build_payload`: defaults + prompt + native size
/// spec + seed + overrides, filtered to the model whitelist (`prompt` always
/// survives a whitelist gap — every endpoint requires it).
pub fn build_payload(
    model: &ModelMeta,
    prompt: &str,
    aspect_ratio: Option<&str>,
    seed: Option<i64>,
    overrides: Option<&Map<String, Value>>,
) -> Value {
    build_payload_inner(model, prompt, aspect_ratio, seed, overrides, None)
}

/// `image_generation_tool._build_fal_edit_payload`: image-edit payload —
/// `image_urls` + prompt, filtered to `edit_supports`.
pub fn build_edit_payload(
    model: &ModelMeta,
    prompt: &str,
    image_urls: &[String],
    aspect_ratio: Option<&str>,
    seed: Option<i64>,
    overrides: Option<&Map<String, Value>>,
) -> Value {
    build_payload_inner(
        model,
        prompt,
        aspect_ratio,
        seed,
        overrides,
        Some(image_urls),
    )
}

fn build_payload_inner(
    meta: &ModelMeta,
    prompt: &str,
    aspect_ratio: Option<&str>,
    seed: Option<i64>,
    overrides: Option<&Map<String, Value>>,
    image_urls: Option<&[String]>,
) -> Value {
    let edit = image_urls.is_some();
    let supports: Vec<&str> = if edit && !meta.edit_supports.is_empty() {
        meta.edit_supports.clone()
    } else {
        meta.supports.clone()
    };
    let aspect = match aspect_ratio
        .map(str::to_lowercase)
        .as_deref()
        .map(str::trim)
    {
        Some(a) if VALID_ASPECT_RATIOS.contains(&a) => a.to_string(),
        _ => DEFAULT_ASPECT_RATIO.to_string(),
    };

    let mut payload = Map::new();
    // defaults first (overridable).
    for (k, v) in &meta.defaults {
        payload.insert(k.to_string(), v.clone());
    }
    // prompt (and the source-image key on edits) survive a whitelist gap.
    payload.insert("prompt".into(), json!(prompt.trim()));
    let mut required = vec!["prompt"];
    if let Some(urls) = image_urls {
        payload.insert("image_urls".into(), json!(urls));
        required.push("image_urls");
    }
    // `image_size` is sent only when the whitelist supports it (edit
    // endpoints mostly auto-infer size).
    let size_key = match meta.size_style {
        SizeStyle::ImageSizePreset => Some("image_size"),
    };
    if let Some(size_key) = size_key {
        let sends_size = !edit || supports.contains(&size_key);
        if sends_size {
            payload.insert(size_key.into(), json!(preset_size(&aspect)));
        }
    }
    if let Some(seed) = seed {
        payload.insert("seed".into(), json!(seed));
    }
    if let Some(overrides) = overrides {
        for (k, v) in overrides {
            if !v.is_null() {
                payload.insert(k.clone(), v.clone());
            }
        }
    }
    // Filter to the whitelist + required keys.
    let filtered: Map<String, Value> = payload
        .into_iter()
        .filter(|(k, _)| supports.contains(&k.as_str()) || required.contains(&k.as_str()))
        .collect();
    Value::Object(filtered)
}

/// The direct-REST equivalent of `_submit_fal_request` + `wait_fal_queue_result`
/// construction: the submit POST for a FAL queue endpoint. Returned as parts
/// so tests can assert the request without a network (the mock seam).
pub struct SubmitRequest {
    pub url: String,
    pub auth_header: String,
    pub body: Value,
}

pub fn build_submit_request(endpoint: &str, payload: &Value, fal_key: &str) -> SubmitRequest {
    SubmitRequest {
        url: format!("https://queue.fal.run/{endpoint}"),
        // fal-client's `Key <secret>` header convention.
        auth_header: format!("Key {fal_key}"),
        body: payload.clone(),
    }
}

/// The submit response envelope (`fal_client.submit`'s return): request_id
/// plus the status/response URLs.
#[derive(Debug, Clone, PartialEq)]
pub struct QueueTicket {
    pub request_id: String,
    pub status_url: String,
    pub response_url: String,
}

/// Parse the submit response (`parse_queue_submit` — the mock seam for tests).
pub fn parse_queue_submit(response: &Value) -> Result<QueueTicket, String> {
    let request_id = response
        .get("request_id")
        .and_then(Value::as_str)
        .ok_or_else(|| "Invalid response from FAL.ai API — no request_id".to_string())?
        .to_string();
    let status_url = response
        .get("status_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://queue.fal.run/requests/{request_id}/status"));
    let response_url = response
        .get("response_url")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| format!("https://queue.fal.run/requests/{request_id}"));
    Ok(QueueTicket {
        request_id: request_id.to_string(),
        status_url,
        response_url,
    })
}

/// `image_generation_tool._format_images`: normalize FAL result images
/// (upscale chaining is per-call opt-in and False everywhere in the catalog,
/// so the formatted row is the plain `{"url", "width", "height", "upscaled": false}`).
pub fn format_images(images: &[Value]) -> Vec<Value> {
    images
        .iter()
        .filter(|img| img.get("url").and_then(Value::as_str).is_some())
        .map(|img| {
            json!({
                "url": img["url"],
                "width": img.get("width").and_then(Value::as_i64).unwrap_or(0),
                "height": img.get("height").and_then(Value::as_i64).unwrap_or(0),
                "upscaled": false,
            })
        })
        .collect()
}

/// `image_generation_tool.image_generate_tool`'s success return.
fn success_json(image: &Value, modality: &str) -> Value {
    json!({
        "success": true,
        "image": image["url"].clone(),
        "modality": modality,
        "upscaled": image.get("upscaled").and_then(Value::as_bool).unwrap_or(false),
    })
}

/// The failure envelope of every error path.
fn provider_error(error: &str, error_type: &str) -> String {
    json!({ "success": false, "image": null, "error": error, "error_type": error_type }).to_string()
}

/// `_prepare_fal_request`: validate inputs and return `(endpoint, arguments)`.
fn prepare_fal_request(
    model_id: &str,
    meta: &ModelMeta,
    prompt: &str,
    aspect_ratio: Option<&str>,
    seed: Option<i64>,
    overrides: Option<&Map<String, Value>>,
    source_images: &[String],
) -> Result<(String, Value), (String, String)> {
    if prompt.trim().is_empty() {
        return Err((
            "Prompt is required and must be a non-empty string".to_string(),
            "ValueError".to_string(),
        ));
    }
    if !source_images.is_empty() && meta.edit_endpoint.is_none() {
        return Err((
            format!(
                "Model '{}' ({model_id}) is not capable of image-to-image / editing. \
                 Provide a text-only prompt (omit image_url), or switch to an edit-capable model.",
                meta.display
            ),
            "ValueError".to_string(),
        ));
    }
    if !source_images.is_empty() {
        let endpoint = meta.edit_endpoint.unwrap_or(model_id);
        Ok((
            endpoint.to_string(),
            build_edit_payload(meta, prompt, source_images, aspect_ratio, seed, overrides),
        ))
    } else {
        Ok((
            model_id.to_string(),
            build_payload(meta, prompt, aspect_ratio, seed, overrides),
        ))
    }
}

/// `_resolve_managed_fal_gateway` / `check_fal_api_key`: direct credentials or
/// nothing (the Rust port has no managed gateway); None means the setup error.
pub fn fal_key() -> Option<String> {
    std::env::var("FAL_KEY")
        .ok()
        .map(|k| k.trim().to_string())
        .filter(|k| !k.is_empty())
}

/// Execute `image_generate` — the registry entry point (JSON string in, out).
pub fn execute(args: &Value) -> String {
    let prompt = args.get("prompt").and_then(Value::as_str).unwrap_or("");
    if prompt.is_empty() {
        return tool_error("prompt is required for image generation");
    }
    let aspect_ratio = args.get("aspect_ratio").and_then(Value::as_str);
    let seed = args.get("seed").and_then(Value::as_i64);
    let mut overrides = Map::new();
    for key in [
        "num_inference_steps",
        "guidance_scale",
        "num_images",
        "output_format",
    ] {
        if let Some(v) = args.get(key).cloned() {
            if !v.is_null() {
                overrides.insert(key.into(), v);
            }
        }
    }
    let source_images: Vec<String> = args
        .get("image_url")
        .and_then(Value::as_str)
        .map(|s| vec![s.to_string()])
        .unwrap_or_default();

    let (model_id, meta) = resolve_fal_model(args.get("fal_model").and_then(Value::as_str));
    let (endpoint, arguments) = match prepare_fal_request(
        &model_id,
        &meta,
        prompt,
        aspect_ratio,
        seed,
        if overrides.is_empty() {
            None
        } else {
            Some(&overrides)
        },
        &source_images,
    ) {
        Ok(v) => v,
        Err((error, error_type)) => return provider_error(&error, &error_type),
    };

    let Some(key) = fal_key() else {
        return provider_error(
            &format!(
                "Error generating image: FAL_KEY environment variable is not set. Get a free API \
                 key at https://fal.ai and set FAL_KEY=<your-key> (then restart the session)."
            ),
            "ValueError",
        );
    };

    // Submit → poll → result, the fal_client queue flow over direct REST.
    let submit = build_submit_request(&endpoint, &arguments, &key);
    let client = crate::web_common::http_client();
    let request_timeout = std::time::Duration::from_secs(120);
    let send = |url: &str| {
        client
            .get(url)
            .timeout(request_timeout)
            .header("Authorization", &submit.auth_header)
            .send()
    };
    let submitted: Value = match client
        .post(&submit.url)
        .timeout(request_timeout)
        .header("Authorization", &submit.auth_header)
        .json(&submit.body)
        .send()
    {
        Ok(resp) => match resp.json() {
            Ok(v) => v,
            Err(e) => {
                return provider_error(
                    &format!("Invalid response from FAL.ai API: {e}"),
                    "ProtocolError",
                )
            }
        },
        Err(e) => {
            return provider_error(
                &format!("Error generating image: could not submit to FAL: {e}"),
                "RequestError",
            )
        }
    };
    let ticket = match parse_queue_submit(&submitted) {
        Ok(t) => t,
        Err(e) => return provider_error(&format!("Error generating image: {e}"), "ProtocolError"),
    };

    // Poll until COMPLETED (fal_client.wait_fal_queue_result).
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(120);
    let _status: Value = loop {
        if std::time::Instant::now() > deadline {
            return provider_error(
                &format!(
                    "Error generating image: FAL job {} timed out",
                    ticket.request_id
                ),
                "TimeoutError",
            );
        }
        let poll = match send(&ticket.status_url) {
            Ok(resp) => resp,
            Err(e) => {
                return provider_error(
                    &format!("Error generating image: could not poll FAL status: {e}"),
                    "RequestError",
                )
            }
        };
        match poll.json::<Value>() {
            Ok(status) => match status.get("status").and_then(Value::as_str) {
                Some("COMPLETED") => break status,
                Some("OK" | "IN_PROGRESS" | "IN_QUEUE") => {
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
                other => {
                    return provider_error(
                        &format!("Error generating image: FAL job failed: {other:?}"),
                        "RuntimeError",
                    )
                }
            },
            Err(e) => {
                return provider_error(
                    &format!("Error generating image: could not decode FAL status: {e}"),
                    "RequestError",
                )
            }
        }
    };

    // GET the result: {"images": [{url, width, height}]}.
    let result = match send(&ticket.response_url) {
        Ok(resp) => resp,
        Err(e) => {
            return provider_error(
                &format!("Error generating image: could not read FAL result: {e}"),
                "RequestError",
            )
        }
    };
    let result: Value = match result.json() {
        Ok(v) => v,
        Err(e) => {
            return provider_error(
                &format!("Error generating image: could not read FAL result: {e}"),
                "RequestError",
            )
        }
    };
    let images = result
        .get("images")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if images.is_empty() {
        return provider_error(
            "Error generating image: No images were generated",
            "ValueError",
        );
    }
    let formatted = format_images(&images);
    if formatted.is_empty() {
        return provider_error(
            "Error generating image: No valid image URLs returned from API",
            "ValueError",
        );
    }
    let modality = if source_images.is_empty() {
        "text"
    } else {
        "image"
    };
    success_json(&formatted[0], modality).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn meta_default() -> ModelMeta {
        catalog()
            .into_iter()
            .find(|(k, _)| *k == DEFAULT_MODEL)
            .unwrap()
            .1
    }

    #[test]
    fn payload_has_defaults_prompt_and_size() {
        let meta = meta_default();
        let out = build_payload(&meta, "  a cat  ", Some("square"), None, None);
        assert_eq!(out["prompt"], "a cat");
        assert_eq!(out["image_size"], "square_hd");
        assert_eq!(out["num_inference_steps"], 4);
        assert_eq!(out["output_format"], "png");
        assert_eq!(out["enable_safety_checker"], false);
    }

    #[test]
    fn unknown_aspect_falls_back_to_default() {
        let meta = meta_default();
        let out = build_payload(&meta, "x", Some("banner"), None, None);
        assert_eq!(out["image_size"], "landscape_16_9");
        let out = build_payload(&meta, "x", Some("PORTRAIT"), None, None);
        assert_eq!(out["image_size"], "portrait_16_9");
    }

    #[test]
    fn seed_only_when_integer() {
        let meta = meta_default();
        let out = build_payload(&meta, "x", None, Some(42), None);
        assert_eq!(out["seed"], 42);
        let out = build_payload(&meta, "x", None, None, None);
        assert!(out.get("seed").is_none());
    }

    #[test]
    fn overrides_filtered_to_whitelist() {
        let meta = meta_default();
        let mut overrides = Map::new();
        overrides.insert("num_images".into(), json!(2));
        overrides.insert("not_a_param".into(), json!("gone"));
        let out = build_payload(&meta, "x", None, None, Some(&overrides));
        assert_eq!(out["num_images"], 2);
        assert!(out.get("not_a_param").is_none());
    }

    #[test]
    fn prompt_survives_a_whitelist_gap() {
        let mut meta = meta_default();
        meta.supports = vec!["image_size"]; // a catalog mistake must not send a broken request
        let out = build_payload(&meta, "x", None, None, None);
        assert_eq!(out["prompt"], "x");
        assert_eq!(out["image_size"], "landscape_16_9");
    }

    #[test]
    fn edit_payload_uses_edit_supports_and_image_urls() {
        let meta = meta_default();
        let urls = vec!["https://e.com/src.png".to_string()];
        let out = build_edit_payload(&meta, "make it night", &urls, None, None, None);
        assert_eq!(out["prompt"], "make it night");
        assert_eq!(out["image_urls"][0], "https://e.com/src.png");
        // edit_supports includes image_url but the singular image_size is NOT
        // in this model's edit list → no image_size key.
        assert!(out.get("image_size").is_none());
        assert!(out.get("enable_safety_checker").is_none()); // defaults filtered out
    }

    #[test]
    fn submit_request_shape() {
        let payload = build_payload(&meta_default(), "a cat", None, None, None);
        let req = build_submit_request("fal-ai/flux-2/klein/9b", &payload, "my-secret");
        assert_eq!(req.url, "https://queue.fal.run/fal-ai/flux-2/klein/9b");
        assert_eq!(req.auth_header, "Key my-secret");
        assert_eq!(req.body["prompt"], "a cat");
    }

    #[test]
    fn queue_ticket_parses_with_fallback_urls() {
        let ticket = parse_queue_submit(&json!({"request_id": "r1"})).unwrap();
        assert_eq!(ticket.request_id, "r1");
        assert_eq!(
            ticket.status_url,
            "https://queue.fal.run/requests/r1/status"
        );
        assert_eq!(ticket.response_url, "https://queue.fal.run/requests/r1");
        let ticket = parse_queue_submit(&json!({
            "request_id": "r2",
            "status_url": "https://gw/r2/s",
            "response_url": "https://gw/r2"
        }))
        .unwrap();
        assert_eq!(ticket.status_url, "https://gw/r2/s");
        assert_eq!(ticket.response_url, "https://gw/r2");
        let err = parse_queue_submit(&json!({})).unwrap_err();
        assert!(err.contains("request_id"));
    }

    #[test]
    fn format_images_normalizes() {
        let out = format_images(&[
            json!({"url": "https://e.com/i.png", "width": 1024, "height": 768}),
            json!({"width": 1}), // no url → dropped
        ]);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0]["url"], "https://e.com/i.png");
        assert_eq!(out[0]["upscaled"], false);
    }

    #[test]
    fn prepare_validates_prompt_and_edit_capability() {
        let meta = meta_default();
        let (endpoint, args) =
            prepare_fal_request(DEFAULT_MODEL, &meta, "a cat", None, None, None, &[]).unwrap();
        assert_eq!(endpoint, DEFAULT_MODEL);
        assert_eq!(args["prompt"], "a cat");
        let (endpoint, args) = prepare_fal_request(
            DEFAULT_MODEL,
            &meta,
            "edit it",
            None,
            None,
            None,
            &["https://e.com/src.png".to_string()],
        )
        .unwrap();
        assert_eq!(endpoint, "fal-ai/flux-2/klein/9b/edit");
        assert_eq!(args["image_urls"][0], "https://e.com/src.png");
        let (error, _) =
            prepare_fal_request(DEFAULT_MODEL, &meta, "   ", None, None, None, &[]).unwrap_err();
        assert!(error.contains("Prompt is required"));
        // Empty prompt is also caught at the execute level as a tool_error.
        let out = execute(&json!({"prompt": ""}));
        assert!(out.contains("error"));
    }

    #[test]
    fn missing_fal_key_is_an_actionable_error() {
        // Force no key: run with a temp-cleared env via the provider_error path.
        // (execute() reads FAL_KEY; when unset the error names it.)
        let had = std::env::var("FAL_KEY").ok();
        std::env::remove_var("FAL_KEY");
        let out = execute(&json!({"prompt": "a cat"}));
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], false);
        assert!(v["error"].as_str().unwrap().contains("FAL_KEY"));
        assert_eq!(v["error_type"], "ValueError");
        if let Some(k) = had {
            std::env::set_var("FAL_KEY", k);
        }
    }
}
