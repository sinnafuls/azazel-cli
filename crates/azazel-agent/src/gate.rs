//! The checkpoint every tool call passes through before and after it runs.

use async_trait::async_trait;
use serde_json::Value;

use crate::event::AgentInfo;
use crate::tool::{Tool, ToolOutput};

pub enum GateDecision {
    /// Run the call, with `input` replacing the model's arguments when it is `Some`.
    Allow { input: Option<Value> },
    /// Do not run it; `reason` becomes the error result the model sees.
    Deny { reason: String },
}

#[async_trait]
pub trait Gate: Send + Sync {
    async fn before(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, input: &Value) -> GateDecision;
    async fn after(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, output: &mut ToolOutput);
}

/// A gate that lets everything through.
pub struct AllowAll;

#[async_trait]
impl Gate for AllowAll {
    async fn before(&self, _: &AgentInfo, _: &dyn Tool, _: &str, _: &Value) -> GateDecision {
        GateDecision::Allow { input: None }
    }

    async fn after(&self, _: &AgentInfo, _: &dyn Tool, _: &str, _: &mut ToolOutput) {}
}
