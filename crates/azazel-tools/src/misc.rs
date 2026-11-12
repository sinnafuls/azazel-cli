//! `todo` and `ask_user`.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{Ui, UiError};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::util::parse_input;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TodoItem {
    pub text: String,
    pub status: TodoStatus,
}

/// Each agent's task list.
#[derive(Default)]
pub struct TodoStore {
    lists: Mutex<HashMap<String, Vec<TodoItem>>>,
}

impl TodoStore {
    pub fn get(&self, agent: &str) -> Vec<TodoItem> {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(agent)
            .cloned()
            .unwrap_or_default()
    }

    /// Replaces `agent`'s list.
    pub fn set(&self, agent: &str, items: Vec<TodoItem>) {
        self.lists
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent.to_string(), items);
    }
}

pub struct TodoTool {
    store: Arc<TodoStore>,
}

impl TodoTool {
    pub fn new(store: Arc<TodoStore>) -> Self {
        TodoTool { store }
    }
}

#[derive(Deserialize)]
struct TodoInput {
    items: Vec<TodoItem>,
}

fn render(items: &[TodoItem]) -> String {
    if items.is_empty() {
        return "todo list cleared".to_string();
    }
    items
        .iter()
        .map(|item| {
            let mark = match item.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Done => "[x]",
            };
            format!("{mark} {}", item.text)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[async_trait]
impl Tool for TodoTool {
    fn name(&self) -> &str {
        "todo"
    }

    fn description(&self) -> &str {
        "Replace your task list. Send the full list every time, each item with `text` and `status` \
         (`pending`, `in_progress` or `done`); at most one item may be `in_progress`. Use it for work \
         with three or more steps and keep it current as you go."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "items": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "text": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done"]}
                        },
                        "required": ["text", "status"]
                    }
                }
            },
            "required": ["items"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: TodoInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.items.iter().any(|item| item.text.trim().is_empty()) {
            return ToolOutput::error("every item needs text");
        }
        let active = input
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::InProgress)
            .count();
        if active > 1 {
            return ToolOutput::error(format!("{active} items are in_progress; at most one may be"));
        }
        let text = render(&input.items);
        let details = json!({"items": input.items});
        self.store.set(&ctx.agent_id, input.items);
        ToolOutput::text(text).with_details(details)
    }
}

pub struct AskUserTool;

#[derive(Deserialize)]
struct AskInput {
    question: String,
    #[serde(default)]
    options: Vec<String>,
    #[serde(default)]
    multi: bool,
}

#[async_trait]
impl Tool for AskUserTool {
    fn name(&self) -> &str {
        "ask_user"
    }

    fn description(&self) -> &str {
        "Ask the user a question and wait for the answer. Give `options` to offer choices (`multi` lets \
         them pick several); without options the user types a free answer. Ask only what you cannot find \
         out yourself and what changes what you do next."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": {"type": "string"},
                "options": {"type": "array", "items": {"type": "string"}},
                "multi": {"type": "boolean"}
            },
            "required": ["question"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }

    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: AskInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let Some(ui) = ctx.services.get::<Ui>() else {
            return ToolOutput::error(UiError::NoUser.to_string());
        };
        let answer = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            answer = ui.0.ask(&input.question, &input.options, input.multi) => answer,
        };
        match answer {
            Ok(answers) if answers.is_empty() => ToolOutput::text("(the user gave no answer)"),
            Ok(answers) => ToolOutput::text(answers.join("\n")),
            Err(UiError::Cancelled) => ToolOutput::error("the user dismissed the question"),
            Err(e) => ToolOutput::error(e.to_string()),
        }
    }
}
