//! The event stream every provider produces for one response.

use std::pin::Pin;

use futures::Stream;
use serde::{Deserialize, Serialize};

use crate::error::AiError;
use crate::types::{Block, Message};

/// Events for one assistant response.
///
/// `Start` comes first and exactly one of `Done` / `Error` comes last. `index` is the position of
/// the block in the final message's `content`. Deltas are for display; `BlockEnd` carries the
/// authoritative block and `Done` the authoritative message.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Start,
    TextDelta {
        index: usize,
        delta: String,
    },
    ThinkingDelta {
        index: usize,
        delta: String,
    },
    ToolCallStart {
        index: usize,
        id: String,
        name: String,
    },
    /// `delta` is a raw fragment of the JSON arguments.
    ToolCallDelta {
        index: usize,
        delta: String,
    },
    BlockEnd {
        index: usize,
        block: Block,
    },
    Done {
        message: Message,
    },
    Error {
        error: AiError,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        partial: Option<Message>,
    },
}

impl StreamEvent {
    pub fn is_terminal(&self) -> bool {
        matches!(self, StreamEvent::Done { .. } | StreamEvent::Error { .. })
    }
}

pub type EventStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;
