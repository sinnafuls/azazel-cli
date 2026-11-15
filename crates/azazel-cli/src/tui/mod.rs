//! The interactive terminal UI: the transcript scrolls into the terminal's own scrollback, and a
//! live region below it holds the streaming reply and running tools on top and, on the last rows
//! of the screen, the input and the status line.

mod backend;
mod bridge;
mod btw;
mod cache;
mod calls;
mod clip;
mod debug;
mod diff_view;
mod extras;
mod fleet;
mod form;
mod gitcmd;
mod history;
mod input;
mod loops;
mod migrate;
mod models;
mod overlay;
mod pager;
mod panels;
mod partial_json;
mod paste;
mod places;
mod popup;
mod probe;
mod quick;
mod rate;
mod recent;
mod remote;
mod resize;
mod running;
mod sessions;
mod setup;
mod share;
mod slash;
mod statusbar;
mod statusedit;
mod statusline;
mod team;
mod term;
mod terminal;
mod usage;
pub(crate) mod view;
#[cfg(feature = "voice")]
mod voice;
#[cfg(not(feature = "voice"))]
#[path = "voice_off.rs"]
mod voice;

use std::collections::VecDeque;
use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_agent::AgentEvent;
use azazel_ai::{Block, ModelSpec, StopReason, StreamEvent};
use azazel_core::{
    ApprovalDecision, CommandOutcome, Level, Paths, PermissionMode, PlanDecision, SessionStore,
};
use azazel_tools::TodoStatus;
use azazel_tui::components::{composer, format_elapsed, format_tokens, markdown, shimmer, spinner, todo};
use azazel_tui::modal;
use azazel_tui::mouse::Mouse;
use azazel_tui::term::{Caps, FOCUS_OFF, FOCUS_ON, MouseCapture};
use azazel_tui::{
    Atom, ColorDepth, Composer, Draft, Editor, Floor, Frame, Glyph, GlyphLevel, HistoryItem, Keymap, Line,
    Renderer, Role, SelectItem, SelectList, Spinner, StreamingMarkdown, Theme, TodoState, Vim,
    disable_enhanced_keys, enable_enhanced_keys, text,
};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
    MouseEvent, MouseEventKind,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use tokio::sync::mpsc;

use crate::app::{Active, App};
use crate::attention::{Attention, Kind};
pub(crate) use backend::Env;
use backend::{Backend, Go, Incoming, Local, Moved};
use bridge::UiRequest;
pub(crate) use input::Queue;
#[cfg(feature = "test-support")]
pub(crate) use input::queue_from_env;
use overlay::{Action, ApprovalRow, Outcome, Overlay, approval_rows};
#[cfg(test)]
pub(crate) use remote::agent_event;
pub(crate) use remote::{Place, Remote};
pub(crate) use slash::{REWIND_NOTE, builtin_names as builtin_command_names, is_note};

const DOUBLE_PRESS: Duration = Duration::from_millis(1000);
const MAX_COMPLETION_FILES: usize = 20_000;
const COMPLETION_ROWS: usize = 8;
/// How the note about a `!command` the user ran starts in the agent's context.
pub(crate) const SHELL_NOTE: &str = "I ran `";

/// Puts the terminal into raw mode and restores it when dropped, however the UI ends. It is
/// also the one owner of the alternate screen, which popups and the full-screen viewer share:
/// whoever ends the UI, or hands the terminal to another program, leaves it through here.
struct TerminalGuard {
    enhanced_keys: bool,
    /// The terminal is ours: raw mode, bracketed paste and enhanced keys are on.
    active: bool,
    /// The alternate screen is showing.
    alt: bool,
    /// The mouse is reported to azazel while a popup or the full-screen viewer is up, and only
    /// then: reporting takes the terminal's own selection and scrolling away.
    mouse: MouseCapture<io::Stdout>,
    /// Changes of the window's size are reported because azazel asked (a Windows console).
    sizes: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = TerminalGuard {
            enhanced_keys: false,
            active: false,
            alt: false,
            mouse: MouseCapture::native(io::stdout()),
            sizes: false,
        };
        guard.take()?;
        Ok(guard)
    }

    /// Switches to the alternate screen. False when it was showing already, in which case
    /// whoever entered it leaves it.
    fn enter_alt(&mut self) -> bool {
        if self.alt {
            return false;
        }
        let _ = crossterm::execute!(io::stdout(), EnterAlternateScreen);
        self.alt = true;
        true
    }

    /// Goes back to the screen with the transcript, which is as it was left.
    fn leave_alt(&mut self) {
        self.capture_mouse(false);
        if std::mem::take(&mut self.alt) {
            let mut out = io::stdout();
            // Whatever painted there may have hidden the cursor; the inline renderer believes
            // it is showing.
            let _ = out.write_all(b"\x1b[?25h");
            let _ = crossterm::execute!(out, LeaveAlternateScreen);
        }
    }

    fn in_alt(&self) -> bool {
        self.alt
    }

    /// Has the terminal report the mouse to azazel, or stops that. Leaving the alternate
    /// screen stops it, and so does handing the terminal over or dropping the guard.
    fn capture_mouse(&mut self, on: bool) {
        if let Err(e) = self.mouse.set(on) {
            tracing::warn!("cannot change mouse reporting: {e}");
        }
    }

    fn take(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.active = true;
        self.sizes = resize::hear_sizes();
        let mut out = io::stdout();
        crossterm::execute!(out, EnableBracketedPaste)?;
        self.enhanced_keys = enable_enhanced_keys(&mut out).unwrap_or(false);
        // The terminal says when its window gains and loses the focus.
        out.write_all(FOCUS_ON.as_bytes())?;
        out.flush()
    }

    /// Hands the terminal back as a shell would have it (cooked mode, no bracketed paste, no
    /// enhanced keys), for a program that takes it over for a while or for exit.
    fn suspend(&mut self) {
        // Another program, or the shell after a crash, must never find the alternate screen.
        self.leave_alt();
        if !std::mem::take(&mut self.active) {
            return;
        }
        let mut out = io::stdout();
        let _ = out.write_all(FOCUS_OFF.as_bytes());
        if std::mem::take(&mut self.enhanced_keys) {
            let _ = disable_enhanced_keys(&mut out);
        }
        let _ = crossterm::execute!(out, DisableBracketedPaste);
        if std::mem::take(&mut self.sizes) {
            resize::stop_hearing_sizes();
        }
        let _ = disable_raw_mode();
        let _ = out.flush();
    }

    /// Takes the terminal again after [`TerminalGuard::suspend`].
    fn resume(&mut self) {
        if !self.active
            && let Err(e) = self.take()
        {
            tracing::error!("cannot take the terminal back: {e}");
        }
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        self.suspend();
    }
}

/// Resolves at `at`; never when there is nothing to wake for.
async fn wake_at(at: Option<Instant>) {
    match at {
        Some(at) => tokio::time::sleep_until(at.into()).await,
        None => std::future::pending().await,
    }
}

/// The next terminal event; never resolves while the terminal is handed to another program.
async fn next_terminal(terminal: &mut Option<EventStream>) -> Option<io::Result<Event>> {
    match terminal {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

/// The assistant message being streamed.
#[derive(Default)]
struct StreamView {
    markdown: StreamingMarkdown,
    thinking: String,
    saw_text: bool,
    /// The blank row that sets this message apart in the transcript has been written.
    opened: bool,
}

struct Completion {
    list: SelectList,
    /// Completing a `/command` rather than an `@file`.
    slash: bool,
    /// Completing the directory of `/cd`.
    dirs: bool,
}

/// Results of work the UI started in the background.
enum Internal {
    /// What a `!command` printed and how it ended.
    Shell { output: String, status: String },
    /// What a registered slash command came back with.
    Command(CommandOutcome),
    /// The files of a session in another process, for completing `@` mentions.
    Files(Vec<String>),
    /// A setting the settings menu saved that applies to the running UI at once.
    Setting { path: String, value: toml::Value },
    /// Something a command of this process shows in a box of its own: the settings screen.
    Request(UiRequest),
    #[cfg(feature = "voice")]
    Voice(voice::Done),
    /// Text of a side question's answer, or its end.
    Btw(btw::Update),
    /// A commit message was drafted, or could not be.
    CommitDraft(gitcmd::Draft),
    /// A reply to a request made outside the conversation (`/consult`, `/handoff`, `/name`).
    Side(sessions::SideEvent),
    /// The judge of a `/loop until` has answered.
    LoopVerdict {
        id: u64,
        verdict: Result<crate::loops::Verdict, String>,
    },
    /// A trigger of the project happened, or cannot be watched.
    #[cfg(feature = "detach")]
    Trigger(crate::schedule::triggers::Event),
    /// A session was shared, a bug report written or its issue opened; or not.
    Sent(share::Sent),
    /// What the providers said about the usage of the stored logins, for the open `/usage` box.
    Usage(Vec<crate::usage::Entry>),
    /// What the clipboard held when a paste key was pressed.
    Clipboard(paste::Clip),
    /// The clipboard's text, for the key that pastes text only.
    ClipboardText(Option<String>),
    /// A step of `/migrate` finished beside the UI loop.
    Migrate(migrate::Step),
    /// The program of `statusline.command` printed its line, or did not.
    StatusCommand(statusbar::CommandLine),
    /// How many files differ from the last commit; none where git cannot say.
    ChangedFiles(Option<usize>),
    /// Something for the open `/models` screen.
    Models(models::Update),
    /// `/settings` opens `/models` and waits for it to close.
    OpenModels(models::Opening, bridge::Reply<()>),
}

pub struct Tui {
    /// The session, in this process or in another.
    backend: Box<dyn Backend>,
    /// What the open session holds, to read: the live store of a local session, a copy of a
    /// remote one that [`Tui::sync`] brings up to date.
    session: Arc<SessionStore>,
    /// This machine's paths and settings, for what belongs to the UI itself.
    env: Env,
    guard: TerminalGuard,
    /// Keys, pastes and resizes; `None` while another program has the terminal.
    terminal: Option<EventStream>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    renderer: Renderer<io::Stdout>,
    /// What the terminal can do: links, images, notifications, progress.
    caps: Caps,
    /// The bell, notifications, the title and the progress indicator.
    attention: Attention,
    /// The lead's last run failed and nothing has been typed since.
    run_failed: bool,
    theme: Theme,
    /// The name the theme was loaded by, for reading it again.
    theme_name: String,
    /// Whether the terminal's background is a light one, as it said when the UI opened; `None`
    /// when it was not asked or did not say. It decides what the theme `auto` is.
    light_terminal: Option<bool>,
    keymap: Keymap,
    editor: Editor,
    /// Vim keys in front of the editor, when they are on.
    vim: Option<Vim>,
    /// Pastes longer than this many lines fold into a token.
    fold_paste_lines: usize,
    /// `[editor] command`, as the settings menu last left it.
    editor_command: String,
    /// Models the cycling keys step through, as written in `tui.favourites`.
    favourites: Vec<String>,
    /// What the cycling keys step through, shown above the input for a moment after a step.
    strip: Option<input::Strip>,
    /// The status line: its layout and what feeds it from outside the session.
    status: statusbar::StatusBar,
    /// Messages waiting for the agent to finish.
    queued: Queue,
    /// The lead just finished a run without being interrupted: the next queued message may go.
    run_ended: bool,
    /// The `/loop` in progress.
    looping: Option<loops::Looping>,
    /// How many loops were started, to tell them apart.
    loop_serial: u64,
    /// Drafts parked with `ctrl+s`, each with the images that were attached to it.
    stashed: Vec<(Draft, Vec<(Atom, Block)>)>,
    /// Keeps the input and the status line on the last rows of the screen.
    floor: Floor,
    /// A change of the window's size that has not settled yet.
    resize: resize::Resize,
    /// The width of the window and the row the live region began on when the alternate screen
    /// was shown, while it is.
    alt_from: Option<(u16, usize)>,
    /// Transcript rows and blocks waiting to be written above the live region.
    transcript: Vec<HistoryItem>,
    /// The last thing written to the transcript is a group of compact tool rows, so the next
    /// group joins it without a blank row between them. Cards always stand apart.
    after_rows: bool,
    /// The checked-out branch, for the status line.
    branch: Option<String>,
    stream: StreamView,
    tools: Vec<running::RunningTool>,
    /// Finished tool calls that are not in the transcript yet, and the last that are.
    calls: calls::Calls,
    /// What was written to the transcript, to draw it again.
    history: history::History,
    overlay: Option<Overlay>,
    /// Overlays of the UI's own that wait until nothing else asks for the user.
    deferred: VecDeque<Overlay>,
    /// The terminal of a program an agent runs interactively, while it is on show. An overlay
    /// comes before it.
    term: Option<term::TermView>,
    /// The terminals heard of so far.
    terms: term::Known,
    /// Tells when something happens to a terminal.
    term_changes: Option<tokio::sync::watch::Receiver<u64>>,
    /// Tells when an extension sets or removes a segment of the status line or a widget.
    extras_changes: Option<tokio::sync::watch::Receiver<u64>>,
    /// The popup screen, while it is up.
    popup: Option<popup::Popup>,
    /// The providers asked for their models in this process, by `/models`.
    models_asked: std::collections::BTreeSet<String>,
    /// The `/models` screen while the prompts of a sign-in it started are up.
    models_parked: Option<Overlay>,
    /// `/models` as `/settings` asked for it, once nothing else is open.
    models_waiting: Option<(models::Opening, bridge::Reply<()>)>,
    /// What was written to the transcript lately, to paint behind a popup.
    backdrop: popup::Backdrop,
    /// Rows the input and what is pinned around it took in the last inline frame: where the
    /// transcript ended on the screen.
    pinned_rows: usize,
    /// The side question (`/btw`) being answered; it stands in for the input.
    btw: Option<btw::Panel>,
    waiting: VecDeque<UiRequest>,
    /// What the session said while a full-screen view had the terminal.
    held: VecDeque<Incoming>,
    completion: Option<Completion>,
    files: Option<Vec<String>>,
    busy: bool,
    busy_since: Instant,
    /// How fast the lead writes, for the status line.
    rate: rate::Rate,
    show_thinking: bool,
    expand_tools: bool,
    /// The images attached to the draft, each with the chip that stands for it there.
    images: Vec<(Atom, Block)>,
    /// Images typed with a slash command, for the prompt that command may turn into.
    carried: Vec<Block>,
    prompts: Vec<String>,
    notice: Option<String>,
    last_escape: Option<Instant>,
    last_interrupt: Option<Instant>,
    /// A `ctrl+v` press reached azazel and its release has not yet (Windows).
    paste_key_down: bool,
    /// When the last burst of typed or pasted keys was taken (Windows).
    last_burst: Option<Instant>,
    /// The chain to go back to after a command that ran one prompt on another model.
    restore_chain: Option<Vec<ModelSpec>>,
    /// The sub-agent whose work is shown in the transcript next to the lead's.
    follow: Option<String>,
    /// Tool calls in flight of the followed agent.
    followed_tools: Vec<running::RunningTool>,
    /// What every sub-agent has done, and where the keyboard is in the roster.
    fleet: fleet::Fleet,
    #[cfg(feature = "voice")]
    voice: voice::Voice,
    quit: bool,
    /// The UI ends for azazel to start again on this session (`/restart`).
    restarting: bool,
    /// The session went away under the UI.
    ended: bool,
}

/// Which palette a theme that follows the background takes in a terminal whose background is
/// light (`Some(true)`), dark, or not known (`None`).
fn background_word(light: Option<bool>) -> &'static str {
    match light {
        Some(true) => "light",
        Some(false) | None => "dark",
    }
}

fn load_theme(env: &Env, light: Option<bool>) -> (Theme, Option<String>) {
    terminal_theme(&env.paths, &env.config, light)
}

/// The configured theme with the colours and symbols this terminal can show, and why the theme
/// could not be read when it could not. `light` is whether the terminal's background is a
/// light one, for the theme `auto`; `None` when the terminal was not asked or did not say.
pub(crate) fn terminal_theme(
    paths: &Paths,
    config: &azazel_core::Config,
    light: Option<bool>,
) -> (Theme, Option<String>) {
    let depth = if std::env::var_os("WT_SESSION").is_some() {
        ColorDepth::TrueColor
    } else {
        ColorDepth::detect(std::env::var("COLORTERM").ok().as_deref())
    };
    let (theme, problem) = named_theme(paths, &config.tui.theme, light);
    let glyphs = GlyphLevel::detect(config.tui.glyphs.as_str(), |name| std::env::var(name).ok());
    (theme.with_depth(depth).with_glyphs(glyphs), problem)
}

/// A built-in theme (`dark`, `light`, `catppuccin`, `nord-dark`, ...),
/// `~/.azazel/themes/<name>.toml`, or the theme file whose full path `name` is
/// (`--theme <file>`). A built-in theme with a dark and a light palette takes the one for a
/// terminal whose background is `light`; dark when that is not known. Falls back to `dark`
/// with a message.
pub(crate) fn named_theme(paths: &Paths, name: &str, light: Option<bool>) -> (Theme, Option<String>) {
    match Theme::builtin_for(name, light) {
        Some(theme) => (theme, None),
        None => {
            let other = name;
            let file = match std::path::Path::new(other).is_absolute() {
                true => std::path::PathBuf::from(other),
                false => paths.home.join("themes").join(format!("{other}.toml")),
            };
            match std::fs::read_to_string(&file) {
                Ok(source) => match Theme::from_toml(&source) {
                    Ok(theme) => (theme, None),
                    Err(e) => (Theme::dark(), Some(format!("{}: {e}", file.display()))),
                },
                Err(_) => (
                    Theme::dark(),
                    Some(format!(
                        "no theme named `{other}` (looked for {})",
                        file.display()
                    )),
                ),
            }
        }
    }
}

/// The UI on a session in this process.
pub async fn run(app: Arc<App>, active: Active, first_prompt: Option<String>) -> u8 {
    let local = Local::new(app, active);
    let env = local.env();
    run_on(Box::new(local), env, first_prompt).await
}

/// The UI on a session in another process: a background session, or azazel on another machine.
/// `env` is this machine's, for the UI's own settings.
pub(crate) async fn run_remote(remote: Remote, env: Env, first_prompt: Option<String>) -> u8 {
    run_on(Box::new(remote), env, first_prompt).await
}

/// This machine's settings for a UI whose session is elsewhere. The project here is not the
/// session's, so nothing of it is trusted.
pub(crate) fn env_here(cli: &crate::cli::Cli, paths: Paths) -> anyhow::Result<Env> {
    let options = azazel_core::LoadOptions {
        trusted: false,
        overrides: crate::app::config_overrides(cli, &paths)?,
        env: std::env::vars().collect(),
    };
    let mut config = azazel_core::Config::load(&paths, &options)?.config;
    if !cli.models.is_empty() {
        // Which models there are is known where the session runs: here a name is taken whole.
        config.tui.favourites = cli
            .models
            .iter()
            .map(|text| {
                text.trim().parse::<ModelSpec>().map_err(|e| {
                    crate::app::UsageError(format!(
                        "--models {text}: {e} (for a session elsewhere, name a provider/model or an @role)"
                    ))
                })
            })
            .collect::<Result<_, _>>()?;
    }
    Ok(Env {
        #[cfg(feature = "voice")]
        credentials: Arc::new(crate::app::credentials(&paths, false).configured(&config.providers)),
        paths,
        config,
    })
}

async fn run_on(mut backend: Box<dyn Backend>, env: Env, first_prompt: Option<String>) -> u8 {
    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("azazel: cannot take over the terminal: {e}");
            backend.finish().await;
            return 1;
        }
    };
    let (internal_tx, internal_rx) = mpsc::unbounded_channel();
    // The settings menu runs as a command, outside the UI loop; this is how it reaches it.
    if let Some(local) = backend.local() {
        let settings_tx = internal_tx.clone();
        local
            .app
            .registry
            .services
            .insert(Arc::new(crate::settings::LiveSettings(Arc::new(
                move |path: &str, value: &toml::Value| {
                    let _ = settings_tx.send(Internal::Setting {
                        path: path.to_string(),
                        value: value.clone(),
                    });
                },
            ))));
        let models_tx = internal_tx.clone();
        local
            .app
            .registry
            .services
            .insert(Arc::new(crate::settings::ModelsHost(Arc::new(move |opening| {
                let (reply, closed) = tokio::sync::oneshot::channel();
                let _ = models_tx.send(Internal::OpenModels(opening, reply));
                closed
            }))));
        let screen_tx = internal_tx.clone();
        local
            .app
            .registry
            .services
            .insert(Arc::new(crate::settings::SettingsHost(Arc::new(move |screen| {
                let (reply, back) = tokio::sync::oneshot::channel();
                let _ = screen_tx.send(Internal::Request(UiRequest::Settings { screen, reply }));
                back
            }))));
    }
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    // Asked before anything reads keys: the answers come in on the same channel. The colour
    // of the background is asked for only when the theme is to follow it, and only of a
    // terminal: a pipe would keep the question as output.
    let follow_background = Theme::follows_background(&env.config.tui.theme)
        && io::stdout().is_terminal()
        && io::stdin().is_terminal();
    let (caps, images_problem, light_terminal) =
        terminal::capabilities(env.config.tui.images, follow_background).await;
    let (theme, theme_problem) = load_theme(&env, light_terminal);
    let theme = theme.with_links(caps.links);
    let attention = Attention::new(caps.clone(), env.config.notify.clone());
    let mut keymap = Keymap::defaults();
    let mut key_problem = None;
    if let Ok(source) = std::fs::read_to_string(env.paths.keybindings_file())
        && let Err(e) = keymap.merge_toml(&source)
    {
        key_problem = Some(format!("{}: {e}", env.paths.keybindings_file().display()));
    }
    let branch = backend.branch();
    let session = backend.session().await;
    backend.list_files(internal_tx.clone());
    let editor_config = env.config.editor.clone();
    let tui_config = env.config.tui.clone();
    let (status, status_problems) = statusbar::StatusBar::new(&env.config.statusline);
    let mut renderer = Renderer::new(io::stdout(), width, height);
    renderer.set_rewraps(azazel_tui::term::rewraps(&|name| std::env::var(name).ok()));
    let mut tui = Tui {
        backend,
        session,
        env,
        guard,
        terminal: Some(EventStream::new()),
        internal_tx,
        internal_rx,
        renderer,
        caps,
        attention,
        run_failed: false,
        theme,
        theme_name: tui_config.theme.clone(),
        light_terminal,
        keymap,
        editor: Editor::new(),
        vim: (editor_config.mode == azazel_core::config::EditorMode::Vim).then(Vim::new),
        fold_paste_lines: editor_config.fold_paste_lines,
        editor_command: editor_config.command,
        favourites: tui_config.favourites.iter().map(ToString::to_string).collect(),
        strip: None,
        status,
        queued: Queue::default(),
        run_ended: false,
        looping: None,
        loop_serial: 0,
        stashed: Vec::new(),
        floor: Floor::new(height),
        resize: resize::Resize::default(),
        alt_from: None,
        transcript: Vec::new(),
        after_rows: false,
        branch,
        stream: StreamView::default(),
        tools: Vec::new(),
        calls: calls::Calls::default(),
        history: history::History::default(),
        overlay: None,
        deferred: VecDeque::new(),
        models_asked: Default::default(),
        models_parked: None,
        models_waiting: None,
        term: None,
        terms: term::Known::default(),
        term_changes: None,
        extras_changes: None,
        popup: None,
        backdrop: popup::Backdrop::default(),
        pinned_rows: popup::PLAIN_PINNED_ROWS,
        btw: None,
        waiting: VecDeque::new(),
        held: VecDeque::new(),
        completion: None,
        files: None,
        busy: false,
        busy_since: Instant::now(),
        rate: rate::Rate::default(),
        show_thinking: tui_config.show_thinking,
        expand_tools: tui_config.expand_tools,
        images: Vec::new(),
        carried: Vec::new(),
        prompts: Vec::new(),
        notice: None,
        last_escape: None,
        last_interrupt: None,
        paste_key_down: false,
        last_burst: None,
        restore_chain: None,
        follow: None,
        followed_tools: Vec::new(),
        fleet: fleet::Fleet::default(),
        #[cfg(feature = "voice")]
        voice: voice::Voice::default(),
        quit: false,
        restarting: false,
        ended: false,
    };
    tui.term_changes = tui.terminal_changes();
    tui.extras_changes = tui.extras_changes();
    let opening = tui.attention.open();
    tui.write_out(opening);
    tui.seed_history();
    tui.welcome();
    let problems = [theme_problem, key_problem, images_problem].into_iter().flatten();
    for problem in problems.chain(status_problems) {
        tui.warn(&problem);
    }
    tui.backend.started().await;
    tui.watch_triggers();
    // A session that is already at work when the UI opens on it shows as working.
    if tui.backend.status().busy {
        tui.busy = true;
        tui.busy_since = Instant::now();
    }
    if let Some(notice) = crate::changelog::update_notice(&tui.env.paths) {
        tui.info(&notice);
    }
    if crate::update::checks_enabled(tui.env.config.update.check) {
        if let Some(notice) = crate::update::notice(&tui.env.paths) {
            tui.info(&notice);
        }
        crate::update::check_in_background(&tui.env.paths, tui.env.config.update.auto);
    }
    if let Some(prompt) = first_prompt {
        // Sent as if typed: without a model it is kept in the editor while settings open.
        tui.editor.set_text(&prompt);
        tui.submit(false).await;
    } else if !tui.backend.has_model() {
        // A first run has nothing to answer with: go straight to signing in or adding a key.
        tui.info(
            "Welcome! azazel needs a model before it can work. Sign in with a Claude or ChatGPT \
             subscription, or add an API key, then pick the default model. You can come back \
             any time with /settings.",
        );
        tui.other_command("settings", "");
    }
    tui.main_loop().await;
    let _ = tui.renderer.suspend();
    let closing = tui.attention.close();
    tui.write_out(&closing);
    tui.guard.suspend();
    // The terminal is the user's again. A Ctrl+C while the session ends ends azazel at once.
    tokio::select! {
        () = tui.backend.finish() => {}
        _ = tokio::signal::ctrl_c() => {
            tracing::warn!("ctrl+c while the session ended: azazel ends at once");
            std::process::exit(130);
        }
    }
    // Starting again returns to the session by itself: nobody has to be told how.
    if let Some(farewell) = tui.backend.farewell().filter(|_| !tui.ended && !tui.restarting) {
        println!("{farewell}");
    }
    // A release the background check downloaded takes the place of this binary now.
    if tui.env.config.update.auto
        && crate::update::checks_enabled(tui.env.config.update.check)
        && let Some(line) = crate::update::apply_staged(&tui.env.paths)
    {
        println!("{line}");
    }
    0
}

impl Tui {
    fn width(&self) -> usize {
        usize::from(self.renderer.size().0)
    }

    fn say(&mut self, lines: Vec<Line>) {
        self.say_items(lines.into_iter().map(HistoryItem::Line).collect());
    }

    fn info(&mut self, message: &str) {
        let lines = view::notice(message, self.width(), &self.theme);
        self.say(lines);
    }

    fn warn(&mut self, message: &str) {
        let lines = view::warning(message, self.width(), &self.theme);
        self.say(lines);
    }

    fn fail(&mut self, message: &str) {
        let lines = view::error(message, self.width(), &self.theme);
        self.say(lines);
    }

    fn welcome(&mut self) {
        let width = self.width();
        let art = view::banner(width, &self.theme);
        self.say(art);
        let model = self.model_label();
        let model = if model.is_empty() {
            "no model yet".to_string()
        } else {
            model
        };
        let separator = self.theme.glyph(Glyph::Separator);
        let mut heading = vec![
            format!("azazel {}", env!("CARGO_PKG_VERSION")),
            model,
            self.backend.cwd().display().to_string(),
        ];
        heading.extend(self.session.name());
        heading.extend(self.backend.place());
        let lines = view::styled(&heading.join(separator), Role::Accent, width, &self.theme);
        self.say(lines);
        let keys = [
            "enter sends",
            "shift+enter adds a line",
            "/ commands",
            "@ files",
            "! shell",
            "/settings accounts and models",
            "ctrl+c twice quits",
        ];
        self.info(&keys.join(separator));
        let skipped = match self.backend.local() {
            Some(local) => local.app.ignored_settings.join(", "),
            None => String::new(),
        };
        if !skipped.is_empty() {
            self.warn(&format!("this project is not trusted; skipped: {skipped}"));
        }
        let opened = self.backend.opened();
        if opened.resumed {
            self.recap();
        }
        if let Some(notice) = opened.notice {
            self.warn(&notice);
        }
        if let Some(stopped) = opened.interrupted {
            self.warn(&backend::interrupted_notice(&stopped));
        }
    }

    /// Shows where a reopened session left off.
    fn recap(&mut self) {
        let context = self.session.build_context(None);
        let name = self.session.name().unwrap_or_else(|| self.session.id());
        self.info(&format!(
            "resumed session {name} ({} messages in context)",
            context.len()
        ));
        let width = self.width();
        let tail: Vec<Line> = context
            .iter()
            .rev()
            .filter(|entry| !entry.message.text().trim().is_empty())
            .take(2)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .flat_map(|entry| view::recap(&entry.message, width, &self.theme))
            .collect();
        self.say(tail);
    }

    fn model_label(&self) -> String {
        self.backend.model_label()
    }

    async fn main_loop(&mut self) {
        let mut ticker = tokio::time::interval(spinner::INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        // The working indicator moves a cell at a time, which takes more frames than a spinner.
        let mut sweep = tokio::time::interval(shimmer::INTERVAL);
        sweep.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !self.quit {
            if self
                .overlay
                .as_ref()
                .is_some_and(|overlay| overlay.action.abandoned())
            {
                self.overlay = None;
            }
            self.waiting.retain(|request| !request.abandoned());
            self.show_next_request();
            if self.overlay.is_none()
                && let Some((opening, done)) = self.models_waiting.take()
            {
                self.open_models(opening, Some(done)).await;
            }
            self.models_tick();
            self.follow_terminals();
            self.voice_tick();
            self.feed_status();
            self.fold_strip();
            if let Err(e) = self.draw() {
                tracing::error!("terminal write failed: {e}");
                break;
            }
            let popup_deadline = self.popup_deadline();
            let loop_deadline = self.loop_deadline();
            let resize_deadline = self.resize.deadline();
            let cache_deadline = self.cache_deadline();
            let status_deadline = self.status_deadline();
            let strip_deadline = self.strip_deadline();
            tokio::select! {
                event = next_terminal(&mut self.terminal) => match event {
                    Some(Ok(event)) => self.on_terminal(event).await,
                    Some(Err(e)) => {
                        tracing::error!("terminal read failed: {e}");
                        break;
                    }
                    None => break,
                },
                incoming = self.backend.next(), if self.held.is_empty() => {
                    self.on_incoming(incoming).await;
                }
                // What arrived while a full-screen view had the terminal comes first.
                _ = std::future::ready(()), if !self.held.is_empty() => {
                    if let Some(incoming) = self.held.pop_front() {
                        self.on_incoming(incoming).await;
                    }
                }
                done = self.internal_rx.recv() => {
                    if let Some(done) = done {
                        self.on_internal(done).await;
                    }
                }
                // Only the spinner and the clocks of working agents animate; nothing is drawn
                // while everything is idle. An open overlay is checked for an asker that left.
                _ = sweep.tick(), if self.busy && self.indicator() != shimmer::Mode::Static => {}
                _ = ticker.tick(),
                    if self.busy
                        || self.agents_active()
                        || self.overlay.is_some()
                        || self.btw.is_some()
                        || self.voice_animates() => {}
                // Something to tell the terminal although nothing happens: see `tell_terminal`.
                _ = wake_at(self.attention.next_wake()) => {}
                // The popup screen is left a moment after its box closed.
                _ = wake_at(popup_deadline) => {}
                // The window has stopped changing size: the frame at its new size is due.
                _ = wake_at(resize_deadline) => {}
                // The lead's prompt cache has gone unread for as long as it is kept.
                _ = wake_at(cache_deadline) => self.cache_tick(),
                // The clock of the status line moved on, or its program may run again.
                _ = wake_at(status_deadline) => {}
                // The models the cycling keys step through have been shown long enough.
                _ = wake_at(strip_deadline) => {}
                // The next round of a `/loop` is due.
                _ = wake_at(loop_deadline) => self.loop_tick().await,
                // A program in a terminal drew something, or started, or ended.
                _ = term::changed(&mut self.term_changes) => {}
                // An extension set or removed a status segment or lines above the input.
                _ = term::changed(&mut self.extras_changes) => {}
            }
        }
        // A popup that is still up gives the screen back, and what was said behind it reaches
        // the transcript.
        if self.popup.is_some() {
            self.quit = true;
            let _ = self.draw();
        }
    }

    async fn on_incoming(&mut self, incoming: Incoming) {
        match incoming {
            Incoming::Lead(event) => {
                let ended = event.is_terminal();
                let ok = matches!(
                    &event,
                    AgentEvent::Idle { stop } if !matches!(stop, StopReason::Aborted | StopReason::Error)
                );
                self.on_agent(event);
                if ended {
                    // What the run added to the session is there to read from now on.
                    self.sync().await;
                }
                let waiting = self.queued.len();
                self.send_queued().await;
                if ended {
                    self.loop_after_run(ok, self.queued.len() != waiting).await;
                }
            }
            Incoming::Request(request) => self.waiting.push_back(request),
            Incoming::Team(event) => self.on_team(event),
            // What another client of the session typed reads as what was typed here.
            Incoming::UserInput { kind, text } => {
                let said = match kind.as_str() {
                    "prompt" => text,
                    "steer" => format!("(steer) {text}"),
                    _ => format!("(follow-up) {text}"),
                };
                let lines = view::user_prompt(&said, self.width(), &self.theme);
                self.say(lines);
            }
            Incoming::Closed(why) => {
                self.fail(&why);
                // Ending: the last frame is drawn whatever the window is doing.
                self.ended = true;
                self.quit = true;
                let _ = self.draw();
            }
        }
    }

    /// Draws a frame. Blank rows between what is in flight and the input keep the live region
    /// tall enough for the input to stay on the last rows of the screen; the transcript grows
    /// into them.
    fn draw(&mut self) -> io::Result<()> {
        self.tell_terminal();
        // A window that is still changing size is not drawn in: one frame follows at the size
        // it comes to rest at.
        if !self.settle_resize() {
            return Ok(());
        }
        // An overlay shown as a popup has the whole window; the transcript waits.
        if self.draw_popup()? {
            return Ok(());
        }
        let history = std::mem::take(&mut self.transcript);
        self.backdrop.record(&history);
        let (_, rows) = self.renderer.size();
        let (mut viewport, pinned, cursor) = self.viewport();
        if self.overlay.is_none() && self.btw.is_none() && !self.term_open() {
            self.pinned_rows = pinned.len();
        }
        let written = history.iter().map(HistoryItem::rows).sum();
        // A region that grows for a while (a menu, a tall input) rises over the transcript on
        // the screen and gives it back when it shrinks, instead of leaving blank rows.
        let restorable = self.renderer.restorable(&history);
        let (filler, cover) = self
            .floor
            .pad_over(written, viewport.len() + pinned.len(), rows, restorable);
        self.renderer.cover(cover);
        viewport.resize(viewport.len() + filler, String::new());
        let offset = viewport.len() as u16;
        viewport.extend(pinned);
        self.renderer.render(Frame {
            history,
            viewport,
            cursor: cursor.map(|(row, column)| (row + offset, column)),
        })
    }

    /// How the working indicator moves, as `tui.indicator` says.
    fn indicator(&self) -> shimmer::Mode {
        match self.env.config.tui.indicator {
            azazel_core::config::Indicator::Shimmer => shimmer::Mode::Shimmer,
            azazel_core::config::Indicator::Kitt => shimmer::Mode::Kitt,
            azazel_core::config::Indicator::Static => shimmer::Mode::Static,
        }
    }

    /// What the lead is doing, for the rule above the input; nothing while it is idle. The
    /// words say what it is at (`running shell`, `thinking`), or are the phrases of
    /// `tui.indicator_text` in turn while no tool runs; a band of light passes over them.
    fn activity(&self) -> Option<String> {
        if !self.busy {
            return None;
        }
        let config = &self.env.config.tui;
        let mode = self.indicator();
        let since = self.busy_since.elapsed();
        let tick = shimmer::tick_at(since);
        // A run that waits for a usage window to reset says so, with the time that is left.
        let running = match self.backend.facts().waiting {
            Some((what, until)) => {
                let left = until.saturating_sub(azazel_ai::credentials::now_ms() / 1000);
                Some(format!(
                    "{what}{}{} left",
                    self.theme.glyph(Glyph::Separator),
                    azazel_ai::limits::time_left(left)
                ))
            }
            None => self.tools.last().map(|tool| match tool.writing() {
                true => format!("writing the {} call", tool.name),
                false => format!("running {}", tool.name),
            }),
        };
        let (what, phase) = match &running {
            Some(running) => (running.as_str(), tick),
            None => shimmer::phrase(&config.indicator_text, tick, mode).unwrap_or({
                if !self.stream.thinking.is_empty() && !self.stream.saw_text {
                    ("thinking", tick)
                } else {
                    ("working", tick)
                }
            }),
        };
        let accent = config.session_accent.then(|| {
            let name = self.session.name().unwrap_or_else(|| self.session.id());
            shimmer::session_accent(&name)
        });
        let separator = self.theme.glyph(Glyph::Separator);
        Some(format!(
            "{} {}{}",
            self.theme
                .paint(Role::Accent, Spinner::now_in(self.theme.glyphs())),
            shimmer::sweep(what, phase, mode, accent, &self.theme),
            self.theme.paint(
                Role::Muted,
                &format!("{separator}{}{separator}esc to interrupt", format_elapsed(since))
            ),
        ))
    }

    /// The colour of the rules around the input: it tells a `!` shell command and the
    /// permission modes apart.
    fn rule_role(&self) -> Role {
        if self.editor.text().starts_with('!') {
            return Role::Bash;
        }
        match self.backend.mode() {
            PermissionMode::Plan => Role::ModePlan,
            PermissionMode::Manual => Role::Border,
            PermissionMode::AcceptEdits => Role::ModeEdits,
            PermissionMode::Auto => Role::ModeAuto,
            PermissionMode::Bypass => Role::Error,
        }
    }

    fn todo_lines(&self, width: usize) -> Vec<Line> {
        let items = self.backend.facts().todo;
        if items.iter().all(|item| item.status == TodoStatus::Done) {
            return Vec::new();
        }
        let rows: Vec<(String, TodoState)> = items
            .into_iter()
            .map(|item| {
                let state = match item.status {
                    TodoStatus::Pending => TodoState::Pending,
                    TodoStatus::InProgress => TodoState::InProgress,
                    TodoStatus::Done => TodoState::Done,
                };
                (item.text, state)
            })
            .collect();
        let max = usize::try_from(self.env.config.tui.todo_max).unwrap_or(usize::MAX);
        todo::render_at_most(&rows, max, width, &self.theme)
    }

    /// The live region in two parts: what is in flight, which follows the transcript, and what
    /// stays at the bottom of the screen (roster, todo list, the input or an overlay, status).
    /// The cursor is a position in the second part.
    fn viewport(&mut self) -> (Vec<Line>, Vec<Line>, Option<(u16, u16)>) {
        let (columns, rows) = self.renderer.size();
        let (width, height) = (usize::from(columns), usize::from(rows));
        let mut bottom: Vec<Line> = Vec::new();
        let mut cursor;
        self.refresh_pane(overlay::inner_width(width));
        if let Some(overlay) = &self.overlay {
            // No popup (the window is too small, or they are off): the box stands in for the
            // input.
            let room = height.saturating_sub(2).clamp(4, overlay::INLINE_ROWS);
            let (lines, at) = overlay.render(overlay.box_width(width), room, &self.theme);
            cursor = at;
            bottom.extend(lines);
        } else if let Some((lines, at)) = self.term_box(
            modal::box_width(width),
            height.saturating_sub(2).clamp(4, overlay::INLINE_ROWS),
        ) {
            cursor = at;
            bottom.extend(lines);
        } else if let Some(panel) = &mut self.btw {
            let (lines, at) = panel.render(width, height.saturating_sub(2).max(4), &self.theme);
            cursor = at;
            bottom.extend(lines);
        } else {
            if let Some(completion) = &self.completion {
                bottom.extend(completion.list.render(width, COMPLETION_ROWS, &self.theme));
            }
            if let Some(notice) = &self.notice {
                bottom.extend(view::notice(notice, width, &self.theme));
            }
            bottom.extend(self.strip_line(width));
            bottom.extend(self.waiting_lines(width));
            bottom.extend(self.terminal_lines(width));
            let rule = self.rule_role();
            let mut prompt_role = if rule == Role::Border { Role::Accent } else { rule };
            // The rules take the colour of the primary agent the lead is, unless a `!` command
            // or bypass mode has to be seen.
            let (rule, painted) = match self.agent().1 {
                Some(color) if !matches!(rule, Role::Bash | Role::Error) => {
                    let mut theme = self.theme.clone();
                    theme.set_color(Role::Border, color);
                    prompt_role = Role::Border;
                    (Role::Border, Some(theme))
                }
                _ => (rule, None),
            };
            let activity = self.voice_status().or_else(|| self.rule_status());
            let prompt = self.theme.glyphs().prefix(Glyph::Prompt);
            let composer = Composer {
                prompt: &prompt,
                prompt_role,
                placeholder: if self.busy {
                    "Steer the agent, alt+enter queues a follow-up"
                } else {
                    "Ask anything, / for commands, @ for files"
                },
                rule,
                status: activity.as_deref(),
                max_rows: composer::max_rows(height),
            };
            let theme = painted.as_ref().unwrap_or(&self.theme);
            let (lines, (row, column)) = composer::render(&self.editor, &composer, width, theme);
            cursor = Some((bottom.len() as u16 + row, column));
            bottom.extend(lines);
        }
        bottom.push(self.status_line(width));

        // The roster takes the keyboard only while the input has it to give.
        if self.overlay.is_some() || self.btw.is_some() || self.term_open() {
            self.fleet.focus = None;
        }
        // Agents at work, the todo list and the widgets of extensions sit right above the
        // input, under one blank row that sets everything pinned to the bottom apart from the
        // transcript. They give way when the screen is too small for them.
        let mut pinned = self.roster_lines(width);
        pinned.extend(self.todo_lines(width));
        pinned.extend(self.widget_lines(width));
        let mut room = height.saturating_sub(bottom.len());
        if pinned.len() + 1 > room {
            pinned.drain(..(pinned.len() + 1 - room).min(pinned.len()));
        }
        if room > pinned.len() {
            pinned.insert(0, String::new());
        }
        room -= pinned.len();
        cursor = cursor.map(|(row, column)| (row + pinned.len() as u16, column));
        pinned.extend(bottom);

        self.fit_calls(width);
        let mut top = self.in_flight(width);
        // The live region never outgrows the screen: what is in flight gives way first.
        if top.len() > room {
            top.drain(..top.len() - room);
        }
        (top, pinned, cursor)
    }

    /// What the lead is saying and doing right now: the unfinished end of its reply, its
    /// thinking and the tool calls that are running. It follows the transcript.
    fn in_flight(&self, width: usize) -> Vec<Line> {
        let mut top: Vec<Line> = Vec::new();
        if self.busy {
            top.extend(self.stream.markdown.tail(width, &self.theme));
            if self.show_thinking && !self.stream.thinking.is_empty() {
                let wrapped = view::thinking(&self.stream.thinking, width, &self.theme);
                let from = wrapped.len().saturating_sub(4);
                top.extend(wrapped.into_iter().skip(from));
            }
        }
        // Calls in flight, and in the compact style the ones before them whose group is still
        // growing; the followed sub-agent's calls among them, whether the lead works or not.
        top.extend(self.live_calls(width));
        top
    }

    // ------------------------------------------------------------ agent events

    fn on_agent(&mut self, event: AgentEvent) {
        let width = self.width();
        if let Some(ended) = running::track(&mut self.tools, &event) {
            self.tool_finished(calls::Finished::from_ended(None, ended));
        }
        match event {
            AgentEvent::AgentStart => {
                self.busy = true;
                self.busy_since = Instant::now();
                self.run_failed = false;
                self.rate.reset();
            }
            AgentEvent::MessageStart => {
                self.stream = StreamView::default();
                self.rate.abandon();
            }
            AgentEvent::Stream(StreamEvent::TextDelta { delta, .. }) => {
                self.rate.text(&delta, Instant::now());
                self.open_reply();
                self.stream.saw_text = true;
                self.stream.markdown.push(&delta);
                let mut diagrams = terminal::draw_diagrams(&self.caps, self.diagram_room());
                let finished = self
                    .stream
                    .markdown
                    .take_finished(width, &self.theme, &mut diagrams);
                drop(diagrams);
                self.say_items(finished);
            }
            AgentEvent::Stream(StreamEvent::ThinkingDelta { delta, .. }) => {
                self.rate.text(&delta, Instant::now());
                self.stream.thinking.push_str(&delta)
            }
            AgentEvent::Stream(StreamEvent::ToolCallDelta { delta, .. }) => self.rate.arguments(&delta),
            AgentEvent::Stream(StreamEvent::BlockEnd {
                block: Block::Thinking { .. },
                ..
            }) => {
                let thinking = std::mem::take(&mut self.stream.thinking);
                if self.show_thinking && !thinking.trim().is_empty() {
                    let lines = view::thinking(thinking.trim(), width, &self.theme);
                    self.say(lines);
                }
            }
            AgentEvent::MessageEnd { message } => {
                let output = message.meta.as_ref().map_or(0, |meta| meta.usage.output);
                self.rate.end(output, Instant::now());
                self.voice_reply(&message);
                if !self.stream.saw_text && !message.text().trim().is_empty() {
                    self.open_reply();
                    self.stream.markdown.push(&message.text());
                }
                let mut diagrams = terminal::draw_diagrams(&self.caps, self.diagram_room());
                let rest = self.stream.markdown.finish(width, &self.theme, &mut diagrams);
                drop(diagrams);
                self.say_items(rest);
                self.stream = StreamView::default();
            }
            AgentEvent::Retry {
                attempt,
                model,
                stalled_secs,
            } => {
                self.stream = StreamView::default();
                self.rate.abandon();
                self.info(&crate::routing::retry_line(
                    &model.to_string(),
                    attempt,
                    stalled_secs,
                ));
            }
            AgentEvent::Fallback { from, to } => {
                self.info(&format!("{from} is unavailable; falling back to {to}"))
            }
            AgentEvent::Account {
                provider,
                from,
                to,
                until,
                limit,
            } => {
                // The status line shows the windows of the account requests go to now.
                if let Some(local) = self.backend.local() {
                    local.account_changed();
                }
                self.info(&crate::accounts::switch_line(
                    &provider, &from, &to, until, &limit,
                ))
            }
            AgentEvent::TierDropped { model, tier } => {
                self.info(&crate::routing::tier_dropped_line(&model, tier))
            }
            AgentEvent::Rule { name } => {
                // What already scrolled away stays on screen; the rest of the reply is dropped.
                let mut diagrams = terminal::draw_diagrams(&self.caps, self.diagram_room());
                let rest = self.stream.markdown.finish(width, &self.theme, &mut diagrams);
                drop(diagrams);
                self.say_items(rest);
                self.stream = StreamView::default();
                self.rate.abandon();
                self.info(&format!("rule {name} stopped the reply; retrying"));
            }
            AgentEvent::Compaction {
                tokens_before,
                tokens_after,
            } => self.info(&format!(
                "context compacted: {} → {} tokens",
                format_tokens(tokens_before),
                format_tokens(tokens_after)
            )),
            AgentEvent::Idle { stop } => {
                let took = self.busy_since.elapsed();
                self.end_of_run();
                // An interrupted run keeps what is queued for the user to send or take back.
                self.run_ended = stop != StopReason::Aborted;
                self.voice_run_ended(&stop);
                // With a message queued, or a loop that sends the next round, the work goes
                // on: nothing to call the user for yet.
                if self.run_ended && self.queued.is_empty() && self.looping.is_none() {
                    self.attend_end(None, took);
                }
                match stop {
                    StopReason::Aborted => self.info("interrupted"),
                    StopReason::Length => self.warn("the reply was cut off at the model's output limit"),
                    _ => {}
                }
            }
            AgentEvent::Failed { error } => {
                let took = self.busy_since.elapsed();
                self.end_of_run();
                self.run_failed = true;
                self.attend_end(Some(&error), took);
                self.fail(&error);
            }
            AgentEvent::ToolStart { .. }
            | AgentEvent::ToolUpdate { .. }
            | AgentEvent::ToolEnd { .. }
            | AgentEvent::TurnStart { .. }
            | AgentEvent::TurnEnd
            | AgentEvent::Stream(_) => {}
        }
    }

    /// Sets a reply apart from what precedes it with one blank row, once per message.
    fn open_reply(&mut self) {
        if !std::mem::replace(&mut self.stream.opened, true) {
            self.say(vec![String::new()]);
        }
    }

    fn end_of_run(&mut self) {
        self.flush_calls();
        self.busy = false;
        self.tools.clear();
        self.stream = StreamView::default();
        self.rate.abandon();
        // The agent may have switched branches, and has probably changed files.
        self.branch = self.backend.branch();
        self.status.files_changed();
        if let Some(chain) = self.restore_chain.take() {
            self.backend.set_chain(chain);
        }
    }

    /// Brings what the UI reads of the session up to date with the session itself.
    async fn sync(&mut self) {
        self.session = self.backend.session().await;
    }

    /// The directory the session works in, when it is one of this machine: paths in it can
    /// then be opened from here.
    fn local_cwd(&self) -> Option<std::path::PathBuf> {
        self.backend.local().map(|local| local.app.paths.cwd.clone())
    }

    /// The application, for `what`, which needs the session to be in this process. Says so
    /// when the open one is not.
    fn app(&mut self, what: &str) -> Option<Arc<App>> {
        let app = self.backend.local().map(|local| local.app.clone());
        if app.is_none() {
            self.here_only(what);
        }
        app
    }

    /// Says that `what` needs a session in this process, when the open one is not.
    fn here_only(&mut self, what: &str) -> bool {
        let here = self.backend.local().is_some();
        if !here {
            self.fail(&format!(
                "{what} works only in a session that runs in this process; this one runs elsewhere"
            ));
        }
        here
    }

    async fn on_internal(&mut self, done: Internal) {
        let (output, status) = match done {
            Internal::Shell { output, status } => (output, status),
            Internal::Files(files) => return self.files = Some(files),
            Internal::Setting { path, value } => return self.apply_setting(&path, &value),
            Internal::Request(request) => return self.waiting.push_back(request),
            Internal::Btw(update) => return self.on_btw(update),
            Internal::CommitDraft(draft) => return self.on_commit_draft(draft),
            Internal::Side(event) => return self.on_side(event).await,
            Internal::LoopVerdict { id, verdict } => return self.on_loop_verdict(id, verdict).await,
            #[cfg(feature = "detach")]
            Internal::Trigger(event) => return self.on_trigger(event).await,
            Internal::Sent(sent) => return self.on_sent(sent),
            Internal::Usage(entries) => return self.on_usage(entries),
            Internal::Clipboard(clip) => return self.on_clipboard(clip),
            Internal::ClipboardText(text) => return self.on_clipboard_text(text),
            Internal::Migrate(step) => return self.on_migrate(step).await,
            Internal::StatusCommand(line) => return self.on_status_command(line),
            Internal::ChangedFiles(changed) => return self.on_changed_files(changed),
            Internal::Models(update) => return self.on_models(update).await,
            Internal::OpenModels(opening, done) => return self.models_waiting = Some((opening, done)),
            Internal::Command(outcome) => {
                return match outcome {
                    CommandOutcome::Done => {}
                    CommandOutcome::Show(shown) => self.info(&shown),
                    CommandOutcome::Prompt { text, model } => self.prompt_on(text, model).await,
                    CommandOutcome::Error(e) => self.fail(&e),
                };
            }
            #[cfg(feature = "voice")]
            Internal::Voice(done) => return self.voice_done(done).await,
        };
        self.notice = None;
        self.branch = self.backend.branch();
        self.status.files_changed();
        let shown: Vec<&str> = output.lines().collect();
        let from = shown.len().saturating_sub(40);
        let mut display = shown[from..].join("\n");
        if from > 0 {
            display = format!(
                "{} {from} earlier lines\n{display}",
                self.theme.glyph(Glyph::Ellipsis)
            );
        }
        self.info(&format!("{display}\n{status}"));
    }

    // ------------------------------------------------------------ requests from agents

    fn show_next_request(&mut self) {
        if self.overlay.is_none() {
            self.overlay = self.deferred.pop_front();
        }
        while self.overlay.is_none() {
            let Some(request) = self.waiting.pop_front() else {
                return;
            };
            self.show_request(request);
        }
    }

    /// Tells extensions (notification hooks) that something wants the user's attention.
    fn announce(&self, level: Level, text: String) {
        // A session in another process tells its own extensions.
        let Some(local) = self.backend.local() else {
            return;
        };
        let bus = local.app.registry.bus.clone();
        tokio::spawn(async move {
            bus.emit(&mut azazel_core::Event::Notification { level, text })
                .await;
        });
    }

    fn show_request(&mut self, request: UiRequest) {
        let pick = |labels: &[&str]| labels.iter().map(|l| SelectItem::new(*l, "")).collect::<Vec<_>>();
        // The notification hooks hear of it first; then, for what an agent waits on, the bell
        // and a notification when the user is looking elsewhere.
        let needs_you = match &request {
            UiRequest::Notify { level, text } => {
                self.announce(*level, text.clone());
                None
            }
            UiRequest::Ask { question, .. } => Some(question.text.clone()),
            UiRequest::Form { questions, .. } => Some(match questions.first() {
                Some(first) => format!("{} (and {} more questions)", first.text, questions.len() - 1),
                None => "the agent asks".to_string(),
            }),
            UiRequest::Confirm { prompt, .. }
            | UiRequest::Select { prompt, .. }
            | UiRequest::Input { prompt, .. } => {
                self.announce(Level::Info, prompt.clone());
                None
            }
            UiRequest::Approval { request, .. } => Some(format!(
                "{} needs permission to use {}",
                request.agent.id, request.tool
            )),
            UiRequest::Plan { .. } => Some("a plan is ready for review".to_string()),
            // Opened by the user a moment ago.
            UiRequest::Settings { .. } => None,
        };
        if let Some(text) = needs_you {
            self.announce(Level::Info, text.clone());
            self.attend(Kind::NeedsYou, &text, None);
        }
        match request {
            UiRequest::Notify { level, text } => match level {
                Level::Info => self.info(&text),
                Level::Warn => self.warn(&text),
                Level::Error => self.fail(&text),
            },
            UiRequest::Ask { question, reply } => {
                let form = form::Form::new(vec![question], &self.other_row());
                self.overlay = Some(Overlay::form(form, Action::Ask { reply }));
            }
            UiRequest::Form { questions, reply } => {
                let form = form::Form::new(questions, &self.other_row());
                self.overlay = Some(Overlay::form(form, Action::Form { reply }));
            }
            UiRequest::Confirm { prompt, reply } => {
                let items = pick(&["Yes", "No"]);
                self.overlay = Some(Overlay::select(
                    "Confirm",
                    vec![prompt],
                    items,
                    false,
                    Action::Confirm { reply },
                ));
            }
            UiRequest::Select {
                prompt,
                options,
                reply,
            } => {
                // A long list (every model of a provider) is easier to search than to scroll.
                let filterable = options.len() > 12;
                let items = options
                    .iter()
                    .map(|option| overlay::option_item(option))
                    .collect();
                let (title, body) = overlay::heading(&prompt, "Choose");
                self.overlay = Some(Overlay::select(
                    title,
                    body,
                    items,
                    filterable,
                    Action::UiSelect { reply },
                ));
            }
            UiRequest::Input {
                prompt,
                secret,
                reply,
            } => {
                let action = Action::UiInput { reply };
                let (title, body) = overlay::heading(&prompt, "Input");
                self.overlay = Some(match secret {
                    true => Overlay::secret(title, body, action),
                    false => Overlay::input(title, body, action),
                });
            }
            UiRequest::Approval { request, reply } => {
                let body = overlay::approval_body(&request.tool, &request.subjects, &request.input);
                let (items, rows) =
                    approval_rows(request.rule.as_deref(), request.destructive, request.secret);
                let title = format!("{} wants to use {}", request.agent.id, request.tool);
                // A stray Enter must not approve a command that destroys work: start on "Deny".
                let deny = rows.iter().position(|row| *row == ApprovalRow::Deny);
                let start = deny.filter(|_| request.destructive).unwrap_or(0);
                let action = Action::Approval { reply, rows };
                // The note stands out under the subjects; a call that destroys work has the
                // whole box in the warning colour.
                self.overlay = Some(
                    Overlay::select(title, body, items, false, action)
                        .starting_at(start)
                        .with_warning(request.note.clone())
                        .destructive(request.destructive),
                );
            }
            UiRequest::Plan { path, plan, reply } => {
                // The plan goes into the transcript, where it stays to be scrolled and re-read,
                // and into the box, where it is read now: ctrl+o shows all of a long one.
                let width = self.width();
                let mut lines = view::styled(
                    &format!("Plan: {}", path.display()),
                    Role::Accent,
                    width,
                    &self.theme,
                );
                lines.extend(markdown::render(&plan, width, &self.theme));
                self.say(lines);
                let items = pick(&["Approve and run", "Revise"]);
                self.overlay = Some(
                    Overlay::select("Plan review", vec![plan], items, false, Action::Plan { reply })
                        .markdown(),
                );
            }
            UiRequest::Settings { screen, reply } => {
                self.overlay = Some(Overlay::settings(screen, Action::Settings { reply }));
            }
        }
    }

    /// The row of a question that lets the user type an answer, drawn in the symbols in use.
    fn other_row(&self) -> String {
        format!("Other{}", self.theme.glyph(Glyph::Ellipsis))
    }

    // ------------------------------------------------------------ terminal input

    async fn on_terminal(&mut self, event: Event) {
        // Windows Terminal keeps `ctrl+v` for its own paste. When the clipboard holds no text
        // (a screenshot) it pastes nothing, and only the key's release reaches azazel.
        if cfg!(windows)
            && let Event::Key(key) = &event
            && input::is_paste_key(key)
        {
            if key.kind != KeyEventKind::Release {
                self.paste_key_down = true;
            } else if input::press_was_kept(
                std::mem::take(&mut self.paste_key_down),
                self.last_burst.map(|at| at.elapsed()),
            ) {
                self.on_key(KeyEvent::new(key.code, key.modifiers)).await;
                return;
            }
        }
        // Windows consoles have no bracketed paste: a paste is a burst of key presses.
        let event = if cfg!(windows)
            && let Event::Key(key) = &event
            && let Some(first) = input::typed_char(key)
        {
            let (burst, after) = input::read_burst(first);
            if input::is_paste(&burst) {
                self.on_paste(&burst);
            } else if burst.chars().count() == 1 {
                self.on_key(*key).await;
            } else {
                for typed in burst.chars() {
                    self.on_key(input::key_for(typed)).await;
                }
            }
            self.last_burst = Some(Instant::now());
            // What ended the burst is handled like any other event.
            match after {
                Some(event) => event,
                None => return,
            }
        } else {
            event
        };
        match event {
            Event::Key(key) if key.kind != KeyEventKind::Release => self.on_key(key).await,
            Event::Paste(pasted) => self.on_paste(&pasted),
            Event::Resize(width, height) => self.size_reported((width, height)),
            Event::FocusGained => self.attention.focus(true),
            Event::FocusLost => self.attention.focus(false),
            Event::Mouse(event) => self.on_mouse(event).await,
            _ => {}
        }
    }

    /// The mouse, which the terminal reports only while a popup is up: what it does inside the
    /// box goes to the overlay as a key would; outside the box it does nothing.
    async fn on_mouse(&mut self, event: MouseEvent) {
        let Some(placed) = self.popup.as_ref().and_then(popup::Popup::placed) else {
            return;
        };
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        let Some(mouse) = Mouse::within(&event, placed) else {
            if event.kind == MouseEventKind::Moved {
                overlay.pointer_left();
            }
            return;
        };
        let outcome = overlay.on_mouse(mouse);
        if let overlay::Kind::Status(editor) = &overlay.kind {
            let trying = editor.config();
            self.set_statusline(trying, false);
        }
        self.overlay_outcome(outcome).await;
    }

    async fn on_key(&mut self, key: KeyEvent) {
        self.notice = None;
        self.run_failed = false;
        // Another step of the cycle shows it again.
        self.strip = None;
        if self.voice_key(&key) {
            return;
        }
        if let Some(overlay) = &mut self.overlay {
            // Feedback for the agent can be written in the user's editor too.
            if panels::is_text_input(overlay) && self.keymap.action_for(&key) == Some("external_editor") {
                return self.external_editor().await;
            }
            let outcome = overlay.on_key(&key);
            // `/statusline` shows on the line itself what is being chosen.
            if let overlay::Kind::Status(editor) = &overlay.kind {
                let trying = editor.config();
                self.set_statusline(trying, false);
            }
            if let Outcome::Hotkey { index, key } = outcome {
                return self.overlay_hotkey(index, key).await;
            }
            // The popup of an agent stays open while it is typed to or stopped.
            match outcome {
                Outcome::Send(typed) => return self.pane_send(typed).await,
                Outcome::Stop => return self.pane_stop().await,
                // A chat opened from the agent hub goes back to it.
                Outcome::Cancelled if self.chat_back() => return,
                outcome => return self.overlay_outcome(outcome).await,
            }
        }
        if self.term_open() {
            return self.term_key(&key);
        }
        if self.btw.is_some() {
            return self.btw_key(key).await;
        }
        // The roster has the keyboard: the key is its, or takes the keyboard back to the input.
        if self.fleet.focus.is_some() && self.fleet_key(&key).await {
            return;
        }
        self.input_key(key).await;
    }

    /// What a key did to the open overlay, other than the keys it keeps open for.
    async fn overlay_outcome(&mut self, outcome: Outcome) {
        if matches!(outcome, Outcome::ReadBody) {
            return self.read_overlay_body().await;
        }
        // `/models` stays open while what it asked for is done.
        if let Outcome::Models(request) = outcome {
            return self.models_request(request).await;
        }
        if !matches!(outcome, Outcome::Pending)
            && let Some(overlay) = self.overlay.take()
        {
            self.finish_overlay(overlay, outcome).await;
        }
    }

    /// A key for the input: completion, the keymap, the editor.
    async fn input_key(&mut self, key: KeyEvent) {
        // `left` twice in an empty input opens the agent hub.
        if self.hub_tap(&key) {
            return;
        }
        // `down` in an empty input goes to the roster, when agents are at work.
        if key.code == KeyCode::Down
            && key.modifiers.is_empty()
            && self.editor.is_empty()
            && self.completion.is_none()
            && self.enter_fleet()
        {
            return;
        }
        if let Some(completion) = &mut self.completion {
            match key.code {
                KeyCode::Up => return completion.list.move_up(),
                KeyCode::Down => return completion.list.move_down(),
                KeyCode::Tab => return self.accept_completion(),
                KeyCode::Esc => {
                    self.completion = None;
                    return;
                }
                KeyCode::Enter if completion.slash => {
                    // Enter on a half-typed command completes it; a complete one runs, even when
                    // a longer command that starts with it (`/mode`, `/model`) is highlighted.
                    let typed = self.editor.text().trim().to_string();
                    let complete = completion.list.items().iter().any(|item| item.label == typed);
                    let chosen = completion.list.selected_item().map(|item| item.label.clone());
                    if !complete && chosen.is_some_and(|label| label != typed) {
                        return self.accept_completion();
                    }
                }
                _ => {}
            }
        }
        // Vim keys come before the keymap: in normal mode letters are commands, and `esc` leaves
        // insert mode before it interrupts anything.
        if let Some(vim) = &mut self.vim
            && vim.handle_key(&mut self.editor, &key)
        {
            return self.refresh_completion();
        }
        // A key azazel has no action for may be an extension's.
        if self.extension_key(&key) {
            return;
        }
        let action = self.keymap.action_for(&key).map(str::to_string);
        match action.as_deref() {
            Some("submit") => self.submit(false).await,
            Some("follow_up") => self.submit(true).await,
            Some("newline") => self.editor.insert_newline(),
            Some("abort") => self.on_escape().await,
            Some("clear_or_exit") => self.on_interrupt(),
            Some("exit") if self.editor.is_empty() => self.quit = true,
            Some("cycle_mode") => {
                // Bypass is not a step of the cycle: it is entered by name, and confirmed.
                self.set_mode(self.backend.mode().next()).await;
            }
            Some("toggle_tool_output") => self.toggle_tool_output(),
            Some("toggle_thinking") => {
                self.show_thinking = !self.show_thinking;
                self.notice = Some(format!(
                    "thinking: {}",
                    if self.show_thinking { "shown" } else { "hidden" }
                ));
            }
            Some("model_picker") => self.open_model_picker().await,
            Some("history_search") => self.open_history(),
            Some("paste_image") => self.paste_image(),
            Some("paste_text") => self.paste_text(),
            // In an empty input there is nothing to complete: the key steps to the next
            // primary agent.
            Some("complete") if self.editor.is_empty() && self.completion.is_none() => {
                self.cycle_agent().await
            }
            Some("complete") => {
                self.refresh_completion();
                if self.completion.as_ref().is_some_and(|c| c.list.len() == 1) {
                    self.accept_completion();
                }
            }
            Some("agent_hub") => self.open_agent_hub(),
            Some("external_editor") => self.external_editor().await,
            // With nothing queued the key is the editor's (up, into the history).
            Some("dequeue") if !self.queued.is_empty() => self.dequeue(),
            Some("stash") => self.stash(),
            Some("cycle_thinking") => self.cycle_thinking().await,
            Some("cycle_model") => self.cycle_model(true).await,
            Some("cycle_model_back") => self.cycle_model(false).await,
            Some("transcript") => self.open_transcript().await,
            Some("retry") => self.slash("retry").await,
            Some("background") if self.foreground_call().is_some() => {
                if let Some(call_id) = self.foreground_call() {
                    self.background_shell(&call_id);
                }
            }
            Some("terminal") => self.show_newest_terminal(),
            Some("voice") => self.voice_pressed(&key).await,
            _ => {
                if self.editor.handle_key(&key) {
                    self.refresh_completion();
                }
            }
        }
    }

    async fn on_escape(&mut self) {
        // A loop ends with the round it is in.
        if self.looping.is_some() {
            self.stop_loop("loop stopped");
            if self.busy {
                self.backend.abort();
            }
            return;
        }
        if self.busy {
            self.backend.abort();
            return;
        }
        if !self.editor.is_empty() {
            return;
        }
        let now = Instant::now();
        if self
            .last_escape
            .is_some_and(|at| now.duration_since(at) < DOUBLE_PRESS)
        {
            self.last_escape = None;
            self.open_rewind().await;
        } else {
            self.last_escape = Some(now);
            self.notice = Some("press esc again to rewind".to_string());
        }
    }

    fn on_interrupt(&mut self) {
        if !self.editor.is_empty() {
            self.editor.set_text("");
            self.completion = None;
            return;
        }
        let now = Instant::now();
        if self
            .last_interrupt
            .is_some_and(|at| now.duration_since(at) < DOUBLE_PRESS)
        {
            self.quit = true;
        } else {
            self.last_interrupt = Some(now);
            self.notice = Some("press ctrl+c again to quit".to_string());
        }
    }

    async fn submit(&mut self, follow_up: bool) {
        if self.editor.is_empty() {
            return;
        }
        // What is echoed keeps a folded paste as its token; what is sent has the text. An
        // attached image is its label, `[Image #1]`, in both.
        let shown = self.editor.shown();
        let images = self.claim_images();
        let entered = self.editor.take();
        self.completion = None;
        let trimmed = entered.trim();
        if let Some(command) = trimmed.strip_prefix('/') {
            let command = command.to_string();
            // A command that turns into a prompt takes the images along.
            self.carried = images;
            return self.slash(&command).await;
        }
        self.carried.clear();
        if let Some(command) = trimmed.strip_prefix('!') {
            // `!!command` runs the same way but its output stays out of the agent's context.
            let (command, share) = match command.strip_prefix('!') {
                Some(private) => (private, false),
                None => (command, true),
            };
            let command = command.trim().to_string();
            self.remember(entered.trim());
            return self.user_shell(command, share);
        }
        // `# note`: remembered for later sessions, and not a prompt.
        if let Some(note) = quick::memory_note(&entered) {
            let lines = view::user_prompt(trimmed, self.width(), &self.theme);
            self.say(lines);
            let Some(local) = self.backend.local() else {
                self.here_only("a `# note` for the memory");
                return;
            };
            let (services, note) = (local.app.registry.services.clone(), note.to_string());
            let done = self.internal_tx.clone();
            tokio::spawn(async move {
                let _ = done.send(Internal::Command(quick::remember(&services, &note).await));
            });
            return;
        }
        // `@agent text`: for an agent of the team, not for the lead.
        if let Some((name, text)) = fleet::mention(trimmed)
            && self.mention(&shown, name, text).await
        {
            self.remember(trimmed);
            return;
        }
        // Nothing can answer yet: keep what was typed and set a model up first.
        if !self.busy && !self.backend.has_model() {
            self.set_draft(&entered, images);
            self.warn("no model is set up yet: sign in or add an API key, then choose a default model");
            return self.other_command("settings", "");
        }
        if self.busy && follow_up {
            // Held here, not by the agent, so that it is sent like any typed prompt.
            return self.queue_message(entered, images);
        }
        self.prompts.push(entered.clone());
        self.remember(&entered);
        let lines = view::user_prompt(&shown, self.width(), &self.theme);
        self.say(lines);
        if self.busy {
            self.backend.steer(entered, images);
            return;
        }
        self.send_prompt(entered, images).await;
    }

    async fn send_prompt(&mut self, prompt: String, images: Vec<Block>) {
        if let Err(reason) = self.backend.prompt(prompt, images).await {
            self.fail(&format!("prompt rejected: {reason}"));
            if let Some(chain) = self.restore_chain.take() {
                self.backend.set_chain(chain);
            }
        }
    }

    /// `!command`: runs in the user's shell where the session works. With `share` the agent is
    /// told what was run and what it printed.
    fn user_shell(&mut self, command: String, share: bool) {
        if command.is_empty() {
            return;
        }
        let echo = if share { "!" } else { "!!" };
        let lines = view::user_prompt(&format!("{echo}{command}"), self.width(), &self.theme);
        self.say(lines);
        self.notice = Some(format!(
            "running `{command}`{}",
            self.theme.glyph(Glyph::Ellipsis)
        ));
        self.backend.shell(command, share, self.internal_tx.clone());
    }

    // ------------------------------------------------------------ completion

    /// The files `@` completes. Those of a session in another process were asked for when
    /// the UI started; until they are there, there are none.
    fn project_files(&mut self) -> &[String] {
        let root = self.local_cwd();
        self.files.get_or_insert_with(|| match root {
            Some(root) => project_files(&root),
            None => Vec::new(),
        })
    }

    fn refresh_completion(&mut self) {
        let typed = self.editor.text().to_string();
        if typed.starts_with('/') && !typed.contains(char::is_whitespace) {
            let items: Vec<SelectItem> = self
                .commands()
                .into_iter()
                .map(|(name, about)| SelectItem::new(format!("/{name}"), about))
                .collect();
            let mut list = SelectList::new(items);
            list.set_filter(&typed[1..]);
            self.completion = (!list.is_empty()).then_some(Completion {
                list,
                slash: true,
                dirs: false,
            });
            return;
        }
        if let Some(partial) = typed.strip_prefix("/cd ") {
            let items: Vec<SelectItem> = self
                .local_cwd()
                .map(|cwd| places::directories(&cwd, partial))
                .unwrap_or_default()
                .into_iter()
                .map(|dir| SelectItem::new(dir, ""))
                .collect();
            let list = SelectList::new(items);
            self.completion = (!list.is_empty()).then_some(Completion {
                list,
                slash: false,
                dirs: true,
            });
            return;
        }
        let token = self.editor.token_before_cursor().to_string();
        let Some(partial) = token.strip_prefix('@') else {
            self.completion = None;
            return;
        };
        if !matches!(&self.completion, Some(c) if !c.slash && !c.dirs) {
            // At the start of a prompt `@` may name an agent: those come before the files.
            let mut items = match typed.len() == token.len() {
                true => self.mention_items(),
                false => Vec::new(),
            };
            items.extend(
                self.project_files()
                    .iter()
                    .map(|file| SelectItem::new(file.as_str(), "")),
            );
            self.completion = Some(Completion {
                list: SelectList::new(items),
                slash: false,
                dirs: false,
            });
        }
        if let Some(completion) = &mut self.completion {
            completion.list.set_filter(partial);
            if completion.list.is_empty() {
                self.completion = None;
            }
        }
    }

    fn accept_completion(&mut self) {
        let Some(completion) = self.completion.take() else {
            return;
        };
        let Some(item) = completion.list.selected_item() else {
            return;
        };
        if completion.dirs {
            // What is below the directory is offered next.
            self.editor.set_text(&format!("/cd {}", item.label));
            self.refresh_completion();
        } else if completion.slash {
            self.editor.set_text(&format!("{} ", item.label));
        } else {
            self.editor
                .replace_token_before_cursor(&format!("@{} ", item.label));
        }
    }

    // ------------------------------------------------------------ overlays

    async fn finish_overlay(&mut self, overlay: Overlay, outcome: Outcome) {
        let Overlay { action, kind, .. } = overlay;
        let (choice, typed) = match outcome {
            Outcome::Pending => return,
            Outcome::Cancelled => {
                // The status line goes back to what it was before `/statusline` tried others on.
                if let overlay::Kind::Status(editor) = kind {
                    self.set_statusline(editor.before(), false);
                }
                return action.cancel();
            }
            Outcome::Chosen(index) => (index, String::new()),
            Outcome::Text(typed) => (0, typed),
            // The answers of a form go to whoever asked.
            Outcome::Answers(mut answers) => {
                return match action {
                    Action::Ask { reply } => drop(reply.send(Ok(answers.pop().unwrap_or_default()))),
                    Action::Form { reply } => drop(reply.send(Ok(answers))),
                    Action::Migrate(offer) => self.migrate_picked(*offer, &answers),
                    other => other.cancel(),
                };
            }
            // Handled where the key was pressed; the overlay stays open.
            Outcome::Hotkey { .. }
            | Outcome::ReadBody
            | Outcome::Send(_)
            | Outcome::Stop
            | Outcome::Models(_) => return,
        };
        match action {
            // A form answers through `Outcome::Answers`; the popup of an agent only closes.
            action @ (Action::Ask { .. } | Action::Form { .. } | Action::Migrate(_)) => action.cancel(),
            Action::MigrateGo(pick) => self.migrate_go(*pick, choice),
            Action::AgentPane | Action::Usage(_) | Action::Models { .. } => {}
            // The screen goes back to the command that showed it, which knows what it asked for.
            Action::Settings { reply } => {
                if let overlay::Kind::Settings(screen) = kind {
                    let _ = reply.send(screen);
                }
            }
            Action::Confirm { reply } => drop(reply.send(Ok(choice == 0))),
            Action::UiSelect { reply } => drop(reply.send(Ok(choice))),
            Action::UiInput { reply } => drop(reply.send(Ok(typed))),
            Action::Approval { reply, rows } => match rows.get(choice) {
                Some(ApprovalRow::Once) => drop(reply.send(Some(ApprovalDecision::AllowOnce))),
                Some(ApprovalRow::Session) => drop(reply.send(Some(ApprovalDecision::AllowSession))),
                Some(ApprovalRow::Always) => drop(reply.send(Some(ApprovalDecision::Always))),
                Some(ApprovalRow::Deny) | None => {
                    drop(reply.send(Some(ApprovalDecision::Deny { feedback: None })))
                }
                Some(ApprovalRow::DenyWhy) => {
                    let body = vec!["Tell the agent what to do instead.".to_string()];
                    self.overlay = Some(Overlay::input("Deny", body, Action::DenyFeedback { reply }));
                }
            },
            Action::DenyFeedback { reply } => drop(reply.send(Some(ApprovalDecision::Deny {
                feedback: Some(typed).filter(|t| !t.trim().is_empty()),
            }))),
            Action::Plan { reply } => {
                if choice == 0 {
                    drop(reply.send(Ok(PlanDecision::Approve)));
                } else {
                    let body = vec!["What should change in the plan?".to_string()];
                    self.overlay = Some(Overlay::input(
                        "Revise the plan",
                        body,
                        Action::PlanFeedback { reply },
                    ));
                }
            }
            Action::PlanFeedback { reply } => drop(reply.send(Ok(PlanDecision::Revise(typed)))),
            Action::Model(specs) => {
                if let Some(spec) = specs.get(choice) {
                    self.set_model(&[spec.as_str()]).await;
                }
            }
            Action::Statusline => {
                if let overlay::Kind::Status(editor) = kind {
                    self.save_statusline(editor.config());
                }
            }
            Action::Thinking => {
                // The list holds the levels the model takes, by name.
                let picked = match &kind {
                    overlay::Kind::Select { list, .. } => list.items().get(choice),
                    _ => None,
                };
                if let Some(level) = picked.and_then(|item| item.label.parse().ok()) {
                    self.set_thinking(level).await;
                }
            }
            Action::Mode => {
                if let Some(mode) = PermissionMode::ALL.get(choice) {
                    self.set_mode(*mode).await;
                }
            }
            Action::Agent(names) => {
                if let Some(name) = names.get(choice) {
                    self.set_agent(name, false).await;
                }
            }
            Action::Bypass => {
                if choice == 1 {
                    self.apply_mode(PermissionMode::Bypass).await;
                }
            }
            Action::Theme(names) => {
                if let Some(name) = names.get(choice) {
                    self.set_theme(name);
                }
            }
            Action::Glyphs(levels) => {
                if let Some(level) = levels.get(choice) {
                    self.choose_glyphs(*level);
                }
            }
            Action::Resume(paths) => {
                if let Some(path) = paths.get(choice) {
                    self.resume_file(path.clone()).await;
                }
            }
            Action::Tree(rows, _) => {
                if let Some((id, is_user, prompt)) = rows.get(choice) {
                    self.jump_to(id, *is_user, prompt).await;
                }
            }
            Action::History(prompts) => {
                if let Some(prompt) = prompts.get(choice) {
                    self.editor.set_text(prompt);
                }
            }
            Action::Rewind(points) => {
                if let Some(point) = points.get(choice) {
                    let items = ["Code and conversation", "Code only", "Conversation only"]
                        .iter()
                        .map(|label| SelectItem::new(*label, ""))
                        .collect();
                    let body = vec![if point.prompt.is_empty() {
                        "Back to the files as they were before a rewind".to_string()
                    } else {
                        format!("Back to before: {}", first_line(&point.prompt))
                    }];
                    self.overlay = Some(Overlay::select(
                        "Rewind what?",
                        body,
                        items,
                        false,
                        Action::RewindWhat(point.clone()),
                    ));
                }
            }
            Action::RewindWhat(point) => self.rewind(point, choice != 2, choice != 1).await,
            Action::Agents(ids) => {
                if let Some(id) = ids.get(choice) {
                    self.open_agent_chat(id);
                }
            }
            Action::AgentMessage(id) => self.message_agent(&id, &typed).await,
            Action::Terminals(ids) => {
                if let Some(id) = ids.get(choice) {
                    self.show_terminal(id);
                }
            }
            #[cfg(feature = "mcp")]
            Action::McpServer(servers) => {
                if let Some((name, enabled)) = servers.get(choice) {
                    let actions: Vec<&'static str> = if *enabled {
                        vec!["reconnect", "disable", "login", "logout"]
                    } else {
                        vec!["enable"]
                    };
                    let items = actions
                        .iter()
                        .map(|action| SelectItem::new(*action, ""))
                        .collect();
                    let action = Action::McpChoice {
                        server: name.clone(),
                        actions,
                    };
                    self.overlay = Some(Overlay::select(
                        format!("MCP server {name}"),
                        Vec::new(),
                        items,
                        false,
                        action,
                    ));
                }
            }
            #[cfg(feature = "mcp")]
            Action::McpChoice { server, actions } => {
                if let Some(action) = actions.get(choice) {
                    self.other_command("mcp", &format!("{action} {server}"));
                }
            }
            action @ (Action::CommitStage(_) | Action::Commit { .. }) => {
                self.finish_commit(action, choice, typed)
            }
            action @ (Action::Share(_) | Action::Bug { .. } | Action::BugIssue(_)) => {
                self.finish_share(action, choice)
            }
            Action::TerminalSetup(change) => self.finish_terminal_setup(&change, choice),
            action @ (Action::CopyCode(_)
            | Action::Context(_)
            | Action::Jobs(_)
            | Action::JobChoice(_)
            | Action::Permissions(_)
            | Action::PermissionAdd
            | Action::PermissionScope(..)) => self.finish_panel(action, choice, typed).await,
            action @ (Action::Sessions { .. }
            | Action::SessionRename(..)
            | Action::SessionDelete(..)
            | Action::ConsultQuestion(_)
            | Action::Side(_)
            | Action::SideAnswer { .. }
            | Action::TreeLabel { .. }) => self.finish_session_overlay(action, choice, typed).await,
        }
    }

    /// Leaves the open session, or the place in it, for another, and shows where that is.
    /// False when it could not be done; the reason has been said.
    async fn go(&mut self, to: Go) -> bool {
        let new = matches!(to, Go::New);
        match self.backend.go(to).await {
            Ok(moved) => {
                self.moved(moved, new).await;
                true
            }
            Err(why) => {
                self.fail(&why);
                // The session that was open is open again, or nothing is.
                self.sync().await;
                false
            }
        }
    }

    /// After the conversation moved: what was in flight belonged to the session that was left.
    async fn moved(&mut self, moved: Moved, new: bool) {
        self.sync().await;
        self.follow = None;
        self.followed_tools.clear();
        self.fleet.clear();
        self.forget_calls();
        self.busy = false;
        self.tools.clear();
        self.stream = StreamView::default();
        if moved.resumed && !new {
            self.recap();
        } else {
            self.info("started a new session");
        }
        if let Some(notice) = moved.notice {
            self.warn(&notice);
        }
        if let Some(stopped) = moved.interrupted {
            self.warn(&backend::interrupted_notice(&stopped));
        }
    }

    /// Changes the session's mode. Entering bypass is asked about first, with "No" as the
    /// answer a stray key gives.
    async fn set_mode(&mut self, mode: PermissionMode) {
        let current = self.backend.mode();
        if mode != PermissionMode::Bypass || current == PermissionMode::Bypass {
            return self.apply_mode(mode).await;
        }
        self.overlay = Some(Overlay::bypass_warning(current));
    }

    async fn apply_mode(&mut self, mode: PermissionMode) {
        if let Err(why) = self.backend.set_mode(mode).await {
            self.fail(&why);
        }
    }

    /// The primary agent the lead is, and its colour when its definition gives one.
    pub(super) fn agent(&self) -> (String, Option<azazel_tui::Color>) {
        let (agents, current) = self.backend.agents();
        let color = agents
            .into_iter()
            .find(|agent| agent.name == current)
            .and_then(|agent| agent.color)
            .and_then(|color| color.parse().ok());
        (current, color)
    }

    /// Makes the lead the primary agent `name`. `quiet` says so in a passing notice (tab)
    /// rather than in the transcript.
    pub(super) async fn set_agent(&mut self, name: &str, quiet: bool) {
        match self.backend.set_agent(name).await {
            Ok(()) if quiet => self.notice = Some(format!("agent: {name}")),
            Ok(()) => self.info(&format!("agent: {name}")),
            Err(why) => self.fail(&why),
        }
    }

    /// `tab` in an empty input: the next primary agent, going round.
    async fn cycle_agent(&mut self) {
        let (agents, current) = self.backend.agents();
        let next = match agents.iter().position(|agent| agent.name == current) {
            Some(at) => (at + 1) % agents.len(),
            None => 0,
        };
        match agents.get(next).filter(|agent| agent.name != current) {
            Some(agent) => {
                let name = agent.name.clone();
                self.set_agent(&name, true).await;
            }
            None => self.notice = Some("there is no other primary agent to switch to".to_string()),
        }
    }

    async fn set_model(&mut self, specs: &[&str]) {
        let texts: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        match self.backend.set_model(&texts).await {
            Ok(chain) => {
                if let [only] = chain.as_slice() {
                    self.remember_model(&only.to_string());
                }
                let label = chain
                    .iter()
                    .map(ModelSpec::to_string)
                    .collect::<Vec<_>>()
                    .join(", ");
                self.info(&format!("model: {label}"));
            }
            Err(why) => self.fail(&why),
        }
    }

    fn set_theme(&mut self, name: &str) {
        if self.apply_theme(name) {
            self.info(&format!("theme: {}", self.theme_label(name)));
        }
    }

    /// `name`, and for a theme that follows the background which of its palettes this terminal
    /// shows: `catppuccin (light)`.
    pub(super) fn theme_label(&self, name: &str) -> String {
        match Theme::follows_background(name) {
            true => format!("{name} ({})", background_word(self.light_terminal)),
            false => name.to_string(),
        }
    }

    /// Which palette a theme that follows the background takes in this terminal, and why, for
    /// the user.
    pub(super) fn background_note(&self) -> String {
        match self.light_terminal {
            Some(true) => "this terminal's background is light".to_string(),
            Some(false) => "this terminal's background is dark".to_string(),
            None if Theme::follows_background(&self.env.config.tui.theme) => {
                "this terminal did not say what its background is, so the dark palette is shown".to_string()
            }
            None => "the dark palette until azazel starts again: the terminal is asked for its background \
                     at the start"
                .to_string(),
        }
    }

    /// The theme `name`, with the palette this terminal's background asks for.
    fn theme_named(&self, name: &str) -> (Theme, Option<String>) {
        named_theme(&self.env.paths, name, self.light_terminal)
    }

    /// Draws with the theme `name` from now on, without a word about it: the settings screen
    /// tries themes on while one is being chosen. False when there is no such theme, which is
    /// said.
    fn apply_theme(&mut self, name: &str) -> bool {
        let depth = self.theme.depth();
        let (theme, problem) = self.theme_named(name);
        if let Some(problem) = problem {
            self.fail(&problem);
            return false;
        }
        let glyphs = self.theme.glyphs().level();
        self.theme = theme
            .with_depth(depth)
            .with_links(self.caps.links)
            .with_glyphs(glyphs);
        self.theme_name = name.to_string();
        true
    }
}

/// The files under `root`, as paths relative to it with forward slashes; ignored files are
/// left out and the list is cut at [`MAX_COMPLETION_FILES`].
pub(crate) fn project_files(root: &std::path::Path) -> Vec<String> {
    ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .filter_entry(|entry| entry.file_name() != ".git")
        .build()
        .flatten()
        .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
        .filter_map(|entry| {
            let relative = entry.path().strip_prefix(root).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .take(MAX_COMPLETION_FILES)
        .collect()
}

fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default();
    text::truncate(line, 70, "…")
}
