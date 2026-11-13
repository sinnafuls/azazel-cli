#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_ai::{Block, Message, Role};
use azazel_core::config::ReadGuard;
use azazel_core::process::Shell;
use azazel_core::session::{Entry, EntryKind};
use azazel_core::{
    ApprovalDecision, ApprovalRequest, ArtifactStore, AskKind, CommandOutcome, ExtCtx, Question, Registry,
    Ui, UiBridge, UiError,
};
use azazel_tools::CoreToolsExt;
use base64::Engine;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

fn bench() -> Bench {
    bench_with(ReadGuard::default())
}

/// A project whose `write` and `edit` ask for what `read_guard` says must have been read.
fn bench_with(read_guard: ReadGuard) -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let services = Arc::new(Services::new());
    let registry = Registry::new(ExtCtx {
        services,
        cwd: dir.path().into(),
        session_id: "s".into(),
    });
    registry
        .load(&CoreToolsExt {
            shell: Shell::resolve(""),
            shell_timeout: Duration::from_secs(30),
            artifacts_dir: dir.path().join(".artifacts"),
            read_guard,
        })
        .unwrap();
    Bench { dir, registry }
}

impl Bench {
    fn root(&self) -> &Path {
        self.dir.path()
    }

    fn path(&self, name: &str) -> PathBuf {
        self.dir.path().join(name)
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
            cwd: self.root().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        }
    }

    async fn call_as(&self, agent: &str, tool: &str, input: Value) -> ToolOutput {
        let ctx = self.ctx(agent);
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, tool).unwrap();
        tool.call(input, &ctx).await
    }

    async fn call(&self, tool: &str, input: Value) -> ToolOutput {
        self.call_as("lead", tool, input).await
    }

    /// The text of an edit of `path` that replaces `old` with `new`.
    async fn edit(&self, path: &str, old: &str, new: &str) -> String {
        let edits = json!({"path": path, "edits": [{"old": old, "new": new}]});
        self.call("edit", edits).await.text_content()
    }

    /// A file of `count` lines, `line 1` to `line <count>`.
    fn numbered(&self, name: &str, count: usize) -> String {
        let text: String = (1..=count).map(|n| format!("line {n}\n")).collect();
        std::fs::write(self.path(name), &text).unwrap();
        text
    }

    /// What `/allow-edit <args>` answers.
    async fn allow_edit(&self, args: &str) -> CommandOutcome {
        let command = self.registry.command("allow-edit").unwrap();
        command.handler.run(args, &self.registry.bus.ctx()).await
    }
}

#[tokio::test]
async fn the_extension_registers_the_core_tool_set() {
    let bench = bench();
    let names: Vec<String> = bench
        .registry
        .tools
        .tools(&bench.ctx("lead").agent)
        .iter()
        .map(|t| t.name().to_string())
        .collect();
    // `ast_grep` is there as well on a machine that has ast-grep.
    let names: Vec<&str> = names
        .iter()
        .map(String::as_str)
        .filter(|name| *name != "ast_grep")
        .collect();
    assert_eq!(
        names,
        [
            "read", "write", "edit", "glob", "grep", "shell", "job", "todo", "ask_user"
        ]
    );
}

#[tokio::test]
async fn write_creates_files_and_refuses_to_clobber_unseen_or_changed_content() {
    let bench = bench();
    let created = bench
        .call("write", json!({"path": "notes/a.txt", "content": "one\n"}))
        .await;
    assert_eq!(created.text_content(), "created notes/a.txt (4 bytes)");
    assert_eq!(
        std::fs::read_to_string(bench.path("notes/a.txt")).unwrap(),
        "one\n"
    );

    // The writer knows what it wrote, so it may write again without reading.
    let again = bench
        .call("write", json!({"path": "notes/a.txt", "content": "two\n"}))
        .await;
    assert_eq!(again.text_content(), "replaced notes/a.txt (4 bytes)");
    assert!(
        again.details.unwrap()["diff"]
            .as_str()
            .unwrap()
            .contains("-one\n+two\n")
    );

    // A file this agent never read.
    std::fs::write(bench.path("existing.txt"), "precious").unwrap();
    let refused = bench
        .call("write", json!({"path": "existing.txt", "content": "x"}))
        .await;
    assert!(refused.is_error);
    assert_eq!(refused.text_content(), "read the file before overwriting it");
    assert_eq!(
        std::fs::read_to_string(bench.path("existing.txt")).unwrap(),
        "precious"
    );

    bench.call("read", json!({"path": "existing.txt"})).await;
    std::fs::write(bench.path("existing.txt"), "changed elsewhere").unwrap();
    let stale = bench
        .call("write", json!({"path": "existing.txt", "content": "x"}))
        .await;
    assert_eq!(
        stale.text_content(),
        "file changed since you last read it; read it again"
    );

    bench.call("read", json!({"path": "existing.txt"})).await;
    assert!(
        !bench
            .call("write", json!({"path": "existing.txt", "content": "x"}))
            .await
            .is_error
    );
    assert!(
        bench
            .call("write", json!({"path": "notes", "content": "x"}))
            .await
            .is_error
    );
}

#[tokio::test]
async fn edit_needs_a_fresh_read_and_writes_nothing_when_an_edit_fails() {
    let bench = bench();
    std::fs::write(bench.path("code.rs"), "fn a() {}\nfn b() {}\n").unwrap();
    let edits = json!({"path": "code.rs", "edits": [{"old": "fn a()", "new": "fn alpha()"}]});

    let unread = bench.call("edit", edits.clone()).await;
    assert_eq!(unread.text_content(), "read the file before overwriting it");

    bench.call("read", json!({"path": "code.rs"})).await;
    let failed = bench
        .call(
            "edit",
            json!({"path": "code.rs", "edits": [{"old": "fn a()", "new": "fn alpha()"}, {"old": "nope", "new": "x"}]}),
        )
        .await;
    assert_eq!(failed.text_content(), "old text not found (edit #2)");
    assert_eq!(
        std::fs::read_to_string(bench.path("code.rs")).unwrap(),
        "fn a() {}\nfn b() {}\n"
    );

    let done = bench.call("edit", edits.clone()).await;
    assert_eq!(done.text_content(), "edited code.rs (1 replacement)");
    assert_eq!(
        std::fs::read_to_string(bench.path("code.rs")).unwrap(),
        "fn alpha() {}\nfn b() {}\n"
    );
    let diff = done.details.unwrap()["diff"].as_str().unwrap().to_string();
    assert!(diff.contains("-fn a() {}\n+fn alpha() {}\n"), "{diff}");

    // Chained edits work without re-reading, because the editor knows its own result.
    let chained = bench
        .call(
            "edit",
            json!({"path": "code.rs", "edits": [{"old": "fn b()", "new": "fn beta()"}]}),
        )
        .await;
    assert!(!chained.is_error);
    assert!(
        bench
            .call(
                "edit",
                json!({"path": "missing.rs", "edits": [{"old": "a", "new": "b"}]})
            )
            .await
            .is_error
    );
    let same = bench
        .call(
            "edit",
            json!({"path": "code.rs", "edits": [{"old": "fn beta()", "new": "fn beta()"}]}),
        )
        .await;
    assert_eq!(same.text_content(), "the edits leave the file unchanged");
}

#[tokio::test]
async fn an_edit_must_be_of_lines_that_were_read() {
    let bench = bench();
    bench.numbered("code.rs", 60);
    let page = bench
        .call("read", json!({"path": "code.rs", "offset": 10, "limit": 11}))
        .await;
    assert!(
        page.text_content()
            .ends_with("[lines 10-20 of 60; continue with offset=21]")
    );

    assert_eq!(
        bench
            .edit("code.rs", "line 40\nline 41\nline 42\n", "forty\n")
            .await,
        "edit targets lines 40-42 of code.rs, which you have not read; read them first"
    );
    // Only the part that was not read is asked for, and every edit of the call is looked at.
    let edits = json!({"path": "code.rs", "edits": [
        {"old": "line 19\nline 20\nline 21\nline 22", "new": "x"},
        {"old": "line 15", "new": "fifteen"},
        {"old": "line 5\n", "new": ""}
    ]});
    assert_eq!(
        bench.call("edit", edits).await.text_content(),
        "edit targets lines 5 and 21-22 of code.rs, which you have not read; read them first"
    );
    assert_eq!(
        std::fs::read_to_string(bench.path("code.rs"))
            .unwrap()
            .lines()
            .count(),
        60,
        "a refused edit writes nothing"
    );

    // Inside what was read the edit goes through, and what it wrote needs no reading.
    assert_eq!(
        bench.edit("code.rs", "line 12\n", "twelve\nand a half\n").await,
        "edited code.rs (1 replacement)"
    );
    assert_eq!(
        bench.edit("code.rs", "and a half", "12.5").await,
        "edited code.rs (1 replacement)"
    );
    // Line 20 is now line 21 and still read; line 21, now 22, still is not.
    assert_eq!(
        bench.edit("code.rs", "line 20", "twenty").await,
        "edited code.rs (1 replacement)"
    );
    assert_eq!(
        bench.edit("code.rs", "line 21", "twenty-one").await,
        "edit targets line 22 of code.rs, which you have not read; read it first"
    );

    // Reading the lines is what it takes; two reads add up.
    bench
        .call("read", json!({"path": "code.rs", "offset": 41, "limit": 3}))
        .await;
    assert_eq!(
        bench
            .edit("code.rs", "line 40\nline 41\nline 42\n", "forty\n")
            .await,
        "edited code.rs (1 replacement)"
    );
    // What another agent read is not what this one read.
    let other = bench
        .call_as(
            "coder-1",
            "edit",
            json!({"path": "code.rs", "edits": [{"old": "line 1\n", "new": ""}]}),
        )
        .await;
    assert_eq!(other.text_content(), "read the file before overwriting it");
}

#[tokio::test]
async fn replacing_a_file_takes_having_read_all_of_it() {
    let bench = bench();
    bench.numbered("code.rs", 30);
    bench.call("read", json!({"path": "code.rs", "limit": 10})).await;
    let partly = bench
        .call("write", json!({"path": "code.rs", "content": "all new\n"}))
        .await;
    assert_eq!(
        partly.text_content(),
        "you have read only part of code.rs; read the rest before overwriting it, or change it with edit"
    );
    bench.call("read", json!({"path": "code.rs", "offset": 11})).await;
    let whole = bench
        .call("write", json!({"path": "code.rs", "content": "all new\n"}))
        .await;
    assert_eq!(whole.text_content(), "replaced code.rs (8 bytes)");

    // Notes and logs are not code: a read of any part of one is enough for either tool.
    for name in ["notes.md", "todo.TXT", "run.log"] {
        bench.numbered(name, 30);
        bench.call("read", json!({"path": name, "limit": 5})).await;
        assert_eq!(
            bench.edit(name, "line 25", "twenty-five").await,
            format!("edited {name} (1 replacement)")
        );
        let replaced = bench.call("write", json!({"path": name, "content": "x"})).await;
        assert_eq!(replaced.text_content(), format!("replaced {name} (1 bytes)"));
    }
    // Unread, they are as closed as any file.
    bench.numbered("other.md", 3);
    assert_eq!(
        bench.edit("other.md", "line 2", "two").await,
        "read the file before overwriting it"
    );
}

#[tokio::test]
async fn the_guard_can_ask_for_any_read_or_for_none() {
    let any = bench_with(ReadGuard::File);
    any.numbered("code.rs", 30);
    assert_eq!(
        any.edit("code.rs", "line 25", "x").await,
        "read the file before overwriting it"
    );
    any.call("read", json!({"path": "code.rs", "limit": 5})).await;
    assert_eq!(
        any.edit("code.rs", "line 25", "x").await,
        "edited code.rs (1 replacement)"
    );
    let replaced = any
        .call("write", json!({"path": "code.rs", "content": "x"}))
        .await;
    assert_eq!(replaced.text_content(), "replaced code.rs (1 bytes)");
    // A change made elsewhere still has to be read.
    std::fs::write(any.path("code.rs"), "changed elsewhere\n").unwrap();
    assert_eq!(
        any.edit("code.rs", "changed", "x").await,
        "file changed since you last read it; read it again"
    );

    let none = bench_with(ReadGuard::Off);
    none.numbered("code.rs", 30);
    assert_eq!(
        none.edit("code.rs", "line 25", "x").await,
        "edited code.rs (1 replacement)"
    );
    std::fs::write(none.path("code.rs"), "changed elsewhere\n").unwrap();
    let replaced = none
        .call("write", json!({"path": "code.rs", "content": "x"}))
        .await;
    assert_eq!(replaced.text_content(), "replaced code.rs (1 bytes)");
}

#[tokio::test]
async fn allow_edit_lets_one_edit_through() {
    let bench = bench();
    bench.numbered("code.rs", 30);
    assert_eq!(
        bench.allow_edit("").await,
        CommandOutcome::Error("usage: /allow-edit <path>".to_string())
    );
    assert!(matches!(
        bench.allow_edit("missing.rs").await,
        CommandOutcome::Error(_)
    ));
    assert_eq!(
        bench.allow_edit(" code.rs ").await,
        CommandOutcome::Show(
            "the next edit of code.rs is allowed whatever the agent has read of it".to_string()
        )
    );

    // An edit that fails for another reason does not use the allowance up.
    assert_eq!(
        bench.edit("code.rs", "no such text", "x").await,
        "old text not found (edit #1)"
    );
    assert_eq!(
        bench.edit("code.rs", "line 25", "twenty-five").await,
        "edited code.rs (1 replacement)"
    );
    // One edit was allowed. The next is of a line the agent wrote, the one after is not.
    assert_eq!(
        bench.edit("code.rs", "twenty-five", "25").await,
        "edited code.rs (1 replacement)"
    );
    assert_eq!(
        bench.edit("code.rs", "line 3\n", "three\n").await,
        "edit targets line 3 of code.rs, which you have not read; read it first"
    );

    // It covers a file that changed since it was read, and a whole replacement.
    bench.call("read", json!({"path": "code.rs", "limit": 5})).await;
    std::fs::write(bench.path("code.rs"), "changed elsewhere\n").unwrap();
    bench.allow_edit("code.rs").await;
    let replaced = bench
        .call("write", json!({"path": "code.rs", "content": "x"}))
        .await;
    assert_eq!(replaced.text_content(), "replaced code.rs (1 bytes)");
    // An edit that needed no allowance leaves it for the one that does.
    bench.numbered("other.rs", 30);
    bench.allow_edit("other.rs").await;
    assert_eq!(
        bench.edit("code.rs", "x", "y").await,
        "edited code.rs (1 replacement)"
    );
    assert_eq!(
        bench.edit("other.rs", "line 9", "nine").await,
        "edited other.rs (1 replacement)"
    );
    assert_eq!(
        bench.edit("other.rs", "line 8", "eight").await,
        "edit targets line 8 of other.rs, which you have not read; read it first"
    );
}

#[tokio::test]
async fn one_agents_edit_makes_the_other_agents_copy_stale() {
    let bench = bench();
    std::fs::write(bench.path("shared.txt"), "v1\n").unwrap();
    bench
        .call_as("coder-1", "read", json!({"path": "shared.txt"}))
        .await;
    bench
        .call_as("coder-2", "read", json!({"path": "shared.txt"}))
        .await;

    let first = bench
        .call_as(
            "coder-1",
            "edit",
            json!({"path": "shared.txt", "edits": [{"old": "v1", "new": "v2"}]}),
        )
        .await;
    assert!(!first.is_error);
    let second = bench
        .call_as(
            "coder-2",
            "edit",
            json!({"path": "shared.txt", "edits": [{"old": "v1", "new": "v3"}]}),
        )
        .await;
    assert_eq!(
        second.text_content(),
        "file changed since you last read it; read it again"
    );
    assert_eq!(std::fs::read_to_string(bench.path("shared.txt")).unwrap(), "v2\n");
}

#[tokio::test]
async fn read_handles_directories_images_binaries_and_schemes() {
    let bench = bench();
    std::fs::create_dir_all(bench.path("src")).unwrap();
    std::fs::write(bench.path("src/lib.rs"), "pub fn f() {}\n").unwrap();
    std::fs::write(bench.path("data.bin"), b"\x00\x01\x02binary").unwrap();

    let listing = bench.call("read", json!({"path": "."})).await.text_content();
    assert_eq!(listing, "data.bin\t9 bytes\nsrc/");
    assert_eq!(
        bench
            .call("read", json!({"path": "src/lib.rs"}))
            .await
            .text_content(),
        "1\tpub fn f() {}"
    );

    let binary = bench.call("read", json!({"path": "data.bin"})).await;
    assert!(binary.is_error);
    assert!(
        binary.text_content().contains("binary file (9 bytes, .bin file)"),
        "{}",
        binary.text_content()
    );
    assert!(bench.call("read", json!({"path": "nope.txt"})).await.is_error);

    let image = image::RgbaImage::from_pixel(3000, 1000, image::Rgba([0, 120, 255, 255]));
    image.save(bench.path("shot.png")).unwrap();
    let shown = bench.call("read", json!({"path": "shot.png"})).await;
    let [Block::Image { mime, data }] = shown.content.as_slice() else {
        panic!("{:?}", shown.content)
    };
    assert_eq!(mime, "image/png");
    let decoded =
        image::load_from_memory(&base64::engine::general_purpose::STANDARD.decode(data).unwrap()).unwrap();
    assert_eq!((decoded.width(), decoded.height()), (1568, 523));

    let store = bench.registry.services.get::<ArtifactStore>().unwrap();
    store.save("call_9.txt", b"spilled output").unwrap();
    assert_eq!(
        bench
            .call("read", json!({"path": "artifact://call_9"}))
            .await
            .text_content(),
        "spilled output"
    );
    let unknown = bench.call("read", json!({"path": "nothing://x"})).await;
    // `pr` and `issue` are listed too where the `gh` program is installed.
    let supported = unknown.text_content();
    let supported = supported
        .strip_prefix("unknown scheme `nothing://`; supported: ")
        .unwrap();
    let always: Vec<&str> = supported
        .split(", ")
        .filter(|scheme| !matches!(*scheme, "pr" | "issue"))
        .collect();
    assert_eq!(always, ["artifact", "job", "session"]);
}

#[tokio::test]
async fn todo_replaces_the_list_per_agent_and_allows_one_item_in_progress() {
    let bench = bench();
    let set = bench
        .call(
            "todo",
            json!({"items": [
                {"text": "find the bug", "status": "done"},
                {"text": "fix it", "status": "in_progress"},
                {"text": "add a test", "status": "pending"}
            ]}),
        )
        .await;
    assert_eq!(set.text_content(), "[x] find the bug\n[~] fix it\n[ ] add a test");
    let store = bench.registry.services.get::<azazel_tools::TodoStore>().unwrap();
    assert_eq!(store.get("lead").len(), 3);
    assert!(store.get("scout-1").is_empty());

    let two_active = bench
        .call("todo", json!({"items": [{"text": "a", "status": "in_progress"}, {"text": "b", "status": "in_progress"}]}))
        .await;
    assert!(two_active.is_error);
    assert_eq!(
        store.get("lead").len(),
        3,
        "a rejected update leaves the list alone"
    );
    assert_eq!(
        bench.call("todo", json!({"items": []})).await.text_content(),
        "todo list cleared"
    );
}

fn message_entry(id: &str, message: Message) -> Entry {
    Entry {
        id: id.into(),
        parent_id: None,
        timestamp: String::new(),
        kind: EntryKind::Message { message },
    }
}

fn todo_call(id: &str, items: Value) -> Message {
    Message {
        role: Role::Assistant,
        content: vec![Block::ToolCall {
            id: id.into(),
            name: "todo".into(),
            input: json!({ "items": items }),
            signature: None,
        }],
        meta: None,
    }
}

fn result(id: &str, is_error: bool) -> Message {
    Message::tool(vec![Block::tool_result(id, "ok", is_error)])
}

#[tokio::test]
async fn the_task_list_comes_back_from_the_transcript_and_is_put_before_the_agent_while_work_remains() {
    let bench = bench();
    let store = bench.registry.services.get::<azazel_tools::TodoStore>().unwrap();
    let hooks = bench.registry.agent_hooks();
    let lead = bench.ctx("lead").agent;
    let sent = async |hooks: &Arc<dyn azazel_agent::AgentHooks>| {
        let mut messages = vec![Message::user("go on")];
        hooks.context(&lead, &mut messages).await;
        messages
    };
    // Nothing to remind of: the request is what the transcript holds.
    assert_eq!(sent(&hooks).await, [Message::user("go on")]);

    let first = json!([{"text": "find the bug", "status": "done"}, {"text": "fix it", "status": "pending"}]);
    let second =
        json!([{"text": "find the bug", "status": "done"}, {"text": "fix it", "status": "in_progress"}]);
    let refused = json!([{"text": "a", "status": "in_progress"}, {"text": "b", "status": "in_progress"}]);
    let branch = vec![
        message_entry("1", Message::user("fix the bug")),
        message_entry("2", todo_call("c1", first)),
        message_entry("3", result("c1", false)),
        message_entry("4", todo_call("c2", second)),
        message_entry("5", result("c2", false)),
        // A call that was refused changed nothing, and one without a result never ran.
        message_entry("6", todo_call("c3", refused)),
        message_entry("7", result("c3", true)),
        message_entry("8", todo_call("c4", json!([]))),
    ];
    store.set("scout-1", store.get("lead"));
    store.rehydrate("lead", &branch);
    let items = store.get("lead");
    assert_eq!(
        items.iter().map(|item| item.text.as_str()).collect::<Vec<_>>(),
        ["find the bug", "fix it"]
    );
    assert_eq!(items[1].status, azazel_tools::TodoStatus::InProgress);
    assert!(
        store.get("scout-1").is_empty(),
        "an agent of the session before is gone"
    );

    // The reminder is the last thing sent and is not part of what the agent keeps.
    let with_plan = sent(&hooks).await;
    assert_eq!(with_plan.len(), 2);
    assert_eq!(
        with_plan[1].text(),
        "[Plan] 1/2 done (your todo list; keep it current with `todo`)\n[x] find the bug\n[~] fix it"
    );
    // Another agent has no list and is told nothing.
    let mut other = vec![Message::user("look")];
    hooks.context(&bench.ctx("scout-2").agent, &mut other).await;
    assert_eq!(other.len(), 1);

    // What the user changed with `/todo` after the agent's last call wins, and a finished list
    // needs no reminder.
    let mut branch = branch;
    let done: Vec<azazel_tools::TodoItem> = items
        .into_iter()
        .map(|mut item| {
            item.status = azazel_tools::TodoStatus::Done;
            item
        })
        .collect();
    branch.push(Entry {
        id: "9".into(),
        parent_id: None,
        timestamp: String::new(),
        kind: azazel_tools::todo_entry(&done),
    });
    store.rehydrate("lead", &branch);
    assert_eq!(store.get("lead"), done);
    assert_eq!(sent(&hooks).await.len(), 1);

    // A branch that never had a list clears the one of the session before.
    store.rehydrate("lead", &branch[..1]);
    assert!(store.get("lead").is_empty());
}

/// Answers every question with what it was given, and keeps the questions.
struct Scripted {
    answers: std::sync::Mutex<Vec<Result<Vec<String>, UiError>>>,
    asked: std::sync::Mutex<Vec<Question>>,
}

#[async_trait::async_trait]
impl UiBridge for Scripted {
    async fn ask(&self, question: &Question) -> Result<Vec<String>, UiError> {
        self.asked.lock().unwrap().push(question.clone());
        self.answers.lock().unwrap().remove(0)
    }
    async fn confirm(&self, _: &str) -> Result<bool, UiError> {
        Err(UiError::NoUser)
    }
    async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
        Err(UiError::NoUser)
    }
    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }
    fn notify(&self, _: azazel_core::Level, _: &str) {}
    async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
        None
    }
}

#[tokio::test]
async fn ask_user_checks_the_question_before_anyone_is_asked() {
    let bench = bench();
    let ui = Arc::new(Scripted {
        answers: std::sync::Mutex::default(),
        asked: std::sync::Mutex::default(),
    });
    bench.registry.services.insert(Arc::new(Ui(ui.clone())));
    for (input, wrong) in [
        (json!({"question": " "}), "`question` is empty"),
        (
            json!({"question": "?", "options": ["only"]}),
            "a `select` question needs 2 to 8 options, not 1",
        ),
        (
            json!({"question": "?", "type": "multiselect", "options": ["1", "2", "3", "4", "5", "6", "7", "8", "9"]}),
            "a `multiselect` question needs 2 to 8 options, not 9",
        ),
        (
            json!({"question": "?", "type": "select"}),
            "a `select` question needs 2 to 8 options, not 0",
        ),
        (
            json!({"question": "?", "options": ["Red", "blue", " red "]}),
            "the option ` red ` is offered twice",
        ),
        (
            json!({"question": "?", "options": ["a", "Other…"], "allow_freeform": true}),
            "the option `Other…` is offered twice",
        ),
        (
            json!({"question": "?", "options": ["a", " "]}),
            "an option is empty",
        ),
        (
            json!({"question": "?", "type": "text", "options": ["a", "b"]}),
            "a `text` question takes no `options`",
        ),
        (
            json!({"question": "?", "type": "confirm", "options": ["yes", "no"]}),
            "a `confirm` question takes no `options`",
        ),
        (
            json!({"question": "?", "options": ["a", "b"], "multi": true, "allow_freeform": true}),
            "`allow_freeform` goes with `select` only, not `multiselect`",
        ),
        (
            json!({"question": "?", "allow_freeform": true}),
            "`allow_freeform` goes with `select` only, not `text`",
        ),
    ] {
        let refused = bench.call("ask_user", input.clone()).await;
        assert!(refused.is_error, "{input}");
        assert_eq!(refused.text_content(), wrong, "{input}");
    }
    assert!(
        ui.asked.lock().unwrap().is_empty(),
        "a question that is wrong is never shown"
    );
}

#[tokio::test]
async fn ask_user_passes_each_kind_of_question_on_and_a_user_who_declines_is_not_an_error() {
    let bench = bench();
    // Nobody there: the agent goes on without an answer.
    let alone = bench.call("ask_user", json!({"question": "which one?"})).await;
    assert!(!alone.is_error);
    assert_eq!(alone.text_content(), "the user declined to answer");
    let details = alone.details.unwrap();
    assert_eq!(
        (&details["cancelled"], &details["reason"], &details["type"]),
        (&json!(true), &json!("no interactive user"), &json!("text"))
    );

    let ui = Arc::new(Scripted {
        answers: std::sync::Mutex::new(vec![
            Ok(vec!["blue".into()]),
            Ok(vec!["teal".into()]),
            Ok(vec!["red".into(), "blue".into()]),
            Ok(vec!["yes".into()]),
            Ok(vec!["because".into()]),
            Err(UiError::Cancelled),
        ]),
        asked: std::sync::Mutex::default(),
    });
    bench.registry.services.insert(Arc::new(Ui(ui.clone())));
    let colours = json!(["red", "blue"]);

    let picked = bench
        .call(
            "ask_user",
            json!({"question": "Which?", "context": "Two paints are left.", "options": colours}),
        )
        .await;
    assert_eq!(picked.text_content(), "blue");
    let details = picked.details.unwrap();
    assert_eq!(
        (&details["answers"], &details["freeform"], &details["cancelled"]),
        (&json!(["blue"]), &json!(false), &json!(false))
    );

    let typed = bench
        .call(
            "ask_user",
            json!({"question": "Which?", "type": "select", "options": colours, "allow_freeform": true}),
        )
        .await;
    assert_eq!(typed.text_content(), "teal");
    assert_eq!(typed.details.unwrap()["freeform"], true);

    let several = bench
        .call(
            "ask_user",
            json!({"question": "Which?", "options": colours, "multi": true}),
        )
        .await;
    assert_eq!(several.text_content(), "red\nblue");

    let confirmed = bench
        .call("ask_user", json!({"question": "Paint it?", "type": "confirm"}))
        .await;
    assert_eq!(confirmed.text_content(), "yes");

    let said = bench.call("ask_user", json!({"question": "Why?"})).await;
    assert_eq!(said.text_content(), "because");

    let dismissed = bench
        .call("ask_user", json!({"question": "Which?", "options": colours}))
        .await;
    assert!(!dismissed.is_error);
    assert_eq!(dismissed.text_content(), "the user declined to answer");
    let details = dismissed.details.unwrap();
    assert_eq!(
        (&details["cancelled"], &details["reason"], &details["answers"]),
        (
            &json!(true),
            &json!("the user dismissed the question"),
            &Value::Null
        )
    );

    let asked = ui.asked.lock().unwrap();
    let kinds: Vec<(AskKind, bool, usize)> = asked
        .iter()
        .map(|question| (question.kind, question.allow_freeform, question.options.len()))
        .collect();
    assert_eq!(
        kinds,
        [
            (AskKind::Select, false, 2),
            (AskKind::Select, true, 2),
            (AskKind::Multiselect, false, 2),
            (AskKind::Confirm, false, 0),
            (AskKind::Text, false, 0),
            (AskKind::Select, false, 2),
        ]
    );
    assert_eq!(asked[0].context.as_deref(), Some("Two paints are left."));
    assert_eq!(asked[3].text, "Paint it?");
}

#[tokio::test]
async fn write_refuses_names_windows_keeps_for_devices() {
    let bench = bench();
    for name in ["con", "NUL", "aux.rs", "src/Com1.txt", "lpt9.tar.gz", "prn .md"] {
        let refused = bench.call("write", json!({"path": name, "content": "x"})).await;
        assert!(refused.is_error, "{name}");
        assert!(
            refused
                .text_content()
                .ends_with("is a reserved Windows device name; pick another file name"),
            "{name}: {}",
            refused.text_content()
        );
    }
    assert!(!bench.path("aux.rs").exists() && !bench.path("src").exists());
    // Names that only look alike are files like any other.
    for name in [
        "console.rs",
        "com10.txt",
        "auxiliary/nul_check.rs",
        "null",
        "lpt.txt",
    ] {
        let written = bench.call("write", json!({"path": name, "content": "x"})).await;
        assert!(!written.is_error, "{name}: {}", written.text_content());
    }
}
