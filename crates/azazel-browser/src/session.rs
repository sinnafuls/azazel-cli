//! The browser of one session: started on first use, its tabs, and what each tool action does.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azazel_ai::services::{self, Service};
use azazel_ai::{Block, ServiceStore};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block, image_size};
use azazel_core::{ArtifactStore, UiBridge, UiError};
use base64::Engine;
use process_wrap::tokio::ChildWrapper;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{Receiver, error::RecvError};
use tokio::sync::{Notify, watch};
use tokio::time::Instant as Moment;
use tokio_util::sync::CancellationToken;

use crate::cdp::{CLOSED, Cdp, CdpEvent};
use crate::launch::{LEFT_AT_ONCE, NOT_FOUND, find_binary, launch};
use crate::live::{self, BrowserStateInfo, Feed, LiveFrame, LiveInput, LiveSettings, UserActions};
use crate::page::{self, Baselines, Condition, Key, ShotFrame};

pub const NO_TAB: &str = "no tab is open; use the `open` action first";

/// How long a page gets to load after `open`, `navigate`, `back` and `reload`.
const LOAD: Duration = Duration::from_secs(30);
const DEFAULT_WAIT: Duration = Duration::from_secs(10);
/// How often `wait` looks at the page when the call names no `poll_ms`.
const DEFAULT_POLL: Duration = Duration::from_millis(100);
/// The shortest time between two looks.
const MIN_POLL: Duration = Duration::from_millis(10);
/// The network counts as idle once nothing has been in flight for this long.
const IDLE: Duration = Duration::from_millis(500);
const LOG_LINES: usize = 100;
/// Characters of an `eval` result returned to the model.
const MAX_EVAL: usize = 16_000;
/// Chromium cannot capture taller images in one piece.
const MAX_SHOT_HEIGHT: f64 = 16_000.0;
/// The page property that tells the document a navigation left from the one it arrived at.
const MARK: &str = "__azazelLeft";
/// What a field of a sign-in form is: the type of an `<input>`, or nothing for anything else.
const FIELD_KIND: &str = "function() { return this.tagName === 'INPUT' ? (this.type || 'text') : ''; }";
/// The kinds of field a user name is typed into.
const NAME_FIELDS: [&str; 6] = ["text", "email", "tel", "search", "url", "number"];
/// How long the browser gets to save its profile and leave before it is started again.
const LEAVE: Duration = Duration::from_secs(5);
/// The longest pause before the browser is started once more on a profile still held.
const RETRY_PAUSE: Duration = Duration::from_secs(4);
/// How long a client that has taken the browser over may go without input before the agents get
/// it back: a device that went away without closing its connection must not keep them waiting.
pub const HOLD_LEASE: Duration = Duration::from_secs(600);
const ANOTHER_HOLDS: &str = "another device has the browser";

#[derive(Debug, Clone)]
pub struct Settings {
    /// `[browser] path`; empty to look for an installed browser.
    pub path: String,
    pub headless: bool,
    /// The user data directory the browser runs on.
    pub profile: PathBuf,
    /// `[browser] screenshot_scale`: below 1, screenshots are scaled down and `x`,`y` are taken in
    /// pixels of the last screenshot.
    pub screenshot_scale: f64,
    /// `[browser] snapshot_diff`: refs keep their numbers within a tab, and a snapshot of the page
    /// the agent last saw is sent as what changed.
    pub snapshot_diff: bool,
    /// `[browser] live_fps`, `live_quality` and `live_max_width`: the live view.
    pub live: LiveSettings,
}

/// A snapshot an action returned: what [`Baselines::revise`] needs to send only what changed.
#[derive(Debug, Clone)]
pub(crate) struct Taken {
    target: String,
    url: String,
    /// The text of the result before the snapshot.
    header: String,
    body: String,
}

/// The input of the `browser` tool.
#[derive(Debug, Default, Deserialize)]
#[serde(default)]
pub struct Input {
    pub action: String,
    pub url: Option<String>,
    pub tab: Option<String>,
    #[serde(rename = "ref")]
    pub reference: Option<String>,
    pub selector: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub text: Option<String>,
    pub submit: bool,
    pub keys: Option<String>,
    pub hold_ms: Option<u64>,
    pub dy: Option<f64>,
    #[serde(rename = "for")]
    pub wait_for: Option<String>,
    pub timeout_ms: Option<u64>,
    pub poll_ms: Option<u64>,
    pub expression: Option<String>,
    pub full_page: bool,
    pub filter: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub ref_user: Option<String>,
    pub ref_password: Option<String>,
    pub reason: Option<String>,
}

struct Request {
    id: String,
    method: String,
    url: String,
    kind: String,
    status: String,
}

/// What a tab logged and requested, collected from the browser's events.
#[derive(Default)]
struct TabLog {
    console: VecDeque<String>,
    requests: VecDeque<Request>,
    in_flight: HashSet<String>,
    last_activity: Option<Instant>,
}

type Logs = Arc<Mutex<HashMap<String, TabLog>>>;

fn text_of<'a>(value: &'a Value, path: &[&str]) -> &'a str {
    path.iter()
        .try_fold(value, |value, key| value.get(key))
        .and_then(Value::as_str)
        .unwrap_or_default()
}

/// Records one browser event in the log of the tab it belongs to.
fn record(logs: &Logs, event: &CdpEvent) {
    let Some(session) = &event.session else {
        return;
    };
    let params = &event.params;
    let mut logs = logs.lock().unwrap_or_else(PoisonError::into_inner);
    let log = logs.entry(session.clone()).or_default();
    let mut say = |line: String| {
        if log.console.len() == LOG_LINES {
            log.console.pop_front();
        }
        log.console.push_back(line);
    };
    match event.method.as_str() {
        "Runtime.consoleAPICalled" => {
            let parts: Vec<String> = params
                .get("args")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|arg| match (arg.get("value"), arg.get("description")) {
                    (Some(Value::String(text)), _) => text.clone(),
                    (Some(value), _) => value.to_string(),
                    (None, Some(Value::String(text))) => text.clone(),
                    _ => text_of(arg, &["type"]).to_string(),
                })
                .collect();
            say(format!("[{}] {}", text_of(params, &["type"]), parts.join(" ")));
        }
        "Runtime.exceptionThrown" => {
            let details = params.get("exceptionDetails").unwrap_or(&Value::Null);
            let what = match text_of(details, &["exception", "description"]) {
                "" => text_of(details, &["text"]),
                description => description,
            };
            say(format!("[exception] {what}"));
        }
        "Network.requestWillBeSent" => {
            let id = text_of(params, &["requestId"]).to_string();
            if log.requests.len() == LOG_LINES {
                log.requests.pop_front();
            }
            log.requests.push_back(Request {
                id: id.clone(),
                method: text_of(params, &["request", "method"]).to_string(),
                url: text_of(params, &["request", "url"]).to_string(),
                kind: text_of(params, &["type"]).to_string(),
                status: "pending".to_string(),
            });
            log.in_flight.insert(id);
            log.last_activity = Some(Instant::now());
        }
        "Network.responseReceived" => {
            let id = text_of(params, &["requestId"]);
            if let Some(request) = log.requests.iter_mut().rev().find(|r| r.id == id) {
                request.status = params
                    .get("response")
                    .and_then(|r| r.get("status"))
                    .and_then(Value::as_i64)
                    .map_or_else(|| "?".to_string(), |status| status.to_string());
                request.kind = text_of(params, &["type"]).to_string();
            }
        }
        "Network.loadingFinished" | "Network.loadingFailed" => {
            let id = text_of(params, &["requestId"]);
            log.in_flight.remove(id);
            log.last_activity = Some(Instant::now());
            if event.method == "Network.loadingFailed"
                && let Some(request) = log.requests.iter_mut().rev().find(|r| r.id == id)
            {
                request.status = format!("failed: {}", text_of(params, &["errorText"]));
            }
        }
        _ => {}
    }
}

struct Tab {
    name: String,
    target: String,
    session: String,
    /// Refs of the last snapshot of this tab.
    refs: HashMap<u32, i64>,
    /// The highest ref number given in this tab, so that none is given twice.
    numbered: u32,
    /// How the last scaled screenshot of the viewport each agent was given relates to the page:
    /// that agent's `x`,`y` are read in it. Per agent, since another agent was shown another image.
    frames: HashMap<String, ShotFrame>,
}

/// What an action acts on.
enum Target {
    /// Parameters naming a DOM node: `{ backendNodeId }` or `{ objectId }`.
    Node(Value),
    Point(f64, f64),
}

struct Running {
    /// Kills the browser when dropped.
    child: Box<dyn ChildWrapper>,
    cdp: Arc<Cdp>,
    tabs: Vec<Tab>,
    /// Name of the tab used last.
    current: Option<String>,
    /// The blank page the browser starts with, used by the first `open`.
    spare: Option<String>,
    logs: Logs,
    named: u32,
    shots: u32,
    /// Whether the browser runs without a window.
    headless: bool,
    /// `[browser] screenshot_scale`.
    scale: f64,
    /// Refs keep their numbers between snapshots of a tab (`[browser] snapshot_diff`).
    stable_refs: bool,
    /// The snapshot the action running now returned.
    taken: Option<Taken>,
    /// The agent whose action runs now.
    agent: String,
}

/// `(target id, URL)` of the pages in a `Target.getTargets` reply.
fn page_targets(reply: &Value) -> Vec<(&str, &str)> {
    reply
        .get("targetInfos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|info| text_of(info, &["type"]) == "page")
        .map(|info| (text_of(info, &["targetId"]), text_of(info, &["url"])))
        .collect()
}

/// Whether a page the browser opened by itself is one to work with, rather than a part of the
/// browser's own interface (`chrome://…`, `edge://…` dialogs).
fn web_page(url: &str) -> bool {
    url == "about:blank" || page::host_of(url).is_some()
}

/// `bytes`, a screenshot of a `page`-sized area, as an image block scaled by `scale`, with the
/// frame that maps positions in it back to the page.
fn scaled_shot(bytes: &[u8], page: (f64, f64), scale: f64) -> Result<(Block, ShotFrame), String> {
    let size = image_size(bytes).ok_or("the browser returned an unreadable screenshot")?;
    let page = if page.0 > 0.0 && page.1 > 0.0 {
        page
    } else {
        (f64::from(size.0), f64::from(size.1))
    };
    let mut frame = ShotFrame::new(page, size, scale, MAX_IMAGE_SIDE);
    let image = image_block(bytes, frame.side())?;
    // The image as encoded, which rounding may leave a pixel off what was asked for.
    if let Block::Image { data, .. } = &image
        && let Ok(png) = base64::engine::general_purpose::STANDARD.decode(data)
        && let Some(shown) = image_size(&png)
    {
        frame.shown = shown;
    }
    Ok((image, frame))
}

impl Running {
    async fn start(settings: &Settings, headless: bool) -> Result<Running, String> {
        let binary = find_binary(&settings.path, &|name| std::env::var(name).ok())
            .ok_or_else(|| NOT_FOUND.to_string())?;
        let launched = launch(&binary, &settings.profile, headless).await?;
        let cdp = Arc::new(Cdp::connect(&launched.ws_url).await?);
        let logs = Logs::default();
        let (mut events, sink) = (cdp.subscribe(), logs.clone());
        tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) => record(&sink, &event),
                    Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => break,
                }
            }
        });
        let targets = cdp.call(None, "Target.getTargets", json!({})).await?;
        let spare = page_targets(&targets)
            .into_iter()
            .find(|(_, url)| *url == "about:blank")
            .map(|(id, _)| id.to_string());
        Ok(Running {
            child: launched.child,
            cdp,
            tabs: Vec::new(),
            current: None,
            spare,
            logs,
            named: 0,
            shots: 0,
            headless,
            scale: settings.screenshot_scale,
            stable_refs: settings.snapshot_diff,
            taken: None,
            agent: String::new(),
        })
    }

    /// The tab used last.
    fn current_tab(&self) -> Option<&Tab> {
        let name = self.current.as_ref()?;
        self.tabs.iter().find(|tab| &tab.name == name)
    }

    /// What the browser is doing now, for the clients that watch it.
    async fn describe(&self) -> BrowserStateInfo {
        let mut info = BrowserStateInfo {
            running: true,
            visible: !self.headless,
            ..BrowserStateInfo::default()
        };
        if let Some(tab) = self.current_tab() {
            info.tab = Some(tab.name.clone());
            if let Ok(Value::Array(parts)) = self
                .evaluate(&tab.session, "[document.title, location.href]")
                .await
            {
                let part = |n: usize| parts.get(n).and_then(Value::as_str).map(str::to_string);
                (info.title, info.url) = (part(0), part(1));
            }
        }
        info
    }

    /// A page to open a tab on: the blank one the browser started with, else a new one.
    async fn new_target(&mut self) -> Result<String, String> {
        if let Some(target) = self.spare.take() {
            return Ok(target);
        }
        let created = self
            .cdp
            .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
            .await?;
        Ok(text_of(&created, &["targetId"]).to_string())
    }

    /// The name and the address of every tab, for a browser that takes this one's place.
    async fn pages(&self) -> Vec<(String, String)> {
        let mut pages = Vec::new();
        for tab in &self.tabs {
            let address = self.evaluate(&tab.session, "location.href").await;
            let address = address.ok().and_then(|value| value.as_str().map(str::to_string));
            pages.push((
                tab.name.clone(),
                address.unwrap_or_else(|| "about:blank".to_string()),
            ));
        }
        pages
    }

    /// Opens the tabs another browser had, under their names.
    async fn reopen(&mut self, pages: Vec<(String, String)>, current: Option<String>) -> Result<(), String> {
        for (name, address) in pages {
            let target = self.new_target().await?;
            let index = self.attach(&target, Some(name)).await?;
            // A page that no longer loads is no reason to lose the others.
            let _ = self
                .go(
                    &self.tabs[index].session,
                    "Page.navigate",
                    json!({ "url": address }),
                )
                .await;
        }
        self.current = current.filter(|name| self.tabs.iter().any(|tab| &tab.name == name));
        Ok(())
    }

    /// Asks the browser to leave, so it saves its profile; the kill is for one that has not
    /// left within `limit`. Returns the process, which may still be on its way out.
    async fn close(mut self, limit: Duration) -> Box<dyn ChildWrapper> {
        let close = self.cdp.call(None, "Browser.close", json!({}));
        let _ = tokio::time::timeout(limit, close).await;
        let _ = tokio::time::timeout(limit, self.child.wait()).await;
        let _ = self.child.start_kill();
        self.child
    }

    /// Calls `function` with the page's object `object` as `this` and returns its value.
    async fn call_on(&self, session: &str, object: &str, function: &str) -> Result<Value, String> {
        let mut reply = self
            .call(
                session,
                "Runtime.callFunctionOn",
                json!({ "objectId": object, "functionDeclaration": function, "returnByValue": true }),
            )
            .await?;
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(text_of(details, &["exception", "description"]).to_string());
        }
        Ok(reply
            .get_mut("result")
            .and_then(|result| result.get_mut("value"))
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// Types the login stored for the page of the tab into the fields the call names, and
    /// nothing when no login is stored for that page or a field is not what the call says it
    /// is. Returns what to tell the agent, and the password typed.
    async fn fill_login(
        &mut self,
        input: &Input,
        stored: &BTreeMap<String, Service>,
    ) -> Result<(String, String), String> {
        let index = self.tab(input.tab.as_deref())?;
        let session = self.tabs[index].session.clone();
        if input.ref_user.is_none() && input.ref_password.is_none() {
            return Err("`fill_login` needs `ref_user`, `ref_password` or both".to_string());
        }
        let address = self.evaluate(&session, "location.href").await?;
        let address = address.as_str().unwrap_or_default();
        let Some((
            name,
            Service::Site {
                username, password, ..
            },
        )) = services::login_for(stored, address)
        else {
            let page = address.split(['?', '#']).next().unwrap_or_default();
            return Err(format!(
                "no login is stored for {page}; the user stores one with `azazel secrets add-site \
                 <address of the sign-in page> --username <name>`"
            ));
        };
        // Every field is looked at before anything is typed: a password never lands in a
        // field that shows it.
        let mut fields = Vec::new();
        for (field, reference, text, secret) in [
            ("ref_user", &input.ref_user, username, false),
            ("ref_password", &input.ref_password, password, true),
        ] {
            let Some(reference) = reference else { continue };
            let node = page::ref_number(reference)
                .and_then(|number| self.tabs[index].refs.get(&number))
                .ok_or_else(|| {
                    format!("no element `{reference}` in the last snapshot of this tab; take a new snapshot")
                })?;
            let resolved = self
                .call(&session, "DOM.resolveNode", json!({ "backendNodeId": node }))
                .await?;
            let object = text_of(&resolved, &["object", "objectId"]).to_string();
            let kind = self.call_on(&session, &object, FIELD_KIND).await?;
            let kind = kind.as_str().unwrap_or_default();
            let fits = match secret {
                true => kind == "password",
                false => NAME_FIELDS.contains(&kind),
            };
            if !fits {
                let wanted = if secret {
                    "a password field"
                } else {
                    "a text field"
                };
                return Err(format!(
                    "`{field}` ({reference}) is not {wanted}; nothing was typed"
                ));
            }
            fields.push((field, object, text));
        }
        for (field, object, text) in &fields {
            self.call_on(&session, object, "function() { this.focus(); this.select(); }")
                .await?;
            self.call(&session, "Input.insertText", json!({ "text": text }))
                .await?;
            let taken = self
                .call_on(&session, object, "function() { return this.value.length; }")
                .await?;
            if taken.as_u64() != Some(text.encode_utf16().count() as u64) {
                return Err(format!("the field of `{field}` did not take what was typed"));
            }
        }
        if input.submit {
            self.press(&session, &page::parse_keys("Enter")?, Duration::ZERO)
                .await?;
        }
        let what = match (&input.ref_user, &input.ref_password) {
            (Some(_), Some(_)) => "the user name and the password",
            (Some(_), None) => "the user name",
            _ => "the password",
        };
        Ok((
            format!(
                "typed {what} of the stored login `{name}`{}",
                if input.submit { " and pressed Enter" } else { "" }
            ),
            password.clone(),
        ))
    }

    async fn call(&self, session: &str, method: &str, params: Value) -> Result<Value, String> {
        self.cdp.call(Some(session), method, params).await
    }

    /// Attaches to a page and turns on the events the logs are built from.
    async fn attach(&mut self, target: &str, name: Option<String>) -> Result<usize, String> {
        let attached = self
            .cdp
            .call(
                None,
                "Target.attachToTarget",
                json!({ "targetId": target, "flatten": true }),
            )
            .await?;
        let session = text_of(&attached, &["sessionId"]).to_string();
        for domain in ["Page.enable", "Runtime.enable", "Network.enable"] {
            self.call(&session, domain, json!({})).await?;
        }
        let name = match name {
            Some(name) => name,
            None => loop {
                self.named += 1;
                let candidate = format!("tab-{}", self.named);
                if !self.tabs.iter().any(|tab| tab.name == candidate) {
                    break candidate;
                }
            },
        };
        self.tabs.push(Tab {
            name,
            target: target.to_string(),
            session,
            refs: HashMap::new(),
            numbered: 0,
            frames: HashMap::new(),
        });
        Ok(self.tabs.len() - 1)
    }

    /// The tab an action is about: the named one, else the one used last.
    fn tab(&mut self, name: Option<&str>) -> Result<usize, String> {
        let wanted = match name {
            Some(name) => name.to_string(),
            None => self.current.clone().ok_or_else(|| NO_TAB.to_string())?,
        };
        let index = self
            .tabs
            .iter()
            .position(|tab| tab.name == wanted)
            .ok_or_else(|| {
                let known: Vec<&str> = self.tabs.iter().map(|tab| tab.name.as_str()).collect();
                format!("no tab named `{wanted}`; open tabs: {}", known.join(", "))
            })?;
        self.current = Some(wanted);
        Ok(index)
    }

    /// Evaluates `expression` in the page and returns its value.
    async fn evaluate(&self, session: &str, expression: &str) -> Result<Value, String> {
        let mut reply = self
            .call(
                session,
                "Runtime.evaluate",
                json!({
                    "expression": expression,
                    "awaitPromise": true,
                    "returnByValue": true,
                    "userGesture": true,
                }),
            )
            .await?;
        if let Some(details) = reply.get("exceptionDetails") {
            return Err(match text_of(details, &["exception", "description"]) {
                "" => text_of(details, &["text"]).to_string(),
                description => description.to_string(),
            });
        }
        Ok(reply
            .get_mut("result")
            .and_then(|result| result.get_mut("value"))
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// `<title> · <url>` of the page; empty while the page cannot be asked (mid-navigation).
    async fn page_line(&self, session: &str) -> String {
        match self.evaluate(session, "[document.title, location.href]").await {
            Ok(Value::Array(parts)) if parts.len() == 2 => {
                format!(
                    "{} · {}",
                    parts[0].as_str().unwrap_or_default(),
                    parts[1].as_str().unwrap_or_default()
                )
            }
            _ => String::new(),
        }
    }

    /// Waits until the navigation issued after `events` was subscribed has brought up a loaded
    /// page: a document other than the one carrying `mark`, or the same one moved within itself.
    /// Returns false when that has not happened after `limit`.
    async fn loaded(
        &self,
        session: &str,
        mut events: Receiver<Arc<CdpEvent>>,
        mark: u64,
        limit: Duration,
    ) -> Result<bool, String> {
        let begun = Instant::now();
        // A page restored from the back/forward cache fires no load event, and events can be
        // dropped under load, so the page itself is asked as well.
        let arrived = format!("document.readyState === 'complete' && window.{MARK} !== {mark}");
        let mut started = false;
        loop {
            tokio::select! {
                event = events.recv() => match event {
                    Ok(event) if event.session.as_deref() == Some(session) => match event.method.as_str() {
                        "Page.loadEventFired" | "Page.navigatedWithinDocument" => return Ok(true),
                        "Page.frameStartedLoading" => started = true,
                        _ => {}
                    },
                    Ok(_) | Err(RecvError::Lagged(_)) => {}
                    Err(RecvError::Closed) => return Err(CLOSED.to_string()),
                },
                _ = tokio::time::sleep(Duration::from_millis(200)) => {
                    if self.evaluate(session, &arrived).await == Ok(Value::Bool(true)) {
                        return Ok(true);
                    }
                    // Nothing began to load: the command left the page where it was.
                    if !started && begun.elapsed() >= Duration::from_millis(500) {
                        return Ok(true);
                    }
                    if begun.elapsed() >= limit {
                        return Ok(false);
                    }
                }
            }
        }
    }

    /// Runs a navigation command and waits for the page; returns the page line.
    async fn go(&self, session: &str, method: &str, params: Value) -> Result<String, String> {
        static MARKS: AtomicU64 = AtomicU64::new(1);
        let mark = MARKS.fetch_add(1, Ordering::Relaxed);
        // Fails on pages that cannot run scripts; the load event still tells those apart.
        let _ = self.evaluate(session, &format!("window.{MARK} = {mark}")).await;
        let events = self.cdp.subscribe();
        let reply = self.call(session, method, params).await?;
        let refused = text_of(&reply, &["errorText"]);
        if !refused.is_empty() {
            return Err(format!("the page could not be loaded: {refused}"));
        }
        let loaded = self.loaded(session, events, mark, LOAD).await?;
        let mut line = self.page_line(session).await;
        if !loaded {
            line.push_str(&format!("\n(still loading after {} s)", LOAD.as_secs()));
        }
        Ok(line)
    }

    async fn snapshot(&mut self, index: usize) -> Result<String, String> {
        let reply = self
            .call(
                &self.tabs[index].session,
                "Accessibility.getFullAXTree",
                json!({}),
            )
            .await?;
        let nodes = reply
            .get("nodes")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let tab = &mut self.tabs[index];
        let shot = if self.stable_refs {
            page::snapshot_with(&nodes, Some(&tab.refs), tab.numbered)
        } else {
            page::snapshot(&nodes)
        };
        tab.numbered = tab.numbered.max(shot.refs.keys().max().copied().unwrap_or(0));
        tab.refs = shot.refs;
        Ok(if shot.text.is_empty() {
            "(the page has no accessible content)\n".to_string()
        } else {
            shot.text
        })
    }

    /// `header` and a snapshot of tab `index`, remembered as what this action returned.
    async fn snapshot_taken(&mut self, index: usize, header: String) -> Result<String, String> {
        let body = self.snapshot(index).await?;
        let session = self.tabs[index].session.clone();
        let url = match self.evaluate(&session, "location.href").await {
            Ok(Value::String(url)) => url,
            _ => String::new(),
        };
        let text = format!("{header}{body}");
        self.taken = Some(Taken {
            target: self.tabs[index].target.clone(),
            url,
            header,
            body,
        });
        Ok(text)
    }

    /// What `ref`, `selector` or `x`,`y` of the input point at; `None` when none is given.
    async fn target(&self, index: usize, input: &Input) -> Result<Option<Target>, String> {
        let tab = &self.tabs[index];
        if let Some(reference) = &input.reference {
            let node = page::ref_number(reference)
                .and_then(|number| tab.refs.get(&number))
                .ok_or_else(|| {
                    format!("no element `{reference}` in the last snapshot of this tab; take a new snapshot")
                })?;
            return Ok(Some(Target::Node(json!({ "backendNodeId": node }))));
        }
        if let Some(selector) = &input.selector {
            let expression = format!("document.querySelector({})", Value::String(selector.clone()));
            let reply = self
                .call(
                    &tab.session,
                    "Runtime.evaluate",
                    json!({ "expression": expression, "returnByValue": false }),
                )
                .await?;
            if let Some(details) = reply.get("exceptionDetails") {
                return Err(format!(
                    "invalid selector `{selector}`: {}",
                    text_of(details, &["exception", "description"])
                ));
            }
            let object = text_of(&reply, &["result", "objectId"]);
            if object.is_empty() {
                return Err(format!("nothing matches the selector `{selector}`"));
            }
            return Ok(Some(Target::Node(json!({ "objectId": object }))));
        }
        match (input.x, input.y) {
            // After a scaled screenshot the model reads positions off the image.
            (Some(x), Some(y)) => {
                let (x, y) = tab
                    .frames
                    .get(&self.agent)
                    .map_or((x, y), |frame| frame.to_page(x, y));
                Ok(Some(Target::Point(x, y)))
            }
            (None, None) => Ok(None),
            _ => Err("give both `x` and `y`".to_string()),
        }
    }

    /// The quad of a node, scrolled into view, in viewport coordinates.
    async fn quad(&self, session: &str, node: &Value) -> Result<Vec<f64>, String> {
        // Fails for nodes that cannot scroll (text nodes); their box is still looked up.
        let _ = self
            .call(session, "DOM.scrollIntoViewIfNeeded", node.clone())
            .await;
        let numbers = |value: &Value| -> Vec<f64> {
            value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_f64)
                .collect()
        };
        if let Ok(reply) = self.call(session, "DOM.getContentQuads", node.clone()).await
            && let Some(quad) = reply
                .get("quads")
                .and_then(Value::as_array)
                .and_then(|quads| quads.first())
            && page::bounds(&numbers(quad)).is_some()
        {
            return Ok(numbers(quad));
        }
        let model = self.call(session, "DOM.getBoxModel", node.clone()).await;
        let quad = model
            .as_ref()
            .ok()
            .and_then(|reply| reply.get("model")?.get("content"))
            .map(numbers);
        quad.filter(|quad| page::bounds(quad).is_some())
            .ok_or_else(|| "the element has no visible box (hidden or detached)".to_string())
    }

    async fn point(&self, session: &str, target: &Target) -> Result<(f64, f64), String> {
        match target {
            Target::Point(x, y) => Ok((*x, *y)),
            Target::Node(node) => {
                let quad = self.quad(session, node).await?;
                page::centre(&quad).ok_or_else(|| "the element has no visible box".to_string())
            }
        }
    }

    async fn mouse(&self, session: &str, kind: &str, x: f64, y: f64, extra: Value) -> Result<(), String> {
        let mut params = json!({ "type": kind, "x": x, "y": y });
        if let (Some(params), Some(extra)) = (params.as_object_mut(), extra.as_object()) {
            params.extend(extra.clone());
        }
        self.call(session, "Input.dispatchMouseEvent", params)
            .await
            .map(drop)
    }

    async fn press(&self, session: &str, key: &Key, hold: Duration) -> Result<(), String> {
        let mut down = json!({
            "type": if key.text.is_some() { "keyDown" } else { "rawKeyDown" },
            "modifiers": key.modifiers,
            "key": key.key,
            "code": key.code,
            "windowsVirtualKeyCode": key.vk,
            "nativeVirtualKeyCode": key.vk,
        });
        if let Some(text) = &key.text {
            down["text"] = json!(text);
        }
        self.call(session, "Input.dispatchKeyEvent", down.clone()).await?;
        if !hold.is_zero() {
            tokio::time::sleep(hold).await;
        }
        down["type"] = json!("keyUp");
        if let Some(up) = down.as_object_mut() {
            up.remove("text");
        }
        self.call(session, "Input.dispatchKeyEvent", down).await.map(drop)
    }

    async fn viewport(&self, session: &str) -> Result<Value, String> {
        let mut metrics = self.call(session, "Page.getLayoutMetrics", json!({})).await?;
        Ok(metrics
            .get_mut("cssVisualViewport")
            .map(Value::take)
            .unwrap_or(Value::Null))
    }

    /// Whether the page shows a node with `role` (and `name`) in its accessibility tree. False
    /// while the tree cannot be read, as in the middle of a navigation.
    async fn shows_role(&self, session: &str, role: &str, name: Option<&str>) -> bool {
        let tree = self.call(session, "Accessibility.getFullAXTree", json!({})).await;
        tree.is_ok_and(|reply| {
            let nodes = reply.get("nodes").and_then(Value::as_array);
            nodes.is_some_and(|nodes| page::has_role(nodes, role, name))
        })
    }

    /// Waits until what `wanted` describes holds (see [`page::condition`]), looking every `poll`.
    async fn wait(
        &self,
        session: &str,
        wanted: &str,
        limit: Duration,
        poll: Duration,
    ) -> Result<String, String> {
        let condition = page::condition(wanted)?;
        let begun = Instant::now();
        loop {
            let met = match &condition {
                Condition::Script(script) => match self.evaluate(session, script).await {
                    Ok(value) => value == Value::Bool(true),
                    // An invalid selector never becomes valid.
                    Err(why) if why.contains("SyntaxError") => {
                        return Err(format!("invalid selector `{wanted}`"));
                    }
                    // The page is navigating; ask again.
                    Err(_) => false,
                },
                Condition::Idle => {
                    let logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
                    logs.get(session).is_none_or(|log| {
                        log.in_flight.is_empty() && log.last_activity.is_none_or(|at| at.elapsed() >= IDLE)
                    })
                }
                Condition::Role { role, name } => self.shows_role(session, role, name.as_deref()).await,
            };
            if met {
                return Ok(format!("`{wanted}` after {} ms", begun.elapsed().as_millis()));
            }
            let left = limit.saturating_sub(begun.elapsed());
            if left.is_zero() {
                return Err(format!(
                    "`{wanted}` did not happen within {} ms",
                    limit.as_millis()
                ));
            }
            // The last look comes at the limit, however long the pause between looks.
            tokio::time::sleep(poll.min(left)).await;
        }
    }

    async fn screenshot(
        &mut self,
        index: usize,
        input: &Input,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Vec<Block>, String> {
        let store = artifacts.ok_or("there is nowhere to save the screenshot: artifacts are unavailable")?;
        let session = self.tabs[index].session.clone();
        let mut params = json!({ "format": "png" });
        let clip = if let Some(Target::Node(node)) = self.target(index, input).await? {
            let quad = self.quad(&session, &node).await?;
            let (x, y, width, height) = page::bounds(&quad).ok_or("the element has no visible box")?;
            let viewport = self.viewport(&session).await?;
            let offset = |key: &str| viewport.get(key).and_then(Value::as_f64).unwrap_or(0.0);
            Some((x + offset("pageX"), y + offset("pageY"), width, height))
        } else if input.full_page {
            let metrics = self.call(&session, "Page.getLayoutMetrics", json!({})).await?;
            let size = |key: &str| {
                metrics
                    .get("cssContentSize")
                    .and_then(|s| s.get(key))
                    .and_then(Value::as_f64)
            };
            size("width")
                .zip(size("height"))
                .map(|(width, height)| (0.0, 0.0, width, height.min(MAX_SHOT_HEIGHT)))
        } else {
            None
        };
        if let Some((x, y, width, height)) = clip {
            params["captureBeyondViewport"] = json!(true);
            params["clip"] = json!({ "x": x, "y": y, "width": width, "height": height, "scale": 1 });
        }
        let reply = self.call(&session, "Page.captureScreenshot", params).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(text_of(&reply, &["data"]))
            .map_err(|e| format!("the browser returned an unreadable screenshot: {e}"))?;
        self.shots += 1;
        let name = format!("browser-{}-{}.png", self.tabs[index].name, self.shots);
        let path = store
            .save(&name, &bytes)
            .map_err(|e| format!("could not save {name}: {e}"))?;
        let saved = format!("saved {}", path.display());
        let scale = self.scale;
        if scale.is_nan() || scale >= 1.0 {
            let image = tokio::task::spawn_blocking(move || image_block(&bytes, MAX_IMAGE_SIDE))
                .await
                .map_err(|e| e.to_string())??;
            return Ok(vec![Block::text(saved), image]);
        }
        let area = match clip {
            Some((_, _, width, height)) => (width, height),
            None => {
                // The window as the screenshot shows it, scrollbars included.
                match self.evaluate(&session, "[innerWidth, innerHeight]").await {
                    Ok(Value::Array(size)) if size.len() == 2 => {
                        (size[0].as_f64().unwrap_or(0.0), size[1].as_f64().unwrap_or(0.0))
                    }
                    _ => (0.0, 0.0),
                }
            }
        };
        let (image, frame) = tokio::task::spawn_blocking(move || scaled_shot(&bytes, area, scale))
            .await
            .map_err(|e| e.to_string())??;
        let note = if clip.is_some() {
            format!(
                "the image is {}x{}, scaled from {}x{} page pixels",
                frame.shown.0,
                frame.shown.1,
                area.0.round(),
                area.1.round()
            )
        } else {
            // Positions in a shot of the viewport are what click, hover and scroll are given next.
            let identity = frame.is_identity();
            let frames = &mut self.tabs[index].frames;
            if identity {
                frames.remove(&self.agent);
            } else {
                frames.insert(self.agent.clone(), frame);
            }
            if identity { String::new() } else { frame.note() }
        };
        let text = if note.is_empty() {
            saved
        } else {
            format!("{saved}\n{note}")
        };
        Ok(vec![Block::text(text), image])
    }

    /// Drops tabs whose page is gone and adopts pages the browser opened by itself (pop-ups, links
    /// opening a new tab) or the user opened, then lists them.
    async fn list_tabs(&mut self) -> Result<String, String> {
        let reply = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        // `(target, a page to work with, still blank)`
        let live: Vec<(String, bool, bool)> = page_targets(&reply)
            .into_iter()
            .map(|(id, url)| (id.to_string(), web_page(url), url == "about:blank"))
            .collect();
        self.tabs
            .retain(|tab| live.iter().any(|(id, ..)| id == &tab.target));
        for (target, web, blank) in &live {
            let known = self.tabs.iter().any(|tab| &tab.target == target);
            // The page the browser started with is kept for the first `open`, unless the user
            // went somewhere with it.
            let spare = self.spare.as_ref() == Some(target);
            if *web && !known && !(spare && *blank) {
                if spare {
                    self.spare = None;
                }
                self.attach(target, None).await?;
            }
        }
        let current = self.current.as_ref();
        if !current.is_some_and(|name| self.tabs.iter().any(|tab| &tab.name == name)) {
            self.current = self.tabs.last().map(|tab| tab.name.clone());
        }
        let mut out = String::new();
        for tab in &self.tabs {
            let marker = if self.current.as_deref() == Some(tab.name.as_str()) {
                "*"
            } else {
                " "
            };
            out.push_str(&format!(
                "{marker} {} · {}\n",
                tab.name,
                self.page_line(&tab.session).await
            ));
        }
        Ok(if out.is_empty() {
            "no tabs are open\n".to_string()
        } else {
            out
        })
    }

    async fn act(&mut self, input: &Input, artifacts: Option<&ArtifactStore>) -> Result<Vec<Block>, String> {
        let text = |text: String| Ok(vec![Block::text(text)]);
        let required = |value: &Option<String>, name: &str| {
            value
                .clone()
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("`{}` needs `{name}`", input.action))
        };
        match input.action.as_str() {
            "open" => {
                let url = required(&input.url, "url")?;
                let existing = input
                    .tab
                    .as_ref()
                    .and_then(|name| self.tabs.iter().position(|tab| &tab.name == name));
                let index = match existing {
                    Some(index) => index,
                    None => {
                        let target = self.new_target().await?;
                        self.attach(&target, input.tab.clone()).await?
                    }
                };
                self.current = Some(self.tabs[index].name.clone());
                let line = self
                    .go(&self.tabs[index].session, "Page.navigate", json!({ "url": url }))
                    .await?;
                let header = format!("tab {} · {line}\n", self.tabs[index].name);
                text(self.snapshot_taken(index, header).await?)
            }
            "tabs" => text(self.list_tabs().await?),
            "close" => {
                let index = self.tab(input.tab.as_deref())?;
                let tab = self.tabs.remove(index);
                self.cdp
                    .call(None, "Target.closeTarget", json!({ "targetId": tab.target }))
                    .await?;
                self.logs
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .remove(&tab.session);
                self.current = self.tabs.last().map(|tab| tab.name.clone());
                text(format!("closed {}", tab.name))
            }
            action => {
                let index = self.tab(input.tab.as_deref())?;
                let session = self.tabs[index].session.clone();
                match action {
                    "navigate" => {
                        let url = required(&input.url, "url")?;
                        text(self.go(&session, "Page.navigate", json!({ "url": url })).await?)
                    }
                    "reload" => text(self.go(&session, "Page.reload", json!({})).await?),
                    "back" => {
                        let history = self
                            .call(&session, "Page.getNavigationHistory", json!({}))
                            .await?;
                        let current =
                            history.get("currentIndex").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let previous = current
                            .checked_sub(1)
                            .and_then(|index| history.get("entries")?.get(index)?.get("id")?.as_i64())
                            .ok_or("there is no earlier page in this tab's history")?;
                        text(
                            self.go(
                                &session,
                                "Page.navigateToHistoryEntry",
                                json!({ "entryId": previous }),
                            )
                            .await?,
                        )
                    }
                    "snapshot" => text(self.snapshot_taken(index, String::new()).await?),
                    "click" | "hover" => {
                        let target = self
                            .target(index, input)
                            .await?
                            .ok_or(format!("`{action}` needs `ref`, `selector` or `x` and `y`"))?;
                        let (x, y) = self.point(&session, &target).await?;
                        self.mouse(&session, "mouseMoved", x, y, json!({})).await?;
                        if action == "click" {
                            let button = json!({ "button": "left", "clickCount": 1 });
                            self.mouse(&session, "mousePressed", x, y, button.clone()).await?;
                            self.mouse(&session, "mouseReleased", x, y, button).await?;
                        }
                        // Let what the click started (a navigation, a script) take hold.
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        let verb = if action == "click" { "clicked" } else { "hovering" };
                        text(format!(
                            "{verb} at {x:.0},{y:.0} · {}",
                            self.page_line(&session).await
                        ))
                    }
                    "type" => {
                        let typed = input.text.clone().ok_or("`type` needs `text`")?;
                        match self.target(index, input).await? {
                            Some(Target::Node(node)) => {
                                if self.call(&session, "DOM.focus", node.clone()).await.is_err() {
                                    let (x, y) = self.point(&session, &Target::Node(node)).await?;
                                    let button = json!({ "button": "left", "clickCount": 1 });
                                    self.mouse(&session, "mousePressed", x, y, button.clone()).await?;
                                    self.mouse(&session, "mouseReleased", x, y, button).await?;
                                }
                            }
                            Some(Target::Point(..)) => {
                                return Err("`type` takes `ref` or `selector`, not a point".into());
                            }
                            // Without a target the text goes to whatever has the focus.
                            None => {}
                        }
                        self.call(&session, "Input.insertText", json!({ "text": typed }))
                            .await?;
                        if input.submit {
                            self.press(&session, &page::parse_keys("Enter")?, Duration::ZERO)
                                .await?;
                        }
                        text(format!(
                            "typed {} characters{}",
                            typed.chars().count(),
                            if input.submit { " and pressed Enter" } else { "" }
                        ))
                    }
                    "key" => {
                        let keys = required(&input.keys, "keys")?;
                        let key = page::parse_keys(&keys)?;
                        self.press(&session, &key, Duration::from_millis(input.hold_ms.unwrap_or(0)))
                            .await?;
                        text(format!("pressed {keys}"))
                    }
                    "scroll" => {
                        let dy = input.dy.ok_or("`scroll` needs `dy`")?;
                        let (x, y) = match self.target(index, input).await? {
                            Some(target) => self.point(&session, &target).await?,
                            None => {
                                let viewport = self.viewport(&session).await?;
                                let half = |key: &str| {
                                    viewport.get(key).and_then(Value::as_f64).unwrap_or(0.0) / 2.0
                                };
                                (half("clientWidth"), half("clientHeight"))
                            }
                        };
                        self.mouse(&session, "mouseWheel", x, y, json!({ "deltaX": 0, "deltaY": dy }))
                            .await?;
                        text(format!("scrolled {dy:.0} at {x:.0},{y:.0}"))
                    }
                    "wait" => {
                        let condition = required(&input.wait_for, "for")?;
                        let limit = input.timeout_ms.map_or(DEFAULT_WAIT, Duration::from_millis);
                        let poll = input
                            .poll_ms
                            .map_or(DEFAULT_POLL, Duration::from_millis)
                            .max(MIN_POLL);
                        text(self.wait(&session, &condition, limit, poll).await?)
                    }
                    "eval" => {
                        let expression = required(&input.expression, "expression")?;
                        let value = self.evaluate(&session, &expression).await?;
                        let mut out =
                            serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
                        if out.chars().count() > MAX_EVAL {
                            out = out.chars().take(MAX_EVAL).collect::<String>() + "\n[result cut here]";
                        }
                        text(out)
                    }
                    "screenshot" => self.screenshot(index, input, artifacts).await,
                    "resize" => {
                        let (Some(width), Some(height)) = (input.width, input.height) else {
                            return Err("`resize` needs `width` and `height`".to_string());
                        };
                        if !(200..=8000).contains(&width) || !(200..=8000).contains(&height) {
                            return Err("`width` and `height` must be between 200 and 8000".to_string());
                        }
                        let metrics = json!({ "width": width, "height": height, "deviceScaleFactor": 1, "mobile": false });
                        self.call(&session, "Emulation.setDeviceMetricsOverride", metrics)
                            .await?;
                        // The last screenshot no longer shows the window as it is.
                        self.tabs[index].frames.clear();
                        text(format!("viewport is {width}×{height}"))
                    }
                    "console" => {
                        let logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
                        let lines: Vec<&str> = logs
                            .get(&session)
                            .into_iter()
                            .flat_map(|log| &log.console)
                            .map(String::as_str)
                            .collect();
                        text(if lines.is_empty() {
                            "the console is empty".to_string()
                        } else {
                            lines.join("\n")
                        })
                    }
                    "network" => {
                        let logs = self.logs.lock().unwrap_or_else(PoisonError::into_inner);
                        let filter = input.filter.as_deref().unwrap_or_default();
                        let lines: Vec<String> = logs
                            .get(&session)
                            .into_iter()
                            .flat_map(|log| &log.requests)
                            .filter(|request| request.url.contains(filter))
                            .map(|r| format!("{} {} {} {}", r.method, r.status, r.kind, r.url))
                            .collect();
                        text(if lines.is_empty() {
                            "no requests".to_string()
                        } else {
                            lines.join("\n")
                        })
                    }
                    other => Err(format!(
                        "unknown action `{other}`; actions: open, navigate, back, reload, snapshot, click, hover, type, \
                         key, scroll, wait, eval, screenshot, resize, console, network, tabs, close, fill_login, \
                         handoff"
                    )),
                }
            }
        }
    }
}

/// An `Input` for one of the actions the controls of the web page come to.
fn action_input(name: &str, url: Option<&str>, tab: Option<&str>) -> Input {
    Input {
        action: name.to_string(),
        url: url.map(str::to_string),
        tab: tab.map(str::to_string),
        ..Input::default()
    }
}

/// The browser of a session. Nothing runs until the first action.
pub struct BrowserSession {
    settings: Settings,
    state: tokio::sync::Mutex<Option<Running>>,
    /// Where the logins of sites are stored, when the session has any.
    logins: Option<Arc<ServiceStore>>,
    /// The passwords typed into pages so far: no result shows them.
    typed: Mutex<Vec<String>>,
    /// The snapshots calls returned, by call id, until [`BrowserSession::revise`] takes them.
    taken: Mutex<HashMap<String, Taken>>,
    /// What each agent was last shown of each tab.
    baselines: Baselines,
    /// The pictures of the tab being watched.
    feed: Arc<Feed>,
    /// What the browser is doing, for the clients that watch it.
    state_tx: watch::Sender<BrowserStateInfo>,
    /// How many clients watch: the screencast runs while there is one.
    watchers: AtomicU32,
    /// The client that has taken the browser over; the agents' actions wait while there is one.
    hold: Mutex<Option<Hold>>,
    /// How long a hold lasts without input from its client.
    lease: Duration,
    /// Told when a hold is set, for [`BrowserSession::keep_lease`].
    held: Notify,
    released: Notify,
    /// What the holder did, to tell the agent when it gets the browser back.
    acted: Mutex<UserActions>,
    /// The screencast that runs now.
    cast: tokio::sync::Mutex<Option<Cast>>,
}

/// Who has taken the browser over, and when it last did something with it.
struct Hold {
    holder: String,
    last: Moment,
}

/// A running screencast of one tab.
struct Cast {
    cdp: Arc<Cdp>,
    session: String,
    cancel: CancellationToken,
}

impl BrowserSession {
    pub fn new(settings: Settings) -> Self {
        BrowserSession {
            settings,
            state: tokio::sync::Mutex::new(None),
            logins: None,
            typed: Mutex::default(),
            taken: Mutex::default(),
            baselines: Baselines::default(),
            feed: Arc::new(Feed::new()),
            state_tx: watch::channel(BrowserStateInfo::default()).0,
            watchers: AtomicU32::new(0),
            hold: Mutex::new(None),
            lease: HOLD_LEASE,
            held: Notify::new(),
            released: Notify::new(),
            acted: Mutex::default(),
            cast: tokio::sync::Mutex::new(None),
        }
    }

    /// How long a client may hold the browser without input ([`HOLD_LEASE`] unless this says).
    pub fn with_lease(mut self, lease: Duration) -> Self {
        self.lease = lease;
        self
    }

    /// Lets `fill_login` type the site logins kept in `stored`.
    pub fn with_logins(mut self, stored: Arc<ServiceStore>) -> Self {
        self.logins = Some(stored);
        self
    }

    /// Carries out one action; actions run one at a time. `artifacts` is where screenshots go.
    pub async fn act(&self, input: Input, artifacts: Option<&ArtifactStore>) -> Result<Vec<Block>, String> {
        self.act_as("", None, input, artifacts).await
    }

    /// [`BrowserSession::act`] for call `call_id`, whose snapshot [`BrowserSession::revise`] may
    /// shorten afterwards.
    pub async fn act_as(
        &self,
        agent: &str,
        call_id: Option<&str>,
        input: Input,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Vec<Block>, String> {
        let done = self.perform(agent, call_id, input, artifacts).await;
        self.without_passwords(done)
    }

    /// Keeps the snapshot `running` took for `call_id`.
    fn keep_taken(&self, call_id: Option<&str>, running: &mut Running) {
        let taken = running.taken.take();
        if let (Some(id), Some(taken), true) = (call_id, taken, self.settings.snapshot_diff) {
            self.taken
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .insert(id.to_string(), taken);
        }
    }

    /// The result of call `call_id` of `agent` with its snapshot cut to what changed since the
    /// snapshot of the same page `agent` was shown last, when that one is still in its context
    /// (`live`). Called once per call, when its result is produced.
    pub fn revise(&self, agent: &str, call_id: &str, live: &dyn Fn(&str) -> bool, output: &mut [Block]) {
        let Some(taken) = self
            .taken
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(call_id)
        else {
            return;
        };
        let [Block::Text { text }] = output else {
            return;
        };
        let typed = self.typed.lock().unwrap_or_else(PoisonError::into_inner);
        // Between the call and now, hooks and `[tools] redact_output` may have changed the result;
        // a text rebuilt from the snapshot would bring back what they took out, so it stays as is.
        let produced = format!("{}{}", taken.header, taken.body);
        if *text != services::scrub(&produced, &typed) {
            return;
        }
        let revised = self.baselines.revise(
            (agent, &taken.target, &taken.url),
            (&taken.header, &taken.body),
            call_id,
            live,
        );
        *text = services::scrub(&revised, &typed).into_owned();
    }

    async fn perform(
        &self,
        agent: &str,
        call_id: Option<&str>,
        input: Input,
        artifacts: Option<&ArtifactStore>,
    ) -> Result<Vec<Block>, String> {
        let mut state = self.begin().await;
        // The user closed the window, or the browser crashed: start over.
        if state.as_ref().is_some_and(|running| running.cdp.is_closed()) {
            *state = None;
            self.settle(None).await;
        }
        if state.is_none() {
            match input.action.as_str() {
                "open" => *state = Some(Running::start(&self.settings, self.settings.headless).await?),
                "tabs" => return Ok(vec![Block::text("no tabs are open\n")]),
                _ => return Err(NO_TAB.to_string()),
            }
        }
        let Some(running) = state.as_mut() else {
            return Err(NO_TAB.to_string());
        };
        if input.action == "fill_login" {
            let stored = self
                .logins
                .as_ref()
                .map(|logins| logins.services())
                .unwrap_or_default();
            let (said, password) = running.fill_login(&input, &stored).await?;
            let mut typed = self.typed.lock().unwrap_or_else(PoisonError::into_inner);
            if !typed.contains(&password) {
                typed.push(password);
            }
            return Ok(vec![Block::text(said)]);
        }
        running.taken = None;
        running.agent = agent.to_string();
        let note = self.user_note();
        let done = running.act(&input, artifacts).await;
        let done = self.with_note(note, done, running);
        self.keep_taken(call_id, running);
        self.settle(state.as_ref()).await;
        done
    }

    /// What the user did in the browser since the agent last used it, as a line to put before the
    /// next result.
    fn user_note(&self) -> Option<String> {
        self.acted.lock().unwrap_or_else(PoisonError::into_inner).note()
    }

    /// `done` with `note` before its text, and the note forgotten; the snapshot `running` took
    /// gets it too, so that [`BrowserSession::revise`] still finds the result as it was produced.
    fn with_note(
        &self,
        note: Option<String>,
        done: Result<Vec<Block>, String>,
        running: &mut Running,
    ) -> Result<Vec<Block>, String> {
        let Some(note) = note else {
            return done;
        };
        // A failed action keeps the note for the next one.
        let mut blocks = done?;
        *self.acted.lock().unwrap_or_else(PoisonError::into_inner) = UserActions::default();
        match blocks.first_mut() {
            Some(Block::Text { text }) => *text = format!("{note}{text}"),
            _ => blocks.insert(0, Block::text(note.clone())),
        }
        if let Some(taken) = &mut running.taken {
            taken.header = format!("{note}{}", taken.header);
        }
        Ok(blocks)
    }

    /// `done` without the passwords `fill_login` typed, whatever the page did with them.
    fn without_passwords(&self, done: Result<Vec<Block>, String>) -> Result<Vec<Block>, String> {
        let typed = self.typed.lock().unwrap_or_else(PoisonError::into_inner);
        if typed.is_empty() {
            return done;
        }
        match done {
            Ok(mut blocks) => {
                for block in &mut blocks {
                    if let Block::Text { text } = block
                        && let std::borrow::Cow::Owned(clean) = services::scrub(text, &typed)
                    {
                        *text = clean;
                    }
                }
                Ok(blocks)
            }
            Err(why) => Err(services::scrub(&why, &typed).into_owned()),
        }
    }

    /// Starts the browser on the profile another one has just left. The one that left can hold
    /// the profile for a moment yet, and the new one then exits at once: it is started again
    /// after a pause, a few times, each pause twice the last.
    async fn start_again(&self, headless: bool) -> Result<Running, String> {
        let mut pause = Duration::from_millis(250);
        loop {
            match Running::start(&self.settings, headless).await {
                Err(why) if why.contains(LEFT_AT_ONCE) && pause <= RETRY_PAUSE => {
                    tokio::time::sleep(pause).await;
                    pause *= 2;
                }
                started => return started,
            }
        }
    }

    /// Hands the browser to the user: it is shown in a window, on the same profile and with the
    /// same tabs, `ui` asks the user to do what `reason` says, and when the user answers the
    /// agent gets the tabs and the page as they are then.
    pub async fn handoff(&self, input: &Input, ui: &dyn UiBridge) -> Result<Vec<Block>, String> {
        self.handoff_as("", None, input, ui).await
    }

    /// [`BrowserSession::handoff`] for call `call_id`; see [`BrowserSession::act_as`].
    pub async fn handoff_as(
        &self,
        agent: &str,
        call_id: Option<&str>,
        input: &Input,
        ui: &dyn UiBridge,
    ) -> Result<Vec<Block>, String> {
        let done = self.hand_over(agent, call_id, input, ui, false).await;
        self.without_passwords(done)
    }

    /// [`BrowserSession::handoff`], with the browser started again `headless` or not: a test
    /// has no screen to show a window on.
    async fn hand_over(
        &self,
        agent: &str,
        call_id: Option<&str>,
        input: &Input,
        ui: &dyn UiBridge,
        headless: bool,
    ) -> Result<Vec<Block>, String> {
        let reason = input
            .reason
            .as_deref()
            .map(str::trim)
            .filter(|reason| !reason.is_empty())
            .ok_or("`handoff` needs `reason`: what the user is asked to do in the browser")?;
        let mut state = self.begin().await;
        self.show(
            &mut state,
            headless,
            input.url.as_deref(),
            input.tab.as_deref(),
            agent,
        )
        .await?;
        self.settle(state.as_ref()).await;
        let running = state.as_mut().ok_or(NO_TAB)?;

        // A short first line: an interface with boxes makes it the title of the box.
        let prompt = format!(
            "Your turn in the browser\n{reason}\nFinish in the browser window, then press Enter here. What you \
             type goes to the agent."
        );
        let note = match ui.input(&prompt).await {
            Ok(note) => note,
            Err(UiError::NoUser) => {
                return Err("nobody is here to take over the browser".to_string());
            }
            Err(UiError::Cancelled) => {
                return Err("the user declined to take over the browser".to_string());
            }
        };
        let mut said = match note.trim() {
            "" => "the user finished in the browser".to_string(),
            note => format!("the user finished in the browser and says: {note}"),
        };
        // What the user did through the web page before this is told as well.
        if let Some(note) = self.user_note() {
            said = format!("{note}{said}");
            *self.acted.lock().unwrap_or_else(PoisonError::into_inner) = UserActions::default();
        }
        if running.cdp.is_closed() {
            *state = None;
            self.settle(None).await;
            return Ok(vec![Block::text(format!(
                "{said}\nthe browser was closed; `open` starts it again"
            ))]);
        }
        // The user may have opened, closed and moved between tabs.
        said.push('\n');
        said.push_str(&running.list_tabs().await?);
        if let Ok(index) = running.tab(None) {
            let line = running.page_line(&running.tabs[index].session).await;
            said.push_str(&format!("tab {} · {line}\n", running.tabs[index].name));
            running.taken = None;
            said = running.snapshot_taken(index, said).await?;
            self.keep_taken(call_id, running);
        }
        self.settle(state.as_ref()).await;
        Ok(vec![Block::text(said)])
    }

    /// Makes sure a browser with a window runs on the profile, with the tabs the agents had, and
    /// opens `url` in it when given. A browser without a window is closed and started again,
    /// which needs the profile to be free. `headless` is for tests, which have no screen.
    async fn show(
        &self,
        state: &mut Option<Running>,
        headless: bool,
        url: Option<&str>,
        tab: Option<&str>,
        agent: &str,
    ) -> Result<(), String> {
        let running = match state.take().filter(|running| !running.cdp.is_closed()) {
            Some(hidden) if hidden.headless => {
                let (pages, current) = (hidden.pages().await, hidden.current.clone());
                let mut left = hidden.close(LEAVE).await;
                // The next browser needs the profile: the whole process tree has to be gone.
                let _ = tokio::time::timeout(LEAVE, left.wait()).await;
                drop(left);
                let mut shown = self.start_again(headless).await?;
                shown.reopen(pages, current).await?;
                shown
            }
            Some(shown) => shown,
            None => Running::start(&self.settings, headless).await?,
        };
        let running = state.insert(running);
        if let Some(address) = url {
            let action = match (tab, &running.current) {
                (None, Some(_)) => "navigate",
                _ => "open",
            };
            running.agent = agent.to_string();
            running
                .act(&action_input(action, Some(address), tab), None)
                .await?;
        }
        if let Ok(index) = running.tab(tab) {
            let _ = running
                .call(&running.tabs[index].session, "Page.bringToFront", json!({}))
                .await;
        }
        Ok(())
    }

    /// Shows the browser in a window, on the agents' profile and with their tabs, for the user to
    /// sign in to sites; `url` is opened when given. The agents are not held and nobody is asked:
    /// the window stays until the session ends. Refused while a client other than `holder` has the
    /// browser: it may be started again, and the holder's page would go with it.
    pub async fn sign_in(&self, holder: &str, url: Option<&str>) -> Result<(), String> {
        self.sign_in_with(holder, url, false).await
    }

    async fn sign_in_with(&self, holder: &str, url: Option<&str>, headless: bool) -> Result<(), String> {
        if let Some(url) = url
            && page::host_of(url).is_none()
            && url != "about:blank"
        {
            return Err(format!("`{url}` is not an http or https URL"));
        }
        let mut state = self.state.lock().await;
        self.lapse_if_stale();
        if self.held_by().is_some_and(|other| other != holder) {
            return Err(ANOTHER_HOLDS.to_string());
        }
        self.show(&mut state, headless, url, None, "").await?;
        // The browser may be another one now: what the agents were shown is not what is there.
        if let Some(running) = state.as_mut() {
            running.taken = None;
        }
        self.baselines.forget_all();
        self.settle(state.as_ref()).await;
        Ok(())
    }

    /// The pictures of the tab being watched. Nothing is sent until someone has called
    /// [`BrowserSession::watch`].
    pub fn frames(&self) -> watch::Receiver<Option<Arc<LiveFrame>>> {
        self.feed.tx.subscribe()
    }

    /// What the browser is doing; changes when the tab, its address, the hold or whether a browser
    /// runs change.
    pub fn states(&self) -> watch::Receiver<BrowserStateInfo> {
        self.state_tx.subscribe()
    }

    /// One more client watches (`on`) or one fewer. The screencast runs on the current tab while
    /// there is a watcher and a browser.
    pub async fn watch(&self, on: bool) {
        let _ = self.watchers.try_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
            Some(if on {
                n.saturating_add(1)
            } else {
                n.saturating_sub(1)
            })
        });
        self.mark();
        let state = self.state.lock().await;
        self.settle(state.as_ref()).await;
    }

    /// Takes the browser over for `holder`: the agents' next browser action waits until
    /// [`BrowserSession::give_back`], or until `holder` has been silent for the lease. Fails while
    /// another holder has it, unless `force` takes it from them. An action that is running
    /// finishes; one that starts later sees the hold, because it reads the hold under the lock of
    /// the browser (see [`BrowserSession::begin`]).
    pub fn take(&self, holder: &str, force: bool) -> Result<(), String> {
        self.lapse_if_stale();
        let taken_from = {
            let mut hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
            let other = hold
                .as_ref()
                .filter(|hold| hold.holder != holder)
                .map(|hold| hold.holder.clone());
            if other.is_some() && !force {
                return Err(ANOTHER_HOLDS.to_string());
            }
            if other.is_some() || hold.is_none() {
                *hold = Some(Hold {
                    holder: holder.to_string(),
                    last: Moment::now(),
                });
            }
            other
        };
        self.touch(holder);
        self.mark();
        if taken_from.is_some() {
            // `held` stays true, but who holds changed: the clients are told again.
            self.state_tx.send_modify(|_| {});
        }
        self.held.notify_waiters();
        Ok(())
    }

    /// Whether `holder` has the browser.
    pub fn holds(&self, holder: &str) -> bool {
        self.lapse_if_stale();
        self.held_by().as_deref() == Some(holder)
    }

    /// Hands the browser back, if `holder` has it: the agents go on, their next snapshot is whole
    /// and their next result says what was done.
    pub fn give_back(&self, holder: &str) {
        {
            let mut hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
            if hold.as_ref().map(|hold| hold.holder.as_str()) != Some(holder) {
                return;
            }
            *hold = None;
        }
        self.baselines.forget_all();
        self.mark();
        self.released.notify_waiters();
    }

    /// The client that has the browser.
    fn held_by(&self) -> Option<String> {
        self.hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|hold| hold.holder.clone())
    }

    /// Notes that `holder` did something with the browser, which keeps its hold.
    fn touch(&self, holder: &str) {
        if let Some(hold) = self.hold.lock().unwrap_or_else(PoisonError::into_inner).as_mut()
            && hold.holder == holder
        {
            hold.last = Moment::now();
        }
    }

    /// When the hold ends if its client does nothing more; `None` when nobody has the browser.
    /// A hold that has run out is let go of first.
    fn held_until(&self) -> Option<Moment> {
        self.lapse_if_stale();
        self.hold
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .as_ref()
            .map(|hold| hold.last + self.lease)
    }

    /// Lets go of a hold whose client has done nothing for the length of the lease, and has the
    /// agents' next result say so.
    fn lapse_if_stale(&self) {
        {
            let mut hold = self.hold.lock().unwrap_or_else(PoisonError::into_inner);
            match &*hold {
                Some(held) if Moment::now() >= held.last + self.lease => *hold = None,
                _ => return,
            }
        }
        self.acted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .lapsed(self.lease);
        self.baselines.forget_all();
        self.mark();
        self.released.notify_waiters();
    }

    /// Lets go of every hold that runs out, for as long as it is polled: a device that went away
    /// without closing its connection is found out even when no agent waits. [`Browser`] in the
    /// command line spawns it with the session.
    pub async fn keep_lease(&self) {
        loop {
            let held = self.held.notified();
            tokio::pin!(held);
            held.as_mut().enable();
            match self.held_until() {
                None => held.await,
                Some(until) => tokio::select! {
                    () = &mut held => {}
                    () = tokio::time::sleep_until(until) => {}
                },
            }
        }
    }

    /// Waits until nobody has the browser, then takes the state. The hold is read again under the
    /// state's lock, which an action keeps while it runs: one that was waiting for the lock when a
    /// client took the browser over does not run.
    async fn begin(&self) -> tokio::sync::MutexGuard<'_, Option<Running>> {
        loop {
            self.wait_for_release().await;
            let state = self.state.lock().await;
            if self.held_until().is_none() {
                return state;
            }
        }
    }

    /// Does what `holder` did in the picture, in the tab shown. Fails unless `holder` has the
    /// browser.
    pub async fn user_input(&self, holder: &str, input: LiveInput) -> Result<(), String> {
        if !self.holds(holder) {
            return Err("take the browser over first".to_string());
        }
        self.touch(holder);
        let mut state = self.state.lock().await;
        // It may have been taken from `holder` while this waited for the browser.
        if !self.holds(holder) {
            return Err("take the browser over first".to_string());
        }
        let running = state
            .as_mut()
            .filter(|running| !running.cdp.is_closed())
            .ok_or("no browser is running; the agent has not opened a page yet")?;
        let tab = running.current_tab().ok_or(NO_TAB)?.name.clone();
        let index = running.tab(Some(&tab))?;
        let session = running.tabs[index].session.clone();
        let meta = self.feed.meta();
        let at = |x: f64, y: f64| {
            meta.map(|meta| meta.to_css(x, y))
                .ok_or_else(|| "the picture of the page has not arrived yet".to_string())
        };
        match &input {
            LiveInput::Click { x, y, button, count } => {
                let (x, y) = at(*x, *y)?;
                running.mouse(&session, "mouseMoved", x, y, json!({})).await?;
                for n in 1..=(*count).clamp(1, 3) {
                    let extra = json!({ "button": button.name(), "clickCount": n });
                    running
                        .mouse(&session, "mousePressed", x, y, extra.clone())
                        .await?;
                    running.mouse(&session, "mouseReleased", x, y, extra).await?;
                }
            }
            LiveInput::Scroll { x, y, dy } => {
                let (x, y) = at(*x, *y)?;
                let dy = if dy.is_finite() { *dy } else { 0.0 };
                running
                    .mouse(&session, "mouseWheel", x, y, json!({ "deltaX": 0, "deltaY": dy }))
                    .await?;
            }
            LiveInput::Key { keys } => {
                let key = page::parse_keys(keys)?;
                running.press(&session, &key, Duration::ZERO).await?;
            }
            LiveInput::Text { text } => {
                running
                    .call(&session, "Input.insertText", json!({ "text": text }))
                    .await?;
            }
            LiveInput::Navigate { url } => {
                if page::host_of(url).is_none() && url != "about:blank" {
                    return Err(format!("`{url}` is not an http or https URL"));
                }
                running
                    .act(&action_input("navigate", Some(url), Some(&tab)), None)
                    .await?;
            }
            LiveInput::Back => {
                running.act(&action_input("back", None, Some(&tab)), None).await?;
            }
            LiveInput::Reload => {
                running
                    .act(&action_input("reload", None, Some(&tab)), None)
                    .await?;
            }
        }
        self.acted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .record(&input);
        // Let what the input started (a script, a navigation) take hold before it is described.
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.touch(holder);
        self.settle(state.as_ref()).await;
        Ok(())
    }

    /// Waits while a client has taken the browser over, or until its lease runs out.
    async fn wait_for_release(&self) {
        loop {
            let released = self.released.notified();
            tokio::pin!(released);
            // Registered before the hold is read, so a release in between is not missed.
            released.as_mut().enable();
            let Some(until) = self.held_until() else {
                return;
            };
            tokio::select! {
                () = &mut released => {}
                () = tokio::time::sleep_until(until) => {}
            }
        }
    }

    /// Copies the hold and the watchers into the state the clients see.
    fn mark(&self) {
        let held = self.held_by().is_some();
        let watchers = self.watchers.load(Ordering::SeqCst);
        self.state_tx.send_if_modified(|state| {
            let changed = state.held != held || state.watchers != watchers;
            (state.held, state.watchers) = (held, watchers);
            changed
        });
    }

    /// Brings what the clients see up to date after the browser changed: its state, and the
    /// screencast, which follows the tab shown while someone watches.
    async fn settle(&self, running: Option<&Running>) {
        let watchers = self.watchers.load(Ordering::SeqCst);
        let running = running.filter(|running| !running.cdp.is_closed());
        match running {
            Some(running) => {
                if self.state_tx.receiver_count() > 0 {
                    let mut info = running.describe().await;
                    info.held = self.held_by().is_some();
                    info.watchers = watchers;
                    self.state_tx.send_if_modified(|state| {
                        let changed = *state != info;
                        *state = info;
                        changed
                    });
                }
            }
            None => {
                self.state_tx.send_if_modified(|state| {
                    let idle = BrowserStateInfo {
                        held: state.held,
                        watchers: state.watchers,
                        ..BrowserStateInfo::default()
                    };
                    let changed = *state != idle;
                    *state = idle;
                    changed
                });
            }
        }
        let wanted = running.filter(|_| watchers > 0).and_then(|running| {
            let tab = running.current_tab()?;
            Some((running.cdp.clone(), tab.session.clone(), tab.name.clone()))
        });
        let mut cast = self.cast.lock().await;
        let same = match (&*cast, &wanted) {
            (Some(cast), Some((cdp, session, _))) => Arc::ptr_eq(&cast.cdp, cdp) && &cast.session == session,
            (None, None) => true,
            _ => false,
        };
        if same {
            return;
        }
        if let Some(old) = cast.take() {
            old.cancel.cancel();
            let _ = old
                .cdp
                .call(Some(&old.session), "Page.stopScreencast", json!({}))
                .await;
            self.feed.clear();
        }
        let Some((cdp, session, tab)) = wanted else {
            return;
        };
        let live = self.settings.live.clamped();
        let cancel = CancellationToken::new();
        tokio::spawn(live::pump(
            cdp.clone(),
            cdp.subscribe(),
            (session.clone(), tab),
            self.feed.clone(),
            live.fps,
            cancel.clone(),
        ));
        let started = cdp
            .call(
                Some(&session),
                "Page.startScreencast",
                json!({
                    "format": "jpeg",
                    "quality": live.quality,
                    "maxWidth": live.max_width,
                    "maxHeight": live.max_width,
                    "everyNthFrame": 1,
                }),
            )
            .await;
        match started {
            Ok(_) => *cast = Some(Cast { cdp, session, cancel }),
            Err(why) => {
                cancel.cancel();
                tracing::debug!("the live view of the browser did not start: {why}");
            }
        }
    }

    /// Closes the browser if it runs and lets go of a hold. The next action starts a new browser.
    pub async fn shutdown(&self) {
        self.hold.lock().unwrap_or_else(PoisonError::into_inner).take();
        self.mark();
        self.released.notify_waiters();
        if let Some(old) = self.cast.lock().await.take() {
            old.cancel.cancel();
        }
        self.feed.clear();
        if let Some(running) = self.state.lock().await.take() {
            running.close(Duration::from_secs(2)).await;
        }
        self.settle(None).await;
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            session: Some("S".into()),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn a_result_changed_after_the_call_is_never_rebuilt_from_its_snapshot() {
        let session = BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: PathBuf::new(),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: crate::live::LiveSettings::default(),
        });
        let take = |call: &str, body: &str| {
            session.taken.lock().unwrap().insert(
                call.to_string(),
                Taken {
                    target: "T".into(),
                    url: "http://x/".into(),
                    header: "tab-1 · x\n".into(),
                    body: body.to_string(),
                },
            );
        };
        let live = |_: &str| true;
        let secret = "sk-ant-api03-SECRETSECRETSECRET";
        let hidden = "[redacted:anthropic key]";
        let first = "- heading \"Keys\"\n- button \"Copy\" [ref=e1]\n";
        take("c1", first);
        let mut output = vec![Block::text(format!("tab-1 · x\n{first}"))];
        session.revise("lead", "c1", &live, &mut output);
        // The next snapshot shows a key, which `[tools] redact_output` hid before shaping.
        let second = format!("{first}- text \"{secret}\"\n");
        take("c2", &second);
        let redacted = format!("tab-1 · x\n{}", second.replace(secret, hidden));
        let mut output = vec![Block::text(redacted.clone())];
        session.revise("lead", "c2", &live, &mut output);
        assert_eq!(text_of_blocks(&output), redacted);
        // A later snapshot with the key still on the page does not bring it back as a change.
        let third = format!("{second}- button \"Done\" [ref=e2]\n");
        take("c3", &third);
        let mut output = vec![Block::text(format!(
            "tab-1 · x\n{}",
            third.replace(secret, hidden)
        ))];
        session.revise("lead", "c3", &live, &mut output);
        let sent = text_of_blocks(&output);
        assert!(!sent.contains(secret) && sent.contains(hidden), "{sent}");
        // An unchanged result is still cut to what changed.
        let fourth = format!("{first}- link \"More\" [ref=e3]\n");
        take("c4", &fourth);
        let mut output = vec![Block::text(format!("tab-1 · x\n{fourth}"))];
        session.revise("lead", "c4", &live, &mut output);
        let sent = text_of_blocks(&output);
        assert!(!sent.contains(secret), "{sent}");
    }

    fn text_of_blocks(blocks: &[Block]) -> String {
        match blocks {
            [Block::Text { text }] => text.clone(),
            _ => panic!("{blocks:?}"),
        }
    }

    #[test]
    fn console_and_network_events_build_the_tab_log() {
        let logs = Logs::default();
        record(
            &logs,
            &event(
                "Runtime.consoleAPICalled",
                json!({ "type": "warning", "args": [
                    { "type": "string", "value": "slow:" },
                    { "type": "number", "value": 12 },
                    { "type": "object", "description": "Object" },
                ] }),
            ),
        );
        record(
            &logs,
            &event(
                "Runtime.exceptionThrown",
                json!({ "exceptionDetails": { "text": "Uncaught", "exception": { "description": "TypeError: x is null" } } }),
            ),
        );
        let request = |id: &str, url: &str| {
            event(
                "Network.requestWillBeSent",
                json!({ "requestId": id, "type": "Fetch", "request": { "method": "GET", "url": url } }),
            )
        };
        record(&logs, &request("1", "http://x/a"));
        record(&logs, &request("2", "http://x/b"));
        record(
            &logs,
            &event(
                "Network.responseReceived",
                json!({ "requestId": "1", "type": "XHR", "response": { "status": 204 } }),
            ),
        );
        record(
            &logs,
            &event("Network.loadingFinished", json!({ "requestId": "1" })),
        );
        record(
            &logs,
            &event(
                "Network.loadingFailed",
                json!({ "requestId": "2", "errorText": "net::ERR_FAILED" }),
            ),
        );
        // Events of the browser itself belong to no tab.
        record(
            &logs,
            &CdpEvent {
                session: None,
                ..request("3", "http://x/c")
            },
        );

        let logs = logs.lock().unwrap();
        assert_eq!(logs.len(), 1);
        let log = &logs["S"];
        assert_eq!(
            log.console,
            ["[warning] slow: 12 Object", "[exception] TypeError: x is null"]
        );
        let seen: Vec<(&str, &str, &str)> = log
            .requests
            .iter()
            .map(|r| (r.url.as_str(), r.status.as_str(), r.kind.as_str()))
            .collect();
        assert_eq!(
            seen,
            [
                ("http://x/a", "204", "XHR"),
                ("http://x/b", "failed: net::ERR_FAILED", "Fetch")
            ]
        );
        assert!(log.in_flight.is_empty());
    }

    fn idle_session() -> Arc<BrowserSession> {
        Arc::new(BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: PathBuf::new(),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: crate::live::LiveSettings::default(),
        }))
    }

    /// An action that needs no browser: it answers at once unless the hold stops it.
    fn tabs() -> Input {
        Input {
            action: "tabs".into(),
            ..Input::default()
        }
    }

    async fn pause() {
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    #[tokio::test]
    async fn an_action_queued_when_a_device_takes_the_browser_waits_for_the_hand_back() {
        let session = idle_session();
        // The action has passed the hold and waits for the browser's lock.
        let lock = session.state.lock().await;
        let acting = tokio::spawn({
            let session = session.clone();
            async move { session.act(tabs(), None).await }
        });
        pause().await;
        session.take("a", false).unwrap();
        drop(lock);
        pause().await;
        assert!(
            !acting.is_finished(),
            "the action ran while a device held the browser"
        );
        session.give_back("a");
        let done = tokio::time::timeout(Duration::from_secs(5), acting)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            text(done.unwrap()),
            "no tabs are open
"
        );
    }

    #[tokio::test]
    async fn a_hold_without_input_is_released_with_a_note_to_the_agent() {
        let session = Arc::new(
            Arc::into_inner(idle_session())
                .unwrap()
                .with_lease(Duration::from_millis(200)),
        );
        session.take("a", false).unwrap();
        assert!(session.holds("a"));
        let done = tokio::time::timeout(Duration::from_secs(5), session.act(tabs(), None))
            .await
            .expect("the agent is freed when the holder is silent");
        assert!(done.is_ok());
        assert!(!session.holds("a"));
        assert!(!session.states().borrow().held);
        let note = session.user_note().unwrap();
        assert!(note.contains("came back to you"), "{note}");
        assert_eq!(
            session.user_input("a", LiveInput::Back).await.unwrap_err(),
            "take the browser over first"
        );
    }

    #[tokio::test]
    async fn the_lease_keeper_tells_the_clients_when_a_hold_runs_out() {
        let session = Arc::new(
            Arc::into_inner(idle_session())
                .unwrap()
                .with_lease(Duration::from_millis(200)),
        );
        let keeper = tokio::spawn({
            let session = session.clone();
            async move { session.keep_lease().await }
        });
        let mut states = session.states();
        session.take("a", false).unwrap();
        tokio::time::timeout(Duration::from_secs(5), states.wait_for(|state| !state.held))
            .await
            .expect("the state says the hold ended")
            .unwrap();
        keeper.abort();
    }

    #[tokio::test]
    async fn another_device_takes_the_browser_only_by_force() {
        let session = idle_session();
        session.take("a", false).unwrap();
        assert_eq!(
            session.take("b", false).unwrap_err(),
            "another device has the browser"
        );
        assert!(session.holds("a"));
        session.take("b", true).unwrap();
        assert!(session.holds("b") && !session.holds("a"));
    }

    #[tokio::test]
    async fn shutdown_tells_the_clients_the_hold_is_gone() {
        let session = idle_session();
        session.take("a", false).unwrap();
        assert!(session.states().borrow().held);
        session.shutdown().await;
        assert!(!session.states().borrow().held);
    }

    #[tokio::test]
    async fn signing_in_is_refused_while_another_device_holds_the_browser() {
        let session = idle_session();
        session.take("a", false).unwrap();
        assert_eq!(
            session.sign_in("b", None).await.unwrap_err(),
            "another device has the browser"
        );
        assert!(session.holds("a"));
    }

    /// The user at the other end of a handoff: remembers what it was asked and answers `reply`.
    struct User {
        asked: Mutex<Vec<String>>,
        reply: Result<String, UiError>,
    }

    #[async_trait::async_trait]
    impl UiBridge for User {
        async fn ask(&self, _: &azazel_core::Question) -> Result<Vec<String>, UiError> {
            Err(UiError::NoUser)
        }

        async fn confirm(&self, _: &str) -> Result<bool, UiError> {
            Err(UiError::NoUser)
        }

        async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
            Err(UiError::NoUser)
        }

        async fn input(&self, prompt: &str) -> Result<String, UiError> {
            self.asked.lock().unwrap().push(prompt.to_string());
            self.reply.clone()
        }

        fn notify(&self, _: azazel_core::Level, _: &str) {}

        async fn approval(&self, _: azazel_core::ApprovalRequest) -> Option<azazel_core::ApprovalDecision> {
            None
        }
    }

    /// Serves a small page for every request and returns the address.
    async fn serve() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        const PAGE: &str = "<!doctype html><html><head><title>fixture</title></head>\
            <body><h1>Account</h1></body></html>";
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    fn text(blocks: Vec<Block>) -> String {
        match blocks.as_slice() {
            [Block::Text { text }] => text.clone(),
            other => panic!("{other:?}"),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_handoff_starts_the_browser_again_with_its_tabs_and_profile_and_returns_what_the_user_left() {
        if find_binary("", &|name| std::env::var(name).ok()).is_none() {
            eprintln!("skipped: no Chromium-based browser on this machine");
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let site = serve().await;
        let session = BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: dir.path().join("profile"),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: crate::live::LiveSettings::default(),
        });
        let browser = &session;
        let act = |input: Input| async move { text(browser.act(input, None).await.unwrap()) };
        let open = |path: &str, tab: &str| Input {
            action: "open".into(),
            url: Some(format!("{site}{path}")),
            tab: Some(tab.into()),
            ..Input::default()
        };
        let eval = |expression: &str, tab: &str| Input {
            action: "eval".into(),
            expression: Some(expression.into()),
            tab: Some(tab.into()),
            ..Input::default()
        };
        act(open("/first", "one")).await;
        act(eval(
            "document.cookie = 'seen=yes; max-age=3600'; window.before = 1",
            "one",
        ))
        .await;
        act(open("/second?page=2", "two")).await;

        let handoff = |reason: Option<&str>| Input {
            action: "handoff".into(),
            reason: reason.map(str::to_string),
            ..Input::default()
        };
        let user = User {
            asked: Mutex::default(),
            reply: Ok("  signed in as me ".into()),
        };
        let back = text(
            session
                .hand_over("", None, &handoff(Some("Sign in to the fixture")), &user, true)
                .await
                .unwrap(),
        );
        assert_eq!(
            *user.asked.lock().unwrap(),
            [
                "Your turn in the browser\nSign in to the fixture\nFinish in the browser window, then press \
              Enter here. What you type goes to the agent."
            ]
        );
        // The agent gets the user's words, the tabs as they are now and the page of its tab.
        let lines: Vec<&str> = back.lines().collect();
        assert_eq!(
            lines[..4],
            [
                "the user finished in the browser and says: signed in as me",
                &format!("  one · fixture · {site}/first"),
                &format!("* two · fixture · {site}/second?page=2"),
                &format!("tab two · fixture · {site}/second?page=2"),
            ],
            "{back}"
        );
        assert!(back.contains("heading \"Account\""), "{back}");
        // It is another browser on the same profile: the page was loaded again, the cookie kept.
        assert_eq!(
            act(eval("[typeof window.before, document.cookie]", "one")).await,
            "[\n  \"undefined\",\n  \"seen=yes\"\n]"
        );

        // A user who declines leaves the browser as it is, and the agent knows.
        let declining = User {
            asked: Mutex::default(),
            reply: Err(UiError::Cancelled),
        };
        assert_eq!(
            session
                .hand_over("", None, &handoff(Some("Solve the CAPTCHA")), &declining, true)
                .await
                .unwrap_err(),
            "the user declined to take over the browser"
        );
        assert!(
            session
                .hand_over("", None, &handoff(None), &user, true)
                .await
                .unwrap_err()
                .contains("needs `reason`")
        );
        assert_eq!(user.asked.lock().unwrap().len(), 1);
        let tabs = act(Input {
            action: "tabs".into(),
            ..Input::default()
        })
        .await;
        assert_eq!(tabs.lines().count(), 2, "{tabs}");
        session.shutdown().await;

        // A handoff can be the first thing a session does with the browser: `url` is where the
        // user starts.
        let fresh = BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: dir.path().join("other-profile"),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: crate::live::LiveSettings::default(),
        });
        let start = Input {
            url: Some(format!("{site}/start")),
            ..handoff(Some("Accept the terms"))
        };
        let back = text(fresh.hand_over("", None, &start, &user, true).await.unwrap());
        assert_eq!(
            back.lines().nth(1),
            Some(format!("* tab-1 · fixture · {site}/start").as_str()),
            "{back}"
        );
        fresh.shutdown().await;
    }
}
