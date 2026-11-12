//! OpenAI Chat Completions wire format: `POST {base_url}/chat/completions`.
//!
//! Most hosted and local model servers speak this format too, so requests stay within the common
//! subset and the decoder tolerates their deviations. Two hosts get what only they understand:
//! OpenRouter (API `openrouter`) a `reasoning` object and cache breakpoints for Anthropic models,
//! and GitHub Copilot the headers that say who started the turn.

use std::borrow::Cow;
use std::sync::atomic::{AtomicU64, Ordering};

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::catalog::{Cost, ModelInfo};
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{
    AssistantMeta, Block, INVALID_JSON_KEY, Message, Request, Role, StopReason, Usage, WireApi,
};
use crate::wire::thinking::{ThinkingParam, thinking_param};
use crate::wire::{Decoder, KeyHeader, add_headers, apply_extra_body};

pub(super) const STREAM_ENDED: &str = "stream ended before the response was complete";

const TOOL_IMAGES_LABEL: &str = "Images returned by the tool calls above:";
const IMAGE_ONLY_RESULT: &str = "(the result is the image attached below)";

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let openrouter = provider.api == WireApi::OpenRouter;
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    let mut messages = messages(req);
    if openrouter && model.id.starts_with("anthropic/") {
        mark_cache_breakpoint(&mut messages);
    }
    body.insert("messages".into(), Value::Array(messages));
    body.insert("stream".into(), json!(true));
    body.insert("stream_options".into(), json!({"include_usage": true}));
    if !req.tools.is_empty() {
        let tools = req
            .tools
            .iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.schema,
                    },
                })
            })
            .collect();
        body.insert("tools".into(), Value::Array(tools));
    }
    if let Some(limit) = req.max_output {
        body.insert("max_tokens".into(), json!(limit));
    }
    let effort = match thinking_param(provider.api, model, req.thinking) {
        ThinkingParam::Effort(effort) => Some(effort),
        ThinkingParam::Off | ThinkingParam::Budget(_) => None,
    };
    match effort {
        // OpenRouter turns its own object into whatever the model behind it takes.
        Some(effort) if openrouter => {
            body.insert("reasoning".into(), json!({"effort": effort}));
        }
        Some(effort) => {
            body.insert("reasoning_effort".into(), json!(effort));
        }
        None => {}
    }
    // Servers reject a sampling temperature on a request that asks for reasoning.
    if let Some(value) = req.temperature
        && effort.is_none()
    {
        body.insert("temperature".into(), temperature(value));
    }
    let mut body = Value::Object(body);
    apply_extra_body(&mut body, provider);
    body
}

/// Anthropic models cache a prompt only up to a marked block. One mark on the last text of the
/// conversation makes everything before it a cached prefix on the next request.
fn mark_cache_breakpoint(messages: &mut [Value]) {
    let mark = || json!({"type": "ephemeral"});
    for message in messages.iter_mut().rev() {
        let Some(content) = message.get_mut("content") else {
            continue;
        };
        match content {
            Value::String(text) if !text.is_empty() => {
                let text = std::mem::take(text);
                *content = json!([{"type": "text", "text": text, "cache_control": mark()}]);
                return;
            }
            Value::Array(parts) => {
                let last_text = parts
                    .iter_mut()
                    .rev()
                    .find(|part| part.get("type").and_then(Value::as_str) == Some("text"));
                if let Some(Value::Object(part)) = last_text {
                    part.insert("cache_control".into(), mark());
                    return;
                }
            }
            _ => {}
        }
    }
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    client
        .post(format!("{}/chat/completions", provider.base_url))
        .json(&build_body(provider, model, req))
        .headers(chat_headers(provider, req, auth))
}

/// [`request_headers`], plus what GitHub Copilot needs to know about the turn: whether a person
/// or the agent started it (which decides how it is billed), and whether it carries images.
pub(super) fn chat_headers(provider: &ProviderConfig, req: &Request, auth: &Auth) -> HeaderMap {
    let mut headers = request_headers(provider, auth);
    if provider.base_url.contains("githubcopilot.com") {
        let by_user = req.messages.last().is_none_or(|last| last.role == Role::User);
        let initiator = if by_user { "user" } else { "agent" };
        headers.insert(
            HeaderName::from_static("x-initiator"),
            HeaderValue::from_static(initiator),
        );
        let has_image = |blocks: &[Block]| {
            blocks.iter().any(|block| match block {
                Block::Image { .. } => true,
                Block::ToolResult { content, .. } => {
                    content.iter().any(|inner| matches!(inner, Block::Image { .. }))
                }
                _ => false,
            })
        };
        if req.messages.iter().any(|message| has_image(&message.content)) {
            headers.insert(
                HeaderName::from_static("copilot-vision-request"),
                HeaderValue::from_static("true"),
            );
        }
    }
    headers
}

pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder> {
    Box::new(ChatDecoder {
        provider: provider.id.clone(),
        model: model.id.clone(),
        api: provider.api,
        started: false,
        finished: false,
        response_id: None,
        usage: Usage::default(),
        finish_reason: None,
        entries: Vec::new(),
        open: None,
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    ErrorInfo::from_body(body).signal(Some(status))
}
/// The Codex CLI version the ChatGPT backend is told about when models are listed; it gates model
/// availability, and an old one hides newer models.
const CODEX_CLIENT_VERSION: &str = "0.159.0";

pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let query = if provider.base_url.contains("://chatgpt.com") {
        format!("?client_version={CODEX_CLIENT_VERSION}")
    } else {
        String::new()
    };
    client
        .get(format!("{}/models{query}", provider.base_url))
        .headers(request_headers(provider, auth))
}

/// Accepts `{"data": [...]}`, `{"models": [...]}` and a bare array. Entries without an `id` (the
/// ChatGPT backend names its models `slug`) are skipped.
pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    body.get("data")
        .or_else(|| body.get("models"))
        .unwrap_or(body)
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| model_info(provider, entry))
        .collect()
}

/// The credential, as a bearer token or in the `api-key` header the provider is configured for,
/// plus the provider's configured headers, which replace anything set here.
pub(super) fn request_headers(provider: &ProviderConfig, auth: &Auth) -> HeaderMap {
    let mut headers = HeaderMap::new();
    add_headers(&mut headers, provider, auth, KeyHeader::Bearer);
    headers
}

/// The non-blank system parts joined with a blank line.
pub(super) fn system_text(req: &Request) -> String {
    req.system
        .iter()
        .map(|part| part.text.as_str())
        .filter(|text| !text.trim().is_empty())
        .collect::<Vec<_>>()
        .join("\n\n")
}

pub(super) fn data_url(mime: &str, data: &str) -> String {
    format!("data:{mime};base64,{data}")
}

/// Widens to `f64` without the binary noise of the `f32` (`0.2` stays `0.2`).
pub(super) fn temperature(value: f32) -> Value {
    json!((f64::from(value) * 1000.0).round() / 1000.0)
}

/// Tool-call arguments as the tool input: blank text and `null` are `{}`; anything that is not a
/// JSON object is reported under [`INVALID_JSON_KEY`].
pub(super) fn parse_arguments(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Null) => json!({}),
        Ok(input @ Value::Object(_)) => input,
        _ => json!({ INVALID_JSON_KEY: raw }),
    }
}

fn non_empty(value: Option<&Value>) -> Option<&str> {
    value.and_then(Value::as_str).filter(|text| !text.is_empty())
}

// ---------------------------------------------------------------------------------------------
// Request mapping
// ---------------------------------------------------------------------------------------------

enum Part<'a> {
    Text(&'a str),
    Image(String),
}

/// One wire message before serialisation, so that adjacent turns with the same wire role can be
/// merged.
enum Turn<'a> {
    User(Vec<Part<'a>>),
    Assistant { text: String, tool_calls: Vec<Value> },
    Tool { call_id: &'a str, text: String },
}

fn messages(req: &Request) -> Vec<Value> {
    let mut turns: Vec<Turn<'_>> = Vec::new();
    for message in &req.messages {
        match message.role {
            Role::User => push_user(&mut turns, user_parts(&message.content)),
            Role::Assistant => push_assistant(&mut turns, &message.content),
            Role::Tool => push_tool_results(&mut turns, &message.content),
        }
    }
    let mut out = Vec::with_capacity(turns.len() + 1);
    let system = system_text(req);
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    out.extend(turns.into_iter().map(Turn::into_value));
    out
}

fn user_parts(content: &[Block]) -> Vec<Part<'_>> {
    content
        .iter()
        .filter_map(|block| match block {
            Block::Text { text } if !text.is_empty() => Some(Part::Text(text)),
            Block::Image { mime, data } => Some(Part::Image(data_url(mime, data))),
            _ => None,
        })
        .collect()
}

fn push_user<'a>(turns: &mut Vec<Turn<'a>>, parts: Vec<Part<'a>>) {
    if parts.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some(Turn::User(existing)) => existing.extend(parts),
        _ => turns.push(Turn::User(parts)),
    }
}

/// Text and tool calls only: reasoning and provider-native items are not part of this format.
fn push_assistant(turns: &mut Vec<Turn<'_>>, content: &[Block]) {
    let mut text = String::new();
    let mut tool_calls = Vec::new();
    for block in content {
        match block {
            Block::Text { text: piece } if !piece.is_empty() => append_paragraph(&mut text, piece),
            Block::ToolCall { id, name, input, .. } => tool_calls.push(json!({
                "id": id,
                "type": "function",
                "function": {"name": name, "arguments": input.to_string()},
            })),
            _ => {}
        }
    }
    if text.is_empty() && tool_calls.is_empty() {
        return;
    }
    match turns.last_mut() {
        Some(Turn::Assistant {
            text: existing,
            tool_calls: existing_calls,
        }) => {
            if !text.is_empty() {
                append_paragraph(existing, &text);
            }
            existing_calls.extend(tool_calls);
        }
        _ => turns.push(Turn::Assistant { text, tool_calls }),
    }
}

/// One `tool` message per result. A `tool` message carries text only, so images follow in a `user`
/// message.
fn push_tool_results<'a>(turns: &mut Vec<Turn<'a>>, content: &'a [Block]) {
    let mut images = Vec::new();
    for block in content {
        let Block::ToolResult {
            call_id,
            content: result,
            ..
        } = block
        else {
            continue;
        };
        let mut text = String::new();
        let mut has_image = false;
        for item in result {
            match item {
                Block::Text { text: piece } => {
                    if !text.is_empty() {
                        text.push('\n');
                    }
                    text.push_str(piece);
                }
                Block::Image { mime, data } => {
                    has_image = true;
                    images.push(Part::Image(data_url(mime, data)));
                }
                _ => {}
            }
        }
        if text.is_empty() && has_image {
            text.push_str(IMAGE_ONLY_RESULT);
        }
        turns.push(Turn::Tool { call_id, text });
    }
    if !images.is_empty() {
        images.insert(0, Part::Text(TOOL_IMAGES_LABEL));
        push_user(turns, images);
    }
}

fn append_paragraph(text: &mut String, piece: &str) {
    if !text.is_empty() {
        text.push_str("\n\n");
    }
    text.push_str(piece);
}

impl Turn<'_> {
    fn into_value(self) -> Value {
        match self {
            Turn::User(parts) => {
                // Plain string content is the form every compatible server accepts.
                let content = if parts.iter().all(|part| matches!(part, Part::Text(_))) {
                    let mut text = String::new();
                    for part in &parts {
                        if let Part::Text(piece) = part {
                            append_paragraph(&mut text, piece);
                        }
                    }
                    Value::String(text)
                } else {
                    parts
                        .into_iter()
                        .map(|part| match part {
                            Part::Text(text) => json!({"type": "text", "text": text}),
                            Part::Image(url) => json!({"type": "image_url", "image_url": {"url": url}}),
                        })
                        .collect()
                };
                json!({"role": "user", "content": content})
            }
            Turn::Assistant { text, tool_calls } => {
                let mut message = Map::new();
                message.insert("role".into(), json!("assistant"));
                message.insert(
                    "content".into(),
                    if text.is_empty() {
                        Value::Null
                    } else {
                        Value::String(text)
                    },
                );
                if !tool_calls.is_empty() {
                    message.insert("tool_calls".into(), Value::Array(tool_calls));
                }
                Value::Object(message)
            }
            Turn::Tool { call_id, text } => {
                json!({"role": "tool", "tool_call_id": call_id, "content": text})
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Model list
// ---------------------------------------------------------------------------------------------

fn model_info(provider: &ProviderConfig, entry: &Value) -> Option<ModelInfo> {
    // The ChatGPT backend marks models it does not want listed.
    if matches!(
        entry.get("visibility").and_then(Value::as_str),
        Some("hide" | "hidden")
    ) {
        return None;
    }
    let id = non_empty(entry.get("id")).or_else(|| non_empty(entry.get("slug")))?;
    let mut info = ModelInfo::unknown(&provider.id, id);
    info.name = ["name", "display_name"]
        .into_iter()
        .find_map(|field| non_empty(entry.get(field)))
        .filter(|name| *name != id)
        .map(str::to_string);
    let positive = |value: Option<&Value>| value.and_then(Value::as_u64).filter(|n| *n > 0);
    // The first of these places that holds a positive number. The names are the ones servers
    // are seen to use: OpenAI's and OpenRouter's, vLLM's `max_model_len`, GitHub Copilot's
    // `capabilities.limits`, the `limit` object of gateways that mirror models.dev, Venice's
    // `model_spec`, and the context a llama.cpp server runs its model with (before the one it
    // was trained with).
    let first = |pointers: &[&str]| pointers.iter().find_map(|at| positive(entry.pointer(at)));

    let window = first(&[
        "/context_length",
        "/context_window",
        "/max_context_length",
        "/max_model_len",
        "/top_provider/context_length",
        "/limit/context",
        "/capabilities/limits/max_context_window_tokens",
        "/model_spec/availableContextTokens",
        "/meta/n_ctx",
        "/meta/n_ctx_train",
    ]);
    if let Some(window) = window {
        info.context_window = window;
    }
    let limit = first(&[
        "/max_completion_tokens",
        "/max_output_tokens",
        "/max_output_length",
        "/max_tokens",
        "/top_provider/max_completion_tokens",
        "/limit/output",
        "/capabilities/limits/max_output_tokens",
        "/model_spec/maxCompletionTokens",
    ]);
    if let Some(limit) = limit {
        // A model cannot write more than its window holds, whatever its list says.
        let limit = limit.min(info.context_window);
        info.max_output = u32::try_from(limit).unwrap_or(u32::MAX);
    } else if entry.get("slug").is_some() {
        // The ChatGPT backend names no output limit; cap it at 128000.
        let cap = info.context_window.min(128_000);
        info.max_output = u32::try_from(cap).unwrap_or(u32::MAX);
    }

    let lists = |pointer: &str, wanted: &[&str]| {
        entry
            .pointer(pointer)
            .and_then(Value::as_array)
            .is_some_and(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .any(|item| wanted.contains(&item))
            })
    };
    // A capability is a `true`, or an object that says `supported: true`.
    let flagged = |pointer: &str| {
        entry.pointer(pointer).is_some_and(|flag| {
            flag.as_bool() == Some(true) || flag.get("supported").and_then(Value::as_bool) == Some(true)
        })
    };
    if lists("/architecture/input_modalities", &["image"])
        || lists("/input_modalities", &["image"])
        || lists("/modalities/input", &["image"])
        || lists("/capabilities", &["vision"])
        || flagged("/capabilities/vision")
        || flagged("/capabilities/supports/vision")
        || flagged("/model_spec/capabilities/supportsVision")
    {
        info.vision = true;
    }
    if lists(
        "/supported_parameters",
        &["reasoning", "reasoning_effort", "include_reasoning"],
    ) || lists("/capabilities", &["reasoning"])
        || flagged("/capabilities/reasoning")
        || flagged("/model_spec/capabilities/supportsReasoning")
        || reasoning_level(entry).is_some()
    {
        info.reasoning = true;
    }
    info.cost = entry.get("pricing").and_then(pricing);
    Some(info)
}

/// The reasoning effort a model is served at, when it names one other than `none` (the ChatGPT
/// backend's way of saying the model does not reason).
fn reasoning_level(entry: &Value) -> Option<&str> {
    let level = entry.get("default_reasoning_level").and_then(Value::as_str)?;
    (!level.eq_ignore_ascii_case("none")).then_some(level)
}

/// OpenRouter-style `pricing`: USD per token as decimal strings. Negative rates mark dynamic
/// pricing and yield no cost.
fn pricing(pricing: &Value) -> Option<Cost> {
    let per_million = |name: &str| -> Option<f64> {
        let value = pricing.get(name)?;
        let per_token = match value {
            Value::String(text) => text.trim().parse::<f64>().ok()?,
            other => other.as_f64()?,
        };
        if per_token.is_finite() && per_token >= 0.0 {
            Some((per_token * 1e12).round() / 1e6)
        } else {
            None
        }
    };
    Some(Cost {
        input: per_million("prompt")?,
        output: per_million("completion")?,
        cache_read: per_million("input_cache_read").unwrap_or(0.0),
        cache_write: per_million("input_cache_write").unwrap_or(0.0),
    })
}

// ---------------------------------------------------------------------------------------------
// Errors
// ---------------------------------------------------------------------------------------------

const OVERFLOW_PHRASES: [&str; 10] = [
    "context length",
    "context window",
    "context size",
    "context_length",
    "maximum prompt length",
    "prompt is too long",
    "prompt too long",
    "too many tokens",
    "exceeds the maximum number of tokens",
    "reduce the length of the messages",
];

/// Error codes that describe a request the provider will never accept.
const CLIENT_TAGS: [&str; 11] = [
    "invalid",
    "policy",
    "content_filter",
    "refusal",
    "unsupported",
    "image",
    "mismatch",
    "not_found",
    "unprocessable",
    "too_large",
    "too_long",
];

/// What an OpenAI-style error object says, reduced to the parts classification needs.
pub(super) struct ErrorInfo {
    /// Set when the provider reports its code as an HTTP status (OpenRouter).
    status: Option<u16>,
    /// Lower-cased `code`, `type` and `error_type` values, each followed by a space.
    tags: String,
    message: String,
}

impl ErrorInfo {
    /// `error` is an object with `code` / `type` / `message`, or a bare string.
    pub(super) fn from_value(error: &Value) -> Self {
        let mut info = ErrorInfo {
            status: None,
            tags: String::new(),
            message: String::new(),
        };
        match error {
            Value::Object(fields) => {
                let http_status =
                    |code: u64| u16::try_from(code).ok().filter(|code| (400..600).contains(code));
                match fields.get("code") {
                    Some(Value::Number(code)) => info.status = code.as_u64().and_then(http_status),
                    Some(Value::String(code)) => match code.parse::<u64>().ok().and_then(http_status) {
                        Some(status) => info.status = Some(status),
                        None => info.tag(code),
                    },
                    _ => {}
                }
                let typed = fields
                    .get("metadata")
                    .and_then(|metadata| metadata.get("error_type"));
                for tag in [fields.get("type"), fields.get("error_type"), typed] {
                    if let Some(tag) = tag.and_then(Value::as_str) {
                        info.tag(tag);
                    }
                }
                match non_empty(fields.get("message")) {
                    Some(message) => info.message = message.to_string(),
                    None => info.message = error.to_string(),
                }
            }
            Value::String(message) => info.message = message.clone(),
            other => info.message = other.to_string(),
        }
        info
    }

    /// An HTTP error body: `{"error": ...}`, a bare error object, or plain text.
    pub(super) fn from_body(body: &str) -> Self {
        match serde_json::from_str::<Value>(body) {
            Ok(value) => match value.get("error").filter(|error| !error.is_null()) {
                Some(error) => Self::from_value(error),
                None => Self::from_value(&value),
            },
            Err(_) => Self::from_value(&Value::String(body.trim().to_string())),
        }
    }

    pub(super) fn tag(&mut self, tag: &str) {
        self.tags.push_str(&tag.to_ascii_lowercase());
        self.tags.push(' ');
    }

    /// What the error says beyond its HTTP status (`None` for an in-band error): context overflow,
    /// overload, or an account that cannot pay. `None` when the status alone classifies it.
    pub(super) fn signal(&self, status: Option<u16>) -> Option<AiError> {
        let message = self.message.to_ascii_lowercase();
        let tagged = |needle: &str| self.tags.contains(needle);
        let status = status.or(self.status);

        let credential_problem = matches!(status, Some(401 | 403 | 429));
        if tagged("context_length_exceeded")
            || tagged("exceed_context_size")
            || (!credential_problem && OVERFLOW_PHRASES.iter().any(|phrase| message.contains(phrase)))
        {
            return Some(AiError::ContextOverflow);
        }
        if tagged("overloaded") || (status.is_none_or(|s| s >= 500) && message.contains("overloaded")) {
            return Some(AiError::Overloaded);
        }
        // Retrying cannot help until the account is topped up; `Auth` moves on to the next model.
        if status == Some(402)
            || tagged("insufficient_quota")
            || tagged("payment_required")
            || tagged("billing")
        {
            return Some(AiError::Auth(self.message.clone()));
        }
        None
    }

    /// The error for a failure reported inside a stream, where there is no HTTP status to map.
    pub(super) fn into_stream_error(self) -> AiError {
        if let Some(error) = self.signal(None) {
            return error;
        }
        let ErrorInfo {
            status,
            tags,
            message,
        } = self;
        if let Some(status) = status {
            return match status {
                401 | 403 => AiError::Auth(message),
                408 => AiError::Network(message),
                429 => AiError::RateLimit { retry_after: None },
                503 | 529 => AiError::Overloaded,
                500..=599 => AiError::Server {
                    status,
                    body: message,
                },
                _ => AiError::InvalidRequest(message),
            };
        }
        if tags.contains("rate_limit") {
            AiError::RateLimit { retry_after: None }
        } else if ["authentication", "api_key", "permission"]
            .iter()
            .any(|tag| tags.contains(tag))
        {
            AiError::Auth(message)
        } else if CLIENT_TAGS.iter().any(|tag| tags.contains(tag)) {
            AiError::InvalidRequest(message)
        } else {
            // The request was accepted before it failed, so an unrecognised failure is the server's.
            AiError::Server {
                status: 500,
                body: message,
            }
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(1);

/// Id for a tool call the server sent without one; unique for the life of the process.
pub(super) fn next_call_id() -> String {
    format!("call_{}", NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed))
}

struct PendingCall {
    /// `index` of the call in the provider's `tool_calls` array.
    wire_index: Option<u64>,
    wire_id: Option<String>,
    id: String,
    name: String,
    arguments: String,
    /// `ToolCallStart` has been emitted, which fixes `id`.
    announced: bool,
}

enum Slot {
    Text(String),
    Thinking(String),
    Call(PendingCall),
}

impl Slot {
    fn to_block(&self) -> Block {
        match self {
            Slot::Text(text) => Block::Text { text: text.clone() },
            Slot::Thinking(text) => Block::Thinking {
                text: text.clone(),
                signature: None,
                redacted: false,
            },
            Slot::Call(call) => Block::ToolCall {
                id: call.id.clone(),
                name: call.name.clone(),
                input: parse_arguments(&call.arguments),
                signature: None,
            },
        }
    }
}

/// A block of the response; its position in `ChatDecoder::entries` is the block index.
struct Entry {
    slot: Slot,
    /// Set once `BlockEnd` has been emitted.
    block: Option<Block>,
}

struct ChatDecoder {
    provider: String,
    model: String,
    api: WireApi,
    started: bool,
    finished: bool,
    response_id: Option<String>,
    usage: Usage,
    finish_reason: Option<String>,
    entries: Vec<Entry>,
    /// The text or thinking block still receiving deltas. The format has no block boundaries, so
    /// such a block ends when content of another kind arrives.
    open: Option<usize>,
}

fn announce(index: usize, call: &mut PendingCall, out: &mut Vec<StreamEvent>) {
    call.id = call.wire_id.clone().unwrap_or_else(next_call_id);
    call.announced = true;
    out.push(StreamEvent::ToolCallStart {
        index,
        id: call.id.clone(),
        name: call.name.clone(),
    });
    if !call.arguments.is_empty() {
        out.push(StreamEvent::ToolCallDelta {
            index,
            delta: call.arguments.clone(),
        });
    }
}

fn parse_usage(usage: &Value) -> Usage {
    let count = |pointer: &str| usage.pointer(pointer).and_then(Value::as_u64);
    let cache_read = count("/prompt_tokens_details/cached_tokens")
        .or_else(|| count("/prompt_cache_hit_tokens"))
        .unwrap_or(0);
    let cache_write = count("/prompt_tokens_details/cache_write_tokens").unwrap_or(0);
    Usage {
        input: count("/prompt_tokens")
            .unwrap_or(0)
            .saturating_sub(cache_read)
            .saturating_sub(cache_write),
        output: count("/completion_tokens").unwrap_or(0),
        cache_read,
        cache_write,
        reasoning: count("/completion_tokens_details/reasoning_tokens").unwrap_or(0),
        cost_usd: reported_cost(usage),
    }
}

/// What the response cost in USD, where a gateway bills per response and says so (`usage.cost`
/// on OpenRouter and the gateways modelled on it). With the user's own upstream key the gateway
/// reports only its fee there and the upstream charge beside it.
fn reported_cost(usage: &Value) -> Option<f64> {
    let amount = |pointer: &str| {
        usage
            .pointer(pointer)
            .and_then(Value::as_f64)
            .filter(|cost| cost.is_finite() && *cost >= 0.0)
    };
    let cost = amount("/cost")?;
    let upstream = match usage.get("is_byok").and_then(Value::as_bool) {
        Some(true) => amount("/cost_details/upstream_inference_cost").unwrap_or(0.0),
        _ => 0.0,
    };
    Some(cost + upstream)
}

impl ChatDecoder {
    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
    }

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

    fn chunk(&mut self, chunk: &Value, out: &mut Vec<StreamEvent>) {
        if self.response_id.is_none()
            && let Some(id) = non_empty(chunk.get("id"))
        {
            self.response_id = Some(id.to_string());
        }
        let usage = [chunk.get("usage"), chunk.pointer("/x_groq/usage")]
            .into_iter()
            .flatten()
            .find(|usage| usage.is_object());
        if let Some(usage) = usage {
            self.usage = parse_usage(usage);
        }
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            self.fail(ErrorInfo::from_value(error).into_stream_error(), out);
            return;
        }
        let Some(choice) = chunk.get("choices").and_then(|choices| choices.get(0)) else {
            return;
        };
        if let Some(delta) = choice.get("delta").or_else(|| choice.get("message")) {
            self.delta(delta, out);
        }
        match non_empty(choice.get("finish_reason")) {
            Some("error") => self.fail(
                AiError::Server {
                    status: 500,
                    body: "the provider ended the response with finish_reason `error`".to_string(),
                },
                out,
            ),
            Some(reason) => self.finish_reason = Some(reason.to_string()),
            None => {}
        }
    }

    fn delta(&mut self, delta: &Value, out: &mut Vec<StreamEvent>) {
        // Servers disagree on the field name, and some send both with the same text.
        let reasoning = ["reasoning_content", "reasoning"]
            .into_iter()
            .find_map(|field| non_empty(delta.get(field)));
        if let Some(text) = reasoning {
            self.text(true, text, out);
        }
        match delta.get("content") {
            Some(Value::String(text)) => self.text(false, text, out),
            // Mistral streams typed content parts when the model reasons.
            Some(Value::Array(parts)) => {
                for part in parts {
                    self.content_part(part, out);
                }
            }
            _ => {}
        }
        if let Some(refusal) = non_empty(delta.get("refusal")) {
            self.text(false, refusal, out);
        }
        for call in delta
            .get("tool_calls")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            self.tool_call(call, out);
        }
    }

    fn content_part(&mut self, part: &Value, out: &mut Vec<StreamEvent>) {
        if part.get("type").and_then(Value::as_str) == Some("thinking") {
            for piece in part
                .get("thinking")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                if let Some(text) = piece.get("text").and_then(Value::as_str) {
                    self.text(true, text, out);
                }
            }
        } else if let Some(text) = part.get("text").and_then(Value::as_str) {
            self.text(false, text, out);
        }
    }

    fn text(&mut self, thinking: bool, delta: &str, out: &mut Vec<StreamEvent>) {
        if delta.is_empty() {
            return;
        }
        let same_kind = |slot: &Slot| match slot {
            Slot::Thinking(_) => thinking,
            Slot::Text(_) => !thinking,
            Slot::Call(_) => false,
        };
        let index = match self.open.filter(|&index| same_kind(&self.entries[index].slot)) {
            Some(index) => index,
            None => {
                self.close_open(out);
                let slot = if thinking {
                    Slot::Thinking(String::new())
                } else {
                    Slot::Text(String::new())
                };
                self.entries.push(Entry { slot, block: None });
                let index = self.entries.len() - 1;
                self.open = Some(index);
                index
            }
        };
        if let Slot::Text(text) | Slot::Thinking(text) = &mut self.entries[index].slot {
            text.push_str(delta);
        }
        let delta = delta.to_string();
        out.push(if thinking {
            StreamEvent::ThinkingDelta { index, delta }
        } else {
            StreamEvent::TextDelta { index, delta }
        });
    }

    fn tool_call(&mut self, call: &Value, out: &mut Vec<StreamEvent>) {
        let wire_index = call.get("index").and_then(Value::as_u64);
        let wire_id = non_empty(call.get("id"));
        let function = call.get("function");
        let name = non_empty(function.and_then(|function| function.get("name")));
        let fragment: Cow<'_, str> = match function.and_then(|function| function.get("arguments")) {
            Some(Value::String(text)) => Cow::Borrowed(text.as_str()),
            // Some servers send the arguments as a JSON object instead of a string.
            Some(arguments @ Value::Object(_)) => Cow::Owned(arguments.to_string()),
            _ => Cow::Borrowed(""),
        };

        self.close_open(out);
        let index = self.call_entry(wire_index, wire_id, name.is_some());
        let Slot::Call(pending) = &mut self.entries[index].slot else {
            return;
        };
        if pending.name.is_empty()
            && let Some(name) = name
        {
            pending.name = name.to_string();
        }
        if !pending.announced && pending.wire_id.is_none() {
            pending.wire_id = wire_id.map(str::to_string);
        }
        pending.arguments.push_str(&fragment);
        if pending.announced {
            if !fragment.is_empty() {
                out.push(StreamEvent::ToolCallDelta {
                    index,
                    delta: fragment.into_owned(),
                });
            }
        } else if !pending.name.is_empty() {
            announce(index, pending, out);
        }
    }

    /// Entry of the call a `tool_calls` fragment belongs to, created on first appearance.
    fn call_entry(&mut self, wire_index: Option<u64>, wire_id: Option<&str>, named: bool) -> usize {
        let candidate = self.entries.iter().rposition(|entry| match &entry.slot {
            Slot::Call(call) => wire_index.is_none() || call.wire_index == wire_index,
            _ => false,
        });
        if let Some(index) = candidate
            && let Slot::Call(call) = &self.entries[index].slot
        {
            // Servers that send each call whole do not always number them: a different id, or a
            // second name on unnumbered fragments, starts another call.
            let other_id = matches!((wire_id, call.wire_id.as_deref()), (Some(new), Some(old)) if new != old);
            let other_name = wire_index.is_none() && wire_id.is_none() && named && !call.name.is_empty();
            if !other_id && !other_name {
                return index;
            }
        }
        self.entries.push(Entry {
            slot: Slot::Call(PendingCall {
                wire_index,
                wire_id: wire_id.map(str::to_string),
                id: String::new(),
                name: String::new(),
                arguments: String::new(),
                announced: false,
            }),
            block: None,
        });
        self.entries.len() - 1
    }

    fn close_open(&mut self, out: &mut Vec<StreamEvent>) {
        if let Some(index) = self.open.take() {
            self.close(index, out);
        }
    }

    fn close(&mut self, index: usize, out: &mut Vec<StreamEvent>) {
        let entry = &mut self.entries[index];
        let block = entry.slot.to_block();
        entry.block = Some(block.clone());
        out.push(StreamEvent::BlockEnd { index, block });
    }

    fn complete(&mut self, out: &mut Vec<StreamEvent>) {
        self.close_open(out);
        // Tool calls stay open until the end because their fragments may interleave.
        for index in 0..self.entries.len() {
            if self.entries[index].block.is_some() {
                continue;
            }
            if let Slot::Call(call) = &mut self.entries[index].slot
                && !call.announced
            {
                announce(index, call, out);
            }
            self.close(index, out);
        }
        let content: Vec<Block> = self.entries.drain(..).filter_map(|entry| entry.block).collect();
        let stop = if self.finish_reason.as_deref() == Some("length") {
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
        let content: Vec<Block> = self
            .entries
            .drain(..)
            .filter_map(|entry| match (entry.block, &entry.slot) {
                (Some(block), _) => Some(block),
                (None, Slot::Call(_)) => None,
                (None, slot) => Some(slot.to_block()),
            })
            .collect();
        let partial =
            (!content.is_empty()).then(|| Message::assistant(content, self.meta(StopReason::Error)));
        out.push(StreamEvent::Error { error, partial });
        self.open = None;
        self.finished = true;
    }
}

impl Decoder for ChatDecoder {
    fn feed(&mut self, _event: Option<&str>, data: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let data = data.trim();
        if data == "[DONE]" {
            self.start(&mut out);
            self.complete(&mut out);
            return out;
        }
        match serde_json::from_str::<Value>(data) {
            Ok(chunk) => {
                self.start(&mut out);
                self.chunk(&chunk, &mut out);
            }
            Err(error) => tracing::debug!("skipping malformed chat completion chunk: {error}"),
        }
        out
    }

    /// Some servers close the stream without `[DONE]`; a `finish_reason` shows the response was
    /// complete.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        if self.finish_reason.is_some() {
            self.complete(&mut out);
        } else {
            self.fail(AiError::Network(STREAM_ENDED.to_string()), &mut out);
        }
        out
    }
}
