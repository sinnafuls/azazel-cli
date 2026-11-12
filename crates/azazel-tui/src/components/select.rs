//! Filterable selection list for pickers and completion menus.

use std::cell::Cell;

use nucleo_matcher::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo_matcher::{Config, Matcher, Utf32Str};

use super::one_line;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// Page size used before the list has been rendered.
const DEFAULT_PAGE: usize = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectItem {
    pub label: String,
    pub detail: String,
}

impl SelectItem {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> SelectItem {
        SelectItem {
            label: label.into(),
            detail: detail.into(),
        }
    }
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
    matcher: Matcher,
}

impl SelectList {
    pub fn new(items: Vec<SelectItem>) -> SelectList {
        let matches = (0..items.len()).collect();
        SelectList {
            items,
            filter: String::new(),
            matches,
            cursor: 0,
            scroll: Cell::new(0),
            page: Cell::new(DEFAULT_PAGE),
            matcher: Matcher::new(Config::DEFAULT),
        }
    }

    pub fn items(&self) -> &[SelectItem] {
        &self.items
    }

    /// Replaces the items, keeping the filter text. The selection returns to the best match.
    pub fn set_items(&mut self, items: Vec<SelectItem>) {
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

    /// Index of the selected item in the original item list.
    pub fn selected(&self) -> Option<usize> {
        self.matches.get(self.cursor).copied()
    }

    pub fn selected_item(&self) -> Option<&SelectItem> {
        self.selected().and_then(|index| self.items.get(index))
    }

    /// Renders at most `max_rows` lines. The selected row is marked `→ ` and filled with the
    /// selection background across the width. When the matches do not fit, the window scrolls
    /// just far enough to keep the selection visible and the last row shows `(position/total)`.
    pub fn render(&self, width: usize, max_rows: usize, theme: &Theme) -> Vec<Line> {
        if max_rows == 0 {
            return Vec::new();
        }
        let total = self.matches.len();
        if total == 0 {
            return vec![text::truncate(
                &theme.paint(Role::Muted, "  no matches"),
                width,
                "",
            )];
        }

        let counter = total > max_rows && max_rows > 1;
        let visible = if counter {
            max_rows - 1
        } else {
            max_rows.min(total)
        };
        let mut scroll = self.scroll.get().min(total - visible);
        if self.cursor < scroll {
            scroll = self.cursor;
        } else if self.cursor >= scroll + visible {
            scroll = self.cursor + 1 - visible;
        }
        self.scroll.set(scroll);
        self.page.set(visible);

        let mut lines = Vec::with_capacity(max_rows.min(total + 1));
        for (position, &index) in self.matches.iter().enumerate().skip(scroll).take(visible) {
            let item = &self.items[index];
            let label = one_line(&item.label);
            let detail = one_line(&item.detail);
            if position == self.cursor {
                let mut line = format!(
                    "{}{}→ {}\x1b[1m{label}\x1b[22m",
                    theme.bg(Role::SelectedBg),
                    theme.fg(Role::Accent),
                    theme.fg(Role::Text),
                );
                if !detail.is_empty() {
                    line.push_str("  ");
                    line.push_str(&theme.fg(Role::Muted));
                    line.push_str(&detail);
                }
                let cut = text::truncate(&line, width, "…");
                let open = cut.strip_suffix(RESET).unwrap_or(&cut);
                lines.push(format!("{}{RESET}", text::pad_right(open, width)));
            } else {
                let mut line = format!("  {label}");
                if !detail.is_empty() {
                    line.push_str("  ");
                    line.push_str(&theme.paint(Role::Muted, &detail));
                }
                lines.push(text::truncate(&line, width, "…"));
            }
        }
        if counter {
            let position = format!("  ({}/{total})", self.cursor + 1);
            lines.push(text::truncate(&theme.paint(Role::Muted, &position), width, ""));
        }
        lines
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
        self.scroll.set(0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

        // Three rows show two items and the position counter, so a page is two items.
        select.render(20, 3, &Theme::dark());
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
    fn renders_marker_detail_and_fits_width() {
        let select = SelectList::new(vec![
            SelectItem::new("/model", "choose the model"),
            SelectItem::new("/mode", "permission mode"),
        ]);
        let theme = Theme::dark();
        let lines = select.render(40, 10, &theme);
        assert_eq!(
            plain(&lines),
            ["→ /model  choose the model", "  /mode  permission mode"]
        );
        // The selected row is a band across the whole width; the others are not.
        assert!(lines[0].starts_with(&theme.bg(Role::SelectedBg)));
        assert_eq!(text::width(&lines[0]), 40);
        assert!(!lines[1].contains(&theme.bg(Role::SelectedBg)));

        let narrow = select.render(12, 10, &theme);
        assert_eq!(plain(&narrow), ["→ /model  c…", "  /mode  pe…"]);
        for width in 0..30 {
            for line in select.render(width, 10, &theme) {
                assert!(text::width(&line) <= width, "width {width}: {line:?}");
            }
        }
    }

    #[test]
    fn scroll_window_keeps_the_selection_visible() {
        let labels: Vec<String> = (0..20).map(|i| format!("item-{i:02}")).collect();
        let refs: Vec<&str> = labels.iter().map(String::as_str).collect();
        let mut select = list(&refs);

        let lines = plain(&select.render(30, 5, &Theme::dark()));
        assert_eq!(
            lines,
            ["→ item-00", "  item-01", "  item-02", "  item-03", "  (1/20)"]
        );

        for _ in 0..6 {
            select.move_down();
        }
        let lines = plain(&select.render(30, 5, &Theme::dark()));
        assert_eq!(
            lines,
            ["  item-03", "  item-04", "  item-05", "→ item-06", "  (7/20)"]
        );

        // Moving back up scrolls only when the selection would leave the window.
        select.move_up();
        let lines = plain(&select.render(30, 5, &Theme::dark()));
        assert_eq!(
            lines,
            ["  item-03", "  item-04", "→ item-05", "  item-06", "  (6/20)"]
        );
        select.page_up();
        select.page_up();
        let lines = plain(&select.render(30, 5, &Theme::dark()));
        assert_eq!(
            lines,
            ["→ item-00", "  item-01", "  item-02", "  item-03", "  (1/20)"]
        );

        select.move_up();
        let lines = plain(&select.render(30, 5, &Theme::dark()));
        assert_eq!(
            lines,
            ["  item-16", "  item-17", "  item-18", "→ item-19", "  (20/20)"]
        );
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
}
