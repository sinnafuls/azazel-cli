//! Tool calls in flight, from the moment the model starts writing one until it ends. The lead's
//! calls, those of the sub-agent followed in the transcript and those of every agent's pane are
//! kept the same way.
//!
//! A call shows as soon as the model names it. While the arguments stream in, the start of what
//! has arrived is read again now and then (each time it has grown by a part of its length, so a
//! long call costs little): its summary fills in, and the text a `write` or an `edit` puts into
//! a file grows in its body. When the call starts it becomes the running call, which shows what
//! the tool prints.

use std::time::{Duration, Instant};

use azazel_agent::{AgentEvent, ToolOutput};
use azazel_ai::{Block, StreamEvent};
use azazel_tui::components::tool_card;
use serde_json::{Map, Value};

use super::partial_json::{self, Field};
use super::view;

/// The arguments are read again once they have grown by this many bytes, or by a 32nd of their
/// length when that is more.
const MIN_GROWTH: usize = 256;
/// The most characters of a value that is still arriving a summary is made from.
const SUMMARY_CHARS: usize = 200;

pub(super) struct RunningTool {
    pub call_id: String,
    pub name: String,
    pub input: Value,
    /// The last lines the tool printed; while the model writes the call, the last lines of the
    /// text it is putting into a file.
    pub tail: Vec<String>,
    pub started: Instant,
    /// The model is writing the call, or has written it and it has not started yet.
    streamed: Option<Streamed>,
}

struct Streamed {
    /// The place of the call in the message being streamed.
    index: usize,
    /// The arguments as far as they have arrived.
    args: String,
    /// How long `args` was when it was last read.
    read: usize,
}

/// A call that ended, with what it returned.
pub(super) struct Ended {
    pub tool: RunningTool,
    pub output: ToolOutput,
    pub elapsed: Duration,
}

impl RunningTool {
    /// The model is still writing the call: it has not started.
    pub(super) fn writing(&self) -> bool {
        self.streamed.is_some()
    }

    fn grow(&mut self, delta: &str) {
        let Some(streamed) = &mut self.streamed else {
            return;
        };
        streamed.args.push_str(delta);
        let length = streamed.args.len();
        if length - streamed.read < MIN_GROWTH.max(length / 32) {
            return;
        }
        streamed.read = length;
        let fields = partial_json::string_fields(&streamed.args);
        let mut input = Map::new();
        for field in fields.iter().filter(|field| field.depth == 1) {
            let shown: String = field.value.chars().take(SUMMARY_CHARS).collect();
            input.insert(field.key.clone(), Value::String(shown));
        }
        self.input = Value::Object(input);
        let body = match self.name.as_str() {
            "write" => fields.iter().rev().find(|field| top(field, "content")),
            "edit" => fields
                .iter()
                .rev()
                .find(|field| field.key == "new" && field.depth == 3),
            _ => None,
        };
        self.tail = last_lines(body.map_or("", |field| field.value.as_str()));
    }

    /// The model has written the call: its arguments are whole.
    fn written(&mut self, id: &str, input: &Value) {
        self.call_id = id.to_string();
        self.input = input.clone();
        let body = match self.name.as_str() {
            "write" => input.get("content"),
            "edit" => input
                .get("edits")
                .and_then(Value::as_array)
                .and_then(|edits| edits.last())
                .and_then(|edit| edit.get("new")),
            _ => None,
        };
        self.tail = last_lines(body.and_then(Value::as_str).unwrap_or_default());
        if let Some(streamed) = &mut self.streamed {
            streamed.args = String::new();
        }
    }
}

fn top(field: &Field, key: &str) -> bool {
    field.depth == 1 && field.key == key
}

/// The last lines of a text being written, as many as a collapsed card shows.
fn last_lines(text: &str) -> Vec<String> {
    let mut lines: Vec<String> = text
        .lines()
        .rev()
        .take(view::COLLAPSED_LINES)
        .map(str::to_string)
        .collect();
    lines.reverse();
    lines
}

/// Brings `tools` up to date with `event`; the call that ended with it, if one did.
pub(super) fn track(tools: &mut Vec<RunningTool>, event: &AgentEvent) -> Option<Ended> {
    match event {
        // The calls of a message that is taken back, or not finished, never start.
        AgentEvent::MessageStart | AgentEvent::Retry { .. } | AgentEvent::Rule { .. } => {
            tools.retain(|tool| !tool.writing());
        }
        AgentEvent::Stream(StreamEvent::ToolCallStart { index, id, name }) => tools.push(RunningTool {
            call_id: id.clone(),
            name: name.clone(),
            input: Value::Object(Map::new()),
            tail: Vec::new(),
            started: Instant::now(),
            streamed: Some(Streamed {
                index: *index,
                args: String::new(),
                read: 0,
            }),
        }),
        AgentEvent::Stream(StreamEvent::ToolCallDelta { index, delta }) => {
            if let Some(tool) = streamed_at(tools, *index) {
                tool.grow(delta);
            }
        }
        AgentEvent::Stream(StreamEvent::BlockEnd {
            index,
            block: Block::ToolCall { id, input, .. },
        }) => {
            if let Some(tool) = streamed_at(tools, *index) {
                tool.written(id, input);
            }
        }
        AgentEvent::ToolStart { call_id, name, input } => {
            let running = RunningTool {
                call_id: call_id.clone(),
                name: name.clone(),
                input: input.clone(),
                tail: Vec::new(),
                started: Instant::now(),
                streamed: None,
            };
            // The card the model's writing made is the one that runs now.
            let written = tools
                .iter()
                .position(|tool| tool.writing() && tool.call_id == *call_id)
                .or_else(|| tools.iter().position(|tool| tool.writing() && tool.name == *name));
            match written {
                Some(at) => tools[at] = running,
                None => tools.push(running),
            }
        }
        AgentEvent::ToolUpdate { call_id, text } => {
            if let Some(tool) = tools.iter_mut().find(|tool| tool.call_id == *call_id) {
                tool.tail.push(text.clone());
                let excess = tool.tail.len().saturating_sub(tool_card::MAX_OUTPUT_LINES);
                tool.tail.drain(..excess);
            }
        }
        AgentEvent::ToolEnd {
            call_id,
            output,
            duration_ms,
        } => {
            let at = tools
                .iter()
                .position(|tool| !tool.writing() && tool.call_id == *call_id)?;
            return Some(Ended {
                tool: tools.remove(at),
                output: output.clone(),
                elapsed: Duration::from_millis(*duration_ms),
            });
        }
        AgentEvent::Idle { .. } | AgentEvent::Failed { .. } => tools.clear(),
        _ => {}
    }
    None
}

fn streamed_at(tools: &mut [RunningTool], index: usize) -> Option<&mut RunningTool> {
    tools.iter_mut().find(|tool| {
        tool.streamed
            .as_ref()
            .is_some_and(|streamed| streamed.index == index)
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn stream(event: StreamEvent) -> AgentEvent {
        AgentEvent::Stream(event)
    }

    #[test]
    fn a_call_shows_while_it_is_written_and_becomes_the_running_call() {
        let mut tools = Vec::new();
        track(&mut tools, &AgentEvent::MessageStart);
        track(
            &mut tools,
            &stream(StreamEvent::ToolCallStart {
                index: 1,
                id: "c1".to_string(),
                name: "write".to_string(),
            }),
        );
        assert_eq!(tools.len(), 1, "the card is there as soon as the call is named");
        let lines: Vec<String> = (1..=200).map(|n| format!("line {n} of the file")).collect();
        let args = json!({"path": "src/big.rs", "content": lines.join("\n")}).to_string();
        // The arguments arrive a few bytes at a time.
        let chunks: Vec<String> = args
            .chars()
            .collect::<Vec<_>>()
            .chunks(7)
            .map(|chunk| chunk.iter().collect())
            .collect();
        let mut seen = Vec::new();
        for chunk in &chunks {
            track(
                &mut tools,
                &stream(StreamEvent::ToolCallDelta {
                    index: 1,
                    delta: chunk.clone(),
                }),
            );
            seen.push(tools[0].tail.last().cloned().unwrap_or_default());
        }
        assert_eq!(view::tool_summary("write", &tools[0].input), "src/big.rs");
        assert!(tools[0].tail.len() <= view::COLLAPSED_LINES);
        // The body grew as the content came, from its start towards its end.
        let first = seen.iter().position(|line| line.starts_with("line ")).unwrap();
        assert!(seen[first].starts_with("line 1"), "{}", seen[first]);
        assert!(seen.last().unwrap().starts_with("line 19"), "{:?}", seen.last());

        let input: Value = serde_json::from_str(&args).unwrap();
        track(
            &mut tools,
            &stream(StreamEvent::BlockEnd {
                index: 1,
                block: Block::ToolCall {
                    id: "c1".to_string(),
                    name: "write".to_string(),
                    input: input.clone(),
                    signature: None,
                },
            }),
        );
        assert_eq!(
            tools[0].tail.last().map(String::as_str),
            Some("line 200 of the file")
        );

        // Starting, the card is the same one; ending, it is gone.
        track(
            &mut tools,
            &AgentEvent::ToolStart {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                input,
            },
        );
        assert_eq!(tools.len(), 1);
        assert!(!tools[0].writing() && tools[0].tail.is_empty());
        let ended = track(
            &mut tools,
            &AgentEvent::ToolEnd {
                call_id: "c1".to_string(),
                output: ToolOutput::text("created src/big.rs"),
                duration_ms: 4,
            },
        );
        assert!(ended.is_some_and(|ended| ended.tool.call_id == "c1"));
        assert!(tools.is_empty());
    }

    #[test]
    fn calls_of_a_message_that_is_retried_never_start() {
        let mut tools = Vec::new();
        track(
            &mut tools,
            &stream(StreamEvent::ToolCallStart {
                index: 0,
                id: "c1".to_string(),
                name: "shell".to_string(),
            }),
        );
        track(&mut tools, &AgentEvent::MessageStart);
        assert!(tools.is_empty());
    }
}
