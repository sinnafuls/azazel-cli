//! Getting the attention of someone who is looking elsewhere, and telling the terminal what the
//! session is doing: the bell, a desktop notification, the window title, the progress indicator
//! of the tab, and keeping the machine awake while agents work.
//!
//! Nothing here writes to the terminal. Each call returns the bytes to write, so the caller keeps
//! them in order with its own output and tests can read them.

use std::time::{Duration, Instant};

use azazel_core::config::{DesktopNotify, NotifyConfig};
use azazel_tui::term::{BELL, Caps, Notify, POP_TITLE, PUSH_TITLE, Progress};

/// How long the machine is kept awake after the last agent stopped.
const AWAKE_AFTER: Duration = Duration::from_secs(30);
/// How often a progress state is sent again to a terminal that forgets it.
const PROGRESS_EVERY: Duration = Duration::from_secs(1);

/// Why the user is wanted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// An approval, a question or a plan waits for an answer.
    NeedsYou,
    /// A run finished.
    Done,
    /// A run failed.
    Failed,
}

/// What the session is doing, for the title and the progress indicator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Activity {
    Idle,
    Working,
    /// An agent waits for the user.
    Waiting,
    /// The last run failed and nothing has happened since.
    Failed,
}

type SystemNotifier = Box<dyn Fn(&str, &str) + Send>;

pub struct Attention {
    caps: Caps,
    config: NotifyConfig,
    /// Whether the terminal has the focus, once it has said so. A terminal that never reports
    /// focus counts as not focused.
    focused: Option<bool>,
    /// The title last set.
    title: Option<String>,
    /// The progress state last sent, and when.
    progress: Option<(Progress, Instant)>,
    awake: Awake,
    system: SystemNotifier,
}

impl Attention {
    pub fn new(caps: Caps, config: NotifyConfig) -> Self {
        Self::with_notifier(caps, config, Box::new(system_notification))
    }

    fn with_notifier(caps: Caps, config: NotifyConfig, system: SystemNotifier) -> Self {
        Attention {
            caps,
            config,
            focused: None,
            title: None,
            progress: None,
            awake: Awake::default(),
            system,
        }
    }

    /// What to write when the UI takes the terminal: the title that is there is put aside.
    pub fn open(&self) -> &'static str {
        if self.config.title { PUSH_TITLE } else { "" }
    }

    /// What to write when the UI gives the terminal back: no progress, and the title as it was.
    pub fn close(&mut self) -> String {
        self.awake.release();
        let mut out = String::new();
        if self.progress.take().is_some() {
            out.push_str(&self.caps.progress(None));
        }
        if self.config.title {
            out.push_str(POP_TITLE);
        }
        self.title = None;
        out
    }

    /// The terminal said whether its window has the focus.
    pub fn focus(&mut self, focused: bool) {
        self.focused = Some(focused);
    }

    /// Calls for the user: the bell and a desktop notification, as configured. Nothing when the
    /// terminal has the focus, or for a run that ended sooner than `notify.min_secs`
    /// (`took`). Returns what to write to the terminal; a notification of the operating system
    /// is sent on its own.
    pub fn signal(&self, kind: Kind, text: &str, took: Option<Duration>) -> String {
        if self.focused == Some(true) {
            return String::new();
        }
        let long_enough = took.is_none_or(|took| took.as_secs() >= self.config.min_secs);
        if kind != Kind::NeedsYou && !long_enough {
            return String::new();
        }
        let mut out = String::new();
        if self.config.bell {
            out.push_str(BELL);
        }
        let through_terminal = match self.config.desktop {
            DesktopNotify::Off => return out,
            DesktopNotify::Terminal => true,
            DesktopNotify::System => false,
            DesktopNotify::Auto => self.caps.notify != Notify::None,
        };
        if through_terminal {
            // A terminal that was not recognised is sent the oldest and most widely read form.
            let caps = match self.caps.notify {
                Notify::None => &Caps {
                    notify: Notify::Osc9,
                    ..self.caps.clone()
                },
                _ => &self.caps,
            };
            out.push_str(&caps.notification("azazel", text));
        } else {
            (self.system)("azazel", text);
        }
        out
    }

    /// Tells the terminal what the session is doing: the title (`azazel · <label>`, with `* `
    /// before it while working and `! ` while waiting for the user) and the progress indicator.
    /// Returns only what changed since the last call; a terminal that forgets the progress
    /// state is told again every second.
    pub fn show(&mut self, activity: Activity, label: &str, now: Instant) -> String {
        let mut out = String::new();
        if self.config.title {
            let mark = match activity {
                Activity::Working => "* ",
                Activity::Waiting => "! ",
                Activity::Idle | Activity::Failed => "",
            };
            let title = format!("{mark}azazel · {label}");
            if self.title.as_deref() != Some(&title) {
                out.push_str(&self.caps.title(&title));
                self.title = Some(title);
            }
        }
        if self.config.progress && self.caps.progress {
            let wanted = match activity {
                Activity::Idle => None,
                Activity::Working => Some(Progress::Working),
                Activity::Waiting => Some(Progress::Waiting),
                Activity::Failed => Some(Progress::Failed),
            };
            let stale = |sent: Instant| {
                self.caps.progress_expires && now.saturating_duration_since(sent) >= PROGRESS_EVERY
            };
            let send = match (self.progress, wanted) {
                (None, None) => false,
                (Some((shown, sent)), Some(wanted)) => shown != wanted || stale(sent),
                _ => true,
            };
            if send {
                out.push_str(&self.caps.progress(wanted));
                self.progress = wanted.map(|state| (state, now));
            }
        }
        out
    }

    /// Holds the machine awake while `working`, and for half a minute after.
    pub fn keep_awake(&mut self, working: bool, now: Instant) {
        if !self.config.keep_awake {
            return self.awake.release();
        }
        if working {
            self.awake.hold(now);
        } else if self.awake.idle_since(now) >= AWAKE_AFTER {
            self.awake.release();
        }
    }

    /// When something here has to be looked at again although nothing happens: the machine is
    /// let go to sleep, or a progress state is due again.
    pub fn next_wake(&self) -> Option<Instant> {
        let progress = match self.progress {
            Some((_, sent)) if self.caps.progress_expires => Some(sent + PROGRESS_EVERY),
            _ => None,
        };
        let awake = self.awake.last_active.map(|at| at + AWAKE_AFTER);
        match (progress, awake) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

/// Keeps the machine from going to sleep (the display may) for as long as it is held.
#[derive(Default)]
struct Awake {
    /// Ends the thread that holds the request when dropped.
    #[cfg(feature = "notify")]
    holder: Option<std::sync::mpsc::Sender<()>>,
    /// When agents last worked; `None` while nothing is held.
    last_active: Option<Instant>,
}

impl Awake {
    fn hold(&mut self, now: Instant) {
        if self.last_active.replace(now).is_some() {
            return;
        }
        // The request belongs to the thread that made it (on Windows it ends with the thread),
        // and making it talks to the session bus on Linux: a thread of its own does both.
        #[cfg(feature = "notify")]
        {
            let (holder, released) = std::sync::mpsc::channel::<()>();
            let spawned = std::thread::Builder::new()
                .name("keep-awake".to_string())
                .spawn(move || {
                    let held = keepawake::Builder::default()
                        .idle(true)
                        .reason("agents are working")
                        .app_name("azazel")
                        .app_reverse_domain("io.github.sinnafuls.azazel")
                        .create();
                    if let Err(e) = &held {
                        tracing::debug!("cannot keep the machine awake: {e}");
                    }
                    // Returns when the sender is dropped.
                    let _ = released.recv();
                    drop(held);
                });
            self.holder = spawned.is_ok().then_some(holder);
        }
    }

    fn idle_since(&self, now: Instant) -> Duration {
        self.last_active
            .map_or(Duration::MAX, |at| now.saturating_duration_since(at))
    }

    fn release(&mut self) {
        self.last_active = None;
        #[cfg(feature = "notify")]
        {
            self.holder = None;
        }
    }
}

/// A notification of the operating system: a Windows toast, the macOS notification centre, or
/// the desktop's notification service on Linux. Sent from a thread of its own; a machine that
/// has no such service (a server) shows nothing.
#[cfg(feature = "notify")]
fn system_notification(title: &str, body: &str) {
    let (title, body) = (title.to_string(), body.to_string());
    let sent = std::thread::Builder::new()
        .name("notify".to_string())
        .spawn(move || {
            let mut notification = notify_rust::Notification::new();
            notification.appname("azazel").summary(&title).body(&body);
            if let Err(e) = notification.show() {
                tracing::debug!("desktop notification failed: {e}");
            }
        });
    if let Err(e) = sent {
        tracing::debug!("desktop notification failed: {e}");
    }
}

#[cfg(not(feature = "notify"))]
fn system_notification(_title: &str, _body: &str) {}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use std::sync::{Arc, Mutex};

    use super::*;

    fn caps(vars: &[(&str, &str)]) -> Caps {
        let vars: Vec<(String, String)> = vars
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();
        Caps::detect(&move |name| vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()))
    }

    type Sent = Arc<Mutex<Vec<(String, String)>>>;

    fn attention(caps: Caps, config: NotifyConfig) -> (Attention, Sent) {
        let sent = Sent::default();
        let record = sent.clone();
        let notifier: SystemNotifier = Box::new(move |title, body| {
            record.lock().unwrap().push((title.to_string(), body.to_string()));
        });
        (Attention::with_notifier(caps, config, notifier), sent)
    }

    #[test]
    fn a_signal_is_the_bell_and_one_notification_and_only_when_nobody_is_looking() {
        // WezTerm shows notifications itself: nothing goes to the operating system.
        let (mut wezterm, sent) = attention(caps(&[("TERM_PROGRAM", "WezTerm")]), NotifyConfig::default());
        assert_eq!(
            wezterm.signal(Kind::NeedsYou, "lead needs permission to use shell", None),
            "\x07\x1b]9;azazel: lead needs permission to use shell\x1b\\"
        );
        assert!(sent.lock().unwrap().is_empty());
        // Focused: silence. Focus lost again: the signal is back.
        wezterm.focus(true);
        assert_eq!(wezterm.signal(Kind::NeedsYou, "x", None), "");
        wezterm.focus(false);
        assert_eq!(
            wezterm.signal(Kind::Failed, "x", None),
            "\x07\x1b]9;azazel: x\x1b\\"
        );

        // Windows Terminal has no notification of its own: bell there, toast from the system.
        let (windows, sent) = attention(caps(&[("WT_SESSION", "1")]), NotifyConfig::default());
        assert_eq!(
            windows.signal(Kind::NeedsYou, "a plan is ready for review", None),
            "\x07"
        );
        assert_eq!(
            *sent.lock().unwrap(),
            [("azazel".to_string(), "a plan is ready for review".to_string())]
        );

        // Inside tmux the notification is passed through and the bell is not.
        let (tmux, _) = attention(
            caps(&[("TERM_PROGRAM", "WezTerm"), ("TMUX", "/tmp/t,1,0")]),
            NotifyConfig::default(),
        );
        assert_eq!(
            tmux.signal(Kind::Done, "done", None),
            "\x07\x1bPtmux;\x1b\x1b]9;azazel: done\x1b\x1b\\\x1b\\"
        );
    }

    #[test]
    fn the_settings_choose_the_bell_and_the_path_of_the_notification() {
        let with = |change: fn(&mut NotifyConfig), vars: &[(&str, &str)]| {
            let mut config = NotifyConfig::default();
            change(&mut config);
            attention(caps(vars), config)
        };
        let kitty = [("KITTY_WINDOW_ID", "1")];

        let (quiet, sent) = with(|c| c.bell = false, &kitty);
        assert_eq!(
            quiet.signal(Kind::NeedsYou, "q", None),
            "\x1b]99;i=azazel:d=0:p=title;azazel\x1b\\\x1b]99;i=azazel:d=1:p=body;q\x1b\\"
        );
        assert!(sent.lock().unwrap().is_empty());

        let (off, sent) = with(|c| c.desktop = DesktopNotify::Off, &kitty);
        assert_eq!(off.signal(Kind::NeedsYou, "q", None), "\x07");
        assert!(sent.lock().unwrap().is_empty());

        // `system` goes past a terminal that could have shown it.
        let (system, sent) = with(|c| c.desktop = DesktopNotify::System, &kitty);
        assert_eq!(system.signal(Kind::NeedsYou, "q", None), "\x07");
        assert_eq!(sent.lock().unwrap().len(), 1);

        // `terminal` on a terminal that was not recognised sends the common form.
        let (terminal, sent) = with(|c| c.desktop = DesktopNotify::Terminal, &[]);
        assert_eq!(
            terminal.signal(Kind::NeedsYou, "q", None),
            "\x07\x1b]9;azazel: q\x1b\\"
        );
        assert!(sent.lock().unwrap().is_empty());
    }

    #[test]
    fn a_short_run_ends_quietly_and_a_request_never_does() {
        let (attention, _) = attention(caps(&[("TERM_PROGRAM", "WezTerm")]), NotifyConfig::default());
        let secs = Duration::from_secs;
        assert_eq!(attention.signal(Kind::Done, "done", Some(secs(19))), "");
        assert_eq!(attention.signal(Kind::Failed, "failed", Some(secs(5))), "");
        assert_ne!(attention.signal(Kind::Done, "done", Some(secs(20))), "");
        assert_ne!(attention.signal(Kind::Failed, "failed", Some(secs(300))), "");
        assert_ne!(attention.signal(Kind::NeedsYou, "asks", Some(secs(1))), "");
    }

    #[test]
    fn the_title_and_the_progress_follow_what_the_session_does() {
        let (mut attention, _) = attention(caps(&[("WT_SESSION", "1")]), NotifyConfig::default());
        let start = Instant::now();
        assert_eq!(attention.open(), "\x1b[22;0t");
        // Idle at first: a title, and no progress to clear.
        assert_eq!(
            attention.show(Activity::Idle, "fix the bug", start),
            "\x1b]0;azazel · fix the bug\x07"
        );
        assert_eq!(
            attention.show(Activity::Working, "fix the bug", start),
            "\x1b]0;* azazel · fix the bug\x07\x1b]9;4;3;0\x1b\\"
        );
        // Nothing changed: nothing is written, however much time passes in this terminal.
        assert_eq!(
            attention.show(Activity::Working, "fix the bug", start + Duration::from_secs(60)),
            ""
        );
        assert_eq!(
            attention.show(Activity::Waiting, "fix the bug", start),
            "\x1b]0;! azazel · fix the bug\x07\x1b]9;4;4;0\x1b\\"
        );
        assert_eq!(
            attention.show(Activity::Failed, "fix the bug", start),
            "\x1b]0;azazel · fix the bug\x07\x1b]9;4;2;100\x1b\\"
        );
        // A new name alone changes the title alone.
        assert_eq!(
            attention.show(Activity::Failed, "renamed", start),
            "\x1b]0;azazel · renamed\x07"
        );
        assert_eq!(
            attention.show(Activity::Idle, "renamed", start),
            "\x1b]9;4;0;0\x1b\\"
        );
        assert_eq!(attention.show(Activity::Idle, "renamed", start), "");
        // Leaving while working clears the progress and brings the old title back.
        attention.show(Activity::Working, "renamed", start);
        assert_eq!(attention.close(), "\x1b]9;4;0;0\x1b\\\x1b[23;0t");
        assert_eq!(attention.close(), "\x1b[23;0t");
    }

    #[test]
    fn progress_is_wrapped_for_tmux_and_repeated_where_it_expires() {
        let (mut tmux, _) = attention(
            caps(&[("WT_SESSION", "1"), ("TMUX", "/tmp/t,1,0")]),
            NotifyConfig::default(),
        );
        let start = Instant::now();
        assert_eq!(
            tmux.show(Activity::Working, "x", start),
            "\x1b]0;* azazel · x\x07\x1bPtmux;\x1b\x1b]9;4;3;0\x1b\x1b\\\x1b\\"
        );
        assert_eq!(tmux.next_wake(), None);

        let (mut ghostty, _) = attention(caps(&[("GHOSTTY_RESOURCES_DIR", "/g")]), NotifyConfig::default());
        ghostty.show(Activity::Working, "x", start);
        assert_eq!(ghostty.next_wake(), Some(start + Duration::from_secs(1)));
        assert_eq!(
            ghostty.show(Activity::Working, "x", start + Duration::from_millis(900)),
            ""
        );
        let later = start + Duration::from_millis(1000);
        assert_eq!(ghostty.show(Activity::Working, "x", later), "\x1b]9;4;3;0\x1b\\");
        assert_eq!(ghostty.next_wake(), Some(later + Duration::from_secs(1)));
        ghostty.show(Activity::Idle, "x", later);
        assert_eq!(ghostty.next_wake(), None);
    }

    #[test]
    fn title_and_progress_can_be_turned_off_and_unknown_terminals_get_no_progress() {
        let config = NotifyConfig {
            title: false,
            progress: false,
            keep_awake: false,
            ..NotifyConfig::default()
        };
        let (mut off, _) = attention(caps(&[("WT_SESSION", "1")]), config);
        let now = Instant::now();
        assert_eq!(off.open(), "");
        assert_eq!(off.show(Activity::Working, "x", now), "");
        assert_eq!(off.close(), "");
        off.keep_awake(true, now);
        assert_eq!(off.next_wake(), None);

        let (mut plain, _) = attention(caps(&[]), NotifyConfig::default());
        assert_eq!(plain.show(Activity::Working, "x", now), "\x1b]0;* azazel · x\x07");
    }

    #[test]
    fn the_machine_is_let_go_half_a_minute_after_the_last_work() {
        let mut awake = Awake::default();
        let start = Instant::now();
        assert_eq!(awake.idle_since(start), Duration::MAX);
        // Only the bookkeeping is exercised: holding for real would keep the test machine awake.
        awake.last_active = Some(start);
        assert_eq!(
            awake.idle_since(start + Duration::from_secs(29)),
            Duration::from_secs(29)
        );
        assert!(awake.idle_since(start + AWAKE_AFTER) >= AWAKE_AFTER);
        awake.release();
        assert_eq!(awake.last_active, None);
    }
}
