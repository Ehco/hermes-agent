//! The ported plugins, chosen by desktop usage:
//!
//! - `kanban` — the desktop's flagship plugin panel; board REST + the
//!   `/events` WebSocket whose frame is THE desktop event envelope.
//! - `hermes-achievements` — REST-only polling panel (second tab in the
//!   desktop sidebar set).
//! - `example` — the test-fixture plugin (`tests/fixtures/plugins/
//!   example-dashboard`), kept as the canonical minimal plugin.

pub mod achievements;
pub mod example;
pub mod kanban;
