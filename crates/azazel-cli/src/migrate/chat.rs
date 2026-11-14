//! Conversations of other harnesses written as azazel sessions.

use std::collections::HashSet;
use std::io::{self, Read};
use std::path::{Path, PathBuf};

use azazel_ai::{AssistantMeta, Block, Message, StopReason, WireApi};
use azazel_core::{EntryKind, SessionStore};
use serde_json::{Value, json};

use super::convert::api_of_provider;
use super::model::{Chat, Source, Turn, TurnKind};

/// The `customType` of the entry an imported session starts with. Its data names the source
/// and the source's id of the conversation, which is how a second import knows it is there.
pub const IMPORTED: &str = "imported";

/// The result a tool call gets when the source recorded none.
const NO_RESULT: &str = "no result was recorded for this call";

/// What an imported session is called.
pub fn name_of(source: Source, title: Option<&str>) -> String {
    let title = title.map(str::trim).filter(|title| !title.is_empty());
    format!(
        "imported from {}: {}",
        source.label(),
        title.unwrap_or("untitled")
    )
}

/// `(source, id)` of the conversations the sessions in `dir` were imported from.
pub fn imported_in(dir: &Path) -> HashSet<(String, String)> {
    let mut out = HashSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return out;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.extension().is_none_or(|ext| ext != "jsonl") {
            continue;
        }
        // The mark is the entry after the header, and a short one: the head of the file holds it.
        let mut head = Vec::new();
        let read = std::fs::File::open(&path).and_then(|file| file.take(16 * 1024).read_to_end(&mut head));
        if read.is_err() {
            continue;
        }
        let head = String::from_utf8_lossy(&head);
        let mark = head
            .lines()
            .nth(1)
            .and_then(|line| serde_json::from_str::<Value>(line).ok())
            .filter(|entry| entry["type"] == "custom" && entry["customType"] == IMPORTED);
        if let Some(mark) = mark
            && let (Some(source), Some(id)) = (mark["data"]["source"].as_str(), mark["data"]["id"].as_str())
        {
            out.insert((source.to_string(), id.to_string()));
        }
    }
    out
}

/// What writing a conversation came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
    pub path: PathBuf,
    pub messages: usize,
    pub agents: usize,
    /// When the conversation was last used.
    pub last: jiff::Timestamp,
    /// What could not be kept: calls without results, results without calls.
    pub notes: Vec<String>,
}

#[derive(Default)]
struct Tally {
    messages: usize,
    orphans: usize,
    unanswered: usize,
    empty: usize,
}

/// The entries of `turns`, appended to `store` in order with the times the source recorded.
fn replay(
    store: &SessionStore,
    turns: &[Turn],
    source: Source,
    start: jiff::Timestamp,
) -> (Tally, jiff::Timestamp) {
    let mut tally = Tally::default();
    let mut now = start;
    // The id of the entry each turn became, for a compaction that names the first turn kept.
    let mut ids: Vec<Option<String>> = Vec::with_capacity(turns.len());
    // The calls of the last reply that have no result yet, and the results gathered for it.
    let mut waiting: Vec<String> = Vec::new();
    let mut results: Vec<(usize, Block)> = Vec::new();

    fn flush(
        store: &SessionStore,
        waiting: &mut Vec<String>,
        results: &mut Vec<(usize, Block)>,
        ids: &mut [Option<String>],
        tally: &mut Tally,
        now: jiff::Timestamp,
    ) {
        for call_id in waiting.drain(..) {
            tally.unanswered += 1;
            results.push((usize::MAX, Block::tool_result(call_id, NO_RESULT, true)));
        }
        if results.is_empty() {
            return;
        }
        let (turns, blocks): (Vec<usize>, Vec<Block>) = results.drain(..).unzip();
        let id = store.append_at(
            EntryKind::Message {
                message: Message::tool(blocks),
            },
            now,
        );
        tally.messages += 1;
        for turn in turns {
            if let Some(slot) = ids.get_mut(turn) {
                *slot = Some(id.clone());
            }
        }
    }

    for (index, turn) in turns.iter().enumerate() {
        ids.push(None);
        if let Some(at) = turn.at
            && at > now
        {
            now = at;
        }
        match &turn.kind {
            TurnKind::ToolResult {
                call_id,
                content,
                is_error,
            } => match waiting.iter().position(|waiting| waiting == call_id) {
                Some(at) => {
                    waiting.remove(at);
                    let content = match content.is_empty() {
                        true => vec![Block::text("")],
                        false => content.clone(),
                    };
                    results.push((
                        index,
                        Block::ToolResult {
                            call_id: call_id.clone(),
                            content,
                            is_error: *is_error,
                        },
                    ));
                }
                None => tally.orphans += 1,
            },
            TurnKind::User(blocks) => {
                flush(store, &mut waiting, &mut results, &mut ids, &mut tally, now);
                if blocks.is_empty() {
                    tally.empty += 1;
                    continue;
                }
                let message = Message::user_blocks(blocks.clone());
                ids[index] = Some(store.append_at(EntryKind::Message { message }, now));
                tally.messages += 1;
            }
            TurnKind::Assistant {
                blocks,
                model,
                api,
                usage,
                failed,
            } => {
                flush(store, &mut waiting, &mut results, &mut ids, &mut tally, now);
                let blocks: Vec<Block> = blocks
                    .iter()
                    .filter(|block| match block {
                        Block::Text { text } => !text.trim().is_empty(),
                        Block::Thinking { text, signature, .. } => {
                            !text.trim().is_empty() || signature.is_some()
                        }
                        _ => true,
                    })
                    .cloned()
                    .collect();
                if blocks.is_empty() {
                    tally.empty += 1;
                    continue;
                }
                waiting = blocks
                    .iter()
                    .filter_map(|block| match block {
                        Block::ToolCall { id, .. } => Some(id.clone()),
                        _ => None,
                    })
                    .collect();
                let (provider, model) = model
                    .clone()
                    .unwrap_or_else(|| (source.name().to_string(), "unknown".to_string()));
                let api = api
                    .or_else(|| api_of_provider(&provider))
                    .unwrap_or(WireApi::OpenAiChat);
                let stop = match (failed, waiting.is_empty()) {
                    (true, _) => StopReason::Error,
                    (false, false) => StopReason::ToolUse,
                    (false, true) => StopReason::Stop,
                };
                let meta = AssistantMeta {
                    provider,
                    model,
                    api,
                    usage: *usage,
                    stop,
                    response_id: None,
                    duration_ms: 0,
                    sources: Vec::new(),
                };
                let message = Message::assistant(blocks, meta);
                ids[index] = Some(store.append_at(EntryKind::Message { message }, now));
                tally.messages += 1;
            }
            TurnKind::Compaction {
                summary,
                tokens_before,
                kept,
            } => {
                flush(store, &mut waiting, &mut results, &mut ids, &mut tally, now);
                // The first entry among the turns that were kept; with none, an id that names
                // nothing, which is how a session says that the summary stands for everything.
                let first = index.saturating_sub(*kept);
                let first_kept = ids[first..index].iter().flatten().next().cloned();
                ids[index] = Some(store.append_at(
                    EntryKind::Compaction {
                        summary: summary.clone(),
                        first_kept_entry_id: first_kept.unwrap_or_default(),
                        tokens_before: *tokens_before,
                    },
                    now,
                ));
            }
            TurnKind::Note { name, data } => {
                ids[index] = Some(store.append_at(
                    EntryKind::Custom {
                        custom_type: format!("{}:{name}", source.name()),
                        data: data.clone(),
                    },
                    now,
                ));
            }
        }
    }
    flush(store, &mut waiting, &mut results, &mut ids, &mut tally, now);
    (tally, now)
}

fn notes_of(tally: &Tally, notes: &mut Vec<String>) {
    let mut note = |count: usize, what: &str| {
        if count > 0 {
            notes.push(format!("{count} {what}"));
        }
    };
    note(
        tally.unanswered,
        "tool calls had no result and were given one that says so",
    );
    note(
        tally.orphans,
        "tool results belonged to no call and were left out",
    );
    note(tally.empty, "empty messages were left out");
}

/// A name a file can have, from what a source calls a sub-agent.
fn agent_id(name: &str, taken: &[String]) -> String {
    let mut id: String = name
        .chars()
        .map(
            |c| match c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.') {
                true => c,
                false => '-',
            },
        )
        .take(48)
        .collect();
    if id.trim_matches(['-', '.']).is_empty() {
        id = "agent".to_string();
    }
    let base = id.clone();
    let mut n = 2;
    while taken.contains(&id) {
        id = format!("{base}-{n}");
        n += 1;
    }
    id
}

/// Writes `chat` as a new session in `dir`, named `imported from <source>: <title>`. The
/// conversations of its sub-agents become that session's agent transcripts.
pub fn write_chat(dir: &Path, source: Source, chat: &Chat, file: &Path) -> io::Result<Written> {
    let created = chat
        .created
        .or_else(|| chat.turns.iter().find_map(|turn| turn.at))
        .unwrap_or_else(jiff::Timestamp::now);
    let cwd = chat.cwd.clone().unwrap_or_default();
    let store = SessionStore::create_dated(dir, &cwd, created)?;
    store.append_at(
        EntryKind::Custom {
            custom_type: IMPORTED.to_string(),
            data: json!({
                "source": source.name(),
                "id": chat.id,
                "title": chat.title,
                "file": file.display().to_string(),
            }),
        },
        created,
    );
    store.append_at(
        EntryKind::SessionInfo {
            name: name_of(source, chat.title.as_deref()),
        },
        created,
    );
    let (tally, mut last) = replay(&store, &chat.turns, source, created);
    let mut notes = chat.dropped.clone();
    notes_of(&tally, &mut notes);

    let mut taken: Vec<String> = Vec::new();
    for (name, agent) in &chat.agents {
        let id = agent_id(name, &taken);
        let transcript = store.agent_transcript(&id);
        let began = agent.created.unwrap_or(created);
        let written = SessionStore::create_at(&transcript, &id, &cwd, Some(&store.path())).map(|sub| {
            let (tally, ended) = replay(&sub, &agent.turns, source, began);
            (tally, ended)
        });
        match written {
            Ok((_, ended)) => {
                last = last.max(ended);
                store.append_at(
                    EntryKind::Agent {
                        agent_id: id.clone(),
                        def: name.clone(),
                        status: "done".to_string(),
                        transcript: transcript.display().to_string(),
                    },
                    last,
                );
                taken.push(id);
            }
            Err(e) => notes.push(format!(
                "the transcript of sub-agent {name} could not be written: {e}"
            )),
        }
    }
    Ok(Written {
        path: store.path(),
        messages: tally.messages,
        agents: taken.len(),
        last,
        notes,
    })
}

#[cfg(test)]
mod tests {
    use azazel_ai::{Role, Usage};

    use super::*;

    fn at(second: i64) -> Option<jiff::Timestamp> {
        Some(jiff::Timestamp::from_second(1_760_000_000 + second).unwrap())
    }

    fn assistant(blocks: Vec<Block>) -> TurnKind {
        TurnKind::Assistant {
            blocks,
            model: Some(("anthropic".to_string(), "claude-x".to_string())),
            api: None,
            usage: Usage {
                input: 10,
                output: 4,
                ..Usage::default()
            },
            failed: false,
        }
    }

    fn call(id: &str) -> Block {
        Block::ToolCall {
            id: id.to_string(),
            name: "read".to_string(),
            input: json!({ "path": "notes.txt" }),
            signature: None,
        }
    }

    fn result(id: &str, text: &str) -> TurnKind {
        TurnKind::ToolResult {
            call_id: id.to_string(),
            content: vec![Block::text(text)],
            is_error: false,
        }
    }

    fn chat(turns: Vec<TurnKind>) -> Chat {
        Chat {
            id: "abc-1".to_string(),
            title: Some("Read the notes".to_string()),
            cwd: Some(PathBuf::from("/work/app")),
            created: at(0),
            turns: turns
                .into_iter()
                .enumerate()
                .map(|(n, kind)| Turn {
                    at: at(n as i64 + 1),
                    kind,
                })
                .collect(),
            ..Chat::default()
        }
    }

    #[test]
    fn a_conversation_becomes_a_session_with_its_name_times_and_paired_results() {
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(vec![
            TurnKind::User(vec![Block::text("read both files")]),
            assistant(vec![Block::text("Reading."), call("c1"), call("c2"), call("c3")]),
            result("c2", "second"),
            result("c1", "first"),
            result("stray", "nobody asked"),
            assistant(vec![Block::text("  ")]),
            assistant(vec![Block::text("Done.")]),
        ]);
        let written = write_chat(dir.path(), Source::Omp, &chat, Path::new("/src/a.jsonl")).unwrap();

        let store = SessionStore::open(&written.path).unwrap();
        assert_eq!(store.name().as_deref(), Some("imported from omp: Read the notes"));
        assert_eq!(store.header().cwd, "/work/app");
        assert_eq!(store.header().timestamp, at(0).unwrap().to_string());
        let messages: Vec<Message> = store
            .build_context(None)
            .into_iter()
            .map(|entry| entry.message)
            .collect();
        let roles: Vec<Role> = messages.iter().map(|message| message.role).collect();
        assert_eq!(roles, [Role::User, Role::Assistant, Role::Tool, Role::Assistant]);
        // One tool message answers the reply: both results, and the call the source lost.
        let answered: Vec<(&str, bool)> = messages[2]
            .content
            .iter()
            .map(|block| match block {
                Block::ToolResult {
                    call_id, is_error, ..
                } => (call_id.as_str(), *is_error),
                other => panic!("not a result: {other:?}"),
            })
            .collect();
        assert_eq!(answered, [("c2", false), ("c1", false), ("c3", true)]);
        let meta = messages[1].meta.as_ref().unwrap();
        assert_eq!(
            (meta.provider.as_str(), meta.model.as_str(), meta.api, meta.stop),
            (
                "anthropic",
                "claude-x",
                WireApi::AnthropicMessages,
                StopReason::ToolUse
            )
        );
        assert_eq!(meta.usage.output, 4);
        assert_eq!(messages[3].meta.as_ref().unwrap().stop, StopReason::Stop);
        // The entries carry the times of the source, so the session sorts by when it was used.
        let last = store.entries().last().unwrap().timestamp.clone();
        assert_eq!(last, at(7).unwrap().to_string());
        assert_eq!(written.messages, 4);
        assert!(
            written
                .notes
                .iter()
                .any(|note| note.starts_with("1 tool calls had no result"))
        );
        assert!(
            written
                .notes
                .iter()
                .any(|note| note.starts_with("1 tool results belonged"))
        );
        assert!(
            written
                .notes
                .iter()
                .any(|note| note.starts_with("1 empty messages"))
        );

        assert_eq!(
            imported_in(dir.path()),
            HashSet::from([("omp".to_string(), "abc-1".to_string())])
        );
    }

    #[test]
    fn a_compaction_keeps_the_turns_the_source_kept() {
        let dir = tempfile::tempdir().unwrap();
        let chat = chat(vec![
            TurnKind::User(vec![Block::text("one")]),
            assistant(vec![Block::text("first answer")]),
            TurnKind::User(vec![Block::text("two")]),
            assistant(vec![Block::text("second answer")]),
            TurnKind::Compaction {
                summary: "They counted to two.".to_string(),
                tokens_before: 900,
                kept: 2,
            },
            TurnKind::User(vec![Block::text("three")]),
            TurnKind::Compaction {
                summary: "They counted to three.".to_string(),
                tokens_before: 1200,
                kept: 0,
            },
            TurnKind::User(vec![Block::text("four")]),
        ]);
        let written = write_chat(dir.path(), Source::Pi, &chat, Path::new("a.jsonl")).unwrap();
        let store = SessionStore::open(&written.path).unwrap();

        // The last compaction kept nothing: the summary, then what came after it.
        let context = store.build_context(None);
        let texts: Vec<String> = context.iter().map(|entry| entry.message.text()).collect();
        assert_eq!(texts.len(), 2);
        assert!(texts[0].contains("They counted to three."), "{texts:?}");
        assert_eq!(texts[1], "four");

        // The first one kept the two turns before it.
        let entries = store.entries();
        let first = entries
            .iter()
            .find(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
            .unwrap();
        let context = store.build_context(Some(&first.id));
        let texts: Vec<String> = context.iter().map(|entry| entry.message.text()).collect();
        assert_eq!(texts[1..], ["two", "second answer"]);
    }

    #[test]
    fn sub_agents_get_transcripts_of_their_own_beside_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let mut main = chat(vec![TurnKind::User(vec![Block::text("delegate")])]);
        let sub = chat(vec![
            TurnKind::User(vec![Block::text("look around")]),
            assistant(vec![Block::text("Looked.")]),
        ]);
        main.agents = vec![
            ("Scout One".to_string(), sub.clone()),
            ("Scout One".to_string(), sub),
        ];
        let written = write_chat(dir.path(), Source::Omp, &main, Path::new("a.jsonl")).unwrap();
        assert_eq!(written.agents, 2);

        let store = SessionStore::open(&written.path).unwrap();
        let agents = store.facts().agents;
        let ids: Vec<&str> = agents.iter().map(|agent| agent.id.as_str()).collect();
        assert_eq!(ids, ["Scout-One", "Scout-One-2"]);
        assert_eq!(agents[0].transcript, store.agent_transcript("Scout-One"));
        let branch = SessionStore::branch_of(&agents[1].transcript).unwrap();
        assert_eq!(branch.len(), 2);
        // Only the session itself is a session of the project.
        assert_eq!(SessionStore::list(dir.path()).len(), 1);
    }
}
