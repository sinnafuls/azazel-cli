//! `shell` runs commands; `job` manages the ones started in the background.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use azazel_agent::{Access, Tool, ToolCtx, ToolOutput};
use azazel_core::process::{AgentEnv, OutputBuffer, RunOptions, Shell, pump_lines, run, spawn};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::util::{parse_input, resolve_path};

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Lines kept per background job.
const JOB_TAIL_LINES: usize = 2000;
const DEFAULT_JOB_TAIL: usize = 100;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum JobState {
    Running,
    Exited(Option<i32>),
    Killed,
}

struct Job {
    command: String,
    started: Instant,
    output: Arc<Mutex<OutputBuffer>>,
    state: Arc<Mutex<JobState>>,
    kill: CancellationToken,
}

/// Background processes of one session. Dropping it kills them all.
#[derive(Default)]
pub struct Jobs {
    next: AtomicU64,
    jobs: Mutex<BTreeMap<String, Job>>,
}

impl Jobs {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn start(
        &self,
        shell: &Shell,
        command: &str,
        cwd: &Path,
        env: &[(String, String)],
    ) -> std::io::Result<String> {
        let mut child = spawn(shell, command, cwd, env, false)?;
        let id = format!("job-{}", self.next.fetch_add(1, Ordering::Relaxed) + 1);
        let output = Arc::new(Mutex::new(OutputBuffer::new(0, JOB_TAIL_LINES)));
        let state = Arc::new(Mutex::new(JobState::Running));
        let kill = CancellationToken::new();

        let (tx, mut rx) = mpsc::unbounded_channel();
        if let Some(stdout) = child.stdout().take() {
            pump_lines(stdout, tx.clone());
        }
        if let Some(stderr) = child.stderr().take() {
            pump_lines(stderr, tx);
        }
        let sink = output.clone();
        tokio::spawn(async move {
            while let Some(line) = rx.recv().await {
                locked(&sink).push(&line);
            }
        });
        let (task_state, task_kill) = (state.clone(), kill.clone());
        tokio::spawn(async move {
            let ended = tokio::select! {
                status = child.wait() => JobState::Exited(status.ok().and_then(|s| s.code())),
                _ = task_kill.cancelled() => {
                    let _ = child.start_kill();
                    let _ = child.wait().await;
                    JobState::Killed
                }
            };
            *locked(&task_state) = ended;
        });

        locked(&self.jobs).insert(
            id.clone(),
            Job {
                command: command.to_string(),
                started: Instant::now(),
                output,
                state,
                kill,
            },
        );
        Ok(id)
    }

    fn describe(job: &Job) -> String {
        match *locked(&job.state) {
            JobState::Running => format!("running for {}s", job.started.elapsed().as_secs()),
            JobState::Exited(Some(code)) => format!("exited with code {code}"),
            JobState::Exited(None) => "exited".to_string(),
            JobState::Killed => "killed".to_string(),
        }
    }

    /// The last `tail` output lines and the job's state.
    pub fn output(&self, id: &str, tail: usize) -> Option<String> {
        let jobs = locked(&self.jobs);
        let job = jobs.get(id)?;
        let text = locked(&job.output).last(tail);
        let status = Self::describe(job);
        Some(if text.is_empty() {
            format!("(no output)\n[{id}: {status}]")
        } else {
            format!("{text}\n[{id}: {status}]")
        })
    }

    /// False when there is no such job.
    pub fn kill(&self, id: &str) -> bool {
        match locked(&self.jobs).get(id) {
            Some(job) => {
                job.kill.cancel();
                true
            }
            None => false,
        }
    }

    /// One line per job: `id  state  command`.
    pub fn list(&self) -> Vec<String> {
        locked(&self.jobs)
            .iter()
            .map(|(id, job)| format!("{id}  {}  {}", Self::describe(job), job.command))
            .collect()
    }

    pub fn kill_all(&self) {
        for job in locked(&self.jobs).values() {
            job.kill.cancel();
        }
    }
}

impl Drop for Jobs {
    fn drop(&mut self) {
        self.kill_all();
    }
}

pub struct ShellTool {
    shell: Shell,
    default_timeout: Duration,
    jobs: Arc<Jobs>,
    env: Arc<AgentEnv>,
    description: String,
}

impl ShellTool {
    pub fn new(shell: Shell, default_timeout: Duration, jobs: Arc<Jobs>, env: Arc<AgentEnv>) -> Self {
        let description = format!(
            "Run a command in {} on {}. stdout and stderr are returned together, followed by the exit \
             code; long output keeps its first 100 and last 300 lines. The default timeout is {} seconds \
             (`timeout_secs` overrides it). Set `background` to true for servers and other long-running \
             processes, then use the `job` tool to read their output or stop them. Prefer the dedicated \
             read, edit, glob and grep tools over shell equivalents.",
            shell.name(),
            std::env::consts::OS,
            default_timeout.as_secs()
        );
        ShellTool {
            shell,
            default_timeout,
            jobs,
            env,
            description,
        }
    }
}

#[derive(Deserialize)]
struct ShellInput {
    command: String,
    cwd: Option<String>,
    timeout_secs: Option<u64>,
    #[serde(default)]
    background: bool,
}

#[async_trait]
impl Tool for ShellTool {
    fn name(&self) -> &str {
        "shell"
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command": {"type": "string"},
                "cwd": {"type": "string", "description": "Directory to run in; defaults to the working directory"},
                "timeout_secs": {"type": "integer", "minimum": 1},
                "background": {"type": "boolean", "description": "Start it and return a job id at once"},
                "description": {"type": "string", "description": "A few words on what this command does"}
            },
            "required": ["command"]
        })
    }

    fn access(&self, _: &Value) -> Access {
        Access::Exec
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("command")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, ctx: &ToolCtx) -> ToolOutput {
        let input: ShellInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        let cwd = match &input.cwd {
            Some(dir) => resolve_path(&ctx.cwd, dir),
            None => ctx.cwd.clone(),
        };
        if !cwd.is_dir() {
            return ToolOutput::error(format!("{} is not a directory", cwd.display()));
        }
        let env = self.env.vars();
        if input.background {
            return match self.jobs.start(&self.shell, &input.command, &cwd, &env) {
                Ok(id) => ToolOutput::text(format!(
                    "started {id} in the background; use the job tool to read its output or stop it"
                ))
                .with_details(json!({"job_id": id})),
                Err(e) => ToolOutput::error(format!("could not start {}: {e}", self.shell.name())),
            };
        }
        let timeout = input
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(self.default_timeout);
        let update = ctx.update.clone();
        let mut options = RunOptions::new(cwd);
        options.env = env;
        options.timeout = Some(timeout);
        options.cancel = ctx.cancel.clone();
        options.on_line = Some(Arc::new(move |line| update.send(line)));
        let result = match run(&self.shell, &input.command, options).await {
            Ok(result) => result,
            Err(e) => return ToolOutput::error(format!("could not start {}: {e}", self.shell.name())),
        };
        let mut text = result.output.render();
        if text.is_empty() {
            text.push_str("(no output)");
        }
        let (status, failed) = if result.timed_out {
            (
                format!(
                    "timed out after {} seconds; the process tree was killed",
                    timeout.as_secs()
                ),
                true,
            )
        } else if result.cancelled {
            ("aborted; the process tree was killed".to_string(), true)
        } else {
            match result.code {
                Some(code) => (format!("exit code: {code}"), false),
                None => ("killed by a signal".to_string(), true),
            }
        };
        ToolOutput {
            content: vec![azazel_ai::Block::text(format!("{text}\n{status}"))],
            is_error: failed,
            details: None,
        }
        .with_details(json!({"exit_code": result.code, "timed_out": result.timed_out}))
    }
}

pub struct JobTool {
    jobs: Arc<Jobs>,
}

impl JobTool {
    pub fn new(jobs: Arc<Jobs>) -> Self {
        JobTool { jobs }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum JobAction {
    Output,
    Kill,
    List,
}

#[derive(Deserialize)]
struct JobInput {
    action: JobAction,
    id: Option<String>,
    tail: Option<usize>,
}

#[async_trait]
impl Tool for JobTool {
    fn name(&self) -> &str {
        "job"
    }

    fn description(&self) -> &str {
        "Manage background jobs started by `shell` with `background: true`. `output` returns the last \
         `tail` lines (default 100) and the job's state, `kill` stops it, `list` shows every job."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "action": {"type": "string", "enum": ["output", "kill", "list"]},
                "id": {"type": "string", "description": "Job id; required for output and kill"},
                "tail": {"type": "integer", "minimum": 1}
            },
            "required": ["action"]
        })
    }

    fn access(&self, input: &Value) -> Access {
        if input.get("action").and_then(Value::as_str) == Some("kill") {
            Access::Exec
        } else {
            Access::Read
        }
    }

    fn subjects(&self, input: &Value) -> Vec<String> {
        input
            .get("action")
            .and_then(Value::as_str)
            .map(str::to_string)
            .into_iter()
            .collect()
    }

    async fn call(&self, input: Value, _: &ToolCtx) -> ToolOutput {
        let input: JobInput = match parse_input(input) {
            Ok(input) => input,
            Err(error) => return error,
        };
        if input.action == JobAction::List {
            let jobs = self.jobs.list();
            return ToolOutput::text(if jobs.is_empty() {
                "no background jobs".to_string()
            } else {
                jobs.join("\n")
            });
        }
        let Some(id) = input.id else {
            return ToolOutput::error("`id` is required for this action");
        };
        let unknown = || ToolOutput::error(format!("no job named `{id}`"));
        match input.action {
            JobAction::Output => match self.jobs.output(&id, input.tail.unwrap_or(DEFAULT_JOB_TAIL)) {
                Some(text) => ToolOutput::text(text),
                None => unknown(),
            },
            _ => {
                if self.jobs.kill(&id) {
                    ToolOutput::text(format!("stopping {id}"))
                } else {
                    unknown()
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use azazel_agent::{AgentInfo, Services, UpdateSink};

    use super::*;

    fn ctx(cwd: &Path, cancel: CancellationToken, update: UpdateSink) -> ToolCtx {
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
            cancel,
            update,
            services: Arc::new(Services::new()),
        }
    }

    fn tools(timeout: Duration) -> (ShellTool, JobTool, Arc<AgentEnv>) {
        let jobs = Arc::new(Jobs::new());
        let env = Arc::new(AgentEnv::default());
        (
            ShellTool::new(Shell::resolve(""), timeout, jobs.clone(), env.clone()),
            JobTool::new(jobs),
            env,
        )
    }

    fn echo_env(shell: &Shell, name: &str) -> String {
        if shell.is_posix() {
            format!("echo \"${name}\"")
        } else {
            format!("Write-Output $env:{name}")
        }
    }

    #[tokio::test]
    async fn runs_in_cwd_with_agent_env_streams_updates_and_reports_exit_code() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, _, env) = tools(Duration::from_secs(60));
        env.set(vec![("AZ_GUARD".into(), "on".into())]);
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let update = UpdateSink::new(move |line| sink.lock().unwrap().push(line));
        let command = format!("{}; exit 7", echo_env(&shell.shell, "AZ_GUARD"));
        let out = shell
            .call(
                json!({"command": command}),
                &ctx(dir.path(), CancellationToken::new(), update),
            )
            .await;
        assert_eq!(out.text_content(), "on\nexit code: 7");
        assert!(!out.is_error);
        assert_eq!(out.details.unwrap()["exit_code"], 7);
        assert_eq!(*seen.lock().unwrap(), ["on"]);
        assert!(shell.description().contains(shell.shell.name()));
    }

    #[tokio::test]
    async fn timeout_is_an_error_result_that_says_so() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, _, _) = tools(Duration::from_secs(60));
        let command = if shell.shell.is_posix() {
            "sleep 60"
        } else {
            "Start-Sleep 60"
        };
        let out = shell
            .call(
                json!({"command": command, "timeout_secs": 1}),
                &ctx(dir.path(), CancellationToken::new(), UpdateSink::default()),
            )
            .await;
        assert!(out.is_error);
        assert_eq!(
            out.text_content(),
            "(no output)\ntimed out after 1 seconds; the process tree was killed"
        );
    }

    #[tokio::test]
    async fn background_job_can_be_read_listed_and_killed() {
        let dir = tempfile::tempdir().unwrap();
        let (shell, job, _) = tools(Duration::from_secs(60));
        let command = if shell.shell.is_posix() {
            "echo ready; sleep 60"
        } else {
            "Write-Output ready; Start-Sleep 60"
        };
        let context = ctx(dir.path(), CancellationToken::new(), UpdateSink::default());
        let started = shell
            .call(json!({"command": command, "background": true}), &context)
            .await;
        let id = started.details.unwrap()["job_id"].as_str().unwrap().to_string();
        assert_eq!(id, "job-1");

        let mut text = String::new();
        for _ in 0..100 {
            text = job
                .call(json!({"action": "output", "id": id}), &context)
                .await
                .text_content();
            if text.starts_with("ready") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(text.starts_with("ready\n[job-1: running"), "{text}");
        assert!(
            job.call(json!({"action": "list"}), &context)
                .await
                .text_content()
                .starts_with("job-1  running")
        );

        assert_eq!(
            job.call(json!({"action": "kill", "id": id}), &context)
                .await
                .text_content(),
            "stopping job-1"
        );
        for _ in 0..100 {
            text = job
                .call(json!({"action": "output", "id": id}), &context)
                .await
                .text_content();
            if text.ends_with("[job-1: killed]") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        assert!(text.ends_with("[job-1: killed]"), "{text}");
        assert!(
            job.call(json!({"action": "kill", "id": "job-9"}), &context)
                .await
                .is_error
        );
        assert!(job.call(json!({"action": "output"}), &context).await.is_error);
    }
}
