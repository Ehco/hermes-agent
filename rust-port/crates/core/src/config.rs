//! Provider configuration (port of the Python provider/profile selection).
//!
//! Shapes are parsed from the user's `config.yaml`:
//!
//! ```yaml
//! model:
//!   default: "glm-5.3-flash"     # (alias: `model:`)
//!   provider: "custom"           # "anthropic" -> Anthropic Messages wire, anything else -> chat_completions
//!   base_url: "http://host:8080/v1"
//!   api_key: "..."               # inline fallback; env var wins
//! providers:
//!   my-gateway:
//!     base_url: "https://gateway.example.com/v1"
//!     api_mode: anthropic_messages
//!     key_env: "MY_PROXY_API_KEY"
//! ```
//!
//! Keys NEVER live in code: they come from the environment (`api_key_env`),
//! with an optional inline `api_key` from config as fallback.

use crate::transport::Transport;
use crate::transports::anthropic::AnthropicTransport;
use crate::transports::chat_completions::ChatCompletionsTransport;
use anyhow::{anyhow, Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;
use std::env;
use std::path::PathBuf;

/// Which wire format the provider speaks (Python `api_mode`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderKind {
    /// OpenAI-compatible `/chat/completions`.
    ChatCompletions,
    /// Native Anthropic Messages `/v1/messages`.
    Anthropic,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub base_url: String,
    /// Name of the environment variable holding the API key.
    pub api_key_env: String,
    /// Inline key from config; used only when the env var is unset.
    pub api_key: Option<String>,
    pub model: String,
    pub kind: ProviderKind,
    /// The configured `model.provider` name ("custom", "openrouter", …) —
    /// Python `runtime.get('provider')` in the readiness payload.
    pub name: Option<String>,
}

impl ProviderConfig {
    pub fn new(kind: ProviderKind) -> Self {
        let (base_url, api_key_env) = match kind {
            ProviderKind::ChatCompletions => {
                ("https://api.openai.com/v1".into(), "OPENAI_API_KEY".into())
            }
            ProviderKind::Anthropic => (
                "https://api.anthropic.com".into(),
                "ANTHROPIC_API_KEY".into(),
            ),
        };
        Self {
            base_url,
            api_key_env,
            api_key: None,
            model: String::new(),
            kind,
            name: None,
        }
    }

    /// API key for requests: the configured env var first, then the inline
    /// config value. Never hardcoded.
    pub fn resolve_api_key(&self) -> Result<String> {
        if let Ok(key) = env::var(&self.api_key_env) {
            if !key.trim().is_empty() {
                return Ok(key);
            }
        }
        if let Some(key) = &self.api_key {
            if !key.trim().is_empty() {
                return Ok(key.clone());
            }
        }
        Err(anyhow!(
            "no API key: set {} (or an inline api_key in config)",
            self.api_key_env
        ))
    }

    /// Build the transport for this provider.
    pub fn build_transport(&self) -> Result<Box<dyn Transport>> {
        match self.kind {
            ProviderKind::ChatCompletions => {
                Ok(Box::new(ChatCompletionsTransport::new(self.clone())))
            }
            ProviderKind::Anthropic => Ok(Box::new(AnthropicTransport::new(self.clone()))),
        }
    }

    /// Default config file: `$HERMES_HOME/config.yaml` (default `~/.hermes/config.yaml`).
    pub fn default_config_path() -> Option<PathBuf> {
        if let Ok(p) = env::var("HERMES_CONFIG") {
            return Some(PathBuf::from(p));
        }
        let home = env::var("HERMES_HOME")
            .ok()
            .or_else(|| env::var("HOME").ok())
            .or_else(|| env::var("USERPROFILE").ok())?;
        Some(PathBuf::from(home).join(".hermes").join("config.yaml"))
    }

    /// Load from the default config path. Missing file -> provider defaults.
    pub fn load_default() -> Result<Self> {
        match Self::default_config_path() {
            Some(path) if path.exists() => Self::load(&path),
            _ => Ok(Self::new(ProviderKind::ChatCompletions)),
        }
    }

    /// The raw config document (mcp_servers, agents, … beyond the provider
    /// scope). `Ok(None)` when the file is missing.
    pub fn load_document(path: &std::path::Path) -> Result<Option<serde_yaml::Value>> {
        match std::fs::read_to_string(path) {
            Ok(text) => Ok(Some(
                serde_yaml::from_str(&text).with_context(|| "parsing config.yaml".to_string())?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(anyhow!(e).context(format!("reading config {}", path.display()))),
        }
    }

    /// Parse the user's config.yaml.
    pub fn load(path: &std::path::Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading config {}", path.display()))?;
        Self::from_yaml_str(&text)
    }

    pub fn from_yaml_str(text: &str) -> Result<Self> {
        let file: ConfigFile =
            serde_yaml::from_str(text).with_context(|| "parsing config.yaml".to_string())?;
        let model = file.model.unwrap_or_default();

        let provider_name = model
            .provider
            .unwrap_or_else(|| "auto".into())
            .to_lowercase();
        let mut cfg = Self::new(ProviderKind::ChatCompletions);

        // Built-in provider names select the wire directly.
        if matches!(provider_name.as_str(), "anthropic" | "claude") {
            cfg.kind = ProviderKind::Anthropic;
        }
        // A provider name that matches a `providers:` entry picks up that entry
        // (base_url / keys / model / api_mode), then top-level `model:` keys win.
        if let Some(entry_name) = named_entry_name(&provider_name) {
            if let Some(entry) = file.providers.get(entry_name) {
                entry.apply(&mut cfg);
                if let Some(mode) = &entry.api_mode {
                    cfg.kind = ProviderKind::from_api_mode(mode);
                }
            }
        }

        // Top-level `model:` keys override the named-provider defaults.
        if let Some(base) = model.base_url {
            cfg.base_url = base;
        }
        if let Some(key) = model.api_key {
            cfg.api_key = Some(key);
        }
        if let Some(env_name) = model.api_key_env {
            cfg.api_key_env = env_name;
        }
        if let Some(m) = model.default.or(model.model) {
            cfg.model = m;
        }
        if let Some(mode) = model.api_mode {
            cfg.kind = ProviderKind::from_api_mode(&mode);
        }

        if cfg.model.is_empty() {
            cfg.model = env::var("HERMES_MODEL").unwrap_or_else(|_| "gpt-4o-mini".into());
        }
        cfg.name = Some(provider_name.clone());
        Ok(cfg)
    }
}

fn named_entry_name(provider: &str) -> Option<&str> {
    // "custom" and "auto" don't name a providers: entry.
    match provider {
        "custom" | "auto" | "" => None,
        other => Some(other),
    }
}

impl ProviderKind {
    /// Python api_mode strings -> wire kind (default: chat_completions).
    pub fn from_api_mode(mode: &str) -> Self {
        match mode.trim().to_lowercase().as_str() {
            "anthropic_messages" | "anthropic" => ProviderKind::Anthropic,
            _ => ProviderKind::ChatCompletions,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ModelSection {
    default: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    api_mode: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct ProviderEntry {
    base_url: Option<String>,
    api_key: Option<String>,
    key_env: Option<String>,
    api_key_env: Option<String>,
    api_mode: Option<String>,
    model: Option<String>,
}

impl ProviderEntry {
    fn apply(&self, cfg: &mut ProviderConfig) {
        if let Some(base) = &self.base_url {
            cfg.base_url = base.clone();
        }
        if let Some(key) = &self.api_key {
            cfg.api_key = Some(key.clone());
        }
        if let Some(env_name) = self.key_env.as_ref().or(self.api_key_env.as_ref()) {
            cfg.api_key_env = env_name.clone();
        }
        if let Some(m) = &self.model {
            cfg.model = m.clone();
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    model: Option<ModelSection>,
    #[serde(default)]
    providers: BTreeMap<String, ProviderEntry>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_model_section_and_defaults() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  default: \"glm-5.3-flash\"\n  provider: \"custom\"\n  base_url: \"http://192.168.1.189:8080/v1\"\n  api_key: \"abc\"\n",
        )
        .unwrap();
        assert_eq!(cfg.model, "glm-5.3-flash");
        assert_eq!(cfg.base_url, "http://192.168.1.189:8080/v1");
        assert_eq!(cfg.kind, ProviderKind::ChatCompletions);
        assert_eq!(cfg.resolve_api_key().unwrap(), "abc");
    }

    #[test]
    fn anthropic_provider_and_named_entries() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  default: claude-fable-5\n  provider: dbx-claude\nproviders:\n  dbx-claude:\n    base_url: https://gw.example.com/anthropic\n    api_mode: anthropic_messages\n    key_env: GW_KEY\n",
        )
        .unwrap();
        assert_eq!(cfg.kind, ProviderKind::Anthropic);
        assert_eq!(cfg.base_url, "https://gw.example.com/anthropic");
        assert_eq!(cfg.api_key_env, "GW_KEY");
        assert_eq!(cfg.model, "claude-fable-5");
    }

    #[test]
    fn key_env_wins_over_inline_and_env_var_wins_over_both() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  api_key: inline\n  api_key_env: TEST_HERMES_KEY\n",
        )
        .unwrap();
        assert_eq!(cfg.api_key.as_deref(), Some("inline"));
        assert_eq!(cfg.api_key_env, "TEST_HERMES_KEY");
        // Env var unset here: falls back to inline.
        assert_eq!(cfg.resolve_api_key().unwrap(), "inline");
    }

    #[test]
    fn api_mode_selects_anthropic() {
        let cfg = ProviderConfig::from_yaml_str(
            "model:\n  default: m\n  provider: custom\n  base_url: http://x/v1\n  api_mode: anthropic_messages\n",
        )
        .unwrap();
        assert_eq!(cfg.kind, ProviderKind::Anthropic);
    }
}
