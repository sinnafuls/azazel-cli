//! A stored transcript in the short form agents read: what was asked, what was said, and one line
//! per tool call.

use azazel_ai::{Block, Role};
use serde_json::Value;

use crate::session::{EntryKind, SessionStore};

/// What a tool call is about, in one line: the tool and the argument that says most.
pub fn activity(name: &str, input: &Value) -> String {
    let detail = ["command", "path", "pattern", "query", "url", "prompt", "to"]
        .iter()
        .find_map(|key| input.get(key).and_then(Value::as_str))
        .and_then(|text| text.lines().next())
        .unwrap_or_default();
    let detail: String = detail.chars().take(60).collect();
    if detail.is_empty() {
        name.to_string()
    } else {
        format!("{name} {detail}")
    }
}

/// One line per tool call and the full text of what the agent said.
pub fn condensed(store: &SessionStore) -> String {
    let mut out = String::new();
    for entry in store.entries() {
        let EntryKind::Message { message } = entry.kind else {
            continue;
        };
        match message.role {
            Role::User => out.push_str(&format!("[prompt]\n{}\n\n", message.text().trim())),
            Role::Assistant => {
                for block in &message.content {
                    match block {
                        Block::Text { text } if !text.trim().is_empty() => {
                            out.push_str(&format!("{}\n\n", text.trim()))
                        }
                        Block::ToolCall { name, input, .. } => {
                            out.push_str(&format!("→ {}\n", activity(name, input)))
                        }
                        _ => {}
                    }
                }
            }
            Role::Tool => {}
        }
    }
    out.trim_end().to_string()
}
