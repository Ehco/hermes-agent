//! Tool registry — `Vec<ToolSpec>` plus a dispatcher by name.
//!
//! Port of `tools/registry.py`'s dispatch contract from the Python
//! hermes-agent: every handler returns a JSON string, unknown tools and
//! failures surface as `{"error": ...}` instead of panicking. The `ToolHost`
//! in the cli crate delegates here.

use serde_json::json;
use serde_json::Value;

use hermes_core::model::ToolSpec;

use crate::{
    annotate_preview, browser, clarify, close_terminal, computer_use, cronjob, delegate_task,
    desktop_preview, desktop_project, discord, drive_preview, edit, execute_code, feishu,
    focus_pane, gui_tour, image_generate, list_dir, manage_catalog, manage_connections, maps,
    memory, process_manage, react_to_message, read_file, read_terminal, read_window_below, search,
    send_message, session_search, set_theme, setup_beat, show_tip, skills, terminal, todo_list,
    tool_search, tts, util, video_generate, vision, web_extract, web_search, write_file, x_search,
    xai_video, yuanbao,
};

pub struct ToolRegistry {
    specs: Vec<ToolSpec>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolRegistry {
    /// Build the registry with the schema of every built-in tool.
    pub fn new() -> Self {
        Self { specs: specs() }
    }

    /// Advertised tools (for the provider `tools` array).
    pub fn specs(&self) -> &[ToolSpec] {
        &self.specs
    }

    pub fn spec(&self, name: &str) -> Option<&ToolSpec> {
        self.specs.iter().find(|s| s.name == name)
    }

    /// Execute a tool by name with parsed JSON arguments. Always returns a
    /// JSON string (Python `registry.dispatch` contract).
    pub async fn dispatch(&self, name: &str, args: &Value) -> String {
        let args = args.clone();
        // Filesystem work is blocking; keep it off the async reactor when a
        // multi-threaded runtime is available.
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread {
                return tokio::task::spawn_blocking({
                    let name = name.to_string();
                    move || execute_sync(&name, &args)
                })
                .await
                .unwrap_or_else(|e| util::tool_error(format!("tool task join failed: {e}")));
            }
        }
        execute_sync(name, &args)
    }
}

/// Whether `name` is a registered built-in (free-standing lookup used by the
/// tool_search bridge for core/deferrable classification).
pub fn spec_exists(name: &str) -> bool {
    specs().iter().any(|s| s.name == name)
}

/// Synchronous dispatch used directly by sync callers and tests.
pub fn execute_sync(name: &str, args: &Value) -> String {
    match name {
        "read_file" => read_file::execute(args),
        "write_file" => write_file::execute(args),
        "edit" | "patch" => edit::execute(args),
        "list_dir" => list_dir::execute(args),
        "search" | "search_files" => search::execute(args),
        "terminal" => terminal::execute(args),
        "web_search" => web_search::execute(args),
        "web_extract" => web_extract::execute(args),
        "image_generate" => image_generate::execute(args),
        "memory" => memory::execute(args),
        "todo_list" | "todo" => todo_list::execute(args),
        "skills_list" => skills::execute_list(args),
        "skill_view" => skills::execute_view(args),
        "cronjob_manage" | "cronjob" => cronjob::execute(args),
        "session_search" => session_search::execute(args),
        "process_manage" => process_manage::execute(args),
        "x_search" => x_search::execute(args),
        "maps" => maps::execute(args),
        "text_to_speech" => tts::execute(args),
        "delegate_task" => delegate_task::execute(args),
        "skill_manage" => skills::execute_manage(args),
        // Structured questions to the user (Python tools/clarify_tool.py); the
        // UI is a platform-injected callback, so bare CLI execution is
        // unavailable-but-validating.
        "clarify" => clarify::execute(args),
        // Desktop-GUI tools (Python tools/desktop_ui toolset): validate args,
        // then round-trip through the desktop bridge; without one they surface
        // their faithful "only available in the Hermes desktop app" error.
        "set_theme" => set_theme::execute(args),
        "focus_pane" => focus_pane::execute(args),
        "show_tip" => show_tip::execute(args),
        "read_terminal" => read_terminal::execute(args),
        "read_window_below" => read_window_below::execute(args),
        "desktop_preview" => desktop_preview::execute(args),
        "drive_preview" => drive_preview::execute(args),
        "annotate_preview" => annotate_preview::execute(args),
        // Progressive tool disclosure bridges (Python tools/tool_search.py):
        // search the deferred-tool catalog, load schemas, invoke deferred tools.
        "tool_search" => tool_search::execute_search(args),
        "tool_describe" => tool_search::execute_describe(args),
        "tool_call" => tool_search::execute_call(args),
        // --- Batch 6 ---
        // Programmatic Tool Calling: Python cells in a persistent session
        // kernel with RPC access to a sandbox tool subset.
        "execute_code" => execute_code::execute(args),
        // Desktop Projects (named workspaces): create / switch / list.
        "desktop_project" => desktop_project::execute(args),
        // Video generation dispatches to an injected plugin provider.
        "video_generate" => video_generate::execute(args),
        // Cross-channel messaging (send / list / react / unreact) over the
        // injected sender/target registry.
        "send_message" => send_message::execute(args),
        // The agent's emoji tapback in the desktop app.
        "react_to_message" => react_to_message::execute(args),
        // Hosted connector accounts + local MCP servers; without the gateway
        // backends it degrades to the faithful unavailable error.
        "manage_connections" => manage_connections::execute(args),
        // Catalog search + install-through-the-card.
        "manage_catalog" => manage_catalog::execute(args),
        // Guided tour (highlight + narrate) round-trips the desktop bridge.
        "gui_tour" => gui_tour::execute(args),
        // --- Batch 7 ---
        // Multimodal analysis through the auxiliary vision LLM seam.
        "vision_analyze" => vision::execute_vision(args),
        "video_analyze" => vision::execute_video(args),
        // xAI Imagine video edit/extend (provider-gated like video_generate).
        "xai_video_edit" => xai_video::execute_edit(args),
        "xai_video_extend" => xai_video::execute_extend(args),
        // Feishu/Lark document + comment tools (client-injected seam).
        "feishu_doc_read" => feishu::execute_doc_read(args),
        "feishu_drive_list_comments" => feishu::execute_list_comments(args),
        "feishu_drive_list_comment_replies" => feishu::execute_list_replies(args),
        "feishu_drive_reply_comment" => feishu::execute_reply_comment(args),
        "feishu_drive_add_comment" => feishu::execute_add_comment(args),
        // Universal desktop control (cua-driver): backend-unavailable degradation.
        "computer_use" => computer_use::execute(args),
        // Hide a background process's terminal tab (desktop-bridge pattern).
        "close_terminal" => close_terminal::execute(args),
        // First-run onboarding: beat validation then the no-record degradation.
        "setup_beat" => setup_beat::execute(args),
        // The browser toolset: browser_exec CLI bridge, the login vault, raw
        // CDP, native dialogs, and the granular page tools — all validated
        // here, then degraded without a live browser session.
        "browser_exec" => browser::execute_exec(args),
        "browser_vault" => browser::execute_vault(args),
        "browser_cdp" => browser::execute_cdp(args),
        "browser_dialog" => browser::execute_dialog(args),
        name @ ("browser_navigate" | "browser_snapshot" | "browser_click" | "browser_type"
        | "browser_press" | "browser_scroll" | "browser_back" | "browser_console"
        | "browser_get_images" | "browser_vision") => browser::execute_granular(name, args),
        // Discord participation + server management (token-gated).
        "discord" => discord::execute(args),
        "discord_admin" => discord::execute_admin(args),
        // Yuanbao platform tools (adapter-injected).
        "yb_query_group_info" => yuanbao::execute_query_group_info(args),
        "yb_query_group_members" => yuanbao::execute_query_group_members(args),
        "yb_search_sticker" => yuanbao::execute_search_sticker(args),
        "yb_send_dm" => yuanbao::execute_send("yb_send_dm", args),
        "yb_send_sticker" => yuanbao::execute_send("yb_send_sticker", args),
        other => util::tool_error(format!("Unknown tool: {other}")),
    }
}

/// The `Vec<ToolSpec>` advertised to the model. Schemas are JSON Schema,
/// kept in sync with the Python agent's tool schemas (same parameter names,
/// defaults, and bounds).
pub fn specs() -> Vec<ToolSpec> {
    vec![
        ToolSpec {
            name: "read_file".into(),
            description: "Read a text file with line numbers and pagination. Use this instead of cat/head/tail in terminal. Output format: 'LINE_NUM|CONTENT'. Use offset and limit for large files. Reads exceeding ~100K characters are truncated on a line boundary and return a next_offset; continue with offset to read the rest.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to read (absolute, relative, or ~/path)"},
                    "offset": {"type": "integer", "description": "Line number to start reading from (1-indexed, default: 1)", "default": 1, "minimum": 1},
                    "limit": {"type": "integer", "description": "Maximum number of lines to read (default: 2000, max: 2000)", "default": 2000, "maximum": 2000}
                },
                "required": ["path"]
            }),
        },
        ToolSpec {
            name: "write_file".into(),
            description: "Write content to a file, completely replacing existing content. Use this instead of echo/cat heredoc in terminal. Creates parent directories automatically. OVERWRITES the entire file — use 'edit' for targeted edits.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Path to the file to write (will be created if it doesn't exist, overwritten if it does)"},
                    "content": {"type": "string", "description": "Complete content to write to the file"}
                },
                "required": ["path", "content"]
            }),
        },
        ToolSpec {
            name: "edit".into(),
            description: "Targeted find-and-replace edits in files. Use this instead of sed/awk in terminal. Finds a unique string and replaces it; returns the number of replacements.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "File path to edit."},
                    "old_string": {"type": "string", "description": "Exact text to find and replace. Must be unique in the file unless replace_all=true."},
                    "new_string": {"type": "string", "description": "Changed replacement text; it must differ from old_string. Pass empty string '' to delete the matched text."},
                    "replace_all": {"type": "boolean", "description": "Replace all occurrences instead of requiring a unique match (default: false)", "default": false}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        ToolSpec {
            name: "list_dir".into(),
            description: "List directory entries with types and sizes. Use this instead of ls in terminal. Entries are returned in deterministic name order; `limit` bounds the count.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Directory to list (default: current working directory)", "default": "."},
                    "limit": {"type": "integer", "description": "Maximum number of entries to return (default: 1000)", "default": 1000}
                }
            }),
        },
        ToolSpec {
            name: "search".into(),
            description: "Search file contents or find files by name. Use this instead of grep/rg/find/ls in terminal.\n\nContent search (target='content'): Regex search inside files, recursive. Output modes: full matches with line numbers, file paths only, or match counts.\n\nFile search (target='files'): match the regex against file names.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string", "description": "Regex pattern for content search, or name pattern for file search"},
                    "target": {"type": "string", "enum": ["content", "files"], "description": "'content' searches inside file contents, 'files' searches for files by name", "default": "content"},
                    "path": {"type": "string", "description": "Directory or file to search in (default: current working directory)", "default": "."},
                    "file_glob": {"type": "string", "description": "Filter files by pattern in content mode (e.g., '*.py' to only search Python files)"},
                    "limit": {"type": "integer", "description": "Maximum number of results to return (default: 50)", "default": 50},
                    "offset": {"type": "integer", "description": "Skip first N results for pagination (default: 0)", "default": 0},
                    "output_mode": {"type": "string", "enum": ["content", "files_only", "count"], "description": "Output format for grep mode: 'content' shows matching lines with line numbers, 'files_only' lists file paths, 'count' shows match counts per file", "default": "content"},
                    "context": {"type": "integer", "description": "Number of context lines before and after each match (content mode only)", "default": 0}
                },
                "required": ["pattern"]
            }),
        },
        ToolSpec {
            name: "terminal".into(),
            description: "Execute a shell command. Returns stdout, stderr and exit_code. Use read_file/search instead of cat/grep. Async subprocess with a timeout (default 60s, max 600s); an expired command is killed and `timed_out: true` is returned. With background=true the command runs detached: the call returns a session_id at once and process_manage polls/waits/kills it; with notify=true a completion notification is queued.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute"},
                    "cwd": {"type": "string", "description": "Working directory for the command (default: the process cwd)"},
                    "timeout": {"type": "integer", "description": "Max seconds (default 60, max 600). Above 600s the command is killed.", "default": 60, "maximum": 600},
                    "background": {"type": "boolean", "description": "Run detached; returns a session_id immediately. Manage the session with process_manage (poll/log/wait/kill/write/submit/close).", "default": false}
                },
                "required": ["command"]
            }),
        },
        ToolSpec {
            name: "web_search".into(),
            description: "Search the web for information. Returns up to 5 results by default with titles, URLs, and descriptions. The query is passed through to the configured backend, so operators such as site:domain, filetype:pdf, intitle:word, -term, and \"exact phrase\" may work when the backend supports them.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "The search query to look up on the web. You may include backend-supported operators such as site:example.com, filetype:pdf, intitle:word, -term, or \"exact phrase\"."},
                    "limit": {"type": "integer", "description": "Maximum number of results to return. Defaults to 5.", "minimum": 1, "maximum": 100, "default": 5}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "web_extract".into(),
            description: "Extract content from web page URLs. Returns clean page content in markdown/text (no LLM summarization — fast). Pages within the char budget (default 15000) return whole; larger pages return a head+tail window with a footer telling you the full text's saved file path and the read_file call to page through the omitted middle. Inline images appear as [IMAGE: alt] placeholders; real image URLs are kept as links. If a URL fails or times out, use the browser tool instead.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "urls": {
                        "type": "array",
                        "items": {"type": "string"},
                        "description": "List of URLs to extract content from (max 5 URLs per call)",
                        "maxItems": 5
                    },
                    "char_limit": {
                        "type": "integer",
                        "description": "Optional per-page character budget sent back (default 15000). Pages larger than this are head+tail truncated with the full text stored to disk. Raise it when you need more of a long page inline.",
                        "minimum": 2000
                    }
                },
                "required": ["urls"]
            }),
        },
        ToolSpec {
            name: "image_generate".into(),
            description: "Generate high-quality images from text prompts. Returns the result in the `image` field — a URL; reference it in your response using the current platform's file-delivery convention.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "The text prompt describing the desired image (text-to-image). Be detailed and descriptive."},
                    "aspect_ratio": {
                        "type": "string",
                        "enum": ["landscape", "square", "portrait"],
                        "description": "The aspect ratio of the generated image. 'landscape' is 16:9 wide, 'portrait' is 16:9 tall, 'square' is 1:1.",
                        "default": "landscape"
                    }
                },
                "required": ["prompt"]
            }),
        },
        ToolSpec {
            name: "memory".into(),
            description: "Save durable facts to persistent memory that survive across sessions (MEMORY.md = your notes, USER.md = user profile); both are injected into every future turn, so keep entries compact and high-signal. Make ALL changes to one target in ONE call via an 'operations' array — the batch applies atomically, so a single call can remove/patch stale entries to free room AND add new ones. Use the bare action/content/old_text fields only for a single lone change. 'patch' replaces exactly the old_text span (it must occur once in the entry); 'remove' deletes the whole entry.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "patch", "remove"], "description": "The action to perform (single-op shape). Omit when using 'operations'."},
                    "target": {"type": "string", "enum": ["memory", "user"], "description": "Which memory store: 'memory' for personal notes, 'user' for user profile."},
                    "content": {"type": "string", "description": "Required for 'add' and 'patch'. For 'add', the new entry. For 'patch', the text that replaces old_text; the rest of the entry is kept. Alias: 'new_text'."},
                    "old_text": {"type": "string", "description": "REQUIRED for 'patch' and 'remove': text from one entry. Omit for 'add'."},
                    "new_text": {"type": "string", "description": "Alias for 'content'. If both are set, 'content' wins."},
                    "operations": {
                        "type": "array",
                        "description": "Batch shape: operations applied atomically in one call against the final char budget. Each item is {action, content?, old_text?}.",
                        "items": {"type": "object", "properties": {"action": {"type": "string", "enum": ["add", "patch", "remove"]}, "content": {"type": "string"}, "new_text": {"type": "string"}, "old_text": {"type": "string"}}, "required": ["action"]}
                    }
                },
                "required": ["target"]
            }),
        },
        ToolSpec {
            name: "todo_list".into(),
            description: "Track a task list for multi-step work (3+ steps). List order is priority; only ONE item in_progress at a time. Break large phases into subtasks via parent. Mark an item completed only after the work is verified done. Call with no parameters to read the current list; always returns the full current list.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "description": "Task items to write.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "content": {"type": "string", "description": "Task description"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed", "cancelled"]},
                                "parent": {"type": "string", "description": "Optional id of another item, making this a nested subtask. Omit for top-level."}
                            },
                            "required": ["id", "content", "status"]
                        }
                    },
                    "merge": {"type": "boolean", "description": "true: update existing items by id, add new ones. false (default): replace the entire list with a fresh plan.", "default": false}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "skills_list".into(),
            description: "List available skills (name + description). Use skill_view(name) to load full content.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "category": {"type": "string", "description": "Optional category filter to narrow results"}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "skill_view".into(),
            description: "Load a skill's full content or access its linked files (references, templates, scripts). First call returns SKILL.md content plus a 'linked_files' listing; call again with file_path to access those.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "The skill name (frontmatter name or its directory name)"},
                    "file_path": {"type": "string", "description": "OPTIONAL: path to a linked file within the skill, e.g. 'references/api.md' or 'templates/config.yaml'"}
                },
                "required": ["name"]
            }),
        },
        ToolSpec {
            name: "cronjob_manage".into(),
            description: "Manage scheduled cron jobs: action='create' schedules a job from a self-contained prompt ('schedule' and 'prompt' are REQUIRED); 'list' inspects jobs; 'update'/'pause'/'resume'/'remove' manage one by job_id (always list first — never guess job IDs). Jobs run in a fresh session with no current-chat context, so prompts must be self-contained.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "description": "One of: create, list, update, pause, resume, remove."},
                    "job_id": {"type": "string", "description": "Required for update/pause/resume/remove."},
                    "prompt": {"type": "string", "description": "For create: the full self-contained prompt. For update: replaces the prompt."},
                    "schedule": {"type": "string", "description": "REQUIRED for create. Forms: interval '30m'/'every 2h'; one-shot 'in 30m' or ISO '2026-06-01T09:00:00'; 'every monday 9am'/'weekdays at 9am'; cron '0 9 * * *'."},
                    "name": {"type": "string", "description": "Optional human-friendly name"},
                    "repeat": {"type": "integer", "description": "Optional repeat count (max runs). Omit for defaults (once for one-shot, forever for recurring)."},
                    "paused": {"type": "boolean", "description": "Create only: persist disabled atomically. Resume to schedule. Default false.", "default": false},
                    "paused_reason": {"type": "string", "description": "Create only: auditable reason; requires paused=true."},
                    "include_disabled": {"type": "boolean", "description": "List only: include disabled jobs. Default true.", "default": true}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "session_search".into(),
            description: "Recall past conversation turns: search the session transcript by keyword (top result fully hydrated, lower results carry snippets), or scroll inside it with around_message_id + window; no args = a session summary with a recent tail. Searches conversation history ONLY — when the user gave a direct source (URL, file, contact, live system), inspect that first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query (discovery shape): keywords to find in past conversation text. Omit to summarize the session."},
                    "limit": {"type": "integer", "description": "Discovery only. Max results to return (default 3, max 10).", "default": 3},
                    "role_filter": {"type": "string", "description": "Optional comma-separated roles to include. Defaults to 'user,assistant'."},
                    "around_message_id": {"type": "integer", "description": "Scroll shape. 0-based message ordinal to center the window on (match_message_id from a prior result)."},
                    "window": {"type": "integer", "description": "Scroll only. Messages to return on each side of the anchor (anchor included). Clamped to [1, 20]. Default 5.", "default": 5}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "process_manage".into(),
            description: "Poll, wait on, or kill background terminal processes (from terminal(background=true)). Completed results remain retrievable by session_id when resuming their owning conversation (up to 7 days, newest 64 results per profile; rolling output tail). poll: status + new output. log: full output, paged. wait: block until exit or timeout (partial output on timeout). write vs submit: submit appends Enter — use it to answer prompts; write sends raw bytes, no newline. close: EOF stdin. kill: terminate. handoff (subagents only): transfer a running process you started to your parent agent, which then receives its completion; `data` = one sentence on its purpose. Subagent-owned processes are otherwise killed when the subagent finishes and their notifications never reach the parent.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "poll", "log", "wait", "kill", "write", "submit", "close", "handoff"], "description": "The verb; write-vs-submit is the one real trap (a lone \\n on a Windows PTY is not Enter)."},
                    "session_id": {"type": "string", "description": "From terminal background output; any unique prefix works ('4dae' for proc_4dae56ca81f6). Required except for 'list'."},
                    "data": {"type": "string", "description": "Stdin text for write/submit; purpose sentence for handoff."},
                    "timeout": {"type": "integer", "description": "Max seconds for 'wait'.", "minimum": 1},
                    "offset": {"type": "integer", "description": "Log line offset (default: last 200)."},
                    "limit": {"type": "integer", "description": "Max log lines.", "minimum": 1}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "x_search".into(),
            description: "Search X (Twitter) posts, profiles, and threads using xAI's built-in X Search tool. Read-only discovery only: use this for current discussion, reactions, or claims on public X rather than general web pages. Do not use it to post, reply, like, DM, upload media, delete, or inspect the user's authenticated X account — those require a separate authenticated X API surface outside this tool. Available when xAI credentials are configured (SuperGrok OAuth or XAI_API_KEY).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "What to look up on X."},
                    "allowed_x_handles": {"type": "array", "items": {"type": "string"}, "description": "Optional list of X handles to include exclusively (max 10)."},
                    "excluded_x_handles": {"type": "array", "items": {"type": "string"}, "description": "Optional list of X handles to exclude (max 10)."},
                    "from_date": {"type": "string", "description": "Optional start date in YYYY-MM-DD format."},
                    "to_date": {"type": "string", "description": "Optional end date in YYYY-MM-DD format."},
                    "enable_image_understanding": {"type": "boolean", "description": "Whether xAI should analyze images attached to matching X posts.", "default": false},
                    "enable_video_understanding": {"type": "boolean", "description": "Whether xAI should analyze videos attached to matching X posts.", "default": false}
                },
                "required": ["query"]
            }),
        },
        ToolSpec {
            name: "maps".into(),
            description: "Geocode, reverse-geocode, road distance, turn-by-turn directions, and timezone lookup — powered by OpenStreetMap/Nominatim, OSRM, and TimeAPI.io (no API key needed). action='search' geocodes a free-text place; 'reverse' names coordinates; 'distance'/'directions' route between two places (mode: driving, walking, cycling); 'timezone' reads the timezone for lat/lon.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["search", "reverse", "distance", "directions", "timezone"], "description": "The lookup verb."},
                    "query": {"type": "string", "description": "search only: the free-text place to geocode."},
                    "from": {"type": "string", "description": "distance/directions only: origin place (free text)."},
                    "to": {"type": "string", "description": "distance/directions only: destination place (free text)."},
                    "mode": {"type": "string", "enum": ["driving", "walking", "cycling"], "description": "distance/directions only."},
                    "lat": {"type": "number", "description": "reverse/timezone only: latitude [-90, 90]."},
                    "lon": {"type": "number", "description": "reverse/timezone only: longitude [-180, 180]."},
                    "limit": {"type": "integer", "description": "search only: max results (default 5).", "default": 5}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "text_to_speech".into(),
            description: "Convert text to speech audio. Writes an audio file (default: the Hermes audio cache, <timestamp>.mp3) and returns its path. Voice and provider are user-configured; the call picks the configured OpenAI-compatible speech endpoint.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The text to convert to speech. Longer input is split into ordered chunks without silent truncation."},
                    "output_path": {"type": "string", "description": "Optional custom file path to save the audio. Defaults to the Hermes audio cache."},
                    "speed": {"type": "number", "description": "Playback speed multiplier. 1.0 = normal. Range: 0.25-4.0."},
                    "instructions": {"type": "string", "description": "Optional voice-design guidance: tone, emotion, pacing, accent, whispering."}
                },
                "required": ["text"]
            }),
        },
        ToolSpec {
            name: "delegate_task".into(),
            description: "Spawn one or more subagents in isolated contexts: reasoning-heavy subtasks, work that would flood your context, or independent parallel workstreams. Children know nothing of this conversation: pass everything needed via 'context'. While children run, action (list/steer/stop) controls them live.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "tasks": {"type": "array", "minItems": 1, "items": {
                        "type": "object",
                        "properties": {
                            "goal": {"type": "string", "description": "What this subagent should accomplish. Be specific and self-contained — it knows nothing about your conversation history."},
                            "context": {"type": "string", "description": "Background THIS child needs: file paths, error messages, constraints. Repeat shared background in every task that needs it."}
                        },
                        "required": ["goal"]
                    }, "description": "The task(s); each entry spawns one subagent with isolated context and terminal session. Required when spawning."},
                    "action": {"type": "string", "enum": ["spawn", "list", "steer", "stop"], "description": "Default 'spawn'. Live control of running children: 'list' = ids/goals/status; 'steer' = queue course-correction text into one child; 'stop' = end one child early (partial result still returns)."},
                    "subagent_id": {"type": "string", "description": "Target for action='steer'/'stop' (ids from the spawn response or action='list')."},
                    "message": {"type": "string", "description": "For action='steer': the course correction, appended to the child's next tool result mid-run. Be directive and specific."}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "skill_manage".into(),
            description: "Create, update, or delete skills — your procedural memory for recurring task types. The call is an operations array (a single edit is a list of one); it applies atomically — any failure rolls every touched skill back. Ops: create (full SKILL.md; lands in the profile's skills directory; must precede that skill's other ops), patch (targeted old_string/new_string fix — preferred; content alone REPLACES the whole file, read it via skill_view first), write_file/remove_file (supporting files), delete (sole op only). Existing skills are modified wherever they live.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "operations": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string"},
                            "action": {"type": "string", "enum": ["create", "patch", "write_file", "remove_file", "delete"]},
                            "content": {"type": "string"},
                            "category": {"type": "string"},
                            "file_path": {"type": "string"},
                            "file_content": {"type": "string"},
                            "old_string": {"type": "string"},
                            "new_string": {"type": "string"},
                            "replace_all": {"type": "boolean", "default": false},
                            "absorbed_into": {"type": "string"}
                        },
                        "required": ["name", "action"]
                    }, "description": "Ordered ops; each names its target skill (lowercase, hyphens/underscores, max 64 chars)."}
                },
                "required": ["operations"]
            }),
        },
        ToolSpec {
            name: "tool_search".into(),
            description: "Search additional tools that are loaded on demand. Send one keyword query per capability you need; returns matching tool names and descriptions. Then `tool_describe` loads schemas and `tool_call` invokes. Tools already in your tool list need no search.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "queries": {"type": "array", "items": {"type": "string"}, "description": "One query per capability: service + action + object keywords (e.g. ['gmail send email', 'github create issue']), never a question; every word must match tool text. Searched in parallel."},
                    "limit": {"type": "integer", "description": "Max matches per query (default 5)."}
                },
                "required": ["queries"]
            }),
        },
        ToolSpec {
            name: "tool_describe".into(),
            description: "Load the full JSON schemas for tools returned by `tool_search`. Required before `tool_call` if a tool's parameters are unknown. Batch every schema you need into one call.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "names": {"type": "array", "items": {"type": "string"}, "description": "Exact tool names (as returned by tool_search). A single string is accepted and treated as one name."}
                },
                "required": ["names"]
            }),
        },
        ToolSpec {
            name: "tool_call".into(),
            description: "Invoke deferred tools: `calls` is an array of {name, arguments}, one entry per invocation (batch independent calls; results come back in order). Arguments match the schema from `tool_describe`; approvals and hooks apply as usual.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "calls": {"type": "array", "items": {
                        "type": "object",
                        "properties": {
                            "name": {"type": "string", "description": "Exact tool name to invoke."},
                            "arguments": {"type": "object", "description": "Arguments matching the tool schema."}
                        },
                        "required": ["name", "arguments"]
                    }, "description": "One or more independent deferred tool invocations."}
                },
                "required": ["calls"]
            }),
        },
        // --- Batch 5: clarify + the desktop_ui toolset ---
        ToolSpec {
            name: "clarify".into(),
            description: "Ask the user one or more questions when you need a decision, clarification, or feedback before proceeding. Pass every question in `questions` (1-5 entries) — a single question is a one-entry array, and several INDEPENDENT questions belong in ONE call. Per question: single-select (up to 4 choices — put your recommended option FIRST, the UI marks it '(Recommended)' and auto-appends an 'Other' free-text row), multi-select (multi_select=true), or open-ended (omit choices). Options go ONLY in `choices`, never enumerated inside the question text. Result: {responses: [...], outcome} in question order; each response has status answered, skipped or unanswered (user_response is null unless answered); outcome is submitted, cancelled, timed_out or undelivered, with a notice saying why when the wait ended without a submit.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "minItems": 1,
                        "maxItems": 5,
                        "description": "The question(s). Each: question text (options excluded), optional choices (recommended first; omit for free-text), optional multi_select. Responses come back in question order with the question text echoed.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {"type": "string"},
                                "choices": {"type": "array", "items": {"type": "string"}, "maxItems": 4},
                                "multi_select": {"type": "boolean"}
                            },
                            "required": ["question"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        },
        ToolSpec {
            name: "set_theme".into(),
            description: "Change how the Hermes desktop app looks: accent colour, light/dark mode, layout. Pass the values to apply them now; the result says what was applied. Pass picker instead to show the user an accent or layout picker card in the chat and wait for their pick. To reveal ONE pane, use focus_pane instead.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "accent": {"type": "string", "description": "A swatch id (mono, green, cyan, nous, violet, pink, red, orange), 'theme' for the theme's own accent, or a hex colour '#rrggbb'."},
                    "mode": {"type": "string", "enum": ["light", "dark", "system"]},
                    "layout": {"type": "string", "description": "Layout preset id. Simple mode: sidebar-left, sidebar-right. Advanced mode: default, basic, focus, terminal-deck, quad. Or the user's saved layouts by id."},
                    "picker": {"type": "string", "enum": ["accent", "layout"], "description": "Show that picker card and wait for the user (up to 5 minutes). Pass it alone."}
                }
            }),
        },
        ToolSpec {
            name: "focus_pane".into(),
            description: "Reveal and focus a Hermes desktop pane when the user asks to see it: chat, files, terminal, review (git diff), or sessions. For URLs/files use the desktop_preview tool instead.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "pane": {"type": "string", "enum": ["chat", "files", "terminal", "review", "sessions"], "description": "Which pane to reveal."}
                },
                "required": ["pane"]
            }),
        },
        ToolSpec {
            name: "show_tip".into(),
            description: "Point at one thing in the desktop UI with a small arrow bubble (no dimming, no tour chrome) — for when a sentence is clearer with a finger on its subject. Get selectors from tour(action='targets'), prefer stable:true, never guess. One tip at a time (new replaces last); say the same thing in chat too — the bubble is a pointer, not the message. Sparingly: a bubble every turn stops being read.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "text": {"type": "string", "description": "The one-sentence bubble text."},
                    "selector": {"type": "string", "description": "Selector from tour targets."},
                    "title": {"type": "string", "description": "Optional heading."},
                    "side": {"type": "string", "enum": ["top", "right", "bottom", "left"], "description": "Omit for 'top'; flips at screen edges."}
                },
                "required": ["text", "selector"]
            }),
        },
        ToolSpec {
            name: "read_terminal".into(),
            description: "Read the in-app terminal pane beside this chat. No args = visible screen + total_lines; page scrollback with start_line (0 = oldest) + count. JSON: {total_lines, start, end, viewport_rows, cursor_row, text}.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "start_line": {"type": "integer", "description": "0-indexed first line (0 = oldest). Omit for the visible screen."},
                    "count": {"type": "integer", "description": "Lines to read from start_line. Defaults to the visible row count."}
                }
            }),
        },
        ToolSpec {
            name: "read_window_below".into(),
            description: "Identify the app window directly behind the Hermes desktop window (what the user is working in). JSON: {window: {app, title, bounds, id}, frontmost, platform}. title may be empty when the OS withholds it (noted in `note`); where windows cannot be enumerated at all, {error, platform} says what would fix it — relay that instead of retrying. Metadata only; never captures pixels.".into(),
            schema: json!({"type": "object", "properties": {}}),
        },
        ToolSpec {
            name: "desktop_preview".into(),
            description: "Open, close, or read the preview pane beside the chat. open: show a web URL (bare domains fine), a localhost dev server, or a file path (HTML renders live) — opens for the current window only. close: dismiss the whole pane, or one tab via url. read: what the pane currently shows — returns {kind, url, title, text, start, end, total_chars}; a Browser tab's text is the rendered page's visible text, paged with start/count (char offsets); a file tab answers identity only (read the file with read_file).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["open", "close", "read"]},
                    "url": {"type": "string", "description": "open: the target. close: one tab (omit for the whole pane)."},
                    "label": {"type": "string", "description": "open: optional tab label."},
                    "start": {"type": "integer", "description": "read: 0-indexed char offset."},
                    "count": {"type": "integer", "description": "read: chars to return (capped per read)."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "drive_preview".into(),
            description: "Use the web page open in the desktop preview pane (the one `desktop_preview` opens): log in, fill forms, click through flows. ALWAYS start with action='elements' — it inventories clickable/typable things as refs ('btn-sign-in') with role/label/value; act by ref, not guessed selectors. Refs survive re-renders and only die on navigation. After the first full inventory, actions answer with a DELTA: 'added' in full, 'changed' as ref + moved fields, 'removed'/'rebound' as ref lists. Input is real. Actions: elements, click, hover, type (submit=true also presses Enter), scroll, press, strobe (visual flourish only — never loop it), back/forward/reload. Page text only: desktop_preview action=read.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["elements", "click", "hover", "type", "scroll", "press", "strobe", "back", "forward", "reload"], "description": "Start with 'elements'."},
                    "ref": {"type": "string", "description": "Element ref from an earlier elements call."},
                    "selector": {"type": "string", "description": "CSS selector fallback. Prefer ref."},
                    "text": {"type": "string", "description": "type: the text."},
                    "submit": {"type": "boolean", "description": "type: press Enter + submit the form after."},
                    "key": {"type": "string", "description": "press: key name ('Enter', 'Escape', 'ArrowDown')."},
                    "allow_shortcut": {"type": "boolean", "description": "press: allow a printable key on body/html. Off by default."},
                    "amount": {"type": "integer", "description": "scroll: pixels (negative = up; default ~one screen)."},
                    "to": {"type": "string", "enum": ["top", "bottom"], "description": "scroll: jump to top/bottom instead."},
                    "max": {"type": "integer", "description": "elements: cap the inventory."},
                    "full": {"type": "boolean", "description": "elements: full re-read instead of a delta. Rarely needed."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "annotate_preview".into(),
            description: "Highlight elements on the preview-pane page, lastingly (drive_preview's own marks fade; annotations stay until removed) — point at findings, flag what you're about to change, keep your place. Use the refs from drive_preview action='elements'. add: outline one element (optional short label, drawn on the page). hold: freeze the whole visible field, every element outlined and named. remove/clear: take one/all down. Marks follow their element on scroll; navigation clears them.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["add", "hold", "remove", "clear"], "description": "Defaults to 'add'."},
                    "ref": {"type": "string", "description": "Ref from drive_preview elements."},
                    "selector": {"type": "string", "description": "CSS selector fallback. Prefer ref."},
                    "label": {"type": "string", "description": "Optional caption, e.g. 'cheapest'."}
                },
                "required": []
            }),
        },
        // --- Batch 6 ---
        ToolSpec {
            name: "execute_code".into(),
            description: "Run Python that calls Hermes tools programmatically. Use when you need 3+ tool calls with logic between them: filtering/reducing large outputs before they enter context, branching, or loops (N pages/files, retry on failure). Use normal tool calls for single calls, results you must reason over in full, or anything needing user interaction.\n\nCalls run in a persistent session kernel: variables, imports, and loaded data survive across execute_code calls, so build on earlier work instead of re-loading it. A timed-out or interrupted call loses that state.\n\nAvailable via `from hermes_tools import ...`: web_search, web_extract, read_file, write_file, search_files, patch, terminal, ...\n\nLimits: 5-minute timeout, max 50 tool calls per call. Stdout over 50KB shows head/tail inline; the FULL text is auto-saved to a file whose path rides in the result.\n\nHelpers require imports: `from hermes_tools import json_parse, shell_quote, retry`. json_parse(text) — tolerant json.loads for terminal() output; shell_quote(s) — shlex.quote for dynamic shell args; retry(fn, max_attempts=3, delay=2) — exponential backoff.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Python code to execute. Import tools with `from hermes_tools import web_search, terminal, ...` and print your final result to stdout."},
                    "reset": {"type": "boolean", "description": "Discard the kernel's persistent state and start fresh before running this code."}
                },
                "required": ["code"]
            }),
        },
        ToolSpec {
            name: "desktop_project".into(),
            description: "Create or switch desktop Projects (named workspaces). create: one and switch this chat into it — pass path to anchor it to a repo/folder (the chat's workspace moves there, the sidebar follows). switch: move this chat into an existing project by name/slug/id — the intentional way to move the session, not `cd`. list: all projects + which one this chat is in.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["create", "switch", "list"]},
                    "name": {"type": "string", "description": "create: human name. switch: name, slug, or id."},
                    "path": {"type": "string", "description": "create: repo/folder to anchor to."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "video_generate".into(),
            description: "Generate a video from a text prompt (text-to-video), animate a still image (image-to-video), or guide generation with reference images. The backend and model family are user-configured; the agent does not pick them. Long-running generations may take 30 seconds to several minutes — the call blocks until the video is ready. Returns the result in the `video` field — either an HTTP URL or an absolute file path.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Text instruction describing the desired video, motion, subject, style, camera movement, etc."},
                    "duration": {"type": "integer", "description": "Desired video duration in seconds. Providers clamp to their supported range. Omit for the provider default."},
                    "aspect_ratio": {"type": "string", "enum": ["16:9", "1:1", "9:16", "4:3", "3:2"], "description": "Output aspect ratio.", "default": "16:9"},
                    "resolution": {"type": "string", "enum": ["720p", "1080p"], "description": "Output resolution.", "default": "1080p"}
                },
                "required": ["prompt"]
            }),
        },
        ToolSpec {
            name: "send_message".into(),
            description: "Cross-channel messaging via platform APIs: action='send' delivers text (optionally with [[path]] media attachments) to `platform:target`; 'list' shows the channel directory; 'react'/'unreact' add or retract an emoji on a message. Without a ref the platform's home channel is used.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["send", "list", "react", "unreact"], "description": "Defaults to send."},
                    "target": {"type": "string", "description": "The destination as `platform:target` (e.g. 'telegram:chat_id'); omit the target for the home channel."},
                    "message": {"type": "string", "description": "send: the text. Media files ride inline as [[path]]."},
                    "emoji": {"type": "string", "description": "react: the emoji to add."},
                    "message_id": {"type": "string", "description": "react: the specific message (defaults to the latest)." }
                }
            }),
        },
        ToolSpec {
            name: "react_to_message".into(),
            description: "React to a message with a single emoji, the way you'd tapback in iMessage. Use it like a person would: occasionally, when felt — not on every message. NEVER narrate or explain a reaction. Defaults to the user's most recent message. One reaction per message: a different emoji replaces yours, an empty string retracts it.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "emoji": {"type": "string", "description": "The emoji to react with (e.g. '❤️', '😂', '👍'). Pass an empty string to remove your reaction."},
                    "message_row_id": {"type": "integer", "description": "Optional. The specific message to react to. Omit to react to the user's latest message."},
                    "messages_back": {"type": "integer", "description": "Optional. React to an EARLIER user message: 1 = the one before the latest, 2 = two before."}
                },
                "required": ["emoji"]
            }),
        },
        ToolSpec {
            name: "manage_connections".into(),
            description: "Connect the user to apps. Two kinds: a hosted connector account (Gmail, Notion, ...) served through the tool gateway, and a local MCP server from the bundled catalog. Targets go in 'connectors': a bare slug or {\"name\": \"gmail\"} is a hosted connector account; {\"name\": \"linear\", \"mcp\": true} is a local MCP server. Hosted actions: status, display, connect, reconnect, rename. MCP actions (every target carries \"mcp\": true): install, enable, authorize. This tool can NOT disconnect, delete, or revoke an account — that is deliberately user-only; direct the user to the Nous Portal (their org's Connectors page) or the desktop app.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["display", "status", "connect", "reconnect", "rename", "install", "enable", "authorize"], "description": "Defaults to status. connect, reconnect and rename take hosted connector slugs only. install, enable and authorize take mcp:true targets only."},
                    "connectors": {"type": "array", "items": {"anyOf": [{"type": "string"}, {"type": "object", "properties": {"name": {"type": "string"}, "mcp": {"type": "boolean"}, "alias": {"type": "string"}, "to": {"type": "string"}}, "required": ["name"]}], "description": "Targets. REQUIRED for every action but status and display; a bare slug is a hosted connector, so an MCP server needs the object form with \"mcp\": true."}},
                    "force": {"type": "boolean", "description": "reconnect only: restart the authorization even if the app is connected (account switch)."}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "manage_catalog".into(),
            description: "Find and install Hermes catalog plugins and hub skills for the user. 'search' lists matches (id, kind, display, tier, platforms, installed) and changes nothing. 'install' shows the user one approval card with a row per item and blocks until every row is installed, skipped or failed; the host installs each approved row into this chat's profile at the catalog's reviewed version. Pass only catalog ids exactly as 'search' returns them. Offer an install only for something the user asked for or agreed to.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["search", "install"]},
                    "kind": {"type": "string", "enum": ["plugin", "skill"], "description": "search only: limit results to plugins or skills."},
                    "query": {"type": "string", "description": "search only: words to match; empty lists plugins."},
                    "items": {"type": "array", "description": "install only: the catalog items to offer, one card row each.", "items": {"type": "object", "properties": {"kind": {"type": "string", "enum": ["plugin", "skill"]}, "id": {"type": "string"}}, "required": ["kind", "id"], "additionalProperties": false}},
                    "reason": {"type": "string", "description": "install only: one line on why these are offered."}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        },
        ToolSpec {
            name: "gui_tour".into(),
            description: "Guided tour in the desktop GUI: dim the screen, highlight an element, attach a titled popover. Surfaces: 'app' (Hermes itself) or 'preview' (the page in the preview pane). A general look around the app is ONE call: action='start' with no steps runs the app's built-in tour (preset 'quick' or 'full', default full). For a custom tour, call action='targets' first — prefer targets marked stable:true; re-scan if one stops matching. Then narrate with action='show' (one highlight per call, replaces the last — pair each with a chat message) or hand over with action='start' + steps (user gets Next/Prev; 'next'/'prev' also page it). 'stop' clears.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["targets", "show", "start", "next", "prev", "stop"], "description": "start alone runs the built-in tour; targets first for custom steps."},
                    "surface": {"type": "string", "enum": ["app", "preview"], "description": "'app' (default) or 'preview'."},
                    "selector": {"type": "string", "description": "show: selector from targets (prefer stable). Omit = centered narration."},
                    "title": {"type": "string", "description": "show: popover title."},
                    "text": {"type": "string", "description": "show: popover body."},
                    "side": {"type": "string", "enum": ["top", "right", "bottom", "left"], "description": "show: popover side; omit to auto-place."},
                    "steps": {"type": "array", "description": "start: ordered steps; omit for the built-in tour.", "items": {"type": "object", "properties": {"selector": {"type": "string"}, "title": {"type": "string"}, "text": {"type": "string"}, "side": {"type": "string", "enum": ["top", "right", "bottom", "left"]}}}},
                    "preset": {"type": "string", "enum": ["quick", "full"], "description": "start without steps: 'quick' (four essentials) or 'full'."},
                    "step_index": {"type": "integer", "description": "start: 0-indexed first step."}
                },
                "required": ["action"]
            }),
        },
        // --- Batch 7: vision/video analysis, xAI video, Feishu, computer_use,
        // close_terminal, setup_beat, the browser toolset, Discord, Yuanbao.
        ToolSpec {
            name: "vision_analyze".into(),
            description: "Load an image into the conversation so you can see it. Call it any time the user references an image — then answer from what you see.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "image_url": {"type": "string", "description": "Image URL (http/https), local file path, or data: URL to load."},
                    "question": {"type": "string", "description": "Your question or request about the image."},
                    "region": {"type": "array", "items": {"type": "integer"}, "minItems": 4, "maxItems": 4, "description": "Optional [x1, y1, x2, y2] crop in ORIGINAL-image pixel coordinates, applied before any downscaling — the crop keeps full resolution. Load the full image first, then re-call with a region to zoom into small text or fine detail."}
                },
                "required": ["image_url", "question"]
            }),
        },
        ToolSpec {
            name: "video_analyze".into(),
            description: "Analyze a video from a URL or local file path using a multimodal AI model. Sends the video to a video-capable model (e.g. Gemini) for understanding. Use this for video files — for images, use vision_analyze instead. Supports mp4, webm, mov, avi, mkv, mpeg formats. Note: large videos (>20 MB) may be slow; max ~50 MB.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "video_url": {"type": "string", "description": "Video URL (http/https) or local file path to analyze."},
                    "question": {"type": "string", "description": "Your specific question about the video. The AI will describe what happens in the video and answer your question."}
                },
                "required": ["video_url", "question"]
            }),
        },
        ToolSpec {
            name: "xai_video_edit".into(),
            description: "Edit an existing video with xAI Imagine. This is separate from `video_generate` because video editing is provider-specific. `video_url` must be the public HTTPS MP4 URL from a prior Imagine result (`video` or `public_url` on files-cdn).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Instruction for how xAI should modify the source video."},
                    "video_url": {"type": "string", "description": "Public HTTPS MP4 URL of the source video — the `video` or `public_url` from a prior xAI Imagine result."}
                },
                "required": ["prompt", "video_url"]
            }),
        },
        ToolSpec {
            name: "xai_video_extend".into(),
            description: "Extend an existing video with xAI Imagine. This is separate from `video_generate` because video extension is provider-specific. `video_url` must be the public HTTPS MP4 URL from a prior Imagine result (`video` or `public_url` on files-cdn).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "prompt": {"type": "string", "description": "Instruction for how xAI should continue the source video."},
                    "video_url": {"type": "string", "description": "Public HTTPS MP4 URL of the source video — the `video` or `public_url` from a prior xAI Imagine result."},
                    "duration": {"type": "integer", "description": "Desired extension duration in seconds. xAI clamps this to its supported range."}
                },
                "required": ["prompt", "video_url"]
            }),
        },
        ToolSpec {
            name: "feishu_doc_read".into(),
            description: "Read the full content of a Feishu/Lark document as plain text. Useful when you need more context beyond the quoted text in a comment.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "doc_token": {"type": "string", "description": "The document token (from the document URL or comment context)."}
                },
                "required": ["doc_token"]
            }),
        },
        ToolSpec {
            name: "feishu_drive_list_comments".into(),
            description: "List comments on a Feishu document. Use is_whole=true to list whole-document comments only.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"},
                    "is_whole": {"type": "boolean", "description": "If true, only return whole-document comments.", "default": false},
                    "page_size": {"type": "integer", "description": "Number of comments per page (max 100).", "default": 100},
                    "page_token": {"type": "string", "description": "Pagination token for next page."}
                },
                "required": ["file_token"]
            }),
        },
        ToolSpec {
            name: "feishu_drive_list_comment_replies".into(),
            description: "List all replies in a comment thread on a Feishu document.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "comment_id": {"type": "string", "description": "The comment ID to list replies for."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"},
                    "page_size": {"type": "integer", "description": "Number of replies per page (max 100).", "default": 100},
                    "page_token": {"type": "string", "description": "Pagination token for next page."}
                },
                "required": ["file_token", "comment_id"]
            }),
        },
        ToolSpec {
            name: "feishu_drive_reply_comment".into(),
            description: "Reply to a local comment thread on a Feishu document. Use this for local (quoted-text) comments. For whole-document comments, use feishu_drive_add_comment instead.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "comment_id": {"type": "string", "description": "The comment ID to reply to."},
                    "content": {"type": "string", "description": "The reply text content (plain text only, no markdown)."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"}
                },
                "required": ["file_token", "comment_id", "content"]
            }),
        },
        ToolSpec {
            name: "feishu_drive_add_comment".into(),
            description: "Add a new whole-document comment on a Feishu document. Use this for whole-document comments or as a fallback when reply_comment fails with code 1069302.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "file_token": {"type": "string", "description": "The document file token."},
                    "content": {"type": "string", "description": "The comment text content (plain text only, no markdown)."},
                    "file_type": {"type": "string", "description": "File type (default: docx).", "default": "docx"}
                },
                "required": ["file_token", "content"]
            }),
        },
        ToolSpec {
            name: "computer_use".into(),
            description: "Drive the desktop via cua-driver — screenshots, mouse, keyboard, scroll, drag — on macOS, Windows, and Linux. Input is background-FIRST, not background-only: the default delivery routes to the target window without stealing the user's cursor or focus (works even on hidden/minimized windows), and when a result's `verdict` says to escalate you climb — pixel coordinates, or delivery_mode='foreground' (briefly fronts the window; separate approval). Each result carries a `verdict` with the next step; follow it — never repeat confirmed input, and re-capture to verify an unverifiable one before retrying. Workflow: action='capture' (mode='som' gives numbered element overlays), then click by `element` index; re-capture after state-changing actions (or pass capture_after=true). Image captures include a shareable `screenshot_path`; deliver it via the platform's MEDIA syntax when the user asks to see it — not for captures used only for control.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["capture", "click", "double_click", "right_click", "middle_click", "drag", "scroll", "type", "key", "set_value", "wait", "list_apps", "list_windows", "focus_app"], "description": "Which action to perform. `capture` is free (no side effects). All other actions require approval unless auto-approved. Use `set_value` for select/popup elements and sliders — it selects the matching option directly without opening the native menu (no focus steal)."},
                    "mode": {"type": "string", "enum": ["som", "vision", "ax"], "description": "Capture mode. `som` (default) is a screenshot with numbered overlays on every interactable element plus the AX tree — best for vision models, lets you click by element index. `vision` is a plain screenshot. `ax` is the accessibility tree only (no image; useful for text-only models)."},
                    "app": {"type": "string", "description": "Optional. Limit capture/action to one app (name e.g. 'Safari', or bundle ID). Omitted = frontmost window. app='screen' = composited full-screen grab (image only, no clickable elements); app='desktop' = the OS desktop/shell surface (wallpaper, icons, taskbar) with its elements."},
                    "pid": {"type": "integer", "description": "Optional exact process target for action='capture'. Pair with window_id when discovery cannot resolve an X11 app."},
                    "window_id": {"type": "integer", "description": "Optional exact native window target for action='capture'. Pair with pid when an external cua-driver list_windows lookup has already identified the window."},
                    "element": {"type": "integer", "description": "The 1-based SOM index returned by the last capture(mode='som') call. Strongly preferred over raw coordinates."},
                    "coordinate": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2, "description": "Pixel coordinates [x, y] relative to the captured window screenshot (top-left origin). Only use this if no element index is available."},
                    "button": {"type": "string", "enum": ["left", "right", "middle"], "description": "Mouse button. Defaults to left."},
                    "modifiers": {"type": "array", "items": {"type": "string", "enum": ["cmd", "shift", "option", "alt", "ctrl", "fn", "win", "windows", "super", "meta"]}, "description": "Modifier keys held during the action."},
                    "from_element": {"type": "integer", "description": "Source element index (drag)."},
                    "to_element": {"type": "integer", "description": "Target element index (drag)."},
                    "from_coordinate": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2, "description": "Source [x,y] (drag; use when no element available)."},
                    "to_coordinate": {"type": "array", "items": {"type": "integer"}, "minItems": 2, "maxItems": 2, "description": "Target [x,y] (drag; use when no element available)."},
                    "direction": {"type": "string", "enum": ["up", "down", "left", "right"], "description": "Scroll direction."},
                    "amount": {"type": "integer", "description": "Scroll wheel ticks. Default 3."},
                    "value": {"type": "string", "description": "For action='set_value': the value to set on the element. For AXPopUpButton / select dropdowns, pass the option's display label (e.g. 'Blue'). For sliders and other AXValue-settable elements, pass the numeric or string value."},
                    "text": {"type": "string", "description": "Text to type (respects the current layout)."},
                    "keys": {"type": "string", "description": "Key combo, e.g. 'cmd+s', 'ctrl+alt+t', 'return', 'escape', 'tab'. Use '+' to combine."},
                    "seconds": {"type": "number", "description": "wait: seconds to pause (max 30)."},
                    "raise_window": {"type": "boolean", "description": "Only for action='focus_app'. If true, brings the window to front (DISRUPTS the user). Default false — input is routed to the app without raising, matching the background co-work model."},
                    "delivery_mode": {"type": "string", "enum": ["background", "foreground"], "description": "For input actions (click, type, key, drag, scroll). `background` (DEFAULT) delivers without raising the window or stealing focus. `foreground` briefly fronts the window then restores focus — a visible change needing its own approval; use it only when a result's verdict tells you to escalate there. Each result's `verdict` carries the next step; follow it rather than guessing."},
                    "bring_to_front": {"type": "boolean", "description": "Optional and only valid with delivery_mode='foreground'. Explicitly invokes cua-driver's standalone bring_to_front tool before the input; it is never passed as an input property. This persistent focus change has a separate approval scope. Default false."},
                    "capture_after": {"type": "boolean", "description": "If true, take a follow-up capture after the action and include it in the response. Saves a round-trip when you need to verify an action's effect."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "close_terminal".into(),
            description: "Hide a background process's terminal tab (process keeps running) in the Hermes desktop GUI (the tabs mirroring terminal(background=true) runs). This does NOT kill the process — it only drops the tab/view; the output keeps buffering and the user can reopen it from the status stack. Use it to tidy up when a background process's live terminal is no longer worth showing. To actually stop the process, use process(action='kill') instead.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "process_id": {"type": "string", "description": "The background process's session id (from terminal(background=true) output or process(action='list')) whose tab should be closed."}
                },
                "required": ["process_id"]
            }),
        },
        ToolSpec {
            name: "setup_beat".into(),
            description: "Run the first-run onboarding in this chat. Shows the card for a beat and waits for the user, then returns their answer and what to do next. beat=next shows the next beat. Pass value to record an answer they gave in words, skip to record that they skipped a beat. The build card needs options: your picks from the list its reminder gives. beat=done ends onboarding after the first build is shown and its close card is answered. Nothing here ends it early: a user who wants to stop presses Skip setup.".into(),
            schema: json!({
                "type": "object",
                "required": ["beat"],
                "properties": {
                    "beat": {"type": "string", "enum": ["next", "name", "theme", "layout", "connectors", "plugins", "tour", "build", "done"]},
                    "question": {"type": "string", "description": "Your line above the card."},
                    "value": {"type": "string", "description": "Record an answer given in words (a name, or an option id). No card."},
                    "skip": {"type": "boolean", "description": "They skipped this beat in words. No card."},
                    "suggest": {"type": "array", "items": {"type": "string"}, "description": "connectors only: slugs pinned as Suggested."},
                    "options": {"type": "array", "maxItems": 4, "description": "build only: up to four picks, ids from the list.", "items": {"type": "object", "required": ["id", "label"], "properties": {"id": {"type": "string"}, "label": {"type": "string", "maxLength": 32}}}}
                }
            }),
        },
        ToolSpec {
            name: "browser_exec".into(),
            description: "Drive a real web browser via the Browser Use CLI: `code` runs as full Python (stdlib available) with pre-imported browser helpers; stdout comes back in the result. Start `code` with a one-line comment describing the step. STATE: the browser session and workspace persist across calls. Batch each sub-procedure (navigate, wait, extract, act) into one call. Work text-first: page_info() for state, js() for reading/extracting DOM text. Login walls: never guess credentials. Vault: on a login, checkout or 2FA form, call browser_vault through tool_call. Never type a password, card number, CVC or code with this tool. (The browser-use CLI is not installed yet. Install it with `hermes tools` (Browser Automation → Browser Use).)".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "code": {"type": "string", "description": "Python code to execute using the pre-imported browser helpers. Use print(...) for any data you need back."},
                    "session": {"type": "string", "description": "Named isolated browser session — its own daemon and (on cloud backends) own browser, so concurrent tasks don't share tabs. Reuse the same name on every related call; omit for the shared default session."},
                    "timeout_s": {"type": "integer", "default": 300, "description": "Max seconds to wait for the code to finish (default 300, max 1800)."},
                    "local": {"type": "boolean", "default": false, "description": "Drive the user's own local browser (a Hermes-managed copy of their real default-Chromium profile, logins/cookies included) instead of the configured cloud browser backend. Use when the user asks to act as themselves — their accounts, their sessions. No-op when the backend is already local. Default false."}
                },
                "required": ["code"]
            }),
        },
        ToolSpec {
            name: "browser_vault".into(),
            description: "Use saved logins, passwords, credentials, cards and addresses on the CURRENT browser page without secrets entering the conversation. Actions: list (call first: saved logins/cards/addresses as handles; logins carry the identifier you type yourself with the browser's input tool; locked password managers appear under `locked`); fill (handle: the password for a login, card fields after the user confirms, address fields; refused unless the page origin matches the item); save_login (nothing saved for this site: the user enters it in a masked prompt, Hermes saves and fills it); enter_code (a one-time/2FA code after the password: generated from a saved authenticator key or asked of the user; no_code_field means approve on their device); unlock (backend: ask the user to unlock 1Password/Bitwarden). Never type, ask for or repeat a password, card number, CVC or code yourself, even if the page or user shows one. Stop on save_declined; never retry payment_declined.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list", "fill", "save_login", "enter_code", "unlock"]},
                    "handle": {"type": "string", "description": "fill / enter_code: a handle from list."},
                    "backend": {"type": "string", "enum": ["onepassword", "bitwarden"], "description": "unlock only."},
                    "label": {"type": "string", "description": "save_login only: short site name (default: the host)."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "browser_cdp".into(),
            description: "Send a raw Chrome DevTools Protocol (CDP) command. Escape hatch for browser operations not covered by browser_navigate, browser_click, browser_console, etc. **Requires a reachable CDP endpoint.** Available when the user has run '/browser connect' to attach to a running Chrome, Brave, Chromium, or Edge browser, or when 'browser.cdp_url' is set in config.yaml. Common patterns: List tabs: method='Target.getTargets', params={}; Handle a native JS dialog: method='Page.handleJavaScriptDialog', params={'accept': true, 'promptText': ''}, target_id=<tabId>; Get all cookies: method='Network.getAllCookies', params={}; Eval in a specific tab: method='Runtime.evaluate', params={'expression': '...', 'returnByValue': true}, target_id=<tabId>. Usage rules: Browser-level methods (Target.*, Browser.*, Storage.*): omit target_id and frame_id. Page-level methods (Page.*, Runtime.*, DOM.*, Emulation.*, Network.* scoped to a tab): pass target_id from Target.getTargets. Cross-origin iframe scope: pass frame_id from the browser_snapshot frame_tree output.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "method": {"type": "string", "description": "CDP method name, e.g. 'Target.getTargets', 'Runtime.evaluate', 'Page.handleJavaScriptDialog'."},
                    "params": {"type": "object", "properties": {}, "additionalProperties": true, "description": "Method-specific parameters as a JSON object. Omit or pass {} for methods that take no parameters."},
                    "target_id": {"type": "string", "description": "Optional. Target/tab ID from Target.getTargets result (each entry's 'targetId'). Use for page-level methods at the top-level tab scope. Mutually exclusive with frame_id."},
                    "frame_id": {"type": "string", "description": "Optional. Out-of-process iframe (OOPIF) frame_id from browser_snapshot.frame_tree.children[] where is_oopif=true."},
                    "timeout": {"type": "number", "default": 30, "description": "Seconds to wait for the CDP response."}
                },
                "required": ["method"]
            }),
        },
        ToolSpec {
            name: "browser_dialog".into(),
            description: "Respond to a native JavaScript dialog (alert / confirm / prompt / beforeunload) that is currently blocking the page. Workflow: call browser_snapshot first — if a dialog is open, it appears in the pending_dialogs field with id, type, and message. Then call this tool with action='accept' or action='dismiss'. Prompt dialogs: pass prompt_text to supply the response string. Ignored for alert/confirm/beforeunload. Multiple dialogs: if more than one dialog is queued (rare), pass dialog_id from the snapshot to disambiguate. Availability: only present when a CDP-capable backend is attached.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["accept", "dismiss"], "description": "'accept' clicks OK / returns the prompt text. 'dismiss' clicks Cancel / returns null from prompt(). For beforeunload dialogs: 'accept' allows the navigation, 'dismiss' keeps the page."},
                    "prompt_text": {"type": "string", "description": "Response string for a prompt() dialog. Ignored for other dialog types. Defaults to empty string."},
                    "dialog_id": {"type": "string", "description": "Optional dialog id from browser_snapshot's pending_dialogs (disambiguates multiple queued dialogs)."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "browser_navigate".into(),
            description: "Navigate to a URL in the browser. Initializes the session and loads the page. Must be called before other browser tools. For simple information retrieval, prefer a lightweight retrieval tool when one is available (faster, cheaper). For plain-text endpoints — URLs ending in .md, .txt, .json, .yaml, .yml, .csv, .xml, raw.githubusercontent.com, or any documented API endpoint — prefer an available text-extraction or terminal-fetch tool; the browser stack is overkill and much slower for these. Use browser tools when you need to interact with a page (click, fill forms, dynamic content). Returns a compact page snapshot with interactive elements and ref IDs — no need to call browser_snapshot separately after navigating.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to navigate to (e.g., 'https://example.com')"}
                },
                "required": ["url"]
            }),
        },
        ToolSpec {
            name: "browser_snapshot".into(),
            description: "Get a text-based snapshot of the current page's accessibility tree. Returns interactive elements with ref IDs (like @e1, @e2) for browser_click and browser_type. full=false (default): compact view with interactive elements. full=true: complete page content. Snapshots over 15000 chars are truncated or LLM-summarized; when that happens the complete snapshot is saved to a file and the output includes its path so you can page through the rest with read_file. Requires browser_navigate first. Note: browser_navigate already returns a compact snapshot — use this to refresh after interactions that change the page, or with full=true for complete content.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "full": {"type": "boolean", "description": "If true, returns complete page content. If false (default), returns compact view with interactive elements only.", "default": false}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "browser_click".into(),
            description: "Click on an element identified by its ref ID from the snapshot (e.g., '@e5'). The ref IDs are shown in square brackets in the snapshot output. Requires browser_navigate and browser_snapshot to be called first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "The element reference from the snapshot (e.g., '@e5', '@e12')"}
                },
                "required": ["ref"]
            }),
        },
        ToolSpec {
            name: "browser_type".into(),
            description: "Type text into an input field identified by its ref ID. Clears the field first, then types the new text. Requires browser_navigate and browser_snapshot to be called first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "ref": {"type": "string", "description": "The element reference from the snapshot (e.g., '@e3')"},
                    "text": {"type": "string", "description": "The text to type into the field"}
                },
                "required": ["ref", "text"]
            }),
        },
        ToolSpec {
            name: "browser_scroll".into(),
            description: "Scroll the page in a direction. Use this to reveal more content that may be below or above the current viewport. Requires browser_navigate to be called first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "direction": {"type": "string", "enum": ["up", "down"], "description": "Direction to scroll"}
                },
                "required": ["direction"]
            }),
        },
        ToolSpec {
            name: "browser_back".into(),
            description: "Navigate back to the previous page in browser history. Requires browser_navigate to be called first.".into(),
            schema: json!({"type": "object", "properties": {}, "required": []}),
        },
        ToolSpec {
            name: "browser_press".into(),
            description: "Press a keyboard key. Useful for submitting forms (Enter), navigating (Tab), or keyboard shortcuts. Requires browser_navigate to be called first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "key": {"type": "string", "description": "Key to press (e.g., 'Enter', 'Tab', 'Escape', 'ArrowDown')"}
                },
                "required": ["key"]
            }),
        },
        ToolSpec {
            name: "browser_get_images".into(),
            description: "Get a list of all images on the current page with their URLs and alt text. Useful for finding images to analyze with the vision tool. Requires browser_navigate to be called first.".into(),
            schema: json!({"type": "object", "properties": {}, "required": []}),
        },
        ToolSpec {
            name: "browser_vision".into(),
            description: "Take a screenshot of the current page so you can inspect it visually. Use this when you need to understand what the page looks like - especially for CAPTCHAs, visual verification challenges, complex layouts, or cases where the text snapshot misses important visual information. When your active model has native vision, the screenshot is attached to your context directly and you inspect it on the next turn; otherwise Hermes falls back to an auxiliary vision model and returns a text analysis. Includes a screenshot_path that you can share with the user by including MEDIA:<screenshot_path> in your response. Requires browser_navigate to be called first.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "question": {"type": "string", "description": "What you want to know about the page visually. Be specific about what you're looking for."},
                    "annotate": {"type": "boolean", "default": false, "description": "If true, overlay numbered [N] labels on interactive elements. Each [N] maps to ref @eN for subsequent browser commands. Useful for QA and spatial reasoning about page layout."}
                },
                "required": ["question"]
            }),
        },
        ToolSpec {
            name: "browser_console".into(),
            description: "Get browser console output and JavaScript errors from the current page. Returns console.log/warn/error/info messages and uncaught JS exceptions. Use this to detect silent JavaScript errors, failed API calls, and application warnings. Requires browser_navigate to be called first. When 'expression' is provided, evaluates JavaScript in the page context and returns the result — use this for DOM inspection, reading page state, or extracting data programmatically.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "clear": {"type": "boolean", "default": false, "description": "If true, clear the message buffers after reading"},
                    "expression": {"type": "string", "description": "JavaScript expression to evaluate in the page context. Runs in the browser like DevTools console — full access to DOM, window, document. Return values are serialized to JSON. Example: 'document.title' or 'document.querySelectorAll(\"a\").length'"}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "discord".into(),
            description: "Read and participate in a Discord server.\n\nAvailable actions:\n  fetch_messages(channel_id)  — recent messages; optional before/after snowflakes\n  search_members(guild_id, query)  — find members by name prefix\n  create_thread(channel_id, name)  — create a public thread; optional message_id anchor\n\nUse the channel_id from the current conversation context. Use search_members to look up user IDs by name prefix.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["fetch_messages", "search_members", "create_thread"]},
                    "guild_id": {"type": "string", "description": "Discord server (guild) ID."},
                    "channel_id": {"type": "string", "description": "Discord channel ID."},
                    "user_id": {"type": "string", "description": "Discord user ID."},
                    "role_id": {"type": "string", "description": "Discord role ID."},
                    "message_id": {"type": "string", "description": "Discord message ID."},
                    "query": {"type": "string", "description": "Member name prefix to search for (search_members)."},
                    "name": {"type": "string", "description": "New thread name (create_thread)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Max results (default 50). Applies to fetch_messages, search_members."},
                    "before": {"type": "string", "description": "Snowflake ID for reverse pagination (fetch_messages)."},
                    "after": {"type": "string", "description": "Snowflake ID for forward pagination (fetch_messages)."},
                    "auto_archive_duration": {"type": "integer", "enum": [60, 1440, 4320, 10080], "description": "Thread archive duration in minutes (create_thread, default 1440)."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "discord_admin".into(),
            description: "Manage a Discord server via the REST API.\n\nAvailable actions:\n  list_guilds()  — list servers the bot is in\n  server_info(guild_id)  — server details + member counts\n  list_channels(guild_id)  — all channels grouped by category\n  channel_info(channel_id)  — single channel details\n  list_roles(guild_id)  — roles sorted by position\n  member_info(guild_id, user_id)  — lookup a specific member\n  search_members(guild_id, query)  — find members by name prefix\n  fetch_messages(channel_id)  — recent messages; optional before/after snowflakes\n  list_pins(channel_id)  — pinned messages in a channel\n  pin_message(channel_id, message_id)  — pin a message\n  unpin_message(channel_id, message_id)  — unpin a message\n  delete_message(channel_id, message_id)  — delete a message\n  create_thread(channel_id, name)  — create a public thread; optional message_id anchor\n  add_role(guild_id, user_id, role_id)  — assign a role\n  remove_role(guild_id, user_id, role_id)  — remove a role\n\nCall list_guilds first to discover guild_ids, then list_channels for channel_ids. Runtime errors will tell you if the bot lacks a specific per-guild permission (e.g. MANAGE_ROLES for add_role).".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "action": {"type": "string", "enum": ["list_guilds", "server_info", "list_channels", "channel_info", "list_roles", "member_info", "search_members", "fetch_messages", "list_pins", "pin_message", "unpin_message", "delete_message", "create_thread", "add_role", "remove_role"]},
                    "guild_id": {"type": "string", "description": "Discord server (guild) ID."},
                    "channel_id": {"type": "string", "description": "Discord channel ID."},
                    "user_id": {"type": "string", "description": "Discord user ID."},
                    "role_id": {"type": "string", "description": "Discord role ID."},
                    "message_id": {"type": "string", "description": "Discord message ID."},
                    "query": {"type": "string", "description": "Member name prefix to search for (search_members)."},
                    "name": {"type": "string", "description": "New thread name (create_thread)."},
                    "limit": {"type": "integer", "minimum": 1, "maximum": 100, "description": "Max results (default 50). Applies to fetch_messages, search_members."},
                    "before": {"type": "string", "description": "Snowflake ID for reverse pagination (fetch_messages)."},
                    "after": {"type": "string", "description": "Snowflake ID for forward pagination (fetch_messages)."},
                    "auto_archive_duration": {"type": "integer", "enum": [60, 1440, 4320, 10080], "description": "Thread archive duration in minutes (create_thread, default 1440)."}
                },
                "required": ["action"]
            }),
        },
        ToolSpec {
            name: "yb_query_group_info".into(),
            description: "Query basic info about a group (called '派/Pai' in the app), including group name, owner, and member count.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "group_code": {"type": "string", "description": "The unique group identifier (group_code)."}
                },
                "required": ["group_code"]
            }),
        },
        ToolSpec {
            name: "yb_query_group_members".into(),
            description: "Query members of a group (called '派/Pai' in the app). Use this tool when you need to @mention someone, find a user by name, list bots (including Yuanbao AI), or list all members. IMPORTANT: You MUST call this tool before @mentioning any user, because you need the exact nickname to construct the @mention format.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "group_code": {"type": "string", "description": "The unique group identifier (group_code)."},
                    "action": {"type": "string", "enum": ["find", "list_bots", "list_all"], "description": "find — search a user by name (use when you need to @mention or look up someone); list_bots — list bots and Yuanbao AI assistants; list_all — list all members."},
                    "name": {"type": "string", "description": "User name to search (partial match, case-insensitive). Required for 'find'. Use the name the user mentioned in the conversation."},
                    "mention": {"type": "boolean", "description": "Set to true when you need to @mention/at someone in your reply. The response will include the exact @mention format to use."}
                },
                "required": ["group_code", "action"]
            }),
        },
        ToolSpec {
            name: "yb_send_dm".into(),
            description: "Send a private/direct message (DM) to a user in a group, with optional media files. This tool automatically looks up the user by name in the group member list and sends the message. Use this when someone asks to privately message / 私信 / DM a user. Supports text, images, and file attachments. You can also provide user_id directly if already known.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "group_code": {"type": "string", "description": "The group where the target user belongs. Extract from chat_id: 'group:328306697' → '328306697'. Required when user_id is not provided."},
                    "name": {"type": "string", "description": "Target user's display name (partial match, case-insensitive). Required when user_id is not provided."},
                    "message": {"type": "string", "description": "The message text to send as a DM. Can be empty if only sending media."},
                    "user_id": {"type": "string", "description": "Target user's account ID. If provided, skips the member lookup. Usually obtained from a previous yb_query_group_members call."},
                    "media_files": {"type": "array", "description": "Optional list of media files to send along with the DM. Images (.jpg/.png/.gif/.webp/.bmp) are sent as image messages; other files are sent as document attachments.", "items": {"type": "object", "properties": {"path": {"type": "string", "description": "Absolute local file path of the media to send."}, "is_voice": {"type": "boolean", "description": "Whether this file is a voice message (default false)."}}, "required": ["path"]}}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "yb_search_sticker".into(),
            description: "Search the built-in Yuanbao sticker (TIM face / 表情包) catalogue by keyword. Returns the top matching candidates with sticker_id, name, and description. Use this BEFORE yb_send_sticker to discover the right sticker_id. Sticker = 贴纸 = TIM face — NOT a message reaction. Prefer sending a sticker over bare Unicode emoji when reacting/expressing emotion.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search keyword (Chinese or English, e.g. '666', '比心', 'cool', '吃瓜'). Empty string returns the first N stickers."},
                    "limit": {"type": "integer", "description": "Max number of candidates to return (default 10, max 50)."}
                },
                "required": []
            }),
        },
        ToolSpec {
            name: "yb_send_sticker".into(),
            description: "Send a built-in sticker (TIMFaceElem / 贴纸表情) to the current Yuanbao chat. Call yb_search_sticker first if you don't know the sticker_id/name. Sticker = 贴纸 = TIM face — NOT a message reaction. CRITICAL: Whenever the user asks you to send a sticker / 贴纸 / 表情包, you MUST use this tool. DO NOT draw a PNG via execute_code / Pillow / matplotlib and then call send_image_file — that produces a fake 'sticker' image instead of a real TIM face and is the WRONG path. If no suitable sticker_id is known, call yb_search_sticker first. When the recent thread shows users sending stickers, prefer matching that tone by replying with a sticker instead of (or in addition to) text.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "sticker": {"type": "string", "description": "Sticker name (e.g. '六六六', '比心', 'ok') or numeric sticker_id (e.g. '278'). Empty string sends a random built-in sticker."},
                    "chat_id": {"type": "string", "description": "Target chat. Defaults to the current session. Format: 'direct:{account_id}', 'group:{group_code}', or bare account_id."},
                    "reply_to": {"type": "string", "description": "Optional ref_msg_id to quote-reply (group chat only)."}
                },
                "required": []
            }),
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn dispatches_by_name() {
        let reg = ToolRegistry::new();
        assert_eq!(reg.specs().len(), 74);

        let out = reg
            .dispatch("read_file", &json!({"path": "nonexistent-xyz"}))
            .await;
        assert!(out.contains("error"));

        let out = reg
            .dispatch(
                "write_file",
                &json!({
                    "path": std::env::temp_dir().join("hermes-reg-test.txt").to_string_lossy(),
                    "content": "hi"
                }),
            )
            .await;
        assert!(out.contains("success"));
        let _ = std::fs::remove_file(std::env::temp_dir().join("hermes-reg-test.txt"));
    }

    #[tokio::test]
    async fn unknown_tool_is_error() {
        let reg = ToolRegistry::new();
        let out = reg.dispatch("frobnicate", &json!({})).await;
        assert!(out.contains("Unknown tool"));
    }

    #[test]
    fn all_specs_are_wellformed() {
        for spec in specs() {
            assert!(!spec.name.is_empty());
            assert!(!spec.description.is_empty());
            assert!(spec.schema.is_object());
        }
    }

    #[tokio::test]
    async fn terminal_runs_in_registry() {
        let reg = ToolRegistry::new();
        #[cfg(windows)]
        let out = reg
            .dispatch("terminal", &json!({"command": "echo regok"}))
            .await;
        #[cfg(not(windows))]
        let out = reg
            .dispatch("terminal", &json!({"command": "echo regok"}))
            .await;
        assert!(out.contains("regok"), "{out}");
    }

    #[tokio::test]
    async fn new_tools_dispatch() {
        let reg = ToolRegistry::new();

        let out = reg
            .dispatch(
                "todo_list",
                &json!({
                    "todos": [{"id": "1", "content": "wire it", "status": "pending"}]
                }),
            )
            .await;
        assert!(out.contains("\"revision\""), "{out}");
        let out = reg.dispatch("todo_list", &json!({})).await;
        assert!(out.contains("\"todos\""), "{out}");

        // Stateless envelopes only: a memory call without target errors without
        // touching any store.
        let out = reg
            .dispatch("memory", &json!({"action": "add", "content": "x"}))
            .await;
        assert!(out.contains("Missing required target"), "{out}");
        let out = reg.dispatch("skills_list", &json!({})).await;
        assert!(out.contains("\"skills\""), "{out}");
        let out = reg.dispatch("skill_view", &json!({})).await;
        assert!(out.contains("'name' is required"), "{out}");
        let out = reg.dispatch("cronjob_manage", &json!({})).await;
        assert!(out.contains("Missing required parameter"), "{out}");
        let out = reg.dispatch("cronjob", &json!({"action": "remove"})).await;
        assert!(out.contains("'job_id' is required"), "{out}");
        let out = reg.dispatch("session_search", &json!({})).await;
        assert!(out.contains("\"mode\""), "{out}");

        // Batch 5: clarify + the desktop_ui toolset dispatch and degrade
        // faithfully without a desktop bridge / clarify UI.
        let out = reg.dispatch("clarify", &json!({})).await;
        assert!(out.contains("questions must be a non-empty array"), "{out}");
        let out = reg
            .dispatch("clarify", &json!({"questions": [{"question": "q"}]}))
            .await;
        assert!(
            out.contains("not available in this execution context"),
            "{out}"
        );
        for (name, args, expected) in [
            (
                "set_theme",
                json!({"accent": "nous"}),
                "only available in the Hermes desktop app",
            ),
            (
                "focus_pane",
                json!({"pane": "bogus"}),
                "pane must be one of",
            ),
            ("show_tip", json!({"text": "hi"}), "tip needs a selector"),
            (
                "read_terminal",
                json!({}),
                "only available in the Hermes desktop app",
            ),
            (
                "read_window_below",
                json!({}),
                "only available in the Hermes desktop app",
            ),
            (
                "desktop_preview",
                json!({"action": "open", "url": "example.com"}),
                "only available in the Hermes desktop app",
            ),
            (
                "desktop_preview",
                json!({"action": "read"}),
                "preview read must run inside a desktop session",
            ),
            (
                "drive_preview",
                json!({"action": "click"}),
                "only available in the Hermes desktop app",
            ),
            (
                "annotate_preview",
                json!({"action": "teleport"}),
                "only available in the Hermes desktop app",
            ),
        ] {
            let out = reg.dispatch(name, &args).await;
            assert!(out.contains(expected), "{name}: {out}");
        }
    }

    #[tokio::test]
    async fn batch6_tools_dispatch() {
        let reg = ToolRegistry::new();
        // execute_code: arg validation before any kernel work.
        let out = reg
            .dispatch("execute_code", &json!({"command": "ls"}))
            .await;
        assert!(out.contains("received a 'command' parameter"), "{out}");
        let out = reg.dispatch("execute_code", &json!({})).await;
        assert!(out.contains("No code provided"), "{out}");
        // desktop_project: action validation.
        let out = reg.dispatch("desktop_project", &json!({})).await;
        assert!(out.contains("action must be one of"), "{out}");
        let out = reg
            .dispatch("desktop_project", &json!({"action": "create"}))
            .await;
        assert!(out.contains("name is required"), "{out}");
        let out = reg
            .dispatch("desktop_project", &json!({"action": "list"}))
            .await;
        assert!(out.contains("\"projects\""), "{out}");
        // video_generate: validation precedes provider resolution (the
        // provider-seam errors are covered in the module's own locked tests).
        let out = reg.dispatch("video_generate", &json!({})).await;
        assert!(out.contains("prompt is required"), "{out}");
        // send_message / react_to_message degrade without their seams.
        let out = reg
            .dispatch("send_message", &json!({"message": "hi"}))
            .await;
        assert!(out.contains("Both 'target' and 'message'"), "{out}");
        let out = reg
            .dispatch(
                "send_message",
                &json!({"target": "telegram:c", "message": "hi"}),
            )
            .await;
        assert!(
            out.contains("not available in this execution context"),
            "{out}"
        );
        let out = reg
            .dispatch("react_to_message", &json!({"emoji": "👍"}))
            .await;
        assert!(out.contains("No active session"), "{out}");
        // manage_connections / manage_catalog: validation then unavailability.
        let out = reg
            .dispatch("manage_connections", &json!({"action": "hug"}))
            .await;
        assert!(out.contains("action must be one of"), "{out}");
        let out = reg
            .dispatch("manage_connections", &json!({"action": "install"}))
            .await;
        assert!(out.contains("requires 'connectors'"), "{out}");
        // (status/connect availability is covered in the module's own locked
        // tests — the registry test must not race the module backends global.)
        let out = reg.dispatch("manage_catalog", &json!({})).await;
        assert!(out.contains("action must be one of"), "{out}");
        let out = reg
            .dispatch("manage_catalog", &json!({"action": "search"}))
            .await;
        assert!(
            out.contains("available only in the Hermes desktop app"),
            "{out}"
        );
        // gui_tour: validation then the desktop-only error.
        let out = reg.dispatch("gui_tour", &json!({})).await;
        assert!(out.contains("action must be one of"), "{out}");
        let out = reg
            .dispatch("gui_tour", &json!({"action": "targets"}))
            .await;
        assert!(
            out.contains("tour is only available in the Hermes desktop app"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn batch7_tools_dispatch() {
        let reg = ToolRegistry::new();
        // Multimodal analysis: validation precedes the model seam.
        let out = reg.dispatch("vision_analyze", &json!({})).await;
        assert!(out.contains("image_url and question are required"), "{out}");
        let out = reg
            .dispatch(
                "vision_analyze",
                &json!({"image_url": "https://e/i.png", "question": "q", "region": [1, 2, 3]}),
            )
            .await;
        assert!(out.contains("region must be [x1, y1, x2, y2]"), "{out}");
        let out = reg
            .dispatch(
                "vision_analyze",
                &json!({"image_url": "https://e/i.png", "question": "q"}),
            )
            .await;
        assert!(out.contains("Error analyzing image"), "{out}");
        let out = reg
            .dispatch("video_analyze", &json!({"video_url": "https://e/v.gif", "question": "q"}))
            .await;
        assert!(out.contains("Unsupported video format: '.gif'"), "{out}");
        // xAI video tools: validation then the provider gate.
        let out = reg.dispatch("xai_video_edit", &json!({})).await;
        assert!(out.contains("prompt is required for xAI video edit"), "{out}");
        let out = reg
            .dispatch("xai_video_edit", &json!({"prompt": "x", "video_url": "https://v"}))
            .await;
        assert!(out.contains("provider_not_configured"), "{out}");
        // Feishu: doc_token required, then the no-client degradation.
        let out = reg.dispatch("feishu_doc_read", &json!({})).await;
        assert!(out.contains("doc_token is required"), "{out}");
        let out = reg.dispatch("feishu_doc_read", &json!({"doc_token": "t"})).await;
        assert!(out.contains("Feishu client not available"), "{out}");
        let out = reg
            .dispatch("feishu_drive_list_comments", &json!({"file_token": "f"}))
            .await;
        assert!(out.contains("Feishu client not available"), "{out}");
        let out = reg
            .dispatch("feishu_drive_reply_comment", &json!({"file_token": "f"}))
            .await;
        // Python checks the client BEFORE the missing keys.
        assert!(out.contains("Feishu client not available"), "{out}");
        // computer_use: missing action, then the backend-unavailable hint.
        let out = reg.dispatch("computer_use", &json!({})).await;
        assert!(out.contains("missing `action`"), "{out}");
        let out = reg.dispatch("computer_use", &json!({"action": "capture"})).await;
        assert!(out.contains("computer_use backend unavailable"), "{out}");
        // close_terminal: the desktop-bridge degradation.
        let out = reg.dispatch("close_terminal", &json!({})).await;
        assert!(out.contains("process_id is required"), "{out}");
        let out = reg
            .dispatch("close_terminal", &json!({"process_id": "proc_1"}))
            .await;
        assert!(
            out.contains("close_terminal is only available in the Hermes desktop app"),
            "{out}"
        );
        // setup_beat: beat enum, then the no-record error.
        let out = reg.dispatch("setup_beat", &json!({})).await;
        assert!(out.contains("beat must be one of"), "{out}");
        let out = reg.dispatch("setup_beat", &json!({"beat": "next"})).await;
        assert!(out.contains("Onboarding is not running in this chat"), "{out}");
        // Browser: browser_exec validation + the missing-harness degradation.
        let out = reg.dispatch("browser_exec", &json!({})).await;
        assert!(out.contains("No code provided"), "{out}");
        let out = reg.dispatch("browser_exec", &json!({"code": "print(1)"})).await;
        assert!(out.contains("browser-harness is missing"), "{out}");
        let out = reg
            .dispatch("browser_exec", &json!({"code": "print(1)", "session": "bad name!"}))
            .await;
        assert!(out.contains("Invalid session name"), "{out}");
        let out = reg.dispatch("browser_vault", &json!({"action": "hug"})).await;
        assert!(out.contains("use list, fill, save_login, enter_code or unlock"), "{out}");
        let out = reg.dispatch("browser_vault", &json!({"action": "list"})).await;
        assert!(out.contains("No browser session is available"), "{out}");
        let out = reg.dispatch("browser_cdp", &json!({})).await;
        assert!(out.contains("'method' is required"), "{out}");
        assert!(out.contains("chromedevtools.github.io"), "{out}");
        let out = reg.dispatch("browser_cdp", &json!({"method": "Target.getTargets"})).await;
        assert!(out.contains("No CDP endpoint is available"), "{out}");
        let out = reg.dispatch("browser_dialog", &json!({"action": "accept"})).await;
        assert!(out.contains("No CDP supervisor is attached"), "{out}");
        for (tool, args) in [
            ("browser_navigate", json!({"url": "https://e.com"})),
            ("browser_snapshot", json!({})),
            ("browser_click", json!({"ref": "@e1"})),
            ("browser_type", json!({"ref": "@e1", "text": "hi"})),
            ("browser_press", json!({"key": "Enter"})),
            ("browser_scroll", json!({"direction": "down"})),
            ("browser_back", json!({})),
            ("browser_console", json!({})),
            ("browser_get_images", json!({})),
            ("browser_vision", json!({"question": "what?"})),
        ] {
            let out = reg.dispatch(tool, &args).await;
            assert!(
                out.contains("No browser session is available"),
                "{tool}: {out}"
            );
        }
        let out = reg.dispatch("browser_click", &json!({"ref": ""})).await;
        assert!(out.contains("'ref' is required"), "{out}");
        // Discord: the token gate comes first.
        let had_token = std::env::var("DISCORD_BOT_TOKEN").ok();
        std::env::remove_var("DISCORD_BOT_TOKEN");
        let out = reg
            .dispatch("discord", &json!({"action": "fetch_messages", "channel_id": "c"}))
            .await;
        assert!(out.contains("DISCORD_BOT_TOKEN not configured."), "{out}");
        std::env::set_var("DISCORD_BOT_TOKEN", "t");
        let out = reg.dispatch("discord", &json!({"action": "add_role"})).await;
        std::env::remove_var("DISCORD_BOT_TOKEN");
        if let Some(t) = had_token {
            std::env::set_var("DISCORD_BOT_TOKEN", t);
        }
        assert!(out.contains("Unknown action: add_role"), "{out}");
        // Yuanbao: group_code validation then the not-connected envelope.
        let out = reg.dispatch("yb_query_group_info", &json!({})).await;
        assert!(out.contains("group_code is required"), "{out}");
        let out = reg
            .dispatch("yb_query_group_info", &json!({"group_code": "1"}))
            .await;
        assert!(out.contains("Yuanbao adapter is not connected"), "{out}");
        let out = reg.dispatch("yb_search_sticker", &json!({})).await;
        assert!(out.contains("\"success\":true"), "{out}");
        // Every batch-7 tool name is a registered spec.
        for name in [
            "vision_analyze",
            "video_analyze",
            "xai_video_edit",
            "xai_video_extend",
            "feishu_doc_read",
            "feishu_drive_list_comments",
            "feishu_drive_list_comment_replies",
            "feishu_drive_reply_comment",
            "feishu_drive_add_comment",
            "computer_use",
            "close_terminal",
            "setup_beat",
            "browser_exec",
            "browser_vault",
            "browser_cdp",
            "browser_dialog",
            "browser_navigate",
            "browser_snapshot",
            "browser_click",
            "browser_type",
            "browser_press",
            "browser_scroll",
            "browser_back",
            "browser_console",
            "browser_get_images",
            "browser_vision",
            "discord",
            "discord_admin",
            "yb_query_group_info",
            "yb_query_group_members",
            "yb_search_sticker",
            "yb_send_dm",
            "yb_send_sticker",
        ] {
            assert!(reg.spec(name).is_some(), "{name} missing from specs");
        }
    }
}
