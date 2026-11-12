//! The session transcript: an append-only JSONL tree with an in-memory leaf pointer.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::UNIX_EPOCH;

use azazel_agent::compaction::{apply_context_edit, summary_message};
use azazel_agent::{ContextEntry, TranscriptSink};
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
    /// When its last entry was written.
    pub updated: String,
    pub name: Option<String>,
    /// Text of the first user message, for pickers.
    pub first_prompt: Option<String>,
    pub messages: usize,
    pub pinned: bool,
    /// Tokens and cost of the lead's replies.
    pub usage: Usage,
}

/// File in a sessions directory that caches each session's summary, so listing does not read
/// every session file again.
const INDEX_FILE: &str = "index.json";

#[derive(Default, Serialize, Deserialize)]
struct Index {
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
    file: File,
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
                file,
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
                file,
                header,
                entries,
                index,
                leaf,
            }),
        })
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
        let written = serde_json::to_string(&entry)
            .map_err(io::Error::from)
            .and_then(|line| writeln!(inner.file, "{line}"));
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
                writeln!(inner.file, "{}", serde_json::to_string(&entry)?)?;
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
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default();
        let mut index = Index::default();
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
        id: header.id,
        updated: entries
            .last()
            .map_or_else(|| header.timestamp.clone(), |e| e.timestamp.clone()),
        timestamp: header.timestamp,
        name: None,
        first_prompt: None,
        messages: 0,
        pinned: false,
        usage: Usage::default(),
    };
    for entry in &entries {
        match &entry.kind {
            EntryKind::Message { message } => {
                summary.messages += 1;
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

        // A file written to is read again, and is now the most recently used.
        std::thread::sleep(std::time::Duration::from_millis(5));
        a.append(EntryKind::Pin { pinned: true });
        let list = SessionStore::list(dir.path());
        assert_eq!(list[0].id, a.id());
        assert_eq!((list[0].name.as_deref(), list[0].pinned), (None, true));
        assert_eq!(list[1].name.as_deref(), Some("cached"));

        // A deleted session leaves the index.
        drop(b);
        std::fs::remove_file(&list[1].path).unwrap();
        assert_eq!(SessionStore::list(dir.path()).len(), 1);
        let index: Value = serde_json::from_slice(&std::fs::read(&index_path).unwrap()).unwrap();
        assert_eq!(index["files"].as_object().unwrap().len(), 1);
    }
}
