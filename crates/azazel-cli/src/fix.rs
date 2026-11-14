//! `/fix`: repairs a conversation a provider would refuse. Tool calls that never got a result
//! get one, results that answer no call go, and so do blocks and messages with nothing in them.
//!
//! A session file is only ever added to, so nothing is rewritten: the conversation is written
//! again, repaired, from the first entry that had to change, as a new branch. The branch as it
//! was stays in the file, and `/tree` shows it.

use std::collections::{HashMap, HashSet};

use azazel_agent::agent::UNFINISHED_TEXT;
use azazel_ai::{Block, Message, Role};
use azazel_core::session::Entry;
use azazel_core::{EntryKind, SessionStore};

/// What `/fix` found and did.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct Fixed {
    /// Messages of the conversation that were looked at.
    pub checked: usize,
    /// The tools of the calls that had no result and got one.
    pub answered: Vec<String>,
    /// Text and thinking blocks with nothing in them, taken out.
    pub empty_blocks: usize,
    /// Blocks no provider takes, taken out: a tool call without an id or a name, a picture
    /// without data, a tool result outside a tool message.
    pub invalid_blocks: usize,
    /// Tool results that answered no call of the reply before them, or one already answered.
    pub stray_results: usize,
    /// Messages left with nothing in them, taken out.
    pub empty_messages: usize,
}

impl Fixed {
    pub(crate) fn changed(&self) -> bool {
        !self.answered.is_empty()
            || self.empty_blocks + self.invalid_blocks + self.stray_results + self.empty_messages > 0
    }

    /// What to tell the user.
    pub(crate) fn report(&self) -> String {
        let count = |n: usize, one: &str, many: &str| match n {
            1 => format!("1 {one}"),
            n => format!("{n} {many}"),
        };
        if !self.changed() {
            return format!(
                "nothing to fix: {} checked, every tool call has its result and no block is empty",
                count(self.checked, "message", "messages")
            );
        }
        let mut lines = vec!["fixed the conversation:".to_string()];
        if !self.answered.is_empty() {
            let mut tools = self.answered.clone();
            tools.sort();
            tools.dedup();
            lines.push(format!(
                "  answered {} that had no result ({}): the agent is told they were interrupted",
                count(self.answered.len(), "tool call", "tool calls"),
                tools.join(", ")
            ));
        }
        for (n, one, many) in [
            (
                self.stray_results,
                "tool result that answered no call",
                "tool results that answered no call",
            ),
            (self.empty_blocks, "empty block", "empty blocks"),
            (
                self.invalid_blocks,
                "block no provider accepts",
                "blocks no provider accepts",
            ),
            (self.empty_messages, "empty message", "empty messages"),
        ] {
            if n > 0 {
                lines.push(format!("  dropped {}", count(n, one, many)));
            }
        }
        lines.push("the conversation as it was stays in the session as a branch; /tree shows it".to_string());
        lines.join("\n")
    }
}

/// What becomes of one entry of the branch, or what is put in between.
enum Step {
    /// The entry at this index of the branch stays as it is.
    Keep(usize),
    /// The message at this index is written again as this one.
    Put(usize, Message),
    /// The entry at this index goes.
    Drop(usize),
    /// A message that was not there.
    Add(Message),
}

/// `message` without the blocks that are empty or that no provider takes.
fn cleaned(message: &Message, fixed: &mut Fixed) -> Message {
    let mut clean = message.clone();
    clean.content.retain(|block| {
        let (empty, invalid) = match (message.role, block) {
            (Role::Tool, Block::ToolResult { call_id, .. }) => (false, call_id.is_empty()),
            (Role::Tool, _) | (_, Block::ToolResult { .. }) => (false, true),
            (_, Block::Text { text }) => (text.trim().is_empty(), false),
            (
                _,
                Block::Thinking {
                    text,
                    signature,
                    redacted,
                },
            ) => (text.trim().is_empty() && signature.is_none() && !redacted, false),
            (_, Block::Image { data, .. }) => (false, data.is_empty()),
            (_, Block::ToolCall { id, name, .. }) => (false, id.is_empty() || name.is_empty()),
            (_, Block::Opaque { .. }) => (false, false),
        };
        fixed.empty_blocks += usize::from(empty);
        fixed.invalid_blocks += usize::from(invalid);
        !empty && !invalid
    });
    clean
}

/// The calls of a reply that wait for their results.
struct Open {
    /// Where in the steps the reply is.
    reply: usize,
    /// `(call id, tool name)` of every call, and whether a result has been seen for it.
    calls: Vec<(String, String, bool)>,
    /// Where in the steps the first tool message after the reply is.
    results: Option<usize>,
}

/// Gives the calls of `open` that have no result one: in the tool message that follows the
/// reply when there is one, else in a new one right after the reply.
fn close(open: Option<Open>, steps: &mut Vec<Step>, branch: &[Entry], fixed: &mut Fixed) {
    let Some(open) = open else { return };
    let missing: Vec<Block> = open
        .calls
        .iter()
        .filter(|(_, _, answered)| !answered)
        .map(|(id, name, _)| {
            fixed.answered.push(name.clone());
            Block::tool_result(id.clone(), UNFINISHED_TEXT, true)
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    match open.results {
        Some(at) => {
            let (index, mut message) = match &steps[at] {
                Step::Put(index, message) => (*index, message.clone()),
                Step::Keep(index) => match &branch[*index].kind {
                    EntryKind::Message { message } => (*index, message.clone()),
                    _ => return,
                },
                Step::Drop(_) | Step::Add(_) => return,
            };
            message.content.extend(missing);
            steps[at] = Step::Put(index, message);
        }
        None => steps.insert(open.reply + 1, Step::Add(Message::tool(missing))),
    }
}

/// What to do with each entry of `branch` from `start` on, so that what is left is a
/// conversation a provider takes.
fn plan(branch: &[Entry], start: usize, fixed: &mut Fixed) -> Vec<Step> {
    let mut steps: Vec<Step> = Vec::with_capacity(branch.len() - start);
    let mut open: Option<Open> = None;
    for (index, entry) in branch.iter().enumerate().skip(start) {
        let EntryKind::Message { message } = &entry.kind else {
            steps.push(Step::Keep(index));
            continue;
        };
        fixed.checked += 1;
        let mut clean = cleaned(message, fixed);
        match clean.role {
            Role::Assistant => {
                close(open.take(), &mut steps, branch, fixed);
                let calls: Vec<(String, String, bool)> = clean
                    .tool_calls()
                    .map(|(id, name, _)| (id.to_string(), name.to_string(), false))
                    .collect();
                if !calls.is_empty() {
                    open = Some(Open {
                        reply: steps.len(),
                        calls,
                        results: None,
                    });
                }
            }
            Role::Tool => {
                // A result stays when it answers a call of the reply before it, once.
                clean.content.retain(|block| {
                    let Block::ToolResult { call_id, .. } = block else {
                        return false;
                    };
                    let call = open
                        .as_mut()
                        .and_then(|open| open.calls.iter_mut().find(|(id, _, _)| id == call_id));
                    match call {
                        Some((_, _, answered)) if !*answered => {
                            *answered = true;
                            true
                        }
                        _ => {
                            fixed.stray_results += 1;
                            false
                        }
                    }
                });
                if clean.content.is_empty() {
                    // Its results were counted as they went.
                    steps.push(Step::Drop(index));
                    continue;
                }
                if let Some(open) = open.as_mut().filter(|open| open.results.is_none()) {
                    open.results = Some(steps.len());
                }
            }
            Role::User => {}
        }
        steps.push(if clean.content.is_empty() {
            fixed.empty_messages += 1;
            Step::Drop(index)
        } else if clean == *message {
            Step::Keep(index)
        } else {
            Step::Put(index, clean)
        });
    }
    close(open, &mut steps, branch, fixed);
    steps
}

/// `kind` as it is written again on the repaired branch, pointing at the entries written
/// there; `None` when it is not written again. `moved` maps the entries written again to what
/// they are now, `gone` holds those that were dropped.
fn carried(kind: &EntryKind, moved: &HashMap<String, String>, gone: &HashSet<String>) -> Option<EntryKind> {
    let now = |id: &String| moved.get(id).cloned().unwrap_or_else(|| id.clone());
    Some(match kind {
        // The name, the pin and the sub-agents are the session's, whatever branch it is on, and
        // the labels are given to the new entries once those are written.
        EntryKind::SessionInfo { .. }
        | EntryKind::Pin { .. }
        | EntryKind::Agent { .. }
        | EntryKind::Label { .. } => return None,
        EntryKind::ContextEdit { target_id, .. } if gone.contains(target_id) => return None,
        EntryKind::ContextEdit {
            target_id,
            replacement,
        } => EntryKind::ContextEdit {
            target_id: now(target_id),
            replacement: replacement.clone(),
        },
        EntryKind::Compaction {
            summary,
            first_kept_entry_id,
            tokens_before,
        } => EntryKind::Compaction {
            summary: summary.clone(),
            first_kept_entry_id: now(first_kept_entry_id),
            tokens_before: *tokens_before,
        },
        EntryKind::Checkpoint {
            turn_entry_id,
            commit,
        } => EntryKind::Checkpoint {
            turn_entry_id: now(turn_entry_id),
            commit: commit.clone(),
        },
        other => other.clone(),
    })
}

/// Repairs the conversation on the current branch of `session`. When something had to change
/// the session's leaf is the end of the repaired branch afterwards; the agent has to be
/// started again from it.
pub(crate) fn fix(session: &SessionStore) -> Fixed {
    let branch = session.branch(None);
    // Only what is sent is looked at: what a compaction summarised is not.
    let start = branch
        .iter()
        .rev()
        .find_map(|entry| match &entry.kind {
            EntryKind::Compaction {
                first_kept_entry_id, ..
            } => branch.iter().position(|kept| &kept.id == first_kept_entry_id),
            _ => None,
        })
        .unwrap_or(0);
    let mut fixed = Fixed::default();
    let steps = plan(&branch, start, &mut fixed);
    let Some(first) = steps.iter().position(|step| !matches!(step, Step::Keep(_))) else {
        return fixed;
    };
    // The new branch leaves the old one after the last entry that stays as it is.
    let from = match first.checked_sub(1).map(|before| &steps[before]) {
        Some(Step::Keep(index)) => Some(branch[*index].id.clone()),
        _ => start.checked_sub(1).map(|before| branch[before].id.clone()),
    };
    session.set_leaf(from.as_deref());
    let labels = session.labels();
    let mut moved: HashMap<String, String> = HashMap::new();
    let mut gone: HashSet<String> = HashSet::new();
    // Entries that went since the last one written: what pointed at them points at the next.
    let mut waiting: Vec<String> = Vec::new();
    for step in steps.into_iter().skip(first) {
        let (old, kind) = match step {
            Step::Keep(index) => {
                let entry = &branch[index];
                match carried(&entry.kind, &moved, &gone) {
                    Some(kind) => (Some(entry.id.clone()), kind),
                    None => continue,
                }
            }
            Step::Put(index, message) => (Some(branch[index].id.clone()), EntryKind::Message { message }),
            Step::Add(message) => (None, EntryKind::Message { message }),
            Step::Drop(index) => {
                gone.insert(branch[index].id.clone());
                waiting.push(branch[index].id.clone());
                continue;
            }
        };
        let is_message = matches!(kind, EntryKind::Message { .. });
        let new = session.append(kind);
        if is_message {
            for dropped in waiting.drain(..) {
                moved.insert(dropped, new.clone());
            }
        }
        if let Some(old) = old {
            if let Some(label) = labels.get(&old) {
                session.append(EntryKind::Label {
                    target_id: new.clone(),
                    label: label.clone(),
                });
            }
            moved.insert(old, new);
        }
    }
    fixed
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn store() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        (dir, store)
    }

    fn say(store: &SessionStore, message: Message) -> String {
        store.append(EntryKind::Message { message })
    }

    fn call(id: &str, name: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: name.into(),
            input: json!({}),
            signature: None,
        }
    }

    fn reply(content: Vec<Block>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            meta: None,
        }
    }

    /// The conversation as it would be sent: who speaks, then what each block is.
    fn sent(store: &SessionStore) -> Vec<String> {
        store
            .build_context(None)
            .iter()
            .map(|entry| {
                let blocks: Vec<String> = entry
                    .message
                    .content
                    .iter()
                    .map(|block| match block {
                        Block::Text { text } => format!("text:{text}"),
                        Block::ToolCall { id, .. } => format!("call:{id}"),
                        Block::ToolResult {
                            call_id, is_error, ..
                        } => {
                            format!("result:{call_id}{}", if *is_error { "!" } else { "" })
                        }
                        Block::Thinking { .. } => "thinking".to_string(),
                        Block::Image { .. } => "image".to_string(),
                        Block::Opaque { .. } => "opaque".to_string(),
                    })
                    .collect();
                format!("{:?}[{}]", entry.message.role, blocks.join(","))
            })
            .collect()
    }

    #[test]
    fn a_sound_conversation_is_left_alone() {
        let (_dir, store) = store();
        say(&store, Message::user("read both"));
        say(&store, reply(vec![Block::text("Reading."), call("c1", "read")]));
        say(
            &store,
            Message::tool(vec![Block::tool_result("c1", "one", false)]),
        );
        say(&store, reply(vec![Block::text("Done.")]));
        let (entries, leaf) = (store.entries().len(), store.leaf());

        let fixed = fix(&store);
        assert!(!fixed.changed());
        assert_eq!(fixed.checked, 4);
        assert_eq!((store.entries().len(), store.leaf()), (entries, leaf));
        assert!(fixed.report().starts_with("nothing to fix: 4 messages checked"));
    }

    #[test]
    fn calls_left_without_a_result_at_the_end_get_one_where_they_are() {
        let (_dir, store) = store();
        say(&store, Message::user("read both"));
        let last = say(&store, reply(vec![call("c1", "read"), call("c2", "shell")]));

        let fixed = fix(&store);
        assert_eq!(fixed.answered, ["read", "shell"]);
        assert_eq!(
            sent(&store),
            [
                "User[text:read both]",
                "Assistant[call:c1,call:c2]",
                "Tool[result:c1!,result:c2!]"
            ]
        );
        // Nothing before the end changed, so nothing was written again: the results hang from
        // the reply that was the last entry.
        let results = store.branch(None).pop().unwrap();
        assert_eq!(results.parent_id, Some(last));
        assert_eq!(store.entries().len(), 3);
        // Fixed once, there is nothing left to fix.
        assert!(!fix(&store).changed());
    }

    #[test]
    fn a_broken_middle_is_written_again_as_a_branch_and_the_old_one_stays() {
        let (_dir, store) = store();
        let prompt = say(&store, Message::user("do it"));
        // Two calls, one result; then a result nobody asked for, twice the same result, an
        // empty reply and a reply with an empty text block.
        say(&store, reply(vec![call("c1", "read"), call("c2", "write")]));
        say(
            &store,
            Message::tool(vec![
                Block::tool_result("c1", "one", false),
                Block::tool_result("c1", "one again", false),
                Block::tool_result("zz", "stray", false),
            ]),
        );
        say(&store, reply(Vec::new()));
        let labelled = say(&store, reply(vec![Block::text("  "), Block::text("Done.")]));
        store.append(EntryKind::Label {
            target_id: labelled.clone(),
            label: "the answer".into(),
        });
        let old_leaf = say(&store, Message::user("thanks"));
        let before = store.entries().len();

        let fixed = fix(&store);
        assert_eq!(
            (
                fixed.answered.clone(),
                fixed.stray_results,
                fixed.empty_blocks,
                fixed.empty_messages,
                fixed.invalid_blocks
            ),
            (vec!["write".to_string()], 2, 1, 1, 0)
        );
        assert_eq!(
            sent(&store),
            [
                "User[text:do it]",
                "Assistant[call:c1,call:c2]",
                "Tool[result:c1,result:c2!]",
                "Assistant[text:Done.]",
                "User[text:thanks]",
            ]
        );
        // The new branch leaves the old one after the reply with the calls, which is sound;
        // the old entries are all still there, and the old leaf still leads to them.
        let branch = store.branch(None);
        assert_eq!(branch[0].id, prompt);
        assert!(store.entries().len() > before);
        assert_eq!(store.branch(Some(&old_leaf)).len(), 7);
        assert_ne!(store.leaf(), Some(old_leaf));
        // The label went along to the entry that stands for the labelled one now.
        let now = branch
            .iter()
            .find(|entry| matches!(&entry.kind, EntryKind::Message { message } if message.text() == "Done."))
            .unwrap();
        assert_eq!(
            store.labels().get(&now.id).map(String::as_str),
            Some("the answer")
        );
        assert_eq!(
            store.labels().get(&labelled).map(String::as_str),
            Some("the answer")
        );

        let report = fixed.report();
        assert!(
            report.contains("answered 1 tool call that had no result (write)"),
            "{report}"
        );
        assert!(
            report.contains("dropped 2 tool results that answered no call"),
            "{report}"
        );
        assert!(!fix(&store).changed());
    }

    #[test]
    fn what_a_compaction_summarised_is_not_touched_and_what_points_back_follows() {
        let (_dir, store) = store();
        // Summarised away: broken, and not sent any more.
        say(&store, Message::user("old"));
        say(&store, reply(vec![call("old", "read")]));
        let kept = say(&store, Message::user("  "));
        let reply_id = say(&store, reply(vec![Block::text("Kept.")]));
        store.append(EntryKind::Compaction {
            summary: "what happened".into(),
            first_kept_entry_id: kept.clone(),
            tokens_before: 10,
        });
        store.append(EntryKind::Checkpoint {
            turn_entry_id: reply_id,
            commit: "abc".into(),
        });
        say(&store, Message::user("next"));

        let fixed = fix(&store);
        assert_eq!(
            (fixed.checked, fixed.empty_messages, fixed.answered.len()),
            (3, 1, 0)
        );
        let context = store.build_context(None);
        assert!(context[0].summary);
        let texts: Vec<String> = context[1..].iter().map(|entry| entry.message.text()).collect();
        assert_eq!(texts, ["Kept.", "next"]);
        // The compaction keeps from the entry that took the dropped one's place, and the
        // checkpoint names the reply as it is written now.
        let branch = store.branch(None);
        let new_reply = &context[1].id;
        assert!(branch.iter().any(|entry| matches!(
            &entry.kind,
            EntryKind::Compaction { first_kept_entry_id, .. } if first_kept_entry_id == new_reply
        )));
        assert!(branch.iter().any(|entry| matches!(
            &entry.kind,
            EntryKind::Checkpoint { turn_entry_id, .. } if turn_entry_id == new_reply
        )));
    }

    #[test]
    fn blocks_no_provider_takes_are_dropped() {
        let (_dir, store) = store();
        say(
            &store,
            Message::user_blocks(vec![
                Block::text("look"),
                Block::Image {
                    mime: "image/png".into(),
                    data: String::new(),
                },
                Block::tool_result("c9", "not here", false),
            ]),
        );
        say(
            &store,
            reply(vec![
                Block::Thinking {
                    text: String::new(),
                    signature: None,
                    redacted: false,
                },
                // Thinking the provider signed is replayed as it came, empty or not.
                Block::Thinking {
                    text: String::new(),
                    signature: Some("sig".into()),
                    redacted: false,
                },
                call("", "read"),
                Block::text("Seen."),
            ]),
        );
        let fixed = fix(&store);
        assert_eq!((fixed.invalid_blocks, fixed.empty_blocks), (3, 1));
        assert_eq!(
            sent(&store),
            ["User[text:look]", "Assistant[thinking,text:Seen.]"]
        );
    }
}
