//! Tool registry — `Vec<ToolSpec>` plus a dispatcher by name.
//!
//! Port of `tools/registry.py`'s dispatch contract from the Python
//! hermes-agent: every handler returns a JSON string, unknown tools and
//! failures surface as `{"error": ...}` instead of panicking. The `ToolHost`
//! in the cli crate delegates here.

use serde_json::Value;
use serde_json::json;

use hermes_core::model::ToolSpec;

use crate::{edit, list_dir, read_file, search, terminal, util, write_file};

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

/// Synchronous dispatch used directly by sync callers and tests.
pub fn execute_sync(name: &str, args: &Value) -> String {
    match name {
        "read_file" => read_file::execute(args),
        "write_file" => write_file::execute(args),
        "edit" | "patch" => edit::execute(args),
        "list_dir" => list_dir::execute(args),
        "search" | "search_files" => search::execute(args),
        "terminal" => terminal::execute(args),
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
            description: "Execute a shell command. Returns stdout, stderr and exit_code. Use read_file/search instead of cat/grep. Async subprocess with a timeout (default 60s, max 600s); an expired command is killed and `timed_out: true` is returned.".into(),
            schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string", "description": "The shell command to execute"},
                    "cwd": {"type": "string", "description": "Working directory for the command (default: the process cwd)"},
                    "timeout": {"type": "integer", "description": "Max seconds (default 60, max 600). Above 600s the command is killed.", "default": 60, "maximum": 600}
                },
                "required": ["command"]
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
        assert_eq!(reg.specs().len(), 6);

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
}
