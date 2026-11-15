//! GitHub through `gh` (here `fake-gh`): `#<n>` in a prompt attaches the issue or pull request
//! it names, when it resolves.

use std::path::PathBuf;
use std::sync::LazyLock;

use crate::common::fixture_program;
use assert_cmd::Command;
use serde_json::json;

static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-gh"));

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home", "bin/gh-data"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::copy(
            &*FIXTURE,
            dir.path()
                .join("bin")
                .join(format!("gh{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn command(&self) -> Command {
        let mut path = vec![self.path("bin")];
        path.extend(std::env::split_paths(
            &std::env::var_os("PATH").unwrap_or_default(),
        ));
        let mut command = Command::cargo_bin("azazel").unwrap();
        command
            .env("AZAZEL_HOME", self.path("home"))
            .env("PATH", std::env::join_paths(path).unwrap())
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("OPENAI_API_KEY")
            .arg("--cwd")
            .arg(self.path("project"));
        command
    }

    /// The text of the first request the scripted lead got for `prompt`.
    fn request_for(&self, prompt: &str) -> String {
        let script = json!({ "lead": [{ "reply": { "text": "ok" } }] });
        std::fs::write(self.path("script.json"), script.to_string()).unwrap();
        let output = self
            .command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .env("AZAZEL_MOCK_REQUESTS", self.path("requests.jsonl"))
            .args(["--model", "mock/lead", "--mode", "auto", "--no-memory"])
            .args(["--set", "session.auto_name=false"])
            .args(["-p", prompt])
            .write_stdin("")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        std::fs::read_to_string(self.path("requests.jsonl")).unwrap()
    }
}

#[test]
fn a_number_in_a_prompt_attaches_the_issue_or_pull_request_only_when_it_resolves() {
    let sandbox = Sandbox::new();
    let data = sandbox.path("bin/gh-data");
    std::fs::write(
        data.join("issue-7.json"),
        r#"{"number":7,"title":"Crash on start","state":"OPEN","author":{"login":"ann"},"labels":[],
        "body":"It crashes.","createdAt":"2026-11-02T10:00:00Z","comments":[]}"#,
    )
    .unwrap();
    std::fs::write(
        data.join("pr-12.json"),
        r#"{"number":12,"title":"Fix start","state":"OPEN","author":{"login":"bob"},"labels":[],
        "body":"Fixes it.","createdAt":"2026-11-03T10:00:00Z","headRefName":"fix","baseRefName":"main",
        "statusCheckRollup":[],"files":[],"comments":[]}"#,
    )
    .unwrap();
    let request = sandbox.request_for("look at #7 and #12, not #99 or C#1x");
    assert!(
        request.contains("issue://7") && request.contains("Crash on start"),
        "{request}"
    );
    assert!(
        request.contains("pr://12") && request.contains("Fix start"),
        "{request}"
    );
    assert!(
        !request.contains("://99") && !request.contains("://1x"),
        "{request}"
    );
    assert!(request.contains("not #99 or C#1x"), "the prompt stays as written");
}

mod pull_requests {
    use std::path::{Path, PathBuf};
    use std::process::Command as Process;

    use azazel_cli::github::{self, Plan};
    use tokio::runtime::Runtime;

    use super::Sandbox;

    fn git(dir: &Path, args: &[&str]) {
        let output = Process::new("git")
            .args([
                "-c",
                "user.name=T",
                "-c",
                "user.email=t@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    /// A clone of a bare `origin` on `main`, with `gh` saying `main` is the default branch.
    fn repo(sandbox: &Sandbox) -> (PathBuf, PathBuf) {
        let origin = sandbox.path("origin.git");
        let work = sandbox.path("work");
        std::fs::create_dir_all(&work).unwrap();
        git(
            &sandbox.path(""),
            &["init", "-q", "--bare", "-b", "main", origin.to_str().unwrap()],
        );
        git(&work, &["init", "-q", "-b", "main"]);
        git(&work, &["remote", "add", "origin", origin.to_str().unwrap()]);
        std::fs::write(work.join("a.txt"), "a\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "first"]);
        git(&work, &["push", "-q", "origin", "main"]);
        let data = sandbox.path("bin/gh-data");
        std::fs::write(data.join("repo.json"), r#"{"defaultBranchRef":{"name":"main"}}"#).unwrap();
        std::fs::write(data.join("signed-in"), "").unwrap();
        (
            work,
            sandbox
                .path("bin")
                .join(format!("gh{}", std::env::consts::EXE_SUFFIX)),
        )
    }

    fn plan(gh: &Path, cwd: &Path) -> Result<Plan, String> {
        Runtime::new().unwrap().block_on(github::plan(gh, cwd))
    }

    #[test]
    fn a_pull_request_is_refused_on_the_default_branch_and_with_uncommitted_changes() {
        let sandbox = Sandbox::new();
        let (work, gh) = repo(&sandbox);
        let on_main = plan(&gh, &work).unwrap_err();
        assert!(on_main.contains("default branch (main)"), "{on_main}");

        git(&work, &["switch", "-q", "-c", "topic"]);
        std::fs::write(work.join("b.txt"), "b\n").unwrap();
        let dirty = plan(&gh, &work).unwrap_err();
        assert!(dirty.contains("uncommitted changes"), "{dirty}");

        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "add b"]);
        let ready = plan(&gh, &work).unwrap();
        assert_eq!((ready.branch.as_str(), ready.base.as_str()), ("topic", "main"));
        assert!(ready.log.contains("add b") && ready.diff.contains("b.txt"));
    }

    #[test]
    fn the_pull_request_is_opened_with_the_edited_title_and_pushes_the_branch() {
        let sandbox = Sandbox::new();
        let (work, gh) = repo(&sandbox);
        git(&work, &["switch", "-q", "-c", "topic"]);
        std::fs::write(work.join("b.txt"), "b\n").unwrap();
        git(&work, &["add", "."]);
        git(&work, &["commit", "-q", "-m", "add b"]);
        let ready = plan(&gh, &work).unwrap();
        // What the user made of the draft in the form is what is sent.
        let (title, body) = github::split_draft("Add b, edited\n\nBecause b.");
        let link = Runtime::new()
            .unwrap()
            .block_on(github::create_pull_request(&gh, &ready, &title, &body))
            .unwrap();
        assert_eq!(link, "https://github.com/tester/repo/pull/9");
        let made = std::fs::read_to_string(sandbox.path("bin/gh-data/pr.md")).unwrap();
        assert_eq!(made, "Add b, edited\n\nBecause b.");
        let pushed = Process::new("git")
            .args(["branch", "--list", "topic"])
            .current_dir(sandbox.path("origin.git"))
            .output()
            .unwrap();
        assert!(String::from_utf8_lossy(&pushed.stdout).contains("topic"));
    }

    #[test]
    fn a_review_is_posted_as_a_comment_and_never_an_approval() {
        let sandbox = Sandbox::new();
        let (work, gh) = repo(&sandbox);
        let data = sandbox.path("bin/gh-data");
        std::fs::write(data.join("pr-current.json"), r#"{"number":12}"#).unwrap();
        std::fs::write(
            data.join("pr-12.json"),
            r#"{"number":12,"title":"T","state":"OPEN","author":{"login":"a"},"labels":[],"body":"b",
            "createdAt":"2026-11-03T10:00:00Z","headRefName":"x","baseRefName":"main",
            "statusCheckRollup":[],"files":[],"comments":[]}"#,
        )
        .unwrap();
        std::fs::write(data.join("pr-12.diff"), "diff --git a/x b/x\n").unwrap();
        let runtime = Runtime::new().unwrap();
        assert_eq!(
            runtime
                .block_on(github::current_pull_request(&gh, &work))
                .unwrap(),
            12
        );
        let (view, diff) = runtime.block_on(github::review_material(&gh, &work, 12)).unwrap();
        assert!(view.contains("#12 T") && diff.starts_with("diff --git"));
        // Nothing is posted until the caller posts.
        assert!(!data.join("review-12.md").exists());
        runtime
            .block_on(github::post_review(&gh, &work, 12, "[P1] a.rs:3 - bug"))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(data.join("review-12.md")).unwrap(),
            "[P1] a.rs:3 - bug"
        );
        let calls = std::fs::read_to_string(data.join("calls.log")).unwrap();
        assert!(calls.contains("pr\treview\t12\t--comment") && !calls.contains("--approve"));
    }
}
