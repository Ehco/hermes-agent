//! App glue: the UI event loop (own OS thread owning the terminal) + the
//! turn loop driven by the existing `hermes_cli::repl::Repl` over the
//! `TuiIo` adapter, plus the graceful line-REPL fallback when stdin is not
//! a TTY.

use crate::autocomplete::Autocomplete;
use crate::editor::{Editor, Outcome};
use crate::keys::{self, Action};
use crate::model::{Block, ToolStatus, Transcript};
use crate::tui_io::{TuiIo, UiInput, UiHandle};
use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use hermes_cli::repl::{Repl, SharedIo, StdinIo};
use hermes_core::model::{Message, ToolSpec};
use hermes_session::SessionStore;
use std::io::{IsTerminal, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// True when the TUI can run (stdin is a terminal); otherwise the caller
/// drops to the line REPL — the Python `main_tui_launch` fallback.
pub fn is_tty() -> bool {
    std::io::stdin().is_terminal()
}

/// Run the TUI, or the line REPL when the terminal is not a TTY.
/// Mirrors the Python TUI launch path and its classic-CLI fallback.
pub async fn run(
    transport: Box<dyn hermes_core::transport::Transport>,
    tools: Vec<ToolSpec>,
    store: SessionStore,
    history: Vec<Message>,
    model: String,
    system_prompt: String,
) -> Result<()> {
    if !is_tty() {
        // Graceful fallback: no TTY → the line REPL the crate already has.
        let io: SharedIo = Arc::new(Mutex::new(StdinIo));
        let mut repl = Repl::new(io, transport, tools, store, history, model, system_prompt);
        return repl.run().await;
    }
    run_tui(transport, tools, store, history, model, system_prompt).await
}

/// The crossterm/ratatui event loop. Owns the terminal; exits when the user
/// quits or `shutdown` flips (REPL done). Sends committed lines via `handle`.
fn event_loop(
    handle: UiHandle,
    transcript: Arc<Mutex<Transcript>>,
    dirty: Arc<AtomicBool>,
    shutdown: Arc<AtomicBool>,
    model: String,
) -> Result<()> {
    let mut stdout = std::io::stdout();
    crossterm::terminal::enable_raw_mode()?;
    crossterm::execute!(stdout, crossterm::terminal::EnterAlternateScreen)?;
    let backend = ratatui::backend::CrosstermBackend::new(stdout);
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.hide_cursor()?;

    let mut editor = Editor::default();
    let mut ac = Autocomplete::default();
    ac.update(editor.text());
    let mut running = false;

    let result = (|| -> Result<()> {
        loop {
            if event::poll(Duration::from_millis(50))? {
                if let Event::Key(key) = event::read()? {
                    if key.kind == KeyEventKind::Press {
                        match handle_key(&transcript, &mut editor, &mut ac, key) {
                            KeyOutcome::Line(line) => {
                                handle.send(UiInput::Line(line));
                            }
                            KeyOutcome::Quit => {
                                handle.send(UiInput::Quit);
                                break;
                            }
                            KeyOutcome::Edited => {}
                        }
                        // Every keystroke re-anchors the popup.
                        ac.update(editor.text());
                    }
                }
            }
            // Track whether a turn is live (streaming text or a running tool).
            if dirty.swap(false, Ordering::AcqRel) {
                running = transcript
                    .lock()
                    .map(|t| {
                        matches!(
                            t.blocks().last(),
                            Some(Block::Assistant { streaming: true, .. })
                                | Some(Block::Tool {
                                    status: ToolStatus::Running,
                                    ..
                                })
                        )
                    })
                    .unwrap_or(false);
            }
            draw_frame(&mut terminal, &transcript, &editor, &ac, running, &model)?;
            if shutdown.load(Ordering::Acquire) {
                break;
            }
        }
        // One last frame so the final transcript state is visible.
        draw_frame(&mut terminal, &transcript, &editor, &ac, running, &model)?;
        Ok(())
    })();

    // Restore the terminal no matter what.
    crossterm::execute!(
        std::io::stdout(),
        crossterm::terminal::LeaveAlternateScreen,
        crossterm::cursor::Show
    )
    .ok();
    crossterm::terminal::disable_raw_mode().ok();
    result
}

enum KeyOutcome {
    /// The key only edited the editor / popup / scroll state.
    Edited,
    /// A line is committed (text or the /stop interrupt).
    Line(String),
    /// Quit (Ctrl+C on empty / Ctrl+D).
    Quit,
}

fn handle_key(
    transcript: &Arc<Mutex<Transcript>>,
    editor: &mut Editor,
    ac: &mut Autocomplete,
    key: KeyEvent,
) -> KeyOutcome {
    let popup_open = ac.is_open();
    let editor_empty = editor.is_empty();
    match keys::map_key(key, popup_open, editor_empty) {
        Action::InsertNewline => {
            editor.newline();
            KeyOutcome::Edited
        }
        Action::Send => match editor.enter() {
            Outcome::Sent(line) => KeyOutcome::Line(line),
            Outcome::None => KeyOutcome::Edited,
        },
        Action::AcceptCompletion => {
            if let Some(completed) = ac.accept(editor.text()) {
                editor.replace(&completed);
            }
            ac.close();
            KeyOutcome::Edited
        }
        Action::AutocompleteNext => {
            ac.move_down();
            KeyOutcome::Edited
        }
        Action::AutocompletePrev => {
            ac.move_up();
            KeyOutcome::Edited
        }
        Action::HistoryPrev => {
            editor.history_prev();
            KeyOutcome::Edited
        }
        Action::HistoryNext => {
            editor.history_next();
            KeyOutcome::Edited
        }
        Action::Interrupt => {
            // Escape = /stop: the REPL's mid-turn dispatcher cancels the
            // in-flight turn (Python double-escape interrupt); at the prompt
            // it just prints "(no turn running)".
            KeyOutcome::Line("/stop".to_string())
        }
        Action::ClearInput => {
            editor.clear();
            KeyOutcome::Edited
        }
        Action::Quit => KeyOutcome::Quit,
        Action::ScrollUp => {
            if let Ok(mut t) = transcript.lock() {
                t.scroll_by(5, 10);
            }
            KeyOutcome::Edited
        }
        Action::ScrollDown => {
            if let Ok(mut t) = transcript.lock() {
                t.scroll_by(-5, 10);
            }
            KeyOutcome::Edited
        }
        Action::Ignored => {
            // Editor defaults: plain typing, Backspace.
            match key.code {
                KeyCode::Char(c)
                    if !key.modifiers.contains(KeyModifiers::CONTROL)
                        && !key.modifiers.contains(KeyModifiers::ALT) =>
                {
                    editor.insert(c);
                    KeyOutcome::Edited
                }
                KeyCode::Backspace => {
                    editor.backspace();
                    KeyOutcome::Edited
                }
                _ => KeyOutcome::Edited,
            }
        }
    }
}

fn status_bar(model: &str, running: bool) -> String {
    let mode = if running { "streaming (Esc = stop)" } else { "idle" };
    format!(
        " hermes-tui | {model} | {mode} | Enter send · Ctrl+J newline · Up/Dn history · Tab complete · Esc stop · Ctrl+C quit"
    )
}

fn draw_frame(
    terminal: &mut ratatui::Terminal<ratatui::backend::CrosstermBackend<std::io::Stdout>>,
    transcript: &Arc<Mutex<Transcript>>,
    editor: &Editor,
    ac: &Autocomplete,
    running: bool,
    model: &str,
) -> Result<()> {
    let status = status_bar(model, running);
    terminal.draw(|f| {
        if let Ok(t) = transcript.lock() {
            crate::ui::draw(f, &t, editor, ac, &status);
        }
    })?;
    std::io::stdout().flush().ok();
    Ok(())
}

/// The full-screen TUI session: seed the transcript from the resumed
/// session, run the line-REPL loop over `TuiIo` on this task, and let the
/// UI thread own the terminal.
async fn run_tui(
    transport: Box<dyn hermes_core::transport::Transport>,
    tools: Vec<ToolSpec>,
    store: SessionStore,
    history: Vec<Message>,
    model: String,
    system_prompt: String,
) -> Result<()> {
    let transcript = Arc::new(Mutex::new(Transcript::default()));
    {
        let mut t = transcript.lock().unwrap();
        t.seed_from_history(&history);
        t.push_notice(&format!(
            "hermes-tui | {model} | {} messages resumed | /help for commands",
            history.len()
        ));
    }
    let dirty = Arc::new(AtomicBool::new(false));
    let shutdown = Arc::new(AtomicBool::new(false));

    let (tx, rx) = tokio::sync::mpsc::channel::<UiInput>(32);
    let handle = UiHandle { tx };

    // UI thread owns the terminal.
    let ui_transcript = transcript.clone();
    let ui_dirty = dirty.clone();
    let ui_shutdown = shutdown.clone();
    let ui_model = model.clone();
    let ui = std::thread::spawn(move || {
        let result = event_loop(handle, ui_transcript, ui_dirty, ui_shutdown, ui_model);
        if let Err(e) = result {
            // The terminal is already restored at this point.
            eprintln!("tui error: {e:#}");
        }
    });

    // The turn loop: the exact line-REPL machinery over the TUI adapter —
    // streaming output rendered in place, /stop interrupt, queueing,
    // session switching.
    let io = Arc::new(Mutex::new(TuiIo::new(rx, transcript.clone(), dirty.clone())));
    let mut repl = Repl::new(io, transport, tools, store, history, model, system_prompt);
    let result = repl.run().await;
    shutdown.store(true, Ordering::Release);
    let _ = ui.join();
    result
}
