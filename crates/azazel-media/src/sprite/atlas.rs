//! The packed layout that exporters describe, and the sheet images that go with it.

use image::{RgbaImage, imageops};

use super::frames::{Animation, Frame};
use super::pack::{Item, pack};

/// One sheet image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Page {
    /// File name of the image: `<name>.png` for the first page, `<name>-<n>.png` from the second.
    pub file: String,
    pub width: u32,
    pub height: u32,
}

impl Page {
    /// The file name without `.png`; descriptor files that belong to one page are named after it.
    pub fn stem(&self) -> &str {
        self.file.strip_suffix(".png").unwrap_or(&self.file)
    }
}

/// Where one frame ended up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedFrame {
    pub name: String,
    /// Index into [`Packed::pages`].
    pub page: usize,
    /// The frame's region in its page, measured from the top left.
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
    /// Where that region sat inside the frame before trimming.
    pub offset_x: u32,
    pub offset_y: u32,
    /// Size of the frame before trimming.
    pub source_width: u32,
    pub source_height: u32,
}

impl PackedFrame {
    /// Whether transparent borders were cut off this frame.
    pub fn trimmed(&self) -> bool {
        self.width != self.source_width || self.height != self.source_height
    }
}

/// A finished layout: pages, frames in source order, and the animations over them.
#[derive(Debug, Clone, PartialEq)]
pub struct Packed {
    pub name: String,
    pub pages: Vec<Page>,
    pub frames: Vec<PackedFrame>,
    pub animations: Vec<Animation>,
}

/// Packs `frames` into square pages of at most `max_size` pixels, `padding` pixels apart (see
/// [`pack`] for the placement rules).
pub fn layout(
    name: &str,
    frames: &[Frame],
    animations: Vec<Animation>,
    padding: u32,
    max_size: u32,
) -> Result<Packed, String> {
    let items: Vec<Item> = frames
        .iter()
        .map(|frame| Item {
            name: frame.name.clone(),
            width: frame.image.width(),
            height: frame.image.height(),
        })
        .collect();
    let bins = pack(&items, padding, max_size)?;

    let mut spots = vec![(0, 0, 0); frames.len()];
    for (page, bin) in bins.iter().enumerate() {
        for placement in &bin.placements {
            spots[placement.item] = (page, placement.x, placement.y);
        }
    }
    let pages = bins
        .iter()
        .enumerate()
        .map(|(index, bin)| Page {
            file: match index {
                0 => format!("{name}.png"),
                _ => format!("{name}-{}.png", index + 1),
            },
            width: bin.size,
            height: bin.size,
        })
        .collect();
    let frames = frames
        .iter()
        .zip(spots)
        .map(|(frame, (page, x, y))| PackedFrame {
            name: frame.name.clone(),
            page,
            x,
            y,
            width: frame.image.width(),
            height: frame.image.height(),
            offset_x: frame.offset_x,
            offset_y: frame.offset_y,
            source_width: frame.source_width,
            source_height: frame.source_height,
        })
        .collect();
    Ok(Packed {
        name: name.to_string(),
        pages,
        frames,
        animations,
    })
}

/// The image of every page, in page order. `frames` must be the frames `packed` was laid out from.
pub fn render(packed: &Packed, frames: &[Frame]) -> impl Iterator<Item = RgbaImage> {
    packed.pages.iter().enumerate().map(move |(index, page)| {
        let mut image = RgbaImage::new(page.width, page.height);
        for (placed, frame) in packed.frames.iter().zip(frames) {
            if placed.page == index {
                imageops::replace(&mut image, &frame.image, i64::from(placed.x), i64::from(placed.y));
            }
        }
        image
    })
}
