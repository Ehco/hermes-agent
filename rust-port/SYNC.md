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

---

## W24 — upstream re-sync (2026-10-26, second wave same day)

Fresh blobless clone at `AppData/Local/hermes/scratch/upstream-clone2/` (HEAD
`c714c42431`). `git log --oneline daec3cd4c6..HEAD` → **91 commits** (dense webapp/
desktop day upstream). **No Rust code changes were required this wave**: every
behavioral commit lands in webapp/desktop internals, test/docs/deps churn, or Python
machinery the Rust port does not carry. Per-file diff review of the ported surface
(`tools/`, `agent/`, `tui_gateway/`, `gateway/platforms/`, `hermes_cli/`,
`plugins/`) found exactly six source files touched, mapped below.

### Ported-surface files — reviewed, no Rust delta (machinery not ported)

| Upstream commit | Python change | Why no Rust delta |
|---|---|---|
| `3226ddd8a5` fix(windows): keep a review tree from taking over the home's hermes command | `hermes_cli/_launchers.py`: `expose_cli` on a non-home checkout leaves `<home>\bin` launchers that start another living checkout in place (new `_launcher_checkout`/`_launcher_of_another_checkout`/`_is_home_checkout`); missing/foreign-dead launchers still replaced, the home's own checkout still republishes. | The Rust port is a native binary; the checkout-launcher publishing machinery (`expose_cli`, `_publish_conveniences`, PATHEXT `.exe`/`.cmd` launchers) has no Rust counterpart. |
| `9ed6c20651` fix(dashboard-auth): give every password-provider session its own tokens | `plugins/dashboard_auth/basic`: `_mint_session` embeds a random `jti` (and `exp`-second reuse) in both signed tokens so same-second logins get distinct pairs; verification unchanged. | The Rust listener authenticates browsers with the stored session token itself (`crates/gateway/src/webapp/credential.rs`); BasicAuthProvider's signed access/refresh token minting is not ported. |

### Deferred (upstream subsystems; shas for the record)

- **Webapp profile multiplexing / session authorities** — served-profile-set cache,
  unserved dials, boot-on-served-profile, picker greying: `8bfff4e273`, `ad03c02014`,
  `b65111aad5`, `99b7565030`, `4f7988a541`, `5a57936c53`, `562d95142c`, `ce4bf05b74`,
  `26005015f8`, `f87ca06826`, `b5b28309f8`, `c714c42431`, `589f3daf97`; held-frame
  bound + drop-at-disconnect + starting-profile waits in `tui_gateway/ws.py` +
  `gateway/session_authorities.py`/`session_ancillary.py`/`session_controls.py`/
  `run_idle_exit.py`: `32c03ed88d`, `422f2f8456`, `83ad726672`, `cedc564279`,
  `f968aa7304`, `2127cddbbd`, `41715be2f8`. The Rust `crates/gateway` desktop API is
  single-profile (`"profile": "default"`) with an arrival-order dispatcher — no held
  lanes/authority connection to bound.
- **Webapp profile prefs/folders + journal/send + host terminal + download tickets**
  — `cd4f55a593`, `514b95b451`, `6f2ec72cf7`, `07aa617860`, `a3928dfca6`,
  `5d9ce70cd6`, `8295aae6ee`, `8e657b956c`, `845b0755ed`, `a357681674`;
  `9a290094bd`, `a0e0b72e3b`, `122d14b791`, `0733c281d6`, `371208bb7a`,
  `1c6871dc55`, `be7cb86be2`, `9ab5fd2ffb`, `cb30903f6d`, `4d8665896c`;
  `hermes_cli/web_host_terminal_sessions.py`: `0cb2f60ac0`, `46da66c337`,
  `252dbe786d`, `43a6470629`, `eda711a5e2`, `d11ff5fb00`, `27e8dbefea`;
  `hermes_cli/web_server_file_tickets.py`: `a3848f78e8`, `11cbd54d50` — webapp
  internals, deferred wholesale per the standing scope.
- **SSH workspace file serving** (`hermes_cli/ssh_workspace_fs.py`) — shared-endpoint
  control-socket teardown, replaced-connection close, down-host stall, plain-Lock:
  `140560affc`, `302fd07584`, `2db4a5262b`, `7b95bb857b` (+tests `dfa7bd3cf8`,
  `64e0b3860a`) — deferred (gateway web surface).
- **Desktop app** — composer draft save/merge across windows (`a950eff764`,
  `e3d5e8c559`, `2406af9920`, `3a818997d7`, `512d8513b7`, `dc23156e1e`), sticky-prompt
  clip/touch (`2cf5948986`, `e136b4894c`, `b8229d3b8d`, `6b27ff58bc`, `16672947ff`,
  `e5f2b15300`), translucency fallback (`8da9e0b432`, `b5868c1f62`), perf
  (`e03c9c668b`, `15732642b0`) — desktop internals, deferred.
- **Plugin-catalog pins** — `aa910ca036` (gonogo v0.3.0, thomas v0.1.3),
  `9976f4857b` (loreconvo 0.10.19): no plugin catalog in the Rust workspace.
- **Chore/deps** — `bb6986f4d9` (brotlicffi 1.2.1.0), `e42cd9d814` (mailmap),
  `edfa1c2a13` (merge), uv.lock/pyproject, i18n keys: ignored.
- Tests-only churn (`27e8dbefea`, `795e9ca42c`, `6ec5ef6720`, `e6ab8816a4`,
  `cb30903f6d`, `4d8665896c`, `b4fa58b262`, `4b4e6e06ab`, `faa7c9c38e`,
  `64e0b3860a`, `dfa7bd3cf8`, `27e8dbefea`, `d49e2ebbae`, `e4bc96b813`,
  `25db5877c1`, `626d7a21a1`, `5712ae891c`, `e9deeb1a15`, `e5f2b15300`,
  `16672947ff`, `1c6871dc55`, `d11ff5fb00`, `122d14b791`, `41715be2f8`,
  `589f3daf97`) and docs: ignored per task rules.

### Verification
- No Rust files touched this wave (see table above); workspace state unchanged from
  the W23 sync. `cargo build` and `cargo test` re-run green (windows-gnu PATH
  prefix): 37 test binaries, **815 passed, 0 failed, 4 ignored**.
