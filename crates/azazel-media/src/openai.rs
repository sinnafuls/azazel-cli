//! OpenAI image generation: `POST {base}/images/generations`, or `POST {base}/images/edits` when
//! the request has reference images, both with a JSON body and a bearer credential. Images come
//! back base64-encoded in `data[].b64_json`; a `data[].url` is downloaded instead.

use std::cmp::Ordering;

use async_trait::async_trait;
use azazel_ai::Auth;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{
    MediaBackend, MediaError, MediaFile, MediaKind, MediaRequest, REQUEST_TIMEOUT, data_url, decode_file,
    download, fetch_json, parse_ratio, until_cancelled,
};

const ID: &str = "openai";

pub struct OpenAiImages {
    base_url: String,
    auth: Auth,
    client: reqwest::Client,
}

impl OpenAiImages {
    /// `base_url` is the `openai` provider's (`https://api.openai.com/v1`), `auth` its credential.
    pub fn new(base_url: impl Into<String>, auth: Auth) -> Self {
        OpenAiImages {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            client: azazel_ai::tls::client(),
        }
    }

    async fn run(&self, req: &MediaRequest) -> Result<Vec<MediaFile>, MediaError> {
        let mut body = Map::new();
        body.insert("model".to_string(), req.model.as_str().into());
        body.insert("prompt".to_string(), req.prompt.as_str().into());
        body.insert("n".to_string(), req.count.max(1).into());
        if let Some(size) = size(req) {
            body.insert("size".to_string(), size.into());
        }
        if req.transparent() {
            body.insert("background".to_string(), "transparent".into());
            body.insert("output_format".to_string(), "png".into());
        }
        let mut images = Vec::with_capacity(req.inputs.len());
        for path in &req.inputs {
            let url = data_url(path).await?;
            images.push(json!({"image_url": url}));
        }
        let endpoint = if images.is_empty() {
            "images/generations"
        } else {
            body.insert("images".to_string(), Value::Array(images));
            "images/edits"
        };

        let request = self
            .client
            .post(format!("{}/{endpoint}", self.base_url))
            .timeout(REQUEST_TIMEOUT)
            .json(&body);
        let request = match &self.auth {
            Auth::ApiKey(token) | Auth::OAuth { access: token, .. } => request.bearer_auth(token),
            Auth::None => request,
        };
        let reply = fetch_json(ID, request).await?;

        let stated = reply["output_format"]
            .as_str()
            .map(|format| format!("image/{format}"));
        let mut files = Vec::new();
        for item in reply["data"].as_array().map(Vec::as_slice).unwrap_or_default() {
            if let Some(encoded) = item["b64_json"].as_str() {
                files.push(decode_file(ID, encoded, stated.as_deref())?);
            } else if let Some(url) = item["url"].as_str() {
                files.push(download(ID, &self.client, url, None).await?);
            }
        }
        if files.is_empty() {
            return Err(MediaError::Failed("openai answered without an image".to_string()));
        }
        Ok(files)
    }
}

/// The `size` to ask for: the requested pixel size, else the standard size nearest the requested
/// aspect ratio, else the model's default.
fn size(req: &MediaRequest) -> Option<String> {
    if let Some((width, height)) = req.size() {
        return Some(format!("{width}x{height}"));
    }
    let (width, height) = parse_ratio(req.aspect_ratio()?)?;
    let size = match width.cmp(&height) {
        Ordering::Equal => "1024x1024",
        Ordering::Greater => "1536x1024",
        Ordering::Less => "1024x1536",
    };
    Some(size.to_string())
}

#[async_trait]
impl MediaBackend for OpenAiImages {
    fn id(&self) -> &str {
        ID
    }

    fn kinds(&self) -> &[MediaKind] {
        &[MediaKind::Image]
    }

    async fn generate(
        &self,
        req: &MediaRequest,
        cancel: CancellationToken,
    ) -> Result<Vec<MediaFile>, MediaError> {
        until_cancelled(&cancel, self.run(req)).await
    }
}
