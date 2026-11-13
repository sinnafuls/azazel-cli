//! Built-in tools: file access, search, shell, jobs, todos, user questions, web, LSP and skills,
//! and the addresses `read` resolves for sessions, jobs and GitHub.

pub mod ast_grep;
pub mod check;
pub mod format;
#[cfg(feature = "lsp")]
pub mod lsp;
pub mod misc;
pub mod notebook;
#[cfg(feature = "pdf")]
pub mod pdf;
pub mod read;
pub mod scan;
pub mod schemes;
pub mod search;
pub mod shell;
pub mod util;
pub mod web;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_core::config::ReadGuard;
use azazel_core::process::{AgentEnv, Shell};
use azazel_core::{
    ArtifactScheme, ArtifactStore, Command, CommandHandler, CommandOutcome, EventKind, ExtCtx, ExtError,
    Extension, FileTracker, Registrar, Schemes,
};

pub use ast_grep::AstGrepTool;
pub use misc::{AskUserTool, TodoItem, TodoStatus, TodoStore, TodoTool, todo_entry};
pub use read::ReadTool;
pub use search::{GlobTool, GrepTool};
pub use shell::{Foreground, JobEntry, JobTool, Jobs, ShellTool};
pub use write::{EditTool, WriteTool};

/// Registers the core tools (and `ast_grep` when the ast-grep program is installed), the
/// `artifact://`, `session://` and `job://` schemes (and `pr://` and `issue://` when the `gh`
/// program is installed), `/allow-edit`, and the services they share: [`FileTracker`],
/// [`ArtifactStore`], [`Jobs`], [`Foreground`], [`AgentEnv`] and [`TodoStore`].
pub struct CoreToolsExt {
    pub shell: Shell,
    pub shell_timeout: Duration,
    pub artifacts_dir: PathBuf,
    /// `tools.read_guard`: what an agent must have read of a file before it changes it.
    pub read_guard: ReadGuard,
}

/// `/allow-edit <path>`: lets the next edit of one file through the read guard.
struct AllowEdit(Arc<FileTracker>);

#[async_trait]
impl CommandHandler for AllowEdit {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let name = args.trim();
        if name.is_empty() {
            return CommandOutcome::Error("usage: /allow-edit <path>".to_string());
        }
        let path = util::resolve_path(&ctx.cwd, name);
        if !path.is_file() {
            return CommandOutcome::Error(format!("{}: no such file", path.display()));
        }
        self.0.allow_once(&path);
        CommandOutcome::Show(format!(
            "the next edit of {} is allowed whatever the agent has read of it",
            util::display_path(&ctx.cwd, &path)
        ))
    }
}

impl Extension for CoreToolsExt {
    fn id(&self) -> &str {
        "core-tools"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let schemes = r
            .services()
            .get::<Schemes>()
            .ok_or_else(|| ExtError("the scheme registry service is missing".to_string()))?;
        let tracker = Arc::new(FileTracker::new());
        let artifacts = Arc::new(ArtifactStore::new(self.artifacts_dir.clone()));
        let jobs = Arc::new(Jobs::new());
        let env = Arc::new(AgentEnv::default());
        let todos = Arc::new(TodoStore::default());
        let foreground = Arc::new(Foreground::new());
        r.service(foreground.clone());
        r.service(tracker.clone());
        r.service(artifacts.clone());
        r.service(jobs.clone());
        r.service(env.clone());
        r.service(todos.clone());
        r.scheme(Arc::new(ArtifactScheme(artifacts)));
        r.scheme(Arc::new(schemes::SessionScheme));
        r.scheme(Arc::new(schemes::JobScheme(jobs.clone())));
        if let Some(gh) = schemes::gh_program() {
            r.scheme(Arc::new(schemes::GhScheme::pull_requests(gh.clone())));
            r.scheme(Arc::new(schemes::GhScheme::issues(gh)));
        }

        r.command(Command {
            name: "allow-edit".to_string(),
            description: "Let the next edit of a file through, whatever the agent has read of it".to_string(),
            argument_hint: Some("<path>".to_string()),
            handler: Arc::new(AllowEdit(tracker.clone())),
        });

        r.tool(Arc::new(ReadTool::new(tracker.clone(), schemes)));
        r.tool(Arc::new(WriteTool::new(tracker.clone(), self.read_guard)));
        r.tool(Arc::new(EditTool::new(tracker.clone(), self.read_guard)));
        r.tool(Arc::new(GlobTool));
        r.tool(Arc::new(GrepTool));
        r.tool(Arc::new(ShellTool::new(
            self.shell.clone(),
            self.shell_timeout,
            jobs.clone(),
            env,
            foreground,
        )));
        r.tool(Arc::new(JobTool::new(jobs)));
        r.tool(Arc::new(TodoTool::new(todos)));
        r.on(EventKind::Context, Arc::new(misc::PlanReminder));
        r.tool(Arc::new(AskUserTool));
        if let Some(program) = ast_grep::program() {
            r.tool(Arc::new(AstGrepTool::new(program, tracker, self.read_guard)));
        }
        Ok(())
    }
}
