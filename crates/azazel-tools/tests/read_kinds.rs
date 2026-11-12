#![allow(clippy::unwrap_used)]

//! What `read` does with PDFs, notebooks and the addresses of sessions, jobs and GitHub, and what
//! `write` and `edit` do when format on write is on.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{Access, AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::{AssistantMeta, Block, Message, StopReason, Usage, WireApi};
use azazel_core::process::Shell;
use azazel_core::session::EntryKind;
use azazel_core::{ExtCtx, Registry, Schemes, SessionStore};
use azazel_tools::format::Formatters;
use azazel_tools::schemes::GhScheme;
use azazel_tools::{CoreToolsExt, Jobs, ReadTool};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: dir.path().into(),
        session_id: "s".into(),
    });
    registry
        .load(&CoreToolsExt {
            shell: Shell::resolve(""),
            shell_timeout: Duration::from_secs(30),
            artifacts_dir: dir.path().join(".artifacts"),
        })
        .unwrap();
    Bench { dir, registry }
}

impl Bench {
    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
    }

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
            services: self.registry.services.clone(),
        }
    }

    async fn call(&self, tool: &str, input: Value) -> ToolOutput {
        let ctx = self.ctx();
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, tool).unwrap();
        tool.call(input, &ctx).await
    }

    async fn read(&self, path: &str) -> ToolOutput {
        self.call("read", json!({ "path": path })).await
    }
}

fn fixture(name: &str) -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/read")
        .join(name)
        .to_string_lossy()
        .into_owned()
}

#[cfg(feature = "pdf")]
#[tokio::test]
async fn a_pdf_reads_as_text_with_page_separators_and_pages_picks_some() {
    let bench = bench();
    let whole = bench.read(&fixture("three-pages.pdf")).await;
    assert!(!whole.is_error, "{}", whole.text_content());
    assert_eq!(
        whole.text_content(),
        "--- page 1 ---\nAlpha one\n\n--- page 2 ---\nBeta two\n\n--- page 3 ---\nGamma three"
    );

    let second = bench
        .call("read", json!({"path": fixture("three-pages.pdf"), "pages": "2"}))
        .await;
    assert_eq!(
        second.text_content(),
        "--- page 2 ---\nBeta two\n[pages 2-2 of 3]"
    );

    let bad = bench
        .call(
            "read",
            json!({"path": fixture("three-pages.pdf"), "pages": "two"}),
        )
        .await;
    assert!(bad.is_error);
    assert!(
        bad.text_content()
            .starts_with("pages `two`: expected a page number"),
        "{}",
        bad.text_content()
    );

    let scanned = bench.read(&fixture("scanned.pdf")).await;
    assert!(scanned.is_error);
    assert!(
        scanned
            .text_content()
            .ends_with("this PDF has no extractable text (it is probably scanned)"),
        "{}",
        scanned.text_content()
    );

    // A PDF is not something `edit` can change, so reading one does not count as having seen it.
    std::fs::copy(fixture("three-pages.pdf"), bench.path("copy.pdf")).unwrap();
    bench.read("copy.pdf").await;
    let overwrite = bench
        .call("write", json!({"path": "copy.pdf", "content": "x"}))
        .await;
    assert!(overwrite.is_error);
}

#[tokio::test]
async fn a_notebook_shows_code_output_and_an_image_note_and_returns_the_first_image() {
    let bench = bench();
    let shown = bench.read(&fixture("notebook.ipynb")).await;
    assert!(!shown.is_error, "{}", shown.text_content());
    assert_eq!(
        shown.text_content(),
        "# cell 1 [markdown]\n# Sales\nTotals by month.\n\n\
         # cell 2 [code]\ntotal = 2 + 3\nprint(total)\n--- output ---\n5\n\n\
         # cell 3 [code]\nplot(total)\n--- output ---\n\
         [image output: cell 3 image 1, image/png, 70 bytes]\n<Figure size 1x1>"
    );
    assert!(
        matches!(
            shown.content.as_slice(),
            [Block::Text { .. }, Block::Image { .. }]
        ),
        "{:?}",
        shown.content
    );

    // `offset` and `limit` count cells; a cell without an image returns none.
    let middle = bench
        .call(
            "read",
            json!({"path": fixture("notebook.ipynb"), "offset": 2, "limit": 1}),
        )
        .await;
    assert_eq!(
        middle.text_content(),
        "# cell 2 [code]\ntotal = 2 + 3\nprint(total)\n--- output ---\n5\n\
         [cells 2-2 of 3; continue with offset=3]"
    );
    assert_eq!(middle.content.len(), 1);

    // A file that only has the extension is shown as the text it is.
    std::fs::write(bench.path("broken.ipynb"), "not json\n").unwrap();
    assert_eq!(bench.read("broken.ipynb").await.text_content(), "1\tnot json");
}

fn reply(text: &str, tool: Option<(&str, Value)>) -> EntryKind {
    let mut content = vec![Block::text(text)];
    if let Some((name, input)) = tool {
        content.push(Block::ToolCall {
            id: "t1".into(),
            name: name.into(),
            input,
            signature: None,
        });
    }
    let meta = AssistantMeta {
        provider: "mock".into(),
        model: "lead".into(),
        api: WireApi::OpenAiChat,
        usage: Usage::default(),
        stop: StopReason::Stop,
        response_id: None,
        duration_ms: 0,
    };
    EntryKind::Message {
        message: Message::assistant(content, meta),
    }
}

#[tokio::test]
async fn session_addresses_describe_the_open_session_and_condense_another() {
    let bench = bench();
    let none = bench.read("session://").await;
    assert!(none.is_error);
    assert_eq!(none.text_content(), "no session is open");

    let sessions = bench.path("sessions");
    let earlier = SessionStore::create(&sessions, bench.dir.path(), None).unwrap();
    earlier.append(EntryKind::Message {
        message: Message::user("find the parser bug"),
    });
    earlier.append(reply(
        "It is in the lexer.",
        Some(("read", json!({"path": "src/lexer.rs"}))),
    ));
    let earlier_id = earlier.id();
    drop(earlier);

    let open = Arc::new(SessionStore::create(&sessions, bench.dir.path(), None).unwrap());
    open.append(EntryKind::Message {
        message: Message::user("what did we do before?"),
    });
    bench.registry.services.insert(open.clone());

    let described = bench.read("session://").await.text_content();
    assert!(
        described.starts_with(&format!("id: {}\nfile: ", open.id())),
        "{described}"
    );
    assert!(
        described.contains("messages: 1 from the user, 0 from the agent, 0 tool results"),
        "{described}"
    );
    assert!(described.contains("read one with session://<id>"), "{described}");
    let listed = described.lines().last().unwrap();
    assert!(
        listed.starts_with(&earlier_id) && listed.ends_with("2 messages  find the parser bug"),
        "{described}"
    );

    let transcript = bench.read(&format!("session://{earlier_id}")).await;
    assert_eq!(
        transcript.text_content(),
        "[prompt]\nfind the parser bug\n\nIt is in the lexer.\n\n→ read src/lexer.rs"
    );

    let missing = bench.read("session://zzzz").await;
    assert!(missing.is_error);
    assert_eq!(
        missing.text_content(),
        "no session with id `zzzz` in this project; session:// lists them"
    );
    let malformed = bench.read("session://../etc").await;
    assert_eq!(
        malformed.text_content(),
        "session://../etc: expected session:// or session://<id>"
    );
}

#[tokio::test]
async fn job_addresses_list_the_jobs_and_return_one_jobs_output() {
    let bench = bench();
    assert_eq!(bench.read("job://").await.text_content(), "no background jobs");
    let missing = bench.read("job://job-9").await;
    assert!(missing.is_error);
    assert_eq!(
        missing.text_content(),
        "no job named `job-9`; there are no background jobs"
    );

    let jobs = bench.registry.services.get::<Jobs>().unwrap();
    let id = jobs
        .start(&Shell::resolve(""), "echo from-the-job", bench.dir.path(), &[])
        .unwrap();
    assert_eq!(id, "job-1");
    let mut output = String::new();
    for _ in 0..200 {
        output = bench.read("job://job-1").await.text_content();
        if output.contains("exited") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert_eq!(output, "from-the-job\n[job-1: exited with code 0]");
    let listed = bench.read("job://").await.text_content();
    assert!(listed.starts_with("job-1  exited with code 0  echo"), "{listed}");
    assert_eq!(
        bench.read("job://nope").await.text_content(),
        "no job named `nope`; the jobs are: job-1"
    );
}

#[tokio::test]
async fn github_addresses_count_as_network_access_to_github() {
    let schemes = Arc::new(Schemes::default());
    schemes.add("test", Arc::new(GhScheme::pull_requests(PathBuf::from("gh"))));
    schemes.add("test", Arc::new(GhScheme::issues(PathBuf::from("gh"))));
    let read = ReadTool::new(Arc::default(), schemes);
    for address in ["pr://", "pr://12/diff", "issue://7"] {
        let input = json!({ "path": address });
        assert_eq!(read.access(&input), Access::Net, "{address}");
        assert_eq!(read.subjects(&input), ["domain:github.com"], "{address}");
    }

    // A malformed address is refused before `gh` is started.
    let bench = bench();
    bench.registry.schemes.add(
        "test",
        Arc::new(GhScheme::pull_requests(PathBuf::from("azazel-no-such-gh"))),
    );
    let refused = bench.read("pr://12/files").await;
    assert!(refused.is_error);
    assert_eq!(
        refused.text_content(),
        "pr://12/files: expected pr://, pr://<number> or pr://<number>/diff"
    );

    let unknown = bench.read("nope://x").await;
    let listed = unknown.text_content();
    assert!(
        listed.starts_with("unknown scheme `nope://`; supported: artifact, ") && listed.contains("job, "),
        "{listed}"
    );
}

/// A program that takes a file path and replaces the file's content with `formatted`, or, with
/// `fails`, complains and leaves it alone.
fn fake_formatter(dir: &Path, fails: bool) -> String {
    let (name, body) = match (cfg!(windows), fails) {
        (true, false) => ("fmt.cmd", "@echo off\r\necho formatted> %1\r\n"),
        (true, true) => ("fmt.cmd", "@echo off\r\necho bad syntax 1>&2\r\nexit /b 3\r\n"),
        (false, false) => ("fmt.sh", "#!/bin/sh\nprintf 'formatted\\n' > \"$1\"\n"),
        (false, true) => ("fmt.sh", "#!/bin/sh\necho 'bad syntax' >&2\nexit 3\n"),
    };
    let program = dir.join(name);
    std::fs::write(&program, body).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    format!("\"{}\"", program.display())
}

#[tokio::test]
async fn a_formatter_changes_the_written_file_and_the_next_edit_is_not_stale() {
    let bench = bench();
    let tools = bench.path("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let command = fake_formatter(&tools, false);
    bench
        .registry
        .services
        .insert(Arc::new(Formatters::new(BTreeMap::from([(
            "txt".to_string(),
            command,
        )]))));

    let written = bench
        .call("write", json!({"path": "a.txt", "content": "messy   text\n"}))
        .await;
    assert!(!written.is_error, "{}", written.text_content());
    let on_disk = std::fs::read_to_string(bench.path("a.txt")).unwrap();
    assert_eq!(on_disk.trim_end(), "formatted");
    assert_eq!(
        written.text_content(),
        format!("created a.txt ({} bytes); formatted with fmt", on_disk.len())
    );
    // The diff shown to the user is of what the file holds now.
    let diff = written.details.unwrap()["diff"].as_str().unwrap().to_string();
    assert!(diff.contains("+formatted") && !diff.contains("messy"), "{diff}");

    // The formatter's version is what the agent is taken to know: no "file changed" refusal.
    let edited = bench
        .call(
            "edit",
            json!({"path": "a.txt", "edits": [{"old": "formatted", "new": "by hand"}]}),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.text_content());
    assert_eq!(
        edited.text_content(),
        "edited a.txt (1 replacement); formatted with fmt"
    );
    assert_eq!(
        std::fs::read_to_string(bench.path("a.txt")).unwrap().trim_end(),
        "formatted"
    );

    // A kind of file no formatter is set for is left exactly as written.
    let other = bench
        .call("write", json!({"path": "b.data", "content": "raw\n"}))
        .await;
    assert_eq!(other.text_content(), "created b.data (4 bytes)");
}

#[tokio::test]
async fn a_failing_formatter_adds_a_note_and_never_fails_the_write() {
    let bench = bench();
    let tools = bench.path("tools");
    std::fs::create_dir_all(&tools).unwrap();
    let command = fake_formatter(&tools, true);
    bench
        .registry
        .services
        .insert(Arc::new(Formatters::new(BTreeMap::from([(
            "txt".to_string(),
            command,
        )]))));
    let written = bench
        .call("write", json!({"path": "a.txt", "content": "kept\n"}))
        .await;
    assert!(!written.is_error);
    assert_eq!(
        written.text_content(),
        "created a.txt (5 bytes); fmt failed: bad syntax"
    );
    assert_eq!(std::fs::read_to_string(bench.path("a.txt")).unwrap(), "kept\n");
    let edited = bench
        .call(
            "edit",
            json!({"path": "a.txt", "edits": [{"old": "kept", "new": "still fine"}]}),
        )
        .await;
    assert!(!edited.is_error, "{}", edited.text_content());
}
