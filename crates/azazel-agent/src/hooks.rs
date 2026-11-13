//! Seams the application plugs into the agent loop: lifecycle hooks and transcript persistence.

use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use azazel_ai::{Message, Request};
use serde_json::Value;

use crate::event::AgentInfo;
use crate::inspect::ContextReport;

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

    /// Called with the text of the reply so far each time more of it arrives. Returning a note
    /// withdraws the reply: the request is cancelled, nothing of the reply is kept, the note is
    /// added as a user message and the request is sent again. A note that opens with
    /// `<rule name="…">` gives the stop its name in [`crate::AgentEvent::Rule`].
    ///
    /// Not async: it runs for every fragment the model sends.
    fn stream_text(&self, _agent: &AgentInfo, _so_far: &str) -> Option<String> {
        None
    }

    /// Like [`AgentHooks::stream_text`] for the reply's thinking.
    fn stream_thinking(&self, _agent: &AgentInfo, _so_far: &str) -> Option<String> {
        None
    }
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

    /// Records that a run began: the work one prompt starts. Called right after the prompt's
    /// message was appended. Returns the run's id, which [`TranscriptSink::run_ended`] repeats.
    /// `request_id` is the name whoever sent the prompt gave it.
    fn run_started(&self, _request_id: Option<&str>) -> String {
        String::new()
    }

    /// Records that a request went out to `model`.
    fn request_started(&self, _model: &str) {}

    /// Records that a tool call was let through and is about to run.
    fn tool_started(&self, _call_id: &str, _name: &str, _input: &Value) {}

    /// Records how the run `run_id` ended: a stop reason, or `failed`. A run without this
    /// record was cut short by the end of the process.
    fn run_ended(&self, _run_id: &str, _stop: &str) {}

    /// Called with what the lead's newest request is made of: when it is sent, and again when
    /// the provider has counted its tokens. A sink that keeps it lets the request be inspected
    /// from outside the process.
    fn request(&self, _report: &ContextReport) {}
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
