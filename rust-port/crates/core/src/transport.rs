//! Transport trait: the provider wire layer (port of `agent/transports/`).

use crate::model::Message;
use anyhow::Result;
use futures::Stream;
use std::pin::Pin;

/// A chunk of a streaming assistant turn.
#[derive(Debug, Clone)]
pub enum StreamItem {
    TextDelta(String),
    ToolCall { id: String, name: String, arguments: String },
    Done,
}

/// One provider transport. Implementations speak the provider's wire format
/// and yield normalized stream items.
pub trait Transport {
    /// Send the conversation, stream the next assistant turn.
    fn stream(
        &self,
        messages: &[Message],
        tools: &[crate::model::ToolSpec],
    ) -> Result<Pin<Box<dyn Stream<Item = Result<StreamItem>> + Send>>>;
}
