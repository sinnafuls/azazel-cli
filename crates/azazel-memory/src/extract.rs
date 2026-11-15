//! Automatic extraction: one model call that turns a conversation into durable memories.

use std::collections::HashSet;

use azazel_agent::compaction::render_span;
use azazel_ai::{Message, ModelSpec, Request, Role, Router, StreamEvent, SystemPart, ThinkingLevel};
use azazel_core::SessionStore;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::store::{MemoryRow, MemoryStore, Scope, checked, normalized};

/// The role whose chain answers extraction requests.
const ROLE: &str = "memory";
/// The conversation span sent: 30k tokens at four characters each, newest kept.
const SPAN_CHARS: usize = 120_000;
/// How much of the end of the span is used to look up related memories.
const QUERY_CHARS: usize = 2000;
const EXISTING: usize = 20;
const MAX_ITEMS: usize = 8;
/// Cosine similarity from which a new memory counts as one already stored.
const DUPLICATE: f32 = 0.92;

const INSTRUCTIONS: &str = "You maintain the long-term memory of a coding agent. From the conversation you \
are given, pick what will still be worth knowing in later sessions on this project: facts about the project, \
decisions and their reasons, preferences of the user, conventions of the codebase, and gotchas that cost \
time.\n\
Leave out what only mattered for the task at hand, what the existing memories already say, and every secret \
or credential (API keys, tokens, passwords, private keys), even when one appears in the conversation.\n\
Reply with a JSON array and nothing else, holding at most 8 items of the form \
{\"kind\": \"fact\" | \"decision\" | \"preference\" | \"convention\" | \"gotcha\", \"text\": \"one \
self-contained sentence\", \"tags\": [\"short\", \"keywords\"], \"replaces\": \"id of an existing memory \
that this one corrects\"}. Leave `replaces` out unless an existing memory is now wrong. Reply with [] when \
nothing qualifies.";

/// What the model proposed to remember.
#[derive(Debug, PartialEq)]
struct Item {
    kind: String,
    text: String,
    tags: Vec<String>,
    replaces: Option<String>,
}

/// The items of a reply, which must hold a JSON array; text around the array (a code fence) is
/// tolerated. Entries without a `kind` and `text` are dropped, as is everything past the eighth.
fn parse(reply: &str) -> Result<Vec<Item>, String> {
    let array = reply
        .find('[')
        .zip(reply.rfind(']'))
        .filter(|(start, end)| start < end)
        .map(|(start, end)| &reply[start..=end])
        .ok_or("the reply holds no JSON array")?;
    let values: Vec<Value> =
        serde_json::from_str(array).map_err(|e| format!("the reply is not a JSON array: {e}"))?;
    let text = |value: &Value, field: &str| value.get(field).and_then(Value::as_str).map(str::to_string);
    Ok(values
        .iter()
        .filter_map(|value| {
            let tags = match value.get("tags") {
                Some(Value::Array(tags)) => tags
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_string)
                    .collect(),
                Some(Value::String(tags)) => tags.split(',').map(|tag| tag.trim().to_string()).collect(),
                _ => Vec::new(),
            };
            Some(Item {
                kind: text(value, "kind")?,
                text: text(value, "text")?,
                tags,
                replaces: text(value, "replaces").filter(|id| !id.trim().is_empty()),
            })
        })
        .take(MAX_ITEMS)
        .collect())
}

fn describe(memory: &MemoryRow) -> String {
    format!("- {} [{}] {}\n", memory.id, memory.kind, memory.text)
}

/// The existing memories most related to the conversation, topped up with the newest ones.
async fn related(store: &MemoryStore, span: &str) -> Result<Vec<MemoryRow>, String> {
    let tail = match span.char_indices().rev().nth(QUERY_CHARS) {
        Some((start, _)) => &span[start..],
        None => span,
    };
    let mut rows = store.search(tail, EXISTING, None).await?;
    for scope in [Scope::Project, Scope::Global] {
        for row in store.list(scope).await? {
            if rows.len() >= EXISTING {
                return Ok(rows);
            }
            if !rows.iter().any(|known| known.id == row.id) {
                rows.push(row);
            }
        }
    }
    Ok(rows)
}

async fn complete(router: &Router, request: &Request) -> Result<String, String> {
    let chain = [ModelSpec::Role(ROLE.to_string())];
    let (_, mut stream) = router
        .stream(&chain, request, CancellationToken::new())
        .await
        .map_err(|e| e.to_string())?;
    loop {
        match std::future::poll_fn(|cx| stream.as_mut().poll_next(cx)).await {
            Some(StreamEvent::Done { message }) => return Ok(message.text()),
            Some(StreamEvent::Error { error, .. }) => return Err(error.to_string()),
            Some(_) => {}
            None => return Err("the model returned nothing".to_string()),
        }
    }
}

/// Asks the `memory` role what in `session`'s conversation is worth keeping and stores the answers
/// that are new, in the project store. Returns how many were stored. Nothing is asked when the
/// conversation has no user message or no model is configured for the role.
pub(crate) async fn extract(
    store: &MemoryStore,
    router: &Router,
    session: &SessionStore,
) -> Result<usize, String> {
    let context = session.build_context(None);
    if !context
        .iter()
        .any(|entry| !entry.summary && entry.message.role == Role::User)
    {
        return Ok(0);
    }
    if router.resolve(&[ModelSpec::Role(ROLE.to_string())]).is_empty() {
        tracing::debug!("no model is configured for @{ROLE}; nothing is extracted");
        return Ok(0);
    }
    let span = render_span(&context, SPAN_CHARS);
    let existing = related(store, &span).await?;
    let mut prompt = String::from("# Existing memories\n");
    if existing.is_empty() {
        prompt.push_str("(none)\n");
    }
    for memory in &existing {
        prompt.push_str(&describe(memory));
    }
    prompt.push_str("\n# Conversation\n");
    prompt.push_str(&span);
    let request = Request {
        system: vec![SystemPart {
            text: INSTRUCTIONS.to_string(),
            name: String::new(),
            cache_boundary: false,
        }],
        messages: vec![Message::user(prompt)],
        thinking: ThinkingLevel::Off,
        ..Request::default()
    };
    let items = parse(&complete(router, &request).await?)?;

    let mut known: HashSet<String> = HashSet::new();
    for scope in [Scope::Project, Scope::Global] {
        known.extend(store.list(scope).await?.iter().map(|row| normalized(&row.text)));
    }
    let session_id = session.id();
    let mut stored = 0;
    for item in items {
        let Ok(text) = checked(&item.text, &item.kind) else {
            continue;
        };
        if !known.insert(normalized(text)) {
            continue;
        }
        let vector = store.embed_one(text).await;
        if let Some(vector) = &vector
            && store
                .closest(vector)
                .await?
                .is_some_and(|similarity| similarity >= DUPLICATE)
        {
            continue;
        }
        let id = store
            .insert(
                text,
                &item.kind,
                &item.tags,
                Scope::Project,
                Some(&session_id),
                vector,
            )
            .await?;
        stored += 1;
        if let Some(old) = &item.replaces {
            store.supersede(old.trim(), &id).await?;
        }
    }
    Ok(stored)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replies_are_read_leniently_but_must_hold_an_array() {
        let reply = concat!(
            "```json\n",
            "[{\"kind\": \"fact\", \"text\": \"CI runs on Linux\", \"tags\": [\"ci\", 3]},\n",
            " {\"kind\": \"gotcha\", \"text\": \"x\", \"tags\": \"a, b\", \"replaces\": \"abc\"},\n",
            " {\"kind\": \"fact\"}, \"stray\"]\n",
            "```"
        );
        assert_eq!(
            parse(reply).unwrap(),
            [
                Item {
                    kind: "fact".into(),
                    text: "CI runs on Linux".into(),
                    tags: vec!["ci".into()],
                    replaces: None,
                },
                Item {
                    kind: "gotcha".into(),
                    text: "x".into(),
                    tags: vec!["a".into(), "b".into()],
                    replaces: Some("abc".into()),
                },
            ]
        );
        assert!(parse("[]").unwrap().is_empty());
        assert!(parse("Nothing worth keeping.").is_err());
        assert!(parse("[not json]").is_err());
    }
}
