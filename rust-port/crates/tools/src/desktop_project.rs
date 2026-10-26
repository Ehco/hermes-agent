//! desktop_project — the agent's INTENTIONAL handle on first-class Projects
//! (port of `tools/project_tools.py`).
//!
//! Per-profile named workspaces (the desktop sidebar's projects). Creating or
//! switching is an explicit tool call, never a side effect of `cd`. The store
//! is injected as the `ProjectsStore` trait: the desktop host supplies its
//! per-profile store (Python: `projects.db`), and the default here is a
//! minimal own-format JSON file (`projects.json` under the Hermes home) that
//! keeps the same contract without a sqlite dependency. The workspace
//! callback seam (`set_workspace_callback`) is what a live GUI session uses to
//! re-anchor its cwd; a live session create/switch re-anchors only that
//! session and must not move the profile-global active pointer.

use serde_json::{json, Value};
use std::sync::RwLock;

#[derive(Debug, Clone, PartialEq)]
pub struct Project {
    pub id: String,
    pub slug: String,
    pub name: String,
    /// Folder paths; the first was created first, any may be flagged primary.
    pub folders: Vec<String>,
    pub primary_path: Option<String>,
    pub archived: bool,
}

impl Project {
    /// Python `_primary_path`: the explicit primary, else the first
    /// primary-flagged folder, else the first folder.
    pub fn primary_path(&self) -> Option<String> {
        self.primary_path
            .clone()
            .or_else(|| self.folders.first().cloned())
    }
}

/// The projects store seam (Python `hermes_cli.projects_db`).
pub trait ProjectsStore: Send + Sync {
    /// `include_archived=false` filters archived projects (list action).
    fn list_projects(&self, include_archived: bool) -> Vec<Project>;
    /// Insert; returns the new project's id. `Err` carries the ValueError text
    /// (e.g. a duplicate slug).
    fn create_project(
        &self,
        name: &str,
        folders: Vec<String>,
        primary_path: Option<String>,
    ) -> Result<String, String>;
    /// The folder already belongs to a project (idempotent create).
    fn find_by_primary_path(&self, path: &str) -> Option<Project>;
    /// The profile-global active pointer.
    fn get_active_id(&self) -> Option<String>;
    fn set_active(&self, id: &str);
}

/// The workspace re-anchoring hook a live GUI session supplies.
pub trait WorkspaceCallback: Send + Sync {
    fn invoke(&self, task_id: &str, path: &str, name: &str);
}

impl<F: Fn(&str, &str, &str) + Send + Sync> WorkspaceCallback for F {
    fn invoke(&self, task_id: &str, path: &str, name: &str) {
        self(task_id, path, name)
    }
}

static WORKSPACE_CALLBACK: RwLock<Option<std::sync::Arc<dyn WorkspaceCallback>>> =
    RwLock::new(None);
static STORE: RwLock<Option<std::sync::Arc<dyn ProjectsStore>>> = RwLock::new(None);

/// Wire the GUI gateway's workspace re-anchoring callback.
pub fn set_workspace_callback(cb: Option<std::sync::Arc<dyn WorkspaceCallback>>) {
    *WORKSPACE_CALLBACK.write().unwrap() = cb;
}

/// Install the profile's store (desktop hosts, tests). None restores the
/// default JSON-file store.
pub fn set_store(store: Option<std::sync::Arc<dyn ProjectsStore>>) {
    *STORE.write().unwrap() = store;
}

fn store() -> std::sync::Arc<dyn ProjectsStore> {
    STORE
        .read()
        .unwrap()
        .clone()
        .unwrap_or_else(|| std::sync::Arc::new(JsonProjectsStore::default_store()))
}

fn default_store_path() -> Option<std::path::PathBuf> {
    if let Ok(home) = std::env::var("HERMES_HOME") {
        if !home.trim().is_empty() {
            return Some(std::path::PathBuf::from(home).join("projects.json"));
        }
    }
    std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .ok()
        .map(|home| {
            std::path::PathBuf::from(home)
                .join(".hermes")
                .join("projects.json")
        })
}

/// Minimal own-format fallback store: one JSON document at
/// `$HERMES_HOME/projects.json` (else `~/.hermes/projects.json`).
pub struct JsonProjectsStore {
    path: Option<std::path::PathBuf>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct JsonDoc {
    #[serde(default)]
    next_id: u64,
    #[serde(default)]
    active_id: Option<String>,
    #[serde(default)]
    projects: Vec<JsonProject>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct JsonProject {
    id: String,
    slug: String,
    name: String,
    #[serde(default)]
    folders: Vec<String>,
    #[serde(default)]
    primary_path: Option<String>,
    #[serde(default)]
    archived: bool,
}

impl From<&Project> for JsonProject {
    fn from(p: &Project) -> Self {
        Self {
            id: p.id.clone(),
            slug: p.slug.clone(),
            name: p.name.clone(),
            folders: p.folders.clone(),
            primary_path: p.primary_path.clone(),
            archived: p.archived,
        }
    }
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars() {
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    slug
}

impl JsonProjectsStore {
    pub fn default_store() -> Self {
        Self {
            path: default_store_path(),
        }
    }

    pub fn at(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn load(&self) -> JsonDoc {
        if let Some(path) = &self.path {
            if let Ok(text) = std::fs::read_to_string(path) {
                if let Ok(doc) = serde_json::from_str::<JsonDoc>(&text) {
                    return doc;
                }
            }
        }
        JsonDoc {
            next_id: 1,
            active_id: None,
            projects: vec![],
        }
    }

    fn save(&self, doc: &JsonDoc) {
        if let Some(path) = &self.path {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Ok(text) = serde_json::to_string_pretty(doc) {
                let _ = std::fs::write(path, text);
            }
        }
    }
}

impl ProjectsStore for JsonProjectsStore {
    fn list_projects(&self, include_archived: bool) -> Vec<Project> {
        self.load()
            .projects
            .iter()
            .filter(|p| include_archived || !p.archived)
            .map(|p| Project {
                id: p.id.clone(),
                slug: p.slug.clone(),
                name: p.name.clone(),
                folders: p.folders.clone(),
                primary_path: p.primary_path.clone(),
                archived: p.archived,
            })
            .collect()
    }

    fn create_project(
        &self,
        name: &str,
        folders: Vec<String>,
        primary_path: Option<String>,
    ) -> Result<String, String> {
        if name.trim().is_empty() {
            return Err("name is required".into());
        }
        let mut doc = self.load();
        let base = slugify(name);
        let base = if base.is_empty() {
            "project".into()
        } else {
            base
        };
        let mut slug = base.clone();
        let mut n = 1;
        while doc.projects.iter().any(|p| p.slug == slug) {
            n += 1;
            slug = format!("{base}-{n}");
        }
        let id = format!("proj_{:06}", doc.next_id);
        doc.next_id += 1;
        doc.projects.push(JsonProject {
            id: id.clone(),
            slug,
            name: name.to_string(),
            folders,
            primary_path,
            archived: false,
        });
        self.save(&doc);
        Ok(id)
    }

    fn find_by_primary_path(&self, path: &str) -> Option<Project> {
        let wanted = normalize_path(path);
        self.list_projects(true).into_iter().find(|p| {
            p.primary_path()
                .map_or(false, |pp| normalize_path(&pp) == wanted)
        })
    }

    fn get_active_id(&self) -> Option<String> {
        self.load().active_id
    }

    fn set_active(&self, id: &str) {
        let mut doc = self.load();
        doc.active_id = Some(id.to_string());
        self.save(&doc);
    }
}

fn normalize_path(path: &str) -> String {
    let expanded = if path.starts_with("~/") || path == "~" {
        format!(
            "{}{}",
            std::env::var("USERPROFILE")
                .or_else(|_| std::env::var("HOME"))
                .unwrap_or_default(),
            &path[1..]
        )
    } else {
        path.to_string()
    };
    expanded.trim_end_matches(['/', '\\']).to_lowercase()
}

/// Execute `desktop_project` — the registry entry point (default store, no
/// workspace callback).
pub fn execute(args: &Value) -> String {
    execute_with(store(), None, None, args)
}

/// `execute` against an injected store + calling-session context (desktop
/// hosts, tests): `task_id` identifies the live session and `callback` the
/// workspace re-anchoring hook.
pub fn execute_with(
    store: std::sync::Arc<dyn ProjectsStore>,
    task_id: Option<&str>,
    callback: Option<&dyn WorkspaceCallback>,
    args: &Value,
) -> String {
    let action = args.get("action").and_then(Value::as_str).unwrap_or("");
    match action.trim() {
        "list" => list(&store),
        "create" => create(&store, task_id, callback, args),
        "switch" => switch(&store, task_id, callback, args),
        other => json!({
            "success": false,
            "error": format!(
                "action must be one of: create, switch, list. (got '{other}')"
            ),
        })
        .to_string(),
    }
}

fn list(store: &std::sync::Arc<dyn ProjectsStore>) -> String {
    // Another tab's switch moves the profile-global pointer; this chat's
    // project is its own cwd. Without the gateway's terminal-override
    // registration (cwd_source) the caller is unscoped and falls back to the
    // profile-global pointer.
    let active = store.get_active_id();
    let projects = store.list_projects(false);
    json!({
        "active_id": active,
        "projects": projects
            .iter()
            .map(|p| json!({
                "id": p.id,
                "slug": p.slug,
                "name": p.name,
                "primary_path": p.primary_path(),
                "active": Some(&p.id) == store.get_active_id().as_ref(),
            }))
            .collect::<Vec<_>>(),
    })
    .to_string()
}

/// True when a live GUI session's workspace will follow the project.
fn moves_session(
    task_id: Option<&str>,
    path: Option<&str>,
    callback: Option<&dyn WorkspaceCallback>,
) -> bool {
    callback.is_some() && task_id.is_some() && path.is_some()
}

fn activated(
    project: &Project,
    task_id: Option<&str>,
    callback: Option<&dyn WorkspaceCallback>,
) -> String {
    let primary = project.primary_path();
    if let (Some(task_id), Some(path), Some(cb)) = (task_id, primary.as_deref(), callback) {
        cb.invoke(task_id, path, &project.name);
    }
    json!({
        "success": true,
        "id": project.id,
        "slug": project.slug,
        "name": project.name,
        "primary_path": primary,
    })
    .to_string()
}

fn create(
    store: &std::sync::Arc<dyn ProjectsStore>,
    task_id: Option<&str>,
    callback: Option<&dyn WorkspaceCallback>,
    args: &Value,
) -> String {
    let name = args
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    if name.is_empty() {
        return json!({"success": false, "error": "name is required"}).to_string();
    }
    let folder = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string);
    // Idempotent create: the folder already belongs to a project. Reusing it
    // beats minting a duplicate — duplicated projects render N identical
    // sidebar subtrees.
    let project = match folder {
        Some(ref folder) => match store.find_by_primary_path(folder) {
            Some(existing) => Some(existing),
            None => match store.create_project(name, vec![folder.clone()], Some(folder.clone())) {
                Ok(id) => store.list_projects(true).into_iter().find(|p| p.id == id),
                Err(e) => return json!({"success": false, "error": e}).to_string(),
            },
        },
        None => match store.create_project(name, vec![], None) {
            Ok(id) => store.list_projects(true).into_iter().find(|p| p.id == id),
            Err(e) => return json!({"success": false, "error": e}).to_string(),
        },
    };
    let Some(project) = project else {
        return json!({"success": false, "error": "project vanished after create"}).to_string();
    };
    // A live session that moves owns its workspace, so it leaves the
    // profile-global pointer alone. Anything that can't move — CLI/messaging,
    // a pathless project — switches through the pointer instead.
    let primary = project.primary_path();
    if !moves_session(task_id, primary.as_deref(), callback) {
        store.set_active(&project.id);
    }
    activated(&project, task_id, callback)
}

/// Resolve a token: exact id / slug / name first, then case-insensitive slug /
/// name.
fn resolve(store: &std::sync::Arc<dyn ProjectsStore>, token: &str) -> Option<Project> {
    let token = token.trim();
    if token.is_empty() {
        return None;
    }
    let projects = store.list_projects(true);
    if let Some(found) = projects
        .iter()
        .find(|p| token == p.id || token == p.slug || token == p.name)
    {
        return Some(found.clone());
    }
    let low = token.to_lowercase();
    projects
        .iter()
        .find(|p| p.slug.to_lowercase() == low || p.name.to_lowercase() == low)
        .cloned()
}

fn switch(
    store: &std::sync::Arc<dyn ProjectsStore>,
    task_id: Option<&str>,
    callback: Option<&dyn WorkspaceCallback>,
    args: &Value,
) -> String {
    let token = args.get("name").and_then(Value::as_str).unwrap_or("");
    let Some(project) = resolve(store, token) else {
        return json!({"success": false, "error": format!("no project matching '{token}'")})
            .to_string();
    };
    // Same rule as create.
    let primary = project.primary_path();
    if !moves_session(task_id, primary.as_deref(), callback) {
        store.set_active(&project.id);
    }
    activated(&project, task_id, callback)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct MemStore {
        projects: Mutex<Vec<Project>>,
        active: Mutex<Option<String>>,
        next: Mutex<u64>,
    }

    impl MemStore {
        fn with(projects: Vec<Project>) -> Self {
            Self {
                projects: Mutex::new(projects),
                active: Mutex::new(None),
                next: Mutex::new(10),
            }
        }
    }

    fn proj(id: &str, slug: &str, name: &str, primary: Option<&str>) -> Project {
        Project {
            id: id.into(),
            slug: slug.into(),
            name: name.into(),
            folders: primary.map(|p| vec![p.to_string()]).unwrap_or_default(),
            primary_path: primary.map(|p| p.to_string()),
            archived: false,
        }
    }

    impl ProjectsStore for MemStore {
        fn list_projects(&self, include_archived: bool) -> Vec<Project> {
            self.projects
                .lock()
                .unwrap()
                .iter()
                .filter(|p| include_archived || !p.archived)
                .cloned()
                .collect()
        }
        fn create_project(
            &self,
            name: &str,
            folders: Vec<String>,
            primary_path: Option<String>,
        ) -> Result<String, String> {
            if name.trim().is_empty() {
                return Err("name is required".into());
            }
            let mut next = self.next.lock().unwrap();
            *next += 1;
            let id = format!("proj_{next:06}");
            self.projects.lock().unwrap().push(Project {
                id: id.clone(),
                slug: slugify(name),
                name: name.into(),
                folders,
                primary_path,
                archived: false,
            });
            Ok(id)
        }
        fn find_by_primary_path(&self, path: &str) -> Option<Project> {
            let wanted = normalize_path(path);
            self.projects
                .lock()
                .unwrap()
                .iter()
                .find(|p| {
                    p.primary_path()
                        .map_or(false, |pp| normalize_path(&pp) == wanted)
                })
                .cloned()
        }
        fn get_active_id(&self) -> Option<String> {
            self.active.lock().unwrap().clone()
        }
        fn set_active(&self, id: &str) {
            *self.active.lock().unwrap() = Some(id.into());
        }
    }

    #[test]
    fn action_must_be_valid() {
        let store = std::sync::Arc::new(MemStore::with(vec![]));
        let out = execute_with(store.clone(), None, None, &json!({"action": "hug"}));
        assert!(
            out.contains("action must be one of: create, switch, list"),
            "{out}"
        );
        let out = execute_with(store, None, None, &json!({}));
        assert!(out.contains("action must be one of"), "{out}");
    }

    #[test]
    fn create_requires_a_name() {
        let store = std::sync::Arc::new(MemStore::with(vec![]));
        let out = execute_with(store, None, None, &json!({"action": "create"}));
        assert!(out.contains(r#""error":"name is required""#), "{out}");
        assert!(out.contains(r#""success":false"#), "{out}");
    }

    #[test]
    fn create_switches_the_active_pointer() {
        let store = std::sync::Arc::new(MemStore::with(vec![]));
        let out = execute_with(
            store.clone(),
            None,
            None,
            &json!({"action": "create", "name": "Port", "path": "C:/repos/x"}),
        );
        assert!(out.contains(r#""success":true"#), "{out}");
        assert_eq!(store.get_active_id().is_some(), true);
        // Idempotent on the same primary path.
        let out = execute_with(
            store.clone(),
            None,
            None,
            &json!({"action": "create", "name": "Port", "path": "C:/repos/x"}),
        );
        assert!(out.contains(r#""success":true"#), "{out}");
        assert_eq!(store.list_projects(true).len(), 1);
    }

    #[test]
    fn live_session_with_a_path_leaves_the_pointer_alone() {
        let store = std::sync::Arc::new(MemStore::with(vec![]));
        let anchored = Mutex::new(Vec::<(String, String, String)>::new());
        let cb = |task: &str, path: &str, name: &str| {
            anchored
                .lock()
                .unwrap()
                .push((task.into(), path.into(), name.into()));
        };
        let out = execute_with(
            store.clone(),
            Some("task-1"),
            Some(&cb),
            &json!({"action": "create", "name": "Port", "path": "C:/repos/x"}),
        );
        assert!(out.contains(r#""primary_path":"C:/repos/x""#), "{out}");
        assert_eq!(
            store.get_active_id(),
            None,
            "a live session must not move the profile-global pointer"
        );
        assert_eq!(anchored.lock().unwrap()[0].0, "task-1");
    }

    #[test]
    fn switch_resolves_id_slug_name_case_insensitively() {
        let store = std::sync::Arc::new(MemStore::with(vec![proj(
            "p1",
            "docs",
            "Docs",
            Some("C:/d"),
        )]));
        for token in ["p1", "docs", "Docs", "DOCS"] {
            let out = execute_with(
                store.clone(),
                None,
                None,
                &json!({"action": "switch", "name": token}),
            );
            assert!(out.contains(r#""slug":"docs""#), "{token}: {out}");
        }
        let out = execute_with(
            store.clone(),
            None,
            None,
            &json!({"action": "switch", "name": "nope"}),
        );
        assert!(
            out.contains(r#""error":"no project matching 'nope'""#),
            "{out}"
        );
        let out = execute_with(store, None, None, &json!({"action": "switch"}));
        assert!(out.contains(r#""error":"no project matching ''""#), "{out}");
    }

    #[test]
    fn list_reports_active() {
        let store = std::sync::Arc::new(MemStore::with(vec![
            proj("p1", "docs", "Docs", Some("C:/d")),
            proj("p2", "code", "Code", None),
        ]));
        store.set_active("p2");
        let out = execute_with(store, None, None, &json!({"action": "list"}));
        assert!(out.contains(r#""active_id":"p2""#), "{out}");
        assert!(out.contains(r#""active":true"#), "{out}");
        assert!(out.contains(r#""active":false"#), "{out}");
    }

    #[test]
    fn json_store_round_trips() {
        let path = std::env::temp_dir().join(format!(
            "hermes-proj-test-{}-{}.json",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let store = JsonProjectsStore::at(path.clone());
        let id = store
            .create_project("My Repo", vec!["C:/r".into()], Some("C:/r".into()))
            .unwrap();
        assert!(id.starts_with("proj_"));
        let reloaded = JsonProjectsStore::at(path.clone());
        assert_eq!(reloaded.list_projects(true)[0].name, "My Repo");
        reloaded.set_active(&id);
        assert_eq!(
            JsonProjectsStore::at(path.clone()).get_active_id(),
            Some(id)
        );
        // Slug uniquing.
        let _ = store.create_project("My Repo", vec![], None);
        let slugs: Vec<String> = store
            .list_projects(true)
            .into_iter()
            .map(|p| p.slug)
            .collect();
        assert_eq!(slugs.len(), 2);
        assert_ne!(slugs[0], slugs[1]);
        let _ = std::fs::remove_file(path);
    }
}
