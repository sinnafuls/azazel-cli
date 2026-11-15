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
    "list_models",
    "list_roles",
    "list_commands",
];

/// The slash commands a browser may run: they only report.
const COMMANDS: &[&str] = &["cost", "context", "session", "tools", "todo", "rules", "doctor"];

/// Whether a device with `access` may send `command`.
pub fn policy(access: Access, command: &Command, allow_bypass: bool) -> Result<(), String> {
    let value = serde_json::to_value(command).map_err(|e| e.to_string())?;
    check(access, &value, allow_bypass)
}

/// [`policy`] for a command as the JSON object a client sent.
pub fn check(access: Access, command: &Value, allow_bypass: bool) -> Result<(), String> {
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
    match kind {
        "shell" => refused("it would run a command on this machine"),
        "export" if given("path") => refused("it would write a file on this machine"),
        "resume_session" if given("path") => refused("it would read a file on this machine"),
        "save_preset" => refused("it would change the configuration"),
        "set_role" if !matches!(text("save"), "" | "session") => {
            refused("it would change the configuration; use `save: session`")
        }
        "set_mode" if text("mode").eq_ignore_ascii_case("bypass") && !allow_bypass => {
            refused("`bypass` is off; set `remote.allow_bypass` to allow it")
        }
        "run_command" => {
            let name = text("name").trim_start_matches('/').to_ascii_lowercase();
            if COMMANDS.contains(&name.as_str()) || name.starts_with("skill:") {
                Ok(())
            } else {
                refused(&format!(
                    "the command `{name}` is not on the list a browser may run"
                ))
            }
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

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
            .filter(|kind| check(Access::View, &json!({ "type": kind }), false).is_ok())
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
            .filter(|kind| check(Access::Control, &json!({ "type": kind }), false).is_err())
            // `run_command` without a name is refused as an unknown command.
            .collect();
        assert_eq!(refused, ["shell", "save_preset", "run_command"]);
    }

    #[test]
    fn what_a_command_carries_decides_for_the_commands_that_are_sometimes_fine() {
        let control = |command: Value, bypass: bool| check(Access::Control, &command, bypass);
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
    fn a_browser_runs_the_slash_commands_that_only_report() {
        let run = |name: &str| {
            check(
                Access::Control,
                &json!({ "type": "run_command", "name": name }),
                false,
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
        ] {
            assert!(run(name).is_ok(), "{name}");
        }
        for name in [
            "trust",
            "add-dir",
            "hooks",
            "forget-approvals",
            "login",
            "skill",
            "",
        ] {
            assert!(run(name).is_err(), "{name}");
        }
    }

    #[test]
    fn a_message_without_a_type_is_not_a_command() {
        assert!(check(Access::Control, &json!({ "text": "hi" }), false).is_err());
        assert!(check(Access::View, &json!("prompt"), false).is_err());
    }

    #[test]
    fn the_typed_form_gives_the_same_answer() {
        let prompt = Command::Prompt {
            text: "hi".into(),
            images: Vec::new(),
            request_id: None,
        };
        assert!(policy(Access::View, &prompt, false).is_err());
        assert!(policy(Access::Control, &prompt, false).is_ok());
        assert!(
            policy(
                Access::Control,
                &Command::Shell {
                    command: "ls".into(),
                    share: true
                },
                false
            )
            .is_err()
        );
        assert!(policy(Access::View, &Command::GetState, false).is_ok());
    }
}
