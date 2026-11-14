//! What another harness holds, in the one form everything is imported from, and the report of
//! what became of it.

use std::fmt;
use std::path::{Path, PathBuf};

use azazel_ai::{Block, ModelOverride, ProviderSettings, Usage, WireApi};

/// A harness whose data can be brought over.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Source {
    Pi,
    Omp,
    Jcode,
    Claude,
    Codex,
}

impl Source {
    pub const ALL: [Source; 5] = [
        Source::Pi,
        Source::Omp,
        Source::Jcode,
        Source::Claude,
        Source::Codex,
    ];

    /// The name on the command line, in file names and in what an imported session records.
    pub fn name(self) -> &'static str {
        match self {
            Source::Pi => "pi",
            Source::Omp => "omp",
            Source::Jcode => "jcode",
            Source::Claude => "claude",
            Source::Codex => "codex",
        }
    }

    /// What the harness is called in a sentence.
    pub fn label(self) -> &'static str {
        match self {
            Source::Pi => "pi",
            Source::Omp => "omp",
            Source::Jcode => "jcode",
            Source::Claude => "Claude Code",
            Source::Codex => "Codex CLI",
        }
    }

    pub fn parse(name: &str) -> Option<Source> {
        let name = name.trim().to_ascii_lowercase();
        Source::ALL
            .into_iter()
            .find(|source| source.name() == name)
            .or(match name.as_str() {
                "oh-my-pi" => Some(Source::Omp),
                "claude-code" => Some(Source::Claude),
                "codex-cli" => Some(Source::Codex),
                _ => None,
            })
    }
}

/// A kind of thing that can be brought over; what `--only` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Kind {
    Models,
    Keys,
    Skills,
    Commands,
    Agents,
    Mcp,
    Rules,
    Settings,
    Chats,
    Memory,
}

impl Kind {
    pub const ALL: [Kind; 10] = [
        Kind::Models,
        Kind::Keys,
        Kind::Skills,
        Kind::Commands,
        Kind::Agents,
        Kind::Mcp,
        Kind::Rules,
        Kind::Settings,
        Kind::Chats,
        Kind::Memory,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Kind::Models => "models",
            Kind::Keys => "keys",
            Kind::Skills => "skills",
            Kind::Commands => "commands",
            Kind::Agents => "agents",
            Kind::Mcp => "mcp",
            Kind::Rules => "rules",
            Kind::Settings => "settings",
            Kind::Chats => "chats",
            Kind::Memory => "memory",
        }
    }

    /// So many things of the kind, in words: `1 skill`, `3 MCP servers`.
    pub fn counted(self, count: usize) -> String {
        let (one, many) = match self {
            Kind::Models => ("model or role", "models and roles"),
            Kind::Keys => ("key or sign-in", "keys and sign-ins"),
            Kind::Skills => ("skill", "skills"),
            Kind::Commands => ("command", "commands"),
            Kind::Agents => ("agent", "agents"),
            Kind::Mcp => ("MCP server", "MCP servers"),
            Kind::Rules => ("rule file", "rule files"),
            Kind::Settings => ("setting", "settings"),
            Kind::Chats => ("chat", "chats"),
            Kind::Memory => ("memory", "memories"),
        };
        format!("{count} {}", if count == 1 { one } else { many })
    }

    /// What the kind covers, for a list of kinds to choose from.
    pub fn about(self) -> &'static str {
        match self {
            Kind::Models => "providers, their models, roles and fallbacks",
            Kind::Keys => "API keys; sign-ins are listed, never copied",
            Kind::Skills => "skills",
            Kind::Commands => "custom commands and prompt templates",
            Kind::Agents => "agent definitions",
            Kind::Mcp => "MCP servers",
            Kind::Rules => "rule and context files azazel does not read where they are",
            Kind::Settings => "settings that have an equivalent",
            Kind::Chats => "conversations of every project",
            Kind::Memory => "memories",
        }
    }

    pub fn parse(name: &str) -> Option<Kind> {
        let name = name.trim().to_ascii_lowercase();
        Kind::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// A key or token. Nothing prints it: its `Debug` form says only that it is one.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

    /// The secret itself, for the one place that stores it.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("<secret>")
    }
}

/// Where something belongs: with the user, or with one project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scope {
    Home,
    Project(PathBuf),
}

/// A provider the source defines, without its key.
#[derive(Debug, Clone, PartialEq)]
pub struct Provider {
    pub id: String,
    pub settings: ProviderSettings,
    /// `(model id, what the source says about it)`.
    pub models: Vec<(String, ModelOverride)>,
}

/// A role and the chain it resolves to, each entry written as azazel reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Role {
    pub name: String,
    pub chain: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// The azazel provider the key is for.
    pub provider: String,
    pub key: Secret,
    /// Where it was found, without the key: `auth.json`, `.env (OPENAI_API_KEY)`.
    pub origin: String,
}

/// A sign-in the source holds. It is never copied: a copied refresh token ends the login of the
/// program it was taken from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Login {
    /// The azazel provider to sign in to.
    pub provider: String,
    /// The account, as the source names it (an e-mail address, usually).
    pub who: Option<String>,
}

/// What a copied file holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Body {
    /// The file, or the directory with everything below it, as it is.
    Copy,
    /// Text converted for azazel.
    Text(String),
}

/// A skill, command, agent definition or rule file to copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileItem {
    pub kind: Kind,
    pub scope: Scope,
    pub from: PathBuf,
    /// Where it goes, below the `.azazel` directory of its scope: `skills/review`,
    /// `commands/git/sync.md`.
    pub to: PathBuf,
    pub body: Body,
    /// What of it was changed or dropped on the way.
    pub notes: Vec<String>,
}

/// A file azazel already reads where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InPlace {
    pub kind: Kind,
    pub path: PathBuf,
}

/// An MCP server entry as the source wrote it (already in the shape of an `mcpServers` entry).
#[derive(Debug, Clone, PartialEq)]
pub struct Server {
    pub scope: Scope,
    pub name: String,
    pub entry: serde_json::Value,
    /// Seconds a call may take, when the source says.
    pub timeout: Option<u64>,
}

/// One key of `config.toml`.
#[derive(Debug, Clone, PartialEq)]
pub struct Setting {
    pub key: Vec<String>,
    pub value: toml::Value,
    /// The source's own setting, for the report: `tools.approvalMode: yolo`.
    pub from: String,
}

/// A conversation that can be converted, found without reading all of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatRef {
    pub file: PathBuf,
    /// The source's id of the conversation.
    pub id: String,
    /// The directory it was held in; `None` when the source does not say.
    pub cwd: Option<PathBuf>,
    pub title: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryItem {
    pub scope: Scope,
    /// One of azazel's memory kinds.
    pub kind: &'static str,
    pub text: String,
}

/// Something that cannot be carried over, or only in part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Note {
    pub kind: Kind,
    pub text: String,
}

/// Everything one source holds.
#[derive(Debug, Clone, PartialEq)]
pub struct Found {
    pub source: Source,
    /// The directory the source keeps its data in.
    pub root: PathBuf,
    pub roles: Vec<Role>,
    pub providers: Vec<Provider>,
    pub keys: Vec<Key>,
    pub logins: Vec<Login>,
    pub files: Vec<FileItem>,
    pub in_place: Vec<InPlace>,
    pub servers: Vec<Server>,
    pub settings: Vec<Setting>,
    pub chats: Vec<ChatRef>,
    pub memories: Vec<MemoryItem>,
    pub lossy: Vec<Note>,
    /// Files that could not be read, as `file: why`.
    pub problems: Vec<String>,
}

impl Found {
    pub fn new(source: Source, root: &Path) -> Found {
        Found {
            source,
            root: root.to_path_buf(),
            roles: Vec::new(),
            providers: Vec::new(),
            keys: Vec::new(),
            logins: Vec::new(),
            files: Vec::new(),
            in_place: Vec::new(),
            servers: Vec::new(),
            settings: Vec::new(),
            chats: Vec::new(),
            memories: Vec::new(),
            lossy: Vec::new(),
            problems: Vec::new(),
        }
    }

    /// How many things of `kind` there are to bring over.
    pub fn count(&self, kind: Kind) -> usize {
        match kind {
            Kind::Models => self.roles.len() + self.providers.len(),
            Kind::Keys => self.keys.len() + self.logins.len(),
            Kind::Skills | Kind::Commands | Kind::Agents | Kind::Rules => {
                self.files.iter().filter(|file| file.kind == kind).count()
            }
            Kind::Mcp => self.servers.len(),
            Kind::Settings => self.settings.len(),
            Kind::Chats => self.chats.len(),
            Kind::Memory => self.memories.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        Kind::ALL.into_iter().all(|kind| self.count(kind) == 0) && self.in_place.is_empty()
    }

    pub fn lossy(&mut self, kind: Kind, text: impl Into<String>) {
        let note = Note {
            kind,
            text: text.into(),
        };
        if !self.lossy.contains(&note) {
            self.lossy.push(note);
        }
    }

    pub fn problem(&mut self, file: &Path, why: impl fmt::Display) {
        self.problems.push(format!("{}: {why}", file.display()));
    }
}

// ---------------------------------------------------------------- conversations

/// One step of a conversation, on the branch the source ended on.
#[derive(Debug, Clone, PartialEq)]
pub enum TurnKind {
    User(Vec<Block>),
    Assistant {
        /// `Text`, `Thinking` and `ToolCall` blocks.
        blocks: Vec<Block>,
        /// `(provider, model)` as azazel names them, when the source says.
        model: Option<(String, String)>,
        api: Option<WireApi>,
        usage: Usage,
        /// The reply ended in an error or was interrupted.
        failed: bool,
    },
    /// The result of one tool call. `content` holds `Text` and `Image` blocks.
    ToolResult {
        call_id: String,
        content: Vec<Block>,
        is_error: bool,
    },
    /// Everything before this point was summarised as `summary`; `kept` turns just before it
    /// were kept as they were.
    Compaction {
        summary: String,
        tokens_before: u64,
        kept: usize,
    },
    /// Something the source recorded that is no part of the conversation; kept as a `custom`
    /// entry.
    Note {
        name: String,
        data: serde_json::Value,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Turn {
    pub at: Option<jiff::Timestamp>,
    pub kind: TurnKind,
}

/// A conversation read from a source.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Chat {
    pub id: String,
    pub title: Option<String>,
    pub cwd: Option<PathBuf>,
    pub created: Option<jiff::Timestamp>,
    pub turns: Vec<Turn>,
    /// The conversations of the sub-agents it started, by the name the source gave each.
    pub agents: Vec<(String, Chat)>,
    /// What was left out, for the report: `3 entries on branches that were left`.
    pub dropped: Vec<String>,
}

// ---------------------------------------------------------------- the report

/// What became of one thing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Status {
    Imported,
    /// Left out: it is there already, or it conflicts with what is.
    Skipped,
    /// Carried over in part, or not at all, because azazel has nothing to hold it.
    Lossy,
    /// Azazel reads it where it is.
    InPlace,
    /// A sign-in to repeat by hand.
    Login,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Line {
    pub source: Source,
    pub kind: Kind,
    pub status: Status,
    pub text: String,
}

/// Everything a migration did, or would do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Report {
    pub dry_run: bool,
    /// `(source, where its data is)` for each source that was read.
    pub sources: Vec<(Source, PathBuf)>,
    pub lines: Vec<Line>,
}

impl Report {
    pub fn push(&mut self, source: Source, kind: Kind, status: Status, text: impl Into<String>) {
        self.lines.push(Line {
            source,
            kind,
            status,
            text: text.into(),
        });
    }

    pub fn count(&self, status: Status) -> usize {
        self.lines.iter().filter(|line| line.status == status).count()
    }

    /// The lines of `source` with `status`, for tests and for the popup's summary.
    pub fn of(&self, source: Source, kind: Kind, status: Status) -> Vec<&str> {
        self.lines
            .iter()
            .filter(|line| line.source == source && line.kind == kind && line.status == status)
            .map(|line| line.text.as_str())
            .collect()
    }

    fn word(&self, status: Status) -> &'static str {
        match (status, self.dry_run) {
            (Status::Imported, true) => "would import",
            (Status::Imported, false) => "imported",
            (Status::Skipped, _) => "skipped",
            (Status::Lossy, _) => "lossy",
            (Status::InPlace, _) => "in place",
            (Status::Login, _) => "sign in",
            (Status::Failed, _) => "failed",
        }
    }

    /// The report as text: a section per source, its lines grouped by kind, and the totals.
    pub fn render(&self) -> String {
        let mut out = String::new();
        let width = if self.dry_run { 12 } else { 8 };
        for (source, root) in &self.sources {
            out.push_str(&format!("{} ({})\n", source.label(), root.display()));
            let mut any = false;
            for kind in Kind::ALL {
                let lines: Vec<&Line> = self
                    .lines
                    .iter()
                    .filter(|line| line.source == *source && line.kind == kind)
                    .collect();
                if lines.is_empty() {
                    continue;
                }
                any = true;
                out.push_str(&format!("  {}\n", kind.name()));
                for line in lines {
                    out.push_str(&format!(
                        "    {:<width$}  {}\n",
                        self.word(line.status),
                        line.text
                    ));
                }
            }
            if !any {
                out.push_str("  nothing to bring over\n");
            }
            out.push('\n');
        }
        out.push_str(&self.summary());
        out
    }

    /// The totals in one line.
    pub fn summary(&self) -> String {
        let mut parts = vec![format!(
            "{} {}",
            self.count(Status::Imported),
            self.word(Status::Imported)
        )];
        for status in [
            Status::Skipped,
            Status::Lossy,
            Status::InPlace,
            Status::Login,
            Status::Failed,
        ] {
            let count = self.count(status);
            if count > 0 {
                parts.push(match status {
                    Status::InPlace => format!("{count} left in place"),
                    Status::Login => format!("{count} to sign in to"),
                    _ => format!("{count} {}", self.word(status)),
                });
            }
        }
        if self.dry_run {
            parts.push("nothing was written".to_string());
        }
        parts.join(", ")
    }
}
