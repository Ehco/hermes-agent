//! hermes-tools: the tool registry and built-in tools
//! (port of `tools/` from the Python hermes-agent).
//!
//! Every tool is a module with an `execute(args: &Value) -> String` entry
//! point that returns a JSON string — errors included (`{"error": ...}`),
//! mirroring the Python `registry.dispatch` contract so the agent loop and
//! the cli `ToolHost` can treat results opaquely.

pub mod cronjob;
pub mod delegate_task;
pub mod edit;
pub mod html_text;
pub mod image_generate;
pub mod list_dir;
pub mod maps;
pub mod memory;
pub mod osv_check;
pub mod process_manage;
pub mod read_file;
pub mod registry;
pub mod search;
pub mod session_search;
pub mod skills;
pub mod terminal;
pub mod todo_list;
pub mod tts;
pub mod util;
pub mod web_common;
pub mod web_extract;
pub mod web_search;
pub mod write_file;
pub mod x_search;

/// Re-exported for convenience; the registry owns the ToolSpec list.
pub use hermes_core::model::ToolSpec;
pub use registry::{execute_sync, specs, ToolRegistry};
