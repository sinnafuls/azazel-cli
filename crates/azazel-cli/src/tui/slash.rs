//! Slash commands and the pickers that go with them.

use std::collections::HashSet;
use std::io::Write;
use std::sync::Arc;

use azazel_agent::{AgentCmd, ToolSource, UserInput};
use azazel_ai::providers::presets;
use azazel_ai::{ModelSpec, Role as MessageRole, ThinkingLevel};
use azazel_core::skills::invocation_text;
use azazel_core::{EntryKind, PermissionMode, SessionStore, StartSource};
use azazel_tui::SelectItem;
use azazel_tui::components::format_tokens;

use super::overlay::{Action, Overlay, RewindPoint};
use super::{Internal, SHELL_NOTE, Tui, first_line, view};
use crate::app::Open;
use crate::doctor;

const BUILTIN: &[(&str, &str)] = &[
    ("help", "List commands"),
    (
        "model",
        "Pick the model, or set it: /model <name> [fallback …] (part of a name is enough)",
    ),
    ("thinking", "Pick the thinking level, or set it: /thinking high"),
    ("mode", "Pick the permission mode, or set it: /mode auto"),
    ("new", "Start a new session"),
    ("resume", "Reopen an earlier session of this project"),
    ("tree", "Go back to an earlier point of this session"),
    ("fork", "Continue from here in a copy of this session"),
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
const REWIND_NOTE: &str = "I rewound the project files";

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
        for command in self.app.registry.commands() {
            if out.iter().any(|(name, _)| *name == command.name) {
                continue;
            }
            let about = match &command.argument_hint {
                Some(hint) => format!("{} {hint}", command.description),
                None => command.description,
            };
            out.push((command.name, about));
        }
        for skill in self.app.registry.skills.list() {
            if skill.user_invocable {
                out.push((format!("skill:{}", skill.name), skill.description));
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
        match name {
            "help" => self.help(),
            "model" if args.is_empty() => self.open_model_picker(),
            "model" => {
                let specs: Vec<&str> = args.split_whitespace().collect();
                self.set_model(&specs);
            }
            "thinking" if args.is_empty() => {
                let items = ThinkingLevel::ALL
                    .iter()
                    .map(|level| SelectItem::new(level.to_string(), ""))
                    .collect();
                let body = vec![format!("current: {}", self.active.lead.status().thinking)];
                self.overlay = Some(Overlay::select("Thinking", body, items, false, Action::Thinking));
            }
            "thinking" => match args.parse::<ThinkingLevel>() {
                Ok(level) => {
                    self.active.lead.send(AgentCmd::SetThinking(level));
                    self.info(&format!("thinking: {level}"));
                }
                Err(e) => self.fail(&e.to_string()),
            },
            "mode" if args.is_empty() => {
                let items = PermissionMode::ALL
                    .iter()
                    .map(|mode| SelectItem::new(mode.to_string(), ""))
                    .collect();
                let body = vec![format!("current: {}", self.app.permissions.mode())];
                self.overlay = Some(Overlay::select(
                    "Permission mode",
                    body,
                    items,
                    false,
                    Action::Mode,
                ));
            }
            "mode" => match args.parse::<PermissionMode>() {
                Ok(mode) => self.app.permissions.set_mode(mode),
                Err(e) => self.fail(&e),
            },
            "new" => {
                if self.idle_to("start a new session") {
                    self.swap_session(Open::New, StartSource::Clear).await;
                }
            }
            "resume" => self.open_resume(),
            "tree" => self.open_tree(),
            "fork" => self.fork().await,
            "compact" => {
                let instructions = (!args.is_empty()).then(|| args.to_string());
                self.active.lead.send(AgentCmd::Compact { instructions });
                self.notice = Some("compacting…".to_string());
            }
            "clear" => self.clear_screen(),
            "cost" => self.show_cost(),
            "context" => self.show_context(),
            "export" => self.export(args),
            "theme" if args.is_empty() => self.open_theme_picker(),
            "theme" => self.set_theme(args),
            "keys" => self.show_keys(),
            "rewind" => self.open_rewind().await,
            "permissions" => match args.strip_prefix("add") {
                Some(rule) if !rule.trim().is_empty() => self.add_rule(rule),
                _ => self.open_permissions(),
            },
            "forget-approvals" => self.forget_approvals(),
            "trust" => self.trust_command(args),
            "transcript" => self.open_transcript().await,
            "copy" => self.copy_command(args),
            "vim" => self.toggle_vim(),
            "tools" => self.show_tools(),
            "jobs" => self.open_jobs(),
            "todo" => self.todo_command(args),
            "reload" => self.reload().await,
            "dirs" => {
                let roots: Vec<String> = self
                    .app
                    .permissions
                    .roots()
                    .iter()
                    .map(|root| root.display().to_string())
                    .collect();
                self.info(&format!("agents may work in:\n{}", roots.join("\n")));
            }
            "changelog" => match crate::changelog::text((!args.is_empty()).then_some(args)) {
                Ok(text) => self.info(&text),
                Err(e) => self.fail(&e),
            },
            "add-dir" if args.is_empty() => self.fail("usage: /add-dir <path>"),
            "add-dir" => {
                let dir = azazel_core::paths::canonical(&self.app.paths.cwd.join(args.trim()));
                if !dir.is_dir() {
                    self.fail(&format!("{} is not a directory", dir.display()));
                } else if self.app.permissions.add_root(&dir) {
                    self.info(&format!(
                        "agents may now work in {} too, until azazel exits",
                        dir.display()
                    ));
                } else {
                    self.info(&format!(
                        "{} is already a directory agents work in",
                        dir.display()
                    ));
                }
            }
            "rules" => {
                let report = crate::rules::session_report(&self.app, &self.active.session.id());
                self.info(&report);
            }
            "hooks" => self.info(&crate::hooks::report(&self.app)),
            "btw" => self.btw_command(args),
            "diff" => self.diff_command(args).await,
            "commit" => self.commit_command(args).await,
            "init" => self.send_prompt(INIT_PROMPT.to_string()).await,
            "doctor" => {
                let report = doctor::examine(&self.app.paths, self.app.trusted).await;
                self.info(&report.render());
            }
            "voice" => self.voice_toggle().await,
            "speak" => self.speak_command(args),
            "quit" | "exit" => self.quit = true,
            #[cfg(feature = "mcp")]
            "mcp" if args.is_empty() => self.open_mcp(),
            _ => self.other_command(name, args).await,
        }
    }

    /// A skill (`/skill:<name>`) or a command registered by an extension. Registered commands run
    /// in the background (some wait on the network or on the user elsewhere); their outcome comes
    /// back through the UI loop.
    pub(super) async fn other_command(&mut self, name: &str, args: &str) {
        if let Some(skill_name) = name.strip_prefix("skill:") {
            let skills = self.app.registry.skills.clone();
            match skills.get(skill_name).filter(|skill| skill.user_invocable) {
                Some(skill) => {
                    skills.mark_used(&skill);
                    azazel_core::skills::allow_for_prompt(&self.app.registry.services, &skill);
                    self.prompt_on(invocation_text(&skill, args), skill.model.clone())
                        .await;
                }
                None => self.fail(&format!("no skill named `{skill_name}`; /skills lists them")),
            }
            return;
        }
        let Some(command) = self.app.registry.command(name) else {
            return self.fail(&format!("unknown command /{name}; /help lists commands"));
        };
        let context = self.app.registry.bus.ctx();
        let (args, done) = (args.to_string(), self.internal_tx.clone());
        tokio::spawn(async move {
            let _ = done.send(Internal::Command(command.handler.run(&args, &context).await));
        });
    }

    /// Sends a prompt, on `model` for this one prompt when it is given.
    pub(super) async fn prompt_on(&mut self, prompt: String, model: Option<Vec<ModelSpec>>) {
        if let Some(chain) = model {
            if self.busy {
                return self.fail("this command runs on its own model; wait until the agent is idle");
            }
            if self.app.router.resolve(&chain).is_empty() {
                self.warn("the command's model resolves to nothing; using the current one");
            } else {
                self.restore_chain = Some(self.active.lead.status().chain);
                self.active.lead.send(AgentCmd::SetModel(chain));
            }
        }
        self.send_prompt(prompt).await;
    }

    /// Whether the lead is idle; says what cannot be done otherwise.
    fn idle_to(&mut self, what: &str) -> bool {
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

    pub(super) fn open_model_picker(&mut self) {
        let router = self.app.router.clone();
        let mut specs: Vec<String> = Vec::new();
        let mut items: Vec<SelectItem> = Vec::new();
        for role in router.roles().keys() {
            let resolved = router.resolve(&[ModelSpec::Role(role.clone())]);
            let Some(first) = resolved.first() else { continue };
            items.push(SelectItem::new(format!("@{role}"), first.to_string()));
            specs.push(format!("@{role}"));
        }
        // Providers that can be reached: a credential is present, or none is needed.
        let configured = presets::resolve(&self.app.config.providers).unwrap_or_default();
        for provider in router.provider_ids() {
            let usable = match configured.iter().find(|p| p.id == provider) {
                Some(config) => !config.requires_key || self.app.credentials.source(config).is_some(),
                None => true,
            };
            if !usable {
                continue;
            }
            for model in router.catalog().for_provider(provider) {
                let detail = format!("{} context", format_tokens(model.context_window));
                items.push(SelectItem::new(model.key(), detail));
                specs.push(model.key());
            }
        }
        // Favourites are marked; ctrl+f on a row adds it to them or takes it out.
        for (item, spec) in items.iter_mut().zip(&specs) {
            if self.favourites.contains(spec) {
                item.detail = format!("★ {}", item.detail);
            }
        }
        let body = vec![format!("current: {}", self.model_label()), self.favourites_line()];
        self.overlay = Some(
            Overlay::select("Model", body, items, true, Action::Model(specs))
                .with_keys(super::panels::model_keys()),
        );
    }

    pub(super) fn open_history(&mut self) {
        let mut prompts: Vec<String> = self.prompts.iter().rev().cloned().collect();
        for entry in self.active.session.branch(None).iter().rev() {
            let EntryKind::Message { message } = &entry.kind else {
                continue;
            };
            let prompt = message.text();
            if message.role == MessageRole::User
                && !prompt.trim().is_empty()
                && !is_note(&prompt)
                && !prompts.contains(&prompt)
            {
                prompts.push(prompt);
            }
        }
        if prompts.is_empty() {
            self.notice = Some("no earlier prompts".to_string());
            return;
        }
        let items = prompts
            .iter()
            .map(|prompt| SelectItem::new(first_line(prompt), ""))
            .collect();
        self.overlay = Some(Overlay::select(
            "Prompt history",
            Vec::new(),
            items,
            true,
            Action::History(prompts),
        ));
    }

    /// Servers to pick from; choosing one offers what can be done with it.
    #[cfg(feature = "mcp")]
    fn open_mcp(&mut self) {
        let Some(manager) = self.app.registry.services.get::<azazel_mcp::McpManager>() else {
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
                    _ => format!("{} · {}", server.state, server.target),
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
        let mut names = vec!["dark".to_string(), "light".to_string()];
        let mut custom: Vec<String> = std::fs::read_dir(self.app.paths.home.join("themes"))
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|e| e == "toml"))
            .filter_map(|path| Some(path.file_stem()?.to_string_lossy().into_owned()))
            .collect();
        custom.sort();
        names.extend(custom);
        let items = names
            .iter()
            .map(|name| SelectItem::new(name.as_str(), ""))
            .collect();
        self.overlay = Some(Overlay::select(
            "Theme",
            Vec::new(),
            items,
            false,
            Action::Theme(names),
        ));
    }

    fn open_resume(&mut self) {
        if !self.idle_to("switch sessions") {
            return;
        }
        let current = self.active.session.path();
        let sessions: Vec<_> = SessionStore::list(&self.app.paths.sessions_dir())
            .into_iter()
            .filter(|session| session.path != current)
            .collect();
        if sessions.is_empty() {
            return self.info("this project has no other sessions");
        }
        let items = sessions
            .iter()
            .map(|session| {
                let label = session
                    .name
                    .as_deref()
                    .or(session.first_prompt.as_deref())
                    .unwrap_or("(no prompt yet)");
                let detail = format!("{} · {} messages", session.timestamp, session.messages);
                SelectItem::new(first_line(label), detail)
            })
            .collect();
        let paths = sessions.into_iter().map(|session| session.path).collect();
        self.overlay = Some(Overlay::select(
            "Resume a session",
            Vec::new(),
            items,
            true,
            Action::Resume(paths),
        ));
    }

    fn open_tree(&mut self) {
        if !self.idle_to("move within the session") {
            return;
        }
        let session = &self.active.session;
        let current: HashSet<String> = session.branch(None).into_iter().map(|entry| entry.id).collect();
        let mut rows = Vec::new();
        let mut items = Vec::new();
        for entry in session.entries().into_iter().rev() {
            let EntryKind::Message { message } = entry.kind else {
                continue;
            };
            let said = message.text();
            if message.role == MessageRole::Tool || said.trim().is_empty() {
                continue;
            }
            let is_user = message.role == MessageRole::User;
            let marker = if current.contains(&entry.id) { "•" } else { " " };
            let who = if is_user { "you" } else { "agent" };
            items.push(SelectItem::new(
                format!("{marker} {who}: {}", first_line(&said)),
                "",
            ));
            rows.push((entry.id, is_user, said));
        }
        if rows.is_empty() {
            return self.info("this session has no messages yet");
        }
        let body = vec![
            "• marks the current branch. Choosing one of your prompts goes back to before it and puts it in the \
             editor; choosing a reply continues from it."
                .to_string(),
        ];
        self.overlay = Some(Overlay::select(
            "Session tree",
            body,
            items,
            true,
            Action::Tree(rows),
        ));
    }

    /// Moves the session to an entry chosen in the tree and restarts the lead from there.
    pub(super) async fn jump_to(&mut self, id: &str, is_user: bool, prompt: &str) {
        let session = self.active.session.clone();
        let target = if is_user {
            session.entry(id).and_then(|entry| entry.parent_id)
        } else {
            Some(id.to_string())
        };
        if !session.set_leaf(target.as_deref()) {
            return self.fail("that entry is no longer part of the session");
        }
        self.swap_session(Open::Store(session), StartSource::Resume).await;
        if is_user {
            self.editor.set_text(prompt);
        }
    }

    async fn fork(&mut self) {
        if !self.idle_to("fork the session") {
            return;
        }
        match self.active.session.fork(&self.app.paths.sessions_dir(), None) {
            Ok(fork) => {
                self.swap_session(Open::Store(Arc::new(fork)), StartSource::Resume)
                    .await
            }
            Err(e) => self.fail(&format!("could not fork the session: {e}")),
        }
    }

    // ------------------------------------------------------------ rewind

    /// The checkpoints on the current branch, newest first, each with the prompt it preceded.
    fn rewind_points(&self) -> Vec<RewindPoint> {
        let branch = self.active.session.branch(None);
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

    pub(super) async fn open_rewind(&mut self) {
        if !self.idle_to("rewind") {
            return;
        }
        let points = self.rewind_points();
        if points.is_empty() {
            let reason = self.active.checkpoints.disabled().await;
            return self.info(&match reason {
                Some(reason) => format!("nothing to rewind to: {reason}"),
                None => "nothing to rewind to: no prompt has been sent in this session".to_string(),
            });
        }
        let checkpoints = self.active.checkpoints.clone();
        let now = checkpoints.snapshot("rewind list").await.ok().flatten();
        let mut items = Vec::new();
        for point in &points {
            let changed = match &now {
                Some(now) => checkpoints.shortstat(&point.commit, now).await,
                None => String::new(),
            };
            let detail = if changed.trim().is_empty() {
                "no file changes since".to_string()
            } else {
                changed.trim().to_string()
            };
            let label = if point.prompt.is_empty() {
                "(files as they were before a rewind)".to_string()
            } else {
                first_line(&point.prompt)
            };
            items.push(SelectItem::new(label, detail));
        }
        let body = vec!["Go back to before:".to_string()];
        self.overlay = Some(Overlay::select(
            "Rewind",
            body,
            items,
            false,
            Action::Rewind(points),
        ));
    }

    pub(super) async fn rewind(&mut self, point: RewindPoint, code: bool, conversation: bool) {
        let session = self.active.session.clone();
        let undo = if code {
            match self.active.checkpoints.restore(&point.commit).await {
                Ok(before) => Some(before),
                Err(e) => return self.fail(&format!("could not restore the files: {e}")),
            }
        } else {
            None
        };
        if conversation {
            let resume_at = session.entry(&point.entry_id).and_then(|entry| entry.parent_id);
            session.set_leaf(resume_at.as_deref());
        }
        // The files as they were just now stay reachable, so the rewind can itself be rewound.
        if let Some(commit) = undo {
            let turn_entry_id = session.leaf().unwrap_or_default();
            session.append(EntryKind::Checkpoint {
                turn_entry_id,
                commit,
            });
        }
        if conversation {
            self.swap_session(Open::Store(session), StartSource::Resume).await;
            self.editor.set_text(&point.prompt);
        } else {
            let target = if point.prompt.is_empty() {
                "before my previous rewind".to_string()
            } else {
                format!("before this prompt: {}", first_line(&point.prompt))
            };
            let note = format!(
                "{REWIND_NOTE} to how they were {target}\nFiles you read or changed since then may differ \
                 now; read them again before editing."
            );
            self.active.lead.send(AgentCmd::Append(UserInput::text(note)));
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

    fn show_cost(&mut self) {
        let status = self.active.lead.status();
        let usage = status.usage;
        let cost = match usage.cost_usd {
            Some(cost) => format!("${cost:.4}"),
            None => "unknown (no price is known for this model)".to_string(),
        };
        let cost = match self.app.router.tier() {
            Some(tier) => format!("{cost}\n{}", crate::routing::tier_cost_note(tier)),
            None => cost,
        };
        let cost = match crate::side::cost_report(&self.app.registry.services) {
            Some(side) => format!("{cost}\n{side}"),
            None => cost,
        };
        self.info(&format!(
            "requests: {}\ninput: {} · output: {} · cache read: {} · cache write: {} · reasoning: {}\ncost: {cost}",
            status.requests,
            format_tokens(usage.input),
            format_tokens(usage.output),
            format_tokens(usage.cache_read),
            format_tokens(usage.cache_write),
            format_tokens(usage.reasoning),
        ));
    }

    fn show_context(&mut self) {
        let status = self.active.lead.status();
        let messages = self.active.session.build_context(None).len();
        let tools = self.app.registry.tools.tools(self.active.lead.info()).len();
        let window = if status.context_window == 0 {
            "unknown until the first reply".to_string()
        } else {
            format!(
                "{} of {} tokens ({}%)",
                format_tokens(status.context_tokens),
                format_tokens(status.context_window),
                status.context_tokens * 100 / status.context_window
            )
        };
        self.info(&format!(
            "context: {window}\nmessages: {messages}\ntools offered: {tools}"
        ));
    }

    fn export(&mut self, args: &str) {
        use crate::export::{Format, default_file_name, render};
        let session = &self.active.session;
        let path = if args.is_empty() {
            self.app
                .paths
                .cwd
                .join(default_file_name(session, Format::Markdown))
        } else {
            self.app.paths.cwd.join(args)
        };
        let format = Format::from_path(&path).unwrap_or(Format::Markdown);
        match std::fs::write(&path, render(session, format)) {
            Ok(()) => self.info(&format!("exported to {}", path.display())),
            Err(e) => self.fail(&format!("could not write {}: {e}", path.display())),
        }
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
            self.app.paths.keybindings_file().display()
        ));
        self.info(&out);
    }
}

/// Whether a user message in the transcript was written by the UI rather than typed as a prompt.
pub(super) fn is_note(message: &str) -> bool {
    message.starts_with(SHELL_NOTE) || message.starts_with(REWIND_NOTE)
}
