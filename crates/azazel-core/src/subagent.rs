//! The seam through which lower layers run a sub-agent without knowing the team that provides it.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::AgentInfo;

use crate::permissions::Rule;

pub const UNAVAILABLE: &str = "sub-agents unavailable";

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Runs agent definition `def` on `prompt` as a child of `parent` and returns its final report.
    async fn run(&self, def: &str, prompt: String, parent: &AgentInfo) -> Result<String, String>;

    /// Like [`SubagentRunner::run`], with the agent limited to the tools `tools` grants, inside
    /// what its definition allows. A runner that cannot limit an agent refuses, rather than run
    /// it with more than was asked for.
    async fn run_limited(
        &self,
        def: &str,
        prompt: String,
        parent: &AgentInfo,
        tools: Vec<Rule>,
    ) -> Result<String, String> {
        let _ = (prompt, parent, tools);
        Err(format!("agent `{def}` cannot be started with a tool limit here"))
    }
}

/// The service under which the runner is registered.
pub struct Subagents(pub Arc<dyn SubagentRunner>);
