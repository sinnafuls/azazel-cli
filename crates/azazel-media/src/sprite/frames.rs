//! From source images to named frames: grid slicing, colour keying, trimming and naming.

use std::collections::HashSet;

use image::{Rgba, RgbaImage, imageops};
use serde::Deserialize;

/// Longest sheet or animation name accepted.
const MAX_NAME_CHARS: usize = 64;
/// Fastest animation accepted, in frames per second.
const MAX_FPS: f64 = 1000.0;

/// One frame as it goes into a sheet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub name: String,
    /// The pixels placed in the sheet.
    pub image: RgbaImage,
    /// Position of `image` inside the frame it was cut from.
    pub offset_x: u32,
    pub offset_y: u32,
    /// Size of the frame `image` was cut from.
    pub source_width: u32,
    pub source_height: u32,
}

/// An animation as a caller describes it: frames `from..=to` of the source order.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct AnimationSpec {
    pub name: String,
    pub from: usize,
    pub to: usize,
    pub fps: f64,
}

/// An animation over frame names.
#[derive(Debug, Clone, PartialEq)]
pub struct Animation {
    pub name: String,
    pub fps: f64,
    pub frames: Vec<String>,
}

/// The names of all frames, in source order, and the animations over them.
#[derive(Debug, Clone, PartialEq)]
pub struct Naming {
    pub frames: Vec<String>,
    pub animations: Vec<Animation>,
}

/// Cuts `sheet` into `cols` × `rows` equal cells, row by row from the top left. Pixels left over
/// when the sheet does not divide evenly are dropped from its right and bottom edges.
pub fn slice(sheet: &RgbaImage, cols: u32, rows: u32) -> Result<Vec<RgbaImage>, String> {
    if cols == 0 || rows == 0 {
        return Err("cols and rows must be at least 1".to_string());
    }
    let (width, height) = sheet.dimensions();
    let (cell_width, cell_height) = (width / cols, height / rows);
    if cell_width == 0 || cell_height == 0 {
        return Err(format!(
            "a {width}x{height} image cannot be cut into {cols}x{rows} cells"
        ));
    }
    let mut cells = Vec::new();
    for row in 0..rows {
        for col in 0..cols {
            let cell = imageops::crop_imm(
                sheet,
                col * cell_width,
                row * cell_height,
                cell_width,
                cell_height,
            );
            cells.push(cell.to_image());
        }
    }
    Ok(cells)
}

/// The most common colour among the four corner pixels of `image`, the earliest corner (top left,
/// top right, bottom left, bottom right) winning a tie. `None` for an image without pixels.
pub fn auto_key(image: &RgbaImage) -> Option<[u8; 3]> {
    let (width, height) = image.dimensions();
    if width == 0 || height == 0 {
        return None;
    }
    let corners = [(0, 0), (width - 1, 0), (0, height - 1), (width - 1, height - 1)].map(|(x, y)| {
        let [r, g, b, _] = image.get_pixel(x, y).0;
        [r, g, b]
    });
    let mut best = corners[0];
    let mut best_count = 0;
    for corner in corners {
        let count = corners.iter().filter(|other| **other == corner).count();
        if count > best_count {
            best = corner;
            best_count = count;
        }
    }
    Some(best)
}

/// Makes every pixel whose colour lies within Euclidean RGB distance `tolerance` of `color`
/// (inclusive) fully transparent.
pub fn key(image: &mut RgbaImage, color: [u8; 3], tolerance: u32) {
    let limit = u64::from(tolerance) * u64::from(tolerance);
    for pixel in image.pixels_mut() {
        let distance: u64 = pixel
            .0
            .iter()
            .zip(color)
            .map(|(channel, wanted)| {
                let difference = u64::from(channel.abs_diff(wanted));
                difference * difference
            })
            .sum();
        if distance <= limit {
            *pixel = Rgba([0, 0, 0, 0]);
        }
    }
}

/// `image` without its fully transparent border, remembering where the rest sat. An image with no
/// visible pixel keeps its top-left pixel, so every frame has a region in the sheet.
pub fn trim(name: impl Into<String>, image: &RgbaImage) -> Frame {
    let (width, height) = image.dimensions();
    let (mut left, mut top, mut right, mut bottom) = (width, height, 0, 0);
    for (x, y, pixel) in image.enumerate_pixels() {
        if pixel.0[3] != 0 {
            left = left.min(x);
            top = top.min(y);
            right = right.max(x);
            bottom = bottom.max(y);
        }
    }
    if left > right || top > bottom {
        (left, top, right, bottom) = (0, 0, 0, 0);
    }
    Frame {
        name: name.into(),
        image: imageops::crop_imm(image, left, top, right - left + 1, bottom - top + 1).to_image(),
        offset_x: left,
        offset_y: top,
        source_width: width,
        source_height: height,
    }
}

/// The whole of `image` as a frame.
pub fn whole(name: impl Into<String>, image: RgbaImage) -> Frame {
    let (width, height) = image.dimensions();
    Frame {
        name: name.into(),
        image,
        offset_x: 0,
        offset_y: 0,
        source_width: width,
        source_height: height,
    }
}

/// Names `count` frames of the sheet `sheet`. A frame inside an animation is `<animation>_<i>`, `i`
/// counting from 0 at the animation's first frame; when animations overlap, the one listed first
/// names the frame and the others refer to it by that name. Every other frame is
/// `<sheet>_<4-digit source index>`.
pub fn frame_names(sheet: &str, count: usize, animations: &[AnimationSpec]) -> Result<Naming, String> {
    check_name("name", sheet)?;
    let mut names: Vec<Option<String>> = vec![None; count];
    let mut named = Vec::with_capacity(animations.len());
    let mut seen = HashSet::new();
    for spec in animations {
        check_name("an animation name", &spec.name)?;
        let name = &spec.name;
        if !seen.insert(name.as_str()) {
            return Err(format!("animation \"{name}\" is listed twice"));
        }
        let playable = spec.fps > 0.0 && spec.fps <= MAX_FPS;
        if !playable {
            return Err(format!(
                "animation \"{name}\" needs an fps above 0 and at most {MAX_FPS}"
            ));
        }
        if spec.from > spec.to {
            return Err(format!(
                "animation \"{name}\" runs from frame {} to frame {}; `from` must not be after `to`",
                spec.from, spec.to
            ));
        }
        if spec.to >= count {
            return Err(format!(
                "animation \"{name}\" ends at frame {} but there are only {count} frames (0-based)",
                spec.to
            ));
        }
        let mut frames = Vec::with_capacity(spec.to - spec.from + 1);
        for (step, slot) in names[spec.from..=spec.to].iter_mut().enumerate() {
            frames.push(slot.get_or_insert_with(|| format!("{name}_{step}")).clone());
        }
        named.push(Animation {
            name: name.clone(),
            fps: spec.fps,
            frames,
        });
    }
    let frames: Vec<String> = names
        .into_iter()
        .enumerate()
        .map(|(index, name)| name.unwrap_or_else(|| format!("{sheet}_{index:04}")))
        .collect();
    let mut unique = HashSet::new();
    if let Some(repeated) = frames.iter().find(|name| !unique.insert(name.as_str())) {
        return Err(format!(
            "two frames would be called \"{repeated}\"; rename the sheet or an animation"
        ));
    }
    Ok(Naming {
        frames,
        animations: named,
    })
}

/// Accepts names that are safe as file names and as identifiers in every descriptor format.
pub(super) fn check_name(what: &str, name: &str) -> Result<(), String> {
    let allowed = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if name.is_empty() || name.len() > MAX_NAME_CHARS || !name.chars().all(allowed) {
        return Err(format!(
            "{what} must be 1 to {MAX_NAME_CHARS} letters, digits, \"_\" or \"-\", got \"{name}\""
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba(color))
    }

    #[test]
    fn slice_walks_rows_then_columns_and_drops_the_remainder() {
        // 7x5 cut 3x2: cells are 2x2, the last column and row of pixels are left out.
        let mut sheet = solid(7, 5, [0, 0, 0, 255]);
        for (x, y, pixel) in sheet.enumerate_pixels_mut() {
            *pixel = Rgba([x as u8, y as u8, 0, 255]);
        }
        let cells = slice(&sheet, 3, 2).unwrap();
        assert_eq!(cells.len(), 6);
        assert!(cells.iter().all(|cell| cell.dimensions() == (2, 2)));
        let origins: Vec<[u8; 2]> = cells
            .iter()
            .map(|cell| {
                let [x, y, ..] = cell.get_pixel(0, 0).0;
                [x, y]
            })
            .collect();
        assert_eq!(origins, [[0, 0], [2, 0], [4, 0], [0, 2], [2, 2], [4, 2]]);
    }

    #[test]
    fn slice_refuses_grids_finer_than_the_image() {
        let sheet = solid(4, 4, [0, 0, 0, 255]);
        assert!(slice(&sheet, 0, 1).unwrap_err().contains("at least 1"));
        assert_eq!(
            slice(&sheet, 5, 1).unwrap_err(),
            "a 4x4 image cannot be cut into 5x1 cells"
        );
    }

    #[test]
    fn trim_of_an_invisible_frame_keeps_one_pixel() {
        let frame = trim("ghost", &solid(8, 6, [255, 0, 0, 0]));
        assert_eq!(frame.image.dimensions(), (1, 1));
        assert_eq!((frame.offset_x, frame.offset_y), (0, 0));
        assert_eq!((frame.source_width, frame.source_height), (8, 6));
    }

    #[test]
    fn trim_keeps_translucent_pixels() {
        let mut image = solid(5, 5, [0, 0, 0, 0]);
        image.put_pixel(1, 2, Rgba([9, 9, 9, 1]));
        image.put_pixel(3, 4, Rgba([9, 9, 9, 255]));
        let frame = trim("f", &image);
        assert_eq!((frame.offset_x, frame.offset_y), (1, 2));
        assert_eq!(frame.image.dimensions(), (3, 3));
        assert_eq!(frame.image.get_pixel(0, 0).0, [9, 9, 9, 1]);
    }

    #[test]
    fn names_are_checked_before_they_reach_a_file_name() {
        for bad in ["", "a b", "../x", "a.b", "é"] {
            assert!(frame_names(bad, 1, &[]).is_err(), "{bad:?}");
        }
        assert!(frame_names(&"n".repeat(65), 1, &[]).is_err());
        assert!(frame_names(&"n".repeat(64), 1, &[]).is_ok());
    }
}
