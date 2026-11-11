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

    /// Run one prompt without the interactive UI and print the result
    #[arg(short = 'p', long = "print", value_name = "PROMPT")]
    pub print: Option<String>,

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
    #[arg(short = 'c', long = "continue")]
    pub resume_latest: bool,

    /// Resume the session with this id (a unique prefix is enough)
    #[arg(short = 'r', long, value_name = "ID")]
    pub resume: Option<String>,

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
