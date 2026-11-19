//! Anthropic Messages API: `POST {base_url}/v1/messages` with `stream: true`.
//!
//! Mapping decisions that follow the API reference for version `2026-10-21`:
//! - that version rejects `temperature`, so it is sent only when `provider.headers` pins an older
//!   `anthropic-version` and thinking is off;
//! - thinking is `{"type": "adaptive"}` steered by `output_config.effort`; only Claude models older
//!   than 4.6, and non-Claude models behind compatible endpoints, take the manual `budget_tokens` form;
//! - an assistant turn is replayed block for block in its original order, thinking blocks included;
//! - the priority tier is fast mode, `speed: "fast"` (the `service_tier` member is legacy and only
//!   chooses between committed and standard capacity); the models on which fast mode is a preview
//!   also need the `fast-mode-2026-02-01` beta header;
//! - a hosted web search ([`HostedTool::WebSearch`]) is the `web_search_20250305` server tool, the
//!   version every platform serves and the one that returns its results as plain blocks. The pages
//!   come back as `web_search_tool_result` blocks and as `citations` on the text that uses them.

use std::collections::{HashMap, HashSet};

use reqwest::header::{CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue};
use serde_json::{Map, Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::limits::{REJECTED, Window};
use crate::providers::presets::ProviderConfig;
use crate::stream::StreamEvent;
use crate::types::{
    AssistantMeta, Block, HostedTool, INVALID_JSON_KEY, Message, Request, Role, StopReason, SystemPart,
    ThinkingLevel, Tier, ToolSpec, Usage, WireApi,
};
use crate::wire::thinking::{ThinkingParam, thinking_param};
use crate::wire::{Decoder, KeyHeader, Sources, add_headers, apply_extra_body};

/// Latest stable API version. It removed the sampling parameters (`temperature`, `top_p`, `top_k`).
const API_VERSION: &str = "2026-10-21";
/// The client the subscription token was issued to; the API answers a login that does not name it
/// as if an unknown client had called.
const CLIENT_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
const VERSION_HEADER: &str = "anthropic-version";
const BETA_HEADER: &str = "anthropic-beta";
/// Needed for fast mode on the models where it is a preview or deprecated.
const FAST_MODE_BETA: &str = "fast-mode-2026-02-01";
/// Sent with a request that keeps tools out of the prompt until the model searches for them.
const TOOL_SEARCH_BETA: &str = "advanced-tool-use-2025-11-20";
/// The server tool that searches deferred tools by what they do.
const TOOL_SEARCH_TOOL: &str = "tool_search_tool_bm25_20251119";
/// What every header of a subscription's unified rate limit starts with.
const UNIFIED: &str = "anthropic-ratelimit-unified-";
/// The API accepts at most this many `cache_control` markers in one request.
const MAX_CACHE_BREAKPOINTS: usize = 4;
/// The mark deep in the conversation sits on a user turn at or before its middle, rounded down to
/// a multiple of this many messages, so that it stays where it is for many requests.
const ANCHOR_STEP: usize = 16;
/// How long a subscription login has its prompt cached. A session paid for by the window, not by
/// the token, gains from a cache that outlives a coffee break more than a write costs it.
const LOGIN_CACHE_TTL: &str = "1h";
/// Output tokens kept for the answer on top of a manual thinking budget.
const ANSWER_RESERVE: u32 = 1024;
/// Largest page the list-models endpoint serves.
const MODELS_PAGE_SIZE: u32 = 1000;

/// The web search server tool without dynamic filtering: later versions run the search from
/// code execution and may leave the result blocks out of the response.
const WEB_SEARCH_TOOL: &str = "web_search_20250305";

const USER: &str = "user";
const ASSISTANT: &str = "assistant";

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request) -> Value {
    let mut breakpoints = MAX_CACHE_BREAKPOINTS;
    let system = system_blocks(&req.system, &mut breakpoints);
    let mut tools = tool_definitions(&req.tools, &mut breakpoints);
    if defers_tools(req) {
        tools.push(json!({"type": TOOL_SEARCH_TOOL, "name": "tool_search_tool_bm25"}));
    }
    tools.extend(req.hosted.iter().map(hosted_definition));
    let mut messages = wire_messages(&provider.id, &req.messages);
    // A one-off (a summary, a memory extraction, a judge's verdict): one message and a system
    // prompt with no cache boundary. Nothing reads its cache again, so writing it is pure cost,
    // twice the input price on a login's hour-long cache.
    let one_off = req.messages.len() == 1 && !req.system.iter().any(|part| part.cache_boundary);
    if one_off {
        breakpoints = 0;
    }
    let mut marked = Vec::new();
    for (index, message) in messages
        .iter_mut()
        .enumerate()
        .rev()
        .filter(|(_, m)| m.role == USER)
        .take(2)
    {
        if let Some(block) = message.content.last_mut() {
            mark_cached(block, &mut breakpoints);
            marked.push(index);
        }
    }
    // A mark further back keeps the older half of the conversation cached when something near
    // its end changes (a rewind, a hook that rewrites a turn, a retried reply).
    let middle = messages.len() / 2 / ANCHOR_STEP * ANCHOR_STEP;
    if let Some(message) = messages
        .iter_mut()
        .enumerate()
        .take(middle)
        .rev()
        .find(|(index, m)| m.role == USER && !marked.contains(index))
        .map(|(_, m)| m)
        && let Some(block) = message.content.last_mut()
    {
        mark_cached(block, &mut breakpoints);
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
    if fast_mode(model, req) {
        body.insert("speed".to_string(), json!("fast"));
    }
    // After the cache marks, so that what is cached ends where the conversation does and the
    // next request, whose notes may differ, still finds it.
    if let Some(note) = req.tail_text().as_deref().and_then(text_block) {
        match messages.last_mut() {
            Some(last) if last.role == USER => last.content.push(note),
            _ => messages.push(WireMessage {
                role: USER,
                content: vec![note],
            }),
        }
    }
    let messages = messages
        .into_iter()
        .map(|m| json!({"role": m.role, "content": m.content}))
        .collect();
    body.insert("messages".to_string(), Value::Array(messages));
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
    let mut body = build_body(provider, model, req);
    // Copilot's Messages route takes a GitHub login, which is no Claude subscription.
    if matches!(auth, Auth::OAuth { .. })
        && !crate::wire::openai_chat::is_copilot(provider)
        && let Some(system) = body.get_mut("system").and_then(Value::as_array_mut)
    {
        // A subscription login must say which client is asking; the API rate limits a login that
        // does not identify as the CLI the token was issued to.
        system.insert(0, json!({"type": "text", "text": CLIENT_IDENTITY}));
    }
    if caches_for_an_hour(provider, auth) {
        extend_cache(&mut body);
    }
    let mut headers = headers(provider, auth, true);
    crate::wire::openai_chat::copilot_turn(provider, req, &mut headers);
    if fast_mode(model, req) && fast_mode_is_beta(&model.id) {
        add_beta(&mut headers, FAST_MODE_BETA);
    }
    if defers_tools(req) {
        add_beta(&mut headers, TOOL_SEARCH_BETA);
    }
    client
        .post(format!("{}/v1/messages", provider.base_url))
        .headers(headers)
        .json(&body)
}

/// Adds `beta` to the `anthropic-beta` header, after what it holds already.
fn add_beta(headers: &mut HeaderMap, beta: &str) {
    let betas = match headers.get(BETA_HEADER).and_then(|value| value.to_str().ok()) {
        Some(others) if others.split(',').any(|b| b.trim() == beta) => return,
        Some(others) if !others.trim().is_empty() => format!("{others},{beta}"),
        _ => beta.to_string(),
    };
    if let Ok(value) = HeaderValue::from_str(&betas) {
        headers.insert(HeaderName::from_static(BETA_HEADER), value);
    }
}

/// Whether the request keeps some of its tools out of the prompt until the model searches for
/// them. The router clears the marks for a provider that does not search tools itself.
fn defers_tools(req: &Request) -> bool {
    req.tools.iter().any(|tool| tool.defer)
}

/// Whether `model_id` can search deferred tools: Claude 4.5 and later (Haiku 4.5, Sonnet 4.5,
/// Opus 4.5 and every newer line). Ids that name no Claude model belong to compatible endpoints,
/// which do not.
pub fn searches_tools(model_id: &str) -> bool {
    claude_version(model_id).is_some_and(|version| version >= (4, 5))
}

/// The `(major, minor)` version in a Claude model id; see [`thinking_shape`] for the forms.
fn claude_version(model_id: &str) -> Option<(u32, u32)> {
    let id = model_id.to_ascii_lowercase();
    let family = id.find("claude")?;
    let mut parts = id[family..]
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(version_part)
        .skip_while(Option::is_none);
    let major = parts.next().flatten()?;
    Some((major, parts.next().flatten().unwrap_or(0)))
}

/// How long what a request wrote to the cache is kept, from the `cache_creation` part of its
/// usage: an hour when any of it was written with the longer lifetime, five minutes when only
/// the shorter one was used, `None` when nothing was written.
pub fn cache_ttl_secs(creation: &Value) -> Option<u64> {
    let count = |key: &str| creation.get(key).and_then(Value::as_u64).unwrap_or(0);
    if count("ephemeral_1h_input_tokens") > 0 {
        Some(3600)
    } else if count("ephemeral_5m_input_tokens") > 0 {
        Some(300)
    } else {
        None
    }
}

/// Whether requests made with `auth` have their prompt cached for an hour: a Claude subscription
/// login. Copilot's Messages route takes a GitHub login, which is billed otherwise.
pub fn caches_for_an_hour(provider: &ProviderConfig, auth: &Auth) -> bool {
    matches!(auth, Auth::OAuth { .. }) && !crate::wire::openai_chat::is_copilot(provider)
}

/// Gives every cache mark of `body` the login's longer lifetime. All marks get the same one: the
/// API wants longer-lived entries before shorter ones.
fn extend_cache(body: &mut Value) {
    let mark = |block: &mut Value| {
        if let Some(control) = block.get_mut("cache_control").and_then(Value::as_object_mut) {
            control.insert("ttl".to_string(), json!(LOGIN_CACHE_TTL));
        }
    };
    for key in ["system", "tools"] {
        for block in body
            .get_mut(key)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            mark(block);
        }
    }
    for message in body
        .get_mut("messages")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        for block in message
            .get_mut("content")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            mark(block);
        }
    }
}

/// Whether the request asks a Claude model for fast mode. Other models behind compatible
/// endpoints do not know the member, and Anthropic has no cheaper tier for a live request.
fn fast_mode(model: &ModelInfo, req: &Request) -> bool {
    req.tier == Some(Tier::Priority) && model.id.to_ascii_lowercase().contains("claude")
}

/// Fast mode needs its beta header on Claude Opus 5 and on the Opus 4 models; from Opus 5.5 on,
/// and on the other families, it is a regular request member.
fn fast_mode_is_beta(model_id: &str) -> bool {
    let id = model_id.to_ascii_lowercase();
    let Some(opus) = id.find("claude-opus") else {
        return false;
    };
    let mut parts = id[opus..]
        .split(|c: char| !c.is_ascii_alphanumeric())
        .map(version_part)
        .skip_while(Option::is_none);
    let major = parts.next().flatten().unwrap_or(0);
    let minor = parts.next().flatten().unwrap_or(0);
    (major, minor) <= (5, 0)
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
        refusal: None,
        response_id: None,
        sources: Sources::default(),
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

/// [`classify_error`] with the response headers: a subscription says in them that the account's
/// usage window is full and when it resets, which a status and a body alone do not tell apart
/// from being asked too often.
pub fn classify_response(status: u16, headers: &HeaderMap, body: &str) -> Option<AiError> {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let (_, message) = parsed.as_ref().map_or(("", ""), error_fields);
    let unified = |name: &str| header(headers, &format!("{UNIFIED}{name}"));
    let rejected = status == 429 && unified("status") == Some(REJECTED);
    if rejected || message.contains(EXTRA_USAGE) {
        let windows = limits(headers);
        let full = windows.iter().find(|window| window.exhausted());
        let resets_at = unified("reset")
            .and_then(|at| at.parse().ok())
            .or_else(|| full.and_then(|window| window.resets_at));
        let message = match (message.is_empty(), full) {
            (false, _) => message.to_string(),
            (true, Some(window)) => format!("the {} of this account is used up", window.label()),
            (true, None) => "the usage limit of this account is reached".to_string(),
        };
        return Some(AiError::Quota { resets_at, message });
    }
    classify_error(status, body)
}

fn header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    headers
        .get(name)?
        .to_str()
        .ok()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}

/// The usage windows of a subscription, from the unified rate-limit headers of a response:
/// `5h` and `7d` with their utilisation and reset time; `7d_oi`, the weekly window of the Fable
/// models, as `7d-fable`; the window the answer's status is about when it is another one; and
/// `overage` (extra usage) when the account has a say on it.
pub fn limits(headers: &HeaderMap) -> Vec<Window> {
    let unified = |name: &str| header(headers, &format!("{UNIFIED}{name}"));
    let status = unified("status").unwrap_or("allowed");
    // The window the status speaks about; the others are within their allowance.
    let claim = unified("representative-claim");
    let mut windows = Vec::new();
    for (prefix, name, claimed, scope) in [
        ("5h", "5h", "five_hour", None),
        ("7d", "7d", "seven_day", None),
        ("7d_oi", "7d-fable", "seven_day_overage_included", Some("Fable")),
    ] {
        let used = unified(&format!("{prefix}-utilization")).and_then(|used| used.parse::<f32>().ok());
        let resets_at = unified(&format!("{prefix}-reset")).and_then(|at| at.parse().ok());
        let is_claim = claim == Some(claimed);
        if used.is_none() && resets_at.is_none() && !is_claim {
            continue;
        }
        windows.push(Window {
            name: name.to_string(),
            used: used.unwrap_or(if is_claim && status == REJECTED { 1.0 } else { 0.0 }),
            resets_at: resets_at.or_else(|| unified("reset").filter(|_| is_claim)?.parse().ok()),
            status: if is_claim { status } else { "allowed" }.to_string(),
            scope: scope.map(str::to_string),
        });
    }
    let other = match claim {
        Some("seven_day_opus") => Some(("7d-opus", "Opus")),
        Some("seven_day_sonnet") => Some(("7d-sonnet", "Sonnet")),
        _ => None,
    };
    if let Some((name, scope)) = other.filter(|_| status != "allowed") {
        windows.push(Window {
            name: name.to_string(),
            used: if status == REJECTED { 1.0 } else { 0.0 },
            resets_at: unified("reset").and_then(|at| at.parse().ok()),
            status: status.to_string(),
            scope: Some(scope.to_string()),
        });
    }
    let overage = unified("overage-status");
    let disabled = unified("overage-disabled-reason");
    if overage.is_some() || disabled.is_some() {
        windows.push(Window {
            name: "overage".to_string(),
            used: unified("overage-utilization")
                .and_then(|used| used.parse().ok())
                .unwrap_or(0.0),
            resets_at: unified("overage-reset").and_then(|at| at.parse().ok()),
            status: match (overage, disabled) {
                (Some(status), Some(reason)) => format!("{status} ({reason})"),
                (Some(status), None) => status.to_string(),
                (None, Some(reason)) => format!("disabled ({reason})"),
                (None, None) => String::new(),
            },
            scope: None,
        });
    }
    windows
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
    add_headers(&mut map, provider, auth, KeyHeader::XApiKey);
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
    for message in out.iter_mut().filter(|m| m.role == ASSISTANT) {
        pair_server_blocks(&mut message.content);
    }
    out.retain(|m| !m.content.is_empty());
    out
}

/// Keeps a server tool's call (`server_tool_use`) only with its result (`…_tool_result` naming
/// it), and the other way round: the API refuses either alone. A reply can end on client tool
/// calls before the server ran its tool, which leaves the call without a result.
fn pair_server_blocks(content: &mut Vec<Value>) {
    let id_of = |block: &Value, key: &str| block.get(key).and_then(Value::as_str).map(str::to_string);
    let is_result =
        |block: &Value| block_type(block).is_some_and(|t| t.ends_with("_tool_result") && t != "tool_result");
    let calls: HashSet<String> = content
        .iter()
        .filter(|b| block_type(b) == Some("server_tool_use"))
        .filter_map(|b| id_of(b, "id"))
        .collect();
    let answered: HashSet<String> = content
        .iter()
        .filter(|b| is_result(b))
        .filter_map(|b| id_of(b, "tool_use_id"))
        .collect();
    content.retain(|block| {
        if block_type(block) == Some("server_tool_use") {
            id_of(block, "id").is_some_and(|id| answered.contains(&id))
        } else if is_result(block) {
            id_of(block, "tool_use_id").is_some_and(|id| calls.contains(&id))
        } else {
            true
        }
    });
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
            let mut definition =
                json!({"name": tool.name, "description": tool.description, "input_schema": schema});
            if tool.defer {
                definition["defer_loading"] = json!(true);
            }
            definition
        })
        .collect();
    // The tools come first, so a mark on the system prompt covers them too; theirs is left to
    // the conversation then. A deferred tool may not carry one.
    if *breakpoints == MAX_CACHE_BREAKPOINTS
        && let Some(last) = definitions
            .iter_mut()
            .rev()
            .find(|definition| definition.get("defer_loading").is_none())
    {
        mark_cached(last, breakpoints);
    }
    definitions
}

/// A server tool as the API names it. The search limits go on the tool itself.
fn hosted_definition(tool: &HostedTool) -> Value {
    match tool {
        HostedTool::WebSearch { domains, max_uses } => {
            let mut definition = Map::new();
            definition.insert("type".to_string(), json!(WEB_SEARCH_TOOL));
            definition.insert("name".to_string(), json!("web_search"));
            if let Some(max_uses) = max_uses {
                definition.insert("max_uses".to_string(), json!(max_uses));
            }
            if !domains.is_empty() {
                definition.insert("allowed_domains".to_string(), json!(domains));
            }
            Value::Object(definition)
        }
    }
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

/// How the refusal of a subscription whose usage window is full reads.
const EXTRA_USAGE: &str = "out of extra usage";

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
        // A subscription refuses requests it will not bill to the plan (some tool names, a used-up
        // usage window) with a 400 about extra usage. Retrying on the same account does not help,
        // but another account or another model in the chain may answer.
        _ if message.contains(EXTRA_USAGE) => Some(AiError::Quota {
            resets_at: None,
            message: message.to_string(),
        }),
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
    /// `stop_details` of a reply the safety system stopped: its category and explanation.
    refusal: Option<(Option<String>, Option<String>)>,
    response_id: Option<String>,
    /// The pages of a hosted web search: what its result blocks list and what the text cites.
    sources: Sources,
}

struct OpenBlock {
    index: usize,
    kind: OpenKind,
    /// The `web_search_result_location` citations of a text block.
    citations: Vec<Value>,
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
        if let Some(ttl) = usage.get("cache_creation").and_then(cache_ttl_secs) {
            self.usage.cache_ttl_secs = Some(ttl);
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
        let mut citations = Vec::new();
        let kind = match start.get("type").and_then(Value::as_str).unwrap_or_default() {
            "text" => {
                if let Some(Value::Array(given)) = start.get("citations") {
                    citations.clone_from(given);
                }
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
            other => {
                if other == "web_search_tool_result" {
                    // A failed search carries an error object here instead of a list.
                    let results = start.get("content").and_then(Value::as_array);
                    for result in results.into_iter().flatten() {
                        let field = |key: &str| result.get(key).and_then(Value::as_str).unwrap_or_default();
                        self.sources.found(field("url"), field("title"));
                    }
                }
                OpenKind::Opaque {
                    item: start.clone(),
                    json: String::new(),
                }
            }
        };
        self.open.insert(
            api_index,
            OpenBlock {
                index,
                kind,
                citations,
            },
        );
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
            ("citations_delta", OpenKind::Text(_)) => {
                if let Some(citation) = delta.get("citation") {
                    open.citations.push(citation.clone());
                }
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
            OpenKind::Text(text) => {
                for citation in &open.citations {
                    let field = |key: &str| citation.get(key).and_then(Value::as_str).unwrap_or_default();
                    // What the page says, or else what the answer says with it.
                    let quoted = Some(field("cited_text")).filter(|quoted| !quoted.trim().is_empty());
                    self.sources
                        .cite(field("url"), field("title"), quoted.unwrap_or(&text));
                }
                Block::Text { text }
            }
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

    fn meta(&mut self, stop: StopReason) -> AssistantMeta {
        AssistantMeta {
            provider: self.provider.clone(),
            model: self.model.clone(),
            api: WireApi::AnthropicMessages,
            usage: self.usage,
            stop,
            response_id: self.response_id.clone(),
            duration_ms: 0,
            sources: self.sources.finish(),
        }
    }

    fn done(&mut self) -> StreamEvent {
        if let Some((category, explanation)) = self.refusal.take() {
            let explanation =
                explanation.unwrap_or_else(|| "the provider's safety system stopped the reply".to_string());
            return self.fail(AiError::Refused {
                category,
                explanation,
            });
        }
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
                if stop_reason == Some("refusal") {
                    let details = payload.get("delta").and_then(|delta| delta.get("stop_details"));
                    let field = |key: &str| {
                        details
                            .and_then(|details| details.get(key))
                            .and_then(Value::as_str)
                            .map(str::to_string)
                    };
                    self.refusal = Some((field("category"), field("explanation")));
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
