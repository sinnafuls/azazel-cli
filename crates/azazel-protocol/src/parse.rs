//! Reading a client's line, with errors a person writing a client can act on.

use std::sync::LazyLock;

use serde_json::{Map, Value};

use crate::{Command, Request};

/// The schema of [`Command`], which says what each command's members must be.
static COMMAND_SCHEMA: LazyLock<Value> = LazyLock::new(|| schemars::schema_for!(Command).to_value());

/// The schema a `$ref` points at, or the schema itself.
fn resolved<'a>(schema: &'a Value, root: &'a Value) -> &'a Value {
    schema
        .get("$ref")
        .and_then(Value::as_str)
        .and_then(|reference| reference.strip_prefix('#'))
        .and_then(|pointer| root.pointer(pointer))
        .unwrap_or(schema)
}

/// The choices of a schema that is one of several (`Option<T>` is `T` or null).
fn choices(schema: &Value) -> Option<&Vec<Value>> {
    schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
}

/// The values an enumeration allows, however the schema spells it: one list, or one choice per
/// documented value.
fn allowed(schema: &Value) -> Option<Vec<&Value>> {
    if let Some(values) = schema.get("enum").and_then(Value::as_array) {
        return Some(values.iter().collect());
    }
    let mut values = Vec::new();
    for choice in choices(schema)? {
        match (choice.get("const"), choice.get("enum").and_then(Value::as_array)) {
            (Some(value), _) => values.push(value),
            (None, Some(several)) => values.extend(several),
            (None, None) => return None,
        }
    }
    Some(values)
}

fn type_matches(name: &str, value: &Value) -> bool {
    match name {
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "integer" => value.is_i64() || value.is_u64(),
        "number" => value.is_number(),
        "array" => value.is_array(),
        "object" => value.is_object(),
        "null" => value.is_null(),
        _ => true,
    }
}

/// Whether `value` has the shape `schema` asks for. Only the outer shape is looked at; what is
/// inside lists and objects is left to the deserializer.
fn accepts(schema: &Value, value: &Value, root: &Value) -> bool {
    let schema = resolved(schema, root);
    if let Some(values) = allowed(schema) {
        return values.contains(&value);
    }
    if let Some(choices) = choices(schema) {
        return choices.iter().any(|choice| accepts(choice, value, root));
    }
    match schema.get("type") {
        Some(Value::String(name)) => type_matches(name, value),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .any(|name| type_matches(name, value)),
        _ => true,
    }
}

/// What `schema` asks for, in words: `a string`, `a list`, `one of allow, always, deny`.
fn expected(schema: &Value, root: &Value) -> String {
    let schema = resolved(schema, root);
    if let Some(values) = allowed(schema) {
        let names: Vec<String> = values
            .iter()
            .map(|value| value.as_str().map_or_else(|| value.to_string(), str::to_string))
            .collect();
        return format!("one of {}", names.join(", "));
    }
    if let Some(choices) = choices(schema) {
        return choices
            .iter()
            .find(|choice| choice.get("type").and_then(Value::as_str) != Some("null"))
            .map_or_else(|| "a value".to_string(), |choice| expected(choice, root));
    }
    let name = match schema.get("type") {
        Some(Value::String(name)) => name.as_str(),
        Some(Value::Array(names)) => names
            .iter()
            .filter_map(Value::as_str)
            .find(|name| *name != "null")
            .unwrap_or_default(),
        _ => "",
    };
    match name {
        "string" => "a string",
        "boolean" => "true or false",
        "integer" | "number" => "a number",
        "array" => "a list",
        "object" => "an object",
        _ => "a value",
    }
    .to_string()
}

/// Names the first member of `object` that is missing or of the wrong kind for the command
/// `kind`. `None` when the members look right, and the deserializer's own words have to do.
fn wrong_member(kind: &str, object: &Map<String, Value>) -> Option<String> {
    let root: &Value = &COMMAND_SCHEMA;
    let variant =
        root.get("oneOf")?.as_array()?.iter().find(|variant| {
            variant.pointer("/properties/type/const").and_then(Value::as_str) == Some(kind)
        })?;
    let required: Vec<&str> = variant
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();
    for (name, schema) in variant.get("properties")?.as_object()? {
        if name == "type" {
            continue;
        }
        let wrong = match object.get(name) {
            None => required.contains(&name.as_str()),
            Some(value) => !accepts(schema, value, root),
        };
        if wrong {
            return Some(format!("`{name}` must be {}", expected(schema, root)));
        }
    }
    None
}

impl Command {
    /// Every command's `type`, in the order of the enum.
    pub fn names() -> Vec<&'static str> {
        COMMAND_SCHEMA
            .get("oneOf")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|variant| variant.pointer("/properties/type/const")?.as_str())
            .collect()
    }

    /// Reads a command from a JSON value. The error says what is wrong with it.
    pub fn from_value(value: Value) -> Result<Command, String> {
        let Value::Object(object) = &value else {
            return Err("a command is a JSON object".to_string());
        };
        let kind = match object.get("type") {
            Some(Value::String(kind)) if !kind.is_empty() => kind.clone(),
            _ => return Err("the command has no `type`".to_string()),
        };
        if !Command::names().contains(&kind.as_str()) {
            return Err(format!("unknown command `{kind}`"));
        }
        if let Some(problem) = wrong_member(&kind, object) {
            return Err(problem);
        }
        serde_json::from_value(value).map_err(|e| format!("`{kind}`: {e}"))
    }
}

impl Request {
    /// Reads one input line. The id is returned even when the command is not understood, so the
    /// answer can carry it.
    pub fn parse(line: &str) -> (Value, Result<Command, String>) {
        match serde_json::from_str::<Value>(line) {
            Ok(value @ Value::Object(_)) => {
                let id = value.get("id").cloned().unwrap_or(Value::Null);
                (id, Command::from_value(value))
            }
            Ok(_) => (Value::Null, Err("a command is a JSON object".to_string())),
            Err(e) => (Value::Null, Err(format!("not JSON: {e}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::{Decision, Image, RewindWhat};

    #[test]
    fn commands_are_read_with_their_defaults() {
        let (id, command) = Request::parse(r#"{"id":7,"type":"prompt","text":"hi"}"#);
        assert_eq!(id, json!(7));
        assert_eq!(
            command,
            Ok(Command::Prompt {
                text: "hi".into(),
                images: Vec::new()
            })
        );
        assert_eq!(
            Command::from_value(
                json!({ "type": "prompt", "text": "see", "images": [{ "mime": "image/png", "data": "AA==" }] })
            ),
            Ok(Command::Prompt {
                text: "see".into(),
                images: vec![Image {
                    mime: "image/png".into(),
                    data: "AA==".into()
                }]
            })
        );
        assert_eq!(
            Command::from_value(json!({ "type": "shell", "command": "ls" })),
            Ok(Command::Shell {
                command: "ls".into(),
                share: true
            })
        );
        assert_eq!(
            Command::from_value(json!({ "type": "rewind", "entry_id": "e1" })),
            Ok(Command::Rewind {
                entry_id: "e1".into(),
                what: RewindWhat::Both
            })
        );
        assert_eq!(
            Command::from_value(
                json!({ "type": "approval_response", "request_id": "r1", "decision": "deny", "feedback": null })
            ),
            Ok(Command::ApprovalResponse {
                request_id: "r1".into(),
                decision: Decision::Deny,
                feedback: None
            })
        );
        assert_eq!(
            Command::from_value(json!({ "type": "ui_response", "request_id": "r2" })),
            Ok(Command::UiResponse {
                request_id: "r2".into(),
                value: Value::Null
            })
        );
    }

    #[test]
    fn mistakes_are_named() {
        let error = |value: Value| Command::from_value(value).unwrap_err();
        assert_eq!(error(json!([1, 2])), "a command is a JSON object");
        assert_eq!(error(json!({ "text": "hi" })), "the command has no `type`");
        assert_eq!(error(json!({ "type": "dance" })), "unknown command `dance`");
        assert_eq!(error(json!({ "type": "prompt" })), "`text` must be a string");
        assert_eq!(
            error(json!({ "type": "prompt", "text": 3 })),
            "`text` must be a string"
        );
        assert_eq!(
            error(json!({ "type": "set_model", "chain": "fable" })),
            "`chain` must be a list"
        );
        assert_eq!(
            error(json!({ "type": "shell", "command": "ls", "share": "yes" })),
            "`share` must be true or false"
        );
        assert_eq!(
            error(json!({ "type": "approval_response", "request_id": "r1", "decision": "maybe" })),
            "`decision` must be one of allow, session, always, deny"
        );
        assert_eq!(
            error(json!({ "type": "rewind", "entry_id": "e1", "what": "all" })),
            "`what` must be one of both, code, conversation"
        );
        // Inside a list the deserializer speaks for itself.
        let inner = error(json!({ "type": "prompt", "text": "x", "images": [{ "mime": "image/png" }] }));
        assert!(
            inner.starts_with("`prompt`: ") && inner.contains("data"),
            "{inner}"
        );

        let (id, not_json) = Request::parse("{oops");
        assert_eq!(id, Value::Null);
        assert!(not_json.unwrap_err().starts_with("not JSON: "));
        let (id, not_object) = Request::parse("[1]");
        assert_eq!(
            (id, not_object),
            (Value::Null, Err("a command is a JSON object".into()))
        );
    }

    #[test]
    fn every_command_has_a_name() {
        let names = Command::names();
        assert_eq!(names.len(), 36, "{names:?}");
        assert!(names.contains(&"prompt") && names.contains(&"run_command"));
    }
}
