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
// several clients, and only a build with the `browser` feature has a browser to serve them; a
// build without either serves the one client of a direct session, or none.
#![cfg_attr(not(all(feature = "detach", feature = "browser")), allow(dead_code))]

use std::sync::{Arc, Mutex, PoisonError, Weak};

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

/// The start of a `browser_frame` line, as the protocol writes it.
pub(crate) const PICTURE: &str = r#"{"type":"browser_frame""#;

/// The pictures of the browser on their way to one client, at most one at a time: a client
/// that reads slowly is sent no more until the last one is out, instead of the pictures piling
/// up in an unbounded queue. A picture that arrives meanwhile is kept, the newest of them only,
/// and goes out when the writer is done, so that a client does not stay on an old picture of a
/// page that has stopped changing.
#[derive(Default)]
pub(crate) struct Pictures {
    state: Mutex<Waiting>,
    /// Where a kept picture goes; weak, so that a client that went is not kept alive by it.
    send: Mutex<Option<Weak<Sender>>>,
}

/// What sends a line to a client.
type Sender = dyn Fn(String) + Send + Sync;

#[derive(Default)]
struct Waiting {
    /// A picture is written or queued for writing.
    in_flight: bool,
    /// The newest picture that arrived meanwhile.
    latest: Option<String>,
}

impl Pictures {
    /// A new picture: handed back when it may be sent now, kept when one is still in flight.
    pub(crate) fn offer(&self, line: String) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if state.in_flight {
            state.latest = Some(line);
            None
        } else {
            state.in_flight = true;
            Some(line)
        }
    }

    /// The picture in flight is written: the kept one, to be sent next, if any.
    pub(crate) fn finished(&self) -> Option<String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let next = state.latest.take();
        state.in_flight = next.is_some();
        next
    }
}

/// Lets one picture wait to be written to a client at a time; see [`Pictures`]. The writer calls
/// [`written`] for every line it has written.
pub(crate) fn one_picture_in_flight(
    pictures: Arc<Pictures>,
    send: impl Fn(String) + Send + Sync + 'static,
) -> impl Fn(String, bool) + Send + Sync + 'static {
    let send: Arc<dyn Fn(String) + Send + Sync> = Arc::new(send);
    *pictures.send.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::downgrade(&send));
    move |line, picture| {
        if picture {
            if let Some(line) = pictures.offer(line) {
                send(line);
            }
        } else {
            send(line);
        }
    }
}

/// Tells [`Pictures`] that a line has been written; `picture` is whether it was a
/// `browser_frame`. The picture that was kept meanwhile is sent now.
pub(crate) fn written(pictures: &Pictures, picture: bool) {
    if !picture {
        return;
    }
    let Some(line) = pictures.finished() else {
        return;
    };
    let send = pictures
        .send
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .as_ref()
        .and_then(Weak::upgrade);
    match send {
        Some(send) => send(line),
        // Its client went: the next one starts afresh.
        None => {
            let _ = pictures.finished();
        }
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
    use std::collections::{HashMap, HashSet};

    use azazel_browser::{BrowserSession, BrowserStateInfo, LiveButton, LiveInput};
    use azazel_protocol::{BrowserInput, MouseButton};
    use tokio::sync::{broadcast, mpsc};

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
        /// Names the watchers that arrived: the pump of that holder (and no other) sends the
        /// latest picture again, so that a new watcher gets one at once, even when the page is
        /// still.
        resend: broadcast::Sender<String>,
        /// How many jobs of each holder are queued or being done, so that a take-over waits
        /// behind them (an `off` sent before it must not undo it) and only then.
        pending: Arc<Mutex<HashMap<String, usize>>>,
    }

    impl Browser {
        /// The browser of the app, when this build and session have one.
        pub(crate) fn of(services: &Services) -> Option<Arc<Browser>> {
            let session = services.get::<BrowserSession>()?;
            let (jobs, queue) = mpsc::unbounded_channel();
            let watching = Arc::new(Mutex::new(HashSet::new()));
            let resend = broadcast::channel(16).0;
            let pending = Arc::new(Mutex::new(HashMap::new()));
            tokio::spawn(work(
                session.clone(),
                watching.clone(),
                resend.clone(),
                pending.clone(),
                queue,
            ));
            Some(Arc::new(Browser {
                session,
                jobs,
                watching,
                resend,
                pending,
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

        /// Does `command` for `holder` and answers with `reply`. Taking is done at once when
        /// nothing of the holder is waiting, as it needs nothing else; behind earlier commands of
        /// the same holder it waits its turn (an input, a hand back and a take sent in a row end
        /// in that order). Handing back and the rest always wait their turn, so that the inputs
        /// that came before a hand back are done first.
        pub(crate) fn run(&self, holder: String, command: Command, reply: Reply) {
            let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
            if let Command::BrowserTake { on: true, force } = command
                && !pending.contains_key(&holder)
            {
                drop(pending);
                let outcome = self.session.take(&holder, force);
                return reply(outcome.and_then(|()| state_data(&self.session, &holder)));
            }
            *pending.entry(holder.clone()).or_insert(0) += 1;
            drop(pending);
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
                        arrived = resend.recv() => match arrived {
                            Ok(who) if who != holder => {}
                            // Behind: some watcher's name was lost, and it may be this one.
                            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {
                                picture(frames.borrow().clone());
                            }
                            Err(broadcast::error::RecvError::Closed) => return,
                        },
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
            BrowserInput::Click {
                x,
                y,
                button,
                count,
                continues,
            } => LiveInput::Click {
                x: fraction("x", x)?,
                y: fraction("y", y)?,
                button: match button {
                    MouseButton::Left => LiveButton::Left,
                    MouseButton::Middle => LiveButton::Middle,
                    MouseButton::Right => LiveButton::Right,
                },
                count,
                continues,
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
            BrowserInput::Forward => LiveInput::Forward,
            BrowserInput::Reload => LiveInput::Reload,
        })
    }

    async fn work(
        session: Arc<BrowserSession>,
        watching: Arc<Mutex<HashSet<String>>>,
        resend: broadcast::Sender<String>,
        pending: Arc<Mutex<HashMap<String, usize>>>,
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
        // Finds out a hold that ran out, so that the clients are told even when no agent waits.
        let keeper = tokio::spawn({
            let session = session.clone();
            async move { session.keep_lease().await }
        });
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
                    // Anything a holder asks of the browser shows it is there.
                    session.renew(&holder);
                    let outcome = match command {
                        Command::BrowserWatch { on } => {
                            if set(&holder, on) {
                                session.watch(on).await;
                            }
                            if on {
                                let _ = resend.send(holder.clone());
                            }
                            Ok(())
                        }
                        Command::BrowserInput { input } => match input_of(input) {
                            Ok(input) => session.user_input(&holder, input).await,
                            Err(why) => Err(why),
                        },
                        Command::BrowserTake { on: true, force } => session.take(&holder, force),
                        Command::BrowserTake { .. } => {
                            session.give_back(&holder);
                            Ok(())
                        }
                        Command::BrowserSignIn { url } => session.sign_in(&holder, url.as_deref()).await,
                        _ => Err("not a browser command".to_string()),
                    };
                    {
                        let mut pending = pending.lock().unwrap_or_else(PoisonError::into_inner);
                        if let Some(n) = pending.get_mut(&holder) {
                            *n -= 1;
                            if *n == 0 {
                                pending.remove(&holder);
                            }
                        }
                    }
                    reply(outcome.and_then(|()| state_data(&session, &holder)));
                }
            }
        }
        keeper.abort();
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

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use super::*;

    /// A watcher that arrives is sent the latest picture; the watchers that were there are not
    /// sent it again.
    #[cfg(feature = "browser")]
    #[tokio::test]
    async fn a_new_watcher_is_sent_the_latest_picture_and_the_others_are_not() {
        use azazel_browser::{BrowserSession, LiveSettings, Settings};
        use tokio::sync::mpsc;

        let session = Arc::new(BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: std::path::PathBuf::new(),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: LiveSettings::default(),
        }));
        let services = Services::new();
        services.insert(session.clone());
        let browser = Browser::of(&services).expect("the session has a browser");
        let client = |holder: &str| {
            let (tx, rx) = mpsc::unbounded_channel::<String>();
            let pump = browser.pump_to(holder.to_string(), move |line, picture| {
                if picture {
                    let _ = tx.send(line);
                }
            });
            (rx, pump)
        };
        let watch = |holder: &str| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            browser.run(
                holder.to_string(),
                Command::BrowserWatch { on: true },
                Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            );
            rx
        };
        let seq = |line: String| {
            serde_json::from_str::<Value>(&line).expect("a frame")["seq"]
                .as_u64()
                .expect("a seq")
        };

        let (mut first, _first) = client("a");
        let (mut second, _second) = client("b");
        watch("a").await.expect("answered").expect("watching");
        session.publish_picture("t1", "AAAA".to_string());
        assert_eq!(seq(first.recv().await.expect("a picture")), 1);

        // `b` arrives while the page is still: it gets the picture that is there.
        watch("b").await.expect("answered").expect("watching");
        assert_eq!(seq(second.recv().await.expect("a picture")), 1);

        // `a` was sent nothing for it: its next picture is the next one.
        session.publish_picture("t1", "AAAA".to_string());
        assert_eq!(seq(first.recv().await.expect("a picture")), 2);
        assert_eq!(seq(second.recv().await.expect("a picture")), 2);
        for _ in 0..50 {
            tokio::task::yield_now().await;
        }
        assert!(first.try_recv().is_err(), "a was sent a picture twice");
        assert!(second.try_recv().is_err(), "b was sent a picture twice");
    }

    /// Commands of one holder sent in a row are done in that order: a take sent after an input
    /// and a hand back must not run before the hand back and be undone by it.
    #[cfg(feature = "browser")]
    #[tokio::test]
    async fn a_take_sent_behind_a_hand_back_is_done_after_it() {
        use azazel_browser::{BrowserSession, LiveSettings, Settings};

        let session = Arc::new(BrowserSession::new(Settings {
            path: String::new(),
            headless: true,
            profile: std::path::PathBuf::new(),
            screenshot_scale: 1.0,
            snapshot_diff: true,
            live: LiveSettings::default(),
        }));
        session.pretend_open();
        let services = Services::new();
        services.insert(session.clone());
        let browser = Browser::of(&services).expect("the session has a browser");
        let send = |holder: &str, command: Command| {
            let (tx, rx) = tokio::sync::oneshot::channel();
            browser.run(
                holder.to_string(),
                command,
                Box::new(move |outcome| {
                    let _ = tx.send(outcome);
                }),
            );
            rx
        };
        let take = |on| Command::BrowserTake { on, force: false };

        // Nothing waits: taking is done at once.
        send("a", take(true)).await.unwrap().unwrap();
        assert!(session.holds("a"));

        // off, then on, sent in a row: the holder ends with the browser.
        let off = send("a", take(false));
        let on = send("a", take(true));
        off.await.unwrap().unwrap();
        on.await.unwrap().unwrap();
        assert!(
            session.holds("a"),
            "the take was undone by the hand back before it"
        );

        // Behind a queued command the take waits its turn, and answers that the browser is held.
        let watch = send("a", Command::BrowserWatch { on: false });
        let off = send("a", take(false));
        let on = send("a", take(true));
        for done in [watch, off, on] {
            done.await.unwrap().unwrap();
        }
        assert!(session.holds("a"));
        // Another holder's queue does not hold it up.
        send("b", take(true)).await.unwrap().unwrap_err();
    }

    #[test]
    fn a_client_is_sent_one_picture_at_a_time_and_every_other_line() {
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let pictures = Arc::new(Pictures::default());
        let send = {
            let sent = sent.clone();
            one_picture_in_flight(pictures.clone(), move |line| sent.lock().unwrap().push(line))
        };
        let picture = |n: u32| format!(r#"{{"type":"browser_frame","seq":{n}}}"#);
        send(picture(1), true);
        send(picture(2), true);
        send(r#"{"type":"browser_state"}"#.to_string(), false);
        send(picture(3), true);
        assert_eq!(
            sent.lock().unwrap().len(),
            2,
            "the pictures behind the first were kept"
        );
        // The writer wrote the first one: the newest picture kept meanwhile goes out, not the
        // ones before it.
        written(&pictures, picture(1).starts_with(PICTURE));
        assert_eq!(sent.lock().unwrap().len(), 3);
        assert!(sent.lock().unwrap()[2].contains(r#""seq":3"#));
        // Lines that are not pictures free nothing.
        written(&pictures, false);
        send(picture(4), true);
        assert_eq!(sent.lock().unwrap().len(), 3);
        // That one is written; the picture that came after it follows, and then it is quiet.
        written(&pictures, true);
        let sent = sent.lock().unwrap();
        assert_eq!(sent.len(), 4);
        assert!(sent[3].contains(r#""seq":4"#), "{sent:?}");
    }

    #[test]
    fn a_skipped_picture_is_sent_when_the_writer_is_done_even_if_none_follows() {
        let sent = Arc::new(Mutex::new(Vec::<String>::new()));
        let pictures = Arc::new(Pictures::default());
        let send = {
            let sent = sent.clone();
            one_picture_in_flight(pictures.clone(), move |line| sent.lock().unwrap().push(line))
        };
        send("first".to_string(), true);
        send("last".to_string(), true);
        written(&pictures, true);
        assert_eq!(*sent.lock().unwrap(), ["first", "last"]);
        written(&pictures, true);
        // Nothing more was kept: a later picture goes straight out.
        send("later".to_string(), true);
        assert_eq!(sent.lock().unwrap().len(), 3);
    }
}
