//! Test double: a transport that replays a scripted conversation, so the
//! agent loop + tool execution can be exercised end-to-end with no network.

use crate::model::{Message, ToolSpec};
use crate::transport::{StreamItem, Transport};
use anyhow::Result;
use futures::Stream;
use std::pin::Pin;

fn replay(turn: Vec<StreamItem>) -> Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>> {
    let items: Vec<Result<StreamItem>> = turn.into_iter().map(Ok).collect();
    Box::pin(futures::stream::iter(items))
}

/// Replays one scripted assistant turn per call, in insertion order; the last
/// turn repeats once the script is exhausted.
pub struct ScriptedTransport {
    pub turns: Vec<Vec<StreamItem>>,
    cursor: std::sync::atomic::AtomicUsize,
}

impl ScriptedTransport {
    pub fn new(turns: Vec<Vec<StreamItem>>) -> Self {
        Self { turns, cursor: std::sync::atomic::AtomicUsize::new(0) }
    }
}

impl Transport for ScriptedTransport {
    fn stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>> {
        let i = self.cursor.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let turn = self.turns.get(i).or_else(|| self.turns.last()).cloned().unwrap_or_default();
        Ok(replay(turn))
    }
}

/// A rotating scripted transport: pops turns in order across calls.
#[derive(Default)]
pub struct RotatingScript {
    pub turns: Vec<Vec<StreamItem>>,
}

impl RotatingScript {
    pub fn push_turn(&mut self, turn: Vec<StreamItem>) {
        self.turns.push(turn);
    }
}

impl Transport for RotatingScript {
    fn stream(
        &self,
        _messages: &[Message],
        _tools: &[ToolSpec],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>> {
        let last = self.turns.last().cloned().unwrap_or_default();
        Ok(replay(last))
    }
}
