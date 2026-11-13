//! Coordinator mode: the lead plans and delegates. While the mode is on the lead is offered the
//! team tools and tools that look things up, and nothing that edits files or runs commands.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, PoisonError, RwLock};

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Tool, ToolSource};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Entry, EntryKind, ExtCtx, ExtError, Extension, Registrar,
    SystemPartProvider,
};

use crate::report::{REPORT_TOOL, ReportTool, Typed};
use crate::team::ORCHESTRATION_TOOLS;

/// What a coordinating lead keeps besides the team tools: reading and searching, its to-do list,
/// questions to the user, skills, memory, and the tools that end a goal, a plan or a typed run.
pub const COORDINATOR_TOOLS: [&str; 16] = [
    "read",
    "glob",
    "grep",
    "todo",
    "ask_user",
    "skill",
    "memory_search",
    "memory_save",
    "memory_forget",
    "semantic_search",
    "web_search",
    "web_fetch",
    "web_grep",
    "goal_done",
    "goal_blocked",
    "plan_submit",
];

/// How the mode is written to a session (`ModeChange` entries), next to the permission modes.
const ENTRY_PREFIX: &str = "coordinator:";

const PROMPT: &str = "# Coordinator mode\n\
You coordinate; you do not do the work yourself. You have no tool that edits files or runs commands: the \
agents you start with `task` do that.\n\
- Plan first. Split the work into pieces that do not depend on each other and decide what each must deliver.\n\
- Delegate in parallel. Put independent pieces into one `task` call, and write every prompt so that it \
stands alone: the goal, the paths that matter, the constraints, what to report.\n\
- Read every result critically. A report is a claim: check it against the code with `read` and `grep`, and \
send the agent back with `agent_send` when something is missing or wrong.\n\
- Verify through agents. Have one run the build and the tests, and a `reviewer` look at the changes, before \
you call the work done.\n\
- Tell the user plainly what was done, what was verified and what is still open.";

/// Whether a coordinating lead is offered the tool `name`.
pub fn offers(name: &str) -> bool {
    ORCHESTRATION_TOOLS.contains(&name) || COORDINATOR_TOOLS.contains(&name)
}

/// The text of the session entry that records the mode.
pub fn entry(on: bool) -> String {
    format!("{ENTRY_PREFIX}{}", if on { "on" } else { "off" })
}

/// The mode a session branch was last left in; `None` when it never recorded one.
pub fn recorded(branch: &[Entry]) -> Option<bool> {
    branch.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::ModeChange { mode } => match mode.strip_prefix(ENTRY_PREFIX)? {
            "on" => Some(true),
            "off" => Some(false),
            _ => None,
        },
        _ => None,
    })
}

type Watcher = Arc<dyn Fn(bool) + Send + Sync>;

/// Whether coordinator mode is on. Registered as a service; the lead's tools, its prompt and the
/// status line read it.
#[derive(Default)]
pub struct Coordinator {
    on: AtomicBool,
    watchers: RwLock<Vec<Watcher>>,
}

impl Coordinator {
    pub fn new(on: bool) -> Coordinator {
        Coordinator {
            on: AtomicBool::new(on),
            watchers: RwLock::default(),
        }
    }

    pub fn is_on(&self) -> bool {
        self.on.load(Ordering::SeqCst)
    }

    /// Switches the mode and tells the watchers when that changed it. Returns whether it did.
    pub fn set(&self, on: bool) -> bool {
        if self.on.swap(on, Ordering::SeqCst) == on {
            return false;
        }
        let watchers = self
            .watchers
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        for watcher in watchers {
            watcher(on);
        }
        true
    }

    /// Puts the mode back to what a session recorded, without telling the watchers: nothing
    /// changed that would need recording again.
    pub fn restore(&self, on: bool) {
        self.on.store(on, Ordering::SeqCst);
    }

    /// `watcher` is called after every change of the mode.
    pub fn on_change(&self, watcher: Watcher) {
        self.watchers
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .push(watcher);
    }
}

/// The lead's tools: everything the session offers, cut down to what a coordinator keeps while
/// the mode is on, plus `report` when the run owes a typed result.
pub struct LeadTools {
    tools: Arc<dyn ToolSource>,
    coordinator: Arc<Coordinator>,
    typed: Option<Arc<Typed>>,
}

impl LeadTools {
    pub fn new(
        tools: Arc<dyn ToolSource>,
        coordinator: Arc<Coordinator>,
        typed: Option<Arc<Typed>>,
    ) -> LeadTools {
        LeadTools {
            tools,
            coordinator,
            typed,
        }
    }
}

impl ToolSource for LeadTools {
    fn tools(&self, agent: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let mut tools = self.tools.tools(agent);
        if self.coordinator.is_on() {
            tools.retain(|tool| offers(tool.name()));
        }
        if let Some(typed) = &self.typed {
            // The run's own `report` takes the place of any other tool of that name.
            tools.retain(|tool| tool.name() != REPORT_TOOL);
            tools.push(Arc::new(ReportTool(typed.clone())));
        }
        tools
    }
}

struct CoordinatorPart(Arc<Coordinator>);

#[async_trait]
impl SystemPartProvider for CoordinatorPart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        (agent.depth == 0 && self.0.is_on()).then(|| PROMPT.to_string())
    }
}

struct CoordinatorCommand(Arc<Coordinator>);

#[async_trait]
impl CommandHandler for CoordinatorCommand {
    async fn run(&self, args: &str, _: &ExtCtx) -> CommandOutcome {
        const ON: &str = "coordinator mode is on: the lead plans and delegates, and has no tool that edits \
                          files or runs commands";
        const OFF: &str = "coordinator mode is off: the lead works with every tool again";
        match args.trim() {
            "" | "status" => CommandOutcome::Show(if self.0.is_on() {
                format!("{ON}; /coordinator off ends it")
            } else {
                "coordinator mode is off; /coordinator on starts it".to_string()
            }),
            "on" => {
                self.0.set(true);
                CommandOutcome::Show(ON.to_string())
            }
            "off" => {
                self.0.set(false);
                CommandOutcome::Show(OFF.to_string())
            }
            other => CommandOutcome::Error(format!("`{other}`: expected /coordinator on, off or status")),
        }
    }
}

/// Registers the [`Coordinator`] service, the lead's coordinator prompt and `/coordinator`.
pub struct CoordinatorExt {
    pub coordinator: Arc<Coordinator>,
}

impl Extension for CoordinatorExt {
    fn id(&self) -> &str {
        "coordinator"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.coordinator.clone());
        r.system_part("coordinator", Arc::new(CoordinatorPart(self.coordinator.clone())));
        r.command(Command {
            name: "coordinator".to_string(),
            description: "Have the lead only plan and delegate: it keeps no tool that edits or runs commands"
                .to_string(),
            argument_hint: Some("on | off | status".to_string()),
            handler: Arc::new(CoordinatorCommand(self.coordinator.clone())),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use azazel_agent::{Access, ToolCtx, ToolOutput};
    use serde_json::{Value, json};

    use super::*;

    struct Named(&'static str);

    #[async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test"
        }
        fn schema(&self) -> Value {
            json!({ "type": "object" })
        }
        fn access(&self, _: &Value) -> Access {
            Access::Read
        }
        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        async fn call(&self, _: Value, _: &ToolCtx) -> ToolOutput {
            ToolOutput::text(self.0)
        }
    }

    struct Fixed(Vec<&'static str>);

    impl ToolSource for Fixed {
        fn tools(&self, _: &AgentInfo) -> Vec<Arc<dyn Tool>> {
            self.0
                .iter()
                .map(|name| Arc::new(Named(name)) as Arc<dyn Tool>)
                .collect()
        }
    }

    fn lead() -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        }
    }

    fn names(source: &LeadTools) -> Vec<String> {
        source
            .tools(&lead())
            .iter()
            .map(|tool| tool.name().to_string())
            .collect()
    }

    const ALL: [&str; 12] = [
        "read",
        "write",
        "edit",
        "shell",
        "grep",
        "task",
        "agent_wait",
        "worktree_apply",
        "run_code",
        "job",
        "mcp__github__create_issue",
        "report",
    ];

    #[test]
    fn a_coordinating_lead_keeps_the_team_tools_and_what_looks_things_up() {
        let coordinator = Arc::new(Coordinator::new(false));
        let tools = LeadTools::new(Arc::new(Fixed(ALL.to_vec())), coordinator.clone(), None);
        assert_eq!(names(&tools), ALL);

        assert!(coordinator.set(true));
        assert_eq!(
            names(&tools),
            ["read", "grep", "task", "agent_wait", "worktree_apply"]
        );
        assert!(coordinator.set(false));
        assert_eq!(names(&tools), ALL);
    }

    #[test]
    fn a_typed_run_gets_its_own_report_tool_in_either_mode() {
        let coordinator = Arc::new(Coordinator::new(true));
        let typed = Arc::new(Typed::new(json!({ "type": "object" })).unwrap());
        let tools = LeadTools::new(Arc::new(Fixed(ALL.to_vec())), coordinator.clone(), Some(typed));
        let offered = tools.tools(&lead());
        assert_eq!(offered.iter().filter(|tool| tool.name() == "report").count(), 1);
        // The one offered is the run's: its schema asks for `result`.
        let report = offered.iter().find(|tool| tool.name() == "report").unwrap();
        assert_eq!(report.schema()["required"], json!(["result"]));
        assert!(!names(&tools).contains(&"write".to_string()));
        coordinator.set(false);
        assert_eq!(names(&tools).iter().filter(|name| *name == "report").count(), 1);
        assert!(names(&tools).contains(&"write".to_string()));
    }

    #[test]
    fn watchers_hear_changes_only_and_a_restore_is_silent() {
        let coordinator = Coordinator::new(false);
        let heard = Arc::new(Mutex::new(Vec::new()));
        let sink = heard.clone();
        coordinator.on_change(Arc::new(move |on| sink.lock().unwrap().push(on)));
        assert!(!coordinator.set(false));
        assert!(coordinator.set(true));
        assert!(!coordinator.set(true));
        coordinator.restore(false);
        assert!(!coordinator.is_on());
        assert!(coordinator.set(true));
        assert_eq!(*heard.lock().unwrap(), [true, true]);
    }

    #[test]
    fn the_recorded_mode_is_the_last_coordinator_entry_of_the_branch() {
        let entry_of = |n: usize, mode: &str| Entry {
            id: n.to_string(),
            parent_id: None,
            timestamp: String::new(),
            kind: EntryKind::ModeChange {
                mode: mode.to_string(),
            },
        };
        assert_eq!(recorded(&[]), None);
        assert_eq!(recorded(&[entry_of(1, "plan")]), None);
        let branch = [
            entry_of(1, &entry(true)),
            entry_of(2, "auto"),
            entry_of(3, &entry(false)),
            entry_of(4, "plan"),
        ];
        assert_eq!(recorded(&branch), Some(false));
        assert_eq!(recorded(&branch[..2]), Some(true));
        assert_eq!(entry(true), "coordinator:on");
    }
}
