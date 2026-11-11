//! One worker thread per database file. The thread owns the connection and runs the jobs sent to
//! it in order, so SQLite is never touched from an async task.

use std::path::Path;
use std::sync::mpsc;
use std::time::Duration;

use rusqlite::Connection;
use tokio::sync::oneshot;

/// Tables of a memory database (the project's and the global one).
pub(crate) const MEMORY_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS memories (id TEXT PRIMARY KEY, kind TEXT NOT NULL, text TEXT NOT NULL,
  tags TEXT NOT NULL DEFAULT '', created_at TEXT NOT NULL, updated_at TEXT NOT NULL, source_session TEXT,
  superseded_by TEXT, embedding BLOB, embed_model TEXT);
CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
  USING fts5(text, tags, content='memories', content_rowid='rowid');
CREATE TRIGGER IF NOT EXISTS memories_ai AFTER INSERT ON memories BEGIN
  INSERT INTO memories_fts(rowid, text, tags) VALUES (new.rowid, new.text, new.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_ad AFTER DELETE ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, text, tags) VALUES ('delete', old.rowid, old.text, old.tags);
END;
CREATE TRIGGER IF NOT EXISTS memories_au AFTER UPDATE OF text, tags ON memories BEGIN
  INSERT INTO memories_fts(memories_fts, rowid, text, tags) VALUES ('delete', old.rowid, old.text, old.tags);
  INSERT INTO memories_fts(rowid, text, tags) VALUES (new.rowid, new.text, new.tags);
END;
";

/// Tables of the search index.
pub(crate) const INDEX_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS files (collection TEXT NOT NULL, path TEXT NOT NULL, mtime INTEGER, size INTEGER,
  hash TEXT, PRIMARY KEY (collection, path));
CREATE TABLE IF NOT EXISTS chunks (id INTEGER PRIMARY KEY, collection TEXT NOT NULL, path TEXT NOT NULL,
  start_line INTEGER, end_line INTEGER, text TEXT NOT NULL, embedding BLOB, embed_model TEXT);
CREATE INDEX IF NOT EXISTS chunks_by_path ON chunks (collection, path);
CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(text, path, content='chunks', content_rowid='id');
CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE OF text, path ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, text, path) VALUES ('delete', old.id, old.text, old.path);
  INSERT INTO chunks_fts(rowid, text, path) VALUES (new.id, new.text, new.path);
END;
";

const CLOSED: &str = "the memory database worker has stopped";

type Job = Box<dyn FnOnce(&mut Connection) + Send>;

/// A database reached through its worker thread. The thread ends when the handle is dropped.
pub(crate) struct Db {
    jobs: mpsc::Sender<Job>,
}

impl Db {
    /// Opens `path`, creating the file, its directory and the tables of `schema` when missing.
    /// Several processes may have the same file open; writers wait for each other.
    pub(crate) fn open(path: &Path, schema: &str) -> Result<Db, String> {
        let fail = |e: &dyn std::fmt::Display| format!("{}: {e}", path.display());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        let mut conn = Connection::open(path).map_err(|e| fail(&e))?;
        conn.busy_timeout(Duration::from_secs(10)).map_err(|e| fail(&e))?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA synchronous = NORMAL;")
            .map_err(|e| fail(&e))?;
        conn.execute_batch(schema).map_err(|e| fail(&e))?;
        let (jobs, queue) = mpsc::channel::<Job>();
        std::thread::Builder::new()
            .name("azazel-memory-db".to_string())
            .spawn(move || {
                for job in queue {
                    job(&mut conn);
                }
            })
            .map_err(|e| fail(&e))?;
        Ok(Db { jobs })
    }

    /// Runs `f` on the worker thread and returns its result.
    pub(crate) async fn call<T, F>(&self, f: F) -> Result<T, String>
    where
        F: FnOnce(&mut Connection) -> rusqlite::Result<T> + Send + 'static,
        T: Send + 'static,
    {
        let (reply, result) = oneshot::channel();
        let job: Job = Box::new(move |conn: &mut Connection| {
            // The caller may have stopped waiting; the work is done either way.
            let _ = reply.send(f(conn));
        });
        self.jobs.send(job).map_err(|_| CLOSED.to_string())?;
        match result.await {
            Ok(result) => result.map_err(|e| e.to_string()),
            Err(_) => Err(CLOSED.to_string()),
        }
    }
}
