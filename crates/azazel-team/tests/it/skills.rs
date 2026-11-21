//! A skill that runs in its own context starts a sub-agent, and its `allowed-tools` is all that
//! agent gets.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use azazel_agent::{
    Agent, AgentConfig, AgentEvent, AgentHandle, AgentInfo, CompactionSettings, Limits, Services, UserInput,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{Block, Catalog, Provider, RetryPolicy, Router};
use azazel_core::agents::{DefSource, discover};
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::process::Shell;
use azazel_core::skills::SkillsExt;
use azazel_core::{
    Config, ExtCtx, Headless, LoadOptions, Paths, PermissionGate, PermissionState, Registry, SessionStore,
    SkillRoot, SkillSource, Ui,
};
use azazel_team::{BUILTIN_AGENTS, Team, TeamDeps, TeamExt};
use azazel_tools::CoreToolsExt;
use serde_json::json;

/// May read and write; the skills below narrow that.
const AUDITOR: &str = "---\nname: auditor\ndescription: Audits files.\nmodel: mock/auditor\n\
                       tools: read, write\n---\nYou audit.\n";

struct Bench {
    dir: tempfile::TempDir,
    mock: Arc<MockProvider>,
    lead: AgentHandle,
}

impl Bench {
    /// A project in `auto` mode with the `auditor` definition and one skill, `audit`, whose
    /// frontmatter ends with `skill_keys`.
    async fn new(skill_keys: &str) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let agents = project.join(".azazel/agents");
        let skills = project.join(".azazel/skills");
        std::fs::create_dir_all(&agents).unwrap();
        std::fs::create_dir_all(skills.join("audit")).unwrap();
        std::fs::write(agents.join("auditor.md"), AUDITOR).unwrap();
        std::fs::write(
            skills.join("audit/SKILL.md"),
            format!("---\nname: audit\ndescription: Audit the notes.\n{skill_keys}\n---\nAudit f.txt.\n"),
        )
        .unwrap();
        std::fs::write(project.join("f.txt"), "one\n").unwrap();
        let paths = Paths::discover(&project, dir.path().join("home").to_str());

        let mut overrides = toml::Table::new();
        overrides.insert(
            "permissions".into(),
            toml::Value::Table(toml::Table::from_iter([("mode".to_string(), "auto".into())])),
        );
        let loaded = Config::load(
            &paths,
            &LoadOptions {
                trusted: true,
                overrides,
                env: Vec::new(),
            },
        )
        .unwrap();
        let permissions = Arc::new(PermissionState::new(&paths, &loaded).unwrap());

        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
        let registry = Arc::new(Registry::new(ExtCtx {
            services: services.clone(),
            cwd: paths.cwd.clone(),
            session_id: String::new(),
        }));
        let shell = Shell::resolve("");
        registry
            .load(&CoreToolsExt {
                shell: shell.clone(),
                shell_timeout: Duration::from_secs(30),
                artifacts_dir: paths.artifacts_dir(),
                read_guard: Default::default(),
                patch: Default::default(),
            })
            .unwrap();
        registry
            .load(&PermissionsExt {
                state: permissions.clone(),
            })
            .unwrap();
        registry
            .load(&SkillsExt {
                roots: vec![SkillRoot::new(skills, SkillSource::Project)],
                trusted: true,
            })
            .unwrap();
        let defs = Arc::new(discover(BUILTIN_AGENTS, &[(agents, DefSource::Project)]));
        assert_eq!(defs.invalid(), []);
        registry
            .load(&TeamExt {
                defs: defs.clone(),
                max_depth: 2,
            })
            .unwrap();

        let mock = Arc::new(MockProvider::new());
        let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
        let router = Arc::new(Router::new(
            providers,
            Catalog::default(),
            BTreeMap::new(),
            RetryPolicy {
                max_attempts: 1,
                ..RetryPolicy::default()
            },
        ));
        let team = Arc::new(Team::new(TeamDeps {
            tools: registry.tools.clone(),
            hooks: registry.agent_hooks(),
            prompt: registry.prompt.clone(),
            bus: registry.bus.clone(),
            services: services.clone(),
            router: router.clone(),
            permissions: permissions.clone(),
            paths: paths.clone(),
            defs,
            max_parallel: 2,
            max_depth: 2,
            wait: Duration::ZERO,
            max_turns: 0,
            max_result_bytes: 50_000,
            compaction: CompactionSettings::default(),
            posix_shell: shell.is_posix(),
            routing: Default::default(),
        }));
        services.insert(team.clone());

        let session = Arc::new(SessionStore::create(&paths.sessions_dir(), &paths.cwd, None).unwrap());
        registry.bus.set_session(&session.id());
        let info = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: session.id(),
        };
        let gate = PermissionGate::new(
            permissions,
            registry.bus.clone(),
            services.clone(),
            shell.is_posix(),
        );
        let lead = Agent::spawn(AgentConfig {
            system: registry.prompt.render(&info).await,
            info,
            tools: registry.tools.clone(),
            chain: vec!["mock/lead".parse().unwrap()],
            thinking: None,
            router,
            gate: Arc::new(gate),
            bus: registry.agent_hooks(),
            services,
            cwd: paths.cwd.clone(),
            initial_messages: Vec::new(),
            sink: session.clone(),
            artifacts: None,
            limits: Limits::default(),
            compaction: CompactionSettings::default(),
        });
        team.attach(lead.clone(), session);
        Bench { dir, mock, lead }
    }

    fn file(&self, name: &str) -> PathBuf {
        self.dir.path().join("project").join(name)
    }

    /// Has the lead load the `audit` skill, with an auditor that tries to write a file and then
    /// reads one. Returns the skill call's result.
    async fn audit(&self) -> String {
        self.mock.push(
            "lead",
            MockStep::tools("", &[("s1", "skill", json!({ "name": "audit" }))]),
        );
        self.mock.push("lead", MockStep::text("done"));
        self.mock.push(
            "auditor",
            MockStep::tools(
                "",
                &[
                    ("w1", "write", json!({ "path": "out.txt", "content": "x" })),
                    ("r1", "read", json!({ "path": "f.txt" })),
                ],
            ),
        );
        self.mock.push("auditor", MockStep::text("audited"));

        let mut events = self.lead.subscribe();
        assert!(self.lead.prompt(UserInput::text("audit it")));
        let mut result = None;
        loop {
            let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
                .await
                .unwrap()
                .unwrap();
            match event {
                AgentEvent::ToolEnd { output, .. } => result = Some(output.text_content()),
                AgentEvent::Idle { .. } => break,
                AgentEvent::Failed { error } => panic!("the lead failed: {error}"),
                _ => {}
            }
        }
        result.unwrap()
    }

    /// The tools the auditor's model was offered, and the results its two calls got.
    fn auditor_saw(&self) -> (Vec<String>, Vec<String>) {
        let requests = self.mock.requests_for("auditor");
        assert_eq!(requests.len(), 2);
        let offered = requests[0].tools.iter().map(|tool| tool.name.clone()).collect();
        let results = requests[1]
            .messages
            .iter()
            .flat_map(|message| &message.content)
            .filter_map(|block| match block {
                Block::ToolResult { content, .. } => Some(
                    content
                        .iter()
                        .filter_map(|inner| match inner {
                            Block::Text { text } => Some(text.as_str()),
                            _ => None,
                        })
                        .collect::<String>(),
                ),
                _ => None,
            })
            .collect();
        (offered, results)
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forked_skill_with_allowed_tools_read_cannot_call_write() {
    let bench = Bench::new("context: fork\nagent: auditor\nallowed-tools: read").await;
    assert_eq!(bench.audit().await, "audited");

    let (offered, results) = bench.auditor_saw();
    assert!(offered.contains(&"read".to_string()), "{offered:?}");
    assert!(!offered.contains(&"write".to_string()), "{offered:?}");
    assert_eq!(results, ["unknown tool `write`", "1\tone"]);
    assert!(!bench.file("out.txt").exists());
    bench.lead.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_forked_skill_without_allowed_tools_keeps_what_the_definition_grants() {
    let bench = Bench::new("context: fork\nagent: auditor").await;
    assert_eq!(bench.audit().await, "audited");

    let (offered, results) = bench.auditor_saw();
    assert!(offered.contains(&"write".to_string()), "{offered:?}");
    assert_eq!(results, ["created out.txt (1 bytes)", "1\tone"]);
    assert_eq!(std::fs::read_to_string(bench.file("out.txt")).unwrap(), "x");
    bench.lead.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn allowed_tools_cannot_widen_what_the_definition_grants() {
    // The definition grants `read` and `write`; a skill that lists `shell` adds nothing.
    let bench = Bench::new("context: fork\nagent: auditor\nallowed-tools: read, shell").await;
    assert_eq!(bench.audit().await, "audited");
    let (offered, _) = bench.auditor_saw();
    assert!(offered.contains(&"read".to_string()), "{offered:?}");
    assert!(
        !offered.contains(&"shell".to_string()) && !offered.contains(&"write".to_string()),
        "{offered:?}"
    );
    bench.lead.shutdown().await;
}
