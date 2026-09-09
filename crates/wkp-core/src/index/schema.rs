//! The `Item`/`IndexError` types and the FTS5 schema (design 5.2, 5.3).

use std::fmt;

use crate::frontmatter::Frontmatter;
use wkp_sys::rusqlite;

use super::Connection;

/// One store item: a path relative to the store root, its parsed
/// frontmatter, and its body (frontmatter already stripped).
// No `Eq`: `embedding: Option<Vec<f32>>` only supports `PartialEq` (`f32`
// isn't `Eq`).
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub path: String,
    pub frontmatter: Frontmatter,
    pub body: String,
    /// A precomputed embedding vector for this item's content, if the
    /// caller obtained one (design 5.3, M1-9: `wkp index --embed-url`).
    /// `insert_item`/`populate` store it as part of the same atomic
    /// temp-file-then-rename write as everything else -- deliberately
    /// *not* a separate `UPDATE` against a published `index.db` after the
    /// fact, which would violate CLAUDE.md's "never write a file a
    /// harness reads in place" for `index.db` itself.
    pub embedding: Option<Vec<f32>>,
    /// Whether this item's latest commit is signed by a `role: human`
    /// principal (design 7.3/7.4, M2-6) -- resolved by the caller via
    /// `wkp_git::allowed_signers::last_signer_for_path`, since `wkp-core`
    /// has no git dependency and stays that way (design 3.3's crate
    /// layout: `wkp-core` and `wkp-git` are siblings, neither depends on
    /// the other). [`super::tier::compute_tier`] is the real,
    /// non-placeholder consumer of this field.
    pub human_signed: bool,
}

#[derive(Debug)]
pub enum IndexError {
    Sqlite(rusqlite::Error),
    Io(std::io::Error),
}

impl fmt::Display for IndexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IndexError::Sqlite(e) => write!(f, "sqlite error: {e}"),
            IndexError::Io(e) => write!(f, "io error: {e}"),
        }
    }
}

impl std::error::Error for IndexError {}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError::Sqlite(e)
    }
}

impl From<std::io::Error> for IndexError {
    fn from(e: std::io::Error) -> Self {
        IndexError::Io(e)
    }
}

pub(super) const SCHEMA: &str = r#"
CREATE TABLE paths (
    path TEXT PRIMARY KEY
);

CREATE VIRTUAL TABLE items USING fts5(
    path UNINDEXED,
    title,
    content,
    tags,
    item_type UNINDEXED,
    workspace UNINDEXED,
    visibility UNINDEXED,
    scope UNINDEXED,
    confidence UNINDEXED,
    provenance_source UNINDEXED,
    tokens UNINDEXED,
    updated UNINDEXED,
    expires UNINDEXED,
    refs UNINDEXED,
    tier UNINDEXED,
    tokens_estimate UNINDEXED,
    embedding UNINDEXED,
    embedding_dim UNINDEXED,
    tokenize = 'porter unicode61'
);

CREATE VIRTUAL TABLE items_trigram USING fts5(
    path UNINDEXED,
    content,
    tokenize = 'trigram'
);

CREATE TABLE edges (
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    edge_type TEXT NOT NULL,
    PRIMARY KEY (source_path, target_path, edge_type)
);
"#;

/// A small, explicit boost on `title` and `tags` (design 5.3), expressed
/// as `bm25()` column weights in schema column order: path, title,
/// content, tags. Columns after `tags` are all `UNINDEXED` and contribute
/// nothing to the score regardless of weight, so they're left at the
/// `bm25()` default.
pub(super) const BM25_WEIGHTS: (f64, f64, f64, f64) = (0.0, 2.0, 1.0, 2.0);

pub(super) fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}
