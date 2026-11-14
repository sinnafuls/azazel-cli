//! The readers against the made-up homes under `tests/fixtures/migrate`, and whole migrations
//! into a scratch azazel home.

use std::path::{Path, PathBuf};

use azazel_ai::{AuthStyle, Block, Credential, Credentials, ThinkingLevel, WireApi};
use azazel_core::{Config, EntryKind, LoadOptions, Paths, SessionStore, frontmatter};
use serde_json::Value;

use super::model::{Body, FileItem, Scope, Setting, Turn, TurnKind};
use super::*;

fn fixture(source: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/fixtures/migrate")
        .join(source)
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for entry in std::fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        match entry.file_type().unwrap().is_dir() {
            true => copy_dir(&entry.path(), &target),
            false => drop(std::fs::copy(entry.path(), target).unwrap()),
        }
    }
}

/// A copy of a fixture's home and project, and an azazel home with nothing in it.
struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new(source: &str) -> Sandbox {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixture(source).join("home"), &dir.path().join("home"));
        let project = fixture(source).join("project");
        if project.is_dir() {
            copy_dir(&project, &dir.path().join("project"));
        }
        // The project is its own root, whatever lies above the temporary directory.
        std::fs::create_dir_all(dir.path().join("project/.git")).unwrap();
        Sandbox { dir }
    }

    fn path(&self, relative: &str) -> PathBuf {
        self.dir.path().join(relative)
    }

    fn env(&self) -> Env {
        Env::at(&self.path("home"))
    }

    fn project(&self) -> PathBuf {
        azazel_core::paths::canonical(&self.path("project"))
    }

    /// The azazel home, and the project the migration runs in.
    fn paths(&self) -> Paths {
        let home = self.path("azazel");
        Paths::discover(&self.project(), Some(&home.to_string_lossy()))
    }

    fn found(&self, source: Source) -> Found {
        scan(&self.env(), &self.project(), &[source]).remove(0)
    }

    async fn migrate(&self, source: Source, options: &Options) -> Report {
        migrate(&self.paths(), &[self.found(source)], options).await
    }

    fn config(&self) -> Config {
        let options = LoadOptions {
            trusted: false,
            overrides: toml::Table::new(),
            env: Vec::new(),
        };
        Config::load(&self.paths(), &options).unwrap().config
    }

    fn keys(&self) -> std::collections::BTreeMap<String, Credential> {
        Credentials::new(self.paths().auth_file()).load()
    }

    fn json(&self, relative: &str) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.path(relative)).unwrap()).unwrap()
    }

    /// The sessions azazel holds for the project at `cwd`.
    fn sessions(&self, cwd: &str) -> Vec<azazel_core::session::SessionSummary> {
        let home = self.path("azazel");
        let dir = Paths::discover(Path::new(cwd), Some(&home.to_string_lossy())).sessions_dir();
        SessionStore::list(&dir)
    }
}

fn everything() -> Options {
    Options::new(Kind::ALL.to_vec(), false, false)
}

fn setting<'a>(found: &'a Found, key: &str) -> Option<&'a toml::Value> {
    found
        .settings
        .iter()
        .find(|setting| setting.key.join(".") == key)
        .map(|setting| &setting.value)
}

fn text_of(found: &Found, to: &str) -> String {
    let item = file_to(found, to);
    match &item.body {
        Body::Text(text) => text.clone(),
        Body::Copy => std::fs::read_to_string(&item.from).unwrap(),
    }
}

fn file_to<'a>(found: &'a Found, to: &str) -> &'a FileItem {
    found
        .files
        .iter()
        .find(|item| item.to == Path::new(to))
        .unwrap_or_else(|| {
            panic!(
                "no file goes to {to}: {:?}",
                found.files.iter().map(|f| &f.to).collect::<Vec<_>>()
            )
        })
}

fn lossy(found: &Found, needle: &str) -> bool {
    found.lossy.iter().any(|note| note.text.contains(needle))
}

fn strings(value: &toml::Value) -> Vec<&str> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(toml::Value::as_str).collect())
        .unwrap_or_default()
}

/// What was said in a conversation, in order: who, and the text or what stands for it.
fn spoken(turns: &[Turn]) -> Vec<String> {
    let blocks = |blocks: &[Block]| -> String {
        blocks
            .iter()
            .map(|block| match block {
                Block::Text { text } => text.clone(),
                Block::Thinking { signature, .. } => {
                    format!("<thinking{}>", if signature.is_some() { " signed" } else { "" })
                }
                Block::Image { mime, .. } => format!("<{mime}>"),
                Block::ToolCall { name, .. } => format!("<call {name}>"),
                other => format!("{other:?}"),
            })
            .collect::<Vec<_>>()
            .join(" | ")
    };
    turns
        .iter()
        .map(|turn| match &turn.kind {
            TurnKind::User(said) => format!("user: {}", blocks(said)),
            TurnKind::Assistant { blocks: said, .. } => format!("assistant: {}", blocks(said)),
            TurnKind::ToolResult {
                call_id,
                content,
                is_error,
            } => format!(
                "result {call_id}{}: {}",
                if *is_error { " (failed)" } else { "" },
                blocks(content)
            ),
            TurnKind::Compaction { summary, kept, .. } => format!("compaction keeping {kept}: {summary}"),
            TurnKind::Note { name, .. } => format!("note: {name}"),
        })
        .collect()
}

// ---------------------------------------------------------------- pi

#[test]
fn pi_models_keys_settings_and_resources_are_found() {
    let home = fixture("pi").join("home");
    let project = fixture("pi").join("project");
    let found = scan(&Env::at(&home), &project, &[Source::Pi]).remove(0);

    let gateway = &found.providers[0];
    assert_eq!(gateway.id, "team-gateway");
    assert_eq!(gateway.settings.api, Some(WireApi::OpenAiChat));
    assert_eq!(
        gateway.settings.base_url.as_deref(),
        Some("https://llm.example.com/v1")
    );
    assert_eq!(gateway.settings.auth, Some(AuthStyle::Bearer));
    assert_eq!(gateway.settings.headers["X-Team"], "platform");
    assert_eq!(
        gateway.settings.api_key, None,
        "a key never goes into the settings"
    );
    let (id, big) = &gateway.models[0];
    assert_eq!(id, "big-model");
    assert_eq!(
        (big.context_window, big.max_output, big.vision, big.reasoning),
        (Some(200_000), Some(32_000), Some(true), Some(true))
    );
    assert_eq!(
        big.cost.map(|cost| (cost.input, cost.cache_write)),
        Some((3.0, 3.75))
    );
    // A built-in provider only gains the models named for it.
    let ollama = &found.providers[1];
    assert_eq!((ollama.id.as_str(), ollama.settings.api), ("ollama", None));
    assert_eq!(ollama.models[0].1.context_window, Some(32_768));

    let keyed: Vec<(&str, &str)> = found
        .keys
        .iter()
        .map(|key| (key.provider.as_str(), key.key.expose()))
        .collect();
    assert_eq!(
        keyed,
        [
            ("team-gateway", "test-key-team-gateway"),
            ("openrouter", "test-key-openrouter")
        ]
    );
    assert_eq!(found.logins.len(), 1);
    assert_eq!(found.logins[0].provider, "anthropic");

    assert_eq!(found.roles[0].name, "default");
    assert_eq!(found.roles[0].chain, ["team-gateway/big-model:high"]);
    assert_eq!(
        strings(setting(&found, "tui.favourites").unwrap()),
        ["team-gateway/big-model", "ollama/local-coder"]
    );
    assert_eq!(setting(&found, "tui.theme").unwrap().as_str(), Some("light"));
    assert_eq!(setting(&found, "tui.queue_mode").unwrap().as_str(), Some("all"));
    assert_eq!(
        setting(&found, "tui.show_thinking").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(
        setting(&found, "compaction.reserve_tokens").unwrap().as_integer(),
        Some(20_000)
    );
    assert_eq!(
        setting(&found, "retry.max_attempts").unwrap().as_integer(),
        Some(5)
    );
    assert!(lossy(&found, "extensions"));

    assert_eq!(file_to(&found, "skills/release-notes").scope, Scope::Home);
    assert_eq!(
        file_to(&found, "skills/deploy").scope,
        Scope::Project(project.clone())
    );
    let review = text_of(&found, "commands/review.md");
    assert!(
        review.contains("Review $ARGUMENTS carefully. Compare $1 with ${2:-main}."),
        "{review}"
    );
    assert!(review.contains("argument-hint: <branch> [base]"), "{review}");
    let noted = &file_to(&found, "commands/review.md").notes;
    assert!(noted.iter().any(|note| note.contains("${@:2}")), "{noted:?}");
    assert_eq!(
        file_to(&found, "commands/ship.md").scope,
        Scope::Project(project.clone())
    );
    assert!(text_of(&found, "rules/pi-agents.md").contains("Never push without asking."));

    let servers: Vec<(&str, &Scope, Option<u64>)> = found
        .servers
        .iter()
        .map(|server| (server.name.as_str(), &server.scope, server.timeout))
        .collect();
    assert_eq!(
        servers,
        [
            ("notes", &Scope::Home, Some(30)),
            ("docs", &Scope::Home, None),
            ("tickets", &Scope::Project(project), None),
        ]
    );
    let chats: Vec<(&str, Option<&Path>)> = found
        .chats
        .iter()
        .map(|chat| (chat.id.as_str(), chat.cwd.as_deref()))
        .collect();
    assert_eq!(
        chats,
        [
            (
                "0f0f0f0f-0000-4000-8000-000000000001",
                Some(Path::new("/work/app"))
            ),
            (
                "0f0f0f0f-0000-4000-8000-000000000002",
                Some(Path::new("/work/app"))
            ),
        ]
    );
}

#[test]
fn a_pi_session_is_read_along_the_branch_it_ended_on() {
    let found = scan(
        &Env::at(&fixture("pi").join("home")),
        Path::new("/nowhere"),
        &[Source::Pi],
    )
    .remove(0);
    let chat = read_chat(&found, &found.chats[0]).unwrap();
    assert_eq!(
        spoken(&chat.turns),
        [
            "user: read notes.txt",
            // The signature of another provider's thinking is no use to azazel.
            "assistant: <thinking> | Reading it. | <call read>",
            "result call_1: hello",
            "assistant: It says hello.",
            "user: what about todo.txt?",
            "assistant: There is no todo.txt.",
        ]
    );
    assert_eq!(chat.title.as_deref(), Some("Notes review"));
    assert_eq!(chat.cwd.as_deref(), Some(Path::new("/work/app")));
    assert_eq!(chat.created, Some("2026-02-16T10:20:30Z".parse().unwrap()));
    assert_eq!(
        chat.dropped,
        ["2 messages on branches that were left are not imported"]
    );
    let TurnKind::Assistant {
        model, api, usage, ..
    } = &chat.turns[1].kind
    else {
        panic!("a reply");
    };
    assert_eq!(
        model.clone().unwrap(),
        ("team-gateway".to_string(), "big-model".to_string())
    );
    assert_eq!(*api, Some(WireApi::OpenAiChat));
    assert_eq!(
        (usage.input, usage.cache_read, usage.cost_usd),
        (120, 900, Some(0.001))
    );

    // A compaction names the first entry it kept; here, the two turns before it.
    let compacted = read_chat(&found, &found.chats[1]).unwrap();
    assert_eq!(
        spoken(&compacted.turns)[2..5],
        [
            "user: go on",
            "assistant: two",
            "compaction keeping 2: They counted to two."
        ]
    );
    assert_eq!(compacted.title.as_deref(), Some("count to two"));
}

// ---------------------------------------------------------------- omp

#[test]
fn omp_roles_follow_models_that_moved_and_settings_take_azazels_names() {
    let home = fixture("omp").join("home");
    let project = fixture("omp").join("project");
    let found = scan(&Env::at(&home), &project, &[Source::Omp]).remove(0);

    let chain = |name: &str| -> Vec<&str> {
        let role = found.roles.iter().find(|role| role.name == name);
        role.map(|role| role.chain.iter().map(String::as_str).collect())
            .unwrap_or_default()
    };
    assert_eq!(chain("default"), ["anthropic/claude-test-1:high"]);
    assert_eq!(chain("smol"), ["gate/small-chat:low"]);
    // The model that speaks another wire format than its provider went to a provider of its own.
    assert_eq!(chain("slow"), ["gate-responses/big-responses:xhigh"]);
    // A role's fallbacks follow it.
    assert_eq!(
        chain("advisor"),
        ["openrouter/vendor/model-x:max", "gate/small-chat:high"]
    );
    assert!(chain("odd").is_empty());
    assert!(lossy(&found, "role odd: `nowhere/model-y` is left out"));
    assert!(lossy(&found, "retry.fallbackChains.gate/small-chat"));

    let ids: Vec<&str> = found.providers.iter().map(|p| p.id.as_str()).collect();
    assert_eq!(ids, ["gate", "gate-responses"]);
    let levels = found.providers[1].models[0].1.thinking_levels.clone().unwrap();
    assert_eq!(levels.last(), Some(&ThinkingLevel::Xhigh));

    assert_eq!(setting(&found, "tui.glyphs").unwrap().as_str(), Some("nerd"));
    assert_eq!(
        setting(&found, "tui.theme").unwrap().as_str(),
        Some("tokyo-night")
    );
    assert_eq!(
        setting(&found, "statusline.separator").unwrap().as_str(),
        Some("powerline")
    );
    assert_eq!(
        setting(&found, "tui.session_accent").unwrap().as_bool(),
        Some(true)
    );
    assert_eq!(
        setting(&found, "team.max_parallel").unwrap().as_integer(),
        Some(6)
    );
    assert_eq!(
        setting(&found, "compaction.keep_recent_tokens")
            .unwrap()
            .as_integer(),
        Some(24_000)
    );
    assert_eq!(setting(&found, "tui.queue_mode").unwrap().as_str(), Some("one"));
    // `yolo` asks nothing, and neither does `auto`; the guards stay unless the user lifts them.
    assert_eq!(
        setting(&found, "permissions.mode").unwrap().as_str(),
        Some("auto")
    );
    assert!(lossy(&found, "yolo becomes `auto`"));
    // Only roles that exist are stepped through.
    assert_eq!(
        strings(setting(&found, "tui.favourites").unwrap()),
        ["@default", "@slow"]
    );
    assert_eq!(strings(setting(&found, "advisor.model").unwrap()), ["@advisor"]);
    assert_eq!(setting(&found, "statusline.preset"), None);
    assert!(lossy(&found, "statusLine.preset: compact has no equivalent"));
    assert!(lossy(&found, "disabledProviders, extensions"));

    let keyed: Vec<(&str, &str)> = found
        .keys
        .iter()
        .map(|key| (key.provider.as_str(), key.origin.as_str()))
        .collect();
    assert_eq!(
        keyed,
        [
            ("gate", "models.yml (provider gate)"),
            ("gate-responses", "models.yml (provider gate)"),
            ("anthropic", ".env (ANTHROPIC_API_KEY)"),
        ]
    );

    let standup = text_of(&found, "commands/standup.md");
    assert!(
        standup.contains("since ${1:-yesterday}") && !standup.contains("side:"),
        "{standup}"
    );
    let auditor = text_of(&found, "agents/auditor.md");
    let (yaml, body) = frontmatter::split(&auditor).unwrap();
    let meta = frontmatter::parse(yaml).unwrap();
    assert_eq!(meta["tools"], serde_json::json!(["read", "grep", "shell"]));
    assert_eq!(meta["model"], "gate/small-chat:high");
    assert_eq!(meta["thinking"], "high");
    assert_eq!(meta["spawns"], "scout");
    assert!(!meta.contains_key("blocking") && body.contains("You audit code"));
    assert!(file_to(&found, "agents/auditor.md").notes[0].contains("blocking"));

    let rules = text_of(&found, "rules/omp-rules.md");
    assert!(rules.contains("always: true") && rules.contains("Scratch files go under tmp/ only."));
    let rust = text_of(&found, "rules/omp/rust.md");
    let (yaml, _) = frontmatter::split(&rust).unwrap();
    let meta = frontmatter::parse(yaml).unwrap();
    assert_eq!(meta["trigger"], r"\.unwrap\(\)");
    assert_eq!(meta["globs"], serde_json::json!(["crates/**/*.rs"]));
    assert!(!meta.contains_key("condition") && !meta.contains_key("interruptMode"));
    assert_eq!(
        file_to(&found, "commands/deploy.md").scope,
        Scope::Project(project.clone())
    );
    assert_eq!(
        found
            .files
            .iter()
            .filter(|item| item.to == Path::new("rules/omp-rules.md"))
            .count(),
        2,
        "the user's and the project's"
    );

    let files = &found.servers[0];
    assert_eq!((files.name.as_str(), files.timeout), ("files", Some(30)));
    assert_eq!(
        found.servers[1].entry["enabled"], false,
        "a server omp lists as disabled stays so"
    );
    assert_eq!(found.chats[0].title.as_deref(), Some("Fix the cart total"));
    assert_eq!(found.chats[0].cwd.as_deref(), Some(Path::new("/work/shop")));
    // A file that does not begin as a session is no conversation, and is said.
    assert_eq!(found.chats.len(), 1);
    assert!(lossy(
        &found,
        "1 session files do not start with a session header"
    ));
}

#[test]
fn an_omp_session_gets_its_images_back_and_its_sub_agents_along() {
    let found = scan(
        &Env::at(&fixture("omp").join("home")),
        Path::new("/nowhere"),
        &[Source::Omp],
    )
    .remove(0);
    let chat = read_chat(&found, &found.chats[0]).unwrap();
    assert_eq!(
        spoken(&chat.turns),
        [
            "user: the cart total is wrong, see the screenshot | <image/png>",
            // Anthropic's own signature goes back to it with the thinking.
            "assistant: <thinking signed> | <call grep> | <call read>",
            "result toolu_1: src/cart.ts:12: const total = sum(items)",
            "result toolu_2: export function total() {}",
            "user: I ran `npm test` myself:\n1 failed\nexit code 1",
            "assistant: The total skipped the discount. Fixed in src/cart.ts.",
        ]
    );
    // The image was moved out of the session file by omp; the bytes come from its blob store.
    let TurnKind::User(blocks) = &chat.turns[0].kind else {
        panic!("a prompt");
    };
    let Block::Image { data, .. } = &blocks[1] else {
        panic!("an image");
    };
    assert!(data.starts_with("iVBORw0KGgo"), "{data}");
    assert_eq!(chat.title.as_deref(), Some("Fix the cart total"));
    // The session had gone back to an earlier leaf before it ended; what came after is a branch.
    assert_eq!(
        chat.dropped,
        ["1 messages on branches that were left are not imported"]
    );

    // Its sub-agent, and the sub-agent that one started.
    let names: Vec<&str> = chat.agents.iter().map(|(name, _)| name.as_str()).collect();
    assert_eq!(names, ["Scout", "Scout.Helper"]);
    let (_, scout) = &chat.agents[0];
    assert_eq!(
        spoken(&scout.turns),
        [
            "user: find where the discount is applied",
            "assistant: In src/discount.ts."
        ]
    );
}

/// The SQLite stores of an omp home: two keys of one provider, sign-ins (one of them removed),
/// and a memory bank with a fact, a superseded fact and a transcript episode.
#[cfg(feature = "memory")]
fn omp_databases(agent: &Path, project: &Path) {
    let conn = rusqlite::Connection::open(agent.join("agent.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE auth_credentials (
            id INTEGER PRIMARY KEY AUTOINCREMENT, provider TEXT NOT NULL, credential_type TEXT NOT NULL,
            data TEXT NOT NULL, disabled_cause TEXT DEFAULT NULL, identity_key TEXT DEFAULT NULL);
         INSERT INTO auth_credentials (provider, credential_type, data, disabled_cause) VALUES
            ('openrouter', 'api_key', '{\"key\":\"test-key-openrouter-one\"}', NULL),
            ('openrouter', 'api_key', '{\"key\":\"test-key-openrouter-two\",\"source\":\"login\"}', NULL),
            ('lm-studio', 'api_key', '{\"key\":\"test-key-lmstudio\"}', NULL),
            ('zai', 'api_key', '{\"key\":\"test-key-removed\"}', 'deleted by user'),
            ('openai-codex', 'oauth', '{\"access\":\"a\",\"refresh\":\"r\",\"expires\":1,\"email\":\"old@example.com\"}', 'oauth refresh failed'),
            ('openai-codex', 'oauth', '{\"access\":\"a\",\"refresh\":\"r\",\"expires\":1,\"email\":\"dev@example.com\"}', NULL),
            ('anthropic', 'oauth', '{\"access\":\"a\",\"refresh\":\"r\",\"expires\":1}', NULL);",
    )
    .unwrap();
    let bank = agent.join("memories/mnemopi/banks/shop-1abc");
    std::fs::create_dir_all(&bank).unwrap();
    let conn = rusqlite::Connection::open(bank.join("mnemopi.db")).unwrap();
    conn.execute_batch(
        "CREATE TABLE working_memory (
            id TEXT PRIMARY KEY, content TEXT NOT NULL, timestamp TEXT, metadata_json TEXT,
            memory_type TEXT DEFAULT 'unknown', superseded_by TEXT DEFAULT NULL);",
    )
    .unwrap();
    let metadata = serde_json::json!({ "cwd": project, "session_id": "s1" }).to_string();
    for (id, content, kind, superseded) in [
        ("m1", "The cart total includes the discount.", "fact", None),
        ("m2", "The cart total excludes the discount.", "fact", Some("m1")),
        (
            "m3",
            "user: the cart total is wrong\nassistant: ...",
            "episode",
            None,
        ),
    ] {
        conn.execute(
            "INSERT INTO working_memory (id, content, timestamp, metadata_json, memory_type, superseded_by)
             VALUES (?1, ?2, '2026-03-01T09:00:00Z', ?3, ?4, ?5)",
            rusqlite::params![id, content, metadata, kind, superseded],
        )
        .unwrap();
    }
}

#[cfg(feature = "memory")]
#[tokio::test]
async fn omp_keys_and_memories_come_out_of_its_databases() {
    let sandbox = Sandbox::new("omp");
    omp_databases(&sandbox.path("home/.omp/agent"), &sandbox.project());
    let found = sandbox.found(Source::Omp);
    let from_database: Vec<&str> = found
        .keys
        .iter()
        .filter(|key| key.origin == "agent.db")
        .map(|key| key.provider.as_str())
        .collect();
    // Credentials omp marked as removed are not among them.
    assert_eq!(from_database, ["openrouter", "openrouter", "lmstudio"]);
    assert_eq!(found.logins.len(), 2);
    assert_eq!(found.logins[0].who.as_deref(), Some("dev@example.com"));
    assert_eq!(found.memories.len(), 1);
    assert_eq!(found.memories[0].scope, Scope::Project(sandbox.project()));
    assert!(lossy(&found, "1 transcript episodes"));

    let options = Options::new(vec![Kind::Keys, Kind::Memory], false, false);
    let report = sandbox.migrate(Source::Omp, &options).await;
    let keys = sandbox.keys();
    // The first key of a provider is its default account; the next is an account beside it.
    let key = |slot: &str| match keys.get(slot) {
        Some(Credential::ApiKey { key }) => key.as_str(),
        other => panic!("{slot}: {other:?}"),
    };
    assert_eq!(key("openrouter"), "test-key-openrouter-one");
    assert_eq!(key("openrouter#omp-2"), "test-key-openrouter-two");
    assert_eq!(key("lmstudio"), "test-key-lmstudio");
    // omp signs in to Anthropic: the key it also has for it would change who is billed.
    assert!(!keys.contains_key("anthropic"));
    let skipped = report.of(Source::Omp, Kind::Keys, Status::Skipped).join("\n");
    assert!(
        skipped
            .contains("anthropic: omp signs in to it, so its key from .env (ANTHROPIC_API_KEY) is left out"),
        "{skipped}"
    );
    // A key of a provider that only exists in omp's models.yml is not stored without it.
    assert!(!keys.contains_key("gate"));
    assert!(report.of(Source::Omp, Kind::Keys, Status::Lossy)[0].contains("azazel has no provider"));
    // A sign-in is named with what to run, its address cut; nothing of it is stored.
    let logins = report.of(Source::Omp, Kind::Keys, Status::Login);
    assert_eq!(
        logins,
        [
            "openai-codex (d***@example.com): sign-ins are not copied; run `azazel login openai-codex`",
            "anthropic: sign-ins are not copied; run `azazel login anthropic`",
        ]
    );
    assert!(!keys.contains_key("openai-codex"));
    // No key shows in the report.
    assert!(!report.render().contains("test-key-"), "{}", report.render());

    let config = sandbox.config();
    let credentials = std::sync::Arc::new(Credentials::new(sandbox.paths().auth_file()));
    let store = azazel_memory::MemoryExt::open(&sandbox.paths(), &config, credentials)
        .unwrap()
        .store();
    let remembered = store.list(azazel_memory::Scope::Project).await.unwrap();
    assert_eq!(remembered.len(), 1);
    assert_eq!(remembered[0].text, "The cart total includes the discount.");
    assert_eq!(remembered[0].tags, ["imported", "omp"]);

    // A second run finds it all there.
    let again = sandbox.migrate(Source::Omp, &options).await;
    assert_eq!(again.count(Status::Imported), 0, "{}", again.render());
    assert!(again.of(Source::Omp, Kind::Memory, Status::Skipped)[0].contains("already remembered"));
    assert_eq!(store.list(azazel_memory::Scope::Project).await.unwrap().len(), 1);
}

// ---------------------------------------------------------------- jcode

#[test]
fn jcode_providers_keys_and_memories_are_found() {
    let home = fixture("jcode").join("home");
    let found = scan(&Env::at(&home), Path::new("/nowhere"), &[Source::Jcode]).remove(0);

    assert_eq!(found.roles[0].chain, ["anthropic/claude-test-1"]);
    let api = &found.providers[0];
    assert_eq!(api.id, "my-api");
    assert_eq!(api.settings.api, Some(WireApi::OpenAiChat));
    assert_eq!(api.settings.api_key_env.as_deref(), Some("MY_API_KEY"));
    let (id, fast) = &api.models[0];
    assert_eq!(
        (id.as_str(), fast.context_window, fast.vision),
        ("fast-1", Some(128_000), Some(true))
    );

    // The provider's key is in its environment file; the other file names a built-in provider.
    let keyed: Vec<(&str, &str)> = found
        .keys
        .iter()
        .map(|key| (key.provider.as_str(), key.key.expose()))
        .collect();
    assert_eq!(
        keyed,
        [
            ("my-api", "test-key-my-api"),
            ("openrouter", "test-key-openrouter-jcode")
        ]
    );
    assert_eq!(found.logins[0].provider, "anthropic");
    assert_eq!(found.logins[0].who.as_deref(), Some("dev@example.com"));

    assert_eq!(setting(&found, "tui.theme").unwrap().as_str(), Some("dark"));
    assert_eq!(setting(&found, "memory.enabled").unwrap().as_bool(), Some(false));
    assert!(lossy(&found, "hooks, keybindings"));
    assert!(file_to(&found, "skills/lint-rust").notes.is_empty());
    assert!(text_of(&found, "rules/jcode-prompt-overlay.md").contains("Prefer small commits."));
    assert_eq!(found.servers[0].timeout, Some(60));
    assert_eq!(found.chats[0].cwd.as_deref(), Some(Path::new("/work/tool")));

    // Only memories in force; one of a project that cannot be told becomes everybody's.
    let memories: Vec<(&str, &str, &Scope)> = found
        .memories
        .iter()
        .map(|memory| (memory.kind, memory.text.as_str(), &memory.scope))
        .collect();
    assert_eq!(
        memories,
        [
            ("preference", "Prefers ripgrep over grep.", &Scope::Home),
            (
                "convention",
                "The parser tests live in tests/parse.",
                &Scope::Home
            ),
        ]
    );
    assert!(lossy(&found, "names only by a hash (00000000deadbeef)"));
}

#[test]
fn a_jcode_session_is_its_snapshot_and_what_the_journal_added() {
    let found = scan(
        &Env::at(&fixture("jcode").join("home")),
        Path::new("/nowhere"),
        &[Source::Jcode],
    )
    .remove(0);
    let chat = read_chat(&found, &found.chats[0]).unwrap();
    assert_eq!(
        spoken(&chat.turns),
        [
            "compaction keeping 0: Earlier they set the project up.",
            "user: read notes.txt",
            "assistant: <thinking signed> | <call read>",
            "result tu_1: hello",
            "note: system",
            // From the journal; the message it repeats is not there twice.
            "assistant: It says hello.",
        ]
    );
    assert_eq!(
        chat.title.as_deref(),
        Some("fix the parser"),
        "the journal's newer title"
    );
    let TurnKind::Assistant { model, usage, .. } = &chat.turns[2].kind else {
        panic!("a reply");
    };
    assert_eq!(
        model.clone().unwrap(),
        ("anthropic".to_string(), "claude-test-1".to_string())
    );
    assert_eq!((usage.input, usage.output, usage.cache_read), (120, 18, 700));
}

// ---------------------------------------------------------------- Claude Code

#[test]
fn claude_settings_become_rules_modes_and_keys_azazel_has() {
    let home = fixture("claude").join("home");
    let project = Path::new("/work/site");
    let found = scan(&Env::at(&home), project, &[Source::Claude]).remove(0);

    assert_eq!(found.roles[0].chain, ["anthropic/claude-test-2"]);
    assert_eq!(
        strings(setting(&found, "permissions.allow").unwrap()),
        [
            "shell(npm run test:*)",
            // A rule for editing covers writing a file anew, as it does in Claude Code.
            "edit(src/**)",
            "write(src/**)",
            "mcp__db__query",
            "mcp__search__*",
            "web_fetch(domain:docs.example.com)",
        ]
    );
    assert!(lossy(&found, "`NotebookEdit` names a tool azazel does not have"));
    assert_eq!(
        strings(setting(&found, "permissions.deny").unwrap()),
        ["read(.env)", "read(/etc/passwd)"]
    );
    assert_eq!(
        strings(setting(&found, "permissions.ask").unwrap()),
        ["shell(git push:*)"]
    );
    assert_eq!(
        setting(&found, "permissions.mode").unwrap().as_str(),
        Some("accept-edits")
    );
    assert_eq!(
        strings(setting(&found, "permissions.additional_dirs").unwrap()),
        ["../lib"]
    );
    assert_eq!(
        setting(&found, "session.retention_days").unwrap().as_integer(),
        Some(45)
    );
    assert_eq!(
        setting(&found, "statusline.command").unwrap().as_str(),
        Some("~/.claude/statusline.sh")
    );
    assert_eq!(
        setting(&found, "providers.anthropic.api_key_command")
            .unwrap()
            .as_str(),
        Some("pass show anthropic")
    );
    assert_eq!(
        setting(&found, "tui.theme").unwrap().as_str(),
        Some("dark-daltonized")
    );
    assert!(lossy(&found, "hooks: not converted"));

    // An API key: the account beside it is the Console's that made the key, no subscription.
    assert_eq!(found.keys[0].provider, "anthropic");
    assert!(found.logins.is_empty());
    let servers: Vec<(&str, &Scope)> = found
        .servers
        .iter()
        .map(|server| (server.name.as_str(), &server.scope))
        .collect();
    assert_eq!(
        servers,
        [
            ("search", &Scope::Home),
            ("db", &Scope::Project(project.to_path_buf()))
        ]
    );

    let sync = text_of(&found, "commands/git/sync.md");
    let (yaml, body) = frontmatter::split(&sync).unwrap();
    let meta = frontmatter::parse(yaml).unwrap();
    assert_eq!(meta["model"], "@smol");
    assert!(!meta.contains_key("allowed-tools"));
    assert!(body.starts_with("Rebase $1 onto ${2:-origin/main}"), "{body}");

    let reviewer = text_of(&found, "agents/reviewer.md");
    let (yaml, _) = frontmatter::split(&reviewer).unwrap();
    let meta = frontmatter::parse(yaml).unwrap();
    assert_eq!(
        meta["tools"],
        serde_json::json!(["read", "grep", "shell(git diff:*)"])
    );
    assert_eq!(meta["model"], "@slow");
    assert_eq!(meta["permission-mode"], "plan");
    assert_eq!(meta["max-turns"], 20);
    assert!(!meta.contains_key("color"));

    // The user's CLAUDE.md is not read by azazel where it is: it becomes a rule, with the file
    // it includes written into it.
    let mine = text_of(&found, "rules/claude.md");
    assert!(
        mine.contains("always: true") && mine.contains("Be brief. No exclamation marks."),
        "{mine}"
    );
    assert_eq!(file_to(&found, "rules/claude/style.md").body, Body::Copy);
    // Its skills are: azazel reads them in place.
    assert!(
        found
            .in_place
            .iter()
            .any(|place| place.path == home.join(".claude/skills"))
    );
    assert!(found.files.iter().all(|item| item.kind != Kind::Skills));

    let memories: Vec<(&str, &Scope)> = found
        .memories
        .iter()
        .map(|memory| (memory.kind, &memory.scope))
        .collect();
    let site = Scope::Project(project.to_path_buf());
    assert_eq!(memories, [("convention", &site), ("preference", &site)]);
    assert!(found.memories[0].text.starts_with("Run `npm run test:visual`"));
}

#[tokio::test]
async fn a_claude_subscription_is_a_sign_in_to_repeat_and_keeps_a_stray_key_out() {
    let sandbox = Sandbox::new("claude");
    std::fs::write(
        sandbox.path("home/.claude/.credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"a","refreshToken":"r","expiresAt":1,"subscriptionType":"max"}}"#,
    )
    .unwrap();
    let report = sandbox
        .migrate(Source::Claude, &Options::new(vec![Kind::Keys], false, false))
        .await;
    assert_eq!(
        report.of(Source::Claude, Kind::Keys, Status::Login),
        ["anthropic (d***@example.com): sign-ins are not copied; run `azazel login anthropic`"]
    );
    // Nothing is stored: not the sign-in, and not the key that would be billed in its place.
    assert!(sandbox.keys().is_empty());
    assert_eq!(report.count(Status::Imported), 0, "{}", report.render());
}

#[test]
fn a_claude_transcript_is_read_through_its_compaction_with_replies_put_back_together() {
    let found = scan(
        &Env::at(&fixture("claude").join("home")),
        Path::new("/nowhere"),
        &[Source::Claude],
    )
    .remove(0);
    let chat = read_chat(&found, &found.chats[0]).unwrap();
    assert_eq!(
        spoken(&chat.turns),
        [
            "user: the navbar overlaps the logo on phones",
            // Three lines of the file, one reply.
            "assistant: <thinking signed> | Let me look at the styles. | <call Read>",
            "result toolu_01: .nav { position: fixed }",
            "assistant: The navbar needs a top margin below 600px.",
            "user: apply that | <image/png>",
            "assistant: Applied.",
            "compaction keeping 0: This session is being continued. The navbar was fixed with a top margin.",
            "user: now the footer",
            "assistant: The footer is fine.",
        ]
    );
    assert_eq!(chat.title.as_deref(), Some("Navbar and footer"));
    assert_eq!(chat.id, "11111111-2222-4333-8444-555555555555");
    assert_eq!(chat.cwd.as_deref(), Some(Path::new("/work/site")));
    assert_eq!(
        chat.dropped,
        ["1 messages on branches that were left are not imported"]
    );
    let TurnKind::Assistant { usage, model, .. } = &chat.turns[1].kind else {
        panic!("a reply");
    };
    // Every line of a reply repeats its usage; it counts once.
    assert_eq!((usage.output, usage.cache_write), (30, 100));
    assert_eq!(model.clone().unwrap().1, "claude-test-2");

    let (name, agent) = &chat.agents[0];
    assert_eq!(name, "agent-a1");
    assert_eq!(
        spoken(&agent.turns),
        [
            "user: find every fixed-position rule",
            "assistant: Only .nav is fixed."
        ]
    );
}

// ---------------------------------------------------------------- Codex

#[test]
fn codex_config_becomes_a_role_a_provider_and_servers() {
    let home = fixture("codex").join("home");
    let found = scan(&Env::at(&home), Path::new("/nowhere"), &[Source::Codex]).remove(0);

    // Signed in with ChatGPT: the model is the subscription's.
    assert_eq!(found.roles[0].chain, ["openai-codex/gpt-test-1:high"]);
    assert_eq!(found.logins[0].provider, "openai-codex");
    assert!(found.keys.is_empty());

    let corp = &found.providers[0];
    assert_eq!(
        (corp.id.as_str(), corp.settings.api),
        ("corp", Some(WireApi::OpenAiResponses))
    );
    assert_eq!(corp.settings.api_key_env.as_deref(), Some("CORP_API_KEY"));
    assert_eq!(corp.settings.headers["X-Org"], "eng");
    assert_eq!(corp.settings.headers["X-Trace"], "${TRACE_ID}");
    assert!(lossy(&found, "provider corp: `query_params`"));

    assert_eq!(
        setting(&found, "permissions.mode").unwrap().as_str(),
        Some("auto")
    );
    assert!(lossy(&found, "danger-full-access becomes `auto`"));
    assert!(lossy(&found, "profiles"));

    let docs = &found.servers[0];
    assert_eq!(docs.entry["headers"]["Authorization"], "Bearer ${DOCS_TOKEN}");
    assert_eq!(docs.timeout, Some(45));
    let files = &found.servers[1];
    assert_eq!(
        files.entry["env"],
        serde_json::json!({ "ROOT": "/data", "HOME_TOKEN": "${HOME_TOKEN}" })
    );
    assert_eq!(files.entry["includeTools"], serde_json::json!(["read_file"]));

    let prompt = text_of(&found, "commands/draftpr.md");
    assert!(prompt.contains("for $1. It costs $0."), "{prompt}");
    // Codex's own example of a hint is not YAML; it is read all the same, and written as YAML.
    let (yaml, _) = frontmatter::split(&prompt).unwrap();
    assert_eq!(
        frontmatter::parse(yaml).unwrap()["argument-hint"],
        "[FILES=<paths>] [PR_TITLE=\"<title>\"]"
    );
    let noted = &file_to(&found, "commands/draftpr.md").notes;
    assert_eq!(noted.len(), 1, "{noted:?}");
    assert!(
        noted[0].starts_with("$FILES, $PR_TITLE left as written"),
        "{noted:?}"
    );
    assert_eq!(file_to(&found, "skills/changelog").kind, Kind::Skills);
    assert!(text_of(&found, "rules/codex-agents.md").contains("Run the tests"));

    assert_eq!(found.chats[0].title.as_deref(), Some("Fix the parser"));
    assert_eq!(found.chats[0].cwd.as_deref(), Some(Path::new("/work/api")));
}

#[test]
fn a_codex_rollout_is_read_from_its_response_items() {
    let found = scan(
        &Env::at(&fixture("codex").join("home")),
        Path::new("/nowhere"),
        &[Source::Codex],
    )
    .remove(0);
    let chat = read_chat(&found, &found.chats[0]).unwrap();
    assert_eq!(
        spoken(&chat.turns),
        [
            // What Codex told the model about the environment is no prompt.
            "user: Fix the parser",
            "assistant: <thinking> | <call shell>",
            "result call_a: fn parse() {}\n",
            "assistant: <call apply_patch>",
            "result call_b: Done",
            "assistant: The parser handles empty input now.",
            "compaction keeping 0: The parser was fixed for empty input.",
            "user: and add a test | <image/png>",
            "assistant: Added tests/empty.rs.",
        ]
    );
    let TurnKind::Assistant {
        blocks,
        model,
        usage,
        api,
        ..
    } = &chat.turns[1].kind
    else {
        panic!("a reply");
    };
    let Block::ToolCall { input, .. } = &blocks[1] else {
        panic!("a call");
    };
    assert_eq!(input["command"], serde_json::json!(["cat", "src/parse.rs"]));
    assert_eq!(
        model.clone().unwrap(),
        ("openai".to_string(), "gpt-test-1".to_string())
    );
    assert_eq!(*api, Some(WireApi::OpenAiResponses));
    assert_eq!(
        usage.output, 0,
        "the count belongs to the reply it was reported after"
    );
    let TurnKind::Assistant { usage, .. } = &chat.turns[5].kind else {
        panic!("a reply");
    };
    assert_eq!(
        (usage.input, usage.cache_read, usage.output, usage.reasoning),
        (500, 1000, 80, 20)
    );
    assert_eq!(chat.created, Some("2026-04-05T19:00:00Z".parse().unwrap()));
}

// ---------------------------------------------------------------- whole migrations

#[tokio::test]
async fn a_migration_brings_everything_over_once() {
    let sandbox = Sandbox::new("pi");
    let report = sandbox.migrate(Source::Pi, &everything()).await;
    assert_eq!(report.count(Status::Failed), 0, "{}", report.render());

    // Roles and settings are in the configuration, which loads.
    let config = sandbox.config();
    assert_eq!(
        config.roles["default"][0].to_string(),
        "team-gateway/big-model:high"
    );
    assert_eq!(config.tui.theme, "light");
    assert_eq!(config.compaction.reserve_tokens, 20_000);
    let gateway = &config.providers["team-gateway"];
    assert_eq!(gateway.base_url.as_deref(), Some("https://llm.example.com/v1"));
    assert_eq!(gateway.api_key, None);
    assert_eq!(
        config.models["team-gateway/big-model"].context_window,
        Some(200_000)
    );
    assert_eq!(config.models["ollama/local-coder"].context_window, Some(32_768));

    // Keys are in auth.json; the sign-in is not, and no key is in the report.
    let keys = sandbox.keys();
    assert_eq!(keys.keys().collect::<Vec<_>>(), ["openrouter", "team-gateway"]);
    assert_eq!(
        report.of(Source::Pi, Kind::Keys, Status::Login),
        ["anthropic: sign-ins are not copied; run `azazel login anthropic`"]
    );
    assert!(!report.render().contains("test-key-"), "{}", report.render());

    // Resources of the user are in the home, those of the project in the project.
    assert!(
        sandbox
            .path("azazel/skills/release-notes/references/format.md")
            .is_file()
    );
    assert!(sandbox.path("azazel/commands/review.md").is_file());
    assert!(sandbox.path("azazel/rules/pi-agents.md").is_file());
    assert!(sandbox.path("project/.azazel/skills/deploy/SKILL.md").is_file());
    assert!(sandbox.path("project/.azazel/commands/ship.md").is_file());
    #[cfg(feature = "mcp")]
    {
        let servers = sandbox.json("azazel/mcp.json");
        assert_eq!(servers["mcpServers"]["notes"]["timeout"], 30);
        assert_eq!(
            servers["mcpServers"]["docs"]["url"],
            "https://docs.example.com/mcp"
        );
        let ours = sandbox.json("project/.azazel/mcp.json");
        assert_eq!(ours["mcpServers"]["tickets"]["command"], "tickets-mcp");
    }

    // The conversations are sessions of the project they were held in, under their names and
    // in the order they were last used.
    let sessions = sandbox.sessions("/work/app");
    let names: Vec<&str> = sessions
        .iter()
        .filter_map(|session| session.name.as_deref())
        .collect();
    assert_eq!(
        names,
        ["imported from pi: count to two", "imported from pi: Notes review"]
    );
    assert_eq!(sessions[1].messages, 6);
    // Ids begin with the time of the conversation, not of the import: the short form that
    // lists show and `--session` takes tells one from the other.
    assert_ne!(sessions[0].id[..8], sessions[1].id[..8]);
    assert_eq!(sessions[1].timestamp, "2026-02-16T10:20:30Z");
    assert_eq!(sessions[1].models["team-gateway/big-model"].input, 360);
    let reopened = SessionStore::open(&sessions[0].path).unwrap();
    let context: Vec<String> = reopened
        .build_context(None)
        .iter()
        .map(|entry| entry.message.text())
        .collect();
    // The compaction holds: its summary, the turns it kept, and what came after.
    assert!(context[0].contains("They counted to two."));
    assert_eq!(context[1..], ["go on", "two", "and three?", "three"]);
    assert!(
        report
            .of(Source::Pi, Kind::Chats, Status::Lossy)
            .iter()
            .any(|line| line.contains("2 messages on branches that were left"))
    );
    // The originals are as they were.
    let original = fixture("pi").join("home/.pi/agent/auth.json");
    assert_eq!(
        std::fs::read(sandbox.path("home/.pi/agent/auth.json")).unwrap(),
        std::fs::read(original).unwrap()
    );

    // A second run brings nothing: everything is there.
    let again = sandbox.migrate(Source::Pi, &everything()).await;
    assert_eq!(again.count(Status::Imported), 0, "{}", again.render());
    assert_eq!(again.count(Status::Failed), 0, "{}", again.render());
    assert_eq!(
        again.of(Source::Pi, Kind::Chats, Status::Skipped),
        [
            "0f0f0f0f-0000-4000-8000-000000000001 (/work/app): already imported",
            "0f0f0f0f-0000-4000-8000-000000000002 (/work/app): already imported",
        ]
    );
    assert_eq!(sandbox.sessions("/work/app").len(), 2);
    assert_eq!(sandbox.keys(), keys);
}

#[tokio::test]
async fn every_source_ends_in_a_home_azazel_loads_with_its_conversations_as_sessions() {
    for (source, name, cwd, session) in [
        (Source::Pi, "pi", "/work/app", "imported from pi: count to two"),
        (
            Source::Omp,
            "omp",
            "/work/shop",
            "imported from omp: Fix the cart total",
        ),
        (
            Source::Jcode,
            "jcode",
            "/work/tool",
            "imported from jcode: fix the parser",
        ),
        (
            Source::Claude,
            "claude",
            "/work/site",
            "imported from Claude Code: Navbar and footer",
        ),
        (
            Source::Codex,
            "codex",
            "/work/api",
            "imported from Codex CLI: Fix the parser",
        ),
    ] {
        let sandbox = Sandbox::new(name);
        let report = sandbox.migrate(source, &everything()).await;
        assert_eq!(report.count(Status::Failed), 0, "{name}: {}", report.render());
        // The configuration it wrote loads, with a default model that names a provider there is.
        let config = sandbox.config();
        let lead = config.roles["default"][0].to_string();
        let provider = lead.split('/').next().unwrap().to_string();
        assert!(
            azazel_ai::providers::presets::is_builtin(&provider) || config.providers.contains_key(&provider),
            "{name}: {lead}"
        );
        let sessions = sandbox.sessions(cwd);
        assert_eq!(sessions[0].name.as_deref(), Some(session), "{name}");
        assert!(sessions[0].messages >= 4, "{name}: {}", sessions[0].messages);
        // What the model would be sent when the session is resumed is a conversation: it
        // starts with the user (or the summary that stands for what came before).
        let store = SessionStore::open(&sessions[0].path).unwrap();
        let context = store.build_context(None);
        assert_eq!(context[0].message.role, azazel_ai::Role::User, "{name}");
        // Its tool calls name azazel's tools wherever azazel has one for them.
        for entry in &context {
            for block in &entry.message.content {
                if let Block::ToolCall { name: tool, .. } = block {
                    assert_eq!(super::convert::tool_name(source, tool), None, "{name}: {tool}");
                }
            }
        }
        // And nothing of it is brought a second time.
        let again = sandbox.migrate(source, &everything()).await;
        assert_eq!(again.count(Status::Imported), 0, "{name}: {}", again.render());
    }
}

#[tokio::test]
async fn what_is_there_is_kept_unless_overwriting_is_asked_for() {
    let sandbox = Sandbox::new("pi");
    let home = sandbox.path("azazel");
    std::fs::create_dir_all(home.join("skills/release-notes")).unwrap();
    std::fs::write(home.join("skills/release-notes/SKILL.md"), "mine").unwrap();
    std::fs::write(
        home.join("config.toml"),
        "# my settings\n[roles]\ndefault = [\"ollama/mine\"]\n\n[tui]\ntheme = \"dark\"\nqueue_mode = \"all\"\n",
    )
    .unwrap();
    std::fs::write(
        home.join("models.yml"),
        "providers:\n  team-gateway:   # mine\n    api: openai-chat\n    base_url: https://mine.example/v1\n",
    )
    .unwrap();
    std::fs::write(
        home.join("auth.json"),
        r#"{"openrouter":{"type":"api_key","key":"test-key-mine"},"team-gateway":{"type":"oauth","access":"a","refresh":"r","expires":1}}"#,
    )
    .unwrap();
    #[cfg(feature = "mcp")]
    std::fs::write(
        home.join("mcp.json"),
        r#"{"mcpServers":{"notes":{"command":"mine"}}}"#,
    )
    .unwrap();

    let report = sandbox.migrate(Source::Pi, &everything()).await;
    let skipped = |kind: Kind| report.of(Source::Pi, kind, Status::Skipped).join("\n");
    assert!(
        skipped(Kind::Models).contains("roles.default is already [\"ollama/mine\"]"),
        "{}",
        report.render()
    );
    assert!(skipped(Kind::Models).contains("`team-gateway` is already in models.yml"));
    assert!(skipped(Kind::Settings).contains("tui.theme is already dark"));
    // The same value is no conflict.
    assert!(skipped(Kind::Settings).contains("tui.queue_mode = all: already so"));
    assert!(skipped(Kind::Keys).contains("openrouter: another key is stored"));
    assert!(skipped(Kind::Keys).contains("team-gateway: a sign-in is stored for it and is kept"));
    assert!(skipped(Kind::Skills).contains("already there"));
    let config = std::fs::read_to_string(home.join("config.toml")).unwrap();
    assert!(
        config.starts_with("# my settings\n[roles]\ndefault = [\"ollama/mine\"]"),
        "{config}"
    );
    assert!(
        config.contains("theme = \"dark\"") && config.contains("show_thinking = true"),
        "{config}"
    );
    assert!(
        std::fs::read_to_string(home.join("models.yml"))
            .unwrap()
            .contains("# mine")
    );
    assert_eq!(
        std::fs::read_to_string(home.join("skills/release-notes/SKILL.md")).unwrap(),
        "mine"
    );
    assert!(matches!(&sandbox.keys()["openrouter"], Credential::ApiKey { key } if key == "test-key-mine"));
    #[cfg(feature = "mcp")]
    assert_eq!(
        sandbox.json("azazel/mcp.json")["mcpServers"]["notes"]["command"],
        "mine"
    );

    let options = Options::new(Kind::ALL.to_vec(), false, true);
    let replaced = sandbox.migrate(Source::Pi, &options).await;
    assert_eq!(replaced.count(Status::Failed), 0, "{}", replaced.render());
    let config = sandbox.config();
    assert_eq!(
        config.roles["default"][0].to_string(),
        "team-gateway/big-model:high"
    );
    assert_eq!(config.tui.theme, "light");
    assert_eq!(
        config.providers["team-gateway"].base_url.as_deref(),
        Some("https://llm.example.com/v1")
    );
    assert!(
        std::fs::read_to_string(home.join("skills/release-notes/SKILL.md"))
            .unwrap()
            .contains("name: release-notes")
    );
    assert!(
        matches!(&sandbox.keys()["openrouter"], Credential::ApiKey { key } if key == "test-key-openrouter")
    );
    // A sign-in is never traded for a key, whatever was asked.
    assert!(matches!(
        &sandbox.keys()["team-gateway"],
        Credential::OAuth { .. }
    ));
    #[cfg(feature = "mcp")]
    assert_eq!(
        sandbox.json("azazel/mcp.json")["mcpServers"]["notes"]["command"],
        "npx"
    );
}

#[tokio::test]
async fn a_dry_run_says_what_a_run_would_do_and_writes_nothing() {
    let sandbox = Sandbox::new("pi");
    let dry = sandbox
        .migrate(Source::Pi, &Options::new(Kind::ALL.to_vec(), true, false))
        .await;
    assert!(!sandbox.path("azazel").exists(), "a dry run made the home");
    assert!(!sandbox.path("project/.azazel").exists());
    assert!(dry.render().contains("would import"), "{}", dry.render());
    assert!(
        dry.summary().ends_with("nothing was written"),
        "{}",
        dry.summary()
    );

    let real = sandbox.migrate(Source::Pi, &everything()).await;
    // But for the conversations, which a dry run does not read, the two say the same.
    for kind in Kind::ALL.into_iter().filter(|kind| *kind != Kind::Chats) {
        for status in [Status::Imported, Status::Skipped, Status::Login, Status::Failed] {
            assert_eq!(
                dry.of(Source::Pi, kind, status),
                real.of(Source::Pi, kind, status),
                "{kind:?} {status:?}"
            );
        }
    }
    assert_eq!(
        dry.of(Source::Pi, Kind::Chats, Status::Imported),
        [
            "0f0f0f0f-0000-4000-8000-000000000001 (/work/app)",
            "0f0f0f0f-0000-4000-8000-000000000002 (/work/app)",
        ]
    );
}

#[tokio::test]
async fn skills_agents_and_commands_with_the_name_of_a_built_in_are_left_out() {
    let sandbox = Sandbox::new("omp");
    let agent = sandbox.path("home/.omp/agent");
    std::fs::create_dir_all(agent.join("skills/code-review")).unwrap();
    std::fs::write(
        agent.join("skills/code-review/SKILL.md"),
        "---\nname: code-review\ndescription: Reviews code\n---\nReview it.\n",
    )
    .unwrap();
    // An agent is known by the name its definition gives, whatever its file is called.
    std::fs::write(
        agent.join("agents/explorer.md"),
        "---\nname: scout\ndescription: Looks around\n---\nLook around.\n",
    )
    .unwrap();
    std::fs::write(agent.join("commands/model.md"), "Pick a model.\n").unwrap();

    let report = sandbox.migrate(Source::Omp, &everything()).await;
    let skipped = |kind| report.of(Source::Omp, kind, Status::Skipped);
    let built_in = |kind| {
        skipped(kind)
            .into_iter()
            .filter(|line| line.ends_with("built-in, azazel has its own"))
            .count()
    };
    assert_eq!(built_in(Kind::Skills), 1, "{}", report.render());
    assert_eq!(built_in(Kind::Agents), 1, "{}", report.render());
    assert_eq!(built_in(Kind::Commands), 1, "{}", report.render());
    let home = sandbox.path("azazel");
    assert!(!home.join("skills/code-review").exists());
    assert!(!home.join("agents/explorer.md").exists());
    assert!(!home.join("commands/model.md").exists());
    // The others come over as before.
    assert!(home.join("agents/auditor.md").is_file());
    assert!(home.join("commands/standup.md").is_file());
}

#[tokio::test]
async fn a_value_azazel_would_refuse_never_reaches_its_configuration() {
    let sandbox = Sandbox::new("pi");
    let home = sandbox.path("azazel");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[tui]\ntheme = \"dark\"\n").unwrap();
    let mut found = Found::new(Source::Pi, Path::new("/source"));
    found.settings.push(Setting {
        key: vec!["permissions".to_string(), "mode".to_string()],
        value: toml::Value::String("reckless".to_string()),
        from: "a setting".to_string(),
    });
    found.settings.push(Setting {
        key: vec!["team".to_string(), "max_parallel".to_string()],
        value: toml::Value::Integer(7),
        from: "another".to_string(),
    });
    let report = migrate(&sandbox.paths(), &[found], &everything()).await;
    let failed = report.of(Source::Pi, Kind::Settings, Status::Failed);
    assert!(failed[0].contains("unknown mode `reckless`"), "{failed:?}");
    // The file is as it was but for the value that is fine.
    let config = sandbox.config();
    assert_eq!(config.permissions.mode, azazel_core::PermissionMode::Manual);
    assert_eq!(config.team.max_parallel, 7);
    assert_eq!(config.tui.theme, "dark");
}

#[tokio::test]
async fn old_conversations_a_retention_setting_would_delete_are_pointed_out() {
    let sandbox = Sandbox::new("pi");
    let home = sandbox.path("azazel");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(home.join("config.toml"), "[session]\nretention_days = 30\n").unwrap();
    let report = sandbox
        .migrate(Source::Pi, &Options::new(vec![Kind::Chats], false, false))
        .await;
    let warned = report.of(Source::Pi, Kind::Chats, Status::Lossy).join("\n");
    assert!(
        warned.contains("2 of the imported conversations were last used more than 30 days ago"),
        "{warned}"
    );
    // The listing of the project's sessions is ready for the first `/resume`.
    let home = home.to_string_lossy().into_owned();
    let dir = Paths::discover(Path::new("/work/app"), Some(&home)).sessions_dir();
    assert!(dir.join("index.json").is_file());
}

#[tokio::test]
async fn claude_rules_join_those_that_are_there_and_project_servers_go_to_the_project() {
    let sandbox = Sandbox::new("claude");
    // Claude Code keeps a project's servers under the project's path in its state file.
    let state = sandbox.path("home/.claude.json");
    let text = std::fs::read_to_string(&state).unwrap();
    let project = serde_json::to_string(&sandbox.project()).unwrap();
    std::fs::write(&state, text.replace("\"/work/site\"", &project)).unwrap();
    let home = sandbox.path("azazel");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::write(
        home.join("config.toml"),
        "[permissions]\nallow = [\"shell(cargo test:*)\", \"edit(src/**)\"]\n",
    )
    .unwrap();

    let report = sandbox.migrate(Source::Claude, &everything()).await;
    assert_eq!(report.count(Status::Failed), 0, "{}", report.render());
    let config = sandbox.config();
    let allowed: Vec<String> = config.permissions.allow.iter().map(ToString::to_string).collect();
    assert_eq!(
        allowed[..3],
        ["shell(cargo test:*)", "edit(src/**)", "shell(npm run test:*)"]
    );
    assert_eq!(allowed.iter().filter(|rule| *rule == "edit(src/**)").count(), 1);
    assert_eq!(config.permissions.mode, azazel_core::PermissionMode::AcceptEdits);
    assert_eq!(config.session.retention_days, 45);
    assert_eq!(
        config.providers["anthropic"].api_key_command.as_deref(),
        Some("pass show anthropic")
    );
    #[cfg(feature = "mcp")]
    {
        // `${env:NAME}` is written the way azazel fills it in.
        let servers = sandbox.json("azazel/mcp.json");
        assert_eq!(
            servers["mcpServers"]["search"]["headers"]["X-Key"],
            "${SEARCH_KEY}"
        );
        let ours = sandbox.json("project/.azazel/mcp.json");
        assert_eq!(ours["mcpServers"]["db"]["command"], "npx");
    }
    // Agent definitions azazel can load, in its own words.
    let reviewer = std::fs::read_to_string(home.join("agents/reviewer.md")).unwrap();
    let parsed = azazel_core::agents::parse_def(
        Path::new("reviewer.md"),
        &reviewer,
        azazel_core::agents::DefSource::Home,
    )
    .unwrap();
    assert_eq!(parsed.name, "change-reviewer");
    assert!(parsed.plan_mode);
    assert_eq!(parsed.max_turns, Some(20));
    assert!(
        report
            .of(Source::Claude, Kind::Skills, Status::InPlace)
            .iter()
            .any(|line| line.contains("azazel reads it where it is"))
    );

    // The sub-agent's conversation is the session's agent transcript.
    let sessions = sandbox.sessions("/work/site");
    assert_eq!(
        sessions[0].name.as_deref(),
        Some("imported from Claude Code: Navbar and footer")
    );
    let store = SessionStore::open(&sessions[0].path).unwrap();
    let agents = store.facts().agents;
    assert_eq!(agents.len(), 1);
    assert_eq!(SessionStore::branch_of(&agents[0].transcript).unwrap().len(), 2);
    assert!(
        store
            .entries()
            .iter()
            .any(|entry| matches!(entry.kind, EntryKind::Compaction { .. }))
    );
}

#[test]
fn a_source_is_looked_for_where_its_own_variables_point() {
    let home = Path::new("/home/dev");
    let plain = Env::at(home);
    assert_eq!(root_of(Source::Pi, &plain), home.join(".pi/agent"));
    assert_eq!(root_of(Source::Omp, &plain), home.join(".omp/agent"));
    assert_eq!(root_of(Source::Codex, &plain), home.join(".codex"));
    let moved = Env::at(home)
        .with_var("CODEX_HOME", "/data/codex")
        .with_var("CLAUDE_CONFIG_DIR", "/data/claude")
        .with_var("JCODE_HOME", "/data/jcode")
        .with_var("PI_CONFIG_DIR", ".ohmy");
    assert_eq!(root_of(Source::Codex, &moved), Path::new("/data/codex"));
    assert_eq!(root_of(Source::Claude, &moved), Path::new("/data/claude"));
    assert_eq!(root_of(Source::Jcode, &moved), Path::new("/data/jcode"));
    assert_eq!(root_of(Source::Omp, &moved), home.join(".ohmy/agent"));
    // pi and omp read the same variable; the directory says whose it is.
    let shared = fixture("omp").join("home/.omp/agent");
    let pointed = Env::at(home).with_var("PI_CODING_AGENT_DIR", &shared.to_string_lossy());
    assert_eq!(root_of(Source::Omp, &pointed), shared);
    assert_eq!(root_of(Source::Pi, &pointed), home.join(".pi/agent"));
}
