//! `eval`: runs Python or JavaScript the agent writes, with the interpreter found on `PATH`.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::process::{AgentEnv, RunOptions, run_program};
use azazel_core::scheme::local_dir;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::util::parse_input;

/// The environment variable that names the session's `local://` directory to a script.
pub const LOCAL_ENV: &str = "AZAZEL_LOCAL";

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Language {
    #[serde(alias = "py")]
    Python,
    #[serde(alias = "js", alias = "node")]
    Javascript,
}

impl Language {
    fn name(self) -> &'static str {
        match self {
            Language::Python => "Python",
            Language::Javascript => "JavaScript",
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
                ("node", &["-"]),
                ("bun", &["run", "-"]),
                ("deno", &["run", "-A", "-"]),
            ],
        }
    }

    fn missing(self) -> &'static str {
        match self {
            Language::Python => {
                "no Python interpreter was found on PATH (tried python, python3 and py); install Python 3 \
                 from https://www.python.org and make sure `python` is on PATH"
            }
            Language::Javascript => {
                "no JavaScript runtime was found on PATH (tried node, bun and deno); install Node.js from \
                 https://nodejs.org (or Bun or Deno)"
            }
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

pub struct EvalTool {
    default_timeout: Duration,
    env: Arc<AgentEnv>,
}

impl EvalTool {
    pub fn new(default_timeout: Duration, env: Arc<AgentEnv>) -> Self {
        EvalTool { default_timeout, env }
    }
}

#[derive(Deserialize)]
struct EvalInput {
    language: Language,
    code: String,
    timeout_secs: Option<u64>,
}

#[async_trait]
impl Tool for EvalTool {
    fn name(&self) -> &str {
        "eval"
    }

    fn description(&self) -> &str {
        "Run a Python or JavaScript program and get what it printed. Use it for computing, parsing \
         and transforming data, or trying out a snippet, where a script is easier than a shell \
         command. Python runs with the `python` on PATH; JavaScript with `node`, else `bun`, else \
         `deno` (top-level `await` works). Each call is a fresh process in the working directory: \
         nothing is kept between calls, so keep what a later call needs in a file. The environment \
         variable `AZAZEL_LOCAL` names the session's scratch directory, the files `local://<name>` \
         addresses in read and write. stdout and stderr come back together with the exit code; long \
         output keeps its first 100 and last 300 lines. It can do anything a program can, so it is \
         approved like a shell command."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "language": {"type": "string", "enum": ["python", "javascript"]},
                "code": {"type": "string", "description": "The whole program"},
                "timeout_secs": {"type": "integer", "minimum": 1, "description": "Seconds the program may run; the shell tool's default when left out"}
            },
            "required": ["language", "code"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Exec
    }

    /// The language, so that a rule can allow one and not the other (`eval(python)`).
    fn subjects(&self, input: &Value) -> Vec<String> {
        let language = input.get("language").and_then(Value::as_str).map(|language| {
            match serde_json::from_value::<Language>(json!(language)) {
                Ok(Language::Python) => "python".to_string(),
                Ok(Language::Javascript) => "javascript".to_string(),
                Err(_) => language.to_string(),
            }
        });
        language.into_iter().collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: EvalInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let Some((program, args)) = interpreter(input.language) else {
            return ToolOutput::error(input.language.missing());
        };
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);
        let mut env = self.env.vars();
        if let Ok(dir) = local_dir(&ctx.services) {
            env.push((LOCAL_ENV.to_string(), dir.to_string_lossy().into_owned()));
        }
        // Output as UTF-8 whatever the console's code page is, and line by line as it is printed.
        env.push(("PYTHONIOENCODING".to_string(), "utf-8".to_string()));
        env.push(("PYTHONUNBUFFERED".to_string(), "1".to_string()));
        let update = ctx.update.clone();
        let mut options = RunOptions::new(ctx.cwd.clone());
        options.env = env;
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
        let language = match language {
            Language::Python => "python",
            Language::Javascript => "javascript",
        };
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
        let Some((out, ..)) = run(
            Language::Javascript,
            "const x = await Promise.resolve(42); console.log(x)",
        )
        .await
        else {
            return;
        };
        assert!(!out.is_error, "{}", out.text_content());
        assert!(out.text_content().contains("42"));
        let Some((failed, ..)) = run(Language::Javascript, "throw new Error('boom')").await else {
            return;
        };
        assert!(failed.is_error && failed.text_content().contains("boom"));
    }
}
