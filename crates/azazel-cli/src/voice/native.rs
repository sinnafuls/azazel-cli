//! Recording and playback through the system's audio API (WASAPI on Windows, Core Audio on
//! macOS). Linux never gets here: linking its audio library would stop the whole binary from
//! starting on machines without it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{ErrorKind, FromSample, I24, Sample, SampleFormat, SizedSample, StreamConfig};
use tokio_util::sync::CancellationToken;

use super::record::{MAX_BYTES, Meter};
use super::wav::{self, Pcm};

/// How long the device gets to start delivering audio.
const START_LIMIT: Duration = Duration::from_secs(5);

/// What the audio thread shares with the rest of the program.
struct Shared {
    /// Mono samples at the device's rate.
    samples: Mutex<Vec<f32>>,
    /// Samples kept at most: what [`MAX_BYTES`] holds once converted.
    limit: usize,
    meter: Arc<Meter>,
    /// Why the device stopped delivering, when it did.
    failed: Mutex<Option<String>>,
}

impl Shared {
    fn fail(&self, error: &cpal::Error) {
        // These are reported while the stream carries on.
        if matches!(
            error.kind(),
            ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied
        ) {
            return;
        }
        *self.failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
    }
}

/// A microphone capture running on its own thread, which owns the stream.
pub(super) struct Capture {
    shared: Arc<Shared>,
    rate: u32,
    /// Dropping this ends the thread.
    stop: mpsc::Sender<()>,
    thread: JoinHandle<()>,
}

pub(super) fn input_name() -> Result<String, String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| "no microphone found".to_string())?;
    Ok(device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "unnamed".to_string()))
}

pub(super) fn output_name() -> Result<String, String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no audio output device found".to_string())?;
    Ok(device
        .description()
        .map(|description| description.name().to_string())
        .unwrap_or_else(|_| "unnamed".to_string()))
}

fn input<T>(
    device: &cpal::Device,
    config: StreamConfig,
    shared: Arc<Shared>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = usize::from(config.channels.max(1));
    let on_error = {
        let shared = shared.clone();
        move |error: cpal::Error| shared.fail(&error)
    };
    device.build_input_stream(
        config,
        move |data: &[T], _: &cpal::InputCallbackInfo| {
            let mut samples = shared.samples.lock().unwrap_or_else(PoisonError::into_inner);
            let mut peak = 0.0f32;
            for frame in data.chunks(channels) {
                let mono = frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / frame.len() as f32;
                peak = peak.max(mono.abs());
                if samples.len() < shared.limit {
                    samples.push(mono);
                }
            }
            shared.meter.set_level(peak.min(1.0));
            if samples.len() >= shared.limit {
                shared.meter.set_full();
            }
        },
        on_error,
        None,
    )
}

/// Opens the default microphone and starts its stream.
fn open(meter: Arc<Meter>) -> Result<(cpal::Stream, Arc<Shared>, u32), String> {
    let device = cpal::default_host()
        .default_input_device()
        .ok_or_else(|| "no microphone found".to_string())?;
    let supported = device
        .default_input_config()
        .map_err(|e| format!("the microphone cannot be used: {e}"))?;
    let rate = supported.sample_rate();
    let shared = Arc::new(Shared {
        samples: Mutex::new(Vec::new()),
        limit: (MAX_BYTES / 2) * rate as usize / wav::SPEECH_RATE as usize,
        meter,
        failed: Mutex::new(None),
    });
    let config = supported.config();
    let stream = match supported.sample_format() {
        SampleFormat::I8 => input::<i8>(&device, config, shared.clone()),
        SampleFormat::I16 => input::<i16>(&device, config, shared.clone()),
        SampleFormat::I24 => input::<I24>(&device, config, shared.clone()),
        SampleFormat::I32 => input::<i32>(&device, config, shared.clone()),
        SampleFormat::U8 => input::<u8>(&device, config, shared.clone()),
        SampleFormat::U16 => input::<u16>(&device, config, shared.clone()),
        SampleFormat::U32 => input::<u32>(&device, config, shared.clone()),
        SampleFormat::F32 => input::<f32>(&device, config, shared.clone()),
        SampleFormat::F64 => input::<f64>(&device, config, shared.clone()),
        other => {
            return Err(format!(
                "the microphone's sample format ({other}) is not supported"
            ));
        }
    }
    .map_err(|e| format!("the microphone could not be opened: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("the microphone could not be started: {e}"))?;
    Ok((stream, shared, rate))
}

impl Capture {
    pub(super) fn start(meter: Arc<Meter>) -> Result<Capture, String> {
        let (stop, stopped) = mpsc::channel::<()>();
        let (ready, opened) = mpsc::channel();
        let thread = std::thread::Builder::new()
            .name("azazel-microphone".to_string())
            .spawn(move || {
                let stream = match open(meter) {
                    Ok((stream, shared, rate)) => {
                        let _ = ready.send(Ok((shared, rate)));
                        stream
                    }
                    Err(why) => {
                        let _ = ready.send(Err(why));
                        return;
                    }
                };
                // Until the capture is finished or dropped.
                let _ = stopped.recv();
                drop(stream);
            })
            .map_err(|e| format!("could not start recording: {e}"))?;
        let (shared, rate) = opened
            .recv_timeout(START_LIMIT)
            .map_err(|_| "the microphone did not start".to_string())??;
        Ok(Capture {
            shared,
            rate,
            stop,
            thread,
        })
    }

    pub(super) fn failed(&self) -> bool {
        self.shared
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }

    /// Stops the capture and returns its mono samples with their rate.
    pub(super) fn finish(self) -> Result<(Vec<f32>, u32), String> {
        drop(self.stop);
        let _ = self.thread.join();
        let samples =
            std::mem::take(&mut *self.shared.samples.lock().unwrap_or_else(PoisonError::into_inner));
        let failed = self
            .shared
            .failed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        match failed {
            // What was said before the device went away is still worth transcribing.
            Some(why) if samples.is_empty() => Err(format!("the microphone stopped: {why}")),
            _ if samples.is_empty() => Err("the microphone delivered no audio".to_string()),
            _ => Ok((samples, self.rate)),
        }
    }
}

fn output<T>(
    device: &cpal::Device,
    config: StreamConfig,
    audio: Arc<Vec<f32>>,
    position: Arc<AtomicUsize>,
    failed: Arc<Mutex<Option<String>>>,
) -> Result<cpal::Stream, cpal::Error>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = usize::from(config.channels.max(1));
    device.build_output_stream(
        config,
        move |out: &mut [T], _: &cpal::OutputCallbackInfo| {
            for frame in out.chunks_mut(channels) {
                let at = position.fetch_add(1, Ordering::Relaxed);
                let value = T::from_sample(audio.get(at).copied().unwrap_or(0.0));
                frame.fill(value);
            }
        },
        move |error: cpal::Error| {
            if !matches!(
                error.kind(),
                ErrorKind::DeviceChanged | ErrorKind::Xrun | ErrorKind::RealtimeDenied
            ) {
                *failed.lock().unwrap_or_else(PoisonError::into_inner) = Some(error.to_string());
            }
        },
        None,
    )
}

/// Plays `pcm` on the default output device and returns when it has been heard or `cancel`
/// fires. Blocks the calling thread.
pub(super) fn play(pcm: &Pcm, cancel: &CancellationToken) -> Result<(), String> {
    let device = cpal::default_host()
        .default_output_device()
        .ok_or_else(|| "no audio output device found".to_string())?;
    let supported = device
        .default_output_config()
        .map_err(|e| format!("the audio output cannot be used: {e}"))?;
    // Speech is one voice: every channel of the device gets the same signal.
    let audio = Arc::new(wav::resample(&pcm.mono(), pcm.rate, supported.sample_rate()));
    let total = audio.len();
    let position = Arc::new(AtomicUsize::new(0));
    let failed = Arc::new(Mutex::new(None));
    let config = supported.config();
    let (a, p, f) = (audio, position.clone(), failed.clone());
    let stream = match supported.sample_format() {
        SampleFormat::I8 => output::<i8>(&device, config, a, p, f),
        SampleFormat::I16 => output::<i16>(&device, config, a, p, f),
        SampleFormat::I24 => output::<I24>(&device, config, a, p, f),
        SampleFormat::I32 => output::<i32>(&device, config, a, p, f),
        SampleFormat::U8 => output::<u8>(&device, config, a, p, f),
        SampleFormat::U16 => output::<u16>(&device, config, a, p, f),
        SampleFormat::U32 => output::<u32>(&device, config, a, p, f),
        SampleFormat::F32 => output::<f32>(&device, config, a, p, f),
        SampleFormat::F64 => output::<f64>(&device, config, a, p, f),
        other => {
            return Err(format!(
                "the audio output's sample format ({other}) is not supported"
            ));
        }
    }
    .map_err(|e| format!("the audio output could not be opened: {e}"))?;
    stream
        .play()
        .map_err(|e| format!("the audio output could not be started: {e}"))?;
    while position.load(Ordering::Relaxed) < total {
        if cancel.is_cancelled() {
            return Ok(());
        }
        if let Some(why) = failed.lock().unwrap_or_else(PoisonError::into_inner).take() {
            return Err(format!("playback stopped: {why}"));
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    // The device still holds the last buffer it was handed.
    std::thread::sleep(Duration::from_millis(200));
    Ok(())
}
