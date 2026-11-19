//! Running commands on behalf of agents: shell resolution, process-tree spawning, bounded output.

use std::collections::VecDeque;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use process_wrap::tokio::{ChildWrapper, CommandWrap, KillOnDrop};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::config::{ProcessPriority, WorktreeTarget};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShellKind {
    Bash,
    Sh,
    Pwsh,
    PowerShell,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Shell {
    pub program: PathBuf,
    pub kind: ShellKind,
}

impl Shell {
    /// `configured` (a program name or path) wins when it is not empty. Otherwise: on Unix `bash`
    /// then `sh`; on Windows `bash.exe` on `PATH` (not the WSL launcher), then the one shipped with
    /// Git, then `pwsh`, then `powershell`.
    pub fn resolve(configured: &str) -> Shell {
        let configured = configured.trim();
        if !configured.is_empty() {
            let program = which::which(configured).unwrap_or_else(|_| PathBuf::from(configured));
            let kind = Self::kind_of(&program);
            return Shell { program, kind };
        }
        if cfg!(windows) {
            if let Some(program) = windows_bash() {
                return Shell {
                    program,
                    kind: ShellKind::Bash,
                };
            }
            if let Ok(program) = which::which("pwsh") {
                return Shell {
                    program,
                    kind: ShellKind::Pwsh,
                };
            }
            let program = which::which("powershell").unwrap_or_else(|_| PathBuf::from("powershell"));
            return Shell {
                program,
                kind: ShellKind::PowerShell,
            };
        }
        match which::which("bash") {
            Ok(program) => Shell {
                program,
                kind: ShellKind::Bash,
            },
            Err(_) => Shell {
                program: which::which("sh").unwrap_or_else(|_| PathBuf::from("sh")),
                kind: ShellKind::Sh,
            },
        }
    }

    fn kind_of(program: &Path) -> ShellKind {
        let stem = program
            .file_stem()
            .map(|s| s.to_string_lossy().to_ascii_lowercase())
            .unwrap_or_default();
        match stem.as_str() {
            "pwsh" => ShellKind::Pwsh,
            "powershell" => ShellKind::PowerShell,
            "bash" => ShellKind::Bash,
            _ => ShellKind::Sh,
        }
    }

    pub fn name(&self) -> &'static str {
        match self.kind {
            ShellKind::Bash => "bash",
            ShellKind::Sh => "sh",
            ShellKind::Pwsh => "pwsh",
            ShellKind::PowerShell => "powershell",
        }
    }

    pub fn is_posix(&self) -> bool {
        matches!(self.kind, ShellKind::Bash | ShellKind::Sh)
    }

    /// Arguments that make the shell run `script`.
    fn args(&self, script: &str) -> Vec<String> {
        if self.is_posix() {
            vec!["-c".to_string(), script.to_string()]
        } else {
            vec![
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script.to_string(),
            ]
        }
    }
}

fn windows_bash() -> Option<PathBuf> {
    // `C:\Windows\System32\bash.exe` starts WSL, which sees a different filesystem.
    let on_path = which::which_all("bash").ok().and_then(|mut found| {
        found.find(|p| {
            !p.to_string_lossy()
                .to_ascii_lowercase()
                .contains("\\windows\\system32")
        })
    });
    on_path.or_else(|| {
        let git = which::which("git").ok()?;
        let bash = git.parent()?.parent()?.join("bin").join("bash.exe");
        bash.is_file().then_some(bash)
    })
}

/// Keeps the first `head` and the last `tail` lines of a stream and counts what fell between.
#[derive(Debug)]
pub struct OutputBuffer {
    head_limit: usize,
    tail_limit: usize,
    head: Vec<String>,
    tail: VecDeque<String>,
    lines: usize,
}

const MAX_LINE_CHARS: usize = 4000;

impl OutputBuffer {
    pub fn new(head: usize, tail: usize) -> Self {
        OutputBuffer {
            head_limit: head,
            tail_limit: tail,
            head: Vec::new(),
            tail: VecDeque::new(),
            lines: 0,
        }
    }

    pub fn push(&mut self, line: &str) {
        let line = match line.char_indices().nth(MAX_LINE_CHARS) {
            Some((end, _)) => format!("{}… [line truncated]", &line[..end]),
            None => line.to_string(),
        };
        self.lines += 1;
        if self.head.len() < self.head_limit {
            self.head.push(line);
            return;
        }
        if self.tail_limit == 0 {
            return;
        }
        if self.tail.len() == self.tail_limit {
            self.tail.pop_front();
        }
        self.tail.push_back(line);
    }

    pub fn lines(&self) -> usize {
        self.lines
    }

    pub fn render(&self) -> String {
        let mut out = self.head.join("\n");
        let omitted = self.lines - self.head.len() - self.tail.len();
        if omitted > 0 {
            out.push_str(&format!("\n[… {omitted} lines omitted …]"));
        }
        for line in &self.tail {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(line);
        }
        out
    }

    /// The last `n` lines seen (bounded by what the buffer kept).
    pub fn last(&self, n: usize) -> String {
        let kept: Vec<&String> = self.head.iter().chain(self.tail.iter()).collect();
        let start = kept.len().saturating_sub(n);
        kept[start..]
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// Environment added to every process started on behalf of an agent, the git push guard among
/// it. Registered as a service; the user's own `!` commands do not use it.
#[derive(Default)]
pub struct AgentEnv {
    vars: std::sync::RwLock<Vec<(String, String)>>,
    guard: std::sync::RwLock<Vec<(String, String)>>,
    priority: std::sync::RwLock<ProcessPriority>,
    target: std::sync::RwLock<(WorktreeTarget, bool)>,
}

impl AgentEnv {
    /// The operating-system priority processes started for agents run at.
    pub fn set_priority(&self, priority: ProcessPriority) {
        *self
            .priority
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = priority;
    }

    pub fn priority(&self) -> ProcessPriority {
        *self
            .priority
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where agents in a worktree build; `sccache` says whether that program is installed.
    pub fn set_worktree_target(&self, target: WorktreeTarget, sccache: bool) {
        *self
            .target
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = (target, sccache);
    }

    pub fn set(&self, vars: Vec<(String, String)>) {
        *self
            .vars
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = vars;
    }

    /// The environment that makes a push fail at the transport; sub-agents get it.
    pub fn set_guard(&self, guard: Vec<(String, String)>) {
        *self
            .guard
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = guard;
    }

    /// Everything, the push guard included: for processes no single agent asked for (hooks,
    /// checks, verify commands).
    pub fn vars(&self) -> Vec<(String, String)> {
        self.vars_in(None)
    }

    /// [`vars`](Self::vars) for a process that runs in `cwd`: in a worktree it also says where the
    /// build output goes.
    pub fn vars_in(&self, cwd: Option<&Path>) -> Vec<(String, String)> {
        let mut vars = self.vars_without_guard();
        vars.extend(self.build_cache(cwd, &|name| std::env::var_os(name).is_some()));
        vars.extend(
            self.guard
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .iter()
                .cloned(),
        );
        vars
    }

    /// What a process of the agent at `depth` gets: the lead (depth 0) may push, so it goes
    /// without the push guard.
    pub fn for_agent(&self, depth: u32, cwd: &Path) -> Vec<(String, String)> {
        match depth {
            0 => {
                let mut vars = self.vars_without_guard();
                vars.extend(self.build_cache(Some(cwd), &|name| std::env::var_os(name).is_some()));
                vars
            }
            _ => self.vars_in(Some(cwd)),
        }
    }

    /// What `[team] worktree_target` adds for a process in `cwd`. A variable the user's environment
    /// already sets (`is_set`) is left alone.
    fn build_cache(&self, cwd: Option<&Path>, is_set: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
        let (target, sccache) = *self
            .target
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut vars = Vec::new();
        let Some(main) = cwd.and_then(main_checkout) else {
            return vars;
        };
        if !main.join("Cargo.toml").is_file() {
            return vars;
        }
        match target {
            WorktreeTarget::Shared if !is_set("CARGO_TARGET_DIR") => {
                vars.push((
                    "CARGO_TARGET_DIR".to_string(),
                    main.join("target").display().to_string(),
                ));
            }
            WorktreeTarget::Sccache if sccache && !is_set("RUSTC_WRAPPER") => {
                vars.push(("RUSTC_WRAPPER".to_string(), "sccache".to_string()));
            }
            _ => {}
        }
        vars
    }

    fn vars_without_guard(&self) -> Vec<(String, String)> {
        self.vars
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

/// The checkout `cwd` is a worktree of, when azazel made it (`<main>/.azazel/worktrees/<id>`, with
/// `cwd` anywhere below `<id>`).
fn main_checkout(cwd: &Path) -> Option<PathBuf> {
    let named = |dir: &Path, name: &str| dir.file_name().is_some_and(|n| n == name);
    cwd.ancestors().find_map(|dir| {
        let worktrees = dir.parent()?;
        let azazel = worktrees.parent()?;
        if named(worktrees, "worktrees") && named(azazel, ".azazel") {
            azazel.parent().map(Path::to_path_buf)
        } else {
            None
        }
    })
}

/// The variables that keep the builds of agents from taking every core, from `[tools] build_jobs`:
/// `half` of `cores` (at least 2), a number, or `all` for none. A variable `is_set` is not set again.
pub fn build_jobs_env(setting: &str, cores: usize, is_set: &dyn Fn(&str) -> bool) -> Vec<(String, String)> {
    let setting = setting.trim();
    let jobs = match setting.parse::<usize>() {
        Ok(n) if n > 0 => n,
        _ if setting.eq_ignore_ascii_case("all") => return Vec::new(),
        _ => (cores / 2).max(2),
    };
    [
        ("CARGO_BUILD_JOBS", jobs.to_string()),
        ("MAKEFLAGS", format!("-j{jobs}")),
        ("CMAKE_BUILD_PARALLEL_LEVEL", jobs.to_string()),
    ]
    .into_iter()
    .filter(|(name, _)| !is_set(name))
    .map(|(name, value)| (name.to_string(), value))
    .collect()
}

/// `program` and `args` as they are started at `priority`. On Unix that is `nice -n <value>
/// program args...`: setting the priority in the child between fork and exec would take `unsafe`, and
/// `nice` execs the program, so the process id and the process group stay the same. Windows sets
/// it by creation flag instead ([`lower`]).
pub fn lowered(program: &Path, args: &[String], priority: ProcessPriority) -> (PathBuf, Vec<String>) {
    #[cfg(unix)]
    {
        let value = match priority {
            ProcessPriority::Normal => None,
            ProcessPriority::Low => Some("10"),
            ProcessPriority::Idle => Some("19"),
        };
        if let Some(value) = value
            && let Ok(nice) = which::which("nice")
        {
            let mut wrapped = vec!["-n".to_string(), value.to_string(), program.display().to_string()];
            wrapped.extend(args.iter().cloned());
            return (nice, wrapped);
        }
    }
    let _ = priority;
    (program.to_path_buf(), args.to_vec())
}

/// Makes the process `command` starts run at `priority` (Windows: `BELOW_NORMAL_PRIORITY_CLASS` or
/// `IDLE_PRIORITY_CLASS`, which what it starts inherits).
#[cfg(windows)]
pub fn lower(command: &mut CommandWrap, priority: ProcessPriority) {
    use windows::Win32::System::Threading::{BELOW_NORMAL_PRIORITY_CLASS, IDLE_PRIORITY_CLASS};
    let class = match priority {
        ProcessPriority::Normal => return,
        ProcessPriority::Low => BELOW_NORMAL_PRIORITY_CLASS,
        ProcessPriority::Idle => IDLE_PRIORITY_CLASS,
    };
    command.wrap(process_wrap::tokio::CreationFlags(class));
}

/// Lowers the priority of a process that is already running, for one started through a
/// pseudo-terminal, which takes no creation flags. What it starts afterwards inherits the class.
#[cfg(windows)]
pub fn lower_running(pid: u32, priority: ProcessPriority) {
    use winsafe::co::{PRIORITY_CLASS, PROCESS};
    let class = match priority {
        ProcessPriority::Normal => return,
        ProcessPriority::Low => PRIORITY_CLASS::BELOW_NORMAL,
        ProcessPriority::Idle => PRIORITY_CLASS::IDLE,
    };
    if let Ok(process) = winsafe::HPROCESS::OpenProcess(PROCESS::SET_INFORMATION, false, pid) {
        let _ = process.SetPriorityClass(class);
    }
}

#[cfg(not(windows))]
pub fn lower_running(_pid: u32, _priority: ProcessPriority) {}

/// Whether the process `pid` still runs. A pid another program has taken over since counts as
/// running: that is the safe answer for a caller that only acts on a process that is gone.
#[cfg(windows)]
pub fn pid_alive(pid: u32) -> bool {
    use winsafe::co::{ERROR, PROCESS};
    /// What `GetExitCodeProcess` says of a process that has not ended.
    const STILL_ACTIVE: u32 = 259;
    match winsafe::HPROCESS::OpenProcess(PROCESS::QUERY_LIMITED_INFORMATION, false, pid) {
        Ok(process) => process
            .GetExitCodeProcess()
            .map_or(true, |code| code == STILL_ACTIVE),
        // One that is not ours to open is still there.
        Err(e) => e == ERROR::ACCESS_DENIED,
    }
}

#[cfg(not(windows))]
pub fn pid_alive(pid: u32) -> bool {
    std::process::Command::new("kill")
        .args(["-0", &pid.to_string()])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        // Without `kill` there is no telling.
        .map_or(true, |status| status.success())
}

/// On Unix the priority goes in the command line, see [`lowered`].
#[cfg(not(windows))]
pub fn lower(_command: &mut CommandWrap, _priority: ProcessPriority) {}

/// Receives each output line of a running command.
pub type LineSink = Arc<dyn Fn(&str) + Send + Sync>;

pub struct RunOptions {
    pub cwd: PathBuf,
    /// Added to the inherited environment.
    pub env: Vec<(String, String)>,
    pub timeout: Option<Duration>,
    pub cancel: CancellationToken,
    /// Written to the child's stdin, which is then closed.
    pub stdin: Option<Vec<u8>>,
    /// Called with each output line as it arrives.
    pub on_line: Option<LineSink>,
    /// Lines to keep from the start and the end of the output.
    pub keep: (usize, usize),
    /// Cancelling this lets go of the command instead of ending it: [`run`] returns at once and
    /// hands the running process over in [`RunOutput::detached`].
    pub detach: Option<CancellationToken>,
    /// The operating-system priority the process and what it starts run at.
    pub priority: ProcessPriority,
}

impl RunOptions {
    pub fn new(cwd: PathBuf) -> Self {
        RunOptions {
            cwd,
            env: Vec::new(),
            timeout: None,
            cancel: CancellationToken::new(),
            stdin: None,
            on_line: None,
            keep: (100, 300),
            detach: None,
            priority: ProcessPriority::Normal,
        }
    }
}

#[derive(Debug)]
pub struct RunOutput {
    /// `None` when the process was killed.
    pub code: Option<i32>,
    /// stdout and stderr interleaved in arrival order.
    pub output: OutputBuffer,
    pub timed_out: bool,
    pub cancelled: bool,
    /// The process, still running, when the caller asked to let go of it.
    pub detached: Option<Detached>,
}

/// A command that was let go of while it ran.
pub struct Detached {
    /// The process tree; it dies when this is dropped.
    pub child: Box<dyn ChildWrapper>,
    /// Its output from here on, line by line.
    pub lines: mpsc::UnboundedReceiver<String>,
}

impl std::fmt::Debug for Detached {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Detached").field("pid", &self.child.id()).finish()
    }
}

/// Starts `script` in `shell` as the leader of its own process group (Unix) or job (Windows), so
/// the whole tree can be killed. Output pipes are captured; the tree dies if the handle is dropped.
pub fn spawn(
    shell: &Shell,
    script: &str,
    cwd: &Path,
    env: &[(String, String)],
    stdin: bool,
    priority: ProcessPriority,
) -> io::Result<Box<dyn ChildWrapper>> {
    spawn_program(&shell.program, &shell.args(script), cwd, env, stdin, priority)
}

/// [`spawn`] for a program started with `args` instead of a shell running a script.
pub fn spawn_program(
    program: &Path,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
    stdin: bool,
    priority: ProcessPriority,
) -> io::Result<Box<dyn ChildWrapper>> {
    let (program, args) = lowered(program, args, priority);
    let mut command = CommandWrap::with_new(&program, |c| {
        c.args(&args)
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    lower(&mut command, priority);
    detach_from_terminal(&mut command);
    command.wrap(KillOnDrop);
    command.spawn()
}

/// Starts what `command` runs in a session of its own (Unix) or a job (Windows). On Unix that
/// leaves it without a controlling terminal: a program that opens `/dev/tty` itself (`ssh` and
/// `sudo` asking for a password, `git` asking for credentials) cannot read the user's keys from
/// under the UI, nor write its prompt into the input. It fails instead, as with nobody to answer.
pub fn detach_from_terminal(command: &mut CommandWrap) {
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessSession);
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
}

/// A command for a helper program azazel starts itself: an MCP server, a language server, a debug
/// adapter, an installer, a Luau `azazel.exec`. Like agent commands it runs in a session of its
/// own with no terminal ([`detach_from_terminal`]), and it is stopped when its handle is dropped.
pub fn helper_command(
    program: impl AsRef<std::ffi::OsStr>,
    init: impl FnOnce(&mut tokio::process::Command),
) -> CommandWrap {
    let mut command = CommandWrap::with_new(program, init);
    detach_from_terminal(&mut command);
    command.wrap(KillOnDrop);
    command
}

/// Runs a [`helper_command`] to its end and collects what it printed, as `Command::output` does.
pub async fn helper_output(mut command: CommandWrap) -> std::io::Result<std::process::Output> {
    command
        .command_mut()
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let child = command.spawn()?;
    Box::into_pin(child.wait_with_output()).await
}

/// Environment that keeps programs an agent starts from asking for a password or credentials on
/// a terminal: nobody can answer there, and on Unix there is none (see [`detach_from_terminal`]).
pub fn no_prompts_env() -> Vec<(String, String)> {
    [
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GCM_INTERACTIVE", "never"),
        ("SSH_ASKPASS_REQUIRE", "never"),
    ]
    .into_iter()
    .map(|(name, value)| (name.to_string(), value.to_string()))
    .collect()
}

/// Forwards the lines of `pipe` to `tx`. Ends at end of stream or when the receiver is gone.
pub fn pump_lines(pipe: impl AsyncRead + Unpin + Send + 'static, tx: mpsc::UnboundedSender<String>) {
    tokio::spawn(async move {
        let mut reader = BufReader::new(pipe);
        let mut raw = Vec::new();
        loop {
            raw.clear();
            match reader.read_until(b'\n', &mut raw).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    let text = String::from_utf8_lossy(&raw);
                    let line = text.trim_end_matches(['\n', '\r']);
                    // A carriage return redraws a progress line; keep its final state.
                    let line = line.rsplit('\r').next().unwrap_or(line);
                    if tx.send(line.to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });
}

/// How long output is drained after the shell itself has exited; a grandchild that inherited the
/// pipes could otherwise keep them open forever.
const DRAIN_AFTER_EXIT: Duration = Duration::from_millis(300);

/// Runs `script` to completion, or until the timeout or cancellation, killing the process tree in
/// the latter cases.
pub async fn run(shell: &Shell, script: &str, options: RunOptions) -> io::Result<RunOutput> {
    run_program(&shell.program, &shell.args(script), options).await
}

/// [`run`] for a program started with `args` instead of a shell running a script.
pub async fn run_program(program: &Path, args: &[String], options: RunOptions) -> io::Result<RunOutput> {
    let mut child = spawn_program(
        program,
        args,
        &options.cwd,
        &options.env,
        options.stdin.is_some(),
        options.priority,
    )?;
    if let Some(bytes) = options.stdin
        && let Some(mut stdin) = child.stdin().take()
    {
        tokio::spawn(async move {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        });
    }
    let (tx, mut rx) = mpsc::unbounded_channel();
    if let Some(stdout) = child.stdout().take() {
        pump_lines(stdout, tx.clone());
    }
    if let Some(stderr) = child.stderr().take() {
        pump_lines(stderr, tx.clone());
    }
    drop(tx);

    let mut output = OutputBuffer::new(options.keep.0, options.keep.1);
    let take = |line: String, output: &mut OutputBuffer| {
        if let Some(on_line) = &options.on_line {
            on_line(&line);
        }
        output.push(&line);
    };
    let deadline = async {
        match options.timeout {
            Some(timeout) => tokio::time::sleep(timeout).await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(deadline);

    let let_go = async {
        match &options.detach {
            Some(detach) => detach.cancelled().await,
            None => std::future::pending().await,
        }
    };
    tokio::pin!(let_go);

    let mut timed_out = false;
    let mut cancelled = false;
    let mut detached = false;
    let mut pipes_open = true;
    let status = loop {
        tokio::select! {
            line = rx.recv(), if pipes_open => match line {
                Some(line) => take(line, &mut output),
                None => pipes_open = false,
            },
            status = child.wait() => break status.ok(),
            _ = &mut deadline => {
                timed_out = true;
                break None;
            }
            _ = options.cancel.cancelled() => {
                cancelled = true;
                break None;
            }
            _ = &mut let_go => {
                detached = true;
                break None;
            }
        }
    };
    if detached {
        return Ok(RunOutput {
            code: None,
            output,
            timed_out: false,
            cancelled: false,
            detached: Some(Detached { child, lines: rx }),
        });
    }
    if timed_out || cancelled {
        let _ = child.start_kill();
        let _ = child.wait().await;
    }
    if pipes_open {
        let drain = async {
            while let Some(line) = rx.recv().await {
                take(line, &mut output);
            }
        };
        let _ = tokio::time::timeout(DRAIN_AFTER_EXIT, drain).await;
    }
    // Whatever is still running in the group or job goes with the handle.
    let _ = child.start_kill();
    Ok(RunOutput {
        code: status.and_then(|s| s.code()),
        output,
        timed_out,
        cancelled,
        detached: None,
    })
}

#[cfg(test)]
mod tests {
    #[test]
    fn the_push_guard_goes_to_sub_agents_only() {
        let env = super::AgentEnv::default();
        env.set(vec![("A".into(), "1".into())]);
        env.set_guard(vec![("GIT_CONFIG_COUNT".into(), "6".into())]);
        let names = |vars: Vec<(String, String)>| vars.into_iter().map(|(name, _)| name).collect::<Vec<_>>();
        assert_eq!(names(env.for_agent(0, Path::new("."))), ["A"]);
        assert_eq!(names(env.for_agent(1, Path::new("."))), ["A", "GIT_CONFIG_COUNT"]);
        assert_eq!(names(env.vars()), ["A", "GIT_CONFIG_COUNT"]);
    }

    use std::sync::Mutex;
    use std::time::Instant;

    use super::*;

    #[test]
    fn output_buffer_keeps_head_and_tail_and_counts_the_gap() {
        let mut buffer = OutputBuffer::new(2, 3);
        for i in 1..=10 {
            buffer.push(&format!("line {i}"));
        }
        assert_eq!(
            buffer.render(),
            "line 1\nline 2\n[… 5 lines omitted …]\nline 8\nline 9\nline 10"
        );
        assert_eq!(buffer.last(2), "line 9\nline 10");
        assert_eq!(buffer.lines(), 10);

        let mut small = OutputBuffer::new(2, 3);
        small.push("only");
        assert_eq!(small.render(), "only");
        let mut long = OutputBuffer::new(1, 0);
        long.push(&"x".repeat(5000));
        assert!(long.render().ends_with("… [line truncated]"));
    }

    #[tokio::test]
    async fn run_merges_output_reports_exit_code_and_streams_lines() {
        let shell = Shell::resolve("");
        let script = if shell.is_posix() {
            "echo out; echo err 1>&2; exit 3"
        } else {
            "Write-Output out; [Console]::Error.WriteLine('err'); exit 3"
        };
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let mut options = RunOptions::new(std::env::temp_dir());
        options.on_line = Some(Arc::new(move |line| sink.lock().unwrap().push(line.to_string())));
        let result = run(&shell, script, options).await.unwrap();
        assert_eq!(result.code, Some(3));
        let mut lines = seen.lock().unwrap().clone();
        lines.sort();
        assert_eq!(lines, ["err", "out"]);
        assert!(result.output.render().contains("out") && result.output.render().contains("err"));
        assert!(!result.timed_out && !result.cancelled);
    }

    #[tokio::test]
    async fn timeout_and_cancel_kill_the_process_promptly() {
        let shell = Shell::resolve("");
        let script = if shell.is_posix() {
            "echo started; sleep 60"
        } else {
            "Write-Output started; Start-Sleep 60"
        };

        let started = Instant::now();
        let mut options = RunOptions::new(std::env::temp_dir());
        options.timeout = Some(Duration::from_millis(1500));
        let result = run(&shell, script, options).await.unwrap();
        assert!(result.timed_out && result.code.is_none());
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(result.output.render(), "started");

        let started = Instant::now();
        let options = RunOptions::new(std::env::temp_dir());
        let cancel = options.cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(1500)).await;
            cancel.cancel();
        });
        let result = run(&shell, script, options).await.unwrap();
        assert!(result.cancelled);
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn build_jobs_are_half_the_cores_unless_the_environment_or_the_setting_says_otherwise() {
        let none = |_: &str| false;
        let value = |vars: &[(String, String)], name: &str| {
            vars.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone())
        };
        let half = build_jobs_env("half", 16, &none);
        assert_eq!(value(&half, "CARGO_BUILD_JOBS").as_deref(), Some("8"));
        assert_eq!(value(&half, "MAKEFLAGS").as_deref(), Some("-j8"));
        assert_eq!(value(&half, "CMAKE_BUILD_PARALLEL_LEVEL").as_deref(), Some("8"));
        assert_eq!(
            value(&build_jobs_env("half", 1, &none), "CARGO_BUILD_JOBS").as_deref(),
            Some("2")
        );
        assert_eq!(
            value(&build_jobs_env("3", 16, &none), "CARGO_BUILD_JOBS").as_deref(),
            Some("3")
        );
        assert!(build_jobs_env("all", 16, &none).is_empty());
        // What the user's environment sets stays theirs; the rest is still filled in.
        let set = |name: &str| name == "CARGO_BUILD_JOBS";
        let partial = build_jobs_env("half", 16, &set);
        assert_eq!(value(&partial, "CARGO_BUILD_JOBS"), None);
        assert_eq!(value(&partial, "MAKEFLAGS").as_deref(), Some("-j8"));
    }

    fn worktree_of(main: &Path) -> PathBuf {
        let dir = main.join(".azazel").join("worktrees").join("coder-1");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        dir
    }

    #[test]
    fn a_worktree_agent_builds_into_the_main_checkouts_target_unless_told_otherwise() {
        let main = tempfile::tempdir().unwrap();
        std::fs::write(
            main.path().join("Cargo.toml"),
            "[package]
",
        )
        .unwrap();
        let dir = worktree_of(main.path());
        let deeper = dir.join("src");
        let env = AgentEnv::default();
        env.set_worktree_target(WorktreeTarget::Shared, false);
        let target = main.path().join("target").display().to_string();
        let found = |vars: Vec<(String, String)>| {
            vars.into_iter()
                .find(|(n, _)| n == "CARGO_TARGET_DIR")
                .map(|(_, v)| v)
        };
        assert_eq!(found(env.for_agent(1, &deeper)), Some(target.clone()));
        assert_eq!(found(env.vars_in(Some(&dir))), Some(target));
        // The main checkout itself, and a project that is not Cargo's, keep their own.
        assert_eq!(found(env.for_agent(1, main.path())), None);
        let other = tempfile::tempdir().unwrap();
        assert_eq!(found(env.for_agent(1, &worktree_of(other.path()))), None);
        env.set_worktree_target(WorktreeTarget::Own, true);
        assert_eq!(found(env.for_agent(1, &dir)), None);
    }

    #[test]
    fn sccache_is_used_only_when_it_is_installed() {
        let main = tempfile::tempdir().unwrap();
        std::fs::write(
            main.path().join("Cargo.toml"),
            "[package]
",
        )
        .unwrap();
        let dir = worktree_of(main.path());
        let env = AgentEnv::default();
        let wrapper = |env: &AgentEnv| {
            env.build_cache(Some(&dir), &|_| false)
                .into_iter()
                .find(|(n, _)| n == "RUSTC_WRAPPER")
        };
        env.set_worktree_target(WorktreeTarget::Sccache, false);
        assert_eq!(wrapper(&env), None);
        env.set_worktree_target(WorktreeTarget::Sccache, true);
        assert_eq!(
            wrapper(&env),
            Some(("RUSTC_WRAPPER".to_string(), "sccache".to_string()))
        );
        assert!(
            env.build_cache(Some(&dir), &|name| name == "RUSTC_WRAPPER")
                .is_empty()
        );
    }

    /// The priority the process `script` runs in reports for itself, or `None` where this machine
    /// has no way to ask.
    async fn own_priority(priority: ProcessPriority) -> Option<String> {
        let (shell, script) = if cfg!(windows) {
            let shell = ["pwsh", "powershell"].iter().find_map(|name| {
                which::which(name).ok().map(|program| Shell {
                    program,
                    kind: if *name == "pwsh" {
                        ShellKind::Pwsh
                    } else {
                        ShellKind::PowerShell
                    },
                })
            })?;
            (shell, "(Get-Process -Id $PID).PriorityClass")
        } else {
            // BSD `nice` without a program prints its usage, not the value: ask `ps`.
            which::which("ps").ok()?;
            (Shell::resolve(""), "ps -o nice= -p $$")
        };
        let mut options = RunOptions::new(std::env::temp_dir());
        options.priority = priority;
        let result = run(&shell, script, options).await.ok()?;
        Some(result.output.render().trim().to_string())
    }

    /// A program an agent starts must not share the user's terminal: one that opens `/dev/tty`
    /// (ssh asking for a password) would read the keys meant for the UI and write into its input.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_agent_command_leads_a_session_of_its_own_with_no_terminal() {
        let shell = Shell::resolve("");
        let script = "[ \"$(ps -o sid= -p $$ | tr -d ' ')\" = \"$$\" ] && echo leader; (: < /dev/tty) 2>/dev/null && echo tty || echo none";
        let mut options = RunOptions::new(std::env::temp_dir());
        options.priority = ProcessPriority::Normal;
        let result = run(&shell, script, options).await.unwrap();
        assert_eq!(result.output.render().trim(), "leader\nnone");
    }

    #[tokio::test]
    async fn a_lowered_process_runs_at_the_priority_the_setting_names() {
        // `normal` keeps whatever the test runner runs at (CI runners are often lowered
        // already), so the lowered levels are compared with it.
        let Some(normal) = own_priority(ProcessPriority::Normal).await else {
            return;
        };
        let (low, idle) = if cfg!(windows) {
            ("BelowNormal".to_string(), "Idle".to_string())
        } else {
            // `nice -n` adds to the runner's own value, which may be negative (macOS CI: -10).
            let base: i32 = normal.parse().unwrap_or(0);
            ((base + 10).min(19).to_string(), (base + 19).min(19).to_string())
        };
        for (priority, wanted) in [(ProcessPriority::Low, low), (ProcessPriority::Idle, idle)] {
            let Some(got) = own_priority(priority).await else {
                return;
            };
            assert_eq!(got, wanted, "{priority} (normal here: {normal})");
        }
    }
}
