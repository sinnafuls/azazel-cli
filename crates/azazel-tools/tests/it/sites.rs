//! GitHub and YouTube addresses in `web_fetch` and `read`, against a local server that answers
//! as GitHub's REST API and YouTube's player do, a local repository to clone, and a stand-in
//! for the `gh` program.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_core::ArtifactStore;
use azazel_tools::web::{Fetcher, GitHub, Sites, UrlScheme, WebFetchTool, WebGrepTool, YouTube};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{body_partial_json, header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

static FAKE_GH: LazyLock<PathBuf> = LazyLock::new(|| crate::common::fixture_program("fake-gh"));

struct Bench {
    dir: tempfile::TempDir,
    services: Arc<Services>,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let services = Arc::new(Services::new());
    services.insert(Arc::new(ArtifactStore::new(dir.path().join("artifacts"))));
    Bench { dir, services }
}

impl Bench {
    fn ctx(&self) -> ToolCtx {
        let info = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: "c1".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        }
    }

    fn artifacts(&self) -> PathBuf {
        self.dir.path().join("artifacts")
    }

    /// A copy of the `gh` stand-in that is signed in or not, and the directory it reads its
    /// answers from.
    fn gh(&self, signed_in: bool) -> (PathBuf, PathBuf) {
        let home = self.dir.path().join("gh");
        let data = home.join("gh-data");
        std::fs::create_dir_all(data.join("api")).unwrap();
        let program = home.join(FAKE_GH.file_name().unwrap());
        std::fs::copy(&*FAKE_GH, &program).unwrap();
        if signed_in {
            std::fs::write(data.join("signed-in"), "").unwrap();
        }
        (program, data)
    }

    async fn fetch(&self, fetcher: &Arc<Fetcher>, input: Value) -> ToolOutput {
        WebFetchTool::new(fetcher.clone()).call(input, &self.ctx()).await
    }

    async fn get(&self, fetcher: &Arc<Fetcher>, url: &str) -> String {
        ok(&self.fetch(fetcher, json!({ "url": url })).await)
    }
}

/// A fetcher that takes `server` for GitHub's API and file server and for YouTube, and reaches
/// GitHub with these programs.
fn fetcher(server: &MockServer, gh: Option<PathBuf>, git: Option<PathBuf>, remote: &str) -> Arc<Fetcher> {
    let sites = Sites {
        github: GitHub::new(gh, git).at(&server.uri(), &format!("{}/raw", server.uri()), remote),
        youtube: YouTube::at(&server.uri()),
    };
    Arc::new(Fetcher::with_sites(sites).unwrap())
}

/// A fetcher without `gh` and without `git`.
fn plain(server: &MockServer) -> Arc<Fetcher> {
    fetcher(server, None, None, "http://127.0.0.1:9")
}

fn ok(output: &ToolOutput) -> String {
    let text = output.text_content();
    assert!(!output.is_error, "{text}");
    text
}

fn failed(output: &ToolOutput) -> String {
    let text = output.text_content();
    assert!(output.is_error, "expected an error, got: {text}");
    text
}

async fn api(server: &MockServer, at: &str, body: Value) {
    Mock::given(method("GET"))
        .and(path(at))
        .and(header("x-github-api-version", "2022-11-28"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(server)
        .await;
}

fn user(login: &str) -> Value {
    json!({ "login": login })
}

#[tokio::test]
async fn a_pull_request_address_returns_the_conversation_and_its_diff_address_the_changes() {
    let server = MockServer::start().await;
    let pull = "/repos/acme/widgets/pulls/7";
    Mock::given(method("GET"))
        .and(path(pull))
        .and(header("accept", "application/vnd.github.diff"))
        .respond_with(ResponseTemplate::new(200).set_body_string("diff --git a/src/gear.rs b/src/gear.rs\n"))
        .mount(&server)
        .await;
    api(
        &server,
        pull,
        json!({
            "title": "Count the teeth of a gear",
            "state": "closed",
            "merged_at": "2026-03-04T10:00:00Z",
            "closed_at": "2026-03-04T10:00:00Z",
            "created_at": "2026-03-01T09:30:00Z",
            "user": user("ada"),
            "head": {"label": "ada:teeth"},
            "base": {"label": "acme:main"},
            "additions": 12,
            "deletions": 3,
            "changed_files": 2,
            "labels": [{"name": "feature"}, {"name": "gears"}],
            "body": "Gears had no way to say how many teeth they have.\r\n",
        }),
    )
    .await;
    api(
        &server,
        &format!("{pull}/files"),
        json!([
            {"filename": "src/gear.rs", "status": "modified", "additions": 10, "deletions": 3},
            {"filename": "tests/gear.rs", "status": "added", "additions": 2, "deletions": 0},
        ]),
    )
    .await;
    api(
        &server,
        "/repos/acme/widgets/issues/7/comments",
        json!([{"user": user("grace"), "created_at": "2026-03-02T08:00:00Z", "body": "Does it count half teeth?"}]),
    )
    .await;
    api(
        &server,
        &format!("{pull}/reviews"),
        json!([
            {"user": user("grace"), "state": "COMMENTED", "submitted_at": "2026-03-02T08:05:00Z", "body": ""},
            {"user": user("linus"), "state": "CHANGES_REQUESTED", "submitted_at": "2026-03-02T09:00:00Z",
             "body": "Please name the unit."},
            {"user": user("linus"), "state": "APPROVED", "submitted_at": "2026-03-03T09:00:00Z", "body": ""},
        ]),
    )
    .await;
    api(
        &server,
        &format!("{pull}/comments"),
        json!([{"user": user("grace"), "path": "src/gear.rs", "line": 41, "created_at": "2026-03-02T08:05:00Z",
                "body": "`u8` is too small for a clock."}]),
    )
    .await;
    let bench = bench();
    let fetcher = plain(&server);

    let conversation = bench
        .get(&fetcher, "https://github.com/acme/widgets/pull/7")
        .await;
    assert_eq!(
        conversation,
        "# Count the teeth of a gear (acme/widgets#7)\n\
         merged 2026-03-04 · @ada · ada:teeth into acme:main · +12 -3 in 2 files · opened 2026-03-01\n\
         Labels: feature, gears\n\
         \n\
         Gears had no way to say how many teeth they have.\n\
         \n\
         ## Files (2)\n\
         \n\
         modified src/gear.rs +10 -3\n\
         added tests/gear.rs +2 -0\n\
         \n\
         ## Reviews (2)\n\
         \n\
         ### @linus · changes requested · 2026-03-02\n\
         \n\
         Please name the unit.\n\
         \n\
         ### @linus · approved · 2026-03-03\n\
         \n\
         (no text)\n\
         \n\
         ## Comments on lines (1)\n\
         \n\
         ### @grace · src/gear.rs:41 · 2026-03-02\n\
         \n\
         `u8` is too small for a clock.\n\
         \n\
         ## Comments (1)\n\
         \n\
         ### @grace · 2026-03-02\n\
         \n\
         Does it count half teeth?\n\
         \n\
         The changes themselves: https://github.com/acme/widgets/pull/7.diff"
    );
    for changes in ["pull/7.diff", "pull/7/files"] {
        let diff = bench
            .get(&fetcher, &format!("https://github.com/acme/widgets/{changes}"))
            .await;
        assert_eq!(diff, "diff --git a/src/gear.rs b/src/gear.rs\n", "{changes}");
    }
}

#[tokio::test]
async fn an_issue_address_returns_the_issue_with_its_comments_and_a_missing_one_says_how_to_sign_in() {
    let server = MockServer::start().await;
    api(
        &server,
        "/repos/acme/widgets/issues/3",
        json!({
            "title": "Gears slip under load",
            "state": "closed",
            "state_reason": "not_planned",
            "closed_at": "2026-02-10T00:00:00Z",
            "created_at": "2026-02-01T00:00:00Z",
            "user": user("grace"),
            "labels": [{"name": "bug"}],
            "comments": 140,
            "body": "Steps:\n1. Load the gear.\n2. Watch it slip.",
        }),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/3/comments"))
        .and(query_param("per_page", "100"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"user": user("ada"), "created_at": "2026-02-02T00:00:00Z", "body": "Which gear?"},
            {"user": null, "created_at": "2026-02-03T00:00:00Z", "body": ""},
        ])))
        .mount(&server)
        .await;
    // GitHub numbers pull requests among the issues: such an "issue" is read as what it is.
    api(
        &server,
        "/repos/acme/widgets/issues/4",
        json!({"title": "A pull request", "pull_request": {"url": "…"}}),
    )
    .await;
    api(
        &server,
        "/repos/acme/widgets/pulls/4",
        json!({"title": "Oil the gears", "state": "open", "draft": true, "created_at": "2026-02-05T00:00:00Z",
               "user": user("ada"), "head": {"label": "ada:oil"}, "base": {"label": "acme:main"},
               "additions": 1, "deletions": 0, "changed_files": 0, "body": null}),
    )
    .await;
    for list in [
        "pulls/4/files",
        "issues/4/comments",
        "pulls/4/reviews",
        "pulls/4/comments",
    ] {
        api(&server, &format!("/repos/acme/widgets/{list}"), json!([])).await;
    }
    let bench = bench();
    let fetcher = plain(&server);

    assert_eq!(
        bench
            .get(&fetcher, "https://github.com/acme/widgets/issues/3")
            .await,
        "# Gears slip under load (acme/widgets#3)\n\
         closed 2026-02-10 (not planned) · @grace · opened 2026-02-01\n\
         Labels: bug\n\
         \n\
         Steps:\n1. Load the gear.\n2. Watch it slip.\n\
         \n\
         ## Comments (2)\n\
         \n\
         ### @ada · 2026-02-02\n\
         \n\
         Which gear?\n\
         \n\
         ### @ghost · 2026-02-03\n\
         \n\
         (no text)\n\
         \n\
         [the first 2 of 140 comments]"
    );
    assert_eq!(
        bench
            .get(&fetcher, "https://github.com/acme/widgets/issues/4")
            .await,
        "# Oil the gears (acme/widgets#4)\n\
         draft · @ada · ada:oil into acme:main · +1 -0 in 0 files · opened 2026-02-05\n\
         \n\
         (no description)\n\
         \n\
         The changes themselves: https://github.com/acme/widgets/pull/4.diff"
    );
    let missing = bench
        .fetch(
            &fetcher,
            json!({"url": "https://github.com/acme/widgets/issues/99"}),
        )
        .await;
    assert_eq!(
        failed(&missing),
        "GitHub has no issue acme/widgets#99 that can be seen without a login; if it is private, sign in \
         with `gh auth login`"
    );
}

#[tokio::test]
async fn the_limit_on_requests_without_a_login_is_said_in_words() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/3"))
        .respond_with(
            ResponseTemplate::new(403)
                .insert_header("x-ratelimit-remaining", "0")
                .set_body_json(json!({"message": "API rate limit exceeded for 203.0.113.7."})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/issues/5"))
        .respond_with(ResponseTemplate::new(410).set_body_json(json!({"message": "This issue was deleted"})))
        .mount(&server)
        .await;
    let bench = bench();
    let fetcher = plain(&server);
    let limited = bench
        .fetch(
            &fetcher,
            json!({"url": "https://github.com/acme/widgets/issues/3"}),
        )
        .await;
    assert_eq!(
        failed(&limited),
        "GitHub's hourly limit for requests without a login is used up; sign in with `gh auth login` to go on"
    );
    let gone = bench
        .fetch(
            &fetcher,
            json!({"url": "https://github.com/acme/widgets/issues/5"}),
        )
        .await;
    assert_eq!(
        failed(&gone),
        "GitHub answered HTTP 410 Gone: This issue was deleted"
    );
}

fn step(number: u64, name: &str, conclusion: &str, started: &str, completed: &str) -> Value {
    json!({"number": number, "name": name, "conclusion": conclusion, "status": "completed",
           "started_at": started, "completed_at": completed})
}

#[tokio::test]
async fn an_actions_run_address_returns_the_jobs_and_the_log_of_each_failed_step() {
    let server = MockServer::start().await;
    let run = "/repos/acme/widgets/actions/runs/900";
    api(
        &server,
        run,
        json!({"name": "CI", "run_number": 58, "display_title": "Count the teeth of a gear", "status": "completed",
               "conclusion": "failure", "event": "pull_request", "head_branch": "teeth",
               "head_sha": "0123456789abcdef0123456789abcdef01234567", "run_attempt": 2,
               "created_at": "2026-03-01T09:31:00Z"}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path(format!("{run}/jobs")))
        .and(query_param("filter", "latest"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"jobs": [
            {"id": 11, "name": "lint", "status": "completed", "conclusion": "success", "steps": [
                step(1, "Set up job", "success", "2026-03-01T09:31:01Z", "2026-03-01T09:31:02Z"),
            ]},
            {"id": 12, "name": "test (windows)", "status": "completed", "conclusion": "failure", "steps": [
                step(1, "Set up job", "success", "2026-03-01T09:31:01.000Z", "2026-03-01T09:31:02.000Z"),
                step(2, "Run cargo test", "failure", "2026-03-01T09:31:03.000Z", "2026-03-01T09:31:05.000Z"),
                step(3, "Upload", "skipped", "2026-03-01T09:31:06.000Z", "2026-03-01T09:31:06.000Z"),
            ]},
            {"id": 13, "name": "deploy", "status": "queued", "conclusion": null, "steps": []},
        ]})))
        .mount(&server)
        .await;
    // GitHub answers a request for a log with the address the log is at.
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/actions/jobs/12/logs"))
        .respond_with(
            ResponseTemplate::new(302).insert_header("location", format!("{}/blob/12.txt", server.uri())),
        )
        .mount(&server)
        .await;
    let log = "\u{feff}2026-03-01T09:31:01.1000000Z Current runner version: '2.330.0'\n\
               2026-03-01T09:31:03.2000000Z ##[group]Run cargo test\n\
               2026-03-01T09:31:04.0000000Z test gear::counts_teeth ... FAILED\n\
               thread 'gear::counts_teeth' panicked at src/gear.rs:41:9\n\
               2026-03-01T09:31:05.5000000Z ##[error]Process completed with exit code 101.\n\
               2026-03-01T09:31:07.0000000Z Cleaning up orphan processes\n";
    Mock::given(method("GET"))
        .and(path("/blob/12.txt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(log))
        .mount(&server)
        .await;
    let bench = bench();
    let fetcher = plain(&server);

    let saved = bench.artifacts().join("c1-job-12.log");
    let whole = bench
        .get(&fetcher, "https://github.com/acme/widgets/actions/runs/900")
        .await;
    assert_eq!(
        whole,
        format!(
            "# CI #58: Count the teeth of a gear (acme/widgets)\n\
             failure · pull_request on teeth · commit 0123456 · attempt 2 · 2026-03-01T09:31:00Z\n\
             \n\
             ## Jobs (3)\n\
             \n\
             lint: success\n\
             test (windows): failure; step 2 \"Run cargo test\" failed\n\
             deploy: queued\n\
             \n\
             ## Logs of the failed steps\n\
             \n\
             ### test (windows) / Run cargo test\n\
             \n\
             ##[group]Run cargo test\n\
             test gear::counts_teeth ... FAILED\n\
             thread 'gear::counts_teeth' panicked at src/gear.rs:41:9\n\
             ##[error]Process completed with exit code 101.\n\
             [the whole log of this job: {}]",
            saved.display()
        )
    );
    assert_eq!(std::fs::read_to_string(&saved).unwrap(), log);

    // The address of one job is that job alone.
    let one = bench
        .get(
            &fetcher,
            "https://github.com/acme/widgets/actions/runs/900/job/11",
        )
        .await;
    assert!(one.ends_with("## Jobs (1)\n\nlint: success"), "{one}");
}

#[tokio::test]
async fn a_log_github_keeps_for_logins_only_is_a_note_under_the_failed_job() {
    let server = MockServer::start().await;
    let run = "/repos/acme/widgets/actions/runs/901";
    api(
        &server,
        run,
        json!({"name": "CI", "run_number": 59, "display_title": "Oil", "conclusion": "failure"}),
    )
    .await;
    api(
        &server,
        &format!("{run}/jobs"),
        json!({"jobs": [{"id": 21, "name": "test", "conclusion": "failure", "steps": []}]}),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/actions/jobs/21/logs"))
        .respond_with(
            ResponseTemplate::new(403)
                .set_body_json(json!({"message": "Must have admin rights to Repository."})),
        )
        .mount(&server)
        .await;
    let bench = bench();
    let text = bench
        .get(
            &plain(&server),
            "https://github.com/acme/widgets/actions/runs/901",
        )
        .await;
    assert!(
        text.ends_with(
            "## Logs of the failed steps\n\n### test\n\n(the log could not be read: GitHub answered HTTP 403 \
             Forbidden: Must have admin rights to Repository.; GitHub gives logs only to a login: sign in \
             with `gh auth login`)"
        ),
        "{text}"
    );
}

#[tokio::test]
async fn a_file_address_returns_the_file_and_the_lines_it_names_and_a_directory_address_a_listing() {
    let server = MockServer::start().await;
    let file: String = (1..=40).map(|n| format!("line {n}\n")).collect();
    // Without a login the file server is asked, with the branch and the path as the address
    // has them: where the branch name ends is for it to know.
    Mock::given(method("GET"))
        .and(path("/raw/acme/widgets/feature/teeth/src/gear.rs"))
        .respond_with(ResponseTemplate::new(200).set_body_string(file.clone()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/raw/acme/widgets/main/logo.bin"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![0xff, 0xfe, 0x00, 0x01]))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/repos/acme/widgets/contents/src"))
        .and(query_param("ref", "main"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!([
            {"name": "gear.rs", "type": "file", "size": 812},
            {"name": "web", "type": "dir", "size": 0},
            {"name": "bin", "type": "dir", "size": 0},
            {"name": "vendor", "type": "submodule", "size": 0},
        ])))
        .mount(&server)
        .await;
    let bench = bench();
    let fetcher = plain(&server);
    let blob = "https://github.com/acme/widgets/blob/feature/teeth/src/gear.rs";

    assert_eq!(bench.get(&fetcher, blob).await, file);
    assert_eq!(
        bench.get(&fetcher, &format!("{blob}#L3-L5")).await,
        "line 3\nline 4\nline 5\n[lines 3-5 of the file's 40]"
    );
    // `read` takes the same address, and pages the file by line.
    let schemes = Arc::new(azazel_core::Schemes::default());
    schemes.add("web", Arc::new(UrlScheme::https(fetcher.clone())));
    let read = azazel_tools::ReadTool::new(Arc::default(), schemes);
    let part = read
        .call(json!({"path": blob, "offset": 39, "limit": 5}), &bench.ctx())
        .await;
    assert_eq!(ok(&part), "line 39\nline 40\n[lines 39-40 of 40]");
    // `web_grep` searches the file, not GitHub's page around it.
    let found = WebGrepTool::new(fetcher.clone())
        .call(json!({"url": blob, "pattern": "line 17"}), &bench.ctx())
        .await;
    assert_eq!(ok(&found), "17: line 17");

    let binary = bench
        .get(&fetcher, "https://github.com/acme/widgets/blob/main/logo.bin")
        .await;
    let saved = bench.artifacts().join("c1-logo.bin");
    assert_eq!(
        binary,
        format!(
            "https://github.com/acme/widgets/blob/main/logo.bin is binary data, not text (4 bytes). Saved to {}",
            saved.display()
        )
    );
    assert_eq!(std::fs::read(saved).unwrap(), [0xff, 0xfe, 0x00, 0x01]);

    assert_eq!(
        bench
            .get(&fetcher, "https://github.com/acme/widgets/tree/main/src")
            .await,
        "acme/widgets at main: src/\n\nbin/\nweb/\ngear.rs\t812 bytes\nvendor\tsubmodule"
    );
    let missing = bench
        .fetch(
            &fetcher,
            json!({"url": "https://github.com/acme/widgets/blob/main/nope.rs"}),
        )
        .await;
    assert_eq!(
        failed(&missing),
        "GitHub has no file main/nope.rs in acme/widgets that can be seen without a login; if it is private, \
         sign in with `gh auth login`"
    );
}

/// Runs git in `dir` and returns what it printed.
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .current_dir(dir)
        .args(["-c", "user.name=Test", "-c", "user.email=test@example.com"])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// A repository `acme/widgets` with one commit, under a directory that stands in for
/// `https://github.com`. Returns that directory as an address, and the commit.
fn remote(dir: &Path) -> (String, String) {
    let work = dir.join("work");
    std::fs::create_dir_all(work.join("src/web")).unwrap();
    std::fs::write(work.join("README.md"), "# Widgets\n\nGears and other widgets.\n").unwrap();
    std::fs::write(work.join("Cargo.toml"), "[package]\nname = \"widgets\"\n").unwrap();
    std::fs::write(work.join("src/gear.rs"), "pub struct Gear;\n").unwrap();
    std::fs::write(work.join("src/web/fetch.rs"), "pub fn fetch() {}\n").unwrap();
    git(&work, &["init", "--quiet", "--initial-branch=main"]);
    git(&work, &["add", "."]);
    git(&work, &["commit", "--quiet", "--message", "Start"]);
    let commit = git(&work, &["rev-parse", "HEAD"]);
    let hosted = dir.join("remotes/acme");
    std::fs::create_dir_all(&hosted).unwrap();
    git(
        dir,
        &["clone", "--quiet", "--bare", "work", "remotes/acme/widgets.git"],
    );
    let address = reqwest::Url::from_directory_path(dir.join("remotes")).unwrap();
    (address.as_str().trim_end_matches('/').to_string(), commit)
}

fn about(size_kb: u64) -> Value {
    json!({"full_name": "acme/widgets", "description": "Gears and other widgets", "default_branch": "main",
           "stargazers_count": 1234, "language": "Rust", "license": {"spdx_id": "MIT"}, "size": size_kb,
           "homepage": "https://widgets.example"})
}

#[tokio::test]
async fn a_repository_address_clones_it_and_returns_its_tree_its_readme_and_where_it_is() {
    let server = MockServer::start().await;
    api(&server, "/repos/acme/widgets", about(120)).await;
    let bench = bench();
    let (remote, commit) = remote(bench.dir.path());
    let git_program = which::which("git").unwrap();
    let fetcher = fetcher(&server, None, Some(git_program), &remote);

    let clone = bench.artifacts().join("github").join("acme-widgets");
    let expected = format!(
        "# acme/widgets\n\
         Gears and other widgets\n\
         default branch main · 1234 stars · Rust · MIT · https://widgets.example\n\
         \n\
         Cloned to {} (branch main, commit {}), one commit deep. Read and search the files there with `read`, \
         `grep` and `glob`.\n\
         \n\
         ## Files (4)\n\
         \n\
         src/\n  web/\n    fetch.rs\n  gear.rs\nCargo.toml\nREADME.md\n\
         \n\
         ## README.md\n\
         \n\
         # Widgets\n\nGears and other widgets.",
        clone.display(),
        &commit[..7]
    );
    assert_eq!(
        bench.get(&fetcher, "https://github.com/acme/widgets").await,
        expected
    );
    assert_eq!(
        std::fs::read_to_string(clone.join("src/gear.rs")).unwrap().trim(),
        "pub struct Gear;"
    );
    assert!(clone.join(".git").is_dir());
    // Nothing is left beside the clone.
    let beside: Vec<_> = std::fs::read_dir(clone.parent().unwrap()).unwrap().collect();
    assert_eq!(beside.len(), 1);

    // A clone that is there is used as it is: this fetcher has nowhere to clone from.
    let again = self::fetcher(
        &server,
        None,
        Some(which::which("git").unwrap()),
        "http://127.0.0.1:9",
    );
    assert_eq!(
        bench.get(&again, "https://github.com/acme/widgets.git").await,
        expected
    );
}

#[tokio::test]
async fn a_repository_that_is_not_cloned_is_answered_from_what_github_lists() {
    let server = MockServer::start().await;
    api(&server, "/repos/acme/widgets", about(120)).await;
    api(&server, "/repos/acme/huge", about(900 * 1024)).await;
    for repo in ["widgets", "huge"] {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/{repo}/git/trees/main")))
            .and(query_param("recursive", "1"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"truncated": false, "tree": [
                    {"path": "src", "type": "tree"},
                    {"path": "src/gear.rs", "type": "blob"},
                    {"path": "README.md", "type": "blob"},
                ]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/{repo}/readme")))
            .and(header("accept", "application/vnd.github.raw+json"))
            .respond_with(ResponseTemplate::new(200).set_body_string("# Widgets\n"))
            .mount(&server)
            .await;
    }
    let bench = bench();
    let listed = "## Files (2)\n\nsrc/\n  gear.rs\nREADME.md\n\n## README\n\n# Widgets";

    let without_git = bench
        .get(&plain(&server), "https://github.com/acme/widgets")
        .await;
    assert!(
        without_git.ends_with(&format!(
            "\n\nNot cloned: git is not installed. This is what GitHub says is in it.\n\n{listed}"
        )),
        "{without_git}"
    );
    let git_program = which::which("git").unwrap();
    let with_git = fetcher(&server, None, Some(git_program), "http://127.0.0.1:9");
    let too_large = bench.get(&with_git, "https://github.com/acme/huge").await;
    assert!(
        too_large.ends_with(&format!(
            "\n\nNot cloned: it is 900 MB, and repositories over 500 MB are not cloned. This is what GitHub \
             says is in it.\n\n{listed}"
        )),
        "{too_large}"
    );
    assert!(!bench.artifacts().join("github").exists());

    let missing = bench
        .fetch(&with_git, json!({"url": "https://github.com/acme/nothing"}))
        .await;
    assert_eq!(
        failed(&missing),
        "GitHub has no repository acme/nothing that can be seen without a login; if it is private, sign in \
         with `gh auth login`"
    );
}

fn calls(data: &Path) -> Vec<String> {
    let log = std::fs::read_to_string(data.join("calls.log")).unwrap_or_default();
    log.lines().map(|line| line.replace('\t', " ")).collect()
}

#[tokio::test]
async fn with_gh_signed_in_everything_goes_through_it() {
    // Nothing is mounted: a request to the server would be answered with 404.
    let server = MockServer::start().await;
    let bench = bench();
    let (gh, data) = bench.gh(true);
    let api = data.join("api");
    // The branch is `feature/teeth`: GitHub knows no branch `feature`.
    std::fs::write(
        api.join("repos_acme_secret_contents_src_gear.rs_ref_feature_teeth.raw"),
        "pub struct Gear;\n",
    )
    .unwrap();
    std::fs::write(
        api.join("repos_acme_secret_issues_3.json"),
        json!({"title": "Private trouble", "state": "open", "created_at": "2026-02-01T00:00:00Z",
               "user": user("grace"), "body": "Only for members."})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        api.join("repos_acme_secret_issues_3_comments_per_page_100.json"),
        "[]",
    )
    .unwrap();
    std::fs::write(api.join("repos_acme_secret.json"), about(64).to_string()).unwrap();
    let clone = data.join("clone/acme-secret");
    std::fs::create_dir_all(clone.join(".git")).unwrap();
    std::fs::write(clone.join("README"), "Members only.\n").unwrap();
    let fetcher = fetcher(&server, Some(gh), which::which("git").ok(), "http://127.0.0.1:9");

    assert_eq!(
        bench
            .get(
                &fetcher,
                "https://github.com/acme/secret/blob/feature/teeth/src/gear.rs"
            )
            .await,
        "pub struct Gear;\n"
    );
    // The address of the file server is the same file, read with the login.
    assert_eq!(
        bench
            .get(
                &fetcher,
                "https://raw.githubusercontent.com/acme/secret/refs/heads/feature/teeth/src/gear.rs"
            )
            .await,
        "pub struct Gear;\n"
    );
    assert_eq!(
        bench
            .get(&fetcher, "https://github.com/acme/secret/issues/3")
            .await,
        "# Private trouble (acme/secret#3)\nopen · @grace · opened 2026-02-01\n\nOnly for members."
    );
    let missing = bench
        .fetch(
            &fetcher,
            json!({"url": "https://github.com/acme/secret/issues/4"}),
        )
        .await;
    assert_eq!(
        failed(&missing),
        "GitHub has no issue acme/secret#4, or the account `gh` is signed in to cannot see it"
    );
    let repository = bench.get(&fetcher, "https://github.com/acme/secret").await;
    let cloned = bench.artifacts().join("github").join("acme-secret");
    assert!(
        repository.contains(&format!("\n\nCloned to {}", cloned.display()))
            && repository.ends_with("## Files (1)\n\nREADME\n\n## README\n\nMembers only."),
        "{repository}"
    );
    assert_eq!(
        std::fs::read_to_string(cloned.join("README")).unwrap(),
        "Members only.\n"
    );

    let accept = "-H Accept: application/vnd.github";
    let version = "-H X-GitHub-Api-Version: 2022-11-28";
    let contents = "repos/acme/secret/contents";
    let staging = bench.artifacts().join("github").join(".acme-secret-c1");
    assert_eq!(
        calls(&data),
        [
            "auth token --hostname github.com".to_string(),
            format!(
                "api --hostname github.com {accept}.raw+json {version} {contents}/teeth/src/gear.rs?ref=feature"
            ),
            format!(
                "api --hostname github.com {accept}.raw+json {version} {contents}/src/gear.rs?ref=feature/teeth"
            ),
            format!(
                "api --hostname github.com {accept}.raw+json {version} {contents}/teeth/src/gear.rs?ref=feature"
            ),
            format!(
                "api --hostname github.com {accept}.raw+json {version} {contents}/src/gear.rs?ref=feature/teeth"
            ),
            format!("api --hostname github.com {accept}+json {version} repos/acme/secret/issues/3"),
            format!(
                "api --hostname github.com {accept}+json {version} repos/acme/secret/issues/3/comments?per_page=100"
            ),
            format!("api --hostname github.com {accept}+json {version} repos/acme/secret/issues/4"),
            format!("api --hostname github.com {accept}+json {version} repos/acme/secret"),
            format!(
                "repo clone acme/secret {} -- --quiet --depth=1 --single-branch --no-tags --config core.autocrlf=false",
                staging.display()
            ),
        ]
    );
    assert!(server.received_requests().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_log_with_colours_is_asked_of_gh_a_second_time_and_shown_without_them() {
    let server = MockServer::start().await;
    let bench = bench();
    let (gh, data) = bench.gh(true);
    let api = data.join("api");
    std::fs::write(
        api.join("repos_acme_secret_actions_runs_7.json"),
        json!({"name": "CI", "run_number": 3, "display_title": "Oil", "conclusion": "failure", "event": "push",
               "head_branch": "main", "head_sha": "89abcdef0123", "run_attempt": 1,
               "created_at": "2026-03-01T09:31:00Z"})
        .to_string(),
    )
    .unwrap();
    std::fs::write(
        api.join("repos_acme_secret_actions_runs_7_jobs_filter_latest_per_page_100.json"),
        json!({"jobs": [{"id": 70, "name": "test", "conclusion": "failure", "steps": [
            step(4, "Run tests", "failure", "2026-03-01T09:31:03Z", "2026-03-01T09:31:04Z"),
        ]}]})
        .to_string(),
    )
    .unwrap();
    let log = "2026-03-01T09:31:02.0000000Z setting up\n\
               2026-03-01T09:31:03.0000000Z \u{1b}[36;1mcargo test\u{1b}[0m\n\
               2026-03-01T09:31:04.0000000Z \u{1b}[31merror\u{1b}[0m: test failed\n";
    std::fs::write(api.join("repos_acme_secret_actions_jobs_70_logs.json"), log).unwrap();
    let fetcher = fetcher(&server, Some(gh), None, "http://127.0.0.1:9");

    let run = bench
        .get(&fetcher, "https://github.com/acme/secret/actions/runs/7")
        .await;
    let saved = bench.artifacts().join("c1-job-70.log");
    assert_eq!(
        run,
        format!(
            "# CI #3: Oil (acme/secret)\n\
             failure · push on main · commit 89abcde · attempt 1 · 2026-03-01T09:31:00Z\n\
             \n\
             ## Jobs (1)\n\
             \n\
             test: failure; step 4 \"Run tests\" failed\n\
             \n\
             ## Logs of the failed steps\n\
             \n\
             ### test / Run tests\n\
             \n\
             cargo test\n\
             error: test failed\n\
             [the whole log of this job: {}]",
            saved.display()
        )
    );
    // The saved log is the log as GitHub has it.
    assert_eq!(std::fs::read_to_string(saved).unwrap(), log);
    let logs: Vec<String> = calls(&data)
        .into_iter()
        .filter(|call| call.ends_with("/logs"))
        .collect();
    let asked = "api --hostname github.com -H Accept: application/vnd.github+json -H X-GitHub-Api-Version: 2022-11-28";
    assert_eq!(
        logs,
        [
            format!("{asked} repos/acme/secret/actions/jobs/70/logs"),
            format!("{asked} --allow-escape-sequences repos/acme/secret/actions/jobs/70/logs"),
        ]
    );
}

#[tokio::test]
async fn with_gh_installed_but_signed_out_github_is_asked_without_a_login() {
    let server = MockServer::start().await;
    api(
        &server,
        "/repos/acme/widgets/issues/3",
        json!({"title": "Gears slip", "state": "open", "created_at": "2026-02-01T00:00:00Z", "user": user("grace"),
               "body": "They slip."}),
    )
    .await;
    api(&server, "/repos/acme/widgets/issues/3/comments", json!([])).await;
    let bench = bench();
    let (gh, data) = bench.gh(false);
    let fetcher = fetcher(&server, Some(gh), None, "http://127.0.0.1:9");
    assert_eq!(
        bench
            .get(&fetcher, "https://github.com/acme/widgets/issues/3")
            .await,
        "# Gears slip (acme/widgets#3)\nopen · @grace · opened 2026-02-01\n\nThey slip."
    );
    // Whether there is a login is asked once, and nothing else is asked of the program.
    assert_eq!(calls(&data), ["auth token --hostname github.com"]);
}

fn player(server: &MockServer, tracks: Value) -> Value {
    let tracks: Vec<Value> = tracks
        .as_array()
        .unwrap()
        .iter()
        .map(|track| {
            let mut track = track.clone();
            let file = track["baseUrl"].as_str().unwrap().to_string();
            track["baseUrl"] = json!(format!("{}{file}", server.uri()));
            track
        })
        .collect();
    json!({
        "playabilityStatus": {"status": "OK"},
        "videoDetails": {"title": "Me at the zoo", "author": "jawed", "lengthSeconds": "3725"},
        "captions": {"playerCaptionsTracklistRenderer": {"captionTracks": tracks}},
    })
}

#[tokio::test]
async fn a_video_address_returns_the_captions_with_the_time_each_part_is_said_at() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .and(body_partial_json(json!({"videoId": "jNQXAC9IVRw"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(player(
            &server,
            json!([
                {"baseUrl": "/api/timedtext?v=jNQXAC9IVRw&lang=en&kind=asr", "languageCode": "en", "kind": "asr",
                 "name": {"runs": [{"text": "English (auto-generated)"}]}},
                {"baseUrl": "/api/timedtext?v=jNQXAC9IVRw&lang=en", "languageCode": "en",
                 "name": {"runs": [{"text": "English"}]}},
                {"baseUrl": "/api/timedtext?v=jNQXAC9IVRw&lang=de", "languageCode": "de",
                 "name": {"runs": [{"text": "German"}]}},
            ]),
        )))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/api/timedtext"))
        .and(query_param("lang", "en"))
        .and(wiremock::matchers::query_param_is_missing("kind"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            "<?xml version=\"1.0\" encoding=\"utf-8\" ?><timedtext format=\"3\">\n<body>\n\
             <p t=\"1200\" d=\"2160\">All right, so here we are, in front of the\nelephants</p>\n\
             <p t=\"5318\" d=\"2656\">the cool thing about these guys is that they\nhave really...</p>\n\
             <p t=\"36000\" d=\"1751\">and that&#39;s cool</p>\n\
             <p t=\"3723000\" d=\"1000\">bye</p>\n</body>\n</timedtext>",
            "text/xml; charset=UTF-8",
        ))
        .mount(&server)
        .await;
    let bench = bench();
    let fetcher = plain(&server);
    let expected = "# Me at the zoo\n\
                    jawed · 1:02:05 · https://www.youtube.com/watch?v=jNQXAC9IVRw\n\
                    Captions: English (not shown: English (auto-generated), German)\n\
                    \n\
                    [0:01] All right, so here we are, in front of the elephants the cool thing about these guys \
                    is that they have really...\n\
                    [0:36] and that's cool\n\
                    [1:02:03] bye";
    assert_eq!(
        bench
            .get(&fetcher, "https://www.youtube.com/watch?v=jNQXAC9IVRw&t=4s")
            .await,
        expected
    );
    // The short address is the same video, and `read` takes it too.
    let schemes = Arc::new(azazel_core::Schemes::default());
    schemes.add("web", Arc::new(UrlScheme::https(fetcher.clone())));
    let read = azazel_tools::ReadTool::new(Arc::default(), schemes);
    let short = read
        .call(json!({"path": "https://youtu.be/jNQXAC9IVRw"}), &bench.ctx())
        .await;
    assert_eq!(ok(&short), expected);
}

#[tokio::test]
async fn a_video_without_captions_or_that_cannot_be_played_says_so() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .and(body_partial_json(json!({"videoId": "aaaaaaaaaaa"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "playabilityStatus": {"status": "ERROR", "reason": "This video is unavailable"},
        })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/youtubei/v1/player"))
        .and(body_partial_json(json!({"videoId": "bbbbbbbbbbb"})))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "playabilityStatus": {"status": "OK"},
            "videoDetails": {"title": "Silent film"},
        })))
        .mount(&server)
        .await;
    let bench = bench();
    let fetcher = plain(&server);
    let gone = bench
        .fetch(&fetcher, json!({"url": "https://youtu.be/aaaaaaaaaaa"}))
        .await;
    assert_eq!(
        failed(&gone),
        "https://www.youtube.com/watch?v=aaaaaaaaaaa has no transcript to read: This video is unavailable"
    );
    let silent = bench
        .fetch(
            &fetcher,
            json!({"url": "https://www.youtube.com/shorts/bbbbbbbbbbb"}),
        )
        .await;
    assert_eq!(
        failed(&silent),
        "https://www.youtube.com/watch?v=bbbbbbbbbbb has no captions, so there is no transcript"
    );
}
