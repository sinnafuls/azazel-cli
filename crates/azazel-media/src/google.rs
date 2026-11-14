//! Google image generation: `POST {base}/models/{model}:generateContent` asking for image output.
//! Reference images go in as `inlineData` parts beside the prompt; generated images come back as
//! `inlineData` parts of the candidate. A request makes one image, so `count` images take
//! `count` requests.

use async_trait::async_trait;
use azazel_ai::Auth;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{
    MediaBackend, MediaError, MediaFile, MediaKind, MediaRequest, REQUEST_TIMEOUT, clip, decode_file,
    fetch_json, parse_ratio, read_input, until_cancelled,
};

const ID: &str = "google";

pub struct GoogleImages {
    base_url: String,
    auth: Auth,
    client: reqwest::Client,
}

impl GoogleImages {
    /// `base_url` is the `google` provider's (`https://generativelanguage.googleapis.com/v1beta`),
    /// `auth` its credential.
    pub fn new(base_url: impl Into<String>, auth: Auth) -> Self {
        GoogleImages {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            auth,
            client: azazel_ai::tls::client(),
        }
    }

    async fn run(&self, req: &MediaRequest) -> Result<Vec<MediaFile>, MediaError> {
        if req.transparent() {
            return Err(MediaError::Unsupported(
                "google image models cannot make a transparent background: leave `transparent` out and \
                 remove the background afterwards (`sprite_sheet` does it with `key_color`)"
                    .to_string(),
            ));
        }
        let mut parts = vec![json!({"text": req.prompt})];
        for path in &req.inputs {
            let (mime, data) = read_input(path).await?;
            parts.push(json!({"inlineData": {"mimeType": mime, "data": data}}));
        }
        let mut generation = Map::new();
        generation.insert("responseModalities".to_string(), json!(["TEXT", "IMAGE"]));
        let image = image_config(req);
        if !image.is_empty() {
            generation.insert("imageConfig".to_string(), Value::Object(image));
        }
        let body = json!({
            "contents": [{"role": "user", "parts": parts}],
            "generationConfig": generation,
        });
        let model = req.model.strip_prefix("models/").unwrap_or(&req.model);
        let url = format!("{}/models/{model}:generateContent", self.base_url);

        let mut files = Vec::new();
        for _ in 0..req.count.max(1) {
            let request = self.client.post(&url).timeout(REQUEST_TIMEOUT).json(&body);
            let request = match &self.auth {
                Auth::ApiKey(key) => request.header("x-goog-api-key", key),
                Auth::OAuth { access, .. } => request.bearer_auth(access),
                Auth::None => request,
            };
            let reply = fetch_json(ID, request).await?;
            let before = files.len();
            for part in parts_of(&reply) {
                let inline = &part["inlineData"];
                if let Some(data) = inline["data"].as_str() {
                    files.push(decode_file(ID, data, inline["mimeType"].as_str())?);
                }
            }
            if files.len() == before {
                return Err(MediaError::Failed(no_image(&reply)));
            }
        }
        Ok(files)
    }
}

/// `imageConfig` for the request: the aspect ratio asked for, or the one of the pixel size asked
/// for, and the resolution tier that covers that size's long side.
fn image_config(req: &MediaRequest) -> Map<String, Value> {
    let mut config = Map::new();
    let size = req.size();
    let ratio = match (req.aspect_ratio(), size) {
        (Some(ratio), _) => parse_ratio(ratio).map(|(w, h)| format!("{w}:{h}")),
        (None, Some((width, height))) => {
            let divisor = gcd(width, height);
            Some(format!("{}:{}", width / divisor, height / divisor))
        }
        (None, None) => None,
    };
    if let Some(ratio) = ratio {
        config.insert("aspectRatio".to_string(), ratio.into());
    }
    let tier = match size.map(|(width, height)| width.max(height)) {
        Some(side) if side > 2048 => Some("4K"),
        Some(side) if side > 1024 => Some("2K"),
        _ => None,
    };
    if let Some(tier) = tier {
        config.insert("imageSize".to_string(), tier.into());
    }
    config
}

fn gcd(a: u32, b: u32) -> u32 {
    if b == 0 { a } else { gcd(b, a % b) }
}

/// The content parts of every candidate.
fn parts_of(reply: &Value) -> impl Iterator<Item = &Value> {
    reply["candidates"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|candidate| candidate["content"]["parts"].as_array())
        .flatten()
}

/// Why a reply holds no image: the block reason, the finish reason, or what the model said instead.
fn no_image(reply: &Value) -> String {
    if let Some(reason) = reply["promptFeedback"]["blockReason"].as_str() {
        return format!("google blocked the prompt: {reason}");
    }
    let candidate = &reply["candidates"][0];
    let said: Vec<&str> = parts_of(reply).filter_map(|part| part["text"].as_str()).collect();
    let why = candidate["finishMessage"]
        .as_str()
        .map(str::to_string)
        .or_else(|| (!said.is_empty()).then(|| said.join(" ")))
        .unwrap_or_default();
    let reason = candidate["finishReason"].as_str().unwrap_or("no reason given");
    match why.trim() {
        "" => format!("google returned no image ({reason})"),
        why => format!("google returned no image ({reason}): {}", clip(why, 400)),
    }
}

#[async_trait]
impl MediaBackend for GoogleImages {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn request(params: Value) -> MediaRequest {
        let Value::Object(params) = params else {
            panic!("params must be an object")
        };
        MediaRequest {
            kind: MediaKind::Image,
            model: "m".into(),
            prompt: "p".into(),
            inputs: Vec::new(),
            params,
            count: 1,
        }
    }

    #[test]
    fn a_pixel_size_becomes_its_ratio_and_the_tier_covering_it() {
        let config = image_config(&request(json!({"size": "1920x1080"})));
        assert_eq!(
            Value::Object(config),
            json!({"aspectRatio": "16:9", "imageSize": "2K"})
        );

        let config = image_config(&request(json!({"size": "1024x1024"})));
        assert_eq!(Value::Object(config), json!({"aspectRatio": "1:1"}));

        let config = image_config(&request(json!({"size": "3840x2160", "aspect_ratio": "4:3"})));
        assert_eq!(
            Value::Object(config),
            json!({"aspectRatio": "4:3", "imageSize": "4K"})
        );

        assert!(image_config(&request(json!({}))).is_empty());
    }

    #[test]
    fn a_reply_without_an_image_says_why() {
        let blocked = json!({"promptFeedback": {"blockReason": "PROHIBITED_CONTENT"}});
        assert_eq!(
            no_image(&blocked),
            "google blocked the prompt: PROHIBITED_CONTENT"
        );

        let talked = json!({"candidates": [{
            "content": {"parts": [{"text": "I can only describe that."}]},
            "finishReason": "STOP"
        }]});
        assert_eq!(
            no_image(&talked),
            "google returned no image (STOP): I can only describe that."
        );

        let stopped = json!({"candidates": [{"finishReason": "NO_IMAGE"}]});
        assert_eq!(no_image(&stopped), "google returned no image (NO_IMAGE)");
    }
}
