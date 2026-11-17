//! Agent runtime: the `Agent` actor loop, the `Tool` and `Gate` traits, events, cancellation and
//! compaction.

pub mod agent;
pub mod compaction;
pub mod event;
pub mod gate;
pub mod hooks;
pub mod inspect;
mod repair;
pub mod tool;

pub use agent::{Agent, AgentConfig, AgentHandle, AgentStatus, Limits};
pub use compaction::{CompactionSettings, ContextEntry};
pub use event::{AgentCmd, AgentEvent, AgentId, AgentInfo, Resume, UserInput};
pub use gate::{AllowAll, Gate, GateDecision};
pub use hooks::{AgentHooks, LimitHandler, NoHooks, NullSink, OnLimit, Recovery, TranscriptSink};
pub use inspect::{Category, CategoryKind, ContextReport, RequestRecord};
pub use tool::{
    Access, ArtifactSink, Deferral, Offer, Services, Timeout, Tool, ToolCtx, ToolOutput, ToolPolicy,
    ToolSettings, ToolSource, UpdateSink, tool_spec,
};
