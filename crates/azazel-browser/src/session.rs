//! The browser of one session: started on first use, its tabs, and what each tool action does.

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azazel_ai::Block;
use azazel_core::ArtifactStore;
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use base64::Engine;
use process_wrap::tokio::ChildWrapper;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::broadcast::{Receiver, error::RecvError};

use crate::cdp::{CLOSED, Cdp, CdpEvent};
use crate::launch::{NOT_FOUND, find_binary, launch};
use crate::page::{self, Condition, Key};

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

#[derive(Debug, Clone)]
pub struct Settings {
    /// `[browser] path`; empty to look for an installed browser.
    pub path: String,
    pub headless: bool,
    /// The user data directory the browser runs on.
    pub profile: PathBuf,
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

impl Running {
    async fn start(settings: &Settings) -> Result<Running, String> {
        let binary = find_binary(&settings.path, &|name| std::env::var(name).ok())
            .ok_or_else(|| NOT_FOUND.to_string())?;
        let launched = launch(&binary, &settings.profile, settings.headless).await?;
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
        })
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
        let shot = page::snapshot(&nodes);
        self.tabs[index].refs = shot.refs;
        Ok(if shot.text.is_empty() {
            "(the page has no accessible content)\n".to_string()
        } else {
            shot.text
        })
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
            (Some(x), Some(y)) => Ok(Some(Target::Point(x, y))),
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
        let image = tokio::task::spawn_blocking(move || image_block(&bytes, MAX_IMAGE_SIDE))
            .await
            .map_err(|e| e.to_string())??;
        Ok(vec![Block::text(format!("saved {}", path.display())), image])
    }

    /// Drops tabs whose page is gone and adopts pages the browser opened by itself (pop-ups, links
    /// opening a new tab), then lists them.
    async fn list_tabs(&mut self) -> Result<String, String> {
        let reply = self.cdp.call(None, "Target.getTargets", json!({})).await?;
        let live: Vec<(String, bool)> = page_targets(&reply)
            .into_iter()
            .map(|(id, url)| (id.to_string(), web_page(url)))
            .collect();
        self.tabs
            .retain(|tab| live.iter().any(|(id, _)| id == &tab.target));
        for (target, web) in &live {
            let known = self.tabs.iter().any(|tab| &tab.target == target);
            if *web && !known && self.spare.as_ref() != Some(target) {
                self.attach(target, None).await?;
            }
        }
        if self
            .current
            .as_ref()
            .is_some_and(|name| !self.tabs.iter().any(|tab| &tab.name == name))
        {
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
                        let target = match self.spare.take() {
                            Some(target) => target,
                            None => {
                                let created = self
                                    .cdp
                                    .call(None, "Target.createTarget", json!({ "url": "about:blank" }))
                                    .await?;
                                text_of(&created, &["targetId"]).to_string()
                            }
                        };
                        self.attach(&target, input.tab.clone()).await?
                    }
                };
                self.current = Some(self.tabs[index].name.clone());
                let line = self
                    .go(&self.tabs[index].session, "Page.navigate", json!({ "url": url }))
                    .await?;
                let snapshot = self.snapshot(index).await?;
                text(format!("tab {} · {line}\n{snapshot}", self.tabs[index].name))
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
                    "snapshot" => text(self.snapshot(index).await?),
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
                         key, scroll, wait, eval, screenshot, resize, console, network, tabs, close"
                    )),
                }
            }
        }
    }
}

/// The browser of a session. Nothing runs until the first action.
pub struct BrowserSession {
    settings: Settings,
    state: tokio::sync::Mutex<Option<Running>>,
}

impl BrowserSession {
    pub fn new(settings: Settings) -> Self {
        BrowserSession {
            settings,
            state: tokio::sync::Mutex::new(None),
        }
    }

    /// Carries out one action; actions run one at a time. `artifacts` is where screenshots go.
    pub async fn act(&self, input: Input, artifacts: Option<&ArtifactStore>) -> Result<Vec<Block>, String> {
        let mut state = self.state.lock().await;
        // The user closed the window, or the browser crashed: start over.
        if state.as_ref().is_some_and(|running| running.cdp.is_closed()) {
            *state = None;
        }
        if state.is_none() {
            match input.action.as_str() {
                "open" => *state = Some(Running::start(&self.settings).await?),
                "tabs" => return Ok(vec![Block::text("no tabs are open\n")]),
                _ => return Err(NO_TAB.to_string()),
            }
        }
        match state.as_mut() {
            Some(running) => running.act(&input, artifacts).await,
            None => Err(NO_TAB.to_string()),
        }
    }

    /// Closes the browser if it runs. The next action starts a new one.
    pub async fn shutdown(&self) {
        let Some(mut running) = self.state.lock().await.take() else {
            return;
        };
        // Asked nicely the browser saves its profile; the kill is for one that does not answer.
        let close = running.cdp.call(None, "Browser.close", json!({}));
        let _ = tokio::time::timeout(Duration::from_secs(2), close).await;
        let _ = tokio::time::timeout(Duration::from_secs(2), running.child.wait()).await;
        let _ = running.child.start_kill();
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn event(method: &str, params: Value) -> CdpEvent {
        CdpEvent {
            session: Some("S".into()),
            method: method.into(),
            params,
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

    #[test]
    fn the_logs_keep_only_the_latest_entries() {
        let logs = Logs::default();
        for i in 0..LOG_LINES + 5 {
            record(
                &logs,
                &event(
                    "Runtime.consoleAPICalled",
                    json!({ "type": "log", "args": [{ "value": i }] }),
                ),
            );
        }
        let logs = logs.lock().unwrap();
        assert_eq!(logs["S"].console.len(), LOG_LINES);
        assert_eq!(logs["S"].console.front().map(String::as_str), Some("[log] 5"));
    }

    #[tokio::test]
    async fn actions_before_a_page_is_open_say_what_to_do() {
        let dir = tempfile::tempdir().unwrap();
        let session = BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: dir.path().join("profile"),
        });
        let click = Input {
            action: "click".into(),
            reference: Some("e1".into()),
            ..Input::default()
        };
        assert_eq!(session.act(click, None).await.unwrap_err(), NO_TAB);
        let tabs = Input {
            action: "tabs".into(),
            ..Input::default()
        };
        assert_eq!(
            session.act(tabs, None).await.unwrap(),
            [Block::text("no tabs are open\n")]
        );
        // Nothing was started, so there is no profile directory and nothing to shut down.
        session.shutdown().await;
        assert!(!dir.path().join("profile").exists());
    }
}
