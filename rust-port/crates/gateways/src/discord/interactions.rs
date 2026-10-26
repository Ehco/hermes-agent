//! Discord slash-command interactions — the portable slice of the Python
//! adapter's slash-command surface (`plugins/platforms/discord/adapter.py`).
//!
//! What ports (pure payload logic over the gateway's raw `INTERACTION_CREATE`
//! dispatch, which the Rust gateway already frames):
//! - interaction payload parsing: the fields every command handler needs
//!   (interaction id/token, type, guild/channel, the invoking user via
//!   `member.user` in guilds or `user` in DMs, and the command name +
//!   option list),
//! - the invocation-text reconstruction the Python handlers hand to
//!   `_run_simple_slash` (e.g. `"/skill deploy --all"` — command name plus
//!   the option values joined in order; string option values are used
//!   verbatim, non-scalar values JSON-encode),
//! - the ack/followup URL builders: the initial callback
//!   (`POST /interactions/{id}/{token}/callback`, the Python
//!   `interaction.response.defer/send_message`) and the webhook followup
//!   (`POST /webhooks/{app_id}/{token}/messages`), plus the deferred-ack
//!   callback payload (type 5).
//!
//! What stays deferred (discord.py-entangled): the command-tree registration
//! and diff-sync (`_safe_sync_slash_commands` over discord.py's
//! `app_commands` tree, the ~20 native commands in `_native_slash_commands`,
//! autocomplete Choices, button/picker Views) — those need discord.py's
//! `Command`/`Interaction` object model; the Rust host registers commands
//! over the REST API when it grows that surface.

use serde_json::{json, Value};

/// The initial-response callback route (`interaction.response.defer` /
/// `send_message` under discord.py).
pub fn callback_url(base: &str, interaction_id: &str, token: &str) -> String {
    format!("{base}/interactions/{interaction_id}/{token}/callback")
}

/// The followup route (a second response after the deferred ack;
/// `interaction.followup.send` under discord.py).
pub fn followup_url(base: &str, application_id: &str, token: &str) -> String {
    format!("{base}/webhooks/{application_id}/{token}/messages")
}

/// `interaction.response.defer(ephemeral=…)` — the deferred-with-source ack
/// the Python handlers issue before running a command body.
pub fn deferred_ack_payload(ephemeral: bool) -> Value {
    let mut out = json!({ "type": 5 });
    if ephemeral {
        out["data"] = json!({ "flags": 64 });
    }
    out
}

/// One parsed `APPLICATION_COMMAND` interaction.
#[derive(Debug, Clone, PartialEq)]
pub struct InteractionCommand {
    pub interaction_id: String,
    pub token: String,
    /// Discord interaction type; 2 = APPLICATION_COMMAND.
    pub kind: u8,
    pub guild_id: Option<String>,
    pub channel_id: Option<String>,
    /// The invoking user id (member.user in guilds, user in DMs).
    pub user_id: Option<String>,
    pub username: Option<String>,
    /// `data.name` — the invoked command.
    pub command_name: String,
    /// `data.options` in order: `(name, value)`.
    pub options: Vec<(String, Value)>,
}

/// Parse one `INTERACTION_CREATE` dispatch `d` payload. Returns `None` for
/// anything that is not an application command (pings, components,
/// autocomplete) or that lacks the fields a command handler needs.
pub fn parse_interaction(d: &Value) -> Option<InteractionCommand> {
    let kind = d.get("type").and_then(Value::as_u64)? as u8;
    if kind != 2 {
        return None;
    }
    let data = d.get("data")?;
    let command_name = data.get("name").and_then(Value::as_str)?.to_string();
    let token = d.get("token").and_then(Value::as_str)?.to_string();
    let user = d
        .get("member")
        .and_then(|m| m.get("user"))
        .cloned()
        .or_else(|| d.get("user").cloned());
    let options = data
        .get("options")
        .and_then(Value::as_array)
        .map(|opts| {
            opts.iter()
                .filter_map(|o| {
                    let name = o.get("name").and_then(Value::as_str)?;
                    Some((name.to_string(), o.get("value").cloned().unwrap_or(Value::Null)))
                })
                .collect()
        })
        .unwrap_or_default();
    Some(InteractionCommand {
        interaction_id: d
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        token,
        kind,
        guild_id: d
            .get("guild_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        channel_id: d
            .get("channel_id")
            .and_then(Value::as_str)
            .map(str::to_string),
        user_id: user.as_ref().and_then(|u| u.get("id")).and_then(Value::as_str).map(str::to_string),
        username: user
            .as_ref()
            .and_then(|u| u.get("username"))
            .and_then(Value::as_str)
            .map(str::to_string),
        command_name,
        options,
    })
}

impl InteractionCommand {
    /// The `"/name arg…"` invocation text the Python handlers forward into
    /// the command runner (`_run_simple_slash(interaction, "/skill {args}")`).
    /// Scalar option values join verbatim; others JSON-encode.
    pub fn invocation_text(&self) -> String {
        let mut parts = vec![format!("/{}", self.command_name)];
        for (_name, value) in &self.options {
            parts.push(match value {
                Value::String(s) => s.clone(),
                Value::Null => "None".to_string(),
                other => other.to_string(),
            });
        }
        parts.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn guild_payload() -> Value {
        json!({
            "id": "987654321",
            "token": "aW50ZXJhY3Rpb24u",
            "type": 2,
            "guild_id": "111",
            "channel_id": "222",
            "member": { "user": { "id": "333", "username": "casey" }, "roles": [] },
            "data": {
                "id": "444",
                "name": "skill",
                "type": 1,
                "options": [
                    { "name": "name", "type": 3, "value": "deploy" },
                    { "name": "args", "type": 3, "value": "run tests" }
                ]
            }
        })
    }

    #[test]
    fn parses_a_guild_command_interaction() {
        let cmd = parse_interaction(&guild_payload()).expect("parses");
        assert_eq!(cmd.kind, 2);
        assert_eq!(cmd.interaction_id, "987654321");
        assert_eq!(cmd.token, "aW50ZXJhY3Rpb24u");
        assert_eq!(cmd.guild_id.as_deref(), Some("111"));
        assert_eq!(cmd.channel_id.as_deref(), Some("222"));
        assert_eq!(cmd.user_id.as_deref(), Some("333"));
        assert_eq!(cmd.username.as_deref(), Some("casey"));
        assert_eq!(cmd.command_name, "skill");
        assert_eq!(cmd.options.len(), 2);
        assert_eq!(cmd.options[0], ("name".into(), json!("deploy")));
        assert_eq!(cmd.options[1], ("args".into(), json!("run tests")));
    }

    #[test]
    fn dm_interactions_carry_user_without_member() {
        let mut payload = guild_payload();
        payload.as_object_mut().unwrap().remove("member");
        payload
            .as_object_mut()
            .unwrap()
            .remove("guild_id");
        payload["user"] = json!({ "id": "555", "username": "dm_user" });
        let cmd = parse_interaction(&payload).expect("parses");
        assert!(cmd.guild_id.is_none());
        assert_eq!(cmd.user_id.as_deref(), Some("555"));
        assert_eq!(cmd.username.as_deref(), Some("dm_user"));
    }

    #[test]
    fn non_command_interactions_do_not_parse() {
        let mut ping = guild_payload();
        ping["type"] = json!(1);
        ping.as_object_mut().unwrap().remove("data");
        assert!(parse_interaction(&ping).is_none());
        let mut component = guild_payload();
        component["type"] = json!(3); // MESSAGE_COMPONENT (button/picker)
        assert!(parse_interaction(&component).is_none());
        // Missing token: unusable (no ack/followup possible).
        let mut tokenless = guild_payload();
        tokenless.as_object_mut().unwrap().remove("token");
        assert!(parse_interaction(&tokenless).is_none());
    }

    #[test]
    fn invocation_text_joins_option_values() {
        let cmd = parse_interaction(&guild_payload()).unwrap();
        assert_eq!(cmd.invocation_text(), "/skill deploy run tests");
        // Non-scalar option values JSON-encode; null renders as Python's None.
        let mut payload = guild_payload();
        payload["data"]["options"] = json!([
            { "name": "count", "type": 4, "value": 3 },
            { "name": "all", "type": 5, "value": true },
            { "name": "args", "type": 3 }
        ]);
        let cmd = parse_interaction(&payload).unwrap();
        assert_eq!(cmd.invocation_text(), "/skill 3 true None");
        // A command with no options is just the invocation.
        let mut bare = guild_payload();
        bare["data"]
            .as_object_mut()
            .unwrap()
            .remove("options");
        let cmd = parse_interaction(&bare).unwrap();
        assert_eq!(cmd.invocation_text(), "/skill");
    }

    #[test]
    fn ack_and_followup_routes_match_the_api_shape() {
        assert_eq!(
            callback_url("https://discord.com/api/v10", "987654321", "aW50ZXJhY3Rpb24u"),
            "https://discord.com/api/v10/interactions/987654321/aW50ZXJhY3Rpb24u/callback"
        );
        assert_eq!(
            followup_url("https://discord.com/api/v10", "app1", "aW50ZXJhY3Rpb24u"),
            "https://discord.com/api/v10/webhooks/app1/aW50ZXJhY3Rpb24u/messages"
        );
        assert_eq!(deferred_ack_payload(false), json!({ "type": 5 }));
        assert_eq!(
            deferred_ack_payload(true),
            json!({ "type": 5, "data": { "flags": 64 } })
        );
    }
}
