//! `manifest.json` — the plugin manifest (port of the Python dashboard-plugin
//! discovery: `plugins/kanban/dashboard/manifest.json` and friends) plus the
//! validation rules the Python side enforces before mounting.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// The name shape `hermes plugins install` accepts (Python
/// `plugin_storage._NAME_RE`): no separators, no traversal, bounded length.
pub const NAME_PATTERN: &str = r"^[a-zA-Z0-9][a-zA-Z0-9._-]{0,63}$";

/// Where a plugin's dashboard tab sits in the desktop sidebar.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct TabSpec {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub path: String,
    /// e.g. `after:skills` (informational for the desktop; the framework only
    /// carries it through).
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub position: String,
}

/// Parsed `manifest.json` of one plugin directory.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginManifest {
    pub name: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub icon: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub tab: Option<TabSpec>,
    /// Frontend entry (`dist/index.js`) — carried through for the desktop.
    #[serde(default)]
    pub entry: String,
    #[serde(default)]
    pub css: String,
    /// Backend API file (`plugin_api.py` in Python; here the presence of an
    /// API is what makes the framework mount REST/WS routes).
    #[serde(default)]
    pub api: String,
}

impl PluginManifest {
    /// Parse and validate `manifest.json` bytes from `plugin_dir`.
    ///
    /// Validation (mirrors the Python gates):
    /// - `name` is required, matches [`Self::name_is_valid`], and has no `..`.
    /// - `api`, when set, must name a file directly inside `plugin_dir`
    ///   (no traversal — the Python code resolves-and-compares the path).
    pub fn from_bytes(plugin_dir: &Path, bytes: &[u8]) -> Result<Self, String> {
        let manifest: PluginManifest =
            serde_json::from_slice(bytes).map_err(|e| format!("invalid manifest.json: {e}"))?;
        manifest.validate(plugin_dir)?;
        Ok(manifest)
    }

    /// Read `manifest.json` out of `plugin_dir`.
    pub fn from_dir(plugin_dir: &Path) -> Result<Self, String> {
        let path = plugin_dir.join("manifest.json");
        let bytes =
            std::fs::read(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
        Self::from_bytes(plugin_dir, &bytes)
    }

    pub fn validate(&self, plugin_dir: &Path) -> Result<(), String> {
        if !Self::name_is_valid(&self.name) {
            return Err(format!("invalid plugin name: {:?}", self.name));
        }
        if let Some(api) = self.api_file() {
            let api_path = plugin_dir.join(api);
            let resolved = api_path
                .canonicalize()
                .map_err(|e| format!("api file not found: {} ({e})", api_path.display()))?;
            let dir = plugin_dir
                .canonicalize()
                .map_err(|e| format!("cannot resolve plugin dir: {e}"))?;
            if !resolved.starts_with(&dir) {
                return Err(format!(
                    "api file escapes the plugin dir: {}",
                    resolved.display()
                ));
            }
        }
        Ok(())
    }

    /// `Some(relative_api_path)` when the manifest declares a backend API.
    pub fn api_file(&self) -> Option<&str> {
        let api = self.api.trim();
        (!api.is_empty()).then_some(api)
    }

    /// The Python name rule, spelled as code so tests do not need a regex
    /// crate: first char alphanumeric, then `[a-zA-Z0-9._-]`, at most 64
    /// chars total, and never `..`.
    pub fn name_is_valid(name: &str) -> bool {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > 64 {
            return false;
        }
        if !bytes[0].is_ascii_alphanumeric() {
            return false;
        }
        if !bytes[1..]
            .iter()
            .all(|&b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
        {
            return false;
        }
        name != ".." && !name.contains("..")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn name_rules_mirror_python() {
        assert!(PluginManifest::name_is_valid("kanban"));
        assert!(PluginManifest::name_is_valid("hermes-achievements"));
        assert!(PluginManifest::name_is_valid("a1._-"));
        assert!(!PluginManifest::name_is_valid(""));
        assert!(!PluginManifest::name_is_valid(".."));
        assert!(!PluginManifest::name_is_valid("a..b"));
        assert!(!PluginManifest::name_is_valid(".hidden"));
        assert!(!PluginManifest::name_is_valid("-dash")); // first char not alphanumeric
        assert!(!PluginManifest::name_is_valid("has/slash"));
        assert!(!PluginManifest::name_is_valid(&"x".repeat(65)));
        assert!(PluginManifest::name_is_valid(&"x".repeat(64)));
    }

    #[test]
    fn parses_the_bundled_kanban_manifest_shape() {
        let bytes = br#"{
            "name": "kanban", "label": "Kanban", "description": "board",
            "icon": "Package", "version": "1.0.0",
            "tab": { "path": "/kanban", "position": "after:skills" },
            "entry": "dist/index.js", "css": "dist/style.css",
            "api": "plugin_api.py"
        }"#;
        // The api-file validation resolves `api` inside the plugin dir, so the
        // fixture gets a real (empty) file.
        let dir = std::env::temp_dir().join(format!(
            "hermes-plugins-manifest-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin_api.py"), b"# plugin api\n").unwrap();
        let m = PluginManifest::from_bytes(&dir, bytes).unwrap();

        assert_eq!(m.name, "kanban");
        assert_eq!(m.api_file(), Some("plugin_api.py"));
        assert_eq!(m.tab.as_ref().unwrap().position, "after:skills");
    }

    #[test]
    fn missing_name_is_an_error() {
        let bytes = br#"{"version": "1.0.0"}"#;
        let err = PluginManifest::from_bytes(Path::new("."), bytes).unwrap_err();
        assert!(err.contains("invalid manifest.json"), "{err}");
    }
}
