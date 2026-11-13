//! `session_recall`: what the session file holds of the agent's own conversation, including what
//! compaction summarised away and what eliding took out of old tool results. It reads the branch
//! the conversation is on and changes nothing.

use std::collections::HashMap;

use async_trait::async_trait;
use azazel_agent::compaction::RECALL_TOOL;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::{Block, Role};
use azazel_core::SessionStore;
use azazel_core::session::{Entry, EntryKind};
use azazel_core::transcript::activity;
use regex::RegexBuilder;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::{display_path, parse_input, resolve_path};

/// Entries a search returns when `limit` is not given, and the most it may ask for.
const SEARCH_LIMIT: usize = 10;
const SEARCH_MAX: usize = 50;
/// Entries a range returns when `limit` is not given, and the most it may ask for.
const RANGE_LIMIT: usize = 20;
const RANGE_MAX: usize = 100;
/// Characters of a matching line shown with a search hit.
const SNIPPET_CHARS: usize = 200;
/// Entry ids listed per file by `touched`; the newest are kept.
const TOUCHED_IDS: usize = 8;
/// A regex larger than this once compiled is refused.
const REGEX_SIZE: usize = 1 << 20;

pub struct SessionRecallTool;

#[derive(Deserialize)]
struct Input {
    #[serde(default)]
    query: Option<String>,
    #[serde(default)]
    regex: Option<String>,
    #[serde(default)]
    range: Option<Vec<String>>,
    #[serde(default)]
    expand: Vec<String>,
    #[serde(default)]
    touched: bool,
    #[serde(default)]
    limit: Option<usize>,
}

/// One entry of the branch that holds something that was said.
struct Item {
    id: String,
    timestamp: String,
    /// Who said it, or what it is the result of.
    label: String,
    /// Everything the entry holds, as text.
    text: String,
    /// The text searches look in, when that is not all of it: what earlier calls of this tool
    /// returned is left out, so a search does not find its own echoes.
    searched: Option<String>,
}

impl Item {
    fn searched(&self) -> &str {
        self.searched.as_deref().unwrap_or(&self.text)
    }

    fn full(&self) -> String {
        format!(
            "── {} · {} · {} ──\n{}",
            self.id,
            self.label,
            self.timestamp,
            self.text.trim_end()
        )
    }
}

/// A file a call read or wrote, and the entry to expand to see it.
struct Touch {
    path: String,
    written: bool,
    entry: String,
}

/// A tool call of the branch, kept until its result is met.
struct Call<'a> {
    tool: &'a str,
    /// What the call was about, in one line.
    about: String,
    /// The entry that made the call.
    entry: &'a str,
    /// The file the call names, as it is shown.
    file: Option<String>,
}

/// The branch as items, and the files its `read`, `write` and `edit` calls touched.
fn index(branch: &[Entry], cwd: &std::path::Path) -> (Vec<Item>, Vec<Touch>) {
    let mut items = Vec::new();
    let mut touches = Vec::new();
    let mut calls: HashMap<&str, Call> = HashMap::new();
    for entry in branch {
        let (label, text, searched) = match &entry.kind {
            EntryKind::Compaction { summary, .. } => ("summary".to_string(), summary.clone(), None),
            EntryKind::Message { message } => {
                let mut text = String::new();
                let mut searched = String::new();
                let mut echoes = false;
                let mut labels: Vec<String> = Vec::new();
                let mut add = |piece: &str, echo: bool, searched: &mut String| {
                    text.push_str(piece);
                    text.push('\n');
                    if !echo {
                        searched.push_str(piece);
                        searched.push('\n');
                    }
                };
                for block in &message.content {
                    match block {
                        Block::Text { text } => add(text, false, &mut searched),
                        Block::Image { .. } => add("[image]", false, &mut searched),
                        Block::ToolCall { id, name, input, .. } => {
                            let about = activity(name, input);
                            let file = input
                                .get("path")
                                .and_then(Value::as_str)
                                .filter(|path| !path.contains("://"))
                                .map(|path| display_path(cwd, &resolve_path(cwd, path)));
                            calls.insert(
                                id,
                                Call {
                                    tool: name,
                                    about,
                                    entry: &entry.id,
                                    file,
                                },
                            );
                            let echo = name == RECALL_TOOL;
                            echoes |= echo;
                            add(&format!("→ {name} {input}"), echo, &mut searched);
                        }
                        Block::ToolResult {
                            call_id,
                            content,
                            is_error,
                        } => {
                            let call = calls.get(call_id.as_str());
                            let about = call.map_or("a tool call", |call| call.about.as_str());
                            let echo = call.is_some_and(|call| call.tool == RECALL_TOOL);
                            echoes |= echo;
                            labels.push(about.to_string());
                            if let Some(call) = call
                                && let Some(file) = &call.file
                                && !is_error
                            {
                                match call.tool {
                                    "read" => touches.push(Touch {
                                        path: file.clone(),
                                        written: false,
                                        entry: entry.id.clone(),
                                    }),
                                    "write" | "edit" => touches.push(Touch {
                                        path: file.clone(),
                                        written: true,
                                        entry: call.entry.to_string(),
                                    }),
                                    _ => {}
                                }
                            }
                            if message.content.len() > 1 {
                                let failed = if *is_error { " (failed)" } else { "" };
                                add(&format!("# {about}{failed}"), echo, &mut searched);
                            }
                            for inner in content {
                                match inner {
                                    Block::Text { text } => add(text, echo, &mut searched),
                                    Block::Image { .. } => add("[image]", echo, &mut searched),
                                    _ => {}
                                }
                            }
                        }
                        Block::Thinking { .. } | Block::Opaque { .. } => {}
                    }
                }
                let label = match message.role {
                    Role::User => "user".to_string(),
                    Role::Assistant => "assistant".to_string(),
                    Role::Tool => match labels.as_slice() {
                        [one] => format!("result of {one}"),
                        many => format!("results of {} calls", many.len()),
                    },
                };
                (label, text, echoes.then_some(searched))
            }
            _ => continue,
        };
        if text.trim().is_empty() {
            continue;
        }
        items.push(Item {
            id: entry.id.clone(),
            timestamp: entry.timestamp.clone(),
            label,
            text,
            searched,
        });
    }
    (items, touches)
}

/// An id as the model may write it: with the brackets of a summary, or spaces.
fn clean_id(id: &str) -> &str {
    id.trim().trim_matches(|c| c == '[' || c == ']').trim()
}

/// The line of `text` that `matches` first, cut to [`SNIPPET_CHARS`] around the match.
fn snippet(text: &str, matches: impl Fn(&str) -> Option<usize>) -> String {
    let Some((line, at)) = text.lines().find_map(|line| matches(line).map(|at| (line, at))) else {
        return String::new();
    };
    let chars: Vec<(usize, char)> = line.char_indices().collect();
    if chars.len() <= SNIPPET_CHARS {
        return line.trim().to_string();
    }
    // Keep the match in view: start a little before it.
    let hit = chars.iter().position(|(i, _)| *i >= at).unwrap_or(0);
    let start = hit
        .saturating_sub(SNIPPET_CHARS / 4)
        .min(chars.len() - SNIPPET_CHARS);
    let shown: String = chars[start..start + SNIPPET_CHARS]
        .iter()
        .map(|(_, c)| c)
        .collect();
    let before = if start > 0 { "…" } else { "" };
    let after = if start + SNIPPET_CHARS < chars.len() {
        "…"
    } else {
        ""
    };
    format!("{before}{}{after}", shown.trim())
}

/// The items that hold any of the words of `query`, best first: those with more of the words,
/// then by how rare the words are and how often they occur, then the newest.
fn ranked<'a>(items: &'a [Item], query: &str) -> Vec<(&'a Item, String)> {
    let mut words: Vec<String> = query.split_whitespace().map(str::to_lowercase).collect();
    words.sort();
    words.dedup();
    if words.is_empty() {
        return Vec::new();
    }
    let lowered: Vec<String> = items.iter().map(|item| item.searched().to_lowercase()).collect();
    let counts: Vec<Vec<usize>> = lowered
        .iter()
        .map(|text| {
            words
                .iter()
                .map(|word| text.matches(word.as_str()).count())
                .collect()
        })
        .collect();
    let rarity: Vec<f64> = (0..words.len())
        .map(|w| {
            let holders = counts.iter().filter(|row| row[w] > 0).count();
            (1.0 + items.len() as f64 / holders.max(1) as f64).ln()
        })
        .collect();
    let mut hits: Vec<(usize, usize, f64)> = counts
        .iter()
        .enumerate()
        .filter_map(|(i, row)| {
            let distinct = row.iter().filter(|count| **count > 0).count();
            let score: f64 = row
                .iter()
                .zip(&rarity)
                .map(|(count, rarity)| rarity * (*count as f64 / (*count as f64 + 1.0)))
                .sum();
            (distinct > 0).then_some((i, distinct, score))
        })
        .collect();
    hits.sort_by(|a, b| b.1.cmp(&a.1).then(b.2.total_cmp(&a.2)).then(b.0.cmp(&a.0)));
    hits.into_iter()
        .map(|(i, _, _)| {
            let shown = snippet(items[i].searched(), |line| {
                let line = line.to_lowercase();
                words.iter().filter_map(|word| line.find(word.as_str())).min()
            });
            (&items[i], shown)
        })
        .collect()
}

fn hits_text(hits: &[(&Item, String)], limit: usize, order: &str) -> String {
    if hits.is_empty() {
        return "no entry of this conversation matches".to_string();
    }
    let mut out = match hits.len() {
        1 => "1 matching entry:\n".to_string(),
        all if all > limit => format!("{limit} of {all} matching entries, {order}:\n"),
        all => format!("{all} matching entries, {order}:\n"),
    };
    for (item, shown) in hits.iter().take(limit) {
        out.push_str(&format!("{}  {}: {shown}\n", item.id, item.label));
    }
    out.push_str("Read an entry in full with expand=[\"<id>\"].");
    out
}

impl SessionRecallTool {
    /// The branch the calling agent's conversation is on: the open session for the lead, its
    /// own transcript for a sub-agent.
    fn branch(ctx: &ToolCtx) -> Result<Vec<Entry>, String> {
        let session = ctx
            .services
            .get::<SessionStore>()
            .ok_or_else(|| "no session is open".to_string())?;
        if ctx.agent.depth == 0 {
            return Ok(session.branch(None));
        }
        let path = session.agent_transcript(&ctx.agent_id);
        SessionStore::branch_of(&path).map_err(|e| format!("cannot read {}: {e}", path.display()))
    }

    fn run(input: Input, items: &[Item], touches: &[Touch]) -> Result<String, String> {
        let query = input.query.as_deref().map(str::trim).filter(|q| !q.is_empty());
        let regex = input.regex.as_deref().filter(|r| !r.is_empty());
        if query.is_none()
            && regex.is_none()
            && input.range.is_none()
            && input.expand.is_empty()
            && !input.touched
        {
            return Err(
                "give `query` or `regex` to search the conversation, `range` or `expand` to read entries \
                 of it in full, or `touched: true` for the files it read and wrote"
                    .to_string(),
            );
        }
        let mut sections: Vec<String> = Vec::new();

        if input.touched {
            sections.push(touched_text(touches));
        }

        if !input.expand.is_empty() {
            let mut out = Vec::new();
            for id in input.expand.iter().map(|id| clean_id(id)) {
                match items.iter().find(|item| item.id == id) {
                    Some(item) => out.push(item.full()),
                    None => out.push(format!(
                        "no entry `{id}` holds a message on this branch of the conversation"
                    )),
                }
            }
            sections.push(out.join("\n\n"));
        }

        if let Some(range) = &input.range {
            let [from, to] = range.as_slice() else {
                return Err(
                    "`range` takes two entry ids: [from, to]; an empty one is the start or the end"
                        .to_string(),
                );
            };
            let position = |id: &str, fallback: usize| -> Result<usize, String> {
                let id = clean_id(id);
                if id.is_empty() {
                    return Ok(fallback);
                }
                items.iter().position(|item| item.id == id).ok_or_else(|| {
                    format!("no entry `{id}` holds a message on this branch of the conversation")
                })
            };
            let last = items.len().saturating_sub(1);
            let (from, to) = (position(from, 0)?, position(to, last)?);
            if from > to {
                return Err(format!(
                    "`range` runs backwards: {} comes after {}",
                    items[from].id, items[to].id
                ));
            }
            let limit = input.limit.unwrap_or(RANGE_LIMIT).clamp(1, RANGE_MAX);
            let span = items.get(from..=to).unwrap_or_default();
            let mut out: Vec<String> = span.iter().take(limit).map(Item::full).collect();
            if span.len() > limit {
                out.push(format!(
                    "[{} more entries; continue with range=[\"{}\", \"{}\"]]",
                    span.len() - limit,
                    span[limit].id,
                    span[span.len() - 1].id
                ));
            }
            if out.is_empty() {
                out.push("this conversation holds no message yet".to_string());
            }
            sections.push(out.join("\n\n"));
        }

        let limit = input.limit.unwrap_or(SEARCH_LIMIT).clamp(1, SEARCH_MAX);
        if let Some(pattern) = regex {
            let regex = RegexBuilder::new(pattern)
                .size_limit(REGEX_SIZE)
                .build()
                .map_err(|e| format!("`regex` is not a pattern this tool can use: {e}"))?;
            let hits: Vec<(&Item, String)> = items
                .iter()
                .filter(|item| regex.is_match(item.searched()))
                .map(|item| {
                    let shown = snippet(item.searched(), |line| regex.find(line).map(|m| m.start()));
                    (item, shown)
                })
                .collect();
            sections.push(hits_text(&hits, limit, "oldest first"));
        }
        if let Some(query) = query {
            sections.push(hits_text(&ranked(items, query), limit, "best first"));
        }
        Ok(sections.join("\n\n"))
    }
}

fn touched_text(touches: &[Touch]) -> String {
    let mut out = String::new();
    for (written, title) in [(false, "read"), (true, "written")] {
        // Each file once, in the order it was first touched, with the entries that touched it.
        let mut files: Vec<(&str, Vec<&str>)> = Vec::new();
        for touch in touches.iter().filter(|touch| touch.written == written) {
            match files.iter_mut().find(|(path, _)| *path == touch.path) {
                Some((_, entries)) => {
                    if !entries.contains(&touch.entry.as_str()) {
                        entries.push(&touch.entry);
                    }
                }
                None => files.push((&touch.path, vec![&touch.entry])),
            }
        }
        if files.is_empty() {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&format!("{title}:\n"));
        for (path, entries) in files {
            let earlier = entries.len().saturating_sub(TOUCHED_IDS);
            let shown = entries[earlier..].join(" ");
            if earlier > 0 {
                out.push_str(&format!("  {path}  {shown} (and {earlier} earlier)\n"));
            } else {
                out.push_str(&format!("  {path}  {shown}\n"));
            }
        }
    }
    if out.is_empty() {
        return "no file was read or written with `read`, `write` or `edit` in this conversation".to_string();
    }
    out.push_str(
        "An entry after a file read holds what was read; after a file written, the call that changed it. \
         expand=[\"<id>\"] shows it.",
    );
    out
}

#[async_trait]
impl Tool for SessionRecallTool {
    fn name(&self) -> &str {
        RECALL_TOOL
    }

    fn description(&self) -> &str {
        "Look up your own conversation as the session file holds it, including what a summary replaced \
         and tool results that were elided from your context. `query` finds entries by words, best match \
         first; `regex` finds them by pattern, oldest first. `expand` returns entries in full by id, and \
         `range` every entry between two ids. `touched` lists the files you read and wrote with the \
         entries that did it. Entry ids are the ones summaries and `[result elided; ...]` notes name. \
         Nothing is changed by a call."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Words to look for; entries holding more of them, and rarer ones, come first"},
                "regex": {"type": "string", "description": "A regular expression to look for instead of words"},
                "range": {
                    "type": "array",
                    "items": {"type": "string"},
                    "minItems": 2,
                    "maxItems": 2,
                    "description": "[from, to] entry ids: every entry between them, in full. An empty id is the start or the end of the conversation"
                },
                "expand": {"type": "array", "items": {"type": "string"}, "description": "Entry ids to return in full"},
                "touched": {"type": "boolean", "description": "List the files read and written, with the ids of the entries that did it"},
                "limit": {"type": "integer", "minimum": 1, "description": "Most entries to return: 10 by default for a search (at most 50), 20 for a range (at most 100)"}
            }
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn replay_safe(&self, _: &Value) -> bool {
        true
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let branch = match Self::branch(ctx) {
            Ok(branch) => branch,
            Err(why) => return ToolOutput::error(why),
        };
        let (items, touches) = index(&branch, &ctx.cwd);
        match Self::run(input, &items, &touches) {
            Ok(text) => ToolOutput::text(text),
            Err(why) => ToolOutput::error(why),
        }
    }
}
