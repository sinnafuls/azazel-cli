//! Status line: session facts on the left, the model on the right.
//!
//! ```text
//! ask · ⑂ main · ~/src/app · ◫ 12% · $0.42                ⬢ provider/model · medium
//! ```

use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Fewest columns kept between the two sides.
const GAP: usize = 2;
/// A right side cut shorter than this says nothing and is left out.
const MIN_RIGHT: usize = 4;

/// Joins the non-empty `left` segments with ` · ` and sets `right` against the right edge.
/// Segments may carry their own SGR styling. When the two sides do not fit with a gap between
/// them, the right side is cut first, then dropped, and only then is the left side cut.
pub fn render(left: &[String], right: &str, width: usize, theme: &Theme) -> Line {
    let separator = theme.paint(Role::Muted, " · ");
    let mut line = String::new();
    for segment in left.iter().map(|s| s.trim()).filter(|s| !s.is_empty()) {
        if !line.is_empty() {
            line.push_str(&separator);
        }
        line.push_str(segment);
    }
    let right = right.trim();
    let used = text::width(&line);
    let gap = if line.is_empty() { 0 } else { GAP };
    let room = width.saturating_sub(used + gap);
    if right.is_empty() || room < MIN_RIGHT.min(text::width(right)) {
        return text::truncate(&line, width, "…");
    }
    let right = text::truncate(right, room, "…");
    line.extend(std::iter::repeat_n(' ', width - used - text::width(&right)));
    line.push_str(&right);
    line
}

/// The colour that warns about a context window filled to `percent`: nothing up to 70 %, a
/// warning above it, an error above 90 %.
pub fn context_role(percent: u64) -> Option<Role> {
    match percent {
        0..=70 => None,
        71..=90 => Some(Role::Warning),
        _ => Some(Role::Error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segments(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn joins_segments_and_skips_empty_ones() {
        let line = render(
            &segments(&["ask", "", "~/src", " ◫ 12% "]),
            "",
            80,
            &Theme::dark(),
        );
        assert_eq!(text::strip_ansi(&line), "ask · ~/src · ◫ 12%");
    }

    #[test]
    fn right_side_sits_against_the_right_edge() {
        let line = render(
            &segments(&["ask", "~/src"]),
            "⬢ mock/model · high",
            40,
            &Theme::dark(),
        );
        assert_eq!(
            text::strip_ansi(&line),
            "ask · ~/src          ⬢ mock/model · high"
        );
        assert_eq!(text::width(&line), 40);

        let alone = render(&[], "⬢ mock/model", 20, &Theme::dark());
        assert_eq!(text::strip_ansi(&alone), "        ⬢ mock/model");
    }

    #[test]
    fn right_side_is_cut_first_then_dropped_then_the_left_is_cut() {
        let left = segments(&["ask", "/a/long/directory"]);
        let theme = Theme::dark();
        // 23 columns of left, a gap of two, and what remains for the right side.
        let cut = render(&left, "⬢ mock/model · high", 34, &theme);
        assert_eq!(text::strip_ansi(&cut), "ask · /a/long/directory  ⬢ mock/m…");
        let dropped = render(&left, "⬢ mock/model · high", 27, &theme);
        assert_eq!(text::strip_ansi(&dropped), "ask · /a/long/directory");
        let short = render(&left, "⬢ mock/model · high", 12, &theme);
        assert_eq!(text::strip_ansi(&short), "ask · /a/lo…");
        for width in 0..50 {
            assert!(text::width(&render(&left, "⬢ mock/model · high", width, &theme)) <= width);
        }
    }

    #[test]
    fn empty_input_is_an_empty_line() {
        assert_eq!(render(&[], "", 10, &Theme::dark()), "");
        assert_eq!(render(&segments(&["status"]), "x", 0, &Theme::dark()), "");
    }

    #[test]
    fn context_colour_follows_the_thresholds() {
        assert_eq!(context_role(0), None);
        assert_eq!(context_role(70), None);
        assert_eq!(context_role(71), Some(Role::Warning));
        assert_eq!(context_role(90), Some(Role::Warning));
        assert_eq!(context_role(91), Some(Role::Error));
        assert_eq!(context_role(250), Some(Role::Error));
    }
}
