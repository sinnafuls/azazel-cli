//! Terminals: the screen of a program an agent runs interactively (`shell` with `interactive`),
//! shown live in a box like every other popup.
//!
//! The box opens when an agent starts such a program and only shows it: the agent drives it.
//! `enter` gives the keyboard to the user, after which every key goes to the program and an
//! agent's own input waits; `ctrl+g` hands it back. `esc` puts the box away while the program
//! goes on; `/terminals` and the `terminal` key bring it back.

use std::collections::BTreeMap;
use std::sync::Arc;

use azazel_tools::pty::{Key, Mods, ScreenView};
use azazel_tools::{Terminal, Terminals};
use azazel_tui::components::overlay::Frame;
use azazel_tui::{Glyph, Hint, Line, Role, SelectItem, Theme, text};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tokio::sync::watch;

use super::Tui;
use super::overlay::{Action, Overlay};

/// A box, and where the typing cursor is in it when it shows one.
pub(super) type Boxed = (Vec<Line>, Option<(u16, u16)>);

/// The terminal on show.
pub(super) struct TermView {
    terminal: Arc<Terminal>,
    /// The keyboard is the program's: every key but `ctrl+g` is typed into it.
    typing: bool,
}

/// Which terminals the UI has heard of, and whether each was running then.
#[derive(Default)]
pub(super) struct Known(BTreeMap<String, bool>);

/// Resolves when `changes` says that something happened (to a terminal, to what extensions show);
/// never when there is nothing to hear of.
pub(super) async fn changed(changes: &mut Option<watch::Receiver<u64>>) {
    let heard = match changes {
        Some(changes) => changes.changed().await.is_ok(),
        None => false,
    };
    if !heard {
        // What told of the changes is gone with its session: nothing more will come.
        *changes = None;
        std::future::pending::<()>().await;
    }
}

/// A key press as the key a program is sent. `None` for what no terminal sends.
pub(super) fn key_of(event: &KeyEvent) -> Option<(Key, Mods)> {
    let mut mods = Mods {
        ctrl: event.modifiers.contains(KeyModifiers::CONTROL),
        alt: event.modifiers.contains(KeyModifiers::ALT),
        shift: event.modifiers.contains(KeyModifiers::SHIFT),
    };
    let key = match event.code {
        KeyCode::Char(c) => {
            // A character already is what shift made of it.
            let upper = mods.shift && c.is_lowercase();
            mods.shift = false;
            Key::Char(if upper {
                c.to_uppercase().next().unwrap_or(c)
            } else {
                c
            })
        }
        KeyCode::Enter => Key::Enter,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => {
            mods.shift = true;
            Key::Tab
        }
        KeyCode::Esc => Key::Esc,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        KeyCode::Insert => Key::Insert,
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::F(n @ 1..=12) => Key::F(n),
        _ => return None,
    };
    Some((key, mods))
}

/// The screen a box `width` columns wide and at most `max_rows` rows tall has room for, as
/// `(rows, columns)`.
pub(super) fn screen_size(width: usize, max_rows: usize) -> (u16, u16) {
    let frame = Frame::new("", width);
    let rows = max_rows.saturating_sub(frame.chrome_rows()).max(1);
    let columns = Frame::inner_width(width).max(1);
    (
        u16::try_from(rows).unwrap_or(u16::MAX),
        u16::try_from(columns).unwrap_or(u16::MAX),
    )
}

/// What a terminal's box says about it besides its screen.
pub(super) struct Caption<'a> {
    pub id: &'a str,
    pub command: &'a str,
    /// `running for 12s`, `exited with code 0`, `stopped`.
    pub state: &'a str,
    pub running: bool,
    /// The user has the keyboard.
    pub typing: bool,
}

/// A terminal as a box `width` columns wide and at most `max_rows` rows tall, and where its
/// cursor is while the user types. The id and the command ride the top edge, the keys the
/// bottom edge. A screen taller than the box shows the rows around its cursor.
pub(super) fn render(
    screen: &ScreenView,
    caption: &Caption<'_>,
    width: usize,
    max_rows: usize,
    theme: &Theme,
) -> Boxed {
    let separator = theme.glyph(Glyph::Separator);
    let command = caption.command.lines().next().unwrap_or_default();
    let state = match (caption.running, caption.typing) {
        (true, true) => "you are typing",
        (true, false) => "the agent is typing",
        (false, _) => caption.state,
    };
    // The state comes first: it is what a narrow box must not cut.
    let title = format!(
        "{}{separator}{state}{separator}{}",
        caption.id,
        text::sanitize(command)
    );
    let mut frame = Frame::new(&title, width);
    if caption.typing {
        frame.border = Role::Accent;
    }
    let hints = if caption.typing {
        vec![Hint::new("ctrl+g", "hand it back to the agent")]
    } else if caption.running {
        vec![
            Hint::new("enter", "type into it"),
            Hint::new("esc", "hide"),
            Hint::new("ctrl+x", "stop it"),
        ]
    } else {
        vec![Hint::new("esc", "close")]
    };
    frame.hints = &hints;

    let room = max_rows.saturating_sub(frame.chrome_rows()).max(1);
    let (cursor_row, cursor_column) = screen.cursor.unwrap_or_default();
    // The rows that fit, down to the cursor's when the screen is taller than the box.
    let first = (usize::from(cursor_row) + 1)
        .saturating_sub(room)
        .min(screen.rows.len().saturating_sub(room));
    let rows: Vec<Line> = screen.rows.iter().skip(first).take(room).cloned().collect();
    let (top, left) = frame.origin();
    let cursor = screen
        .cursor
        .filter(|_| caption.typing && usize::from(cursor_column) < Frame::inner_width(width))
        .map(|_| {
            (
                (usize::from(cursor_row) - first + top) as u16,
                (usize::from(cursor_column) + left) as u16,
            )
        });
    (frame.render(&rows, theme), cursor)
}

impl Tui {
    fn terminals(&self) -> Option<Arc<Terminals>> {
        self.backend.local()?.app.registry.services.get::<Terminals>()
    }

    /// Notices of what happens to the terminals of a session in this process.
    pub(super) fn terminal_changes(&self) -> Option<watch::Receiver<u64>> {
        self.terminals().map(|terminals| terminals.subscribe())
    }

    /// Whether a terminal is on show.
    pub(super) fn term_open(&self) -> bool {
        self.term.is_some()
    }

    /// Opens the box for a program an agent has just started, and closes it or says so when
    /// the program on show has ended.
    pub(super) fn follow_terminals(&mut self) {
        let Some(terminals) = self.terminals() else {
            return;
        };
        for terminal in terminals.all() {
            let running = terminal.running();
            match self.terms.0.insert(terminal.id.clone(), running) {
                // A new one takes the place of the one on show, unless the user is typing there.
                None if running => {
                    if !self.term.as_ref().is_some_and(|view| view.typing) {
                        self.term = Some(TermView {
                            terminal,
                            typing: false,
                        });
                    }
                }
                Some(true) if !running => {
                    let shown = self
                        .term
                        .as_ref()
                        .is_some_and(|view| view.terminal.id == terminal.id);
                    // Someone who was typing gets to read how it ended; a box that was only
                    // watched goes away with its program.
                    let typing = shown && self.term.as_ref().is_some_and(|view| view.typing);
                    if typing {
                        self.release_keyboard();
                    } else if shown {
                        self.term = None;
                    }
                    let separator = self.theme.glyph(Glyph::Separator);
                    self.info(&format!(
                        "{} {}{separator}{}",
                        terminal.id,
                        terminal.describe(),
                        first_line(&terminal.command)
                    ));
                }
                _ => {}
            }
        }
    }

    /// Hands the keyboard back to the agent.
    fn release_keyboard(&mut self) {
        if let Some(view) = &mut self.term {
            view.typing = false;
            view.terminal.set_user_typing(false);
        }
    }

    /// The terminal on show as a box that fits `width` and `max_rows`. The program is given a
    /// screen of the size the box has room for.
    pub(super) fn term_box(&self, width: usize, max_rows: usize) -> Option<Boxed> {
        let view = self.term.as_ref()?;
        let terminal = &view.terminal;
        let (rows, columns) = screen_size(width, max_rows);
        terminal.resize(rows, columns);
        let state = terminal.describe();
        let caption = Caption {
            id: &terminal.id,
            command: &terminal.command,
            state: &state,
            running: terminal.running(),
            typing: view.typing,
        };
        Some(render(&terminal.view(), &caption, width, max_rows, &self.theme))
    }

    /// A key while a terminal is on show.
    pub(super) fn term_key(&mut self, key: &KeyEvent) {
        let Some(view) = &mut self.term else {
            return;
        };
        let terminal = view.terminal.clone();
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if view.typing {
            if ctrl && key.code == KeyCode::Char('g') {
                return self.release_keyboard();
            }
            if let Some((key, mods)) = key_of(key)
                && terminal.press(key, mods)
            {
                terminal.typed_by_user();
            }
            return;
        }
        match key.code {
            KeyCode::Esc => self.term = None,
            KeyCode::Char('c') if ctrl => self.term = None,
            KeyCode::Enter if !terminal.running() => self.term = None,
            KeyCode::Enter => {
                view.typing = true;
                terminal.set_user_typing(true);
            }
            KeyCode::Char('g') if ctrl && terminal.running() => {
                view.typing = true;
                terminal.set_user_typing(true);
            }
            KeyCode::Char('x') if ctrl => {
                terminal.stop();
            }
            _ => {}
        }
    }

    /// A paste while a terminal is on show: typed into the program when the user has the
    /// keyboard. True when the terminal took it (or it is on show and nothing else should).
    pub(super) fn term_paste(&mut self, pasted: &str) -> bool {
        let Some(view) = &self.term else {
            return false;
        };
        if view.typing && view.terminal.paste(pasted) {
            view.terminal.typed_by_user();
        }
        true
    }

    /// Puts a terminal on show.
    pub(super) fn show_terminal(&mut self, id: &str) {
        match self.terminals().and_then(|terminals| terminals.get(id)) {
            Some(terminal) => {
                self.release_keyboard();
                self.term = Some(TermView {
                    terminal,
                    typing: false,
                });
            }
            None => self.fail(&format!("no terminal named `{id}`")),
        }
    }

    /// The `terminal` key: the newest program that still runs in a terminal, or the newest
    /// that has ended when none does.
    pub(super) fn show_newest_terminal(&mut self) {
        let all = self
            .terminals()
            .map(|terminals| terminals.all())
            .unwrap_or_default();
        let newest = all
            .iter()
            .rev()
            .find(|terminal| terminal.running())
            .or(all.last());
        match newest {
            Some(terminal) => {
                let id = terminal.id.clone();
                self.show_terminal(&id);
            }
            None => self.notice = Some("no program runs in a terminal".to_string()),
        }
    }

    /// `/terminals`: the programs agents run in terminals; choosing one shows its screen.
    pub(super) fn open_terminals(&mut self) {
        let Some(app) = self.app("/terminals") else {
            return;
        };
        let entries = app
            .registry
            .services
            .get::<Terminals>()
            .map(|terminals| terminals.entries())
            .unwrap_or_default();
        match entries.as_slice() {
            [] => self
                .info("no program runs in a terminal; an agent starts one with an interactive `shell` call"),
            [only] => {
                let id = only.id.clone();
                self.show_terminal(&id);
            }
            _ => {
                let items = entries
                    .iter()
                    .map(|entry| {
                        SelectItem::new(
                            format!("{}  {}", entry.id, first_line(&entry.command)),
                            entry.state.as_str(),
                        )
                    })
                    .collect();
                let ids = entries.into_iter().map(|entry| entry.id).collect();
                self.overlay = Some(
                    Overlay::select("Terminals", Vec::new(), items, false, Action::Terminals(ids))
                        .with_enter("show"),
                );
            }
        }
    }

    /// One muted row above the input while programs run in terminals that are not on show.
    pub(super) fn terminal_lines(&self, width: usize) -> Vec<Line> {
        let running: Vec<Arc<Terminal>> = self
            .terminals()
            .map(|terminals| terminals.all())
            .unwrap_or_default()
            .into_iter()
            .filter(|terminal| terminal.running())
            .collect();
        let key = self.keymap.keys_for("terminal").first().map(ToString::to_string);
        let separator = self.theme.glyph(Glyph::Separator);
        let row = match (running.as_slice(), key) {
            ([], _) => return Vec::new(),
            ([only], Some(key)) => format!(
                "{} runs {}{separator}{key} shows it",
                only.id,
                first_line(&only.command)
            ),
            ([only], None) => format!(
                "{} runs {}{separator}/terminals shows it",
                only.id,
                first_line(&only.command)
            ),
            (many, _) => format!(
                "{} programs run in terminals{separator}/terminals lists them",
                many.len()
            ),
        };
        let cut = text::truncate(&row, width, self.theme.glyph(Glyph::Ellipsis));
        vec![self.theme.paint(Role::Muted, &cut)]
    }
}

fn first_line(command: &str) -> String {
    let line = text::sanitize(command.lines().next().unwrap_or_default());
    text::truncate(&line, 60, "…")
}

#[cfg(test)]
mod tests {
    use azazel_tui::GlyphLevel;
    use crossterm::event::{KeyEventKind, KeyEventState};

    use super::*;

    fn press(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn screen(rows: &[&str], cursor: Option<(u16, u16)>) -> ScreenView {
        ScreenView {
            rows: rows.iter().map(|row| row.to_string()).collect(),
            size: (rows.len() as u16, 40),
            cursor,
        }
    }

    fn plain(lines: &[Line]) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    const WATCHED: Caption<'static> = Caption {
        id: "pty-1",
        command: "npm init\nsecond line",
        state: "running for 3s",
        running: true,
        typing: false,
    };

    #[test]
    fn keys_reach_the_program_as_what_they_are() {
        let none = KeyModifiers::NONE;
        let plain = Mods::default();
        assert_eq!(
            key_of(&press(KeyCode::Char('a'), none)),
            Some((Key::Char('a'), plain))
        );
        // Shift is in the character; a terminal that reports it apart is not believed twice.
        assert_eq!(
            key_of(&press(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some((Key::Char('A'), plain))
        );
        assert_eq!(
            key_of(&press(KeyCode::Char('a'), KeyModifiers::SHIFT)),
            Some((Key::Char('A'), plain))
        );
        let ctrl = Mods { ctrl: true, ..plain };
        assert_eq!(
            key_of(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            Some((Key::Char('c'), ctrl))
        );
        assert_eq!(
            key_of(&press(KeyCode::Left, KeyModifiers::CONTROL)),
            Some((Key::Left, ctrl))
        );
        let shift = Mods { shift: true, ..plain };
        assert_eq!(key_of(&press(KeyCode::BackTab, none)), Some((Key::Tab, shift)));
        assert_eq!(
            key_of(&press(KeyCode::BackTab, KeyModifiers::SHIFT)),
            Some((Key::Tab, shift))
        );
        assert_eq!(key_of(&press(KeyCode::Esc, none)), Some((Key::Esc, plain)));
        assert_eq!(key_of(&press(KeyCode::F(5), none)), Some((Key::F(5), plain)));
        assert_eq!(key_of(&press(KeyCode::F(13), none)), None);
        assert_eq!(key_of(&press(KeyCode::CapsLock, none)), None);
    }

    #[test]
    fn the_program_gets_the_room_inside_the_box() {
        assert_eq!(screen_size(72, 20), (18, 68));
        assert_eq!(screen_size(44, 8), (6, 40));
        // Never nothing, however small the window.
        assert_eq!(screen_size(3, 1), (1, 3));
        assert_eq!(screen_size(0, 0), (1, 1));
    }

    #[test]
    fn a_watched_terminal_names_its_program_and_the_keys_and_has_no_cursor() {
        let theme = Theme::dark();
        let view = screen(&["? package name: (demo)", "", ""], Some((0, 23)));
        let (lines, cursor) = render(&view, &WATCHED, 60, 10, &theme);
        let shown = plain(&lines);
        assert_eq!(shown.len(), 5, "the screen's rows and the two edges");
        assert!(shown.iter().all(|line| text::width(line) == 60), "{shown:#?}");
        assert!(
            shown[0].starts_with("╭─ pty-1 · the agent is typing · npm init ─"),
            "{}",
            shown[0]
        );
        assert!(shown[1].starts_with("│ ? package name: (demo)"), "{}", shown[1]);
        assert!(
            shown[4].starts_with("╰─ enter type into it · esc hide · ctrl+x stop it ─"),
            "{}",
            shown[4]
        );
        assert_eq!(cursor, None);
    }

    #[test]
    fn typing_shows_the_cursor_where_the_program_has_it_and_how_to_hand_back() {
        let theme = Theme::dark();
        let view = screen(&["name:", "> ab"], Some((1, 4)));
        let caption = Caption {
            typing: true,
            ..WATCHED
        };
        let (lines, cursor) = render(&view, &caption, 60, 10, &theme);
        let shown = plain(&lines);
        assert!(shown[0].contains("you are typing"), "{}", shown[0]);
        assert!(
            shown[3].starts_with("╰─ ctrl+g hand it back to the agent ─"),
            "{}",
            shown[3]
        );
        // One row down for the top edge, two columns in for the border and its gap.
        assert_eq!(cursor, Some((2, 6)));
        // A program that hides its cursor has none here either.
        let hidden = screen(&["name:", "> ab"], None);
        assert_eq!(render(&hidden, &caption, 60, 10, &theme).1, None);
    }

    #[test]
    fn a_screen_taller_than_the_box_shows_the_rows_around_the_cursor() {
        let theme = Theme::dark();
        let rows: Vec<String> = (0..12).map(|row| format!("row {row}")).collect();
        let rows: Vec<&str> = rows.iter().map(String::as_str).collect();
        let caption = Caption {
            typing: true,
            ..WATCHED
        };
        // Room for four rows; the cursor is on the tenth.
        let (lines, cursor) = render(&screen(&rows, Some((9, 2))), &caption, 50, 6, &theme);
        let shown = plain(&lines);
        assert_eq!(shown.len(), 6);
        assert!(shown[1].starts_with("│ row 6"), "{}", shown[1]);
        assert!(shown[4].starts_with("│ row 9"), "{}", shown[4]);
        assert_eq!(cursor, Some((4, 4)));
        // With the cursor near the top the first rows show.
        let (lines, _) = render(&screen(&rows, Some((1, 0))), &caption, 50, 6, &theme);
        assert!(plain(&lines)[1].starts_with("│ row 0"));
    }

    #[test]
    fn an_ended_program_says_how_it_ended_and_fits_every_width_in_every_symbol_set() {
        let ended = Caption {
            state: "exited with code 2",
            running: false,
            ..WATCHED
        };
        let view = screen(&["\x1b[1;31merror:\x1b[0m nope", "日本語 wide"], Some((1, 0)));
        for level in [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            for width in 5..=160 {
                for max_rows in 1..8 {
                    let (lines, cursor) = render(&view, &ended, width, max_rows, &theme);
                    assert!(lines.len() <= max_rows.max(3), "{width}x{max_rows}");
                    for line in &lines {
                        assert_eq!(text::width(line), width, "{width}x{max_rows}: {line:?}");
                    }
                    assert_eq!(cursor, None);
                }
            }
        }
        let theme = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        let shown = plain(&render(&view, &ended, 70, 6, &theme).0);
        assert!(
            shown[0].contains("pty-1 | exited with code 2 | npm init"),
            "{}",
            shown[0]
        );
        assert!(shown[3].contains("esc close"), "{}", shown[3]);
        assert!(shown[1].contains("error: nope"));
    }
}
