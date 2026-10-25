//! hermes-rs: terminal front-end for the Rust agent.
//!
//! REPL: loads the provider config (config.yaml + CLI flags), streams assistant
//! output live, executes tool calls, and persists the conversation to a JSONL
//! session store so a restart resumes where it left off.

use anyhow::{anyhow, Result};
use futures::StreamExt;
use hermes_core::agent::ToolHost;
use hermes_core::config::ProviderConfig;
use hermes_core::model::{Content, Message, Role, ToolSpec};
use hermes_core::transport::{StreamItem, Transport};
use hermes_session::SessionStore;
use serde_json::{json, Value};
use std::future::Future;
use std::io::{BufRead, Write};
use std::path::PathBuf;

mod tool_bridge;
use tool_bridge::bridge;

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
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown flag {other} (try --help)")),
            }
        }
        Ok(f)
    }
}

fn print_help() {
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
         \x20 /help  show commands\n\
         \x20 /new   clear the session history\n\
         \x20 /quit  exit"
    );
}

// ─── Tool host: delegates into the hermes-tools registry via tool_bridge ─────

pub struct CliToolHost;

/// The JSON Schemas for the tools the bridge implements.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "terminal".into(),
            description: "Run a shell command and return its output.".into(),
            schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        },
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file (1-indexed line numbers).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                },
                "required": ["path"],
            }),
        },
        ToolSpec {
            name: "list_dir".into(),
            description: "List a directory's entries with sizes.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
            }),
        },
    ]
}

impl ToolHost for CliToolHost {
    fn execute(&self, name: &str, arguments: &str) -> impl Future<Output = Result<String>> + Send {
        let name = name.to_string();
        async move {
            let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            Ok(bridge(&name, &args))
        }
    }
}

// ─── Streaming turn loop (same contract as core::agent::run_turn, but with
//     live output of text deltas as they arrive) ─────────────────────────────

async fn run_turn_live(
    transport: &dyn Transport,
    tools: &[ToolSpec],
    history: &mut Vec<Message>,
    store: &SessionStore,
) -> Result<String> {
    loop {
        let mut stream = transport.stream(history, tools)?;
        let mut text = String::new();
        let mut calls: Vec<(String, String, String)> = Vec::new();

        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::TextDelta(d) => {
                    print!("{d}");
                    std::io::stdout().flush().ok();
                    text.push_str(&d);
                }
                StreamItem::ToolCall { id, name, arguments } => {
                    calls.push((id, name, arguments));
                }
                StreamItem::Done => break,
            }
        }
        println!();

        let mut assistant = Message { role: Role::Assistant, content: Vec::new() };
        if !text.is_empty() {
            assistant.content.push(Content::Text { text: text.clone() });
        }
        for (id, name, arguments) in &calls {
            assistant.content.push(Content::ToolUse {
                call: hermes_core::ToolCall {
                    id: id.clone(),
                    name: name.clone(),
                    arguments: arguments.clone(),
                },
            });
        }
        store.append(&assistant)?;
        history.push(assistant);

        if calls.is_empty() {
            return Ok(text);
        }

        let host = CliToolHost;
        for (id, name, arguments) in calls {
            let output = host
                .execute(&name, &arguments)
                .await
                .unwrap_or_else(|e| format!("tool error: {e}"));
            let msg = Message {
                role: Role::Tool,
                content: vec![Content::ToolResult { call_id: id, output }],
            };
            store.append(&msg)?;
            history.push(msg);
        }
    }
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
    let tools = tool_specs();

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

    let stdin = std::io::stdin();
    let mut input = String::new();
    loop {
        print!("› ");
        std::io::stdout().flush().ok();
        input.clear();
        if stdin.lock().read_line(&mut input)? == 0 {
            break; // EOF
        }
        let line = input.trim();
        match line {
            "" => continue,
            "/quit" | "/exit" | "/q" => break,
            "/help" => print_help(),
            "/new" => {
                store.clear()?;
                history.clear();
                history.push(Message::text(Role::System, SYSTEM_PROMPT));
                println!("(new session)");
                continue;
            }
            slash if slash.starts_with('/') => {
                println!("unknown command {slash} — try /help");
                continue;
            }
            user_input => {
                let user = Message::text(Role::User, user_input);
                store.append(&user)?;
                history.push(user);
                if let Err(e) = run_turn_live(&*transport, &tools, &mut history, &store).await {
                    eprintln!("\nerror: {e:#}");
                }
            }
        }
    }
    println!("bye");
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}
