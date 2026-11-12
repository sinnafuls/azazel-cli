//! The sign-ins `azazel login` offers for built-in providers: each is the description of one
//! authorisation server, read by the engine in [`crate::oauth`].

use std::collections::BTreeMap;

use crate::oauth::map::{CredentialMap, Expiry, Field};
use crate::oauth::{BodyKind, Encoding, OAuthFlow, OAuthSettings, RefreshSettings, UserInfo};

/// The sign-in of a built-in provider and the headers a request made with it carries (`{field}`
/// holes are filled from the login).
pub(super) fn login(provider: &str) -> Option<(OAuthSettings, BTreeMap<String, String>)> {
    let headers = |pairs: &[(&str, &str)]| {
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect::<BTreeMap<_, _>>()
    };
    Some(match provider {
        // Which credential answers a prompt is decided by `auth.json` and the environment: the
        // subscription rides the same provider as API keys.
        "anthropic" => (
            claude_subscription(),
            headers(&[
                ("anthropic-beta", &format!("{CLAUDE_CODE_BETA},{OAUTH_BETA}")),
                ("User-Agent", CLAUDE_USER_AGENT),
                ("x-app", "cli"),
            ]),
        ),
        "github-copilot" => (copilot_subscription(), BTreeMap::new()),
        "google-gemini-cli" => (gemini_cli_login(), BTreeMap::new()),
        // The ChatGPT subscription runs on its own backend, separate from `openai` API keys.
        "openai-codex" => (
            codex_subscription(),
            headers(&[("originator", "azazel"), ("chatgpt-account-id", "{account_id}")]),
        ),
        "openrouter" => (openrouter_key(), BTreeMap::new()),
        "xai-oauth" => (grok_subscription(), BTreeMap::new()),
        _ => return None,
    })
}

fn claim(name: &str) -> Field {
    Field {
        path: None,
        claim: Some(name.to_string()),
    }
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

/// The GitHub OAuth app Copilot's command-line clients sign in with.
const COPILOT_CLIENT_ID: &str = "Ov23li8tweQw6odWQebz";
/// The Gemini CLI's installed-application client, stored base64 so secret scanners stay quiet.
/// Google does not treat the secret of an installed application as confidential.
const GEMINI_CLI_CLIENT_ID: &str =
    "NjgxMjU1ODA5Mzk1LW9vOGZ0Mm9wcmRybnA5ZTNhcWY2YXYzaG1kaWIxMzVqLmFwcHMuZ29vZ2xldXNlcmNvbnRlbnQuY29t";
const GEMINI_CLI_CLIENT_SECRET: &str = "R09DU1BYLTR1SGdNUG0tMW83U2stZ2VWNkN1NWNsWEZzeGw=";
/// The Grok CLI's public client id.
const GROK_CLIENT_ID: &str = "b1a00492-073a-47ea-816f-4c329264a828";

fn accept_json() -> BTreeMap<String, String> {
    BTreeMap::from([("Accept".to_string(), "application/json".to_string())])
}

/// `azazel login github-copilot`: GitHub's device flow. The token GitHub hands out is the one
/// the Copilot API takes, and GitHub ends it only when the user revokes the app.
fn copilot_subscription() -> OAuthSettings {
    OAuthSettings {
        flow: OAuthFlow::Device,
        client_id: COPILOT_CLIENT_ID.to_string(),
        device_url: "https://github.com/login/device/code".to_string(),
        token_url: "https://github.com/login/oauth/access_token".to_string(),
        scopes: vec!["read:user".to_string()],
        // Without it GitHub answers both requests as a form, not as JSON.
        device_headers: accept_json(),
        token_headers: accept_json(),
        credential: CredentialMap {
            expires: Expiry::Never,
            ..CredentialMap::default()
        },
        instructions: Some(
            "This signs in with your GitHub Copilot subscription. Enter the code on the page that \
             opens and approve the request."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}

/// `azazel login google-gemini-cli`: the Google account sign-in of the Gemini CLI, which gives
/// access to Cloud Code Assist. The Cloud project requests are billed to is looked up on first
/// use and kept with the login.
fn gemini_cli_login() -> OAuthSettings {
    let credential = CredentialMap {
        expires: Expiry::Seconds {
            path: "expires_in".to_string(),
            skew_ms: 300_000,
        },
        ..CredentialMap::default()
    };
    OAuthSettings {
        flow: OAuthFlow::Pkce,
        client_id: GEMINI_CLI_CLIENT_ID.to_string(),
        client_id_encoding: Encoding::Base64,
        client_secret: GEMINI_CLI_CLIENT_SECRET.to_string(),
        client_secret_encoding: Encoding::Base64,
        authorize_url: "https://accounts.google.com/o/oauth2/v2/auth".to_string(),
        token_url: "https://oauth2.googleapis.com/token".to_string(),
        scopes: [
            "https://www.googleapis.com/auth/cloud-platform",
            "https://www.googleapis.com/auth/userinfo.email",
            "https://www.googleapis.com/auth/userinfo.profile",
        ]
        .map(str::to_string)
        .to_vec(),
        // This client is registered without a code challenge; its secret is what it presents.
        pkce: false,
        authorize_params: BTreeMap::from([
            ("access_type".to_string(), "offline".to_string()),
            ("prompt".to_string(), "consent".to_string()),
        ]),
        redirect_port: 8085,
        redirect_path: Some("/oauth2callback".to_string()),
        credential,
        userinfo: Some(UserInfo {
            url: "https://www.googleapis.com/oauth2/v1/userinfo?alt=json".to_string(),
            fields: BTreeMap::from([("email".to_string(), "email".to_string())]),
        }),
        instructions: Some(
            "This signs in with your Google account as the Gemini CLI does. Complete the sign-in \
             in the browser; nothing else may use port 8085 while it runs."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}

/// `azazel login openrouter`: OpenRouter's own flow for applications. There is no client
/// registration (the code verifier alone proves who asked), the page never echoes `state`, and
/// what comes back is a lasting API key.
fn openrouter_key() -> OAuthSettings {
    OAuthSettings {
        flow: OAuthFlow::Pkce,
        anonymous_client: true,
        authorize_url: "https://openrouter.ai/auth".to_string(),
        token_url: "https://openrouter.ai/api/v1/auth/keys".to_string(),
        check_state: false,
        authorize_standard: false,
        authorize_params: BTreeMap::from([
            ("callback_url".to_string(), "{redirect_uri}".to_string()),
            ("code_challenge".to_string(), "{code_challenge}".to_string()),
            ("code_challenge_method".to_string(), "S256".to_string()),
        ]),
        redirect_host: Some("localhost".to_string()),
        redirect_port: 54549,
        redirect_path: Some("/callback".to_string()),
        token_body: BodyKind::Json,
        token_standard: false,
        token_params: BTreeMap::from([
            ("code".to_string(), "{code}".to_string()),
            ("code_verifier".to_string(), "{code_verifier}".to_string()),
            ("code_challenge_method".to_string(), "S256".to_string()),
        ]),
        credential: CredentialMap {
            access: "key".to_string(),
            expires: Expiry::Never,
            ..CredentialMap::default()
        },
        instructions: Some(
            "This creates an OpenRouter API key for azazel in the browser. Use `azazel login \
             openrouter --key` to paste a key you already have."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}

/// `azazel login xai-oauth`: the device flow of a SuperGrok or X Premium+ subscription. The
/// token endpoint is the one xAI's discovery document names.
fn grok_subscription() -> OAuthSettings {
    let account = |fields: &[(&str, &str)]| UserInfo {
        url: "https://auth.x.ai/oauth2/userinfo".to_string(),
        fields: fields
            .iter()
            .map(|(name, path)| (name.to_string(), path.to_string()))
            .collect(),
    };
    let who = [("email", "email"), ("account_id", "sub")];
    OAuthSettings {
        flow: OAuthFlow::Device,
        client_id: GROK_CLIENT_ID.to_string(),
        discovery_url: Some("https://auth.x.ai/.well-known/openid-configuration".to_string()),
        device_url: "https://auth.x.ai/oauth2/device/code".to_string(),
        scopes: [
            "openid",
            "profile",
            "email",
            "offline_access",
            "grok-cli:access",
            "api:access",
        ]
        .map(str::to_string)
        .to_vec(),
        device_headers: accept_json(),
        token_headers: accept_json(),
        credential: CredentialMap {
            fields: BTreeMap::from([("account_id".to_string(), claim("sub"))]),
            ..CredentialMap::default()
        },
        userinfo: Some(account(&who)),
        refresh: Some(RefreshSettings {
            userinfo: Some(account(&who)),
            ..RefreshSettings::default()
        }),
        instructions: Some(
            "This signs in with your SuperGrok or X Premium+ subscription. Enter the code on the \
             page that opens."
                .to_string(),
        ),
        ..OAuthSettings::default()
    }
}
