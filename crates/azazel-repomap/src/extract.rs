//! What the map knows of a source file: its definitions and the identifiers it uses, and the
//! extractor that reads them. The default is the line scanner in `scan.rs`; the `treesitter`
//! feature reads with grammars instead.

use std::path::Path;

use serde::{Deserialize, Serialize};

/// Longest signature kept, in characters.
pub(crate) const MAX_SIGNATURE: usize = 140;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Lang {
    Rust,
    Python,
    Go,
    JavaScript,
    TypeScript,
    Tsx,
}

impl Lang {
    pub fn of(path: &Path) -> Option<Lang> {
        let ext = path.extension()?.to_str()?;
        Some(match ext {
            "rs" => Lang::Rust,
            "py" | "pyi" => Lang::Python,
            "go" => Lang::Go,
            "js" | "mjs" | "cjs" | "jsx" => Lang::JavaScript,
            "ts" | "mts" | "cts" => Lang::TypeScript,
            "tsx" => Lang::Tsx,
            _ => return None,
        })
    }
}

/// One definition: a function, type, trait, class, method or constant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Def {
    /// The name other code refers to it by; empty for a block that only groups definitions
    /// (`impl Foo`), which is shown as the header of what it holds but is never ranked.
    pub name: String,
    /// 1-based line of the first line of the signature.
    pub line: u32,
    /// Source lines the signature spans.
    pub rows: u32,
    /// The definition this one is inside of (an index into the file's definitions).
    pub parent: Option<u32>,
    /// How many definitions it is inside of.
    pub depth: u32,
    /// The signature, on one line.
    pub sig: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Tags {
    pub defs: Vec<Def>,
    /// The identifiers the file uses, with how often.
    pub refs: Vec<(String, u32)>,
}

/// The definitions of `source` and the identifiers it uses; `None` when it cannot be read.
pub fn extract(lang: Lang, source: &str) -> Option<Tags> {
    #[cfg(feature = "treesitter")]
    return crate::treesitter::extract(lang, source);
    #[cfg(not(feature = "treesitter"))]
    return Some(crate::scan::extract(lang, source));
}
