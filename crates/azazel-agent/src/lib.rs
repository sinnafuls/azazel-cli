//! Agent runtime: the `Agent` actor loop, the `Tool` and `Gate` traits, events, cancellation and
//! compaction.

pub mod agent;
pub mod compaction;
pub mod event;
pub mod gate;
pub mod hooks;
pub mod tool;

pub use agent::{Agent, AgentConfig, AgentHandle, AgentStatus, Limits};
pub use compaction::{CompactionSettings, ContextEntry};
pub use event::{AgentCmd, AgentEvent, AgentId, AgentInfo, UserInput};
pub use gate::{AllowAll, Gate, GateDecision};
pub use hooks::{AgentHooks, NoHooks, NullSink, TranscriptSink};
pub use tool::{
    Access, ArtifactSink, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink, tool_spec,
};
