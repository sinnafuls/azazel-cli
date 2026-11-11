//! The `screen` tool: capture windows and displays, and send mouse and keyboard input to whatever
//! is on screen. This is how native applications and games are driven.

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_ai::Block;
use azazel_core::ArtifactStore;
use azazel_core::media::MAX_IMAGE_SIDE;
use base64::Engine;
use enigo::{Axis, Button, Coordinate, Direction, Enigo, Keyboard, Mouse};
use image::imageops::FilterType;
use image::{ImageFormat, RgbaImage};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::page::{self, ALT, CONTROL, META, SHIFT};

pub const NO_CAPTURE: &str = "capture first: coordinates refer to the last capture";

const MAX_WAIT_MS: u64 = 10_000;
const MAX_RECORD_SECONDS: f64 = 60.0;
const MAX_RECORD_FPS: f64 = 10.0;
const MAX_DRAG: Duration = Duration::from_secs(5);
const INPUT_SETTLE: Duration = Duration::from_millis(150);

/// How pixels of the image last shown to the model map to the screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Mapping {
    /// Screen position of the capture's top-left corner.
    pub origin: (i32, i32),
    /// Width and height the capture covers on screen, in the units input events use.
    pub extent: (u32, u32),
    /// Width and height of the image as it was returned.
    pub shown: (u32, u32),
}

impl Mapping {
    /// The screen position of pixel `x`,`y` of the returned image.
    pub fn to_screen(&self, x: f64, y: f64) -> Result<(i32, i32), String> {
        let (width, height) = (self.shown.0 as f64, self.shown.1 as f64);
        if !(0.0..width).contains(&x) || !(0.0..height).contains(&y) {
            return Err(format!(
                "{x},{y} is outside the last capture, which is {}×{}",
                self.shown.0, self.shown.1
            ));
        }
        let along = |value: f64, shown: f64, extent: u32| {
            ((value * extent as f64 / shown) as i32).min(extent.saturating_sub(1) as i32)
        };
        Ok((
            self.origin.0 + along(x, width, self.extent.0),
            self.origin.1 + along(y, height, self.extent.1),
        ))
    }
}

/// `width`×`height` scaled down, keeping its shape, so that neither side exceeds `max`.
pub fn fit(width: u32, height: u32, max: u32) -> (u32, u32) {
    let long = width.max(height);
    if long <= max {
        return (width, height);
    }
    let scale = |side: u32| ((side as u64 * max as u64 + long as u64 / 2) / long as u64).max(1) as u32;
    (scale(width), scale(height))
}

/// At most `wanted` indices into `count` items, spread from the first to the last.
pub fn evenly_spaced(count: usize, wanted: usize) -> Vec<usize> {
    if count <= wanted || wanted < 2 {
        return (0..count.min(wanted)).collect();
    }
    (0..wanted).map(|i| i * (count - 1) / (wanted - 1)).collect()
}

/// Up to nine frames as one image, three to a row in reading order, no side longer than
/// `max_side`. `None` without frames.
pub fn contact_sheet(frames: &[RgbaImage], max_side: u32) -> Option<RgbaImage> {
    let first = frames.first()?;
    let frames = &frames[..frames.len().min(9)];
    let columns = frames.len().min(3) as u32;
    let rows = (frames.len() as u32).div_ceil(3);
    let (tile_width, tile_height) = fit(
        first.width(),
        first.height(),
        (max_side / columns).min(max_side / rows),
    );
    let mut sheet = RgbaImage::from_pixel(
        tile_width * columns,
        tile_height * rows,
        image::Rgba([0, 0, 0, 255]),
    );
    for (index, frame) in frames.iter().enumerate() {
        // A frame of another shape (the window was resized) is centred in its tile.
        let (width, height) = fit(frame.width(), frame.height(), tile_width.max(tile_height));
        let (width, height) = (width.min(tile_width), height.min(tile_height));
        let tile = image::imageops::resize(frame, width, height, FilterType::Triangle);
        let (column, row) = (index as u32 % 3, index as u32 / 3);
        let left = column * tile_width + (tile_width - width) / 2;
        let top = row * tile_height + (tile_height - height) / 2;
        image::imageops::replace(&mut sheet, &tile, left.into(), top.into());
    }
    Some(sheet)
}

/// The part of `image` that `region` (`[x, y, width, height]` in the units of `extent`) selects.
fn crop(image: &RgbaImage, extent: (u32, u32), region: [u32; 4]) -> Result<RgbaImage, String> {
    let [x, y, width, height] = region;
    let inside = width > 0
        && height > 0
        && x.checked_add(width).is_some_and(|right| right <= extent.0)
        && y.checked_add(height).is_some_and(|bottom| bottom <= extent.1);
    if !inside {
        return Err(format!(
            "region {x},{y} {width}×{height} does not lie within the target, which is {}×{}",
            extent.0, extent.1
        ));
    }
    let across = |value: u32, of: u32, pixels: u32| (value as u64 * pixels as u64 / of as u64) as u32;
    let (left, top) = (
        across(x, extent.0, image.width()),
        across(y, extent.1, image.height()),
    );
    let wide = across(width, extent.0, image.width()).max(1);
    let tall = across(height, extent.1, image.height()).max(1);
    Ok(image::imageops::crop_imm(image, left, top, wide, tall).to_image())
}

fn png(image: &RgbaImage) -> Result<Vec<u8>, String> {
    let mut bytes = Vec::new();
    image
        .write_to(&mut Cursor::new(&mut bytes), ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(bytes)
}

/// An image block of `image`, scaled to what a model takes, and the size it was scaled to.
fn shown_block(image: &RgbaImage) -> Result<(Block, (u32, u32)), String> {
    let (width, height) = fit(image.width(), image.height(), MAX_IMAGE_SIDE);
    let bytes = if (width, height) == image.dimensions() {
        png(image)?
    } else {
        png(&image::imageops::resize(
            image,
            width,
            height,
            FilterType::Triangle,
        ))?
    };
    let block = Block::Image {
        mime: "image/png".to_string(),
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
    };
    Ok((block, (width, height)))
}

/// The key of a parsed key name, for the input library.
fn input_key(key: &page::Key) -> Result<enigo::Key, String> {
    use enigo::Key;
    Ok(match key.key.as_str() {
        "Enter" => Key::Return,
        "Tab" => Key::Tab,
        "Escape" => Key::Escape,
        "Backspace" => Key::Backspace,
        "Delete" => Key::Delete,
        "Insert" => Key::Insert,
        " " => Key::Space,
        "ArrowUp" => Key::UpArrow,
        "ArrowDown" => Key::DownArrow,
        "ArrowLeft" => Key::LeftArrow,
        "ArrowRight" => Key::RightArrow,
        "Home" => Key::Home,
        "End" => Key::End,
        "PageUp" => Key::PageUp,
        "PageDown" => Key::PageDown,
        "F1" => Key::F1,
        "F2" => Key::F2,
        "F3" => Key::F3,
        "F4" => Key::F4,
        "F5" => Key::F5,
        "F6" => Key::F6,
        "F7" => Key::F7,
        "F8" => Key::F8,
        "F9" => Key::F9,
        "F10" => Key::F10,
        "F11" => Key::F11,
        "F12" => Key::F12,
        other => {
            let mut chars = other.chars();
            match (chars.next(), chars.next()) {
                // Shift is held as a modifier, so the key itself is the unshifted one.
                (Some(c), None) => Key::Unicode(c.to_ascii_lowercase()),
                _ => return Err(format!("the key `{other}` cannot be sent")),
            }
        }
    })
}

fn held_modifiers(modifiers: u8) -> Vec<enigo::Key> {
    [
        (CONTROL, enigo::Key::Control),
        (ALT, enigo::Key::Alt),
        (SHIFT, enigo::Key::Shift),
        (META, enigo::Key::Meta),
    ]
    .into_iter()
    .filter(|(bit, _)| modifiers & bit != 0)
    .map(|(_, key)| key)
    .collect()
}

/// Runs `f` with a connection to the system's input injection; the library's error is returned
/// as it is. Input is queued for the application, which handles it in its own time, so a capture
/// taken right after would show the screen before the input: give it a moment.
fn inject<T>(f: impl FnOnce(&mut Enigo) -> enigo::InputResult<T>) -> Result<T, String> {
    let mut enigo = Enigo::new(&enigo::Settings::default()).map_err(|e| e.to_string())?;
    let done = f(&mut enigo).map_err(|e| e.to_string())?;
    std::thread::sleep(INPUT_SETTLE);
    Ok(done)
}

fn mouse_button(name: Option<&str>) -> Result<Button, String> {
    match name.unwrap_or("left") {
        "left" => Ok(Button::Left),
        "right" => Ok(Button::Right),
        "middle" => Ok(Button::Middle),
        other => Err(format!("unknown button `{other}`; use left, right or middle")),
    }
}

/// The window `selector` names: its id, or part of its title (case ignored). A visible window is
/// preferred over a minimised one.
fn find_window(selector: &Value) -> Result<xcap::Window, String> {
    let id = selector
        .as_u64()
        .or_else(|| selector.as_str().and_then(|text| text.trim().parse().ok()));
    let title = selector.as_str().unwrap_or_default().to_lowercase();
    let mut matches: Vec<xcap::Window> = xcap::Window::all()
        .map_err(|e| e.to_string())?
        .into_iter()
        .filter(|window| match id {
            Some(id) => window.id().is_ok_and(|own| u64::from(own) == id),
            None => {
                !title.is_empty()
                    && window
                        .title()
                        .is_ok_and(|own| own.to_lowercase().contains(&title))
            }
        })
        .collect();
    matches.sort_by_key(|window| window.is_minimized().unwrap_or(false));
    matches
        .into_iter()
        .next()
        .ok_or_else(|| format!("no window matches {selector}; list them with the `windows` action"))
}

/// One captured target.
struct Grab {
    image: RgbaImage,
    origin: (i32, i32),
    extent: (u32, u32),
    label: String,
}

fn grab(window: Option<&Value>, display: Option<usize>, region: Option<[u32; 4]>) -> Result<Grab, String> {
    let lib = |e: xcap::XCapError| e.to_string();
    let mut grab = match window {
        Some(selector) => {
            let window = find_window(selector)?;
            Grab {
                image: window.capture_image().map_err(lib)?,
                origin: (window.x().map_err(lib)?, window.y().map_err(lib)?),
                extent: (window.width().map_err(lib)?, window.height().map_err(lib)?),
                label: format!("window \"{}\"", window.title().unwrap_or_default()),
            }
        }
        None => {
            let monitors = xcap::Monitor::all().map_err(lib)?;
            let count = monitors.len();
            let (index, monitor) = match display {
                Some(index) => monitors.into_iter().enumerate().nth(index),
                None => monitors
                    .into_iter()
                    .enumerate()
                    .min_by_key(|(_, monitor)| !monitor.is_primary().unwrap_or(false)),
            }
            .ok_or_else(|| {
                format!(
                    "there is no display {}; displays: 0..{count}",
                    display.unwrap_or(0)
                )
            })?;
            Grab {
                image: monitor.capture_image().map_err(lib)?,
                origin: (monitor.x().map_err(lib)?, monitor.y().map_err(lib)?),
                extent: (monitor.width().map_err(lib)?, monitor.height().map_err(lib)?),
                label: format!("display {index}"),
            }
        }
    };
    if let Some(region) = region {
        grab.image = crop(&grab.image, grab.extent, region)?;
        grab.origin = (grab.origin.0 + region[0] as i32, grab.origin.1 + region[1] as i32);
        grab.extent = (region[2], region[3]);
    }
    if grab.image.width() == 0 || grab.image.height() == 0 || grab.extent.0 == 0 || grab.extent.1 == 0 {
        return Err(format!("{} has no visible area (minimised?)", grab.label));
    }
    Ok(grab)
}

fn list_windows() -> Result<String, String> {
    let mut out = String::new();
    for window in xcap::Window::all().map_err(|e| e.to_string())? {
        let (Ok(id), Ok(title)) = (window.id(), window.title()) else {
            continue;
        };
        if title.trim().is_empty() {
            continue;
        }
        out.push_str(&format!(
            "{id} \"{title}\" {} at {},{} {}×{}{}\n",
            window.app_name().unwrap_or_default(),
            window.x().unwrap_or(0),
            window.y().unwrap_or(0),
            window.width().unwrap_or(0),
            window.height().unwrap_or(0),
            if window.is_minimized().unwrap_or(false) {
                " minimised"
            } else {
                ""
            },
        ));
    }
    Ok(if out.is_empty() {
        "no windows\n".to_string()
    } else {
        out
    })
}

/// Brings `window` to the front and gives it the keyboard.
#[cfg(windows)]
fn focus(window: &xcap::Window) -> Result<(), String> {
    use windows_sys::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        BringWindowToTop, GetForegroundWindow, GetWindowThreadProcessId, IsIconic, SW_RESTORE,
        SetForegroundWindow, ShowWindow,
    };
    let hwnd = window.id().map_err(|e| e.to_string())? as usize as windows_sys::Win32::Foundation::HWND;
    // SAFETY: these calls take a window handle by value and tolerate a stale one (they fail);
    // the thread ids come from the system, and the attachment made here is undone before
    // returning.
    let brought = unsafe {
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        }
        // Windows lets only the foreground thread hand the foreground on: join its input queue
        // for the moment of the switch.
        let this = GetCurrentThreadId();
        let front = GetWindowThreadProcessId(GetForegroundWindow(), std::ptr::null_mut());
        let attached = front != 0 && front != this && AttachThreadInput(this, front, 1) != 0;
        let brought = SetForegroundWindow(hwnd) != 0;
        BringWindowToTop(hwnd);
        if attached {
            AttachThreadInput(this, front, 0);
        }
        brought
    };
    if brought {
        Ok(())
    } else {
        Err("Windows refused to bring the window to the front".to_string())
    }
}

/// Runs a helper program, returning what it printed on failure.
#[cfg(not(windows))]
fn helper(program: &str, args: &[&str]) -> Result<(), String> {
    let output = std::process::Command::new(program)
        .args(args)
        .output()
        .map_err(|e| format!("{program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{program}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// Brings `window` to the front and gives it the keyboard.
#[cfg(target_os = "macos")]
fn focus(window: &xcap::Window) -> Result<(), String> {
    let pid = window.pid().map_err(|e| e.to_string())?;
    let script = format!(
        "tell application \"System Events\" to set frontmost of (first process whose unix id is {pid}) to true"
    );
    helper("osascript", &["-e", &script])
}

/// Brings `window` to the front and gives it the keyboard.
#[cfg(not(any(windows, target_os = "macos")))]
fn focus(window: &xcap::Window) -> Result<(), String> {
    let id = window.id().map_err(|e| e.to_string())?.to_string();
    if which::which("xdotool").is_ok() {
        helper("xdotool", &["windowactivate", "--sync", &id])
    } else if which::which("wmctrl").is_ok() {
        helper("wmctrl", &["-i", "-a", &id])
    } else {
        Err("focusing a window needs `xdotool` or `wmctrl` on PATH".to_string())
    }
}

#[derive(Debug, Default, Deserialize)]
#[serde(default)]
struct Input {
    action: String,
    window: Option<Value>,
    display: Option<usize>,
    region: Option<[u32; 4]>,
    x: Option<f64>,
    y: Option<f64>,
    button: Option<String>,
    double: bool,
    from: Option<[f64; 2]>,
    to: Option<[f64; 2]>,
    duration_ms: Option<u64>,
    dy: Option<i32>,
    keys: Option<String>,
    hold_ms: Option<u64>,
    text: Option<String>,
    ms: Option<u64>,
    seconds: Option<f64>,
    fps: Option<f64>,
}

#[derive(Default)]
struct State {
    mapping: Mutex<Option<Mapping>>,
    shots: AtomicU32,
}

impl State {
    fn screen(&self, x: Option<f64>, y: Option<f64>, action: &str) -> Result<(i32, i32), String> {
        let (Some(x), Some(y)) = (x, y) else {
            return Err(format!("`{action}` needs `x` and `y`"));
        };
        let mapping = *self.mapping.lock().unwrap_or_else(PoisonError::into_inner);
        mapping.ok_or_else(|| NO_CAPTURE.to_string())?.to_screen(x, y)
    }
}

/// Makes window positions, capture pixels and input coordinates one coordinate space. Windows
/// otherwise reports them to this process scaled by the display's zoom, each API in its own way.
fn one_coordinate_space() {
    #[cfg(windows)]
    {
        static ONCE: std::sync::Once = std::sync::Once::new();
        // Refused when the awareness was already set, which is as good.
        ONCE.call_once(|| {
            let _ = enigo::set_dpi_awareness();
        });
    }
}

/// The `screen` tool.
#[derive(Default)]
pub struct ScreenTool {
    state: Arc<State>,
}

const DESCRIPTION: &str = "See and operate the desktop: list windows, capture a window or display as \
an image, focus a window, move and click the mouse, drag, scroll, press keys, type text, and record a \
short clip. Use it to run and playtest native applications and games. `x` and `y` are pixels of the \
image the last `capture` returned, so capture before pointing, and capture again after acting to see \
the result.";

/// What the blocking part of an action hands back.
type Done = Result<Vec<Block>, String>;

fn say(text: impl Into<String>) -> Done {
    Ok(vec![Block::text(text)])
}

impl ScreenTool {
    fn capture(state: &State, input: &Input, store: &ArtifactStore) -> Done {
        let grab = grab(input.window.as_ref(), input.display, input.region)?;
        let number = state.shots.fetch_add(1, Ordering::Relaxed) + 1;
        let name = format!("screen-{number}.png");
        let path = store
            .save(&name, &png(&grab.image)?)
            .map_err(|e| format!("could not save {name}: {e}"))?;
        let (block, shown) = shown_block(&grab.image)?;
        *state.mapping.lock().unwrap_or_else(PoisonError::into_inner) = Some(Mapping {
            origin: grab.origin,
            extent: grab.extent,
            shown,
        });
        Ok(vec![
            Block::text(format!(
                "captured {}: {}×{}, saved {}. `x` and `y` of click, move, drag and scroll are pixels of this image.",
                grab.label,
                shown.0,
                shown.1,
                path.display()
            )),
            block,
        ])
    }

    /// The actions that finish quickly and block while they run.
    fn act(state: &State, input: &Input, store: Option<&ArtifactStore>) -> Done {
        match input.action.as_str() {
            "windows" => say(list_windows()?),
            "capture" => Self::capture(state, input, store.ok_or("there is nowhere to save the capture")?),
            "focus" => {
                let window = find_window(input.window.as_ref().ok_or("`focus` needs `window`")?)?;
                focus(&window)?;
                say(format!("focused \"{}\"", window.title().unwrap_or_default()))
            }
            "move" | "click" => {
                let (x, y) = state.screen(input.x, input.y, &input.action)?;
                let button = mouse_button(input.button.as_deref())?;
                let presses = match (input.action.as_str(), input.double) {
                    ("move", _) => 0,
                    (_, false) => 1,
                    (_, true) => 2,
                };
                inject(|enigo| {
                    enigo.move_mouse(x, y, Coordinate::Abs)?;
                    for _ in 0..presses {
                        enigo.button(button, Direction::Click)?;
                    }
                    Ok(())
                })?;
                say(match presses {
                    0 => format!("pointer at screen {x},{y}"),
                    1 => format!("clicked at screen {x},{y}"),
                    _ => format!("double-clicked at screen {x},{y}"),
                })
            }
            "drag" => {
                let (Some(from), Some(to)) = (input.from, input.to) else {
                    return Err("`drag` needs `from` and `to`, each `[x, y]`".to_string());
                };
                let start = state.screen(Some(from[0]), Some(from[1]), "drag")?;
                let end = state.screen(Some(to[0]), Some(to[1]), "drag")?;
                let duration = Duration::from_millis(input.duration_ms.unwrap_or(300)).min(MAX_DRAG);
                let steps = (duration.as_millis() / 16).max(1) as i32;
                inject(|enigo| {
                    enigo.move_mouse(start.0, start.1, Coordinate::Abs)?;
                    enigo.button(Button::Left, Direction::Press)?;
                    // Applications follow a drag by the moves in between, not by its ends.
                    let moved = (1..=steps).try_for_each(|step| {
                        std::thread::sleep(duration / steps as u32);
                        let along = |a: i32, b: i32| a + (b - a) * step / steps;
                        enigo.move_mouse(along(start.0, end.0), along(start.1, end.1), Coordinate::Abs)
                    });
                    enigo.button(Button::Left, Direction::Release)?;
                    moved
                })?;
                say(format!(
                    "dragged from screen {},{} to {},{}",
                    start.0, start.1, end.0, end.1
                ))
            }
            "scroll" => {
                let (x, y) = state.screen(input.x, input.y, "scroll")?;
                let dy = input.dy.ok_or("`scroll` needs `dy`")?;
                inject(|enigo| {
                    enigo.move_mouse(x, y, Coordinate::Abs)?;
                    enigo.scroll(dy, Axis::Vertical)
                })?;
                say(format!("scrolled {dy} at screen {x},{y}"))
            }
            "key" => {
                let keys = input.keys.as_deref().ok_or("`key` needs `keys`")?;
                let parsed = page::parse_keys(keys)?;
                let key = input_key(&parsed)?;
                let modifiers = held_modifiers(parsed.modifiers);
                let hold = Duration::from_millis(input.hold_ms.unwrap_or(0).min(MAX_WAIT_MS));
                inject(|enigo| {
                    for modifier in &modifiers {
                        enigo.key(*modifier, Direction::Press)?;
                    }
                    let pressed = enigo.key(key, Direction::Press);
                    if pressed.is_ok() {
                        std::thread::sleep(hold);
                    }
                    // Released even when the press failed, so no key stays down.
                    let released = enigo.key(key, Direction::Release);
                    for modifier in modifiers.iter().rev() {
                        enigo.key(*modifier, Direction::Release)?;
                    }
                    pressed.and(released)
                })?;
                say(format!("pressed {keys}"))
            }
            "type" => {
                let text = input.text.as_deref().ok_or("`type` needs `text`")?;
                inject(|enigo| enigo.text(text))?;
                say(format!("typed {} characters", text.chars().count()))
            }
            other => Err(format!(
                "unknown action `{other}`; actions: windows, capture, focus, click, move, drag, scroll, key, \
                 type, wait, record"
            )),
        }
    }

    /// Captures frames for a while, then joins them into a video when `ffmpeg` is installed.
    async fn record(&self, input: Input, ctx: &ToolCtx, store: &ArtifactStore) -> Done {
        let seconds = input.seconds.ok_or("`record` needs `seconds`")?;
        if !(seconds > 0.0 && seconds <= MAX_RECORD_SECONDS) {
            return Err(format!(
                "`seconds` must be above 0 and at most {MAX_RECORD_SECONDS}"
            ));
        }
        let fps = input.fps.unwrap_or(5.0);
        if !(fps > 0.0 && fps <= MAX_RECORD_FPS) {
            return Err(format!("`fps` must be above 0 and at most {MAX_RECORD_FPS}"));
        }
        let stamp = jiff::Timestamp::now().strftime("%Y%m%dT%H%M%SZ").to_string();
        let dir = store.dir().join("recordings").join(stamp);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;

        let interval = Duration::from_secs_f64(1.0 / fps);
        let wanted = (seconds * fps).ceil().max(1.0) as usize;
        let begun = Instant::now();
        let mut written: Vec<PathBuf> = Vec::new();
        for index in 0..wanted {
            // A frame that took longer than the interval makes the next ones late, not denser.
            let due = interval * index as u32;
            tokio::select! {
                _ = tokio::time::sleep(due.saturating_sub(begun.elapsed())) => {}
                _ = ctx.cancel.cancelled() => return Err(ABORTED_TEXT.to_string()),
            }
            if begun.elapsed() > Duration::from_secs_f64(seconds) + interval {
                break;
            }
            let path = dir.join(format!("{:04}.png", written.len() + 1));
            let (window, display, region, file) =
                (input.window.clone(), input.display, input.region, path.clone());
            tokio::task::spawn_blocking(move || {
                let frame = grab(window.as_ref(), display, region)?;
                std::fs::write(&file, png(&frame.image)?).map_err(|e| format!("{}: {e}", file.display()))
            })
            .await
            .map_err(|e| e.to_string())??;
            written.push(path);
        }

        let mut report = format!("recorded {} frames to {}", written.len(), dir.display());
        if which::which("ffmpeg").is_ok() {
            match encode(&dir, fps).await {
                Ok(()) => report.push_str(&format!("\nvideo: {}", dir.join("out.mp4").display())),
                Err(why) => report.push_str(&format!("\nno video: {why}")),
            }
        }
        let picked: Vec<PathBuf> = evenly_spaced(written.len(), 9)
            .into_iter()
            .map(|i| written[i].clone())
            .collect();
        let sheet = tokio::task::spawn_blocking(move || -> Result<Option<Block>, String> {
            let frames = picked
                .iter()
                .map(|path| {
                    image::open(path)
                        .map(|image| image.to_rgba8())
                        .map_err(|e| e.to_string())
                })
                .collect::<Result<Vec<_>, _>>()?;
            contact_sheet(&frames, MAX_IMAGE_SIDE)
                .map(|sheet| shown_block(&sheet).map(|(block, _)| block))
                .transpose()
        })
        .await
        .map_err(|e| e.to_string())??;
        report.push_str("\nThe image shows evenly spaced frames, left to right, top to bottom.");
        Ok(std::iter::once(Block::text(report)).chain(sheet).collect())
    }
}

/// Joins the frames in `dir` into `out.mp4`.
async fn encode(dir: &Path, fps: f64) -> Result<(), String> {
    let run = tokio::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel",
            "error",
            "-framerate",
            &fps.to_string(),
            "-i",
            "%04d.png",
        ])
        // H.264 needs even dimensions.
        .args([
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-pix_fmt",
            "yuv420p",
            "out.mp4",
        ])
        .current_dir(dir)
        .stdin(std::process::Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(Duration::from_secs(120), run)
        .await
        .map_err(|_| "ffmpeg did not finish within 120 s".to_string())?
        .map_err(|e| format!("ffmpeg: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "ffmpeg: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

#[async_trait]
impl Tool for ScreenTool {
    fn name(&self) -> &str {
        "screen"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["windows", "capture", "focus", "click", "move", "drag", "scroll", "key", "type", "wait", "record"],
                    "description": "windows: list id, title, app, position and size. capture: image of `window`, \
                        else of `display` (the primary one by default), optionally only `region`. focus: bring \
                        `window` to the front. click/move: at `x`,`y`. drag: `from` to `to`. scroll: `dy` at \
                        `x`,`y`. key: `keys`. type: `text` into the focused window. wait: `ms`. record: frames \
                        of `window` or the display for `seconds`."
                },
                "window": { "type": ["string", "integer"], "description": "Window id, or part of its title" },
                "display": { "type": "integer", "minimum": 0, "description": "Display index" },
                "region": {
                    "type": "array", "items": { "type": "integer", "minimum": 0 }, "minItems": 4, "maxItems": 4,
                    "description": "[x, y, width, height] within the window or display, in its own size as `windows` reports it"
                },
                "x": { "type": "number", "description": "Pixel column of the last capture" },
                "y": { "type": "number", "description": "Pixel row of the last capture" },
                "button": { "type": "string", "enum": ["left", "right", "middle"] },
                "double": { "type": "boolean", "description": "Double-click" },
                "from": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2, "description": "[x, y] where the drag starts" },
                "to": { "type": "array", "items": { "type": "number" }, "minItems": 2, "maxItems": 2, "description": "[x, y] where the drag ends" },
                "duration_ms": { "type": "integer", "minimum": 0, "description": "How long the drag takes (default 300, at most 5000)" },
                "dy": { "type": "integer", "description": "Wheel notches; positive scrolls down" },
                "keys": { "type": "string", "description": "One key with optional modifiers: Enter, Escape, ArrowLeft, w, Control+S, Alt+F4" },
                "hold_ms": { "type": "integer", "minimum": 0, "description": "Keep the key down this long (movement in games); at most 10000" },
                "text": { "type": "string", "description": "Text to type" },
                "ms": { "type": "integer", "minimum": 0, "maximum": MAX_WAIT_MS, "description": "How long to wait" },
                "seconds": { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_RECORD_SECONDS, "description": "Length of the recording" },
                "fps": { "type": "number", "exclusiveMinimum": 0, "maximum": MAX_RECORD_FPS, "description": "Frames per second (default 5)" }
            },
            "required": ["action"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Exec
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        vec![
            input
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
        ]
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match serde_json::from_value(input) {
            Ok(input) => input,
            Err(e) => return ToolOutput::error(format!("invalid input: {e}")),
        };
        one_coordinate_space();
        let store = ctx.services.get::<ArtifactStore>();
        let done = match input.action.as_str() {
            "wait" => match input.ms {
                Some(ms) if ms <= MAX_WAIT_MS => tokio::select! {
                    _ = tokio::time::sleep(Duration::from_millis(ms)) => say(format!("waited {ms} ms")),
                    _ = ctx.cancel.cancelled() => Err(ABORTED_TEXT.to_string()),
                },
                _ => Err(format!("`wait` needs `ms`, at most {MAX_WAIT_MS}")),
            },
            "record" => match &store {
                Some(store) => self.record(input, ctx, store).await,
                None => Err("there is nowhere to save the recording".to_string()),
            },
            _ => {
                let state = self.state.clone();
                tokio::task::spawn_blocking(move || Self::act(&state, &input, store.as_deref()))
                    .await
                    .unwrap_or_else(|e| Err(e.to_string()))
            }
        };
        match done {
            Ok(content) => ToolOutput {
                content,
                is_error: false,
                details: None,
            },
            Err(why) => ToolOutput::error(why),
        }
    }
}

/// Registers the `screen` tool.
pub struct ScreenExt;

impl azazel_core::Extension for ScreenExt {
    fn id(&self) -> &str {
        "screen"
    }

    fn register(&self, r: &mut azazel_core::Registrar) -> Result<(), azazel_core::ExtError> {
        r.tool(Arc::new(ScreenTool::default()));
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use image::Rgba;

    use super::*;

    #[test]
    fn capture_pixels_map_to_the_screen_through_scale_and_origin() {
        // A 3136×1764 window at (-200, 50), shown at half size.
        let mapping = Mapping {
            origin: (-200, 50),
            extent: (3136, 1764),
            shown: (1568, 882),
        };
        assert_eq!(mapping.to_screen(0.0, 0.0), Ok((-200, 50)));
        assert_eq!(mapping.to_screen(100.0, 10.0), Ok((0, 70)));
        assert_eq!(mapping.to_screen(1567.0, 881.0), Ok((-200 + 3134, 50 + 1762)));
        // Fractions (a point between two pixels) are accepted.
        assert_eq!(mapping.to_screen(0.5, 0.5), Ok((-199, 51)));

        // Shown at its own size: pixels are screen units.
        let same = Mapping {
            origin: (10, 20),
            extent: (800, 600),
            shown: (800, 600),
        };
        assert_eq!(same.to_screen(799.0, 599.0), Ok((809, 619)));

        // A capture larger than the screen area it covers (a scaled display).
        let dense = Mapping {
            origin: (0, 0),
            extent: (1280, 720),
            shown: (1568, 882),
        };
        assert_eq!(dense.to_screen(784.0, 441.0), Ok((640, 360)));
        assert_eq!(dense.to_screen(1567.9, 881.9), Ok((1279, 719)));

        for (x, y) in [(800.0, 0.0), (0.0, 600.0), (-1.0, 5.0), (f64::NAN, 5.0)] {
            let error = same.to_screen(x, y).unwrap_err();
            assert!(
                error.contains("outside the last capture, which is 800×600"),
                "{error}"
            );
        }
    }

    #[test]
    fn pointing_before_any_capture_is_refused() {
        let state = State::default();
        assert_eq!(
            state.screen(Some(1.0), Some(1.0), "click").unwrap_err(),
            NO_CAPTURE
        );
        assert_eq!(
            state.screen(Some(1.0), None, "click").unwrap_err(),
            "`click` needs `x` and `y`"
        );
    }

    #[test]
    fn images_are_fitted_keeping_their_shape() {
        assert_eq!(fit(800, 600, 1568), (800, 600));
        assert_eq!(fit(3136, 1764, 1568), (1568, 882));
        assert_eq!(fit(1000, 4000, 1568), (392, 1568));
        // Never down to nothing.
        assert_eq!(fit(10_000, 1, 100), (100, 1));
    }

    #[test]
    fn frames_for_the_contact_sheet_span_the_recording() {
        assert_eq!(evenly_spaced(0, 9), Vec::<usize>::new());
        assert_eq!(evenly_spaced(4, 9), [0, 1, 2, 3]);
        assert_eq!(evenly_spaced(9, 9), [0, 1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(evenly_spaced(50, 9), [0, 6, 12, 18, 24, 30, 36, 42, 49]);
    }

    fn frame(width: u32, height: u32, shade: u8) -> RgbaImage {
        RgbaImage::from_pixel(width, height, Rgba([shade, shade, shade, 255]))
    }

    #[test]
    fn a_contact_sheet_lays_frames_out_three_to_a_row_in_order() {
        let frames: Vec<RgbaImage> = (1..=7).map(|i| frame(400, 200, i * 30)).collect();
        let sheet = contact_sheet(&frames, 600).unwrap();
        // Three columns of 200×100 tiles, three rows for seven frames.
        assert_eq!(sheet.dimensions(), (600, 300));
        let at = |column: u32, row: u32| sheet.get_pixel(column * 200 + 100, row * 100 + 50).0[0];
        assert_eq!([at(0, 0), at(1, 0), at(2, 0)], [30, 60, 90]);
        assert_eq!([at(0, 1), at(1, 1), at(2, 1)], [120, 150, 180]);
        // The last row has one frame; the rest stays black.
        assert_eq!([at(0, 2), at(1, 2), at(2, 2)], [210, 0, 0]);

        // Fewer than three frames make a narrower sheet; more than nine are cut.
        assert_eq!(contact_sheet(&frames[..2], 600).unwrap().dimensions(), (600, 150));
        let many: Vec<RgbaImage> = (0..12).map(|_| frame(90, 90, 9)).collect();
        assert_eq!(contact_sheet(&many, 300).unwrap().dimensions(), (270, 270));
        assert!(contact_sheet(&[], 600).is_none());

        // A frame of another shape is centred in its tile, not stretched.
        let mixed = [frame(400, 200, 200), frame(100, 200, 100)];
        let sheet = contact_sheet(&mixed, 800).unwrap();
        assert_eq!(sheet.dimensions(), (800, 200));
        assert_eq!(sheet.get_pixel(600, 100).0[0], 100);
        assert_eq!(sheet.get_pixel(410, 100).0[0], 0);
    }

    #[test]
    fn a_region_is_cut_in_the_units_of_the_target() {
        // A 200×100 target captured at twice its size; the right half is white.
        let mut image = frame(400, 200, 0);
        for x in 200..400 {
            for y in 0..200 {
                image.put_pixel(x, y, Rgba([255, 255, 255, 255]));
            }
        }
        let right = crop(&image, (200, 100), [100, 0, 100, 50]).unwrap();
        assert_eq!(right.dimensions(), (200, 100));
        assert!(right.pixels().all(|pixel| pixel.0[0] == 255));
        let straddling = crop(&image, (200, 100), [90, 90, 20, 10]).unwrap();
        assert_eq!(straddling.dimensions(), (40, 20));
        assert_eq!(
            (straddling.get_pixel(0, 0).0[0], straddling.get_pixel(39, 0).0[0]),
            (0, 255)
        );

        for region in [
            [150, 0, 100, 50],
            [0, 0, 0, 10],
            [0, 90, 10, 20],
            [u32::MAX, 0, 2, 2],
        ] {
            let error = crop(&image, (200, 100), region).unwrap_err();
            assert!(
                error.contains("does not lie within the target, which is 200×100"),
                "{error}"
            );
        }
    }

    #[test]
    fn key_names_map_to_input_keys() {
        let key = |keys: &str| input_key(&page::parse_keys(keys).unwrap()).unwrap();
        assert_eq!(key("Enter"), enigo::Key::Return);
        assert_eq!(key("ArrowLeft"), enigo::Key::LeftArrow);
        assert_eq!(key("F11"), enigo::Key::F11);
        assert_eq!(key("space"), enigo::Key::Space);
        assert_eq!(key("w"), enigo::Key::Unicode('w'));
        // With Shift held the key itself stays the unshifted one.
        assert_eq!(key("Shift+W"), enigo::Key::Unicode('w'));
        assert_eq!(
            held_modifiers(page::parse_keys("Control+Shift+S").unwrap().modifiers),
            [enigo::Key::Control, enigo::Key::Shift]
        );
        assert!(held_modifiers(0).is_empty());
    }

    #[test]
    fn buttons_parse() {
        assert_eq!(mouse_button(None), Ok(Button::Left));
        assert_eq!(mouse_button(Some("right")), Ok(Button::Right));
        assert!(
            mouse_button(Some("fourth"))
                .unwrap_err()
                .contains("unknown button `fourth`")
        );
    }
}
