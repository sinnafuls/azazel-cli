//! Application core: paths, config, trust, sessions, checkpoints, permissions, the `Extension`
//! registrar and event bus, hooks, context files, skills, commands, agent definitions and URL schemes.

pub mod agents;
pub mod artifacts;
pub mod builtin;
pub mod checkpoint;
pub mod commands;
pub mod config;
pub mod context;
pub mod ext;
pub mod frontmatter;
pub mod hooks;
pub mod limits;
pub mod media;
pub mod memory;
pub mod packages;
pub mod paths;
pub mod permissions;
pub mod process;
pub mod prompt;
pub mod rules;
pub mod scheme;
pub mod session;
pub mod skills;
pub mod subagent;
pub mod tracker;
pub mod trust;
pub mod ui;

pub use artifacts::{ArtifactScheme, ArtifactStore};
pub use config::{Config, ConfigError, LoadOptions, LoadedConfig, PermissionMode};
pub use ext::{
    Command, CommandHandler, CommandOutcome, CommandSource, Event, EventBus, EventHandler, EventKind, ExtCtx,
    ExtError, Extension, Level, McpRegistrar, McpServers, Outcome, Registrar, Registry, RegistryHandle,
    StartSource, ToolRegistry,
};
pub use memory::{Memory, MemoryApi, MemoryHit};
pub use paths::Paths;
pub use permissions::gate::PermissionGate;
pub use permissions::{PermissionState, Rule, RuleKind};
pub use prompt::{PromptBuilder, SystemPartProvider};
pub use scheme::{Scheme, Schemes};
pub use session::{Entry, EntryKind, SessionStore, SessionSummary};
pub use skills::{Skill, SkillRegistry, SkillRoot, SkillSource};
pub use subagent::{SubagentRunner, Subagents};
pub use tracker::{FileTracker, Freshness};
pub use trust::TrustStore;
pub use ui::{
    ApprovalDecision, ApprovalRequest, Headless, PlanDecision, ShortcutHandler, Ui, UiBridge, UiError,
    UiExtras,
};
