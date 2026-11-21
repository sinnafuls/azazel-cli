//! The interactive UI in a terminal, as a user sees it.

use std::io::{Read, Write};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(60);
const WORDS: usize = 2000;

/// A reply that streams `WORDS` words, one event each, the way a fast model does.
fn streamed_reply() -> Value {
    let mut events = vec![json!({"type": "start"})];
    let mut full = String::new();
    for n in 0..WORDS {
        let word = format!("w{n} ");
        full.push_str(&word);
        events.push(json!({"type": "text_delta", "index": 0, "delta": word}));
    }
    let block = json!({"type": "text", "text": full});
    events.push(json!({"type": "block_end", "index": 0, "block": block}));
    let meta = json!({"provider": "mock", "model": "lead", "api": "anthropic-messages", "usage": {}, "stop": "stop"});
    events.push(json!({"type": "done", "message": {"role": "assistant", "content": [block], "meta": meta}}));
    json!({ "lead": [{ "events": events }] })
}

/// A prompt is typed into the UI and `wanted` waits to be shown; returns what the terminal
/// received from the moment the prompt was sent, and how long that took.
fn ask(script: &Value, prompt: &str, wanted: &str) -> (String, Duration) {
    use portable_pty::{CommandBuilder, PtySize, native_pty_system};

    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".azazel")).unwrap();
    std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut command = CommandBuilder::new(assert_cmd::cargo::cargo_bin("azazel"));
    command.env("AZAZEL_HOME", dir.path().join("home"));
    command.env("AZAZEL_MOCK_SCRIPT", dir.path().join("script.json"));
    command.args(["--set", "session.auto_name=false", "--no-memory"]);
    command.args(["--set", "notify.desktop=\"off\""]);
    command.arg("--cwd");
    command.arg(&project);
    let mut child = pair.slave.spawn_command(command).unwrap();
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().unwrap();
    let mut writer = pair.master.take_writer().unwrap();
    let (sender, received) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        let mut buffer = [0u8; 8192];
        while let Ok(read) = reader.read(&mut buffer) {
            if read == 0
                || sender
                    .send(String::from_utf8_lossy(&buffer[..read]).into_owned())
                    .is_err()
            {
                break;
            }
        }
    });
    let started = Instant::now();
    let read_until = |wanted: &str, shown: &mut String, writer: &mut Box<dyn Write + Send>| {
        while !shown.contains(wanted) {
            let left = LIMIT.saturating_sub(started.elapsed());
            let more = received
                .recv_timeout(left)
                .unwrap_or_else(|_| panic!("never showed `{wanted}`: {shown:?}"));
            // A Windows console asks where the cursor is before it shows anything.
            for _ in more.matches("[6n") {
                writer.write_all(b"\x1b[1;1R").unwrap();
                writer.flush().unwrap();
            }
            shown.push_str(&more);
        }
    };
    let mut shown = String::new();
    read_until("mock/lead", &mut shown, &mut writer);
    writer.write_all(prompt.as_bytes()).unwrap();
    writer.flush().unwrap();
    let mut typed = String::new();
    read_until(prompt, &mut typed, &mut writer);
    writer.write_all(b"\r").unwrap();
    writer.flush().unwrap();
    let sent = Instant::now();
    let mut after = String::new();
    read_until(wanted, &mut after, &mut writer);
    let took = sent.elapsed();
    let _ = child.kill();
    let _ = child.wait();
    (after, took)
}

/// Agents stream faster than a frame is drawn. The UI takes in everything that has arrived
/// before it draws, so a screen shows what the agent said now, not what it said seconds ago.
#[test]
fn a_fast_stream_is_not_drawn_once_per_event() {
    let (shown, _) = ask(&streamed_reply(), "go", &format!("w{}", WORDS - 1));
    // Every frame is one synchronized update.
    let frames = shown.matches("\x1b[?2026h").count();
    assert!(frames > 0, "the terminal showed no frames: {shown:?}");
    assert!(frames < WORDS / 10, "{frames} frames for {WORDS} events");
}
