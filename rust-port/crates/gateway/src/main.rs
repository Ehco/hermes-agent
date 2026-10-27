//! hermes-gateway: the Rust gateway binary target.
//!
//! Desktop (Electron) spawn contract — the same one the Python CLI serves
//! (`hermes_cli/gateway_runtime.py`): Desktop runs
//!
//!   hermes-gateway [--profile <name-or-path>] gateway ensure --json
//!
//! as a bounded client. This binary ensures a detached gateway OWNER is
//! serving the desktop REST + `/api/ws` API on loopback and prints the
//! single-line JSON verdict the Desktop parses (`local-gateway.ts
//! parseGatewayEnsureOutput`):
//!
//!   {"state":"ready","endpoint":{"profile_id":…,"instance_id":…,
//!    "authority_epoch":1,"runtime_protocol":1,"api_origin":"http://127.0.0.1:P",
//!    "capabilities":["session-authority-v1"],"supervisor":"process",…}}
//!
//! The owner is this same binary re-exec'd detached:
//!   hermes-gateway --serve --port <P> [--home <dir>]
//! (`--port` is the supported port argument; `0`/unset picks a free one.)
//!
//! Owner identity survives restarts via `<home>/gateway-rs.json`
//! {"port","instance_id"}; a dial-able recorded owner is attached as-is.

use anyhow::{anyhow, Context, Result};
use hermes_core::config::{ProviderConfig, ProviderKind};
use hermes_core::model::{Message, ToolSpec};
use hermes_core::transport::Transport;
use hermes_gateway::{DesktopApi, RegistryHost};
use serde_json::json;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The gateway protocol version the Desktop validates
/// (`assertLocalGatewayEndpoint`: `runtime_protocol === 1`).
const RUNTIME_PROTOCOL: u64 = 1;
/// How long the ensure client waits for a freshly spawned owner to accept.
const OWNER_START_TIMEOUT: Duration = Duration::from_secs(15);
/// Per-attempt TCP connect deadline against the loopback owner.
const OWNER_PROBE_TIMEOUT: Duration = Duration::from_millis(500);

#[derive(Debug, Default)]
struct Args {
    profile: Option<String>,
    home: Option<PathBuf>,
    port: Option<u16>,
    serve: bool,
    ensure: bool,
    json: bool,
}

impl Args {
    fn parse<I: Iterator<Item = String>>(args: I) -> Result<Self> {
        let mut a = Args::default();
        let mut it = args.peekable();

        while let Some(arg) = it.next() {
            match arg.as_str() {
                "--profile" => a.profile = Some(take(&mut it, &arg)?),
                "--home" => a.home = Some(PathBuf::from(take(&mut it, &arg)?)),
                "--port" => {
                    let value = take(&mut it, &arg)?;
                    a.port = Some(
                        value
                            .parse()
                            .map_err(|_| anyhow!("--port must be a port number, got {value}"))?,
                    );
                }
                "--serve" => a.serve = true,
                "gateway" => {
                    // Subcommand form the Desktop sends: `gateway ensure`.
                    match it.next().as_deref() {
                        Some("ensure") => a.ensure = true,
                        Some(other) => {
                            return Err(anyhow!("unknown gateway verb {other} (try ensure)"))
                        }
                        None => return Err(anyhow!("gateway requires a verb (ensure)")),
                    }
                }
                "ensure" => a.ensure = true,
                "--json" => a.json = true,
                "--help" | "-h" => {
                    print_help();
                    std::process::exit(0);
                }
                other => return Err(anyhow!("unknown argument {other} (try --help)")),
            }
        }

        Ok(a)
    }
}

fn take<I: Iterator<Item = String>>(it: &mut std::iter::Peekable<I>, flag: &str) -> Result<String> {
    it.next().ok_or_else(|| anyhow!("missing value for {flag}"))
}

fn print_help() {
    println!(
        "hermes-gateway — Rust gateway for the Hermes desktop\n\n\
         USAGE:\n\
         \x20 hermes-gateway [--profile <name-or-path>] gateway ensure --json\n\
         \x20     Ensure a detached gateway owner serves on loopback; print the\n\
         \x20     single-line JSON verdict (Desktop spawn contract).\n\n\
         \x20 hermes-gateway --serve --port <N> [--home <dir>]\n\
         \x20     Run the gateway owner in this process (internal; --port 0 or an\n\
         \x20     omitted --port picks a free loopback port).\n\n\
         FLAGS:\n\
         \x20 --profile <name-or-path>  profile home: a bare name resolves under\n\
         \x20                           <home>/profiles/<name>, a path is used as-is\n\
         \x20 --home <dir>              override HERMES_HOME (default $HERMES_HOME or ~/.hermes)\n\
         \x20 --port <N>                owner port (0/unset = free loopback port)\n\
         \x20 --serve                   run the owner in this process"
    );
}

// ─── Paths ───────────────────────────────────────────────────────────────────

/// `$HERMES_HOME`, else `~/.hermes` (`~/.config`-free: matches the CLI port).
fn default_home() -> PathBuf {
    let home = std::env::var("HERMES_HOME")
        .ok()
        .or_else(|| std::env::var("HOME").ok())
        .or_else(|| std::env::var("USERPROFILE").ok())
        .unwrap_or_else(|| ".".into());
    PathBuf::from(home)
}

/// The profile home this gateway serves (`endpoint.profile_id`): the Desktop
/// mints and routes tickets against this exact path, so it must be the
/// resolved directory, not a display name.
fn profile_home(args: &Args) -> PathBuf {
    let root = args.home.clone().unwrap_or_else(default_home);
    match args.profile.as_deref() {
        None => root,
        // Python `resolve_profile_env`: the *default* profile IS the home
        // root (config.yaml at $HERMES_HOME), named profiles live under
        // profiles/<name>. A path is used as-is.
        Some("default") => root,
        Some(name) if is_bare_profile_name(name) => root.join("profiles").join(name),
        Some(path) => PathBuf::from(path),
    }
}

fn is_bare_profile_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(['/', '\\']) && name != "." && name != ".."
}

/// Owner identity state: `<home>/gateway-rs.json`.
fn state_path(home: &Path) -> PathBuf {
    home.join("gateway-rs.json")
}

#[derive(Clone)]
struct OwnerState {
    port: u16,
    instance_id: String,
}

fn read_owner_state(home: &Path) -> Option<OwnerState> {
    let text = std::fs::read_to_string(state_path(home)).ok()?;
    let value: serde_json::Value = serde_json::from_str(&text).ok()?;
    let port = value.get("port")?.as_u64()?.try_into().ok()?;
    let instance_id = value.get("instance_id")?.as_str()?.to_string();

    if instance_id.is_empty() {
        return None;
    }

    Some(OwnerState { port, instance_id })
}

fn write_owner_state(home: &Path, state: &OwnerState) -> Result<()> {
    std::fs::create_dir_all(home)?;
    let text =
        serde_json::to_string(&json!({ "port": state.port, "instance_id": state.instance_id }))?;
    // Temp + rename so a concurrent ensure never reads a half-written state.
    let tmp = state_path(home).with_extension("json.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, state_path(home))?;
    Ok(())
}

fn owner_listens(port: u16) -> bool {
    std::net::TcpStream::connect_timeout(
        &(std::net::Ipv4Addr::LOCALHOST, port).into(),
        OWNER_PROBE_TIMEOUT,
    )
    .is_ok()
}

// ─── Owner process: `--serve` ────────────────────────────────────────────────

/// Transport object-safe wrapper: the desktop API server is generic over one
/// concrete `Transport`, and the provider is chosen at startup from config.
enum AnyTransport {
    ChatCompletions(hermes_core::transports::chat_completions::ChatCompletionsTransport),
    Anthropic(hermes_core::transports::anthropic::AnthropicTransport),
}

impl Transport for AnyTransport {
    fn stream(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
    ) -> Result<
        std::pin::Pin<
            Box<dyn futures::Stream<Item = Result<hermes_core::transport::StreamItem>> + Send>,
        >,
    > {
        match self {
            AnyTransport::ChatCompletions(t) => t.stream(messages, tools),
            AnyTransport::Anthropic(t) => t.stream(messages, tools),
        }
    }
}

fn serve_owner(args: &Args) -> Result<()> {
    let port = args.port.unwrap_or(0);
    let home = profile_home(args);

    // The profile's own config.yaml (the Python strict profile scope);
    // fall back to the default path when the profile has none.
    let profile_config = home.join("config.yaml");
    let cfg = if profile_config.exists() {
        ProviderConfig::load(&profile_config)?
    } else {
        ProviderConfig::load_default()?
    };
    let transport = match cfg.kind {
        ProviderKind::ChatCompletions => AnyTransport::ChatCompletions(
            hermes_core::transports::chat_completions::ChatCompletionsTransport::new(cfg.clone()),
        ),
        ProviderKind::Anthropic => AnyTransport::Anthropic(
            hermes_core::transports::anthropic::AnthropicTransport::new(cfg.clone()),
        ),
    };

    let api = DesktopApi::new(
        std::sync::Arc::new(transport),
        std::sync::Arc::new(RegistryHost::default()),
    )
    // Sessions persist as JSONL under the profile home and are restored on
    // owner start, so the sidebar survives owner restarts.
    .with_store_dir(home.join("sessions"))
    // The setup readiness RPCs read the same provider config the
    // transport was built from (Python `session_readiness`).
    .with_provider(cfg.clone())
    // Skills/MCP read the profile's real dirs/config: the desktop's
    // Capabilities view lists the same servers the Python app does.
    .with_skills_root(home.join("skills"))
    .with_mcp_servers(mcp_servers_from_config(&home))
    .with_config_path(home.join("config.yaml"))
    // The normal desktop's session store — merged read-only so both apps
    // see the same chats.
    .with_state_db(home.join("state.db"));

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
            .await
            .with_context(|| format!("binding 127.0.0.1:{port}"))?;
        eprintln!("[hermes-gateway] serving on 127.0.0.1:{port}");
        axum::serve(listener, api.router()).await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

// ─── Ensure client (the Desktop spawn contract) ──────────────────────────────

fn spawn_detached_owner(exe: &Path, home: &Path, port: u16) -> Result<std::process::Child> {
    // The owner's diagnostics survive the client: append to a log under the
    // profile home (this is where a failed start names its cause).
    std::fs::create_dir_all(home)?;
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(home.join("gateway-owner.log"))?;

    let mut cmd = std::process::Command::new(exe);
    cmd.arg("--serve")
        .arg("--port")
        .arg(port.to_string())
        .arg("--home")
        .arg(home)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(
            log.try_clone().context("cloning the owner log handle")?,
        ))
        .stderr(std::process::Stdio::from(log));

    #[cfg(windows)]
    {
        // CREATE_NEW_PROCESS_GROUP: the owner outlives this client (which
        // Desktop's bounded runner may kill at any time) and no Ctrl event
        // reaches it. Plain DETACHED_PROCESS is avoided deliberately: a
        // windows-gnu console binary hangs before main() without a console.
        const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP);
    }

    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    cmd.spawn()
        .map_err(|e| anyhow!("spawning the gateway owner: {e}"))
}

/// One ready line on stdout + exit 0; on failure a `state != ready` verdict
/// whose `detail` names the cause (Desktop surfaces that verbatim).
fn print_verdict(payload: serde_json::Value) {
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{payload}");
    let _ = out.flush();
}

fn fail_verdict(detail: &str) -> ! {
    print_verdict(json!({
        "state": "error",
        "reason_code": "ensure_failed",
        "detail": detail,
    }));
    std::process::exit(0);
}

/// Ensure loop: spawn the owner, then wait for either the port to answer
/// (success — write identity state and print the ready verdict) or the owner
/// to exit early (failure — the exit status goes into the verdict detail).
fn ensure(args: &Args, exe: &Path) -> Result<()> {
    let home = profile_home(args);
    std::fs::create_dir_all(&home)
        .with_context(|| format!("creating profile home {}", home.display()))?;

    // 1. A recorded owner that still answers is attached as-is.
    if let Some(state) = read_owner_state(&home) {
        if owner_listens(state.port) {
            print_verdict(ready_verdict(&home, &state));
            return Ok(());
        }
    }

    // 2. Start (or replace): pick a free loopback port by binding and
    //    releasing, then re-exec this binary detached as the owner.
    let port = free_loopback_port()?;
    let mut owner = spawn_detached_owner(exe, &home, port)?;

    let deadline = std::time::Instant::now() + OWNER_START_TIMEOUT;

    while std::time::Instant::now() < deadline {
        if owner_listens(port) {
            let state = OwnerState {
                port,
                instance_id: hermes_gateway::desktop::new_id("gwinst"),
            };
            write_owner_state(&home, &state)?;
            print_verdict(ready_verdict(&home, &state));

            // The child handle is dropped here; the owner outlives this
            // client by construction (detached / new process group).
            let _ = owner.try_wait();
            return Ok(());
        }

        // An owner that died before accepting names its exit in the verdict.
        if let Some(status) = owner.try_wait()? {
            fail_verdict(&format!(
                "the gateway owner exited before accepting on port {port} ({status}); see gateway-owner.log under the profile home"
            ));
        }

        std::thread::sleep(Duration::from_millis(100));
    }

    fail_verdict("the gateway owner did not accept on its port within the startup deadline; check the gateway owner logs under the profile home");
}

fn ready_verdict(home: &Path, state: &OwnerState) -> serde_json::Value {
    json!({
        "state": "ready",
        "endpoint": {
            "profile_id": home.display().to_string(),
            "instance_id": state.instance_id,
            "authority_epoch": 1,
            "runtime_protocol": RUNTIME_PROTOCOL,
            "api_origin": format!("http://127.0.0.1:{}", state.port),
            "capabilities": ["session-authority-v1"],
            "supervisor": "process",
            "control_home": serde_json::Value::Null,
            "code_sha": serde_json::Value::Null,
        },
    })
}

/// Bind 127.0.0.1:0, read the assigned port, drop. The owner then binds the
/// same port: a small race the readiness poll tolerates.
fn free_loopback_port() -> Result<u16> {
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    let port = listener.local_addr()?.port();
    drop(listener);
    Ok(port)
}

// ─── main ────────────────────────────────────────────────────────────────────

fn run() -> Result<()> {
    let args = Args::parse(std::env::args().skip(1))?;

    // `--serve` wins: the owner re-exec form (also runnable directly).
    if args.serve {
        return serve_owner(&args);
    }

    // The Desktop spawn contract: `gateway ensure --json`.
    if args.ensure {
        let exe = std::env::current_exe().context("resolving the gateway binary path")?;
        return ensure(&args, &exe);
    }

    // No verb: a plain status probe (human/CI convenience, not the contract).
    let home = args.home.clone().unwrap_or_else(default_home);

    if let Some(state) = read_owner_state(&home).filter(|s| owner_listens(s.port)) {
        println!(
            "gateway ready on 127.0.0.1:{} (instance {})",
            state.port, state.instance_id
        );
    } else {
        println!("gateway not running");
    }

    Ok(())
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e:#}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_profile_names_join_the_profiles_dir() {
        let args = Args {
            profile: Some("work".into()),
            ..Default::default()
        };
        assert_eq!(
            profile_home(&args),
            default_home().join("profiles").join("work")
        );
    }

    #[test]
    fn profile_paths_are_homes() {
        let args = Args {
            profile: Some("C:\\homes\\work".into()),
            ..Default::default()
        };
        assert_eq!(profile_home(&args), PathBuf::from("C:\\homes\\work"));
    }

    #[test]
    fn no_profile_is_the_root_home() {
        let args = Args {
            home: Some(PathBuf::from("/h")),
            ..Default::default()
        };
        assert_eq!(profile_home(&args), PathBuf::from("/h"));
    }
}

/// config.yaml `mcp_servers:` → the desktop's /api/mcp/servers entries
/// (`{name, enabled, transport: "stdio", command, args}`), the same records
/// the Python webapp serves from the profile config.
fn mcp_servers_from_config(home: &std::path::Path) -> Vec<serde_json::Value> {
    let Some(doc) =
        ProviderConfig::load_document(&home.join("config.yaml")).ok().flatten()
    else {
        return Vec::new();
    };
    let Some(servers) = doc.get("mcp_servers").and_then(|m| m.as_mapping()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(key, spec)| {
            let name = key.as_str()?.to_string();
            let command = spec.get("command")?.as_str()?.to_string();
            let args: Vec<String> = spec
                .get("args")
                .and_then(|a| a.as_sequence())
                .map(|seq| {
                    seq.iter()
                        .filter_map(|v| v.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            let enabled = spec.get("enabled").and_then(|e| e.as_bool()).unwrap_or(true);
            Some(
                serde_json::json!({
                    "name": name, "enabled": enabled,
                    "transport": "stdio", "command": command, "args": args,
                }),
            )
        })
        .collect()
}
