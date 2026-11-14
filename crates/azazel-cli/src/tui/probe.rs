//! Asking the terminal about itself, once, before the UI starts reading keys: the size of a
//! cell in pixels and whether it draws Sixel graphics, and the colour of its background (see
//! [`azazel_tui::term::queries`]).
//!
//! The replies arrive on the same channel as the keys, so they are read here, with a limit: a
//! terminal that does not answer costs the limit once, shows no images and counts as dark.
//! What is not read here is read as keys later, and typed into the input: a reply that has
//! begun to arrive is read to its end.

use std::io::Write;
use std::time::{Duration, Instant};

use azazel_tui::term::{Replies, queries};

/// How long the terminal has to answer.
const LIMIT: Duration = Duration::from_millis(80);
/// How long it has when it is asked for its background. The answer decides the theme, and a
/// terminal that answers costs none of the time, so a slow line gets more of it.
const BACKGROUND_LIMIT: Duration = Duration::from_millis(200);
/// How long the colour is waited for once the device attributes are there without it: a
/// terminal in front of another (a Windows console, a multiplexer) answered that question
/// itself and passed the one about the background on, whose answer comes after.
const PASSED_ON: Duration = Duration::from_millis(80);
/// How long the rest of a reply is waited for after its last character.
const REST: Duration = Duration::from_millis(60);
/// The longest the replies are read for, however slowly they come.
const LONGEST: Duration = Duration::from_millis(500);

/// Asks the terminal for the size of a cell (`cells`) and for the colour of its background
/// (`background`), and reads the replies. Raw mode must be on, and nothing else may be reading
/// the terminal.
pub(super) async fn ask(cells: bool, background: bool) -> Replies {
    #[cfg(windows)]
    let _replies_arrive = TerminalInput::enable();
    let mut out = std::io::stdout();
    if out
        .write_all(queries(cells, background).as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        return Replies::default();
    }
    Replies::parse(&read(Reading::begin(background)).await)
}

/// How long the replies are read for, as they arrive.
struct Reading {
    /// The colour of the background was asked for.
    background: bool,
    /// Reading stops here unless something arrives that moves it.
    until: Instant,
    longest: Instant,
    /// The device attributes came without the colour, which was given its time since.
    passed_on: bool,
}

impl Reading {
    fn begin(background: bool) -> Reading {
        let now = Instant::now();
        Reading {
            background,
            until: now + if background { BACKGROUND_LIMIT } else { LIMIT },
            longest: now + LONGEST,
            passed_on: false,
        }
    }

    /// Takes in what has arrived so far, `read`. False when everything asked for is there.
    fn more(&mut self, read: &[u8]) -> bool {
        let replies = Replies::parse(read);
        let cut_off = unfinished(read);
        // The device attributes end the replies of a terminal that answers in order.
        if replies.complete && !cut_off && (!self.background || replies.background.is_some()) {
            return false;
        }
        let now = Instant::now();
        if replies.complete && !self.passed_on {
            self.passed_on = true;
            self.until = now + PASSED_ON;
        }
        if cut_off {
            self.until = self.until.max(now + REST);
        }
        self.until = self.until.min(self.longest);
        true
    }
}

/// Whether `read` ends in the middle of a reply: one has begun and its last character has not
/// come.
fn unfinished(read: &[u8]) -> bool {
    const ESCAPE: u8 = 0x1b;
    const CONTROL: &[u8] = b"\x1b[";
    const BACKGROUND: &[u8] = b"]11;";
    // The first character of a reply, or of the two that end the colour; the first characters
    // of the colour's reply, which a Windows console hands over without the one before them.
    if read.ends_with(&[ESCAPE]) || (1..BACKGROUND.len()).any(|length| read.ends_with(&BACKGROUND[..length]))
    {
        return true;
    }
    let last = |start: &[u8]| read.windows(start.len()).rposition(|window| window == start);
    match (last(CONTROL), last(BACKGROUND)) {
        // The colour ends with a bell, or with an escape character and a backslash.
        (control, Some(at)) if control.is_none_or(|control| control < at) => !read[at + BACKGROUND.len()..]
            .iter()
            .any(|byte| matches!(*byte, 0x07 | ESCAPE)),
        // A control sequence ends with a letter or one of the signs around the letters.
        (Some(at), _) => !read[at + CONTROL.len()..]
            .iter()
            .any(|byte| (0x40..=0x7e).contains(byte)),
        (None, _) => false,
    }
}

/// A Windows console hands a program key presses and keeps what a terminal answers to itself,
/// unless the program asks for its input the way a terminal sends it. This asks, and puts the
/// mode back when dropped.
#[cfg(windows)]
struct TerminalInput {
    console: crossterm_winapi::ConsoleMode,
    before: u32,
}

#[cfg(windows)]
impl TerminalInput {
    fn enable() -> Option<TerminalInput> {
        use crossterm_winapi::{ConsoleMode, Handle};
        const ENABLE_VIRTUAL_TERMINAL_INPUT: u32 = 0x0200;
        let console = ConsoleMode::from(Handle::current_in_handle().ok()?);
        let before = console.mode().ok()?;
        console.set_mode(before | ENABLE_VIRTUAL_TERMINAL_INPUT).ok()?;
        Some(TerminalInput { console, before })
    }
}

#[cfg(windows)]
impl Drop for TerminalInput {
    fn drop(&mut self) {
        let _ = self.console.set_mode(self.before);
    }
}

/// What the terminal sends until the replies are complete or the time is up.
#[cfg(unix)]
async fn read(mut reading: Reading) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    use tokio::net::unix::pipe;

    let mut bytes = Vec::new();
    // The terminal opened anew: this handle alone reads without waiting, and what it does not
    // read stays for the key reader. Where the terminal device cannot be watched this way
    // (macOS), nothing is read and the terminal counts as not having answered.
    let Ok(mut terminal) = pipe::OpenOptions::new().unchecked(true).open_receiver("/dev/tty") else {
        return bytes;
    };
    let mut buffer = [0u8; 256];
    loop {
        let until = tokio::time::Instant::from_std(reading.until);
        let Ok(Ok(count)) = tokio::time::timeout_at(until, terminal.read(&mut buffer)).await else {
            break;
        };
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if !reading.more(&bytes) {
            break;
        }
    }
    bytes
}

/// On Windows the replies arrive as key presses, one per character. The escape character that
/// starts each reply is not among them (the key reader has no key for it), so it is put back
/// in front of every `[`.
#[cfg(windows)]
async fn read(mut reading: Reading) -> Vec<u8> {
    use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};

    let mut bytes = Vec::new();
    loop {
        let left = reading.until.saturating_duration_since(Instant::now());
        if left.is_zero() || !matches!(crossterm::event::poll(left), Ok(true)) {
            break;
        }
        match crossterm::event::read() {
            Ok(Event::Key(key)) if key.kind == KeyEventKind::Press => match key.code {
                KeyCode::Esc => bytes.push(0x1b),
                KeyCode::Char('[') => {
                    if bytes.last() != Some(&0x1b) {
                        bytes.push(0x1b);
                    }
                    bytes.push(b'[');
                }
                // The bell that ends a reply is the key ctrl+g.
                KeyCode::Char(c)
                    if key.modifiers.contains(KeyModifiers::CONTROL) && c.is_ascii_lowercase() =>
                {
                    bytes.push(c as u8 - b'a' + 1);
                }
                KeyCode::Char(c) => {
                    let mut utf8 = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break,
        }
        if !reading.more(&bytes) {
            break;
        }
    }
    bytes
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reply_that_is_cut_off_is_told_from_one_that_is_whole() {
        // Nothing, typed text and whole replies are nothing to wait for.
        for whole in [
            &b""[..],
            b"hello",
            b"\x1b[6;20;9t",
            b"\x1b[?62;4c",
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\",
            b"]11;rgb:1e1e/1e1e/2e2e\x07",
            b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\\x1b[6;20;9t\x1b[?62;4c",
            b"]11;rgb:1e1e/1e1e/2e2e\x1b[?61c",
        ] {
            assert!(!unfinished(whole), "{:?}", String::from_utf8_lossy(whole));
        }
        // The middle of the device attributes, of the cell size and of the colour, a reply of
        // which only the start is there, and a colour whose end is half there.
        for cut in [
            &b"\x1b[?61;6;7;21"[..],
            b"\x1b[6;20;9t\x1b[?6",
            b"\x1b[",
            b"\x1b[?61c\x1b",
            b"\x1b[?61c]",
            b"\x1b[?61c]1",
            b"\x1b[?61c\x1b]11",
            b"\x1b]11;rgb:ffff/ff",
            b"]11;rgb:1e1e/1e1e/2e2e",
            b"]11;rgb:1e1e/1e1e/2e2e\x1b",
            b"\x1b[6;20;9t]11;rgb:1e",
        ] {
            assert!(unfinished(cut), "{:?}", String::from_utf8_lossy(cut));
        }
    }

    /// Feeds `reading` what a terminal sends, piece by piece as it arrives; returns how much of
    /// it was read before the reading said it had everything.
    fn fed(reading: &mut Reading, pieces: &[&[u8]]) -> usize {
        let mut read = Vec::new();
        for piece in pieces {
            read.extend_from_slice(piece);
            if !reading.more(&read) {
                break;
            }
        }
        read.len()
    }

    #[test]
    fn reading_ends_with_the_device_attributes_once_what_was_asked_for_is_whole() {
        // The cell size and the attributes, in order: done with the last character.
        let whole: &[&[u8]] = &[b"\x1b[6;20", b";9t\x1b[?62", b";4c", b"typed"];
        assert_eq!(fed(&mut Reading::begin(false), whole), 17);
        // Asked for the colour too, a terminal that answers in order is done as soon.
        let in_order: &[&[u8]] = &[b"\x1b]11;rgb:ffff/ffff/ffff\x1b\\", b"\x1b[?62;4c", b"typed"];
        assert_eq!(fed(&mut Reading::begin(true), in_order), 33);
        // A Windows console answers for itself first and hands the colour on after, one
        // character at a time: the reading goes on to the end of the colour's reply, and not
        // only to where the colour can be told.
        let reply = b"\x1b[?61;4c]11;rgb:1e1e/1e1e/2e2e\x1b\\";
        let characters: Vec<&[u8]> = reply.chunks(1).chain([&b"typed"[..]]).collect();
        assert_eq!(fed(&mut Reading::begin(true), &characters), reply.len());
    }

    #[test]
    fn a_reply_that_has_begun_gets_time_for_its_rest_and_the_reading_an_end() {
        // Nothing asked of the background: the short limit, which typed text leaves alone.
        let mut reading = Reading::begin(false);
        let limit = reading.until;
        assert!(reading.more(b"abc"));
        assert_eq!(reading.until, limit);
        // The limit is here and a reply is half there: it gets the time for its rest.
        let before = Instant::now();
        reading.until = before;
        assert!(reading.more(b"\x1b[?61;6"));
        assert!(reading.until >= before + REST && reading.until <= Instant::now() + REST);

        // The attributes without the colour: the colour gets its time from then on, once.
        let mut reading = Reading::begin(true);
        assert!(reading.until >= before + BACKGROUND_LIMIT);
        let before = Instant::now();
        assert!(reading.more(b"\x1b[?61;4c"));
        assert!(reading.until >= before + PASSED_ON && reading.until <= Instant::now() + PASSED_ON);
        let given = reading.until;
        assert!(reading.more(b"\x1b[?61;4cx"));
        assert_eq!(reading.until, given);

        // However slowly the characters come, the end stays where it was set at the start.
        reading.longest = before;
        assert!(reading.more(b"\x1b[?61;4c]11;rgb:1e"));
        assert_eq!(reading.until, before);
    }
}
