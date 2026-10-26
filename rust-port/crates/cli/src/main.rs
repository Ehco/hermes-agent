//! hermes-rs: terminal front-end for the Rust agent.
//!
//! REPL: loads the provider config (config.yaml + CLI flags), streams assistant
//! output live, executes tool calls, and persists the conversation to a JSONL
//! session store so a restart resumes where it left off.

use anyhow::{anyhow, Result};
use hermes_core::config::ProviderConfig;
use hermes_core::model::{Message, Role};
use hermes_session::SessionStore;
use repl::Repl;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

mod multiline;
mod repl;
mod session_switch;
mod slash;
mod tool_bridge;

// ─── CLI flags ───────────────────────────────────────────────────────────────

#[derive(Debug, Default)]
struct Flags {
    config: Option<String>,
    model: Option<String>,
    provider: Option<String>,
    base_url: Option<String>,
    api_key_env: Option<String>,
    api_key: Option<String>,
    session: Option<String>,
}

impl Flags {
    fn parse(args: impl Iterator<Item = String>) -> Result<Self> {
        let mut f = Flags::default();
        let mut it = args;
        while let Some(arg) = it.next() {
            let mut take = || it.next().ok_or_else(|| anyhow!("missing value for {arg}"));
            match arg.as_str() {
                "--model" => f.model = Some(take()?),
                "--provider" => f.provider = Some(take()?),
                "--base-url" => f.base_url = Some(take()?),
                "--api-key-env" => f.api_key_env = Some(take()?),
                "--api-key" => f.api_key = Some(take()?),
                "--config" => f.config = Some(take()?),
                "--session" => f.session = Some(take()?),
                "--help" | "-h" => {
                    print_flags_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown flag {other} (try --help)")),
            }
        }
        Ok(f)
    }
}

fn print_flags_help() {
    println!(
        "hermes-rs — Rust port of the hermes-agent CLI\n\n\
         FLAGS:\n\
         \x20 --config <path>      config.yaml path (default $HERMES_CONFIG or ~/.hermes/config.yaml)\n\
         \x20 --model <model>      override the model\n\
         \x20 --provider <name>    override the provider (\"anthropic\" = Anthropic wire, else chat completions)\n\
         \x20 --base-url <url>     override the provider base URL\n\
         \x20 --api-key-env <var>  env var holding the API key (default OPENAI_API_KEY / ANTHROPIC_API_KEY)\n\
         \x20 --api-key <key>      inline API key (env var still wins)\n\
         \x20 --session <path>     JSONL session file (default ~/.hermes/sessions/cli-rs.jsonl)\n\n\
         SLASH COMMANDS:\n\
         \x20 type /help inside the REPL for the command table"
    );
}

// ─── REPL ─────────────────────────────────────────────────────────────────────

fn session_path(flags: &Flags) -> PathBuf {
    if let Some(p) = &flags.session {
        return PathBuf::from(p);
    }
    let home = std::env::var("HERMES_HOME")
        .or_else(|_| std::env::var("HOME").or_else(|_| std::env::var("USERPROFILE")))
        .unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".hermes").join("sessions").join("cli-rs.jsonl")
}

const SYSTEM_PROMPT: &str =
    "You are Hermes, a helpful agent running from the Rust CLI port.";

async fn run() -> Result<()> {
    let flags = Flags::parse(std::env::args().skip(1))?;

    let mut cfg = match &flags.config {
        Some(p) => ProviderConfig::load(std::path::Path::new(p)).unwrap_or_else(|e| {
            eprintln!("warning: {e:#}; using provider defaults");
            ProviderConfig::new(hermes_core::config::ProviderKind::ChatCompletions)
        }),
        None => ProviderConfig::load_default()?,
    };
    // Flags override config.
    if let Some(m) = &flags.model { cfg.model = m.clone(); }
    if let Some(p) = &flags.provider {
        let is_anthropic = p == "anthropic" || p == "claude";
        cfg.kind = hermes_core::config::ProviderKind::from_api_mode(if is_anthropic {
            "anthropic_messages"
        } else {
            "chat_completions"
        });
        if is_anthropic && cfg.api_key_env == "OPENAI_API_KEY" {
            cfg.api_key_env = "ANTHROPIC_API_KEY".into();
        }
    }
    if let Some(b) = &flags.base_url { cfg.base_url = b.clone(); }
    if let Some(k) = &flags.api_key_env { cfg.api_key_env = k.clone(); }
    if let Some(k) = &flags.api_key { cfg.api_key = Some(k.clone()); }

    let transport = cfg.build_transport()?;
    let tools = repl::tool_specs();

    let store = SessionStore::open(session_path(&flags))?;
    let mut history: Vec<Message> = store.load()?;
    if history.is_empty() {
        history.push(Message::text(Role::System, SYSTEM_PROMPT));
    }

    println!(
        "hermes-rs | {} via {} ({}) | {} messages resumed | /help for commands",
        cfg.model,
        if cfg.kind == hermes_core::config::ProviderKind::Anthropic {
            "anthropic"
        } else {
            "chat_completions"
        },
        cfg.base_url,
        history.len(),
    );

    let io: Arc<Mutex<dyn repl::ReplIo>> = Arc::new(Mutex::new(repl::StdinIo));
    let mut repl = Repl::new(
        io,
        transport,
        tools,
        store,
        history,
        cfg.model.clone(),
        SYSTEM_PROMPT.into(),
    );
    repl.run().await
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
