# Rust Port — Wave Ledger & Queue (unattended mode)

## Contract (user-approved 2026-10-25)
User approved unattended operation: when a wave completes, the session owner
(verifier) MUST, without waiting for the user:
1. `cargo build` + `cargo test` the whole workspace (PATH prefix:
   `export PATH="/c/msys64/mingw64/bin:$HOME/.cargo/bin:$PATH"`, cargo 1.99 windows-gnu).
2. Fix any breakage; never push red.
3. Push the wave to branch `rust-port` on Ehco/hermes-agent via the Git Data API
   (blobs → tree with base_tree → commit → PATCH ref; `git push` hangs in this
   environment). Verify the pushed tree by re-fetching it cache-busted.
4. Update THIS file (move wave from queue to ledger, append next).
4b. Runnable-checkpoint duty (user-added 2026-10-26): after each wave,
   cargo build --release -p hermes-tui -p hermes-gateway, refresh the desktop
   launcher (C:\Users\Administrator\hermes-rs\rusted-hermes.cmd + Desktop
   shortcut "Rusted Hermes" via make-shortcut.ps1), smoke-test the release
   binary, and note run blockers here.
5. Immediately dispatch the next wave (one agent at a time) and end turn.
Stop and ask the user only for: credentials/secrets, killing non-agent
processes, or a genuine design fork. Never print the GitHub token
(C:\Users\Administrator\.git-credentials, account Ehco). Never
checkout/reset in the live repo C:\Users\Administrator\AppData\Local\hermes\hermes-agent.

## Push recipe (proven, waves 3–7)
- Base: GET /repos/Ehco/hermes-agent/branches/rust-port → commit sha + tree sha.
- Diff candidates by git-blob hash (sha1("blob <len>\0" + bytes)) vs
  GET /git/trees/<base>?recursive=1 (cache-bust every GET with &zz=<uniq>).
- POST /git/blobs (base64) for changed paths → POST /git/trees {base_tree, tree}
  → ASSERT returned sha != base tree (wave-3 lesson: merges can no-op) →
  POST /git/commits {tree, parents:[base]} → PATCH /git/refs/heads/rust-port.
- Re-verify: refetch branch + recursive tree; confirm new files present.

## Done (waves 1–7)
- W1-2: core (agent loop, OpenAI+Anthropic SSE, config), 12 tools, session
  JSONL, CLI REPL + tool bridge. Pushed.
- W3: skills + memory store, schedulers (cron) + delegation. head a347f0eb37.
- W4: MCP client (stdio+HTTP), Telegram gateway. head 091da86e43.
- W5: Discord+Slack gateways, tools batch 2 (memory/todo/skills/cronjob/
  session_search wrappers), CLI slash commands + session switching. head beff889c6a.
- W6: TUI (ratatui) reusing the CLI Repl; crates/cli lib+bin. head 7e5ecc3950.
- W7: desktop API server in crates/gateway — /api/ws canonical protocol
  (tickets, streaming, replay, approvals), REST dashboard (sessions/skills/
  mcp/system). head 587de791c40d. Workspace: 10 crates, 430 tests green.

- W8: plugins platform (crates/plugins): manifest gates, event bus with
  desktop envelope + ?since= replay, PluginHost consent gate, live
  /api/plugins dispatcher (WS-capable), attach_gateway adapter; kanban +
  achievements + example plugins. head b0f164b7be1b. 11 crates, 449 tests.
- W9: tools batch 3: process_manage (background session registry + terminal
  background=true), x_search, maps, tts, delegate_task, skill_manage,
  osv_check (internal). head 1b45d23499d7. 11 crates, 502 tests, 26 tools files.
- W10: gateway hardening: desktop sessions persisted to the JSONL store
  (construction replay, start recovery, resume reload, DELETE removes file),
  PATCH/mutation-snapshot/fenced-DELETE, /messages pagination contract,
  archived/order filters, SessionInfo rows, skills bare-array + PUT,
  profiles dir read. head d1b7942d8000. 11 crates, 505 tests.

## W11 — tools batch 4: tool_search bridge + niche subsystem evaluation
Ported (crates/tools/src/tool_search.rs, +registry wiring):
- tool_search catalog machinery: stateless BM25 catalog over deferrable
  tool-defs (rarest-token gate + term-coverage floor + exact-name-first),
  budgeted byte-stable listing (full → names → per-server summary collapse),
  ToolSearchConfig.from_raw (clamped fields, legacy bool, tri-state aliases,
  curated default defer list from config_defaults), classify_tools /
  is_deferrable_tool_name, estimate_tokens_from_schemas, should_activate,
  listing_token_budget, bridge_tool_schemas (tool_search/tool_describe/
  tool_call), assemble_tool_defs + AssemblyResult (idempotent).
- tool_search_validation: normalize_tool_call_entries (batch + legacy single +
  JSON-string envelopes), local_batch_error echo shape,
  validate_deferred_call_args (required-probe, fail-open),
  session_unavailable_error / not_deferrable_error / scope messages verbatim.
- Bridge dispatch: dispatch_tool_search (per-query groups, shared tools map
  with clipped descriptions + required, empty-group available_sources hint),
  dispatch_tool_describe (schemas / not_found / errors), resolve_underlying_call.
- Registered tool_search/tool_describe/tool_call in registry.rs (24 specs);
  execute_call routes through registry execute_sync after validation.
- 24 unit tests in tool_search.rs (no network). hermes-tools: 192 passed.
Divergences (documented in the module header): no connectors subsystem (the
remote Nous tool-gateway search/describe, CONNECTOR_BATCH_SENTINEL, hosted
failure reporting) — the local catalog is the whole corpus; stemming is a
built-in suffix stripper instead of Snowball; hidden_declared_sources returns
empty (no platform declaration/MCP liveness); validate_deferred_call_args is
required-probe only (no jsonschema crate); record_tool_search metrics skipped.
Deferred with reasons:
- Honcho dialectic user modeling: entangled — agent/memory_provider.py +
  hermes_cli/{profiles_honcho,profile_memory_config,subcommands/memory}.py +
  the `honcho` SDK dependency; not a self-contained module. Revisit with the
  memory-provider abstraction if ported.
- Voice transcription: 11 transcription_*.py files (~3.5k lines) + voice_mode.py
  (1.5k) with heavy audio deps (faster-whisper/ffmpeg/streaming); only the
  cloud-HTTP slice would be portable, but it is interleaved with the local
  backends — defer wholesale.
- weekly-review / document-to-action-items / meeting-action-items: SKILLS only
  in Python (no agent-tool registry entries) — nothing to port as tools.
- memory nudge / skill-creation loop: system-prompt/agent-loop hooks, not
  tools; skill_manage/memory already cover the tool surface.
- session_search extra verbs (session_id read, @session: profile links,
  lineage/profiles): require the multi-session SQLite/FTS5 state.db; the Rust
  store is a single JSONL transcript — the three shapes (discover/scroll/
  browse) are already ported and stay as-is.

## W14 — tools batch 6
execute_code (full programmatic tool calling kernel: stdin JSON-cell wire
protocol with per-kernel sentinel, loopback tool-RPC server with token +
whitelist + per-cell budget, sandbox hermes_tools.py stub generator,
persistent per-owner kernels, kernel-died/hint result shapes),
desktop_project (ProjectsStore trait, projects.json fallback),
video_generate, send_message, react_to_message (tapback semantics),
manage_connections, manage_catalog, gui_tour. Registry now 41 tools.
Pushed: head b469bf674e23. Workspace: 623 tests, 0 failures. First release
build + "Rusted Hermes" desktop shortcut created (rusted-hermes.cmd +
make-shortcut.ps1, smoke-tested).

- W14: tools batch 6 — execute_code (full programmatic tool calling kernel),
  desktop_project, video_generate, send_message, react_to_message,
  manage_connections, manage_catalog, gui_tour. head b469bf674e23. 623 tests,
  registry 41 tools. First release build + "Rusted Hermes" desktop shortcut.
- W15: tools batch 7 — registry PARITY: 33 more tools (vision/video/feishu/
  computer_use/browser family/discord/yuanbao), 74 registered. Only the 15
  kanban_* tools deferred (kanban-mode subsystem). head 2eeddcbfe529.
  12 crates, 669 tests, 172 branch files.

- W16: UPSTREAM SYNC (~230 commits reviewed, upstream daec3cd4c6): ported
  tool_search step-3 retry semantics, memory batch would_empty guard,
  60-char skill-description budget; full mapping in SYNC.md. head
  f2009737bdd9. 672 tests, 173 branch files.

## W17 — kanban subsystem: REGISTRY CLOSED
All 15 kanban_* tools with byte-frozen schemas, Python-identical errors,
worker-ownership enforcement, mode gating; board-state layer (atomic-write
board.json per board, statuses/links/runs/comments/events/attachments/
heartbeat/claim, recompute_ready, snapshot-rollback). Schema parity enforced
by a checked-in Python-dump diff test (caught 7 drifts). Deviations:
SQLite -> atomic JSON board doc; profile toolset gate -> HERMES_KANBAN_TOOLSET env.
Pushed: head aff0c67d04f7. Workspace: 688 tests, 0 failures. Registry: 89 tools
= FULL PARITY with tools/registry.py. Release build + shortcut refreshed, smoke-tested.

- W18: backlog lanes — Discord wss over rustls (WsStream enum, webpki-roots,
  loopback TLS round-trip test), Slack files_upload_v2 3-step + transient
  retry, MCP server-side expose mode (`hermes-rs --serve-mcp` over the full
  registry, live duplex verified). head 19767afc771e. 703 tests, 180 files.

## Queue (next → later; one agent per wave)
1. W19 (next): optional-skills content port (optional-skills/ → a skills
   content crate/dir the discovery layer picks up) + voice/Honcho
   deferral re-assessment.

## W19 — optional-skills content + voice/Honcho re-assessment
1. Optional-skills CONTENT: the Python repo's optional-skills/ tree copied
   VERBATIM into crates/skills/optional/ (152 SKILL.md packages, 854 files,
   14MB; category/<skill>/ layout preserved; upstream daec3cd4c6). Wired as a
   fallback root: hermes_skills::optional_skills_root() resolves
   $HERMES_OPTIONAL_SKILLS, else the embedded optional/ (CARGO_MANIFEST_DIR —
   works in-repo/cargo test; packaged builds set the env var).
   discover_skills_with_optional() merges the catalog BEHIND the active
   skills root (an active copy of the same name shadows the optional one —
   hub-catalog vs installed tiering, as on the Python side);
   locate_skill_with_optional() gives skill_view the fallback; tools-level
   skills_list marks optional rows "source": "optional". Registry untouched
   (closed at 89). See crates/skills/optional/README.md.
2. Voice transcription RE-ASSESSMENT: the cloud-STT provider HTTP contracts
   ported to crates/tools/src/transcription.rs over an injected
   MultipartTransport — shared result envelopes (_ok_result/_error_result/
   no_speech), the transcript-text normalizer (_extract_transcript_text:
   string | {text} | {error} | Voxtral <asr_text> prefix, the #78098 rule),
   the language ladder (_resolve_stt_language), xAI model normalization +
   /v1/stt request contract (model always named; format gated on language;
   diarize default off), ElevenLabs /speech-to-text contract (model_id,
   lowercase-stringified bools, language_code omitted when blank,
   detail.message error extraction), and the shared _rest_provider flow
   (non-200 → "<label> API error (HTTP n): detail" with 300-char fallback;
   empty text → no_speech). STILL Python-side (the deferral's remaining
   substance): local backends (faster-whisper/local command/streaming),
   audio transcoding, the OpenAI/groq/mistral SDK-shaped paths, xAI OAuth
   credential resolution, and the voice_mode/voice_live host loops — all
   local-audio or SDK-entangled, not portable without an audio stack.
3. Honcho dialectic RE-ASSESSMENT: the deferred subsystem is today's
   RetainDB memory plugin (Honcho's successor; hermes_cli/profiles_honcho.py
   is just a config-file rename helper). Portable core ported to
   crates/tools/src/retaindb.rs over an injected RetainDbTransport: the
   dialectic/agent-self-model HTTP contracts (ask_user →
   POST /v1/memory/profile/{user}/ask with {project, query,
   reasoning_level}; get_agent_model/seed_agent_identity →
   /v1/memory/agent/{id}/{model,seed}; get_profile →
   /v1/memory/profile/{user}?project&include_pending), the reasoning-level
   ladder (<120 low / <400 medium / else high), agent-model gating
   (memory_count > 0), the prefetch cache semantics (queue_prefetch fills;
   prefetch() consumes exactly once and clears), the agent self-model block
   formatting (_AGENT_MODEL_FIELDS) and the context-overlay builder
   (_build_overlay + _compact/_norm: 320-char compaction, top-5 each,
   cross-dedupe against local entries). Still unported (the original
   entanglement holds): the MemoryProvider lifecycle, the SQLite
   write-behind queue, the shared file store, and the ten retaindb_* tool
   schemas — they need the memory-provider/plugin-host abstraction the Rust
   port doesn't have. Both modules are seam-injected and registry-free (no
   network in tests); hermes-tools + hermes-skills green, full workspace
   build/test run in this wave.

## Verification totals
- W17 push: 688 tests / 0 failures, 12 crates, build clean. Branch files: 179.
  REGISTRY CLOSED at 89 tools — full parity with tools/registry.py.
  Release binaries rebuilt; shortcut live (smoke-tested).

## How to run locally today (HERMES_HOME points at the real profile)
  export PATH="/c/msys64/mingw64/bin:$HOME/.cargo/bin:$PATH"
  export HERMES_HOME="/c/Users/Administrator/AppData/Local/hermes"
  cd /c/Users/Administrator/hermes-rs
  cargo run --release -p hermes-cli     # line REPL
  cargo run --release -p hermes-tui     # full-screen TUI
  cargo run --release -p hermes-gateway # desktop API server (/api/ws)
  (config.yaml/model keys are read from $HERMES_HOME/config.yaml)
- Done extra: W13 tools batch 5 — desktop-ui bridge tools (clarify,
  set_theme, focus_pane, show_tip, read_terminal, read_window_below,
  desktop_preview, drive_preview, annotate_preview) behind an injected
  DesktopUi trait; registry at 33 tools. head 22446aa0a80e. 574 tests.
- Done extra: W12b desktop spawn wiring — branch desktop-spawn-rust
  (commit c526080d60e8): HERMES_AGENT_BIN override in electron main
  (Python default unchanged), agent-bin decision logic + 5 vitest tests,
  REST ws-ticket mint branch; Rust-side hermes-gateway binary implementing
  `gateway ensure --json` (crates/gateway/src/main.rs, also on rust-port
  head c1d48b2492d0, patch archived at desktop-spawn-rust.patch). Verified:
  tsc clean, vitest 28 passed, live cold-start + warm attach + ticket +
  WS 101. 12 crates, 525 tests.
- Backlog: long-tail tools until registry parity; Discord TLS+wss; Slack
  admin/file lanes; MCP server side; optional-skills content.

## Verification totals
- W15 push: 669 tests / 0 failures, 12 crates, build clean. Branch files: 172.
  REGISTRY PARITY reached (74/83 Python tools; kanban_* subsystem deferred).
