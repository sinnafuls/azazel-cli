//! The embeddings client: OpenAI-compatible `/embeddings` and Google's `batchEmbedContents`.

use std::sync::Arc;
use std::time::Duration;

use azazel_ai::providers::presets;
use azazel_ai::{Auth, Credentials, ProviderConfig, WireApi};
use azazel_core::Config;
use serde_json::{Value, json};

use crate::search::normalize;

/// Texts sent per request.
const BATCH: usize = 64;
/// Longest text sent for one vector; embedding models reject inputs beyond their window.
const MAX_CHARS: usize = 8000;
const TIMEOUT: Duration = Duration::from_secs(60);

pub(crate) struct Embedder {
    http: reqwest::Client,
    provider: ProviderConfig,
    model: String,
    credentials: Arc<Credentials>,
}

impl Embedder {
    /// The client `[embeddings]` asks for; `None` when the table is absent.
    pub(crate) fn from_config(
        config: &Config,
        credentials: Arc<Credentials>,
    ) -> Result<Option<Embedder>, String> {
        let Some(wanted) = &config.embeddings else {
            return Ok(None);
        };
        let provider = presets::resolve(&config.providers)?
            .into_iter()
            .find(|p| p.id == wanted.provider)
            .ok_or_else(|| {
                let id = &wanted.provider;
                format!(
                    "[embeddings] provider `{id}` is not known: use a built-in one or define [providers.{id}]"
                )
            })?;
        if matches!(
            provider.api,
            WireApi::AnthropicMessages | WireApi::BedrockConverse
        ) {
            return Err(format!(
                "[embeddings] provider `{}` speaks {}, which has no embeddings endpoint: use an \
                 OpenAI-compatible or Google provider",
                provider.id, provider.api
            ));
        }
        let model = wanted.model.trim().trim_start_matches("models/").to_string();
        if model.is_empty() {
            return Err("[embeddings] model is empty".to_string());
        }
        let http = azazel_ai::tls::client_builder()
            .timeout(TIMEOUT)
            .user_agent(concat!("azazel/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| format!("cannot set up the embeddings client: {e}"))?;
        Ok(Some(Embedder {
            http,
            provider,
            model,
            credentials,
        }))
    }

    /// The configured model; rows remember which model embedded them.
    pub(crate) fn model(&self) -> &str {
        &self.model
    }

    /// One unit-length vector per text, in order.
    pub(crate) async fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        let mut vectors = Vec::with_capacity(texts.len());
        for batch in texts.chunks(BATCH) {
            let batch: Vec<&str> = batch.iter().map(|text| clip(text.as_str())).collect();
            let mut got = self.request(&batch).await?;
            if got.len() != batch.len() {
                return Err(format!(
                    "{} returned {} embeddings for {} texts",
                    self.provider.id,
                    got.len(),
                    batch.len()
                ));
            }
            for vector in &mut got {
                normalize(vector);
            }
            vectors.append(&mut got);
        }
        Ok(vectors)
    }

    async fn request(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, String> {
        let auth = self
            .credentials
            .resolve(&self.provider)
            .await
            .map_err(|e| e.to_string())?;
        let google = self.provider.api == WireApi::GoogleGenerative;
        let (url, body) = if google {
            let model = format!("models/{}", self.model);
            let requests: Vec<Value> = texts
                .iter()
                .map(|text| json!({ "model": model, "content": { "parts": [{ "text": text }] } }))
                .collect();
            (
                format!("{}/{model}:batchEmbedContents", self.provider.base_url),
                json!({ "requests": requests }),
            )
        } else {
            (
                format!("{}/embeddings", self.provider.base_url),
                json!({ "model": self.model, "input": texts }),
            )
        };
        let mut request = self.http.post(url).json(&body);
        request = match (&auth, google) {
            (Auth::None, _) => request,
            (Auth::ApiKey(key), true) => request.header("x-goog-api-key", key),
            (Auth::ApiKey(token), false) | (Auth::OAuth { access: token, .. }, _) => {
                request.bearer_auth(token)
            }
        };
        for (name, value) in &self.provider.headers {
            request = request.header(name, value);
        }
        let failed = |why: String| format!("embeddings request to {} failed: {why}", self.provider.id);
        let response = request.send().await.map_err(|e| failed(e.to_string()))?;
        let status = response.status();
        let text = response.text().await.map_err(|e| failed(e.to_string()))?;
        if status == reqwest::StatusCode::UNAUTHORIZED {
            self.credentials.invalidate(&self.provider.id);
        }
        if !status.is_success() {
            let shown: String = text.chars().take(300).collect();
            return Err(failed(format!("{status}: {shown}")));
        }
        let reply: Value =
            serde_json::from_str(&text).map_err(|e| failed(format!("the reply is not JSON: {e}")))?;
        let vectors = if google {
            google_vectors(&reply)
        } else {
            openai_vectors(&reply)
        };
        vectors.ok_or_else(|| failed("the reply holds no embeddings".to_string()))
    }
}

/// The text up to [`MAX_CHARS`] characters.
fn clip(text: &str) -> &str {
    match text.char_indices().nth(MAX_CHARS) {
        Some((end, _)) => &text[..end],
        None => text,
    }
}

fn floats(value: &Value) -> Option<Vec<f32>> {
    value
        .as_array()?
        .iter()
        .map(|x| x.as_f64().map(|x| x as f32))
        .collect()
}

/// `{"data": [{"index": 0, "embedding": [...]}, ...]}`, returned in `index` order.
fn openai_vectors(reply: &Value) -> Option<Vec<Vec<f32>>> {
    let mut rows: Vec<(u64, Vec<f32>)> = Vec::new();
    for (position, row) in reply.get("data")?.as_array()?.iter().enumerate() {
        let index = row
            .get("index")
            .and_then(Value::as_u64)
            .unwrap_or(position as u64);
        rows.push((index, floats(row.get("embedding")?)?));
    }
    rows.sort_by_key(|(index, _)| *index);
    Some(rows.into_iter().map(|(_, vector)| vector).collect())
}

/// `{"embeddings": [{"values": [...]}, ...]}`.
fn google_vectors(reply: &Value) -> Option<Vec<Vec<f32>>> {
    reply
        .get("embeddings")?
        .as_array()?
        .iter()
        .map(|row| floats(row.get("values")?))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_of_both_apis_are_read_in_input_order() {
        let openai = json!({ "data": [
            { "index": 1, "embedding": [0.0, 1.0] },
            { "index": 0, "embedding": [1, 0] }
        ]});
        assert_eq!(
            openai_vectors(&openai),
            Some(vec![vec![1.0, 0.0], vec![0.0, 1.0]])
        );
        assert_eq!(
            openai_vectors(&json!({ "data": [{ "embedding": "base64" }] })),
            None
        );
        let google = json!({ "embeddings": [{ "values": [0.5, 0.5] }, { "values": [1.0] }] });
        assert_eq!(google_vectors(&google), Some(vec![vec![0.5, 0.5], vec![1.0]]));
        assert_eq!(google_vectors(&json!({ "error": {} })), None);
    }

    #[test]
    fn long_texts_are_cut_on_a_character_boundary() {
        let text = "é".repeat(MAX_CHARS + 5);
        assert_eq!(clip(&text).chars().count(), MAX_CHARS);
        assert_eq!(clip("short"), "short");
    }
}
