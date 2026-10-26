//! Library surface for the hermes CLI internals.
//!
//! The TUI crate (`hermes-tui`) reuses these modules instead of redefining
//! them: the slash-command table (`slash`), the line-continuation rules
//! (`multiline`), session switching (`session_switch`), and the whole REPL
//! turn loop behind the `ReplIo` seam (`repl`).

pub mod multiline;
pub mod repl;
pub mod session_switch;
pub mod slash;
pub mod tool_bridge;
