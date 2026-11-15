//! Context bookkeeping: token estimates, eliding old tool results, choosing where to cut for a
//! summary, and rendering the span to summarise.

use azazel_ai::{Block, Message, Role};

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct CompactionSettings {
    pub enabled: bool,
    /// Full compaction starts when the estimate exceeds `context_window - reserve_tokens`.
    pub reserve_tokens: u64,
    /// Roughly how much of the newest context survives a full compaction verbatim.
    pub keep_recent_tokens: u64,
    /// Tool results newer than this many are never elided.
    pub keep_tool_results: usize,
    /// Eliding starts when the estimate exceeds this fraction of the context window.
    pub micro_threshold: f64,
}

impl Default for CompactionSettings {
    fn default() -> Self {
        CompactionSettings {
            enabled: true,
            reserve_tokens: 16_384,
            keep_recent_tokens: 20_000,
            keep_tool_results: 8,
            micro_threshold: 0.6,
        }
    }
}

/// One message of the agent's context with the transcript entry id it was recorded under.
#[derive(Debug, Clone, PartialEq)]
pub struct ContextEntry {
    pub id: String,
    pub message: Message,
    /// True for the synthetic message that stands in for compacted history.
    pub summary: bool,
}

pub(crate) const IMAGE_TOKENS: u64 = 1500;
const ELIDED_PREFIX: &str = "[result elided: ";
const SUMMARY_PREFIX: &str = "Summary of the conversation so far:\n\n";
/// How the note under a summary starts that says where the summarised messages can be read again.
const RECALL_NOTE_PREFIX: &str = "\n\n[The messages summarised above are kept in the session";

/// The tool that reads back what was taken out of the context. An agent that is offered a tool
/// of this name is told, where a result is elided or history is summarised, which entries of the
/// session hold the original.
pub const RECALL_TOOL: &str = "session_recall";

pub fn estimate_text(text: &str) -> u64 {
    (text.len() as u64).div_ceil(4)
}

pub(crate) fn estimate_block(block: &Block) -> u64 {
    match block {
        Block::Text { text } | Block::Thinking { text, .. } => estimate_text(text),
        Block::Image { .. } => IMAGE_TOKENS,
        Block::ToolCall { name, input, .. } => {
            estimate_text(name) + (input.to_string().len() as u64).div_ceil(4)
        }
        Block::ToolResult { content, .. } => content.iter().map(estimate_block).sum(),
        Block::Opaque { item, .. } => (item.to_string().len() as u64).div_ceil(4),
    }
}

/// Bytes / 4 for text, a flat 1500 per image.
pub fn estimate_message(message: &Message) -> u64 {
    message.content.iter().map(estimate_block).sum()
}

/// What stands in for a tool result of `bytes` bytes. `recall` is the entry that holds the
/// original, named when the agent can read it back.
pub fn elided_text(bytes: usize, recall: Option<&str>) -> String {
    match recall {
        Some(id) => format!("{ELIDED_PREFIX}{bytes} bytes; {RECALL_TOOL} expand={id}]"),
        None => format!("{ELIDED_PREFIX}{bytes} bytes; re-run the tool if needed]"),
    }
}

fn result_bytes(content: &[Block]) -> usize {
    content
        .iter()
        .map(|b| match b {
            Block::Text { text } => text.len(),
            Block::Image { data, .. } => data.len(),
            _ => 0,
        })
        .sum()
}

fn is_elided(content: &[Block]) -> bool {
    matches!(content, [Block::Text { text }] if text.starts_with(ELIDED_PREFIX))
}

/// Indices of `Tool` entries whose results are all older than the newest `keep` results and still
/// carry their original content.
pub fn micro_targets(entries: &[ContextEntry], keep: usize) -> Vec<usize> {
    let mut newer = 0usize;
    let mut targets = Vec::new();
    for (i, entry) in entries.iter().enumerate().rev() {
        if entry.message.role != Role::Tool {
            continue;
        }
        let results: Vec<&Vec<Block>> = entry
            .message
            .content
            .iter()
            .filter_map(|b| match b {
                Block::ToolResult { content, .. } => Some(content),
                _ => None,
            })
            .collect();
        if newer >= keep && results.iter().any(|content| !is_elided(content)) {
            targets.push(i);
        }
        newer += results.len();
    }
    targets.reverse();
    targets
}

/// Replaces the body of every tool result in `entry` with the elision note, which names the
/// entry when `recall` says the agent can read it back. Returns the note, or `None` when there
/// was nothing left to elide.
pub fn elide(entry: &mut ContextEntry, recall: bool) -> Option<String> {
    let bytes: usize = entry
        .message
        .content
        .iter()
        .map(|b| match b {
            Block::ToolResult { content, .. } if !is_elided(content) => result_bytes(content),
            _ => 0,
        })
        .sum();
    if bytes == 0 {
        return None;
    }
    let note = elided_text(bytes, recall.then_some(entry.id.as_str()));
    apply_context_edit(&mut entry.message, &note);
    Some(note)
}

/// How a recorded `context_edit` changes its target: tool results keep their call ids and error
/// flags and get `replacement` as their body; any other message becomes a single text block.
pub fn apply_context_edit(message: &mut Message, replacement: &str) {
    if message.role == Role::Tool {
        for block in &mut message.content {
            if let Block::ToolResult { content, .. } = block {
                *content = vec![Block::text(replacement)];
            }
        }
    } else {
        message.content = vec![Block::text(replacement)];
    }
}

/// Index of the first entry to keep verbatim; everything before it gets summarised.
///
/// Walks back from the newest entry until `keep_recent_tokens` are covered, then moves the cut
/// earlier so it never lands on a `Tool` message (a tool call and its results stay together).
/// `None` when that leaves nothing to summarise, unless `forced`, in which case the cut is the
/// latest boundary that still removes something.
pub fn cut_point(entries: &[ContextEntry], keep_recent_tokens: u64, forced: bool) -> Option<usize> {
    let mut cut = entries.len();
    let mut kept = 0u64;
    while cut > 0 {
        cut -= 1;
        kept += estimate_message(&entries[cut].message);
        if kept >= keep_recent_tokens {
            break;
        }
    }
    while cut > 0 && entries[cut].message.role == Role::Tool {
        cut -= 1;
    }
    // A lone summary before the cut is not worth re-summarising.
    let worthwhile = cut > 1 || (cut == 1 && !entries[0].summary);
    if worthwhile {
        return Some(cut);
    }
    if !forced {
        return None;
    }
    (1..entries.len())
        .rev()
        .find(|&i| entries[i].message.role != Role::Tool && (i > 1 || !entries[0].summary))
}

pub fn summary_message(summary: &str) -> Message {
    Message::user(format!("{SUMMARY_PREFIX}{summary}"))
}

/// The summary text of a summary entry, without the note about reading the originals again.
pub fn summary_text(message: &Message) -> String {
    let text = message.text();
    let text = text.strip_prefix(SUMMARY_PREFIX).unwrap_or(&text);
    let end = text.rfind(RECALL_NOTE_PREFIX).unwrap_or(text.len());
    text[..end].to_string()
}

/// What a summary ends with when the agent can read the summarised messages again: `last` is
/// the newest entry the summary covers.
pub fn recall_note(last: &str) -> String {
    format!(
        "{RECALL_NOTE_PREFIX}, up to entry {last}. {RECALL_TOOL} finds them by `query` and returns \
         them in full by `range` or `expand`; an id in square brackets above is the entry that \
         holds the detail.]"
    )
}

/// Added to [`SUMMARY_SYSTEM`] when the span is rendered with entry ids.
pub const SUMMARY_IDS: &str = "\n\nEvery message of the conversation is labelled with its entry id in square \
brackets. Where the summary leaves out detail that may be needed again (what a file contained, a long output, \
the exact text of an error), put the id of the entry that holds it after the fact, like `[1a2b3c4d]`. Copy ids \
exactly and never invent one.";

pub const SUMMARY_SYSTEM: &str = "You are summarising a coding-agent conversation so the work can continue \
in a fresh context window. Reply with the summary only, using exactly these headings in this order:\n\
## Goal\n## Constraints\n## Progress\n## Decisions\n## Files read\n## Files modified\n## Next steps\n\
Be specific: keep file paths, commands, error messages, identifiers and decisions verbatim, and leave out \
pleasantries. Under \"Files read\" and \"Files modified\" list paths only. When a previous summary is given, \
merge it into the new one and keep every fact that still matters.";

const MAX_CALL_CHARS: usize = 600;
const MAX_RESULT_CHARS: usize = 2000;

fn clip(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((end, _)) => format!("{}… [{} more bytes]", &text[..end], text.len() - end),
        None => text.to_string(),
    }
}

/// Plain-text rendering of `entries` for the summariser, at most `max_chars` long. When it would
/// be longer the oldest part is dropped, since the newest history matters most.
pub fn render_span(entries: &[ContextEntry], max_chars: usize) -> String {
    render(entries, max_chars, false)
}

/// [`render_span`] with every message labelled with its entry id, so a summary can say where a
/// detail it leaves out is kept.
pub fn render_span_ids(entries: &[ContextEntry], max_chars: usize) -> String {
    render(entries, max_chars, true)
}

fn render(entries: &[ContextEntry], max_chars: usize, ids: bool) -> String {
    let mut out = String::new();
    for entry in entries {
        let speaker = match entry.message.role {
            Role::User => "User",
            Role::Assistant => "Assistant",
            Role::Tool => "Tool",
        };
        let id = if ids {
            format!(" [{}]", entry.id)
        } else {
            String::new()
        };
        for block in &entry.message.content {
            match block {
                Block::Text { text } => out.push_str(&format!("{speaker}{id}: {text}\n\n")),
                Block::Image { .. } => out.push_str(&format!("{speaker}{id}: [image]\n\n")),
                Block::ToolCall { name, input, .. } => out.push_str(&format!(
                    "Assistant called {name}{id}: {}\n\n",
                    clip(&input.to_string(), MAX_CALL_CHARS)
                )),
                Block::ToolResult {
                    content, is_error, ..
                } => {
                    let mut text = String::new();
                    for inner in content {
                        match inner {
                            Block::Text { text: t } => text.push_str(t),
                            Block::Image { .. } => text.push_str("[image]"),
                            _ => {}
                        }
                    }
                    let label = if *is_error { "Tool error" } else { "Tool result" };
                    out.push_str(&format!("{label}{id}: {}\n\n", clip(&text, MAX_RESULT_CHARS)));
                }
                Block::Thinking { .. } | Block::Opaque { .. } => {}
            }
        }
    }
    let total = out.chars().count();
    if total > max_chars {
        let skip = total - max_chars;
        let start = out.char_indices().nth(skip).map_or(out.len(), |(i, _)| i);
        return format!("[earlier history omitted]\n\n{}", &out[start..]);
    }
    out
}

#[cfg(test)]
mod tests {
    use azazel_ai::{AssistantMeta, StopReason, Usage, WireApi};
    use serde_json::json;

    use super::*;

    fn entry(i: usize, message: Message) -> ContextEntry {
        ContextEntry {
            id: format!("e{i}"),
            message,
            summary: false,
        }
    }

    fn assistant(blocks: Vec<Block>) -> Message {
        Message::assistant(
            blocks,
            AssistantMeta {
                provider: "mock".into(),
                model: "a".into(),
                api: WireApi::OpenAiChat,
                usage: Usage::default(),
                stop: StopReason::Stop,
                response_id: None,
                duration_ms: 0,
                sources: Vec::new(),
            },
        )
    }

    fn call(id: &str) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: "read".into(),
            input: json!({}),
            signature: None,
        }
    }

    /// user, then `rounds` × (assistant call, tool result of `bytes` bytes).
    fn transcript(rounds: usize, bytes: usize) -> Vec<ContextEntry> {
        let mut messages = vec![Message::user("start")];
        for r in 0..rounds {
            let id = format!("c{r}");
            messages.push(assistant(vec![call(&id)]));
            messages.push(Message::tool(vec![Block::tool_result(
                id,
                "x".repeat(bytes),
                false,
            )]));
        }
        messages
            .into_iter()
            .enumerate()
            .map(|(i, m)| entry(i, m))
            .collect()
    }

    #[test]
    fn micro_targets_spare_the_newest_results_and_already_elided_ones() {
        let mut entries = transcript(5, 400);
        // Tool entries sit at indices 2, 4, 6, 8, 10; keeping 2 spares 8 and 10.
        assert_eq!(micro_targets(&entries, 2), vec![2, 4, 6]);
        let note = elide(&mut entries[2], false).unwrap();
        assert_eq!(note, "[result elided: 400 bytes; re-run the tool if needed]");
        assert_eq!(micro_targets(&entries, 2), vec![4, 6]);
        assert_eq!(elide(&mut entries[2], false), None);
        assert!(micro_targets(&entries, 5).is_empty());
    }

    #[test]
    fn an_elided_result_names_its_entry_when_it_can_be_read_back() {
        let mut entries = transcript(2, 400);
        let note = elide(&mut entries[2], true).unwrap();
        assert_eq!(note, "[result elided: 400 bytes; session_recall expand=e2]");
        assert_eq!(
            entries[2].message.content,
            vec![Block::tool_result("c0", note, false)]
        );
        // Either form of the note marks the result as elided already.
        assert_eq!(elide(&mut entries[2], true), None);
        assert_eq!(micro_targets(&entries, 0), vec![4]);
    }

    #[test]
    fn the_recall_note_of_a_summary_is_not_part_of_its_text() {
        let stored = format!("## Goal\nship it{}", recall_note("e41"));
        assert!(stored.contains("up to entry e41"));
        assert!(stored.contains("session_recall"));
        let message = summary_message(&stored);
        assert_eq!(summary_text(&message), "## Goal\nship it");
        assert_eq!(summary_text(&summary_message("no note")), "no note");
    }

    #[test]
    fn elide_keeps_call_ids_and_error_flags() {
        let mut entry = entry(
            0,
            Message::tool(vec![
                Block::tool_result("c1", "aaaa", false),
                Block::tool_result("c2", "bbbbbb", true),
            ]),
        );
        let note = elide(&mut entry, false).unwrap();
        assert_eq!(note, elided_text(10, None));
        assert_eq!(
            entry.message.content,
            vec![
                Block::tool_result("c1", note.clone(), false),
                Block::tool_result("c2", note, true)
            ]
        );
    }

    #[test]
    fn cut_never_lands_on_a_tool_message() {
        // Each round is ~1000 tokens of tool result. Keeping ~1500 tokens covers the last two tool
        // messages; the walk stops on a Tool entry (index 8) and must back up to its assistant (7).
        let entries = transcript(5, 4000);
        let cut = cut_point(&entries, 1500, false).unwrap();
        assert_eq!(cut, 7);
        assert_eq!(entries[cut].message.role, Role::Assistant);
    }

    #[test]
    fn nothing_to_cut_when_everything_fits_unless_forced() {
        let entries = transcript(3, 40);
        assert_eq!(cut_point(&entries, 20_000, false), None);
        // Forced: keep only the last assistant call and its result.
        assert_eq!(cut_point(&entries, 20_000, true), Some(5));
        assert_eq!(cut_point(&entries[..1], 20_000, true), None);
    }

    #[test]
    fn render_span_labels_speakers_clips_results_and_keeps_the_tail() {
        let entries = vec![
            entry(0, Message::user("fix the bug")),
            entry(1, assistant(vec![Block::text("looking"), call("c1")])),
            entry(
                2,
                Message::tool(vec![Block::tool_result("c1", "y".repeat(5000), true)]),
            ),
        ];
        let text = render_span(&entries, 1_000_000);
        assert!(text.starts_with(
            "User: fix the bug\n\nAssistant: looking\n\nAssistant called read: {}\n\nTool error: yyy"
        ));
        assert!(text.contains("[3000 more bytes]"));

        let tail = render_span(&entries, 50);
        assert!(tail.starts_with("[earlier history omitted]"));
        assert!(tail.ends_with("[3000 more bytes]\n\n"));
        assert!(!tail.contains("fix the bug"));

        let labelled = render_span_ids(&entries, 1_000_000);
        assert!(labelled.starts_with(
            "User [e0]: fix the bug\n\nAssistant [e1]: looking\n\nAssistant called read [e1]: {}\n\n\
             Tool error [e2]: yyy"
        ));
    }
}
