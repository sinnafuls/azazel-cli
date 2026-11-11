//! Anthropic Messages API: `POST {base_url}/v1/messages` with `stream: true`.
//!
//! Mapping decisions that follow the API reference for version `2026-10-21`:
//! - that version rejects `temperature`, so it is sent only when `provider.headers` pins an older
//!   `anthropic-version` and thinking is off;
//! - thinking is `{"type": "adaptive"}` steered by `output_config.effort`; only Claude models older
//!   than 4.6, and non-Claude models behind compatible endpoints, take the manual `budget_tokens` form;
//! - an assistant turn is replayed block for block in its original order, thinking blocks included.

use std::collections::HashMap;

use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{
    AssistantMeta, Block, INVALID_JSON_KEY, Message, Request, Role, StopReason, SystemPart, ThinkingLevel,
    ToolSpec, Usage, WireApi,
};
use crate::wire::Decoder;
use crate::wire::thinking::{ThinkingParam, thinking_param};

/// Latest stable API version. It removed the sampling parameters (`temperature`, `top_p`, `top_k`).
const API_VERSION: &str = "2026-10-21";
/// The client the subscription token was issued to; the API answers a login that does not name it
/// as if an unknown client had called.
const CLIENT_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const VERSION_HEADER: &str = "anthropic-version";
/// The API accepts at most this many `cache_control` markers in one request.
const MAX_CACHE_BREAKPOINTS: usize = 4;
/// Output tokens kept for the answer on top of a manual thinking budget.
const ANSWER_RESERVE: u32 = 1024;
/// Largest page the list-models endpoint serves.
const MODELS_PAGE_SIZE: u32 = 1000;

const USER: &str = "user";
const ASSISTANT: &str = "assistant";

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let mut breakpoints = MAX_CACHE_BREAKPOINTS;
    let system = system_blocks(&req.system, &mut breakpoints);
    let tools = tool_definitions(&req.tools, &mut breakpoints);
    let mut messages = wire_messages(&provider.id, &req.messages);
    for message in messages.iter_mut().rev().filter(|m| m.role == USER).take(2) {
        if let Some(block) = message.content.last_mut() {
            mark_cached(block, &mut breakpoints);
        }
    }

    let mut thinking = thinking_config(provider, model, req);
    if matches!(thinking, ThinkingConfig::Manual(_)) && open_turn_lacks_thinking(&messages) {
        thinking = ThinkingConfig::Off;
    }
    let mut max_tokens = req.max_output.unwrap_or(model.max_output);
    if let ThinkingConfig::Manual(budget) = thinking {
        // Thinking tokens count towards `max_tokens`, which must stay above the budget.
        max_tokens = max_tokens.max(budget.saturating_add(ANSWER_RESERVE));
    }

    let mut body = Map::new();
    body.insert("model".to_string(), json!(model.id));
    body.insert("max_tokens".to_string(), json!(max_tokens));
    body.insert("stream".to_string(), json!(true));
    if !system.is_empty() {
        body.insert("system".to_string(), Value::Array(system));
    }
    if !tools.is_empty() {
        body.insert("tools".to_string(), Value::Array(tools));
    }
    match thinking {
        ThinkingConfig::Off => {
            if let Some(temperature) = req.temperature.filter(|t| t.is_finite())
                && sampling_allowed(provider)
            {
                body.insert("temperature".to_string(), decimal(temperature));
            }
        }
        ThinkingConfig::Manual(budget) => {
            let config = json!({"type": "enabled", "budget_tokens": budget});
            body.insert("thinking".to_string(), config);
        }
        ThinkingConfig::Adaptive(effort) => {
            // Newer models hide the thinking text unless a summary is requested.
            let config = json!({"type": "adaptive", "display": "summarized"});
            body.insert("thinking".to_string(), config);
            body.insert("output_config".to_string(), json!({"effort": effort}));
        }
    }
    let messages = messages
        .into_iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect();
    body.insert("messages".to_string(), Value::Array(messages));
    Value::Object(body)
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let mut body = build_body(provider, model, req);
    if matches!(auth, Auth::OAuth { .. })
        && let Some(system) = body.get_mut("system").and_then(Value::as_array_mut)
    {
        // A subscription login must say which client is asking; the API rate limits a login that
        // does not identify as the CLI the token was issued to.
        system.insert(0, json!({"type": "text", "text": CLIENT_IDENTITY}));
    }
    client
        .post(format!("{}/v1/messages", provider.base_url))
        .headers(headers(provider, auth, true))
        .json(&body)
}

pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder> {
    Box::new(MessagesDecoder {
        provider: provider.id.clone(),
        model: model.id.clone(),
        started: false,
        finished: false,
        open: HashMap::new(),
        blocks: Vec::new(),
        usage: Usage::default(),
        stop_reason: None,
        response_id: None,
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    // 413 comes from the edge proxy, often without a JSON body: the request is too large to accept.
    if status == 413 {
        return Some(AiError::ContextOverflow);
    }
    let parsed = serde_json::from_str::<Value>(body).ok();
    let (kind, message) = parsed.as_ref().map_or(("", ""), error_fields);
    let message = if message.is_empty() { body } else { message };
    match classify_kind(kind, message)? {
        // The generic mapping handles these statuses and also reads `retry-after`.
        AiError::RateLimit { .. } if status == 429 => None,
        AiError::Auth(_) if matches!(status, 401 | 403) => None,
        error => Some(error),
    }
}

pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    client
        .get(format!(
            "{}/v1/models?limit={MODELS_PAGE_SIZE}",
            provider.base_url
        ))
        .headers(headers(provider, auth, false))
}

pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    let Some(data) = body.get("data").and_then(Value::as_array) else {
        return Vec::new();
    };
    if body.get("has_more").and_then(Value::as_bool) == Some(true) {
        tracing::debug!(provider = %provider.id, "model list has more pages than the first");
    }
    data.iter()
        .filter_map(|entry| model_info(provider, entry))
        .collect()
}

fn model_info(provider: &ProviderConfig, entry: &Value) -> Option<ModelInfo> {
    let id = entry
        .get("id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())?;
    if entry.get("lifecycle").and_then(Value::as_str) == Some("retired") {
        return None;
    }
    let mut info = ModelInfo::unknown(&provider.id, id);
    // The endpoint reports `null` or 0 for a limit it does not know.
    let limit = |key: &str| entry.get(key).and_then(Value::as_u64).filter(|n| *n > 0);
    if let Some(tokens) = limit("max_input_tokens") {
        info.context_window = tokens;
    }
    if let Some(tokens) = limit("max_tokens") {
        info.max_output = u32::try_from(tokens).unwrap_or(u32::MAX);
    }
    let supported = |capability: &str| {
        entry
            .get("capabilities")
            .and_then(|c| c.get(capability))
            .and_then(|c| c.get("supported"))
            .and_then(Value::as_bool)
    };
    if let Some(vision) = supported("image_input") {
        info.vision = vision;
    }
    if let Some(reasoning) = supported("thinking") {
        info.reasoning = reasoning;
    }
    Some(info)
}

/// Defaults first, then `provider.headers`, which replace a default of the same name.
fn headers(provider: &ProviderConfig, auth: &Auth, json_body: bool) -> HeaderMap {
    let mut map = HeaderMap::new();
    map.insert(
        HeaderName::from_static(VERSION_HEADER),
        HeaderValue::from_static(API_VERSION),
    );
    if json_body {
        map.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    }
    let secret = match auth {
        Auth::None => None,
        Auth::ApiKey(key) => Some((HeaderName::from_static("x-api-key"), key.clone())),
        Auth::OAuth { access, .. } => Some((AUTHORIZATION, format!("Bearer {access}"))),
    };
    if let Some((name, value)) = secret {
        match HeaderValue::from_str(&value) {
            Ok(mut value) => {
                value.set_sensitive(true);
                map.insert(name, value);
            }
            Err(_) => tracing::warn!(provider = %provider.id, "credential is not a valid header value"),
        }
    }
    for (name, value) in &provider.headers {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(value),
        ) {
            (Ok(name), Ok(value)) => {
                map.insert(name, value);
            }
            _ => tracing::warn!(provider = %provider.id, header = %name, "ignoring invalid header"),
        }
    }
    for (name, value) in auth.login_headers(provider) {
        match (
            HeaderName::from_bytes(name.as_bytes()),
            HeaderValue::from_str(&value),
        ) {
            (Ok(name), Ok(value)) => {
                map.insert(name, value);
            }
            _ => tracing::warn!(provider = %provider.id, header = %name, "ignoring invalid header"),
        }
    }
    map
}

/// Whether the request runs under an API version that still accepts `temperature`.
fn sampling_allowed(provider: &ProviderConfig) -> bool {
    // Versions are ISO dates, so string order is date order.
    provider
        .headers
        .iter()
        .any(|(name, value)| name.eq_ignore_ascii_case(VERSION_HEADER) && value.trim() < API_VERSION)
}

/// The shortest decimal that reads back as `value`, so 0.7 is sent as `0.7` and not as its `f64`
/// widening.
fn decimal(value: f32) -> Value {
    value
        .to_string()
        .parse::<f64>()
        .map_or_else(|_| Value::from(value), Value::from)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingConfig {
    Off,
    /// `{"type": "enabled", "budget_tokens": n}`.
    Manual(u32),
    /// `{"type": "adaptive"}` with this `output_config.effort`.
    Adaptive(&'static str),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ThinkingShape {
    Manual,
    Adaptive { xhigh: bool },
}

fn thinking_config(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> ThinkingConfig {
    let shape = thinking_shape(&model.id);
    match thinking_param(provider.api, model, req.thinking) {
        ThinkingParam::Off => ThinkingConfig::Off,
        ThinkingParam::Budget(budget) if shape == ThinkingShape::Manual => ThinkingConfig::Manual(budget),
        ThinkingParam::Budget(_) | ThinkingParam::Effort(_) => {
            let xhigh = shape == ThinkingShape::Adaptive { xhigh: true };
            ThinkingConfig::Adaptive(effort(req.thinking, xhigh))
        }
    }
}

/// Which thinking request a model takes, read from the version in its id. Ids follow
/// `claude-<line>-<major>-<minor>` or the older `claude-<major>-<minor>-<line>`, with an optional
/// vendor prefix and date suffix.
///
/// Claude up to 4.5 accepts only a manual budget and rejects `adaptive`; 4.7 and later reject the
/// manual form; 4.6 takes both but has no `xhigh` effort, and neither do Claude ids without a version.
/// Ids that do not name a Claude model belong to compatible endpoints, which implement the manual
/// form.
fn thinking_shape(model_id: &str) -> ThinkingShape {
    let id = model_id.to_ascii_lowercase();
    let Some(family) = id.find("claude") else {
        return ThinkingShape::Manual;
    };
    let mut parts = id[family..]
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(version_part)
        .skip_while(Option::is_none);
    let Some(Some(major)) = parts.next() else {
        return ThinkingShape::Adaptive { xhigh: false };
    };
    let minor = parts.next().flatten().unwrap_or(0);
    match (major, minor) {
        version if version < (4, 6) => ThinkingShape::Manual,
        (4, 6) => ThinkingShape::Adaptive { xhigh: false },
        _ => ThinkingShape::Adaptive { xhigh: true },
    }
}

/// A one- or two-digit number; longer digit runs in a model id are release dates.
fn version_part(token: &str) -> Option<u32> {
    token.parse().ok().filter(|_| token.len() <= 2)
}

fn effort(level: ThinkingLevel, xhigh: bool) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "low",
        ThinkingLevel::Medium => "medium",
        ThinkingLevel::High => "high",
        ThinkingLevel::Xhigh if xhigh => "xhigh",
        ThinkingLevel::Xhigh => "high",
        ThinkingLevel::Max => "max",
    }
}

/// Manual thinking requires the assistant turn in progress (the request ends with tool results) to
/// open with a thinking block. A turn started by another model has none, so thinking stays off until
/// the next user prompt.
fn open_turn_lacks_thinking(messages: &[WireMessage]) -> bool {
    if !messages.last().is_some_and(is_tool_turn) {
        return false;
    }
    let turn_start = messages
        .iter()
        .rposition(|m| m.role == USER && !is_tool_turn(m))
        .map_or(0, |prompt| prompt + 1);
    let opener = messages.iter().skip(turn_start).find(|m| m.role == ASSISTANT);
    let first = opener.and_then(|m| m.content.first()).and_then(block_type);
    opener.is_some() && !matches!(first, Some("thinking" | "redacted_thinking"))
}

/// A user turn that answers tool calls, so the assistant turn before it is still in progress.
fn is_tool_turn(m: &WireMessage) -> bool {
    m.role == USER && m.content.iter().any(|b| block_type(b) == Some("tool_result"))
}

fn block_type(block: &Value) -> Option<&str> {
    block.get("type").and_then(Value::as_str)
}

struct WireMessage {
    role: &'static str,
    content: Vec<Value>,
}

/// Converts messages, drops those left without content and joins neighbours that share a wire role.
fn wire_messages(provider_id: &str, messages: &[Message]) -> Vec<WireMessage> {
    let mut out: Vec<WireMessage> = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::Assistant => ASSISTANT,
            Role::User | Role::Tool => USER,
        };
        let content: Vec<Value> = message
            .content
            .iter()
            .filter_map(|block| wire_block(provider_id, role, block))
            .collect();
        if content.is_empty() {
            continue;
        }
        match out.last_mut() {
            Some(last) if last.role == role => last.content.extend(content),
            _ => out.push(WireMessage { role, content }),
        }
    }
    out
}

fn wire_block(provider_id: &str, role: &str, block: &Block) -> Option<Value> {
    match block {
        Block::Text { text } => text_block(text),
        Block::Image { mime, data } if role == USER => Some(image_block(mime, data)),
        Block::ToolResult {
            call_id,
            content,
            is_error,
        } if role == USER => Some(tool_result_block(call_id, content, *is_error)),
        Block::ToolCall { id, name, input, .. } if role == ASSISTANT => {
            Some(json!({"type": "tool_use", "id": id, "name": name, "input": input_object(input)}))
        }
        Block::Thinking {
            text,
            signature,
            redacted,
        } if role == ASSISTANT => {
            // The API verifies the signature; a block without one cannot be sent back.
            let signature = signature.as_deref().filter(|s| !s.is_empty())?;
            Some(if *redacted {
                json!({"type": "redacted_thinking", "data": signature})
            } else {
                json!({"type": "thinking", "thinking": text, "signature": signature})
            })
        }
        Block::Opaque { provider, item } if provider == provider_id && item.is_object() => Some(item.clone()),
        _ => None,
    }
}

/// `tool_use.input` has to be an object.
fn input_object(input: &Value) -> Value {
    match input {
        Value::Object(_) => input.clone(),
        _ => json!({}),
    }
}

/// The API rejects text blocks that are empty or only whitespace.
fn text_block(text: &str) -> Option<Value> {
    (!text.trim().is_empty()).then(|| json!({"type": "text", "text": text}))
}

fn image_block(mime: &str, data: &str) -> Value {
    json!({"type": "image", "source": {"type": "base64", "media_type": mime, "data": data}})
}

fn tool_result_block(call_id: &str, content: &[Block], is_error: bool) -> Value {
    let parts: Vec<Value> = content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } => text_block(text),
            Block::Image { mime, data } => Some(image_block(mime, data)),
            _ => None,
        })
        .collect();
    let mut result = Map::new();
    result.insert("type".to_string(), json!("tool_result"));
    result.insert("tool_use_id".to_string(), json!(call_id));
    if !parts.is_empty() {
        result.insert("content".to_string(), Value::Array(parts));
    }
    if is_error {
        result.insert("is_error".to_string(), json!(true));
    }
    Value::Object(result)
}

/// One text block per non-empty part; the cache marker goes on the last block at or before the last
/// part flagged `cache_boundary`.
fn system_blocks(parts: &[SystemPart], breakpoints: &mut usize) -> Vec<Value> {
    let mut blocks = Vec::new();
    let mut boundary = None;
    for part in parts {
        if let Some(block) = text_block(&part.text) {
            blocks.push(block);
        }
        if part.cache_boundary && !blocks.is_empty() {
            boundary = Some(blocks.len() - 1);
        }
    }
    if let Some(block) = boundary.and_then(|index| blocks.get_mut(index)) {
        mark_cached(block, breakpoints);
    }
    blocks
}

fn tool_definitions(tools: &[ToolSpec], breakpoints: &mut usize) -> Vec<Value> {
    let mut definitions: Vec<Value> = tools
        .iter()
        .map(|tool| {
            let schema = if tool.schema.is_object() {
                tool.schema.clone()
            } else {
                json!({"type": "object", "properties": {}})
            };
            json!({"name": tool.name, "description": tool.description, "input_schema": schema})
        })
        .collect();
    if let Some(last) = definitions.last_mut() {
        mark_cached(last, breakpoints);
    }
    definitions
}

fn mark_cached(block: &mut Value, breakpoints: &mut usize) {
    if *breakpoints == 0 {
        return;
    }
    if let Some(object) = block.as_object_mut() {
        object.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
        *breakpoints -= 1;
    }
}

/// `(type, message)` of an error envelope `{"type": "error", "error": {"type", "message"}}`.
fn error_fields(envelope: &Value) -> (&str, &str) {
    let field = |key: &str| {
        envelope
            .get("error")
            .and_then(|e| e.get(key))
            .and_then(Value::as_str)
            .unwrap_or_default()
    };
    (field("type"), field("message"))
}

/// What the error object alone says, whatever the HTTP status was.
fn classify_kind(kind: &str, message: &str) -> Option<AiError> {
    match kind {
        "overloaded_error" => Some(AiError::Overloaded),
        "rate_limit_error" => Some(AiError::RateLimit { retry_after: None }),
        // A billing failure is an account problem too: another model may still answer.
        "authentication_error" | "permission_error" | "billing_error" => {
            Some(AiError::Auth(message.to_string()))
        }
        "request_too_large" => Some(AiError::ContextOverflow),
        _ if is_context_overflow(message) => Some(AiError::ContextOverflow),
        // A subscription answers a request its short usage window refuses with a 400 that says the
        // extra usage is used up; the window resets on its own, so it is worth another attempt.
        _ if message.contains("out of extra usage") => Some(AiError::RateLimit { retry_after: None }),
        _ => None,
    }
}

fn is_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    ["prompt is too long", "exceed context limit", "input is too long"]
        .iter()
        .any(|needle| message.contains(needle))
}

/// An `error` event inside a 200 response.
fn stream_error(envelope: &Value, raw: &str) -> AiError {
    let (kind, message) = error_fields(envelope);
    let message = if message.is_empty() { raw } else { message };
    classify_kind(kind, message).unwrap_or_else(|| AiError::Server {
        status: 500,
        body: message.to_string(),
    })
}

struct MessagesDecoder {
    provider: String,
    model: String,
    started: bool,
    finished: bool,
    /// Blocks still receiving deltas, keyed by the API's content-block index.
    open: HashMap<u64, OpenBlock>,
    /// Finished blocks at their own index; `None` while a block is open.
    blocks: Vec<Option<Block>>,
    usage: Usage,
    stop_reason: Option<String>,
    response_id: Option<String>,
}

struct OpenBlock {
    index: usize,
    kind: OpenKind,
}

enum OpenKind {
    Text(String),
    Thinking {
        text: String,
        signature: String,
    },
    Redacted(String),
    Tool {
        id: String,
        name: String,
        json: String,
        /// `input` of the start payload, used when no argument fragment follows.
        input: Value,
    },
    /// A block type this client does not interpret (server tools and their results).
    Opaque {
        item: Map<String, Value>,
        json: String,
    },
}

impl MessagesDecoder {
    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
    }

    fn merge_usage(&mut self, usage: &Value) {
        let count = |key: &str| usage.get(key).and_then(Value::as_u64);
        if let Some(tokens) = count("input_tokens") {
            self.usage.input = tokens;
        }
        if let Some(tokens) = count("output_tokens") {
            self.usage.output = tokens;
        }
        if let Some(tokens) = count("cache_read_input_tokens") {
            self.usage.cache_read = tokens;
        }
        if let Some(tokens) = count("cache_creation_input_tokens") {
            self.usage.cache_write = tokens;
        }
        let thinking = usage
            .get("output_tokens_details")
            .and_then(|details| details.get("thinking_tokens"))
            .and_then(Value::as_u64);
        if let Some(tokens) = thinking {
            self.usage.reasoning = tokens;
        }
    }

    fn block_start(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        let start = payload.get("content_block").and_then(Value::as_object);
        let (Some(api_index), Some(start)) = (api_index(payload), start) else {
            tracing::debug!("ignoring content_block_start without an index or a block");
            return;
        };
        if let Some(previous) = self.open.remove(&api_index) {
            self.close(previous, out);
        }
        let index = self.blocks.len();
        self.blocks.push(None);
        let field = |key: &str| text_field(start, key);
        let kind = match start.get("type").and_then(Value::as_str).unwrap_or_default() {
            "text" => {
                let text = field("text");
                if !text.is_empty() {
                    let delta = text.clone();
                    out.push(StreamEvent::TextDelta { index, delta });
                }
                OpenKind::Text(text)
            }
            "thinking" => {
                let text = field("thinking");
                if !text.is_empty() {
                    let delta = text.clone();
                    out.push(StreamEvent::ThinkingDelta { index, delta });
                }
                OpenKind::Thinking {
                    text,
                    signature: field("signature"),
                }
            }
            "redacted_thinking" => OpenKind::Redacted(field("data")),
            "tool_use" => {
                let (id, name) = (field("id"), field("name"));
                out.push(StreamEvent::ToolCallStart {
                    index,
                    id: id.clone(),
                    name: name.clone(),
                });
                OpenKind::Tool {
                    id,
                    name,
                    json: String::new(),
                    input: start.get("input").cloned().unwrap_or(Value::Null),
                }
            }
            _ => OpenKind::Opaque {
                item: start.clone(),
                json: String::new(),
            },
        };
        self.open.insert(api_index, OpenBlock { index, kind });
    }

    fn block_delta(&mut self, payload: &Value, out: &mut Vec<StreamEvent>) {
        let open = api_index(payload).and_then(|api_index| self.open.get_mut(&api_index));
        let (Some(open), Some(delta)) = (open, payload.get("delta")) else {
            tracing::debug!("ignoring content_block_delta for a block that is not open");
            return;
        };
        let index = open.index;
        let fragment = |key: &str| delta.get(key).and_then(Value::as_str).unwrap_or_default();
        let delta_type = delta.get("type").and_then(Value::as_str).unwrap_or_default();
        match (delta_type, &mut open.kind) {
            ("text_delta", OpenKind::Text(text)) => {
                let delta = fragment("text");
                if !delta.is_empty() {
                    text.push_str(delta);
                    let delta = delta.to_string();
                    out.push(StreamEvent::TextDelta { index, delta });
                }
            }
            ("thinking_delta", OpenKind::Thinking { text, .. }) => {
                // With the thinking text omitted the API still sends one empty delta.
                let delta = fragment("thinking");
                if !delta.is_empty() {
                    text.push_str(delta);
                    let delta = delta.to_string();
                    out.push(StreamEvent::ThinkingDelta { index, delta });
                }
            }
            ("signature_delta", OpenKind::Thinking { signature, .. }) => {
                signature.push_str(fragment("signature"));
            }
            ("input_json_delta", OpenKind::Tool { json, .. }) => {
                let delta = fragment("partial_json");
                if !delta.is_empty() {
                    json.push_str(delta);
                    let delta = delta.to_string();
                    out.push(StreamEvent::ToolCallDelta { index, delta });
                }
            }
            ("input_json_delta", OpenKind::Opaque { json, .. }) => json.push_str(fragment("partial_json")),
            ("text_delta", OpenKind::Opaque { item, .. }) => {
                if let Some(Value::String(text)) = item.get_mut("text") {
                    text.push_str(fragment("text"));
                }
            }
            (other, _) => tracing::debug!(delta = other, "ignoring content block delta"),
        }
    }

    fn close(&mut self, open: OpenBlock, out: &mut Vec<StreamEvent>) {
        let block = match open.kind {
            OpenKind::Text(text) => Block::Text { text },
            OpenKind::Thinking { text, signature } => Block::Thinking {
                text,
                signature: Some(signature).filter(|s| !s.is_empty()),
                redacted: false,
            },
            OpenKind::Redacted(data) => Block::Thinking {
                text: String::new(),
                signature: Some(data),
                redacted: true,
            },
            OpenKind::Tool {
                id,
                name,
                json,
                input,
            } => Block::ToolCall {
                id,
                name,
                input: tool_input(&json, input),
                signature: None,
            },
            OpenKind::Opaque { mut item, json } => {
                if !json.trim().is_empty() {
                    match serde_json::from_str::<Value>(&json) {
                        Ok(input) => {
                            item.insert("input".to_string(), input);
                        }
                        Err(error) => tracing::debug!("server block input is not valid JSON: {error}"),
                    }
                }
                Block::Opaque {
                    provider: self.provider.clone(),
                    item: Value::Object(item),
                }
            }
        };
        if let Some(slot) = self.blocks.get_mut(open.index) {
            *slot = Some(block.clone());
        }
        out.push(StreamEvent::BlockEnd {
            index: open.index,
            block,
        });
    }

    fn close_all(&mut self, out: &mut Vec<StreamEvent>) {
        let mut open: Vec<OpenBlock> = self.open.drain().map(|(_, block)| block).collect();
        open.sort_by_key(|block| block.index);
        for block in open {
            self.close(block, out);
        }
    }

    fn meta(&self, stop: StopReason) -> AssistantMeta {
        AssistantMeta {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: WireApi::AnthropicMessages,
            usage: self.usage,
            stop,
            response_id: self.response_id.clone(),
            duration_ms: 0,
        }
    }

    fn done(&mut self) -> StreamEvent {
        self.finished = true;
        let content: Vec<Block> = self.blocks.drain(..).flatten().collect();
        let stop = if content.iter().any(|b| matches!(b, Block::ToolCall { .. })) {
            StopReason::ToolUse
        } else {
            match self.stop_reason.as_deref() {
                Some("max_tokens" | "model_context_window_exceeded") => StopReason::Length,
                _ => StopReason::Stop,
            }
        };
        StreamEvent::Done {
            message: Message::assistant(content, self.meta(stop)),
        }
    }

    fn fail(&mut self, error: AiError) -> StreamEvent {
        self.finished = true;
        let content: Vec<Block> = self.blocks.iter().flatten().cloned().collect();
        let meta = self.meta(StopReason::Error);
        let partial = (!content.is_empty()).then(|| Message::assistant(content, meta));
        StreamEvent::Error { error, partial }
    }
}

impl Decoder for MessagesDecoder {
    fn feed(&mut self, event: Option<&str>, data: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let payload: Value = match serde_json::from_str(data) {
            Ok(payload) => payload,
            Err(error) => {
                tracing::debug!("skipping malformed SSE data: {error}");
                return out;
            }
        };
        let kind = payload
            .get("type")
            .and_then(Value::as_str)
            .or(event)
            .unwrap_or_default();
        match kind {
            "message_start" => {
                self.start(&mut out);
                if let Some(message) = payload.get("message") {
                    self.response_id = message
                        .get("id")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                        .map(str::to_string);
                    if let Some(usage) = message.get("usage") {
                        self.merge_usage(usage);
                    }
                }
            }
            "content_block_start" => {
                self.start(&mut out);
                self.block_start(&payload, &mut out);
            }
            "content_block_delta" => self.block_delta(&payload, &mut out),
            "content_block_stop" => {
                if let Some(open) = api_index(&payload).and_then(|i| self.open.remove(&i)) {
                    self.close(open, &mut out);
                }
            }
            "message_delta" => {
                let stop_reason = payload
                    .get("delta")
                    .and_then(|delta| delta.get("stop_reason"))
                    .and_then(Value::as_str);
                if let Some(stop_reason) = stop_reason {
                    self.stop_reason = Some(stop_reason.to_string());
                }
                // Counts in this event are cumulative, so they replace earlier ones.
                if let Some(usage) = payload.get("usage") {
                    self.merge_usage(usage);
                }
            }
            "message_stop" => {
                self.start(&mut out);
                self.close_all(&mut out);
                out.push(self.done());
            }
            "error" => {
                self.start(&mut out);
                out.push(self.fail(stream_error(&payload, data)));
            }
            "ping" => {}
            other => tracing::debug!(event = other, "ignoring unknown stream event"),
        }
        out
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        // `message_delta` reports the stop reason once every block has closed, so a stream that ends
        // right after it lost nothing but the `message_stop` marker.
        if self.stop_reason.is_some() && self.open.is_empty() {
            out.push(self.done());
        } else {
            let error = AiError::Network("stream ended before the response was complete".to_string());
            out.push(self.fail(error));
        }
        out
    }
}

fn api_index(payload: &Value) -> Option<u64> {
    payload.get("index").and_then(Value::as_u64)
}

fn text_field(block: &Map<String, Value>, key: &str) -> String {
    let text = block.get(key).and_then(Value::as_str);
    text.unwrap_or_default().to_string()
}

/// Arguments of a finished tool call: the joined fragments, or the start payload's `input` when
/// there were none.
fn tool_input(fragments: &str, initial: Value) -> Value {
    if fragments.trim().is_empty() {
        return if initial.is_object() { initial } else { json!({}) };
    }
    serde_json::from_str(fragments).unwrap_or_else(|_| json!({ INVALID_JSON_KEY: fragments }))
}
