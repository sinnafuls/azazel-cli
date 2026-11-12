//! Azure OpenAI: the Responses and Chat Completions formats at a resource's own address, where a
//! model is a deployment.
//!
//! - `azure-openai-responses`: `POST {resource}/openai/v1/responses`
//! - `azure-openai-chat`: `POST {resource}/openai/deployments/{deployment}/chat/completions`
//!
//! `base_url` is the resource (`https://<name>.openai.azure.com`); a trailing `/openai` or
//! `/openai/v1` is accepted and ignored. Two environment variables adjust requests:
//! `AZURE_OPENAI_API_VERSION` (sent as `api-version`; chat needs one and defaults to
//! [`CHAT_API_VERSION`]) and `AZURE_OPENAI_DEPLOYMENT_NAME_MAP` (`model=deployment,…`, for
//! deployments not named after their model).

use std::borrow::Cow;

use serde_json::Value;

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::providers::presets::ProviderConfig;
use crate::types::Request;
use crate::wire::{openai_chat, openai_responses};

/// The newest generally available version of the deployment-scoped API.
pub const CHAT_API_VERSION: &str = "2024-10-21";
const VERSION_ENV: &str = "AZURE_OPENAI_API_VERSION";
const DEPLOYMENTS_ENV: &str = "AZURE_OPENAI_DEPLOYMENT_NAME_MAP";

/// What the environment says about the resource.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AzureSettings {
    pub api_version: Option<String>,
    /// `model=deployment` pairs, comma separated.
    pub deployments: Option<String>,
}

impl AzureSettings {
    pub fn from_env() -> Self {
        let read = |name: &str| std::env::var(name).ok().filter(|value| !value.trim().is_empty());
        AzureSettings {
            api_version: read(VERSION_ENV),
            deployments: read(DEPLOYMENTS_ENV),
        }
    }

    /// The deployment that serves `model`: the one the map names, else one called like the model.
    pub fn deployment<'a>(&'a self, model: &'a str) -> &'a str {
        self.deployments
            .as_deref()
            .into_iter()
            .flat_map(|map| map.split(','))
            .filter_map(|pair| pair.split_once('='))
            .map(|(from, to)| (from.trim(), to.trim()))
            .find(|(from, to)| *from == model && !to.is_empty())
            .map_or(model, |(_, to)| to)
    }
}

/// The resource's address without the `/openai` or `/openai/v1` of either API.
fn resource(base_url: &str) -> &str {
    let trimmed = base_url.trim_end_matches('/');
    let trimmed = trimmed.strip_suffix("/v1").unwrap_or(trimmed);
    trimmed.strip_suffix("/openai").unwrap_or(trimmed)
}

pub fn responses_url(base_url: &str, settings: &AzureSettings) -> String {
    let mut url = format!("{}/openai/v1/responses", resource(base_url));
    // The v1 API needs no version; one is sent only when asked for (preview features).
    if let Some(version) = &settings.api_version {
        url.push_str("?api-version=");
        url.push_str(version);
    }
    url
}

pub fn chat_url(base_url: &str, model: &str, settings: &AzureSettings) -> String {
    let version = settings.api_version.as_deref().unwrap_or(CHAT_API_VERSION);
    // An address that already names a deployment is used as it is.
    let deployment = match base_url.contains("/deployments/") {
        true => base_url.trim_end_matches('/').to_string(),
        false => format!(
            "{}/openai/deployments/{}",
            resource(base_url),
            settings.deployment(model)
        ),
    };
    format!("{deployment}/chat/completions?api-version={version}")
}

/// `model` as the request names it: by its deployment.
fn deployed<'a>(model: &'a ModelInfo, settings: &AzureSettings) -> Cow<'a, ModelInfo> {
    match settings.deployment(&model.id) {
        same if same == model.id => Cow::Borrowed(model),
        deployment => Cow::Owned(ModelInfo {
            id: deployment.to_string(),
            ..model.clone()
        }),
    }
}

pub fn responses_body(
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    settings: &AzureSettings,
) -> Value {
    openai_responses::build_body(provider, &deployed(model, settings), req)
}

pub fn chat_body(
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    settings: &AzureSettings,
) -> Value {
    openai_chat::build_body(provider, &deployed(model, settings), req)
}

pub fn build_responses_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let settings = AzureSettings::from_env();
    client
        .post(responses_url(&provider.base_url, &settings))
        .json(&responses_body(provider, model, req, &settings))
        .headers(openai_chat::request_headers(provider, auth))
}

pub fn build_chat_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    model: &ModelInfo,
    req: &Request,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    let settings = AzureSettings::from_env();
    client
        .post(chat_url(&provider.base_url, &model.id, &settings))
        .json(&chat_body(provider, model, req, &settings))
        .headers(openai_chat::request_headers(provider, auth))
}

/// The models the resource can serve: `GET {resource}/openai/v1/models`. Which of them are
/// deployed is not part of the answer, so the built-in `azure` provider does not ask.
pub fn models_request(
    client: &reqwest::Client,
    provider: &ProviderConfig,
    auth: &Auth,
) -> reqwest::RequestBuilder {
    client
        .get(format!("{}/openai/v1/models", resource(&provider.base_url)))
        .headers(openai_chat::request_headers(provider, auth))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_way_of_writing_the_resource_reaches_the_same_addresses() {
        let plain = AzureSettings::default();
        for base in [
            "https://res.openai.azure.com",
            "https://res.openai.azure.com/",
            "https://res.openai.azure.com/openai",
            "https://res.openai.azure.com/openai/v1",
        ] {
            assert_eq!(
                responses_url(base, &plain),
                "https://res.openai.azure.com/openai/v1/responses",
                "{base}"
            );
            assert_eq!(
                chat_url(base, "gpt-x", &plain),
                "https://res.openai.azure.com/openai/deployments/gpt-x/chat/completions?api-version=2024-10-21",
                "{base}"
            );
        }
        let full = "https://res.openai.azure.com/openai/deployments/prod";
        assert_eq!(
            chat_url(full, "gpt-x", &plain),
            format!("{full}/chat/completions?api-version=2024-10-21")
        );
    }

    #[test]
    fn the_environment_names_the_version_and_the_deployments() {
        let settings = AzureSettings {
            api_version: Some("preview".to_string()),
            deployments: Some("gpt-x = prod-x, broken, gpt-y=".to_string()),
        };
        assert_eq!(settings.deployment("gpt-x"), "prod-x");
        assert_eq!(settings.deployment("gpt-y"), "gpt-y");
        assert_eq!(settings.deployment("other"), "other");
        assert_eq!(
            responses_url("https://res.openai.azure.com", &settings),
            "https://res.openai.azure.com/openai/v1/responses?api-version=preview"
        );
        assert_eq!(
            chat_url("https://res.openai.azure.com", "gpt-x", &settings),
            "https://res.openai.azure.com/openai/deployments/prod-x/chat/completions?api-version=preview"
        );
    }
}
