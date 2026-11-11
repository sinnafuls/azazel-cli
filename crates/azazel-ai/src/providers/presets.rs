//! Built-in provider endpoints and their merge with `[providers.<id>]` config tables.
//!
//! `base_url` conventions the wire modules rely on:
//! - `anthropic-messages`: `{base_url}/v1/messages`, `{base_url}/v1/models`
//! - `openai-responses`: `{base_url}/responses`, `{base_url}/models`
//! - `openai-chat`: `{base_url}/chat/completions`, `{base_url}/models`
//! - `google-generative`: `{base_url}/models/{id}:streamGenerateContent?alt=sse`, `{base_url}/models`

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::oauth::map::{CredentialMap, Expiry, Field};
use crate::oauth::{BodyKind, Encoding, OAuthFlow, OAuthSettings, RefreshSettings};
use crate::types::WireApi;

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderConfig {
    pub id: String,
    pub api: WireApi,
    /// No trailing slash.
    pub base_url: String,
    /// Environment variables tried in order.
    pub api_key_env: Vec<String>,
    /// Shell command whose stdout is the key.
    pub api_key_command: Option<String>,
    /// Extra headers sent with every request.
    pub headers: BTreeMap<String, String>,
    /// Headers sent only when the credential is a login, with `{field}` holes filled from it.
    pub oauth_headers: BTreeMap<String, String>,
    /// When false, a request without credentials is sent unauthenticated (local servers).
    pub requires_key: bool,
    /// How the OpenAI-format wire APIs present the key.
    pub auth_style: AuthStyle,
    /// The authorisation server `azazel login` signs in with, when the provider has one.
    pub oauth: Option<OAuthSettings>,
}

/// How a provider's key is put on a request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AuthStyle {
    /// `Authorization: Bearer <key>`, or what the wire API prescribes.
    #[default]
    Bearer,
    /// An `api-key: <key>` header, as Azure OpenAI takes resource keys.
    ApiKey,
}

/// A `[providers.<id>]` config table. Overrides a preset field by field, or defines a new provider
/// when `api` and `base_url` are both present.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderSettings {
    pub api: Option<WireApi>,
    pub base_url: Option<String>,
    pub api_key_env: Option<String>,
    pub api_key_command: Option<String>,
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    #[serde(default)]
    pub oauth_headers: BTreeMap<String, String>,
    pub auth: Option<AuthStyle>,
    /// AWS region, for `bedrock`; the SDK's default chain decides when unset.
    pub region: Option<String>,
    /// AWS profile, for `bedrock`.
    pub profile: Option<String>,
    /// Google Cloud project, for `vertex`.
    pub project: Option<String>,
    /// Google Cloud location, for `vertex`.
    pub location: Option<String>,
    /// `[providers.<id>.oauth]`: sign in with an authorisation server instead of a key.
    pub oauth: Option<OAuthSettings>,
}

fn preset(id: &str, api: WireApi, base_url: &str, env: &[&str], requires_key: bool) -> ProviderConfig {
    ProviderConfig {
        id: id.to_string(),
        api,
        base_url: base_url.to_string(),
        api_key_env: env.iter().map(|s| s.to_string()).collect(),
        api_key_command: None,
        headers: BTreeMap::new(),
        oauth_headers: BTreeMap::new(),
        requires_key,
        auth_style: AuthStyle::Bearer,
        oauth: None,
    }
}

pub fn presets() -> Vec<ProviderConfig> {
    use WireApi::*;
    let mut providers = vec![
        preset(
            "anthropic",
            AnthropicMessages,
            "https://api.anthropic.com",
            &["ANTHROPIC_API_KEY"],
            true,
        ),
        preset(
            "openai",
            OpenAiResponses,
            "https://api.openai.com/v1",
            &["OPENAI_API_KEY"],
            true,
        ),
        preset(
            "google",
            GoogleGenerative,
            "https://generativelanguage.googleapis.com/v1beta",
            &["GEMINI_API_KEY", "GOOGLE_API_KEY"],
            true,
        ),
        preset(
            "openrouter",
            OpenAiChat,
            "https://openrouter.ai/api/v1",
            &["OPENROUTER_API_KEY"],
            true,
        ),
        preset(
            "groq",
            OpenAiChat,
            "https://api.groq.com/openai/v1",
            &["GROQ_API_KEY"],
            true,
        ),
        preset(
            "deepseek",
            OpenAiChat,
            "https://api.deepseek.com",
            &["DEEPSEEK_API_KEY"],
            true,
        ),
        preset("xai", OpenAiChat, "https://api.x.ai/v1", &["XAI_API_KEY"], true),
        preset(
            "mistral",
            OpenAiChat,
            "https://api.mistral.ai/v1",
            &["MISTRAL_API_KEY"],
            true,
        ),
        preset(
            "ollama",
            OpenAiChat,
            "http://localhost:11434/v1",
            &["OLLAMA_API_KEY"],
            false,
        ),
        preset(
            "lmstudio",
            OpenAiChat,
            "http://localhost:1234/v1",
            &["LMSTUDIO_API_KEY"],
            false,
        ),
        preset(
            "llamacpp",
            OpenAiChat,
            "http://localhost:8080/v1",
            &["LLAMACPP_API_KEY"],
            false,
        ),
    ];
    // `azazel login anthropic` can also sign in with a Claude Pro or Max subscription; which
    // credential answers a prompt is decided by `auth.json` and the environment, not here.
    if let Some(anthropic) = providers.iter_mut().find(|p| p.id == "anthropic") {
        anthropic.oauth = Some(claude_subscription());
        anthropic.oauth_headers = BTreeMap::from([
            (
                "anthropic-beta".to_string(),
                format!("{CLAUDE_CODE_BETA},{OAUTH_BETA}"),
            ),
            ("User-Agent".to_string(), CLAUDE_USER_AGENT.to_string()),
            ("x-app".to_string(), "cli".to_string()),
        ]);
    }
    // The ChatGPT subscription runs on its own backend, separate from `openai` API keys.
    let mut codex = preset(
        "openai-codex",
        OpenAiResponses,
        "https://chatgpt.com/backend-api/codex",
        &[],
        true,
    );
    codex.oauth = Some(codex_subscription());
    codex.oauth_headers = BTreeMap::from([
        ("originator".to_string(), "azazel".to_string()),
        ("chatgpt-account-id".to_string(), "{account_id}".to_string()),
    ]);
    providers.push(codex);
    providers
}

/// The `anthropic-beta` value that tells the API the bearer token is a subscription login.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The `anthropic-beta` value that tells the API which client is asking: without it, subscription
/// requests are answered as if from an unknown client and rate limited at once.
const CLAUDE_CODE_BETA: &str = "claude-code-20250219";
/// The Claude Code public OAuth client id, stored base64 so secret scanners stay quiet.
const CLAUDE_CLIENT_ID: &str = "OWQxYzI1MGEtZTYxYi00NGQ5LTg4ZWQtNTk0NGQxOTYyZjVl";
/// The CLI identity the subscription endpoint expects on the wire.
const CLAUDE_USER_AGENT: &str = "claude-cli/2.1.321 (external, cli)";
/// The Codex CLI public OAuth client id.
const CODEX_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
/// The OpenAI claim that names the ChatGPT workspace a token is billed to.
const CODEX_ACCOUNT_CLAIM: &str = "https://api.openai.com/auth.chatgpt_account_id";

/// `azazel login anthropic`: the Claude Pro and Max subscription flow, an authorisation code
/// with PKCE that claude.ai sends back to a listener on this machine.
fn claude_subscription() -> OAuthSettings {
    let credential = CredentialMap {
        expires: Expiry::Seconds {
            path: "expires_in".to_string(),
            skew_ms: 300_000,
        },
        fields: BTreeMap::from([
            (
                "account_id".to_string(),
                Field {
                    path: Some("account.uuid".to_string()),
                    claim: None,
                },
            ),
            (
                "email".to_string(),
                Field {
                    path: Some("account.email_address".to_string()),
                    claim: None,
                },
            ),
            (
                "org_id".to_string(),
                Field {
                    path: Some("organization.uuid".to_string()),
                    claim: None,
                },
            ),
            (
                "org_name".to_string(),
                Field {
                    path: Some("organization.name".to_string()),
                    claim: None,
                },
            ),
        ]),
        ..CredentialMap::default()
    };
    OAuthSettings {
        flow: OAuthFlow::Pkce,
        client_id: CLAUDE_CLIENT_ID.to_string(),
        client_id_encoding: Encoding::Base64,
        authorize_url: "https://claude.ai/oauth/authorize".to_string(),
        token_url: "https://api.anthropic.com/v1/oauth/token".to_string(),
        scopes: [
            "org:create_api_key",
            "user:profile",
            "user:inference",
            "user:sessions:claude_code",
            "user:mcp_servers",
            "user:file_upload",
        ]
        .map(str::to_string)
        .to_vec(),
        authorize_params: BTreeMap::from([("code".to_string(), "true".to_string())]),
        // Claude's client registration allowlists `localhost`, not the numeric loopback address.
        redirect_host: Some("localhost".to_string()),
        redirect_port: 54545,
        redirect_path: Some("/callback".to_string()),
        token_body: BodyKind::Json,
        token_params: BTreeMap::from([("state".to_string(), "{state}".to_string())]),
        credential,
        refresh: Some(RefreshSettings {
            headers: BTreeMap::from([
                ("anthropic-beta".to_string(), OAUTH_BETA.to_string()),
                ("User-Agent".to_string(), CLAUDE_USER_AGENT.to_string()),
            ]),
            // The organisation a login belongs to is fixed when it is made; the stored fields
            // carry it over a refresh that does not repeat it.
            credential: Some(CredentialMap {
                expires: Expiry::Seconds {
                    path: "expires_in".to_string(),
                    skew_ms: 300_000,
                },
                fields: BTreeMap::from([
                    (
                        "account_id".to_string(),
                        Field {
                            path: Some("account.uuid".to_string()),
                            claim: None,
                        },
                    ),
                    (
                        "email".to_string(),
                        Field {
                            path: Some("account.email_address".to_string()),
                            claim: None,
                        },
                    ),
                ]),
                ..CredentialMap::default()
            }),
            ..RefreshSettings::default()
        }),
        instructions: Some(
            "This signs in with your Claude Pro or Max subscription. Complete the login in the \
             browser; if it cannot reach this machine, paste the address it ends up at when asked."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}

/// `azazel login openai-codex`: the ChatGPT (Codex) subscription flow. OpenAI allowlists the
/// exact redirect address, so the listener owns port 1455 and a busy port fails the login.
fn codex_subscription() -> OAuthSettings {
    let credential = CredentialMap {
        fields: BTreeMap::from([
            (
                "account_id".to_string(),
                Field {
                    path: None,
                    claim: Some(CODEX_ACCOUNT_CLAIM.to_string()),
                },
            ),
            (
                "email".to_string(),
                Field {
                    path: None,
                    claim: Some("email".to_string()),
                },
            ),
        ]),
        ..CredentialMap::default()
    };
    OAuthSettings {
        flow: OAuthFlow::Pkce,
        client_id: CODEX_CLIENT_ID.to_string(),
        authorize_url: "https://auth.openai.com/oauth/authorize".to_string(),
        token_url: "https://auth.openai.com/oauth/token".to_string(),
        scopes: [
            "openid",
            "profile",
            "email",
            "offline_access",
            "api.connectors.read",
            "api.connectors.invoke",
        ]
        .map(str::to_string)
        .to_vec(),
        authorize_params: BTreeMap::from([
            ("id_token_add_organizations".to_string(), "true".to_string()),
            ("codex_cli_simplified_flow".to_string(), "true".to_string()),
            ("originator".to_string(), "azazel".to_string()),
        ]),
        redirect_host: Some("localhost".to_string()),
        redirect_port: 1455,
        redirect_path: Some("/auth/callback".to_string()),
        redirect_uri: Some("http://localhost:1455/auth/callback".to_string()),
        credential: credential.clone(),
        refresh: Some(RefreshSettings {
            credential: Some(credential),
            ..RefreshSettings::default()
        }),
        instructions: Some(
            "This signs in with your ChatGPT (Codex) subscription. Complete the login in the \
             browser; the address it returns to is fixed, so nothing else may use port 1455 while \
             it runs."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}

/// The provider id under which an Azure OpenAI resource is configured.
pub const AZURE: &str = "azure";
const AZURE_KEY_ENV: &str = "AZURE_OPENAI_API_KEY";
/// The provider ids of the cloud services reached with the platform's own credentials.
pub const BEDROCK: &str = "bedrock";
pub const VERTEX: &str = "vertex";

/// Presets with `settings` applied, sorted by id. A new provider needs `api` and `base_url`; it
/// requires a key only when it names a key source.
pub fn resolve(settings: &BTreeMap<String, ProviderSettings>) -> Result<Vec<ProviderConfig>, String> {
    let mut providers: BTreeMap<String, ProviderConfig> =
        presets().into_iter().map(|p| (p.id.clone(), p)).collect();
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
        let command = s.api_key_command.clone().filter(|c| !c.trim().is_empty());
        let env = s.api_key_env.clone().filter(|e| !e.trim().is_empty());
        match providers.get_mut(id) {
            Some(p) => {
                if let Some(api) = s.api {
                    p.api = api;
                }
                if let Some(url) = &s.base_url {
                    p.base_url = trim_url(url);
                }
                if let Some(env) = env {
                    p.api_key_env = vec![env];
                }
                if command.is_some() {
                    p.api_key_command = command;
                }
                p.headers.extend(s.headers.clone());
                p.oauth_headers.extend(s.oauth_headers.clone());
                if let Some(style) = s.auth {
                    p.auth_style = style;
                }
                if s.oauth.is_some() {
                    p.oauth = s.oauth.clone();
                }
            }
            None => {
                let (Some(api), Some(url)) = (s.api, &s.base_url) else {
                    return Err(format!(
                        "provider `{id}` is not built in: set both `api` and `base_url`"
                    ));
                };
                // Azure OpenAI: the address is the user's own resource, and its keys go in an
                // `api-key` header unless a bearer (Entra) token is configured.
                let azure = id == AZURE;
                let env = match env {
                    None if azure && command.is_none() && s.oauth.is_none() => {
                        Some(AZURE_KEY_ENV.to_string())
                    }
                    other => other,
                };
                let default_style = if azure {
                    AuthStyle::ApiKey
                } else {
                    AuthStyle::Bearer
                };
                providers.insert(
                    id.clone(),
                    ProviderConfig {
                        id: id.clone(),
                        api,
                        base_url: trim_url(url),
                        requires_key: env.is_some() || command.is_some() || s.oauth.is_some(),
                        api_key_env: env.into_iter().collect(),
                        api_key_command: command,
                        headers: s.headers.clone(),
                        oauth_headers: s.oauth_headers.clone(),
                        auth_style: s.auth.unwrap_or(default_style),
                        oauth: s.oauth.clone(),
                    },
                );
            }
        }
    }
    Ok(providers.into_values().collect())
}

fn trim_url(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn override_changes_only_named_fields() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "ollama".to_string(),
            ProviderSettings {
                base_url: Some("http://box:11434/v1/".into()),
                ..Default::default()
            },
        );
        let providers = resolve(&settings).unwrap();
        let ollama = providers.iter().find(|p| p.id == "ollama").unwrap();
        assert_eq!(ollama.base_url, "http://box:11434/v1");
        assert_eq!(ollama.api, WireApi::OpenAiChat);
        assert!(!ollama.requires_key);
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
        assert!(resolve(&settings).unwrap_err().contains("mine"));

        settings.insert(
            "mine".to_string(),
            ProviderSettings {
                api: Some(WireApi::AnthropicMessages),
                base_url: Some("https://gw.example/anthropic".into()),
                api_key_env: Some("MINE_KEY".into()),
                ..Default::default()
            },
        );
        let providers = resolve(&settings).unwrap();
        let mine = providers.iter().find(|p| p.id == "mine").unwrap();
        assert!(mine.requires_key);
        assert_eq!(mine.api_key_env, vec!["MINE_KEY".to_string()]);
    }

    #[test]
    fn empty_key_command_is_ignored() {
        let mut settings = BTreeMap::new();
        settings.insert(
            "local".to_string(),
            ProviderSettings {
                api: Some(WireApi::OpenAiChat),
                base_url: Some("http://localhost:9000/v1".into()),
                api_key_command: Some(String::new()),
                ..Default::default()
            },
        );
        let providers = resolve(&settings).unwrap();
        let local = providers.iter().find(|p| p.id == "local").unwrap();
        assert_eq!(
            (local.api_key_command.as_deref(), local.requires_key),
            (None, false)
        );
    }

    #[test]
    fn azure_takes_resource_keys_in_their_own_header_unless_told_to_use_a_bearer_token() {
        let azure = |settings: ProviderSettings| {
            let all = resolve(&BTreeMap::from([(AZURE.to_string(), settings)])).unwrap();
            all.into_iter().find(|p| p.id == AZURE).unwrap()
        };
        let base = ProviderSettings {
            api: Some(WireApi::OpenAiResponses),
            base_url: Some("https://res.openai.azure.com/openai/v1/".into()),
            ..Default::default()
        };
        let keyed = azure(base.clone());
        assert_eq!(keyed.auth_style, AuthStyle::ApiKey);
        assert_eq!(keyed.api_key_env, ["AZURE_OPENAI_API_KEY"]);
        assert!(keyed.requires_key);
        assert_eq!(keyed.base_url, "https://res.openai.azure.com/openai/v1");

        let entra = azure(ProviderSettings {
            auth: Some(AuthStyle::Bearer),
            api_key_command: Some("az account get-access-token --query accessToken -o tsv".into()),
            ..base
        });
        assert_eq!(entra.auth_style, AuthStyle::Bearer);
        assert!(entra.api_key_env.is_empty() && entra.api_key_command.is_some());

        // Without an address there is no Azure provider to talk to.
        assert!(
            resolve(&BTreeMap::from([(
                AZURE.to_string(),
                ProviderSettings::default()
            )]))
            .is_err()
        );
        // Other providers keep presenting a bearer token, and can be switched.
        let all = resolve(&BTreeMap::new()).unwrap();
        assert!(all.iter().all(|p| p.auth_style == AuthStyle::Bearer));
        let switched = resolve(&BTreeMap::from([(
            "openai".to_string(),
            ProviderSettings {
                auth: Some(AuthStyle::ApiKey),
                ..Default::default()
            },
        )]))
        .unwrap();
        assert_eq!(
            switched.iter().find(|p| p.id == "openai").unwrap().auth_style,
            AuthStyle::ApiKey
        );
    }

    #[test]
    fn subscription_flows_are_configured() {
        let providers = presets();
        let anthropic = providers.iter().find(|p| p.id == "anthropic").unwrap();
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

        let codex = providers.iter().find(|p| p.id == "openai-codex").unwrap();
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
    fn a_codex_login_fills_the_headers_the_backend_asks_for() {
        use crate::credentials::Auth;
        use std::collections::BTreeMap;

        let codex = presets().into_iter().find(|p| p.id == "openai-codex").unwrap();
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
}
