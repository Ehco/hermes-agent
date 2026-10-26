//! The Webapp: the browser-hosted Desktop on the gateway's own HTTP listener
//! (port of `gateway/webapp_listener.py`, `hermes_cli/web_routers/webapp.py`,
//! `hermes_cli/webapp_sign_in.py`, `hermes_cli/webapp_access.py` and the
//! launcher half of `hermes_cli/webapp_launcher.py`).
//!
//! `webapp.enabled` in the gateway home's `config.yaml` turns it on: the
//! listener then binds `webapp.host`/`webapp.port` (a stable port, so the
//! browser's URL survives restarts) and serves the built bundle plus the
//! webapp-only routes. The browser's credential is the dashboard session
//! token, kept user-only in the home so open tabs keep working across a
//! gateway restart; signing out replaces it.
//!
//! Layers, each in its own module:
//!
//! - [`settings`] — the `webapp:` block reader and the addressing contract
//!   (hosts, ports, public URLs, dial origins, bundle staleness).
//! - [`credential`] — the browser session token (`webapp/session-token`) and
//!   the SHA-256/HMAC sign-in binding.
//! - [`tickets`] — the one-use window/launch handoff tickets.
//! - [`launcher`] — port/binding selection and the `hermes webapp` CLI
//!   entrypoint.
//! - [`routes`] — the axum listener: static bundle serving, the ticket mints
//!   and the handoff pages.
//!
//! ## The crates/cli wiring point
//!
//! `hermes webapp` maps to [`launcher::cli_entrypoint`] (see its docs); the
//! listener half is [`WebappApi::router`], which a host process mounts the way
//! `main.rs` mounts the desktop router — the CLI itself never starts one, it
//! reports status and rotates the credential against the home.

pub mod credential;
pub mod launcher;
pub mod routes;
pub mod settings;
pub mod tickets;

pub use credential::{BrowserCredential, CredentialError};
pub use launcher::{CliOutcome, ListenerPlan, WebappCommand};
pub use routes::{WebappApi, WebappState};
pub use settings::{webapp_settings, WebappSettings, DEFAULT_HOST, DEFAULT_PORT};

/// A fresh one-use id for tickets and page nonces: the same 43-character
/// urlsafe shape the launch page validates (`secrets.token_urlsafe(32)`).
pub fn new_token_id() -> String {
    credential::generate_token()
}
