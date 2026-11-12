//! Command-line definition.

use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputFormat {
    /// The final assistant text.
    Text,
    /// One JSON event per line.
    Json,
}

#[cfg(feature = "lua")]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum ExtAction {
    /// Write the type declarations of the `azazel` global to ~/.azazel/types/azazel.d.luau
    Types,
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum PkgAction {
    /// Install a package: a git URL, github:<owner>/<repo>[@ref], a directory, or a registry name
    Add {
        source: String,
        /// The name to install it under instead of its own
        #[arg(long, value_name = "NAME")]
        name: Option<String>,
    },
    /// Show the installed packages
    List,
    /// Fetch the newest version of one package, or of all
    Update { name: Option<String> },
    /// Uninstall a package
    Remove { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SkillsAction {
    /// Install a package of skills, or offer one skill of a package to the model: <package>/<skill>
    Add { source: String },
    /// Show every skill available in this project
    List,
    /// Uninstall the package of that name
    Remove { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SessionsAction {
    /// The sessions of this project, most recently used first
    List {
        /// Of every project
        #[arg(long)]
        all_projects: bool,
        /// One JSON object per session
        #[arg(long)]
        json: bool,
    },
    /// Sessions whose messages contain a text (case-insensitive)
    Search {
        query: String,
        /// In every project
        #[arg(long)]
        all_projects: bool,
        /// One JSON object per matching line
        #[arg(long)]
        json: bool,
    },
    /// Delete a session: its file and its sub-agents' transcripts
    Delete {
        /// A path to its file, or its id (a unique prefix is enough)
        session: String,
        /// Do not ask first
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// Delete the unpinned sessions not used for a while
    Prune {
        /// How long unused: 30d, 12h, 2w
        #[arg(long, value_name = "AGE", default_value = "30d")]
        older_than: String,
        /// In every project
        #[arg(long)]
        all_projects: bool,
        /// Only list what would be deleted
        #[arg(long)]
        dry_run: bool,
    },
    /// Write a session to a file (the latest of this project when no id is given)
    Export {
        session: Option<String>,
        /// markdown, html or jsonl; else taken from the file name, else markdown
        #[arg(long, value_enum)]
        format: Option<crate::export::Format>,
        /// The file to write, or - for standard output
        #[arg(short = 'o', long = "output-file", value_name = "FILE")]
        file: Option<PathBuf>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Subcommand)]
pub enum AuthAction {
    /// Where each provider's credential comes from, without printing it
    Status {
        /// One JSON object per provider
        #[arg(long)]
        json: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Sub {
    /// Choose the default model and create this project's config file
    Init,
    /// Check the installation and this project's setup
    Doctor,
    /// Sign in to a provider, or store its API key, in ~/.azazel/auth.json
    Login {
        /// The provider; asked for when left out
        provider: Option<String>,
        /// Store an API key even when the provider also has a sign-in
        #[arg(long)]
        key: bool,
    },
    /// Forget the stored key or login of a provider
    Logout { provider: String },
    /// Credentials of the providers
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// List the models of the providers you have a credential for
    Models {
        /// Only models whose name matches this, fuzzily
        query: Option<String>,
        /// Only this provider's models
        #[arg(long, value_name = "ID")]
        provider: Option<String>,
        /// Every model in the catalogue, credential or not
        #[arg(long)]
        all: bool,
        /// One JSON object per model
        #[arg(long)]
        json: bool,
    },
    /// Print a completion script for a shell
    Completions { shell: clap_complete::Shell },
    /// What changed in each release, newest first
    Changelog {
        /// Only the releases newer than this version
        #[arg(long, value_name = "VERSION")]
        since: Option<String>,
    },
    /// Sessions, prompts, tokens and cost per day, model, project and tool
    Stats {
        /// How many days back, today included
        #[arg(long, value_name = "N", default_value_t = 30)]
        days: u32,
        /// One JSON object
        #[arg(long)]
        json: bool,
    },
    /// List, search, delete, prune and export sessions
    Sessions {
        #[command(subcommand)]
        action: SessionsAction,
    },
    /// Write a session to a file: Markdown, HTML or JSON lines
    Export {
        /// A path to its file, or its id; the latest session of this project when left out
        session: Option<String>,
        /// markdown, html or jsonl; else taken from the file name, else markdown
        #[arg(long, value_enum)]
        format: Option<crate::export::Format>,
        /// The file to write, or - for standard output
        #[arg(short = 'o', long = "output-file", value_name = "FILE")]
        file: Option<PathBuf>,
    },
    /// Install the newest release from GitHub over this binary
    Update {
        /// Only report whether a newer release exists
        #[arg(long)]
        check: bool,
    },
    /// Tools for writing Luau extensions
    #[cfg(feature = "lua")]
    Ext {
        #[command(subcommand)]
        action: ExtAction,
    },
    /// Work on an objective until the checker verifies it; exits 0 when done, 2 when not
    Goal {
        /// What should be true when the work is finished
        objective: String,
        /// Failed attempts to finish after which the goal fails
        #[arg(long, value_name = "N")]
        max_iterations: Option<u32>,
        /// Stop when this much has been spent
        #[arg(long, value_name = "USD")]
        max_cost: Option<f64>,
        /// Stop after this long
        #[arg(long, value_name = "N")]
        max_minutes: Option<u64>,
    },
    /// Manage MCP servers
    #[cfg(feature = "mcp")]
    Mcp {
        #[command(subcommand)]
        action: McpAction,
    },
    /// Install and manage packages of skills, agents, commands and extensions
    Pkg {
        #[command(subcommand)]
        action: PkgAction,
    },
    /// Install and list skills
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// Serve one session to another program: JSON lines on standard input and output
    Rpc,
    /// Show, search and forget what is remembered about this project
    #[cfg(feature = "memory")]
    Memory {
        #[command(subcommand)]
        action: MemoryAction,
    },
    /// Rebuild the index of this project's code that `semantic_search` uses
    #[cfg(feature = "memory")]
    Index,
    /// Manage collections of documents that `semantic_search` can look through
    #[cfg(feature = "memory")]
    Rag {
        #[command(subcommand)]
        action: RagAction,
    },
}

#[cfg(feature = "memory")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum MemoryAction {
    /// Every memory of this project and the global ones
    List,
    /// The memories closest to a query
    Search {
        query: String,
        #[arg(long, value_name = "N", default_value_t = 8)]
        limit: usize,
    },
    /// Delete a memory by its id
    Forget { id: String },
    /// Every memory as JSON lines
    Export,
}

#[cfg(feature = "memory")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RagAction {
    /// Index files or directories into the collection `docs:<name>`
    Add {
        name: String,
        #[arg(required = true, value_name = "PATH")]
        paths: Vec<PathBuf>,
    },
    /// The document collections and their sizes
    List,
    /// Delete a collection from the index
    Remove { name: String },
}

#[cfg(feature = "mcp")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum McpAction {
    /// Show every configured server and whether it connects
    List,
    /// Add a server: `azazel mcp add <name> -- <command> [args…]` or `azazel mcp add <name> --url <url>`
    Add {
        name: String,
        /// Write to this project's .azazel/mcp.json instead of ~/.azazel/mcp.json
        #[arg(long)]
        project: bool,
        /// A remote server's address (streamable HTTP)
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// A header sent with every request to --url; repeat for more
        #[arg(long = "header", value_name = "NAME=VALUE")]
        headers: Vec<String>,
        /// The command that starts a local server, after `--`
        #[arg(last = true)]
        command: Vec<String>,
    },
    /// Remove a server from the config file that defines it
    Remove { name: String },
    /// Log in to a remote server in the browser
    Login { name: String },
    /// Forget the stored login of a remote server
    Logout { name: String },
}

#[derive(Debug, Parser)]
#[command(name = "azazel", version, about = "A team of coding agents in your terminal")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Sub>,

    /// Run the prompt without the interactive UI and print the result
    #[arg(short = 'p', long = "print")]
    pub print: bool,

    /// The prompt: the first message of the session, or with -p the whole run; `--` before it
    /// when it starts like a subcommand
    #[arg(value_name = "PROMPT")]
    pub prompt: Vec<String>,

    /// Model to use, as provider/model[:thinking] or @role; repeat to add fallbacks
    #[arg(long = "model", value_name = "SPEC", global = true)]
    pub model: Vec<String>,

    /// Thinking level: off, minimal, low, medium, high, xhigh, max
    #[arg(long, value_name = "LEVEL", global = true)]
    pub thinking: Option<String>,

    /// Permission mode: plan, ask, accept-edits, auto
    #[arg(long, value_name = "MODE", global = true)]
    pub mode: Option<String>,

    /// Output of print mode
    #[arg(long, value_enum, default_value = "text", global = true)]
    pub output: OutputFormat,

    /// Continue the most recent session of this project
    #[arg(short = 'c', long = "continue", conflicts_with_all = ["session", "fork"])]
    pub resume_latest: bool,

    /// Open a session: a path to its .jsonl file, or its id (a unique prefix is enough)
    #[arg(
        short = 'r',
        long = "session",
        alias = "resume",
        value_name = "PATH|ID",
        conflicts_with = "fork"
    )]
    pub session: Option<String>,

    /// Continue a copy of a session, given like --session, leaving the original as it is
    #[arg(long, value_name = "PATH|ID")]
    pub fork: Option<String>,

    /// Name the session
    #[arg(long, value_name = "NAME")]
    pub name: Option<String>,

    /// Keep this run's sessions in DIR instead of the project's own directory
    #[arg(long = "session-dir", value_name = "DIR", global = true)]
    pub session_dir: Option<PathBuf>,

    /// Keep nothing of this run: no session file, no checkpoints
    #[arg(long = "no-session")]
    pub no_session: bool,

    /// Let agents work in this directory too; repeat for more
    #[arg(long = "add-dir", value_name = "DIR", global = true)]
    pub add_dir: Vec<PathBuf>,

    /// Change one setting for this run, as in config.toml: --set limits.max_turns=20
    #[arg(long = "set", value_name = "TABLE.KEY=VALUE", global = true)]
    pub set: Vec<String>,

    /// Offer only these tools (comma-separated; globs such as mcp__github__* work)
    #[arg(
        long,
        value_name = "NAMES",
        value_delimiter = ',',
        conflicts_with = "no_tools"
    )]
    pub tools: Option<Vec<String>>,

    /// Offer no tools at all
    #[arg(long = "no-tools")]
    pub no_tools: bool,

    /// Never offer these tools (comma-separated; globs work)
    #[arg(long = "disallowed-tools", value_name = "NAMES", value_delimiter = ',')]
    pub disallowed_tools: Vec<String>,

    /// Replace the lead's built-in instructions with this text, or with a file's as @FILE
    #[arg(long = "system-prompt", value_name = "TEXT|@FILE")]
    pub system_prompt: Option<String>,

    /// Add this text, or a file's as @FILE, to the lead's instructions
    #[arg(long = "append-system-prompt", value_name = "TEXT|@FILE")]
    pub append_system_prompt: Option<String>,

    /// Load no Luau extensions
    #[arg(long = "no-extensions")]
    pub no_extensions: bool,

    /// Load no skills
    #[arg(long = "no-skills")]
    pub no_skills: bool,

    /// Run no hooks
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,

    /// Read no AGENTS.md or CLAUDE.md files
    #[arg(long = "no-context-files")]
    pub no_context_files: bool,

    /// Connect to no MCP servers
    #[arg(long = "no-mcp")]
    pub no_mcp: bool,

    /// Use no memory
    #[arg(long = "no-memory")]
    pub no_memory: bool,

    /// All of the --no-* switches above
    #[arg(long)]
    pub bare: bool,

    /// Requests the lead may make per prompt
    #[arg(long = "max-turns", value_name = "N", global = true)]
    pub max_turns: Option<u32>,

    /// Stop every agent once the run has spent this much
    #[arg(long = "max-cost", value_name = "USD")]
    pub max_cost: Option<f64>,

    /// Work in this directory instead of the current one
    #[arg(long, value_name = "DIR", global = true)]
    pub cwd: Option<PathBuf>,

    /// Trust this project's configuration for this run
    #[arg(long, global = true)]
    pub trust: bool,

    /// API key for the provider of the first --model, for this run only
    #[arg(long = "api-key", value_name = "KEY")]
    pub api_key: Option<String>,

    /// Write debug-level logs
    #[arg(long)]
    pub debug: bool,
}

impl Cli {
    /// The prompt as one text; empty when none was given.
    pub fn prompt_text(&self) -> String {
        self.prompt.join(" ")
    }

    /// Whether this run opens the interactive UI.
    pub fn interactive(&self) -> bool {
        !self.print && self.command.is_none()
    }

    /// Whether a discovery switch (`--no-<what>` or `--bare`) is on.
    pub fn skips(&self, what: Discovery) -> bool {
        self.bare
            || match what {
                Discovery::Extensions => self.no_extensions,
                Discovery::Skills => self.no_skills,
                Discovery::Hooks => self.no_hooks,
                Discovery::ContextFiles => self.no_context_files,
                Discovery::Mcp => self.no_mcp,
                Discovery::Memory => self.no_memory,
            }
    }
}

/// What the `--no-*` switches turn off.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Discovery {
    Extensions,
    Skills,
    Hooks,
    ContextFiles,
    Mcp,
    Memory,
}
