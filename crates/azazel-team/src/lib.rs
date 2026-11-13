//! The agent team: supervisor, mailbox, worktrees, goals, the checker, orchestration tools and the
//! built-in agent definitions.

pub mod advisor;
pub mod builtin;
pub mod checker;
pub mod coordinator;
pub mod detect;
mod ext;
pub mod goal;
pub mod report;
mod team;
pub mod tools;
pub mod worktree;

pub use advisor::{Advisor, AdvisorExt, AdvisorSettings};
pub use builtin::BUILTIN_AGENTS;
pub use checker::Checker;
pub use coordinator::{Coordinator, CoordinatorExt, LeadTools};
pub use ext::TeamExt;
pub use goal::{Budget, Goal, GoalExt, GoalSettings, GoalStatus, Goals};
pub use report::{REPORT_TOOL, Typed};
pub use team::{
    Finished, LEAD, MemberInfo, MemberState, Mentioned, ORCHESTRATION_TOOLS, SpawnRequest, Team, TeamDeps,
    TeamEvent, WaitOutcome,
};
