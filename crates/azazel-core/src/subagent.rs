//! The seam through which lower layers run a sub-agent without knowing the team that provides it.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::AgentInfo;

pub const UNAVAILABLE: &str = "sub-agents unavailable";

#[async_trait]
pub trait SubagentRunner: Send + Sync {
    /// Runs agent definition `def` on `prompt` as a child of `parent` and returns its final report.
    async fn run(&self, def: &str, prompt: String, parent: &AgentInfo) -> Result<String, String>;
}

/// The service under which the runner is registered.
pub struct Subagents(pub Arc<dyn SubagentRunner>);
