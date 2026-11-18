//! Starting a background session from a browser: which folders it may name, and the question
//! the terminal of `azazel remote` is asked before a session works in a folder nobody trusted.
//!
//! A browser may start sessions in the projects sessions have worked in and in the folders of
//! `[remote] folders`, or anywhere once `[remote] allow_bypass` or `[remote] full_access` is
//! set. A folder whose project is not trusted is started trusted for that session only, and only
//! when one of those is set or the person at the terminal says yes.

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::Duration;

use azazel_core::session::SessionStore;
use azazel_core::{Paths, TrustStore};
use azazel_remote::{Folder, FolderKind, Folders, NewSession, StartError};
use tokio::sync::{mpsc, oneshot};

use crate::running;

/// Folders a look into one lists at most.
const MAX_FOLDERS: usize = 200;
/// Projects the first list offers at most.
const MAX_PROJECTS: usize = 50;
/// How long the terminal has to answer before the browser is told no.
const ANSWER_WITHIN: Duration = Duration::from_secs(120);

/// A question for the person at the terminal of `azazel remote`.
pub(crate) struct Ask {
    pub question: String,
    pub answer: oneshot::Sender<bool>,
}

/// What decides where a browser may start a session.
#[derive(Clone)]
pub(crate) struct Rules {
    /// azazel's home (`~/.azazel`), where sessions and trust are kept.
    pub home: PathBuf,
    /// The user's home directory, for `~`.
    pub user_home: Option<PathBuf>,
    /// `[remote] folders`, as written.
    pub roots: Vec<String>,
    /// `[remote] allow_bypass` or `[remote] full_access`: any folder, and no question.
    pub anywhere: bool,
}

impl Rules {
    /// The rules `[remote]` sets: full access opens every folder as bypass does.
    pub fn of(
        home: PathBuf,
        user_home: Option<PathBuf>,
        remote: &azazel_core::config::RemoteConfig,
    ) -> Rules {
        Rules {
            home,
            user_home,
            roots: remote.folders.clone(),
            anywhere: remote.allow_bypass || remote.full_access,
        }
    }

    /// `typed` as a folder: `~` is the home directory, and the path must be whole.
    fn path(&self, typed: &str) -> Option<PathBuf> {
        let typed = typed.trim();
        let path = match typed.strip_prefix('~') {
            Some(rest) if rest.is_empty() || rest.starts_with(['/', '\\']) => self
                .user_home
                .as_ref()?
                .join(rest.trim_start_matches(['/', '\\'])),
            _ => PathBuf::from(typed),
        };
        path.is_absolute().then_some(path)
    }

    /// The folder `typed` names, as it is on disk.
    fn folder(&self, typed: &str) -> Result<PathBuf, StartError> {
        let path = self.path(typed).ok_or_else(|| {
            StartError::Invalid(format!("`{typed}` is not a whole path: start it with / or ~"))
        })?;
        if !path.is_dir() {
            return Err(StartError::Invalid(format!(
                "there is no folder {}",
                path.display()
            )));
        }
        Ok(azazel_core::paths::canonical(&path))
    }

    fn name(&self, path: &Path) -> String {
        crate::tui::display_path(path, self.user_home.as_deref())
    }

    /// The folders of `[remote] folders` that exist.
    fn roots(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .filter_map(|root| self.path(root))
            .filter(|root| root.is_dir())
            .map(|root| azazel_core::paths::canonical(&root))
            .collect()
    }

    /// The folders sessions have worked in, most recently used first: the background sessions
    /// running now, then the saved sessions of every project.
    fn projects(&self) -> Vec<PathBuf> {
        let mut found: Vec<PathBuf> = running::list(&self.home)
            .into_iter()
            .map(|entry| entry.cwd)
            .collect();
        let paths = Paths::discover(&self.home, Some(&self.home.to_string_lossy()));
        let mut saved: Vec<_> = crate::sessions::session_dirs(&paths, true)
            .iter()
            .flat_map(|dir| SessionStore::list(dir))
            .filter(|session| !session.cwd.is_empty())
            .collect();
        saved.sort_by(|a, b| b.updated.cmp(&a.updated));
        found.extend(saved.into_iter().map(|session| PathBuf::from(session.cwd)));
        let mut projects: Vec<PathBuf> = Vec::new();
        for path in found {
            if projects.len() >= MAX_PROJECTS {
                break;
            }
            if !path.is_dir() {
                continue;
            }
            let path = azazel_core::paths::canonical(&path);
            if !projects.contains(&path) {
                projects.push(path);
            }
        }
        projects
    }

    /// Whether the folders inside `dir` may be looked into.
    fn browsable(&self, dir: &Path, roots: &[PathBuf]) -> bool {
        self.anywhere || roots.iter().any(|root| dir.starts_with(root))
    }

    /// Whether a session may be started in `dir`.
    fn allowed(&self, dir: &Path, roots: &[PathBuf]) -> bool {
        self.browsable(dir, roots) || self.projects().iter().any(|project| project == dir)
    }

    /// Whether the project `dir` belongs to is trusted.
    fn trusted(&self, dir: &Path) -> bool {
        let paths = Paths::discover(dir, Some(&self.home.to_string_lossy()));
        TrustStore::new(paths.trust_file()).decision(&paths.project_root) == Some(true)
    }

    fn entry(&self, path: PathBuf, kind: FolderKind, startable: bool) -> Folder {
        Folder {
            name: self.name(&path),
            trusted: self.trusted(&path),
            path: path.display().to_string(),
            kind,
            startable,
        }
    }

    /// The folders to start in (`under` is `None`), or the folders inside `under`.
    pub fn folders(&self, under: Option<&str>) -> Result<Folders, StartError> {
        let roots = self.roots();
        let Some(under) = under else {
            let mut folders: Vec<Folder> = Vec::new();
            let mut seen: Vec<PathBuf> = Vec::new();
            let mut add = |path: PathBuf, kind: FolderKind| {
                if !seen.contains(&path) {
                    seen.push(path.clone());
                    folders.push(self.entry(path, kind, true));
                }
            };
            for root in roots.iter().cloned() {
                add(root, FolderKind::Root);
            }
            for project in self.projects() {
                add(project, FolderKind::Project);
            }
            if self.anywhere
                && let Some(home) = self.user_home.clone().filter(|home| home.is_dir())
            {
                add(azazel_core::paths::canonical(&home), FolderKind::Dir);
            }
            return Ok(Folders {
                at: None,
                parent: None,
                folders,
                anywhere: self.anywhere,
            });
        };
        let dir = self.folder(under)?;
        if !self.browsable(&dir, &roots) {
            return Err(StartError::Refused(format!(
                "{} is not open to browsers; add it, or a folder above it, to `[remote] folders`",
                self.name(&dir)
            )));
        }
        let mut inside: Vec<PathBuf> = std::fs::read_dir(&dir)
            .map_err(|e| StartError::Failed(format!("cannot read {}: {e}", self.name(&dir))))?
            .flatten()
            .filter(|entry| !entry.file_name().to_string_lossy().starts_with('.'))
            .map(|entry| entry.path())
            .filter(|path| path.is_dir())
            .collect();
        inside.sort_by_key(|path| path.file_name().map(|name| name.to_string_lossy().to_lowercase()));
        inside.truncate(MAX_FOLDERS);
        let parent = dir
            .parent()
            .filter(|parent| self.browsable(parent, &roots))
            .map(|parent| parent.display().to_string());
        Ok(Folders {
            at: Some(dir.display().to_string()),
            parent,
            folders: inside
                .into_iter()
                .map(|path| self.entry(azazel_core::paths::canonical(&path), FolderKind::Dir, true))
                .collect(),
            anywhere: self.anywhere,
        })
    }

    /// Where a new session works and whether it is to be trusted for its run, once the folder
    /// is allowed and, when it is not trusted, the terminal said yes.
    pub async fn admit(
        &self,
        new: &NewSession,
        asks: Option<&mpsc::Sender<Ask>>,
    ) -> Result<(PathBuf, bool), StartError> {
        let rules = self.clone();
        let typed = new.cwd.clone();
        let (dir, allowed, trusted) = tokio::task::spawn_blocking(move || {
            let dir = rules.folder(&typed)?;
            let allowed = rules.allowed(&dir, &rules.roots());
            let trusted = rules.trusted(&dir);
            Ok::<_, StartError>((dir, allowed, trusted))
        })
        .await
        .map_err(|e| StartError::Failed(e.to_string()))??;
        let name = self.name(&dir);
        if !allowed {
            return Err(StartError::Refused(format!(
                "{name} is not open to browsers: add it, or a folder above it, to `[remote] folders` on \
                 the computer that runs azazel remote"
            )));
        }
        if trusted {
            return Ok((dir, false));
        }
        if self.anywhere {
            return Ok((dir, true));
        }
        let Some(asks) = asks else {
            return Err(StartError::Refused(format!(
                "{name} is not trusted, and nobody is at azazel remote to allow it: run azazel there once \
                 and trust it"
            )));
        };
        let (answer, answered) = oneshot::channel();
        let question = format!(
            "\nA browser asks to start a session in {name}, which is not trusted: its configuration can \
             run commands.\nAllow it, trusting the folder for that session? [y/N] "
        );
        if asks.send(Ask { question, answer }).await.is_err() {
            return Err(StartError::Refused(format!(
                "{name} is not trusted, and nobody is at azazel remote to allow it"
            )));
        }
        match tokio::time::timeout(ANSWER_WITHIN, answered).await {
            Ok(Ok(true)) => Ok((dir, true)),
            Ok(Ok(false)) => Err(StartError::Refused(format!(
                "the person at the computer did not allow a session in {name}"
            ))),
            Ok(Err(_)) => Err(StartError::Refused(format!(
                "nobody is at azazel remote to allow a session in {name}"
            ))),
            Err(_) => Err(StartError::Refused(format!(
                "nobody allowed a session in {name} at the computer within {} minutes",
                ANSWER_WITHIN.as_secs() / 60
            ))),
        }
    }
}

/// The arguments of the background process of a new session: `carried` (what `azazel remote`
/// was started with), the folder, the mode, the trust, then `host` with the first message.
pub(crate) fn host_args(
    carried: &[OsString],
    dir: &Path,
    new: &NewSession,
    trust: bool,
    id: &str,
) -> Vec<OsString> {
    let mut args: Vec<OsString> = carried.to_vec();
    args.extend(["--cwd".into(), dir.as_os_str().to_os_string()]);
    if let Some(mode) = new.mode.as_deref().filter(|mode| !mode.trim().is_empty()) {
        args.extend(["--mode".into(), mode.trim().into()]);
    }
    if trust {
        args.push("--trust".into());
    }
    args.extend(["host", "--id", id].map(OsString::from));
    if let Some(request) = &new.request_id {
        args.extend(["--request-id".into(), request.into()]);
    }
    if let Some(prompt) = new.prompt.as_deref().filter(|prompt| !prompt.trim().is_empty()) {
        // A message that starts with `-` is a message, not an option.
        args.extend(["--".into(), prompt.into()]);
    }
    args
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(home: &Path, user_home: &Path, roots: &[&Path], anywhere: bool) -> Rules {
        Rules {
            home: home.to_path_buf(),
            user_home: Some(user_home.to_path_buf()),
            roots: roots.iter().map(|root| root.display().to_string()).collect(),
            anywhere,
        }
    }

    fn new(cwd: &Path) -> NewSession {
        NewSession {
            cwd: cwd.display().to_string(),
            ..NewSession::default()
        }
    }

    #[tokio::test]
    async fn a_browser_starts_only_in_the_folders_it_is_given_unless_bypass_is_allowed() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let user = scratch.path().join("user");
        let work = user.join("work");
        let app = work.join("app");
        let elsewhere = scratch.path().join("elsewhere");
        for dir in [&home, &app, &elsewhere] {
            std::fs::create_dir_all(dir).unwrap();
        }
        let canonical = |path: &Path| azazel_core::paths::canonical(path);
        TrustStore::new(home.join("trust.json"))
            .set(&canonical(&app), true)
            .unwrap();
        let given = rules(&home, &user, &[Path::new("~/work")], false);

        // Inside a folder of `[remote] folders`, by `~` or in full, a trusted project starts as it is.
        let (dir, trust) = given.admit(&new(Path::new("~/work/app")), None).await.unwrap();
        assert_eq!((dir, trust), (canonical(&app), false));
        // Outside them it is refused, and a folder that is not there is a mistake.
        let refused = given.admit(&new(&elsewhere), None).await.unwrap_err();
        assert!(matches!(refused, StartError::Refused(_)), "{refused:?}");
        let missing = given.admit(&new(&work.join("gone")), None).await.unwrap_err();
        assert!(matches!(missing, StartError::Invalid(_)), "{missing:?}");
        assert!(matches!(
            given.admit(&new(Path::new("work")), None).await,
            Err(StartError::Invalid(_))
        ));

        // With bypass allowed any folder starts, trusted for its run, without a question.
        let anywhere = rules(&home, &user, &[], true);
        let (dir, trust) = anywhere.admit(&new(&elsewhere), None).await.unwrap();
        assert_eq!((dir, trust), (canonical(&elsewhere), true));
    }

    #[tokio::test]
    async fn an_untrusted_folder_waits_for_the_terminal() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let work = scratch.path().join("work");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&work).unwrap();
        let given = rules(&home, scratch.path(), &[&work], false);
        // Nobody at the terminal: refused.
        assert!(matches!(
            given.admit(&new(&work), None).await,
            Err(StartError::Refused(_))
        ));
        let (asks, mut asked) = mpsc::channel::<Ask>(1);
        let request = new(&work);
        for yes in [true, false] {
            let answer = async {
                let ask = asked.recv().await.unwrap();
                assert!(ask.question.contains("not trusted"), "{}", ask.question);
                ask.answer.send(yes).unwrap();
            };
            let (admitted, ()) = tokio::join!(given.admit(&request, Some(&asks)), answer);
            match yes {
                true => assert_eq!(admitted.unwrap(), (azazel_core::paths::canonical(&work), true)),
                false => assert!(matches!(admitted, Err(StartError::Refused(_))), "{admitted:?}"),
            }
        }
        // The terminal went away while the browser waited.
        drop(asked);
        assert!(matches!(
            given.admit(&request, Some(&asks)).await,
            Err(StartError::Refused(_))
        ));
    }

    #[tokio::test]
    async fn full_access_starts_an_untrusted_folder_anywhere_without_asking_the_terminal() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let elsewhere = scratch.path().join("elsewhere");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let remote = |allow_bypass: bool, full_access: bool| azazel_core::config::RemoteConfig {
            allow_bypass,
            full_access,
            ..Default::default()
        };
        let (asks, mut asked) = mpsc::channel::<Ask>(1);
        for (bypass, full) in [(false, true), (true, false)] {
            let rules = Rules::of(
                home.clone(),
                Some(scratch.path().to_path_buf()),
                &remote(bypass, full),
            );
            let (dir, trust) = rules.admit(&new(&elsewhere), Some(&asks)).await.unwrap();
            assert_eq!((dir, trust), (azazel_core::paths::canonical(&elsewhere), true));
            assert!(asked.try_recv().is_err(), "the terminal was asked");
        }
        // Neither: the folder is not open to browsers.
        let closed = Rules::of(
            home.clone(),
            Some(scratch.path().to_path_buf()),
            &remote(false, false),
        );
        assert!(matches!(
            closed.admit(&new(&elsewhere), Some(&asks)).await,
            Err(StartError::Refused(_))
        ));
    }

    #[test]
    fn the_folders_inside_a_given_one_are_listed_and_hidden_ones_left_out() {
        let scratch = tempfile::tempdir().unwrap();
        let home = scratch.path().join("azazel");
        let work = scratch.path().join("work");
        for dir in ["b", "A", ".git", "c"] {
            std::fs::create_dir_all(work.join(dir)).unwrap();
        }
        std::fs::write(work.join("file.txt"), "").unwrap();
        std::fs::create_dir_all(&home).unwrap();
        let given = rules(&home, scratch.path(), &[&work], false);
        let first = given.folders(None).unwrap();
        assert_eq!(first.folders.len(), 1);
        assert_eq!(first.folders[0].kind, FolderKind::Root);
        assert!(!first.anywhere);
        let inside = given.folders(Some(&work.display().to_string())).unwrap();
        let names: Vec<String> = inside
            .folders
            .iter()
            .map(|folder| {
                Path::new(&folder.path)
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect();
        assert_eq!(names, ["A", "b", "c"]);
        // The folder given is as far up as one may look.
        assert_eq!(inside.parent, None);
        let below = given
            .folders(Some(&work.join("b").display().to_string()))
            .unwrap();
        assert!(below.parent.is_some());
        assert!(matches!(
            given.folders(Some(&scratch.path().display().to_string())),
            Err(StartError::Refused(_))
        ));
    }

    #[test]
    fn the_host_is_started_in_the_folder_with_the_first_message_last() {
        let new = NewSession {
            cwd: "/w".into(),
            prompt: Some("-v is a flag?".into()),
            mode: Some("auto".into()),
            request_id: Some("r1".into()),
        };
        let args = host_args(
            &["--set".into(), "a=1".into()],
            Path::new("/w"),
            &new,
            true,
            "ab12",
        );
        let args: Vec<String> = args
            .iter()
            .map(|arg| arg.to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            args,
            [
                "--set",
                "a=1",
                "--cwd",
                "/w",
                "--mode",
                "auto",
                "--trust",
                "host",
                "--id",
                "ab12",
                "--request-id",
                "r1",
                "--",
                "-v is a flag?"
            ]
        );
    }
}
