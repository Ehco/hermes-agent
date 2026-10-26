//! Discord markdown handling — port of the formatting bits of
//! `plugins/platforms/discord/adapter.py`.
//!
//! Discord renders standard markdown natively (`supports_code_blocks = True`,
//! `BasePlatformAdapter.format_message` default = identity), so unlike Telegram
//! or Slack there is no dialect conversion: the only transformations the Python
//! send path applies are the clickable tool-preview link format and the
//! `allowed_mentions` policy.

use regex::Regex;

/// Discord's per-message cap (`DiscordAdapter.MAX_MESSAGE_LENGTH`).
pub const MAX_MESSAGE_LENGTH: i64 = 2000;
/// Split-delivery safety ceiling (`DiscordAdapter.MAX_SPLIT_MESSAGES`, #86581):
/// chunks beyond the cap become a single truncation notice so a degenerate
/// turn can't flood the channel. The full text stays in session history.
pub const MAX_SPLIT_MESSAGES: usize = 8;

/// `format_message` is the identity for Discord (standard markdown passes
/// through; the platform renders fences/bold/lists itself).
pub fn format_message(content: &str) -> String {
    content.to_string()
}

/// Port of `_DISCORD_URL_LABEL_SCHEME_RE` (only an `http(s)://` label prefix is
/// stripped — URL-shaped labels win as a broken link) and
/// `_DISCORD_MARKDOWN_LINK_LABEL_RE` (label `[`/`]`/`\` escaped).
const LABEL_SCHEME_RE: &str = r"(?i)^https?://";
const LABEL_ESCAPE_RE: &str = r"([\\\[\]])";

/// Port of `_format_discord_markdown_link`: a Discord markdown link whose label
/// is not itself a URL, with the URL in angle brackets so Discord doesn't
/// unfurl an embed.
pub fn format_markdown_link(label: &str, url: &str) -> String {
    let scheme = Regex::new(LABEL_SCHEME_RE).unwrap();
    let label = scheme.replace(label, "").to_string();
    let esc = Regex::new(LABEL_ESCAPE_RE).unwrap();
    let escaped_label = esc.replace_all(&label, r"\$1").to_string();
    // urllib.parse.quote(url, safe=":/?#[]@!$&'*+,;=%")
    let escaped_url = percent_encode(url, ":/?#[]@!$&'*+,;=");
    format!("[{}](<{}>)", escaped_label, escaped_url)
}

fn percent_encode(s: &str, safe: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "-_.~()".contains(c) || safe.contains(c) {
            out.push(c);
        } else {
            out.push_str(&format!("%{:02X}", b));
        }
    }
    out
}

/// The default `allowed_mentions` policy (`_build_allowed_mentions`): deny
/// @everyone/@here/role pings (any LLM output with `@everyone` would otherwise
/// ping the server); user / replied-user pings stay on. Overridable in Python
/// via `discord.allow_mentions.*` — the Rust port ships the safe default.
pub fn default_allowed_mentions() -> serde_json::Value {
    serde_json::json!({
        "parse": ["users"],
        "replied_user": true,
    })
}

/// Port of `_cap_split_chunks`: cap the number of chunks sent for one logical
/// response — keep the first `MAX_SPLIT_MESSAGES - 1` and replace the rest with
/// a notice (i18n key `platform.discord.limits.response_truncated`).
pub fn cap_split_chunks(chunks: Vec<String>) -> Vec<String> {
    if chunks.len() <= MAX_SPLIT_MESSAGES {
        return chunks;
    }
    let mut kept: Vec<String> = chunks[..MAX_SPLIT_MESSAGES - 1].to_vec();
    let dropped_chars: usize = chunks[MAX_SPLIT_MESSAGES - 1..]
        .iter()
        .map(|c| c.chars().count())
        .sum();
    kept.push(format!(
        "\n\n⚠️ **Response truncated** — this reply exceeded the delivery limit ({} messages). \
         {} characters were not delivered; the full response is in the session logs.",
        MAX_SPLIT_MESSAGES, dropped_chars
    ));
    kept
}

/// The truncation notice text (used by `cap_split_chunks`; exposed for tests).
pub const TRUNCATION_NOTICE_PREFIX: &str = "\n\n⚠️ **Response truncated**";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_message_is_identity() {
        assert_eq!(format_message("**bold** `code`"), "**bold** `code`");
    }

    #[test]
    fn markdown_link_strips_label_scheme_and_escapes_label() {
        // Only the scheme is stripped (`^https?://`) — the rest of a URL-shaped
        // label stays.
        assert_eq!(
            format_markdown_link("https://example.com docs", "https://example.com/docs"),
            "[example.com docs](<https://example.com/docs>)"
        );
        // Python escapes only [ ] and \ in the label.
        assert_eq!(
            format_markdown_link("see [this] (x)", "https://e.com/a(b)"),
            "[see \\[this\\] (x)](<https://e.com/a(b)>)"
        );
        assert_eq!(
            format_markdown_link("HTTP://upper.example", "http://upper.example"),
            "[upper.example](<http://upper.example>)"
        );
    }

    #[test]
    fn percent_encodes_unsafe_chars() {
        assert_eq!(percent_encode("a b&c", "&"), "a%20b&c");
        assert_eq!(percent_encode("100%", ""), "100%25");
    }

    #[test]
    fn allowed_mentions_default_denies_everyone_and_roles() {
        let am = default_allowed_mentions();
        // everyone/roles denied (absent from `parse`), user + replied-user pings on.
        let parse = am["parse"].as_array().unwrap();
        assert_eq!(
            parse,
            &serde_json::json!(["users"]).as_array().unwrap().clone()
        );
        assert_eq!(am["replied_user"], true);
        assert!(am.get("everyone").is_none() || am["everyone"] == false);
        assert!(am.get("roles").is_none() || am["roles"] == false);
    }

    #[test]
    fn cap_split_leaves_small_deliveries_alone() {
        let chunks = vec!["a".to_string(), "b".to_string()];
        assert_eq!(cap_split_chunks(chunks.clone()), chunks);
    }

    #[test]
    fn cap_split_replaces_overflow_with_notice() {
        let chunks: Vec<String> = (0..10).map(|i| i.to_string().repeat(5)).collect();
        let capped = cap_split_chunks(chunks);
        assert_eq!(capped.len(), MAX_SPLIT_MESSAGES);
        assert!(capped.last().unwrap().starts_with(TRUNCATION_NOTICE_PREFIX));
        // 3 dropped chunks × 5 chars each = 15 dropped characters.
        assert!(capped.last().unwrap().contains("15 characters"));
        // Chunks 0..=6 kept, chunk 7 dropped.
        assert!(capped[0].contains("00000"));
        assert!(!capped[6].contains("70000"));
    }
}
