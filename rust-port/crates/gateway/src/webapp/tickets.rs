//! The one-use handoff tickets each browser socket dial and child-window
//! handoff mints (port of `hermes_cli/web_routers/webapp.py::
//! _mint_handoff_ticket`): `POST /api/webapp/window-ticket` mints one for the
//! `POST /webapp/window-session` exchange, and `webapp-launch` mints the
//! launch kind the `/webapp/launch` page spends. The store keeps
//! `(expiry, credential generation, launch)`, never the credential itself.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use super::credential::BrowserCredential;

/// `POST /webapp/window-session` tickets: a child window's authorization.
pub const WINDOW_TICKET_TTL_SECONDS: u64 = 30;
/// A sandboxed (snap/Flatpak) browser's cold start can take several seconds to
/// reach the page (`LAUNCH_TICKET_TTL_SECONDS`).
pub const LAUNCH_TICKET_TTL_SECONDS: u64 = 60;
/// The pending-ticket cap (`_MAX_WINDOW_TICKETS`).
pub const MAX_WINDOW_TICKETS: usize = 128;

/// The mint-time shape of a handoff ticket.
#[derive(Clone, Copy, Debug)]
pub struct Handoff {
    /// Whether this ticket crossed a browser launcher's argv (the
    /// server-opened-browser kind): only a redeemer the store's caller vouches
    /// for may spend it.
    pub launch: bool,
}

#[derive(Clone, Debug)]
struct Entry {
    expires_at: Instant,
    generation: u64,
    launch: bool,
}

#[derive(Debug)]
pub struct MintError(pub String);

impl std::fmt::Display for MintError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

#[derive(Debug)]
pub struct RedeemError(pub String);

impl std::fmt::Display for RedeemError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// The in-memory handoff ticket store. (`Clone` exists for builder moves, like
/// the desktop `TicketStore`; clones share no state.)
#[derive(Default)]
pub struct HandoffTickets {
    tickets: Mutex<HashMap<String, Entry>>,
}

impl std::clone::Clone for HandoffTickets {
    fn clone(&self) -> Self {
        Self { tickets: Mutex::new(HashMap::new()) }
    }
}

impl HandoffTickets {
    /// Mint one-use ticket for a `ttl` handoff; `Err` at the pending cap
    /// (Python raises `HTTPException(429)` there).
    pub fn mint(
        &self,
        ttl: Duration,
        launch: bool,
        credential: &BrowserCredential,
    ) -> Result<String, MintError> {
        self.gc_expired();
        {
            let tickets = self.tickets.lock().unwrap();
            if tickets.len() >= MAX_WINDOW_TICKETS {
                return Err(MintError("Too many pending Webapp windows; try again shortly".into()));
            }
        }
        let ticket = super::new_token_id();
        self.tickets.lock().unwrap().insert(
            ticket.clone(),
            Entry {
                expires_at: Instant::now() + ttl,
                generation: credential.generation(),
                launch,
            },
        );
        Ok(ticket)
    }

    /// Spend *ticket*: `Ok(generation)` exactly once, before expiry, and only
    /// while the credential it was minted under is still current
    /// (`browser_credential.current_if`). A launch ticket whose redeemer
    /// (`launch`) is not vouched for is left unspent — a refused attempt must
    /// not burn it for the real browser.
    pub fn redeem(
        &self,
        ticket: &str,
        credential: &BrowserCredential,
        redeemer_vouched: bool,
    ) -> Result<(), RedeemError> {
        let now = Instant::now();
        let mut tickets = self.tickets.lock().unwrap();
        let entry = tickets.get(ticket).ok_or_else(|| {
            RedeemError("Webapp window link expired or was already used; open a new window".into())
        })?;
        if entry.launch && !redeemer_vouched {
            // Left unspent on purpose (Python: `entry = None` without delete).
            return Err(RedeemError(
                "Webapp window link expired or was already used; open a new window".into(),
            ));
        }
        if entry.expires_at <= now {
            tickets.remove(ticket);
            return Err(RedeemError(
                "Webapp window link expired or was already used; open a new window".into(),
            ));
        }
        if !credential.is_current(entry.generation) {
            tickets.remove(ticket);
            return Err(RedeemError(
                "Webapp window link expired or was already used; open a new window".into(),
            ));
        }
        tickets.remove(ticket);
        Ok(())
    }

    fn gc_expired(&self) {
        let now = Instant::now();
        self.tickets
            .lock()
            .unwrap()
            .retain(|_, entry| entry.expires_at > now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(dir: &std::path::Path) -> BrowserCredential {
        std::fs::create_dir_all(dir).unwrap();
        BrowserCredential::load(dir).unwrap()
    }

    #[test]
    fn tickets_are_single_use_and_generation_bound() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-tkt-{}", std::process::id()));
        let cred = credential(&dir);
        let store = HandoffTickets::default();
        let t = store.mint(Duration::from_secs(60), false, &cred).unwrap();
        assert!(store.redeem(&t, &cred, true).is_ok());
        // Replayed: already used.
        assert!(store.redeem(&t, &cred, true).is_err());
        // A ticket minted before a rotate opens nothing after it.
        let t2 = store.mint(Duration::from_secs(60), false, &cred).unwrap();
        let mut cred = cred;
        cred.rotate().unwrap();
        assert!(store.redeem(&t2, &cred, true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn expired_tickets_are_refused() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-tkt-x-{}", std::process::id()));
        let cred = credential(&dir);
        let store = HandoffTickets::default();
        let t = store.mint(Duration::ZERO, false, &cred).unwrap();
        std::thread::sleep(std::time::Duration::from_millis(5));
        assert!(store.redeem(&t, &cred, true).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn launch_tickets_survive_an_unvouched_redeemer() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-tkt-l-{}", std::process::id()));
        let cred = credential(&dir);
        let store = HandoffTickets::default();
        let t = store.mint(Duration::from_secs(60), true, &cred).unwrap();
        // An unvouched redeemer (another OS user's connection) does not burn it.
        assert!(store.redeem(&t, &cred, false).is_err());
        assert!(store.redeem(&t, &cred, true).is_ok());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
