//! Tracks what each agent has read so stale edits are refused, and serialises writes per path.
//!
//! Beside the content of a file as an agent last saw it, the tracker keeps which of its lines the
//! agent has seen: a read of lines 40 to 80 covers those lines and no others. Coverage follows
//! the file through the agent's own edits, and through changes made by others as far as the
//! per-line hashes of the content show which lines are still the ones that were read.

use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hasher};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tokio::sync::OwnedMutexGuard;

use crate::paths::canonical;

/// Files with more lines than this keep no per-line hashes: when one changes under an agent that
/// has read only part of it, what the agent had read no longer counts.
pub const MAX_LINE_HASHES: usize = 3000;

/// Lines `first..=last`, counted from 1.
pub type LineRange = (u32, u32);

/// A run of `old` lines starting at line `first` that an edit replaced with `new` lines. Lines
/// are counted in the text as it was just before this replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Splice {
    pub first: u32,
    pub old: u32,
    pub new: u32,
}

/// Which lines of a file an agent has seen.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Cover {
    All,
    /// Sorted ranges that neither overlap nor touch.
    Lines(Vec<LineRange>),
}

/// What becomes of the lines between the unchanged head and tail of a file that changed.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Changed {
    /// Somebody else changed them: they have not been seen.
    Unseen,
    /// A formatter rewrote them after the agent's own write: they count as seen when everything
    /// they replace had been.
    SeenIfReplacedWere,
}

fn merged(mut ranges: Vec<LineRange>) -> Vec<LineRange> {
    ranges.retain(|(first, last)| first <= last && *first >= 1);
    ranges.sort_unstable();
    let mut out: Vec<LineRange> = Vec::with_capacity(ranges.len());
    for (first, last) in ranges {
        match out.last_mut() {
            Some(before) if first <= before.1.saturating_add(1) => before.1 = before.1.max(last),
            _ => out.push((first, last)),
        }
    }
    out
}

/// The parts of `range` that `ranges` (sorted, merged) leave out.
fn gaps(ranges: &[LineRange], (mut first, last): LineRange, out: &mut Vec<LineRange>) {
    for &(start, end) in ranges {
        if end < first {
            continue;
        }
        if start > last {
            break;
        }
        if start > first {
            out.push((first, start - 1));
        }
        first = end.saturating_add(1);
        if first > last {
            return;
        }
    }
    if first <= last {
        out.push((first, last));
    }
}

fn shifted(line: u32, by: i64) -> u32 {
    u32::try_from(i64::from(line) + by).unwrap_or(u32::MAX)
}

impl Cover {
    fn none() -> Cover {
        Cover::Lines(Vec::new())
    }

    fn add(&mut self, range: LineRange) {
        if let Cover::Lines(ranges) = self {
            let mut all = std::mem::take(ranges);
            all.push(range);
            *ranges = merged(all);
        }
    }

    /// Lines 1 to `total` all seen is the whole file seen.
    fn settle(&mut self, total: u32) {
        if let Cover::Lines(ranges) = self
            && (total == 0
                || ranges
                    .first()
                    .is_some_and(|&(first, last)| first == 1 && last >= total))
        {
            *self = Cover::All;
        }
    }

    /// The parts of `targets` that have not been seen.
    fn unread(&self, targets: &[LineRange]) -> Vec<LineRange> {
        let Cover::Lines(ranges) = self else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for &target in targets {
            gaps(ranges, target, &mut out);
        }
        merged(out)
    }

    fn splice(&mut self, splice: Splice) {
        let Cover::Lines(ranges) = self else {
            return;
        };
        let end = splice.first.saturating_add(splice.old);
        let by = i64::from(splice.new) - i64::from(splice.old);
        let mut out = Vec::with_capacity(ranges.len() + 2);
        for &(first, last) in ranges.iter() {
            if last < splice.first {
                out.push((first, last));
                continue;
            }
            if first < splice.first {
                out.push((first, splice.first - 1));
            }
            if last >= end {
                out.push((shifted(first.max(end), by), shifted(last, by)));
            }
        }
        if splice.new > 0 {
            out.push((splice.first, splice.first + splice.new - 1));
        }
        *ranges = merged(out);
    }

    /// What is still seen once the content whose lines hashed to `old` has become the content
    /// whose lines hash to `new`: the lines of the unchanged head, those of the unchanged tail
    /// where they now are, and of what lies between whatever `changed` says.
    fn carried(self, old: Option<&[u64]>, new: Option<&[u64]>, changed: Changed) -> Cover {
        let ranges = match self {
            // Nothing the formatter replaced was unseen.
            Cover::All if changed == Changed::SeenIfReplacedWere => return Cover::All,
            Cover::All => vec![(
                1,
                old.map_or(0, |old| u32::try_from(old.len()).unwrap_or(u32::MAX)),
            )],
            Cover::Lines(ranges) => ranges,
        };
        let (Some(old), Some(new)) = (old, new) else {
            return Cover::none();
        };
        let head = old.iter().zip(new).take_while(|(a, b)| a == b).count();
        let most = old.len().min(new.len()) - head;
        let tail = old
            .iter()
            .rev()
            .zip(new.iter().rev())
            .take(most)
            .take_while(|(a, b)| a == b)
            .count();
        let line = |index: usize| u32::try_from(index).unwrap_or(u32::MAX);
        let (head, old_tail, new_tail) = (line(head), line(old.len() - tail), line(new.len() - tail));
        let by = i64::from(new_tail) - i64::from(old_tail);
        let mut out = Vec::with_capacity(ranges.len() + 1);
        for &(first, last) in &ranges {
            if first <= head {
                out.push((first, last.min(head)));
            }
            if last > old_tail {
                out.push((shifted(first.max(old_tail + 1), by), shifted(last, by)));
            }
        }
        if changed == Changed::SeenIfReplacedWere && new_tail > head {
            // An insertion replaces nothing; it is seen when the line it follows was.
            let replaced = if old_tail > head {
                (head + 1, old_tail)
            } else {
                (head.max(1), head.max(1))
            };
            let mut unseen = Vec::new();
            gaps(&ranges, replaced, &mut unseen);
            if unseen.is_empty() {
                out.push((head + 1, new_tail));
            }
        }
        Cover::Lines(merged(out))
    }
}

/// The lines of `content`: split at `\n`, a final line needing no line ending.
fn lines_of(content: &[u8]) -> impl Iterator<Item = &[u8]> {
    let body = content.strip_suffix(b"\n").unwrap_or(content);
    let empty = content.is_empty();
    body.split(|byte| *byte == b'\n').filter(move |_| !empty)
}

fn line_count(content: &[u8]) -> u32 {
    let ends = content.iter().filter(|byte| **byte == b'\n').count();
    let open = usize::from(content.last().is_some_and(|last| *last != b'\n'));
    u32::try_from(ends + open).unwrap_or(u32::MAX)
}

/// One hash per line of `content`, or `None` when it has more than [`MAX_LINE_HASHES`] lines.
fn line_hashes(content: &[u8]) -> Option<Box<[u64]>> {
    let mut hashes = Vec::new();
    for line in lines_of(content) {
        if hashes.len() == MAX_LINE_HASHES {
            return None;
        }
        let mut hasher = DefaultHasher::new();
        hasher.write(line);
        hashes.push(hasher.finish());
    }
    Some(hashes.into_boxed_slice())
}

/// What tells one content of a file from another.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Print {
    len: u64,
    hash: blake3::Hash,
}

impl Print {
    fn of(content: &[u8]) -> Print {
        Print {
            len: content.len() as u64,
            hash: blake3::hash(content),
        }
    }
}

/// A content of a file as an agent knows it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Stamp {
    print: Print,
    cover: Cover,
    lines: Option<Box<[u64]>>,
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
    /// Files whose next edit the user has allowed whatever was read of them.
    allowed: Mutex<HashSet<PathBuf>>,
}

fn locked<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FileTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores what `agent` now knows of `path`: `content`, and the lines `cover` makes of what
    /// the agent knew before (`None` when it knew nothing), given the line hashes of `content`.
    /// The content is hashed before the map is locked, so a large file holds nobody up.
    fn stamp(
        &self,
        agent: &str,
        path: PathBuf,
        content: &[u8],
        cover: impl FnOnce(Option<Stamp>, Option<&[u64]>) -> Cover,
    ) {
        let key = (agent.to_string(), path);
        let (print, lines, total) = (Print::of(content), line_hashes(content), line_count(content));
        let mut seen = locked(&self.seen);
        let before = seen.remove(&key);
        let mut cover = cover(before, lines.as_deref());
        cover.settle(total);
        seen.insert(key, Stamp { print, cover, lines });
    }

    /// Queues a note for every agent other than `agent` that has read `path`.
    fn notify(&self, agent: &str, path: &Path) {
        let readers: Vec<String> = locked(&self.seen)
            .keys()
            .filter(|(reader, seen_path)| reader != agent && seen_path == path)
            .map(|(reader, _)| reader.clone())
            .collect();
        let mut stale = locked(&self.stale);
        for reader in readers {
            if !stale.iter().any(|(r, p, _)| r == &reader && p == path) {
                stale.push((reader, path.to_path_buf(), agent.to_string()));
            }
        }
    }

    /// Remembers `content` as what `agent` knows of `path`, all of it seen. Call after reading
    /// the whole file.
    pub fn record(&self, agent: &str, path: &Path, content: &[u8]) {
        self.stamp(agent, canonical(path), content, |_, _| Cover::All);
    }

    /// Remembers `content` as what `agent` knows of `path`, of which it has now seen lines
    /// `first..=last` beside those it had seen before. Lines seen in an earlier content count
    /// while they are still the same lines.
    pub fn record_lines(&self, agent: &str, path: &Path, content: &[u8], first: u32, last: u32) {
        self.record_ranges(agent, path, content, &[(first, last)]);
    }

    /// Like [`FileTracker::record_lines`] for several runs of lines seen at once.
    pub fn record_ranges(&self, agent: &str, path: &Path, content: &[u8], ranges: &[LineRange]) {
        let print = Print::of(content);
        self.stamp(agent, canonical(path), content, |before, now| {
            let mut cover = match before {
                Some(before) if before.print == print => before.cover,
                Some(before) => before
                    .cover
                    .carried(before.lines.as_deref(), now, Changed::Unseen),
                None => Cover::none(),
            };
            for range in ranges {
                cover.add(*range);
            }
            cover
        });
    }

    /// Like [`FileTracker::record`] for a write of the whole file, and queues a note for every
    /// other agent that has read the file.
    pub fn record_write(&self, agent: &str, path: &Path, content: &[u8]) {
        let path = canonical(path);
        self.stamp(agent, path.clone(), content, |_, _| Cover::All);
        self.notify(agent, &path);
    }

    /// Remembers that `agent` turned `old` into `new` by the replacements `splices`, applied in
    /// that order. The lines it wrote are seen; the others are as seen as they were, which is
    /// not at all when `old` is not what the agent last knew of the file. Queues a note for
    /// every other agent that has read the file.
    pub fn record_edit(&self, agent: &str, path: &Path, old: &[u8], new: &[u8], splices: &[Splice]) {
        let path = canonical(path);
        let was = Print::of(old);
        self.stamp(agent, path.clone(), new, |before, _| {
            let mut cover = match before {
                Some(before) if before.print == was => before.cover,
                _ => Cover::none(),
            };
            for splice in splices {
                cover.splice(*splice);
            }
            cover
        });
        self.notify(agent, &path);
    }

    /// Remembers `content` as what a formatter made of the file `agent` has just written. The
    /// lines it left alone are as seen as they were; the lines it rewrote are seen when all of
    /// what they replace was, so an agent may go on editing what it wrote. Queues a note for
    /// every other agent that has read the file.
    pub fn record_formatted(&self, agent: &str, path: &Path, content: &[u8]) {
        let path = canonical(path);
        self.stamp(agent, path.clone(), content, |before, now| match before {
            Some(before) => before
                .cover
                .carried(before.lines.as_deref(), now, Changed::SeenIfReplacedWere),
            None => Cover::none(),
        });
        self.notify(agent, &path);
    }

    /// Whether `agent` may overwrite `path` without reading it again.
    pub fn check(&self, agent: &str, path: &Path) -> Freshness {
        let path = canonical(path);
        let Some(print) = locked(&self.seen)
            .get(&(agent.to_string(), path.clone()))
            .map(|stamp| stamp.print.clone())
        else {
            return Freshness::NeverRead;
        };
        // Timestamps are too coarse to trust on some filesystems; the content decides.
        match std::fs::metadata(&path) {
            Ok(meta) if meta.len() == print.len => {}
            _ => return Freshness::Changed,
        }
        match std::fs::read(&path) {
            Ok(bytes) if blake3::hash(&bytes) == print.hash => Freshness::Fresh,
            _ => Freshness::Changed,
        }
    }

    /// The parts of `targets` that `agent` has not seen in what it knows of `path`: all of them
    /// when it knows nothing of the file, none when it has seen every line. The file itself is
    /// not looked at; [`FileTracker::check`] says whether it is still that content.
    pub fn unread(&self, agent: &str, path: &Path, targets: &[LineRange]) -> Vec<LineRange> {
        match locked(&self.seen).get(&(agent.to_string(), canonical(path))) {
            Some(stamp) => stamp.cover.unread(targets),
            None => merged(targets.to_vec()),
        }
    }

    /// Whether `agent` has seen every line of what it knows of `path`.
    pub fn read_whole(&self, agent: &str, path: &Path) -> bool {
        locked(&self.seen)
            .get(&(agent.to_string(), canonical(path)))
            .is_some_and(|stamp| stamp.cover == Cover::All)
    }

    /// Lets the next edit of `path` through whatever its agent has read of it.
    pub fn allow_once(&self, path: &Path) {
        locked(&self.allowed).insert(canonical(path));
    }

    /// Whether an edit of `path` was allowed with [`FileTracker::allow_once`]; the allowance is
    /// used up by asking.
    pub fn take_allowed(&self, path: &Path) -> bool {
        locked(&self.allowed).remove(&canonical(path))
    }

    /// The file at `from` is now at `to` with the same content: what every agent knew of it, it
    /// knows of the file at its new place.
    pub fn moved(&self, from: &Path, to: &Path) {
        let (from, to) = (canonical(from), canonical(to));
        let mut seen = locked(&self.seen);
        let agents: Vec<String> = seen
            .keys()
            .filter(|(_, path)| *path == from)
            .map(|(agent, _)| agent.clone())
            .collect();
        for agent in agents {
            if let Some(stamp) = seen.remove(&(agent.clone(), from.clone())) {
                seen.insert((agent, to.clone()), stamp);
            }
        }
    }

    /// Serialises writers of one path. Hold the guard across check, write and record.
    pub async fn lock(&self, path: &Path) -> OwnedMutexGuard<()> {
        let lock = locked(&self.locks).entry(canonical(path)).or_default().clone();
        lock.lock_owned().await
    }

    /// Locks every one of `paths`, in one order whoever asks, so that two writers of the same
    /// files cannot wait on each other. A path given twice is locked once.
    pub async fn lock_all(&self, paths: &[&Path]) -> Vec<OwnedMutexGuard<()>> {
        let mut ordered: Vec<PathBuf> = paths.iter().map(|path| canonical(path)).collect();
        ordered.sort();
        ordered.dedup();
        let mut guards = Vec::with_capacity(ordered.len());
        for path in &ordered {
            guards.push(self.lock(path).await);
        }
        guards
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

    /// `count` lines `line 1` to `line <count>`.
    fn numbered(count: usize) -> String {
        (1..=count).map(|n| format!("line {n}\n")).collect()
    }

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
        tracker.record_lines("scout-1", &path, b"one", 1, 1);
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

        // An edit and a formatter's rewrite are writes like any other to those who read the file.
        let splice = Splice {
            first: 1,
            old: 1,
            new: 1,
        };
        tracker.record_edit("coder-1", &path, b"two", b"three", &[splice]);
        assert_eq!(tracker.take_stale("scout-1").len(), 1);
        tracker.record_formatted("coder-1", &path, b"three\n");
        assert_eq!(tracker.take_stale("scout-1").len(), 1);
        assert_eq!(tracker.take_stale("lead").len(), 1);

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

        // Several files are locked in one order, however they are named and listed.
        let (a, b) = (dir.path().join("a.txt"), dir.path().join("b.txt"));
        let held = tracker.lock_all(&[&b, &a, &b]).await;
        assert_eq!(held.len(), 2);
        let other = {
            let (tracker, a, b) = (tracker.clone(), a.clone(), b.clone());
            tokio::spawn(async move { tracker.lock_all(&[&a, &b]).await.len() })
        };
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        assert!(!other.is_finished());
        drop(held);
        assert_eq!(other.await.unwrap(), 2);
    }

    #[test]
    fn lines_read_in_several_goes_add_up() {
        let path = Path::new("ranges.rs");
        let text = numbered(100);
        let tracker = FileTracker::new();
        assert_eq!(tracker.unread("lead", path, &[(5, 9)]), [(5, 9)], "never read");

        tracker.record_lines("lead", path, text.as_bytes(), 10, 20);
        tracker.record_lines("lead", path, text.as_bytes(), 40, 50);
        assert_eq!(tracker.unread("lead", path, &[(10, 20), (45, 50)]), []);
        assert_eq!(
            tracker.unread("lead", path, &[(5, 45), (48, 60), (58, 70)]),
            [(5, 9), (21, 39), (51, 70)],
            "the unread parts, joined where they meet"
        );
        assert!(!tracker.read_whole("lead", path));
        // What another agent read is its own.
        assert_eq!(tracker.unread("scout-1", path, &[(10, 20)]), [(10, 20)]);

        // Overlapping and adjoining reads merge into one run; with the rest read the file is read.
        tracker.record_lines("lead", path, text.as_bytes(), 18, 39);
        assert_eq!(tracker.unread("lead", path, &[(10, 50)]), []);
        tracker.record_lines("lead", path, text.as_bytes(), 1, 9);
        assert!(!tracker.read_whole("lead", path));
        tracker.record_lines("lead", path, text.as_bytes(), 51, 100);
        assert!(tracker.read_whole("lead", path));
        assert_eq!(tracker.unread("lead", path, &[(1, 500)]), []);

        // A whole read needs no ranges, and a file without a final line ending counts its last line.
        tracker.record("scout-1", path, text.as_bytes());
        assert!(tracker.read_whole("scout-1", path));
        tracker.record_lines("coder-1", Path::new("open.rs"), b"a\nb", 1, 2);
        assert!(tracker.read_whole("coder-1", Path::new("open.rs")));
        tracker.record_lines("coder-1", Path::new("empty.rs"), b"", 1, 1);
        assert!(tracker.read_whole("coder-1", Path::new("empty.rs")));
    }

    #[test]
    fn a_change_by_someone_else_leaves_only_the_unchanged_lines_read() {
        let path = Path::new("changed.rs");
        let text = numbered(60);
        let tracker = FileTracker::new();
        tracker.record_lines("lead", path, text.as_bytes(), 1, 20);
        tracker.record_lines("lead", path, text.as_bytes(), 41, 60);

        // Lines 30 and 31 become three lines: what was read above them stays where it was, what
        // was read below them is one line further down.
        let changed = text.replace("line 30\nline 31\n", "x\ny\nz\n");
        tracker.record_lines("lead", path, changed.as_bytes(), 30, 32);
        assert_eq!(
            tracker.unread("lead", path, &[(1, 61)]),
            [(21, 29), (33, 41)],
            "old 41-60 is now 42-61; the changed lines were read just now"
        );

        // A change inside what was read takes those lines out, and only those.
        let again = changed.replace("line 10\n", "ten\n");
        tracker.record_lines("lead", path, again.as_bytes(), 61, 61);
        assert_eq!(
            tracker.unread("lead", path, &[(1, 61)]),
            [(10, 10), (21, 29), (33, 41)]
        );

        // Everything read, then one line changed: reading that line again is enough.
        tracker.record("scout-1", path, text.as_bytes());
        let one = text.replace("line 7\n", "seven\n");
        tracker.record_lines("scout-1", path, one.as_bytes(), 7, 7);
        assert!(tracker.read_whole("scout-1", path));
        // Without that read the changed line is the one thing not read.
        tracker.record("coder-1", path, text.as_bytes());
        tracker.record_lines("coder-1", path, one.as_bytes(), 1, 1);
        assert_eq!(tracker.unread("coder-1", path, &[(1, 60)]), [(7, 7)]);
    }

    #[test]
    fn a_file_too_long_for_line_hashes_starts_over_when_it_changes() {
        let path = Path::new("long.rs");
        let text = numbered(MAX_LINE_HASHES + 1);
        let tracker = FileTracker::new();
        tracker.record_lines("lead", path, text.as_bytes(), 1, 100);
        tracker.record_lines("lead", path, text.as_bytes(), 200, 300);
        assert_eq!(tracker.unread("lead", path, &[(1, 300)]), [(101, 199)]);

        let changed = text.replace("line 2000\n", "x\n");
        tracker.record_lines("lead", path, changed.as_bytes(), 500, 510);
        assert_eq!(
            tracker.unread("lead", path, &[(1, 600)]),
            [(1, 499), (511, 600)],
            "which lines are still the ones read cannot be told"
        );
        // At the limit they can.
        let short = numbered(MAX_LINE_HASHES);
        tracker.record_lines("scout-1", path, short.as_bytes(), 1, 100);
        let changed = short.replace("line 2000\n", "x\n");
        tracker.record_lines("scout-1", path, changed.as_bytes(), 500, 510);
        assert_eq!(
            tracker.unread("scout-1", path, &[(1, 600)]),
            [(101, 499), (511, 600)]
        );
    }

    #[test]
    fn an_agents_own_edit_moves_what_it_read_and_adds_what_it_wrote() {
        let path = Path::new("edited.rs");
        let old = numbered(50);
        let tracker = FileTracker::new();
        tracker.record_lines("lead", path, old.as_bytes(), 10, 30);

        // Lines 12-13 become five lines; then old line 37, which is line 40 by then, goes.
        let new = old
            .replace("line 12\nline 13\n", "a\nb\nc\nd\ne\n")
            .replace("line 37\n", "");
        let splices = [
            Splice {
                first: 12,
                old: 2,
                new: 5,
            },
            Splice {
                first: 40,
                old: 1,
                new: 0,
            },
        ];
        tracker.record_edit("lead", path, old.as_bytes(), new.as_bytes(), &splices);
        assert_eq!(
            tracker.unread("lead", path, &[(1, 52)]),
            [(1, 9), (34, 52)],
            "10-30 grew by three lines; removing an unread line adds nothing"
        );

        // A replacement in unread lines makes exactly the written lines read.
        let splice = Splice {
            first: 45,
            old: 1,
            new: 2,
        };
        let newer = new.replace("line 43\n", "x\ny\n");
        tracker.record_edit("lead", path, new.as_bytes(), newer.as_bytes(), &[splice]);
        assert_eq!(
            tracker.unread("lead", path, &[(1, 53)]),
            [(1, 9), (34, 44), (47, 53)]
        );

        // An edit of a content the agent did not know leaves only what it wrote as read.
        tracker.record_edit(
            "lead",
            path,
            b"other\ncontent\n",
            b"other\nnew\n",
            &[Splice {
                first: 2,
                old: 1,
                new: 1,
            }],
        );
        assert_eq!(tracker.unread("lead", path, &[(1, 2)]), [(1, 1)]);
        // An agent that had read everything still has.
        tracker.record("scout-1", path, old.as_bytes());
        tracker.record_edit("scout-1", path, old.as_bytes(), new.as_bytes(), &splices);
        assert!(tracker.read_whole("scout-1", path));
    }

    #[test]
    fn a_formatters_rewrite_is_read_only_where_what_it_replaced_was() {
        let path = Path::new("formatted.rs");
        let written = numbered(40);
        let tracker = FileTracker::new();

        // The formatter splits line 12, which the agent had read: the three lines are read.
        tracker.record_lines("lead", path, written.as_bytes(), 10, 20);
        let formatted = written.replace("line 12\n", "line\n12\nsplit\n");
        tracker.record_formatted("lead", path, formatted.as_bytes());
        assert_eq!(tracker.unread("lead", path, &[(1, 42)]), [(1, 9), (23, 42)]);

        // It rewrites lines 5 to 30, of which the agent had read a part: none of them is read.
        tracker.record_lines("coder-1", path, written.as_bytes(), 10, 20);
        tracker.record_lines("coder-1", path, written.as_bytes(), 35, 40);
        let formatted = written
            .replace("line 5\n", "five\n")
            .replace("line 30\n", "thirty\n");
        tracker.record_formatted("coder-1", path, formatted.as_bytes());
        assert_eq!(tracker.unread("coder-1", path, &[(1, 40)]), [(1, 34)]);

        // Lines it adds after a line that was read are read; the whole file stays the whole file.
        tracker.record_lines("scout-1", path, written.as_bytes(), 1, 10);
        let formatted = written.replace("line 10\n", "line 10\n\n");
        tracker.record_formatted("scout-1", path, formatted.as_bytes());
        assert_eq!(tracker.unread("scout-1", path, &[(1, 41)]), [(12, 41)]);
        tracker.record_write("lead", path, written.as_bytes());
        tracker.record_formatted("lead", path, b"something else entirely\n");
        assert!(tracker.read_whole("lead", path));
    }

    #[test]
    fn an_allowance_is_for_one_edit_and_a_move_takes_the_reads_along() {
        let dir = tempfile::tempdir().unwrap();
        let (from, to) = (dir.path().join("a.rs"), dir.path().join("sub/b.rs"));
        std::fs::write(&from, "one\ntwo\n").unwrap();
        let tracker = FileTracker::new();
        assert!(!tracker.take_allowed(&from));
        tracker.allow_once(&from);
        assert!(!tracker.take_allowed(&to));
        assert!(tracker.take_allowed(&from));
        assert!(!tracker.take_allowed(&from));

        tracker.record_lines("lead", &from, b"one\ntwo\n", 2, 2);
        tracker.record("scout-1", &from, b"one\ntwo\n");
        std::fs::create_dir_all(to.parent().unwrap()).unwrap();
        std::fs::rename(&from, &to).unwrap();
        tracker.moved(&from, &to);
        assert_eq!(tracker.check("lead", &from), Freshness::NeverRead);
        assert_eq!(tracker.check("lead", &to), Freshness::Fresh);
        assert_eq!(tracker.unread("lead", &to, &[(1, 2)]), [(1, 1)]);
        assert!(tracker.read_whole("scout-1", &to));
    }
}
