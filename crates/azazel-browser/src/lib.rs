//! Browser automation over the Chrome DevTools Protocol, plus screen capture and input injection.

pub mod cdp;
pub mod launch;
pub mod live;
pub mod page;
#[cfg(feature = "screen")]
pub mod screen;
pub mod session;

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Access, ShapeCtx, Tool, ToolCtx, ToolOutput};
use azazel_ai::ServiceStore;
use azazel_core::{
    ArtifactStore, Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar, Ui,
};
use serde_json::{Value, json};

pub use launch::{NOT_FOUND, find_binary, open_profile};
pub use live::{BrowserStateInfo, LiveButton, LiveFrame, LiveInput, LiveSettings};
pub use session::{BrowserSession, Input, Settings};

const DESCRIPTION: &str = "Drive a real Chromium browser: open pages, read them as an accessibility \
snapshot, click, type, press keys, run JavaScript, take screenshots, and read the console and network \
log. Start with `open`; act on elements by the `ref` shown in the latest snapshot (refs change with \
every snapshot), by CSS `selector`, or by `x`,`y`. Use it to check a web app the way a user would. To \
sign in, use `fill_login`: it types the login the user stored for the page, which you never see; when a \
page needs the user in person (a login that is not stored, a second factor, a CAPTCHA), use `handoff`.";

/// The `browser` tool.
pub struct BrowserTool(pub Arc<BrowserSession>);

#[async_trait]
impl Tool for BrowserTool {
    fn name(&self) -> &str {
        "browser"
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": [
                        "open", "navigate", "back", "reload", "snapshot", "click", "hover", "type", "key",
                        "scroll", "wait", "eval", "screenshot", "resize", "console", "network", "tabs", "close",
                        "fill_login", "handoff"
                    ],
                    "description": "open: new tab at `url`, returns title, URL and a snapshot. \
                        navigate/back/reload: move the tab. snapshot: the page as `- role \"name\" [ref=eN]` lines. \
                        click/hover: `ref`, `selector` or `x`,`y`. type: `text` into `ref` or `selector` \
                        (the focused element when neither is given), `submit` presses Enter. key: `keys`. \
                        scroll: `dy` pixels, at `ref` when given. wait: until `for` holds, so the next action \
                        meets a settled page. eval: `expression`. \
                        screenshot: the viewport, the `full_page`, or one element. resize: set the tab's \
                        viewport to `width`×`height` CSS pixels (responsive checks). console/network: the \
                        last 100 entries. tabs: list. close: close a tab. fill_login: type the login stored \
                        for this page into the fields `ref_user` and `ref_password` (either or both; `submit` \
                        presses Enter); nothing is typed on a page the login is not stored for. handoff: show \
                        the browser to the user with `reason`, wait until they have finished there, and get \
                        the page back; `url` opens a page for them first."
                },
                "url": { "type": "string", "description": "http(s) URL for open, navigate and handoff" },
                "tab": { "type": "string", "description": "Tab name; defaults to the tab used last. With open, names the new tab" },
                "ref": { "type": "string", "description": "Element ref from the latest snapshot, e.g. e12" },
                "selector": { "type": "string", "description": "CSS selector, as an alternative to ref" },
                "x": { "type": "number", "description": "Viewport x in CSS pixels" },
                "y": { "type": "number", "description": "Viewport y in CSS pixels" },
                "text": { "type": "string", "description": "Text to type" },
                "submit": { "type": "boolean", "description": "Press Enter after typing" },
                "keys": { "type": "string", "description": "One key with optional modifiers: Enter, Escape, ArrowDown, a, Control+A, Shift+Tab" },
                "hold_ms": { "type": "integer", "minimum": 0, "description": "How long to hold the key down" },
                "dy": { "type": "number", "description": "Pixels to scroll; positive is down" },
                "for": { "type": "string", "description": "What wait waits for: load, idle (no network activity), a CSS selector, text:<string>, role:<role> \"<name>\" (as a snapshot line shows it; the name is optional), url:contains:<string>, title:contains:<string>, or gone:<selector> (no element matches any more)" },
                "timeout_ms": { "type": "integer", "minimum": 0, "description": "Limit for wait (default 10000)" },
                "poll_ms": { "type": "integer", "minimum": 10, "description": "How often wait looks at the page (default 100)" },
                "expression": { "type": "string", "description": "JavaScript; promises are awaited; the value comes back as JSON" },
                "full_page": { "type": "boolean", "description": "Capture the whole page instead of the viewport" },
                "width": { "type": "integer", "minimum": 200, "maximum": 8000, "description": "Viewport width for resize" },
                "height": { "type": "integer", "minimum": 200, "maximum": 8000, "description": "Viewport height for resize" },
                "filter": { "type": "string", "description": "Substring of the URL, for network" },
                "ref_user": { "type": "string", "description": "Ref of the user name or e-mail field, for fill_login" },
                "ref_password": { "type": "string", "description": "Ref of the password field, for fill_login" },
                "reason": { "type": "string", "description": "What the user is asked to do in the browser, for handoff" }
            },
            "required": ["action"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Exec
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        let action = input.get("action").and_then(Value::as_str).unwrap_or_default();
        let host = input.get("url").and_then(Value::as_str).and_then(page::host_of);
        match (action, host) {
            ("open" | "navigate", Some(host)) => vec![format!("domain:{host}")],
            _ => vec![action.to_string()],
        }
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: Input = match serde_json::from_value(input) {
            Ok(input) => input,
            Err(e) => return ToolOutput::error(format!("invalid input: {e}")),
        };
        if matches!(input.action.as_str(), "open" | "navigate" | "handoff")
            && let Some(url) = &input.url
            && page::host_of(url).is_none()
            && url != "about:blank"
        {
            return ToolOutput::error(format!("`{url}` is not an http or https URL"));
        }
        let artifacts = ctx.services.get::<ArtifactStore>();
        let act = async {
            if input.action != "handoff" {
                return self
                    .0
                    .act_as(&ctx.agent_id, Some(&ctx.call_id), input, artifacts.as_deref())
                    .await;
            }
            match ctx.services.get::<Ui>() {
                Some(ui) => {
                    self.0
                        .handoff_as(&ctx.agent_id, Some(&ctx.call_id), &input, ui.0.as_ref())
                        .await
                }
                None => Err("nobody is here to take over the browser".to_string()),
            }
        };
        let result = tokio::select! {
            result = act => result,
            _ = ctx.cancel.cancelled() => Err(ABORTED_TEXT.to_string()),
        };
        match result {
            Ok(content) => ToolOutput {
                content,
                is_error: false,
                details: None,
            },
            Err(why) => ToolOutput::error(why),
        }
    }

    fn shape(&self, _input: &Value, output: &mut ToolOutput, ctx: &ShapeCtx<'_>) {
        self.0
            .revise(ctx.agent_id, ctx.call_id, ctx.in_context, &mut output.content);
    }
}

struct CloseAtSessionEnd(Arc<BrowserSession>);

#[async_trait]
impl EventHandler for CloseAtSessionEnd {
    async fn handle(&self, _: &mut Event, _: &ExtCtx) -> Outcome {
        self.0.shutdown().await;
        Outcome::Continue
    }
}

/// Registers the `browser` tool. The browser itself starts with the first `open` and is closed
/// when the session ends.
pub struct BrowserExt {
    /// `[browser] path`.
    pub path: String,
    /// `[browser] headless`.
    pub headless: bool,
    /// The browser's user data directory.
    pub profile: PathBuf,
    /// `[browser] screenshot_scale`.
    pub screenshot_scale: f64,
    /// `[browser] snapshot_diff`.
    pub snapshot_diff: bool,
    /// `[browser] live_fps`, `live_quality` and `live_max_width`.
    pub live: LiveSettings,
    /// Where the logins `fill_login` types are stored.
    pub logins: Arc<ServiceStore>,
}

impl Extension for BrowserExt {
    fn id(&self) -> &str {
        "browser"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let settings = Settings {
            path: self.path.clone(),
            headless: self.headless,
            profile: self.profile.clone(),
            screenshot_scale: self.screenshot_scale,
            snapshot_diff: self.snapshot_diff,
            live: self.live,
        };
        let session = Arc::new(BrowserSession::new(settings).with_logins(self.logins.clone()));
        r.service(session.clone());
        r.tool(Arc::new(BrowserTool(session.clone())));
        r.on(EventKind::SessionEnd, Arc::new(CloseAtSessionEnd(session)));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn permission_subjects_are_the_site_for_navigation_and_the_action_otherwise() {
        let tool = BrowserTool(Arc::new(BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: PathBuf::from("unused"),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: LiveSettings::default(),
        })));
        let subjects = |input: Value| tool.subjects(&input);
        assert_eq!(
            subjects(json!({ "action": "open", "url": "https://Docs.rs/tokio" })),
            ["domain:docs.rs"]
        );
        assert_eq!(
            subjects(json!({ "action": "navigate", "url": "http://localhost:5173/" })),
            ["domain:localhost"]
        );
        assert_eq!(subjects(json!({ "action": "click", "ref": "e3" })), ["click"]);
        // A URL on an action that does not load it grants nothing.
        assert_eq!(
            subjects(json!({ "action": "eval", "url": "https://docs.rs" })),
            ["eval"]
        );
        assert_eq!(
            subjects(json!({ "action": "open", "url": "file:///etc/passwd" })),
            ["open"]
        );
        // Typing a stored login and handing the browser over are asked about as themselves.
        assert_eq!(
            subjects(json!({ "action": "fill_login", "ref_password": "e4" })),
            ["fill_login"]
        );
        assert_eq!(
            subjects(json!({ "action": "handoff", "reason": "sign in", "url": "https://docs.rs" })),
            ["handoff"]
        );
    }
}
