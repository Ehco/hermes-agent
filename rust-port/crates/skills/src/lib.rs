//! hermes-skills: SKILL.md parsing, skill discovery, and the memory store
//! (port of the Python skills/memory machinery).
//!
//! - [`frontmatter`]: YAML frontmatter parsing + validation
//!   (`agent/skill_utils.py`, `tools/skills_tool_plugin.py`)
//! - [`skills`]: catalog discovery, name lookup, linked files
//!   (`tools/skills_tool.py`)
//! - [`memory`]: bounded MEMORY.md / USER.md store
//!   (`tools/memory_tool_store.py`)

pub mod frontmatter;
pub mod memory;
pub mod skills;

pub use memory::{MemoryError, MemoryStore, MutationOutcome, Target, ENTRY_DELIMITER};
pub use skills::{
    discover_skills, discover_skills_with_optional, linked_files, load_skill, locate_skill,
    locate_skill_with_optional, optional_skills_root, sort_skills, SkillMeta,
};
