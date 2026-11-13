//! A row of tabs: the names of the pages of a screen, with the one in view set apart.

use crate::glyphs::Glyph;
use crate::render::Line;
use crate::text;
use crate::theme::{RESET, Role, Theme};

/// Columns between two tabs.
const GAP: usize = 2;

/// One tab: its name and, when there is something to count on its page, the count.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tab {
    pub label: String,
    /// Shown after the label (`Appearance 2`); empty shows nothing.
    pub badge: String,
}

impl Tab {
    pub fn new(label: impl Into<String>, badge: impl Into<String>) -> Tab {
        Tab {
            label: label.into(),
            badge: badge.into(),
        }
    }

    fn width(&self) -> usize {
        let badge = text::width(&self.badge);
        text::width(&self.label) + if badge == 0 { 0 } else { badge + 1 }
    }
}

/// The tabs that fit `width` columns around the one in view, as `(first, end)`: as many as fit
/// after it, then as many as fit before it. A side with tabs left out keeps room for the mark
/// that says so.
fn window(tabs: &[Tab], current: usize, width: usize, more: usize) -> (usize, usize) {
    let fits = |first: usize, end: usize| {
        let names: usize = tabs[first..end].iter().map(Tab::width).sum();
        let gaps = GAP * (end - first).saturating_sub(1);
        let left = if first > 0 { more + 1 } else { 0 };
        let right = if end < tabs.len() { more + 1 } else { 0 };
        names + gaps + left + right <= width
    };
    let (mut first, mut end) = (current, current + 1);
    loop {
        let grown = if end < tabs.len() && fits(first, end + 1) {
            end += 1;
            true
        } else if first > 0 && fits(first - 1, end) {
            first -= 1;
            true
        } else {
            false
        };
        if !grown {
            return (first, end);
        }
    }
}

/// The tabs as one row of at most `width` columns. The tab in view is bold and underlined in
/// the accent colour, the others are muted; counts are in the accent colour. When the tabs do not fit, the row shows those around the one in view and an ellipsis
/// on each side that has more.
pub fn row(tabs: &[Tab], current: usize, width: usize, theme: &Theme) -> Line {
    if tabs.is_empty() || width == 0 {
        return String::new();
    }
    let current = current.min(tabs.len() - 1);
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let (first, end) = window(tabs, current, width, text::width(ellipsis));
    let mut line = String::new();
    if first > 0 {
        line.push_str(&theme.paint(Role::Muted, ellipsis));
        line.push(' ');
    }
    for (at, tab) in tabs.iter().enumerate().take(end).skip(first) {
        if at > first {
            line.push_str(&" ".repeat(GAP));
        }
        let label = text::sanitize(&tab.label);
        match at == current {
            true => line.push_str(&format!(
                "\x1b[1;4m{}{label}\x1b[22;24m{RESET}",
                theme.fg(Role::Accent)
            )),
            false => line.push_str(&theme.paint(Role::Muted, &label)),
        }
        if !tab.badge.is_empty() {
            line.push(' ');
            line.push_str(&theme.paint(Role::Accent, &text::sanitize(&tab.badge)));
        }
    }
    if end < tabs.len() {
        line.push(' ');
        line.push_str(&theme.paint(Role::Muted, ellipsis));
    }
    text::truncate(&line, width, ellipsis)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::glyphs::GlyphLevel;

    fn tabs(names: &[&str]) -> Vec<Tab> {
        names.iter().map(|name| Tab::new(*name, "")).collect()
    }

    fn plain(line: &str) -> String {
        text::strip_ansi(line)
    }

    #[test]
    fn all_tabs_show_when_they_fit_and_the_one_in_view_is_set_apart() {
        let theme = Theme::dark();
        let mut pages = tabs(&["Accounts", "Models", "Appearance"]);
        pages[2].badge = "2".to_string();
        let line = row(&pages, 1, 60, &theme);
        assert_eq!(plain(&line), "Accounts  Models  Appearance 2");
        // Only the tab in view is bold and underlined.
        assert_eq!(line.matches("\x1b[1;4m").count(), 1);
        let bold_at = line.find("\x1b[1;4m").unwrap();
        assert!(line[bold_at..].contains("Models"));
        assert!(!line[..bold_at].contains("Models"));
    }

    #[test]
    fn a_row_too_narrow_for_all_keeps_the_tab_in_view_and_marks_what_is_left_out() {
        let theme = Theme::dark();
        let pages = tabs(&[
            "Accounts",
            "Models",
            "Appearance",
            "Status line",
            "Input",
            "Permissions",
            "Tools",
        ]);
        assert_eq!(
            plain(&row(&pages, 0, 30, &theme)),
            "Accounts  Models  Appearance …"
        );
        assert_eq!(plain(&row(&pages, 3, 30, &theme)), "… Status line  Input …");
        assert_eq!(plain(&row(&pages, 6, 30, &theme)), "… Input  Permissions  Tools");
        // Every width holds the row, and the tab in view is in it while it fits at all.
        for width in 1..=120 {
            for current in 0..pages.len() {
                let line = row(&pages, current, width, &theme);
                assert!(text::width(&line) <= width, "{width}: {line:?}");
                if width >= pages[current].width() + 4 {
                    assert!(
                        plain(&line).contains(&pages[current].label),
                        "{width}/{current}: {line:?}"
                    );
                }
            }
        }
        assert_eq!(row(&[], 0, 40, &theme), "");
    }

    #[test]
    fn the_ascii_set_draws_the_row_in_ascii() {
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let pages = tabs(&["Accounts", "Models", "Appearance", "Status line"]);
        let line = plain(&row(&pages, 2, 24, &theme));
        assert!(line.is_ascii(), "{line:?}");
        assert_eq!(line, "... Appearance ...");
    }
}
