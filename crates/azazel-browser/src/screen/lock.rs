//! One azazel session at a time drives the mouse and keyboard: a lock file under the azazel home
//! that names the process and the session holding it.

use std::fs;
use std::io::{ErrorKind, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    pid: u32,
    session: String,
}

/// The lock, held by this process for `session`.
#[derive(Debug)]
pub struct Lock {
    path: PathBuf,
    stamp: Stamp,
}

impl Lock {
    /// Takes the lock at `path`. A lock this session already holds is taken again; one whose
    /// process `alive` no longer finds is recovered; another live session's lock is an error that
    /// names that session.
    pub fn acquire(path: &Path, session: &str, alive: impl Fn(u32) -> bool) -> Result<Lock, String> {
        let stamp = Stamp {
            pid: std::process::id(),
            session: session.to_string(),
        };
        let json = serde_json::to_string(&stamp).map_err(|e| e.to_string())?;
        if let Some(dir) = path.parent() {
            fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        // The file is written whole under a name of its own and linked into place: a competing
        // process sees no lock or a complete one, never a half-written one.
        let staged = path.with_extension(format!("{}.tmp", stamp.pid));
        fs::File::create(&staged)
            .and_then(|mut file| file.write_all(json.as_bytes()))
            .map_err(|e| format!("{}: {e}", staged.display()))?;
        let taken = Self::link(path, &staged, &stamp, &alive);
        let _ = fs::remove_file(&staged);
        taken.map(|()| Lock {
            path: path.to_path_buf(),
            stamp,
        })
    }

    fn link(path: &Path, staged: &Path, ours: &Stamp, alive: &impl Fn(u32) -> bool) -> Result<(), String> {
        for _ in 0..2 {
            match fs::hard_link(staged, path) {
                Ok(()) => return Ok(()),
                Err(e) if e.kind() == ErrorKind::AlreadyExists => {}
                Err(e) => return Err(format!("{}: {e}", path.display())),
            }
            let text = fs::read_to_string(path).unwrap_or_default();
            match serde_json::from_str::<Stamp>(&text) {
                Ok(held) if held == *ours => return Ok(()),
                Ok(held) if alive(held.pid) => {
                    return Err(format!(
                        "another azazel session (`{}`, process {}) is driving the mouse and keyboard; \
                         wait until it is done, or stop it ({})",
                        held.session,
                        held.pid,
                        path.display()
                    ));
                }
                // Its process is gone, or the file is not a lock at all: take it over, unless
                // somebody else has replaced it since it was read.
                _ => {
                    // shortcut: two processes recovering the same stale lock at the very same
                    // moment can both win; upgrade when that is seen in practice
                    if fs::read_to_string(path).unwrap_or_default() == text {
                        let _ = fs::remove_file(path);
                    }
                }
            }
        }
        Err(format!("could not take the lock {}", path.display()))
    }

    /// Removes the lock file, when it is still ours.
    pub fn release(&self) {
        let ours = fs::read_to_string(&self.path)
            .ok()
            .and_then(|text| serde_json::from_str::<Stamp>(&text).ok())
            .is_some_and(|held| held == self.stamp);
        if ours {
            let _ = fs::remove_file(&self.path);
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    const GONE: u32 = 4_000_000_000;

    fn stamp_file(path: &Path, pid: u32, session: &str) {
        let stamp = Stamp {
            pid,
            session: session.to_string(),
        };
        fs::write(path, serde_json::to_string(&stamp).unwrap()).unwrap();
    }

    #[test]
    fn the_lock_is_created_with_pid_and_session_and_released() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("home").join("screen.lock");
        let lock = Lock::acquire(&path, "s1", |_| true).unwrap();
        let held: Stamp = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            held,
            Stamp {
                pid: std::process::id(),
                session: "s1".to_string()
            }
        );
        // No staging file is left beside it.
        assert_eq!(fs::read_dir(path.parent().unwrap()).unwrap().count(), 1);
        lock.release();
        assert!(!path.exists());
    }

    #[test]
    fn a_live_session_blocks_another_and_is_named() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen.lock");
        stamp_file(&path, 4242, "other-session");
        let error = Lock::acquire(&path, "mine", |pid| pid == 4242).unwrap_err();
        assert!(
            error.contains("other-session") && error.contains("4242"),
            "{error}"
        );
        // The other session's lock is untouched.
        assert!(path.exists());
    }

    #[test]
    fn a_lock_of_a_gone_process_is_recovered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen.lock");
        stamp_file(&path, GONE, "crashed");
        let lock = Lock::acquire(&path, "mine", |_| false).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains("mine"));
        lock.release();
        assert!(!path.exists());

        // A file that is not a lock is taken over too.
        fs::write(&path, "garbage").unwrap();
        Lock::acquire(&path, "mine", |_| true).unwrap();
    }

    #[test]
    fn taking_the_lock_again_and_releasing_a_lost_one() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("screen.lock");
        let first = Lock::acquire(&path, "mine", |_| true).unwrap();
        let again = Lock::acquire(&path, "mine", |_| true).unwrap();
        // Somebody else holds it now: releasing the old handle must not remove their lock.
        stamp_file(&path, 4242, "other");
        first.release();
        again.release();
        assert!(path.exists());
    }
}
