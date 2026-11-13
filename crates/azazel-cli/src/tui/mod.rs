//! The interactive terminal UI: the transcript scrolls into the terminal's own scrollback, and a
//! live region below it holds the streaming reply and running tools on top and, on the last rows
//! of the screen, the input and the status line.

mod backend;
mod bridge;
mod btw;
mod clip;
mod gitcmd;
mod input;
mod overlay;
mod pager;
mod panels;
mod probe;
mod quick;
mod rate;
mod remote;
mod sessions;
mod slash;
mod statusline;
mod team;
mod terminal;
pub(crate) mod view;
#[cfg(feature = "voice")]
mod voice;
#[cfg(not(feature = "voice"))]
#[path = "voice_off.rs"]
mod voice;

use std::collections::VecDeque;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_agent::AgentEvent;
use azazel_ai::{Block, ModelSpec, StopReason, StreamEvent};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use azazel_core::{
    ApprovalDecision, CommandOutcome, Level, Paths, PermissionMode, PlanDecision, SessionStore,
};
use azazel_tools::TodoStatus;
use azazel_tui::components::{
    composer, format_elapsed, format_tokens, markdown, shimmer, spinner, todo, tool_card,
};
use azazel_tui::term::{Caps, FOCUS_OFF, FOCUS_ON};
use azazel_tui::{
    ColorDepth, Composer, Draft, Editor, Floor, Frame, Glyph, GlyphLevel, HistoryItem, Keymap, Line,
    Renderer, Role, SelectItem, SelectList, Spinner, StreamingMarkdown, Theme, TodoState, Vim,
    disable_enhanced_keys, enable_enhanced_keys, text,
};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::mpsc;

use crate::app::{Active, App};
use crate::attention::{Attention, Kind};
pub(crate) use backend::Env;
use backend::{Backend, Go, Incoming, Local, Moved};
use bridge::UiRequest;
pub(crate) use input::Queue;
#[cfg(feature = "test-support")]
pub(crate) use input::queue_from_env;
use overlay::{Action, Outcome, Overlay, ask_items};
#[cfg(test)]
pub(crate) use remote::agent_event;
pub(crate) use remote::{Place, Remote};
pub(crate) use slash::{REWIND_NOTE, is_note};

const DOUBLE_PRESS: Duration = Duration::from_millis(1000);
const MAX_COMPLETION_FILES: usize = 20_000;
const COMPLETION_ROWS: usize = 8;
/// How the note about a `!command` the user ran starts in the agent's context.
pub(crate) const SHELL_NOTE: &str = "I ran `";

/// Puts the terminal into raw mode and restores it when dropped, however the UI ends.
struct TerminalGuard {
    enhanced_keys: bool,
    /// The terminal is ours: raw mode, bracketed paste and enhanced keys are on.
    active: bool,
}

impl TerminalGuard {
    fn enter() -> io::Result<Self> {
        let mut guard = TerminalGuard {
            enhanced_keys: false,
            active: false,
        };
        guard.take()?;
        Ok(guard)
    }

    fn take(&mut self) -> io::Result<()> {
        enable_raw_mode()?;
        self.active = true;
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
        if !std::mem::take(&mut self.active) {
            return;
        }
        let mut out = io::stdout();
        let _ = out.write_all(FOCUS_OFF.as_bytes());
        if std::mem::take(&mut self.enhanced_keys) {
            let _ = disable_enhanced_keys(&mut out);
        }
        let _ = crossterm::execute!(out, DisableBracketedPaste);
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

struct RunningTool {
    call_id: String,
    name: String,
    input: Value,
    tail: Vec<String>,
    started: Instant,
}

struct Completion {
    list: SelectList,
    /// Completing a `/command` rather than an `@file`.
    slash: bool,
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
    #[cfg(feature = "voice")]
    Voice(voice::Done),
    /// Text of a side question's answer, or its end.
    Btw(btw::Update),
    /// A commit message was drafted, or could not be.
    CommitDraft(gitcmd::Draft),
    /// A reply to a request made outside the conversation (`/consult`, `/handoff`, `/name`).
    Side(sessions::SideEvent),
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
    /// Messages waiting for the agent to finish.
    queued: Queue,
    /// The lead just finished a run without being interrupted: the next queued message may go.
    run_ended: bool,
    /// Drafts parked with `ctrl+s`, each with the images that were attached to it.
    stashed: Vec<(Draft, Vec<Block>)>,
    /// Keeps the input and the status line on the last rows of the screen.
    floor: Floor,
    /// Transcript rows and blocks waiting to be written above the live region.
    transcript: Vec<HistoryItem>,
    /// The last thing written to the transcript is a tool card, so the next card joins it
    /// without a blank row between them.
    after_card: bool,
    /// The checked-out branch, for the status line.
    branch: Option<String>,
    stream: StreamView,
    tools: Vec<RunningTool>,
    overlay: Option<Overlay>,
    /// Overlays of the UI's own that wait until nothing else asks for the user.
    deferred: VecDeque<Overlay>,
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
    images: Vec<Block>,
    prompts: Vec<String>,
    notice: Option<String>,
    last_escape: Option<Instant>,
    last_interrupt: Option<Instant>,
    /// The chain to go back to after a command that ran one prompt on another model.
    restore_chain: Option<Vec<ModelSpec>>,
    /// The sub-agent whose work is shown in the transcript next to the lead's.
    follow: Option<String>,
    /// Tool calls in flight of the followed agent.
    followed_tools: Vec<RunningTool>,
    #[cfg(feature = "voice")]
    voice: voice::Voice,
    quit: bool,
    /// The session went away under the UI.
    ended: bool,
}

fn load_theme(env: &Env) -> (Theme, Option<String>) {
    let depth = if std::env::var_os("WT_SESSION").is_some() {
        ColorDepth::TrueColor
    } else {
        ColorDepth::detect(std::env::var("COLORTERM").ok().as_deref())
    };
    let (theme, problem) = named_theme(&env.paths, &env.config.tui.theme);
    let glyphs = GlyphLevel::detect(env.config.tui.glyphs.as_str(), |name| std::env::var(name).ok());
    (theme.with_depth(depth).with_glyphs(glyphs), problem)
}

/// A built-in theme (`dark`, `light`, `catppuccin-mocha`, ...) or `~/.azazel/themes/<name>.toml`.
/// Falls back to `dark` with a message.
fn named_theme(paths: &Paths, name: &str) -> (Theme, Option<String>) {
    match Theme::builtin(name) {
        Some(theme) => (theme, None),
        None => {
            let other = name;
            let file = paths.home.join("themes").join(format!("{other}.toml"));
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
        overrides: crate::app::config_overrides(cli)?,
        env: std::env::vars().collect(),
    };
    let config = azazel_core::Config::load(&paths, &options)?.config;
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
    }
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    // Asked before anything reads keys: the answers come in on the same channel.
    let (caps, images_problem) = terminal::capabilities(env.config.tui.images).await;
    let (theme, theme_problem) = load_theme(&env);
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
    let mut tui = Tui {
        backend,
        session,
        env,
        guard,
        terminal: Some(EventStream::new()),
        internal_tx,
        internal_rx,
        renderer: Renderer::new(io::stdout(), width, height),
        caps,
        attention,
        run_failed: false,
        theme,
        theme_name: tui_config.theme.clone(),
        keymap,
        editor: Editor::new(),
        vim: (editor_config.mode == azazel_core::config::EditorMode::Vim).then(Vim::new),
        fold_paste_lines: editor_config.fold_paste_lines,
        editor_command: editor_config.command,
        favourites: tui_config.favourites.iter().map(ToString::to_string).collect(),
        queued: Queue::default(),
        run_ended: false,
        stashed: Vec::new(),
        floor: Floor::new(height),
        transcript: Vec::new(),
        after_card: false,
        branch,
        stream: StreamView::default(),
        tools: Vec::new(),
        overlay: None,
        deferred: VecDeque::new(),
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
        prompts: Vec::new(),
        notice: None,
        last_escape: None,
        last_interrupt: None,
        restore_chain: None,
        follow: None,
        followed_tools: Vec::new(),
        #[cfg(feature = "voice")]
        voice: voice::Voice::default(),
        quit: false,
        ended: false,
    };
    let opening = tui.attention.open();
    tui.write_out(opening);
    tui.seed_history();
    tui.welcome();
    for problem in [theme_problem, key_problem, images_problem].into_iter().flatten() {
        tui.warn(&problem);
    }
    tui.backend.started().await;
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
    tui.backend.finish().await;
    if let Some(farewell) = tui.backend.farewell().filter(|_| !tui.ended) {
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

    /// Writes a tool card to the transcript: a blank row above, unless it follows another card.
    fn say_card(&mut self, lines: Vec<Line>) {
        if !self.after_card {
            self.transcript.push(String::new().into());
        }
        self.transcript.extend(lines.into_iter().map(HistoryItem::Line));
        self.after_card = true;
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
            self.voice_tick();
            if let Err(e) = self.draw() {
                tracing::error!("terminal write failed: {e}");
                break;
            }
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
            }
        }
    }

    async fn on_incoming(&mut self, incoming: Incoming) {
        match incoming {
            Incoming::Lead(event) => {
                let ended = event.is_terminal();
                self.on_agent(event);
                if ended {
                    // What the run added to the session is there to read from now on.
                    self.sync().await;
                }
                self.send_queued().await;
            }
            Incoming::Request(request) => self.waiting.push_back(request),
            Incoming::Team(event) => self.on_team(event),
            Incoming::Closed(why) => {
                self.fail(&why);
                let _ = self.draw();
                self.ended = true;
                self.quit = true;
            }
        }
    }

    /// Draws a frame. Blank rows between what is in flight and the input keep the live region
    /// tall enough for the input to stay on the last rows of the screen; the transcript grows
    /// into them.
    fn draw(&mut self) -> io::Result<()> {
        self.tell_terminal();
        let history = std::mem::take(&mut self.transcript);
        let (_, rows) = self.renderer.size();
        let (mut viewport, pinned, cursor) = self.viewport();
        let written = history.iter().map(HistoryItem::rows).sum();
        let filler = self.floor.pad(written, viewport.len() + pinned.len(), rows);
        viewport.resize(viewport.len() + filler, String::new());
        let offset = viewport.len() as u16;
        viewport.extend(pinned);
        self.renderer.render(Frame {
            history,
            viewport,
            cursor: cursor.map(|(row, column)| (row + offset, column)),
        })
    }

    /// Session facts on the left, the model and its thinking level against the right edge.
    fn status_line(&self, width: usize) -> Line {
        let status = self.backend.status();
        let facts = self.backend.facts();
        let cwd = statusline::display_path(&self.backend.cwd(), std::env::home_dir().as_deref());
        let model = self.model_label();
        statusline::StatusLine {
            mode: self.backend.mode(),
            coordinator: facts.coordinator,
            branch: self.branch.as_deref(),
            cwd: &cwd,
            context: (status.context_tokens * 100).checked_div(status.context_window),
            rate: self.rate.current(Instant::now()),
            busy: self.busy,
            cost: status.usage.cost_usd,
            warn_cost: self.env.config.limits.warn_cost_usd,
            billing: &facts.billing,
            agents: self.active_agents(),
            goal: facts.goal.as_deref(),
            model: &model,
            tier: facts.tier.map(crate::routing::tier_word),
            thinking: &status.thinking.to_string(),
        }
        .render(width, &self.theme)
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
        let running = self.tools.last().map(|tool| format!("running {}", tool.name));
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
            PermissionMode::Ask => Role::Border,
            PermissionMode::AcceptEdits => Role::ModeEdits,
            PermissionMode::Auto => Role::ModeAuto,
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
        todo::render(&rows, width, &self.theme)
    }

    /// The live region in two parts: what is in flight, which follows the transcript, and what
    /// stays at the bottom of the screen (roster, todo list, the input or an overlay, status).
    /// The cursor is a position in the second part.
    fn viewport(&mut self) -> (Vec<Line>, Vec<Line>, Option<(u16, u16)>) {
        let (columns, rows) = self.renderer.size();
        let (width, height) = (usize::from(columns), usize::from(rows));
        let mut bottom: Vec<Line> = Vec::new();
        let mut cursor;
        if let Some(overlay) = &self.overlay {
            let (lines, at) = overlay.render(width, height.saturating_sub(2).max(4), &self.theme);
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
            bottom.extend(self.waiting_lines(width));
            if !self.images.is_empty() {
                let label = format!("[{} image(s) attached to the next prompt]", self.images.len());
                bottom.push(self.theme.paint(Role::Muted, &label));
            }
            let rule = self.rule_role();
            let activity = self.voice_status().or_else(|| self.rule_status());
            let prompt = self.theme.glyphs().prefix(Glyph::Prompt);
            let composer = Composer {
                prompt: &prompt,
                prompt_role: if rule == Role::Border { Role::Accent } else { rule },
                placeholder: if self.busy {
                    "Steer the agent, alt+enter queues a follow-up"
                } else {
                    "Ask anything, / for commands, @ for files"
                },
                rule,
                status: activity.as_deref(),
                max_rows: composer::max_rows(height),
            };
            let (lines, (row, column)) = composer::render(&self.editor, &composer, width, &self.theme);
            cursor = Some((bottom.len() as u16 + row, column));
            bottom.extend(lines);
        }
        bottom.push(self.status_line(width));

        // Agents at work and the todo list sit right above the input, under one blank row that
        // sets everything pinned to the bottom apart from the transcript. They give way when
        // the screen is too small for them.
        let mut pinned = self.roster_lines(width);
        pinned.extend(self.todo_lines(width));
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

        let mut top: Vec<Line> = Vec::new();
        if self.busy {
            top.extend(self.stream.markdown.tail(width, &self.theme));
            if self.show_thinking && !self.stream.thinking.is_empty() {
                let wrapped = view::thinking(&self.stream.thinking, width, &self.theme);
                let from = wrapped.len().saturating_sub(4);
                top.extend(wrapped.into_iter().skip(from));
            }
            if !self.tools.is_empty() && !self.after_card {
                top.push(String::new());
            }
            for tool in &self.tools {
                let summary = view::tool_summary(&tool.name, &tool.input);
                top.extend(tool_card::running(
                    &tool.name,
                    &summary,
                    &tool.tail,
                    tool.started.elapsed(),
                    width,
                    &self.theme,
                ));
            }
        }
        // The live region never outgrows the screen: what is in flight gives way first.
        if top.len() > room {
            top.drain(..top.len() - room);
        }
        (top, pinned, cursor)
    }

    // ------------------------------------------------------------ agent events

    fn on_agent(&mut self, event: AgentEvent) {
        let width = self.width();
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
            AgentEvent::Retry { attempt, model } => {
                self.stream = StreamView::default();
                self.rate.abandon();
                self.info(&format!("{model} did not answer; retrying (attempt {attempt})"));
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
            } => self.info(&crate::accounts::switch_line(
                &provider, &from, &to, until, &limit,
            )),
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
            AgentEvent::ToolStart { call_id, name, input } => {
                self.tools.push(RunningTool {
                    call_id,
                    name,
                    input,
                    tail: Vec::new(),
                    started: Instant::now(),
                });
            }
            AgentEvent::ToolUpdate { call_id, text } => {
                if let Some(tool) = self.tools.iter_mut().find(|t| t.call_id == call_id) {
                    tool.tail.push(text);
                    let excess = tool.tail.len().saturating_sub(tool_card::MAX_OUTPUT_LINES);
                    tool.tail.drain(..excess);
                }
            }
            AgentEvent::ToolEnd {
                call_id,
                output,
                duration_ms,
            } => {
                if let Some(at) = self.tools.iter().position(|t| t.call_id == call_id) {
                    let tool = self.tools.remove(at);
                    let lines = view::tool_done(
                        &tool.name,
                        &tool.input,
                        &output,
                        Duration::from_millis(duration_ms),
                        self.expand_tools,
                        self.local_cwd().as_deref(),
                        width,
                        &self.theme,
                    );
                    self.say_card(lines);
                    self.show_images(&output);
                }
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
                // With a message queued the work goes on: nothing to call the user for yet.
                if self.run_ended && self.queued.is_empty() {
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
            AgentEvent::TurnStart { .. } | AgentEvent::TurnEnd | AgentEvent::Stream(_) => {}
        }
    }

    /// Sets a reply apart from what precedes it with one blank row, once per message.
    fn open_reply(&mut self) {
        if !std::mem::replace(&mut self.stream.opened, true) {
            self.say(vec![String::new()]);
        }
    }

    fn end_of_run(&mut self) {
        self.busy = false;
        self.tools.clear();
        self.stream = StreamView::default();
        self.rate.abandon();
        // The agent may have switched branches.
        self.branch = self.backend.branch();
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
            Internal::Btw(update) => return self.on_btw(update),
            Internal::CommitDraft(draft) => return self.on_commit_draft(draft),
            Internal::Side(event) => return self.on_side(event).await,
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
            UiRequest::Ask { question, .. } => Some(question.clone()),
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
            UiRequest::Ask {
                question,
                options,
                reply,
                ..
            } if options.is_empty() => {
                self.overlay = Some(Overlay::input(
                    "The agent asks",
                    vec![question],
                    Action::AskFree { reply },
                ));
            }
            UiRequest::Ask {
                question,
                options,
                multi,
                reply,
            } => {
                let picked = vec![false; options.len()];
                let items = ask_items(&options, &picked, multi);
                let action = Action::Ask {
                    reply,
                    options,
                    multi,
                    picked,
                };
                self.overlay = Some(Overlay::select(
                    "The agent asks",
                    vec![question],
                    items,
                    false,
                    action,
                ));
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
                let items = options.iter().map(|o| SelectItem::new(o.as_str(), "")).collect();
                self.overlay = Some(Overlay::select(
                    "Choose",
                    vec![prompt],
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
                self.overlay = Some(match secret {
                    true => Overlay::secret("Input", vec![prompt], action),
                    false => Overlay::input("Input", vec![prompt], action),
                });
            }
            UiRequest::Approval { request, reply } => {
                let mut body: Vec<String> = if request.subjects.is_empty() {
                    serde_json::to_string_pretty(&request.input)
                        .unwrap_or_default()
                        .lines()
                        .map(str::to_string)
                        .collect()
                } else {
                    request.subjects.clone()
                };
                if let Some(note) = &request.note {
                    body.push(String::new());
                    body.push(note.clone());
                }
                let mut items = vec![SelectItem::new("Allow once", "")];
                if let Some(rule) = &request.rule {
                    items.push(SelectItem::new(
                        format!("Always allow {rule}"),
                        "saved for this project",
                    ));
                }
                items.push(SelectItem::new("Deny", ""));
                items.push(SelectItem::new("Deny and say what to do instead", ""));
                let title = format!("{} wants to use {}", request.agent.id, request.tool);
                // A stray Enter must not approve a command that destroys work: start on "Deny".
                let deny = if request.rule.is_some() { 2 } else { 1 };
                let start = if request.destructive { deny } else { 0 };
                let action = Action::Approval {
                    reply,
                    has_rule: request.rule.is_some(),
                };
                self.overlay = Some(Overlay::select(title, body, items, false, action).starting_at(start));
            }
            UiRequest::Plan { path, plan, reply } => {
                // The plan itself goes into the transcript, where it can be scrolled and re-read.
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
                let body = vec!["The plan is shown above.".to_string()];
                self.overlay = Some(Overlay::select(
                    "Plan review",
                    body,
                    items,
                    false,
                    Action::Plan { reply },
                ));
            }
        }
    }

    // ------------------------------------------------------------ terminal input

    async fn on_terminal(&mut self, event: Event) {
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
            Event::Resize(width, height) => {
                self.renderer.resize(width, height);
                self.floor.resize(height);
            }
            Event::FocusGained => self.attention.focus(true),
            Event::FocusLost => self.attention.focus(false),
            _ => {}
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        self.notice = None;
        self.run_failed = false;
        if self.voice_key(&key) {
            return;
        }
        if let Some(overlay) = &mut self.overlay {
            // Feedback for the agent can be written in the user's editor too.
            if panels::is_text_input(overlay) && self.keymap.action_for(&key) == Some("external_editor") {
                return self.external_editor().await;
            }
            let outcome = overlay.on_key(&key);
            if let Outcome::Chosen(index) = outcome
                && overlay.toggle(index)
            {
                return;
            }
            if let Outcome::Hotkey { index, key } = outcome {
                return self.overlay_hotkey(index, key).await;
            }
            if !matches!(outcome, Outcome::Pending)
                && let Some(overlay) = self.overlay.take()
            {
                self.finish_overlay(overlay, outcome).await;
            }
            return;
        }
        if self.btw.is_some() {
            return self.btw_key(key).await;
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
                    // Enter on a half-typed command completes it; a complete one runs.
                    let typed = self.editor.text().trim().to_string();
                    let chosen = completion.list.selected_item().map(|item| item.label.clone());
                    if chosen.is_some_and(|label| label != typed) {
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
        let action = self.keymap.action_for(&key).map(str::to_string);
        match action.as_deref() {
            Some("submit") => self.submit(false).await,
            Some("follow_up") => self.submit(true).await,
            Some("newline") => self.editor.insert_newline(),
            Some("abort") => self.on_escape().await,
            Some("clear_or_exit") => self.on_interrupt(),
            Some("exit") if self.editor.is_empty() => self.quit = true,
            Some("cycle_mode") => {
                let modes = PermissionMode::ALL;
                let current = modes.iter().position(|m| *m == self.backend.mode()).unwrap_or(0);
                self.set_mode(modes[(current + 1) % modes.len()]).await;
            }
            Some("toggle_tool_output") => {
                self.expand_tools = !self.expand_tools;
                self.notice = Some(format!(
                    "tool output: {}",
                    if self.expand_tools {
                        "expanded"
                    } else {
                        "collapsed"
                    }
                ));
            }
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
            Some("voice") => self.voice_pressed(&key).await,
            _ => {
                if self.editor.handle_key(&key) {
                    self.refresh_completion();
                }
            }
        }
    }

    async fn on_escape(&mut self) {
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
        if self.editor.is_empty() && self.images.is_empty() {
            return;
        }
        // What is echoed keeps a folded paste as its token; what is sent has the text.
        let shown = self.editor.text().to_string();
        let entered = self.editor.take();
        self.completion = None;
        let trimmed = entered.trim();
        if let Some(command) = trimmed.strip_prefix('/') {
            let command = command.to_string();
            return self.slash(&command).await;
        }
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
        // Nothing can answer yet: keep what was typed and set a model up first.
        if !self.busy && !self.backend.has_model() {
            self.editor.set_text(&entered);
            self.warn("no model is set up yet: sign in or add an API key, then choose a default model");
            return self.other_command("settings", "");
        }
        if self.busy && follow_up {
            // Held here, not by the agent, so that it is sent like any typed prompt.
            return self.queue_message(entered);
        }
        self.prompts.push(entered.clone());
        self.remember(&entered);
        let lines = view::user_prompt(&shown, self.width(), &self.theme);
        self.say(lines);
        if self.busy {
            let images = std::mem::take(&mut self.images);
            self.backend.steer(entered, images);
            return;
        }
        self.send_prompt(entered).await;
    }

    async fn send_prompt(&mut self, prompt: String) {
        let images = std::mem::take(&mut self.images);
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

    fn paste_image(&mut self) {
        let image = arboard::Clipboard::new().and_then(|mut clipboard| clipboard.get_image());
        let Ok(image) = image else {
            self.notice = Some("the clipboard holds no image".to_string());
            return;
        };
        let (width, height) = (image.width as u32, image.height as u32);
        let encoded =
            image::RgbaImage::from_raw(width, height, image.bytes.into_owned()).and_then(|pixels| {
                let mut png = Vec::new();
                pixels
                    .write_to(&mut io::Cursor::new(&mut png), image::ImageFormat::Png)
                    .ok()?;
                image_block(&png, MAX_IMAGE_SIDE).ok()
            });
        match encoded {
            Some(block) => {
                self.images.push(block);
                self.notice = Some(format!("attached a {width}×{height} image"));
            }
            None => self.notice = Some("could not read the clipboard image".to_string()),
        }
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
            self.completion = (!list.is_empty()).then_some(Completion { list, slash: true });
            return;
        }
        let token = self.editor.token_before_cursor().to_string();
        let Some(partial) = token.strip_prefix('@') else {
            self.completion = None;
            return;
        };
        if !matches!(&self.completion, Some(c) if !c.slash) {
            let items = self
                .project_files()
                .iter()
                .map(|file| SelectItem::new(file.as_str(), ""))
                .collect();
            self.completion = Some(Completion {
                list: SelectList::new(items),
                slash: false,
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
        if completion.slash {
            self.editor.set_text(&format!("{} ", item.label));
        } else {
            self.editor
                .replace_token_before_cursor(&format!("@{} ", item.label));
        }
    }

    // ------------------------------------------------------------ overlays

    async fn finish_overlay(&mut self, overlay: Overlay, outcome: Outcome) {
        let Overlay { action, .. } = overlay;
        let (choice, typed) = match outcome {
            Outcome::Pending => return,
            Outcome::Cancelled => return action.cancel(),
            Outcome::Chosen(index) => (index, String::new()),
            Outcome::Text(typed) => (0, typed),
            // Handled where the key was pressed; the overlay stays open.
            Outcome::Hotkey { .. } => return,
        };
        match action {
            Action::AskFree { reply } => drop(reply.send(Ok(vec![typed]))),
            Action::Ask {
                reply,
                options,
                multi,
                picked,
            } => {
                let chosen = if multi {
                    options
                        .iter()
                        .zip(&picked)
                        .filter(|(_, on)| **on)
                        .map(|(o, _)| o.clone())
                        .collect()
                } else {
                    options.get(choice).cloned().into_iter().collect()
                };
                drop(reply.send(Ok(chosen)));
            }
            Action::Confirm { reply } => drop(reply.send(Ok(choice == 0))),
            Action::UiSelect { reply } => drop(reply.send(Ok(choice))),
            Action::UiInput { reply } => drop(reply.send(Ok(typed))),
            Action::Approval { reply, has_rule } => {
                // Rows: allow once, [always], deny, deny with feedback.
                let row = if has_rule {
                    choice
                } else if choice == 0 {
                    0
                } else {
                    choice + 1
                };
                match row {
                    0 => drop(reply.send(Some(ApprovalDecision::AllowOnce))),
                    1 => drop(reply.send(Some(ApprovalDecision::Always))),
                    2 => drop(reply.send(Some(ApprovalDecision::Deny { feedback: None }))),
                    _ => {
                        let body = vec!["Tell the agent what to do instead.".to_string()];
                        self.overlay = Some(Overlay::input("Deny", body, Action::DenyFeedback { reply }));
                    }
                }
            }
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
            Action::Thinking => {
                if let Some(level) = azazel_ai::ThinkingLevel::ALL.get(choice) {
                    self.set_thinking(*level).await;
                }
            }
            Action::Mode => {
                if let Some(mode) = PermissionMode::ALL.get(choice) {
                    self.set_mode(*mode).await;
                }
            }
            Action::Theme(names) => {
                if let Some(name) = names.get(choice) {
                    self.set_theme(name);
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
                    self.follow_agent(id);
                }
            }
            Action::AgentMessage(id) => self.message_agent(&id, &typed).await,
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
            action @ (Action::CopyCode(_)
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
    }

    async fn set_mode(&mut self, mode: PermissionMode) {
        if let Err(why) = self.backend.set_mode(mode).await {
            self.fail(&why);
        }
    }

    async fn set_model(&mut self, specs: &[&str]) {
        let texts: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        match self.backend.set_model(&texts).await {
            Ok(chain) => {
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
        let depth = self.theme.depth();
        let (theme, problem) = named_theme(&self.env.paths, name);
        match problem {
            Some(problem) => self.fail(&problem),
            None => {
                let glyphs = self.theme.glyphs().level();
                self.theme = theme
                    .with_depth(depth)
                    .with_links(self.caps.links)
                    .with_glyphs(glyphs);
                self.theme_name = name.to_string();
                self.info(&format!("theme: {name}"));
            }
        }
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
