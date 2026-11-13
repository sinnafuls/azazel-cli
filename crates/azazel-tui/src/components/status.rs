//! Status line: session facts on the left, the model on the right.
//!
//! ```text
//! ask · ⑂ main · ~/src/app · ◫ 12% [━╸──────] · ↯ 78 tok/s · $0.42        ⬢ provider/model · medium
//! ```

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Role, Theme};

/// Fewest columns kept between the two sides.
const GAP: usize = 2;
/// A right side cut shorter than this says nothing and is left out.
const MIN_RIGHT: usize = 4;
/// Cells of the context bar between its brackets.
const BAR_CELLS: u64 = 8;
/// The context bar is drawn on terminals at least this wide, where it takes the place of
/// nothing else.
pub const BAR_MIN_WIDTH: usize = 100;

/// Joins the non-empty `left` segments with the separator of the glyph level (` · `) and sets
/// `right` against the right edge. Segments may carry their own SGR styling.
///
/// When the two sides do not fit with a gap between them, whole segments are dropped from the
/// end of the left side, down to its first one. If that is still too wide the right side is
/// cut, then dropped, and only then is the left side cut.
pub fn render(left: &[String], right: &str, width: usize, theme: &Theme) -> Line {
    let separator = theme.glyph(Glyph::Separator);
    let separator_width = text::width(separator);
    let right = right.trim();
    let right_width = text::width(right);
    let mut segments = measured(left);
    let joined = |segments: &[(&str, usize)]| joined_width(segments, separator_width);
    while segments.len() > 1 && wanted(&segments, separator_width, right_width) > width {
        segments.pop();
    }

    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let separator = theme.paint(Role::Muted, separator);
    let mut line = String::new();
    for (segment, _) in &segments {
        if !line.is_empty() {
            line.push_str(&separator);
        }
        line.push_str(segment);
    }
    let used = joined(&segments);
    let gap = if line.is_empty() { 0 } else { GAP };
    let room = width.saturating_sub(used + gap);
    if right.is_empty() || room < MIN_RIGHT.min(right_width) {
        return text::truncate(&line, width, ellipsis);
    }
    let right = text::truncate(right, room, ellipsis);
    line.extend(std::iter::repeat_n(' ', width - used - text::width(&right)));
    line.push_str(&right);
    line
}

/// Whether both sides fit `width` whole, with the gap between them: [`render`] would drop and
/// cut nothing.
pub fn fits(left: &[String], right: &str, width: usize, theme: &Theme) -> bool {
    let separator_width = text::width(theme.glyph(Glyph::Separator));
    wanted(&measured(left), separator_width, text::width(right.trim())) <= width
}

/// The segments that say something, each with its width.
fn measured(left: &[String]) -> Vec<(&str, usize)> {
    left.iter()
        .map(|segment| segment.trim())
        .filter(|segment| !segment.is_empty())
        .map(|segment| (segment, text::width(segment)))
        .collect()
}

fn joined_width(segments: &[(&str, usize)], separator_width: usize) -> usize {
    segments.iter().map(|(_, width)| width).sum::<usize>()
        + separator_width * segments.len().saturating_sub(1)
}

/// Columns the segments and a right side `right_width` wide need to be shown whole.
fn wanted(segments: &[(&str, usize)], separator_width: usize, right_width: usize) -> usize {
    let gap = if segments.is_empty() || right_width == 0 {
        0
    } else {
        GAP
    };
    joined_width(segments, separator_width) + gap + right_width
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

/// How full the context window is as a bar of eight cells, `[━━━╸────]`: half a cell per
/// 6.25 %, in the colour of [`context_role`].
pub fn context_bar(percent: u64, theme: &Theme) -> String {
    let halves = ((percent.min(100) * BAR_CELLS * 2 + 50) / 100).min(BAR_CELLS * 2);
    let (full, head) = (halves / 2, halves % 2);
    let cells = |glyph: Glyph, count: u64| theme.glyph(glyph).repeat(usize::try_from(count).unwrap_or(0));
    let filled = format!(
        "{}{}",
        cells(Glyph::ProgressFull, full),
        cells(Glyph::ProgressHead, head)
    );
    let empty = cells(Glyph::ProgressEmpty, BAR_CELLS - full - head);
    let frame = |edge: &str| theme.paint(Role::BorderMuted, edge);
    let mut bar = frame("[");
    if !filled.is_empty() {
        bar.push_str(&theme.paint(context_role(percent).unwrap_or(Role::Muted), &filled));
    }
    if !empty.is_empty() {
        bar.push_str(&theme.paint(Role::BorderMuted, &empty));
    }
    bar.push_str(&frame("]"));
    bar
}

/// `text` behind the icon of `glyph`: `⑂ main`, `git: main` at the `ascii` level. The icon is
/// left out where the level has none, and where it is a word the text itself starts with.
pub fn labelled(glyph: Glyph, text: &str, theme: &Theme) -> String {
    let icon = theme.glyph(glyph);
    let word = icon.trim_end_matches(':');
    let repeats = !word.is_empty()
        && word.chars().all(|c| c.is_ascii_alphabetic())
        && text
            .get(..word.len())
            .is_some_and(|start| start.eq_ignore_ascii_case(word))
        && !text[word.len()..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric);
    if icon.is_empty() || repeats {
        text.to_string()
    } else {
        format!("{icon} {text}")
    }
}

/// Money spent, with less precision the more it is: `$0.42`, `$12.4`, `$240`.
pub fn cost(usd: f64, theme: &Theme) -> String {
    let amount = if usd < 10.0 {
        format!("{usd:.2}")
    } else if usd < 100.0 {
        format!("{usd:.1}")
    } else {
        format!("{usd:.0}")
    };
    match theme.glyph(Glyph::Cost) {
        "$" => format!("${amount}"),
        icon => format!("{icon} {amount}"),
    }
}

/// Output speed as `↯ 78 tok/s`; nothing below one token per second, which says nothing.
pub fn rate(tokens_per_second: f64, theme: &Theme) -> Option<String> {
    (tokens_per_second >= 1.0).then(|| {
        format!(
            "{}{tokens_per_second:.0} tok/s",
            theme.glyphs().prefix(Glyph::Rate)
        )
    })
}

/// `path` cut from the left to at most `max` columns, starting at a directory boundary where
/// there is one: `…/crates/azazel-tui`.
pub fn shorten_path(path: &str, max: usize, theme: &Theme) -> String {
    if text::width(path) <= max {
        return path.to_string();
    }
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let room = max.saturating_sub(text::width(ellipsis));
    let mut used = 0;
    let mut from = path.len();
    let mut buffer = [0u8; 4];
    for (at, c) in path.char_indices().rev() {
        let cells = text::width(c.encode_utf8(&mut buffer));
        if used + cells > room {
            break;
        }
        used += cells;
        from = at;
    }
    let tail = &path[from..];
    // Whole directory names read better than the end of one.
    let tail = match tail.find(['/', '\\']) {
        Some(at) if at + 1 < tail.len() => &tail[at..],
        _ => tail,
    };
    if text::width(ellipsis) > max {
        return text::truncate(tail, max, "");
    }
    format!("{ellipsis}{tail}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn segments(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn ascii() -> Theme {
        Theme::dark().with_glyphs(GlyphLevel::Ascii)
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
        let line = render(&segments(&["ask", "", "~/src"]), "m", 20, &ascii());
        assert_eq!(text::strip_ansi(&line), "ask | ~/src        m");
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
    fn segments_drop_from_the_end_before_the_right_side_is_cut() {
        let left = segments(&["ask", "⑂ main", "~/src/app", "◫ 42%", "$0.42", "⚑ goal"]);
        let right = "⬢ mock/model · high";
        let theme = Theme::dark();
        let shown = |width: usize| text::strip_ansi(&render(&left, right, width, &theme));
        // 49 columns of left, a gap of two and 19 of right.
        assert_eq!(
            shown(70),
            "ask · ⑂ main · ~/src/app · ◫ 42% · $0.42 · ⚑ goal  ⬢ mock/model · high"
        );
        assert_eq!(
            shown(69),
            "ask · ⑂ main · ~/src/app · ◫ 42% · $0.42          ⬢ mock/model · high"
        );
        assert_eq!(shown(53), "ask · ⑂ main · ~/src/app · ◫ 42%  ⬢ mock/model · high");
        assert_eq!(shown(52), "ask · ⑂ main · ~/src/app         ⬢ mock/model · high");
        assert_eq!(shown(33), "ask · ⑂ main  ⬢ mock/model · high");
        assert_eq!(shown(32), "ask          ⬢ mock/model · high");
        // The first segment is never dropped: now the right side gives way, then the left.
        assert_eq!(shown(24), "ask  ⬢ mock/model · high");
        assert_eq!(shown(23), "ask  ⬢ mock/model · hi…");
        assert_eq!(shown(9), "ask  ⬢ m…");
        assert_eq!(shown(8), "ask");
        assert_eq!(shown(2), "a…");
        for width in 0..120 {
            let line = render(&left, right, width, &theme);
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
            if width >= 24 {
                assert_eq!(text::width(&line), width);
                assert!(text::strip_ansi(&line).ends_with(right), "width {width}");
            }
        }
    }

    #[test]
    fn without_a_right_side_segments_drop_until_the_left_fits() {
        let left = segments(&["ask", "/a/long/directory", "◫ 42%"]);
        let theme = Theme::dark();
        let shown = |width: usize| text::strip_ansi(&render(&left, "", width, &theme));
        assert_eq!(shown(31), "ask · /a/long/directory · ◫ 42%");
        assert_eq!(shown(30), "ask · /a/long/directory");
        assert_eq!(shown(22), "ask");
    }

    #[test]
    fn fitting_means_nothing_is_dropped_or_cut() {
        let left = segments(&["ask", "", "~/src", "◫ 42%"]);
        let theme = Theme::dark();
        // 19 columns of left, a gap of two and 12 of right.
        assert!(fits(&left, "⬢ mock/model", 33, &theme));
        assert!(!fits(&left, "⬢ mock/model", 32, &theme));
        assert!(fits(&left, "", 19, &theme) && !fits(&left, "", 18, &theme));
        assert!(fits(&[], "⬢ mock/model", 12, &theme));
        for width in 0..60 {
            let whole = text::strip_ansi(&render(&left, "⬢ mock/model", width, &theme));
            let complete = whole.starts_with("ask · ~/src · ◫ 42%") && whole.ends_with("⬢ mock/model");
            assert_eq!(
                fits(&left, "⬢ mock/model", width, &theme),
                complete,
                "width {width}"
            );
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

    #[test]
    fn the_context_bar_fills_by_half_cells() {
        let theme = Theme::dark();
        let bar = |percent| text::strip_ansi(&context_bar(percent, &theme));
        assert_eq!(bar(0), "[────────]");
        assert_eq!(bar(3), "[────────]");
        assert_eq!(bar(4), "[╸───────]");
        assert_eq!(bar(12), "[━───────]");
        assert_eq!(bar(42), "[━━━╸────]");
        assert_eq!(bar(50), "[━━━━────]");
        assert_eq!(bar(97), "[━━━━━━━━]");
        assert_eq!(bar(400), "[━━━━━━━━]");
        for percent in 0..=120 {
            assert_eq!(text::width(&context_bar(percent, &theme)), 10, "{percent}%");
        }
        // The filled part warns as the percentage does.
        assert!(context_bar(80, &theme).contains(&theme.fg(Role::Warning)));
        assert!(context_bar(95, &theme).contains(&theme.fg(Role::Error)));
        assert_eq!(text::strip_ansi(&context_bar(42, &ascii())), "[===>----]");
    }

    #[test]
    fn labels_take_the_icon_of_the_level_unless_it_repeats_the_text() {
        let (unicode, ascii) = (Theme::dark(), ascii());
        assert_eq!(labelled(Glyph::Branch, "main", &unicode), "⑂ main");
        assert_eq!(labelled(Glyph::Branch, "main", &ascii), "git: main");
        assert_eq!(labelled(Glyph::Branch, "gitea-fix", &ascii), "git: gitea-fix");
        assert_eq!(labelled(Glyph::Goal, "goal active", &unicode), "⚑ goal active");
        assert_eq!(labelled(Glyph::Goal, "goal active", &ascii), "goal active");
        assert_eq!(labelled(Glyph::Goal, "ship it", &ascii), "goal: ship it");
        assert_eq!(labelled(Glyph::Agents, "2 agents", &ascii), "2 agents");
        assert_eq!(labelled(Glyph::Context, "42%", &ascii), "ctx 42%");
    }

    #[test]
    fn cost_and_rate_are_short() {
        let theme = Theme::dark();
        assert_eq!(cost(0.0, &theme), "$0.00");
        assert_eq!(cost(0.416, &theme), "$0.42");
        assert_eq!(cost(9.994, &theme), "$9.99");
        assert_eq!(cost(12.44, &theme), "$12.4");
        assert_eq!(cost(240.3, &theme), "$240");
        assert_eq!(cost(1.5, &ascii()), "$1.50");
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        assert_eq!(cost(1.5, &nerd), "\u{f155} 1.50");

        assert_eq!(rate(78.4, &theme).as_deref(), Some("↯ 78 tok/s"));
        assert_eq!(rate(1.0, &theme).as_deref(), Some("↯ 1 tok/s"));
        assert_eq!(rate(0.99, &theme), None);
        assert_eq!(rate(78.4, &ascii()).as_deref(), Some("78 tok/s"));
    }

    #[test]
    fn long_paths_keep_their_end() {
        let theme = Theme::dark();
        assert_eq!(shorten_path("~/src/app", 20, &theme), "~/src/app");
        assert_eq!(
            shorten_path("~/projects/azazel/crates/azazel-tui", 24, &theme),
            "…/crates/azazel-tui"
        );
        assert_eq!(
            shorten_path("C:\\work\\azazel\\crates", 16, &theme),
            "…\\azazel\\crates"
        );
        // One long name has no boundary to start at.
        assert_eq!(shorten_path("a-very-long-directory", 8, &theme), "…rectory");
        assert_eq!(
            shorten_path("~/projects/azazel/crates", 12, &ascii()),
            ".../crates"
        );
        for max in 0..40 {
            for path in ["~/projects/azazel/crates/azazel-tui", "/日本語/パス/名前"] {
                let shown = shorten_path(path, max, &theme);
                assert!(text::width(&shown) <= max, "{max}: {shown:?}");
            }
        }
    }

    #[test]
    fn the_ascii_level_draws_with_ascii_only() {
        let theme = ascii();
        let left = vec![
            "ask".to_string(),
            labelled(Glyph::Branch, "main", &theme),
            shorten_path("~/projects/azazel/crates/azazel-tui", 24, &theme),
            format!(
                "{} {}",
                labelled(Glyph::Context, "42%", &theme),
                context_bar(42, &theme)
            ),
            rate(78.0, &theme).unwrap(),
            cost(0.42, &theme),
            labelled(Glyph::Usage, "61% 5h", &theme),
            labelled(Glyph::Agents, "2 agents", &theme),
            labelled(Glyph::Goal, "goal active", &theme),
        ];
        let right = labelled(Glyph::Model, "mock/model | high", &theme);
        let line = text::strip_ansi(&render(&left, &right, 140, &theme));
        assert!(
            line.starts_with(
                "ask | git: main | .../crates/azazel-tui | ctx 42% [===>----] | 78 tok/s | $0.42 | \
                 use 61% 5h | 2 agents | goal active  "
            ),
            "{line:?}"
        );
        assert!(line.ends_with("  mock/model | high"), "{line:?}");
        for width in 0..160 {
            let line = text::strip_ansi(&render(&left, &right, width, &theme));
            assert!(line.is_ascii(), "width {width}: {line:?}");
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
        }
    }
}
