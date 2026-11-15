//! Worktrees a session works in: `/worktree` moves the open session into a new one, and
//! `azazel -p --worktree` runs in a new one. Each is `.azazel/worktrees/<name>` of the project,
//! on a new branch `azazel/<name>` from HEAD.
//!
//! A session that moves into a new worktree takes the uncommitted changes of the tree it
//! leaves with it, through `git stash`: they are stashed there, untracked files included and
//! `.azazel/` left alone, and the stash is popped in the new worktree, which starts at the same
//! commit, so it applies as it was. Ignored files stay where they are. A print run starts from
//! HEAD and leaves the tree it was started in as it is.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use azazel_core::paths::canonical;
use azazel_core::{Paths, TrustStore};
use tokio::process::Command;

/// The prefix of the branches of these worktrees.
const BRANCH_PREFIX: &str = "azazel/";

/// A worktree that was just made.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Created {
    pub path: PathBuf,
    pub branch: String,
    /// Where to work in it: the directory of the same place in the tree as the one it was
    /// made from, where that exists.
    pub cwd: PathBuf,
    /// Whether uncommitted changes were carried into it.
    pub carried: bool,
}

/// A worktree under the project's `.azazel/worktrees`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    pub path: PathBuf,
    /// Its branch; none when it is at a commit of its own (detached).
    pub branch: Option<String>,
}

/// Where the worktrees of the project of `paths` are, also when the session is in one.
pub fn dir(paths: &Paths) -> PathBuf {
    paths.main_project_root().join(".azazel").join("worktrees")
}

/// Runs git in `dir`; its output when it succeeds, what it said otherwise. Variables that
/// would point git at another repository are dropped, so `dir` alone decides.
pub(crate) async fn git<S: AsRef<OsStr>>(dir: &Path, args: &[S]) -> Result<String, String> {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["-c", "core.quotepath=false"])
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .output()
        .await
        .map_err(|e| format!("git could not be started: {e}"))?;
    if output.status.success() {
        return Ok(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    let said = String::from_utf8_lossy(&output.stderr).trim().to_string();
    Err(if said.is_empty() {
        format!(
            "git {} failed",
            args.first()
                .map(|a| a.as_ref().to_string_lossy())
                .unwrap_or_default()
        )
    } else {
        said
    })
}

/// A name that is safe as a directory name and as a part of a branch name.
pub fn checked_name(name: &str) -> Result<&str, String> {
    let safe = |c: char| c.is_ascii_alphanumeric() || c == '_' || c == '-';
    if !name.is_empty() && name.len() <= 64 && name.chars().all(safe) && !name.starts_with('-') {
        Ok(name)
    } else {
        Err(format!(
            "`{name}` cannot name a worktree: use letters, digits, `_` and `-`, at most 64"
        ))
    }
}

/// A name after the time, one no worktree in `dir` has yet.
pub fn new_name(dir: &Path) -> String {
    let stamp = jiff::Zoned::now().strftime("%Y%m%d-%H%M%S").to_string();
    let mut name = stamp.clone();
    let mut n = 2;
    while dir.join(&name).exists() {
        name = format!("{stamp}-{n}");
        n += 1;
    }
    name
}

/// The stash's newest entry, if there is one.
async fn newest_stash(dir: &Path) -> Option<String> {
    git(dir, &["rev-parse", "-q", "--verify", "refs/stash"])
        .await
        .ok()
        .map(|id| id.trim().to_string())
}

/// Makes `<dir>/<name>` on a new branch `azazel/<name>` from the HEAD of the tree `cwd` is
/// in. With `carry` the uncommitted changes of that tree move into it.
pub async fn create(cwd: &Path, dir: &Path, name: &str, carry: bool) -> Result<Created, String> {
    create_from(cwd, dir, name, carry, None).await
}

/// [`create`], starting from the commit `start` names instead of the HEAD of the tree `cwd` is in.
pub async fn create_from(
    cwd: &Path,
    dir: &Path,
    name: &str,
    carry: bool,
    start: Option<&str>,
) -> Result<Created, String> {
    let name = checked_name(name)?;
    let top = git(cwd, &["rev-parse", "--show-toplevel"])
        .await
        .map_err(|_| format!("{} is not in a git repository", cwd.display()))?;
    let top = canonical(Path::new(top.trim()));
    let prefix = git(cwd, &["rev-parse", "--show-prefix"])
        .await?
        .trim()
        .to_string();
    // One commit for the worktree and the stash alike, whatever happens in the tree meanwhile.
    let wanted = format!("{}^{{commit}}", start.unwrap_or("HEAD"));
    let Ok(head) = git(cwd, &["rev-parse", "--verify", "--quiet", wanted.as_str()]).await else {
        return Err(match start {
            Some(start) => format!("the commit {start} is not in the repository"),
            None => "the repository has no commit yet: a worktree starts from one".to_string(),
        });
    };
    let head = head.trim().to_string();
    let path = canonical(&dir.join(name));
    if path.exists() {
        return Err(format!("{} already exists", path.display()));
    }
    let branch = format!("{BRANCH_PREFIX}{name}");
    let reference = format!("refs/heads/{branch}");
    if git(cwd, &["show-ref", "--verify", "--quiet", reference.as_str()])
        .await
        .is_ok()
    {
        return Err(format!("the branch {branch} already exists"));
    }
    let add = [
        OsStr::new("worktree"),
        OsStr::new("add"),
        OsStr::new("-b"),
        OsStr::new(&branch),
        path.as_os_str(),
        OsStr::new(&head),
    ];
    git(cwd, &add)
        .await
        .map_err(|e| format!("could not make the worktree: {e}"))?;
    let carried = match carry {
        true => match carry_changes(&top, dir, &path).await {
            Ok(carried) => carried,
            Err(Carry::Kept(why)) => return Err(why),
            Err(Carry::Undone(why)) => {
                // Nothing is left of the attempt: the changes are back where they were.
                let _ = git(
                    cwd,
                    &[
                        OsStr::new("worktree"),
                        OsStr::new("remove"),
                        OsStr::new("--force"),
                        path.as_os_str(),
                    ],
                )
                .await;
                let _ = git(cwd, &["branch", "-D", branch.as_str()]).await;
                return Err(why);
            }
        },
        false => false,
    };
    let inside = path.join(&prefix);
    let cwd = if !prefix.is_empty() && inside.is_dir() {
        canonical(&inside)
    } else {
        path.clone()
    };
    Ok(Created {
        path,
        branch,
        cwd,
        carried,
    })
}

/// Why uncommitted changes could not be carried into a new worktree.
enum Carry {
    /// Nothing changed: the worktree can go.
    Undone(String),
    /// The worktree holds part of them and stays, for the user to finish by hand.
    Kept(String),
}

/// Moves the uncommitted changes of the tree at `top` into the worktree at `path` (see the
/// module's notes). False when there were none.
async fn carry_changes(top: &Path, dir: &Path, path: &Path) -> Result<bool, Carry> {
    // The `.azazel` that holds the worktrees is the project's own, never part of the changes:
    // stashing the other worktrees with them would take their work away. Nothing is carried
    // when it cannot be named inside the repository.
    let azazel = dir.parent().map(canonical);
    let inside = azazel
        .as_deref()
        .and_then(|azazel| azazel.strip_prefix(top).ok())
        .and_then(Path::to_str)
        .filter(|inside| !inside.is_empty())
        .map(|inside| inside.replace('\\', "/"))
        .ok_or_else(|| {
            Carry::Undone(format!(
                "the uncommitted changes cannot be carried: {} is not inside the repository at {}",
                dir.display(),
                top.display()
            ))
        })?;
    let outside = format!(":(top,exclude,literal){inside}");
    let status = [
        "status",
        "--porcelain",
        "--untracked-files=normal",
        "--",
        ":/",
        outside.as_str(),
    ];
    let changes = git(top, &status).await.map_err(Carry::Undone)?;
    if changes.trim().is_empty() {
        return Ok(false);
    }
    let before = newest_stash(top).await;
    let message = format!("azazel: carried into {}", path.display());
    let push = [
        "stash",
        "push",
        "--include-untracked",
        "-m",
        message.as_str(),
        "--",
        ":/",
        outside.as_str(),
    ];
    git(top, &push)
        .await
        .map_err(|e| Carry::Undone(format!("could not stash the uncommitted changes: {e}")))?;
    // Nothing was stashed after all: the newest entry is someone else's.
    if newest_stash(top).await == before {
        return Ok(false);
    }
    let Err(why) = git(path, &["stash", "pop", "--index"]).await else {
        return Ok(true);
    };
    // A pop that failed half way leaves the stash, and the worktree with what it applied.
    let clean = git(path, &["status", "--porcelain"])
        .await
        .is_ok_and(|status| status.trim().is_empty());
    if !clean {
        return Err(Carry::Kept(format!(
            "the uncommitted changes went into {} only in part ({why}); they are all in the newest entry of \
             `git stash list`, and the tree they came from no longer has them",
            path.display()
        )));
    }
    Err(Carry::Undone(
        match git(top, &["stash", "pop", "--index"]).await {
            Ok(_) => format!(
                "could not carry the uncommitted changes into the worktree ({why}); they are back where they were"
            ),
            Err(again) => format!(
                "could not carry the uncommitted changes into the worktree ({why}), nor put them back ({again}); \
             they are the newest entry of `git stash list`"
            ),
        },
    ))
}

/// The worktrees in `dir`, as git knows them.
pub async fn list(cwd: &Path, dir: &Path) -> Result<Vec<Listed>, String> {
    let porcelain = git(cwd, &["worktree", "list", "--porcelain"]).await?;
    let dir = canonical(dir);
    let mut found = Vec::new();
    for block in porcelain.split("\n\n") {
        let mut path = None;
        let mut branch = None;
        for line in block.lines() {
            if let Some(at) = line.strip_prefix("worktree ") {
                path = Some(canonical(Path::new(at)));
            } else if let Some(name) = line.strip_prefix("branch ") {
                branch = Some(name.trim_start_matches("refs/heads/").to_string());
            }
        }
        let Some(path) = path else { continue };
        if path.parent() != Some(dir.as_path()) {
            continue;
        }
        let name = path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_default();
        found.push(Listed { name, path, branch });
    }
    found.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(found)
}

/// Removes the worktree `name` of `dir`. One with uncommitted changes is kept unless `force`;
/// its branch is deleted only when another branch has every commit of it.
pub async fn remove(cwd: &Path, dir: &Path, name: &str, force: bool) -> Result<String, String> {
    let name = checked_name(name)?;
    let Some(found) = list(cwd, dir).await?.into_iter().find(|w| w.name == name) else {
        return Err(format!("there is no worktree `{name}` in {}", dir.display()));
    };
    if !force {
        let status = git(&found.path, &["status", "--porcelain"]).await?;
        if !status.trim().is_empty() {
            return Err(format!(
                "{name} has uncommitted changes: commit them, or /worktree remove {name} --force drops them"
            ));
        }
    }
    let mut command = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if force {
        command.push(OsStr::new("--force"));
    }
    command.push(found.path.as_os_str());
    git(cwd, &command).await?;
    Ok(match found.branch {
        Some(branch) => match git(cwd, &["branch", "-d", branch.as_str()]).await {
            Ok(_) => format!("removed {} and its branch {branch}", found.path.display()),
            Err(_) => format!(
                "removed {}; its branch {branch} is kept, it has commits no other branch has (git branch -D {branch} \
                 deletes it)",
                found.path.display()
            ),
        },
        None => format!("removed {}", found.path.display()),
    })
}

/// A worktree of a project the user trusts is trusted too: it holds the same configuration.
pub fn inherit_trust(paths: &Paths, worktree: &Path) {
    let store = TrustStore::new(paths.trust_file());
    if store.decision(&paths.project_root) == Some(true)
        && let Err(e) = store.set(&canonical(worktree), true)
    {
        tracing::warn!("could not trust {}: {e}", worktree.display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git_ok(dir: &Path, args: &[&str]) -> String {
        let output = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    fn repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = canonical(&tmp.path().join("repo"));
        std::fs::create_dir_all(root.join("src")).unwrap();
        git_ok(&root, &["init", "--quiet", "-b", "main"]);
        git_ok(&root, &["config", "user.email", "test@example.com"]);
        git_ok(&root, &["config", "user.name", "Test"]);
        git_ok(&root, &["config", "core.autocrlf", "false"]);
        std::fs::write(root.join("src/a.txt"), "one\n").unwrap();
        std::fs::write(root.join("b.txt"), "bee\n").unwrap();
        git_ok(&root, &["add", "-A"]);
        git_ok(&root, &["commit", "--quiet", "-m", "first"]);
        (tmp, root)
    }

    #[tokio::test]
    async fn uncommitted_changes_move_into_the_new_worktree_and_azazel_stays() {
        let (_tmp, root) = repo();
        let dir = root.join(".azazel").join("worktrees");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(root.join(".azazel/config.toml"), "# mine\n").unwrap();
        std::fs::write(root.join("src/a.txt"), "one\ntwo\n").unwrap();
        std::fs::write(root.join("new.txt"), "fresh\n").unwrap();
        git_ok(&root, &["add", "b.txt"]);
        std::fs::write(root.join("b.txt"), "bee\nstaged? no\n").unwrap();

        let created = create(&root.join("src"), &dir, "feature", true).await.unwrap();
        assert!(created.carried);
        assert_eq!(created.branch, "azazel/feature");
        assert_eq!(created.path, canonical(&dir.join("feature")));
        // Working in `src` goes on in `src` of the worktree.
        assert_eq!(created.cwd, created.path.join("src"));
        assert_eq!(
            std::fs::read_to_string(created.path.join("src/a.txt")).unwrap(),
            "one\ntwo\n"
        );
        assert_eq!(
            std::fs::read_to_string(created.path.join("new.txt")).unwrap(),
            "fresh\n"
        );
        // The tree it left is clean, and keeps its own `.azazel`.
        assert_eq!(std::fs::read_to_string(root.join("src/a.txt")).unwrap(), "one\n");
        assert!(!root.join("new.txt").exists());
        assert_eq!(
            std::fs::read_to_string(root.join(".azazel/config.toml")).unwrap(),
            "# mine\n"
        );
        assert_eq!(git_ok(&root, &["stash", "list"]), "");

        let listed = list(&root, &dir).await.unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].name, "feature");
        assert_eq!(listed[0].branch.as_deref(), Some("azazel/feature"));

        // A second worktree of the same name is refused; a clean tree carries nothing.
        assert!(create(&root, &dir, "feature", true).await.is_err());
        let second = create(&root, &dir, "other", true).await.unwrap();
        assert!(!second.carried);
        assert_eq!(second.cwd, second.path);

        // What another worktree holds is never part of the changes carried.
        std::fs::write(created.path.join("notes.txt"), "the other worktree's\n").unwrap();
        std::fs::write(root.join("b.txt"), "bee\nagain\n").unwrap();
        let third = create(&root, &dir, "third", true).await.unwrap();
        assert!(third.carried);
        assert_eq!(
            std::fs::read_to_string(third.path.join("b.txt")).unwrap(),
            "bee\nagain\n"
        );
        assert!(!third.path.join("notes.txt").exists());
        assert_eq!(
            std::fs::read_to_string(created.path.join("notes.txt")).unwrap(),
            "the other worktree's\n"
        );

        // One with changes is kept unless forced; a branch without commits of its own goes.
        let kept = remove(&root, &dir, "feature", false).await.unwrap_err();
        assert!(kept.contains("uncommitted"), "{kept}");
        let said = remove(&root, &dir, "feature", true).await.unwrap();
        assert!(said.contains("its branch azazel/feature"), "{said}");
        assert!(!created.path.exists());
        assert!(git_ok(&root, &["branch", "--list", "azazel/feature"]).is_empty());
        assert!(remove(&root, &dir, "feature", true).await.is_err());
    }

    #[tokio::test]
    async fn a_branch_with_work_of_its_own_is_kept_when_its_worktree_goes() {
        let (_tmp, root) = repo();
        let dir = root.join(".azazel").join("worktrees");
        let created = create(&root, &dir, "work", false).await.unwrap();
        std::fs::write(created.path.join("b.txt"), "changed\n").unwrap();
        git_ok(&created.path, &["commit", "--quiet", "-am", "work"]);
        let said = remove(&root, &dir, "work", false).await.unwrap();
        assert!(said.contains("is kept"), "{said}");
        assert_eq!(git_ok(&root, &["branch", "--list", "azazel/work"]), "azazel/work");
    }

    #[tokio::test]
    async fn outside_a_repository_or_before_the_first_commit_nothing_is_made() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("wt");
        assert!(
            create(tmp.path(), &dir, "x", true)
                .await
                .unwrap_err()
                .contains("not in a git repository")
        );
        git_ok(tmp.path(), &["init", "--quiet"]);
        assert!(
            create(tmp.path(), &dir, "x", true)
                .await
                .unwrap_err()
                .contains("no commit")
        );
    }
}
