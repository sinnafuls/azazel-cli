//! `/remote`: the session of this terminal, shared with browsers. The terminal stays the master:
//! what a browser sends arrives here as [`Event::Web`] and runs as if it had been typed.

use azazel_ai::ThinkingLevel;
use azazel_core::PermissionMode;
use azazel_core::config::RemoteAccess;
use azazel_remote::Access;
use azazel_tui::{Line, Role, Theme, text};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use super::overlay::{Action, Overlay};
use super::{Internal, Tui, view};
use crate::remote::terminal::{Asked, Event, Sharing, Way, open_tunnel, qr_rows};

const TITLE: &str = "Remote";

impl Tui {
    /// `/remote [view|tunnel|tailscale|share|stop]`.
    pub(super) async fn remote_command(&mut self, args: &str) {
        let args = args.trim();
        if args == "stop" {
            return match self.sharing.take() {
                Some(sharing) => {
                    sharing.stop().await;
                    self.info("the session is no longer shared; browsers are signed out");
                }
                None => self.info("the session is not shared"),
            };
        }
        let (way, access) = match args {
            "" => (Way::Local, Access::Control),
            "view" => (Way::Local, Access::View),
            "tunnel" => (Way::Cloudflare, Access::Control),
            "tailscale" => (Way::Tailscale, Access::Control),
            "share" => (self.configured_way(), Access::Control),
            _ => {
                return self.fail("/remote [view|tunnel|tailscale|share|stop]: see /help remote");
            }
        };
        if self.sharing.is_none() && !self.start_sharing().await {
            return;
        }
        let Some(sharing) = &self.sharing else {
            return;
        };
        if way == Way::Local {
            return self.show_link(access);
        }
        if sharing.tunnel.is_some() {
            return self.show_link(access);
        }
        let (paths, port, ask) = (self.env.paths.clone(), sharing.port(), sharing.tui_bridge());
        let done = self.internal_tx.clone();
        self.notice = Some("starting the tunnel…".to_string());
        tokio::spawn(async move {
            let tunnel = open_tunnel(paths, way, port, ask).await.map(Box::new);
            let _ = done.send(Internal::Remote(Event::Tunnel(tunnel, access)));
        });
    }

    fn configured_way(&self) -> Way {
        match self.env.config.remote.access {
            RemoteAccess::Cloudflare => Way::Cloudflare,
            RemoteAccess::Tailscale => Way::Tailscale,
        }
    }

    async fn start_sharing(&mut self) -> bool {
        let Some(local) = self.backend.local() else {
            self.here_only("/remote");
            return false;
        };
        let (web, mut from_web) = mpsc::unbounded_channel();
        match Sharing::start(&local.app, &local.active.lead, &local.active.session, web).await {
            Ok(sharing) => {
                let done = self.internal_tx.clone();
                tokio::spawn(async move {
                    while let Some(web) = from_web.recv().await {
                        if done.send(Internal::Remote(Event::Web(web))).is_err() {
                            break;
                        }
                    }
                });
                self.sharing = Some(sharing);
                if self.env.config.remote.full_access {
                    self.warn(crate::remote::FULL_ACCESS_WARNING);
                }
                true
            }
            Err(why) => {
                self.fail(&format!("/remote: {why}"));
                false
            }
        }
    }

    /// Opens the box with a new link. It is shown here, not in the scrollback, so that the
    /// token stays out of it.
    fn show_link(&mut self, access: Access) {
        let Some(sharing) = &self.sharing else {
            return;
        };
        let link = sharing.link(access);
        let kind = match access {
            Access::Control => "Control link (can answer and steer)",
            Access::View => "View link (can watch only)",
        };
        let head = format!("{kind}, works once, until {}:", link.expires);
        let urls = sharing.urls().join(" and ");
        let qr = qr_rows(&link.url, self.theme.glyphs());
        let url = link.url;
        let rows = Box::new(move |width: usize, theme: &Theme| -> Vec<Line> {
            let mut lines = vec![
                format!("Serving this session at {urls}"),
                String::new(),
                head.clone(),
            ];
            lines.extend(
                text::wrap_hard(&url, width.max(1))
                    .iter()
                    .map(|part| theme.paint(Role::Accent, part)),
            );
            lines.push(String::new());
            lines.extend(qr.iter().cloned());
            lines.push(String::new());
            lines.push(theme.paint(
                Role::Muted,
                "/remote view: a view link; /remote tunnel or tailscale: from a phone; /remote stop: end",
            ));
            lines
        });
        self.overlay = Some(Overlay::report(TITLE, rows, Action::Remote));
    }

    pub(super) async fn on_remote(&mut self, event: Event) {
        match event {
            Event::Tunnel(tunnel, access) => {
                self.notice = None;
                let Some(sharing) = &mut self.sharing else {
                    // Sharing ended while the tunnel came up: it stops with the value.
                    return;
                };
                match tunnel {
                    Ok(tunnel) => match sharing.set_public_url(&tunnel.url) {
                        Ok(()) => {
                            let notes = tunnel.notes.clone();
                            sharing.tunnel = Some(*tunnel);
                            for note in notes {
                                self.warn(&note);
                            }
                            self.show_link(access);
                        }
                        Err(why) => self.fail(&format!("/remote: {why}")),
                    },
                    Err(why) => self.fail(&format!("/remote: {why}")),
                }
            }
            Event::Web(web) => {
                let outcome = self.run_web(web.what).await;
                let _ = web.reply.send(outcome);
            }
        }
    }

    /// What a browser asked for, run as the terminal would run it typed.
    async fn run_web(&mut self, asked: Asked) -> Result<Value, String> {
        match asked {
            Asked::Prompt { text, images } | Asked::Steer { text, images } => {
                if !self.busy && !self.backend.has_model() {
                    return Err("no model is set up in this terminal yet".to_string());
                }
                let lines = view::user_prompt(&text, self.width(), &self.theme);
                self.say(lines);
                if self.busy {
                    self.backend.steer(text, images);
                } else {
                    self.backend.prompt(text, images).await?;
                }
                Ok(Value::Null)
            }
            Asked::FollowUp { text } => {
                let lines = view::user_prompt(&text, self.width(), &self.theme);
                self.say(lines);
                if self.busy {
                    self.queue_message(text, Vec::new());
                } else {
                    self.backend.prompt(text, Vec::new()).await?;
                }
                Ok(Value::Null)
            }
            Asked::Abort => {
                self.backend.abort();
                Ok(Value::Null)
            }
            Asked::SetMode(mode) => {
                let mode: PermissionMode = mode.parse()?;
                self.backend.set_mode(mode).await?;
                Ok(json!({ "mode": mode.to_string() }))
            }
            Asked::SetModel(chain) => {
                let set = self.backend.set_model(&chain).await?;
                Ok(json!({ "models": set.iter().map(ToString::to_string).collect::<Vec<_>>() }))
            }
            Asked::SetThinking(level) => {
                let level: ThinkingLevel = level.parse().map_err(|e| format!("{e}"))?;
                self.backend.set_thinking(level).await?;
                Ok(Value::Null)
            }
        }
    }

    /// Tells the browsers what was typed here.
    pub(super) fn share_input(&self, kind: &str, text: &str) {
        if let Some(sharing) = &self.sharing {
            sharing.feed.user_input(kind, text, None);
        }
    }

    /// The line that says the session is shared.
    pub(super) fn sharing_notice(&self, width: usize) -> Vec<Line> {
        let Some(sharing) = &self.sharing else {
            return Vec::new();
        };
        let n = sharing.clients();
        let text = format!(
            "shared on port {}: {n} browser{} attached; /remote stop ends it",
            sharing.port(),
            if n == 1 { "" } else { "s" }
        );
        view::notice(&text, width, &self.theme)
    }
}
