//! Microphone capture. Two back ends, both ending in a 16 kHz mono 16-bit WAV file in memory:
//! the system's audio API (Windows and macOS) and a recording program that writes to its
//! standard output until it is stopped (everywhere, and the only one on Linux).

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azazel_core::config::VoiceConfig;
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::task::JoinHandle;

#[cfg(any(windows, target_os = "macos"))]
use super::native;
use super::wav::{self, Format};
use super::{first_installed, last_line, program, short_name};

/// A recording never grows past this, whatever `voice.max_secs` allows.
pub const MAX_BYTES: usize = 10 * 1024 * 1024;

/// A stream that still has no readable header after this much is not audio.
const MAX_HEADER_BYTES: usize = 1024 * 1024;

/// How long a recorder gets to write out what it holds after it is told to stop.
const WIND_DOWN: Duration = Duration::from_millis(1500);

/// Recording programs tried in order when `voice.record_command` is empty. Each writes 16 kHz
/// mono audio to its standard output; `pw-record` can only write it there without a header.
const RECORDERS: &[&str] = &[
    "arecord -q -f S16_LE -r 16000 -c 1 -t wav",
    "pw-record --raw --rate 16000 --channels 1 --format s16 -",
    "sox -q -d -t wav -r 16000 -c 1 -b 16 -e signed-integer -",
    "ffmpeg -hide_banner -loglevel error -nostdin -f pulse -i default -ac 1 -ar 16000 -f wav -",
];

/// What the capture side tells the UI while it runs.
#[derive(Default)]
pub(crate) struct Meter {
    /// Peak of the newest audio, as the bits of an `f32` between 0 and 1.
    level: AtomicU32,
    /// The size limit is reached; nothing more is kept.
    full: AtomicBool,
}

impl Meter {
    pub(crate) fn set_level(&self, level: f32) {
        self.level.store(level.to_bits(), Ordering::Relaxed);
    }

    pub(crate) fn set_full(&self) {
        self.full.store(true, Ordering::Relaxed);
    }
}

enum Source {
    Command(Box<CommandCapture>),
    #[cfg(any(windows, target_os = "macos"))]
    Native(native::Capture),
}

/// A recording in progress. Dropping it stops the capture and discards the audio.
pub struct Recording {
    source: Source,
    meter: Arc<Meter>,
    started: Instant,
}

/// The recording program to run: the configured one, else the first installed default. `None`
/// where the system's audio API is used instead.
fn recorder(settings: &VoiceConfig) -> Result<Option<&str>, String> {
    let configured = settings.record_command.trim();
    if !configured.is_empty() {
        return Ok(Some(configured));
    }
    if cfg!(any(windows, target_os = "macos")) {
        return Ok(None);
    }
    first_installed(RECORDERS).map(Some).ok_or_else(|| {
        "no recording program found: install arecord, pw-record, sox or ffmpeg, or set [voice] record_command"
            .to_string()
    })
}

/// What records on this machine, for `azazel doctor`.
pub fn describe(settings: &VoiceConfig) -> Result<String, String> {
    match recorder(settings)? {
        Some(command) => {
            let configured = !settings.record_command.trim().is_empty();
            let (found, _) = program(command).map_err(|why| match configured {
                true => format!("{why} (from [voice] record_command)"),
                false => why,
            })?;
            Ok(format!("the program {}", found.display()))
        }
        #[cfg(any(windows, target_os = "macos"))]
        None => native::input_name().map(|name| format!("the default input device ({name})")),
        #[cfg(not(any(windows, target_os = "macos")))]
        None => Err("no recorder".to_string()),
    }
}

/// Starts recording.
pub async fn start(settings: &VoiceConfig) -> Result<Recording, String> {
    let meter = Arc::new(Meter::default());
    let source = match recorder(settings)? {
        Some(command) => Source::Command(Box::new(CommandCapture::start(command, meter.clone())?)),
        #[cfg(any(windows, target_os = "macos"))]
        None => {
            let meter = meter.clone();
            // Opening the device can take a moment.
            let capture = tokio::task::spawn_blocking(move || native::Capture::start(meter))
                .await
                .map_err(|e| format!("could not start recording: {e}"))??;
            Source::Native(capture)
        }
        #[cfg(not(any(windows, target_os = "macos")))]
        None => return Err("no recorder".to_string()),
    };
    Ok(Recording {
        source,
        meter,
        started: Instant::now(),
    })
}

impl Recording {
    pub fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// Peak of the newest audio, between 0 and 1.
    pub fn level(&self) -> f32 {
        f32::from_bits(self.meter.level.load(Ordering::Relaxed))
    }

    /// Whether the recording has come to an end by itself: the size limit is reached, the
    /// recording program exited, or the device failed.
    pub fn ended(&mut self) -> bool {
        if self.meter.full.load(Ordering::Relaxed) {
            return true;
        }
        match &mut self.source {
            Source::Command(capture) => !matches!(capture.child.try_wait(), Ok(None)),
            #[cfg(any(windows, target_os = "macos"))]
            Source::Native(capture) => capture.failed(),
        }
    }

    /// Stops the capture and returns the recording as a 16 kHz mono 16-bit WAV file.
    pub async fn finish(self) -> Result<Vec<u8>, String> {
        let (mono, rate) = match self.source {
            Source::Command(capture) => capture.finish().await?,
            #[cfg(any(windows, target_os = "macos"))]
            Source::Native(capture) => capture.finish()?,
        };
        if mono.is_empty() {
            return Err("the recording is empty".to_string());
        }
        tokio::task::spawn_blocking(move || wav::speech_wav(&mono, rate, MAX_BYTES))
            .await
            .map_err(|e| format!("could not convert the recording: {e}"))
    }
}

/// A recording program and what it has written so far.
struct CommandCapture {
    name: String,
    child: Child,
    audio: Arc<Mutex<Vec<u8>>>,
    reader: JoinHandle<()>,
    errors: JoinHandle<Vec<u8>>,
}

impl CommandCapture {
    fn start(command: &str, meter: Arc<Meter>) -> Result<CommandCapture, String> {
        let (program, args) = program(command)?;
        let name = short_name(&program);
        let mut child = Command::new(&program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(|e| format!("could not start {name}: {e}"))?;
        let audio = Arc::new(Mutex::new(Vec::new()));
        let reader = match child.stdout.take() {
            Some(pipe) => tokio::spawn(read_audio(pipe, audio.clone(), meter)),
            None => tokio::spawn(async {}),
        };
        let errors = match child.stderr.take() {
            Some(mut pipe) => tokio::spawn(async move {
                let mut text = Vec::new();
                let _ = pipe.read_to_end(&mut text).await;
                text
            }),
            None => tokio::spawn(async { Vec::new() }),
        };
        Ok(CommandCapture {
            name,
            child,
            audio,
            reader,
            errors,
        })
    }

    /// Stops the program and returns its audio as mono samples with their rate.
    async fn finish(mut self) -> Result<(Vec<f32>, u32), String> {
        stop(&mut self.child).await;
        // The pipe closes with the program, unless something it started still holds it.
        if tokio::time::timeout(WIND_DOWN, &mut self.reader).await.is_err() {
            self.reader.abort();
        }
        let audio = std::mem::take(&mut *self.audio.lock().unwrap_or_else(PoisonError::into_inner));
        let name = self.name;
        if audio.is_empty() {
            let said = match tokio::time::timeout(WIND_DOWN, self.errors).await {
                Ok(Ok(text)) => last_line(&text),
                _ => String::new(),
            };
            return Err(match said.is_empty() {
                true => format!("{name} recorded nothing"),
                false => format!("{name} recorded nothing: {said}"),
            });
        }
        tokio::task::spawn_blocking(move || {
            let pcm = match audio.starts_with(b"RIFF") {
                true => wav::decode(&audio).map_err(|e| format!("{name}: {e}"))?,
                false => wav::decode_as(&Format::raw_speech(), &audio),
            };
            Ok((pcm.mono(), pcm.rate))
        })
        .await
        .map_err(|e| format!("could not read the recording: {e}"))?
    }
}

/// Ends a recording program. Asked to stop, a recorder writes out the audio it still holds; one
/// that is killed loses it, and with it the end of the last word.
async fn stop(child: &mut Child) {
    #[cfg(unix)]
    if let Some(pid) = child.id() {
        let asked = Command::new("kill")
            .arg("-INT")
            .arg(pid.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if asked.is_ok_and(|status| status.success())
            && tokio::time::timeout(WIND_DOWN, child.wait()).await.is_ok()
        {
            return;
        }
    }
    let _ = child.kill().await;
}

/// Collects what the recorder writes, feeding the level meter and watching the size limit.
async fn read_audio(mut pipe: ChildStdout, audio: Arc<Mutex<Vec<u8>>>, meter: Arc<Meter>) {
    let mut chunk = vec![0u8; 16 * 1024];
    let mut format: Option<Format> = None;
    // How much of the stream the meter has seen.
    let mut metered = 0;
    loop {
        let read = match pipe.read(&mut chunk).await {
            Ok(0) | Err(_) => break,
            Ok(read) => read,
        };
        let mut audio = audio.lock().unwrap_or_else(PoisonError::into_inner);
        audio.extend_from_slice(&chunk[..read]);
        if format.is_none() {
            format = match audio.starts_with(b"RIFF") {
                // Not readable until the whole header has arrived.
                true => wav::format(&audio).ok(),
                false => (audio.len() >= 4).then(Format::raw_speech),
            };
            metered = format.map_or(0, |format| format.data_at);
        }
        let Some(format) = format else {
            if audio.len() > MAX_HEADER_BYTES {
                break;
            }
            continue;
        };
        let frame = format.frame_bytes();
        let whole = (audio.len() - metered) / frame * frame;
        if whole > 0 {
            meter.set_level(format.peak(&audio[metered..metered + whole]));
            metered += whole;
        }
        // What the audio so far will weigh once it is 16 kHz mono 16-bit.
        let frames = ((metered - format.data_at) / frame) as u64;
        let converted = frames * u64::from(wav::SPEECH_RATE) / u64::from(format.rate) * 2;
        if converted >= MAX_BYTES as u64 {
            meter.set_full();
            break;
        }
    }
}
