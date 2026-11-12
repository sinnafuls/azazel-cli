//! Seams the application plugs into the agent loop: lifecycle hooks and transcript persistence.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use azazel_ai::{Message, Request};

use crate::event::AgentInfo;

/// Called by the agent loop at fixed points. The application's event bus implements this.
#[async_trait]
pub trait AgentHooks: Send + Sync {
    /// Returning an error ends the run before the request is sent, with that text as the failure
    /// (a spending limit, for example).
    async fn turn_start(&self, _agent: &AgentInfo) -> Result<(), String> {
        Ok(())
    }

    async fn turn_end(&self, _agent: &AgentInfo) {}

    /// May rewrite the messages about to be sent. The change is not persisted.
    async fn context(&self, _agent: &AgentInfo, _messages: &mut Vec<Message>) {}

    /// Last chance to change the request: system parts, tools, sampling.
    async fn before_request(&self, _agent: &AgentInfo, _request: &mut Request) {}

    /// The agent has no more tool calls. Returning a reason keeps it going with that reason as a user
    /// message; `stop_hook_active` is true when a previous reason already did so for this prompt.
    async fn stop(&self, _agent: &AgentInfo, _stop_hook_active: bool) -> Option<String> {
        None
    }

    /// Returning a reason cancels a manual compaction.
    async fn pre_compact(&self, _agent: &AgentInfo, _manual: bool) -> Option<String> {
        None
    }

    async fn post_compact(&self, _agent: &AgentInfo) {}
}

pub struct NoHooks;

impl AgentHooks for NoHooks {}

/// Where the agent records its transcript. Ids returned here identify entries in later calls.
pub trait TranscriptSink: Send + Sync {
    /// Appends a message and returns its entry id.
    fn message(&self, message: &Message) -> String;

    /// Records that the content of entry `target` is replaced by `replacement` in future context.
    fn context_edit(&self, target: &str, replacement: &str);

    /// Records that everything before entry `first_kept` is replaced by `summary`; returns the id
    /// of the compaction entry.
    fn compaction(&self, summary: &str, first_kept: &str, tokens_before: u64) -> String;
}

/// Keeps nothing; hands out sequential ids. For agents whose transcript is not persisted.
#[derive(Default)]
pub struct NullSink {
    next: AtomicU64,
}

impl NullSink {
    fn id(&self) -> String {
        format!("{:08x}", self.next.fetch_add(1, Ordering::Relaxed))
    }
}

impl TranscriptSink for NullSink {
    fn message(&self, _message: &Message) -> String {
        self.id()
    }

    fn context_edit(&self, _target: &str, _replacement: &str) {}

    fn compaction(&self, _summary: &str, _first_kept: &str, _tokens_before: u64) -> String {
        self.id()
    }
}
