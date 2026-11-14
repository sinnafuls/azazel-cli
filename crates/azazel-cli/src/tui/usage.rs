//! The `/usage` popup: what each subscription has left, in a box that asks the providers again
//! on a key.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use azazel_ai::credentials::now_ms;
use azazel_tui::KeyCombo;
use crossterm::event::{KeyCode, KeyModifiers};

use super::overlay::{Action, Overlay};
use super::{Internal, Tui};
use crate::usage::{self, Ask, Entry, Source, View};

const TITLE: &str = "Usage";
/// The title while the providers are asked: the box shows what was known until they answer.
const ASKING: &str = "Usage · asking the providers…";

/// What the open box shows. The answers of the providers arrive while it is open.
pub struct Shown {
    entries: Mutex<Vec<Entry>>,
    /// The providers are being asked.
    asking: AtomicBool,
    /// `[usage] redact_email`.
    redact: bool,
}

impl Tui {
    fn usage_source(&self) -> Option<Source> {
        let app = &self.backend.local()?.app;
        Source::of(&app.config, app.credentials.clone(), &app.paths).ok()
    }

    /// `/usage`: opens the box with what is known at once, and asks the providers about every
    /// account whose last answer is older than five minutes. `r` asks about all of them.
    pub(super) async fn open_usage(&mut self) {
        let Some(source) = self.usage_source() else {
            // A session in another process answers in text.
            return self.other_command("usage", "");
        };
        let shown = Arc::new(Shown {
            entries: Mutex::new(usage::collect(&source, Ask::Known).await),
            asking: AtomicBool::new(false),
            redact: self.env.config.usage.redact_email,
        });
        let drawn = shown.clone();
        let rows = Box::new(move |width: usize, theme: &azazel_tui::Theme| {
            let view = View {
                now: now_ms() / 1000,
                redact: drawn.redact,
            };
            let entries = drawn.entries.lock().unwrap_or_else(PoisonError::into_inner);
            usage::page(&entries, &view, width, theme)
        });
        let keys = vec![(KeyCombo::new(KeyCode::Char('r'), KeyModifiers::NONE), "refresh")];
        self.overlay = Some(Overlay::report(TITLE, rows, Action::Usage(shown)).with_keys(keys));
        self.ask_usage(Ask::Stale);
    }

    /// Asks the providers beside the UI loop; the open box shows the answers when they are in.
    pub(super) fn ask_usage(&mut self, ask: Ask) {
        let Some(source) = self.usage_source() else {
            return;
        };
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        let Action::Usage(shown) = &overlay.action else {
            return;
        };
        // One round at a time: a second key press while the first is out asks nothing more.
        if shown.asking.swap(true, Ordering::Relaxed) {
            return;
        }
        overlay.title = ASKING.to_string();
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let _ = done.send(Internal::Usage(usage::collect(&source, ask).await));
        });
    }

    /// The providers answered: the open box shows what they said.
    pub(super) fn on_usage(&mut self, entries: Vec<Entry>) {
        if let Some(overlay) = &mut self.overlay
            && let Action::Usage(shown) = &overlay.action
        {
            *shown.entries.lock().unwrap_or_else(PoisonError::into_inner) = entries;
            shown.asking.store(false, Ordering::Relaxed);
            overlay.title = TITLE.to_string();
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::{Account, Window};
    use azazel_tui::{Theme, text};
    use crossterm::event::KeyEvent;

    use super::*;
    use crate::tui::overlay::Outcome;

    /// A login with a window per name.
    fn entry(label: &str, windows: &[&str]) -> Entry {
        Entry {
            provider: "anthropic".into(),
            account: Account {
                label: label.into(),
                login: true,
                email: None,
                org: None,
            },
            in_use: label == "default",
            report: None,
            seen: windows
                .iter()
                .map(|name| Window::new(*name, 0.5, None, "allowed"))
                .collect(),
            problem: None,
            blocks: Vec::new(),
        }
    }

    fn popup(shown: &Arc<Shown>) -> Overlay {
        let drawn = shown.clone();
        let rows = Box::new(move |width: usize, theme: &Theme| {
            let view = View {
                now: 0,
                redact: drawn.redact,
            };
            usage::page(&drawn.entries.lock().unwrap(), &view, width, theme)
        });
        let keys = vec![(KeyCombo::new(KeyCode::Char('r'), KeyModifiers::NONE), "refresh")];
        Overlay::report(TITLE, rows, Action::Usage(shown.clone())).with_keys(keys)
    }

    fn drawn(overlay: &Overlay, rows: usize) -> Vec<String> {
        overlay
            .render(60, rows, &Theme::dark())
            .0
            .iter()
            .map(|line| text::strip_ansi(line))
            .collect()
    }

    fn press(overlay: &mut Overlay, code: KeyCode) -> Outcome {
        overlay.on_key(&KeyEvent::new(code, KeyModifiers::NONE))
    }

    #[test]
    fn the_box_scrolls_when_it_is_too_short_and_shows_new_answers_where_it_stands() {
        let shown = Arc::new(Shown {
            entries: Mutex::new(vec![
                entry("default", &["5h", "7d"]),
                entry("work", &["5h", "7d"]),
                entry("zed", &["5h", "7d"]),
            ]),
            asking: AtomicBool::new(false),
            redact: true,
        });
        let mut overlay = popup(&shown);

        // Tall enough: every account, no scrolling, and the keys in the bottom edge.
        let all = drawn(&overlay, 20);
        assert_eq!(all.len(), 12);
        assert!(all[0].starts_with("╭─ Usage ─"), "{}", all[0]);
        assert!(all[1].contains("anthropic"));
        assert!(all[2].contains("● default · in use"), "{}", all[2]);
        let edge = all.last().unwrap();
        assert!(edge.contains("r refresh") && edge.contains("esc close"), "{edge}");
        assert!(!edge.contains("scroll"), "{edge}");

        // Six rows of ten: the first ones, a scrollbar, and the keys that move it.
        let top = drawn(&overlay, 8);
        assert_eq!(top.len(), 8);
        assert!(top[1].contains("anthropic") && top[5].contains("work"), "{top:?}");
        assert!(drawn(&overlay, 8).last().unwrap().contains("scroll"));
        assert!(matches!(press(&mut overlay, KeyCode::Down), Outcome::Pending));
        // The rows without the scrollbar beside them.
        let text_of = |line: &str| line.chars().take(40).collect::<String>();
        let moved = drawn(&overlay, 8);
        assert_eq!(text_of(&moved[1]), text_of(&top[2]));
        assert!(matches!(press(&mut overlay, KeyCode::End), Outcome::Pending));
        let end = drawn(&overlay, 8);
        assert!(end[4].contains("zed") && end[6].contains("7d"), "{end:?}");
        // It cannot be scrolled past its end, nor before its start.
        press(&mut overlay, KeyCode::PageDown);
        assert_eq!(drawn(&overlay, 8), end);
        press(&mut overlay, KeyCode::Home);
        press(&mut overlay, KeyCode::Up);
        assert_eq!(drawn(&overlay, 8), top);

        // `r` is the box's own key and leaves it open. What the providers answer is shown
        // when it arrives, wherever the box was scrolled to.
        press(&mut overlay, KeyCode::End);
        assert!(matches!(
            press(&mut overlay, KeyCode::Char('r')),
            Outcome::Hotkey { index: 0, key: 0 }
        ));
        *shown.entries.lock().unwrap() = vec![entry("default", &["5h"])];
        let fresh = drawn(&overlay, 8);
        assert_eq!(fresh.len(), 5);
        assert!(fresh[3].contains("5h") && fresh[3].contains("50%"), "{fresh:?}");

        // There is nothing to choose: enter and esc both close it.
        assert!(matches!(press(&mut overlay, KeyCode::Enter), Outcome::Cancelled));
        assert!(matches!(press(&mut overlay, KeyCode::Esc), Outcome::Cancelled));
    }
}
