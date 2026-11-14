//! Commands that look into the running session and change it: `/tools`, `/jobs`, `/todo`,
//! `/context`, `/permissions`, `/forget-approvals`, `/trust` and `/reload`, plus what the keys of
//! overlays do.

use azazel_agent::ContextReport;
use azazel_core::agents;
use azazel_core::commands::CustomCommandsExt;
use azazel_core::hooks::HooksExt;
use azazel_core::packages::Packages;
use azazel_core::permissions::{Rule, RuleKind, SESSION, SourcedRule};
use azazel_core::{Config, Extension};
use azazel_tools::Jobs;
use azazel_tui::components::format_tokens;
use azazel_tui::components::overlay::Frame;
use azazel_tui::{Glyph, KeyCombo, Keymap, Role, SelectItem, Theme, modal, text};
use crossterm::event::{KeyCode, KeyModifiers};

use super::overlay::{Action, Kind, Overlay};
use super::pager::{self, Source, Start};
use super::{Tui, first_line};
use crate::context;

/// The most cells a category's bar takes in the `/context` box, and the fewest worth drawing.
const CONTEXT_BAR: usize = 20;
const CONTEXT_BAR_MIN: usize = 6;
/// Columns of a row's numbers: `  1.2k  31%`.
const CONTEXT_NUMBERS: usize = 11;
/// The widest the list makes its label column, and what a row takes besides label and detail.
const CONTEXT_LABELS: usize = 32;
const CONTEXT_ROW_CHROME: usize = 5;

/// The rows of the `/context` box for a list `room` columns wide: each category with its
/// tokens, its share, a bar of that share and what it counts. A narrow list drops the count,
/// then the bar, so that the numbers stay.
pub(super) fn context_items(report: &ContextReport, theme: &Theme, room: usize) -> Vec<SelectItem> {
    let labels: Vec<String> = report.categories.iter().map(context::label).collect();
    let counts: Vec<String> = report.categories.iter().map(context::counted).collect();
    let column = labels
        .iter()
        .map(|label| text::width(label))
        .max()
        .unwrap_or(0)
        .min(CONTEXT_LABELS);
    let count_width = counts.iter().map(|count| text::width(count)).max().unwrap_or(0);
    let avail = room.saturating_sub(column + CONTEXT_ROW_CHROME);
    let with_bar = CONTEXT_NUMBERS + 2 + CONTEXT_BAR_MIN;
    let (cells, with_count) = if avail >= with_bar + 2 + count_width {
        ((avail - CONTEXT_NUMBERS - 4 - count_width).min(CONTEXT_BAR), true)
    } else if avail >= with_bar {
        ((avail - CONTEXT_NUMBERS - 2).min(CONTEXT_BAR), false)
    } else {
        (0, false)
    };
    report
        .categories
        .iter()
        .zip(labels)
        .zip(counts)
        .map(|((category, label), count)| {
            let numbers = format!(
                "{:>6} {:>3}%",
                format_tokens(category.tokens),
                context::percent(category, report)
            );
            if avail < CONTEXT_NUMBERS {
                // No room to line anything up: the numbers as short as they go, for the list to
                // cut or leave out as it must.
                let short = format!(
                    "{} {}%",
                    format_tokens(category.tokens),
                    context::percent(category, report)
                );
                return SelectItem::new(label, short);
            }
            let mut detail = format!("{}{numbers}", theme.fg(Role::Text));
            if cells > 0 {
                let filled = context::bar_cells(category, report, cells);
                detail.push_str(&format!(
                    "  {}{}{}{}",
                    theme.fg(Role::Accent),
                    theme.glyph(Glyph::ProgressFull).repeat(filled),
                    theme.fg(Role::BorderMuted),
                    theme.glyph(Glyph::ProgressEmpty).repeat(cells - filled),
                ));
            }
            if with_count {
                detail.push_str(&format!("  {}{count}", theme.fg(Role::Muted)));
            }
            SelectItem::with_preview(label, detail)
        })
        .collect()
}

/// The keys of the agent hub, in the order [`Tui::overlay_hotkey`] reads them.
pub(super) fn hub_keys() -> Vec<(KeyCombo, &'static str)> {
    let plain = |c| KeyCombo::new(KeyCode::Char(c), KeyModifiers::NONE);
    vec![
        (plain('m'), "message"),
        (plain('s'), "stop"),
        (plain('S'), "stop all"),
        (plain('o'), "open"),
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
                if let Some(key) = ['m', 's', 'S', 'o'].get(key) {
                    self.hub_key(*key, &id).await;
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
            Action::Context(report) => self.read_context(*report, choice).await,
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
                let jobs = self
                    .app("/jobs")
                    .and_then(|app| app.registry.services.get::<Jobs>());
                let Some(jobs) = jobs else {
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
                    let Some(app) = self.app("/permissions") else {
                        return;
                    };
                    app.permissions.add_session_rule(rule.clone(), kind);
                    self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
                    self.open_permissions();
                }
                Err(why) => self.fail(&why),
            },
            Action::PermissionScope(rule, kind) => {
                let Some(app) = self.app("/permissions") else {
                    return;
                };
                if choice == 0 {
                    app.permissions.add_session_rule(rule.clone(), kind);
                    self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
                } else {
                    match app.permissions.remember_allow(&rule) {
                        Ok(()) => self.info(&format!("allow {rule} is saved for this project")),
                        Err(e) => self.fail(&format!("could not save the rule: {e}")),
                    }
                }
                self.open_permissions();
            }
            _ => {}
        }
    }

    // ------------------------------------------------------------ /context

    /// `/context`: what the lead's last request was made of, a row per category; choosing one
    /// shows its exact text full screen. Before any request there is nothing to take apart, and
    /// what is known is said instead.
    pub(super) async fn open_context(&mut self) {
        let report = match self.backend.report("context", "json").await {
            Some(Ok(json)) => serde_json::from_str::<ContextReport>(&json).ok(),
            _ => None,
        };
        match report {
            Some(report) if !report.categories.is_empty() => self.show_context(report, 0, None),
            _ => match self.backend.report("context", "").await {
                Some(Ok(text)) => self.info(&text),
                Some(Err(why)) => self.fail(&why),
                None => {}
            },
        }
    }

    /// Opens the `/context` box on `report` with row `at` selected; `note` is said above it.
    fn show_context(&mut self, report: ContextReport, at: usize, note: Option<String>) {
        let room = Frame::inner_width(modal::box_width(self.width()));
        let items = context_items(&report, &self.theme, room);
        let mut body = context::headline(&report);
        body.extend(note);
        self.overlay = Some(
            Overlay::select("Context", body, items, false, Action::Context(Box::new(report)))
                .with_enter("read")
                .starting_at(at),
        );
    }

    /// Shows the text of category `index` of `report` full screen, then the box again. When the
    /// agent has sent another request meanwhile, the box opens on that one instead.
    async fn read_context(&mut self, report: ContextReport, index: usize) {
        let Some(category) = report.categories.get(index) else {
            return;
        };
        let asked = format!("text {} {index}", report.request);
        match self.backend.report("context", &asked).await {
            Some(Ok(text)) => {
                let title = format!("Context: {}", context::label(category));
                let about = format!(
                    "{} tokens ({}% of request {}), {}",
                    format_tokens(category.tokens),
                    context::percent(category, &report),
                    report.request,
                    context::counted(category)
                );
                let sources = vec![Source::Note(about), Source::Text(text)];
                pager::show(self, &title, sources, Start::Top).await;
                self.show_context(report, index, None);
            }
            Some(Err(why)) => {
                let newer = match self.backend.report("context", "json").await {
                    Some(Ok(json)) => serde_json::from_str::<ContextReport>(&json).ok(),
                    _ => None,
                };
                match newer.filter(|newer| newer.request != report.request) {
                    Some(newer) => {
                        let note = format!(
                            "The agent has sent request {} since; this is what it is made of.",
                            newer.request
                        );
                        self.show_context(newer, 0, Some(note));
                    }
                    None => self.fail(&why),
                }
            }
            None => {}
        }
    }

    // ------------------------------------------------------------ /jobs

    /// `/jobs`: the background jobs; choosing one offers its output or stops it.
    pub(super) fn open_jobs(&mut self) {
        let Some(app) = self.app("/jobs") else {
            return;
        };
        let entries = app
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

    // ------------------------------------------------------------ permissions and trust

    /// `/permissions`: the rules in effect with where each comes from. `d` removes one that was
    /// added here or remembered, `a` adds one.
    pub(super) fn open_permissions(&mut self) {
        let Some(app) = self.app("/permissions") else {
            return;
        };
        let rules = app.permissions.rules();
        let local = app.paths.project_local_config().display().to_string();
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
            format!("mode: {}", app.permissions.mode()),
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
                let Some(app) = self.app("/permissions") else {
                    return;
                };
                app.permissions.add_session_rule(rule.clone(), kind);
                self.info(&format!("{} {rule} applies until azazel exits", kind_word(kind)));
            }
            Err(why) => self.fail(&why),
        }
    }

    fn remove_rule(&mut self, sourced: &SourcedRule) {
        let Some(app) = self.app("/permissions") else {
            return;
        };
        let local = app.paths.project_local_config().display().to_string();
        let label = format!("{} {}", kind_word(sourced.kind), sourced.rule);
        if sourced.source == SESSION {
            app.permissions.remove_session_rule(&sourced.rule, sourced.kind);
            self.info(&format!("removed {label}"));
        } else if sourced.source == local && sourced.kind == RuleKind::Allow {
            match app.permissions.forget_allow(&sourced.rule) {
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

    // ------------------------------------------------------------ /reload

    /// `/reload`: reads skills, custom commands, agent definitions, hooks, key bindings and the
    /// theme again, then the Luau scripts, and says what changed and what failed to load.
    pub(super) async fn reload(&mut self) {
        let Some(app) = self.app("/reload") else {
            return;
        };
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
            roots: app.run.command_roots(&app.paths, compat, &packages),
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
        if let Some(team) = registry.services.get::<azazel_team::Team>() {
            let defs = team.defs();
            let def_names = |defs: &agents::AgentDefs| -> Vec<String> {
                defs.list().iter().map(|def| def.name.clone()).collect()
            };
            let before = def_names(defs);
            let mut fresh = agents::discover(
                azazel_team::BUILTIN_AGENTS,
                &app.run.def_roots(&app.paths, compat, &packages),
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
        let (theme, problem) = self.theme_named(&self.theme_name);
        match problem {
            Some(problem) => problems.push(problem),
            None => {
                let glyphs = self.theme.glyphs().level();
                self.theme = theme
                    .with_depth(depth)
                    .with_links(self.caps.links)
                    .with_glyphs(glyphs);
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
            self.other_command("reload", "");
        }
    }
}

/// Whether an overlay takes text (and so the external editor key).
pub(super) fn is_text_input(overlay: &Overlay) -> bool {
    match &overlay.kind {
        Kind::Input { secret, .. } => !secret,
        Kind::Form(form) => form.is_typing(),
        Kind::Pane(_) => true,
        Kind::Select { .. } | Kind::Settings(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::{Category, CategoryKind};
    use azazel_tui::{GlyphLevel, SelectList};

    use super::*;

    fn context_report() -> ContextReport {
        let category = |kind, name: &str, items, tokens| Category {
            kind,
            name: name.to_string(),
            items,
            estimate: tokens,
            tokens,
        };
        ContextReport {
            agent: "lead".into(),
            request: 4,
            model: Some("mock/lead".into()),
            window: 200_000,
            estimate: 40_000,
            reported: Some(40_000),
            tokens: 40_000,
            categories: vec![
                category(CategoryKind::System, "identity", 1, 500),
                category(CategoryKind::System, "context_files", 1, 3_500),
                category(CategoryKind::Tools, "", 24, 8_000),
                category(CategoryKind::Tools, "github", 120, 16_000),
                category(CategoryKind::Messages, "user", 2, 0),
                category(CategoryKind::ToolResults, "", 9, 10_000),
                category(CategoryKind::Images, "", 1, 2_000),
            ],
        }
    }

    #[test]
    fn the_context_rows_fit_any_width_and_drop_the_count_then_the_bar_then_line_up_no_more() {
        let report = context_report();
        for level in [GlyphLevel::Unicode, GlyphLevel::Nerd, GlyphLevel::Ascii] {
            let theme = Theme::dark().with_glyphs(level);
            let full = theme.glyph(Glyph::ProgressFull);
            for width in 20..=160 {
                let items = context_items(&report, &theme, width);
                assert_eq!(items.len(), report.categories.len());
                let view = SelectList::new(items).view(width, 16, &theme);
                let rows: Vec<String> = view.lines.iter().map(|line| text::strip_ansi(line)).collect();
                assert_eq!(rows.len(), 7, "{level:?} at {width}");
                for row in &rows {
                    assert!(text::width(row) <= width, "{level:?} at {width}: {row:?}");
                }
                // The numbers are there as soon as a row can hold them beside its label.
                if width >= 24 + CONTEXT_ROW_CHROME + CONTEXT_NUMBERS {
                    assert!(
                        rows[3].contains("16.0k  40%"),
                        "{level:?} at {width}: {:?}",
                        rows[3]
                    );
                    assert!(rows[4].contains("0   0%"), "{level:?} at {width}: {:?}", rows[4]);
                }
                let bars: Vec<usize> = rows.iter().map(|row| row.matches(full).count()).collect();
                if width >= 24 + CONTEXT_ROW_CHROME + CONTEXT_NUMBERS + 2 + CONTEXT_BAR_MIN {
                    // Something that holds tokens always shows at least one cell; nothing shows none.
                    assert!(
                        bars[0] >= 1 && bars[3] > bars[2] && bars[2] >= bars[1],
                        "{bars:?} at {width}"
                    );
                    assert_eq!(bars[4], 0);
                } else if level != GlyphLevel::Ascii {
                    assert_eq!(bars, [0; 7], "{level:?} at {width}");
                }
                let counted = rows[3].contains("120 tools");
                assert_eq!(
                    counted,
                    width >= 24 + CONTEXT_ROW_CHROME + CONTEXT_NUMBERS + 2 + CONTEXT_BAR_MIN + 2 + 9,
                    "{level:?} at {width}: {:?}",
                    rows[3]
                );
            }
            // With room to spare the bar is twenty cells, shared out by tokens: 40 % is eight.
            let items = context_items(&report, &theme, 120);
            let wide = text::strip_ansi(&items[3].detail);
            assert_eq!(wide.matches(full).count(), 8, "{wide}");
            assert_eq!(
                text::width(&wide),
                CONTEXT_NUMBERS + 2 + CONTEXT_BAR + 2 + "120 tools".len()
            );
        }
    }

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
