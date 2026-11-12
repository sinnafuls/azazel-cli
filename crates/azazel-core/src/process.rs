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

/// Environment added to every process started on behalf of an agent (the git guard lives here).
/// Registered as a service; the user's own `!` commands do not use it.
#[derive(Default)]
pub struct AgentEnv {
    vars: std::sync::RwLock<Vec<(String, String)>>,
}

impl AgentEnv {
    pub fn set(&self, vars: Vec<(String, String)>) {
        *self
            .vars
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = vars;
    }

    pub fn vars(&self) -> Vec<(String, String)> {
        self.vars
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

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
) -> io::Result<Box<dyn ChildWrapper>> {
    let mut command = CommandWrap::with_new(&shell.program, |c| {
        c.args(shell.args(script))
            .current_dir(cwd)
            .envs(env.iter().map(|(k, v)| (k, v)))
            .stdin(if stdin { Stdio::piped() } else { Stdio::null() })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
    });
    #[cfg(unix)]
    command.wrap(process_wrap::tokio::ProcessGroup::leader());
    #[cfg(windows)]
    command.wrap(process_wrap::tokio::JobObject);
    command.wrap(KillOnDrop);
    command.spawn()
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
    let mut child = spawn(shell, script, &options.cwd, &options.env, options.stdin.is_some())?;
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
    async fn run_passes_env_cwd_and_stdin() {
        let shell = Shell::resolve("");
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("marker.txt"), "x").unwrap();
        let script = if shell.is_posix() {
            "cat; echo \" $AZ_TEST_VAR\"; ls"
        } else {
            "$input | Write-Output; Write-Output \" $env:AZ_TEST_VAR\"; Get-ChildItem -Name"
        };
        let mut options = RunOptions::new(dir.path().to_path_buf());
        options.env = vec![("AZ_TEST_VAR".into(), "from-env".into())];
        options.stdin = Some(b"from-stdin\n".to_vec());
        let text = run(&shell, script, options).await.unwrap().output.render();
        assert!(
            text.contains("from-stdin") && text.contains("from-env") && text.contains("marker.txt"),
            "{text}"
        );
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
}
