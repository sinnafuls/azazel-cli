//! The spans of a session, made from what its agents say they do.
//!
//! One tree per time a session is open: the session, under it each prompt, under a prompt each
//! request to a model, under a request the tool calls its reply asked for. A sub-agent's prompt
//! hangs under the tool call that was running in its parent when it started.
//!
//! A span is handed out when it ends. The session's own span is handed out twice: when it
//! opens (no end yet), for backends that show a trace while it grows, and when it closes.

use std::collections::HashMap;

use azazel_agent::AgentEvent;
use azazel_ai::{Message, StopReason, StreamEvent, Usage};
use azazel_tui::text::redact;
use jiff::Timestamp;
use serde_json::{Map, Value, json};
use uuid::Uuid;

/// The most characters of one text that are sent.
const MAX_TEXT: usize = 20_000;
/// The most characters of an error that are sent.
const MAX_ERROR: usize = 500;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Session,
    Prompt,
    Request,
    Tool,
}

/// A tool call a reply asked for.
#[derive(Debug, Clone, PartialEq)]
pub struct Call {
    pub id: String,
    pub name: String,
    pub input: Value,
}

/// What was said in a span. Only recorded with `[telemetry] content = true`.
#[derive(Debug, Clone, PartialEq)]
pub enum Content {
    Prompt {
        prompt: Option<String>,
        reply: Option<String>,
    },
    Reply {
        text: String,
        calls: Vec<Call>,
    },
    Tool {
        input: Value,
        output: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub struct Span {
    pub id: Uuid,
    /// The id of the tree's root.
    pub trace: Uuid,
    pub parent: Option<Uuid>,
    /// The start and id of every span from the root down to this one, joined by dots: what
    /// LangSmith orders the runs of a trace by.
    pub order: String,
    pub kind: Kind,
    pub name: String,
    pub start: Timestamp,
    /// `None` for a span that is still open.
    pub end: Option<Timestamp>,
    pub error: Option<String>,
    pub attrs: Map<String, Value>,
    pub content: Option<Content>,
}

/// Which agent an event is from.
#[derive(Debug, Clone, Copy)]
pub struct Who<'a> {
    pub id: &'a str,
    /// The name of its definition (`lead` for the lead).
    pub def: &'a str,
    pub parent: Option<&'a str>,
}

/// A span that has started.
#[derive(Debug, Clone)]
struct Open {
    id: Uuid,
    parent: Option<Uuid>,
    order: String,
    start: Timestamp,
}

impl Open {
    fn root(now: Timestamp) -> Open {
        let id = Uuid::now_v7();
        Open {
            id,
            parent: None,
            order: order_part(now, id),
            start: now,
        }
    }

    fn child(&self, now: Timestamp) -> Open {
        let id = Uuid::now_v7();
        Open {
            id,
            parent: Some(self.id),
            order: format!("{}.{}", self.order, order_part(now, id)),
            start: now,
        }
    }
}

/// `20261113T101502123456Z<id>`.
fn order_part(at: Timestamp, id: Uuid) -> String {
    format!("{}{id}", at.strftime("%Y%m%dT%H%M%S%6fZ"))
}

struct Request {
    span: Open,
    turn: u32,
    retries: u32,
    stalls: u32,
    fallback: Option<String>,
    /// When the first piece of the reply arrived.
    first: Option<Timestamp>,
    /// When the agent was done with the request, where no reply had ended it.
    left: Option<Timestamp>,
}

struct Tool {
    span: Open,
    name: String,
    input: Value,
}

struct Prompt {
    span: Open,
    agent: String,
    def: String,
    text: Option<String>,
    reply: String,
    usage: Usage,
    requests: u32,
    tools: u32,
    compactions: u32,
    request: Option<Request>,
    /// The request whose reply asked for the calls that run now.
    asked: Option<Open>,
    calls: HashMap<String, Tool>,
}

#[derive(Default)]
struct Totals {
    prompts: u32,
    requests: u32,
    tools: u32,
    usage: Usage,
}

/// What a session is known by.
#[derive(Debug, Clone, Default)]
pub struct SessionFacts {
    pub id: String,
    pub name: Option<String>,
    /// It existed before it was opened this time.
    pub resumed: bool,
}

pub struct Recorder {
    content: bool,
    facts: SessionFacts,
    session: Open,
    /// The prompt each agent is working on.
    prompts: HashMap<String, Prompt>,
    totals: Totals,
}

/// `text` as it leaves the machine: without what looks like a secret, and not without end.
fn shown(text: &str, most: usize) -> String {
    let text = redact(text);
    match text.char_indices().nth(most) {
        Some((at, _)) => format!(
            "{}… [{} more characters]",
            &text[..at],
            text[at..].chars().count()
        ),
        None => text.into_owned(),
    }
}

fn stop_name(stop: StopReason) -> Value {
    serde_json::to_value(stop).unwrap_or(Value::Null)
}

fn usage_attrs(attrs: &mut Map<String, Value>, usage: &Usage) {
    let input = usage.input + usage.cache_read + usage.cache_write;
    attrs.insert("gen_ai.usage.input_tokens".into(), json!(input));
    attrs.insert("gen_ai.usage.output_tokens".into(), json!(usage.output));
    if usage.cache_read > 0 {
        attrs.insert(
            "gen_ai.usage.cache_read.input_tokens".into(),
            json!(usage.cache_read),
        );
    }
    if usage.cache_write > 0 {
        attrs.insert(
            "gen_ai.usage.cache_creation.input_tokens".into(),
            json!(usage.cache_write),
        );
    }
    if usage.reasoning > 0 {
        attrs.insert("azazel.usage.reasoning_tokens".into(), json!(usage.reasoning));
    }
    if let Some(cost) = usage.cost_usd {
        attrs.insert("azazel.cost_usd".into(), json!(cost));
    }
}

fn millis(from: Timestamp, to: Timestamp) -> i64 {
    i64::try_from(to.duration_since(from).as_millis()).unwrap_or(i64::MAX)
}

impl Recorder {
    /// Starts the tree of a session that was opened at `now`. The span that comes back is the
    /// session's, still open.
    pub fn new(facts: SessionFacts, content: bool, now: Timestamp) -> (Recorder, Span) {
        let recorder = Recorder {
            content,
            facts,
            session: Open::root(now),
            prompts: HashMap::new(),
            totals: Totals::default(),
        };
        let opened = recorder.session_span(None);
        (recorder, opened)
    }

    fn span(
        &self,
        open: Open,
        kind: Kind,
        name: String,
        end: Option<Timestamp>,
        mut attrs: Map<String, Value>,
    ) -> Span {
        attrs.insert("session.id".into(), json!(self.facts.id));
        Span {
            id: open.id,
            trace: self.session.id,
            parent: open.parent,
            order: open.order,
            kind,
            name,
            start: open.start,
            end,
            error: None,
            attrs,
            content: None,
        }
    }

    fn session_span(&self, end: Option<Timestamp>) -> Span {
        let mut attrs = Map::new();
        attrs.insert("azazel.session.resumed".into(), json!(self.facts.resumed));
        if let Some(name) = &self.facts.name {
            attrs.insert("azazel.session.name".into(), json!(name));
        }
        if end.is_some() {
            attrs.insert("azazel.prompts".into(), json!(self.totals.prompts));
            attrs.insert("azazel.requests".into(), json!(self.totals.requests));
            attrs.insert("azazel.tool_calls".into(), json!(self.totals.tools));
            usage_attrs(&mut attrs, &self.totals.usage);
        }
        self.span(
            self.session.clone(),
            Kind::Session,
            "session".to_string(),
            end,
            attrs,
        )
    }

    /// Whether the prompt `agent` works on is still without its text, and the text is wanted.
    pub fn needs_prompt(&self, agent: &str) -> bool {
        self.content
            && self
                .prompts
                .get(agent)
                .is_some_and(|prompt| prompt.text.is_none())
    }

    /// Says what the prompt `agent` works on was.
    pub fn prompt_text(&mut self, agent: &str, text: &str) {
        if let Some(prompt) = self.prompts.get_mut(agent) {
            prompt.text = Some(shown(text, MAX_TEXT));
        }
    }

    /// Where a prompt of `who` hangs: under the call that runs in its parent, else under the
    /// parent's prompt, else under the session.
    fn above(&self, who: &Who) -> &Open {
        let Some(parent) = who.parent.and_then(|parent| self.prompts.get(parent)) else {
            return &self.session;
        };
        parent
            .calls
            .values()
            .map(|call| &call.span)
            .max_by_key(|span| span.start)
            .unwrap_or(&parent.span)
    }

    /// Takes in one event of `who` at `now`; returns the spans it ended.
    pub fn event(&mut self, who: &Who, event: &AgentEvent, now: Timestamp) -> Vec<Span> {
        let mut done = Vec::new();
        if matches!(event, AgentEvent::AgentStart) {
            // A prompt that never said how it ended.
            self.end_prompt(who.id, now, Some("closed"), None, &mut done);
            self.totals.prompts += 1;
            let prompt = Prompt {
                span: self.above(who).child(now),
                agent: who.id.to_string(),
                def: who.def.to_string(),
                text: None,
                reply: String::new(),
                usage: Usage::default(),
                requests: 0,
                tools: 0,
                compactions: 0,
                request: None,
                asked: None,
                calls: HashMap::new(),
            };
            self.prompts.insert(who.id.to_string(), prompt);
            return done;
        }
        // Events of a prompt that began before anyone listened belong to no span.
        let Some(mut prompt) = self.prompts.remove(who.id) else {
            return done;
        };
        let mut ended = None;
        match event {
            AgentEvent::TurnStart { turn } => {
                if let Some(span) = self.end_request(&mut prompt, now, None, None) {
                    done.push(span);
                }
                prompt.asked = None;
                prompt.request = Some(Request {
                    span: prompt.span.child(now),
                    turn: *turn,
                    retries: 0,
                    stalls: 0,
                    fallback: None,
                    first: None,
                    left: None,
                });
            }
            AgentEvent::Stream(
                StreamEvent::TextDelta { .. }
                | StreamEvent::ThinkingDelta { .. }
                | StreamEvent::ToolCallStart { .. },
            ) => {
                if let Some(request) = &mut prompt.request
                    && request.first.is_none()
                {
                    request.first = Some(now);
                }
            }
            AgentEvent::Retry { stalled_secs, .. } => {
                if let Some(request) = &mut prompt.request {
                    request.retries += 1;
                    request.stalls += u32::from(stalled_secs.is_some());
                }
            }
            AgentEvent::Fallback { from, .. } => {
                if let Some(request) = &mut prompt.request {
                    request.fallback = Some(from.to_string());
                }
            }
            AgentEvent::MessageEnd { message } => {
                let text = message.text();
                if !text.trim().is_empty() {
                    prompt.reply = text;
                }
                if let Some(span) = self.end_request(&mut prompt, now, Some(message), None) {
                    done.push(span);
                }
            }
            AgentEvent::Rule { name } => {
                let note = format!("withdrawn by rule {name}");
                if let Some(mut span) = self.end_request(&mut prompt, now, None, None) {
                    span.attrs.insert("azazel.outcome".into(), json!(note));
                    done.push(span);
                }
            }
            AgentEvent::TurnEnd => {
                if let Some(request) = &mut prompt.request {
                    request.left = Some(now);
                }
            }
            AgentEvent::ToolStart { call_id, name, input } => {
                let under = prompt.asked.as_ref().unwrap_or(&prompt.span);
                let tool = Tool {
                    span: under.child(now),
                    name: name.clone(),
                    input: input.clone(),
                };
                prompt.calls.insert(call_id.clone(), tool);
            }
            AgentEvent::ToolEnd {
                call_id,
                output,
                duration_ms,
            } => {
                if let Some(tool) = prompt.calls.remove(call_id) {
                    prompt.tools += 1;
                    self.totals.tools += 1;
                    let text: String = output
                        .content
                        .iter()
                        .filter_map(|block| match block {
                            azazel_ai::Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let mut attrs = Map::new();
                    attrs.insert("gen_ai.operation.name".into(), json!("execute_tool"));
                    attrs.insert("gen_ai.tool.name".into(), json!(tool.name));
                    attrs.insert("gen_ai.tool.call.id".into(), json!(call_id));
                    attrs.insert("gen_ai.agent.id".into(), json!(prompt.agent));
                    attrs.insert("azazel.tool.duration_ms".into(), json!(duration_ms));
                    attrs.insert("azazel.tool.output_chars".into(), json!(text.chars().count()));
                    attrs.insert("azazel.tool.is_error".into(), json!(output.is_error));
                    let name = format!("execute_tool {}", tool.name);
                    let mut span = self.span(tool.span, Kind::Tool, name, Some(now), attrs);
                    if output.is_error {
                        span.error = Some(if self.content {
                            shown(text.lines().next().unwrap_or_default(), MAX_ERROR)
                        } else {
                            "the tool reported an error".to_string()
                        });
                    }
                    if self.content {
                        // The input as an object again when hiding secrets left it one.
                        let input = shown(&tool.input.to_string(), MAX_TEXT);
                        span.content = Some(Content::Tool {
                            input: serde_json::from_str(&input).unwrap_or(Value::String(input)),
                            output: shown(&text, MAX_TEXT),
                        });
                    }
                    done.push(span);
                }
            }
            AgentEvent::Compaction { .. } => prompt.compactions += 1,
            AgentEvent::Idle { stop } => ended = Some((stop_name(*stop), None)),
            AgentEvent::Failed { error } => {
                ended = Some((json!("failed"), Some(shown(error, MAX_ERROR))));
            }
            _ => {}
        }
        self.prompts.insert(who.id.to_string(), prompt);
        if let Some((stop, error)) = ended {
            self.end_prompt(who.id, now, stop.as_str(), error, &mut done);
        }
        done
    }

    /// Ends the request of `prompt` that is open, if one is: with the reply that answered it,
    /// or without one.
    fn end_request(
        &mut self,
        prompt: &mut Prompt,
        now: Timestamp,
        reply: Option<&Message>,
        error: Option<&str>,
    ) -> Option<Span> {
        let request = prompt.request.take()?;
        prompt.requests += 1;
        self.totals.requests += 1;
        let mut attrs = Map::new();
        attrs.insert("gen_ai.operation.name".into(), json!("chat"));
        attrs.insert("gen_ai.agent.id".into(), json!(prompt.agent));
        attrs.insert("azazel.turn".into(), json!(request.turn));
        if request.retries > 0 {
            attrs.insert("azazel.retries".into(), json!(request.retries));
        }
        if request.stalls > 0 {
            attrs.insert("azazel.stalls".into(), json!(request.stalls));
        }
        if let Some(from) = &request.fallback {
            attrs.insert("azazel.fallback_from".into(), json!(from));
        }
        if let Some(first) = request.first {
            attrs.insert(
                "azazel.first_token_ms".into(),
                json!(millis(request.span.start, first)),
            );
        }
        let mut name = "chat".to_string();
        let mut content = None;
        let meta = reply.and_then(|message| message.meta.as_ref());
        if let Some(meta) = meta {
            name = format!("chat {}", meta.model);
            attrs.insert("gen_ai.provider.name".into(), json!(meta.provider));
            attrs.insert("gen_ai.request.model".into(), json!(meta.model));
            attrs.insert("gen_ai.response.model".into(), json!(meta.model));
            attrs.insert(
                "gen_ai.response.finish_reasons".into(),
                json!([stop_name(meta.stop)]),
            );
            if let Some(id) = &meta.response_id {
                attrs.insert("gen_ai.response.id".into(), json!(id));
            }
            usage_attrs(&mut attrs, &meta.usage);
            prompt.usage.add(&meta.usage);
            self.totals.usage.add(&meta.usage);
        }
        if let Some(message) = reply {
            let calls: Vec<Call> = message
                .tool_calls()
                .map(|(id, name, input)| Call {
                    id: id.to_string(),
                    name: name.to_string(),
                    input: input.clone(),
                })
                .collect();
            let text = message.text();
            attrs.insert("azazel.reply_chars".into(), json!(text.chars().count()));
            attrs.insert("azazel.tool_calls".into(), json!(calls.len()));
            if self.content {
                let calls = calls
                    .into_iter()
                    .map(|call| {
                        let input = shown(&call.input.to_string(), MAX_TEXT);
                        Call {
                            input: serde_json::from_str(&input).unwrap_or(Value::String(input)),
                            ..call
                        }
                    })
                    .collect();
                content = Some(Content::Reply {
                    text: shown(&text, MAX_TEXT),
                    calls,
                });
            }
            prompt.asked = Some(request.span.clone());
        }
        let end = match reply {
            Some(_) => now,
            None => {
                if error.is_none() {
                    attrs.insert("azazel.outcome".into(), json!("no reply"));
                }
                request.left.unwrap_or(now)
            }
        };
        let mut span = self.span(request.span, Kind::Request, name, Some(end), attrs);
        span.error = error.map(str::to_string);
        span.content = content;
        Some(span)
    }

    /// Ends what `agent` works on: the request and the calls that are open, then the prompt.
    /// `stop` is how it ended.
    fn end_prompt(
        &mut self,
        agent: &str,
        now: Timestamp,
        stop: Option<&str>,
        error: Option<String>,
        done: &mut Vec<Span>,
    ) {
        let Some(mut prompt) = self.prompts.remove(agent) else {
            return;
        };
        if let Some(mut span) = self.end_request(&mut prompt, now, None, error.as_deref()) {
            if let (None, Some(stop)) = (&error, stop) {
                span.attrs.insert("azazel.outcome".into(), json!(stop));
            }
            done.push(span);
        }
        for (call_id, tool) in std::mem::take(&mut prompt.calls) {
            let mut attrs = Map::new();
            attrs.insert("gen_ai.operation.name".into(), json!("execute_tool"));
            attrs.insert("gen_ai.tool.name".into(), json!(tool.name));
            attrs.insert("gen_ai.tool.call.id".into(), json!(call_id));
            attrs.insert("azazel.outcome".into(), json!("no result"));
            let name = format!("execute_tool {}", tool.name);
            done.push(self.span(tool.span, Kind::Tool, name, Some(now), attrs));
        }
        let mut attrs = Map::new();
        attrs.insert("gen_ai.operation.name".into(), json!("invoke_agent"));
        attrs.insert("gen_ai.agent.name".into(), json!(prompt.def));
        attrs.insert("gen_ai.agent.id".into(), json!(prompt.agent));
        attrs.insert("azazel.requests".into(), json!(prompt.requests));
        attrs.insert("azazel.tool_calls".into(), json!(prompt.tools));
        if prompt.compactions > 0 {
            attrs.insert("azazel.compactions".into(), json!(prompt.compactions));
        }
        if let Some(stop) = stop {
            attrs.insert("azazel.stop".into(), json!(stop));
        }
        usage_attrs(&mut attrs, &prompt.usage);
        let mut span = self.span(prompt.span, Kind::Prompt, "prompt".to_string(), Some(now), attrs);
        span.error = error;
        if self.content {
            span.content = Some(Content::Prompt {
                prompt: prompt.text,
                reply: (!prompt.reply.is_empty()).then(|| shown(&prompt.reply, MAX_TEXT)),
            });
        }
        done.push(span);
    }

    /// The session closes at `now`: what is still open ends, the session's span last.
    pub fn close(&mut self, now: Timestamp) -> Vec<Span> {
        let mut done = Vec::new();
        // A sub-agent's prompt before the prompt it hangs under.
        let mut open: Vec<(Timestamp, String)> = self
            .prompts
            .iter()
            .map(|(agent, prompt)| (prompt.span.start, agent.clone()))
            .collect();
        open.sort();
        for (_, agent) in open.into_iter().rev() {
            self.end_prompt(&agent, now, Some("closed"), None, &mut done);
        }
        done.push(self.session_span(Some(now)));
        done
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::ToolOutput;
    use azazel_ai::{AssistantMeta, Block, ModelRef, WireApi};

    use super::*;

    fn at(ms: i64) -> Timestamp {
        Timestamp::from_millisecond(1_800_000_000_000 + ms).unwrap()
    }

    const LEAD: Who = Who {
        id: "lead",
        def: "lead",
        parent: None,
    };

    fn reply(content: Vec<Block>, stop: StopReason, usage: Usage) -> AgentEvent {
        let meta = AssistantMeta {
            provider: "mock".into(),
            model: "lead".into(),
            api: WireApi::AnthropicMessages,
            usage,
            stop,
            response_id: Some("resp_1".into()),
            duration_ms: 0,
            sources: Vec::new(),
        };
        AgentEvent::MessageEnd {
            message: Message::assistant(content, meta),
        }
    }

    fn call(id: &str, name: &str, input: Value) -> Block {
        Block::ToolCall {
            id: id.into(),
            name: name.into(),
            input,
            signature: None,
        }
    }

    fn recorder(content: bool) -> Recorder {
        let facts = SessionFacts {
            id: "s1".into(),
            name: None,
            resumed: false,
        };
        Recorder::new(facts, content, at(0)).0
    }

    /// Feeds `events` to `recorder`, each at its time, and collects what ended.
    fn feed(recorder: &mut Recorder, who: &Who, events: Vec<(i64, AgentEvent)>) -> Vec<Span> {
        events
            .into_iter()
            .flat_map(|(ms, event)| recorder.event(who, &event, at(ms)))
            .collect()
    }

    /// One prompt: a request that asks for a tool, the tool, a request that answers.
    fn one_prompt() -> Vec<(i64, AgentEvent)> {
        let usage = Usage {
            input: 100,
            output: 20,
            cache_read: 900,
            cache_write: 0,
            reasoning: 5,
            cost_usd: Some(0.01),
        };
        vec![
            (10, AgentEvent::AgentStart),
            (20, AgentEvent::TurnStart { turn: 1 }),
            (30, AgentEvent::MessageStart),
            (
                45,
                AgentEvent::Stream(StreamEvent::TextDelta {
                    index: 0,
                    delta: "I".into(),
                }),
            ),
            (
                120,
                reply(
                    vec![
                        Block::text("I will read it."),
                        call("c1", "read", json!({"path": "a.rs"})),
                    ],
                    StopReason::ToolUse,
                    usage,
                ),
            ),
            (
                130,
                AgentEvent::ToolStart {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"}),
                },
            ),
            (
                180,
                AgentEvent::ToolEnd {
                    call_id: "c1".into(),
                    output: ToolOutput::text("fn main() {}"),
                    duration_ms: 50,
                },
            ),
            (190, AgentEvent::TurnEnd),
            (200, AgentEvent::TurnStart { turn: 2 }),
            (
                300,
                reply(
                    vec![Block::text("It is a main function.")],
                    StopReason::Stop,
                    usage,
                ),
            ),
            (310, AgentEvent::TurnEnd),
            (
                320,
                AgentEvent::Idle {
                    stop: StopReason::Stop,
                },
            ),
        ]
    }

    #[test]
    fn a_prompt_is_a_tree_of_requests_and_the_calls_they_asked_for() {
        let mut recorder = recorder(false);
        let mut spans = feed(&mut recorder, &LEAD, one_prompt());
        spans.extend(recorder.close(at(400)));
        let kinds: Vec<Kind> = spans.iter().map(|span| span.kind).collect();
        assert_eq!(
            kinds,
            [
                Kind::Request,
                Kind::Tool,
                Kind::Request,
                Kind::Prompt,
                Kind::Session
            ]
        );
        let [first, tool, second, prompt, session] = &spans[..] else {
            unreachable!()
        };
        // session -> prompt -> request -> tool call
        assert_eq!(session.parent, None);
        assert_eq!(prompt.parent, Some(session.id));
        assert_eq!(first.parent, Some(prompt.id));
        assert_eq!(second.parent, Some(prompt.id));
        assert_eq!(tool.parent, Some(first.id));
        assert!(spans.iter().all(|span| span.trace == session.id));
        assert!(tool.order.starts_with(&format!("{}.", first.order)));
        assert!(first.order.starts_with(&format!("{}.", prompt.order)));
        assert_eq!(session.order, format!("20270115T080000000000Z{}", session.id));

        // Durations are those of the events.
        assert_eq!((first.start, first.end), (at(20), Some(at(120))));
        assert_eq!((tool.start, tool.end), (at(130), Some(at(180))));
        assert_eq!((prompt.start, prompt.end), (at(10), Some(at(320))));
        assert_eq!((session.start, session.end), (at(0), Some(at(400))));

        // Model, tokens and cost of a request; cached input counts as input.
        assert_eq!(first.name, "chat lead");
        assert_eq!(first.attrs["gen_ai.provider.name"], "mock");
        assert_eq!(first.attrs["gen_ai.request.model"], "lead");
        assert_eq!(first.attrs["gen_ai.usage.input_tokens"], 1000);
        assert_eq!(first.attrs["gen_ai.usage.cache_read.input_tokens"], 900);
        assert_eq!(first.attrs["gen_ai.usage.output_tokens"], 20);
        assert_eq!(first.attrs["azazel.cost_usd"], 0.01);
        assert_eq!(first.attrs["gen_ai.response.finish_reasons"], json!(["tool_use"]));
        assert_eq!(first.attrs["azazel.first_token_ms"], 25);
        assert_eq!(first.attrs["azazel.tool_calls"], 1);
        assert_eq!(tool.name, "execute_tool read");
        assert_eq!(tool.attrs["azazel.tool.duration_ms"], 50);

        // The prompt and the session add up what happened under them.
        assert_eq!(prompt.attrs["azazel.requests"], 2);
        assert_eq!(prompt.attrs["azazel.tool_calls"], 1);
        assert_eq!(prompt.attrs["azazel.stop"], "stop");
        assert_eq!(prompt.attrs["gen_ai.usage.output_tokens"], 40);
        assert_eq!(prompt.attrs["azazel.cost_usd"], 0.02);
        assert_eq!(session.attrs["azazel.prompts"], 1);
        assert_eq!(session.attrs["gen_ai.usage.input_tokens"], 2000);
        assert!(spans.iter().all(|span| span.attrs["session.id"] == "s1"));

        // Nothing that was said is in a span.
        assert!(
            spans
                .iter()
                .all(|span| span.content.is_none() && span.error.is_none())
        );
    }

    #[test]
    fn what_was_said_is_kept_only_when_asked_for_and_without_secrets() {
        let mut recorder = recorder(true);
        let mut events = one_prompt();
        events[6].1 = AgentEvent::ToolEnd {
            call_id: "c1".into(),
            output: ToolOutput::text("token = ghp_0123456789abcdefghijklmnopqrstuvwxyzAB"),
            duration_ms: 50,
        };
        let mut spans = Vec::new();
        for (ms, event) in events {
            if matches!(event, AgentEvent::MessageStart) {
                assert!(recorder.needs_prompt("lead"));
                recorder.prompt_text("lead", "what is a.rs?");
                assert!(!recorder.needs_prompt("lead"));
            }
            spans.extend(recorder.event(&LEAD, &event, at(ms)));
        }
        assert_eq!(
            spans[0].content,
            Some(Content::Reply {
                text: "I will read it.".into(),
                calls: vec![Call {
                    id: "c1".into(),
                    name: "read".into(),
                    input: json!({"path": "a.rs"})
                }],
            })
        );
        let Some(Content::Tool { input, output }) = &spans[1].content else {
            panic!("the call has no content")
        };
        assert_eq!(input, &json!({"path": "a.rs"}));
        assert!(
            output.contains("[redacted:") && !output.contains("ghp_0123"),
            "{output}"
        );
        assert_eq!(
            spans[3].content,
            Some(Content::Prompt {
                prompt: Some("what is a.rs?".into()),
                reply: Some("It is a main function.".into()),
            })
        );

        // Without content nobody asks for the prompt's text.
        let mut quiet = super::tests::recorder(false);
        quiet.event(&LEAD, &AgentEvent::AgentStart, at(1));
        assert!(!quiet.needs_prompt("lead"));
    }

    #[test]
    fn a_long_text_is_cut_and_says_by_how_much() {
        let text = "é".repeat(MAX_TEXT + 7);
        let cut = shown(&text, MAX_TEXT);
        assert!(
            cut.ends_with("… [7 more characters]"),
            "{}",
            &cut[cut.len() - 40..]
        );
        assert_eq!(cut.chars().filter(|c| *c == 'é').count(), MAX_TEXT);
        assert_eq!(shown("short", MAX_TEXT), "short");
    }

    #[test]
    fn a_failed_request_and_a_failed_tool_carry_the_error() {
        let mut recorder = recorder(false);
        let spans = feed(
            &mut recorder,
            &LEAD,
            vec![
                (0, AgentEvent::AgentStart),
                (10, AgentEvent::TurnStart { turn: 1 }),
                (
                    20,
                    reply(
                        vec![call("c1", "shell", json!({"command": "false"}))],
                        StopReason::ToolUse,
                        Usage::default(),
                    ),
                ),
                (
                    30,
                    AgentEvent::ToolStart {
                        call_id: "c1".into(),
                        name: "shell".into(),
                        input: json!({"command": "false"}),
                    },
                ),
                (
                    40,
                    AgentEvent::ToolEnd {
                        call_id: "c1".into(),
                        output: ToolOutput::error("exit code 1\nsecret details"),
                        duration_ms: 10,
                    },
                ),
                (50, AgentEvent::TurnEnd),
                (60, AgentEvent::TurnStart { turn: 2 }),
                (
                    70,
                    AgentEvent::Retry {
                        attempt: 1,
                        model: ModelRef::new("mock", "lead"),
                        stalled_secs: Some(20),
                    },
                ),
                (500, AgentEvent::TurnEnd),
                (
                    510,
                    AgentEvent::Failed {
                        error: "mock/lead: 529 overloaded".into(),
                    },
                ),
            ],
        );
        let tool = spans.iter().find(|span| span.kind == Kind::Tool).unwrap();
        // What the tool printed is content; without content only that it failed is said.
        assert_eq!(tool.error.as_deref(), Some("the tool reported an error"));
        assert_eq!(tool.attrs["azazel.tool.is_error"], true);
        let failed = &spans[spans.len() - 2];
        assert_eq!(failed.kind, Kind::Request);
        assert_eq!(failed.name, "chat");
        assert_eq!(failed.error.as_deref(), Some("mock/lead: 529 overloaded"));
        // It ended when the agent left it, not when the failure was announced.
        assert_eq!(failed.end, Some(at(500)));
        assert_eq!(failed.attrs["azazel.retries"], 1);
        assert_eq!(failed.attrs["azazel.stalls"], 1);
        let prompt = spans.last().unwrap();
        assert_eq!(prompt.kind, Kind::Prompt);
        assert_eq!(prompt.error.as_deref(), Some("mock/lead: 529 overloaded"));
        assert_eq!(prompt.attrs["azazel.stop"], "failed");
        assert_eq!(prompt.attrs["azazel.requests"], 2);

        // With content the tool's own first line is the error.
        let mut loud = super::tests::recorder(true);
        let spans = feed(
            &mut loud,
            &LEAD,
            vec![
                (0, AgentEvent::AgentStart),
                (
                    1,
                    AgentEvent::ToolStart {
                        call_id: "c1".into(),
                        name: "shell".into(),
                        input: json!({}),
                    },
                ),
                (
                    2,
                    AgentEvent::ToolEnd {
                        call_id: "c1".into(),
                        output: ToolOutput::error("exit code 1\nmore"),
                        duration_ms: 1,
                    },
                ),
            ],
        );
        assert_eq!(spans[0].error.as_deref(), Some("exit code 1"));
    }

    #[test]
    fn a_sub_agent_hangs_under_the_call_that_started_it() {
        let mut recorder = recorder(false);
        let scout = Who {
            id: "scout-1",
            def: "scout",
            parent: Some("lead"),
        };
        let mut spans = feed(
            &mut recorder,
            &LEAD,
            vec![
                (0, AgentEvent::AgentStart),
                (10, AgentEvent::TurnStart { turn: 1 }),
                (
                    20,
                    reply(
                        vec![call("c1", "agent", json!({"agent": "scout"}))],
                        StopReason::ToolUse,
                        Usage::default(),
                    ),
                ),
                (
                    30,
                    AgentEvent::ToolStart {
                        call_id: "c1".into(),
                        name: "agent".into(),
                        input: json!({"agent": "scout"}),
                    },
                ),
            ],
        );
        spans.extend(feed(
            &mut recorder,
            &scout,
            vec![
                (40, AgentEvent::AgentStart),
                (50, AgentEvent::TurnStart { turn: 1 }),
                (
                    60,
                    reply(vec![Block::text("found it")], StopReason::Stop, Usage::default()),
                ),
                (
                    70,
                    AgentEvent::Idle {
                        stop: StopReason::Stop,
                    },
                ),
            ],
        ));
        spans.extend(feed(
            &mut recorder,
            &LEAD,
            vec![(
                80,
                AgentEvent::ToolEnd {
                    call_id: "c1".into(),
                    output: ToolOutput::text("found it"),
                    duration_ms: 50,
                },
            )],
        ));
        let tool = spans.iter().find(|span| span.kind == Kind::Tool).unwrap();
        let sub = spans
            .iter()
            .find(|span| span.kind == Kind::Prompt && span.attrs["gen_ai.agent.id"] == "scout-1")
            .unwrap();
        assert_eq!(sub.parent, Some(tool.id));
        assert_eq!(sub.attrs["gen_ai.agent.name"], "scout");
        assert!(sub.order.starts_with(&tool.order));

        // The session closes while the lead is at work: its prompt ends as closed.
        let closing = recorder.close(at(100));
        let kinds: Vec<Kind> = closing.iter().map(|span| span.kind).collect();
        assert_eq!(kinds, [Kind::Prompt, Kind::Session]);
        assert_eq!(closing[0].attrs["azazel.stop"], "closed");
        assert_eq!(closing[1].attrs["azazel.prompts"], 2);
    }

    #[test]
    fn events_before_a_prompt_began_make_no_span() {
        let mut recorder = recorder(false);
        let spans = feed(
            &mut recorder,
            &LEAD,
            vec![
                (0, AgentEvent::TurnStart { turn: 3 }),
                (
                    10,
                    AgentEvent::Failed {
                        error: "compaction failed".into(),
                    },
                ),
            ],
        );
        assert!(spans.is_empty());
    }
}
