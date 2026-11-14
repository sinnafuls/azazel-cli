//! Built-in provider endpoints and their merge with configured provider settings
//! (`[providers.<id>]` in `config.toml`, `providers:` in `models.yml`).
//!
//! `base_url` conventions the wire modules rely on:
//! - `anthropic-messages`: `{base_url}/v1/messages`, `{base_url}/v1/models`
//! - `openai-responses`: `{base_url}/responses`, `{base_url}/models`
//! - `openai-chat`, `openrouter`: `{base_url}/chat/completions`, `{base_url}/models`
//! - `google-generative`: `{base_url}/models/{id}:streamGenerateContent?alt=sse`, `{base_url}/models`
//! - `ollama-chat`: `{root}/api/chat`, where `root` is `base_url` without a trailing `/v1` or `/api`
//! - `azure-openai-responses`: `{resource}/openai/v1/responses`
//! - `azure-openai-chat`: `{resource}/openai/deployments/{deployment}/chat/completions`
//! - `google-gemini-cli`: `{base_url}/v1internal:streamGenerateContent?alt=sse`

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value};

use crate::oauth::OAuthSettings;
use crate::providers::generated::GENERATED;
use crate::providers::logins;
use crate::providers::table::{PRESETS, Preset};
use crate::types::WireApi;

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub id: String,
    /// What the provider is called in lists; its id when it has no other name.
    pub name: String,
    pub api: WireApi,
    /// No trailing slash. A `${VAR}` that is still in it names a variable the address needs and
    /// the environment does not have (see [`ProviderConfig::unset_variable`]).
    pub base_url: String,
    /// Environment variables tried in order.
    pub api_key_env: Vec<String>,
    /// Shell command whose stdout is the key.
    pub api_key_command: Option<String>,
    /// A key written in the configuration itself.
    pub api_key: Option<String>,
    /// Extra headers sent with every request.
    pub headers: BTreeMap<String, String>,
    /// Headers sent only when the credential is a login, with `{field}` holes filled from it.
    pub oauth_headers: BTreeMap<String, String>,
    /// When false, a request without credentials is sent unauthenticated (local servers).
    pub requires_key: bool,
    /// How the key is presented.
    pub auth_style: AuthStyle,
    /// The authorisation server `azazel login` signs in with, when the provider has one.
    pub oauth: Option<OAuthSettings>,
    /// How the provider's models are listed.
    pub discovery: Discovery,
    /// Members written into every request body after the wire API has built it, replacing what
    /// it wrote under the same name: routing preferences and vendor switches.
    pub extra_body: Map<String, Value>,
}

impl ProviderConfig {
    /// A provider at `base_url` that needs no key, lists its models the wire API's way and sends
    /// nothing extra.
    pub fn new(id: impl Into<String>, api: WireApi, base_url: impl Into<String>) -> Self {
        let id = id.into();
        ProviderConfig {
            name: id.clone(),
            id,
            api,
            base_url: base_url.into(),
            api_key_env: Vec::new(),
            api_key_command: None,
            api_key: None,
            headers: BTreeMap::new(),
            oauth_headers: BTreeMap::new(),
            requires_key: false,
            auth_style: AuthStyle::Auto,
            oauth: None,
            discovery: Discovery::Auto,
            extra_body: Map::new(),
        }
    }

    /// The environment variable the address still needs, when one is unset: a provider whose
    /// endpoint is per account (a Cloudflare account, an Azure resource) cannot be reached
    /// before it is named.
    pub fn unset_variable(&self) -> Option<&str> {
        let start = self.base_url.find("${")? + 2;
        let rest = &self.base_url[start..];
        let end = rest.find([':', '}']).unwrap_or(rest.len());
        Some(&rest[..end])
    }
}

/// How a provider's key is put on a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthStyle {
    /// What the wire API prescribes: a bearer token for the OpenAI formats and Ollama,
    /// `x-api-key` for Anthropic, `x-goog-api-key` for Google.
    #[default]
    Auto,
    /// `Authorization: Bearer <key>`, whatever the wire API.
    Bearer,
    /// An `api-key: <key>` header, as Azure OpenAI takes resource keys.
    ApiKey,
}

/// How a provider's models are found for the pickers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Discovery {
    /// The model list of the wire API: `GET {base_url}/models` and its relatives.
    #[default]
    Auto,
    /// Ollama's own `/api/tags`, then `/api/show` per model for its context window and
    /// capabilities.
    Ollama,
    /// LM Studio's own `/api/v0/models`, which knows the loaded context window, falling back to
    /// the OpenAI list.
    LmStudio,
    /// Never asked: only models from the catalogue and the configuration are listed.
    Off,
}

impl Discovery {
    const NAMES: [(&'static str, Discovery); 4] = [
        ("auto", Discovery::Auto),
        ("ollama", Discovery::Ollama),
        ("lm-studio", Discovery::LmStudio),
        ("off", Discovery::Off),
    ];

    pub fn as_str(self) -> &'static str {
        Self::NAMES
            .iter()
            .find(|(_, kind)| *kind == self)
            .map_or("auto", |(name, _)| name)
    }
}

impl Serialize for Discovery {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for Discovery {
    /// A name, or a boolean: `true` is `auto` and `false` is `off`.
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Flag(bool),
            Name(String),
        }
        match Raw::deserialize(deserializer)? {
            Raw::Flag(true) => Ok(Discovery::Auto),
            Raw::Flag(false) => Ok(Discovery::Off),
            Raw::Name(name) => Discovery::NAMES
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, kind)| *kind)
                .ok_or_else(|| {
                    serde::de::Error::custom(format!(
                        "unknown discovery `{name}`, expected auto, ollama, lm-studio or off"
                    ))
                }),
        }
    }
}

/// A `[providers.<id>]` config table or a `providers.<id>` entry of `models.yml`. Overrides a
/// preset field by field, or defines a new provider when `api` and `base_url` are both present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api: Option<WireApi>,
    /// May contain `${VAR}` and `${VAR:-default}`, filled from the environment.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_env: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key_command: Option<String>,
    /// The key itself. Prefer `api_key_env` or `api_key_command` for anything that is shared.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Values may contain `${VAR}`; a header whose variable is unset is not sent.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub oauth_headers: BTreeMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthStyle>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discovery: Option<Discovery>,
    /// Members written into every request body; see [`ProviderConfig::extra_body`].
    #[serde(default, skip_serializing_if = "Map::is_empty")]
    pub extra_body: Map<String, Value>,
    /// AWS region, for `bedrock`; the SDK's default chain decides when unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub region: Option<String>,
    /// AWS profile, for `bedrock`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub profile: Option<String>,
    /// Google Cloud project, for `vertex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    /// Google Cloud location, for `vertex`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub location: Option<String>,
    /// `[providers.<id>.oauth]`: sign in with an authorisation server instead of a key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub oauth: Option<OAuthSettings>,
    /// The label of the stored account to use when a session selects none; the default account
    /// when unset. See `Credentials::prefer`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account: Option<String>,
}

/// Looks an environment variable up; the process environment in production.
pub type Env<'a> = &'a dyn Fn(&str) -> Option<String>;

fn process_env(name: &str) -> Option<String> {
    std::env::var(name).ok()
}

/// `text` with `${VAR}` and `${VAR:-default}` filled from `env`. A default may itself hold
/// variables. A variable that is unset or blank and has no default stays as it was written, so
/// the caller can say which one is missing.
pub fn expand(text: &str, env: Env<'_>) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = closing_brace(after) else {
            out.push_str(&rest[start..]);
            return out;
        };
        let inner = &after[..end];
        let (name, default) = match inner.split_once(":-") {
            Some((name, default)) => (name, Some(default)),
            None => (inner, None),
        };
        match (env(name).filter(|value| !value.trim().is_empty()), default) {
            (Some(value), _) => out.push_str(value.trim()),
            (None, Some(default)) => out.push_str(&expand(default, env)),
            (None, None) => {
                out.push_str("${");
                out.push_str(inner);
                out.push('}');
            }
        }
        rest = &after[end + 1..];
    }
    out.push_str(rest);
    out
}

/// Where the `}` that closes a `${` sits in the text after it, counting the ones nested inside.
fn closing_brace(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    let mut at = 0;
    while at < bytes.len() {
        match bytes[at] {
            b'$' if bytes.get(at + 1) == Some(&b'{') => {
                depth += 1;
                at += 1;
            }
            b'}' if depth == 0 => return Some(at),
            b'}' => depth -= 1,
            _ => {}
        }
        at += 1;
    }
    None
}

/// The headers of `configured` with their variables filled; a header that still needs one is
/// left out.
fn expand_headers(configured: &BTreeMap<String, String>, env: Env<'_>) -> BTreeMap<String, String> {
    configured
        .iter()
        .map(|(name, value)| (name.clone(), expand(value, env)))
        .filter(|(_, value)| !value.contains("${"))
        .collect()
}

fn from_row(row: &Preset, env: Env<'_>) -> ProviderConfig {
    let headers: BTreeMap<String, String> = row
        .headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect();
    let (oauth, oauth_headers) = match logins::login(row.id) {
        Some((oauth, headers)) => (Some(oauth), headers),
        None => (None, BTreeMap::new()),
    };
    ProviderConfig {
        name: row.name.to_string(),
        api_key_env: row.env.iter().map(|name| name.to_string()).collect(),
        headers: expand_headers(&headers, env),
        oauth_headers,
        requires_key: !row.keyless,
        auth_style: row.auth,
        oauth,
        discovery: row.discovery,
        ..ProviderConfig::new(row.id, row.api, trim_url(&expand(row.base_url, env)))
    }
}

/// The built-in rows, sorted by id: every hand-written one, and each generated one that no
/// hand-written row covers.
fn rows() -> Vec<&'static Preset> {
    merge(PRESETS, GENERATED)
}

/// `hand` and the rows of `generated` that none of `hand` covers, by id or by the models.dev
/// provider it takes its models from, sorted by id. A hand-written row carries what models.dev
/// does not say (a sign-in, headers, a quirk), and the generated file may predate it.
fn merge<'a>(hand: &'a [Preset], generated: &'a [Preset]) -> Vec<&'a Preset> {
    let covered = |id: &str| hand.iter().any(|row| row.id == id || row.catalog == id);
    let mut rows: Vec<&Preset> = hand
        .iter()
        .chain(generated.iter().filter(|row| !covered(row.id)))
        .collect();
    rows.sort_by_key(|row| row.id);
    rows
}

/// The built-in providers, sorted by id, with the process environment filled into their
/// addresses.
pub fn presets() -> Vec<ProviderConfig> {
    presets_with(&process_env)
}

pub fn presets_with(env: Env<'_>) -> Vec<ProviderConfig> {
    rows().into_iter().map(|row| from_row(row, env)).collect()
}

/// The hand-written providers as `(provider id, name on models.dev, key variables)`; the name is
/// empty when the catalogue snapshot takes no models for it. `cargo xtask catalog` generates rows
/// for the other models.dev providers.
pub fn hand_written() -> Vec<(&'static str, &'static str, &'static [&'static str])> {
    PRESETS.iter().map(|row| (row.id, row.catalog, row.env)).collect()
}

/// The provider id under which an Azure OpenAI resource is configured.
pub const AZURE: &str = "azure";
/// The provider ids of the cloud services reached with the platform's own credentials.
pub const BEDROCK: &str = "bedrock";
pub const VERTEX: &str = "vertex";

/// Whether `id` names a provider that exists without any configuration.
pub fn is_builtin(id: &str) -> bool {
    id == BEDROCK || id == VERTEX || rows().iter().any(|row| row.id == id)
}

/// Presets with `settings` applied, sorted by id. A new provider needs `api` and `base_url`; it
/// requires a key only when it names a key source.
pub fn resolve(settings: &BTreeMap<String, ProviderSettings>) -> Result<Vec<ProviderConfig>, String> {
    resolve_with(settings, &process_env)
}

/// [`resolve`] against `env` instead of the process environment.
pub fn resolve_with(
    settings: &BTreeMap<String, ProviderSettings>,
    env: Env<'_>,
) -> Result<Vec<ProviderConfig>, String> {
    let mut providers: BTreeMap<String, ProviderConfig> =
        presets_with(env).into_iter().map(|p| (p.id.clone(), p)).collect();
    for (id, s) in settings {
        // The cloud providers are not HTTP endpoints with a key; they read their own settings.
        if id == BEDROCK || id == VERTEX {
            continue;
        }
        if let Some(oauth) = &s.oauth {
            oauth
                .check()
                .map_err(|why| format!("[providers.{id}.oauth]: {why}"))?;
        }
        let filled = |value: &Option<String>| value.clone().filter(|text| !text.trim().is_empty());
        let key_env = filled(&s.api_key_env);
        let command = filled(&s.api_key_command);
        let literal = filled(&s.api_key);
        let names_key = key_env.is_some() || command.is_some() || literal.is_some() || s.oauth.is_some();
        if !providers.contains_key(id) {
            let (Some(api), Some(_)) = (s.api, &s.base_url) else {
                return Err(format!(
                    "provider `{id}` is not built in: set both `api` and `base_url`"
                ));
            };
            let mut new = ProviderConfig::new(id.clone(), api, String::new());
            new.requires_key = names_key;
            providers.insert(id.clone(), new);
        }
        let Some(provider) = providers.get_mut(id) else {
            continue;
        };
        if let Some(api) = s.api {
            provider.api = api;
        }
        if let Some(url) = &s.base_url {
            provider.base_url = trim_url(&expand(url, env));
        }
        if let Some(name) = filled(&s.name) {
            provider.name = name;
        }
        if let Some(key_env) = key_env {
            provider.api_key_env = vec![key_env];
        }
        if command.is_some() {
            provider.api_key_command = command;
        }
        if literal.is_some() {
            provider.api_key = literal;
        }
        provider.headers.extend(expand_headers(&s.headers, env));
        provider.oauth_headers.extend(s.oauth_headers.clone());
        if let Some(style) = s.auth {
            provider.auth_style = style;
        }
        if s.oauth.is_some() {
            provider.oauth = s.oauth.clone();
        }
        if let Some(discovery) = s.discovery {
            provider.discovery = discovery;
        }
        provider.extra_body.extend(s.extra_body.clone());
    }
    Ok(providers.into_values().collect())
}

fn trim_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::oauth::{BodyKind, OAuthFlow};
    use crate::providers::table::row;

    fn no_env(_: &str) -> Option<String> {
        None
    }

    fn with(settings: ProviderSettings, id: &str, env: Env<'_>) -> ProviderConfig {
        let all = resolve_with(&BTreeMap::from([(id.to_string(), settings)]), env).unwrap();
        all.into_iter().find(|p| p.id == id).unwrap()
    }

    fn preset(id: &str) -> ProviderConfig {
        presets_with(&no_env)
            .into_iter()
            .find(|p| p.id == id)
            .unwrap_or_else(|| panic!("no preset `{id}`"))
    }

    #[test]
    fn the_table_is_sorted_and_every_row_is_usable() {
        for table in [PRESETS, GENERATED] {
            let ids: Vec<&str> = table.iter().map(|row| row.id).collect();
            let mut sorted = ids.clone();
            sorted.sort_unstable();
            sorted.dedup();
            assert_eq!(ids, sorted, "rows must be sorted by id, each id once");
        }
        for provider in presets_with(&no_env) {
            assert!(!provider.name.is_empty(), "{}", provider.id);
            // An address may start with the variable that names it.
            assert!(
                (provider.base_url.starts_with("http") || provider.unset_variable().is_some())
                    && !provider.base_url.ends_with('/'),
                "{}: {}",
                provider.id,
                provider.base_url
            );
            // A provider is reachable with a key variable, a login, or no key at all.
            assert!(
                !provider.requires_key || !provider.api_key_env.is_empty() || provider.oauth.is_some(),
                "{} has no way to get a credential",
                provider.id
            );
            if let Some(oauth) = &provider.oauth {
                oauth
                    .check()
                    .unwrap_or_else(|why| panic!("{}: {why}", provider.id));
            }
        }
    }

    #[test]
    fn presets_carry_the_endpoint_facts_of_their_provider() {
        let cerebras = preset("cerebras");
        assert_eq!(
            (cerebras.api, cerebras.base_url.as_str(), cerebras.requires_key),
            (WireApi::OpenAiChat, "https://api.cerebras.ai/v1", true)
        );
        assert_eq!(cerebras.api_key_env, ["CEREBRAS_API_KEY"]);
        assert_eq!(cerebras.name, "Cerebras");

        let minimax = preset("minimax");
        assert_eq!(
            (minimax.api, minimax.base_url.as_str()),
            (WireApi::AnthropicMessages, "https://api.minimax.io/anthropic")
        );

        let openrouter = preset("openrouter");
        assert_eq!(openrouter.api, WireApi::OpenRouter);
        assert_eq!(openrouter.headers["X-Title"], "azazel");

        let cloud = preset("ollama-cloud");
        assert_eq!(
            (cloud.api, cloud.discovery, cloud.requires_key),
            (WireApi::OllamaChat, Discovery::Ollama, true)
        );
        assert_eq!(cloud.api_key_env, ["OLLAMA_CLOUD_API_KEY", "OLLAMA_API_KEY"]);

        // Local servers answer without a key and are asked for their models their own way.
        for (id, discovery) in [
            ("ollama", Discovery::Ollama),
            ("lmstudio", Discovery::LmStudio),
            ("llamacpp", Discovery::Auto),
            ("vllm", Discovery::Auto),
            ("litellm", Discovery::Auto),
        ] {
            let local = preset(id);
            assert!(!local.requires_key, "{id}");
            assert_eq!(local.discovery, discovery, "{id}");
            assert!(local.base_url.starts_with("http://localhost:"), "{id}");
        }

        let azure = preset(AZURE);
        assert_eq!(
            (azure.api, azure.auth_style),
            (WireApi::AzureOpenAiResponses, AuthStyle::ApiKey)
        );
        assert_eq!(azure.api_key_env, ["AZURE_OPENAI_API_KEY"]);
    }

    #[test]
    fn an_address_takes_its_account_from_the_environment() {
        // Unset, the address says which variable it is waiting for.
        let gateway = preset("cloudflare-ai-gateway");
        assert_eq!(gateway.unset_variable(), Some("CLOUDFLARE_ACCOUNT_ID"));
        assert_eq!(preset(AZURE).unset_variable(), Some("AZURE_OPENAI_RESOURCE_NAME"));
        assert_eq!(preset("openai").unset_variable(), None);

        let env = |name: &str| match name {
            "CLOUDFLARE_ACCOUNT_ID" => Some("acct".to_string()),
            "CLOUDFLARE_GATEWAY_ID" => Some("gw".to_string()),
            "AZURE_OPENAI_RESOURCE_NAME" => Some("res".to_string()),
            "AWS_REGION" => Some("eu-west-1".to_string()),
            "COREWEAVE_PROJECT" => Some("team/proj".to_string()),
            _ => None,
        };
        let all = presets_with(&env);
        let url = |id: &str| all.iter().find(|p| p.id == id).unwrap().base_url.clone();
        assert_eq!(
            url("cloudflare-ai-gateway"),
            "https://gateway.ai.cloudflare.com/v1/acct/gw/compat"
        );
        assert_eq!(url(AZURE), "https://res.openai.azure.com");
        assert_eq!(
            url("bedrock-mantle"),
            "https://bedrock-mantle.eu-west-1.api.aws/v1"
        );
        let coreweave = all.iter().find(|p| p.id == "coreweave").unwrap();
        assert_eq!(coreweave.headers["OpenAI-Project"], "team/proj");

        // A default stands in for an unset variable, and a whole-address variable replaces it.
        assert_eq!(
            preset("bedrock-mantle").base_url,
            "https://bedrock-mantle.us-east-1.api.aws/v1"
        );
        assert!(!preset("coreweave").headers.contains_key("OpenAI-Project"));
        let moved =
            |name: &str| (name == "AZURE_OPENAI_BASE_URL").then(|| "https://gw.example/azure/".to_string());
        assert_eq!(
            presets_with(&moved)
                .into_iter()
                .find(|p| p.id == AZURE)
                .unwrap()
                .base_url,
            "https://gw.example/azure"
        );
    }

    #[test]
    fn expansion_handles_defaults_nesting_and_text_that_is_not_a_variable() {
        let env = |name: &str| (name == "A").then(|| " one ".to_string());
        assert_eq!(expand("x-${A}-y", &env), "x-one-y");
        assert_eq!(expand("${B:-two}", &env), "two");
        assert_eq!(expand("${B:-${A}/${C:-three}}", &env), "one/three");
        assert_eq!(expand("${B}", &env), "${B}");
        assert_eq!(expand("plain ${ unclosed", &env), "plain ${ unclosed");
    }

    #[test]
    fn override_changes_only_named_fields() {
        let ollama = with(
            ProviderSettings {
                base_url: Some("http://box:11434/v1/".into()),
                ..Default::default()
            },
            "ollama",
            &no_env,
        );
        assert_eq!(ollama.base_url, "http://box:11434/v1");
        assert_eq!(ollama.api, WireApi::OpenAiChat);
        assert_eq!(ollama.discovery, Discovery::Ollama);
        assert!(!ollama.requires_key);
    }

    #[test]
    fn settings_reach_every_field_of_a_provider() {
        let env = |name: &str| (name == "TEAM").then(|| "platform".to_string());
        let gateway = with(
            ProviderSettings {
                name: Some("Our gateway".into()),
                api: Some(WireApi::OpenRouter),
                base_url: Some("https://${HOST:-llm.example.com}/v1".into()),
                api_key: Some("sk-literal".into()),
                headers: BTreeMap::from([
                    ("X-Team".to_string(), "${TEAM}".to_string()),
                    ("X-Unset".to_string(), "${NOPE}".to_string()),
                ]),
                discovery: Some(Discovery::Off),
                extra_body: json!({"provider": {"order": ["a", "b"]}})
                    .as_object()
                    .cloned()
                    .unwrap(),
                ..Default::default()
            },
            "gateway",
            &env,
        );
        assert_eq!(gateway.name, "Our gateway");
        assert_eq!(gateway.base_url, "https://llm.example.com/v1");
        assert_eq!(gateway.api_key.as_deref(), Some("sk-literal"));
        assert!(gateway.requires_key);
        assert_eq!(
            gateway.headers,
            BTreeMap::from([("X-Team".to_string(), "platform".to_string())])
        );
        assert_eq!(gateway.discovery, Discovery::Off);
        assert_eq!(gateway.extra_body["provider"]["order"][1], "b");

        // The same members on a preset are added to what it already sends.
        let routed = with(
            ProviderSettings {
                extra_body: json!({"provider": {"only": ["x"]}}).as_object().cloned().unwrap(),
                ..Default::default()
            },
            "openrouter",
            &no_env,
        );
        assert_eq!(routed.extra_body["provider"]["only"][0], "x");
        assert_eq!(routed.headers["X-Title"], "azazel");
    }

    #[test]
    fn new_provider_needs_api_and_base_url() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "mine".to_string(),
            ProviderSettings {
                api: Some(WireApi::OpenAiChat),
                ..Default::default()
            },
        );
        assert!(resolve_with(&settings, &no_env).unwrap_err().contains("mine"));

        let mine = with(
            ProviderSettings {
                api: Some(WireApi::AnthropicMessages),
                base_url: Some("https://gw.example/anthropic".into()),
                api_key_env: Some("MINE_KEY".into()),
                ..Default::default()
            },
            "mine",
            &no_env,
        );
        assert!(mine.requires_key);
        assert_eq!(mine.api_key_env, vec!["MINE_KEY".to_string()]);
        assert_eq!(mine.name, "mine");
    }

    #[test]
    fn empty_key_command_is_ignored() {
        let local = with(
            ProviderSettings {
                api: Some(WireApi::OpenAiChat),
                base_url: Some("http://localhost:9000/v1".into()),
                api_key_command: Some(String::new()),
                ..Default::default()
            },
            "local",
            &no_env,
        );
        assert_eq!(
            (local.api_key_command.as_deref(), local.requires_key),
            (None, false)
        );
    }

    #[test]
    fn azure_takes_resource_keys_in_their_own_header_unless_told_to_use_a_bearer_token() {
        // A resource configured the old way keeps talking the plain Responses format.
        let base = ProviderSettings {
            api: Some(WireApi::OpenAiResponses),
            base_url: Some("https://res.openai.azure.com/openai/v1/".into()),
            ..Default::default()
        };
        let keyed = with(base.clone(), AZURE, &no_env);
        assert_eq!(keyed.api, WireApi::OpenAiResponses);
        assert_eq!(keyed.auth_style, AuthStyle::ApiKey);
        assert_eq!(keyed.api_key_env, ["AZURE_OPENAI_API_KEY"]);
        assert!(keyed.requires_key);
        assert_eq!(keyed.base_url, "https://res.openai.azure.com/openai/v1");

        let entra = with(
            ProviderSettings {
                auth: Some(AuthStyle::Bearer),
                api_key_command: Some("az account get-access-token --query accessToken -o tsv".into()),
                ..base
            },
            AZURE,
            &no_env,
        );
        assert_eq!(entra.auth_style, AuthStyle::Bearer);
        assert!(entra.api_key_command.is_some());

        // Other providers present the key the way their wire API prescribes, and can be switched.
        let all = resolve_with(&BTreeMap::new(), &no_env).unwrap();
        assert!(
            all.iter()
                .all(|p| p.id == AZURE || p.auth_style == AuthStyle::Auto)
        );
        let switched = with(
            ProviderSettings {
                auth: Some(AuthStyle::ApiKey),
                ..Default::default()
            },
            "openai",
            &no_env,
        );
        assert_eq!(switched.auth_style, AuthStyle::ApiKey);
    }

    #[test]
    fn discovery_reads_names_and_booleans() {
        let read = |text: &str| serde_json::from_str::<Discovery>(text);
        assert_eq!(read("\"lm-studio\"").unwrap(), Discovery::LmStudio);
        assert_eq!(read("true").unwrap(), Discovery::Auto);
        assert_eq!(read("false").unwrap(), Discovery::Off);
        assert!(
            read("\"sometimes\"")
                .unwrap_err()
                .to_string()
                .contains("sometimes")
        );
        assert_eq!(serde_json::to_string(&Discovery::Ollama).unwrap(), "\"ollama\"");
    }

    #[test]
    fn subscription_flows_are_configured() {
        let anthropic = preset("anthropic");
        let claude = anthropic.oauth.as_ref().expect("the Claude subscription flow");
        claude.check().unwrap();
        assert_eq!(claude.client().unwrap().0, "9d1c250a-e61b-44d9-88ed-5944d1962f5e");
        assert_eq!(claude.authorize_url, "https://claude.ai/oauth/authorize");
        assert_eq!(claude.token_url, "https://api.anthropic.com/v1/oauth/token");
        assert_eq!(
            (claude.redirect_port, claude.redirect_path.as_deref()),
            (54545, Some("/callback"))
        );
        assert_eq!(claude.redirect_host.as_deref(), Some("localhost"));
        assert_eq!(claude.token_body, BodyKind::Json);
        assert_eq!(claude.token_params["state"], "{state}");
        let refresh = claude.refresh.as_ref().unwrap();
        assert_eq!(refresh.headers["anthropic-beta"], "oauth-2025-04-20");
        // The subscription rides the same provider as API keys: the credential decides.
        assert_eq!(
            anthropic.oauth_headers["anthropic-beta"],
            "claude-code-20250219,oauth-2025-04-20"
        );
        assert_eq!(anthropic.oauth_headers["x-app"], "cli");

        let codex = preset("openai-codex");
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(codex.api, WireApi::OpenAiResponses);
        assert!(codex.requires_key && codex.api_key_env.is_empty());
        let chatgpt = codex.oauth.as_ref().expect("the ChatGPT subscription flow");
        chatgpt.check().unwrap();
        assert_eq!(chatgpt.authorize_url, "https://auth.openai.com/oauth/authorize");
        assert_eq!(chatgpt.token_url, "https://auth.openai.com/oauth/token");
        assert_eq!(
            chatgpt.redirect_uri.as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(chatgpt.redirect_port, 1455);
        // The account a request is billed to sits in the access token's own claims.
        assert_eq!(
            chatgpt.credential.fields["account_id"].claim.as_deref(),
            Some("https://api.openai.com/auth.chatgpt_account_id")
        );
    }

    #[test]
    fn the_newer_sign_ins_describe_their_servers() {
        let copilot = preset("github-copilot");
        let github = copilot.oauth.as_ref().expect("the Copilot device flow");
        assert_eq!(github.flow, OAuthFlow::Device);
        assert_eq!(github.device_url, "https://github.com/login/device/code");
        assert_eq!(github.scopes, ["read:user"]);
        assert_eq!(copilot.headers["Copilot-Integration-Id"], "copilot-developer-cli");

        let gemini = preset("google-gemini-cli");
        let google = gemini.oauth.as_ref().expect("the Gemini CLI sign-in");
        let (id, secret) = google.client().unwrap();
        assert!(id.ends_with(".apps.googleusercontent.com") && !secret.is_empty());
        assert!(!google.pkce);
        assert_eq!(
            (google.redirect_port, google.redirect_path.as_deref()),
            (8085, Some("/oauth2callback"))
        );
        assert!(gemini.requires_key && gemini.api_key_env.is_empty());

        // OpenRouter registers no client and hands back a key, not a token that expires.
        let openrouter = preset("openrouter");
        let key = openrouter.oauth.as_ref().expect("the OpenRouter key flow");
        assert!(key.anonymous_client && !key.check_state && !key.authorize_standard);
        assert_eq!(key.credential.access, "key");
        assert_eq!(key.token_params["code_verifier"], "{code_verifier}");

        let grok = preset("xai-oauth");
        let xai = grok.oauth.as_ref().expect("the Grok device flow");
        assert_eq!(xai.flow, OAuthFlow::Device);
        assert!(xai.discovery_url.is_some());
        assert_eq!(grok.api_key_env, ["XAI_OAUTH_TOKEN"]);
    }

    #[test]
    fn a_codex_login_fills_the_headers_the_backend_asks_for() {
        use crate::credentials::Auth;

        let codex = preset("openai-codex");
        let login = Auth::OAuth {
            access: "at-1".to_string(),
            fields: BTreeMap::from([("account_id".to_string(), "acct-9".to_string())]),
        };
        let mut headers = login.login_headers(&codex);
        headers.sort();
        assert_eq!(
            headers,
            [
                ("chatgpt-account-id".to_string(), "acct-9".to_string()),
                ("originator".to_string(), "azazel".to_string())
            ]
        );

        // An API key carries none of it, and a login without the account is billed to no one the
        // backend knows, so the header is left off rather than sent empty.
        assert!(Auth::ApiKey("sk".into()).login_headers(&codex).is_empty());
        let bare = Auth::OAuth {
            access: "at-1".to_string(),
            fields: BTreeMap::new(),
        };
        assert_eq!(
            bare.login_headers(&codex),
            [("originator".to_string(), "azazel".to_string())]
        );
    }

    #[test]
    fn hand_written_rows_name_where_each_provider_is_listed() {
        let rows: Vec<(&str, &str)> = hand_written().iter().map(|(id, name, _)| (*id, *name)).collect();
        assert!(rows.contains(&("fireworks", "fireworks-ai")));
        assert!(rows.contains(&("openai", "openai")));
        // Local servers are asked themselves; a snapshot of someone else's models would mislead.
        for local in ["ollama", "lmstudio", "llamacpp"] {
            assert!(rows.contains(&(local, "")), "{local}");
        }
    }

    #[test]
    fn every_generated_row_resolves_to_a_provider_with_a_key_of_its_own() {
        assert!(
            GENERATED.len() >= 100,
            "generated.rs holds {} rows",
            GENERATED.len()
        );
        // Every variable an address waits for, set.
        let filled = |name: &str| {
            Some(
                if name.ends_with("_URL") {
                    "https://gateway.example"
                } else {
                    "acct"
                }
                .to_string(),
            )
        };
        let all = resolve_with(&BTreeMap::new(), &filled).unwrap();
        let catalog = crate::catalog::Catalog::embedded();
        let dir = tempfile::tempdir().unwrap();
        for row in GENERATED {
            let provider = all
                .iter()
                .find(|p| p.id == row.id)
                .unwrap_or_else(|| panic!("generated `{}` is covered by a hand-written row", row.id));
            let id = &provider.id;
            assert!(is_builtin(id), "{id}");
            assert!(
                [
                    WireApi::AnthropicMessages,
                    WireApi::OpenAiResponses,
                    WireApi::OpenAiChat,
                    WireApi::GoogleGenerative
                ]
                .contains(&provider.api),
                "{id}: {:?}",
                provider.api
            );
            assert_eq!(provider.unset_variable(), None, "{id}: {}", provider.base_url);
            assert!(
                provider.base_url.starts_with("https://") || provider.base_url.starts_with("http://"),
                "{id}: {}",
                provider.base_url
            );
            let local = ["http://localhost:", "http://127.0.0.1:"]
                .iter()
                .any(|prefix| provider.base_url.starts_with(prefix));
            assert_eq!(provider.requires_key, !local, "{id}");
            if !local {
                assert!(
                    !catalog.for_provider(id).is_empty(),
                    "{id} has no models in the catalogue"
                );
            }

            // Its variable makes it usable, and makes no other provider usable.
            assert!(!provider.api_key_env.is_empty(), "{id}");
            for name in &provider.api_key_env {
                let readers: Vec<&str> = all
                    .iter()
                    .filter(|other| other.api_key_env.contains(name))
                    .map(|other| other.id.as_str())
                    .collect();
                assert_eq!(readers, [id.as_str()], "{name}");
            }
            let key = provider.api_key_env[0].clone();
            let credentials = crate::credentials::Credentials::with_env(
                dir.path().join("auth.json"),
                std::sync::Arc::new(move |name: &str| (name == key).then(|| "k".to_string())),
            );
            assert!(credentials.source(provider).is_some(), "{id}");
        }
    }

    #[test]
    fn a_hand_written_row_replaces_a_generated_one() {
        let hand = [
            row(
                "acme",
                "Acme (hand)",
                WireApi::AnthropicMessages,
                "https://hand.acme.example",
                &["ACME_KEY"],
            ),
            row(
                "bolt",
                "Bolt",
                WireApi::OpenAiChat,
                "https://bolt.example/v1",
                &["BOLT_KEY"],
            )
            .catalog("bolt-ai"),
        ];
        let generated = [
            row(
                "acme",
                "Acme",
                WireApi::OpenAiChat,
                "https://acme.example/v1",
                &["ACME_API_KEY"],
            ),
            row(
                "aero",
                "Aero",
                WireApi::OpenAiChat,
                "https://aero.example/v1",
                &["AERO_API_KEY"],
            ),
            row(
                "bolt-ai",
                "Bolt AI",
                WireApi::OpenAiChat,
                "https://bolt.example/v1",
                &["BOLT_AI_API_KEY"],
            ),
        ];
        let merged: Vec<(&str, &str)> = merge(&hand, &generated)
            .iter()
            .map(|row| (row.id, row.base_url))
            .collect();
        // Same id: the hand-written row. Same models.dev provider under another id: gone.
        assert_eq!(
            merged,
            [
                ("acme", "https://hand.acme.example"),
                ("aero", "https://aero.example/v1"),
                ("bolt", "https://bolt.example/v1")
            ]
        );
    }

    #[test]
    fn the_models_and_logins_page_lists_every_generated_row() {
        let page = include_str!("../../../../docs/models-and-logins.md");
        for row in GENERATED {
            let line = format!("| `{}` | ", row.id);
            assert!(
                page.contains(&line),
                "docs/models-and-logins.md does not list `{}`",
                row.id
            );
        }
    }
}
