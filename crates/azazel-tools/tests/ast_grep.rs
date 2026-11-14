#![allow(clippy::unwrap_used)]

//! The `ast_grep` tool against `fake-sg`, a stand-in that is run as ast-grep is and prints what
//! ast-grep prints with `--json=stream`. ast-grep itself is not needed.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use azazel_agent::{Access, AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_core::config::{CheckReport, ReadGuard};
use azazel_core::process::Shell;
use azazel_core::{ExtCtx, FileTracker, Registry};
use azazel_tools::check::ChecksExt;
use azazel_tools::{AstGrepTool, CoreToolsExt};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

/// The fixture programs `fake-sg` and `fake-check`, built on first use into the directory this
/// test runs from.
static FIXTURES: LazyLock<(PathBuf, PathBuf)> = LazyLock::new(|| {
    // <target>/<profile>/deps/<this test>
    let test = std::env::current_exe().unwrap();
    let profile_dir = test.parent().unwrap().parent().unwrap();
    let mut build = Command::new(env!("CARGO"));
    build.args([
        "build",
        "--quiet",
        "--package",
        "fake-sg",
        "--package",
        "fake-check",
    ]);
    build.arg("--target-dir").arg(profile_dir.parent().unwrap());
    if profile_dir.file_name().unwrap() == "release" {
        build.arg("--release");
    }
    build.current_dir(Path::new(env!("CARGO_MANIFEST_DIR")).join("../.."));
    assert!(build.status().unwrap().success(), "could not build the fixtures");
    let binary = |name: &str| {
        let binary = profile_dir.join(format!("{name}{}", std::env::consts::EXE_SUFFIX));
        assert!(binary.is_file(), "{} is missing", binary.display());
        binary
    };
    (binary("fake-sg"), binary("fake-check"))
});

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
    tool: AstGrepTool,
}

/// A project with two script files that log, and a checker for `.chk` files that reports what
/// `findings.txt` holds.
fn bench(read_guard: ReadGuard) -> Bench {
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
            read_guard,
            patch: Default::default(),
        })
        .unwrap();
    let checker = format!(
        "\"{}\" --from \"{}\"",
        FIXTURES.1.display(),
        root.join("findings.txt").display()
    );
    registry
        .load(&ChecksExt {
            root: root.clone(),
            on_write: true,
            configured: BTreeMap::from([("chk".to_string(), checker)]),
            trusted: true,
            report: CheckReport::Delta,
            timeout: Duration::from_secs(30),
            limit: Duration::from_secs(30),
            gitleaks: None,
        })
        .unwrap();
    std::fs::create_dir_all(root.join("src")).unwrap();
    std::fs::write(root.join("src/a.ts"), "start()\nlog(first)\nlog(second)\nend()\n").unwrap();
    std::fs::write(root.join("src/b.ts"), "one\ntwo\nthree\nfour\nlog(third)\n").unwrap();
    let tracker = registry.services.get::<FileTracker>().unwrap();
    let tool = AstGrepTool::new(FIXTURES.0.clone(), tracker, read_guard);
    Bench { dir, registry, tool }
}

impl Bench {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

    fn text(&self, name: &str) -> String {
        std::fs::read_to_string(self.path(name)).unwrap()
    }

    fn ctx(&self, agent: &str) -> ToolCtx {
        let info = AgentInfo {
            id: agent.into(),
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
            services: self.registry.services.clone(),
        }
    }

    async fn run(&self, input: Value) -> ToolOutput {
        self.tool.call(input, &self.ctx("lead")).await
    }

    async fn ok(&self, input: Value) -> String {
        let output = self.run(input).await;
        assert!(!output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn err(&self, input: Value) -> String {
        let output = self.run(input).await;
        assert!(output.is_error, "{}", output.text_content());
        output.text_content()
    }

    /// A core tool called by `agent`.
    async fn core(&self, agent: &str, tool: &str, input: Value) -> String {
        let ctx = self.ctx(agent);
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, tool).unwrap();
        tool.call(input, &ctx).await.text_content()
    }
}

fn search(pattern: &str) -> Value {
    json!({ "action": "search", "pattern": pattern })
}

/// `log($MSG)` to `print($MSG)`: a preview, or with `paths` the writing of those files.
fn replace(paths: Option<&[&str]>) -> Value {
    let mut input = json!({ "action": "replace", "pattern": "log($MSG)", "rewrite": "print($MSG)" });
    if let Some(paths) = paths {
        input["apply"] = true.into();
        input["paths"] = json!(paths);
    }
    input
}

const BOTH: Option<&[&str]> = Some(&["src/a.ts", "src/b.ts"]);

#[tokio::test]
async fn a_search_lists_the_matches_file_by_file_with_what_the_metavariables_stand_for() {
    let bench = bench(ReadGuard::Range);
    // The fixture prints the last file first; the tool puts the matches in order.
    assert_eq!(
        bench.ok(search("log($MSG)")).await,
        "src/a.ts:2: log(first)\n  $MSG = first\n\
         src/a.ts:3: log(second)\n  $MSG = second\n\
         src/b.ts:5: log(third)\n  $MSG = third"
    );
    let mut some = search("log($MSG)");
    some["max_matches"] = 2.into();
    assert!(
        bench.ok(some).await.ends_with(
            "  $MSG = second\n[2 of 3 matches shown; narrow the pattern or the paths, or raise max_matches (at most 200)]"
        )
    );
    let mut one_file = search("log($$$ARGS)");
    one_file["paths"] = json!(["src/b.ts"]);
    assert_eq!(
        bench.ok(one_file).await,
        "src/b.ts:5: log(third)\n  $$$ARGS = third"
    );

    assert_eq!(bench.ok(search("nothing(here)")).await, "no matches");
    let mut other_language = search("log($MSG)");
    other_language["lang"] = "python".into();
    assert_eq!(bench.ok(other_language).await, "no matches");

    // What ast-grep says when it fails is the error, whichever way it exits.
    assert_eq!(
        bench.err(search("FAIL")).await,
        "ast-grep failed: Error: Cannot parse query as a valid pattern."
    );
    let mut unknown = search("log($MSG)");
    unknown["lang"] = "nope".into();
    assert_eq!(
        bench.err(unknown).await,
        "ast-grep failed: error: invalid value 'nope' for '--lang <LANG>': nope is not supported!"
    );
    assert_eq!(bench.err(search(" ")).await, "`pattern` is empty");
    let mut rewriting = search("log($MSG)");
    rewriting["rewrite"] = "x".into();
    assert_eq!(
        bench.err(rewriting).await,
        "`rewrite` goes with the action `replace`"
    );
    let missing = json!({ "action": "replace", "pattern": "log($MSG)" });
    assert_eq!(bench.err(missing).await, "`replace` needs `rewrite`");
    let unknown = json!({ "action": "dump", "pattern": "x" });
    assert_eq!(
        bench.err(unknown).await,
        "unknown action `dump`; actions: search, replace"
    );

    // A search shows one line of each match, which is not having read the file.
    let edit = json!({ "path": "src/a.ts", "edits": [{ "old": "log(first)", "new": "x" }] });
    assert_eq!(
        bench.core("lead", "edit", edit).await,
        "read the file before overwriting it"
    );
}

#[tokio::test]
async fn a_replacement_is_previewed_and_then_written_to_the_files_named() {
    let bench = bench(ReadGuard::Range);
    let before = (bench.text("src/a.ts"), bench.text("src/b.ts"));
    let preview = bench.run(replace(None)).await;
    let text = preview.text_content();
    assert!(
        text.starts_with(
            "preview: 3 replacements in 2 files, nothing written. To write them, run this again with \
             apply: true and paths: [\"src/a.ts\",\"src/b.ts\"]\n--- a/src/a.ts\n+++ b/src/a.ts\n"
        ),
        "{text}"
    );
    assert!(
        text.contains("-log(first)\n-log(second)\n+print(first)\n+print(second)\n"),
        "{text}"
    );
    assert!(text.ends_with("-log(third)\n+print(third)"), "{text}");
    assert_eq!(
        preview.details.unwrap()["diff"].as_str().unwrap().trim_end(),
        text.split_once('\n').unwrap().1
    );
    assert_eq!(
        (bench.text("src/a.ts"), bench.text("src/b.ts")),
        before,
        "a preview writes nothing"
    );
    assert_eq!(bench.tool.access(&replace(None)), Access::Read);
    assert_eq!(bench.tool.access(&replace(BOTH)), Access::Write);
    assert_eq!(bench.tool.subjects(&replace(BOTH)), ["src/a.ts", "src/b.ts"]);

    // The files to write are named one by one: they are what is asked permission for.
    for paths in [
        Some(&["src"][..]),
        Some(&["src/a.ts", "src/missing.ts"][..]),
        Some(&[][..]),
    ] {
        assert_eq!(
            bench.err(replace(paths)).await,
            "with `apply`, `paths` must name the files to change; the preview lists them"
        );
    }
    assert_eq!(bench.text("src/a.ts"), before.0);

    let applied = bench.run(replace(BOTH)).await;
    assert_eq!(
        applied.text_content(),
        "replaced: 3 replacements in 2 files\nsrc/a.ts (2 replacements)\nsrc/b.ts (1 replacement)"
    );
    assert_eq!(
        bench.text("src/a.ts"),
        "start()\nprint(first)\nprint(second)\nend()\n"
    );
    assert_eq!(bench.text("src/b.ts"), "one\ntwo\nthree\nfour\nprint(third)\n");
    let diff = applied.details.unwrap()["diff"].as_str().unwrap().to_string();
    assert!(
        diff.contains("+++ b/src/a.ts\n") && diff.contains("+print(third)\n"),
        "{diff}"
    );

    // The agent knows the lines it replaced, as after an edit, and no other line of the files.
    let own = json!({ "path": "src/b.ts", "edits": [{ "old": "print(third)", "new": "print(3)" }] });
    assert_eq!(
        bench.core("lead", "edit", own).await,
        "edited src/b.ts (1 replacement)"
    );
    let other = json!({ "path": "src/b.ts", "edits": [{ "old": "two", "new": "2" }] });
    assert_eq!(
        bench.core("lead", "edit", other).await,
        "edit targets line 2 of src/b.ts, which you have not read; read it first"
    );
    // Nothing is left to replace.
    assert_eq!(bench.ok(replace(None)).await, "no matches");
    assert_eq!(
        bench.err(replace(Some(&["src/a.ts"]))).await,
        "nothing to replace in these files"
    );
}

#[tokio::test]
async fn files_are_written_only_when_their_lines_were_seen_and_they_have_not_changed() {
    let bench = bench(ReadGuard::Range);
    let before = (bench.text("src/a.ts"), bench.text("src/b.ts"));
    // Neither previewed nor read.
    assert_eq!(
        bench.err(replace(BOTH)).await,
        "nothing was written; preview the replacement (apply: false) or read these lines first:\n\
         src/a.ts: read the file before overwriting it\n\
         src/b.ts: read the file before overwriting it"
    );
    // Read, but not where the replacement is.
    bench
        .core("lead", "read", json!({ "path": "src/b.ts", "limit": 2 }))
        .await;
    bench.core("lead", "read", json!({ "path": "src/a.ts" })).await;
    assert_eq!(
        bench.err(replace(BOTH)).await,
        "nothing was written; preview the replacement (apply: false) or read these lines first:\n\
         src/b.ts: the replacement targets line 5 of src/b.ts, which you have not read; read it first"
    );
    assert_eq!(
        (bench.text("src/a.ts"), bench.text("src/b.ts")),
        before,
        "one file refused is no file written"
    );

    // Previewed, then changed by someone else: the preview is not of what the file holds now.
    bench.ok(replace(None)).await;
    std::fs::write(bench.path("src/b.ts"), "log(changed)\nlog(third)\n").unwrap();
    assert_eq!(
        bench.err(replace(BOTH)).await,
        "nothing was written; preview the replacement (apply: false) or read these lines first:\n\
         src/b.ts: file changed since you last read it; read it again"
    );
    assert_eq!(bench.text("src/a.ts"), before.0);

    // A file the agent has read needs no preview, and one file can be written without the other.
    assert_eq!(
        bench.ok(replace(Some(&["src/a.ts"]))).await,
        "replaced: 2 replacements in 1 file\nsrc/a.ts (2 replacements)"
    );
    assert_eq!(bench.text("src/b.ts"), "log(changed)\nlog(third)\n");
    bench.ok(replace(None)).await;
    assert_eq!(
        bench.ok(replace(Some(&["src/b.ts"]))).await,
        "replaced: 2 replacements in 1 file\nsrc/b.ts (2 replacements)"
    );

    // With the read guard off nothing has to have been seen.
    let open = self::bench(ReadGuard::Off);
    assert_eq!(
        open.ok(replace(BOTH)).await,
        "replaced: 3 replacements in 2 files\nsrc/a.ts (2 replacements)\nsrc/b.ts (1 replacement)"
    );
}

#[tokio::test]
async fn what_it_writes_is_checked_and_made_known_like_any_edit() {
    let bench = bench(ReadGuard::Range);
    std::fs::write(bench.path("job.chk"), "one\nlog(job)\n").unwrap();
    std::fs::write(
        bench.path("findings.txt"),
        "job.chk:2:1: error: print is not allowed here\n",
    )
    .unwrap();
    // Another agent has read the file the replacement is about to change.
    bench.core("coder-1", "read", json!({ "path": "job.chk" })).await;
    let mut preview = replace(None);
    preview["paths"] = json!(["job.chk"]);
    bench.ok(preview).await;

    assert_eq!(
        bench.ok(replace(Some(&["job.chk"]))).await,
        "replaced: 1 replacement in 1 file\njob.chk (1 replacement)\n\
         checks:\njob.chk:2:1 error print is not allowed here [fake-check]"
    );
    let tracker = bench.registry.services.get::<FileTracker>().unwrap();
    let stale = tracker.take_stale("coder-1");
    assert_eq!(stale.len(), 1);
    assert_eq!(stale[0].1, "lead");
    let theirs = json!({ "path": "job.chk", "edits": [{ "old": "one", "new": "1" }] });
    assert_eq!(
        bench.core("coder-1", "edit", theirs).await,
        "file changed since you last read it; read it again"
    );
}
