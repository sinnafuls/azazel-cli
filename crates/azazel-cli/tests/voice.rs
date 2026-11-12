#![allow(clippy::unwrap_used)]
#![cfg(feature = "voice")]

//! Voice: the transcription and speech requests, recorder and player programs, dictation into
//! the editor, and what `doctor` reports.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::time::Duration;

use assert_cmd::Command;
use azazel_ai::types::WireApi;
use azazel_ai::{Auth, Credentials, ProviderConfig, ProviderSettings};
use azazel_cli::doctor::Severity;
use azazel_cli::voice::{self, Endpoint, record, speak, stt, wav};
use azazel_core::Config;
use azazel_tui::Editor;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The `fake-audio` fixture, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = std::process::Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-audio", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-audio fixture"
    );
    let binary = profile_dir.join(format!("fake-audio{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

/// A command line that runs the fixture with `args`.
fn fake(args: &str) -> String {
    format!("\"{}\" {args}", FIXTURE.display())
}

fn credentials() -> Credentials {
    Credentials::new(PathBuf::from("no-such-auth.json"))
}

fn endpoint(server: &MockServer, model: &str) -> Endpoint {
    Endpoint {
        provider: ProviderConfig::new("openai", WireApi::OpenAiResponses, format!("{}/v1", server.uri())),
        auth: Auth::ApiKey("sk-test-secret".to_string()),
        model: model.to_string(),
    }
}

/// A configuration whose provider `speech` is `server`, with a key of its own.
fn config_for(server: &MockServer) -> Config {
    let mut config = Config::default();
    config.providers.insert(
        "speech".to_string(),
        ProviderSettings {
            api: Some(WireApi::OpenAiChat),
            base_url: Some(format!("{}/v1", server.uri())),
            api_key: Some("sk-test-secret".to_string()),
            ..ProviderSettings::default()
        },
    );
    config
}

/// A quarter of a second of tone at 24 kHz, the way a speech service streams it: the lengths
/// in the header are placeholders.
fn streamed_speech() -> Vec<u8> {
    let samples: Vec<i16> = (0..6_000).map(|n| ((n % 100) as i16 - 50) * 200).collect();
    let mut audio = wav::encode(&samples, 24_000, 1);
    audio[4..8].copy_from_slice(&u32::MAX.to_le_bytes());
    audio[40..44].copy_from_slice(&u32::MAX.to_le_bytes());
    audio
}

/// The parts of a multipart body as `(headers, content)`.
fn parts(content_type: &str, body: &[u8]) -> Vec<(String, Vec<u8>)> {
    let boundary = content_type.split("boundary=").nth(1).unwrap().trim_matches('"');
    let delimiter = format!("--{boundary}").into_bytes();
    let mut found = Vec::new();
    let mut rest = body;
    while let Some(at) = rest.windows(delimiter.len()).position(|w| w == delimiter) {
        rest = &rest[at + delimiter.len()..];
        if rest.starts_with(b"--") {
            break;
        }
        let end = rest
            .windows(delimiter.len())
            .position(|w| w == delimiter)
            .unwrap();
        // Each part is framed by line breaks; a blank line ends its headers.
        let part = &rest[2..end - 2];
        let split = part.windows(4).position(|w| w == b"\r\n\r\n").unwrap();
        found.push((
            String::from_utf8_lossy(&part[..split]).to_lowercase(),
            part[split + 4..].to_vec(),
        ));
    }
    found
}

#[tokio::test]
async fn transcription_posts_the_recording_as_multipart() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("authorization", "Bearer sk-test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": " fix the\nfailing test. " })))
        .expect(2)
        .mount(&server)
        .await;
    let client = reqwest::Client::new();
    let recording = wav::encode(&[0, 1000, -1000, 0], 16_000, 1);

    let text = stt::by_http(
        &client,
        &endpoint(&server, "whisper-1"),
        " en ",
        recording.clone(),
    )
    .await
    .unwrap();
    assert_eq!(text, "fix the failing test.");
    stt::by_http(&client, &endpoint(&server, "whisper-1"), "", recording.clone())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    let content_type = requests[0].headers["content-type"].to_str().unwrap();
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "{content_type}"
    );
    let sent = parts(content_type, &requests[0].body);
    assert_eq!(sent.len(), 3);
    let (file, model, language) = (&sent[0], &sent[1], &sent[2]);
    assert!(file.0.contains("name=\"file\""), "{}", file.0);
    assert!(file.0.contains("filename=\"recording.wav\""), "{}", file.0);
    assert!(file.0.contains("content-type: audio/wav"), "{}", file.0);
    assert_eq!(file.1, recording);
    assert!(model.0.contains("name=\"model\""), "{}", model.0);
    assert_eq!(model.1, b"whisper-1");
    assert!(language.0.contains("name=\"language\""), "{}", language.0);
    assert_eq!(language.1, b"en");

    // No language set: the service detects it.
    let content_type = requests[1].headers["content-type"].to_str().unwrap();
    let sent = parts(content_type, &requests[1].body);
    assert_eq!(sent.len(), 2);
    assert!(sent.iter().all(|(headers, _)| !headers.contains("language")));
}

#[tokio::test]
async fn the_settings_choose_the_provider_model_and_language() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .and(header("authorization", "Bearer sk-test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "text": "dzień dobry" })))
        .mount(&server)
        .await;
    let mut config = config_for(&server);
    config.voice.stt = "speech/whisper-large-v3".to_string();
    config.voice.language = "pl".to_string();
    let recording = wav::encode(&[0; 160], 16_000, 1);

    let text = stt::transcribe(&config, &credentials(), recording).await.unwrap();
    assert_eq!(text, "dzień dobry");
    let requests = server.received_requests().await.unwrap();
    let sent = parts(
        requests[0].headers["content-type"].to_str().unwrap(),
        &requests[0].body,
    );
    assert_eq!(sent[1].1, b"whisper-large-v3");
    assert_eq!(sent[2].1, b"pl");

    // A setting that names no provider, or nothing at all, says what to write.
    config.voice.stt = "nowhere/whisper-1".to_string();
    let error = stt::transcribe(&config, &credentials(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(error, "[voice] stt: there is no provider named `nowhere`");
    config.voice.stt.clear();
    let error = stt::transcribe(&config, &credentials(), Vec::new())
        .await
        .unwrap_err();
    assert!(
        error.starts_with("voice input is not set up: set [voice] stt"),
        "{error}"
    );
}

#[tokio::test]
async fn failed_requests_keep_neither_the_address_nor_the_key() {
    let server = MockServer::start().await;
    let address = server.uri();
    Mock::given(method("POST"))
        .and(path("/v1/audio/transcriptions"))
        .respond_with(ResponseTemplate::new(401).set_body_json(json!({
            "error": { "message": format!("Incorrect API key provided: sk-test-secret, sent to {address}/v1/audio/transcriptions") }
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .respond_with(ResponseTemplate::new(503).set_body_string("upstream is down\nmore detail"))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();

    let error = stt::by_http(&client, &endpoint(&server, "whisper-1"), "", vec![0; 64])
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "transcription failed: openai answered 401 Unauthorized: Incorrect API key provided: [key], \
         sent to [address]/audio/transcriptions"
    );
    let error = speak::synthesize(&client, &endpoint(&server, "tts-1"), "", "hello")
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "speech failed: openai answered 503 Service Unavailable: upstream is down"
    );

    // Nothing listens at this address: the port was free a moment ago.
    let port = std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port();
    let mut gone = endpoint(&server, "whisper-1");
    gone.provider.base_url = format!("http://127.0.0.1:{port}/v1");
    let error = stt::by_http(&client, &gone, "", vec![0; 64]).await.unwrap_err();
    assert_eq!(error, "transcription failed: could not connect to openai");

    // A reply that is not what the endpoint promises.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "segments": [] })))
        .mount(&server)
        .await;
    let error = stt::by_http(&client, &endpoint(&server, "whisper-1"), "", vec![0; 64])
        .await
        .unwrap_err();
    assert_eq!(error, "transcription failed: the answer of openai has no text");
}

#[tokio::test]
async fn speech_is_requested_as_wav_and_played_by_the_player_program() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/audio/speech"))
        .and(header("authorization", "Bearer sk-test-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(streamed_speech(), "audio/wav"))
        .mount(&server)
        .await;
    let client = reqwest::Client::new();

    let audio = speak::synthesize(
        &client,
        &endpoint(&server, "gpt-4o-mini-tts"),
        " ",
        "The tests pass.",
    )
    .await
    .unwrap();
    assert_eq!(audio, streamed_speech());
    let audio = speak::synthesize(&client, &endpoint(&server, "tts-1"), "nova", "Done.")
        .await
        .unwrap();
    assert_eq!(audio, streamed_speech());
    let requests = server.received_requests().await.unwrap();
    let bodies: Vec<Value> = requests.iter().map(|r| r.body_json().unwrap()).collect();
    assert_eq!(
        bodies[0],
        json!({
            "model": "gpt-4o-mini-tts",
            "voice": "alloy",
            "input": "The tests pass.",
            "response_format": "wav",
        })
    );
    assert_eq!(bodies[1]["voice"], "nova");

    // The player gets a file whose header lengths are filled in: the fixture refuses others.
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("played.txt");
    let mut config = config_for(&server);
    config.voice.play_command = fake(&format!("play \"{}\"", marker.display()));
    speak::play(&config.voice, &audio, &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "24000 Hz, 1 channel, 250 ms"
    );

    // The whole path, from the settings: provider, model, voice, player with a `{file}` hole.
    let marker = dir.path().join("said.txt");
    config.voice.tts = "speech/tts-1".to_string();
    config.voice.voice = "onyx".to_string();
    config.voice.play_command = fake(&format!("play \"{}\" {{file}}", marker.display()));
    speak::say(&config, &credentials(), "All done.", &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(&marker).unwrap(),
        "24000 Hz, 1 channel, 250 ms"
    );
    let requests = server.received_requests().await.unwrap();
    let last: Value = requests.last().unwrap().body_json().unwrap();
    assert_eq!(
        (
            last["model"].as_str(),
            last["voice"].as_str(),
            last["input"].as_str()
        ),
        (Some("tts-1"), Some("onyx"), Some("All done."))
    );
}

#[tokio::test]
async fn playback_stops_when_it_is_cancelled() {
    let dir = tempfile::tempdir().unwrap();
    let marker = dir.path().join("playing.txt");
    let mut config = Config::default();
    config.voice.play_command = fake(&format!("play --hang \"{}\"", marker.display()));
    let cancel = CancellationToken::new();
    let stopper = cancel.clone();
    let started = marker.clone();
    tokio::spawn(async move {
        while !started.is_file() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        stopper.cancel();
    });
    let played = tokio::time::timeout(
        Duration::from_secs(60),
        speak::play(&config.voice, &streamed_speech(), &cancel),
    )
    .await
    .expect("a cancelled player must not be waited for");
    assert_eq!(played, Ok(()));

    // Audio that is not WAV, a player that is missing, a player that fails.
    let error = speak::play(&config.voice, b"ID3 not wav", &cancel)
        .await
        .unwrap_err();
    assert_eq!(error, "speech cannot be played: not a WAV stream");
    config.voice.play_command = "azazel-no-such-player -q".to_string();
    let error = speak::play(&config.voice, &streamed_speech(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "`azazel-no-such-player` was not found (from [voice] play_command)"
    );
    config.voice.play_command = fake("play");
    let error = speak::play(&config.voice, &streamed_speech(), &CancellationToken::new())
        .await
        .unwrap_err();
    assert!(
        error.starts_with("fake-audio failed (") && error.ends_with("play needs a marker and a file"),
        "{error}"
    );
}

#[tokio::test]
async fn a_recording_from_a_recorder_program_lands_in_the_editor() {
    let mut config = Config::default();
    // Half a second of 44.1 kHz stereo, then the recorder waits to be stopped.
    config.voice.record_command = fake("record 500");
    config.voice.stt_command = fake("transcribe {file}");

    let mut recording = record::start(&config.voice).await.unwrap();
    // The meter moves once the tone is being read.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while recording.level() == 0.0 {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recorder wrote nothing"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(recording.level() > 0.4, "level {}", recording.level());
    assert!(!recording.ended(), "the recorder is still running");
    assert!(recording.elapsed() > Duration::ZERO);
    // The fixture needs a moment to finish writing; a real recorder has nothing more to say.
    tokio::time::sleep(Duration::from_millis(500)).await;

    // The transcriber reports the file it was given: all of the audio, as 16 kHz mono.
    let text = voice::dictate(recording, &config, &credentials()).await.unwrap();
    assert_eq!(text, "16000 Hz, 1 channel, 500 ms");

    let mut editor = Editor::new();
    editor.set_text("note:");
    voice::insert(&mut editor, &text);
    assert_eq!(editor.text(), "note: 16000 Hz, 1 channel, 500 ms");
}

/// Waits, as the UI does on its ticks, for a recorder that ends by itself.
async fn ended(recording: &mut record::Recording) {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(60);
    while !recording.ended() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the recorder did not exit"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

#[tokio::test]
async fn a_recorder_may_write_raw_audio_and_end_by_itself() {
    let mut config = Config::default();
    config.voice.record_command = fake("record --raw --exit 250");
    let mut recording = record::start(&config.voice).await.unwrap();
    ended(&mut recording).await;
    let audio = recording.finish().await.unwrap();
    let pcm = wav::decode(&audio).unwrap();
    assert_eq!((pcm.rate, pcm.channels, pcm.samples.len()), (16_000, 1, 4_000));
    // The tone is still there, at the level the fixture wrote it.
    let peak = pcm.samples.iter().fold(0.0f32, |peak, s| peak.max(s.abs()));
    assert!((peak - 16_000.0 / 32_768.0).abs() < 0.01, "peak {peak}");
}

#[tokio::test]
async fn a_recorder_that_cannot_record_says_why() {
    let mut config = Config::default();
    config.voice.stt_command = fake("transcribe {file}");
    config.voice.record_command = fake("record --fail");
    let mut recording = record::start(&config.voice).await.unwrap();
    ended(&mut recording).await;
    let error = voice::dictate(recording, &config, &credentials())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "fake-audio recorded nothing: fake-audio: audio open error: Device or resource busy"
    );

    config.voice.record_command = "azazel-no-such-recorder -f wav".to_string();
    let error = record::start(&config.voice).await.err().unwrap();
    assert_eq!(error, "`azazel-no-such-recorder` was not found");

    // A transcriber that fails, and one that is missing.
    config.voice.record_command = fake("record --exit 100");
    config.voice.stt_command = fake("transcribe no-such-dir/{file}");
    let mut recording = record::start(&config.voice).await.unwrap();
    ended(&mut recording).await;
    let error = voice::dictate(recording, &config, &credentials())
        .await
        .unwrap_err();
    assert!(error.starts_with("fake-audio failed ("), "{error}");
    config.voice.stt_command = "azazel-no-such-whisper {file}".to_string();
    let error = stt::transcribe(&config, &credentials(), Vec::new())
        .await
        .unwrap_err();
    assert_eq!(
        error,
        "`azazel-no-such-whisper` was not found (from [voice] stt_command)"
    );
}

#[test]
fn doctor_reports_the_voice_setup() {
    let line = |findings: &[azazel_cli::doctor::Finding], start: &str| {
        let found = findings
            .iter()
            .find(|finding| finding.text.starts_with(start))
            .unwrap_or_else(|| panic!("no finding starts with `{start}`"));
        (found.severity, found.text.clone())
    };
    let mut config = Config::default();
    let findings = voice::findings(&config, &credentials());
    assert_eq!(findings.len(), 3);
    assert!(findings[0].text.starts_with("voice recorder: "));
    assert_eq!(
        line(&findings, "voice transcription"),
        (
            Severity::Ok,
            "voice transcription: not set up; set [voice] stt = \"provider/model\" or stt_command to dictate"
                .to_string()
        )
    );
    assert_eq!(
        line(&findings, "spoken replies"),
        (
            Severity::Ok,
            "spoken replies: not set up; set [voice] tts = \"provider/model\" to use /speak".to_string()
        )
    );

    // Programs for every step.
    config.voice.record_command = fake("record 100");
    config.voice.stt_command = fake("transcribe {file}");
    config.voice.play_command = fake("play marker");
    config.voice.tts = "speech/tts-1".to_string();
    config.providers.insert(
        "speech".to_string(),
        ProviderSettings {
            api: Some(WireApi::OpenAiChat),
            base_url: Some("http://127.0.0.1:9/v1".to_string()),
            ..ProviderSettings::default()
        },
    );
    let findings = voice::findings(&config, &credentials());
    let program = FIXTURE.display().to_string();
    assert_eq!(
        line(&findings, "voice recorder"),
        (Severity::Ok, format!("voice recorder: the program {program}"))
    );
    assert_eq!(
        line(&findings, "voice transcription"),
        (
            Severity::Ok,
            format!("voice transcription: the program {program}")
        )
    );
    assert_eq!(
        line(&findings, "spoken replies: tts-1"),
        (
            Severity::Ok,
            "spoken replies: tts-1 at speech, no key needed".to_string()
        )
    );
    assert_eq!(
        line(&findings, "spoken replies: played"),
        (
            Severity::Ok,
            format!("spoken replies: played by the program {program}")
        )
    );

    // What is missing is a warning that names the setting.
    config.voice.record_command = "azazel-no-such-recorder".to_string();
    config.voice.stt_command.clear();
    config.voice.stt = "whisper-1".to_string();
    config.voice.tts = "nowhere/tts-1".to_string();
    config.voice.play_command = "azazel-no-such-player".to_string();
    let findings = voice::findings(&config, &credentials());
    let texts: Vec<(Severity, &str)> = findings.iter().map(|f| (f.severity, f.text.as_str())).collect();
    assert_eq!(
        texts,
        [
            (
                Severity::Warn,
                "voice recorder: `azazel-no-such-recorder` was not found (from [voice] record_command)"
            ),
            (
                Severity::Warn,
                "voice transcription: [voice] stt must be \"provider/model\", not \"whisper-1\""
            ),
            (
                Severity::Warn,
                "spoken replies: [voice] tts: there is no provider named `nowhere`"
            ),
            (
                Severity::Warn,
                "spoken replies: `azazel-no-such-player` was not found (from [voice] play_command)"
            ),
        ]
    );
}

#[test]
fn the_doctor_command_includes_voice() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    std::fs::write(
        dir.path().join("home/config.toml"),
        "[voice]\nstt = \"openai/whisper-1\"\nlanguage = \"en\"\n",
    )
    .unwrap();
    let output = Command::cargo_bin("azazel")
        .unwrap()
        .env("AZAZEL_HOME", dir.path().join("home"))
        .env_remove("OPENAI_API_KEY")
        .arg("--cwd")
        .arg(dir.path().join("project"))
        .arg("doctor")
        .output()
        .unwrap();
    let report = String::from_utf8_lossy(&output.stdout);
    assert!(
        report.contains("warn  voice transcription: whisper-1 at openai, but openai has no credential"),
        "{report}"
    );
    assert!(report.contains("voice recorder: "), "{report}");
    assert!(report.contains("ok    spoken replies: not set up"), "{report}");
}
