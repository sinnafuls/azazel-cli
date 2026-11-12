//! Commands an agent accepts and the events it emits.

use azazel_ai::{Block, Message, ModelRef, ModelSpec, StopReason, StreamEvent, ThinkingLevel, Tier};
use serde::{Serialize, Serializer, ser::SerializeStruct};
use serde_json::Value;

use crate::tool::ToolOutput;

pub type AgentId = String;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct AgentInfo {
    pub id: AgentId,
    /// Name of the agent definition (`lead` for the main agent).
    pub def: String,
    pub parent: Option<AgentId>,
    /// 0 for the lead.
    pub depth: u32,
    pub session_id: String,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct UserInput {
    /// `Text` and `Image` blocks.
    pub content: Vec<Block>,
}

impl UserInput {
    pub fn text(text: impl Into<String>) -> Self {
        UserInput {
            content: vec![Block::text(text)],
        }
    }

    pub fn into_message(self) -> Message {
        Message::user_blocks(self.content)
    }
}

#[derive(Debug, Clone)]
pub enum AgentCmd {
    /// Start working on this input. While the agent is busy it is queued like `FollowUp`.
    Prompt(UserInput),
    /// Deliver after the current tool batch, before the next request.
    Steer(UserInput),
    /// Deliver once the agent would otherwise go idle.
    FollowUp(UserInput),
    /// Add to the context without asking for a reply (while busy it is delivered like `Steer`).
    Append(UserInput),
    Abort,
    SetModel(Vec<ModelSpec>),
    SetThinking(ThinkingLevel),
    Compact {
        instructions: Option<String>,
    },
    Shutdown,
}

/// One prompt produces `AgentStart`, then per request `TurnStart`, `MessageStart`, `Stream`*,
/// `MessageEnd`, (`ToolStart`, `ToolUpdate`*, `ToolEnd`)*, `TurnEnd`, and finally `Idle` or `Failed`.
/// A request whose reply a rule withdrew has `Rule` in place of `MessageEnd`.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    AgentStart,
    TurnStart {
        turn: u32,
    },
    MessageStart,
    /// Non-terminal stream events of the message in progress; serialised as `{"event": …}`.
    #[serde(serialize_with = "serialize_stream")]
    Stream(StreamEvent),
    MessageEnd {
        message: Message,
    },
    ToolStart {
        call_id: String,
        name: String,
        input: Value,
    },
    ToolUpdate {
        call_id: String,
        text: String,
    },
    ToolEnd {
        call_id: String,
        output: ToolOutput,
        duration_ms: u64,
    },
    TurnEnd,
    /// The message in progress was discarded and the same model is being asked again.
    Retry {
        attempt: u32,
        model: ModelRef,
    },
    Fallback {
        from: ModelRef,
        to: ModelRef,
    },
    /// Account `from` of `provider` is out of allowance until `until` (Unix seconds); requests
    /// now use account `to`. `limit` names what ran out (`5-hour limit`).
    Account {
        provider: String,
        from: String,
        to: String,
        until: u64,
        limit: String,
    },
    /// `model` refused the service tier and is asked without one from now on.
    TierDropped {
        model: ModelRef,
        tier: Tier,
    },
    /// A rule matched the reply as it was written: the message in progress was discarded and the
    /// request is sent again with the rule added.
    Rule {
        name: String,
    },
    Compaction {
        tokens_before: u64,
        tokens_after: u64,
    },
    Idle {
        stop: StopReason,
    },
    Failed {
        error: String,
    },
}

impl AgentEvent {
    /// Whether this event ends the work started by a prompt.
    pub fn is_terminal(&self) -> bool {
        matches!(self, AgentEvent::Idle { .. } | AgentEvent::Failed { .. })
    }
}

fn serialize_stream<S: Serializer>(event: &StreamEvent, serializer: S) -> Result<S::Ok, S::Error> {
    let mut s = serializer.serialize_struct("Stream", 1)?;
    s.serialize_field("event", event)?;
    s.end()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn events_serialise_with_snake_case_type_tag() {
        assert_eq!(
            serde_json::to_value(AgentEvent::AgentStart).unwrap(),
            json!({"type": "agent_start"})
        );
        assert_eq!(
            serde_json::to_value(AgentEvent::Idle {
                stop: StopReason::Stop
            })
            .unwrap(),
            json!({"type": "idle", "stop": "stop"})
        );
        assert_eq!(
            serde_json::to_value(AgentEvent::Stream(StreamEvent::TextDelta {
                index: 0,
                delta: "hi".into()
            }))
            .unwrap(),
            json!({"type": "stream", "event": {"type": "text_delta", "index": 0, "delta": "hi"}})
        );
        assert_eq!(
            serde_json::to_value(AgentEvent::ToolEnd {
                call_id: "c".into(),
                output: ToolOutput::error("no"),
                duration_ms: 3
            })
            .unwrap(),
            json!({
                "type": "tool_end",
                "call_id": "c",
                "output": {"content": [{"type": "text", "text": "no"}], "is_error": true},
                "duration_ms": 3
            })
        );
    }
}
