// Windows is where starting again has a way of its own: no process can take the place of
// another there, so the new one is started beside the old one, which waits for it. Elsewhere
// it is one call of the standard library.
#![cfg(windows)]

//! `/restart` in a terminal, under a shell as a person runs it: the UI ends, azazel starts
//! again on the open session in the same terminal, the shell does not get the terminal back
//! in between, and it gets azazel's exit code when the new process ends.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_core::process::Shell;
use azazel_tools::pty::{Key, Mods};
use azazel_tools::{Terminal, TerminalState, Terminals};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(90);

/// Waits until `wanted` holds; `what` says what was waited for when it never does.
fn until(terminal: &Terminal, what: &str, wanted: impl Fn() -> bool) {
    let deadline = Instant::now() + LIMIT;
    while !wanted() {
        assert!(
            Instant::now() < deadline,
            "{what} did not happen ({}); the screen:\n{}",
            terminal.describe(),
            terminal.snapshot().text()
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn shows(terminal: &Terminal, text: &str) {
    until(terminal, &format!("`{text}` on the screen"), || {
        terminal.snapshot().text().contains(text)
    });
}

/// Types `text` and, once the input shows it, presses Enter: keys that arrive in one burst
/// would be taken for a paste.
fn enter(terminal: &Terminal, text: &str) {
    assert!(terminal.type_text(text), "{}", terminal.describe());
    shows(terminal, &format!("❯ {}", text.trim_end()));
    assert!(terminal.press(Key::Enter, Mods::default()));
}

/// The requests the scripted provider was sent.
fn requests(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
        .collect()
}

/// `text` as one word of a command line of PowerShell.
fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

#[test]
fn restart_opens_the_session_again_in_the_same_terminal_and_hands_on_the_exit_code() {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".azazel")).unwrap();
    std::fs::create_dir_all(dir.path().join("home")).unwrap();
    let script = dir.path().join("script.json");
    // The process that starts again reads the script from its start.
    std::fs::write(
        &script,
        json!({ "lead": [{ "reply": { "text": "the first answer" } }] }).to_string(),
    )
    .unwrap();
    let asked = dir.path().join("requests.jsonl");

    let path = |path: &Path| path.to_string_lossy().into_owned();
    let env = vec![
        ("AZAZEL_HOME".to_string(), path(&dir.path().join("home"))),
        ("AZAZEL_MOCK_SCRIPT".to_string(), path(&script)),
        ("AZAZEL_MOCK_REQUESTS".to_string(), path(&asked)),
        // The input's mark is looked for on the screen.
        ("AZAZEL_GLYPHS".to_string(), "unicode".to_string()),
    ];
    // PowerShell, which every Windows has. It ends with what azazel ended with, plus 40: a
    // code no shell ends with by itself. Without images the terminal is asked nothing at the
    // start: an answer that came late, on a busy machine, would be read as typed text.
    let shell = Shell::resolve("powershell");
    let command = format!(
        "& {} --cwd {} --no-memory --set session.auto_name=false --set tui.popups=false --set update.check=false \
         --set tui.images=off --mode plan remember this; exit ($LASTEXITCODE + 40)",
        quoted(&path(&assert_cmd::cargo::cargo_bin("azazel"))),
        quoted(&path(&project)),
    );
    let terminals = Arc::new(Terminals::new(dir.path().join("terminals")));
    let terminal = terminals.start(&shell, &command, &project, &env, "test").unwrap();

    // The prompt on the command line is answered; the session is on the mode that was given.
    shows(&terminal, "the first answer");
    until(&terminal, "the first request", || requests(&asked).len() == 1);
    shows(&terminal, "plan ·");
    enter(&terminal, "/mode auto ");
    shows(&terminal, "auto ·");

    // Starting again: the UI comes back on the same session, and on the mode it was switched
    // to, not the one the command line named. The shell has not taken the terminal back: what
    // is typed goes to azazel, as a prompt, and the prompt of the command line is not sent
    // again.
    enter(&terminal, "/restart ");
    shows(&terminal, "resumed session");
    shows(&terminal, "auto ·");
    let screen = terminal.snapshot().text();
    assert!(!screen.contains("plan ·"), "{screen}");
    enter(&terminal, "and now this");
    until(&terminal, "the second request", || requests(&asked).len() == 2);
    let second = &requests(&asked)[1];
    let said: Vec<(&str, &str)> = second["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            (
                message["role"].as_str().unwrap(),
                message["content"][0]["text"].as_str().unwrap(),
            )
        })
        .collect();
    assert_eq!(
        said,
        [
            ("user", "remember this"),
            ("assistant", "the first answer"),
            ("user", "and now this")
        ]
    );
    // The answer is the script's first again: the new process read the script from its start.
    until(&terminal, "the second answer", || {
        terminal.snapshot().text().matches("the first answer").count() >= 2
    });
    assert_eq!(terminal.state(), TerminalState::Running);

    // Quitting the new process ends the one the shell started, with its exit code.
    enter(&terminal, "/quit ");
    until(&terminal, "the end of azazel", || !terminal.running());
    assert_eq!(terminal.state(), TerminalState::Exited(40));
}
