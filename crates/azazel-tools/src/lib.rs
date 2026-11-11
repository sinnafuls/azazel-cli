//! Built-in tools: file access, search, shell, jobs, todos, user questions, web, LSP and skills.

#[cfg(feature = "lsp")]
pub mod lsp;
pub mod misc;
pub mod read;
pub mod search;
pub mod shell;
pub mod util;
pub mod web;
pub mod write;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_core::process::{AgentEnv, Shell};
use azazel_core::{ArtifactScheme, ArtifactStore, ExtError, Extension, FileTracker, Registrar, Schemes};

pub use misc::{AskUserTool, TodoItem, TodoStatus, TodoStore, TodoTool};
pub use read::ReadTool;
pub use search::{GlobTool, GrepTool};
pub use shell::{JobTool, Jobs, ShellTool};
pub use write::{EditTool, WriteTool};

/// Registers the core tools, the `artifact://` scheme, and the services they share:
/// [`FileTracker`], [`ArtifactStore`], [`Jobs`], [`AgentEnv`] and [`TodoStore`].
pub struct CoreToolsExt {
    pub shell: Shell,
    pub shell_timeout: Duration,
    pub artifacts_dir: PathBuf,
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
        r.service(tracker.clone());
        r.service(artifacts.clone());
        r.service(jobs.clone());
        r.service(env.clone());
        r.service(todos.clone());
        r.scheme(Arc::new(ArtifactScheme(artifacts)));

        r.tool(Arc::new(ReadTool::new(tracker.clone(), schemes)));
        r.tool(Arc::new(WriteTool::new(tracker.clone())));
        r.tool(Arc::new(EditTool::new(tracker)));
        r.tool(Arc::new(GlobTool));
        r.tool(Arc::new(GrepTool));
        r.tool(Arc::new(ShellTool::new(
            self.shell.clone(),
            self.shell_timeout,
            jobs.clone(),
            env,
        )));
        r.tool(Arc::new(JobTool::new(jobs)));
        r.tool(Arc::new(TodoTool::new(todos)));
        r.tool(Arc::new(AskUserTool));
        Ok(())
    }
}
