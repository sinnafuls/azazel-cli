#![allow(clippy::unwrap_used)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink};
use azazel_ai::Block;
use azazel_core::process::Shell;
use azazel_core::{ArtifactStore, ExtCtx, Registry};
use azazel_tools::CoreToolsExt;
use base64::Engine;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
}

fn bench() -> Bench {
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

    let asked = bench.call("ask_user", json!({"question": "which one?"})).await;
    assert!(asked.is_error);
    assert_eq!(asked.text_content(), "no interactive user");
}
