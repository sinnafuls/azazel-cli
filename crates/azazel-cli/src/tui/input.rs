//! What happens around the input line: the external editor, dropped files, the queue of
//! follow-up messages, the draft stash, Vim keys, and the keys that step through thinking levels
//! and favourite models.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};

use azazel_ai::{Block, ThinkingLevel};
use azazel_core::commands::split_args;
use azazel_core::config::EditorMode;
use azazel_core::media::{MAX_IMAGE_SIDE, image_block, image_mime};
use azazel_tools::Foreground;
use azazel_tui::{Draft, Glyph, GlyphLevel, Line, Role, Vim};
use crossterm::event::{Event, EventStream, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::{Tui, first_line, view};

/// Files larger than this are mentioned, never read to see whether they are images.
const MAX_DROPPED_IMAGE_BYTES: u64 = 20 * 1024 * 1024;
/// More paths than this in one paste is text about files, not a drop.
const MAX_DROPPED_FILES: usize = 32;
/// The models the cycling keys step through when no favourite is set.
const DEFAULT_CYCLE: [&str; 3] = ["@default", "@smol", "@slow"];
pub(super) const NO_EDITOR: &str = "no editor found: set $VISUAL or $EDITOR, or [editor] command";

/// Messages typed while the agent works, waiting for it to finish. The UI holds them, so that
/// each one is sent like a typed prompt (mentions expanded, checkpoint taken, prompt hooks run)
/// and can still be taken back.
#[derive(Default)]
pub(crate) struct Queue {
    waiting: VecDeque<(String, Vec<Block>)>,
}

impl Queue {
    pub(crate) fn push(&mut self, text: String, images: Vec<Block>) {
        self.waiting.push_back((text, images));
    }

    /// Takes back the message queued last.
    pub(crate) fn pop_last(&mut self) -> Option<(String, Vec<Block>)> {
        self.waiting.pop_back()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.waiting.is_empty()
    }

    pub(crate) fn len(&self) -> usize {
        self.waiting.len()
    }

    fn texts(&self) -> impl Iterator<Item = &str> {
        self.waiting.iter().map(|(text, _)| text.as_str())
    }

    /// Takes the oldest message, to be sent as a prompt of its own.
    pub(crate) fn next(&mut self) -> Option<(String, Vec<Block>)> {
        self.waiting.pop_front()
    }
}

/// For tests, which have no terminal to type into: the messages of `AZAZEL_TEST_QUEUE`, one per
/// line, queued the way `alt+enter` queues them.
#[cfg(feature = "test-support")]
pub(crate) fn queue_from_env() -> Queue {
    let mut queue = Queue::default();
    for line in std::env::var("AZAZEL_TEST_QUEUE").unwrap_or_default().lines() {
        if !line.trim().is_empty() {
            queue.push(line.to_string(), Vec::new());
        }
    }
    queue
}

/// How long the next key of a paste may take to arrive before the burst counts as over.
const BURST_GAP: std::time::Duration = std::time::Duration::from_millis(10);
/// Fewer keys than this were typed, however fast they arrived.
const BURST_MIN_KEYS: usize = 4;
/// A burst this long is a paste even when it is one line that ends with enter.
const BURST_LINE_KEYS: usize = 32;
/// Keys read in one burst; a longer paste arrives as several.
const BURST_MAX_KEYS: usize = 1 << 20;

/// The character a key press types, for telling pasted text from commands: a letter without
/// `ctrl` or `alt` (both together are AltGr), `enter` as a line break, `tab`.
pub(super) fn typed_char(key: &KeyEvent) -> Option<char> {
    if key.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    let plain = key.modifiers.difference(KeyModifiers::SHIFT).is_empty();
    match key.code {
        KeyCode::Char(c) if ctrl == alt => Some(c),
        KeyCode::Enter if plain => Some('\n'),
        KeyCode::Tab if plain => Some('\t'),
        _ => None,
    }
}

/// Reads the key presses that are already waiting behind `first`. A console without bracketed
/// paste (Windows) delivers a paste this way: as key presses that arrive faster than anyone
/// types. Returns what the keys type, and the event that ended the burst when it was not a key
/// that types.
pub(super) fn read_burst(first: char) -> (String, Option<Event>) {
    let mut text = String::from(first);
    let mut wait = std::time::Duration::ZERO;
    while text.len() < BURST_MAX_KEYS && matches!(crossterm::event::poll(wait), Ok(true)) {
        // The first look is free, so ordinary typing is never delayed.
        wait = BURST_GAP;
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Release => {}
            Ok(Event::Key(key)) => match typed_char(&key) {
                Some(c) => text.push(c),
                None => return (text, Some(Event::Key(key))),
            },
            Ok(other) => return (text, Some(other)),
            Err(_) => break,
        }
    }
    (text, None)
}

/// Whether a burst of key presses was pasted rather than typed. A few keys that queued up while
/// the UI was busy are typing, and so is a short line that ends with `enter`: it is sent, as it
/// would be in a shell.
pub(super) fn is_paste(burst: &str) -> bool {
    let keys = burst.chars().count();
    let body = burst.strip_suffix('\n').unwrap_or(burst);
    keys >= BURST_MIN_KEYS && (body.contains('\n') || body.len() == burst.len() || keys >= BURST_LINE_KEYS)
}

/// A typed character as the key press it came from.
pub(super) fn key_for(c: char) -> KeyEvent {
    match c {
        '\n' => KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE),
        '\t' => KeyEvent::new(KeyCode::Tab, KeyModifiers::NONE),
        c => KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE),
    }
}

/// The program and arguments that open a file for editing: `[editor] command`, else `$VISUAL`,
/// else `$EDITOR`, else Notepad on Windows and `vi` elsewhere.
pub(super) fn editor_command(
    configured: &str,
    visual: Option<String>,
    editor: Option<String>,
) -> Vec<String> {
    let fallback = if cfg!(windows) { "notepad" } else { "vi" };
    [Some(configured.to_string()), visual, editor]
        .into_iter()
        .flatten()
        .map(|command| split_args(&command))
        .find(|words| !words.is_empty())
        .unwrap_or_else(|| vec![fallback.to_string()])
}

fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut at = 0;
    while at < bytes.len() {
        let hex = (bytes[at] == b'%')
            .then(|| text.get(at + 1..at + 3))
            .flatten()
            .and_then(|pair| u8::from_str_radix(pair, 16).ok());
        match hex {
            Some(byte) => {
                out.push(byte);
                at += 3;
            }
            None => {
                out.push(bytes[at]);
                at += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Pasted text split the way a shell would: at white space, with `'…'`, `"…"` and (outside
/// Windows, where it separates path components) backslash escapes keeping words together.
fn shell_words(text: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut started = false;
    let mut quote: Option<char> = None;
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) if c == q => quote = None,
            Some(_) => word.push(c),
            None if c == '\'' || c == '"' => {
                quote = Some(c);
                started = true;
            }
            None if c == '\\' && !cfg!(windows) => {
                word.extend(chars.next());
                started = true;
            }
            None if c.is_whitespace() => {
                if started {
                    words.push(std::mem::take(&mut word));
                    started = false;
                }
            }
            None => {
                word.push(c);
                started = true;
            }
        }
    }
    if started {
        words.push(word);
    }
    words
}

/// One dropped word as a path: `file://` URLs are decoded and `~/` is the home directory.
/// `None` for anything that is not an absolute path, because a pasted word that happens to name
/// a file in the project is far more often just text.
fn dropped_path(word: &str, home: Option<&Path>) -> Option<PathBuf> {
    let path = if let Some(rest) = word.strip_prefix("file://") {
        let decoded = percent_decode(rest.strip_prefix("localhost").unwrap_or(rest));
        // `file:///C:/x` on Windows: the slash before the drive letter is not part of the path.
        let windows_drive = decoded.as_bytes().get(2) == Some(&b':') && decoded.starts_with('/');
        PathBuf::from(if windows_drive { &decoded[1..] } else { &decoded })
    } else if let Some(rest) = word.strip_prefix("~/") {
        home?.join(rest)
    } else {
        PathBuf::from(word)
    };
    (path.is_absolute() && path.is_file()).then_some(path)
}

/// The files a paste names, when it is nothing but existing files: what a terminal types when
/// files are dropped on it.
pub(super) fn dropped_files(pasted: &str, home: Option<&Path>) -> Option<Vec<PathBuf>> {
    let pasted = pasted.trim();
    if pasted.is_empty() || pasted.len() > 16 * 1024 {
        return None;
    }
    // One path with spaces, dropped without quotes.
    if !pasted.contains('\n')
        && let Some(path) = dropped_path(pasted, home)
    {
        return Some(vec![path]);
    }
    let words = shell_words(pasted);
    if words.is_empty() || words.len() > MAX_DROPPED_FILES {
        return None;
    }
    words.iter().map(|word| dropped_path(word, home)).collect()
}

/// A file as an `@` mention: relative to `cwd` when it is inside it, quoted when it has spaces.
pub(super) fn mention(path: &Path, cwd: &Path) -> String {
    let shown = path.strip_prefix(cwd).unwrap_or(path);
    let shown = shown.to_string_lossy().replace('\\', "/");
    if shown.contains(char::is_whitespace) {
        format!("@\"{shown}\"")
    } else {
        format!("@{shown}")
    }
}

/// The level after `current` among those `supported`, wrapping around. `off` is always one of
/// them; a model that says nothing about its levels is offered every level.
pub(super) fn next_thinking(current: ThinkingLevel, supported: &[ThinkingLevel]) -> ThinkingLevel {
    let levels: Vec<ThinkingLevel> = if supported.is_empty() {
        ThinkingLevel::ALL.to_vec()
    } else {
        ThinkingLevel::ALL
            .into_iter()
            .filter(|level| *level == ThinkingLevel::Off || supported.contains(level))
            .collect()
    };
    let at = levels.iter().position(|level| *level >= current).unwrap_or(0);
    if levels.get(at) == Some(&current) {
        levels[(at + 1) % levels.len()]
    } else {
        levels[at]
    }
}

/// The model to switch to from `current` among `cycle`, a step forwards or backwards. From a
/// model that is not in the cycle, the first (or last) one.
pub(super) fn next_model<'a>(cycle: &'a [String], current: &str, forward: bool) -> Option<&'a str> {
    let count = cycle.len();
    let next = match cycle.iter().position(|spec| spec == current) {
        Some(at) if forward => (at + 1) % count.max(1),
        Some(at) => (at + count - 1) % count.max(1),
        None if forward => 0,
        None => count.saturating_sub(1),
    };
    cycle.get(next).map(String::as_str)
}

impl Tui {
    // ------------------------------------------------------------ the rule above the input

    /// What rides the rule above the input: the Vim mode, and what the lead is doing.
    pub(super) fn rule_status(&self) -> Option<String> {
        let mode = self
            .vim
            .as_ref()
            .and_then(Vim::label)
            .map(|label| self.theme.paint(Role::Accent, &label));
        match (mode, self.activity()) {
            (Some(mode), Some(activity)) => Some(format!("{mode} {activity}")),
            (mode, activity) => mode.or(activity),
        }
    }

    /// One muted row per queued message and one for stashed drafts, shown above the input.
    pub(super) fn waiting_lines(&self, width: usize) -> Vec<Line> {
        let glyphs = self.theme.glyphs();
        let muted = |row: &str| {
            let cut = azazel_tui::text::truncate(row, width, glyphs.get(Glyph::Ellipsis));
            self.theme.paint(Role::Muted, &cut)
        };
        let mut lines: Vec<Line> = self
            .queued
            .texts()
            .map(|text| {
                muted(&format!(
                    "{}queued: {}",
                    glyphs.prefix(Glyph::Queued),
                    first_line(text)
                ))
            })
            .collect();
        if !self.stashed.is_empty() {
            let count = self.stashed.len();
            lines.push(muted(&format!(
                "{count} draft{} stashed{}ctrl+s on an empty input brings {} back",
                if count == 1 { "" } else { "s" },
                glyphs.get(Glyph::Separator),
                if count == 1 { "it" } else { "the last one" }
            )));
        }
        lines
    }

    // ------------------------------------------------------------ queue and stash

    /// Holds a message typed while the agent works until it finishes.
    pub(super) fn queue_message(&mut self, text: String) {
        let images = std::mem::take(&mut self.images);
        self.queued.push(text, images);
        self.notice = Some(format!(
            "queued until the agent finishes ({} waiting){}alt+up takes it back",
            self.queued.len(),
            self.theme.glyph(Glyph::Separator)
        ));
    }

    /// After the lead went idle: sends the next queued message as a prompt of its own.
    pub(super) async fn send_queued(&mut self) {
        if !std::mem::take(&mut self.run_ended) || self.busy {
            return;
        }
        let Some((text, images)) = self.queued.next() else {
            return;
        };
        let sent = self.backend.prompt(text.clone(), images).await;
        self.notice = None;
        self.prompts.push(text.clone());
        self.remember(&text);
        let lines = view::user_prompt(&text, self.width(), &self.theme);
        self.say(lines);
        if let Err(reason) = sent {
            self.fail(&format!("prompt rejected: {reason}"));
        }
    }

    /// Takes the last queued message back into the input, with its images.
    pub(super) fn dequeue(&mut self) {
        let Some((text, mut images)) = self.queued.pop_last() else {
            return;
        };
        // What was being typed is not lost: it waits in the stash.
        if !self.editor.is_empty() {
            self.stashed.push((self.editor.take_draft(), Vec::new()));
        }
        self.editor.set_text(&text);
        self.images.append(&mut images);
        self.completion = None;
    }

    /// `ctrl+s`: parks the draft, or on an empty input brings the last parked one back.
    pub(super) fn stash(&mut self) {
        if self.editor.is_empty() && self.images.is_empty() {
            match self.stashed.pop() {
                Some((draft, images)) => {
                    self.editor.restore_draft(draft);
                    self.images = images;
                }
                None => self.notice = Some("no draft is stashed".to_string()),
            }
            return;
        }
        // The row above the input counts what is parked.
        let draft: Draft = self.editor.take_draft();
        self.stashed.push((draft, std::mem::take(&mut self.images)));
        self.completion = None;
    }

    // ------------------------------------------------------------ pastes

    /// Pasted text: dropped files become attachments and mentions, a long paste folds into a
    /// token, anything else goes in as typed.
    pub(super) fn on_paste(&mut self, pasted: &str) {
        if let Some(overlay) = &mut self.overlay {
            return overlay.on_paste(pasted);
        }
        if self.term_paste(pasted) {
            return;
        }
        if let Some(panel) = &mut self.btw {
            return panel.on_paste(pasted);
        }
        // What is pasted is for the input, wherever the keyboard was.
        self.fleet.focus = None;
        let home = std::env::home_dir();
        match dropped_files(pasted, home.as_deref()) {
            Some(files) => self.drop_files(&files),
            None => self.editor.insert_paste(pasted, self.fold_paste_lines),
        }
        self.refresh_completion();
    }

    fn drop_files(&mut self, files: &[PathBuf]) {
        let mut attached = 0;
        for path in files {
            let small = std::fs::metadata(path).is_ok_and(|meta| meta.len() <= MAX_DROPPED_IMAGE_BYTES);
            let image = small
                .then(|| std::fs::read(path).ok())
                .flatten()
                .filter(|bytes| image_mime(bytes).is_some())
                .and_then(|bytes| image_block(&bytes, MAX_IMAGE_SIDE).ok());
            match image {
                Some(block) => {
                    self.images.push(block);
                    attached += 1;
                }
                None => {
                    // A session elsewhere does not have this machine's files under its own
                    // directory: the path is given as it is here.
                    let mention = match self.local_cwd() {
                        Some(cwd) => mention(path, &cwd),
                        None => path.display().to_string(),
                    };
                    self.editor.insert_str(&format!("{mention} "));
                }
            }
        }
        if attached > 0 {
            self.notice = Some(format!(
                "attached {attached} image{}",
                if attached == 1 { "" } else { "s" }
            ));
        }
    }

    // ------------------------------------------------------------ external editor

    /// Opens the draft (or the text of an input overlay) in the user's editor and takes back
    /// what they saved.
    pub(super) async fn external_editor(&mut self) {
        let draft = match &mut self.overlay {
            Some(overlay) => match overlay.editor_mut() {
                Some(editor) => editor.expanded(),
                None => return,
            },
            None => self.editor.expanded(),
        };
        let file = self
            .env
            .paths
            .cache_dir()
            .join(format!("prompt-{}.md", std::process::id()));
        let written = file
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&file, &draft));
        if let Err(e) = written {
            return self.fail(&format!("could not write {}: {e}", file.display()));
        }
        let words = editor_command(
            &self.editor_command,
            std::env::var("VISUAL").ok(),
            std::env::var("EDITOR").ok(),
        );
        let Some((program, args)) = words.split_first() else {
            return self.fail(NO_EDITOR);
        };

        // The editor gets the terminal as a shell would hand it over: cooked, no bracketed
        // paste, no enhanced keys, and nobody else reading its input.
        let _ = self.renderer.suspend();
        self.terminal = None;
        self.guard.suspend();
        let status = tokio::process::Command::new(program)
            .args(args)
            .arg(&file)
            .status()
            .await;
        self.guard.resume();
        self.terminal = Some(EventStream::new());

        let edited = std::fs::read_to_string(&file);
        let _ = std::fs::remove_file(&file);
        match status {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return self.fail(NO_EDITOR),
            Err(e) => return self.fail(&format!("could not start {program}: {e}")),
            Ok(status) if !status.success() => {
                return self.warn(&format!("{program} ended with {status}; the draft is unchanged"));
            }
            Ok(_) => {}
        }
        let edited = match edited {
            Ok(edited) => edited,
            Err(e) => return self.fail(&format!("could not read the draft back: {e}")),
        };
        // Editors end a file with a line break the prompt does not want.
        let edited = edited.trim_end_matches(['\n', '\r']);
        match &mut self.overlay {
            Some(overlay) => {
                if let Some(editor) = overlay.editor_mut() {
                    editor.set_text(edited);
                }
            }
            None => {
                self.editor.set_text(edited);
                self.completion = None;
            }
        }
    }

    // ------------------------------------------------------------ Vim keys

    pub(super) fn set_vim(&mut self, on: bool) {
        self.vim = on.then(Vim::new);
    }

    /// `/vim`: switches Vim keys on or off and keeps the choice for later sessions.
    pub(super) fn toggle_vim(&mut self) {
        let on = self.vim.is_none();
        self.set_vim(on);
        let mode = if on { EditorMode::Vim } else { EditorMode::Emacs };
        let saved = crate::init::set_config_value(
            &self.env.paths.home_config(),
            &["editor", "mode"],
            Some(toml_edit::value(mode.to_string())),
        );
        match saved {
            Ok(()) if on => self.info("Vim keys are on: esc leaves insert mode, i returns to it"),
            Ok(()) => self.info("Vim keys are off"),
            Err(e) => self.warn(&format!("Vim keys changed for this session only: {e:#}")),
        }
    }

    // ------------------------------------------------------------ thinking and model keys

    /// Steps to the next thinking level the current model supports.
    pub(super) async fn cycle_thinking(&mut self) {
        let supported = self.backend.thinking_levels();
        let level = next_thinking(self.backend.status().thinking, &supported);
        match self.backend.set_thinking(level).await {
            Ok(()) => self.notice = Some(format!("thinking: {level}")),
            Err(why) => self.fail(&why),
        }
    }

    /// Changes the lead's thinking level; the session records it, so that it comes back on it
    /// when it is resumed.
    pub(super) async fn set_thinking(&mut self, level: ThinkingLevel) {
        match self.backend.set_thinking(level).await {
            Ok(()) => self.info(&format!("thinking: {level}")),
            Err(why) => self.fail(&why),
        }
    }

    /// The models the cycling keys step through.
    fn model_cycle(&self) -> Vec<String> {
        if self.favourites.is_empty() {
            DEFAULT_CYCLE.iter().map(|spec| spec.to_string()).collect()
        } else {
            self.favourites.clone()
        }
    }

    /// Switches to the next or previous favourite model.
    pub(super) async fn cycle_model(&mut self, forward: bool) {
        let cycle: Vec<String> = self
            .model_cycle()
            .into_iter()
            .filter(|spec| self.backend.resolves(spec))
            .collect();
        let chain = self.backend.status().chain;
        let current = match chain.as_slice() {
            [only] => only.to_string(),
            _ => String::new(),
        };
        match next_model(&cycle, &current, forward) {
            Some(spec) if spec != current => {
                let spec = spec.to_string();
                self.set_model(&[spec.as_str()]).await;
            }
            Some(_) => {
                self.notice =
                    Some("there is one model to cycle through; ctrl+f in /model adds favourites".into())
            }
            None => self.notice = Some("no favourite model can be used; ctrl+f in /model adds one".into()),
        }
    }

    /// Adds `spec` to the favourites or removes it, and saves the list. Returns whether it is a
    /// favourite now.
    pub(super) fn toggle_favourite(&mut self, spec: &str) -> bool {
        let added = match self.favourites.iter().position(|favourite| favourite == spec) {
            Some(at) => {
                self.favourites.remove(at);
                false
            }
            None => {
                self.favourites.push(spec.to_string());
                true
            }
        };
        let list = toml_edit::Array::from_iter(self.favourites.iter().map(String::as_str));
        if let Err(e) = crate::init::set_config_value(
            &self.env.paths.home_config(),
            &["tui", "favourites"],
            Some(toml_edit::value(list)),
        ) {
            self.warn(&format!("the favourites were not saved: {e:#}"));
        }
        added
    }

    // ------------------------------------------------------------ running commands

    /// The tool call of the lead's shell command that is running in the foreground, if any.
    pub(super) fn foreground_call(&self) -> Option<String> {
        let foreground = self.backend.local()?.app.registry.services.get::<Foreground>()?;
        let running = foreground.running();
        self.tools
            .iter()
            .rev()
            .chain(self.followed_tools.iter().rev())
            .find(|tool| running.iter().any(|(call_id, _)| *call_id == tool.call_id))
            .map(|tool| tool.call_id.clone())
    }

    /// Moves the running shell command to the background; the agent's call returns with a job id.
    pub(super) fn background_shell(&mut self, call_id: &str) {
        let moved = self
            .backend
            .local()
            .and_then(|local| local.app.registry.services.get::<Foreground>())
            .is_some_and(|foreground| foreground.background(call_id));
        self.notice = Some(
            if moved {
                "moving the command to the background; /jobs lists it"
            } else {
                "the command has just finished"
            }
            .to_string(),
        );
    }

    // ------------------------------------------------------------ settings that apply at once

    /// Applies a setting the settings menu just saved to the running UI. The appearance
    /// settings are kept in the UI's own copy of the configuration, where whatever draws reads
    /// them.
    pub(super) fn apply_setting(&mut self, path: &str, value: &toml::Value) {
        let tui = &mut self.env.config.tui;
        match (path, value) {
            ("tui.theme", toml::Value::String(name)) => self.set_theme(name),
            ("tui.glyphs", toml::Value::String(set)) => {
                if let Ok(setting) = set.parse() {
                    tui.glyphs = setting;
                }
                // `AZAZEL_GLYPHS` still wins, as it did when the UI opened.
                let level = GlyphLevel::detect(set, |name| std::env::var(name).ok());
                self.theme = self.theme.clone().with_glyphs(level);
            }
            ("tui.indicator", toml::Value::String(mode)) => {
                if let Ok(mode) = mode.parse() {
                    tui.indicator = mode;
                }
            }
            ("tui.session_accent", toml::Value::Boolean(on)) => tui.session_accent = *on,
            ("tui.tool_style", toml::Value::String(style)) => {
                if let Ok(style) = style.parse() {
                    tui.tool_style = style;
                }
            }
            ("tui.redact", toml::Value::Boolean(on)) => tui.redact = *on,
            ("tui.popups", toml::Value::Boolean(on)) => tui.popups = *on,
            ("tui.show_thinking", toml::Value::Boolean(on)) => self.show_thinking = *on,
            ("tui.expand_tools", toml::Value::Boolean(on)) => self.expand_tools = *on,
            ("editor.mode", toml::Value::String(mode)) => self.set_vim(mode == "vim"),
            ("editor.command", toml::Value::String(command)) => self.editor_command = command.clone(),
            ("editor.fold_paste_lines", toml::Value::Integer(lines)) => {
                self.fold_paste_lines = usize::try_from(*lines).unwrap_or(0);
            }
            _ => {}
        }
        self.restyle_calls();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_editor_is_the_setting_then_visual_then_editor_then_a_default() {
        let fallback = if cfg!(windows) { "notepad" } else { "vi" };
        assert_eq!(editor_command("", None, None), [fallback]);
        assert_eq!(
            editor_command("", Some("code --wait".into()), Some("nano".into())),
            ["code", "--wait"]
        );
        assert_eq!(editor_command("", None, Some("nano".into())), ["nano"]);
        assert_eq!(
            editor_command("hx -c 'my config'", Some("code".into()), None),
            ["hx", "-c", "my config"]
        );
        // Blank values are skipped, not run.
        assert_eq!(
            editor_command("  ", Some(String::new()), Some("vim".into())),
            ["vim"]
        );
    }

    #[test]
    fn a_paste_of_existing_absolute_paths_is_a_drop() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("notes.txt");
        let spaced = dir.path().join("my file.md");
        std::fs::write(&plain, "x").unwrap();
        std::fs::write(&spaced, "y").unwrap();
        let (plain_text, spaced_text) = (plain.to_string_lossy(), spaced.to_string_lossy());

        assert_eq!(dropped_files(&plain_text, None), Some(vec![plain.clone()]));
        assert_eq!(
            dropped_files(&format!("  {plain_text}\n"), None),
            Some(vec![plain.clone()])
        );
        // A path with spaces arrives bare, quoted, or with the other file beside it.
        assert_eq!(dropped_files(&spaced_text, None), Some(vec![spaced.clone()]));
        assert_eq!(
            dropped_files(&format!("\"{spaced_text}\" '{plain_text}'"), None),
            Some(vec![spaced.clone(), plain.clone()])
        );
        assert_eq!(
            dropped_files(&format!("{plain_text}\n\"{spaced_text}\""), None),
            Some(vec![plain.clone(), spaced.clone()])
        );
        // A file URL, with its spaces encoded.
        let url = format!(
            "file://{}{}",
            if cfg!(windows) { "/" } else { "" },
            spaced_text.replace('\\', "/").replace(' ', "%20")
        );
        assert_eq!(
            dropped_files(&url, None).map(|files| files.len()),
            Some(1),
            "{url}"
        );
        // The home directory, written the short way.
        assert_eq!(
            dropped_files("~/notes.txt", Some(dir.path())),
            Some(vec![dir.path().join("notes.txt")])
        );

        // Text is text: a missing file, a directory, a relative name, prose around a path.
        assert_eq!(dropped_files(&format!("{plain_text}.missing"), None), None);
        assert_eq!(dropped_files(&dir.path().to_string_lossy(), None), None);
        assert_eq!(dropped_files("notes.txt", None), None);
        assert_eq!(dropped_files(&format!("look at {plain_text}"), None), None);
        assert_eq!(dropped_files("", None), None);
    }

    #[cfg(not(windows))]
    #[test]
    fn backslashes_escape_spaces_in_dropped_paths() {
        let dir = tempfile::tempdir().unwrap();
        let spaced = dir.path().join("my file.md");
        let other = dir.path().join("b.txt");
        std::fs::write(&spaced, "y").unwrap();
        std::fs::write(&other, "z").unwrap();
        let escaped = format!(
            "{} {}",
            spaced.to_string_lossy().replace(' ', "\\ "),
            other.to_string_lossy()
        );
        assert_eq!(dropped_files(&escaped, None), Some(vec![spaced, other]));
    }

    #[test]
    fn mentions_are_relative_inside_the_project_and_quoted_when_spaced() {
        let cwd = Path::new(if cfg!(windows) {
            "C:\\work\\proj"
        } else {
            "/work/proj"
        });
        assert_eq!(mention(&cwd.join("src").join("main.rs"), cwd), "@src/main.rs");
        assert_eq!(mention(&cwd.join("my notes.md"), cwd), "@\"my notes.md\"");
        let outside = Path::new(if cfg!(windows) {
            "D:\\other\\a.txt"
        } else {
            "/other/a.txt"
        });
        assert_eq!(
            mention(outside, cwd),
            format!("@{}", outside.to_string_lossy().replace('\\', "/"))
        );
    }

    #[test]
    fn thinking_steps_through_supported_levels_and_wraps() {
        use ThinkingLevel::{High, Low, Max, Medium, Minimal, Off, Xhigh};
        let supported = [Low, High];
        assert_eq!(next_thinking(Off, &supported), Low);
        assert_eq!(next_thinking(Low, &supported), High);
        assert_eq!(next_thinking(High, &supported), Off);
        // From a level the model does not have, the next one up that it has.
        assert_eq!(next_thinking(Medium, &supported), High);
        assert_eq!(next_thinking(Max, &supported), Off);
        // A model that lists nothing gets every level.
        let mut level = Off;
        let mut seen = Vec::new();
        for _ in 0..7 {
            level = next_thinking(level, &[]);
            seen.push(level);
        }
        assert_eq!(seen, [Minimal, Low, Medium, High, Xhigh, Max, Off]);
    }

    #[test]
    fn models_cycle_both_ways_through_the_favourites() {
        let cycle = vec!["@default".to_string(), "@smol".to_string(), "@slow".to_string()];
        assert_eq!(next_model(&cycle, "@default", true), Some("@smol"));
        assert_eq!(next_model(&cycle, "@slow", true), Some("@default"));
        assert_eq!(next_model(&cycle, "@default", false), Some("@slow"));
        assert_eq!(next_model(&cycle, "@smol", false), Some("@default"));
        // From a model outside the cycle: the first going forwards, the last going back.
        assert_eq!(next_model(&cycle, "anthropic/x", true), Some("@default"));
        assert_eq!(next_model(&cycle, "anthropic/x", false), Some("@slow"));
        assert_eq!(next_model(&[], "@default", true), None);
    }

    #[test]
    fn a_burst_of_keys_is_a_paste_unless_it_reads_like_typing() {
        // A few keys that queued up, and a short line with enter at its end, were typed.
        assert!(!is_paste("ab"));
        assert!(!is_paste("yes\n"));
        assert!(!is_paste("cargo test --workspace\n"));
        // Text without enter, several lines, or a long line were pasted.
        assert!(is_paste("C:\\Users\\me\\a.png"));
        assert!(is_paste("one\ntwo\n"));
        assert!(is_paste("one\ntwo"));
        assert!(is_paste(&format!("{}\n", "x".repeat(40))));

        let press = |code, modifiers| KeyEvent::new(code, modifiers);
        assert_eq!(
            typed_char(&press(KeyCode::Char('a'), KeyModifiers::NONE)),
            Some('a')
        );
        assert_eq!(
            typed_char(&press(KeyCode::Char('A'), KeyModifiers::SHIFT)),
            Some('A')
        );
        assert_eq!(
            typed_char(&press(
                KeyCode::Char('@'),
                KeyModifiers::CONTROL | KeyModifiers::ALT
            )),
            Some('@'),
            "AltGr characters are text"
        );
        assert_eq!(typed_char(&press(KeyCode::Enter, KeyModifiers::NONE)), Some('\n'));
        assert_eq!(
            typed_char(&press(KeyCode::Char('c'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(typed_char(&press(KeyCode::Enter, KeyModifiers::ALT)), None);
        assert_eq!(typed_char(&press(KeyCode::Up, KeyModifiers::NONE)), None);
        let release = KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Release);
        assert_eq!(typed_char(&release), None);
        assert_eq!(key_for('\n').code, KeyCode::Enter);
        assert_eq!(key_for('x').code, KeyCode::Char('x'));
    }

    #[test]
    fn shell_words_keep_quoted_text_together() {
        assert_eq!(shell_words("a 'b c'  \"d e\" f"), ["a", "b c", "d e", "f"]);
        assert_eq!(shell_words("''"), [""]);
        assert_eq!(shell_words("   "), Vec::<String>::new());
        assert_eq!(percent_decode("a%20b%2Fc%zz%"), "a b/c%zz%");
    }
}
