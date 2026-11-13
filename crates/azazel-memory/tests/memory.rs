#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_agent::{AgentInfo, Services, ToolCtx, ToolOutput, UpdateSink};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{
    Catalog, Credentials, Message, ModelSpec, Provider, ProviderSettings, RetryPolicy, Router, WireApi,
};
use azazel_core::config::EmbeddingsConfig;
use azazel_core::{
    CommandOutcome, Config, EntryKind, Event, ExtCtx, Paths, Registry, SessionStore, StartSource,
};
use azazel_memory::{MemoryExt, MemoryRow, MemoryStore, Scope};
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A temporary azazel home and a project beside it.
struct Home {
    dir: tempfile::TempDir,
    paths: Paths,
}

fn home() -> Home {
    let dir = tempfile::tempdir().unwrap();
    let project = dir.path().join("project");
    std::fs::create_dir_all(project.join(".azazel")).unwrap();
    let paths = Paths::discover(&project, dir.path().join("home").to_str());
    Home { dir, paths }
}

/// Memory with nothing happening in the background.
fn quiet() -> Config {
    let mut config = Config::default();
    config.memory.auto_index = false;
    config.memory.auto_extract = false;
    config
}

fn open(home: &Home, config: &Config) -> MemoryExt {
    let credentials = Credentials::with_env(home.paths.auth_file(), Arc::new(|_: &str| None::<String>));
    MemoryExt::open(&home.paths, config, Arc::new(credentials)).unwrap()
}

fn registry(home: &Home, ext: &MemoryExt, session: &str) -> Arc<Registry> {
    let registry = Arc::new(Registry::new(ExtCtx {
        services: Arc::new(Services::new()),
        cwd: home.paths.cwd.clone(),
        session_id: session.to_string(),
    }));
    registry.load(ext).unwrap();
    registry
}

fn lead(session: &str) -> AgentInfo {
    AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: session.into(),
    }
}

/// Calls a registered tool the way the lead agent would.
async fn call(registry: &Registry, tool: &str, input: Value) -> ToolOutput {
    let ctx = registry.bus.ctx();
    let agent = lead(&ctx.session_id);
    let tool = registry.tools.get(&agent, tool).unwrap();
    let ctx = ToolCtx {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        agent,
        call_id: "call".into(),
        cwd: ctx.cwd,
        cancel: CancellationToken::new(),
        update: UpdateSink::default(),
        services: ctx.services,
    };
    tool.call(input, &ctx).await
}

/// Runs `/memory <args>` and returns what it shows, or its error.
async fn slash(registry: &Registry, args: &str) -> Result<String, String> {
    let command = registry.command("memory").unwrap();
    match command.handler.run(args, &registry.bus.ctx()).await {
        CommandOutcome::Show(text) => Ok(text),
        CommandOutcome::Error(why) => Err(why),
        other => panic!("unexpected outcome {other:?}"),
    }
}

/// Submits a prompt to the extensions and returns the text the agent would receive.
async fn prompt(registry: &Registry, text: &str) -> String {
    let mut event = Event::UserPrompt {
        text: text.to_string(),
        images: Vec::new(),
    };
    registry.bus.emit(&mut event).await;
    match event {
        Event::UserPrompt { text, .. } => text,
        other => panic!("unexpected event {other:?}"),
    }
}

fn ids(rows: &[MemoryRow]) -> Vec<&str> {
    rows.iter().map(|row| row.id.as_str()).collect()
}

fn texts(rows: &[MemoryRow]) -> Vec<&str> {
    rows.iter().map(|row| row.text.as_str()).collect()
}

async fn found(store: &MemoryStore, query: &str) -> Vec<String> {
    let rows = store.search(query, 5, None).await.unwrap();
    rows.into_iter().map(|row| row.id).collect()
}

/// Polls `check` until it holds, failing after twenty seconds.
async fn eventually<F, Fut>(what: &str, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + Duration::from_secs(20);
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting until {what}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

#[test]
fn the_bundled_sqlite_has_fts5() {
    let conn = rusqlite::Connection::open_in_memory().unwrap();
    conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(x)")
        .unwrap();
    conn.execute("INSERT INTO t (x) VALUES ('hello world')", [])
        .unwrap();
    let hits: i64 = conn
        .query_row("SELECT COUNT(*) FROM t WHERE t MATCH 'hello'", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(hits, 1);
}

#[tokio::test]
async fn memories_are_saved_listed_found_and_forgotten_per_scope() {
    let home = home();
    let ext = open(&home, &quiet());
    let store = ext.store();
    let style = ["style".to_string()];
    let tabs = store
        .save(
            " Indent with tabs, never spaces ",
            "convention",
            &style,
            Scope::Project,
            Some("s1"),
        )
        .await
        .unwrap();
    let jenkins = store
        .save("Deploys go through Jenkins", "fact", &[], Scope::Project, None)
        .await
        .unwrap();
    let terse = store
        .save(
            "The user prefers short answers",
            "preference",
            &[],
            Scope::Global,
            None,
        )
        .await
        .unwrap();

    // Only the five kinds are accepted, and a memory has to say something.
    let refused = store
        .save("x", "rumour", &[], Scope::Project, None)
        .await
        .unwrap_err();
    assert!(
        refused.contains("`rumour`") && refused.contains("gotcha"),
        "{refused}"
    );
    assert!(store.save("  ", "fact", &[], Scope::Project, None).await.is_err());

    // Newest first, each scope on its own.
    let project = store.list(Scope::Project).await.unwrap();
    assert_eq!(ids(&project), [jenkins.as_str(), tabs.as_str()]);
    assert_eq!(
        project[1],
        MemoryRow {
            id: tabs.clone(),
            kind: "convention".into(),
            text: "Indent with tabs, never spaces".into(),
            tags: vec!["style".into()],
            scope: Scope::Project,
            created_at: project[1].created_at.clone(),
        }
    );
    let global = store.list(Scope::Global).await.unwrap();
    assert_eq!(ids(&global), [terse.as_str()]);
    assert_eq!(global[0].scope, Scope::Global);

    // A search covers both stores unless told otherwise; text and tags both match.
    assert_eq!(
        found(&store, "how are deploys done?").await,
        std::slice::from_ref(&jenkins)
    );
    assert_eq!(found(&store, "short answers").await, std::slice::from_ref(&terse));
    assert_eq!(found(&store, "style").await, std::slice::from_ref(&tabs));
    let narrowed = store.search("short answers", 5, Some(Scope::Project)).await;
    assert!(narrowed.unwrap().is_empty());
    assert_eq!(found(&store, "kubernetes").await, Vec::<String>::new());
    assert_eq!(found(&store, "?!").await, Vec::<String>::new());
    // The better match comes first and the limit cuts the rest.
    store
        .save(
            "Jenkins deploys need the VPN",
            "gotcha",
            &[],
            Scope::Project,
            None,
        )
        .await
        .unwrap();
    let both = store.search("jenkins vpn", 5, None).await.unwrap();
    assert_eq!(
        texts(&both),
        ["Jenkins deploys need the VPN", "Deploys go through Jenkins"]
    );
    assert_eq!(store.search("jenkins vpn", 1, None).await.unwrap().len(), 1);

    // Everything is exported, one JSON object per line, oldest first.
    let export = store.export().await.unwrap();
    let lines: Vec<Value> = export
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["id"], tabs);
    assert_eq!(lines[0]["scope"], "project");
    assert_eq!(lines[0]["tags"], json!(["style"]));
    assert_eq!(lines[0]["source_session"], "s1");
    assert_eq!(lines[0]["superseded_by"], Value::Null);
    assert_eq!(lines[3]["scope"], "global");

    // Forgetting works across scopes and says whether there was anything to forget.
    assert!(store.forget(&jenkins).await.unwrap());
    assert!(!store.forget(&jenkins).await.unwrap());
    assert!(store.forget(&terse).await.unwrap());
    assert_eq!(found(&store, "short answers").await, Vec::<String>::new());
    assert!(store.list(Scope::Global).await.unwrap().is_empty());
    assert_eq!(
        texts(&store.list(Scope::Project).await.unwrap()),
        ["Jenkins deploys need the VPN", "Indent with tabs, never spaces"]
    );
}

const SIGN_IN: &str = "Sign-in goes through the OAuth device flow";
const DEPLOY: &str = "The deploy script lives in tools/release.sh";
const MIGRATIONS: &str = "Database migrations run at startup";

/// The vector the fake embeddings server gives a text: one axis per topic, not normalised.
fn topic_vector(text: &str) -> Vec<f32> {
    let text = text.to_lowercase();
    let about = |words: &[&str]| words.iter().any(|word| text.contains(word));
    if about(&["oauth", "authenticate"]) {
        vec![3.0, 0.3, 0.0]
    } else if about(&["deploy", "release"]) {
        vec![0.0, 2.0, 0.0]
    } else {
        vec![0.0, 0.0, 5.0]
    }
}

#[tokio::test]
async fn vectors_find_what_words_cannot_and_both_rankings_are_fused() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/embeddings"))
        .respond_with(|request: &wiremock::Request| {
            let body: Value = request.body_json().unwrap();
            assert_eq!(body["model"], "embed-test");
            let inputs = body["input"].as_array().unwrap();
            let data: Vec<Value> = (0..inputs.len())
                .map(|index| {
                    let vector = topic_vector(inputs[index].as_str().unwrap());
                    json!({ "index": index, "embedding": vector })
                })
                .collect();
            ResponseTemplate::new(200).set_body_json(json!({ "data": data }))
        })
        .mount(&server)
        .await;

    // Without embeddings the ranking is by words alone.
    let home = home();
    let lexical = open(&home, &quiet());
    let store = lexical.store();
    let mut saved = Vec::new();
    for text in [SIGN_IN, DEPLOY, MIGRATIONS] {
        saved.push(store.save(text, "fact", &[], Scope::Project, None).await.unwrap());
    }
    let (sign_in, deploy, migrations) = (saved[0].clone(), saved[1].clone(), saved[2].clone());
    let question = "how do users authenticate";
    assert_eq!(found(&store, question).await, Vec::<String>::new());
    assert_eq!(
        found(&store, "release script").await,
        std::slice::from_ref(&deploy)
    );
    drop(lexical);

    // The same stores with embeddings configured: the rows saved without vectors get theirs in the
    // background, and then the question finds the memory it shares no word with.
    let mut config = quiet();
    config.providers.insert(
        "embedder".to_string(),
        ProviderSettings {
            api: Some(WireApi::OpenAiChat),
            base_url: Some(server.uri()),
            ..ProviderSettings::default()
        },
    );
    config.embeddings = Some(EmbeddingsConfig {
        provider: "embedder".to_string(),
        model: "embed-test".to_string(),
    });
    let hybrid = open(&home, &config);
    let store = hybrid.store();
    let registry = registry(&home, &hybrid, "s2");
    let start = StartSource::Startup;
    registry
        .bus
        .emit(&mut Event::SessionStart { source: start })
        .await;
    let expected = [sign_in.clone(), deploy.clone(), migrations.clone()];
    eventually("the stored memories have vectors", || async {
        found(&store, question).await == expected
    })
    .await;

    // A memory in both rankings beats the leaders of either one.
    assert_eq!(found(&store, "release script").await[0], deploy);
    // A memory saved now is embedded on the spot.
    let login = store
        .save("Login uses OAuth", "fact", &[], Scope::Project, None)
        .await
        .unwrap();
    let mut top_two = found(&store, question).await;
    top_two.truncate(2);
    top_two.sort();
    let mut closest = [sign_in, login];
    closest.sort();
    assert_eq!(top_two, closest);

    // A provider that cannot embed is refused when the stores are opened.
    config.embeddings = Some(EmbeddingsConfig {
        provider: "anthropic".to_string(),
        model: "embed-test".to_string(),
    });
    let credentials = Credentials::with_env(home.paths.auth_file(), Arc::new(|_: &str| None::<String>));
    let refused = MemoryExt::open(&home.paths, &config, Arc::new(credentials))
        .err()
        .unwrap();
    assert!(refused.contains("no embeddings endpoint"), "{refused}");
}

#[tokio::test]
async fn a_later_session_is_told_what_an_earlier_one_remembered() {
    let home = home();
    let config = quiet();
    {
        let first = open(&home, &config);
        let registry = registry(&home, &first, "s1");
        for input in [
            json!({ "text": "Indent with tabs", "kind": "convention" }),
            json!({ "text": "The deploy script is tools/release.sh", "kind": "fact", "tags": ["deploy"] }),
            json!({ "text": "The user likes terse answers", "kind": "preference", "scope": "global" }),
        ] {
            let saved = call(&registry, "memory_save", input).await;
            assert!(!saved.is_error, "{}", saved.text_content());
        }
        let refused = call(&registry, "memory_save", json!({ "text": "x", "kind": "rumour" })).await;
        assert!(refused.is_error);
    }

    let second = open(&home, &config);
    let registry = registry(&home, &second, "s2");

    // Preferences and conventions are put before the agent with every request; facts are not.
    // They follow the conversation, so that saving one does not change what a provider cached.
    let sent = registry.prompt.prompt(&lead("s2")).await;
    assert!(sent.system.iter().all(|part| !part.text.contains("# Memory")));
    let memory: Vec<&str> = sent
        .tail
        .iter()
        .filter(|part| part.name == "memory")
        .map(|part| part.text.as_str())
        .filter(|text| text.starts_with("# Memory"))
        .collect();
    assert_eq!(memory.len(), 1);
    assert!(
        memory[0].contains("- [convention] Indent with tabs"),
        "{}",
        memory[0]
    );
    assert!(memory[0].contains("- [preference] The user likes terse answers"));
    assert!(!memory[0].contains("deploy script"));

    // A matching fact rides along with the first prompt it matches, and only that one.
    let start = StartSource::Startup;
    registry
        .bus
        .emit(&mut Event::SessionStart { source: start })
        .await;
    let asked = "how does the deploy script work?";
    assert_eq!(
        prompt(&registry, asked).await,
        format!("{asked}\n\n<memory>\n- [fact] The deploy script is tools/release.sh\n</memory>")
    );
    assert_eq!(prompt(&registry, asked).await, asked);
    assert_eq!(prompt(&registry, "what is 2 + 2?").await, "what is 2 + 2?");
    // A new session starts with nothing shown.
    registry
        .bus
        .emit(&mut Event::SessionStart { source: start })
        .await;
    assert!(prompt(&registry, asked).await.ends_with("</memory>"));

    // The tools and the command see the same memories.
    let listed = slash(&registry, "list").await.unwrap();
    assert_eq!(listed.lines().count(), 3, "{listed}");
    let hit = call(&registry, "memory_search", json!({ "query": "deploy" })).await;
    let line = hit.text_content();
    assert!(
        line.ends_with("[fact, project] The deploy script is tools/release.sh #deploy"),
        "{line}"
    );
    let id = line.split(' ').next().unwrap().to_string();
    assert!(listed.contains(&id));
    let none = call(
        &registry,
        "memory_search",
        json!({ "query": "deploy", "scope": "global" }),
    )
    .await;
    assert_eq!(none.text_content(), "no memory matches `deploy`");

    let forgot = call(&registry, "memory_forget", json!({ "id": id })).await;
    assert!(!forgot.is_error);
    assert!(
        call(&registry, "memory_forget", json!({ "id": id }))
            .await
            .is_error
    );
    assert_eq!(slash(&registry, "list").await.unwrap().lines().count(), 2);
    let terse = second.store().list(Scope::Global).await.unwrap()[0].id.clone();
    assert_eq!(
        slash(&registry, &format!("forget {terse}")).await.unwrap(),
        format!("forgot {terse}")
    );
    assert!(slash(&registry, &format!("forget {terse}")).await.is_err());
    assert!(
        slash(&registry, "shred everything")
            .await
            .unwrap_err()
            .starts_with("usage:")
    );
}

#[tokio::test]
async fn the_memory_part_keeps_the_newest_within_its_budget() {
    let home = home();
    let ext = open(&home, &quiet());
    let store = ext.store();
    let registry = registry(&home, &ext, "s1");
    // Nothing remembered, no part.
    assert!(registry.prompt.prompt(&lead("s1")).await.tail.is_empty());

    for n in 0..40 {
        let rule = "wrap lines at one hundred and ten columns ".repeat(5);
        store
            .save(
                &format!("Convention number {n}: {rule}"),
                "convention",
                &[],
                Scope::Project,
                None,
            )
            .await
            .unwrap();
    }
    let notes = registry.prompt.prompt(&lead("s1")).await.tail;
    let part = &notes[0].text;
    // About 1200 tokens: the newest conventions are in, the oldest are not.
    assert!(part.contains("Convention number 39:"));
    assert!(!part.contains("Convention number 0:"));
    assert!((4000..=5000).contains(&part.len()), "{}", part.len());
}

#[tokio::test]
async fn extraction_stores_what_is_new_and_supersedes_what_it_replaces() {
    let home = home();
    let mut config = Config::default();
    config.memory.auto_index = false;
    let ext = open(&home, &config);
    let store = ext.store();
    store
        .save(
            "The project indents with tabs",
            "convention",
            &[],
            Scope::Project,
            None,
        )
        .await
        .unwrap();
    let jenkins = store
        .save("Deploys go through Jenkins", "fact", &[], Scope::Project, None)
        .await
        .unwrap();

    let mock = Arc::new(MockProvider::new());
    let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
    let roles = BTreeMap::from([("memory".to_string(), vec!["mock/m".parse::<ModelSpec>().unwrap()])]);
    let router = Arc::new(Router::new(
        providers,
        Catalog::default(),
        roles,
        RetryPolicy::default(),
    ));
    let session = Arc::new(SessionStore::create(&home.paths.sessions_dir(), &home.paths.cwd, None).unwrap());
    session.append(EntryKind::Message {
        message: Message::user("We moved the deploys to GitHub Actions last week."),
    });
    let registry = registry(&home, &ext, &session.id());
    registry.services.insert(router);
    registry.services.insert(session.clone());

    // The reply repeats a known memory in other spelling and corrects another one.
    let reply = json!([
        { "kind": "convention", "text": "the project   INDENTS with tabs", "tags": [] },
        { "kind": "fact", "text": "Deploys go through GitHub Actions", "tags": ["ci"], "replaces": jenkins },
    ]);
    mock.push("m", MockStep::text(format!("```json\n{reply}\n```")));
    registry.bus.emit(&mut Event::SessionEnd).await;

    let listed = store.list(Scope::Project).await.unwrap();
    assert_eq!(
        texts(&listed),
        [
            "Deploys go through GitHub Actions",
            "The project indents with tabs"
        ]
    );
    assert_eq!(listed[0].tags, ["ci"]);
    assert_eq!(listed[0].kind, "fact");
    // The superseded memory is kept on record, but neither listed nor found.
    assert_eq!(found(&store, "Jenkins").await, Vec::<String>::new());
    let export = store.export().await.unwrap();
    let records: Vec<Value> = export
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    assert_eq!(records.len(), 3);
    let old = records.iter().find(|record| record["id"] == jenkins).unwrap();
    assert_eq!(old["superseded_by"], listed[0].id);
    let new = records
        .iter()
        .find(|record| record["id"] == listed[0].id)
        .unwrap();
    assert_eq!(new["source_session"], session.id());

    // The model was asked once, with the conversation and the memories it could replace.
    let requests = mock.requests_for("m");
    assert_eq!(requests.len(), 1);
    assert!(requests[0].tools.is_empty());
    assert!(requests[0].system[0].text.contains("credential"));
    let asked = requests[0].messages[0].text();
    assert!(
        asked.contains("We moved the deploys to GitHub Actions last week."),
        "{asked}"
    );
    assert!(
        asked.contains(&format!("- {jenkins} [fact] Deploys go through Jenkins")),
        "{asked}"
    );

    // Switched off for the session, nothing is asked.
    let off = slash(&registry, "auto off").await.unwrap();
    assert_eq!(off, "automatic extraction is off for this session");
    registry.bus.emit(&mut Event::SessionEnd).await;
    assert_eq!(mock.requests_for("m").len(), 1);

    // A reply that is not the JSON asked for changes nothing.
    slash(&registry, "auto on").await.unwrap();
    mock.push("m", MockStep::text("Nothing here is worth keeping."));
    registry.bus.emit(&mut Event::SessionEnd).await;
    assert_eq!(mock.requests_for("m").len(), 2);
    assert_eq!(store.list(Scope::Project).await.unwrap(), listed);
}

const CONFIG_RS: &str = "/// Reads the settings file.
pub fn parse_config(text: &str) -> Settings {
    let mut config = Settings::default();
    for line in text.lines() {
        parse_config_line(&mut config, line);
    }
    config
}
";
const RENDER_RS: &str = "pub fn draw_frame(buffer: &mut Buffer) {
    buffer.clear();
    buffer.flush_scanlines();
}
";
const NET_RS: &str = "pub fn open_socket(address: &str) -> Socket {
    Socket::connect(address)
}
";

async fn semantic(registry: &Registry, input: Value) -> String {
    let output = call(registry, "semantic_search", input).await;
    assert!(!output.is_error, "{}", output.text_content());
    output.text_content()
}

/// Reports a finished `edit` or `write` of `path` to the extensions, as the agent loop does.
async fn agent_wrote(registry: &Registry, call_id: &str, tool: &str, path: &str, output: ToolOutput) {
    let agent = lead("s1");
    let mut started = Event::ToolCall {
        agent: agent.clone(),
        call_id: call_id.to_string(),
        tool: tool.to_string(),
        input: json!({ "path": path }),
        skip_prompt: false,
    };
    registry.bus.emit(&mut started).await;
    let mut finished = Event::ToolResult {
        agent,
        call_id: call_id.to_string(),
        tool: tool.to_string(),
        output,
    };
    registry.bus.emit(&mut finished).await;
}

#[tokio::test]
async fn the_project_index_follows_the_files() {
    let home = home();
    let project = home.paths.project_root.clone();
    std::fs::create_dir_all(project.join("src")).unwrap();
    std::fs::create_dir_all(project.join("generated")).unwrap();
    std::fs::write(project.join("src/config.rs"), CONFIG_RS).unwrap();
    std::fs::write(project.join("src/render.rs"), RENDER_RS).unwrap();
    std::fs::write(project.join("src/net.rs"), NET_RS).unwrap();
    // None of these belong in the index: lock files, a file over 1 MB, a binary, an ignored file.
    std::fs::write(project.join("Cargo.lock"), "parse config\n").unwrap();
    std::fs::write(project.join("package-lock.json"), "{\"parse\": \"config\"}\n").unwrap();
    std::fs::write(project.join("big.txt"), "parse config\n".repeat(90_000)).unwrap();
    std::fs::write(project.join("blob.bin"), b"parse config\0\0\0").unwrap();
    std::fs::write(project.join(".gitignore"), "generated/\n").unwrap();
    std::fs::write(project.join("generated/out.rs"), "fn parse_config() {}\n").unwrap();

    let ext = open(&home, &quiet());
    let store = ext.store();
    let registry = registry(&home, &ext, "s1");
    assert_eq!(store.index_project(true).await.unwrap(), (3, 3));
    assert_eq!(store.collections().await.unwrap(), [("code".to_string(), 3, 3)]);

    // The file that defines the function is found, located by its lines.
    let hits = semantic(&registry, json!({ "query": "parse config" })).await;
    assert_eq!(hits, format!("src/config.rs:1-8\n{}", CONFIG_RS.trim_end()));
    let hits = semantic(&registry, json!({ "query": "socket", "path_glob": "src/n*.rs" })).await;
    assert!(hits.starts_with("src/net.rs:1-3\n"), "{hits}");
    let hits = semantic(&registry, json!({ "query": "socket", "path_glob": "*.md" })).await;
    assert_eq!(hits, "nothing in `code` matches `socket`");

    // What an agent writes is searchable two seconds later; a failed edit is not looked at.
    let handshake = format!("{NET_RS}\npub fn websocket_handshake() {{}}\n");
    std::fs::write(project.join("src/net.rs"), handshake).unwrap();
    std::fs::write(project.join("src/render.rs"), "pub fn tessellate_mesh() {}\n").unwrap();
    let failed = ToolOutput::error("old text not found (edit #1)");
    agent_wrote(&registry, "c1", "edit", "src/render.rs", failed).await;
    agent_wrote(&registry, "c2", "edit", "src/net.rs", ToolOutput::text("edited")).await;
    let query = json!({ "query": "websocket handshake" });
    assert_eq!(
        semantic(&registry, query.clone()).await,
        "nothing in `code` matches `websocket handshake`"
    );
    eventually("the edit is indexed", || async {
        semantic(&registry, query.clone())
            .await
            .starts_with("src/net.rs:1-5\n")
    })
    .await;
    let stale = semantic(&registry, json!({ "query": "tessellate mesh" })).await;
    assert_eq!(stale, "nothing in `code` matches `tessellate mesh`");

    // A refresh picks up changes nobody reported and drops files that are gone.
    std::fs::remove_file(project.join("src/net.rs")).unwrap();
    assert_eq!(store.index_project(false).await.unwrap(), (2, 2));
    let hits = semantic(&registry, json!({ "query": "tessellate mesh" })).await;
    assert!(hits.starts_with("src/render.rs:1-1\n"), "{hits}");
    let hits = semantic(&registry, json!({ "query": "websocket handshake socket" })).await;
    assert_eq!(hits, "nothing in `code` matches `websocket handshake socket`");

    // Documents live in collections of their own.
    let handbook = home.dir.path().join("handbook");
    std::fs::create_dir_all(&handbook).unwrap();
    std::fs::write(
        handbook.join("release.md"),
        "# Release checklist\nTag the commit and publish the changelog.\n",
    )
    .unwrap();
    assert_eq!(
        store
            .add_documents("handbook", std::slice::from_ref(&handbook))
            .await
            .unwrap(),
        (1, 1)
    );
    assert_eq!(
        store.collections().await.unwrap(),
        [("code".to_string(), 2, 2), ("docs:handbook".to_string(), 1, 1)]
    );
    let hits = semantic(
        &registry,
        json!({ "query": "changelog", "collection": "handbook" }),
    )
    .await;
    assert!(
        hits.contains("handbook/release.md:1-2\n# Release checklist"),
        "{hits}"
    );
    let hits = semantic(&registry, json!({ "query": "changelog" })).await;
    assert_eq!(hits, "nothing in `code` matches `changelog`");
    let missing = store.add_documents("handbook", &[handbook.join("nope.md")]).await;
    assert!(missing.unwrap_err().ends_with("does not exist"));
    assert!(store.add_documents("hand book", &[handbook]).await.is_err());
    assert!(store.remove_collection("handbook").await.unwrap());
    assert!(!store.remove_collection("handbook").await.unwrap());
    assert_eq!(store.collections().await.unwrap(), [("code".to_string(), 2, 2)]);
}

/// Reads `memory://<rest>` the way the `read` tool would.
async fn address(registry: &Registry, rest: &str) -> Result<String, String> {
    let ctx = registry.bus.ctx();
    let agent = lead(&ctx.session_id);
    let ctx = ToolCtx {
        agent_id: agent.id.clone(),
        session_id: agent.session_id.clone(),
        agent,
        call_id: "call".into(),
        cwd: ctx.cwd,
        cancel: CancellationToken::new(),
        update: UpdateSink::default(),
        services: ctx.services,
    };
    let blocks = registry.schemes.get("memory").unwrap().read(rest, &ctx).await?;
    Ok(blocks
        .iter()
        .filter_map(|block| match block {
            azazel_ai::Block::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect())
}

#[tokio::test]
async fn memory_addresses_list_what_is_remembered_show_one_memory_and_search() {
    let home = home();
    let ext = open(&home, &quiet());
    let registry = registry(&home, &ext, "s1");
    assert_eq!(address(&registry, "").await.unwrap(), "nothing is remembered yet");

    let store = ext.store();
    let style = ["style".to_string()];
    let tabs = store
        .save(
            "Indent with tabs, never spaces",
            "convention",
            &style,
            Scope::Project,
            None,
        )
        .await
        .unwrap();
    let terse = store
        .save(
            "The user prefers short answers",
            "preference",
            &[],
            Scope::Global,
            None,
        )
        .await
        .unwrap();

    assert_eq!(
        address(&registry, "").await.unwrap(),
        format!(
            "{tabs} [convention, project] Indent with tabs, never spaces #style\n\
             {terse} [preference, global] The user prefers short answers"
        )
    );

    let one = address(&registry, &tabs).await.unwrap();
    assert!(
        one.starts_with(&format!("id: {tabs}\nkind: convention\nscope: project\nsaved: ")),
        "{one}"
    );
    assert!(
        one.ends_with("\ntags: style\n\nIndent with tabs, never spaces"),
        "{one}"
    );

    // The query may be written as in a URL.
    for query in [
        "search/short answers",
        "search/short%20answers",
        "search/short+answers/",
    ] {
        assert_eq!(
            address(&registry, query).await.unwrap(),
            format!("{terse} [preference, global] The user prefers short answers"),
            "{query}"
        );
    }
    assert_eq!(
        address(&registry, "search/zebra").await.unwrap(),
        "no memory matches `zebra`"
    );
    assert_eq!(
        address(&registry, "search/").await.unwrap_err(),
        "memory://search/: give the words to search for after the slash"
    );
    assert_eq!(
        address(&registry, "nope").await.unwrap_err(),
        "no memory has the id `nope`; memory:// lists them, memory://search/<query> searches"
    );
}
