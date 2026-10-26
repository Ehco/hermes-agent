//! manage_connections — hosted connector accounts and local MCP servers
//! (port of `tools/connectors/tool.py` + `targets.py`).
//!
//! The target model, action routing and validation are ported faithfully;
//! the operation half is injected: `ConnectorClient` (the hosted gateway) and
//! `McpBackend` (the bundled catalog/OAuth runner). Bare registry execution has
//! neither, so it answers the faithful unavailable error — Python hides the
//! tool behind `connectors_available()` for the same reason.

use serde_json::Value;
use std::sync::RwLock;

pub const CONNECTOR_ACTIONS: &[&str] = &["display", "status", "connect", "reconnect", "rename"];
pub const MCP_ACTIONS: &[&str] = &["install", "enable", "authorize"];
pub const ALL_ACTIONS: &[&str] = &[
    "display",
    "status",
    "connect",
    "reconnect",
    "rename",
    "install",
    "enable",
    "authorize",
];
const TARGET_FIELDS: &[&str] = &["name", "mcp", "alias", "to"];
pub const ALIAS_PATTERN: &str = "^[a-z0-9][a-z0-9-]{0,31}$";
const ALIAS_FORMAT: &str = "1-32 lowercase letters, digits or '-', starting with a letter or digit";

/// A hosted connector target. `alias` names one account of that connector;
/// `to` is the new alias of a rename.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostedTarget {
    pub name: String,
    pub alias: Option<String>,
    pub to: Option<String>,
}

/// Python `normalize_targets`: `(["gmail"] | {"name": "gmail"} | {"name":
/// "linear", "mcp": true}) -> (hosted targets, MCP server names, error).
pub fn normalize_targets(raw: Option<&Value>) -> Result<(Vec<HostedTarget>, Vec<String>), String> {
    let Some(raw) = raw else {
        return Ok((vec![], vec![]));
    };
    let items: Vec<Value> = match raw {
        Value::String(_) | Value::Object(_) => vec![raw.clone()],
        Value::Array(items) => items.clone(),
        _ => return Err("'connectors' must be a list of names or {name, mcp} objects.".into()),
    };
    let mut managed: Vec<HostedTarget> = vec![];
    let mut mcp: Vec<String> = vec![];
    for item in &items {
        let (name, is_mcp, alias, to) = match item {
            Value::Object(obj) => {
                let unknown: Vec<String> = obj
                    .keys()
                    .filter(|k| !TARGET_FIELDS.contains(&k.as_str()))
                    .cloned()
                    .collect();
                if !unknown.is_empty() {
                    return Err(format!(
                        "unknown target field(s) {}: a target is {{\"name\": \"<slug>\"}} or \
                         {{\"name\": \"<server>\", \"mcp\": true}}. Transport, URLs and credentials \
                         come from the catalog manifest, never from the call.",
                        unknown.join(", ")
                    ));
                }
                let name = obj
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_lowercase();
                let is_mcp = obj.get("mcp").and_then(Value::as_bool).unwrap_or(false);
                if is_mcp && (obj.contains_key("alias") || obj.contains_key("to")) {
                    return Err(
                        "'alias' and 'to' name hosted connector accounts; an MCP target takes neither."
                            .into(),
                    );
                }
                let alias = obj
                    .get("alias")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string);
                let to = obj
                    .get("to")
                    .and_then(Value::as_str)
                    .map(str::trim)
                    .filter(|v| !v.is_empty())
                    .map(str::to_string);
                (name, is_mcp, alias, to)
            }
            Value::String(s) => (s.trim().to_lowercase(), false, None, None),
            _ => (item.to_string().trim().to_lowercase(), false, None, None),
        };
        if name.is_empty() {
            return Err("every target needs a non-empty 'name'.".into());
        }
        if is_mcp {
            if !mcp.contains(&name) {
                mcp.push(name);
            }
            continue;
        }
        let target = HostedTarget { name, alias, to };
        if !managed
            .iter()
            .any(|t| t.name == target.name && t.alias == target.alias)
        {
            managed.push(target);
        }
    }
    Ok((managed, mcp))
}

fn bad_alias(key: &str, value: Option<&str>) -> Option<String> {
    let Some(value) = value else {
        return None;
    };
    if is_valid_alias(value) {
        return None;
    }
    Some(format!(
        "'{key}' '{value}' is not a valid account name: use {ALIAS_FORMAT}."
    ))
}

/// Python `ALIAS_PATTERN` fullmatch: `^[a-z0-9][a-z0-9-]{0,31}$`.
pub fn is_valid_alias(alias: &str) -> bool {
    let mut chars = alias.chars();
    match (chars.next(), chars.count()) {
        (Some(c), rest) if c.is_ascii_lowercase() || c.is_ascii_digit() => {
            rest <= 31
                && alias
                    .chars()
                    .skip(1)
                    .all(|c| c == '-' || c.is_ascii_lowercase() || c.is_ascii_digit())
        }
        _ => false,
    }
}

/// Python `validate_action`.
pub fn validate_action(
    action: &str,
    managed: &[HostedTarget],
    mcp: &[String],
) -> Result<(), String> {
    if !ALL_ACTIONS.contains(&action) {
        return Err(format!(
            "action must be one of {}. {}, {} apply to local MCP servers (targets \
             {{\"name\": ..., \"mcp\": true}}); the rest apply to managed connectors. Disconnecting \
             an account is done by the user in the Nous Portal dashboard, not through this tool.",
            ALL_ACTIONS.join(", "),
            CONNECTOR_ACTIONS.join(", "),
            MCP_ACTIONS.join(", "),
        ));
    }
    if MCP_ACTIONS.contains(&action) {
        if !managed.is_empty() {
            return Err(format!(
                "'{action}' is an MCP action: every target must carry \"mcp\": true (got managed \
                 connector(s) {}). Managed connectors use connect / reconnect / status.",
                managed
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if mcp.is_empty() {
            return Err(format!(
                "'{action}' requires 'connectors': the MCP server name(s), e.g. \
                 [{{\"name\": \"linear\", \"mcp\": true}}]."
            ));
        }
    } else if !mcp.is_empty() {
        return Err(format!(
            "'{action}' is a managed-connector action; MCP targets ({}) use {}, {}.",
            mcp.join(", "),
            MCP_ACTIONS[0],
            MCP_ACTIONS[1..].join(", ")
        ));
    }
    validate_accounts(action, managed)
}

fn validate_accounts(action: &str, managed: &[HostedTarget]) -> Result<(), String> {
    if action != "rename" && managed.iter().any(|t| t.to.is_some()) {
        return Err("'to' is the new name of an account and applies to action rename only.".into());
    }
    if action == "display" && managed.iter().any(|t| t.alias.is_some()) {
        return Err(
            "display takes connector slugs to suggest (e.g. [\"gmail\"]); accounts are named on connect."
                .into(),
        );
    }
    if action == "rename" {
        if managed.len() != 1 {
            return Err(
                "rename takes exactly one target {\"name\": \"<slug>\", \"alias\": \"<current name>\", \
                 \"to\": \"<new name>\"}. Use action status to list each connector's accounts."
                    .into(),
            );
        }
        let target = &managed[0];
        if target.alias.is_none() || target.to.is_none() {
            return Err(
                "rename takes exactly one target {\"name\": \"<slug>\", \"alias\": \"<current name>\", \
                 \"to\": \"<new name>\"}. Use action status to list each connector's accounts."
                    .into(),
            );
        }
        // The current name may be the vendor's label of an unnamed account, so
        // only the new one is checked.
        return bad_alias("to", target.to.as_deref())
            .map(Err)
            .unwrap_or(Ok(()));
    }
    if managed.iter().any(|t| t.alias.is_some()) && managed.len() > 1 {
        return Err(
            "a call with an 'alias' takes that one target only; name other accounts in separate calls."
                .into(),
        );
    }
    if action == "reconnect" {
        // Like rename, a repair may address an unnamed account by its label;
        // the account read checks it.
        return Ok(());
    }
    for target in managed {
        if let Some(e) = bad_alias("alias", target.alias.as_deref()) {
            return Err(e);
        }
    }
    Ok(())
}

/// The hosted connector gateway seam (Python `ConnectorClient`).
pub trait ConnectorClient: Send + Sync {
    fn run(
        &self,
        action: &str,
        targets: &[HostedTarget],
        force: bool,
        args: &Value,
    ) -> Result<Value, String>;
}

/// The local MCP catalog/OAuth seam (Python `run_mcp_operation`).
pub trait McpBackend: Send + Sync {
    fn run(&self, action: &str, targets: &[String], args: &Value) -> Result<Value, String>;
}

static CLIENT: RwLock<Option<std::sync::Arc<dyn ConnectorClient>>> = RwLock::new(None);
static MCP: RwLock<Option<std::sync::Arc<dyn McpBackend>>> = RwLock::new(None);

/// Install the operation seams (hosted hosts, tests).
pub fn set_backends(
    client: Option<std::sync::Arc<dyn ConnectorClient>>,
    mcp: Option<std::sync::Arc<dyn McpBackend>>,
) {
    *CLIENT.write().unwrap() = client;
    *MCP.write().unwrap() = mcp;
}

/// Execute `manage_connections` — the registry entry point (no backends).
pub fn execute(args: &Value) -> String {
    execute_with(
        CLIENT.read().unwrap().clone(),
        MCP.read().unwrap().clone(),
        args,
    )
}

/// `execute` against injected backends (hosts, tests).
pub fn execute_with(
    client: Option<std::sync::Arc<dyn ConnectorClient>>,
    mcp_backend: Option<std::sync::Arc<dyn McpBackend>>,
    args: &Value,
) -> String {
    let action = args
        .get("action")
        .and_then(Value::as_str)
        .map(str::trim)
        .unwrap_or("status")
        .to_lowercase();
    let (managed, mcp) = match normalize_targets(args.get("connectors")) {
        Ok(pair) => pair,
        Err(e) => return crate::util::tool_error(e),
    };
    if let Err(e) = validate_action(&action, &managed, &mcp) {
        return crate::util::tool_error(e);
    }
    if MCP_ACTIONS.contains(&action.as_str()) {
        return match mcp_backend {
            Some(backend) => match backend.run(&action, &mcp, args) {
                Ok(result) => result.to_string(),
                Err(e) => crate::util::tool_error(e),
            },
            None => tool_unavailable(),
        };
    }
    match client {
        Some(client) => {
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            match client.run(&action, &managed, force, args) {
                Ok(result) => result.to_string(),
                Err(e) => crate::util::tool_error(e),
            }
        }
        None => tool_unavailable(),
    }
}

fn tool_unavailable() -> String {
    crate::util::tool_error(
        "manage_connections needs the hosted connector gateway (or MCP catalog), which is not \
         available in this execution context. Configure connectors for this host, or direct the \
         user to the Nous Portal's Connectors page.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn normalize_targets_shapes() {
        let (managed, mcp) = normalize_targets(None).unwrap();
        assert!(managed.is_empty() && mcp.is_empty());
        let (managed, mcp) = normalize_targets(Some(&json!("gmail"))).unwrap();
        assert_eq!(
            managed,
            vec![HostedTarget {
                name: "gmail".into(),
                alias: None,
                to: None
            }]
        );
        assert!(mcp.is_empty());
        let (managed, mcp) = normalize_targets(Some(&json!([
            "gmail",
            {"name": "Gmail", "alias": "work"},
            {"name": "linear", "mcp": true},
            {"name": "linear", "mcp": true},
        ])))
        .unwrap();
        assert_eq!(managed.len(), 2, "dedupe by (name, alias)");
        assert_eq!(managed[1].alias.as_deref(), Some("work"));
        assert_eq!(mcp, vec!["linear"]);
        // Error shapes.
        for (args, expected) in [
            (json!(42), "must be a list"),
            (json!([{}]), "every target needs a non-empty 'name'"),
            (
                json!([{"name": "x", "port": 1}]),
                "unknown target field(s) port",
            ),
            (
                json!([{"name": "l", "mcp": true, "alias": "a"}]),
                "takes neither",
            ),
        ] {
            let err = normalize_targets(Some(&args)).unwrap_err();
            assert!(err.contains(expected), "{args}: {err}");
        }
    }

    #[test]
    fn alias_validation() {
        assert!(is_valid_alias("work"));
        assert!(is_valid_alias("a-1"));
        assert!(!is_valid_alias("Work"));
        assert!(!is_valid_alias("-work"));
        assert!(!is_valid_alias("a".repeat(33).as_str()));
    }

    #[test]
    fn validate_action_matrix() {
        let managed_one = vec![HostedTarget {
            name: "gmail".into(),
            alias: Some("work".into()),
            to: None,
        }];
        let err = validate_action("install", &managed_one, &[]).unwrap_err();
        assert!(err.contains("every target must carry"), "{err}");
        let err = validate_action("install", &[], &[]).unwrap_err();
        assert!(err.contains("requires 'connectors'"), "{err}");
        let err = validate_action("status", &[], &["linear".into()]).unwrap_err();
        assert!(err.contains("managed-connector action"), "{err}");
        let err = validate_action("hug", &[], &[]).unwrap_err();
        assert!(err.starts_with("action must be one of"), "{err}");
        assert!(validate_action("connect", &managed_one, &[]).is_ok());
    }

    #[test]
    fn rename_and_to_rules() {
        let rename = vec![HostedTarget {
            name: "gmail".into(),
            alias: Some("work".into()),
            to: Some("job".into()),
        }];
        assert!(validate_action("rename", &rename, &[]).is_ok());
        // 'to' applies to rename only (checked before the display/alias rules,
        // matching connectors/targets.py `_validate_accounts`).
        let err = validate_action("connect", &rename, &[]).unwrap_err();
        assert!(err.contains("'to' is the new name"), "{err}");
        let err = validate_action("display", &rename, &[]).unwrap_err();
        assert!(err.contains("'to' is the new name"), "{err}");
        // display rejects aliases.
        let aliased = vec![HostedTarget {
            name: "gmail".into(),
            alias: Some("work".into()),
            to: None,
        }];
        let err = validate_action("display", &aliased, &[]).unwrap_err();
        assert!(err.contains("accounts are named on connect"), "{err}");
        // Bad new name.
        let bad = vec![HostedTarget {
            name: "gmail".into(),
            alias: Some("work".into()),
            to: Some("Job".into()),
        }];
        let err = validate_action("rename", &bad, &[]).unwrap_err();
        assert!(err.contains("is not a valid account name"), "{err}");
    }

    #[test]
    fn without_backends_is_unavailable_after_validation() {
        let _g = guard();
        set_backends(None, None);
        // Validation errors come first (no backends needed).
        let out = execute(&json!({"action": "install"}));
        assert!(out.contains("requires 'connectors'"), "{out}");
        // Valid calls surface the unavailable error.
        let out = execute(&json!({"action": "status"}));
        assert!(
            out.contains("not available in this execution context"),
            "{out}"
        );
        let out = execute(&json!({"action": "connect", "connectors": ["gmail"]}));
        assert!(
            out.contains("not available in this execution context"),
            "{out}"
        );
    }

    #[test]
    fn backends_receive_the_routed_operation() {
        let _g = guard();
        struct StubClient {
            calls: Mutex<Vec<(String, Vec<String>, bool)>>,
        }
        impl ConnectorClient for StubClient {
            fn run(
                &self,
                action: &str,
                targets: &[HostedTarget],
                force: bool,
                _args: &Value,
            ) -> Result<Value, String> {
                self.calls.lock().unwrap().push((
                    action.into(),
                    targets.iter().map(|t| t.name.clone()).collect(),
                    force,
                ));
                Ok(json!({"connected": ["gmail"]}))
            }
        }
        struct StubMcp {
            calls: Mutex<Vec<(String, Vec<String>)>>,
        }
        impl McpBackend for StubMcp {
            fn run(
                &self,
                action: &str,
                targets: &[String],
                _args: &Value,
            ) -> Result<Value, String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push((action.into(), targets.to_vec()));
                Ok(json!({"installed": ["linear"]}))
            }
        }
        let client = std::sync::Arc::new(StubClient {
            calls: Mutex::new(vec![]),
        });
        let mcp = std::sync::Arc::new(StubMcp {
            calls: Mutex::new(vec![]),
        });
        set_backends(Some(client.clone()), Some(mcp.clone()));
        let out = execute(&json!({"action": "connect", "connectors": ["gmail"], "force": true}));
        assert!(out.contains(r#""connected":["gmail"]"#), "{out}");
        assert_eq!(client.calls.lock().unwrap()[0].0, "connect");
        assert_eq!(client.calls.lock().unwrap()[0].1, vec!["gmail"]);
        assert_eq!(client.calls.lock().unwrap()[0].2, true, "force rides along");
        let out =
            execute(&json!({"action": "install", "connectors": [{"name": "linear", "mcp": true}]}));
        assert!(out.contains(r#""installed":["linear"]"#), "{out}");
        assert_eq!(mcp.calls.lock().unwrap()[0].0, "install");
        set_backends(None, None);
    }
}
