//! hermes-tools: the tool registry and built-in tools
//! (port of `tools/` from the Python hermes-agent).
//!
//! Every tool is a module with an `execute(args: &Value) -> String` entry
//! point that returns a JSON string — errors included (`{"error": ...}`),
//! mirroring the Python `registry.dispatch` contract so the agent loop and
//! the cli `ToolHost` can treat results opaquely.

pub mod annotate_preview;
pub mod browser;
pub mod clarify;
pub mod close_terminal;
pub mod computer_use;
pub mod cronjob;
pub mod delegate_task;
pub mod desktop_bridge;
pub mod desktop_preview;
pub mod desktop_project;
pub mod discord;
pub mod drive_preview;
pub mod edit;
pub mod execute_code;
pub mod feishu;
pub mod focus_pane;
pub mod gui_tour;
// Kanban subsystem (tools/kanban_tools.py + hermes_cli/kanban_db.py port):
// 15 kanban_* tools with the mode gating and the JSON board-state layer.
pub mod kanban;
pub mod kanban_state;
pub mod html_text;
pub mod image_generate;
pub mod list_dir;
pub mod manage_catalog;
pub mod manage_connections;
pub mod maps;
pub mod memory;
pub mod osv_check;
pub mod process_manage;
pub mod react_to_message;
pub mod read_file;
pub mod read_terminal;
pub mod read_window_below;
pub mod registry;
pub mod search;
pub mod send_message;
pub mod session_search;
pub mod set_theme;
pub mod setup_beat;
pub mod show_tip;
pub mod skills;
// Voice: cloud-STT provider HTTP contracts (transcription_cloud.py slice) and
// the RetainDB dialectic-modeling core + its ten retaindb_* agent tools — all
// over injected HTTP seams, so no live network in tests. The ten tools ARE
// registry entries (99 total); visibility pins to the Python exposure gate
// (memory toolset + provider availability) via retaindb::tools::
// retaindb_tools_visible.
pub mod retaindb;
pub mod transcription;
pub mod terminal;
pub mod todo_list;
pub mod tool_search;
pub mod tts;
pub mod util;
pub mod video_generate;
pub mod vision;
pub mod web_common;
pub mod web_extract;
pub mod web_search;
pub mod write_file;
pub mod x_search;
pub mod xai_video;
pub mod yuanbao;

/// Re-exported for convenience; the registry owns the ToolSpec list.
pub use hermes_core::model::ToolSpec;
pub use registry::{execute_sync, specs, ToolRegistry};
