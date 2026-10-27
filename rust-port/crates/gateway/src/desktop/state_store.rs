//! Read-only access to the Python backend's `state.db` — the session store
//! the normal (Python-backed) Hermes desktop writes. The Rust gateway serves
//! its own JSONL sessions alongside these so both apps see the same chats.
//!
//! Read-only by design: the Python backend owns the database and may hold
//! WAL write locks; this reader never writes (the Python `sessions` /
//! `messages` schemas in `hermes_state_local`).

use rusqlite::OpenFlags;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

/// Read-only handle on the profile's `state.db`.
pub struct StateDb {
    path: PathBuf,
}

impl StateDb {
    pub fn open(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    fn connect(&self) -> Result<rusqlite::Connection, String> {
        if !self.path.exists() {
            return Err("state.db does not exist".into());
        }
        let conn = rusqlite::Connection::open_with_flags(
            &self.path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(|e| e.to_string())?;
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .map_err(|e| e.to_string())?;
        Ok(conn)
    }

    /// The visible session rows, newest activity first, projected into the
    /// same JSON shape as the gateway's own `rest_row()` rows.
    pub fn list_session_rows(&self, exclude_ids: &std::collections::HashSet<String>) -> Vec<Value> {
        let conn = match self.connect() {
            Ok(conn) => conn,
            Err(_) => return Vec::new(),
        };
        let mut stmt = match conn.prepare(
            "SELECT s.id, COALESCE(NULLIF(s.display_name, ''), s.title, \
             (SELECT substr(TRIM(COALESCE(m.content, '')), 1, 120) FROM messages m \
              WHERE m.session_id = s.id AND m.role = 'user' \
              AND TRIM(COALESCE(m.content, '')) != '' \
              ORDER BY COALESCE(m.display_order, m.id) LIMIT 1), '') AS title, \
             (SELECT substr(TRIM(COALESCE(m.content, '')), 1, 120) FROM messages m \
              WHERE m.session_id = s.id AND m.role = 'user' \
              AND TRIM(COALESCE(m.content, '')) != '' \
              ORDER BY COALESCE(m.display_order, m.id) LIMIT 1) AS preview, \
             s.source, s.cwd, s.started_at, s.ended_at, s.last_activity_at, s.message_count, \
             s.input_tokens, s.output_tokens, s.tool_call_count, \
             COALESCE(s.archived, 0) AS archived, COALESCE(s.pinned, 0) AS pinned, \
             COALESCE(s.hidden, 0) AS hidden, s.git_branch, s.git_repo_root \
             FROM sessions s WHERE COALESCE(s.hidden, 0) = 0 \
             ORDER BY COALESCE(s.last_activity_at, s.started_at, 0) DESC",
        ) {
            Ok(stmt) => stmt,
            Err(_) => return Vec::new(),
        };
        let rows = match stmt.query_map([], |row| {
            Ok(json!({
                "id": row.get::<_, String>("id")?,
                "title": row.get::<_, String>("title")?,
                "started_at": row.get::<_, Option<f64>>("started_at")?,
                "ended_at": row.get::<_, Option<f64>>("ended_at")?,
                "last_active": row.get::<_, Option<f64>>("last_activity_at")?,
                "message_count": row.get::<_, i64>("message_count")?,
                "preview": row.get::<_, Option<String>>("preview")?,
                "source": row.get::<_, String>("source")?,
                "cwd": row.get::<_, String>("cwd")?,
                "model": Value::Null,
                "is_active": false,
                "archived": row.get::<_, i64>("archived")? != 0,
                "pinned": row.get::<_, i64>("pinned")? != 0,
                "unread": false,
                "hidden": row.get::<_, i64>("hidden")? != 0,
                "profile": "default",
                "is_default_profile": true,
                "git_branch": row.get::<_, Option<String>>("git_branch")?,
                "git_repo_root": row.get::<_, Option<String>>("git_repo_root")?,
                "input_tokens": row.get::<_, i64>("input_tokens")?,
                "output_tokens": row.get::<_, i64>("output_tokens")?,
                "tool_call_count": row.get::<_, i64>("tool_call_count")?,
            }))
        }) {
            Ok(mapped) => mapped,
            Err(_) => return Vec::new(),
        };
        let mut out = Vec::new();
        for row in rows.flatten() {
            // Registry/JSONL rows own their ids; never duplicate them.
            let id = row["id"].as_str().unwrap_or("").to_string();
            // Hide never-used sessions (no title, no messages): boot-retry
            // empties would otherwise clutter the sidebar.
            if !exclude_ids.contains(&id)
                && !(row["title"].as_str().map(str::is_empty).unwrap_or(true)
                    && row["message_count"].as_i64().unwrap_or(0) == 0)
            {
                out.push(row);
            }
        }
        out
    }

    /// The visible chat history of one session: `(role, text)` pairs for
    /// user/assistant rows with content, in display order.
    pub fn history(&self, session_id: &str) -> Option<Vec<(String, String)>> {
        let conn = self.connect().ok()?;
        let mut stmt = conn
            .prepare(
                "SELECT role, content FROM messages \
                 WHERE session_id = ?1 AND role IN ('user', 'assistant') \
                 AND TRIM(COALESCE(content, '')) != '' \
                 ORDER BY COALESCE(display_order, id), id",
            )
            .ok()?;
        let rows = stmt
            .query_map([session_id], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .ok()?;
        let mut out = Vec::new();
        for row in rows.flatten() {
            out.push(row);
        }
        Some(out)
    }

    /// One session row by id (the drill-in/open path uses the full row).
    pub fn find_session_row(&self, session_id: &str) -> Option<Value> {
        self.list_session_rows(&std::collections::HashSet::from([session_id.to_string()]))
            .into_iter()
            .find(|row| row["id"] == json!(session_id))
    }
}

/// Convenience: open a StateDb at `path` if the file exists.
pub fn open_if_present(path: impl AsRef<Path>) -> Option<StateDb> {
    let path = path.as_ref();
    path.exists().then(|| StateDb::open(path))
}
