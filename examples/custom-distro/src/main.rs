//! A harness of your own: azazel with an extension compiled in. Everything an extension can add is
//! on [`Registrar`]; this one adds a tool and vetoes a kind of shell command.
//!
//! ```text
//! cargo run -p custom-distro -- -p "how long is src/main.rs?"
//! ```

use std::process::ExitCode;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar};
use serde_json::{Value, json};

/// `count_lines`: how many lines a file has.
struct CountLines;

#[async_trait]
impl Tool for CountLines {
    fn name(&self) -> &str {
        "count_lines"
    }

    fn description(&self) -> &str {
        "Count the lines of a text file."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "path": { "type": "string", "description": "File to count, relative to the working directory" } },
            "required": ["path"]
        })
    }

    /// What the call touches decides how it is permitted: a read of a file inside the project
    /// needs no prompt.
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }

    /// What permission rules such as `count_lines(secrets/**)` are matched against.
    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(path) = input.get("path").and_then(Value::as_str) else {
            return ToolOutput::error("`path` is required");
        };
        match tokio::fs::read_to_string(ctx.cwd.join(path)).await {
            Ok(text) => ToolOutput::text(format!("{} lines", text.lines().count())),
            Err(e) => ToolOutput::error(format!("{path}: {e}")),
        }
    }
}

/// Runs before every tool call of every agent: refuses shell commands that ask for root.
struct NoSudo;

#[async_trait]
impl EventHandler for NoSudo {
    async fn handle(&self, event: &mut Event, _: &ExtCtx) -> Outcome {
        if let Event::ToolCall { tool, input, .. } = event
            && tool == "shell"
            && let Some(command) = input.get("command").and_then(Value::as_str)
            && command.split_whitespace().any(|word| word == "sudo")
        {
            return Outcome::Block {
                reason: "this harness does not run commands as root; find a way without sudo".to_string(),
            };
        }
        Outcome::Continue
    }
}

struct MyExt;

impl Extension for MyExt {
    fn id(&self) -> &str {
        "my-ext"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.tool(Arc::new(CountLines));
        r.on(EventKind::ToolCall, Arc::new(NoSudo));
        Ok(())
    }
}

fn main() -> ExitCode {
    azazel_cli::Builder::default().extension(MyExt).run()
}
