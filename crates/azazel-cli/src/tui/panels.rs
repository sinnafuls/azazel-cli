//! Commands that look into the running session and change it: `/tools`, `/jobs`, `/todo`,
//! `/permissions`, `/forget-approvals`, `/trust` and `/reload`, plus what the keys of overlays do.

use std::collections::BTreeMap;

use azazel_core::agents;
use azazel_core::commands::{CustomCommandsExt, command_roots};
use azazel_core::hooks::HooksExt;
use azazel_core::packages::Packages;
use azazel_core::permissions::{Rule, RuleKind, SESSION, SourcedRule};
use azazel_core::trust::{TrustStore, needs_trust};
use azazel_core::{Config, Extension};
use azazel_tools::{Jobs, TodoItem, TodoStatus, TodoStore};
use azazel_tui::{KeyCombo, Keymap, SelectItem};
use crossterm::event::{KeyCode, KeyModifiers};

use super::overlay::{Action, Kind, Overlay};
use super::{Tui, first_line};

/// The keys of the agent hub, in the order [`Tui::overlay_hotkey`] reads them.
pub(super) fn hub_keys() -> Vec<(KeyCombo, &'static str)> {
    let plain = |c| KeyCombo::new(KeyCode::Char(c), KeyModifiers::NONE);
    vec![
        (plain('m'), "message"),
        (plain('s'), "stop"),
        (plain('S'), "stop all"),
    ]
}

/// The key of the model picker that adds a model to the favourites or removes it.
pub(super) fn model_keys() -> Vec<(KeyCombo, &'static str)> {
    vec![(
        KeyCombo::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        "favourite",
    )]
}

fn permission_keys() -> Vec<(KeyCombo, &'static str)> {
    let plain = |c| KeyCombo::new(KeyCode::Char(c), KeyModifiers::NONE);
    vec![(plain('d'), "remove"), (plain('a'), "add")]
}

fn kind_word(kind: RuleKind) -> &'static str {
    match kind {
        RuleKind::Allow => "allow",
        RuleKind::Ask => "ask",
        RuleKind::Deny => "deny",
    }
}

/// A rule as typed in `/permissions`: `allow shell(cargo test:*)`.
pub(super) fn parse_rule(typed: &str) -> Result<(RuleKind, Rule), String> {
    let usage = "write the kind and the rule, for example: allow shell(cargo test:*)";
    let (kind, rule) = typed.trim().split_once(char::is_whitespace).ok_or(usage)?;
    let kind = match kind.to_ascii_lowercase().as_str() {
        "allow" => RuleKind::Allow,
        "ask" => RuleKind::Ask,
        "deny" => RuleKind::Deny,
        _ => return Err(usage.to_string()),
    };
    Ok((kind, rule.trim().parse()?))
}

/// Which group of `/tools` a tool belongs to: its MCP server, or what its extension is.
fn tool_group(name: &str, owner: &str) -> String {
    if let Some(rest) = name.strip_prefix("mcp__") {
        let server = rest.split("__").next().unwrap_or(rest);
        return format!("MCP server {server}");
    }
    match owner {
        "core-tools" | "permissions" | "web" | "lsp" | "skills" => "core".to_string(),
        "team" | "goal" => "team".to_string(),
        "lua" => "scripts".to_string(),
        other => other.to_string(),
    }
}

/// What changed between two lists of names, as `+2 (a, b) −1 (c)`; empty when nothing did.
pub(super) fn name_changes(before: &[String], after: &[String]) -> String {
    let added: Vec<&str> = after
        .iter()
        .filter(|name| !before.contains(name))
        .map(String::as_str)
        .collect();
    let removed: Vec<&str> = before
        .iter()
        .filter(|name| !after.contains(name))
        .map(String::as_str)
        .collect();
    let mut parts = Vec::new();
    if !added.is_empty() {
        parts.push(format!("+{} ({})", added.len(), added.join(", ")));
    }
    if !removed.is_empty() {
        parts.push(format!("−{} ({})", removed.len(), removed.join(", ")));
    }
    parts.join(" ")
}

/// `/todo` edits as changes to a list. `Err` is what to tell the user.
pub(super) fn edit_todo(mut items: Vec<TodoItem>, args: &str) -> Result<Vec<TodoItem>, String> {
    let (verb, rest) = match args.split_once(char::is_whitespace) {
        Some((verb, rest)) => (verb, rest.trim()),
        None => (args, ""),
    };
    match verb {
        "add" if rest.is_empty() => Err("usage: /todo add <text>".to_string()),
        "add" => {
            items.push(TodoItem {
                text: rest.to_string(),
                status: TodoStatus::Pending,
            });
            Ok(items)
        }
        "done" => {
            let count = items.len();
            let item = rest
                .parse::<usize>()
                .ok()
                .and_then(|number| number.checked_sub(1))
                .and_then(|index| items.get_mut(index))
                .ok_or_else(|| format!("usage: /todo done <n>, with n from 1 to {count}"))?;
            item.status = TodoStatus::Done;
            Ok(items)
        }
        "clear" => Ok(Vec::new()),
        _ => Err("usage: /todo, /todo add <text>, /todo done <n> or /todo clear".to_string()),
    }
}

fn todo_text(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "the todo list is empty; /todo add <text> adds an item".to_string();
    }
    items
        .iter()
        .enumerate()
        .map(|(index, item)| {
            let mark = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Done => "[x]",
            };
            format!("{:>2}. {mark} {}", index + 1, item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl Tui {
    // ------------------------------------------------------------ overlay keys

    /// Acts on one of the open overlay's own keys: `key` indexes its `keys`, `index` is the
    /// selected row. The overlay stays open unless the action replaces or closes it.
    pub(super) async fn overlay_hotkey(&mut self, index: usize, key: usize) {
        if self.session_hotkey(index, key).await {
            return;
        }
        // What the key acts on is copied out first: acting on it may replace the overlay.
        enum Target {
            Agent(String),
            Model(String),
            Rule(SourcedRule),
        }
        let target = match self.overlay.as_ref().map(|overlay| &overlay.action) {
            Some(Action::Agents(ids)) => ids.get(index).cloned().map(Target::Agent),
            Some(Action::Model(specs)) => specs.get(index).cloned().map(Target::Model),
            Some(Action::Permissions(rules)) => rules.get(index).cloned().map(Target::Rule),
            _ => None,
        };
        match target {
            Some(Target::Agent(id)) => {
                if let Some(key) = ['m', 's', 'S'].get(key) {
                    self.hub_key(*key, &id);
                }
            }
            Some(Target::Model(spec)) => {
                let added = self.toggle_favourite(&spec);
                let what = if added { "added to" } else { "removed from" };
                let body = vec![
                    format!("current: {}", self.model_label()),
                    format!("{spec} {what} the favourites. {}", self.favourites_line()),
                ];
                if let Some(overlay) = &mut self.overlay {
                    overlay.body = body;
                }
            }
            Some(Target::Rule(rule)) if key == 0 => self.remove_rule(&rule),
            Some(Target::Rule(_)) => {
                let body = vec![
                    "Kind and rule, for example: allow shell(cargo test:*)".to_string(),
                    "Kinds: allow, ask, deny. A rule is a tool, or tool(pattern).".to_string(),
                ];
                self.overlay = Some(Overlay::input("Add a rule", body, Action::PermissionAdd));
            }
            None => {}
        }
    }

    /// The favourites as the model picker's body shows them.
    pub(super) fn favourites_line(&self) -> String {
        if self.favourites.is_empty() {
            "No favourites yet: alt+m steps through @default, @smol and @slow.".to_string()
        } else {
            format!("alt+m steps through: {}", self.favourites.join(", "))
        }
    }

    /// What the overlays of this module do when they are answered.
    pub(super) async fn finish_panel(&mut self, action: Action, choice: usize, typed: String) {
        match action {
            Action::CopyCode(blocks) => {
                if let Some(block) = blocks.get(choice) {
                    self.copy_text(block, "the code block");
                }
            }
            Action::Jobs(ids) => {
                if let Some(id) = ids.get(choice) {
                    let items = ["Read its output", "Stop it"]
                        .iter()
                        .map(|label| SelectItem::new(*label, ""))
                        .collect();
                    self.overlay = Some(Overlay::select(
                        format!("Background job {id}"),
                        Vec::new(),
                        items,
                        false,
                        Action::JobChoice(id.clone()),
                    ));
                }
            }
            Action::JobChoice(id) => {
                let Some(jobs) = self.app.registry.services.get::<Jobs>() else {
                    return;
                };
                if choice == 0 {
                    match jobs.output(&id, 100) {
                        Some(output) => self.info(&output),
                        None => self.fail(&format!("no job named `{id}`")),
                    }
                } else if jobs.kill(&id) {
                    self.info(&format!("stopping {id}"));
                } else {
                    self.fail(&format!("no job named `{id}`"));
                }
            }
            Action::Permissions(rules) => {
                if let Some(rule) = rules.get(choice) {
                    self.info(&format!(
                        "{} {} comes from {}",
                        kind_word(rule.kind),
                        rule.rule,
                        rule.source
                    ));
                }
            }
            Action::PermissionAdd => match parse_rule(&typed) {
                Ok((RuleKind::Allow, rule)) => {
                    let items = vec![
                        SelectItem::new("For this session", "until azazel exits"),
                        SelectItem::new("Saved for this project", ".azazel/config.local.toml"),
                    ];
                    self.overlay = Some(Overlay::select(
                        format!("Keep allow {rule}"),
                        Vec::new(),
                        items,
                        false,
                        Action::PermissionScope(rule, RuleKind::Allow),
                    ));
                }
                // Only approvals are saved from here; ask and deny rules belong in a config file.
                Ok((kind, rule)) => {
                    self.app.permissions.add_session_rule(rule.clone(), kind);
                    self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
                    self.open_permissions();
                }
                Err(why) => self.fail(&why),
            },
            Action::PermissionScope(rule, kind) => {
                if choice == 0 {
                    self.app.permissions.add_session_rule(rule.clone(), kind);
                    self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
                } else {
                    match self.app.permissions.remember_allow(&rule) {
                        Ok(()) => self.info(&format!("allow {rule} is saved for this project")),
                        Err(e) => self.fail(&format!("could not save the rule: {e}")),
                    }
                }
                self.open_permissions();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ /tools, /jobs, /todo

    /// `/tools`: every tool the lead could be offered, grouped by where it comes from.
    pub(super) fn show_tools(&mut self) {
        let lead = self.active.lead.info();
        let listing = self.app.registry.tools.listing(lead);
        #[cfg(feature = "mcp")]
        let deferred: Vec<String> = self
            .app
            .registry
            .services
            .get::<azazel_mcp::McpManager>()
            .map(|manager| {
                manager
                    .tool_states()
                    .into_iter()
                    .filter(|(_, offered)| !offered)
                    .map(|(name, _)| name)
                    .collect()
            })
            .unwrap_or_default();
        #[cfg(not(feature = "mcp"))]
        let deferred: Vec<String> = Vec::new();

        let mut groups: BTreeMap<String, Vec<String>> = BTreeMap::new();
        let mut offered = 0;
        for tool in &listing {
            let mark = if tool.offered {
                offered += 1;
                ""
            } else {
                " (disabled)"
            };
            groups
                .entry(tool_group(&tool.name, &tool.owner))
                .or_default()
                .push(format!("{}{mark}", tool.name));
        }
        for name in &deferred {
            if !listing.iter().any(|tool| tool.name == *name) {
                groups
                    .entry(tool_group(name, "mcp"))
                    .or_default()
                    .push(format!("{name} (deferred: found through tool_search)"));
            }
        }
        let mut out = format!("{offered} tools are offered to the lead\n");
        // Core first, then the rest by name.
        let core = groups.remove("core");
        for (group, mut names) in core
            .map(|names| ("core".to_string(), names))
            .into_iter()
            .chain(groups)
        {
            names.sort();
            out.push_str(&format!("\n{group}\n  {}\n", names.join("\n  ")));
        }
        self.info(out.trim_end());
    }

    /// `/jobs`: the background jobs; choosing one offers its output or stops it.
    pub(super) fn open_jobs(&mut self) {
        let entries = self
            .app
            .registry
            .services
            .get::<Jobs>()
            .map(|jobs| jobs.entries())
            .unwrap_or_default();
        if entries.is_empty() {
            return self.info("no background jobs");
        }
        let items = entries
            .iter()
            .map(|job| {
                SelectItem::new(
                    format!("{}  {}", job.id, first_line(&job.command)),
                    job.state.as_str(),
                )
            })
            .collect();
        let ids = entries.into_iter().map(|job| job.id).collect();
        self.overlay = Some(Overlay::select(
            "Background jobs",
            Vec::new(),
            items,
            false,
            Action::Jobs(ids),
        ));
    }

    /// `/todo`, `/todo add <text>`, `/todo done <n>`, `/todo clear`: the lead's task list.
    pub(super) fn todo_command(&mut self, args: &str) {
        let Some(store) = self.app.registry.services.get::<TodoStore>() else {
            return self.info("this session has no todo list");
        };
        let lead = self.active.lead.id.clone();
        let items = store.get(&lead);
        if args.is_empty() {
            return self.info(&todo_text(&items));
        }
        match edit_todo(items, args) {
            Ok(items) => {
                self.info(&todo_text(&items));
                store.set(&lead, items);
            }
            Err(why) => self.fail(&why),
        }
    }

    // ------------------------------------------------------------ permissions and trust

    /// `/permissions`: the rules in effect with where each comes from. `d` removes one that was
    /// added here or remembered, `a` adds one.
    pub(super) fn open_permissions(&mut self) {
        let rules = self.app.permissions.rules();
        let local = self.app.paths.project_local_config().display().to_string();
        let items: Vec<SelectItem> = rules
            .iter()
            .map(|sourced| {
                let source = if sourced.source == SESSION {
                    "this session".to_string()
                } else if sourced.source == local && sourced.kind == RuleKind::Allow {
                    "remembered for this project".to_string()
                } else {
                    format!("from {}", sourced.source)
                };
                SelectItem::new(format!("{:<5} {}", kind_word(sourced.kind), sourced.rule), source)
            })
            .collect();
        let mut body = vec![
            format!("mode: {}", self.app.permissions.mode()),
            "d removes a rule of this session or a remembered one; rules from files are changed there"
                .to_string(),
        ];
        if items.is_empty() {
            // The keys act on a row, so an empty list is text that says how to add a rule. This
            // also closes the list after its last rule was removed.
            self.overlay = None;
            body[1] = "no rules yet; add one with: /permissions add allow shell(cargo test:*)".to_string();
            return self.info(&body.join("\n"));
        }
        self.overlay = Some(
            Overlay::select("Permission rules", body, items, false, Action::Permissions(rules))
                .with_keys(permission_keys()),
        );
    }

    /// `/permissions add <kind> <rule>`: a rule for this session.
    pub(super) fn add_rule(&mut self, typed: &str) {
        match parse_rule(typed) {
            Ok((kind, rule)) => {
                self.app.permissions.add_session_rule(rule.clone(), kind);
                self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
            }
            Err(why) => self.fail(&why),
        }
    }

    fn remove_rule(&mut self, sourced: &SourcedRule) {
        let local = self.app.paths.project_local_config().display().to_string();
        let label = format!("{} {}", kind_word(sourced.kind), sourced.rule);
        if sourced.source == SESSION {
            self.app
                .permissions
                .remove_session_rule(&sourced.rule, sourced.kind);
            self.info(&format!("removed {label}"));
        } else if sourced.source == local && sourced.kind == RuleKind::Allow {
            match self.app.permissions.forget_allow(&sourced.rule) {
                Ok(_) => self.info(&format!("removed {label} from {local}")),
                Err(e) => self.fail(&format!("could not edit {local}: {e}")),
            }
        } else {
            self.warn(&format!(
                "{label} comes from {}; edit that file to remove it",
                sourced.source
            ));
        }
        self.open_permissions();
    }

    /// `/forget-approvals`: drops every remembered approval and every rule of this session.
    pub(super) fn forget_approvals(&mut self) {
        match self.app.permissions.clear_remembered() {
            Ok(0) => self.info("there were no remembered approvals or session rules"),
            Ok(1) => self.info("forgot 1 rule; the agent asks again from now on"),
            Ok(count) => self.info(&format!("forgot {count} rules; the agent asks again from now on")),
            Err(e) => self.fail(&format!(
                "could not edit {}: {e}",
                self.app.paths.project_local_config().display()
            )),
        }
    }

    /// `/trust`: whether this project is trusted and what that covers; `/trust yes|no` decides.
    pub(super) fn trust_command(&mut self, args: &str) {
        let paths = &self.app.paths;
        let store = TrustStore::new(paths.trust_file());
        let decide = match args.to_ascii_lowercase().as_str() {
            "" => None,
            "yes" | "y" | "on" => Some(true),
            "no" | "n" | "off" => Some(false),
            _ => return self.fail("usage: /trust, /trust yes or /trust no"),
        };
        if let Some(trusted) = decide {
            return match store.set(&paths.project_root, trusted) {
                Ok(()) if trusted == self.app.trusted => self.info(if trusted {
                    "this project is trusted"
                } else {
                    "this project is not trusted"
                }),
                Ok(()) => self.info(if trusted {
                    "this project is trusted from the next start of azazel: its hooks, extensions, MCP \
                     servers and permission settings load then"
                } else {
                    "this project is no longer trusted from the next start of azazel; what it loaded \
                     stays active until then"
                }),
                Err(e) => self.fail(&format!("could not save the decision: {e}")),
            };
        }
        let gated = needs_trust(paths, self.app.config.compat.claude);
        let mut out = match (self.app.trusted, store.decision(&paths.project_root)) {
            (true, Some(true)) => "this project is trusted".to_string(),
            (true, _) => "this project is trusted for this run (--trust)".to_string(),
            (false, Some(false)) => "this project is not trusted (you said no)".to_string(),
            (false, Some(true)) => "this project is trusted from the next start of azazel".to_string(),
            (false, None) => "this project is not trusted (no decision yet)".to_string(),
        };
        out.push_str(&format!("\n{}", paths.project_root.display()));
        if gated.is_empty() {
            out.push_str("\nnothing in it needs trust: it runs no code and widens no permissions");
        } else {
            out.push_str(if self.app.trusted {
                "\ntrust lets it use:"
            } else {
                "\nwithout trust these are skipped:"
            });
            for reason in &gated {
                out.push_str(&format!("\n  - {reason}"));
            }
        }
        if !self.app.ignored_settings.is_empty() {
            out.push_str(&format!(
                "\nskipped in this run: {}",
                self.app.ignored_settings.join(", ")
            ));
        }
        out.push_str("\n/trust yes or /trust no changes it; a restart applies the change");
        self.info(&out);
    }

    // ------------------------------------------------------------ /reload

    /// `/reload`: reads skills, custom commands, agent definitions, hooks, key bindings and the
    /// theme again, then the Luau scripts, and says what changed and what failed to load.
    pub(super) async fn reload(&mut self) {
        let app = self.app.clone();
        let registry = &app.registry;
        let mut report: Vec<String> = Vec::new();
        let mut problems: Vec<String> = Vec::new();
        let changed = |what: &str, count: usize, changes: String| {
            if changes.is_empty() {
                format!("{what}: {count}, unchanged")
            } else {
                format!("{what}: {count}, {changes}")
            }
        };

        // The configuration is read again for what follows from it: hooks and agent overrides.
        let (config, hook_sources) = match Config::load(&app.paths, &app.load_options) {
            Ok(loaded) => (
                Some(loaded.config),
                azazel_core::hooks::hook_sources(&loaded.layers),
            ),
            Err(e) => {
                problems.push(format!("configuration: {e}"));
                (None, Default::default())
            }
        };
        let compat = config
            .as_ref()
            .map_or(app.config.compat.claude, |c| c.compat.claude);
        let packages = Packages::new(&app.paths).load();
        if let Some(why) = &packages.problem {
            problems.push(format!("packages: {why}"));
        }

        // Skills.
        let names = |skills: &azazel_core::skills::SkillRegistry| -> Vec<String> {
            skills.list().into_iter().map(|skill| skill.name).collect()
        };
        let before = names(&registry.skills);
        registry.skills.reload();
        let after = names(&registry.skills);
        report.push(changed("skills", after.len(), name_changes(&before, &after)));
        for invalid in registry.skills.invalid() {
            problems.push(format!("{}: {}", invalid.path.display(), invalid.error));
        }

        // Custom commands: registered again from the files as they are now.
        let custom = CustomCommandsExt {
            roots: command_roots(&app.paths, compat, &packages),
            shell: app.shell.clone(),
            cwd: app.paths.cwd.clone(),
            trusted: app.trusted,
        };
        let command_names = |registry: &azazel_core::Registry| -> Vec<String> {
            registry.commands().into_iter().map(|c| c.name).collect()
        };
        let before = command_names(registry);
        registry.unload(custom.id());
        if let Err(e) = registry.load(&custom) {
            problems.push(format!("custom commands: {e}"));
        }
        let after = command_names(registry);
        let found = azazel_core::commands::discover(&custom.roots).len();
        report.push(changed("custom commands", found, name_changes(&before, &after)));

        // Agent definitions: every holder shares the one set, which takes the new files over.
        if let Some(team) = &self.team {
            let defs = team.defs();
            let def_names = |defs: &agents::AgentDefs| -> Vec<String> {
                defs.list().iter().map(|def| def.name.clone()).collect()
            };
            let before = def_names(defs);
            let mut fresh = agents::discover(
                azazel_team::BUILTIN_AGENTS,
                &agents::def_roots(&app.paths, compat, &packages),
            );
            fresh.apply_overrides(config.as_ref().map_or(&app.config.agents, |c| &c.agents));
            for invalid in fresh.invalid() {
                problems.push(format!("{}: {}", invalid.path.display(), invalid.error));
            }
            defs.replace(fresh);
            let after = def_names(defs);
            report.push(changed(
                "agent definitions",
                after.len(),
                name_changes(&before, &after),
            ));
        }

        // Hooks: the handlers are dropped and registered again from the new configuration.
        if let Some(config) = &config {
            let hooks = HooksExt {
                hooks: config.hooks.clone(),
                shell: app.shell.clone(),
                paths: app.paths.clone(),
                claude_compat: compat,
                disabled: app.hooks_disabled,
                trusted: app.trusted,
                sources: hook_sources,
            };
            registry.unload(hooks.id());
            match registry.load(&hooks) {
                Ok(()) if app.hooks_disabled => report.push("hooks: disabled for this run".to_string()),
                Ok(()) => {
                    let count: usize = config.hooks.values().map(Vec::len).sum();
                    report.push(format!("hooks: {count} from the configuration"));
                }
                Err(e) => problems.push(format!("hooks: {e}")),
            }
        }

        // Rule files.
        if let Some(rules) = registry.services.get::<azazel_core::rules::Rules>() {
            let loaded = rules.reload();
            report.push(format!("rule files: {}", loaded.rules.len()));
            for (file, why) in &loaded.problems {
                problems.push(format!("{file}: {why}"));
            }
        }

        // Key bindings and the theme.
        let mut keymap = Keymap::defaults();
        match std::fs::read_to_string(app.paths.keybindings_file()) {
            Ok(source) => match keymap.merge_toml(&source) {
                Ok(()) => {
                    self.keymap = keymap;
                    report.push("key bindings: read again".to_string());
                }
                Err(e) => problems.push(format!("{}: {e}", app.paths.keybindings_file().display())),
            },
            Err(_) => {
                self.keymap = keymap;
                report.push("key bindings: the defaults".to_string());
            }
        }
        let depth = self.theme.depth();
        let (theme, problem) = super::named_theme(&app, &self.theme_name);
        match problem {
            Some(problem) => problems.push(problem),
            None => {
                self.theme = theme.with_depth(depth);
                report.push(format!("theme: {}", self.theme_name));
            }
        }
        // The file list of `@` completion is read again on its next use.
        self.files = None;

        let mut out = format!("reloaded\n  {}", report.join("\n  "));
        if !problems.is_empty() {
            out.push_str(&format!("\nnot loaded:\n  {}", problems.join("\n  ")));
        }
        self.info(&out);
        azazel_core::ext::config_changed(&registry.services, "reload").await;
        // Last, the Luau scripts: their own command drops their state and loads them again.
        if registry.command("reload").is_some() {
            self.other_command("reload", "").await;
        }
    }
}

/// Whether an overlay takes text (and so the external editor key).
pub(super) fn is_text_input(overlay: &Overlay) -> bool {
    matches!(overlay.kind, Kind::Input { secret: false, .. })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_are_typed_as_kind_and_rule() {
        let (kind, rule) = parse_rule("allow shell(cargo test:*)").unwrap();
        assert_eq!(
            (kind, rule.to_string().as_str()),
            (RuleKind::Allow, "shell(cargo test:*)")
        );
        let (kind, rule) = parse_rule("  DENY   write(secrets/**) ").unwrap();
        assert_eq!(
            (kind, rule.to_string().as_str()),
            (RuleKind::Deny, "write(secrets/**)")
        );
        assert_eq!(parse_rule("ask web_fetch").unwrap().0, RuleKind::Ask);
        assert!(parse_rule("shell(ls)").unwrap_err().contains("for example"));
        assert!(parse_rule("permit shell").unwrap_err().contains("for example"));
        assert!(
            parse_rule("allow shell(")
                .unwrap_err()
                .contains("invalid permission rule")
        );
    }

    #[test]
    fn todo_edits_add_finish_and_clear() {
        let items = edit_todo(Vec::new(), "add write the tests").unwrap();
        let items = edit_todo(items, "add run them").unwrap();
        assert_eq!(todo_text(&items), " 1. [ ] write the tests\n 2. [ ] run them");
        let items = edit_todo(items, "done 1").unwrap();
        assert_eq!(items[0].status, TodoStatus::Done);
        assert_eq!(items[1].status, TodoStatus::Pending);
        assert!(
            edit_todo(items.clone(), "done 3")
                .unwrap_err()
                .contains("from 1 to 2")
        );
        assert!(edit_todo(items.clone(), "done x").is_err());
        assert!(edit_todo(items.clone(), "add").is_err());
        assert!(edit_todo(items.clone(), "shuffle").unwrap_err().contains("usage"));
        assert_eq!(edit_todo(items, "clear").unwrap(), Vec::new());
        assert!(todo_text(&[]).contains("empty"));
    }

    #[test]
    fn tools_group_by_where_they_come_from() {
        assert_eq!(tool_group("read", "core-tools"), "core");
        assert_eq!(tool_group("plan_submit", "permissions"), "core");
        assert_eq!(tool_group("task", "team"), "team");
        assert_eq!(
            tool_group("mcp__github__create_issue", "mcp"),
            "MCP server github"
        );
        assert_eq!(tool_group("deploy", "lua"), "scripts");
        assert_eq!(tool_group("memory_search", "memory"), "memory");
    }

    #[test]
    fn changes_between_name_lists_are_counted_and_named() {
        let list = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(name_changes(&list(&["a", "b"]), &list(&["a", "b"])), "");
        assert_eq!(name_changes(&list(&["a"]), &list(&["a", "b", "c"])), "+2 (b, c)");
        assert_eq!(
            name_changes(&list(&["a", "b"]), &list(&["b", "z"])),
            "+1 (z) −1 (a)"
        );
    }
}
