//! The derived SQLite index (design 5.2, 5.3): an FTS5 table with BM25
//! ranking over `content`/`title`/`tags`, a second FTS5 table with the
//! `trigram` tokenizer for identifier/path substring matches, and plain
//! columns from frontmatter used for filtering.
//!
//! FTS5 applies one tokenizer to an entire virtual table, not per column,
//! so "an additional trigram tokenizer column" (design 5.3) is implemented
//! here as a second FTS5 table (`items_trigram`) kept in step with the
//! main one, rather than a literal extra column on the same table — SQLite
//! has no mechanism for a single FTS5 table to use two tokenizers.
//!
//! Every write to `index.db` goes through [`build_index`], which builds
//! the whole database into a fresh temporary file and swaps it into place
//! with `rename(2)` (CLAUDE.md hard rule: never write a file a harness
//! reads in place). A build that errors partway never touches the
//! destination path.
//!
//! Split into modules by concern (schema/types, tier computation,
//! build/update, search/traversal/materialize, hybrid search) but
//! re-exported flat from here, so every `wkp_core::index::foo(...)` call
//! site elsewhere in the workspace is unaffected by which file a given
//! item actually lives in.

// `wkp-core` never depends on `rusqlite` directly: `wkp-sys` owns bundled
// SQLite (design 3.3) and re-exports it, so this is the one place the
// dependency is named.
pub use wkp_sys::rusqlite::Connection;

mod hybrid;
mod schema;
mod search;
mod store;
mod tier;

#[cfg(test)]
mod test_support;

pub use hybrid::{hybrid_search, set_embedding, HybridSearchError};
pub use schema::{IndexError, Item};
pub use search::{context, materialize, search, search_trigram, traverse, SearchFilter, SearchHit};
pub use store::{build_in_memory, build_index, known_paths, open_index, update_index};
