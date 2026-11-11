//! Per-prompt checkpoints of the project tree, kept in a shadow git repository so that changes made
//! by shell commands are captured as well as those made by the file tools.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::process::Command;
use tokio::sync::Mutex;

use crate::config::CheckpointsConfig;
use crate::paths::Paths;

struct State {
    ready: bool,
    disabled: Option<String>,
    /// The newest snapshot of this session.
    last: Option<String>,
}

pub struct Checkpoints {
    root: PathBuf,
    shadow: PathBuf,
    reference: String,
    max_files: u64,
    state: Mutex<State>,
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
            state: Mutex::new(State {
                ready: false,
                disabled,
                last: None,
            }),
        }
    }

    /// Runs git against the shadow repository, insulated from the user's and the project's git
    /// configuration.
    async fn git(&self, args: &[&str]) -> Result<String, String> {
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = Command::new("git")
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
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("could not run git: {e}"))?;
        if !output.status.success() {
            return Err(format!(
                "git {}: {}",
                args.join(" "),
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    }

    async fn prepare(&self, state: &mut State) -> Result<(), String> {
        if !self.shadow.join("HEAD").exists() {
            std::fs::create_dir_all(&self.shadow).map_err(|e| format!("{}: {e}", self.shadow.display()))?;
            // `git init --bare` refuses to run with a work tree in the environment.
            let init = Command::new("git")
                .args(["init", "--bare", "--quiet"])
                .arg(&self.shadow)
                .env_remove("GIT_DIR")
                .env_remove("GIT_WORK_TREE")
                .env_remove("GIT_INDEX_FILE")
                .stdin(Stdio::null())
                .output()
                .await
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
            std::fs::write(info.join("exclude"), ".azazel/worktrees/\n").map_err(|e| e.to_string())?;
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
            if which_git().is_none() {
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
        self.git(&["add", "--all"]).await?;
        let tree = self.git(&["write-tree"]).await?;
        let mut args = vec!["commit-tree", tree.as_str(), "-m", label];
        let parent = state.last.clone();
        if let Some(parent) = &parent {
            args.extend(["-p", parent.as_str()]);
        }
        let commit = self.git(&args).await?;
        self.write_reference(&commit)?;
        state.last = Some(commit.clone());
        Ok(Some(commit))
    }

    /// `git diff --shortstat` between two snapshots; empty when they are identical.
    pub async fn shortstat(&self, from: &str, to: &str) -> String {
        self.git(&["diff", "--shortstat", from, to])
            .await
            .unwrap_or_default()
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
    /// returned, so the rewind itself can be undone. Ignored files are left alone.
    pub async fn restore(&self, commit: &str) -> Result<String, String> {
        let before = self
            .snapshot("before rewind")
            .await?
            .ok_or("checkpoints are disabled")?;
        self.git(&["read-tree", "--reset", "-u", commit]).await?;
        Ok(before)
    }
}

fn which_git() -> Option<PathBuf> {
    which::which("git").ok()
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

    fn checkpoints(f: &Fixture, max_files: u64) -> Checkpoints {
        Checkpoints::new(
            &f.paths,
            "session-1",
            &CheckpointsConfig {
                enabled: true,
                max_files,
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
