//! Per-prompt checkpoints of the project tree, kept in a shadow git repository so that changes made
//! by shell commands are captured as well as those made by the file tools.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::CheckpointsConfig;
use crate::paths::Paths;

/// The shortest and longest time one git command may be given (`checkpoints.timeout_ms`).
const TIMEOUT_MS: std::ops::RangeInclusive<u64> = 1_000..=300_000;

struct State {
    ready: bool,
    disabled: Option<String>,
    /// The newest snapshot of this session.
    last: Option<String>,
    /// The tree of `last`, once it is known.
    last_tree: Option<String>,
}

pub struct Checkpoints {
    root: PathBuf,
    shadow: PathBuf,
    reference: String,
    max_files: u64,
    /// How long one git command may take.
    timeout: Duration,
    /// What `info/exclude` of the shadow repository holds.
    exclude: String,
    /// A snapshot of another session this one continues from (a fork), which its history
    /// starts at.
    seed: Option<String>,
    /// The git program; another one only in tests.
    program: OsString,
    state: Mutex<State>,
}

/// Why a git command did not do its work.
enum GitError {
    /// It ran longer than `checkpoints.timeout_ms` and was killed.
    TimedOut(String),
    Failed(String),
}

impl From<GitError> for String {
    fn from(error: GitError) -> String {
        match error {
            GitError::TimedOut(why) | GitError::Failed(why) => why,
        }
    }
}

/// Why `root` must not be snapshotted wholesale, if so.
fn unsafe_root(root: &Path, user_home: Option<&Path>) -> Option<&'static str> {
    if root.parent().is_none() {
        return Some("the project root is a filesystem root");
    }
    if user_home.is_some_and(|home| home == root) {
        return Some("the project root is your home directory");
    }
    None
}

impl Checkpoints {
    pub fn new(paths: &Paths, session_id: &str, config: &CheckpointsConfig) -> Self {
        let disabled = if !config.enabled {
            Some("checkpoints are turned off in the configuration".to_string())
        } else {
            unsafe_root(&paths.project_root, std::env::home_dir().as_deref()).map(str::to_string)
        };
        Checkpoints {
            root: paths.project_root.clone(),
            shadow: paths.shadow_git(),
            reference: format!("refs/azazel/{session_id}"),
            max_files: config.max_files,
            timeout: Duration::from_millis(config.timeout_ms.clamp(*TIMEOUT_MS.start(), *TIMEOUT_MS.end())),
            exclude: config
                .exclude
                .iter()
                .map(|pattern| format!("{pattern}\n"))
                .collect(),
            seed: None,
            program: "git".into(),
            state: Mutex::new(State {
                ready: false,
                disabled,
                last: None,
                last_tree: None,
            }),
        }
    }

    /// Starts this session's history at `commit`, a snapshot of the session it was forked from,
    /// when it has no history of its own yet. The snapshots the fork's entries name then stay
    /// reachable from the fork itself.
    pub fn seeded(mut self, commit: Option<String>) -> Self {
        self.seed = commit;
        self
    }

    /// Runs git against the shadow repository, insulated from the user's and the project's git
    /// configuration. A command that takes longer than the timeout is killed.
    async fn git(&self, args: &[&str]) -> Result<String, GitError> {
        self.git_with(args, None).await
    }

    /// [`Checkpoints::git`] with `input` on its standard input.
    async fn git_with(&self, args: &[&str], input: Option<String>) -> Result<String, GitError> {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut command = Command::new(&self.program);
        command
            .args(args)
            .current_dir(&self.root)
            .env("GIT_DIR", &self.shadow)
            .env("GIT_WORK_TREE", &self.root)
            .env("GIT_INDEX_FILE", self.shadow.join("index"))
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_AUTHOR_NAME", "azazel")
            .env("GIT_AUTHOR_EMAIL", "azazel@localhost")
            .env("GIT_COMMITTER_NAME", "azazel")
            .env("GIT_COMMITTER_EMAIL", "azazel@localhost")
            .env_remove("GIT_CONFIG_COUNT")
            .stdin(if input.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            // Dropping the command, which the timeout does, ends the process.
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .map_err(|e| GitError::Failed(format!("could not run git: {e}")))?;
        let stdin = child.stdin.take();
        // Fed while its output is read, so a long list cannot fill both pipes.
        let feed = async move {
            if let (Some(mut stdin), Some(input)) = (stdin, input) {
                let _ = stdin.write_all(input.as_bytes()).await;
            }
        };
        let run = async { tokio::join!(feed, child.wait_with_output()).1 };
        let output = match tokio::time::timeout(self.timeout, run).await {
            Ok(output) => output.map_err(|e| GitError::Failed(format!("could not run git: {e}")))?,
            Err(_) => {
                return Err(GitError::TimedOut(format!(
                    "git {} took longer than {} ms (`checkpoints.timeout_ms`)",
                    args.first().copied().unwrap_or_default(),
                    self.timeout.as_millis()
                )));
            }
        };
        if !output.status.success() {
            return Err(GitError::Failed(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn prepare(&self, state: &mut State) -> Result<(), String> {
        if !self.shadow.join("HEAD").exists() {
            std::fs::create_dir_all(&self.shadow).map_err(|e| format!("{}: {e}", self.shadow.display()))?;
            // `git init --bare` refuses to run with a work tree in the environment.
            let init = Command::new(&self.program)
                .args(["init", "--bare", "--quiet"])
                .arg(&self.shadow)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output();
            let init = tokio::time::timeout(self.timeout, init)
                .await
                .map_err(|_| {
                    format!(
                        "git init took longer than {} ms (`checkpoints.timeout_ms`)",
                        self.timeout.as_millis()
                    )
                })?
                .map_err(|e| format!("could not run git: {e}"))?;
            if !init.status.success() {
                return Err(format!(
                    "git init: {}",
                    String::from_utf8_lossy(&init.stderr).trim()
                ));
            }
            // Store and restore bytes exactly: no line-ending conversion, no filters, whatever the
            // project's own .gitattributes says (`info/attributes` takes precedence over it).
            self.git(&["config", "core.autocrlf", "false"]).await?;
            self.git(&["config", "core.longpaths", "true"]).await?;
            let info = self.shadow.join("info");
            std::fs::create_dir_all(&info).map_err(|e| e.to_string())?;
            std::fs::write(
                info.join("attributes"),
                "* -text -filter -ident -working-tree-encoding\n",
            )
            .map_err(|e| e.to_string())?;
        }
        // What is left out is the configuration's to say, each time a session starts.
        let exclude = self.shadow.join("info").join("exclude");
        if std::fs::read_to_string(&exclude).ok().as_deref() != Some(self.exclude.as_str()) {
            std::fs::write(&exclude, &self.exclude).map_err(|e| format!("{}: {e}", exclude.display()))?;
        }
        // Counting is for the first snapshot only: afterwards the index says what the tree holds.
        if !self.shadow.join("index").exists() {
            let untracked = self
                .git(&["ls-files", "--others", "--exclude-standard", "-z"])
                .await?;
            // `-z` terminates every entry with a NUL.
            let files = untracked.bytes().filter(|&b| b == 0).count() as u64;
            if files > self.max_files {
                return Err(format!(
                    "the project has {files} files, more than checkpoints.max_files ({})",
                    self.max_files
                ));
            }
        }
        state.last = self.read_reference().await;
        // A fork has no snapshot of its own yet: its history starts where its parent's stood.
        if state.last.is_none()
            && let Some(seed) = &self.seed
            && self
                .git(&["cat-file", "-e", &format!("{seed}^{{commit}}")])
                .await
                .is_ok()
        {
            self.write_reference(seed)?;
            state.last = Some(seed.clone());
        }
        Ok(())
    }

    /// The file of this session's reference. Only this type writes the reference, so it is always
    /// a loose one.
    fn reference_file(&self) -> PathBuf {
        self.shadow.join(&self.reference)
    }

    /// The last snapshot of this session, read without starting git where that is possible: every
    /// process started costs more than the read.
    async fn read_reference(&self) -> Option<String> {
        if let Ok(text) = std::fs::read_to_string(self.reference_file()) {
            return Some(text.trim().to_string()).filter(|commit| !commit.is_empty());
        }
        if !self.shadow.join("packed-refs").exists() {
            return None;
        }
        // Someone packed the repository's references by hand.
        self.git(&["rev-parse", "--quiet", "--verify", &self.reference])
            .await
            .ok()
    }

    fn write_reference(&self, commit: &str) -> Result<(), String> {
        let file = self.reference_file();
        let fail = |e: std::io::Error| format!("{}: {e}", file.display());
        if let Some(dir) = file.parent() {
            std::fs::create_dir_all(dir).map_err(fail)?;
        }
        let staged = file.with_extension("new");
        std::fs::write(&staged, format!("{commit}\n")).map_err(fail)?;
        std::fs::rename(&staged, &file).map_err(fail)
    }

    /// Why checkpoints are off, if they are.
    pub async fn disabled(&self) -> Option<String> {
        self.state.lock().await.disabled.clone()
    }

    /// Records the current tree. `Ok(None)` when checkpoints are disabled; the first call that
    /// finds them unusable disables them and returns the reason as `Err` once.
    pub async fn snapshot(&self, label: &str) -> Result<Option<String>, String> {
        let mut state = self.state.lock().await;
        if state.disabled.is_some() {
            return Ok(None);
        }
        if !state.ready {
            if which::which(&self.program).is_err() {
                let reason = "git is not installed".to_string();
                state.disabled = Some(reason.clone());
                return Err(reason);
            }
            if let Err(reason) = self.prepare(&mut state).await {
                state.disabled = Some(reason.clone());
                return Err(reason);
            }
            state.ready = true;
        }
        match self.commit(&mut state, label).await {
            Ok(commit) => Ok(Some(commit)),
            // A project git cannot snapshot in time would hold up every prompt: no more
            // snapshots in this session.
            Err(GitError::TimedOut(reason)) => {
                state.disabled = Some(reason.clone());
                Err(reason)
            }
            Err(GitError::Failed(reason)) => Err(reason),
        }
    }

    /// Commits the tree as it is now on top of the session's last snapshot. A tree that has not
    /// changed since then gets no commit of its own: the last snapshot stands for it.
    async fn commit(&self, state: &mut State, label: &str) -> Result<String, GitError> {
        self.git(&["add", "--all"]).await?;
        let tree = self.git(&["write-tree"]).await?;
        if let Some(last) = state.last.clone() {
            if state.last_tree.is_none() {
                state.last_tree = self
                    .git(&["rev-parse", "--quiet", "--verify", &format!("{last}^{{tree}}")])
                    .await
                    .ok();
            }
            if state.last_tree.as_deref() == Some(tree.as_str()) {
                return Ok(last);
            }
        }
        let mut args = vec!["commit-tree", tree.as_str(), "-m", label];
        let parent = state.last.clone();
        if let Some(parent) = &parent {
            args.extend(["-p", parent.as_str()]);
        }
        let commit = self.git(&args).await?;
        self.write_reference(&commit).map_err(GitError::Failed)?;
        state.last = Some(commit.clone());
        state.last_tree = Some(tree);
        Ok(commit)
    }

    /// `git diff --shortstat` between two snapshots; empty when they are identical.
    pub async fn shortstat(&self, from: &str, to: &str) -> String {
        self.git(&["diff", "--shortstat", from, to])
            .await
            .unwrap_or_default()
    }

    /// [`Checkpoints::shortstat`] of each of `from` against `to` in one git run, by snapshot.
    /// A snapshot identical to `to`, or one git cannot compare, has no entry.
    pub async fn shortstats(&self, from: &[String], to: &str) -> HashMap<String, String> {
        let input: String = from.iter().map(|commit| format!("{commit} {to}\n")).collect();
        let output = self
            .git_with(&["diff-tree", "--stdin", "-r", "-M", "--shortstat"], Some(input))
            .await
            .unwrap_or_default();
        // Each non-empty comparison prints the commit it started from, then its stat line.
        let mut stats = HashMap::new();
        let mut commit = None;
        for line in output.lines().map(str::trim).filter(|line| !line.is_empty()) {
            match commit.take() {
                None => commit = Some(line.to_string()),
                Some(commit) => {
                    stats.insert(commit, line.to_string());
                }
            }
        }
        stats
    }

    /// What changed between two snapshots: the diffstat, then the diff itself cut at `max_bytes`.
    /// Empty when they are identical or cannot be compared.
    pub async fn diff(&self, from: &str, to: &str, max_bytes: usize) -> String {
        let stat = self.git(&["diff", "--stat", from, to]).await.unwrap_or_default();
        if stat.trim().is_empty() {
            return String::new();
        }
        let mut diff = self
            .git(&["diff", "--no-color", "--no-ext-diff", from, to])
            .await
            .unwrap_or_default();
        if diff.len() > max_bytes {
            let mut cut = max_bytes;
            while !diff.is_char_boundary(cut) {
                cut -= 1;
            }
            diff.truncate(cut);
            diff.push_str("\n[diff cut here]");
        }
        format!("{}\n\n{diff}", stat.trim_end())
    }

    /// Puts the project tree back to `commit`. The current state is snapshotted first and its id
    /// returned, so the rewind itself can be undone. Ignored files are left alone. A rewind that
    /// fails part of the way is taken back: the files are put as they were before it.
    pub async fn restore(&self, commit: &str) -> Result<String, String> {
        let before = self
            .snapshot("before rewind")
            .await?
            .ok_or("checkpoints are disabled")?;
        let Err(failed) = self.git(&["read-tree", "--reset", "-u", commit]).await else {
            return Ok(before);
        };
        let failed = String::from(failed);
        Err(match self.git(&["read-tree", "--reset", "-u", &before]).await {
            Ok(_) => format!("{failed}; the files are as they were before the rewind"),
            Err(rollback) => format!(
                "{failed}; putting the files back as they were failed too: {}",
                String::from(rollback)
            ),
        })
    }

    /// Whether git would pack the repository: it judges the number of loose objects by one of
    /// their 256 folders, and so does this, without starting it.
    fn worth_packing(&self) -> bool {
        std::fs::read_dir(self.shadow.join("objects").join("17")).is_ok_and(|folder| folder.count() > 1)
    }

    /// Packs the shadow repository when it has gathered enough loose objects. For the end of a
    /// session that took snapshots; a failure is nobody's problem.
    pub async fn tidy(&self) {
        let state = self.state.lock().await;
        if !state.ready || state.disabled.is_some() || !self.worth_packing() {
            return;
        }
        let packed = self
            .git(&[
                "-c",
                "gc.auto=256",
                "-c",
                "gc.autoDetach=false",
                "gc",
                "--auto",
                "--quiet",
            ])
            .await;
        if let Err(why) = packed {
            tracing::debug!("checkpoints: {}", String::from(why));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fixture {
        _tmp: tempfile::TempDir,
        paths: Paths,
    }

    fn fixture() -> Fixture {
        let tmp = tempfile::tempdir().unwrap();
        let project = tmp.path().join("proj");
        std::fs::create_dir_all(project.join(".azazel")).unwrap();
        let home = tmp.path().join("home");
        let paths = Paths::discover(&project, Some(&home.to_string_lossy()));
        Fixture { _tmp: tmp, paths }
    }

    /// Git is given the longest time allowed: these tests are about what a snapshot holds, and a
    /// hosted runner can take more than the default 10 s to add a thousand files. The test of the
    /// limit sets its own.
    fn checkpoints(f: &Fixture, max_files: u64) -> Checkpoints {
        Checkpoints::new(
            &f.paths,
            "session-1",
            &CheckpointsConfig {
                enabled: true,
                max_files,
                timeout_ms: *TIMEOUT_MS.end(),
                ..CheckpointsConfig::default()
            },
        )
    }

    fn write(f: &Fixture, name: &str, content: &[u8]) {
        let path = f.paths.project_root.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    fn read(f: &Fixture, name: &str) -> Option<Vec<u8>> {
        std::fs::read(f.paths.project_root.join(name)).ok()
    }

    #[tokio::test]
    async fn rewind_restores_edits_and_removes_new_files_and_can_be_undone() {
        let f = fixture();
        let ck = checkpoints(&f, 100_000);
        ck.snapshot("prompt 1").await.unwrap().unwrap();
        write(&f, "a.txt", b"v1");
        let before_prompt_2 = ck.snapshot("prompt 2").await.unwrap().unwrap();
        // Prompt 2: a shell command creates one file and changes another.
        write(&f, "b.txt", b"new");
        write(&f, "a.txt", b"v2");
        write(&f, "sub/dir/c.txt", b"deep");

        let now = ck.snapshot("now").await.unwrap().unwrap();
        let stat = ck.shortstat(&before_prompt_2, &now).await;
        assert!(stat.contains("3 files changed"), "{stat}");

        let before_rewind = ck.restore(&before_prompt_2).await.unwrap();
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v1"[..]));
        assert_eq!(read(&f, "b.txt"), None);
        assert_eq!(read(&f, "sub/dir/c.txt"), None);

        ck.restore(&before_rewind).await.unwrap();
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v2"[..]));
        assert_eq!(read(&f, "b.txt").as_deref(), Some(&b"new"[..]));
        assert_eq!(read(&f, "sub/dir/c.txt").as_deref(), Some(&b"deep"[..]));
    }

    #[tokio::test]
    async fn bytes_survive_whatever_the_project_says_about_line_endings_and_ignored_files_are_left_alone() {
        let f = fixture();
        write(&f, ".gitattributes", b"* text=auto eol=lf\n");
        write(&f, ".gitignore", b"target/\n");
        write(&f, "crlf.txt", b"one\r\ntwo\r\n");
        write(&f, "target/out.bin", b"build output");
        let ck = checkpoints(&f, 100_000);
        let start = ck.snapshot("start").await.unwrap().unwrap();

        write(&f, "crlf.txt", b"changed\n");
        write(&f, "target/out.bin", b"rebuilt");
        ck.restore(&start).await.unwrap();
        assert_eq!(read(&f, "crlf.txt").as_deref(), Some(&b"one\r\ntwo\r\n"[..]));
        assert_eq!(read(&f, "target/out.bin").as_deref(), Some(&b"rebuilt"[..]));
        // The shadow repository lives outside the project.
        assert!(!f.paths.project_root.join(".git").exists());
        assert!(f.paths.shadow_git().join("HEAD").exists());
    }

    #[tokio::test]
    async fn a_new_session_object_continues_the_same_history() {
        let f = fixture();
        write(&f, "a.txt", b"v1");
        let first = checkpoints(&f, 100_000).snapshot("one").await.unwrap().unwrap();
        write(&f, "a.txt", b"v2");
        let resumed = checkpoints(&f, 100_000);
        resumed.snapshot("two").await.unwrap().unwrap();
        resumed.restore(&first).await.unwrap();
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v1"[..]));
    }

    /// A program that takes git's place and never finishes on its own.
    fn stuck_git(f: &Fixture) -> PathBuf {
        let dir = f.paths.home.join("bin");
        std::fs::create_dir_all(&dir).unwrap();
        if cfg!(windows) {
            let path = dir.join("git.cmd");
            std::fs::write(&path, "@echo off\r\n:again\r\ngoto again\r\n").unwrap();
            path
        } else {
            let path = dir.join("git");
            std::fs::write(&path, "#!/bin/sh\nexec sleep 600\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
            }
            // A test running beside this one may fork while the file is still open for writing;
            // until that child execs, running the script fails with "text file busy". Once one
            // run starts, no such copy of the write handle is left.
            for _ in 0..100 {
                match std::process::Command::new(&path).arg("probe").spawn() {
                    Ok(mut child) => {
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(20)),
                }
            }
            path
        }
    }

    #[tokio::test]
    async fn a_git_that_does_not_finish_is_killed_and_turns_checkpoints_off_for_the_session() {
        let f = fixture();
        write(&f, "a.txt", b"v1");
        let mut ck = checkpoints(&f, 100_000);
        ck.snapshot("one").await.unwrap().unwrap();

        ck.program = stuck_git(&f).into();
        ck.timeout = Duration::from_millis(300);
        write(&f, "a.txt", b"v2");
        let started = std::time::Instant::now();
        let reason = ck.snapshot("two").await.unwrap_err();
        assert_eq!(
            reason,
            "git add took longer than 300 ms (`checkpoints.timeout_ms`)"
        );
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the command was not waited for: {:?}",
            started.elapsed()
        );
        // Said once; after that the session goes on without snapshots.
        assert_eq!(ck.snapshot("three").await, Ok(None));
        assert_eq!(ck.disabled().await, Some(reason));

        // The setting is kept within what is sensible.
        let with = |timeout_ms| {
            Checkpoints::new(
                &f.paths,
                "s",
                &CheckpointsConfig {
                    timeout_ms,
                    ..CheckpointsConfig::default()
                },
            )
            .timeout
        };
        assert_eq!(with(5), Duration::from_secs(1));
        assert_eq!(with(2_500), Duration::from_millis(2_500));
        assert_eq!(with(u64::MAX), Duration::from_secs(300));
    }

    #[tokio::test]
    async fn a_tree_that_has_not_changed_gets_no_snapshot_of_its_own() {
        let f = fixture();
        write(&f, "a.txt", b"v1");
        let ck = checkpoints(&f, 100_000);
        let first = ck.snapshot("one").await.unwrap().unwrap();
        assert_eq!(ck.snapshot("two").await.unwrap().unwrap(), first);
        // Also for a session that is opened again, which has to look the tree up.
        let resumed = checkpoints(&f, 100_000);
        assert_eq!(resumed.snapshot("three").await.unwrap().unwrap(), first);

        write(&f, "a.txt", b"v2");
        let second = resumed.snapshot("four").await.unwrap().unwrap();
        assert_ne!(second, first);
        assert_eq!(
            resumed.git(&["rev-list", &second]).await.ok().unwrap(),
            format!("{second}\n{first}")
        );
        // Rewinding and coming back leaves no trace either: the tree is the one already kept.
        let before = resumed.restore(&first).await.unwrap();
        assert_eq!(before, second);
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v1"[..]));
    }

    #[tokio::test]
    async fn what_the_configuration_excludes_is_neither_kept_nor_put_back() {
        let f = fixture();
        let excluding = |patterns: &[&str]| {
            Checkpoints::new(
                &f.paths,
                "session-1",
                &CheckpointsConfig {
                    exclude: patterns.iter().map(ToString::to_string).collect(),
                    ..CheckpointsConfig::default()
                },
            )
        };
        write(&f, "a.txt", b"v1");
        write(&f, "scratch/notes.txt", b"v1");
        let ck = excluding(&["scratch/", "*.log"]);
        let start = ck.snapshot("start").await.unwrap().unwrap();
        let file = f.paths.shadow_git().join("info").join("exclude");
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "scratch/\n*.log\n");

        write(&f, "a.txt", b"v2");
        write(&f, "scratch/notes.txt", b"v2");
        write(&f, "build.log", b"noise");
        ck.restore(&start).await.unwrap();
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v1"[..]));
        assert_eq!(read(&f, "scratch/notes.txt").as_deref(), Some(&b"v2"[..]));
        assert_eq!(read(&f, "build.log").as_deref(), Some(&b"noise"[..]));

        // The list is the configuration's each time a session starts.
        excluding(&["*.tmp"]).snapshot("next").await.unwrap().unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "*.tmp\n");
    }

    #[tokio::test]
    async fn a_fork_continues_the_history_of_the_session_it_came_from() {
        let f = fixture();
        write(&f, "a.txt", b"v1");
        let parent = checkpoints(&f, 100_000).snapshot("one").await.unwrap().unwrap();
        let fork = |session: &str, seed: &str| {
            Checkpoints::new(&f.paths, session, &CheckpointsConfig::default()).seeded(Some(seed.to_string()))
        };

        // Nothing changed since: the fork stands on its parent's snapshot, under its own name.
        let forked = fork("session-2", &parent);
        assert_eq!(forked.snapshot("two").await.unwrap().unwrap(), parent);
        let own = f.paths.shadow_git().join("refs/azazel/session-2");
        assert_eq!(std::fs::read_to_string(&own).unwrap().trim(), parent);

        write(&f, "a.txt", b"v2");
        let next = forked.snapshot("three").await.unwrap().unwrap();
        assert_eq!(
            forked.git(&["rev-list", &next]).await.ok().unwrap(),
            format!("{next}\n{parent}")
        );
        // The parent's own history is where it was.
        let original = f.paths.shadow_git().join("refs/azazel/session-1");
        assert_eq!(std::fs::read_to_string(&original).unwrap().trim(), parent);

        // A snapshot the repository does not hold starts nothing: the history begins anew.
        write(&f, "a.txt", b"v3");
        let lost = fork("session-3", "0123456789012345678901234567890123456789");
        let alone = lost.snapshot("four").await.unwrap().unwrap();
        assert_eq!(lost.git(&["rev-list", &alone]).await.ok().unwrap(), alone);
    }

    #[tokio::test]
    async fn a_rewind_that_cannot_be_done_leaves_the_files_as_they_were() {
        let f = fixture();
        write(&f, "a.txt", b"v1");
        let ck = checkpoints(&f, 100_000);
        ck.snapshot("start").await.unwrap().unwrap();
        write(&f, "a.txt", b"v2");
        let failed = ck
            .restore("0123456789012345678901234567890123456789")
            .await
            .unwrap_err();
        assert!(
            failed.starts_with("git read-tree --reset -u 0123456789012345678901234567890123456789: ")
                && failed.ends_with("; the files are as they were before the rewind"),
            "{failed}"
        );
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v2"[..]));
    }

    /// A file another program holds open cannot be replaced on Windows, so the rewind stops
    /// after the files that sort before it were already put back.
    #[cfg(windows)]
    #[tokio::test]
    async fn a_rewind_that_fails_part_of_the_way_is_taken_back() {
        use std::os::windows::fs::OpenOptionsExt;

        let f = fixture();
        write(&f, "a.txt", b"v1");
        write(&f, "sub/b.txt", b"v1");
        let ck = checkpoints(&f, 100_000);
        let start = ck.snapshot("start").await.unwrap().unwrap();
        write(&f, "a.txt", b"v2 is longer");
        write(&f, "sub/b.txt", b"v2 is longer");

        // Others may read it (the snapshot before the rewind does), not write or delete it.
        const FILE_SHARE_READ: u32 = 1;
        let held = std::fs::OpenOptions::new()
            .read(true)
            .share_mode(FILE_SHARE_READ)
            .open(f.paths.project_root.join("sub/b.txt"))
            .unwrap();
        let failed = ck.restore(&start).await.unwrap_err();
        drop(held);
        assert!(
            failed.ends_with("; the files are as they were before the rewind"),
            "{failed}"
        );
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v2 is longer"[..]));
        assert_eq!(read(&f, "sub/b.txt").as_deref(), Some(&b"v2 is longer"[..]));
        // Once the file is free the same rewind goes through.
        ck.restore(&start).await.unwrap();
        assert_eq!(read(&f, "a.txt").as_deref(), Some(&b"v1"[..]));
        assert_eq!(read(&f, "sub/b.txt").as_deref(), Some(&b"v1"[..]));
    }

    #[tokio::test]
    async fn tidying_packs_a_repository_that_gathered_loose_objects_and_keeps_every_snapshot() {
        let f = fixture();
        let ck = checkpoints(&f, 100_000);
        let objects = || {
            let dir = f.paths.shadow_git().join("objects");
            std::fs::read_dir(dir)
                .unwrap()
                .filter_map(Result::ok)
                .filter(|entry| entry.file_name().len() == 2)
                .map(|entry| std::fs::read_dir(entry.path()).unwrap().count())
                .sum::<usize>()
        };
        // Before the first snapshot there is nothing to tidy and no git to run.
        ck.tidy().await;
        assert!(!f.paths.shadow_git().exists());

        // Git guesses the number of loose objects from one of its 256 folders, so it takes a
        // good many more than the threshold before it is sure.
        for round in 0..1500 {
            write(
                &f,
                &format!("src/file-{round}.txt"),
                format!("content {round}").as_bytes(),
            );
        }
        let first = ck.snapshot("many").await.unwrap().unwrap();
        write(&f, "src/file-0.txt", b"changed");
        ck.snapshot("more").await.unwrap().unwrap();
        assert!(objects() > 1500, "{}", objects());
        ck.tidy().await;
        assert!(objects() < 50, "{}", objects());
        ck.restore(&first).await.unwrap();
        assert_eq!(read(&f, "src/file-0.txt").as_deref(), Some(&b"content 0"[..]));
    }

    #[tokio::test]
    async fn too_many_files_or_an_unsafe_root_turn_checkpoints_off_with_one_notice() {
        let f = fixture();
        write(&f, "a.txt", b"1");
        write(&f, "b.txt", b"2");
        write(&f, "c.txt", b"3");
        let ck = checkpoints(&f, 2);
        let reason = ck.snapshot("p").await.unwrap_err();
        assert!(
            reason.contains("3 files") && reason.contains("max_files (2)"),
            "{reason}"
        );
        assert_eq!(ck.snapshot("p").await, Ok(None));
        assert_eq!(ck.disabled().await, Some(reason));

        let off = Checkpoints::new(
            &f.paths,
            "s",
            &CheckpointsConfig {
                enabled: false,
                max_files: 10,
                ..CheckpointsConfig::default()
            },
        );
        assert_eq!(off.snapshot("p").await, Ok(None));

        let home = Path::new(if cfg!(windows) {
            "C:\\Users\\me"
        } else {
            "/home/me"
        });
        assert!(unsafe_root(home, Some(home)).is_some());
        assert!(unsafe_root(Path::new(if cfg!(windows) { "C:\\" } else { "/" }), Some(home)).is_some());
        assert_eq!(unsafe_root(&home.join("proj"), Some(home)), None);
    }
}
