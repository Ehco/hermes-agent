//! clarify — structured multiple-choice / open-ended questions to the user.
//!
//! Port of `tools/clarify_tool.py`: schema validation, recommendation
//! decoration and result shaping are all here; the UI is a platform-provided
//! callback (the `ClarifyUi` seam — gateway/TUI inject it, tests stub it, plain
//! CLI execution has none).

use serde_json::Value::String as SValue;
use serde_json::{json, Value};

use crate::util::tool_error;

pub const MAX_CHOICES: usize = 4; // the UI always appends an "Other" free-text row
pub const MAX_QUESTIONS: usize = 5; // independent questions per call
pub const MAX_CHOICE_CHARS: usize = 8000;
pub const RECOMMENDED_LABEL: &str = "(Recommended)";
const UNAVAILABLE: &str = "Clarify tool is not available in this execution context.";
const SHAPE: &str =
    "Pass questions=[{question, choices?, multi_select?}]; a single question is a one-entry array.";

/// The platform-injected UI: receives the normalized questions, answers with
/// `{"answers": {qid: raw|None}, "outcome", "notice"?}`.
pub trait ClarifyUi: Send + Sync {
    fn ask(&self, questions: &Value) -> Result<Value, String>;
}

/// Suffix the first choice with the recommended label (Python
/// `mark_recommended`); idempotent, and a lone choice is left untouched.
pub fn mark_recommended(choices: Vec<String>) -> Vec<String> {
    let first = choices
        .first()
        .map(|c| c.trim().to_string())
        .unwrap_or_default();
    // Already-decorated first choice is left alone (idempotent), as is a lone
    // choice (nothing to prefer it over) — Python's guard order.
    if choices.len() < 2 || first != strip_recommended(&first) {
        return choices;
    }
    let mut out = vec![format!("{first} {RECOMMENDED_LABEL}")];
    out.extend(choices.into_iter().skip(1));
    out
}

/// Remove the recommendation label so presentation never leaks into
/// `user_response` (Python `strip_recommended`).
pub fn strip_recommended(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed
        .to_lowercase()
        .ends_with(&RECOMMENDED_LABEL.to_lowercase())
    {
        return trimmed[..trimmed.len() - RECOMMENDED_LABEL.len()]
            .trim()
            .to_string();
    }
    trimmed.to_string()
}

/// Strip presentation from a locked answer (Python `_clean_answer`): a
/// multi-select answer is a list, a JSON-array string, or one typed answer.
fn clean_answer(raw: &Value, multi: bool) -> Vec<String> {
    if !multi {
        return match raw {
            SValue(s) => vec![strip_recommended(s)],
            other => vec![strip_recommended(&other.to_string())],
        };
    }
    let parsed: Vec<Value> = match raw {
        SValue(s) => {
            serde_json::from_str::<Vec<Value>>(s).unwrap_or_else(|_| vec![Value::String(s.clone())])
        }
        Value::Array(items) => items.clone(),
        other => vec![other.clone()],
    };
    parsed
        .iter()
        .map(|item| match item {
            SValue(s) => strip_recommended(s),
            other => strip_recommended(&other.to_string()),
        })
        .filter(|item| !item.is_empty())
        .collect()
}

/// Validate `questions` (Python `_normalize_questions`): `(normalized, error)`.
pub fn normalize_questions(questions: &Value) -> Result<Vec<Value>, String> {
    let Some(items) = questions.as_array() else {
        return Err(format!("questions must be a non-empty array. {SHAPE}"));
    };
    if items.is_empty() {
        return Err(format!("questions must be a non-empty array. {SHAPE}"));
    }
    if items.len() > MAX_QUESTIONS {
        return Err(format!("questions supports at most {MAX_QUESTIONS} items."));
    }
    let mut normalized = Vec::new();
    for (index, item) in items.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(format!("questions[{index}] must be an object. {SHAPE}"));
        };
        let text = obj
            .get("question")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim()
            .to_string();
        if text.is_empty() {
            return Err(format!(
                "questions[{index}].question must be non-empty text."
            ));
        }
        let mut choices: Option<Vec<String>> = None;
        if let Some(raw) = obj.get("choices").filter(|c| !c.is_null()) {
            let Some(list) = raw.as_array() else {
                return Err(format!(
                    "questions[{index}].choices must be a list of strings."
                ));
            };
            let mut strings = Vec::new();
            for choice in list {
                let Some(s) = choice.as_str() else {
                    return Err(format!(
                        "questions[{index}].choices must be a list of strings."
                    ));
                };
                if s.trim().chars().count() > MAX_CHOICE_CHARS {
                    return Err(format!(
                        "questions[{index}].choices[{}] is {} characters; the limit is {MAX_CHOICE_CHARS}. \
                         Move detail into the question text and resend.",
                        strings.len(),
                        s.trim().chars().count()
                    ));
                }
                strings.push(s.to_string());
            }
            let cleaned: Vec<String> = strings
                .iter()
                .filter(|c| !c.trim().is_empty())
                .map(|c| c.trim().to_string())
                .take(MAX_CHOICES)
                .collect();
            if !list.is_empty() && cleaned.is_empty() {
                return Err(format!(
                    "questions[{index}].choices has {} entries but all are blank. \
                     Send real labels, or omit choices to ask open-ended.",
                    list.len()
                ));
            }
            choices = Some(cleaned);
        }
        let decorated = choices.map(mark_recommended);
        let multi = obj
            .get("multi_select")
            .and_then(Value::as_bool)
            .unwrap_or(false)
            && decorated.is_some();
        normalized.push(json!({
            "qid": format!("q{index}"),
            "question": text,
            "choices": decorated,
            "choices_offered": decorated,
            "multi_select": multi,
        }));
    }
    Ok(normalized)
}

/// Result JSON from a callback reply (Python `_result`): every response carries
/// `status` and `user_response` (null unless answered); `outcome` says how the
/// wait ended and `notice` says why when it ended without a submit.
fn result_json(normalized: &[Value], reply: &Value) -> String {
    let answers = reply.get("answers").cloned().unwrap_or_else(|| json!({}));
    let mut responses = Vec::new();
    for entry in normalized {
        let qid = entry["qid"].as_str().unwrap_or_default();
        let multi = entry["multi_select"].as_bool().unwrap_or(false);
        let raw = answers
            .get(qid)
            .filter(|a| !a.is_null() && a.as_str() != Some(""));
        let cleaned = raw
            .map(|a| clean_answer(a, multi))
            .filter(|c| !c.is_empty());
        let (status, value) = match cleaned {
            Some(value) => ("answered", Some(json!(value))),
            None => (
                if answers.get(qid).is_some() {
                    "skipped"
                } else {
                    "unanswered"
                },
                None,
            ),
        };
        responses.push(json!({
            "question": entry["question"],
            "choices_offered": entry["choices_offered"],
            "status": status,
            "user_response": value,
        }));
    }
    let mut result = json!({ "responses": responses, "outcome": reply.get("outcome").cloned().unwrap_or(Value::Null) });
    if let Some(notice) = reply
        .get("notice")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
    {
        result["notice"] = json!(notice);
    }
    result.to_string()
}

/// Execute `clarify` — the registry entry point (no UI available).
pub fn execute(args: &Value) -> String {
    execute_with(None, args)
}

/// `execute` against an injected UI (gateways, tests).
pub fn execute_with(ui: Option<&dyn ClarifyUi>, args: &Value) -> String {
    let normalized = match normalize_questions(args.get("questions").unwrap_or(&Value::Null)) {
        Ok(normalized) => normalized,
        Err(error) => return tool_error(error),
    };
    let Some(ui) = ui else {
        return tool_error(UNAVAILABLE);
    };
    match ui.ask(&Value::Array(normalized.clone())) {
        Ok(reply) => result_json(&normalized, &reply),
        Err(e) => tool_error(format!("Failed to get user input: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::sync::Mutex;

    struct StubUi {
        replies: Mutex<Vec<Result<Value, String>>>,
        received: Mutex<Vec<Value>>,
    }

    impl StubUi {
        fn new(replies: Vec<Result<Value, String>>) -> Self {
            Self {
                replies: Mutex::new(replies),
                received: Mutex::new(Vec::new()),
            }
        }
    }

    impl ClarifyUi for StubUi {
        fn ask(&self, questions: &Value) -> Result<Value, String> {
            self.received.lock().unwrap().push(questions.clone());
            self.replies
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(Err("no fixture".into()))
        }
    }

    #[test]
    fn validation_errors_are_faithful() {
        let cases = [
            (json!({}), "questions must be a non-empty array"),
            (
                json!({"questions": []}),
                "questions must be a non-empty array",
            ),
            (
                json!({"questions": (0..6).map(|i| json!({"question": format!("q{i}")})).collect::<Vec<_>>()}),
                "questions supports at most 5 items",
            ),
            (
                json!({"questions": ["q"]}),
                "questions[0] must be an object",
            ),
            (
                json!({"questions": [{"question": "  "}]}),
                "questions[0].question must be non-empty text",
            ),
            (
                json!({"questions": [{"question": "q", "choices": ["a", 3]}]}),
                "choices must be a list of strings",
            ),
            (
                json!({"questions": [{"question": "q", "choices": ["  "], }]}),
                "has 1 entries but all are blank",
            ),
        ];
        for (args, expected) in cases {
            let out = execute_with(Some(&StubUi::new(vec![])), &args);
            assert!(out.contains(expected), "{out}");
        }
    }

    #[test]
    fn long_choice_is_rejected_at_the_source() {
        let long = "x".repeat(MAX_CHOICE_CHARS + 1);
        let out = execute_with(
            Some(&StubUi::new(vec![])),
            &json!({"questions": [{"question": "q", "choices": [long]}]}),
        );
        assert!(out.contains("the limit is 8000"), "{out}");
    }

    #[test]
    fn no_ui_is_unavailable_after_validation() {
        let out = execute(&json!({"questions": [{"question": "pick one", "choices": ["a", "b"]}]}));
        assert_eq!(
            out,
            r#"{"error":"Clarify tool is not available in this execution context."}"#
        );
    }

    #[test]
    fn recommendations_are_marked_then_stripped() {
        assert_eq!(strip_recommended("Pick me (Recommended)"), "Pick me");
        assert_eq!(strip_recommended("pick me (recommended)"), "pick me");
        assert_eq!(strip_recommended("just text"), "just text");
        assert_eq!(
            mark_recommended(vec!["a".to_string()]),
            vec!["a".to_string()]
        );
        assert_eq!(
            mark_recommended(vec!["a".to_string(), "b".to_string()]),
            vec!["a (Recommended)".to_string(), "b".to_string()]
        );
        // Idempotent: a second pass leaves the decorated list alone.
        let decorated = mark_recommended(vec!["a".into(), "b".into()]);
        assert_eq!(mark_recommended(decorated.clone()), decorated);
    }

    #[test]
    fn answers_are_shaped_and_presentation_stripped() {
        let ui = StubUi::new(vec![Ok(json!({
            "answers": {
                "q0": "Option A (Recommended)",
                "q1": ["Opt 1", "  ", "Opt 2 (Recommended)"],
                "q2": null,
                "q3": ""
            },
            "outcome": "submitted",
            "notice": "all good"
        }))]);
        let out = execute_with(
            Some(&ui),
            &json!({"questions": [
                {"question": "single", "choices": ["Option A", "Option B"]},
                {"question": "multi", "choices": ["Opt 1", "Opt 2"], "multi_select": true},
                {"question": "skipped", "choices": ["Opt 1", "Opt 2"]},
                {"question": "open"},
                {"question": "unanswered"}
            ]}),
        );
        let parsed: Value = serde_json::from_str(&out).unwrap();
        let responses = parsed["responses"].as_array().unwrap();
        assert_eq!(parsed["outcome"], "submitted");
        assert_eq!(parsed["notice"], "all good");
        assert_eq!(responses[0]["status"], "answered");
        assert_eq!(responses[0]["user_response"], json!(["Option A"]));
        assert_eq!(responses[1]["status"], "answered");
        assert_eq!(responses[1]["user_response"], json!(["Opt 1", "Opt 2"]));
        assert_eq!(responses[2]["status"], "skipped");
        assert_eq!(responses[2]["user_response"], Value::Null);
        assert_eq!(responses[3]["status"], "skipped");
        assert_eq!(responses[4]["status"], "unanswered");
        // The UI saw the decorated choices; the response echoes the offered ones.
        let received = ui.received.lock().unwrap();
        let seen = received.last().unwrap();
        assert_eq!(
            seen[0]["choices"],
            json!(["Option A (Recommended)", "Option B"])
        );
        assert_eq!(
            responses[0]["choices_offered"],
            json!(["Option A (Recommended)", "Option B"])
        );
    }

    #[test]
    fn ui_failure_is_a_tool_error() {
        let out = execute_with(
            Some(&StubUi::new(vec![Err("gateway closed".into())])),
            &json!({"questions": [{"question": "q"}]}),
        );
        assert_eq!(
            out,
            r#"{"error":"Failed to get user input: gateway closed"}"#
        );
    }
}
