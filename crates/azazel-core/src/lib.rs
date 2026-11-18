//! Application core: paths, config, trust, sessions, checkpoints, permissions, the `Extension`
//! registrar and event bus, hooks, context files, skills, commands, agent definitions and URL schemes.

pub mod agents;
pub mod artifacts;
pub mod builtin;
pub mod cache;
pub mod checkpoint;
pub mod classify;
pub mod commands;
pub mod config;
pub mod config_keys;
pub mod context;
pub mod ext;
pub mod frontmatter;
pub mod hooks;
pub mod ledger;
pub mod limits;
pub mod media;
pub mod memory;
pub mod names;
pub mod packages;
pub mod paths;
pub mod permissions;
pub mod process;
pub mod prompt;
pub mod rules;
pub mod runaway;
pub mod scheme;
pub mod session;
pub mod settings;
pub mod side;
pub mod skills;
pub mod subagent;
pub mod toolcall;
pub mod toolsearch;
pub mod tracker;
pub mod transcript;
pub mod trust;
pub mod ui;

pub use artifacts::{ArtifactScheme, ArtifactStore};
pub use config::{Config, ConfigError, LoadOptions, LoadedConfig, PermissionMode};
pub use ext::{
    Command, CommandHandler, CommandOutcome, CommandProvider, CommandSource, Event, EventBus, EventHandler,
    EventKind, ExtCtx, ExtError, Extension, Level, McpRegistrar, McpServers, Outcome, Registrar, Registry,
    RegistryHandle, StartSource, ToolListing, ToolRegistry, ToolWrap,
};
pub use memory::{Memory, MemoryApi, MemoryHit};
pub use paths::Paths;
pub use permissions::gate::{OutputRedactor, PermissionGate};
pub use permissions::{PermissionState, Rule, RuleKind};
pub use prompt::{PromptBuilder, SystemPartProvider};
pub use scheme::{Lines, Scheme, Schemes};
pub use session::{
    AgentRun, Entry, EntryKind, InterruptedRun, RequestRun, SessionFacts, SessionStore, SessionSummary,
};
pub use skills::{Skill, SkillGrants, SkillGrantsExt, SkillRegistry, SkillRoot, SkillSource};
pub use subagent::{SubagentRunner, Subagents};
pub use toolcall::ToolCaller;
pub use tracker::{FileTracker, Freshness};
pub use trust::TrustStore;
pub use ui::{
    ApprovalDecision, ApprovalRequest, AskKind, Excerpt, Headless, PlanDecision, Post, Question,
    ShortcutHandler, Ui, UiBridge, UiError, UiExtras,
};
