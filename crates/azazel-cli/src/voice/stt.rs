//! Speech to text: an OpenAI-style `audio/transcriptions` endpoint, or a local program.

use std::process::Stdio;

use azazel_ai::Credentials;
use azazel_core::Config;
use azazel_core::config::VoiceConfig;
use reqwest::multipart::{Form, Part};
use tokio::process::Command;

use super::{CLIENT, Endpoint, LIMIT, endpoint, last_line, program, short_name, with_file};

/// What to do when neither way of transcribing is set.
pub const NOT_SET: &str = "voice input is not set up: set [voice] stt = \"provider/model\" \
                           (for example \"openai/whisper-1\") or [voice] stt_command";

/// Whether the settings name a way to transcribe.
pub fn configured(settings: &VoiceConfig) -> bool {
    !settings.stt_command.trim().is_empty() || !settings.stt.trim().is_empty()
}

/// Transcribes a WAV recording the way the settings say. A program wins over a provider: it is
/// the more specific choice.
pub async fn transcribe(
    config: &Config,
    credentials: &Credentials,
    recording: Vec<u8>,
) -> Result<String, String> {
    let settings = &config.voice;
    if !settings.stt_command.trim().is_empty() {
        return by_command(&settings.stt_command, &recording).await;
    }
    if settings.stt.trim().is_empty() {
        return Err(NOT_SET.to_string());
    }
    let endpoint = endpoint(config, credentials, "stt", &settings.stt).await?;
    by_http(&CLIENT, &endpoint, &settings.language, recording).await
}

/// Lines and runs of blanks become single spaces: the text goes into one input.
fn tidy(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Posts the recording to `{base_url}/audio/transcriptions` and returns the text of the reply.
pub async fn by_http(
    client: &reqwest::Client,
    endpoint: &Endpoint,
    language: &str,
    recording: Vec<u8>,
) -> Result<String, String> {
    const WHAT: &str = "transcription";
    let file = Part::bytes(recording)
        .file_name("recording.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("{WHAT} failed: {e}"))?;
    let mut form = Form::new()
        .part("file", file)
        .text("model", endpoint.model.clone());
    let language = language.trim();
    if !language.is_empty() {
        form = form.text("language", language.to_string());
    }
    let request = endpoint.post(client, "audio/transcriptions").multipart(form);
    let body = endpoint.send(WHAT, request).await?;
    let reply: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|_| format!("{WHAT} failed: {} did not answer with JSON", endpoint.provider.id))?;
    reply["text"].as_str().map(tidy).ok_or_else(|| {
        format!(
            "{WHAT} failed: the answer of {} has no text",
            endpoint.provider.id
        )
    })
}

/// Runs `command` on the recording, saved as a temporary WAV file whose path replaces `{file}`,
/// and returns what the program prints.
pub async fn by_command(command: &str, recording: &[u8]) -> Result<String, String> {
    let (program, args) = program(command).map_err(|why| format!("{why} (from [voice] stt_command)"))?;
    let name = short_name(&program);
    let dir = tempfile::tempdir().map_err(|e| format!("could not save the recording: {e}"))?;
    let file = dir.path().join("recording.wav");
    tokio::fs::write(&file, recording)
        .await
        .map_err(|e| format!("could not save the recording: {e}"))?;
    let run = Command::new(&program)
        .args(with_file(args, &file))
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output();
    let output = tokio::time::timeout(LIMIT, run)
        .await
        .map_err(|_| format!("{name} did not finish within {} s", LIMIT.as_secs()))?
        .map_err(|e| format!("could not run {name}: {e}"))?;
    if !output.status.success() {
        let said = last_line(&output.stderr);
        return Err(match said.is_empty() {
            true => format!("{name} failed ({})", output.status),
            false => format!("{name} failed ({}): {said}", output.status),
        });
    }
    Ok(tidy(&String::from_utf8_lossy(&output.stdout)))
}
