#![cfg(feature = "detach")]

//! `azazel --ssh`: a session on another machine. The other machine is this one, reached
//! through the `fake-ssh` fixture, which runs the command it is given the way `ssh` would hand
//! it to a shell there. With `AZAZEL_TEST_SSH=1` the same runs go through the real `ssh` to
//! `localhost` as well (CI does that on Linux, where it sets a key up for it).

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::LazyLock;
use std::sync::mpsc::{Receiver, channel};
use std::time::{Duration, Instant};

use serde_json::{Value, json};

const LIMIT: Duration = Duration::from_secs(60);

/// The `fake-ssh` fixture, built on first use into the directory this test runs from.
static FAKE_SSH: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-ssh", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-ssh fixture"
    );
    let binary = profile_dir.join(format!("fake-ssh{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

/// How the other machine is reached.
#[derive(Clone, Copy, PartialEq)]
enum Through {
    /// The fixture, found as `ssh` on the path.
    Fake,
    /// The `ssh` program, to `localhost`.
    Real,
}

/// A home and a project of their own, both "on the other machine".
struct Sandbox {
    dir: tempfile::TempDir,
    through: Through,
}

/// A path as a word that needs no quoting on any shell.
fn word(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

impl Sandbox {
    fn new(script: &Value, through: Through) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        std::fs::create_dir_all(dir.path().join("here")).unwrap();
        std::fs::write(dir.path().join("script.json"), script.to_string()).unwrap();
        if through == Through::Fake {
            let bin = dir.path().join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            let ssh = bin.join(format!("ssh{}", std::env::consts::EXE_SUFFIX));
            std::fs::copy(&*FAKE_SSH, ssh).unwrap();
        }
        Sandbox { dir, through }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn project(&self) -> PathBuf {
        self.dir.path().join("project")
    }

    /// `azazel --ssh <the other machine>` with `args` after it. Nothing of the sandbox is in
    /// the directory it is started from: the project is the other machine's.
    fn azazel(&self, args: &[&str]) -> Command {
        self.azazel_to(None, None, args)
    }

    /// Like [`Sandbox::azazel`], to the machine `host` and with azazel started there as
    /// `remote` says, when they are given.
    fn azazel_to(&self, host: Option<&str>, remote: Option<&str>, args: &[&str]) -> Command {
        let binary = assert_cmd::cargo::cargo_bin("azazel");
        let variables = [
            ("AZAZEL_HOME", word(&self.home())),
            ("AZAZEL_MOCK_SCRIPT", word(&self.dir.path().join("script.json"))),
            (
                "AZAZEL_MOCK_REQUESTS",
                word(&self.dir.path().join("requests.jsonl")),
            ),
        ];
        let mut command = Command::new(&binary);
        command.current_dir(self.dir.path().join("here"));
        let (default_host, remote_azazel) = match self.through {
            // The command runs as a child of this one and has its environment.
            Through::Fake => {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let mut dirs = vec![self.dir.path().join("bin")];
                dirs.extend(std::env::split_paths(&path));
                command.env("PATH", std::env::join_paths(dirs).unwrap());
                command.envs(variables.iter().map(|(name, value)| (name, value)));
                ("build-box", word(&binary))
            }
            // A login there starts from a clean environment: the variables go with the command.
            Through::Real => {
                let set: Vec<String> = variables
                    .iter()
                    .map(|(name, value)| format!("{name}={value}"))
                    .collect();
                command.env("AZAZEL_HOME", self.dir.path().join("here-home"));
                ("localhost", format!("env {} {}", set.join(" "), word(&binary)))
            }
        };
        command
            .arg(format!("--ssh={}", host.unwrap_or(default_host)))
            .args(["--remote-azazel", remote.unwrap_or(&remote_azazel)])
            .arg("--remote-dir")
            .arg(self.project())
            .args(["--set", "session.auto_name=false", "--no-memory"])
            .args(["--set", "notify.desktop=\"off\""])
            .args(args)
            .stdin(Stdio::null());
        command
    }

    fn run(&self, args: &[&str]) -> Output {
        self.azazel(args).output().unwrap()
    }

    fn ok(&self, args: &[&str]) -> String {
        let output = self.run(args);
        assert!(
            output.status.success(),
            "{args:?}: {}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8(output.stdout).unwrap()
    }

    fn ps(&self) -> Vec<Value> {
        self.ok(&["ps", "--json"])
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect()
    }

    fn wait_for_state(&self, id: &str, state: &str) {
        let started = Instant::now();
        loop {
            let rows = self.ps();
            if rows.iter().any(|row| row["id"] == id && row["state"] == state) {
                return;
            }
            assert!(started.elapsed() < LIMIT, "{id} never was `{state}`: {rows:?}");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let listed = self.azazel(&["ps", "--json"]).output();
        let rows = listed
            .map(|output| String::from_utf8_lossy(&output.stdout).into_owned())
            .unwrap_or_default();
        for row in rows
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        {
            if let Some(id) = row["id"].as_str() {
                let _ = self.azazel(&["kill", id]).output();
            }
        }
    }
}

fn lines_of(reader: impl std::io::Read + Send + 'static) -> Receiver<String> {
    let (sender, lines) = channel();
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines().map_while(Result::ok) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    lines
}

fn read_until(lines: &Receiver<String>, wanted: &str) -> Vec<String> {
    let mut read = Vec::new();
    loop {
        let line = lines
            .recv_timeout(LIMIT)
            .unwrap_or_else(|_| panic!("no line with `{wanted}` after {read:?}"));
        let found = line.contains(wanted);
        read.push(line);
        if found {
            return read;
        }
    }
}

/// The ways to reach the other machine that this run of the tests has.
fn ways() -> Vec<Through> {
    let mut ways = vec![Through::Fake];
    if std::env::var("AZAZEL_TEST_SSH").is_ok_and(|value| value == "1") {
        ways.push(Through::Real);
    }
    ways
}

#[test]
fn a_prompt_runs_on_the_other_machine_and_its_reply_is_printed_here() {
    for through in ways() {
        let script = json!({ "lead": [
            { "reply": { "tool_calls": [{ "id": "c1", "name": "write",
                                           "input": { "path": "made-there.txt", "content": "x\n" } }] } },
            { "reply": { "text": "Done on the other machine." } },
        ] });
        let sandbox = Sandbox::new(&script, through);
        // The mode is this run's; it is the other azazel that works in it.
        let said = sandbox.ok(&["--mode", "auto", "-p", "make", "the", "file"]);
        assert_eq!(said.trim(), "Done on the other machine.");
        // Its tools worked in the directory of the other machine, and its session is kept there.
        assert_eq!(
            std::fs::read_to_string(sandbox.project().join("made-there.txt")).unwrap(),
            "x\n"
        );
        assert!(sandbox.home().join("projects").is_dir());
        assert!(!sandbox.dir.path().join("here").join("made-there.txt").exists());
    }
}

#[test]
fn a_background_session_there_outlives_the_connection_and_is_come_back_to() {
    for through in ways() {
        let script = json!({ "lead": [
            { "reply": { "text": "Starting.", "tool_calls": [{ "id": "c1", "name": "read",
                                                               "input": { "path": "notes.txt" } }] } },
            { "delay_ms": 4000, "reply": { "text": "Finished after the connection broke." } },
        ] });
        let sandbox = Sandbox::new(&script, through);
        std::fs::write(sandbox.project().join("notes.txt"), "a note\n").unwrap();
        let id = sandbox.ok(&["--detach", "read the notes"]).trim().to_string();
        assert!(
            id.len() == 8 && id.chars().all(|c| c.is_ascii_hexdigit()),
            "an id: {id:?}"
        );
        assert!(sandbox.ps().iter().any(|row| row["id"] == id));

        // A client that is cut off: its process is killed, as when the network drops.
        let mut first = sandbox
            .azazel(&["--attach", &id, "--plain"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let shown = lines_of(first.stdout.take().unwrap());
        let before = read_until(&shown, "→ read notes.txt");
        assert!(before.contains(&"❯ read the notes".to_string()), "{before:?}");
        first.kill().unwrap();
        first.wait().unwrap();

        // The session went on meanwhile; whoever attaches next is told what was missed.
        sandbox.wait_for_state(&id, "idle");
        let mut second = sandbox
            .azazel(&["--attach", &id, "--plain"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut typing = second.stdin.take().unwrap();
        let shown = lines_of(second.stdout.take().unwrap());
        let missed = read_until(&shown, "· done");
        assert!(
            missed.contains(&"Finished after the connection broke.".to_string()),
            "{missed:?}"
        );
        assert!(
            !missed.iter().any(|line| line.contains("read the notes")),
            "{missed:?}"
        );
        writeln!(typing, "/detach").unwrap();
        read_until(&shown, "detached");
        assert!(second.wait().unwrap().success());

        let log = sandbox.ok(&["logs", &id]);
        assert!(log.contains("Finished after the connection broke."), "{log}");
        assert_eq!(sandbox.ok(&["kill", &id]).trim(), format!("stopped {id}"));
        assert!(sandbox.ps().is_empty());
    }
}

#[test]
fn a_machine_without_azazel_is_told_how_to_get_it() {
    let sandbox = Sandbox::new(&json!({ "lead": [] }), Through::Fake);
    let output = sandbox
        .azazel_to(None, Some("no-such-azazel"), &["-p", "hello"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(
        said.contains("could not start azazel on build-box") && said.contains("command not found"),
        "{said}"
    );
    assert!(said.contains("install/install.sh | sh"), "{said}");

    // A machine that cannot be reached says so in ssh's own words.
    let output = sandbox
        .azazel_to(Some("unreachable"), None, &["-p", "hello"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(1), "{said}");
    assert!(said.contains("Connection refused"), "{said}");

    // A name that ssh would read as an option is not passed to it.
    let output = sandbox
        .azazel_to(Some("-oProxyCommand=x"), None, &["-p", "hello"])
        .output()
        .unwrap();
    let said = String::from_utf8_lossy(&output.stderr).into_owned();
    assert_eq!(output.status.code(), Some(2), "{said}");
    assert!(said.contains("--ssh takes the name of a machine"), "{said}");
}
