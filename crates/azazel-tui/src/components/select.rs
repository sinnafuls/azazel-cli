//! Filterable selection list for pickers and completion menus.

use std::cell::Cell;

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::one_line;
use super::overlay::Scroll;
use crate::glyphs::Glyph;
use crate::mouse::MouseKind;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// Page size used before the list has been rendered.
const DEFAULT_PAGE: usize = 10;
/// The widest the label column gets before details stop lining up.
const PRIMARY_WIDTH: usize = 32;
/// Columns between the label column and the details.
const GAP: usize = 2;
/// A detail that would get fewer columns than this (and does not fit whole) is left out.
const MIN_DETAIL: usize = 10;
/// Columns before the label: the rail of the selected row and a space.
const RAIL: usize = 2;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub label: String,
    pub detail: String,
    /// A check box before the label, ticked or not: one option of several that may be picked.
    pub mark: Option<bool>,
    /// The detail carries its own colours and is shown as it is. It must come from the UI
    /// itself and must not reset styles.
    pub styled: bool,
}

impl SelectItem {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            detail: detail.into(),
            mark: None,
            styled: false,
        }
    }

    /// The item with a check box before its label.
    pub fn checked(mut self, on: bool) -> SelectItem {
        self.mark = Some(on);
        self
    }

    /// An item whose detail is a preview drawn in colours of its own (swatches of a theme). The
    /// detail is shown verbatim: it must not contain a reset.
    pub fn with_preview(label: impl Into<String>, preview: impl Into<String>) -> SelectItem {
        SelectItem {
            styled: true,
            ..SelectItem::new(label, preview)
        }
    }
}

/// What the mouse did to a list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListMouse {
    /// Nothing to act on: the pointer moved, the wheel turned, or it was not over a row.
    Nothing,
    /// A press selected a row that was not selected: the index of its item.
    Selected(usize),
    /// A press on the row that was already selected: what `enter` would pick.
    Again(usize),
}

/// The rows of a list in view, and where they are in the whole list when it does not fit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListView {
    pub lines: Vec<Line>,
    pub scroll: Option<Scroll>,
}

pub struct SelectList {
    items: Vec<SelectItem>,
    filter: String,
    /// Indices into `items` of the entries that pass the filter, best match first.
    matches: Vec<usize>,
    /// Position of the selection within `matches`.
    cursor: usize,
    /// First visible position; adjusted while rendering so the selection stays in view.
    scroll: Cell<usize>,
    /// Rows of items shown by the last render: the distance of a page movement.
    page: Cell<usize>,
    /// The position within `matches` the mouse is over, drawn as a band of its own.
    hover: Option<usize>,
    /// Width of the label column: the widest label that has a detail and is no wider than
    /// [`PRIMARY_WIDTH`]. `None` when no such label exists.
    primary: Option<usize>,
    matcher: Matcher,
}

/// The label column for `items`. Labels wider than [`PRIMARY_WIDTH`] do not widen it: each of
/// those pushes its own detail to the right instead.
fn primary_width(items: &[SelectItem]) -> Option<usize> {
    items
        .iter()
        .filter(|item| !item.detail.is_empty())
        .map(|item| text::width(&item.label))
        .filter(|width| *width <= PRIMARY_WIDTH)
        .max()
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>) -> SelectList {
        let matches = (0..items.len()).collect();
        SelectList {
            primary: primary_width(&items),
            items,
            filter: String::new(),
            matches,
            cursor: 0,
            scroll: Cell::new(0),
            page: Cell::new(DEFAULT_PAGE),
            hover: None,
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Replaces the items, keeping the filter text. The selection returns to the best match.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
        self.primary = primary_width(&items);
        self.items = items;
        self.refilter();
    }

    pub fn filter(&self) -> &str {
        &self.filter
    }

    /// Keeps the items whose label fuzzy-matches `filter`, best match first (ties keep their
    /// original order), and selects the first. An empty filter shows every item in order.
    /// Matching ignores case unless the filter contains an upper-case letter.
    pub fn set_filter(&mut self, filter: &str) {
        self.filter = filter.to_string();
        self.refilter();
    }

    /// Number of items that pass the filter.
    pub fn len(&self) -> usize {
        self.matches.len()
    }

    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Selects the row at `index` among those shown; out of range leaves the selection as it is.
    pub fn set_cursor(&mut self, index: usize) {
        if index < self.matches.len() {
            self.cursor = index;
        }
    }

    /// Moves the selection up one row, wrapping to the bottom.
    pub fn move_up(&mut self) {
        if !self.matches.is_empty() {
            self.cursor = self.cursor.checked_sub(1).unwrap_or(self.matches.len() - 1);
        }
    }

    /// Moves the selection down one row, wrapping to the top.
    pub fn move_down(&mut self) {
        if !self.matches.is_empty() {
            self.cursor = (self.cursor + 1) % self.matches.len();
        }
    }

    /// Moves the selection up by one page (the number of items the last render showed), stopping
    /// at the first item.
    pub fn page_up(&mut self) {
        self.cursor = self.cursor.saturating_sub(self.page.get().max(1));
    }

    /// Moves the selection down by one page, stopping at the last item.
    pub fn page_down(&mut self) {
        let last = self.matches.len().saturating_sub(1);
        self.cursor = self.cursor.saturating_add(self.page.get().max(1)).min(last);
    }

    /// The position among the rows shown (`0` the first match) of the row `row` rows below the
    /// top of the last [`SelectList::view`], or `None` when no row of the list is there.
    pub fn row_at(&self, row: usize) -> Option<usize> {
        let visible = self.page.get().min(self.matches.len());
        (row < visible).then(|| self.scroll.get() + row)
    }

    /// The mouse over the list: `row` is the row of the last [`SelectList::view`] it is on,
    /// `None` when it is elsewhere. Moving over a row marks it; a press selects a row, and a
    /// press on the selected row is [`ListMouse::Again`]; the wheel moves the selection one row,
    /// stopping at the ends.
    pub fn on_mouse(&mut self, kind: MouseKind, row: Option<usize>) -> ListMouse {
        let at = row.and_then(|row| self.row_at(row));
        match kind {
            MouseKind::Move => self.hover = at,
            MouseKind::ScrollUp => self.cursor = self.cursor.saturating_sub(1),
            MouseKind::ScrollDown => {
                self.cursor = (self.cursor + 1).min(self.matches.len().saturating_sub(1));
            }
            MouseKind::Press | MouseKind::Drag => {
                let Some(at) = at else {
                    return ListMouse::Nothing;
                };
                let again = at == self.cursor && kind == MouseKind::Press;
                self.cursor = at;
                let index = self.matches[at];
                return match again {
                    true => ListMouse::Again(index),
                    false if kind == MouseKind::Press => ListMouse::Selected(index),
                    false => ListMouse::Nothing,
                };
            }
        }
        ListMouse::Nothing
    }

    /// Forgets the row the mouse was over: the keys moved the selection.
    pub fn clear_hover(&mut self) {
        self.hover = None;
    }

    /// Index of the selected item in the original item list.
    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.selected().and_then(|index| self.items.get(index))
    }

    /// Renders at most `max_rows` rows, each `width` columns or less. The first column is kept
    /// for the rail that marks the selected row, which is also filled with the selection
    /// background across the width and has its label in bold. Details line up in a column of
    /// their own and are cut, never wrapped. When the matches do not fit, the window scrolls
    /// just far enough to keep the selection visible and `scroll` says where it is.
    pub fn view(&self, width: usize, max_rows: usize, theme: &Theme) -> ListView {
        let total = self.matches.len();
        if max_rows == 0 {
            return ListView {
                lines: Vec::new(),
                scroll: None,
            };
        }
        if total == 0 {
            let none = theme.paint(Role::Muted, "  no matches");
            return ListView {
                lines: vec![text::truncate(&none, width, "")],
                scroll: None,
            };
        }

        let visible = max_rows.min(total);
        let mut scroll = self.scroll.get().min(total - visible);
        if self.cursor < scroll {
            scroll = self.cursor;
        } else if self.cursor >= scroll + visible {
            scroll = self.cursor + 1 - visible;
        }
        self.scroll.set(scroll);
        self.page.set(visible);

        let lines = self
            .matches
            .iter()
            .enumerate()
            .skip(scroll)
            .take(visible)
            .map(|(position, &index)| {
                let state = match position {
                    position if position == self.cursor => RowState::Selected,
                    position if Some(position) == self.hover => RowState::Hovered,
                    _ => RowState::Plain,
                };
                self.row(&self.items[index], state, width, theme)
            })
            .collect();
        ListView {
            lines,
            scroll: (total > visible).then_some(Scroll {
                offset: scroll,
                visible,
                total,
            }),
        }
    }

    /// The list on its own, outside a frame: the rows of [`SelectList::view`], with a scrollbar
    /// in the last column when the matches do not fit.
    pub fn render(&self, width: usize, max_rows: usize, theme: &Theme) -> Vec<Line> {
        if self.matches.len() <= max_rows || width < RAIL + 2 {
            return self.view(width, max_rows, theme).lines;
        }
        let view = self.view(width - 1, max_rows, theme);
        let cells = view.scroll.map(|scroll| scroll.cells(theme)).unwrap_or_default();
        view.lines
            .into_iter()
            .zip(cells)
            .map(|(line, cell)| format!("{}{cell}{RESET}", text::pad_right(&line, width - 1)))
            .collect()
    }

    /// The row a filter is typed into: the search symbol and the text on the left, the number
    /// of matches out of all items on the right. Also returns the column of the typing cursor.
    pub fn filter_row(&self, width: usize, theme: &Theme) -> (Line, u16) {
        let icon = theme.glyphs().prefix(Glyph::Search);
        let count = format!("{}/{}", self.matches.len(), self.items.len());
        let lead = text::width(&icon);
        // The count gives way before the text does; a long text shows its end.
        let count_room = text::width(&count) + 1;
        let with_count = width >= lead + count_room + 4;
        let room = width
            .saturating_sub(lead + if with_count { count_room } else { 0 })
            .saturating_sub(1);
        let typed = one_line(&self.filter);
        let mut shown = typed.as_str();
        while text::width(shown) > room {
            let mut chars = shown.chars();
            chars.next();
            shown = chars.as_str();
        }
        let column = (lead + text::width(shown)).min(width.saturating_sub(1));
        let mut line = format!("{}{shown}", theme.paint(Role::Accent, &icon));
        if with_count {
            let used = lead + text::width(shown);
            line.push_str(&" ".repeat(width - used - text::width(&count)));
            line.push_str(&theme.paint(Role::Muted, &count));
        }
        (text::truncate(&line, width, ""), column as u16)
    }

    fn row(&self, item: &SelectItem, state: RowState, width: usize, theme: &Theme) -> Line {
        let ellipsis = theme.glyph(Glyph::Ellipsis);
        let mark = match item.mark {
            Some(true) => theme.glyphs().prefix(Glyph::CheckOn),
            Some(false) => theme.glyphs().prefix(Glyph::CheckOff),
            None => String::new(),
        };
        let room = width.saturating_sub(RAIL + text::width(&mark));
        let label = one_line(&item.label);
        let detail = if item.styled {
            item.detail.clone()
        } else {
            one_line(&item.detail)
        };
        let label_width = text::width(&label);
        let detail_width = text::width(&detail);

        // A label longer than the column keeps what it needs as long as its detail still gets
        // the least a detail is worth showing in.
        let cap = room.saturating_sub(GAP + MIN_DETAIL);
        let column = self
            .primary
            .unwrap_or(PRIMARY_WIDTH.min(cap))
            .max(label_width.min(cap));
        let detail_room = room.saturating_sub(column + GAP);
        let fits = if item.styled {
            detail_width <= detail_room
        } else {
            detail_room >= MIN_DETAIL.min(detail_width)
        };
        let (label, detail) = if detail.is_empty() || !fits || detail_room == 0 {
            (text::truncate(&label, room, ellipsis), String::new())
        } else {
            let label = text::pad_right(&text::truncate(&label, column, ellipsis), column + GAP);
            let detail = if item.styled {
                detail
            } else {
                text::truncate(&detail, detail_room, ellipsis)
            };
            (label, detail)
        };

        if state == RowState::Selected {
            // One band across the row: nothing in it may reset the background.
            let mut line = format!(
                "{}{}{} {}{mark}\x1b[1m{label}\x1b[22m",
                theme.bg(Role::SelectedBg),
                theme.fg(Role::Accent),
                theme.glyph(Glyph::Rail),
                theme.fg(Role::Text),
            );
            if !detail.is_empty() {
                line.push_str(&theme.fg(Role::Muted));
                line.push_str(&detail);
            }
            let cut = text::truncate(&line, width, "");
            let open = cut.strip_suffix(RESET).unwrap_or(&cut);
            format!("{}{RESET}", text::pad_right(open, width))
        } else if state == RowState::Hovered {
            // A quieter band than the selection's, with no rail: the mouse is over the row.
            let mut line = format!("{}{}{mark}{label}", theme.bg(Role::UserBg), " ".repeat(RAIL));
            if !detail.is_empty() {
                line.push_str(&theme.fg(Role::Muted));
                line.push_str(&detail);
            }
            let cut = text::truncate(&line, width, "");
            let open = cut.strip_suffix(RESET).unwrap_or(&cut);
            format!("{}{RESET}", text::pad_right(open, width))
        } else {
            let mut line = format!("{}{mark}{label}", " ".repeat(RAIL));
            if !detail.is_empty() {
                line.push_str(&theme.fg(Role::Muted));
                line.push_str(&detail);
                line.push_str(RESET);
            }
            text::truncate(&line, width, "")
        }
    }

    fn refilter(&mut self) {
        if self.filter.trim().is_empty() {
            self.matches = (0..self.items.len()).collect();
        } else {
            let pattern = Pattern::new(
                &self.filter,
                CaseMatching::Smart,
                Normalization::Smart,
                AtomKind::Fuzzy,
            );
            let mut buf = Vec::new();
            let mut scored: Vec<(u32, usize)> = Vec::new();
            for (index, item) in self.items.iter().enumerate() {
                let haystack = Utf32Str::new(&item.label, &mut buf);
                if let Some(score) = pattern.score(haystack, &mut self.matcher) {
                    scored.push((score, index));
                }
            }
            scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
            self.matches = scored.into_iter().map(|(_, index)| index).collect();
        }
        self.cursor = 0;
        self.hover = None;
        self.scroll.set(0);
    }
}

/// How a row of a list is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowState {
    Plain,
    Selected,
    Hovered,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn list(labels: &[&str]) -> SelectList {
        SelectList::new(labels.iter().map(|label| SelectItem::new(*label, "")).collect())
    }

    /// The rows as text, without the padding that fills the selected row.
    fn plain(lines: &[Line]) -> Vec<String> {
        lines
            .iter()
            .map(|l| text::strip_ansi(l).trim_end().to_string())
            .collect()
    }

    #[test]
    fn unfiltered_list_keeps_original_order() {
        let mut select = list(&["beta", "alpha", "gamma"]);
        assert_eq!(select.len(), 3);
        assert_eq!(select.selected(), Some(0));
        select.move_down();
        assert_eq!(select.selected(), Some(1));
        assert_eq!(
            select.selected_item().map(|item| item.label.as_str()),
            Some("alpha")
        );
    }

    #[test]
    fn filter_ranks_matches_and_maps_back_to_original_indices() {
        let mut select = list(&["domain_model.rs", "README.md", "main.rs"]);
        select.set_filter("main");
        // "main.rs" matches at the start of the label, "domain_model.rs" only inside a word, and
        // "README.md" not at all.
        assert_eq!(select.len(), 2);
        assert_eq!(select.selected(), Some(2));
        select.move_down();
        assert_eq!(select.selected(), Some(0));
        select.move_down();
        assert_eq!(select.selected(), Some(2), "moving past the end wraps");

        select.set_filter("");
        assert_eq!(select.len(), 3);
        assert_eq!(select.selected(), Some(0));
    }

    #[test]
    fn filter_is_fuzzy_and_smart_case() {
        let mut select = list(&["src/components/markdown.rs", "src/render.rs", "Cargo.toml"]);
        select.set_filter("cmpmd");
        assert_eq!(select.selected(), Some(0));
        assert_eq!(select.len(), 1);

        select.set_filter("cargo");
        assert_eq!(select.selected(), Some(2), "lower-case filters ignore case");
        select.set_filter("CARGO");
        assert_eq!(select.selected(), None, "upper-case filters are case-sensitive");
        assert!(select.is_empty());
        select.move_down();
        select.page_down();
        assert_eq!(select.selected(), None);
    }

    #[test]
    fn filter_characters_are_literal() {
        let mut select = list(&["!important", "plain"]);
        select.set_filter("!imp");
        assert_eq!(select.selected(), Some(0));
        assert_eq!(select.len(), 1);
    }

    #[test]
    fn replacing_items_reapplies_the_filter() {
        let mut select = list(&["one", "two"]);
        select.set_filter("tw");
        assert_eq!(select.selected(), Some(1));
        select.set_items(vec![
            SelectItem::new("twelve", ""),
            SelectItem::new("three", ""),
            SelectItem::new("two", ""),
        ]);
        assert_eq!(select.filter(), "tw");
        assert_eq!(select.len(), 2);
        assert!(matches!(select.selected(), Some(0 | 2)));
    }

    #[test]
    fn navigation_wraps_and_pages_clamp() {
        let mut select = list(&["a", "b", "c", "d", "e"]);
        select.move_up();
        assert_eq!(select.selected(), Some(4));
        select.move_down();
        assert_eq!(select.selected(), Some(0));

        // Two rows show two items, so a page is two items.
        select.render(20, 2, &Theme::dark());
        select.page_down();
        assert_eq!(select.selected(), Some(2));
        select.page_down();
        assert_eq!(select.selected(), Some(4));
        select.page_down();
        assert_eq!(select.selected(), Some(4));
        select.page_up();
        assert_eq!(select.selected(), Some(2));
        select.page_up();
        assert_eq!(select.selected(), Some(0));
        select.page_up();
        assert_eq!(select.selected(), Some(0));

        // Before the first render a page is longer than this list.
        let mut fresh = list(&["a", "b", "c", "d", "e"]);
        fresh.page_down();
        assert_eq!(fresh.selected(), Some(4));
    }

    #[test]
    fn the_selected_row_has_the_rail_a_band_and_a_bold_label() {
        let select = SelectList::new(vec![
            SelectItem::new("/model", "choose the model"),
            SelectItem::new("/mode", "permission mode"),
        ]);
        let theme = Theme::dark();
        let lines = select.render(40, 10, &theme);
        assert_eq!(
            plain(&lines),
            ["▌ /model  choose the model", "  /mode   permission mode"]
        );
        // The selected row is a band across the whole width; the others are not.
        assert!(lines[0].starts_with(&theme.bg(Role::SelectedBg)));
        assert!(lines[0].contains("\x1b[1m/model"));
        assert_eq!(text::width(&lines[0]), 40);
        assert_eq!(lines[0].matches(RESET).count(), 1, "{:?}", lines[0]);
        assert!(!lines[1].contains(&theme.bg(Role::SelectedBg)));
        assert!(!lines[1].contains("\x1b[1m"));

        // The rail column is kept on every row: moving the selection moves no text.
        let mut moved = SelectList::new(select.items().to_vec());
        moved.move_down();
        assert_eq!(
            plain(&moved.render(40, 10, &theme)),
            ["  /model  choose the model", "▌ /mode   permission mode"]
        );
    }

    #[test]
    fn details_line_up_in_a_column_and_are_cut_or_dropped_never_wrapped() {
        let select = SelectList::new(vec![
            SelectItem::new("anthropic/claude-opus", "ctx 200k · $15/$75"),
            SelectItem::new("openai/gpt", "ctx 400k · $1.25/$10"),
            SelectItem::new(
                "a label far longer than the column details start in",
                "its detail",
            ),
            SelectItem::new("no detail at all, so the label may use the whole row", ""),
        ]);
        let theme = Theme::dark();
        assert_eq!(
            plain(&select.render(70, 10, &theme)),
            [
                "▌ anthropic/claude-opus  ctx 200k · $15/$75",
                "  openai/gpt             ctx 400k · $1.25/$10",
                "  a label far longer than the column details start in  its detail",
                "  no detail at all, so the label may use the whole row",
            ]
        );
        // A long label gives way to its detail down to the label column; details are cut.
        assert_eq!(
            plain(&select.render(40, 10, &theme)),
            [
                "▌ anthropic/claude-opus  ctx 200k · $15…",
                "  openai/gpt             ctx 200k · $1.…"
                    .replace("200k", "400k")
                    .as_str(),
                "  a label far longer than t…  its detail",
                "  no detail at all, so the label may us…",
            ]
        );
        // Too narrow for a detail worth reading: labels only.
        assert_eq!(
            plain(&select.render(30, 10, &theme)),
            [
                "▌ anthropic/claude-opus",
                "  openai/gpt",
                "  a label far longer than the…",
                "  no detail at all, so the la…",
            ]
        );
        for width in 0..=160 {
            for line in select.render(width, 10, &theme) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn check_boxes_and_previews_are_drawn_before_and_after_the_label() {
        let theme = Theme::dark();
        let select = SelectList::new(vec![
            SelectItem::new("tests", "").checked(true),
            SelectItem::new("docs", "").checked(false),
        ]);
        assert_eq!(plain(&select.render(30, 10, &theme)), ["▌ ▣ tests", "  □ docs"]);

        let swatch = format!("{}●{}●", theme.fg(Role::Success), theme.fg(Role::Error));
        let mut previews = SelectList::new(vec![
            SelectItem::with_preview("dark", swatch.clone()),
            SelectItem::with_preview("light", swatch.clone()),
        ]);
        let lines = previews.render(30, 10, &theme);
        assert_eq!(plain(&lines), ["▌ dark   ●●", "  light  ●●"]);
        // The colours of a preview reach the screen, and the band of the selected row holds.
        assert!(lines.iter().all(|line| line.contains(&swatch)));
        assert_eq!(lines[0].matches(RESET).count(), 1);
        assert_eq!(text::width(&lines[0]), 30);
        // A preview is never cut in the middle of its colours: it is shown whole or not at all.
        assert_eq!(plain(&previews.render(10, 10, &theme)), ["▌ dark", "  light"]);
        previews.move_down();
        assert_eq!(
            plain(&previews.render(30, 10, &theme)),
            ["  dark   ●●", "▌ light  ●●"]
        );
    }

    #[test]
    fn the_symbols_follow_the_glyph_level() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let labels: Vec<String> = (0..9).map(|i| format!("a rather long item number {i}")).collect();
        let mut select = SelectList::new(
            labels
                .iter()
                .map(|label| SelectItem::new(label.as_str(), "with a detail").checked(true))
                .collect(),
        );
        select.set_filter("item");
        for width in 0..=80 {
            let (filter, _) = select.filter_row(width, &theme);
            for line in select.render(width, 4, &theme).iter().chain([&filter]) {
                let shown = text::strip_ansi(line);
                assert!(shown.is_ascii(), "width {width}: {shown:?}");
                assert!(text::width(line) <= width, "width {width}: {shown:?}");
            }
        }
        assert_eq!(
            plain(&select.render(30, 3, &theme)),
            [
                "> [x] a rather long item n...#",
                "  [x] a rather long item n...|",
                "  [x] a rather long item n...|",
            ]
        );
    }

    #[test]
    fn scroll_window_keeps_the_selection_visible() {
        let labels: Vec<String> = (0..20).map(|i| format!("item-{i:02}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = list(&refs);
        let theme = Theme::dark();
        let window = |select: &SelectList| {
            let view = select.view(30, 4, &theme);
            (plain(&view.lines), view.scroll.map(|scroll| scroll.offset))
        };

        assert_eq!(
            window(&select),
            (
                vec![
                    "▌ item-00".to_string(),
                    "  item-01".to_string(),
                    "  item-02".to_string(),
                    "  item-03".to_string()
                ],
                Some(0)
            )
        );

        for _ in 0..6 {
            select.move_down();
        }
        let (rows, offset) = window(&select);
        assert_eq!(rows, ["  item-03", "  item-04", "  item-05", "▌ item-06"]);
        assert_eq!(offset, Some(3));

        // Moving back up scrolls only when the selection would leave the window.
        select.move_up();
        let (rows, offset) = window(&select);
        assert_eq!(rows, ["  item-03", "  item-04", "▌ item-05", "  item-06"]);
        assert_eq!(offset, Some(3));
        select.page_up();
        select.page_up();
        assert_eq!(window(&select).1, Some(0));

        select.move_up();
        let (rows, offset) = window(&select);
        assert_eq!(rows, ["  item-16", "  item-17", "  item-18", "▌ item-19"]);
        assert_eq!(offset, Some(16));
        assert_eq!(
            select.view(30, 4, &theme).scroll,
            Some(Scroll {
                offset: 16,
                visible: 4,
                total: 20
            })
        );
        // Everything in view: nothing to scroll.
        assert_eq!(select.view(30, 20, &theme).scroll, None);
    }

    #[test]
    fn a_list_on_its_own_shows_a_scrollbar_in_its_last_column() {
        let labels: Vec<String> = (0..8).map(|i| format!("item-{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = list(&refs);
        let theme = Theme::dark();
        for width in 4..=60 {
            let lines = select.render(width, 4, &theme);
            assert_eq!(lines.len(), 4);
            for line in &lines {
                assert_eq!(text::width(line), width, "width {width}: {line:?}");
            }
        }
        assert_eq!(
            plain(&select.render(12, 4, &theme)),
            ["▌ item-0   ┃", "  item-1   ┃", "  item-2   │", "  item-3   │"]
        );
        for _ in 0..7 {
            select.move_down();
        }
        assert_eq!(
            plain(&select.render(12, 4, &theme)),
            ["  item-4   │", "  item-5   │", "  item-6   ┃", "▌ item-7   ┃"]
        );
        // The list fits: no bar, and rows are not padded.
        assert_eq!(plain(&select.render(12, 8, &theme))[1], "  item-1");
    }

    #[test]
    fn the_filter_row_shows_the_text_and_how_many_items_match() {
        let labels: Vec<String> = (0..340).map(|i| format!("model-{i}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = list(&refs);
        let theme = Theme::dark();
        let (line, column) = select.filter_row(30, &theme);
        assert_eq!(text::strip_ansi(&line), format!("⌕ {}340/340", " ".repeat(21)));
        assert_eq!(column, 2);

        select.set_filter("model-33");
        let (line, column) = select.filter_row(30, &theme);
        assert_eq!(
            text::strip_ansi(&line),
            format!("⌕ model-33{}16/340", " ".repeat(14))
        );
        assert_eq!(column, 10);

        // A text longer than the row shows its end, where the cursor is.
        select.set_filter("a filter that is much longer than the row");
        let (line, column) = select.filter_row(30, &theme);
        assert_eq!(text::strip_ansi(&line), "⌕ h longer than the row  0/340");
        assert_eq!(column, 23);
        for width in 0..=160 {
            let (line, column) = select.filter_row(width, &theme);
            assert!(text::width(&line) <= width, "width {width}: {line:?}");
            assert!(usize::from(column) < width.max(1), "width {width}");
        }
    }

    #[test]
    fn row_limits_are_respected() {
        let select = list(&["a", "b", "c"]);
        assert!(select.render(20, 0, &Theme::dark()).is_empty());
        assert_eq!(select.render(20, 1, &Theme::dark()).len(), 1);
        assert_eq!(select.render(20, 2, &Theme::dark()).len(), 2);
        assert_eq!(select.render(20, 3, &Theme::dark()).len(), 3);
        assert_eq!(select.render(20, 9, &Theme::dark()).len(), 3);
    }

    #[test]
    fn empty_result_says_so() {
        let mut select = list(&["a"]);
        select.set_filter("zzz");
        assert_eq!(plain(&select.render(20, 5, &Theme::dark())), ["  no matches"]);
    }

    #[test]
    fn rows_under_the_mouse_follow_the_scroll() {
        let labels: Vec<String> = (0..20).map(|n| format!("item {n}")).collect();
        let labels: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = list(&labels);
        let theme = Theme::dark();
        select.view(30, 5, &theme);
        assert_eq!(
            (select.row_at(0), select.row_at(4), select.row_at(5)),
            (Some(0), Some(4), None)
        );
        select.set_cursor(12);
        select.view(30, 5, &theme);
        // Rows 8..=12 are in view.
        assert_eq!(
            (select.row_at(0), select.row_at(4), select.row_at(5)),
            (Some(8), Some(12), None)
        );
        select.set_filter("item 1");
        select.view(30, 5, &theme);
        assert_eq!(select.row_at(0), Some(0));
        select.set_filter("zzz");
        select.view(30, 5, &theme);
        assert_eq!(select.row_at(0), None);
    }

    #[test]
    fn a_press_selects_a_second_press_picks_and_the_wheel_moves() {
        let mut select = list(&["alpha", "beta", "gamma", "delta"]);
        let theme = Theme::dark();
        select.view(30, 3, &theme);
        assert_eq!(select.on_mouse(MouseKind::Press, Some(1)), ListMouse::Selected(1));
        assert_eq!(select.selected(), Some(1));
        assert_eq!(select.on_mouse(MouseKind::Press, Some(1)), ListMouse::Again(1));
        assert_eq!(select.on_mouse(MouseKind::Press, None), ListMouse::Nothing);
        assert_eq!(
            select.selected(),
            Some(1),
            "a press beside the rows changes nothing"
        );
        // Dragging over the rows carries the selection without picking.
        assert_eq!(select.on_mouse(MouseKind::Drag, Some(2)), ListMouse::Nothing);
        assert_eq!(select.selected(), Some(2));

        select.on_mouse(MouseKind::ScrollDown, None);
        assert_eq!(select.selected(), Some(3));
        select.on_mouse(MouseKind::ScrollDown, Some(0));
        assert_eq!(select.selected(), Some(3), "the wheel stops at the end");
        select.view(30, 3, &theme);
        // The view scrolled to keep the selection: its first row is now `beta`.
        assert_eq!(select.on_mouse(MouseKind::Press, Some(0)), ListMouse::Selected(1));
        for _ in 0..5 {
            select.on_mouse(MouseKind::ScrollUp, None);
        }
        assert_eq!(select.selected(), Some(0), "nor does it wrap at the top");
    }

    #[test]
    fn the_row_under_the_mouse_is_set_apart() {
        let mut select = SelectList::new(vec![
            SelectItem::new("alpha", "first"),
            SelectItem::new("beta", "second"),
        ]);
        let theme = Theme::dark();
        let band = theme.bg(Role::UserBg);
        let before = plain(&select.view(30, 2, &theme).lines);
        select.on_mouse(MouseKind::Move, Some(1));
        let lines = select.view(30, 2, &theme).lines;
        assert!(!lines[0].contains(&band) && lines[1].contains(&band));
        assert_eq!(text::width(&lines[1]), 30, "the band runs across the row");
        assert_eq!(plain(&lines), before, "the text stays where it was");
        select.on_mouse(MouseKind::Move, None);
        assert!(!select.view(30, 2, &theme).lines[1].contains(&band));
        select.on_mouse(MouseKind::Move, Some(1));
        select.clear_hover();
        assert!(!select.view(30, 2, &theme).lines[1].contains(&band));
    }
}
