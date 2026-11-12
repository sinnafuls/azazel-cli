//! Google Cloud Code Assist, the endpoint a Gemini CLI sign-in talks to:
//! `POST {base_url}/v1internal:streamGenerateContent?alt=sse`.
//!
//! The request is the Generative Language body ([`super::google`]) inside an envelope that names
//! the Cloud project it is billed to, and each SSE record wraps the usual chunk in `response`.
//! The project comes from the login (field [`PROJECT_FIELD`]); the provider looks it up before
//! the first request.

use reqwest::header::{ACCEPT, HeaderMap, HeaderName, HeaderValue, USER_AGENT};
use serde_json::{Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::providers::presets::ProviderConfig;
use crate::types::Request;
use crate::wire::{KeyHeader, add_headers, google};

/// The field of a login that holds the Cloud project requests are made in.
pub const PROJECT_FIELD: &str = "project_id";
/// How the Gemini CLI describes itself; the service is offered to that client.
const CLIENT_METADATA: &str = "ideType=IDE_UNSPECIFIED,platform=PLATFORM_UNSPECIFIED,pluginType=GEMINI";
const CLI_VERSION: &str = "0.46.0";
/// Limits of the models served here, which the model list does not state.
const CONTEXT_WINDOW: u64 = 1_048_576;
const MAX_OUTPUT: u32 = 65_536;

/// The project a credential carries.
pub fn project(auth: &Auth) -> Option<&str> {
    match auth {
        Auth::OAuth { fields, .. } => fields.get(PROJECT_FIELD).map(String::as_str),
        _ => None,
    }
}

pub fn build_body(provider: &ProviderConfig, model: &ModelInfo, req: &Request, project: &str) -> Value {
    json!({
        "project": project,
        "model": model.id,
        "request": google::build_body(provider, model, req),
    })
}

pub fn build_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let body = build_body(provider, model, req, project(auth).unwrap_or_default());
    let mut headers = headers(provider, auth, &model.id);
    headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
    client
        .post(format!(
            "{}/v1internal:streamGenerateContent?alt=sse",
            provider.base_url
        ))
        .headers(headers)
        .json(&body)
}

/// The models the signed-in account has quota for: `POST {base_url}/v1internal:retrieveUserQuota`.
pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let body = match project(auth) {
        Some(project) => json!({ "project": project }),
        None => json!({}),
    };
    client
        .post(format!("{}/v1internal:retrieveUserQuota", provider.base_url))
        .headers(headers(provider, auth, ""))
        .json(&body)
}

/// One model per quota bucket; a model with several buckets is listed once.
pub fn parse_models(provider: &ProviderConfig, body: &Value) -> Vec<ModelInfo> {
    let mut models: Vec<ModelInfo> = Vec::new();
    let buckets = body.get("buckets").and_then(Value::as_array);
    for id in buckets
        .into_iter()
        .flatten()
        .filter_map(|bucket| bucket.get("modelId").and_then(Value::as_str))
        .filter(|id| !id.is_empty())
    {
        if models.iter().all(|known| known.id != id) {
            models.push(ModelInfo {
                context_window: CONTEXT_WINDOW,
                max_output: MAX_OUTPUT,
                vision: true,
                reasoning: true,
                ..ModelInfo::unknown(&provider.id, id)
            });
        }
    }
    models
}

/// The bearer token and the client description, then the provider's configured headers.
pub(crate) fn headers(provider: &ProviderConfig, auth: &Auth, model: &str) -> HeaderMap {
    let mut headers = HeaderMap::new();
    let platform = match std::env::consts::OS {
        "windows" => "win32",
        "macos" => "darwin",
        other => other,
    };
    let arch = match std::env::consts::ARCH {
        "x86_64" => "x64",
        "aarch64" => "arm64",
        other => other,
    };
    let model = if model.is_empty() { "unknown" } else { model };
    let agent = format!("GeminiCLI/{CLI_VERSION}/{model} ({platform}; {arch}; terminal)");
    if let Ok(agent) = HeaderValue::from_str(&agent) {
        headers.insert(USER_AGENT, agent);
    }
    headers.insert(
        HeaderName::from_static("client-metadata"),
        HeaderValue::from_static(CLIENT_METADATA),
    );
    add_headers(&mut headers, provider, auth, KeyHeader::Bearer);
    headers
}
