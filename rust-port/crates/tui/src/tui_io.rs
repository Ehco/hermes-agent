//! `ReplIo` adapter for the TUI: committed lines arrive over a channel from
//! the UI thread (so `Repl::run` can race input against a streaming turn
//! exactly as it does with the line REPL — that is what makes Escape→/stop
//! interrupt work), and every Repl write lands in the shared `Transcript`
//! with a redraw flag.

use hermes_cli::repl::{InputFuture, ReplIo};
use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::mpsc::{Receiver, Sender};

/// What the UI thread sends to the REPL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UiInput {
    /// A committed editor line (a user prompt or a slash command).
    Line(String),
    /// Ctrl+D / Ctrl+C on empty: EOF, the REPL exits its loop.
    Quit,
}

#[derive(Debug)]
pub struct TuiIo {
    /// The channel is shared with the future returned by `read_line` (which
    /// must be `'static`); tokio's `Receiver` is not `Clone`, so the future
    /// locks the mutex around each recv.
    rx: Arc<tokio::sync::Mutex<Receiver<UiInput>>>,
    transcript: Arc<Mutex<crate::model::Transcript>>,
    dirty: Arc<AtomicBool>,
}

impl TuiIo {
    pub fn new(
        rx: Receiver<UiInput>,
        transcript: Arc<Mutex<crate::model::Transcript>>,
        dirty: Arc<AtomicBool>,
    ) -> Self {
        Self { rx: Arc::new(tokio::sync::Mutex::new(rx)), transcript, dirty }
    }

    fn touch(&self) {
        self.dirty.store(true, Ordering::Release);
    }
}

impl ReplIo for TuiIo {
    fn read_line(&mut self) -> InputFuture {
        let rx = self.rx.clone();
        Box::pin(async move {
            let mut guard = rx.lock().await;
            match guard.recv().await {
                Some(UiInput::Line(line)) => Some(line),
                Some(UiInput::Quit) | None => None,
            }
        })
    }

    fn write_out(&mut self, text: &str) {
        if let Ok(mut t) = self.transcript.lock() {
            t.push_notice(text);
        }
        self.touch();
    }

    fn write_err(&mut self, text: &str) {
        self.write_out(text);
    }

    fn flush_out(&mut self) {
        self.touch();
    }

    /// Live streaming: deltas append to the in-place assistant card.
    fn stream_delta(&mut self, delta: &str) {
        if let Ok(mut t) = self.transcript.lock() {
            t.apply_stream_delta(delta);
        }
        self.touch();
    }

    /// Tool cards render in place: a Running card on start, its result on
    /// completion (Python tool-card pair).
    fn tool_event(&mut self, name: &str, arguments: &str, output: Option<&str>) {
        if let Ok(mut t) = self.transcript.lock() {
            match output {
                None => t.start_tool(name, arguments),
                Some(out) => {
                    let ok = !out.starts_with("tool error:");
                    t.finish_tool(out, ok);
                }
            }
        }
        self.touch();
    }
}

/// Sender side held by the UI thread (kept out of the `ReplIo` object).
#[derive(Debug, Clone)]
pub struct UiHandle {
    pub tx: Sender<UiInput>,
}

impl UiHandle {
    /// Send a committed line (UI thread → REPL). `blocking_send` because the
    /// UI thread is a plain OS thread, not async.
    pub fn send(&self, input: UiInput) {
        let _ = self.tx.blocking_send(input);
    }
}
