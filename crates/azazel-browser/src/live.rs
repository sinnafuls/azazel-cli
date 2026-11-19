//! The live view of the agents' browser: the pictures a screencast produces, what a user does in
//! them, and what the agent is told afterwards.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

use crate::cdp::{Cdp, CdpEvent};

/// The longest pause between two pictures when nothing is waiting to be sent.
const IDLE: Duration = Duration::from_secs(3600);
/// How many addresses and moves the note to the agent names.
const MOVES: usize = 5;
/// The longest address the note quotes.
const ADDRESS: usize = 200;

/// `[browser] live_fps`, `live_quality` and `live_max_width`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LiveSettings {
    pub fps: u32,
    pub quality: u32,
    pub max_width: u32,
}

impl Default for LiveSettings {
    fn default() -> Self {
        LiveSettings {
            fps: 5,
            quality: 60,
            max_width: 1280,
        }
    }
}

impl LiveSettings {
    /// The settings held to what makes sense: 1 to 15 pictures a second, quality 20 to 90 and a
    /// width of 320 to 1920 pixels.
    pub fn clamped(self) -> LiveSettings {
        LiveSettings {
            fps: self.fps.clamp(1, 15),
            quality: self.quality.clamp(20, 90),
            max_width: self.max_width.clamp(320, 1920),
        }
    }
}

/// One picture of the tab being watched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveFrame {
    /// The name of the tab.
    pub tab: String,
    /// A base64 JPEG.
    pub data: String,
    pub width: u32,
    pub height: u32,
    /// Counts the pictures of the session.
    pub seq: u64,
}

/// What the browser is doing, as the web page shows it. Which client holds the browser is for
/// whoever serves the clients to say.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct BrowserStateInfo {
    pub running: bool,
    /// Whether its window is shown on the machine that runs azazel.
    pub visible: bool,
    pub tab: Option<String>,
    pub url: Option<String>,
    pub title: Option<String>,
    /// Whether a client has taken the browser over and the agents wait.
    pub held: bool,
    pub watchers: u32,
}

/// The button of a click.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LiveButton {
    #[default]
    Left,
    Middle,
    Right,
}

impl LiveButton {
    /// The name `Input.dispatchMouseEvent` knows the button by.
    pub(crate) fn name(self) -> &'static str {
        match self {
            LiveButton::Left => "left",
            LiveButton::Middle => "middle",
            LiveButton::Right => "right",
        }
    }
}

/// What a user does in the picture. `x` and `y` are fractions (0 to 1) of the picture.
#[derive(Debug, Clone, PartialEq)]
pub enum LiveInput {
    Click {
        x: f64,
        y: f64,
        button: LiveButton,
        count: u8,
    },
    Scroll {
        x: f64,
        y: f64,
        dy: f64,
    },
    Key {
        keys: String,
    },
    Text {
        text: String,
    },
    Navigate {
        url: String,
    },
    Back,
    Reload,
}

/// What the last screencast picture shows, to map a position in it to the page.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct FrameMeta {
    /// Size of the screen the picture shows, in device-independent pixels.
    pub device_width: f64,
    pub device_height: f64,
    /// The height of the browser's own controls above the page, inside `device_height`.
    pub offset_top: f64,
    /// Pinch zoom: the page is seen this many times larger than its CSS pixels.
    pub scale: f64,
}

impl FrameMeta {
    /// From the `metadata` of a `Page.screencastFrame`.
    pub fn from_json(metadata: &Value) -> Option<FrameMeta> {
        let number = |key: &str| metadata.get(key).and_then(Value::as_f64);
        let meta = FrameMeta {
            device_width: number("deviceWidth")?,
            device_height: number("deviceHeight")?,
            offset_top: number("offsetTop").unwrap_or(0.0),
            scale: number("pageScaleFactor").unwrap_or(1.0),
        };
        (meta.device_width > 0.0 && meta.device_height > 0.0).then_some(meta)
    }

    /// The viewport position, in CSS pixels, of the point at fractions `fx`,`fy` of the picture.
    /// The picture covers the whole screen, so the controls above the page are taken off.
    pub fn to_css(self, fx: f64, fy: f64) -> (f64, f64) {
        let fraction = |f: f64| if f.is_finite() { f.clamp(0.0, 1.0) } else { 0.0 };
        let scale = if self.scale > 0.0 { self.scale } else { 1.0 };
        (
            fraction(fx) * self.device_width / scale,
            (fraction(fy) * self.device_height - self.offset_top).max(0.0) / scale,
        )
    }
}

/// Whether a picture may be sent at `now` when the last one went at `last`, at most `fps` a second.
pub(crate) fn due(last: Instant, now: Instant, fps: u32) -> bool {
    now.saturating_duration_since(last) >= interval(fps)
}

fn interval(fps: u32) -> Duration {
    Duration::from_millis(1000 / u64::from(fps.max(1)))
}

/// What a user did in the browser while the agent waited, to tell the agent. What was typed is
/// never kept: it may be a password.
#[derive(Debug, Default)]
pub struct UserActions {
    moves: Vec<String>,
    clicks: u32,
    scrolls: u32,
    fields: u32,
    /// Whether the last thing was typing, which the next key belongs to as well.
    typing: bool,
    /// How long the user had the browser without input before it was taken back.
    lapsed: Option<Duration>,
}

impl UserActions {
    /// The user's hold ran out after `lease` without input.
    pub(crate) fn lapsed(&mut self, lease: Duration) {
        self.lapsed = Some(lease);
    }

    pub(crate) fn record(&mut self, input: &LiveInput) {
        let mut moved = |what: String| {
            if self.moves.len() == MOVES {
                self.moves.remove(0);
            }
            self.moves.push(what);
        };
        match input {
            LiveInput::Click { .. } => {
                self.clicks += 1;
                self.typing = false;
            }
            LiveInput::Scroll { .. } => self.scrolls += 1,
            LiveInput::Key { .. } | LiveInput::Text { .. } => {
                if !self.typing {
                    self.fields += 1;
                }
                self.typing = true;
            }
            LiveInput::Navigate { url } => {
                let url: String = url.chars().take(ADDRESS).collect();
                moved(format!("went to {url}"));
                self.typing = false;
            }
            LiveInput::Back => {
                moved("went back".to_string());
                self.typing = false;
            }
            LiveInput::Reload => {
                moved("reloaded the page".to_string());
                self.typing = false;
            }
        }
    }

    /// The line that starts the agent's next browser result; `None` when the user did nothing.
    pub fn note(&self) -> Option<String> {
        let times = |n: u32| {
            if n == 1 {
                "once".to_string()
            } else {
                format!("{n} times")
            }
        };
        let mut parts = self.moves.clone();
        if self.clicks > 0 {
            parts.push(format!("clicked {}", times(self.clicks)));
        }
        if self.scrolls > 0 {
            parts.push(format!("scrolled {}", times(self.scrolls)));
        }
        if self.fields > 0 {
            let fields = if self.fields == 1 { "field" } else { "fields" };
            parts.push(format!("typed in {} {fields}", self.fields));
        }
        if let Some(lease) = self.lapsed {
            let secs = lease.as_secs();
            let span = if secs >= 120 {
                format!("{} minutes", secs / 60)
            } else {
                format!("{secs} s")
            };
            parts.push(format!(
                "gave no input for {span}, so the browser came back to you"
            ));
        }
        (!parts.is_empty()).then(|| {
            format!(
                "the user used the browser while you waited: {}. Refs from earlier snapshots may be stale.\n",
                parts.join(", ")
            )
        })
    }
}

/// The pictures and what is known of them, shared by the session and the task that reads the
/// screencast.
pub(crate) struct Feed {
    pub tx: watch::Sender<Option<Arc<LiveFrame>>>,
    seq: AtomicU64,
    meta: Mutex<Option<FrameMeta>>,
}

impl Feed {
    pub fn new() -> Feed {
        Feed {
            tx: watch::channel(None).0,
            seq: AtomicU64::new(0),
            meta: Mutex::new(None),
        }
    }

    pub fn meta(&self) -> Option<FrameMeta> {
        *self.meta.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn set_meta(&self, meta: Option<FrameMeta>) {
        *self.meta.lock().unwrap_or_else(PoisonError::into_inner) = meta;
    }

    /// Sends a picture of `tab` to whoever watches. `data` is a base64 JPEG.
    pub fn publish(&self, tab: &str, data: String) {
        let size = base64::engine::general_purpose::STANDARD
            .decode(&data)
            .ok()
            .and_then(|bytes| jpeg_size(&bytes))
            .or_else(|| {
                self.meta()
                    .map(|meta| (meta.device_width as u32, meta.device_height as u32))
            })
            .unwrap_or_default();
        self.tx.send_replace(Some(Arc::new(LiveFrame {
            tab: tab.to_string(),
            data,
            width: size.0,
            height: size.1,
            seq: self.seq.fetch_add(1, Ordering::Relaxed) + 1,
        })));
    }

    /// Forgets the last picture: nothing is watched any more.
    pub fn clear(&self) {
        self.tx.send_replace(None);
        self.set_meta(None);
    }
}

/// `(width, height)` of a JPEG, from its first frame header.
pub(crate) fn jpeg_size(bytes: &[u8]) -> Option<(u32, u32)> {
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return None;
    }
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            at += 1;
            continue;
        }
        let marker = bytes[at + 1];
        match marker {
            0xFF => at += 1,
            // Markers that stand alone.
            0x01 | 0xD0..=0xD9 => at += 2,
            // The frame headers (not the tables that sit among them).
            0xC0..=0xCF if !matches!(marker, 0xC4 | 0xC8 | 0xCC) => {
                let header = bytes.get(at + 5..at + 9)?;
                let height = u32::from(u16::from_be_bytes([header[0], header[1]]));
                let width = u32::from(u16::from_be_bytes([header[2], header[3]]));
                return Some((width, height));
            }
            _ => at += 2 + usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]])),
        }
    }
    None
}

/// Reads the `Page.screencastFrame` events of `session` from `events` until `cancel` fires or the
/// connection ends: acknowledges each at once so the browser never stalls, remembers where the
/// picture sits on the page, and sends the newest picture at most `fps` times a second. A picture
/// that arrives too soon waits, so that the last one of a burst is never lost.
pub(crate) async fn pump(
    cdp: Arc<Cdp>,
    mut events: Receiver<Arc<CdpEvent>>,
    (session, tab): (String, String),
    feed: Arc<Feed>,
    fps: u32,
    cancel: CancellationToken,
) {
    let every = interval(fps);
    let mut pending: Option<String> = None;
    let mut last: Option<Instant> = None;
    loop {
        let wait = match (&pending, last) {
            (Some(_), Some(at)) => every.saturating_sub(at.elapsed()),
            (Some(_), None) => Duration::ZERO,
            _ => IDLE,
        };
        tokio::select! {
            _ = cancel.cancelled() => break,
            event = events.recv() => match event {
                Ok(event) if event.session.as_deref() == Some(session.as_str())
                    && event.method == "Page.screencastFrame" =>
                {
                    if let Some(number) = event.params.get("sessionId").and_then(Value::as_i64) {
                        let (cdp, session) = (cdp.clone(), session.clone());
                        tokio::spawn(async move {
                            let _ = cdp
                                .call(Some(&session), "Page.screencastFrameAck", json!({ "sessionId": number }))
                                .await;
                        });
                    }
                    if let Some(meta) = event.params.get("metadata").and_then(FrameMeta::from_json) {
                        feed.set_meta(Some(meta));
                    }
                    if let Some(data) = event.params.get("data").and_then(Value::as_str) {
                        let now = Instant::now();
                        if last.is_none_or(|at| due(at, now, fps)) {
                            feed.publish(&tab, data.to_string());
                            (last, pending) = (Some(now), None);
                        } else {
                            pending = Some(data.to_string());
                        }
                    }
                }
                Ok(_) | Err(RecvError::Lagged(_)) => {}
                Err(RecvError::Closed) => break,
            },
            _ = tokio::time::sleep(wait), if pending.is_some() => {
                if let Some(data) = pending.take() {
                    feed.publish(&tab, data);
                    last = Some(Instant::now());
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn a_position_in_the_picture_is_mapped_to_css_pixels_of_the_page() {
        let meta = FrameMeta {
            device_width: 800.0,
            device_height: 600.0,
            offset_top: 0.0,
            scale: 1.0,
        };
        assert_eq!(meta.to_css(0.5, 0.25), (400.0, 150.0));
        // Outside the picture and not a number are held to its edge.
        assert_eq!(meta.to_css(2.0, -1.0), (800.0, 0.0));
        assert_eq!(meta.to_css(f64::NAN, 1.0), (0.0, 600.0));
        // The browser's own controls are not part of the page; pinch zoom shows less of it.
        let zoomed = FrameMeta {
            device_height: 640.0,
            offset_top: 40.0,
            scale: 2.0,
            ..meta
        };
        assert_eq!(zoomed.to_css(1.0, 1.0), (400.0, 300.0));
    }

    #[test]
    fn the_metadata_of_a_frame_needs_a_size() {
        let meta =
            FrameMeta::from_json(&json!({ "deviceWidth": 1280, "deviceHeight": 720, "pageScaleFactor": 1 }));
        assert_eq!(meta.map(|meta| meta.device_height), Some(720.0));
        assert!(FrameMeta::from_json(&json!({ "deviceWidth": 0, "deviceHeight": 720 })).is_none());
        assert!(FrameMeta::from_json(&json!({})).is_none());
    }

    #[test]
    fn the_note_tells_what_was_done_and_never_what_was_typed() {
        let mut actions = UserActions::default();
        assert_eq!(actions.note(), None);
        let click = LiveInput::Click {
            x: 0.1,
            y: 0.2,
            button: LiveButton::Left,
            count: 1,
        };
        actions.record(&LiveInput::Navigate {
            url: "https://example.com/in".into(),
        });
        for _ in 0..3 {
            actions.record(&click);
        }
        actions.record(&LiveInput::Text {
            text: "hunter2".into(),
        });
        actions.record(&LiveInput::Key { keys: "Enter".into() });
        actions.record(&LiveInput::Scroll {
            x: 0.5,
            y: 0.5,
            dy: 120.0,
        });
        let note = actions.note().unwrap();
        assert_eq!(
            note,
            "the user used the browser while you waited: went to https://example.com/in, clicked 3 times, \
             scrolled once, typed in 1 field. Refs from earlier snapshots may be stale.\n"
        );
        assert!(!note.contains("hunter2"));
        // A click ends a field: the next key is another one.
        actions.record(&click);
        actions.record(&LiveInput::Text { text: "x".into() });
        assert!(actions.note().unwrap().contains("typed in 2 fields"));
    }

    #[test]
    fn the_note_names_the_last_few_moves_only() {
        let mut actions = UserActions::default();
        for n in 0..8 {
            actions.record(&LiveInput::Navigate {
                url: format!("https://example.com/{n}"),
            });
        }
        let note = actions.note().unwrap();
        assert!(
            !note.contains("/2,") && note.contains("/3,") && note.contains("/7"),
            "{note}"
        );
    }

    #[test]
    fn pictures_are_due_at_the_pace_of_the_rate() {
        let start = Instant::now();
        assert!(!due(start, start + Duration::from_millis(199), 5));
        assert!(due(start, start + Duration::from_millis(200), 5));
        assert!(!due(start, start + Duration::from_millis(500), 1));
        assert_eq!(
            LiveSettings {
                fps: 99,
                quality: 1,
                max_width: 5
            }
            .clamped(),
            LiveSettings {
                fps: 15,
                quality: 20,
                max_width: 320
            }
        );
    }

    #[test]
    fn the_size_of_a_jpeg_is_read_from_its_frame_header() {
        // SOI, an APP0 segment of 4 bytes, then SOF0 of a 3 by 2 picture.
        let bytes = [
            0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x00, 0x00, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x02, 0x00,
            0x03, 0x01,
        ];
        assert_eq!(jpeg_size(&bytes), Some((3, 2)));
        assert_eq!(jpeg_size(b"not a picture"), None);
    }
}
