//! Provider transports (port of `agent/transports/`): the wire layer that
//! converts the shared Message/ToolSpec model into provider requests and maps
//! provider SSE streams back into normalized `StreamItem`s.

pub mod anthropic;
pub mod chat_completions;
pub mod sse;

use crate::transport::StreamItem;
use anyhow::{anyhow, Result};
use futures::stream::{Stream, StreamExt};
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use sse::{SseEvent, SseParser};
use std::collections::VecDeque;

/// Per-provider event sink: consumes SSE data payloads and emits stream items.
/// Kept separate from the byte pump so it is unit-testable without network.
pub trait EventSink {
    /// Handle one SSE event; push produced items (Ok items, or an Err to fail
    /// the stream). Implementations must not rely on being called again.
    fn on_event(&mut self, event: &SseEvent, out: &mut Vec<Result<StreamItem>>);
    /// Stream ended: flush any pending work (e.g. an unterminated tool call).
    fn on_finish(&mut self, out: &mut Vec<Result<StreamItem>>);
}

/// Byte pump: pulls the provider HTTP body, parses SSE frames, and drives an
/// [`EventSink`].
pub struct SseStream<A: EventSink> {
    inner: Pin<Box<dyn Stream<Item = reqwest::Result<Vec<u8>>> + Send>>,
    parser: SseParser,
    acc: A,
    out: VecDeque<Result<StreamItem>>,
    done: bool,
}

impl<A: EventSink> SseStream<A> {
    pub fn new(inner: Pin<Box<dyn Stream<Item = reqwest::Result<Vec<u8>>> + Send>>, acc: A) -> Self {
        Self { inner, parser: SseParser::new(), acc, out: VecDeque::new(), done: false }
    }
}

impl<A: EventSink + Send + Unpin> Stream for SseStream<A> {
    type Item = Result<StreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            if let Some(item) = self.out.pop_front() {
                return Poll::Ready(Some(item));
            }
            if self.done {
                return Poll::Ready(None);
            }
            // `Pin<&mut dyn Stream>`: Stream + Unpin, so this is safe to poll.
            match self.inner.as_mut().poll_next_unpin(cx) {
                Poll::Ready(Some(Ok(bytes))) => {
                    for ev in self.parser.feed(&bytes) {
                        let mut produced = Vec::new();
                        self.acc.on_event(&ev, &mut produced);
                        self.out.extend(produced);
                    }
                }
                Poll::Ready(Some(Err(e))) => {
                    self.done = true;
                    return Poll::Ready(Some(Err(anyhow!("provider stream error: {e}"))));
                }
                Poll::Ready(None) => {
                    for ev in self.parser.finish() {
                        let mut produced = Vec::new();
                        self.acc.on_event(&ev, &mut produced);
                        self.out.extend(produced);
                    }
                    let mut tail = Vec::new();
                    self.acc.on_finish(&mut tail);
                    self.out.extend(tail);
                    self.done = true;
                }
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

/// Lazily connects (async request send) and then delegates to the resulting
/// stream. Lets the synchronous `Transport::stream` hand out a stream before
/// the HTTP connection is established.
pub struct ConnectThen<S> {
    fut: Option<Pin<Box<dyn Future<Output = Result<S>> + Send>>>,
    inner: Option<S>,
}

impl<S> ConnectThen<S> {
    pub fn new(fut: Pin<Box<dyn Future<Output = Result<S>> + Send>>) -> Self {
        Self { fut: Some(fut), inner: None }
    }
}

impl<S> Stream for ConnectThen<S>
where
    S: Stream<Item = Result<StreamItem>> + Send + Unpin,
{
    type Item = Result<StreamItem>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.inner.is_none() {
            let Some(mut fut) = self.fut.take() else { return Poll::Ready(None) };
            match fut.as_mut().poll(cx) {
                Poll::Pending => {
                    self.fut = Some(fut);
                    return Poll::Pending;
                }
                // Surface the connect error once, then end the stream (fut and
                // inner are both gone, so later polls return None).
                Poll::Ready(Err(e)) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(Ok(stream)) => self.inner = Some(stream),
            }
        }
        self.inner.as_mut().unwrap().poll_next_unpin(cx)
    }
}

/// Shared helpers for building request JSON from the shared model.

/// Join the Text contents of a message.
pub(crate) fn text_of(content: &[crate::model::Content]) -> String {
    content
        .iter()
        .filter_map(|c| match c {
            crate::model::Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Truncate an error body for display without dumping unbounded payloads.
pub(crate) fn truncate_body(s: &str) -> String {
    s.trim().chars().take(512).collect()
}
