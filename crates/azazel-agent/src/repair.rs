//! Tool calls the way models get them slightly wrong: a tool called by the name another harness
//! gives it (`bash`, `str_replace`), an object sent as a JSON string, `"20"` for a number, `null`
//! for a field left out, keys the tool does not take. Such a call is mended against the tool's
//! JSON schema before it runs, and the result says what was mended, so the model learns the real
//! shape. A value that already fits its schema is never changed.

use std::sync::Arc;

use azazel_ai::INVALID_JSON_KEY;
use serde_json::{Map, Number, Value};

use crate::tool::Tool;

/// Names other harnesses give the tools, and the tool each stands for.
const ALIASES: [(&str, &str); 8] = [
    ("bash", "shell"),
    ("str_replace", "edit"),
    ("apply_edit", "edit"),
    ("grep_search", "grep"),
    ("list_dir", "read"),
    ("ls", "read"),
    ("todowrite", "todo"),
    // 0.17 folded `run_code` into `eval`; sessions and habits from before still call it. Kept for
    // one release.
    ("run_code", "eval"),
];

/// A call as it is run: the tool its name stands for, and its input after repair.
pub(crate) struct Prepared {
    pub call_id: String,
    /// The tool's own name when the call resolved to a tool, otherwise the name as called.
    pub name: String,
    pub input: Value,
    pub tool: Option<Arc<dyn Tool>>,
    /// What was mended, in the words of the note; empty for a call that was right.
    pub repairs: Vec<String>,
}

impl Prepared {
    /// The line added to the call's result when something was mended.
    pub fn note(&self) -> Option<String> {
        (!self.repairs.is_empty()).then(|| format!("note: {}", self.repairs.join("; ")))
    }
}

/// The tool `name` stands for: the one of that name, else the one an alias names, else the one
/// whose name matches ignoring case. The second value says whether it was found by another name.
pub(crate) fn resolve(name: &str, tools: &[Arc<dyn Tool>]) -> Option<(Arc<dyn Tool>, bool)> {
    let named = |wanted: &str| tools.iter().find(|tool| tool.name() == wanted).cloned();
    if let Some(tool) = named(name) {
        return Some((tool, false));
    }
    let lower = name.to_ascii_lowercase();
    ALIASES
        .iter()
        .find(|(alias, _)| *alias == lower)
        .and_then(|(_, real)| named(real))
        .or_else(|| {
            tools
                .iter()
                .find(|tool| tool.name().eq_ignore_ascii_case(name))
                .cloned()
        })
        .map(|tool| (tool, true))
}

/// Resolves the call's tool and mends its input against the tool's schema.
pub(crate) fn prepare(call_id: String, name: String, mut input: Value, tools: &[Arc<dyn Tool>]) -> Prepared {
    let Some((tool, aliased)) = resolve(&name, tools) else {
        return Prepared {
            call_id,
            name,
            input,
            tool: None,
            repairs: Vec::new(),
        };
    };
    let mut repairs = Vec::new();
    if aliased {
        repairs.push(format!("called as `{name}`; the tool is `{}`", tool.name()));
        // `run_code` took only `code` and ran Luau; `eval` needs to be told.
        if name.eq_ignore_ascii_case("run_code")
            && tool.name() == "eval"
            && let Some(object) = input.as_object_mut()
            && !object.contains_key("language")
        {
            object.insert("language".to_string(), Value::String("luau".to_string()));
            repairs.push("`eval` needs `language: \"luau\"` for what `run_code` ran".to_string());
        }
    }
    // Arguments that were not JSON at all are refused whole, not guessed at.
    if input.get(INVALID_JSON_KEY).is_none() {
        repair(&tool.schema(), &mut input, &mut repairs);
    }
    Prepared {
        call_id,
        name: tool.name().to_string(),
        input,
        tool: Some(tool),
        repairs,
    }
}

/// Mends `input` against `schema`, adding a line to `repairs` for each change.
pub(crate) fn repair(schema: &Value, input: &mut Value, repairs: &mut Vec<String>) {
    mend(schema, input, &mut String::new(), repairs);
}

/// The types a schema allows, `None` when it says nothing about the type.
fn types(schema: &Value) -> Option<Vec<&str>> {
    match schema.get("type") {
        Some(Value::String(one)) => Some(vec![one.as_str()]),
        Some(Value::Array(many)) => Some(many.iter().filter_map(Value::as_str).collect()),
        _ if schema.get("properties").is_some() => Some(vec!["object"]),
        _ if schema.get("items").is_some() => Some(vec!["array"]),
        _ => None,
    }
}

fn is_type(value: &Value, wanted: &str) -> bool {
    match wanted {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.is_i64() || value.is_u64() || value.as_f64().is_some_and(|n| n.fract() == 0.0),
        _ => true,
    }
}

/// The branches of an `anyOf` or `oneOf`.
fn branches(schema: &Value) -> Option<&Vec<Value>> {
    schema
        .get("anyOf")
        .or_else(|| schema.get("oneOf"))
        .and_then(Value::as_array)
}

/// Whether `value` has a type `schema` allows. Only the value itself is looked at, not what it
/// holds: each nested value is judged against its own schema.
fn fits(schema: &Value, value: &Value) -> bool {
    if let Some(branches) = branches(schema) {
        return branches.iter().any(|branch| fits(branch, value));
    }
    types(schema).is_none_or(|types| types.iter().any(|wanted| is_type(value, wanted)))
}

fn shown(path: &str) -> String {
    if path.is_empty() {
        "the input".to_string()
    } else {
        format!("`{path}`")
    }
}

/// `value` as one of `types`, when it is a string that holds one.
fn coerced(types: &[&str], value: &Value) -> Option<(Value, &'static str)> {
    let text = value.as_str()?.trim();
    for wanted in types {
        let found = match *wanted {
            "object" | "array" => serde_json::from_str::<Value>(text)
                .ok()
                .filter(|parsed| is_type(parsed, wanted))
                .map(|parsed| {
                    let what = if parsed.is_object() {
                        "a JSON object in a string, parsed"
                    } else {
                        "a JSON array in a string, parsed"
                    };
                    (parsed, what)
                }),
            "integer" => text
                .parse::<i64>()
                .ok()
                .map(|n| (Value::from(n), "a number in a string, read as a number")),
            "number" => match text.parse::<i64>() {
                Ok(n) => Some(Value::from(n)),
                Err(_) => text
                    .parse::<f64>()
                    .ok()
                    .and_then(Number::from_f64)
                    .map(Value::Number),
            }
            .map(|n| (n, "a number in a string, read as a number")),
            "boolean" if text.eq_ignore_ascii_case("true") => {
                Some((Value::Bool(true), "`true` in a string, read as a boolean"))
            }
            "boolean" if text.eq_ignore_ascii_case("false") => {
                Some((Value::Bool(false), "`false` in a string, read as a boolean"))
            }
            _ => None,
        };
        if found.is_some() {
            return found;
        }
    }
    None
}

fn mend(schema: &Value, value: &mut Value, path: &mut String, repairs: &mut Vec<String>) {
    if let Some(branches) = branches(schema) {
        let fitting = branches.iter().find(|branch| fits(branch, value));
        let branch = match fitting {
            Some(branch) => branch,
            None => {
                let Some((branch, (mended, what))) = branches.iter().find_map(|branch| {
                    let types = types(branch)?;
                    coerced(&types, value).map(|found| (branch, found))
                }) else {
                    return;
                };
                repairs.push(format!("{} was {what}", shown(path)));
                *value = mended;
                branch
            }
        };
        mend(branch, value, path, repairs);
        return;
    }
    if !fits(schema, value)
        && let Some(types) = types(schema)
        && let Some((mended, what)) = coerced(&types, value)
    {
        repairs.push(format!("{} was {what}", shown(path)));
        *value = mended;
    }
    match value {
        Value::Object(object) => mend_object(schema, object, path, repairs),
        Value::Array(items) => {
            let Some(item_schema) = schema.get("items").filter(|items| items.is_object()) else {
                return;
            };
            for (index, item) in items.iter_mut().enumerate() {
                let len = path.len();
                path.push_str(&format!("[{index}]"));
                mend(item_schema, item, path, repairs);
                path.truncate(len);
            }
        }
        _ => {}
    }
}

fn mend_object(
    schema: &Value,
    object: &mut Map<String, Value>,
    path: &mut String,
    repairs: &mut Vec<String>,
) {
    let properties = schema.get("properties").and_then(Value::as_object);
    let required = |key: &str| {
        schema
            .get("required")
            .and_then(Value::as_array)
            .is_some_and(|required| required.iter().any(|name| name == key))
    };
    let extra = schema.get("additionalProperties");
    let closed = extra == Some(&Value::Bool(false));
    let keys: Vec<String> = object.keys().cloned().collect();
    for key in keys {
        let len = path.len();
        if !path.is_empty() {
            path.push('.');
        }
        path.push_str(&key);
        let own = properties.and_then(|properties| properties.get(&key));
        let field_schema = own.or(extra.filter(|extra| extra.is_object()));
        let null = object.get(&key).is_some_and(Value::is_null);
        if own.is_none() && closed {
            object.remove(&key);
            repairs.push(format!("`{path}` is not a field of this tool, dropped"));
        } else if null && !required(&key) && field_schema.is_some_and(|field| !fits(field, &Value::Null)) {
            object.remove(&key);
            repairs.push(format!("`{path}` was null, left out"));
        } else if let (Some(field_schema), Some(field)) = (field_schema, object.get_mut(&key)) {
            mend(field_schema, field, path, repairs);
        }
        path.truncate(len);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn repaired(schema: Value, input: Value) -> (Value, Vec<String>) {
        let mut input = input;
        let mut repairs = Vec::new();
        repair(&schema, &mut input, &mut repairs);
        (input, repairs)
    }

    fn edit_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "edits": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "old": {"type": "string"},
                            "new": {"type": "string"},
                            "replace_all": {"type": "boolean"}
                        },
                        "required": ["old", "new"]
                    }
                }
            },
            "required": ["path", "edits"]
        })
    }

    #[test]
    fn a_json_object_or_array_in_a_string_is_parsed_where_the_schema_wants_one() {
        let (input, repairs) = repaired(
            edit_schema(),
            json!({"path": "a.rs", "edits": "[{\"old\": \"a\", \"new\": \"b\"}]"}),
        );
        assert_eq!(input["edits"], json!([{"old": "a", "new": "b"}]));
        assert_eq!(repairs, ["`edits` was a JSON array in a string, parsed"]);

        // The whole input as a string.
        let (input, repairs) = repaired(edit_schema(), json!("{\"path\": \"a.rs\", \"edits\": []}"));
        assert_eq!(input, json!({"path": "a.rs", "edits": []}));
        assert_eq!(repairs, ["the input was a JSON object in a string, parsed"]);

        // A string that the schema wants as a string is left alone, JSON or not.
        let (input, repairs) = repaired(edit_schema(), json!({"path": "[1]", "edits": []}));
        assert_eq!(input["path"], "[1]");
        assert!(repairs.is_empty());
    }

    #[test]
    fn numbers_and_booleans_in_strings_are_read_as_such() {
        let schema = json!({
            "type": "object",
            "properties": {
                "limit": {"type": "integer"},
                "ratio": {"type": "number"},
                "all": {"type": "boolean"},
                "name": {"type": "string"}
            }
        });
        let (input, repairs) = repaired(
            schema.clone(),
            json!({"limit": "20", "ratio": " 0.5 ", "all": "TRUE", "name": "20"}),
        );
        assert_eq!(
            input,
            json!({"limit": 20, "ratio": 0.5, "all": true, "name": "20"})
        );
        assert_eq!(repairs.len(), 3, "{repairs:?}");

        // What cannot be read as the type stays as it is, for the tool to refuse.
        let (input, repairs) = repaired(schema, json!({"limit": "2.5", "all": "yes"}));
        assert_eq!(input, json!({"limit": "2.5", "all": "yes"}));
        assert!(repairs.is_empty());

        // Nested, inside the items of an array.
        let (input, repairs) = repaired(
            edit_schema(),
            json!({"path": "a", "edits": [{"old": "a", "new": "b", "replace_all": "false"}]}),
        );
        assert_eq!(input["edits"][0]["replace_all"], false);
        assert_eq!(
            repairs,
            ["`edits[0].replace_all` was `false` in a string, read as a boolean"]
        );
    }

    #[test]
    fn null_for_an_optional_field_is_left_out_but_not_for_a_required_one() {
        let schema = json!({
            "type": "object",
            "properties": {
                "path": {"type": "string"},
                "offset": {"type": "integer"},
                "cursor": {"type": ["string", "null"]}
            },
            "required": ["path"]
        });
        let (input, repairs) = repaired(
            schema.clone(),
            json!({"path": "a", "offset": null, "cursor": null}),
        );
        assert_eq!(input, json!({"path": "a", "cursor": null}));
        assert_eq!(repairs, ["`offset` was null, left out"]);

        let (input, repairs) = repaired(schema, json!({"path": null}));
        assert_eq!(input, json!({"path": null}));
        assert!(repairs.is_empty());
    }

    #[test]
    fn unknown_keys_are_dropped_only_from_a_closed_object() {
        let open = json!({"type": "object", "properties": {"path": {"type": "string"}}});
        let (input, repairs) = repaired(open, json!({"path": "a", "extra": 1}));
        assert_eq!(input, json!({"path": "a", "extra": 1}));
        assert!(repairs.is_empty());

        let closed = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "additionalProperties": false
        });
        let (input, repairs) = repaired(closed, json!({"path": "a", "extra": 1}));
        assert_eq!(input, json!({"path": "a"}));
        assert_eq!(repairs, ["`extra` is not a field of this tool, dropped"]);
    }

    #[test]
    fn a_value_that_fits_one_branch_of_any_of_is_left_alone() {
        let schema = json!({
            "type": "object",
            "properties": {"timeout": {"anyOf": [{"type": "integer"}, {"type": "string"}]}}
        });
        let (input, repairs) = repaired(schema.clone(), json!({"timeout": "30"}));
        assert_eq!(input, json!({"timeout": "30"}));
        assert!(repairs.is_empty());

        let schema = json!({
            "type": "object",
            "properties": {"items": {"anyOf": [{"type": "array"}, {"type": "null"}]}}
        });
        let (input, repairs) = repaired(schema, json!({"items": "[1, 2]"}));
        assert_eq!(input, json!({"items": [1, 2]}));
        assert_eq!(repairs.len(), 1);
    }

    struct Named(&'static str);

    #[async_trait::async_trait]
    impl Tool for Named {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            ""
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self, _: &Value) -> crate::tool::Access {
            crate::tool::Access::Read
        }
        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        async fn call(&self, _: Value, _: &crate::tool::ToolCtx) -> crate::tool::ToolOutput {
            crate::tool::ToolOutput::text("")
        }
    }

    #[test]
    fn other_harnesses_names_and_another_case_resolve_to_the_real_tool() {
        let tools: Vec<Arc<dyn Tool>> = ["shell", "edit", "grep", "read", "todo", "eval", "Glob"]
            .into_iter()
            .map(|name| Arc::new(Named(name)) as Arc<dyn Tool>)
            .collect();
        let found =
            |name: &str| resolve(name, &tools).map(|(tool, aliased)| (tool.name().to_string(), aliased));
        for (alias, real) in ALIASES {
            assert_eq!(found(alias), Some((real.to_string(), true)), "{alias}");
        }
        assert_eq!(found("Bash"), Some(("shell".to_string(), true)));
        assert_eq!(found("READ"), Some(("read".to_string(), true)));
        assert_eq!(found("glob"), Some(("Glob".to_string(), true)));
        assert_eq!(found("read"), Some(("read".to_string(), false)));
        assert_eq!(found("rm"), None);

        // A tool registered under an alias's name is that tool.
        let with_bash: Vec<Arc<dyn Tool>> = vec![Arc::new(Named("bash")), Arc::new(Named("shell"))];
        let (tool, aliased) = resolve("bash", &with_bash).unwrap();
        assert_eq!((tool.name(), aliased), ("bash", false));

        let old = prepare(
            "c2".into(),
            "run_code".into(),
            json!({"code": "return 1"}),
            &tools,
        );
        assert_eq!(old.name, "eval");
        assert_eq!(old.input, json!({"language": "luau", "code": "return 1"}));
        let kept = prepare(
            "c3".into(),
            "run_code".into(),
            json!({"language": "python", "code": "1"}),
            &tools,
        );
        assert_eq!(kept.input["language"], "python");

        let prepared = prepare("c1".into(), "bash".into(), json!({}), &tools);
        assert_eq!(prepared.name, "shell");
        assert_eq!(
            prepared.note().as_deref(),
            Some("note: called as `bash`; the tool is `shell`")
        );
    }
}
