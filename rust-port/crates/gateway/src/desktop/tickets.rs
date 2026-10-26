//! One-use WS-upgrade tickets (port of
//! `hermes_cli/dashboard_auth/ws_tickets.py`): minted over authenticated REST
//! (`POST /api/auth/ws-ticket`), consumed on the `/api/ws` upgrade — riding
//! either `?ticket=` or the `hermes-gateway-ticket.<t>` subprotocol — with a
//! 30-second TTL, in-memory.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Long enough for `getWsTicket()` → open WS; short enough that a leaked
/// ticket is uninteresting (the Python module's `TTL_SECONDS`).
pub const TTL_SECONDS: u64 = 30;

#[derive(Debug)]
pub struct TicketInvalid(pub String);

impl std::fmt::Display for TicketInvalid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ticket invalid: {}", self.0)
    }
}

struct Entry {
    expires_at: Instant,
    #[allow(dead_code)] // identity rides the mint; the WS gate only consumes.
    info: serde_json::Value,
}

/// One-shot ticket bound to this identity; `consume_ticket` hands `info`
/// back to the WS handler. (`Clone` exists for the builder pattern on
/// `DesktopInner`; clones share no state — tickets are minted/consumed on one.)
pub struct TicketStore {
    ttl: Duration,
    tickets: Mutex<HashMap<String, Entry>>,
    counter: AtomicU64,
}

impl Clone for TicketStore {
    fn clone(&self) -> Self {
        Self {
            ttl: self.ttl,
            tickets: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(self.counter.load(Ordering::Relaxed)),
        }
    }
}

impl TicketStore {
    pub fn new() -> Self {
        Self {
            ttl: Duration::from_secs(TTL_SECONDS),
            tickets: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// One-shot ticket bound to this identity; `consume_ticket` hands `info`
    /// back to the WS handler.
    pub fn mint(&self, info: serde_json::Value) -> String {
        let ticket = super::new_id("tkt");
        let n = self.counter.fetch_add(1, Ordering::Relaxed);
        let mut t = ticket.clone();
        // Mix a per-mint counter so equal-second mints stay distinct.
        if n > 0 {
            t.push_str(&format!("{n:x}"));
        }
        self.tickets.lock().unwrap().insert(
            t.clone(),
            Entry {
                expires_at: Instant::now() + self.ttl,
                info,
            },
        );
        self.gc_expired();
        t
    }

    /// Validate and consume (single-use). `Err` on missing/expired/used.
    pub fn consume(&self, ticket: &str) -> Result<serde_json::Value, TicketInvalid> {
        let entry = self
            .tickets
            .lock()
            .unwrap()
            .remove(ticket)
            .ok_or_else(|| TicketInvalid(format!("unknown ticket: {}", truncate(ticket))))?;
        if entry.expires_at < Instant::now() {
            return Err(TicketInvalid("expired".into()));
        }
        Ok(entry.info)
    }

    fn gc_expired(&self) {
        let now = Instant::now();
        let mut tickets = self.tickets.lock().unwrap();
        tickets.retain(|_, entry| entry.expires_at >= now);
    }
}

fn truncate(ticket: &str) -> &str {
    if ticket.len() <= 8 {
        ticket
    } else {
        &ticket[..8]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticket_is_single_use() {
        let store = TicketStore::new();
        let t = store.mint(serde_json::json!({"user_id": "u"}));
        assert!(store.consume(&t).is_ok());
        assert!(store.consume(&t).is_err(), "second consume must fail");
    }

    #[test]
    fn unknown_ticket_fails() {
        let store = TicketStore::new();
        assert!(store.consume("nope").is_err());
    }
}
