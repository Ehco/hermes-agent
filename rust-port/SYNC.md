# Upstream sync — NousResearch/hermes-agent main (2026-10-26)

## Scope of review
Fresh blobless clone of `https://github.com/NousResearch/hermes-agent.git` (upstream,
not the Ehco fork) at `AppData/Local/hermes/scratch/upstream-clone/` (HEAD `daec3cd4c6`).
Reviewed `git log` for commits dated **2026-10-24 → 2026-10-26** (the review window;
~230 commits — the window was dense: copilot-seams integration, webapp-on-gateway,
prepared-compaction, vault keychain, toolset/terminal fixes), with per-commit diffs on
the ported surface: `tools/`, `agent/`, `tui_gateway/`, `gateway/platforms/`,
`hermes_cli/`, `plugins/`.

## Ported (commit-by-commit mapping)

| Upstream commit | Rust change |
|---|---|
| `079b38cd07` — tool_search catalog keeps small servers' descriptions next to a huge one | `crates/tools/src/tool_search.rs`: step 3 of `build_catalog_listing_with_form` now retries the uncollapsed servers in **full** form before names-only (deterministic `summarized` set); one-tool groups drop their `<label> tools (1):` heading and render as bare `- name` lines placed first, straight under the catalog header; `_CONNECTIONS_HINT`/`CONNECTIONS_HINT` reworded to the hosted-connector sentence (no example apps; results named `connectors__<app>__<tool>`; `manage_connections` "checks or connects"). |
| `faef4e0ab8` — a one-item remove batch can delete the last saved fact | `crates/tools/src/memory.rs`: added the batch `would_empty` guard (refuse a batch that empties a previously non-empty MEMORY.md/USER.md, all-or-nothing abort, Python refusal text naming the one-item batch) with the upstream exemption: `operations` of exactly one `remove` is the deliberate single delete. Module doc updated; regression test `batch_would_empty_guard_and_one_item_remove_exemption`. |
| `df92e33081` + `3a3bf5648e`/`b66ab2bb7b` — enforce the 60-char description limit on every write to the root SKILL.md | `crates/tools/src/skills.rs`: new `SKILL_PROMPT_DESC_LIMIT` (60), `description_of`, `description_budget_error` (grandfathering: an unchanged legacy over-limit description stays editable) and `check_root_skill_description`; wired into `create`, full-rewrite `patch`/`edit`, targeted `patch` and `write_file` whenever the target is the skill's root `SKILL.md`. Test `root_skill_md_writes_enforce_the_60_char_description_budget`. (The word-boundary clip half — `clip_skill_description` — lives in the Python agent prompt-builder; the Rust skills_list rows keep the 1024-char tool contract, matching upstream `skills_tool.py`.) |

## Reviewed, no Rust delta needed (behavior already matches, or the subsystem isn't on the ported surface)

- `b044f7cf28` + `07af2357ec` (one-item remove/duplicate-add not refused on an over-cap store; one-item add over budget returns live entries) — the Rust batch applies ops through `MemoryStore` per op; a one-item remove/duplicate-add never grows the store, and the single-add overflow already returns `current_entries` + usage like the new `_add_overflow`.
- `5ae17fccb8` / `bb216b95bd` (browser_exec header names `vision_analyze` conditionally; screenshots note) — the Rust `browser_exec` always degrades with the missing-harness error and never emits the streaming header; the tool description lives in the registry, not the runtime header.
- `9c1a75e56f` + `2712a096db` (Discord config allow_from governs guild channels; button clicks in threads get the thread verdict) — the Rust `crates/gateways/src/discord` covers message formatting/REST delivery only; no allowlist/component-auth logic is ported.
- `39661bb10b` (run tool discovery before resolving a legacy name-only pin) — `pinned_tool_defs`/`mcp_tool_agent` pin-restore machinery is not ported.
- `1a871b3c5b` / `b02fa68132` (disabled_toolsets naming) — toolset strip semantics beyond the registry are not ported.
- `01185213a3` (stream image tags prefix-stably; drop them from queued text) — the Rust gateways carry no stream consumer / `extract_images` / MEDIA: hold-back; image tags never stream.
- `f219708926` (+ `c975831fbc`, later folded) — ElevenLabs voice selection (`voice_id`, `find_voices`, `tts_voices` tool added then folded into `text_to_speech`): the Rust `crates/tools/src/tts.rs` implements only the OpenAI-compatible path; the ElevenLabs provider surface (voice search API, pagination, filters) is not ported, so there is no `tts_voices` tool to add.
- CLI batch (`eb54244142`, `5005522166`, `342a55ebad`, `dc8e416b34`, `1227a2d5bb`) — `show_reasoning`, `/discard` and the launch-directory behavior live in Python CLI commands the Rust `crates/cli` command registry (new/clear/stop/history/compress/resume/sessions/model/help/quit) does not carry.
- `1205a010f7` / `86a61729d3` / `8e5ea80dfb` / `bc8e9107b6` (planned-restart marker ownership) and `a5d3c25664` (`message.complete` on an active-session refusal) — gateway restart/update lifecycle is Python `hermes_cli/gateway.py` machinery outside `crates/gateway`'s desktop-API scope; the Rust `message.complete` emission on its turn path already covers the emitted event set.
- No net-new tools on the ported registry surface: the only `tools/` module additions in the window (`tts_voices_tool.py`, `browser_vault_context.py`, `browser_launcher*.py`) either self-folded away (`tts_voices`) or are helper modules for the browser-harness the Rust port degrades on. Registry contract otherwise unchanged in the window.

## Deferred (upstream subsystems the Rust port does not cover)

- **github-copilot labs plugin / left-core migration** (`1da2eb42dc` "remove GitHub Copilot from core", the whole `copilot-seams/*` series: per-home provider rows/aliases, static provider tables, migration notices, doctor/update wiring) — provider-plugin registry and Copilot are not ported.
- **prepared compaction / compression telemetry** (`32a636bc8d`, `c2383a6806`, `d2d1824fdf`, the shared-metrics v4 schema work) — compression in the Rust core is a stub; the opt-in `compression.prepare_ahead` regime, pass accounting and `hermes.shared_metrics` relay have no Rust counterpart yet.
- **webapp / Web Access** (the `webapp-on-gateway` merge `d6b868b7ab` and ~90 `fix(webapp*)` commits) — browser-hosted Desktop, Web Access control, browser credentials, SSH/docker workspace file serving: deferred wholesale (gateway web surface).
- **vault keychain** (`7abb9cc0fa` + follow-ups: OS-keychain vault key storage, verified migration, plugin login backends) — the Rust browser_vault tool shells the verbs but no keychain/plugin-backend machinery is ported.
- **config backend seam** (`871900df63` + the `config-backend` review series) — Rust reads config.yaml directly; the Python ConfigBackend seam (file backend only, reader gates) is an internal Python abstraction.
- **macOS launchd / telemetry exit flush / process-identity / desktop UI internals / Honcho / voice streaming** — per the standing port scope, out of the ported surface.
- Tests-only churn, docs, README, CI, optional-skills catalog pins, evals: ignored per task rules.

## Verification
- `cargo build` — workspace green (dev profile, windows-gnu toolchain with the
  `export PATH="/c/msys64/mingw64/bin:$HOME/.cargo/bin:$PATH"` prefix).
- `cargo test` — **671 passed, 0 failed, 3 ignored** across all crates; includes the
  new tool_search catalog, memory would-empty and skill description-budget tests.
- Files touched: `crates/tools/src/tool_search.rs`, `crates/tools/src/memory.rs`,
  `crates/tools/src/skills.rs`. No git repo in `hermes-rs` (files left on disk).
- The guarded live repo (`AppData/Local/hermes/hermes-agent`) was not touched; the
  token in `~/.git-credentials` was never printed (clone output redacted).
