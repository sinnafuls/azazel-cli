//! `/diff`: what changed, full screen. The files are listed on the left with what happened to
//! each, the selected file's diff is on the right, in one column or old beside new.
//!
//! ```text
//! Diff · working tree against HEAD              file 1/3 · 0/3 reviewed · unified
//! ▌M src/lib.rs   +3 -1 │ M src/lib.rs  +3 -1
//!  A notes.md       +12 │▌ @@ -1,4 +1,6 @@
//!  ? new.txt         +2 │   1  1  fn main() {
//! j/k move · n/p file · [ ] hunk · m reviewed · s split · w wrap · / search · q back
//! ```
//!
//! A file marked reviewed stays marked for the session, as long as its diff stays the same.

use std::collections::HashSet;
use std::io::Write;

use azazel_core::{EntryKind, SessionStore};
use azazel_tui::components::diff::{self, Laid, Layout, Row};
use azazel_tui::{Glyph, Line, RESET, Role, Theme, text};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use serde_json::json;
use sha2::{Digest, Sha256};

use super::backend::Incoming;
use super::{Tui, clip};
use crate::git::FileDiff;

/// The session entry that records a file marked reviewed, or unmarked.
const REVIEW_ENTRY: &str = "diff_review";
/// Below this many columns the file list gives its room to the diff.
const MIN_LIST_WIDTH: usize = 60;
/// From this many columns on, `tui.diff_view = "auto"` shows old beside new.
pub(super) const SPLIT_WIDTH: usize = 140;
/// The cursor's mark and a space before each row of the diff.
const GUTTER: usize = 2;

/// One file of the viewer.
pub(super) struct File {
    path: String,
    status: char,
    added: usize,
    removed: usize,
    /// The file's unified diff, which `y` copies.
    text: String,
    rows: Vec<Row>,
    /// What a reviewed mark is kept under.
    key: String,
}

impl File {
    pub(super) fn new(diff: FileDiff) -> File {
        File {
            rows: diff::rows(&diff.text),
            key: review_key(&diff.path, &diff.text),
            path: diff.path,
            status: diff.status,
            added: diff.added,
            removed: diff.removed,
            text: diff.text,
        }
    }
}

/// The path and a digest of the file's diff: a file that changes again after it was reviewed is
/// no longer marked.
fn review_key(path: &str, diff: &str) -> String {
    let digest = Sha256::digest(diff.as_bytes());
    let hex: String = digest[..8].iter().map(|byte| format!("{byte:02x}")).collect();
    format!("{path}@{hex}")
}

/// The keys of the files marked reviewed in the session's branch.
pub(super) fn reviewed_marks(session: &SessionStore) -> HashSet<String> {
    let mut marks = HashSet::new();
    for entry in session.branch(None) {
        if let EntryKind::Custom { custom_type, data } = entry.kind
            && custom_type == REVIEW_ENTRY
            && let Some(key) = data["key"].as_str()
        {
            match data["reviewed"].as_bool() {
                Some(true) => marks.insert(key.to_string()),
                _ => marks.remove(key),
            };
        }
    }
    marks
}

/// Records in the session that the file under `key` was marked reviewed, or unmarked.
pub(super) fn record_mark(session: &SessionStore, key: &str, reviewed: bool) {
    session.append(EntryKind::Custom {
        custom_type: REVIEW_ENTRY.to_string(),
        data: json!({ "key": key, "reviewed": reviewed }),
    });
}

/// What a key did that the caller acts on.
#[derive(Debug, PartialEq)]
pub(super) enum Outcome {
    Pending,
    Close,
    /// A file was marked reviewed or unmarked: the mark is kept in the session.
    Reviewed {
        key: String,
        reviewed: bool,
    },
    Copy {
        text: String,
    },
}

/// The columns of the screen: the file list (none when the window is narrow) and the diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Columns {
    list: usize,
    /// The diff's own width, without the cursor's gutter.
    diff: usize,
}

impl Columns {
    fn of(width: usize) -> Columns {
        let list = match width >= MIN_LIST_WIDTH {
            true => (width / 4).clamp(22, 40),
            false => 0,
        };
        let separator = usize::from(list > 0);
        Columns {
            list,
            diff: width.saturating_sub(list + separator + GUTTER).max(1),
        }
    }
}

pub(super) struct DiffView {
    title: String,
    files: Vec<File>,
    selected: usize,
    /// The row of the selected file's diff the cursor is on, and the first row shown.
    cursor: usize,
    top: usize,
    list_top: usize,
    /// `tui.diff_view`: `None` is `auto`, split from [`SPLIT_WIDTH`] columns on.
    configured: Option<bool>,
    /// What `s` chose, over the setting, until the viewer closes.
    split: Option<bool>,
    wrap: bool,
    reviewed: HashSet<String>,
    theme: Theme,
    width: usize,
    /// The selected file laid out at `width`, with the layout it was laid out with.
    laid: Option<(Layout, Laid)>,
    typing: Option<String>,
    search: Option<String>,
    notice: Option<String>,
    /// Rows of the diff the last screen showed, for paging.
    page: usize,
}

impl DiffView {
    /// `split` is `tui.diff_view`: `Some(true)` split, `Some(false)` unified, `None` auto.
    pub(super) fn new(
        title: impl Into<String>,
        files: Vec<File>,
        reviewed: HashSet<String>,
        split: Option<bool>,
        theme: Theme,
    ) -> DiffView {
        DiffView {
            title: title.into(),
            files,
            selected: 0,
            cursor: 0,
            top: 0,
            list_top: 0,
            configured: split,
            split: None,
            wrap: false,
            reviewed,
            theme,
            width: 80,
            laid: None,
            typing: None,
            search: None,
            notice: None,
            page: 1,
        }
    }

    fn layout(&self) -> Layout {
        let auto = self.width >= SPLIT_WIDTH;
        Layout {
            split: self.split.or(self.configured).unwrap_or(auto),
            wrap: self.wrap,
        }
    }

    /// The selected file laid out for the current width and layout.
    fn laid(&mut self) -> &Laid {
        let layout = self.layout();
        if self.laid.as_ref().is_none_or(|(laid_as, _)| *laid_as != layout) {
            let laid = self.lay_out(self.selected, layout);
            self.laid = Some((layout, laid));
        }
        &self.laid.get_or_insert_default().1
    }

    fn lay_out(&self, index: usize, layout: Layout) -> Laid {
        let file = &self.files[index];
        let width = Columns::of(self.width).diff;
        let path = file.path.rsplit(" → ").next().unwrap_or(&file.path);
        diff::lay_out(&file.rows, path, width, layout, &self.theme)
    }

    fn is_reviewed(&self, index: usize) -> bool {
        self.reviewed.contains(&self.files[index].key)
    }

    fn select(&mut self, index: usize) {
        if index != self.selected {
            self.selected = index;
            self.laid = None;
        }
        self.cursor = 0;
        self.top = 0;
    }

    /// Lays the diff out again (another width, `s`, `w`) with the cursor on the hunk it was in.
    fn relay(&mut self, change: impl FnOnce(&mut DiffView)) {
        let hunk = self
            .laid
            .as_ref()
            .and_then(|(_, laid)| laid.hunks.iter().rposition(|start| *start <= self.cursor));
        change(self);
        self.laid = None;
        let landing = hunk.and_then(|hunk| self.laid().hunks.get(hunk).copied());
        self.cursor = landing.unwrap_or(0);
        self.top = self.cursor;
    }

    fn matches(&self, line: &str) -> bool {
        self.search.as_deref().is_some_and(|query| found(query, line))
    }

    /// Moves to the next row that matches the search: further down this file, then in the files
    /// after it, round to the top of this one.
    fn next_match(&mut self) {
        let Some(query) = self.search.clone() else {
            return self.set_notice("no search yet: / starts one");
        };
        let from = self.cursor + 1;
        if let Some(row) = self
            .laid()
            .lines
            .iter()
            .skip(from)
            .position(|line| found(&query, line))
        {
            self.cursor = from + row;
            return;
        }
        let count = self.files.len();
        for step in 1..=count {
            let index = (self.selected + step) % count;
            if !self.files[index].text.to_lowercase().contains(&query) {
                continue;
            }
            let laid = self.lay_out(index, self.layout());
            if let Some(row) = laid.lines.iter().position(|line| found(&query, line)) {
                self.select(index);
                self.laid = Some((self.layout(), laid));
                self.cursor = row;
                return;
            }
        }
        self.set_notice(format!("no match for `{query}`"));
    }

    fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    pub(super) fn is_typing(&self) -> bool {
        self.typing.is_some()
    }

    pub(super) fn paste(&mut self, pasted: &str) {
        if let Some(query) = &mut self.typing {
            query.push_str(pasted.lines().next().unwrap_or_default());
        }
    }

    pub(super) fn on_key(&mut self, key: &KeyEvent) -> Outcome {
        if key.kind == KeyEventKind::Release {
            return Outcome::Pending;
        }
        self.notice = None;
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if let Some(query) = &mut self.typing {
            match key.code {
                KeyCode::Esc => self.typing = None,
                KeyCode::Char('c') if ctrl => self.typing = None,
                KeyCode::Backspace => {
                    query.pop();
                }
                KeyCode::Enter => {
                    // An empty query looks for the last one again.
                    let query = self.typing.take().unwrap_or_default().to_lowercase();
                    if !query.is_empty() {
                        self.search = Some(query);
                    }
                    self.next_match();
                }
                KeyCode::Char(c) if !ctrl => query.push(c),
                _ => {}
            }
            return Outcome::Pending;
        }
        let last = self.laid().lines.len().saturating_sub(1);
        let page = self.page.max(1);
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => return Outcome::Close,
            KeyCode::Char('c') if ctrl => return Outcome::Close,
            KeyCode::Up | KeyCode::Char('k') => self.cursor = self.cursor.saturating_sub(1),
            KeyCode::Down | KeyCode::Char('j') => self.cursor = (self.cursor + 1).min(last),
            KeyCode::PageUp | KeyCode::Char('b') => self.cursor = self.cursor.saturating_sub(page),
            KeyCode::PageDown | KeyCode::Char(' ') => self.cursor = (self.cursor + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => self.cursor = 0,
            KeyCode::End | KeyCode::Char('G') => self.cursor = last,
            KeyCode::Char('n') | KeyCode::Tab => match self.selected + 1 < self.files.len() {
                true => self.select(self.selected + 1),
                false => self.set_notice("this is the last file"),
            },
            KeyCode::Char('p') | KeyCode::BackTab => match self.selected > 0 {
                true => self.select(self.selected - 1),
                false => self.set_notice("this is the first file"),
            },
            KeyCode::Char(']') => {
                let cursor = self.cursor;
                match self.laid().hunks.iter().find(|start| **start > cursor).copied() {
                    Some(start) => {
                        self.cursor = start;
                        self.top = start;
                    }
                    None => self.set_notice("no more hunks in this file: n goes to the next file"),
                }
            }
            KeyCode::Char('[') => {
                let cursor = self.cursor;
                match self
                    .laid()
                    .hunks
                    .iter()
                    .rev()
                    .find(|start| **start < cursor)
                    .copied()
                {
                    Some(start) => {
                        self.cursor = start;
                        self.top = start;
                    }
                    None => self.set_notice("no hunk above this one: p goes to the file before"),
                }
            }
            KeyCode::Char('m') => return self.toggle_reviewed(),
            KeyCode::Char('s') => {
                let split = !self.layout().split;
                self.relay(|view| view.split = Some(split));
            }
            KeyCode::Char('w') => self.relay(|view| view.wrap = !view.wrap),
            KeyCode::Char('/') => self.typing = Some(String::new()),
            KeyCode::Char('y') => {
                return Outcome::Copy {
                    text: self.files[self.selected].text.clone(),
                };
            }
            _ => {}
        }
        Outcome::Pending
    }

    /// `m`: marks the selected file reviewed and goes on to the next file that is not, or takes
    /// the mark away.
    fn toggle_reviewed(&mut self) -> Outcome {
        let key = self.files[self.selected].key.clone();
        let reviewed = !self.reviewed.remove(&key);
        if reviewed {
            self.reviewed.insert(key.clone());
            let count = self.files.len();
            let next = (1..count)
                .map(|step| (self.selected + step) % count)
                .find(|index| !self.is_reviewed(*index));
            match next {
                Some(index) => self.select(index),
                None => self.set_notice("every file is reviewed"),
            }
        }
        Outcome::Reviewed { key, reviewed }
    }

    /// One screen: a title row, the file list beside the selected file's diff, and a row of key
    /// hints. Always `height` rows, none wider than `width`.
    pub(super) fn render(&mut self, width: usize, height: usize) -> Vec<Line> {
        let width = width.max(1);
        if width != self.width {
            self.relay(|view| view.width = width);
        }
        let theme = self.theme.clone();
        let columns = Columns::of(width);
        // The title, the file's own header row and the hints take a row each.
        let body = height.saturating_sub(2).max(1);
        let shown = body.saturating_sub(1).max(1);
        self.page = shown;
        let total = self.laid().lines.len();
        self.cursor = self.cursor.min(total.saturating_sub(1));
        if self.cursor < self.top {
            self.top = self.cursor;
        } else if self.cursor >= self.top + shown {
            self.top = self.cursor + 1 - shown;
        }
        self.top = self.top.min(total.saturating_sub(1));
        if self.selected < self.list_top {
            self.list_top = self.selected;
        } else if self.selected >= self.list_top + body {
            self.list_top = self.selected + 1 - body;
        }

        let mut out = Vec::with_capacity(height);
        if height >= 3 {
            out.push(self.title_row(width, &theme));
        }
        let mut panel = vec![self.file_header(columns.diff + GUTTER, &theme)];
        let laid_lines = match &self.laid {
            Some((_, laid)) => laid.lines.clone(),
            None => Vec::new(),
        };
        for row in self.top..self.top + shown {
            let Some(line) = laid_lines.get(row) else {
                panel.push(String::new());
                continue;
            };
            let gutter = if row == self.cursor {
                theme.paint(Role::Accent, &format!("{} ", theme.glyph(Glyph::Rail)))
            } else if self.matches(line) {
                theme.paint(Role::Warning, &format!("{} ", theme.glyphs().bullets()[0]))
            } else {
                "  ".to_string()
            };
            panel.push(format!("{gutter}{line}"));
        }
        let bar = theme.paint(Role::Border, theme.glyph(Glyph::TreeLine));
        for (index, row) in panel.into_iter().enumerate() {
            let line = match columns.list {
                0 => row,
                list => format!("{}{bar}{row}", self.list_row(self.list_top + index, list, &theme)),
            };
            out.push(text::truncate(&line, width, ""));
        }
        if height >= 2 {
            out.push(self.footer(width, &theme));
        }
        out.truncate(height);
        out
    }

    fn title_row(&self, width: usize, theme: &Theme) -> Line {
        let reviewed = (0..self.files.len())
            .filter(|index| self.is_reviewed(*index))
            .count();
        let layout = self.layout();
        let mut place = format!(
            "file {}/{}{}{reviewed}/{} reviewed{}{}",
            self.selected + 1,
            self.files.len(),
            theme.glyph(Glyph::Separator),
            self.files.len(),
            theme.glyph(Glyph::Separator),
            if layout.split { "split" } else { "unified" }
        );
        if layout.wrap {
            place.push_str(theme.glyph(Glyph::Separator));
            place.push_str("wrap");
        }
        let title = text::truncate(
            &self.title,
            width.saturating_sub(text::width(&place) + 2),
            theme.glyph(Glyph::Ellipsis),
        );
        let gap = width.saturating_sub(text::width(&title) + text::width(&place));
        let line = format!(
            "{}{}{}",
            theme.paint(Role::Accent, &title),
            " ".repeat(gap),
            theme.paint(Role::Muted, &place)
        );
        text::truncate(&line, width, "")
    }

    /// The letter of what happened to a file, in the colour of it.
    fn status(file: &File) -> (String, Role) {
        let role = match file.status {
            'A' => Role::Success,
            'D' => Role::Error,
            'R' => Role::Accent,
            '?' => Role::Muted,
            _ => Role::Warning,
        };
        (file.status.to_string(), role)
    }

    /// `+3 -1`, each part only when it is not nothing, coloured with `prefix` before each colour.
    fn counts(file: &File, theme: &Theme) -> (String, usize) {
        let mut parts = Vec::new();
        let mut shown = Vec::new();
        if file.added > 0 {
            let part = format!("+{}", file.added);
            parts.push(format!("{}{part}", theme.fg(Role::Success)));
            shown.push(part);
        }
        if file.removed > 0 {
            let part = format!("-{}", file.removed);
            parts.push(format!("{}{part}", theme.fg(Role::Error)));
            shown.push(part);
        }
        (parts.join(" "), text::width(&shown.join(" ")))
    }

    /// The row of the file list for file `index`, exactly `width` columns.
    fn list_row(&self, index: usize, width: usize, theme: &Theme) -> Line {
        let Some(file) = self.files.get(index) else {
            return " ".repeat(width);
        };
        let selected = index == self.selected;
        let done = theme.glyph(Glyph::TodoDone);
        let mark = match self.is_reviewed(index) {
            true => format!("{}{done}", theme.fg(Role::Success)),
            false => " ".repeat(text::width(done)),
        };
        let rail = match selected {
            true => format!("{}{}", theme.fg(Role::Accent), theme.glyph(Glyph::Rail)),
            false => " ".to_string(),
        };
        let (letter, role) = Self::status(file);
        let (counts, counts_width) = Self::counts(file, theme);
        // The rail, the mark, the letter and a space before the path; a space before the counts.
        let lead = 1 + text::width(done) + 2;
        let room = width.saturating_sub(lead + counts_width + usize::from(counts_width > 0));
        let path = cut_left(&file.path, room, theme.glyph(Glyph::Ellipsis));
        let gap = width.saturating_sub(lead + text::width(&path) + counts_width);
        let background = match selected {
            true => theme.bg(Role::SelectedBg),
            false => String::new(),
        };
        let weight = if selected { "\x1b[1m" } else { "" };
        let row = format!(
            "{background}{rail}{mark}{}{letter} {}{weight}{path}\x1b[22m{}{counts}",
            theme.fg(role),
            theme.fg(Role::Text),
            " ".repeat(gap),
        );
        format!(
            "{}{RESET}",
            text::truncate(&row, width, "").trim_end_matches(RESET)
        )
    }

    /// The first row of the diff: the file, what happened to it, and whether it is reviewed.
    fn file_header(&self, width: usize, theme: &Theme) -> Line {
        let Some(file) = self.files.get(self.selected) else {
            return String::new();
        };
        let (letter, role) = Self::status(file);
        let (counts, _) = Self::counts(file, theme);
        let mut line = format!(
            "  {} \x1b[1m{}{}\x1b[22m  {counts}{RESET}",
            theme.paint(role, &letter),
            theme.fg(Role::Accent),
            file.path
        );
        if self.is_reviewed(self.selected) {
            let done = format!("{} reviewed", theme.glyph(Glyph::TodoDone));
            line.push_str(&format!("  {}", theme.paint(Role::Success, &done)));
        }
        text::truncate(&line, width, theme.glyph(Glyph::Ellipsis))
    }

    fn footer(&self, width: usize, theme: &Theme) -> Line {
        let foot = match (&self.typing, &self.notice) {
            (Some(query), _) => format!("{}{query}", theme.paint(Role::Accent, "/")),
            (None, Some(notice)) => theme.paint(Role::Warning, notice),
            (None, None) => {
                let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
                let search = match self.search {
                    Some(_) => "/ search (enter: next)",
                    None => "/ search",
                };
                let hints = [
                    &format!("j/k {arrows} move"),
                    "n/p file",
                    "[ ] hunk",
                    "m reviewed",
                    "s split",
                    "w wrap",
                    search,
                    "y copy",
                    "q back",
                ];
                theme.paint(Role::Muted, &hints.join(theme.glyph(Glyph::Separator)))
            }
        };
        text::truncate(&foot, width, theme.glyph(Glyph::Ellipsis))
    }

    /// Where the terminal cursor belongs while a query is typed: `(row, column)` on the screen.
    pub(super) fn typing_cursor(&self, width: usize, height: usize) -> Option<(u16, u16)> {
        let query = self.typing.as_deref()?;
        let column = (1 + text::width(query)).min(width.saturating_sub(1));
        Some((height.saturating_sub(1) as u16, column as u16))
    }
}

/// Whether the text of `line` holds `query`, which is lower-case, in any case.
fn found(query: &str, line: &str) -> bool {
    text::strip_ansi(line).to_lowercase().contains(query)
}

/// `path` cut to `width` columns from the left, so the file's name stays: `…/deep/name.rs`.
fn cut_left(path: &str, width: usize, ellipsis: &str) -> String {
    if text::width(path) <= width {
        return path.to_string();
    }
    let room = width.saturating_sub(text::width(ellipsis));
    let mut kept: Vec<char> = Vec::new();
    let mut used = 0;
    for c in path.chars().rev() {
        let w = text::width(c.encode_utf8(&mut [0; 4]));
        if used + w > room {
            break;
        }
        used += w;
        kept.push(c);
    }
    let tail: String = kept.into_iter().rev().collect();
    match room == 0 {
        true => String::new(),
        false => format!("{ellipsis}{tail}"),
    }
}

/// Shows `files` full screen under `title` until the user leaves with `q` or `esc`.
pub(super) async fn show(tui: &mut Tui, title: &str, files: Vec<FileDiff>) {
    let Some(session) = tui.backend.local().map(|local| local.active.session.clone()) else {
        return;
    };
    let configured = match tui.env.config.tui.diff_view {
        azazel_core::config::DiffView::Auto => None,
        azazel_core::config::DiffView::Split => Some(true),
        azazel_core::config::DiffView::Unified => Some(false),
    };
    let files = files.into_iter().map(File::new).collect();
    let mut view = DiffView::new(
        title,
        files,
        reviewed_marks(&session),
        configured,
        tui.theme.clone(),
    );
    let _ = tui.renderer.suspend();
    tui.leaving_main();
    // A popup may have the alternate screen already; then it stays when the viewer closes.
    let entered = tui.guard.enter_alt();
    let mut dirty = true;
    loop {
        if tui.settle_resize() || dirty {
            let (width, height) = tui.renderer.size();
            if draw(tui, &mut view, width, height).is_err() {
                break;
            }
            dirty = false;
        }
        let resize_deadline = tui.resize.deadline();
        tokio::select! {
            event = super::next_terminal(&mut tui.terminal) => match event {
                Some(Ok(Event::Key(key))) if key.kind != KeyEventKind::Release => {
                    dirty = true;
                    match view.on_key(&key) {
                        Outcome::Pending => {}
                        Outcome::Close => break,
                        Outcome::Reviewed { key, reviewed } => record_mark(&session, &key, reviewed),
                        Outcome::Copy { text } => {
                            let how = clip::copy(&text, tui.renderer.get_mut());
                            view.set_notice(format!("copied the file's diff ({}) to {how}", clip::size(&text)));
                        }
                    }
                }
                Some(Ok(Event::Paste(pasted))) if view.is_typing() => {
                    view.paste(&pasted);
                    dirty = true;
                }
                Some(Ok(Event::FocusGained)) => tui.attention.focus(true),
                Some(Ok(Event::FocusLost)) => tui.attention.focus(false),
                Some(Ok(Event::Resize(columns, rows))) => tui.size_reported((columns, rows)),
                Some(Ok(_)) => {}
                Some(Err(_)) | None => break,
            },
            incoming = tui.backend.next() => match incoming {
                Incoming::Request(request) => {
                    tui.waiting.push_back(request);
                    view.set_notice("the agent is waiting for you: q returns to it");
                    dirty = true;
                }
                // What the agents do meanwhile is shown once the viewer is closed.
                other => tui.held.push_back(other),
            },
            _ = super::wake_at(resize_deadline) => {}
        }
    }
    if entered {
        tui.guard.leave_alt();
        tui.back_on_main();
    } else {
        tui.popup_painted_over();
    }
}

fn draw(tui: &mut Tui, view: &mut DiffView, width: u16, height: u16) -> std::io::Result<()> {
    let (columns, rows) = (usize::from(width), usize::from(height));
    let lines = view.render(columns, rows);
    let cursor = view.typing_cursor(columns, rows);
    let frame = super::pager::frame(&lines, cursor);
    let out = tui.renderer.get_mut();
    out.write_all(frame.as_bytes())?;
    out.flush()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn file(path: &str, status: char, text: &str) -> File {
        let rows = diff::rows(text);
        let added = rows.iter().filter(|row| matches!(row, Row::Added { .. })).count();
        let removed = rows
            .iter()
            .filter(|row| matches!(row, Row::Removed { .. }))
            .count();
        File::new(FileDiff {
            file: path.to_string(),
            path: path.to_string(),
            status,
            added,
            removed,
            text: text.to_string(),
        })
    }

    fn files() -> Vec<File> {
        let long: String = (1..=40).map(|n| format!(" line {n}\n")).collect();
        vec![
            file(
                "src/lib.rs",
                'M',
                &format!(
                    "--- a/src/lib.rs\n+++ b/src/lib.rs\n@@ -1,3 +1,3 @@\n fn main() {{\n-    old();\n+    new();\n }}\n@@ -50,41 +50,42 @@\n{long}+added at the end\n"
                ),
            ),
            file(
                "notes.md",
                'A',
                "new file\n--- /dev/null\n+++ b/notes.md\n@@ -0,0 +1,2 @@\n+# Notes\n+needle here",
            ),
            file(
                "old.txt",
                'D',
                "--- a/old.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone",
            ),
        ]
    }

    fn view(width: usize) -> DiffView {
        let mut view = DiffView::new("Diff · test", files(), HashSet::new(), None, Theme::dark());
        view.render(width, 24);
        view
    }

    fn press(view: &mut DiffView, keys: &str) -> Vec<Outcome> {
        let mut outcomes = Vec::new();
        for c in keys.chars() {
            let code = match c {
                '\n' => KeyCode::Enter,
                c => KeyCode::Char(c),
            };
            outcomes.push(view.on_key(&KeyEvent::new(code, KeyModifiers::NONE)));
            view.render(view.width, 24);
        }
        outcomes
    }

    /// The text of the row of the diff the cursor is on.
    fn at_cursor(view: &mut DiffView) -> String {
        let cursor = view.cursor;
        text::strip_ansi(&view.laid().lines[cursor])
            .trim_end()
            .to_string()
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn the_screen_fits_at_every_width_with_the_list_beside_the_diff_from_sixty_columns() {
        for width in [20, 59, 60, 80, 120, 139, 140, 200] {
            let mut view = view(width);
            let lines = view.render(width, 24);
            assert_eq!(lines.len(), 24, "{width}");
            for line in &lines {
                assert!(text::width(line) <= width, "{width}: {line:?}");
            }
            let shown = plain(&lines);
            let listed = shown[1].contains("src/lib.rs") && shown[2].contains("notes.md");
            assert_eq!(listed, width >= MIN_LIST_WIDTH, "{width}: {shown:?}");
        }
    }

    #[test]
    fn eighty_columns_show_one_column_and_a_hundred_and_forty_old_beside_new() {
        let mut narrow = view(80);
        let shown = plain(&narrow.render(80, 24));
        assert!(
            shown[0].starts_with("Diff · test") && shown[0].ends_with("file 1/3 · 0/3 reviewed · unified")
        );
        // The list is 22 columns at 80: the rail, the mark, the letter, the path and the counts.
        assert_eq!(shown[1].split('│').next().unwrap(), "▌ M src/lib.rs   +2 -1");
        assert_eq!(shown[2].split('│').next().unwrap(), "  A notes.md        +2");
        assert_eq!(shown[3].split('│').next().unwrap(), "  D old.txt         -1");
        let diff: Vec<&str> = shown[1..23]
            .iter()
            .map(|row| row.split_once('│').unwrap().1)
            .collect();
        assert!(diff[0].contains("M src/lib.rs  +2 -1"), "{diff:?}");
        assert_eq!(diff[1].trim_end(), "▌ @@ -1,3 +1,3 @@");
        assert_eq!(diff[3].trim_end(), "   2    -    old();");
        assert_eq!(diff[4].trim_end(), "      2 +    new();");

        let mut wide = view(140);
        let shown = plain(&wide.render(140, 24));
        assert!(shown[0].ends_with("split"));
        let row = shown[4].split_once('│').unwrap().1;
        let (old, new) = row.split_once('│').unwrap();
        assert_eq!((old.trim(), new.trim()), ("2 -    old();", "2 +    new();"));

        let mut widest = view(200);
        let shown = plain(&widest.render(200, 24));
        assert!(shown[0].ends_with("split"));
        assert_eq!(
            shown[1].chars().position(|c| c == '│'),
            Some(40),
            "the list stops growing at 40 columns"
        );
    }

    #[test]
    fn the_setting_and_s_choose_the_layout_over_the_width() {
        let mut view = DiffView::new("t", files(), HashSet::new(), Some(true), Theme::dark());
        assert!(plain(&view.render(80, 24))[0].ends_with("split"));
        press(&mut view, "s");
        assert!(plain(&view.render(80, 24))[0].ends_with("unified"));
        press(&mut view, "w");
        assert!(plain(&view.render(80, 24))[0].ends_with("unified · wrap"));
    }

    #[test]
    fn keys_move_through_rows_hunks_and_files() {
        let mut view = view(80);
        press(&mut view, "jj");
        assert_eq!(view.cursor, 2);
        press(&mut view, "]");
        let second = view.laid().hunks[1];
        assert_eq!(view.cursor, second);
        press(&mut view, "]");
        assert!(view.notice.as_deref().unwrap().starts_with("no more hunks"));
        press(&mut view, "[");
        assert_eq!(view.cursor, 0);
        press(&mut view, "G");
        let last = view.laid().lines.len() - 1;
        assert_eq!(view.cursor, last);
        press(&mut view, "n");
        assert_eq!((view.selected, view.cursor), (1, 0));
        press(&mut view, "nn");
        assert_eq!(view.selected, 2);
        assert_eq!(view.notice.as_deref(), Some("this is the last file"));
        press(&mut view, "p");
        assert_eq!(view.selected, 1);
        assert_eq!(press(&mut view, "q"), [Outcome::Close]);
    }

    #[test]
    fn search_finds_the_next_match_in_this_file_then_in_the_next() {
        let mut view = view(80);
        press(&mut view, "/LINE 1\n");
        let shown = plain(&view.render(80, 24));
        assert_eq!(view.selected, 0);
        assert!(at_cursor(&mut view).ends_with(" line 1"));
        // The other matches on screen (`line 10`, ...) are marked.
        assert!(
            shown
                .iter()
                .any(|row| row.contains("• ") && row.contains("line 10")),
            "{shown:?}"
        );
        // Enter on an empty query looks again.
        press(&mut view, "/\n");
        assert!(at_cursor(&mut view).ends_with(" line 10"));
        press(&mut view, "/needle\n");
        assert_eq!(view.selected, 1);
        assert!(at_cursor(&mut view).contains("needle"));
        press(&mut view, "/nothing like it\n");
        assert_eq!(view.notice.as_deref(), Some("no match for `nothing like it`"));
    }

    #[test]
    fn marking_a_file_reviewed_moves_on_and_is_kept_in_the_session() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap();
        let mut view = view(80);
        let outcomes = press(&mut view, "m");
        let Outcome::Reviewed { key, reviewed: true } = &outcomes[0] else {
            panic!("{outcomes:?}");
        };
        assert_eq!(view.selected, 1, "the next file that is not reviewed");
        let shown = plain(&view.render(80, 24));
        assert!(shown[0].contains("1/3 reviewed"));
        assert!(shown[1].starts_with(" ✓M src/lib.rs"), "{:?}", shown[1]);
        record_mark(&session, key, true);

        // Opened again in the same session, the mark is there.
        let marks = reviewed_marks(&SessionStore::open(&session.path()).unwrap());
        let mut again = DiffView::new("t", files(), marks.clone(), None, Theme::dark());
        assert!(plain(&again.render(80, 24))[0].contains("1/3 reviewed"));
        // A file whose diff changed since is not.
        let mut changed = files();
        changed[0] = file("src/lib.rs", 'M', "@@ -1 +1 @@\n-a\n+b");
        let mut later = DiffView::new("t", changed, marks, None, Theme::dark());
        assert!(plain(&later.render(80, 24))[0].contains("0/3 reviewed"));

        // `m` on a reviewed file takes the mark away, and so does the session.
        let Outcome::Reviewed { key, reviewed: false } = &press(&mut again, "m")[0] else {
            panic!("unmarking");
        };
        record_mark(&session, key, false);
        assert!(reviewed_marks(&session).is_empty());
    }

    #[test]
    fn the_ascii_symbol_set_draws_the_viewer_in_ascii() {
        let theme = Theme::dark().with_glyphs(azazel_tui::GlyphLevel::Ascii);
        let mut view = DiffView::new("Diff", files(), HashSet::new(), None, theme);
        press(&mut view, "m");
        for width in [80, 140] {
            for line in view.render(width, 24) {
                assert!(text::strip_ansi(&line).is_ascii(), "{line:?}");
            }
        }
    }

    #[test]
    fn long_paths_keep_their_file_name() {
        assert_eq!(cut_left("src/deep/down/name.rs", 12, "…"), "…own/name.rs");
        assert_eq!(cut_left("a.rs", 12, "…"), "a.rs");
        assert_eq!(cut_left("abc", 0, "…"), "");
    }
}
