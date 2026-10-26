# hermes-plugins — port notes

Port of the Python plugins platform (`plugins/` + the dashboard plugin-API
mount in `hermes_cli/web_server_dashboard.py`) to Rust, with the client
surface of `apps/desktop/src/api/plugins.ts` pinned by tests.

## What maps to what

| Python | Rust (this crate) |
| --- | --- |
| `manifest.json` discovery/validation (`iter_plugin_dirs`, `_NAME_RE`) | `manifest::PluginManifest` (`from_dir`, `name_is_valid`) |
| `PluginManager` (register, enable/disable, lifecycle) | `host::PluginHost` (`register`, `enable`/`disable`, `start_all`/`stop_all`) |
| `plugins.isolation: host` (`hermes_cli/plugin_host.py`) | not ported — Rust plugins link in, so there is no out-of-process host to proxy; see "Deferred" |
| per-plugin `/events` WS tailers (kanban `_EventTail`, 300 ms SQLite poll) | `bus::EventBus` — direct fanout, same frames, per-plugin monotonic `cursor` |
| `_mount_plugin_api_routes` / hot-mount fallback | `routes::plugin_api_router` — a live dispatcher, so enable/disable takes effect per request |
| per-request runtime consent gate (`_plugin_api_request_refused`) | `dispatch` checks `PluginHost::is_enabled` per request (unknown name → 404, like Python) |
| `tests/fixtures/plugins/example-dashboard` | `plugins::ExamplePlugin` (`/hello`, `/whoami`) |
| `plugins/kanban/dashboard/plugin_api.py` | `plugins::KanbanPlugin` (board REST + `/events` WS) |
| `plugins/hermes-achievements/dashboard/plugin_api.py` | `plugins::AchievementsPlugin` (REST-only, like the Python original) |

## Desktop envelope (the contract the tests pin)

`GET /api/plugins/<name>/events?since=<cursor>` sends frames:

```json
{ "events": [ { "id": 1, "task_id": 1, "kind": "created", "payload": null, "created_at": 0 } ], "cursor": 1 }
```

- no `since` → start at the tail (the client already holds the snapshot);
  explicit `since=0` replays retained history (Python `_since_param`).
- empty publishes emit no frame (Python tailers only send on new rows).
- the desktop `pluginSocket` forwards frames to the app as parsed JSON, so
  this frame shape IS the app-visible contract (kanban panel).

## Gateway adapter (crates/gateway)

The framework is standalone; `crates/gateway` adopts it with one call before
layering its middleware:

```rust
use hermes_plugins::{PluginHost, attach_gateway};
use std::sync::Arc;

let host = Arc::new(PluginHost::default());
// register built-ins + enable defaults here
let app = attach_gateway(Router::new(), Arc::clone(&host));
// ... then the gateway's own routes/layers as today
```

`attach_gateway` merges `GET /api/plugins`, the enable/disable control routes,
and a live dispatcher at `/api/plugins/<name>/<rest>` that runs the request
through the named plugin's own router (404 for disabled/unknown names) — so
enable/disable takes effect immediately, no router rebuild needed, mirroring
the Python hot-mount fallback. The dispatcher strips the namespace prefix and
re-attaches the query string; hyper's upgrade state is preserved so plugin
WebSocket routes upgrade through the dispatcher.

Auth note: the Python side gates `/api/plugins/...` behind dashboard auth
(token/native grant/OAuth). That lives in the gateway's middleware, not here —
the framework exposes the same routes unauthenticated over its own router and
delegates auth to the mounting server (same seam as `web_server_dashboard`
using the shared middleware stack).

## Deviations from Python

- Plugin "loading" = factory lookup by manifest `name` (Rust plugins are
  linked in; there is no dynamic import). A directory whose name has no
  factory is recorded as an error, like an unenabled user plugin.
- `created_at` is epoch-millis (`u64`), not an ISO string.
- Kanban board state is in-memory (single process); the Python original
  persists to SQLite (WAL) — persistence is deferred with the other plugins.
- Achievements: definitions subset + explicit unlock flip; the Python
  session-scan evaluator is not ported.

## Deferred plugins (by desktop usage)

- `memory` providers (`byterover`, `retaindb`) — provider-category plugins,
  need the `PluginContext.register_memory_provider` seam, not dashboard tabs.
- `context_engine`, `cron_providers` — same category-plugin story.
- `image_gen`, `video_gen`, `google_meet`, `teams_pipeline`, `disk-cleanup`,
  `dashboard_auth` (framework-level concern, mostly gateway middleware),
  `browser`/`computer_use`/`web` (tool-provider plugins).
- SQLite-backed kanban persistence + multi-board support (`?board=<slug>`).
- Kanban multipart upload routes (`POST /tasks/{id}/files`).
- Static asset serving (`/dashboard-plugins/<name>/...`) — the desktop loads
  plugin UI bundles through a different channel today.
