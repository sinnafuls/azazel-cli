//! Background sessions on this machine, as the files under `<home>/run/` describe them.
//!
//! Each background session (`azazel host`) keeps there, under its id:
//!
//! - `<id>.json`: what it is ([`Entry`]), rewritten whenever its state changes;
//! - `<id>.log`: every event frame it has written, one JSON object per line;
//! - `<id>.err`: why it could not start, when it could not;
//! - `<id>.lock`: a file it holds locked for as long as it lives;
//! - `req-<hash>.req`: the id of the session a named `azazel bg` started, so that the same
//!   command sent again starts nothing;
//! - `session-<session id>.lock`: the same for the session it has open, so that no other
//!   process writes to that session's file.
//!
//! The locks are on files of their own rather than on the session file: on Windows a locked
//! range cannot be read through another handle, and session files are read by every listing.

use std::fs::{File, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use azazel_core::SessionStore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// How long the log of a session that has ended is kept, and the entry of one whose process
/// died in the middle of a run.
const KEEP_LOGS: Duration = Duration::from_secs(7 * 24 * 60 * 60);

/// The state of a session whose process is gone while its last run was cut short.
pub const INTERRUPTED: &str = "interrupted";

/// What a background session says about itself.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Entry {
    pub id: String,
    pub pid: u32,
    pub session_id: String,
    pub session_file: PathBuf,
    pub cwd: PathBuf,
    pub name: Option<String>,
    /// When it started, RFC 3339.
    pub started: String,
    /// What a client connects to: a socket file on Unix, a named pipe on Windows.
    pub socket: String,
    pub version: String,
    /// `working`, `waiting for you` or `idle`; `interrupted` once the process is gone and the
    /// session's last run was cut short.
    pub state: String,
    pub cost_usd: Option<f64>,
    /// The permission mode it works in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// The options it was started with that a process started in its place is to have.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub args: Vec<String>,
    /// The size of the session file when its last run was found cut short: while the file
    /// keeps that size, it need not be read again.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted_at: Option<u64>,
}

pub fn run_dir(home: &Path) -> PathBuf {
    home.join("run")
}

pub fn entry_file(home: &Path, id: &str) -> PathBuf {
    run_dir(home).join(format!("{id}.json"))
}

pub fn log_file(home: &Path, id: &str) -> PathBuf {
    run_dir(home).join(format!("{id}.log"))
}

pub fn err_file(home: &Path, id: &str) -> PathBuf {
    run_dir(home).join(format!("{id}.err"))
}

pub fn host_lock(home: &Path, id: &str) -> PathBuf {
    run_dir(home).join(format!("{id}.lock"))
}

fn session_lock(home: &Path, session_id: &str) -> PathBuf {
    run_dir(home).join(format!("session-{session_id}.lock"))
}

/// Creates `<home>/run`, readable by its owner only: on Unix whoever can open a session's
/// socket can drive the session.
pub fn create_run_dir(home: &Path) -> io::Result<PathBuf> {
    let dir = run_dir(home);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder.create(&dir)?;
    Ok(dir)
}

impl Entry {
    /// Writes the entry so that a reader sees the old one or the new one, never half of one.
    pub fn write(&self, home: &Path) -> io::Result<()> {
        let file = entry_file(home, &self.id);
        let staged = file.with_extension("json.tmp");
        std::fs::write(&staged, serde_json::to_vec_pretty(self)?)?;
        std::fs::rename(&staged, &file)
    }

    /// Whether the process that wrote the entry is still there.
    pub fn alive(&self, home: &Path) -> bool {
        Lock::held(&host_lock(home, &self.id))
    }

    /// Whether the process is gone and left a run unfinished: `azazel attach` starts it again.
    pub fn interrupted(&self) -> bool {
        self.state == INTERRUPTED
    }

    /// Looks at the session of an entry whose process is gone. `Some` with the entry as it is
    /// listed when the session's last run was cut short, nobody else has the session open, and
    /// the process has not been gone for too long.
    fn left_unfinished(mut self, home: &Path) -> Option<Entry> {
        let recent = std::fs::metadata(entry_file(home, &self.id))
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| SystemTime::now().duration_since(modified).ok())
            .is_some_and(|age| age <= KEEP_LOGS);
        if !recent || Lock::held(&session_lock(home, &self.session_id)) {
            return None;
        }
        let size = std::fs::metadata(&self.session_file).ok()?.len();
        if self.interrupted_at != Some(size) {
            SessionStore::interrupted_in(&self.session_file)?;
            self.state = INTERRUPTED.to_string();
            self.interrupted_at = Some(size);
            // Kept, so that the next listing need not read the session again.
            let _ = self.write(home);
        }
        Some(self)
    }
}

/// Leaves the reason a background session could not start where `azazel bg` looks for it:
/// nobody reads what such a process prints.
pub fn note_failure(home: &Path, id: &str, why: &str) {
    if create_run_dir(home).is_ok() {
        let _ = std::fs::write(err_file(home, id), why);
    }
}

/// A file held locked until this is dropped, which also deletes it.
#[derive(Debug)]
pub struct Lock {
    file: Option<File>,
    path: PathBuf,
}

impl Lock {
    /// Takes the lock; `None` when another process holds it.
    pub fn acquire(path: &Path) -> io::Result<Option<Lock>> {
        let file = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Lock {
                file: Some(file),
                path: path.to_path_buf(),
            })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }

    /// Whether some process holds the lock at `path`.
    pub fn held(path: &Path) -> bool {
        let Ok(file) = File::options().write(true).open(path) else {
            return false;
        };
        match file.try_lock() {
            Ok(()) => {
                let _ = file.unlock();
                false
            }
            Err(TryLockError::WouldBlock) => true,
            Err(TryLockError::Error(_)) => false,
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        // Closed before it is deleted: Windows does not delete a file that is open.
        drop(self.file.take());
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Locks the session for this process. `Err` names the background session that has it.
pub fn lock_session(home: &Path, session_id: &str) -> Result<Lock, String> {
    create_run_dir(home).map_err(|e| format!("cannot create {}: {e}", run_dir(home).display()))?;
    let path = session_lock(home, session_id);
    match Lock::acquire(&path) {
        Ok(Some(lock)) => Ok(lock),
        Ok(None) => Err(in_background(home, session_id)),
        Err(e) => Err(format!("cannot lock {}: {e}", path.display())),
    }
}

/// The id of the background session that has `session_id` open, when one does.
pub fn host_of(home: &Path, session_id: &str) -> Option<String> {
    if !Lock::held(&session_lock(home, session_id)) {
        return None;
    }
    let host = read_entries(home)
        .into_iter()
        .find(|entry| entry.session_id == session_id)
        .map(|entry| entry.id);
    // Between a host taking the lock and writing its entry, the session's own id has to do.
    Some(host.unwrap_or_else(|| session_id.to_string()))
}

/// Why a session a background session has open cannot be opened here.
pub fn in_background(home: &Path, session_id: &str) -> String {
    let id = host_of(home, session_id).unwrap_or_else(|| session_id.to_string());
    format!("this session is running in the background: azazel attach {id}")
}

/// The entry of the background session `id`, as it last wrote it.
pub fn read_entry(home: &Path, id: &str) -> Option<Entry> {
    serde_json::from_slice(&std::fs::read(entry_file(home, id)).ok()?).ok()
}

fn read_entries(home: &Path) -> Vec<Entry> {
    let Ok(files) = std::fs::read_dir(run_dir(home)) else {
        return Vec::new();
    };
    files
        .flatten()
        .map(|file| file.path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "json"))
        .filter_map(|path| serde_json::from_slice(&std::fs::read(path).ok()?).ok())
        .collect()
}

/// Removes what a background session that died without cleaning up left behind, and the logs
/// of sessions that ended long ago.
fn sweep(home: &Path, dead: &[Entry]) {
    for entry in dead {
        forget(home, entry);
    }
    let Ok(files) = std::fs::read_dir(run_dir(home)) else {
        return;
    };
    let now = SystemTime::now();
    for file in files.flatten() {
        let path = file.path();
        // The name of a session that is neither listed nor logged any more names nothing.
        if path.extension().is_some_and(|extension| extension == "req") {
            let named = std::fs::read_to_string(&path).unwrap_or_default();
            let named = named.trim();
            if !entry_file(home, named).exists() && !log_file(home, named).exists() {
                let _ = std::fs::remove_file(path);
            }
            continue;
        }
        let kept = path
            .extension()
            .is_some_and(|extension| extension == "log" || extension == "err");
        let old = file
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .is_some_and(|age| age > KEEP_LOGS);
        if kept && old && !path.with_extension("json").exists() {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// Removes what the background session of `entry`, whose process has gone, left behind. Its
/// log stays for as long as logs are kept.
pub fn forget(home: &Path, entry: &Entry) {
    let _ = std::fs::remove_file(entry_file(home, &entry.id));
    let _ = std::fs::remove_file(host_lock(home, &entry.id));
    let _ = std::fs::remove_file(session_lock(home, &entry.session_id));
    // A socket file; a named pipe went with its process.
    if Path::new(&entry.socket).is_absolute() && !entry.socket.starts_with(r"\\") {
        let _ = std::fs::remove_file(&entry.socket);
    }
}

/// The background sessions, oldest first: those that are running, and those whose process
/// died in the middle of a run (state `interrupted`). Entries of other processes that have
/// gone are removed on the way.
pub fn list(home: &Path) -> Vec<Entry> {
    let (mut listed, dead): (Vec<Entry>, Vec<Entry>) = read_entries(home)
        .into_iter()
        .partition(|entry| entry.alive(home));
    let mut gone = Vec::new();
    for entry in dead {
        match entry.clone().left_unfinished(home) {
            Some(unfinished) => listed.push(unfinished),
            None => gone.push(entry),
        }
    }
    sweep(home, &gone);
    listed.sort_by(|a, b| a.started.cmp(&b.started));
    listed
}

fn request_file(home: &Path, request_id: &str) -> PathBuf {
    let digest = Sha256::digest(request_id);
    let name: String = digest.iter().take(8).map(|byte| format!("{byte:02x}")).collect();
    run_dir(home).join(format!("req-{name}.req"))
}

/// Notes that the background session `id` is the one the request named `request_id` starts.
/// `Err` carries the id of the session an earlier request of that name started, when that
/// session is still listed or logged.
pub fn claim_request(home: &Path, request_id: &str, id: &str) -> io::Result<Result<(), String>> {
    create_run_dir(home)?;
    let file = request_file(home, request_id);
    loop {
        // Created in one step, so that of two requests at the same moment one finds the other.
        match File::options().write(true).create_new(true).open(&file) {
            Ok(mut created) => {
                io::Write::write_all(&mut created, id.as_bytes())?;
                return Ok(Ok(()));
            }
            Err(e) if e.kind() == io::ErrorKind::AlreadyExists => {
                let named = std::fs::read_to_string(&file).unwrap_or_default();
                let named = named.trim();
                if !named.is_empty() && (entry_file(home, named).exists() || log_file(home, named).exists()) {
                    return Ok(Err(named.to_string()));
                }
                // The session it names never started, or is long gone.
                std::fs::remove_file(&file)?;
            }
            Err(e) => return Err(e),
        }
    }
}

/// Takes back [`claim_request`] for a session that did not start.
pub fn release_request(home: &Path, request_id: &str) {
    let _ = std::fs::remove_file(request_file(home, request_id));
}

/// The background session `wanted` names, running or interrupted: its id or the start of it,
/// its session's name, or the start of its session's id.
pub fn find(home: &Path, wanted: &str) -> Result<Entry, String> {
    let all = list(home);
    let pick = |matches: Vec<&Entry>| match matches.as_slice() {
        [] => None,
        [one] => Some(Ok((*one).clone())),
        many => Some(Err(format!(
            "`{wanted}` matches {} background sessions: {}",
            many.len(),
            many.iter()
                .map(|entry| entry.id.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    };
    let by_id = all.iter().filter(|entry| entry.id.starts_with(wanted)).collect();
    let by_name = all
        .iter()
        .filter(|entry| entry.name.as_deref() == Some(wanted))
        .collect();
    let by_session = all
        .iter()
        .filter(|entry| entry.session_id.starts_with(wanted))
        .collect();
    pick(by_id)
        .or_else(|| pick(by_name))
        .or_else(|| pick(by_session))
        .unwrap_or_else(|| {
            Err(format!(
                "no background session `{wanted}` is running; `azazel ps` lists them"
            ))
        })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    fn entry(id: &str, session: &str, name: Option<&str>) -> Entry {
        Entry {
            id: id.into(),
            pid: 1,
            session_id: session.into(),
            session_file: PathBuf::from("s.jsonl"),
            cwd: PathBuf::from("."),
            name: name.map(str::to_string),
            started: format!("2026-01-01T00:00:0{}Z", id.len()),
            socket: "sock".into(),
            version: "0".into(),
            state: "idle".into(),
            cost_usd: None,
            mode: None,
            args: Vec::new(),
            interrupted_at: None,
        }
    }

    #[test]
    fn a_lock_is_held_until_it_is_dropped() {
        let home = tempfile::tempdir().unwrap();
        let path = home.path().join("a.lock");
        assert!(!Lock::held(&path));
        let lock = Lock::acquire(&path).unwrap().unwrap();
        assert!(Lock::held(&path));
        assert!(Lock::acquire(&path).unwrap().is_none());
        drop(lock);
        assert!(!Lock::held(&path));
        assert!(!path.exists());
    }

    #[test]
    fn a_session_open_in_the_background_names_who_has_it() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(host_of(home, "s-1"), None);
        let lock = lock_session(home, "s-1").unwrap();
        let _alive = Lock::acquire(&host_lock(home, "ab12")).unwrap().unwrap();
        entry("ab12", "s-1", None).write(home).unwrap();
        assert_eq!(host_of(home, "s-1").as_deref(), Some("ab12"));
        assert_eq!(
            lock_session(home, "s-1").unwrap_err(),
            "this session is running in the background: azazel attach ab12"
        );
        drop(lock);
        assert_eq!(host_of(home, "s-1"), None);
    }

    #[test]
    fn only_live_sessions_are_listed_and_found() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        create_run_dir(home).unwrap();
        let _one = Lock::acquire(&host_lock(home, "ab12")).unwrap().unwrap();
        let _two = Lock::acquire(&host_lock(home, "ab345")).unwrap().unwrap();
        entry("ab12", "s-1", Some("fix")).write(home).unwrap();
        entry("ab345", "t-2", None).write(home).unwrap();
        // Its process is gone: nothing holds its lock.
        entry("dead", "u-3", Some("old")).write(home).unwrap();
        std::fs::write(log_file(home, "dead"), "").unwrap();

        let ids: Vec<String> = list(home).into_iter().map(|entry| entry.id).collect();
        assert_eq!(ids, ["ab12", "ab345"]);
        assert!(!entry_file(home, "dead").exists());
        // The log of a session that ended stays for `azazel logs`.
        assert!(log_file(home, "dead").exists());

        assert_eq!(find(home, "ab3").unwrap().id, "ab345");
        assert_eq!(find(home, "fix").unwrap().id, "ab12");
        assert_eq!(find(home, "t-").unwrap().id, "ab345");
        assert_eq!(
            find(home, "ab").unwrap_err(),
            "`ab` matches 2 background sessions: ab12, ab345"
        );
        assert!(
            find(home, "old")
                .unwrap_err()
                .starts_with("no background session `old`")
        );
    }
    /// A session file whose last run has a start and no end.
    fn cut_short(dir: &Path) -> (String, PathBuf) {
        use azazel_agent::TranscriptSink;
        let store = SessionStore::create(dir, Path::new("/work"), None).unwrap();
        store.message(&azazel_ai::Message::user("go"));
        store.run_started(None);
        (store.id(), store.path())
    }

    #[test]
    fn a_session_whose_process_died_in_a_run_stays_listed_as_interrupted() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        create_run_dir(home).unwrap();
        let (session_id, session_file) = cut_short(&home.join("sessions"));
        let dead = Entry {
            session_file: session_file.clone(),
            state: "working".into(),
            ..entry("dead", &session_id, Some("night"))
        };
        dead.write(home).unwrap();

        let listed = list(home);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].interrupted() && !listed[0].alive(home));
        assert_eq!(find(home, "night").unwrap().state, INTERRUPTED);
        // Found once, the session file is not read again while it keeps its size.
        let kept = read_entry(home, "dead").unwrap();
        assert_eq!(
            kept.interrupted_at,
            Some(std::fs::metadata(&session_file).unwrap().len())
        );
        assert_eq!(list(home).len(), 1);

        // Someone went on with the run elsewhere and it ended: the entry has nothing to say.
        {
            use azazel_agent::TranscriptSink;
            let store = SessionStore::open(&session_file).unwrap();
            let run = store.interrupted_run(None).unwrap();
            store.message(&azazel_ai::Message {
                role: azazel_ai::Role::Assistant,
                content: vec![azazel_ai::Block::text("done")],
                meta: None,
            });
            store.run_ended(&run.run_id, "stop");
        }
        assert!(list(home).is_empty());
        assert!(!entry_file(home, "dead").exists());
    }

    #[test]
    fn an_interrupted_session_another_process_holds_is_not_listed() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        create_run_dir(home).unwrap();
        let (session_id, session_file) = cut_short(&home.join("sessions"));
        let dead = Entry {
            session_file,
            ..entry("dead", &session_id, None)
        };
        dead.write(home).unwrap();
        // The background session that was started in its place has the session.
        let _held = lock_session(home, &session_id).unwrap();
        assert!(list(home).is_empty());
    }

    #[test]
    fn a_named_request_is_claimed_once() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path();
        assert_eq!(claim_request(home, "nightly-42", "ab12").unwrap(), Ok(()));
        // The session it started is listed: the same request again names it.
        entry("ab12", "s-1", None).write(home).unwrap();
        assert_eq!(
            claim_request(home, "nightly-42", "cd34").unwrap(),
            Err("ab12".to_string())
        );
        assert_eq!(claim_request(home, "another", "cd34").unwrap(), Ok(()));
        // A session that never started, or is long gone, is not in the way.
        std::fs::remove_file(entry_file(home, "ab12")).unwrap();
        assert_eq!(claim_request(home, "nightly-42", "ef56").unwrap(), Ok(()));
        release_request(home, "nightly-42");
        assert_eq!(claim_request(home, "nightly-42", "0a0a").unwrap(), Ok(()));
    }
}
