//! Activity spinner.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::glyphs::Glyphs;

/// Time between spinner frames. A redraw timer is only needed while a spinner is visible.
pub const INTERVAL: Duration = Duration::from_millis(80);

const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

pub struct Spinner;

impl Spinner {
    pub const INTERVAL: Duration = INTERVAL;

    /// The glyph for a tick; one column wide. Ticks advance once per [`INTERVAL`].
    pub fn frame(tick: u64) -> &'static str {
        let index = usize::try_from(tick % 10).unwrap_or_default();
        FRAMES[index % FRAMES.len()]
    }

    /// The tick reached after `elapsed` time.
    pub fn tick_at(elapsed: Duration) -> u64 {
        u64::try_from(elapsed.as_millis() / INTERVAL.as_millis()).unwrap_or(u64::MAX)
    }

    /// The glyph to draw now. It follows the wall clock, so every spinner drawn in one frame
    /// shows the same glyph and all of them advance together.
    pub fn now() -> &'static str {
        Spinner::frame(Spinner::tick_at(since_epoch()))
    }

    /// The glyph for a tick in the frames of `glyphs`: braille, or `. o O o` at the `ascii`
    /// level (no `-` or `|`, which the rules and separators are drawn with).
    pub fn frame_in(glyphs: Glyphs, tick: u64) -> &'static str {
        let frames = glyphs.spinner();
        let count = u64::try_from(frames.len()).unwrap_or(1).max(1);
        frames
            .get(usize::try_from(tick % count).unwrap_or_default())
            .copied()
            .unwrap_or("")
    }

    /// [`Spinner::now`] in the frames of `glyphs`.
    pub fn now_in(glyphs: Glyphs) -> &'static str {
        Spinner::frame_in(glyphs, Spinner::tick_at(since_epoch()))
    }
}

fn since_epoch() -> Duration {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::{Glyph, GlyphLevel};

    #[test]
    fn each_glyph_level_cycles_through_its_own_frames() {
        let unicode = Glyphs::new(GlyphLevel::Unicode);
        let braille: Vec<&str> = (0..10).map(|tick| Spinner::frame_in(unicode, tick)).collect();
        assert_eq!(braille, FRAMES);
        assert_eq!(Spinner::frame_in(unicode, 13), FRAMES[3]);

        let ascii = Glyphs::new(GlyphLevel::Ascii);
        let plain: Vec<&str> = (0..5).map(|tick| Spinner::frame_in(ascii, tick)).collect();
        assert_eq!(plain, [".", "o", "O", "o", "."]);
        // Not a character the rules and the separators are drawn with.
        let drawn = [ascii.get(Glyph::Separator), "-", "|"];
        assert!(ascii.spinner().iter().all(|frame| !drawn.contains(frame)));
        assert!(ascii.spinner().contains(&Spinner::now_in(ascii)));
    }
}
