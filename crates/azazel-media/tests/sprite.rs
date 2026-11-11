#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_media::sprite::export::Target;
use azazel_media::sprite::frames::{AnimationSpec, auto_key, frame_names, key, slice, trim};
use azazel_media::sprite::pack::{Item, pack};
use azazel_media::sprite::{ImageSource, KeyColor, NO_IMAGE_MODEL, Options, SpriteSheetTool, build};
use image::{Rgba, RgbaImage};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/sprites")
}

fn fixture(name: &str) -> RgbaImage {
    image::open(fixtures().join(name)).unwrap().to_rgba8()
}

/// A fixed-seed generator: the same rectangles on every run and machine.
fn rectangles(count: usize) -> Vec<Item> {
    let mut state = 0x2545_f491_4f6c_dd1d_u64;
    let mut next = move |modulo: u64| {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        (state % modulo) as u32
    };
    (0..count)
        .map(|index| Item {
            name: format!("r{index:02}"),
            width: 3 + next(60),
            height: 3 + next(60),
        })
        .collect()
}

#[test]
fn forty_rectangles_are_packed_apart_inside_their_pages_the_same_way_every_time() {
    let items = rectangles(40);
    let padding = 2;
    let bins = pack(&items, padding, 4096).unwrap();
    assert_eq!(bins, pack(&items, padding, 4096).unwrap());

    let placed: usize = bins.iter().map(|bin| bin.placements.len()).sum();
    assert_eq!(placed, 40);
    for bin in &bins {
        assert!(bin.size.is_power_of_two() && bin.size >= 64);
        let boxes: Vec<(u32, u32, u32, u32)> = bin
            .placements
            .iter()
            .map(|p| (p.x, p.y, items[p.item].width, items[p.item].height))
            .collect();
        for (index, &(x, y, w, h)) in boxes.iter().enumerate() {
            assert!(
                x + w <= bin.size && y + h <= bin.size,
                "{} leaves the page",
                items[index].name
            );
            for &(ox, oy, ow, oh) in &boxes[index + 1..] {
                // Grown by the padding, no two boxes may touch.
                let apart = x + w + padding <= ox
                    || ox + ow + padding <= x
                    || y + h + padding <= oy
                    || oy + oh + padding <= y;
                assert!(
                    apart,
                    "({x},{y},{w},{h}) and ({ox},{oy},{ow},{oh}) are closer than {padding}"
                );
            }
        }
    }
}

#[test]
fn pages_grow_by_doubling_and_overflow_starts_another_page() {
    let square = |n: usize, side: u32| -> Vec<Item> {
        (0..n)
            .map(|i| Item {
                name: format!("s{i}"),
                width: side,
                height: side,
            })
            .collect()
    };
    // Four 40-pixel squares do not fit 64 but fit 128.
    let grown = pack(&square(4, 40), 0, 4096).unwrap();
    assert_eq!(grown.iter().map(|bin| bin.size).collect::<Vec<_>>(), [128]);
    // With pages capped at 64, each page holds one.
    let capped = pack(&square(3, 40), 0, 64).unwrap();
    assert_eq!(
        capped
            .iter()
            .map(|bin| (bin.size, bin.placements.len()))
            .collect::<Vec<_>>(),
        [(64, 1); 3]
    );
    let too_large = pack(&square(1, 65), 0, 64).unwrap_err();
    assert_eq!(too_large, "frame s0 is 65x65, larger than max_size 64");
}

#[test]
fn the_magenta_sheet_becomes_eight_trimmed_frames_on_transparency() {
    let mut sheet = fixture("sheet_4x2.png");
    let color = auto_key(&sheet).unwrap();
    assert_eq!(color, [255, 0, 255]);
    key(&mut sheet, color, 24);
    let cells = slice(&sheet, 4, 2).unwrap();
    assert_eq!(cells.len(), 8);

    for (index, cell) in cells.iter().enumerate() {
        assert_eq!(cell.dimensions(), (16, 16));
        let frame = trim(format!("f{index}"), cell);
        // Cell n holds a (4+n)×(12-n) rectangle two pixels in from the top left.
        let n = index as u32;
        assert_eq!(frame.image.dimensions(), (4 + n, 12 - n), "cell {index}");
        assert_eq!((frame.offset_x, frame.offset_y), (2, 2));
        assert_eq!((frame.source_width, frame.source_height), (16, 16));
        assert!(frame.image.pixels().all(|pixel| pixel.0[3] == 255));
        assert!(
            cell.pixels()
                .all(|pixel| pixel.0[3] == 0 || pixel.0[..3] != [255, 0, 255])
        );
    }
}

#[test]
fn the_key_tolerance_is_a_distance_that_includes_its_boundary() {
    let mut image = RgbaImage::new(3, 1);
    image.put_pixel(0, 0, Rgba([255, 0, 255, 255])); // the key itself
    image.put_pixel(1, 0, Rgba([245, 0, 255, 255])); // distance 10
    image.put_pixel(2, 0, Rgba([244, 0, 255, 255])); // distance 11
    key(&mut image, [255, 0, 255], 10);
    let alpha: Vec<u8> = image.pixels().map(|pixel| pixel.0[3]).collect();
    assert_eq!(alpha, [0, 0, 255]);

    // The corner colour wins by count; a tie goes to the earliest corner.
    let mut corners = RgbaImage::from_pixel(4, 4, Rgba([9, 9, 9, 255]));
    corners.put_pixel(0, 0, Rgba([1, 2, 3, 255]));
    assert_eq!(auto_key(&corners), Some([9, 9, 9]));
    corners.put_pixel(3, 0, Rgba([1, 2, 3, 255]));
    assert_eq!(auto_key(&corners), Some([1, 2, 3]));
}

fn animation(name: &str, from: usize, to: usize, fps: f64) -> AnimationSpec {
    AnimationSpec {
        name: name.into(),
        from,
        to,
        fps,
    }
}

#[test]
fn frames_are_named_by_animation_or_by_sheet_index() {
    let naming = frame_names("hero", 6, &[animation("walk", 0, 3, 10.0)]).unwrap();
    assert_eq!(
        naming.frames,
        ["walk_0", "walk_1", "walk_2", "walk_3", "hero_0004", "hero_0005"]
    );
    assert_eq!(
        naming.animations[0].frames,
        ["walk_0", "walk_1", "walk_2", "walk_3"]
    );

    // Overlapping animations share the frame under the first one's name.
    let shared = frame_names("hero", 3, &[animation("a", 0, 1, 5.0), animation("b", 1, 2, 5.0)]).unwrap();
    assert_eq!(shared.frames, ["a_0", "a_1", "b_1"]);
    assert_eq!(shared.animations[1].frames, ["a_1", "b_1"]);

    assert!(
        frame_names("hero", 2, &[animation("walk", 0, 2, 10.0)])
            .unwrap_err()
            .contains("only 2 frames")
    );
    assert!(
        frame_names("hero", 2, &[animation("walk", 1, 0, 10.0)])
            .unwrap_err()
            .contains("must not be after")
    );
    assert!(
        frame_names("hero", 2, &[animation("walk", 0, 1, 0.0)])
            .unwrap_err()
            .contains("fps")
    );
}

fn hero(target: Target) -> Vec<(String, String)> {
    let names = [
        "walk_0.png",
        "walk_1.png",
        "walk_2.png",
        "walk_3.png",
        "idle_0.png",
        "idle_1.png",
    ];
    let cells = names.iter().map(|name| fixture(name)).collect();
    let options = Options {
        name: "hero".into(),
        animations: vec![animation("walk", 0, 3, 10.0), animation("idle", 4, 5, 2.5)],
        key_color: KeyColor::None,
        tolerance: 24,
        trim: true,
        padding: 2,
        max_size: 4096,
        target,
    };
    build(cells, &options, "assets/sprites/hero").unwrap().descriptors
}

#[test]
fn descriptors_for_every_target() {
    for (target, name) in [
        (Target::Json, "json"),
        (Target::Godot, "godot"),
        (Target::Unity, "unity"),
        (Target::Roblox, "roblox"),
        (Target::Unreal, "unreal"),
    ] {
        let mut text = String::new();
        for (file, content) in hero(target) {
            text.push_str(&format!("=== {file} ===\n{content}"));
        }
        insta::assert_snapshot!(name, text);
    }
}

fn ctx(dir: &Path) -> ToolCtx {
    let agent = AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s".into(),
    };
    ToolCtx {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        agent,
        call_id: "c1".into(),
        cwd: dir.to_path_buf(),
        cancel: CancellationToken::new(),
        update: UpdateSink::default(),
        services: Arc::new(Services::default()),
    }
}

/// A project holding the fixture images under `art/`.
fn project() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("art")).unwrap();
    for entry in std::fs::read_dir(fixtures()).unwrap().flatten() {
        std::fs::copy(entry.path(), dir.path().join("art").join(entry.file_name())).unwrap();
    }
    dir
}

struct Painter;

#[async_trait]
impl ImageSource for Painter {
    async fn image(&self, prompt: &str, _: CancellationToken) -> Result<Vec<u8>, String> {
        assert!(
            prompt.starts_with("a bouncing ball") && prompt.contains("4 columns and 2 rows"),
            "{prompt}"
        );
        Ok(std::fs::read(fixtures().join("sheet_4x2.png")).unwrap())
    }
}

async fn call(tool: &SpriteSheetTool, dir: &Path, input: Value) -> ToolOutput {
    tool.call(input, &ctx(dir)).await
}

#[tokio::test]
async fn the_tool_writes_a_sheet_and_its_descriptor_into_the_project() {
    let dir = project();
    let tool = SpriteSheetTool::new(dir.path().to_path_buf(), "assets/generated".into(), None);
    let frames: Vec<String> = (0..4).map(|i| format!("art/walk_{i}.png")).collect();

    let output = call(
        &tool,
        dir.path(),
        json!({
            "source": { "frames": frames },
            "name": "hero",
            "animations": [{ "name": "walk", "from": 0, "to": 3, "fps": 10 }],
            "target": "godot"
        }),
    )
    .await;
    assert!(!output.is_error, "{}", output.text_content());
    let report = output.text_content();
    assert!(
        report.starts_with("packed 4 frames into one sheet (64×64) for godot\n"),
        "{report}"
    );
    let out = dir.path().join("assets/generated/sprites/hero");
    let sheet = image::open(out.join("hero.png")).unwrap();
    assert_eq!((sheet.width(), sheet.height()), (64, 64));
    let tres = std::fs::read_to_string(out.join("hero.tres")).unwrap();
    assert!(tres.contains("path=\"res://assets/generated/sprites/hero/hero.png\""));
    assert_eq!(tres.matches("[sub_resource type=\"AtlasTexture\"").count(), 4);
    assert_eq!(
        tool.subjects(&json!({ "name": "hero" })),
        ["assets/generated/sprites/hero"]
    );

    // Unity slices whole cells, Roblox caps the sheet, and both say so through their output.
    let unity = call(
        &tool,
        dir.path(),
        json!({ "source": { "sheet": "art/sheet_4x2.png", "cols": 4, "rows": 2 }, "name": "cells",
                "key_color": "#FF00FF", "target": "unity", "out_dir": "Assets/Sprites" }),
    )
    .await;
    assert!(
        unity.text_content().contains("Frames were not trimmed"),
        "{}",
        unity.text_content()
    );
    let meta = std::fs::read_to_string(dir.path().join("Assets/Sprites/cells.png.meta")).unwrap();
    assert_eq!(meta.matches("width: 16\n        height: 16").count(), 8);

    let big = image::RgbaImage::from_pixel(700, 700, Rgba([1, 2, 3, 255]));
    big.save(dir.path().join("art/big_a.png")).unwrap();
    big.save(dir.path().join("art/big_b.png")).unwrap();
    let roblox = call(
        &tool,
        dir.path(),
        json!({ "source": { "frames": ["art/big_a.png", "art/big_b.png"] }, "name": "big", "target": "roblox" }),
    )
    .await;
    let report = roblox.text_content();
    assert!(
        report.starts_with("packed 2 frames into 2 sheets (1024×1024, 1024×1024) for roblox\n"),
        "{report}"
    );
    assert!(report.contains("Upload the image to Roblox"));
    assert!(
        dir.path()
            .join("assets/generated/sprites/big/big-2.png")
            .is_file()
    );
}

#[tokio::test]
async fn bad_requests_are_refused_with_what_to_change() {
    let dir = project();
    let tool = SpriteSheetTool::new(dir.path().to_path_buf(), "assets/generated".into(), None);
    let frames = json!({ "frames": ["art/walk_0.png"] });
    let error = |input: Value| async {
        let output = call(&tool, dir.path(), input).await;
        assert!(output.is_error);
        output.text_content()
    };

    let outside =
        error(json!({ "source": frames, "name": "x", "target": "json", "out_dir": "../elsewhere" })).await;
    assert!(outside.contains("outside the project"), "{outside}");
    assert!(!dir.path().parent().unwrap().join("elsewhere").exists());

    let target = error(json!({ "source": frames, "name": "x", "target": "phaser" })).await;
    assert_eq!(
        target,
        "unknown target `phaser`; targets: json, godot, unity, roblox, unreal"
    );
    let both = error(
        json!({ "source": { "frames": [], "sheet": "art/sheet_4x2.png" }, "name": "x", "target": "json" }),
    )
    .await;
    assert!(both.starts_with("`source` must be exactly one of"), "{both}");
    let grid =
        error(json!({ "source": { "sheet": "art/sheet_4x2.png" }, "name": "x", "target": "json" })).await;
    assert_eq!(grid, "this source needs `cols` and `rows`");
    let missing =
        error(json!({ "source": { "frames": ["art/nope.png"] }, "name": "x", "target": "json" })).await;
    assert!(missing.starts_with("art/nope.png: "), "{missing}");
    let unpainted = error(json!({ "source": { "prompt": "a bouncing ball", "cols": 4, "rows": 2 }, "name": "x", "target": "json" })).await;
    assert_eq!(unpainted, NO_IMAGE_MODEL);
}

#[tokio::test]
async fn a_prompt_source_is_generated_keyed_and_cut() {
    let dir = project();
    let tool = SpriteSheetTool::new(
        dir.path().to_path_buf(),
        "assets/generated".into(),
        Some(Arc::new(Painter)),
    );
    let output = call(
        &tool,
        dir.path(),
        json!({ "source": { "prompt": "a bouncing ball", "cols": 4, "rows": 2 }, "name": "ball", "target": "json" }),
    )
    .await;
    assert!(!output.is_error, "{}", output.text_content());
    let out = dir.path().join("assets/generated/sprites/ball");
    let document: Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("ball.json")).unwrap()).unwrap();
    let frames = document["frames"].as_object().unwrap();
    assert_eq!(frames.len(), 8);
    // The magenta background was keyed out by default, so the frames were trimmed to their shapes.
    assert_eq!(frames["ball_0000"]["frame"]["w"], 4);
    assert_eq!(
        frames["ball_0000"]["spriteSourceSize"],
        json!({ "x": 2, "y": 2, "w": 4, "h": 12 })
    );
    assert_eq!(frames["ball_0000"]["sourceSize"], json!({ "w": 16, "h": 16 }));
    let sheet = image::open(out.join("ball.png")).unwrap().to_rgba8();
    assert!(
        sheet
            .pixels()
            .all(|pixel| pixel.0[3] == 0 || pixel.0[..3] != [255, 0, 255])
    );
}
