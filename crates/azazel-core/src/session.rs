//! The session transcript: an append-only JSONL tree with an in-memory leaf pointer.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::UNIX_EPOCH;

use azazel_agent::compaction::{apply_context_edit, summary_message};
use azazel_agent::{ContextEntry, ContextReport, TranscriptSink};
use azazel_ai::{Message, ModelSpec, Role, Usage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const SESSION_VERSION: u32 = 1;

/// First line of a session file.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Header {
    #[serde(rename = "type")]
    kind: HeaderTag,
    pub version: u32,
    pub id: String,
    pub timestamp: String,
    pub cwd: String,
    #[serde(rename = "parentSession")]
    pub parent_session: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum HeaderTag {
    Session,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum EntryKind {
    Message {
        message: Message,
    },
    ModelChange {
        chain: Vec<ModelSpec>,
    },
    ModeChange {
        mode: String,
    },
    #[serde(rename_all = "camelCase")]
    ContextEdit {
        target_id: String,
        replacement: String,
    },
    #[serde(rename_all = "camelCase")]
    Compaction {
        summary: String,
        first_kept_entry_id: String,
        tokens_before: u64,
    },
    #[serde(rename_all = "camelCase")]
    Checkpoint {
        turn_entry_id: String,
        commit: String,
    },
    #[serde(rename_all = "camelCase")]
    Agent {
        agent_id: String,
        def: String,
        status: String,
        transcript: String,
    },
    #[serde(rename_all = "camelCase")]
    Goal {
        goal_id: String,
        objective: String,
        status: String,
        iteration: u32,
    },
    #[serde(rename_all = "camelCase")]
    Label {
        target_id: String,
        label: String,
    },
    #[serde(rename_all = "camelCase")]
    Custom {
        custom_type: String,
        data: Value,
    },
    SessionInfo {
        name: String,
    },
    /// The thinking level the session switched to.
    ThinkingChange {
        level: String,
    },
    /// Pinned sessions are listed first.
    Pin {
        pinned: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    #[serde(rename = "parentId")]
    pub parent_id: Option<String>,
    pub timestamp: String,
    #[serde(flatten)]
    pub kind: EntryKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SessionSummary {
    #[serde(skip)]
    pub path: PathBuf,
    pub id: String,
    /// When the session was created.
    pub timestamp: String,
    /// When it was last used: the time of its last entry that is not a name, a pin or a label.
    pub updated: String,
    pub name: Option<String>,
    /// Text of the first user message, for pickers.
    pub first_prompt: Option<String>,
    pub messages: usize,
    pub pinned: bool,
    /// Tokens and cost of the lead's replies.
    pub usage: Usage,
    /// Messages you sent.
    #[serde(default)]
    pub prompts: usize,
    /// Tokens and cost per `provider/model`.
    #[serde(default)]
    pub models: BTreeMap<String, Usage>,
    /// Calls per tool name.
    #[serde(default)]
    pub tools: BTreeMap<String, u32>,
}

/// A sub-agent that ran in a session.
#[derive(Debug, Clone, PartialEq)]
pub struct AgentRun {
    pub id: String,
    pub def: String,
    /// How it ended.
    pub status: String,
    pub transcript: PathBuf,
}

/// What a session file holds, in numbers.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SessionFacts {
    pub created: String,
    /// When it was last worked in.
    pub updated: String,
    /// The session this one was forked or handed off from.
    pub parent_session: Option<String>,
    /// Messages by who wrote them, over every branch.
    pub user: usize,
    pub assistant: usize,
    pub tool: usize,
    /// Places where the conversation went two or more ways.
    pub branch_points: usize,
    /// Tokens and cost of the lead's replies, over every branch.
    pub usage: Usage,
    pub checkpoints: usize,
    pub compactions: usize,
    pub agents: Vec<AgentRun>,
}

/// File in a session's own folder (`<sessions>/<id>/`) that holds what the lead's newest request
/// was made of.
const CONTEXT_FILE: &str = "context.json";

/// File in a sessions directory that caches each session's summary, so listing does not read
/// every session file again.
const INDEX_FILE: &str = "index.json";
/// Bumped when [`SessionSummary`] gains a field, so older indexes are rebuilt rather than read
/// with that field empty.
const INDEX_VERSION: u32 = 3;

#[derive(Default, Serialize, Deserialize)]
struct Index {
    #[serde(default)]
    version: u32,
    files: BTreeMap<String, Indexed>,
}

/// A summary, valid while the file keeps this size and modification time. Session files only
/// grow, so any change to one changes its size.
#[derive(Serialize, Deserialize)]
struct Indexed {
    size: u64,
    mtime_ns: u64,
    summary: SessionSummary,
}

struct Inner {
    path: PathBuf,
    /// `None` for a copy of a session that another process holds: it is only read.
    file: Option<File>,
    header: Header,
    entries: Vec<Entry>,
    index: HashMap<String, usize>,
    leaf: Option<String>,
}

pub struct SessionStore {
    inner: Mutex<Inner>,
}

fn now() -> String {
    jiff::Timestamp::now().to_string()
}

impl SessionStore {
    /// Starts `<dir>/<utc-timestamp>_<uuid>.jsonl`.
    pub fn create(dir: &Path, cwd: &Path, parent_session: Option<&Path>) -> io::Result<Self> {
        let id = uuid::Uuid::now_v7().to_string();
        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ");
        Self::create_at(&dir.join(format!("{stamp}_{id}.jsonl")), &id, cwd, parent_session)
    }

    /// Starts a transcript at an exact path (sub-agent transcripts).
    pub fn create_at(path: &Path, id: &str, cwd: &Path, parent_session: Option<&Path>) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let header = Header {
            kind: HeaderTag::Session,
            version: SESSION_VERSION,
            id: id.to_string(),
            timestamp: now(),
            cwd: cwd.to_string_lossy().into_owned(),
            parent_session: parent_session.map(|p| p.to_string_lossy().into_owned()),
        };
        let mut file = File::options().create_new(true).append(true).open(path)?;
        writeln!(file, "{}", serde_json::to_string(&header)?)?;
        Ok(SessionStore {
            inner: Mutex::new(Inner {
                path: path.to_path_buf(),
                file: Some(file),
                header,
                entries: Vec::new(),
                index: HashMap::new(),
                leaf: None,
            }),
        })
    }

    /// Loads an existing session; the leaf is its last entry. Lines that do not parse are skipped
    /// and left in place.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (header, entries) = read_file(path)?;
        let file = File::options().append(true).open(path)?;
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        let leaf = entries.last().map(|e| e.id.clone());
        Ok(SessionStore {
            inner: Mutex::new(Inner {
                path: path.to_path_buf(),
                file: Some(file),
                header,
                entries,
                index,
                leaf,
            }),
        })
    }

    /// A copy, in memory, of a session that lives in another process: what it holds can be
    /// read as from any other store. Nothing is written to `path`; what is appended stays in
    /// the copy.
    pub fn mirror(header: Header, entries: Vec<Entry>, leaf: Option<String>, path: PathBuf) -> Self {
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        SessionStore {
            inner: Mutex::new(Inner {
                path,
                file: None,
                header,
                entries,
                index,
                leaf,
            }),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn id(&self) -> String {
        self.lock().header.id.clone()
    }

    pub fn path(&self) -> PathBuf {
        self.lock().path.clone()
    }

    pub fn header(&self) -> Header {
        self.lock().header.clone()
    }

    pub fn leaf(&self) -> Option<String> {
        self.lock().leaf.clone()
    }

    /// Moves the leaf; the next append branches from `id` (`None` starts a new root).
    pub fn set_leaf(&self, id: Option<&str>) -> bool {
        let mut inner = self.lock();
        match id {
            Some(id) if !inner.index.contains_key(id) => false,
            _ => {
                inner.leaf = id.map(str::to_string);
                true
            }
        }
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.lock().entries.clone()
    }

    pub fn entry(&self, id: &str) -> Option<Entry> {
        let inner = self.lock();
        inner.index.get(id).map(|&i| inner.entries[i].clone())
    }

    /// Appends below the current leaf, which then moves to the new entry. Returns its id. A write
    /// failure is logged and the entry is kept in memory so the session can go on.
    pub fn append(&self, kind: EntryKind) -> String {
        let mut inner = self.lock();
        let id = loop {
            // The tail of a v7 UUID is its random part.
            let candidate = uuid::Uuid::now_v7().simple().to_string()[24..].to_string();
            if !inner.index.contains_key(&candidate) {
                break candidate;
            }
        };
        let entry = Entry {
            id: id.clone(),
            parent_id: inner.leaf.clone(),
            timestamp: now(),
            kind,
        };
        let written =
            serde_json::to_string(&entry)
                .map_err(io::Error::from)
                .and_then(|line| match &mut inner.file {
                    Some(file) => writeln!(file, "{line}"),
                    None => Ok(()),
                });
        if let Err(e) = written {
            tracing::error!("could not write to {}: {e}", inner.path.display());
        }
        let position = inner.entries.len();
        inner.index.insert(id.clone(), position);
        inner.entries.push(entry);
        inner.leaf = Some(id.clone());
        id
    }

    /// Entries from the root down to `leaf` (default: the current leaf).
    pub fn branch(&self, leaf: Option<&str>) -> Vec<Entry> {
        let inner = self.lock();
        let mut path = Vec::new();
        let mut cursor = leaf.map(str::to_string).or_else(|| inner.leaf.clone());
        while let Some(id) = cursor {
            let Some(&i) = inner.index.get(&id) else { break };
            let entry = &inner.entries[i];
            cursor = entry.parent_id.clone();
            path.push(entry.clone());
            if path.len() > inner.entries.len() {
                break; // a parent cycle in a damaged file
            }
        }
        path.reverse();
        path
    }

    /// The model context at `leaf`: the newest compaction on the branch contributes its summary and
    /// everything from its first kept entry; then the newest context edit per target is applied.
    pub fn build_context(&self, leaf: Option<&str>) -> Vec<ContextEntry> {
        let branch = self.branch(leaf);
        let compaction = branch
            .iter()
            .rposition(|e| matches!(e.kind, EntryKind::Compaction { .. }));
        let mut context = Vec::new();
        let mut start = 0;
        if let Some(at) = compaction
            && let EntryKind::Compaction {
                summary,
                first_kept_entry_id,
                ..
            } = &branch[at].kind
        {
            context.push(ContextEntry {
                id: branch[at].id.clone(),
                message: summary_message(summary),
                summary: true,
            });
            start = branch
                .iter()
                .position(|e| &e.id == first_kept_entry_id)
                .unwrap_or(at);
        }
        for entry in &branch[start..] {
            if let EntryKind::Message { message } = &entry.kind {
                context.push(ContextEntry {
                    id: entry.id.clone(),
                    message: message.clone(),
                    summary: false,
                });
            }
        }
        for entry in &branch {
            if let EntryKind::ContextEdit {
                target_id,
                replacement,
            } = &entry.kind
                && let Some(target) = context.iter_mut().find(|c| &c.id == target_id && !c.summary)
            {
                apply_context_edit(&mut target.message, replacement);
            }
        }
        context
    }

    /// Copies the branch ending at `at` (default: the current leaf) into a new session in `dir`.
    pub fn fork(&self, dir: &Path, at: Option<&str>) -> io::Result<SessionStore> {
        let branch = self.branch(at);
        let (cwd, source) = {
            let inner = self.lock();
            (PathBuf::from(&inner.header.cwd), inner.path.clone())
        };
        let fork = SessionStore::create(dir, &cwd, Some(&source))?;
        {
            let mut inner = fork.lock();
            for entry in branch {
                if let Some(file) = &mut inner.file {
                    writeln!(file, "{}", serde_json::to_string(&entry)?)?;
                }
                let position = inner.entries.len();
                inner.index.insert(entry.id.clone(), position);
                inner.leaf = Some(entry.id.clone());
                inner.entries.push(entry);
            }
        }
        Ok(fork)
    }

    /// The most recent `session_info` name.
    pub fn name(&self) -> Option<String> {
        self.lock().entries.iter().rev().find_map(|e| match &e.kind {
            EntryKind::SessionInfo { name } => Some(name.clone()),
            _ => None,
        })
    }

    /// Whether the session is pinned.
    pub fn pinned(&self) -> bool {
        self.lock()
            .entries
            .iter()
            .rev()
            .find_map(|e| match &e.kind {
                EntryKind::Pin { pinned } => Some(*pinned),
                _ => None,
            })
            .unwrap_or(false)
    }

    /// The label of each labelled entry. The newest label of an entry counts; an empty one
    /// takes the label away.
    pub fn labels(&self) -> HashMap<String, String> {
        let mut labels = HashMap::new();
        for entry in &self.lock().entries {
            if let EntryKind::Label { target_id, label } = &entry.kind {
                if label.trim().is_empty() {
                    labels.remove(target_id);
                } else {
                    labels.insert(target_id.clone(), label.trim().to_string());
                }
            }
        }
        labels
    }

    /// The model chain the branch ending at `leaf` last switched to.
    pub fn last_chain(&self, leaf: Option<&str>) -> Option<Vec<ModelSpec>> {
        self.branch(leaf).into_iter().rev().find_map(|e| match e.kind {
            EntryKind::ModelChange { chain } if !chain.is_empty() => Some(chain),
            _ => None,
        })
    }

    /// The thinking level the branch ending at `leaf` last switched to.
    pub fn last_thinking(&self, leaf: Option<&str>) -> Option<String> {
        self.branch(leaf).into_iter().rev().find_map(|e| match e.kind {
            EntryKind::ThinkingChange { level } => Some(level),
            _ => None,
        })
    }

    /// The last message you sent on the branch ending at `leaf`, skipping those `skip` says the
    /// UI wrote for you (notes about a command you ran, for example).
    pub fn last_prompt(&self, leaf: Option<&str>, skip: &dyn Fn(&str) -> bool) -> Option<Entry> {
        self.branch(leaf)
            .into_iter()
            .rev()
            .find(|entry| match &entry.kind {
                EntryKind::Message { message } => {
                    message.role == Role::User
                        && !skip(&message.text())
                        && message
                            .content
                            .iter()
                            .any(|block| !matches!(block, azazel_ai::Block::ToolResult { .. }))
                }
                _ => false,
            })
    }

    /// What `/session` reports, counted over the whole file.
    pub fn facts(&self) -> SessionFacts {
        let inner = self.lock();
        let mut facts = SessionFacts {
            created: inner.header.timestamp.clone(),
            updated: last_used(&inner.header, &inner.entries),
            parent_session: inner.header.parent_session.clone(),
            ..SessionFacts::default()
        };
        let mut children: HashMap<Option<&str>, usize> = HashMap::new();
        for entry in &inner.entries {
            *children.entry(entry.parent_id.as_deref()).or_default() += 1;
            match &entry.kind {
                EntryKind::Message { message } => {
                    match message.role {
                        Role::User => facts.user += 1,
                        Role::Assistant => facts.assistant += 1,
                        Role::Tool => facts.tool += 1,
                    }
                    if let Some(meta) = &message.meta {
                        facts.usage.add(&meta.usage);
                    }
                }
                EntryKind::Checkpoint { .. } => facts.checkpoints += 1,
                EntryKind::Compaction { .. } => facts.compactions += 1,
                EntryKind::Agent {
                    agent_id,
                    def,
                    status,
                    transcript,
                } => {
                    let run = AgentRun {
                        id: agent_id.clone(),
                        def: def.clone(),
                        status: status.clone(),
                        transcript: PathBuf::from(transcript),
                    };
                    // An agent that was woken again is recorded again; its last state counts.
                    match facts.agents.iter_mut().find(|known| known.id == run.id) {
                        Some(known) => *known = run,
                        None => facts.agents.push(run),
                    }
                }
                _ => {}
            }
        }
        facts.branch_points = children.values().filter(|count| **count > 1).count();
        facts
    }

    /// The last `count` messages with text in a session file, oldest first, for a preview.
    pub fn preview(path: &Path, count: usize) -> Vec<(Role, String)> {
        let Ok(branch) = read_branch(path) else {
            return Vec::new();
        };
        let mut shown: Vec<(Role, String)> = branch
            .into_iter()
            .rev()
            .filter_map(|entry| match entry.kind {
                EntryKind::Message { message }
                    if message.role != Role::Tool && !message.text().trim().is_empty() =>
                {
                    Some((message.role, message.text()))
                }
                _ => None,
            })
            .take(count)
            .collect();
        shown.reverse();
        shown
    }

    /// The branch a transcript file ends on, read without opening the file for writing (a
    /// sub-agent's own transcript, for example).
    pub fn branch_of(path: &Path) -> io::Result<Vec<Entry>> {
        read_branch(path)
    }

    /// The transcript of sub-agent `agent` of this session: where
    /// [`crate::paths::Paths::agent_transcripts_dir`] puts it for a session kept in its sessions
    /// directory.
    pub fn agent_transcript(&self, agent: &str) -> PathBuf {
        let inner = self.lock();
        inner
            .path
            .parent()
            .unwrap_or(Path::new(""))
            .join(&inner.header.id)
            .join(crate::paths::AGENT_TRANSCRIPTS)
            .join(format!("{agent}.jsonl"))
    }

    /// Where the breakdown of the newest request of session `id` is kept, for the session file
    /// `file`: in the session's folder, which is deleted with it.
    fn context_file(file: &Path, id: &str) -> PathBuf {
        file.parent().unwrap_or(Path::new("")).join(id).join(CONTEXT_FILE)
    }

    /// What the lead's newest request in the session at `file` was made of; `None` when the
    /// session has not sent one since this was first recorded.
    pub fn last_request(file: &Path) -> Option<ContextReport> {
        let (header, _) = read_file(file).ok()?;
        let bytes = std::fs::read(Self::context_file(file, &header.id)).ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The tokens and cost recorded in a transcript file (a sub-agent's, for example).
    pub fn usage_of(path: &Path) -> Option<Usage> {
        summarise(path).map(|summary| summary.usage)
    }

    /// Deletes a session: its file and the folder of its sub-agents' transcripts.
    pub fn delete(summary: &SessionSummary) -> io::Result<()> {
        std::fs::remove_file(&summary.path)?;
        if let Some(dir) = summary.path.parent() {
            let folder = dir.join(&summary.id);
            if folder.is_dir() {
                std::fs::remove_dir_all(folder)?;
            }
        }
        Ok(())
    }

    /// The unpinned sessions in `dir` last used before `cutoff`; deleted unless `dry_run`.
    /// `keep` (the open session, by id) is never touched.
    pub fn prune(
        dir: &Path,
        cutoff: jiff::Timestamp,
        keep: Option<&str>,
        dry_run: bool,
    ) -> Vec<SessionSummary> {
        let old: Vec<SessionSummary> = SessionStore::list(dir)
            .into_iter()
            .filter(|s| !s.pinned && Some(s.id.as_str()) != keep)
            .filter(|s| {
                s.updated
                    .parse::<jiff::Timestamp>()
                    .is_ok_and(|updated| updated < cutoff)
            })
            .collect();
        if dry_run {
            return old;
        }
        old.into_iter()
            .filter(|s| match SessionStore::delete(s) {
                Ok(()) => true,
                Err(e) => {
                    tracing::warn!("could not delete {}: {e}", s.path.display());
                    false
                }
            })
            .collect()
    }

    /// Sessions in `dir` whose messages contain `query` (case-insensitive), most recently used
    /// first, each with its matching lines (at most `per_session`).
    pub fn search(dir: &Path, query: &str, per_session: usize) -> Vec<(SessionSummary, Vec<String>)> {
        let needle = query.to_lowercase();
        if needle.trim().is_empty() {
            return Vec::new();
        }
        SessionStore::list(dir)
            .into_iter()
            .filter_map(|summary| {
                let (_, entries) = read_file(&summary.path).ok()?;
                let lines: Vec<String> = entries
                    .iter()
                    .filter_map(|entry| match &entry.kind {
                        EntryKind::Message { message } => Some(message.text()),
                        _ => None,
                    })
                    .flat_map(|text| {
                        text.lines()
                            .filter(|line| line.to_lowercase().contains(&needle))
                            .map(|line| line.trim().to_string())
                            .collect::<Vec<_>>()
                    })
                    .take(per_session)
                    .collect();
                (!lines.is_empty()).then_some((summary, lines))
            })
            .collect()
    }

    /// Sessions in `dir`, most recently used first. Summaries come from the directory's index
    /// where a file is unchanged since it was summarised; the index is rewritten when anything
    /// changed.
    pub fn list(dir: &Path) -> Vec<SessionSummary> {
        let Ok(read_dir) = std::fs::read_dir(dir) else {
            return Vec::new();
        };
        let index_path = dir.join(INDEX_FILE);
        let old: Index = std::fs::read(&index_path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Index>(&bytes).ok())
            .filter(|index| index.version == INDEX_VERSION)
            .unwrap_or_default();
        let mut index = Index {
            version: INDEX_VERSION,
            files: BTreeMap::new(),
        };
        let mut changed = false;
        for entry in read_dir.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            // Asked of the file itself: on Windows a directory listing can report a stale size
            // and time for a file another handle is still writing.
            let (Some(name), Ok(meta)) = (
                path.file_name().and_then(|n| n.to_str()),
                std::fs::metadata(&path),
            ) else {
                continue;
            };
            let size = meta.len();
            let mtime_ns = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map_or(0, |d| d.as_nanos() as u64);
            let summary = match old.files.get(name) {
                Some(known) if known.size == size && known.mtime_ns == mtime_ns => known.summary.clone(),
                _ => match summarise(&path) {
                    Some(summary) => {
                        changed = true;
                        summary
                    }
                    None => continue,
                },
            };
            index.files.insert(
                name.to_string(),
                Indexed {
                    size,
                    mtime_ns,
                    summary,
                },
            );
        }
        if changed || index.files.len() != old.files.len() {
            write_index(&index_path, &index);
        }
        let mut sessions: Vec<SessionSummary> = index
            .files
            .into_iter()
            .map(|(name, indexed)| SessionSummary {
                path: dir.join(name),
                ..indexed.summary
            })
            .collect();
        sessions.sort_by(|a, b| {
            b.updated
                .cmp(&a.updated)
                .then_with(|| b.timestamp.cmp(&a.timestamp))
        });
        sessions
    }
}

/// Reads a whole session file into its summary.
fn summarise(path: &Path) -> Option<SessionSummary> {
    let (header, entries) = read_file(path).ok()?;
    let mut summary = SessionSummary {
        path: path.to_path_buf(),
        updated: last_used(&header, &entries),
        id: header.id,
        timestamp: header.timestamp,
        name: None,
        first_prompt: None,
        messages: 0,
        pinned: false,
        usage: Usage::default(),
        prompts: 0,
        models: BTreeMap::new(),
        tools: BTreeMap::new(),
    };
    for entry in &entries {
        match &entry.kind {
            EntryKind::Message { message } => {
                summary.messages += 1;
                if message.role == Role::User {
                    summary.prompts += 1;
                }
                for (_, name, _) in message.tool_calls() {
                    *summary.tools.entry(name.to_string()).or_default() += 1;
                }
                if let Some(meta) = &message.meta {
                    summary
                        .models
                        .entry(format!("{}/{}", meta.provider, meta.model))
                        .or_default()
                        .add(&meta.usage);
                }
                if summary.first_prompt.is_none() && message.role == Role::User {
                    summary.first_prompt = Some(message.text());
                }
                if let Some(meta) = &message.meta {
                    summary.usage.add(&meta.usage);
                }
            }
            EntryKind::SessionInfo { name } => summary.name = Some(name.clone()),
            EntryKind::Pin { pinned } => summary.pinned = *pinned,
            _ => {}
        }
    }
    Some(summary)
}

/// The branch a session file ends on, read without opening the file for writing.
fn read_branch(path: &Path) -> io::Result<Vec<Entry>> {
    let (_, entries) = read_file(path)?;
    let by_id: HashMap<&str, &Entry> = entries.iter().map(|e| (e.id.as_str(), e)).collect();
    let mut branch = Vec::new();
    let mut cursor = entries.last();
    while let Some(entry) = cursor {
        branch.push(entry.clone());
        if branch.len() > entries.len() {
            break; // a parent cycle in a damaged file
        }
        cursor = entry.parent_id.as_deref().and_then(|id| by_id.get(id).copied());
    }
    branch.reverse();
    Ok(branch)
}

/// When a session was last worked in. Naming, pinning or labelling it from a picker is not use,
/// so it neither moves the session up the list nor saves it from being pruned.
fn last_used(header: &Header, entries: &[Entry]) -> String {
    entries
        .iter()
        .rev()
        .find(|entry| {
            !matches!(
                entry.kind,
                EntryKind::SessionInfo { .. } | EntryKind::Pin { .. } | EntryKind::Label { .. }
            )
        })
        .map_or_else(|| header.timestamp.clone(), |entry| entry.timestamp.clone())
}

/// Replaces the index in one step, so a concurrent reader sees the old or the new one.
fn write_index(path: &Path, index: &Index) {
    let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
    let written = serde_json::to_vec(index)
        .map_err(io::Error::other)
        .and_then(|bytes| std::fs::write(&temp, bytes))
        .and_then(|()| std::fs::rename(&temp, path));
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temp);
        tracing::debug!("could not write {}: {e}", path.display());
    }
}

fn read_file(path: &Path) -> io::Result<(Header, Vec<Entry>)> {
    let text = std::fs::read_to_string(path)?;
    let mut lines = text.lines();
    let header: Header = lines
        .next()
        .and_then(|line| serde_json::from_str(line).ok())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a session file", path.display()),
            )
        })?;
    let mut entries = Vec::new();
    for (n, line) in lines.enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            Err(e) => tracing::warn!("{}:{}: skipping unreadable entry: {e}", path.display(), n + 2),
        }
    }
    Ok((header, entries))
}

impl TranscriptSink for SessionStore {
    fn message(&self, message: &Message) -> String {
        self.append(EntryKind::Message {
            message: message.clone(),
        })
    }

    fn context_edit(&self, target: &str, replacement: &str) {
        self.append(EntryKind::ContextEdit {
            target_id: target.to_string(),
            replacement: replacement.to_string(),
        });
    }

    fn compaction(&self, summary: &str, first_kept: &str, tokens_before: u64) -> String {
        self.append(EntryKind::Compaction {
            summary: summary.to_string(),
            first_kept_entry_id: first_kept.to_string(),
            tokens_before,
        })
    }

    /// Replaces the session's `context.json` in one step. A copy of a session that lives in
    /// another process writes nothing.
    fn request(&self, report: &ContextReport) {
        let path = {
            let inner = self.lock();
            if inner.file.is_none() {
                return;
            }
            Self::context_file(&inner.path, &inner.header.id)
        };
        let temp = path.with_extension(format!("json.{}.tmp", std::process::id()));
        let written = serde_json::to_vec(report)
            .map_err(io::Error::other)
            .and_then(|bytes| {
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&temp, bytes)
            })
            .and_then(|()| std::fs::rename(&temp, &path));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp);
            tracing::debug!("could not write {}: {e}", path.display());
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::Block;
    use serde_json::json;

    use super::*;

    fn assistant(text: &str) -> Message {
        let mut message = Message::user(text);
        message.role = Role::Assistant;
        message
    }

    fn texts(context: &[ContextEntry]) -> Vec<String> {
        context.iter().map(|c| c.message.text()).collect()
    }

    #[test]
    fn a_mirror_reads_like_the_session_and_writes_no_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        let first = store.message(&Message::user("hello"));
        store.message(&assistant("hi"));
        store.append(EntryKind::SessionInfo {
            name: "greeting".into(),
        });
        store.set_leaf(Some(&first));
        let size = std::fs::metadata(store.path()).unwrap().len();

        // As it travels to another process: the header and the entries as JSON, and the leaf.
        let header: Header = serde_json::from_value(serde_json::to_value(store.header()).unwrap()).unwrap();
        let entries: Vec<Entry> = store
            .entries()
            .iter()
            .map(|entry| serde_json::from_value(serde_json::to_value(entry).unwrap()).unwrap())
            .collect();
        let mirror = SessionStore::mirror(header, entries, store.leaf(), store.path());
        assert_eq!(mirror.id(), store.id());
        assert_eq!(mirror.name().as_deref(), Some("greeting"));
        assert_eq!(mirror.entries(), store.entries());
        assert_eq!(texts(&mirror.build_context(None)), ["hello"]);
        assert_eq!(mirror.branch(None), store.branch(None));

        // What is appended to the copy stays in the copy.
        mirror.message(&assistant("only here"));
        assert_eq!(texts(&mirror.build_context(None)), ["hello", "only here"]);
        assert_eq!(std::fs::metadata(store.path()).unwrap().len(), size);
    }

    #[test]
    fn written_session_reloads_to_the_same_context() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        store.message(&Message::user("hello"));
        store.append(EntryKind::ModelChange {
            chain: vec!["@default".parse().unwrap()],
        });
        store.message(&assistant("hi"));
        store.append(EntryKind::SessionInfo {
            name: "greeting".into(),
        });
        let original = store.build_context(None);
        assert_eq!(texts(&original), ["hello", "hi"]);

        let reopened = SessionStore::open(&store.path()).unwrap();
        assert_eq!(reopened.build_context(None), original);
        assert_eq!(reopened.id(), store.id());
        assert_eq!(reopened.name().as_deref(), Some("greeting"));
        assert_eq!(reopened.header().cwd, "/work");
    }

    #[test]
    fn file_lines_have_the_documented_shape() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        let first = store.message(&Message::user("hello"));
        store.context_edit(&first, "[gone]");
        store.compaction("sum", &first, 123);
        let lines: Vec<Value> = std::fs::read_to_string(store.path())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(lines[0]["type"], "session");
        assert_eq!(lines[0]["version"], 1);
        assert_eq!(lines[0]["parentSession"], Value::Null);
        assert_eq!(lines[1]["type"], "message");
        assert_eq!(lines[1]["parentId"], Value::Null);
        assert_eq!(lines[1]["id"].as_str().unwrap().len(), 8);
        assert_eq!(
            lines[1]["message"],
            json!({"role": "user", "content": [{"type": "text", "text": "hello"}]})
        );
        assert_eq!(lines[2]["type"], "context_edit");
        assert_eq!(lines[2]["targetId"], lines[1]["id"]);
        assert_eq!(lines[2]["parentId"], lines[1]["id"]);
        assert_eq!(lines[3]["firstKeptEntryId"], lines[1]["id"]);
        assert_eq!(lines[3]["tokensBefore"], 123);
    }

    #[test]
    fn compaction_yields_summary_plus_kept_tail_and_survives_reload() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        store.message(&Message::user("old question"));
        store.message(&assistant("old answer"));
        let kept = store.message(&Message::user("recent question"));
        store.message(&assistant("recent answer"));
        let compaction = store.compaction("what happened before", &kept, 9000);
        store.message(&Message::user("next"));

        for context in [
            store.build_context(None),
            SessionStore::open(&store.path()).unwrap().build_context(None),
        ] {
            assert_eq!(
                texts(&context),
                [
                    "Summary of the conversation so far:\n\nwhat happened before",
                    "recent question",
                    "recent answer",
                    "next"
                ]
            );
            assert!(context[0].summary);
            assert_eq!(context[0].id, compaction);
            assert_eq!(context[1].id, kept);
        }
    }

    #[test]
    fn context_edits_rewrite_their_target_and_the_newest_wins() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        store.message(&Message::user("q"));
        let results = store.message(&Message::tool(vec![
            Block::tool_result("c1", "long output", false),
            Block::tool_result("c2", "failure text", true),
        ]));
        store.context_edit(&results, "[first]");
        store.context_edit(&results, "[second]");
        store.context_edit("missing", "[ignored]");
        let context = store.build_context(None);
        assert_eq!(
            context[1].message.content,
            vec![
                Block::tool_result("c1", "[second]", false),
                Block::tool_result("c2", "[second]", true)
            ]
        );
    }

    #[test]
    fn moving_the_leaf_branches_without_rewriting_and_fork_copies_one_branch() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        let root = store.message(&Message::user("question"));
        store.message(&assistant("answer one"));
        let size_before = std::fs::metadata(store.path()).unwrap().len();

        assert!(store.set_leaf(Some(&root)));
        assert!(!store.set_leaf(Some("nope")));
        let alternative = store.message(&assistant("answer two"));
        assert_eq!(texts(&store.build_context(None)), ["question", "answer two"]);
        assert_eq!(texts(&store.build_context(Some(&root))), ["question"]);
        assert!(std::fs::metadata(store.path()).unwrap().len() > size_before);
        assert_eq!(store.entries().len(), 3);

        let forks = tempfile::tempdir().unwrap();
        let fork = store.fork(forks.path(), Some(&alternative)).unwrap();
        assert_eq!(texts(&fork.build_context(None)), ["question", "answer two"]);
        assert_eq!(fork.entries().len(), 2);
        assert_eq!(
            fork.header().parent_session.map(PathBuf::from),
            Some(store.path())
        );
        assert_ne!(fork.id(), store.id());
    }

    #[test]
    fn labels_restore_points_the_last_prompt_and_facts_come_from_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/w"), None).unwrap();
        let say = |message: Message| store.append(EntryKind::Message { message });
        let first = say(Message::user("first"));
        say(assistant("one"));
        store.append(EntryKind::ModelChange {
            chain: vec![ModelSpec::Role("slow".into())],
        });
        store.append(EntryKind::ThinkingChange { level: "high".into() });
        let second = say(Message::user("second"));
        let reply = say(assistant("two"));
        let note = say(Message::user("I ran `ls` myself"));
        let used = store.entry(&note).unwrap().timestamp;

        // The note the UI wrote is not a prompt.
        let is_note = |text: &str| text.starts_with("I ran `");
        assert_eq!(store.last_prompt(None, &is_note).unwrap().id, second);
        assert_eq!(store.last_chain(None), Some(vec![ModelSpec::Role("slow".into())]));
        assert_eq!(store.last_thinking(None).as_deref(), Some("high"));

        let label = |target: &str, label: &str| {
            store.append(EntryKind::Label {
                target_id: target.to_string(),
                label: label.to_string(),
            })
        };
        label(&first, "start");
        label(&reply, "wrong turn");
        label(&reply, " ");
        assert_eq!(
            store.labels(),
            HashMap::from([(first.clone(), "start".to_string())])
        );
        assert!(!store.pinned());
        store.append(EntryKind::Pin { pinned: true });
        store.append(EntryKind::SessionInfo { name: "demo".into() });
        assert!(store.pinned());

        // A name, a pin or a label is not use: the session keeps its place in the list.
        let listed = SessionStore::list(dir.path());
        assert_eq!(listed[0].updated, used);

        // Another branch from the first prompt: it never switched model or thinking.
        assert!(store.set_leaf(Some(&first)));
        say(assistant("another one"));
        assert_eq!(store.last_chain(None), None);
        assert_eq!(store.last_thinking(None), None);
        assert_eq!(store.last_prompt(None, &is_note).unwrap().id, first);

        let facts = store.facts();
        assert_eq!((facts.user, facts.assistant, facts.tool), (3, 3, 0));
        assert_eq!(facts.branch_points, 1);
        assert_eq!(facts.updated, store.entries().last().unwrap().timestamp);

        // A preview follows the branch the file ends on.
        let shown = SessionStore::preview(&store.path(), 6);
        assert_eq!(
            shown,
            vec![
                (Role::User, "first".to_string()),
                (Role::Assistant, "another one".to_string())
            ]
        );
        assert_eq!(SessionStore::preview(&store.path(), 1).len(), 1);

        let reopened = SessionStore::open(&store.path()).unwrap();
        assert!(reopened.pinned());
        assert_eq!(reopened.labels().len(), 1);
        assert_eq!(reopened.last_thinking(Some(&reply)).as_deref(), Some("high"));
        assert_eq!(
            reopened.last_chain(Some(&reply)).map(|chain| chain.len()),
            Some(1)
        );
    }

    #[test]
    fn damaged_lines_are_skipped_and_the_file_is_not_truncated() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        store.message(&Message::user("kept"));
        let path = store.path();
        drop(store);
        let mut text = std::fs::read_to_string(&path).unwrap();
        text.push_str("{\"type\":\"message\",\"id\":\"broken\n");
        text.push_str(
            "{\"type\":\"from_the_future\",\"id\":\"ffffffff\",\"parentId\":null,\"timestamp\":\"t\"}\n",
        );
        std::fs::write(&path, &text).unwrap();

        let reopened = SessionStore::open(&path).unwrap();
        assert_eq!(texts(&reopened.build_context(None)), ["kept"]);
        reopened.message(&assistant("still works"));
        let after = std::fs::read_to_string(&path).unwrap();
        assert!(after.starts_with(&text));
        assert_eq!(
            texts(&SessionStore::open(&path).unwrap().build_context(None)),
            ["kept", "still works"]
        );
    }

    #[test]
    fn list_reports_name_first_prompt_and_count_newest_first() {
        let dir = tempfile::tempdir().unwrap();
        let older = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        older.message(&Message::user("first session prompt"));
        older.message(&assistant("ok"));
        std::thread::sleep(std::time::Duration::from_millis(5));
        let newer = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        newer.append(EntryKind::SessionInfo { name: "named".into() });
        std::fs::write(dir.path().join("notes.txt"), "not a session").unwrap();

        let list = SessionStore::list(dir.path());
        assert_eq!(
            list.iter().map(|s| s.id.clone()).collect::<Vec<_>>(),
            [newer.id(), older.id()]
        );
        assert_eq!((list[0].name.as_deref(), list[0].messages), (Some("named"), 0));
        assert_eq!(
            (list[1].first_prompt.as_deref(), list[1].messages),
            (Some("first session prompt"), 2)
        );
        assert!(SessionStore::list(&dir.path().join("missing")).is_empty());
    }

    #[test]
    fn prune_keeps_pinned_recent_and_open_sessions_and_search_finds_lines() {
        let dir = tempfile::tempdir().unwrap();
        let make = |text: &str| {
            let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
            store.message(&Message::user(text));
            std::fs::create_dir_all(dir.path().join(store.id()).join("agents")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(5));
            store
        };
        let old = make("old work on the Parser");
        let pinned = make("pinned work");
        pinned.append(EntryKind::Pin { pinned: true });
        let open = make("open work");
        std::thread::sleep(std::time::Duration::from_millis(5));
        let cutoff = jiff::Timestamp::now();
        std::thread::sleep(std::time::Duration::from_millis(5));
        let recent = make("recent parser fix\nsecond line");

        let found = SessionStore::search(dir.path(), "PARSER", 3);
        let ids: Vec<String> = found.iter().map(|(s, _)| s.id.clone()).collect();
        assert_eq!(ids, [recent.id(), old.id()]);
        assert_eq!(found[0].1, ["recent parser fix"]);
        assert!(SessionStore::search(dir.path(), "  ", 3).is_empty());

        let would = SessionStore::prune(dir.path(), cutoff, Some(&open.id()), true);
        assert_eq!(would.iter().map(|s| s.id.clone()).collect::<Vec<_>>(), [old.id()]);
        assert!(old.path().is_file());
        let gone = SessionStore::prune(dir.path(), cutoff, Some(&open.id()), false);
        assert_eq!(gone.len(), 1);
        assert!(!old.path().exists() && !dir.path().join(old.id()).exists());
        let left: Vec<String> = SessionStore::list(dir.path()).into_iter().map(|s| s.id).collect();
        assert_eq!(left.len(), 3);
        assert!(left.contains(&pinned.id()) && left.contains(&open.id()) && left.contains(&recent.id()));
    }

    #[test]
    fn the_index_reuses_unchanged_summaries_and_rereads_changed_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        a.message(&Message::user("hello"));
        let b = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        b.message(&Message::user("other"));
        assert_eq!(SessionStore::list(dir.path()).len(), 2);

        // Edited cached summaries come back for unchanged files: they were not read again.
        let index_path = dir.path().join(INDEX_FILE);
        let mut index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        for file in index["files"].as_object_mut().unwrap().values_mut() {
            file["summary"]["name"] = json!("cached");
        }
        std::fs::write(&index_path, serde_json::to_vec(&index).unwrap()).unwrap();
        let list = SessionStore::list(dir.path());
        assert!(
            list.iter().all(|s| s.name.as_deref() == Some("cached")),
            "{list:?}"
        );

        // A file written to is read again. Pinning is not use: it keeps its place in the list.
        std::thread::sleep(std::time::Duration::from_millis(5));
        a.append(EntryKind::Pin { pinned: true });
        let list = SessionStore::list(dir.path());
        assert_eq!(list[1].id, a.id());
        assert_eq!((list[1].name.as_deref(), list[1].pinned), (None, true));
        assert_eq!(list[0].name.as_deref(), Some("cached"));
        // Work in it is: it moves to the top.
        a.message(&Message::user("again"));
        assert_eq!(SessionStore::list(dir.path())[0].id, a.id());

        // A deleted session leaves the index.
        drop(b);
        std::fs::remove_file(&list[0].path).unwrap();
        assert_eq!(SessionStore::list(dir.path()).len(), 1);
        let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        assert_eq!(index["files"].as_object().unwrap().len(), 1);
    }
}
