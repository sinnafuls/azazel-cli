#![allow(clippy::unwrap_used)]

//! `run_code` scripts and `azazel.tools.call`: tool calls made for an agent pass the permission
//! gate, and a script stays inside its limits.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{
    Agent, AgentConfig, AgentInfo, CompactionSettings, Gate, GateDecision, Limits, Services, Tool, ToolCtx,
    ToolOutput, ToolSource, UpdateSink,
};
use azazel_ai::testing::MockProvider;
use azazel_ai::{Catalog, Credentials, Provider, RetryPolicy, Router, ThinkingLevel};
use azazel_core::agents::discover;
use azazel_core::config::set_override;
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::process::Shell;
use azazel_core::{
    Config, ExtCtx, Headless, LoadOptions, Paths, PermissionGate, PermissionState, Registry, SessionStore,
    ToolCaller, Ui,
};
use azazel_lua::{CodeExt, CodeLimits, LuaExt};
use azazel_team::{BUILTIN_AGENTS, Team, TeamDeps};
use azazel_tools::CoreToolsExt;
use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

fn lead() -> AgentInfo {
    AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s1".into(),
    }
}

/// The permission gate, noting every call that reaches it as `<tool> <call id>`.
struct Watched {
    gate: PermissionGate,
    seen: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl Gate for Watched {
    async fn before(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, input: &Value) -> GateDecision {
        self.seen
            .lock()
            .unwrap()
            .push(format!("{} {call_id}", tool.name()));
        self.gate.before(agent, tool, call_id, input).await
    }

    async fn after(&self, agent: &AgentInfo, tool: &dyn Tool, call_id: &str, output: &mut ToolOutput) {
        self.gate.after(agent, tool, call_id, output).await
    }
}

/// A project with the core tools, `run_code` and the real permission gate in `mode`, with nobody
/// to ask.
struct Bench {
    _dir: tempfile::TempDir,
    paths: Paths,
    services: Arc<Services>,
    registry: Arc<Registry>,
    permissions: Arc<PermissionState>,
    seen: Arc<Mutex<Vec<String>>>,
}

impl Bench {
    fn new(mode: &str, limits: CodeLimits) -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let paths = Paths::discover(&project, home.to_str());
        let mut overrides = Default::default();
        set_override(&mut overrides, &format!("permissions.mode={mode}")).unwrap();
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
            session_id: "s1".into(),
        }));
        registry.publish();
        let shell = Shell::resolve("");
        registry
            .load(&CoreToolsExt {
                shell: shell.clone(),
                shell_timeout: Duration::from_secs(30),
                artifacts_dir: paths.artifacts_dir(),
            })
            .unwrap();
        registry
            .load(&PermissionsExt {
                state: permissions.clone(),
            })
            .unwrap();
        registry.load(&CodeExt { limits }).unwrap();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let gate = Watched {
            gate: PermissionGate::new(
                permissions.clone(),
                registry.bus.clone(),
                services.clone(),
                shell.is_posix(),
            ),
            seen: seen.clone(),
        };
        services.insert(Arc::new(ToolCaller::new(registry.tools.clone(), Arc::new(gate))));
        Bench {
            _dir: dir,
            paths,
            services,
            registry,
            permissions,
            seen,
        }
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.paths.project_root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn ctx(&self, agent: AgentInfo) -> ToolCtx {
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: self.paths.cwd.clone(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        }
    }

    async fn call(&self, tool: &str, input: Value) -> ToolOutput {
        let ctx = self.ctx(lead());
        let tool = self.registry.tools.get(&ctx.agent, tool).unwrap();
        tool.call(input, &ctx).await
    }

    async fn run(&self, code: &str) -> ToolOutput {
        self.call("run_code", json!({ "code": code })).await
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_reads_two_files_in_one_call_and_returns_one_result() {
    let bench = Bench::new("ask", CodeLimits::default());
    bench.write("a.txt", "one\ntwo\nthree\n");
    bench.write("src/b.txt", "only\n");
    let output = bench
        .run(
            r#"
            local counts = {}
            for _, path in ipairs({ "a.txt", "src/b.txt" }) do
                local text = tools.read({ path = path })
                local lines = 0
                for _ in string.gmatch(text, "[^\n]+") do lines += 1 end
                table.insert(counts, path .. ": " .. lines)
            end
            return table.concat(counts, ", ")
            "#,
        )
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    assert_eq!(output.text_content(), "a.txt: 3, src/b.txt: 1");
    assert_eq!(output.details, Some(json!({ "tool_calls": 2 })));
    // Each call went through the gate under an id of its own.
    assert_eq!(bench.seen(), ["read c1.1", "read c1.2"]);
}

#[tokio::test(flavor = "multi_thread")]
async fn printed_lines_come_before_the_returned_value_and_tables_return_as_json() {
    let bench = Bench::new("ask", CodeLimits::default());
    let output = bench
        .run(
            r#"
            print("checking", 2, true)
            local decoded = json.decode('{"n": 3}')
            return { total = decoded.n, names = { "a", "b" } }, "done"
            "#,
        )
        .await;
    assert!(!output.is_error, "{}", output.text_content());
    let text = output.text_content();
    assert!(text.starts_with("checking\t2\ttrue\n{"), "{text}");
    assert!(text.ends_with("}\ndone"), "{text}");
    let json: Value =
        serde_json::from_str(&text["checking\t2\ttrue\n".len()..text.len() - "\ndone".len()]).unwrap();
    assert_eq!(json, json!({"total": 3, "names": ["a", "b"]}));

    let silent = bench.run("local x = 1").await;
    assert_eq!(silent.text_content(), "(the script printed and returned nothing)");

    // Nothing in the VM reaches the disk or a process: files go through `tools`.
    let reach = bench
        .run("return type(require) .. ' ' .. type(io) .. ' ' .. type(os.execute) .. ' ' .. type(azazel)")
        .await;
    assert_eq!(reach.text_content(), "nil nil nil nil");

    // What was printed before a failure is kept, and a syntax error names the line.
    let failed = bench.run("print('step 1')\nerror('no such thing')").await;
    assert!(failed.is_error);
    assert_eq!(
        failed.text_content(),
        "step 1\nscript error: run_code:2: no such thing"
    );
    let broken = bench.run("local = 3").await;
    assert!(broken.is_error);
    assert!(
        broken.text_content().starts_with("run_code:1: "),
        "{}",
        broken.text_content()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_in_ask_mode_with_nobody_to_ask_is_refused_by_the_gate() {
    let bench = Bench::new("ask", CodeLimits::default());
    let marker = bench.paths.project_root.join("ran.txt");
    let command = format!("echo ran > \"{}\"", marker.display());
    let refused = bench
        .run(&format!("return tools.shell({{ command = {command:?} }})"))
        .await;
    assert!(refused.is_error);
    assert_eq!(
        refused.text_content(),
        "script error: run_code:1: shell: permission required for shell: re-run with --mode auto or add an \
         allow rule"
    );
    assert!(!marker.exists());
    assert_eq!(bench.seen(), ["shell c1.1"]);

    // A script can catch the refusal and go on; writing is refused the same way.
    let caught = bench
        .run(
            r#"
            local ok, why = pcall(tools.write, { path = "new.txt", content = "x" })
            return tostring(ok) .. ": " .. tostring(why)
            "#,
        )
        .await;
    assert!(!caught.is_error, "{}", caught.text_content());
    assert!(
        caught
            .text_content()
            .starts_with("false: write: permission required for write"),
        "{}",
        caught.text_content()
    );
    assert!(!bench.paths.project_root.join("new.txt").exists());

    // The same calls run once the mode allows them.
    bench.permissions.set_mode(azazel_core::PermissionMode::Auto);
    let allowed = bench
        .run(r#"return tools.write({ path = "new.txt", content = "x" })"#)
        .await;
    assert_eq!(allowed.text_content(), "created new.txt (1 bytes)");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_loop_stops_at_the_time_limit_while_waiting_for_tools_does_not_count() {
    let limits = CodeLimits {
        script_time: Duration::from_millis(300),
        ..CodeLimits::default()
    };
    let bench = Bench::new("auto", limits);
    let started = std::time::Instant::now();
    let stuck = bench.run("print('before')\nwhile true do end").await;
    assert!(stuck.is_error);
    assert_eq!(
        stuck.text_content(),
        "before\nscript error: the script used its 300 ms of running time without finishing"
    );
    assert!(started.elapsed() < Duration::from_secs(20));

    // A command that takes longer than the script's whole budget is waited for.
    let sleep = if Shell::resolve("").is_posix() {
        "sleep 1.2 && echo slept"
    } else {
        "Start-Sleep -Milliseconds 1200; echo slept"
    };
    let started = std::time::Instant::now();
    let waited = bench
        .run(&format!("return tools.shell({{ command = {sleep:?} }})"))
        .await;
    assert!(!waited.is_error, "{}", waited.text_content());
    assert!(
        waited.text_content().contains("slept"),
        "{}",
        waited.text_content()
    );
    assert!(started.elapsed() >= Duration::from_secs(1));
}

#[tokio::test(flavor = "multi_thread")]
async fn the_fifty_first_tool_call_fails_and_some_tools_cannot_be_called_at_all() {
    let bench = Bench::new("ask", CodeLimits::default());
    bench.write("a.txt", "one\n");
    let output = bench
        .run(
            r#"
            local done = 0
            for i = 1, 60 do
                local ok, why = pcall(tools.read, { path = "a.txt" })
                if not ok then
                    return "call " .. i .. " failed: " .. why
                end
                done += 1
            end
            return "all " .. done
            "#,
        )
        .await;
    assert_eq!(
        output.text_content(),
        "call 51 failed: the limit of 50 tool calls per script is reached"
    );
    assert_eq!(bench.seen().len(), 50);
    assert_eq!(output.details, Some(json!({ "tool_calls": 50 })));

    for name in ["run_code", "task", "agent_send"] {
        let refused = bench.run(&format!("return tools.{name}({{}})")).await;
        assert!(refused.is_error);
        assert_eq!(
            refused.text_content(),
            format!("script error: run_code:1: `{name}` cannot be called from a script")
        );
    }
    let unknown = bench.run("return tools.nope({})").await;
    assert_eq!(
        unknown.text_content(),
        "script error: run_code:1: nope: unknown tool `nope`"
    );
    let unnamed = bench.run("return tools.read('a.txt')").await;
    assert!(
        unnamed
            .text_content()
            .contains("tools.read: the arguments are a string; pass one table of named values"),
        "{}",
        unnamed.text_content()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_long_result_is_cut_and_kept_whole_as_an_artifact() {
    let limits = CodeLimits {
        result_bytes: 1000,
        ..CodeLimits::default()
    };
    let bench = Bench::new("ask", limits);
    let output = bench.run("return string.rep('x', 5000)").await;
    let text = output.text_content();
    assert!(
        text.ends_with("\n[result cut: 5000 bytes in total] full result: artifact://c1-code"),
        "{}",
        &text[text.len() - 120..]
    );
    assert_eq!(text.chars().filter(|c| *c == 'x').count(), 1000);
    let whole = bench.call("read", json!({ "path": "artifact://c1-code" })).await;
    assert_eq!(whole.text_content().len(), 5000);
}

#[tokio::test(flavor = "multi_thread")]
async fn the_description_lists_the_tools_a_script_can_call() {
    let bench = Bench::new("ask", CodeLimits::default());
    let tool = bench.registry.tools.get(&lead(), "run_code").unwrap();
    let description = tool.description();
    assert!(description.starts_with("Run a Luau script that calls your other tools"));
    assert!(
        description.contains("\n- read{ path: string, offset?: integer, limit?: integer, pages?: string }: "),
        "{description}"
    );
    assert!(description.contains("\n- shell{ "), "{description}");
    assert!(!description.contains("\n- run_code{"), "{description}");
    // The text is built once: a request sent later carries the very same description.
    assert!(std::ptr::eq(description, tool.description()));
}

/// A session with a team and a lead agent, which `azazel.tools.call` acts for.
async fn with_lead(bench: &Bench) -> azazel_agent::AgentHandle {
    let providers: Vec<Arc<dyn Provider>> = vec![Arc::new(MockProvider::new())];
    let router = Arc::new(Router::new(
        providers,
        Catalog::default(),
        BTreeMap::new(),
        RetryPolicy::default(),
    ));
    let shell = Shell::resolve("");
    let team = Arc::new(Team::new(TeamDeps {
        tools: bench.registry.tools.clone(),
        hooks: bench.registry.agent_hooks(),
        prompt: bench.registry.prompt.clone(),
        bus: bench.registry.bus.clone(),
        services: bench.services.clone(),
        router: router.clone(),
        permissions: bench.permissions.clone(),
        paths: bench.paths.clone(),
        defs: Arc::new(discover(BUILTIN_AGENTS, &[])),
        max_parallel: 2,
        max_depth: 2,
        max_result_bytes: 50_000,
        compaction: CompactionSettings::default(),
        posix_shell: shell.is_posix(),
    }));
    bench.services.insert(team.clone());
    let session =
        Arc::new(SessionStore::create(&bench.paths.sessions_dir(), &bench.paths.cwd, None).unwrap());
    let agent = Agent::spawn(AgentConfig {
        info: lead(),
        system: Vec::new(),
        tools: bench.registry.tools.clone(),
        chain: vec!["mock/lead".parse().unwrap()],
        thinking: ThinkingLevel::Off,
        router,
        gate: Arc::new(azazel_agent::AllowAll),
        bus: bench.registry.agent_hooks(),
        services: bench.services.clone(),
        cwd: bench.paths.cwd.clone(),
        initial_messages: Vec::new(),
        sink: session.clone(),
        artifacts: None,
        limits: Limits::default(),
        compaction: CompactionSettings::default(),
    });
    team.attach(agent.clone(), session);
    agent
}

fn home_script(bench: &Bench, name: &str, source: &str) {
    let path = bench.paths.home.join("extensions").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn load_scripts(bench: &Bench) {
    let credentials = Arc::new(Credentials::new(bench.paths.auth_file()));
    let ext = LuaExt::new(bench.paths.clone(), false, credentials, Vec::new(), Vec::new());
    bench.registry.load(&ext).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_calls_harness_tools_as_the_lead_through_the_gate() {
    let bench = Bench::new("ask", CodeLimits::default());
    bench.write("notes.txt", "one\ntwo\n");
    home_script(
        &bench,
        "counter.luau",
        r#"
        azazel.tool({
            name = "line_count",
            description = "Counts the lines of a file.",
            access = "read",
            parameters = { type = "object", properties = { path = { type = "string" } }, required = { "path" } },
            execute = function(args)
                local text = azazel.tools.call("read", { path = args.path })
                local lines = 0
                for _ in string.gmatch(text, "[^\n]+") do lines += 1 end
                return tostring(lines)
            end,
        })
        azazel.tool({
            name = "sneaky",
            description = "Runs a command.",
            access = "read",
            execute = function()
                return azazel.tools.call("shell", { command = "echo hi" })
            end,
        })
        "#,
    );
    load_scripts(&bench);

    // Without a session there is no agent to act for.
    let early = bench.call("line_count", json!({ "path": "notes.txt" })).await;
    assert!(early.is_error);
    assert!(
        early
            .text_content()
            .contains("azazel.tools.call: no session with a team of agents is running"),
        "{}",
        early.text_content()
    );

    let agent = with_lead(&bench).await;
    let counted = bench.call("line_count", json!({ "path": "notes.txt" })).await;
    assert!(!counted.is_error, "{}", counted.text_content());
    assert_eq!(counted.text_content(), "2");
    let seen = bench.seen();
    assert_eq!(seen.len(), 1);
    assert!(seen[0].starts_with("read lua-counter-"), "{seen:?}");

    // The extension's own access level does not excuse what it calls.
    let refused = bench.call("sneaky", json!({})).await;
    assert!(refused.is_error);
    assert!(
        refused
            .text_content()
            .contains("azazel.tools.call: shell: permission required for shell"),
        "{}",
        refused.text_content()
    );
    agent.shutdown().await;
}

#[test]
fn the_tool_is_registered_under_its_own_extension() {
    let bench = Bench::new("ask", CodeLimits::default());
    assert!(bench.registry.extensions().contains(&"code".to_string()));
    let names: Vec<String> = bench
        .registry
        .tools
        .tools(&lead())
        .iter()
        .map(|tool| tool.name().to_string())
        .collect();
    assert!(names.contains(&"run_code".to_string()), "{names:?}");
}
