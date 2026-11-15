//! The memory stores of one project: its own, the global one shared by all projects, and the handle
//! on the search index.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use azazel_ai::Credentials;
use azazel_core::{Config, Paths};
use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::json;

use crate::db::{Db, INDEX_SCHEMA, MEMORY_SCHEMA};
use crate::embed::Embedder;
use crate::search::{CANDIDATES, dot, from_blob, fts_query, fuse, to_blob, top};

/// What a memory can be.
pub const KINDS: [&str; 5] = ["fact", "decision", "preference", "convention", "gotcha"];

/// Characters the `memory` system part may hold: 1200 tokens at four characters each.
const PINNED_CHARS: usize = 4800;
/// Rows embedded per request when catching up.
const EMBED_BATCH: i64 = 64;
/// Cosine similarity from which a new memory counts as one already stored.
pub(crate) const DUPLICATE: f32 = 0.92;
/// Memories compared with a new one when looking for one that says the same.
const SIMILAR_CANDIDATES: usize = 8;
/// Times an extraction left for a later session is started before it is let go.
const EXTRACTION_ATTEMPTS: i64 = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Scope {
    /// Kept with this project's data.
    Project,
    /// Shared by every project of this user.
    Global,
}

impl Scope {
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }

    /// `project` or `global`.
    pub fn parse(name: &str) -> Option<Scope> {
        match name {
            "project" => Some(Scope::Project),
            "global" => Some(Scope::Global),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRow {
    pub id: String,
    /// One of [`KINDS`].
    pub kind: String,
    pub text: String,
    pub tags: Vec<String>,
    pub scope: Scope,
    /// RFC 3339, UTC.
    pub created_at: String,
}

/// Memories and the search index of one project.
pub struct MemoryStore {
    project: Db,
    global: Db,
    pub(crate) index: Db,
    /// The project root; indexed paths are relative to it.
    pub(crate) root: PathBuf,
    pub(crate) embedder: Option<Embedder>,
    /// Held by whoever is changing the index, so refreshes never interleave.
    pub(crate) indexing: tokio::sync::Mutex<()>,
}

/// UTC with a fixed number of digits, so that timestamps order as text.
fn now() -> String {
    let now = jiff::Timestamp::now();
    format!(
        "{}.{:06}Z",
        now.strftime("%Y-%m-%dT%H:%M:%S"),
        now.subsec_microsecond().max(0)
    )
}

/// Twelve hex digits from the random part of a version 7 UUID.
fn new_id() -> String {
    uuid::Uuid::now_v7().simple().to_string()[20..].to_string()
}

fn join_tags(tags: &[String]) -> String {
    let clean: Vec<String> = tags
        .iter()
        .map(|tag| {
            tag.replace(',', " ")
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ")
        })
        .filter(|tag| !tag.is_empty())
        .collect();
    clean.join(",")
}

fn split_tags(tags: &str) -> Vec<String> {
    tags.split(',')
        .filter(|tag| !tag.is_empty())
        .map(str::to_string)
        .collect()
}

/// Lower case with runs of whitespace collapsed: the form in which two memories count as the same.
pub(crate) fn normalized(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// The words of `text`, lower case.
fn words(text: &str) -> HashSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(str::to_lowercase)
        .collect()
}

/// Whether two memories say the same thing in other words: they share the same words, or at least
/// 70% of all the words either uses. A text of fewer than four words only matches by its words.
pub(crate) fn similar(a: &str, b: &str) -> bool {
    let (a, b) = (words(a), words(b));
    if a == b {
        return !a.is_empty();
    }
    if a.len().min(b.len()) < 4 {
        return false;
    }
    let common = a.intersection(&b).count();
    common * 10 >= (a.len() + b.len() - common) * 7
}

/// `old` and `new` tags without repeats (ignoring case), the old ones first.
fn merged_tags(old: &[String], new: &[String]) -> Vec<String> {
    let mut seen = HashSet::new();
    old.iter()
        .chain(new)
        .filter(|tag| seen.insert(tag.to_lowercase()))
        .cloned()
        .collect()
}

/// What [`MemoryStore::remember`] did with a memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Remembered {
    /// Added as a new memory with this id.
    Stored(String),
    /// An existing memory said the same: it took the newer wording and the tags, and its time moved on.
    Updated(String),
    /// A memory in force already says it.
    Known,
}

/// Marks the store as cleaned of duplicates by [`MemoryStore::collapse_duplicates`].
const COLLAPSED: &str = "duplicates-collapsed";

/// Supersedes every memory in force that says what a newer one says, once per database: the
/// marker is set in the same transaction. Returns how many were merged.
fn collapse(conn: &mut Connection) -> rusqlite::Result<usize> {
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let done: bool = tx.query_row(
        "SELECT EXISTS (SELECT 1 FROM meta WHERE key = ?1)",
        params![COLLAPSED],
        |row| row.get(0),
    )?;
    if done {
        return Ok(0);
    }
    let rows: Vec<(String, String, String)> = {
        let mut statement = tx.prepare(
            "SELECT id, text, tags FROM memories WHERE superseded_by IS NULL
             ORDER BY updated_at DESC, rowid DESC",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    // Newest first, so the wording that is kept is the newest.
    let mut kept: Vec<(String, String, Vec<String>, bool)> = Vec::new();
    let mut merged = 0;
    let at = now();
    for (id, text, tags) in rows {
        let tags = split_tags(&tags);
        match kept.iter_mut().find(|(_, other, _, _)| similar(other, &text)) {
            Some((kept_id, _, kept_tags, changed)) => {
                let all = merged_tags(kept_tags, &tags);
                *changed |= all.len() != kept_tags.len();
                *kept_tags = all;
                tx.execute(
                    "UPDATE memories SET superseded_by = ?2, updated_at = ?3 WHERE id = ?1",
                    params![id, &*kept_id, at],
                )?;
                merged += 1;
            }
            None => kept.push((id, text, tags, false)),
        }
    }
    for (id, _, tags, _) in kept.iter().filter(|kept| kept.3) {
        tx.execute(
            "UPDATE memories SET tags = ?2 WHERE id = ?1",
            params![id, join_tags(tags)],
        )?;
    }
    tx.execute("INSERT INTO meta (key) VALUES (?1)", params![COLLAPSED])?;
    tx.commit()?;
    Ok(merged)
}

/// Reads `id, kind, text, tags, created_at` from the first five columns.
fn row_at(row: &rusqlite::Row<'_>, scope: Scope) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        id: row.get(0)?,
        kind: row.get(1)?,
        text: row.get(2)?,
        tags: split_tags(&row.get::<_, String>(3)?),
        scope,
        created_at: row.get(4)?,
    })
}

fn lexical(conn: &Connection, scope: Scope, query: &str) -> rusqlite::Result<Vec<(MemoryRow, f64)>> {
    let mut statement = conn.prepare_cached(
        "SELECT m.id, m.kind, m.text, m.tags, m.created_at, bm25(memories_fts)
         FROM memories_fts JOIN memories m ON m.rowid = memories_fts.rowid
         WHERE memories_fts MATCH ?1 AND m.superseded_by IS NULL
         ORDER BY bm25(memories_fts) LIMIT ?2",
    )?;
    let rows = statement.query_map(params![query, CANDIDATES as i64], |row| {
        Ok((row_at(row, scope)?, row.get::<_, f64>(5)?))
    })?;
    rows.collect()
}

fn nearest(
    conn: &Connection,
    scope: Scope,
    query: &[f32],
    model: &str,
) -> rusqlite::Result<Vec<(MemoryRow, f32)>> {
    let mut statement = conn.prepare_cached(
        "SELECT id, kind, text, tags, created_at, embedding FROM memories
         WHERE superseded_by IS NULL AND embedding IS NOT NULL AND embed_model = ?1",
    )?;
    let rows = statement.query_map(params![model], |row| {
        Ok((row_at(row, scope)?, row.get::<_, Vec<u8>>(5)?))
    })?;
    let mut scored = Vec::new();
    for row in rows {
        let (memory, blob) = row?;
        if let Some(score) = dot(query, &from_blob(&blob)) {
            scored.push((memory, score));
        }
    }
    Ok(top(scored))
}

/// Embeds the rows of `table` that have no vector from the configured model, a batch at a time.
/// `key` is the table's integer row id column.
pub(crate) async fn embed_pending(
    db: &Db,
    embedder: &Embedder,
    table: &'static str,
    key: &'static str,
) -> Result<(), String> {
    loop {
        let model = embedder.model().to_string();
        let rows: Vec<(i64, String)> = db
            .call(move |conn| {
                let mut statement = conn.prepare_cached(&format!(
                    "SELECT {key}, text FROM {table}
                     WHERE embedding IS NULL OR embed_model IS NOT ?1 LIMIT ?2"
                ))?;
                let rows = statement.query_map(params![model, EMBED_BATCH], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect()
            })
            .await?;
        if rows.is_empty() {
            return Ok(());
        }
        let texts: Vec<String> = rows.iter().map(|(_, text)| text.clone()).collect();
        let vectors = embedder.embed(&texts).await?;
        let model = embedder.model().to_string();
        db.call(move |conn| {
            let tx = conn.transaction()?;
            {
                let mut update = tx.prepare_cached(&format!(
                    "UPDATE {table} SET embedding = ?1, embed_model = ?2 WHERE {key} = ?3"
                ))?;
                for ((id, _), vector) in rows.iter().zip(&vectors) {
                    update.execute(params![to_blob(vector), model, id])?;
                }
            }
            tx.commit()
        })
        .await?;
    }
}

impl MemoryStore {
    pub(crate) fn open(
        paths: &Paths,
        config: &Config,
        credentials: Arc<Credentials>,
    ) -> Result<MemoryStore, String> {
        Ok(MemoryStore {
            embedder: Embedder::from_config(config, credentials)?,
            project: Db::open(&paths.memory_db(), MEMORY_SCHEMA)?,
            global: Db::open(&paths.global_memory_db(), MEMORY_SCHEMA)?,
            index: Db::open(&paths.index_db(), INDEX_SCHEMA)?,
            root: paths.project_root.clone(),
            indexing: tokio::sync::Mutex::new(()),
        })
    }

    fn db(&self, scope: Scope) -> &Db {
        match scope {
            Scope::Project => &self.project,
            Scope::Global => &self.global,
        }
    }

    /// The memories of `scope` that have not been superseded, newest first.
    pub async fn list(&self, scope: Scope) -> Result<Vec<MemoryRow>, String> {
        self.db(scope)
            .call(move |conn| {
                let mut statement = conn.prepare_cached(
                    "SELECT id, kind, text, tags, created_at FROM memories
                     WHERE superseded_by IS NULL ORDER BY created_at DESC, rowid DESC",
                )?;
                let rows = statement.query_map([], |row| row_at(row, scope))?;
                rows.collect()
            })
            .await
    }

    /// The memories that best match `query`, best first, from `scope` or from both stores. Full-text
    /// and, when embeddings are configured, vector rankings are fused; superseded memories never match.
    pub async fn search(
        &self,
        query: &str,
        limit: usize,
        scope: Option<Scope>,
    ) -> Result<Vec<MemoryRow>, String> {
        let terms = fts_query(query);
        let vector = self.embed_one(query).await;
        if limit == 0 || (terms.is_none() && vector.is_none()) {
            return Ok(Vec::new());
        }
        let model = self.embedder.as_ref().map(|e| e.model().to_string());
        let mut by_terms: Vec<(MemoryRow, f64)> = Vec::new();
        let mut by_vector: Vec<(MemoryRow, f32)> = Vec::new();
        for scope in scope.map_or(vec![Scope::Project, Scope::Global], |s| vec![s]) {
            let (terms, vector, model) = (terms.clone(), vector.clone(), model.clone());
            let (mut matched, mut near) = self
                .db(scope)
                .call(move |conn| {
                    let matched = match &terms {
                        Some(terms) => lexical(conn, scope, terms)?,
                        None => Vec::new(),
                    };
                    let near = match (&vector, &model) {
                        (Some(vector), Some(model)) => nearest(conn, scope, vector, model)?,
                        _ => Vec::new(),
                    };
                    Ok((matched, near))
                })
                .await?;
            by_terms.append(&mut matched);
            by_vector.append(&mut near);
        }
        // bm25 is smaller for better matches.
        by_terms.sort_by(|a, b| a.1.total_cmp(&b.1));
        by_terms.truncate(CANDIDATES);
        let by_vector = top(by_vector);

        let term_ids: Vec<&str> = by_terms.iter().map(|(row, _)| row.id.as_str()).collect();
        let vector_ids: Vec<&str> = by_vector.iter().map(|(row, _)| row.id.as_str()).collect();
        let order = fuse(&[term_ids.as_slice(), vector_ids.as_slice()]);
        let mut rows: HashMap<&str, &MemoryRow> = HashMap::new();
        for row in by_terms
            .iter()
            .map(|(row, _)| row)
            .chain(by_vector.iter().map(|(row, _)| row))
        {
            rows.entry(row.id.as_str()).or_insert(row);
        }
        Ok(order
            .into_iter()
            .take(limit)
            .filter_map(|id| rows.get(id).copied().cloned())
            .collect())
    }

    /// Stores a memory and returns its id. `kind` must be one of [`KINDS`]; `session` is the session
    /// it was learned in.
    pub async fn save(
        &self,
        text: &str,
        kind: &str,
        tags: &[String],
        scope: Scope,
        session: Option<&str>,
    ) -> Result<String, String> {
        let text = checked(text, kind)?;
        let vector = self.embed_one(text).await;
        self.insert(text, kind, tags, scope, session, vector).await
    }

    /// Stores memories that were learned in another program, each as `(text, kind)`, all with
    /// `tags`. One whose text a memory in force in `scope` already has is left out. Returns how
    /// many were stored and how many were there already. They get their vectors when memories
    /// are next caught up with the embedding model.
    pub async fn import(
        &self,
        memories: &[(String, &str)],
        tags: &[String],
        scope: Scope,
    ) -> Result<(usize, usize), String> {
        let mut known: std::collections::HashSet<String> = self
            .list(scope)
            .await?
            .iter()
            .map(|row| normalized(&row.text))
            .collect();
        let (mut stored, mut present) = (0, 0);
        for (text, kind) in memories {
            let text = checked(text, kind)?;
            if !known.insert(normalized(text)) {
                present += 1;
                continue;
            }
            self.insert(text, kind, tags, scope, None, None).await?;
            stored += 1;
        }
        Ok((stored, present))
    }

    /// The memory in force in `scope` that says what `text` says, if any: the full-text and vector
    /// search find the candidates, [`similar`] decides.
    async fn similar_in(&self, scope: Scope, text: &str) -> Result<Option<MemoryRow>, String> {
        Ok(self
            .search(text, SIMILAR_CANDIDATES, Some(scope))
            .await?
            .into_iter()
            .find(|row| similar(&row.text, text)))
    }

    /// Stores what an extraction learned in the project store, unless it is already there: a
    /// memory that says the same takes the newer wording, the merged tags and a new time instead
    /// of a second row. `text` and `kind` must have been checked.
    pub(crate) async fn remember(
        &self,
        text: &str,
        kind: &str,
        tags: &[String],
        session: Option<&str>,
    ) -> Result<Remembered, String> {
        if self.similar_in(Scope::Global, text).await?.is_some() {
            return Ok(Remembered::Known);
        }
        let vector = self.embed_one(text).await;
        if let Some(old) = self.similar_in(Scope::Project, text).await? {
            let reworded = normalized(&old.text) != normalized(text);
            let tags = merged_tags(&old.tags, tags);
            let (id, text) = (old.id.clone(), text.to_string());
            let (blob, model) = match (&vector, &self.embedder) {
                (Some(vector), Some(embedder)) => (Some(to_blob(vector)), Some(embedder.model().to_string())),
                _ => (None, None),
            };
            let at = now();
            self.project
                .call(move |conn| {
                    if reworded {
                        conn.execute(
                            "UPDATE memories SET text = ?2, tags = ?3, updated_at = ?4, embedding = ?5, embed_model = ?6
                             WHERE id = ?1",
                            params![id, text, join_tags(&tags), at, blob, model],
                        )
                    } else {
                        conn.execute(
                            "UPDATE memories SET tags = ?2, updated_at = ?3 WHERE id = ?1",
                            params![id, join_tags(&tags), at],
                        )
                    }
                })
                .await?;
            return Ok(Remembered::Updated(old.id));
        }
        if let Some(vector) = &vector
            && self
                .closest(vector)
                .await?
                .is_some_and(|similarity| similarity >= DUPLICATE)
        {
            return Ok(Remembered::Known);
        }
        let id = self
            .insert(text, kind, tags, Scope::Project, session, vector)
            .await?;
        Ok(Remembered::Stored(id))
    }

    /// Merges the memories in force that say the same thing in both stores, keeping the newest
    /// wording and the tags of all; the others are marked superseded by it (`azazel memory export`
    /// still shows them). Done once per store, the first time it is called after this version came;
    /// returns how many were merged, 0 on every later call. A memory without a duplicate is never
    /// touched.
    pub async fn collapse_duplicates(&self) -> Result<usize, String> {
        let mut merged = 0;
        for scope in [Scope::Project, Scope::Global] {
            merged += self.db(scope).call(collapse).await?;
        }
        Ok(merged)
    }

    /// Deletes the memory `id` from whichever store holds it; false when neither does.
    pub async fn forget(&self, id: &str) -> Result<bool, String> {
        let mut found = false;
        for scope in [Scope::Project, Scope::Global] {
            let id = id.to_string();
            found |= self
                .db(scope)
                .call(move |conn| conn.execute("DELETE FROM memories WHERE id = ?1", params![id]))
                .await?
                > 0;
        }
        Ok(found)
    }

    /// Puts the conversation in the session file `session` in line for extraction, which a later
    /// session of this project does in the background.
    pub(crate) async fn queue_extraction(&self, session: &Path) -> Result<(), String> {
        let session = session.to_string_lossy().into_owned();
        self.project
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO pending_extractions (session) VALUES (?1)
                     ON CONFLICT (session) DO UPDATE SET attempts = 0",
                    params![session],
                )
            })
            .await
            .map(drop)
    }

    /// The session files in line for extraction, each counted as tried once more. One that was
    /// tried [`EXTRACTION_ATTEMPTS`] times already is let go: a process that ends before an
    /// extraction finishes does not have every later one pay for it again.
    pub(crate) async fn take_extractions(&self) -> Result<Vec<PathBuf>, String> {
        self.project
            .call(|conn| {
                let tx = conn.transaction()?;
                tx.execute(
                    "DELETE FROM pending_extractions WHERE attempts >= ?1",
                    params![EXTRACTION_ATTEMPTS],
                )?;
                tx.execute("UPDATE pending_extractions SET attempts = attempts + 1", [])?;
                let sessions: Vec<String> = {
                    let mut statement =
                        tx.prepare("SELECT session FROM pending_extractions ORDER BY rowid")?;
                    let rows = statement.query_map([], |row| row.get(0))?;
                    rows.collect::<rusqlite::Result<_>>()?
                };
                tx.commit()?;
                Ok(sessions.into_iter().map(PathBuf::from).collect())
            })
            .await
    }

    /// Takes `session` out of the line: it was extracted, or there is nothing left to extract.
    pub(crate) async fn extracted(&self, session: &Path) -> Result<(), String> {
        let session = session.to_string_lossy().into_owned();
        self.project
            .call(move |conn| {
                conn.execute(
                    "DELETE FROM pending_extractions WHERE session = ?1",
                    params![session],
                )
            })
            .await
            .map(drop)
    }

    /// Every memory of both stores, superseded ones included, as JSON lines, oldest first.
    pub async fn export(&self) -> Result<String, String> {
        let mut out = String::new();
        for scope in [Scope::Project, Scope::Global] {
            let lines: Vec<String> = self
                .db(scope)
                .call(move |conn| {
                    let mut statement = conn.prepare_cached(
                        "SELECT id, kind, text, tags, created_at, updated_at, source_session, superseded_by
                         FROM memories ORDER BY created_at, rowid",
                    )?;
                    let rows = statement.query_map([], |row| {
                        let memory = row_at(row, scope)?;
                        Ok(json!({
                            "id": memory.id,
                            "scope": scope.as_str(),
                            "kind": memory.kind,
                            "text": memory.text,
                            "tags": memory.tags,
                            "created_at": memory.created_at,
                            "updated_at": row.get::<_, String>(5)?,
                            "source_session": row.get::<_, Option<String>>(6)?,
                            "superseded_by": row.get::<_, Option<String>>(7)?,
                        })
                        .to_string())
                    })?;
                    rows.collect()
                })
                .await?;
            for line in lines {
                out.push_str(&line);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Inserts a memory whose text and kind have been checked, with the vector of its text if there
    /// is one.
    pub(crate) async fn insert(
        &self,
        text: &str,
        kind: &str,
        tags: &[String],
        scope: Scope,
        session: Option<&str>,
        vector: Option<Vec<f32>>,
    ) -> Result<String, String> {
        let id = new_id();
        let model = self.embedder.as_ref().map(|e| e.model().to_string());
        let (blob, model) = match (vector, model) {
            (Some(vector), Some(model)) => (Some(to_blob(&vector)), Some(model)),
            _ => (None, None),
        };
        let row = (
            id.clone(),
            kind.to_string(),
            text.to_string(),
            join_tags(tags),
            now(),
            session.map(str::to_string),
        );
        self.db(scope)
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO memories
                       (id, kind, text, tags, created_at, updated_at, source_session, embedding, embed_model)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6, ?7, ?8)",
                    params![row.0, row.1, row.2, row.3, row.4, row.5, blob, model],
                )
            })
            .await?;
        Ok(id)
    }

    /// Marks `old` as replaced by `new`; false when `old` is unknown or already superseded.
    pub(crate) async fn supersede(&self, old: &str, new: &str) -> Result<bool, String> {
        let mut found = false;
        for scope in [Scope::Project, Scope::Global] {
            let (old, new, at) = (old.to_string(), new.to_string(), now());
            found |= self
                .db(scope)
                .call(move |conn| {
                    conn.execute(
                        "UPDATE memories SET superseded_by = ?2, updated_at = ?3
                         WHERE id = ?1 AND superseded_by IS NULL",
                        params![old, new, at],
                    )
                })
                .await?
                > 0;
        }
        Ok(found)
    }

    /// The preferences and conventions in force, newest first, as many as fit the system part.
    pub(crate) async fn pinned(&self) -> Result<Vec<MemoryRow>, String> {
        let mut rows = self.list(Scope::Project).await?;
        rows.append(&mut self.list(Scope::Global).await?);
        rows.retain(|row| row.kind == "preference" || row.kind == "convention");
        rows.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        let mut used = 0;
        let fits = rows
            .iter()
            .take_while(|row| {
                used += row.text.len() + row.kind.len() + 6;
                used <= PINNED_CHARS
            })
            .count();
        rows.truncate(fits);
        Ok(rows)
    }

    /// The unit vector of `text`; `None` without embeddings or when the request fails, in which case
    /// the caller goes on with full-text search alone.
    pub(crate) async fn embed_one(&self, text: &str) -> Option<Vec<f32>> {
        let embedder = self.embedder.as_ref()?;
        match embedder.embed(&[text.to_string()]).await {
            Ok(mut vectors) => vectors.pop(),
            Err(why) => {
                tracing::warn!("{why}");
                None
            }
        }
    }

    /// The highest cosine similarity between `vector` and a memory in force; `None` when no memory
    /// has a vector from the configured model.
    pub(crate) async fn closest(&self, vector: &[f32]) -> Result<Option<f32>, String> {
        let Some(model) = self.embedder.as_ref().map(|e| e.model().to_string()) else {
            return Ok(None);
        };
        let mut best: Option<f32> = None;
        for scope in [Scope::Project, Scope::Global] {
            let (vector, model) = (vector.to_vec(), model.clone());
            let near = self
                .db(scope)
                .call(move |conn| nearest(conn, scope, &vector, &model))
                .await?;
            if let Some((_, score)) = near.first() {
                best = Some(best.map_or(*score, |b| b.max(*score)));
            }
        }
        Ok(best)
    }

    /// Gives every memory a vector from the configured model. Does nothing without embeddings.
    pub(crate) async fn embed_memories(&self) -> Result<(), String> {
        let Some(embedder) = &self.embedder else {
            return Ok(());
        };
        for scope in [Scope::Project, Scope::Global] {
            embed_pending(self.db(scope), embedder, "memories", "rowid").await?;
        }
        Ok(())
    }
}

/// The trimmed text when it and `kind` make a valid memory.
pub(crate) fn checked<'a>(text: &'a str, kind: &str) -> Result<&'a str, String> {
    let text = text.trim();
    if text.is_empty() {
        return Err("a memory needs text".to_string());
    }
    if !KINDS.contains(&kind) {
        return Err(format!("unknown kind `{kind}`: use one of {}", KINDS.join(", ")));
    }
    Ok(text)
}
