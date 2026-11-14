#![allow(clippy::unwrap_used)]

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use azazel_agent::{
    Access, Agent, AgentConfig, AgentEvent, AgentInfo, AllowAll, CompactionSettings, Limits, NullSink,
    Services, Tool, ToolCtx, ToolOutput, ToolSource, UpdateSink, UserInput,
};
use azazel_ai::testing::{MockProvider, MockStep};
use azazel_ai::{Catalog, Credentials, Provider, RetryPolicy, Router, ThinkingLevel};
use azazel_core::{CommandOutcome, Event, ExtCtx, Headless, Outcome, Paths, Registry, Ui, UiExtras};
use azazel_lua::{LuaExt, Scripts, type_declarations};
use serde_json::{Value, json};

const GUARD: &str = include_str!("../../../examples/extensions/guard.luau");
const WORD_COUNT: &str = include_str!("../../../examples/extensions/word_count.luau");
const AUTO_MODEL: &str = include_str!("../../../examples/extensions/auto_model.luau");

fn lead() -> AgentInfo {
    AgentInfo {
        id: "lead".into(),
        def: "lead".into(),
        parent: None,
        depth: 0,
        session_id: "s1".into(),
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// An empty azazel home and project with a registry that has loaded nothing yet.
struct Bench {
    dir: tempfile::TempDir,
    paths: Paths,
    services: Arc<Services>,
    registry: Arc<Registry>,
}

impl Bench {
    fn new() -> Bench {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("project");
        let home = dir.path().join("home");
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let paths = Paths::discover(&project, home.to_str());
        let services = Arc::new(Services::new());
        services.insert(Arc::new(Ui(Arc::new(Headless))));
        let registry = Arc::new(Registry::new(ExtCtx {
            services: services.clone(),
            cwd: paths.cwd.clone(),
            session_id: "s1".into(),
        }));
        registry.publish();
        Bench {
            dir,
            paths,
            services,
            registry,
        }
    }

    /// Writes `<home>/extensions/<name>`.
    fn home_script(&self, name: &str, source: &str) {
        write(&self.paths.home.join("extensions").join(name), source);
    }

    /// Writes `<project>/.azazel/extensions/<name>`.
    fn project_script(&self, name: &str, source: &str) {
        write(&self.paths.project_dir().join("extensions").join(name), source);
    }

    fn load(&self, trusted: bool) {
        self.load_with(trusted, Scripts::default());
    }

    /// Loads the scripts of the directories and `scripts`.
    fn load_with(&self, trusted: bool, scripts: Scripts) {
        let credentials = Arc::new(Credentials::new(self.paths.auth_file()));
        let ext = LuaExt::new(self.paths.clone(), trusted, credentials, Vec::new(), scripts);
        self.registry.load(&ext).unwrap();
    }

    /// Calls the tool `name` as the lead and returns whether it failed and what it said.
    async fn call(&self, name: &str, input: Value) -> (bool, String) {
        let ctx = ToolCtx {
            agent: lead(),
            agent_id: "lead".into(),
            session_id: "s1".into(),
            call_id: "c1".into(),
            cwd: self.paths.cwd.clone(),
            cancel: tokio_util::sync::CancellationToken::new(),
            update: UpdateSink::default(),
            services: self.services.clone(),
        };
        let tool = self
            .registry
            .tools
            .get(&lead(), name)
            .unwrap_or_else(|| panic!("there is no tool {name}"));
        let output = tool.call(input, &ctx).await;
        (output.is_error, output.text_content())
    }

    /// Who provides the tool `name`, whom it replaced and who wraps it.
    fn listed(&self, name: &str) -> (String, Option<String>, Vec<String>) {
        let listing = self.registry.tools.listing(&lead());
        let tool = listing.into_iter().find(|tool| tool.name == name).unwrap();
        (tool.owner, tool.replaces, tool.wrapped_by)
    }

    async fn command(&self, name: &str, args: &str) -> CommandOutcome {
        let command = self
            .registry
            .command(name)
            .unwrap_or_else(|| panic!("there is no /{name}"));
        command.handler.run(args, &self.registry.bus.ctx()).await
    }

    /// The text `/name args` shows.
    async fn shown(&self, name: &str, args: &str) -> String {
        match self.command(name, args).await {
            CommandOutcome::Show(text) => text,
            other => panic!("/{name} {args}: {other:?}"),
        }
    }

    async fn emit(&self, mut event: Event) -> (Outcome, Event) {
        let outcome = self.registry.bus.emit(&mut event).await;
        (outcome, event)
    }

    fn extras(&self) -> Arc<UiExtras> {
        self.services.get::<UiExtras>().unwrap()
    }

    fn tool_names(&self) -> Vec<String> {
        let tools = self.registry.tools.tools(&lead());
        tools.iter().map(|tool| tool.name().to_string()).collect()
    }
}

/// A tool of the harness itself: answers with its name and the arguments it was given.
struct BuiltIn(&'static str);

#[async_trait::async_trait]
impl Tool for BuiltIn {
    fn name(&self) -> &str {
        self.0
    }
    fn description(&self) -> &str {
        "built in"
    }
    fn schema(&self) -> Value {
        json!({ "type": "object" })
    }
    fn access(&self, _: &Value) -> Access {
        Access::Read
    }
    fn subjects(&self, _: &Value) -> Vec<String> {
        Vec::new()
    }
    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        ToolOutput::text(format!("{} {input}", self.0))
    }
}

fn tool_call(tool: &str, input: Value) -> Event {
    Event::ToolCall {
        agent: lead(),
        call_id: "c1".into(),
        tool: tool.into(),
        input,
        skip_prompt: false,
    }
}

#[tokio::test]
async fn a_tool_defined_by_a_script_is_offered_to_the_model_and_runs_when_called() {
    let bench = Bench::new();
    bench.home_script("word_count.luau", WORD_COUNT);
    write(
        &bench.paths.project_root.join("notes.txt"),
        "one two\nthree  four\n",
    );
    bench.load(false);

    let mock = Arc::new(MockProvider::new());
    let calls = [
        ("c1", "word_count", json!({ "path": "notes.txt" })),
        ("c2", "word_count", json!({ "path": "missing.txt" })),
    ];
    mock.push("a", MockStep::tools("", &calls));
    mock.push("a", MockStep::text("four words"));
    let providers: Vec<Arc<dyn Provider>> = vec![mock.clone()];
    let router = Arc::new(Router::new(
        providers,
        Catalog::default(),
        BTreeMap::new(),
        RetryPolicy::default(),
    ));
    let agent = Agent::spawn(AgentConfig {
        info: lead(),
        system: Vec::new(),
        tools: bench.registry.tools.clone(),
        chain: vec!["mock/a".parse().unwrap()],
        thinking: None,
        router,
        gate: Arc::new(AllowAll),
        bus: bench.registry.agent_hooks(),
        services: bench.services.clone(),
        cwd: bench.paths.cwd.clone(),
        initial_messages: Vec::new(),
        sink: Arc::new(NullSink::default()),
        artifacts: None,
        limits: Limits::default(),
        compaction: CompactionSettings::default(),
    });
    let mut events = agent.subscribe();
    assert!(agent.prompt(UserInput::text("count the words")));
    let mut results = Vec::new();
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            AgentEvent::ToolEnd { call_id, output, .. } => {
                results.push((call_id, output.is_error, output.text_content()));
            }
            AgentEvent::Idle { .. } => break,
            AgentEvent::Failed { error } => panic!("the agent failed: {error}"),
            _ => {}
        }
    }
    agent.shutdown().await;

    results.sort();
    assert_eq!(results[0], ("c1".to_string(), false, "4".to_string()));
    let (_, failed, why) = &results[1];
    assert!(*failed && why.contains("missing.txt"), "{why}");

    let requests = mock.requests();
    let (_, request) = &requests[0];
    let offered = request
        .tools
        .iter()
        .find(|tool| tool.name == "word_count")
        .unwrap();
    assert!(offered.description.starts_with("Count the words"));
    assert_eq!(offered.schema["required"], json!(["path"]));
    assert_eq!(offered.schema["properties"]["path"]["type"], "string");
}

#[tokio::test]
async fn a_tool_call_handler_can_deny_the_call_or_rewrite_its_input() {
    let bench = Bench::new();
    bench.home_script(
        "policy.luau",
        r#"
        azazel.on("tool_call", function(ev, ctx)
            if ev.tool == "deploy" then
                return { block = true, reason = "no" }
            end
            if ev.tool == "shell" and ev.agent.id == ctx.agent_id then
                return { input = { command = "echo " .. ev.input.command }, skip_prompt = true }
            end
        end)
        azazel.on("turn_end", function()
            return { block = true, reason = "a turn cannot be blocked" }
        end)
        "#,
    );
    bench.load(false);

    let (outcome, _) = bench.emit(tool_call("deploy", json!({}))).await;
    assert_eq!(outcome, Outcome::Block { reason: "no".into() });

    let (outcome, event) = bench.emit(tool_call("shell", json!({ "command": "rm x" }))).await;
    assert_eq!(outcome, Outcome::Continue);
    let Event::ToolCall {
        input, skip_prompt, ..
    } = event
    else {
        panic!()
    };
    assert_eq!(input, json!({ "command": "echo rm x" }));
    assert!(skip_prompt);

    let untouched = json!({ "path": "a.txt", "limit": 3, "flags": ["x"], "nested": { "deep": true } });
    let (outcome, event) = bench.emit(tool_call("read", untouched.clone())).await;
    assert_eq!(outcome, Outcome::Continue);
    let Event::ToolCall {
        input, skip_prompt, ..
    } = event
    else {
        panic!()
    };
    assert_eq!(input, untouched);
    assert!(!skip_prompt);

    let (outcome, _) = bench.emit(Event::TurnEnd { agent: lead() }).await;
    assert_eq!(outcome, Outcome::Continue);
}

#[tokio::test]
async fn the_guard_example_refuses_destructive_shell_commands_only() {
    let bench = Bench::new();
    bench.home_script("guard.luau", GUARD);
    bench.load(false);

    for command in [
        "rm -rf /",
        "sudo rm -fr /* && echo done",
        "dd if=/dev/zero of=/dev/sda",
    ] {
        let (outcome, _) = bench
            .emit(tool_call("shell", json!({ "command": command })))
            .await;
        let Outcome::Block { reason } = outcome else {
            panic!("`{command}` was let through")
        };
        assert!(reason.starts_with("guard:"), "{reason}");
    }
    for command in ["rm -rf /tmp/build", "rm -rf ./target", "ls -la /", "farm -x /"] {
        let (outcome, _) = bench
            .emit(tool_call("shell", json!({ "command": command })))
            .await;
        assert_eq!(outcome, Outcome::Continue, "{command}");
    }
    let (outcome, _) = bench
        .emit(tool_call("write", json!({ "command": "rm -rf /" })))
        .await;
    assert_eq!(outcome, Outcome::Continue);
    assert!(bench.shown("guard", "").await.contains("refused 3 command(s)"));
}

#[tokio::test]
async fn a_handler_that_never_returns_is_stopped_and_only_its_script_is_disabled() {
    let bench = Bench::new();
    bench.home_script(
        "a_spin.luau",
        r#"
        azazel.tool({ name = "spin_tool", description = "unused", execute = function() return "" end })
        azazel.command({ name = "spin", run = function() return "" end })
        azazel.status("spin", "spinning")
        azazel.on("turn_start", function() while true do end end)
        "#,
    );
    bench.home_script(
        "b_steady.luau",
        r#"
        azazel.tool({ name = "steady_tool", description = "unused", execute = function() return "" end })
        azazel.on("turn_start", function(ev) azazel.status("turn", "turn of " .. ev.agent.id) end)
        azazel.on("user_prompt", function(ev) return { text = ev.text .. "!" } end)
        "#,
    );
    bench.load(false);
    assert_eq!(bench.tool_names(), ["spin_tool", "steady_tool"]);
    assert_eq!(bench.extras().status(), ["spinning"]);

    let started = Instant::now();
    let (outcome, _) = bench.emit(Event::TurnStart { agent: lead() }).await;
    let took = started.elapsed();
    assert_eq!(outcome, Outcome::Continue);
    assert!(
        took > Duration::from_secs(4) && took < Duration::from_secs(10),
        "took {took:?}"
    );
    // The handler registered after the runaway one ran in the same round, and what the disabled
    // script contributed is gone.
    assert_eq!(bench.extras().status(), ["turn of lead"]);
    assert_eq!(bench.tool_names(), ["steady_tool"]);
    assert!(bench.registry.command("spin").is_none());
    let listing = bench.shown("ext", "").await;
    let lines: Vec<&str> = listing.lines().collect();
    let spin = lines
        .iter()
        .position(|line| line.contains("a_spin.luau"))
        .unwrap();
    assert!(
        lines[spin + 1].contains("disabled") && lines[spin + 1].contains("turn_start"),
        "{listing}"
    );
    let steady = lines
        .iter()
        .position(|line| line.contains("b_steady.luau"))
        .unwrap();
    assert!(
        lines
            .get(steady + 1)
            .is_none_or(|line| !line.contains("disabled")),
        "{listing}"
    );

    let started = Instant::now();
    bench.emit(Event::TurnStart { agent: lead() }).await;
    assert!(started.elapsed() < Duration::from_secs(1));
    let prompt = Event::UserPrompt {
        text: "hi".into(),
        images: Vec::new(),
    };
    let (outcome, event) = bench.emit(prompt).await;
    assert_eq!(outcome, Outcome::Continue);
    let Event::UserPrompt { text, .. } = event else {
        panic!()
    };
    assert_eq!(text, "hi!");
}

#[tokio::test]
async fn files_can_be_read_and_written_inside_the_allowed_directories_only() {
    let bench = Bench::new();
    bench.home_script(
        "files.luau",
        r#"
        azazel.command({ name = "cat", run = function(args) return azazel.fs.read(args) end })
        azazel.command({ name = "put", run = function(args)
            azazel.fs.write(args, "written")
            return tostring(azazel.fs.exists(args))
        end })
        azazel.command({ name = "ls", run = function(args)
            local names = {}
            for _, entry in ipairs(azazel.fs.list(args)) do
                table.insert(names, entry.name .. (entry.is_dir and "/" or ""))
            end
            return table.concat(names, " ")
        end })
        azazel.command({ name = "own", run = function()
            local file = azazel.fs.data_dir .. "/note.txt"
            azazel.fs.write(file, "mine")
            return azazel.fs.read(file)
        end })
        "#,
    );
    let project = bench.paths.project_root.clone();
    write(&project.join("inside.txt"), "hello");
    let secret = bench.dir.path().join("secret.txt");
    write(&secret, "secret");
    bench.load(false);

    assert_eq!(bench.shown("cat", "inside.txt").await, "hello");
    for path in ["../secret.txt", "sub/../../secret.txt", secret.to_str().unwrap()] {
        let CommandOutcome::Error(why) = bench.command("cat", path).await else {
            panic!("{path} was read")
        };
        assert!(why.contains("outside the directories"), "{why}");
    }

    assert_eq!(bench.shown("put", "sub/new.txt").await, "true");
    assert_eq!(
        std::fs::read_to_string(project.join("sub").join("new.txt")).unwrap(),
        "written"
    );
    assert!(matches!(
        bench.command("put", "../escaped.txt").await,
        CommandOutcome::Error(_)
    ));
    assert!(!bench.dir.path().join("escaped.txt").exists());
    assert_eq!(bench.shown("ls", ".").await, ".azazel/ inside.txt sub/");

    assert_eq!(bench.shown("own", "").await, "mine");
    let note = bench.paths.home.join("extensions/.data/files/note.txt");
    assert_eq!(std::fs::read_to_string(note).unwrap(), "mine");
}

#[tokio::test]
async fn reload_runs_the_scripts_again_and_their_state_survives_it() {
    let bench = Bench::new();
    bench.home_script(
        "counter.luau",
        r#"
        local loads = (azazel.state.get("loads") or 0) + 1
        azazel.state.set("loads", loads)
        azazel.command({ name = "bump", run = function()
            local seen = azazel.state.get("seen") or { count = 0, marks = {} }
            seen.count += 1
            table.insert(seen.marks, "load " .. loads)
            azazel.state.set("seen", seen)
            return seen.count .. ": " .. table.concat(seen.marks, ", ")
        end })
        "#,
    );
    bench.load(false);
    assert_eq!(bench.shown("bump", "").await, "1: load 1");
    assert_eq!(bench.shown("bump", "").await, "2: load 1, load 1");
    assert!(bench.registry.command("late").is_none());

    bench.home_script(
        "late.luau",
        r#"azazel.command({ name = "late", run = function() return "here" end })"#,
    );
    let reloaded = bench.shown("reload", "").await;
    assert!(
        reloaded.contains("counter.luau") && reloaded.contains("late.luau"),
        "{reloaded}"
    );
    assert_eq!(bench.shown("late", "").await, "here");
    assert_eq!(bench.shown("bump", "").await, "3: load 1, load 1, load 2");
    assert_eq!(bench.registry.extensions(), ["lua"]);
}

#[tokio::test]
async fn a_script_that_fails_to_load_is_listed_with_its_error_while_the_others_load() {
    let bench = Bench::new();
    bench.home_script("broken.luau", "azazel.tool({ name = \n");
    bench.home_script(
        "crash.luau",
        "azazel.command({ name = 'never', run = print })\nerror('boom at load')",
    );
    bench.home_script(
        "hello/init.luau",
        r#"
        azazel.command({
            name = "hello",
            description = "Greets",
            run = function(args, ctx)
                return "hello " .. args .. ", session " .. ctx.session_id .. ", agent " .. ctx.agent_id
            end,
        })
        "#,
    );
    bench.load(false);

    let listing = bench.shown("ext", "").await;
    assert!(listing.starts_with("extensions: lua\n"), "{listing}");
    let lines: Vec<&str> = listing.lines().collect();
    let line_of = |file: &str| {
        let at = lines.iter().position(|line| line.contains(file));
        at.unwrap_or_else(|| panic!("{file} is not listed in\n{listing}"))
    };
    let broken = line_of("broken.luau");
    assert!(lines[broken].trim_start().starts_with("broken "), "{listing}");
    assert!(
        lines[broken + 1].trim_start().starts_with("disabled: "),
        "{listing}"
    );
    let crash = line_of("crash.luau");
    assert!(lines[crash + 1].contains("boom at load"), "{listing}");
    let hello = line_of("init.luau");
    assert!(lines[hello].trim_start().starts_with("hello "), "{listing}");
    assert!(
        lines.get(hello + 1).is_none_or(|line| !line.contains("disabled")),
        "{listing}"
    );

    // Nothing of a script whose top level failed is registered, even what it defined before failing.
    assert!(bench.registry.command("never").is_none());
    assert_eq!(
        bench.shown("hello", "there").await,
        "hello there, session s1, agent lead"
    );
    assert_eq!(bench.registry.command("hello").unwrap().description, "Greets");
}

#[tokio::test]
async fn project_scripts_load_only_in_a_trusted_project_and_then_replace_home_scripts_of_the_same_id() {
    let which = |place: &str| {
        format!(r#"azazel.command({{ name = "which", run = function() return "{place}" end }})"#)
    };
    let project_only = r#"azazel.command({ name = "local", run = function() return "" end })"#;

    let untrusted = Bench::new();
    untrusted.home_script("same.luau", &which("home"));
    untrusted.project_script("same.luau", &which("project"));
    untrusted.project_script("extra.luau", project_only);
    untrusted.load(false);
    assert_eq!(untrusted.shown("which", "").await, "home");
    assert!(untrusted.registry.command("local").is_none());

    let trusted = Bench::new();
    trusted.home_script("same.luau", &which("home"));
    trusted.project_script("same.luau", &which("project"));
    trusted.project_script("extra.luau", project_only);
    trusted.load(true);
    assert_eq!(trusted.shown("which", "").await, "project");
    assert!(trusted.registry.command("local").is_some());
}

#[tokio::test]
async fn status_segments_widgets_and_key_bindings_reach_the_ui() {
    let bench = Bench::new();
    bench.home_script(
        "ui.luau",
        r#"
        azazel.status("branch", "main")
        azazel.widget("todo", { "one", "two" })
        azazel.shortcut("Ctrl+K", function(ctx)
            azazel.status("key", "pressed in " .. ctx.session_id)
        end)
        azazel.command({ name = "clear", run = function()
            azazel.status("branch", nil)
            azazel.widget("todo", nil)
        end })
        "#,
    );
    bench.load(false);
    let extras = bench.extras();
    assert_eq!(extras.status(), ["main"]);
    assert_eq!(extras.widget_lines(), ["one", "two"]);

    let binding = extras.shortcut("ctrl+k").unwrap();
    binding.run(&bench.registry.bus.ctx()).await;
    assert_eq!(extras.status(), ["main", "pressed in s1"]);

    assert_eq!(bench.command("clear", "").await, CommandOutcome::Done);
    assert_eq!(extras.status(), ["pressed in s1"]);
    assert_eq!(extras.widget_lines(), Vec::<String>::new());
}

#[tokio::test]
async fn functions_that_register_are_refused_once_the_script_has_loaded() {
    let bench = Bench::new();
    bench.home_script(
        "late.luau",
        r#"
        azazel.on("session_end", function()
            local noop = function() end
            local attempts = {
                tool = function() azazel.tool({ name = "late", description = "late", execute = noop }) end,
                on = function() azazel.on("turn_end", noop) end,
                command = function() azazel.command({ name = "late", run = noop }) end,
                shortcut = function() azazel.shortcut("ctrl+l", noop) end,
                provider = function()
                    azazel.provider({ id = "late", api = "openai-chat", base_url = "http://localhost:1" })
                end,
                mcp = function() azazel.mcp({ name = "late", command = "late" }) end,
                prompt = function() azazel.prompt("late", noop) end,
            }
            local refused = {}
            for name, attempt in pairs(attempts) do
                local ok, why = pcall(attempt)
                if not ok and string.find(tostring(why), "while the script loads", 1, true) then
                    table.insert(refused, name)
                end
            end
            table.sort(refused)
            azazel.status("refused", table.concat(refused, " "))
        end)
        "#,
    );
    bench.load(false);
    let (outcome, _) = bench.emit(Event::SessionEnd).await;
    assert_eq!(outcome, Outcome::Continue);
    assert_eq!(
        bench.extras().status(),
        ["command mcp on prompt provider shortcut tool"]
    );
    assert_eq!(bench.tool_names(), Vec::<String>::new());
    assert!(bench.registry.command("late").is_none());
    assert!(bench.extras().shortcut("ctrl+l").is_none());
    assert!(bench.registry.providers().is_empty());
}

#[tokio::test]
async fn exec_runs_a_program_directly_and_returns_its_exit_code_and_output() {
    let bench = Bench::new();
    bench.home_script(
        "run.luau",
        r#"
        azazel.command({ name = "run", run = function(args)
            local result = azazel.exec(args, { "--version" })
            return result.code .. "|" .. result.stdout .. "|" .. result.stderr
        end })
        "#,
    );
    bench.load(false);
    let shown = bench.shown("run", env!("CARGO")).await;
    assert!(shown.starts_with("0|cargo "), "{shown}");
    let CommandOutcome::Error(why) = bench.command("run", "no-such-program-for-azazel").await else {
        panic!("a missing program ran")
    };
    assert!(why.contains("cannot run `no-such-program-for-azazel`"), "{why}");
}

#[tokio::test]
async fn what_the_session_lacks_is_reported_to_the_script_as_an_error() {
    let bench = Bench::new();
    bench.home_script(
        "lacking.luau",
        r#"
        local ok, why = pcall(azazel.mcp, { name = "docs", command = "docs-server" })
        azazel.status("mcp", tostring(ok) .. " " .. tostring(why))
        azazel.command({ name = "ask", run = function(_, ctx) return tostring(ctx.ui.confirm("sure?")) end })
        azazel.command({ name = "recall", run = function() return #azazel.memory.search("tabs") end })
        azazel.command({ name = "nudge", run = function() azazel.send("carry on") end })
        azazel.command({ name = "early", run = function() return "loaded" end })
        "#,
    );
    bench.load(false);
    let status = bench.extras().status();
    assert!(
        status[0].starts_with("false ") && status[0].contains("MCP unavailable"),
        "{status:?}"
    );
    for (command, reason) in [
        ("ask", "no interactive user"),
        ("recall", "memory unavailable"),
        ("nudge", "no session"),
    ] {
        let CommandOutcome::Error(why) = bench.command(command, "").await else {
            panic!("/{command} did not fail")
        };
        assert!(why.contains(reason), "/{command}: {why}");
    }
    // None of these errors disables the script.
    assert_eq!(bench.shown("early", "").await, "loaded");
}

#[tokio::test]
async fn a_script_adds_parts_to_the_system_prompt() {
    let bench = Bench::new();
    bench.home_script(
        "house.luau",
        r#"
        azazel.prompt("house_rules", function(ctx)
            return "House rule: tabs. Agent " .. ctx.agent_id .. " in session " .. ctx.session_id .. "."
        end)
        azazel.prompt("only_for_scouts", function(ctx)
            if ctx.agent_id == "lead" then return nil end
            return "You are a scout."
        end)
        local ok, why = pcall(azazel.prompt, "identity", function() return "I am someone else" end)
        azazel.status("identity", tostring(ok) .. " " .. tostring(why))
        "#,
    );
    bench.home_script(
        "broken.luau",
        r#"azazel.prompt("broken", function() error("no such rule") end)"#,
    );
    bench.load(false);
    let status = bench.extras().status();
    assert!(
        status[0].starts_with("false ") && status[0].contains("`identity` is a part of the built-in prompt"),
        "{status:?}"
    );

    let texts =
        |parts: Vec<azazel_ai::SystemPart>| parts.into_iter().map(|part| part.text).collect::<Vec<_>>();
    assert_eq!(
        texts(bench.registry.prompt.render(&lead()).await),
        ["House rule: tabs. Agent lead in session s1."]
    );
    let scout = AgentInfo {
        id: "scout-1".into(),
        def: "scout".into(),
        parent: Some("lead".into()),
        depth: 1,
        session_id: "s1".into(),
    };
    assert_eq!(
        texts(bench.registry.prompt.render(&scout).await),
        [
            "House rule: tabs. Agent scout-1 in session s1.",
            "You are a scout."
        ]
    );
    // A part that fails disables its script and is left out from then on.
    let listed = bench.shown("ext", "").await;
    assert!(
        listed.contains("disabled: its prompt part `broken` failed:") && listed.contains("no such rule"),
        "{listed}"
    );
    // The parts go when the scripts are unloaded, and come back with a reload.
    bench.registry.unload("lua");
    assert_eq!(
        texts(bench.registry.prompt.render(&lead()).await),
        Vec::<String>::new()
    );
}

#[tokio::test]
async fn a_script_reads_and_names_the_open_session() {
    let bench = Bench::new();
    bench.home_script(
        "session.luau",
        r#"
        azazel.command({ name = "name", run = function(args)
            local before = azazel.session.name()
            azazel.session.set_name(args)
            return tostring(before) .. " -> " .. azazel.session.name()
        end })
        azazel.command({ name = "said", run = function()
            local lines = {}
            for _, message in ipairs(azazel.session.messages()) do
                local kinds = {}
                for _, block in ipairs(message.content) do
                    table.insert(kinds, block.type .. (block.data and "+bytes" or ""))
                end
                table.insert(lines, message.role .. ": " .. message.text .. " [" .. table.concat(kinds, " ") .. "]")
            end
            return table.concat(lines, "\n")
        end })
        azazel.command({ name = "think", run = function(args) azazel.session.set_thinking(args) end })
        azazel.command({ name = "model", run = function(args) azazel.session.set_model(args) end })
        "#,
    );
    bench.load(false);
    let failure = |outcome: CommandOutcome| match outcome {
        CommandOutcome::Error(why) => why,
        other => panic!("{other:?}"),
    };
    assert!(failure(bench.command("name", "x").await).contains("no session is open"));

    let store = Arc::new(
        azazel_core::SessionStore::create(&bench.dir.path().join("sessions"), &bench.paths.cwd, None)
            .unwrap(),
    );
    bench.services.insert(store.clone());
    assert_eq!(bench.shown("name", " demo ").await, "nil -> demo");
    assert_eq!(bench.shown("name", "second").await, "demo -> second");
    assert!(failure(bench.command("name", "  ").await).contains("the name is empty"));
    assert_eq!(store.name().as_deref(), Some("second"));

    store.append(azazel_core::EntryKind::Message {
        message: azazel_ai::Message::user_blocks(vec![
            azazel_ai::Block::text("look"),
            azazel_ai::Block::Image {
                mime: "image/png".into(),
                data: "AAAA".into(),
            },
        ]),
    });
    assert_eq!(bench.shown("said", "").await, "user: look [text image]");

    // Without a lead agent there is nothing to switch, and a mistake is named either way.
    assert!(failure(bench.command("think", "extreme").await).contains("extreme"));
    assert!(failure(bench.command("think", "high").await).contains("no session with a team of agents"));
    assert!(failure(bench.command("model", "mock/a").await).contains("names no model that is configured"));
}

#[tokio::test]
async fn the_type_declarations_cover_everything_a_script_is_given() {
    let bench = Bench::new();
    bench.home_script(
        "surface.luau",
        r#"
        azazel.command({ name = "surface", run = function(_, ctx)
            local names = {}
            for name, value in pairs(azazel) do
                table.insert(names, name)
                if type(value) == "table" then
                    for inner in pairs(value) do
                        table.insert(names, name .. "." .. inner)
                    end
                end
            end
            for name in pairs(ctx) do
                table.insert(names, "ctx." .. name)
            end
            for name in pairs(ctx.ui) do
                table.insert(names, "ctx.ui." .. name)
            end
            table.sort(names)
            return table.concat(names, " ")
        end })
        "#,
    );
    bench.load(false);
    let surface = bench.shown("surface", "").await;
    assert_eq!(
        surface,
        "agent agent.list agent.send agent.spawn agent.stop classify command ctx.agent_id ctx.cwd ctx.session_id \
         ctx.ui ctx.ui.confirm ctx.ui.input ctx.ui.select exec fs fs.data_dir fs.exists fs.list fs.read \
         fs.write http json json.decode json.encode mcp memory memory.save memory.search notify on prompt \
         provider router send session session.compact session.messages session.name session.set_model \
         session.set_name session.set_thinking shortcut state state.get state.set status tool tools \
         tools.call widget wrap"
    );
    let declarations = type_declarations();
    assert!(declarations.contains("declare azazel: {"));
    for name in surface.split(' ').chain(["ctx.mode"]) {
        let field = name.rsplit('.').next().unwrap();
        assert!(
            declarations.contains(&format!("\t{field}: ")),
            "`{name}` is missing from the type declarations"
        );
    }
}

/// A router over `mock`'s models with `roles`, routing virtual models through what the scripts
/// of `bench` registered.
fn routed(bench: &Bench, mock: Arc<MockProvider>, roles: &[(&str, &str)]) -> Arc<Router> {
    let providers: Vec<Arc<dyn Provider>> = vec![mock];
    let roles = roles
        .iter()
        .map(|(role, spec)| (role.to_string(), vec![spec.parse().unwrap()]))
        .collect();
    Arc::new(
        Router::new(providers, Catalog::default(), roles, RetryPolicy::default())
            .with_virtual(bench.registry.virtual_models.clone()),
    )
}

fn lead_on(
    bench: &Bench,
    router: Arc<Router>,
    chain: &str,
    sink: Arc<azazel_core::SessionStore>,
) -> azazel_agent::AgentHandle {
    Agent::spawn(AgentConfig {
        info: lead(),
        system: Vec::new(),
        tools: bench.registry.tools.clone(),
        chain: vec![chain.parse().unwrap()],
        thinking: None,
        router,
        gate: Arc::new(AllowAll),
        bus: bench.registry.agent_hooks(),
        services: bench.services.clone(),
        cwd: bench.paths.cwd.clone(),
        initial_messages: Vec::new(),
        sink,
        artifacts: None,
        limits: Limits::default(),
        compaction: CompactionSettings::default(),
    })
}

/// Sends `text` and waits for the lead to stop; `Err` is why it failed.
async fn prompted(agent: &azazel_agent::AgentHandle, text: &str) -> Result<(), String> {
    let mut events = agent.subscribe();
    assert!(agent.prompt(UserInput::text(text)));
    loop {
        let event = tokio::time::timeout(Duration::from_secs(30), events.recv())
            .await
            .unwrap()
            .unwrap();
        match event {
            AgentEvent::Idle { .. } => return Ok(()),
            AgentEvent::Failed { error } => return Err(error),
            _ => {}
        }
    }
}

#[tokio::test]
async fn a_router_defined_by_a_script_picks_the_model_of_each_request_and_the_session_records_it() {
    let bench = Bench::new();
    bench.home_script("auto_model.luau", AUTO_MODEL);
    bench.home_script(
        "levels.luau",
        r#"
        azazel.router("deep", function(request)
            return { model = { "nowhere/gone", "@slow" }, thinking = request.thinking == "off" and "high" or "max" }
        end)
        azazel.router("broken", function(request) error("no route for " .. request.reason) end)
        azazel.router("empty", function() return nil end)
        "#,
    );
    bench.load(false);
    assert_eq!(
        bench.registry.virtual_models.names(),
        ["auto", "broken", "deep", "empty"]
    );

    let mock = Arc::new(MockProvider::new());
    mock.push("big", MockStep::tools("", &[("c1", "absent", json!({}))]));
    mock.push("big", MockStep::text("the cache is cold"));
    mock.push("small", MockStep::text("renamed"));
    let router = routed(
        &bench,
        mock.clone(),
        &[("slow", "mock/big"), ("smol", "mock/small")],
    );
    let session = Arc::new(
        azazel_core::SessionStore::create(&bench.dir.path().join("sessions"), &bench.paths.cwd, None)
            .unwrap(),
    );
    let agent = lead_on(&bench, router.clone(), "virtual/auto", session.clone());

    // A question that needs thought goes to the slow role, and the request after the tool
    // result stays on the model that began the turn.
    prompted(&agent, "Why is the build slow?").await.unwrap();
    assert_eq!(mock.requests_for("big").len(), 2);
    let status = agent.status();
    assert_eq!(status.model.unwrap().to_string(), "mock/big");
    assert_eq!(status.via.unwrap().to_string(), "virtual/auto");
    // Anything else goes to the small one.
    prompted(&agent, "rename x to y").await.unwrap();
    assert_eq!(mock.requests_for("small").len(), 1);
    assert_eq!(agent.status().model.unwrap().to_string(), "mock/small");
    agent.shutdown().await;

    // Each request is in the session with the model it went to and the virtual model behind it.
    let dispatched: Vec<(String, Option<String>)> = session
        .entries()
        .into_iter()
        .filter_map(|entry| match entry.kind {
            azazel_core::EntryKind::RequestStarted { model, via } => Some((model, via)),
            _ => None,
        })
        .collect();
    let via = Some("virtual/auto".to_string());
    assert_eq!(
        dispatched,
        [
            ("mock/big".to_string(), via.clone()),
            ("mock/big".to_string(), via.clone()),
            ("mock/small".to_string(), via),
        ]
    );

    // A route may be a chain with a thinking level: the model nobody provides is passed over.
    mock.push("big", MockStep::text("thought about it"));
    let agent = lead_on(&bench, router.clone(), "virtual/deep", session.clone());
    prompted(&agent, "think").await.unwrap();
    assert_eq!(agent.status().model.unwrap().to_string(), "mock/big:high");
    assert_eq!(mock.requests_for("big")[2].thinking, ThinkingLevel::High);
    agent.shutdown().await;

    // A router that fails, or picks nothing, fails the request with its reason.
    let agent = lead_on(&bench, router.clone(), "virtual/broken", session.clone());
    let failed = prompted(&agent, "x").await.unwrap_err();
    assert!(
        failed.contains("virtual/broken: ") && failed.ends_with("no route for user"),
        "{failed}"
    );
    agent.shutdown().await;
    let agent = lead_on(&bench, router.clone(), "virtual/empty", session.clone());
    let failed = prompted(&agent, "x").await.unwrap_err();
    assert!(
        failed.ends_with("virtual/empty: the router returned nothing"),
        "{failed}"
    );
    agent.shutdown().await;

    // A reload registers the routers of the scripts that are there now.
    std::fs::remove_file(bench.paths.home.join("extensions").join("levels.luau")).unwrap();
    bench.shown("reload", "").await;
    assert_eq!(bench.registry.virtual_models.names(), ["auto"]);
    assert_eq!(router.virtual_models(), ["auto"]);
}

#[tokio::test]
async fn a_router_is_registered_while_the_script_loads_and_a_script_can_ask_for_a_judgement() {
    let bench = Bench::new();
    bench.home_script(
        "judge.luau",
        r#"
        local ok, why = pcall(azazel.router, "not a name", function() end)
        azazel.status("refused", tostring(why))
        azazel.command({ name = "late", run = function()
            azazel.router("late", function() return "@smol" end)
        end })
        azazel.command({ name = "judge", run = function(args)
            local answer = azazel.classify(args, { "docs", "code" })
            return answer.choice .. " " .. answer.index .. " " .. answer.confidence .. " " .. answer.model
        end })
        -- A router may ask a model before it answers.
        azazel.router("triage", function(request)
            local answer = azazel.classify("What does this ask for? " .. request.text, { "docs", "code" })
            return answer.choice == "code" and "mock/big" or "mock/small"
        end)
        "#,
    );
    bench.load(false);
    assert!(
        bench.extras().status()[0].contains(
            "azazel.router: `not a name` is not a model name; use up to 64 letters, digits, `_`, `-` and `.`"
        ),
        "{:?}",
        bench.extras().status()
    );
    match bench.command("late", "").await {
        CommandOutcome::Error(why) => assert!(why.contains("while the script loads"), "{why}"),
        other => panic!("{other:?}"),
    }
    match bench.command("judge", "fix the typo").await {
        CommandOutcome::Error(why) => {
            assert!(
                why.contains("azazel.classify: no model can be asked in this session"),
                "{why}"
            );
        }
        other => panic!("{other:?}"),
    }

    let mock = Arc::new(MockProvider::new());
    let router = routed(&bench, mock.clone(), &[("judge", "mock/judge")]);
    bench.services.insert(Arc::new(azazel_core::side::Side {
        router: router.clone(),
        prompt: bench.registry.prompt.clone(),
        tools: bench.registry.tools.clone(),
        costs: Arc::new(azazel_core::side::SideCosts::default()),
    }));
    mock.push(
        "judge",
        MockStep::text(r#"{"choice": "docs", "confidence": 0.8}"#),
    );
    assert_eq!(
        bench.shown("judge", "fix the typo").await,
        "docs 1 0.8 mock/judge"
    );

    mock.push("judge", MockStep::text(r#"{"choice": 2, "confidence": 0.7}"#));
    mock.push("big", MockStep::text("done"));
    let session = Arc::new(
        azazel_core::SessionStore::create(&bench.dir.path().join("sessions"), &bench.paths.cwd, None)
            .unwrap(),
    );
    let agent = lead_on(&bench, router, "virtual/triage", session);
    prompted(&agent, "add a retry to the uploader").await.unwrap();
    assert_eq!(agent.status().model.unwrap().to_string(), "mock/big");
    let asked = mock.requests_for("judge")[1].messages[0].text();
    assert!(
        asked.contains("What does this ask for? add a retry to the uploader"),
        "{asked}"
    );
    agent.shutdown().await;
}

#[tokio::test]
async fn a_script_replaces_a_built_in_tool_only_when_it_says_so_and_while_it_works() {
    let bench = Bench::new();
    bench.registry.tools.add("core", Arc::new(BuiltIn("read")));
    bench.registry.tools.add("core", Arc::new(BuiltIn("grep")));
    // Same name, no `replaces`: the built-in tool stays.
    bench.home_script(
        "quiet.luau",
        r#"azazel.tool({ name = "grep", description = "mine", execute = function() return "quiet" end })"#,
    );
    bench.home_script(
        "vault.luau",
        r#"
        azazel.tool({
            name = "read",
            description = "Read a file from the vault.",
            access = "read",
            replaces = true,
            parameters = { type = "object", properties = { path = { type = "string" } } },
            execute = function(args)
                if args.path == "boom" then
                    error("the vault is closed")
                end
                return "vault:" .. args.path
            end,
        })
        azazel.on("turn_start", function() error("broken handler") end)
        "#,
    );
    bench.load(false);

    assert_eq!(bench.tool_names(), ["read", "grep"]);
    let (failed, text) = bench.call("grep", json!({})).await;
    assert_eq!((failed, text.as_str()), (false, "grep {}"));
    assert_eq!(bench.listed("grep"), ("core".to_string(), None, Vec::new()));

    let read = bench.registry.tools.get(&lead(), "read").unwrap();
    assert_eq!(read.description(), "Read a file from the vault.");
    assert_eq!(
        bench.call("read", json!({ "path": "a.txt" })).await,
        (false, "vault:a.txt".to_string())
    );
    assert_eq!(
        bench.listed("read"),
        ("lua:vault".to_string(), Some("core".to_string()), Vec::new())
    );
    // An error in the tool is the tool's result, not a reason to give the name back.
    let (failed, why) = bench.call("read", json!({ "path": "boom" })).await;
    assert!(failed && why.contains("the vault is closed"), "{why}");
    assert_eq!(bench.listed("read").0, "lua:vault");

    // A disabled script replaces nothing: the built-in tool is back.
    bench.emit(Event::TurnStart { agent: lead() }).await;
    assert_eq!(
        bench.call("read", json!({ "path": "a.txt" })).await,
        (false, r#"read {"path":"a.txt"}"#.to_string())
    );
    assert_eq!(bench.listed("read"), ("core".to_string(), None, Vec::new()));

    // Loaded again, it replaces again, once.
    bench.shown("reload", "").await;
    assert_eq!(bench.listed("read").0, "lua:vault");
    assert_eq!(bench.tool_names(), ["read", "grep"]);
}

#[tokio::test]
async fn a_wrap_runs_around_a_tool_and_can_change_refuse_or_answer_the_call() {
    let bench = Bench::new();
    bench.registry.tools.add("core", Arc::new(BuiltIn("shell")));
    bench.home_script(
        "audit.luau",
        r#"
        local seen = {}
        azazel.wrap("shell", {
            before = function(input, ctx)
                table.insert(seen, ctx.agent_id .. ":" .. tostring(input.command))
                if input.command == "rm" then
                    return { block = true, reason = "rm is not run here" }
                end
                if input.command == "date" then
                    return { content = "a fixed date" }
                end
                if input.command == "ls" then
                    return { input = { command = "ls -la" } }
                end
                if input.command == "crash" then
                    error("the audit log is full")
                end
            end,
            after = function(result, input)
                if input.command == "fail" then
                    return { is_error = true }
                end
                if input.command == "ls -la" then
                    return result.content .. " [" .. #seen .. " seen]"
                end
            end,
        })
        "#,
    );
    bench.load(false);
    assert_eq!(
        bench.listed("shell"),
        ("core".to_string(), None, vec!["lua:audit".to_string()])
    );

    // Untouched when both functions return nothing.
    assert_eq!(
        bench.call("shell", json!({ "command": "pwd" })).await,
        (false, r#"shell {"command":"pwd"}"#.to_string())
    );
    // The tool runs with the arguments `before` gave, and `after` sees those.
    assert_eq!(
        bench.call("shell", json!({ "command": "ls" })).await,
        (false, r#"shell {"command":"ls -la"} [2 seen]"#.to_string())
    );
    assert_eq!(
        bench.call("shell", json!({ "command": "rm" })).await,
        (true, "rm is not run here".to_string())
    );
    assert_eq!(
        bench.call("shell", json!({ "command": "date" })).await,
        (false, "a fixed date".to_string())
    );
    assert_eq!(
        bench.call("shell", json!({ "command": "fail" })).await,
        (true, r#"shell {"command":"fail"}"#.to_string())
    );

    // A wrap that fails disables its script; the tool goes on without it.
    assert_eq!(
        bench.call("shell", json!({ "command": "crash" })).await,
        (false, r#"shell {"command":"crash"}"#.to_string())
    );
    let listed = bench.shown("ext", "").await;
    assert!(
        listed.contains("its `before` around `shell` failed") && listed.contains("the audit log is full"),
        "{listed}"
    );
    assert_eq!(
        bench.call("shell", json!({ "command": "rm" })).await,
        (false, r#"shell {"command":"rm"}"#.to_string())
    );
}

#[tokio::test]
async fn wrap_is_given_a_tool_name_and_at_least_one_function_while_the_script_loads() {
    let bench = Bench::new();
    bench.home_script("a.luau", r#"azazel.wrap("shell", {})"#);
    bench.home_script(
        "b.luau",
        r#"azazel.wrap("not a name", { before = function() end })"#,
    );
    bench.home_script(
        "c.luau",
        r#"azazel.command({ name = "late", run = function()
            azazel.wrap("shell", { before = function() end })
        end })"#,
    );
    bench.load(false);
    let listed = bench.shown("ext", "").await;
    assert!(
        listed.contains("azazel.wrap: give `before`, `after` or both"),
        "{listed}"
    );
    assert!(listed.contains("`not a name` is not a tool name"), "{listed}");
    match bench.command("late", "").await {
        CommandOutcome::Error(why) => {
            assert!(why.contains("can only be called while the script loads"), "{why}")
        }
        other => panic!("{other:?}"),
    }
}

#[tokio::test]
async fn scripts_named_for_the_run_load_last_and_alone_when_discovery_is_off() {
    let script =
        |text: &str| format!(r#"azazel.command({{ name = "who", run = function() return "{text}" end }})"#);
    let bench = Bench::new();
    bench.home_script("who.luau", &script("home"));
    bench.home_script(
        "other.luau",
        r#"azazel.command({ name = "other", run = function() return "other" end })"#,
    );
    let outside = bench.dir.path().join("elsewhere/who.luau");
    write(&outside, &script("run"));
    let run = vec![("who".to_string(), outside)];

    // Beside the scripts of the directories, replacing the one with its id.
    bench.load_with(
        false,
        Scripts {
            run: run.clone(),
            ..Scripts::default()
        },
    );
    assert_eq!(bench.shown("who", "").await, "run");
    assert_eq!(bench.shown("other", "").await, "other");
    // A reload keeps it.
    bench.shown("reload", "").await;
    assert_eq!(bench.shown("who", "").await, "run");

    // With discovery off, nothing but it.
    let alone = Bench::new();
    alone.home_script("other.luau", &script("home"));
    alone.load_with(
        false,
        Scripts {
            run,
            only_run: true,
            ..Scripts::default()
        },
    );
    assert_eq!(alone.shown("who", "").await, "run");
    assert!(alone.registry.command("other").is_none());
}
