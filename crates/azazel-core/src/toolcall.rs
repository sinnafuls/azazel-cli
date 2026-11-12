//! Calling a tool on an agent's behalf from outside the agent loop (a script run by `run_code`, a
//! Luau extension), through the same gate a call the model makes itself passes.

use std::collections::HashMap;
use std::sync::{Arc, PoisonError, RwLock};
use std::time::Duration;

use azazel_agent::agent::ABORTED_TEXT;
use azazel_agent::{Gate, GateDecision, Tool, ToolCtx, ToolOutput, ToolSource};
use serde_json::Value;

use crate::ext::ToolRegistry;

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
