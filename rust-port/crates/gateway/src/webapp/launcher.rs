//! How the Webapp picks its address and how `hermes webapp` drives it: the
//! launcher half of `hermes_cli/webapp_launcher.py` and
//! `hermes_cli/webapp_access.py::_port_to_serve`, plus the CLI entrypoint the
//! `crates/cli` port wires up (`pub fn cli_entrypoint` — see the module-tail
//! docs for the calling contract).

use std::path::Path;

use super::settings::{
    bundle_missing_reason, dial_origin, is_wildcard, local_only, webapp_settings, WebappSettings,
    DEFAULT_PORT,
};

/// How many ports above the default the launcher tries for a Webapp that has
/// no `webapp.port` of its own (`_PORT_SEARCH`).
pub const PORT_SEARCH: u16 = 100;

/// Whether a listener could bind *host*:*port* now (`_port_free`): a plain
/// bind probe, dropped before the gateway binds for real — a small race the
/// gateway's own bind error surfaces.
pub fn port_free(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let target = format!("{host}:{port}");
    match target.to_socket_addrs() {
        Ok(addrs) => match addrs.into_iter().next() {
            Some(addr) => std::net::TcpListener::bind(addr).is_ok(),
            None => false,
        },
        Err(_) => false,
    }
}

/// `(port, port to write)` for the Webapp of *home*'s gateway, bound on *host*
/// (`_port_to_serve`): a configured `webapp.port` is the user's and is kept
/// (`None` — nothing to write); a home with none gets the first port above the
/// default that is free now.
///
/// Deferral: the Python version also excludes the ports other Hermes profile
/// homes claim (each profile's `webapp.port`) under a cross-process file lock.
/// The Rust gateway does not implement the multi-profile claimed-port scan
/// (there is no per-profile gateway runtime yet), so only this home's own
/// config and the default port are considered.
pub fn port_to_serve(home: &Path, host: &str) -> (u16, Option<u16>) {
    let settings = webapp_settings(home);
    if settings.port_configured {
        return (settings.port, None);
    }
    for port in (DEFAULT_PORT + 1)..=(DEFAULT_PORT + PORT_SEARCH) {
        if port_free(host, port) {
            return (port, Some(port));
        }
    }
    (settings.port, None)
}

/// The Webapp access shape the settings describe
/// (`webapp_access._mode`): `public` (its own URL), `lan` (a wildcard bind),
/// `local` (a loopback bind), or `None` when it is off.
pub fn access_mode(settings: &WebappSettings) -> Option<&'static str> {
    if !settings.enabled {
        return None;
    }
    if !settings.public_url.is_empty() {
        Some("public")
    } else if is_wildcard(&settings.host) {
        Some("lan")
    } else {
        Some("local")
    }
}

/// The gateway listener's plan for serving *home* (`listener_plan`): the bind
/// address with the Webapp on (else a private loopback listener on an
/// ephemeral port), whether a mount is wanted, and the bundle-missing reason
/// the listener serves instead of a bundle.
pub struct ListenerPlan {
    pub host: String,
    pub port: u16,
    /// Whether the Webapp is on (a mount is served from `dist`).
    pub enabled: bool,
    /// Why a `webapp:` block that asks for the Webapp cannot be used.
    pub reason: Option<String>,
    /// The bundle dir to serve; `None` keeps the listener private.
    pub dist: Option<std::path::PathBuf>,
    /// The `webapp.public_url`, `""` for none.
    pub public_url: String,
}

impl ListenerPlan {
    /// `(host, port)` the listener binds (`WebappSettings::bind`).
    pub fn bind(&self) -> (String, u16) {
        (self.host.clone(), self.port)
    }

    /// The loopback origin native clients dial (`dial_origin`).
    pub fn native_origin(&self) -> String {
        dial_origin(&self.host, self.port)
    }

    /// Why the bundle cannot be served, or `None` once it is built
    /// (`bundle_missing_reason`); `None` when no mount is planned.
    pub fn bundle_missing_reason(&self) -> Option<String> {
        self.dist.as_ref().and_then(|d| bundle_missing_reason(d))
    }
}

/// The listener plan for *home*: enabled with the configured address and the
/// bundle dir from `webapp.enabled`, else a private ephemeral loopback
/// listener. (The Rust port takes the bundle dir as `dist_hint` because it has
/// no checkout-layout `get_project_root()`; the CLI caller passes
/// `<project>/apps/desktop/dist-webapp` when it exists.)
pub fn listener_plan(home: &Path, dist_hint: Option<std::path::PathBuf>) -> ListenerPlan {
    let settings = webapp_settings(home);
    if settings.enabled {
        ListenerPlan {
            host: settings.host.clone(),
            port: settings.port,
            enabled: true,
            reason: settings.reason.clone(),
            dist: dist_hint,
            public_url: settings.public_url.clone(),
        }
    } else {
        ListenerPlan {
            host: "127.0.0.1".into(),
            port: 0,
            enabled: false,
            reason: settings.reason.clone(),
            dist: None,
            public_url: String::new(),
        }
    }
}

/// Whether the sign-in gate engages on this bind (`should_require_auth` +
/// `webapp.public_url` semantics): a wildcard or non-local-only host, or a
/// configured public URL on a loopback bind.
pub fn auth_required(settings: &WebappSettings) -> bool {
    if !settings.enabled {
        return false;
    }
    !local_only(&settings.host) || !settings.public_url.is_empty()
}

// ── the `hermes webapp` CLI entrypoint ───────────────────────────────────────

/// The `hermes webapp` subcommands the Rust port implements.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WebappCommand {
    /// `--status`: what the Webapp would serve (or why it is off).
    Status,
    /// `--sign-out`: replace the stored browser credential; every open tab
    /// must sign in again when the gateway next serves the Webapp.
    SignOut,
}

/// What `hermes webapp <verb>` reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CliOutcome {
    /// The process exit code (`cmd_webapp`'s return).
    pub code: i32,
    /// The human-readable report (one or more lines, no trailing newline).
    pub message: String,
}

/// The `hermes webapp` wiring point: `crates/cli` should call this from its
/// `webapp` subcommand with the resolved gateway home, e.g.
///
/// ```ignore
/// // crates/cli/src/... (the `webapp` subcommand handler)
/// let outcome = hermes_gateway::webapp::cli_entrypoint(&home, WebappCommand::Status);
/// println!("{}", outcome.message);
/// return outcome.code;
/// ```
///
/// `Status` reports the settings the gateway reads at start — the address it
/// will serve, or the `webapp:` block's own problem — and never starts a
/// gateway. `SignOut` rotates the stored browser credential in the home,
/// which ends every login bound to it at the next serve. The live control
/// verbs of the Python launcher (`webapp-launch` over the control socket, the
/// browser openers) are deferred: the Rust gateway serves the Webapp in its
/// own process, so a future CLI port opens `cli_entrypoint`-computed launch
/// links directly against the running listener.
pub fn cli_entrypoint(home: &Path, command: WebappCommand) -> CliOutcome {
    match command {
        WebappCommand::Status => {
            let settings = webapp_settings(home);
            if let Some(reason) = &settings.reason {
                return CliOutcome {
                    code: 1,
                    message: format!(
                        "The Webapp is off: {reason}. Fix it, then run hermes webapp again."
                    ),
                };
            }
            if !settings.enabled {
                return CliOutcome {
                    code: 1,
                    message: format!(
                        "The Webapp is off. Set webapp.enabled: true in {}, then run hermes webapp again.",
                        home.join("config.yaml").display()
                    ),
                };
            }
            let origin = dial_origin(&settings.host, settings.port);
            let mode = access_mode(&settings).unwrap_or("local");
            CliOutcome {
                code: 0,
                message: format!("Serving at {origin} (mode: {mode})"),
            }
        }
        WebappCommand::SignOut => match super::credential::BrowserCredential::load(home) {
            Ok(mut credential) => match credential.rotate() {
                Ok(_) => CliOutcome {
                    code: 0,
                    message: "Replaced the stored Webapp credential: every browser must sign in \
                              again when the gateway next serves the Webapp."
                        .into(),
                },
                Err(e) => CliOutcome {
                    code: 1,
                    message: format!("Nobody was signed out: {e}"),
                },
            },
            Err(e) => CliOutcome {
                code: 1,
                message: format!("Nobody was signed out: {e}"),
            },
        },
    }
}
