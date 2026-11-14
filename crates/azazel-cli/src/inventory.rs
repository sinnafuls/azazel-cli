//! What a default build offers by name: slash commands and tools. `cargo xtask docs-check`
//! compares the lists with the docs, so they come from the same registries the program uses.

use std::path::Path;

use anyhow::Result;
use azazel_agent::AgentInfo;
use clap::Parser;

use crate::app::{App, discover_paths};
use crate::cli::Cli;

/// The names of a default build with default settings.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inventory {
    /// Slash commands: the UI's own and those extensions register, without the ones that depend
    /// on the machine (skills, custom commands, MCP prompts).
    pub commands: Vec<String>,
    /// Tools the lead of a new session is offered.
    pub tools: Vec<String>,
    /// Tools that are registered only under a condition, with the condition.
    pub conditional_tools: Vec<(String, &'static str)>,
}

/// The slash commands the terminal UI answers by itself, aliases included.
pub fn builtin_command_names() -> Vec<&'static str> {
    crate::tui::builtin_command_names().collect()
}

/// The tools a default build has that a new session with default settings does not offer.
fn conditional_tools() -> Vec<(String, &'static str)> {
    let mut tools = vec![(azazel_tools::ast_grep::NAME.to_string(), "ast-grep is installed")];
    tools.extend(
        azazel_team::goal::GOAL_TOOLS
            .iter()
            .map(|name| (name.to_string(), "a goal is open")),
    );
    tools.push((
        azazel_team::REPORT_TOOL.to_string(),
        "the agent owes a typed result",
    ));
    #[cfg(feature = "mcp")]
    tools.push((
        azazel_mcp::TOOL_SEARCH.to_string(),
        "there are too many MCP tools to list",
    ));
    #[cfg(feature = "media")]
    tools.extend(
        azazel_media::MediaKind::ALL
            .iter()
            .map(|kind| (kind.tool_name().to_string(), "a media backend is configured")),
    );
    tools
}

/// Builds the application as `azazel` would for a new project, with `home` as its home directory
/// and `project` as the working directory (both should be empty), and lists what it registered.
/// Nothing is asked of the network and no session is opened.
pub async fn default_app(home: &Path, project: &Path) -> Result<Inventory> {
    let cli = Cli::parse_from([
        "azazel".as_ref(),
        "--home".as_ref(),
        home.as_os_str(),
        "--cwd".as_ref(),
        project.as_os_str(),
    ]);
    let paths = discover_paths(&cli)?;
    let app = App::build(&cli, paths, false, &[]).await?;
    let lead = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: String::new(),
    };
    let mut commands: Vec<String> = builtin_command_names()
        .into_iter()
        .map(str::to_string)
        .chain(app.registry.registered_commands())
        .collect();
    commands.sort();
    commands.dedup();
    Ok(Inventory {
        commands,
        tools: app.registry.tools.all_names(&lead),
        conditional_tools: conditional_tools(),
    })
}
