//! Summaries of a branch the conversation leaves (`/tree`): what happened on it is written up
//! by the `smol` role and added to the conversation where it goes on, so the agent knows what
//! was tried there without the branch being in its context.

use std::collections::HashSet;
use std::time::Duration;

use azazel_agent::{AgentCmd, UserInput};
use azazel_ai::{Block, Message, ModelSpec, Role};
use azazel_core::ext::Level;
use azazel_core::names::NAMER_ROLE;
use azazel_core::side::{Side, SideQuestion};
use azazel_core::{EntryKind, SessionStore, Ui};
use azazel_tui::text::truncate;
use tokio_util::sync::CancellationToken;

use crate::app::{Active, App};
use crate::tui::view::tool_summary;

/// How the note about a branch that was left starts in the agent's context.
pub(crate) const BRANCH_NOTE: &str = "I went back to an earlier point of this conversation. What happened on the \
branch I left, in short:";
/// How long the summary may take.
const LIMIT: Duration = Duration::from_secs(60);
/// The most of a branch that is sent to be summarised, in characters; the middle of a longer
/// one is left out.
const MAX_CHARS: usize = 24_000;
/// The most of one tool result that is sent, in characters.
const MAX_RESULT_CHARS: usize = 400;

const INSTRUCTIONS: &str = "The user of a coding agent went back to an earlier point of their conversation. Below is \
the part they left behind, which the agent will not see any more. Write it up for the agent in at most six short \
lines: what was asked for, what was done (files changed, commands run), what was found out, and what was left \
unfinished. Plain text, no heading, nothing before or after the lines.";

/// The messages of the branch that ended at `left` which are not on the current branch.
pub(crate) fn left_behind(session: &SessionStore, left: Option<&str>) -> Vec<Message> {
    let Some(left) = left else {
        return Vec::new();
    };
    let here: HashSet<String> = session.branch(None).into_iter().map(|entry| entry.id).collect();
    session
        .branch(Some(left))
        .into_iter()
        .filter(|entry| !here.contains(&entry.id))
        .filter_map(|entry| match entry.kind {
            EntryKind::Message { message } => Some(message),
            _ => None,
        })
        .collect()
}

/// `messages` as the text the summariser reads: who said what, which tools were called and the
/// start of what each returned.
fn written_out(messages: &[Message]) -> String {
    let mut out = String::new();
    for message in messages {
        for block in &message.content {
            let line = match (message.role, block) {
                (Role::User, Block::Text { text }) => format!("User: {}", text.trim()),
                (Role::Assistant, Block::Text { text }) => format!("Agent: {}", text.trim()),
                (_, Block::ToolCall { name, input, .. }) => {
                    format!("Agent called {name}: {}", tool_summary(name, input))
                }
                (
                    _,
                    Block::ToolResult {
                        content, is_error, ..
                    },
                ) => {
                    let text: String = content
                        .iter()
                        .filter_map(|block| match block {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect();
                    let what = if *is_error { "Failed" } else { "Result" };
                    // One line, however many the tool printed.
                    let text = text.trim().replace(['\r', '\n'], " ");
                    format!("{what}: {}", truncate(&text, MAX_RESULT_CHARS, "…"))
                }
                _ => continue,
            };
            if line.ends_with(": ") {
                continue;
            }
            out.push_str(&line);
            out.push('\n');
        }
    }
    out
}

/// The request to the summariser for the branch `messages` were on; `None` when the agent did
/// nothing on it, so there is nothing to tell.
pub(crate) fn question(messages: &[Message]) -> Option<String> {
    let acted = messages
        .iter()
        .any(|message| message.role == Role::Assistant && !message.content.is_empty());
    if !acted {
        return None;
    }
    let mut text = written_out(messages);
    let chars = text.chars().count();
    if chars > MAX_CHARS {
        // The start says what was asked for and the end where it got to.
        let head: String = text.chars().take(MAX_CHARS / 3).collect();
        let tail: String = text.chars().skip(chars - MAX_CHARS * 2 / 3).collect();
        text = format!("{head}\n[...]\n{tail}");
    }
    Some(format!(
        "{INSTRUCTIONS}\n\n<conversation>\n{}\n</conversation>",
        text.trim_end()
    ))
}

/// After the conversation moved away from the branch that ended at `left`: has the `smol` role
/// write up what happened there and adds that to the conversation where it is now, as a note
/// that asks for no reply. The user is told when it is there. Returns at once; a summary that
/// cannot be had is left out.
pub(crate) fn summarise_left(app: &App, active: &Active, left: Option<&str>) {
    if !app.config.session.branch_summary {
        return;
    }
    let Some(question) = question(&left_behind(&active.session, left)) else {
        return;
    };
    let services = app.registry.services.clone();
    let Some(side) = services.get::<Side>() else {
        return;
    };
    let (lead, session) = (active.lead.clone(), active.session.clone());
    let came_to = session.leaf();
    tokio::spawn(async move {
        let mut ignore = |_: &str| {};
        let asked = side.ask(
            SideQuestion {
                agent: lead.info(),
                context: Vec::new(),
                question,
                chain: vec![ModelSpec::Role(NAMER_ROLE.to_string())],
                purpose: "branch summary",
                framed: false,
            },
            &mut ignore,
            CancellationToken::new(),
        );
        let summary = match tokio::time::timeout(LIMIT, asked).await {
            Ok(Ok(answer)) => answer.text.trim().to_string(),
            Ok(Err(why)) => return tracing::debug!("the branch that was left was not summarised: {why}"),
            Err(_) => {
                return tracing::debug!("the branch that was left was not summarised: it took too long");
            }
        };
        // The conversation went somewhere else again meanwhile: the note would be out of place.
        let still_here = match &came_to {
            Some(came_to) => session.branch(None).iter().any(|entry| &entry.id == came_to),
            None => true,
        };
        if summary.is_empty() || !still_here {
            return;
        }
        if lead.send(AgentCmd::Append(UserInput::text(format!(
            "{BRANCH_NOTE}\n{summary}"
        )))) && let Some(ui) = services.get::<Ui>()
        {
            ui.0.notify(
                Level::Info,
                &format!("the agent was told what happened on the branch you left:\n{summary}"),
            );
        }
    });
}

#[cfg(test)]
mod tests {

    use super::*;

    fn reply(content: Vec<Block>) -> Message {
        Message {
            role: Role::Assistant,
            content,
            meta: None,
        }
    }

    #[test]
    fn what_was_left_behind_is_the_part_of_the_old_branch_that_is_not_on_the_new_one() {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        let say = |message: Message| store.append(EntryKind::Message { message });
        say(Message::user("first"));
        let one = say(reply(vec![Block::text("one")]));
        say(Message::user("second"));
        let two = say(reply(vec![Block::text("two")]));
        let texts = |messages: Vec<Message>| -> Vec<String> { messages.iter().map(Message::text).collect() };

        // Nothing is left behind where the conversation is.
        assert!(left_behind(&store, Some(&two)).is_empty());
        assert!(left_behind(&store, None).is_empty());
        // Going back to the first reply leaves the second exchange.
        store.set_leaf(Some(&one));
        assert_eq!(texts(left_behind(&store, Some(&two))), ["second", "two"]);
        // On from there, and back to the old end: the new exchange is what is left now.
        say(Message::user("another way"));
        let other = say(reply(vec![Block::text("done otherwise")]));
        store.set_leaf(Some(&two));
        assert_eq!(
            texts(left_behind(&store, Some(&other))),
            ["another way", "done otherwise"]
        );
        // Going on down the same branch leaves nothing.
        store.set_leaf(Some(&one));
        store.set_leaf(Some(&two));
        assert!(left_behind(&store, Some(&one)).is_empty());
    }
}
