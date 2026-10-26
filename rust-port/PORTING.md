# Rust Port — Wave Ledger & Queue (unattended mode)

## Contract (user-approved 2026-10-25, extended 2026-10-26)
User approved unattended operation: when a wave completes, the session owner
(verifier) MUST, without waiting for the user:
1. `cargo build` + `cargo test` the whole workspace (PATH prefix:
   `export PATH="/c/msys64/mingw64/bin:$HOME/.cargo/bin:$PATH"`, cargo 1.99 windows-gnu).
2. Fix any breakage; never push red.
3. Push the wave to branch `rust-port` on Ehco/hermes-agent via the Git Data API
   (blobs → chunked trees, each chunk asserted ≠ previous base → commit →
   PATCH ref; `git push` hangs here; GitHub rejects single huge tree POSTs —
   chunk at ~150 items; see push_w19.py for a working script).
   Verify the pushed tree by re-fetching it cache-busted.
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

## Push recipe (proven, waves 3–19)
- Base: GET /repos/Ehco/hermes-agent/branches/rust-port → commit sha + tree sha.
- Diff candidates by git-blob hash (sha1("blob <len>\0" + bytes)) vs
  GET /git/trees/<base>?recursive=1 (cache-bust every GET with &zz=<uniq>).
- POST /git/blobs (base64) for changed paths → chunked POST /git/trees
  (base_tree chains, ~150 items per chunk, assert each chunk's sha differs
  from the previous) → POST /git/commits → PATCH /git/refs/heads/rust-port.
- Re-verify: refetch branch + recursive tree; confirm new files present.
- For big pushes use a script file (push_w19.py pattern) run via the
  terminal tool (execute_code's kernel resets between calls).

## Wave ledger (latest first)
- W21: webapp surface (crates/gateway/src/webapp) — WebappSettings from the
  config webapp block (host canonicalization, bundle staleness NOT_BUILT/
  NOT_REBUILT), browser credential (session-token + dependency-free
  SHA-256/HMAC verified vs FIPS/RFC vectors), one-use window/launch tickets
  (generation-bound), static bundle serving (traversal-guarded, stale 503),
  nonce-CSP handoff pages, mint_launch_link for the CLI. head 8bd41591cd6e.
  779 tests, 1045 branch files.
- W20: RetainDB tool surface complete — the ten retaindb_* tools with
  byte-frozen schemas, MemoryContext provider seam, legacy fallbacks,
  visibility gating. head be01fbc78e27. 746 tests, 1038 branch files.
  Deferred tool surface: EMPTY.
- W19: optional-skills content (152 packages, 855 files verbatim into
  crates/skills/optional, embedded fallback root + active-copy shadowing +
  locate fallback + "source":"optional" marking), voice transcription HTTP
  contracts (crates/tools/src/transcription.rs), RetainDB dialectic core
  (crates/tools/src/retaindb.rs). head 818bdb9fe0aa. 723 tests, 1037 branch
  files (incl. 855 optional-content files).
- W18: backlog lanes — Discord wss over rustls (WsStream enum, webpki-roots,
  loopback TLS round-trip test), Slack files_upload_v2 3-step + transient
  retry, MCP server-side expose mode (`hermes-rs --serve-mcp` over the full
  registry, live duplex verified). head 19767afc771e. 703 tests, 180 files.
- W17: kanban subsystem — REGISTRY CLOSED: 15 kanban_* tools, byte-frozen
  schemas, worker-ownership enforcement, mode gating, board-state layer
  (atomic-write board.json; SQLite→JSON deviation documented); schema-parity
  test vs checked-in Python dump (caught 7 drifts). head aff0c67d04f7.
  688 tests. 89 tools = full parity with tools/registry.py.
- W16: UPSTREAM SYNC (~230 commits reviewed, upstream daec3cd4c6): ported
  tool_search step-3 retry, memory batch would_empty guard, 60-char
  skill-description budget; mapping in SYNC.md. head f2009737bdd9. 672 tests.
- W15: tools batch 7 — registry parity: 33 more tools (vision/video/feishu/
  computer_use/browser family/discord/yuanbao), 74 registered.
  head 2eeddcbfe529. 669 tests.
- W14: tools batch 6 — execute_code kernel, desktop_project, video_generate,
  send_message, react_to_message, manage_connections/catalog, gui_tour.
  head b469bf674e23. 623 tests. First release build + "Rusted Hermes"
  desktop shortcut (rusted-hermes.cmd + make-shortcut.ps1).
- W13: tools batch 5 — desktop-ui bridge tools (clarify, set_theme,
  focus_pane, show_tip, read_terminal, read_window_below, desktop_preview,
  drive_preview, annotate_preview) behind an injected DesktopUi trait.
  head 22446aa0a80e. 574 tests.
- W12b: desktop spawn wiring — branch desktop-spawn-rust (commit
  c526080d60e8): HERMES_AGENT_BIN override in electron main (Python default
  unchanged), REST ws-ticket mint branch; Rust-side hermes-gateway binary
  implementing `gateway ensure --json` (crates/gateway/src/main.rs, also on
  rust-port; patch archived at desktop-spawn-rust.patch). Verified: tsc
  clean, vitest 28 passed, live cold-start + warm attach + ticket + WS 101.
- Waves 1–12 earlier: core (agent loop, OpenAI+Anthropic SSE, config),
  tools, session JSONL, skills+memory, schedulers+delegation, MCP client,
  chat gateways (Telegram/Discord/Slack), CLI + TUI, desktop API server
  (/api/ws canonical protocol + REST dashboard, persisted sessions),
  plugins platform.

## Deferred (documented; re-assess periodically)
- Voice: local backends (faster-whisper/local command/streaming), audio
  transcoding, SDK-shaped paths (openai/groq/mistral), xAI OAuth, voice_mode
  host loops — need an audio stack.
- Honcho/RetainDB: MemoryProvider lifecycle, SQLite write-behind queue,
  shared file store, the ten retaindb_* tool schemas — need a
  memory-provider/plugin-host abstraction.
- Webapp/Web Access (~90 upstream commits), Copilot/left-core plugin
  migration, prepared-compaction + shared-metrics v4, vault keychain,
  config backend seam, launchd/telemetry/desktop internals.
- Kanban SQLite storage (JSON board doc deviation), Discord slash
  commands/native streaming/media, Slack admin (not present in Python) —
  minor.

- W22: kanban SQLite storage — Python's verbatim schema (7 tables + 14
  indexes, completion_contract migration, WAL + busy_timeout), legacy JSON
  import, BEGIN IMMEDIATE boundaries, board paths matching kanban_db_path.
  Byte-level board interchange verified LIVE both directions (Rust cards
  read by Python; Python tasks claimed/completed by Rust). head
  35975fafb1b3. 782 tests, 1046 branch files.

## Queue (next → later; one agent per wave)
1. W23 (next): Discord native streaming/slash-commands/media lanes + voice
   local backends assessment.
2. Then: upstream re-sync cadence; RetainDB lifecycle leftovers; webapp
   polish.
3. POST-PORT FLAGSHIP (user-requested 2026-10-26): recreate the desktop UI
   natively in Rust with GPUI (https://gpui.rs/ — Zed's framework,
   Windows-supported) once every port is over. Feasibility wave first
   (crate availability on windows-gnu; window/websocket/system-API needs),
   then the UI surface: chat transcript, composer, tool cards, settings,
   tray/notifications. Contract guide: the desktop API crates/gateway
   already serves.

## Verification totals
- W21 push: 779 tests / 0 failures, 12 crates, build clean. Branch files: 1045.
  Release binaries rebuilt; "Rusted Hermes" shortcut live (smoke-tested).

## How to run locally today (HERMES_HOME points at the real profile)
  export PATH="/c/msys64/mingw64/bin:$HOME/.cargo/bin:$PATH"
  export HERMES_HOME="/c/Users/Administrator/AppData/Local/hermes"
  cd /c/Users/Administrator/hermes-rs
  cargo run --release -p hermes-cli     # line REPL
  cargo run --release -p hermes-tui     # full-screen TUI
  cargo run --release -p hermes-gateway # desktop API server (/api/ws)
  (config.yaml/model keys are read from $HERMES_HOME/config.yaml)
  Or double-click the Desktop shortcut "Rusted Hermes".
