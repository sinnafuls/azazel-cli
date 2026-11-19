//! Shrinks large tool results once, when they are produced: JSON from MCP tools and the web loses
//! the fields that tell a model nothing (`[tools] slim_results`), and with `[tools] digest = "smol"`
//! a large MCP, web or PDF result is replaced by notes the `smol` role writes about it, with the
//! full text saved where `read` finds it.
//!
//! What this returns is stored in the conversation and repeated by every later request, so a
//! result is decided here once and never rewritten; anything that goes wrong leaves it as it was.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{ResultShaper, ShapeRequest, ToolOutput};
use azazel_ai::{Block, ModelSpec};
use azazel_core::ArtifactStore;
use azazel_core::config::DigestMode;
use azazel_core::names::NAMER_ROLE;
use azazel_core::side::{Side, SideQuestion};
use azazel_core::slim;
use sha2::{Digest, Sha256};

/// How long the `smol` role may take over its notes before the result goes through as it is.
const LIMIT: Duration = Duration::from_secs(90);
/// Results longer than this are not sent to the `smol` role: they would not fit its context.
const MAX_DIGEST_CHARS: usize = 400_000;
/// The longest notes kept, in words.
const MAX_WORDS: usize = 1_500;
/// The reply limit of the request, in tokens: room for [`MAX_WORDS`] words.
const MAX_OUTPUT: u32 = 2_600;
/// Bumped when the request changes, so notes written for an older one are not reused.
const CACHE_VERSION: &str = "1";

/// The settings of `[tools]` this needs.
pub struct DigestSettings {
    pub slim: bool,
    pub mode: DigestMode,
    pub min_chars: usize,
    pub min_pages: usize,
    /// Where notes are kept by the hash of what they are about.
    pub cache_dir: PathBuf,
}

/// The [`ResultShaper`] of the application.
pub struct Digester(pub DigestSettings);

/// What kind of result a call gives, for what may be done to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Source {
    /// An MCP tool: slimmed, and digested above `min_chars`.
    Mcp,
    /// A web tool: slimmed; `web_fetch` is digested above `min_chars`.
    Web { digest: bool },
    /// A `read` of a PDF: digested above `min_pages` pages.
    Pdf,
}

fn source(name: &str, input: &serde_json::Value) -> Option<Source> {
    if name.starts_with("mcp__") {
        return Some(Source::Mcp);
    }
    match name {
        // A page the model asked for line by line is what it wants to see.
        "web_fetch" => Some(Source::Web {
            digest: input.get("offset").is_none_or(serde_json::Value::is_null),
        }),
        "web_search" | "http" => Some(Source::Web { digest: false }),
        // A PDF is never a file the agent edits; any other read (code, notes, configuration,
        // instructions) is shown as it is.
        "read" => {
            let path = input.get("path")?.as_str()?.trim();
            let pdf = !path.contains("://") && path.to_ascii_lowercase().ends_with(".pdf");
            pdf.then_some(Source::Pdf)
        }
        _ => None,
    }
}

#[async_trait]
impl ResultShaper for Digester {
    async fn shape(&self, request: ShapeRequest<'_>, output: &mut ToolOutput) {
        let Some(source) = source(request.name, request.input) else {
            return;
        };
        // What slimming dropped is saved whole, and the result says so: a field that looked like
        // noise may be what the request was about.
        let mut notes = Vec::new();
        if self.0.slim
            && source != Source::Pdf
            && let Some(store) = request.services.get::<ArtifactStore>()
        {
            for block in &mut output.content {
                if let Block::Text { text } = block
                    && let Some(slimmed) = slim::slim_text(text)
                {
                    if slimmed.dropped > 0 {
                        let name = if notes.is_empty() {
                            format!("slim-{}", request.call_id)
                        } else {
                            format!("slim-{}-{}", request.call_id, notes.len() + 1)
                        };
                        // Without the original saved, nothing is dropped.
                        if store.save(&format!("{name}.txt"), text.as_bytes()).is_err() {
                            continue;
                        }
                        notes.push(slim_note(slimmed.dropped, &name));
                    }
                    *text = slimmed.text;
                }
            }
        }
        if self.0.mode == DigestMode::Off
            || output
                .content
                .iter()
                .any(|block| !matches!(block, Block::Text { .. }))
        {
            output.content.extend(notes.into_iter().map(Block::text));
            return;
        }
        let text = output.text_content();
        let large = match source {
            Source::Mcp | Source::Web { digest: true } => text.len() > self.0.min_chars,
            Source::Pdf => pages(&text) > self.0.min_pages,
            Source::Web { digest: false } => false,
        };
        if large
            && text.len() <= MAX_DIGEST_CHARS
            && let Some(mut digest) = self.digest(&request, &text).await
        {
            for note in &notes {
                digest.push_str(
                    "

",
                );
                digest.push_str(note);
            }
            output.content = vec![Block::text(digest)];
            return;
        }
        output.content.extend(notes.into_iter().map(Block::text));
    }
}

/// The line that tells the model slimming removed fields, and where the whole result is.
fn slim_note(dropped: usize, name: &str) -> String {
    format!(
        "[{dropped} fields or items of little use (empty values, ids, and for GitHub or Linear results          colours, avatars and API links) were removed from this JSON; the full text is artifact://{name}]"
    )
}

/// The pages a PDF read shows: one `--- page N ---` line each.
fn pages(text: &str) -> usize {
    text.lines()
        .filter(|line| line.starts_with("--- page ") && line.ends_with(" ---"))
        .count()
}

impl Digester {
    /// The notes that stand in for `text`, with where the full text is; `None` when they cannot
    /// be had.
    async fn digest(&self, request: &ShapeRequest<'_>, text: &str) -> Option<String> {
        // JSON is saved one field to a line, so the notes can point into it by line.
        let full = slim::pretty(text).unwrap_or_else(|| text.to_string());
        let store = request.services.get::<ArtifactStore>()?;
        let ask = request.last_user.unwrap_or("").trim();
        let key = cache_key(&full, ask);
        let cached = self.0.cache_dir.join(format!("{key}.md"));
        let notes = match std::fs::read_to_string(&cached) {
            Ok(notes) if !notes.trim().is_empty() => notes,
            _ => {
                let notes = self.ask(request, &full, ask).await?;
                if let Err(e) =
                    std::fs::create_dir_all(&self.0.cache_dir).and_then(|()| std::fs::write(&cached, &notes))
                {
                    tracing::debug!("the digest was not cached: {e}");
                }
                notes
            }
        };
        let name = format!("digest-{}", request.call_id);
        store.save(&format!("{name}.txt"), full.as_bytes()).ok()?;
        let lines = full.lines().count();
        Some(format!(
            "[notes on this {}-line result, written by a smaller model for the request at hand; the full \
             text is artifact://{name}: read it, with offset and limit for the lines the notes cite, when \
             you need more than they say]\n\n{}",
            lines,
            notes.trim()
        ))
    }

    async fn ask(&self, request: &ShapeRequest<'_>, full: &str, ask: &str) -> Option<String> {
        let side = request.services.get::<Side>()?;
        let mut numbered = String::with_capacity(full.len() + full.len() / 8);
        for (n, line) in full.lines().enumerate() {
            numbered.push_str(&format!("{}\t{line}\n", n + 1));
        }
        let question = format!(
            "An agent called the tool `{tool}` and got the result below, too long to keep in its \
             context. Write notes that let it go on with the request without the result: the facts, \
             names, numbers, identifiers, links and errors that bear on the request, quoted exactly; \
             what the result covers and what it does not. Cite the lines each point comes from as \
             (L12-L30). At most {MAX_WORDS} words, no preamble.\n\nThe request:\n{request_text}\n\n\
             The result, with its line numbers:\n{numbered}",
            tool = request.name,
            request_text = if ask.is_empty() { "(not known)" } else { ask },
        );
        let mut ignore = |_: &str| {};
        let asked = side.ask_capped(
            SideQuestion {
                agent: request.agent,
                context: Vec::new(),
                question,
                chain: vec![ModelSpec::Role(NAMER_ROLE.to_string())],
                purpose: "digest",
                framed: false,
            },
            Some(MAX_OUTPUT),
            &mut ignore,
            request.cancel.clone(),
        );
        let answer = match tokio::time::timeout(LIMIT, asked).await {
            Ok(Ok(answer)) => answer,
            Ok(Err(why)) => {
                tracing::debug!("the result of {} was not digested: {why}", request.call_id);
                return None;
            }
            Err(_) => {
                tracing::debug!(
                    "the result of {} was not digested: it took too long",
                    request.call_id
                );
                return None;
            }
        };
        let notes = cap_words(answer.text.trim(), MAX_WORDS);
        // Notes no shorter than the result save nothing.
        (!notes.is_empty() && notes.len() < full.len() / 2).then_some(notes)
    }
}

fn cache_key(full: &str, ask: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(CACHE_VERSION.as_bytes());
    hasher.update([0]);
    hasher.update(ask.as_bytes());
    hasher.update([0]);
    hasher.update(full.as_bytes());
    hasher
        .finalize()
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// `text` cut after `max` words, keeping its lines.
fn cap_words(text: &str, max: usize) -> String {
    let mut words = 0;
    let mut out = String::new();
    for line in text.lines() {
        let count = line.split_whitespace().count();
        if words + count > max {
            let rest: Vec<&str> = line.split_whitespace().take(max - words).collect();
            out.push_str(&rest.join(" "));
            out.push_str(" …");
            return out;
        }
        words += count;
        out.push_str(line);
        out.push('\n');
    }
    out.trim_end().to_string()
}

/// The shaper for `config`, or `None` when it would do nothing.
pub fn digester(config: &azazel_core::Config, cache_dir: PathBuf) -> Option<Arc<Digester>> {
    let tools = &config.tools;
    (tools.slim_results || tools.digest != DigestMode::Off).then(|| {
        Arc::new(Digester(DigestSettings {
            slim: tools.slim_results,
            mode: tools.digest,
            min_chars: tools.digest_min_chars,
            min_pages: tools.digest_min_pages,
            cache_dir,
        }))
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn only_mcp_web_and_pdf_results_are_touched() {
        assert_eq!(source("mcp__github__list_issues", &json!({})), Some(Source::Mcp));
        assert_eq!(
            source("web_fetch", &json!({ "url": "https://x" })),
            Some(Source::Web { digest: true })
        );
        assert_eq!(
            source("web_fetch", &json!({ "url": "https://x", "offset": 200 })),
            Some(Source::Web { digest: false })
        );
        assert_eq!(
            source("read", &json!({ "path": "docs/Spec.PDF" })),
            Some(Source::Pdf)
        );
        for (name, input) in [
            ("read", json!({ "path": "src/main.rs" })),
            ("read", json!({ "path": "AGENTS.md" })),
            ("read", json!({ "path": "artifact://digest-c1" })),
            ("shell", json!({ "command": "cargo test" })),
            ("grep", json!({ "pattern": "x" })),
        ] {
            assert_eq!(source(name, &input), None, "{name} {input}");
        }
    }

    #[test]
    fn pages_count_the_page_lines_of_a_pdf_read() {
        assert_eq!(
            pages("--- page 1 ---\na\n\n--- page 2 ---\nb\n[pages 1-2 of 9]"),
            2
        );
        assert_eq!(pages("no pages"), 0);
    }

    #[test]
    fn notes_are_cut_at_the_word_limit() {
        let text = "one two three\nfour five\nsix";
        assert_eq!(cap_words(text, 10), text);
        assert_eq!(cap_words(text, 4), "one two three\nfour …");
    }

    #[test]
    fn the_cache_key_depends_on_the_text_and_the_request() {
        assert_eq!(cache_key("a", "q"), cache_key("a", "q"));
        assert_ne!(cache_key("a", "q"), cache_key("b", "q"));
        assert_ne!(cache_key("a", "q"), cache_key("a", "r"));
        assert_ne!(cache_key("ab", ""), cache_key("a", "b"));
    }
}
