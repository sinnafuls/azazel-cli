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
    /// Install a package: a git URL, github:<owner>/<repo>[@ref], npm:<name>[@version], a directory,
    /// or a registry name
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

#[cfg(feature = "detach")]
#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum ScheduleAction {
    /// Add a schedule: `add "0 9 * * 1-5" "<prompt>"`, `add every 10m "<prompt>"`,
    /// `add at +2h "<prompt>"`, or `add --on ci-failure "<prompt>"`
    Add {
        /// Run when this happens instead of at a time: ci-failure, or file:<glob>
        #[arg(long, value_name = "EVENT")]
        on: Option<String>,
        /// Send the prompt to this session every time, instead of starting a new one
        #[arg(long, value_name = "ID", conflicts_with = "new")]
        session: Option<String>,
        /// Start a new background session every time (what a time does unless --session is given)
        #[arg(long)]
        new: bool,
        /// When to run (five cron fields, `every 10m`, `at +2h`), then the prompt as the last word
        #[arg(required = true, value_name = "WHEN… PROMPT")]
        spec: Vec<String>,
    },
    /// The schedules of this project, when each runs next and what it last did
    List {
        /// One JSON object per schedule
        #[arg(long)]
        json: bool,
    },
    /// Remove a schedule, from the system's scheduler too
    Rm {
        /// Its id as `azazel schedule list` shows it (the start of it is enough)
        id: String,
    },
    /// Run a schedule now
    Run {
        /// Its id as `azazel schedule list` shows it (the start of it is enough)
        id: Option<String>,
        /// Run only what is due now: every schedule of the project, or the one named
        #[arg(long)]
        due: bool,
        /// How the system's scheduler calls it: run only when due, and print nothing
        #[arg(long, hide = true)]
        scheduled: bool,
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

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum SecretsAction {
    /// Store a credential that is added to the agents' requests to one host
    Add {
        /// A name for the service
        name: String,
        /// The host the requests go to: api.example.com, *.example.com or localhost:8080
        #[arg(long)]
        host: String,
        /// A request header, as "Name: value"; - reads it from the terminal
        #[arg(long, value_name = "HEADER", allow_hyphen_values = true)]
        header: Option<String>,
        /// A query parameter, as "name=value"; - reads it from the terminal
        #[arg(long, value_name = "PARAMETER", allow_hyphen_values = true)]
        query: Option<String>,
        /// Basic authentication, as "user:password"; - reads it from the terminal
        #[arg(long, value_name = "LOGIN", allow_hyphen_values = true)]
        basic: Option<String>,
    },
    /// Store the login of a site, which the browser tool types into its sign-in page
    AddSite {
        /// The address of the sign-in page; * stands for any run of characters in its path
        url_pattern: String,
        #[arg(long)]
        username: String,
        /// Asked for at the terminal when left out
        #[arg(long, allow_hyphen_values = true)]
        password: Option<String>,
        /// A name for the login; taken from the host of the page when left out
        #[arg(long)]
        name: Option<String>,
    },
    /// What is stored, without the secrets
    List {
        /// One JSON object per entry
        #[arg(long)]
        json: bool,
    },
    /// Forget a stored credential or login
    #[command(alias = "remove")]
    Rm { name: String },
}

#[derive(Debug, Clone, PartialEq, Subcommand)]
pub enum Sub {
    /// Choose the default model and create this project's config file
    Init,
    /// Check the installation and this project's setup
    Doctor,
    /// Write a bug report (version, system, doctor, settings without secrets, log) as a zip
    Bug {
        /// What went wrong
        #[arg(value_name = "DESCRIPTION")]
        description: Vec<String>,
        /// Include the latest session of this project, secrets hidden
        #[arg(long)]
        transcript: bool,
        /// Include this session instead: a path to its file, or its id
        #[arg(long, value_name = "PATH|ID")]
        session: Option<String>,
        /// Open an issue on azazel's repository with the summary, through `gh`
        #[arg(long)]
        issue: bool,
    },
    /// Sign in to a provider, or store its API key, in ~/.azazel/auth.json
    Login {
        /// The provider; asked for when left out
        provider: Option<String>,
        /// Store an API key even when the provider also has a sign-in
        #[arg(long)]
        key: bool,
        /// Keep it as another account of the provider, under this label
        #[arg(long = "as", value_name = "LABEL")]
        label: Option<String>,
    },
    /// Forget the stored key or login of a provider
    Logout {
        provider: String,
        /// The account to forget; the default account when left out
        #[arg(long, value_name = "LABEL")]
        account: Option<String>,
    },
    /// Credentials of the providers
    Auth {
        #[command(subcommand)]
        action: AuthAction,
    },
    /// Credentials the agents use without seeing them: API keys by host, logins by site
    Secrets {
        #[command(subcommand)]
        action: SecretsAction,
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
    /// What your Claude and ChatGPT subscriptions have left, and when each window resets
    Usage {
        /// One JSON array, an object per account
        #[arg(long)]
        json: bool,
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
    /// What the last request of a session was made of: tokens by category
    Context {
        /// A path to its file, or its id; the latest session of this project when left out
        session: Option<String>,
        /// One JSON object
        #[arg(long)]
        json: bool,
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
    /// Bring over what pi, omp, jcode, Claude Code and Codex CLI hold: models, keys, skills,
    /// commands, agents, MCP servers, rules, settings, chats and memory
    Migrate {
        /// Where from: pi, omp, jcode, claude, codex or all; several separated by commas
        #[arg(long, value_name = "SOURCE", value_delimiter = ',')]
        from: Vec<String>,
        /// Only these kinds: models, keys, skills, commands, agents, mcp, rules, settings, chats, memory
        #[arg(long, value_name = "KINDS", value_delimiter = ',')]
        only: Vec<String>,
        /// Say what would be brought over, and write nothing
        #[arg(long)]
        dry_run: bool,
        /// Replace what is already there instead of leaving it
        #[arg(long)]
        overwrite: bool,
        /// The project whose own files (skills, commands, MCP servers) are brought over; this one when left out
        #[arg(long, value_name = "DIR")]
        project: Option<PathBuf>,
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
    /// Show the rule files of this project and test one
    Rules {
        #[command(subcommand)]
        action: RulesAction,
    },
    /// Serve one session to another program: JSON lines on standard input and output
    Rpc {
        /// Instead of a session of its own, connect standard input and output to this
        /// background session
        #[arg(long, value_name = "ID")]
        attach: Option<String>,
    },
    /// Run a prompt in a background session and print the session's id
    #[cfg(feature = "detach")]
    Bg {
        /// A name for this request: the same command sent again starts nothing and prints
        /// the id of the session the first one started
        #[arg(long, value_name = "ID")]
        request_id: Option<String>,
        /// What to do
        #[arg(required = true, value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// List the background sessions that are running
    #[cfg(feature = "detach")]
    Ps {
        /// One JSON object per session
        #[arg(long)]
        json: bool,
    },
    /// Show what a background session has done so far
    #[cfg(feature = "detach")]
    Logs {
        /// Its id as `azazel ps` shows it (the start of it is enough), or its name
        id: String,
        /// Keep printing as the session goes on
        #[arg(short = 'f', long)]
        follow: bool,
        /// The frames as they were written, one JSON object per line
        #[arg(long)]
        json: bool,
    },
    /// Open a background session in the UI; --plain shows it as lines instead
    #[cfg(feature = "detach")]
    Attach {
        /// Its id as `azazel ps` shows it (the start of it is enough), or its name
        id: String,
        /// End the background session after its current work, which disconnects every client,
        /// and continue it here
        #[arg(long)]
        take: bool,
        /// With --take: stop the current work instead of waiting for it
        #[arg(long, requires = "take")]
        now: bool,
    },
    /// Stop a background session
    #[cfg(feature = "detach")]
    Kill {
        /// Its id as `azazel ps` shows it (the start of it is enough), or its name
        id: String,
    },
    /// The process of one background session; `azazel bg` starts it
    #[cfg(feature = "detach")]
    #[command(hide = true)]
    Host {
        /// The id the session is listed under
        #[arg(long)]
        id: String,
        /// How long to stay with nothing to do and nobody attached
        #[arg(long, value_name = "SECONDS", default_value_t = 600)]
        idle_secs: u64,
        /// The name `azazel bg` was given for the first message
        #[arg(long, value_name = "ID")]
        request_id: Option<String>,
        /// The first message; without one the session waits for a client
        #[arg(value_name = "PROMPT")]
        prompt: Vec<String>,
    },
    /// Run prompts at a time or when something happens: add, list, rm, run
    #[cfg(feature = "detach")]
    Schedule {
        #[command(subcommand)]
        action: ScheduleAction,
    },
    /// Serve editors that speak the Agent Client Protocol, on standard input and output
    #[cfg(feature = "acp")]
    Acp,
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
    /// Let servers this project defines start, now and in later sessions
    Approve {
        /// The servers; with --all, every one that waits for approval
        names: Vec<String>,
        #[arg(long)]
        all: bool,
    },
    /// Keep a server this project defines from starting
    Reject { name: String },
    /// Serve azazel to another MCP client on standard input and output
    Serve,
    /// Copy the servers another client is configured with into ~/.azazel/mcp.json
    Import {
        /// claude-desktop, claude-code, cursor, vscode or windsurf; all of them when left out
        client: Option<String>,
        /// Import every server found without asking; names already configured are left alone
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Subcommand)]
pub enum RulesAction {
    /// Show every rule, where it comes from and when it applies
    List,
    /// Say whether a rule file would apply to a path, a text or a tool
    Test {
        /// The rule file
        rule: PathBuf,
        /// Match the rule's trigger against this file's text; `-` reads standard input
        #[arg(long, value_name = "FILE|-")]
        text: Option<String>,
        /// Match the rule's paths against this path
        #[arg(long, value_name = "PATH")]
        path: Option<String>,
        /// Say whether the rule checks calls to this tool
        #[arg(long, value_name = "NAME")]
        tool: Option<String>,
    },
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

    /// Permission mode: plan, manual, accept-edits, auto, bypass
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

    /// With -c or --session: go on with the run the session's last process did not finish,
    /// instead of sending a prompt
    #[arg(long = "continue-run")]
    pub continue_run: bool,

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

    /// Load this Luau script, or this directory laid out like a package, for this run; repeat
    /// for more
    #[arg(short = 'e', long = "extension", value_name = "PATH", conflicts_with = "ssh")]
    pub extension: Vec<PathBuf>,

    /// Add this skill, or this directory of skills, for this run; repeat for more
    #[arg(long = "skill", value_name = "PATH", conflicts_with = "ssh")]
    pub skill: Vec<PathBuf>,

    /// Connect to the MCP servers this file defines, for this run; repeat for more
    #[arg(long = "mcp-config", value_name = "FILE", conflicts_with = "ssh")]
    pub mcp_config: Vec<PathBuf>,

    /// Show this run in a theme: the name of a built-in or installed one, or a theme file
    #[arg(long, value_name = "NAME|FILE", global = true)]
    pub theme: Option<String>,

    /// Take this run's models from this provider only
    #[arg(long, value_name = "ID")]
    pub provider: Option<String>,

    /// Load no Luau extensions but those named with --extension
    #[arg(long = "no-extensions")]
    pub no_extensions: bool,

    /// Load no skills but those named with --skill
    #[arg(long = "no-skills")]
    pub no_skills: bool,

    /// Run no hooks
    #[arg(long = "no-hooks")]
    pub no_hooks: bool,

    /// Read no AGENTS.md or CLAUDE.md files and no rule files
    #[arg(long = "no-context-files")]
    pub no_context_files: bool,

    /// Connect to no MCP servers but those named with --mcp-config
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

    /// Service tier of the requests: priority (faster, costs more), standard, economy
    #[arg(long, value_name = "TIER", global = true)]
    pub tier: Option<String>,

    /// Coordinator mode: the lead plans and delegates, and gets no tool that edits or runs commands
    #[arg(long, global = true)]
    pub coordinator: bool,

    /// With -p: the run must end in JSON that matches this schema (a file, or the schema itself);
    /// only that JSON is printed
    #[arg(long = "json-schema", value_name = "FILE|JSON", requires = "print")]
    pub json_schema: Option<String>,

    /// Use the models of a saved preset ([presets.<name>]) for this run
    #[arg(long, value_name = "NAME", global = true)]
    pub preset: Option<String>,

    /// Work in this directory instead of the current one
    #[arg(long, value_name = "DIR", global = true)]
    pub cwd: Option<PathBuf>,

    /// Trust this project's configuration for this run
    #[arg(long, global = true)]
    pub trust: bool,

    /// Keep azazel's own files in this directory instead of ~/.azazel, as AZAZEL_HOME does. A
    /// process azazel starts for itself (a scheduled run, a background session) is told so.
    #[arg(long, value_name = "DIR", global = true, hide = true)]
    pub home: Option<PathBuf>,

    /// Work on another machine: azazel runs there, reached over SSH, and is shown here
    #[arg(long, value_name = "HOST", global = true)]
    pub ssh: Option<String>,

    /// With --ssh: the directory to work in on the other machine
    #[arg(long = "remote-dir", value_name = "DIR", requires = "ssh", global = true)]
    pub remote_dir: Option<String>,

    /// With --ssh: how azazel is started on the other machine
    #[arg(
        long = "remote-azazel",
        value_name = "COMMAND",
        requires = "ssh",
        default_value = "azazel",
        global = true
    )]
    pub remote_azazel: String,

    /// With --ssh: run the prompt in a background session there, which outlives the connection
    #[arg(long, requires = "ssh", conflicts_with = "attach")]
    pub detach: bool,

    /// With --ssh: open a background session of the other machine
    #[arg(long, value_name = "ID", requires = "ssh")]
    pub attach: Option<String>,

    /// Show a background session as lines you answer, not in the full UI
    #[arg(long, global = true)]
    pub plain: bool,

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

    /// Whether this run opens the interactive UI on a session of this process.
    pub fn interactive(&self) -> bool {
        !self.print && self.command.is_none() && self.ssh.is_none()
    }

    /// Whether this process is a background session, which keeps its session to itself.
    pub fn background(&self) -> bool {
        #[cfg(feature = "detach")]
        if matches!(self.command, Some(Sub::Host { .. })) {
            return true;
        }
        false
    }

    /// The options that shape a run whatever its session and prompt are, as arguments for
    /// another azazel that is to work the same way (`/bg`). The model, the thinking level and
    /// the permission mode are not among them: they change while a session runs, so whoever
    /// starts the other process says what they are now. An option added to [`Cli`] that
    /// belongs here has to be added here.
    #[cfg(feature = "detach")]
    pub fn carried(&self) -> Vec<std::ffi::OsString> {
        use std::ffi::OsString;
        let mut args: Vec<OsString> = Vec::new();
        let mut value = |flag: &str, value: OsString| args.extend([OsString::from(flag), value]);
        for assignment in &self.set {
            value("--set", assignment.into());
        }
        for dir in &self.add_dir {
            value("--add-dir", dir.into());
        }
        for path in &self.extension {
            value("--extension", path.into());
        }
        for path in &self.skill {
            value("--skill", path.into());
        }
        for file in &self.mcp_config {
            value("--mcp-config", file.into());
        }
        if let Some(provider) = &self.provider {
            value("--provider", provider.into());
        }
        if let Some(dir) = &self.session_dir {
            value("--session-dir", dir.into());
        }
        if let Some(dir) = &self.home {
            value("--home", dir.into());
        }
        if let Some(tools) = &self.tools {
            value("--tools", tools.join(",").into());
        }
        if !self.disallowed_tools.is_empty() {
            value("--disallowed-tools", self.disallowed_tools.join(",").into());
        }
        if let Some(text) = &self.system_prompt {
            value("--system-prompt", text.into());
        }
        if let Some(text) = &self.append_system_prompt {
            value("--append-system-prompt", text.into());
        }
        if let Some(turns) = self.max_turns {
            value("--max-turns", turns.to_string().into());
        }
        if let Some(cost) = self.max_cost {
            value("--max-cost", cost.to_string().into());
        }
        if let Some(tier) = &self.tier {
            value("--tier", tier.into());
        }
        if let Some(preset) = &self.preset {
            value("--preset", preset.into());
        }
        if let Some(key) = &self.api_key {
            value("--api-key", key.into());
        }
        let switches = [
            (self.no_tools, "--no-tools"),
            (self.no_extensions, "--no-extensions"),
            (self.no_skills, "--no-skills"),
            (self.no_hooks, "--no-hooks"),
            (self.no_context_files, "--no-context-files"),
            (self.no_mcp, "--no-mcp"),
            (self.no_memory, "--no-memory"),
            (self.bare, "--bare"),
            (self.coordinator, "--coordinator"),
            (self.trust, "--trust"),
            (self.debug, "--debug"),
        ];
        args.extend(
            switches
                .into_iter()
                .filter(|(on, _)| *on)
                .map(|(_, flag)| OsString::from(flag)),
        );
        args
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
