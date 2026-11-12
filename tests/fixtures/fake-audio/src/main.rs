//! Stand-ins for the audio programs voice tests need: a recorder, a transcriber and a player.
//!
//! - `record [--raw] [--exit] <milliseconds>` writes that much of a 440 Hz tone to standard
//!   output the way a recorder does on a pipe: 44.1 kHz stereo 16-bit WAV whose header lengths
//!   are placeholders. With `--raw` it writes 16 kHz mono samples and no header. It then waits
//!   to be stopped, like a recorder with an open microphone; with `--exit` it ends by itself.
//! - `record --fail` reports a busy device on standard error and exits with status 1.
//! - `transcribe <file>` prints what the 16-bit WAV file holds: `<rate> Hz, <channels> channel,
//!   <milliseconds> ms`, over two lines.
//! - `play [--hang] <marker> <file>` writes the same description of `<file>` to `<marker>` and
//!   exits; with `--hang` it then keeps "playing" until it is stopped.

use std::f64::consts::PI;
use std::io::{self, Write};
use std::process::ExitCode;
use std::time::Duration;

fn tone(rate: u32, channels: u16, milliseconds: u64) -> Vec<u8> {
    let frames = u64::from(rate) * milliseconds / 1000;
    let mut out = Vec::new();
    for n in 0..frames {
        let value = (2.0 * PI * 440.0 * n as f64 / f64::from(rate)).sin();
        let sample = (value * 16_000.0) as i16;
        for _ in 0..channels {
            out.extend_from_slice(&sample.to_le_bytes());
        }
    }
    out
}

/// A WAV header as written to a pipe: the two lengths cannot be filled in afterwards.
fn streaming_header(rate: u32, channels: u16) -> Vec<u8> {
    let block = channels * 2;
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes());
    out.extend_from_slice(&channels.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&(rate * u32::from(block)).to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&16u16.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&u32::MAX.to_le_bytes());
    out
}

fn wait_to_be_stopped() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(3600));
    }
}

fn record(args: &[String]) -> io::Result<ExitCode> {
    if args.iter().any(|arg| arg == "--fail") {
        eprintln!("fake-audio: opening the device");
        eprintln!("fake-audio: audio open error: Device or resource busy");
        return Ok(ExitCode::FAILURE);
    }
    let milliseconds: u64 = args
        .iter()
        .find_map(|arg| arg.parse().ok())
        .ok_or_else(|| io::Error::other("record needs a length in milliseconds"))?;
    let mut stream = Vec::new();
    if args.iter().any(|arg| arg == "--raw") {
        stream.extend(tone(16_000, 1, milliseconds));
    } else {
        stream.extend(streaming_header(44_100, 2));
        stream.extend(tone(44_100, 2, milliseconds));
    }
    let mut out = io::stdout().lock();
    out.write_all(&stream)?;
    out.flush()?;
    if args.iter().any(|arg| arg == "--exit") {
        return Ok(ExitCode::SUCCESS);
    }
    wait_to_be_stopped()
}

/// `<rate> Hz, <channels> channel, <milliseconds> ms` for a canonical 16-bit WAV file.
fn describe(file: &str) -> io::Result<String> {
    let bytes = std::fs::read(file)?;
    let field = |at: usize, len: usize| -> io::Result<u32> {
        let raw = bytes
            .get(at..at + len)
            .ok_or_else(|| io::Error::other("the file is too short for a WAV header"))?;
        Ok(raw
            .iter()
            .rev()
            .fold(0, |value, byte| value << 8 | u32::from(*byte)))
    };
    if bytes.get(..4) != Some(b"RIFF") || bytes.get(36..40) != Some(b"data") {
        return Err(io::Error::other("not a canonical WAV file"));
    }
    let (channels, rate, data) = (field(22, 2)?, field(24, 4)?, field(40, 4)?);
    if data as usize != bytes.len() - 44 || field(4, 4)? as usize != bytes.len() - 8 {
        return Err(io::Error::other("the header lengths do not match the file"));
    }
    let frames = u64::from(data) / u64::from(channels * 2);
    Ok(format!(
        "{rate} Hz, {channels} channel, {} ms",
        frames * 1000 / u64::from(rate)
    ))
}

fn run(args: &[String]) -> io::Result<ExitCode> {
    let rest = args.get(1..).unwrap_or_default();
    match args.first().map(String::as_str) {
        Some("record") => record(rest),
        Some("transcribe") => {
            let file = rest
                .last()
                .ok_or_else(|| io::Error::other("transcribe needs a file"))?;
            // Programs such as whisper.cpp print one line per segment.
            println!("{}", describe(file)?.replacen(", ", ",\n  ", 1));
            Ok(ExitCode::SUCCESS)
        }
        Some("play") => {
            let hang = rest.iter().any(|arg| arg == "--hang");
            let paths: Vec<&String> = rest.iter().filter(|arg| *arg != "--hang").collect();
            let [marker, file] = paths.as_slice() else {
                return Err(io::Error::other("play needs a marker and a file"));
            };
            std::fs::write(marker, describe(file)?)?;
            if hang {
                wait_to_be_stopped()
            }
            Ok(ExitCode::SUCCESS)
        }
        _ => Err(io::Error::other("usage: fake-audio record|transcribe|play …")),
    }
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("fake-audio: {e}");
            ExitCode::FAILURE
        }
    }
}
