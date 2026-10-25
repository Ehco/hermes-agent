//! hermes-session: JSONL session persistence (port of the Python session
//! store's on-disk contract: one JSON object per line, resume = replay).
//!
//! Every message is a single `serde_json::to_string(&Message)` line, so the
//! file is append-only and a resume is just "read all lines into Vec<Message>".

use anyhow::{Context, Result};
use hermes_core::Message;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

pub struct SessionStore {
    path: PathBuf,
}

impl SessionStore {
    /// Open (or create) the JSONL file at `path`. Parent dirs are created.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating session dir {}", parent.display()))?;
        }
        File::create_new(&path).or_else(|_| File::options().append(true).open(&path))?;
        Ok(Self { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one message as a JSON line (flushed immediately, crash-safe).
    pub fn append(&self, msg: &Message) -> Result<()> {
        let line = serde_json::to_string(msg)?;
        let mut f = OpenOptions::new().append(true).open(&self.path)?;
        writeln!(f, "{line}")?;
        f.flush()?;
        Ok(())
    }

    /// Resume: read every line back into the conversation history.
    /// Blank lines are skipped; a corrupt line fails the load loudly.
    pub fn load(&self) -> Result<Vec<Message>> {
        let text = std::fs::read_to_string(&self.path)?;
        let mut out = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            out.push(serde_json::from_str::<Message>(line).with_context(|| {
                format!("corrupt session line {} in {}", n + 1, self.path.display())
            })?);
        }
        Ok(out)
    }

    /// Drop all history (the `/new` command).
    pub fn clear(&self) -> Result<()> {
        std::fs::write(&self.path, "")?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hermes_core::model::{Content, Role, ToolCall};

    fn temp_path(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-session-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos(),
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    #[test]
    fn roundtrips_messages_through_jsonl() {
        let path = temp_path("session.jsonl");
        let store = SessionStore::open(&path).unwrap();

        let mut assistant = Message::text(Role::Assistant, "thinking");
        assistant.content.push(Content::ToolUse {
            call: ToolCall { id: "c1".into(), name: "terminal".into(), arguments: "{}".into() },
        });
        let result = Message {
            role: Role::Tool,
            content: vec![Content::ToolResult { call_id: "c1".into(), output: "ok".into() }],
        };

        store.append(&Message::text(Role::User, "hi")).unwrap();
        store.append(&assistant).unwrap();
        store.append(&result).unwrap();

        let back = store.load().unwrap();
        assert_eq!(back.len(), 3);
        assert_eq!(back[0].role, Role::User);
        assert_eq!(back[1].role, Role::Assistant);
        match &back[1].content[1] {
            Content::ToolUse { call } => {
                assert_eq!(call.id, "c1");
                assert_eq!(call.name, "terminal");
            }
            other => panic!("wrong content: {other:?}"),
        }
        match &back[2].content[0] {
            Content::ToolResult { call_id, output } => {
                assert_eq!(call_id, "c1");
                assert_eq!(output, "ok");
            }
            other => panic!("wrong content: {other:?}"),
        }
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn resume_appends_after_reload_and_clear_empties() {
        let path = temp_path("session2.jsonl");
        {
            let store = SessionStore::open(&path).unwrap();
            store.append(&Message::text(Role::User, "one")).unwrap();
        }
        // Simulate a restart: reopen, resume, append.
        let store = SessionStore::open(&path).unwrap();
        assert_eq!(store.load().unwrap().len(), 1);
        store.append(&Message::text(Role::User, "two")).unwrap();
        assert_eq!(store.load().unwrap().len(), 2);

        store.clear().unwrap();
        assert!(store.load().unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn skips_blank_lines() {
        let path = temp_path("session3.jsonl");
        std::fs::write(&path, "\n".to_string().repeat(3)).unwrap();
        let store = SessionStore::open(&path).unwrap();
        assert!(store.load().unwrap().is_empty());
        std::fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
