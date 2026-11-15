//! Side questions (`/btw`): a question about the open session that is answered outside it. The
//! lead keeps working, nothing is written to the session, and the cost is counted as `btw`.

use std::sync::Arc;

use azazel_agent::{AgentHandle, AgentInfo, Services};
use azazel_ai::{Block, Message, ModelSpec, Role};
use azazel_core::SessionStore;
use azazel_core::side::{SIDE_NOTE, Side, SideAnswer, SideQuestion, settled};
use azazel_tui::components::format_tokens;
use tokio_util::sync::CancellationToken;

/// The key side questions are counted under in the session's cost.
const PURPOSE: &str = "btw";

/// A side question and what came back.
#[derive(Debug, Clone, PartialEq)]
pub struct Exchange {
    pub question: String,
    pub answer: String,
}

/// A side question ready to be sent: the session as it is now, the exchanges that went before on
/// the side, and the model the lead is on.
pub struct Btw {
    side: Arc<Side>,
    agent: AgentInfo,
    context: Vec<Message>,
    chain: Vec<ModelSpec>,
    question: String,
}

impl Btw {
    /// Takes the session's context now, so the answer is about the work up to this moment.
    /// `earlier` is what was already asked on the side; it goes into this request only.
    pub fn prepare(
        services: &Services,
        session: &SessionStore,
        lead: &AgentHandle,
        earlier: &[Exchange],
        question: &str,
    ) -> Result<Btw, String> {
        let side = services
            .get::<Side>()
            .ok_or("side questions are not available in this session")?;
        let mut context = settled(
            session
                .build_context(None)
                .into_iter()
                .map(|entry| entry.message)
                .collect(),
        );
        for exchange in earlier {
            context.push(Message::user(format!("{SIDE_NOTE}\n\n{}", exchange.question)));
            context.push(Message {
                role: Role::Assistant,
                content: vec![Block::text(exchange.answer.as_str())],
                meta: None,
            });
        }
        Ok(Btw {
            side,
            agent: lead.info().clone(),
            context,
            chain: lead.status().chain,
            question: question.to_string(),
        })
    }

    /// Sends the question; the answer's text goes to `on_text` as it arrives.
    pub async fn ask(
        self,
        on_text: &mut (dyn FnMut(&str) + Send),
        cancel: CancellationToken,
    ) -> Result<SideAnswer, String> {
        let question = SideQuestion {
            agent: &self.agent,
            context: self.context,
            question: self.question,
            chain: self.chain,
            purpose: PURPOSE,
            framed: true,
        };
        self.side.ask(question, on_text, cancel).await
    }
}

/// What to say when the model answered a side question with tool calls, which were dropped.
pub fn dropped_note(tools: &[String]) -> Option<String> {
    if tools.is_empty() {
        return None;
    }
    let mut names: Vec<&str> = Vec::new();
    for tool in tools {
        if !names.contains(&tool.as_str()) {
            names.push(tool);
        }
    }
    Some(format!(
        "the model tried to use {}; ask in the main conversation",
        names.join(", ")
    ))
}

/// What requests on the side have cost, one line per purpose, for `/cost`. `None` when there
/// were none.
pub fn cost_report(services: &Services) -> Option<String> {
    let costs = services.get::<Side>()?.costs.by_purpose();
    if costs.is_empty() {
        return None;
    }
    let mut out = String::from("on the side:");
    for (purpose, usage) in costs {
        let cost = match usage.cost_usd {
            Some(cost) => format!("${cost:.4}"),
            None => "no price known".to_string(),
        };
        out.push_str(&format!(
            "\n  {purpose}: {} in · {} out · {cost}",
            format_tokens(usage.input + usage.cache_read + usage.cache_write),
            format_tokens(usage.output)
        ));
    }
    Some(out)
}

/// For tests, which have no terminal to type `/btw` into: `AZAZEL_TEST_BTW` is asked on the side
/// a moment after the prompt went out, and its answer is printed as one JSON line of its own.
#[cfg(feature = "test-support")]
pub fn ask_from_env(
    services: Arc<Services>,
    session: Arc<SessionStore>,
    lead: AgentHandle,
) -> Option<tokio::task::JoinHandle<()>> {
    let question = std::env::var("AZAZEL_TEST_BTW").ok()?;
    Some(tokio::spawn(async move {
        // Long enough for the lead's request to be on its way.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let mut streamed = String::new();
        let asked = match Btw::prepare(&services, &session, &lead, &[], &question) {
            Ok(btw) => {
                btw.ask(&mut |delta| streamed.push_str(delta), CancellationToken::new())
                    .await
            }
            Err(why) => Err(why),
        };
        let line = match asked {
            Ok(answer) => serde_json::json!({
                "type": "btw",
                "answer": answer.text,
                "streamed": streamed,
                "note": dropped_note(&answer.dropped_tools),
            }),
            Err(why) => serde_json::json!({ "type": "btw", "error": why }),
        };
        println!("{line}");
    }))
}
