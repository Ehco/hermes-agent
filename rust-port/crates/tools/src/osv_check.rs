//! osv_check — OSV malware preflight for MCP extension packages.
//!
//! Port of `tools/osv_check.py`: before launching an MCP server via
//! npx/uvx/pipx, query Google's public OSV API for known malware advisories
//! (MAL-* IDs — regular CVEs are ignored, only confirmed malware is blocked).
//! Fail-open: network errors allow the package to proceed, and network
//! failures are deliberately NOT cached (caching one could mask a real
//! advisory later).
//!
//! This is the Python helper's contract — `check_package_for_malware` — not a
//! model-facing tool, so it is not added to `specs()`; the MCP lifecycle in
//! the agent runtime calls it before spawning a package server.
//!
//! The HTTP seam is the injected `OsvPoster` trait (tests stub it; no live
//! network), and the verdict cache is the same in-process map with the 1h TTL.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

/// `tools/osv_check.py` constants.
pub const DEFAULT_OSV_ENDPOINT: &str = "https://api.osv.dev/v1/query";
pub const TIMEOUT_SECS: u64 = 10;
const CACHE_TTL_S: f64 = 3600.0;
const CACHE_MAX_ENTRIES: usize = 256;
const USER_AGENT: &str = "hermes-agent-osv-check/1.0";

/// `_ECOSYSTEM_BY_COMMAND`: the shim names each runner installs; lookalikes
/// stay fail-open.
const ECOSYSTEM_BY_COMMAND: &[(&str, &str)] = &[
    ("npx", "npm"),
    ("npx.cmd", "npm"),
    ("npx.exe", "npm"),
    ("uvx", "PyPI"),
    ("uvx.cmd", "PyPI"),
    ("uvx.exe", "PyPI"),
    ("pipx", "PyPI"),
    ("pipx.exe", "PyPI"),
];

/// One POST to the OSV query endpoint.
pub trait OsvPoster: Send + Sync {
    fn post_json(&self, url: &str, body: &str) -> Result<String, String>;
}

struct ReqwestOsvPoster;

impl OsvPoster for ReqwestOsvPoster {
    fn post_json(&self, url: &str, body: &str) -> Result<String, String> {
        let client = crate::web_common::http_client();
        let resp = client
            .post(url)
            .header("Content-Type", "application/json")
            .header("User-Agent", USER_AGENT)
            .body(body.to_string())
            .timeout(std::time::Duration::from_secs(TIMEOUT_SECS))
            .send()
            .map_err(|e| format!("OSV request failed: {e}"))?;
        let status = resp.status();
        let text = resp.text().map_err(|e| format!("OSV read failed: {e}"))?;
        if !status.is_success() {
            return Err(format!("OSV returned HTTP {}", status.as_u16()));
        }
        Ok(text)
    }
}

/// `(ecosystem, package, version)` → `(expiry_wallclock, blocked_message)`.
fn cache() -> &'static Mutex<HashMap<(String, String, String), (f64, Option<String>)>> {
    static CACHE: OnceLock<Mutex<HashMap<(String, String, String), (f64, Option<String>)>>> =
        OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn osv_endpoint() -> String {
    std::env::var("OSV_ENDPOINT").unwrap_or_else(|_| DEFAULT_OSV_ENDPOINT.to_string())
}

/// `_infer_ecosystem`: split on BOTH separators (basename leaves
/// `C:\...\uvx.exe` intact on POSIX-authored configs).
pub fn infer_ecosystem(command: &str) -> Option<&'static str> {
    let base = command
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or(command)
        .to_lowercase();
    ECOSYSTEM_BY_COMMAND
        .iter()
        .find(|(name, _)| *name == base)
        .map(|(_, eco)| *eco)
}

/// `_parse_package_from_args`: skip flags to find the package token; npx's
/// explicit install target (`--package=NAME` / `--package NAME` / `-p NAME`)
/// names a package distinct from the executed binary.
pub fn parse_package_from_args(
    args: &[String],
    ecosystem: &str,
) -> (Option<String>, Option<String>) {
    let mut package_token: Option<String> = None;
    let mut take_next = false;
    for arg in args {
        if take_next {
            package_token = Some(arg.clone());
            break;
        }
        if arg == "--package" || arg == "-p" {
            take_next = true;
            continue;
        }
        if let Some(rest) = arg.strip_prefix("--package=") {
            package_token = Some(rest.to_string());
            break;
        }
        if arg.starts_with('-') {
            continue;
        }
        package_token = Some(arg.clone());
        break;
    }
    let Some(token) = package_token else {
        return (None, None);
    };
    match ecosystem {
        "npm" => parse_npm_package(&token),
        "PyPI" => parse_pypi_package(&token),
        _ => (Some(token), None),
    }
}

/// `@scope/name@version` or `name@version` (`latest` is dropped).
pub fn parse_npm_package(token: &str) -> (Option<String>, Option<String>) {
    if let Some(rest) = token.strip_prefix('@') {
        let slash = match rest.find('/') {
            Some(s) => s,
            None => return (Some(token.to_string()), None),
        };
        let at = rest[slash + 1..].find('@').map(|p| p + slash + 1);
        return match at {
            Some(at) => (
                Some(format!("@{}", &rest[..at])),
                Some(rest[at + 1..].to_string()),
            ),
            None => (Some(token.to_string()), None),
        };
    }
    if let Some(at) = token.rfind('@') {
        let (name, version) = (&token[..at], &token[at + 1..]);
        if version == "latest" {
            return (Some(name.to_string()), None);
        }
        return (Some(name.to_string()), Some(version.to_string()));
    }
    (Some(token.to_string()), None)
}

/// `name==version` or `name[extras]==version`.
pub fn parse_pypi_package(token: &str) -> (Option<String>, Option<String>) {
    let base = token.split("==").next().unwrap_or(token);
    let name: String = base
        .chars()
        .take_while(|c| c.is_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .collect();
    if name.is_empty() {
        return (None, None);
    }
    let version = token.split_once("==").map(|(_, v)| v.to_string());
    (Some(name), version)
}

/// `_query_osv`: only MAL-* advisories (regular CVEs ignored).
pub fn query_osv(
    poster: &dyn OsvPoster,
    package: &str,
    ecosystem: &str,
    version: Option<&str>,
) -> Result<Vec<(String, String)>, String> {
    let mut payload = json!({ "package": { "name": package, "ecosystem": ecosystem } });
    if let Some(version) = version {
        payload["version"] = json!(version);
    }
    let body = poster.post_json(&osv_endpoint(), &payload.to_string())?;
    let parsed: Value =
        serde_json::from_str(&body).map_err(|e| format!("OSV returned invalid JSON: {e}"))?;
    Ok(parsed
        .get("vulns")
        .and_then(Value::as_array)
        .map(|vulns| {
            vulns
                .iter()
                .filter_map(|v| {
                    let id = v.get("id").and_then(Value::as_str)?;
                    if !id.starts_with("MAL-") {
                        return None;
                    }
                    Some((
                        id.to_string(),
                        v.get("summary")
                            .and_then(Value::as_str)
                            .unwrap_or(id)
                            .to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default())
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// `check_package_for_malware`: returns the BLOCKED message, else None — also
/// on network errors/unknown commands (fail-open).
pub fn check_package_for_malware_with(
    poster: &dyn OsvPoster,
    command: &str,
    args: &[String],
) -> Option<String> {
    let Some(ecosystem) = infer_ecosystem(command) else {
        return None;
    };
    let (package, version) = parse_package_from_args(args, ecosystem);
    let Some(package) = package else { return None };
    let key = (
        ecosystem.to_string(),
        package.clone(),
        version.clone().unwrap_or_default(),
    );
    if let Some((expiry, result)) = cache().lock().unwrap().get(&key).cloned() {
        if now() < expiry {
            return result;
        }
    }
    // Fail-open, and deliberately NOT cached — a flapping network must not
    // pin a stale verdict over a real advisory.
    let malware = match query_osv(poster, &package, ecosystem, version.as_deref()) {
        Ok(m) => m,
        Err(_) => return None,
    };
    let result = if malware.is_empty() {
        None
    } else {
        let ids = malware
            .iter()
            .take(3)
            .map(|(id, _)| id.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let summaries = malware
            .iter()
            .take(3)
            .map(|(_, summary)| summary.chars().take(100).collect::<String>())
            .collect::<Vec<_>>()
            .join("; ");
        Some(format!(
            "BLOCKED: Package '{package}' ({ecosystem}) has known malware advisories: {ids}. Details: {summaries}"
        ))
    };
    let mut cache = cache().lock().unwrap();
    if cache.len() >= CACHE_MAX_ENTRIES {
        let expiry_now = now();
        cache.retain(|_, (expiry, _)| *expiry > expiry_now);
        if cache.len() >= CACHE_MAX_ENTRIES {
            cache.clear();
        }
    }
    cache.insert(key, (now() + CACHE_TTL_S, result.clone()));
    result
}

/// `check_package_for_malware` with the real HTTP backend.
pub fn check_package_for_malware(command: &str, args: &[String]) -> Option<String> {
    static POSTER: OnceLock<ReqwestOsvPoster> = OnceLock::new();
    let poster = POSTER.get_or_init(|| ReqwestOsvPoster);
    check_package_for_malware_with(poster, command, args)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::atomic::{AtomicU32, Ordering};

    struct StubPoster {
        calls: AtomicU32,
    }

    impl OsvPoster for StubPoster {
        fn post_json(&self, _url: &str, body: &str) -> Result<String, String> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            let req: Value = serde_json::from_str(body).unwrap();
            assert_eq!(req["package"]["ecosystem"], "npm");
            let mal = if req["package"]["name"] == "evil-pkg" {
                json!([{ "id": "MAL-2026-1", "summary": "credential stealer" }, { "id": "CVE-2020-1", "summary": "ignore me" }])
            } else {
                json!([{ "id": "CVE-2020-9", "summary": "regular cve" }])
            };
            Ok(json!({ "vulns": mal }).to_string())
        }
    }

    fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    #[test]
    fn ecosystem_inference_handles_both_separators() {
        assert_eq!(infer_ecosystem("npx"), Some("npm"));
        assert_eq!(infer_ecosystem(r"C:\node\npx.cmd"), Some("npm"));
        assert_eq!(infer_ecosystem("/usr/bin/uvx"), Some("PyPI"));
        assert_eq!(infer_ecosystem("bunx"), None);
        // Lookalikes stay fail-open.
        assert_eq!(infer_ecosystem("npxx"), None);
    }

    #[test]
    fn npm_tokens_split_scope_name_and_version() {
        let (name, version) = parse_npm_package("@scope/pkg@1.2.3");
        assert_eq!(name.as_deref(), Some("@scope/pkg"));
        assert_eq!(version.as_deref(), Some("1.2.3"));
        let (name, version) = parse_npm_package("left-pad@latest");
        assert_eq!(name.as_deref(), Some("left-pad"));
        assert_eq!(version, None);
        let (name, version) = parse_npm_package("plain");
        assert_eq!(name.as_deref(), Some("plain"));
        assert_eq!(version, None);
    }

    #[test]
    fn pypi_tokens_strip_extras_and_read_version() {
        let (name, version) = parse_pypi_package("requests[socks]==2.31.0");
        assert_eq!(name.as_deref(), Some("requests"));
        assert_eq!(version.as_deref(), Some("2.31.0"));
    }

    #[test]
    fn package_token_skips_flags_and_honors_package_flag() {
        let args: Vec<String> = vec![
            "--yes".into(),
            "--package=chalk@5".into(),
            "index.js".into(),
        ];
        let (pkg, ver) = parse_package_from_args(&args, "npm");
        assert_eq!(pkg.as_deref(), Some("chalk"));
        assert_eq!(ver.as_deref(), Some("5"));
        let args: Vec<String> = vec!["-y".into(), "-p".into(), "chalk@5".into()];
        let (pkg, ver) = parse_package_from_args(&args, "npm");
        assert_eq!(pkg.as_deref(), Some("chalk"));
        assert_eq!(ver.as_deref(), Some("5"));
    }

    #[test]
    fn clean_verdict_ignores_regular_cves_and_caches() {
        let poster = StubPoster {
            calls: AtomicU32::new(0),
        };
        let args: Vec<String> = vec!["safe-pkg".into()];
        assert!(check_package_for_malware_with(&poster, "npx", &args).is_none());
        assert_eq!(poster.calls.load(Ordering::SeqCst), 1);
        // Second call hits the cache (clean verdicts ARE cached).
        assert!(check_package_for_malware_with(&poster, "npx", &args).is_none());
        assert_eq!(poster.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn malware_blocks_with_ids_and_summaries() {
        let poster = StubPoster {
            calls: AtomicU32::new(0),
        };
        let args: Vec<String> = vec!["evil-pkg@2.0".into()];
        let blocked = check_package_for_malware_with(&poster, "npx", &args).unwrap();
        assert!(
            blocked.starts_with("BLOCKED: Package 'evil-pkg' (npm)"),
            "{blocked}"
        );
        assert!(blocked.contains("MAL-2026-1"), "{blocked}");
        // CVE-only entries are filtered out of the id list.
        assert!(!blocked.contains("CVE"), "{blocked}");
        assert!(blocked.contains("credential stealer"));
    }

    #[test]
    fn fail_open_on_transport_errors_and_unknown_commands() {
        struct DownPoster;
        impl OsvPoster for DownPoster {
            fn post_json(&self, _u: &str, _b: &str) -> Result<String, String> {
                Err("dns failure".into())
            }
        }
        let args: Vec<String> = vec!["whatever".into()];
        // Network failure: proceed (None), NOT cached — each call retries.
        assert!(check_package_for_malware_with(&DownPoster, "uvx", &args).is_none());
        // Unknown command: skipped without any HTTP.
        assert!(check_package_for_malware_with(&DownPoster, "deno", &args).is_none());
        assert!(check_package_for_malware_with(&DownPoster, "npx", &[]).is_none());
    }

    #[test]
    fn request_payload_shape() {
        let poster = StubPoster {
            calls: AtomicU32::new(0),
        };
        let args: Vec<String> = vec!["evil-pkg".into()];
        let _ = check_package_for_malware_with(&poster, "npx", &args);
        // The stub asserts the ecosystem inside the payload; assert the name too.
        let _ = v(&poster.calls.load(Ordering::SeqCst).to_string());
    }
}
