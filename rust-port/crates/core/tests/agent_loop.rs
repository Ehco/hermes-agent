//! End-to-end test of the agent loop with a scripted transport and a stub
//! tool host: proves turn iteration, tool-call execution, and result feeding
//! work without any network.

use anyhow::Result;
use hermes_core::agent::run_turn;
use hermes_core::model::{Message, Role, ToolSpec};
use hermes_core::test_transport::ScriptedTransport;
use hermes_core::transport::{StreamItem, Transport};

struct Host;
impl hermes_core::agent::ToolHost for Host {
    async fn execute(&self, name: &str, _arguments: &str) -> Result<String> {
        assert_eq!(name, "terminal");
        Ok("ok: 42".to_string())
    }
}

#[tokio::test]
async fn tool_call_round_completes() {
    // Round 1: assistant streams text + a tool call. Round 2: final answer.
    let transport = ScriptedTransport::new(vec![
        vec![
            StreamItem::TextDelta("checking ".into()),
            StreamItem::TextDelta("the answer".into()),
            StreamItem::ToolCall { id: "c1".into(), name: "terminal".into(), arguments: r#"{"cmd":"echo 42"}"#.into() },
        ],
        vec![StreamItem::TextDelta("the answer is 42".into()), StreamItem::Done],
    ]);
    let tools = vec![ToolSpec {
        name: "terminal".into(),
        description: "run a command".into(),
        schema: serde_json::json!({"type": "object"}),
    }];

    let mut history = vec![Message::text(Role::User, "what is it?")];
    let final_text = run_turn(&transport, &tools, &mut history, &Host).await.unwrap();

    assert_eq!(final_text, "the answer is 42");
    // user, assistant(+tool call), tool result, assistant(final)
    assert_eq!(history.len(), 4);
    assert!(matches!(history[2].content[0], hermes_core::model::Content::ToolResult { ref output, .. } if output.contains("42")));
}

#[tokio::test]
async fn plain_answer_short_circuits() {
    let transport = ScriptedTransport::new(vec![vec![StreamItem::TextDelta("hi".into()), StreamItem::Done]]);
    let mut history = vec![Message::text(Role::User, "ping")];
    let out = run_turn(&transport, &[], &mut history, &Host).await.unwrap();
    assert_eq!(out, "hi");
    assert_eq!(history.len(), 2);
}
