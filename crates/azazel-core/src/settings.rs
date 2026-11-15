//! The settings table: every configuration key as the settings screen shows it, with the page
//! it is on, how it is edited and one sentence that says what it does.
//!
//! The table sits beside the schema in [`crate::config`] so that the two can be compared: every
//! leaf of `Config::default()` has a row in [`SETTINGS`] or a line in [`NOT_EDITABLE`] that says
//! why it has none. A row carries no default of its own: [`Setting::default_value`] reads it
//! from `Config::default()`, so it cannot drift from the schema.

use std::sync::LazyLock;

use toml::{Table, Value};

use crate::config::{
    AdvisorWhen, Autocomplete, CacheWarming, CheckReport, Config, DesktopNotify, DestructivePolicy, DiffView,
    EditorMode, GlyphSetting, ImageMode, Indicator, LspInstall, PatchMode, PermissionMode, QueueMode,
    ReadGuard, SessionResume, StatusPreset, StatusSeparator, TelemetryExporter, ToolStyle, WriteSeverity,
};

/// A page of the settings screen.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Page {
    Accounts,
    Models,
    Appearance,
    Input,
    Permissions,
    Tools,
    Sessions,
    Usage,
    Advanced,
}

impl Page {
    /// The pages in the order of their tabs.
    pub const ALL: [Page; 9] = [
        Page::Accounts,
        Page::Models,
        Page::Appearance,
        Page::Input,
        Page::Permissions,
        Page::Tools,
        Page::Sessions,
        Page::Usage,
        Page::Advanced,
    ];

    /// The name on the tab.
    pub fn title(self) -> &'static str {
        match self {
            Page::Accounts => "Accounts",
            Page::Models => "Models",
            Page::Appearance => "Appearance",
            Page::Input => "Input",
            Page::Permissions => "Permissions",
            Page::Tools => "Tools",
            Page::Sessions => "Sessions",
            Page::Usage => "Usage",
            Page::Advanced => "Advanced",
        }
    }

    /// One sentence about what the page holds.
    pub fn lead(self) -> &'static str {
        match self {
            Page::Accounts => "Subscriptions, API keys and the order accounts are tried in.",
            Page::Models => "Which model does what, and how requests to models are retried and cached.",
            Page::Appearance => "Colours, symbols, the working indicator, notifications and the status line.",
            Page::Input => "The input: its keys, the external editor, queued messages and voice.",
            Page::Permissions => "What agents may do without asking, and the guards that watch them.",
            Page::Tools => "Tools, checks after a write, language servers, MCP, web, media and verification.",
            Page::Sessions => "Sessions, checkpoints, compaction, memory and the context files agents read.",
            Page::Usage => "Subscription usage and spending limits.",
            Page::Advanced => "Team size, schedules, updates, trace export and what only the file can say.",
        }
    }
}

/// How a setting is edited.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Kind {
    /// On or off.
    Switch,
    /// One of these values.
    Choice(&'static [&'static str]),
    /// One of the themes there are: the built-in ones and the files in `~/.azazel/themes`.
    Theme,
    /// A number that is not negative: whole, or with a fraction when the default has one.
    Number,
    Text,
    /// A list of texts.
    List,
    /// A table of names to values (`rs = "rustfmt"`); a value is any TOML value.
    Table,
    /// The chain of a model role, chosen with the model picker.
    Role,
}

/// One configuration key on the settings screen.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Setting {
    /// Where it lives in the config file, as `table.key`.
    pub path: &'static str,
    pub page: Page,
    /// The heading it stands under on its page.
    pub group: &'static str,
    pub label: &'static str,
    pub kind: Kind,
    /// The running UI takes a new value at once; the others apply to the next session.
    pub live: bool,
    /// One sentence about what it does.
    pub description: &'static str,
}

impl Setting {
    /// What the key is when no file sets it; `None` for a key that has no default (the
    /// `default` role).
    pub fn default_value(&self) -> Option<Value> {
        lookup(defaults(), self.path).cloned()
    }
}

static DEFAULTS: LazyLock<Table> = LazyLock::new(|| Table::try_from(Config::default()).unwrap_or_default());

/// `Config::default()` as a table.
pub fn defaults() -> &'static Table {
    &DEFAULTS
}

/// The value at `table.key` in a configuration table.
pub fn lookup<'a>(table: &'a Table, path: &str) -> Option<&'a Value> {
    let mut parts = path.split('.');
    let mut value = table.get(parts.next()?)?;
    for part in parts {
        value = value.get(part)?;
    }
    Some(value)
}

/// The paths of every leaf of a configuration table, as `table.key`: each value that is not a
/// table, and each table that is empty.
pub fn leaves(table: &Table) -> Vec<String> {
    fn walk(table: &Table, prefix: &str, out: &mut Vec<String>) {
        for (key, value) in table {
            let path = match prefix.is_empty() {
                true => key.clone(),
                false => format!("{prefix}.{key}"),
            };
            match value {
                Value::Table(inner) if !inner.is_empty() => walk(inner, &path, out),
                _ => out.push(path),
            }
        }
    }
    let mut out = Vec::new();
    walk(table, "", &mut out);
    out
}

/// The row of a configuration key.
pub fn setting(path: &str) -> Option<&'static Setting> {
    SETTINGS.iter().find(|setting| setting.path == path)
}

/// The search backends `web.search` takes.
const WEB_SEARCH: &[&str] = &[
    "auto",
    "provider",
    "brave",
    "tavily",
    "exa",
    "searxng",
    "duckduckgo",
];

macro_rules! settings {
    ($( $page:ident / $group:literal {
        $( $path:literal : $kind:expr, $label:literal, $description:literal $(, $live:ident)? ; )*
    } )*) => {
        /// Every setting the settings screen can change, page by page and group by group, in the
        /// order they are shown.
        pub const SETTINGS: &[Setting] = &[
            $( $( Setting {
                path: $path,
                page: Page::$page,
                group: $group,
                label: $label,
                kind: $kind,
                live: settings!(@live $($live)?),
                description: $description,
            }, )* )*
        ];
    };
    (@live) => { false };
    (@live live) => { true };
}

use Kind::{Choice, List, Number, Role, Switch, Text, Theme};

settings! {
    Accounts / "Rotation" {
        "accounts.order": Kind::Table, "Account order",
            "Per provider, the labels of its accounts in the order they are tried, such as anthropic = [\"work\", \"default\"].";
    }
    Models / "Roles" {
        "roles.default": Role, "default", "The model for everything that has no model of its own.";
        "roles.lead": Role, "lead", "The agent you talk to.";
        "roles.scout": Role, "scout", "Fast read-only exploration.";
        "roles.planner": Role, "planner", "Writes implementation plans.";
        "roles.coder": Role, "coder", "Implements changes.";
        "roles.tester": Role, "tester", "Writes and runs tests.";
        "roles.reviewer": Role, "reviewer", "Reviews diffs.";
        "roles.verifier": Role, "verifier", "Checks that a goal is really met.";
        "roles.researcher": Role, "researcher", "Web research.";
        "roles.designer": Role, "designer", "User interface work.";
        "roles.artist": Role, "artist", "Images, video, 3D and audio.";
        "roles.playtester": Role, "playtester", "Plays builds and reports defects.";
        "roles.smol": Role, "smol", "Small, quick jobs: summaries, session names, memory.";
        "roles.slow": Role, "slow", "The most capable model, for hard reviews.";
        "roles.plan": Role, "plan", "Planning.";
        "roles.verify": Role, "verify", "Verification.";
        "roles.vision": Role, "vision", "Work that needs to see images.";
        "roles.judge": Role, "judge", "Quick judgements: `classify` in scripts.";
        "roles.memory": Role, "memory", "Extracting memories.";
    }
    Models / "Switching and advice" {
        "tui.favourites": List, "Favourite models",
            "Models and @roles ctrl+p steps through, such as @default or anthropic/claude-x; empty steps through the roles that have models.", live;
        "advisor.when": Choice(AdvisorWhen::NAMES), "Second-model advisor",
            "stop: a second model reviews the lead's work each time it is about to finish; off leaves it to /advise.";
        "advisor.model": List, "Advisor model",
            "The chain of the second model: the first that answers is used; empty means no advisor.";
    }
    Models / "Agents" {
        "agents.default": Text, "Lead agent",
            "The primary agent the lead is when a session starts: build, plan or one of your own (tab switches).";
        "agents": Kind::Table, "Agent overrides",
            "Per agent, a model chain and a thinking level that replace its definition's, such as scout = { model = [\"@smol\"], thinking = \"low\" }.";
    }
    Models / "Requests" {
        "retry.max_attempts": Number, "Tries per model", "Tries per model before the router moves down the chain.";
        "retry.cooldown_secs": Number, "Cooldown after a failure", "Seconds a model that failed is skipped.";
        "retry.first_event_secs": Number, "Wait for the first sign",
            "Seconds to wait for the first sign of a response; 0 waits forever.";
        "retry.stall_secs": Number, "Longest silence", "Seconds a response that has begun may send nothing; 0 waits forever.";
        "retry.max_stalls": Number, "Stalls before falling back",
            "Stalls of one model within a request before the router moves down the chain; 0 asks the stalled model again.";
        "cache.warming": Choice(CacheWarming::NAMES), "Keep the prompt cache warm",
            "idle keeps the provider's prompt cache warm while the session waits for you.";
        "cache.warm_minutes": Number, "Warm for", "Minutes after the lead's last request the cache is kept warm.";
        "cache.report_misses": Switch, "Report cache misses",
            "Say in the transcript when a request misses the cached prefix, and why.";
    }
    Appearance / "Interface" {
        "tui.theme": Theme, "Theme",
            "The colours: a built-in theme, a file in ~/.azazel/themes, or auto (follows the terminal).", live;
        "tui.glyphs": Choice(GlyphSetting::NAMES), "Symbols",
            "The symbols the UI draws with: unicode, nerd (needs a Nerd Font) or ascii; auto is unicode unless the locale is not UTF-8.", live;
        "tui.indicator": Choice(Indicator::NAMES), "Working indicator", "How the working indicator moves.", live;
        "tui.indicator_text": List, "Indicator phrases",
            "Phrases the working indicator shows in turn; empty shows what the agent is doing.";
        "tui.session_accent": Switch, "Session colour in the indicator",
            "Tint the working indicator with a colour derived from the session's name.", live;
        "tui.todo_max": Number, "Todo rows above the input",
            "Rows the todo list above the input takes at most; 0 hides it.";
        "tui.tool_style": Choice(ToolStyle::NAMES), "Tool calls",
            "card frames each tool call; compact shows a finished call on one row.", live;
        "tui.diff_view": Choice(DiffView::NAMES), "Diff layout",
            "How /diff shows a file: split (old beside new), unified (one column), or auto (split from 140 columns on).";
        "tui.redact": Switch, "Hide secrets in what is shown",
            "Hide what looks like a secret (API keys, tokens, private keys) in what the UI shows; the model still sees it.", live;
        "tui.popups": Switch, "Menus and questions as popups",
            "Menus, pickers and questions open as a box in the middle of the window.", live;
        "tui.mouse": Switch, "Mouse in popups",
            "Click, hover and scroll in popups; the terminal's own selection works again once a popup closes.", live;
        "tui.images": Choice(ImageMode::NAMES), "Inline images",
            "auto shows images where the terminal can; off never does; kitty, iterm and sixel force a protocol.";
        "tui.show_thinking": Switch, "Show the model's thinking", "Show the model's thinking as it is written.", live;
        "tui.expand_tools": Switch, "Show tool output in full", "Show the whole output of tool calls instead of a summary.", live;
    }
    Appearance / "Notifications" {
        "notify.bell": Switch, "Ring the terminal bell",
            "Ring the bell when an agent needs you or a long run ends while the terminal is not focused.";
        "notify.desktop": Choice(DesktopNotify::NAMES), "Desktop notifications",
            "A desktop notification at the same moments: through the terminal, through the system, or off.";
        "notify.title": Switch, "Session in the terminal title", "Show the session and its state in the terminal's title.";
        "notify.progress": Switch, "Progress in the terminal tab", "Show working, waiting and failed in the terminal's tab.";
        "notify.min_secs": Number, "Shortest run that notifies",
            "A finished run notifies only when it took at least this many seconds.";
        "notify.keep_awake": Switch, "Keep the computer awake", "Keep the machine from sleeping while agents work.";
    }
    Appearance / "Status line" {
        "statusline.preset": Choice(StatusPreset::NAMES), "Preset",
            "The set of segments shown when the left and right lists are empty; /statusline sets the line up with it in view.", live;
        "statusline.left": List, "Left segments",
            "Segment names on the left, in order (mode, branch, path, context, ...); empty takes the preset's, none leaves the side empty.", live;
        "statusline.right": List, "Right segments",
            "Segment names on the right, in order (model, thinking, time, ...); empty takes the preset's, none leaves the side empty.", live;
        "statusline.separator": Choice(StatusSeparator::NAMES), "Separator",
            "What stands between two segments; powerline makes each segment a block of colour.", live;
        "statusline.segments": Kind::Table, "Segment options",
            "Options of each segment by its name, such as path = { style = \"basename\" } or time = { seconds = true }.", live;
        "statusline.command": Text, "Command",
            "A program given the session as JSON whose first line of output replaces the left side of the status line; empty runs nothing.", live;
    }
    Input / "Editor" {
        "editor.mode": Choice(EditorMode::NAMES), "Keys in the input", "emacs or vim keys in the input.", live;
        "editor.command": Text, "External editor",
            "The program that opens a draft; empty uses $VISUAL, then $EDITOR, then notepad or vi.", live;
        "editor.fold_paste_lines": Number, "Fold long pastes",
            "Pastes longer than this many lines fold into one token; 0 never folds.", live;
        "tui.complete_ignored": Switch, "Ignored files in @",
            "Offer files that .gitignore leaves out when you type @ (target/, node_modules/ and .git/ are still skipped).", live;
        "tui.queue_mode": Choice(QueueMode::NAMES), "Queued messages",
            "How messages queued while the agent works are delivered: one per turn, or all at once.", live;
        "tui.autocomplete": Choice(Autocomplete::NAMES), "Word completion",
            "words shows the rest of the word you are typing as ghost text, from what you wrote before; tab or right accepts it. off shows none.", live;
        "tui.predict_next": Switch, "Predict the next message",
            "After a finished turn, ask the model what you will say next and show it as ghost text in the empty input; tab or right accepts it. Each finished turn costs one more request.", live;
    }
    Input / "Voice" {
        "voice.stt": Text, "Transcription model", "The model that transcribes, as provider/model.";
        "voice.tts": Text, "Speech model", "The model that speaks, as provider/model.";
        "voice.voice": Text, "Voice", "The name of the voice for speech.";
        "voice.language": Text, "Language", "The language of what is dictated, as a code such as en; empty lets the service detect it.";
        "voice.auto_send": Switch, "Send at once", "Send what was dictated as a prompt at once.";
        "voice.max_secs": Number, "Longest recording", "The longest recording, in seconds.";
        "voice.record_command": Text, "Record command", "A program that records instead of the built-in way.";
        "voice.stt_command": Text, "Transcribe command", "A program that transcribes instead of the model.";
        "voice.play_command": Text, "Play command", "A program that plays speech instead of the built-in way.";
    }
    Permissions / "Permissions" {
        "permissions.mode": Choice(PermissionMode::NAMES), "Mode a session starts in",
            "How much is asked before a tool call runs: plan, manual, accept-edits, auto or bypass.";
        "permissions.destructive": Choice(DestructivePolicy::NAMES), "Commands that destroy work",
            "What a command that destroys work (git reset --hard, rm -r ~) needs in every mode but bypass.";
        "permissions.allow": List, "Allow rules", "Rules for calls that run without asking, such as shell(cargo build:*).";
        "permissions.ask": List, "Ask rules", "Rules for calls that are always asked about.";
        "permissions.deny": List, "Deny rules", "Rules for calls that never run.";
        "permissions.additional_dirs": List, "Additional directories",
            "Directories outside the project that agents may work in.";
        "permissions.secret_paths": List, "Secret files",
            "Patterns added to the built-in list of secret files: reading or writing one is asked about.";
    }
    Permissions / "Guards" {
        "guards.done_check": Switch, "Check before done",
            "Send the lead back once when it stops on changed code and ran no build or test since.";
        "guards.stuck_after": Number, "Stuck after",
            "Add a note to a shell command that failed this many times in a row in a turn; 0 is off.";
        "guards.runaway": Switch, "Stop runaway replies",
            "Stop a reply that repeats one block of text three times in a row, and ask again.";
    }
    Tools / "Tools" {
        "tools.disabled": List, "Disabled tools", "Tool names withheld from every agent.";
        "tools.max_result_bytes": Number, "Largest tool result", "Tool output beyond this many bytes is cut and saved as an artifact.";
        "tools.shell_timeout_secs": Number, "Shell time limit", "The default time limit of a shell call, in seconds.";
        "tools.shell": Text, "Shell", "The shell program; empty finds one (bash, sh; on Windows Git's bash, pwsh, powershell).";
        "tools.patch": Choice(PatchMode::NAMES), "Patch tool",
            "Who gets the patch tool: auto gives it instead of edit to OpenAI GPT and Codex models, on to every model.";
        "tools.redact_output": Switch, "Redact secrets for the model",
            "Replace secrets in tool results with [redacted:<kind>] before the model and the session file get them.";
    }
    Tools / "After a write" {
        "tools.format_on_write": Switch, "Format on write",
            "Run the file's formatter after a tool changes it: the configured one, else one the project uses.";
        "tools.format": Kind::Table, "Formatters",
            "File extension (without the dot) to formatter command, such as rs = rustfmt --edition 2024; empty turns it off.";
        "tools.check_on_write": Switch, "Check on write", "Run the file's checkers (linters) after write and edit, when they are installed.";
        "tools.check": Kind::Table, "Checkers",
            "File extension to checker command, replacing the built-in one; {file} is the file's path.";
        "tools.check_report": Choice(CheckReport::NAMES), "Findings report",
            "What the lead is told about findings when it stops: those on lines it changed (delta), all, or nothing.";
        "tools.read_guard": Choice(ReadGuard::NAMES), "Read before edit",
            "What a read must cover before an edit: the lines the edit changes (range), any read of the file, or nothing.";
        "tools.scan.secrets": Switch, "Scan for committed secrets",
            "Look for secrets committed to the project with gitleaks, once per session.";
        "tools.eval.python": Switch, "Python in eval",
            "Let agents run Python programs with the eval tool. Off for both languages, eval is not offered.";
        "tools.eval.javascript": Switch, "JavaScript in eval",
            "Let agents run JavaScript programs (node, bun or deno) with the eval tool.";
    }
    Tools / "Language servers" {
        "lsp.servers": Kind::Table, "Servers",
            "Language servers by name, such as rust = { command = \"rust-analyzer\", extensions = [\"rs\"] }.";
        "lsp.idle_secs": Number, "Stop when idle",
            "Stop a server after this many seconds in which none of its files was touched; 0 keeps it running.";
        "lsp.write_severity": Choice(WriteSeverity::NAMES), "Reported after a write", "The lowest severity reported after a write.";
        "lsp.install": Choice(LspInstall::NAMES), "Install servers",
            "When a file has a known language server that is not installed: ask before installing it into ~/.azazel/lsp, install it without asking, or never.";
    }
    Tools / "Debugger" {
        "debug.adapters": Kind::Table, "Adapters",
            "Debug adapter by language, such as python = { command = \"python\", args = [\"-m\", \"debugpy.adapter\"] }; the one on PATH is used otherwise.";
        "debug.timeout_secs": Number, "Wait for a stop",
            "Seconds a debug call waits for the program to stop or end.";
    }
    Tools / "MCP" {
        "mcp.startup_wait_ms": Number, "Startup wait", "Milliseconds the first request waits for MCP servers.";
        "mcp.defer_threshold": Number, "Defer above", "Above this many MCP tools, they are found through tool_search instead.";
        "mcp.idle_timeout_secs": Number, "Idle timeout",
            "A server started on demand stops after this many seconds without a call; 0 never stops it.";
    }
    Tools / "Web and browser" {
        "web.search": Choice(WEB_SEARCH), "Search backend", "Where web_search looks: auto picks the first that is set up.";
        "web.searxng_url": Text, "SearXNG address", "The address of a SearXNG instance.";
        "web.provider_search": Switch, "Provider's own search",
            "Under auto, search through the lead model's provider first when it has a search tool.";
        "browser.path": Text, "Browser program", "The browser binary; empty looks for Chrome, Chromium, Edge or Brave.";
        "browser.headless": Switch, "Headless browser", "Run the browser without a window.";
    }
    Tools / "Media" {
        "media.output_dir": Text, "Output directory", "Where generated files go, relative to the project.";
        "media.models.image": Text, "Image model", "The model for images, as backend:model id.";
        "media.models.video": Text, "Video model", "The model for video, as backend:model id.";
        "media.models.model3d": Text, "3D model", "The model for 3D, as backend:model id.";
        "media.models.audio": Text, "Audio model", "The model for audio, as backend:model id.";
    }
    Tools / "Verification" {
        "verify.commands": List, "Check commands", "The commands that verify a goal; empty detects them.";
        "verify.agent": Switch, "Verifier agent", "Run the verifier agent after the commands pass.";
        "verify.timeout_secs": Number, "Time limit per command", "The time limit of each check command, in seconds.";
        "verify.on_stop": Switch, "Verify ordinary prompts", "Also run the commands when an ordinary prompt changed something.";
        "goal.max_iterations": Number, "Goal attempts", "Failed verifications after which a goal fails.";
        "goal.max_nudges": Number, "Goal reminders", "Idle reminders before a goal is paused.";
    }
    Sessions / "Sessions" {
        "session.auto_name": Switch, "Name sessions automatically", "Have a small model name a session after its first reply.";
        "session.resume": Choice(SessionResume::NAMES), "Open at start",
            "new starts a new session; last opens this project's most recent one, as -c does.";
        "session.restore_model": Switch, "Restore the model on resume",
            "A resumed session starts on the model and thinking level it last used.";
        "session.retention_days": Number, "Retention",
            "Sessions not used for this many days are deleted at start; 0 keeps everything.";
        "session.resume_runs": Switch, "Continue interrupted runs",
            "A run cut off by a crash or a closed terminal continues when the session is opened again.";
        "session.branch_summary": Switch, "Summarise a branch you leave",
            "Going back with /tree has a small model tell the agent what happened on the branch you left.";
    }
    Sessions / "Checkpoints" {
        "checkpoints.enabled": Switch, "Checkpoints", "Snapshot the project before each prompt, so /rewind can bring it back.";
        "checkpoints.max_files": Number, "Largest project", "Checkpoints turn off for projects with more files than this.";
        "checkpoints.timeout_ms": Number, "Snapshot time limit",
            "Longest one snapshot may take, in milliseconds (1000 to 300000); past it checkpoints turn off for the session.";
        "checkpoints.exclude": List, "Left out", "Patterns, as in .gitignore, that checkpoints leave out.";
    }
    Sessions / "Compaction" {
        "compaction.enabled": Switch, "Compaction", "Summarise the conversation when the context window fills up.";
        "compaction.reserve_tokens": Number, "Reserve", "Summarise when the context is within this many tokens of the window.";
        "compaction.keep_recent_tokens": Number, "Kept verbatim", "Roughly how many tokens of recent context are kept as they are.";
        "compaction.keep_tool_results": Number, "Tool results kept", "The newest tool results that are never elided.";
        "compaction.micro_threshold": Number, "Elide old results at",
            "The fraction of the window (0 to 1) at which old tool results are elided.";
    }
    Sessions / "Memory" {
        "memory.enabled": Switch, "Memory", "Project memory and its tools.";
        "memory.auto_extract": Switch, "Save memories from conversations", "Extract memories at session end and after compaction.";
        "memory.auto_index": Switch, "Index the project", "Refresh the code index in the background.";
    }
    Sessions / "Context and rules" {
        "context.nested": Switch, "Nested context files",
            "Read the AGENTS.md of every directory an agent works in, not only the root one.";
        "context.include_depth": Number, "Include depth", "How deep @path includes inside context files are followed; 0 follows none.";
        "context.repo_map_tokens": Number, "Repository map size", "Tokens of the repository map put in the lead's system prompt; 0 leaves it out.";
        "rules.compat": Switch, "Rules of other tools", "Also read the rule files of Cursor, Windsurf, Copilot, Gemini and Cline.";
        "compat.claude": Switch, "Claude Code files", "Also read CLAUDE.md and the skills, agents and commands under .claude/.";
    }
    Usage / "Subscription usage" {
        "usage.auto_reset": Switch, "Spend resets unattended",
            "Spend a banked usage reset without asking when nobody is there to ask.";
        "usage.poll_secs": Number, "Fetch usage every",
            "Seconds between two fetches of the usage of subscription accounts while a session is open; 0 never.";
        "usage.redact_email": Switch, "Shorten e-mail addresses", "Show e-mail addresses as j***@example.com.";
    }
    Usage / "Spending limits" {
        "limits.max_cost_usd": Number, "Cost cap", "Spend in USD at which a run stops; 0 is no cap.";
        "limits.warn_cost_usd": Number, "Cost warning", "Spend in USD at which a warning is shown once; 0 is no warning.";
        "limits.max_turns": Number, "Turn cap", "Requests per prompt for the lead; 0 is no limit.";
    }
    Advanced / "Team" {
        "team.coordinator": Switch, "Coordinator mode",
            "The lead only plans and delegates: it is offered no tool that edits or runs commands.";
        "team.max_parallel": Number, "Agents at once", "Sub-agents working at the same time.";
        "team.max_depth": Number, "Spawn depth", "How deep agents may spawn agents.";
    }
    Advanced / "Schedules and loops" {
        "schedule.register": Switch, "Use the system scheduler",
            "Hand schedules to Task Scheduler, cron or launchd; off, they run only when `azazel schedule run` is called.";
        "schedule.poll_secs": Number, "CI poll", "Seconds between two looks at the CI runs of a ci-failure trigger (10 at least).";
        "schedule.loop_max": Number, "Loop rounds", "Rounds after which /loop stops by itself; 0 is no limit.";
    }
    Advanced / "Triggers" {
        "triggers.handle": Text, "Pull-request handle", "The handle a pull-request comment has to mention to start a run (on pr-comment).";
        "triggers.mode": Choice(PermissionMode::NAMES), "Mode of triggered runs",
            "The permission mode of runs started by a comment or a message; bypass counts as auto.";
        "triggers.github_authors": List, "GitHub users who may start runs",
            "Empty: the user gh is signed in as and users with write access.";
        "triggers.slack_users": List, "Slack users who may start runs", "Slack user ids; empty means nobody.";
        "triggers.slack_url": Text, "Slack API address", "Where the Slack bot token is sent.";
    }
    Advanced / "Updates" {
        "update.check": Switch, "Look for updates", "Look for a newer release at most once a day, in the background.";
        "update.auto": Switch, "Install updates", "Download a newer release in the background and install it when the UI exits.";
    }
    Advanced / "Trace export" {
        "telemetry.exporter": Choice(TelemetryExporter::NAMES), "Exporter", "Where traces of what a session does are sent.";
        "telemetry.endpoint": Text, "Endpoint", "The collector's address; empty uses the exporter's default.";
        "telemetry.project": Text, "Project", "The project name shown by the backend.";
        "telemetry.api_key_env": Text, "Key variable", "The environment variable that holds the backend's key.";
        "telemetry.content": Switch, "Send text", "Send prompt and reply text, not only the shape of each step.";
    }
}

/// Configuration keys the settings screen shows but does not edit, each with why: their values
/// are lists of tables or tables of tables with members of their own.
pub const NOT_EDITABLE: &[(&str, &str)] = &[
    (
        "providers",
        "Endpoints, headers and keys of providers: sign in or add a key on the Accounts page; a new \
         provider is written as [providers.<id>] in the config file or in models.yml.",
    ),
    (
        "models",
        "Limits, capabilities and prices per model: written as [models.\"<provider>/<id>\"] in the \
         config file or in models.yml.",
    ),
    (
        "presets",
        "Saved sets of role chains: `/preset save <name>` writes one, `/preset <name>` applies it.",
    ),
    (
        "hooks",
        "Programs run on events, each a table of its own: written as [[hooks.<event>]] in the config file.",
    ),
    (
        "stream_rules",
        "Patterns that stop a reply as it is written, each a table of its own: written as \
         [[stream_rules]] in the config file.",
    ),
    (
        "embeddings",
        "The embedding endpoint needs its provider and its model together: written as [embeddings] in \
         the config file.",
    ),
];

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;
    use crate::config::TABLES;

    /// Whether the configuration accepts `value` at `path`.
    fn accepted(path: &str, value: Value) -> Result<(), String> {
        let mut nested = value;
        for part in path.rsplit('.') {
            let mut table = Table::new();
            table.insert(part.to_string(), nested);
            nested = Value::Table(table);
        }
        nested
            .try_into::<Config>()
            .map(|_| ())
            .map_err(|e| e.message().to_string())
    }

    #[test]
    fn every_leaf_of_the_default_configuration_has_a_row_or_a_reason_for_having_none() {
        let rows: BTreeSet<&str> = SETTINGS.iter().map(|setting| setting.path).collect();
        assert_eq!(rows.len(), SETTINGS.len(), "a path has two rows");
        let fixed: BTreeSet<&str> = NOT_EDITABLE.iter().map(|(path, _)| *path).collect();
        let leaves = leaves(defaults());
        assert!(leaves.len() > 120, "the walk found only {} keys", leaves.len());
        let missing: Vec<&String> = leaves
            .iter()
            .filter(|leaf| !rows.contains(leaf.as_str()) && !fixed.contains(leaf.as_str()))
            .collect();
        assert!(
            missing.is_empty(),
            "configuration keys with no row in SETTINGS and no line in NOT_EDITABLE: {missing:?}"
        );

        // The other way round: a row or a reason names a key the configuration has. The roles
        // are an open table, and `embeddings` is absent until it is set.
        let leaves: BTreeSet<&str> = leaves.iter().map(String::as_str).collect();
        for path in rows.iter().chain(&fixed) {
            // `agents` holds the overrides the user names beside its own `default`.
            let known = leaves.contains(path)
                || *path == "roles.default"
                || *path == "agents"
                || (*path == "embeddings" && TABLES.contains(path));
            assert!(known, "{path} is not a configuration key");
        }
        for (path, why) in NOT_EDITABLE {
            assert!(
                !rows.contains(path),
                "{path} has a row and a reason for having none"
            );
            assert!(why.ends_with('.') && why.len() > 40, "{path}: {why}");
        }
    }

    #[test]
    fn every_row_edits_a_value_of_its_kind() {
        for setting in SETTINGS {
            let default = setting.default_value();
            let fits = match (setting.kind, &default) {
                (Kind::Switch, Some(value)) => value.is_bool(),
                (Kind::Choice(options), Some(value)) => {
                    assert!(!options.is_empty(), "{}", setting.path);
                    for option in options {
                        accepted(setting.path, Value::String(option.to_string()))
                            .unwrap_or_else(|why| panic!("{} = {option}: {why}", setting.path));
                    }
                    // A word that is not among them is refused, unless the key takes any text.
                    let free = accepted(setting.path, Value::String("no such value".into())).is_ok();
                    assert_eq!(free, setting.path == "web.search", "{}", setting.path);
                    value.as_str().is_some_and(|now| options.contains(&now))
                }
                (Kind::Number, Some(value)) => value.is_integer() || value.is_float(),
                (Kind::Text, Some(value)) => value.is_str(),
                (Kind::Theme, Some(value)) => value.is_str(),
                (Kind::List, Some(value)) => value.is_array(),
                (Kind::Table, Some(value)) => value.is_table(),
                (Kind::Role, Some(value)) => value.is_array(),
                (Kind::Role, None) => setting.path == "roles.default",
                (_, None) => false,
            };
            assert!(fits, "{} is {default:?}, not a {:?}", setting.path, setting.kind);
        }
    }

    #[test]
    fn every_page_has_rows_and_every_row_says_what_it_does() {
        for page in Page::ALL {
            assert!(
                SETTINGS.iter().any(|setting| setting.page == page),
                "{} has no rows",
                page.title()
            );
            assert!(page.lead().ends_with('.'));
        }
        for setting in SETTINGS {
            assert!(!setting.label.is_empty() && !setting.group.is_empty());
            assert!(
                setting.description.ends_with('.') && setting.description.len() >= 8,
                "{}: {}",
                setting.path,
                setting.description
            );
        }
        // A group keeps its rows together: its name does not come back later on the page.
        let mut seen: Vec<(Page, &str)> = Vec::new();
        for setting in SETTINGS {
            let group = (setting.page, setting.group);
            if seen.last() != Some(&group) {
                assert!(!seen.contains(&group), "{group:?} is split");
                seen.push(group);
            }
        }
    }
}
