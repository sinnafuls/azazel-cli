//! The `sprite_sheet` tool: frames in; a packed sheet and a descriptor for the engine out.

pub mod atlas;
pub mod export;
pub mod frames;
pub mod pack;

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use image::RgbaImage;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use self::atlas::{Packed, layout, render};
use self::export::{Target, export};
use self::frames::{AnimationSpec, auto_key, frame_names, key, slice, trim, whole};

pub const NO_IMAGE_MODEL: &str =
    "no image model configured: set [media.models] image = \"<backend>:<model id>\"";

const DEFAULT_TOLERANCE: u32 = 24;
const DEFAULT_PADDING: u32 = 2;
const DEFAULT_MAX_SIZE: u32 = 4096;
/// Most cells a generated or sliced grid may have.
const MAX_CELLS: u32 = 1024;

/// Where frames come from when the tool is asked to generate them.
#[async_trait]
pub trait ImageSource: Send + Sync {
    /// One image for `prompt`, as PNG bytes.
    async fn image(&self, prompt: &str, cancel: CancellationToken) -> Result<Vec<u8>, String>;
}

/// What is appended to the prompt of a generated sheet so that it can be cut into cells.
fn grid_prompt(prompt: &str, cols: u32, rows: u32) -> String {
    format!(
        "{}\n\nLay this out as a sprite sheet: a grid of exactly {cols} columns and {rows} rows of \
         equally sized frames that fill the whole image, read left to right, top to bottom. The same \
         subject at the same size and position in every frame. Flat solid #FF00FF background. No \
         gaps, borders, grid lines, numbers or text.",
        prompt.trim()
    )
}

/// The colour made transparent before frames are cut out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyColor {
    None,
    /// The most common colour of the first frame's four corners.
    Auto,
    Rgb([u8; 3]),
}

impl KeyColor {
    /// `none`, `auto` or `#RRGGBB`.
    pub fn parse(text: &str) -> Result<KeyColor, String> {
        let invalid = || format!("key_color must be `none`, `auto` or `#RRGGBB`, got `{text}`");
        match text.trim() {
            "none" => Ok(KeyColor::None),
            "auto" => Ok(KeyColor::Auto),
            other => {
                let hex = other
                    .strip_prefix('#')
                    .filter(|hex| hex.len() == 6 && hex.is_ascii())
                    .ok_or_else(invalid)?;
                let channel = |at: usize| u8::from_str_radix(&hex[at..at + 2], 16).map_err(|_| invalid());
                Ok(KeyColor::Rgb([channel(0)?, channel(2)?, channel(4)?]))
            }
        }
    }
}

/// How frames become a sheet.
#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub name: String,
    pub animations: Vec<AnimationSpec>,
    pub key_color: KeyColor,
    pub tolerance: u32,
    pub trim: bool,
    pub padding: u32,
    pub max_size: u32,
    pub target: Target,
}

/// A finished sheet: its layout, one image per page, and the descriptor files as `(name, text)`.
pub struct Sheet {
    pub packed: Packed,
    pub images: Vec<RgbaImage>,
    pub descriptors: Vec<(String, String)>,
}

/// Keys, trims, names and packs `cells` (the frames in order) and describes the result for the
/// target. `res_dir` is where the files will live, relative to the project root.
pub fn build(mut cells: Vec<RgbaImage>, options: &Options, res_dir: &str) -> Result<Sheet, String> {
    if cells.is_empty() {
        return Err("there are no frames to pack".to_string());
    }
    let color = match options.key_color {
        KeyColor::None => None,
        KeyColor::Auto => auto_key(&cells[0]),
        KeyColor::Rgb(color) => Some(color),
    };
    if let Some(color) = color {
        for cell in &mut cells {
            key(cell, color, options.tolerance);
        }
    }
    let naming = frame_names(&options.name, cells.len(), &options.animations)?;
    let trimming = options.trim && options.target.allows_trim();
    let frames: Vec<_> = naming
        .frames
        .into_iter()
        .zip(cells)
        .map(|(name, cell)| {
            if trimming {
                trim(name, &cell)
            } else {
                whole(name, cell)
            }
        })
        .collect();
    let max_size = options
        .target
        .max_size()
        .map_or(options.max_size, |limit| options.max_size.min(limit));
    let packed = layout(
        &options.name,
        &frames,
        naming.animations,
        options.padding,
        max_size,
    )?;
    let images = render(&packed, &frames).collect();
    let descriptors = export(options.target, &packed, res_dir);
    Ok(Sheet {
        packed,
        images,
        descriptors,
    })
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

#[derive(Debug, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
struct Source {
    frames: Option<Vec<String>>,
    sheet: Option<String>,
    prompt: Option<String>,
    cols: Option<u32>,
    rows: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct Input {
    source: Source,
    name: String,
    #[serde(default)]
    animations: Vec<AnimationSpec>,
    key_color: Option<String>,
    tolerance: Option<u32>,
    trim: Option<bool>,
    padding: Option<u32>,
    max_size: Option<u32>,
    target: String,
}

/// The `sprite_sheet` tool.
pub struct SpriteSheetTool {
    project_root: PathBuf,
    /// `media.output_dir`, relative to the project root.
    output_dir: String,
    source: Option<Arc<dyn ImageSource>>,
}

impl SpriteSheetTool {
    pub fn new(project_root: PathBuf, output_dir: String, source: Option<Arc<dyn ImageSource>>) -> Self {
        SpriteSheetTool {
            project_root,
            output_dir,
            source,
        }
    }

    /// Where the sheet of `input` goes, as given (relative to the project root).
    fn out_dir_of(&self, input: &Value) -> String {
        match input.get("out_dir").and_then(Value::as_str).map(str::trim) {
            Some(dir) if !dir.is_empty() => dir.to_string(),
            _ => {
                let name = input.get("name").and_then(Value::as_str).unwrap_or_default();
                format!("{}/sprites/{name}", self.output_dir.trim_end_matches(['/', '\\']))
            }
        }
    }

    /// The output directory as an absolute path and as a `/`-separated path below the project
    /// root; refused when it lies outside the project.
    fn resolve_out(&self, dir: &str) -> Result<(PathBuf, String), String> {
        let root = clean(&self.project_root);
        let absolute = clean(&root.join(dir));
        let relative = absolute
            .strip_prefix(&root)
            .map_err(|_| format!("out_dir `{dir}` is outside the project; sheets are written inside it"))?;
        let relative = relative.to_string_lossy().replace('\\', "/");
        Ok((absolute, relative))
    }

    fn load(&self, path: &str) -> Result<RgbaImage, String> {
        let file = self.project_root.join(path);
        image::open(&file)
            .map(|image| image.to_rgba8())
            .map_err(|e| format!("{path}: {e}"))
    }

    /// The frames of `source`, in order, and whether they were generated.
    async fn cells(
        &self,
        source: &Source,
        cancel: &CancellationToken,
    ) -> Result<(Vec<RgbaImage>, bool), String> {
        let grid = || match (source.cols, source.rows) {
            (Some(cols), Some(rows)) if cols > 0 && rows > 0 && cols.saturating_mul(rows) <= MAX_CELLS => {
                Ok((cols, rows))
            }
            (Some(_), Some(_)) => Err(format!("cols × rows must be between 1 and {MAX_CELLS}")),
            _ => Err("this source needs `cols` and `rows`".to_string()),
        };
        match (&source.frames, &source.sheet, &source.prompt) {
            (Some(paths), None, None) => {
                let cells = paths
                    .iter()
                    .map(|path| self.load(path))
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((cells, false))
            }
            (None, Some(sheet), None) => {
                let (cols, rows) = grid()?;
                Ok((slice(&self.load(sheet)?, cols, rows)?, false))
            }
            (None, None, Some(prompt)) => {
                let (cols, rows) = grid()?;
                let source = self.source.as_ref().ok_or_else(|| NO_IMAGE_MODEL.to_string())?;
                let bytes = source
                    .image(&grid_prompt(prompt, cols, rows), cancel.clone())
                    .await?;
                let sheet = image::load_from_memory(&bytes)
                    .map_err(|e| format!("the generated sheet is not a readable image: {e}"))?
                    .to_rgba8();
                Ok((slice(&sheet, cols, rows)?, true))
            }
            _ => Err(
                "`source` must be exactly one of { frames }, { sheet, cols, rows } or { prompt, cols, rows }"
                    .to_string(),
            ),
        }
    }

    async fn run(&self, input: Value, cancel: &CancellationToken) -> Result<String, String> {
        let out_dir = self.out_dir_of(&input);
        let input: Input = serde_json::from_value(input).map_err(|e| format!("invalid input: {e}"))?;
        let target = Target::parse(&input.target).ok_or_else(|| {
            format!(
                "unknown target `{}`; targets: {}",
                input.target,
                Target::NAMES.join(", ")
            )
        })?;
        let (dir, res_dir) = self.resolve_out(&out_dir)?;
        let (cells, generated) = self.cells(&input.source, cancel).await?;
        let key_color = match &input.key_color {
            Some(text) => KeyColor::parse(text)?,
            None if generated => KeyColor::Auto,
            None => KeyColor::None,
        };
        let options = Options {
            name: input.name,
            animations: input.animations,
            key_color,
            tolerance: input.tolerance.unwrap_or(DEFAULT_TOLERANCE),
            trim: input.trim.unwrap_or(true),
            padding: input.padding.unwrap_or(DEFAULT_PADDING),
            max_size: input.max_size.unwrap_or(DEFAULT_MAX_SIZE),
            target,
        };
        let target_name = input.target;

        tokio::task::spawn_blocking(move || -> Result<String, String> {
            let sheet = build(cells, &options, &res_dir)?;
            std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
            let mut written = Vec::new();
            for (page, image) in sheet.packed.pages.iter().zip(&sheet.images) {
                let path = dir.join(&page.file);
                image
                    .save(&path)
                    .map_err(|e| format!("{}: {e}", path.display()))?;
                written.push(path);
            }
            for (name, text) in &sheet.descriptors {
                let path = dir.join(name);
                std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))?;
                written.push(path);
            }

            let sizes: Vec<String> = sheet
                .packed
                .pages
                .iter()
                .map(|page| format!("{}×{}", page.width, page.height))
                .collect();
            let mut report = format!(
                "packed {} frames into {} ({}) for {}\n",
                sheet.packed.frames.len(),
                if sizes.len() == 1 {
                    "one sheet".to_string()
                } else {
                    format!("{} sheets", sizes.len())
                },
                sizes.join(", "),
                target_name,
            );
            for path in &written {
                let bytes = std::fs::metadata(path).map(|meta| meta.len()).unwrap_or(0);
                report.push_str(&format!("{} ({bytes} bytes)\n", path.display()));
            }
            if options.target == Target::Roblox {
                report
                    .push_str("Upload the image to Roblox and put its asset id in `image` of the module.\n");
            }
            if options.trim && !options.target.allows_trim() {
                report.push_str("Frames were not trimmed: Unity slices by rectangle.\n");
            }
            Ok(report)
        })
        .await
        .map_err(|e| e.to_string())?
    }
}

#[async_trait]
impl Tool for SpriteSheetTool {
    fn name(&self) -> &str {
        "sprite_sheet"
    }

    fn description(&self) -> &str {
        "Pack animation frames into a sprite sheet with a descriptor for a game engine. Frames come \
         from image files, from an existing grid sheet, or from an image generated for a prompt. A flat \
         background colour can be keyed out, transparent borders are trimmed, and the result is written \
         for Godot (SpriteFrames .tres), Unity (sliced .meta), Unreal (Paper2D), Roblox (Luau module) \
         or as TexturePacker-style JSON."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "source": {
                    "type": "object",
                    "description": "Exactly one of: { frames: [paths] }, { sheet: path, cols, rows }, { prompt, cols, rows }. Paths are relative to the project root",
                    "properties": {
                        "frames": { "type": "array", "items": { "type": "string" }, "description": "Image files, in frame order" },
                        "sheet": { "type": "string", "description": "An image holding a grid of equal cells" },
                        "prompt": { "type": "string", "description": "What to draw; an image model generates the grid" },
                        "cols": { "type": "integer", "minimum": 1 },
                        "rows": { "type": "integer", "minimum": 1 }
                    }
                },
                "name": { "type": "string", "description": "Name of the sheet and its files: letters, digits, _ and -" },
                "animations": {
                    "type": "array",
                    "description": "Frame ranges that play together; frames are numbered from 0 in source order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "name": { "type": "string" },
                            "from": { "type": "integer", "minimum": 0 },
                            "to": { "type": "integer", "minimum": 0, "description": "Last frame, inclusive" },
                            "fps": { "type": "number", "exclusiveMinimum": 0 }
                        },
                        "required": ["name", "from", "to", "fps"]
                    }
                },
                "key_color": { "type": "string", "description": "Background colour to make transparent: none, auto (corner colour) or #RRGGBB. Default none; auto for prompt sources" },
                "tolerance": { "type": "integer", "minimum": 0, "description": "How far (RGB distance) a pixel may be from the key colour and still be removed; default 24" },
                "trim": { "type": "boolean", "description": "Cut transparent borders off each frame; default true" },
                "padding": { "type": "integer", "minimum": 0, "description": "Pixels between frames; default 2" },
                "max_size": { "type": "integer", "minimum": 1, "description": "Largest sheet side; more frames go to further sheets; default 4096" },
                "target": { "type": "string", "enum": Target::NAMES },
                "out_dir": { "type": "string", "description": "Directory for the files, relative to the project root; default <media output dir>/sprites/<name>" }
            },
            "required": ["source", "name", "target"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        vec![self.out_dir_of(input)]
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let done = tokio::select! {
            done = self.run(input, &ctx.cancel) => done,
            _ = ctx.cancel.cancelled() => Err(ABORTED_TEXT.to_string()),
        };
        match done {
            Ok(report) => ToolOutput::text(report),
            Err(why) => ToolOutput::error(why),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_output_directory_stays_inside_the_project() {
        let tool = SpriteSheetTool::new(PathBuf::from("/work/game"), "assets/generated/".into(), None);
        assert_eq!(
            tool.out_dir_of(&json!({ "name": "hero" })),
            "assets/generated/sprites/hero"
        );
        assert_eq!(
            tool.out_dir_of(&json!({ "name": "hero", "out_dir": " art/hero " })),
            "art/hero"
        );
        let (_, inside) = tool.resolve_out("art/../assets/./hero").unwrap();
        assert_eq!(inside, "assets/hero");
        assert_eq!(tool.resolve_out("").unwrap().1, "");
        for outside in ["../elsewhere", "art/../../x", "/tmp/x"] {
            assert!(
                tool.resolve_out(outside)
                    .unwrap_err()
                    .contains("outside the project"),
                "{outside}"
            );
        }
    }
}
