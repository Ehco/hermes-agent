//! hermes-tui binary: launches the full-screen TUI (falling back to the
//! line REPL when stdin is not a TTY), reusing the CLI crate's config
//! loading, tool specs, and session store.

use anyhow::{anyhow, Result};
use hermes_cli::repl::tool_specs;
use hermes_core::config::ProviderConfig;
use hermes_core::model::{Message, Role};
use hermes_session::SessionStore;
use std::path::PathBuf;

const SYSTEM_PROMPT: &str =
    "You are Hermes, a helpful agent running from the Rust CLI port.";

struct Flags {
    config: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    base_url: Option<String>,
    api_key: Option<String>,
    api_key_env: Option<String>,
    session: Option<String>,
}

impl Flags {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut f = Flags {
            config: None,
            model: None,
            provider: None,
            base_url: None,
            api_key: None,
            api_key_env: None,
            session: None,
        };
        let mut it = args;
        while let Some(arg) = it.next() {
            let mut take = || it.next().ok_or_else(|| anyhow!("missing value for {arg}"));
            match arg.as_str() {
                "--config" => f.config = Some(take()?),
                "--model" => f.model = Some(take()?),
                "--provider" => f.provider = Some(take()?),
                "--base-url" => f.base_url = Some(take()?),
                "--api-key" => f.api_key = Some(take()?),
                "--api-key-env" => f.api_key_env = Some(take()?),
                "--session" => f.session = Some(take()?),
                "--help" | "-h" => {
                    println!(
                        "hermes-tui — full-screen TUI for the Rust agent\n\n\
                         FLAGS:\n\
                         \x20 --config <path>      config.yaml path (default ~/.hermes/config.yaml)\n\
                         \x20 --model <model>      override the model\n\
                         \x20 --provider <name>    override the provider\n\
                         \x20 --base-url <url>     override the provider base URL\n\
                         \x20 --api-key <key>      inline API key\n\
                         \x20 --api-key-env <var>  env var holding the API key\n\
                         \x20 --session <path>     JSONL session file"
                    );
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown flag {other} (try --help)")),
            }
        }
        Ok(f)
    }
}

fn session_path(flags: &Flags) -> PathBuf {
    if let Some(p) = &flags.session {
        return PathBuf::from(p);
    }
    let home = std::env::var("HERMES_HOME")
        .or_else(|_| std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".hermes")
        .join("sessions")
        .join("tui-rs.jsonl")
}

async fn run() -> Result<()> {
    let flags = Flags::parse(std::env::args().skip(1))?;

    let mut cfg = match &flags.config {
        Some(p) => ProviderConfig::load(std::path::Path::new(p)).unwrap_or_else(|e| {
            eprintln!("warning: {e:#}; using provider defaults");
            ProviderConfig::new(hermes_core::config::ProviderKind::ChatCompletions)
        }),
        None => ProviderConfig::load_default()?,
    };
    if let Some(m) = &flags.model {
        cfg.model = m.clone();
    }
    if let Some(p) = &flags.provider {
        let is_anthropic = p == "anthropic" || p == "claude";
        cfg.kind = hermes_core::config::ProviderKind::from_api_mode(if is_anthropic {
            "anthropic_messages"
        } else {
            "chat_completions"
        });
    }
    if let Some(b) = &flags.base_url {
        cfg.base_url = b.clone();
    }
    if let Some(k) = &flags.api_key_env {
        cfg.api_key_env = k.clone();
    }
    if let Some(k) = &flags.api_key {
        cfg.api_key = Some(k.clone());
    }

    let transport = cfg.build_transport()?;
    let store = SessionStore::open(session_path(&flags))?;
    let mut history: Vec<Message> = store.load()?;
    if history.is_empty() {
        history.push(Message::text(Role::System, SYSTEM_PROMPT));
    }

    hermes_tui::app::run(
        transport,
        tool_specs(),
        store,
        history,
        cfg.model.clone(),
        SYSTEM_PROMPT.into(),
    )
    .await
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
