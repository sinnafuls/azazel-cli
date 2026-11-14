#![allow(clippy::unwrap_used)]

//! Checks on write with a stand-in checker: what a `write` or `edit` is told, what the lead is
//! told when it stops, and the secret scan.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, UpdateSink};
use azazel_core::config::CheckReport;
use azazel_core::process::Shell;
use azazel_core::{Event, ExtCtx, Outcome, Registry};
use azazel_tools::CoreToolsExt;
use azazel_tools::check::ChecksExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The `fake-check` fixture program, built on first use into the directory this test runs from.
static FIXTURE: LazyLock<PathBuf> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args(["build", "--quiet", "--package", "fake-check", "--target-dir"]);
    build.arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(
        build.status().unwrap().success(),
        "could not build the fake-check fixture"
    );
    let binary = profile_dir.join(format!("fake-check{}", std::env::consts::EXE_SUFFIX));
    assert!(binary.is_file(), "{} is missing", binary.display());
    binary
});

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

struct Setup {
    on_write: bool,
    report: CheckReport,
    /// How long a write waits for its checker.
    timeout: Duration,
    /// How long a checker may run at all.
    limit: Duration,
    secrets: bool,
}

impl Default for Setup {
    fn default() -> Self {
        Setup {
            on_write: true,
            report: CheckReport::Delta,
            timeout: Duration::from_secs(30),
            limit: Duration::from_secs(30),
            secrets: false,
        }
    }
}

/// A project where the fixture checks four kinds of file:
///
/// - `.chk`: prints what `findings.txt` holds, and exits with 1 when that is anything;
/// - `.slow`: takes five seconds (long enough for the writes behind it on a loaded machine), then prints what `findings.txt` holds and adds a line
///   to `finished.log`;
/// - `.stuck`: takes four seconds, then adds a line to `finished.log`;
/// - `.bad`: fails without a finding;
/// - `.gone`: a program that is not installed.
///
/// Every run of the first two adds a line to `runs.log`.
fn bench(setup: Setup) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    let registry = Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: root.clone(),
        session_id: "s".into(),
    });
    registry
        .load(&CoreToolsExt {
            shell: Shell::resolve(""),
            shell_timeout: Duration::from_secs(30),
            artifacts_dir: root.join(".artifacts"),
            read_guard: Default::default(),
            patch: Default::default(),
        })
        .unwrap();
    let fixture = format!("\"{}\"", FIXTURE.display());
    let in_root = |name: &str| format!("\"{}\"", root.join(name).display());
    let (findings, log) = (in_root("findings.txt"), in_root("runs.log"));
    let configured = BTreeMap::from([
        (
            "chk".to_string(),
            format!("{fixture} --log {log} --from {findings}"),
        ),
        (
            "slow".to_string(),
            format!(
                "{fixture} --log {log} --sleep-ms 5000 --from {findings} --log {}",
                in_root("finished.log")
            ),
        ),
        (
            "stuck".to_string(),
            format!("{fixture} --sleep-ms 4000 --log {}", in_root("finished.log")),
        ),
        (
            "bad".to_string(),
            format!("{fixture} --err \"  \" --err \"the config is broken\" --exit 2"),
        ),
        ("gone".to_string(), "azazel-no-such-checker --strict".to_string()),
    ]);
    registry
        .load(&ChecksExt {
            root,
            on_write: setup.on_write,
            configured,
            trusted: true,
            report: setup.report,
            timeout: setup.timeout,
            limit: setup.limit,
            gitleaks: setup.secrets.then(|| FIXTURE.clone()),
        })
        .unwrap();
    Bench { dir, registry }
}

fn agent(id: &str, depth: u32) -> AgentInfo {
    AgentInfo {
        id: id.into(),
        def: id.into(),
        parent: (depth > 0).then(|| "lead".to_string()),
        depth,
        session_id: "s".into(),
    }
}

impl Bench {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    /// What the checker of `.chk` files finds from now on.
    fn findings(&self, lines: &[&str]) {
        std::fs::write(self.path("findings.txt"), lines.join("\n") + "\n").unwrap();
    }

    fn runs(&self) -> usize {
        std::fs::read_to_string(self.path("runs.log"))
            .map(|log| log.lines().count())
            .unwrap_or(0)
    }

    /// The text of a successful call.
    async fn ok(&self, tool: &str, input: Value) -> String {
        let info = agent("lead", 0);
        let ctx = ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: "c1".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        };
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, tool).unwrap();
        let output = tool.call(input, &ctx).await;
        assert!(!output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn write(&self, path: &str, content: &str) -> String {
        self.ok("write", json!({ "path": path, "content": content }))
            .await
    }

    async fn prompt(&self) {
        let mut event = Event::UserPrompt {
            text: "go".into(),
            images: Vec::new(),
        };
        self.registry.bus.emit(&mut event).await;
    }

    /// What `who` is told when it stops, if anything.
    async fn stop(&self, who: AgentInfo) -> Option<String> {
        let mut event = Event::Stop {
            agent: who,
            stop_hook_active: false,
        };
        match self.registry.bus.emit(&mut event).await {
            Outcome::Block { reason } => Some(reason),
            Outcome::Continue => None,
        }
    }

    async fn lead_stops(&self) -> Option<String> {
        self.stop(agent("lead", 0)).await
    }
}

#[tokio::test]
async fn errors_join_the_result_of_the_write_and_the_lead_is_told_everything_once_when_it_stops() {
    let bench = bench(Setup::default());
    bench.prompt().await;
    bench.findings(&[
        "a.chk:2:3: warning: second is odd",
        &format!("{}:1:1: error: first is wrong", bench.path("a.chk").display()),
        "a.chk:1:1: error: first is wrong",
        "other.chk:1:1: error: about another file",
        "summary: 3 problems",
    ]);
    let written = bench.write("a.chk", "one\ntwo\n").await;
    assert_eq!(
        written,
        "created a.chk (8 bytes)\nchecks:\na.chk:1:1 error first is wrong [fake-check]"
    );
    // A file with only a warning: nothing in the result, the warning waits for the stop.
    bench.findings(&["sub/b.chk:1:1: warning: b is odd"]);
    assert_eq!(
        bench.write("sub/b.chk", "b\n").await,
        "created sub/b.chk (2 bytes)"
    );
    // A kind of file nothing checks.
    assert_eq!(bench.write("notes.md", "n\n").await, "created notes.md (2 bytes)");
    assert_eq!(bench.runs(), 2);

    // The report is the lead's; an agent it started is told nothing and uses nothing up.
    assert_eq!(bench.stop(agent("coder-1", 1)).await, None);
    let report = bench.lead_stops().await.unwrap();
    assert_eq!(
        report,
        "<checks>\n\
         The checkers found 1 error, 2 warnings in 2 files written in this turn:\n\
         a.chk: 1 error, 1 warning\n\
         sub/b.chk: 1 warning\n\
         On the lines changed in this turn:\n\
         a.chk:1:1 error first is wrong [fake-check]\n\
         a.chk:2:3 warning second is odd [fake-check]\n\
         sub/b.chk:1:1 warning b is odd [fake-check]\n\
         </checks>\n\
         This is an automatic report, not a message from the user. Fix what your changes caused, say in \
         a line what you leave and why, then finish."
    );
    // Once per turn, whatever is written after it.
    bench.findings(&["a.chk:1:1: error: still wrong"]);
    bench
        .ok(
            "edit",
            json!({ "path": "a.chk", "edits": [{ "old": "one", "new": "uno" }] }),
        )
        .await;
    assert_eq!(bench.lead_stops().await, None);

    // A new prompt is a new turn: what the last one found is forgotten, a file written again is
    // checked again, and a fixed file is not reported.
    bench.prompt().await;
    assert_eq!(bench.lead_stops().await, None);
    bench.findings(&[]);
    assert_eq!(
        bench
            .ok(
                "edit",
                json!({ "path": "a.chk", "edits": [{ "old": "uno", "new": "one" }] })
            )
            .await,
        "edited a.chk (1 replacement)"
    );
    assert_eq!(bench.lead_stops().await, None);
}

#[tokio::test]
async fn only_findings_on_changed_lines_are_listed_unless_all_are_asked_for() {
    for report in [CheckReport::Delta, CheckReport::All, CheckReport::Off] {
        let bench = bench(Setup {
            report,
            ..Setup::default()
        });
        std::fs::write(bench.path("b.chk"), "a\nb\nc\nd\ne\n").unwrap();
        bench.prompt().await;
        bench.ok("read", json!({ "path": "b.chk" })).await;
        bench.findings(&[
            "b.chk:1:1: warning: a was always odd",
            "b.chk:4:1: warning: D is new and odd",
        ]);
        bench
            .ok(
                "edit",
                json!({ "path": "b.chk", "edits": [{ "old": "d", "new": "D" }] }),
            )
            .await;
        // Two lines go in at the top; the line changed before is now the sixth.
        bench.findings(&[
            "b.chk:3:1: warning: a was always odd",
            "b.chk:6:1: warning: D is new and odd",
            "b.chk:7:1: note: e could be better",
        ]);
        bench
            .ok(
                "edit",
                json!({ "path": "b.chk", "edits": [{ "old": "a\nb", "new": "x\ny\na\nb" }] }),
            )
            .await;
        let told = bench.lead_stops().await;
        match report {
            CheckReport::Delta => {
                let told = told.unwrap();
                assert!(
                    told.contains(
                        "The checkers found 2 warnings, 1 note in 1 file written in this turn:\n\
                         b.chk: 2 warnings, 1 note\n\
                         On the lines changed in this turn:\n\
                         b.chk:6:1 warning D is new and odd [fake-check]\n\
                         </checks>"
                    ),
                    "{told}"
                );
            }
            CheckReport::All => {
                let told = told.unwrap();
                assert!(
                    told.contains(
                        "All of them:\n\
                         b.chk:3:1 warning a was always odd [fake-check]\n\
                         b.chk:6:1 warning D is new and odd [fake-check]\n\
                         b.chk:7:1 note e could be better [fake-check]\n\
                         </checks>"
                    ),
                    "{told}"
                );
            }
            CheckReport::Off => assert_eq!(told, None),
        }
    }
}

#[tokio::test]
async fn findings_on_lines_the_turn_left_alone_are_not_reported_but_those_beside_a_removal_are() {
    let bench = bench(Setup::default());
    std::fs::write(bench.path("b.chk"), "a\nb\nc\nd\ne\n").unwrap();
    bench.prompt().await;
    bench.ok("read", json!({ "path": "b.chk" })).await;
    bench.findings(&["b.chk:1:1: warning: a was always odd"]);
    bench
        .ok(
            "edit",
            json!({ "path": "b.chk", "edits": [{ "old": "e", "new": "E" }] }),
        )
        .await;
    assert_eq!(bench.lead_stops().await, None);

    // A removed line changes nothing that is left, but what it breaks shows where it was.
    bench.prompt().await;
    bench.findings(&[
        "b.chk:1:1: warning: a was always odd",
        "b.chk:3:1: error: d has lost its c",
    ]);
    bench
        .ok(
            "edit",
            json!({ "path": "b.chk", "edits": [{ "old": "b\nc\n", "new": "b\n" }] }),
        )
        .await;
    let told = bench.lead_stops().await.unwrap();
    assert!(
        told.contains(
            "On the lines changed in this turn:\n\
             b.chk:3:1 error d has lost its c [fake-check]\n\
             </checks>"
        ),
        "{told}"
    );
}

#[tokio::test]
async fn a_checker_that_outlasts_the_write_goes_on_and_what_it_finds_is_in_the_report() {
    let bench = bench(Setup {
        timeout: Duration::from_millis(300),
        ..Setup::default()
    });
    bench.prompt().await;
    bench.findings(&[
        "a.slow:1:1: error: a is wrong, found late",
        "b.slow:1:1: warning: b is odd, found later",
    ]);
    assert_eq!(
        bench.write("a.slow", "a\n").await,
        "created a.slow (2 bytes); fake-check is still running; what it finds joins the report when you stop"
    );
    // One run of a checker in one place at a time: what is written meanwhile waits behind it,
    // and the write does not.
    let behind =
        "fake-check is still checking an earlier write; what it finds here joins the report when you stop";
    assert_eq!(
        bench.write("b.slow", "b\n").await,
        format!("created b.slow (2 bytes); {behind}")
    );
    for (old, new) in [("b", "B"), ("B", "b2")] {
        let edited = bench
            .ok(
                "edit",
                json!({ "path": "b.slow", "edits": [{ "old": old, "new": new }] }),
            )
            .await;
        assert_eq!(edited, format!("edited b.slow (1 replacement); {behind}"));
    }
    // The checker notes its start when it runs, which on a busy machine is a moment after the
    // write returned; there is never a second run beside it.
    let started = std::time::Instant::now();
    while bench.runs() == 0 && started.elapsed() < Duration::from_secs(10) {
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert_eq!(bench.runs(), 1);
    assert!(!bench.path("finished.log").exists());

    // The lead's stop waits for both runs: the one that was under way, and one for the file
    // written three times behind it.
    let report = bench.lead_stops().await.unwrap();
    assert!(
        report.contains(
            "The checkers found 1 error, 1 warning in 2 files written in this turn:\n\
             a.slow: 1 error\n\
             b.slow: 1 warning\n\
             On the lines changed in this turn:\n\
             a.slow:1:1 error a is wrong, found late [fake-check]\n\
             b.slow:1:1 warning b is odd, found later [fake-check]\n\
             </checks>"
        ),
        "{report}"
    );
    assert_eq!(bench.runs(), 2);
}

#[tokio::test]
async fn a_checker_is_ended_at_its_limit_and_by_the_next_prompt() {
    let bench = bench(Setup {
        timeout: Duration::from_millis(200),
        limit: Duration::from_millis(1500),
        ..Setup::default()
    });
    bench.prompt().await;
    let running = "fake-check is still running; what it finds joins the report when you stop";
    assert_eq!(
        bench.write("a.stuck", "a\n").await,
        format!("created a.stuck (2 bytes); {running}")
    );
    // A new prompt lets go of what the last turn left running, and has nothing to wait for.
    bench.prompt().await;
    assert_eq!(bench.lead_stops().await, None);

    assert_eq!(
        bench.write("b.stuck", "b\n").await,
        format!("created b.stuck (2 bytes); {running}")
    );
    bench.findings(&["c.chk:1:1: warning: c is odd"]);
    bench.write("c.chk", "c\n").await;
    // The stop waits for the checker no longer than the checker may run.
    let report = bench.lead_stops().await.unwrap();
    assert!(
        report.contains(
            "1 file was not checked, so nothing found there does not mean it is clean: \
             b.stuck (fake-check did not finish within 1500 ms)\n"
        ),
        "{report}"
    );
    // Neither run was left to finish behind the agent's back.
    tokio::time::sleep(Duration::from_millis(4500)).await;
    assert!(!bench.path("finished.log").exists());

    // Without the report nothing would show what a run in the background finds, so a checker
    // that makes the write wait too long is ended there.
    let quiet = bench_with(Setup {
        timeout: Duration::from_millis(200),
        report: CheckReport::Off,
        ..Setup::default()
    });
    assert_eq!(
        quiet.write("a.stuck", "a\n").await,
        "created a.stuck (2 bytes); fake-check did not finish within 200 ms"
    );
}

#[tokio::test]
async fn a_checker_that_fails_or_is_missing_is_a_note_and_never_fails_the_write() {
    let bench = bench(Setup::default());
    bench.prompt().await;
    assert_eq!(
        bench.write("a.bad", "kept\n").await,
        "created a.bad (5 bytes); fake-check failed: the config is broken"
    );
    assert_eq!(
        bench.write("a.gone", "kept\n").await,
        "created a.gone (5 bytes); checker `azazel-no-such-checker` was not found"
    );
    assert_eq!(std::fs::read_to_string(bench.path("a.bad")).unwrap(), "kept\n");
    assert_eq!(std::fs::read_to_string(bench.path("a.gone")).unwrap(), "kept\n");
    // A file over the size limit is left alone.
    let big = "x\n".repeat(600_000);
    assert_eq!(
        bench.write("big.chk", &big).await,
        "created big.chk (1200000 bytes)"
    );
    assert_eq!(bench.runs(), 0);

    // With checks on write off nothing runs at all.
    let off = bench_off();
    off.findings(&["a.chk:1:1: error: never seen"]);
    assert_eq!(off.write("a.chk", "a\n").await, "created a.chk (2 bytes)");
    assert_eq!(off.write("a.gone", "a\n").await, "created a.gone (2 bytes)");
    assert_eq!(off.runs(), 0);
    assert_eq!(off.lead_stops().await, None);
}

/// [`bench`], for a test that has a bench of that name already.
fn bench_with(setup: Setup) -> Bench {
    bench(setup)
}

fn bench_off() -> Bench {
    bench(Setup {
        on_write: false,
        ..Setup::default()
    })
}

#[tokio::test]
async fn secrets_the_scan_finds_reach_the_lead_once_and_never_the_secret_itself() {
    let bench = bench(Setup {
        secrets: true,
        ..Setup::default()
    });
    std::fs::write(
        bench.path(".fake-leaks.json"),
        json!([
            { "RuleID": "generic-api-key", "File": "config/dev.env", "StartLine": 3,
              "Secret": "hunter2-hunter2", "Match": "KEY=hunter2-hunter2" },
            { "RuleID": "aws-access-token", "File": "deploy/keys.txt", "StartLine": 12,
              "Secret": "AKIAIOSFODNN7EXAMPLE" }
        ])
        .to_string(),
    )
    .unwrap();
    // Nothing is scanned before the first prompt.
    assert_eq!(bench.lead_stops().await, None);
    bench.prompt().await;
    let begun = Instant::now();
    let report = loop {
        if let Some(report) = bench.lead_stops().await {
            break report;
        }
        assert!(
            begun.elapsed() < Duration::from_secs(60),
            "the scan never finished"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert!(
        report.starts_with(
            "<checks>\n\
             gitleaks found 2 secrets in the project (file, line and the rule that matched):\n\
             config/dev.env:3 generic-api-key\n\
             deploy/keys.txt:12 aws-access-token\n\
             </checks>\n"
        ),
        "{report}"
    );
    assert!(
        !report.contains("hunter2") && !report.contains("AKIA"),
        "{report}"
    );

    // Told once: not again in this turn, and not in the next, which does not scan again.
    assert_eq!(bench.lead_stops().await, None);
    bench.prompt().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(bench.lead_stops().await, None);
}
