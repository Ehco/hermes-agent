//! Tool registry — `Vec<ToolSpec>` plus a dispatcher by name.
//!
//! Port of `tools/registry.py`'s dispatch contract from the Python
//! hermes-agent: every handler returns a JSON string, unknown tools and
//! failures surface as `{"error": ...}` instead of panicking. The `ToolHost`
//! in the cli crate delegates here.

use serde_json::Value;
use serde_json::json;

use hermes_core::model::ToolSpec;

use crate::{cronjob, delegate_task, edit, image_generate, list_dir, maps, memory, process_manage, read_file, search,
            session_search, skills, terminal, todo_list, tool_search, tts, util, web_extract, web_search, write_file, x_search};

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
        // Progressive tool disclosure bridges (Python tools/tool_search.py):
        // search the deferred-tool catalog, load schemas, invoke deferred tools.
        "tool_search" => tool_search::execute_search(args),
        "tool_describe" => tool_search::execute_describe(args),
        "tool_call" => tool_search::execute_call(args),
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
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn dispatches_by_name() {
        let reg = ToolRegistry::new();
        assert_eq!(reg.specs().len(), 24);

        let out = reg.dispatch("read_file", &json!({"path": "nonexistent-xyz"})).await;
        assert!(out.contains("error"));

        let out = reg.dispatch("write_file", &json!({
            "path": std::env::temp_dir().join("hermes-reg-test.txt").to_string_lossy(),
            "content": "hi"
        })).await;
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
        let out = reg.dispatch("terminal", &json!({"command": "echo regok"})).await;
        #[cfg(not(windows))]
        let out = reg.dispatch("terminal", &json!({"command": "echo regok"})).await;
        assert!(out.contains("regok"), "{out}");
    }

    #[tokio::test]
    async fn new_tools_dispatch() {
        let reg = ToolRegistry::new();

        let out = reg.dispatch("todo_list", &json!({
            "todos": [{"id": "1", "content": "wire it", "status": "pending"}]
        })).await;
        assert!(out.contains("\"revision\""), "{out}");
        let out = reg.dispatch("todo_list", &json!({})).await;
        assert!(out.contains("\"todos\""), "{out}");

        // Stateless envelopes only: a memory call without target errors without
        // touching any store.
        let out = reg.dispatch("memory", &json!({"action": "add", "content": "x"})).await;
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
    }
}
