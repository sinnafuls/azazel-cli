//! OpenAI Responses wire format: `POST {base_url}/responses`.
//!
//! Requests are stateless (`store: false`): the whole conversation is sent as input items, and
//! reasoning items come back with encrypted content so they can be replayed.

use std::collections::BTreeMap;

use serde_json::{Map, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{AssistantMeta, Block, Message, Request, Role, StopReason, Usage, WireApi};
use crate::wire::openai_chat::{
    self, ErrorInfo, STREAM_ENDED, data_url, parse_arguments, request_headers, service_tier, system_text,
    temperature,
};
use crate::wire::thinking::{ThinkingParam, thinking_param};
use crate::wire::{Decoder, apply_extra_body};

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
    if !req.tools.is_empty() {
        let tools = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "name": tool.name,
                    "description": tool.description,
                    "parameters": tool.schema,
                    "strict": false,
                })
            })
            .collect();
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
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    openai_chat::classify_error(status, body)
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

fn input_items(provider: &ProviderConfig, req: &Request) -> Vec<Value> {
    let mut items = Vec::new();
    for message in &req.messages {
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
    fn meta(&self, stop: StopReason) -> AssistantMeta {
        AssistantMeta {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: self.api,
            usage: self.usage,
            stop,
            response_id: self.response_id.clone(),
            duration_ms: 0,
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
