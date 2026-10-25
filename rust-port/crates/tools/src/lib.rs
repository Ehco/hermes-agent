//! hermes-tools: the tool registry and built-in tools
//! (port of `tools/` from the Python hermes-agent).
//!
//! Every tool is a module with an `execute(args: &Value) -> String` entry
//! point that returns a JSON string — errors included (`{"error": ...}`),
//! mirroring the Python `registry.dispatch` contract so the agent loop and
//! the cli `ToolHost` can treat results opaquely.

pub mod edit;
pub mod html_text;
pub mod image_generate;
pub mod list_dir;
pub mod read_file;
pub mod registry;
pub mod search;
pub mod terminal;
pub mod util;
pub mod web_common;
pub mod web_extract;
pub mod web_search;
pub mod write_file;

/// Re-exported for convenience; the registry owns the ToolSpec list.
pub use hermes_core::model::ToolSpec;
pub use registry::{execute_sync, specs, ToolRegistry};
