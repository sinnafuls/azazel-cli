//! Ollama's own chat API: `POST {root}/api/chat`, answered as one JSON object per line.
//!
//! Unlike the OpenAI-compatible endpoint the same server offers, this one takes a thinking
//! switch (`think`), returns reasoning apart from the answer, and sends each tool call whole,
//! with its arguments as an object.

use std::collections::HashMap;

use reqwest::header::HeaderMap;
use serde_json::{Map, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{
    AssistantMeta, Block, INVALID_JSON_KEY, Message, Request, Role, StopReason, ThinkingLevel, Usage, WireApi,
};
use crate::wire::openai_chat::{self, ErrorInfo, STREAM_ENDED, next_call_id, system_text, temperature};
use crate::wire::thinking::{ThinkingParam, thinking_param};
use crate::wire::{Decoder, KeyHeader, add_headers, apply_extra_body};

/// The most output tokens Ollama's hosted service accepts a request for.
const CLOUD_MAX_OUTPUT: u32 = 65_536;
const CLOUD_HOST: &str = "://ollama.com";

/// The server's address without the `/v1` of its OpenAI-compatible endpoint or the `/api` of this
/// one, so a provider configured for either reaches both.
pub fn root(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    trimmed
        .strip_suffix("/v1")
        .or_else(|| trimmed.strip_suffix("/api"))
        .unwrap_or(trimmed)
}

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let mut body = Map::new();
    body.insert("model".into(), json!(model.id));
    body.insert("messages".into(), Value::Array(messages(req)));
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
    match thinking_param(provider.api, model, req.thinking) {
        ThinkingParam::Effort(effort) => {
            body.insert("think".into(), json!(effort));
        }
        // A model that can think does so unless told not to.
        ThinkingParam::Off if model.reasoning && req.thinking == ThinkingLevel::Off => {
            body.insert("think".into(), json!(false));
        }
        ThinkingParam::Off | ThinkingParam::Budget(_) => {}
    }
    let mut options = Map::new();
    if let Some(limit) = req.max_output {
        let limit = match provider.base_url.contains(CLOUD_HOST) {
            true => limit.min(CLOUD_MAX_OUTPUT),
            false => limit,
        };
        options.insert("num_predict".into(), json!(limit));
    }
    if let Some(value) = req.temperature {
        options.insert("temperature".into(), temperature(value));
    }
    if !options.is_empty() {
        body.insert("options".into(), Value::Object(options));
    }
    body.insert("stream".into(), json!(true));
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
        .post(format!("{}/api/chat", root(&provider.base_url)))
        .json(&build_body(provider, model, req))
        .headers(headers(provider, auth))
}

pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder> {
    Box::new(OllamaDecoder {
        provider: provider.id.clone(),
        model: model.id.clone(),
        started: false,
        finished: false,
        blocks: Vec::new(),
        open: None,
        usage: Usage::default(),
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    openai_chat::classify_error(status, body)
}

/// The installed models, by name only: `GET {root}/api/tags`. What each can do is a second
/// request per model, made when the provider's discovery is `ollama`.
pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    client
        .get(format!("{}/api/tags", root(&provider.base_url)))
        .headers(headers(provider, auth))
}

pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    tag_names(body)
        .into_iter()
        .map(|id| ModelInfo::unknown(&provider.id, id))
        .collect()
}

/// The model ids in an `/api/tags` answer.
pub(crate) fn tag_names(body: &Value) -> Vec<&str> {
    body.get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            ["model", "name"]
                .into_iter()
                .find_map(|field| entry.get(field).and_then(Value::as_str))
                .filter(|id| !id.is_empty())
        })
        .collect()
}

pub(crate) fn headers(provider: &ProviderConfig, auth: &Auth) -> HeaderMap {
    let mut headers = HeaderMap::new();
    add_headers(&mut headers, provider, auth, KeyHeader::Bearer);
    headers
}

// ---------------------------------------------------------------------------------------------
// Request mapping
// ---------------------------------------------------------------------------------------------

fn messages(req: &Request) -> Vec<Value> {
    let mut out = Vec::with_capacity(req.messages.len() + 1);
    let system = system_text(req);
    if !system.is_empty() {
        out.push(json!({"role": "system", "content": system}));
    }
    // A tool result names the tool it answers, not the call, so remember which call was which.
    let mut tools: HashMap<&str, &str> = HashMap::new();
    let tail = req.tail_message();
    for message in req.messages.iter().chain(&tail) {
        match message.role {
            Role::User => out.extend(text_message("user", &message.content)),
            Role::Assistant => out.extend(assistant_message(&message.content, &mut tools)),
            Role::Tool => {
                for block in &message.content {
                    let Block::ToolResult { call_id, content, .. } = block else {
                        continue;
                    };
                    let mut result = text_message("tool", content)
                        .unwrap_or_else(|| json!({"role": "tool", "content": ""}));
                    if let Some(name) = tools.get(call_id.as_str()) {
                        result["tool_name"] = json!(name);
                    }
                    out.push(result);
                }
            }
        }
    }
    out
}

/// A message of text and images: the text parts joined, the images as bare base64. `None` when
/// there is neither.
fn text_message(role: &str, content: &[Block]) -> Option<Value> {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in content {
        match block {
            Block::Text { text: piece } if !piece.is_empty() => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(piece);
            }
            Block::Image { data, .. } => images.push(json!(data)),
            _ => {}
        }
    }
    if text.is_empty() && images.is_empty() {
        return None;
    }
    let mut message = json!({"role": role, "content": text});
    if !images.is_empty() {
        message["images"] = Value::Array(images);
    }
    Some(message)
}

/// Text and tool calls. Earlier reasoning is not sent back: the server does not want it.
fn assistant_message<'a>(content: &'a [Block], tools: &mut HashMap<&'a str, &'a str>) -> Option<Value> {
    let mut text = String::new();
    let mut calls = Vec::new();
    for block in content {
        match block {
            Block::Text { text: piece } if !piece.is_empty() => {
                if !text.is_empty() {
                    text.push_str("\n\n");
                }
                text.push_str(piece);
            }
            Block::ToolCall { id, name, input, .. } => {
                tools.insert(id, name);
                calls.push(json!({"type": "function", "function": {"name": name, "arguments": input}}));
            }
            _ => {}
        }
    }
    if text.is_empty() && calls.is_empty() {
        return None;
    }
    let mut message = json!({"role": "assistant", "content": text});
    if !calls.is_empty() {
        message["tool_calls"] = Value::Array(calls);
    }
    Some(message)
}

// ---------------------------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------------------------

struct OllamaDecoder {
    provider: String,
    model: String,
    started: bool,
    finished: bool,
    /// Finished blocks, in index order.
    blocks: Vec<Block>,
    /// The text or thinking still receiving deltas, as `(is thinking, text so far)`. Its index is
    /// `blocks.len()`.
    open: Option<(bool, String)>,
    usage: Usage,
}

impl OllamaDecoder {
    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        let Some((thinking, text)) = self.open.take() else {
            return;
        };
        let block = match thinking {
            true => Block::Thinking {
                text,
                signature: None,
                redacted: false,
            },
            false => Block::Text { text },
        };
        out.push(StreamEvent::BlockEnd {
            index: self.blocks.len(),
            block: block.clone(),
        });
        self.blocks.push(block);
    }

    fn text(&mut self, thinking: bool, delta: &str, out: &mut Vec<StreamEvent>) {
        if delta.is_empty() {
            return;
        }
        if self.open.as_ref().is_some_and(|(kind, _)| *kind != thinking) {
            self.close(out);
        }
        let (_, text) = self.open.get_or_insert_with(|| (thinking, String::new()));
        text.push_str(delta);
        let (index, delta) = (self.blocks.len(), delta.to_string());
        out.push(match thinking {
            true => StreamEvent::ThinkingDelta { index, delta },
            false => StreamEvent::TextDelta { index, delta },
        });
    }

    /// A call arrives whole. The server gives it no id, so one is made up.
    fn tool_call(&mut self, call: &Value, out: &mut Vec<StreamEvent>) {
        let Some(name) = call
            .pointer("/function/name")
            .and_then(Value::as_str)
            .filter(|name| !name.is_empty())
        else {
            return;
        };
        self.close(out);
        let input = match call.pointer("/function/arguments") {
            Some(arguments @ Value::Object(_)) => arguments.clone(),
            Some(Value::String(text)) => openai_chat::parse_arguments(text),
            None | Some(Value::Null) => json!({}),
            Some(other) => json!({ INVALID_JSON_KEY: other.to_string() }),
        };
        let index = self.blocks.len();
        let id = next_call_id();
        out.push(StreamEvent::ToolCallStart {
            index,
            id: id.clone(),
            name: name.to_string(),
        });
        out.push(StreamEvent::ToolCallDelta {
            index,
            delta: input.to_string(),
        });
        let block = Block::ToolCall {
            id,
            name: name.to_string(),
            input,
            signature: None,
        };
        out.push(StreamEvent::BlockEnd {
            index,
            block: block.clone(),
        });
        self.blocks.push(block);
    }

    fn fail(&mut self, error: AiError, out: &mut Vec<StreamEvent>) {
        self.close(out);
        self.finished = true;
        let partial = (!self.blocks.is_empty()).then(|| Message {
            role: Role::Assistant,
            content: std::mem::take(&mut self.blocks),
            meta: None,
        });
        out.push(StreamEvent::Error { error, partial });
    }

    fn done(&mut self, chunk: &Value, out: &mut Vec<StreamEvent>) {
        self.close(out);
        let count = |name: &str| chunk.get(name).and_then(Value::as_u64).unwrap_or(0);
        let cache_read = count("prompt_eval_cached_count");
        self.usage = Usage {
            input: count("prompt_eval_count").saturating_sub(cache_read),
            output: count("eval_count"),
            cache_read,
            ..Usage::default()
        };
        let called = self
            .blocks
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. }));
        let stop = match chunk.get("done_reason").and_then(Value::as_str) {
            // The server loaded the model and stopped: it had nothing from a user to answer.
            Some("load") => {
                let why = "Ollama loaded the model but generated nothing: the request has no user message";
                return self.fail(AiError::InvalidRequest(why.to_string()), out);
            }
            _ if called => StopReason::ToolUse,
            Some("length") => StopReason::Length,
            _ => StopReason::Stop,
        };
        self.finished = true;
        let meta = AssistantMeta {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: WireApi::OllamaChat,
            usage: self.usage,
            stop,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        };
        out.push(StreamEvent::Done {
            message: Message::assistant(std::mem::take(&mut self.blocks), meta),
        });
    }
}

impl Decoder for OllamaDecoder {
    fn feed(&mut self, _event: Option<&str>, data: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                tracing::debug!("skipping a malformed line: {e}");
                return out;
            }
        };
        self.start(&mut out);
        if let Some(error) = chunk.get("error").filter(|error| !error.is_null()) {
            self.fail(ErrorInfo::from_value(error).into_stream_error(), &mut out);
            return out;
        }
        if let Some(message) = chunk.get("message") {
            if let Some(thinking) = message.get("thinking").and_then(Value::as_str) {
                self.text(true, thinking, &mut out);
            }
            if let Some(content) = message.get("content").and_then(Value::as_str) {
                self.text(false, content, &mut out);
            }
            for call in message
                .get("tool_calls")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                self.tool_call(call, &mut out);
            }
        }
        if chunk.get("done").and_then(Value::as_bool) == Some(true) {
            self.done(&chunk, &mut out);
        }
        out
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if !self.finished {
            self.start(&mut out);
            self.fail(AiError::Network(STREAM_ENDED.to_string()), &mut out);
        }
        out
    }
}
