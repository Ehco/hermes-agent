//! Per-plugin event fanout (the Python `/events` WebSocket tailers, made
//! shared). Each plugin gets an independent cursor and history; every frame
//! the bus emits matches the desktop envelope the kanban plugin established:
//!
//! ```json
//! { "events": [ ... ], "cursor": 12 }
//! ```
//!
//! `cursor` is the id of the last event in the frame; a client passes it back
//! as `?since=<cursor>` to resume. No `since` starts at the tail (the client
//! already holds the snapshot); `since=0` replays history (Python
//! `_since_param` semantics).

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;

use serde_json::{json, Value};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

/// How many past events a plugin keeps for `?since=` replay.
const HISTORY_LIMIT: usize = 1000;

#[derive(Default)]
struct PluginChannel {
    cursor: u64,
    history: VecDeque<(u64, Value)>,
    subscribers: Vec<UnboundedSender<Value>>,
}

/// Shared, thread-safe event bus. Cheap to clone behind an `Arc`.
pub struct EventBus {
    channels: Mutex<HashMap<String, PluginChannel>>,
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new()
    }
}

impl EventBus {
    pub fn new() -> Self {
        Self {
            channels: Mutex::new(HashMap::new()),
        }
    }

    /// Live subscriber handle for one plugin's stream.
    pub fn subscribe(&self, plugin: &str) -> UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(plugin.to_string())
            .or_default()
            .subscribers
            .push(tx);
        rx
    }

    /// Publish `events` under `plugin`, stamping them with sequential ids that
    /// continue from the channel cursor. Emits one envelope frame per
    /// subscriber: `{"events": [...], "cursor": <id of the last event>}`.
    /// An empty `events` emits nothing (the Python tailers only send on new
    /// rows); the current cursor is still returned so a REST snapshot can
    /// carry it.
    pub fn publish(&self, plugin: &str, events: Vec<Value>) -> u64 {
        let mut channels = self.channels.lock().unwrap_or_else(|e| e.into_inner());
        let channel = channels.entry(plugin.to_string()).or_default();
        let mut batch = Vec::with_capacity(events.len());
        for mut event in events {
            channel.cursor += 1;
            // Stamp the sequential id the cursor protocol is built on.
            if let Some(obj) = event.as_object_mut() {
                obj.insert("id".into(), json!(channel.cursor));
            }
            channel.history.push_back((channel.cursor, event.clone()));
            batch.push(event);
        }
        while channel.history.len() > HISTORY_LIMIT {
            channel.history.pop_front();
        }
        let cursor = channel.cursor;
        if !batch.is_empty() {
            let frame = json!({ "events": batch, "cursor": cursor });
            channel
                .subscribers
                .retain(|sub| sub.send(frame.clone()).is_ok());
        }
        cursor
    }

    /// The channel's current cursor (0 when nothing was published yet).
    pub fn cursor(&self, plugin: &str) -> u64 {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(plugin)
            .map(|c| c.cursor)
            .unwrap_or(0)
    }

    /// Events with id > `since`, oldest first (for the `?since=` replay).
    pub fn replay(&self, plugin: &str, since: u64) -> Vec<Value> {
        self.channels
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(plugin)
            .map(|c| {
                c.history
                    .iter()
                    .filter(|(id, _)| *id > since)
                    .map(|(_, e)| e.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn frames_match_the_desktop_envelope() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe("kanban");

        bus.publish("kanban", vec![json!({"kind": "created"})]);

        let frame = rx.recv().await.unwrap();
        assert_eq!(
            frame,
            json!({"events": [{"kind": "created", "id": 1}], "cursor": 1})
        );
    }

    #[tokio::test]
    async fn empty_batch_emits_nothing_but_advances_nothing() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe("kanban");
        assert_eq!(bus.publish("kanban", vec![]), 0);
        assert!(rx.try_recv().is_err());
    }

    #[tokio::test]
    async fn cursors_are_per_plugin_and_monotonic() {
        let bus = EventBus::new();
        let mut rx = bus.subscribe("kanban");
        let mut rx2 = bus.subscribe("other");

        bus.publish("kanban", vec![json!({"a": 1}), json!({"a": 2})]);
        bus.publish("other", vec![json!({"b": 1})]);

        let frame = rx.recv().await.unwrap();
        assert_eq!(frame["cursor"], 2);
        assert_eq!(frame["events"].as_array().unwrap().len(), 2);
        assert_eq!(frame["events"][1]["id"], 2);
        assert_eq!(rx2.recv().await.unwrap()["cursor"], 1);

        assert_eq!(bus.cursor("kanban"), 2);
        assert_eq!(bus.cursor("other"), 1);
        assert_eq!(bus.cursor("unknown"), 0);
    }

    #[tokio::test]
    async fn replay_returns_events_after_since() {
        let bus = EventBus::new();
        bus.publish(
            "kanban",
            vec![json!({"a": 1}), json!({"a": 2}), json!({"a": 3})],
        );
        let events = bus.replay("kanban", 1);
        assert_eq!(
            events,
            vec![json!({"a": 2, "id": 2}), json!({"a": 3, "id": 3})]
        );
        assert!(bus.replay("kanban", 0).len() == 3);
        assert!(bus.replay("unknown", 0).is_empty());
    }

    #[tokio::test]
    async fn dead_subscribers_are_dropped() {
        let bus = EventBus::new();
        {
            let mut rx = bus.subscribe("kanban");
            bus.publish("kanban", vec![json!({"a": 1})]);
            let _ = rx.recv().await;
        } // rx dropped here
        let mut rx2 = bus.subscribe("kanban");
        bus.publish("kanban", vec![json!({"a": 2})]);
        let frame = rx2.recv().await.unwrap();
        assert_eq!(frame["cursor"], 2);
    }
}
