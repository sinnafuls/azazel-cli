//! What the code registers, by name: the side of the comparison that is always right.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::{Context, Result, anyhow};
use clap::CommandFactory;
use serde_json::Value;

/// A `--flag` of one command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Flag {
    pub takes_value: bool,
    /// Declared on the root command and accepted after every subcommand.
    pub global: bool,
    pub hidden: bool,
    /// Its one-letter form, when it has one.
    pub short: Option<char>,
}

/// `azazel` itself (an empty path) or one of its subcommands.
#[derive(Debug, Clone, Default)]
pub struct CliCommand {
    pub path: Vec<String>,
    pub hidden: bool,
    /// By `--long-name`, aliases included (those as hidden flags).
    pub flags: BTreeMap<String, Flag>,
    /// Short flags, and whether each takes a value.
    pub shorts: BTreeMap<char, bool>,
    /// Every name a subcommand answers to, with the name it is listed under.
    pub subcommands: BTreeMap<String, String>,
}

impl CliCommand {
    /// `azazel sessions prune`.
    pub fn name(&self) -> String {
        std::iter::once("azazel")
            .chain(self.path.iter().map(String::as_str))
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// The subcommands as they are listed, without aliases.
    pub fn actions(&self) -> BTreeSet<String> {
        self.subcommands.values().cloned().collect()
    }
}

/// Everything the docs are compared with.
#[derive(Debug, Clone, Default)]
pub struct Code {
    /// The version of the workspace.
    pub version: String,
    /// `table.key` to its default as a TOML literal; see `azazel_core::config_keys`.
    pub config: BTreeMap<String, Option<String>>,
    pub trust_gated: BTreeSet<String>,
    pub hook_events: BTreeSet<String>,
    /// Keymap action to the keys bound to it by default, as the keymap writes them (`ctrl+l`).
    pub key_actions: BTreeMap<String, BTreeSet<String>>,
    pub cli: Vec<CliCommand>,
    pub rpc_commands: BTreeSet<String>,
    pub rpc_frames: BTreeSet<String>,
    pub rpc_ui_kinds: BTreeSet<String>,
    /// What `docs/rpc.schema.json` must hold.
    pub rpc_schema: String,
    /// `azazel.fs.read`, `azazel.fs.data_dir`, …
    pub luau_api: BTreeSet<String>,
    pub luau_events: BTreeSet<String>,
    pub slash_commands: BTreeSet<String>,
    /// Tools of a new session, and those registered only under a condition.
    pub tools: BTreeSet<String>,
    /// The tasks of `cargo xtask`.
    pub xtasks: BTreeSet<String>,
}

impl Code {
    pub fn command(&self, path: &[String]) -> Option<&CliCommand> {
        self.cli.iter().find(|command| command.path == path)
    }
}

fn collect_cli(command: &clap::Command, path: Vec<String>, hidden: bool, out: &mut Vec<CliCommand>) {
    let mut entry = CliCommand {
        path: path.clone(),
        hidden,
        ..CliCommand::default()
    };
    for arg in command.get_arguments() {
        let flag = Flag {
            takes_value: arg.get_action().takes_values(),
            global: arg.is_global_set(),
            hidden: arg.is_hide_set(),
            short: arg.get_short(),
        };
        if let Some(long) = arg.get_long() {
            entry.flags.insert(format!("--{long}"), flag.clone());
            for alias in arg.get_all_aliases().into_iter().flatten() {
                let alias_flag = Flag {
                    hidden: true,
                    short: None,
                    ..flag.clone()
                };
                entry.flags.entry(format!("--{alias}")).or_insert(alias_flag);
            }
        }
        if let Some(short) = arg.get_short() {
            entry.shorts.insert(short, flag.takes_value);
        }
    }
    for sub in command.get_subcommands() {
        // `help` is clap's own.
        if sub.get_name() == "help" {
            continue;
        }
        let name = sub.get_name().to_string();
        entry.subcommands.insert(name.clone(), name.clone());
        for alias in sub.get_all_aliases() {
            entry.subcommands.insert(alias.to_string(), name.clone());
        }
        let mut below = path.clone();
        below.push(name);
        collect_cli(sub, below, hidden || sub.is_hide_set(), out);
    }
    out.push(entry);
}

/// The command line as clap built it, the root first.
fn cli() -> Vec<CliCommand> {
    let mut root = azazel_cli::cli::Cli::command();
    root.build();
    let mut out = Vec::new();
    collect_cli(&root, Vec::new(), false, &mut out);
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

/// Every action of the default keymap with its keys.
fn key_actions() -> BTreeMap<String, BTreeSet<String>> {
    let keymap = azazel_tui::keys::Keymap::defaults();
    keymap
        .actions()
        .map(|action| {
            let keys = keymap.keys_for(action).iter().map(ToString::to_string).collect();
            (action.to_string(), keys)
        })
        .collect()
}

/// The `type` (or another tag) of every variant of a tagged union in a JSON Schema.
fn variants(schema: &Value, tag: &str) -> BTreeSet<String> {
    schema["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|variant| variant["properties"][tag]["const"].as_str())
        .map(str::to_string)
        .collect()
}

/// The functions and fields of the `azazel` global, and the events `azazel.on` takes, from the
/// type declarations scripts are checked against.
pub fn luau(declarations: &str) -> (BTreeSet<String>, BTreeSet<String>) {
    let mut api = BTreeSet::new();
    let mut events = BTreeSet::new();
    // The path of the tables the line is inside; empty outside `declare azazel`.
    let mut inside: Vec<String> = Vec::new();
    let mut on = false;
    for line in declarations.lines() {
        let text = line.trim();
        // `type On` is one paragraph: an overload of `azazel.on` per event.
        if text.starts_with("type On") {
            on = true;
        } else if text.is_empty() {
            on = false;
        }
        if on && let Some((_, tail)) = text.split_once("event:") {
            let literals = tail.split(", handler").next().unwrap_or_default();
            events.extend(literals.split('"').skip(1).step_by(2).map(str::to_string));
        }
        if text.starts_with("declare azazel:") {
            inside = vec!["azazel".to_string()];
            continue;
        }
        if inside.is_empty() || text.starts_with("--") {
            continue;
        }
        // A member is one tab deeper than the table it belongs to, and the table's closing brace
        // is at the table's own depth; deeper lines are the insides of a member's type.
        let depth = line.chars().take_while(|c| *c == '\t').count();
        if text.starts_with('}') {
            if depth + 1 == inside.len() {
                inside.pop();
            }
            continue;
        }
        let Some((name, value)) = text.split_once(':') else {
            continue;
        };
        let plain = !name.is_empty() && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
        if !plain || depth != inside.len() {
            continue;
        }
        if value.trim() == "{" {
            inside.push(name.to_string());
        } else {
            api.insert(format!("{}.{name}", inside.join(".")));
        }
    }
    (api, events)
}

/// Reads every registry. `root` is the workspace; the application is built in an empty scratch
/// home and project, so nothing of this machine's configuration is seen.
pub async fn gather(root: &Path) -> Result<Code> {
    let scratch = tempfile::tempdir()?;
    let (home, project) = (scratch.path().join("home"), scratch.path().join("project"));
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&project)?;
    let inventory = azazel_cli::inventory::default_app(&home, &project)
        .await
        .context("could not build the application to list its commands and tools")?;

    let schema = azazel_protocol::schema();
    let frames = &schema["$defs"]["Frame"];
    let ui_request = frames["oneOf"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|variant| variant["properties"]["type"]["const"] == "ui_request")
        .ok_or_else(|| anyhow!("the RPC schema has no `ui_request` frame"))?;
    let mut rpc_schema = serde_json::to_string_pretty(&schema)?;
    rpc_schema.push('\n');

    let declarations = root.join("crates/azazel-lua/src/azazel.d.luau");
    let declarations = std::fs::read_to_string(&declarations)
        .with_context(|| format!("could not read {}", declarations.display()))?;
    let (luau_api, luau_events) = luau(&declarations);

    Ok(Code {
        version: env!("CARGO_PKG_VERSION").to_string(),
        config: azazel_core::config_keys::keys().into_iter().collect(),
        trust_gated: azazel_core::config::trust_gated_names().into_iter().collect(),
        hook_events: azazel_core::config::HOOK_EVENTS
            .iter()
            .map(ToString::to_string)
            .collect(),
        key_actions: key_actions(),
        cli: cli(),
        rpc_commands: variants(&schema["$defs"]["Request"], "type"),
        rpc_frames: variants(frames, "type"),
        rpc_ui_kinds: variants(ui_request, "kind"),
        rpc_schema,
        luau_api,
        luau_events,
        slash_commands: inventory.commands.into_iter().collect(),
        tools: inventory
            .tools
            .into_iter()
            .chain(inventory.conditional_tools.into_iter().map(|(name, _)| name))
            .collect(),
        xtasks: xtask::TASKS.iter().map(ToString::to_string).collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_luau_declarations_give_functions_fields_and_events() {
        let text = "type On =\n\t((event: \"session_start\", handler: (ev: A, ctx: Ctx) -> ()) -> ())\n\t& ((\n\t\tevent: \"tool_call\",\n\t\thandler: (ev: B, ctx: Ctx) -> ()\n\t) -> ())\n\t& ((event: \"turn_start\" | \"turn_end\", handler: (ev: C, ctx: Ctx) -> ()) -> ())\n\ntype Other = { name: \"not_an_event\" }\n\ndeclare azazel: {\n\t-- Load only.\n\ttool: (spec: ToolSpec) -> (),\n\ton: On,\n\texec: (program: string, options: {\n\t\tcwd: string?,\n\t}?) -> ExecResult,\n\tfs: {\n\t\t-- Its own directory.\n\t\tdata_dir: string,\n\t\tread: (path: string) -> string,\n\t},\n\tnotify: (text: string) -> (),\n}\n";
        let (api, events) = luau(text);
        assert_eq!(
            api.into_iter().collect::<Vec<_>>(),
            [
                "azazel.exec",
                "azazel.fs.data_dir",
                "azazel.fs.read",
                "azazel.notify",
                "azazel.on",
                "azazel.tool"
            ]
        );
        assert_eq!(
            events.into_iter().collect::<Vec<_>>(),
            ["session_start", "tool_call", "turn_end", "turn_start"]
        );
    }

    #[test]
    fn tagged_variants_are_read_from_a_schema() {
        let schema = serde_json::json!({ "oneOf": [
            { "properties": { "type": { "const": "prompt" } } },
            { "properties": { "type": { "const": "abort" } } },
            { "properties": { "other": {} } },
        ]});
        assert_eq!(
            variants(&schema, "type").into_iter().collect::<Vec<_>>(),
            ["abort", "prompt"]
        );
    }
}
