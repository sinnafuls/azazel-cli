//! Descriptor files that tell an engine where each frame sits in the sheet.

use std::fmt::Write as _;

use serde_json::{Map, Value, json};

use super::atlas::{Packed, PackedFrame};
use super::frames::Animation;

/// The engine or format a sheet is described for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target {
    Json,
    Godot,
    Unity,
    Roblox,
    Unreal,
}

impl Target {
    pub const NAMES: [&'static str; 5] = ["json", "godot", "unity", "roblox", "unreal"];

    pub fn parse(name: &str) -> Option<Target> {
        match name {
            "json" => Some(Target::Json),
            "godot" => Some(Target::Godot),
            "unity" => Some(Target::Unity),
            "roblox" => Some(Target::Roblox),
            "unreal" => Some(Target::Unreal),
            _ => None,
        }
    }

    /// Unity slices sprites by rectangle and has no notion of a trimmed frame's original size.
    pub fn allows_trim(self) -> bool {
        self != Target::Unity
    }

    /// The largest sheet side this target takes, when it has a limit of its own.
    pub fn max_size(self) -> Option<u32> {
        (self == Target::Roblox).then_some(1024)
    }
}

/// The descriptor files for `packed` as `(file name, text)`. `res_dir` is the directory the files
/// go to, relative to the project root with `/` separators (empty for the root itself); Godot
/// resources refer to the sheet by that path.
pub fn export(target: Target, packed: &Packed, res_dir: &str) -> Vec<(String, String)> {
    match target {
        Target::Json => json_hash(packed, "json", false),
        Target::Unreal => json_hash(packed, "paper2dsprites", true),
        Target::Godot => vec![(format!("{}.tres", packed.name), godot(packed, res_dir))],
        Target::Unity => unity(packed),
        Target::Roblox => vec![(format!("{}.luau", packed.name), roblox(packed))],
    }
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

fn animations_json(animations: &[Animation]) -> Value {
    let map: Map<String, Value> = animations
        .iter()
        .map(|animation| (animation.name.clone(), json!(animation.frames)))
        .collect();
    Value::Object(map)
}

/// TexturePacker's JSON-hash document, one per page. The first page's document carries the
/// animations and names the documents of the other pages. `paper2d` adds what Unreal's Paper2D
/// importer reads besides: a pivot per frame and the target marker.
fn json_hash(packed: &Packed, extension: &str, paper2d: bool) -> Vec<(String, String)> {
    let file_of = |page: usize| format!("{}.{extension}", packed.pages[page].stem());
    packed
        .pages
        .iter()
        .enumerate()
        .map(|(index, page)| {
            let mut frames = Map::new();
            for frame in packed.frames.iter().filter(|frame| frame.page == index) {
                let mut entry = json!({
                    "frame": { "x": frame.x, "y": frame.y, "w": frame.width, "h": frame.height },
                    "rotated": false,
                    "trimmed": frame.trimmed(),
                    "spriteSourceSize": {
                        "x": frame.offset_x, "y": frame.offset_y, "w": frame.width, "h": frame.height
                    },
                    "sourceSize": { "w": frame.source_width, "h": frame.source_height },
                });
                if paper2d {
                    entry["pivot"] = json!({ "x": 0.5, "y": 0.5 });
                }
                frames.insert(frame.name.clone(), entry);
            }
            let mut meta = json!({
                "image": page.file,
                "size": { "w": page.width, "h": page.height },
                "scale": "1",
            });
            if paper2d {
                meta["target"] = json!("paper2d");
            }
            let mut document = Map::new();
            document.insert("frames".into(), Value::Object(frames));
            if index == 0 {
                document.insert("animations".into(), animations_json(&packed.animations));
                if packed.pages.len() > 1 {
                    let others: Vec<String> = (1..packed.pages.len()).map(file_of).collect();
                    meta["related_multi_packs"] = json!(others);
                }
            }
            document.insert("meta".into(), meta);
            (file_of(index), pretty(&Value::Object(document)))
        })
        .collect()
}

/// A float the way Godot's text resources write it: always with a decimal point.
fn real(value: f64) -> String {
    format!("{value:?}")
}

/// The animations a Godot `SpriteFrames` needs: the ones given, plus `default` holding the frames
/// that belong to none (all of them when no animation was given).
fn godot_animations(packed: &Packed) -> Vec<Animation> {
    let mut animations = packed.animations.clone();
    let loose: Vec<String> = packed
        .frames
        .iter()
        .filter(|frame| !animations.iter().any(|a| a.frames.contains(&frame.name)))
        .map(|frame| frame.name.clone())
        .collect();
    if !loose.is_empty() && !animations.iter().any(|a| a.name == "default") {
        animations.push(Animation {
            name: "default".to_string(),
            fps: 5.0,
            frames: loose,
        });
    }
    animations
}

/// A Godot 4 `SpriteFrames` text resource: one `AtlasTexture` per frame over the sheet image(s).
fn godot(packed: &Packed, res_dir: &str) -> String {
    let mut out = String::new();
    let load_steps = packed.pages.len() + packed.frames.len() + 1;
    let _ = writeln!(
        out,
        "[gd_resource type=\"SpriteFrames\" load_steps={load_steps} format=3]\n"
    );
    let dir = res_dir.trim_matches('/');
    for (index, page) in packed.pages.iter().enumerate() {
        let path = if dir.is_empty() {
            page.file.clone()
        } else {
            format!("{dir}/{}", page.file)
        };
        let _ = writeln!(
            out,
            "[ext_resource type=\"Texture2D\" path=\"res://{path}\" id=\"{}\"]",
            index + 1
        );
    }
    for frame in &packed.frames {
        let _ = writeln!(
            out,
            "\n[sub_resource type=\"AtlasTexture\" id=\"AtlasTexture_{}\"]",
            frame.name
        );
        let _ = writeln!(out, "atlas = ExtResource(\"{}\")", frame.page + 1);
        let _ = writeln!(
            out,
            "region = Rect2({}, {}, {}, {})",
            frame.x, frame.y, frame.width, frame.height
        );
        if frame.trimmed() {
            // The margin puts the cut-off transparent border back, so frames keep their alignment.
            let _ = writeln!(
                out,
                "margin = Rect2({}, {}, {}, {})",
                frame.offset_x,
                frame.offset_y,
                frame.source_width - frame.width,
                frame.source_height - frame.height
            );
        }
    }
    out.push_str("\n[resource]\nanimations = [");
    for (index, animation) in godot_animations(packed).iter().enumerate() {
        out.push_str(if index == 0 { "{\n" } else { ", {\n" });
        out.push_str("\"frames\": [");
        for (position, frame) in animation.frames.iter().enumerate() {
            out.push_str(if position == 0 { "{\n" } else { ", {\n" });
            let _ = writeln!(
                out,
                "\"duration\": 1.0,\n\"texture\": SubResource(\"AtlasTexture_{frame}\")"
            );
            out.push('}');
        }
        let _ = write!(
            out,
            "],\n\"loop\": true,\n\"name\": &\"{}\",\n\"speed\": {}\n}}",
            animation.name,
            real(animation.fps)
        );
    }
    out.push_str("]\n");
    out
}

/// 32 hex digits derived from `text`: Unity wants stable ids, and equal input must give equal
/// output.
fn stable_id(text: &str) -> String {
    let fnv = |seed: u64| {
        text.bytes().fold(seed, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    format!(
        "{:016x}{:016x}",
        fnv(0xcbf2_9ce4_8422_2325),
        fnv(0x8422_2325_cbf2_9ce4)
    )
}

/// A positive 63-bit number derived from `text`, as Unity's sprite `internalID`.
fn stable_number(text: &str) -> i64 {
    let hex = stable_id(text);
    (u64::from_str_radix(&hex[..16], 16).unwrap_or(1) >> 1).max(1) as i64
}

/// Unity: a `TextureImporter` meta per sheet image, sliced into one sprite per frame, and the
/// animations as JSON for an editor script to build clips from. Unity measures sprite rectangles
/// from the bottom left.
fn unity(packed: &Packed) -> Vec<(String, String)> {
    let mut files = Vec::new();
    for (index, page) in packed.pages.iter().enumerate() {
        let frames: Vec<&PackedFrame> = packed.frames.iter().filter(|frame| frame.page == index).collect();
        let mut out = String::new();
        let _ = writeln!(out, "fileFormatVersion: 2\nguid: {}", stable_id(&page.file));
        out.push_str("TextureImporter:\n  internalIDToNameTable:\n");
        for frame in &frames {
            let _ = writeln!(
                out,
                "  - first:\n      213: {}\n    second: {}",
                stable_number(&frame.name),
                frame.name
            );
        }
        out.push_str(
            "  externalObjects: {}\n  serializedVersion: 12\n  mipmaps:\n    enableMipMap: 0\n  \
             textureType: 8\n  textureShape: 1\n  spriteMode: 2\n  spriteExtrude: 1\n  spriteMeshType: 0\n  \
             alignment: 0\n  spritePivot: {x: 0.5, y: 0.5}\n  spritePixelsToUnits: 100\n  \
             alphaIsTransparency: 1\n  spriteSheet:\n    serializedVersion: 2\n    sprites:\n",
        );
        for frame in &frames {
            let _ = writeln!(
                out,
                "    - serializedVersion: 2\n      name: {name}\n      rect:\n        serializedVersion: 2\n        \
                 x: {x}\n        y: {y}\n        width: {w}\n        height: {h}\n      alignment: 0\n      \
                 pivot: {{x: 0.5, y: 0.5}}\n      border: {{x: 0, y: 0, z: 0, w: 0}}\n      outline: []\n      \
                 physicsShape: []\n      tessellationDetail: 0\n      bones: []\n      spriteID: {id}\n      \
                 internalID: {number}\n      vertices: []\n      indices: \n      edges: []\n      weights: []",
                name = frame.name,
                x = frame.x,
                y = page.height - frame.y - frame.height,
                w = frame.width,
                h = frame.height,
                id = stable_id(&frame.name),
                number = stable_number(&frame.name),
            );
        }
        out.push_str(
            "    outline: []\n    physicsShape: []\n    bones: []\n    spriteID: \n    internalID: 0\n    \
             vertices: []\n    indices: \n    edges: []\n    weights: []\n    secondaryTextures: []\n    \
             nameFileIdTable:\n",
        );
        for frame in &frames {
            let _ = writeln!(out, "      {}: {}", frame.name, stable_number(&frame.name));
        }
        out.push_str("  userData: \n  assetBundleName: \n  assetBundleVariant: \n");
        files.push((format!("{}.meta", page.file), out));
    }
    let animations: Vec<Value> = packed
        .animations
        .iter()
        .map(|a| json!({ "name": a.name, "fps": a.fps, "frames": a.frames }))
        .collect();
    let textures: Vec<&str> = packed.pages.iter().map(|page| page.file.as_str()).collect();
    let document = json!({ "textures": textures, "animations": animations });
    files.push((format!("{}.animations.json", packed.name), pretty(&document)));
    files
}

/// A number as Luau source: whole numbers without a fraction.
fn luau_number(value: f64) -> String {
    if value.fract() == 0.0 {
        format!("{value:.0}")
    } else {
        value.to_string()
    }
}

/// A Roblox ModuleScript. `offset` and `size` of a frame are what `ImageRectOffset` and
/// `ImageRectSize` take; `image` is a placeholder until the sheet has been uploaded.
fn roblox(packed: &Packed) -> String {
    let mut out = String::from("return {\n");
    let several = packed.pages.len() > 1;
    if several {
        out.push_str("\timage = {\n");
        for _ in &packed.pages {
            out.push_str("\t\t\"rbxassetid://0\",\n");
        }
        out.push_str("\t},\n\tsize = {\n");
        for page in &packed.pages {
            let _ = writeln!(out, "\t\tVector2.new({}, {}),", page.width, page.height);
        }
        out.push_str("\t},\n");
    } else if let Some(page) = packed.pages.first() {
        let _ = writeln!(
            out,
            "\timage = \"rbxassetid://0\",\n\tsize = Vector2.new({}, {}),",
            page.width, page.height
        );
    }
    out.push_str("\tframes = {\n");
    for frame in &packed.frames {
        let _ = write!(
            out,
            "\t\t[\"{}\"] = {{ offset = Vector2.new({}, {}), size = Vector2.new({}, {})",
            frame.name, frame.x, frame.y, frame.width, frame.height
        );
        if frame.trimmed() {
            let _ = write!(
                out,
                ", sourceOffset = Vector2.new({}, {}), sourceSize = Vector2.new({}, {})",
                frame.offset_x, frame.offset_y, frame.source_width, frame.source_height
            );
        }
        if several {
            let _ = write!(out, ", page = {}", frame.page + 1);
        }
        out.push_str(" },\n");
    }
    out.push_str("\t},\n\tanimations = {\n");
    for animation in &packed.animations {
        let names: Vec<String> = animation
            .frames
            .iter()
            .map(|name| format!("\"{name}\""))
            .collect();
        let _ = writeln!(
            out,
            "\t\t[\"{}\"] = {{ fps = {}, frames = {{ {} }} }},",
            animation.name,
            luau_number(animation.fps),
            names.join(", ")
        );
    }
    out.push_str("\t},\n}\n");
    out
}

#[cfg(test)]
mod tests {
    use super::super::atlas::Page;
    use super::*;

    fn frame(name: &str, page: usize, x: u32, y: u32, trimmed: bool) -> PackedFrame {
        PackedFrame {
            name: name.to_string(),
            page,
            x,
            y,
            width: if trimmed { 10 } else { 16 },
            height: if trimmed { 12 } else { 16 },
            offset_x: if trimmed { 3 } else { 0 },
            offset_y: if trimmed { 2 } else { 0 },
            source_width: 16,
            source_height: 16,
        }
    }

    fn packed() -> Packed {
        Packed {
            name: "hero".to_string(),
            pages: vec![Page {
                file: "hero.png".into(),
                width: 64,
                height: 64,
            }],
            frames: vec![
                frame("walk_0", 0, 0, 0, false),
                frame("walk_1", 0, 18, 0, true),
                frame("hero_0002", 0, 0, 18, false),
            ],
            animations: vec![Animation {
                name: "walk".into(),
                fps: 10.0,
                frames: vec!["walk_0".into(), "walk_1".into()],
            }],
        }
    }

    #[test]
    fn unity_rectangles_are_measured_from_the_bottom_left() {
        let files = export(Target::Unity, &packed(), "");
        let names: Vec<&str> = files.iter().map(|(name, _)| name.as_str()).collect();
        assert_eq!(names, ["hero.png.meta", "hero.animations.json"]);
        let meta = &files[0].1;
        assert!(meta.contains("spriteMode: 2"));
        // walk_0 sits at the top of a 64-pixel sheet: its bottom edge is 48 pixels up.
        let walk_0 = meta.split("name: walk_0\n").nth(1).unwrap();
        assert!(
            walk_0.contains("x: 0\n        y: 48\n        width: 16\n        height: 16"),
            "{walk_0}"
        );
        let lower = meta.split("name: hero_0002\n").nth(1).unwrap();
        assert!(lower.contains("x: 0\n        y: 30\n"), "{lower}");
        // Ids are the same on every run and differ between sprites.
        assert_eq!(*meta, export(Target::Unity, &packed(), "")[0].1);
        assert_ne!(stable_id("walk_0"), stable_id("walk_1"));
        assert_eq!(stable_id("walk_0").len(), 32);
    }

    #[test]
    fn godot_gives_loose_frames_a_default_animation_and_keeps_trimmed_frames_aligned() {
        let text = export(Target::Godot, &packed(), "assets/sprites/hero")
            .remove(0)
            .1;
        assert!(text.starts_with("[gd_resource type=\"SpriteFrames\" load_steps=5 format=3]\n"));
        assert!(text.contains("path=\"res://assets/sprites/hero/hero.png\" id=\"1\""));
        assert!(text.contains("region = Rect2(18, 0, 10, 12)\nmargin = Rect2(3, 2, 6, 4)\n"));
        assert!(text.contains("\"name\": &\"walk\",\n\"speed\": 10.0\n}"));
        assert!(text.contains("\"name\": &\"default\",\n\"speed\": 5.0\n}"));
        assert_eq!(text.matches("SubResource(\"AtlasTexture_hero_0002\")").count(), 1);
        // At the project root the path has no directory.
        assert!(
            export(Target::Godot, &packed(), "")
                .remove(0)
                .1
                .contains("path=\"res://hero.png\"")
        );
    }

    #[test]
    fn several_pages_are_described_per_page_and_linked() {
        let mut two = packed();
        two.pages.push(Page {
            file: "hero-2.png".into(),
            width: 32,
            height: 32,
        });
        two.frames[2].page = 1;
        two.frames[2].y = 0;

        let json = export(Target::Json, &two, "");
        assert_eq!(
            json.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            ["hero.json", "hero-2.json"]
        );
        let first: Value = serde_json::from_str(&json[0].1).unwrap();
        let second: Value = serde_json::from_str(&json[1].1).unwrap();
        assert_eq!(first["meta"]["related_multi_packs"], json!(["hero-2.json"]));
        assert_eq!(first["animations"]["walk"], json!(["walk_0", "walk_1"]));
        assert!(first["frames"].get("hero_0002").is_none() && second["frames"].get("hero_0002").is_some());
        assert!(second.get("animations").is_none());
        assert_eq!(second["meta"]["image"], "hero-2.png");

        let luau = export(Target::Roblox, &two, "").remove(0).1;
        assert!(luau.contains("\timage = {\n\t\t\"rbxassetid://0\",\n\t\t\"rbxassetid://0\",\n\t},"));
        assert!(luau.contains(
            "[\"hero_0002\"] = { offset = Vector2.new(0, 0), size = Vector2.new(16, 16), page = 2 },"
        ));
        assert_eq!(export(Target::Unity, &two, "").len(), 3);
    }
}
