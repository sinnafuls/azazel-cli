//! What each access level may send. The server checks every command before it reaches the
//! session; a refusal is an error response with the command's id.
//!
//! The check reads the command as JSON, by its `type`, so that a command the protocol gains later
//! is refused to a viewer until it is listed here.

use azazel_protocol::Command;
use serde_json::Value;

use crate::Access;

/// The commands a viewer may send: they read, and change nothing.
const VIEW: &[&str] = &[
    "attach",
    "get_state",
    "get_messages",
    "get_agent_messages",
    "get_tree",
    "get_usage",
    "get_theme",
    "get_context",
    "get_cost",
    "list_models",
    "list_roles",
    "list_commands",
];

/// Commands that change what the machine allows or who is signed in to it: a browser sends them
/// only with `remote.full_access`.
const FULL_ACCESS_ONLY: &[&str] = &["trust_project", "add_dir", "forget_approvals"];

/// The commands a control device may send as they are. Every type of the protocol is in exactly
/// one of [`VIEW`], this list and [`RESTRICTED`], and a test holds it to that: a command the
/// protocol gains is refused to a browser until someone decides which it is.
const CONTROL: &[&str] = &[
    "prompt",
    "continue_run",
    "steer",
    "follow_up",
    "abort",
    "set_model",
    "set_agent",
    "agent_send",
    "agent_stop",
    "agent_mention",
    "approval_response",
    "ui_response",
    "new_session",
    "fork_session",
    "list_sessions",
    "set_leaf",
    "set_label",
    "get_session",
    "set_name",
    "set_thinking",
    "set_tier",
    "compact",
    "retry",
    "rewind",
    "btw",
    "handoff",
    "list_providers",
    "refresh_models",
    "apply_preset",
    "list_files",
    "get_settings",
    "stop",
];

/// The commands a control device may send only with what they carry checked, or not at all
/// without `full_access`: [`check`] decides each one.
#[cfg(test)]
const RESTRICTED: &[&str] = &[
    "set_setting",
    "set_mode",
    "set_role",
    "save_preset",
    "shell",
    "export",
    "resume_session",
    "run_command",
    "trust_project",
    "add_dir",
    "forget_approvals",
];

/// What a server lets a control device do beyond the default.
#[derive(Debug, Clone, Copy)]
pub struct Grants {
    /// `remote.allow_bypass`: the `bypass` mode, and the project's own commands.
    pub allow_bypass: bool,
    /// `remote.full_access`: everything the terminal can do.
    pub full_access: bool,
    /// Whether a configuration key is one a project may set only once it is trusted. The server
    /// does not know the configuration, so whoever starts it says.
    pub gated: fn(&str) -> bool,
}

fn nothing_is_gated(_: &str) -> bool {
    false
}

impl Grants {
    /// Only the bypass switch; no full access and nothing gated.
    pub fn bypass_only(allow_bypass: bool) -> Grants {
        Grants {
            allow_bypass,
            ..Grants::default()
        }
    }
}

impl Default for Grants {
    fn default() -> Grants {
        Grants {
            allow_bypass: false,
            full_access: false,
            gated: nothing_is_gated,
        }
    }
}

/// The slash commands a browser may run: the reports, and the commands of the built-in
/// extensions that act on the session itself. Skills (`skill:<name>`) and MCP prompts
/// (`mcp:<server>:<prompt>`) only make a prompt, so they may run too. The ones with
/// subcommands are held to what [`subcommand_allowed`] lists.
const COMMANDS: &[&str] = &[
    // Reports.
    "cost",
    "context",
    "session",
    "tools",
    "todo",
    "dirs",
    "rules",
    "doctor",
    // Commands of the built-in extensions.
    "fast",
    "slow",
    "preset",
    "usage",
    "stats",
    "skills",
    "agents",
    "msg",
    "stop",
    "task",
    "review",
    "security-review",
    "playtest",
    "goal",
    "verify",
    "coordinator",
    "advise",
    "memory",
    "repomap",
    "lsp",
    "mcp",
    "media",
    "ext",
];

/// Slash commands a browser never runs: they sign in or out, change the configuration or the
/// trust of a project, widen where agents may work, or start programs of their own.
const NEVER: &[&str] = &[
    "login",
    "logout",
    "account",
    "settings",
    "reset-usage",
    "trust",
    "add-dir",
    "hooks",
    "forget-approvals",
    "allow-edit",
    "reload",
    "bg",
];

/// Whether `name` is a slash command this policy has decided about, one way or the other. A
/// command a program registers that is in neither list is one nobody has decided on.
pub fn command_decided(name: &str) -> bool {
    COMMANDS.contains(&name) || NEVER.contains(&name)
}

/// A word of a subcommand: nothing a shell or a handler's tokeniser treats specially, so that
/// every way of splitting the arguments sees the same words.
fn plain_word(word: &str) -> bool {
    !word.is_empty()
        && word
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
}

/// For the commands that have subcommands: whether `args` is one that changes nothing on disk, in
/// the configuration, in what is remembered or in what a server runs. It is an allowlist on the
/// raw text, and a quote or any other character outside [`plain_word`] fails it: the handlers
/// split the arguments their own way (`/mcp` strips quotes), and the two must not disagree.
fn subcommand_allowed(name: &str, args: &str) -> bool {
    if !matches!(name, "mcp" | "preset" | "memory" | "media") {
        return true;
    }
    let words: Vec<&str> = args.split_whitespace().collect();
    if !words.iter().all(|word| plain_word(word)) {
        return false;
    }
    match (name, words.as_slice()) {
        // `list` reads, and `reconnect` restarts a server the user has already approved. `enable`,
        // `disable`, `approve`, `reject`, `login` and `logout` change the configuration, what
        // starts or the stored sign-in.
        ("mcp", [] | ["list"] | ["reconnect", _]) => true,
        // Applying a preset changes this session's roles; `save` writes the configuration.
        ("preset", []) => true,
        ("preset", [word]) => *word != "save",
        // `forget` deletes a memory and `auto` changes what is remembered from now on.
        ("memory", [] | ["list"]) => true,
        // `/media` reports the backends and takes no arguments.
        ("media", []) => true,
        _ => false,
    }
}

/// Why `name args` may not run from a browser, when it may not.
fn command_refusal(name: &str, args: &str, grants: Grants) -> Option<String> {
    if name.is_empty() {
        return Some("the command has no name".to_string());
    }
    if grants.full_access {
        return None;
    }
    if NEVER.contains(&name) {
        return Some(format!(
            "`/{name}` changes sign-ins, settings or trust; run it in a terminal"
        ));
    }
    if COMMANDS.contains(&name) {
        return match subcommand_allowed(name, args) {
            true => None,
            false => Some(format!(
                "`/{name}` with those arguments could change the configuration, sign in or start a server; run it in a terminal"
            )),
        };
    }
    if name.starts_with("skill:") || name.starts_with("mcp:") {
        return None;
    }
    // The project's custom commands and the commands of Luau scripts can run programs with what
    // they are given; a browser that may already lift every guard may run them as well.
    match grants.allow_bypass {
        true => None,
        false => Some(format!(
            "`/{name}` is not on the list a browser may run; `remote.allow_bypass` allows the project's own commands"
        )),
    }
}

/// Whether a command of `list_commands` is one a browser may run with no arguments, which is
/// what its menu offers. `origin` is where the session says the command comes from.
pub(crate) fn command_listed(name: &str, origin: Option<&str>, grants: Grants) -> bool {
    let name = name.trim_start_matches('/').to_ascii_lowercase();
    if command_refusal(&name, "", grants).is_some() {
        return false;
    }
    // A command of the project or of a script runs only with `allow_bypass`, whatever its name.
    grants.allow_bypass || grants.full_access || !matches!(origin, Some("custom" | "script"))
}

/// Whether a device with `access` may send `command`.
pub fn policy(access: Access, command: &Command, grants: Grants) -> Result<(), String> {
    let value = serde_json::to_value(command).map_err(|e| e.to_string())?;
    check(access, &value, grants)
}

/// [`policy`] for a command as the JSON object a client sent.
pub fn check(access: Access, command: &Value, grants: Grants) -> Result<(), String> {
    let Some(kind) = command.get("type").and_then(Value::as_str) else {
        return Err("a command has a `type`".to_string());
    };
    if access == Access::View {
        return match VIEW.contains(&kind) {
            true => Ok(()),
            false => Err(format!("this is a view link: `{kind}` needs a control link")),
        };
    }
    let given = |member: &str| command.get(member).is_some_and(|value| !value.is_null());
    let text = |member: &str| command.get(member).and_then(Value::as_str).unwrap_or_default();
    let refused = |why: &str| Err(format!("`{kind}` is not available from a browser: {why}"));
    if grants.full_access {
        // A control device does what the terminal does; only a command with no name is no command.
        return match kind {
            "run_command" if text("name").trim().trim_start_matches('/').is_empty() => {
                refused("the command has no name")
            }
            _ => Ok(()),
        };
    }
    match kind {
        _ if FULL_ACCESS_ONLY.contains(&kind) => refused("it needs `remote.full_access`"),
        // The protocol's default scope is the session.
        "set_setting" => match (text("scope"), (grants.gated)(text("path"))) {
            ("session" | "", false) => Ok(()),
            ("session" | "", true) => refused("that setting is not one a browser may change"),
            _ => refused("it would change the configuration; use `scope: session`"),
        },
        "shell" => refused("it would run a command on this machine"),
        "export" if given("path") => refused("it would write a file on this machine"),
        "resume_session" if given("path") => refused("it would read a file on this machine"),
        "save_preset" => refused("it would change the configuration"),
        "set_role" if !matches!(text("save"), "" | "session") => {
            refused("it would change the configuration; use `save: session`")
        }
        "set_mode" if text("mode").eq_ignore_ascii_case("bypass") && !grants.allow_bypass => {
            refused("`bypass` is off; set `remote.allow_bypass` to allow it")
        }
        "run_command" => {
            let name = text("name").trim().trim_start_matches('/').to_ascii_lowercase();
            match command_refusal(&name, text("args"), grants) {
                None => Ok(()),
                Some(why) => refused(&why),
            }
        }
        // What the arms above did not refuse: a mode, a role, an export or a resume by id.
        "set_mode" | "set_role" | "export" | "resume_session" => Ok(()),
        _ if VIEW.contains(&kind) || CONTROL.contains(&kind) => Ok(()),
        _ => refused("this server does not know it"),
    }
}

/// What the session must hold a command to once [`check`] has passed it. A `run_command` of a
/// browser that may not run the project's own commands carries `only`: the session then runs it
/// only when the command it resolves the name to comes from the program, a skill or an MCP
/// server. The name was approved for what the policy knows it as; a custom command or a script
/// command of the same name must not take its place. Whatever `only` the client sent is replaced.
pub fn confine(command: &mut Value, grants: Grants) {
    if command.get("type").and_then(Value::as_str) != Some("run_command") {
        return;
    }
    let Some(members) = command.as_object_mut() else {
        return;
    };
    if grants.allow_bypass || grants.full_access {
        members.remove("only");
    } else {
        members.insert("only".to_string(), serde_json::json!(["builtin", "skill", "mcp"]));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn bypass(allow_bypass: bool) -> Grants {
        Grants::bypass_only(allow_bypass)
    }

    fn full() -> Grants {
        Grants {
            full_access: true,
            ..Grants::default()
        }
    }

    /// The configuration's own gate, as the server is started with.
    fn gated(path: &str) -> bool {
        matches!(path, "permissions.mode" | "hooks.pre_tool" | "remote.full_access")
    }

    /// Every command the protocol knows, from its schema.
    fn every_type() -> Vec<String> {
        let schema: Value = serde_json::from_str(include_str!("../../../docs/rpc.schema.json")).unwrap();
        schema["$defs"]["Request"]["oneOf"]
            .as_array()
            .unwrap()
            .iter()
            .map(|variant| {
                variant["properties"]["type"]["const"]
                    .as_str()
                    .unwrap()
                    .to_string()
            })
            .collect()
    }

    #[test]
    fn a_viewer_may_only_read() {
        let types = every_type();
        assert!(types.len() > 40);
        let mut allowed: Vec<&str> = types
            .iter()
            .map(String::as_str)
            .filter(|kind| check(Access::View, &json!({ "type": kind }), bypass(false)).is_ok())
            .collect();
        allowed.sort_unstable();
        let mut expected: Vec<&str> = VIEW
            .iter()
            .copied()
            // Lane A adds `get_agent_messages` to the protocol; before that it is not in the schema.
            .filter(|kind| types.iter().any(|known| known == kind))
            .collect();
        expected.sort_unstable();
        assert_eq!(allowed, expected);
        assert!(!allowed.contains(&"prompt") && !allowed.contains(&"approval_response"));
    }

    #[test]
    fn a_controller_may_send_everything_but_what_reaches_the_machine() {
        let refused: Vec<String> = every_type()
            .into_iter()
            .filter(|kind| check(Access::Control, &json!({ "type": kind }), bypass(false)).is_err())
            // `run_command` without a name is refused as an unknown command.
            .collect();
        // `set_setting` without a scope or path is a session change of no gated key, so it passes.
        assert_eq!(
            refused,
            [
                "shell",
                "save_preset",
                "run_command",
                "trust_project",
                "add_dir",
                "forget_approvals"
            ]
        );
    }

    #[test]
    fn what_a_command_carries_decides_for_the_commands_that_are_sometimes_fine() {
        let control = |command: Value, allow: bool| check(Access::Control, &command, bypass(allow));
        assert!(control(json!({ "type": "export" }), false).is_ok());
        assert!(control(json!({ "type": "export", "format": "html", "path": null }), false).is_ok());
        assert!(control(json!({ "type": "export", "path": "/tmp/a.html" }), false).is_err());
        assert!(control(json!({ "type": "resume_session", "session_id": "abc" }), false).is_ok());
        assert!(control(json!({ "type": "resume_session", "path": "x.jsonl" }), false).is_err());
        assert!(
            control(
                json!({ "type": "set_role", "role": "default", "save": "session" }),
                false
            )
            .is_ok()
        );
        assert!(control(json!({ "type": "set_role", "role": "default" }), false).is_ok());
        assert!(
            control(
                json!({ "type": "set_role", "role": "default", "save": "home" }),
                false
            )
            .is_err()
        );
        assert!(
            control(
                json!({ "type": "set_role", "role": "default", "save": "project" }),
                false
            )
            .is_err()
        );
        assert!(control(json!({ "type": "set_mode", "mode": "auto" }), false).is_ok());
        assert!(control(json!({ "type": "set_mode", "mode": "Bypass" }), false).is_err());
        assert!(control(json!({ "type": "set_mode", "mode": "bypass" }), true).is_ok());
    }

    #[test]
    fn a_browser_runs_the_commands_that_act_on_the_session_but_not_those_that_change_the_machine() {
        let run = |name: &str, args: &str, allow: bool| {
            check(
                Access::Control,
                &json!({ "type": "run_command", "name": name, "args": args }),
                bypass(allow),
            )
        };
        for name in [
            "cost",
            "context",
            "session",
            "tools",
            "todo",
            "rules",
            "doctor",
            "/cost",
            "skill:review",
            "mcp:github:triage",
            "fast",
            "goal",
            "review",
            "compact-notes-is-unknown-but-bypass-allows-it",
        ] {
            assert!(run(name, "", true).is_ok(), "{name}");
        }
        for name in [
            "cost",
            "skill:review",
            "usage",
            "memory",
            "agents",
            "preset",
            "mcp",
        ] {
            assert!(run(name, "", false).is_ok(), "{name}");
        }
        // A project's own command can run programs: only with `allow_bypass`.
        assert!(run("deploy", "", false).is_err());
        assert!(run("deploy", "", true).is_ok());
        for name in [
            "trust",
            "add-dir",
            "hooks",
            "forget-approvals",
            "login",
            "logout",
            "settings",
            "reload",
            "bg",
            "",
        ] {
            assert!(run(name, "", true).is_err(), "{name}");
        }
        assert!(run("preset", "save fast", true).is_err());
        assert!(run("preset", "fast", false).is_ok());
        assert!(run("mcp", "approve github", true).is_err());
        assert!(run("mcp", "reconnect github", false).is_ok());
    }

    #[test]
    fn a_subcommand_is_allowed_by_a_list_and_not_refused_by_one() {
        let run = |name: &str, args: &str| {
            check(
                Access::Control,
                &json!({ "type": "run_command", "name": name, "args": args }),
                bypass(true),
            )
        };
        for args in [
            "",
            "list",
            "  list  ",
            "reconnect github",
            "reconnect   my-server_2.x",
            "	reconnect	github",
        ] {
            assert!(run("mcp", args).is_ok(), "mcp {args:?}");
        }
        // The handler strips quotes and the policy must not read what the handler does not.
        for args in [
            "\"approve\" srv",
            "'approve' srv",
            "ap\"prove\" srv",
            "\"\"approve srv",
            "APPROVE srv",
            "Approve srv",
            "approve srv",
            "  approve   srv",
            "enable srv",
            "disable srv",
            "reject srv",
            "login srv",
            "logout srv",
            "\"logout\" srv",
            "reconnect",
            "reconnect a b",
            "reconnect \"srv\"",
            "reconnect srv;rm",
            "list extra",
            "unknown",
        ] {
            assert!(run("mcp", args).is_err(), "mcp {args:?}");
        }
        for args in ["", "fast", " fast ", "my-preset"] {
            assert!(run("preset", args).is_ok(), "preset {args:?}");
        }
        for args in [
            "save fast",
            "save",
            "SAVE",
            "Save x",
            "\"save\" x",
            "'save'",
            "a b",
            "fa\"st",
        ] {
            let outcome = run("preset", args);
            // `SAVE` is a preset name the handler looks up, and it is not `save`.
            if args == "SAVE" {
                assert!(outcome.is_ok(), "preset {args:?}");
            } else {
                assert!(outcome.is_err(), "preset {args:?}");
            }
        }
        for args in ["", "list", " list "] {
            assert!(run("memory", args).is_ok(), "memory {args:?}");
        }
        for args in [
            "forget 1",
            "auto",
            "auto on",
            "auto off",
            "\"forget\" 1",
            "FORGET 1",
            "x",
        ] {
            assert!(run("memory", args).is_err(), "memory {args:?}");
        }
        assert!(run("media", "").is_ok());
        assert!(run("media", "  ").is_ok());
        assert!(run("media", "anything").is_err());
        // Another command takes what it takes.
        assert!(run("cost", "\"x\" y").is_ok());
    }

    #[test]
    fn the_name_of_a_command_is_read_as_the_session_reads_it() {
        let run = |name: &str, args: &str| {
            check(
                Access::Control,
                &json!({ "type": "run_command", "name": name, "args": args }),
                bypass(false),
            )
        };
        assert!(run(" /MCP ", "\"approve\" srv").is_err());
        assert!(run("/Mcp", "reconnect x").is_ok());
        assert!(run("/LOGIN", "").is_err());
    }

    #[test]
    fn a_run_command_is_confined_to_what_the_program_provides_unless_the_project_may_run() {
        let command = json!({ "type": "run_command", "name": "mcp", "args": "", "only": ["custom"] });
        let mut sent = command.clone();
        confine(&mut sent, bypass(false));
        assert_eq!(sent["only"], json!(["builtin", "skill", "mcp"]));
        for grants in [bypass(true), full()] {
            let mut sent = command.clone();
            confine(&mut sent, grants);
            assert!(sent.get("only").is_none());
        }
        // Another command is left as it is.
        let mut other = json!({ "type": "prompt", "text": "hi" });
        confine(&mut other, bypass(false));
        assert_eq!(other, json!({ "type": "prompt", "text": "hi" }));
    }

    #[test]
    fn a_menu_lists_no_custom_or_script_command_without_allow_bypass() {
        assert!(command_listed("fast", Some("builtin"), bypass(false)));
        assert!(command_listed("skill:review", Some("skill"), bypass(false)));
        assert!(command_listed("mcp:github:triage", Some("mcp"), bypass(false)));
        // A project command named like a built-in one is not the built-in one.
        assert!(!command_listed("memory", Some("custom"), bypass(false)));
        assert!(!command_listed("mcp", Some("script"), bypass(false)));
        assert!(!command_listed("deploy", Some("custom"), bypass(false)));
        assert!(command_listed("deploy", Some("custom"), bypass(true)));
        assert!(!command_listed("login", Some("builtin"), bypass(true)));
        assert!(command_listed("login", Some("builtin"), full()));
    }

    #[test]
    fn every_command_of_the_protocol_is_in_exactly_one_list() {
        for kind in every_type() {
            let kind = kind.as_str();
            let places = [
                VIEW.contains(&kind),
                CONTROL.contains(&kind),
                RESTRICTED.contains(&kind),
            ];
            assert_eq!(
                places.iter().filter(|here| **here).count(),
                1,
                "{kind}: {places:?}"
            );
        }
        // And every name of a list is a command of the protocol.
        let known = every_type();
        for kind in VIEW.iter().chain(CONTROL).chain(RESTRICTED) {
            assert!(
                known.iter().any(|known| known == kind),
                "{kind} is not in the protocol"
            );
        }
    }

    #[test]
    fn a_command_the_protocol_does_not_know_is_refused_to_a_control_device() {
        assert!(check(Access::Control, &json!({ "type": "brand_new" }), bypass(true)).is_err());
        assert!(check(Access::Control, &json!({ "type": "brand_new" }), full()).is_ok());
    }

    #[test]
    fn no_command_is_both_allowed_and_never() {
        for name in COMMANDS {
            assert!(!NEVER.contains(name), "{name}");
        }
        assert!(command_decided("mcp") && command_decided("login") && !command_decided("deploy"));
    }

    #[test]
    fn a_message_without_a_type_is_not_a_command() {
        assert!(check(Access::Control, &json!({ "text": "hi" }), bypass(false)).is_err());
        assert!(check(Access::View, &json!("prompt"), bypass(false)).is_err());
    }

    #[test]
    fn the_typed_form_gives_the_same_answer() {
        let prompt = Command::Prompt {
            text: "hi".into(),
            images: Vec::new(),
            request_id: None,
        };
        assert!(policy(Access::View, &prompt, bypass(false)).is_err());
        assert!(policy(Access::Control, &prompt, bypass(false)).is_ok());
        assert!(
            policy(
                Access::Control,
                &Command::Shell {
                    command: "ls".into(),
                    share: true
                },
                bypass(false)
            )
            .is_err()
        );
        assert!(policy(Access::View, &Command::GetState, bypass(false)).is_ok());
    }

    #[test]
    fn a_browser_changes_only_session_settings_that_are_not_gated() {
        let grants = Grants {
            gated,
            ..Grants::default()
        };
        let set = |path: &str, scope: &str| {
            check(
                Access::Control,
                &json!({ "type": "set_setting", "path": path, "value": 1, "scope": scope }),
                grants,
            )
        };
        assert!(set("tui.theme", "session").is_ok());
        assert!(set("tui.theme", "home").is_err());
        assert!(set("tui.theme", "project").is_err());
        assert!(set("permissions.mode", "session").is_err());
        // No scope is the protocol's default, the session.
        let bare = |path: &str| {
            check(
                Access::Control,
                &json!({ "type": "set_setting", "path": path, "value": 1 }),
                grants,
            )
        };
        assert!(bare("tui.theme").is_ok());
        assert!(bare("permissions.mode").is_err());
        assert!(set("remote.full_access", "session").is_err());
        assert!(check(Access::View, &json!({ "type": "set_setting" }), grants).is_err());
        for kind in ["get_theme", "get_context", "get_cost"] {
            assert!(
                check(Access::View, &json!({ "type": kind }), grants).is_ok(),
                "{kind}"
            );
        }
        for kind in ["get_settings", "list_models"] {
            assert!(
                check(Access::Control, &json!({ "type": kind }), grants).is_ok(),
                "{kind}"
            );
        }
    }

    #[test]
    fn trusting_adding_directories_and_forgetting_approvals_need_full_access() {
        for kind in ["trust_project", "add_dir", "forget_approvals"] {
            let command = json!({ "type": kind, "path": "x" });
            assert!(check(Access::Control, &command, bypass(true)).is_err(), "{kind}");
            assert!(check(Access::Control, &command, full()).is_ok(), "{kind}");
            assert!(check(Access::View, &command, full()).is_err(), "{kind}");
        }
    }

    #[test]
    fn full_access_refuses_a_control_device_nothing_but_a_nameless_command() {
        let grants = Grants { gated, ..full() };
        for command in [
            json!({ "type": "shell", "command": "ls" }),
            json!({ "type": "export", "path": "/tmp/a.html" }),
            json!({ "type": "resume_session", "path": "x.jsonl" }),
            json!({ "type": "save_preset", "name": "p" }),
            json!({ "type": "set_role", "role": "default", "save": "home" }),
            json!({ "type": "set_mode", "mode": "bypass" }),
            json!({ "type": "set_setting", "path": "permissions.mode", "value": "bypass", "scope": "project" }),
            json!({ "type": "run_command", "name": "login", "args": "" }),
            json!({ "type": "run_command", "name": "preset", "args": "save fast" }),
            json!({ "type": "run_command", "name": "mcp", "args": "approve github" }),
            json!({ "type": "run_command", "name": "deploy", "args": "" }),
        ] {
            assert!(check(Access::Control, &command, grants).is_ok(), "{command}");
        }
        assert!(
            check(
                Access::Control,
                &json!({ "type": "run_command", "name": " / " }),
                grants
            )
            .is_err()
        );
        // A view device stays view-only.
        assert!(check(Access::View, &json!({ "type": "shell", "command": "ls" }), grants).is_err());
        assert!(check(Access::View, &json!({ "type": "prompt", "text": "hi" }), grants).is_err());
    }

    #[test]
    fn the_command_menu_follows_full_access() {
        for name in ["login", "settings", "trust", "reload", "bg"] {
            assert!(!command_listed(name, Some("builtin"), bypass(true)), "{name}");
            assert!(command_listed(name, Some("builtin"), full()), "{name}");
        }
    }
}
