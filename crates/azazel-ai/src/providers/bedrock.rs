//! Amazon Bedrock through the Converse API (`converse_stream`), with the region and credentials of
//! the AWS default chain.
//!
//! Thinking is requested for Claude models only, as the additional request fields the Anthropic
//! API takes; other model families run without it.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::time::Instant;

use async_trait::async_trait;
use aws_sdk_bedrockruntime::error::{ProvideErrorMetadata, SdkError};
use aws_sdk_bedrockruntime::primitives::Blob;
use aws_sdk_bedrockruntime::types::{
    self as sdk, CachePointBlock, CachePointType, ContentBlock, ContentBlockDelta, ContentBlockStart,
    ConversationRole, ConverseStreamOutput, ImageBlock, ImageFormat, ImageSource, InferenceConfiguration,
    ReasoningContentBlock, ReasoningContentBlockDelta, ReasoningTextBlock, SystemContentBlock, Tool,
    ToolConfiguration, ToolInputSchema, ToolResultBlock, ToolResultContentBlock, ToolResultStatus,
    ToolSpecification, ToolUseBlock,
};
use aws_smithy_types::{Document, Number};
use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tokio::sync::OnceCell;
use tokio_util::sync::CancellationToken;

use crate::catalog::{Cost, ModelInfo};
use crate::error::AiError;
use crate::provider::Provider;
use crate::providers::http::prepare;
use crate::providers::presets::{BEDROCK, ProviderConfig};
use crate::stream::{EventStream, StreamEvent};
use crate::types::{
    AssistantMeta, Block, INVALID_JSON_KEY, Message, Request, Role, StopReason, Usage, WireApi,
};
use crate::wire;
use crate::wire::normalize::Target;

const STREAM_ENDED: &str = "stream ended before the response was complete";
const EMPTY_RESULT: &str = "(no output)";

/// Whether `model` is one of Anthropic's (`anthropic.claude-…`, `us.anthropic.claude-…`).
fn is_claude(model: &str) -> bool {
    model.to_ascii_lowercase().contains("claude")
}

fn invalid(what: impl std::fmt::Display) -> AiError {
    AiError::InvalidRequest(format!("bedrock: {what}"))
}

fn document(value: &Value) -> Document {
    match value {
        Value::Null => Document::Null,
        Value::Bool(flag) => Document::Bool(*flag),
        Value::Number(number) => Document::Number(match (number.as_u64(), number.as_i64()) {
            (Some(unsigned), _) => Number::PosInt(unsigned),
            (None, Some(signed)) => Number::NegInt(signed),
            (None, None) => Number::Float(number.as_f64().unwrap_or_default()),
        }),
        Value::String(text) => Document::String(text.clone()),
        Value::Array(items) => Document::Array(items.iter().map(document).collect()),
        Value::Object(members) => Document::Object(
            members
                .iter()
                .map(|(name, value)| (name.clone(), document(value)))
                .collect(),
        ),
    }
}

fn cache_point() -> Result<CachePointBlock, AiError> {
    CachePointBlock::builder()
        .r#type(CachePointType::Default)
        .build()
        .map_err(invalid)
}

fn image(mime: &str, data: &str) -> Result<ImageBlock, AiError> {
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        "image/gif" => ImageFormat::Gif,
        "image/webp" => ImageFormat::Webp,
        other => return Err(invalid(format!("images of type {other} are not accepted"))),
    };
    let bytes = STANDARD
        .decode(data)
        .map_err(|e| invalid(format!("an image is not valid base64: {e}")))?;
    ImageBlock::builder()
        .format(format)
        .source(ImageSource::Bytes(Blob::new(bytes)))
        .build()
        .map_err(invalid)
}

/// One of our blocks as the content of a user or assistant message; `None` for what the API has
/// no place for.
fn content_block(block: &Block) -> Result<Option<ContentBlock>, AiError> {
    Ok(match block {
        Block::Text { text } if text.is_empty() => None,
        Block::Text { text } => Some(ContentBlock::Text(text.clone())),
        Block::Image { mime, data } => Some(ContentBlock::Image(image(mime, data)?)),
        // Thinking is replayed only with the signature that proves where it came from.
        Block::Thinking {
            signature: Some(signature),
            redacted: true,
            ..
        } => STANDARD.decode(signature).ok().map(|bytes| {
            ContentBlock::ReasoningContent(ReasoningContentBlock::RedactedContent(Blob::new(bytes)))
        }),
        Block::Thinking {
            text,
            signature: Some(signature),
            redacted: false,
        } => {
            let reasoning = ReasoningTextBlock::builder()
                .text(text)
                .signature(signature)
                .build()
                .map_err(invalid)?;
            Some(ContentBlock::ReasoningContent(
                ReasoningContentBlock::ReasoningText(reasoning),
            ))
        }
        Block::Thinking { .. } | Block::Opaque { .. } => None,
        Block::ToolCall { id, name, input, .. } => {
            let call = ToolUseBlock::builder()
                .tool_use_id(id)
                .name(name)
                .input(document(input))
                .build()
                .map_err(invalid)?;
            Some(ContentBlock::ToolUse(call))
        }
        Block::ToolResult {
            call_id,
            content,
            is_error,
        } => {
            let mut parts = Vec::with_capacity(content.len());
            for part in content {
                match part {
                    Block::Text { text } if !text.is_empty() => {
                        parts.push(ToolResultContentBlock::Text(text.clone()))
                    }
                    Block::Image { mime, data } => {
                        parts.push(ToolResultContentBlock::Image(image(mime, data)?))
                    }
                    _ => {}
                }
            }
            if parts.is_empty() {
                parts.push(ToolResultContentBlock::Text(EMPTY_RESULT.to_string()));
            }
            let status = if *is_error {
                ToolResultStatus::Error
            } else {
                ToolResultStatus::Success
            };
            let result = ToolResultBlock::builder()
                .tool_use_id(call_id)
                .set_content(Some(parts))
                .status(status)
                .build()
                .map_err(invalid)?;
            Some(ContentBlock::ToolResult(result))
        }
    })
}

/// The conversation as alternating user and assistant messages; tool results travel as user
/// content, and neighbours with the same role are joined.
fn converse_messages(messages: &[Message]) -> Result<Vec<sdk::Message>, AiError> {
    let mut turns: Vec<(ConversationRole, Vec<ContentBlock>)> = Vec::new();
    for message in messages {
        let role = match message.role {
            Role::Assistant => ConversationRole::Assistant,
            Role::User | Role::Tool => ConversationRole::User,
        };
        let mut content = Vec::with_capacity(message.content.len());
        for block in &message.content {
            content.extend(content_block(block)?);
        }
        if content.is_empty() {
            continue;
        }
        match turns.last_mut() {
            Some((last, blocks)) if *last == role => blocks.extend(content),
            _ => turns.push((role, content)),
        }
    }
    turns
        .into_iter()
        .map(|(role, content)| {
            sdk::Message::builder()
                .role(role)
                .set_content(Some(content))
                .build()
                .map_err(invalid)
        })
        .collect()
}

/// Everything a `converse_stream` call takes besides the model id.
#[derive(Debug)]
struct ConverseInput {
    system: Vec<SystemContentBlock>,
    messages: Vec<sdk::Message>,
    tools: Option<ToolConfiguration>,
    inference: InferenceConfiguration,
    additional: Option<Document>,
}

fn converse_input(
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
) -> Result<ConverseInput, AiError> {
    let claude = is_claude(&model.id);

    let mut system = Vec::new();
    for part in req.system.iter().filter(|part| !part.text.is_empty()) {
        system.push(SystemContentBlock::Text(part.text.clone()));
        // What precedes the boundary stays the same from request to request.
        if part.cache_boundary && claude {
            system.push(SystemContentBlock::CachePoint(cache_point()?));
        }
    }

    let tools = if req.tools.is_empty() {
        None
    } else {
        let mut tools = Vec::with_capacity(req.tools.len() + 1);
        for tool in &req.tools {
            let spec = ToolSpecification::builder()
                .name(&tool.name)
                .description(&tool.description)
                .input_schema(ToolInputSchema::Json(document(&tool.schema)))
                .build()
                .map_err(invalid)?;
            tools.push(Tool::ToolSpec(spec));
        }
        if claude {
            tools.push(Tool::CachePoint(cache_point()?));
        }
        Some(
            ToolConfiguration::builder()
                .set_tools(Some(tools))
                .build()
                .map_err(invalid)?,
        )
    };

    // Claude takes the thinking request of the Anthropic API, so that module decides its shape and
    // the output limit that goes with it.
    let mut max_tokens = req.max_output.unwrap_or(model.max_output);
    let mut additional = BTreeMap::new();
    if claude {
        let body = wire::anthropic::build_body(provider, model, req);
        if let Some(limit) = body.get("max_tokens").and_then(Value::as_u64) {
            max_tokens = u32::try_from(limit).unwrap_or(u32::MAX);
        }
        for member in ["thinking", "output_config"] {
            if let Some(value) = body.get(member) {
                additional.insert(member.to_string(), value.clone());
            }
        }
    }
    let thinking = additional.contains_key("thinking");
    let inference = InferenceConfiguration::builder()
        .max_tokens(i32::try_from(max_tokens).unwrap_or(i32::MAX))
        // Models that are thinking do not take a temperature.
        .set_temperature(req.temperature.filter(|t| t.is_finite() && !thinking))
        .build();

    Ok(ConverseInput {
        system,
        messages: converse_messages(&req.messages)?,
        tools,
        inference,
        additional: (!additional.is_empty()).then(|| document(&json!(additional))),
    })
}

enum Open {
    Text(String),
    Thinking { text: String, signature: String },
    Redacted(Vec<u8>),
    Tool { id: String, name: String, json: String },
}

/// Turns the events of one `converse_stream` response into stream events.
struct ConverseDecoder {
    model: String,
    started: bool,
    finished: bool,
    /// Blocks still receiving deltas, by the API's block index, with their index in the message.
    open: HashMap<i32, (usize, Open)>,
    /// Finished blocks at their own index; `None` while a block is open.
    blocks: Vec<Option<Block>>,
    usage: Usage,
    stop: Option<sdk::StopReason>,
}

impl ConverseDecoder {
    fn new(model: &ModelInfo) -> Self {
        ConverseDecoder {
            model: model.id.clone(),
            started: false,
            finished: false,
            open: HashMap::new(),
            blocks: Vec::new(),
            usage: Usage::default(),
            stop: None,
        }
    }

    fn start(&mut self, out: &mut Vec<StreamEvent>) {
        if !self.started {
            self.started = true;
            out.push(StreamEvent::Start);
        }
    }

    /// The open block at the API's `key`, opened with `fresh` when no start event announced it
    /// (text and reasoning blocks begin with their first delta).
    fn open_at(&mut self, key: i32, fresh: impl FnOnce() -> Open) -> (usize, &mut Open) {
        let blocks = &mut self.blocks;
        let (index, open) = self.open.entry(key).or_insert_with(|| {
            blocks.push(None);
            (blocks.len() - 1, fresh())
        });
        (*index, open)
    }

    fn delta(&mut self, key: i32, delta: ContentBlockDelta, out: &mut Vec<StreamEvent>) {
        match delta {
            ContentBlockDelta::Text(delta) => {
                if let (index, Open::Text(text)) = self.open_at(key, || Open::Text(String::new())) {
                    text.push_str(&delta);
                    out.push(StreamEvent::TextDelta { index, delta });
                }
            }
            ContentBlockDelta::ReasoningContent(ReasoningContentBlockDelta::RedactedContent(blob)) => {
                if let (_, Open::Redacted(bytes)) = self.open_at(key, || Open::Redacted(Vec::new())) {
                    bytes.extend_from_slice(blob.as_ref());
                }
            }
            ContentBlockDelta::ReasoningContent(reasoning) => {
                let fresh = || Open::Thinking {
                    text: String::new(),
                    signature: String::new(),
                };
                let (index, Open::Thinking { text, signature }) = self.open_at(key, fresh) else {
                    return;
                };
                match reasoning {
                    ReasoningContentBlockDelta::Text(delta) => {
                        text.push_str(&delta);
                        out.push(StreamEvent::ThinkingDelta { index, delta });
                    }
                    ReasoningContentBlockDelta::Signature(part) => signature.push_str(&part),
                    _ => {}
                }
            }
            ContentBlockDelta::ToolUse(call) => {
                if let Some((index, Open::Tool { json, .. })) = self.open.get_mut(&key) {
                    json.push_str(&call.input);
                    if !call.input.is_empty() {
                        out.push(StreamEvent::ToolCallDelta {
                            index: *index,
                            delta: call.input,
                        });
                    }
                }
            }
            _ => tracing::debug!("ignoring a content block delta of a kind this client does not use"),
        }
    }

    fn close(&mut self, key: i32, out: &mut Vec<StreamEvent>) {
        let Some((index, open)) = self.open.remove(&key) else {
            return;
        };
        let block = match open {
            Open::Text(text) => Block::Text { text },
            Open::Thinking { text, signature } => Block::Thinking {
                text,
                signature: Some(signature).filter(|s| !s.is_empty()),
                redacted: false,
            },
            Open::Redacted(bytes) => Block::Thinking {
                text: String::new(),
                signature: Some(STANDARD.encode(bytes)),
                redacted: true,
            },
            Open::Tool { id, name, json } => Block::ToolCall {
                id,
                name,
                input: tool_input(&json),
                signature: None,
            },
        };
        self.blocks[index] = Some(block.clone());
        out.push(StreamEvent::BlockEnd { index, block });
    }

    fn close_all(&mut self, out: &mut Vec<StreamEvent>) {
        let mut keys: Vec<(usize, i32)> = self.open.iter().map(|(key, (index, _))| (*index, *key)).collect();
        keys.sort_unstable();
        for (_, key) in keys {
            self.close(key, out);
        }
    }

    fn meta(&self, stop: StopReason) -> AssistantMeta {
        AssistantMeta {
            provider: BEDROCK.to_string(),
            model: self.model.clone(),
            api: WireApi::BedrockConverse,
            usage: self.usage,
            stop,
            response_id: None,
            duration_ms: 0,
        }
    }

    fn done(&mut self, out: &mut Vec<StreamEvent>) {
        self.close_all(out);
        self.finished = true;
        let content: Vec<Block> = self.blocks.drain(..).flatten().collect();
        let stop = if content
            .iter()
            .any(|block| matches!(block, Block::ToolCall { .. }))
        {
            StopReason::ToolUse
        } else {
            match self.stop {
                Some(sdk::StopReason::MaxTokens | sdk::StopReason::ModelContextWindowExceeded) => {
                    StopReason::Length
                }
                _ => StopReason::Stop,
            }
        };
        out.push(StreamEvent::Done {
            message: Message::assistant(content, self.meta(stop)),
        });
    }

    fn fail(&mut self, error: AiError, out: &mut Vec<StreamEvent>) {
        self.start(out);
        self.close_all(out);
        self.finished = true;
        let content: Vec<Block> = self.blocks.drain(..).flatten().collect();
        let partial =
            (!content.is_empty()).then(|| Message::assistant(content, self.meta(StopReason::Error)));
        out.push(StreamEvent::Error { error, partial });
    }

    /// The events one SDK event stands for. Nothing comes after a terminal event.
    fn feed(&mut self, event: ConverseStreamOutput) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        self.start(&mut out);
        match event {
            ConverseStreamOutput::MessageStart(_) => {}
            ConverseStreamOutput::ContentBlockStart(start) => {
                if let Some(ContentBlockStart::ToolUse(call)) = start.start {
                    self.blocks.push(None);
                    let index = self.blocks.len() - 1;
                    out.push(StreamEvent::ToolCallStart {
                        index,
                        id: call.tool_use_id.clone(),
                        name: call.name.clone(),
                    });
                    let open = Open::Tool {
                        id: call.tool_use_id,
                        name: call.name,
                        json: String::new(),
                    };
                    self.open.insert(start.content_block_index, (index, open));
                }
            }
            ConverseStreamOutput::ContentBlockDelta(event) => {
                if let Some(delta) = event.delta {
                    self.delta(event.content_block_index, delta, &mut out);
                }
            }
            ConverseStreamOutput::ContentBlockStop(stop) => self.close(stop.content_block_index, &mut out),
            // The usage follows in the metadata event, which ends the response.
            ConverseStreamOutput::MessageStop(stop) => self.stop = Some(stop.stop_reason),
            ConverseStreamOutput::Metadata(metadata) => {
                if let Some(usage) = metadata.usage {
                    let count = |tokens: i32| u64::try_from(tokens).unwrap_or(0);
                    self.usage = Usage {
                        input: count(usage.input_tokens),
                        output: count(usage.output_tokens),
                        cache_read: usage.cache_read_input_tokens.map_or(0, count),
                        cache_write: usage.cache_write_input_tokens.map_or(0, count),
                        reasoning: 0,
                        cost_usd: None,
                    };
                }
                self.done(&mut out);
            }
            _ => tracing::debug!("ignoring a stream event of a kind this client does not use"),
        }
        out
    }

    /// The stream ended. A response that got as far as its stop reason is complete even when the
    /// usage never arrived.
    fn finish(&mut self) -> Vec<StreamEvent> {
        let mut out = Vec::new();
        if self.finished {
            return out;
        }
        if self.stop.is_some() {
            self.start(&mut out);
            self.done(&mut out);
        } else {
            self.fail(AiError::Network(STREAM_ENDED.to_string()), &mut out);
        }
        out
    }
}

/// The arguments of a tool call: `{}` when the model sent none, and what is not a JSON object
/// under [`INVALID_JSON_KEY`].
fn tool_input(raw: &str) -> Value {
    if raw.trim().is_empty() {
        return json!({});
    }
    match serde_json::from_str::<Value>(raw) {
        Ok(value @ Value::Object(_)) => value,
        _ => json!({ INVALID_JSON_KEY: raw }),
    }
}

/// The error an exception of the service stands for. `code` is its name, `detail` everything that
/// is known about the failure.
fn classify(code: Option<&str>, message: &str, detail: String) -> AiError {
    let text = if message.is_empty() {
        detail.clone()
    } else {
        message.to_string()
    };
    let lower = text.to_ascii_lowercase();
    match code {
        Some("ThrottlingException") => AiError::RateLimit { retry_after: None },
        Some("ServiceUnavailableException" | "ModelNotReadyException") => AiError::Overloaded,
        Some("ModelTimeoutException") => AiError::Network(text),
        Some("AccessDeniedException" | "UnrecognizedClientException" | "ExpiredTokenException") => {
            AiError::Auth(text)
        }
        Some("ValidationException")
            if lower.contains("too long")
                || lower.contains("too many tokens")
                || lower.contains("context window") =>
        {
            AiError::ContextOverflow
        }
        Some("ValidationException" | "ResourceNotFoundException") => AiError::InvalidRequest(text),
        Some("InternalServerException" | "ModelErrorException" | "ModelStreamErrorException") => {
            AiError::Server {
                status: 500,
                body: text,
            }
        }
        // No answer from the service: missing credentials, or the connection.
        _ if detail.to_ascii_lowercase().contains("credential") => AiError::Auth(format!(
            "no usable AWS credentials ({detail}): configure a profile, SSO or the AWS_* variables"
        )),
        _ => AiError::Network(detail),
    }
}

fn sdk_error<E, R>(error: &SdkError<E, R>) -> AiError
where
    E: ProvideErrorMetadata + std::error::Error + 'static,
    R: std::fmt::Debug + 'static,
{
    let (code, message) = match error.as_service_error() {
        Some(service) => (service.code(), service.message().unwrap_or_default()),
        None => (None, ""),
    };
    // The SDK's own message says only which stage failed; the causes say why.
    let mut detail = error.to_string();
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        detail.push_str(": ");
        detail.push_str(&cause.to_string());
        source = cause.source();
    }
    classify(code, message, detail)
}

pub struct BedrockProvider {
    region: Option<String>,
    profile: Option<String>,
    config: ProviderConfig,
    client: OnceCell<aws_sdk_bedrockruntime::Client>,
}

impl BedrockProvider {
    /// `region` and `profile` override what the AWS default chain finds.
    pub fn new(region: Option<String>, profile: Option<String>) -> Self {
        let chosen = |value: Option<String>| value.map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        BedrockProvider {
            region: chosen(region),
            profile: chosen(profile),
            config: ProviderConfig {
                requires_key: true,
                ..ProviderConfig::new(BEDROCK, WireApi::BedrockConverse, String::new())
            },
            client: OnceCell::new(),
        }
    }

    /// The client, made from the environment on first use: looking up credentials can take a
    /// moment and nothing should pay for it before a Bedrock model is used.
    async fn client(&self) -> &aws_sdk_bedrockruntime::Client {
        self.client
            .get_or_init(|| async {
                let mut loader = aws_config::defaults(aws_config::BehaviorVersion::latest());
                if let Some(region) = &self.region {
                    loader = loader.region(aws_config::Region::new(region.clone()));
                }
                if let Some(profile) = &self.profile {
                    loader = loader.profile_name(profile);
                }
                aws_sdk_bedrockruntime::Client::new(&loader.load().await)
            })
            .await
    }
}

type Events = aws_sdk_bedrockruntime::primitives::event_stream::EventReceiver<
    ConverseStreamOutput,
    sdk::error::ConverseStreamOutputError,
>;

/// One response being received.
struct Live {
    events: Events,
    decoder: ConverseDecoder,
    queue: VecDeque<StreamEvent>,
    finished: bool,
    cancel: CancellationToken,
    started: Instant,
    cost: Option<Cost>,
}

impl Live {
    fn accept(&mut self, events: Vec<StreamEvent>) {
        for mut event in events {
            if let StreamEvent::Done { message } = &mut event
                && let Some(meta) = &mut message.meta
            {
                meta.duration_ms = self.started.elapsed().as_millis() as u64;
                if let Some(cost) = &self.cost {
                    meta.usage.price(cost);
                }
            }
            self.finished |= event.is_terminal();
            self.queue.push_back(event);
        }
    }

    async fn next(mut self) -> Option<(StreamEvent, Live)> {
        loop {
            if let Some(event) = self.queue.pop_front() {
                return Some((event, self));
            }
            if self.finished {
                return None;
            }
            let events = tokio::select! {
                biased;
                _ = self.cancel.cancelled() => {
                    let mut out = Vec::new();
                    self.decoder.fail(AiError::Aborted, &mut out);
                    out
                }
                received = self.events.recv() => match received {
                    Ok(Some(event)) => self.decoder.feed(event),
                    Ok(None) => self.decoder.finish(),
                    Err(error) => {
                        let mut out = Vec::new();
                        self.decoder.fail(sdk_error(&error), &mut out);
                        out
                    }
                },
            };
            let ended = events.is_empty() && self.decoder.finished;
            self.accept(events);
            self.finished |= ended;
        }
    }
}

#[async_trait]
impl Provider for BedrockProvider {
    fn id(&self) -> &str {
        BEDROCK
    }

    async fn stream(
        &self,
        model: &ModelInfo,
        req: &Request,
        cancel: CancellationToken,
    ) -> Result<EventStream, AiError> {
        let target = Target {
            provider: BEDROCK,
            model,
            api: WireApi::BedrockConverse,
        };
        let input = converse_input(&self.config, model, &prepare(req, &target))?;
        let started = Instant::now();
        let call = self
            .client()
            .await
            .converse_stream()
            .model_id(&model.id)
            .set_system((!input.system.is_empty()).then_some(input.system))
            .set_messages(Some(input.messages))
            .set_tool_config(input.tools)
            .set_inference_config(Some(input.inference))
            .set_additional_model_request_fields(input.additional)
            .send();
        let output = tokio::select! {
            _ = cancel.cancelled() => return Err(AiError::Aborted),
            sent = call => sent.map_err(|error| sdk_error(&error))?,
        };
        let live = Live {
            events: output.stream,
            decoder: ConverseDecoder::new(model),
            queue: VecDeque::new(),
            finished: false,
            cancel,
            started,
            cost: model.cost,
        };
        Ok(Box::pin(futures::stream::unfold(live, Live::next)))
    }

    /// The models come from the catalogue: listing them is a different AWS service.
    async fn list_models(&self) -> Result<Vec<ModelInfo>, AiError> {
        Ok(Vec::new())
    }
}

#[cfg(test)]
mod tests {
    use aws_sdk_bedrockruntime::types::{
        ContentBlockDeltaEvent, ContentBlockStartEvent, ContentBlockStopEvent, ConverseStreamMetadataEvent,
        MessageStartEvent, MessageStopEvent, TokenUsage, ToolUseBlockDelta, ToolUseBlockStart,
    };

    use super::*;
    use crate::types::{SystemPart, ThinkingLevel, ToolSpec};

    fn model(id: &str) -> ModelInfo {
        ModelInfo {
            reasoning: true,
            max_output: 64_000,
            ..ModelInfo::unknown(BEDROCK, id)
        }
    }

    fn delta(index: i32, delta: ContentBlockDelta) -> ConverseStreamOutput {
        let event = ContentBlockDeltaEvent::builder()
            .content_block_index(index)
            .delta(delta)
            .build()
            .unwrap();
        ConverseStreamOutput::ContentBlockDelta(event)
    }

    fn reasoning(index: i32, delta: ReasoningContentBlockDelta) -> ConverseStreamOutput {
        self::delta(index, ContentBlockDelta::ReasoningContent(delta))
    }

    fn stop_block(index: i32) -> ConverseStreamOutput {
        let event = ContentBlockStopEvent::builder()
            .content_block_index(index)
            .build()
            .unwrap();
        ConverseStreamOutput::ContentBlockStop(event)
    }

    fn stop(reason: sdk::StopReason) -> ConverseStreamOutput {
        ConverseStreamOutput::MessageStop(MessageStopEvent::builder().stop_reason(reason).build().unwrap())
    }

    fn metadata() -> ConverseStreamOutput {
        let usage = TokenUsage::builder()
            .input_tokens(12)
            .output_tokens(34)
            .total_tokens(146)
            .cache_read_input_tokens(100)
            .build()
            .unwrap();
        ConverseStreamOutput::Metadata(ConverseStreamMetadataEvent::builder().usage(usage).build())
    }

    fn run(events: Vec<ConverseStreamOutput>) -> (Vec<StreamEvent>, ConverseDecoder) {
        let mut decoder = ConverseDecoder::new(&model("anthropic.claude-sonnet-4-5-v1:0"));
        let out = events.into_iter().flat_map(|event| decoder.feed(event)).collect();
        (out, decoder)
    }

    #[test]
    fn a_response_with_thinking_text_and_a_tool_call_maps_block_by_block() {
        let role = MessageStartEvent::builder()
            .role(ConversationRole::Assistant)
            .build()
            .unwrap();
        let call = ToolUseBlockStart::builder()
            .tool_use_id("t1")
            .name("grep")
            .build()
            .unwrap();
        let begin = ContentBlockStartEvent::builder()
            .content_block_index(2)
            .start(ContentBlockStart::ToolUse(call))
            .build()
            .unwrap();
        let arguments = |part: &str| {
            let call = ToolUseBlockDelta::builder().input(part).build().unwrap();
            delta(2, ContentBlockDelta::ToolUse(call))
        };
        let (events, _) = run(vec![
            ConverseStreamOutput::MessageStart(role),
            reasoning(0, ReasoningContentBlockDelta::Text("Let me ".into())),
            reasoning(0, ReasoningContentBlockDelta::Text("look.".into())),
            reasoning(0, ReasoningContentBlockDelta::Signature("sig".into())),
            stop_block(0),
            delta(1, ContentBlockDelta::Text("Searching".into())),
            stop_block(1),
            ConverseStreamOutput::ContentBlockStart(begin),
            arguments("{\"pattern\":"),
            arguments("\"FIXME\"}"),
            stop_block(2),
            stop(sdk::StopReason::ToolUse),
            metadata(),
            // Nothing follows the end of the response.
            delta(3, ContentBlockDelta::Text("late".into())),
        ]);

        let thinking = Block::Thinking {
            text: "Let me look.".into(),
            signature: Some("sig".into()),
            redacted: false,
        };
        let text = Block::text("Searching");
        let tool = Block::ToolCall {
            id: "t1".into(),
            name: "grep".into(),
            input: json!({ "pattern": "FIXME" }),
            signature: None,
        };
        let usage = Usage {
            input: 12,
            output: 34,
            cache_read: 100,
            ..Usage::default()
        };
        let meta = AssistantMeta {
            provider: "bedrock".into(),
            model: "anthropic.claude-sonnet-4-5-v1:0".into(),
            api: WireApi::BedrockConverse,
            usage,
            stop: StopReason::ToolUse,
            response_id: None,
            duration_ms: 0,
        };
        let thinking_delta = |delta: &str| StreamEvent::ThinkingDelta {
            index: 0,
            delta: delta.into(),
        };
        let call_delta = |delta: &str| StreamEvent::ToolCallDelta {
            index: 2,
            delta: delta.into(),
        };
        assert_eq!(
            events,
            vec![
                StreamEvent::Start,
                thinking_delta("Let me "),
                thinking_delta("look."),
                StreamEvent::BlockEnd {
                    index: 0,
                    block: thinking.clone()
                },
                StreamEvent::TextDelta {
                    index: 1,
                    delta: "Searching".into()
                },
                StreamEvent::BlockEnd {
                    index: 1,
                    block: text.clone()
                },
                StreamEvent::ToolCallStart {
                    index: 2,
                    id: "t1".into(),
                    name: "grep".into()
                },
                call_delta("{\"pattern\":"),
                call_delta("\"FIXME\"}"),
                StreamEvent::BlockEnd {
                    index: 2,
                    block: tool.clone()
                },
                StreamEvent::Done {
                    message: Message::assistant(vec![thinking, text, tool], meta)
                },
            ]
        );
    }

    #[test]
    fn how_a_response_ends_decides_between_done_length_and_error() {
        // Blocks nobody closed are closed by the end of the message; the limit is reported.
        let (events, _) = run(vec![
            delta(0, ContentBlockDelta::Text("cut of".into())),
            stop(sdk::StopReason::MaxTokens),
            metadata(),
        ]);
        let Some(StreamEvent::Done { message }) = events.last() else {
            panic!("{events:?}");
        };
        assert_eq!(message.text(), "cut of");
        assert_eq!(message.meta.as_ref().unwrap().stop, StopReason::Length);
        assert!(matches!(&events[2], StreamEvent::BlockEnd { index: 0, .. }));

        // The stop reason arrived but the usage did not: still a complete answer.
        let (mut events, mut decoder) = run(vec![
            delta(0, ContentBlockDelta::Text("ok".into())),
            stop_block(0),
            stop(sdk::StopReason::EndTurn),
        ]);
        events.extend(decoder.finish());
        assert!(matches!(events.last(), Some(StreamEvent::Done { message }) if message.text() == "ok"));
        assert!(decoder.finish().is_empty());

        // The stream broke off: what was received is handed over with the error.
        let (mut events, mut decoder) = run(vec![delta(0, ContentBlockDelta::Text("half".into()))]);
        events.extend(decoder.finish());
        let Some(StreamEvent::Error { error, partial }) = events.last() else {
            panic!("{events:?}");
        };
        assert_eq!(error, &AiError::Network(STREAM_ENDED.into()));
        assert_eq!(partial.as_ref().unwrap().text(), "half");

        // Nothing at all still starts before it fails.
        let mut empty = ConverseDecoder::new(&model("m"));
        assert_eq!(
            empty.finish(),
            vec![
                StreamEvent::Start,
                StreamEvent::Error {
                    error: AiError::Network(STREAM_ENDED.into()),
                    partial: None
                }
            ]
        );
    }

    #[test]
    fn redacted_reasoning_and_broken_arguments_survive_the_round_trip() {
        let call = ToolUseBlockStart::builder()
            .tool_use_id("t1")
            .name("grep")
            .build()
            .unwrap();
        let begin = ContentBlockStartEvent::builder()
            .content_block_index(1)
            .start(ContentBlockStart::ToolUse(call))
            .build()
            .unwrap();
        let broken = ToolUseBlockDelta::builder()
            .input("{\"pattern\":")
            .build()
            .unwrap();
        let (events, _) = run(vec![
            reasoning(
                0,
                ReasoningContentBlockDelta::RedactedContent(Blob::new(vec![1, 2, 255])),
            ),
            stop_block(0),
            ConverseStreamOutput::ContentBlockStart(begin),
            delta(1, ContentBlockDelta::ToolUse(broken)),
            stop(sdk::StopReason::ToolUse),
            metadata(),
        ]);
        let Some(StreamEvent::Done { message }) = events.last() else {
            panic!("{events:?}");
        };
        let redacted = Block::Thinking {
            text: String::new(),
            signature: Some(STANDARD.encode([1, 2, 255])),
            redacted: true,
        };
        assert_eq!(message.content[0], redacted);
        assert_eq!(
            message.tool_calls().next().unwrap().2,
            &json!({ INVALID_JSON_KEY: "{\"pattern\":" })
        );

        // Sent back, the redacted block is the same bytes again.
        let Some(ContentBlock::ReasoningContent(ReasoningContentBlock::RedactedContent(blob))) =
            content_block(&redacted).unwrap()
        else {
            panic!("a redacted reasoning block");
        };
        assert_eq!(blob.as_ref(), [1, 2, 255]);
        assert_eq!(tool_input("  "), json!({}));
        assert_eq!(tool_input("[1]"), json!({ INVALID_JSON_KEY: "[1]" }));
    }

    fn request() -> Request {
        Request {
            system: vec![
                SystemPart {
                    text: "You are careful.".into(),
                    name: String::new(),
                    cache_boundary: true,
                },
                SystemPart {
                    text: String::new(),
                    name: String::new(),
                    cache_boundary: false,
                },
                SystemPart {
                    text: "Today is Monday.".into(),
                    name: String::new(),
                    cache_boundary: false,
                },
            ],
            messages: vec![
                Message::user("find the markers"),
                Message::assistant(
                    vec![
                        Block::Thinking {
                            text: "unsigned".into(),
                            signature: None,
                            redacted: false,
                        },
                        Block::text("Searching"),
                        Block::ToolCall {
                            id: "t1".into(),
                            name: "grep".into(),
                            input: json!({ "pattern": "FIXME", "limit": 5, "deep": true, "ratio": 0.5 }),
                            signature: None,
                        },
                    ],
                    AssistantMeta {
                        provider: BEDROCK.into(),
                        model: "m".into(),
                        api: WireApi::BedrockConverse,
                        usage: Usage::default(),
                        stop: StopReason::ToolUse,
                        response_id: None,
                        duration_ms: 0,
                    },
                ),
                Message::tool(vec![Block::ToolResult {
                    call_id: "t1".into(),
                    content: Vec::new(),
                    is_error: true,
                }]),
                Message::user("and now?"),
            ],
            tools: vec![ToolSpec {
                name: "grep".into(),
                description: "Search files".into(),
                schema: json!({ "type": "object", "properties": { "pattern": { "type": "string" } } }),
            }],
            thinking: ThinkingLevel::Off,
            max_output: Some(2000),
            temperature: Some(0.5),
            cache_key: None,
            tier: None,
        }
    }

    fn config() -> ProviderConfig {
        BedrockProvider::new(None, None).config
    }

    #[test]
    fn a_request_becomes_alternating_turns_with_tools_and_limits() {
        let input = converse_input(&config(), &model("amazon.nova-pro-v1:0"), &request()).unwrap();

        // No cache points for a model family that is not known to take them.
        assert_eq!(
            input.system,
            [
                SystemContentBlock::Text("You are careful.".into()),
                SystemContentBlock::Text("Today is Monday.".into())
            ]
        );
        let roles: Vec<&ConversationRole> = input.messages.iter().map(|message| &message.role).collect();
        assert_eq!(
            roles,
            [
                &ConversationRole::User,
                &ConversationRole::Assistant,
                &ConversationRole::User
            ]
        );
        // Unsigned thinking is dropped; the call keeps its arguments with their types.
        let assistant = &input.messages[1].content;
        assert_eq!(assistant.len(), 2);
        let ContentBlock::ToolUse(call) = &assistant[1] else {
            panic!("{assistant:?}");
        };
        assert_eq!((call.tool_use_id.as_str(), call.name.as_str()), ("t1", "grep"));
        let Document::Object(arguments) = &call.input else {
            panic!("{:?}", call.input);
        };
        assert_eq!(arguments["pattern"], Document::String("FIXME".into()));
        assert_eq!(arguments["limit"], Document::Number(Number::PosInt(5)));
        assert_eq!(arguments["deep"], Document::Bool(true));
        assert_eq!(arguments["ratio"], Document::Number(Number::Float(0.5)));
        // The tool result and the user's next words share one user turn.
        let last = &input.messages[2].content;
        let ContentBlock::ToolResult(result) = &last[0] else {
            panic!("{last:?}");
        };
        assert_eq!(result.status, Some(ToolResultStatus::Error));
        assert_eq!(
            result.content,
            [ToolResultContentBlock::Text(EMPTY_RESULT.into())]
        );
        assert_eq!(last[1], ContentBlock::Text("and now?".into()));

        let tools = input.tools.unwrap().tools;
        assert_eq!(tools.len(), 1);
        let Tool::ToolSpec(spec) = &tools[0] else {
            panic!("{tools:?}");
        };
        assert_eq!(
            (spec.name.as_str(), spec.description.as_deref()),
            ("grep", Some("Search files"))
        );
        assert_eq!(input.inference.max_tokens, Some(2000));
        assert_eq!(input.inference.temperature, Some(0.5));
        assert_eq!(input.additional, None);
    }

    #[test]
    fn claude_models_get_cache_points_and_the_anthropic_thinking_request() {
        let mut request = request();
        request.thinking = ThinkingLevel::Medium;
        request.messages.truncate(1);
        let input = converse_input(
            &config(),
            &model("us.anthropic.claude-sonnet-4-5-20250929-v1:0"),
            &request,
        )
        .unwrap();

        assert!(matches!(input.system[1], SystemContentBlock::CachePoint(_)));
        assert_eq!(input.system.len(), 3);
        let tools = input.tools.unwrap().tools;
        assert!(matches!(tools.last(), Some(Tool::CachePoint(_))));

        let Some(Document::Object(additional)) = &input.additional else {
            panic!("{:?}", input.additional);
        };
        let Document::Object(thinking) = &additional["thinking"] else {
            panic!("{additional:?}");
        };
        assert_eq!(thinking["type"], Document::String("enabled".into()));
        assert_eq!(
            thinking["budget_tokens"],
            Document::Number(Number::PosInt(10_000))
        );
        // The answer needs room beyond the thinking budget, and thinking excludes a temperature.
        assert!(input.inference.max_tokens.unwrap() > 10_000);
        assert_eq!(input.inference.temperature, None);
    }

    #[test]
    fn service_failures_become_the_errors_the_router_understands() {
        let of = |code: &str, message: &str| classify(Some(code), message, format!("{code}: {message}"));
        assert_eq!(
            of("ThrottlingException", "slow down"),
            AiError::RateLimit { retry_after: None }
        );
        assert_eq!(of("ServiceUnavailableException", ""), AiError::Overloaded);
        assert_eq!(
            of("ValidationException", "Input is too long for requested model."),
            AiError::ContextOverflow
        );
        assert_eq!(
            of("ValidationException", "unknown field"),
            AiError::InvalidRequest("unknown field".into())
        );
        assert_eq!(
            of("AccessDeniedException", "no access"),
            AiError::Auth("no access".into())
        );
        assert!(matches!(
            of("ModelStreamErrorException", "boom"),
            AiError::Server { status: 500, .. }
        ));
        assert!(
            of("ThrottlingException", "").is_retryable() && !of("AccessDeniedException", "").is_retryable()
        );
        let no_credentials = classify(None, "", "dispatch failure: no credentials were loaded".into());
        assert!(matches!(&no_credentials, AiError::Auth(why) if why.contains("no usable AWS credentials")));
        assert_eq!(
            classify(None, "", "dispatch failure: connection reset".into()),
            AiError::Network("dispatch failure: connection reset".into())
        );
    }
}
