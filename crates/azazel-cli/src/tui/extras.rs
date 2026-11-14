//! What extensions add to the UI with `azazel.widget` and `azazel.shortcut`: lines above the
//! input, and keys of their own. What `azazel.status` sets is a segment of the status line
//! ([`super::statusline::Name::Extensions`]).
//!
//! All of it is kept by [`UiExtras`], a service of the session's process: a session that runs
//! in another process has its extensions there, and nothing of them is shown here.

use std::sync::Arc;

use azazel_core::{ShortcutHandler, UiExtras};
use azazel_tui::{Glyph, KeyCombo, Keymap, Line, Role, Theme, text};
use crossterm::event::KeyEvent;

use super::{Tui, input};

/// The most rows the widgets of all extensions take above the input.
const WIDGET_ROWS: usize = 10;

/// The lines of the widgets as rows above the input: one row a line, cut to `width`. A script
/// decides what the lines are, so nothing in them moves the cursor or changes colours.
pub(super) fn widget_rows(lines: &[String], width: usize, theme: &Theme) -> Vec<Line> {
    let ellipsis = theme.glyph(Glyph::Ellipsis);
    let clean: Vec<String> = lines.iter().map(|line| text::sanitize(line)).collect();
    let rows: Vec<&str> = clean.iter().flat_map(|line| line.split('\n')).collect();
    let row = |shown: &str| theme.paint(Role::Muted, &text::truncate(shown, width, ellipsis));
    if rows.len() <= WIDGET_ROWS {
        return rows.into_iter().map(row).collect();
    }
    // The last row says how much is not shown.
    let kept = WIDGET_ROWS - 1;
    let mut out: Vec<Line> = rows[..kept].iter().map(|shown| row(shown)).collect();
    out.push(row(&format!("{ellipsis} {} more lines", rows.len() - kept)));
    out
}

/// What an extension bound `key` to. A key azazel has an action for keeps that action, and a
/// key that types a character types it: neither is an extension's to take.
pub(super) fn shortcut(
    extras: &UiExtras,
    keymap: &Keymap,
    key: &KeyEvent,
) -> Option<Arc<dyn ShortcutHandler>> {
    if keymap.action_for(key).is_some() || input::typed_char(key).is_some() {
        return None;
    }
    // A script writes the key as it likes (`Ctrl+K`, `shift+alt+p`, `ctrl+return`): what it
    // means is compared, not how it is spelled.
    let pressed = KeyCombo::from_event(key);
    let (bound, _) = extras
        .shortcuts()
        .into_iter()
        .find(|(bound, _)| bound.parse::<KeyCombo>().is_ok_and(|combo| combo == pressed))?;
    extras.shortcut(&bound)
}

/// The keys extensions bound, for `/keys`: each with who bound it, and why it never runs when
/// it cannot.
pub(super) fn bindings(extras: &UiExtras, keymap: &Keymap) -> Vec<String> {
    extras
        .shortcuts()
        .iter()
        .map(|(key, source)| {
            let note = match key.parse::<KeyCombo>() {
                Err(_) => "  (no key is written like this: it never runs)".to_string(),
                Ok(combo) => {
                    let press = KeyEvent::new(combo.code, combo.mods);
                    match keymap.action_for(&press) {
                        Some(action) => format!("  (the key is bound to `{action}`: it never runs)"),
                        None if input::typed_char(&press).is_some() => {
                            "  (the key types a character: it never runs)".to_string()
                        }
                        None => String::new(),
                    }
                }
            };
            format!("{key:<20} {source}{note}")
        })
        .collect()
}

impl Tui {
    /// What the extensions of a session in this process show and bind.
    pub(super) fn extras(&self) -> Option<Arc<UiExtras>> {
        self.backend.local()?.app.registry.services.get::<UiExtras>()
    }

    /// Notices of a segment or a widget an extension set or removed.
    pub(super) fn extras_changes(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        self.extras().map(|extras| extras.subscribe())
    }

    /// The widgets of extensions, right above the input.
    pub(super) fn widget_lines(&self, width: usize) -> Vec<Line> {
        match self.extras() {
            Some(extras) => widget_rows(&extras.widget_lines(), width, &self.theme),
            None => Vec::new(),
        }
    }

    /// Runs what an extension bound `key` to, beside the UI: a handler may wait for a program
    /// or ask the user. False when the key is not an extension's.
    pub(super) fn extension_key(&self, key: &KeyEvent) -> bool {
        let Some(local) = self.backend.local() else {
            return false;
        };
        let registry = &local.app.registry;
        let handler = registry
            .services
            .get::<UiExtras>()
            .and_then(|extras| shortcut(&extras, &self.keymap, key));
        let Some(handler) = handler else {
            return false;
        };
        // The context commands of extensions run with: the services and the open session.
        let context = registry.bus.ctx();
        tokio::spawn(async move { handler.run(&context).await });
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: Vec<Line>) -> Vec<String> {
        lines.iter().map(|line| text::strip_ansi(line)).collect()
    }

    #[test]
    fn many_widget_lines_keep_to_their_rows_and_say_how_many_are_left_out() {
        let theme = Theme::dark();
        let lines: Vec<String> = (1..=14).map(|n| format!("line {n}")).collect();
        let rows = plain(widget_rows(&lines, 40, &theme));
        assert_eq!(rows.len(), WIDGET_ROWS);
        assert_eq!(rows[8], "line 9");
        assert_eq!(rows[9], "… 5 more lines");
        // A line of a script cannot reach the terminal as anything but text.
        let rows = widget_rows(&["\x1b[2Jtwo\nrows\x07".to_string()], 40, &theme);
        assert_eq!(plain(rows.clone()), ["two", "rows"]);
        assert!(
            rows.iter()
                .all(|row| !row.contains("\x1b[2J") && !row.contains('\x07'))
        );
    }

    /// With the scripts of a user loaded as a session loads them.
    #[cfg(feature = "lua")]
    mod scripted {
        use azazel_agent::Services;
        use azazel_core::config::StatuslineConfig;
        use azazel_core::{ExtCtx, Headless, Paths, Registry, Ui};
        use crossterm::event::{KeyCode, KeyModifiers};

        use super::super::super::backend::Billing;
        use super::super::super::statusline::{self, Layout, StatusLine};
        use super::*;

        /// A registry that has loaded `script` as an extension of the user, in a session `s1`.
        fn loaded(script: &str) -> (tempfile::TempDir, Registry) {
            let dir = tempfile::tempdir().unwrap();
            let (project, home) = (dir.path().join("project"), dir.path().join("home"));
            std::fs::create_dir_all(project.join(".azazel")).unwrap();
            std::fs::create_dir_all(home.join("extensions")).unwrap();
            std::fs::write(home.join("extensions").join("panel.luau"), script).unwrap();
            let paths = Paths::discover(&project, home.to_str());
            let services = Arc::new(Services::new());
            services.insert(Arc::new(Ui(Arc::new(Headless))));
            let registry = Registry::new(ExtCtx {
                services,
                cwd: paths.cwd.clone(),
                session_id: "s1".to_string(),
            });
            let credentials = Arc::new(azazel_ai::Credentials::new(paths.auth_file()));
            let scripts = azazel_lua::LuaExt::new(
                paths,
                false,
                credentials,
                Vec::new(),
                azazel_lua::Scripts::default(),
            );
            registry.load(&scripts).unwrap();
            (dir, registry)
        }

        fn press(code: KeyCode, mods: KeyModifiers) -> KeyEvent {
            KeyEvent::new(code, mods)
        }

        #[tokio::test]
        async fn what_a_script_shows_and_binds_is_on_the_line_above_the_input_and_on_its_key() {
            let (_dir, registry) = loaded(
                r#"
                azazel.status("ci", "ci passing")
                azazel.widget("plan", { "step one", "step two of the plan" })
                azazel.shortcut("Shift+Alt+K", function(ctx)
                    azazel.status("key", "pressed in " .. ctx.session_id)
                    azazel.widget("plan", "done")
                end)
                azazel.shortcut("ctrl+o", function() azazel.status("taken", "ctrl+o") end)
                azazel.shortcut("x", function() azazel.status("taken", "x") end)
                "#,
            );
            let extras = registry.services.get::<UiExtras>().unwrap();
            let (keymap, theme) = (Keymap::defaults(), Theme::dark());
            let (layout, _) = Layout::new(&StatuslineConfig::default());
            let line = |extras: &UiExtras| {
                let line = StatusLine {
                    extensions: extras.status(),
                    ..statusline::sample(&Billing::Metered)
                };
                text::strip_ansi(&line.render(&layout, None, 200, &theme))
            };

            // What the script set while it loaded: a segment after the facts of the session,
            // and rows above the input that are cut, not wrapped.
            assert!(
                line(&extras).contains("⚑ goal active · ci passing  "),
                "{}",
                line(&extras)
            );
            assert_eq!(
                plain(widget_rows(&extras.widget_lines(), 12, &theme)),
                ["step one", "step two of…"]
            );

            // Its key runs its handler with the open session, however the script spelled the
            // key; what the handler shows is a reason to draw.
            let mut changes = extras.subscribe();
            changes.mark_unchanged();
            let key = press(KeyCode::Char('K'), KeyModifiers::ALT | KeyModifiers::SHIFT);
            let handler = shortcut(&extras, &keymap, &key).unwrap();
            handler.run(&registry.bus.ctx()).await;
            assert!(changes.has_changed().unwrap());
            assert!(
                line(&extras).contains(" · ci passing · pressed in s1  "),
                "{}",
                line(&extras)
            );
            assert_eq!(plain(widget_rows(&extras.widget_lines(), 12, &theme)), ["done"]);

            // A key azazel binds stays azazel's, a key that types still types, and a key
            // nobody bound is nobody's.
            let ctrl_o = press(KeyCode::Char('o'), KeyModifiers::CONTROL);
            assert_eq!(keymap.action_for(&ctrl_o), Some("toggle_tool_output"));
            assert!(shortcut(&extras, &keymap, &ctrl_o).is_none());
            assert!(shortcut(&extras, &keymap, &press(KeyCode::Char('x'), KeyModifiers::NONE)).is_none());
            assert!(shortcut(&extras, &keymap, &press(KeyCode::Char('k'), KeyModifiers::ALT)).is_none());
            // Once the user gives `ctrl+o` to nothing, it is the script's.
            let mut unbound = Keymap::defaults();
            unbound.merge_toml("toggle_tool_output = []").unwrap();
            assert!(shortcut(&extras, &unbound, &ctrl_o).is_some());

            // `/keys` names each key with its script, and says which never run.
            assert_eq!(
                bindings(&extras, &keymap),
                [
                    "shift+alt+k          lua:panel",
                    "ctrl+o               lua:panel  (the key is bound to `toggle_tool_output`: it never runs)",
                    "x                    lua:panel  (the key types a character: it never runs)",
                ]
            );
        }
    }
}
