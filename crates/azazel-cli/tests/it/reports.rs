//! What goes to GitHub and what stays: a session shared as a secret gist, and `azazel bug`,
//! which writes its zip on this machine and opens an issue only with the summary. GitHub is
//! `fake-gh`, a stand-in that keeps what it is given.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::PathBuf;
use std::process::Output;
use std::sync::LazyLock;

use crate::common::fixture_program;
use assert_cmd::Command;
use azazel_cli::share::{self, Shared};
use azazel_core::SessionStore;
use serde_json::json;

static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| fixture_program("fake-gh"));

/// A key as a provider gives one out; it must not leave the machine.
const KEY: &str = "sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789";
const PROMPT: &str =
    "why does deploy fail with ANTHROPIC_API_KEY=sk-ant-api03-abcdefghijklmnopqrstuvwxyz0123456789 ?";

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    /// A project, a home with a provider key written into its config and a log, and `gh` in a
    /// directory put first on `PATH`; `signed_in` is whether that `gh` has an account.
    fn new(signed_in: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["project/.git", "home/logs", "bin/gh-data"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        std::fs::copy(
            &*FIXTURE,
            dir.path()
                .join("bin")
                .join(format!("gh{}", std::env::consts::EXE_SUFFIX)),
        )
        .unwrap();
        if signed_in {
            std::fs::write(dir.path().join("bin/gh-data/signed-in"), "").unwrap();
        }
        std::fs::write(
            dir.path().join("home/config.toml"),
            "[providers.corp]\napi = \"openai-chat\"\nbase_url = \"https://llm.corp.example/v1\"\n\
             api_key = \"corp-key-in-plain-text\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("home/logs/azazel.log"),
            format!("WARN request failed, key was {KEY}\n"),
        )
        .unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn gh(&self) -> PathBuf {
        self.path("bin")
            .join(format!("gh{}", std::env::consts::EXE_SUFFIX))
    }

    /// `azazel` in this sandbox, with the stand-in `gh` first on `PATH`.
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

    /// One prompt answered by the scripted lead: the session the tests share and report.
    fn converse(&self) -> SessionStore {
        let script = json!({ "lead": [{ "reply": { "text": "The key is not valid." } }] });
        std::fs::write(self.path("script.json"), script.to_string()).unwrap();
        let output = self
            .command()
            .env("AZAZEL_MOCK_SCRIPT", self.path("script.json"))
            .args(["--model", "mock/lead", "--mode", "auto"])
            .args(["--set", "memory.auto_extract=false"])
            .args(["--set", "memory.auto_index=false"])
            .args(["--set", "session.auto_name=false"])
            .args(["-p", PROMPT])
            .write_stdin("")
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let paths =
            azazel_core::Paths::discover(&self.path("project"), Some(&self.path("home").to_string_lossy()));
        let latest = SessionStore::list(&paths.sessions_dir()).remove(0);
        SessionStore::open(&latest.path).unwrap()
    }

    fn bug(&self, args: &[&str]) -> Output {
        self.command()
            .arg("bug")
            .args(args)
            .write_stdin("")
            .output()
            .unwrap()
    }

    /// The reports written so far, oldest first, each as its files by name.
    fn reports(&self) -> Vec<BTreeMap<String, String>> {
        let mut files: Vec<PathBuf> = std::fs::read_dir(self.path("home/reports"))
            .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
            .unwrap_or_default();
        files.sort();
        files
            .iter()
            .map(|file| {
                let mut archive = zip::ZipArchive::new(std::fs::File::open(file).unwrap()).unwrap();
                let mut out = BTreeMap::new();
                for index in 0..archive.len() {
                    let mut entry = archive.by_index(index).unwrap();
                    let mut text = String::new();
                    entry.read_to_string(&mut text).unwrap();
                    out.insert(entry.name().unwrap().to_string(), text);
                }
                out
            })
            .collect()
    }

    /// What the stand-in `gh` was run with, one call per line.
    fn gh_calls(&self) -> String {
        std::fs::read_to_string(self.path("bin/gh-data/calls.log")).unwrap_or_default()
    }
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_session_is_shared_as_a_secret_gist_without_its_secrets() {
    let sandbox = Sandbox::new(true);
    let session = tokio::task::block_in_place(|| sandbox.converse());
    let shared = Shared::of(&session).unwrap();
    let link = shared.upload(&sandbox.gh()).await.unwrap();
    assert_eq!(
        link,
        "https://gist.github.com/tester/0123456789abcdef0123456789abcdef"
    );

    // One call, and nothing in it that would make the gist public.
    let calls = sandbox.gh_calls();
    assert_eq!(calls.lines().count(), 1, "{calls}");
    assert!(
        calls.starts_with("gist\tcreate\t--desc\t") && !calls.contains("--public"),
        "{calls}"
    );
    let gist = sandbox.path("bin/gh-data/gist");
    let markdown = std::fs::read_to_string(gist.join(format!("azazel-session-{}.md", session.id()))).unwrap();
    let page =
        std::fs::read_to_string(gist.join(format!("azazel-session-{}.view.html", session.id()))).unwrap();
    for file in [&markdown, &page] {
        assert!(file.contains("why does deploy fail") && file.contains("The key is not valid."));
        assert!(!file.contains("abcdefghij"), "the key was uploaded");
    }
    assert_eq!(
        std::fs::read_to_string(gist.join(".description")).unwrap(),
        format!("azazel session {}", session.id())
    );

    // Not signed in: nothing is uploaded and `gh` says what to do.
    let nobody = Sandbox::new(false);
    let why = shared.upload(&nobody.gh()).await.unwrap_err();
    assert!(
        why.starts_with("`gh gist create` failed: ") && why.contains("gh auth login"),
        "{why}"
    );
    assert!(!nobody.path("bin/gh-data/gist").exists());

    // An issue goes to the repository it is told, with the text it is given.
    let issue = share::open_issue(&sandbox.gh(), "owner/project", "It broke", "This is how.\n")
        .await
        .unwrap();
    assert_eq!(issue, "https://github.com/owner/project/issues/7");
    assert_eq!(
        std::fs::read_to_string(sandbox.path("bin/gh-data/issue.md")).unwrap(),
        "It broke\n\nThis is how.\n"
    );
}

#[test]
fn a_bug_report_stays_on_the_machine_and_its_issue_carries_the_summary() {
    let sandbox = Sandbox::new(true);
    sandbox.converse();

    // Not at a terminal and without flags: the report is written, nothing is asked, nothing
    // leaves, and the conversation is not in it.
    let output = sandbox.bug(&["the", "status line flickers"]);
    assert!(output.status.success(), "{output:?}");
    let said = text(&output.stdout);
    assert!(said.starts_with("bug report written to "), "{said}");
    assert!(said.contains("azazel bug --issue"), "{said}");
    assert_eq!(sandbox.gh_calls(), "");
    let reports = sandbox.reports();
    let names: Vec<&str> = reports[0].keys().map(String::as_str).collect();
    assert_eq!(names, ["config.toml", "doctor.txt", "log.txt", "report.md"]);
    let report = &reports[0];
    assert!(report["report.md"].starts_with("# azazel bug report\n\nthe status line flickers\n"));
    assert!(report["report.md"].contains(&format!("- azazel: {}", env!("CARGO_PKG_VERSION"))));
    assert!(report["report.md"].contains(&format!("- system: {}", std::env::consts::OS)));
    assert!(report["doctor.txt"].contains("config: loaded"));
    // The settings without the key in them, the log without the key it printed.
    let config: toml::Table = report["config.toml"].parse().unwrap();
    assert_eq!(config["providers"]["corp"]["api_key"].as_str(), Some("[removed]"));
    assert_eq!(
        config["providers"]["corp"]["base_url"].as_str(),
        Some("https://llm.corp.example/v1")
    );
    assert!(report["log.txt"].starts_with("WARN request failed, key was [redacted:"));
    for (name, file) in report {
        assert!(
            !file.contains("corp-key-in-plain-text") && !file.contains("abcdefghij"),
            "{name}"
        );
    }

    // With the conversation, and an issue: the zip gains the page, the issue gets the summary
    // and none of the files.
    let output = sandbox.bug(&["--transcript", "--issue", "deploy fails"]);
    assert!(output.status.success(), "{output:?}");
    let said = text(&output.stdout);
    assert!(
        said.trim_end()
            .ends_with("issue opened: https://github.com/sinnafuls/azazel-cli/issues/7"),
        "{said}"
    );
    let reports = sandbox.reports();
    assert_eq!(reports.len(), 2);
    let page = &reports[1]["transcript.html"];
    assert!(page.contains("why does deploy fail") && page.contains("The key is not valid."));
    assert!(!page.contains("abcdefghij"));
    let calls = sandbox.gh_calls();
    assert!(
        calls
            .starts_with("issue\tcreate\t--repo\tsinnafuls/azazel-cli\t--title\tdeploy fails\t--body-file\t"),
        "{calls}"
    );
    let issue = std::fs::read_to_string(sandbox.path("bin/gh-data/issue.md")).unwrap();
    assert!(
        issue.starts_with("deploy fails\n\ndeploy fails\n\n- azazel: "),
        "{issue}"
    );
    assert!(issue.contains("`azazel doctor` found"));
    assert!(!issue.contains("why does deploy fail") && !issue.contains("WARN request failed"));
}

#[test]
fn an_issue_that_cannot_be_opened_leaves_the_report_and_says_why() {
    let sandbox = Sandbox::new(false);
    let output = sandbox.bug(&["--issue", "it broke"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(text(&output.stdout).starts_with("bug report written to "));
    let why = text(&output.stderr);
    assert!(
        why.starts_with("azazel: no issue was opened: `gh issue create` failed: ")
            && why.contains("gh auth login"),
        "{why}"
    );
    assert_eq!(sandbox.reports().len(), 1);

    // A session that was asked for and is not there is an error before anything is written.
    let output = sandbox.bug(&["--session", "no-such-session"]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert_eq!(sandbox.reports().len(), 1);
}
