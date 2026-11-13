//! Asking the terminal what it can do, once, before the UI starts reading keys: the size of a
//! cell in pixels and whether it draws Sixel graphics (see [`azazel_tui::term::QUERIES`]).
//!
//! The replies arrive on the same channel as the keys, so they are read here, with a limit: a
//! terminal that does not answer costs the limit once and shows no images.

use std::io::Write;
use std::time::{Duration, Instant};

use azazel_tui::term::{QUERIES, Replies};

/// How long the terminal has to answer.
pub(super) const LIMIT: Duration = Duration::from_millis(80);

/// Sends the queries and reads the replies. Raw mode must be on, and nothing else may be
/// reading the terminal.
pub(super) async fn ask(limit: Duration) -> Replies {
    #[cfg(windows)]
    let _replies_arrive = TerminalInput::enable();
    let mut out = std::io::stdout();
    if out
        .write_all(QUERIES.as_bytes())
        .and_then(|()| out.flush())
        .is_err()
    {
        return Replies::default();
    }
    Replies::parse(&read(Instant::now() + limit).await)
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

/// What the terminal sends until the device attributes have arrived or the time is up.
#[cfg(unix)]
async fn read(deadline: Instant) -> Vec<u8> {
    use tokio::io::AsyncReadExt;
    use tokio::net::unix::pipe;

    let mut bytes = Vec::new();
    // The terminal opened anew: this handle alone reads without waiting, and what it does not
    // read stays for the key reader. Where the terminal device cannot be watched this way
    // (macOS), nothing is read and the terminal counts as not having answered.
    let Ok(mut terminal) = pipe::OpenOptions::new().unchecked(true).open_receiver("/dev/tty") else {
        return bytes;
    };
    let deadline = tokio::time::Instant::from_std(deadline);
    let mut buffer = [0u8; 256];
    while let Ok(Ok(count)) = tokio::time::timeout_at(deadline, terminal.read(&mut buffer)).await {
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if Replies::parse(&bytes).complete {
            break;
        }
    }
    bytes
}

/// On Windows the replies arrive as key presses, one per character. The escape character that
/// starts each reply is not among them (the key reader has no key for it), so it is put back
/// in front of every `[`.
#[cfg(windows)]
async fn read(deadline: Instant) -> Vec<u8> {
    use crossterm::event::{Event, KeyCode, KeyEventKind};

    let mut bytes = Vec::new();
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
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
                KeyCode::Char(c) => {
                    let mut utf8 = [0u8; 4];
                    bytes.extend_from_slice(c.encode_utf8(&mut utf8).as_bytes());
                }
                _ => {}
            },
            Ok(_) => {}
            Err(_) => break,
        }
        if Replies::parse(&bytes).complete {
            break;
        }
    }
    bytes
}
