//! Sending the last prompt again: where the session goes back to, and what is submitted.

use std::path::Path;

use azazel_ai::{Block, Role};
use azazel_core::commands::expand_mentions;
use azazel_core::{EntryKind, SessionStore};

use crate::tui::is_note;

/// How the file blocks that `@path` mentions add to a prompt start.
const ATTACHED: &str = "\n\n<file path=\"";

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

/// The last message the user sent on the current branch, leaving out the notes the interface
/// writes itself (a `!command`'s output, a rewind).
pub fn last_prompt(session: &SessionStore, cwd: &Path) -> Option<Again> {
    let branch = session.branch(None);
    let (entry, message) = branch.iter().rev().find_map(|entry| match &entry.kind {
        EntryKind::Message { message } if message.role == Role::User && !is_note(&message.text()) => {
            Some((entry, message))
        }
        _ => None,
    })?;
    let stored = message.text();
    let text = match stored.find(ATTACHED) {
        Some(at) => stored[..at].to_string(),
        None => stored,
    };
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
        parent: entry.parent_id.clone(),
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
        let again = last_prompt(&session, dir.path()).unwrap();
        assert_eq!(again.parent.as_deref(), Some(reply.as_str()));
        assert_eq!(again.text, "look at @notes.md and @pixel.png");
        assert_eq!(again.images, [image("pasted")]);
    }
}
