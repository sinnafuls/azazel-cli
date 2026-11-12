//! Typed results: an agent that was given a JSON Schema hands in its result by calling `report`
//! with a value that matches it, instead of writing prose.

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{Event, EventHandler, ExtCtx, Outcome};
use serde_json::{Value, json};

use crate::team::Team;

/// The tool an agent with a result schema reports through. Only that agent is offered it.
pub const REPORT_TOOL: &str = "report";
/// How often an agent that stops without a report is told to make one.
pub const MAX_REMINDERS: u32 = 3;
/// Why an agent failed when it never reported.
pub const NO_REPORT: &str = "ended without a report";
/// Violations listed in one refusal; a value that is wrong all over needs no longer list.
const MAX_VIOLATIONS: usize = 20;

#[derive(Default)]
struct State {
    value: Option<Value>,
    reminders: u32,
}

/// The result one agent owes: its schema, and the value once the agent has reported one that
/// matches.
pub struct Typed {
    schema: Value,
    validator: jsonschema::Validator,
    state: Mutex<State>,
}

impl std::fmt::Debug for Typed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Typed").field("schema", &self.schema).finish()
    }
}

impl Typed {
    /// `Err` says why `schema` is not a usable JSON Schema. A schema that refers to a file or a
    /// URL is refused: nothing is fetched for a schema.
    pub fn new(schema: Value) -> Result<Typed, String> {
        if !schema.is_object() {
            return Err("a JSON Schema must be an object".to_string());
        }
        let validator = jsonschema::validator_for(&schema).map_err(|e| e.to_string())?;
        Ok(Typed {
            schema,
            validator,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub fn schema(&self) -> &Value {
        &self.schema
    }

    /// What is wrong with `value`, one line per violation; empty when it matches.
    fn violations(&self, value: &Value) -> Vec<String> {
        self.validator
            .iter_errors(value)
            .take(MAX_VIOLATIONS)
            .map(|error| {
                let at = error.instance_path().to_string();
                let at = if at.is_empty() { "(the value)" } else { at.as_str() };
                format!("{at}: {error}")
            })
            .collect()
    }

    /// Takes `value` as the result when it matches the schema; otherwise returns the violations.
    /// A later report replaces an earlier one.
    pub fn submit(&self, value: Value) -> Result<(), Vec<String>> {
        let mut violations = self.violations(&value);
        // Models sometimes send the value as a string of JSON; that is the value they mean.
        let value = match &value {
            Value::String(text) if !violations.is_empty() => match serde_json::from_str::<Value>(text) {
                Ok(parsed) if self.violations(&parsed).is_empty() => {
                    violations.clear();
                    parsed
                }
                _ => value,
            },
            _ => value,
        };
        if !violations.is_empty() {
            return Err(violations);
        }
        self.state().value = Some(value);
        Ok(())
    }

    /// The reported value, once there is one.
    pub fn value(&self) -> Option<Value> {
        self.state().value.clone()
    }

    /// The reported value as indented JSON.
    pub fn json(&self) -> Option<String> {
        self.value()
            .map(|value| serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string()))
    }

    /// Starts over for a new piece of work: the agent owes a report again.
    pub fn reset(&self) {
        *self.state() = State::default();
    }

    /// What to tell an agent that stops without having reported; `None` once it has, or when it
    /// was told [`MAX_REMINDERS`] times already.
    pub fn reminder(&self) -> Option<String> {
        let mut state = self.state();
        if state.value.is_some() || state.reminders >= MAX_REMINDERS {
            return None;
        }
        state.reminders += 1;
        Some(format!(
            "You have not handed in your result. Call `{REPORT_TOOL}` with a value that matches its schema; \
             text alone is not a result. (Reminder {} of {MAX_REMINDERS}.)",
            state.reminders
        ))
    }
}

/// `report`, for the one agent `Typed` belongs to.
pub struct ReportTool(pub Arc<Typed>);

#[async_trait]
impl Tool for ReportTool {
    fn name(&self) -> &str {
        REPORT_TOOL
    }

    fn description(&self) -> &str {
        "Hand in your result. `result` must match the schema given here; a value that does not is refused \
         with what is wrong, so you can correct it and call again. Call it when your work is done: the \
         value is what your caller receives."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "result": self.0.schema() },
            "required": ["result"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    async fn call(&self, mut input: Value, _: &ToolCtx) -> ToolOutput {
        let Some(result) = input.get_mut("result").map(Value::take) else {
            return ToolOutput::error("`result` is missing: pass the value as `result`");
        };
        match self.0.submit(result) {
            Ok(()) => ToolOutput::text("The result is recorded. End your turn now."),
            Err(violations) => ToolOutput::error(format!(
                "the result does not match the schema:\n- {}\nCorrect it and call `{REPORT_TOOL}` again.",
                violations.join("\n- ")
            )),
        }
    }
}

/// Keeps an agent that owes a typed result going until it has reported, a few times at most.
pub(crate) struct Reminder;

#[async_trait]
impl EventHandler for Reminder {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        let Event::Stop { agent, .. } = event else {
            return Outcome::Continue;
        };
        let reminder = ctx
            .services
            .get::<Team>()
            .and_then(|team| team.typed(&agent.id))
            .and_then(|typed| typed.reminder());
        match reminder {
            Some(reason) => Outcome::Block { reason },
            None => Outcome::Continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed() -> Typed {
        let schema = json!({
            "type": "object",
            "properties": {
                "files": { "type": "array", "items": { "type": "string" } },
                "count": { "type": "integer", "minimum": 0 }
            },
            "required": ["files", "count"],
            "additionalProperties": false
        });
        Typed::new(schema).unwrap()
    }

    fn ctx() -> ToolCtx {
        let agent = azazel_agent::AgentInfo {
            id: "scout-1".into(),
            def: "scout".into(),
            parent: Some("lead".into()),
            depth: 1,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: std::path::PathBuf::from("."),
            cancel: tokio_util::sync::CancellationToken::new(),
            update: azazel_agent::UpdateSink::default(),
            services: Arc::new(azazel_agent::Services::new()),
        }
    }

    #[test]
    fn a_value_is_taken_only_when_it_matches_and_each_violation_names_its_place() {
        let typed = typed();
        let violations = typed
            .submit(json!({ "files": ["a.rs", 3], "count": -1, "extra": true }))
            .unwrap_err();
        assert_eq!(violations.len(), 3, "{violations:?}");
        assert!(
            violations.iter().any(|v| v.starts_with("/files/1: ")),
            "{violations:?}"
        );
        assert!(
            violations.iter().any(|v| v.starts_with("/count: ")),
            "{violations:?}"
        );
        assert!(
            violations
                .iter()
                .any(|v| v.starts_with("(the value): ") && v.contains("extra")),
            "{violations:?}"
        );
        assert_eq!(typed.value(), None);

        assert_eq!(typed.submit(json!({ "files": ["a.rs"], "count": 1 })), Ok(()));
        assert_eq!(typed.value(), Some(json!({ "files": ["a.rs"], "count": 1 })));
        // A later report replaces the earlier one; a refused one leaves it.
        assert_eq!(typed.submit(json!({ "files": [], "count": 0 })), Ok(()));
        assert!(typed.submit(json!("nonsense")).is_err());
        assert_eq!(typed.value(), Some(json!({ "files": [], "count": 0 })));
        assert_eq!(
            typed.json().as_deref(),
            Some("{\n  \"files\": [],\n  \"count\": 0\n}")
        );
    }

    #[test]
    fn a_value_sent_as_a_string_of_json_is_read_as_the_value() {
        let typed = typed();
        assert_eq!(typed.submit(json!("{\"files\": [], \"count\": 2}")), Ok(()));
        assert_eq!(typed.value(), Some(json!({ "files": [], "count": 2 })));
        // Where the schema wants a string, a string stays one.
        let text = Typed::new(json!({ "type": "string" })).unwrap();
        assert_eq!(text.submit(json!("{\"a\": 1}")), Ok(()));
        assert_eq!(text.value(), Some(json!("{\"a\": 1}")));
    }

    #[test]
    fn a_schema_that_is_not_one_or_points_elsewhere_is_refused() {
        assert!(Typed::new(json!("object")).is_err_and(|why| why.contains("must be an object")));
        assert!(Typed::new(json!({ "type": "nonsense" })).is_err());
        // Nothing is fetched for a schema.
        assert!(Typed::new(json!({ "$ref": "https://example.invalid/schema.json" })).is_err());
    }

    #[test]
    fn reminders_stop_after_three_and_once_there_is_a_report() {
        let typed = typed();
        for n in 1..=MAX_REMINDERS {
            let reminder = typed.reminder().unwrap();
            assert!(reminder.contains(&format!("Reminder {n} of 3")), "{reminder}");
        }
        assert_eq!(typed.reminder(), None);

        typed.reset();
        assert!(typed.reminder().is_some());
        assert_eq!(typed.submit(json!({ "files": [], "count": 0 })), Ok(()));
        assert_eq!(typed.reminder(), None);
        // A new piece of work owes a new report.
        typed.reset();
        assert_eq!(typed.value(), None);
        assert!(typed.reminder().is_some());
    }

    #[tokio::test]
    async fn the_tool_wraps_the_schema_and_says_what_is_wrong() {
        let tool = ReportTool(Arc::new(typed()));
        assert_eq!(
            tool.schema()["properties"]["result"]["required"],
            json!(["files", "count"])
        );
        let ctx = ctx();
        let missing = tool.call(json!({ "files": [] }), &ctx).await;
        assert!(missing.is_error && missing.text_content().contains("`result` is missing"));
        let wrong = tool
            .call(json!({ "result": { "files": [], "count": "two" } }), &ctx)
            .await;
        assert!(wrong.is_error, "{}", wrong.text_content());
        assert!(
            wrong.text_content().contains("- /count: "),
            "{}",
            wrong.text_content()
        );
        let right = tool
            .call(json!({ "result": { "files": [], "count": 2 } }), &ctx)
            .await;
        assert!(!right.is_error);
        assert_eq!(tool.0.value(), Some(json!({ "files": [], "count": 2 })));
    }
}
