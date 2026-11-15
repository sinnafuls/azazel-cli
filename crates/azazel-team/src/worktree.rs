//! Git worktrees for isolated sub-agents: each agent works on its own branch in its own directory, and
//! what it changed comes back to the main tree as a patch.
//!
//! Everything here runs against the user's repository, index and git configuration. Nothing is ever
//! committed, stashed, checked out or hard-reset in the main tree. Calls to [`apply`] for one repository
//! must not overlap, because each compares the index before and after its own `git apply`.

use std::collections::BTreeSet;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

use tokio::process::Command;

pub const NOT_A_REPOSITORY: &str = "isolation: worktree requires a git repository";
/// Prefixed to an isolated agent's report when the main tree had uncommitted changes.
pub const DIRTY_NOTE: &str = "worktree started from HEAD; uncommitted changes were not visible";

const NO_COMMITS: &str = "isolation: worktree requires a repository with at least one commit";

/// Paths staged in the index, NUL-terminated.
const STAGED_PATHS: [&str; 5] = ["diff", "--cached", "--name-only", "--no-renames", "-z"];
/// Paths with unresolved merge conflicts, NUL-terminated.
const UNMERGED_PATHS: [&str; 4] = ["diff", "--name-only", "--diff-filter=U", "-z"];

/// Most path bytes handed to one `git reset`, far below the Windows command-line limit of 32767 characters.
const RESET_BATCH_BYTES: usize = 16_000;
/// What one path costs on a command line besides its own bytes: the separator and a pair of quotes.
const ARGUMENT_OVERHEAD: usize = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Worktree {
    pub path: PathBuf,
    pub branch: String,
    /// The commit the worktree was created at.
    pub base: String,
    /// The main tree had uncommitted changes when the worktree was created.
    pub started_dirty: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Changes {
    /// `git diff --cached --binary HEAD` of the worktree.
    pub patch: Vec<u8>,
    /// `git diff --cached --stat HEAD`, trimmed.
    pub diffstat: String,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ApplyError {
    /// The patch was applied with conflicts in these paths (relative to the project root, `/` separators).
    #[error("conflicts in: {}", .0.join(", "))]
    Conflicts(Vec<String>),
    /// Nothing was applied; the text is git's explanation.
    #[error("{0}")]
    Failed(String),
}

/// Runs git in `dir` with the user's configuration and returns whatever it exited with. Variables that
/// would point git at another repository or index are dropped, so `dir` alone decides what is touched.
async fn run<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Output, String> {
    Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_COMMON_DIR")
        .env_remove("GIT_PREFIX")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not run git: {e}"))
}

/// Runs git in `dir` and returns its stdout; a non-zero exit is an error carrying git's stderr.
async fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<Vec<u8>, String> {
    let output = run(dir, args).await?;
    if output.status.success() {
        Ok(output.stdout)
    } else {
        Err(failure(args, &output))
    }
}

/// Git's own explanation of a failed command, or the command and its exit status when git gave none.
fn failure<S: AsRef<OsStr>>(args: &[S], output: &Output) -> String {
    let stderr = String::from_utf8_lossy(&output.stderr);
    let stderr = stderr.trim();
    if !stderr.is_empty() {
        return stderr.to_string();
    }
    let command: Vec<_> = args.iter().map(|arg| OsStr::new(arg).to_string_lossy()).collect();
    format!("git {} failed ({})", command.join(" "), output.status)
}

/// A path as git printed it, in the form a process argument takes.
#[cfg(unix)]
fn os_string(bytes: &[u8]) -> OsString {
    use std::os::unix::ffi::OsStrExt;
    OsStr::from_bytes(bytes).to_os_string()
}

/// A path as git printed it, in the form a process argument takes.
#[cfg(not(unix))]
fn os_string(bytes: &[u8]) -> OsString {
    OsString::from(String::from_utf8_lossy(bytes).into_owned())
}

/// The entries of NUL-terminated (`-z`) git output, sorted.
fn nul_terminated(output: &[u8]) -> BTreeSet<&[u8]> {
    output
        .split(|&byte| byte == 0)
        .filter(|entry| !entry.is_empty())
        .collect()
}

/// Accepts only names that are safe both as one component of a branch name and as a directory name.
fn checked_name<'a>(what: &str, name: &'a str) -> Result<&'a str, String> {
    let safe = |byte: u8| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-';
    if !name.is_empty() && name.bytes().all(safe) {
        Ok(name)
    } else {
        Err(format!(
            "invalid {what} {name:?}: only letters, digits, `_` and `-` are allowed"
        ))
    }
}

/// The top level of the work tree containing `dir`, and where `dir` lies inside it in git's notation:
/// empty at the top level, otherwise `/`-separated with a trailing `/`.
async fn top_level(dir: &Path) -> Result<(PathBuf, String), String> {
    let output = git(dir, &["rev-parse", "--show-toplevel", "--show-prefix"]).await?;
    let mut lines = output.split(|&byte| byte == b'\n');
    let top = lines.next().unwrap_or_default();
    let prefix = lines.next().unwrap_or_default();
    if top.is_empty() {
        return Err(format!("{} is not inside a git work tree", dir.display()));
    }
    Ok((
        PathBuf::from(os_string(top)),
        String::from_utf8_lossy(prefix).into_owned(),
    ))
}

/// Rewrites `path`, relative to the top level of a work tree, as relative to the directory `prefix` inside
/// that tree (`prefix` in the notation of [`top_level`]).
fn relative_to_prefix(prefix: &str, path: &str) -> String {
    let base: Vec<&str> = prefix.split('/').filter(|part| !part.is_empty()).collect();
    let parts: Vec<&str> = path.split('/').collect();
    let shared = base.iter().zip(&parts).take_while(|(a, b)| a == b).count();
    let mut relative = vec![".."; base.len() - shared];
    relative.extend_from_slice(&parts[shared..]);
    relative.join("/")
}

/// The arguments of a `git diff` of the index against commit `base` whose output `git apply` accepts
/// whatever the user's diff configuration says; `format` selects what is printed.
fn diff_against<'a>(format: &'a str, base: &'a str) -> [&'a str; 12] {
    [
        "-c",
        "core.quotePath=false",
        "diff",
        "--cached",
        format,
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--no-renames",
        "--src-prefix=a/",
        "--dst-prefix=b/",
        base,
    ]
}

/// Splits `paths` into runs that each cost at most `limit` bytes on a command line; a path costing more
/// than that gets a run of its own.
fn batches<'a>(paths: &BTreeSet<&'a [u8]>, limit: usize) -> Vec<Vec<&'a [u8]>> {
    let mut all: Vec<Vec<&'a [u8]>> = Vec::new();
    let mut used = 0;
    for &path in paths {
        let cost = path.len() + ARGUMENT_OVERHEAD;
        match all.last_mut() {
            Some(batch) if used + cost <= limit => {
                batch.push(path);
                used += cost;
            }
            _ => {
                all.push(vec![path]);
                used = cost;
            }
        }
    }
    all
}

/// Puts the index entries of `paths` back to what HEAD holds, leaving the files themselves alone.
async fn unstage(top: &Path, paths: &BTreeSet<&[u8]>) -> Result<(), String> {
    for batch in batches(paths, RESET_BATCH_BYTES) {
        let mut args = Vec::from(["--literal-pathspecs", "reset", "-q", "--"].map(OsString::from));
        args.extend(batch.into_iter().map(os_string));
        git(top, args.as_slice()).await?;
    }
    Ok(())
}

/// `git worktree add <worktrees_dir>/<id> -b azazel/<session_short>/<id> HEAD`.
/// Fails with [`NOT_A_REPOSITORY`] when `project_root` is not inside a git work tree, and with a
/// readable message when the repository has no commit yet or the worktree/branch already exists.
pub async fn create(
    project_root: &Path,
    worktrees_dir: &Path,
    session_short: &str,
    id: &str,
) -> Result<Worktree, String> {
    let session_short = checked_name("session id", session_short)?;
    let id = checked_name("agent id", id)?;

    let inside = run(project_root, &["rev-parse", "--is-inside-work-tree"]).await?;
    if !inside.status.success() || inside.stdout.trim_ascii() != b"true" {
        let reason = String::from_utf8_lossy(&inside.stderr).trim().to_string();
        tracing::debug!(project = %project_root.display(), %reason, "not inside a git work tree");
        return Err(NOT_A_REPOSITORY.to_string());
    }
    let head = run(project_root, &["rev-parse", "--verify", "--quiet", "HEAD"]).await?;
    if !head.status.success() {
        return Err(NO_COMMITS.to_string());
    }

    let path = std::path::absolute(worktrees_dir.join(id))
        .map_err(|e| format!("{}: {e}", worktrees_dir.display()))?;
    if tokio::fs::try_exists(&path)
        .await
        .map_err(|e| format!("{}: {e}", path.display()))?
    {
        return Err(format!("worktree {} already exists", path.display()));
    }
    let branch = format!("azazel/{session_short}/{id}");
    let reference = format!("refs/heads/{branch}");
    let existing = run(
        project_root,
        &["show-ref", "--verify", "--quiet", reference.as_str()],
    )
    .await?;
    if existing.status.success() {
        return Err(format!("branch {branch} already exists"));
    }

    // The whole repository counts, wherever the project root lies in it; `.azazel/` holds the worktrees.
    let status = [
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--",
        ":/",
        ":(exclude).azazel",
    ];
    let started_dirty = !git(project_root, &status).await?.is_empty();

    let add = [
        OsStr::new("worktree"),
        OsStr::new("add"),
        path.as_os_str(),
        OsStr::new("-b"),
        OsStr::new(&branch),
        OsStr::new("HEAD"),
    ];
    git(project_root, &add)
        .await
        .map_err(|e| format!("could not create worktree: {e}"))?;
    Ok(Worktree {
        path,
        branch,
        base: String::from_utf8_lossy(&head.stdout).trim().to_string(),
        started_dirty,
    })
}

/// Stages everything in the worktree (`git -C <wt> add -A`) and returns what changed since the commit
/// it started from, including anything committed there meanwhile; `None` when nothing did. Fails
/// without staging anything when the directory is no longer the top of its own work tree, where git
/// would act on the enclosing main repository instead.
pub async fn changes(worktree: &Worktree) -> Result<Option<Changes>, String> {
    let dir = worktree.path.as_path();
    let position = git(dir, &["rev-parse", "--show-prefix"]).await?;
    if !position.trim_ascii().is_empty() {
        return Err(format!("{} is no longer a git worktree", dir.display()));
    }
    git(dir, &["add", "-A"]).await?;
    let patch = git(dir, &diff_against("--binary", &worktree.base)).await?;
    if patch.is_empty() {
        return Ok(None);
    }
    let diffstat = git(dir, &diff_against("--stat", &worktree.base)).await?;
    Ok(Some(Changes {
        patch,
        diffstat: String::from_utf8_lossy(&diffstat).trim().to_string(),
    }))
}

/// What an agent has done in its worktree so far, read without touching its index:
/// `git status --short` and `git log --oneline <base>..HEAD`.
pub async fn progress(worktree: &Worktree) -> Result<crate::status::WorktreeProgress, String> {
    let dir = worktree.path.as_path();
    let lines = |bytes: Vec<u8>| -> Vec<String> {
        String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(str::to_string)
            .collect()
    };
    let changed = git(dir, &["status", "--short", "--untracked-files=all"]).await?;
    let range = format!("{}..HEAD", worktree.base);
    let commits = git(dir, &["log", "--oneline", range.as_str()]).await?;
    Ok(crate::status::WorktreeProgress {
        path: worktree.path.clone(),
        changed: lines(changed),
        commits: lines(commits),
    })
}

/// Applies `patch` (a file written from [`Changes::patch`]) to the main tree with `git apply --3way`.
/// On success the changes are left as ordinary unstaged modifications (new files untracked), except
/// for paths the user already had staged before the call, which are left staged; nothing else in the
/// user's index is touched. On conflict the conflict markers are left in the files for the caller to
/// resolve and the index is put back the same way, so a conflicted path the user had not staged shows
/// as an ordinary modification. A patch that cannot be applied at all changes nothing.
pub async fn apply(project_root: &Path, patch: &Path) -> Result<(), ApplyError> {
    let patch =
        std::path::absolute(patch).map_err(|e| ApplyError::Failed(format!("{}: {e}", patch.display())))?;
    // A patch names paths from the top of the repository, and git skips those outside its directory.
    let (top, prefix) = top_level(project_root).await.map_err(ApplyError::Failed)?;

    // `git apply` trusts the index's cached file times and rejects a file that was merely touched; bring
    // them up to date first. Its exit status says nothing about whether the patch will apply.
    run(&top, &["update-index", "-q", "--refresh"])
        .await
        .map_err(ApplyError::Failed)?;

    let staged_before = git(&top, &STAGED_PATHS).await.map_err(ApplyError::Failed)?;
    let staged_before = nul_terminated(&staged_before);
    let unmerged_before = git(&top, &UNMERGED_PATHS).await.map_err(ApplyError::Failed)?;
    let unmerged_before = nul_terminated(&unmerged_before);

    let command = [
        OsStr::new("apply"),
        OsStr::new("--3way"),
        OsStr::new("--whitespace=nowarn"),
        patch.as_os_str(),
    ];
    let applied = run(&top, &command).await.map_err(ApplyError::Failed)?;

    let staged_after = git(&top, &STAGED_PATHS).await.map_err(ApplyError::Failed)?;
    let staged_after = nul_terminated(&staged_after);
    let unmerged_after = git(&top, &UNMERGED_PATHS).await.map_err(ApplyError::Failed)?;
    let unmerged_after = nul_terminated(&unmerged_after);

    let newly_staged: BTreeSet<&[u8]> = staged_after
        .union(&unmerged_after)
        .copied()
        .filter(|path| !staged_before.contains(path))
        .collect();
    unstage(&top, &newly_staged).await.map_err(ApplyError::Failed)?;

    let mut conflicts: Vec<String> = unmerged_after
        .difference(&unmerged_before)
        .map(|path| relative_to_prefix(&prefix, &String::from_utf8_lossy(path)))
        .collect();
    if !conflicts.is_empty() {
        conflicts.sort();
        return Err(ApplyError::Conflicts(conflicts));
    }
    if !applied.status.success() {
        return Err(ApplyError::Failed(failure(&command, &applied)));
    }
    Ok(())
}

/// Removes the worktree directory (`git worktree remove --force`) and deletes its branch. Missing
/// pieces are not an error (it may be called twice). A directory git no longer knows as a worktree,
/// which is what a removal interrupted by a locked file leaves behind, is deleted directly; a directory
/// holding a repository of its own never is.
pub async fn remove(project_root: &Path, worktree: &Worktree) -> Result<(), String> {
    let path = worktree.path.as_path();
    let command = [
        OsStr::new("worktree"),
        OsStr::new("remove"),
        OsStr::new("--force"),
        path.as_os_str(),
    ];
    if let Err(message) = git(project_root, &command).await {
        let exists = tokio::fs::try_exists(path)
            .await
            .map_err(|e| format!("{}: {e}", path.display()))?;
        if exists {
            let own_repository = tokio::fs::metadata(path.join(".git"))
                .await
                .is_ok_and(|meta| meta.is_dir());
            if own_repository {
                return Err(message);
            }
            tokio::fs::remove_dir_all(path)
                .await
                .map_err(|e| format!("could not remove {}: {e}", path.display()))?;
        }
    }

    let reference = format!("refs/heads/{}", worktree.branch);
    let lookup = ["show-ref", "--verify", "--quiet", reference.as_str()];
    let found = run(project_root, &lookup).await?;
    match found.status.code() {
        Some(0) => git(project_root, &["branch", "-D", worktree.branch.as_str()])
            .await
            .map(drop),
        Some(1) => Ok(()),
        _ => Err(failure(&lookup, &found)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SESSION: &str = "s1";
    const A_COMMITTED: &[u8] = b"one\ntwo\nthree\n";
    const A_EDITED: &[u8] = b"one\nTWO\nthree\n";
    const B_COMMITTED: &[u8] = b"1\n2\n3\n4\n5\n6\n7\n8\n9\n";
    const B_LAST_LINE_EDITED: &[u8] = b"1\n2\n3\n4\n5\n6\n7\n8\nNINE\n";
    const BINARY: &[u8] = &[0, 159, 146, 150, 0, 1, 2, 255];
    const NEW_FILE: &str = "notes for caf\u{e9}.txt";

    /// Runs git for test setup and inspection, panicking with git's stderr when it fails.
    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn write(dir: &Path, name: &str, content: &[u8]) {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    struct Repo {
        tmp: tempfile::TempDir,
        root: PathBuf,
    }

    impl Repo {
        fn worktrees(&self) -> PathBuf {
            self.root.join(".azazel").join("worktrees")
        }

        fn write(&self, name: &str, content: &[u8]) {
            write(&self.root, name, content);
        }

        fn read(&self, name: &str) -> Option<Vec<u8>> {
            std::fs::read(self.root.join(name)).ok()
        }

        fn git(&self, args: &[&str]) -> String {
            git_ok(&self.root, args)
        }

        /// Commits every change to tracked files.
        fn commit_tracked(&self, message: &str) {
            self.git(&["commit", "--quiet", "--all", "-m", message]);
        }

        fn staged(&self) -> String {
            self.git(&["diff", "--cached", "--name-only"])
        }

        fn branches(&self) -> String {
            self.git(&["branch", "--list", "azazel/*"])
        }

        fn registered_worktrees(&self) -> usize {
            let list = self.git(&["worktree", "list", "--porcelain"]);
            list.lines().filter(|line| line.starts_with("worktree ")).count()
        }

        async fn worktree(&self, id: &str) -> Worktree {
            create(&self.root, &self.worktrees(), SESSION, id).await.unwrap()
        }

        /// Collects the worktree's changes into a patch file outside the repository.
        async fn patch(&self, worktree: &Worktree) -> PathBuf {
            let changed = changes(worktree).await.unwrap().unwrap();
            let file = self
                .tmp
                .path()
                .join(format!("{}.patch", worktree.branch.replace('/', "-")));
            std::fs::write(&file, &changed.patch).unwrap();
            file
        }
    }

    fn empty_repo() -> Repo {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("proj");
        std::fs::create_dir_all(&root).unwrap();
        git_ok(&root, &["init", "--quiet"]);
        for (key, value) in [
            ("user.name", "Test"),
            ("user.email", "test@localhost"),
            ("core.autocrlf", "false"),
            ("commit.gpgsign", "false"),
        ] {
            git_ok(&root, &["config", key, value]);
        }
        Repo { tmp, root }
    }

    /// A repository with one commit holding `a.txt`, `b.txt`, `gone.txt` and `sub/s.txt`.
    fn committed_repo() -> Repo {
        let repo = empty_repo();
        repo.write("a.txt", A_COMMITTED);
        repo.write("b.txt", B_COMMITTED);
        repo.write("gone.txt", b"bye\n");
        repo.write("sub/s.txt", b"s\n");
        repo.git(&["add", "-A"]);
        repo.git(&["commit", "--quiet", "-m", "initial"]);
        repo
    }

    /// An agent's work: an edit, a new file with a space and a non-ASCII letter in its name, a deletion
    /// and a binary file.
    fn agent_edits(worktree: &Worktree) {
        write(&worktree.path, "a.txt", A_EDITED);
        write(&worktree.path, NEW_FILE, b"hello\n");
        write(&worktree.path, "blob.bin", BINARY);
        std::fs::remove_file(worktree.path.join("gone.txt")).unwrap();
    }

    #[tokio::test]
    async fn create_checks_out_head_on_a_branch_named_after_session_and_agent() {
        let repo = committed_repo();
        let worktree = repo.worktree("scout-1").await;

        assert_eq!(
            worktree,
            Worktree {
                path: std::path::absolute(repo.worktrees().join("scout-1")).unwrap(),
                branch: "azazel/s1/scout-1".to_string(),
                base: repo.git(&["rev-parse", "HEAD"]).trim().to_string(),
                started_dirty: false,
            }
        );
        assert_eq!(std::fs::read(worktree.path.join("a.txt")).unwrap(), A_COMMITTED);
        assert_eq!(
            git_ok(&worktree.path, &["rev-parse", "--abbrev-ref", "HEAD"]),
            "azazel/s1/scout-1"
        );
        // The worktrees kept under `.azazel/` do not make the main tree count as dirty.
        assert!(!repo.worktree("scout-2").await.started_dirty);
    }

    #[tokio::test]
    async fn create_notes_uncommitted_changes_in_the_main_tree() {
        let edited = committed_repo();
        edited.write("a.txt", b"edited, not committed\n");
        let worktree = edited.worktree("coder-1").await;
        assert!(worktree.started_dirty);
        assert_eq!(std::fs::read(worktree.path.join("a.txt")).unwrap(), A_COMMITTED);

        let untracked = committed_repo();
        untracked.write("draft.txt", b"new\n");
        let worktree = untracked.worktree("coder-1").await;
        assert!(worktree.started_dirty);
        assert!(!worktree.path.join("draft.txt").exists());
    }

    #[tokio::test]
    async fn create_in_a_repository_without_commits_fails_readably() {
        let repo = empty_repo();
        let error = create(&repo.root, &repo.worktrees(), SESSION, "scout-1")
            .await
            .unwrap_err();
        assert_eq!(error, NO_COMMITS);
        assert!(!repo.worktrees().exists());
    }

    #[tokio::test]
    async fn create_refuses_to_reuse_a_worktree_or_a_branch() {
        let repo = committed_repo();
        let first = repo.worktree("scout-1").await;

        let same_directory = create(&repo.root, &repo.worktrees(), SESSION, "scout-1")
            .await
            .unwrap_err();
        assert!(
            same_directory.contains("scout-1") && same_directory.ends_with("already exists"),
            "{same_directory}"
        );

        let elsewhere = repo.tmp.path().join("elsewhere");
        let same_branch = create(&repo.root, &elsewhere, SESSION, "scout-1")
            .await
            .unwrap_err();
        assert_eq!(same_branch, "branch azazel/s1/scout-1 already exists");
        assert!(!elsewhere.join("scout-1").exists());

        assert_eq!(std::fs::read(first.path.join("a.txt")).unwrap(), A_COMMITTED);
        assert_eq!(repo.registered_worktrees(), 2);
    }

    #[tokio::test]
    async fn create_rejects_names_that_are_unsafe_in_a_ref() {
        let repo = committed_repo();
        for bad in ["", "a/b", "a b", "..", "x.lock", "caf\u{e9}", "a\\b"] {
            let as_id = create(&repo.root, &repo.worktrees(), SESSION, bad).await;
            assert!(as_id.is_err(), "agent id {bad:?}");
            let as_session = create(&repo.root, &repo.worktrees(), bad, "scout-1").await;
            assert!(as_session.is_err(), "session id {bad:?}");
        }
        assert!(!repo.worktrees().exists());
        assert_eq!(repo.branches(), "");
    }

    #[tokio::test]
    async fn changes_cover_edits_additions_deletions_and_binary_files() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        agent_edits(&worktree);

        let found = changes(&worktree).await.unwrap().unwrap();

        for name in ["a.txt", NEW_FILE, "gone.txt", "blob.bin"] {
            assert!(
                found.diffstat.contains(name),
                "{name} missing from:\n{}",
                found.diffstat
            );
        }
        assert!(!found.diffstat.contains("b.txt"), "{}", found.diffstat);
        assert_eq!(found.diffstat, found.diffstat.trim());
        let patch = String::from_utf8_lossy(&found.patch).into_owned();
        assert!(patch.contains("+TWO"), "{patch}");
        assert!(patch.contains(&format!("b/{NEW_FILE}")), "{patch}");
        assert!(patch.contains("deleted file mode"), "{patch}");
        assert!(patch.contains("GIT binary patch"), "{patch}");
        // Asking again reports the same changes.
        assert_eq!(changes(&worktree).await.unwrap(), Some(found));
    }

    #[tokio::test]
    async fn changes_refuses_a_directory_that_is_no_longer_a_worktree() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        // Without its link the directory is just a folder inside the main repository.
        std::fs::remove_file(worktree.path.join(".git")).unwrap();
        repo.write("a.txt", b"the user's own edit\n");

        assert!(changes(&worktree).await.is_err());
        assert_eq!(repo.staged(), "");
    }

    #[tokio::test]
    async fn apply_lands_the_changes_as_unstaged_modifications() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        agent_edits(&worktree);
        let patch = repo.patch(&worktree).await;

        apply(&repo.root, &patch).await.unwrap();

        assert_eq!(repo.read("a.txt").as_deref(), Some(A_EDITED));
        assert_eq!(repo.read(NEW_FILE).as_deref(), Some(&b"hello\n"[..]));
        assert_eq!(repo.read("blob.bin").as_deref(), Some(BINARY));
        assert_eq!(repo.read("gone.txt"), None);
        assert_eq!(repo.staged(), "");
        assert_eq!(repo.git(&["diff", "--name-only"]), "a.txt\ngone.txt");
        let untracked = repo.git(&[
            "-c",
            "core.quotePath=false",
            "ls-files",
            "--others",
            "--exclude-standard",
        ]);
        for name in ["blob.bin", NEW_FILE] {
            assert!(
                untracked.lines().any(|line| line == name),
                "{name} missing from:\n{untracked}"
            );
        }
    }

    #[tokio::test]
    async fn apply_leaves_what_the_user_had_staged_staged() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        // The user stages an edit to the first line of b.txt and a new file.
        repo.write("b.txt", b"ONE\n2\n3\n4\n5\n6\n7\n8\n9\n");
        repo.write("user.txt", b"mine\n");
        repo.git(&["add", "b.txt", "user.txt"]);
        // The agent edits the last line of b.txt and another file.
        write(&worktree.path, "b.txt", B_LAST_LINE_EDITED);
        write(&worktree.path, "a.txt", A_EDITED);
        let patch = repo.patch(&worktree).await;

        apply(&repo.root, &patch).await.unwrap();

        assert_eq!(
            repo.read("b.txt").as_deref(),
            Some(&b"ONE\n2\n3\n4\n5\n6\n7\n8\nNINE\n"[..])
        );
        assert_eq!(repo.read("a.txt").as_deref(), Some(A_EDITED));
        assert_eq!(repo.staged(), "b.txt\nuser.txt");
        assert_eq!(repo.git(&["diff", "--name-only"]), "a.txt");
    }

    #[tokio::test]
    async fn apply_reports_conflicting_paths_and_leaves_markers_in_the_file() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        repo.write("a.txt", b"one\nmain\nthree\n");
        repo.commit_tracked("main edits the second line");
        write(&worktree.path, "a.txt", b"one\nagent\nthree\n");
        write(&worktree.path, "b.txt", B_LAST_LINE_EDITED);
        let patch = repo.patch(&worktree).await;

        let error = apply(&repo.root, &patch).await.unwrap_err();

        assert_eq!(error, ApplyError::Conflicts(vec!["a.txt".to_string()]));
        assert_eq!(error.to_string(), "conflicts in: a.txt");
        let merged = String::from_utf8(repo.read("a.txt").unwrap()).unwrap();
        for part in ["<<<<<<<", "main", "=======", "agent", ">>>>>>>"] {
            assert!(merged.contains(part), "{part} missing from:\n{merged}");
        }
        // The file without a conflict landed, and the index is as the user left it.
        assert_eq!(repo.read("b.txt").as_deref(), Some(B_LAST_LINE_EDITED));
        assert_eq!(repo.staged(), "");
        assert_eq!(repo.git(&["diff", "--name-only", "--diff-filter=U"]), "");
    }

    #[tokio::test]
    async fn apply_changes_nothing_when_a_patched_file_has_unstaged_edits() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        repo.write("a.txt", b"one\ntwo\nthree\nlocal\n");
        write(&worktree.path, "a.txt", A_EDITED);
        write(&worktree.path, "b.txt", B_LAST_LINE_EDITED);
        let patch = repo.patch(&worktree).await;

        match apply(&repo.root, &patch).await {
            Err(ApplyError::Failed(message)) => assert!(message.contains("a.txt"), "{message}"),
            other => panic!("expected the patch to be rejected, got {other:?}"),
        }
        assert_eq!(
            repo.read("a.txt").as_deref(),
            Some(&b"one\ntwo\nthree\nlocal\n"[..])
        );
        assert_eq!(repo.read("b.txt").as_deref(), Some(B_COMMITTED));
        assert_eq!(repo.staged(), "");
    }

    #[tokio::test]
    async fn apply_accepts_a_file_whose_timestamp_changed_without_its_content() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        let touched = std::fs::File::options()
            .write(true)
            .open(repo.root.join("a.txt"))
            .unwrap();
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(120);
        touched.set_modified(later).unwrap();
        drop(touched);
        write(&worktree.path, "a.txt", A_EDITED);
        let patch = repo.patch(&worktree).await;

        apply(&repo.root, &patch).await.unwrap();

        assert_eq!(repo.read("a.txt").as_deref(), Some(A_EDITED));
        assert_eq!(repo.staged(), "");
    }

    #[tokio::test]
    async fn apply_from_a_project_below_the_top_of_the_repository() {
        let repo = committed_repo();
        let project = repo.root.join("sub");
        let worktrees = project.join(".azazel").join("worktrees");
        let worktree = create(&project, &worktrees, SESSION, "coder-1").await.unwrap();
        repo.write("a.txt", b"one\nmain\nthree\n");
        repo.commit_tracked("main edits the second line");
        write(&worktree.path, "a.txt", b"one\nagent\nthree\n");
        write(&worktree.path, "sub/s.txt", b"s2\n");
        let patch = repo.patch(&worktree).await;

        let error = apply(&project, &patch).await.unwrap_err();

        assert_eq!(error, ApplyError::Conflicts(vec!["../a.txt".to_string()]));
        assert_eq!(repo.read("sub/s.txt").as_deref(), Some(&b"s2\n"[..]));
        assert_eq!(repo.staged(), "");
    }

    #[tokio::test]
    async fn remove_deletes_directory_and_branch_and_may_be_repeated() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        write(&worktree.path, "scratch.txt", b"never committed\n");

        remove(&repo.root, &worktree).await.unwrap();

        assert!(!worktree.path.exists());
        assert_eq!(repo.branches(), "");
        assert_eq!(repo.registered_worktrees(), 1);
        remove(&repo.root, &worktree).await.unwrap();
        // Directory and branch name are free again.
        assert_eq!(repo.worktree("coder-1").await.path, worktree.path);
    }

    #[tokio::test]
    async fn remove_clears_a_leftover_directory_git_no_longer_tracks() {
        let repo = committed_repo();
        let worktree = repo.worktree("coder-1").await;
        // What an interrupted removal leaves: git has forgotten the worktree, files remain.
        repo.git(&["worktree", "remove", "--force", worktree.path.to_str().unwrap()]);
        write(&worktree.path, "left/over.txt", b"still here\n");

        remove(&repo.root, &worktree).await.unwrap();

        assert!(!worktree.path.exists());
        assert_eq!(repo.branches(), "");
    }

    #[tokio::test]
    async fn remove_never_deletes_a_directory_holding_its_own_repository() {
        let main = committed_repo();
        let other = committed_repo();
        let bogus = Worktree {
            path: other.root.clone(),
            branch: "azazel/s1/nobody".to_string(),
            base: "HEAD".to_string(),
            started_dirty: false,
        };

        assert!(remove(&main.root, &bogus).await.is_err());
        assert_eq!(other.read("a.txt").as_deref(), Some(A_COMMITTED));
    }

    #[test]
    fn conflict_paths_are_rewritten_relative_to_the_project_directory() {
        assert_eq!(relative_to_prefix("", "a/b.txt"), "a/b.txt");
        assert_eq!(relative_to_prefix("sub/", "sub/s.txt"), "s.txt");
        assert_eq!(relative_to_prefix("sub/", "a.txt"), "../a.txt");
        assert_eq!(relative_to_prefix("x/y/", "x/z/f.txt"), "../z/f.txt");
        assert_eq!(relative_to_prefix("x/y/", "f.txt"), "../../f.txt");
        assert_eq!(relative_to_prefix("sub/", "subway/f.txt"), "../subway/f.txt");
    }
}
