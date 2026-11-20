//! `eval`: runs Python or JavaScript the agent writes, with the interpreter found on `PATH`, or a
//! Luau script that calls the other tools, which the engine in `azazel-lua` runs (the `Luau` service).

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, AgentInfo, CodeFirst, Services, Tool, ToolCtx, ToolOutput, ToolSource};
use azazel_core::process::{AgentEnv, RunOptions, run_program};
use azazel_core::scheme::local_dir;
use azazel_core::toolcall::{Luau, ToolCaller};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::parse_input;

/// The environment variable that names the session's `local://` directory to a script.
pub const LOCAL_ENV: &str = "AZAZEL_LOCAL";

/// Loaded before a Node program: gives an ES module the `require` of CommonJS.
const NODE_REQUIRE: &str = "data:text/javascript,import{createRequire}from'node:module';\
globalThis.require=createRequire(process.cwd()+'/');";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[serde(alias = "py")]
    Python,
    #[serde(alias = "js", alias = "node")]
    Javascript,
    #[serde(alias = "lua")]
    Luau,
}

impl Language {
    /// The name in the tool's input and in `[tools.eval]`.
    pub fn key(self) -> &'static str {
        match self {
            Language::Python => "python",
            Language::Javascript => "javascript",
            Language::Luau => "luau",
        }
    }

    fn name(self) -> &'static str {
        match self {
            Language::Python => "Python",
            Language::Javascript => "JavaScript",
            Language::Luau => "Luau",
        }
    }

    /// The programs tried in order, each with the arguments that make it run a script read from
    /// standard input.
    fn candidates(self) -> &'static [(&'static str, &'static [&'static str])] {
        match self {
            Language::Python => &[
                ("python", &["-u", "-"]),
                ("python3", &["-u", "-"]),
                ("py", &["-3", "-u", "-"]),
            ],
            Language::Javascript => &[
                // Node reads standard input as CommonJS, where top-level `await` fails: it runs
                // the code as a module, with `require` put back for scripts that use it.
                ("node", &["--input-type=module", "--import", NODE_REQUIRE, "-"]),
                ("bun", &["run", "-"]),
                ("deno", &["run", "-A", "-"]),
            ],
            // Luau runs inside azazel; there is no program to find.
            Language::Luau => &[],
        }
    }

    fn missing(self) -> &'static str {
        match self {
            Language::Python => {
                "no Python interpreter was found on PATH (tried python, python3 and py); install Python 3 \
                 from https://www.python.org and make sure `python` is on PATH, or set \
                 `tools.eval.python = false` to stop offering it"
            }
            Language::Javascript => {
                "no JavaScript runtime was found on PATH (tried node, bun and deno); install Node.js from \
                 https://nodejs.org (or Bun or Deno), or set `tools.eval.javascript = false` to stop \
                 offering it"
            }
            Language::Luau => "Luau needs no program on PATH",
        }
    }
}

/// The first interpreter for `language` on `PATH`, with its arguments. On Windows the `python`
/// and `python3` under `WindowsApps` that only open the Store are passed over.
pub fn interpreter(language: Language) -> Option<(PathBuf, Vec<String>)> {
    language.candidates().iter().find_map(|(program, args)| {
        let found = which::which_all(program).ok()?.find(|path| {
            !path
                .to_string_lossy()
                .to_ascii_lowercase()
                .contains("\\microsoft\\windowsapps\\")
        })?;
        Some((found, args.iter().map(|arg| arg.to_string()).collect()))
    })
}

/// How long a background Luau script may run when nothing says otherwise.
const DEFAULT_BACKGROUND_MAX: Duration = Duration::from_secs(7200);

/// `[tools.eval]`: the languages `eval` may run and how long a background script may last, as a
/// service of the registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvalSettings {
    pub languages: Vec<Language>,
    /// Wall-clock cap of a background Luau script; zero is no cap.
    pub background_max: Duration,
}

impl EvalSettings {
    /// Python and JavaScript; Luau only where the application says the build has it.
    pub fn all() -> EvalSettings {
        EvalSettings {
            languages: vec![Language::Python, Language::Javascript],
            background_max: DEFAULT_BACKGROUND_MAX,
        }
    }

    /// Sets the cap of a background script, in seconds; zero is no cap.
    pub fn with_background_max_secs(mut self, secs: u64) -> EvalSettings {
        self.background_max = Duration::from_secs(secs);
        self
    }

    /// The languages whose switch is on. `luau` arrives already combined with "this build has Lua".
    pub fn from_switches(python: bool, javascript: bool, luau: bool) -> EvalSettings {
        let mut languages = Vec::new();
        if python {
            languages.push(Language::Python);
        }
        if javascript {
            languages.push(Language::Javascript);
        }
        if luau {
            languages.push(Language::Luau);
        }
        EvalSettings {
            languages,
            background_max: DEFAULT_BACKGROUND_MAX,
        }
    }
}

pub struct EvalTool {
    default_timeout: Duration,
    env: Arc<AgentEnv>,
    /// The languages `[tools.eval]` turns on; never empty (with none on, the tool is not offered).
    languages: Vec<Language>,
    /// Wall-clock cap of a background script.
    background_max: Duration,
    services: Arc<Services>,
    /// The description before the Luau engine can describe itself: everything but its paragraph.
    base: String,
    /// The full description, built once both `ToolCaller` and the `Luau` service exist and then
    /// kept: one that changed between requests would break the provider's prompt cache.
    description: OnceLock<String>,
}

const OPENING: &str = "Run code and get what it printed or returned, in the `language` you pick. Use it for \
     computing, parsing and transforming data, or trying out a snippet, where a script is easier \
     than a shell command.";

const PROGRAMS: &str = "Python runs with the `python` on PATH; JavaScript with `node`, else `bun`, else `deno` \
     (top-level `await` works). Each Python or JavaScript call is a fresh process in the working \
     directory: nothing is kept between calls, so keep what a later call needs in a file. The \
     environment variable `AZAZEL_LOCAL` names the session's scratch directory, the files \
     `local://<name>` addresses in read and write. stdout and stderr come back together with the \
     exit code; long output keeps its first 100 and last 300 lines. A Python or JavaScript program \
     can do anything a program can, so it is approved like a shell command.";

impl EvalTool {
    pub fn new(default_timeout: Duration, env: Arc<AgentEnv>) -> Self {
        EvalTool::with_languages(
            default_timeout,
            env,
            vec![Language::Python, Language::Javascript],
            Arc::new(Services::new()),
        )
    }

    /// `services` is where the `Luau` engine and `ToolCaller` are found, once the extensions that
    /// provide them have loaded.
    pub fn with_languages(
        default_timeout: Duration,
        env: Arc<AgentEnv>,
        languages: Vec<Language>,
        services: Arc<Services>,
    ) -> Self {
        let mut base = OPENING.to_string();
        if languages
            .iter()
            .any(|l| matches!(l, Language::Python | Language::Javascript))
        {
            base.push_str("\n\n");
            base.push_str(PROGRAMS);
        }
        EvalTool {
            default_timeout,
            env,
            languages,
            background_max: DEFAULT_BACKGROUND_MAX,
            services,
            base,
            description: OnceLock::new(),
        }
    }

    /// Caps a background script at `max` of wall-clock time; zero is no cap.
    pub fn with_background_max(mut self, max: Duration) -> Self {
        self.background_max = max;
        self
    }

    fn offers(&self, language: Language) -> bool {
        self.languages.contains(&language)
    }

    /// The description with the Luau paragraph, when Luau is on and the engine is ready.
    fn built(&self) -> Option<String> {
        if !self.offers(Language::Luau) {
            return Some(self.base.clone());
        }
        // Before the tool caller exists the extensions are still loading.
        let caller = self.services.get::<ToolCaller>()?;
        let engine = self.services.get::<Luau>()?;
        let mut text = self.base.clone();
        text.push_str("\n\n");
        // The tools code-first mode keeps out of the tool list are described here instead. The
        // description is the lead's view, built once.
        let anyone = AgentInfo {
            id: "lead".to_string(),
            def: "lead".to_string(),
            parent: None,
            depth: 0,
            session_id: String::new(),
        };
        let hidden: Vec<Arc<dyn Tool>> = match self.services.get::<CodeFirst>() {
            Some(code_first) => caller
                .tools
                .tools(&anyone)
                .into_iter()
                .filter(|tool| code_first.hides(&anyone, tool.name()))
                .collect(),
            None => Vec::new(),
        };
        text.push_str(&engine.0.describe(&self.services, &hidden));
        Some(text)
    }
}

/// The language an input names, when it names one `eval` knows.
fn language_of(input: &Value) -> Option<Language> {
    serde_json::from_value(input.get("language")?.clone()).ok()
}

#[derive(Deserialize)]
struct EvalInput {
    language: Language,
    code: String,
    timeout_secs: Option<u64>,
    #[serde(default)]
    background: bool,
}

#[async_trait]
impl Tool for EvalTool {
    fn name(&self) -> &str {
        "eval"
    }

    fn description(&self) -> &str {
        if let Some(built) = self.description.get() {
            return built;
        }
        match self.built() {
            Some(text) => self.description.get_or_init(|| text),
            None => &self.base,
        }
    }

    fn schema(&self) -> Value {
        let mut schema = json!({
            "type": "object",
            "properties": {
                "language": {"type": "string", "enum": self.languages.iter().map(|l| l.key()).collect::<Vec<_>>()},
                "code": {"type": "string", "description": "The whole program or script"},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "Python and JavaScript only: seconds the program may run; the shell tool's default when left out"}
            },
            "required": ["language", "code"]
        });
        // Fixed for the session by the configuration and the build, so the tool list never changes.
        if self.offers(Language::Luau) {
            schema["properties"]["background"] = json!({
                "type": "boolean",
                "description": "Luau only: run the script as a job and return its id at once; `job` shows its output and stops it, and you are woken with its result"
            });
        }
        schema
    }

    /// A Luau script touches nothing itself, and each tool call it makes is judged on its own; a
    /// Python or JavaScript program can do anything, so it is approved like a shell command.
    fn access(&self, input: &Value) -> Access {
        match language_of(input) {
            Some(Language::Luau) => Access::Read,
            _ => Access::Exec,
        }
    }

    /// The language, so that a rule can allow one and not the other (`eval(python)`).
    fn subjects(&self, input: &Value) -> Vec<String> {
        match language_of(input) {
            Some(language) => vec![language.key().to_string()],
            None => input
                .get("language")
                .and_then(Value::as_str)
                .map(str::to_string)
                .into_iter()
                .collect(),
        }
    }

    /// A Luau script waits for its tool calls and the order of an agent's calls matters to it.
    fn concurrency_safe(&self, _: &Value) -> bool {
        false
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: EvalInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.background && input.language != Language::Luau {
            return ToolOutput::error(
                "`background` runs only Luau scripts; use `shell` with `background` for programs",
            );
        }
        if input.language == Language::Luau {
            let Some(engine) = ctx.services.get::<Luau>() else {
                return ToolOutput::error(
                    "Luau is not in this build of azazel (it was built without the `lua` feature)",
                );
            };
            if !self.offers(Language::Luau) {
                return ToolOutput::error(
                    "Luau is turned off for eval (tools.eval.luau = false in the configuration)",
                );
            }
            if input.background {
                return engine.0.start(input.code, ctx, self.background_max);
            }
            return engine.0.run(input.code, ctx).await;
        }
        if !self.offers(input.language) {
            return ToolOutput::error(format!(
                "{} is turned off for eval (tools.eval.{} = false in the configuration)",
                input.language.name(),
                input.language.key()
            ));
        }
        let Some((program, args)) = interpreter(input.language) else {
            return ToolOutput::error(input.language.missing());
        };
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);
        let mut env = self.env.for_agent(ctx.agent.depth, &ctx.cwd);
        if let Ok(dir) = local_dir(&ctx.services) {
            env.push((LOCAL_ENV.to_string(), dir.to_string_lossy().into_owned()));
        }
        // Output as UTF-8 whatever the console's code page is, and line by line as it is printed.
        env.push(("PYTHONIOENCODING".to_string(), "utf-8".to_string()));
        env.push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));
        let update = ctx.update.clone();
        let mut options = RunOptions::new(ctx.cwd.clone());
        options.env = env;
        options.priority = self.env.priority();
        options.timeout = Some(timeout);
        options.cancel = ctx.cancel.clone();
        options.stdin = Some(input.code.into_bytes());
        options.on_line = Some(Arc::new(move |line| update.send(line)));
        let shown = program
            .file_stem()
            .map(|stem| stem.to_string_lossy().into_owned())
            .unwrap_or_default();
        let result = match run_program(&program, &args, options).await {
            Ok(result) => result,
            Err(e) => return ToolOutput::error(format!("could not start {}: {e}", program.display())),
        };
        let mut text = result.output.render();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        let (status, failed) = if result.timed_out {
            (
                format!(
                    "timed out after {} seconds; the program was killed",
                    timeout.as_secs()
                ),
                true,
            )
        } else if result.cancelled {
            ("aborted; the program was killed".to_string(), true)
        } else {
            match result.code {
                Some(0) => ("exit code: 0".to_string(), false),
                Some(code) => (format!("exit code: {code}"), true),
                None => ("killed by a signal".to_string(), true),
            }
        };
        ToolOutput {
            content: vec![azazel_ai::Block::text(format!(
                "{text}\n{status} ({} with {shown})",
                input.language.name()
            ))],
            is_error: failed,
            details: None,
        }
        .with_details(json!({"exit_code": result.code, "timed_out": result.timed_out, "program": shown}))
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::{AgentInfo, Services, UpdateSink};
    use azazel_core::SessionStore;
    use tokio_util::sync::CancellationToken;

    use super::*;

    fn ctx(cwd: &std::path::Path, services: Arc<Services>) -> ToolCtx {
        let agent = AgentInfo {
            id: "lead".into(),
            def: "lead".into(),
            parent: None,
            depth: 0,
            session_id: "s".into(),
        };
        ToolCtx {
            agent_id: agent.id.clone(),
            session_id: agent.session_id.clone(),
            agent,
            call_id: "c1".into(),
            cwd: cwd.to_path_buf(),
            cancel: CancellationToken::new(),
            update: UpdateSink::default(),
            services,
        }
    }

    async fn run(language: Language, code: &str) -> Option<(ToolOutput, PathBuf, tempfile::TempDir)> {
        if interpreter(language).is_none() {
            eprintln!("skipped: no {} interpreter on PATH", language.name());
            return None;
        }
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().to_path_buf();
        let session = Arc::new(SessionStore::create(&dir.join("sessions"), &dir, None).unwrap());
        let services = Arc::new(Services::new());
        services.insert(session.clone());
        let tool = EvalTool::new(Duration::from_secs(60), Arc::new(AgentEnv::default()));
        let language = language.key();
        let out = tool
            .call(json!({"language": language, "code": code}), &ctx(&dir, services))
            .await;
        Some((out, session.local_dir(), temp))
    }

    #[tokio::test]
    async fn python_runs_and_writes_a_local_file() {
        let code = "import os, pathlib\n\
                    pathlib.Path(os.environ['AZAZEL_LOCAL'], 'sum.txt').write_text(str(sum(range(10))))\n\
                    print('é', 6 * 7)";
        let Some((out, local, _temp)) = run(Language::Python, code).await else {
            return;
        };
        assert!(!out.is_error, "{}", out.text_content());
        assert!(out.text_content().contains("é 42"), "{}", out.text_content());
        assert_eq!(std::fs::read_to_string(local.join("sum.txt")).unwrap(), "45");
    }

    #[tokio::test]
    async fn javascript_runs_with_top_level_await_and_reports_failure() {
        let code = "const fs = await import('node:fs'); const os = require('os');\n\
                    console.log(typeof fs.readFileSync, typeof os.platform, await Promise.resolve(42))";
        let Some((out, ..)) = run(Language::Javascript, code).await else {
            return;
        };
        assert!(!out.is_error, "{}", out.text_content());
        assert!(
            out.text_content().contains("function function 42"),
            "{}",
            out.text_content()
        );
        let Some((failed, ..)) = run(Language::Javascript, "throw new Error('boom')").await else {
            return;
        };
        assert!(failed.is_error && failed.text_content().contains("boom"));
    }

    #[tokio::test]
    async fn a_language_turned_off_is_not_offered_and_is_refused() {
        let temp = tempfile::tempdir().unwrap();
        let tool = EvalTool::with_languages(
            Duration::from_secs(5),
            Arc::new(AgentEnv::default()),
            vec![Language::Python],
            Arc::new(Services::new()),
        );
        assert_eq!(tool.schema()["properties"]["language"]["enum"], json!(["python"]));
        let out = tool
            .call(
                json!({"language": "js", "code": "console.log(1)"}),
                &ctx(temp.path(), Arc::new(Services::new())),
            )
            .await;
        assert!(out.is_error && out.text_content().contains("tools.eval.javascript"));
    }

    fn offering(python: bool, javascript: bool, luau: bool) -> Vec<String> {
        let languages = EvalSettings::from_switches(python, javascript, luau).languages;
        let tool = EvalTool::with_languages(
            Duration::from_secs(5),
            Arc::new(AgentEnv::default()),
            languages,
            Arc::new(Services::new()),
        );
        tool.schema()["properties"]["language"]["enum"]
            .as_array()
            .unwrap()
            .iter()
            .map(|key| key.as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn the_language_enum_lists_exactly_the_languages_that_are_on() {
        assert_eq!(offering(true, false, false), ["python"]);
        assert_eq!(offering(true, false, true), ["python", "luau"]);
        assert_eq!(offering(true, true, true), ["python", "javascript", "luau"]);
        assert_eq!(offering(false, false, true), ["luau"]);
    }

    #[test]
    fn a_luau_script_is_a_read_and_a_program_is_an_exec() {
        let tool = EvalTool::new(Duration::from_secs(5), Arc::new(AgentEnv::default()));
        let luau = json!({"language": "lua", "code": "return 1"});
        assert!(matches!(tool.access(&luau), Access::Read));
        assert_eq!(tool.subjects(&luau), ["luau"]);
        for language in ["python", "py", "javascript"] {
            let input = json!({"language": language, "code": "1"});
            assert!(matches!(tool.access(&input), Access::Exec), "{language}");
        }
        assert_eq!(tool.subjects(&json!({"language": "js"})), ["javascript"]);
    }

    #[tokio::test]
    async fn luau_without_an_engine_says_the_build_lacks_it() {
        let temp = tempfile::tempdir().unwrap();
        let tool = EvalTool::with_languages(
            Duration::from_secs(5),
            Arc::new(AgentEnv::default()),
            vec![Language::Python],
            Arc::new(Services::new()),
        );
        let out = tool
            .call(
                json!({"language": "luau", "code": "return 1"}),
                &ctx(temp.path(), Arc::new(Services::new())),
            )
            .await;
        assert!(out.is_error && out.text_content().contains("without the `lua` feature"));
    }

    struct Engine;

    #[async_trait]
    impl azazel_core::toolcall::LuauEngine for Engine {
        fn describe(&self, _: &Services, _: &[Arc<dyn Tool>]) -> String {
            "LUAU PARAGRAPH".to_string()
        }
        async fn run(&self, code: String, _: &ToolCtx) -> ToolOutput {
            ToolOutput::text(format!("ran {code}"))
        }
        fn start(&self, code: String, _: &ToolCtx, max: Duration) -> ToolOutput {
            ToolOutput::text(format!("started {code} for {}s", max.as_secs()))
        }
    }

    #[tokio::test]
    async fn the_description_is_built_once_the_engine_and_caller_exist_and_then_stays() {
        let services = Arc::new(Services::new());
        let tool = EvalTool::with_languages(
            Duration::from_secs(5),
            Arc::new(AgentEnv::default()),
            vec![Language::Python, Language::Luau],
            services.clone(),
        );
        let early = tool.description().to_string();
        assert!(!early.contains("LUAU PARAGRAPH"));
        services.insert(Arc::new(Luau(Arc::new(Engine))));
        assert_eq!(
            tool.description(),
            early,
            "without a tool caller nothing is built yet"
        );
        let registry = Arc::new(azazel_core::ext::ToolRegistry::default());
        let gate: Arc<dyn azazel_agent::Gate> = Arc::new(azazel_agent::AllowAll);
        services.insert(Arc::new(ToolCaller::new(registry, gate)));
        let first = tool.description().to_string();
        assert!(first.contains("LUAU PARAGRAPH") && first.starts_with(&early));
        assert_eq!(tool.description(), first);

        let temp = tempfile::tempdir().unwrap();
        let out = tool
            .call(
                json!({"language": "luau", "code": "x"}),
                &ctx(temp.path(), services),
            )
            .await;
        assert_eq!(out.text_content(), "ran x");
    }
}
