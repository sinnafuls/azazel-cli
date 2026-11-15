//! The repository map: an outline of the project's most important definitions, ranked by how the
//! code refers to them, that fits a token budget. Read by a line scanner, or by grammars with the `treesitter` feature; kept per file by
//! modification time so that a second run reads only what changed.

mod ext;
mod extract;
mod index;
#[cfg(any(test, not(feature = "treesitter")))]
mod lex;
mod rank;
#[cfg(any(test, not(feature = "treesitter")))]
mod scan;
#[cfg(feature = "treesitter")]
mod treesitter;

pub use ext::{RepoMap, RepoMapExt, TOOL};
pub use index::Stats;
pub use rank::Focus;
