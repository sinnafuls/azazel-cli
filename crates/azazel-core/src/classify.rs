//! Quick judgements: one small request that picks one of several answers and says how sure it
//! is. Asked of the `judge` role unless the caller names a model; nothing is written to a
//! session, and what it costs is counted under [`PURPOSE`].

use azazel_agent::AgentInfo;
use azazel_ai::{ModelRef, ModelSpec};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::side::{Side, SideQuestion};

/// The role that answers when no model is named. It falls back to `smol`.
pub const JUDGE_ROLE: &str = "judge";
/// The key judgements are counted under in [`crate::side::SideCosts`].
pub const PURPOSE: &str = "classify";
/// A question has from two to this many choices.
pub const MAX_CHOICES: usize = 20;
/// How much of a reply that cannot be read is quoted back.
const QUOTED_CHARS: usize = 200;

/// The answer to one question.
#[derive(Debug, Clone, PartialEq)]
pub struct Classification {
    /// The choice picked, as it was given.
    pub choice: String,
    /// Its place among the choices, from 0.
    pub index: usize,
    /// How sure the model is, from 0 to 1.
    pub confidence: f64,
    /// Why, in a sentence; empty when the model gave none.
    pub reason: String,
    pub model: ModelRef,
}

fn request_text(question: &str, choices: &[String]) -> String {
    let mut text = format!(
        "Answer the question by picking exactly one of the numbered choices.\n\n<question>\n{}\n</question>\n\n\
         Choices:\n",
        question.trim()
    );
    for (number, choice) in choices.iter().enumerate() {
        text.push_str(&format!("{}. {}\n", number + 1, choice.trim()));
    }
    text.push_str(
        "\nReply with one JSON object and nothing else:\n{\"choice\": <the number of the choice>, \
         \"confidence\": <how sure you are, from 0 to 1>, \"reason\": \"<one short sentence>\"}",
    );
    text
}

fn number(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().trim_end_matches('%').parse().ok(),
        _ => None,
    }
}

/// `(index, confidence, reason)` from a reply: the JSON object in it, whatever surrounds it.
/// The choice may be given by number or by its text; a confidence above 1 is read as a
/// percentage.
fn parse(reply: &str, choices: &[String]) -> Result<(usize, f64, String), String> {
    let unreadable = |what: &str| {
        let quoted: String = reply.trim().chars().take(QUOTED_CHARS).collect();
        format!("the model did not answer in the form asked for ({what}): {quoted}")
    };
    let object = reply
        .find('{')
        .zip(reply.rfind('}'))
        .filter(|(start, end)| start < end)
        .and_then(|(start, end)| serde_json::from_str::<Value>(&reply[start..=end]).ok())
        .ok_or_else(|| unreadable("no JSON object"))?;
    let picked = object.get("choice").ok_or_else(|| unreadable("no `choice`"))?;
    let by_text = picked.as_str().and_then(|text| {
        choices
            .iter()
            .position(|choice| choice.trim().eq_ignore_ascii_case(text.trim()))
    });
    let by_number = || {
        let n = number(picked)?;
        (n.fract() == 0.0 && n >= 1.0 && n <= choices.len() as f64).then(|| n as usize - 1)
    };
    let index = by_text
        .or_else(by_number)
        .ok_or_else(|| unreadable("`choice` is none of the choices"))?;
    let confidence = object
        .get("confidence")
        .and_then(number)
        .filter(|confidence| confidence.is_finite() && *confidence >= 0.0)
        .ok_or_else(|| unreadable("no `confidence`"))?;
    let confidence = if confidence > 1.0 {
        confidence / 100.0
    } else {
        confidence
    };
    let reason = object
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .to_string();
    Ok((index, confidence.min(1.0), reason))
}

/// Has a model pick the one of `choices` that answers `question`. `chain` is the model to ask;
/// `None` asks the `judge` role. `agent` is whose request it is. `Err` says what was wrong with
/// the question, the model, or its reply.
pub async fn classify(
    side: &Side,
    agent: &AgentInfo,
    question: &str,
    choices: &[String],
    chain: Option<Vec<ModelSpec>>,
    cancel: CancellationToken,
) -> Result<Classification, String> {
    if question.trim().is_empty() {
        return Err("the question is empty".to_string());
    }
    if choices.len() < 2 || choices.len() > MAX_CHOICES {
        return Err(format!(
            "there are {} choices; give from 2 to {MAX_CHOICES}",
            choices.len()
        ));
    }
    for (at, choice) in choices.iter().enumerate() {
        if choice.trim().is_empty() {
            return Err(format!("choice {} is empty", at + 1));
        }
        if choices[..at]
            .iter()
            .any(|earlier| earlier.trim().eq_ignore_ascii_case(choice.trim()))
        {
            return Err(format!("the choice `{}` is there twice", choice.trim()));
        }
    }
    let chain = match chain {
        Some(chain) if side.router.resolve(&chain).is_empty() => {
            let named: Vec<String> = chain.iter().map(ToString::to_string).collect();
            return Err(format!(
                "`{}` names no model that is configured",
                named.join(", ")
            ));
        }
        Some(chain) => chain,
        None => {
            let chain = vec![ModelSpec::Role(JUDGE_ROLE.to_string())];
            if side.router.resolve(&chain).is_empty() {
                return Err(format!(
                    "no model is configured for the `{JUDGE_ROLE}` role; set `[roles] {JUDGE_ROLE}` or name a model"
                ));
            }
            chain
        }
    };
    let asked = SideQuestion {
        agent,
        context: Vec::new(),
        question: request_text(question, choices),
        chain,
        purpose: PURPOSE,
        framed: false,
    };
    let answer = side.ask(asked, &mut |_| {}, cancel).await?;
    let (index, confidence, reason) = parse(&answer.text, choices)?;
    Ok(Classification {
        choice: choices[index].clone(),
        index,
        confidence,
        reason,
        model: answer.model,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::sync::Arc;
    use std::time::Duration;

    use azazel_ai::testing::{MockProvider, MockStep};
    use azazel_ai::{Catalog, Provider, RetryPolicy, Router, Usage};

    use super::*;
    use crate::prompt::PromptBuilder;
    use crate::side::SideCosts;

    fn lead() -> AgentInfo {
        AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s1".into(),
        }
    }

    fn side(mock: Arc<MockProvider>, roles: &[(&str, &str)]) -> Side {
        let roles: BTreeMap<String, Vec<ModelSpec>> = roles
            .iter()
            .map(|(role, spec)| (role.to_string(), vec![spec.parse().unwrap()]))
            .collect();
        let router = Router::new(
            vec![mock as Arc<dyn Provider>],
            Catalog::default(),
            roles,
            RetryPolicy {
                max_attempts: 1,
                base_delay: Duration::ZERO,
                max_jitter: Duration::ZERO,
                ..RetryPolicy::default()
            },
        );
        Side {
            router: Arc::new(router),
            prompt: Arc::new(PromptBuilder::default()),
            tools: Arc::new(crate::ext::ToolRegistry::default()),
            costs: Arc::new(SideCosts::default()),
        }
    }

    fn choices(texts: &[&str]) -> Vec<String> {
        texts.iter().map(ToString::to_string).collect()
    }

    #[test]
    fn a_reply_is_read_by_number_or_by_text_whatever_surrounds_the_object() {
        let options = choices(&["bug", "feature request", "question"]);
        assert_eq!(
            parse(
                r#"{"choice": 2, "confidence": 0.8, "reason": "asks for something new"}"#,
                &options
            ),
            Ok((1, 0.8, "asks for something new".to_string()))
        );
        // A fenced reply, the choice by its text, the confidence as a percentage.
        assert_eq!(
            parse(
                "Here you go:\n```json\n{\"choice\": \"Question\", \"confidence\": \"90%\"}\n```",
                &options
            ),
            Ok((2, 0.9, String::new()))
        );
        assert_eq!(
            parse(r#"{"choice": "1", "confidence": 1}"#, &options),
            Ok((0, 1.0, String::new()))
        );

        for (reply, what) in [
            ("It is a bug.", "no JSON object"),
            (r#"{"confidence": 0.5}"#, "no `choice`"),
            (
                r#"{"choice": 4, "confidence": 0.5}"#,
                "`choice` is none of the choices",
            ),
            (
                r#"{"choice": "praise", "confidence": 0.5}"#,
                "`choice` is none of the choices",
            ),
            (r#"{"choice": 1}"#, "no `confidence`"),
            (r#"{"choice": 1, "confidence": -1}"#, "no `confidence`"),
        ] {
            assert_eq!(
                parse(reply, &options),
                Err(format!(
                    "the model did not answer in the form asked for ({what}): {reply}"
                ))
            );
        }
    }

    #[tokio::test]
    async fn the_judge_role_answers_one_request_without_prompt_or_tools_and_its_cost_is_counted() {
        let mock = Arc::new(MockProvider::new());
        mock.push(
            "judge",
            MockStep::text(r#"{"choice": 1, "confidence": 0.75, "reason": "it crashes"}"#).with_usage(
                Usage {
                    input: 40,
                    output: 12,
                    cost_usd: Some(0.01),
                    ..Usage::default()
                },
            ),
        );
        let side = side(mock.clone(), &[("judge", "mock/judge")]);
        let answer = classify(
            &side,
            &lead(),
            "What kind of report is this?\nThe app crashes on start.",
            &choices(&["bug", "question"]),
            None,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            answer,
            Classification {
                choice: "bug".to_string(),
                index: 0,
                confidence: 0.75,
                reason: "it crashes".to_string(),
                model: ModelRef::new("mock", "judge"),
            }
        );
        let requests = mock.requests_for("judge");
        assert_eq!(requests.len(), 1);
        assert!(requests[0].system.is_empty() && requests[0].tools.is_empty());
        let asked = requests[0].messages[0].text();
        assert!(
            asked.contains("The app crashes on start.\n</question>\n\nChoices:\n1. bug\n2. question\n"),
            "{asked}"
        );
        assert_eq!(side.costs.by_purpose()[PURPOSE].cost_usd, Some(0.01));

        // A model named by the caller is asked instead of the role.
        mock.push(
            "other",
            MockStep::text(r#"{"choice": "question", "confidence": 0.5}"#),
        );
        let answer = classify(
            &side,
            &lead(),
            "Which?",
            &choices(&["bug", "question"]),
            Some(vec!["mock/other".parse().unwrap()]),
            CancellationToken::new(),
        )
        .await
        .unwrap();
        assert_eq!(
            (answer.index, answer.model.to_string()),
            (1, "mock/other".to_string())
        );
    }

    #[tokio::test]
    async fn a_question_that_cannot_be_asked_is_refused_before_any_request() {
        let mock = Arc::new(MockProvider::new());
        let with_judge = side(mock.clone(), &[("judge", "mock/judge")]);
        let ask = async |side: &Side, question: &str, options: &[&str], chain: Option<&str>| {
            classify(
                side,
                &lead(),
                question,
                &choices(options),
                chain.map(|spec| vec![spec.parse().unwrap()]),
                CancellationToken::new(),
            )
            .await
            .unwrap_err()
        };
        assert_eq!(
            ask(&with_judge, " ", &["a", "b"], None).await,
            "the question is empty"
        );
        assert_eq!(
            ask(&with_judge, "q", &["a"], None).await,
            "there are 1 choices; give from 2 to 20"
        );
        assert_eq!(
            ask(&with_judge, "q", &["a", " "], None).await,
            "choice 2 is empty"
        );
        assert_eq!(
            ask(&with_judge, "q", &["Yes", "no", "yes "], None).await,
            "the choice `yes` is there twice"
        );
        assert_eq!(
            ask(&with_judge, "q", &["a", "b"], Some("@nobody")).await,
            "`@nobody` names no model that is configured"
        );
        let without = side(mock.clone(), &[]);
        assert_eq!(
            ask(&without, "q", &["a", "b"], None).await,
            "no model is configured for the `judge` role; set `[roles] judge` or name a model"
        );
        assert!(mock.requests().is_empty());
    }
}
