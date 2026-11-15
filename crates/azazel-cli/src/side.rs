//! Side questions (`/btw`): a question about the open session that is answered outside it. The
//! lead keeps working, nothing is written to the session, and the cost is counted as `btw`. The
//! request itself is [`azazel_core::side::Btw`], which agents asking each other use too.

#[cfg(feature = "test-support")]
use std::sync::Arc;

#[cfg(feature = "test-support")]
use azazel_agent::AgentHandle;
use azazel_agent::Services;
#[cfg(feature = "test-support")]
use azazel_core::SessionStore;
use azazel_core::side::Side;
pub use azazel_core::side::{Btw, Exchange};
use azazel_tui::components::format_tokens;
#[cfg(feature = "test-support")]
use tokio_util::sync::CancellationToken;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dropped_tools_are_named_once_each() {
        assert_eq!(dropped_note(&[]), None);
        let tools = ["shell".to_string(), "read".to_string(), "shell".to_string()];
        assert_eq!(
            dropped_note(&tools).as_deref(),
            Some("the model tried to use shell, read; ask in the main conversation")
        );
    }
}
