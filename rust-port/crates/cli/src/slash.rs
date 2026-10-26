//! Slash-command registry — Rust port of the Python `hermes_cli/commands.py`
//! `COMMAND_REGISTRY` for the subset the Rust CLI implements. Names, aliases,
//! args hints, categories, and busy policies are taken verbatim from the
//! Python `CommandDef` rows.
//!
//! Consumers (the REPL dispatcher, `/help`, the mid-turn busy dispatcher)
//! all derive from `COMMAND_REGISTRY`, mirroring the Python module where CLI
//! help, gateway dispatch, and autocomplete share one table.

use std::fmt::Write as _;

/// Mid-run (agent busy) behavior, ported from the Python `busy_policy` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusyPolicy {
    /// Runs while a turn is in flight (Python `dispatch`).
    Dispatch,
    /// Refused mid-turn with the generic "agent is running" notice (`reject`).
    Reject,
    /// Cancels the in-flight turn first (`interrupt_then_dispatch`).
    InterruptThenDispatch,
}

#[derive(Debug)]
pub struct CommandDef {
    /// Canonical name without slash: "resume".
    pub name: &'static str,
    pub description: &'static str,
    pub category: &'static str,
    /// Alternative names: (["compact"]).
    pub aliases: &'static [&'static str],
    /// Argument placeholder: "[name]", "<prompt>".
    pub args_hint: &'static str,
    pub busy_policy: BusyPolicy,
}

const fn def(
    name: &'static str,
    description: &'static str,
    category: &'static str,
    aliases: &'static [&'static str],
    args_hint: &'static str,
    busy_policy: BusyPolicy,
) -> CommandDef {
    CommandDef { name, description, category, aliases, args_hint, busy_policy }
}

pub const COMMAND_REGISTRY: &[CommandDef] = &[
    // Session
    def("new", "Start a new session (fresh session ID + history)", "Session",
        &["reset"], "[name]", BusyPolicy::InterruptThenDispatch),
    def("clear", "Clear screen and start a new session", "Session",
        &[], "", BusyPolicy::Reject),
    def("stop", "Kill all running background processes", "Session",
        &[], "", BusyPolicy::InterruptThenDispatch),
    def("history", "Show conversation history", "Session",
        &[], "", BusyPolicy::Reject),
    def("compress", "Compress conversation context (LLM summary replaces the raw turns)", "Session",
        &["compact"], "[here [N] | --preview]", BusyPolicy::Reject),
    def("resume", "Resume a previously-named session", "Session",
        &[], "[name]", BusyPolicy::Reject),
    def("sessions", "Browse and resume previous sessions", "Session",
        &[], "[list | id-or-name]", BusyPolicy::Reject),
    // Configuration
    def("model", "Switch model (session-scoped; --global to persist)", "Configuration",
        &[], "[model]", BusyPolicy::Reject),
    // Info
    def("help", "Show available commands", "Info",
        &[], "", BusyPolicy::Dispatch),
    // Exit
    def("quit", "Exit the CLI (use --delete to also remove session history)", "Exit",
        &["exit"], "[--delete]", BusyPolicy::Reject),
];

/// Resolve a command name or alias (leading slash optional, case-insensitive) —
/// port of the Python `resolve_command`.
pub fn resolve_command(name: &str) -> Option<&'static CommandDef> {
    let key = name.trim().trim_start_matches('/').to_ascii_lowercase();
    COMMAND_REGISTRY.iter().find(|c| {
        c.name.eq_ignore_ascii_case(&key)
            || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(&key))
    })
}

/// One parsed dispatchable line.
#[derive(Debug)]
pub struct Invocation {
    pub def: &'static CommandDef,
    pub args: String,
}

/// How a raw input line is interpreted.
#[derive(Debug)]
pub enum ParsedLine {
    Command(Invocation),
    /// A `/word` that resolved to nothing (Python `cli_unknown_command`).
    Unknown(String),
    /// Plain conversation text (or a bare number with no armed resume list).
    Text,
}

/// Split "/name rest-of-line" into ("name", "rest-of-line").
fn split_name_args(line: &str) -> (&str, &str) {
    let rest = line.trim_start_matches('/').trim_start();
    match rest.find(char::is_whitespace) {
        Some(pos) => (&rest[..pos], rest[pos..].trim_start()),
        None => (rest, ""),
    }
}

pub fn parse_line(line: &str) -> ParsedLine {
    let trimmed = line.trim();
    if !trimmed.starts_with('/') {
        return ParsedLine::Text;
    }
    let (name, args) = split_name_args(trimmed);
    match resolve_command(name) {
        Some(def) => ParsedLine::Command(Invocation { def, args: args.to_string() }),
        None => ParsedLine::Unknown(trimmed.to_string()),
    }
}

/// What the dispatcher does with a line typed while a turn streams — port of
/// the Python Guard-2 mid-run dispatch (`busy_policy` + text queued).
#[derive(Debug, PartialEq, Eq)]
pub enum MidTurn {
    /// `/stop`, `/new`: cancel the in-flight turn, then handle.
    Interrupt,
    /// Dispatchable while busy (e.g. `/help`): handle without interrupting.
    Dispatch,
    /// Rejected with the "agent is running" notice.
    Busy(String),
    /// Plain text: queued for after the turn (Python busy=queue default).
    Queue(String),
}

pub fn mid_turn_action(line: &str) -> MidTurn {
    match parse_line(line) {
        ParsedLine::Command(inv) => match inv.def.busy_policy {
            BusyPolicy::InterruptThenDispatch => MidTurn::Interrupt,
            BusyPolicy::Dispatch => MidTurn::Dispatch,
            BusyPolicy::Reject => MidTurn::Busy(format!("/{}", inv.def.name)),
        },
        ParsedLine::Unknown(raw) => MidTurn::Busy(raw),
        ParsedLine::Text => MidTurn::Queue(line.trim().to_string()),
    }
}

/// Grouped help text in registry order — the CLI slice of the Python
/// `build_commands_by_category`.
pub fn render_help() -> String {
    let mut out = String::from("SLASH COMMANDS:\n");
    let mut last_category = "";
    for cmd in COMMAND_REGISTRY {
        if cmd.category != last_category {
            writeln!(&mut out, "\x20 {}", cmd.category).ok();
            last_category = cmd.category;
        }
        let mut names = format!("/{}", cmd.name);
        for alias in cmd.aliases {
            write!(&mut names, " /{alias}").ok();
        }
        let hint = if cmd.args_hint.is_empty() {
            String::new()
        } else {
            format!(" {}", cmd.args_hint)
        };
        writeln!(&mut out, "\x20   {names}{hint} — {}", cmd.description).ok();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_names_and_aliases_case_insensitively() {
        assert_eq!(resolve_command("new").unwrap().name, "new");
        assert_eq!(resolve_command("reset").unwrap().name, "new");
        assert_eq!(resolve_command("/COMPACT").unwrap().name, "compress");
        assert_eq!(resolve_command("exit").unwrap().name, "quit");
        assert_eq!(resolve_command("/sessions").unwrap().name, "sessions");
        assert!(resolve_command("/nosuch").is_none());
    }

    #[test]
    fn parses_commands_args_and_plain_text() {
        match parse_line("/model gpt-5-mini") {
            ParsedLine::Command(inv) => {
                assert_eq!(inv.def.name, "model");
                assert_eq!(inv.args, "gpt-5-mini");
            }
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_line("  /resume  <b>  ") {
            ParsedLine::Command(inv) => {
                assert_eq!(inv.def.name, "resume");
                assert_eq!(inv.args, "<b>");
            }
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_line("hello agent") {
            ParsedLine::Text => {}
            other => panic!("wrong parse: {other:?}"),
        }
        match parse_line("/nosuch x") {
            ParsedLine::Unknown(raw) => assert_eq!(raw, "/nosuch x"),
            other => panic!("wrong parse: {other:?}"),
        }
        // A bare "/" resolves to nothing.
        assert!(matches!(parse_line("/"), ParsedLine::Unknown(_)));
    }

    #[test]
    fn mid_turn_policies_match_the_python_busy_table() {
        assert_eq!(mid_turn_action("/stop"), MidTurn::Interrupt);
        assert_eq!(mid_turn_action("/new"), MidTurn::Interrupt);
        assert_eq!(mid_turn_action("/help"), MidTurn::Dispatch);
        assert_eq!(mid_turn_action("/model x"), MidTurn::Busy("/model".into()));
        assert_eq!(mid_turn_action("/nosuch"), MidTurn::Busy("/nosuch".into()));
        assert_eq!(mid_turn_action("hello there"), MidTurn::Queue("hello there".into()));
    }

    #[test]
    fn help_lists_commands_with_aliases_and_hints() {
        let help = render_help();
        assert!(help.contains("/resume [name] — Resume a previously-named session"));
        // Aliases render right after the name, before the args hint.
        assert!(help.contains("/compress /compact [here [N] | --preview]"), "got: {help}");
        assert!(help.contains("/exit"));
        assert!(help.contains("\n  Session\n"), "got: {help}");
        assert!(help.contains("\n  Exit\n"));
    }
}
