//! hermes-tui — full-screen terminal TUI for hermes-rs, the Rust port of the
//! Python prompt_toolkit TUI (`hermes_cli/cli_tui_mixin.py` +
//! `main_tui_launch.py`).
//!
//! Layering (widget code stays thin; everything testable is a view-model):
//! - [`model`]: transcript cards + streaming-delta application + line
//!   wrapping/scroll math → ratatui `Line`s.
//! - [`autocomplete`]: slash-command popup state machine over the *exact*
//!   REPL command table (`hermes_cli::slash::COMMAND_REGISTRY`).
//! - [`keys`]: keybinding → action mapping (Enter send, Ctrl+J/Alt+Enter
//!   newline, Up/Down history, Tab/Enter accept, Esc = /stop, Ctrl+C/D).
//! - [`editor`]: character-cursor buffer, backslash continuation, history.
//! - [`tui_io`]: `ReplIo` adapter — the turn loop is the existing
//!   `hermes_cli::repl::Repl` over a channel-fed IO, so streaming tool
//!   output, mid-turn interrupts, queueing, and session switching are the
//!   REPL's own machinery, not a reimplementation.
//! - [`ui`]: thin ratatui layout/widgets; [`app`]: event loop + fallback
//!   to the line REPL when stdin is not a TTY.

pub mod app;
pub mod autocomplete;
pub mod editor;
pub mod keys;
pub mod model;
pub mod tui_io;
pub mod ui;
