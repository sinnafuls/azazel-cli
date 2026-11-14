#![allow(clippy::unwrap_used)]

//! The done-check and the stuck guard as the event bus drives them, beside the checks on write
//! whose clean run counts as a check.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::Block;
use azazel_core::config::CheckReport;
use azazel_core::process::Shell;
use azazel_core::runaway::Runaway;
use azazel_core::{Event, ExtCtx, Outcome, Registry};
use azazel_tools::CoreToolsExt;
use azazel_tools::check::ChecksExt;
use azazel_tools::guards::GuardsExt;
use serde_json::json;
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

/// A project whose `.chk` files a stand-in checker checks (it finds what `findings.txt`
/// holds), with the guards as `guards` sets them.
fn bench(guards: GuardsExt) -> Bench {
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
    let checker = format!(
        "\"{}\" --from \"{}\"",
        FIXTURE.display(),
        root.join("findings.txt").display()
    );
    registry
        .load(&ChecksExt {
            root,
            on_write: true,
            configured: BTreeMap::from([("chk".to_string(), checker)]),
            trusted: true,
            report: CheckReport::Delta,
            timeout: Duration::from_secs(30),
            limit: Duration::from_secs(30),
            gitleaks: None,
        })
        .unwrap();
    registry.load(&guards).unwrap();
    Bench { dir, registry }
}

fn guards() -> GuardsExt {
    GuardsExt {
        done_check: true,
        stuck_after: 3,
        runaway: true,
        verify: vec!["tools/ok --all".to_string()],
    }
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

    async fn emit(&self, mut event: Event) -> (Event, Outcome) {
        let outcome = self.registry.bus.emit(&mut event).await;
        (event, outcome)
    }

    async fn prompt(&self) {
        self.emit(Event::UserPrompt {
            text: "go".into(),
            images: Vec::new(),
        })
        .await;
    }

    /// The lead writes `name` with the `write` tool, and the bus hears of the change as it
    /// does when the call went through the gate.
    async fn write(&self, name: &str, content: &str) -> String {
        let info = agent("lead", 0);
        let ctx = ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info.clone(),
            call_id: "w".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        };
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, "write").unwrap();
        let output = tool.call(json!({ "path": name, "content": content }), &ctx).await;
        assert!(!output.is_error, "{}", output.text_content());
        self.changed(&info, name).await;
        output.text_content()
    }

    /// `who` changed `name`, as far as the bus is told.
    async fn changed(&self, who: &AgentInfo, name: &str) {
        self.emit(Event::FileChanged {
            agent: who.clone(),
            path: self.path(name),
            tool: "write".into(),
        })
        .await;
    }

    /// `who` runs `command` in the shell and it ends with `code`. Returns the result as the
    /// agent gets it.
    async fn shell(&self, who: &AgentInfo, call_id: &str, command: &str, code: i64) -> String {
        self.emit(Event::ToolCall {
            agent: who.clone(),
            call_id: call_id.into(),
            tool: "shell".into(),
            input: json!({ "command": command }),
            skip_prompt: false,
        })
        .await;
        let output = ToolOutput {
            content: vec![Block::text(format!("out\nexit code: {code}"))],
            is_error: false,
            details: Some(json!({ "exit_code": code, "timed_out": false })),
        };
        let (event, _) = self
            .emit(Event::ToolResult {
                agent: who.clone(),
                call_id: call_id.into(),
                tool: "shell".into(),
                output,
            })
            .await;
        match event {
            Event::ToolResult { output, .. } => output.text_content(),
            other => panic!("{other:?}"),
        }
    }

    /// What `who` is told when it stops, if anything.
    async fn stop(&self, who: AgentInfo) -> Option<String> {
        let event = Event::Stop {
            agent: who,
            stop_hook_active: false,
        };
        match self.emit(event).await.1 {
            Outcome::Block { reason } => Some(reason),
            Outcome::Continue => None,
        }
    }

    async fn lead_stops(&self) -> Option<String> {
        self.stop(agent("lead", 0)).await
    }
}

const DONE_CHECK_TAIL: &str = "</done-check>\nThis is an automatic note, not a message from the user. Prove \
                               the change: run the project's tests, its build or its linter now and say \
                               what they showed. If nothing can check it, say plainly in your answer that \
                               the change is unverified.";

fn done_check(files: &str, named: &str) -> String {
    format!(
        "<done-check>\n{files} changed in this turn and nothing has run since that shows the change \
         works: {named}\n{DONE_CHECK_TAIL}"
    )
}

#[tokio::test]
async fn the_lead_is_sent_back_once_when_it_stops_on_changes_nothing_has_checked() {
    let bench = bench(guards());
    let lead = agent("lead", 0);
    bench.prompt().await;
    // Nothing changed: nothing to prove.
    assert_eq!(bench.lead_stops().await, None);

    bench.write("src/a.rs", "fn a() {}\n").await;
    bench.write("src/a.rs", "fn a() { }\n").await;
    bench.changed(&agent("coder-1", 1), "src/b.rs").await;
    // Prose is not something to build or test.
    bench.write("README.md", "# readme\n").await;
    // Looking around is not a check, and a check that an agent it started stops on is not the
    // lead's stop.
    bench.shell(&lead, "c1", "git status", 0).await;
    assert_eq!(bench.stop(agent("coder-1", 1)).await, None);
    assert_eq!(
        bench.lead_stops().await,
        Some(done_check("2 files", "src/a.rs, src/b.rs"))
    );
    // Once per prompt, whatever follows.
    bench.write("src/c.rs", "fn c() {}\n").await;
    assert_eq!(bench.lead_stops().await, None);

    // The next prompt starts over, and many files are named only in part.
    bench.prompt().await;
    assert_eq!(bench.lead_stops().await, None);
    for n in 0..7 {
        bench.changed(&lead, &format!("m{n}.py")).await;
    }
    assert_eq!(
        bench.lead_stops().await,
        Some(done_check(
            "7 files",
            "m0.py, m1.py, m2.py, m3.py, m4.py and 2 more"
        ))
    );
}

#[tokio::test]
async fn a_check_that_ran_after_the_last_change_is_proof_enough() {
    let bench = bench(guards());
    let lead = agent("lead", 0);

    // A test run, passing or failing, by the lead or by an agent it started.
    for (who, command, code) in [
        (lead.clone(), "cargo test --workspace 2>&1 | tail -n 5", 0),
        (lead.clone(), "npm test", 1),
        (agent("verifier-1", 1), "cd sub && pytest -q", 0),
        // The project's own check command, which no table knows.
        (lead.clone(), "tools/ok --all", 0),
    ] {
        bench.prompt().await;
        bench.changed(&lead, "src/a.rs").await;
        bench.shell(&who, "c1", command, code).await;
        assert_eq!(bench.lead_stops().await, None, "{command}");
    }

    // A check that ran before the change proves nothing about it.
    bench.prompt().await;
    bench.changed(&lead, "src/a.rs").await;
    bench.shell(&lead, "c1", "cargo test", 0).await;
    bench.changed(&lead, "src/b.rs").await;
    assert_eq!(bench.lead_stops().await, Some(done_check("1 file", "src/b.rs")));

    // A goal that was claimed done had the project's checks run by the harness, whether they
    // passed or sent the claim back.
    for verdict in [
        ToolOutput::text("verified"),
        ToolOutput::error("not met: 1 test fails"),
    ] {
        bench.prompt().await;
        bench.changed(&lead, "src/a.rs").await;
        bench
            .emit(Event::ToolResult {
                agent: lead.clone(),
                call_id: "g".into(),
                tool: "goal_done".into(),
                output: verdict,
            })
            .await;
        assert_eq!(bench.lead_stops().await, None);
    }
}

#[tokio::test]
async fn files_their_checker_found_clean_need_no_other_check() {
    let bench = bench(guards());
    bench.prompt().await;
    std::fs::write(bench.path("findings.txt"), "").unwrap();
    assert_eq!(bench.write("a.chk", "one\n").await, "created a.chk (4 bytes)");
    assert_eq!(bench.lead_stops().await, None);

    // One clean file and one nothing checks: the second is still unproven.
    bench.prompt().await;
    bench.write("a.chk", "two\n").await;
    bench.write("b.rs", "fn b() {}\n").await;
    assert_eq!(bench.lead_stops().await, Some(done_check("1 file", "b.rs")));

    // A warning is not an error: the file counts as clean, and the report of the checks, which
    // has the first word, tells the lead of the warning.
    bench.prompt().await;
    std::fs::write(bench.path("findings.txt"), "a.chk:1:1: warning: odd\n").unwrap();
    bench.write("a.chk", "three\n").await;
    let report = bench.lead_stops().await.unwrap();
    assert!(report.starts_with("<checks>\n"), "{report}");
    assert_eq!(bench.lead_stops().await, None);

    // With an error the report comes first and the done-check at the stop after it: the file is
    // still as the checker refused it.
    bench.prompt().await;
    std::fs::write(bench.path("findings.txt"), "a.chk:1:1: error: wrong\n").unwrap();
    bench.write("a.chk", "four\n").await;
    let report = bench.lead_stops().await.unwrap();
    assert!(report.starts_with("<checks>\n"), "{report}");
    assert_eq!(bench.lead_stops().await, Some(done_check("1 file", "a.chk")));
    assert_eq!(bench.lead_stops().await, None);
}

const STUCK: &str = "times in a row in this turn. Running it again will fail the same way: form a new \
                     hypothesis about the cause and test that before trying this again.";

#[tokio::test]
async fn the_third_failure_of_the_same_command_carries_a_note_and_so_does_every_third_after_it() {
    let bench = bench(guards());
    let lead = agent("lead", 0);
    bench.prompt().await;
    let plain = "out\nexit code: 1";
    let noted = |count: u32| format!("{plain}\n[stuck] this command has failed {count} {STUCK}");

    assert_eq!(bench.shell(&lead, "c1", "cargo test -p core", 1).await, plain);
    // Another command in between, and another agent failing the same one, change nothing.
    assert_eq!(bench.shell(&lead, "c2", "cargo build", 1).await, plain);
    assert_eq!(
        bench
            .shell(&agent("coder-1", 1), "c3", "cargo test -p core", 1)
            .await,
        plain
    );
    // The same command, however it is spaced and whatever shapes its output.
    assert_eq!(
        bench
            .shell(&lead, "c4", "cargo  test -p core 2>&1 | tail -n 20", 1)
            .await,
        plain
    );
    assert_eq!(bench.shell(&lead, "c5", "cargo test -p core", 1).await, noted(3));
    assert_eq!(bench.shell(&lead, "c6", "cargo test -p core", 1).await, plain);
    assert_eq!(bench.shell(&lead, "c7", "cargo test -p core", 1).await, plain);
    assert_eq!(bench.shell(&lead, "c8", "cargo test -p core", 1).await, noted(6));

    // Once it passes it is no longer stuck: the count starts again.
    assert_eq!(
        bench.shell(&lead, "c9", "cargo test -p core", 0).await,
        "out\nexit code: 0"
    );
    assert_eq!(bench.shell(&lead, "c10", "cargo test -p core", 1).await, plain);
    assert_eq!(bench.shell(&lead, "c11", "cargo test -p core", 1).await, plain);
    // And a new prompt forgets the failures of the last.
    bench.prompt().await;
    assert_eq!(bench.shell(&lead, "c12", "cargo test -p core", 1).await, plain);
}

#[tokio::test]
async fn each_guard_is_off_when_its_setting_is() {
    let off = bench(GuardsExt {
        done_check: false,
        stuck_after: 0,
        runaway: false,
        verify: Vec::new(),
    });
    let lead = agent("lead", 0);
    off.prompt().await;
    off.changed(&lead, "src/a.rs").await;
    for n in 0..4 {
        let result = off.shell(&lead, &format!("c{n}"), "false", 1).await;
        assert_eq!(result, "out\nexit code: 1");
    }
    assert_eq!(off.lead_stops().await, None);
    assert!(off.registry.services.get::<Runaway>().is_none());

    // Each alone: the stuck guard without the done-check, and the other way round.
    let stuck_only = bench(GuardsExt {
        done_check: false,
        ..guards()
    });
    stuck_only.prompt().await;
    stuck_only.changed(&lead, "src/a.rs").await;
    stuck_only.shell(&lead, "c1", "false", 1).await;
    stuck_only.shell(&lead, "c2", "false", 1).await;
    let third = stuck_only.shell(&lead, "c3", "false", 1).await;
    assert!(
        third.contains("[stuck] this command has failed 3 times"),
        "{third}"
    );
    assert_eq!(stuck_only.lead_stops().await, None);
    assert!(stuck_only.registry.services.get::<Runaway>().is_some());

    let done_only = bench(GuardsExt {
        stuck_after: 0,
        ..guards()
    });
    done_only.prompt().await;
    done_only.changed(&lead, "src/a.rs").await;
    for n in 0..3 {
        let result = done_only.shell(&lead, &format!("c{n}"), "false", 1).await;
        assert_eq!(result, "out\nexit code: 1");
    }
    assert!(done_only.lead_stops().await.is_some());
}
