//! Sending the last prompt again: where the session goes back to, and what is submitted.

use std::path::Path;

use azazel_ai::Block;
use azazel_core::commands::expand_mentions;
use azazel_core::{EntryKind, SessionStore};

use crate::tui::is_note;

/// How the file blocks that `@path` mentions add to a prompt start.
const ATTACHED: &str = "\n\n<file path=\"";

/// How the first message of a session started by `/handoff` starts: the brief, not a prompt.
pub const HANDOFF_NOTE: &str = "Hand-off brief from session ";

/// A user message as it was typed: without the file blocks its `@path` mentions attached.
pub fn typed(sent: &str) -> &str {
    match sent.find(ATTACHED) {
        Some(at) => &sent[..at],
        None => sent,
    }
}

/// Whether a user message is a prompt the user wrote, rather than a note the interface wrote
/// itself (a `!command`'s output, a rewind, a hand-off brief).
pub fn is_prompt(sent: &str) -> bool {
    !is_note(sent) && !sent.starts_with(HANDOFF_NOTE)
}

/// The last prompt of a branch, ready to be submitted again.
#[derive(Debug, Clone, PartialEq)]
pub struct Again {
    /// The entry the prompt followed; the leaf moves here, so the earlier attempt stays in the
    /// file as a branch. `None` when the prompt was the first entry.
    pub parent: Option<String>,
    /// The prompt as it was typed: files its `@path` mentions attached are read again when it
    /// is submitted.
    pub text: String,
    /// The images that were pasted or sent with it. Images its mentions attached are left out
    /// for the same reason.
    pub images: Vec<Block>,
}

/// The last prompt the user wrote on the current branch (see [`is_prompt`]).
pub fn last_prompt(session: &SessionStore, cwd: &Path) -> Option<Again> {
    let skip = |sent: &str| !is_prompt(sent);
    let entry = session.last_prompt(None, &skip)?;
    let EntryKind::Message { message } = &entry.kind else {
        return None;
    };
    let text = typed(&message.text()).to_string();
    let mut images: Vec<Block> = message
        .content
        .iter()
        .filter(|block| matches!(block, Block::Image { .. }))
        .cloned()
        .collect();
    // Mentioned images were added after the given ones, and submitting adds them again.
    let mentioned = expand_mentions(&text, cwd).images.len();
    images.truncate(images.len().saturating_sub(mentioned));
    Some(Again {
        parent: entry.parent_id,
        text,
        images,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use azazel_ai::Message;

    use super::*;
    use crate::tui::SHELL_NOTE;

    fn image(data: &str) -> Block {
        Block::Image {
            mime: "image/png".into(),
            data: data.into(),
        }
    }

    #[test]
    fn the_last_typed_prompt_comes_back_without_what_mentions_attached() {
        let dir = tempfile::tempdir().unwrap();
        let session = SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap();
        assert_eq!(last_prompt(&session, dir.path()), None);

        let first = session.append(EntryKind::Message {
            message: Message::user("first"),
        });
        let reply = session.append(EntryKind::Message {
            message: Message::user_blocks(vec![Block::text("reply stand-in")]),
        });
        assert_eq!(
            last_prompt(&session, dir.path()).unwrap().parent.as_deref(),
            Some(first.as_str())
        );

        // A mentioned text file was appended to the prompt, and a mentioned image attached after
        // the pasted one.
        std::fs::write(dir.path().join("pixel.png"), b"\x89PNG\r\n\x1a\n rest").unwrap();
        session.append(EntryKind::Message {
            message: Message::user_blocks(vec![
                Block::text("look at @notes.md and @pixel.png\n\n<file path=\"notes.md\">\nold\n</file>"),
                image("pasted"),
                image("mentioned"),
            ]),
        });
        // What the interface wrote after it is not a prompt.
        session.append(EntryKind::Message {
            message: Message::user(format!("{SHELL_NOTE}ls` myself:\nfile\nexit code: 0")),
        });
        session.append(EntryKind::Message {
            message: Message::user(format!("{HANDOFF_NOTE}abc:\n\nthe brief")),
        });
        let again = last_prompt(&session, dir.path()).unwrap();
        assert_eq!(again.parent.as_deref(), Some(reply.as_str()));
        assert_eq!(again.text, "look at @notes.md and @pixel.png");
        assert_eq!(again.images, [image("pasted")]);
    }
}
