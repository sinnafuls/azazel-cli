//! The contract between the generation tools and the services that generate, and what the
//! backends share: HTTP plumbing, input encoding and file typing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use azazel_core::media::image_mime;
use base64::Engine;
use bytes::Bytes;
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

/// Longest one request to a generation service may take.
pub(crate) const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Longest the download of one generated file may take.
const DOWNLOAD_TIMEOUT: Duration = Duration::from_secs(600);
/// Most characters of a service's error text passed on.
const ERROR_CHARS: usize = 600;
const OCTET_STREAM: &str = "application/octet-stream";

/// File extensions and their MIME types; the first row for a MIME type is its extension.
const TYPES: [(&str, &str); 22] = [
    ("png", "image/png"),
    ("jpg", "image/jpeg"),
    ("webp", "image/webp"),
    ("gif", "image/gif"),
    ("svg", "image/svg+xml"),
    ("mp4", "video/mp4"),
    ("webm", "video/webm"),
    ("mov", "video/quicktime"),
    ("glb", "model/gltf-binary"),
    ("gltf", "model/gltf+json"),
    ("obj", "model/obj"),
    ("stl", "model/stl"),
    ("usdz", "model/vnd.usdz+zip"),
    ("fbx", OCTET_STREAM),
    ("zip", "application/zip"),
    ("mp3", "audio/mpeg"),
    ("wav", "audio/wav"),
    ("wav", "audio/x-wav"),
    ("ogg", "audio/ogg"),
    ("flac", "audio/flac"),
    ("opus", "audio/opus"),
    ("m4a", "audio/mp4"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum MediaKind {
    Image,
    Video,
    Model3d,
    Audio,
}

impl MediaKind {
    pub const ALL: [MediaKind; 4] = [
        MediaKind::Image,
        MediaKind::Video,
        MediaKind::Model3d,
        MediaKind::Audio,
    ];

    /// The kind's key under `[media.models]` and its directory under the output directory.
    pub fn as_str(self) -> &'static str {
        match self {
            MediaKind::Image => "image",
            MediaKind::Video => "video",
            MediaKind::Model3d => "model3d",
            MediaKind::Audio => "audio",
        }
    }

    /// The tool that generates this kind; registered only when a media backend is configured.
    pub fn tool_name(self) -> &'static str {
        match self {
            MediaKind::Image => "generate_image",
            MediaKind::Video => "generate_video",
            MediaKind::Model3d => "generate_3d",
            MediaKind::Audio => "generate_audio",
        }
    }
}

impl std::fmt::Display for MediaKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One generation. `model` is the backend's own model id. `inputs` are local files the backend
/// sends as reference media. `params` holds what the caller asked for under the tool input names,
/// each only when given: `size` (`"<width>x<height>"`), `aspect_ratio` (`"<w>:<h>"`),
/// `transparent` (bool), `duration_secs` (integer), `format` (`glb`, `obj` or `fbx`), `kind`
/// (`sfx`, `music` or `speech`) and `voice`. Each backend translates them to its service's names.
#[derive(Debug, Clone, PartialEq)]
pub struct MediaRequest {
    pub kind: MediaKind,
    pub model: String,
    pub prompt: String,
    pub inputs: Vec<PathBuf>,
    pub params: Map<String, Value>,
    pub count: u8,
}

impl MediaRequest {
    fn text(&self, key: &str) -> Option<&str> {
        self.params
            .get(key)?
            .as_str()
            .map(str::trim)
            .filter(|text| !text.is_empty())
    }

    /// `size` as width and height in pixels.
    pub fn size(&self) -> Option<(u32, u32)> {
        parse_size(self.text("size")?)
    }

    pub fn aspect_ratio(&self) -> Option<&str> {
        self.text("aspect_ratio")
    }

    pub fn transparent(&self) -> bool {
        self.params.get("transparent").and_then(Value::as_bool) == Some(true)
    }

    pub fn duration_secs(&self) -> Option<u64> {
        self.params.get("duration_secs")?.as_u64()
    }

    pub fn format(&self) -> Option<&str> {
        self.text("format")
    }

    pub fn voice(&self) -> Option<&str> {
        self.text("voice")
    }
}

/// `"<width>x<height>"` as two positive numbers.
pub fn parse_size(text: &str) -> Option<(u32, u32)> {
    pair(text.trim().split_once(['x', 'X'])?)
}

/// `"<w>:<h>"` as two positive numbers.
pub fn parse_ratio(text: &str) -> Option<(u32, u32)> {
    pair(text.trim().split_once(':')?)
}

fn pair((first, second): (&str, &str)) -> Option<(u32, u32)> {
    let (first, second) = (first.trim().parse().ok()?, second.trim().parse().ok()?);
    (first > 0 && second > 0).then_some((first, second))
}

/// One generated file. `ext` has no dot. `seed` is the seed the service reports having used.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaFile {
    pub bytes: Bytes,
    pub mime: String,
    pub ext: String,
    pub seed: Option<u64>,
}

impl MediaFile {
    /// A file typed by its content when that is recognisable (PNG, JPEG, WebP, GIF, GLB), else by
    /// the extension of `name` (a file name or URL), else by `mime` (a `Content-Type` value).
    pub fn detect(bytes: Bytes, mime: Option<&str>, name: Option<&str>) -> MediaFile {
        let by_content =
            image_mime(&bytes).or_else(|| bytes.starts_with(b"glTF").then_some("model/gltf-binary"));
        let stated = mime
            .map(|mime| mime.split(';').next().unwrap_or_default().trim())
            .filter(|mime| !mime.is_empty() && !mime.ends_with("/octet-stream"));
        let (mime, ext) = if let Some(mime) = by_content {
            (mime.to_string(), ext_for_mime(mime).unwrap_or("bin").to_string())
        } else if let Some(ext) = name.and_then(extension_of) {
            let mime = mime_for_ext(&ext).filter(|mime| *mime != OCTET_STREAM).or(stated);
            (mime.unwrap_or(OCTET_STREAM).to_string(), ext)
        } else if let Some(mime) = stated {
            (mime.to_string(), ext_for_mime(mime).unwrap_or("bin").to_string())
        } else {
            (OCTET_STREAM.to_string(), "bin".to_string())
        };
        MediaFile {
            bytes,
            mime,
            ext,
            seed: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MediaError {
    /// The request cannot be expressed for this backend.
    #[error("{0}")]
    Unsupported(String),
    /// An input file could not be read.
    #[error("{0}")]
    Input(String),
    /// The service could not be reached.
    #[error("{0}")]
    Network(String),
    /// The service refused the request.
    #[error("{backend} answered HTTP {status}: {message}")]
    Api {
        backend: String,
        status: u16,
        message: String,
    },
    /// The service took the request and produced nothing usable.
    #[error("{0}")]
    Failed(String),
    #[error("aborted by user")]
    Aborted,
}

/// A service that generates media.
#[async_trait]
pub trait MediaBackend: Send + Sync {
    /// The name models are qualified with: `<id>:<model id>`.
    fn id(&self) -> &str;
    fn kinds(&self) -> &[MediaKind];
    /// Generates `req.count` files, or as many as the model makes per request. Returns
    /// [`MediaError::Aborted`] promptly once `cancel` fires.
    async fn generate(
        &self,
        req: &MediaRequest,
        cancel: CancellationToken,
    ) -> Result<Vec<MediaFile>, MediaError>;
}

/// `work`, given up with [`MediaError::Aborted`] as soon as `cancel` fires.
pub(crate) async fn until_cancelled<T>(
    cancel: &CancellationToken,
    work: impl Future<Output = Result<T, MediaError>>,
) -> Result<T, MediaError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(MediaError::Aborted),
        result = work => result,
    }
}

/// `error` with its causes, which `reqwest` hides behind "error sending request", and without the
/// URL, which may carry more than the user should see in a transcript.
pub(crate) fn describe(error: reqwest::Error) -> String {
    let error = error.without_url();
    let mut text = error.to_string();
    let mut source = std::error::Error::source(&error);
    while let Some(cause) = source {
        let cause_text = cause.to_string();
        if !text.contains(&cause_text) {
            text.push_str(": ");
            text.push_str(&cause_text);
        }
        source = cause.source();
    }
    text
}

/// The first `max` characters of `text`.
pub(crate) fn clip(text: &str, max: usize) -> String {
    match text.char_indices().nth(max) {
        Some((end, _)) => format!("{}…", &text[..end]),
        None => text.to_string(),
    }
}

/// What an error body says went wrong. Reads `{"error": {"message": …}}` (OpenAI, Google),
/// `{"detail": "…"}` and `{"detail": [{"msg": …}]}` (fal); anything else is passed on as it came.
pub(crate) fn error_text(body: &str) -> String {
    let parsed: Option<Value> = serde_json::from_str(body).ok();
    let found = parsed.as_ref().and_then(|value| {
        let error = &value["error"];
        let detail = &value["detail"];
        let listed = detail.as_array().map(|items| {
            let messages: Vec<&str> = items.iter().filter_map(|item| item["msg"].as_str()).collect();
            messages.join("; ")
        });
        error["message"]
            .as_str()
            .or(error.as_str())
            .or(detail.as_str())
            .or(value["message"].as_str())
            .map(str::to_string)
            .or(listed)
            .filter(|text| !text.trim().is_empty())
    });
    let text = found.unwrap_or_else(|| body.trim().to_string());
    if text.is_empty() {
        "the response had no body".to_string()
    } else {
        clip(&text, ERROR_CHARS)
    }
}

/// Sends `request` and returns the status and body of the answer, whatever the status.
pub(crate) async fn send(
    backend: &str,
    request: reqwest::RequestBuilder,
) -> Result<(u16, String), MediaError> {
    let offline = |error: reqwest::Error| {
        MediaError::Network(format!("{backend} could not be reached: {}", describe(error)))
    };
    let response = request.send().await.map_err(offline)?;
    let status = response.status().as_u16();
    let body = response.text().await.map_err(offline)?;
    Ok((status, body))
}

/// The JSON body of a successful answer, or the error a failed one is.
pub(crate) fn json_reply(backend: &str, status: u16, body: &str) -> Result<Value, MediaError> {
    if !(200..300).contains(&status) {
        return Err(MediaError::Api {
            backend: backend.to_string(),
            status,
            message: error_text(body),
        });
    }
    serde_json::from_str(body)
        .map_err(|e| MediaError::Failed(format!("{backend} sent a response that could not be read: {e}")))
}

/// Sends `request` and returns the JSON body of a successful response.
pub(crate) async fn fetch_json(backend: &str, request: reqwest::RequestBuilder) -> Result<Value, MediaError> {
    let (status, body) = send(backend, request).await?;
    json_reply(backend, status, &body)
}

/// Fetches a generated file from where the service left it, without credentials: file hosts are
/// not the service's API.
pub(crate) async fn download(
    backend: &str,
    client: &reqwest::Client,
    url: &str,
    name: Option<&str>,
) -> Result<MediaFile, MediaError> {
    let failed = |error: reqwest::Error| {
        MediaError::Network(format!(
            "a file {backend} generated could not be downloaded: {}",
            describe(error)
        ))
    };
    let response = client
        .get(url)
        .timeout(DOWNLOAD_TIMEOUT)
        .send()
        .await
        .map_err(failed)?;
    let status = response.status();
    if !status.is_success() {
        return Err(MediaError::Failed(format!(
            "a file {backend} generated could not be downloaded: HTTP {status}"
        )));
    }
    let mime = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(str::to_string);
    let bytes = response.bytes().await.map_err(failed)?;
    Ok(MediaFile::detect(
        bytes,
        mime.as_deref(),
        Some(name.unwrap_or(url)),
    ))
}

/// A file a service returned as base64 text; `mime` is the type the service states, if any.
pub(crate) fn decode_file(backend: &str, encoded: &str, mime: Option<&str>) -> Result<MediaFile, MediaError> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|e| MediaError::Failed(format!("{backend} sent a file that is not valid base64: {e}")))?;
    Ok(MediaFile::detect(Bytes::from(bytes), mime, None))
}

/// The MIME type and base64 content of a local file sent as reference media.
pub(crate) async fn read_input(path: &Path) -> Result<(String, String), MediaError> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|e| MediaError::Input(format!("cannot read {}: {e}", path.display())))?;
    let mime = image_mime(&bytes)
        .or_else(|| mime_for_ext(&extension_of(&path.to_string_lossy())?))
        .unwrap_or(OCTET_STREAM);
    Ok((
        mime.to_string(),
        base64::engine::general_purpose::STANDARD.encode(&bytes),
    ))
}

/// A local file as a `data:` URL.
pub(crate) async fn data_url(path: &Path) -> Result<String, MediaError> {
    let (mime, data) = read_input(path).await?;
    Ok(format!("data:{mime};base64,{data}"))
}

/// The extension a file of this MIME type is saved with.
pub(crate) fn ext_for_mime(mime: &str) -> Option<&'static str> {
    let mime = if mime == "image/jpg" { "image/jpeg" } else { mime };
    TYPES.iter().find(|(_, m)| *m == mime).map(|(ext, _)| *ext)
}

/// The MIME type of files with this extension (lower case, no dot).
pub(crate) fn mime_for_ext(ext: &str) -> Option<&'static str> {
    TYPES.iter().find(|(e, _)| *e == ext).map(|(_, mime)| *mime)
}

/// The lower-case extension of a file name or URL, with `jpeg` as `jpg`.
pub(crate) fn extension_of(name: &str) -> Option<String> {
    let path = name.split(['?', '#']).next()?;
    let file = path.rsplit(['/', '\\']).next()?;
    let (_, ext) = file.rsplit_once('.')?;
    let plausible = (1..=5).contains(&ext.len()) && ext.chars().all(|c| c.is_ascii_alphanumeric());
    let ext = ext.to_ascii_lowercase();
    plausible.then(|| if ext == "jpeg" { "jpg".to_string() } else { ext })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_typed_by_content_then_name_then_stated_type() {
        let png = Bytes::from_static(b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR");
        let file = MediaFile::detect(png, Some("image/jpeg"), Some("https://cdn.example/a.jpeg"));
        assert_eq!((file.mime.as_str(), file.ext.as_str()), ("image/png", "png"));

        let named = MediaFile::detect(
            Bytes::from_static(b"ID3"),
            Some("application/octet-stream"),
            Some("https://cdn.example/out/boom.MP3?sig=a.b"),
        );
        assert_eq!((named.mime.as_str(), named.ext.as_str()), ("audio/mpeg", "mp3"));

        let stated = MediaFile::detect(
            Bytes::from_static(b"...."),
            Some("video/mp4; codecs=avc1"),
            Some("https://cdn.example/download"),
        );
        assert_eq!((stated.mime.as_str(), stated.ext.as_str()), ("video/mp4", "mp4"));

        let unknown = MediaFile::detect(Bytes::from_static(b"...."), None, None);
        assert_eq!(
            (unknown.mime.as_str(), unknown.ext.as_str()),
            (OCTET_STREAM, "bin")
        );
    }

    #[test]
    fn error_bodies_of_each_service_are_reduced_to_their_message() {
        assert_eq!(
            error_text(r#"{"error":{"message":"Invalid size","type":"invalid_request_error"}}"#),
            "Invalid size"
        );
        assert_eq!(
            error_text(r#"{"detail":"Request timed out","error_type":"request_timeout"}"#),
            "Request timed out"
        );
        assert_eq!(
            error_text(r#"{"detail":[{"loc":["body","prompt"],"msg":"too long"},{"msg":"bad seed"}]}"#),
            "too long; bad seed"
        );
        assert_eq!(error_text("  upstream unavailable \n"), "upstream unavailable");
        assert_eq!(error_text(""), "the response had no body");
    }
}
