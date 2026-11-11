//! The agent team: supervisor, mailbox, worktrees, goals, the checker, orchestration tools and the
//! built-in agent definitions.

pub mod builtin;
pub mod checker;
pub mod detect;
mod ext;
pub mod goal;
mod team;
pub mod tools;
pub mod worktree;

pub use builtin::BUILTIN_AGENTS;
pub use checker::Checker;
pub use ext::TeamExt;
pub use goal::{Budget, Goal, GoalExt, GoalSettings, GoalStatus, Goals};
pub use team::{
    Finished, LEAD, MemberInfo, MemberState, ORCHESTRATION_TOOLS, SpawnRequest, Team, TeamDeps, TeamEvent,
    WaitOutcome,
};
