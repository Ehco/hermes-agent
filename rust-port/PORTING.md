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

## Queue (next → later; one agent per wave)
1. W12 desktop spawn wiring: launch the Rust binary instead of Python
   (scratch clone of apps/desktop; never touch the live repo's checkout).
- Backlog: long-tail tools until registry parity; Discord TLS+wss; Slack
  admin/file lanes; MCP server side; optional-skills content.

## Verification totals
- W11 push: 522 tests / 0 failures, 11 crates, build clean. Branch files: 143.
