//! The plugin host (Rust port of the Python `PluginManager` + the dashboard
//! mount gates in `web_server_dashboard.py`): registration, discovery from a
//! plugin directory, the enable/disable consent gates, and lifecycle.
//!
//! Python loads third-party code by importing it; Rust plugins link in, so
//! "loading from a directory" here means: scan `root` for subdirectories with
//! a valid `manifest.json`, validate each, and match `name` against the
//! factories the host was given (a factory returns the built-in plugin that
//! owns that name — the analog of the Python trust gates: a plugin not built
//! into this binary is `source: user` and is only "mounted" if a factory
//! exists for it).

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::manifest::PluginManifest;
use crate::plugin::Plugin;

/// Builds the plugin that owns a manifest name. `None` = "installed but not
/// built in" — recorded, never mounted (Python's disabled-by-default user
/// plugin).
pub type PluginFactory = fn(Arc<crate::bus::EventBus>) -> Option<Arc<dyn Plugin>>;

/// One hosted plugin's public info (`GET /api/plugins` list item).
#[derive(Debug, Clone, serde::Serialize)]
pub struct HostedPluginInfo {
    pub name: String,
    pub label: String,
    pub description: String,
    pub version: String,
    pub enabled: bool,
}

struct Hosted {
    manifest: PluginManifest,
    plugin: Option<Arc<dyn Plugin>>,
    enabled: bool,
    started: bool,
}

/// Owns the set of plugins for one profile (Python: one `PluginManager` per
/// profile home) and the shared event bus their WebSockets fan out on.
pub struct PluginHost {
    bus: Arc<crate::bus::EventBus>,
    plugins: RwLock<Vec<Hosted>>,
}

impl Default for PluginHost {
    fn default() -> Self {
        Self::new()
    }
}

impl PluginHost {
    pub fn new() -> Self {
        Self {
            bus: Arc::new(crate::bus::EventBus::new()),
            plugins: RwLock::new(Vec::new()),
        }
    }

    /// The shared event bus. Plugins publish to and subscribe on it; the
    /// per-plugin `/events` WebSockets are its fanout surface.
    pub fn bus(&self) -> Arc<crate::bus::EventBus> {
        Arc::clone(&self.bus)
    }

    /// Register an already-built plugin (disabled by default — Python's
    /// consent gate: nothing mounts until `enable`).
    pub fn register(&self, plugin: Arc<dyn Plugin>) {
        let manifest = plugin.manifest().clone();
        let mut plugins = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        if plugins.iter().any(|h| h.manifest.name == manifest.name) {
            return; // one plugin per name, like the Python registry
        }
        plugins.push(Hosted {
            manifest,
            plugin: Some(plugin),
            enabled: false,
            started: false,
        });
    }

    /// Register the factory set of built-in plugins.
    pub fn register_factories(&self, factories: &HashMap<String, PluginFactory>) {
        for factory in factories.values() {
            if let Some(plugin) = factory(Arc::clone(&self.bus)) {
                self.register(plugin);
            }
        }
    }

    /// Scan `root` for plugin directories (`<root>/<name>/manifest.json`),
    /// validate each manifest, and mount the ones a factory knows. Returns
    /// `(mounted names, errors)` — an unreadable/invalid directory never
    /// aborts the sweep (Python `iter_plugin_dirs` skips one bad child).
    pub fn load_from_dir(
        &self,
        root: &Path,
        factories: &HashMap<String, PluginFactory>,
    ) -> (Vec<String>, Vec<String>) {
        let mut mounted = Vec::new();
        let mut errors = Vec::new();
        let Ok(entries) = std::fs::read_dir(root) else {
            return (mounted, errors);
        };
        let mut dirs: Vec<_> = entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        dirs.sort();
        for dir in dirs {
            // `.`/`_`-prefixed dirs are skipped by Python's discovery.
            let Some(name) = dir.file_name().and_then(|n| n.to_str()) else {
                continue;
            };
            if name.starts_with('.') || name.starts_with('_') {
                continue;
            }
            match PluginManifest::from_dir(&dir) {
                Ok(manifest) => {
                    if self.plugin(&manifest.name).is_some() {
                        errors.push(format!("{}: duplicate plugin name", manifest.name));
                        continue;
                    }
                    match factories.get(&manifest.name) {
                        Some(factory) => match factory(Arc::clone(&self.bus)) {
                            Some(plugin) => {
                                self.register(plugin);
                                mounted.push(manifest.name);
                            }
                            None => errors.push(format!(
                                "{}: no plugin built in for this name",
                                manifest.name
                            )),
                        },
                        None => errors.push(format!(
                            "{}: no plugin built in for this name",
                            manifest.name
                        )),
                    }
                }
                Err(e) => errors.push(format!("{}: {e}", dir.display())),
            }
        }
        (mounted, errors)
    }

    /// Enable + start plugin `name`. True when a state change happened.
    pub fn enable(&self, name: &str) -> bool {
        let mut plugins = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        let Some(hosted) = plugins.iter_mut().find(|h| h.manifest.name == name) else {
            return false;
        };
        if hosted.enabled {
            return false;
        }
        hosted.enabled = true;
        if !hosted.started {
            if let Some(plugin) = &hosted.plugin {
                plugin.start();
            }
            hosted.started = true;
        }
        true
    }

    /// Disable + stop plugin `name`. Mounted routes disappear on the next
    /// `router()` build — the Python server re-checks consent per request, the
    /// Rust gateway rebuilds its route table on change (see port_notes.md).
    pub fn disable(&self, name: &str) -> bool {
        let mut plugins = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        let Some(hosted) = plugins.iter_mut().find(|h| h.manifest.name == name) else {
            return false;
        };
        if !hosted.enabled {
            return false;
        }
        hosted.enabled = false;
        if hosted.started {
            if let Some(plugin) = &hosted.plugin {
                plugin.stop();
            }
            hosted.started = false;
        }
        true
    }

    /// Start every enabled plugin (Python: the startup sweep).
    pub fn start_all(&self) {
        let mut plugins = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        for hosted in plugins.iter_mut() {
            if hosted.enabled && !hosted.started {
                if let Some(plugin) = &hosted.plugin {
                    plugin.start();
                }
                hosted.started = true;
            }
        }
    }

    /// Stop everything (shutdown).
    pub fn stop_all(&self) {
        let mut plugins = self.plugins.write().unwrap_or_else(|e| e.into_inner());
        for hosted in plugins.iter_mut() {
            if hosted.started {
                if let Some(plugin) = &hosted.plugin {
                    plugin.stop();
                }
                hosted.started = false;
            }
        }
    }

    /// The listing `GET /api/plugins` serves.
    pub fn list(&self) -> Vec<HostedPluginInfo> {
        self.plugins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|h| HostedPluginInfo {
                name: h.manifest.name.clone(),
                label: h.manifest.label.clone(),
                description: h.manifest.description.clone(),
                version: h.manifest.version.clone(),
                enabled: h.enabled,
            })
            .collect()
    }

    /// The enabled plugins' routers + framework routes, as an axum `Router`.
    /// Rebuild (call again) after any enable/disable — see `routes`.
    pub fn router(self: Arc<Self>) -> axum::Router<()> {
        crate::routes::plugin_api_router(self)
    }

    /// The plugin with `name`, if registered (regardless of enabled state).
    pub fn plugin(&self, name: &str) -> Option<Arc<dyn Plugin>> {
        self.plugins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|h| h.manifest.name == name)
            .and_then(|h| h.plugin.clone())
    }

    /// Whether plugin `name` is currently enabled.
    pub fn is_enabled(&self, name: &str) -> bool {
        self.plugins
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .find(|h| h.manifest.name == name)
            .map(|h| h.enabled)
            .unwrap_or(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugins::example::ExamplePlugin;
    use crate::plugins::{achievements::AchievementsPlugin, kanban::KanbanPlugin};

    fn builtin_factories() -> HashMap<String, PluginFactory> {
        HashMap::from([
            (
                "kanban".to_string(),
                (|bus| Some(Arc::new(KanbanPlugin::new(bus)) as Arc<dyn Plugin>)) as PluginFactory,
            ),
            (
                "hermes-achievements".to_string(),
                (|bus| Some(Arc::new(AchievementsPlugin::new(bus)) as Arc<dyn Plugin>))
                    as PluginFactory,
            ),
            (
                "example".to_string(),
                (|bus| Some(Arc::new(ExamplePlugin::new(bus)) as Arc<dyn Plugin>)) as PluginFactory,
            ),
        ])
    }

    fn write_plugin_dir(root: &Path, dir_name: &str, name: &str) -> std::path::PathBuf {
        let dir = root.join(dir_name);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"name": "{name}", "version": "1.0.0", "api": "plugin_api.py"}}"#),
        )
        .unwrap();
        // The declared backend API file must exist inside the plugin dir
        // (Python's resolve-and-compare validation).
        std::fs::write(dir.join("plugin_api.py"), b"# plugin api\n").unwrap();
        dir
    }

    fn temp_root(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "hermes-plugins-test-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_plugins_from_a_fixture_dir() {
        let root = temp_root("load");
        write_plugin_dir(&root, "kanban", "kanban");
        write_plugin_dir(&root, "achievements", "hermes-achievements");
        write_plugin_dir(&root, "broken", "kanban"); // duplicate name → second factory hit is a no-op
        std::fs::create_dir_all(root.join("_private")).unwrap(); // skipped prefix
        std::fs::create_dir_all(root.join("not-a-plugin")).unwrap(); // no manifest

        let host = PluginHost::new();
        let (mounted, errors) = host.load_from_dir(&root, &builtin_factories());
        assert_eq!(
            mounted,
            vec!["hermes-achievements", "kanban"],
            "sorted; skipped dirs not mounted"
        );
        assert!(errors
            .iter()
            .any(|e| e.contains("not-a-plugin") || e.contains("broken")));

        host.enable("kanban");
        assert!(host.is_enabled("kanban"));
        assert!(!host.is_enabled("hermes-achievements"));
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn enable_disable_lifecycle() {
        let host = PluginHost::new();
        host.register_factories(&builtin_factories());

        assert!(!host.enable("no-such-plugin"));
        assert!(host.enable("kanban"));
        assert!(!host.enable("kanban"), "second enable is a no-op");
        assert!(host.is_enabled("kanban"));
        host.stop_all();
        assert!(
            !host.disable("hermes-achievements"),
            "disable of a disabled plugin is a no-op"
        );
        assert!(host.disable("kanban"));
        assert!(!host.is_enabled("kanban"));

        let list = host.list();
        assert_eq!(list.len(), 3);
        let kanban = list.iter().find(|p| p.name == "kanban").unwrap();
        assert_eq!(kanban.label, "Kanban");
        assert!(!kanban.enabled);
    }

    #[test]
    fn register_ignores_duplicate_names() {
        let host = PluginHost::new();
        let factory = builtin_factories().remove("kanban").unwrap();
        let bus = host.bus();
        host.register(factory(Arc::clone(&bus)).unwrap());
        host.register(factory(bus).unwrap());
        assert_eq!(host.list().len(), 1);
    }
}
