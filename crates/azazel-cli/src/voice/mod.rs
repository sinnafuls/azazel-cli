//! Voice input and spoken replies.
//!
//! [`record`] captures the microphone as a 16 kHz mono WAV file in memory, [`stt`] turns it into
//! text and [`speak`] reads a reply aloud. Speech services are the OpenAI-style `audio` endpoints
//! of a configured provider; a local program can stand in for each step.

#[cfg(any(windows, target_os = "macos"))]
mod native;
pub mod record;
pub mod speak;
pub mod stt;
pub mod wav;

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use azazel_ai::providers::http::http_client;
use azazel_ai::providers::presets;
use azazel_ai::{Auth, AuthStyle, Credentials, ProviderConfig};
use azazel_core::Config;
use azazel_core::commands::split_args;
use azazel_core::config::VoiceConfig;
use azazel_tui::Editor;

use crate::doctor::{Finding, Severity};
use crate::voice::record::Recording;

/// How long a speech service or a transcription program may take.
pub const LIMIT: Duration = Duration::from_secs(60);

/// The voice asked for when `[voice] voice` is empty.
pub const DEFAULT_VOICE: &str = "alloy";

/// One client for every speech request of the process.
pub(crate) static CLIENT: LazyLock<reqwest::Client> = LazyLock::new(http_client);

/// A provider's audio API with the credential and the model to use.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub provider: ProviderConfig,
    pub auth: Auth,
    pub model: String,
}

/// Ends `recording` and turns it into text.
pub async fn dictate(
    recording: Recording,
    config: &Config,
    credentials: &Credentials,
) -> Result<String, String> {
    let audio = recording.finish().await?;
    stt::transcribe(config, credentials, audio).await
}

/// Puts dictated text at the cursor, set off from a word it would otherwise run into.
pub fn insert(editor: &mut Editor, text: &str) {
    let before = &editor.text()[..editor.cursor()];
    match before.chars().next_back().is_some_and(|c| !c.is_whitespace()) {
        true => editor.insert_str(&format!(" {text}")),
        false => editor.insert_str(text),
    }
}

/// Splits a `provider/model` setting.
fn spec<'a>(setting: &str, value: &'a str) -> Result<(&'a str, &'a str), String> {
    value
        .trim()
        .split_once('/')
        .filter(|(provider, model)| !provider.is_empty() && !model.is_empty())
        .ok_or_else(|| format!("[voice] {setting} must be \"provider/model\", not \"{value}\""))
}

fn provider(config: &Config, setting: &str, id: &str) -> Result<ProviderConfig, String> {
    presets::resolve(&config.providers)?
        .into_iter()
        .find(|provider| provider.id == id)
        .ok_or_else(|| format!("[voice] {setting}: there is no provider named `{id}`"))
}

/// Resolves the `[voice] stt` or `tts` setting (`setting` names which) to an endpoint.
pub async fn endpoint(
    config: &Config,
    credentials: &Credentials,
    setting: &str,
    value: &str,
) -> Result<Endpoint, String> {
    let (id, model) = spec(setting, value)?;
    let provider = provider(config, setting, id)?;
    let auth = credentials
        .resolve(&provider)
        .await
        .map_err(|e| format!("[voice] {setting}: {e}"))?;
    Ok(Endpoint {
        provider,
        auth,
        model: model.to_string(),
    })
}

impl Endpoint {
    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.provider.base_url.trim_end_matches('/'))
    }

    /// A request to `path` under the provider's address, with its credential and headers.
    pub(crate) fn post(&self, client: &reqwest::Client, path: &str) -> reqwest::RequestBuilder {
        let mut request = client.post(self.url(path));
        request = match &self.auth {
            Auth::None => request,
            Auth::OAuth { access, .. } => request.bearer_auth(access),
            Auth::ApiKey(key) if self.provider.auth_style == AuthStyle::ApiKey => {
                request.header("api-key", key)
            }
            Auth::ApiKey(key) => request.bearer_auth(key),
        };
        for (name, value) in &self.provider.headers {
            request = request.header(name, value);
        }
        for (name, value) in self.auth.login_headers(&self.provider) {
            request = request.header(name, value);
        }
        request
    }

    /// Takes the credential and the address out of text that came back from the service.
    fn scrub(&self, text: &str) -> String {
        let mut clean = text.replace(&self.provider.base_url, "[address]");
        if let Auth::ApiKey(secret) | Auth::OAuth { access: secret, .. } = &self.auth
            && !secret.is_empty()
        {
            clean = clean.replace(secret, "[key]");
        }
        clean
    }

    /// Sends `request` within [`LIMIT`] and returns the body of a successful reply. `what` names
    /// the step in error messages, which never carry the address or the credential.
    pub(crate) async fn send(&self, what: &str, request: reqwest::RequestBuilder) -> Result<Vec<u8>, String> {
        let id = &self.provider.id;
        let exchange = async {
            let response = request.send().await?;
            let status = response.status();
            Ok::<_, reqwest::Error>((status, response.bytes().await?))
        };
        let (status, body) = match tokio::time::timeout(LIMIT, exchange).await {
            Err(_) => {
                return Err(format!(
                    "{what} failed: {id} did not answer within {} s",
                    LIMIT.as_secs()
                ));
            }
            Ok(Err(e)) if e.is_connect() => return Err(format!("{what} failed: could not connect to {id}")),
            Ok(Err(e)) if e.is_timeout() => return Err(format!("{what} failed: {id} stopped answering")),
            Ok(Err(e)) => return Err(format!("{what} failed: {}", e.without_url())),
            Ok(Ok(reply)) => reply,
        };
        if status.is_success() {
            return Ok(body.to_vec());
        }
        let said = service_message(&body);
        Err(match said.is_empty() {
            true => format!("{what} failed: {id} answered {status}"),
            false => format!("{what} failed: {id} answered {status}: {}", self.scrub(&said)),
        })
    }
}

/// What an error reply says, in one short line.
fn service_message(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(body);
    let said = match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(json) => json["error"]["message"]
            .as_str()
            .or_else(|| json["error"].as_str())
            .or_else(|| json["message"].as_str())
            .unwrap_or_default()
            .to_string(),
        Err(_) => text.lines().next().unwrap_or_default().to_string(),
    };
    azazel_tui::text::truncate(said.trim(), 200, "…")
}

/// Splits a configured command line into its program, looked up on `PATH`, and its arguments.
/// It is run directly, not through a shell: audio passes through unchanged and stopping the
/// program stops the right process.
pub(crate) fn program(command: &str) -> Result<(PathBuf, Vec<String>), String> {
    let mut words = split_args(command).into_iter();
    let name = words.next().ok_or_else(|| "the command is empty".to_string())?;
    let program = which::which(&name).map_err(|_| format!("`{name}` was not found"))?;
    Ok((program, words.collect()))
}

/// The first of `candidates` whose program is installed.
pub(crate) fn first_installed(candidates: &[&'static str]) -> Option<&'static str> {
    candidates
        .iter()
        .copied()
        .find(|candidate| program(candidate).is_ok())
}

/// Puts `file` where the arguments say `{file}`, or at the end when they do not.
pub(crate) fn with_file(args: Vec<String>, file: &Path) -> Vec<String> {
    let path = file.to_string_lossy();
    if !args.iter().any(|arg| arg.contains("{file}")) {
        return args.into_iter().chain([path.into_owned()]).collect();
    }
    args.into_iter().map(|arg| arg.replace("{file}", &path)).collect()
}

/// A program's name as messages show it.
pub(crate) fn short_name(program: &Path) -> String {
    program
        .file_stem()
        .unwrap_or(program.as_os_str())
        .to_string_lossy()
        .into_owned()
}

/// The last line a program wrote to its error stream, for a message.
pub(crate) fn last_line(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|line| !line.trim().is_empty());
    azazel_tui::text::truncate(line.unwrap_or_default().trim(), 200, "…")
}

/// What `azazel doctor` says about voice: the recorder, both speech settings and what is missing.
pub fn findings(config: &Config, credentials: &Credentials) -> Vec<Finding> {
    let voice = &config.voice;
    let mut out = Vec::new();
    let mut add = |severity, text: String| out.push(Finding { severity, text });
    let dictation = stt::configured(voice);
    match record::describe(voice) {
        Ok(recorder) => add(Severity::Ok, format!("voice recorder: {recorder}")),
        // Only a problem for someone who set dictation up.
        Err(missing) if dictation => add(Severity::Warn, format!("voice recorder: {missing}")),
        Err(missing) => add(Severity::Ok, format!("voice recorder: none ({missing})")),
    }
    let (severity, text) = transcription(config, credentials, voice);
    add(severity, format!("voice transcription: {text}"));
    if voice.tts.trim().is_empty() {
        add(
            Severity::Ok,
            "spoken replies: not set up; set [voice] tts = \"provider/model\" to use /speak".to_string(),
        );
    } else {
        let (severity, text) = service(config, credentials, "tts", &voice.tts);
        add(severity, format!("spoken replies: {text}"));
        match speak::describe(voice) {
            Ok(player) => add(Severity::Ok, format!("spoken replies: played by {player}")),
            Err(missing) => add(Severity::Warn, format!("spoken replies: {missing}")),
        }
    }
    out
}

fn transcription(config: &Config, credentials: &Credentials, voice: &VoiceConfig) -> (Severity, String) {
    let command = voice.stt_command.trim();
    if !command.is_empty() {
        return match program(command) {
            Ok((program, _)) => (Severity::Ok, format!("the program {}", program.display())),
            Err(why) => (Severity::Warn, format!("{why} (from [voice] stt_command)")),
        };
    }
    if voice.stt.trim().is_empty() {
        return (
            Severity::Ok,
            "not set up; set [voice] stt = \"provider/model\" or stt_command to dictate".to_string(),
        );
    }
    service(config, credentials, "stt", &voice.stt)
}

/// Whether a `provider/model` setting names a provider that can be reached. Nothing is sent.
fn service(config: &Config, credentials: &Credentials, setting: &str, value: &str) -> (Severity, String) {
    let found = spec(setting, value).and_then(|(id, model)| Ok((provider(config, setting, id)?, model)));
    let (provider, model) = match found {
        Ok(found) => found,
        Err(why) => return (Severity::Warn, why),
    };
    let id = &provider.id;
    match credentials.source(&provider) {
        Some(source) => (Severity::Ok, format!("{model} at {id}, credential from {source}")),
        None if !provider.requires_key => (Severity::Ok, format!("{model} at {id}, no key needed")),
        None => (
            Severity::Warn,
            format!("{model} at {id}, but {id} has no credential"),
        ),
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::types::WireApi;

    use super::*;

    #[test]
    fn settings_must_name_a_provider_and_a_model() {
        assert_eq!(spec("stt", "openai/whisper-1"), Ok(("openai", "whisper-1")));
        assert_eq!(
            spec("stt", " groq/whisper-large-v3-turbo "),
            Ok(("groq", "whisper-large-v3-turbo"))
        );
        // Only the first slash separates: model ids have their own.
        assert_eq!(
            spec("tts", "local/hexgrad/kokoro"),
            Ok(("local", "hexgrad/kokoro"))
        );
        for bad in ["whisper-1", "/whisper-1", "openai/", ""] {
            let error = spec("stt", bad).unwrap_err();
            assert!(error.starts_with("[voice] stt must be"), "{error}");
        }
    }

    #[test]
    fn error_text_keeps_neither_the_key_nor_the_address() {
        let endpoint = Endpoint {
            provider: ProviderConfig::new("openai", WireApi::OpenAiResponses, "https://api.example.test/v1"),
            auth: Auth::ApiKey("sk-secret".to_string()),
            model: "whisper-1".to_string(),
        };
        assert_eq!(
            endpoint.scrub("bad key sk-secret for https://api.example.test/v1/audio/speech"),
            "bad key [key] for [address]/audio/speech"
        );
        assert_eq!(
            service_message(br#"{"error":{"message":" model not found "}}"#),
            "model not found"
        );
        assert_eq!(service_message(br#"{"error":"no such voice"}"#), "no such voice");
        assert_eq!(service_message(b"<html>\nBad Gateway"), "<html>");
        assert_eq!(service_message(b""), "");
    }
}
