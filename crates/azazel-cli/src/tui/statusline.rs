//! What the status line says, from the facts of the session and `[statusline]`.
//!
//! ```text
//! manual · ⑂ main · ~/src/app · ◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42 · ⇶ 2 agents · ⚑ goal      ⬢ model · high
//! ```
//!
//! The line is made of named segments. Which of them stand on the left and on the right, in
//! which order and with what between them is the user's choice ([`Layout`]); what each one
//! says comes from the session ([`StatusLine`]). A segment whose fact is missing is left out,
//! and when the terminal is too narrow the segment with the lowest priority leaves first.

use std::collections::BTreeMap;

use azazel_core::PermissionMode;
use azazel_core::config::{StatusPreset, StatusSeparator, StatuslineConfig};
use azazel_tui::components::format_tokens;
use azazel_tui::components::status::{self, Segment, Separator};
use azazel_tui::{Glyph, Line, Role, Theme, text};

use super::backend::Billing;
use crate::usage::NEARLY_FULL;

/// The widest the working directory is shown, as a share of the line.
const PATH_SHARE: usize = 3;
/// The working directory keeps at least this many columns before it is shortened.
const PATH_MIN: usize = 16;
/// The widest a session's name is shown, unless `max` says otherwise.
const SESSION_MAX: usize = 24;
/// The widest one text of an extension is shown, unless `max` says otherwise.
const EXTENSION_MAX: usize = 32;
/// In a list of segments: the side is empty, where an empty list would take the preset's.
pub(crate) const NONE: &str = "none";

/// A segment of the status line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Name {
    Mode,
    Model,
    Thinking,
    Tier,
    Path,
    Branch,
    Context,
    Bar,
    Rate,
    Cost,
    Usage,
    Agents,
    Goal,
    Todo,
    Queue,
    Session,
    Time,
    GitDirty,
    Account,
    Cache,
    Extensions,
}

impl Name {
    pub(crate) const ALL: [Name; 21] = [
        Name::Mode,
        Name::Model,
        Name::Thinking,
        Name::Tier,
        Name::Path,
        Name::Branch,
        Name::Context,
        Name::Bar,
        Name::Rate,
        Name::Cost,
        Name::Usage,
        Name::Agents,
        Name::Goal,
        Name::Todo,
        Name::Queue,
        Name::Session,
        Name::Time,
        Name::GitDirty,
        Name::Account,
        Name::Cache,
        Name::Extensions,
    ];

    /// The name `[statusline]` knows the segment by.
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Name::Mode => "mode",
            Name::Model => "model",
            Name::Thinking => "thinking",
            Name::Tier => "tier",
            Name::Path => "path",
            Name::Branch => "branch",
            Name::Context => "context",
            Name::Bar => "bar",
            Name::Rate => "rate",
            Name::Cost => "cost",
            Name::Usage => "usage",
            Name::Agents => "agents",
            Name::Goal => "goal",
            Name::Todo => "todo",
            Name::Queue => "queue",
            Name::Session => "session",
            Name::Time => "time",
            Name::GitDirty => "git_dirty",
            Name::Account => "account",
            Name::Cache => "cache",
            Name::Extensions => "extensions",
        }
    }

    pub(crate) fn parse(name: &str) -> Option<Name> {
        Name::ALL.into_iter().find(|segment| segment.as_str() == name)
    }

    /// What the segment shows, in a few words.
    pub(crate) fn about(self) -> &'static str {
        match self {
            Name::Mode => "the permission mode",
            Name::Model => "the model that answers",
            Name::Thinking => "the thinking level",
            Name::Tier => "the service tier, while one is on",
            Name::Path => "the working directory",
            Name::Branch => "the git branch",
            Name::Context => "how full the context window is",
            Name::Bar => "the context window as a bar",
            Name::Rate => "output tokens per second",
            Name::Cost => "money spent, with an API key",
            Name::Usage => "the fullest usage window of a subscription",
            Name::Agents => "sub-agents at work",
            Name::Goal => "the open goal",
            Name::Todo => "todo items done",
            Name::Queue => "messages queued for the agent",
            Name::Session => "the session's name",
            Name::Time => "the time of day",
            Name::GitDirty => "files changed since the last commit",
            Name::Account => "the account the provider is used with",
            Name::Cache => "the share of the prompt read from the cache",
            Name::Extensions => "what extensions show with azazel.status",
        }
    }

    /// How long the segment stays on a line that is too narrow: the lowest leaves first.
    pub(crate) fn priority(self) -> u8 {
        match self {
            Name::Mode => 100,
            Name::Model => 95,
            Name::Branch => 85,
            Name::GitDirty => 84,
            Name::Path => 80,
            Name::Context => 75,
            Name::Thinking => 70,
            Name::Rate => 60,
            Name::Cost | Name::Usage => 55,
            Name::Agents => 50,
            Name::Goal => 45,
            Name::Tier => 40,
            Name::Queue => 38,
            Name::Todo => 35,
            Name::Extensions => 32,
            Name::Cache => 30,
            Name::Session => 25,
            Name::Account => 20,
            Name::Time => 15,
            Name::Bar => 5,
        }
    }

    /// The options `[statusline.segments.<name>]` takes beside `priority` and `icon`.
    fn own_options(self) -> &'static [&'static str] {
        match self {
            Name::Path | Name::Model | Name::Account => &["max", "style"],
            Name::Context => &["style"],
            Name::Branch | Name::Goal | Name::Session | Name::Extensions => &["max"],
            Name::Time => &["format", "seconds"],
            Name::Bar => &["cells", "min_width"],
            _ => &[],
        }
    }
}

/// The segments of a preset: its left side and its right side.
pub(crate) fn preset(preset: StatusPreset) -> (&'static [Name], &'static [Name]) {
    use Name::*;
    match preset {
        StatusPreset::Default => (
            &[
                Mode, Branch, Path, Context, Bar, Rate, Cost, Usage, Agents, Goal, Extensions,
            ],
            &[Model, Tier, Thinking],
        ),
        StatusPreset::Minimal => (&[Mode, Path], &[Model]),
        StatusPreset::Powerline => (
            &[Mode, Branch, GitDirty, Path, Context, Agents, Extensions],
            &[Cost, Usage, Thinking, Model],
        ),
        StatusPreset::Full => (
            &[
                Mode, Branch, GitDirty, Path, Context, Bar, Rate, Cost, Usage, Cache, Agents, Goal, Todo,
                Queue, Extensions,
            ],
            &[Session, Account, Model, Tier, Thinking, Time],
        ),
    }
}

/// How the working directory is written.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum PathStyle {
    /// `~` for the home directory, and the end of a long path.
    #[default]
    Short,
    /// The whole path.
    Full,
    /// The directory's own name.
    Basename,
}

/// The options every segment takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
struct Common {
    priority: Option<u8>,
    /// The symbol before the text is left out.
    bare: bool,
    /// The most columns of its text.
    max: Option<usize>,
}

/// `[statusline.segments]`, read.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Options {
    common: [Common; Name::ALL.len()],
    path: PathStyle,
    /// The model without its provider.
    short_model: bool,
    /// The context as tokens, `84.0k/200.0k`, instead of a percentage.
    context_tokens: bool,
    /// The account by the e-mail address it signed in with, where it has one.
    account_email: bool,
    hour12: bool,
    seconds: bool,
    bar_cells: u64,
    bar_min_width: usize,
}

impl Default for Options {
    fn default() -> Options {
        Options {
            common: [Common::default(); Name::ALL.len()],
            path: PathStyle::Short,
            short_model: false,
            context_tokens: false,
            account_email: false,
            hour12: false,
            seconds: false,
            bar_cells: status::BAR_CELLS,
            bar_min_width: status::BAR_MIN_WIDTH,
        }
    }
}

impl Options {
    /// Reads the options of every segment. What cannot be understood is left at its default
    /// and named in `problems`.
    fn read(tables: &BTreeMap<String, toml::Table>, problems: &mut Vec<String>) -> Options {
        let mut options = Options::default();
        for (segment, table) in tables {
            let Some(name) = Name::parse(segment) else {
                problems.push(format!(
                    "statusline.segments.{segment}: there is no segment `{segment}` (segments: {})",
                    names()
                ));
                continue;
            };
            for (key, value) in table {
                if let Err(why) = options.set(name, key, value) {
                    problems.push(format!("statusline.segments.{segment}: {why}"));
                }
            }
        }
        options
    }

    fn set(&mut self, name: Name, key: &str, value: &toml::Value) -> Result<(), String> {
        let whole = |least: i64, most: i64| match value.as_integer() {
            Some(number) if (least..=most).contains(&number) => Ok(number),
            _ => Err(format!("`{key}` is a whole number from {least} to {most}")),
        };
        let switch = || value.as_bool().ok_or_else(|| format!("`{key}` is true or false"));
        // Which of `words` the value is.
        let word = |words: &[&str]| {
            value
                .as_str()
                .and_then(|word| words.iter().position(|known| *known == word))
                .ok_or_else(|| format!("`{key}` is one of: {}", words.join(", ")))
        };
        let common = &mut self.common[name as usize];
        match key {
            "priority" => common.priority = Some(whole(0, 100)? as u8),
            "icon" => common.bare = !switch()?,
            "max" if name.own_options().contains(&"max") => common.max = Some(whole(4, 200)? as usize),
            "style" if name == Name::Path => {
                self.path = [PathStyle::Short, PathStyle::Full, PathStyle::Basename]
                    [word(&["short", "full", "basename"])?];
            }
            "style" if name == Name::Model => self.short_model = word(&["full", "short"])? == 1,
            "style" if name == Name::Context => self.context_tokens = word(&["percent", "tokens"])? == 1,
            "style" if name == Name::Account => self.account_email = word(&["label", "email"])? == 1,
            "format" if name == Name::Time => self.hour12 = word(&["24h", "12h"])? == 1,
            "seconds" if name == Name::Time => self.seconds = switch()?,
            "cells" if name == Name::Bar => self.bar_cells = whole(4, 40)? as u64,
            "min_width" if name == Name::Bar => self.bar_min_width = whole(0, 500)? as usize,
            _ => {
                let mut known = vec!["icon", "priority"];
                known.extend(name.own_options());
                known.sort_unstable();
                return Err(format!(
                    "there is no option `{key}` (options: {})",
                    known.join(", ")
                ));
            }
        }
        Ok(())
    }

    fn of(&self, name: Name) -> Common {
        self.common[name as usize]
    }
}

/// The names of all segments, as a message lists them.
fn names() -> String {
    Name::ALL.map(Name::as_str).join(", ")
}

/// `[statusline]`, resolved: which segments stand where, what is between them and how each
/// one is written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Layout {
    pub left: Vec<Name>,
    pub right: Vec<Name>,
    pub separator: Separator,
    options: Options,
}

impl Layout {
    /// The layout `config` asks for, and what in it could not be understood. A segment named
    /// twice stands where it is named first.
    pub(crate) fn new(config: &StatuslineConfig) -> (Layout, Vec<String>) {
        let mut problems = Vec::new();
        let (preset_left, preset_right) = preset(config.preset);
        let mut seen: Vec<Name> = Vec::new();
        let mut side = |key: &str, listed: &[String], preset: &[Name]| -> Vec<Name> {
            let mut out = Vec::new();
            if listed.is_empty() {
                out.extend(preset.iter().copied().filter(|name| !seen.contains(name)));
            }
            for word in listed.iter().filter(|word| *word != NONE) {
                match Name::parse(word) {
                    Some(name) if seen.contains(&name) || out.contains(&name) => {
                        problems.push(format!("statusline.{key}: `{word}` is listed twice"));
                    }
                    Some(name) => out.push(name),
                    None => problems.push(format!(
                        "statusline.{key}: there is no segment `{word}` (segments: {})",
                        names()
                    )),
                }
            }
            seen.extend(&out);
            out
        };
        // A side that is listed wins over the preset's other side for a segment both name.
        let (left, right) = if config.left.is_empty() {
            let right = side("right", &config.right, preset_right);
            (side("left", &config.left, preset_left), right)
        } else {
            let left = side("left", &config.left, preset_left);
            (left, side("right", &config.right, preset_right))
        };
        let separator = match config.separator {
            // The preset called `powerline` is drawn as one unless another separator is chosen.
            StatusSeparator::Dot if config.preset == StatusPreset::Powerline => Separator::Powerline,
            StatusSeparator::Dot => Separator::Dot,
            StatusSeparator::Bar => Separator::Bar,
            StatusSeparator::Space => Separator::Space,
            StatusSeparator::Powerline => Separator::Powerline,
        };
        let options = Options::read(&config.segments, &mut problems);
        (
            Layout {
                left,
                right,
                separator,
                options,
            },
            problems,
        )
    }

    /// Whether `name` is on the line.
    pub(crate) fn shows(&self, name: Name) -> bool {
        self.left.contains(&name) || self.right.contains(&name)
    }

    /// Whether the clock shows seconds, and so has to be drawn every second.
    pub(crate) fn clock_seconds(&self) -> bool {
        self.options.seconds
    }
}

/// The facts of a session the status line is made of.
pub(super) struct StatusLine<'a> {
    pub mode: PermissionMode,
    pub coordinator: bool,
    pub branch: Option<&'a str>,
    /// Files changed since the last commit, once git has been asked.
    pub dirty: Option<usize>,
    /// The working directory, with `~` for the home directory.
    pub cwd: &'a str,
    /// Tokens in the context window and its size; none while the size is unknown.
    pub context: Option<(u64, u64)>,
    /// Output tokens per second of the lead.
    pub rate: Option<f64>,
    /// The lead is at work: the rate is of now, not of the turn that ended.
    pub busy: bool,
    /// What the lead has cost, when that is known.
    pub cost: Option<f64>,
    /// The cost from which it is shown as a warning; 0 never warns.
    pub warn_cost: f64,
    pub billing: &'a Billing,
    /// The account of the lead's provider that requests go out with.
    pub account: Option<&'a str>,
    /// The e-mail address that account signed in with, as it may be shown.
    pub account_email: Option<&'a str>,
    /// The share of the prompt tokens so far that the provider read from its cache, in percent.
    pub cache: Option<u64>,
    /// Agents at work beside the lead.
    pub agents: usize,
    pub goal: Option<&'a str>,
    /// Todo items done, and how many there are.
    pub todo: Option<(usize, usize)>,
    /// Messages waiting for the agent to finish.
    pub queued: usize,
    /// The session's name; empty while it has none.
    pub session: &'a str,
    /// The time of day as hours, minutes and seconds.
    pub clock: Option<(u8, u8, u8)>,
    /// What extensions show with `azazel.status`, in the order they first set it.
    pub extensions: Vec<String>,
    /// The model the lead answers with; empty when there is none.
    pub model: &'a str,
    pub tier: Option<&'a str>,
    pub thinking: &'a str,
}

impl StatusLine<'_> {
    /// The line in `layout`, `width` columns wide. `command` is the output of
    /// `statusline.command`, which stands in for the left side.
    pub fn render(&self, layout: &Layout, command: Option<&str>, width: usize, theme: &Theme) -> Line {
        let side = |names: &[Name]| {
            let mut segments = Vec::with_capacity(names.len() + 1);
            for (at, name) in names.iter().enumerate() {
                // The bar belongs to the percentage it draws, when it stands right behind it.
                let joined = at > 0 && names[at - 1] == Name::Context;
                self.segment(*name, joined, layout, width, theme, &mut segments);
            }
            segments
        };
        let left = match command {
            Some(line) => vec![Segment::styled(line).priority(u8::MAX)],
            None => side(&layout.left),
        };
        status::render(&left, &side(&layout.right), layout.separator, width, theme)
    }

    /// Adds what `name` shows to `out`; nothing when its fact is missing.
    fn segment(
        &self,
        name: Name,
        joined: bool,
        layout: &Layout,
        width: usize,
        theme: &Theme,
        out: &mut Vec<Segment>,
    ) {
        let options = &layout.options;
        let common = options.of(name);
        let priority = common.priority.unwrap_or(name.priority());
        let labelled = |glyph: Glyph, text: &str| match common.bare {
            true => text.to_string(),
            false => status::labelled(glyph, text, theme),
        };
        let cut = |text: &str, default: Option<usize>| match common.max.or(default) {
            Some(max) => text::truncate(text, max, theme.glyph(Glyph::Ellipsis)),
            None => text.to_string(),
        };
        let percent = self
            .context
            .and_then(|(used, window)| (used * 100).checked_div(window));
        let segment = match name {
            Name::Mode => {
                let mode = self.mode.to_string();
                let (role, glyph) = match self.mode {
                    PermissionMode::Plan => (Role::ModePlan, Some(Glyph::ModePlan)),
                    PermissionMode::Manual => (Role::Muted, None),
                    PermissionMode::AcceptEdits => (Role::ModeEdits, Some(Glyph::ModeEdits)),
                    PermissionMode::Auto => (Role::ModeAuto, Some(Glyph::ModeAuto)),
                    // No guard is left: the mode stands in the colour of errors.
                    PermissionMode::Bypass => (Role::Error, Some(Glyph::Warning)),
                };
                let text = match glyph {
                    Some(glyph) => labelled(glyph, &mode),
                    None => mode,
                };
                out.push(Segment::new(role, text).filled(role).priority(priority));
                // Coordinator mode is part of how the session works, and stays nearly as long.
                if self.coordinator {
                    let priority = priority.saturating_sub(10);
                    out.push(Segment::new(Role::Accent, "coordinator").priority(priority));
                }
                None
            }
            Name::Branch => self
                .branch
                .map(|branch| Segment::new(Role::Muted, labelled(Glyph::Branch, &cut(branch, None)))),
            Name::GitDirty => self
                .dirty
                .filter(|changed| *changed > 0)
                .map(|changed| Segment::new(Role::Warning, labelled(Glyph::Dirty, &changed.to_string()))),
            Name::Path => {
                let path = match options.path {
                    PathStyle::Short => {
                        let room = common.max.unwrap_or((width / PATH_SHARE).max(PATH_MIN));
                        status::shorten_path(self.cwd, room, theme)
                    }
                    PathStyle::Full => match common.max {
                        Some(max) => status::shorten_path(self.cwd, max, theme),
                        None => self.cwd.to_string(),
                    },
                    PathStyle::Basename => {
                        let name = self.cwd.trim_end_matches(['/', '\\']);
                        let name = name.rsplit(['/', '\\']).next().unwrap_or(name);
                        cut(if name.is_empty() { self.cwd } else { name }, None)
                    }
                };
                Some(Segment::new(Role::Muted, path))
            }
            Name::Context => percent.map(|percent| {
                let text = match (options.context_tokens, self.context) {
                    (true, Some((used, window))) => {
                        format!("{}/{}", format_tokens(used), format_tokens(window))
                    }
                    _ => format!("{percent}%"),
                };
                let role = status::context_role(percent).unwrap_or(Role::Muted);
                Segment::new(role, labelled(Glyph::Context, &text))
            }),
            // The bar is an ornament: it is drawn on a wide terminal and is the first to leave.
            Name::Bar => percent.filter(|_| width >= options.bar_min_width).map(|percent| {
                let bar = status::context_bar(percent, options.bar_cells, theme);
                if joined { bar.joined() } else { bar }
            }),
            Name::Rate => self.rate.and_then(|rate| status::rate(rate, theme)).map(|rate| {
                let rate = match common.bare {
                    true => rate
                        .strip_prefix(&theme.glyphs().prefix(Glyph::Rate))
                        .unwrap_or(&rate)
                        .to_string(),
                    false => rate,
                };
                Segment::new(if self.busy { Role::Accent } else { Role::Muted }, rate)
            }),
            // Tokens of a subscription are not paid for one by one: what counts there is how
            // much of its usage window is left.
            Name::Cost => match self.billing {
                Billing::Metered => self.cost.map(|cost| {
                    let warns = self.warn_cost > 0.0 && cost >= self.warn_cost;
                    Segment::new(
                        if warns { Role::Warning } else { Role::Muted },
                        status::cost(cost, theme),
                    )
                }),
                Billing::Subscription { .. } => None,
            },
            Name::Usage => match self.billing {
                Billing::Subscription {
                    window: Some((window, used)),
                } => {
                    let percent = (used * 100.0).round().clamp(0.0, 999.0) as u32;
                    let role = if *used >= NEARLY_FULL {
                        Role::Warning
                    } else {
                        Role::Muted
                    };
                    Some(Segment::new(
                        role,
                        labelled(Glyph::Usage, &format!("{percent}% {window}")),
                    ))
                }
                _ => None,
            },
            Name::Cache => self
                .cache
                .map(|share| Segment::new(Role::Muted, labelled(Glyph::Cache, &format!("{share}%")))),
            Name::Agents => match self.agents {
                0 => None,
                1 => Some(Segment::new(Role::Accent, labelled(Glyph::Agents, "1 agent"))),
                n => Some(Segment::new(
                    Role::Accent,
                    labelled(Glyph::Agents, &format!("{n} agents")),
                )),
            },
            Name::Goal => self
                .goal
                .map(|goal| Segment::new(Role::Accent, labelled(Glyph::Goal, &cut(goal, None)))),
            Name::Todo => self.todo.filter(|(_, total)| *total > 0).map(|(done, total)| {
                let role = if done == total { Role::Success } else { Role::Muted };
                Segment::new(role, labelled(Glyph::Tasks, &format!("{done}/{total}")))
            }),
            Name::Queue => (self.queued > 0).then(|| {
                Segment::new(
                    Role::Accent,
                    labelled(Glyph::Queued, &format!("{} queued", self.queued)),
                )
            }),
            Name::Session => (!self.session.is_empty()).then(|| {
                Segment::new(
                    Role::Muted,
                    labelled(Glyph::Session, &cut(self.session, Some(SESSION_MAX))),
                )
            }),
            Name::Account => self.account.map(|account| {
                // A stored key has no address: its label stands for it.
                let account = match (options.account_email, self.account_email) {
                    (true, Some(email)) => email,
                    _ => account,
                };
                Segment::new(Role::Muted, labelled(Glyph::Account, &cut(account, None)))
            }),
            Name::Model => {
                let model = match (self.model.is_empty(), options.short_model) {
                    (true, _) => "no model".to_string(),
                    (false, false) => self.model.to_string(),
                    // `virtual/auto -> provider/model` keeps both of its names.
                    (false, true) => self
                        .model
                        .split(" -> ")
                        .map(|model| model.split_once('/').map_or(model, |(_, id)| id))
                        .collect::<Vec<_>>()
                        .join(" -> "),
                };
                let icon = match common.bare {
                    true => String::new(),
                    false => theme.glyphs().prefix(Glyph::Model),
                };
                Some(
                    Segment::new(Role::Accent, icon)
                        .and(Role::Muted, cut(&model, None))
                        .filled(Role::Accent),
                )
            }
            Name::Tier => self.tier.map(|tier| Segment::new(Role::Muted, tier)),
            Name::Thinking => Some(Segment::new(Role::Thinking, self.thinking)),
            Name::Time => self.clock.map(|(hour, minute, second)| {
                let mut time = match options.hour12 {
                    true => format!("{}:{minute:02}", (hour + 11) % 12 + 1),
                    false => format!("{hour:02}:{minute:02}"),
                };
                if options.seconds {
                    time.push_str(&format!(":{second:02}"));
                }
                if options.hour12 {
                    time.push_str(if hour < 12 { " am" } else { " pm" });
                }
                Segment::new(Role::Muted, labelled(Glyph::Clock, &time))
            }),
            // Each text is a segment of its own, so the separator of the line stands between
            // them. A script decides what the text is: one line of it is shown, without
            // anything that would move the cursor or change colours.
            Name::Extensions => {
                for shown in &self.extensions {
                    let shown = text::sanitize(shown);
                    let shown = shown.lines().next().unwrap_or_default().trim();
                    if !shown.is_empty() {
                        let shown = cut(shown, Some(EXTENSION_MAX));
                        out.push(Segment::new(Role::Muted, shown).priority(priority));
                    }
                }
                None
            }
        };
        out.extend(segment.map(|segment| segment.priority(priority)));
    }
}

/// A session with every fact present, to show what a layout looks like.
pub(super) fn sample(billing: &Billing) -> StatusLine<'_> {
    StatusLine {
        mode: PermissionMode::AcceptEdits,
        coordinator: false,
        branch: Some("main"),
        dirty: Some(3),
        cwd: "~/src/app",
        context: Some((84_000, 200_000)),
        rate: Some(78.0),
        busy: true,
        cost: Some(0.42),
        warn_cost: 0.0,
        billing,
        account: Some("work"),
        account_email: Some("j***@example.com"),
        cache: Some(87),
        agents: 2,
        goal: Some("goal active"),
        todo: Some((2, 5)),
        queued: 1,
        session: "fix the build",
        clock: Some((14, 5, 9)),
        extensions: vec!["ci ok".to_string()],
        model: "anthropic/claude-opus-5-5",
        tier: Some("priority"),
        thinking: "high",
    }
}

/// `path` as the status line names it: `~` stands for `home`, with forward slashes below it.
pub(super) fn display_path(path: &std::path::Path, home: Option<&std::path::Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_string(),
        Some(rest) => format!("~/{}", rest.to_string_lossy().replace('\\', "/")),
        None => path.display().to_string(),
    }
}

#[cfg(test)]
mod tests {
    use azazel_tui::GlyphLevel;

    use super::*;

    const METERED: Billing = Billing::Metered;

    fn full(billing: &Billing) -> StatusLine<'_> {
        StatusLine {
            mode: PermissionMode::Plan,
            coordinator: false,
            branch: Some("main"),
            dirty: None,
            cwd: "~/src/app",
            context: Some((84_000, 200_000)),
            rate: Some(78.3),
            busy: true,
            cost: Some(0.42),
            warn_cost: 0.0,
            billing,
            account: None,
            account_email: None,
            cache: None,
            agents: 2,
            goal: Some("goal active, attempt 1"),
            todo: None,
            queued: 0,
            session: "",
            clock: None,
            extensions: Vec::new(),
            model: "anthropic/claude-opus-5-5",
            tier: Some("priority"),
            thinking: "high",
        }
    }

    /// The layout of a `[statusline]` table that has nothing wrong with it.
    fn layout(table: &str) -> Layout {
        let config: StatuslineConfig = toml::from_str(table).unwrap();
        let (layout, problems) = Layout::new(&config);
        assert!(problems.is_empty(), "{problems:?}");
        layout
    }

    fn problems(table: &str) -> Vec<String> {
        Layout::new(&toml::from_str(table).unwrap()).1
    }

    fn shown_in(line: &StatusLine, layout: &Layout, width: usize, theme: &Theme) -> String {
        text::strip_ansi(&line.render(layout, None, width, theme))
    }

    /// The line in the default layout.
    fn shown(line: &StatusLine, width: usize, theme: &Theme) -> String {
        shown_in(line, &layout(""), width, theme)
    }

    fn painted(line: &StatusLine, width: usize, theme: &Theme) -> String {
        line.render(&layout(""), None, width, theme)
    }

    /// `left` and `right` with as many spaces between them as make the line `width` wide.
    fn sides(left: &str, right: &str, width: usize) -> String {
        let gap = width - text::width(left) - text::width(right);
        format!("{left}{}{right}", " ".repeat(gap))
    }

    #[test]
    fn every_fact_has_its_place_and_the_bar_needs_a_wide_terminal() {
        let theme = Theme::dark();
        let line = full(&METERED);
        assert_eq!(
            shown(&line, 160, &theme),
            sides(
                "⏸ plan · ⑂ main · ~/src/app · ◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42 · ⇶ 2 agents · \
                 ⚑ goal active, attempt 1",
                "⬢ anthropic/claude-opus-5-5 · priority · high",
                160
            )
        );
        // Under a hundred columns the percentage stands alone, and above it the bar is only
        // drawn where nothing has to leave the line for it.
        let short = StatusLine {
            agents: 0,
            goal: None,
            tier: None,
            ..full(&METERED)
        };
        assert!(shown(&short, 99, &theme).contains("◫ 42% · ↯ 78 tok/s · $0.42  "));
        assert!(shown(&short, 100, &theme).contains("◫ 42% · ↯ 78 tok/s · $0.42  "));
        assert!(shown(&short, 110, &theme).contains("◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42  "));

        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(
            shown(&line, 160, &ascii),
            sides(
                "|| plan | git: main | ~/src/app | ctx 42% [===>----] | 78 tok/s | $0.42 | 2 agents | \
                 goal active, attempt 1",
                "anthropic/claude-opus-5-5 | priority | high",
                160
            )
        );
        let nerd = shown(&line, 160, &Theme::dark().with_glyphs(GlyphLevel::Nerd));
        assert!(nerd.starts_with("\u{f04c} plan · \u{e0a0} main · "), "{nerd:?}");
    }

    #[test]
    fn on_a_narrow_line_what_matters_least_leaves_first() {
        let theme = Theme::dark();
        let line = full(&METERED);
        // Each segment by what marks it, from the one that stays longest to the first to go.
        let marks = [
            "plan", "⬢", "⑂", "~/", "◫", "high", "↯", "$", "⇶", "⚑", "priority", "[",
        ];
        let mut kept = marks.len();
        for width in (20..=200).rev() {
            let row = shown(&line, width, &theme);
            assert!(text::width(&row) <= width, "width {width}: {row:?}");
            let present: Vec<bool> = marks.iter().map(|mark| row.contains(mark)).collect();
            let count = present.iter().filter(|here| **here).count();
            // Whatever is shown is the top of the order: nothing that matters more is missing.
            assert!(
                present.iter().take(count).all(|here| *here),
                "width {width}: {row:?}"
            );
            assert!(
                count <= kept,
                "width {width} shows more than a wider line: {row:?}"
            );
            kept = count;
            // The mode stays whatever happens, and the model for as long as anything else does.
            assert!(row.starts_with("⏸ plan"), "width {width}: {row:?}");
            if count > 2 {
                assert!(
                    row.contains("⬢ anthropic/claude-opus-5-5"),
                    "width {width}: {row:?}"
                );
            }
        }
        assert_eq!(shown(&line, 200, &theme).matches(" · ").count(), 9);
        assert_eq!(shown(&line, 35, &theme), "⏸ plan  ⬢ anthropic/claude-opus-5-5");
        assert_eq!(shown(&line, 20, &theme), "⏸ plan  ⬢ anthropic…");

        // A priority of its own keeps a segment longer, or sends it away sooner.
        let goal_first = layout("[segments.goal]\npriority = 99\n[segments.branch]\npriority = 1");
        let row = shown_in(&line, &goal_first, 70, &theme);
        assert_eq!(
            row,
            sides(
                "⏸ plan · ⚑ goal active, attempt 1",
                "⬢ anthropic/claude-opus-5-5",
                70
            )
        );
    }

    #[test]
    fn no_line_is_wider_than_the_terminal_in_any_preset_at_any_glyph_level() {
        let subscription = Billing::Subscription {
            window: Some(("5h".to_string(), 0.61)),
        };
        let mut long = sample(&subscription);
        long.cwd = "~/projects/a-rather-long-directory/with/several/levels/below/it";
        long.coordinator = true;
        let every = sample(&METERED);
        for preset in StatusPreset::NAMES {
            for separator in StatusSeparator::NAMES {
                let layout = layout(&format!("preset = \"{preset}\"\nseparator = \"{separator}\""));
                for level in GlyphLevel::ALL {
                    let theme = Theme::dark().with_glyphs(level);
                    for width in 0..=200 {
                        for line in [&full(&METERED), &long, &every] {
                            let row = shown_in(line, &layout, width, &theme);
                            let at = format!("{preset} {separator} {level} at {width}");
                            assert!(text::width(&row) <= width, "{at}: {row:?}");
                            if level == GlyphLevel::Ascii {
                                assert!(row.is_ascii(), "{at}: {row:?}");
                            }
                            // From twenty columns on the mode is there to read.
                            if width >= 20 {
                                let mode = line.mode.to_string();
                                assert!(row.contains(&mode[..4]), "{at}: {row:?}");
                            }
                        }
                    }
                }
            }
        }
        // A long directory keeps its end and no more than a third of the line.
        let long = StatusLine {
            mode: PermissionMode::Plan,
            ..long
        };
        let row = shown(&long, 120, &Theme::dark());
        assert!(
            row.starts_with("⏸ plan · coordinator · ⑂ main · …/with/several/levels/below/it · ◫ 42% · "),
            "{row:?}"
        );
        let row = shown(&long, 200, &Theme::dark());
        assert!(
            row.contains(" · ~/projects/a-rather-long-directory/with/several/levels/below/it · "),
            "{row:?}"
        );
    }

    #[test]
    fn a_subscription_shows_its_usage_window_and_never_a_cost() {
        let theme = Theme::dark();
        let subscription = Billing::Subscription {
            window: Some(("5h".to_string(), 0.61)),
        };
        let line = full(&subscription);
        let row = shown(&line, 160, &theme);
        assert!(row.contains(" · ↯ 78 tok/s · ◔ 61% 5h · ⇶ 2 agents"), "{row:?}");
        assert!(!row.contains('$'), "{row:?}");
        // It is quiet until the window is nearly full.
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Muted, "◔ 61% 5h")));
        let nearly = Billing::Subscription {
            window: Some(("7d".to_string(), 0.8)),
        };
        assert!(painted(&full(&nearly), 160, &theme).contains(&theme.paint(Role::Warning, "◔ 80% 7d")));

        // No window is known yet: still no cost.
        let unknown = Billing::Subscription { window: None };
        let row = shown(&full(&unknown), 160, &theme);
        assert!(
            row.contains("tok/s · ⇶ 2 agents") && !row.contains('$'),
            "{row:?}"
        );
    }

    #[test]
    fn a_key_shows_the_cost_once_it_is_known_and_warns_past_the_budget() {
        let theme = Theme::dark();
        let mut line = full(&METERED);
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Muted, "$0.42")));
        line.warn_cost = 0.4;
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Warning, "$0.42")));
        line.warn_cost = 0.5;
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Muted, "$0.42")));
        line.cost = Some(12.345);
        assert!(shown(&line, 160, &theme).contains(" · $12.3 · "));
        line.cost = None;
        assert!(!shown(&line, 160, &theme).contains('$'));
    }

    #[test]
    fn the_rate_is_bright_while_the_lead_works_and_quiet_afterwards() {
        let theme = Theme::dark();
        let mut line = full(&METERED);
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Accent, "↯ 78 tok/s")));
        line.busy = false;
        assert!(painted(&line, 160, &theme).contains(&theme.paint(Role::Muted, "↯ 78 tok/s")));
        line.rate = Some(0.4);
        assert!(!shown(&line, 160, &theme).contains("tok/s"));
        line.rate = None;
        assert!(!shown(&line, 160, &theme).contains("tok/s"));
    }

    #[test]
    fn modes_and_missing_facts() {
        let theme = Theme::dark();
        let bare = StatusLine {
            mode: PermissionMode::Manual,
            coordinator: true,
            branch: None,
            context: None,
            rate: None,
            busy: false,
            cost: None,
            agents: 1,
            goal: None,
            model: "",
            tier: None,
            thinking: "off",
            cwd: "~",
            ..full(&METERED)
        };
        assert_eq!(
            shown(&bare, 60, &theme),
            sides("manual · coordinator · ~ · ⇶ 1 agent", "⬢ no model · off", 60)
        );
        // Nothing of the full preset is shown without its fact, either.
        assert_eq!(
            shown_in(&bare, &layout("preset = \"full\""), 60, &theme),
            sides("manual · coordinator · ~ · ⇶ 1 agent", "⬢ no model · off", 60)
        );
        let mode = |mode| {
            shown(
                &StatusLine {
                    mode,
                    agents: 0,
                    coordinator: false,
                    ..full(&METERED)
                },
                30,
                &theme,
            )
        };
        assert!(mode(PermissionMode::AcceptEdits).starts_with("⏵ accept-edits"));
        assert!(mode(PermissionMode::Auto).starts_with("⏵⏵ auto"));
        assert!(mode(PermissionMode::Manual).starts_with("manual"));
        // Bypass stands out: its own mark, in the colour of errors.
        assert!(mode(PermissionMode::Bypass).starts_with("⚠ bypass"));
        let bypass = StatusLine {
            mode: PermissionMode::Bypass,
            ..full(&METERED)
        };
        let row = painted(&bypass, 80, &theme);
        assert!(
            row.starts_with(&format!("{}⚠ bypass", theme.fg(Role::Error))),
            "{row:?}"
        );
    }

    #[test]
    fn the_full_preset_shows_every_segment_that_has_something_to_say() {
        let theme = Theme::dark();
        let full = layout("preset = \"full\"");
        assert_eq!(
            shown_in(&sample(&METERED), &full, 240, &theme),
            sides(
                "⏵ accept-edits · ⑂ main · ± 3 · ~/src/app · ◫ 42% [━━━╸────] · ↯ 78 tok/s · $0.42 · ≋ 87% · \
                 ⇶ 2 agents · ⚑ goal active · ☑ 2/5 · ⏵ 1 queued · ci ok",
                "❖ fix the build · ♙ work · ⬢ anthropic/claude-opus-5-5 · priority · high · ◷ 14:05",
                240
            )
        );
        let subscription = Billing::Subscription {
            window: Some(("5h".to_string(), 0.61)),
        };
        let row = shown_in(&sample(&subscription), &full, 240, &theme);
        assert!(row.contains(" · ↯ 78 tok/s · ◔ 61% 5h · ≋ 87% · "), "{row:?}");
        let ascii = Theme::dark().with_glyphs(GlyphLevel::Ascii);
        assert_eq!(
            shown_in(&sample(&METERED), &full, 240, &ascii),
            sides(
                "> accept-edits | git: main | +- 3 | ~/src/app | ctx 42% [===>----] | 78 tok/s | $0.42 | \
                 cache 87% | 2 agents | goal active | todo 2/5 | > 1 queued | ci ok",
                "fix the build | as work | anthropic/claude-opus-5-5 | priority | high | 14:05",
                240
            )
        );
        // A clean tree, a finished list and an empty queue.
        let done = StatusLine {
            dirty: Some(0),
            todo: Some((5, 5)),
            queued: 0,
            ..sample(&METERED)
        };
        let row = done.render(&full, None, 240, &theme);
        assert!(row.contains(&theme.paint(Role::Success, "☑ 5/5")), "{row:?}");
        let row = text::strip_ansi(&row);
        assert!(!row.contains('±') && !row.contains("queued"), "{row:?}");

        let minimal = layout("preset = \"minimal\"");
        assert_eq!(
            shown_in(&sample(&METERED), &minimal, 60, &theme),
            sides("⏵ accept-edits · ~/src/app", "⬢ anthropic/claude-opus-5-5", 60)
        );
    }

    #[test]
    fn the_lists_say_which_segments_stand_where() {
        let theme = Theme::dark();
        let line = sample(&METERED);
        let custom = layout("left = [\"model\", \"time\"]\nright = [\"path\", \"mode\"]");
        assert_eq!(
            shown_in(&line, &custom, 70, &theme),
            sides(
                "⬢ anthropic/claude-opus-5-5 · ◷ 14:05",
                "~/src/app · ⏵ accept-edits",
                70
            )
        );
        // One list alone leaves the other side to the preset, less what the list took.
        let moved = layout("left = [\"mode\", \"thinking\"]");
        assert_eq!(moved.left, [Name::Mode, Name::Thinking]);
        assert_eq!(moved.right, [Name::Model, Name::Tier]);
        let moved = layout("preset = \"minimal\"\nright = [\"path\"]");
        assert_eq!((moved.left, moved.right), (vec![Name::Mode], vec![Name::Path]));
        // `none` is a side with nothing on it.
        let bare = layout("right = [\"none\"]\npreset = \"minimal\"");
        assert_eq!(shown_in(&line, &bare, 40, &theme), "⏵ accept-edits · ~/src/app");
        assert!(bare.shows(Name::Path) && !bare.shows(Name::Model));
    }

    #[test]
    fn what_cannot_be_understood_is_said_and_left_out() {
        let found = problems("left = [\"mode\", \"clock\", \"mode\"]\nright = [\"mode\"]");
        assert_eq!(found.len(), 3, "{found:?}");
        assert!(
            found[0].starts_with("statusline.left: there is no segment `clock` (segments: mode, model, ")
        );
        assert_eq!(found[1], "statusline.left: `mode` is listed twice");
        assert_eq!(found[2], "statusline.right: `mode` is listed twice");
        let (layout, _) = Layout::new(&toml::from_str("left = [\"mode\", \"clock\"]").unwrap());
        assert_eq!(layout.left, [Name::Mode]);

        let found = problems(
            "[segments.clock]\nstyle = \"x\"\n[segments.path]\nstyle = \"tiny\"\ncolour = 1\n\
             [segments.time]\nseconds = \"yes\"\n[segments.mode]\npriority = 101\nmax = 9",
        );
        assert_eq!(
            found,
            [
                "statusline.segments.clock: there is no segment `clock` (segments: mode, model, thinking, \
                 tier, path, branch, context, bar, rate, cost, usage, agents, goal, todo, queue, session, \
                 time, git_dirty, account, cache, extensions)",
                "statusline.segments.mode: there is no option `max` (options: icon, priority)",
                "statusline.segments.mode: `priority` is a whole number from 0 to 100",
                "statusline.segments.path: there is no option `colour` (options: icon, max, priority, style)",
                "statusline.segments.path: `style` is one of: short, full, basename",
                "statusline.segments.time: `seconds` is true or false",
            ]
        );
    }

    #[test]
    fn segments_take_options() {
        let theme = Theme::dark();
        let mut line = sample(&METERED);
        line.cwd = "~/projects/azazel/crates/azazel-tui";
        let at = |table: &str, width: usize| shown_in(&line, &layout(table), width, &theme);
        let path = |style: &str| {
            at(
                &format!("left = [\"path\"]\nright = [\"none\"]\n[segments.path]\n{style}"),
                60,
            )
        };
        assert_eq!(path("style = \"short\""), "…/crates/azazel-tui");
        assert_eq!(path("style = \"full\""), "~/projects/azazel/crates/azazel-tui");
        assert_eq!(path("style = \"basename\""), "azazel-tui");
        assert_eq!(path("max = 12"), "…/azazel-tui");
        assert_eq!(path("style = \"full\"\nmax = 30"), "…/azazel/crates/azazel-tui");

        let right = |table: &str| {
            at(&format!("left = [\"none\"]\n{table}"), 60)
                .trim_start()
                .to_string()
        };
        assert_eq!(
            right("right = [\"model\"]\n[segments.model]\nstyle = \"short\""),
            "⬢ claude-opus-5-5"
        );
        assert_eq!(
            right("right = [\"model\"]\n[segments.model]\nicon = false\nmax = 12"),
            "anthropic/c…"
        );
        assert_eq!(
            right("right = [\"context\"]\n[segments.context]\nstyle = \"tokens\""),
            "◫ 84.0k/200.0k"
        );
        assert_eq!(
            right(
                "right = [\"branch\", \"rate\"]\n[segments.branch]\nicon = false\n[segments.rate]\nicon = false"
            ),
            "main · 78 tok/s"
        );
        assert_eq!(
            right("right = [\"time\"]\n[segments.time]\nformat = \"12h\"\nseconds = true"),
            "◷ 2:05:09 pm"
        );
        assert_eq!(
            right("right = [\"session\"]\n[segments.session]\nmax = 8"),
            "❖ fix the…"
        );
        // The account by its label, or by the address it signed in with.
        assert_eq!(right("right = [\"account\"]"), "♙ work");
        assert_eq!(
            right("right = [\"account\"]\n[segments.account]\nstyle = \"email\""),
            "♙ j***@example.com"
        );
        assert_eq!(
            right("right = [\"account\"]\n[segments.account]\nstyle = \"email\"\nmax = 9"),
            "♙ j***@exa…"
        );
        // A bar of another length, on a terminal of any width, wherever it stands.
        assert_eq!(
            right("right = [\"bar\", \"context\"]\n[segments.bar]\ncells = 4\nmin_width = 0"),
            "[━╸──] · ◫ 42%"
        );
        line.model = "virtual/auto -> anthropic/claude-opus-5-5";
        assert_eq!(
            shown_in(
                &line,
                &layout("left = [\"none\"]\nright = [\"model\"]\n[segments.model]\nstyle = \"short\""),
                60,
                &theme
            )
            .trim_start(),
            "⬢ auto -> claude-opus-5-5"
        );
        // A stored key signed in with no address: its label stands for it.
        line.account_email = None;
        assert_eq!(
            shown_in(
                &line,
                &layout("left = [\"account\"]\nright = [\"none\"]\n[segments.account]\nstyle = \"email\""),
                60,
                &theme
            ),
            "♙ work"
        );
        line.clock = Some((0, 7, 0));
        assert_eq!(
            shown_in(
                &line,
                &layout("left = [\"time\"]\nright = [\"none\"]\n[segments.time]\nformat = \"12h\""),
                60,
                &theme
            ),
            "◷ 12:07 am"
        );
    }

    #[test]
    fn the_powerline_preset_is_drawn_in_blocks_unless_another_separator_is_chosen() {
        let nerd = Theme::dark().with_glyphs(GlyphLevel::Nerd);
        let line = full(&METERED);
        let blocks = layout("preset = \"powerline\"");
        assert_eq!(blocks.separator, Separator::Powerline);
        assert_eq!(
            shown_in(&line, &blocks, 100, &nerd),
            sides(
                " \u{f04c} plan \u{e0b0} \u{e0a0} main \u{e0b0} ~/src/app \u{e0b0} \u{e70f} 42% \u{e0b0} \
                 \u{f0c0} 2 agents \u{e0b0}",
                "\u{e0b2} \u{f155} 0.42 \u{e0b2} high \u{e0b2} \u{ec19} anthropic/claude-opus-5-5 ",
                100
            )
        );
        // The mode and the model are filled with their colours.
        let row = line.render(&blocks, None, 100, &nerd);
        assert!(
            row.starts_with(&nerd.bg_color(nerd.color(Role::ModePlan))),
            "{row:?}"
        );
        let (_, model) = row
            .split_once(&nerd.bg_color(nerd.color(Role::Accent)))
            .expect("a block in the accent colour");
        assert_eq!(text::strip_ansi(model), " \u{ec19} anthropic/claude-opus-5-5 ");
        let bars = layout("preset = \"powerline\"\nseparator = \"bar\"");
        assert_eq!(bars.separator, Separator::Bar);
        assert!(shown_in(&line, &bars, 100, &nerd).starts_with("\u{f04c} plan │ \u{e0a0} main │ "));
        // Any list of segments can be drawn in blocks.
        let blocks = layout("separator = \"powerline\"\nleft = [\"mode\", \"context\", \"bar\"]");
        assert_eq!(
            shown_in(&line, &blocks, 100, &Theme::dark()),
            sides(
                " ⏸ plan  ◫ 42% [━━━╸────] ",
                " ⬢ anthropic/claude-opus-5-5  priority  high ",
                100
            )
        );
    }

    #[test]
    fn the_output_of_a_command_stands_in_for_the_left_side() {
        let theme = Theme::dark();
        let line = full(&METERED);
        let row = line.render(&layout(""), Some("\x1b[32mmain\x1b[0m ~/src"), 80, &theme);
        // Its colours end with it.
        assert!(row.starts_with("\x1b[32mmain\x1b[0m ~/src\x1b[0m  "), "{row:?}");
        assert_eq!(
            text::strip_ansi(&row),
            sides("main ~/src", "⬢ anthropic/claude-opus-5-5 · priority · high", 80)
        );
        // The right side gives way to it, down to the model.
        let row = text::strip_ansi(&line.render(&layout(""), Some("a line of some length"), 50, &theme));
        assert_eq!(
            row,
            sides("a line of some length", "⬢ anthropic/claude-opus-5-5", 50)
        );
        for width in 0..=120 {
            let row = line.render(&layout(""), Some("\x1b[32ma line of some length"), width, &theme);
            assert!(text::width(&row) <= width, "{width}: {row:?}");
        }
    }

    #[test]
    fn what_extensions_show_is_on_the_line_and_leaves_it_before_the_model() {
        let theme = Theme::dark();
        let line = StatusLine {
            rate: None,
            cost: None,
            agents: 0,
            goal: None,
            tier: None,
            // A script decides what the text is: one line of it is shown, and nothing of it
            // reaches the terminal as an escape sequence.
            extensions: vec![
                "ci passing".to_string(),
                "\x1b[2J3 todos\nand a second line".to_string(),
                "\x1b[31m\t".to_string(),
            ],
            ..full(&METERED)
        };
        assert_eq!(
            shown(&line, 120, &theme),
            sides(
                "⏸ plan · ⑂ main · ~/src/app · ◫ 42% [━━━╸────] · ci passing · 3 todos",
                "⬢ anthropic/claude-opus-5-5 · high",
                120
            )
        );
        assert!(!painted(&line, 120, &theme).contains("\x1b[2J"));
        // The texts stand where the layout puts the segment, with its separator between them.
        let right = layout("separator = \"bar\"\nleft = [\"mode\"]\nright = [\"extensions\", \"model\"]");
        assert_eq!(
            shown_in(&line, &right, 80, &theme),
            sides("⏸ plan", "ci passing │ 3 todos │ ⬢ anthropic/claude-opus-5-5", 80)
        );
        assert!(!shown_in(&line, &layout("preset = \"minimal\""), 120, &theme).contains("ci passing"));
        // A long text is cut, to `max` columns where that is set.
        let cut = layout("[segments.extensions]\nmax = 6");
        assert!(shown_in(&line, &cut, 120, &theme).contains(" · ci pa… · 3 tod…  "));
        let long = StatusLine {
            extensions: vec!["x".repeat(80)],
            ..full(&METERED)
        };
        let row = shown(&long, 200, &theme);
        assert!(row.contains(&format!(" · {}… ", "x".repeat(31))), "{row:?}");

        // On a narrow line the texts go before the context, the path, the branch and the
        // model do, the last of them first.
        let mut gone = None;
        for width in (20..=120).rev() {
            let row = shown(&line, width, &theme);
            assert!(text::width(&row) <= width, "width {width}: {row:?}");
            if row.contains("ci passing") {
                assert!(
                    gone.is_none(),
                    "width {width} shows more than a wider line: {row:?}"
                );
                assert!(
                    row.contains("⑂ main · ~/src/app · ◫ 42%") && row.contains("⬢ anthropic/claude-opus-5-5"),
                    "width {width}: {row:?}"
                );
            } else {
                assert!(!row.contains("3 todos"), "width {width}: {row:?}");
                gone.get_or_insert(width);
            }
        }
        let gone = gone.unwrap();
        assert_eq!(
            shown(&line, gone, &theme),
            sides(
                "⏸ plan · ⑂ main · ~/src/app · ◫ 42%",
                "⬢ anthropic/claude-opus-5-5 · high",
                gone
            )
        );
    }

    #[test]
    fn the_home_directory_is_a_tilde() {
        let home = std::path::Path::new("/home/ann");
        let at = |path: &str| display_path(std::path::Path::new(path), Some(home));
        assert_eq!(at("/home/ann"), "~");
        assert_eq!(at("/home/ann/src/app"), "~/src/app");
        assert_eq!(
            at("/srv/app"),
            std::path::Path::new("/srv/app").display().to_string()
        );
        assert_eq!(display_path(home, None), home.display().to_string());
    }
}
