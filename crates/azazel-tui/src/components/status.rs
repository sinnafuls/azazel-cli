//! Status line: segments on the left and on the right, laid out to fit the terminal.
//!
//! ```text
//! ask · ⑂ main · ~/src/app · ◫ 12% [━╸──────] · ↯ 78 tok/s · $0.42        ⬢ provider/model · medium
//! ```
//!
//! A [`Segment`] is a piece of text in the colours of its roles, with a priority: when the line
//! is too narrow for all of them, the segment with the lowest priority leaves first.

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{Color, RESET, Role, Theme};

/// Fewest columns kept between the two sides.
const GAP: usize = 2;
/// A right side cut shorter than this says nothing and is left out.
const MIN_RIGHT: usize = 4;
/// Cells of the context bar between its brackets, unless the caller asks for another number.
pub const BAR_CELLS: u64 = 8;
/// The context bar is drawn on terminals at least this wide, unless the caller says otherwise.
pub const BAR_MIN_WIDTH: usize = 100;
/// The priority of a segment that was given none.
const PRIORITY: u8 = 50;

/// What stands between two segments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Separator {
    /// ` · `
    #[default]
    Dot,
    /// ` │ `
    Bar,
    /// Two spaces.
    Space,
    /// Every segment is a block of colour; with a Nerd Font an arrow ends each block.
    Powerline,
}

/// One piece of the status line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Segment {
    /// The text, piece by piece, each in the colour of its role.
    spans: Vec<(Role, String)>,
    /// Text that carries colours of its own (the output of a command) and is shown as it is.
    styled: Option<String>,
    width: usize,
    priority: u8,
    joined: bool,
    fill: Option<Role>,
}

impl Segment {
    /// `text` in the colour of `role`.
    pub fn new(role: Role, text: impl Into<String>) -> Segment {
        Segment {
            spans: Vec::new(),
            styled: None,
            width: 0,
            priority: PRIORITY,
            joined: false,
            fill: None,
        }
        .and(role, text)
    }

    /// Text with SGR colours of its own, shown as it is and never as a block.
    pub fn styled(text: impl Into<String>) -> Segment {
        let text = text.into();
        Segment {
            width: text::width(&text),
            spans: Vec::new(),
            styled: Some(text),
            priority: PRIORITY,
            joined: false,
            fill: None,
        }
    }

    /// Adds `text` in the colour of `role` right behind what the segment holds.
    pub fn and(mut self, role: Role, text: impl Into<String>) -> Segment {
        let text = text.into();
        if !text.is_empty() {
            self.width += text::width(&text);
            self.spans.push((role, text));
        }
        self
    }

    /// Segments with a lower priority leave a narrow line first; the one with the highest
    /// priority of its side never leaves.
    pub fn priority(mut self, priority: u8) -> Segment {
        self.priority = priority;
        self
    }

    /// Stands one space behind the segment before it, without a separator, in the same block,
    /// and leaves the line when that segment does.
    pub fn joined(mut self) -> Segment {
        self.joined = true;
        self
    }

    /// Under [`Separator::Powerline`] its block is filled with the colour of `role` and its
    /// text is written in the colour of the background.
    pub fn filled(mut self, role: Role) -> Segment {
        self.fill = Some(role);
        self
    }

    /// Columns the text takes.
    pub fn width(&self) -> usize {
        self.width
    }

    /// The text in its colours, as it stands between plain separators.
    pub fn paint(&self, theme: &Theme) -> String {
        match &self.styled {
            Some(text) => close(text.clone()),
            None => self
                .spans
                .iter()
                .map(|(role, text)| theme.paint(*role, text))
                .collect(),
        }
    }
}

/// `line` with its colours switched off at the end, when it has any.
fn close(mut line: String) -> String {
    if line.contains('\x1b') && !line.ends_with(RESET) {
        line.push_str(RESET);
    }
    line
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

/// How segments are set side by side: the separator of plain styles, or blocks.
struct Style<'a> {
    theme: &'a Theme,
    /// The separator between two segments; empty for blocks.
    between: &'static str,
    between_width: usize,
    blocks: bool,
}

impl<'a> Style<'a> {
    fn new(separator: Separator, theme: &'a Theme) -> Style<'a> {
        let between = match separator {
            Separator::Dot => theme.glyph(Glyph::Separator),
            Separator::Bar => theme.glyph(Glyph::SeparatorBar),
            Separator::Space => "  ",
            Separator::Powerline => "",
        };
        Style {
            theme,
            between,
            between_width: text::width(between),
            blocks: separator == Separator::Powerline,
        }
    }

    /// The arrow that ends a block on `side`; empty where the symbols have none.
    fn arrow(&self, side: Side) -> &'static str {
        self.theme.glyph(match side {
            Side::Left => Glyph::PowerRight,
            Side::Right => Glyph::PowerLeft,
        })
    }

    /// Whether `segments[at]` shares the block (or the place between two separators) of the
    /// segment before it.
    fn joins(segments: &[&Segment], at: usize) -> bool {
        at > 0 && segments[at].joined && segments[at - 1].styled.is_none() && segments[at].styled.is_none()
    }

    /// Columns `segments` take side by side.
    fn width(&self, segments: &[&Segment], side: Side) -> usize {
        let arrow = text::width(self.arrow(side));
        let mut width = 0;
        for (at, segment) in segments.iter().enumerate() {
            width += segment.width;
            if Self::joins(segments, at) {
                width += 1;
            } else if self.blocks {
                // The padding of a block and the arrow that ends it.
                if segment.styled.is_none() {
                    width += 2 + arrow;
                }
            } else if at > 0 {
                width += self.between_width;
            }
        }
        width
    }

    /// Columns both sides need to be shown whole, with the gap between them.
    fn wanted(&self, left: &[&Segment], right: &[&Segment]) -> usize {
        let gap = if left.is_empty() || right.is_empty() {
            0
        } else {
            GAP
        };
        self.width(left, Side::Left) + gap + self.width(right, Side::Right)
    }

    fn draw(&self, segments: &[&Segment], side: Side) -> String {
        if self.blocks {
            return self.draw_blocks(segments, side);
        }
        let between = self.theme.paint(Role::Muted, self.between);
        let mut line = String::new();
        for (at, segment) in segments.iter().enumerate() {
            if Self::joins(segments, at) {
                line.push(' ');
            } else if at > 0 {
                line.push_str(&between);
            }
            line.push_str(&segment.paint(self.theme));
        }
        line
    }

    /// The background of the block that starts with `first`, the `nth` block of its side.
    fn block_color(&self, first: &Segment, nth: usize) -> Color {
        match first.fill {
            Some(role) => self.theme.color(role),
            // Two quiet backgrounds in turn tell neighbours apart where there is no arrow.
            None if nth.is_multiple_of(2) => self.theme.color(Role::SelectedBg),
            None => self.theme.color(Role::CodeBg),
        }
    }

    fn draw_blocks(&self, segments: &[&Segment], side: Side) -> String {
        let theme = self.theme;
        // Text on a filled block is in the colour of the background behind the line.
        let ink = match theme.color(Role::Surface) {
            Color::Default if theme.is_light() => Color::Rgb(255, 255, 255),
            Color::Default => Color::Rgb(0, 0, 0),
            color => color,
        };
        // Where each block starts, and its colour; text with its own colours is no block.
        let mut blocks: Vec<(usize, Option<Color>)> = Vec::new();
        for (at, segment) in segments.iter().enumerate() {
            if Self::joins(segments, at) {
                continue;
            }
            let color = match segment.styled {
                Some(_) => None,
                None => Some(self.block_color(segment, blocks.len())),
            };
            blocks.push((at, color));
        }
        let arrow = self.arrow(side);
        let background = |color: Option<Color>| theme.bg_color(color.unwrap_or(Color::Default));
        let mut line = String::new();
        for (nth, (start, color)) in blocks.iter().enumerate() {
            let end = blocks.get(nth + 1).map_or(segments.len(), |(next, _)| *next);
            let Some(color) = *color else {
                if line.contains('\x1b') {
                    line.push_str(RESET);
                }
                line.push_str(&segments[*start].paint(theme));
                continue;
            };
            let before = nth.checked_sub(1).and_then(|nth| blocks[nth].1);
            let after = blocks.get(nth + 1).and_then(|(_, color)| *color);
            if side == Side::Right && !arrow.is_empty() {
                line.push_str(&background(before));
                line.push_str(&theme.fg_color(color));
                line.push_str(arrow);
            }
            line.push_str(&theme.bg_color(color));
            line.push(' ');
            for (at, segment) in segments[*start..end].iter().enumerate() {
                if at > 0 {
                    line.push(' ');
                }
                for (role, text) in &segment.spans {
                    let color = match segments[*start].fill {
                        Some(_) => ink,
                        None => theme.color(*role),
                    };
                    line.push_str(&theme.fg_color(color));
                    line.push_str(text);
                }
            }
            line.push(' ');
            if side == Side::Left && !arrow.is_empty() {
                line.push_str(&background(after));
                line.push_str(&theme.fg_color(color));
                line.push_str(arrow);
            }
        }
        close(line)
    }
}

/// Takes the segment with the lowest priority off the line: of several, the last one of the
/// left side, then the first one of the right side. The segment with the highest priority of
/// each side stays. False when there is nothing left to take.
fn drop_one<'a>(left: &mut Vec<&'a Segment>, right: &mut Vec<&'a Segment>) -> bool {
    fn keeper(side: &[&Segment]) -> Option<usize> {
        let highest = side.iter().map(|segment| segment.priority).max()?;
        side.iter().position(|segment| segment.priority == highest)
    }
    let mut lowest: Option<(Side, usize, u8)> = None;
    let keep = keeper(left);
    for (at, segment) in left.iter().enumerate() {
        if Some(at) != keep && lowest.is_none_or(|(_, _, priority)| segment.priority <= priority) {
            lowest = Some((Side::Left, at, segment.priority));
        }
    }
    let keep = keeper(right);
    for (at, segment) in right.iter().enumerate() {
        if Some(at) != keep && lowest.is_none_or(|(_, _, priority)| segment.priority < priority) {
            lowest = Some((Side::Right, at, segment.priority));
        }
    }
    let Some((side, at, _)) = lowest else {
        return false;
    };
    let side = match side {
        Side::Left => left,
        Side::Right => right,
    };
    side.remove(at);
    // What stood joined to it goes with it.
    while side.get(at).is_some_and(|segment| segment.joined) {
        side.remove(at);
    }
    true
}

/// Sets the `left` segments against the left edge and the `right` ones against the right edge,
/// with `separator` between the segments of a side. Empty segments are skipped.
///
/// When the two sides do not fit with a gap between them, segments leave the line in the order
/// of their priority, lowest first, until one is left on each side. If that is still too wide
/// the right side is cut, then dropped, and only then is the left side cut.
pub fn render(
    left: &[Segment],
    right: &[Segment],
    separator: Separator,
    width: usize,
    theme: &Theme,
) -> Line {
    if width == 0 {
        return String::new();
    }
    let style = Style::new(separator, theme);
    let mut left: Vec<&Segment> = left.iter().filter(|segment| segment.width > 0).collect();
    let mut right: Vec<&Segment> = right.iter().filter(|segment| segment.width > 0).collect();
    while style.wanted(&left, &right) > width && drop_one(&mut left, &mut right) {}

    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let mut line = style.draw(&left, Side::Left);
    let used = style.width(&left, Side::Left);
    let right_width = style.width(&right, Side::Right);
    let gap = if left.is_empty() { 0 } else { GAP };
    let room = width.saturating_sub(used + gap);
    if right.is_empty() || room < MIN_RIGHT.min(right_width) {
        return text::truncate(&line, width, ellipsis);
    }
    let right = text::truncate(&style.draw(&right, Side::Right), room, ellipsis);
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

/// How full the context window is as a bar of `cells` cells, `[━━━╸────]`: half a cell per
/// step, in the colour of [`context_role`].
pub fn context_bar(percent: u64, cells: u64, theme: &Theme) -> Segment {
    let cells = cells.max(1);
    let halves = ((percent.min(100) * cells * 2 + 50) / 100).min(cells * 2);
    let (full, head) = (halves / 2, halves % 2);
    let run = |glyph: Glyph, count: u64| theme.glyph(glyph).repeat(usize::try_from(count).unwrap_or(0));
    let filled = format!(
        "{}{}",
        run(Glyph::ProgressFull, full),
        run(Glyph::ProgressHead, head)
    );
    Segment::new(Role::BorderMuted, "[")
        .and(context_role(percent).unwrap_or(Role::Muted), filled)
        .and(Role::BorderMuted, run(Glyph::ProgressEmpty, cells - full - head))
        .and(Role::BorderMuted, "]")
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

    fn muted(items: &[&str]) -> Vec<Segment> {
        items
            .iter()
            .map(|text| Segment::new(Role::Muted, *text))
            .collect()
    }

    fn ascii() -> Theme {
        Theme::dark().with_glyphs(GlyphLevel::Ascii)
    }

    fn nerd() -> Theme {
        Theme::dark().with_glyphs(GlyphLevel::Nerd)
    }

    fn shown(
        left: &[Segment],
        right: &[Segment],
        separator: Separator,
        width: usize,
        theme: &Theme,
    ) -> String {
        text::strip_ansi(&render(left, right, separator, width, theme))
    }

    /// A left side and a right side whose segments leave in a known order.
    fn sides() -> (Vec<Segment>, Vec<Segment>) {
        let segment = |text: &str, priority| Segment::new(Role::Muted, text).priority(priority);
        let left = vec![
            segment("ask", 100),
            segment("⑂ main", 85),
            segment("~/src/app", 80),
            segment("◫ 42%", 75),
            segment("$0.42", 55),
            segment("⚑ goal", 45),
        ];
        let right = vec![segment("⬢ mock/model", 95), segment("high", 70)];
        (left, right)
    }

    #[test]
    fn segments_are_joined_by_the_separator_and_empty_ones_are_skipped() {
        let left = muted(&["ask", "", "~/src", "◫ 12%"]);
        let theme = Theme::dark();
        let line = |separator, theme: &Theme| shown(&left, &[], separator, 80, theme);
        assert_eq!(line(Separator::Dot, &theme), "ask · ~/src · ◫ 12%");
        assert_eq!(line(Separator::Bar, &theme), "ask │ ~/src │ ◫ 12%");
        assert_eq!(line(Separator::Space, &theme), "ask  ~/src  ◫ 12%");
        assert_eq!(line(Separator::Dot, &ascii()), "ask | ~/src | ◫ 12%");
        assert_eq!(line(Separator::Bar, &ascii()), "ask | ~/src | ◫ 12%");
        // The separator is quiet, whatever the segments are.
        let painted = render(&left, &[], Separator::Dot, 80, &theme);
        assert!(painted.contains(&theme.paint(Role::Muted, " · ")), "{painted:?}");
        let both = shown(
            &muted(&["ask", "", "~/src"]),
            &muted(&["m"]),
            Separator::Dot,
            20,
            &ascii(),
        );
        assert_eq!(both, "ask | ~/src        m");
    }

    #[test]
    fn the_right_side_sits_against_the_right_edge() {
        let theme = Theme::dark();
        let right = muted(&["⬢ mock/model", "high"]);
        let line = render(&muted(&["ask", "~/src"]), &right, Separator::Dot, 40, &theme);
        assert_eq!(
            text::strip_ansi(&line),
            "ask · ~/src          ⬢ mock/model · high"
        );
        assert_eq!(text::width(&line), 40);
        let alone = shown(&[], &muted(&["⬢ mock/model"]), Separator::Dot, 20, &theme);
        assert_eq!(alone, "        ⬢ mock/model");
    }

    #[test]
    fn the_lowest_priority_leaves_first_and_one_segment_stays_on_each_side() {
        let (left, right) = sides();
        let theme = Theme::dark();
        let at = |width: usize| shown(&left, &right, Separator::Dot, width, &theme);
        // 49 columns of left, a gap of two and 19 of right.
        assert_eq!(
            at(70),
            "ask · ⑂ main · ~/src/app · ◫ 42% · $0.42 · ⚑ goal  ⬢ mock/model · high"
        );
        assert_eq!(
            at(69),
            "ask · ⑂ main · ~/src/app · ◫ 42% · $0.42          ⬢ mock/model · high"
        );
        assert_eq!(
            at(61),
            "ask · ⑂ main · ~/src/app · ◫ 42% · $0.42  ⬢ mock/model · high"
        );
        assert_eq!(
            at(60),
            "ask · ⑂ main · ~/src/app · ◫ 42%         ⬢ mock/model · high"
        );
        // The thinking level matters less than how full the context is.
        assert_eq!(at(52), "ask · ⑂ main · ~/src/app · ◫ 42%        ⬢ mock/model");
        assert_eq!(at(45), "ask · ⑂ main · ~/src/app         ⬢ mock/model");
        assert_eq!(at(37), "ask · ⑂ main             ⬢ mock/model");
        assert_eq!(at(26), "ask · ⑂ main  ⬢ mock/model");
        assert_eq!(at(25), "ask          ⬢ mock/model");
        assert_eq!(at(17), "ask  ⬢ mock/model");
        // One segment is left on each side: now the right side is cut, then dropped, and then
        // the left side is cut.
        assert_eq!(at(16), "ask  ⬢ mock/mod…");
        assert_eq!(at(9), "ask  ⬢ m…");
        assert_eq!(at(8), "ask");
        assert_eq!(at(2), "a…");
        for width in 0..120 {
            let line = render(&left, &right, Separator::Dot, width, &theme);
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
            if width >= 17 {
                assert_eq!(text::width(&line), width);
            }
        }
    }

    #[test]
    fn segments_of_one_priority_leave_from_the_end_of_the_left_side_first() {
        let theme = Theme::dark();
        let left = muted(&["one", "two", "three"]);
        let right = muted(&["four", "five"]);
        let at = |width: usize| shown(&left, &right, Separator::Space, width, &theme);
        assert_eq!(at(27), "one  two  three  four  five");
        assert_eq!(at(26), "one  two        four  five");
        assert_eq!(at(20), "one  two  four  five");
        assert_eq!(at(19), "one      four  five");
        assert_eq!(at(15), "one  four  five");
        // The first of each side is what stays.
        assert_eq!(at(14), "one       four");
        assert_eq!(at(9), "one  four");
        assert_eq!(at(8), "one");
        let alone = |width: usize| shown(&left, &[], Separator::Space, width, &theme);
        assert_eq!(alone(15), "one  two  three");
        assert_eq!(alone(14), "one  two");
        assert_eq!(alone(7), "one");
    }

    #[test]
    fn a_joined_segment_stands_a_space_behind_its_neighbour_and_leaves_with_it() {
        let theme = Theme::dark();
        let bar = context_bar(42, BAR_CELLS, &theme).joined();
        let left = vec![
            Segment::new(Role::Muted, "ask").priority(100),
            Segment::new(Role::Muted, "◫ 42%").priority(60),
            bar.clone().priority(90),
            Segment::new(Role::Muted, "$0.42").priority(70),
        ];
        let at = |width: usize| shown(&left, &[], Separator::Dot, width, &theme);
        assert_eq!(at(30), "ask · ◫ 42% [━━━╸────] · $0.42");
        // The percentage is the first to go, and takes its bar along.
        assert_eq!(at(29), "ask · $0.42");
        // A bar that matters least goes alone.
        let left = vec![
            Segment::new(Role::Muted, "ask").priority(100),
            Segment::new(Role::Muted, "◫ 42%").priority(60),
            bar.priority(5),
        ];
        let at = |width: usize| shown(&left, &[], Separator::Dot, width, &theme);
        assert_eq!(at(22), "ask · ◫ 42% [━━━╸────]");
        assert_eq!(at(21), "ask · ◫ 42%");
    }

    #[test]
    fn powerline_draws_blocks_of_colour_and_arrows_where_the_font_has_them() {
        let left = |theme: &Theme| {
            vec![
                Segment::new(Role::ModePlan, "plan")
                    .filled(Role::ModePlan)
                    .priority(100),
                Segment::new(Role::Muted, "main"),
                Segment::new(Role::Muted, "◫ 42%"),
                context_bar(42, 4, theme).joined(),
            ]
        };
        let right = vec![Segment::new(Role::Accent, "mock/model").filled(Role::Accent)];
        let theme = nerd();
        let line = render(&left(&theme), &right, Separator::Powerline, 60, &theme);
        assert_eq!(
            text::strip_ansi(&line),
            format!(
                " plan \u{e0b0} main \u{e0b0} ◫ 42% [━╸──] \u{e0b0}{}\u{e0b2} mock/model ",
                " ".repeat(18)
            )
        );
        assert_eq!(text::width(&line), 60);
        let (fg, bg) = (
            |role| theme.fg_color(theme.color(role)),
            |role| theme.bg_color(theme.color(role)),
        );
        // The mode fills its block with its colour; the arrow carries it into the next block.
        assert!(line.starts_with(&format!("{} ", bg(Role::ModePlan))), "{line:?}");
        assert!(
            line.contains(&format!("{}{}\u{e0b0}", bg(Role::CodeBg), fg(Role::ModePlan))),
            "{line:?}"
        );
        // The last arrow of the left side ends on the terminal's own background.
        assert!(
            line.contains(&format!("\x1b[49m{}\u{e0b0}", fg(Role::SelectedBg))),
            "{line:?}"
        );
        assert!(
            line.contains(&format!(
                "\x1b[49m{}\u{e0b2}{} ",
                fg(Role::Accent),
                bg(Role::Accent)
            )),
            "{line:?}"
        );
        assert!(line.ends_with(RESET));

        // Without a Nerd Font the blocks stand side by side, told apart by their colours.
        for theme in [Theme::dark(), ascii()] {
            let line = render(&left(&theme), &right, Separator::Powerline, 60, &theme);
            let bar = if theme.glyphs().level() == GlyphLevel::Ascii {
                "[=>--]"
            } else {
                "[━╸──]"
            };
            assert_eq!(
                text::strip_ansi(&line),
                format!(" plan  main  ◫ 42% {bar} {} mock/model ", " ".repeat(22))
            );
            assert!(line.contains(&format!("{} ", theme.bg(Role::CodeBg))), "{line:?}");
        }
    }

    #[test]
    fn text_with_its_own_colours_is_shown_as_it_is() {
        let theme = Theme::dark();
        let left = [Segment::styled("\x1b[32mmain\x1b[0m ~/src")];
        let right = muted(&["⬢ mock/model", "high"]);
        for separator in [Separator::Dot, Separator::Powerline] {
            let line = render(&left, &right[..1], separator, 40, &theme);
            assert!(line.starts_with("\x1b[32mmain\x1b[0m ~/src"), "{line:?}");
            assert_eq!(text::width(&line), 40);
        }
        let at = |width: usize| shown(&left, &right, Separator::Dot, width, &theme);
        assert_eq!(at(31), "main ~/src  ⬢ mock/model · high");
        assert_eq!(at(30), "main ~/src        ⬢ mock/model");
        assert_eq!(at(6), "main …");
        // Colours left open do not run into what follows.
        let open = [Segment::styled("\x1b[32mmain")];
        let line = render(&open, &right, Separator::Dot, 40, &theme);
        assert!(line.starts_with("\x1b[32mmain\x1b[0m "), "{line:?}");
    }

    #[test]
    fn no_line_is_wider_than_the_terminal_whatever_the_separator() {
        let (left, right) = sides();
        let mut left: Vec<Segment> = left;
        left[0] = left[0].clone().filled(Role::ModePlan);
        left.push(context_bar(42, BAR_CELLS, &Theme::dark()).joined().priority(5));
        for level in GlyphLevel::ALL {
            let theme = Theme::dark().with_glyphs(level);
            for separator in [
                Separator::Dot,
                Separator::Bar,
                Separator::Space,
                Separator::Powerline,
            ] {
                for width in 0..=200 {
                    let line = render(&left, &right, separator, width, &theme);
                    assert!(
                        text::width(&line) <= width,
                        "{level} {separator:?} at {width}: {line:?}"
                    );
                    if width >= 30 {
                        assert_eq!(text::width(&line), width, "{level} {separator:?} at {width}");
                    }
                }
            }
        }
    }

    #[test]
    fn the_context_bar_fills_by_half_cells() {
        let theme = Theme::dark();
        let bar = |percent| text::strip_ansi(&context_bar(percent, BAR_CELLS, &theme).paint(&theme));
        assert_eq!(bar(0), "[────────]");
        assert_eq!(bar(3), "[────────]");
        assert_eq!(bar(4), "[╸───────]");
        assert_eq!(bar(12), "[━───────]");
        assert_eq!(bar(42), "[━━━╸────]");
        assert_eq!(bar(50), "[━━━━────]");
        assert_eq!(bar(97), "[━━━━━━━━]");
        assert_eq!(bar(400), "[━━━━━━━━]");
        for percent in 0..=120 {
            assert_eq!(context_bar(percent, BAR_CELLS, &theme).width(), 10, "{percent}%");
        }
        // The filled part warns as the percentage does.
        assert!(
            context_bar(80, 8, &theme)
                .paint(&theme)
                .contains(&theme.fg(Role::Warning))
        );
        assert!(
            context_bar(95, 8, &theme)
                .paint(&theme)
                .contains(&theme.fg(Role::Error))
        );
        let ascii = ascii();
        assert_eq!(
            text::strip_ansi(&context_bar(42, 8, &ascii).paint(&ascii)),
            "[===>----]"
        );
        // Any number of cells, and never none.
        assert_eq!(
            text::strip_ansi(&context_bar(50, 4, &theme).paint(&theme)),
            "[━━──]"
        );
        assert_eq!(context_bar(50, 0, &theme).width(), 3);
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
        let muted = |text: String| Segment::new(Role::Muted, text);
        let left = vec![
            muted("ask".to_string()).priority(100),
            muted(labelled(Glyph::Branch, "main", &theme)),
            muted(shorten_path("~/projects/azazel/crates/azazel-tui", 24, &theme)),
            muted(labelled(Glyph::Context, "42%", &theme)),
            context_bar(42, BAR_CELLS, &theme).joined(),
            muted(rate(78.0, &theme).unwrap()),
            muted(cost(0.42, &theme)),
            muted(labelled(Glyph::Usage, "61% 5h", &theme)),
            muted(labelled(Glyph::Tasks, "2/5", &theme)),
            muted(labelled(Glyph::Dirty, "3", &theme)),
            muted(labelled(Glyph::Account, "work", &theme)),
            muted(labelled(Glyph::Cache, "87%", &theme)),
            muted(labelled(Glyph::Agents, "2 agents", &theme)),
            muted(labelled(Glyph::Goal, "goal active", &theme)),
        ];
        let right = vec![
            muted(labelled(Glyph::Model, "mock/model", &theme)),
            muted(labelled(Glyph::Session, "fix the build", &theme)),
            muted(labelled(Glyph::Clock, "14:05", &theme)),
        ];
        let line = shown(&left, &right, Separator::Dot, 200, &theme);
        assert!(
            line.starts_with(
                "ask | git: main | .../crates/azazel-tui | ctx 42% [===>----] | 78 tok/s | $0.42 | \
                 use 61% 5h | todo 2/5 | +- 3 | as work | cache 87% | 2 agents | goal active  "
            ),
            "{line:?}"
        );
        assert!(line.ends_with("  mock/model | fix the build | 14:05"), "{line:?}");
        for separator in [
            Separator::Dot,
            Separator::Bar,
            Separator::Space,
            Separator::Powerline,
        ] {
            for width in 0..220 {
                let line = shown(&left, &right, separator, width, &theme);
                assert!(line.is_ascii(), "{separator:?} at {width}: {line:?}");
                assert!(text::width(&line) <= width, "{separator:?} at {width}: {line:?}");
            }
        }
    }
}
