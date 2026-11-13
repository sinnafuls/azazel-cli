//! Mermaid diagrams: source to SVG, SVG to pixels, pixels to an inline image.
//!
//! The text of a diagram is drawn with one font that ships in the binary (Liberation Sans, which
//! has the metrics of Arial, the font diagrams are laid out for). The fonts of the system are
//! not looked through for drawing: that takes hundreds of milliseconds.

use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::{Arc, LazyLock};

use azazel_tui::components::markdown::Fenced;
use azazel_tui::term::Caps;
use resvg::{tiny_skia, usvg};

/// SIL Open Font License 1.1; the licence is beside the file.
const FONT: &[u8] = include_bytes!("../assets/LiberationSans-Regular.ttf");
const FONT_FAMILY: &str = "Liberation Sans";
/// A diagram is never drawn larger than this many times its natural size.
const MAX_SCALE: f32 = 1.5;
/// The longest side of the picture a diagram is drawn into, in pixels.
const MAX_SIDE: f32 = 4000.0;

static FONTS: LazyLock<Arc<usvg::fontdb::Database>> = LazyLock::new(|| {
    let mut fonts = usvg::fontdb::Database::new();
    fonts.load_font_data(FONT.to_vec());
    // Whatever family a diagram asks for, this is the one there is.
    fonts.set_sans_serif_family(FONT_FAMILY);
    fonts.set_serif_family(FONT_FAMILY);
    fonts.set_monospace_family(FONT_FAMILY);
    fonts.set_cursive_family(FONT_FAMILY);
    fonts.set_fantasy_family(FONT_FAMILY);
    Arc::new(fonts)
});

fn options() -> mermaid_rs_renderer::RenderOptions {
    let mut options = mermaid_rs_renderer::RenderOptions::default();
    options.theme.font_family = format!("Arial, \"{FONT_FAMILY}\", sans-serif");
    options
}

/// A Mermaid diagram as SVG. The error says what is wrong with the source, on one line.
pub fn svg(source: &str) -> Result<String, String> {
    // The renderer is given model output; a diagram it cannot cope with must not end the UI.
    match catch_unwind(AssertUnwindSafe(|| {
        mermaid_rs_renderer::render_with_options(source, options())
    })) {
        Ok(Ok(svg)) => Ok(svg),
        Ok(Err(e)) => Err(first_line(&format!("{e:#}"))),
        Err(_) => Err("the diagram renderer failed on it".to_string()),
    }
}

fn first_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().trim().to_string()
}

/// `#rrggbb` as a colour; white when it is anything else.
fn background(hex: &str) -> tiny_skia::Color {
    let channel = |at: usize| {
        hex.get(at..at + 2)
            .and_then(|pair| u8::from_str_radix(pair, 16).ok())
    };
    match (hex.len(), hex.strip_prefix('#')) {
        (7, Some(_)) => match (channel(1), channel(3), channel(5)) {
            (Some(r), Some(g), Some(b)) => tiny_skia::Color::from_rgba8(r, g, b, 255),
            _ => tiny_skia::Color::WHITE,
        },
        _ => tiny_skia::Color::WHITE,
    }
}

/// Draws an SVG into pixels no wider than `max_width` and no taller than `max_height`, on the
/// background of the diagram theme.
pub fn pixels(svg: &str, max_width: u32, max_height: u32) -> Result<image::RgbaImage, String> {
    let options = usvg::Options {
        font_family: FONT_FAMILY.to_string(),
        fontdb: FONTS.clone(),
        ..usvg::Options::default()
    };
    let drawn = catch_unwind(AssertUnwindSafe(|| -> Result<image::RgbaImage, String> {
        let tree = usvg::Tree::from_str(svg, &options).map_err(|e| first_line(&e.to_string()))?;
        let size = tree.size();
        let scale = (max_width as f32 / size.width())
            .min(max_height as f32 / size.height())
            .min(MAX_SIDE / size.width().max(size.height()))
            .min(MAX_SCALE);
        let (width, height) = (
            (size.width() * scale).ceil().max(1.0) as u32,
            (size.height() * scale).ceil().max(1.0) as u32,
        );
        let mut pixmap = tiny_skia::Pixmap::new(width, height).ok_or("the diagram has no size to draw at")?;
        pixmap.fill(background(&self::options().theme.background));
        resvg::render(
            &tree,
            tiny_skia::Transform::from_scale(scale, scale),
            &mut pixmap.as_mut(),
        );
        // The background is opaque, so premultiplied and straight colours are the same.
        image::RgbaImage::from_raw(width, height, pixmap.take())
            .ok_or_else(|| "the drawn diagram has the wrong size".to_string())
    }));
    drawn.unwrap_or_else(|_| Err("the diagram could not be drawn".to_string()))
}

/// What a finished fenced block becomes in the transcript: a `mermaid` block is drawn as an
/// image of at most `max_cols` by `max_rows` cells when the terminal shows images, and keeps its
/// source with a line saying why when it cannot be drawn. Anything else is left as it is.
pub fn fenced(language: &str, source: &str, caps: &Caps, max_cols: u16, max_rows: u16) -> Option<Fenced> {
    if !language.eq_ignore_ascii_case("mermaid") || caps.images == azazel_tui::term::Images::None {
        return None;
    }
    let (cell_w, cell_h) = caps.cell_px;
    let drawn = svg(source).and_then(|svg| {
        pixels(
            &svg,
            u32::from(max_cols) * u32::from(cell_w.max(1)),
            u32::from(max_rows) * u32::from(cell_h.max(1)),
        )
    });
    Some(match drawn {
        Ok(picture) => Fenced::Replace(vec![azazel_tui::image::encode_rgba(
            &picture, caps, max_cols, max_rows,
        )?]),
        Err(why) => Fenced::Note(format!(
            "this diagram could not be drawn: {why} (/open diagram tries it in the browser)"
        )),
    })
}

#[cfg(test)]
mod tests {
    use azazel_tui::HistoryItem;
    use azazel_tui::term::Images;

    use super::*;

    const FLOW: &str =
        "flowchart LR\n    A[Start] --> B{Decision}\n    B -->|Yes| C[OK]\n    B -->|No| D[Cancel]\n";

    fn caps(images: Images) -> Caps {
        let mut caps = Caps::detect(&|_| None);
        caps.images = images;
        caps
    }

    #[test]
    fn a_flowchart_becomes_svg_and_then_pixels_with_its_text_drawn() {
        let svg = svg(FLOW).unwrap();
        assert!(svg.contains("<svg") && svg.contains("Decision"));
        let picture = pixels(&svg, 1000, 400).unwrap();
        assert!(picture.width() <= 1000 && picture.height() <= 400);
        assert!(picture.width() > 100 && picture.height() > 20);
        // Shapes and text are there: the picture is not one flat colour, and it has dark pixels
        // (text and outlines) on its light background.
        let first = *picture.get_pixel(0, 0);
        assert!(picture.pixels().any(|pixel| *pixel != first));
        let dark = picture
            .pixels()
            .filter(|pixel| pixel.0[..3].iter().all(|channel| *channel < 96))
            .count();
        assert!(dark > 50, "{dark} dark pixels");
        assert!(picture.pixels().all(|pixel| pixel.0[3] == 255));

        // Without the bundled font nothing could be written: the label alone draws dark pixels.
        let label = r##"<svg xmlns="http://www.w3.org/2000/svg" width="200" height="40"><text x="4" y="28" font-family="Inter, sans-serif" font-size="24" fill="#000000">Decision</text></svg>"##;
        let drawn = pixels(label, 400, 80).unwrap();
        assert!(drawn.pixels().any(|pixel| pixel.0[0] < 64));
    }

    #[test]
    fn a_diagram_is_scaled_into_its_box() {
        let svg = svg(FLOW).unwrap();
        let small = pixels(&svg, 120, 400).unwrap();
        assert!(small.width() <= 120);
        let flat = pixels(&svg, 4000, 30).unwrap();
        assert!(flat.height() <= 30);
    }

    #[test]
    fn only_mermaid_is_drawn_and_only_where_images_show() {
        let kitty = caps(Images::Kitty);
        let Some(Fenced::Replace(items)) = fenced("mermaid", FLOW, &kitty, 80, 30) else {
            panic!("the flowchart was not drawn");
        };
        let [HistoryItem::Block { rows, paint }] = &items[..] else {
            panic!("not one block");
        };
        assert!((1..=30).contains(rows));
        assert!(paint.starts_with("\x1b_Ga=T,f=100"));

        assert!(fenced("rust", "fn main() {}", &kitty, 80, 30).is_none());
        assert!(fenced("mermaid", FLOW, &caps(Images::None), 80, 30).is_none());
    }

    #[test]
    fn a_diagram_that_does_not_parse_keeps_its_source_and_says_why() {
        let kitty = caps(Images::Kitty);
        let broken = "flowchart LR\n    A[Start --> B\n";
        match (svg(broken), fenced("mermaid", broken, &kitty, 80, 30)) {
            (Err(why), Some(Fenced::Note(note))) => {
                assert!(!why.is_empty() && !why.contains('\n'));
                assert!(note.starts_with("this diagram could not be drawn: "), "{note}");
                assert!(note.contains(&why));
            }
            // A renderer that draws it anyway is no failure of ours.
            (Ok(_), Some(Fenced::Replace(_))) => {}
            (parsed, _) => panic!("the source and the block disagree: {parsed:?}"),
        }
        assert!(svg("this is not a diagram at all").is_err());
    }
}
