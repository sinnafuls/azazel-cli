//! Session names: after the lead's first reply in a session that has none, a small model is asked
//! for a short title. A name given by hand (`--name`, `/name`) is never replaced.

use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::AgentInfo;
use azazel_ai::{ModelSpec, Role};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::ext::{Event, EventHandler, EventKind, ExtCtx, ExtError, Extension, Outcome, Registrar};
use crate::session::{EntryKind, SessionStore};
use crate::side::{Side, SideQuestion};

/// How long the namer may take.
const NAMING_LIMIT: Duration = Duration::from_secs(10);
/// Characters of the first prompt and reply shown to the namer.
const EXCERPT: usize = 2000;
const MAX_WORDS: usize = 6;
const MAX_CHARS: usize = 60;

/// The role that names sessions.
pub const NAMER_ROLE: &str = "smol";

/// Registers the namer when `session.auto_name` is on, and the [`Namer`] service either way.
pub struct SessionNamesExt {
    pub auto_name: bool,
}

/// The naming started in the background, so the application can let it finish before exiting.
#[derive(Default)]
pub struct Namer {
    pending: Mutex<Option<JoinHandle<()>>>,
}

impl Namer {
    /// Waits for a naming still running, at most [`NAMING_LIMIT`].
    pub async fn settle(&self) {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner).take();
        if let Some(handle) = pending {
            let _ = tokio::time::timeout(NAMING_LIMIT, handle).await;
        }
    }
}

impl Extension for SessionNamesExt {
    fn id(&self) -> &str {
        "session-names"
    }

    fn register(&self, r: &mut Registrar) -> Result<(), ExtError> {
        let namer = Arc::new(Namer::default());
        r.service(namer.clone());
        if self.auto_name {
            r.on(EventKind::Stop, Arc::new(NameOnFirstStop { namer }));
        }
        Ok(())
    }
}

struct NameOnFirstStop {
    namer: Arc<Namer>,
}

#[async_trait]
impl EventHandler for NameOnFirstStop {
    async fn handle(&self, ev: &mut Event, ctx: &ExtCtx) -> Outcome {
        let Event::Stop { agent, .. } = ev else {
            return Outcome::Continue;
        };
        if agent.depth != 0 {
            return Outcome::Continue;
        }
        let (Some(session), Some(side)) = (ctx.services.get::<SessionStore>(), ctx.services.get::<Side>())
        else {
            return Outcome::Continue;
        };
        if session.name().is_some() {
            return Outcome::Continue;
        }
        let mut pending = self.namer.pending.lock().unwrap_or_else(PoisonError::into_inner);
        if pending.as_ref().is_some_and(|handle| !handle.is_finished()) {
            return Outcome::Continue;
        }
        let agent = agent.clone();
        *pending = Some(tokio::spawn(async move {
            match suggest(&side, &agent, &session).await {
                // Named by hand while the namer was thinking: the hand wins.
                Ok(name) if session.name().is_none() => {
                    session.append(EntryKind::SessionInfo { name });
                }
                Ok(_) => {}
                Err(why) => tracing::debug!("the session was not named: {why}"),
            }
        }));
        Outcome::Continue
    }
}

/// Asks the namer for a title for `session`, from its first prompt and reply. `Err` says why
/// there is none.
pub async fn suggest(side: &Side, agent: &AgentInfo, session: &SessionStore) -> Result<String, String> {
    let question = naming_question(session).ok_or("the session has no prompt to name it after yet")?;
    let mut ignore = |_: &str| {};
    let asked = side.ask(
        SideQuestion {
            agent,
            context: Vec::new(),
            question,
            chain: vec![ModelSpec::Role(NAMER_ROLE.to_string())],
            purpose: "naming",
            framed: false,
        },
        &mut ignore,
        CancellationToken::new(),
    );
    match tokio::time::timeout(NAMING_LIMIT, asked).await {
        Ok(Ok(answer)) => clean_title(&answer.text).ok_or_else(|| "the namer gave no title".to_string()),
        Ok(Err(why)) => Err(why),
        Err(_) => Err("the namer took too long".to_string()),
    }
}

fn excerpt(text: &str) -> String {
    match text.char_indices().nth(EXCERPT) {
        Some((cut, _)) => format!("{}...", &text[..cut]),
        None => text.to_string(),
    }
}

/// The request to the namer, from the session's first prompt and first reply.
fn naming_question(session: &SessionStore) -> Option<String> {
    let messages: Vec<_> = session
        .branch(None)
        .into_iter()
        .filter_map(|entry| match entry.kind {
            EntryKind::Message { message } => Some(message),
            _ => None,
        })
        .collect();
    let prompt = messages.iter().find(|m| m.role == Role::User)?.text();
    let reply = messages
        .iter()
        .find(|m| m.role == Role::Assistant && !m.text().trim().is_empty())
        .map(|m| m.text())
        .unwrap_or_default();
    Some(format!(
        "Give this coding session a title of at most {MAX_WORDS} words that says what the work is about. \
         Reply with the title only, no quotes.\n\nFirst request:\n{}\n\nFirst reply:\n{}",
        excerpt(prompt.trim()),
        excerpt(reply.trim())
    ))
}

/// The title from the namer's answer: its first line without quotes or a final full stop, at most
/// [`MAX_WORDS`] words and [`MAX_CHARS`] characters. `None` when nothing is left.
pub fn clean_title(answer: &str) -> Option<String> {
    let line = answer.lines().map(str::trim).find(|line| !line.is_empty())?;
    let line = line
        .trim_start_matches(['#', '*', ' '])
        .trim_matches(['"', '\'', '`', '*', ' '])
        .trim_start_matches("Title:")
        .trim()
        .trim_end_matches('.');
    let mut title = line
        .split_whitespace()
        .take(MAX_WORDS)
        .collect::<Vec<_>>()
        .join(" ");
    if let Some((cut, _)) = title.char_indices().nth(MAX_CHARS) {
        title.truncate(cut);
    }
    let title = title.trim().to_string();
    (!title.is_empty()).then_some(title)
}

#[cfg(test)]
mod tests {}
