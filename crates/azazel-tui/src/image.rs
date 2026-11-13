//! Inline images: a picture as the escape sequence that makes the terminal draw it, sized in
//! cells so that the renderer knows how many rows it takes.
//!
//! Three protocols: kitty graphics (kitty, Ghostty), iTerm2 inline files (iTerm2, WezTerm) and
//! Sixel (Windows Terminal, foot, xterm). The first two are told how many rows to use; a Sixel
//! picture is as many pixels as it is, so its rows follow from the size of a cell.

use base64::Engine as _;
use image::RgbaImage;

use crate::render::HistoryItem;
use crate::term::{Caps, Images, passthrough};

/// Base64 characters per kitty graphics chunk, the most the protocol allows.
const KITTY_CHUNK: usize = 4096;
/// A Sixel band is six pixels tall.
const SIXEL_BAND: u32 = 6;

/// The size a picture is shown at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Fit {
    /// Pixels.
    pub width: u32,
    pub height: u32,
    /// Cells.
    pub cols: u16,
    pub rows: u16,
}

/// Fits a picture of `width` by `height` pixels into `max_cols` by `max_rows` cells of `cell`
/// pixels each. It is made smaller to fit, never larger. `None` for an empty picture or no room.
pub fn fit(width: u32, height: u32, cell: (u16, u16), max_cols: u16, max_rows: u16) -> Option<Fit> {
    let (cell_w, cell_h) = (u32::from(cell.0.max(1)), u32::from(cell.1.max(1)));
    let (room_w, room_h) = (u32::from(max_cols) * cell_w, u32::from(max_rows) * cell_h);
    if width == 0 || height == 0 || room_w == 0 || room_h == 0 {
        return None;
    }
    let (mut w, mut h) = (u64::from(width), u64::from(height));
    if w > u64::from(room_w) {
        h = (h * u64::from(room_w) / w).max(1);
        w = u64::from(room_w);
    }
    if h > u64::from(room_h) {
        w = (w * u64::from(room_h) / h).max(1);
        h = u64::from(room_h);
    }
    let (w, h) = (w as u32, h as u32);
    Some(Fit {
        width: w,
        height: h,
        cols: w.div_ceil(cell_w) as u16,
        rows: h.div_ceil(cell_h) as u16,
    })
}

/// An encoded picture (PNG, JPEG, WebP, GIF) as a transcript block of at most `max_cols` by
/// `max_rows` cells. `None` when the terminal draws no images or the picture cannot be read.
pub fn encode(bytes: &[u8], caps: &Caps, max_cols: u16, max_rows: u16) -> Option<HistoryItem> {
    if caps.images == Images::None {
        return None;
    }
    let picture = image::load_from_memory(bytes).ok()?.into_rgba8();
    encode_rgba(&picture, caps, max_cols, max_rows)
}

/// [`encode`] for pixels that are already decoded.
pub fn encode_rgba(picture: &RgbaImage, caps: &Caps, max_cols: u16, max_rows: u16) -> Option<HistoryItem> {
    if caps.images == Images::None {
        return None;
    }
    let mut size = fit(
        picture.width(),
        picture.height(),
        caps.cell_px,
        max_cols,
        max_rows,
    )?;
    if caps.images == Images::Sixel && size.height > SIXEL_BAND {
        // Whole bands only, so that the last one does not reach into the row below.
        size.height -= size.height % SIXEL_BAND;
    }
    let resized;
    let shown = if (size.width, size.height) == picture.dimensions() {
        picture
    } else {
        resized = image::imageops::resize(
            picture,
            size.width,
            size.height,
            image::imageops::FilterType::Triangle,
        );
        &resized
    };
    let paint = match caps.images {
        Images::None => return None,
        Images::Kitty => kitty(&png(shown)?, size.rows, caps.tmux),
        Images::Iterm => iterm(&png(shown)?, size.rows, caps.tmux),
        Images::Sixel => sixel(shown)?,
    };
    Some(HistoryItem::Block {
        rows: size.rows,
        paint,
    })
}

fn png(picture: &RgbaImage) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    picture
        .write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
        .ok()?;
    Some(out)
}

/// The kitty graphics protocol: transmit and show a PNG (`a=T,f=100`) over `rows` rows, quietly
/// (`q=2`) and without moving the cursor (`C=1`), in chunks of base64.
fn kitty(png: &[u8], rows: u16, tmux: bool) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let chunks: Vec<&str> = data
        .as_bytes()
        .chunks(KITTY_CHUNK)
        .map(|chunk| std::str::from_utf8(chunk).unwrap_or_default())
        .collect();
    let mut out = String::with_capacity(data.len() + chunks.len() * 16 + 32);
    for (index, chunk) in chunks.iter().enumerate() {
        let more = u8::from(index + 1 < chunks.len());
        let control = match index {
            0 => format!("a=T,f=100,q=2,C=1,r={rows},m={more}"),
            _ => format!("m={more}"),
        };
        let sequence = format!("\x1b_G{control};{chunk}\x1b\\");
        out.push_str(&if tmux { passthrough(&sequence) } else { sequence });
    }
    out
}

/// iTerm2's inline file: a PNG shown `rows` cells tall, as wide as its shape makes it.
fn iterm(png: &[u8], rows: u16, tmux: bool) -> String {
    let data = base64::engine::general_purpose::STANDARD.encode(png);
    let sequence = format!(
        "\x1b]1337;File=inline=1;size={};height={rows};preserveAspectRatio=1:{data}\x07",
        png.len()
    );
    if tmux { passthrough(&sequence) } else { sequence }
}

fn sixel(picture: &RgbaImage) -> Option<String> {
    icy_sixel::SixelImage::try_from_rgba(
        picture.as_raw().clone(),
        picture.width() as usize,
        picture.height() as usize,
    )
    .ok()?
    .encode()
    .ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn caps(images: Images, tmux: bool) -> Caps {
        let mut caps = Caps::detect(&|_| None);
        caps.images = images;
        caps.tmux = tmux;
        caps.cell_px = (10, 20);
        caps
    }

    /// Noise, so that the PNG of a larger picture does not shrink into one chunk.
    fn picture(width: u32, height: u32) -> RgbaImage {
        let mut seed = 0x2545_f491_u32;
        RgbaImage::from_fn(width, height, |_, _| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let [r, g, b, _] = seed.to_be_bytes();
            image::Rgba([r, g, b, 255])
        })
    }

    fn block(item: Option<HistoryItem>) -> (u16, String) {
        match item.unwrap() {
            HistoryItem::Block { rows, paint } => (rows, paint),
            HistoryItem::Line(line) => panic!("a line: {line}"),
        }
    }

    #[test]
    fn a_picture_is_made_smaller_to_fit_and_never_larger() {
        let cell = (10, 20);
        // Small enough: shown as it is, on the cells it touches.
        assert_eq!(
            fit(95, 41, cell, 60, 20),
            Some(Fit {
                width: 95,
                height: 41,
                cols: 10,
                rows: 3
            })
        );
        // Too wide: 1200 by 300 becomes 600 by 150.
        assert_eq!(
            fit(1200, 300, cell, 60, 20),
            Some(Fit {
                width: 600,
                height: 150,
                cols: 60,
                rows: 8
            })
        );
        // Too tall: 400 by 1600 becomes 100 by 400.
        assert_eq!(
            fit(400, 1600, cell, 60, 20),
            Some(Fit {
                width: 100,
                height: 400,
                cols: 10,
                rows: 20
            })
        );
        // Too wide and, even then, too tall.
        let both = fit(4000, 4000, cell, 60, 20).unwrap();
        assert_eq!(
            (both.width, both.height, both.cols, both.rows),
            (400, 400, 40, 20)
        );
        assert_eq!(fit(0, 10, cell, 60, 20), None);
        assert_eq!(fit(10, 10, cell, 0, 20), None);
        // A cell size of zero cannot divide.
        assert!(fit(10, 10, (0, 0), 60, 20).is_some());
    }

    #[test]
    fn kitty_sends_a_png_in_chunks_without_moving_the_cursor() {
        let (rows, paint) = block(encode_rgba(&picture(12, 50), &caps(Images::Kitty, false), 60, 20));
        assert_eq!(rows, 3);
        let encoded = base64::engine::general_purpose::STANDARD.encode(png(&picture(12, 50)).unwrap());
        assert!(encoded.len() <= KITTY_CHUNK);
        assert_eq!(paint, format!("\x1b_Ga=T,f=100,q=2,C=1,r=3,m=0;{encoded}\x1b\\"));

        // A picture too big for one chunk: every chunk but the last says more follows.
        let (rows, paint) = block(encode_rgba(
            &picture(300, 200),
            &caps(Images::Kitty, false),
            60,
            20,
        ));
        assert_eq!(rows, 10);
        let chunks: Vec<&str> = paint.split_terminator("\x1b\\").collect();
        assert!(chunks.len() > 1);
        assert!(chunks[0].starts_with("\x1b_Ga=T,f=100,q=2,C=1,r=10,m=1;"));
        for chunk in &chunks[1..chunks.len() - 1] {
            assert!(chunk.starts_with("\x1b_Gm=1;"), "{}", &chunk[..12]);
            assert_eq!(chunk.len(), "\x1b_Gm=1;".len() + KITTY_CHUNK);
        }
        assert!(chunks[chunks.len() - 1].starts_with("\x1b_Gm=0;"));
        let data: String = chunks
            .iter()
            .map(|chunk| chunk.split_once(';').unwrap().1)
            .collect();
        let decoded = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        assert_eq!(
            image::load_from_memory(&decoded).unwrap().into_rgba8(),
            picture(300, 200)
        );

        // Inside tmux every chunk is passed through on its own.
        let (_, wrapped) = block(encode_rgba(&picture(12, 50), &caps(Images::Kitty, true), 60, 20));
        assert_eq!(
            wrapped,
            format!("\x1bPtmux;\x1b\x1b_Ga=T,f=100,q=2,C=1,r=3,m=0;{encoded}\x1b\x1b\\\x1b\\")
        );
    }

    #[test]
    fn iterm_sends_an_inline_file_with_its_height_in_rows() {
        let (rows, paint) = block(encode_rgba(&picture(30, 50), &caps(Images::Iterm, false), 60, 20));
        let file = png(&picture(30, 50)).unwrap();
        let encoded = base64::engine::general_purpose::STANDARD.encode(&file);
        assert_eq!(rows, 3);
        assert_eq!(
            paint,
            format!(
                "\x1b]1337;File=inline=1;size={};height=3;preserveAspectRatio=1:{encoded}\x07",
                file.len()
            )
        );
    }

    #[test]
    fn sixel_is_whole_bands_inside_its_rows() {
        // 50 pixels are two and a half cells: three rows, and 48 pixels (eight bands) of picture.
        let (rows, paint) = block(encode_rgba(&picture(30, 50), &caps(Images::Sixel, false), 60, 20));
        assert_eq!(rows, 3);
        assert!(paint.starts_with("\x1bP"), "{:?}", &paint[..8]);
        assert!(paint.ends_with("\x1b\\"));
        let decoded = icy_sixel::SixelImage::decode(paint.as_bytes()).unwrap();
        assert_eq!((decoded.width, decoded.height), (30, 48));
    }

    #[test]
    fn a_large_picture_is_scaled_into_the_box_and_bad_bytes_give_nothing() {
        let mut file = Vec::new();
        picture(1600, 900)
            .write_to(&mut std::io::Cursor::new(&mut file), image::ImageFormat::Png)
            .unwrap();
        let (rows, _) = block(encode(&file, &caps(Images::Kitty, false), 60, 20));
        // 600 by 337 pixels: 17 rows of 20.
        assert_eq!(rows, 17);
        assert_eq!(encode(&file, &caps(Images::None, false), 60, 20), None);
        assert_eq!(
            encode(b"not a picture", &caps(Images::Kitty, false), 60, 20),
            None
        );
    }
}
