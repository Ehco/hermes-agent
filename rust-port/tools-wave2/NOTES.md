# tools-wave2 — web + media tools port notes (hermes-rs crates/tools)

Source of truth: `C:\Users\Administrator\AppData\Local\hermes\hermes-agent\tools\*.py`
and `plugins\web\*`. Everything here is wired into `crates/tools/src/registry.rs`
(dispatch + ToolSpec) and covered by unit tests.

## What was ported

| Tool | Python source | Rust module |
|---|---|---|
| web_search | tools/web_tools.py + plugins/web/_common.py + plugins/web/ddgs | web_search.rs |
| web_extract | tools/web_tools.py + web_tools_extract.py + web_tools_truncate.py | web_extract.rs |
| image_generate | tools/image_generation_tool.py + image_generation_catalog.py | image_generate.rs |

Shared helpers live in `web_common.rs` (`page_error`, `host_slug`,
`base64_images_to_links`, `hermes_cache_web_dir`, local SHA-256) and
`html_text.rs` (HTML → readable text with markdown links/images, entity
decoding — the direct-fetch stand-in for the Firecrawl/Tavily backends).

## Deliberate deviations (and why)

1. **No vendor SDK backends.** The Python web_extract dispatches through
   Firecrawl/Tavily/Exa/etc. provider plugins. The Rust crate has none of
   those SDKs, so web_extract fetches URLs directly with reqwest and converts
   HTML locally. The *tool-level contract* is what was kept identical:
   `{"results": [{url, title, content, error}]}` (trimmed to those keys),
   `{"success": false, "error": ...}` whole-call failures, and
   `tool_error("Content was inaccessible or not found")` when nothing parsed.
2. **web_search backend.** The Python's keyless tier is the `ddgs` package
   (multi-engine). Here: the DuckDuckGo HTML endpoint (POST, one GET fallback
   when the parse yields zero rows — DDG serves JS challenges to some
   clients), parsed into `title_hit` rows: title-first
   `{title, url, description, position}` with 1-based positions inside
   `{"success": true, "data": {"web": [...]}}`. DDG rate-limits aggressively;
   a rate-limited run yields an empty-but-successful `web` list (same as the
   Python's search_ok([])).
3. **Result-shape key order.** serde_json objects serialize keys sorted, so
   the Python's "key order is part of the contract" wire detail cannot be
   preserved byte-for-byte; key *sets* are preserved instead.
4. **image_generate is the direct-REST FAL queue flow** (`POST
   https://queue.fal.run/<endpoint>` with `Authorization: Key $FAL_KEY`,
   poll `status_url`, fetch `response_url`) replacing `fal_client`. Only the
   default catalog entry ships (`fal-ai/flux-2/klein/9b`, defaults + supports
   whitelist verbatim from the Python catalog); `_build_payload` /
   `_build_fal_edit_payload` filtering semantics (whitelist ∪ required keys,
   defaults overridable, seed only when int, image_size sent on t2i only) are
   faithful and unit-tested. Plugin/managed-gateway routing (Krea/Portal) is
   out of scope; missing FAL_KEY gives the actionable setup error.
5. **think/sleep/current-time were NOT ported**: the Python tool registry
   (`registry.register` set) contains none of them — nothing to match.
6. **Binary-payload detection** compares NUL-stripped magic bytes against the
   decoded (UTF-8) head, so only ASCII-safe magics (SQLite, ZIP, bzip2, ELF,
   GIF, FLAC, Ogg) are reliable; non-ASCII ones (gzip/PNG/JPEG) get mangled by
   the charset decode — the Python has the same property via latin-1 of the
   vendor-returned text, minus the lossy step.
7. **Full-text store** uses `~/.hermes/cache/web/{host_slug}-{sha256[:10]}.md`
   (`HERMES_DIR` override honored). SHA-256 is implemented locally (no hex
   crate), FIPS-180 test-vector verified.
8. **Secret-URL gate** ports `url_safety._SENSITIVE_QUERY_PARAM_NAMES`
   verbatim plus a `_PREFIX_RE` subset (sk-, ghp_, AKIA, xoxb-, JWT bodies…)
   applied to the raw AND percent-decoded URL; a hit refuses the whole call
   (`_refuse_all`).
9. **reqwest::blocking gotcha** (cost real debugging): the blocking client
   must be built on a non-tokio thread AND must never be dropped inside an
   async context (its internal runtime + reqwest's per-request
   `wait::enter` runtime both panic on drop there). Hence the process-global
   `web_common::http_client()` built on a dedicated OS thread, with per-request
   timeouts — the registry's `spawn_blocking` dispatch path never hits it, but
   direct callers might, so the ignored live tests run on plain test threads.

## Wiring

- registry.rs: dispatch arms `web_search` / `web_extract` / `image_generate`;
  three ToolSpecs matching the Python schemas (web_search limit 1–100 default
  5; web_extract urls maxItems 5 + char_limit min 2000; image_generate prompt
  + aspect_ratio enum); spec-count test updated 6 → 9.
- crates/tools/Cargo.toml: `reqwest = { workspace = true, features =
  ["blocking"] }` (the workspace dep already pins 0.12 + rustls).

## Verification status

- `cargo build` → 0 warnings, exit 0 (whole workspace).
- `cargo test` → 125 passed / 0 failed (hermes-tools alone: 87 + 2 network
  ignored).
- Ignored live tests (`-- --ignored live`) verified against the network:
  web_search returned real DDG rows in the exact envelope; web_extract
  extracted https://example.com (title "Example Domain", body text, null
  error). DDG rate-limits quickly — an empty-but-successful list is expected
  on repeat runs.
