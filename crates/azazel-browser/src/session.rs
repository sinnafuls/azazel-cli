//! The browser of one session: started on first use, its tabs, and what each tool action does.

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use azazel_ai::services::{self, Service};
use azazel_ai::{Block, ServiceStore};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use azazel_core::{ArtifactStore, UiBridge, UiError};
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
/// What a field of a sign-in form is: the type of an `<input>`, or nothing for anything else.
const FIELD_KIND: &str = "function() { return this.tagName === 'INPUT' ? (this.type || 'text') : ''; }";
/// The kinds of field a user name is typed into.
const NAME_FIELDS: [&str; 6] = ["text", "email", "tel", "search", "url", "number"];
/// How long the browser gets to save its profile and leave before it is started again.
const LEAVE: Duration = Duration::from_secs(5);

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
        })
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

    /// Asks the browser to leave, so it saves its profile; the kill is for one that does not
    /// answer within `limit`.
    async fn close(mut self, limit: Duration) {
        let close = self.cdp.call(None, "Browser.close", json!({}));
        let _ = tokio::time::timeout(limit, close).await;
        let _ = tokio::time::timeout(limit, self.child.wait()).await;
        let _ = self.child.start_kill();
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
                         key, scroll, wait, eval, screenshot, resize, console, network, tabs, close, fill_login, \
                         handoff"
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
    /// Where the logins of sites are stored, when the session has any.
    logins: Option<Arc<ServiceStore>>,
    /// The passwords typed into pages so far: no result shows them.
    typed: Mutex<Vec<String>>,
}

impl BrowserSession {
    pub fn new(settings: Settings) -> Self {
        BrowserSession {
            settings,
            state: tokio::sync::Mutex::new(None),
            logins: None,
            typed: Mutex::default(),
        }
    }

    /// Lets `fill_login` type the site logins kept in `stored`.
    pub fn with_logins(mut self, stored: Arc<ServiceStore>) -> Self {
        self.logins = Some(stored);
        self
    }

    /// Carries out one action; actions run one at a time. `artifacts` is where screenshots go.
    pub async fn act(&self, input: Input, artifacts: Option<&ArtifactStore>) -> Result<Vec<Block>, String> {
        let done = self.perform(input, artifacts).await;
        self.without_passwords(done)
    }

    async fn perform(&self, input: Input, artifacts: Option<&ArtifactStore>) -> Result<Vec<Block>, String> {
        let mut state = self.state.lock().await;
        // The user closed the window, or the browser crashed: start over.
        if state.as_ref().is_some_and(|running| running.cdp.is_closed()) {
            *state = None;
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
        running.act(&input, artifacts).await
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

    /// Hands the browser to the user: it is shown in a window, on the same profile and with the
    /// same tabs, `ui` asks the user to do what `reason` says, and when the user answers the
    /// agent gets the tabs and the page as they are then.
    pub async fn handoff(&self, input: &Input, ui: &dyn UiBridge) -> Result<Vec<Block>, String> {
        let done = self.hand_over(input, ui, false).await;
        self.without_passwords(done)
    }

    /// [`BrowserSession::handoff`], with the browser started again `headless` or not: a test
    /// has no screen to show a window on.
    async fn hand_over(
        &self,
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
        let mut state = self.state.lock().await;
        let running = match state.take().filter(|running| !running.cdp.is_closed()) {
            Some(hidden) if hidden.headless => {
                let (pages, current) = (hidden.pages().await, hidden.current.clone());
                hidden.close(LEAVE).await;
                let mut shown = match Running::start(&self.settings, headless).await {
                    Ok(shown) => shown,
                    // The browser that left may still hold the profile for a moment.
                    Err(_) => {
                        tokio::time::sleep(Duration::from_secs(1)).await;
                        Running::start(&self.settings, headless).await?
                    }
                };
                shown.reopen(pages, current).await?;
                shown
            }
            Some(shown) => shown,
            None => Running::start(&self.settings, headless).await?,
        };
        let running = state.insert(running);
        if let Some(address) = &input.url {
            let action = match (&input.tab, &running.current) {
                (None, Some(_)) => "navigate",
                _ => "open",
            };
            let go = Input {
                action: action.to_string(),
                url: Some(address.clone()),
                tab: input.tab.clone(),
                ..Input::default()
            };
            running.act(&go, None).await?;
        }
        if let Ok(index) = running.tab(input.tab.as_deref()) {
            let _ = running
                .call(&running.tabs[index].session, "Page.bringToFront", json!({}))
                .await;
        }

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
        if running.cdp.is_closed() {
            *state = None;
            return Ok(vec![Block::text(format!(
                "{said}\nthe browser was closed; `open` starts it again"
            ))]);
        }
        // The user may have opened, closed and moved between tabs.
        said.push('\n');
        said.push_str(&running.list_tabs().await?);
        if let Ok(index) = running.tab(None) {
            let line = running.page_line(&running.tabs[index].session).await;
            let snapshot = running.snapshot(index).await?;
            said.push_str(&format!("tab {} · {line}\n{snapshot}", running.tabs[index].name));
        }
        Ok(vec![Block::text(said)])
    }

    /// Closes the browser if it runs. The next action starts a new one.
    pub async fn shutdown(&self) {
        if let Some(running) = self.state.lock().await.take() {
            running.close(Duration::from_secs(2)).await;
        }
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
                .hand_over(&handoff(Some("Sign in to the fixture")), &user, true)
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
                .hand_over(&handoff(Some("Solve the CAPTCHA")), &declining, true)
                .await
                .unwrap_err(),
            "the user declined to take over the browser"
        );
        assert!(
            session
                .hand_over(&handoff(None), &user, true)
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
        });
        let start = Input {
            url: Some(format!("{site}/start")),
            ..handoff(Some("Accept the terms"))
        };
        let back = text(fresh.hand_over(&start, &user, true).await.unwrap());
        assert_eq!(
            back.lines().nth(1),
            Some(format!("* tab-1 · fixture · {site}/start").as_str()),
            "{back}"
        );
        fresh.shutdown().await;
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
