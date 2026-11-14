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
    /// The primary agent the lead switched to.
    AgentChange {
        agent: String,
    },
    /// Pinned sessions are listed first.
    Pin {
        pinned: bool,
    },
    /// An archived session is kept out of the lists of sessions (`-p --archived`).
    Archive {
        archived: bool,
    },
    /// A run began: the work one prompt starts. The entry hangs from that prompt. `request_id`
    /// is the name whoever sent the prompt gave it.
    #[serde(rename_all = "camelCase")]
    RunStarted {
        run_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    /// A request of the run went out to this model.
    RequestStarted {
        model: String,
        /// The virtual model that picked `model` for this request (`virtual/<name>`).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        via: Option<String>,
    },
    /// A tool call of the run was let through and began.
    #[serde(rename_all = "camelCase")]
    ToolStarted {
        call_id: String,
        name: String,
        input: Value,
        /// What was mended in the call as the model sent it before it ran: another name for
        /// the tool, a number sent as a string.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        repairs: Vec<String>,
    },
    /// The run ended: `stop` is a stop reason, or `failed`. A run without this entry was cut
    /// short by the end of the process.
    #[serde(rename_all = "camelCase")]
    RunEnded {
        run_id: String,
        stop: String,
    },
}

impl EntryKind {
    /// Whether the entry belongs to the journal of a run: it says where the run stands and is
    /// no part of the conversation. Such an entry hangs from the entry it was written after,
    /// and nothing continues from it.
    pub fn is_journal(&self) -> bool {
        matches!(
            self,
            EntryKind::RunStarted { .. }
                | EntryKind::RequestStarted { .. }
                | EntryKind::ToolStarted { .. }
                | EntryKind::RunEnded { .. }
        )
    }
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
    /// The directory the session works in, as its header says.
    #[serde(default)]
    pub cwd: String,
    /// Kept out of the lists of sessions unless they are asked for all (`-p --archived`).
    #[serde(default)]
    pub archived: bool,
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

/// A run the end of the process cut short: the last run of a branch, with no record of its end.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptedRun {
    pub run_id: String,
    /// The name whoever sent the prompt gave it.
    pub request_id: Option<String>,
    /// The calls of the last reply that have no result, as `(call id, tool name)`.
    pub open_calls: Vec<(String, String)>,
    /// Those of them that had started.
    pub started: Vec<String>,
    /// The model a request had gone out to when nothing came back.
    pub model: Option<String>,
    /// When the run last wrote anything, RFC 3339.
    pub at: String,
}

impl InterruptedRun {
    /// Where the run stopped, in a few words.
    pub fn describe(&self) -> String {
        let running: Vec<&str> = self
            .open_calls
            .iter()
            .filter(|(id, _)| self.started.contains(id))
            .map(|(_, name)| name.as_str())
            .collect();
        match (running.as_slice(), self.open_calls.len(), &self.model) {
            ([], 0, Some(model)) => format!("stopped while waiting for {model}"),
            ([], 0, None) => "stopped before the model was asked".to_string(),
            ([], calls, _) => format!("stopped before {calls} tool call(s) ran"),
            (running, ..) => format!("stopped while {} ran", running.join(", ")),
        }
    }
}

/// What became of the run a named request started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RequestRun {
    pub run_id: String,
    /// How it ended; `None` while it has not.
    pub stop: Option<String>,
    /// The last thing the lead said in it.
    pub text: Option<String>,
}

/// The run that was cut short on the branch ending at `leaf`: the last run that began on the
/// branch, when the file holds no end of it and something is left to do. A run begins at the
/// prompt its `run_started` entry hangs from.
fn interrupted(
    entries: &[Entry],
    index: &HashMap<String, usize>,
    leaf: Option<&str>,
) -> Option<InterruptedRun> {
    let place = |id: Option<&str>| id.and_then(|id| index.get(id).copied());
    // The branch from the leaf up, as far as it has been walked.
    let mut tail: Vec<&Entry> = Vec::new();
    let mut cursor = place(leaf);
    // Runs follow each other, so the last one of the branch is the one written last whose
    // prompt is on the branch. An entry is written after the one it follows: the walk up the
    // branch need go no further than the prompt looked for.
    for (at, entry) in entries.iter().enumerate().rev() {
        let EntryKind::RunStarted { run_id, request_id } = &entry.kind else {
            continue;
        };
        let Some(prompt) = place(entry.parent_id.as_deref()) else {
            continue;
        };
        while let Some(position) = cursor.filter(|position| *position > prompt) {
            tail.push(&entries[position]);
            if tail.len() > entries.len() {
                return None; // a parent cycle in a damaged file
            }
            cursor = place(entries[position].parent_id.as_deref());
        }
        if cursor == Some(prompt) {
            tail.push(&entries[prompt]);
            tail.reverse();
            return unfinished(entries, index, at, run_id, request_id.as_deref(), &tail);
        }
        cursor?;
    }
    None
}

/// What is left of the run whose `run_started` entry is at `at`; `tail` is the branch from the
/// run's prompt to the leaf.
fn unfinished(
    entries: &[Entry],
    index: &HashMap<String, usize>,
    at: usize,
    run_id: &str,
    request_id: Option<&str>,
    tail: &[&Entry],
) -> Option<InterruptedRun> {
    let ended = entries[at + 1..]
        .iter()
        .any(|entry| matches!(&entry.kind, EntryKind::RunEnded { run_id: ended, .. } if ended == run_id));
    if ended {
        return None;
    }
    let last = tail
        .iter()
        .rfind(|entry| matches!(entry.kind, EntryKind::Message { .. }))?;
    let EntryKind::Message { message } = &last.kind else {
        return None;
    };
    let open_calls: Vec<(String, String)> = match message.role {
        Role::Assistant => message
            .tool_calls()
            .map(|(id, name, _)| (id.to_string(), name.to_string()))
            .collect(),
        _ => Vec::new(),
    };
    // A reply without calls is whole: there is nothing to go on with.
    if message.role == Role::Assistant && open_calls.is_empty() {
        return None;
    }
    // What the run noted after its last message, up to whatever was said elsewhere since.
    let from = index.get(&last.id)? + 1;
    let noted = entries
        .iter()
        .enumerate()
        .skip(from)
        .take_while(|(position, entry)| {
            *position == at
                || !matches!(
                    entry.kind,
                    EntryKind::Message { .. } | EntryKind::RunStarted { .. }
                )
        });
    let (mut started, mut model) = (Vec::new(), None);
    for (_, entry) in noted {
        match &entry.kind {
            EntryKind::ToolStarted { call_id, .. }
                if open_calls.iter().any(|(id, _)| id == call_id) && !started.contains(call_id) =>
            {
                started.push(call_id.clone());
            }
            EntryKind::RequestStarted { model: asked, .. } if open_calls.is_empty() => {
                model = Some(asked.clone())
            }
            _ => {}
        }
    }
    Some(InterruptedRun {
        run_id: run_id.to_string(),
        request_id: request_id.map(str::to_string),
        open_calls,
        started,
        model,
        at: tail
            .last()
            .map(|entry| entry.timestamp.clone())
            .unwrap_or_default(),
    })
}

/// File in a session's own folder (`<sessions>/<id>/`) that holds what the lead's newest request
/// was made of.
const CONTEXT_FILE: &str = "context.json";

/// Directory in a session's own folder that holds its `local://` files.
const LOCAL_FILES: &str = "local";

/// File in a sessions directory that caches each session's summary, so listing does not read
/// every session file again.
const INDEX_FILE: &str = "index.json";
/// Bumped when [`SessionSummary`] gains a field, so older indexes are rebuilt rather than read
/// with that field empty.
const INDEX_VERSION: u32 = 4;

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

    /// Starts `<dir>/<utc-timestamp>_<uuid>.jsonl` for a conversation that began at `created`
    /// somewhere else: the file name, the header and the id carry that time, so the session
    /// sorts where it belongs among the others and many brought in at once have ids that tell
    /// them apart by their first digits.
    pub fn create_dated(dir: &Path, cwd: &Path, created: jiff::Timestamp) -> io::Result<Self> {
        let seconds = u64::try_from(created.as_second()).unwrap_or(0);
        let nanos = u32::try_from(created.subsec_nanosecond()).unwrap_or(0);
        let time = uuid::Timestamp::from_unix(uuid::NoContext, seconds, nanos);
        let id = uuid::Uuid::new_v7(time).to_string();
        let stamp = created.strftime("%Y%m%dT%H%M%SZ");
        let path = dir.join(format!("{stamp}_{id}.jsonl"));
        Self::start(&path, &id, cwd, None, created.to_string())
    }

    /// Starts a transcript at an exact path (sub-agent transcripts).
    pub fn create_at(path: &Path, id: &str, cwd: &Path, parent_session: Option<&Path>) -> io::Result<Self> {
        Self::start(path, id, cwd, parent_session, now())
    }

    fn start(
        path: &Path,
        id: &str,
        cwd: &Path,
        parent_session: Option<&Path>,
        timestamp: String,
    ) -> io::Result<Self> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let header = Header {
            kind: HeaderTag::Session,
            version: SESSION_VERSION,
            id: id.to_string(),
            timestamp,
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

    /// Loads an existing session; the leaf is its last entry that is part of the conversation.
    /// Lines that do not parse are skipped and left in place.
    pub fn open(path: &Path) -> io::Result<Self> {
        let (header, entries) = read_file(path)?;
        let file = File::options().append(true).open(path)?;
        let index = entries
            .iter()
            .enumerate()
            .map(|(i, e)| (e.id.clone(), i))
            .collect();
        // The conversation goes on from what was said last, not from a note about a run.
        let leaf = entries
            .iter()
            .rev()
            .find(|entry| !entry.kind.is_journal())
            .map(|entry| entry.id.clone());
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

    /// The session's scratch files, the directory `local://` addresses name: in the session's
    /// folder beside its sub-agents' transcripts, shared by the lead and every sub-agent, and
    /// deleted with the session.
    pub fn local_dir(&self) -> PathBuf {
        let inner = self.lock();
        inner
            .path
            .parent()
            .unwrap_or(Path::new(""))
            .join(&inner.header.id)
            .join(LOCAL_FILES)
    }

    pub fn header(&self) -> Header {
        self.lock().header.clone()
    }

    pub fn leaf(&self) -> Option<String> {
        self.lock().leaf.clone()
    }

    /// Moves the leaf; the next append branches from `id` (`None` starts a new root). An entry
    /// of a run's journal stands for the entry it hangs from: nothing continues from a note.
    pub fn set_leaf(&self, id: Option<&str>) -> bool {
        let mut inner = self.lock();
        let Some(id) = id else {
            inner.leaf = None;
            return true;
        };
        let Some(&at) = inner.index.get(id) else {
            return false;
        };
        let entry = &inner.entries[at];
        let leaf = match entry.kind.is_journal() {
            true => entry.parent_id.clone(),
            false => Some(entry.id.clone()),
        };
        inner.leaf = leaf;
        true
    }

    pub fn entries(&self) -> Vec<Entry> {
        self.lock().entries.clone()
    }

    pub fn entry(&self, id: &str) -> Option<Entry> {
        let inner = self.lock();
        inner.index.get(id).map(|&i| inner.entries[i].clone())
    }

    /// Appends below the current leaf, which then moves to the new entry; an entry of a run's
    /// journal leaves the leaf where it is. Returns the entry's id. A write failure is logged
    /// and the entry is kept in memory so the session can go on.
    pub fn append(&self, kind: EntryKind) -> String {
        self.append_stamped(kind, now())
    }

    /// [`SessionStore::append`] for an entry that was written at `at` somewhere else.
    pub fn append_at(&self, kind: EntryKind, at: jiff::Timestamp) -> String {
        self.append_stamped(kind, at.to_string())
    }

    fn append_stamped(&self, kind: EntryKind, timestamp: String) -> String {
        let mut inner = self.lock();
        let id = loop {
            // The tail of a v7 UUID is its random part.
            let candidate = uuid::Uuid::now_v7().simple().to_string()[24..].to_string();
            if !inner.index.contains_key(&candidate) {
                break candidate;
            }
        };
        let aside = kind.is_journal();
        let entry = Entry {
            id: id.clone(),
            parent_id: inner.leaf.clone(),
            timestamp,
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
        if !aside {
            inner.leaf = Some(id.clone());
        }
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

    /// The first value `pick` finds in an entry of the branch ending at `leaf` (default: the
    /// current leaf), looking from the leaf up: the newest entry of a kind, without copying
    /// the branch.
    pub fn latest<T>(&self, leaf: Option<&str>, mut pick: impl FnMut(&Entry) -> Option<T>) -> Option<T> {
        let inner = self.lock();
        let mut cursor = leaf.or(inner.leaf.as_deref());
        let mut walked = 0;
        while let Some(&i) = cursor.and_then(|id| inner.index.get(id)) {
            let entry = &inner.entries[i];
            if let Some(found) = pick(entry) {
                return Some(found);
            }
            walked += 1;
            if walked > inner.entries.len() {
                break; // a parent cycle in a damaged file
            }
            cursor = entry.parent_id.as_deref();
        }
        None
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

    /// The run at `leaf` (default: the current leaf) that the end of the process cut short:
    /// the last run of the branch, when its end was never recorded and something is left to do.
    pub fn interrupted_run(&self, leaf: Option<&str>) -> Option<InterruptedRun> {
        let inner = self.lock();
        interrupted(&inner.entries, &inner.index, leaf.or(inner.leaf.as_deref()))
    }

    /// [`SessionStore::interrupted_run`] of the branch a session file ends on, read without
    /// opening the file for writing.
    pub fn interrupted_in(path: &Path) -> Option<InterruptedRun> {
        let (_, entries) = read_file(path).ok()?;
        let index = entries
            .iter()
            .enumerate()
            .map(|(at, entry)| (entry.id.clone(), at))
            .collect();
        let leaf = entries.iter().rev().find(|entry| !entry.kind.is_journal());
        interrupted(&entries, &index, leaf.map(|entry| entry.id.as_str()))
    }

    /// The run the prompt named `request_id` started, when this session has seen one.
    pub fn request_run(&self, request_id: &str) -> Option<RequestRun> {
        let inner = self.lock();
        let (at, run_id) =
            inner
                .entries
                .iter()
                .enumerate()
                .rev()
                .find_map(|(at, entry)| match &entry.kind {
                    EntryKind::RunStarted {
                        run_id,
                        request_id: Some(named),
                    } if named == request_id => Some((at, run_id.clone())),
                    _ => None,
                })?;
        let mut run = RequestRun {
            run_id,
            stop: None,
            text: None,
        };
        // One agent writes the file and its runs follow each other, so what follows the start
        // up to the end is the run.
        for entry in &inner.entries[at + 1..] {
            match &entry.kind {
                EntryKind::RunEnded { run_id, stop } if *run_id == run.run_id => {
                    run.stop = Some(stop.clone());
                    break;
                }
                EntryKind::RunStarted { .. } => break,
                EntryKind::Message { message }
                    if message.role == Role::Assistant && !message.text().trim().is_empty() =>
                {
                    run.text = Some(message.text());
                }
                _ => {}
            }
        }
        Some(run)
    }

    /// Copies the branch ending at `at` (default: the current leaf) into a new session in `dir`.
    /// The journal of its runs stays behind: a run that goes on, or was cut short, in the
    /// original is not one of the copy.
    pub fn fork(&self, dir: &Path, at: Option<&str>) -> io::Result<SessionStore> {
        let branch = self.branch(at);
        let (cwd, source) = {
            let inner = self.lock();
            (PathBuf::from(&inner.header.cwd), inner.path.clone())
        };
        let fork = SessionStore::create(dir, &cwd, Some(&source))?;
        fork.extend(branch)?;
        Ok(fork)
    }

    /// A new session in `dir` that works in `cwd` and holds `entries` as they are, from a file
    /// written elsewhere (`azazel sessions import`). Its file name, header and id carry
    /// `created`; the id is its own, so a session brought in twice is two sessions.
    pub fn import(
        dir: &Path,
        cwd: &Path,
        created: jiff::Timestamp,
        entries: Vec<Entry>,
    ) -> io::Result<SessionStore> {
        let store = SessionStore::create_dated(dir, cwd, created)?;
        store.extend(entries)?;
        Ok(store)
    }

    /// Writes `entries` after those the store holds, as they are. The leaf moves to the last
    /// of them that is part of the conversation.
    fn extend(&self, entries: Vec<Entry>) -> io::Result<()> {
        let mut inner = self.lock();
        for entry in entries {
            if let Some(file) = &mut inner.file {
                writeln!(file, "{}", serde_json::to_string(&entry)?)?;
            }
            if !entry.kind.is_journal() {
                inner.leaf = Some(entry.id.clone());
            }
            let position = inner.entries.len();
            inner.index.insert(entry.id.clone(), position);
            inner.entries.push(entry);
        }
        Ok(())
    }

    /// Moves the session in `path`, which no process has open, into `dir` for work in `cwd`:
    /// the file keeps its name and id, its header names `cwd`, and the transcripts of its
    /// sub-agents go with it. Returns where the file is now. The header is rewritten in place
    /// when `dir` is where the file already is.
    pub fn relocate(path: &Path, dir: &Path, cwd: &Path) -> io::Result<PathBuf> {
        let text = std::fs::read_to_string(path)?;
        let (first, rest) = text.split_once('\n').unwrap_or((text.as_str(), ""));
        let mut header: Header = serde_json::from_str(first).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{}: not a session file", path.display()),
            )
        })?;
        header.cwd = cwd.to_string_lossy().into_owned();
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{}: no file name", path.display()),
            )
        })?;
        std::fs::create_dir_all(dir)?;
        let target = dir.join(name);
        let moving = crate::paths::canonical(&target) != crate::paths::canonical(path);
        if moving && target.exists() {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!("{} already exists", target.display()),
            ));
        }
        // Written beside its place and renamed into it, so the file is never seen half written.
        let temp = target.with_extension(format!("jsonl.{}.tmp", std::process::id()));
        let written = serde_json::to_string(&header)
            .map_err(io::Error::from)
            .and_then(|line| std::fs::write(&temp, format!("{line}\n{rest}")))
            .and_then(|()| std::fs::rename(&temp, &target));
        if let Err(e) = written {
            let _ = std::fs::remove_file(&temp);
            return Err(e);
        }
        if moving {
            let agents = path.parent().map(|old| old.join(&header.id));
            if let Some(agents) = agents.filter(|folder| folder.is_dir()) {
                move_dir(&agents, &dir.join(&header.id))?;
            }
            std::fs::remove_file(path)?;
        }
        Ok(target)
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

    /// The primary agent the branch ending at `leaf` last switched to.
    pub fn last_agent(&self, leaf: Option<&str>) -> Option<String> {
        self.branch(leaf).into_iter().rev().find_map(|e| match e.kind {
            EntryKind::AgentChange { agent } => Some(agent),
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
            // A note about a run is no way the conversation went.
            if entry.kind.is_journal() {
                continue;
            }
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
        cwd: header.cwd,
        archived: false,
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
            EntryKind::Archive { archived } => summary.archived = *archived,
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
    let mut cursor = entries.iter().rev().find(|entry| !entry.kind.is_journal());
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
                EntryKind::SessionInfo { .. }
                    | EntryKind::Pin { .. }
                    | EntryKind::Archive { .. }
                    | EntryKind::Label { .. }
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

/// Moves a directory; across file systems, where it cannot be renamed, by copying it.
fn move_dir(from: &Path, to: &Path) -> io::Result<()> {
    if std::fs::rename(from, to).is_ok() {
        return Ok(());
    }
    copy_dir(from, to)?;
    std::fs::remove_dir_all(from)
}

fn copy_dir(from: &Path, to: &Path) -> io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let target = to.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}

/// The lines of a session file, or of the entries of one as `azazel export --format jsonl`
/// writes them: the header when the text starts with one, the entries that parse, and how many
/// lines did not.
pub fn parse_lines(text: &str) -> (Option<Header>, Vec<Entry>, usize) {
    let mut lines = text.lines().filter(|line| !line.trim().is_empty()).peekable();
    let header = lines
        .peek()
        .and_then(|line| serde_json::from_str::<Header>(line).ok());
    if header.is_some() {
        lines.next();
    }
    let mut entries = Vec::new();
    let mut skipped = 0;
    for line in lines {
        match serde_json::from_str::<Entry>(line) {
            Ok(entry) => entries.push(entry),
            Err(_) => skipped += 1,
        }
    }
    (header, entries, skipped)
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

    fn run_started(&self, request_id: Option<&str>) -> String {
        // The tail of a v7 UUID is its random part.
        let run_id = uuid::Uuid::now_v7().simple().to_string()[20..].to_string();
        self.append(EntryKind::RunStarted {
            run_id: run_id.clone(),
            request_id: request_id.map(str::to_string),
        });
        run_id
    }

    fn request_started(&self, model: &str, via: Option<&str>) {
        self.append(EntryKind::RequestStarted {
            model: model.to_string(),
            via: via.map(str::to_string),
        });
    }

    fn tool_started(&self, call_id: &str, name: &str, input: &Value, repairs: &[String]) {
        self.append(EntryKind::ToolStarted {
            call_id: call_id.to_string(),
            name: name.to_string(),
            input: input.clone(),
            repairs: repairs.to_vec(),
        });
    }

    fn run_ended(&self, run_id: &str, stop: &str) {
        self.append(EntryKind::RunEnded {
            run_id: run_id.to_string(),
            stop: stop.to_string(),
        });
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

    fn calling(calls: &[(&str, &str)]) -> Message {
        Message {
            role: Role::Assistant,
            content: calls
                .iter()
                .map(|(id, name)| Block::ToolCall {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: json!({}),
                    signature: None,
                })
                .collect(),
            meta: None,
        }
    }

    #[test]
    fn a_run_without_an_end_is_interrupted_where_its_last_entry_says() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        assert_eq!(store.interrupted_run(None), None);

        // A run that ended is not interrupted, whatever it did.
        store.message(&Message::user("one"));
        let first = store.run_started(None);
        store.request_started("mock/lead", None);
        store.message(&assistant("done"));
        store.run_ended(&first, "stop");
        assert_eq!(store.interrupted_run(None), None);

        // The process ends while the model is asked: the request is what is left to do.
        store.message(&Message::user("two"));
        let run_id = store.run_started(Some("req-1"));
        store.request_started("mock/lead", None);
        let waiting = store.interrupted_run(None).unwrap();
        assert_eq!(
            (waiting.run_id.as_str(), waiting.request_id.as_deref()),
            (run_id.as_str(), Some("req-1"))
        );
        assert!(waiting.open_calls.is_empty() && waiting.started.is_empty());
        assert_eq!(waiting.describe(), "stopped while waiting for mock/lead");

        // Then while tools run: the calls without a result, and which of them had started.
        store.message(&calling(&[("c1", "read"), ("c2", "shell"), ("c3", "write")]));
        store.tool_started("c1", "read", &json!({}), &[]);
        let mended = ["called as `bash`; the tool is `shell`".to_string()];
        store.tool_started("c2", "shell", &json!({ "command": "make" }), &mended);
        let reopened = SessionStore::open(&store.path()).unwrap();
        // What was mended in a call is kept with it.
        let kept: Vec<Vec<String>> = reopened
            .entries()
            .into_iter()
            .filter_map(|entry| match entry.kind {
                EntryKind::ToolStarted { repairs, .. } => Some(repairs),
                _ => None,
            })
            .collect();
        assert_eq!(kept, [Vec::new(), mended.to_vec()]);
        for session in [&store, &reopened] {
            let run = session.interrupted_run(None).unwrap();
            assert_eq!(
                run.open_calls,
                [
                    ("c1".to_string(), "read".to_string()),
                    ("c2".to_string(), "shell".to_string()),
                    ("c3".to_string(), "write".to_string())
                ]
            );
            assert_eq!(run.started, ["c1", "c2"]);
            assert_eq!(run.describe(), "stopped while read, shell ran");
        }
        assert_eq!(
            SessionStore::interrupted_in(&store.path()),
            store.interrupted_run(None)
        );

        // Once the results are in and the reply is whole there is nothing to go on with.
        store.message(&Message {
            role: Role::Tool,
            content: vec![Block::tool_result("c1", "ok", false)],
            meta: None,
        });
        assert!(
            store
                .interrupted_run(None)
                .is_some_and(|run| run.open_calls.is_empty())
        );
        store.message(&assistant("all done"));
        assert_eq!(store.interrupted_run(None), None);
        store.run_ended(&run_id, "stop");
        assert_eq!(SessionStore::interrupted_in(&store.path()), None);
    }

    #[test]
    fn the_journal_of_a_run_hangs_beside_the_conversation() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        let prompt = store.message(&Message::user("go"));
        let run_id = store.run_started(None);
        store.request_started("mock/lead", None);
        let middle = store.message(&calling(&[("c1", "shell")]));
        store.tool_started("c1", "shell", &json!({}), &[]);
        let result = store.message(&Message {
            role: Role::Tool,
            content: vec![Block::tool_result("c1", "ok", false)],
            meta: None,
        });
        let last = store.message(&assistant("done"));
        store.run_ended(&run_id, "stop");

        // The conversation is the messages alone: each follows the one before it, and what
        // the run noted on the way hangs from the message it was noted after.
        let branch: Vec<String> = store.branch(None).into_iter().map(|entry| entry.id).collect();
        assert_eq!(branch, [prompt.clone(), middle.clone(), result, last.clone()]);
        assert_eq!(store.leaf(), Some(last.clone()));
        let hangs_from = |kind: fn(&EntryKind) -> bool| {
            store
                .entries()
                .into_iter()
                .find(|entry| kind(&entry.kind))
                .and_then(|entry| entry.parent_id)
        };
        assert_eq!(
            hangs_from(|kind| matches!(kind, EntryKind::RunStarted { .. })),
            Some(prompt)
        );
        assert_eq!(
            hangs_from(|kind| matches!(kind, EntryKind::ToolStarted { .. })),
            Some(middle.clone())
        );
        // It is no way the conversation went, and a session opened again goes on from what
        // was said last, not from the note that the run ended.
        assert_eq!(store.facts().branch_points, 0);
        let reopened = SessionStore::open(&store.path()).unwrap();
        assert_eq!(reopened.leaf(), Some(last.clone()));
        assert_eq!(reopened.build_context(None), store.build_context(None));

        // A note is no place to continue from: the entry it hangs from is.
        let noted = store.entries().last().map(|entry| entry.id.clone());
        assert!(store.set_leaf(noted.as_deref()));
        assert_eq!(store.leaf(), Some(last.clone()));
        // Taken up in the middle of the run, the conversation is not that of a run cut
        // short: the run's end is in the file.
        assert!(store.set_leaf(Some(&middle)));
        assert_eq!(store.interrupted_run(None), None);
        // A copy holds the conversation and nothing of the runs.
        let fork = store.fork(dir.path(), Some(&middle)).unwrap();
        assert!(fork.entries().iter().all(|entry| !entry.kind.is_journal()));
        assert_eq!(fork.interrupted_run(None), None);
    }

    #[test]
    fn a_run_cut_short_is_found_from_the_branch_it_is_on() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        // A run that ended, then one that was cut short while a call ran.
        let root = store.message(&Message::user("one"));
        let ended = store.run_started(None);
        store.message(&assistant("done"));
        store.run_ended(&ended, "stop");
        let prompt = store.message(&Message::user("two"));
        let cut = store.run_started(None);
        store.message(&calling(&[("c1", "shell")]));
        store.tool_started("c1", "shell", &json!({}), &[]);
        let cut_leaf = store.leaf();
        assert_eq!(
            store.interrupted_run(None).map(|run| run.run_id),
            Some(cut.clone())
        );

        // Another branch, from the first prompt: its own run ends; the calls there have the
        // same ids, as a scripted model's do.
        assert!(store.set_leaf(Some(&root)));
        store.message(&Message::user("other"));
        let other = store.run_started(None);
        store.message(&calling(&[("c1", "write")]));
        store.tool_started("c1", "write", &json!({}), &[]);
        store.message(&assistant("did the other thing"));
        store.run_ended(&other, "stop");
        assert_eq!(store.interrupted_run(None), None);

        // Back on the first branch the run is still the one that was cut short, with what
        // it had started and nothing of the other branch.
        let run = store.interrupted_run(cut_leaf.as_deref()).unwrap();
        assert_eq!(run.run_id, cut);
        assert_eq!(run.open_calls, [("c1".to_string(), "shell".to_string())]);
        assert_eq!(run.started, ["c1"]);
        // From before its prompt there is no run to go on with.
        assert_eq!(store.interrupted_run(Some(&root)), None);
        assert!(store.interrupted_run(Some(&prompt)).is_some());
    }

    #[test]
    fn a_named_request_is_found_with_how_its_run_ended() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work"), None).unwrap();
        assert_eq!(store.request_run("req-1"), None);
        store.message(&Message::user("go"));
        let run_id = store.run_started(Some("req-1"));
        store.message(&assistant("working on it"));
        assert_eq!(
            store.request_run("req-1"),
            Some(RequestRun {
                run_id: run_id.clone(),
                stop: None,
                text: Some("working on it".into())
            })
        );
        store.message(&assistant("done"));
        store.run_ended(&run_id, "stop");
        // What another run says later is not this one's.
        store.message(&Message::user("more"));
        let other = store.run_started(Some("req-2"));
        store.message(&assistant("more done"));
        store.run_ended(&other, "aborted");
        let reopened = SessionStore::open(&store.path()).unwrap();
        assert_eq!(
            reopened.request_run("req-1"),
            Some(RequestRun {
                run_id,
                stop: Some("stop".into()),
                text: Some("done".into())
            })
        );
        assert_eq!(
            reopened.request_run("req-2").and_then(|run| run.stop),
            Some("aborted".to_string())
        );
        assert_eq!(reopened.request_run("req-3"), None);
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

    #[test]
    fn summaries_say_where_a_session_works_and_whether_it_is_archived() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), Path::new("/work/app"), None).unwrap();
        store.message(&Message::user("run in ci"));
        let used = SessionStore::list(dir.path())[0].updated.clone();
        std::thread::sleep(std::time::Duration::from_millis(5));
        store.append(EntryKind::Archive { archived: true });
        let listed = &SessionStore::list(dir.path())[0];
        assert_eq!((listed.cwd.as_str(), listed.archived), ("/work/app", true));
        // Archiving is not use: the session keeps its place in time.
        assert_eq!(listed.updated, used);
    }

    #[test]
    fn an_export_is_imported_as_a_new_session_of_its_own() {
        let from = tempfile::tempdir().unwrap();
        let original = SessionStore::create(from.path(), Path::new("/there"), None).unwrap();
        original.append(EntryKind::SessionInfo {
            name: "shared".into(),
        });
        original.message(&Message::user("question"));
        original.message(&assistant("answer"));
        let exported: String = original
            .branch(None)
            .iter()
            .map(|entry| serde_json::to_string(entry).unwrap() + "\n")
            .collect();

        // The lines of an export, and those of a whole session file, both read.
        let (header, entries, skipped) = parse_lines(&format!("{exported}not json\n"));
        assert_eq!((header, entries.len(), skipped), (None, 3, 1));
        let whole = std::fs::read_to_string(original.path()).unwrap();
        let (header, entries, skipped) = parse_lines(&whole);
        assert_eq!(
            (header.map(|h| h.id), entries.len(), skipped),
            (Some(original.id()), 3, 0)
        );

        let to = tempfile::tempdir().unwrap();
        let created: jiff::Timestamp = "2026-01-02T03:04:05Z".parse().unwrap();
        let imported = SessionStore::import(to.path(), Path::new("/here"), created, entries).unwrap();
        assert_ne!(imported.id(), original.id());
        let reopened = SessionStore::open(&imported.path()).unwrap();
        assert_eq!(reopened.header().cwd, "/here");
        assert_eq!(reopened.header().timestamp, created.to_string());
        assert_eq!(reopened.name().as_deref(), Some("shared"));
        assert_eq!(texts(&reopened.build_context(None)), ["question", "answer"]);
        let listed = SessionStore::list(to.path());
        assert_eq!((listed.len(), listed[0].messages), (1, 2));
    }

    #[test]
    fn a_relocated_session_moves_with_its_agents_and_names_its_new_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let old = tmp.path().join("old");
        let new = tmp.path().join("new");
        let store = SessionStore::create(&old, Path::new("/first"), None).unwrap();
        store.message(&Message::user("keep me"));
        let agents = old.join(store.id()).join(crate::paths::AGENT_TRANSCRIPTS);
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::write(agents.join("scout.jsonl"), "{}").unwrap();
        let file = store.path();
        drop(store);

        // In place, only the header changes.
        let same = SessionStore::relocate(&file, &old, Path::new("/second")).unwrap();
        assert_eq!(same, file);
        assert_eq!(SessionStore::open(&file).unwrap().header().cwd, "/second");

        let moved = SessionStore::relocate(&file, &new, Path::new("/third")).unwrap();
        assert_eq!(moved, new.join(file.file_name().unwrap()));
        assert!(!file.exists());
        let reopened = SessionStore::open(&moved).unwrap();
        assert_eq!(reopened.header().cwd, "/third");
        assert_eq!(texts(&reopened.build_context(None)), ["keep me"]);
        let id = reopened.id();
        assert!(
            new.join(&id)
                .join(crate::paths::AGENT_TRANSCRIPTS)
                .join("scout.jsonl")
                .is_file()
        );
        assert!(!old.join(&id).exists());
        // Nothing is overwritten.
        let other = SessionStore::create(&old, Path::new("/x"), None).unwrap();
        let clash = new.join(other.path().file_name().unwrap());
        std::fs::write(&clash, "taken").unwrap();
        let other_file = other.path();
        drop(other);
        assert!(SessionStore::relocate(&other_file, &new, Path::new("/y")).is_err());
        assert!(other_file.exists());
    }
}
