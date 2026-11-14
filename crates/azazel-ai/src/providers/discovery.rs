//! Asking a provider which models it has: the wire API's own list, or the richer one a local
//! server keeps beside it.

use std::time::Duration;

use futures::StreamExt;
use serde_json::{Value, json};

use crate::catalog::ModelInfo;
use crate::credentials::Auth;
use crate::error::AiError;
use crate::providers::azure;
use crate::providers::http::{HttpProvider, Wire};
use crate::providers::presets::Discovery;
use crate::types::WireApi;
use crate::wire;

/// How long one request of a discovery may take: a server that lists nothing in this time is
/// treated as not answering.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// The most pages of a paged list that are read.
const MAX_PAGES: usize = 25;
/// How many models are asked about at once.
const PARALLEL_SHOWS: usize = 8;
/// The window assumed for an Ollama model that does not state one.
const OLLAMA_CONTEXT: u64 = 128_000;
const OLLAMA_MAX_OUTPUT: u64 = 8192;
/// Ollama's own variable for the context every model is loaded with.
const OLLAMA_CONTEXT_ENV: &str = "OLLAMA_CONTEXT_LENGTH";

pub(super) async fn list(
    provider: &HttpProvider,
    wire: &Wire,
    auth: &Auth,
    kind: Discovery,
) -> Result<Vec<ModelInfo>, AiError> {
    match kind {
        Discovery::Off => Ok(Vec::new()),
        Discovery::Ollama => ollama(provider, wire, auth).await,
        Discovery::LmStudio => match lm_studio(provider, wire, auth).await? {
            Some(models) => Ok(models),
            // An older server has no native list; its OpenAI one still names the models.
            None => provider.wire_models(wire, auth).await,
        },
        Discovery::Azure => azure::deployments(provider, wire, auth).await,
        Discovery::Auto if provider.config().api == WireApi::GoogleGenerative => {
            google_pages(provider, wire, auth).await
        }
        Discovery::Auto => provider.wire_models(wire, auth).await,
    }
}

/// Every page of the Generative Language model list.
async fn google_pages(provider: &HttpProvider, wire: &Wire, auth: &Auth) -> Result<Vec<ModelInfo>, AiError> {
    let config = provider.config_for(auth);
    let mut models = Vec::new();
    let mut page: Option<String> = None;
    for _ in 0..MAX_PAGES {
        let request =
            wire::google::models_page_request(provider.client(), &config, auth, page.as_deref());
        let body = provider.fetch_json(wire, request).await?;
        models.extend(wire::google::parse_models(&config, &body));
        let next = wire::google::next_page_token(&body).map(str::to_string);
        // A token that repeats would be read for ever.
        if next.is_none() || next == page {
            break;
        }
        page = next;
    }
    Ok(models)
}

/// `/api/tags` for the installed models, then `/api/show` for what each can do. A model that
/// cannot be asked about is still listed, with defaults.
async fn ollama(provider: &HttpProvider, wire: &Wire, auth: &Auth) -> Result<Vec<ModelInfo>, AiError> {
    let config = provider.config_for(auth);
    let root = wire::ollama::root(&config.base_url);
    let headers = wire::ollama::headers(&config, auth);
    let tags = provider
        .client()
        .get(format!("{root}/api/tags"))
        .headers(headers.clone())
        .timeout(REQUEST_TIMEOUT);
    let body = provider.fetch_json(wire, tags).await?;
    let names: Vec<String> = wire::ollama::tag_names(&body)
        .into_iter()
        .map(str::to_string)
        .collect();
    let context_override = std::env::var(OLLAMA_CONTEXT_ENV)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|tokens| *tokens > 0);

    let requests: Vec<reqwest::RequestBuilder> = names
        .iter()
        .map(|name| {
            provider
                .client()
                .post(format!("{root}/api/show"))
                .headers(headers.clone())
                .timeout(REQUEST_TIMEOUT)
                .json(&json!({ "model": name }))
        })
        .collect();
    let shows = requests
        .into_iter()
        .map(|request| async move { provider.fetch_json(wire, request).await.ok() });
    let shown: Vec<Option<Value>> = futures::stream::iter(shows)
        .buffered(PARALLEL_SHOWS)
        .collect()
        .await;
    Ok(names
        .iter()
        .zip(shown)
        .filter_map(|(name, show)| ollama_model(&config.id, name, show.as_ref(), context_override))
        .collect())
}

/// One Ollama model from its `/api/show` answer. `None` for a model that cannot chat (an
/// embedding model). `context_override` is the window the server was told to load models with.
pub(super) fn ollama_model(
    provider: &str,
    id: &str,
    show: Option<&Value>,
    context_override: Option<u64>,
) -> Option<ModelInfo> {
    let mut info = ModelInfo::unknown(provider, id);
    let capabilities: Option<Vec<&str>> = show
        .and_then(|show| show.get("capabilities"))
        .and_then(Value::as_array)
        .map(|items| items.iter().filter_map(Value::as_str).collect());
    if let Some(capabilities) = &capabilities {
        if !capabilities.contains(&"completion") {
            return None;
        }
        info.vision = capabilities.contains(&"vision");
        info.reasoning = capabilities.contains(&"thinking");
    }
    // The window the model was built with, under whichever architecture name it is filed.
    let trained = show
        .and_then(|show| show.get("model_info"))
        .and_then(Value::as_object)
        .and_then(|fields| {
            [".context_length", ".num_ctx", ".context_window"]
                .iter()
                .find_map(|suffix| {
                    fields
                        .iter()
                        .filter(|(name, _)| name.ends_with(suffix))
                        .find_map(|(_, value)| value.as_u64().filter(|tokens| *tokens > 0))
                })
        });
    // A `num_ctx` in the model's own parameters is what it actually runs with.
    let configured = show
        .and_then(|show| show.get("parameters"))
        .and_then(Value::as_str)
        .and_then(|parameters| {
            parameters.lines().find_map(|line| {
                let mut words = line.split_whitespace();
                (words.next() == Some("num_ctx"))
                    .then(|| words.next()?.parse::<u64>().ok())
                    .flatten()
            })
        })
        .filter(|tokens| *tokens > 0);
    info.context_window = configured
        .or(context_override)
        .or(trained)
        .unwrap_or(OLLAMA_CONTEXT);
    info.max_output = u32::try_from(info.context_window.min(OLLAMA_MAX_OUTPUT)).unwrap_or(u32::MAX);
    Some(info)
}

/// LM Studio's native list, `GET {root}/api/v0/models`; `None` when the server has none. A
/// server that cannot be reached at all is an error: asking it a second way would only wait
/// twice.
async fn lm_studio(
    provider: &HttpProvider,
    wire: &Wire,
    auth: &Auth,
) -> Result<Option<Vec<ModelInfo>>, AiError> {
    let config = provider.config_for(auth);
    let root = wire::ollama::root(&config.base_url);
    let mut headers = reqwest::header::HeaderMap::new();
    wire::add_headers(&mut headers, &config, auth, wire::KeyHeader::Bearer);
    let request = provider
        .client()
        .get(format!("{root}/api/v0/models"))
        .headers(headers)
        .timeout(REQUEST_TIMEOUT);
    let body = match provider.fetch_json(wire, request).await {
        Ok(body) => body,
        Err(unreachable @ AiError::Network(_)) => return Err(unreachable),
        Err(_) => return Ok(None),
    };
    let models = lm_studio_models(&config.id, &body);
    Ok((!models.is_empty()).then_some(models))
}

/// The chat models of an `/api/v0/models` answer. The window is the one a loaded model runs
/// with, which the user chose, before the largest it could have.
pub(super) fn lm_studio_models(provider: &str, body: &Value) -> Vec<ModelInfo> {
    body.get("data")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())?;
            let kind = entry.get("type").and_then(Value::as_str).unwrap_or_default();
            if kind.starts_with("embedding") {
                return None;
            }
            let mut info = ModelInfo::unknown(provider, id);
            let tokens = |name: &str| entry.get(name).and_then(Value::as_u64).filter(|n| *n > 0);
            let loaded = entry.get("state").and_then(Value::as_str) == Some("loaded");
            let window = tokens("loaded_context_length")
                .filter(|_| loaded)
                .or_else(|| tokens("max_context_length"))
                .or_else(|| tokens("context_length"))
                .or_else(|| tokens("max_model_len"));
            if let Some(window) = window {
                info.context_window = window;
                info.max_output = info.max_output.min(u32::try_from(window).unwrap_or(u32::MAX));
            }
            let sees = entry
                .get("capabilities")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter_map(Value::as_str)
                        .any(|capability| matches!(capability, "vision" | "image"))
                });
            info.vision = kind == "vlm" || sees;
            Some(info)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_ollama_model_is_described_by_what_show_reports() {
        let show = json!({
            "capabilities": ["completion", "tools", "thinking", "vision"],
            "model_info": {
                "general.architecture": "mockarch",
                "mockarch.context_length": 262_144,
                "mockarch.embedding_length": 4096,
            },
        });
        let model = ollama_model("ollama", "mock:8b", Some(&show), None).unwrap();
        assert_eq!(
            (
                model.context_window,
                model.max_output,
                model.vision,
                model.reasoning
            ),
            (262_144, 8192, true, true)
        );
        assert_eq!(model.key(), "ollama/mock:8b");

        // What the model is loaded with is narrower than what it was built with.
        let pinned = json!({
            "capabilities": ["completion"],
            "parameters": "stop \"<|im_end|>\"\nnum_ctx 4096\ntemperature 0.6",
            "model_info": {"mockarch.context_length": 131_072},
        });
        let model = ollama_model("ollama", "small", Some(&pinned), Some(32_768)).unwrap();
        assert_eq!((model.context_window, model.max_output), (4096, 4096));
        assert!(!model.vision && !model.reasoning);

        let plain = json!({"capabilities": ["completion"], "model_info": {"mockarch.context_length": 8192}});
        let model = ollama_model("ollama", "plain", Some(&plain), Some(32_768)).unwrap();
        assert_eq!(model.context_window, 32_768);
    }

    #[test]
    fn an_ollama_model_that_cannot_chat_is_left_out_and_a_silent_one_gets_defaults() {
        let embedder = json!({"capabilities": ["embedding"]});
        assert!(ollama_model("ollama", "mock-embed", Some(&embedder), None).is_none());

        let silent = ollama_model("ollama", "mystery", None, None).unwrap();
        assert_eq!(
            (
                silent.context_window,
                silent.max_output,
                silent.vision,
                silent.reasoning
            ),
            (128_000, 8192, false, false)
        );
    }

    #[test]
    fn lm_studio_reports_the_window_a_model_is_loaded_with() {
        let body = json!({
            "data": [
                {"id": "loaded-vlm", "type": "vlm", "state": "loaded",
                 "loaded_context_length": 16_384, "max_context_length": 131_072},
                {"id": "idle-llm", "type": "llm", "state": "not-loaded",
                 "loaded_context_length": 4096, "max_context_length": 32_768},
                {"id": "tiny", "type": "llm", "state": "loaded", "loaded_context_length": 2048},
                {"id": "embedder", "type": "embeddings", "max_context_length": 2048},
                {"type": "llm"},
            ],
        });
        let models = lm_studio_models("lmstudio", &body);
        let summary: Vec<(&str, u64, u32, bool)> = models
            .iter()
            .map(|m| (m.id.as_str(), m.context_window, m.max_output, m.vision))
            .collect();
        assert_eq!(
            summary,
            [
                ("loaded-vlm", 16_384, 8192, true),
                ("idle-llm", 32_768, 8192, false),
                ("tiny", 2048, 2048, false),
            ]
        );
    }
}
