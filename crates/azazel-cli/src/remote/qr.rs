//! A QR code as lines of terminal text.

use azazel_tui::{Glyph, GlyphLevel, Glyphs};

/// Modules of quiet zone round the code; scanners need some, and the spec asks for four, which a
/// terminal can spare only in part.
const QUIET: usize = 2;

/// The code drawn with the symbols of `glyphs`. On a dark terminal the light modules are the ink,
/// the usual way to print a code there. Two rows of modules share a line, except in ASCII, where
/// a module is two characters and a row of its own.
pub(crate) fn qr_lines(matrix: &[Vec<bool>], glyphs: Glyphs) -> Vec<String> {
    let size = matrix.len();
    if size == 0 {
        return Vec::new();
    }
    // Dark is true; the ink is what is not dark, and what is off the code is not dark either.
    let ink = |x: usize, y: usize| {
        let inside = x >= QUIET && y >= QUIET && x < size + QUIET && y < size + QUIET;
        !(inside && matrix[y - QUIET][x - QUIET])
    };
    let side = size + 2 * QUIET;
    if glyphs.level() == GlyphLevel::Ascii {
        let (full, empty) = (glyphs.get(Glyph::QrFull), glyphs.get(Glyph::QrEmpty));
        return (0..side)
            .map(|y| (0..side).map(|x| if ink(x, y) { full } else { empty }).collect())
            .collect();
    }
    (0..side)
        .step_by(2)
        .map(|y| {
            (0..side)
                .map(|x| {
                    let lower = y + 1 < side && ink(x, y + 1);
                    // A row that is not there is the quiet zone: ink.
                    let lower = lower || y + 1 >= side;
                    glyphs.get(match (ink(x, y), lower) {
                        (true, true) => Glyph::QrFull,
                        (true, false) => Glyph::QrUpper,
                        (false, true) => Glyph::QrLower,
                        (false, false) => Glyph::QrEmpty,
                    })
                })
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn code() -> Vec<Vec<bool>> {
        azazel_remote::qr_matrix("https://example.com/#pair=abc")
    }

    #[test]
    fn two_rows_share_a_line_and_the_quiet_zone_is_ink() {
        let matrix = code();
        let lines = qr_lines(&matrix, Glyphs::new(GlyphLevel::Unicode));
        let side = matrix.len() + 4;
        assert_eq!(lines.len(), side.div_ceil(2));
        assert!(lines.iter().all(|line| line.chars().count() == side));
        assert!(lines[0].chars().all(|c| c == '█'));
    }

    #[test]
    fn ascii_draws_a_row_per_line_and_two_characters_per_module() {
        let matrix = code();
        let lines = qr_lines(&matrix, Glyphs::new(GlyphLevel::Ascii));
        assert_eq!(lines.len(), matrix.len() + 4);
        assert!(
            lines
                .iter()
                .all(|line| line.is_ascii() && line.len() == (matrix.len() + 4) * 2)
        );
        // The top-left finder pattern has a dark corner module, which is blank here.
        assert_eq!(&lines[2][4..6], "  ");
    }
}
