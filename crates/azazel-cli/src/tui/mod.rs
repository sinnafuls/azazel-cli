//! The interactive terminal UI: the transcript scrolls into the terminal's own scrollback, and a
//! live region below it holds the streaming reply and running tools on top and, on the last rows
//! of the screen, the input and the status line.

mod bridge;
mod btw;
mod clip;
mod gitcmd;
mod input;
mod overlay;
mod pager;
mod panels;
mod quick;
mod slash;
mod team;
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

use azazel_agent::{AgentCmd, AgentEvent, UserInput};
use azazel_ai::{Block, ModelSpec, StopReason, StreamEvent};
use azazel_core::media::{MAX_IMAGE_SIDE, image_block};
use azazel_core::process::{RunOptions, run as run_command};
use azazel_core::{
    ApprovalDecision, CommandOutcome, EntryKind, Level, PermissionMode, PlanDecision, StartSource, Ui,
};
use azazel_team::{Goals, Team, TeamEvent};
use azazel_tools::{TodoStatus, TodoStore};
use azazel_tui::components::{
    composer, format_elapsed, format_tokens, markdown, spinner, status, todo, tool_card,
};
use azazel_tui::{
    ColorDepth, Composer, Draft, Editor, Floor, Frame, Keymap, Line, Renderer, Role, SelectItem, SelectList,
    Spinner, StreamingMarkdown, Theme, TodoState, Vim, disable_enhanced_keys, enable_enhanced_keys, text,
};
use crossterm::event::{
    DisableBracketedPaste, EnableBracketedPaste, Event, EventStream, KeyCode, KeyEvent, KeyEventKind,
};
use crossterm::terminal::{disable_raw_mode, enable_raw_mode};
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

use crate::app::{Active, App, Open};
use bridge::{TuiBridge, UiRequest};
pub(crate) use input::Queue;
#[cfg(feature = "test-support")]
pub(crate) use input::queue_from_env;
use overlay::{Action, Outcome, Overlay, ask_items};

const DOUBLE_PRESS: Duration = Duration::from_millis(1000);
const MAX_COMPLETION_FILES: usize = 20_000;
const COMPLETION_ROWS: usize = 8;
/// How the note about a `!command` the user ran starts in the agent's context.
const SHELL_NOTE: &str = "I ran `";

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
        Ok(())
    }

    /// Hands the terminal back as a shell would have it (cooked mode, no bracketed paste, no
    /// enhanced keys), for a program that takes it over for a while or for exit.
    fn suspend(&mut self) {
        if !std::mem::take(&mut self.active) {
            return;
        }
        let mut out = io::stdout();
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
    Shell {
        command: String,
        output: String,
        status: String,
        /// Whether the agent is told what was run (`!command`) or not (`!!command`).
        share: bool,
    },
    /// What a registered slash command came back with.
    Command(CommandOutcome),
    /// A setting the settings menu saved that applies to the running UI at once.
    Setting { path: String, value: toml::Value },
    #[cfg(feature = "voice")]
    Voice(voice::Done),
    /// Text of a side question's answer, or its end.
    Btw(btw::Update),
    /// A commit message was drafted, or could not be.
    CommitDraft(gitcmd::Draft),
}

pub struct Tui {
    app: Arc<App>,
    active: Active,
    guard: TerminalGuard,
    /// Keys, pastes and resizes; `None` while another program has the terminal.
    terminal: Option<EventStream>,
    events: mpsc::UnboundedReceiver<AgentEvent>,
    requests: mpsc::UnboundedReceiver<UiRequest>,
    internal_tx: mpsc::UnboundedSender<Internal>,
    internal_rx: mpsc::UnboundedReceiver<Internal>,
    renderer: Renderer<io::Stdout>,
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
    /// Transcript lines waiting to be written above the live region.
    transcript: Vec<Line>,
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
    completion: Option<Completion>,
    files: Option<Vec<String>>,
    busy: bool,
    busy_since: Instant,
    show_thinking: bool,
    expand_tools: bool,
    images: Vec<Block>,
    prompts: Vec<String>,
    notice: Option<String>,
    last_escape: Option<Instant>,
    last_interrupt: Option<Instant>,
    /// The chain to go back to after a command that ran one prompt on another model.
    restore_chain: Option<Vec<ModelSpec>>,
    team: Option<Arc<Team>>,
    team_events: Option<broadcast::Receiver<TeamEvent>>,
    /// The sub-agent whose work is shown in the transcript next to the lead's.
    follow: Option<String>,
    /// Tool calls in flight of the followed agent.
    followed_tools: Vec<RunningTool>,
    #[cfg(feature = "voice")]
    voice: voice::Voice,
    quit: bool,
}

fn load_theme(app: &App) -> (Theme, Option<String>) {
    let depth = if std::env::var_os("WT_SESSION").is_some() {
        ColorDepth::TrueColor
    } else {
        ColorDepth::detect(std::env::var("COLORTERM").ok().as_deref())
    };
    let (theme, problem) = named_theme(app, &app.config.tui.theme);
    (theme.with_depth(depth), problem)
}

/// `dark`, `light`, or `~/.azazel/themes/<name>.toml`. Falls back to `dark` with a message.
fn named_theme(app: &App, name: &str) -> (Theme, Option<String>) {
    match name {
        "dark" => (Theme::dark(), None),
        "light" => (Theme::light(), None),
        other => {
            let file = app.paths.home.join("themes").join(format!("{other}.toml"));
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

pub async fn run(app: Arc<App>, active: Active, first_prompt: Option<String>) -> u8 {
    let (request_tx, requests) = mpsc::unbounded_channel();
    app.registry
        .services
        .insert(Arc::new(Ui(Arc::new(TuiBridge::new(request_tx)))));
    let guard = match TerminalGuard::enter() {
        Ok(guard) => guard,
        Err(e) => {
            eprintln!("azazel: cannot take over the terminal: {e}");
            app.finish(&active).await;
            return 1;
        }
    };
    let (internal_tx, internal_rx) = mpsc::unbounded_channel();
    // The settings menu runs as a command, outside the UI loop; this is how it reaches it.
    let settings_tx = internal_tx.clone();
    app.registry
        .services
        .insert(Arc::new(crate::settings::LiveSettings(Arc::new(
            move |path: &str, value: &toml::Value| {
                let _ = settings_tx.send(Internal::Setting {
                    path: path.to_string(),
                    value: value.clone(),
                });
            },
        ))));
    let (width, height) = crossterm::terminal::size().unwrap_or((80, 24));
    let (theme, theme_problem) = load_theme(&app);
    let mut keymap = Keymap::defaults();
    let mut key_problem = None;
    if let Ok(source) = std::fs::read_to_string(app.paths.keybindings_file())
        && let Err(e) = keymap.merge_toml(&source)
    {
        key_problem = Some(format!("{}: {e}", app.paths.keybindings_file().display()));
    }
    let branch = azazel_core::prompt::git_branch(&app.paths.project_root);
    let events = active.lead.subscribe();
    let resumed = active.resumed;
    let team = app.registry.services.get::<Team>();
    let team_events = team.as_ref().map(|team| team.subscribe());
    let editor_config = app.config.editor.clone();
    let tui_config = app.config.tui.clone();
    let mut tui = Tui {
        app,
        active,
        guard,
        terminal: Some(EventStream::new()),
        events,
        requests,
        internal_tx,
        internal_rx,
        renderer: Renderer::new(io::stdout(), width, height),
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
        completion: None,
        files: None,
        busy: false,
        busy_since: Instant::now(),
        show_thinking: tui_config.show_thinking,
        expand_tools: tui_config.expand_tools,
        images: Vec::new(),
        prompts: Vec::new(),
        notice: None,
        last_escape: None,
        last_interrupt: None,
        restore_chain: None,
        team,
        team_events,
        follow: None,
        followed_tools: Vec::new(),
        #[cfg(feature = "voice")]
        voice: voice::Voice::default(),
        quit: false,
    };
    tui.welcome(resumed);
    for problem in [theme_problem, key_problem].into_iter().flatten() {
        tui.warn(&problem);
    }
    tui.app
        .start(if resumed {
            StartSource::Resume
        } else {
            StartSource::Startup
        })
        .await;
    if let Some(notice) = crate::changelog::update_notice(&tui.app.paths) {
        tui.info(&notice);
    }
    if crate::update::checks_enabled(tui.app.config.update.check) {
        if let Some(notice) = crate::update::notice(&tui.app.paths) {
            tui.info(&notice);
        }
        crate::update::check_in_background(&tui.app.paths);
    }
    if let Some(prompt) = first_prompt {
        // Sent as if typed: without a model it is kept in the editor while settings open.
        tui.editor.set_text(&prompt);
        tui.submit(false).await;
    } else if !tui.app.has_model(&tui.active.lead.status().chain) {
        // A first run has nothing to answer with: go straight to signing in or adding a key.
        tui.info(
            "Welcome! azazel needs a model before it can work. Sign in with a Claude or ChatGPT \
             subscription, or add an API key, then pick the default model. You can come back \
             any time with /settings.",
        );
        tui.other_command("settings", "").await;
    }
    tui.main_loop().await;
    let _ = tui.renderer.suspend();
    tui.guard.suspend();
    tui.app.finish(&tui.active).await;
    0
}

impl Tui {
    fn width(&self) -> usize {
        usize::from(self.renderer.size().0)
    }

    fn say(&mut self, lines: Vec<Line>) {
        if !lines.is_empty() {
            self.after_card = false;
            self.transcript.extend(lines);
        }
    }

    /// Writes a tool card to the transcript: a blank row above, unless it follows another card.
    fn say_card(&mut self, lines: Vec<Line>) {
        if !self.after_card {
            self.transcript.push(String::new());
        }
        self.transcript.extend(lines);
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

    fn welcome(&mut self, resumed: bool) {
        let width = self.width();
        let art = view::banner(width, &self.theme);
        self.say(art);
        let model = self.model_label();
        let model = if model.is_empty() {
            "no model yet".to_string()
        } else {
            model
        };
        let heading = format!(
            "azazel {} · {model} · {}",
            env!("CARGO_PKG_VERSION"),
            self.app.paths.cwd.display()
        );
        let lines = view::styled(&heading, Role::Accent, width, &self.theme);
        self.say(lines);
        self.info(
            "enter sends · shift+enter adds a line · / commands · @ files · ! shell · /settings accounts and models · ctrl+c twice quits",
        );
        if !self.app.ignored_settings.is_empty() {
            let skipped = self.app.ignored_settings.join(", ");
            self.warn(&format!("this project is not trusted; skipped: {skipped}"));
        }
        if resumed {
            self.recap();
        }
    }

    /// Shows where a reopened session left off.
    fn recap(&mut self) {
        let context = self.active.session.build_context(None);
        let name = self
            .active
            .session
            .name()
            .unwrap_or_else(|| self.active.session.id());
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
        let status = self.active.lead.status();
        match status.model {
            Some(model) => model.to_string(),
            None => self
                .app
                .router
                .resolve(&status.chain)
                .first()
                .map(|m| m.to_string())
                .unwrap_or_default(),
        }
    }

    async fn main_loop(&mut self) {
        let mut ticker = tokio::time::interval(spinner::INTERVAL);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
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
                event = self.events.recv() => match event {
                    Some(event) => {
                        self.on_agent(event);
                        self.send_queued().await;
                    }
                    None => break,
                },
                request = self.requests.recv() => {
                    if let Some(request) = request {
                        self.waiting.push_back(request);
                    }
                }
                done = self.internal_rx.recv() => {
                    if let Some(done) = done {
                        self.on_internal(done).await;
                    }
                }
                event = team::next_event(&mut self.team_events) => self.on_team(event),
                // Only the spinner and the clocks of working agents animate; nothing is drawn
                // while everything is idle. An open overlay is checked for an asker that left.
                _ = ticker.tick(),
                    if self.busy
                        || self.agents_active()
                        || self.overlay.is_some()
                        || self.btw.is_some()
                        || self.voice_animates() => {}
            }
        }
    }

    /// Draws a frame. Blank rows between what is in flight and the input keep the live region
    /// tall enough for the input to stay on the last rows of the screen; the transcript grows
    /// into them.
    fn draw(&mut self) -> io::Result<()> {
        let history = std::mem::take(&mut self.transcript);
        let (_, rows) = self.renderer.size();
        let (mut viewport, pinned, cursor) = self.viewport();
        let filler = self.floor.pad(history.len(), viewport.len() + pinned.len(), rows);
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
        let theme = &self.theme;
        let muted = |text: &str| theme.paint(Role::Muted, text);
        let status = self.active.lead.status();
        let mode = self.app.permissions.mode();
        let mut left = vec![match mode {
            PermissionMode::Plan => theme.paint(Role::ModePlan, &format!("⏸ {mode}")),
            PermissionMode::Ask => muted(&mode.to_string()),
            PermissionMode::AcceptEdits => theme.paint(Role::ModeEdits, &format!("⏵⏵ {mode}")),
            PermissionMode::Auto => theme.paint(Role::ModeAuto, &format!("⏵⏵ {mode}")),
        }];
        if let Some(branch) = &self.branch {
            left.push(muted(&format!("⑂ {branch}")));
        }
        let cwd = &self.app.paths.cwd;
        let shown = match std::env::home_dir()
            .and_then(|home| cwd.strip_prefix(home).ok().map(|rest| rest.to_path_buf()))
        {
            Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
            Some(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
            None => cwd.display().to_string(),
        };
        left.push(muted(&shown));
        if let Some(percent) = (status.context_tokens * 100).checked_div(status.context_window) {
            let role = status::context_role(percent).unwrap_or(Role::Muted);
            left.push(theme.paint(role, &format!("◫ {percent}%")));
        }
        if let Some(cost) = status.usage.cost_usd {
            left.push(muted(&format!("${cost:.2}")));
        }
        match self.active_agents() {
            0 => {}
            1 => left.push(theme.paint(Role::Accent, "⇶ 1 agent")),
            n => left.push(theme.paint(Role::Accent, &format!("⇶ {n} agents"))),
        }
        let goal = self
            .app
            .registry
            .services
            .get::<Goals>()
            .and_then(|goals| goals.current());
        if let Some(goal) = goal.filter(|goal| goal.status.is_open()) {
            left.push(theme.paint(
                Role::Accent,
                &format!("⚑ goal {}, attempt {}", goal.status.label(), goal.iteration + 1),
            ));
        }

        let model = self.model_label();
        let model = if model.is_empty() {
            "no model".to_string()
        } else {
            model
        };
        let tier = match self.app.router.tier() {
            Some(tier) => format!("{} · ", crate::routing::tier_word(tier)),
            None => String::new(),
        };
        let right = format!(
            "{} {}{}",
            theme.paint(Role::Accent, "⬢"),
            muted(&format!("{model} · {tier}")),
            theme.paint(Role::Thinking, &status.thinking.to_string()),
        );
        status::render(&left, &right, width, theme)
    }

    /// What the lead is doing, for the rule above the input; nothing while it is idle.
    fn activity(&self) -> Option<String> {
        if !self.busy {
            return None;
        }
        let what = match self.tools.last() {
            Some(tool) => format!("running {}", tool.name),
            None if !self.stream.thinking.is_empty() && !self.stream.saw_text => "thinking".to_string(),
            None => "working".to_string(),
        };
        let elapsed = format_elapsed(self.busy_since.elapsed());
        Some(format!(
            "{} {}",
            self.theme.paint(Role::Accent, Spinner::now()),
            self.theme
                .paint(Role::Muted, &format!("{what} · {elapsed} · esc to interrupt")),
        ))
    }

    /// The colour of the rules around the input: it tells a `!` shell command and the
    /// permission modes apart.
    fn rule_role(&self) -> Role {
        if self.editor.text().starts_with('!') {
            return Role::Bash;
        }
        match self.app.permissions.mode() {
            PermissionMode::Plan => Role::ModePlan,
            PermissionMode::Ask => Role::Border,
            PermissionMode::AcceptEdits => Role::ModeEdits,
            PermissionMode::Auto => Role::ModeAuto,
        }
    }

    fn todo_lines(&self, width: usize) -> Vec<Line> {
        let Some(store) = self.app.registry.services.get::<TodoStore>() else {
            return Vec::new();
        };
        let items = store.get(&self.active.lead.id);
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
            let composer = Composer {
                prompt: "❯ ",
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
            }
            AgentEvent::MessageStart => self.stream = StreamView::default(),
            AgentEvent::Stream(StreamEvent::TextDelta { delta, .. }) => {
                self.open_reply();
                self.stream.saw_text = true;
                self.stream.markdown.push(&delta);
                let finished = self.stream.markdown.take_finished(width, &self.theme);
                self.say(finished);
            }
            AgentEvent::Stream(StreamEvent::ThinkingDelta { delta, .. }) => {
                self.stream.thinking.push_str(&delta)
            }
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
                self.voice_reply(&message);
                if !self.stream.saw_text && !message.text().trim().is_empty() {
                    self.open_reply();
                    self.stream.markdown.push(&message.text());
                }
                let rest = self.stream.markdown.finish(width, &self.theme);
                self.say(rest);
                self.stream = StreamView::default();
            }
            AgentEvent::Retry { attempt, model } => {
                self.stream = StreamView::default();
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
                let rest = self.stream.markdown.finish(width, &self.theme);
                self.say(rest);
                self.stream = StreamView::default();
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
                        width,
                        &self.theme,
                    );
                    self.say_card(lines);
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
                self.end_of_run();
                // An interrupted run keeps what is queued for the user to send or take back.
                self.run_ended = stop != StopReason::Aborted;
                self.voice_run_ended(&stop);
                match stop {
                    StopReason::Aborted => self.info("interrupted"),
                    StopReason::Length => self.warn("the reply was cut off at the model's output limit"),
                    _ => {}
                }
            }
            AgentEvent::Failed { error } => {
                self.end_of_run();
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
        // The agent may have switched branches.
        self.branch = azazel_core::prompt::git_branch(&self.app.paths.project_root);
        if let Some(chain) = self.restore_chain.take() {
            self.active.lead.send(AgentCmd::SetModel(chain));
        }
    }

    async fn on_internal(&mut self, done: Internal) {
        let (command, output, status, share) = match done {
            Internal::Shell {
                command,
                output,
                status,
                share,
            } => (command, output, status, share),
            Internal::Setting { path, value } => return self.apply_setting(&path, &value),
            Internal::Btw(update) => return self.on_btw(update),
            Internal::CommitDraft(draft) => return self.on_commit_draft(draft),
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
        self.branch = azazel_core::prompt::git_branch(&self.app.paths.project_root);
        let shown: Vec<&str> = output.lines().collect();
        let from = shown.len().saturating_sub(40);
        let mut display = shown[from..].join("\n");
        if from > 0 {
            display = format!("… {from} earlier lines\n{display}");
        }
        self.info(&format!("{display}\n{status}"));
        if !share {
            return;
        }
        // The agent sees what the user ran, without being asked to respond.
        let note = format!("{SHELL_NOTE}{command}` myself:\n{output}\n{status}");
        self.active.lead.send(AgentCmd::Append(UserInput::text(note)));
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
        let bus = self.app.registry.bus.clone();
        tokio::spawn(async move {
            bus.emit(&mut azazel_core::Event::Notification { level, text })
                .await;
        });
    }

    fn show_request(&mut self, request: UiRequest) {
        let pick = |labels: &[&str]| labels.iter().map(|l| SelectItem::new(*l, "")).collect::<Vec<_>>();
        match &request {
            UiRequest::Notify { level, text } => self.announce(*level, text.clone()),
            UiRequest::Ask { question, .. } => self.announce(Level::Info, question.clone()),
            UiRequest::Confirm { prompt, .. }
            | UiRequest::Select { prompt, .. }
            | UiRequest::Input { prompt, .. } => self.announce(Level::Info, prompt.clone()),
            UiRequest::Approval { request, .. } => self.announce(
                Level::Info,
                format!("{} needs permission to use {}", request.agent.id, request.tool),
            ),
            UiRequest::Plan { .. } => self.announce(Level::Info, "a plan is ready for review".to_string()),
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
            _ => {}
        }
    }

    async fn on_key(&mut self, key: KeyEvent) {
        self.notice = None;
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
                let current = modes
                    .iter()
                    .position(|m| *m == self.app.permissions.mode())
                    .unwrap_or(0);
                self.app.permissions.set_mode(modes[(current + 1) % modes.len()]);
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
            Some("model_picker") => self.open_model_picker(),
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
            Some("cycle_thinking") => self.cycle_thinking(),
            Some("cycle_model") => self.cycle_model(true),
            Some("cycle_model_back") => self.cycle_model(false),
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
            self.active.lead.abort();
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
            return self.user_shell(command.trim().to_string(), share);
        }
        // `# note`: remembered for later sessions, and not a prompt.
        if let Some(note) = quick::memory_note(&entered) {
            let lines = view::user_prompt(trimmed, self.width(), &self.theme);
            self.say(lines);
            let (services, note) = (self.app.registry.services.clone(), note.to_string());
            let done = self.internal_tx.clone();
            tokio::spawn(async move {
                let _ = done.send(Internal::Command(quick::remember(&services, &note).await));
            });
            return;
        }
        // Nothing can answer yet: keep what was typed and set a model up first.
        if !self.busy && !self.app.has_model(&self.active.lead.status().chain) {
            self.editor.set_text(&entered);
            self.warn("no model is set up yet: sign in or add an API key, then choose a default model");
            return self.other_command("settings", "").await;
        }
        if self.busy && follow_up {
            // Held here, not by the agent, so that it is sent like any typed prompt.
            return self.queue_message(entered);
        }
        self.prompts.push(entered.clone());
        let lines = view::user_prompt(&shown, self.width(), &self.theme);
        self.say(lines);
        if self.busy {
            let mut content = vec![Block::text(entered)];
            content.append(&mut self.images);
            self.active.lead.steer(UserInput { content });
            return;
        }
        self.send_prompt(entered).await;
    }

    async fn send_prompt(&mut self, prompt: String) {
        let images = std::mem::take(&mut self.images);
        if let Err(reason) = self.app.submit(&self.active, prompt, images).await {
            self.fail(&format!("prompt rejected: {reason}"));
            if let Some(chain) = self.restore_chain.take() {
                self.active.lead.send(AgentCmd::SetModel(chain));
            }
        }
    }

    /// `!command`: runs in the user's shell, outside the permission engine and the push guard.
    /// With `share` the agent is told what was run and what it printed.
    fn user_shell(&mut self, command: String, share: bool) {
        if command.is_empty() {
            return;
        }
        let echo = if share { "!" } else { "!!" };
        let lines = view::user_prompt(&format!("{echo}{command}"), self.width(), &self.theme);
        self.say(lines);
        self.notice = Some(format!("running `{command}`…"));
        let shell = self.app.shell.clone();
        let cwd = self.app.paths.cwd.clone();
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let mut options = RunOptions::new(cwd);
            options.keep = (200, 800);
            let (output, status) = match run_command(&shell, &command, options).await {
                Ok(result) => {
                    let status = match result.code {
                        Some(code) => format!("exit code: {code}"),
                        None => "killed".to_string(),
                    };
                    (result.output.render(), status)
                }
                Err(e) => (String::new(), format!("could not start: {e}")),
            };
            let _ = done.send(Internal::Shell {
                command,
                output,
                status,
                share,
            });
        });
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

    fn project_files(&mut self) -> &[String] {
        let root = self.app.paths.cwd.clone();
        self.files.get_or_insert_with(|| {
            ignore::WalkBuilder::new(&root)
                .hidden(false)
                .require_git(false)
                .filter_entry(|entry| entry.file_name() != ".git")
                .build()
                .flatten()
                .filter(|entry| entry.file_type().is_some_and(|t| t.is_file()))
                .filter_map(|entry| {
                    let relative = entry.path().strip_prefix(&root).ok()?;
                    Some(relative.to_string_lossy().replace('\\', "/"))
                })
                .take(MAX_COMPLETION_FILES)
                .collect()
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
                    self.set_model(&[spec.as_str()]);
                }
            }
            Action::Thinking => {
                if let Some(level) = azazel_ai::ThinkingLevel::ALL.get(choice) {
                    self.active.lead.send(AgentCmd::SetThinking(*level));
                    self.info(&format!("thinking: {level}"));
                }
            }
            Action::Mode => {
                if let Some(mode) = PermissionMode::ALL.get(choice) {
                    self.app.permissions.set_mode(*mode);
                }
            }
            Action::Theme(names) => {
                if let Some(name) = names.get(choice) {
                    self.set_theme(name);
                }
            }
            Action::Resume(paths) => {
                if let Some(path) = paths.get(choice) {
                    self.swap_session(Open::File(path.clone()), StartSource::Resume)
                        .await;
                }
            }
            Action::Tree(rows) => {
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
            Action::AgentMessage(id) => self.message_agent(&id, &typed),
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
                    self.other_command("mcp", &format!("{action} {server}")).await;
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
        }
    }

    /// Replaces the open session with another one, keeping the model selection.
    async fn swap_session(&mut self, how: Open, source: StartSource) {
        let status = self.active.lead.status();
        self.app.close(&self.active).await;
        match self.app.open(how, status.chain.clone(), status.thinking).await {
            Ok(active) => {
                self.events = active.lead.subscribe();
                let resumed = active.resumed;
                self.active = active;
                // The agents of the session that was left are gone with it.
                self.follow = None;
                self.followed_tools.clear();
                self.busy = false;
                self.tools.clear();
                self.stream = StreamView::default();
                self.app.start(source).await;
                if resumed {
                    self.recap();
                } else {
                    self.info("started a new session");
                }
            }
            Err(e) => {
                self.fail(&format!("could not open the session: {e:#}"));
                // Keep working in the session we had.
                match self
                    .app
                    .open(
                        Open::Store(self.active.session.clone()),
                        status.chain,
                        status.thinking,
                    )
                    .await
                {
                    Ok(active) => {
                        self.events = active.lead.subscribe();
                        self.active = active;
                    }
                    Err(e) => {
                        self.fail(&format!("could not reopen the previous session either: {e:#}"));
                        self.quit = true;
                    }
                }
            }
        }
    }

    fn record_chain(&mut self, chain: Vec<ModelSpec>) {
        self.active.lead.send(AgentCmd::SetModel(chain.clone()));
        let label = chain
            .iter()
            .map(ModelSpec::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        self.active.session.append(EntryKind::ModelChange { chain });
        self.info(&format!("model: {label}"));
    }

    fn set_model(&mut self, specs: &[&str]) {
        let texts: Vec<String> = specs.iter().map(|s| s.to_string()).collect();
        let chain = match self.app.model_specs(&texts) {
            Ok(chain) => chain,
            Err(e) => return self.fail(e.trim_start_matches("--model ")),
        };
        if self.app.router.resolve(&chain).is_empty() {
            return self.fail("that selection resolves to no model");
        }
        self.record_chain(chain);
    }

    fn set_theme(&mut self, name: &str) {
        let depth = self.theme.depth();
        let (theme, problem) = named_theme(&self.app, name);
        match problem {
            Some(problem) => self.fail(&problem),
            None => {
                self.theme = theme.with_depth(depth);
                self.theme_name = name.to_string();
                self.info(&format!("theme: {name}"));
            }
        }
    }
}

fn first_line(prompt: &str) -> String {
    let line = prompt.lines().next().unwrap_or_default();
    text::truncate(line, 70, "…")
}
