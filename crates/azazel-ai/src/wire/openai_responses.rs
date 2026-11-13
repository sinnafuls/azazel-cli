//! OpenAI Responses wire format: `POST {base_url}/responses`.
//!
//! Requests are stateless (`store: false`): the whole conversation is sent as input items, and
//! reasoning items come back with encrypted content so they can be replayed.
//!
//! A hosted web search ([`HostedTool::WebSearch`]) is the `web_search` tool, which OpenAI, the
//! ChatGPT backend and xAI all take in this format. The pages come back as `url_citation`
//! annotations on the answer's text; xAI also lists every page it opened in the response's
//! `citations`.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::limits::Window;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{AssistantMeta, Block, HostedTool, Message, Request, Role, StopReason, Usage, WireApi};
use crate::wire::openai_chat::{
    self, ErrorInfo, STREAM_ENDED, data_url, parse_arguments, request_headers, service_tier, system_text,
    temperature,
};
use crate::wire::thinking::{ThinkingParam, thinking_param};
use crate::wire::{Decoder, Sources, apply_extra_body};

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    let instructions = system_text(req);
    if !instructions.is_empty() {
        body.insert("instructions".into(), Value::String(instructions));
    }
    body.insert("input".into(), Value::Array(input_items(provider, req)));
    body.insert("stream".into(), json!(true));
    body.insert("store".into(), json!(false));
    let functions = req.tools.iter().map(|tool| {
        json!({
            "type": "function",
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.schema,
            "strict": false,
        })
    });
    let tools: Vec<Value> = functions.chain(req.hosted.iter().map(hosted_tool)).collect();
    if !tools.is_empty() {
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(limit) = req.max_output {
        body.insert("max_output_tokens".into(), json!(limit));
    }
    if let Some(key) = &req.cache_key {
        body.insert("prompt_cache_key".into(), json!(key));
    }
    if let Some(tier) = service_tier(req) {
        body.insert("service_tier".into(), json!(tier));
    }
    if let ThinkingParam::Effort(effort) = thinking_param(provider.api, model, req.thinking) {
        body.insert("reasoning".into(), json!({"effort": effort, "summary": "auto"}));
    }
    if model.reasoning {
        // A reasoning model reasons at its default effort even when none is requested, and a
        // stateless request can only replay reasoning items that carry their encrypted content.
        body.insert("include".into(), json!(["reasoning.encrypted_content"]));
    } else if let Some(value) = req.temperature {
        // Reasoning models reject a sampling temperature.
        body.insert("temperature".into(), temperature(value));
    }
    let mut body = Value::Object(body);
    apply_extra_body(&mut body, provider);
    body
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{}/responses", provider.base_url))
        .json(&build_body(provider, model, req))
        .headers(request_headers(provider, auth))
}

pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder> {
    Box::new(ResponsesDecoder {
        provider: provider.id.clone(),
        model: model.id.clone(),
        api: provider.api,
        started: false,
        finished: false,
        response_id: None,
        usage: Usage::default(),
        blocks: Vec::new(),
        items: BTreeMap::new(),
        sources: Sources::default(),
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    openai_chat::classify_error(status, body)
}

/// The usage windows of a ChatGPT subscription, which its backend reports with every answer:
/// `x-codex-primary-*` and `x-codex-secondary-*`, each with `used-percent`, `window-minutes`
/// and `reset-at` (Unix seconds) or `reset-after-seconds`. Other endpoints of this API send
/// none of them.
pub fn limits(headers: &reqwest::header::HeaderMap) -> Vec<Window> {
    let number = |name: &str| -> Option<f64> { headers.get(name)?.to_str().ok()?.trim().parse().ok() };
    let mut windows = Vec::new();
    for key in ["primary", "secondary"] {
        let field = |name: &str| number(&format!("x-codex-{key}-{name}"));
        let Some(percent) = field("used-percent") else {
            continue;
        };
        let used = (percent / 100.0) as f32;
        windows.push(Window {
            name: crate::providers::usage::window_name(
                field("window-minutes").map(|minutes| (minutes * 60.0) as u64),
                key,
            ),
            used,
            resets_at: field("reset-at").map(|at| at as u64).or_else(|| {
                let wait = field("reset-after-seconds")? as u64;
                Some(crate::credentials::now_ms() / 1000 + wait)
            }),
            // A refusal comes as an error of its own; the headers only count.
            status: "allowed".to_string(),
        });
    }
    windows
}

pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    openai_chat::models_request(client, provider, auth)
}

pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    openai_chat::parse_models(provider, body)
}

// ---------------------------------------------------------------------------------------------
// Request mapping
// ---------------------------------------------------------------------------------------------

/// A built-in tool as the API names it.
fn hosted_tool(tool: &HostedTool) -> Value {
    match tool {
        // The API has no limit on the number of searches to pass on.
        HostedTool::WebSearch { domains, .. } if domains.is_empty() => json!({"type": "web_search"}),
        HostedTool::WebSearch { domains, .. } => {
            json!({"type": "web_search", "filters": {"allowed_domains": domains}})
        }
    }
}

fn input_items(provider: &ProviderConfig, req: &Request) -> Vec<Value> {
    let mut items = Vec::new();
    let tail = req.tail_message();
    for message in req.messages.iter().chain(&tail) {
        match message.role {
            Role::User => push_user(&mut items, content_parts(&message.content)),
            Role::Assistant => push_assistant(&mut items, provider, &message.content),
            Role::Tool => {
                for block in &message.content {
                    if let Block::ToolResult { call_id, content, .. } = block {
                        items.push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": tool_output(content),
                        }));
                    }
                }
            }
        }
    }
    items
}

/// Text and image blocks as `input_text` / `input_image` parts.
fn content_parts(content: &[Block]) -> Vec<Value> {
    content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } if !text.is_empty() => Some(json!({"type": "input_text", "text": text})),
            Block::Image { mime, data } => Some(json!({
                "type": "input_image",
                "image_url": data_url(mime, data),
                "detail": "auto",
            })),
            _ => None,
        })
        .collect()
}

/// Adjacent user messages become one message item.
fn push_user(items: &mut Vec<Value>, parts: Vec<Value>) {
    if parts.is_empty() {
        return;
    }
    let previous = items
        .last_mut()
        .filter(|item| item["type"] == "message" && item["role"] == "user")
        .and_then(|item| item.get_mut("content"))
        .and_then(Value::as_array_mut);
    match previous {
        Some(content) => content.extend(parts),
        None => items.push(json!({"type": "message", "role": "user", "content": parts})),
    }
}

/// Items in block order: text as an output message, tool calls as `function_call`, and this
/// provider's own reasoning items verbatim. Reasoning summaries (`Thinking`) are display-only.
fn push_assistant(items: &mut Vec<Value>, provider: &ProviderConfig, content: &[Block]) {
    fn flush(text_parts: &mut Vec<Value>, items: &mut Vec<Value>) {
        if !text_parts.is_empty() {
            items.push(json!({
                "type": "message",
                "role": "assistant",
                "content": std::mem::take(text_parts),
            }));
        }
    }
    let mut text_parts = Vec::new();
    for block in content {
        match block {
            Block::Text { text } if !text.is_empty() => {
                text_parts.push(json!({"type": "output_text", "text": text, "annotations": []}));
            }
            Block::ToolCall { id, name, input, .. } => {
                flush(&mut text_parts, items);
                items.push(json!({
                    "type": "function_call",
                    "call_id": id,
                    "name": name,
                    "arguments": input.to_string(),
                }));
            }
            Block::Opaque {
                provider: source,
                item,
            } if *source == provider.id && replayable(item) => {
                flush(&mut text_parts, items);
                items.push(item.clone());
            }
            _ => {}
        }
    }
    flush(&mut text_parts, items);
}

/// A reasoning item without encrypted content refers to server-side state that a stateless
/// request does not have, and the API rejects it.
fn replayable(item: &Value) -> bool {
    item.get("type").and_then(Value::as_str) != Some("reasoning")
        || item
            .get("encrypted_content")
            .and_then(Value::as_str)
            .is_some_and(|content| !content.is_empty())
}

/// A plain string, or the content-list form when the result carries images.
fn tool_output(content: &[Block]) -> Value {
    if content.iter().any(|block| matches!(block, Block::Image { .. })) {
        return Value::Array(content_parts(content));
    }
    let mut text = String::new();
    for block in content {
        if let Block::Text { text: piece } = block {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(piece);
        }
    }
    Value::String(text)
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Message,
    Reasoning,
    FunctionCall,
    /// Built-in tool calls and other items this harness does not use.
    Other,
}

impl Kind {
    fn of(item: &Value) -> Self {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => Kind::Message,
            Some("reasoning") => Kind::Reasoning,
            Some("function_call") => Kind::FunctionCall,
            _ => Kind::Other,
        }
    }
}

/// One output item of the response, keyed by its `output_index`.
struct Item {
    kind: Kind,
    /// Index of the item's text, thinking or tool-call block once it has appeared.
    index: Option<usize>,
    /// Text, reasoning or raw arguments streamed so far.
    buffer: String,
    call_id: String,
    name: String,
    /// Summary or content part the last reasoning delta belonged to.
    part: Option<u64>,
    done: bool,
}

impl Item {
    fn new(kind: Kind) -> Self {
        Item {
            kind,
            index: None,
            buffer: String::new(),
            call_id: String::new(),
            name: String::new(),
            part: None,
            done: false,
        }
    }

    /// The block made of what has streamed so far.
    fn streamed_block(&self) -> Option<Block> {
        match self.kind {
            Kind::Message => Some(Block::Text {
                text: self.buffer.clone(),
            }),
            Kind::Reasoning => Some(Block::Thinking {
                text: self.buffer.clone(),
                signature: None,
                redacted: false,
            }),
            Kind::FunctionCall => Some(Block::ToolCall {
                id: self.call_id.clone(),
                name: self.name.clone(),
                input: parse_arguments(&self.buffer),
                signature: None,
            }),
            Kind::Other => None,
        }
    }
}

struct ResponsesDecoder {
    provider: String,
    model: String,
    api: WireApi,
    started: bool,
    finished: bool,
    response_id: Option<String>,
    usage: Usage,
    /// Finished blocks by block index; `None` while a block is still streaming.
    blocks: Vec<Option<Block>>,
    items: BTreeMap<u64, Item>,
    /// The pages a hosted web search cited in the answer.
    sources: Sources,
}

fn allocate(blocks: &mut Vec<Option<Block>>) -> usize {
    blocks.push(None);
    blocks.len() - 1
}

fn text_field<'a>(value: &'a Value, name: &str) -> Option<&'a str> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|text| !text.is_empty())
}

fn output_index(payload: &Value) -> Option<u64> {
    payload.get("output_index").and_then(Value::as_u64)
}

/// Emits `ToolCallStart` for a function-call item.
fn open_call(
    blocks: &mut Vec<Option<Block>>,
    key: u64,
    item: &mut Item,
    json: &Value,
    out: &mut Vec<StreamEvent>,
) -> usize {
    item.call_id = match text_field(json, "call_id").or_else(|| text_field(json, "id")) {
        Some(id) => id.to_string(),
        None => format!("call_{key}"),
    };
    item.name = text_field(json, "name").unwrap_or_default().to_string();
    let index = allocate(blocks);
    item.index = Some(index);
    out.push(StreamEvent::ToolCallStart {
        index,
        id: item.call_id.clone(),
        name: item.name.clone(),
    });
    index
}

/// Text of the `output_text` and `refusal` parts of a message item.
fn message_text(item: &Value) -> String {
    let mut text = String::new();
    for part in item
        .get("content")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(piece) = text_field(part, "text").or_else(|| text_field(part, "refusal")) {
            text.push_str(piece);
        }
    }
    text
}

/// The summary of a reasoning item, or its raw reasoning text when the provider exposes that
/// instead. Parts are separate paragraphs.
fn reasoning_text(item: &Value) -> String {
    let paragraphs = |field: &str| {
        item.get(field)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|part| text_field(part, "text"))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    let summary = paragraphs("summary");
    if summary.is_empty() {
        paragraphs("content")
    } else {
        summary
    }
}

/// A `url_citation` annotation as `(url, title)`. xAI puts the citation's number where OpenAI
/// puts the page title; a number is no title.
fn url_citation(annotation: &Value) -> Option<(&str, &str)> {
    if annotation.get("type").and_then(Value::as_str) != Some("url_citation") {
        return None;
    }
    let url = text_field(annotation, "url")?;
    let title = text_field(annotation, "title")
        .filter(|title| title.parse::<u64>().is_err())
        .unwrap_or_default();
    Some((url, title))
}

/// What the answer says where it cites a page: the paragraph around the citation at
/// characters `at`, without the citation marks (`spans`, every annotation of the text) in it.
/// The annotation covers only its own mark (`([site](url))`, `[[1]](url)`), so the claim it
/// belongs to is the text beside it.
fn cited_paragraph(text: &[char], spans: &[(usize, usize)], at: (usize, usize)) -> String {
    let keep = |range: std::ops::Range<usize>| -> String {
        range
            .filter(|index| !spans.iter().any(|(from, to)| (*from..*to).contains(index)))
            .map(|index| text[index])
            .collect()
    };
    let line_start = |before: usize| {
        text[..before]
            .iter()
            .rposition(|c| *c == '\n')
            .map_or(0, |i| i + 1)
    };
    let (from, to) = (at.0.min(text.len()), at.1.min(text.len()));
    let start = line_start(from);
    let end = text[to..]
        .iter()
        .position(|c| *c == '\n')
        .map_or(text.len(), |i| to + i);
    let paragraph = keep(start..end);
    if !paragraph.trim().is_empty() {
        return paragraph;
    }
    // A citation on a line of its own belongs to the paragraph above it.
    let above = text[..start]
        .iter()
        .rposition(|c| !c.is_whitespace())
        .map_or(0, |i| i + 1);
    keep(line_start(above)..above)
}

fn parse_usage(usage: &Value) -> Usage {
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = count("/input_tokens_details/cached_tokens");
    let cache_write = count("/input_tokens_details/cache_write_tokens");
    Usage {
        input: count("/input_tokens")
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: count("/output_tokens"),
        cache_read,
        cache_write,
        reasoning: count("/output_tokens_details/reasoning_tokens"),
        cost_usd: None,
    }
}

/// The error of a `response.failed` event.
fn failure(response: Option<&Value>) -> AiError {
    let error = response
        .and_then(|response| response.get("error"))
        .filter(|error| !error.is_null());
    let mut info = match error {
        Some(error) => ErrorInfo::from_value(error),
        None => ErrorInfo::from_value(&json!("the provider reported a failed response")),
    };
    // OpenRouter keeps its precise error category beside the native error object.
    if let Some(category) = response.and_then(|response| text_field(response, "error_type")) {
        info.tag(category);
    }
    info.into_stream_error()
}

impl ResponsesDecoder {
    fn meta(&mut self, stop: StopReason) -> AssistantMeta {
        AssistantMeta {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: self.api,
            usage: self.usage,
            stop,
            response_id: self.response_id.clone(),
            duration_ms: 0,
            sources: self.sources.finish(),
        }
    }

    /// The pages the text parts of a finished message item cite, each with the paragraph of
    /// the answer that cites it.
    fn note_citations(&mut self, item: &Value) {
        let parts = item.get("content").and_then(Value::as_array);
        for part in parts.into_iter().flatten() {
            let Some(annotations) = part.get("annotations").and_then(Value::as_array) else {
                continue;
            };
            let text: Vec<char> = text_field(part, "text").unwrap_or_default().chars().collect();
            let span = |annotation: &Value| {
                let index = |name: &str| usize::try_from(annotation.get(name)?.as_u64()?).ok();
                Some((index("start_index")?, index("end_index")?)).filter(|(from, to)| from <= to)
            };
            let spans: Vec<(usize, usize)> = annotations.iter().filter_map(span).collect();
            for annotation in annotations {
                let Some((url, title)) = url_citation(annotation) else {
                    continue;
                };
                let paragraph = span(annotation).map(|at| cited_paragraph(&text, &spans, at));
                self.sources.cite(url, title, &paragraph.unwrap_or_default());
            }
        }
    }

    fn note_response(&mut self, response: Option<&Value>) {
        let Some(response) = response else {
            return;
        };
        if let Some(id) = text_field(response, "id") {
            self.response_id = Some(id.to_string());
        }
        if let Some(usage) = response.get("usage").filter(|usage| usage.is_object()) {
            self.usage = parse_usage(usage);
        }
    }

    fn end_block(&mut self, index: usize, block: Block, out: &mut Vec<StreamEvent>) {
        self.blocks[index] = Some(block.clone());
        out.push(StreamEvent::BlockEnd { index, block });
    }

    fn item_added(&mut self, key: u64, json: &Value, out: &mut Vec<StreamEvent>) {
        if self.items.contains_key(&key) {
            return;
        }
        let mut item = Item::new(Kind::of(json));
        if item.kind == Kind::FunctionCall {
            let index = open_call(&mut self.blocks, key, &mut item, json, out);
            if let Some(arguments) = text_field(json, "arguments") {
                item.buffer.push_str(arguments);
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    delta: arguments.to_string(),
                });
            }
        }
        self.items.insert(key, item);
    }

    fn text_delta(&mut self, payload: &Value, delta: &str, out: &mut Vec<StreamEvent>) {
        let Some(key) = output_index(payload).filter(|_| !delta.is_empty()) else {
            return;
        };
        let item = self.items.entry(key).or_insert_with(|| Item::new(Kind::Message));
        if item.kind != Kind::Message || item.done {
            return;
        }
        let index = *item.index.get_or_insert_with(|| allocate(&mut self.blocks));
        item.buffer.push_str(delta);
        out.push(StreamEvent::TextDelta {
            index,
            delta: delta.to_string(),
        });
    }

    /// `part_field` names the index that tells the parts of the reasoning apart.
    fn reasoning_delta(
        &mut self,
        payload: &Value,
        part_field: &str,
        delta: &str,
        out: &mut Vec<StreamEvent>,
    ) {
        let Some(key) = output_index(payload).filter(|_| !delta.is_empty()) else {
            return;
        };
        let item = self
            .items
            .entry(key)
            .or_insert_with(|| Item::new(Kind::Reasoning));
        if item.kind != Kind::Reasoning || item.done {
            return;
        }
        let index = *item.index.get_or_insert_with(|| allocate(&mut self.blocks));
        let part = payload.get(part_field).and_then(Value::as_u64);
        if item.part != part && !item.buffer.is_empty() {
            item.buffer.push_str("\n\n");
            out.push(StreamEvent::ThinkingDelta {
                index,
                delta: "\n\n".to_string(),
            });
        }
        item.part = part;
        item.buffer.push_str(delta);
        out.push(StreamEvent::ThinkingDelta {
            index,
            delta: delta.to_string(),
        });
    }

    fn arguments_delta(&mut self, payload: &Value, delta: &str, out: &mut Vec<StreamEvent>) {
        let Some(key) = output_index(payload) else {
            return;
        };
        let Some(item) = self.items.get_mut(&key) else {
            return;
        };
        if let Some(index) = item.index
            && item.kind == Kind::FunctionCall
            && !item.done
            && !delta.is_empty()
        {
            item.buffer.push_str(delta);
            out.push(StreamEvent::ToolCallDelta {
                index,
                delta: delta.to_string(),
            });
        }
    }

    /// `json` is the complete item, which is authoritative over what the deltas carried.
    fn item_done(&mut self, key: u64, json: &Value, out: &mut Vec<StreamEvent>) {
        let mut item = self
            .items
            .remove(&key)
            .unwrap_or_else(|| Item::new(Kind::of(json)));
        if !item.done {
            match item.kind {
                Kind::Message => {
                    self.note_citations(json);
                    let mut text = message_text(json);
                    if text.is_empty() {
                        text = std::mem::take(&mut item.buffer);
                    }
                    if !text.is_empty() {
                        let index = item.index.unwrap_or_else(|| allocate(&mut self.blocks));
                        self.end_block(index, Block::Text { text }, out);
                    }
                }
                Kind::Reasoning => {
                    let mut text = reasoning_text(json);
                    if text.is_empty() {
                        text = std::mem::take(&mut item.buffer);
                    }
                    if !text.is_empty() {
                        let index = item.index.unwrap_or_else(|| allocate(&mut self.blocks));
                        let block = Block::Thinking {
                            text,
                            signature: None,
                            redacted: false,
                        };
                        self.end_block(index, block, out);
                    }
                    // The item itself is what the next request must send back.
                    let index = allocate(&mut self.blocks);
                    let block = Block::Opaque {
                        provider: self.provider.clone(),
                        item: json.clone(),
                    };
                    self.end_block(index, block, out);
                }
                Kind::FunctionCall => {
                    let index = match item.index {
                        Some(index) => index,
                        None => open_call(&mut self.blocks, key, &mut item, json, out),
                    };
                    let arguments = text_field(json, "arguments").unwrap_or(&item.buffer);
                    let block = Block::ToolCall {
                        id: item.call_id.clone(),
                        name: item.name.clone(),
                        input: parse_arguments(arguments),
                        signature: None,
                    };
                    self.end_block(index, block, out);
                }
                Kind::Other => {}
            }
            item.done = true;
        }
        self.items.insert(key, item);
    }

    fn complete(&mut self, response: Option<&Value>, out: &mut Vec<StreamEvent>) {
        self.note_response(response);
        // xAI: every page its search tools opened, cited or not.
        let opened = response
            .and_then(|response| response.get("citations"))
            .and_then(Value::as_array);
        for url in opened.into_iter().flatten().filter_map(Value::as_str) {
            self.sources.found(url, "");
        }
        // Items the provider never closed end with what was streamed.
        for item in self.items.values().filter(|item| !item.done) {
            if let Some(index) = item.index
                && let Some(block) = item.streamed_block()
            {
                self.blocks[index] = Some(block.clone());
                out.push(StreamEvent::BlockEnd { index, block });
            }
        }
        let content: Vec<Block> = self.blocks.drain(..).flatten().collect();
        let cut_at_limit = response
            .and_then(|response| response.pointer("/incomplete_details/reason"))
            .and_then(Value::as_str)
            == Some("max_output_tokens");
        let stop = if cut_at_limit {
            StopReason::Length
        } else if content
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. }))
        {
            StopReason::ToolUse
        } else {
            StopReason::Stop
        };
        out.push(StreamEvent::Done {
            message: Message::assistant(content, self.meta(stop)),
        });
        self.finished = true;
    }

    /// `partial` keeps the finished blocks and the text streamed so far; a tool call whose
    /// arguments were cut short is left out.
    fn fail(&mut self, error: AiError, out: &mut Vec<StreamEvent>) {
        let mut blocks = std::mem::take(&mut self.blocks);
        for item in self.items.values().filter(|item| !item.done) {
            if let Some(index) = item.index
                && item.kind != Kind::FunctionCall
            {
                blocks[index] = item.streamed_block();
            }
        }
        let content: Vec<Block> = blocks.into_iter().flatten().collect();
        let partial =
            (!content.is_empty()).then(|| Message::assistant(content, self.meta(StopReason::Error)));
        out.push(StreamEvent::Error { error, partial });
        self.finished = true;
    }
}

impl Decoder for ResponsesDecoder {
    fn feed(&mut self, event: Option<&str>, data: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let payload: Value = match serde_json::from_str(data) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!("skipping malformed responses event: {error}");
                return out;
            }
        };
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .or(event)
            .unwrap_or_default();
        let delta = payload.get("delta").and_then(Value::as_str).unwrap_or_default();
        match kind {
            "response.created" | "response.queued" | "response.in_progress" => {
                self.note_response(payload.get("response"));
            }
            "response.output_item.added" => {
                if let (Some(key), Some(item)) = (output_index(&payload), payload.get("item")) {
                    self.item_added(key, item, &mut out);
                }
            }
            "response.output_text.delta" | "response.refusal.delta" => {
                self.text_delta(&payload, delta, &mut out);
            }
            "response.reasoning_summary_text.delta" => {
                self.reasoning_delta(&payload, "summary_index", delta, &mut out);
            }
            "response.reasoning_text.delta" => {
                self.reasoning_delta(&payload, "content_index", delta, &mut out);
            }
            "response.function_call_arguments.delta" => self.arguments_delta(&payload, delta, &mut out),
            // The finished item repeats the annotation with the text it sits in; this keeps the
            // page when the item never finishes.
            "response.output_text.annotation.added" => {
                if let Some((url, title)) = payload.get("annotation").and_then(url_citation) {
                    self.sources.cite(url, title, "");
                }
            }
            "response.output_item.done" => {
                if let (Some(key), Some(item)) = (output_index(&payload), payload.get("item")) {
                    self.item_done(key, item, &mut out);
                }
            }
            "response.completed" | "response.incomplete" => {
                self.complete(payload.get("response"), &mut out);
            }
            "response.failed" => {
                let response = payload.get("response");
                self.note_response(response);
                self.fail(failure(response), &mut out);
            }
            "error" => {
                let error = payload
                    .get("error")
                    .filter(|error| error.is_object())
                    .unwrap_or(&payload);
                self.fail(ErrorInfo::from_value(error).into_stream_error(), &mut out);
            }
            _ => {}
        }
        out
    }

    /// The API always ends a response with a terminal event, so a stream that just stops was cut.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
        self.fail(AiError::Network(STREAM_ENDED.to_string()), &mut out);
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    use super::*;

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        pairs
            .iter()
            .map(|(name, value)| {
                (
                    HeaderName::from_bytes(name.as_bytes()).unwrap(),
                    HeaderValue::from_str(value).unwrap(),
                )
            })
            .collect()
    }

    #[test]
    fn a_chatgpt_answer_reports_its_windows_in_headers() {
        let windows = limits(&headers(&[
            ("x-codex-primary-used-percent", "81.5"),
            ("x-codex-primary-window-minutes", "300"),
            ("x-codex-primary-reset-at", "1794506400"),
            ("x-codex-secondary-used-percent", "12"),
            ("x-codex-secondary-window-minutes", "10080"),
            ("x-codex-secondary-reset-after-seconds", "3600"),
            ("x-codex-plan-type", "plus"),
        ]));
        assert_eq!(windows.len(), 2);
        assert_eq!(
            windows[0],
            Window {
                name: "5h".to_string(),
                used: 0.815,
                resets_at: Some(1_794_506_400),
                status: "allowed".to_string(),
            }
        );
        assert_eq!((windows[1].name.as_str(), windows[1].used), ("7d", 0.12));
        let now = crate::credentials::now_ms() / 1000;
        assert!(
            windows[1]
                .resets_at
                .is_some_and(|at| (now + 3590..=now + 3610).contains(&at))
        );
        // The plain OpenAI API sends none of these.
        assert!(limits(&headers(&[("x-ratelimit-remaining-requests", "99")])).is_empty());
        // A window without its length is still told apart from the other.
        let bare = limits(&headers(&[("x-codex-secondary-used-percent", "3")]));
        assert_eq!(bare[0].name, "secondary");
    }
}
