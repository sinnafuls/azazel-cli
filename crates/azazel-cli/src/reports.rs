//! What a session can say about itself in a few lines: `/cost`, `/context`, `/session`, `/tools`,
//! `/todo`, `/dirs`, `/add-dir`, `/rules`, `/hooks`, `/doctor`, `/trust` and `/forget-approvals`.
//!
//! Each is text made from the session, so every front end shows the same: the terminal UI
//! prints it, and `run_command` of the RPC protocol answers with it, which is also how a UI
//! attached to a session in another process gets it.

use std::collections::BTreeMap;
use std::path::PathBuf;

use azazel_ai::Usage;
use azazel_core::SessionStore;
use azazel_core::session::SessionFacts;
use azazel_core::side::Side;
use azazel_core::trust::{TrustStore, needs_trust};
use azazel_tools::{TodoItem, TodoStatus, TodoStore};
use azazel_tui::components::format_tokens;

use crate::app::{Active, App};
use crate::sessions::local_time;

/// The reports by name, each with what `/help` says about it.
pub const REPORTS: &[(&str, &str)] = &[
    ("cost", "Tokens and money spent in this session"),
    (
        "context",
        "What the last request was made of: tokens by category, and the text of each",
    ),
    (
        "session",
        "What this session is: file, messages, tokens, cost, agents",
    ),
    (
        "tools",
        "List the tools offered to the agent, by where they come from",
    ),
    (
        "todo",
        "The agent's task list: /todo, /todo add <text>, /todo done <n>, /todo clear",
    ),
    ("dirs", "List the directories agents may work in"),
    (
        "add-dir",
        "Let agents work in another directory too: /add-dir <path>",
    ),
    (
        "rules",
        "List the rule files and whether each has applied in this session",
    ),
    (
        "hooks",
        "List the hooks, where they come from and how each last ran",
    ),
    ("doctor", "Check the installation and this project's setup"),
    (
        "trust",
        "Whether this project is trusted, and what that covers: /trust [yes|no]",
    ),
    (
        "forget-approvals",
        "Forget every remembered approval and every rule added in this session",
    ),
];

/// Runs the report `name`. `None` when there is no report of that name; `Err` is what to tell
/// the user instead of a report.
pub async fn run(app: &App, active: &Active, name: &str, args: &str) -> Option<Result<String, String>> {
    let args = args.trim();
    Some(match name {
        "cost" => Ok(cost(app, active)),
        "context" => crate::context::report(app, active, args),
        "session" => Ok(session(app, active).await),
        "tools" => Ok(tools(app, active)),
        "todo" => todo(app, active, args),
        "dirs" => Ok(dirs(app)),
        "add-dir" => add_dir(app, args),
        "rules" => Ok(crate::rules::session_report(app, &active.session.id())),
        "hooks" => Ok(crate::hooks::report(app)),
        "doctor" => Ok(crate::doctor::examine(&app.paths, app.trusted).await.render()),
        "trust" => trust(app, args),
        "forget-approvals" => forget_approvals(app),
        _ => return None,
    })
}

/// The model the lead answers with: the one that answered last, else the first its chain
/// resolves to.
pub fn model_label(app: &App, active: &Active) -> String {
    let status = active.lead.status();
    match status.model {
        Some(model) => model.to_string(),
        None => app
            .router
            .resolve(&status.chain)
            .first()
            .map(|model| model.to_string())
            .unwrap_or_default(),
    }
}

fn cost(app: &App, active: &Active) -> String {
    let status = active.lead.status();
    let usage = status.usage;
    let cost = match usage.cost_usd {
        Some(cost) => format!("${cost:.4}"),
        None => "unknown (no price is known for this model)".to_string(),
    };
    let cost = match app.router.tier() {
        Some(tier) => format!("{cost}\n{}", crate::routing::tier_cost_note(tier)),
        None => cost,
    };
    let cost = match crate::side::cost_report(&app.registry.services) {
        Some(side) => format!("{cost}\n{side}"),
        None => cost,
    };
    format!(
        "requests: {}\ninput: {} · output: {} · cache read: {} · cache write: {} · reasoning: {}\ncost: {cost}",
        status.requests,
        format_tokens(usage.input),
        format_tokens(usage.output),
        format_tokens(usage.cache_read),
        format_tokens(usage.cache_write),
        format_tokens(usage.reasoning),
    )
}

fn usage_text(usage: &Usage) -> String {
    let cost = match usage.cost_usd {
        Some(cost) => format!("${cost:.4}"),
        None => "no price known".to_string(),
    };
    format!(
        "{} in · {} out · {} cache read · {} cache write · {cost}",
        format_tokens(usage.input),
        format_tokens(usage.output),
        format_tokens(usage.cache_read),
        format_tokens(usage.cache_write),
    )
}

/// Everything `/session` says.
struct SessionReport {
    file: PathBuf,
    id: String,
    name: Option<String>,
    pinned: bool,
    facts: SessionFacts,
    /// Messages the model sees on the current branch.
    in_context: usize,
    chain: String,
    model: String,
    thinking: String,
    agents_usage: Usage,
    side: BTreeMap<String, Usage>,
    checkpoints: String,
}

impl SessionReport {
    fn render(&self) -> String {
        let facts = &self.facts;
        let mut rows: Vec<(&str, String)> = vec![
            (
                "session",
                self.name
                    .clone()
                    .unwrap_or_else(|| "(no name yet; /name gives it one)".to_string()),
            ),
            ("id", self.id.clone()),
            ("file", self.file.display().to_string()),
            (
                "created",
                format!(
                    "{} · last used {}",
                    local_time(&facts.created),
                    local_time(&facts.updated)
                ),
            ),
        ];
        if let Some(parent) = &facts.parent_session {
            rows.push(("continues", parent.clone()));
        }
        if self.pinned {
            rows.push(("pinned", "yes".to_string()));
        }
        rows.push((
            "messages",
            format!(
                "{} from you · {} replies · {} tool results · {} in the model's context",
                facts.user, facts.assistant, facts.tool, self.in_context
            ),
        ));
        rows.push((
            "branches",
            match facts.branch_points {
                0 => "the conversation never branched".to_string(),
                1 => "1 branch point; /tree shows it".to_string(),
                n => format!("{n} branch points; /tree shows them"),
            },
        ));
        if facts.compactions > 0 {
            rows.push(("compacted", format!("{} times", facts.compactions)));
        }
        let model = if self.model.is_empty() || self.model == self.chain {
            self.chain.clone()
        } else {
            format!("{} (now {})", self.chain, self.model)
        };
        rows.push(("model", format!("{model} · thinking {}", self.thinking)));
        rows.push(("tokens", format!("lead: {}", usage_text(&facts.usage))));
        if !facts.agents.is_empty() {
            rows.push(("", format!("agents: {}", usage_text(&self.agents_usage))));
        }
        if !self.side.is_empty() {
            let side: Vec<String> = self
                .side
                .iter()
                .map(|(purpose, usage)| match usage.cost_usd {
                    Some(cost) => format!("{purpose} ${cost:.4}"),
                    None => format!("{purpose} {} tokens", format_tokens(usage.input + usage.output)),
                })
                .collect();
            rows.push(("", format!("on the side, in this run: {}", side.join(", "))));
        }
        rows.push(("checkpoints", self.checkpoints.clone()));
        rows.push((
            "agents",
            if facts.agents.is_empty() {
                "none run".to_string()
            } else {
                let runs: Vec<String> = facts
                    .agents
                    .iter()
                    .map(|run| format!("{} ({}, {})", run.id, run.def, run.status))
                    .collect();
                format!("{} run: {}", runs.len(), runs.join(", "))
            },
        ));
        rows.iter()
            .map(|(what, value)| format!("{what:<12}{value}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// `/session`: what the open session is and what it holds.
async fn session(app: &App, active: &Active) -> String {
    let session = &active.session;
    let facts = session.facts();
    let status = active.lead.status();
    let mut agents_usage = Usage::default();
    for run in &facts.agents {
        if let Some(usage) = SessionStore::usage_of(&run.transcript) {
            agents_usage.add(&usage);
        }
    }
    let checkpoints = match active.checkpoints.disabled().await {
        Some(reason) => format!("off: {reason}"),
        None => format!("on · {} taken; /rewind goes back to one", facts.checkpoints),
    };
    let report = SessionReport {
        file: session.path(),
        id: session.id(),
        name: session.name(),
        pinned: session.pinned(),
        in_context: session.build_context(None).len(),
        chain: status
            .chain
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
        model: model_label(app, active),
        thinking: status.thinking.to_string(),
        agents_usage,
        side: app
            .registry
            .services
            .get::<Side>()
            .map(|side| side.costs.by_purpose())
            .unwrap_or_default(),
        checkpoints,
        facts,
    };
    report.render()
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

/// `/todo` edits as changes to a list. `Err` is what to tell the user.
fn edit_todo(mut items: Vec<TodoItem>, args: &str) -> Result<Vec<TodoItem>, String> {
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

/// `/tools`: every tool the lead could be offered, grouped by where it comes from.
fn tools(app: &App, active: &Active) -> String {
    let lead = active.lead.info();
    let listing = app.registry.tools.listing(lead);
    #[cfg(feature = "mcp")]
    let deferred: Vec<String> = app
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
        let mark = if !tool.offered {
            " (disabled)"
        } else if app.coordinator.is_on() && !azazel_team::coordinator::offers(&tool.name) {
            " (withheld in coordinator mode)"
        } else {
            offered += 1;
            ""
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
    out.trim_end().to_string()
}

/// `/todo`, `/todo add <text>`, `/todo done <n>`, `/todo clear`: the lead's task list.
fn todo(app: &App, active: &Active, args: &str) -> Result<String, String> {
    let Some(store) = app.registry.services.get::<TodoStore>() else {
        return Ok("this session has no todo list".to_string());
    };
    let lead = &active.lead.id;
    let items = store.get(lead);
    if args.is_empty() {
        return Ok(todo_text(&items));
    }
    let items = edit_todo(items, args)?;
    let shown = todo_text(&items);
    // Recorded, so that the list is as the user left it when the session is opened again.
    active.session.append(azazel_tools::todo_entry(&items));
    store.set(lead, items);
    Ok(shown)
}

fn dirs(app: &App) -> String {
    let roots: Vec<String> = app
        .permissions
        .roots()
        .iter()
        .map(|root| root.display().to_string())
        .collect();
    format!("agents may work in:\n{}", roots.join("\n"))
}

fn add_dir(app: &App, args: &str) -> Result<String, String> {
    if args.is_empty() {
        return Err("usage: /add-dir <path>".to_string());
    }
    let dir = azazel_core::paths::canonical(&app.paths.cwd.join(args));
    if !dir.is_dir() {
        Err(format!("{} is not a directory", dir.display()))
    } else if app.permissions.add_root(&dir) {
        Ok(format!(
            "agents may now work in {} too, until azazel exits",
            dir.display()
        ))
    } else {
        Ok(format!("{} is already a directory agents work in", dir.display()))
    }
}

/// `/forget-approvals`: drops every remembered approval and every rule of this session.
fn forget_approvals(app: &App) -> Result<String, String> {
    match app.permissions.clear_remembered() {
        Ok(0) => Ok("there were no remembered approvals or session rules".to_string()),
        Ok(1) => Ok("forgot 1 rule; the agent asks again from now on".to_string()),
        Ok(count) => Ok(format!("forgot {count} rules; the agent asks again from now on")),
        Err(e) => Err(format!(
            "could not edit {}: {e}",
            app.paths.project_local_config().display()
        )),
    }
}

/// `/trust`: whether this project is trusted and what that covers; `/trust yes|no` decides.
fn trust(app: &App, args: &str) -> Result<String, String> {
    let paths = &app.paths;
    let store = TrustStore::new(paths.trust_file());
    let decide = match args.to_ascii_lowercase().as_str() {
        "" => None,
        "yes" | "y" | "on" => Some(true),
        "no" | "n" | "off" => Some(false),
        _ => return Err("usage: /trust, /trust yes or /trust no".to_string()),
    };
    if let Some(trusted) = decide {
        return match store.set(&paths.project_root, trusted) {
            Ok(()) if trusted == app.trusted => Ok(if trusted {
                "this project is trusted"
            } else {
                "this project is not trusted"
            }
            .to_string()),
            Ok(()) => Ok(if trusted {
                "this project is trusted from the next start of azazel: its hooks, extensions, MCP \
                 servers and permission settings load then"
            } else {
                "this project is no longer trusted from the next start of azazel; what it loaded \
                 stays active until then"
            }
            .to_string()),
            Err(e) => Err(format!("could not save the decision: {e}")),
        };
    }
    let gated = needs_trust(paths, app.config.compat.claude, &app.config.tools);
    let mut out = match (app.trusted, store.decision(&paths.project_root)) {
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
        out.push_str(if app.trusted {
            "\ntrust lets it use:"
        } else {
            "\nwithout trust these are skipped:"
        });
        for reason in &gated {
            out.push_str(&format!("\n  - {reason}"));
        }
    }
    if !app.ignored_settings.is_empty() {
        out.push_str(&format!(
            "\nskipped in this run: {}",
            app.ignored_settings.join(", ")
        ));
    }
    out.push_str("\n/trust yes or /trust no changes it; a restart applies the change");
    Ok(out)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_core::session::AgentRun;

    use super::*;

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
    fn the_session_report_names_what_the_session_holds() {
        let mut facts = SessionFacts {
            created: "2026-11-12T10:00:00Z".to_string(),
            updated: "2026-11-12T12:30:00Z".to_string(),
            user: 3,
            assistant: 5,
            tool: 12,
            branch_points: 2,
            checkpoints: 3,
            ..SessionFacts::default()
        };
        facts.usage = Usage {
            input: 12_000,
            output: 2_000,
            cost_usd: Some(0.25),
            ..Usage::default()
        };
        facts.agents.push(AgentRun {
            id: "scout-1".to_string(),
            def: "scout".to_string(),
            status: "done".to_string(),
            transcript: PathBuf::from("scout-1.jsonl"),
        });
        let mut report = SessionReport {
            file: PathBuf::from("sessions/s.jsonl"),
            id: "0199-abc".to_string(),
            name: Some("parser rework".to_string()),
            pinned: true,
            facts,
            in_context: 8,
            chain: "@lead".to_string(),
            model: "mock/lead".to_string(),
            thinking: "high".to_string(),
            agents_usage: Usage {
                input: 500,
                output: 100,
                ..Usage::default()
            },
            side: BTreeMap::from([(
                "handoff".to_string(),
                Usage {
                    cost_usd: Some(0.01),
                    ..Usage::default()
                },
            )]),
            checkpoints: "on · 3 taken".to_string(),
        };
        let shown = report.render();
        for expected in [
            "session     parser rework",
            "id          0199-abc",
            "pinned      yes",
            "3 from you · 5 replies · 12 tool results · 8 in the model's context",
            "2 branch points",
            "model       @lead (now mock/lead) · thinking high",
            "tokens      lead: 12.0k in · 2.0k out · 0 cache read · 0 cache write · $0.2500",
            "agents: 500 in · 100 out · 0 cache read · 0 cache write · no price known",
            "on the side, in this run: handoff $0.0100",
            "checkpoints on · 3 taken",
            "agents      1 run: scout-1 (scout, done)",
        ] {
            assert!(shown.contains(expected), "missing `{expected}` in:\n{shown}");
        }
        assert!(!shown.contains("continues"));

        report.name = None;
        report.pinned = false;
        report.facts.agents.clear();
        report.facts.branch_points = 0;
        report.facts.parent_session = Some("sessions/old.jsonl".to_string());
        let shown = report.render();
        assert!(shown.contains("(no name yet; /name gives it one)"), "{shown}");
        assert!(shown.contains("continues   sessions/old.jsonl"), "{shown}");
        assert!(
            shown.contains("never branched") && shown.contains("none run"),
            "{shown}"
        );
        assert!(!shown.contains("pinned") && !shown.contains("agents:"), "{shown}");
    }
}
