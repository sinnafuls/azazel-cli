//! Plan mode's exit: the `plan_submit` tool, and the extension that installs the permission pieces.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Tool, ToolCtx, ToolOutput};
use serde_json::{Value, json};

use super::PermissionState;
use crate::config::PermissionMode;
use crate::ext::{ExtError, Extension, Registrar};
use crate::prompt::SystemPartProvider;
use crate::ui::{PlanDecision, Ui, UiError};

pub const PLAN_APPROVED: &str = "approved; implement the plan";

pub struct PlanSubmitTool {
    state: Arc<PermissionState>,
}

#[async_trait]
impl Tool for PlanSubmitTool {
    fn name(&self) -> &str {
        "plan_submit"
    }

    fn description(&self) -> &str {
        "Submit the plan file you wrote under `.azazel/plans/` for the user's approval. On approval you \
         leave plan mode and implement it; otherwise you get the user's feedback to revise the plan."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {"path": {"type": "string", "description": "Path of the plan file"}},
            "required": ["path"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Write
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("path")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let Some(given) = input.get("path").and_then(Value::as_str) else {
            return ToolOutput::error("invalid arguments: `path` is required");
        };
        let path = if Path::new(given).is_absolute() {
            Path::new(given).to_path_buf()
        } else {
            ctx.cwd.join(given)
        };
        if !self.state.is_plan_file(&path) {
            return ToolOutput::error("the plan must be a file under .azazel/plans/");
        }
        let plan = match tokio::fs::read_to_string(&path).await {
            Ok(plan) if !plan.trim().is_empty() => plan,
            Ok(_) => return ToolOutput::error(format!("{} is empty; write the plan first", path.display())),
            Err(e) => return ToolOutput::error(format!("{}: {e}", path.display())),
        };
        let Some(ui) = ctx.services.get::<Ui>() else {
            return unattended(&path);
        };
        let decision = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error("aborted by user"),
            decision = ui.0.review_plan(&path, &plan) => decision,
        };
        match decision {
            Ok(PlanDecision::Approve) => {
                self.state.set_mode(PermissionMode::AcceptEdits);
                ToolOutput::text(PLAN_APPROVED)
            }
            Ok(PlanDecision::Revise(feedback)) => ToolOutput::text(format!(
                "the user wants the plan revised before approving it: {feedback}"
            )),
            Err(UiError::NoUser) => unattended(&path),
            Err(UiError::Cancelled) => ToolOutput::text(
                "the user closed the plan without deciding; ask what they would like changed before submitting again",
            ),
        }
    }
}

fn unattended(path: &Path) -> ToolOutput {
    ToolOutput::text(format!(
        "plan saved at {}; nobody is attached to approve it, so stop here and summarise the plan",
        path.display()
    ))
}

struct ModePart(Arc<PermissionState>);

#[async_trait]
impl SystemPartProvider for ModePart {
    async fn render(&self, agent: &AgentInfo) -> Option<String> {
        self.0.mode_prompt(agent)
    }
}

/// Registers the permission state as a service, the `plan_submit` tool and the `mode` prompt part.
pub struct PermissionsExt {
    pub state: Arc<PermissionState>,
}

impl Extension for PermissionsExt {
    fn id(&self) -> &str {
        "permissions"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.state.clone());
        r.tool(Arc::new(PlanSubmitTool {
            state: self.state.clone(),
        }));
        r.system_part("mode", Arc::new(ModePart(self.state.clone())));
        Ok(())
    }
}
