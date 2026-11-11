//! Project memory and retrieval: the SQLite memory store, the search index, the embeddings client and
//! the memory tools.

mod db;
mod embed;
mod ext;
mod extract;
mod index;
mod search;
mod store;
mod tools;

pub use ext::MemoryExt;
pub use store::{KINDS, MemoryRow, MemoryStore, Scope};
