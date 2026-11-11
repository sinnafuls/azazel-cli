//! The search index: which files are in it, how they are cut into chunks, and chunk search.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use azazel_core::paths::canonical;
use globset::Glob;
use ignore::WalkBuilder;
use rusqlite::{Connection, params};
use tokio::sync::mpsc;

use crate::search::{CANDIDATES, dot, from_blob, fts_query, fuse, top};
use crate::store::{MemoryStore, embed_pending};

/// The collection holding the project's own files.
pub(crate) const CODE: &str = "code";
/// Document collections are stored as `docs:<name>`.
const DOCS_PREFIX: &str = "docs:";

const MAX_FILE_BYTES: u64 = 1 << 20;
/// Lines per chunk.
const WINDOW: usize = 60;
/// Lines a chunk shares with the one before it.
const OVERLAP: usize = 10;
/// How far a chunk's end may move to land on a blank line.
const SLACK: usize = 10;
/// Files written per transaction.
const BATCH_FILES: usize = 64;
const LOCK_FILES: [&str; 3] = ["package-lock.json", "pnpm-lock.yaml", "bun.lockb"];
/// Full-text hits looked at when a path glob has to be applied to them.
const GLOB_SCAN: i64 = 5000;

/// One indexed chunk found by a search.
pub(crate) struct ChunkHit {
    pub(crate) path: String,
    /// First and last line, 1-based.
    pub(crate) start: i64,
    pub(crate) end: i64,
    pub(crate) text: String,
}

/// Something to walk: a directory or file, or a single file below a directory whose ignore rules
/// decide whether it belongs to the index.
struct Root {
    dir: PathBuf,
    only: Option<PathBuf>,
}

impl Root {
    /// Whether the indexed path `path` lies in what this root walks.
    fn covers(&self, project: &Path, path: &str) -> bool {
        let scope = key(project, self.only.as_deref().unwrap_or(&self.dir));
        scope.is_empty()
            || path == scope
            || path
                .strip_prefix(&scope)
                .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// What the index knows about a file.
struct Known {
    mtime: i64,
    size: i64,
    hash: String,
}

struct Chunk {
    start: i64,
    end: i64,
    text: String,
}

/// A file whose index entry has to be written. `chunks` is `None` when only its timestamp or size
/// changed and the content is the one already indexed.
struct Changed {
    path: String,
    mtime: i64,
    size: i64,
    hash: String,
    chunks: Option<Vec<Chunk>>,
}

enum Found {
    /// Not something the index holds: a lock file, too large, binary or unreadable.
    Skip,
    /// Indexed and unchanged.
    Same(String),
    Changed(Changed),
}

/// The path under which a file is indexed: relative to the project when inside it, with `/`.
fn key(project: &Path, path: &Path) -> String {
    path.strip_prefix(project)
        .unwrap_or(path)
        .to_string_lossy()
        .replace('\\', "/")
}

/// The chunks of a file with `lines.len()` lines, as 1-based inclusive line ranges. A chunk is 60
/// lines and starts 10 lines before the end of the previous one; its end moves to the nearest blank
/// line when one lies within 10 lines.
pub(crate) fn chunk_ranges(lines: &[&str]) -> Vec<(usize, usize)> {
    let total = lines.len();
    let blank = |line: usize| lines[line - 1].trim().is_empty();
    let mut ranges = Vec::new();
    let mut start = 1;
    while start <= total {
        let nominal = start + WINDOW - 1;
        if nominal >= total {
            ranges.push((start, total));
            break;
        }
        let end = (0..=SLACK)
            .flat_map(|distance| [nominal - distance, nominal + distance])
            .find(|&line| line < total && blank(line))
            .unwrap_or(nominal);
        ranges.push((start, end));
        start = end + 1 - OVERLAP;
    }
    ranges
}

fn chunks_of(text: &str) -> Vec<Chunk> {
    let lines: Vec<&str> = text.lines().collect();
    chunk_ranges(&lines)
        .into_iter()
        .filter_map(|(start, end)| {
            let body = lines[start - 1..end].join("\n");
            (!body.trim().is_empty()).then_some(Chunk {
                start: start as i64,
                end: end as i64,
                text: body,
            })
        })
        .collect()
}

fn inspect(project: &Path, path: &Path, known: &HashMap<String, Known>) -> Found {
    let name = path.file_name().map(|n| n.to_string_lossy()).unwrap_or_default();
    let name: &str = &name;
    if name.ends_with(".lock") || LOCK_FILES.contains(&name) {
        return Found::Skip;
    }
    let Ok(meta) = std::fs::metadata(path) else {
        return Found::Skip;
    };
    if meta.len() > MAX_FILE_BYTES {
        return Found::Skip;
    }
    let stored = key(project, path);
    let size = meta.len() as i64;
    let mtime = meta
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |since| since.as_millis() as i64);
    let before = known.get(&stored);
    if before.is_some_and(|k| k.mtime == mtime && k.size == size) {
        return Found::Same(stored);
    }
    let Ok(bytes) = std::fs::read(path) else {
        return Found::Skip;
    };
    if bytes.iter().take(8192).any(|byte| *byte == 0) {
        return Found::Skip;
    }
    let hash = blake3::hash(&bytes).to_hex().to_string();
    let chunks = if before.is_some_and(|k| k.hash == hash) {
        None
    } else {
        Some(chunks_of(&String::from_utf8_lossy(&bytes)))
    };
    Found::Changed(Changed {
        path: stored,
        mtime,
        size,
        hash,
        chunks,
    })
}

/// Walks `roots` honouring ignore files, sends what has to be written in batches, and returns the
/// indexed paths below the roots that are no longer there.
fn scan(
    project: &Path,
    roots: &[Root],
    known: &HashMap<String, Known>,
    out: &mpsc::Sender<Vec<Changed>>,
) -> Vec<String> {
    let mut present: HashSet<String> = HashSet::new();
    let mut batch = Vec::new();
    for root in roots {
        let mut walk = WalkBuilder::new(&root.dir);
        walk.require_git(false);
        if let Some(only) = root.only.clone() {
            walk.filter_entry(move |entry| only.starts_with(entry.path()));
        }
        for entry in walk.build().flatten() {
            if !entry.file_type().is_some_and(|kind| kind.is_file()) {
                continue;
            }
            match inspect(project, entry.path(), known) {
                Found::Skip => {}
                Found::Same(path) => {
                    present.insert(path);
                }
                Found::Changed(changed) => {
                    present.insert(changed.path.clone());
                    batch.push(changed);
                    if batch.len() == BATCH_FILES && out.blocking_send(std::mem::take(&mut batch)).is_err() {
                        return Vec::new();
                    }
                }
            }
        }
    }
    if !batch.is_empty() && out.blocking_send(batch).is_err() {
        return Vec::new();
    }
    known
        .keys()
        .filter(|path| !present.contains(*path) && roots.iter().any(|root| root.covers(project, path)))
        .cloned()
        .collect()
}

fn write(conn: &mut Connection, collection: &str, batch: &[Changed]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    {
        let mut drop_chunks = tx.prepare_cached("DELETE FROM chunks WHERE collection = ?1 AND path = ?2")?;
        let mut add_chunk = tx.prepare_cached(
            "INSERT INTO chunks (collection, path, start_line, end_line, text) VALUES (?1, ?2, ?3, ?4, ?5)",
        )?;
        let mut record = tx.prepare_cached(
            "INSERT INTO files (collection, path, mtime, size, hash) VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT (collection, path)
             DO UPDATE SET mtime = excluded.mtime, size = excluded.size, hash = excluded.hash",
        )?;
        for file in batch {
            if let Some(chunks) = &file.chunks {
                drop_chunks.execute(params![collection, file.path])?;
                for chunk in chunks {
                    add_chunk.execute(params![collection, file.path, chunk.start, chunk.end, chunk.text])?;
                }
            }
            record.execute(params![collection, file.path, file.mtime, file.size, file.hash])?;
        }
    }
    tx.commit()
}

fn remove(conn: &mut Connection, collection: &str, paths: &[String]) -> rusqlite::Result<()> {
    let tx = conn.transaction()?;
    for path in paths {
        tx.execute(
            "DELETE FROM chunks WHERE collection = ?1 AND path = ?2",
            params![collection, path],
        )?;
        tx.execute(
            "DELETE FROM files WHERE collection = ?1 AND path = ?2",
            params![collection, path],
        )?;
    }
    tx.commit()
}

/// Empties `collection`; returns how many files it held.
fn clear(conn: &mut Connection, collection: &str) -> rusqlite::Result<usize> {
    let tx = conn.transaction()?;
    tx.execute("DELETE FROM chunks WHERE collection = ?1", params![collection])?;
    let files = tx.execute("DELETE FROM files WHERE collection = ?1", params![collection])?;
    tx.commit()?;
    Ok(files)
}

/// `docs:<name>` for a collection given by name, with or without the prefix.
fn docs_collection(name: &str) -> Result<String, String> {
    let name = name.strip_prefix(DOCS_PREFIX).unwrap_or(name);
    let valid = !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'));
    if !valid {
        return Err(format!(
            "`{name}` is not a collection name: use letters, digits, `-`, `_` and `.`"
        ));
    }
    Ok(format!("{DOCS_PREFIX}{name}"))
}

/// The stored name of the collection a search names: `code`, `docs:<name>`, or a bare document
/// collection name.
pub(crate) fn search_collection(name: &str) -> String {
    if name == CODE || name.starts_with(DOCS_PREFIX) {
        name.to_string()
    } else {
        format!("{DOCS_PREFIX}{name}")
    }
}

impl MemoryStore {
    /// Brings the project's code in the index up to date and returns `(files, chunks)` now in it.
    /// `full` rebuilds from nothing; otherwise only new and changed files are read, and files that
    /// are gone leave the index.
    pub async fn index_project(&self, full: bool) -> Result<(usize, usize), String> {
        let project = Root {
            dir: self.root.clone(),
            only: None,
        };
        self.refresh(CODE, vec![project], full).await?;
        self.counts(CODE).await
    }

    /// Indexes files and directories into the document collection `name` and returns
    /// `(files, chunks)` now in it. Paths already in the collection are refreshed.
    pub async fn add_documents(&self, name: &str, paths: &[PathBuf]) -> Result<(usize, usize), String> {
        let collection = docs_collection(name)?;
        if paths.is_empty() {
            return Err("give at least one file or directory to index".to_string());
        }
        let mut roots = Vec::new();
        for path in paths {
            let path = canonical(path);
            if !path.exists() {
                return Err(format!("{} does not exist", path.display()));
            }
            roots.push(Root {
                dir: path,
                only: None,
            });
        }
        self.refresh(&collection, roots, false).await?;
        self.counts(&collection).await
    }

    /// Every collection in the index as `(collection, files, chunks)`, by name. The project's code
    /// is `code`; document collections are `docs:<name>`.
    pub async fn collections(&self) -> Result<Vec<(String, usize, usize)>, String> {
        self.index
            .call(|conn| {
                let mut statement = conn.prepare_cached(
                    "SELECT f.collection, COUNT(*),
                            (SELECT COUNT(*) FROM chunks c WHERE c.collection = f.collection)
                     FROM files f GROUP BY f.collection ORDER BY f.collection",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)? as usize,
                        row.get::<_, i64>(2)? as usize,
                    ))
                })?;
                rows.collect()
            })
            .await
    }

    /// Removes the document collection `name`; false when there is none.
    pub async fn remove_collection(&self, name: &str) -> Result<bool, String> {
        let collection = docs_collection(name)?;
        let _busy = self.indexing.lock().await;
        let files = self.index.call(move |conn| clear(conn, &collection)).await?;
        Ok(files > 0)
    }

    /// Re-reads one project file after it changed: indexes it, or drops it when it is gone or
    /// ignored. Files outside the project are not the index's business.
    pub(crate) async fn refresh_file(&self, path: &Path) -> Result<(), String> {
        let path = canonical(path);
        if !path.starts_with(&self.root) {
            return Ok(());
        }
        let file = Root {
            dir: self.root.clone(),
            only: Some(path),
        };
        self.refresh(CODE, vec![file], false).await
    }

    /// The chunks of `collection` that best match `query`, best first, optionally only those whose
    /// path matches `path_glob`.
    pub(crate) async fn semantic_search(
        &self,
        query: &str,
        collection: &str,
        path_glob: Option<&str>,
        limit: usize,
    ) -> Result<Vec<ChunkHit>, String> {
        let glob = match path_glob {
            Some(pattern) => Some(
                Glob::new(pattern)
                    .map_err(|e| format!("invalid path_glob: {e}"))?
                    .compile_matcher(),
            ),
            None => None,
        };
        let terms = fts_query(query);
        let vector = self.embed_one(query).await;
        let model = self.embedder.as_ref().map(|e| e.model().to_string());
        let collection = collection.to_string();
        self.index
            .call(move |conn| {
                let wanted = |path: &str| glob.as_ref().is_none_or(|glob| glob.is_match(path));
                let mut by_terms: Vec<i64> = Vec::new();
                if let Some(terms) = &terms {
                    let mut statement = conn.prepare_cached(
                        "SELECT c.id, c.path FROM chunks_fts JOIN chunks c ON c.id = chunks_fts.rowid
                         WHERE chunks_fts MATCH ?1 AND c.collection = ?2
                         ORDER BY bm25(chunks_fts) LIMIT ?3",
                    )?;
                    let scanned = if glob.is_some() {
                        GLOB_SCAN
                    } else {
                        CANDIDATES as i64
                    };
                    let mut rows = statement.query(params![terms, collection, scanned])?;
                    while let Some(row) = rows.next()? {
                        if wanted(&row.get::<_, String>(1)?) {
                            by_terms.push(row.get(0)?);
                            if by_terms.len() == CANDIDATES {
                                break;
                            }
                        }
                    }
                }
                let mut scored: Vec<(i64, f32)> = Vec::new();
                if let (Some(vector), Some(model)) = (&vector, &model) {
                    let mut statement = conn.prepare_cached(
                        "SELECT id, path, embedding FROM chunks
                         WHERE collection = ?1 AND embedding IS NOT NULL AND embed_model = ?2",
                    )?;
                    let mut rows = statement.query(params![collection, model])?;
                    while let Some(row) = rows.next()? {
                        if !wanted(&row.get::<_, String>(1)?) {
                            continue;
                        }
                        if let Some(score) = dot(vector, &from_blob(&row.get::<_, Vec<u8>>(2)?)) {
                            scored.push((row.get(0)?, score));
                        }
                    }
                }
                let by_vector: Vec<i64> = top(scored).into_iter().map(|(id, _)| id).collect();

                let mut fetch =
                    conn.prepare_cached("SELECT path, start_line, end_line, text FROM chunks WHERE id = ?1")?;
                let mut hits = Vec::new();
                for id in fuse(&[by_terms.as_slice(), by_vector.as_slice()])
                    .into_iter()
                    .take(limit)
                {
                    hits.push(fetch.query_row(params![id], |row| {
                        Ok(ChunkHit {
                            path: row.get(0)?,
                            start: row.get(1)?,
                            end: row.get(2)?,
                            text: row.get(3)?,
                        })
                    })?);
                }
                Ok(hits)
            })
            .await
    }

    /// Walks `roots` and makes `collection` agree with what is on disk below them.
    async fn refresh(&self, collection: &str, roots: Vec<Root>, full: bool) -> Result<(), String> {
        let _busy = self.indexing.lock().await;
        let collection = collection.to_string();
        if full {
            let collection = collection.clone();
            self.index.call(move |conn| clear(conn, &collection)).await?;
        }
        // A single-file refresh needs to know about that file only.
        let only = match roots.as_slice() {
            [Root { only: Some(path), .. }] => Some(key(&self.root, path)),
            _ => None,
        };
        let known = self.known(&collection, only).await?;
        let (batches, mut incoming) = mpsc::channel::<Vec<Changed>>(2);
        let project = self.root.clone();
        let walk = tokio::task::spawn_blocking(move || scan(&project, &roots, &known, &batches));
        while let Some(batch) = incoming.recv().await {
            let collection = collection.clone();
            self.index
                .call(move |conn| write(conn, &collection, &batch))
                .await?;
        }
        let gone = walk.await.map_err(|e| format!("the index walk failed: {e}"))?;
        if !gone.is_empty() {
            self.index
                .call(move |conn| remove(conn, &collection, &gone))
                .await?;
        }
        if let Some(embedder) = &self.embedder
            && let Err(why) = embed_pending(&self.index, embedder, "chunks", "id").await
        {
            tracing::warn!("some chunks are indexed without vectors: {why}");
        }
        Ok(())
    }

    /// What the index holds for `collection`, or for the one path `only`.
    async fn known(&self, collection: &str, only: Option<String>) -> Result<HashMap<String, Known>, String> {
        let collection = collection.to_string();
        self.index
            .call(move |conn| {
                let mut statement = conn.prepare_cached(
                    "SELECT path, mtime, size, hash FROM files
                     WHERE collection = ?1 AND (?2 IS NULL OR path = ?2)",
                )?;
                let rows = statement.query_map(params![collection, only], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        Known {
                            mtime: row.get::<_, Option<i64>>(1)?.unwrap_or(-1),
                            size: row.get::<_, Option<i64>>(2)?.unwrap_or(-1),
                            hash: row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                        },
                    ))
                })?;
                rows.collect()
            })
            .await
    }

    async fn counts(&self, collection: &str) -> Result<(usize, usize), String> {
        let collection = collection.to_string();
        self.index
            .call(move |conn| {
                conn.query_row(
                    "SELECT (SELECT COUNT(*) FROM files WHERE collection = ?1),
                            (SELECT COUNT(*) FROM chunks WHERE collection = ?1)",
                    params![collection],
                    |row| Ok((row.get::<_, i64>(0)? as usize, row.get::<_, i64>(1)? as usize)),
                )
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `total` lines of code with blank lines at the given 1-based positions.
    fn ranges(total: usize, blanks: &[usize]) -> Vec<(usize, usize)> {
        let lines: Vec<&str> = (1..=total)
            .map(|line| if blanks.contains(&line) { "  " } else { "code" })
            .collect();
        chunk_ranges(&lines)
    }

    #[test]
    fn chunks_are_sixty_lines_with_ten_shared() {
        assert!(ranges(0, &[]).is_empty());
        assert_eq!(ranges(60, &[]), [(1, 60)]);
        assert_eq!(ranges(61, &[]), [(1, 60), (51, 61)]);
        assert_eq!(ranges(150, &[]), [(1, 60), (51, 110), (101, 150)]);
    }

    #[test]
    fn a_chunk_ends_on_a_blank_line_within_ten_lines_of_its_end() {
        // Before the nominal end: the chunk is cut short and the next one starts ten lines earlier.
        assert_eq!(ranges(150, &[55]), [(1, 55), (46, 105), (96, 150)]);
        // After it: the chunk grows.
        assert_eq!(ranges(150, &[70]), [(1, 70), (61, 120), (111, 150)]);
        // The nearest blank line wins, the earlier one when two are equally near.
        assert_eq!(ranges(100, &[52, 62])[0], (1, 62));
        assert_eq!(ranges(100, &[58, 62])[0], (1, 58));
        // Eleven lines away is too far on either side.
        assert_eq!(ranges(100, &[49, 71])[0], (1, 60));
    }

    #[test]
    fn blank_stretches_make_no_chunks_and_lines_keep_their_numbers() {
        assert!(chunks_of("\n\n  \n").is_empty());
        let chunks = chunks_of("a\nb\r\nc");
        assert_eq!(chunks.len(), 1);
        assert_eq!((chunks[0].start, chunks[0].end), (1, 3));
        assert_eq!(chunks[0].text, "a\nb\nc");
    }

    #[test]
    fn roots_cover_their_own_files_only() {
        let project = Path::new("/p");
        let whole = Root {
            dir: project.to_path_buf(),
            only: None,
        };
        assert!(whole.covers(project, "src/a.rs"));
        let dir = Root {
            dir: project.join("docs"),
            only: None,
        };
        assert!(dir.covers(project, "docs/guide/a.md"));
        assert!(!dir.covers(project, "docs2/a.md"));
        let file = Root {
            dir: project.to_path_buf(),
            only: Some(project.join("src/a.rs")),
        };
        assert!(file.covers(project, "src/a.rs"));
        assert!(!file.covers(project, "src/a.rs.bak"));
    }

    #[test]
    fn collection_names() {
        assert_eq!(docs_collection("guide").as_deref(), Ok("docs:guide"));
        assert_eq!(docs_collection("docs:guide").as_deref(), Ok("docs:guide"));
        assert!(docs_collection("").is_err());
        assert!(docs_collection("a b").is_err());
        assert_eq!(search_collection("code"), "code");
        assert_eq!(search_collection("guide"), "docs:guide");
        assert_eq!(search_collection("docs:guide"), "docs:guide");
    }
}
