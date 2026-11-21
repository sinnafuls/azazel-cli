//! Calling a tool on an agent's behalf from outside the agent loop (a Luau script run by `eval`, a
//! Luau extension), through the same gate a call the model makes itself passes.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Gate, GateDecision, Services, Tool, ToolCtx, ToolOutput, ToolSource};
use serde_json::Value;

use crate::ext::ToolRegistry;
use crate::ui::{ApprovalDecision, ApprovalRequest};

/// The Luau engine behind `eval`'s `luau` language. It lives in `azazel-lua`; `azazel-tools` reaches
/// it through the [`Luau`] service, so a build without the `lua` feature simply has none.
#[async_trait]
pub trait LuauEngine: Send + Sync {
    /// The Luau part of `eval`'s description; built once, after `ToolCaller` exists. `hidden` holds
    /// the tools that are callable only from a script (empty unless the tools are code-first).
    fn describe(&self, services: &Services, hidden: &[Arc<dyn Tool>]) -> String;
    /// Runs `code` as the agent of `ctx`.
    async fn run(&self, code: String, ctx: &ToolCtx) -> ToolOutput;
    /// Starts `code` as a background job of the agent of `ctx` and returns at once with the job's
    /// id. The job ends after `background.max` of wall-clock time spent running (never when it is
    /// zero), and the agent is woken with its result. `background` also says whether a call that
    /// needs approval pauses the script.
    fn start(&self, code: String, ctx: &ToolCtx, background: Background) -> ToolOutput;
}

/// The service that holds the engine.
pub struct Luau(pub Arc<dyn LuauEngine>);

/// How `eval` runs a background script.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Background {
    /// Wall-clock time the passes of the script may take; zero is no limit.
    pub max: Duration,
    /// Whether a call that needs approval pauses the script (false: the call is refused).
    pub pause: bool,
    /// Longest a paused script waits for an answer; zero waits until the session ends.
    pub pause_max: Duration,
}

#[derive(Default)]
struct DeferState {
    pending: HashMap<String, ApprovalRequest>,
    decided: HashMap<String, (String, ApprovalDecision)>,
    blocked: Option<String>,
}

/// What a background script does at a call the gate would ask about: the gate records the request
/// and refuses the call for this pass (a script that pauses), or only refuses it. The answer the
/// user gives is handed back with [`Deferral::decide`], and the gate applies it when the script
/// runs again and makes the call again.
pub struct Deferral {
    pause: bool,
    state: Mutex<DeferState>,
}

impl Deferral {
    pub fn new(pause: bool) -> Arc<Deferral> {
        Arc::new(Deferral {
            pause,
            state: Mutex::default(),
        })
    }

    fn state(&self) -> std::sync::MutexGuard<'_, DeferState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether calls that would ask pause the run (false: they are refused).
    pub fn pauses(&self) -> bool {
        self.pause && self.state().blocked.is_none()
    }

    /// The log of the run can no longer be replayed, for this reason; from now on calls that would
    /// ask are refused.
    pub fn block(&self, why: String) {
        self.state().blocked.get_or_insert(why);
    }

    /// The user's answer for this call, taken once; `None` when there is none or it was for
    /// another tool.
    pub fn decided(&self, call_id: &str, tool: &str) -> Option<ApprovalDecision> {
        let mut state = self.state();
        match state.decided.remove(call_id) {
            Some((decided_tool, decision)) if decided_tool == tool => Some(decision),
            _ => None,
        }
    }

    pub fn decide(&self, call_id: &str, tool: &str, decision: ApprovalDecision) {
        self.state()
            .decided
            .insert(call_id.to_string(), (tool.to_string(), decision));
    }

    /// Records the request of a call that must wait for the user and returns the refusal the call
    /// gets in this pass.
    pub fn defer(&self, call_id: &str, request: ApprovalRequest) -> String {
        let reason = format!(
            "{} needs the user's approval; the script is paused until it is given",
            request.tool
        );
        self.state().pending.insert(call_id.to_string(), request);
        reason
    }

    /// The request [`Deferral::defer`] recorded for this call, if the call was deferred.
    pub fn take_pending(&self, call_id: &str) -> Option<ApprovalRequest> {
        self.state().pending.remove(call_id)
    }

    /// What a call that would ask is told when the script does not pause.
    pub fn refusal(&self, tool: &str) -> String {
        let mut text = format!(
            "{tool} needed approval, which a background script cannot get; run it in the foreground \
             (without `background`) or allow it with a rule"
        );
        if self.pause
            && let Some(why) = &self.state().blocked
        {
            text.push_str(&format!("; this run cannot pause because {why}"));
        }
        text
    }
}

tokio::task_local! {
    static DEFERRAL: Arc<Deferral>;
}

/// Runs `work` so that a tool call it makes through [`ToolCaller`] that the gate would ask about
/// is handled by `deferral` instead of asking.
pub async fn deferring<F: std::future::Future>(deferral: Arc<Deferral>, work: F) -> F::Output {
    DEFERRAL.scope(deferral, work).await
}

/// The deferral of the code asking, inside [`deferring`].
pub fn deferral() -> Option<Arc<Deferral>> {
    DEFERRAL.try_with(Arc::clone).ok()
}

/// How long a cancelled tool gets to wind down before its task is dropped.
const ABORT_GRACE: Duration = Duration::from_secs(2);

/// Runs one tool call: lookup, `gate.before`, the call, `gate.after`. Registered as a service.
pub struct ToolCaller {
    pub tools: Arc<ToolRegistry>,
    pub gate: Arc<dyn Gate>,
    /// The tools of agents that are offered fewer than the registry holds, by agent id.
    scopes: RwLock<HashMap<String, Arc<dyn ToolSource>>>,
}

impl ToolCaller {
    pub fn new(tools: Arc<ToolRegistry>, gate: Arc<dyn Gate>) -> ToolCaller {
        ToolCaller {
            tools,
            gate,
            scopes: RwLock::default(),
        }
    }

    /// Limits what `agent` can reach through this service to `tools`, so a script cannot call a
    /// tool the agent itself is not offered.
    pub fn scope_agent(&self, agent: &str, tools: Arc<dyn ToolSource>) {
        self.scopes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(agent.to_string(), tools);
    }

    pub fn forget_agent(&self, agent: &str) {
        self.scopes
            .write()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(agent);
    }

    /// The tools the calling agent of `ctx` is offered.
    pub fn available(&self, ctx: &ToolCtx) -> Vec<Arc<dyn Tool>> {
        let scope = self
            .scopes
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&ctx.agent.id)
            .cloned();
        match scope {
            Some(scope) => scope.tools(&ctx.agent),
            None => self.tools.tools(&ctx.agent),
        }
    }

    /// Calls tool `name` with `input` as the agent of `ctx`; `ctx.call_id` identifies the call to
    /// the gate and to hooks. A refusal, an unknown tool and a cancelled call are error results.
    pub async fn call(&self, ctx: &ToolCtx, name: &str, input: Value) -> ToolOutput {
        let Some(tool) = self.available(ctx).into_iter().find(|tool| tool.name() == name) else {
            return ToolOutput::error(format!("unknown tool `{name}`"));
        };
        if ctx.cancel.is_cancelled() {
            return ToolOutput::error(ABORTED_TEXT);
        }
        let decision = tokio::select! {
            _ = ctx.cancel.cancelled() => return ToolOutput::error(ABORTED_TEXT),
            decision = self.gate.before(&ctx.agent, tool.as_ref(), &ctx.call_id, &input) => decision,
        };
        let input = match decision {
            GateDecision::Deny { reason } => return ToolOutput::error(reason),
            GateDecision::Allow {
                input: Some(replacement),
            } => replacement,
            GateDecision::Allow { input: None } => input,
        };
        // A task of its own, so a panicking tool fails this call and nothing else.
        let (runner, call_ctx) = (tool.clone(), ctx.clone());
        let mut task = tokio::spawn(async move { runner.call(input, &call_ctx).await });
        let mut output = tokio::select! {
            joined = &mut task => match joined {
                Ok(output) => output,
                Err(e) if e.is_panic() => ToolOutput::error(format!("tool `{name}` panicked")),
                Err(_) => ToolOutput::error(ABORTED_TEXT),
            },
            _ = ctx.cancel.cancelled() => {
                if tokio::time::timeout(ABORT_GRACE, &mut task).await.is_err() {
                    task.abort();
                }
                return ToolOutput::error(ABORTED_TEXT);
            }
        };
        self.gate
            .after(&ctx.agent, tool.as_ref(), &ctx.call_id, &mut output)
            .await;
        output
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::Mutex;

    use async_trait::async_trait;
    use azazel_agent::{Access, AgentInfo, Services, UpdateSink};
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    use super::*;

    struct Echo(&'static str);

    #[async_trait]
    impl Tool for Echo {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "test"
        }
        fn schema(&self) -> Value {
            json!({"type": "object"})
        }
        fn access(&self, _: &Value) -> Access {
            Access::Read
        }
        fn subjects(&self, _: &Value) -> Vec<String> {
            Vec::new()
        }
        async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
            if input.get("panic").is_some() {
                panic!("boom");
            }
            ToolOutput::text(format!("{} {} {input}", self.0, ctx.call_id))
        }
    }

    /// Denies `secret`, rewrites the input of `echo`, and marks every result.
    #[derive(Default)]
    struct Recorder(Mutex<Vec<String>>);

    #[async_trait]
    impl Gate for Recorder {
        async fn before(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, _: &Value) -> GateDecision {
            self.0
                .lock()
                .unwrap()
                .push(format!("before {} {} {call_id}", agent.id, tool.name()));
            match tool.name() {
                "secret" => GateDecision::Deny {
                    reason: "not for you".to_string(),
                },
                _ => GateDecision::Allow {
                    input: Some(json!({"rewritten": true})),
                },
            }
        }

        async fn after(&self, _: &AgentInfo, tool: &dyn Tool, _: &str, output: &mut ToolOutput) {
            self.0.lock().unwrap().push(format!("after {}", tool.name()));
            output.content.push(azazel_ai::Block::text("[seen]"));
        }
    }

    fn ctx(agent: &str, call_id: &str) -> ToolCtx {
        let info = AgentInfo {
            id: agent.into(),
            def: agent.into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: call_id.into(),
            cwd: PathBuf::from("."),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: Arc::new(Services::new()),
        }
    }

    fn caller() -> (ToolCaller, Arc<Recorder>) {
        let tools = Arc::new(ToolRegistry::default());
        tools.add("test", Arc::new(Echo("echo")));
        tools.add("test", Arc::new(Echo("secret")));
        let gate = Arc::new(Recorder::default());
        (ToolCaller::new(tools, gate.clone()), gate)
    }

    #[tokio::test]
    async fn a_call_passes_the_gate_before_and_after_and_runs_with_the_gates_input() {
        let (caller, gate) = caller();
        let output = caller.call(&ctx("lead", "c1"), "echo", json!({"a": 1})).await;
        assert!(!output.is_error);
        assert_eq!(output.text_content(), "echo c1 {\"rewritten\":true}\n[seen]");
        assert_eq!(*gate.0.lock().unwrap(), ["before lead echo c1", "after echo"]);
    }

    #[tokio::test]
    async fn refusals_unknown_tools_panics_and_cancelled_calls_are_error_results() {
        let (caller, gate) = caller();
        let denied = caller.call(&ctx("lead", "c1"), "secret", json!({})).await;
        assert!(denied.is_error);
        assert_eq!(denied.text_content(), "not for you");
        // A denied call never ran, so there is nothing for `after` to see.
        assert_eq!(*gate.0.lock().unwrap(), ["before lead secret c1"]);

        let unknown = caller.call(&ctx("lead", "c2"), "nope", json!({})).await;
        assert_eq!(unknown.text_content(), "unknown tool `nope`");

        let cancelled = ctx("lead", "c3");
        cancelled.cancel.cancel();
        let aborted = caller.call(&cancelled, "echo", json!({})).await;
        assert!(aborted.is_error);
        assert_eq!(aborted.text_content(), ABORTED_TEXT);

        struct Open;
        #[async_trait]
        impl Gate for Open {
            async fn before(&self, _: &AgentInfo, _: &dyn Tool, _: &str, _: &Value) -> GateDecision {
                GateDecision::Allow { input: None }
            }
            async fn after(&self, _: &AgentInfo, _: &dyn Tool, _: &str, _: &mut ToolOutput) {}
        }
        let open = ToolCaller::new(caller.tools.clone(), Arc::new(Open));
        let panicked = open
            .call(&ctx("lead", "c4"), "echo", json!({"panic": true}))
            .await;
        assert!(panicked.is_error);
        assert_eq!(panicked.text_content(), "tool `echo` panicked");
    }

    #[tokio::test]
    async fn a_deferral_is_visible_only_inside_deferring_and_an_answer_is_taken_once() {
        assert!(deferral().is_none());
        let outer = Deferral::new(true);
        deferring(outer.clone(), async {
            let seen = deferral().expect("inside deferring");
            assert!(Arc::ptr_eq(&seen, &outer));
            seen.decide("c1.1", "write", ApprovalDecision::AllowOnce);
            // Another tool's answer is not for this call, and is gone once asked for.
            assert_eq!(seen.decided("c1.1", "shell"), None);
            assert_eq!(seen.decided("c1.1", "write"), None);
            seen.decide("c1.1", "write", ApprovalDecision::AllowSession);
            assert_eq!(
                seen.decided("c1.1", "write"),
                Some(ApprovalDecision::AllowSession)
            );
            assert_eq!(seen.decided("c1.1", "write"), None);
        })
        .await;
        assert!(deferral().is_none());
    }

    #[test]
    fn a_blocked_deferral_stops_pausing_and_says_why_in_its_refusal() {
        let deferral = Deferral::new(true);
        assert!(deferral.pauses());
        assert!(!deferral.refusal("write").contains("cannot pause"));
        deferral.block("call 4 is too big".to_string());
        assert!(!deferral.pauses());
        assert!(
            deferral
                .refusal("write")
                .ends_with("; this run cannot pause because call 4 is too big")
        );
        assert!(!Deferral::new(false).pauses());
    }

    #[tokio::test]
    async fn a_scoped_agent_reaches_only_its_own_tools() {
        let (caller, _) = caller();
        let only_echo: Vec<Arc<dyn Tool>> = vec![Arc::new(Echo("echo"))];
        caller.scope_agent("scout-1", Arc::new(only_echo));
        let refused = caller.call(&ctx("scout-1", "c1"), "secret", json!({})).await;
        assert_eq!(refused.text_content(), "unknown tool `secret`");
        assert!(
            !caller
                .call(&ctx("scout-1", "c2"), "echo", json!({}))
                .await
                .is_error
        );
        // Another agent is not affected, and neither is this one once it is forgotten.
        assert_eq!(
            caller
                .call(&ctx("lead", "c3"), "secret", json!({}))
                .await
                .text_content(),
            "not for you"
        );
        caller.forget_agent("scout-1");
        assert_eq!(
            caller
                .call(&ctx("scout-1", "c4"), "secret", json!({}))
                .await
                .text_content(),
            "not for you"
        );
    }
}
