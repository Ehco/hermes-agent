//! send_message — cross-channel messaging via platform APIs (port of
//! `tools/send_message_tool.py`).
//!
//! Faithful surface: action=send / list / react / unreact over the
//! `platform:target` addressing scheme. The delivery half is the injected
//! sender/target registry (Python `send_message_targets.py` /
//! `send_message_senders.py` + the gateway's live adapters): the host supplies
//! `ChannelDirectory` (target resolution, home channels, relay authorization)
//! and `MessageSender` (platform delivery + reactions). Bare registry
//! execution has neither, so it answers the faithful no-adapter error.

use serde_json::{json, Value};

use crate::util::tool_error;

/// Target/directory seam (Python `send_message_targets` + gateway config).
pub trait ChannelDirectory: Send + Sync {
    /// Formatted channel directory (action=list).
    fn format_directory(&self) -> String;
    /// Resolve `platform:ref` to `(chat_id, thread_id)`; Err is the
    /// resolution error text. `chat_id` is None when no ref was given (the
    /// caller falls back to the home channel).
    fn resolve(
        &self,
        platform: &str,
        target_ref: Option<&str>,
    ) -> Result<(Option<String>, Option<String>), String>;
    /// The platform's home channel chat id.
    fn home_chat_id(&self, platform: &str) -> Result<String, String>;
    /// Relay egress-authorization guard: Ok when the send may proceed.
    fn authorize_relay(
        &self,
        platform: &str,
        chat_id: &str,
        thread_id: Option<&str>,
    ) -> Result<(), String>;
}

/// One resolved send request (Python `_send_to_platform`).
pub struct SendRequest<'a> {
    pub platform: &'a str,
    pub chat_id: &'a str,
    pub thread_id: Option<&'a str>,
    pub message: &'a str,
    /// [[media]] paths extracted from the message, in order.
    pub media_files: Vec<String>,
    pub force_document: bool,
}

/// Delivery seam (Python `send_message_senders` / live adapters).
pub trait MessageSender: Send + Sync {
    fn send(&self, request: &SendRequest) -> Result<Value, String>;
    /// `add` attaches `emoji` to the message, `remove` retracts it.
    fn react(
        &self,
        platform: &str,
        chat_id: &str,
        message_id: Option<&str>,
        emoji: &str,
        remove: bool,
    ) -> Result<Value, String>;
}

/// Execute `send_message` — the registry entry point (no registry seams).
pub fn execute(args: &Value) -> String {
    execute_with(None, None, args)
}

/// `execute` against an injected sender/target registry (gateway hosts, tests).
pub fn execute_with(
    directory: Option<&dyn ChannelDirectory>,
    sender: Option<&dyn MessageSender>,
    args: &Value,
) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("send");
    match action {
        "list" => {
            let Some(directory) = directory else {
                return json!({"targets": []}).to_string();
            };
            return json!({ "targets": directory.format_directory() }).to_string();
        }
        "react" | "unreact" => {
            return react(directory, sender, args, action == "unreact");
        }
        "send" => {}
        other => {
            return tool_error(format!(
                "action must be one of: send, list, react, unreact. (got '{other}')"
            ))
        }
    }
    send(directory, sender, args)
}

fn send(
    directory: Option<&dyn ChannelDirectory>,
    sender: Option<&dyn MessageSender>,
    args: &Value,
) -> String {
    let target = args.get("target").and_then(Value::as_str).unwrap_or("");
    let message = args.get("message").and_then(Value::as_str).unwrap_or("");
    if target.is_empty() || message.is_empty() {
        return tool_error("Both 'target' and 'message' are required when action='send'");
    }
    let Some(directory) = directory else {
        return tool_error(no_gateway_text());
    };
    // Target shape: `platform:ref`; no ref = the platform's home channel.
    let (platform_name, target_ref) = split_target(target);
    let (chat_id, thread_id) = match directory.resolve(&platform_name, target_ref) {
        Ok(pair) => pair,
        Err(e) => return tool_error(e),
    };
    let Some(sender) = sender else {
        return tool_error(no_gateway_text());
    };
    let (chat_id, used_home_channel) = match chat_id {
        Some(chat_id) => (chat_id, false),
        None => match directory.home_chat_id(&platform_name) {
            Ok(chat_id) => (chat_id, true),
            Err(e) => return tool_error(e),
        },
    };
    if let Err(e) = directory.authorize_relay(&platform_name, &chat_id, thread_id.as_deref()) {
        return tool_error(e);
    }
    // Capture [[as_document]] before media extraction strips it.
    let force_document = message.contains("[[as_document]]");
    let media_files = extract_media(message);
    let cleaned = clean_message(message);
    let request = SendRequest {
        platform: &platform_name,
        chat_id: &chat_id,
        thread_id: thread_id.as_deref(),
        message: &cleaned,
        media_files,
        force_document,
    };
    match sender.send(&request) {
        Ok(mut result) => {
            if let Some(obj) = result.as_object_mut() {
                if obj.get("success").and_then(Value::as_bool).unwrap_or(false) {
                    if used_home_channel {
                        obj.insert(
                            "note".into(),
                            json!(format!(
                                "Sent to {platform_name} home channel (chat_id: {chat_id})"
                            )),
                        );
                    }
                }
            }
            result.to_string()
        }
        Err(e) => json!({ "success": false, "error": format!("Send failed: {e}") }).to_string(),
    }
}

fn react(
    directory: Option<&dyn ChannelDirectory>,
    sender: Option<&dyn MessageSender>,
    args: &Value,
    remove: bool,
) -> String {
    let target = args.get("target").and_then(Value::as_str).unwrap_or("");
    let emoji = args
        .get("emoji")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("");
    if remove {
        if target.is_empty() {
            return tool_error("'target' is required when action='unreact'");
        }
    } else if target.is_empty() || emoji.is_empty() {
        return tool_error("Both 'target' and 'emoji' are required when action='react'");
    }
    let Some(directory) = directory else {
        return tool_error(no_gateway_text());
    };
    let (platform_name, target_ref) = split_target(target);
    // Platform-native ids match no parser/directory entry; the adapter validates.
    let (chat_id, _thread_id) = match directory.resolve(&platform_name, target_ref) {
        Ok(pair) => pair,
        Err(e) => return tool_error(e),
    };
    let Some(sender) = sender else {
        return tool_error(no_gateway_text());
    };
    let chat_id = match chat_id {
        Some(chat_id) => chat_id,
        None => match directory.home_chat_id(&platform_name) {
            Ok(home) => home,
            Err(_) => {
                return tool_error(format!(
                    "No chat specified and no home channel set for {platform_name}. \
                     Use '{platform_name}:chat_id'."
                ))
            }
        },
    };
    if let Err(e) = directory.authorize_relay(&platform_name, &chat_id, None) {
        return tool_error(e);
    }
    let message_id = args
        .get("message_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty());
    match sender.react(&platform_name, &chat_id, message_id, emoji, remove) {
        Ok(result) => result.to_string(),
        Err(e) => json!({ "error": format!("Reaction failed: {e}") }).to_string(),
    }
}

/// `platform:ref` -> (lowercased platform, Some(ref) when non-empty).
fn split_target(target: &str) -> (String, Option<&str>) {
    let (platform, rest) = match target.split_once(':') {
        Some((p, r)) => (p, Some(r)),
        None => (target, None),
    };
    let target_ref = rest.map(str::trim).filter(|r| !r.is_empty());
    (platform.trim().to_lowercase(), target_ref)
}

/// Python `BasePlatformAdapter.extract_media`: `[[path]]` runs in the message.
fn extract_media(message: &str) -> Vec<String> {
    message
        .split("[[")
        .skip(1)
        .filter_map(|rest| rest.split("]]").next().map(str::to_string))
        .filter(|p| !p.trim().is_empty())
        .collect()
}

/// The message with `[[...]]` media runs stripped.
fn clean_message(message: &str) -> String {
    let mut result = String::new();
    let mut skip = false;
    let mut chars = message.chars().peekable();
    while let Some(c) = chars.next() {
        if skip {
            if c == ']' {
                if chars.peek() == Some(&']') {
                    chars.next();
                }
                skip = false;
            }
            continue;
        }
        if c == '[' && chars.peek() == Some(&'[') {
            chars.next();
            skip = true;
            continue;
        }
        result.push(c);
    }
    result
}

fn no_gateway_text() -> String {
    "send_message requires a messaging gateway with a live platform adapter, which is not \
     available in this execution context. Configure the platform (its token in the profile's \
     config/.env) and run the gateway, or deliver the message through the normal channel instead."
        .into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubDir;

    impl ChannelDirectory for StubDir {
        fn format_directory(&self) -> String {
            "telegram: @hermes (home)\ndiscord: #general".into()
        }
        fn resolve(
            &self,
            platform: &str,
            target_ref: Option<&str>,
        ) -> Result<(Option<String>, Option<String>), String> {
            if platform != "telegram" && platform != "discord" {
                return Err(format!("Unknown platform: {platform}"));
            }
            match target_ref {
                None => Ok((None, None)),
                Some("bad") => Err("Cannot resolve target 'bad'".into()),
                Some(r) => Ok((
                    Some(format!("chat-{r}")),
                    if platform == "discord" {
                        Some("t1".into())
                    } else {
                        None
                    },
                )),
            }
        }
        fn home_chat_id(&self, platform: &str) -> Result<String, String> {
            Ok(format!("home-{platform}"))
        }
        fn authorize_relay(
            &self,
            platform: &str,
            chat_id: &str,
            _t: Option<&str>,
        ) -> Result<(), String> {
            if platform == "discord" && chat_id.contains("forbidden") {
                return Err("Refusing to send to relay target 'discord'".into());
            }
            Ok(())
        }
    }

    struct StubSender {
        sends: Mutex<Vec<(String, String, String)>>,
    }

    impl MessageSender for StubSender {
        fn send(&self, request: &SendRequest) -> Result<Value, String> {
            self.sends.lock().unwrap().push((
                request.platform.into(),
                request.chat_id.into(),
                request.message.into(),
            ));
            Ok(json!({"success": true, "message_id": "m-1"}))
        }
        fn react(
            &self,
            _platform: &str,
            _chat: &str,
            _message_id: Option<&str>,
            _emoji: &str,
            remove: bool,
        ) -> Result<Value, String> {
            if remove {
                Ok(json!({"success": true, "removed": true}))
            } else {
                Ok(json!({"success": true, "reacted": _emoji}))
            }
        }
    }

    #[test]
    fn send_requires_target_and_message() {
        let out = execute(&json!({"message": "hi"}));
        assert!(out.contains("Both 'target' and 'message'"), "{out}");
        let out = execute(&json!({"target": "telegram:chat"}));
        assert!(out.contains("Both 'target' and 'message'"), "{out}");
    }

    #[test]
    fn send_without_a_gateway_degrades() {
        let out = execute(&json!({"target": "telegram:chat", "message": "hi"}));
        assert!(
            out.contains("not available in this execution context"),
            "{out}"
        );
    }

    #[test]
    fn send_resolves_target_and_delivers() {
        let sender = StubSender {
            sends: Mutex::new(vec![]),
        };
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({
                "target": "TELEGRAM:general", "message": "hello [[C:/a.png]] world"
            }),
        );
        assert!(out.contains(r#""success":true"#), "{out}");
        let (platform, chat, text) = &sender.sends.lock().unwrap()[0];
        assert_eq!(platform, "telegram");
        assert_eq!(chat, "chat-general");
        assert_eq!(text, "hello  world", "media run stripped");
    }

    #[test]
    fn home_channel_fallback_and_note() {
        let sender = StubSender {
            sends: Mutex::new(vec![]),
        };
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({
                "target": "telegram", "message": "hi"
            }),
        );
        assert!(
            out.contains("home channel (chat_id: home-telegram)"),
            "{out}"
        );
        let (platform, chat, _) = &sender.sends.lock().unwrap()[0];
        assert_eq!(
            (platform.as_str(), chat.as_str()),
            ("telegram", "home-telegram")
        );
    }

    #[test]
    fn relay_denial_blocks_the_send() {
        let sender = StubSender {
            sends: Mutex::new(vec![]),
        };
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({
                "target": "discord:forbidden", "message": "hi"
            }),
        );
        assert!(out.contains("Refusing to send"), "{out}");
        assert!(sender.sends.lock().unwrap().is_empty());
    }

    #[test]
    fn react_and_unreact() {
        let sender = StubSender {
            sends: Mutex::new(vec![]),
        };
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({"action": "react", "target": "telegram:general", "emoji": "👍"}),
        );
        assert!(out.contains(r#""reacted":"👍""#), "{out}");
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({"action": "unreact", "target": "telegram:general"}),
        );
        assert!(out.contains(r#""removed":true"#), "{out}");
        let out = execute_with(
            Some(&StubDir),
            Some(&sender),
            &json!({"action": "react", "target": "telegram:general"}),
        );
        assert!(out.contains("Both 'target' and 'emoji'"), "{out}");
        let out = execute_with(Some(&StubDir), Some(&sender), &json!({"action": "unreact"}));
        assert!(
            out.contains("'target' is required when action='unreact'"),
            "{out}"
        );
    }

    #[test]
    fn list_without_a_gateway_is_an_empty_targets_list() {
        let out = execute(&json!({"action": "list"}));
        assert_eq!(out, r#"{"targets":[]}"#);
        let out = execute_with(Some(&StubDir), None, &json!({"action": "list"}));
        assert!(out.contains("discord: #general"), "{out}");
    }

    #[test]
    fn media_extraction_runs() {
        assert_eq!(
            extract_media("a [[x.png]] b [[y.mp4]]"),
            vec!["x.png", "y.mp4"]
        );
        assert_eq!(clean_message("a [[x.png]] b"), "a  b");
        assert_eq!(clean_message("plain"), "plain");
    }
}
