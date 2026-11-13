//! Slash commands and the pickers that go with them.

use std::io::Write;

use azazel_ai::{ModelSpec, Role as MessageRole, ThinkingLevel};
use azazel_core::{EntryKind, PermissionMode, SessionStore};
use azazel_tui::{Glyph, GlyphLevel, Glyphs, Role, SelectItem, Theme};

use super::backend::Go;
use super::overlay::{Action, Overlay, RewindPoint};
use super::{SHELL_NOTE, Tui, first_line, view};

/// Matching lines listed per session by `/search`.
pub(super) const SEARCH_LINES: usize = 3;

const BUILTIN: &[(&str, &str)] = &[
    ("help", "List commands"),
    (
        "model",
        "Pick the model, or set it: /model <name> [fallback …] (part of a name is enough)",
    ),
    ("thinking", "Pick the thinking level, or set it: /thinking high"),
    ("mode", "Pick the permission mode, or set it: /mode auto"),
    ("new", "Start a new session"),
    (
        "resume",
        "Reopen an earlier session of this project; its keys rename, delete, pin and preview",
    ),
    (
        "name",
        "Name this session: /name <text>, or /name alone to have one suggested",
    ),
    (
        "session",
        "What this session is: file, messages, tokens, cost, agents",
    ),
    (
        "search",
        "Find sessions of this project by what was said in them: /search <text>",
    ),
    (
        "tree",
        "Go back to an earlier point of this session; alt+l labels an entry",
    ),
    ("fork", "Continue from here in a copy of this session"),
    (
        "retry",
        "Send your last prompt again; the first attempt stays as a branch",
    ),
    (
        "continue",
        "Go on with the run that was cut short when azazel last ended in the middle of it",
    ),
    (
        "handoff",
        "Continue in a new session that starts from a brief of this one: /handoff [what matters]",
    ),
    (
        "consult",
        "Ask a saved session a question without resuming it: /consult [question]",
    ),
    (
        "compact",
        "Summarise the conversation so far: /compact [what to keep]",
    ),
    ("clear", "Clear the screen"),
    ("cost", "Tokens and money spent in this session"),
    ("context", "How full the context window is"),
    (
        "export",
        "Write the conversation to a file: /export [path.md|.html|.jsonl]",
    ),
    ("theme", "Pick the colour theme, or set it: /theme light"),
    (
        "glyphs",
        "Pick the symbols the UI draws with, or set them: /glyphs nerd|unicode|ascii|auto",
    ),
    ("keys", "List key bindings"),
    (
        "rewind",
        "Go back to before an earlier prompt: files, conversation or both",
    ),
    (
        "permissions",
        "The permission rules in effect and where they come from; d removes one, a adds one",
    ),
    (
        "forget-approvals",
        "Forget every remembered approval and every rule added in this session",
    ),
    (
        "trust",
        "Whether this project is trusted, and what that covers: /trust [yes|no]",
    ),
    (
        "transcript",
        "Read the whole conversation full screen, with search and copy",
    ),
    (
        "copy",
        "Copy the last reply: /copy, /copy code for a code block, /copy <n> for an earlier reply",
    ),
    ("vim", "Switch Vim keys in the input on or off"),
    (
        "tools",
        "List the tools offered to the agent, by where they come from",
    ),
    ("jobs", "Background jobs: read their output or stop them"),
    (
        "todo",
        "The agent's task list: /todo, /todo add <text>, /todo done <n>, /todo clear",
    ),
    (
        "reload",
        "Read skills, commands, agent definitions, hooks, key bindings, the theme and scripts again",
    ),
    (
        "add-dir",
        "Let agents work in another directory too: /add-dir <path>",
    ),
    (
        "changelog",
        "What changed in each release: /changelog [since version]",
    ),
    (
        "open",
        "Open the last link of the last reply: /open, /open <n>, /open <path>, /open diagram",
    ),
    ("dirs", "List the directories agents may work in"),
    (
        "rules",
        "List the rule files and whether each has applied in this session",
    ),
    (
        "hooks",
        "List the hooks, where they come from and how each last ran",
    ),
    (
        "btw",
        "Ask a question on the side while the agent works: /btw <question>",
    ),
    (
        "diff",
        "Show the uncommitted changes; /diff turn or /diff session for changes since a checkpoint",
    ),
    (
        "commit",
        "Commit with a drafted message you can edit: /commit [what to say]",
    ),
    ("init", "Have the agent write AGENTS.md for this project"),
    ("doctor", "Check the installation and this project's setup"),
    #[cfg(feature = "voice")]
    (
        "voice",
        "Dictate: start recording, or stop and put the words in the input",
    ),
    #[cfg(feature = "voice")]
    ("speak", "Read replies aloud: /speak on|off|stop"),
    ("quit", "Exit"),
];

/// How the note about a files-only rewind starts in the agent's context.
pub(crate) const REWIND_NOTE: &str = "I rewound the project files";

const INIT_PROMPT: &str = "Explore this repository and write AGENTS.md at the project root for the coding agents \
that will work here later. Use exactly these sections: Build, Test, Layout, Conventions. Read the build files, the \
CI configuration and a sample of the source before writing. Give exact commands. Keep it under 200 lines. If \
AGENTS.md already exists, correct and extend it rather than replacing what is still accurate.";

impl Tui {
    /// Every command as `(name, description)`: built-ins, then registered ones, then skills.
    pub(super) fn commands(&self) -> Vec<(String, String)> {
        let mut out: Vec<(String, String)> = BUILTIN
            .iter()
            .map(|(n, d)| (n.to_string(), d.to_string()))
            .collect();
        for (name, about) in self.backend.commands() {
            if !out.iter().any(|(known, _)| *known == name) {
                out.push((name, about));
            }
        }
        out
    }

    pub(super) async fn slash(&mut self, line: &str) {
        let (name, args) = match line.split_once(char::is_whitespace) {
            Some((name, args)) => (name, args.trim()),
            None => (line, ""),
        };
        let echo = view::user_prompt(&format!("/{line}"), self.width(), &self.theme);
        self.say(echo);
        // What the commands read of the session is as the session is now.
        self.sync().await;
        match name {
            "help" => self.help(),
            "model" if args.is_empty() => self.open_model_picker().await,
            "model" => {
                let specs: Vec<&str> = args.split_whitespace().collect();
                self.set_model(&specs).await;
            }
            "thinking" if args.is_empty() => {
                let items = ThinkingLevel::ALL
                    .iter()
                    .map(|level| SelectItem::new(level.to_string(), ""))
                    .collect();
                let body = vec![format!("current: {}", self.backend.status().thinking)];
                self.overlay = Some(Overlay::select("Thinking", body, items, false, Action::Thinking));
            }
            "thinking" => match args.parse::<ThinkingLevel>() {
                Ok(level) => self.set_thinking(level).await,
                Err(e) => self.fail(&e.to_string()),
            },
            "mode" if args.is_empty() => {
                let items = PermissionMode::ALL
                    .iter()
                    .map(|mode| SelectItem::new(mode.to_string(), mode.summary()))
                    .collect();
                let body = vec![format!("current: {}", self.backend.mode())];
                self.overlay = Some(Overlay::select(
                    "Permission mode",
                    body,
                    items,
                    false,
                    Action::Mode,
                ));
            }
            "mode" => match args.parse::<PermissionMode>() {
                Ok(mode) => self.set_mode(mode).await,
                Err(e) => self.fail(&e),
            },
            "new" => {
                if self.idle_to("start a new session") {
                    self.go(Go::New).await;
                }
            }
            "resume" => self.open_resume().await,
            "name" => self.name_command(args).await,
            "search" => self.search_command(args).await,
            "tree" => self.open_tree(),
            "fork" => self.fork().await,
            "retry" => self.retry().await,
            "continue" => self.continue_run().await,
            "handoff" => self.handoff(args),
            "consult" => self.open_consult(args).await,
            "compact" => {
                let instructions = (!args.is_empty()).then(|| args.to_string());
                self.backend.compact(instructions);
                self.notice = Some("compacting…".to_string());
            }
            "clear" => self.clear_screen(),
            "export" => match self.backend.export(args, self.env.config.tui.redact).await {
                Ok(file) => self.info(&format!("exported to {file}")),
                Err(why) => self.fail(&why),
            },
            "theme" if args.is_empty() => self.open_theme_picker(),
            "theme" => self.set_theme(args),
            "glyphs" if args.is_empty() => self.open_glyph_picker(),
            "glyphs" => match glyph_choice(args) {
                Some(level) => self.choose_glyphs(level),
                None => self.fail(&format!(
                    "unknown glyph set `{args}`: nerd, unicode, ascii or auto"
                )),
            },
            "keys" => self.show_keys(),
            "rewind" => self.open_rewind().await,
            "permissions" => match args.strip_prefix("add") {
                Some(rule) if !rule.trim().is_empty() => self.add_rule(rule),
                _ => self.open_permissions(),
            },
            "transcript" => self.open_transcript().await,
            "copy" => self.copy_command(args),
            "vim" => self.toggle_vim(),
            "jobs" => self.open_jobs(),
            "reload" => self.reload().await,
            "changelog" => match crate::changelog::text((!args.is_empty()).then_some(args)) {
                Ok(text) => {
                    let sources = vec![super::pager::Source::Markdown(text)];
                    super::pager::show(self, "Changelog", sources, super::pager::Start::Top).await;
                }
                Err(e) => self.fail(&e),
            },
            "open" => self.open_command(args),
            "btw" => self.btw_command(args),
            "diff" => self.diff_command(args).await,
            "commit" => self.commit_command(args).await,
            "init" => self.send_prompt(INIT_PROMPT.to_string()).await,
            "voice" => self.voice_toggle().await,
            "speak" => self.speak_command(args),
            "quit" | "exit" => self.quit = true,
            #[cfg(feature = "mcp")]
            "mcp" if args.is_empty() && self.backend.local().is_some() => self.open_mcp(),
            // What the session says about itself, then what extensions added.
            _ => match self.backend.report(name, args).await {
                Some(Ok(report)) => self.info(&report),
                Some(Err(why)) => self.fail(&why),
                None => self.other_command(name, args),
            },
        }
    }

    /// A skill (`/skill:<name>`) or a command registered by an extension. They run beside the
    /// UI loop (some wait on the network or on the user elsewhere); their outcome comes back
    /// through it.
    pub(super) fn other_command(&mut self, name: &str, args: &str) {
        self.backend.command(name, args, self.internal_tx.clone());
    }

    /// Sends a prompt, on `model` for this one prompt when it is given.
    pub(super) async fn prompt_on(&mut self, prompt: String, model: Option<Vec<ModelSpec>>) {
        if let Some(chain) = model {
            if self.busy {
                return self.fail("this command runs on its own model; wait until the agent is idle");
            }
            let usable = chain.iter().any(|spec| self.backend.resolves(&spec.to_string()));
            if usable {
                self.restore_chain = Some(self.backend.status().chain);
                self.backend.set_chain(chain);
            } else {
                self.warn("the command's model resolves to nothing; using the current one");
            }
        }
        self.send_prompt(prompt).await;
    }

    /// Whether the lead is idle; says what cannot be done otherwise.
    /// `/continue`: goes on with the run the end of a process cut short.
    async fn continue_run(&mut self) {
        if !self.idle_to("go on with an interrupted run") {
            return;
        }
        match self.backend.continue_run().await {
            Ok(()) => self.info("going on with the interrupted run"),
            Err(why) => self.fail(&why),
        }
    }

    pub(super) fn idle_to(&mut self, what: &str) -> bool {
        if self.busy {
            self.fail(&format!("the agent is working; press esc first to {what}"));
        }
        !self.busy
    }

    fn help(&mut self) {
        let commands = self.commands();
        let widest = commands.iter().map(|(name, _)| name.len()).max().unwrap_or(0);
        let mut out = String::new();
        for (name, about) in commands {
            out.push_str(&format!("/{name:<widest$}  {}\n", first_line(&about)));
        }
        out.push_str("\n@path attaches a file · !command runs in your shell · /keys lists key bindings");
        self.info(&out);
    }

    // ------------------------------------------------------------ pickers

    pub(super) async fn open_model_picker(&mut self) {
        let (specs, mut items): (Vec<String>, Vec<SelectItem>) = self
            .backend
            .models()
            .await
            .into_iter()
            .map(|(spec, detail)| (spec.clone(), SelectItem::new(spec, detail)))
            .unzip();
        // Favourites are marked; ctrl+f on a row adds it to them or takes it out.
        let star = self.theme.glyph(Glyph::Favourite);
        for (item, spec) in items.iter_mut().zip(&specs) {
            if self.favourites.contains(spec) {
                item.detail = format!("{star} {}", item.detail);
            }
        }
        let body = vec![format!("current: {}", self.model_label()), self.favourites_line()];
        self.overlay = Some(
            Overlay::select("Model", body, items, true, Action::Model(specs))
                .with_keys(super::panels::model_keys()),
        );
    }

    /// Servers to pick from; choosing one offers what can be done with it.
    #[cfg(feature = "mcp")]
    fn open_mcp(&mut self) {
        let manager = self
            .backend
            .local()
            .and_then(|local| local.app.registry.services.get::<azazel_mcp::McpManager>());
        let Some(manager) = manager else {
            return self.info("MCP is not available in this session");
        };
        let servers = manager.status();
        if servers.is_empty() {
            return self.info(&azazel_mcp::status_report(&manager));
        }
        let items = servers
            .iter()
            .map(|server| {
                let detail = match server.state {
                    azazel_mcp::State::Ready => format!("ready · {} tools · {}", server.tools, server.target),
                    _ => format!("{} · {}", server.state_text(), server.target),
                };
                SelectItem::new(server.name.as_str(), detail)
            })
            .collect();
        let body = manager
            .problems()
            .iter()
            .map(|(what, why)| format!("{what} was not loaded: {why}"))
            .collect();
        let rows = servers
            .into_iter()
            .map(|server| (server.name, server.state != azazel_mcp::State::Disabled))
            .collect();
        self.overlay = Some(Overlay::select(
            "MCP servers",
            body,
            items,
            false,
            Action::McpServer(rows),
        ));
    }

    fn open_theme_picker(&mut self) {
        let names = crate::settings::theme_names(&self.env.paths.home);
        // Each row shows the colours it would bring: accent, success, warning, error.
        let dot = self.theme.glyph(Glyph::Bullet);
        let items = names
            .iter()
            .map(|name| {
                let (theme, problem) = super::named_theme(&self.env.paths, name);
                match problem {
                    Some(_) => SelectItem::new(name.as_str(), "cannot be read"),
                    None => SelectItem::with_preview(
                        name.as_str(),
                        swatches(&theme.with_depth(self.theme.depth()), dot),
                    ),
                }
            })
            .collect();
        let current = names.iter().position(|name| *name == self.theme_name);
        let body = vec![format!("current: {}", self.theme_name)];
        self.overlay = Some(
            Overlay::select("Theme", body, items, false, Action::Theme(names))
                .starting_at(current.unwrap_or(0)),
        );
    }

    /// `/glyphs`: the symbol sets, each row drawn in its own symbols so the right one can be
    /// told by eye. A font without the symbols of a row shows boxes there.
    fn open_glyph_picker(&mut self) {
        let mut levels: Vec<Option<GlyphLevel>> = GlyphLevel::ALL.into_iter().map(Some).collect();
        levels.push(None);
        let items = levels
            .iter()
            .map(|level| match level {
                Some(level) => SelectItem::new(level.name(), glyph_preview(*level)),
                None => SelectItem::new("auto", "unicode, or ascii when the locale is not UTF-8"),
            })
            .collect();
        let setting = self.env.config.tui.glyphs.as_str();
        let mut body = vec![format!(
            "now: {} · a row that shows boxes needs a font this terminal does not have",
            self.theme.glyphs().level()
        )];
        if std::env::var("AZAZEL_GLYPHS").is_ok_and(|value| value.parse::<GlyphLevel>().is_ok()) {
            body.push("AZAZEL_GLYPHS is set and wins over the choice made here.".to_string());
        }
        let current = levels
            .iter()
            .position(|level| level.map_or("auto", GlyphLevel::name) == setting)
            .or_else(|| {
                let shown = self.theme.glyphs().level();
                levels.iter().position(|level| *level == Some(shown))
            });
        self.overlay = Some(
            Overlay::select("Glyphs", body, items, false, Action::Glyphs(levels))
                .starting_at(current.unwrap_or(0)),
        );
    }

    /// Draws with another symbol set from now on and keeps the choice as `tui.glyphs`. `None`
    /// is `auto`.
    pub(super) fn choose_glyphs(&mut self, level: Option<GlyphLevel>) {
        let name = level.map_or("auto", GlyphLevel::name);
        self.apply_setting("tui.glyphs", &toml::Value::String(name.to_string()));
        let saved = crate::init::set_config_value(
            &self.env.paths.home_config(),
            &["tui", "glyphs"],
            Some(toml_edit::value(name)),
        );
        let shown = self.theme.glyphs().level();
        match saved {
            Ok(()) if level.is_some_and(|level| level != shown) => self.warn(&format!(
                "glyphs: {name} is saved, but AZAZEL_GLYPHS keeps this terminal on {shown}"
            )),
            Ok(()) if level.is_none() => self.info(&format!("glyphs: auto ({shown})")),
            Ok(()) => self.info(&format!("glyphs: {name}")),
            Err(e) => self.warn(&format!("glyphs: {name}, for this session only: {e:#}")),
        }
    }

    /// Moves the session to an entry chosen in the tree and restarts the lead from there. One
    /// of your prompts is gone back to before, and put in the editor.
    pub(super) async fn jump_to(&mut self, id: &str, is_user: bool, prompt: &str) {
        let to = Go::Leaf {
            entry: id.to_string(),
            before: is_user,
        };
        if self.go(to).await && is_user {
            self.editor.set_text(prompt);
        }
    }

    async fn fork(&mut self) {
        if self.idle_to("fork the session") {
            self.go(Go::Fork).await;
        }
    }

    // ------------------------------------------------------------ rewind

    pub(super) async fn open_rewind(&mut self) {
        if !self.idle_to("rewind") {
            return;
        }
        let points = match self.backend.rewind_points().await {
            Ok(points) => points,
            Err(why) => return self.info(&why),
        };
        let mut items = Vec::new();
        for (point, changed) in &points {
            let detail = if changed.is_empty() {
                "no file changes since".to_string()
            } else {
                changed.clone()
            };
            let label = if point.prompt.is_empty() {
                "(files as they were before a rewind)".to_string()
            } else {
                first_line(&point.prompt)
            };
            items.push(SelectItem::new(label, detail));
        }
        let body = vec!["Go back to before:".to_string()];
        let points = points.into_iter().map(|(point, _)| point).collect();
        self.overlay = Some(Overlay::select(
            "Rewind",
            body,
            items,
            false,
            Action::Rewind(points),
        ));
    }

    pub(super) async fn rewind(&mut self, point: RewindPoint, code: bool, conversation: bool) {
        match self.backend.rewind(&point, code, conversation).await {
            Ok(Some(moved)) => {
                self.moved(moved, false).await;
                self.editor.set_text(&point.prompt);
            }
            Ok(None) => self.sync().await,
            Err(why) => {
                self.sync().await;
                return self.fail(&why);
            }
        }
        self.info(match (code, conversation) {
            (true, true) => "files and conversation rewound; /rewind can bring the files back",
            (true, false) => "files rewound; /rewind can bring them back",
            _ => "conversation rewound; files are unchanged",
        });
    }

    // ------------------------------------------------------------ reports

    fn clear_screen(&mut self) {
        self.transcript.clear();
        self.backdrop.clear();
        self.after_card = false;
        let _ = self.renderer.suspend();
        let out = self.renderer.get_mut();
        // Screen, scrollback, then home.
        let _ = out.write_all(b"\x1b[2J\x1b[3J\x1b[H");
        let _ = out.flush();
        // The live region starts over at the top of an empty screen and fills it again.
        let (_, rows) = self.renderer.size();
        self.floor.reset(rows);
    }

    fn show_keys(&mut self) {
        let mut out = String::new();
        for action in self.keymap.actions() {
            let keys: Vec<String> = self
                .keymap
                .keys_for(action)
                .iter()
                .map(ToString::to_string)
                .collect();
            let keys = if keys.is_empty() {
                "(unbound)".to_string()
            } else {
                keys.join(", ")
            };
            out.push_str(&format!("{action:<20} {keys}\n"));
        }
        out.push_str(&format!(
            "\nchange them in {}",
            self.env.paths.keybindings_file().display()
        ));
        self.info(&out);
    }
}

/// What `/glyphs <name>` names: a level, or `None` for `auto`. `None` when it names neither.
fn glyph_choice(name: &str) -> Option<Option<GlyphLevel>> {
    match name.trim().to_ascii_lowercase().as_str() {
        "auto" => Some(None),
        other => other.parse().ok().map(Some),
    }
}

/// A handful of the symbols of `level`, as its row of the glyph picker shows them.
fn glyph_preview(level: GlyphLevel) -> String {
    let glyphs = Glyphs::new(level);
    [
        Glyph::Success,
        Glyph::Warning,
        Glyph::Fail,
        Glyph::Branch,
        Glyph::Context,
        Glyph::Rate,
        Glyph::Model,
        Glyph::Prompt,
    ]
    .into_iter()
    .map(|glyph| glyphs.get(glyph))
    .filter(|symbol| !symbol.is_empty())
    .collect::<Vec<_>>()
    .join(" ")
}

/// The accent, success, warning and error colours of `theme` as four dots, for its row of the
/// theme picker. Nothing in it resets styles, so the band of a selected row holds.
fn swatches(theme: &Theme, dot: &str) -> String {
    [Role::Accent, Role::Success, Role::Warning, Role::Error]
        .into_iter()
        .map(|role| format!("{}{dot}", theme.fg(role)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// The checkpoints on the current branch, newest first, each with the prompt it preceded.
pub(super) fn rewind_points(session: &SessionStore) -> Vec<RewindPoint> {
    let branch = session.branch(None);
    let mut points = Vec::new();
    for (at, entry) in branch.iter().enumerate() {
        let EntryKind::Checkpoint { commit, .. } = &entry.kind else {
            continue;
        };
        // A checkpoint taken by a rewind is not followed by a prompt of its own.
        let mut prompt = String::new();
        for later in &branch[at + 1..] {
            match &later.kind {
                EntryKind::Checkpoint { .. } => break,
                EntryKind::Message { message }
                    if message.role == MessageRole::User && !is_note(&message.text()) =>
                {
                    prompt = message.text();
                    break;
                }
                _ => {}
            }
        }
        points.push(RewindPoint {
            entry_id: entry.id.clone(),
            commit: commit.clone(),
            prompt,
        });
    }
    points.reverse();
    points
}

/// Whether a user message in the transcript was written by the UI rather than typed as a prompt.
pub(crate) fn is_note(message: &str) -> bool {
    message.starts_with(SHELL_NOTE) || message.starts_with(REWIND_NOTE)
}

#[cfg(test)]
mod tests {
    use azazel_tui::{RESET, SelectList, text};

    use super::*;

    #[test]
    fn a_glyph_set_is_named_as_in_the_configuration() {
        assert_eq!(glyph_choice("nerd"), Some(Some(GlyphLevel::Nerd)));
        assert_eq!(glyph_choice(" ASCII "), Some(Some(GlyphLevel::Ascii)));
        assert_eq!(glyph_choice("auto"), Some(None));
        assert_eq!(glyph_choice("fancy"), None);
    }

    #[test]
    fn each_glyph_set_previews_its_own_symbols() {
        assert_eq!(glyph_preview(GlyphLevel::Unicode), "✔ ⚠ ✘ ⑂ ◫ ↯ ⬢ ❯");
        // Symbols that are words or nothing at this level are shown as they are, or left out.
        assert_eq!(glyph_preview(GlyphLevel::Ascii), "[+] [!] [-] git: ctx >");
        assert!(glyph_preview(GlyphLevel::Ascii).is_ascii());
        let nerd = glyph_preview(GlyphLevel::Nerd);
        assert_eq!(nerd.split(' ').count(), 8);
        assert!(nerd.chars().any(|c| ('\u{e000}'..='\u{f8ff}').contains(&c)));
    }

    #[test]
    fn theme_swatches_carry_the_colours_of_their_theme_through_a_selected_row() {
        let theme = Theme::builtin("gruvbox-dark").unwrap();
        let preview = swatches(&theme, "●");
        assert_eq!(text::strip_ansi(&preview), "● ● ● ●");
        for role in [Role::Accent, Role::Success, Role::Warning, Role::Error] {
            assert!(preview.contains(&theme.fg(role)), "{role:?}");
        }
        // Nothing in it ends the band of a selected row.
        assert!(!preview.contains(RESET));
        let shown = Theme::dark();
        let list = SelectList::new(vec![SelectItem::with_preview("gruvbox-dark", preview.clone())]);
        let row = &list.view(40, 5, &shown).lines[0];
        assert!(row.contains(&preview));
        assert_eq!(row.matches(RESET).count(), 1);
        assert_eq!(text::width(row), 40);
    }
}
