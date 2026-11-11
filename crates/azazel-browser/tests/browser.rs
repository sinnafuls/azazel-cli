#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::sync::Arc;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::Block;
use azazel_browser::{BrowserSession, BrowserTool, Settings, find_binary};
use azazel_core::ArtifactStore;
use base64::Engine;
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;

const PAGE: &str = r#"<!doctype html>
<html><head><title>start</title></head>
<body>
<h1>Fixture page</h1>
<button id="go" onclick="document.title = 'clicked'; console.log('button pressed', 7)">Press me</button>
<input id="name" aria-label="Your name">
<p id="late"></p>
<script>setTimeout(() => { document.getElementById('late').textContent = 'arrived later'; }, 300);</script>
</body></html>"#;

/// Serves [`PAGE`] for every request and returns the address.
async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}")
}

struct Bench {
    tool: BrowserTool,
    session: Arc<BrowserSession>,
    ctx: ToolCtx,
}

impl Bench {
    fn new(dir: &Path) -> Bench {
        let session = Arc::new(BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: dir.join("profile"),
        }));
        let services = Arc::new(Services::default());
        services.insert(Arc::new(ArtifactStore::new(dir.join("artifacts"))));
        let agent = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        Bench {
            tool: BrowserTool(session.clone()),
            session,
            ctx: ToolCtx {
                agent_id: agent.id.clone(),
                session_id: agent.session_id.clone(),
                agent,
                call_id: "c1".into(),
                cwd: dir.to_path_buf(),
                cancel: CancellationToken::new(),
                update: UpdateSink::default(),
                services,
            },
        }
    }

    async fn call(&self, input: Value) -> ToolOutput {
        self.tool.call(input, &self.ctx).await
    }

    /// The text of a call that must succeed.
    async fn ok(&self, input: Value) -> String {
        let output = self.call(input.clone()).await;
        assert!(!output.is_error, "{input}: {}", output.text_content());
        output.text_content()
    }
}

/// The ref of the snapshot line containing `needle`.
fn ref_of(snapshot: &str, needle: &str) -> String {
    let line = snapshot
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no `{needle}` in:\n{snapshot}"));
    let start = line.find("[ref=").unwrap() + 5;
    line[start..].trim_end_matches(']').to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_page_is_opened_read_clicked_typed_into_and_captured() {
    if find_binary("", &|name| std::env::var(name).ok()).is_none() {
        eprintln!("skipped: no Chromium-based browser on this machine");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bench = Bench::new(dir.path());
    let site = serve().await;

    let opened = bench
        .ok(json!({ "action": "open", "url": format!("{site}/"), "tab": "app" }))
        .await;
    assert!(
        opened.starts_with(&format!("tab app · start · {site}/")),
        "{opened}"
    );
    assert!(opened.contains("heading \"Fixture page\""), "{opened}");
    assert!(opened.contains("textbox \"Your name\""), "{opened}");

    // Click by ref.
    let button = ref_of(&opened, "button \"Press me\"");
    let clicked = bench.ok(json!({ "action": "click", "ref": button })).await;
    assert!(
        clicked.contains("clicked at") && clicked.contains("· clicked ·"),
        "{clicked}"
    );
    assert_eq!(
        bench
            .ok(json!({ "action": "eval", "expression": "document.title" }))
            .await,
        "\"clicked\""
    );
    let console = bench.ok(json!({ "action": "console" })).await;
    assert!(console.contains("[log] button pressed 7"), "{console}");

    // Type by selector, then by ref from a fresh snapshot, and submit with a key.
    bench
        .ok(json!({ "action": "type", "selector": "#name", "text": "Ada" }))
        .await;
    let snapshot = bench.ok(json!({ "action": "snapshot" })).await;
    assert!(
        snapshot.contains("textbox \"Your name\" value=\"Ada\""),
        "{snapshot}"
    );
    let field = ref_of(&snapshot, "textbox \"Your name\"");
    bench
        .ok(json!({ "action": "type", "ref": field, "text": " Lovelace" }))
        .await;
    bench.ok(json!({ "action": "key", "keys": "Shift+!" })).await;
    assert_eq!(
        bench
            .ok(json!({ "action": "eval", "expression": "document.getElementById('name').value" }))
            .await,
        "\"Ada Lovelace!\""
    );

    // Waiting: text that appears later, a selector, and a condition that never holds.
    let waited = bench
        .ok(json!({ "action": "wait", "for": "text:arrived later" }))
        .await;
    assert!(waited.starts_with("`text:arrived later` after"), "{waited}");
    bench.ok(json!({ "action": "wait", "for": "#go" })).await;
    bench.ok(json!({ "action": "wait", "for": "idle" })).await;

    // The viewport follows `resize`.
    assert_eq!(
        bench
            .ok(json!({ "action": "resize", "width": 360, "height": 800 }))
            .await,
        "viewport is 360×800"
    );
    assert_eq!(
        bench
            .ok(json!({ "action": "eval", "expression": "window.innerWidth" }))
            .await,
        "360"
    );
    let tiny = bench
        .call(json!({ "action": "resize", "width": 10, "height": 800 }))
        .await;
    assert!(tiny.is_error && tiny.text_content().contains("between 200 and 8000"));
    bench
        .ok(json!({ "action": "resize", "width": 1280, "height": 800 }))
        .await;
    let never = bench
        .call(json!({ "action": "wait", "for": "#missing", "timeout_ms": 300 }))
        .await;
    assert!(never.is_error && never.text_content().contains("did not happen within 300 ms"));

    // Errors name what is wrong.
    let stale = bench.call(json!({ "action": "click", "ref": "e999" })).await;
    assert!(
        stale
            .text_content()
            .contains("no element `e999` in the last snapshot"),
        "{}",
        stale.text_content()
    );
    let nothing = bench
        .call(json!({ "action": "click", "selector": "#missing" }))
        .await;
    assert_eq!(nothing.text_content(), "nothing matches the selector `#missing`");
    let thrown = bench
        .call(json!({ "action": "eval", "expression": "nope()" }))
        .await;
    assert!(
        thrown.is_error && thrown.text_content().contains("ReferenceError"),
        "{}",
        thrown.text_content()
    );
    let scheme = bench
        .call(json!({ "action": "navigate", "url": "file:///etc/passwd" }))
        .await;
    assert_eq!(
        scheme.text_content(),
        "`file:///etc/passwd` is not an http or https URL"
    );

    // Screenshots: the viewport, and one element, which is smaller.
    let shot = bench.call(json!({ "action": "screenshot" })).await;
    assert!(!shot.is_error, "{}", shot.text_content());
    let saved = shot.text_content();
    let path = saved.strip_prefix("saved ").unwrap();
    let whole = image::load_from_memory(&std::fs::read(path).unwrap()).unwrap();
    let Block::Image { mime, data } = &shot.content[1] else {
        panic!("no image block: {:?}", shot.content);
    };
    assert_eq!(mime, "image/png");
    let sent = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
    let sent = image::load_from_memory(&sent).unwrap();
    assert!(sent.width().max(sent.height()) <= 1568);
    let element = bench
        .call(json!({ "action": "screenshot", "selector": "#go" }))
        .await;
    let element_path = element.text_content();
    let part = image::load_from_memory(&std::fs::read(element_path.strip_prefix("saved ").unwrap()).unwrap())
        .unwrap();
    assert!(part.width() < whole.width() && part.height() < whole.height());

    // Navigation and history.
    let moved = bench
        .ok(json!({ "action": "navigate", "url": format!("{site}/second") }))
        .await;
    assert_eq!(moved, format!("start · {site}/second"));
    // The earlier page may come back from the browser's cache as it was left, title included.
    let back = bench.ok(json!({ "action": "back" })).await;
    assert!(back.ends_with(&format!(" · {site}/")), "{back}");
    let network = bench
        .ok(json!({ "action": "network", "filter": "/second" }))
        .await;
    assert_eq!(network, format!("GET 200 Document {site}/second"));

    // Tabs: a second one becomes current; closing it returns to the first.
    bench
        .ok(json!({ "action": "open", "url": format!("{site}/other") }))
        .await;
    let tabs = bench.ok(json!({ "action": "tabs" })).await;
    let listed: Vec<&str> = tabs.lines().collect();
    assert_eq!(listed.len(), 2, "{tabs}");
    assert!(
        listed[0].starts_with("  app · ") && listed[0].ends_with(&format!(" · {site}/")),
        "{tabs}"
    );
    assert_eq!(listed[1], format!("* tab-1 · start · {site}/other"));
    assert_eq!(bench.ok(json!({ "action": "close" })).await, "closed tab-1");
    let unknown = bench.call(json!({ "action": "snapshot", "tab": "tab-1" })).await;
    assert_eq!(unknown.text_content(), "no tab named `tab-1`; open tabs: app");
    assert!(
        bench
            .ok(json!({ "action": "eval", "expression": "location.pathname" }))
            .await
            .contains('/')
    );

    // After the session ends the browser is gone, and a new `open` starts another.
    bench.session.shutdown().await;
    let after = bench.call(json!({ "action": "snapshot" })).await;
    assert_eq!(after.text_content(), azazel_browser::session::NO_TAB);
    let again = bench
        .ok(json!({ "action": "open", "url": format!("{site}/") }))
        .await;
    assert!(again.starts_with("tab tab-1 · start"), "{again}");
    bench.session.shutdown().await;
}
