//! One-off requests outside the conversation: side questions (`/btw`), hand-off briefs, questions
//! to a saved session, commit messages, the advisor. Nothing is written to a session; what they
//! cost is kept per purpose in [`SideCosts`].

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, PoisonError};

use azazel_agent::tool::tool_spec;
use azazel_agent::{AgentInfo, ToolSource};
use azazel_ai::{
    AiError, Block, HostedTool, Message, ModelRef, ModelSpec, Request, Role, Router, StreamEvent, SystemPart,
    ThinkingLevel, Usage, WebSource,
};
use futures::StreamExt;
use tokio_util::sync::CancellationToken;

use crate::prompt::PromptBuilder;

/// How a side question is framed for the model when it is asked with the lead's prompt.
pub const SIDE_NOTE: &str = "<side-question>The user asks this on the side, without interrupting the work. \
Answer from what is already known in this conversation. Do not use tools; reply once.</side-question>";

/// What side requests cost, by purpose (`btw`, `handoff`, `advisor`, ...). Counted in the run's
/// spend and shown by `/cost`.
#[derive(Debug, Default)]
pub struct SideCosts(Mutex<BTreeMap<String, Usage>>);

impl SideCosts {
    pub fn add(&self, purpose: &str, usage: &Usage) {
        self.0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .entry(purpose.to_string())
            .or_default()
            .add(usage);
    }

    pub fn by_purpose(&self) -> BTreeMap<String, Usage> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner).clone()
    }

    /// USD over every purpose; models without a price add nothing.
    pub fn total_cost(&self) -> f64 {
        self.by_purpose()
            .values()
            .filter_map(|usage| usage.cost_usd)
            .sum()
    }
}

/// One side request.
pub struct SideQuestion<'a> {
    /// Whose prompt and tools frame the request (normally the lead).
    pub agent: &'a AgentInfo,
    /// The conversation the question is about; empty for a request that stands alone.
    pub context: Vec<Message>,
    pub question: String,
    pub chain: Vec<ModelSpec>,
    /// Key under which the usage is counted in [`SideCosts`].
    pub purpose: &'a str,
    /// Send the agent's system prompt and tool list unchanged, so the provider's prompt cache is
    /// reused, and wrap the question in [`SIDE_NOTE`]. Without it the request has no system prompt
    /// and no tools, and the question goes as written.
    pub framed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SideAnswer {
    pub text: String,
    pub model: ModelRef,
    pub usage: Usage,
    /// Tools the model tried to call anyway; the calls were dropped.
    pub dropped_tools: Vec<String>,
}

/// The key web searches run by a provider are counted under in [`SideCosts`].
pub const SEARCH_PURPOSE: &str = "web_search";

/// What a provider's own web search found.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchAnswer {
    /// The pages, the ones the answer cites first.
    pub sources: Vec<WebSource>,
    /// What the model wrote about them.
    pub text: String,
    pub model: ModelRef,
}

/// The models the session's lead is on now, first choice first. Registered as a service when a
/// session opens, for requests that go to the lead's model from outside its conversation.
pub struct LeadModel(pub Arc<dyn Fn() -> Vec<ModelSpec> + Send + Sync>);

/// Everything a side request needs. Registered as a service by the application.
pub struct Side {
    pub router: Arc<Router>,
    pub prompt: Arc<PromptBuilder>,
    /// The tools of the agent the request is framed for: for the lead, what it is offered now
    /// (coordinator mode withholds some), so the request matches the cached prefix.
    pub tools: Arc<dyn ToolSource>,
    pub costs: Arc<SideCosts>,
}

impl Side {
    /// Sends one request and streams its text to `on_text` as it arrives. `Err` is the failure
    /// as text; a cancelled request is `Err` with [`AiError::Aborted`]'s text.
    pub async fn ask(
        &self,
        question: SideQuestion<'_>,
        on_text: &mut (dyn FnMut(&str) + Send),
        cancel: CancellationToken,
    ) -> Result<SideAnswer, String> {
        let request = self.request(&question).await;
        let (model, message) = self
            .send(&question.chain, &request, question.purpose, on_text, cancel)
            .await?;
        Ok(SideAnswer {
            text: message.text(),
            dropped_tools: message
                .tool_calls()
                .map(|(_, name, _)| name.to_string())
                .collect(),
            model,
            usage: message.meta.map(|meta| meta.usage).unwrap_or_default(),
        })
    }

    /// The model a web search through the lead's provider runs on: the first model of `lead`,
    /// when its provider searches the web itself. Its thinking level is left off: a search is
    /// a lookup.
    pub fn search_model(&self, lead: &[ModelSpec]) -> Option<ModelRef> {
        let model = self.router.resolve(lead).into_iter().next()?;
        self.router.hosts_web_search(&model).then_some(ModelRef {
            thinking: None,
            ..model
        })
    }

    /// Has the provider of `model` search the web with its own tool: one request, with the
    /// model's credentials, whose only prompt is `instructions` and `question`. The answer
    /// carries the pages the provider found. Counted as [`SEARCH_PURPOSE`].
    pub async fn search(
        &self,
        model: &ModelRef,
        instructions: &str,
        question: String,
        tool: HostedTool,
        cancel: CancellationToken,
    ) -> Result<SearchAnswer, String> {
        let request = Request {
            system: vec![SystemPart {
                text: instructions.to_string(),
                name: String::new(),
                cache_boundary: false,
            }],
            messages: vec![Message::user(question)],
            hosted: vec![tool],
            ..Request::default()
        };
        let chain = [ModelSpec::Model(model.clone())];
        let (model, message) = self
            .send(&chain, &request, SEARCH_PURPOSE, &mut |_| {}, cancel)
            .await?;
        Ok(SearchAnswer {
            text: message.text(),
            sources: message.meta.map(|meta| meta.sources).unwrap_or_default(),
            model,
        })
    }

    /// Sends `request` down `chain` and returns the finished reply, its usage counted under
    /// `purpose`.
    async fn send(
        &self,
        chain: &[ModelSpec],
        request: &Request,
        purpose: &str,
        on_text: &mut (dyn FnMut(&str) + Send),
        cancel: CancellationToken,
    ) -> Result<(ModelRef, Message), String> {
        let (model, mut events) = self
            .router
            .stream(chain, request, cancel)
            .await
            .map_err(|e| e.to_string())?;
        while let Some(event) = events.next().await {
            match event {
                StreamEvent::TextDelta { delta, .. } => on_text(&delta),
                StreamEvent::Done { message } => {
                    let usage = message.meta.as_ref().map(|meta| meta.usage).unwrap_or_default();
                    self.costs.add(purpose, &usage);
                    return Ok((model, message));
                }
                StreamEvent::Error { error, .. } => return Err(error.to_string()),
                _ => {}
            }
        }
        Err(AiError::Network("the reply ended before it was complete".to_string()).to_string())
    }

    async fn request(&self, question: &SideQuestion<'_>) -> Request {
        let mut messages = question.context.clone();
        let (prompt, tools, text) = if question.framed {
            let mut tools = self.tools.tools(question.agent);
            // The tools the agent's own request offers its first model, so the prefix matches.
            if let Some(model) = self.router.resolve(&question.chain).first() {
                tools.retain(|tool| tool.offered_to(model));
            }
            (
                self.prompt.prompt(question.agent).await,
                tools.iter().map(|tool| tool_spec(tool.as_ref())).collect(),
                format!("{SIDE_NOTE}\n\n{}", question.question),
            )
        } else {
            (Default::default(), Vec::new(), question.question.clone())
        };
        messages.push(Message::user(text));
        Request {
            system: prompt.system,
            // What the agent is told about the moment (mode, goal, what is remembered) holds for
            // the question too.
            tail: prompt.tail,
            messages,
            tools,
            thinking: ThinkingLevel::Off,
            max_output: None,
            temperature: None,
            // Side requests run at the standard tier whatever the session's tier is.
            tier: None,
            // The lead's key, so a provider that caches by key finds the lead's prefix.
            cache_key: question
                .framed
                .then(|| format!("{}:{}", question.agent.session_id, question.agent.id)),
            hosted: Vec::new(),
        }
    }
}

/// A conversation as a side request can send it. While tools run, the newest reply still has
/// calls without results, which no provider accepts: those calls are left out, and so is what
/// the reply thought on the way to them. Any earlier message is returned as it is.
pub fn settled(mut context: Vec<Message>) -> Vec<Message> {
    let Some(last) = context.last_mut() else {
        return context;
    };
    if last.role != Role::Assistant || last.tool_calls().next().is_none() {
        return context;
    }
    last.content.retain(|block| matches!(block, Block::Text { .. }));
    if last.text().trim().is_empty() {
        context.pop();
    }
    context
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use azazel_ai::testing::{MockProvider, MockStep};
    use azazel_ai::{Catalog, ModelRef, Provider, RetryPolicy};
    use serde_json::json;

    use super::*;
    use crate::prompt::StaticPart;

    fn lead() -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s1".into(),
        }
    }

    fn side(mock: Arc<MockProvider>) -> Side {
        let router = Router::new(
            vec![mock as Arc<dyn Provider>],
            Catalog::default(),
            BTreeMap::new(),
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_jitter: Duration::ZERO,
                ..RetryPolicy::default()
            },
        );
        let prompt = Arc::new(PromptBuilder::default());
        prompt.add("t", "identity", Arc::new(StaticPart("You are the lead.".into())));
        Side {
            router: Arc::new(router),
            prompt,
            tools: Arc::new(crate::ext::ToolRegistry::default()),
            costs: Arc::new(SideCosts::default()),
        }
    }

    fn chain() -> Vec<ModelSpec> {
        vec![ModelSpec::Model(ModelRef::new("mock", "m"))]
    }

    #[tokio::test]
    async fn a_framed_question_reuses_the_lead_prompt_streams_and_counts_its_cost() {
        let mock = Arc::new(MockProvider::new());
        let mut step = MockStep::tools("It is 42.", &[("c1", "shell", json!({ "command": "ls" }))]);
        step.reply.as_mut().unwrap().usage = Usage {
            input: 10,
            output: 3,
            cost_usd: Some(0.5),
            ..Usage::default()
        };
        mock.push("m", step);
        let side = side(mock.clone());
        let mut streamed = String::new();
        let answer = side
            .ask(
                SideQuestion {
                    agent: &lead(),
                    context: vec![Message::user("earlier prompt")],
                    question: "what is the answer?".into(),
                    chain: chain(),
                    purpose: "btw",
                    framed: true,
                },
                &mut |delta| streamed.push_str(delta),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(
            (answer.text.as_str(), streamed.as_str()),
            ("It is 42.", "It is 42.")
        );
        assert_eq!(answer.dropped_tools, ["shell"]);
        assert_eq!(side.costs.by_purpose()["btw"].output, 3);
        assert_eq!(side.costs.total_cost(), 0.5);

        let request = &mock.requests_for("m")[0];
        assert_eq!(request.system[0].text, "You are the lead.");
        assert_eq!(request.cache_key.as_deref(), Some("s1:lead"));
        assert_eq!(request.messages.len(), 2);
        let last = request.messages.last().unwrap();
        assert_eq!(last.role, Role::User);
        assert!(last.text().starts_with(SIDE_NOTE) && last.text().ends_with("what is the answer?"));
    }

    #[tokio::test]
    async fn a_plain_question_goes_alone_and_failures_are_text() {
        let mock = Arc::new(MockProvider::new());
        mock.push("m", MockStep::text("fix: handle empty input"));
        mock.push("m", MockStep::fail(AiError::Overloaded));
        let side = side(mock.clone());
        let agent = lead();
        let ask = |question: &str| SideQuestion {
            agent: &agent,
            context: Vec::new(),
            question: question.into(),
            chain: chain(),
            purpose: "commit",
            framed: false,
        };
        let answer = side
            .ask(
                ask("write a commit message"),
                &mut |_| {},
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(answer.text, "fix: handle empty input");
        let request = &mock.requests_for("m")[0];
        assert!(request.system.is_empty() && request.tools.is_empty() && request.cache_key.is_none());
        assert_eq!(request.messages[0].text(), "write a commit message");

        let failed = side
            .ask(ask("again"), &mut |_| {}, CancellationToken::new())
            .await;
        assert!(failed.unwrap_err().contains("overloaded"));
    }

    #[tokio::test]
    async fn a_search_is_one_request_to_the_model_with_the_providers_tool_and_returns_its_pages() {
        let mock = Arc::new(MockProvider::new());
        let side = side(mock.clone());
        // A provider without a search tool has no model to search on.
        assert_eq!(side.search_model(&chain()), None);

        let found = WebSource {
            url: "https://tokio.rs/blog".into(),
            title: "Tokio blog".into(),
            snippet: "Tokio 1.48 is out.".into(),
        };
        let usage = Usage {
            output: 7,
            ..Usage::default()
        };
        mock.push(
            azazel_ai::testing::MOCK_SEARCH_QUEUE,
            MockStep::search("Tokio 1.48 is out.", vec![found.clone()]).with_usage(usage),
        );
        // The lead's first model, without the thinking level it runs at.
        let lead: Vec<ModelSpec> = vec!["mock/m:high".parse().unwrap(), "mock/other".parse().unwrap()];
        let model = side.search_model(&lead).unwrap();
        assert_eq!(model, ModelRef::new("mock", "m"));

        let tool = HostedTool::WebSearch {
            domains: vec!["tokio.rs".into()],
            max_uses: Some(2),
        };
        let answer = side
            .search(
                &model,
                "Search and cite.",
                "Query: tokio".into(),
                tool.clone(),
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(answer.sources, [found]);
        assert_eq!(
            (answer.text.as_str(), &answer.model),
            ("Tokio 1.48 is out.", &model)
        );
        assert_eq!(side.costs.by_purpose()[SEARCH_PURPOSE].output, 7);

        let (asked, request) = &mock.requests()[0];
        assert_eq!(asked, "m");
        assert_eq!(request.hosted, [tool]);
        assert_eq!(request.system[0].text, "Search and cite.");
        assert_eq!(request.messages.len(), 1);
        assert_eq!(request.messages[0].text(), "Query: tokio");
        assert!(request.tools.is_empty() && request.cache_key.is_none());
        assert_eq!(request.thinking, ThinkingLevel::Off);
    }

    #[test]
    fn a_reply_whose_tools_still_run_is_sent_without_its_calls() {
        let call = |id: &str| Block::ToolCall {
            id: id.into(),
            name: "shell".into(),
            input: json!({}),
            signature: None,
        };
        let reply = |content: Vec<Block>| Message {
            role: Role::Assistant,
            ..Message::user_blocks(content)
        };
        let done = vec![
            Message::user("go"),
            reply(vec![call("c1")]),
            Message::tool(vec![Block::tool_result("c1", "ok", false)]),
        ];
        assert_eq!(settled(done.clone()), done);

        let mut running = done.clone();
        running.push(reply(vec![Block::text("Running the tests."), call("c2")]));
        let sent = settled(running);
        assert_eq!(sent.len(), 4);
        assert_eq!(sent[3].content, vec![Block::text("Running the tests.")]);

        // Nothing but calls: the whole reply is left out.
        let mut silent = done.clone();
        silent.push(reply(vec![call("c2")]));
        assert_eq!(settled(silent), done);
        assert_eq!(settled(Vec::new()), Vec::new());
    }
}
