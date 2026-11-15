//! The advisor: a second model that looks over the lead's work when the lead is about to stop,
//! and on demand (`/advise`). What it says is advice for one more round, never a verdict.

use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{AgentHandle, AgentInfo, Services};
use azazel_ai::{ModelSpec, Role};
use azazel_core::checkpoint::Checkpoints;
use azazel_core::side::{Side, SideQuestion};
use azazel_core::{
    Command, CommandHandler, CommandOutcome, Entry, EntryKind, Event, EventHandler, EventKind, ExtCtx,
    ExtError, Extension, Level, Outcome, Registrar, SessionStore, Ui,
};
use tokio_util::sync::CancellationToken;

/// The name the advisor's requests are counted under in the run's cost.
pub const PURPOSE: &str = "advisor";
/// Bytes of the diff the advisor is shown.
const DIFF_LIMIT: usize = 30_000;
/// Characters of one prompt, and of the lead's last message, the advisor is shown.
const TEXT_LIMIT: usize = 6_000;
/// Messages from the user's side of a run the advisor is shown: the first and the latest.
const MAX_PROMPTS: usize = 8;
/// How long the lead waits for the advisor before it stops without advice.
const TIMEOUT: Duration = Duration::from_secs(120);

const REVIEW: &str = "You are a second model reviewing another coding agent's work just before it reports to \
the user. Below is what the user asked, the agent's final message and the changes it made to the project. \
Look for what the agent missed: a request left undone, a claim the changes do not support, a bug in the \
diff, a check it should have run.\n\n\
Reply with exactly `OK` when the work needs nothing more. Otherwise reply with your advice only: short, \
concrete points, each naming the file or the claim it is about. Do not restate the work and do not praise it.";

const ON_DEMAND: &str = "You are a second model looking over another coding agent's work for the user. Below \
is what the user asked, the agent's latest message and the changes it made to the project so far. Answer the \
user directly and concretely, naming the files and claims you mean.";

/// `[advisor]` as the session uses it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AdvisorSettings {
    /// The second model; empty means there is no advisor.
    pub chain: Vec<ModelSpec>,
    /// Review the lead's work each time it is about to stop.
    pub on_stop: bool,
}

struct Attached {
    lead: AgentInfo,
    session: Arc<SessionStore>,
    checkpoints: Arc<Checkpoints>,
}

/// What the advisor is shown of a run.
#[derive(Debug, Clone, PartialEq)]
struct Work {
    prompts: Vec<String>,
    last_message: String,
    /// The checkpoint taken when the run's prompt was sent.
    base: Option<String>,
}

fn cut(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let kept: String = text.chars().take(limit).collect();
    format!("{kept}\n[cut here]")
}

/// Whether the advisor's answer says the work needs nothing more.
fn approves(answer: &str) -> bool {
    answer
        .trim()
        .trim_matches(|c: char| !c.is_alphanumeric())
        .eq_ignore_ascii_case("ok")
}

/// The run that starts at entry `start` of `branch` (default: at the last message from the
/// user's side), as the advisor sees it. `None` when the lead has said nothing in it yet.
fn work(branch: &[Entry], start: Option<usize>) -> Option<Work> {
    let said = |entry: &Entry, role: Role| match &entry.kind {
        EntryKind::Message { message } if message.role == role => {
            Some(message.text()).filter(|text| !text.trim().is_empty())
        }
        _ => None,
    };
    let start = start
        .filter(|start| *start <= branch.len())
        .or_else(|| branch.iter().rposition(|entry| said(entry, Role::User).is_some()))?;
    let run = &branch[start..];
    let last_message = run.iter().rev().find_map(|entry| said(entry, Role::Assistant))?;
    let mut prompts: Vec<String> = run
        .iter()
        .filter_map(|entry| said(entry, Role::User))
        .map(|text| cut(&text, TEXT_LIMIT))
        .collect();
    if prompts.len() > MAX_PROMPTS {
        // The request itself, then what was said most recently.
        prompts.drain(1..prompts.len() - (MAX_PROMPTS - 1));
    }
    let checkpoint = |entry: &Entry| match &entry.kind {
        EntryKind::Checkpoint { commit, .. } => Some(commit.clone()),
        _ => None,
    };
    // A prompt's checkpoint is recorded just before its message; a run that was not started by
    // a prompt is measured from the last one before it.
    let base = run
        .iter()
        .find_map(checkpoint)
        .or_else(|| branch[..start].iter().rev().find_map(checkpoint));
    Some(Work {
        prompts,
        last_message: cut(&last_message, TEXT_LIMIT),
        base,
    })
}

/// The request text: instructions, then the run.
fn brief(instructions: &str, work: &Work, diff: &str, question: Option<&str>) -> String {
    let mut text = format!("{instructions}\n\n<prompts>\n");
    for prompt in &work.prompts {
        text.push_str(&format!("<prompt>\n{prompt}\n</prompt>\n"));
    }
    text.push_str(&format!(
        "</prompts>\n\n<final-message>\n{}\n</final-message>\n\n<changes>\n{}\n</changes>",
        work.last_message,
        if diff.trim().is_empty() {
            "(no change to the project's files is recorded for this run)"
        } else {
            diff
        }
    ));
    if let Some(question) = question {
        text.push_str(&format!("\n\nThe user asks:\n{question}"));
    }
    text
}

/// The second model of a session. Registered as a service.
pub struct Advisor {
    settings: AdvisorSettings,
    attached: RwLock<Option<Attached>>,
    /// How long the session's branch was when the run's prompt was sent.
    run_start: Mutex<Option<usize>>,
}

impl Advisor {
    pub fn new(settings: AdvisorSettings) -> Advisor {
        Advisor {
            settings,
            attached: RwLock::default(),
            run_start: Mutex::default(),
        }
    }

    /// Whether a model is set for the advisor.
    pub fn configured(&self) -> bool {
        !self.settings.chain.is_empty()
    }

    /// Points the advisor at the open session: its lead, its transcript and its checkpoints.
    pub fn attach(&self, lead: &AgentHandle, session: Arc<SessionStore>, checkpoints: Arc<Checkpoints>) {
        *self.attached.write().unwrap_or_else(PoisonError::into_inner) = Some(Attached {
            lead: lead.info().clone(),
            session,
            checkpoints,
        });
        *self.run_start.lock().unwrap_or_else(PoisonError::into_inner) = None;
    }

    fn prompt_sent(&self) {
        let attached = self.attached.read().unwrap_or_else(PoisonError::into_inner);
        if let Some(attached) = attached.as_ref() {
            *self.run_start.lock().unwrap_or_else(PoisonError::into_inner) =
                Some(attached.session.branch(None).len());
        }
    }

    /// Asks the second model about the current run: a review (`question` is `None`) or an
    /// answer to `question`. `Ok(None)` when there is nothing to look at yet.
    pub async fn ask(&self, services: &Services, question: Option<&str>) -> Result<Option<String>, String> {
        if !self.configured() {
            return Err(
                "no advisor model is set: add `model = [\"provider/model\"]` under [advisor]".to_string(),
            );
        }
        let side = services
            .get::<Side>()
            .ok_or("side requests are not available in this session")?;
        let (lead, work, checkpoints) = {
            let attached = self.attached.read().unwrap_or_else(PoisonError::into_inner);
            let attached = attached.as_ref().ok_or("no session is open")?;
            let start = *self.run_start.lock().unwrap_or_else(PoisonError::into_inner);
            let Some(work) = work(&attached.session.branch(None), start) else {
                return Ok(None);
            };
            (attached.lead.clone(), work, attached.checkpoints.clone())
        };
        let diff = match &work.base {
            Some(base) => match checkpoints.snapshot("advisor").await {
                Ok(Some(now)) => checkpoints.diff(base, &now, DIFF_LIMIT).await,
                _ => String::new(),
            },
            None => String::new(),
        };
        let instructions = if question.is_some() { ON_DEMAND } else { REVIEW };
        let cancel = CancellationToken::new();
        let mut unheard = |_: &str| {};
        let asked = side.ask(
            SideQuestion {
                agent: &lead,
                context: Vec::new(),
                question: brief(instructions, &work, &diff, question),
                chain: self.settings.chain.clone(),
                purpose: PURPOSE,
                framed: false,
            },
            &mut unheard,
            cancel.clone(),
        );
        let answer = match tokio::time::timeout(TIMEOUT, asked).await {
            Ok(answer) => answer?,
            Err(_) => {
                cancel.cancel();
                return Err(format!("no answer within {} s", TIMEOUT.as_secs()));
            }
        };
        let text = answer.text.trim();
        if text.is_empty() {
            return Err("the answer was empty".to_string());
        }
        Ok(Some(text.to_string()))
    }
}

/// Reviews the lead's work when it is about to stop, once per run.
struct StopReview(Arc<Advisor>);

#[async_trait]
impl EventHandler for StopReview {
    async fn handle(&self, event: &mut Event, ctx: &ExtCtx) -> Outcome {
        match event {
            Event::UserPrompt { .. } => self.0.prompt_sent(),
            // A stop that already led to another round is not reviewed again, whoever asked
            // for that round.
            Event::Stop {
                agent,
                stop_hook_active: false,
            } if agent.depth == 0 && self.0.settings.on_stop && self.0.configured() => {
                let notify = |level: Level, text: &str| {
                    if let Some(ui) = ctx.services.get::<Ui>() {
                        ui.0.notify(level, text);
                    }
                };
                match self.0.ask(&ctx.services, None).await {
                    Ok(Some(answer)) if !approves(&answer) => {
                        notify(
                            Level::Info,
                            &format!("the advisor has advice, and the lead takes one more round:\n{answer}"),
                        );
                        return Outcome::Block {
                            reason: format!(
                                "<advisor>\n{answer}\n</advisor>\nA second model reviewed your work and gave the \
                                 advice above. Act on what is right in it, say briefly where you disagree, then \
                                 finish."
                            ),
                        };
                    }
                    Ok(_) => {}
                    Err(why) => notify(Level::Warn, &format!("the advisor was not heard: {why}")),
                }
            }
            _ => {}
        }
        Outcome::Continue
    }
}

struct AdviseCommand(Arc<Advisor>);

#[async_trait]
impl CommandHandler for AdviseCommand {
    async fn run(&self, args: &str, ctx: &ExtCtx) -> CommandOutcome {
        let question = match args.trim() {
            "" => "What do you make of the work so far? Say what is missing, wrong or unverified.",
            question => question,
        };
        match self.0.ask(&ctx.services, Some(question)).await {
            Ok(Some(answer)) => CommandOutcome::Show(format!("advisor:\n{answer}")),
            Ok(None) => CommandOutcome::Show("there is nothing for the advisor to look at yet".to_string()),
            Err(why) => CommandOutcome::Error(format!("the advisor did not answer: {why}")),
        }
    }
}

/// Registers the [`Advisor`] service, its review of the lead's stops and `/advise`. Load it
/// before the goal extension: a goal's own check then still has the last word on a stop.
pub struct AdvisorExt {
    pub advisor: Arc<Advisor>,
}

impl Extension for AdvisorExt {
    fn id(&self) -> &str {
        "advisor"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        r.service(self.advisor.clone());
        let review = Arc::new(StopReview(self.advisor.clone()));
        r.on(EventKind::UserPrompt, review.clone());
        r.on(EventKind::Stop, review);
        r.command(Command {
            name: "advise".to_string(),
            description: "Ask the advisor, a second model, about the work so far; only you see the answer"
                .to_string(),
            argument_hint: Some("[question]".to_string()),
            handler: Arc::new(AdviseCommand(self.advisor.clone())),
        });
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use azazel_ai::{Block, Message};

    use super::*;

    fn said(text: impl Into<String>) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Block::text(text)],
            meta: None,
        }
    }

    fn entry(n: usize, kind: EntryKind) -> Entry {
        Entry {
            id: n.to_string(),
            parent_id: None,
            timestamp: String::new(),
            kind,
        }
    }

    fn message(n: usize, message: Message) -> Entry {
        entry(n, EntryKind::Message { message })
    }

    fn checkpoint(n: usize, commit: &str) -> Entry {
        entry(
            n,
            EntryKind::Checkpoint {
                turn_entry_id: String::new(),
                commit: commit.to_string(),
            },
        )
    }

    fn branch() -> Vec<Entry> {
        vec![
            checkpoint(0, "c-old"),
            message(1, Message::user("fix the parser")),
            message(2, said("fixed")),
            checkpoint(3, "c-new"),
            message(4, Message::user("now add a test")),
            message(5, said("")),
            message(6, Message::user("and run it")),
            message(7, said("added and ran it")),
        ]
    }

    #[test]
    fn the_advisor_sees_the_runs_prompts_the_last_message_and_the_prompts_checkpoint() {
        let branch = branch();
        // The prompt was sent when the branch had three entries.
        assert_eq!(
            work(&branch, Some(3)),
            Some(Work {
                prompts: vec!["now add a test".to_string(), "and run it".to_string()],
                last_message: "added and ran it".to_string(),
                base: Some("c-new".to_string()),
            })
        );
        // With no record of where the run began, it is taken from the last message of the user,
        // measured from the checkpoint before it.
        assert_eq!(
            work(&branch, None),
            Some(Work {
                prompts: vec!["and run it".to_string()],
                last_message: "added and ran it".to_string(),
                base: Some("c-new".to_string()),
            })
        );
        // Nothing to review before the lead has spoken, or in a session without prompts.
        assert_eq!(work(&branch[..5], Some(3)), None);
        assert_eq!(work(&[], None), None);
        // A start beyond the branch (the leaf was moved) falls back the same way.
        assert!(work(&branch[..3], Some(7)).is_some_and(|work| work.base.as_deref() == Some("c-old")));
    }

    #[test]
    fn long_runs_keep_the_request_and_the_latest_messages() {
        let mut branch = vec![message(0, Message::user("the request"))];
        for n in 1..=12 {
            branch.push(message(n, Message::user(format!("note {n}"))));
        }
        branch.push(message(13, said("x".repeat(TEXT_LIMIT + 5))));
        let work = work(&branch, Some(0)).unwrap();
        assert_eq!(work.prompts.len(), MAX_PROMPTS);
        assert_eq!(work.prompts[0], "the request");
        assert_eq!(work.prompts[1], "note 6");
        assert_eq!(work.prompts[7], "note 12");
        assert!(work.last_message.ends_with("\n[cut here]"));
        assert_eq!(work.base, None);
    }

    #[test]
    fn only_a_bare_ok_approves() {
        for yes in ["OK", "ok", " `OK`\n", "OK.", "**OK**"] {
            assert!(approves(yes), "{yes}");
        }
        for no in [
            "",
            "OK, but run the tests",
            "The tests were not run.",
            "okay then",
        ] {
            assert!(!approves(no), "{no}");
        }
    }
}
