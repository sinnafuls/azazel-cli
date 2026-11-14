#![allow(clippy::unwrap_used)]

//! Sessions across projects from the shell: `azazel sessions import`, `azazel export
//! --sanitize`, and the print-mode flags `-f/--file`, `--worktree` and `--archived`.

use std::path::{Path, PathBuf};
use std::process::Output;

use assert_cmd::Command;
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

const KEY: &str = "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("other/.git")).unwrap();
        std::fs::create_dir_all(dir.path().join("home")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn project(&self) -> PathBuf {
        self.path("project")
    }

    fn requests_file(&self) -> PathBuf {
        self.path("requests.jsonl")
    }

    fn command(&self, cwd: &Path, args: &[&str]) -> Command {
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env_remove("AZAZEL_MOCK_SCRIPT")
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(cwd)
            .args(args)
            .write_stdin("");
        command
    }

    /// `azazel` in `cwd`, answering from `script`, every request recorded.
    fn scripted(&self, cwd: &Path, script: &Value, args: &[&str]) -> Output {
        let script_path = self.path("script.json");
        std::fs::write(&script_path, script.to_string()).unwrap();
        let _ = std::fs::remove_file(self.requests_file());
        self.command(
            cwd,
            &[&["--no-memory", "--set", "session.auto_name=false"][..], args].concat(),
        )
        .env("AZAZEL_MOCK_SCRIPT", &script_path)
        .env("AZAZEL_MOCK_REQUESTS", self.requests_file())
        .output()
        .unwrap()
    }

    fn azazel(&self, cwd: &Path, args: &[&str]) -> Output {
        self.command(cwd, args).output().unwrap()
    }

    fn requests(&self) -> Vec<Value> {
        std::fs::read_to_string(self.requests_file())
            .unwrap_or_default()
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).unwrap()["request"].clone())
            .collect()
    }
}

fn ok(text: &str) -> Value {
    json!({ "lead": [{ "reply": { "text": text } }] })
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// The content blocks of the first user message of a request.
fn prompt_blocks(request: &Value) -> Vec<Value> {
    request["messages"]
        .as_array()
        .unwrap()
        .iter()
        .find(|message| message["role"] == "user")
        .unwrap()["content"]
        .as_array()
        .unwrap()
        .clone()
}

#[test]
fn files_of_a_print_run_are_inlined_or_attached_and_bad_ones_are_refused_first() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    std::fs::write(project.join("notes.md"), "remember the milk\n").unwrap();
    let mut png = Vec::new();
    image::RgbaImage::new(3, 2)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .unwrap();
    std::fs::write(project.join("tiny.png"), &png).unwrap();

    let output = sandbox.scripted(
        &project,
        &ok("seen"),
        &["-p", "look at these", "-f", "notes.md", "--file", "tiny.png"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let blocks = prompt_blocks(&sandbox.requests()[0]);
    let text = blocks[0]["text"].as_str().unwrap();
    assert!(text.starts_with("look at these"), "{text}");
    assert!(
        text.ends_with("<file path=\"notes.md\">\nremember the milk\n</file>"),
        "{text}"
    );
    let images: Vec<&Value> = blocks.iter().filter(|block| block["type"] == "image").collect();
    assert_eq!(images.len(), 1, "{blocks:?}");
    assert_eq!(images[0]["mime"], "image/png");

    // A file that is not there, or one too many, stops the run before any request.
    std::fs::write(project.join("blob.bin"), [0u8, 159, 146, 150, 255]).unwrap();
    for (args, said) in [
        (vec!["-p", "x", "-f", "missing.txt"], "missing.txt"),
        (vec!["-p", "x", "-f", "blob.bin"], "neither text nor"),
    ] {
        let output = sandbox.scripted(&project, &ok("never"), &args);
        assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
        assert!(stderr(&output).contains(said), "{}", stderr(&output));
        assert!(sandbox.requests().is_empty());
    }
    let many: Vec<&str> = std::iter::once("-p")
        .chain(std::iter::once("x"))
        .chain(std::iter::repeat_n(["-f", "notes.md"], 101).flatten())
        .collect();
    let output = sandbox.scripted(&project, &ok("never"), &many);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
    assert!(stderr(&output).contains("at most 100"), "{}", stderr(&output));
    // Outside print mode the flag is a usage error.
    let output = sandbox.azazel(&project, &["-f", "notes.md", "sessions", "list"]);
    assert_eq!(output.status.code(), Some(2), "{}", stderr(&output));
}

#[test]
fn an_archived_run_stays_out_of_the_lists_and_of_continue() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    let kept = sandbox.scripted(&project, &ok("kept"), &["-p", "the work to keep"]);
    assert!(kept.status.success(), "{}", stderr(&kept));
    let archived = sandbox.scripted(&project, &ok("ci"), &["-p", "a ci check", "--archived"]);
    assert!(archived.status.success(), "{}", stderr(&archived));

    let listed = stdout(&sandbox.azazel(&project, &["sessions", "list"]));
    assert_eq!(listed.lines().count(), 1, "{listed}");
    assert!(listed.contains("the work to keep"), "{listed}");
    let all = stdout(&sandbox.azazel(&project, &["sessions", "list", "--all"]));
    assert_eq!(all.lines().count(), 2, "{all}");
    assert!(
        all.lines()
            .any(|line| line.contains("a ci check") && line.ends_with("archived")),
        "{all}"
    );

    // `-c` goes on with the latest session that is not archived.
    let again = sandbox.scripted(&project, &ok("more"), &["-p", "-c", "and more"]);
    assert!(again.status.success(), "{}", stderr(&again));
    let said: Vec<String> = sandbox.requests()[0]["messages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|message| {
            message["content"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        })
        .collect();
    assert_eq!(said, ["the work to keep", "kept", "and more"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_are_imported_from_a_file_or_an_address_and_exported_sanitized() {
    let sandbox = Sandbox::new();
    let project = sandbox.project();
    std::fs::write(project.join("recipe.txt"), "the secret recipe\n").unwrap();
    let script = json!({ "lead": [
        { "reply": { "tool_calls": [{ "id": "c1", "name": "read", "input": { "path": "recipe.txt" } }] } },
        { "reply": { "text": "I read it." } }
    ] });
    let run = sandbox.scripted(
        &project,
        &script,
        &["-p", &format!("read recipe.txt; my key is {KEY}")],
    );
    assert!(run.status.success(), "{}", stderr(&run));

    // A plain export keeps everything; it is what a session is brought in from.
    let exported = sandbox.path("export.jsonl");
    let export = sandbox.azazel(
        &project,
        &["export", "--format", "jsonl", "-o", exported.to_str().unwrap()],
    );
    assert!(export.status.success(), "{}", stderr(&export));
    assert!(
        std::fs::read_to_string(&exported)
            .unwrap()
            .contains("the secret recipe")
    );

    // Into another project, from the file.
    let other = sandbox.path("other");
    let imported = sandbox.azazel(
        &project,
        &[
            "sessions",
            "import",
            exported.to_str().unwrap(),
            "--project",
            other.to_str().unwrap(),
        ],
    );
    assert!(imported.status.success(), "{}", stderr(&imported));
    assert!(stdout(&imported).contains("as session"), "{}", stdout(&imported));
    let listed = stdout(&sandbox.azazel(&other, &["sessions", "list", "--json"]));
    let rows: Vec<Value> = listed
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(rows.len(), 1, "{listed}");
    assert_eq!(rows[0]["messages"], 4);
    assert!(
        rows[0]["first_prompt"]
            .as_str()
            .unwrap()
            .starts_with("read recipe.txt")
    );

    // From an address, into this project: a second session beside the first.
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/shared.jsonl"))
        .respond_with(ResponseTemplate::new(200).set_body_string(std::fs::read_to_string(&exported).unwrap()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/missing.jsonl"))
        .respond_with(ResponseTemplate::new(404))
        .mount(&server)
        .await;
    let address = format!("{}/shared.jsonl", server.uri());
    let mut command = sandbox.command(&project, &["sessions", "import", &address]);
    let fetched = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert!(fetched.status.success(), "{}", stderr(&fetched));
    let listed = stdout(&sandbox.azazel(&project, &["sessions", "list"]));
    assert_eq!(listed.lines().count(), 2, "{listed}");
    let missing = format!("{}/missing.jsonl", server.uri());
    let mut command = sandbox.command(&project, &["sessions", "import", &missing]);
    let failed = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    assert_eq!(failed.status.code(), Some(1));
    assert!(stderr(&failed).contains("404"), "{}", stderr(&failed));

    // What is not a session is said so, and nothing is made.
    std::fs::write(sandbox.path("notes.txt"), "just some notes\n").unwrap();
    let refused = sandbox.azazel(
        &project,
        &["sessions", "import", sandbox.path("notes.txt").to_str().unwrap()],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        stderr(&refused).contains("holds no session"),
        "{}",
        stderr(&refused)
    );
    assert_eq!(
        stdout(&sandbox.azazel(&project, &["sessions", "list"]))
            .lines()
            .count(),
        2
    );

    // Sanitized, in every format: no secret and nothing a file held.
    for name in ["shared.md", "shared.html", "shared.jsonl"] {
        let file = sandbox.path(name);
        let output = sandbox.azazel(&other, &["export", "--sanitize", "-o", file.to_str().unwrap()]);
        assert!(output.status.success(), "{}", stderr(&output));
        let text = std::fs::read_to_string(&file).unwrap();
        assert!(!text.contains("the secret recipe"), "{name}: {text}");
        assert!(!text.contains(KEY), "{name}: {text}");
        assert!(text.contains("I read it."), "{name}: {text}");
    }
    let lines = std::fs::read_to_string(sandbox.path("shared.jsonl")).unwrap();
    assert!(lines.contains("[file content removed]"), "{lines}");
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

#[test]
fn a_print_run_in_a_worktree_works_there_and_says_where_it_is() {
    let sandbox = Sandbox::new();
    let repo = sandbox.path("repo");
    std::fs::create_dir_all(&repo).unwrap();
    git(&repo, &["init", "--quiet", "-b", "main"]);
    git(&repo, &["config", "user.email", "test@example.com"]);
    git(&repo, &["config", "user.name", "Test"]);
    git(&repo, &["config", "core.autocrlf", "false"]);
    std::fs::write(repo.join("README.md"), "hello\n").unwrap();
    git(&repo, &["add", "-A"]);
    git(&repo, &["commit", "--quiet", "-m", "first"]);
    std::fs::write(repo.join("draft.txt"), "not committed\n").unwrap();

    let script = json!({ "lead": [
        { "reply": { "tool_calls": [{ "id": "c1", "name": "write", "input": { "path": "made.txt", "content": "here\n" } }] } },
        { "reply": { "text": "made it" } }
    ] });
    let output = sandbox.scripted(
        &repo,
        &script,
        &["-p", "make a file", "--mode", "auto", "--worktree=ci-run"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    assert_eq!(stdout(&output).trim(), "made it");
    let worktree = dunce::canonicalize(&repo)
        .unwrap()
        .join(".azazel")
        .join("worktrees")
        .join("ci-run");
    assert!(
        stderr(&output).contains(&format!("worktree {} (branch azazel/ci-run)", worktree.display())),
        "{}",
        stderr(&output)
    );
    // The work happened there, from HEAD; the tree it was started in is as it was.
    assert_eq!(
        std::fs::read_to_string(worktree.join("made.txt")).unwrap(),
        "here\n"
    );
    assert!(!repo.join("made.txt").exists());
    assert!(repo.join("draft.txt").exists());
    assert!(!worktree.join("draft.txt").exists());
    assert_eq!(
        git(
            &repo,
            &["branch", "--list", "--format=%(refname:short)", "azazel/ci-run"]
        ),
        "azazel/ci-run"
    );
    // The session is one of the worktree's project.
    let listed = stdout(&sandbox.azazel(&worktree, &["sessions", "list"]));
    assert!(listed.contains("make a file"), "{listed}");

    // Without a name it is named after the time; with JSON output its place is the last line.
    let output = sandbox.scripted(
        &repo,
        &ok("fine"),
        &["-p", "hi", "--output", "json", "--worktree"],
    );
    assert!(output.status.success(), "{}", stderr(&output));
    let last: Value = serde_json::from_str(stdout(&output).lines().last().unwrap()).unwrap();
    assert_eq!(last["type"], "worktree");
    assert!(
        last["branch"].as_str().unwrap().starts_with("azazel/20"),
        "{last}"
    );
    assert!(Path::new(last["path"].as_str().unwrap()).is_dir());

    // A name taken, and a session that is not a new one, are refused.
    let taken = sandbox.scripted(&repo, &ok("x"), &["-p", "hi", "--worktree=ci-run"]);
    assert_eq!(taken.status.code(), Some(1), "{}", stderr(&taken));
    assert!(stderr(&taken).contains("already exists"), "{}", stderr(&taken));
    let resumed = sandbox.scripted(&repo, &ok("x"), &["-p", "-c", "hi", "--worktree"]);
    assert_eq!(resumed.status.code(), Some(2), "{}", stderr(&resumed));
}
