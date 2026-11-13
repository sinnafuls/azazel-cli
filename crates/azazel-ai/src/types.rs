//! Provider-neutral message, request and model-reference types.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::catalog::Cost;

/// Key under which a wire decoder stores tool-call arguments that were not valid JSON:
/// `{"_invalid_json": "<raw text>"}`. The agent loop turns such a call into an error result.
pub const INVALID_JSON_KEY: &str = "_invalid_json";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
    Tool,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Block {
    Text {
        text: String,
    },
    Thinking {
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
        #[serde(default, skip_serializing_if = "is_false")]
        redacted: bool,
    },
    /// `data` is base64.
    Image {
        mime: String,
        data: String,
    },
    ToolCall {
        id: String,
        name: String,
        input: Value,
        /// Provider-issued signature that must accompany this call when it is replayed
        /// (Google thought signatures).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        signature: Option<String>,
    },
    /// `content` holds `Text` and `Image` blocks only.
    ToolResult {
        call_id: String,
        content: Vec<Block>,
        #[serde(default)]
        is_error: bool,
    },
    /// Provider-native item, replayed only to the provider that produced it.
    Opaque {
        provider: String,
        item: Value,
    },
}

impl Block {
    pub fn text(text: impl Into<String>) -> Self {
        Block::Text { text: text.into() }
    }

    pub fn tool_result(call_id: impl Into<String>, text: impl Into<String>, is_error: bool) -> Self {
        Block::ToolResult {
            call_id: call_id.into(),
            content: vec![Block::text(text)],
            is_error,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<Block>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub meta: Option<AssistantMeta>,
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message {
            role: Role::User,
            content: vec![Block::text(text)],
            meta: None,
        }
    }

    pub fn user_blocks(content: Vec<Block>) -> Self {
        Message {
            role: Role::User,
            content,
            meta: None,
        }
    }

    pub fn assistant(content: Vec<Block>, meta: AssistantMeta) -> Self {
        Message {
            role: Role::Assistant,
            content,
            meta: Some(meta),
        }
    }

    /// A `Tool` message; every block must be a `ToolResult`.
    pub fn tool(results: Vec<Block>) -> Self {
        Message {
            role: Role::Tool,
            content: results,
            meta: None,
        }
    }

    /// Concatenation of the top-level `Text` blocks.
    pub fn text(&self) -> String {
        let mut out = String::new();
        for block in &self.content {
            if let Block::Text { text } = block {
                out.push_str(text);
            }
        }
        out
    }

    /// `(id, name, input)` of each tool call, in order.
    pub fn tool_calls(&self) -> impl Iterator<Item = (&str, &str, &Value)> {
        self.content.iter().filter_map(|b| match b {
            Block::ToolCall { id, name, input, .. } => Some((id.as_str(), name.as_str(), input)),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantMeta {
    pub provider: String,
    pub model: String,
    pub api: WireApi,
    #[serde(default)]
    pub usage: Usage,
    pub stop: StopReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_id: Option<String>,
    #[serde(default)]
    pub duration_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

/// Token counts for one response. `input` excludes cached tokens, which are counted in `cache_read`
/// and `cache_write`; `reasoning` is the part of `output` spent on reasoning.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub reasoning: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
}

impl Usage {
    /// Fills `cost_usd` from per-million-token rates.
    pub fn price(&mut self, cost: &Cost) {
        let total = self.input as f64 * cost.input
            + self.output as f64 * cost.output
            + self.cache_read as f64 * cost.cache_read
            + self.cache_write as f64 * cost.cache_write;
        self.cost_usd = Some(total / 1_000_000.0);
    }

    /// Tokens the next request will carry forward as context.
    pub fn context_tokens(&self) -> u64 {
        self.input + self.cache_read + self.cache_write + self.output
    }

    /// Adds `other` to these totals; a known cost on either side keeps the sum known.
    pub fn add(&mut self, other: &Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.reasoning += other.reasoning;
        if let Some(cost) = other.cost_usd {
            self.cost_usd = Some(self.cost_usd.unwrap_or(0.0) + cost);
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    #[default]
    Off,
    Minimal,
    Low,
    Medium,
    High,
    Xhigh,
    Max,
}

impl ThinkingLevel {
    pub const ALL: [ThinkingLevel; 7] = [
        ThinkingLevel::Off,
        ThinkingLevel::Minimal,
        ThinkingLevel::Low,
        ThinkingLevel::Medium,
        ThinkingLevel::High,
        ThinkingLevel::Xhigh,
        ThinkingLevel::Max,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ThinkingLevel::Off => "off",
            ThinkingLevel::Minimal => "minimal",
            ThinkingLevel::Low => "low",
            ThinkingLevel::Medium => "medium",
            ThinkingLevel::High => "high",
            ThinkingLevel::Xhigh => "xhigh",
            ThinkingLevel::Max => "max",
        }
    }
}

impl fmt::Display for ThinkingLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for ThinkingLevel {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        ThinkingLevel::ALL
            .into_iter()
            .find(|l| l.as_str() == s)
            .ok_or_else(|| ParseError(format!("unknown thinking level `{s}`")))
    }
}

/// How fast, and at what price, a provider should serve a request. Providers without tiers
/// ignore it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    /// Faster output at a premium: OpenAI priority processing, Anthropic fast mode.
    Priority,
    /// The provider's default; the same as naming no tier.
    Standard,
    /// Cheaper and slower: OpenAI flex processing.
    Economy,
}

impl Tier {
    pub const ALL: [Tier; 3] = [Tier::Priority, Tier::Standard, Tier::Economy];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Priority => "priority",
            Tier::Standard => "standard",
            Tier::Economy => "economy",
        }
    }
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

impl FromStr for Tier {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Tier::ALL
            .into_iter()
            .find(|tier| tier.as_str() == s)
            .ok_or_else(|| ParseError(format!("unknown tier `{s}`: use priority, standard or economy")))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum WireApi {
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    #[serde(rename = "openai-chat")]
    OpenAiChat,
    #[serde(rename = "google-generative")]
    GoogleGenerative,
    #[serde(rename = "bedrock-converse")]
    BedrockConverse,
    /// Ollama's own chat endpoint, `{base_url}/api/chat`, answered as JSON lines.
    #[serde(rename = "ollama-chat")]
    OllamaChat,
    /// Chat Completions as OpenRouter extends it: a `reasoning` object, cache breakpoints for
    /// the models that take them, and the price of the response in its usage.
    #[serde(rename = "openrouter")]
    OpenRouter,
    /// The Responses format at an Azure OpenAI resource, where the model is a deployment.
    #[serde(rename = "azure-openai-responses")]
    AzureOpenAiResponses,
    /// Chat Completions at an Azure OpenAI resource, addressed by deployment.
    #[serde(rename = "azure-openai-chat")]
    AzureOpenAiChat,
    /// Google Cloud Code Assist, the endpoint behind a Gemini CLI sign-in: the Generative
    /// Language format inside a project envelope.
    #[serde(rename = "google-gemini-cli")]
    GoogleGeminiCli,
}

impl WireApi {
    /// Every wire API, in the order they are documented.
    pub const ALL: [WireApi; 10] = [
        WireApi::AnthropicMessages,
        WireApi::OpenAiResponses,
        WireApi::OpenAiChat,
        WireApi::GoogleGenerative,
        WireApi::BedrockConverse,
        WireApi::OllamaChat,
        WireApi::OpenRouter,
        WireApi::AzureOpenAiResponses,
        WireApi::AzureOpenAiChat,
        WireApi::GoogleGeminiCli,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            WireApi::AnthropicMessages => "anthropic-messages",
            WireApi::OpenAiResponses => "openai-responses",
            WireApi::OpenAiChat => "openai-chat",
            WireApi::GoogleGenerative => "google-generative",
            WireApi::BedrockConverse => "bedrock-converse",
            WireApi::OllamaChat => "ollama-chat",
            WireApi::OpenRouter => "openrouter",
            WireApi::AzureOpenAiResponses => "azure-openai-responses",
            WireApi::AzureOpenAiChat => "azure-openai-chat",
            WireApi::GoogleGeminiCli => "google-gemini-cli",
        }
    }

    /// Whether the API rejects two consecutive messages with the same role.
    pub fn requires_alternation(self) -> bool {
        matches!(
            self,
            WireApi::AnthropicMessages
                | WireApi::GoogleGenerative
                | WireApi::BedrockConverse
                | WireApi::GoogleGeminiCli
        )
    }

    /// Whether requests are written in one of the OpenAI formats, which share their headers,
    /// model list and error shapes.
    pub fn is_openai_format(self) -> bool {
        matches!(
            self,
            WireApi::OpenAiResponses
                | WireApi::OpenAiChat
                | WireApi::OpenRouter
                | WireApi::AzureOpenAiResponses
                | WireApi::AzureOpenAiChat
        )
    }
}

impl fmt::Display for WireApi {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub system: Vec<SystemPart>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    #[serde(default)]
    pub thinking: ThinkingLevel,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_key: Option<String>,
    /// The service tier asked for; `None` leaves it to the provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tier: Option<Tier>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SystemPart {
    pub text: String,
    /// Which part of the prompt this is (`identity`, `skills`, `memory`, ...), for whoever
    /// reports what a request is made of. Empty when the part was not built under a name. Never
    /// sent to a provider.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub name: String,
    /// Marks the end of the prefix that stays byte-stable across requests.
    #[serde(default, skip_serializing_if = "is_false")]
    pub cache_boundary: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON Schema of the input object.
    pub schema: Value,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct ParseError(pub String);

/// A concrete model: `provider/model[:level]`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ModelRef {
    pub provider: String,
    pub model: String,
    pub thinking: Option<ThinkingLevel>,
}

impl ModelRef {
    pub fn new(provider: impl Into<String>, model: impl Into<String>) -> Self {
        ModelRef {
            provider: provider.into(),
            model: model.into(),
            thinking: None,
        }
    }

    /// `provider/model` without the thinking suffix; the catalogue and cooldown key.
    pub fn key(&self) -> String {
        format!("{}/{}", self.provider, self.model)
    }
}

impl fmt::Display for ModelRef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}/{}", self.provider, self.model)?;
        if let Some(level) = self.thinking {
            write!(f, ":{level}")?;
        }
        Ok(())
    }
}

impl FromStr for ModelRef {
    type Err = ParseError;

    /// The provider is everything before the first `/`. Model ids may themselves contain `/` and `:`,
    /// so a trailing `:suffix` is a thinking level only when it names one.
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (provider, rest) = s
            .split_once('/')
            .filter(|(p, m)| !p.is_empty() && !m.is_empty())
            .ok_or_else(|| ParseError(format!("expected `provider/model`, got `{s}`")))?;
        let (model, thinking) = match rest.rsplit_once(':') {
            Some((model, suffix)) if !model.is_empty() => match suffix.parse::<ThinkingLevel>() {
                Ok(level) => (model, Some(level)),
                Err(_) => (rest, None),
            },
            _ => (rest, None),
        };
        Ok(ModelRef {
            provider: provider.to_string(),
            model: model.to_string(),
            thinking,
        })
    }
}

impl TryFrom<String> for ModelRef {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModelRef> for String {
    fn from(m: ModelRef) -> String {
        m.to_string()
    }
}

/// One entry of a fallback chain: a role (`@smol`) or a concrete model.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub enum ModelSpec {
    Role(String),
    Model(ModelRef),
}

impl fmt::Display for ModelSpec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ModelSpec::Role(role) => write!(f, "@{role}"),
            ModelSpec::Model(model) => model.fmt(f),
        }
    }
}

impl FromStr for ModelSpec {
    type Err = ParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.strip_prefix('@') {
            Some("") => Err(ParseError("empty role name".to_string())),
            Some(role) => Ok(ModelSpec::Role(role.to_string())),
            None => s.parse().map(ModelSpec::Model),
        }
    }
}

impl TryFrom<String> for ModelSpec {
    type Error = ParseError;

    fn try_from(s: String) -> Result<Self, Self::Error> {
        s.parse()
    }
}

impl From<ModelSpec> for String {
    fn from(m: ModelSpec) -> String {
        m.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_parses_provider_model_and_level() {
        let m: ModelRef = "mock/alpha:high".parse().unwrap();
        assert_eq!(
            (m.provider.as_str(), m.model.as_str(), m.thinking),
            ("mock", "alpha", Some(ThinkingLevel::High))
        );
        assert_eq!(m.to_string(), "mock/alpha:high");
    }

    #[test]
    fn model_ref_keeps_slashes_and_non_level_colons_in_model_id() {
        let m: ModelRef = "router/vendor/name:free".parse().unwrap();
        assert_eq!(
            (m.provider.as_str(), m.model.as_str(), m.thinking),
            ("router", "vendor/name:free", None)
        );
        let m: ModelRef = "local/name:8b:low".parse().unwrap();
        assert_eq!(
            (m.model.as_str(), m.thinking),
            ("name:8b", Some(ThinkingLevel::Low))
        );
    }

    #[test]
    fn model_ref_rejects_missing_parts() {
        for bad in ["mock", "/x", "mock/", ""] {
            assert!(bad.parse::<ModelRef>().is_err(), "{bad}");
        }
    }

    #[test]
    fn model_spec_round_trips_through_serde_as_string() {
        let chain: Vec<ModelSpec> = serde_json::from_str(r#"["@smol","mock/a"]"#).unwrap();
        assert_eq!(
            chain,
            vec![
                ModelSpec::Role("smol".into()),
                ModelSpec::Model(ModelRef::new("mock", "a"))
            ]
        );
        assert_eq!(serde_json::to_string(&chain).unwrap(), r#"["@smol","mock/a"]"#);
        assert!(serde_json::from_str::<ModelSpec>(r#""@""#).is_err());
    }

    #[test]
    fn block_serialises_with_type_tag_and_omits_empty_optionals() {
        let block = Block::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: serde_json::json!({"path": "a"}),
            signature: None,
        };
        assert_eq!(
            serde_json::to_value(&block).unwrap(),
            serde_json::json!({"type": "tool_call", "id": "c1", "name": "read", "input": {"path": "a"}})
        );
        let msg = Message::user("hi");
        assert_eq!(
            serde_json::to_value(&msg).unwrap(),
            serde_json::json!({"role": "user", "content": [{"type": "text", "text": "hi"}]})
        );
    }

    #[test]
    fn usage_price_uses_per_million_rates() {
        let mut usage = Usage {
            input: 1_000_000,
            output: 500_000,
            cache_read: 2_000_000,
            ..Usage::default()
        };
        usage.price(&Cost {
            input: 3.0,
            output: 15.0,
            cache_read: 0.3,
            cache_write: 3.75,
        });
        assert_eq!(usage.cost_usd, Some(3.0 + 7.5 + 0.6));
    }
}
