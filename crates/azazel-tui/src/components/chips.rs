//! A row of names with one of them current: what a key steps through, shown for a moment.
//!
//! ```text
//! … @default [@smol] @slow …  ctrl+p next
//! ```

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// Columns between the names and the hint.
const HINT_GAP: usize = 2;

/// The names of `names` around the one at `current`, as many as `width` columns hold, with the
/// current one in brackets on the band of a selected row. A mark stands at an end that hides
/// names. `hint` follows the names where there is room for it.
pub fn strip(names: &[String], current: usize, hint: &str, width: usize, theme: &Theme) -> Line {
    if names.is_empty() || width == 0 {
        return String::new();
    }
    let current = current.min(names.len() - 1);
    // A name, the brackets of the current one, and the space before each but the first.
    let cell = |at: usize| text::width(&names[at]) + 2 * usize::from(at == current);
    let more = theme.glyph(Glyph::Ellipsis);
    let more_width = text::width(more) + 1;
    // What the marks of hidden names take when the names `from..to` are shown.
    let marks = |from: usize, to: usize| {
        usize::from(from > 0) * more_width + usize::from(to < names.len()) * more_width
    };
    let (mut from, mut to) = (current, current + 1);
    let mut used = cell(current);
    let step = |at: usize| cell(at) + 1;
    if used + marks(from, to) > width {
        // Not even the current name fits whole.
        let name = text::truncate(&names[current], width.saturating_sub(2), more);
        return text::truncate(&format!("[{name}]"), width, "");
    }
    // The neighbours on both sides in turn, the next one first.
    loop {
        let mut grew = false;
        if to < names.len() && used + step(to) + marks(from, to + 1) <= width {
            used += step(to);
            to += 1;
            grew = true;
        }
        if from > 0 && used + step(from - 1) + marks(from - 1, to) <= width {
            used += step(from - 1);
            from -= 1;
            grew = true;
        }
        if !grew {
            break;
        }
    }

    let mut line = String::new();
    if from > 0 {
        line.push_str(&theme.paint(Role::Muted, &format!("{more} ")));
    }
    for (at, name) in names.iter().enumerate().take(to).skip(from) {
        if at > from {
            line.push(' ');
        }
        if at == current {
            line.push_str(&theme.bg(Role::SelectedBg));
            line.push_str(&theme.fg(Role::Accent));
            line.push_str(&format!("[{name}]"));
            line.push_str(RESET);
        } else {
            line.push_str(&theme.paint(Role::Muted, name));
        }
    }
    if to < names.len() {
        line.push_str(&theme.paint(Role::Muted, &format!(" {more}")));
    }
    used += marks(from, to);
    if !hint.is_empty() && used + HINT_GAP + text::width(hint) <= width {
        line.extend(std::iter::repeat_n(' ', HINT_GAP));
        line.push_str(&theme.paint(Role::Muted, hint));
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn names(items: &[&str]) -> Vec<String> {
        items.iter().map(|name| (*name).to_string()).collect()
    }

    fn shown(items: &[&str], current: usize, hint: &str, width: usize) -> String {
        text::strip_ansi(&strip(&names(items), current, hint, width, &Theme::dark()))
    }

    #[test]
    fn every_name_is_shown_and_the_current_one_is_marked() {
        let roles = ["@default", "@smol", "@slow"];
        assert_eq!(shown(&roles, 1, "", 60), "@default [@smol] @slow");
        assert_eq!(shown(&roles, 0, "", 60), "[@default] @smol @slow");
        assert_eq!(
            shown(&roles, 2, "ctrl+p next", 60),
            "@default @smol [@slow]  ctrl+p next"
        );
        // The current name stands on the band of a selected row, the others are quiet.
        let theme = Theme::dark();
        let line = strip(&names(&roles), 1, "", 60, &theme);
        assert!(
            line.contains(&format!(
                "{}{}[@smol]{RESET}",
                theme.bg(Role::SelectedBg),
                theme.fg(Role::Accent)
            )),
            "{line:?}"
        );
        assert!(
            line.starts_with(&theme.paint(Role::Muted, "@default")),
            "{line:?}"
        );
        // An index past the end is the last name.
        assert_eq!(shown(&roles, 9, "", 60), "@default @smol [@slow]");
    }

    #[test]
    fn a_narrow_row_keeps_the_neighbours_of_the_current_name() {
        let models = ["alpha", "beta", "gamma", "delta", "epsilon", "zeta"];
        // All of them take 37 columns.
        assert_eq!(shown(&models, 3, "", 37), "alpha beta gamma [delta] epsilon zeta");
        // The next name comes before the one before, and a mark stands where names are hidden.
        assert_eq!(shown(&models, 3, "", 30), "… gamma [delta] epsilon zeta");
        assert_eq!(shown(&models, 3, "", 22), "… [delta] epsilon zeta");
        assert_eq!(shown(&models, 3, "", 21), "… [delta] epsilon …");
        assert_eq!(shown(&models, 3, "", 11), "… [delta] …");
        assert_eq!(shown(&models, 0, "", 16), "[alpha] beta …");
        assert_eq!(shown(&models, 5, "", 17), "… epsilon [zeta]");
        // The hint is the first thing to go.
        assert_eq!(
            shown(&models, 3, "alt+p all", 40),
            "alpha beta gamma [delta] epsilon zeta"
        );
        assert_eq!(
            shown(&models, 3, "alt+p all", 48),
            "alpha beta gamma [delta] epsilon zeta  alt+p all"
        );
        // Too narrow for the name itself.
        assert_eq!(shown(&models, 4, "", 8), "[epsil…]");
        assert_eq!(shown(&models, 4, "", 2), "[]");
    }

    #[test]
    fn the_row_fits_any_width_at_any_glyph_level() {
        let models = names(&[
            "@default",
            "anthropic/claude-opus-5-5",
            "openai/gpt-6",
            "@smol",
            "日本語/モデル",
        ]);
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            for width in 0..=120 {
                for current in 0..models.len() {
                    let line = strip(&models, current, "ctrl+p next · alt+p all", width, &theme);
                    assert!(
                        text::width(&line) <= width,
                        "{level} at {width}, {current}: {line:?}"
                    );
                    // Whatever else is cut, the current name is in the row.
                    if width >= 30 {
                        let plain = text::strip_ansi(&line);
                        assert!(plain.contains(&format!("[{}]", models[current])), "{plain:?}");
                    }
                }
            }
        }
        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let line = strip(&names(&["@default", "@smol", "@slow"]), 1, "", 16, &ascii);
        assert_eq!(text::strip_ansi(&line), "... [@smol] ...");
        assert_eq!(strip(&[], 0, "hint", 40, &ascii), "");
    }
}
