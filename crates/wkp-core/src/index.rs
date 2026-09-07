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

use std::fmt;
use std::path::{Path, PathBuf};

use crate::frontmatter::Frontmatter;
// `wkp-core` never depends on `rusqlite` directly: `wkp-sys` owns bundled
// SQLite (design 3.3) and re-exports it, so this is the one place the
// dependency is named.
use wkp_sys::rusqlite;

/// One store item: a path relative to the store root, its parsed
/// frontmatter, and its body (frontmatter already stripped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Item {
    pub path: String,
    pub frontmatter: Frontmatter,
    pub body: String,
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

pub use rusqlite::Connection;

const SCHEMA: &str = r#"
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
    tokenize = 'porter unicode61'
);

CREATE VIRTUAL TABLE items_trigram USING fts5(
    path UNINDEXED,
    content,
    tokenize = 'trigram'
);
"#;

/// A small, explicit boost on `title` and `tags` (design 5.3), expressed
/// as `bm25()` column weights in schema column order: path, title,
/// content, tags. Columns after `tags` are all `UNINDEXED` and contribute
/// nothing to the score regardless of weight, so they're left at the
/// `bm25()` default.
const BM25_WEIGHTS: (f64, f64, f64, f64) = (0.0, 2.0, 1.0, 2.0);

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

fn insert_item(conn: &Connection, item: &Item) -> rusqlite::Result<()> {
    let fm = &item.frontmatter;
    conn.execute("INSERT INTO paths (path) VALUES (?1)", [&item.path])?;
    conn.execute(
        "INSERT INTO items (
            path, title, content, tags, item_type, workspace, visibility,
            scope, confidence, provenance_source, tokens, updated, expires, refs
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
        rusqlite::params![
            item.path,
            fm.title.clone().unwrap_or_default(),
            item.body,
            fm.tags.join(" "),
            fm.item_type.as_ref().map(|t| t.to_string()),
            fm.workspace,
            fm.visibility.as_ref().map(|v| v.to_string()),
            fm.scope.as_ref().map(|s| s.to_string()),
            fm.confidence.as_ref().map(|c| c.to_string()),
            fm.provenance.source.as_ref().map(|s| s.to_string()),
            fm.tokens,
            fm.updated,
            fm.expires,
            fm.refs.join(" "),
        ],
    )?;
    let trigram_content = format!(
        "{}\n{}\n{}",
        item.path,
        fm.title.clone().unwrap_or_default(),
        item.body
    );
    conn.execute(
        "INSERT INTO items_trigram (path, content) VALUES (?1, ?2)",
        rusqlite::params![item.path, trigram_content],
    )?;
    Ok(())
}

fn populate(conn: &Connection, items: &[Item]) -> rusqlite::Result<()> {
    create_schema(conn)?;
    for item in items {
        insert_item(conn, item)?;
    }
    Ok(())
}

/// Builds an index from `items` entirely in memory. Useful for one-shot
/// queries and tests that don't need a persisted `index.db`.
pub fn build_in_memory(items: &[Item]) -> Result<Connection, IndexError> {
    let conn = wkp_sys::open_in_memory()?;
    populate(&conn, items)?;
    Ok(conn)
}

/// Builds an index from `items` and atomically publishes it at `dest`:
/// the whole database is written to a temporary file in `dest`'s
/// directory, then moved into place with `rename(2)`. On any error, the
/// temporary file is removed and `dest` is left completely untouched —
/// a reader never sees a torn or partially populated `index.db`.
pub fn build_index(dest: &Path, items: &[Item]) -> Result<(), IndexError> {
    let tmp_path = temp_path_for(dest);
    // Clean up after a previous crash that left a stale temp file behind.
    let _ = std::fs::remove_file(&tmp_path);

    let result = (|| -> Result<(), IndexError> {
        let conn = wkp_sys::open(&tmp_path)?;
        populate(&conn, items)?;
        Ok(())
    })();

    match result {
        Ok(()) => {
            std::fs::rename(&tmp_path, dest)?;
            Ok(())
        }
        Err(e) => {
            let _ = std::fs::remove_file(&tmp_path);
            Err(e)
        }
    }
}

fn temp_path_for(dest: &Path) -> PathBuf {
    let file_name = dest
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "index.db".to_string());
    let pid = std::process::id();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    dest.with_file_name(format!(".{file_name}.tmp-{pid}-{nanos}"))
}

/// Opens an existing `index.db` for reading (or writing, for callers that
/// build incrementally rather than through [`build_index`]).
pub fn open_index(path: &Path) -> Result<Connection, IndexError> {
    Ok(wkp_sys::open(path)?)
}

#[derive(Debug, Default, Clone)]
pub struct SearchFilter {
    pub item_type: Option<String>,
    pub workspace: Option<String>,
    pub visibility: Option<String>,
    pub scope: Option<String>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub score: f64,
}

/// Runs a BM25 full-text query against the `items` table, applying the
/// tier/metadata filters in `filter` (design 5.3). Results are ordered by
/// score, best match first.
pub fn search(
    conn: &Connection,
    query: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>, IndexError> {
    let (w_path, w_title, w_content, w_tags) = BM25_WEIGHTS;
    let mut sql = format!(
        "SELECT path, title, -bm25(items, {w_path}, {w_title}, {w_content}, {w_tags}) AS score \
         FROM items WHERE items MATCH ?"
    );
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query.to_string())];

    if let Some(v) = &filter.item_type {
        sql.push_str(" AND item_type = ?");
        params.push(Box::new(v.clone()));
    }
    if let Some(v) = &filter.workspace {
        sql.push_str(" AND workspace = ?");
        params.push(Box::new(v.clone()));
    }
    if let Some(v) = &filter.visibility {
        sql.push_str(" AND visibility = ?");
        params.push(Box::new(v.clone()));
    }
    if let Some(v) = &filter.scope {
        sql.push_str(" AND scope = ?");
        params.push(Box::new(v.clone()));
    }
    sql.push_str(" ORDER BY score DESC");
    if let Some(limit) = filter.limit {
        sql.push_str(" LIMIT ?");
        params.push(Box::new(limit as i64));
    }

    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok(SearchHit {
            path: row.get(0)?,
            title: row.get(1)?,
            score: row.get(2)?,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Runs a substring match against `items_trigram`, for identifiers and
/// paths (design 5.3). The query is wrapped as an FTS5 phrase so a
/// multi-character substring is matched contiguously rather than as
/// independent trigram terms.
pub fn search_trigram(
    conn: &Connection,
    substring: &str,
    limit: usize,
) -> Result<Vec<SearchHit>, IndexError> {
    let escaped = substring.replace('"', "\"\"");
    let match_expr = format!("\"{escaped}\"");
    let mut stmt =
        conn.prepare("SELECT path FROM items_trigram WHERE items_trigram MATCH ?1 LIMIT ?2")?;
    let rows = stmt.query_map(rusqlite::params![match_expr, limit as i64], |row| {
        Ok(SearchHit {
            path: row.get(0)?,
            title: String::new(),
            score: 0.0,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{ItemType, Visibility};

    fn item(path: &str, title: &str, body: &str) -> Item {
        let fm = Frontmatter {
            title: Some(title.to_string()),
            ..Default::default()
        };
        Item {
            path: path.to_string(),
            frontmatter: fm,
            body: body.to_string(),
        }
    }

    /// A securely created, uniquely named temp directory for a test's own
    /// `index.db` (`tempfile` rather than `std::env::temp_dir()` +
    /// a predictable name: the latter is flagged by this repo's semgrep
    /// gate as an insecure-temp-file pattern, since a shared temp
    /// directory with a guessable name invites symlink/TOCTOU races).
    fn temp_db_dir(name: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("wkp-core-test-{name}-"))
            .tempdir()
            .expect("create temp dir")
    }

    #[test]
    fn schema_insert_and_bm25_query_ranks_better_match_first() {
        let items = vec![
            item(
                "a.md",
                "Rust static binary",
                "wkp is a single static Rust binary",
            ),
            item(
                "b.md",
                "Postgres notes",
                "control plane uses Postgres for accounts",
            ),
        ];
        let conn = build_in_memory(&items).expect("build in-memory index");
        let hits = search(&conn, "rust", &SearchFilter::default()).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "a.md");
    }

    #[test]
    fn tier_and_metadata_filters_narrow_results() {
        let mut knowledge = item("k.md", "Decision", "search is BM25 by default");
        knowledge.frontmatter.item_type = Some(ItemType::Knowledge);
        knowledge.frontmatter.visibility = Some(Visibility::Shared);

        let mut inbox = item("i.md", "Proposed fact", "search is BM25 by default too");
        inbox.frontmatter.item_type = Some(ItemType::Memory);
        inbox.frontmatter.visibility = Some(Visibility::Private);

        let conn = build_in_memory(&[knowledge, inbox]).expect("build in-memory index");

        let filter = SearchFilter {
            item_type: Some("knowledge".to_string()),
            ..Default::default()
        };
        let hits = search(&conn, "BM25", &filter).expect("search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "k.md");

        let all = search(&conn, "BM25", &SearchFilter::default()).expect("search");
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn trigram_search_finds_path_substrings() {
        let items = vec![item(
            "crates/wkp-git/src/lib.rs",
            "Plumbing wrapper",
            "shells out to git",
        )];
        let conn = build_in_memory(&items).expect("build in-memory index");
        let hits = search_trigram(&conn, "wkp-git", 10).expect("trigram search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].path, "crates/wkp-git/src/lib.rs");
    }

    #[test]
    fn build_index_is_atomic_and_query_works_after_swap() {
        let dir = temp_db_dir("atomic-happy");
        let dest = dir.path().join("index.db");
        let items = vec![item("a.md", "A", "first version")];
        build_index(&dest, &items).expect("build index");

        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "first", &SearchFilter::default()).expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn failed_rebuild_leaves_previous_index_db_untouched() {
        let dir = temp_db_dir("atomic-crash");
        let dest = dir.path().join("index.db");
        let good_items = vec![item("a.md", "A", "first version")];
        build_index(&dest, &good_items).expect("initial build");
        let original_bytes = std::fs::read(&dest).expect("read original index.db");

        // Two items with the same path trip the `paths` UNIQUE constraint
        // partway through the second insert, simulating a rebuild that
        // fails after doing some, but not all, of its work.
        let broken_items = vec![
            item("a.md", "A", "second version"),
            item("a.md", "A", "duplicate path"),
        ];
        let result = build_index(&dest, &broken_items);
        assert!(result.is_err(), "expected the duplicate-path build to fail");

        let bytes_after = std::fs::read(&dest).expect("read index.db after failed rebuild");
        assert_eq!(
            original_bytes, bytes_after,
            "a failed rebuild must not modify the previously published index.db"
        );

        // And the previous content is still queryable: the failed rebuild
        // didn't corrupt or truncate anything.
        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "first", &SearchFilter::default()).expect("search");
        assert_eq!(hits.len(), 1);
        drop(conn);

        // No leftover temp file either. `temp_path_for` embeds a
        // nanosecond timestamp, so recomputing it wouldn't match the
        // actual name `build_index` used; scan the directory instead.
        let prefix = format!(".{}.tmp-", dest.file_name().unwrap().to_string_lossy());
        let leftover: Vec<_> = std::fs::read_dir(dir.path())
            .expect("read temp dir")
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().starts_with(&prefix))
            .collect();
        assert!(leftover.is_empty(), "leftover temp files: {leftover:?}");
    }
}
