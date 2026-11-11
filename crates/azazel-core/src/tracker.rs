//! Tracks what each agent has read so stale edits are refused, and serialises writes per path.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::OwnedMutexGuard;

use crate::paths::canonical;

#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    len: u64,
    hash: blake3::Hash,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Freshness {
    /// The file is as this agent last saw it.
    Fresh,
    /// This agent has not read the file.
    NeverRead,
    /// The file changed after this agent last read or wrote it.
    Changed,
}

#[derive(Default)]
pub struct FileTracker {
    seen: Mutex<HashMap<(String, PathBuf), Stamp>>,
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    /// `(agent, path, changed by)`: notes for agents whose view of a file went stale.
    stale: Mutex<Vec<(String, PathBuf, String)>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Remembers `content` as what `agent` knows of `path`. Call after reading or writing it.
    pub fn record(&self, agent: &str, path: &Path, content: &[u8]) {
        let stamp = Stamp {
            len: content.len() as u64,
            hash: blake3::hash(content),
        };
        locked(&self.seen).insert((agent.to_string(), canonical(path)), stamp);
    }

    /// Like [`FileTracker::record`] for a write, and queues a note for every other agent that has
    /// read the file.
    pub fn record_write(&self, agent: &str, path: &Path, content: &[u8]) {
        self.record(agent, path, content);
        let path = canonical(path);
        let readers: Vec<String> = locked(&self.seen)
            .keys()
            .filter(|(reader, seen_path)| reader != agent && seen_path == &path)
            .map(|(reader, _)| reader.clone())
            .collect();
        let mut stale = locked(&self.stale);
        for reader in readers {
            if !stale.iter().any(|(r, p, _)| r == &reader && p == &path) {
                stale.push((reader, path.clone(), agent.to_string()));
            }
        }
    }

    /// Whether `agent` may overwrite `path` without reading it again.
    pub fn check(&self, agent: &str, path: &Path) -> Freshness {
        let path = canonical(path);
        let Some(stamp) = locked(&self.seen)
            .get(&(agent.to_string(), path.clone()))
            .cloned()
        else {
            return Freshness::NeverRead;
        };
        // Timestamps are too coarse to trust on some filesystems; the content decides.
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() == stamp.len => {}
            _ => return Freshness::Changed,
        }
        match std::fs::read(&path) {
            Ok(bytes) if blake3::hash(&bytes) == stamp.hash => Freshness::Fresh,
            _ => Freshness::Changed,
        }
    }

    /// Serialises writers of one path. Hold the guard across check, write and record.
    pub async fn lock(&self, path: &Path) -> OwnedMutexGuard<()> {
        let lock = locked(&self.locks).entry(canonical(path)).or_default().clone();
        lock.lock_owned().await
    }

    /// Files `agent` has read that another agent changed since, as `(path, changed by)`. Each note
    /// is returned once.
    pub fn take_stale(&self, agent: &str) -> Vec<(PathBuf, String)> {
        let mut stale = locked(&self.stale);
        let mut mine = Vec::new();
        stale.retain(|(reader, path, by)| {
            if reader == agent {
                mine.push((path.clone(), by.clone()));
                false
            } else {
                true
            }
        });
        mine
    }

    /// Drops everything remembered for `agent`.
    pub fn forget(&self, agent: &str) {
        locked(&self.seen).retain(|(a, _), _| a != agent);
        locked(&self.stale).retain(|(a, _, _)| a != agent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn freshness_follows_content_not_timestamps() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let tracker = FileTracker::new();
        assert_eq!(tracker.check("lead", &path), Freshness::NeverRead);

        tracker.record("lead", &path, b"one");
        assert_eq!(tracker.check("lead", &path), Freshness::Fresh);
        assert_eq!(tracker.check("scout-1", &path), Freshness::NeverRead);

        // Rewritten with the same bytes: still fresh. A same-length change below must be caught
        // even when the timestamp does not move.
        std::fs::write(&path, "one").unwrap();
        assert_eq!(tracker.check("lead", &path), Freshness::Fresh);

        std::fs::write(&path, "two").unwrap();
        assert_eq!(tracker.check("lead", &path), Freshness::Changed);
        std::fs::remove_file(&path).unwrap();
        assert_eq!(tracker.check("lead", &path), Freshness::Changed);
    }

    #[test]
    fn a_write_notifies_other_readers_once() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        std::fs::write(&path, "one").unwrap();
        let tracker = FileTracker::new();
        tracker.record("lead", &path, b"one");
        tracker.record("coder-1", &path, b"one");

        std::fs::write(&path, "two").unwrap();
        tracker.record_write("coder-1", &path, b"two");
        tracker.record_write("coder-1", &path, b"two");
        assert_eq!(tracker.check("coder-1", &path), Freshness::Fresh);
        assert_eq!(tracker.check("lead", &path), Freshness::Changed);
        assert_eq!(
            tracker.take_stale("lead"),
            vec![(canonical(&path), "coder-1".to_string())]
        );
        assert!(tracker.take_stale("lead").is_empty());
        assert!(tracker.take_stale("coder-1").is_empty());

        tracker.forget("lead");
        assert_eq!(tracker.check("lead", &path), Freshness::NeverRead);
    }

    #[tokio::test]
    async fn path_lock_admits_one_writer_at_a_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.txt");
        let tracker = Arc::new(FileTracker::new());
        let guard = tracker.lock(&path).await;
        let other = {
            let tracker = tracker.clone();
            let path = path.clone();
            tokio::spawn(async move { drop(tracker.lock(&path).await) })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!other.is_finished());
        drop(guard);
        other.await.unwrap();
    }
}
