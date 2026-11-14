#![allow(clippy::unwrap_used)]

//! `session_recall` over a session whose context was elided and compacted.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{AgentInfo, Services, Tool, ToolCtx, ToolOutput, ToolSource, TranscriptSink, UpdateSink};
use azazel_ai::{AssistantMeta, Block, Message, StopReason, Usage, WireApi};
use azazel_core::config::ReadGuard;
use azazel_core::process::Shell;
use azazel_core::{ExtCtx, Registry, SessionStore};
use azazel_tools::CoreToolsExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

struct Bench {
    dir: tempfile::TempDir,
    registry: Registry,
    session: Arc<SessionStore>,
}

fn bench() -> Bench {
    let dir = tempfile::tempdir().unwrap();
    let registry = Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: dir.path().into(),
        session_id: "s".into(),
    });
    registry
        .load(&CoreToolsExt {
            shell: Shell::resolve(""),
            shell_timeout: Duration::from_secs(30),
            artifacts_dir: dir.path().join(".artifacts"),
            read_guard: ReadGuard::default(),
            patch: Default::default(),
        })
        .unwrap();
    let session = Arc::new(SessionStore::create(&dir.path().join("sessions"), dir.path(), None).unwrap());
    registry.services.insert(session.clone());
    Bench {
        dir,
        registry,
        session,
    }
}

fn assistant(content: Vec<Block>) -> Message {
    Message::assistant(
        content,
        AssistantMeta {
            provider: "mock".into(),
            model: "lead".into(),
            api: WireApi::OpenAiChat,
            usage: Usage::default(),
            stop: StopReason::Stop,
            response_id: None,
            duration_ms: 0,
            sources: Vec::new(),
        },
    )
}

fn call(id: &str, name: &str, input: Value) -> Block {
    Block::ToolCall {
        id: id.into(),
        name: name.into(),
        input,
        signature: None,
    }
}

impl Bench {
    fn ctx(&self, agent: &str, depth: u32) -> ToolCtx {
        let info = AgentInfo {
            id: agent.into(),
            def: "lead".into(),
            parent: (depth > 0).then(|| "lead".to_string()),
            depth,
            session_id: self.session.id(),
        };
        ToolCtx {
            agent_id: info.id.clone(),
            session_id: info.session_id.clone(),
            agent: info,
            call_id: "c".into(),
            cwd: self.dir.path().to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.registry.services.clone(),
        }
    }

    async fn recall_as(&self, agent: &str, depth: u32, input: Value) -> ToolOutput {
        let ctx = self.ctx(agent, depth);
        let tool: Arc<dyn Tool> = self.registry.tools.get(&ctx.agent, "session_recall").unwrap();
        tool.call(input, &ctx).await
    }

    async fn recall(&self, input: Value) -> String {
        let output = self.recall_as("lead", 0, input).await;
        assert!(!output.is_error, "{}", output.text_content());
        output.text_content()
    }

    async fn refused(&self, input: Value) -> String {
        let output = self.recall_as("lead", 0, input).await;
        assert!(output.is_error, "{}", output.text_content());
        output.text_content()
    }
}

/// What the conversation below holds, by the ids its entries got.
struct Ids {
    prompt: String,
    read_call: String,
    read_result: String,
    edit_call: String,
    said: String,
    second_prompt: String,
    compaction: String,
    after: String,
}

const PARSER: &str = "fn parse(tokens: &[Token]) -> Ast {\n    // zebra-stripe marker: the old recursive descent\n    descend(tokens, 0)\n}\n";

/// A conversation that read and edited a file, had the read elided and its first half
/// summarised, and went on.
fn compacted(store: &SessionStore) -> Ids {
    let prompt = store.message(&Message::user("port the parser to the new tokenizer"));
    let read_call = store.message(&assistant(vec![
        Block::text("Reading the parser first."),
        call("c1", "read", json!({"path": "src/parser.rs"})),
    ]));
    let read_result = store.message(&Message::tool(vec![Block::tool_result("c1", PARSER, false)]));
    let edit_call = store.message(&assistant(vec![call(
        "c2",
        "edit",
        json!({"path": "src/parser.rs", "edits": [{"old": "descend(tokens, 0)", "new": "walk(tokens)"}]}),
    )]));
    store.message(&Message::tool(vec![Block::tool_result(
        "c2",
        "edited src/parser.rs",
        false,
    )]));
    store.message(&assistant(vec![call(
        "c3",
        "read",
        json!({"path": "src/lexer.rs"}),
    )]));
    store.message(&Message::tool(vec![Block::tool_result(
        "c3",
        "src/lexer.rs: no such file",
        true,
    )]));
    let said = store.message(&assistant(vec![Block::text(
        "The tokenizer handles unicode identifiers now.",
    )]));
    let second_prompt = store.message(&Message::user("now add tests for the tokenizer"));
    store.message(&assistant(vec![Block::text("Adding tokenizer tests.")]));
    store.context_edit(
        &read_result,
        &format!("[result elided: 110 bytes; session_recall expand={read_result}]"),
    );
    let compaction = store.compaction(
        "## Goal\nPort the parser.\n## Files read\nsrc/parser.rs",
        &second_prompt,
        4000,
    );
    let after = store.message(&Message::user("what did the parser look like before?"));
    Ids {
        prompt,
        read_call,
        read_result,
        edit_call,
        said,
        second_prompt,
        compaction,
        after,
    }
}

fn context_text(store: &SessionStore) -> String {
    store
        .build_context(None)
        .iter()
        .map(|entry| serde_json::to_string(&entry.message).unwrap())
        .collect()
}

#[tokio::test]
async fn a_search_finds_what_compaction_and_eliding_took_out_of_the_context() {
    let bench = bench();
    let ids = compacted(&bench.session);
    // The agent's context no longer holds the file it read.
    assert!(!context_text(&bench.session).contains("zebra-stripe"));

    let found = bench.recall(json!({"query": "zebra-stripe"})).await;
    assert_eq!(
        found,
        format!(
            "1 matching entry:\n{}  result of read src/parser.rs: // zebra-stripe marker: the old recursive \
             descent\nRead an entry in full with expand=[\"<id>\"].",
            ids.read_result
        )
    );

    // The entry comes back as it was recorded, not as the elision left it.
    let full = bench.recall(json!({"expand": [ids.read_result]})).await;
    let (header, body) = full.split_once('\n').unwrap();
    assert!(
        header.starts_with(&format!(
            "── {} · result of read src/parser.rs · ",
            ids.read_result
        )),
        "{header}"
    );
    assert_eq!(body, PARSER.trim_end());
}

#[tokio::test]
async fn words_rank_entries_by_how_many_they_hold_then_rarity_and_ties_go_to_the_newest() {
    let bench = bench();
    let ids = compacted(&bench.session);
    // "tokenizer" is in four entries, "unicode" in one: the entry with both leads, the rest of
    // the tokenizer entries follow newest first.
    let found = bench.recall(json!({"query": "Tokenizer unicode"})).await;
    let order: Vec<&str> = found
        .lines()
        .skip(1)
        .filter_map(|line| line.split_once("  ").map(|(id, _)| id))
        .collect();
    assert_eq!(order.len(), 4, "{found}");
    assert_eq!(order[0], ids.said);
    assert_eq!(order[3], ids.prompt);
    assert_eq!(order[2], ids.second_prompt);

    let limited = bench.recall(json!({"query": "tokenizer", "limit": 2})).await;
    assert!(
        limited.starts_with("2 of 4 matching entries, best first:\n"),
        "{limited}"
    );
    assert_eq!(limited.lines().count(), 4);

    assert_eq!(
        bench.recall(json!({"query": "quaternion"})).await,
        "no entry of this conversation matches"
    );
}

#[tokio::test]
async fn a_regex_finds_entries_in_the_order_of_the_conversation() {
    let bench = bench();
    let ids = compacted(&bench.session);
    let found = bench.recall(json!({"regex": r"fn \w+\(|## Goal"})).await;
    let hits: Vec<&str> = found.lines().skip(1).take(2).collect();
    assert_eq!(
        hits,
        [
            format!(
                "{}  result of read src/parser.rs: fn parse(tokens: &[Token]) -> Ast {{",
                ids.read_result
            ),
            format!("{}  summary: ## Goal", ids.compaction)
        ]
    );
    assert!(found.starts_with("2 matching entries, oldest first:\n"));

    let refused = bench.refused(json!({"regex": "fn ("})).await;
    assert!(
        refused.starts_with("`regex` is not a pattern this tool can use:"),
        "{refused}"
    );
}

#[tokio::test]
async fn a_range_returns_entries_in_full_and_says_how_to_go_on() {
    let bench = bench();
    let ids = compacted(&bench.session);
    let span = bench
        .recall(json!({"range": [ids.read_call, ids.edit_call]}))
        .await;
    let headers: Vec<&str> = span.lines().filter(|line| line.starts_with("── ")).collect();
    assert_eq!(headers.len(), 3, "{span}");
    assert!(headers[0].starts_with(&format!("── {} · assistant · ", ids.read_call)));
    assert!(headers[1].starts_with(&format!(
        "── {} · result of read src/parser.rs · ",
        ids.read_result
    )));
    assert!(headers[2].starts_with(&format!("── {} · assistant · ", ids.edit_call)));
    assert!(span.contains("Reading the parser first.\n→ read {\"path\":\"src/parser.rs\"}"));
    assert!(span.contains("zebra-stripe marker"));
    assert!(span.contains("\"new\":\"walk(tokens)\""));

    // An empty id is an end of the conversation; the limit cuts and names where to continue.
    let cut = bench.recall(json!({"range": ["", ids.said], "limit": 2})).await;
    assert_eq!(cut.lines().filter(|line| line.starts_with("── ")).count(), 2);
    let trailer = cut.lines().last().unwrap();
    assert_eq!(
        trailer,
        format!(
            "[6 more entries; continue with range=[\"{}\", \"{}\"]]",
            ids.read_result, ids.said
        )
    );
    let tail = bench.recall(json!({"range": [ids.compaction, ""]})).await;
    assert!(tail.contains("· summary ·") && tail.contains("what did the parser look like before?"));
    assert!(tail.contains(&ids.after));

    assert_eq!(
        bench.refused(json!({"range": [ids.said, ids.prompt]})).await,
        format!("`range` runs backwards: {} comes after {}", ids.said, ids.prompt)
    );
    assert_eq!(
        bench.refused(json!({"range": ["nope", ""]})).await,
        "no entry `nope` holds a message on this branch of the conversation"
    );
    assert!(
        bench
            .refused(json!({"range": [ids.prompt]}))
            .await
            .starts_with("`range` takes two")
    );
    assert!(
        bench
            .refused(json!({}))
            .await
            .starts_with("give `query` or `regex`")
    );
}

#[tokio::test]
async fn touched_lists_files_with_the_entries_that_read_and_wrote_them() {
    let bench = bench();
    let ids = compacted(&bench.session);
    let touched = bench.recall(json!({"touched": true})).await;
    // The failed read of src/lexer.rs touched nothing.
    assert!(
        touched.starts_with(&format!(
            "read:\n  src/parser.rs  {}\n\nwritten:\n  src/parser.rs  {}\n",
            ids.read_result, ids.edit_call
        )),
        "{touched}"
    );
    assert!(!touched.contains("lexer"));

    // An unknown id is said beside the entries that were found; ids may come in brackets.
    let mixed = bench
        .recall(json!({"expand": [format!("[{}]", ids.said), "0000"]}))
        .await;
    assert!(mixed.contains("The tokenizer handles unicode identifiers now."));
    assert!(mixed.ends_with("no entry `0000` holds a message on this branch of the conversation"));

    let empty = self::bench();
    assert_eq!(
        empty.recall(json!({"touched": true})).await,
        "no file was read or written with `read`, `write` or `edit` in this conversation"
    );
}

#[tokio::test]
async fn a_search_does_not_find_what_earlier_searches_returned() {
    let bench = bench();
    let ids = compacted(&bench.session);
    let asked = bench.session.message(&assistant(vec![call(
        "c9",
        "session_recall",
        json!({"query": "zebra-stripe"}),
    )]));
    let answered = bench.session.message(&Message::tool(vec![Block::tool_result(
        "c9",
        "1 matching entry:\nabc  result of read src/parser.rs: // zebra-stripe marker",
        false,
    )]));
    let found = bench.recall(json!({"query": "zebra-stripe"})).await;
    assert!(found.starts_with("1 matching entry:\n"), "{found}");
    assert!(found.contains(&ids.read_result));
    assert!(!found.contains(&asked) && !found.contains(&answered));
    // They are still part of the conversation and can be read.
    let full = bench.recall(json!({"expand": [answered]})).await;
    assert!(full.contains("result of session_recall zebra-stripe"));
}

#[tokio::test]
async fn only_the_branch_the_conversation_is_on_is_searched() {
    let bench = bench();
    let ids = compacted(&bench.session);
    // Go back to before the second prompt and take another way from there.
    assert!(bench.session.set_leaf(Some(&ids.said)));
    bench
        .session
        .message(&Message::user("leave the tokenizer; document the parser"));
    let found = bench.recall(json!({"query": "tests"})).await;
    assert_eq!(found, "no entry of this conversation matches");
    assert_eq!(
        bench.refused(json!({"range": [ids.after, ""]})).await,
        format!(
            "no entry `{}` holds a message on this branch of the conversation",
            ids.after
        )
    );
    assert!(
        bench
            .recall(json!({"query": "document"}))
            .await
            .starts_with("1 matching entry:")
    );
}

#[tokio::test]
async fn a_sub_agent_recalls_its_own_transcript() {
    let bench = bench();
    compacted(&bench.session);
    let path = bench.session.agent_transcript("scout-1");
    assert!(
        path.ends_with(
            Path::new(&bench.session.id())
                .join("agents")
                .join("scout-1.jsonl")
        )
    );
    let own = SessionStore::create_at(&path, "scout-1", bench.dir.path(), None).unwrap();
    let asked = own.message(&Message::user("map the heron module"));

    let output = bench
        .recall_as("scout-1", 1, json!({"query": "heron parser"}))
        .await;
    assert!(!output.is_error);
    let found = output.text_content();
    assert!(found.starts_with("1 matching entry:\n"), "{found}");
    assert!(found.contains(&format!("{asked}  user: map the heron module")));

    // An agent without a transcript is told so.
    let missing = bench.recall_as("ghost", 1, json!({"query": "heron"})).await;
    assert!(missing.is_error);
    assert!(
        missing.text_content().contains("ghost.jsonl"),
        "{}",
        missing.text_content()
    );
}

#[tokio::test]
async fn the_tool_is_offered_with_the_core_tools_and_only_reads() {
    let bench = bench();
    let lead = bench.ctx("lead", 0).agent;
    let tool = bench
        .registry
        .tools
        .tools(&lead)
        .into_iter()
        .find(|tool| tool.name() == "session_recall")
        .expect("session_recall is registered");
    let input = json!({"query": "x"});
    assert_eq!(tool.access(&input), azazel_agent::Access::Read);
    assert!(tool.concurrency_safe(&input));
    assert!(tool.subjects(&input).is_empty());

    // A call leaves the session file as it was.
    let ids = compacted(&bench.session);
    let before = std::fs::read(bench.session.path()).unwrap();
    bench
        .recall(json!({"query": "parser", "touched": true, "expand": [ids.prompt], "range": ["", ""]}))
        .await;
    assert_eq!(std::fs::read(bench.session.path()).unwrap(), before);
}
