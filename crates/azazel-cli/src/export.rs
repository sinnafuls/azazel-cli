//! Writing a session out: Markdown to read, HTML to share (one file, no scripts), or the branch's
//! entries as JSON lines. Used by `/export`, `azazel export` and `azazel sessions export`.
//!
//! With `tui.redact` on, what looks like a secret is hidden in the Markdown and the HTML. The
//! JSON lines are the entries as the session stores them, so that the file opens as a session
//! again: nothing is hidden in those. `--sanitize` exports a [`sanitized`] copy instead, in which
//! secrets are hidden in every format and what files held is left out.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::Path;

use azazel_ai::{Block, Message, Role};
use azazel_core::SessionStore;
use azazel_core::session::{Entry, EntryKind};
use serde_json::Value;

use crate::tui::view::{redacted, tool_summary};

/// What a sanitized export holds in place of what a file held.
pub const REMOVED: &str = "[file content removed]";

/// The tools whose results are what files hold: their text, matching lines of it, or diffs.
const FILE_TOOLS: &[&str] = &["read", "grep", "edit", "write", "patch", "lsp", "ast_grep"];

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Markdown,
    Html,
    Jsonl,
}

impl Format {
    /// The format a file name asks for by its extension.
    pub fn from_path(path: &Path) -> Option<Format> {
        match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
            "md" | "markdown" => Some(Format::Markdown),
            "html" | "htm" => Some(Format::Html),
            "jsonl" | "json" => Some(Format::Jsonl),
            _ => None,
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Markdown => "md",
            Format::Html => "html",
            Format::Jsonl => "jsonl",
        }
    }
}

/// `azazel-session-<id>.<ext>`.
pub fn default_file_name(session: &SessionStore, format: Format) -> String {
    format!("azazel-session-{}.{}", session.id(), format.extension())
}

fn title(session: &SessionStore) -> String {
    session
        .name()
        .unwrap_or_else(|| format!("Session {}", session.id()))
}

/// The session's current branch in `format`; with `redact`, without what looks like a secret
/// where the format is one to read or share.
pub fn render(session: &SessionStore, format: Format, redact: bool) -> String {
    match format {
        Format::Markdown => redacted(markdown(session), redact),
        Format::Html => html(session, redact),
        Format::Jsonl => session
            .branch(None)
            .iter()
            .filter_map(|entry| serde_json::to_string(entry).ok())
            .map(|line| line + "\n")
            .collect(),
    }
}

fn markdown(session: &SessionStore) -> String {
    let mut out = format!("# {}\n", title(session));
    for section in sections(session) {
        out.push('\n');
        out.push_str(&section);
    }
    out
}

/// The conversation on the current branch as Markdown sections, one per message that says
/// something: a heading with who wrote it, then its text and the tools it called.
pub(crate) fn sections(session: &SessionStore) -> Vec<String> {
    let mut out = Vec::new();
    for entry in session.build_context(None) {
        let heading = match entry.message.role {
            _ if entry.summary => "Earlier (summarised)",
            Role::User => "You",
            Role::Assistant => "Assistant",
            Role::Tool => continue,
        };
        let mut body = String::new();
        for block in &entry.message.content {
            match block {
                Block::Text { text } => body.push_str(text),
                Block::ToolCall { name, input, .. } => {
                    body.push_str(&format!("\n- `{name}` {}\n", tool_summary(name, input)));
                }
                Block::Image { .. } => body.push_str("\n[image]\n"),
                Block::Thinking { .. } | Block::ToolResult { .. } | Block::Opaque { .. } => {}
            }
        }
        if !body.trim().is_empty() {
            out.push(format!("## {heading}\n\n{}\n", body.trim()));
        }
    }
    out
}

const STYLE: &str = "body{font:15px/1.5 system-ui,sans-serif;max-width:52rem;margin:2rem auto;padding:0 1rem;\
color:#1d1d1f;background:#fff}h1{font-size:1.4rem}.msg{border-radius:8px;padding:.6rem .9rem;margin:.8rem 0}\
.user{background:#eef3ff}.assistant{background:#f6f6f6}.summary{background:#fff7e0}.role{font-size:.75rem;\
font-weight:600;text-transform:uppercase;color:#666;margin-bottom:.3rem}pre{white-space:pre-wrap;\
word-break:break-word;margin:.3rem 0;font:13px/1.4 ui-monospace,monospace}details{margin:.4rem 0}\
summary{cursor:pointer;color:#444;font:13px ui-monospace,monospace}.add{color:#116329;background:#e6ffec}\
.del{color:#82071e;background:#ffebe9}.hunk{color:#6639ba}.error{color:#82071e}img{max-width:100%}\
@media(prefers-color-scheme:dark){body{background:#111;color:#ddd}.user{background:#1b2333}\
.assistant{background:#1c1c1c}.summary{background:#2a2414}.add{background:#12261a;color:#7ee2a0}\
.del{background:#2d1216;color:#ff9aa5}summary,.role{color:#aaa}}";

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            _ => out.push(c),
        }
    }
    out
}

/// Tool output shown as preformatted text; when it reads as a diff, its lines are coloured.
fn output_block(text: &str) -> String {
    let diff = text.lines().any(|line| line.starts_with("@@"));
    let mut out = String::from("<pre>");
    for line in text.lines() {
        let class = match line.chars().next() {
            Some('+') if diff && !line.starts_with("+++") => Some("add"),
            Some('-') if diff && !line.starts_with("---") => Some("del"),
            Some('@') if diff && line.starts_with("@@") => Some("hunk"),
            _ => None,
        };
        match class {
            Some(class) => {
                let _ = writeln!(out, "<span class=\"{class}\">{}</span>", escape(line));
            }
            None => {
                let _ = writeln!(out, "{}", escape(line));
            }
        }
    }
    out.push_str("</pre>");
    out
}

fn html(session: &SessionStore, redact: bool) -> String {
    // Each text is cleared of secrets on its own: a picture's data must stay as it is.
    let shown = |text: &str| escape(&redacted(text.to_string(), redact));
    let title = shown(&title(session));
    let mut out = format!(
        "<!doctype html>\n<html lang=\"en\"><head><meta charset=\"utf-8\"><meta name=\"viewport\" \
         content=\"width=device-width,initial-scale=1\"><title>{title}</title><style>{STYLE}</style></head>\
         <body>\n<h1>{title}</h1>\n"
    );
    let entries = session.build_context(None);
    // Tool results are shown inside the call that produced them.
    let result_of = |call_id: &str| {
        entries.iter().find_map(|entry| {
            entry.message.content.iter().find_map(|block| match block {
                Block::ToolResult {
                    call_id: id,
                    content,
                    is_error,
                } if id == call_id => Some((content, *is_error)),
                _ => None,
            })
        })
    };
    for entry in &entries {
        let (class, role) = match entry.message.role {
            _ if entry.summary => ("summary", "Earlier, summarised"),
            Role::User => ("user", "You"),
            Role::Assistant => ("assistant", "Assistant"),
            Role::Tool => continue,
        };
        let mut body = String::new();
        for block in &entry.message.content {
            match block {
                Block::Text { text } if !text.trim().is_empty() => {
                    let _ = write!(body, "<pre>{}</pre>", shown(text.trim()));
                }
                Block::Thinking { text, .. } if !text.trim().is_empty() => {
                    let _ = write!(
                        body,
                        "<details><summary>thinking</summary><pre>{}</pre></details>",
                        shown(text.trim())
                    );
                }
                Block::Image { mime, data } => {
                    let _ = write!(
                        body,
                        "<img alt=\"image\" src=\"data:{};base64,{}\">",
                        escape(mime),
                        escape(data)
                    );
                }
                Block::ToolCall { id, name, input, .. } => {
                    let summary = shown(&format!("{name} {}", tool_summary(name, input)));
                    let _ = write!(body, "<details><summary>{summary}</summary>");
                    match result_of(id) {
                        Some((content, is_error)) => {
                            for part in content {
                                match part {
                                    Block::Text { text } if is_error => {
                                        let _ = write!(body, "<pre class=\"error\">{}</pre>", shown(text));
                                    }
                                    Block::Text { text } => {
                                        body.push_str(&output_block(&redacted(text.clone(), redact)));
                                    }
                                    Block::Image { mime, data } => {
                                        let _ = write!(
                                            body,
                                            "<img alt=\"tool image\" src=\"data:{};base64,{}\">",
                                            escape(mime),
                                            escape(data)
                                        );
                                    }
                                    _ => {}
                                }
                            }
                        }
                        None => body.push_str("<pre>(no result)</pre>"),
                    }
                    body.push_str("</details>");
                }
                _ => {}
            }
        }
        if !body.is_empty() {
            let _ = writeln!(
                out,
                "<div class=\"msg {class}\"><div class=\"role\">{role}</div>{body}</div>"
            );
        }
    }
    out.push_str("</body></html>\n");
    out
}

/// A copy of `session`, in memory, fit to share: what looks like a secret is hidden in every
/// text, and what files held is left out: the results of the tools that read, search or change
/// files, and the text `write`, `edit` and `patch` were given. Pictures are kept as they are.
pub fn sanitized(session: &SessionStore) -> SessionStore {
    let entries = session.entries();
    let mut tools: HashMap<String, String> = HashMap::new();
    for entry in &entries {
        if let EntryKind::Message { message } = &entry.kind {
            for (id, name, _) in message.tool_calls() {
                tools.insert(id.to_string(), name.to_string());
            }
        }
    }
    let entries = entries
        .into_iter()
        .map(|entry| Entry {
            kind: scrubbed(entry.kind, &tools),
            ..entry
        })
        .collect();
    SessionStore::mirror(session.header(), entries, session.leaf(), session.path())
}

fn hidden(text: String) -> String {
    redacted(text, true)
}

fn scrubbed(kind: EntryKind, tools: &HashMap<String, String>) -> EntryKind {
    match kind {
        EntryKind::Message { message } => EntryKind::Message {
            message: scrubbed_message(message, tools),
        },
        EntryKind::Compaction {
            summary,
            first_kept_entry_id,
            tokens_before,
        } => EntryKind::Compaction {
            summary: hidden(summary),
            first_kept_entry_id,
            tokens_before,
        },
        EntryKind::ContextEdit {
            target_id,
            replacement,
        } => EntryKind::ContextEdit {
            target_id,
            replacement: hidden(replacement),
        },
        EntryKind::SessionInfo { name } => EntryKind::SessionInfo { name: hidden(name) },
        EntryKind::Label { target_id, label } => EntryKind::Label {
            target_id,
            label: hidden(label),
        },
        EntryKind::Custom { custom_type, data } => EntryKind::Custom {
            custom_type,
            data: hidden_json(data),
        },
        EntryKind::ToolStarted { call_id, name, input } => EntryKind::ToolStarted {
            input: scrubbed_input(&name, input),
            call_id,
            name,
        },
        other => other,
    }
}

fn scrubbed_message(mut message: Message, tools: &HashMap<String, String>) -> Message {
    message.content = std::mem::take(&mut message.content)
        .into_iter()
        .map(|block| match block {
            Block::Text { text } => Block::Text { text: hidden(text) },
            Block::Thinking {
                text,
                signature,
                redacted,
            } => Block::Thinking {
                text: hidden(text),
                signature,
                redacted,
            },
            Block::ToolCall {
                id,
                name,
                input,
                signature,
            } => Block::ToolCall {
                input: scrubbed_input(&name, input),
                id,
                name,
                signature,
            },
            Block::ToolResult {
                call_id,
                content,
                is_error,
            } => {
                let of_files = tools
                    .get(&call_id)
                    .is_some_and(|name| FILE_TOOLS.contains(&name.as_str()));
                let content = match of_files && !is_error {
                    true => vec![Block::text(REMOVED)],
                    false => content
                        .into_iter()
                        .map(|part| match part {
                            Block::Text { text } => Block::Text { text: hidden(text) },
                            other => other,
                        })
                        .collect(),
                };
                Block::ToolResult {
                    call_id,
                    content,
                    is_error,
                }
            }
            other => other,
        })
        .collect();
    message
}

/// A tool call's input with secrets hidden and, for the tools that write files, the text they
/// were to write left out.
fn scrubbed_input(tool: &str, input: Value) -> Value {
    let mut input = hidden_json(input);
    let removed = || Value::String(REMOVED.to_string());
    match tool {
        "write" => {
            if let Some(content) = input.get_mut("content") {
                *content = removed();
            }
        }
        "edit" => {
            for edit in input
                .get_mut("edits")
                .and_then(Value::as_array_mut)
                .into_iter()
                .flatten()
            {
                for key in ["old", "new"] {
                    if let Some(text) = edit.get_mut(key) {
                        *text = removed();
                    }
                }
            }
        }
        "patch" => {
            if let Some(text) = input.get_mut("patch_text") {
                *text = removed();
            }
        }
        _ => {}
    }
    input
}

fn hidden_json(value: Value) -> Value {
    match value {
        Value::String(text) => Value::String(hidden(text)),
        Value::Array(items) => Value::Array(items.into_iter().map(hidden_json).collect()),
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| (key, hidden_json(value)))
                .collect(),
        ),
        other => other,
    }
}

/// Whether the session has anything worth exporting: a message, not only its header.
pub fn has_messages(session: &SessionStore) -> bool {
    session
        .branch(None)
        .iter()
        .any(|entry| matches!(entry.kind, EntryKind::Message { .. }))
}

#[cfg(test)]
mod tests {
    use azazel_ai::Message;
    use serde_json::json;

    use super::*;

    fn assistant(text: &str) -> Message {
        let mut message = Message::user(text);
        message.role = Role::Assistant;
        message
    }

    fn session() -> (tempfile::TempDir, SessionStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        store.append(EntryKind::SessionInfo {
            name: "fix <the> tests".into(),
        });
        store.append(EntryKind::Message {
            message: Message::user("please fix it & say so"),
        });
        let mut call = assistant("Reading.");
        call.content.insert(
            0,
            Block::Thinking {
                text: "look first".into(),
                signature: None,
                redacted: false,
            },
        );
        call.content.push(Block::ToolCall {
            id: "c1".into(),
            name: "edit".into(),
            input: json!({ "path": "src/a.rs" }),
            signature: None,
        });
        store.append(EntryKind::Message { message: call });
        store.append(EntryKind::Message {
            message: Message::tool(vec![Block::ToolResult {
                call_id: "c1".into(),
                content: vec![Block::text("@@ -1 +1 @@\n-old line\n+new line")],
                is_error: false,
            }]),
        });
        store.append(EntryKind::Message {
            message: assistant("Done: <b>fixed</b>."),
        });
        (dir, store)
    }

    #[test]
    fn html_is_one_safe_file_with_every_message_once() {
        let (_dir, store) = session();
        let page = render(&store, Format::Html, false);
        assert!(!page.contains("<script"));
        assert!(page.contains("<title>fix &lt;the&gt; tests</title>"));
        assert_eq!(page.matches("please fix it &amp; say so").count(), 1);
        assert_eq!(page.matches("Done: &lt;b&gt;fixed&lt;/b&gt;.").count(), 1);
        assert!(page.contains("<details><summary>thinking</summary><pre>look first</pre></details>"));
        assert!(page.contains("<details><summary>edit src/a.rs</summary>"));
        assert!(page.contains("<span class=\"del\">-old line</span>"));
        assert!(page.contains("<span class=\"add\">+new line</span>"));
    }

    #[test]
    fn markdown_and_jsonl_follow_the_branch() {
        let (_dir, store) = session();
        let text = render(&store, Format::Markdown, false);
        assert!(text.starts_with("# fix <the> tests\n"));
        assert!(text.contains("## You\n\nplease fix it & say so"));
        assert!(text.contains("- `edit` src/a.rs"));

        let lines = render(&store, Format::Jsonl, true);
        assert_eq!(lines.lines().count(), store.branch(None).len());
        let first: serde_json::Value = serde_json::from_str(lines.lines().next().unwrap()).unwrap();
        assert_eq!(first["type"], "session_info");
        assert!(has_messages(&store));

        assert_eq!(Format::from_path(Path::new("x.HTML")), Some(Format::Html));
        assert_eq!(Format::from_path(Path::new("x.txt")), None);
    }

    #[test]
    fn secrets_are_hidden_in_what_is_read_and_kept_in_the_entries() {
        let key = "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        store.append(EntryKind::Message {
            message: Message::user(format!("my key is {key}")),
        });
        let mut call = assistant("Checking.");
        call.content.push(Block::ToolCall {
            id: "c1".into(),
            name: "shell".into(),
            input: json!({ "command": "cat .env" }),
            signature: None,
        });
        store.append(EntryKind::Message { message: call });
        store.append(EntryKind::Message {
            message: Message::tool(vec![Block::ToolResult {
                call_id: "c1".into(),
                content: vec![
                    Block::text(format!("ANTHROPIC_API_KEY={key}\nDB_PASSWORD=\"hunter two\"")),
                    // A picture's data is never rewritten, whatever it happens to spell.
                    Block::Image {
                        mime: "image/png".into(),
                        data: format!("AAAA{key}AAAA"),
                    },
                ],
                is_error: false,
            }]),
        });

        for format in [Format::Markdown, Format::Html] {
            let text = render(&store, format, true);
            assert!(text.contains("my key is [redacted:anthropic-key]"), "{text}");
            assert_eq!(
                text.matches(key).count(),
                usize::from(format == Format::Html),
                "{text}"
            );
            assert!(render(&store, format, false).contains(&format!("my key is {key}")));
        }
        let page = render(&store, Format::Html, true);
        assert!(page.contains("ANTHROPIC_API_KEY=[redacted:anthropic-key]\nDB_PASSWORD=[redacted:secret]"));
        assert!(
            page.contains(&format!("base64,AAAA{key}AAAA\"")),
            "the picture is intact"
        );

        // The entries are the session itself: the file must open as one again.
        let lines = render(&store, Format::Jsonl, true);
        assert_eq!(lines.matches(key).count(), 3);
    }

    #[test]
    fn a_sanitized_export_hides_secrets_and_what_files_held_in_every_format() {
        let key = "sk-ant-api03-A1b2C3d4E5f6G7h8I9j0K1l2M3n4O5p6";
        let dir = tempfile::tempdir().unwrap();
        let store = SessionStore::create(dir.path(), dir.path(), None).unwrap();
        store.append(EntryKind::Message {
            message: Message::user(format!("read src/a.rs; my key is {key}")),
        });
        let mut calls = assistant("Reading.");
        for (id, name, input) in [
            ("c1", "read", json!({ "path": "src/a.rs" })),
            (
                "c2",
                "write",
                json!({ "path": "b.txt", "content": "fn secret_sauce() {}" }),
            ),
            (
                "c3",
                "edit",
                json!({ "path": "src/a.rs", "edits": [{ "old": "fn a()", "new": "fn private_name()" }] }),
            ),
            ("c4", "shell", json!({ "command": format!("echo {key}") })),
        ] {
            calls.content.push(Block::ToolCall {
                id: id.into(),
                name: name.into(),
                input,
                signature: None,
            });
        }
        store.append(EntryKind::Message { message: calls });
        let result = |id: &str, text: &str| Block::ToolResult {
            call_id: id.into(),
            content: vec![Block::text(text)],
            is_error: false,
        };
        store.append(EntryKind::Message {
            message: Message::tool(vec![
                result("c1", "1\tfn a() { let token = 42; }"),
                result("c2", "wrote b.txt"),
                result("c3", "@@ -1 +1 @@\n-fn a()\n+fn private_name()"),
                result("c4", &format!("{key}\nexit code: 0")),
            ]),
        });
        store.append(EntryKind::Message {
            message: assistant("All done."),
        });

        let clean = sanitized(&store);
        for format in [Format::Markdown, Format::Html, Format::Jsonl] {
            let text = render(&clean, format, true);
            for held in ["let token = 42", "secret_sauce", "private_name", key] {
                assert!(!text.contains(held), "{format:?} still holds `{held}`:\n{text}");
            }
            assert!(text.contains("All done."), "{text}");
        }
        let lines = render(&clean, Format::Jsonl, true);
        assert!(lines.contains(REMOVED), "{lines}");
        assert!(lines.contains("[redacted:anthropic-key]"), "{lines}");
        // What was shared opens as a session again, and the original is untouched.
        let (_, entries, skipped) = azazel_core::session::parse_lines(&lines);
        assert_eq!((entries.len(), skipped), (store.branch(None).len(), 0));
        assert!(render(&store, Format::Jsonl, true).contains("secret_sauce"));
    }
}
