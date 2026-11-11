//! Google Generative Language API (Gemini): `models/{id}:streamGenerateContent?alt=sse`.
//!
//! The API has no block boundaries and no end-of-stream record: every SSE record is a
//! `GenerateContentResponse` chunk, and completion is a `finishReason` followed by the stream closing.

use std::borrow::Cow;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Number, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{AssistantMeta, Block, Message, Request, Role, StopReason, ThinkingLevel, Usage, WireApi};
use crate::wire::Decoder;
use crate::wire::thinking::{ThinkingParam, thinking_param};

/// Documented placeholder that makes the API skip signature validation for a function call it did
/// not produce itself (history from another model).
const SKIP_SIGNATURE: &str = "skip_thought_signature_validator";

/// MIME types the API accepts for images nested inside a function response.
const NESTED_IMAGE_MIMES: [&str; 3] = ["image/png", "image/jpeg", "image/webp"];

/// Source of tool-call ids for responses that carry neither a call id nor a response id.
static NEXT_CALL_ID: AtomicU64 = AtomicU64::new(0);

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let mut body = Map::new();

    let system: Vec<Value> = req
        .system
        .iter()
        .filter(|part| !part.text.is_empty())
        .map(|part| json!({ "text": part.text }))
        .collect();
    if !system.is_empty() {
        body.insert(
            "systemInstruction".into(),
            object([("parts", Value::Array(system))]),
        );
    }

    body.insert("contents".into(), Value::Array(contents(provider, model, req)));

    if !req.tools.is_empty() {
        let declarations: Vec<Value> = req
            .tools
            .iter()
            .map(|tool| {
                object([
                    ("name", tool.name.as_str().into()),
                    ("description", tool.description.as_str().into()),
                    ("parametersJsonSchema", parameters_schema(&tool.schema)),
                ])
            })
            .collect();
        let tools = object([("functionDeclarations", Value::Array(declarations))]);
        body.insert("tools".into(), Value::Array(vec![tools]));
    }

    let mut config = Map::new();
    if let Some(max_output) = req.max_output {
        config.insert("maxOutputTokens".into(), max_output.into());
    }
    if let Some(temperature) = req.temperature.and_then(f32_number) {
        config.insert("temperature".into(), Value::Number(temperature));
    }
    if let Some(thinking) = thinking_config(provider, model, req) {
        config.insert("thinkingConfig".into(), thinking);
    }
    if !config.is_empty() {
        body.insert("generationConfig".into(), Value::Object(config));
    }

    Value::Object(body)
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let id = model.id.strip_prefix("models/").unwrap_or(&model.id);
    let url = format!("{}/models/{id}:streamGenerateContent?alt=sse", provider.base_url);
    client
        .post(url)
        .headers(headers(provider, auth))
        .json(&build_body(provider, model, req))
}

pub fn decoder(provider: &ProviderConfig, model: &ModelInfo) -> Box<dyn Decoder> {
    Box::new(GoogleDecoder {
        provider: provider.id.clone(),
        model: model.id.clone(),
        started: false,
        finished: false,
        blocks: Vec::new(),
        open: None,
        tool_calls: 0,
        stop: None,
        usage: Usage::default(),
        response_id: None,
    })
}

pub fn classify_error(status: u16, body: &str) -> Option<AiError> {
    let body: Value = serde_json::from_str(body).ok()?;
    classify(status, error_object(&body)?)
}

pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    // 1000 is the largest page the endpoint returns.
    client
        .get(format!("{}/models?pageSize=1000", provider.base_url))
        .headers(headers(provider, auth))
}

pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    let Some(models) = body.get("models").and_then(Value::as_array) else {
        return Vec::new();
    };
    models
        .iter()
        .filter_map(|entry| {
            let name = entry.get("name").and_then(Value::as_str)?;
            let id = name.strip_prefix("models/").unwrap_or(name);
            let generates = entry
                .get("supportedGenerationMethods")
                .and_then(Value::as_array)?
                .iter()
                .any(|method| matches!(method.as_str(), Some("generateContent" | "streamGenerateContent")));
            if id.is_empty() || !generates {
                return None;
            }
            let mut info = ModelInfo::unknown(&provider.id, id);
            if let Some(limit) = entry.get("inputTokenLimit").and_then(Value::as_u64) {
                info.context_window = limit;
            }
            let output_limit = entry.get("outputTokenLimit").and_then(Value::as_u64);
            if let Some(limit) = output_limit.and_then(|limit| u32::try_from(limit).ok()) {
                info.max_output = limit;
            }
            if let Some(thinking) = entry.get("thinking").and_then(Value::as_bool) {
                info.reasoning = thinking;
            }
            Some(info)
        })
        .collect()
}

/// Credential header first, then the configured headers, which replace it on a name clash.
fn headers(provider: &ProviderConfig, auth: &Auth) -> HeaderMap {
    let mut map = HeaderMap::new();
    let credential = match auth {
        Auth::None => None,
        Auth::ApiKey(key) => Some(("x-goog-api-key", Cow::Borrowed(key.as_str()))),
        Auth::OAuth { access, .. } => Some(("authorization", Cow::Owned(format!("Bearer {access}")))),
    };
    if let Some((name, value)) = credential {
        match HeaderValue::from_str(&value) {
            Ok(mut value) => {
                value.set_sensitive(true);
                map.insert(name, value);
            }
            Err(_) => tracing::warn!(
                "provider `{}`: the credential cannot be sent as an HTTP header",
                provider.id
            ),
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
            _ => tracing::warn!("provider `{}`: ignoring invalid header `{name}`", provider.id),
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
            _ => tracing::warn!("provider `{}`: ignoring invalid header `{name}`", provider.id),
        }
    }
    map
}

/// Major version of a `gemini-<major>[.<minor>]-…` model id.
fn gemini_major(id: &str) -> Option<u32> {
    let name = id.rsplit('/').next().unwrap_or(id);
    let rest = name.strip_prefix("gemini-")?;
    let digits = rest.split(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse().ok()
}

/// Gemini 3 and later take a named thinking level instead of a token budget, validate thought
/// signatures on function calls and accept images nested inside a function response. Earlier and
/// unrecognised models get the older request shapes, which every model accepts or tolerates.
fn is_gemini_3_plus(model: &ModelInfo) -> bool {
    gemini_major(&model.id).is_some_and(|major| major >= 3)
}

fn thinking_config(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Option<Value> {
    let param = thinking_param(provider.api, model, req.thinking);
    if param == ThinkingParam::Off {
        return None;
    }
    let mut config = Map::new();
    if is_gemini_3_plus(model) {
        config.insert("thinkingLevel".into(), thinking_level(req.thinking).into());
    } else {
        let budget = match param {
            ThinkingParam::Budget(budget) => i64::from(clamp_budget(model, budget, req.max_output)),
            // No budget to translate: -1 lets the model size its own.
            ThinkingParam::Off | ThinkingParam::Effort(_) => -1,
        };
        config.insert("thinkingBudget".into(), budget.into());
    }
    config.insert("includeThoughts".into(), true.into());
    Some(Value::Object(config))
}

/// `MINIMAL` is rejected by several level-based models, so the lowest level sent is `LOW`, which all
/// of them accept.
fn thinking_level(level: ThinkingLevel) -> &'static str {
    match level {
        ThinkingLevel::Off | ThinkingLevel::Minimal | ThinkingLevel::Low => "LOW",
        ThinkingLevel::Medium => "MEDIUM",
        ThinkingLevel::High | ThinkingLevel::Xhigh | ThinkingLevel::Max => "HIGH",
    }
}

fn clamp_budget(model: &ModelInfo, budget: u32, max_output: Option<u32>) -> u32 {
    // `maxOutputTokens` counts thought tokens too, so keep room for the answer.
    let budget = max_output.map_or(budget, |max| budget.min(max.saturating_sub(1024)));
    if gemini_major(&model.id).is_none() {
        return budget;
    }
    // Documented ranges of the budget-based Gemini families; a value outside is rejected.
    let (min, max) = if model.id.contains("pro") {
        (128, 32_768)
    } else {
        (512, 24_576)
    };
    budget.clamp(min, max)
}

/// Shortest decimal form of the value (`0.7`, not the widened `0.699999988079071`).
fn f32_number(value: f32) -> Option<Number> {
    let widened = value.to_string().parse::<f64>().ok()?;
    Number::from_f64(widened)
}

/// The API takes JSON Schema as is; only the dialect marker, which describes nothing, is removed.
fn parameters_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => Value::Object(
            map.iter()
                .filter(|(key, _)| key.as_str() != "$schema")
                .map(|(key, value)| (key.clone(), value.clone()))
                .collect(),
        ),
        _ => json!({ "type": "object", "properties": {} }),
    }
}

/// A JSON object from values that are already built (`json!` would serialise, and so copy, them).
fn object<const N: usize>(entries: [(&str, Value); N]) -> Value {
    let mut map = Map::new();
    for (key, value) in entries {
        map.insert(key.to_string(), value);
    }
    Value::Object(map)
}

fn inline_data(mime: &str, data: &str) -> Value {
    json!({ "inlineData": { "mimeType": mime, "data": data } })
}

fn contents(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Vec<Value> {
    let modern = is_gemini_3_plus(model);
    let mut names: HashMap<&str, &str> = HashMap::new();
    let mut turns: Vec<(&'static str, Vec<Value>)> = Vec::new();

    for message in &req.messages {
        let role = match message.role {
            Role::Assistant => "model",
            Role::User | Role::Tool => "user",
        };
        // A thought signature is only valid for the model that issued it.
        let same_origin = message
            .meta
            .as_ref()
            .is_none_or(|meta| meta.provider == provider.id && meta.model == model.id);
        let mut parts = Vec::new();
        // Tool-result images that cannot be nested. They follow all function responses of the
        // message so that the responses to parallel calls stay adjacent.
        let mut loose_images = Vec::new();
        let mut first_call = true;

        for block in &message.content {
            match block {
                Block::Text { text } => {
                    if !text.is_empty() {
                        parts.push(json!({ "text": text }));
                    }
                }
                Block::Image { mime, data } => parts.push(inline_data(mime, data)),
                Block::ToolCall {
                    id,
                    name,
                    input,
                    signature,
                } => {
                    names.insert(id.as_str(), name.as_str());
                    let args = if input.is_object() {
                        input.clone()
                    } else {
                        json!({})
                    };
                    let call = object([
                        ("id", id.as_str().into()),
                        ("name", name.as_str().into()),
                        ("args", args),
                    ]);
                    let mut part = Map::new();
                    part.insert("functionCall".into(), call);
                    // Signature-validating models require one on the first call of each model turn.
                    let signature = signature
                        .as_deref()
                        .filter(|_| same_origin)
                        .or((modern && first_call).then_some(SKIP_SIGNATURE));
                    if let Some(signature) = signature {
                        part.insert("thoughtSignature".into(), signature.into());
                    }
                    first_call = false;
                    parts.push(Value::Object(part));
                }
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                } => {
                    let mut text = String::new();
                    let mut nested = Vec::new();
                    let mut has_image = false;
                    for item in content {
                        match item {
                            Block::Text { text: piece } => {
                                if !text.is_empty() {
                                    text.push('\n');
                                }
                                text.push_str(piece);
                            }
                            Block::Image { mime, data } => {
                                has_image = true;
                                let image = inline_data(mime, data);
                                if modern && NESTED_IMAGE_MIMES.contains(&mime.as_str()) {
                                    nested.push(image);
                                } else {
                                    loose_images.push(image);
                                }
                            }
                            _ => {}
                        }
                    }
                    if text.is_empty() && has_image {
                        text.push_str("see attached image");
                    }
                    let name = names.get(call_id.as_str()).copied().unwrap_or("unknown");
                    let key = if *is_error { "error" } else { "output" };
                    let payload = object([(key, text.into())]);
                    let mut response = Map::new();
                    response.insert("id".into(), call_id.as_str().into());
                    response.insert("name".into(), name.into());
                    response.insert("response".into(), payload);
                    if !nested.is_empty() {
                        response.insert("parts".into(), Value::Array(nested));
                    }
                    parts.push(object([("functionResponse", Value::Object(response))]));
                }
                Block::Thinking { .. } | Block::Opaque { .. } => {}
            }
        }

        parts.append(&mut loose_images);
        if parts.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last_role, last_parts)) if *last_role == role => last_parts.append(&mut parts),
            _ => turns.push((role, parts)),
        }
    }

    turns
        .into_iter()
        .map(|(role, parts)| object([("role", role.into()), ("parts", Value::Array(parts))]))
        .collect()
}

/// The `error` object of an error body. A non-SSE streaming error body is a one-element array.
fn error_object(body: &Value) -> Option<&Value> {
    let body = match body {
        Value::Array(items) => items.first()?,
        other => other,
    };
    body.get("error").filter(|error| error.is_object())
}

fn str_field<'a>(value: &'a Value, key: &str) -> &'a str {
    value.get(key).and_then(Value::as_str).unwrap_or_default()
}

/// Signals in a Google `error` object that the HTTP status alone does not carry.
fn classify(status: u16, error: &Value) -> Option<AiError> {
    let message = str_field(error, "message");
    let grpc = str_field(error, "status");
    let details = error
        .get("details")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    // Quota errors name token counts too, so they are excluded from the overflow check.
    if grpc != "RESOURCE_EXHAUSTED" && status != 429 && is_context_overflow(message) {
        return Some(AiError::ContextOverflow);
    }
    // A rejected API key is reported as 400 INVALID_ARGUMENT with an `API_KEY_*` reason.
    let rejected_key = details
        .iter()
        .any(|detail| str_field(detail, "reason").starts_with("API_KEY_"));
    if rejected_key || matches!(grpc, "UNAUTHENTICATED" | "PERMISSION_DENIED") {
        return Some(AiError::Auth(message.to_string()));
    }
    match grpc {
        "UNAVAILABLE" => Some(AiError::Overloaded),
        // Only 429 is a rate limit: 402 RESOURCE_EXHAUSTED is a depleted balance.
        "RESOURCE_EXHAUSTED" if status == 429 => {
            let retry_after = Some(retry_delay(details)?);
            Some(AiError::RateLimit { retry_after })
        }
        _ => message
            .to_ascii_lowercase()
            .contains("overloaded")
            .then_some(AiError::Overloaded),
    }
}

fn is_context_overflow(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "exceeds the maximum number of tokens",
        "exceeds the maximum allowed number of tokens",
        "context window",
        "context length",
        "prompt is too long",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// `retryDelay` of a `google.rpc.RetryInfo` detail, formatted as decimal seconds (`"32s"`).
fn retry_delay(details: &[Value]) -> Option<Duration> {
    details.iter().find_map(|detail| {
        let delay = detail.get("retryDelay").and_then(Value::as_str)?;
        let seconds = delay.strip_suffix('s')?.parse::<f64>().ok()?;
        Duration::try_from_secs_f64(seconds).ok()
    })
}

/// Classification of an `error` object that arrives inside the stream.
fn stream_error(error: &Value) -> AiError {
    let code = error.get("code").and_then(Value::as_u64);
    let status =
        code.and_then(|code| u16::try_from(code).ok())
            .unwrap_or_else(|| match str_field(error, "status") {
                "INVALID_ARGUMENT" | "FAILED_PRECONDITION" => 400,
                "UNAUTHENTICATED" => 401,
                "PERMISSION_DENIED" => 403,
                "NOT_FOUND" => 404,
                "RESOURCE_EXHAUSTED" => 429,
                "UNAVAILABLE" => 503,
                "DEADLINE_EXCEEDED" => 504,
                _ => 500,
            });
    classify(status, error).unwrap_or_else(|| {
        let body = str_field(error, "message").to_string();
        match status {
            401 | 403 => AiError::Auth(body),
            429 => AiError::RateLimit { retry_after: None },
            503 | 529 => AiError::Overloaded,
            _ => AiError::Server { status, body },
        }
    })
}

fn usage_from(metadata: &Value) -> Usage {
    let count = |key: &str| metadata.get(key).and_then(Value::as_u64).unwrap_or(0);
    let cache_read = count("cachedContentTokenCount");
    let reasoning = count("thoughtsTokenCount");
    Usage {
        input: count("promptTokenCount").saturating_sub(cache_read),
        output: count("candidatesTokenCount") + reasoning,
        cache_read,
        cache_write: 0,
        reasoning,
        cost_usd: None,
    }
}

/// A text or thought block that later parts may still extend.
enum Open {
    Text(String),
    Thinking { text: String, signature: Option<String> },
}

struct GoogleDecoder {
    provider: String,
    model: String,
    started: bool,
    finished: bool,
    blocks: Vec<Block>,
    open: Option<Open>,
    tool_calls: usize,
    /// Set once the candidate reported a finish reason that is not a block.
    stop: Option<StopReason>,
    usage: Usage,
    response_id: Option<String>,
}

impl GoogleDecoder {
    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
    }

    fn close(&mut self, out: &mut Vec<StreamEvent>) {
        let block = match self.open.take() {
            Some(Open::Text(text)) => Block::Text { text },
            Some(Open::Thinking { text, signature }) => Block::Thinking {
                text,
                signature,
                redacted: false,
            },
            None => return,
        };
        out.push(StreamEvent::BlockEnd {
            index: self.blocks.len(),
            block: block.clone(),
        });
        self.blocks.push(block);
    }

    fn part(&mut self, part: Value, out: &mut Vec<StreamEvent>) {
        let Value::Object(mut part) = part else {
            return;
        };
        let signature = match part.remove("thoughtSignature") {
            Some(Value::String(signature)) if !signature.is_empty() => Some(signature),
            _ => None,
        };

        if let Some(Value::Object(mut call)) = part.remove("functionCall") {
            self.close(out);
            let index = self.blocks.len();
            let name = match call.remove("name") {
                Some(Value::String(name)) => name,
                _ => String::new(),
            };
            let id = match call.remove("id") {
                Some(Value::String(id)) if !id.is_empty() => id,
                _ => match &self.response_id {
                    Some(response) => format!("call_{response}_{}", self.tool_calls),
                    None => format!("call_{}", NEXT_CALL_ID.fetch_add(1, Ordering::Relaxed)),
                },
            };
            self.tool_calls += 1;
            let input = match call.remove("args") {
                Some(args @ Value::Object(_)) => args,
                _ => json!({}),
            };
            out.push(StreamEvent::ToolCallStart {
                index,
                id: id.clone(),
                name: name.clone(),
            });
            out.push(StreamEvent::ToolCallDelta {
                index,
                delta: input.to_string(),
            });
            let block = Block::ToolCall {
                id,
                name,
                input,
                signature,
            };
            out.push(StreamEvent::BlockEnd {
                index,
                block: block.clone(),
            });
            self.blocks.push(block);
            return;
        }

        let Some(Value::String(text)) = part.remove("text") else {
            return;
        };
        if part.get("thought") == Some(&Value::Bool(true)) {
            if !matches!(self.open, Some(Open::Thinking { .. })) {
                if text.is_empty() {
                    return;
                }
                self.close(out);
                self.open = Some(Open::Thinking {
                    text: String::new(),
                    signature: None,
                });
            }
            let index = self.blocks.len();
            if let Some(Open::Thinking {
                text: thinking,
                signature: current,
            }) = &mut self.open
            {
                thinking.push_str(&text);
                if signature.is_some() {
                    *current = signature;
                }
            }
            if !text.is_empty() {
                out.push(StreamEvent::ThinkingDelta { index, delta: text });
            }
        } else {
            // The last chunk often carries an empty text part, sometimes with only a signature.
            if text.is_empty() {
                return;
            }
            if !matches!(self.open, Some(Open::Text(_))) {
                self.close(out);
                self.open = Some(Open::Text(String::new()));
            }
            let index = self.blocks.len();
            if let Some(Open::Text(answer)) = &mut self.open {
                answer.push_str(&text);
            }
            out.push(StreamEvent::TextDelta { index, delta: text });
        }
    }

    /// The final (or partial) message. Leaves the decoder empty, so call it once.
    fn message(&mut self, stop: StopReason) -> Message {
        Message::assistant(
            std::mem::take(&mut self.blocks),
            AssistantMeta {
                provider: std::mem::take(&mut self.provider),
                model: std::mem::take(&mut self.model),
                api: WireApi::GoogleGenerative,
                usage: self.usage,
                stop,
                response_id: self.response_id.take(),
                duration_ms: 0,
            },
        )
    }

    fn fail(&mut self, error: AiError, out: &mut Vec<StreamEvent>) {
        self.close(out);
        self.finished = true;
        let partial = if self.blocks.is_empty() {
            None
        } else {
            Some(self.message(StopReason::Error))
        };
        out.push(StreamEvent::Error { error, partial });
    }
}

impl Decoder for GoogleDecoder {
    fn feed(&mut self, _event: Option<&str>, data: &str) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        let mut chunk: Value = match serde_json::from_str(data) {
            Ok(chunk) => chunk,
            Err(e) => {
                tracing::debug!("skipping malformed SSE data: {e}");
                return out;
            }
        };
        self.start(&mut out);

        if let Some(error) = error_object(&chunk) {
            let error = stream_error(error);
            self.fail(error, &mut out);
            return out;
        }

        if self.response_id.is_none() {
            self.response_id = match chunk.get_mut("responseId").map(Value::take) {
                Some(Value::String(id)) if !id.is_empty() => Some(id),
                _ => None,
            };
        }
        if let Some(metadata) = chunk.get("usageMetadata") {
            self.usage = usage_from(metadata);
        }
        let parts = chunk.pointer_mut("/candidates/0/content/parts").map(Value::take);
        if let Some(Value::Array(parts)) = parts {
            for part in parts {
                self.part(part, &mut out);
            }
        }

        let prompt_block = chunk
            .pointer("/promptFeedback/blockReason")
            .and_then(Value::as_str)
            .filter(|reason| *reason != "BLOCK_REASON_UNSPECIFIED");
        let finish = chunk
            .pointer("/candidates/0/finishReason")
            .and_then(Value::as_str)
            .filter(|reason| *reason != "FINISH_REASON_UNSPECIFIED");
        let blocked = match (prompt_block, finish) {
            (Some(reason), _) => Some(reason),
            (None, Some("STOP")) => {
                self.stop = Some(StopReason::Stop);
                None
            }
            // `CONTINUATION` is the per-request output cap of long-decode models.
            (None, Some("MAX_TOKENS" | "CONTINUATION")) => {
                self.stop = Some(StopReason::Length);
                None
            }
            (None, other) => other,
        };
        if let Some(reason) = blocked {
            let error = AiError::InvalidRequest(format!("response blocked: {reason}"));
            self.fail(error, &mut out);
        }
        out
    }

    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        match self.stop {
            Some(stop) => {
                self.close(&mut out);
                self.finished = true;
                let stop = if self.tool_calls > 0 {
                    StopReason::ToolUse
                } else {
                    stop
                };
                let message = self.message(stop);
                out.push(StreamEvent::Done { message });
            }
            None => {
                let error = AiError::Network("stream ended before the response was complete".into());
                self.fail(error, &mut out);
            }
        }
        out
    }
}
