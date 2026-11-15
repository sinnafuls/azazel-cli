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
            read_guard: Default::default(),
            patch: Default::default(),
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
        sources: Vec::new(),
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
    bench.registry.services.insert(Arc::new(Formatters::new(
        BTreeMap::from([("txt".to_string(), command)]),
        bench.dir.path().to_path_buf(),
        true,
    )));

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
    bench.registry.services.insert(Arc::new(Formatters::new(
        BTreeMap::from([("txt".to_string(), command)]),
        bench.dir.path().to_path_buf(),
        true,
    )));
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

/// The directory under the artifacts directory a file named `name` was converted into.
#[cfg(feature = "office")]
fn converted(bench: &Bench, name: &str) -> PathBuf {
    let prefix = format!("{name}-");
    let mut found: Vec<PathBuf> = std::fs::read_dir(bench.path(".artifacts"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.file_name().unwrap().to_string_lossy().starts_with(&prefix))
        .collect();
    assert_eq!(found.len(), 1, "{found:?}");
    found.remove(0)
}

#[cfg(feature = "office")]
fn text_of(path: PathBuf) -> String {
    std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

#[cfg(feature = "office")]
const REPORT: &str = "# Quarterly report\n\
\n\
Revenue grew by **12 %** over the *previous* quarter, see [the Q3 page](https://example.com/q3).\n\
\n\
## Revenue\n\
\n\
By region, in thousands:\n\
\n\
| Region | Q2 | Q3 |\n\
| --- | --- | --- |\n\
| North \\| East | 120 | 134 |\n\
| South | 80 | 91 |\n\
\n\
## Risks\n\
\n\
- Supply of gears\n\
- Exchange rates\n\
\n\
### Next steps\n\
\n\
1. Hire two engineers\n\
2. Open the Lyon office\n\
\n\
![](images/image1.png)\n\
\n\
Tom & Jerry <approved> this — in “quotes”.\n";

#[cfg(feature = "office")]
#[tokio::test]
async fn a_word_document_is_converted_once_and_read_by_its_outline() {
    let bench = bench();
    std::fs::copy(fixture("report.docx"), bench.path("report.docx")).unwrap();
    let file = bench.path("report.docx").display().to_string();
    let whole = bench.read("report.docx").await;
    assert!(!whole.is_error, "{}", whole.text_content());
    let dir = converted(&bench, "report.docx");
    assert_eq!(
        whole.text_content(),
        format!(
            "{file} is a Word document (36299 bytes). It was converted to Markdown in {}:\n\
             report.md\t26 lines\n\
             images/image1.png\t76 bytes\n\
             \n\
             Outline of report.md; `L` is the line, which `offset` takes when reading this file or report.md:\n\
             L1\t# Quarterly report\n\
             L5\t## Revenue\n\
             L14\t## Risks\n\
             L19\t### Next steps\n\
             \n\
             All of report.md:\n\
             \n\
             {}",
            dir.display(),
            REPORT.trim_end()
        )
    );
    assert_eq!(text_of(dir.join("report.md")), REPORT);
    let picture = std::fs::read(dir.join("images/image1.png")).unwrap();
    assert!(picture.starts_with(b"\x89PNG\r\n"));

    // A line of the outline is where `offset` starts, in the document as in its Markdown.
    let risks = bench
        .call("read", json!({"path": file, "offset": 14, "limit": 4}))
        .await;
    assert_eq!(
        risks.text_content(),
        "14\t## Risks\n15\t\n16\t- Supply of gears\n17\t- Exchange rates\n[lines 14-17 of 26; continue with offset=18]"
    );
    let markdown = dir.join("report.md");
    let same = bench
        .call("read", json!({"path": markdown, "offset": 14, "limit": 4}))
        .await;
    assert_eq!(same.text_content(), risks.text_content());

    // The conversion is kept: what is there is what the next read returns.
    std::fs::write(&markdown, "# Kept\n").unwrap();
    let again = bench.read(&file).await.text_content();
    assert!(
        again.ends_with("L1\t# Kept\n\nAll of report.md:\n\n# Kept"),
        "{again}"
    );
    assert_eq!(std::fs::read_dir(bench.path(".artifacts")).unwrap().count(), 1);

    // A document is not something `edit` can change, so reading one is not having seen it.
    std::fs::copy(&file, bench.path("copy.docx")).unwrap();
    bench.read("copy.docx").await;
    let overwrite = bench
        .call("write", json!({"path": "copy.docx", "content": "x"}))
        .await;
    assert!(overwrite.is_error);
}

#[cfg(feature = "office")]
#[tokio::test]
async fn a_presentation_is_its_slides_in_order_with_tables_pictures_and_notes() {
    let bench = bench();
    let shown = bench.read(&fixture("deck.pptx")).await;
    let shown = shown.text_content();
    let dir = converted(&bench, "deck.pptx");
    assert!(
        shown.contains(
            "deck.md\t19 lines\nimages/image1.png\t76 bytes\n\nOutline of deck.md; `L` is the line, which \
             `offset` takes when reading this file or deck.md:\n\
             L1\t# Slide 1: Gears for everyone\nL5\t# Slide 2: Where we are\nL13\t# Slide 3: Numbers\n"
        ),
        "{shown}"
    );
    assert_eq!(
        text_of(dir.join("deck.md")),
        "# Slide 1: Gears for everyone\n\
         \n\
         Acme Widgets, spring review\n\
         \n\
         # Slide 2: Where we are\n\
         \n\
         - Revenue is up\n  - North leads\n- Costs are flat\n\
         \n\
         Notes: Mention the Lyon office here.\n\
         \n\
         # Slide 3: Numbers\n\
         \n\
         | Region | Q3 |\n| --- | --- |\n| North | 134 |\n\
         \n\
         ![image.png](images/image1.png)\n"
    );
    assert!(
        std::fs::read(dir.join("images/image1.png"))
            .unwrap()
            .starts_with(b"\x89PNG")
    );
}

#[cfg(feature = "office")]
#[tokio::test]
async fn a_workbook_is_a_csv_file_per_sheet_and_a_preview_of_each() {
    let bench = bench();
    let shown = bench.read(&fixture("budget.xlsx")).await;
    let shown = shown.text_content();
    let dir = converted(&bench, "budget.xlsx");
    assert!(
        shown.contains(
            "budget.md\t26 lines\n\
             sheets/1-Budget_2026.csv\t152 bytes\n\
             sheets/2-Notes.csv\t30 bytes\n\
             sheets/3-Scratch.csv\t6 bytes\n\
             \n\
             Outline of budget.md; `L` is the line, which `offset` takes when reading this file or budget.md:\n\
             L1\t# Sheet 1: Budget 2026\nL14\t# Sheet 2: Notes\nL21\t# Sheet 3: Scratch\n"
        ),
        "{shown}"
    );
    // Row 4 and row 6 are empty in the sheet, and so are lines 4 and 6: a line is a row. The
    // total was never computed by Excel, so it is the formula.
    assert_eq!(
        text_of(dir.join("sheets/1-Budget_2026.csv")),
        "Item,Cost,Due,Paid,Note\n\
         \"Gears, small\",1200.5,2026-03-01,TRUE,\"said \"\"rush\"\"\"\n\
         Oil,80,2026-03-15 14:30:00,FALSE\n\
         \n\
         Total,=SUM(B2:B3),,,0.3\n\
         \n\
         ,,,,,,far away\n"
    );
    assert_eq!(
        text_of(dir.join("sheets/2-Notes.csv")),
        "\"Line one\nline two\",ünïcode\n"
    );
    assert_eq!(text_of(dir.join("sheets/3-Scratch.csv")), "1,2,3\n");
    assert_eq!(
        text_of(dir.join("budget.md")),
        "# Sheet 1: Budget 2026\n\
         \n\
         7 rows, 7 columns. All of it: sheets/1-Budget_2026.csv\n\
         \n\
         | Item | Cost | Due | Paid | Note |  |  |\n\
         | --- | --- | --- | --- | --- | --- | --- |\n\
         | Gears, small | 1200.5 | 2026-03-01 | TRUE | said \"rush\" |  |  |\n\
         | Oil | 80 | 2026-03-15 14:30:00 | FALSE |  |  |  |\n\
         |  |  |  |  |  |  |  |\n\
         | Total | =SUM(B2:B3) |  |  | 0.3 |  |  |\n\
         |  |  |  |  |  |  |  |\n\
         |  |  |  |  |  |  | far away |\n\
         \n\
         # Sheet 2: Notes\n\
         \n\
         1 row, 2 columns. All of it: sheets/2-Notes.csv\n\
         \n\
         | Line one<br>line two | ünïcode |\n\
         | --- | --- |\n\
         \n\
         # Sheet 3: Scratch\n\
         \n\
         1 row, 3 columns, hidden. All of it: sheets/3-Scratch.csv\n\
         \n\
         | 1 | 2 | 3 |\n\
         | --- | --- | --- |\n"
    );
    // The CSV is a text file like any other.
    let csv = bench
        .call("read", json!({"path": dir.join("sheets/3-Scratch.csv")}))
        .await;
    assert_eq!(csv.text_content(), "1\t1,2,3");
}

#[cfg(feature = "office")]
#[tokio::test]
async fn mail_of_both_kinds_is_its_people_its_text_and_its_attachments_as_files() {
    let bench = bench();
    let shown = bench.read(&fixture("order.eml")).await;
    assert!(!shown.is_error, "{}", shown.text_content());
    let dir = converted(&bench, "order.eml");
    // The subject and the sender are written in encoded words, the text in Latin-1 with
    // escapes: all of it comes out as what it says. The plain text is the body; the HTML is
    // the same again.
    assert_eq!(
        text_of(dir.join("order.md")),
        "# Order 4711: gears – confirmed\n\
         \n\
         - From: Gräce Hopper <grace@example.com>\n\
         - To: Ada Lovelace <ada@example.org>, linus@example.org\n\
         - Cc: Office, Lyon <lyon@example.com>\n\
         - Date: 2026-03-01T15:30:03+01:00\n\
         \n\
         ## Message\n\
         \n\
         Dear Ada,\n\
         \n\
         the order is confirmed: 12 gears at 4,50 £ each.\n\
         They ship on Monday.\n\
         \n\
         Gräce\n\
         \n\
         ## Attachments\n\
         \n\
         - attachments/packing_list.csv (19 bytes)\n"
    );
    assert_eq!(
        text_of(dir.join("attachments/packing_list.csv")),
        "item,count\ngear,12\n"
    );
    assert!(
        shown
            .text_content()
            .contains("L1\t# Order 4711: gears – confirmed\nL8\t## Message\nL17\t## Attachments\n"),
        "{}",
        shown.text_content()
    );

    let outlook = bench.read(&fixture("gears.msg")).await;
    assert!(!outlook.is_error, "{}", outlook.text_content());
    let dir = converted(&bench, "gears.msg");
    assert_eq!(
        text_of(dir.join("gears.md")),
        "# Gears for Lyon\n\
         \n\
         - From: Grace Hopper <grace@example.com>\n\
         - To: Ada Lovelace <ada@example.org>\n\
         - Date: 2026-03-01 14:30:03 UTC\n\
         \n\
         ## Message\n\
         \n\
         Dear Ada,\n\
         \n\
         the gears ship on Monday.\n\
         \n\
         ## Attachments\n\
         \n\
         - attachments/packing_list.csv (19 bytes)\n"
    );
    assert_eq!(
        text_of(dir.join("attachments/packing_list.csv")),
        "item,count\ngear,12\n"
    );
}

#[cfg(feature = "office")]
#[tokio::test]
async fn a_file_that_only_has_the_name_of_an_office_file_is_read_as_what_it_is() {
    let bench = bench();
    std::fs::write(bench.path("notes.docx"), "plain words\n").unwrap();
    std::fs::write(bench.path("notes.eml"), "Dear all,\nno headers here.\n").unwrap();
    assert_eq!(bench.read("notes.docx").await.text_content(), "1\tplain words");
    assert_eq!(
        bench.read("notes.eml").await.text_content(),
        "1\tDear all,\n2\tno headers here."
    );
    // A workbook under the name of a document: an Office file, but not that one.
    std::fs::copy(fixture("budget.xlsx"), bench.path("wrong.docx")).unwrap();
    let wrong = bench.read("wrong.docx").await;
    assert!(wrong.is_error);
    assert!(
        wrong
            .text_content()
            .ends_with("wrong.docx: it holds no document: it is not a Word file"),
        "{}",
        wrong.text_content()
    );
    assert!(!bench.path(".artifacts").exists());
}
