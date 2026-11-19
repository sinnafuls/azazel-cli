//! Handing a session's work to a new one: the request that has the model write the brief, the
//! draft that is made without a model and kept when that request fails, and the ranking of the
//! files the session worked on. Shared by `/handoff` in the terminal UI and the `handoff` command
//! of the protocol.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};

use azazel_ai::{Block, Message, Role};
use azazel_core::{EntryKind, SessionStore};
use azazel_tools::{TodoItem, TodoStatus};
use azazel_tui::text;
use serde_json::Value;

use crate::retry::{HANDOFF_NOTE, is_prompt, typed};

/// Files named in a brief, most used first.
const MAX_FILES: usize = 15;
/// What each kind of use adds to a file's score.
const WEIGHT_EDIT: u32 = 3;
const WEIGHT_READ: u32 = 2;
const WEIGHT_SHELL: u32 = 1;
/// Requests listed in the draft, the newest of them.
const MAX_REQUESTS: usize = 12;
/// Longest request in the draft, and longest last reply, in columns.
const EXCERPT: usize = 300;
const LAST_SAID: usize = 800;
/// Longest name a continued session is given.
const MAX_NAME_CHARS: usize = 80;

/// A file the session worked on and how much.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankedFile {
    pub path: String,
    pub score: u32,
}

/// The request that has the model write the brief. `files` is what [`ranked_files`] found: the
/// model is told which files mattered rather than left to guess it from a long conversation.
pub fn prompt(focus: &str, files: &[RankedFile]) -> String {
    let mut text = "Write a hand-off brief for whoever continues this work in a new session that has none of this \
        conversation. Use exactly these sections: Requests (each thing the user asked for, with its status: done, \
        in progress or not started), Where things stand, Rules the user set (how they want the work done, from \
        now on), Files (the ones that matter and why, the most important first), Errors already solved (what \
        failed and what fixed it, so nobody tries it again), Next steps. Be specific: name files, commands and \
        results. At most 1500 words. Do not repeat what AGENTS.md or other project instructions say: the new \
        session reads them itself. Reply with the brief only."
        .to_string();
    if !files.is_empty() {
        text.push_str(
            "\n\nFiles by use in this session (edits count 3, reads 2, mentions in shell commands 1):",
        );
        for file in files {
            text.push_str(&format!("\n- {} ({})", file.path, file.score));
        }
    }
    if !focus.trim().is_empty() {
        text.push_str(&format!("\n\nPut the weight on: {}", focus.trim()));
    }
    text
}

/// The messages of the branch the session is on, compactions and all: a file read before the
/// conversation was compacted still counts.
fn messages_of(session: &SessionStore) -> Vec<Message> {
    session
        .branch(None)
        .into_iter()
        .filter_map(|entry| match entry.kind {
            EntryKind::Message { message } => Some(message),
            _ => None,
        })
        .collect()
}

/// The conversation as context for a request on the side. A reply that was cut off while it was
/// calling tools, with some or none of its results recorded, is still open, and a provider
/// rejects that; it is left out.
pub fn closed_context(session: &SessionStore) -> Vec<Message> {
    settled(
        session
            .build_context(None)
            .into_iter()
            .map(|entry| entry.message)
            .collect(),
    )
}

/// The conversation up to the last point where every tool call has its result: a side question
/// asked while tools run, or after a run was cut off, must not carry calls that are still open.
pub fn settled(mut context: Vec<Message>) -> Vec<Message> {
    let Some(at) = context
        .iter()
        .rposition(|message| message.role == Role::Assistant && message.tool_calls().next().is_some())
    else {
        return context;
    };
    let answered: Vec<&str> = context[at + 1..]
        .iter()
        .flat_map(|message| &message.content)
        .filter_map(|block| match block {
            Block::ToolResult { call_id, .. } => Some(call_id.as_str()),
            _ => None,
        })
        .collect();
    if !context[at].tool_calls().all(|(id, _, _)| answered.contains(&id)) {
        context.truncate(at);
    }
    context
}

/// The files of the session's tool calls, most used first, at most [`MAX_FILES`], and only those
/// that still exist. An edit, a write or a patch weighs 3, a read 2, a path named in a shell
/// command 1.
pub fn ranked_files(session: &SessionStore, cwd: &Path) -> Vec<RankedFile> {
    rank(&messages_of(session), cwd)
}

fn rank(messages: &[Message], cwd: &Path) -> Vec<RankedFile> {
    let mut scores: HashMap<String, u32> = HashMap::new();
    let mut add = |path: &str, weight: u32| {
        if let Some(path) = existing(path, cwd) {
            *scores.entry(path).or_default() += weight;
        }
    };
    let path_of = |input: &Value| input.get("path").and_then(Value::as_str).map(str::to_string);
    for message in messages.iter().filter(|message| message.role == Role::Assistant) {
        for (_, name, input) in message.tool_calls() {
            match name {
                "edit" | "write" => {
                    if let Some(path) = path_of(input) {
                        add(&path, WEIGHT_EDIT);
                    }
                }
                "patch" => {
                    let patch = input
                        .get("patch_text")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    for path in patched_paths(patch) {
                        add(path, WEIGHT_EDIT);
                    }
                }
                "read" => {
                    if let Some(path) = path_of(input) {
                        add(&path, WEIGHT_READ);
                    }
                }
                "shell" => {
                    let command = input.get("command").and_then(Value::as_str).unwrap_or_default();
                    for word in shell_words(command) {
                        add(word, WEIGHT_SHELL);
                    }
                }
                _ => {}
            }
        }
    }
    let mut ranked: Vec<RankedFile> = scores
        .into_iter()
        .map(|(path, score)| RankedFile { path, score })
        .collect();
    ranked.sort_by(|a, b| b.score.cmp(&a.score).then_with(|| a.path.cmp(&b.path)));
    ranked.truncate(MAX_FILES);
    ranked
}

/// The files a patch adds, updates, deletes or moves to.
fn patched_paths(patch: &str) -> impl Iterator<Item = &str> {
    patch.lines().filter_map(|line| {
        [
            "*** Add File: ",
            "*** Update File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ]
        .iter()
        .find_map(|head| line.strip_prefix(head))
        .map(str::trim)
    })
}

/// The words of a shell command that could be a path.
fn shell_words(command: &str) -> impl Iterator<Item = &str> {
    command
        .split(|c: char| c.is_whitespace() || "\"'`;|&<>()=,".contains(c))
        .filter(|word| word.len() > 1 && !word.starts_with('-') && !word.contains("://"))
        .filter(|word| word.contains(['/', '\\', '.']))
}

/// `path` as it is shown (relative to `cwd` when it is inside it, with `/`), when it names a
/// file that exists.
fn existing(path: &str, cwd: &Path) -> Option<String> {
    let given = Path::new(path.trim());
    let full = if given.is_absolute() {
        given.to_path_buf()
    } else {
        cwd.join(given)
    };
    if !full.is_file() {
        return None;
    }
    let shown = full.strip_prefix(cwd).unwrap_or(&full);
    let shown = shown.to_string_lossy().replace('\\', "/");
    Some(shown.strip_prefix("./").unwrap_or(&shown).to_string())
}

/// A brief made without a model: the requests the user made and whether they were answered,
/// where the task list and the last reply left things, and the ranked files. Secrets are masked.
pub fn draft(session: &SessionStore, todos: &[TodoItem], cwd: &Path) -> String {
    let messages = messages_of(session);
    let mut requests: Vec<(String, bool)> = Vec::new();
    for message in &messages {
        match message.role {
            Role::User => {
                let said = message.text();
                if is_prompt(&said) && !said.trim().is_empty() {
                    requests.push((typed(&said).to_string(), false));
                }
            }
            Role::Assistant if message.tool_calls().next().is_none() && !message.text().trim().is_empty() => {
                if let Some(last) = requests.last_mut() {
                    last.1 = true;
                }
            }
            _ => {}
        }
    }
    let mut out = String::from("## Requests\n");
    let left_out = requests.len().saturating_sub(MAX_REQUESTS);
    if left_out > 0 {
        out.push_str(&format!("({left_out} earlier requests left out)\n"));
    }
    if requests.is_empty() {
        out.push_str("none\n");
    }
    for (said, answered) in requests.iter().skip(left_out) {
        let status = if *answered { "answered" } else { "no answer yet" };
        out.push_str(&format!("- [{status}] {}\n", one_line(said, EXCERPT)));
    }
    out.push_str("\n## Where things stand\n");
    for item in todos {
        let mark = match item.status {
            TodoStatus::Pending => "pending",
            TodoStatus::InProgress => "in progress",
            TodoStatus::Done => "done",
        };
        out.push_str(&format!("- task [{mark}] {}\n", one_line(&item.text, EXCERPT)));
    }
    let last_said = messages
        .iter()
        .rev()
        .filter(|message| message.role == Role::Assistant)
        .map(Message::text)
        .find(|said| !said.trim().is_empty());
    match last_said {
        Some(said) => out.push_str(&format!("Last reply: {}\n", one_line(&said, LAST_SAID))),
        None if todos.is_empty() => out.push_str("nothing was answered yet\n"),
        None => {}
    }
    out.push_str("\n## Files\n");
    let files = rank(&messages, cwd);
    if files.is_empty() {
        out.push_str("none\n");
    }
    for file in &files {
        out.push_str(&format!("- {} ({})\n", file.path, file.score));
    }
    text::redact(&out).into_owned()
}

fn one_line(said: &str, max: usize) -> String {
    text::truncate(&said.split_whitespace().collect::<Vec<_>>().join(" "), max, "…")
}

/// Keeps `draft` in `dir` so that a request that fails still leaves a brief; returns its path.
pub fn save_draft(dir: &Path, session_id: &str, draft: &str) -> io::Result<PathBuf> {
    std::fs::create_dir_all(dir)?;
    let file = dir.join(format!("handoff-{session_id}.md"));
    std::fs::write(&file, draft)?;
    Ok(file)
}

/// Starts the session that continues `old` in `dir`: its first message is `brief`, masked, and
/// nothing answers it until the user writes. Returns the new session's file, closed again.
pub fn create_continuation(dir: &Path, cwd: &Path, old: &SessionStore, brief: &str) -> io::Result<PathBuf> {
    let new = SessionStore::create(dir, cwd, Some(&old.path()))?;
    new.append(EntryKind::Message {
        message: Message::user(format!(
            "{HANDOFF_NOTE}{}:\n\n{}",
            old.id(),
            text::redact(brief.trim())
        )),
    });
    if let Some(name) = old.name() {
        new.append(EntryKind::SessionInfo {
            name: text::truncate(&format!("{name} (continued)"), MAX_NAME_CHARS, "…"),
        });
    }
    Ok(new.path())
}

#[cfg(test)]
mod tests {
    use azazel_ai::Block;
    use serde_json::json;

    use super::*;

    fn call(name: &str, input: Value) -> Message {
        Message::assistant(
            vec![Block::ToolCall {
                id: "c".to_string(),
                name: name.to_string(),
                input,
                signature: None,
            }],
            azazel_ai::AssistantMeta {
                provider: "mock".into(),
                model: "lead".into(),
                api: azazel_ai::WireApi::AnthropicMessages,
                usage: Default::default(),
                stop: azazel_ai::StopReason::ToolUse,
                response_id: None,
                duration_ms: 0,
                sources: Vec::new(),
            },
        )
    }

    fn says(text: &str) -> Message {
        let mut message = call("x", json!({}));
        message.content = vec![Block::text(text)];
        message
    }

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for file in ["a.rs", "b.rs", "c.rs", "notes.md"] {
            std::fs::write(dir.path().join(file), "x").unwrap();
        }
        dir
    }

    #[test]
    fn files_are_ranked_by_edits_then_reads_then_shell_mentions_and_gone_files_are_left_out() {
        let dir = project();
        let messages = vec![
            call("read", json!({"path": "a.rs"})),
            call("read", json!({"path": "a.rs"})),
            call("read", json!({"path": "b.rs"})),
            call("edit", json!({"path": "b.rs", "edits": []})),
            call("write", json!({"path": "gone.rs", "content": ""})),
            call(
                "shell",
                json!({"command": "cargo test --manifest-path c.rs && cat notes.md ./notes.md"}),
            ),
            call(
                "patch",
                json!({"patch_text": "*** Begin Patch\n*** Update File: c.rs\n*** End Patch"}),
            ),
        ];
        let ranked: Vec<(String, u32)> = rank(&messages, dir.path())
            .into_iter()
            .map(|file| (file.path, file.score))
            .collect();
        assert_eq!(
            ranked,
            [("b.rs", 5), ("a.rs", 4), ("c.rs", 4), ("notes.md", 2)]
                .map(|(path, score)| (path.to_string(), score))
        );
    }

    #[test]
    fn only_the_fifteen_best_files_are_kept() {
        let dir = tempfile::tempdir().unwrap();
        let mut messages = Vec::new();
        for n in 0..20 {
            std::fs::write(dir.path().join(format!("f{n:02}.rs")), "x").unwrap();
            messages.push(call("read", json!({"path": format!("f{n:02}.rs")})));
        }
        messages.push(call("edit", json!({"path": "f19.rs"})));
        let ranked = rank(&messages, dir.path());
        assert_eq!(ranked.len(), MAX_FILES);
        assert_eq!(
            (ranked[0].path.as_str(), ranked[1].path.as_str()),
            ("f19.rs", "f00.rs")
        );
    }

    #[test]
    fn the_draft_lists_requests_with_their_status_tasks_and_files_and_masks_secrets() {
        let dir = project();
        let store = SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap();
        for message in [
            Message::user("add the clock to a.rs"),
            says("done, the clock is in"),
            Message::user("now use key sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789 in b.rs"),
            call("edit", json!({"path": "a.rs", "edits": []})),
        ] {
            store.append(EntryKind::Message { message });
        }
        let todos = vec![TodoItem {
            text: "wire the chip".to_string(),
            status: TodoStatus::InProgress,
        }];
        let draft = draft(&store, &todos, dir.path());
        assert!(draft.contains("- [answered] add the clock to a.rs"), "{draft}");
        assert!(draft.contains("- [no answer yet] now use key"), "{draft}");
        assert!(draft.contains("- task [in progress] wire the chip"), "{draft}");
        assert!(draft.contains("Last reply: done, the clock is in"), "{draft}");
        assert!(draft.contains("- a.rs (3)"), "{draft}");
        assert!(!draft.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{draft}");
    }

    #[test]
    fn the_continuation_starts_from_the_masked_brief_and_keeps_the_name() {
        let dir = project();
        let sessions = dir.path().join("sessions");
        let old = SessionStore::create(&sessions, dir.path(), None).unwrap();
        old.append(EntryKind::SessionInfo {
            name: "the clock".to_string(),
        });
        let file = create_continuation(
            &sessions,
            dir.path(),
            &old,
            "token ghp_abcdefghijklmnopqrstuvwxyz0123456789 stays out",
        )
        .unwrap();
        let new = SessionStore::open(&file).unwrap();
        let first = new
            .build_context(None)
            .into_iter()
            .map(|entry| entry.message.text())
            .next()
            .unwrap();
        assert!(
            first.starts_with(&format!("{HANDOFF_NOTE}{}:", old.id())),
            "{first}"
        );
        assert!(!first.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{first}");
        assert_eq!(new.name().as_deref(), Some("the clock (continued)"));
    }

    #[test]
    fn the_prompt_names_the_sections_the_cap_and_the_ranked_files() {
        let files = [RankedFile {
            path: "a.rs".to_string(),
            score: 5,
        }];
        let text = prompt("the web chip", &files);
        for section in [
            "Requests",
            "Where things stand",
            "Rules the user set",
            "Files",
            "Errors already solved",
            "Next steps",
        ] {
            assert!(text.contains(section), "{section}");
        }
        assert!(text.contains("1500 words") && text.contains("AGENTS.md"));
        assert!(text.contains("- a.rs (5)") && text.ends_with("the web chip"));
        assert!(!prompt("", &[]).contains("Files by use"));
    }
}
