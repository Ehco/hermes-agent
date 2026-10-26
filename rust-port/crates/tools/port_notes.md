# hermes-tools — port notes: the RetainDB tool surface

Port of the ten `retaindb_*` agent tools from
`plugins/memory/retaindb/__init__.py` (`_SCHEMAS` + `_TOOLS` +
`handle_tool_call`/`_dispatch`), closing the last deferred tool surface.
Source of truth was re-read from the Python plugin; schemas, validation
order, error strings and output envelopes are byte-frozen and pinned by
tests.

## What maps to what

| Python | Rust (crates/tools) |
| --- | --- |
| `_SCHEMAS` (ten tool schemas) | `retaindb::tools::{TOOL_NAMES, spec_description, spec_schema}` — registered in `registry::specs()` after the kanban block (registry is now 99) |
| `_TOOLS` dispatch + `_dispatch` required probe | `retaindb::tools::execute` — same order: client check → unknown tool → required arg → handler |
| `handle_tool_call`'s `not self._client` → `tool_error("RetainDB not initialized")` | no injected `RetainDbTransport` → the same error, before any validation |
| `_Client` memory/context/file routes | `RetainDbClient::{search, query_context, get_profile_with_fallback, add_memory, delete_memory, list_files, get_file, read_file_content, upload_file, ingest_file, delete_file}` |
| `_with_fallback` legacy routes | profile → `/v1/memories?…&limit=200`; add_memory → POST `/v1/memories` (bare body); delete_memory → DELETE `/v1/memories/{id}` |
| `_context_overlay` envelope | `{"context": <overlay>, "raw": <query result>}` via the existing `build_overlay` |
| `MemoryProvider` lifecycle fields (user/session/agent ids, project, base_url, api key) | `MemoryContext` trait, injected via `retaindb::tools::set_memory_context`; default `EnvMemoryContext` resolves the standalone-CLI case from the environment |
| `is_available()` (scoped `RETAINDB_API_KEY`) | `retaindb::tools::provider_available()` |
| `memory_provider_tools_enabled` / `_exposed` (the `memory` toolset gate) | `retaindb::tools::memory_toolset_enabled()` + `retaindb_tools_visible()`; the static registry advertises all ten (kanban precedent) and hosts filter |

## The seam contract

- `RetainDbTransport` (request / request_raw / download) is the HTTP layer —
  install with `retaindb::tools::set_transport`. `request_raw` covers the
  multipart upload (`_Client._raw`); `download` covers binary file content.
- Without a transport, every tool answers `{"error": "RetainDB not
  initialized"}` — the Python pre-initialize behavior, and it precedes the
  required-arg probe (pinned by test).
- Unknown `retaindb_*` names → `{"error": "Unknown tool: <name>"}`; missing
  required args (Python truthiness: absent/null/""/false/0) →
  `{"error": "<arg> is required"}`; HTTP failures → `{"error": …}` through the
  bounded `tool_error` body.

## Lifecycle gaps (deliberate, documented)

1. **config.yaml leg of non-secret config**: Python `initialize` resolves
   `base_url`/`project` from env → the Dashboard-written `memory.retaindb`
   config.yaml block → defaults. The Rust default context reads the
   environment only (`RETAINDB_BASE_URL`, `RETAINDB_PROJECT`, `HERMES_HOME`
   for the `hermes-<profile>` partition fallback); a host that wants the
   config.yaml leg injects a `MemoryContext` that reads it.
2. **`raise_if_read_blocked`**: Python gates the upload's local read through
   `agent.file_safety` (secret dirs, .env files…). The Rust upload performs
   the existence check and read but not that gate — the file-safety module
   has no Rust port yet; wire it into `retaindb_upload_file` when it exists.
3. **`mimetypes.guess_type`**: approximated by a small extension table
   (`guess_mime`) defaulting to `application/octet-stream`; exotic stdlib
   entries may differ (cosmetic — the mime only rides the multipart part).
4. **SOUL.md seeding, write-behind SQLite queue, session ingest**:
   `initialize`/`sync_turn`/`shutdown` lifecycle (background threads +
   `retaindb_queue.db`) stays unported — host-side lifecycle the Rust
   registry doesn't run; the tool surface does not depend on it.
5. **`on_memory_write` mirroring** (built-in memory adds mirrored to
   RetainDB): needs the memory-manager hook point; deferred with the
   lifecycle.
6. **int/float coercion**: `top_k`/`limit`/`importance` mirror Python's
   `int()`/`float()` edge behavior (float truncation, string parsing,
   Python's error texts); the schema-level `enum` validation for
   `memory_type`/`scope` stays schema-only, like Python's dispatch.

## Verification

`cargo build` and `cargo test -p hermes-tools` cover: per-tool contract tests
against a scripted transport (wire paths, bodies, fallbacks, envelopes),
validation-order tests, `EnvMemoryContext` resolution tests, and the
visibility gate tests — no live network anywhere. The MCP server test's
"registry closed at 89" comment still holds as `tools.len() > 80`.
