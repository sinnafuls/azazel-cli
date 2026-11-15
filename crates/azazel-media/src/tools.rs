//! The `generate_image`, `generate_video`, `generate_3d` and `generate_audio` tools.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::Block;
use azazel_core::media::image_block;
use serde_json::{Map, Value, json};

use crate::backend::{MediaFile, MediaKind};
use crate::service::Media;

/// Long side, in pixels, of images returned inline.
const INLINE_SIDE: u32 = 1024;
/// Most files one call may ask for.
const MAX_COUNT: u64 = 8;
/// Words of the prompt that name a file.
const SLUG_WORDS: usize = 6;

pub const COST_NOTE: &str = "this creates files in the project and may cost money at the media provider";

/// The first six words of `prompt` as a file-name stem: lower-case ASCII letters and digits joined
/// by hyphens. `fallback` when the prompt has none.
pub fn slug(prompt: &str, fallback: &str) -> String {
    let words: Vec<String> = prompt
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| !word.is_empty())
        .take(SLUG_WORDS)
        .map(str::to_ascii_lowercase)
        .collect();
    if words.is_empty() {
        fallback.to_string()
    } else {
        words.join("-")
    }
}

/// `path` with `.` and `..` folded, without touching the file system.
fn clean(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other),
        }
    }
    out
}

/// Where file `index` of `total` goes. With `out`: that path below `root` (refused outside it),
/// given the file's extension when it has none, and `-<n>` before the extension when there are
/// several files. Without: `<root>/<output_dir>/<kind>/<slug>-<stamp>[-<n>].<ext>`.
#[allow(clippy::too_many_arguments)]
pub fn output_path(
    root: &Path,
    output_dir: &str,
    kind: MediaKind,
    out: Option<&str>,
    prompt: &str,
    stamp: &str,
    ext: &str,
    index: usize,
    total: usize,
) -> Result<PathBuf, String> {
    let numbered = |stem: &str| {
        if total > 1 {
            format!("{stem}-{}", index + 1)
        } else {
            stem.to_string()
        }
    };
    let root = clean(root);
    let Some(out) = out.map(str::trim).filter(|out| !out.is_empty()) else {
        let stem = numbered(&format!("{}-{stamp}", slug(prompt, kind.as_str())));
        return Ok(root
            .join(output_dir)
            .join(kind.as_str())
            .join(format!("{stem}.{ext}")));
    };
    let path = clean(&root.join(out));
    if !path.starts_with(&root) || path == root {
        return Err(format!("`out` must name a file inside the project, got `{out}`"));
    }
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().to_string())
        .unwrap_or_default();
    let extension = path
        .extension()
        .map_or_else(|| ext.to_string(), |e| e.to_string_lossy().to_string());
    Ok(path.with_file_name(format!("{}.{extension}", numbered(&stem))))
}

/// A generation tool for one kind of media.
pub struct GenerateTool {
    media: Arc<Media>,
    kind: MediaKind,
}

impl GenerateTool {
    pub fn new(media: Arc<Media>, kind: MediaKind) -> Self {
        GenerateTool { media, kind }
    }

    /// The input keys handed to the backend as parameters, for this kind.
    fn param_keys(&self) -> &'static [&'static str] {
        match self.kind {
            MediaKind::Image => &["size", "aspect_ratio", "transparent"],
            MediaKind::Video => &["duration_secs", "aspect_ratio"],
            MediaKind::Model3d => &["format"],
            MediaKind::Audio => &["kind", "duration_secs", "voice"],
        }
    }

    /// Local files the request refers to, relative to the project root.
    fn inputs(&self, input: &Value) -> Vec<PathBuf> {
        let root = self.media.project_root();
        let one = |key: &str| input.get(key).and_then(Value::as_str).map(|path| root.join(path));
        match self.kind {
            MediaKind::Image => input
                .get("reference_images")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .map(|path| root.join(path))
                .collect(),
            MediaKind::Video | MediaKind::Model3d => one("image").into_iter().collect(),
            MediaKind::Audio => Vec::new(),
        }
    }

    async fn run(&self, input: &Value, ctx: &ToolCtx) -> Result<Vec<Block>, String> {
        let text = |key: &str| {
            input
                .get(key)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|t| !t.is_empty())
        };
        let prompt = text("prompt").unwrap_or_default();
        let inputs = self.inputs(input);
        if prompt.is_empty() && !(self.kind == MediaKind::Model3d && !inputs.is_empty()) {
            return Err(match self.kind {
                MediaKind::Model3d => "give a `prompt` or an `image`".to_string(),
                _ => "`prompt` is required".to_string(),
            });
        }
        if self.kind == MediaKind::Audio && !matches!(text("kind"), Some("sfx" | "music" | "speech")) {
            return Err("`kind` must be `sfx`, `music` or `speech`".to_string());
        }
        let count = match input.get("count") {
            None | Some(Value::Null) => 1,
            Some(count) => count
                .as_u64()
                .filter(|n| (1..=MAX_COUNT).contains(n))
                .ok_or_else(|| format!("`count` must be between 1 and {MAX_COUNT}"))?,
        };
        let mut params = Map::new();
        for key in self.param_keys() {
            if let Some(value) = input.get(*key).filter(|value| !value.is_null()) {
                params.insert((*key).to_string(), value.clone());
            }
        }
        let model = self.media.model_for(self.kind, text("model"))?;
        // Checked before anything is spent: a bad `out` must not cost a generation.
        output_path(
            self.media.project_root(),
            "",
            self.kind,
            text("out"),
            prompt,
            "",
            "bin",
            0,
            1,
        )?;

        ctx.update.send(format!("generating with {model}"));
        let files = self
            .media
            .generate(
                self.kind,
                Some(&model),
                prompt,
                &inputs,
                params.clone(),
                count as u8,
                ctx.cancel.clone(),
            )
            .await?;

        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
        let total = files.len();
        let mut report = String::new();
        let mut blocks = Vec::new();
        for (index, file) in files.into_iter().enumerate() {
            let path = output_path(
                self.media.project_root(),
                self.media.output_dir(),
                self.kind,
                text("out"),
                prompt,
                &stamp,
                &file.ext,
                index,
                total,
            )?;
            let sidecar = json!({
                "kind": self.kind.as_str(),
                "prompt": prompt,
                "model": model,
                "parameters": params,
                "inputs": inputs.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                "seed": file.seed,
                "created_at": jiff::Timestamp::now().to_string(),
            });
            save(&path, &file, &sidecar).await?;
            report.push_str(&format!(
                "{} ({} bytes, {})\n",
                path.display(),
                file.bytes.len(),
                file.mime
            ));
            if self.kind == MediaKind::Image {
                let bytes = file.bytes.clone();
                let inline = tokio::task::spawn_blocking(move || image_block(&bytes, INLINE_SIDE)).await;
                if let Ok(Ok(block)) = inline {
                    blocks.push(block);
                }
            }
        }
        blocks.insert(0, Block::text(format!("generated with {model}:\n{report}")));
        Ok(blocks)
    }
}

/// Writes a generated file and, beside it, `<file>.json` saying how it was made.
async fn save(path: &Path, file: &MediaFile, sidecar: &Value) -> Result<(), String> {
    let failed = |e: std::io::Error| format!("{}: {e}", path.display());
    if let Some(dir) = path.parent() {
        tokio::fs::create_dir_all(dir).await.map_err(failed)?;
    }
    tokio::fs::write(path, &file.bytes).await.map_err(failed)?;
    let mut name = path.as_os_str().to_owned();
    name.push(".json");
    let text = serde_json::to_string_pretty(sidecar).unwrap_or_default();
    tokio::fs::write(PathBuf::from(name), text).await.map_err(failed)
}

#[async_trait]
impl Tool for GenerateTool {
    fn name(&self) -> &str {
        self.kind.tool_name()
    }

    fn description(&self) -> &str {
        match self.kind {
            MediaKind::Image => {
                "Generate one or more images from a prompt, optionally guided by reference images. The files \
                 are saved in the project with a .json beside each recording how it was made, and returned \
                 for you to look at."
            }
            MediaKind::Video => {
                "Generate a short video clip from a prompt, optionally starting from an image. Saved in the \
                 project with a .json beside it."
            }
            MediaKind::Model3d => {
                "Generate a 3D model from a prompt or from an image of the object. Saved in the project with \
                 a .json beside it."
            }
            MediaKind::Audio => {
                "Generate a sound effect, a piece of music or spoken text. Saved in the project with a .json \
                 beside it."
            }
        }
    }

    fn schema(&self) -> Value {
        let model = json!({ "type": "string", "description": "<backend>:<model id>; overrides the configured model" });
        let out = json!({ "type": "string", "description": "File to write, relative to the project root; default is under the media output directory" });
        let ratio = json!({ "type": "string", "description": "Width to height, e.g. 16:9" });
        let (properties, required) = match self.kind {
            MediaKind::Image => (
                json!({
                    "prompt": { "type": "string", "description": "What to draw: subject, composition, style, constraints" },
                    "size": { "type": "string", "description": "Width x height in pixels, e.g. 1024x1024" },
                    "aspect_ratio": ratio,
                    "count": { "type": "integer", "minimum": 1, "maximum": MAX_COUNT, "description": "How many images" },
                    "transparent": { "type": "boolean", "description": "Ask for a transparent background, when the model can" },
                    "reference_images": { "type": "array", "items": { "type": "string" }, "description": "Image files to guide style or subject" },
                    "model": model,
                    "out": out
                }),
                json!(["prompt"]),
            ),
            MediaKind::Video => (
                json!({
                    "prompt": { "type": "string", "description": "One shot: subject, action, camera, setting, light" },
                    "image": { "type": "string", "description": "Image file the clip starts from" },
                    "duration_secs": { "type": "integer", "minimum": 1 },
                    "aspect_ratio": ratio,
                    "model": model,
                    "out": out
                }),
                json!(["prompt"]),
            ),
            MediaKind::Model3d => (
                json!({
                    "prompt": { "type": "string", "description": "The object, its material and proportions" },
                    "image": { "type": "string", "description": "Image file of the object; more controllable than a prompt" },
                    "format": { "type": "string", "enum": ["glb", "obj", "fbx"], "description": "Default glb" },
                    "model": model,
                    "out": out
                }),
                json!([]),
            ),
            MediaKind::Audio => (
                json!({
                    "prompt": { "type": "string", "description": "The sound or music to make, or the words to speak" },
                    "kind": { "type": "string", "enum": ["sfx", "music", "speech"] },
                    "duration_secs": { "type": "integer", "minimum": 1 },
                    "voice": { "type": "string", "description": "Voice for speech, as the model names it" },
                    "model": model,
                    "out": out
                }),
                json!(["prompt", "kind"]),
            ),
        };
        json!({ "type": "object", "properties": properties, "required": required })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Net
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn approval_note(&self, _: &Value) -> Option<String> {
        Some(COST_NOTE.to_string())
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        match self.run(&input, ctx).await {
            Ok(content) => ToolOutput {
                content,
                is_error: false,
                details: None,
            },
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_paths_follow_out_or_the_media_directory() {
        let root = Path::new("/work/game");
        let at = |out: Option<&str>, index: usize, total: usize| {
            output_path(
                root,
                "assets/generated",
                MediaKind::Image,
                out,
                "red door",
                "20261111T000000Z",
                "png",
                index,
                total,
            )
        };
        assert_eq!(
            at(None, 0, 1).unwrap(),
            root.join("assets/generated/image/red-door-20261111T000000Z.png")
        );
        assert_eq!(
            at(None, 1, 3).unwrap(),
            root.join("assets/generated/image/red-door-20261111T000000Z-2.png")
        );
        // `out` is kept as given; its extension is added only when missing.
        assert_eq!(
            at(Some("art/door.webp"), 0, 1).unwrap(),
            root.join("art/door.webp")
        );
        assert_eq!(at(Some("art/door"), 0, 1).unwrap(), root.join("art/door.png"));
        assert_eq!(
            at(Some("art/door.png"), 2, 3).unwrap(),
            root.join("art/door-3.png")
        );
        assert_eq!(at(Some("art/../door.png"), 0, 1).unwrap(), root.join("door.png"));
        for outside in ["../door.png", "art/../../door.png", "/tmp/door.png", "."] {
            assert!(
                at(Some(outside), 0, 1)
                    .unwrap_err()
                    .contains("inside the project"),
                "{outside}"
            );
        }
    }
}
