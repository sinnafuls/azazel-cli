//! The status line of the running UI: the layout `[statusline]` asks for, what it shows that
//! does not come from the session (the output of `statusline.command`, git's count of changed
//! files, the clock), and `/statusline`, the popup that sets it up.

use std::time::{Duration, Instant};

use azazel_core::config::{StatusPreset, StatusSeparator, StatuslineConfig};
use azazel_core::process::{RunOptions, Shell, run as run_command};
use azazel_tools::TodoStatus;
use azazel_tui::{Line, text};

use super::overlay::{Action, Overlay};
use super::statusedit::StatusEditor;
use super::statusline::{self, Layout, Name};
use super::{Internal, Tui};

/// The program of `statusline.command` is ended when it takes longer than this.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(2);
/// The program runs when what it is given has changed, and no more often than this.
const COMMAND_EVERY: Duration = Duration::from_secs(1);
/// Git is asked for the changed files no more often than this.
const DIRTY_EVERY: Duration = Duration::from_secs(5);
const DIRTY_TIMEOUT: Duration = Duration::from_secs(5);
/// The longest line of a program that is looked at.
const COMMAND_LINE_BYTES: usize = 2000;

/// What `statusline.command` printed, or why it printed nothing.
pub(super) type CommandLine = Result<Option<String>, String>;

/// The program of `statusline.command` and what it last printed.
#[derive(Default)]
struct Command {
    /// What the program was given when it last ran.
    sent: String,
    /// When what the program is given was last compared with what it was given before.
    checked: Option<Instant>,
    /// A frame came sooner than [`COMMAND_EVERY`] after that: the comparison is still due.
    due: bool,
    running: bool,
    /// The first line it printed; nothing while it has printed none, failed or ran too long.
    line: Option<String>,
    /// It failed, and that has been said.
    complained: bool,
}

/// Git's count of the files that differ from the last commit.
#[derive(Default)]
struct Dirty {
    asked: Option<Instant>,
    running: bool,
    changed: Option<usize>,
}

pub(super) struct StatusBar {
    layout: Layout,
    command: Command,
    dirty: Dirty,
}

impl StatusBar {
    /// Whether the line shows the segment `name`.
    pub(super) fn shows(&self, name: Name) -> bool {
        self.layout.shows(name)
    }

    /// The status line `config` asks for, and what in it could not be understood.
    pub(super) fn new(config: &StatuslineConfig) -> (StatusBar, Vec<String>) {
        let (layout, problems) = Layout::new(config);
        let bar = StatusBar {
            layout,
            command: Command::default(),
            dirty: Dirty::default(),
        };
        (bar, problems)
    }

    /// Something may have changed files: git is asked again before the next frame, and the
    /// program of `statusline.command` has something new to look at.
    pub(super) fn files_changed(&mut self) {
        self.dirty.asked = None;
        self.command.sent.clear();
    }
}

/// The first line `command` prints when it is given `payload` on its standard input.
async fn first_line(shell: Shell, command: String, cwd: std::path::PathBuf, payload: String) -> CommandLine {
    // Only what the program prints on its standard output is its answer.
    let script = match shell.is_posix() {
        true => format!("{{ {command}\n}} 2>/dev/null"),
        false => format!("& {{ {command} }} 2>$null"),
    };
    let mut options = RunOptions::new(cwd);
    options.timeout = Some(COMMAND_TIMEOUT);
    options.stdin = Some(payload.into_bytes());
    options.keep = (1, 0);
    let ran = run_command(&shell, &script, options)
        .await
        .map_err(|e| format!("could not be started: {e}"))?;
    if ran.timed_out {
        return Err(format!(
            "took longer than {} seconds and was ended",
            COMMAND_TIMEOUT.as_secs()
        ));
    }
    match ran.code {
        Some(0) => {}
        Some(code) => return Err(format!("ended with exit code {code}")),
        None => return Err("was killed".to_string()),
    }
    let printed = ran.output.render();
    let line = printed.lines().next().unwrap_or_default();
    let line = match line.char_indices().nth(COMMAND_LINE_BYTES) {
        Some((end, _)) => &line[..end],
        None => line,
    };
    let line = text::keep_colours(line);
    Ok(Some(line).filter(|line| !text::strip_ansi(line).trim().is_empty()))
}

/// `config` with its key `key` set to `value`, as the settings screen hands a value over;
/// `None` when the value is not one the key takes.
fn with_setting(mut config: StatuslineConfig, key: &str, value: &toml::Value) -> Option<StatuslineConfig> {
    let words = |value: &toml::Value| -> Option<Vec<String>> {
        value
            .as_array()?
            .iter()
            .map(|word| word.as_str().map(str::to_string))
            .collect()
    };
    match (key, value) {
        ("preset", toml::Value::String(preset)) => config.preset = preset.parse().ok()?,
        ("separator", toml::Value::String(separator)) => config.separator = separator.parse().ok()?,
        ("left", list) => config.left = words(list)?,
        ("right", list) => config.right = words(list)?,
        ("segments", toml::Value::Table(segments)) => {
            config.segments = segments
                .iter()
                .filter_map(|(name, options)| Some((name.clone(), options.as_table()?.clone())))
                .collect();
        }
        ("command", toml::Value::String(command)) => config.command.clone_from(command),
        _ => return None,
    }
    Some(config)
}

impl Tui {
    /// Session facts on the left, the model and its thinking level against the right edge, or
    /// whatever `[statusline]` asks for.
    pub(super) fn status_line(&self, width: usize) -> Line {
        let status = self.backend.status();
        let facts = self.backend.facts();
        let layout = &self.status.layout;
        let cwd = statusline::display_path(&self.backend.cwd(), std::env::home_dir().as_deref());
        let model = self.model_label();
        let session = match layout.shows(Name::Session) {
            true => self.session.name().unwrap_or_default(),
            false => String::new(),
        };
        let clock = layout.shows(Name::Time).then(|| {
            let now = jiff::Zoned::now();
            (now.hour() as u8, now.minute() as u8, now.second() as u8)
        });
        let extensions = match layout.shows(Name::Extensions) {
            true => self.extras().map(|extras| extras.status()).unwrap_or_default(),
            false => Vec::new(),
        };
        let (agent, agent_color) = self.agent();
        let usage = &status.usage;
        let prompt_tokens = usage.input + usage.cache_read + usage.cache_write;
        let done = facts
            .todo
            .iter()
            .filter(|item| item.status == TodoStatus::Done)
            .count();
        statusline::StatusLine {
            mode: self.backend.mode(),
            coordinator: facts.coordinator,
            agent: &agent,
            agent_color,
            branch: self.branch.as_deref(),
            dirty: self.status.dirty.changed,
            cwd: &cwd,
            context: (status.context_window > 0).then_some((status.context_tokens, status.context_window)),
            rate: self.rate.current(Instant::now()),
            busy: self.busy,
            cost: usage.cost_usd,
            warn_cost: self.env.config.limits.warn_cost_usd,
            billing: &facts.billing,
            account: facts.account.as_deref(),
            account_email: facts.account_email.as_deref(),
            cache: (usage.cache_read * 100).checked_div(prompt_tokens),
            cache_clock: match layout.shows(Name::Cache) {
                true => self.cache_clock().map(|clock| clock.text()),
                false => None,
            },
            agents: self.active_agents(),
            goal: facts.goal.as_deref(),
            todo: Some((done, facts.todo.len())),
            queued: self.queued.len(),
            session: &session,
            clock,
            extensions,
            model: &model,
            tier: facts.tier.map(crate::routing::tier_word),
            thinking: &status.thinking.to_string(),
        }
        .render(layout, self.status.command.line.as_deref(), width, &self.theme)
    }

    /// Starts what feeds the status line, where it is due: the program of
    /// `statusline.command` and git's count of changed files. Called before every frame.
    pub(super) fn feed_status(&mut self) {
        self.run_status_command();
        self.count_changed_files();
    }

    /// When the status line has to be drawn again although nothing happens: the clock moves
    /// on, or the program of `statusline.command` may run again.
    pub(super) fn status_deadline(&self) -> Option<Instant> {
        let layout = &self.status.layout;
        let clock = layout.shows(Name::Time).then(|| {
            let now = jiff::Zoned::now();
            let second = Duration::from_secs(1);
            let into_second = Duration::from_nanos(u64::try_from(now.subsec_nanosecond()).unwrap_or(0));
            let mut left = second.saturating_sub(into_second);
            if !layout.clock_seconds() {
                left += second * u32::try_from(59 - now.second()).unwrap_or(0);
            }
            Instant::now() + left
        });
        let command = &self.status.command;
        let again = command
            .checked
            .filter(|_| command.due && !command.running)
            .map(|checked| checked + COMMAND_EVERY);
        [clock, again, self.cache_clock_deadline()]
            .into_iter()
            .flatten()
            .min()
    }

    /// What `statusline.command` is given on its standard input: the session as JSON, with the
    /// names Claude Code gives the same facts, so that a program written for it works here.
    fn status_payload(&self) -> String {
        let status = self.backend.status();
        let cwd = self.backend.cwd();
        let project = self
            .backend
            .local()
            .map_or_else(|| cwd.clone(), |local| local.app.paths.project_root.clone());
        let model = match &status.model {
            Some(model) => format!("{}/{}", model.provider, model.model),
            None => self.model_label(),
        };
        let name = model.rsplit('/').next().unwrap_or(&model);
        let used = (status.context_tokens * 100).checked_div(status.context_window);
        serde_json::json!({
            "hook_event_name": "Status",
            "session_id": self.session.id(),
            "session": { "id": self.session.id(), "name": self.session.name() },
            "cwd": cwd,
            "workspace": { "current_dir": cwd, "project_dir": project },
            "model": { "id": model, "display_name": name },
            "mode": self.backend.mode().to_string(),
            "thinking": status.thinking.to_string(),
            "branch": self.branch,
            "context": {
                "used_tokens": status.context_tokens,
                "window": status.context_window,
                "used_percentage": used,
            },
            "context_window": {
                "context_window_size": status.context_window,
                "used_percentage": used,
                "remaining_percentage": used.map(|used| 100u64.saturating_sub(used)),
            },
            "cost": { "total_cost_usd": status.usage.cost_usd },
            "version": env!("CARGO_PKG_VERSION"),
        })
        .to_string()
    }

    fn run_status_command(&mut self) {
        let command = self.env.config.statusline.command.trim();
        if command.is_empty() || self.status.command.running {
            return;
        }
        let now = Instant::now();
        let feed = &mut self.status.command;
        if feed
            .checked
            .is_some_and(|checked| now.duration_since(checked) < COMMAND_EVERY)
        {
            feed.due = true;
            return;
        }
        feed.checked = Some(now);
        feed.due = false;
        let command = command.to_string();
        let payload = self.status_payload();
        if payload == self.status.command.sent {
            return;
        }
        self.status.command.sent.clone_from(&payload);
        self.status.command.running = true;
        // The program runs where the UI is, whatever machine the session is on.
        let (shell, cwd) = match self.backend.local() {
            Some(local) => (local.app.shell.clone(), local.app.paths.cwd.clone()),
            None => (
                Shell::resolve(&self.env.config.tools.shell),
                self.env.paths.cwd.clone(),
            ),
        };
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let line = first_line(shell, command, cwd, payload).await;
            let _ = done.send(Internal::StatusCommand(line));
        });
    }

    /// The program of `statusline.command` has ended.
    pub(super) fn on_status_command(&mut self, line: CommandLine) {
        let feed = &mut self.status.command;
        feed.running = false;
        match line {
            Ok(line) => {
                feed.line = line;
                feed.complained = false;
            }
            Err(why) => {
                feed.line = None;
                // Said once, not at every run: the line goes back to its segments meanwhile.
                if !std::mem::replace(&mut feed.complained, true) {
                    self.warn(&format!(
                        "statusline.command {why}; the status line shows its segments"
                    ));
                }
            }
        }
    }

    fn count_changed_files(&mut self) {
        if !self.status.layout.shows(Name::GitDirty) {
            return;
        }
        // A session elsewhere has its files elsewhere.
        let Some(local) = self.backend.local() else {
            return;
        };
        let feed = &mut self.status.dirty;
        if feed.running || feed.asked.is_some_and(|asked| asked.elapsed() < DIRTY_EVERY) {
            return;
        }
        feed.running = true;
        feed.asked = Some(Instant::now());
        let cwd = local.app.paths.cwd.clone();
        let done = self.internal_tx.clone();
        tokio::spawn(async move {
            let changed = tokio::time::timeout(DIRTY_TIMEOUT, crate::git::changed_files(&cwd))
                .await
                .ok()
                .flatten();
            let _ = done.send(Internal::ChangedFiles(changed));
        });
    }

    pub(super) fn on_changed_files(&mut self, changed: Option<usize>) {
        self.status.dirty.running = false;
        self.status.dirty.changed = changed;
    }

    // ------------------------------------------------------------ setting it up

    /// Draws the status line as `config` asks from now on. With `say`, what in it cannot be
    /// understood is said.
    pub(super) fn set_statusline(&mut self, config: StatuslineConfig, say: bool) {
        let (layout, problems) = Layout::new(&config);
        if say {
            for problem in problems {
                self.warn(&problem);
            }
        }
        if config.command.trim() != self.env.config.statusline.command.trim() {
            // Another program, or none: what the last one printed is not its answer.
            self.status.command = Command::default();
        }
        self.status.layout = layout;
        self.status.dirty.asked = None;
        self.env.config.statusline = config;
    }

    /// A key of `[statusline]` the settings screen saved, or is trying on.
    pub(super) fn apply_statusline_setting(&mut self, key: &str, value: &toml::Value) {
        if let Some(config) = with_setting(self.env.config.statusline.clone(), key, value) {
            self.set_statusline(config, true);
        }
    }

    /// `/statusline`: the popup that picks a preset and a separator and puts segments on the
    /// line, takes them off and orders them.
    pub(super) fn open_statusline(&mut self) {
        let subscription = matches!(
            self.backend.facts().billing,
            super::backend::Billing::Subscription { .. }
        );
        let editor = StatusEditor::new(&self.env.config.statusline, subscription);
        self.overlay = Some(Overlay::status(editor, Action::Statusline));
    }

    /// `/statusline <preset>`: the segments of a preset, saved.
    pub(super) fn statusline_preset(&mut self, name: &str) {
        let Ok(preset) = name.parse::<StatusPreset>() else {
            return self.fail(&format!(
                "/statusline takes a preset: {}",
                StatusPreset::NAMES.join(", ")
            ));
        };
        let mut config = self.env.config.statusline.clone();
        config.preset = preset;
        config.left.clear();
        config.right.clear();
        self.save_statusline(config);
    }

    /// Draws the status line as `config` asks and keeps it in `~/.azazel/config.toml`.
    pub(super) fn save_statusline(&mut self, config: StatuslineConfig) {
        let list = |words: &[String]| {
            (!words.is_empty())
                .then(|| toml_edit::value(toml_edit::Array::from_iter(words.iter().map(String::as_str))))
        };
        let separator = (config.separator != StatusSeparator::default())
            .then(|| toml_edit::value(config.separator.as_str()));
        let file = self.env.paths.home_config();
        let saved = [
            ("preset", Some(toml_edit::value(config.preset.as_str()))),
            ("separator", separator),
            ("left", list(&config.left)),
            ("right", list(&config.right)),
        ]
        .into_iter()
        .try_for_each(|(key, value)| crate::init::set_config_value(&file, &["statusline", key], value));
        let preset = config.preset;
        let custom = !config.left.is_empty() || !config.right.is_empty();
        self.set_statusline(config, true);
        let what = match custom {
            true => format!("your own segments (preset {preset})"),
            false => format!("preset {preset}"),
        };
        match saved {
            Ok(()) => self.info(&format!("status line: {what}")),
            Err(e) => self.warn(&format!("status line: {what}, for this session only: {e:#}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell() -> Shell {
        Shell::resolve("")
    }

    async fn printed(command: &str, payload: &str) -> CommandLine {
        let dir = tempfile::tempdir().unwrap();
        first_line(
            shell(),
            command.to_string(),
            dir.path().to_path_buf(),
            payload.to_string(),
        )
        .await
    }

    #[test]
    fn a_setting_of_the_settings_screen_changes_its_key_and_nothing_else() {
        let config: StatuslineConfig =
            toml::from_str("preset = \"full\"\ncommand = \"echo hi\"\nleft = [\"mode\"]").unwrap();
        let set = |key: &str, value: &str| {
            let value: toml::Table = toml::from_str(&format!("v = {value}")).unwrap();
            with_setting(config.clone(), key, &value["v"])
        };
        let preset = set("preset", "\"minimal\"").unwrap();
        assert_eq!(preset.preset, StatusPreset::Minimal);
        assert_eq!(
            (preset.left.as_slice(), preset.command.as_str()),
            (&["mode".to_string()][..], "echo hi")
        );
        assert_eq!(
            set("separator", "\"bar\"").unwrap().separator,
            StatusSeparator::Bar
        );
        assert_eq!(set("left", "[]").unwrap().left, Vec::<String>::new());
        assert_eq!(
            set("right", "[\"time\", \"model\"]").unwrap().right,
            ["time", "model"]
        );
        assert_eq!(set("command", "\"\"").unwrap().command, "");
        let segments = set(
            "segments",
            "{ path = { style = \"full\" }, time = { seconds = true } }",
        )
        .unwrap();
        let (layout, problems) = Layout::new(&segments);
        assert!(problems.is_empty(), "{problems:?}");
        assert!(layout.clock_seconds());
        // What a key does not take changes nothing.
        assert!(set("preset", "\"fancy\"").is_none());
        assert!(set("separator", "3").is_none());
        assert!(set("left", "[1, 2]").is_none());
        assert!(set("colour", "\"red\"").is_none());
    }

    #[tokio::test]
    async fn the_first_line_a_program_prints_is_its_answer() {
        if !shell().is_posix() {
            return;
        }
        // It reads the session from its standard input.
        let line = printed("cat; echo; echo second", "{\"model\":\"m\"}").await;
        assert_eq!(line, Ok(Some("{\"model\":\"m\"}".to_string())));
        // Colours are kept; whatever else would move the cursor is not. What it says on its
        // standard error is not its answer.
        let line = printed(
            "echo warning >&2; printf '\\033[32mmain\\033[0m \\033[2K~/src\\ttab\\n'",
            "",
        )
        .await;
        assert_eq!(line, Ok(Some("\x1b[32mmain\x1b[0m ~/src tab".to_string())));
        assert_eq!(printed("true", "").await, Ok(None));
        assert_eq!(printed("echo '   '", "").await, Ok(None));
    }

    #[tokio::test]
    async fn a_program_that_fails_or_hangs_says_why() {
        if !shell().is_posix() {
            return;
        }
        assert_eq!(
            printed("echo half; exit 3", "").await,
            Err("ended with exit code 3".to_string())
        );
        let started = Instant::now();
        assert_eq!(
            printed("sleep 30", "").await,
            Err("took longer than 2 seconds and was ended".to_string())
        );
        assert!(started.elapsed() < Duration::from_secs(20));
    }
}
