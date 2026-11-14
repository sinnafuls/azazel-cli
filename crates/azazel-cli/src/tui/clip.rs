//! Copying to the clipboard: through tmux inside tmux, else the system clipboard where there is
//! one, the terminal's own (OSC 52) over SSH or when that fails. Also `/copy`.

use std::io::Write;

use azazel_ai::Role as MessageRole;
use azazel_core::EntryKind;
use azazel_tui::SelectItem;
use azazel_tui::components::pager::code_blocks;
use base64::Engine;

use super::overlay::{Action, Overlay};
use super::{Tui, first_line};

/// The escape sequence that asks the terminal to put `text` on the clipboard. Inside tmux it is
/// wrapped so that tmux passes it on to the terminal outside.
pub(super) fn osc52(text: &str, tmux: bool) -> String {
    let encoded = base64::engine::general_purpose::STANDARD.encode(text);
    let sequence = format!("\x1b]52;c;{encoded}\x07");
    if tmux {
        // Pass-through: every escape inside is doubled.
        format!("\x1bPtmux;{}\x1b\\", sequence.replace('\x1b', "\x1b\x1b"))
    } else {
        sequence
    }
}

/// Whether this terminal is at the other end of an SSH connection, where the system clipboard
/// is the remote machine's and not the one the person pastes from.
fn over_ssh() -> bool {
    ["SSH_CONNECTION", "SSH_TTY", "SSH_CLIENT"]
        .iter()
        .any(|name| std::env::var_os(name).is_some())
}

/// Hands `text` to tmux as its newest buffer; with `-w` tmux also passes it on to the clipboard
/// of the terminal it runs in.
fn tmux_load(text: &str) -> bool {
    use std::process::{Command, Stdio};
    let Ok(mut child) = Command::new("tmux")
        .args(["load-buffer", "-w", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let written = child
        .stdin
        .take()
        .is_some_and(|mut stdin| stdin.write_all(text.as_bytes()).is_ok());
    child.wait().is_ok_and(|status| status.success()) && written
}

/// Puts `text` on the clipboard and says which one: tmux's (which passes it on), the system's,
/// or the terminal's.
pub(super) fn copy(text: &str, out: &mut impl Write) -> &'static str {
    let in_tmux = std::env::var_os("TMUX").is_some();
    if in_tmux && tmux_load(text) {
        return "the tmux buffer and the clipboard";
    }
    let system = !over_ssh()
        && arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text))
            .is_ok();
    if system {
        return "the clipboard";
    }
    let _ = out.write_all(osc52(text, in_tmux).as_bytes());
    let _ = out.flush();
    "the terminal's clipboard"
}

/// How much was copied, for the confirmation.
pub(super) fn size(text: &str) -> String {
    match text.lines().count() {
        0 | 1 => format!("{} characters", text.chars().count()),
        lines => format!("{lines} lines"),
    }
}

/// The last `count` of `sections` as one text, a blank line between them; `None` when there is
/// none.
fn last_sections(sections: &[String], count: usize) -> Option<String> {
    let from = sections.len().saturating_sub(count);
    let text = sections[from..].join("\n");
    (!text.is_empty()).then(|| text.trim_end().to_string())
}

impl Tui {
    pub(super) fn copy_text(&mut self, text: &str, what: &str) {
        let how = copy(text, self.renderer.get_mut());
        self.info(&format!("copied {what} ({}) to {how}", size(text)));
    }

    /// The agent's replies on the current branch, newest first, as Markdown.
    fn replies(&self) -> Vec<String> {
        self.session
            .branch(None)
            .into_iter()
            .rev()
            .filter_map(|entry| match entry.kind {
                EntryKind::Message { message } if message.role == MessageRole::Assistant => {
                    Some(message.text().trim().to_string()).filter(|text| !text.is_empty())
                }
                _ => None,
            })
            .collect()
    }

    /// `/copy last <n>`: the last `n` messages of the conversation, yours and the agent's, as
    /// the Markdown `/export` writes.
    fn copy_last(&mut self, count: &str) {
        let Some(count) = count.trim().parse::<usize>().ok().filter(|count| *count >= 1) else {
            return self.fail("usage: /copy last <n> for the last n messages, yours and the agent's");
        };
        let sections = crate::export::sections(&self.session);
        let Some(text) = last_sections(&sections, count) else {
            return self.info("there is no message to copy yet");
        };
        let what = match count.min(sections.len()) {
            1 => "the last message".to_string(),
            taken => format!("the last {taken} messages"),
        };
        self.copy_text(&text, &what);
    }

    /// `/copy`: the last reply; `/copy <n>`: the n-th reply from the end; `/copy code`: one of
    /// the last reply's code blocks; `/copy last <n>`: the last n messages.
    pub(super) fn copy_command(&mut self, args: &str) {
        if let Some(count) = args.strip_prefix("last") {
            return self.copy_last(count);
        }
        let replies = self.replies();
        let Some(last) = replies.first() else {
            return self.info("there is no reply to copy yet");
        };
        match args {
            "" => {
                let text = last.clone();
                self.copy_text(&text, "the last reply");
            }
            "code" => {
                let mut blocks = code_blocks(last);
                match blocks.len() {
                    0 => self.info("the last reply has no code block"),
                    1 => {
                        let block = blocks.remove(0);
                        self.copy_text(&block, "the code block");
                    }
                    _ => {
                        let items = blocks
                            .iter()
                            .map(|block| SelectItem::new(first_line(block), size(block)))
                            .collect();
                        self.overlay = Some(Overlay::select(
                            "Copy which code block?",
                            Vec::new(),
                            items,
                            false,
                            Action::CopyCode(blocks),
                        ));
                    }
                }
            }
            number => match number.parse::<usize>() {
                Ok(n) if n >= 1 => match replies.get(n - 1) {
                    Some(reply) => {
                        let text = reply.clone();
                        self.copy_text(&text, &format!("reply {n} from the end"));
                    }
                    None => self.fail(&format!("this conversation has {} replies", replies.len())),
                },
                _ => self.fail(
                    "usage: /copy, /copy code, /copy <n> for the n-th reply from the end, or /copy last <n> for \
                     the last n messages",
                ),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn osc52_carries_the_text_as_base64_and_is_wrapped_for_tmux() {
        assert_eq!(osc52("hi there", false), "\x1b]52;c;aGkgdGhlcmU=\x07");
        assert_eq!(
            osc52("hi there", true),
            "\x1bPtmux;\x1b\x1b]52;c;aGkgdGhlcmU=\x07\x1b\\"
        );
        assert_eq!(osc52("", false), "\x1b]52;c;\x07");
    }

    #[test]
    fn the_last_messages_are_copied_as_the_export_writes_them() {
        use azazel_ai::{Block, Message};
        let dir = tempfile::tempdir().unwrap();
        let store = azazel_core::SessionStore::create(dir.path(), dir.path(), None).unwrap();
        let say = |message: Message| drop(store.append(EntryKind::Message { message }));
        let assistant = |text: &str| Message {
            role: MessageRole::Assistant,
            content: vec![Block::text(text)],
            meta: None,
        };
        say(Message::user("first question"));
        let mut reply = assistant("Looking.");
        reply.content.push(Block::ToolCall {
            id: "c1".into(),
            name: "read".into(),
            input: serde_json::json!({ "path": "a.txt" }),
            signature: None,
        });
        say(reply);
        say(Message::tool(vec![Block::tool_result("c1", "contents", false)]));
        say(assistant("It says hello."));
        say(Message::user("thanks"));

        let sections = crate::export::sections(&store);
        // A tool result is no message of its own: the two before it and the two after count.
        assert_eq!(sections.len(), 4);
        assert_eq!(
            last_sections(&sections, 2).unwrap(),
            "## Assistant\n\nIt says hello.\n\n## You\n\nthanks"
        );
        assert_eq!(
            last_sections(&sections, 3).unwrap(),
            "## Assistant\n\nLooking.\n- `read` a.txt\n\n## Assistant\n\nIt says hello.\n\n## You\n\nthanks"
        );
        // Asking for more than there is gives everything.
        assert!(
            last_sections(&sections, 99)
                .unwrap()
                .starts_with("## You\n\nfirst question")
        );
        assert_eq!(last_sections(&[], 3), None);
    }
}
