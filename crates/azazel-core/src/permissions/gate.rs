//! The gate every tool call passes: rules, the push guard, extension handlers, the mode, the user.

use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Gate, GateDecision, Services, Tool, ToolOutput};
use azazel_ai::Block;
use serde_json::Value;

use super::shell::{ShellAnalysis, analyze};
use super::{Call, PermissionState, Rule, Verdict};
use crate::ext::{Event, EventBus, Outcome};
use crate::ui::{ApprovalDecision, ApprovalRequest, Ui};

pub struct PermissionGate {
    state: Arc<PermissionState>,
    bus: Arc<EventBus>,
    services: Arc<Services>,
    /// Whether agents' commands run in a POSIX shell (and can therefore be parsed).
    posix_shell: bool,
}

struct Subject {
    subjects: Vec<String>,
    shell: Option<ShellAnalysis>,
    command: Option<String>,
}

impl PermissionGate {
    pub fn new(
        state: Arc<PermissionState>,
        bus: Arc<EventBus>,
        services: Arc<Services>,
        posix_shell: bool,
    ) -> Self {
        PermissionGate {
            state,
            bus,
            services,
            posix_shell,
        }
    }

    fn subject(&self, tool: &dyn Tool, input: &Value) -> Subject {
        if tool.name() == "shell" {
            let command = input
                .get("command")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            let analysis = analyze(&command, self.posix_shell);
            return Subject {
                subjects: analysis.subjects(),
                shell: Some(analysis),
                command: Some(command),
            };
        }
        Subject {
            subjects: tool.subjects(input),
            shell: None,
            command: None,
        }
    }
}

fn call<'a>(agent: &'a AgentInfo, tool: &'a dyn Tool, input: &Value, subject: &'a Subject) -> Call<'a> {
    let access = tool.access(input);
    Call {
        agent,
        // Reading a web address is a fetch, so the rules written for `web_fetch` govern it.
        tool: if tool.name() == "read" && access == Access::Net {
            "web_fetch"
        } else {
            tool.name()
        },
        access,
        subjects: &subject.subjects,
        shell: subject.shell.as_ref(),
        command: subject.command.as_deref(),
    }
}

#[async_trait]
impl Gate for PermissionGate {
    async fn before(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, input: &Value) -> GateDecision {
        let deny = |reason: String| GateDecision::Deny { reason };

        // Deny rules, the push guard, protected paths.
        let mut subject = self.subject(tool, input);
        let mut protected = match self.state.precheck(&call(agent, tool, input, &subject)) {
            Ok(protected) => protected,
            Err(reason) => return deny(reason),
        };

        // Extension handlers: may block, rewrite the input, or waive the prompt.
        let mut event = Event::ToolCall {
            agent: agent.clone(),
            call_id: call_id.to_string(),
            tool: tool.name().to_string(),
            input: input.clone(),
            skip_prompt: false,
        };
        if let Outcome::Block { reason } = self.bus.emit(&mut event).await {
            return deny(reason);
        }
        let Event::ToolCall {
            input: handled,
            skip_prompt,
            ..
        } = event
        else {
            return deny("an extension replaced the tool-call event".to_string());
        };
        let rewritten = (&handled != input).then_some(handled);
        let input = rewritten.as_ref().unwrap_or(input);
        if rewritten.is_some() {
            // What a handler turned the call into is held to the same hard limits.
            subject = self.subject(tool, input);
            protected = match self.state.precheck(&call(agent, tool, input, &subject)) {
                Ok(protected) => protected,
                Err(reason) => return deny(reason),
            };
        }

        let the_call = call(agent, tool, input, &subject);
        let rule = match self.state.decide(&the_call, protected, skip_prompt) {
            Verdict::Allow => return GateDecision::Allow { input: rewritten },
            Verdict::Deny(reason) => return deny(reason),
            Verdict::Prompt { rule } => rule,
        };

        let mut request = Event::PermissionRequest {
            agent: agent.clone(),
            tool: tool.name().to_string(),
            input: rewritten.clone().unwrap_or_else(|| input.clone()),
            subjects: subject.subjects.clone(),
            decision: None,
        };
        if let Outcome::Block { reason } = self.bus.emit(&mut request).await {
            return deny(reason);
        }
        match request {
            Event::PermissionRequest {
                decision: Some(true), ..
            } => return GateDecision::Allow { input: rewritten },
            Event::PermissionRequest {
                decision: Some(false),
                ..
            } => {
                return deny("denied by a permission hook".to_string());
            }
            _ => {}
        }

        let headless = || {
            deny(format!(
                "permission required for {}: re-run with --mode auto or add an allow rule",
                tool.name()
            ))
        };
        let Some(ui) = self.services.get::<Ui>() else {
            return headless();
        };
        let answer =
            ui.0.approval(ApprovalRequest {
                agent: agent.clone(),
                tool: tool.name().to_string(),
                access: the_call.access,
                subjects: subject.subjects.clone(),
                input: input.clone(),
                rule: rule.clone(),
                note: protected
                    .then(|| "this path is protected and is asked about every time".to_string())
                    .or_else(|| tool.approval_note(input)),
            })
            .await;
        match answer {
            None => headless(),
            Some(ApprovalDecision::AllowOnce) => GateDecision::Allow { input: rewritten },
            Some(ApprovalDecision::Always) => {
                if let Some(rule) = rule.and_then(|text| text.parse::<Rule>().ok())
                    && let Err(e) = self.state.remember_allow(&rule)
                {
                    tracing::warn!("could not save rule `{rule}`: {e}");
                }
                GateDecision::Allow { input: rewritten }
            }
            Some(ApprovalDecision::Deny { feedback }) => {
                deny(match feedback.filter(|f| !f.trim().is_empty()) {
                    Some(feedback) => format!("the user denied this call: {feedback}"),
                    None => "the user denied this call".to_string(),
                })
            }
        }
    }

    async fn after(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, output: &mut ToolOutput) {
        let mut event = Event::ToolResult {
            agent: agent.clone(),
            call_id: call_id.to_string(),
            tool: tool.name().to_string(),
            output: std::mem::take(output),
        };
        let outcome = self.bus.emit(&mut event).await;
        if let Event::ToolResult { output: handled, .. } = event {
            *output = handled;
        }
        if let Outcome::Block { reason } = outcome {
            output.content.push(Block::text(reason));
        }
    }
}
