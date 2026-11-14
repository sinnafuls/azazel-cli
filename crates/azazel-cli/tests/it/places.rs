// Going to another directory starts azazel again in the same terminal, the way `/restart`
// does; on Windows that is a process of its own the old one waits for. The terminal is driven
// through PowerShell, as `tests/restart.rs` does.
#![cfg(windows)]

//! `/open`, `/cd` and `/worktree` in a terminal: the session goes on in another directory or
//! project, read afresh there, and its file follows it into that project's sessions.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_core::process::Shell;
use azazel_tools::pty::{Key, Mods};
use azazel_tools::{Terminal, Terminals};
use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(90);

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

/// Types `text` and, once the input shows it, presses Enter.
fn enter(terminal: &Terminal, text: &str) {
    assert!(terminal.type_text(text), "{}", terminal.describe());
    shows(terminal, &format!("❯ {}", text.trim_end()));
    assert!(terminal.press(Key::Enter, Mods::default()));
}

fn requests(file: &Path) -> Vec<Value> {
    std::fs::read_to_string(file)
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
        .collect()
}

/// What the user and the agent said in a request, in order.
fn said(request: &Value) -> Vec<String> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            message["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect()
}

/// Clears the screen, sends `prompt` and waits for its answer: the agent is idle after it.
fn ask(terminal: &Terminal, prompt: &str, asked: &Path, count: usize) {
    enter(terminal, "/clear ");
    until(terminal, "a clear screen", || {
        !terminal.snapshot().text().contains("noted")
    });
    enter(terminal, prompt);
    until(terminal, "the request", || requests(asked).len() == count);
    shows(terminal, "noted");
}

fn quoted(text: &str) -> String {
    format!("'{}'", text.replace('\'', "''"))
}

fn git(dir: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// The session files of every project, by the project directory's name under `projects`.
fn session_files(home: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    for project in std::fs::read_dir(home.join("projects"))
        .into_iter()
        .flatten()
        .flatten()
    {
        let slug = project.file_name().to_string_lossy().into_owned();
        for file in std::fs::read_dir(project.path().join("sessions"))
            .into_iter()
            .flatten()
            .flatten()
        {
            if file.path().extension().is_some_and(|ext| ext == "jsonl") {
                found.push((slug.clone(), file.path()));
            }
        }
    }
    found
}

struct Setup {
    dir: tempfile::TempDir,
    terminals: Arc<Terminals>,
}

impl Setup {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        // Every process answers from the script's start: one answer is all each one needs.
        std::fs::write(
            dir.path().join("script.json"),
            json!({ "lead": [{ "reply": { "text": "noted" } }] }).to_string(),
        )
        .unwrap();
        let terminals = Arc::new(Terminals::new(dir.path().join("terminals")));
        Setup { dir, terminals }
    }

    fn path(&self, name: &str) -> PathBuf {
        dunce::canonicalize(self.dir.path()).unwrap().join(name)
    }

    fn home(&self) -> PathBuf {
        self.path("home")
    }

    fn asked(&self) -> PathBuf {
        self.path("requests.jsonl")
    }

    fn env(&self) -> Vec<(String, String)> {
        let path = |path: PathBuf| path.to_string_lossy().into_owned();
        vec![
            ("AZAZEL_HOME".to_string(), path(self.home())),
            ("AZAZEL_MOCK_SCRIPT".to_string(), path(self.path("script.json"))),
            ("AZAZEL_MOCK_REQUESTS".to_string(), path(self.asked())),
            ("AZAZEL_GLYPHS".to_string(), "unicode".to_string()),
        ]
    }

    /// The command line that starts azazel in `cwd`, with `rest` after the options.
    fn command(&self, cwd: &Path, rest: &str) -> String {
        format!(
            "& {} --cwd {} --no-memory --set session.auto_name=false --set tui.popups=false --set update.check=false \
             --set tui.images=off {rest}; exit $LASTEXITCODE",
            quoted(&assert_cmd::cargo::cargo_bin("azazel").to_string_lossy()),
            quoted(&cwd.to_string_lossy()),
        )
    }

    fn start(&self, cwd: &Path, rest: &str) -> Arc<Terminal> {
        let shell = Shell::resolve("powershell");
        self.terminals
            .start(&shell, &self.command(cwd, rest), cwd, &self.env(), "test")
            .unwrap()
    }

    /// Runs azazel in print mode in `cwd`, to leave a session there.
    fn print(&self, cwd: &Path, prompt: &str) {
        let mut command = std::process::Command::new(assert_cmd::cargo::cargo_bin("azazel"));
        for (key, value) in self.env() {
            command.env(key, value);
        }
        let output = command
            .args([
                "--cwd",
                &cwd.to_string_lossy(),
                "--no-memory",
                "--set",
                "session.auto_name=false",
                "-p",
                prompt,
            ])
            .stdin(std::process::Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

#[test]
fn cd_and_worktree_carry_the_session_along_and_move_its_file() {
    let setup = Setup::new();
    let repo = setup.path("repo");
    std::fs::create_dir_all(repo.join("sub")).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    // Files are compared byte for byte, whatever the machine's git converts line ends to.
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("sub/kept.txt"), "tracked\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "first"]);
    std::fs::write(repo.join("draft.txt"), "uncommitted\n").unwrap();

    let terminal = setup.start(&repo, "remember this");
    shows(&terminal, "noted");
    until(&terminal, "the first request", || {
        requests(&setup.asked()).len() == 1
    });

    // Within the project: the session goes on where it is, working in `sub`.
    enter(&terminal, "/cd sub");
    until(&terminal, "the restart", || {
        terminal.snapshot().text().contains("resumed session")
    });
    ask(&terminal, "where now", &setup.asked(), 2);
    let second = &requests(&setup.asked())[1];
    assert_eq!(said(second), ["remember this", "noted", "where now"]);
    let system = second["system"].to_string();
    let sub = repo.join("sub").to_string_lossy().replace('\\', "\\\\");
    assert!(system.contains(&sub), "{system}");
    let files = session_files(&setup.home());
    assert_eq!(files.len(), 1, "{files:?}");

    // Into a worktree: the uncommitted file goes along, and the session file moves to the
    // worktree's project.
    // The screen `ask` cleared tells the azazel that starts again from this one.
    assert!(!terminal.snapshot().text().contains("resumed session"));
    enter(&terminal, "/worktree feat ");
    shows(&terminal, "resumed session");
    let worktree = repo.join(".azazel").join("worktrees").join("feat");
    assert_eq!(
        std::fs::read_to_string(worktree.join("draft.txt")).unwrap(),
        "uncommitted\n"
    );
    assert!(!repo.join("draft.txt").exists());
    assert_eq!(
        git(
            &repo,
            &["branch", "--list", "--format=%(refname:short)", "azazel/feat"]
        ),
        "azazel/feat"
    );
    ask(&terminal, "and here", &setup.asked(), 3);
    let third = &requests(&setup.asked())[2];
    assert_eq!(
        said(third),
        ["remember this", "noted", "where now", "noted", "and here"]
    );
    let inside = worktree.join("sub").to_string_lossy().replace('\\', "\\\\");
    assert!(
        third["system"].to_string().contains(&inside),
        "{}",
        third["system"]
    );
    let moved = session_files(&setup.home());
    assert_eq!(moved.len(), 1, "{moved:?}");
    assert_ne!(moved[0].0, files[0].0, "the session is now the worktree's");
    assert_eq!(moved[0].1.file_name(), files[0].1.file_name());

    enter(&terminal, "/worktree list ");
    shows(&terminal, "feat (here)  azazel/feat");
    enter(&terminal, "/quit ");
    until(&terminal, "the end of azazel", || !terminal.running());
}

#[test]
fn open_lists_the_sessions_of_every_project_and_goes_to_the_one_chosen() {
    let setup = Setup::new();
    let first = setup.path("first");
    let second = setup.path("second");
    for project in [&first, &second] {
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
    }
    setup.print(&first, "work of the first project");

    let terminal = setup.start(&second, "");
    shows(&terminal, "mock/lead");
    enter(&terminal, "/open ");
    shows(&terminal, "Open a session of any project");
    shows(&terminal, "work of the first project");
    assert!(terminal.press(Key::Enter, Mods::default()));
    until(&terminal, "the session of the other project", || {
        terminal.snapshot().text().contains("resumed session")
    });
    ask(&terminal, "go on", &setup.asked(), 2);
    let request = &requests(&setup.asked())[1];
    assert_eq!(said(request), ["work of the first project", "noted", "go on"]);
    let there = first.to_string_lossy().replace('\\', "\\\\");
    assert!(
        request["system"].to_string().contains(&there),
        "{}",
        request["system"]
    );
    enter(&terminal, "/quit ");
    until(&terminal, "the end of azazel", || !terminal.running());
}
