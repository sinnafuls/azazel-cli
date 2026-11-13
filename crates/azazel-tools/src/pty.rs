//! Programs in a real terminal. `shell` with `interactive: true` starts a command in a
//! pseudo-terminal (ConPTY on Windows) whose screen is emulated here; the `pty` tool reads that
//! screen, types into it and stops the program, and the interface shows the screen live and
//! lets the user type into it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::File;
use std::io::{Read, Write};
use std::num::NonZeroU16;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, mpsc as thread_channel};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::process::Shell;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::util::parse_input;

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The screen a program gets until an interface that shows it gives it another: rows, columns.
pub const DEFAULT_SIZE: (u16, u16) = (30, 100);
/// The smallest screen a program is given.
const MIN_SIZE: (u16, u16) = (4, 20);
/// Programs that may run in terminals at once.
const MAX_RUNNING: usize = 8;
/// How long a program that was just started is given to draw something.
const START_OUTPUT: Duration = Duration::from_secs(5);
/// How long a program is given to react to what was typed. One that echoes nothing (a
/// password prompt) costs this much.
const SEND_OUTPUT: Duration = Duration::from_secs(1);
/// The screen has settled when nothing was written to it for this long.
const QUIET: Duration = Duration::from_millis(200);
/// The longest a start or a `send` waits for the screen to settle once it has begun to change.
const SETTLE_LIMIT: Duration = Duration::from_secs(5);
/// How long `send` waits for the user to hand the keyboard back.
const HANDBACK_LIMIT: Duration = Duration::from_secs(120);
/// How long what a program wrote last is waited for once it has ended.
const DRAIN_LIMIT: Duration = Duration::from_secs(2);
const DEFAULT_READ_WAIT: Duration = Duration::from_secs(30);
const MAX_READ_WAIT: Duration = Duration::from_secs(1800);

fn cell_count(n: u16) -> NonZeroU16 {
    NonZeroU16::new(n).unwrap_or(NonZeroU16::MIN)
}

// ------------------------------------------------------------------ keys

/// A key that is not a character of text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Char(char),
    Enter,
    Tab,
    Esc,
    Backspace,
    Delete,
    Insert,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// `F(1)` to `F(12)`.
    F(u8),
}

/// The modifier keys held with a [`Key`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Mods {
    pub ctrl: bool,
    pub alt: bool,
    pub shift: bool,
}

impl Mods {
    /// The number xterm puts into a key's sequence for these modifiers; `None` without any.
    fn parameter(self) -> Option<u8> {
        let bits = u8::from(self.shift) + 2 * u8::from(self.alt) + 4 * u8::from(self.ctrl);
        (bits > 0).then_some(bits + 1)
    }
}

/// A key by its name: `enter`, `tab`, `esc`, `up`, `pagedown`, `f5`, `ctrl+c`, `alt+x`,
/// `shift+tab`, `ctrl+left`, a single character. `None` for anything else.
pub fn parse_key(name: &str) -> Option<(Key, Mods)> {
    let lower = name.trim().to_ascii_lowercase();
    let mut mods = Mods::default();
    let mut rest = lower.as_str();
    loop {
        let (modifier, after) = match rest.split_once('+') {
            // `ctrl++` and a bare `+` name the plus key.
            Some((modifier, after)) if !modifier.is_empty() && !after.is_empty() => (modifier, after),
            _ => break,
        };
        match modifier.trim() {
            "ctrl" | "control" => mods.ctrl = true,
            "alt" | "meta" | "option" => mods.alt = true,
            "shift" => mods.shift = true,
            _ => return None,
        }
        rest = after.trim_start();
    }
    let key = match rest {
        "enter" | "return" | "submit" => Key::Enter,
        "tab" => Key::Tab,
        "backtab" => {
            mods.shift = true;
            Key::Tab
        }
        "esc" | "escape" => Key::Esc,
        "backspace" | "bs" => Key::Backspace,
        "delete" | "del" => Key::Delete,
        "insert" | "ins" => Key::Insert,
        "up" => Key::Up,
        "down" => Key::Down,
        "left" => Key::Left,
        "right" => Key::Right,
        "home" => Key::Home,
        "end" => Key::End,
        "pageup" | "pgup" => Key::PageUp,
        "pagedown" | "pgdn" | "pgdown" => Key::PageDown,
        "space" => Key::Char(' '),
        other => {
            if let Some(number) = other.strip_prefix('f').and_then(|n| n.parse::<u8>().ok()) {
                if !(1..=12).contains(&number) {
                    return None;
                }
                Key::F(number)
            } else {
                if other.chars().count() != 1 {
                    return None;
                }
                // The character as it was written: `G` is not `g`.
                Key::Char(name.trim_end().chars().next_back()?)
            }
        }
    };
    Some((key, mods))
}

/// The bytes a terminal sends a program for a key, as xterm does. `application_cursor` is the
/// mode full-screen programs switch the arrow keys to.
pub fn key_bytes(key: Key, mods: Mods, application_cursor: bool) -> Vec<u8> {
    let mut out = Vec::new();
    let alt_prefix = |out: &mut Vec<u8>| {
        if mods.alt {
            out.push(0x1b);
        }
    };
    // `ESC [ 1 ; <mods> <letter>` with modifiers, `ESC [ <letter>` (or `ESC O <letter>`) without.
    let lettered = |letter: char, application: bool| match mods.parameter() {
        Some(parameter) => format!("\x1b[1;{parameter}{letter}").into_bytes(),
        None if application => format!("\x1bO{letter}").into_bytes(),
        None => format!("\x1b[{letter}").into_bytes(),
    };
    let numbered = |number: u8| match mods.parameter() {
        Some(parameter) => format!("\x1b[{number};{parameter}~").into_bytes(),
        None => format!("\x1b[{number}~").into_bytes(),
    };
    match key {
        Key::Char(c) => {
            alt_prefix(&mut out);
            match (mods.ctrl, c) {
                (true, 'a'..='z') => out.push(c as u8 - b'a' + 1),
                (true, 'A'..='Z') => out.push(c as u8 - b'A' + 1),
                (true, ' ' | '@' | '2') => out.push(0),
                (true, '[' | '3') => out.push(0x1b),
                (true, '\\' | '4') => out.push(0x1c),
                (true, ']' | '5') => out.push(0x1d),
                (true, '^' | '6') => out.push(0x1e),
                (true, '_' | '/' | '7') => out.push(0x1f),
                (true, '?' | '8') => out.push(0x7f),
                _ => out.extend_from_slice(c.encode_utf8(&mut [0; 4]).as_bytes()),
            }
        }
        Key::Enter => {
            alt_prefix(&mut out);
            out.push(b'\r');
        }
        Key::Tab if mods.shift => out.extend_from_slice(b"\x1b[Z"),
        Key::Tab => {
            alt_prefix(&mut out);
            out.push(b'\t');
        }
        Key::Esc => {
            alt_prefix(&mut out);
            out.push(0x1b);
        }
        Key::Backspace => {
            alt_prefix(&mut out);
            out.push(if mods.ctrl { 0x08 } else { 0x7f });
        }
        Key::Up => out = lettered('A', application_cursor),
        Key::Down => out = lettered('B', application_cursor),
        Key::Right => out = lettered('C', application_cursor),
        Key::Left => out = lettered('D', application_cursor),
        Key::Home => out = lettered('H', application_cursor),
        Key::End => out = lettered('F', application_cursor),
        Key::Insert => out = numbered(2),
        Key::Delete => out = numbered(3),
        Key::PageUp => out = numbered(5),
        Key::PageDown => out = numbered(6),
        Key::F(n @ 1..=4) => out = lettered(char::from(b'P' + n - 1), mods.parameter().is_none()),
        Key::F(n) => {
            const CODES: [u8; 8] = [15, 17, 18, 19, 20, 21, 23, 24];
            out = numbered(CODES[usize::from(n.clamp(5, 12)) - 5]);
        }
    }
    out
}

/// Text as a terminal sends it when it is typed: a line break is the Enter key.
pub fn text_bytes(text: &str) -> Vec<u8> {
    text.replace("\r\n", "\r").replace('\n', "\r").into_bytes()
}

// ------------------------------------------------------------------ the screen

/// What the emulator is told besides what to draw.
#[derive(Default)]
struct Events {
    /// Answers to what the program asked the terminal: where the cursor is, what it is.
    replies: Vec<u8>,
    /// Rows that scrolled off the top of the screen since they were last collected.
    scrolled: String,
    capture: vt100::capture::BasicFormattedCaptureState,
}

impl vt100::Callbacks for Events {
    fn unhandled_csi(
        &mut self,
        screen: &mut vt100::Screen,
        i1: Option<u8>,
        _i2: Option<u8>,
        params: &[&[u16]],
        c: char,
    ) {
        let first = params.first().and_then(|p| p.first()).copied().unwrap_or(0);
        match (i1, c, first) {
            // "Are you there?"
            (None, 'n', 5) => self.replies.extend_from_slice(b"\x1b[0n"),
            // "Where is the cursor?" A program that asks waits for the answer.
            (None, 'n', 6) => {
                let (row, column) = screen.cursor_position();
                self.replies
                    .extend_from_slice(format!("\x1b[{};{}R", row + 1, column + 1).as_bytes());
            }
            // "What are you?" A VT100 with the advanced video option, as xterm answers.
            (None, 'c', 0) => self.replies.extend_from_slice(b"\x1b[?1;2c"),
            _ => {}
        }
    }

    fn on_scroll(&mut self, contents: vt100::capture::RowContents<'_>, alternate_screen: bool) {
        // What a full-screen program scrolls is not output to keep.
        if !alternate_screen {
            let _ = contents.write_formatted_basic(&mut self.scrolled, &mut self.capture);
        }
    }
}

type Emulator = vt100::Parser<Events>;

/// The screen of a terminal at one moment, as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    /// The rows from the top down to the last that holds anything, without trailing spaces.
    pub rows: Vec<String>,
    /// `(rows, columns)`.
    pub size: (u16, u16),
    /// `(row, column)` of the cursor, from 0.
    pub cursor: (u16, u16),
}

impl Snapshot {
    pub fn text(&self) -> String {
        if self.rows.is_empty() {
            "(blank screen)".to_string()
        } else {
            self.rows.join("\n")
        }
    }
}

/// The screen of a terminal for showing it: every row with its colours.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScreenView {
    /// One line per row of the screen, styled with SGR sequences only and ending reset.
    pub rows: Vec<String>,
    /// `(rows, columns)`.
    pub size: (u16, u16),
    /// `(row, column)` of the cursor; `None` while the program hides it.
    pub cursor: Option<(u16, u16)>,
}

fn sgr_color(color: vt100::Color, base: u8, codes: &mut Vec<String>) {
    match color {
        vt100::Color::Default => {}
        vt100::Color::Idx(i @ 0..=7) => codes.push((base + i).to_string()),
        vt100::Color::Idx(i @ 8..=15) => codes.push((base + 60 + i - 8).to_string()),
        vt100::Color::Idx(i) => codes.push(format!("{};5;{i}", base + 8)),
        vt100::Color::Rgb(r, g, b) => codes.push(format!("{};2;{r};{g};{b}", base + 8)),
    }
}

/// The SGR parameters that select a cell's look; empty for the default look.
fn cell_style(cell: &vt100::Cell) -> String {
    let mut codes = Vec::new();
    for (on, code) in [
        (cell.bold(), "1"),
        (cell.dim(), "2"),
        (cell.italic(), "3"),
        (cell.underline(), "4"),
        (cell.inverse(), "7"),
    ] {
        if on {
            codes.push(code.to_string());
        }
    }
    sgr_color(cell.fgcolor(), 30, &mut codes);
    sgr_color(cell.bgcolor(), 40, &mut codes);
    codes.join(";")
}

fn styled_row(screen: &vt100::Screen, row: u16, columns: u16) -> String {
    const RESET: &str = "\x1b[0m";
    let mut out = String::new();
    let mut style = String::new();
    // Blank cells of the default look at the end of a row are left out.
    let mut pending = 0usize;
    for column in 0..columns {
        let Some(cell) = screen.cell(row, column) else {
            break;
        };
        if cell.is_wide_continuation() {
            continue;
        }
        let look = cell_style(cell);
        if !cell.has_contents() && look.is_empty() {
            pending += 1;
            continue;
        }
        if look != style {
            if !style.is_empty() {
                out.push_str(RESET);
            }
            out.extend(std::iter::repeat_n(' ', std::mem::take(&mut pending)));
            if !look.is_empty() {
                let _ = write!(out, "\x1b[{look}m");
            }
            style = look;
        } else {
            out.extend(std::iter::repeat_n(' ', std::mem::take(&mut pending)));
        }
        out.push_str(if cell.has_contents() { cell.contents() } else { " " });
    }
    if !style.is_empty() {
        out.push_str(RESET);
    }
    out
}

// ------------------------------------------------------------------ a terminal

/// How a program in a terminal is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalState {
    Running,
    /// It ended by itself, with this exit code.
    Exited(u32),
    /// It was stopped.
    Stopped,
}

/// A program running in a pseudo-terminal, and the screen it has drawn.
pub struct Terminal {
    pub id: String,
    pub command: String,
    /// The agent that started it.
    pub agent: String,
    started: Instant,
    emulator: Mutex<Emulator>,
    /// What to write to the program; a thread of its own does the writing.
    input: Mutex<thread_channel::Sender<Vec<u8>>>,
    /// The terminal's end of the pty, for resizing; gone once the program has ended.
    master: Mutex<Option<Box<dyn MasterPty + Send>>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    state: Mutex<TerminalState>,
    stopping: AtomicBool,
    /// Counts what happened to the screen and the state; waiting for a change is waiting for
    /// this to move.
    revision: watch::Sender<u64>,
    /// Moves with every terminal of the session, for an interface that shows them.
    any: watch::Sender<u64>,
    /// The revision an agent last got the screen at.
    seen: AtomicU64,
    /// The user has the keyboard: an agent's input waits until they hand it back.
    user_typing: watch::Sender<bool>,
    /// The user typed since an agent last got the screen.
    user_typed: AtomicBool,
    log_path: PathBuf,
    log: Mutex<Option<File>>,
    /// Something has been written to the log.
    logged: AtomicBool,
}

impl Terminal {
    fn changed(&self) {
        self.revision.send_modify(|revision| *revision += 1);
        self.any.send_modify(|revision| *revision += 1);
    }

    pub fn state(&self) -> TerminalState {
        *locked(&self.state)
    }

    pub fn running(&self) -> bool {
        self.state() == TerminalState::Running
    }

    /// `running for 12s`, `exited with code 0`, `stopped`.
    pub fn describe(&self) -> String {
        match self.state() {
            TerminalState::Running => format!("running for {}s", self.started.elapsed().as_secs()),
            TerminalState::Exited(code) => format!("exited with code {code}"),
            TerminalState::Stopped => "stopped".to_string(),
        }
    }

    /// Notices of every change of the screen or the state from now on.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.revision.subscribe()
    }

    /// `(rows, columns)` of the screen.
    pub fn size(&self) -> (u16, u16) {
        let (rows, columns) = locked(&self.emulator).screen().size();
        (rows.get(), columns.get())
    }

    /// Gives the program a screen of another size. Nothing happens when it has that size, or
    /// has ended.
    pub fn resize(&self, rows: u16, columns: u16) {
        let (rows, columns) = (rows.max(MIN_SIZE.0), columns.max(MIN_SIZE.1));
        if self.size() == (rows, columns) {
            return;
        }
        let Some(master) = &*locked(&self.master) else {
            return;
        };
        let resized = master.resize(PtySize {
            rows,
            cols: columns,
            pixel_width: 0,
            pixel_height: 0,
        });
        if let Err(e) = resized {
            tracing::warn!("could not resize {}: {e}", self.id);
            return;
        }
        locked(&self.emulator).set_size(cell_count(rows), cell_count(columns));
        self.changed();
    }

    /// The screen as text.
    pub fn snapshot(&self) -> Snapshot {
        let emulator = locked(&self.emulator);
        let screen = emulator.screen();
        let (rows, columns) = screen.size();
        let mut text: Vec<String> = screen
            .rows(0, columns.get())
            .map(|row| row.trim_end().to_string())
            .collect();
        while text.last().is_some_and(String::is_empty) {
            text.pop();
        }
        Snapshot {
            rows: text,
            size: (rows.get(), columns.get()),
            cursor: screen.cursor_position(),
        }
    }

    /// The screen as it looks.
    pub fn view(&self) -> ScreenView {
        let emulator = locked(&self.emulator);
        let screen = emulator.screen();
        let (rows, columns) = screen.size();
        ScreenView {
            rows: (0..rows.get())
                .map(|row| styled_row(screen, row, columns.get()))
                .collect(),
            size: (rows.get(), columns.get()),
            cursor: (!screen.hide_cursor()).then(|| screen.cursor_position()),
        }
    }

    fn write(&self, bytes: Vec<u8>) -> bool {
        !bytes.is_empty() && self.running() && locked(&self.input).send(bytes).is_ok()
    }

    /// Presses a key. False when the program has ended.
    pub fn press(&self, key: Key, mods: Mods) -> bool {
        let application = locked(&self.emulator).screen().application_cursor();
        self.write(key_bytes(key, mods, application))
    }

    /// Types text; a line break is the Enter key.
    pub fn type_text(&self, text: &str) -> bool {
        self.write(text_bytes(text))
    }

    /// Pastes text, marked as a paste when the program asked for that.
    pub fn paste(&self, text: &str) -> bool {
        let mut bytes = text_bytes(text);
        if locked(&self.emulator).screen().bracketed_paste() {
            bytes.splice(0..0, *b"\x1b[200~");
            bytes.extend_from_slice(b"\x1b[201~");
        }
        self.write(bytes)
    }

    /// The user takes the keyboard, or hands it back to the agent.
    pub fn set_user_typing(&self, typing: bool) {
        self.user_typing.send_replace(typing);
        self.changed();
    }

    pub fn user_typing(&self) -> bool {
        *self.user_typing.borrow()
    }

    /// Notes that what was just typed came from the user, for the agent to be told.
    pub fn typed_by_user(&self) {
        self.user_typed.store(true, Ordering::Relaxed);
    }

    /// Ends the program. Its screen stays to be read. False when it had ended already.
    pub fn stop(&self) -> bool {
        if !self.running() {
            return false;
        }
        self.stopping.store(true, Ordering::Relaxed);
        let _ = locked(&self.killer).kill();
        true
    }

    /// The file everything the program printed is saved to: the rows that scrolled off its
    /// screen as they do, and its last screen once it has ended.
    pub fn log_path(&self) -> &Path {
        &self.log_path
    }

    /// The log as an address the `read` tool takes.
    pub fn log_address(&self) -> String {
        let name = self.log_path.file_name().unwrap_or_default().to_string_lossy();
        format!("artifact://{name}")
    }

    fn log(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        self.logged.store(true, Ordering::Relaxed);
        let mut log = locked(&self.log);
        let failed = match &mut *log {
            Some(file) => file.write_all(text.as_bytes()).is_err(),
            None => false,
        };
        if failed {
            *log = None;
        }
    }

    /// Takes in what the program wrote.
    fn feed(&self, bytes: &[u8]) {
        let (replies, scrolled) = {
            let mut emulator = locked(&self.emulator);
            emulator.process(bytes);
            let events = emulator.callbacks_mut();
            (
                std::mem::take(&mut events.replies),
                std::mem::take(&mut events.scrolled),
            )
        };
        if !replies.is_empty() {
            let _ = locked(&self.input).send(replies);
        }
        let plain: String = vt100::capture::basic_formatted_to_plain(&scrolled).collect();
        self.log(&plain);
        self.changed();
    }

    /// Records that the program has ended: its last screen closes the log.
    fn ended(&self, code: u32) {
        // The rows that scrolled off come first; the last of them has no line break yet.
        let after_rows = if self.logged.load(Ordering::Relaxed) {
            "\n"
        } else {
            ""
        };
        self.log(&format!("{after_rows}{}\n", self.snapshot().rows.join("\n")));
        *locked(&self.log) = None;
        *locked(&self.state) = if self.stopping.load(Ordering::Relaxed) {
            TerminalState::Stopped
        } else {
            TerminalState::Exited(code)
        };
        self.changed();
    }

    /// Waits for the screen to settle: for the first change (at most `first`), then until
    /// nothing has changed for [`QUIET`], which is given up after `limit`. Not past the end of
    /// the program or `cancel`.
    async fn settle(
        &self,
        changes: &mut watch::Receiver<u64>,
        first: Duration,
        limit: Duration,
        cancel: &CancellationToken,
    ) {
        let deadline = Instant::now() + first + limit;
        let mut quiet = first;
        while self.running() {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return;
            }
            tokio::select! {
                changed = tokio::time::timeout(quiet.min(left), changes.changed()) => match changed {
                    Ok(Ok(())) => quiet = QUIET,
                    // Nothing was written for long enough, or the terminal is gone.
                    Ok(Err(_)) | Err(_) => return,
                },
                _ = cancel.cancelled() => return,
            }
        }
    }

    /// Waits until `done` holds of this terminal, looking again at every change. False when
    /// `limit` passed or `cancel` fired first.
    async fn wait_until(
        &self,
        mut done: impl FnMut(&Terminal) -> bool,
        limit: Duration,
        cancel: &CancellationToken,
    ) -> bool {
        let mut changes = self.changes();
        let deadline = tokio::time::Instant::now() + limit;
        loop {
            if done(self) {
                return true;
            }
            tokio::select! {
                changed = tokio::time::timeout_at(deadline, changes.changed()) => {
                    if !matches!(changed, Ok(Ok(()))) {
                        return done(self);
                    }
                }
                _ = cancel.cancelled() => return false,
            }
        }
    }

    /// Resolves when the program has ended.
    pub async fn wait_exit(&self) {
        let never = CancellationToken::new();
        while !self.wait_until(|t| !t.running(), MAX_READ_WAIT, &never).await {}
    }

    /// The screen and the state as an agent is given them, with what the user did meanwhile.
    fn report(&self) -> String {
        let snapshot = self.snapshot();
        self.seen.store(*self.revision.borrow(), Ordering::Relaxed);
        let mut out = snapshot.text();
        let (rows, columns) = snapshot.size;
        let _ = write!(out, "\n[{}: {}", self.id, self.describe());
        if self.running() {
            let (row, column) = snapshot.cursor;
            let _ = write!(
                out,
                "; screen {columns}x{rows}, cursor at row {} column {}",
                row + 1,
                column + 1
            );
        } else {
            let _ = write!(out, "; full log: {}", self.log_address());
        }
        out.push(']');
        if self.user_typed.swap(false, Ordering::Relaxed) {
            out.push_str("\n[the user typed into this terminal since you last saw it]");
        }
        if self.user_typing() {
            out.push_str("\n[the user has the keyboard of this terminal right now]");
        }
        out
    }

    fn details(&self) -> Value {
        let state = match self.state() {
            TerminalState::Running => "running",
            TerminalState::Exited(_) => "exited",
            TerminalState::Stopped => "stopped",
        };
        let code = match self.state() {
            TerminalState::Exited(code) => Some(code),
            _ => None,
        };
        json!({"pty_id": self.id, "state": state, "exit_code": code})
    }
}

/// One terminal as a picker shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalEntry {
    pub id: String,
    /// `running for 12s`, `exited with code 0`, `stopped`.
    pub state: String,
    pub command: String,
    pub running: bool,
}

/// The terminals of one session. Dropping it ends every program in them.
pub struct Terminals {
    next: AtomicU64,
    terminals: Mutex<BTreeMap<String, Arc<Terminal>>>,
    /// Where logs are saved: the artifact directory, so that `artifact://` reads them.
    log_dir: PathBuf,
    /// Sets this session's logs apart from those of others in the same directory.
    stamp: u64,
    any: watch::Sender<u64>,
}

impl Terminals {
    pub fn new(log_dir: PathBuf) -> Self {
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0, |since| since.as_millis() as u64);
        Terminals {
            next: AtomicU64::new(0),
            terminals: Mutex::default(),
            log_dir,
            stamp,
            any: watch::Sender::new(0),
        }
    }

    /// Notices of anything that happens to any terminal: one started, drew, or ended.
    pub fn subscribe(&self) -> watch::Receiver<u64> {
        self.any.subscribe()
    }

    pub fn get(&self, id: &str) -> Option<Arc<Terminal>> {
        locked(&self.terminals).get(id).cloned()
    }

    /// Every terminal, oldest first.
    pub fn all(&self) -> Vec<Arc<Terminal>> {
        let mut all: Vec<Arc<Terminal>> = locked(&self.terminals).values().cloned().collect();
        all.sort_by_key(|terminal| terminal.started);
        all
    }

    pub fn entries(&self) -> Vec<TerminalEntry> {
        self.all()
            .iter()
            .map(|terminal| TerminalEntry {
                id: terminal.id.clone(),
                state: terminal.describe(),
                command: terminal.command.clone(),
                running: terminal.running(),
            })
            .collect()
    }

    pub fn stop_all(&self) {
        for terminal in locked(&self.terminals).values() {
            terminal.stop();
        }
    }

    /// Starts `command` in `shell`, in a terminal of its own, for `agent`.
    pub fn start(
        &self,
        shell: &Shell,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
        agent: &str,
    ) -> Result<Arc<Terminal>, String> {
        let running = locked(&self.terminals)
            .values()
            .filter(|terminal| terminal.running())
            .count();
        if running >= MAX_RUNNING {
            return Err(format!(
                "{MAX_RUNNING} programs are running in terminals already; stop one with the pty tool first"
            ));
        }
        let (rows, columns) = DEFAULT_SIZE;
        let failed =
            |e: &dyn std::fmt::Display| format!("could not start {} in a terminal: {e}", shell.name());
        let pair = native_pty_system()
            .openpty(PtySize {
                rows,
                cols: columns,
                pixel_width: 0,
                pixel_height: 0,
            })
            .map_err(|e| failed(&e))?;

        let mut builder = CommandBuilder::new(&shell.program);
        if shell.is_posix() {
            builder.args(["-c", command]);
        } else {
            builder.args(["-NoProfile", "-Command", command]);
        }
        builder.cwd(cwd);
        // The environment of this process as it is now, not as the system would set it up for
        // a new login.
        builder.env_clear();
        for (name, value) in std::env::vars_os() {
            builder.env(name, value);
        }
        builder.env("TERM", "xterm-256color");
        for (name, value) in env {
            builder.env(name, value);
        }
        let mut child = pair.slave.spawn_command(builder).map_err(|e| failed(&e))?;
        // The program has its end of the pty; ours would keep the pty open after it ends.
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader().map_err(|e| failed(&e))?;
        let mut writer = pair.master.take_writer().map_err(|e| failed(&e))?;

        let number = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let id = format!("pty-{number}");
        let log_path = self.log_dir.join(format!("pty-{}-{number}.log", self.stamp));
        let log = std::fs::create_dir_all(&self.log_dir)
            .and_then(|()| File::create(&log_path))
            .map_err(|e| tracing::warn!("no log for {id}: {}: {e}", log_path.display()))
            .ok();
        let (input, typed) = thread_channel::channel::<Vec<u8>>();
        let terminal = Arc::new(Terminal {
            id: id.clone(),
            command: command.to_string(),
            agent: agent.to_string(),
            started: Instant::now(),
            emulator: Mutex::new(vt100::Parser::new_with_callbacks(
                cell_count(rows),
                cell_count(columns),
                0,
                Events::default(),
            )),
            input: Mutex::new(input),
            master: Mutex::new(Some(pair.master)),
            killer: Mutex::new(child.clone_killer()),
            state: Mutex::new(TerminalState::Running),
            stopping: AtomicBool::new(false),
            revision: watch::Sender::new(0),
            any: self.any.clone(),
            seen: AtomicU64::new(0),
            user_typing: watch::Sender::new(false),
            user_typed: AtomicBool::new(false),
            log_path,
            log: Mutex::new(log),
            logged: AtomicBool::new(false),
        });

        // What is typed, in order, without ever making the caller wait for the program.
        std::thread::spawn(move || {
            while let Ok(bytes) = typed.recv() {
                if writer.write_all(&bytes).and_then(|()| writer.flush()).is_err() {
                    break;
                }
            }
        });
        let (drained, reading) = thread_channel::channel::<()>();
        let screen = terminal.clone();
        std::thread::spawn(move || {
            let mut buffer = [0u8; 8192];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => screen.feed(&buffer[..n]),
                }
            }
            let _ = drained.send(());
        });
        let ending = terminal.clone();
        std::thread::spawn(move || {
            let code = child.wait().map_or(1, |status| status.exit_code());
            // Closing our end makes the system deliver what is left of the output and then
            // end the stream the reader is on.
            drop(locked(&ending.master).take());
            let _ = reading.recv_timeout(DRAIN_LIMIT);
            ending.ended(code);
        });

        locked(&self.terminals).insert(id, terminal.clone());
        terminal.changed();
        Ok(terminal)
    }
}

impl Drop for Terminals {
    fn drop(&mut self) {
        self.stop_all();
    }
}

/// What `shell` returns for a command it started in a terminal: the id, and the screen once it
/// has settled.
pub(crate) async fn started(terminal: &Terminal, cancel: &CancellationToken) -> ToolOutput {
    // A terminal may be told things that draw nothing before the program gets to say anything:
    // the first output is the first thing on the screen.
    terminal
        .wait_until(
            |t| !t.running() || !t.snapshot().rows.is_empty(),
            START_OUTPUT,
            cancel,
        )
        .await;
    let mut changes = terminal.changes();
    terminal.settle(&mut changes, QUIET, SETTLE_LIMIT, cancel).await;
    ToolOutput::text(format!(
        "started {} in a terminal; read its screen, type into it and stop it with the pty tool\n{}",
        terminal.id,
        terminal.report()
    ))
    .with_details(terminal.details())
}

/// What the lead is told when a terminal it asked to be woken for has ended.
pub(crate) fn exit_notice(terminal: &Terminal) -> String {
    format!(
        "<terminal-exit id=\"{}\">\nThe program you started in a terminal has ended: {}\n{}\n</terminal-exit>",
        terminal.id,
        terminal.command,
        terminal.report()
    )
}

// ------------------------------------------------------------------ the tool

pub struct PtyTool {
    terminals: Arc<Terminals>,
}

impl PtyTool {
    pub fn new(terminals: Arc<Terminals>) -> Self {
        PtyTool { terminals }
    }

    /// The terminal a call is about: the one it names, or the only one there is.
    fn target(&self, input: &Value) -> Result<Arc<Terminal>, String> {
        match input.get("id").and_then(Value::as_str) {
            Some(id) => self
                .terminals
                .get(id.trim())
                .ok_or_else(|| format!("no terminal named `{id}`; `list` shows them")),
            None => match self.terminals.all().as_slice() {
                [only] => Ok(only.clone()),
                [] => Err(
                    "no program runs in a terminal; start one with `shell` and `interactive: true`"
                        .to_string(),
                ),
                _ => Err("`id` is required: more than one terminal is open (`list` shows them)".to_string()),
            },
        }
    }

    fn action(input: &Value) -> Option<&str> {
        input.get("action").and_then(Value::as_str)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PtyAction {
    Send,
    Read,
    Stop,
    List,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Wait {
    Change,
    Exit,
}

#[derive(Deserialize)]
struct PtyInput {
    action: PtyAction,
    text: Option<String>,
    #[serde(default)]
    keys: Vec<String>,
    #[serde(default)]
    submit: bool,
    wait: Option<Wait>,
    wait_for: Option<String>,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for PtyTool {
    fn name(&self) -> &str {
        "pty"
    }

    fn description(&self) -> &str {
        "Work with a program that `shell` started in a terminal (`interactive: true`): an editor, a \
         REPL, an installer that asks questions, anything that draws a screen or waits for keys. \
         `read` returns the screen as text; it can wait for the screen to change (`wait: \"change\"`), \
         for text to appear on it (`wait_for`) or for the program to end (`wait: \"exit\"`). `send` \
         types `text`, then presses `keys` (enter, tab, esc, up, down, left, right, home, end, pageup, \
         pagedown, backspace, delete, f1-f12, ctrl+c, alt+x, shift+tab), then Enter when `submit` is \
         true, and returns the screen once it has settled. `stop` ends the program; `list` shows \
         every terminal. The user sees the terminal live and may type into it too: a result says \
         when they did. What scrolled off the screen is in the log a result names once the program \
         has ended."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["send", "read", "stop", "list"]},
                "id": {"type": "string", "description": "The terminal, such as pty-1; may be left out while only one is open"},
                "text": {"type": "string", "description": "send: text to type; a line break presses Enter"},
                "keys": {
                    "type": "array",
                    "items": {"type": "string"},
                    "description": "send: keys to press after the text, in order, such as [\"down\", \"down\", \"enter\"] or [\"ctrl+c\"]"
                },
                "submit": {"type": "boolean", "description": "send: press Enter after the text and the keys"},
                "wait": {"type": "string", "enum": ["change", "exit"], "description": "read: wait until the screen has changed since you last saw it, or until the program has ended"},
                "wait_for": {"type": "string", "description": "read: wait until this text is on the screen"},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "read: the longest to wait (default 30)"}
            },
            "required": ["action"]
        })
    }

    /// Typing into a program and stopping it are held to what the program itself is: see
    /// [`Tool::subjects`].
    fn access(&self, input: &Value) -> Access {
        match Self::action(input) {
            Some("send" | "stop") if self.target(input).is_ok() => Access::Exec,
            _ => Access::Read,
        }
    }

    /// The command that runs in the terminal a call types into or stops: the call is allowed
    /// where running that command is.
    fn subjects(&self, input: &Value) -> Vec<String> {
        match Self::action(input) {
            Some("send" | "stop") => self
                .target(input)
                .map(|terminal| terminal.command.clone())
                .into_iter()
                .collect(),
            _ => Vec::new(),
        }
    }

    fn approval_note(&self, input: &Value) -> Option<String> {
        let terminal = self.target(input).ok()?;
        match Self::action(input)? {
            "stop" => Some(format!("stops the program in {}", terminal.id)),
            "send" => {
                let mut parts = Vec::new();
                if let Some(text) = input
                    .get("text")
                    .and_then(Value::as_str)
                    .filter(|t| !t.is_empty())
                {
                    parts.push(format!("{text:?}"));
                }
                let keys = input.get("keys").and_then(Value::as_array);
                parts.extend(
                    keys.into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .map(str::to_string),
                );
                if input.get("submit").and_then(Value::as_bool) == Some(true) {
                    parts.push("enter".to_string());
                }
                Some(format!("types into {}: {}", terminal.id, parts.join(" ")))
            }
            _ => None,
        }
    }

    fn replay_safe(&self, input: &Value) -> bool {
        matches!(Self::action(input), Some("read" | "list"))
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let target = self.target(&input);
        let input: PtyInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.action == PtyAction::List {
            let lines: Vec<String> = self
                .terminals
                .entries()
                .iter()
                .map(|entry| format!("{}  {}  {}", entry.id, entry.state, entry.command))
                .collect();
            return ToolOutput::text(if lines.is_empty() {
                "no terminals".to_string()
            } else {
                lines.join("\n")
            });
        }
        let terminal = match target {
            Ok(terminal) => terminal,
            Err(why) => return ToolOutput::error(why),
        };
        let done = |terminal: &Terminal, note: Option<String>| {
            let mut text = terminal.report();
            if let Some(note) = note {
                text.push_str(&format!("\n[{note}]"));
            }
            ToolOutput::text(text).with_details(terminal.details())
        };
        match input.action {
            PtyAction::Stop => {
                if !terminal.stop() {
                    return done(&terminal, Some("it had ended already".to_string()));
                }
                terminal
                    .wait_until(|t| !t.running(), DRAIN_LIMIT * 3, &ctx.cancel)
                    .await;
                done(&terminal, None)
            }
            PtyAction::Read => {
                let limit = input
                    .timeout_secs
                    .map_or(DEFAULT_READ_WAIT, Duration::from_secs)
                    .min(MAX_READ_WAIT);
                let seen = terminal.seen.load(Ordering::Relaxed);
                let (waited_for, arrived) = match (&input.wait_for, input.wait) {
                    (Some(wanted), _) if !wanted.is_empty() => (
                        Some(format!("`{wanted}` to appear")),
                        terminal
                            .wait_until(
                                |t| !t.running() || t.snapshot().text().contains(wanted.as_str()),
                                limit,
                                &ctx.cancel,
                            )
                            .await
                            && terminal.snapshot().text().contains(wanted.as_str()),
                    ),
                    (_, Some(Wait::Exit)) => (
                        Some("the program to end".to_string()),
                        terminal.wait_until(|t| !t.running(), limit, &ctx.cancel).await,
                    ),
                    (_, Some(Wait::Change)) => {
                        let changed = terminal
                            .wait_until(
                                |t| !t.running() || *t.revision.borrow() != seen,
                                limit,
                                &ctx.cancel,
                            )
                            .await;
                        if changed {
                            // The rest of what is being drawn comes with it.
                            let mut changes = terminal.changes();
                            terminal
                                .settle(&mut changes, QUIET, SETTLE_LIMIT, &ctx.cancel)
                                .await;
                        }
                        (Some("the screen to change".to_string()), changed)
                    }
                    _ => (None, true),
                };
                let note = match waited_for {
                    Some(what) if !arrived && terminal.running() => Some(format!(
                        "waited {}s for {what}; it has not happened yet",
                        limit.as_secs()
                    )),
                    Some(what) if !arrived && input.wait != Some(Wait::Exit) => {
                        Some(format!("the program ended while waiting for {what}"))
                    }
                    _ => None,
                };
                done(&terminal, note)
            }
            // `list` was answered before a terminal was looked for.
            PtyAction::Send | PtyAction::List => {
                let mut keys = Vec::new();
                for name in &input.keys {
                    match parse_key(name) {
                        Some(key) => keys.push(key),
                        None => {
                            return ToolOutput::error(format!(
                                "`{name}` is not a key; keys are enter, tab, esc, backspace, delete, insert, \
                                 up, down, left, right, home, end, pageup, pagedown, space, f1-f12 and single \
                                 characters, with ctrl+, alt+ or shift+ in front; put text in `text`"
                            ));
                        }
                    }
                }
                let text = input.text.unwrap_or_default();
                if text.is_empty() && keys.is_empty() && !input.submit {
                    return ToolOutput::error("nothing to send: give `text`, `keys` or `submit`");
                }
                if !terminal.running() {
                    return ToolOutput {
                        is_error: true,
                        ..done(
                            &terminal,
                            Some("nothing was sent: the program has ended".to_string()),
                        )
                    };
                }
                // Two typists at once garble each other: the user goes first.
                let mut typing = terminal.user_typing.subscribe();
                let free = tokio::select! {
                    free = tokio::time::timeout(HANDBACK_LIMIT, typing.wait_for(|typing| !typing)) => free.is_ok(),
                    _ = ctx.cancel.cancelled() => false,
                };
                if !free {
                    return ToolOutput {
                        is_error: true,
                        ..done(
                            &terminal,
                            Some(
                                "nothing was sent: the user is typing into this terminal and has not handed it \
                                 back (ctrl+g)"
                                    .to_string(),
                            ),
                        )
                    };
                }
                let mut changes = terminal.changes();
                terminal.type_text(&text);
                for (key, mods) in keys {
                    terminal.press(key, mods);
                }
                if input.submit {
                    terminal.press(Key::Enter, Mods::default());
                }
                terminal
                    .settle(&mut changes, SEND_OUTPUT, SETTLE_LIMIT, &ctx.cancel)
                    .await;
                done(&terminal, None)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::{AgentInfo, Services, UpdateSink};

    use super::*;

    fn ctx(cwd: &Path) -> ToolCtx {
        let agent = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: cwd.to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: Arc::new(Services::new()),
        }
    }

    /// A command that prints `prompt`, reads a line and prints `got <line>`.
    fn ask(shell: &Shell, prompt: &str) -> String {
        if shell.is_posix() {
            format!("printf '{prompt} '; read answer; echo \"got $answer\"")
        } else {
            format!("$answer = Read-Host '{prompt}'; Write-Output \"got $answer\"")
        }
    }

    async fn wait_for(terminal: &Terminal, wanted: &str) {
        let found = terminal
            .wait_until(
                |t| t.snapshot().text().contains(wanted),
                Duration::from_secs(30),
                &CancellationToken::new(),
            )
            .await;
        assert!(
            found,
            "`{wanted}` never appeared:\n{}",
            terminal.snapshot().text()
        );
    }

    #[test]
    fn keys_are_named_as_people_write_them() {
        let plain = Mods::default();
        let ctrl = Mods { ctrl: true, ..plain };
        assert_eq!(parse_key("enter"), Some((Key::Enter, plain)));
        assert_eq!(parse_key(" Escape "), Some((Key::Esc, plain)));
        assert_eq!(parse_key("ctrl+c"), Some((Key::Char('c'), ctrl)));
        assert_eq!(parse_key("Ctrl+Left"), Some((Key::Left, ctrl)));
        assert_eq!(
            parse_key("shift+tab"),
            Some((Key::Tab, Mods { shift: true, ..plain }))
        );
        assert_eq!(parse_key("backtab"), parse_key("shift+tab"));
        assert_eq!(
            parse_key("ctrl+alt+x"),
            Some((
                Key::Char('x'),
                Mods {
                    ctrl: true,
                    alt: true,
                    shift: false
                }
            ))
        );
        assert_eq!(parse_key("f12"), Some((Key::F(12), plain)));
        assert_eq!(parse_key("G"), Some((Key::Char('G'), plain)));
        assert_eq!(parse_key("+"), Some((Key::Char('+'), plain)));
        assert_eq!(parse_key("ctrl++"), Some((Key::Char('+'), ctrl)));
        assert_eq!(parse_key("space"), Some((Key::Char(' '), plain)));
        for bad in ["", "f13", "f0", "hyper+x", "hello", "ctrl+"] {
            assert_eq!(parse_key(bad), None, "{bad}");
        }
    }

    #[test]
    fn keys_become_the_bytes_xterm_sends() {
        let bytes = |name: &str, application: bool| {
            let (key, mods) = parse_key(name).unwrap();
            String::from_utf8(key_bytes(key, mods, application)).unwrap()
        };
        assert_eq!(bytes("enter", false), "\r");
        assert_eq!(bytes("tab", false), "\t");
        assert_eq!(bytes("shift+tab", false), "\x1b[Z");
        assert_eq!(bytes("esc", false), "\x1b");
        assert_eq!(bytes("backspace", false), "\x7f");
        assert_eq!(bytes("ctrl+c", false), "\x03");
        assert_eq!(bytes("ctrl+C", false), "\x03");
        assert_eq!(bytes("ctrl+space", false), "\0");
        assert_eq!(bytes("ctrl+[", false), "\x1b");
        assert_eq!(bytes("alt+x", false), "\x1bx");
        assert_eq!(bytes("é", false), "é");
        // Arrows change with the mode a full-screen program sets; modifiers override it.
        assert_eq!(bytes("up", false), "\x1b[A");
        assert_eq!(bytes("up", true), "\x1bOA");
        assert_eq!(bytes("ctrl+right", true), "\x1b[1;5C");
        assert_eq!(bytes("shift+alt+left", false), "\x1b[1;4D");
        assert_eq!(bytes("home", false), "\x1b[H");
        assert_eq!(bytes("end", true), "\x1bOF");
        assert_eq!(bytes("delete", false), "\x1b[3~");
        assert_eq!(bytes("ctrl+delete", false), "\x1b[3;5~");
        assert_eq!(bytes("pageup", false), "\x1b[5~");
        assert_eq!(bytes("pagedown", false), "\x1b[6~");
        assert_eq!(bytes("insert", false), "\x1b[2~");
        assert_eq!(bytes("f1", false), "\x1bOP");
        assert_eq!(bytes("f4", false), "\x1bOS");
        assert_eq!(bytes("shift+f1", false), "\x1b[1;2P");
        assert_eq!(bytes("f5", false), "\x1b[15~");
        assert_eq!(bytes("f12", false), "\x1b[24~");
        assert_eq!(text_bytes("a\r\nb\nc"), b"a\rb\rc");
    }

    fn emulator(rows: u16, columns: u16) -> Emulator {
        vt100::Parser::new_with_callbacks(cell_count(rows), cell_count(columns), 0, Events::default())
    }

    #[test]
    fn the_emulator_answers_what_programs_ask_a_terminal() {
        let mut emulator = emulator(5, 20);
        emulator.process(b"ab\r\ncd\x1b[6n\x1b[5n\x1b[c");
        assert_eq!(
            String::from_utf8(std::mem::take(&mut emulator.callbacks_mut().replies)).unwrap(),
            "\x1b[2;3R\x1b[0n\x1b[?1;2c"
        );
    }

    #[test]
    fn rows_that_scroll_off_are_kept_as_plain_lines_but_not_those_of_a_full_screen_program() {
        let mut emulator = emulator(3, 10);
        emulator.process(b"\x1b[31mone\x1b[0m\r\ntwo\r\nthree\r\nfour\r\nfive");
        let scrolled = std::mem::take(&mut emulator.callbacks_mut().scrolled);
        let plain: String = vt100::capture::basic_formatted_to_plain(&scrolled).collect();
        assert_eq!(plain, "one\ntwo");
        // The alternate screen is where editors and pagers draw; what they scroll is not output.
        emulator.process(b"\x1b[?1049hA\r\nB\r\nC\r\nD\r\nE\x1b[?1049l");
        assert_eq!(emulator.callbacks().scrolled, "");
        emulator.process(b"\r\nsix");
        let scrolled = std::mem::take(&mut emulator.callbacks_mut().scrolled);
        let plain: String = vt100::capture::basic_formatted_to_plain(&scrolled).collect();
        assert_eq!(plain, "\nthree");
    }

    #[test]
    fn a_row_is_drawn_with_its_colours_and_nothing_that_moves_the_cursor() {
        let mut emulator = emulator(2, 12);
        emulator.process(b"a \x1b[1;31mred\x1b[0m  \x1b[7mz\x1b[0m\r\n\x1b[48;5;200m \x1b[0m");
        let screen = emulator.screen();
        assert_eq!(
            styled_row(screen, 0, 12),
            "a \x1b[1;31mred\x1b[0m  \x1b[7mz\x1b[0m"
        );
        // A coloured blank is drawn; default blanks after it are not.
        assert_eq!(styled_row(screen, 1, 12), "\x1b[48;5;200m \x1b[0m");
        let mut wide = self::emulator(1, 8);
        wide.process("日本 x\x1b[38;2;1;2;3my".as_bytes());
        assert_eq!(styled_row(wide.screen(), 0, 8), "日本 x\x1b[38;2;1;2;3my\x1b[0m");
    }

    #[tokio::test]
    async fn a_program_is_read_typed_into_and_ends_with_its_log_saved() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::resolve("");
        let terminals = Arc::new(Terminals::new(dir.path().join("artifacts")));
        let tool = PtyTool::new(terminals.clone());
        let context = ctx(dir.path());
        let mut any = terminals.subscribe();
        let env = vec![("AZ_PTY_TEST".to_string(), "from the agent".to_string())];
        let command = format!(
            "{}; {}",
            ask(&shell, "name?"),
            if shell.is_posix() {
                "echo \"$AZ_PTY_TEST\"; exit 3"
            } else {
                "Write-Output $env:AZ_PTY_TEST; exit 3"
            }
        );
        let terminal = terminals
            .start(&shell, &command, dir.path(), &env, "lead")
            .unwrap();
        assert_eq!(terminal.id, "pty-1");
        assert_eq!(terminal.size(), DEFAULT_SIZE);
        assert!(any.has_changed().unwrap(), "an interface hears of a new terminal");
        any.mark_unchanged();
        wait_for(&terminal, "name?").await;
        assert!(any.has_changed().unwrap(), "and of what it draws");

        // Reading waits for nothing by default and says how the program is doing.
        let read = tool.call(json!({"action": "read"}), &context).await;
        let text = read.text_content();
        assert!(text.contains("name?"), "{text}");
        assert!(text.contains("[pty-1: running for "), "{text}");
        assert!(text.contains("screen 100x30, cursor at row "), "{text}");
        assert_eq!(read.details.unwrap()["state"], "running");
        assert_eq!(
            tool.call(json!({"action": "list"}), &context)
                .await
                .text_content()
                .split("  ")
                .collect::<Vec<_>>()[0],
            "pty-1"
        );

        // What is sent is typed; the result is the screen after the program reacted.
        let sent = tool
            .call(
                json!({"action": "send", "id": "pty-1", "text": "Ada", "submit": true}),
                &context,
            )
            .await;
        assert!(!sent.is_error, "{}", sent.text_content());
        let read = tool
            .call(
                json!({"action": "read", "wait": "exit", "timeout_secs": 60}),
                &context,
            )
            .await;
        let text = read.text_content();
        assert!(text.contains("got Ada"), "{text}");
        assert!(text.contains("from the agent"), "{text}");
        assert_eq!(terminal.state(), TerminalState::Exited(3));
        let log = terminal.log_address();
        assert!(
            text.contains(&format!("[pty-1: exited with code 3; full log: {log}]")),
            "{text}"
        );
        assert_eq!(read.details.unwrap()["exit_code"], 3);

        // The log is in the artifact directory under the name its address gives.
        assert!(
            log.starts_with("artifact://pty-") && log.ends_with("-1.log"),
            "{log}"
        );
        let saved = std::fs::read_to_string(terminal.log_path()).unwrap();
        assert!(saved.contains("got Ada"), "{saved}");
        assert_eq!(
            terminal.log_path().parent(),
            Some(dir.path().join("artifacts").as_path())
        );

        // Nothing can be typed into a program that has ended, and the result says so.
        let late = tool.call(json!({"action": "send", "text": "x"}), &context).await;
        assert!(late.is_error);
        assert!(
            late.text_content()
                .contains("nothing was sent: the program has ended")
        );
        let entries = terminals.entries();
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].running && entries[0].state == "exited with code 3");
    }

    #[tokio::test]
    async fn a_read_waits_for_text_and_a_stopped_program_keeps_its_screen() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::resolve("");
        let terminals = Arc::new(Terminals::new(dir.path().to_path_buf()));
        let tool = PtyTool::new(terminals.clone());
        let context = ctx(dir.path());
        let command = if shell.is_posix() {
            "echo first; sleep 1; echo second; sleep 600"
        } else {
            "Write-Output first; Start-Sleep 1; Write-Output second; Start-Sleep 600"
        };
        let terminal = terminals.start(&shell, command, dir.path(), &[], "lead").unwrap();

        let read = tool
            .call(
                json!({"action": "read", "wait_for": "second", "timeout_secs": 60}),
                &context,
            )
            .await;
        assert!(
            read.text_content().contains("first\nsecond"),
            "{}",
            read.text_content()
        );

        // Waiting for what does not come ends at the limit and says what is on the screen.
        let waited = Instant::now();
        let read = tool
            .call(
                json!({"action": "read", "wait_for": "third", "timeout_secs": 1}),
                &context,
            )
            .await;
        assert!(!read.is_error);
        assert!(waited.elapsed() >= Duration::from_secs(1));
        let text = read.text_content();
        assert!(
            text.ends_with("[waited 1s for `third` to appear; it has not happened yet]"),
            "{text}"
        );
        // Nothing has changed since: waiting for a change waits, too.
        let read = tool
            .call(
                json!({"action": "read", "wait": "change", "timeout_secs": 1}),
                &context,
            )
            .await;
        assert!(
            read.text_content()
                .ends_with("[waited 1s for the screen to change; it has not happened yet]"),
            "{}",
            read.text_content()
        );

        // The call is held to the command that runs in the terminal.
        let stop = json!({"action": "stop", "id": "pty-1"});
        assert_eq!(tool.access(&stop), Access::Exec);
        assert_eq!(tool.subjects(&stop), [command]);
        assert_eq!(tool.access(&json!({"action": "read"})), Access::Read);
        assert_eq!(
            tool.access(&json!({"action": "send", "id": "pty-9"})),
            Access::Read
        );
        assert_eq!(
            tool.approval_note(&json!({"action": "send", "text": "y", "keys": ["tab"], "submit": true})),
            Some("types into pty-1: \"y\" tab enter".to_string())
        );

        let stopped = tool.call(stop, &context).await;
        let text = stopped.text_content();
        assert!(text.contains("first\nsecond"), "{text}");
        assert!(text.contains("[pty-1: stopped; full log: "), "{text}");
        assert_eq!(terminal.state(), TerminalState::Stopped);
        let again = tool.call(json!({"action": "stop"}), &context).await;
        assert!(again.text_content().ends_with("[it had ended already]"));

        assert!(
            tool.call(json!({"action": "read", "id": "pty-7"}), &context)
                .await
                .is_error
        );
        let bad_key = tool
            .call(json!({"action": "send", "keys": ["enter", "hello"]}), &context)
            .await;
        assert!(bad_key.is_error && bad_key.text_content().starts_with("`hello` is not a key"));
    }

    #[tokio::test]
    async fn the_user_types_first_and_the_agent_is_told() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::resolve("");
        let terminals = Arc::new(Terminals::new(dir.path().to_path_buf()));
        let tool = Arc::new(PtyTool::new(terminals.clone()));
        let context = ctx(dir.path());
        let command = format!("{}; {}", ask(&shell, "one?"), ask(&shell, "two?"));
        let terminal = terminals
            .start(&shell, &command, dir.path(), &[], "lead")
            .unwrap();
        wait_for(&terminal, "one?").await;

        // The user takes the keyboard: what the agent sends waits for it to be handed back.
        terminal.set_user_typing(true);
        let (sender, call_ctx) = (tool.clone(), context.clone());
        let sending = tokio::spawn(async move {
            sender
                .call(
                    json!({"action": "send", "text": "agent", "submit": true}),
                    &call_ctx,
                )
                .await
        });
        terminal.type_text("user\n");
        terminal.typed_by_user();
        wait_for(&terminal, "two?").await;
        assert!(!sending.is_finished(), "the agent's input is held back");
        assert!(!terminal.snapshot().text().contains("agent"));
        terminal.set_user_typing(false);
        let sent = sending.await.unwrap();
        assert!(!sent.is_error, "{}", sent.text_content());
        wait_for(&terminal, "got agent").await;
        let read = tool
            .call(json!({"action": "read", "wait": "exit"}), &context)
            .await;
        let text = read.text_content();
        assert!(text.contains("got user") && text.contains("got agent"), "{text}");
        // Said once, with the first screen the agent got after the user typed.
        assert!(
            sent.text_content()
                .contains("[the user typed into this terminal since you last saw it]")
        );
        assert!(!text.contains("the user typed"), "{text}");
    }

    #[tokio::test]
    async fn a_screen_follows_its_size_and_dropping_the_terminals_ends_the_programs() {
        let dir = tempfile::tempdir().unwrap();
        let shell = Shell::resolve("");
        let terminals = Terminals::new(dir.path().to_path_buf());
        let command = if shell.is_posix() {
            "echo ready; sleep 600"
        } else {
            "Write-Output ready; Start-Sleep 600"
        };
        let terminal = terminals.start(&shell, command, dir.path(), &[], "lead").unwrap();
        wait_for(&terminal, "ready").await;
        terminal.resize(12, 40);
        assert_eq!(terminal.size(), (12, 40));
        let view = terminal.view();
        assert_eq!(view.size, (12, 40));
        assert_eq!(view.rows.len(), 12);
        // Never smaller than a program can draw in.
        terminal.resize(1, 1);
        assert_eq!(terminal.size(), MIN_SIZE);

        drop(terminals);
        let ended = terminal
            .wait_until(
                |t| !t.running(),
                Duration::from_secs(30),
                &CancellationToken::new(),
            )
            .await;
        assert!(ended);
        assert_eq!(terminal.state(), TerminalState::Stopped);
    }
}
