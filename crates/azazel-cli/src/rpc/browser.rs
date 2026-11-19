//! The live view of the agents' browser, for the clients of a session: `browser_watch`,
//! `browser_take`, `browser_input` and `browser_sign_in`, and the `browser_frame` and
//! `browser_state` frames.
//!
//! Three places serve clients and share this: a direct RPC session (one client, [`HOLDER`]), a
//! background session (`host.rs`, one holder per connection) and a terminal that shares its
//! session (`remote/terminal.rs`, likewise). Each names its clients by a holder id, which is what
//! the browser session keeps its hold under. The frames are pictures and are never written to a
//! log or kept for a client that attaches later: they go only to the clients that asked for them.
//!
//! Without the `browser` feature there is no [`Browser`] to be had, and the commands are answered
//! with [`missing`].

// Only the background host and the shared terminals (the `detach` feature) name the holders of
// several clients; a build without it serves the one client of a direct session.
#![cfg_attr(not(feature = "detach"), allow(dead_code))]

use std::sync::Arc;

use azazel_agent::Services;
use azazel_protocol::{BrowserState, Command, Frame};
use serde_json::Value;
use tokio::task::JoinHandle;

/// What answers a command, once it has been done.
pub(crate) type Reply = Box<dyn FnOnce(Result<Value, String>) + Send>;

/// The holder of a direct RPC client, which is the only one.
pub(crate) const HOLDER: &str = "rpc";

/// Whether `command` is one of the four that are served here.
pub(crate) fn is_browser(command: &Command) -> bool {
    matches!(
        command,
        Command::BrowserWatch { .. }
            | Command::BrowserTake { .. }
            | Command::BrowserInput { .. }
            | Command::BrowserSignIn { .. }
    )
}

/// Why a session cannot serve them.
pub(crate) fn missing() -> &'static str {
    if cfg!(feature = "browser") {
        "the browser is not available in this session"
    } else {
        "the browser is not part of this build"
    }
}

/// The name a connection holds the browser under.
pub(crate) fn holder_of(conn: u64) -> String {
    format!("conn-{conn}")
}

/// The line of a `browser_state` frame for a client that does (`held_by_you`) or does not have
/// the browser.
pub(crate) fn state_line(state: &BrowserState, held_by_you: bool) -> Option<String> {
    let mut state = state.clone();
    state.held_by_you = held_by_you;
    serde_json::to_string(&Frame::BrowserState(state)).ok()
}

/// Does `command` for `holder` without waiting for the browser: `reply` is called when it is
/// done, or at once when there is no browser.
pub(crate) fn dispatch(browser: Option<&Arc<Browser>>, holder: String, command: Command, reply: Reply) {
    match browser {
        Some(browser) => browser.run(holder, command, reply),
        None => reply(Err(missing().to_string())),
    }
}

#[cfg(feature = "browser")]
pub(crate) use real::Browser;

#[cfg(feature = "browser")]
mod real {
    use std::collections::HashSet;
    use std::sync::{Mutex, PoisonError};

    use azazel_browser::{BrowserSession, BrowserStateInfo, LiveButton, LiveInput};
    use azazel_protocol::{BrowserInput, MouseButton};
    use tokio::sync::{mpsc, watch};

    use super::*;

    enum Job {
        Do {
            holder: String,
            command: Command,
            reply: Reply,
        },
        Leave {
            holder: String,
        },
    }

    /// The browser session, as the clients of a session reach it.
    pub(crate) struct Browser {
        session: Arc<BrowserSession>,
        /// Commands that need the browser's lock, in the order they came: a click must not
        /// overtake the key before it, and a watcher that leaves must not overtake one that
        /// came.
        jobs: mpsc::UnboundedSender<Job>,
        /// The holders that watch.
        watching: Arc<Mutex<HashSet<String>>>,
        /// Counts the watchers that arrived: every pump sends the latest picture again when it
        /// changes, so that a new watcher gets one at once, even when the page is still.
        resend: watch::Sender<u64>,
    }

    impl Browser {
        /// The browser of the app, when this build and session have one.
        pub(crate) fn of(services: &Services) -> Option<Arc<Browser>> {
            let session = services.get::<BrowserSession>()?;
            let (jobs, queue) = mpsc::unbounded_channel();
            let watching = Arc::new(Mutex::new(HashSet::new()));
            let resend = watch::channel(0).0;
            tokio::spawn(work(session.clone(), watching.clone(), resend.clone(), queue));
            Some(Arc::new(Browser {
                session,
                jobs,
                watching,
                resend,
            }))
        }

        /// Whether `holder` watches.
        pub(crate) fn watching(&self, holder: &str) -> bool {
            self.watching
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .contains(holder)
        }

        /// Whether `holder` has taken the browser over.
        pub(crate) fn holds(&self, holder: &str) -> bool {
            self.session.holds(holder)
        }

        /// Does `command` for `holder` and answers with `reply`. Taking and handing back are done
        /// at once; the rest waits its turn, as it needs the browser itself.
        pub(crate) fn run(&self, holder: String, command: Command, reply: Reply) {
            if let Command::BrowserTake { on } = command {
                let outcome = if on {
                    self.session.take(&holder)
                } else {
                    self.session.give_back(&holder);
                    Ok(())
                };
                return reply(outcome.and_then(|()| state_data(&self.session, &holder)));
            }
            let _ = self.jobs.send(Job::Do {
                holder,
                command,
                reply,
            });
        }

        /// `holder` is gone: it stops watching and hands the browser back.
        pub(crate) fn leave(&self, holder: &str) {
            self.session.give_back(holder);
            let _ = self.jobs.send(Job::Leave {
                holder: holder.to_string(),
            });
        }

        /// Tells `holder`, through `send`, what the browser does: its state (the current one
        /// first) and, while it watches, the pictures. `send` is given a line and whether it is a
        /// picture, which a client that is behind may do without. Ends when the task is aborted.
        pub(crate) fn pump_to(
            self: &Arc<Self>,
            holder: String,
            send: impl Fn(String, bool) + Send + Sync + 'static,
        ) -> JoinHandle<()> {
            let browser = self.clone();
            let mut frames = self.session.frames();
            let mut states = self.session.states();
            let mut resend = self.resend.subscribe();
            tokio::spawn(async move {
                let state = |info: &BrowserStateInfo| {
                    let held_by_you = browser.holds(&holder);
                    if let Some(line) = state_line(&state_of(info, held_by_you), held_by_you) {
                        send(line, false);
                    }
                };
                let picture = |frame: Option<Arc<azazel_browser::LiveFrame>>| {
                    if let Some(frame) = frame
                        && browser.watching(&holder)
                        && let Ok(line) = serde_json::to_string(&Frame::BrowserFrame {
                            tab: frame.tab.clone(),
                            data: frame.data.clone(),
                            width: frame.width,
                            height: frame.height,
                            seq: frame.seq,
                        })
                    {
                        send(line, true);
                    }
                };
                state(&states.borrow_and_update().clone());
                loop {
                    tokio::select! {
                        changed = frames.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            picture(frames.borrow_and_update().clone());
                        }
                        changed = states.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            state(&states.borrow_and_update().clone());
                        }
                        changed = resend.changed() => {
                            if changed.is_err() {
                                return;
                            }
                            resend.borrow_and_update();
                            picture(frames.borrow().clone());
                        }
                    }
                }
            })
        }
    }

    /// What the browser is doing, as the protocol says it; `held_by_you` is left to the caller.
    fn state_of(info: &BrowserStateInfo, held_by_you: bool) -> BrowserState {
        BrowserState {
            running: info.running,
            visible: info.visible,
            tab: info.tab.clone(),
            url: info.url.clone(),
            title: info.title.clone(),
            held: info.held,
            held_by_you,
            watchers: info.watchers,
        }
    }

    /// The data of a response: the state of the browser for `holder`.
    fn state_data(session: &BrowserSession, holder: &str) -> Result<Value, String> {
        let info = session.states().borrow().clone();
        serde_json::to_value(state_of(&info, session.holds(holder))).map_err(|e| e.to_string())
    }

    /// A position in the picture, as a fraction of it.
    fn fraction(name: &str, value: f64) -> Result<f64, String> {
        if (0.0..=1.0).contains(&value) {
            Ok(value)
        } else {
            Err(format!(
                "`{name}` must be a fraction of the picture between 0 and 1"
            ))
        }
    }

    fn input_of(input: BrowserInput) -> Result<LiveInput, String> {
        Ok(match input {
            BrowserInput::Click { x, y, button, count } => LiveInput::Click {
                x: fraction("x", x)?,
                y: fraction("y", y)?,
                button: match button {
                    MouseButton::Left => LiveButton::Left,
                    MouseButton::Middle => LiveButton::Middle,
                    MouseButton::Right => LiveButton::Right,
                },
                count,
            },
            BrowserInput::Scroll { x, y, dy } => LiveInput::Scroll {
                x: fraction("x", x)?,
                y: fraction("y", y)?,
                dy,
            },
            BrowserInput::Key { keys } => LiveInput::Key { keys },
            BrowserInput::Text { text } => LiveInput::Text { text },
            BrowserInput::Navigate { url } => LiveInput::Navigate { url },
            BrowserInput::Back => LiveInput::Back,
            BrowserInput::Reload => LiveInput::Reload,
        })
    }

    async fn work(
        session: Arc<BrowserSession>,
        watching: Arc<Mutex<HashSet<String>>>,
        resend: watch::Sender<u64>,
        mut queue: mpsc::UnboundedReceiver<Job>,
    ) {
        let set = |holder: &str, on: bool| {
            let mut watching = watching.lock().unwrap_or_else(PoisonError::into_inner);
            if on {
                watching.insert(holder.to_string())
            } else {
                watching.remove(holder)
            }
        };
        while let Some(job) = queue.recv().await {
            match job {
                Job::Leave { holder } => {
                    if set(&holder, false) {
                        session.watch(false).await;
                    }
                }
                Job::Do {
                    holder,
                    command,
                    reply,
                } => {
                    let outcome = match command {
                        Command::BrowserWatch { on } => {
                            if set(&holder, on) {
                                session.watch(on).await;
                            }
                            if on {
                                resend.send_modify(|n| *n += 1);
                            }
                            Ok(())
                        }
                        Command::BrowserInput { input } => match input_of(input) {
                            Ok(input) => session.user_input(&holder, input).await,
                            Err(why) => Err(why),
                        },
                        Command::BrowserSignIn { url } => session.sign_in(url.as_deref()).await,
                        _ => Err("not a browser command".to_string()),
                    };
                    reply(outcome.and_then(|()| state_data(&session, &holder)));
                }
            }
        }
    }
}

/// Without the `browser` feature there is no browser: this never has a value, so that the code
/// that serves clients is the same in both builds.
#[cfg(not(feature = "browser"))]
pub(crate) enum Browser {}

#[cfg(not(feature = "browser"))]
impl Browser {
    pub(crate) fn of(_: &Services) -> Option<Arc<Browser>> {
        None
    }

    pub(crate) fn watching(&self, _: &str) -> bool {
        match *self {}
    }

    pub(crate) fn holds(&self, _: &str) -> bool {
        match *self {}
    }

    pub(crate) fn run(&self, _: String, _: Command, _: Reply) {
        match *self {}
    }

    pub(crate) fn leave(&self, _: &str) {
        match *self {}
    }

    pub(crate) fn pump_to(
        self: &Arc<Self>,
        _: String,
        _: impl Fn(String, bool) + Send + Sync + 'static,
    ) -> JoinHandle<()> {
        match **self {}
    }
}
