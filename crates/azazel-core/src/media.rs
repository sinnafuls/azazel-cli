//! Turning image bytes into blocks a model can take.

use std::io::Cursor;

use azazel_ai::Block;
use base64::Engine;
use image::{ImageFormat, imageops::FilterType};

/// Long side, in pixels, above which images sent to a model are scaled down.
pub const MAX_IMAGE_SIDE: u32 = 1568;

/// The MIME type when `bytes` are a PNG, JPEG, WebP or GIF image.
pub fn image_mime(bytes: &[u8]) -> Option<&'static str> {
    match image::guess_format(bytes).ok()? {
        ImageFormat::Png => Some("image/png"),
        ImageFormat::Jpeg => Some("image/jpeg"),
        ImageFormat::WebP => Some("image/webp"),
        ImageFormat::Gif => Some("image/gif"),
        _ => None,
    }
}

/// An `Image` block for `bytes`. Images whose long side exceeds `max_side` are scaled down and
/// re-encoded as PNG; smaller ones are passed through untouched. Decoding is CPU-bound: call from
/// `spawn_blocking` for large inputs.
pub fn image_block(bytes: &[u8], max_side: u32) -> Result<Block, String> {
    let mime = image_mime(bytes).ok_or("not a PNG, JPEG, WebP or GIF image")?;
    let encode = |data: &[u8]| base64::engine::general_purpose::STANDARD.encode(data);
    let (width, height) = image::ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?
        .into_dimensions()
        .map_err(|e| e.to_string())?;
    if width.max(height) <= max_side {
        return Ok(Block::Image {
            mime: mime.to_string(),
            data: encode(bytes),
        });
    }
    let image = image::load_from_memory(bytes).map_err(|e| e.to_string())?;
    let scaled = image.resize(max_side, max_side, FilterType::Triangle);
    let mut png = Vec::new();
    scaled
        .write_to(&mut Cursor::new(&mut png), ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(Block::Image {
        mime: "image/png".to_string(),
        data: encode(&png),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let image = image::RgbaImage::from_pixel(width, height, image::Rgba([200, 30, 30, 255]));
        let mut bytes = Vec::new();
        image
            .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
            .unwrap();
        bytes
    }

    fn decode(block: &Block) -> (String, image::DynamicImage) {
        let Block::Image { mime, data } = block else {
            panic!("{block:?}")
        };
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        (mime.clone(), image::load_from_memory(&bytes).unwrap())
    }

    #[test]
    fn small_images_pass_through_and_large_ones_are_scaled_keeping_aspect() {
        let small = png(40, 20);
        let Block::Image { data, .. } = image_block(&small, 100).unwrap() else {
            panic!()
        };
        assert_eq!(
            base64::engine::general_purpose::STANDARD.decode(data).unwrap(),
            small
        );

        let (mime, scaled) = decode(&image_block(&png(400, 100), 100).unwrap());
        assert_eq!(mime, "image/png");
        assert_eq!((scaled.width(), scaled.height()), (100, 25));
    }
}
