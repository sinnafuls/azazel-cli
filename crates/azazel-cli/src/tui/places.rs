//! Where a session works: `/open` (a session of any project), `/cd` and `/worktree`.
//!
//! The project a session works in is read once, at the start: its root, trust, configuration,
//! context files, rules and servers. Going to another directory therefore starts azazel again
//! there, in the same terminal, as `/restart` does (see [`crate::restart`]), on the session that
//! goes on. A session that changes project moves into the sessions of the project it goes to.

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use azazel_core::Paths;
use azazel_core::SessionStore;
use azazel_core::paths::canonical;
use azazel_core::session::SessionSummary;

use super::Tui;
use super::sessions::PickFor;
use crate::restart::Move;

/// Directories `/cd` completion offers at most.
const MAX_DIRECTORIES: usize = 200;

/// The sessions of every project, most recently used first.
pub(super) fn every_session(paths: &Paths) -> Vec<SessionSummary> {
    let mut sessions: Vec<SessionSummary> = crate::sessions::session_dirs(paths, true)
        .iter()
        .flat_map(|dir| SessionStore::list(dir))
        .collect();
    sessions.sort_by(|a, b| {
        b.updated
            .cmp(&a.updated)
            .then_with(|| b.timestamp.cmp(&a.timestamp))
    });
    sessions
}

/// A session's directory as pickers show it, `~` for the home directory.
pub(super) fn shown_dir(cwd: &str) -> String {
    if cwd.is_empty() {
        return "(no directory)".to_string();
    }
    super::statusline::display_path(Path::new(cwd), std::env::home_dir().as_deref())
}

/// A directory as typed after `/cd`: `~` is the home directory, a relative one is below `cwd`.
fn typed_dir(typed: &str, cwd: &Path) -> PathBuf {
    let typed = typed.trim().trim_matches('"');
    let home = std::env::home_dir();
    let below_home = typed
        .strip_prefix("~/")
        .or_else(|| typed.strip_prefix("~\\"))
        .or((typed == "~").then_some(""));
    match (below_home, home) {
        (Some(rest), Some(home)) => home.join(rest),
        _ => cwd.join(typed),
    }
}

/// The directories `/cd <typed>` completes to, as they would be typed: those in the directory
/// `typed` names up to its last separator whose names start with what follows it. Hidden ones
/// only when that starts with a dot.
pub(super) fn directories(cwd: &Path, typed: &str) -> Vec<String> {
    let cut = typed.rfind(['/', '\\']).map_or(0, |at| at + 1);
    let (head, tail) = typed.split_at(cut);
    let base = match head {
        "" => cwd.to_path_buf(),
        head => typed_dir(head, cwd),
    };
    let tail = tail.to_lowercase();
    let mut found: Vec<String> = std::fs::read_dir(base)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| entry.path().is_dir())
        .filter_map(|entry| entry.file_name().into_string().ok())
        .filter(|name| {
            name.to_lowercase().starts_with(&tail) && (!name.starts_with('.') || tail.starts_with('.'))
        })
        .map(|name| format!("{head}{name}/"))
        .collect();
    found.sort_by_key(|name| name.to_lowercase());
    found.truncate(MAX_DIRECTORIES);
    found
}

impl Tui {
    /// `/open`: the sessions of every project to pick from.
    pub(super) async fn open_picker(&mut self) {
        if self.here_only("/open") && self.idle_to("open another session") {
            self.open_sessions(PickFor::Open, false, None, None).await;
        }
    }

    /// Opens a session chosen in `/open`: in place when it is one of this project, else by
    /// starting again in its directory.
    pub(super) async fn open_anywhere(&mut self, row: SessionSummary) {
        let Some(app) = self.app("/open") else { return };
        if row.id == self.session.id() {
            return self.info("that session is the one that is open");
        }
        let here = row
            .path
            .parent()
            .is_some_and(|dir| canonical(dir) == canonical(&app.paths.sessions_dir()));
        if here {
            return self.resume_file(row.path).await;
        }
        let cwd = PathBuf::from(&row.cwd);
        if row.cwd.is_empty() || !cwd.is_dir() {
            return self.fail(&format!(
                "that session worked in {}, which is not there any more; azazel --cwd <dir> -r {} opens it \
                 in another directory",
                shown_dir(&row.cwd),
                row.path.display()
            ));
        }
        if let Some(host) = crate::running::host_of(&app.paths.home, &row.id) {
            return self.fail(&format!(
                "that session runs in the background; azazel attach {host} opens it"
            ));
        }
        if !self.idle_to("open another session") {
            return;
        }
        self.info(&format!("opening it in {}", shown_dir(&row.cwd)));
        self.start_again_in(&cwd, row.path, false, false, None);
    }

    /// `/cd <dir>`: the session goes on in another directory. In another project it moves into
    /// that project's sessions.
    pub(super) fn cd_command(&mut self, args: &str) {
        let Some(app) = self.app("/cd") else { return };
        let cwd = app.paths.cwd.clone();
        if args.trim().is_empty() {
            return self.info(&format!(
                "working in {}; /cd <dir> goes on in another directory",
                cwd.display()
            ));
        }
        let dir = typed_dir(args, &cwd);
        if !dir.is_dir() {
            return self.fail(&format!("{} is not a directory", dir.display()));
        }
        let dir = canonical(&dir);
        if dir == cwd {
            return self.info(&format!("already working in {}", dir.display()));
        }
        if !self.idle_to("change directory") {
            return;
        }
        let Some(moving) = self.moving_to(&app, &dir) else {
            return;
        };
        let other = Paths::discover(&dir, Some(&app.paths.home.to_string_lossy())).project_root
            != app.paths.project_root;
        if other {
            self.info(&format!(
                "{} is another project; the session moves into its sessions",
                dir.display()
            ));
        }
        self.start_again_in(&dir, moving.target(), true, !other, Some(moving));
    }

    /// The move of the open session into the sessions of the project of `cwd`. Says why there
    /// is none when the session keeps no file.
    fn moving_to(&mut self, app: &crate::app::App, cwd: &Path) -> Option<Move> {
        let file = self.session.path();
        if !app.keeps_sessions() || !file.is_file() {
            self.fail("this session keeps no file (--no-session), so it cannot go on in another directory");
            return None;
        }
        let target = Paths::discover(cwd, Some(&app.paths.home.to_string_lossy()));
        let dir = app
            .paths
            .sessions_override
            .clone()
            .unwrap_or_else(|| target.sessions_dir());
        Some(Move {
            file,
            dir,
            cwd: cwd.to_path_buf(),
        })
    }

    /// `/worktree [name]`, `/worktree list`, `/worktree remove <name> [--force]`.
    pub(super) async fn worktree_command(&mut self, args: &str) {
        let Some(app) = self.app("/worktree") else { return };
        let dir = crate::worktree::dir(&app.paths);
        let words: Vec<&str> = args.split_whitespace().collect();
        match words.as_slice() {
            ["list"] => match crate::worktree::list(&app.paths.cwd, &dir).await {
                Ok(found) if found.is_empty() => self.info(&format!("no worktree in {}", dir.display())),
                Ok(found) => {
                    let here = &app.paths.project_root;
                    let lines: Vec<String> = found
                        .iter()
                        .map(|worktree| {
                            format!(
                                "{}{}  {}  {}",
                                worktree.name,
                                if worktree.path == *here { " (here)" } else { "" },
                                worktree.branch.as_deref().unwrap_or("(detached)"),
                                worktree.path.display()
                            )
                        })
                        .collect();
                    self.info(&lines.join("\n"));
                }
                Err(why) => self.fail(&why),
            },
            ["remove", name, rest @ ..] if rest.iter().all(|word| *word == "--force") => {
                let path = canonical(&dir.join(name));
                if app.paths.cwd.starts_with(&path) {
                    return self.fail(&format!(
                        "the session works in {name}; /cd to another directory first"
                    ));
                }
                let force = !rest.is_empty();
                match crate::worktree::remove(&app.paths.cwd, &dir, name, force).await {
                    Ok(said) => self.info(&said),
                    Err(why) => self.fail(&why),
                }
            }
            ["list", ..] | ["remove", ..] => {
                self.fail("usage: /worktree [name], /worktree list, /worktree remove <name> [--force]")
            }
            [] | [_] => {
                if !self.idle_to("move to a worktree") {
                    return;
                }
                let name = match words.first() {
                    Some(name) => (*name).to_string(),
                    None => crate::worktree::new_name(&dir),
                };
                // Checked before anything is made: a session without a file cannot go on there.
                if self.moving_to(&app, &app.paths.cwd).is_none() {
                    return;
                }
                let created = match crate::worktree::create(&app.paths.cwd, &dir, &name, true).await {
                    Ok(created) => created,
                    Err(why) => return self.fail(&why),
                };
                crate::worktree::inherit_trust(&app.paths, &created.path);
                let Some(moving) = self.moving_to(&app, &created.cwd) else {
                    return;
                };
                self.info(&format!(
                    "made {} on branch {}{}; the session goes on there",
                    created.path.display(),
                    created.branch,
                    if created.carried {
                        ", with the uncommitted changes, which left this tree"
                    } else {
                        ""
                    }
                ));
                // A worktree holds the project's own configuration: a `--trust` given still holds.
                self.start_again_in(&created.cwd, moving.target(), true, true, Some(moving));
            }
            _ => self.fail("usage: /worktree [name], /worktree list, /worktree remove <name> [--force]"),
        }
    }

    /// Ends this azazel and starts it again in `cwd` on the session in `file` (its place once
    /// `moving` has moved it). With `live` the session goes on, on the model, thinking level
    /// and permission mode it is on now; otherwise the session opened brings its own. Without
    /// `keep_trust`, a `--trust` given on the command line is left behind: it was given for
    /// another project.
    fn start_again_in(
        &mut self,
        cwd: &Path,
        file: PathBuf,
        live: bool,
        keep_trust: bool,
        moving: Option<Move>,
    ) {
        let mut reopen: Vec<OsString> = Vec::new();
        if live {
            match self.backend.reopen() {
                Ok(mut args) => {
                    // What opens the session is said below, for where it will be.
                    if let Some(at) = args.iter().position(|arg| arg == "-r") {
                        args.truncate(at);
                    }
                    reopen.extend(args);
                }
                Err(why) => return self.fail(&why),
            }
        }
        reopen.extend([OsString::from("--cwd"), cwd.as_os_str().to_owned()]);
        reopen.extend([OsString::from("-r"), file.into_os_string()]);
        let original: Vec<OsString> = std::env::args_os().skip(1).collect();
        let without: &[&str] = if keep_trust { &["cwd"] } else { &["cwd", "trust"] };
        crate::restart::request(
            crate::restart::arguments(&original, &reopen, true, without),
            moving,
        );
        self.restarting = true;
        self.quit = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cd_completes_directories_as_typed() {
        let tmp = tempfile::tempdir().unwrap();
        let cwd = tmp.path().join("work");
        for dir in ["src", "Scripts", ".hidden", "docs/guide"] {
            std::fs::create_dir_all(cwd.join(dir)).unwrap();
        }
        std::fs::write(cwd.join("start.txt"), "not a directory").unwrap();
        std::fs::create_dir_all(tmp.path().join("sibling")).unwrap();

        assert_eq!(directories(&cwd, "s"), ["Scripts/", "src/"]);
        assert_eq!(directories(&cwd, ""), ["docs/", "Scripts/", "src/"]);
        assert_eq!(directories(&cwd, "."), [".hidden/"]);
        assert_eq!(directories(&cwd, "docs/"), ["docs/guide/"]);
        assert_eq!(directories(&cwd, "../si"), ["../sibling/"]);
        assert!(directories(&cwd, "missing/").is_empty());
    }

    #[test]
    fn typed_directories_are_below_the_working_one_or_home() {
        let cwd = Path::new("/work");
        assert_eq!(typed_dir("src", cwd), cwd.join("src"));
        assert_eq!(typed_dir("\"my dir\"", cwd), cwd.join("my dir"));
        if let Some(home) = std::env::home_dir() {
            assert_eq!(typed_dir("~", cwd), home.join(""));
            assert_eq!(typed_dir("~/code", cwd), home.join("code"));
        }
    }
}
