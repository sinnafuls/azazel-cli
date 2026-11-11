//! fal: images, video, 3D models and audio through the fal queue.
//!
//! `POST {base}/{model}` queues a job and answers with its status, response and cancel URLs. The
//! status is polled until it reads `COMPLETED`; the response is then the model's own JSON, and
//! every file URL in it is downloaded. Requests carry `Authorization: Key <key>`.
//!
//! Every model on fal defines its own input names. A request uses the names the common models
//! share and, where those differ, sends the value under each: reference images as `image_url`
//! and `image_urls` (`input_image_urls` for 3D), an audio prompt as `prompt` and `text`, an audio
//! length as `duration`, `duration_seconds` and `seconds_total`.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::backend::{
    MediaBackend, MediaError, MediaFile, MediaKind, MediaRequest, REQUEST_TIMEOUT, clip, data_url, describe,
    download, fetch_json, until_cancelled,
};

pub const FAL_URL: &str = "https://queue.fal.run";
/// How often a queued job's status is asked for.
pub const POLL_INTERVAL: Duration = Duration::from_secs(2);

const ID: &str = "fal";
/// Longest a job is waited for.
const MAX_WAIT: Duration = Duration::from_secs(30 * 60);
const KINDS: [MediaKind; 4] = MediaKind::ALL;

pub struct Fal {
    base_url: String,
    key: String,
    poll: Duration,
    client: reqwest::Client,
}

/// What fal answers when it accepts a job.
#[derive(Deserialize)]
struct Job {
    request_id: String,
    status_url: String,
    response_url: String,
    cancel_url: Option<String>,
}

/// A file named in a job's result.
#[derive(Debug, PartialEq, Eq)]
struct FileRef {
    url: String,
    content_type: Option<String>,
    file_name: Option<String>,
}

impl Fal {
    /// `base_url` is the queue endpoint ([`FAL_URL`]); `poll` is the pause between status checks.
    pub fn new(base_url: impl Into<String>, key: impl Into<String>, poll: Duration) -> Self {
        Fal {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            key: key.into(),
            poll,
            client: reqwest::Client::new(),
        }
    }

    fn authorized(&self, request: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        request
            .header(reqwest::header::AUTHORIZATION, format!("Key {}", self.key))
            .timeout(REQUEST_TIMEOUT)
    }

    /// The model input for `req`, under fal's names.
    async fn body(&self, req: &MediaRequest) -> Result<Map<String, Value>, MediaError> {
        let mut images = Vec::with_capacity(req.inputs.len());
        for path in &req.inputs {
            images.push(Value::String(data_url(path).await?));
        }
        let mut body = Map::new();
        let mut put = |key: &str, value: Value| {
            body.insert(key.to_string(), value);
        };
        let prompt = req.prompt.trim();
        if !prompt.is_empty() {
            put("prompt", prompt.into());
        }
        match req.kind {
            MediaKind::Image => {
                if let Some((width, height)) = req.size() {
                    put("image_size", json!({"width": width, "height": height}));
                }
                if let Some(ratio) = req.aspect_ratio() {
                    put("aspect_ratio", ratio.into());
                }
                if req.count > 1 {
                    put("num_images", req.count.into());
                }
                if req.transparent() {
                    put("background", "transparent".into());
                    put("output_format", "png".into());
                }
                if !images.is_empty() {
                    put("image_url", images[0].clone());
                    put("image_urls", Value::Array(images));
                }
            }
            MediaKind::Video => {
                if let Some(first) = images.into_iter().next() {
                    put("image_url", first);
                }
                // Models that take a duration list the lengths they offer as strings.
                if let Some(secs) = req.duration_secs() {
                    put("duration", secs.to_string().into());
                }
                if let Some(ratio) = req.aspect_ratio() {
                    put("aspect_ratio", ratio.into());
                }
            }
            MediaKind::Model3d => {
                if !images.is_empty() {
                    put("image_url", images[0].clone());
                    put("input_image_urls", Value::Array(images));
                }
                if let Some(format) = req.format() {
                    put("geometry_file_format", format.into());
                }
            }
            MediaKind::Audio => {
                put("text", prompt.into());
                if let Some(secs) = req.duration_secs() {
                    put("duration", secs.into());
                    put("duration_seconds", secs.into());
                    put("seconds_total", secs.into());
                }
                if let Some(voice) = req.voice() {
                    put("voice", voice.into());
                }
            }
        }
        Ok(body)
    }

    async fn submit(&self, model: &str, body: &Map<String, Value>) -> Result<Job, MediaError> {
        let url = format!("{}/{}", self.base_url, model.trim_matches('/'));
        let reply = fetch_json(ID, self.authorized(self.client.post(url)).json(body)).await?;
        serde_json::from_value(reply).map_err(|e| {
            MediaError::Failed(format!("fal queued the job but its reply could not be read: {e}"))
        })
    }

    /// Returns once the job has completed without an error.
    async fn wait(&self, job: &Job) -> Result<(), MediaError> {
        let deadline = Instant::now() + MAX_WAIT;
        loop {
            let status = fetch_json(ID, self.authorized(self.client.get(&job.status_url))).await?;
            match status["status"].as_str() {
                Some("COMPLETED") => {
                    return match &status["error"] {
                        Value::Null => Ok(()),
                        Value::String(error) if error.is_empty() => Ok(()),
                        Value::String(error) => Err(failed(job, error)),
                        other => Err(failed(job, &other.to_string())),
                    };
                }
                Some("IN_QUEUE" | "IN_PROGRESS") => {}
                other => {
                    return Err(MediaError::Failed(format!(
                        "fal reported a job status this client does not know: {}",
                        other.unwrap_or("none")
                    )));
                }
            }
            if Instant::now() >= deadline {
                return Err(MediaError::Failed(format!(
                    "fal job {} was still running after {} minutes; its result will be at {}",
                    job.request_id,
                    MAX_WAIT.as_secs() / 60,
                    job.response_url
                )));
            }
            tokio::time::sleep(self.poll).await;
        }
    }

    /// Waits for the job, then downloads every file its result names.
    async fn collect(&self, job: &Job) -> Result<Vec<MediaFile>, MediaError> {
        self.wait(job).await?;
        let result = fetch_json(ID, self.authorized(self.client.get(&job.response_url))).await?;
        let refs = file_refs(&result);
        if refs.is_empty() {
            return Err(MediaError::Failed(format!(
                "fal finished job {} but its result names no file: {}",
                job.request_id,
                clip(&result.to_string(), 300)
            )));
        }
        let seed = result["seed"].as_u64();
        let mut files = Vec::with_capacity(refs.len());
        for file in refs {
            let mut downloaded = download(ID, &self.client, &file.url, file.file_name.as_deref()).await?;
            if downloaded.ext == "bin"
                && let Some(stated) = &file.content_type
            {
                let name = file.file_name.as_deref();
                downloaded = MediaFile::detect(downloaded.bytes, Some(stated.as_str()), name);
            }
            downloaded.seed = seed;
            files.push(downloaded);
        }
        Ok(files)
    }

    /// Asks fal to drop the job, without waiting for the answer.
    fn withdraw(&self, job: &Job) {
        let Some(url) = &job.cancel_url else {
            return;
        };
        let request = self
            .authorized(self.client.put(url))
            .timeout(Duration::from_secs(10));
        let id = job.request_id.clone();
        tokio::spawn(async move {
            if let Err(error) = request.send().await {
                tracing::debug!("could not cancel fal job {id}: {}", describe(error));
            }
        });
    }
}

fn failed(job: &Job, error: &str) -> MediaError {
    MediaError::Failed(format!("fal job {} failed: {error}", job.request_id))
}

fn is_http(url: &str) -> bool {
    url.starts_with("https://") || url.starts_with("http://")
}

/// Every file a result names, in document order and once each: objects with a `url`, and strings
/// under a key that is `url` or ends in `_url`.
fn file_refs(result: &Value) -> Vec<FileRef> {
    fn walk(value: &Value, key: Option<&str>, out: &mut Vec<FileRef>) {
        let text = |field: &Value| field.as_str().map(str::to_string);
        match value {
            Value::Object(fields) => match fields.get("url").and_then(Value::as_str) {
                Some(url) if is_http(url) => out.push(FileRef {
                    url: url.to_string(),
                    content_type: fields.get("content_type").and_then(text),
                    file_name: fields.get("file_name").and_then(text),
                }),
                _ => {
                    for (name, field) in fields {
                        walk(field, Some(name), out);
                    }
                }
            },
            Value::Array(items) => {
                for item in items {
                    walk(item, key, out);
                }
            }
            Value::String(url)
                if key.is_some_and(|key| key == "url" || key.ends_with("_url")) && is_http(url) =>
            {
                out.push(FileRef {
                    url: url.clone(),
                    content_type: None,
                    file_name: None,
                });
            }
            _ => {}
        }
    }

    let mut found = Vec::new();
    walk(result, None, &mut found);
    let mut unique: Vec<FileRef> = Vec::with_capacity(found.len());
    for file in found {
        if !unique.iter().any(|seen| seen.url == file.url) {
            unique.push(file);
        }
    }
    unique
}

#[async_trait]
impl MediaBackend for Fal {
    fn id(&self) -> &str {
        ID
    }

    fn kinds(&self) -> &[MediaKind] {
        &KINDS
    }

    async fn generate(
        &self,
        req: &MediaRequest,
        cancel: CancellationToken,
    ) -> Result<Vec<MediaFile>, MediaError> {
        let body = until_cancelled(&cancel, self.body(req)).await?;
        let job = until_cancelled(&cancel, self.submit(&req.model, &body)).await?;
        let outcome = until_cancelled(&cancel, self.collect(&job)).await;
        if matches!(outcome, Err(MediaError::Aborted)) {
            self.withdraw(&job);
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn urls(result: Value) -> Vec<String> {
        file_refs(&result).into_iter().map(|file| file.url).collect()
    }

    #[test]
    fn files_are_found_wherever_a_result_puts_them() {
        assert_eq!(
            urls(json!({
                "images": [
                    {"url": "https://cdn.example/a.png", "content_type": "image/png", "width": 64},
                    {"url": "https://cdn.example/b.png"}
                ],
                "seed": 7,
                "prompt": "see https://not-a-file.example"
            })),
            ["https://cdn.example/a.png", "https://cdn.example/b.png"]
        );
        assert_eq!(
            urls(json!({
                "model_mesh": {"url": "https://cdn.example/m.glb", "file_name": "m.glb"},
                "textures": [{"url": "https://cdn.example/t.png"}],
                "audio_url": "https://cdn.example/s.mp3",
                "preview": {"nested": {"video": {"url": "https://cdn.example/m.glb"}}}
            })),
            [
                "https://cdn.example/m.glb",
                "https://cdn.example/t.png",
                "https://cdn.example/s.mp3"
            ]
        );
    }

    #[test]
    fn inline_data_and_text_fields_are_not_files() {
        let result = json!({"image": {"url": "data:image/png;base64,AAAA"}, "description": "done"});
        assert!(urls(result).is_empty());
    }

    #[test]
    fn a_file_keeps_the_name_and_type_the_result_states() {
        let found = file_refs(&json!({
            "video": {"url": "https://cdn.example/x", "content_type": "video/mp4", "file_name": "clip.mp4"}
        }));
        assert_eq!(
            found,
            [FileRef {
                url: "https://cdn.example/x".into(),
                content_type: Some("video/mp4".into()),
                file_name: Some("clip.mp4".into()),
            }]
        );
    }
}
