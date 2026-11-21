//! The gate every tool call passes: rules, the push guard, extension handlers, the mode, the user.
//! On the way back a tool's result passes it too, where secrets are taken out when that is on.

use std::borrow::Cow;
use std::sync::Arc;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, Gate, GateDecision, Services, Tool, ToolOutput};
use azazel_ai::Block;
use serde_json::Value;

use super::shell::{ShellAnalysis, analyze};
use super::{Call, PermissionState, Rule, Verdict};
use crate::ext::{Event, EventBus, Outcome};
use crate::ui::{ApprovalDecision, ApprovalRequest, Ui};

/// A service: what takes secrets out of a tool's result before the model and the session file
/// get it (`[tools] redact_output`). It returns the text with each secret replaced by
/// `[redacted:<kind>]`, or the text itself when it holds none.
pub struct OutputRedactor(pub for<'a> fn(&'a str) -> Cow<'a, str>);

/// The mark [`OutputRedactor`] leaves for each secret it hides.
const REDACTED: &str = "[redacted:";

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

    /// A command line as what the rules for `shell` are matched against.
    fn command(&self, command: String) -> Subject {
        let analysis = analyze(&command, self.posix_shell);
        Subject {
            subjects: analysis.subjects(),
            shell: Some(analysis),
            command: Some(command),
        }
    }

    fn subject(&self, tool: &dyn Tool, input: &Value) -> Subject {
        let command = match tool.name() {
            "shell" => Some(
                input
                    .get("command")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string(),
            ),
            // Typing into a program in a terminal, or stopping it, is allowed where running
            // that program is: the tool names the command that runs there.
            "pty" if tool.access(input) == Access::Exec => tool.subjects(input).into_iter().next(),
            _ => None,
        };
        match command {
            Some(command) => self.command(command),
            None => Subject {
                subjects: tool.subjects(input),
                shell: None,
                command: None,
            },
        }
    }

    /// What a `pty` call types into a terminal, as the command line it may well be: a shell
    /// is a program too. It is held to the limits no mode lifts, like a command of `shell`.
    fn typed(&self, tool: &dyn Tool, input: &Value) -> Option<Subject> {
        if tool.name() != "pty" {
            return None;
        }
        let text = input.get("text").and_then(Value::as_str)?.trim();
        (!text.is_empty()).then(|| self.command(text.to_string()))
    }

    /// The command lines the call has run later, unattended, each as the command line of a
    /// `shell` call.
    fn later(&self, tool: &dyn Tool, input: &Value) -> Vec<Subject> {
        tool.commands(input)
            .into_iter()
            .filter(|command| !command.trim().is_empty())
            .map(|command| self.command(command))
            .collect()
    }

    /// The call's subject, what it types and what it has run later, after the checks nothing
    /// overrides. The flag says whether the call writes to a protected path.
    fn checked(&self, agent: &AgentInfo, tool: &dyn Tool, input: &Value) -> Result<Checked, String> {
        let subject = self.subject(tool, input);
        let protected = self.state.precheck(&call(agent, tool, input, &subject))?;
        let typed = self.typed(tool, input);
        if let Some(typed) = &typed {
            self.state.precheck_typed(&call(agent, tool, input, typed))?;
        }
        let later = self.later(tool, input);
        for command in &later {
            self.state.precheck(&call(agent, tool, input, command))?;
        }
        Ok((subject, typed, later, protected))
    }
}

type Checked = (Subject, Option<Subject>, Vec<Subject>, bool);

fn call<'a>(agent: &'a AgentInfo, tool: &'a dyn Tool, input: &Value, subject: &'a Subject) -> Call<'a> {
    let access = tool.access(input);
    Call {
        agent,
        // Reading a web address is a fetch, and so is searching the page behind one, so the rules
        // written for `web_fetch` govern both. What is held to a command line (a `pty` call that
        // acts on a program) is governed by the rules written for `shell`.
        tool: if (tool.name() == "read" && access == Access::Net) || tool.name() == "web_grep" {
            "web_fetch"
        } else if subject.command.is_some() {
            "shell"
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
        let (mut subject, mut typed, mut later, mut protected) = match self.checked(agent, tool, input) {
            Ok(checked) => checked,
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
            (subject, typed, later, protected) = match self.checked(agent, tool, input) {
                Ok(checked) => checked,
                Err(reason) => return deny(reason),
            };
        }

        let the_call = call(agent, tool, input, &subject);
        let typed_call = typed.as_ref().map(|typed| call(agent, tool, input, typed));
        // What is typed into a terminal is asked about like a command that does the same.
        let typed_risk = typed_call
            .as_ref()
            .and_then(|typed| self.state.destructive(typed));
        let typed_secret = typed_call.as_ref().and_then(|typed| self.state.secret(typed));
        // What runs later is decided as `shell` calls of it would be; the call is asked about
        // when any of them would be.
        let later_calls: Vec<Call<'_>> = later
            .iter()
            .map(|command| call(agent, tool, input, command))
            .collect();
        let mut later_asked = false;
        for later_call in &later_calls {
            match self.state.decide(later_call, false, skip_prompt) {
                Verdict::Allow => {}
                Verdict::Deny(reason) => return deny(reason),
                Verdict::Prompt { .. } => later_asked = true,
            }
        }
        let later_risk = later_calls.iter().find_map(|later| self.state.destructive(later));
        let later_secret = later_calls.iter().find_map(|later| self.state.secret(later));
        let risk = self.state.destructive(&the_call).or(typed_risk).or(later_risk);
        // A secret file is asked about like a destructive command, but may be allowed for the
        // session; a command that is both is the stricter of the two.
        let secret = self
            .state
            .secret(&the_call)
            .or(typed_secret.clone())
            .or(later_secret)
            .filter(|_| risk.is_none());
        let asked_beside = typed_risk.is_some() || typed_secret.is_some() || later_asked;
        let rule = match self.state.decide(&the_call, protected, skip_prompt) {
            // The call itself may go ahead; what it types or runs later is asked about all the same.
            Verdict::Allow if asked_beside => None,
            Verdict::Allow => return GateDecision::Allow { input: rewritten },
            Verdict::Deny(reason) => return deny(reason),
            Verdict::Prompt { rule } => rule.filter(|_| !asked_beside),
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
            deny(match (risk, &secret) {
                (Some(risk), _) => format!(
                    "this command {risk}; it needs the user's confirmation and nobody is here to give it \
                     (`[permissions] destructive = \"allow\"` runs such commands unattended)"
                ),
                (None, Some(secret)) => format!(
                    "`{secret}` is a secret file; using it needs the user's confirmation and nobody is \
                     here to give it"
                ),
                (None, None) => format!(
                    "permission required for {}: re-run with --mode auto or add an allow rule",
                    tool.name()
                ),
            })
        };
        let request = ApprovalRequest {
            agent: agent.clone(),
            tool: tool.name().to_string(),
            access: the_call.access,
            subjects: subject.subjects.clone(),
            input: input.clone(),
            rule: rule.clone(),
            destructive: risk.is_some(),
            secret: secret.is_some(),
            note: risk
                .map(|risk| format!("destructive: this command {risk}; it is asked about in every mode"))
                .or_else(|| {
                    secret.as_ref().map(|secret| {
                        format!(
                            "secret file: `{secret}` may hold credentials; it is asked about in every \
                             mode and cannot be allowed for good"
                        )
                    })
                })
                .or_else(|| {
                    protected.then(|| "this path is protected and is asked about every time".to_string())
                })
                .or_else(|| tool.approval_note(input)),
            job: None,
        };
        // A background script is not there to answer: it pauses until the user has (and the call
        // is made again with the answer), or the call is refused, in every UI.
        let answer = if let Some(deferral) = crate::toolcall::deferral() {
            match deferral.decided(call_id, tool.name()) {
                Some(decision) => Some(decision),
                None if deferral.pauses() => return deny(deferral.defer(call_id, request)),
                None => return deny(deferral.refusal(tool.name())),
            }
        } else {
            let Some(ui) = self.services.get::<Ui>() else {
                return headless();
            };
            ui.0.approval(request).await
        };
        match answer {
            None => headless(),
            Some(ApprovalDecision::AllowOnce) => GateDecision::Allow { input: rewritten },
            Some(ApprovalDecision::AllowSession) => {
                if secret.is_some() {
                    self.state.allow_secrets_for_session(&the_call);
                    if let Some(typed) = &typed_call {
                        self.state.allow_secrets_for_session(typed);
                    }
                    for later in &later_calls {
                        self.state.allow_secrets_for_session(later);
                    }
                } else if let Some(rule) = rule.and_then(|text| text.parse::<Rule>().ok()) {
                    self.state.allow_for_session(&rule);
                }
                GateDecision::Allow { input: rewritten }
            }
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
        if let Some(redactor) = self.services.get::<OutputRedactor>() {
            redact(output, &redactor);
        }
        // The file tools report the paths they changed, relative to the agent's directory:
        // `write` and `edit` one `path`, `patch` its `files` (a deleted one is no longer there).
        let details = output.details.as_ref().filter(|_| !output.is_error);
        let changed: Vec<&str> = match (tool.name(), details) {
            ("write" | "edit", Some(details)) => {
                details.get("path").and_then(Value::as_str).into_iter().collect()
            }
            ("patch", Some(details)) => details
                .get("files")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter(|file| file.get("status").and_then(Value::as_str) != Some("deleted"))
                .filter_map(|file| file.get("path").and_then(Value::as_str))
                .collect(),
            _ => Vec::new(),
        };
        for path in changed {
            let path = self.state.cwd_of(agent).join(path);
            self.bus
                .emit(&mut Event::FileChanged {
                    agent: agent.clone(),
                    path,
                    tool: tool.name().to_string(),
                })
                .await;
        }
    }
}

/// Takes the secrets out of the text of a result, and of the diff that goes with it, and notes
/// how many there were in `details.redacted`, for the interface to say.
fn redact(output: &mut ToolOutput, redactor: &OutputRedactor) {
    let mut hidden = 0;
    let mut clean = |text: &mut String| {
        if let Cow::Owned(clean) = (redactor.0)(text) {
            hidden += clean
                .matches(REDACTED)
                .count()
                .saturating_sub(text.matches(REDACTED).count());
            *text = clean;
        }
    };
    for block in &mut output.content {
        if let Block::Text { text } = block {
            clean(text);
        }
    }
    if let Some(Value::String(diff)) = output
        .details
        .as_mut()
        .and_then(|details| details.get_mut("diff"))
    {
        clean(diff);
    }
    if hidden == 0 {
        return;
    }
    // Details of another shape belong to the tool; the text says what was hidden.
    if let Value::Object(details) = output
        .details
        .get_or_insert_with(|| Value::Object(serde_json::Map::new()))
    {
        details.insert("redacted".to_string(), hidden.into());
    }
}
