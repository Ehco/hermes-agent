# Optional skills (ported content)

Verbatim copy of the Python hermes-agent repo's `optional-skills/` tree
(152 SKILL.md packages under category directories, e.g.
`creative/ascii-art/SKILL.md`), synced through upstream daec3cd4c6.
Read-only content: the Rust discovery layer serves it; nothing here is
machine-edited.

## Wiring

`hermes_skills::skills::optional_skills_root()` resolves the catalog:

1. `HERMES_OPTIONAL_SKILLS` env var when set and non-empty (packaged
   builds — the compile-time path below does not travel with the binary);
2. this `optional/` directory, via the crate's `CARGO_MANIFEST_DIR`
   (works for `cargo test` and in-repo runs).

`discover_skills_with_optional(active_root)` merges the catalog behind
the active skills root (`$HERMES_SKILLS_DIR` → `$HERMES_HOME/skills` →
`~/.hermes/skills`, per `crates/tools/src/skills.rs`), with an active
copy of the same skill name shadowing the optional one — the same
tiering the Python side uses for hub-catalog vs installed skills.
`skill_view` falls back to the optional root when a name is not found
in the active root, so linked files resolve against the real skill dir.
