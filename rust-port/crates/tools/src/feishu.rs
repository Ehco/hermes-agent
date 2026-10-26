//! feishu_doc_read + the feishu_drive comment tools (port of
//! `tools/feishu_doc_tool.py` and `tools/feishu_drive_tool.py`).
//!
//! The Python tools share a lazily-built lark-oapi client that the Feishu
//! comment event handler injects per-thread (`feishu_lark.get_client`); without
//! that context they return the faithful "Feishu client not available" errors.
//! The Rust port keeps the URI templates, query/body shapes, validation and
//! error text verbatim, with the client behind the injected `LarkClient` seam.

use serde_json::{json, Value};

use crate::util::tool_error;

/// The lark client (Python `feishu_lark.get_client`'s BaseRequest path).
/// Returns `(code, msg, data)` like `lark_call`; a transport failure is `Err`.
pub trait LarkClient: Send + Sync {
    fn request(
        &self,
        method: &str,
        path: &str,
        query: &[(String, String)],
        body: Option<&Value>,
    ) -> Result<(i64, String, Value), String>;
}

static CLIENT: std::sync::RwLock<Option<std::sync::Arc<dyn LarkClient>>> =
    std::sync::RwLock::new(None);

/// Install (or with None, clear) the injected Feishu client.
pub fn set_client(client: Option<std::sync::Arc<dyn LarkClient>>) {
    *CLIENT.write().unwrap() = client;
}

fn get_client() -> Option<std::sync::Arc<dyn LarkClient>> {
    CLIENT.read().unwrap().clone()
}

const COMMENTS_URI: &str = "/open-apis/drive/v1/files/:file_token/comments";
const REPLIES_URI: &str = "/open-apis/drive/v1/files/:file_token/comments/:comment_id/replies";
const ADD_COMMENT_URI: &str = "/open-apis/drive/v1/files/:file_token/new_comments";

fn arg_str(args: &Value, key: &str) -> String {
    args.get(key)
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim()
        .to_string()
}

/// `_file_type` — defaults to docx.
fn file_type(args: &Value) -> String {
    let ft = arg_str(args, "file_type");
    if ft.is_empty() {
        "docx".to_string()
    } else {
        ft
    }
}

/// `_paged_queries`; page_token goes last.
fn paged_queries(args: &Value, extra: &[(&str, &str)]) -> Vec<(String, String)> {
    let mut queries: Vec<(String, String)> = vec![
        ("file_type".into(), file_type(args)),
        ("user_id_type".into(), "open_id".into()),
        (
            "page_size".into(),
            args.get("page_size")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .to_string(),
        ),
    ];
    for (k, v) in extra {
        queries.push(((*k).into(), (*v).into()));
    }
    let page_token = arg_str(args, "page_token");
    if !page_token.is_empty() {
        queries.push(("page_token".into(), page_token));
    }
    queries
}

/// Execute `feishu_doc_read`.
pub fn execute_doc_read(args: &Value) -> String {
    let doc_token = arg_str(args, "doc_token");
    if doc_token.is_empty() {
        return tool_error("doc_token is required");
    }
    let Some(client) = get_client() else {
        return tool_error("Feishu client not available (not in a Feishu comment context)");
    };
    let path = format!("/open-apis/docx/v1/documents/{doc_token}/raw_content");
    match client.request("GET", &path, &[], None) {
        Err(e) => tool_error(&format!("Failed to read document: {e}")),
        Ok((code, msg, data)) => {
            if code != 0 {
                return tool_error(&format!(
                    "Failed to read document: code={code} msg={msg}"
                ));
            }
            let content = data
                .get("data")
                .and_then(|d| d.get("content"))
                .cloned()
                .unwrap_or(Value::Null);
            match content.as_str() {
                Some(content) => json!({ "success": true, "content": content }).to_string(),
                None => tool_error("No content returned from document API"),
            }
        }
    }
}

/// `_comment_op`: client check, then required keys (stripped), then the call.
fn comment_op(
    args: &Value,
    keys: &[&str],
    missing_msg: &str,
    label: &str,
    method: &str,
    uri: &str,
    queries: impl FnOnce(&Value) -> Vec<(String, String)>,
    body: Option<Value>,
    flag_success: bool,
) -> String {
    let client = match get_client() {
        Some(client) => client,
        None => return tool_error("Feishu client not available"),
    };
    let values: Vec<String> = keys.iter().map(|k| arg_str(args, k)).collect();
    if values.iter().any(|v| v.is_empty()) {
        return tool_error(missing_msg);
    }
    // Substitute the URI path params (all keys except `content`).
    let mut path = uri.to_string();
    for (k, v) in keys.iter().zip(&values) {
        if *k != "content" {
            path = path.replace(&format!(":{k}"), v);
        }
    }
    let _ = uri;
    match client.request(method, &path, &queries(args), body.as_ref()) {
        Err(e) => tool_error(&format!("{label} failed: {e}")),
        Ok((code, msg, data)) => {
            if code != 0 {
                return tool_error(&format!("{label} failed: code={code} msg={msg}"));
            }
            if flag_success {
                json!({ "success": true, "data": data }).to_string()
            } else {
                json!({ "data": data }).to_string()
            }
        }
    }
}

/// Execute `feishu_drive_list_comments`.
pub fn execute_list_comments(args: &Value) -> String {
    let is_whole = args.get("is_whole").and_then(Value::as_bool).unwrap_or(false);
    let mut extra: Vec<(&str, &str)> = Vec::new();
    if is_whole {
        extra.push(("is_whole", "true"));
    }
    comment_op(
        args,
        &["file_token"],
        "file_token is required",
        "List comments",
        "GET",
        COMMENTS_URI,
        |a| paged_queries(a, &extra),
        None,
        false,
    )
}

/// Execute `feishu_drive_list_comment_replies`.
pub fn execute_list_replies(args: &Value) -> String {
    comment_op(
        args,
        &["file_token", "comment_id"],
        "file_token and comment_id are required",
        "List replies",
        "GET",
        REPLIES_URI,
        |a| paged_queries(a, &[]),
        None,
        false,
    )
}

/// Execute `feishu_drive_reply_comment` (rich content.elements[text_run] body).
pub fn execute_reply_comment(args: &Value) -> String {
    let content = arg_str(args, "content");
    let body = json!({ "content": { "elements": [ { "type": "text_run", "text_run": { "text": content } } ] } });
    comment_op(
        args,
        &["file_token", "comment_id", "content"],
        "file_token, comment_id, and content are required",
        "Reply comment",
        "POST",
        REPLIES_URI,
        |a| vec![("file_type".into(), file_type(a))],
        Some(body),
        true,
    )
}

/// Execute `feishu_drive_add_comment` (flat reply_elements[text] body).
pub fn execute_add_comment(args: &Value) -> String {
    let content = arg_str(args, "content");
    let body = json!({
        "file_type": file_type(args),
        "reply_elements": [{ "type": "text", "text": content }]
    });
    comment_op(
        args,
        &["file_token", "content"],
        "file_token and content are required",
        "Add comment",
        "POST",
        ADD_COMMENT_URI,
        |_| vec![],
        Some(body),
        true,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct Recording {
        calls: Mutex<Vec<(String, String, Vec<(String, String)>, Option<Value>)>>,
        code: i64,
        msg: String,
        data: Value,
    }
    impl LarkClient for Recording {
        fn request(
            &self,
            method: &str,
            path: &str,
            query: &[(String, String)],
            body: Option<&Value>,
        ) -> Result<(i64, String, Value), String> {
            self.calls.lock().unwrap().push((
                method.to_string(),
                path.to_string(),
                query.to_vec(),
                body.cloned(),
            ));
            Ok((self.code, self.msg.clone(), self.data.clone()))
        }
    }

    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: Mutex<()> = Mutex::new(());
        LOCK.lock().unwrap()
    }

    #[test]
    fn doc_read_validation_and_client_gate() {
        let _g = guard();
        set_client(None);
        assert!(execute_doc_read(&json!({})).contains("doc_token is required"));
        assert!(execute_doc_read(&json!({"doc_token": "tok"}))
            .contains("Feishu client not available (not in a Feishu comment context)"));
    }

    #[test]
    fn doc_read_happy_path_and_api_error() {
        let _g = guard();
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({ "data": { "content": "hello doc" } }),
        });
        set_client(Some(stub.clone()));
        let out = execute_doc_read(&json!({"doc_token": "abc"}));
        set_client(None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        assert_eq!(v["content"], "hello doc");
        let (method, path, query, _) = &stub.calls.lock().unwrap()[0];
        assert_eq!((method.as_str(), path.as_str(), query.len()), ("GET", "/open-apis/docx/v1/documents/abc/raw_content", 0));

        let failing = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 230001,
            msg: "nope".into(),
            data: json!({}),
        });
        set_client(Some(failing));
        let out = execute_doc_read(&json!({"doc_token": "abc"}));
        set_client(None);
        assert!(out.contains("Failed to read document: code=230001 msg=nope"), "{out}");
    }

    #[test]
    fn list_comments_validation_queries_and_uri() {
        let _g = guard();
        // Client check precedes the key check (Python `_comment_op` order), so
        // a stub client is needed to reach the key validation.
        let probe = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({}),
        });
        set_client(Some(probe));
        assert!(execute_list_comments(&json!({})).contains("file_token is required"));
        set_client(None);
        assert!(execute_list_comments(&json!({"file_token": "f"}))
            .contains("Feishu client not available"));

        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({ "items": [] }),
        });
        set_client(Some(stub.clone()));
        let out = execute_list_comments(&json!({
            "file_token": "f", "is_whole": true, "page_size": 50
        }));
        set_client(None);
        let (method, path, query, _) = &stub.calls.lock().unwrap()[0];
        assert_eq!(method, "GET");
        assert_eq!(path, "/open-apis/drive/v1/files/f/comments");
        assert_eq!(
            query,
            &[
                ("file_type".to_string(), "docx".to_string()),
                ("user_id_type".to_string(), "open_id".to_string()),
                ("page_size".to_string(), "50".to_string()),
                ("is_whole".to_string(), "true".to_string()),
            ]
        );
        assert!(out.contains("\"data\""));
    }

    #[test]
    fn reply_comment_shapes_the_text_run_body() {
        let _g = guard();
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({}),
        });
        set_client(Some(stub.clone()));
        let out = execute_reply_comment(&json!({
            "file_token": "f", "comment_id": "c", "content": "hi", "file_type": "docx"
        }));
        set_client(None);
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["success"], true);
        let (method, path, query, body) = &stub.calls.lock().unwrap()[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/open-apis/drive/v1/files/f/comments/c/replies"));
        assert_eq!(query[0], ("file_type".to_string(), "docx".to_string()));
        assert_eq!(
            body.as_ref().unwrap()["content"]["elements"][0]["text_run"]["text"],
            "hi"
        );
    }

    #[test]
    fn add_comment_shapes_the_reply_elements_body() {
        let _g = guard();
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({}),
        });
        set_client(Some(stub.clone()));
        let out = execute_add_comment(&json!({"file_token": "f", "content": "note"}));
        set_client(None);
        assert!(out.contains("\"success\":true"), "{out}");
        let (method, path, _, body) = &stub.calls.lock().unwrap()[0];
        assert_eq!((method.as_str(), path.as_str()), ("POST", "/open-apis/drive/v1/files/f/new_comments"));
        assert_eq!(body.as_ref().unwrap()["reply_elements"][0]["text"], "note");
        assert_eq!(body.as_ref().unwrap()["file_type"], "docx");
    }

    #[test]
    fn missing_keys_and_api_errors() {
        let _g = guard();
        // Python checks the client BEFORE the missing keys, so a stub client
        // is required to reach the key validation.
        let stub = std::sync::Arc::new(Recording {
            calls: Mutex::new(vec![]),
            code: 0,
            msg: "ok".into(),
            data: json!({}),
        });
        set_client(Some(stub));
        assert!(execute_list_replies(&json!({"file_token": "f"}))
            .contains("file_token and comment_id are required"));
        assert!(execute_reply_comment(&json!({"file_token": "f", "comment_id": "c"}))
            .contains("file_token, comment_id, and content are required"));
        assert!(execute_add_comment(&json!({"file_token": "f"}))
            .contains("file_token and content are required"));
        set_client(None);
        // Without a client every drive op degrades first.
        assert!(execute_reply_comment(&json!({"file_token": "f"}))
            .contains("Feishu client not available"));
    }
}
