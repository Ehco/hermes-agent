//! The REPL: slash-command dispatch, interruptible streaming turns, multiline
//! input, and mid-conversation session switching.
//!
//! Everything user-facing is behind the `ReplIo` trait (line input + text
//! output), so tests drive the command table with an in-memory IO instead of
//! a TTY — mirroring how the Python CLI's prompt_toolkit layer is faked in
//! its tests.

use crate::multiline;
use crate::session_switch::{self, SessionEntry};
use crate::slash::{self, MidTurn, ParsedLine};
use crate::tool_bridge::bridge;
use anyhow::Result;
use futures::StreamExt;
use hermes_core::agent::ToolHost;
use hermes_core::model::{Content, Message, Role, ToolSpec};
use hermes_core::transport::{StreamItem, Transport};
use hermes_session::SessionStore;
use serde_json::{json, Value};
#[cfg(test)]
use std::collections::VecDeque;
use std::future::Future;
use std::io::Write as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::task::Poll;

/// A boxed input future: the REPL polls it while a turn streams, so it must
/// be detached from `&mut self` (Python parity: input is read concurrently
/// with the agent run, which is what makes `/stop` interrupt possible).
pub type InputFuture = Pin<Box<dyn Future<Output = Option<String>> + Send + 'static>>;

pub type SharedIo = Arc<Mutex<dyn ReplIo>>;

pub trait ReplIo: Send {
    /// Next line of input (trailing newline stripped), or `None` on EOF.
    /// The returned future must be `'static` and, once polled, independent of
    /// `self` (implementations move their state into the future).
    fn read_line(&mut self) -> InputFuture;
    fn write_out(&mut self, text: &str);
    fn write_err(&mut self, text: &str);
    fn flush_out(&mut self);
    /// A streaming assistant text delta (port of the Python TUI's live
    /// render hook). Default: a raw write, which is the classic REPL's
    /// behavior. The TUI overrides this to append into the live assistant
    /// card in the transcript.
    fn stream_delta(&mut self, delta: &str) {
        self.write_out(delta);
    }
    /// Tool execution progress (Python tool-card rendering): `output = None`
    /// when a call starts, `Some(result)` when it finishes. Default: silent,
    /// which is the classic REPL's behavior; the TUI overrides this to draw
    /// a tool card in place.
    fn tool_event(&mut self, _name: &str, _arguments: &str, _output: Option<&str>) {}
}

/// Real terminal IO: reads stdin lines on tokio's blocking pool so a turn can
/// stream while the user types (the interrupt path needs concurrent input).
pub struct StdinIo;

impl ReplIo for StdinIo {
    fn read_line(&mut self) -> InputFuture {
        Box::pin(async move {
            tokio::task::spawn_blocking(move || {
                let mut buf = String::new();
                match std::io::stdin().read_line(&mut buf) {
                    Ok(0) | Err(_) => None,
                    Ok(_) => {
                        while buf.ends_with('\n') || buf.ends_with('\r') {
                            buf.pop();
                        }
                        Some(buf)
                    }
                }
            })
            .await
            .ok()
            .flatten()
        })
    }
    fn write_out(&mut self, text: &str) {
        print!("{text}");
    }
    fn write_err(&mut self, text: &str) {
        eprint!("{text}");
    }
    fn flush_out(&mut self) {
        std::io::stdout().flush().ok();
    }
    // stream_delta inherits the default (raw write) — exactly StdinIo's
    // previous behavior.
}

/// Tool execution host: delegates into the tool bridge by name (no hardcoded
/// tool names in the CLI — the registry's dispatch stays generic).
pub struct CliToolHost;

impl ToolHost for CliToolHost {
    fn execute(&self, name: &str, arguments: &str) -> impl Future<Output = Result<String>> + Send {
        let name = name.to_string();
        async move {
            let args: Value = serde_json::from_str(arguments).unwrap_or_else(|_| json!({}));
            Ok(bridge(&name, &args))
        }
    }
}

/// The JSON Schemas for the tools the bridge implements.
pub fn tool_specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "terminal".into(),
            description: "Run a shell command and return its output.".into(),
            schema: json!({
                "type": "object",
                "properties": {"command": {"type": "string"}},
                "required": ["command"],
            }),
        },
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file (1-indexed line numbers).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "offset": {"type": "integer"},
                    "limit": {"type": "integer"},
                },
                "required": ["path"],
            }),
        },
        ToolSpec {
            name: "list_dir".into(),
            description: "List a directory's entries with sizes.".into(),
            schema: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}, "limit": {"type": "integer"}},
            }),
        },
    ]
}

/// Test fixture system prompt; production builds take it from main.rs.
#[cfg(test)]
const SYSTEM_PROMPT: &str = "You are Hermes, a helpful agent running from the Rust CLI port.";

/// Result of dispatching one command at the prompt.
#[derive(Debug, PartialEq, Eq)]
pub enum Flow {
    Continue,
    Quit,
}

pub struct Repl {
    io: SharedIo,
    transport: Box<dyn Transport>,
    tools: Vec<ToolSpec>,
    store: SessionStore,
    history: Vec<Message>,
    /// Session-scoped model override (`/model`), mirroring the Python
    /// model_switch session scope.
    pub model: String,
    /// One-shot bare-number resume selection (Python `_pending_resume_sessions`).
    pending_resume: Option<Vec<SessionEntry>>,
    /// Prompts typed while a turn was streaming (Python busy=queue default).
    queued: Vec<String>,
    system_prompt: String,
}

impl Repl {
    pub fn new(
        io: SharedIo,
        transport: Box<dyn Transport>,
        tools: Vec<ToolSpec>,
        store: SessionStore,
        history: Vec<Message>,
        model: String,
        system_prompt: String,
    ) -> Self {
        Self {
            io,
            transport,
            tools,
            store,
            history,
            model,
            pending_resume: None,
            queued: Vec::new(),
            system_prompt,
        }
    }

    // ── IO helpers ───────────────────────────────────────────────────────────

    fn out(&self, text: &str) {
        if let Ok(mut io) = self.io.lock() {
            io.write_out(text);
            io.write_out("\n");
            io.flush_out();
        }
    }

    fn out_raw(&self, text: &str) {
        if let Ok(mut io) = self.io.lock() {
            io.write_out(text);
            io.flush_out();
        }
    }

    fn stream(&self, delta: &str) {
        // Streaming deltas go through the hook so the TUI can render them
        // into the live assistant card (default = raw write, as before).
        if let Ok(mut io) = self.io.lock() {
            io.stream_delta(delta);
        }
    }

    fn tool_progress(&self, name: &str, arguments: &str, output: Option<&str>) {
        if let Ok(mut io) = self.io.lock() {
            io.tool_event(name, arguments, output);
        }
    }

    fn err(&self, text: &str) {
        if let Ok(mut io) = self.io.lock() {
            io.write_err(text);
            io.write_err("\n");
        }
    }

    fn io_next_line(&self) -> InputFuture {
        // The returned future is 'static, so the mutex is released here.
        match self.io.lock() {
            Ok(mut io) => io.read_line(),
            Err(_) => Box::pin(std::future::pending()),
        }
    }

    /// Read one full input, joining backslash-continued lines (Python
    /// `_apply_backslash_line_continuation`): a trailing `\` folds the next
    /// line in with a literal newline.
    async fn read_input_line(&self) -> Option<String> {
        let mut line = self.io_next_line().await?;
        if !multiline::is_backslash_line_continuation(&line) {
            return Some(line);
        }
        let mut buf = multiline::apply_backslash_line_continuation(&line);
        loop {
            line = self.io_next_line().await?;
            if multiline::is_backslash_line_continuation(&line) {
                buf.push_str(&multiline::apply_backslash_line_continuation(&line));
            } else {
                buf.push_str(&line);
                return Some(buf);
            }
        }
    }

    // ── Main loop ────────────────────────────────────────────────────────────

    pub async fn run(&mut self) -> Result<()> {
        loop {
            self.out_raw("› ");
            let line = match self.read_input_line().await {
                Some(line) => line,
                None => break, // EOF
            };
            let line = line.trim().to_string();
            if line.is_empty() {
                continue;
            }
            // Armed one-shot bare-number selection (Python `_pending_resume_sessions`):
            // typing just `3` after /sessions or bare /resume resumes row 3.
            if self.pending_resume.is_some()
                && !line.is_empty()
                && line.chars().all(|c| c.is_ascii_digit())
            {
                let entries = self.pending_resume.take();
                if let Err(e) = self.resume_by_target(&line, entries) {
                    self.err(&format!("error: {e:#}"));
                }
                continue;
            }
            match slash::parse_line(&line) {
                ParsedLine::Text => {
                    if let Err(e) = self.start_conversation(&line).await {
                        self.err(&format!("error: {e:#}"));
                    }
                    self.drain_queue().await;
                }
                ParsedLine::Unknown(raw) => {
                    self.out(&format!("unknown command {raw} — try /help"));
                }
                ParsedLine::Command(inv) => match self.dispatch(&inv).await? {
                    Flow::Continue => {}
                    Flow::Quit => break,
                },
            }
        }
        self.out("bye");
        Ok(())
    }

    // ── Command dispatch ─────────────────────────────────────────────────────

    async fn dispatch(&mut self, inv: &slash::Invocation) -> Result<Flow> {
        match inv.def.name {
            "help" => self.out(&slash::render_help()),

            "new" | "clear" => {
                self.store.clear()?;
                self.history.clear();
                self.history
                    .push(Message::text(Role::System, self.system_prompt.as_str()));
                self.pending_resume = None;
                self.out("(new session)");
            }

            // At the prompt there is nothing to interrupt (mid-turn /stop is
            // handled inside run_turn's select).
            "stop" => self.out("(no turn running)"),

            "history" => {
                if self.history.is_empty() {
                    self.out("(no history)");
                }
                for msg in &self.history {
                    self.out(&format_history_msg(msg));
                }
            }

            "model" => {
                let arg = inv.args.trim();
                if arg.is_empty() {
                    self.out(&format!("model: {}", self.model));
                } else {
                    // Session-scoped switch; flags like --global are accepted
                    // but not yet implemented (config file untouched).
                    let model = arg
                        .split_whitespace()
                        .find(|t| !t.starts_with("--"))
                        .unwrap_or(arg);
                    self.model = model.to_string();
                    self.out(&format!(
                        "model switched to {model} (session-scoped; --global not implemented yet)"
                    ));
                }
            }

            "sessions" => {
                // Python `_handle_sessions_command`: bare/list shows the table;
                // an explicit target delegates to /resume.
                let target = session_switch::strip_placeholder(inv.args.trim());
                if !target.is_empty() && !matches!(target, "list" | "ls" | "browse") {
                    let armed = self.pending_resume.take();
                    self.resume_by_target(target, armed)?;
                    return Ok(Flow::Continue);
                }
                self.show_sessions();
            }

            "resume" => {
                let target = inv.args.trim();
                if target.is_empty() {
                    self.show_sessions(); // arms the bare-number selection
                } else {
                    let armed = self.pending_resume.take();
                    self.resume_by_target(target, armed)?;
                }
            }

            "compress" => self.compress().await?,

            "quit" => {
                // Python: /quit --delete also removes the session history.
                if inv.args.trim() == "--delete" {
                    let path = self.store.path().to_path_buf();
                    let _ = std::fs::remove_file(path);
                }
                return Ok(Flow::Quit);
            }

            other => self.out(&format!("command '{other}' is registered but not implemented yet")),
        }
        Ok(Flow::Continue)
    }

    // ── Session listing / resume ─────────────────────────────────────────────

    fn sessions_dir(&self) -> PathBuf {
        self.store
            .path()
            .parent()
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// Show the recent-sessions table and arm the one-shot bare-number
    /// selection (Python `_show_recent_sessions` + `_pending_resume_sessions`).
    fn show_sessions(&mut self) {
        let entries =
            session_switch::list_sessions(&self.sessions_dir(), Some(self.store.path()));
        if entries.is_empty() {
            self.out("No previous sessions found. Sessions are saved next to the current one.");
            return;
        }
        self.out(&session_switch::format_listing(&entries));
        self.pending_resume = Some(entries);
    }

    /// `/resume <n|id>` — resolve against the pending list when armed (it must
    /// be the same list the table showed, Python #34584), else a fresh listing.
    fn resume_by_target(&mut self, target: &str, armed: Option<Vec<SessionEntry>>) -> Result<()> {
        let entries = armed.unwrap_or_else(|| {
            session_switch::list_sessions(&self.sessions_dir(), Some(self.store.path()))
        });
        match session_switch::resolve_target(&entries, target) {
            Ok(entry) => self.switch_to(entry.clone()),
            Err(session_switch::ResolveError::OutOfRange(n)) => {
                self.out(&format!(
                    "{n} is out of range — /resume (or /sessions) shows the numbered list"
                ));
                Ok(())
            }
            Err(session_switch::ResolveError::NotFound(t)) => {
                self.out(&format!("session not found: {t} — try /sessions"));
                Ok(())
            }
        }
    }

    /// Point the REPL at another JSONL store: load its history, swap the
    /// store, disarm any pending selection. (Rust port of the Python
    /// `_end_current_session` + `_sync_agent_to_session` switch plumbing —
    /// the agent here is just `history` + `store`.)
    fn switch_to(&mut self, entry: SessionEntry) -> Result<()> {
        let store = SessionStore::open(&entry.path)?;
        let mut history = store.load()?;
        if history.is_empty() || !matches!(history.first().map(|m| &m.role), Some(Role::System)) {
            history.insert(0, Message::text(Role::System, self.system_prompt.as_str()));
        }
        let count = history.len();
        self.store = store;
        self.history = history;
        self.pending_resume = None;
        self.out(&format!(
            "resumed \"{}\" ({} messages)",
            entry.id, count
        ));
        Ok(())
    }

    // ── Turns ────────────────────────────────────────────────────────────────

    async fn start_conversation(&mut self, text: &str) -> Result<()> {
        let user = Message::text(Role::User, text);
        self.store.append(&user)?;
        self.history.push(user);
        self.run_turn().await
    }

    /// Run queued prompts one at a time (Python busy=queue default).
    async fn drain_queue(&mut self) {
        while let Some(next) = self.queued.pop() {
            self.out(&format!("(queued prompt) {next}"));
            if let Err(e) = self.start_conversation(&next).await {
                self.err(&format!("error: {e:#}"));
            }
        }
    }

    /// One streaming agent turn with live output and mid-turn input:
    /// `/stop` and `/new` cancel the in-flight turn (Python
    /// `interrupt_then_dispatch`), rejected commands print the busy notice,
    /// and plain text is queued for after the turn.
    async fn run_turn(&mut self) -> Result<()> {
        let host = CliToolHost;
        let mut queued = std::mem::take(&mut self.queued);
        let mut input_fut: InputFuture = self.io_next_line();

        loop {
            let mut stream = self.transport.stream(&self.history, &self.tools)?;
            let mut text = String::new();
            let mut calls: Vec<(String, String, String)> = Vec::new();
            let mut done = false;
            let mut interrupted = false;

            while !done {
                // Race the stream against the next input line so /stop can
                // interrupt a running turn (Python's concurrent-input model).
                // None = the stream arm won this round (keep racing input);
                // Some(None) = EOF on input; Some(Some(l)) = a line arrived.
                let mut input_result: Option<Option<String>> = None;
                tokio::select! {
                    item = stream.next() => match item {
                        Some(Ok(StreamItem::TextDelta(d))) => {
                            self.stream(&d);
                            text.push_str(&d);
                        }
                        Some(Ok(StreamItem::ToolCall { id, name, arguments })) => {
                            calls.push((id, name, arguments));
                        }
                        Some(Ok(StreamItem::Done)) | None => done = true,
                        Some(Err(e)) => {
                            // Keep the partial text but surface the error.
                            self.err(&format!("\nstream error: {e:#}"));
                            done = true;
                        }
                    },
                    line = input_fut.as_mut() => input_result = Some(line),
                }
                match input_result {
                    Some(Some(line)) => {
                        // Re-arm input for the remainder of the turn.
                        input_fut = self.io_next_line();
                        match slash::mid_turn_action(&line) {
                            MidTurn::Interrupt => {
                                interrupted = true;
                                break;
                            }
                            MidTurn::Dispatch => self.out(&slash::render_help()),
                            MidTurn::Busy(name) => self.out(&format!(
                                "agent is running — {name} was not sent (try /stop first)"
                            )),
                            MidTurn::Queue(t) => queued.push(t),
                        }
                    }
                    Some(None) => {
                        // EOF while streaming: stop racing input, let the turn drain.
                        input_fut = Box::pin(std::future::pending());
                    }
                    None => {}
                }
            }

            if interrupted {
                // Python interrupt semantics: the in-flight turn is cancelled;
                // the partial response is discarded, the user message stays.
                self.out("\n(interrupted — partial response discarded)");
                self.queued = queued;
                return Ok(());
            }
            self.out_raw("\n");

            let mut assistant = Message { role: Role::Assistant, content: Vec::new() };
            if !text.is_empty() {
                assistant.content.push(Content::Text { text: text.clone() });
            }
            for (id, name, arguments) in &calls {
                assistant.content.push(Content::ToolUse {
                    call: hermes_core::ToolCall {
                        id: id.clone(),
                        name: name.clone(),
                        arguments: arguments.clone(),
                    },
                });
            }
            self.store.append(&assistant)?;
            self.history.push(assistant);

            if calls.is_empty() {
                break;
            }

            for (id, name, arguments) in calls {
                // Tool cards render in place: call start, then its result
                // (Python renders the same pair in the TUI).
                self.tool_progress(&name, &arguments, None);
                let output = host
                    .execute(&name, &arguments)
                    .await
                    .unwrap_or_else(|e| format!("tool error: {e}"));
                self.tool_progress(&name, &arguments, Some(&output));
                let msg = Message {
                    role: Role::Tool,
                    content: vec![Content::ToolResult { call_id: id, output }],
                };
                self.store.append(&msg)?;
                self.history.push(msg);
            }
        }

        self.queued = queued;
        Ok(())
    }

    /// `/compress` — the Python command delegates to the context compressor,
    /// an LLM call, so this goes through the same provider transport: the
    /// conversation is summarized and the raw turns are folded into the
    /// summary (kept as a compact user turn, also persisted).
    async fn compress(&mut self) -> Result<()> {
        let turns = self.history.len();
        if turns <= 1 {
            self.out("(nothing to compress)");
            return Ok(());
        }
        let mut msgs = self.history.clone();
        msgs.push(Message::text(
            Role::User,
            "Summarize the conversation above into a compact brief with the durable facts, decisions, and open work. Output only the summary.",
        ));
        let mut stream = self.transport.stream(&msgs, &[])?;
        let mut summary = String::new();
        while let Some(item) = stream.next().await {
            match item? {
                StreamItem::TextDelta(d) => summary.push_str(&d),
                StreamItem::Done => break,
                _ => {}
            }
        }
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            self.out("(compress failed: empty summary — context kept)");
            return Ok(());
        }
        let compact = Message::text(
            Role::User,
            format!("(compacted conversation — summary follows)\n\n{summary}"),
        );
        self.store.append(&compact)?;
        self.history = vec![
            Message::text(Role::System, self.system_prompt.as_str()),
            compact,
        ];
        self.out(&format!(
            "(context compressed — {turns} messages folded into a summary)"
        ));
        Ok(())
    }
}

fn format_history_msg(msg: &Message) -> String {
    let role = if matches!(msg.role, Role::User) {
        "user"
    } else if matches!(msg.role, Role::Assistant) {
        "assistant"
    } else if matches!(msg.role, Role::System) {
        "system"
    } else {
        "tool"
    };
    let mut out = format!("[{role}]");
    for content in &msg.content {
        if let Content::Text { text } = content {
            out.push(' ');
            out.push_str(text);
        }
    }
    out
}

// ─── Tests: command table through the ReplIo seam ────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::test_transport::ScriptedTransport;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    type Output = Arc<Mutex<String>>;

    /// In-memory IO: scripted input lines, captured output. The optional
    /// `gate` holds reads (Pending) from the SECOND line on until the flag
    /// flips — the interrupt test uses it to make the mid-turn `/stop` race a
    /// pending stream deterministically (the first line starts the turn).
    struct TestIo {
        lines: Arc<Mutex<VecDeque<String>>>,
        out: Output,
        gate: Option<Arc<AtomicBool>>,
        reads: Arc<AtomicUsize>,
    }

    impl TestIo {
        fn shared(lines: &[&str], gate: Option<Arc<AtomicBool>>) -> (SharedIo, Output) {
            let out: Output = Arc::new(Mutex::new(String::new()));
            let lines: Arc<Mutex<VecDeque<String>>> = Arc::new(Mutex::new(
                lines.iter().map(|s| s.to_string()).collect(),
            ));
            let io: SharedIo = Arc::new(Mutex::new(TestIo {
                lines,
                out: out.clone(),
                gate,
                reads: Arc::new(AtomicUsize::new(0)),
            }));
            (io, out)
        }
    }

    impl ReplIo for TestIo {
        fn read_line(&mut self) -> InputFuture {
            let lines = self.lines.clone();
            let gate = self.gate.clone();
            let reads = self.reads.clone();
            Box::pin(futures::future::poll_fn(move |_| {
                let n = reads.fetch_add(1, Ordering::AcqRel);
                // Gate only mid-turn reads (second line onward), never the
                // line that starts the turn.
                if let Some(gate) = &gate {
                    if n >= 1 && !gate.load(Ordering::Acquire) {
                        return Poll::Pending;
                    }
                }
                match lines.lock().unwrap().pop_front() {
                    Some(line) => Poll::Ready(Some(line)),
                    // Ungated + empty = EOF so run() terminates; gated + empty
                    // stays Pending (nothing more to say mid-turn).
                    None if gate.is_some() => Poll::Pending,
                    None => Poll::Ready(None),
                }
            }))
        }
        fn write_out(&mut self, text: &str) {
            self.out.lock().unwrap().push_str(text);
        }
        fn write_err(&mut self, text: &str) {
            self.out.lock().unwrap().push_str(text);
        }
        fn flush_out(&mut self) {}
    }

    fn temp_session_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-repl-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn scripted_repl(tag: &str, lines: &[&str], turns: Vec<Vec<StreamItem>>) -> (Repl, Output, PathBuf) {
        let dir = temp_session_dir(tag);
        let store = SessionStore::open(dir.join("current.jsonl")).unwrap();
        let history = vec![Message::text(Role::System, SYSTEM_PROMPT)];
        let (io, out) = TestIo::shared(lines, None);
        let transport = Box::new(ScriptedTransport::new(turns)) as Box<dyn Transport>;
        let repl = Repl::new(io, transport, tool_specs(), store, history, "test-model".into(), SYSTEM_PROMPT.into());
        (repl, out, dir)
    }

    async fn run_repl(repl: &mut Repl) {
        repl.run().await.unwrap();
    }

    #[tokio::test]
    async fn help_quit_and_unknown_flow_through_the_table() {
        let (mut repl, out, dir) = scripted_repl("help", &["/help", "/bogus", "/quit"], vec![]);
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        assert!(out.contains("/resume [name] — Resume a previously-named session"), "got: {out}");
        assert!(out.contains("unknown command /bogus — try /help"));
        assert!(out.contains("bye"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn new_resets_history_and_store() {
        let (mut repl, out, dir) = scripted_repl("new", &["/new", "/quit"], vec![]);
        repl.store
            .append(&Message::text(Role::User, "old turn"))
            .unwrap();
        repl.history.push(Message::text(Role::User, "old turn"));
        run_repl(&mut repl).await;
        assert!(out.lock().unwrap().contains("(new session)"));
        assert_eq!(repl.history.len(), 1); // just the system prompt
        assert!(repl.store.load().unwrap().is_empty());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn model_switches_session_scoped() {
        let (mut repl, out, dir) =
            scripted_repl("model", &["/model gpt-5-mini", "/model", "/quit"], vec![]);
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        assert!(out.contains("model switched to gpt-5-mini (session-scoped"), "got: {out}");
        assert!(out.contains("model: gpt-5-mini"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn sessions_then_numbered_resume_switches_stores() {
        let (mut repl, out, dir) =
            scripted_repl("resume", &["/sessions", "/resume 2", "/quit"], vec![]);
        // Two other sessions; written newest-last so "second" is row 1.
        SessionStore::open(dir.join("first.jsonl"))
            .unwrap()
            .append(&Message::text(Role::User, "one"))
            .unwrap();
        std::thread::sleep(std::time::Duration::from_millis(120));
        let second = SessionStore::open(dir.join("second.jsonl")).unwrap();
        second.append(&Message::text(Role::User, "a")).unwrap();
        second.append(&Message::text(Role::User, "b")).unwrap();
        run_repl(&mut repl).await;

        let out = out.lock().unwrap().clone();
        assert!(out.contains("1. second (2 messages)"), "got: {out}");
        assert!(out.contains("2. first (1 messages)"));
        // Row 2 = "first" (1 stored message + injected system prompt).
        assert!(out.contains("resumed \"first\" (2 messages)"), "got: {out}");
        assert_eq!(repl.store.path().file_stem().unwrap(), "first");
        assert_eq!(repl.history.len(), 2);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn bare_resume_arms_a_bare_number_selection() {
        let (mut repl, out, dir) = scripted_repl("arm", &["/resume", "1", "/quit"], vec![]);
        SessionStore::open(dir.join("solo.jsonl"))
            .unwrap()
            .append(&Message::text(Role::User, "hi"))
            .unwrap();
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        assert!(out.contains("Recent sessions:"));
        assert!(out.contains("resumed \"solo\" (2 messages)"), "got: {out}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn out_of_range_and_missing_targets_are_reported() {
        let (mut repl, out, dir) =
            scripted_repl("range", &["/resume 9", "/resume <ghost>", "/quit"], vec![]);
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        assert!(out.contains("9 is out of range"), "got: {out}");
        assert!(out.contains("session not found: ghost"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn placeholder_targets_are_stripped() {
        let (mut repl, out, dir) = scripted_repl("ph", &["/resume <target>"], vec![]);
        SessionStore::open(dir.join("target.jsonl"))
            .unwrap()
            .append(&Message::text(Role::User, "hi"))
            .unwrap();
        run_repl(&mut repl).await;
        assert!(out.lock().unwrap().contains("resumed \"target\""));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn a_turn_streams_and_persists() {
        let (mut repl, out, dir) = scripted_repl(
            "turn",
            &["hello agent", "/quit"],
            vec![vec![
                StreamItem::TextDelta("hi ".into()),
                StreamItem::TextDelta("there".into()),
                StreamItem::Done,
            ]],
        );
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        // Deltas may interleave with mid-turn notices (select! polls in
        // random order), so check pieces, not contiguity.
        assert!(out.contains("hi"), "got: {out}");
        assert!(out.contains("there"), "got: {out}");
        let stored = repl.store.load().unwrap();
        assert_eq!(stored.len(), 2); // user + assistant
        let Content::Text { text } = &stored[1].content[0] else { panic!("not text") };
        assert_eq!(text, "hi there");
        assert!(matches!(stored[1].content[0], Content::Text { .. }));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn compress_folds_history_into_a_summary() {
        let (mut repl, out, dir) = scripted_repl(
            "compress",
            &["/compress", "/quit"],
            vec![vec![StreamItem::TextDelta("durable summary".into()), StreamItem::Done]],
        );
        repl.history.push(Message::text(Role::User, "turn one"));
        repl.store
            .append(&Message::text(Role::User, "turn one"))
            .unwrap();
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        assert!(out.contains("context compressed"), "got: {out}");
        assert_eq!(repl.history.len(), 2); // system + compact turn
        let compact = &repl.history[1];
        let Content::Text { text } = &compact.content[0] else { panic!("not text") };
        assert!(text.contains("durable summary"));
        assert_eq!(repl.store.load().unwrap().len(), 2); // user turn + compact
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn multiline_backslash_continuation_joins_lines() {
        let (mut repl, out, dir) = scripted_repl(
            "multiline",
            &["one \\", "two \\", "three", "/quit"],
            vec![vec![StreamItem::TextDelta("ok".into()), StreamItem::Done]],
        );
        run_repl(&mut repl).await;
        let stored = repl.store.load().unwrap();
        let Content::Text { text } = &stored[0].content[0] else { panic!("not text") };
        assert_eq!(text, "one \ntwo \nthree");
        assert!(out.lock().unwrap().contains("ok"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    // ── Interrupt semantics ──────────────────────────────────────────────────

    /// A transport that yields one delta on first poll (opening the gate so
    /// the mid-turn input becomes ready), then pends forever — the turn can
    /// only end via an interrupt.
    struct HangTransport {
        gate: Arc<AtomicBool>,
    }

    impl Transport for HangTransport {
        fn stream(
            &self,
            _messages: &[Message],
            _tools: &[ToolSpec],
        ) -> Result<std::pin::Pin<Box<dyn futures::Stream<Item = Result<StreamItem>> + Send>>>
        {
            let gate = self.gate.clone();
            Ok(Box::pin(futures::stream::poll_fn(move |_| {
                if !gate.swap(true, Ordering::AcqRel) {
                    Poll::Ready(Some(Ok(StreamItem::TextDelta("partial".into()))))
                } else {
                    Poll::Pending
                }
            })))
        }
    }

    #[tokio::test]
    async fn stop_mid_turn_cancels_and_keeps_state_clean() {
        let gate = Arc::new(AtomicBool::new(false));
        let (io, out) = TestIo::shared(&["hello", "/stop", "/quit"], Some(gate.clone()));
        let dir = temp_session_dir("interrupt");
        let store = SessionStore::open(dir.join("current.jsonl")).unwrap();
        let history = vec![Message::text(Role::System, SYSTEM_PROMPT)];
        let transport = Box::new(HangTransport { gate }) as Box<dyn Transport>;
        let mut repl = Repl::new(
            io,
            transport,
            tool_specs(),
            store,
            history,
            "test-model".into(),
            SYSTEM_PROMPT.into(),
        );
        run_repl(&mut repl).await;

        let out = out.lock().unwrap().clone();
        assert!(out.contains("interrupted — partial response discarded"), "got: {out}");
        // The user message stays; no assistant turn was appended.
        assert_eq!(repl.history.len(), 2);
        assert!(matches!(repl.history[1].role, Role::User));
        let stored = repl.store.load().unwrap();
        assert_eq!(stored.len(), 1);
        assert!(matches!(stored[0].role, Role::User));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn busy_policies_are_reported_mid_turn() {
        // Unit-level: the dispatcher's mid-turn verdicts.
        // (Interrupt/Queue covered end-to-end above; here the reject path.)
        assert!(matches!(slash::mid_turn_action("/model x"), MidTurn::Busy(_)));
        // The queued prompt runs after the turn.
        let (mut repl, out, dir) = scripted_repl(
            "queue",
            &["first", "second", "/quit"],
            vec![
                vec![StreamItem::TextDelta("one".into()), StreamItem::Done],
                vec![StreamItem::TextDelta("two".into()), StreamItem::Done],
            ],
        );
        run_repl(&mut repl).await;
        let out = out.lock().unwrap().clone();
        // "second" may be queued mid-turn or read at the next prompt
        // (select! poll order is random); both paths run the same turns.
        assert!(out.contains("two"), "got: {out}");
        let stored = repl.store.load().unwrap();
        // user "first", assistant "one", user "second", assistant "two"
        assert_eq!(stored.len(), 4);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn quit_delete_removes_the_session_file() {
        let (mut repl, _out, dir) = scripted_repl("quitdel", &["/quit --delete"], vec![]);
        let path = repl.store.path().to_path_buf();
        run_repl(&mut repl).await;
        assert!(!path.exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn registry_lookup_covers_every_name_and_alias() {
        // Registry drift guard: every /name in help resolves.
        for cmd in slash::COMMAND_REGISTRY {
            assert!(
                slash::resolve_command(cmd.name).is_some(),
                "registry lookup failed for {}",
                cmd.name
            );
            for alias in cmd.aliases {
                assert!(slash::resolve_command(alias).is_some());
            }
        }
    }

    #[test]
    fn format_history_renders_roles() {
        let msg = Message::text(Role::User, "hello");
        assert_eq!(format_history_msg(&msg), "[user] hello");
        let sys = Message::text(Role::System, "sys");
        assert_eq!(format_history_msg(&sys), "[system] sys");
    }
}
