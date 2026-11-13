//! The frames of a session as lines a person reads: what `azazel logs` prints and what the
//! line client of `azazel attach` shows.

use std::collections::HashMap;
use std::io::Write;

use azazel_protocol::{Frame, StreamEvent, UiRequest};
use serde_json::Value;

use crate::tui::view::tool_summary;

const LEAD: &str = "lead";

pub(crate) struct Printer<W: Write> {
    out: W,
    /// Whether the reader can answer: requests then say how.
    interactive: bool,
    /// The lead's reply is being written and its line is not finished.
    mid_line: bool,
    /// Text of the message in progress has been written.
    streamed: bool,
    /// The tool of each call in flight, by agent and call.
    calls: HashMap<(String, String), String>,
    /// The state each sub-agent was last seen in.
    members: HashMap<String, String>,
}

/// The text blocks of a message as `message_end` carries it.
fn message_text(message: &Value) -> String {
    message["content"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|block| block["type"] == "text")
        .filter_map(|block| block["text"].as_str())
        .collect()
}

fn numbered(options: &[String]) -> String {
    options
        .iter()
        .enumerate()
        .map(|(index, option)| format!("  {}. {option}", index + 1))
        .collect::<Vec<_>>()
        .join("\n")
}

impl<W: Write> Printer<W> {
    pub(crate) fn new(out: W, interactive: bool) -> Self {
        Printer {
            out,
            interactive,
            mid_line: false,
            streamed: false,
            calls: HashMap::new(),
            members: HashMap::new(),
        }
    }

    /// Writes a line of its own, after ending the line a reply was being written on.
    pub(crate) fn line(&mut self, text: &str) {
        if std::mem::take(&mut self.mid_line) {
            let _ = writeln!(self.out);
        }
        let _ = writeln!(self.out, "{text}");
        let _ = self.out.flush();
    }

    fn of(&mut self, agent: &str, text: &str) {
        if agent == LEAD {
            self.line(text);
        } else {
            self.line(&format!("[{agent}] {text}"));
        }
    }

    pub(crate) fn frame(&mut self, frame: &Frame) {
        match frame {
            Frame::UserInput { kind, text } => {
                let how = match kind.as_str() {
                    "prompt" => "",
                    "steer" => "(steer) ",
                    _ => "(follow-up) ",
                };
                self.line(&format!("\n❯ {how}{text}"));
            }
            Frame::MessageStart { agent } if agent == LEAD => self.streamed = false,
            Frame::Stream {
                agent,
                event: StreamEvent::TextDelta { delta, .. },
            } if agent == LEAD => {
                let _ = write!(self.out, "{delta}");
                let _ = self.out.flush();
                self.streamed = true;
                self.mid_line = !delta.ends_with('\n');
            }
            Frame::MessageEnd { agent, message } => {
                let text = message_text(message);
                if agent != LEAD {
                    if !text.trim().is_empty() {
                        self.of(agent, text.trim());
                    }
                } else if std::mem::take(&mut self.streamed) {
                    if std::mem::take(&mut self.mid_line) {
                        let _ = writeln!(self.out);
                    }
                } else if !text.trim().is_empty() {
                    self.line(text.trim());
                }
            }
            Frame::ToolStart {
                agent,
                call_id,
                name,
                input,
            } => {
                self.calls.insert((agent.clone(), call_id.clone()), name.clone());
                let summary = tool_summary(name, input);
                self.of(agent, format!("→ {name} {summary}").trim_end());
            }
            Frame::ToolEnd {
                agent,
                call_id,
                output,
                ..
            } => {
                let name = self
                    .calls
                    .remove(&(agent.clone(), call_id.clone()))
                    .unwrap_or_default();
                if output.is_error {
                    let text = output.text();
                    let why = text
                        .lines()
                        .find(|line| !line.trim().is_empty())
                        .unwrap_or_default();
                    self.of(agent, &format!("  ✗ {name}: {}", why.trim()));
                }
            }
            Frame::Retry {
                agent,
                attempt,
                model,
            } => self.of(
                agent,
                &format!("{model} did not answer; retrying (attempt {attempt})"),
            ),
            Frame::Fallback { agent, from, to } => {
                self.of(agent, &format!("{from} is unavailable; falling back to {to}"));
            }
            Frame::Account {
                provider,
                from,
                to,
                until,
                limit,
                ..
            } => self.line(&crate::accounts::switch_line(provider, from, to, *until, limit)),
            Frame::TierDropped { agent, model, tier } => self.of(
                agent,
                &format!(
                    "{model} does not take the {tier} tier; it answers at the standard tier for the rest \
                     of this session"
                ),
            ),
            Frame::Rule { agent, name } => {
                self.of(agent, &format!("rule {name} stopped the reply; retrying"));
            }
            Frame::Compaction {
                agent,
                tokens_before,
                tokens_after,
            } => self.of(
                agent,
                &format!("context compacted: {tokens_before} → {tokens_after} tokens"),
            ),
            Frame::Idle { agent, stop } if agent == LEAD => match stop.as_str() {
                "aborted" => self.line("interrupted"),
                "length" => self.line("the reply was cut off at the model's output limit"),
                _ => self.line("· done"),
            },
            Frame::Failed { agent, error } => self.of(agent, &format!("error: {error}")),
            Frame::Roster { members } => {
                for member in members.iter().filter(|member| member.id != LEAD) {
                    let known = self.members.insert(member.id.clone(), member.state.clone());
                    if known.as_deref() != Some(member.state.as_str()) {
                        self.line(&format!("[{}] {} · {}", member.id, member.def, member.state));
                    }
                }
            }
            Frame::ApprovalRequest {
                agent,
                tool,
                subjects,
                input,
                note,
                ..
            } => {
                let what = if subjects.is_empty() {
                    input.to_string()
                } else {
                    subjects.join(", ")
                };
                self.line(&format!("? {agent} wants to use {tool}: {what}"));
                if let Some(note) = note {
                    self.line(&format!("  {note}"));
                }
                if self.interactive {
                    self.line("  /approve, /always, or /deny [what to do instead]");
                }
            }
            Frame::UiRequest { request, .. } => self.request(request),
            _ => {}
        }
    }

    fn request(&mut self, request: &UiRequest) {
        let hint = match request {
            UiRequest::Notify { level, text } => {
                let text = match level.as_str() {
                    "info" => text.clone(),
                    level => format!("{level}: {text}"),
                };
                return self.line(&text);
            }
            UiRequest::Ask {
                question,
                options,
                multi,
            } => {
                self.line(&format!("? {question}"));
                if options.is_empty() {
                    "/answer <text>"
                } else {
                    self.line(&numbered(options));
                    if *multi {
                        "/answer <numbers, separated by commas>"
                    } else {
                        "/answer <number>"
                    }
                }
            }
            UiRequest::Confirm { prompt } => {
                self.line(&format!("? {prompt}"));
                "/answer yes, or /answer no"
            }
            UiRequest::Select { prompt, options } => {
                self.line(&format!("? {prompt}"));
                self.line(&numbered(options));
                "/answer <number>"
            }
            UiRequest::Input { prompt, .. } => {
                self.line(&format!("? {prompt}"));
                "/answer <text>"
            }
            UiRequest::Plan { path, plan } => {
                self.line(&format!("? a plan is ready for review: {path}"));
                self.line(plan.trim_end());
                "/answer yes, or /answer <what to change>"
            }
        };
        if self.interactive {
            self.line(&format!("  {hint}"));
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use serde_json::json;

    use super::*;

    fn printed(frames: &[Value], interactive: bool) -> String {
        let mut out = Vec::new();
        let mut printer = Printer::new(&mut out, interactive);
        for frame in frames {
            printer.frame(&serde_json::from_value(frame.clone()).unwrap());
        }
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn a_run_reads_as_prompt_reply_tools_and_end() {
        let frames = [
            json!({ "type": "user_input", "kind": "prompt", "text": "fix it" }),
            json!({ "type": "agent_start", "agent": "lead" }),
            json!({ "type": "message_start", "agent": "lead" }),
            json!({ "type": "stream", "agent": "lead", "event": { "type": "text_delta", "index": 0, "delta": "Look" } }),
            json!({ "type": "stream", "agent": "lead", "event": { "type": "text_delta", "index": 0, "delta": "ing." } }),
            json!({ "type": "message_end", "agent": "lead",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "Looking." }] } }),
            json!({ "type": "tool_start", "agent": "lead", "call_id": "c1", "name": "read", "input": { "path": "a.rs" } }),
            json!({ "type": "tool_end", "agent": "lead", "call_id": "c1", "duration_ms": 2,
                    "output": { "content": [{ "type": "text", "text": "\nno such file\nmore" }], "is_error": true } }),
            json!({ "type": "roster", "members": [
                { "id": "lead", "def": "lead", "parent": null, "depth": 0, "state": "running", "elapsed_ms": 1, "tokens": 0, "cost_usd": null },
                { "id": "scout-1", "def": "scout", "parent": "lead", "depth": 1, "state": "running", "elapsed_ms": 1, "tokens": 0, "cost_usd": null },
            ] }),
            json!({ "type": "message_end", "agent": "scout-1",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "Found it." }] } }),
            json!({ "type": "message_start", "agent": "lead" }),
            // A reply that was not streamed is printed whole.
            json!({ "type": "message_end", "agent": "lead",
                    "message": { "role": "assistant", "content": [{ "type": "text", "text": "Fixed." }] } }),
            json!({ "type": "idle", "agent": "lead", "stop": "stop" }),
        ];
        assert_eq!(
            printed(&frames, false),
            "\n❯ fix it\nLooking.\n→ read a.rs\n  ✗ read: no such file\n[scout-1] scout · running\n\
             [scout-1] Found it.\nFixed.\n· done\n"
        );
    }

    #[test]
    fn requests_say_how_to_answer_only_to_someone_who_can() {
        let frames = [
            json!({ "type": "approval_request", "request_id": "r1", "agent": "lead", "tool": "shell",
                    "access": "exec", "subjects": ["cargo test"], "input": {}, "note": "runs for a while" }),
            json!({ "type": "ui_request", "request_id": "r2", "kind": "ask", "question": "Which?",
                    "options": ["a", "b"], "multi": false }),
            json!({ "type": "ui_request", "request_id": "r3", "kind": "notify", "level": "warn", "text": "careful" }),
        ];
        assert_eq!(
            printed(&frames, true),
            "? lead wants to use shell: cargo test\n  runs for a while\n  /approve, /always, or /deny [what to do instead]\n\
             ? Which?\n  1. a\n  2. b\n  /answer <number>\nwarn: careful\n"
        );
        assert_eq!(
            printed(&frames, false),
            "? lead wants to use shell: cargo test\n  runs for a while\n? Which?\n  1. a\n  2. b\nwarn: careful\n"
        );
    }
}
