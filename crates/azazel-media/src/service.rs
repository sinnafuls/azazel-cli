//! The media service: which backends can be used, which model each kind of media uses, and where
//! generated files go.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_ai::providers::presets;
use azazel_ai::{Auth, Credential, Credentials, ProviderConfig};
use azazel_core::config::MediaConfig;
use azazel_core::{Config, Paths};
use serde_json::{Map, Value};
use tokio_util::sync::CancellationToken;

use crate::backend::{MediaBackend, MediaFile, MediaKind, MediaRequest};
use crate::codex::{CodexImages, CodexOff};
use crate::fal::{FAL_URL, Fal, POLL_INTERVAL};
use crate::google::GoogleImages;
use crate::openai::OpenAiImages;
use crate::sprite::ImageSource;

/// Environment variable holding the fal key; `auth.json` may hold one under `fal` instead.
const FAL_KEY: &str = "FAL_KEY";

/// A backend that is ready, or a provider whose credential is fetched when it is first needed
/// (a key command should not run before something is generated).
enum Entry {
    Ready(Arc<dyn MediaBackend>),
    Provider {
        provider: Box<ProviderConfig>,
        credentials: Arc<Credentials>,
        /// Why `openai-codex` is switched off, per account, kept here because a backend is made
        /// for each call.
        codex_off: CodexOff,
    },
}

impl Entry {
    fn id(&self) -> &str {
        match self {
            Entry::Ready(backend) => backend.id(),
            Entry::Provider { provider, .. } => &provider.id,
        }
    }

    async fn backend(&self) -> Result<Arc<dyn MediaBackend>, String> {
        match self {
            Entry::Ready(backend) => Ok(backend.clone()),
            Entry::Provider {
                provider,
                credentials,
                codex_off,
            } => {
                let auth = credentials.resolve(provider).await.map_err(|e| e.to_string())?;
                if auth == Auth::None {
                    return Err(format!("no credentials for {}", provider.id));
                }
                Ok(match provider.id.as_str() {
                    "google" => Arc::new(GoogleImages::new(provider.base_url.clone(), auth)),
                    "openai-codex" => {
                        let headers = auth.login_headers(provider);
                        Arc::new(
                            CodexImages::new(provider.base_url.clone(), auth, headers)
                                .sharing(codex_off.clone()),
                        )
                    }
                    _ => Arc::new(OpenAiImages::new(provider.base_url.clone(), auth)),
                })
            }
        }
    }
}

pub struct Media {
    entries: Vec<Entry>,
    models: MediaConfig,
    project_root: PathBuf,
}

impl Media {
    pub fn new(config: &MediaConfig, project_root: PathBuf, backends: Vec<Arc<dyn MediaBackend>>) -> Media {
        Media {
            entries: backends.into_iter().map(Entry::Ready).collect(),
            models: config.clone(),
            project_root,
        }
    }

    /// The backends whose credentials are present, pointed at the real services: `fal` with
    /// `FAL_KEY` (or an `auth.json` entry `fal`), `openai`, `google` and `openai-codex` with the
    /// credentials of the providers of those names.
    pub async fn from_config(config: &Config, paths: &Paths, credentials: Arc<Credentials>) -> Media {
        let mut entries = Vec::new();
        let stored = || match credentials.load().remove("fal") {
            Some(Credential::ApiKey { key }) => Some(key),
            _ => None,
        };
        let fal_key = std::env::var(FAL_KEY)
            .ok()
            .filter(|key| !key.trim().is_empty())
            .or_else(stored);
        if let Some(key) = fal_key {
            entries.push(Entry::Ready(Arc::new(Fal::new(
                FAL_URL,
                key.trim(),
                POLL_INTERVAL,
            ))));
        }
        for provider in presets::resolve(&config.providers).unwrap_or_default() {
            if matches!(provider.id.as_str(), "openai" | "google" | "openai-codex")
                && credentials.source(&provider).is_some()
            {
                entries.push(Entry::Provider {
                    provider: Box::new(provider),
                    credentials: credentials.clone(),
                    codex_off: CodexOff::default(),
                });
            }
        }
        Media {
            entries,
            models: config.media.clone(),
            project_root: paths.project_root.clone(),
        }
    }

    /// Ids of the usable backends.
    pub fn backends(&self) -> Vec<String> {
        self.entries.iter().map(|entry| entry.id().to_string()).collect()
    }

    pub fn project_root(&self) -> &Path {
        &self.project_root
    }

    /// `media.output_dir`, relative to the project root.
    pub fn output_dir(&self) -> &str {
        &self.models.output_dir
    }

    /// The configured `<backend>:<model id>` for `kind`, when one is set.
    pub fn configured(&self, kind: MediaKind) -> Option<&str> {
        let models = &self.models.models;
        let spec = match kind {
            MediaKind::Image => &models.image,
            MediaKind::Video => &models.video,
            MediaKind::Model3d => &models.model3d,
            MediaKind::Audio => &models.audio,
        };
        Some(spec.trim()).filter(|spec| !spec.is_empty())
    }

    /// The `<backend>:<model id>` a generation of `kind` would use: `model` when given, else the
    /// configured one.
    pub fn model_for(&self, kind: MediaKind, model: Option<&str>) -> Result<String, String> {
        let chosen = model
            .map(str::trim)
            .filter(|spec| !spec.is_empty())
            .or(self.configured(kind));
        chosen.map(str::to_string).ok_or_else(|| {
            format!("no {kind} model configured: set [media.models] {kind} = \"<backend>:<model id>\"")
        })
    }

    fn entry(&self, spec: &str, kind: MediaKind) -> Result<(&Entry, String), String> {
        let usable = || match self.backends() {
            ids if ids.is_empty() => "no media backend has credentials".to_string(),
            ids => format!("usable backends: {}", ids.join(", ")),
        };
        let (backend, model) = spec
            .split_once(':')
            .map(|(backend, model)| (backend.trim(), model.trim()))
            .filter(|(backend, model)| !backend.is_empty() && !model.is_empty())
            .ok_or_else(|| format!("`{spec}` is not `<backend>:<model id>`; {}", usable()))?;
        let entry = self
            .entries
            .iter()
            .find(|entry| entry.id() == backend)
            .ok_or_else(|| {
                format!(
                    "media backend `{backend}` is unknown or has no credentials; {}",
                    usable()
                )
            })?;
        let images_only = matches!(entry, Entry::Provider { .. });
        if images_only && kind != MediaKind::Image {
            return Err(format!(
                "the `{backend}` backend generates images only, not {kind}"
            ));
        }
        Ok((entry, model.to_string()))
    }

    /// Generates with the model named by `model`, else the configured one for `kind`.
    #[allow(clippy::too_many_arguments)]
    pub async fn generate(
        &self,
        kind: MediaKind,
        model: Option<&str>,
        prompt: &str,
        inputs: &[PathBuf],
        params: Map<String, Value>,
        count: u8,
        cancel: CancellationToken,
    ) -> Result<Vec<MediaFile>, String> {
        let spec = self.model_for(kind, model)?;
        let (entry, model) = self.entry(&spec, kind)?;
        let backend = entry.backend().await?;
        if !backend.kinds().contains(&kind) {
            return Err(format!("the `{}` backend does not generate {kind}", backend.id()));
        }
        let request = MediaRequest {
            kind,
            model,
            prompt: prompt.to_string(),
            inputs: inputs.to_vec(),
            params,
            count: count.max(1),
        };
        let files = backend
            .generate(&request, cancel)
            .await
            .map_err(|e| e.to_string())?;
        if files.is_empty() {
            return Err(format!("{spec} returned no file"));
        }
        Ok(files)
    }
}

#[async_trait]
impl ImageSource for Media {
    async fn image(&self, prompt: &str, cancel: CancellationToken) -> Result<Vec<u8>, String> {
        let files = self
            .generate(MediaKind::Image, None, prompt, &[], Map::new(), 1, cancel)
            .await?;
        let file = files
            .into_iter()
            .next()
            .ok_or("the image model returned nothing")?;
        if file.mime == "image/png" {
            return Ok(file.bytes.to_vec());
        }
        tokio::task::spawn_blocking(move || {
            let image = image::load_from_memory(&file.bytes).map_err(|e| format!("unreadable image: {e}"))?;
            let mut png = Vec::new();
            image
                .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
                .map_err(|e| e.to_string())?;
            Ok(png)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use super::*;

    fn login(extra: &[(&str, &str)]) -> Credential {
        Credential::OAuth {
            access: "access-token".into(),
            refresh: "refresh-token".into(),
            expires: azazel_ai::credentials::now_ms() + 3_600_000,
            extra: extra
                .iter()
                .map(|(name, value)| (name.to_string(), serde_json::json!(value)))
                .collect::<BTreeMap<_, _>>(),
        }
    }

    async fn backends(store: impl FnOnce(&Credentials)) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let credentials = Credentials::with_env(dir.path().join("auth.json"), Arc::new(|_: &str| None));
        store(&credentials);
        let paths = Paths::discover(dir.path(), Some(&dir.path().join("home").to_string_lossy()));
        Media::from_config(&Config::default(), &paths, Arc::new(credentials))
            .await
            .backends()
    }

    #[tokio::test]
    async fn a_chatgpt_login_makes_the_codex_backend_usable() {
        let with = backends(|c| {
            c.store("openai-codex", login(&[("account_id", "acct-7")]))
                .unwrap()
        })
        .await;
        assert!(with.contains(&"openai-codex".to_string()), "{with:?}");
        assert!(!backends(|_| {}).await.contains(&"openai-codex".to_string()));
    }
}
