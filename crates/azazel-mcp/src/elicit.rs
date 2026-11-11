//! Answering a server's request for user input by asking through the UI bridge, one field at a time.

use azazel_core::{UiBridge, UiError};
use serde_json::{Map, Value};

/// How often a field that needs a number is asked again after an answer that is not one.
const NUMBER_ATTEMPTS: usize = 3;

#[derive(Debug, Clone, PartialEq)]
pub enum Answer {
    /// The values the user gave, keyed by property name.
    Accepted(Value),
    /// Nobody is there to ask.
    Declined,
    /// The user dismissed a question.
    Cancelled,
}

/// The choices of an enumerated property as `(value, label)`: `enum` (with optional `enumNames`),
/// or `oneOf` entries carrying `const` and `title`.
fn choices(property: &Value) -> Vec<(Value, String)> {
    let label = |value: &Value| value.as_str().map_or_else(|| value.to_string(), str::to_string);
    if let Some(values) = property.get("enum").and_then(Value::as_array) {
        let names = property.get("enumNames").and_then(Value::as_array);
        return values
            .iter()
            .enumerate()
            .map(|(index, value)| {
                let name = names.and_then(|names| names.get(index)).and_then(Value::as_str);
                (value.clone(), name.map_or_else(|| label(value), str::to_string))
            })
            .collect();
    }
    let options = property
        .get("oneOf")
        .or_else(|| property.get("anyOf"))
        .and_then(Value::as_array);
    options
        .into_iter()
        .flatten()
        .filter_map(|option| {
            let value = option.get("const")?;
            let title = option.get("title").and_then(Value::as_str);
            Some((value.clone(), title.map_or_else(|| label(value), str::to_string)))
        })
        .collect()
}

/// Asks for every property of `schema` (a JSON Schema object with primitive properties) and
/// returns the answers. Optional fields left empty are omitted.
pub async fn fill(ui: &dyn UiBridge, server: &str, message: &str, schema: &Value) -> Answer {
    let empty = Map::new();
    let properties = schema
        .get("properties")
        .and_then(Value::as_object)
        .unwrap_or(&empty);
    let required: Vec<&str> = schema
        .get("required")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .collect();
    let mut answers = Map::new();
    for (name, property) in properties {
        let title = property.get("title").and_then(Value::as_str).unwrap_or(name);
        let mut prompt = format!("{server} asks: {message}\n{title}");
        if let Some(description) = property.get("description").and_then(Value::as_str) {
            prompt.push_str(&format!(" ({description})"));
        }
        let is_required = required.contains(&name.as_str());
        if !is_required {
            prompt.push_str(" [optional]");
        }
        let kind = property.get("type").and_then(Value::as_str).unwrap_or("string");
        let options = choices(property);
        let answer = if !options.is_empty() {
            let labels: Vec<String> = options.iter().map(|(_, label)| label.clone()).collect();
            ui.select(&prompt, &labels)
                .await
                .map(|index| options.get(index).map(|(value, _)| value.clone()))
        } else if kind == "boolean" {
            ui.confirm(&prompt).await.map(|yes| Some(Value::Bool(yes)))
        } else if kind == "number" || kind == "integer" {
            ask_number(ui, &prompt, kind == "integer", is_required).await
        } else {
            ui.input(&prompt)
                .await
                .map(|text| (is_required || !text.trim().is_empty()).then_some(Value::String(text)))
        };
        match answer {
            Ok(Some(value)) => {
                answers.insert(name.clone(), value);
            }
            Ok(None) => {}
            Err(UiError::NoUser) => return Answer::Declined,
            Err(UiError::Cancelled) => return Answer::Cancelled,
        }
    }
    Answer::Accepted(Value::Object(answers))
}

async fn ask_number(
    ui: &dyn UiBridge,
    prompt: &str,
    integer: bool,
    required: bool,
) -> Result<Option<Value>, UiError> {
    let mut prompt = prompt.to_string();
    for _ in 0..NUMBER_ATTEMPTS {
        let typed = ui.input(&prompt).await?;
        let typed = typed.trim();
        if typed.is_empty() && !required {
            return Ok(None);
        }
        let parsed = if integer {
            typed.parse::<i64>().ok().map(Value::from)
        } else {
            typed
                .parse::<f64>()
                .ok()
                .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
        };
        if let Some(value) = parsed {
            return Ok(Some(value));
        }
        if !prompt.ends_with("(enter a number)") {
            prompt.push_str(" (enter a number)");
        }
    }
    Err(UiError::Cancelled)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use azazel_core::{ApprovalDecision, ApprovalRequest, Headless, Level};
    use serde_json::json;

    use super::*;

    /// Answers questions from a script and records the prompts it was shown.
    #[derive(Default)]
    struct Scripted {
        answers: Mutex<VecDeque<Result<String, UiError>>>,
        prompts: Mutex<Vec<String>>,
    }

    impl Scripted {
        fn new(answers: &[Result<&str, UiError>]) -> Self {
            let answers = answers.iter().map(|a| a.clone().map(str::to_string)).collect();
            Scripted {
                answers: Mutex::new(answers),
                prompts: Mutex::default(),
            }
        }

        fn next(&self, prompt: &str) -> Result<String, UiError> {
            self.prompts.lock().unwrap().push(prompt.to_string());
            self.answers
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or(Err(UiError::Cancelled))
        }
    }

    #[async_trait]
    impl UiBridge for Scripted {
        async fn ask(&self, question: &str, _: &[String], _: bool) -> Result<Vec<String>, UiError> {
            self.next(question).map(|answer| vec![answer])
        }
        async fn confirm(&self, prompt: &str) -> Result<bool, UiError> {
            self.next(prompt).map(|answer| answer == "yes")
        }
        async fn select(&self, prompt: &str, _: &[String]) -> Result<usize, UiError> {
            self.next(prompt).map(|answer| answer.parse().unwrap())
        }
        async fn input(&self, prompt: &str) -> Result<String, UiError> {
            self.next(prompt)
        }
        fn notify(&self, _: Level, _: &str) {}
        async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
            None
        }
    }

    fn schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "title": "Your name", "description": "as on the badge" },
                "age": { "type": "integer" },
                "ratio": { "type": "number" },
                "subscribe": { "type": "boolean" },
                "size": { "type": "string", "enum": ["s", "m", "l"], "enumNames": ["Small", "Medium", "Large"] },
                "plan": { "oneOf": [{ "const": 1, "title": "Free" }, { "const": 2, "title": "Paid" }] },
                "note": { "type": "string" }
            },
            "required": ["name", "age"]
        })
    }

    #[tokio::test]
    async fn each_property_is_asked_in_order_with_the_right_kind_of_question() {
        let ui = Scripted::new(&[
            Ok("Ada"),
            Ok("x"),
            Ok("36"),
            Ok(""),
            Ok("yes"),
            Ok("2"),
            Ok("1"),
            Ok(" "),
        ]);
        let answer = fill(&ui, "forms", "Sign up", &schema()).await;
        assert_eq!(
            answer,
            Answer::Accepted(json!({ "name": "Ada", "age": 36, "subscribe": true, "size": "l", "plan": 2 }))
        );
        let prompts = ui.prompts.lock().unwrap().clone();
        assert_eq!(prompts[0], "forms asks: Sign up\nYour name (as on the badge)");
        assert_eq!(prompts[1], "forms asks: Sign up\nage");
        assert_eq!(prompts[2], "forms asks: Sign up\nage (enter a number)");
        assert_eq!(prompts[3], "forms asks: Sign up\nratio [optional]");
        assert_eq!(prompts.len(), 8);
    }

    #[tokio::test]
    async fn no_user_declines_and_a_dismissed_question_cancels() {
        assert_eq!(
            fill(&Headless, "forms", "Sign up", &schema()).await,
            Answer::Declined
        );
        let dismissed = Scripted::new(&[Ok("Ada"), Err(UiError::Cancelled)]);
        assert_eq!(
            fill(&dismissed, "forms", "Sign up", &schema()).await,
            Answer::Cancelled
        );
        let stubborn = Scripted::new(&[Ok("Ada"), Ok("a"), Ok("b"), Ok("c")]);
        assert_eq!(
            fill(&stubborn, "forms", "Sign up", &schema()).await,
            Answer::Cancelled
        );
        // A schema without properties needs no questions.
        assert_eq!(
            fill(&Headless, "forms", "ok?", &json!({ "type": "object" })).await,
            Answer::Accepted(json!({}))
        );
    }
}
