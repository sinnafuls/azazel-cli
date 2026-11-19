use std::path::Path;
use std::sync::Arc;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::services::REDACTED;
use azazel_ai::{Block, Service, ServiceStore};
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

/// A page that changes 300 ms after its button is pressed: a spinner goes, a button and a status
/// line arrive, and the title and the address move on.
const LATER_PAGE: &str = r#"<!doctype html>
<html><head><title>draft</title></head>
<body>
<h1>Editor</h1>
<button id="save" onclick="save()">Save</button>
<div id="spinner">working</div>
<script>
function save() {
  setTimeout(() => {
    document.getElementById('spinner').remove();
    const undo = document.createElement('button');
    undo.textContent = 'Undo save';
    document.body.append(undo);
    document.title = 'draft (saved)';
    history.replaceState(null, '', '/editor?state=saved');
  }, 300);
}
</script>
</body></html>"#;

/// A sign-in form: a user name field that already holds something, a password field, a field
/// that is neither, and a title that says who signed in.
const LOGIN_PAGE: &str = r#"<!doctype html>
<html><head><title>sign in</title></head>
<body>
<h1>Sign in</h1>
<form onsubmit="document.title = 'signed in as ' + document.getElementById('user').value; return false">
<input id="user" type="email" aria-label="Email" value="old@example.com">
<input id="pass" type="password" aria-label="Password">
<input id="note" type="text" aria-label="Note">
<button>Sign in</button>
</form>
</body></html>"#;

/// Serves `page` for every request and returns the address.
async fn serve(page: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut request = [0u8; 4096];
                let _ = stream.read(&mut request).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{page}",
                    page.len()
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
    /// Where the site logins of this bench are stored.
    logins: Arc<ServiceStore>,
    ctx: ToolCtx,
}

impl Bench {
    fn new(dir: &Path) -> Bench {
        Bench::scaled(dir, 1.0)
    }

    fn scaled(dir: &Path, screenshot_scale: f64) -> Bench {
        let logins = Arc::new(ServiceStore::new(dir.join("secrets.json")));
        let settings = Settings {
            path: String::new(),
            headless: true,
            profile: dir.join("profile"),
            screenshot_scale,
            snapshot_diff: true,
        };
        let session = Arc::new(BrowserSession::new(settings).with_logins(logins.clone()));
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
            logins,
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
    let site = serve(PAGE).await;

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

#[tokio::test(flavor = "multi_thread")]
async fn waits_follow_what_the_page_does_after_an_action() {
    if find_binary("", &|name| std::env::var(name).ok()).is_none() {
        eprintln!("skipped: no Chromium-based browser on this machine");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bench = Bench::new(dir.path());
    let site = serve(LATER_PAGE).await;
    let opened = bench
        .ok(json!({ "action": "open", "url": format!("{site}/editor") }))
        .await;
    assert!(opened.contains("button \"Save\""), "{opened}");
    let state = "[document.title, location.search, !!document.getElementById('spinner')]";
    let state_now = || async {
        let value = bench.ok(json!({ "action": "eval", "expression": state })).await;
        serde_json::from_str::<Value>(&value).unwrap()
    };

    // Before the button is pressed none of it holds, and a wait says so at its limit, however
    // long the pause between its looks at the page.
    assert_eq!(state_now().await, json!(["draft", "", true]));
    assert!(
        bench
            .ok(json!({ "action": "wait", "for": "role:button \"Save\"" }))
            .await
            .starts_with("`role:button \"Save\"` after")
    );
    for never in [
        "role:button \"Undo save\"",
        "role:checkbox",
        "title:contains:saved",
        "url:contains:state=saved",
        "gone:#spinner",
    ] {
        let begun = std::time::Instant::now();
        let out = bench
            .call(json!({ "action": "wait", "for": never, "timeout_ms": 250, "poll_ms": 60000 }))
            .await;
        assert!(
            out.is_error && out.text_content() == format!("`{never}` did not happen within 250 ms"),
            "{never}: {}",
            out.text_content()
        );
        assert!(begun.elapsed() < std::time::Duration::from_secs(5), "{never}");
    }

    // Each wait returns once the page has caught up with the click, and not before.
    bench.ok(json!({ "action": "click", "selector": "#save" })).await;
    let waited = bench
        .ok(json!({ "action": "wait", "for": "role:button \"Undo save\"", "poll_ms": 20 }))
        .await;
    assert!(
        waited.starts_with("`role:button \"Undo save\"` after"),
        "{waited}"
    );
    assert_eq!(state_now().await, json!(["draft (saved)", "?state=saved", false]));
    for holds in [
        "title:contains:(saved)",
        "url:contains:/editor?state=saved",
        "gone:#spinner",
        "role:button",
    ] {
        let waited = bench.ok(json!({ "action": "wait", "for": holds })).await;
        assert!(waited.starts_with(&format!("`{holds}` after")), "{waited}");
    }

    // The same again from a fresh page, each of the other conditions leading the way.
    for leads in [
        "gone:#spinner",
        "title:contains:saved",
        "url:contains:state=saved",
    ] {
        bench
            .ok(json!({ "action": "navigate", "url": format!("{site}/editor") }))
            .await;
        assert_eq!(state_now().await, json!(["draft", "", true]), "{leads}");
        bench.ok(json!({ "action": "click", "selector": "#save" })).await;
        bench.ok(json!({ "action": "wait", "for": leads })).await;
        assert_eq!(
            state_now().await,
            json!(["draft (saved)", "?state=saved", false]),
            "{leads}"
        );
    }

    // What cannot be waited for says why at once.
    let no_role = bench.call(json!({ "action": "wait", "for": "role:" })).await;
    assert!(no_role.is_error && no_role.text_content().contains("names no role"));
    let bad = bench
        .call(json!({ "action": "wait", "for": "gone:a[", "timeout_ms": 5000 }))
        .await;
    assert_eq!(bad.text_content(), "invalid selector `gone:a[`");
    bench.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stored_login_is_typed_into_its_own_page_only_and_never_shown() {
    if find_binary("", &|name| std::env::var(name).ok()).is_none() {
        eprintln!("skipped: no Chromium-based browser on this machine");
        return;
    }
    const PASSWORD: &str = "hunter2-pass-9";
    let dir = tempfile::tempdir().unwrap();
    let bench = Bench::new(dir.path());
    let site = serve(LOGIN_PAGE).await;
    bench
        .logins
        .store_service(
            "fixture",
            Service::site(&format!("{site}/login*"), "me@example.com", PASSWORD).unwrap(),
        )
        .unwrap();
    let value = |id: &str| json!({ "action": "eval", "expression": format!("document.getElementById('{id}').value") });

    // The same form at an address the login was not stored for gets nothing.
    let elsewhere = bench
        .ok(json!({ "action": "open", "url": format!("{site}/account?next=/login") }))
        .await;
    let refused = bench
        .call(json!({
            "action": "fill_login",
            "ref_user": ref_of(&elsewhere, "\"Email\""),
            "ref_password": ref_of(&elsewhere, "\"Password\""),
        }))
        .await;
    assert!(refused.is_error);
    assert!(
        refused
            .text_content()
            .starts_with(&format!("no login is stored for {site}/account;")),
        "{}",
        refused.text_content()
    );
    assert_eq!(bench.ok(value("pass")).await, "\"\"");

    let page = bench
        .ok(json!({ "action": "open", "url": format!("{site}/login?next=/account"), "tab": "login" }))
        .await;
    let (user, pass, note) = (
        ref_of(&page, "\"Email\""),
        ref_of(&page, "\"Password\""),
        ref_of(&page, "\"Note\""),
    );
    // A password goes into a password field or nowhere, and the user name is not typed either.
    let visible = bench
        .call(json!({ "action": "fill_login", "ref_user": user, "ref_password": note }))
        .await;
    assert_eq!(
        visible.text_content(),
        format!("`ref_password` ({note}) is not a password field; nothing was typed")
    );
    assert_eq!(bench.ok(value("user")).await, "\"old@example.com\"");
    assert_eq!(bench.ok(value("note")).await, "\"\"");
    let missing = bench.call(json!({ "action": "fill_login" })).await;
    assert!(missing.text_content().contains("needs `ref_user`"));

    let typed = bench
        .ok(json!({ "action": "fill_login", "ref_user": user, "ref_password": pass, "submit": true }))
        .await;
    assert_eq!(
        typed,
        "typed the user name and the password of the stored login `fixture` and pressed Enter"
    );
    // What was in the field is replaced, and the form was sent with both.
    assert_eq!(
        bench
            .ok(json!({ "action": "eval", "expression": "document.title" }))
            .await,
        "\"signed in as me@example.com\""
    );
    let matches = format!("document.getElementById('pass').value === '{PASSWORD}'");
    assert_eq!(
        bench.ok(json!({ "action": "eval", "expression": matches })).await,
        "true"
    );

    // Whatever the agent does next, the page does not give the password back.
    assert_eq!(bench.ok(value("pass")).await, format!("\"{REDACTED}\""));
    let shown = bench
        .ok(json!({ "action": "eval", "expression": "document.getElementById('pass').type = 'text'; 1" }))
        .await;
    assert_eq!(shown, "1");
    let snapshot = bench.ok(json!({ "action": "snapshot" })).await;
    assert!(snapshot.contains("value=\"me@example.com\""), "{snapshot}");
    assert!(!snapshot.contains(PASSWORD), "{snapshot}");
    let thrown = bench
        .call(json!({
            "action": "eval",
            "expression": "throw new Error(document.getElementById('pass').value)",
        }))
        .await;
    assert!(thrown.is_error);
    assert!(
        thrown.text_content().contains(REDACTED) && !thrown.text_content().contains(PASSWORD),
        "{}",
        thrown.text_content()
    );

    // One field at a time, for a form that asks for the password on its own page.
    bench
        .ok(json!({ "action": "eval", "expression": "document.getElementById('pass').type = 'password'; document.getElementById('pass').value = ''; 1" }))
        .await;
    let again = bench.ok(json!({ "action": "snapshot" })).await;
    assert_eq!(
        bench
            .ok(json!({ "action": "fill_login", "ref_password": ref_of(&again, "\"Password\"") }))
            .await,
        "typed the password of the stored login `fixture`"
    );

    bench.session.shutdown().await;
}

/// A tall page of products with a button far from the top left corner.
const SHOP_PAGE: &str = r#"<!doctype html>
<html><head><title>shop</title></head>
<body style="margin:0">
<h1>Shop</h1>
<div style="height:300px"></div>
<button id="buy" style="margin-left:500px;width:200px;height:60px" onclick="document.title = 'bought'">Buy now</button>
<ul id="list"></ul>
<script>
const list = document.getElementById('list');
for (let i = 1; i <= 40; i++) { const li = document.createElement('li'); const a = document.createElement('a'); a.href = '#p' + i; a.textContent = 'Product number ' + i; li.append(a); list.append(li); }
document.getElementById('buy').addEventListener('click', () => { const li = document.createElement('li'); li.textContent = 'Thanks'; list.append(li); });
</script>
</body></html>"#;

#[tokio::test(flavor = "multi_thread")]
async fn a_click_on_a_scaled_screenshot_lands_on_the_pixel_it_points_at() {
    if find_binary("", &|name| std::env::var(name).ok()).is_none() {
        eprintln!("skipped: no Chromium-based browser on this machine");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let site = serve(SHOP_PAGE).await;
    let image_of = |output: &ToolOutput| {
        let Some(Block::Image { data, .. }) = output.content.get(1) else {
            panic!("no image: {:?}", output.content);
        };
        let bytes = base64::engine::general_purpose::STANDARD.decode(data).unwrap();
        let size = image::load_from_memory(&bytes).unwrap();
        (bytes.len(), data.len(), (size.width(), size.height()))
    };

    // Scale 1: the image is the window; no note is needed.
    let full = Bench::scaled(dir.path(), 1.0);
    full.ok(json!({ "action": "open", "url": site })).await;
    full.ok(json!({ "action": "resize", "width": 1280, "height": 800 }))
        .await;
    let whole = full.call(json!({ "action": "screenshot" })).await;
    let (full_bytes, _, full_size) = image_of(&whole);
    assert_eq!(full_size, (1280, 800));
    assert!(
        !whole.text_content().contains("the image is"),
        "{}",
        whole.text_content()
    );
    full.session.shutdown().await;

    // Scale 0.5: half the size, and positions are read off the image.
    let half = Bench::scaled(dir.path(), 0.5);
    half.ok(json!({ "action": "open", "url": site })).await;
    half.ok(json!({ "action": "resize", "width": 1280, "height": 800 }))
        .await;
    let shot = half.call(json!({ "action": "screenshot" })).await;
    let (half_bytes, _, half_size) = image_of(&shot);
    assert_eq!(half_size, (640, 400));
    assert!(
        shot.text_content()
            .contains("the image is 640x400, the window 1280x800"),
        "{}",
        shot.text_content()
    );
    println!(
        "screenshot 1280x800: scale 1.0 {full_bytes} bytes PNG, ~{} tokens; scale 0.5 {half_bytes} bytes PNG, ~{} tokens",
        1280u32.div_ceil(28) * 800u32.div_ceil(28),
        640u32.div_ceil(28) * 400u32.div_ceil(28)
    );
    let rect = half
        .ok(json!({ "action": "eval", "expression": "(() => { const r = document.getElementById('buy').getBoundingClientRect(); return [r.x + r.width / 2, r.y + r.height / 2]; })()" }))
        .await;
    let centre: Vec<f64> = serde_json::from_str(&rect).unwrap();
    // Where the button is in the image the model sees.
    let (x, y) = (centre[0] / 2.0, centre[1] / 2.0);
    let clicked = half.ok(json!({ "action": "click", "x": x, "y": y })).await;
    assert!(
        clicked.starts_with(&format!("clicked at {:.0},{:.0}", centre[0], centre[1])),
        "{clicked}"
    );
    assert!(clicked.contains("bought"), "{clicked}");
    half.session.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_second_snapshot_of_a_page_is_the_changes_only_and_old_refs_still_work() {
    if find_binary("", &|name| std::env::var(name).ok()).is_none() {
        eprintln!("skipped: no Chromium-based browser on this machine");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let bench = Bench::new(dir.path());
    let site = serve(SHOP_PAGE).await;
    // What the agent loop does: run the call, then shape its result, with every earlier result
    // still in the context.
    let step = |n: u32, input: Value| {
        let bench = &bench;
        async move {
            let mut ctx_call = bench.ctx.clone();
            ctx_call.call_id = format!("c{n}");
            let mut output = bench.tool.call(input.clone(), &ctx_call).await;
            assert!(!output.is_error, "{input}: {}", output.text_content());
            let live = |_: &str| true;
            bench.tool.shape(
                &input,
                &mut output,
                &azazel_agent::ShapeCtx {
                    agent_id: "lead",
                    call_id: &format!("c{n}"),
                    in_context: &live,
                },
            );
            output.text_content()
        }
    };
    let first = step(1, json!({ "action": "open", "url": site })).await;
    let buy = ref_of(&first, "Buy now");
    let same = step(2, json!({ "action": "snapshot" })).await;
    assert!(same.contains("unchanged"), "{same}");
    step(3, json!({ "action": "click", "ref": buy })).await;
    let second = step(4, json!({ "action": "snapshot" })).await;
    println!(
        "second snapshot: full {} bytes ~{} tokens; sent {} bytes ~{} tokens",
        first.len(),
        first.len().div_ceil(4),
        second.len(),
        second.len().div_ceil(4)
    );
    assert!(second.starts_with("(the page changed"), "{second}");
    assert!(second.contains("+ ") && second.contains("Thanks"), "{second}");
    assert!(second.len() * 3 < first.len(), "{second}");
    // The ref of the first snapshot still names the button.
    assert_eq!(ref_of(&first, "Buy now"), buy);
    step(5, json!({ "action": "click", "ref": buy })).await;
    bench.session.shutdown().await;
}
