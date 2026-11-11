//! The seam through which extensions and lower layers reach project memory without depending on
//! the crate that stores it.

use std::sync::Arc;

use async_trait::async_trait;

pub const UNAVAILABLE: &str = "memory unavailable";

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub id: String,
    /// `fact`, `decision`, `preference`, `convention` or `gotcha`.
    pub kind: String,
    pub text: String,
    pub tags: Vec<String>,
    /// `project` or `global`.
    pub scope: String,
}

#[async_trait]
pub trait MemoryApi: Send + Sync {
    /// Stores a memory and returns its id. `global` puts it in the store shared by all projects.
    async fn save(&self, text: &str, kind: &str, tags: &[String], global: bool) -> Result<String, String>;

    /// The memories that best match `query`, best first, from the project and the global store.
    async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryHit>, String>;
}

/// The service under which memory is registered when the feature is on.
pub struct Memory(pub Arc<dyn MemoryApi>);
