//! `eval` scripts in Luau and `azazel.tools.call`: tool calls made for an agent pass the permission
//! gate, and a script stays inside its limits.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{
    Access, Agent, AgentConfig, AgentInfo, CompactionSettings, Gate, GateDecision, Limits, Services, Tool,
    ToolCtx, ToolOutput, ToolSource, UpdateSink,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{Catalog, Credentials, ModelSpec, Provider, RetryPolicy, Router};
use azazel_core::agents::discover;
use azazel_core::config::set_override;
use azazel_core::permissions::plan::PermissionsExt;
use azazel_core::process::Shell;
use azazel_core::side::{Side, SideCosts};
use azazel_core::ui::Question;
use azazel_core::{
    ApprovalDecision, ApprovalRequest, Config, EntryKind, ExtCtx, Headless, LoadOptions, Paths,
    PermissionGate, PermissionState, Registry, SessionStore, ToolCaller, Ui, UiBridge, UiError,
};
use azazel_lua::{CodeExt, CodeLimits, LuaExt};
use azazel_team::{BUILTIN_AGENTS, Team, TeamDeps, TeamExt};
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

/// A project with the core tools, `eval` and the real permission gate in `mode`, with nobody
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
        Bench::with_cap(mode, limits, 7200)
    }

    /// Like `new`, with `background_max_secs` for background scripts.
    fn with_cap(mode: &str, limits: CodeLimits, cap_secs: u64) -> Bench {
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
        services.insert(Arc::new(
            azazel_tools::eval::EvalSettings::from_switches(true, true, true)
                .with_background_max_secs(cap_secs),
        ));
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
                read_guard: Default::default(),
                patch: Default::default(),
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
        self.call("eval", json!({ "language": "luau", "code": code }))
            .await
    }

    fn seen(&self) -> Vec<String> {
        self.seen.lock().unwrap().clone()
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_reads_two_files_in_one_call_and_returns_one_result() {
    let bench = Bench::new("manual", CodeLimits::default());
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
    let bench = Bench::new("manual", CodeLimits::default());
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
        "step 1\nscript error: eval:2: no such thing"
    );
    let broken = bench.run("local = 3").await;
    assert!(broken.is_error);
    assert!(
        broken.text_content().starts_with("eval:1: "),
        "{}",
        broken.text_content()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_command_in_ask_mode_with_nobody_to_ask_is_refused_by_the_gate() {
    let bench = Bench::new("manual", CodeLimits::default());
    let marker = bench.paths.project_root.join("ran.txt");
    let command = format!("echo ran > \"{}\"", marker.display());
    let refused = bench
        .run(&format!("return tools.shell({{ command = {command:?} }})"))
        .await;
    assert!(refused.is_error);
    assert_eq!(
        refused.text_content(),
        "script error: eval:1: shell: permission required for shell: re-run with --mode auto or add an \
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
    let bench = Bench::new("manual", CodeLimits::default());
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

    for name in ["eval", "task", "agent_send"] {
        let refused = bench.run(&format!("return tools.{name}({{}})")).await;
        assert!(refused.is_error);
        assert_eq!(
            refused.text_content(),
            format!("script error: eval:1: `{name}` cannot be called from a script")
        );
    }
    let unknown = bench.run("return tools.nope({})").await;
    assert_eq!(
        unknown.text_content(),
        "script error: eval:1: nope: unknown tool `nope`"
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
    let bench = Bench::new("manual", limits);
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

/// A session with a team and a lead agent, which `azazel.tools.call` acts for.
async fn with_lead(bench: &Bench) -> azazel_agent::AgentHandle {
    with_lead_on(bench, Arc::new(MockProvider::new())).await
}

/// [`with_lead`] with the models of `mock` to run sub-agents on.
async fn with_lead_on(bench: &Bench, mock: Arc<MockProvider>) -> azazel_agent::AgentHandle {
    let providers: Vec<Arc<dyn Provider>> = vec![mock];
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
        max_turns: 0,
        max_result_bytes: 50_000,
        compaction: CompactionSettings::default(),
        posix_shell: shell.is_posix(),
        routing: Default::default(),
    }));
    bench.services.insert(team.clone());
    let session =
        Arc::new(SessionStore::create(&bench.paths.sessions_dir(), &bench.paths.cwd, None).unwrap());
    let agent = Agent::spawn(AgentConfig {
        info: lead(),
        system: Vec::new(),
        tools: bench.registry.tools.clone(),
        chain: vec!["mock/lead".parse().unwrap()],
        thinking: None,
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
    team.attach(agent.clone(), session.clone());
    bench.services.insert(session);
    agent
}

fn home_script(bench: &Bench, name: &str, source: &str) {
    let path = bench.paths.home.join("extensions").join(name);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, source).unwrap();
}

fn load_scripts(bench: &Bench) {
    let credentials = Arc::new(Credentials::new(bench.paths.auth_file()));
    let ext = LuaExt::new(
        bench.paths.clone(),
        false,
        credentials,
        Vec::new(),
        azazel_lua::Scripts::default(),
    );
    bench.registry.load(&ext).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn an_extension_calls_harness_tools_as_the_lead_through_the_gate() {
    let bench = Bench::new("manual", CodeLimits::default());
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

/// A tool that takes a moment and notes how many of its calls ran at the same time.
struct Slow {
    name: &'static str,
    access: Access,
    running: AtomicUsize,
    peak: AtomicUsize,
}

impl Slow {
    fn new(name: &'static str, access: Access) -> Arc<Slow> {
        Arc::new(Slow {
            name,
            access,
            running: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        })
    }
}

#[async_trait]
impl Tool for Slow {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "Waits a moment. For tests."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"n": {"type": "integer"}}})
    }
    fn access(&self, _: &Value) -> Access {
        self.access
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let now = self.running.fetch_add(1, Ordering::SeqCst) + 1;
        self.peak.fetch_max(now, Ordering::SeqCst);
        tokio::time::sleep(Duration::from_millis(150)).await;
        self.running.fetch_sub(1, Ordering::SeqCst);
        match input.get("n").and_then(Value::as_i64) {
            Some(13) => ToolOutput::error("thirteen is refused"),
            Some(n) => ToolOutput::text(format!("{}", n * 10)),
            None => ToolOutput::text("no n"),
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn parallel_runs_calls_at_the_same_time_and_returns_results_in_their_places() {
    let bench = Bench::new("auto", CodeLimits::default());
    let nap = Slow::new("nap", Access::Read);
    let touch = Slow::new("touch", Access::Write);
    bench.registry.tools.add("test", nap.clone());
    bench.registry.tools.add("test", touch.clone());

    // Six calls, four at a time where no team sets another width: two rounds, not six.
    let started = std::time::Instant::now();
    let output = bench
        .run(
            r#"
            local calls = {}
            for n = 1, 6 do
                calls[n] = function() return tools.nap({ n = n }) end
            end
            return table.concat(parallel(calls), " ")
            "#,
        )
        .await;
    assert_eq!(output.text_content(), "10 20 30 40 50 60");
    assert_eq!(nap.peak.load(Ordering::SeqCst), 4);
    assert!(
        started.elapsed() < Duration::from_millis(700),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(output.details, Some(json!({ "tool_calls": 6 })));
    // Every call passed the gate under an id of its own.
    let mut seen = bench.seen();
    seen.sort();
    assert_eq!(
        seen,
        [
            "nap c1.1", "nap c1.2", "nap c1.3", "nap c1.4", "nap c1.5", "nap c1.6"
        ]
    );

    // Named entries keep their names, and what is not a tool call runs as well.
    let named = bench
        .run(
            r#"
            local got = parallel({
                first = function() return tools.nap({ n = 1 }) end,
                plain = function() return 7 end,
            })
            return got.first .. " " .. got.plain
            "#,
        )
        .await;
    assert_eq!(named.text_content(), "10 7");

    // A call that changes something waits until it is alone, as it would for the agent itself.
    let alone = bench
        .run(
            r#"
            local got = parallel({
                function() return tools.touch({ n = 1 }) end,
                function() return tools.touch({ n = 2 }) end,
                function() return tools.touch({ n = 3 }) end,
            })
            return table.concat(got, " ")
            "#,
        )
        .await;
    assert_eq!(alone.text_content(), "10 20 30");
    assert_eq!(touch.peak.load(Ordering::SeqCst), 1);

    // One failure is raised once the others have finished; `pcall` inside keeps the rest.
    let failed = bench
        .run(
            r#"
            return parallel({
                function() return tools.nap({ n = 1 }) end,
                function() return tools.nap({ n = 13 }) end,
            })
            "#,
        )
        .await;
    assert!(failed.is_error);
    assert_eq!(
        failed.text_content(),
        "script error: parallel: entry `2`: eval:4: nap: thirteen is refused"
    );
    let kept = bench
        .run(
            r#"
            local got = parallel({
                function() return tools.nap({ n = 1 }) end,
                function() return select(2, pcall(tools.nap, { n = 13 })) end,
            })
            return got[1] .. " / " .. got[2]
            "#,
        )
        .await;
    assert_eq!(kept.text_content(), "10 / nap: thirteen is refused");
    let wrong = bench.run("return parallel({ 1 })").await;
    assert_eq!(
        wrong.text_content(),
        "script error: parallel: every entry must be a function; entry `1` is not"
    );
}

/// The team's tools and a lead to act for, with `mock` answering for the agents a script starts.
async fn with_team(bench: &Bench, mock: Arc<MockProvider>) -> azazel_agent::AgentHandle {
    bench
        .registry
        .load(&TeamExt {
            defs: Arc::new(discover(BUILTIN_AGENTS, &[])),
            max_depth: 2,
        })
        .unwrap();
    with_lead_on(bench, mock).await
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_starts_agents_and_gets_their_reports_or_typed_values() {
    let bench = Bench::new("manual", CodeLimits::default());
    // Without the team's tools the agent has nothing to start agents with.
    let early = bench.run(r#"return agents.run("scout", "look around")"#).await;
    assert_eq!(
        early.text_content(),
        "script error: agents.run: this agent cannot start agents"
    );

    let mock = Arc::new(MockProvider::new());
    mock.push("one", MockStep::text("there are three files"));
    mock.push(
        "typed",
        MockStep::tools("", &[("r1", "report", json!({ "result": { "count": 3 } }))]),
    );
    mock.push("typed", MockStep::text("reported"));
    let agent = with_team(&bench, mock.clone()).await;

    let report = bench
        .run(
            r#"
            local report, id = agents.run("scout", "count the files", { model = "mock/one" })
            return id .. ": " .. report
            "#,
        )
        .await;
    assert_eq!(report.text_content(), "scout-1: there are three files");
    // The agent got the prompt, and starting it passed the gate as a `task` call.
    let asked = mock.requests_for("one");
    assert_eq!(asked[0].messages.last().unwrap().text(), "count the files");
    assert_eq!(bench.seen(), ["task c1.1"]);

    // With a schema the result is the value the agent reported, as a table.
    let typed = bench
        .run(
            r#"
            local value = agents.run("scout", "count the files", {
                model = "mock/typed",
                schema = {
                    type = "object",
                    properties = { count = { type = "integer" } },
                    required = { "count" },
                },
            })
            return type(value) .. " " .. value.count + 1
            "#,
        )
        .await;
    assert_eq!(typed.text_content(), "table 4");

    // Agents started from `parallel` work at the same time; each result is in its place. One
    // after the other they take two delays; together they take one, and a whole delay is left
    // for a slow runner.
    let delay = Duration::from_secs(1);
    mock.push("a", MockStep::text("alpha").delayed(delay));
    mock.push("b", MockStep::text("beta").delayed(delay));
    let started = std::time::Instant::now();
    let both = bench
        .run(
            r#"
            local got = parallel({
                function() return agents.run("scout", "a", { model = "mock/a" }) end,
                function() return agents.run("scout", "b", { model = "mock/b" }) end,
            })
            return got[1] .. " " .. got[2]
            "#,
        )
        .await;
    assert_eq!(both.text_content(), "alpha beta");
    assert!(started.elapsed() < 2 * delay, "{:?}", started.elapsed());

    // What `task` refuses is the script's error to catch.
    let unknown = bench.run(r#"return agents.run("nobody", "x")"#).await;
    assert!(
        unknown
            .text_content()
            .starts_with("script error: agents.run: no agent definition named `nobody`; there are: "),
        "{}",
        unknown.text_content()
    );
    let option = bench
        .run(r#"return agents.run("scout", "x", { wait = false })"#)
        .await;
    assert_eq!(
        option.text_content(),
        "script error: agents.run: there is no option `wait`; the options are model, isolation, schema and name"
    );
    // An agent whose model fails has not done its work: that is an error, with what it said.
    mock.push(
        "broken",
        MockStep::fail(azazel_ai::AiError::Auth("no key".into())),
    );
    let failed = bench
        .run(r#"return agents.run("scout", "x", { model = "mock/broken", name = "probe" })"#)
        .await;
    assert!(failed.is_error);
    assert!(
        failed
            .text_content()
            .starts_with("script error: agents.run: agent `probe` failed: "),
        "{}",
        failed.text_content()
    );
    agent.shutdown().await;
}

#[tokio::test(flavor = "multi_thread")]
async fn stored_values_are_kept_in_the_session_and_follow_its_branches() {
    let bench = Bench::new("manual", CodeLimits::default());
    let homeless = bench.run(r#"store("k", 1)"#).await;
    assert_eq!(homeless.text_content(), "script error: store: no session is open");

    let session =
        Arc::new(SessionStore::create(&bench.paths.sessions_dir(), &bench.paths.cwd, None).unwrap());
    bench.services.insert(session.clone());
    let start = session.append(EntryKind::Message {
        message: azazel_ai::Message::user("go"),
    });
    let first = bench
        .run(r#"store("cursor", { page = 2, seen = { "a", "b" } }) return load("cursor").page"#)
        .await;
    assert_eq!(first.text_content(), "2");
    // A later script finds it, and so does the session when it is opened again.
    let later = bench
        .run(r#"return table.concat(load("cursor").seen, ",") .. " " .. tostring(load("other"))"#)
        .await;
    assert_eq!(later.text_content(), "a,b nil");
    let reopened = Arc::new(SessionStore::open(&session.path()).unwrap());
    let stored: Vec<Value> = reopened
        .entries()
        .into_iter()
        .filter_map(|entry| match entry.kind {
            EntryKind::Custom { custom_type, data } if custom_type == "run_code_store" => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(
        stored,
        [json!({ "key": "cursor", "value": { "page": 2, "seen": ["a", "b"] } })]
    );

    // The newest value wins; nil forgets the key.
    bench.run(r#"store("cursor", 5)"#).await;
    assert_eq!(bench.run(r#"return load("cursor")"#).await.text_content(), "5");
    let latest = session.leaf();
    bench.run(r#"store("cursor", nil)"#).await;
    assert_eq!(
        bench
            .run(r#"return tostring(load("cursor"))"#)
            .await
            .text_content(),
        "nil"
    );
    // Where the conversation goes back to an earlier point, what was stored after it is not seen.
    session.set_leaf(latest.as_deref());
    assert_eq!(bench.run(r#"return load("cursor")"#).await.text_content(), "5");
    session.set_leaf(Some(&start));
    assert_eq!(
        bench
            .run(r#"return tostring(load("cursor"))"#)
            .await
            .text_content(),
        "nil"
    );

    let huge = bench.run(r#"store("big", string.rep("x", 300000))"#).await;
    assert!(
        huge.text_content().starts_with(
            "script error: store: the value of `big` is 300002 bytes of JSON; one value may be 262144."
        ),
        "{}",
        huge.text_content()
    );
    let unstorable = bench.run(r#"store("f", print)"#).await;
    assert!(unstorable.is_error, "{}", unstorable.text_content());
}

/// What an MCP client with many tools offers: `tool_search`, and a tool only once a search
/// has found it.
struct HeldBack {
    found: Arc<std::sync::atomic::AtomicBool>,
}

struct Finder(Arc<std::sync::atomic::AtomicBool>);

#[async_trait]
impl Tool for Finder {
    fn name(&self) -> &str {
        "tool_search"
    }
    fn description(&self) -> &str {
        "Find tools provided by MCP servers."
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"query": {"type": "string"}}})
    }
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, _: Value, _: &ToolCtx) -> ToolOutput {
        self.0.store(true, Ordering::SeqCst);
        ToolOutput::text("found")
    }
}

impl ToolSource for HeldBack {
    fn tools(&self, _: &AgentInfo) -> Vec<Arc<dyn Tool>> {
        let mut tools: Vec<Arc<dyn Tool>> = vec![Arc::new(Finder(self.found.clone()))];
        if self.found.load(Ordering::SeqCst) {
            tools.push(Arc::new(Named(
                "mcp__tracker__create_ticket",
                "Files a ticket in the issue tracker. Needs a title.",
            )));
        }
        tools
    }
}

/// A tool that says its name and what it was given.
struct Named(&'static str, &'static str);

#[async_trait]
impl Tool for Named {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        self.1
    }
    fn schema(&self) -> Value {
        json!({"type": "object", "properties": {"title": {"type": "string"}}, "required": ["title"]})
    }
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        ToolOutput::text(format!("{} got {input}", self.0))
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_finds_and_describes_tools_that_are_not_listed() {
    let bench = Bench::new("manual", CodeLimits::default());
    bench.registry.tools.add_source(
        "test",
        Arc::new(HeldBack {
            found: Arc::default(),
        }),
    );
    bench.registry.tools.add(
        "test",
        Arc::new(Named("search", "A tool that happens to be called search.")),
    );

    // The ticket tool is not offered until a search finds it; then it is ranked and callable.
    let hidden = bench
        .run(r#"return tools.mcp__tracker__create_ticket({ title = "x" })"#)
        .await;
    assert!(hidden.is_error, "{}", hidden.text_content());
    let found = bench
        .run(
            r#"
            local hits = tools.search("file a ticket in the tracker", 3)
            local first = hits[1]
            return first.name .. " | " .. first.description .. " | " .. tools[first.name]({ title = "crash" })
            "#,
        )
        .await;
    assert_eq!(
        found.text_content(),
        "mcp__tracker__create_ticket | Files a ticket in the issue tracker | mcp__tracker__create_ticket got \
         {\"title\":\"crash\"}"
    );

    // The tools a script cannot call are never found, and the limit is kept.
    let names = bench
        .run(
            r#"
            local names = {}
            for _, hit in ipairs(tools.search("run a script or command or start agents task", 25)) do
                table.insert(names, hit.name)
            end
            return #tools.search("read a file", 2) .. " " .. table.concat(names, " ")
            "#,
        )
        .await;
    let names = names.text_content();
    assert!(names.starts_with("2 "), "{names}");
    assert!(names.contains("shell") && !names.contains("eval"), "{names}");
    let nothing = bench.run(r#"return #tools.search("zzzqqq")"#).await;
    assert_eq!(nothing.text_content(), "0");

    let described = bench
        .run(
            r#"
            local read = tools.describe("read")
            return read.name .. " | " .. tostring(string.find(read.signature, "read{ path: string", 1, true))
                .. " | " .. read.parameters.properties.path.type .. " | " .. tostring(#read.description > 40)
                .. " | " .. tostring(tools.describe("nope")) .. " | " .. tostring(tools.describe("eval"))
            "#,
        )
        .await;
    assert_eq!(described.text_content(), "read | 1 | string | true | nil | nil");

    // A tool that is itself called `search` is still called with a table of arguments.
    let own = bench.run(r#"return tools.search({ title = "t" })"#).await;
    assert_eq!(own.text_content(), "search got {\"title\":\"t\"}");
    let empty = bench.run(r#"return tools.search("  ")"#).await;
    assert_eq!(
        empty.text_content(),
        "script error: tools.search: the query is empty"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn classify_asks_the_judge_role_and_counts_as_a_call() {
    let bench = Bench::new("manual", CodeLimits::default());
    let modelless = bench.run(r#"return classify("q", { "a", "b" })"#).await;
    assert_eq!(
        modelless.text_content(),
        "script error: classify: no model can be asked in this session"
    );

    let mock = Arc::new(MockProvider::new());
    mock.push(
        "judge",
        MockStep::text(r#"{"choice": 2, "confidence": 0.9, "reason": "nothing crashes"}"#),
    );
    mock.push("other", MockStep::text(r#"{"choice": "bug", "confidence": 0.6}"#));
    let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
    let roles: BTreeMap<String, Vec<ModelSpec>> =
        BTreeMap::from([("judge".to_string(), vec!["mock/judge".parse().unwrap()])]);
    bench.services.insert(Arc::new(Side {
        router: Arc::new(Router::new(
            providers,
            Catalog::default(),
            roles,
            RetryPolicy::default(),
        )),
        prompt: bench.registry.prompt.clone(),
        tools: bench.registry.tools.clone(),
        costs: Arc::new(SideCosts::default()),
    }));

    let output = bench
        .run(
            r#"
            local answer = classify("Is this a bug report?\nThe docs have a typo.", { "bug", "not a bug" })
            local second = classify("And this?", { "bug", "not a bug" }, { model = "mock/other" })
            return string.format("%s %d %.1f %s %s | %s %s", answer.choice, answer.index, answer.confidence,
                answer.reason, answer.model, second.choice, second.model)
            "#,
        )
        .await;
    assert_eq!(
        output.text_content(),
        "not a bug 2 0.9 nothing crashes mock/judge | bug mock/other"
    );
    assert_eq!(output.details, Some(json!({ "tool_calls": 2 })));
    // The judge was asked the question alone: no prompt of the agent, none of its tools.
    let asked = &mock.requests_for("judge")[0];
    assert!(asked.system.is_empty() && asked.tools.is_empty());
    assert!(asked.messages[0].text().contains("The docs have a typo."));

    let refused = bench.run(r#"return classify("q", { "only" })"#).await;
    assert_eq!(
        refused.text_content(),
        "script error: classify: there are 1 choices; give from 2 to 20"
    );
}

/// A user who is present and would approve everything, counting how often they were asked.
struct Approver {
    asked: AtomicUsize,
}

#[async_trait]
impl UiBridge for Approver {
    async fn ask(&self, _: &Question) -> Result<Vec<String>, UiError> {
        Err(UiError::NoUser)
    }
    async fn confirm(&self, _: &str) -> Result<bool, UiError> {
        Ok(true)
    }
    async fn select(&self, _: &str, _: &[String]) -> Result<usize, UiError> {
        Ok(0)
    }
    async fn input(&self, _: &str) -> Result<String, UiError> {
        Err(UiError::NoUser)
    }
    fn notify(&self, _: azazel_core::ext::Level, _: &str) {}
    async fn approval(&self, _: ApprovalRequest) -> Option<ApprovalDecision> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Some(ApprovalDecision::AllowOnce)
    }
}

/// The text of `job output` for `id` once the job is no longer running.
async fn finished_job(bench: &Bench, id: &str) -> String {
    for _ in 0..400 {
        let text = bench
            .call("job", json!({ "action": "output", "id": id }))
            .await
            .text_content();
        if !text.contains(&format!("[{id}: running")) {
            return text;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("{id} did not end");
}

async fn start_background(bench: &Bench, code: &str) -> String {
    let started = bench
        .call(
            "eval",
            json!({ "language": "luau", "code": code, "background": true }),
        )
        .await;
    assert!(!started.is_error, "{}", started.text_content());
    started.details.unwrap()["job"].as_str().unwrap().to_string()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_script_is_denied_instead_of_asking_even_when_a_user_is_present() {
    let bench = Bench::new("manual", CodeLimits::default());
    let user = Arc::new(Approver {
        asked: AtomicUsize::new(0),
    });
    bench.services.insert(Arc::new(Ui(user.clone())));
    let marker = bench.paths.project_root.join("ran.txt");
    let command = format!("echo ran > \"{}\"", marker.display());

    let id = start_background(
        &bench,
        &format!("return tools.shell({{ command = {command:?} }})"),
    )
    .await;
    let text = finished_job(&bench, &id).await;
    assert!(text.contains("needed approval"), "{text}");
    assert!(text.contains("foreground"), "{text}");
    assert!(text.contains(&format!("[{id}: failed")), "{text}");
    assert_eq!(user.asked.load(Ordering::SeqCst), 0);
    assert!(!marker.exists());

    // The same call in the foreground does ask, and runs once approved.
    let foreground = bench
        .run(&format!("return tools.shell({{ command = {command:?} }})"))
        .await;
    assert!(!foreground.is_error, "{}", foreground.text_content());
    assert_eq!(user.asked.load(Ordering::SeqCst), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_script_returns_at_once_runs_as_a_job_and_job_kill_stops_it() {
    let bench = Bench::new("auto", CodeLimits::default());
    bench.registry.tools.add("test", Slow::new("nap", Access::Read));

    let started = std::time::Instant::now();
    let id = start_background(
        &bench,
        "for n = 1, 1000 do tools.nap({ n = n }) end return 'done'",
    )
    .await;
    assert!(started.elapsed() < Duration::from_millis(140));
    assert_eq!(id, "lua-1");
    let listed = bench
        .call("job", json!({ "action": "list" }))
        .await
        .text_content();
    assert!(listed.contains("lua-1  running"), "{listed}");

    let stopped = bench.call("job", json!({ "action": "kill", "id": &id })).await;
    assert_eq!(stopped.text_content(), "stopping lua-1");
    let text = finished_job(&bench, &id).await;
    assert!(text.contains("[lua-1: killed"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_script_that_finishes_leaves_its_result_in_the_job_output() {
    let bench = Bench::new("auto", CodeLimits::default());
    let id = start_background(&bench, "print('working') return 'all done'").await;
    let text = finished_job(&bench, &id).await;
    assert!(text.contains("working") && text.contains("all done"), "{text}");
    assert!(text.contains("[lua-1: finished"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_background_script_is_stopped_at_the_wall_clock_cap() {
    let bench = Bench::with_cap("auto", CodeLimits::default(), 1);
    bench.registry.tools.add("test", Slow::new("nap", Access::Read));
    let id = start_background(
        &bench,
        "for n = 1, 1000 do tools.nap({ n = n }) end return 'done'",
    )
    .await;
    let text = finished_job(&bench, &id).await;
    assert!(text.contains("stopped after 1 s"), "{text}");
    assert!(text.contains("[lua-1: timed out"), "{text}");
}

#[tokio::test(flavor = "multi_thread")]
async fn background_is_refused_for_other_languages() {
    let bench = Bench::new("auto", CodeLimits::default());
    let refused = bench
        .call(
            "eval",
            json!({ "language": "python", "code": "print(1)", "background": true }),
        )
        .await;
    assert!(refused.is_error);
    assert!(
        refused.text_content().contains("only Luau"),
        "{}",
        refused.text_content()
    );
}

/// Saves `source` as the user's script `name` and makes the scripts available to `bench`.
fn saved(bench: &Bench, name: &str, description: &str, source: &str) {
    let scripts = Arc::new(azazel_core::scripts::Scripts::new(bench.paths.clone(), true));
    scripts.save(name, description, source).unwrap();
    bench.services.insert(scripts);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_script_runs_a_saved_script_with_input_and_its_tool_calls_pass_the_gate() {
    let bench = Bench::new("auto", CodeLimits::default());
    bench.write("a.txt", "alpha\n");
    saved(
        &bench,
        "first-line",
        "Read the first line of a file",
        "-- input: { path: string }\nlocal text = tools.read({ path = input.path })\nreturn { found = text:find(\"alpha\", 1, true) ~= nil }",
    );
    let out = bench
        .run(
            r#"
            local result, err = tools.run("first-line", { path = "a.txt" })
            local missing, why = tools.run("nope", {})
            return json.encode(result) .. " | " .. tostring(err) .. " | " .. tostring(missing) .. " | " .. why
            "#,
        )
        .await;
    assert_eq!(
        out.text_content(),
        "{\"found\":true} | nil | nil | tools.run: there is no saved script `nope`"
    );
    // The nested read went through the gate under a call id of the running script.
    assert!(
        bench.seen().iter().any(|call| call.starts_with("read c1.")),
        "{:?}",
        bench.seen()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn saved_scripts_may_nest_three_levels_deep_and_a_script_that_fails_gives_nil_and_a_message() {
    let bench = Bench::new("auto", CodeLimits::default());
    saved(
        &bench,
        "again",
        "Runs itself",
        "local v, e = tools.run(\"again\", input + 1)\nreturn v or e",
    );
    saved(&bench, "boom", "Fails", "error(\"bad input\")");
    let out = bench.run(r#"return tools.run("again", 0)"#).await;
    // Levels 1, 2 and 3 run; the fourth is refused, and the message comes back up each level.
    assert_eq!(
        out.text_content(),
        "tools.run: scripts may run others only 3 levels deep"
    );
    let out = bench
        .run(r#"local v, e = tools.run("boom", nil) return tostring(v) .. " | " .. e"#)
        .await;
    let text = out.text_content();
    assert!(
        text.starts_with("nil | tools.run: `boom`: ") && text.contains("bad input"),
        "{text}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn search_and_describe_find_saved_scripts_as_kind_script() {
    let bench = Bench::new("auto", CodeLimits::default());
    saved(
        &bench,
        "summarize-todos",
        "Count the TODO comments in a folder",
        "-- input: { dir: string }\nreturn 1",
    );
    let out = bench
        .run(
            r#"
            local hits = tools.search("count todo comments", 3)
            local d = tools.describe("summarize-todos")
            return hits[1].name .. " " .. hits[1].kind .. " | " .. d.kind .. " | " .. d.input .. " | " .. d.signature
            "#,
        )
        .await;
    assert_eq!(
        out.text_content(),
        "summarize-todos script | script | { dir: string } | tools.run(\"summarize-todos\", input)"
    );
}
