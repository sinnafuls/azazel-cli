//! `/statusline`: a preset, a separator and the segments of each side, chosen in a list with
//! the line they make in view above it.

use std::cell::Cell;

use azazel_core::config::{StatusPreset, StatusSeparator, StatuslineConfig};
use azazel_tui::components::select::ListMouse;
use azazel_tui::mouse::MouseKind;
use azazel_tui::{Glyph, Hint, SelectItem, SelectList, Theme};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

use super::backend::Billing;
use super::overlay::Widget;
use super::statusline::{self, Layout, NONE, Name};

/// What a key did to the editor.
pub enum Step {
    Pending,
    Cancelled,
    /// The line is to be kept as it is now.
    Save,
}

/// A row of the list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Row {
    Preset,
    Separator,
    Segment(Name),
}

pub struct StatusEditor {
    /// `[statusline]` as it was when the popup opened: what `esc` goes back to, and the keys
    /// the popup does not edit.
    before: StatuslineConfig,
    preset: StatusPreset,
    separator: StatusSeparator,
    left: Vec<Name>,
    right: Vec<Name>,
    list: SelectList,
    /// The row of the last widget the list started on, under the sample line.
    list_from: Cell<usize>,
    /// The lead is on a subscription: the sample line shows a usage window, not a cost.
    subscription: bool,
}

/// The separators a preset can be drawn with. `dot` is how the configuration says "the
/// preset's own", which for `powerline` is blocks, so it is not a choice of its own there.
fn separators(preset: StatusPreset) -> &'static [StatusSeparator] {
    match preset {
        StatusPreset::Powerline => &[
            StatusSeparator::Powerline,
            StatusSeparator::Bar,
            StatusSeparator::Space,
        ],
        _ => StatusSeparator::ALL,
    }
}

/// The value `steps` places after `current` in `all`, going round.
fn turn<T: Copy + PartialEq>(all: &[T], current: T, steps: isize) -> T {
    let at = all.iter().position(|value| *value == current).unwrap_or(0) as isize;
    all[(at + steps).rem_euclid(all.len() as isize) as usize]
}

impl StatusEditor {
    pub fn new(config: &StatuslineConfig, subscription: bool) -> StatusEditor {
        let (layout, _) = Layout::new(config);
        let separator = match (config.preset, config.separator) {
            (preset, StatusSeparator::Dot) if preset.blocks() => StatusSeparator::Powerline,
            (_, separator) => separator,
        };
        let mut editor = StatusEditor {
            before: config.clone(),
            preset: config.preset,
            separator,
            left: layout.left,
            right: layout.right,
            list: SelectList::new(Vec::new()),
            list_from: Cell::new(0),
            subscription,
        };
        editor.list.set_items(editor.items());
        editor
    }

    /// `[statusline]` as it was when the popup opened.
    pub fn before(&self) -> StatuslineConfig {
        self.before.clone()
    }

    /// `[statusline]` as the popup has it now. A side that is the preset's is left to the
    /// preset, so that a later change of the preset's segments reaches it.
    pub fn config(&self) -> StatuslineConfig {
        let (preset_left, preset_right) = statusline::preset(self.preset);
        let listed = |side: &[Name], preset: &[Name]| -> Vec<String> {
            if side == preset {
                Vec::new()
            } else if side.is_empty() {
                vec![NONE.to_string()]
            } else {
                side.iter().map(|name| name.as_str().to_string()).collect()
            }
        };
        let mut config = self.before.clone();
        config.preset = self.preset;
        config.separator = match (self.preset, self.separator) {
            (preset, StatusSeparator::Powerline) if preset.blocks() => StatusSeparator::Dot,
            (_, separator) => separator,
        };
        config.left = listed(&self.left, preset_left);
        config.right = listed(&self.right, preset_right);
        config
    }

    fn rows(&self) -> Vec<Row> {
        let off = Name::ALL
            .into_iter()
            .filter(|name| !self.left.contains(name) && !self.right.contains(name));
        [Row::Preset, Row::Separator]
            .into_iter()
            .chain(self.left.iter().copied().map(Row::Segment))
            .chain(self.right.iter().copied().map(Row::Segment))
            .chain(off.map(Row::Segment))
            .collect()
    }

    fn items(&self) -> Vec<SelectItem> {
        self.rows()
            .into_iter()
            .map(|row| match row {
                Row::Preset => SelectItem::new("Preset", self.preset.as_str()),
                Row::Separator => SelectItem::new("Separator", self.separator.as_str()),
                Row::Segment(name) => {
                    let side = if self.left.contains(&name) {
                        Some("left")
                    } else if self.right.contains(&name) {
                        Some("right")
                    } else {
                        None
                    };
                    let detail = match side {
                        Some(side) => format!("{side:<5}  {}", name.about()),
                        None => format!("       {}", name.about()),
                    };
                    SelectItem::new(name.as_str(), detail).checked(side.is_some())
                }
            })
            .collect()
    }

    /// Shows the rows as they are now, with the cursor on `row`.
    fn refresh(&mut self, row: Row) {
        self.list.set_items(self.items());
        let at = self.rows().iter().position(|other| *other == row);
        self.list.set_cursor(at.unwrap_or(0));
    }

    fn set_preset(&mut self, preset: StatusPreset) {
        let (left, right) = statusline::preset(preset);
        self.preset = preset;
        self.left = left.to_vec();
        self.right = right.to_vec();
        // Each preset starts with its own separator: blocks for `powerline`, dots otherwise.
        self.separator = separators(preset)[0];
    }

    /// Takes `name` off the line, or puts it at the end of the left side.
    fn toggle(&mut self, name: Name) {
        let on = self.left.contains(&name) || self.right.contains(&name);
        self.left.retain(|other| *other != name);
        self.right.retain(|other| *other != name);
        if !on {
            self.left.push(name);
        }
    }

    /// Puts `name` at the end of the left or the right side, unless it is on that side.
    fn put(&mut self, name: Name, right: bool) {
        let (to, from) = match right {
            true => (&mut self.right, &mut self.left),
            false => (&mut self.left, &mut self.right),
        };
        if !to.contains(&name) {
            from.retain(|other| *other != name);
            to.push(name);
        }
    }

    /// Moves `name` one place towards the start or the end of the line. The sides follow each
    /// other in the list, so a move past the end of one goes to the start of the other.
    fn shift(&mut self, name: Name, down: bool) {
        if let Some(at) = self.left.iter().position(|other| *other == name) {
            match down {
                true if at + 1 == self.left.len() => {
                    self.left.pop();
                    self.right.insert(0, name);
                }
                true => self.left.swap(at, at + 1),
                false if at > 0 => self.left.swap(at, at - 1),
                false => {}
            }
        } else if let Some(at) = self.right.iter().position(|other| *other == name) {
            match down {
                true if at + 1 < self.right.len() => self.right.swap(at, at + 1),
                true => {}
                false if at > 0 => self.right.swap(at, at - 1),
                false => {
                    self.right.remove(0);
                    self.left.push(name);
                }
            }
        }
    }

    pub fn on_key(&mut self, key: &KeyEvent) -> Step {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        self.list.clear_hover();
        let Some(row) = self.list.selected().and_then(|at| self.rows().get(at).copied()) else {
            return Step::Cancelled;
        };
        match (key.code, row) {
            (KeyCode::Esc, _) => return Step::Cancelled,
            (KeyCode::Char('c'), _) if ctrl => return Step::Cancelled,
            (KeyCode::Enter, _) => return Step::Save,
            (KeyCode::Up, Row::Segment(name)) if shift => self.shift(name, false),
            (KeyCode::Down, Row::Segment(name)) if shift => self.shift(name, true),
            (KeyCode::Char('['), Row::Segment(name)) => self.shift(name, false),
            (KeyCode::Char(']'), Row::Segment(name)) => self.shift(name, true),
            (KeyCode::Up, _) => return self.moved(SelectList::move_up),
            (KeyCode::Down, _) => return self.moved(SelectList::move_down),
            (KeyCode::PageUp, _) => return self.moved(SelectList::page_up),
            (KeyCode::PageDown, _) => return self.moved(SelectList::page_down),
            (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), Row::Preset) => {
                let steps = if key.code == KeyCode::Left { -1 } else { 1 };
                self.set_preset(turn(StatusPreset::ALL, self.preset, steps));
            }
            (KeyCode::Left | KeyCode::Right | KeyCode::Char(' '), Row::Separator) => {
                let steps = if key.code == KeyCode::Left { -1 } else { 1 };
                self.separator = turn(separators(self.preset), self.separator, steps);
            }
            (KeyCode::Char(' '), Row::Segment(name)) => self.toggle(name),
            (KeyCode::Left, Row::Segment(name)) => self.put(name, false),
            (KeyCode::Right, Row::Segment(name)) => self.put(name, true),
            _ => return Step::Pending,
        }
        self.refresh(row);
        Step::Pending
    }

    /// The mouse over the widget, `row` its row there. A press selects a row; a segment's
    /// box ticks at the first press, and a second press on the preset or the separator steps
    /// it, as `space` does. The wheel moves through the list.
    pub fn on_mouse(&mut self, kind: MouseKind, row: Option<usize>) -> Step {
        let row = row.and_then(|row| row.checked_sub(self.list_from.get()));
        let (index, again) = match self.list.on_mouse(kind, row) {
            ListMouse::Selected(index) => (index, false),
            ListMouse::Again(index) => (index, true),
            ListMouse::Nothing => return Step::Pending,
        };
        let segment = matches!(self.rows().get(index), Some(Row::Segment(_)));
        if segment || again {
            return self.on_key(&KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        }
        Step::Pending
    }

    fn moved(&mut self, step: fn(&mut SelectList)) -> Step {
        step(&mut self.list);
        Step::Pending
    }

    pub fn hints(&self, theme: &Theme) -> Vec<Hint> {
        let arrows = format!("{}{}", theme.glyph(Glyph::MoreUp), theme.glyph(Glyph::MoreDown));
        vec![
            Hint::new(arrows, "move"),
            Hint::new("space", "on or off"),
            Hint::new("left right", "side or value"),
            Hint::new("[ ]", "reorder"),
            Hint::new("enter", "save"),
            Hint::new("esc", "cancel"),
        ]
    }

    /// The line the choices make, for a session that has something to show in every segment,
    /// and the list under it.
    pub fn widget(&self, width: usize, rows: usize, theme: &Theme) -> Widget {
        let mut lines = Vec::new();
        // The preview gives way to the list when the box is small.
        if rows >= 6 {
            let (layout, _) = Layout::new(&self.config());
            let billing = match self.subscription {
                true => Billing::Subscription {
                    window: Some(("5h".to_string(), 0.61)),
                },
                false => Billing::Metered,
            };
            lines.push(statusline::sample(&billing).render(&layout, None, width, theme));
            lines.push(String::new());
        }
        let first = lines.len();
        self.list_from.set(first);
        let view = self.list.view(width, rows.saturating_sub(first).max(1), theme);
        lines.extend(view.lines);
        Widget {
            lines,
            cursor: None,
            scroll: view.scroll.map(|scroll| (first, scroll)),
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_tui::text;

    use super::*;

    fn editor(table: &str) -> StatusEditor {
        StatusEditor::new(&toml::from_str(table).unwrap(), false)
    }

    fn press(editor: &mut StatusEditor, code: KeyCode) {
        assert!(matches!(
            editor.on_key(&KeyEvent::new(code, KeyModifiers::NONE)),
            Step::Pending
        ));
    }

    /// Moves the cursor to the row of `label`.
    fn go_to(editor: &mut StatusEditor, label: &str) {
        let at = editor
            .items()
            .iter()
            .position(|item| item.label == label)
            .unwrap();
        editor.list.set_cursor(at);
    }

    fn names(side: &[Name]) -> Vec<&'static str> {
        side.iter().map(|name| name.as_str()).collect()
    }

    fn shown(editor: &StatusEditor, width: usize, rows: usize) -> Vec<String> {
        let theme = Theme::dark();
        editor
            .widget(width, rows, &theme)
            .lines
            .iter()
            .map(|line| text::strip_ansi(line).trim_end().to_string())
            .collect()
    }

    #[test]
    fn the_rows_are_the_two_choices_then_the_line_from_left_to_right_then_what_is_off() {
        let editor = editor("");
        let rows = shown(&editor, 160, 40);
        assert!(
            rows[0].starts_with("⏵ accept-edits · build · ⑂ main · ~/src/app · ◫ 42% · "),
            "{rows:?}"
        );
        assert!(rows[0].ends_with("⬢ anthropic/claude-opus-5-5 · priority · high"));
        assert_eq!(rows[1], "");
        assert_eq!(rows[2], "▌ Preset      default");
        assert_eq!(rows[3], "  Separator   dot");
        assert_eq!(rows[4], "  ▣ mode        left   the permission mode");
        assert_eq!(rows[5], "  ▣ agent       left   the primary agent the lead is");
        assert_eq!(
            rows[15],
            "  ▣ extensions  left   what extensions show with azazel.status"
        );
        assert_eq!(rows[16], "  ▣ model       right  the model that answers");
        assert_eq!(rows[19], "  □ todo               todo items done");
        // Every segment and the two choices, whatever is on the line.
        assert_eq!(rows.len(), 2 + 2 + Name::ALL.len());
        // A small box keeps the list.
        assert_eq!(shown(&editor, 100, 5).len(), 5);
        assert_eq!(shown(&editor, 100, 5)[0], "▌ Preset      default");
    }

    #[test]
    fn a_preset_brings_its_segments_and_its_separator() {
        let mut editor = editor("");
        press(&mut editor, KeyCode::Right);
        assert_eq!(editor.preset, StatusPreset::Minimal);
        assert_eq!(names(&editor.left), ["mode", "path", "agents"]);
        assert_eq!(names(&editor.right), ["model"]);
        press(&mut editor, KeyCode::Char(' '));
        assert_eq!(editor.preset, StatusPreset::Powerline);
        assert_eq!(editor.separator, StatusSeparator::Powerline);
        // Left to the preset: only the preset is written.
        let config = editor.config();
        assert_eq!(config.preset, StatusPreset::Powerline);
        assert_eq!(config.separator, StatusSeparator::Dot);
        assert!(config.left.is_empty() && config.right.is_empty());
        assert_eq!(
            Layout::new(&config).0.left,
            statusline::preset(StatusPreset::Powerline).0
        );

        // Blocks are the preset's own separator, so `dot` is no choice of its own there.
        press(&mut editor, KeyCode::Down);
        press(&mut editor, KeyCode::Right);
        assert_eq!(editor.separator, StatusSeparator::Bar);
        assert_eq!(editor.config().separator, StatusSeparator::Bar);
        press(&mut editor, KeyCode::Right);
        press(&mut editor, KeyCode::Right);
        assert_eq!(editor.separator, StatusSeparator::Powerline);
        // Going back round, and the cursor stays where it is.
        press(&mut editor, KeyCode::Up);
        press(&mut editor, KeyCode::Left);
        press(&mut editor, KeyCode::Left);
        press(&mut editor, KeyCode::Left);
        assert_eq!(editor.preset, StatusPreset::Full);
        assert_eq!(editor.separator, StatusSeparator::Dot);
        assert_eq!(editor.list.selected(), Some(0));
    }

    #[test]
    fn segments_go_on_and_off_the_line_change_sides_and_places() {
        let mut editor = editor("preset = \"minimal\"");
        go_to(&mut editor, "time");
        press(&mut editor, KeyCode::Char(' '));
        assert_eq!(names(&editor.left), ["mode", "path", "agents", "time"]);
        // The cursor follows the segment.
        assert_eq!(editor.list.selected_item().unwrap().label, "time");
        press(&mut editor, KeyCode::Right);
        assert_eq!(names(&editor.left), ["mode", "path", "agents"]);
        assert_eq!(names(&editor.right), ["model", "time"]);
        press(&mut editor, KeyCode::Char('['));
        assert_eq!(names(&editor.right), ["time", "model"]);
        // Past the start of the right side is the end of the left side, and back.
        press(&mut editor, KeyCode::Char('['));
        assert_eq!(names(&editor.left), ["mode", "path", "agents", "time"]);
        assert_eq!(names(&editor.right), ["model"]);
        press(&mut editor, KeyCode::Char('['));
        press(&mut editor, KeyCode::Char('['));
        press(&mut editor, KeyCode::Char('['));
        assert_eq!(names(&editor.left), ["time", "mode", "path", "agents"]);
        let shifted = KeyEvent::new(KeyCode::Down, KeyModifiers::SHIFT);
        assert!(matches!(editor.on_key(&shifted), Step::Pending));
        assert_eq!(names(&editor.left), ["mode", "time", "path", "agents"]);
        assert_eq!(editor.list.selected_item().unwrap().label, "time");

        let config = editor.config();
        assert_eq!(config.left, ["mode", "time", "path", "agents"]);
        // The right side is the preset's again, and is left to it.
        assert!(config.right.is_empty());
        let (layout, problems) = Layout::new(&config);
        assert!(problems.is_empty(), "{problems:?}");
        assert_eq!(
            (layout.left, layout.right),
            (editor.left.clone(), editor.right.clone())
        );

        // A side with nothing on it stays empty when it is read again.
        go_to(&mut editor, "model");
        press(&mut editor, KeyCode::Char(' '));
        let config = editor.config();
        assert_eq!(config.right, ["none"]);
        assert!(Layout::new(&config).0.right.is_empty());
        // Off, a segment has no place to move to.
        press(&mut editor, KeyCode::Char(']'));
        assert!(editor.right.is_empty());
        press(&mut editor, KeyCode::Left);
        assert_eq!(names(&editor.left), ["mode", "time", "path", "agents", "model"]);
    }

    #[test]
    fn what_the_popup_does_not_edit_is_kept_and_esc_goes_back() {
        let table = "command = \"echo hi\"\nleft = [\"mode\"]\n[segments.path]\nstyle = \"full\"";
        let mut editor = editor(table);
        assert_eq!(names(&editor.left), ["mode"]);
        go_to(&mut editor, "branch");
        press(&mut editor, KeyCode::Char(' '));
        let config = editor.config();
        assert_eq!(config.left, ["mode", "branch"]);
        assert_eq!(config.command, "echo hi");
        assert_eq!(config.segments["path"]["style"].as_str(), Some("full"));
        assert_eq!(editor.before().left, ["mode"]);
        assert!(matches!(
            editor.on_key(&KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE)),
            Step::Cancelled
        ));
        assert!(matches!(
            editor.on_key(&KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            Step::Save
        ));
    }

    #[test]
    fn the_sample_line_follows_every_change_and_fits_the_box() {
        let mut editor = editor("");
        go_to(&mut editor, "branch");
        press(&mut editor, KeyCode::Char(' '));
        assert!(shown(&editor, 100, 40)[0].starts_with("⏵ accept-edits · build · ~/src/app · "));
        let subscription = StatusEditor::new(&StatuslineConfig::default(), true);
        assert!(shown(&subscription, 120, 40)[0].contains("◔ 61% 5h"));
        let theme = Theme::dark();
        for width in 1..=120 {
            for rows in 1..=30 {
                let widget = editor.widget(width, rows, &theme);
                assert!(widget.lines.len() <= rows, "{width}x{rows}");
                for line in &widget.lines {
                    assert!(text::width(line) <= width, "{width}x{rows}: {line:?}");
                }
            }
        }
    }
}
