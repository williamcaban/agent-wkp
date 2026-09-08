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

use crate::frontmatter::{Frontmatter, ItemType};
// `wkp-core` never depends on `rusqlite` directly: `wkp-sys` owns bundled
// SQLite (design 3.3) and re-exports it, so this is the one place the
// dependency is named.
use wkp_sys::rusqlite;
use wkp_sys::rusqlite::OptionalExtension;

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
    /// the other). [`compute_tier`] is the real, non-placeholder consumer
    /// of this field.
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
const BM25_WEIGHTS: (f64, f64, f64, f64) = (0.0, 2.0, 1.0, 2.0);

fn create_schema(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(SCHEMA)
}

/// The real, provenance-gated tier rule (design 7.4, M2-6) -- replaces
/// the M1-4 placeholder (a type/confidence-only heuristic; see the
/// milestone's own doc history) now that signed commits (M2-2) and the
/// `allowed_signers` identity model (M2-1) exist to check against. Tier
/// 0/1 requires *all* of:
///
/// - `human_signed`: the item's latest commit is signed by a `role:
///   human` principal. This alone is the actual design-7.4 gate; every
///   check below is an additional, independent guard on top of it.
/// - not under `inbox/` (design 5.4: "agent-written, unreviewed memory,
///   Tier 2 only until promoted") -- independent of `human_signed`,
///   because a human could otherwise directly commit a file under
///   `inbox/` without ever going through `wkp promote` (M2-7), which is
///   what this store's audit trail is supposed to require.
/// - `expires` (if set) has not passed (design 5.4: "the indexer excludes
///   expired items from Tier 0 and Tier 1 automatically" -- stored since
///   M1-1 but never actually checked until now).
/// - `type: instruction` additionally requires the path to be under
///   `user/` or anywhere under a `projects/<name>/` directory (design
///   7.4's extra scoping for instruction-like content). **Simplification**:
///   this checks "any `projects/*/`", not "the *current* project"
///   specifically -- tier is a property of the stored item computed at
///   index-build time, with no per-session "current project" context
///   available here; revisit if that distinction becomes load-bearing.
///
/// Otherwise: `project-state`/(scoped) `instruction` -> 0,
/// `feedback`/`knowledge` -> 1, everything else -> 2.
///
/// **Deliberately does not consult `confidence`.** An earlier version of
/// this gate treated `confidence: inferred`/`proposed` as a second,
/// independent "not yet reviewed" signal -- reasonable-looking while
/// `human_signed` didn't exist yet (the M1-4 placeholder used
/// `confidence` as its only proxy for "reviewed" at all), but wrong once
/// real signing landed: `wkp promote` (M2-7) explicitly moves an item
/// into the durable tree with a human-signed commit *without* rewriting
/// its `confidence`/`provenance` frontmatter (promotion is about the
/// commit's signer, not a content rewrite), so a promoted
/// `confidence: proposed` item would otherwise be permanently stuck at
/// tier 2 no matter who signs it -- caught by M2-7's own end-to-end
/// integration test actually promoting something, not by reasoning about
/// it in the abstract. Design 5.4 also never ties `confidence` to the
/// tier gate: it "mirrors the 'did the user say it' test," an epistemic
/// property of the *fact* (did the user state it, or did the agent infer
/// or propose it), orthogonal to whether the *item* has been reviewed and
/// signed. `human_signed` plus the `inbox/` check are design 7.4's actual,
/// complete gate.
///
/// Deliberately does **not** verify commits arriving via sync/fetch from
/// another machine (`wkp verify`) -- that's M3's "unsigned or
/// unknown-signer commits are excluded from Tier 0 and 1" exit-criterion
/// line, not this function's; `human_signed` here reflects whatever the
/// caller resolved regardless of a commit's origin.
fn compute_tier(path: &str, fm: &Frontmatter, human_signed: bool) -> u8 {
    if path.starts_with("inbox/") || path.contains("/inbox/") {
        return 2;
    }
    if !human_signed {
        return 2;
    }
    if fm.expires.as_deref().is_some_and(is_expired) {
        return 2;
    }
    match fm.item_type {
        Some(ItemType::Instruction) => {
            if path.starts_with("user/") || path.contains("projects/") {
                0
            } else {
                2
            }
        }
        Some(ItemType::ProjectState) => 0,
        Some(ItemType::Feedback | ItemType::Knowledge) => 1,
        _ => 2,
    }
}

/// Whether `expires` (design 5.4's optional ISO date, e.g. `2026-01-01`)
/// is in the past, by lexicographic comparison against today's date in
/// the same `YYYY-MM-DD` format -- correct because zero-padded ISO 8601
/// dates sort identically as strings and as calendar dates, so no date
/// parsing/arithmetic is needed for the comparison itself. A value that
/// doesn't look like `YYYY-MM-DD` (too short) is treated as unparseable
/// and therefore not expired -- fail open, matching this parser's
/// tolerant posture elsewhere (`wkp-core::frontmatter`), rather than
/// accidentally demoting an item over a malformed date.
fn is_expired(expires: &str) -> bool {
    if expires.len() < 10 {
        return false;
    }
    expires[..10] < *today_iso_date()
}

/// Today's date as `YYYY-MM-DD`, computed from the system clock without a
/// calendar/date crate dependency (CLAUDE.md's slim-core rule): Howard
/// Hinnant's `civil_from_days` algorithm
/// (<http://howardhinnant.github.io/date_algorithms.html#civil_from_days>),
/// a well-known, allocation-free, leap-year-correct conversion from a day
/// count to a proleptic Gregorian calendar date. Verified against known
/// reference dates (including a leap day and a pre-epoch day) in this
/// module's tests, not merely transcribed and trusted.
fn today_iso_date() -> String {
    let days_since_epoch = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() / 86_400) as i64)
        .unwrap_or(0);
    let (year, month, day) = civil_from_days(days_since_epoch);
    format!("{year:04}-{month:02}-{day:02}")
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

/// Falls back to roughly 4 characters per token (a common rough estimate
/// for English prose) when frontmatter doesn't carry an explicit `tokens:`
/// value. An estimate, not a real tokenizer count -- good enough for
/// budget truncation, not for billing or precise context-window math.
fn estimate_tokens(fm: &Frontmatter, body: &str) -> u32 {
    fm.tokens
        .unwrap_or_else(|| (body.chars().count() as u32 / 4).max(1))
}

fn insert_item(conn: &Connection, item: &Item) -> rusqlite::Result<()> {
    let fm = &item.frontmatter;
    let tier = compute_tier(&item.path, fm, item.human_signed);
    let tokens_estimate = estimate_tokens(fm, &item.body);
    conn.execute("INSERT INTO paths (path) VALUES (?1)", [&item.path])?;
    let embedding_bytes = item
        .embedding
        .as_ref()
        .map(|v| crate::embed::serialize_embedding(v));
    let embedding_dim = item.embedding.as_ref().map(|v| v.len() as i64);
    conn.execute(
        "INSERT INTO items (
            path, title, content, tags, item_type, workspace, visibility,
            scope, confidence, provenance_source, tokens, updated, expires,
            refs, tier, tokens_estimate, embedding, embedding_dim
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18)",
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
            tier,
            tokens_estimate,
            embedding_bytes,
            embedding_dim,
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
    // Edges are extracted in a second pass, after every item's content row
    // exists: a `[[wikilink]]` can name any item in the batch regardless of
    // insertion order, and resolving it (see `resolve_wikilink`) queries
    // `paths` directly.
    for item in items {
        insert_edges_for_item(conn, item)?;
    }
    Ok(())
}

fn delete_edges_from(conn: &Connection, source_path: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM edges WHERE source_path = ?1", [source_path])?;
    Ok(())
}

/// Recomputes `item`'s outgoing edges: `refs:` entries (normalized via
/// [`crate::graph::normalize_ref`]) and `[[wikilink]]` mentions in the body
/// (extracted via [`crate::graph::extract_wikilink_names`], resolved
/// against the store's current paths via [`resolve_wikilink`]). Always
/// clears the item's previous outgoing edges first, so calling this again
/// after the body changed doesn't leave stale edges behind.
fn insert_edges_for_item(conn: &Connection, item: &Item) -> rusqlite::Result<()> {
    delete_edges_from(conn, &item.path)?;
    for raw_ref in &item.frontmatter.refs {
        if let Some(target) = crate::graph::normalize_ref(&item.path, raw_ref) {
            conn.execute(
                "INSERT OR IGNORE INTO edges (source_path, target_path, edge_type) \
                 VALUES (?1, ?2, 'refs')",
                rusqlite::params![item.path, target],
            )?;
        }
    }
    for name in crate::graph::extract_wikilink_names(&item.body) {
        if let Some(target) = resolve_wikilink(conn, &name)? {
            conn.execute(
                "INSERT OR IGNORE INTO edges (source_path, target_path, edge_type) \
                 VALUES (?1, ?2, 'mentions')",
                rusqlite::params![item.path, target],
            )?;
        }
    }
    Ok(())
}

/// Resolves a `[[name]]` wikilink to a store path by filename stem,
/// matching the old Python tool's `**/{name}.md` glob semantics: an exact
/// `<name>.md` at the store root, or `<name>.md` anywhere in a
/// subdirectory. Ambiguous matches (more than one file with that stem)
/// resolve to the lexicographically first path — a best-effort choice,
/// same posture as the old tool's `glob()`-order pick.
fn resolve_wikilink(conn: &Connection, name: &str) -> rusqlite::Result<Option<String>> {
    let exact = format!("{name}.md");
    let suffix = format!("%/{name}.md");
    conn.query_row(
        "SELECT path FROM paths WHERE path = ?1 OR path LIKE ?2 ORDER BY path LIMIT 1",
        rusqlite::params![exact, suffix],
        |row| row.get(0),
    )
    .optional()
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

fn delete_item(conn: &Connection, path: &str) -> rusqlite::Result<()> {
    conn.execute("DELETE FROM paths WHERE path = ?1", [path])?;
    conn.execute("DELETE FROM items WHERE path = ?1", [path])?;
    conn.execute("DELETE FROM items_trigram WHERE path = ?1", [path])?;
    delete_edges_from(conn, path)?;
    // Known limitation: edges *pointing at* `path` from other, unchanged
    // items are left dangling rather than cleaned up -- `traverse`'s join
    // against `items` already excludes them from results (a dangling edge
    // has no matching row to join to), so this doesn't produce wrong
    // output, just an unused row. A full incoming-edge sweep would need to
    // touch every item that might reference the deleted path, which is
    // exactly the O(corpus) cost M1-3 exists to avoid.
    Ok(())
}

/// Applies an incremental change (design 5.1: "only those files are
/// hashed and re-indexed", M1-3) to the index at `dest`, without reading
/// or re-inserting every unchanged item.
///
/// `dest` is still never written in place (CLAUDE.md hard rule): the
/// current `index.db` is cloned into a fresh temp file with SQLite's own
/// `VACUUM INTO` (a read of `dest`, not a write to it), the changes are
/// applied to that copy, and the copy is renamed over `dest` on success.
/// If `dest` doesn't exist yet, this builds a fresh index instead,
/// equivalent to [`build_index`] with just `upserts`.
///
/// `VACUUM INTO` copies the whole file regardless of how many rows
/// change, so this does not make the on-disk write itself proportional
/// to the change count — what it avoids is the far more expensive part at
/// realistic corpus scale: re-reading, re-parsing, and re-tokenizing every
/// unchanged file's content, which is what made the old Python tool's
/// `git hash-object`-every-file approach slow (design 5.1).
///
/// That said, `VACUUM INTO`'s file copy is itself not free at realistic
/// scale: benchmarked at ~460ms for a 10-item change against a 50k-item
/// corpus, well past design 4.3's incremental-index target. See
/// `docs/adr/0002-incremental-index-write-mechanism.md` (status: proposed,
/// not yet decided) for the options — including writing `dest` in place
/// inside a SQLite transaction instead, which this function does not do
/// today, deliberately, pending that decision.
pub fn update_index(
    dest: &Path,
    upserts: &[Item],
    deleted_paths: &[String],
) -> Result<(), IndexError> {
    let tmp_path = temp_path_for(dest);
    let _ = std::fs::remove_file(&tmp_path);

    let result = (|| -> Result<(), IndexError> {
        if dest.exists() {
            let src = wkp_sys::open(dest)?;
            src.execute("VACUUM INTO ?1", [tmp_path.to_string_lossy().into_owned()])?;
        } else {
            let conn = wkp_sys::open(&tmp_path)?;
            create_schema(&conn)?;
        }

        let conn = wkp_sys::open(&tmp_path)?;
        for path in deleted_paths {
            delete_item(&conn, path)?;
        }
        for item in upserts {
            // An upsert on a path already present must replace, not
            // duplicate, its row -- clear it first regardless of whether
            // the caller classified it as "added" or "modified".
            delete_item(&conn, &item.path)?;
            insert_item(&conn, item)?;
        }
        // Same reasoning as `populate`: edges recomputed only after every
        // upserted item's content row exists, so a wikilink in one changed
        // item can resolve against another changed item in the same batch.
        // Unchanged items keep whatever edges `VACUUM INTO` already copied.
        for item in upserts {
            insert_edges_for_item(&conn, item)?;
        }
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

/// Every path the index currently holds a row for. Lets a caller compute
/// "added since the index last saw this store" (in the working tree but
/// not here) and "deleted" (here but no longer in the working tree)
/// without needing its own separate bookkeeping of what was indexed last
/// time -- `index.db` already is that bookkeeping.
pub fn known_paths(conn: &Connection) -> Result<std::collections::HashSet<String>, IndexError> {
    let mut stmt = conn.prepare("SELECT path FROM paths")?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
    Ok(rows.collect::<Result<_, _>>()?)
}

#[derive(Debug, Default, Clone)]
pub struct SearchFilter {
    pub item_type: Option<String>,
    pub workspace: Option<String>,
    pub visibility: Option<String>,
    pub scope: Option<String>,
    /// Keep only items at or below this tier (0 is the most restrictive).
    /// See [`compute_tier`]'s doc comment for what "tier" means today and
    /// its known limitation (a provisional heuristic, not design 7.4's
    /// real provenance gate, which lands with signing in M2).
    pub tier: Option<u8>,
    /// Stop including results once their cumulative estimated token count
    /// would exceed this. Applied after ranking and the tier/metadata
    /// filters, so the highest-scoring results within budget win. The
    /// first result is always kept even if it alone exceeds the budget,
    /// so a budget smaller than any single item still returns something
    /// rather than nothing.
    pub budget: Option<u32>,
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub path: String,
    pub title: String,
    pub score: f64,
    pub tier: u8,
    pub tokens: u32,
    /// Hops from a `traverse`/`context` starting point along explicit
    /// `refs:`/`[[wikilink]]` edges; `0` for a direct search match.
    pub hop_distance: u32,
}

/// Runs a BM25 full-text query against the `items` table, applying the
/// tier/metadata filters in `filter` (design 5.3), then truncates to
/// `filter.budget` estimated tokens if set. Results are ordered by score,
/// best match first.
pub fn search(
    conn: &Connection,
    query: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>, IndexError> {
    let (w_path, w_title, w_content, w_tags) = BM25_WEIGHTS;
    let mut sql = format!(
        "SELECT path, title, -bm25(items, {w_path}, {w_title}, {w_content}, {w_tags}) AS score, \
         tier, tokens_estimate FROM items WHERE items MATCH ?"
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
    if let Some(v) = filter.tier {
        sql.push_str(" AND tier <= ?");
        params.push(Box::new(v));
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
            tier: row.get(3)?,
            tokens: row.get(4)?,
            hop_distance: 0,
        })
    })?;
    let hits = rows.collect::<Result<Vec<_>, _>>()?;
    Ok(apply_budget(hits, filter.budget))
}

fn apply_budget(hits: Vec<SearchHit>, budget: Option<u32>) -> Vec<SearchHit> {
    let Some(budget) = budget else {
        return hits;
    };
    let mut kept = Vec::with_capacity(hits.len());
    let mut spent: u64 = 0;
    for hit in hits {
        let would_spend = spent + u64::from(hit.tokens);
        if !kept.is_empty() && would_spend > u64::from(budget) {
            break;
        }
        spent = would_spend;
        kept.push(hit);
    }
    kept
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
            tier: 0,
            tokens: 0,
            hop_distance: 0,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Follows explicit `refs:`/`[[wikilink]]` edges from `start_path` up to
/// `max_depth` hops (design 5.1, M1-5), ordered by hop distance then tier.
/// A cycle terminates naturally at `max_depth` rather than needing
/// separate cycle-detection: `WITH RECURSIVE` bounds recursion by depth
/// regardless of how many cyclic paths reach a node, and `GROUP BY`
/// collapses a node reached multiple ways to its single shortest depth.
pub fn traverse(
    conn: &Connection,
    start_path: &str,
    max_depth: u32,
) -> Result<Vec<SearchHit>, IndexError> {
    let sql = "
        WITH RECURSIVE reachable(path, depth) AS (
            SELECT ?1, 0
            UNION ALL
            SELECT e.target_path, r.depth + 1
            FROM edges e
            JOIN reachable r ON e.source_path = r.path
            WHERE r.depth < ?2
        )
        SELECT i.path, i.title, i.tier, i.tokens_estimate, MIN(r.depth) AS depth
        FROM reachable r
        JOIN items i ON i.path = r.path
        WHERE i.path != ?1
        GROUP BY i.path
        ORDER BY depth ASC, i.tier ASC
    ";
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(rusqlite::params![start_path, max_depth], |row| {
        let depth: u32 = row.get(4)?;
        Ok(SearchHit {
            path: row.get(0)?,
            title: row.get(1)?,
            score: 1.0 / f64::from(1 + depth),
            tier: row.get(2)?,
            tokens: row.get(3)?,
            hop_distance: depth,
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

/// Tier-aware context assembly (design 5.1/5.4, M1-5): BM25 search for
/// `topic`, then graph traversal (depth 2) from the top three hits,
/// deduplicated by path, truncated to `filter.budget` estimated tokens.
///
/// Deliberate behavioral difference from the old Python tool's
/// `context_assemble`: that implementation skips *any* item whose own
/// token count exceeds the remaining budget, including the very first one
/// — a topic whose best match alone exceeds the budget returns nothing at
/// all. This reuses [`apply_budget`]'s "always keep the best single
/// result" rule (already established in M1-4 for `search`) instead, for
/// the same reason: an empty result is a worse outcome than one
/// over-budget result for a caller assembling context.
pub fn context(
    conn: &Connection,
    topic: &str,
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>, IndexError> {
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut combined = Vec::new();

    let search_filter = SearchFilter {
        item_type: filter.item_type.clone(),
        tier: filter.tier,
        workspace: filter.workspace.clone(),
        visibility: filter.visibility.clone(),
        scope: filter.scope.clone(),
        budget: None,
        limit: Some(20),
    };
    for hit in search(conn, topic, &search_filter)? {
        if seen.insert(hit.path.clone()) {
            combined.push(hit);
        }
    }

    let seeds: Vec<String> = combined.iter().take(3).map(|h| h.path.clone()).collect();
    for seed in seeds {
        for neighbor in traverse(conn, &seed, 2)? {
            if let Some(tier) = filter.tier {
                if neighbor.tier > tier {
                    continue;
                }
            }
            if seen.insert(neighbor.path.clone()) {
                combined.push(neighbor);
            }
        }
    }

    Ok(apply_budget(combined, filter.budget))
}

/// Stores `embedding` for the item at `path` (design 5.3, M1-9): the
/// `embedding` BLOB and `embedding_dim` columns are plain columns on
/// `items` regardless of whether this build has the `embed` Cargo
/// feature -- schema shape must not depend on how the binary was built
/// (CLAUDE.md: "no new on-disk formats"), only whether anything ever
/// populates or reads them does. A no-op (zero rows affected) if `path`
/// isn't in the index; callers already know the set of paths they just
/// upserted.
pub fn set_embedding(conn: &Connection, path: &str, embedding: &[f32]) -> Result<(), IndexError> {
    conn.execute(
        "UPDATE items SET embedding = ?1, embedding_dim = ?2 WHERE path = ?3",
        rusqlite::params![
            crate::embed::serialize_embedding(embedding),
            embedding.len() as i64,
            path,
        ],
    )?;
    Ok(())
}

/// Every distinct `embedding_dim` currently stored, for a helpful "index
/// has embeddings at dimension N" detail in a dimension-mismatch error
/// message -- without this, a user re-indexing with a different
/// embedding model only sees "no usable embeddings", not why.
fn known_embedding_dims(conn: &Connection) -> Result<Vec<u32>, IndexError> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT embedding_dim FROM items WHERE embedding_dim IS NOT NULL ORDER BY embedding_dim",
    )?;
    let rows = stmt.query_map([], |row| row.get::<_, i64>(0))?;
    Ok(rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|d| d as u32)
        .collect())
}

/// Why [`hybrid_search`] returned no fused result at all: distinguished
/// from [`IndexError`] because it isn't a failure of the index itself --
/// it's the signal a caller uses to fall back to BM25-only `search`
/// (design 5.3: "Falls back to FTS5-only if the vector index dimension
/// mismatches the embedding model", carried over from agent-wkp).
#[derive(Debug)]
pub enum HybridSearchError {
    Index(IndexError),
    /// No stored embedding matches `query_dim` -- either nothing has been
    /// embedded yet (`stored_dims` empty), or `wkp index --embed-url` used
    /// a different model last time (`stored_dims` non-empty but doesn't
    /// contain `query_dim`).
    NoMatchingEmbeddingDimension {
        query_dim: usize,
        stored_dims: Vec<u32>,
    },
}

impl fmt::Display for HybridSearchError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HybridSearchError::Index(e) => write!(f, "{e}"),
            HybridSearchError::NoMatchingEmbeddingDimension {
                query_dim,
                stored_dims,
            } if stored_dims.is_empty() => write!(
                f,
                "no items have been embedded yet (query embedding is {query_dim}-dimensional); \
                 run `wkp index --embed-url ...` first, or omit --embed-url to use BM25"
            ),
            HybridSearchError::NoMatchingEmbeddingDimension {
                query_dim,
                stored_dims,
            } => write!(
                f,
                "embedding dimension mismatch: the query embedding is {query_dim}-dimensional, \
                 but the index has embeddings at dimension {stored_dims:?} -- re-index with the \
                 same embedding model, or omit --embed-url to use BM25"
            ),
        }
    }
}

impl std::error::Error for HybridSearchError {}

impl From<IndexError> for HybridSearchError {
    fn from(e: IndexError) -> Self {
        HybridSearchError::Index(e)
    }
}

impl From<rusqlite::Error> for HybridSearchError {
    fn from(e: rusqlite::Error) -> Self {
        HybridSearchError::Index(IndexError::from(e))
    }
}

/// Hybrid search (design 5.3, M1-9): Reciprocal Rank Fusion between BM25
/// full-text ranking and cosine similarity against `query_embedding`,
/// scoped by the same tier/metadata filters as plain [`search`]. Computed
/// entirely in Rust over the rows already selected by SQL (agent-memory
/// corpora are small -- design 4.3's own fixture corpora top out at 50k
/// items -- so a brute-force scan avoids adding a native vector-search
/// SQLite extension, which would need its own `unsafe`-carrying C
/// dependency in `wkp-sys`; see the ADR referenced in `embed.rs`).
///
/// Returns [`HybridSearchError::NoMatchingEmbeddingDimension`] rather than
/// an empty result when nothing in scope has a usable embedding, so the
/// caller can fall back to BM25-only `search` and tell the user why,
/// exactly as agent-wkp's Python implementation did.
pub fn hybrid_search(
    conn: &Connection,
    query: &str,
    query_embedding: &[f32],
    filter: &SearchFilter,
) -> Result<Vec<SearchHit>, HybridSearchError> {
    let bm25_filter = SearchFilter {
        limit: Some(40),
        budget: None,
        ..filter.clone()
    };
    let bm25_hits = search(conn, query, &bm25_filter)?;
    let bm25_ranked: Vec<String> = bm25_hits.iter().map(|h| h.path.clone()).collect();

    let mut sql = "SELECT path, embedding FROM items WHERE embedding_dim = ?1".to_string();
    let mut params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(query_embedding.len() as i64)];
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
    if let Some(v) = filter.tier {
        sql.push_str(" AND tier <= ?");
        params.push(Box::new(v));
    }
    let mut stmt = conn.prepare(&sql)?;
    let param_refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|b| b.as_ref()).collect();
    let rows = stmt.query_map(param_refs.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    let mut scored: Vec<(String, f32)> = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(|(path, bytes)| {
            let vec = crate::embed::deserialize_embedding(&bytes);
            let score = crate::embed::cosine_similarity(&vec, query_embedding);
            (path, score)
        })
        .collect();

    if scored.is_empty() {
        return Err(HybridSearchError::NoMatchingEmbeddingDimension {
            query_dim: query_embedding.len(),
            stored_dims: known_embedding_dims(conn)?,
        });
    }
    scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    scored.truncate(40);
    let vec_ranked: Vec<String> = scored.into_iter().map(|(path, _)| path).collect();

    let fused = crate::embed::reciprocal_rank_fusion(&[&bm25_ranked, &vec_ranked]);

    let mut by_path: std::collections::HashMap<String, SearchHit> =
        bm25_hits.into_iter().map(|h| (h.path.clone(), h)).collect();
    let mut hits = Vec::with_capacity(fused.len());
    for (path, score) in fused {
        let hit = match by_path.remove(&path) {
            Some(mut hit) => {
                hit.score = score;
                hit
            }
            None => {
                let Some((title, tier, tokens)) = fetch_item_metadata(conn, &path)? else {
                    continue;
                };
                SearchHit {
                    path,
                    title,
                    score,
                    tier,
                    tokens,
                    hop_distance: 0,
                }
            }
        };
        hits.push(hit);
    }
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    if let Some(limit) = filter.limit {
        hits.truncate(limit);
    }
    Ok(apply_budget(hits, filter.budget))
}

fn fetch_item_metadata(
    conn: &Connection,
    path: &str,
) -> Result<Option<(String, u8, u32)>, IndexError> {
    conn.query_row(
        "SELECT title, tier, tokens_estimate FROM items WHERE path = ?1",
        [path],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .map_err(IndexError::from)
}

/// Assembles the exact `tier{N}.md` content for `wkp materialize` (design
/// 4.2/4.3, M1-6): every item whose computed tier equals `tier` exactly
/// (not "at or below" -- each materialized file is its own tier's content,
/// composable by concatenation if a caller wants more than one), wrapped
/// in a `<wkp-context tier="N">` block matching the marker `AGENTS.md`
/// already documents for a harness to recognize injected content by.
///
/// `compute_tier`'s `inbox/` exclusion means this can never surface
/// agent-written, unreviewed content in tier 0/1 output -- there is no
/// separate check here for it, because there is nothing to filter: an
/// `inbox/` item is never tier 0 or 1 in the first place.
pub fn materialize(conn: &Connection, tier: u8) -> Result<String, IndexError> {
    let mut stmt =
        conn.prepare("SELECT path, title, content FROM items WHERE tier = ?1 ORDER BY path")?;
    let rows = stmt.query_map([tier], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut body = String::new();
    for row in rows {
        let (path, title, content) = row?;
        body.push_str(&format!(
            "## {title}\n\n<!-- source: {path} -->\n\n{content}\n\n"
        ));
    }
    Ok(format!(
        "<wkp-context tier=\"{tier}\">\n\n{body}</wkp-context>\n"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{Confidence, ItemType, Visibility};

    /// `human_signed: true` by default: every existing test in this
    /// module predates M2-6's signing gate and is testing something else
    /// entirely (search ranking, traversal, materialize output shape,
    /// ...) -- defaulting to "properly reviewed" here means none of them
    /// needed individual updates for M2-6, only the tests that
    /// specifically exercise the gate itself (below) override it.
    fn item(path: &str, title: &str, body: &str) -> Item {
        let fm = Frontmatter {
            title: Some(title.to_string()),
            ..Default::default()
        };
        Item {
            path: path.to_string(),
            frontmatter: fm,
            body: body.to_string(),
            embedding: None,
            human_signed: true,
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
    fn compute_tier_reflects_type_when_human_signed() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(compute_tier("a.md", &fm, true), 0);

        fm.item_type = Some(ItemType::Feedback);
        assert_eq!(compute_tier("a.md", &fm, true), 1);

        fm.item_type = Some(ItemType::Reference);
        assert_eq!(compute_tier("a.md", &fm, true), 2);
    }

    /// M2-7 found this the hard way (an end-to-end `wkp promote` test
    /// failed until this was fixed): `wkp promote` explicitly never
    /// rewrites `confidence`, so a promoted item human-signs into the
    /// durable tree while still carrying `confidence: proposed` from
    /// when it was written. Tier eligibility must not depend on
    /// `confidence` at all -- only on `human_signed` and not being under
    /// `inbox/` (design 7.4's actual gate).
    #[test]
    fn compute_tier_ignores_confidence_once_human_signed_and_out_of_inbox() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            confidence: Some(Confidence::Proposed),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("projects/wkp/promoted.md", &fm, true),
            0,
            "a human-signed, promoted item must reach tier 0 even with confidence: proposed"
        );

        fm.confidence = Some(Confidence::Inferred);
        assert_eq!(compute_tier("projects/wkp/promoted.md", &fm, true), 0);

        fm.confidence = Some(Confidence::Stated);
        assert_eq!(compute_tier("projects/wkp/promoted.md", &fm, true), 0);
    }

    /// M2-6's actual gate (design 7.4): the check the M1-4 placeholder
    /// could not make, since signed commits didn't exist yet.
    #[test]
    fn compute_tier_requires_a_human_signed_commit_for_tier_0_or_1() {
        let fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("a.md", &fm, false),
            2,
            "type: project-state alone must not reach tier 0/1 without a human-signed commit"
        );
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "the same frontmatter with a human-signed commit does reach tier 0"
        );
    }

    #[test]
    fn compute_tier_forces_tier_2_for_anything_under_inbox() {
        let fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("inbox/import/claude-md.md", &fm, true),
            2,
            "an inbox/ item must not self-promote to tier 0 via its type, even if human-signed"
        );
        assert_eq!(
            compute_tier("projects/wkp/inbox/note.md", &fm, true),
            2,
            "a nested inbox/ directory anywhere in the path must also be caught"
        );
        assert_eq!(
            compute_tier("projects/wkp/decision.md", &fm, true),
            0,
            "a normal path with the same frontmatter is unaffected"
        );
    }

    #[test]
    fn compute_tier_instruction_type_requires_user_or_projects_path_scoping() {
        let fm = Frontmatter {
            item_type: Some(ItemType::Instruction),
            ..Default::default()
        };
        assert_eq!(
            compute_tier("org/wide-policy.md", &fm, true),
            2,
            "type: instruction outside user/ or projects/ must not reach tier 0"
        );
        assert_eq!(compute_tier("user/preferences.md", &fm, true), 0);
        assert_eq!(compute_tier("projects/wkp/agents.md", &fm, true), 0);
    }

    #[test]
    fn compute_tier_expired_item_is_tier_2_regardless_of_type_and_signing() {
        let mut fm = Frontmatter {
            item_type: Some(ItemType::ProjectState),
            ..Default::default()
        };
        fm.expires = Some("2000-01-01".to_string());
        assert_eq!(
            compute_tier("a.md", &fm, true),
            2,
            "an item past its expires date must not reach tier 0/1"
        );

        fm.expires = Some("9999-12-31".to_string());
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "an item with a future expires date is unaffected"
        );

        fm.expires = None;
        assert_eq!(
            compute_tier("a.md", &fm, true),
            0,
            "no expires date at all is unaffected"
        );
    }

    #[test]
    fn is_expired_treats_a_too_short_value_as_unparseable_and_not_expired() {
        assert!(!is_expired("2000"));
        assert!(!is_expired(""));
    }

    #[test]
    fn civil_from_days_matches_known_reference_dates() {
        // Cross-checked against Python's datetime.date arithmetic,
        // including a leap day and a pre-epoch (negative day count) date
        // -- not just transcribed from the algorithm source and trusted.
        for (days, expected) in [
            (0i64, (1970, 1, 1)),
            (11_017, (2000, 3, 1)),
            (20_703, (2026, 9, 7)),
            (19_782, (2024, 2, 29)),
            (-1, (1969, 12, 31)),
        ] {
            assert_eq!(civil_from_days(days), expected, "days={days}");
        }
    }

    /// End-to-end through `insert_item`/`search`, not just a direct
    /// `compute_tier` call -- proves `Item::human_signed` actually flows
    /// through the real index-build path, not only the standalone
    /// function.
    #[test]
    fn unsigned_item_does_not_reach_tier_0_through_the_real_index_build_path() {
        let mut signed = item("signed.md", "Signed", "shared distinctive vocabulary");
        signed.frontmatter.item_type = Some(ItemType::ProjectState);
        signed.human_signed = true;

        let mut unsigned = item(
            "unsigned.md",
            "Unsigned",
            "shared distinctive vocabulary too",
        );
        unsigned.frontmatter.item_type = Some(ItemType::ProjectState);
        unsigned.human_signed = false;

        let conn = build_in_memory(&[signed, unsigned]).expect("build in-memory index");
        let filter = SearchFilter {
            tier: Some(0),
            ..Default::default()
        };
        let hits = search(&conn, "distinctive", &filter).expect("search");
        assert_eq!(
            hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["signed.md"],
            "the unsigned item must not appear in a tier-0 search despite matching type"
        );
    }

    #[test]
    fn tier_filter_keeps_only_items_at_or_below_the_requested_tier() {
        let mut tier0 = item("t0.md", "Decision", "shared vocabulary here");
        tier0.frontmatter.item_type = Some(ItemType::ProjectState);

        let mut tier1 = item("t1.md", "Lesson", "shared vocabulary here too");
        tier1.frontmatter.item_type = Some(ItemType::Feedback);

        let mut tier2 = item("t2.md", "Note", "shared vocabulary here as well");
        tier2.frontmatter.item_type = Some(ItemType::Reference);

        let conn = build_in_memory(&[tier0, tier1, tier2]).expect("build in-memory index");

        let filter = SearchFilter {
            tier: Some(0),
            ..Default::default()
        };
        let hits = search(&conn, "vocabulary", &filter).expect("search");
        assert_eq!(
            hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["t0.md"]
        );

        let filter = SearchFilter {
            tier: Some(1),
            ..Default::default()
        };
        let hits = search(&conn, "vocabulary", &filter).expect("search");
        let mut paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["t0.md", "t1.md"]);
    }

    #[test]
    fn budget_stops_once_cumulative_tokens_would_be_exceeded() {
        let mut small = item("small.md", "Small", "shared topic word content");
        small.frontmatter.tokens = Some(10);
        let mut medium = item("medium.md", "Medium", "shared topic word content extra");
        medium.frontmatter.tokens = Some(20);
        let mut large = item("large.md", "Large", "shared topic word content extra more");
        large.frontmatter.tokens = Some(1000);

        // Insert so BM25 ranks small > medium > large for "topic" (more
        // exact-length match on shorter content raises the score, but the
        // exact ranking doesn't matter here -- what matters is that the
        // truncation happens in ranked order without exceeding budget).
        let conn = build_in_memory(&[small, medium, large]).expect("build in-memory index");

        let filter = SearchFilter {
            budget: Some(25),
            ..Default::default()
        };
        let hits = search(&conn, "topic", &filter).expect("search");
        let total: u32 = hits.iter().map(|h| h.tokens).sum();
        assert!(total <= 25 || hits.len() == 1, "budget exceeded: {hits:?}");
        assert!(!hits.is_empty());
    }

    #[test]
    fn budget_smaller_than_the_single_best_result_still_returns_it() {
        let mut huge = item("huge.md", "Huge", "distinctive query term appears here");
        huge.frontmatter.tokens = Some(5_000);
        let conn = build_in_memory(&[huge]).expect("build in-memory index");

        let filter = SearchFilter {
            budget: Some(1),
            ..Default::default()
        };
        let hits = search(&conn, "distinctive", &filter).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "a budget smaller than any item must still return the best match"
        );
        assert_eq!(hits[0].path, "huge.md");
    }

    #[test]
    fn refs_edge_is_extracted_and_traversable() {
        let mut a = item("a.md", "A", "links to b via refs");
        a.frontmatter.refs = vec!["b.md".to_string()];
        let b = item("b.md", "B", "target of the ref");
        let conn = build_in_memory(&[a, b]).expect("build in-memory index");

        let neighbors = traverse(&conn, "a.md", 1).expect("traverse");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].path, "b.md");
        assert_eq!(neighbors[0].hop_distance, 1);
    }

    #[test]
    fn wikilink_edge_is_extracted_and_traversable() {
        let a = item("a.md", "A", "see [[Target Note]] for details");
        let b = item("Target Note.md", "Target Note", "the referenced content");
        let conn = build_in_memory(&[a, b]).expect("build in-memory index");

        let neighbors = traverse(&conn, "a.md", 1).expect("traverse");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].path, "Target Note.md");
    }

    #[test]
    fn wikilink_in_a_subdirectory_resolves_by_filename_stem() {
        let a = item("a.md", "A", "see [[note]] elsewhere");
        let b = item("projects/note.md", "Note", "nested target");
        let conn = build_in_memory(&[a, b]).expect("build in-memory index");

        let neighbors = traverse(&conn, "a.md", 1).expect("traverse");
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].path, "projects/note.md");
    }

    #[test]
    fn traverse_respects_max_depth() {
        let mut a = item("a.md", "A", "chain start");
        a.frontmatter.refs = vec!["b.md".to_string()];
        let mut b = item("b.md", "B", "chain middle");
        b.frontmatter.refs = vec!["c.md".to_string()];
        let c = item("c.md", "C", "chain end");
        let conn = build_in_memory(&[a, b, c]).expect("build in-memory index");

        let depth1 = traverse(&conn, "a.md", 1).expect("traverse depth 1");
        assert_eq!(
            depth1.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["b.md"]
        );

        let depth2 = traverse(&conn, "a.md", 2).expect("traverse depth 2");
        let mut paths: Vec<&str> = depth2.iter().map(|h| h.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["b.md", "c.md"]);
    }

    #[test]
    fn traverse_handles_a_cycle_without_infinite_loop_or_duplicates() {
        let mut a = item("a.md", "A", "cycle start");
        a.frontmatter.refs = vec!["b.md".to_string()];
        let mut b = item("b.md", "B", "cycle back to a");
        b.frontmatter.refs = vec!["a.md".to_string()];
        let conn = build_in_memory(&[a, b]).expect("build in-memory index");

        let neighbors = traverse(&conn, "a.md", 5).expect("traverse a 5-deep cycle");
        // Must terminate (this call returning at all is half the test) and
        // must not report "a.md" (the start) or duplicate "b.md".
        assert_eq!(neighbors.len(), 1);
        assert_eq!(neighbors[0].path, "b.md");
    }

    #[test]
    fn context_combines_search_and_traversal_deduplicated() {
        let mut hit = item("hit.md", "Hit", "distinctive search topic");
        hit.frontmatter.refs = vec!["neighbor.md".to_string()];
        let neighbor = item("neighbor.md", "Neighbor", "reached only via refs");
        let conn = build_in_memory(&[hit, neighbor]).expect("build in-memory index");

        let hits = context(&conn, "distinctive", &SearchFilter::default()).expect("context");
        let mut paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["hit.md", "neighbor.md"]);

        let hit_entry = hits.iter().find(|h| h.path == "hit.md").unwrap();
        assert_eq!(hit_entry.hop_distance, 0);
        let neighbor_entry = hits.iter().find(|h| h.path == "neighbor.md").unwrap();
        assert_eq!(neighbor_entry.hop_distance, 1);
    }

    #[test]
    fn context_does_not_duplicate_a_neighbor_that_is_also_a_direct_hit() {
        let mut hit = item("hit.md", "Hit", "distinctive search topic");
        hit.frontmatter.refs = vec!["other.md".to_string()];
        let mut other = item("other.md", "Other", "distinctive search topic too");
        other.frontmatter.refs = vec!["hit.md".to_string()];
        let conn = build_in_memory(&[hit, other]).expect("build in-memory index");

        let hits = context(&conn, "distinctive", &SearchFilter::default()).expect("context");
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(
            paths.len(),
            2,
            "each path must appear exactly once: {paths:?}"
        );
    }

    #[test]
    fn context_tier_filter_excludes_higher_tier_neighbors() {
        let mut hit = item("hit.md", "Hit", "distinctive search topic");
        hit.frontmatter.item_type = Some(ItemType::ProjectState); // tier 0
        hit.frontmatter.refs = vec!["neighbor.md".to_string()];
        let mut neighbor = item("neighbor.md", "Neighbor", "reached via refs");
        neighbor.frontmatter.item_type = Some(ItemType::Reference); // tier 2
        let conn = build_in_memory(&[hit, neighbor]).expect("build in-memory index");

        let filter = SearchFilter {
            tier: Some(0),
            ..Default::default()
        };
        let hits = context(&conn, "distinctive", &filter).expect("context");
        assert_eq!(
            hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["hit.md"]
        );
    }

    #[test]
    fn context_respects_budget_across_search_and_traversal() {
        let mut hit = item("hit.md", "Hit", "distinctive search topic");
        hit.frontmatter.tokens = Some(10);
        hit.frontmatter.refs = vec!["neighbor.md".to_string()];
        let mut neighbor = item("neighbor.md", "Neighbor", "reached via refs");
        neighbor.frontmatter.tokens = Some(1_000);
        let conn = build_in_memory(&[hit, neighbor]).expect("build in-memory index");

        let filter = SearchFilter {
            budget: Some(10),
            ..Default::default()
        };
        let hits = context(&conn, "distinctive", &filter).expect("context");
        assert_eq!(
            hits.iter().map(|h| h.path.as_str()).collect::<Vec<_>>(),
            vec!["hit.md"],
            "the 1000-token neighbor must not fit in a 10-token remaining budget"
        );
    }

    #[test]
    fn edges_are_recomputed_when_an_item_is_updated() {
        let dir = temp_db_dir("edges-update");
        let dest = dir.path().join("index.db");
        let mut a = item("a.md", "A", "first version");
        a.frontmatter.refs = vec!["b.md".to_string()];
        let b = item("b.md", "B", "target");
        build_index(&dest, &[a, b]).expect("initial build");

        {
            let conn = open_index(&dest).expect("open index");
            let neighbors = traverse(&conn, "a.md", 1).expect("traverse");
            assert_eq!(neighbors.len(), 1);
        }

        // a.md no longer refs b.md.
        let a_updated = item("a.md", "A", "no longer links anywhere");
        update_index(&dest, &[a_updated], &[]).expect("update_index");

        let conn = open_index(&dest).expect("open index");
        let neighbors = traverse(&conn, "a.md", 1).expect("traverse after update");
        assert!(
            neighbors.is_empty(),
            "stale edge must not survive an update: {neighbors:?}"
        );
    }

    #[test]
    fn materialize_includes_only_the_exact_requested_tier() {
        let mut tier0 = item("t0.md", "Tier Zero", "tier zero content");
        tier0.frontmatter.item_type = Some(ItemType::ProjectState);
        let mut tier1 = item("t1.md", "Tier One", "tier one content");
        tier1.frontmatter.item_type = Some(ItemType::Feedback);
        let mut tier2 = item("t2.md", "Tier Two", "tier two content");
        tier2.frontmatter.item_type = Some(ItemType::Reference);
        let conn = build_in_memory(&[tier0, tier1, tier2]).expect("build in-memory index");

        let rendered0 = materialize(&conn, 0).expect("materialize tier 0");
        assert!(rendered0.starts_with("<wkp-context tier=\"0\">"));
        assert!(rendered0.ends_with("</wkp-context>\n"));
        assert!(rendered0.contains("tier zero content"));
        assert!(!rendered0.contains("tier one content"));
        assert!(!rendered0.contains("tier two content"));

        let rendered1 = materialize(&conn, 1).expect("materialize tier 1");
        assert!(rendered1.contains("tier one content"));
        assert!(!rendered1.contains("tier zero content"));
    }

    #[test]
    fn materialize_never_includes_inbox_items_in_tier_0_or_1() {
        // An inbox/ item claiming type: project-state would compute to
        // tier 0 by type alone; compute_tier's inbox/ exclusion (M1-6)
        // must override that, and materialize must reflect it -- this is
        // the M1-6 acceptance criterion boundary test.
        let mut smuggled = item(
            "inbox/import/claude-md.md",
            "Smuggled",
            "should never appear in tier 0 output",
        );
        smuggled.frontmatter.item_type = Some(ItemType::ProjectState);
        smuggled.frontmatter.confidence = Some(Confidence::Proposed);
        let conn = build_in_memory(&[smuggled]).expect("build in-memory index");

        let rendered0 = materialize(&conn, 0).expect("materialize tier 0");
        assert!(!rendered0.contains("should never appear"));
        let rendered1 = materialize(&conn, 1).expect("materialize tier 1");
        assert!(!rendered1.contains("should never appear"));

        // It's still reachable at tier 2 -- excluded from auto-injection,
        // not deleted or hidden from explicit search.
        let rendered2 = materialize(&conn, 2).expect("materialize tier 2");
        assert!(rendered2.contains("should never appear"));
    }

    /// Golden test (M1-8): locks down the exact `tier0.md` shape a harness's
    /// `SessionStart` hook `cat`s verbatim into the model's context --
    /// heading, source comment, body, ordering and blank-line spacing are
    /// all part of the contract, not just "contains" substring checks.
    /// Update this string deliberately if the format changes.
    #[test]
    fn materialize_tier0_output_is_locked_down() {
        let mut zebra = item("z.md", "Zebra Item", "zebra body");
        zebra.frontmatter.item_type = Some(ItemType::ProjectState);
        // Under user/ so type: instruction's M2-6 path-scoping rule
        // still lets it reach tier 0 -- this test is about materialize's
        // ordering/formatting, not the scoping rule itself (covered by
        // its own test), so the fixture just needs to stay eligible.
        let mut apple = item("user/a.md", "Apple Item", "apple body");
        apple.frontmatter.item_type = Some(ItemType::Instruction);
        let conn = build_in_memory(&[zebra, apple]).expect("build in-memory index");

        let rendered = materialize(&conn, 0).expect("materialize tier 0");
        assert_eq!(
            rendered,
            "<wkp-context tier=\"0\">\n\n\
             ## Apple Item\n\n<!-- source: user/a.md -->\n\napple body\n\n\
             ## Zebra Item\n\n<!-- source: z.md -->\n\nzebra body\n\n\
             </wkp-context>\n",
            "materialize must order items by path and use this exact heading/comment shape"
        );
    }

    /// Golden test (M1-8): a tier with no items still produces a
    /// well-formed, harness-safe wrapper -- `cat`-ing this must never
    /// inject a bare error or truncated tag into context.
    #[test]
    fn materialize_empty_tier_output_is_locked_down() {
        let conn = build_in_memory(&[]).expect("build in-memory index");
        let rendered = materialize(&conn, 0).expect("materialize empty tier 0");
        assert_eq!(rendered, "<wkp-context tier=\"0\">\n\n</wkp-context>\n");
    }

    #[test]
    fn hybrid_search_ranks_a_path_present_in_both_bm25_and_vector_results_first() {
        let mut a = item("a.md", "A", "distinctive keyword content");
        a.frontmatter.item_type = Some(ItemType::Knowledge);
        let mut b = item("b.md", "B", "distinctive keyword content too");
        b.frontmatter.item_type = Some(ItemType::Knowledge);
        let mut c = item("c.md", "C", "unrelated text with no overlap");
        c.frontmatter.item_type = Some(ItemType::Knowledge);
        let conn = build_in_memory(&[a, b, c]).expect("build in-memory index");

        // a.md and b.md both match the BM25 query; only a.md's embedding
        // is close to the query vector -- it should come out ranked first
        // by RRF fusion of the two signals, even though b.md alone might
        // win a BM25-only search on a shorter/more specific title.
        set_embedding(&conn, "a.md", &[1.0, 0.0, 0.0]).expect("set_embedding a.md");
        set_embedding(&conn, "b.md", &[0.0, 1.0, 0.0]).expect("set_embedding b.md");
        set_embedding(&conn, "c.md", &[0.0, 0.0, 1.0]).expect("set_embedding c.md");

        let hits = hybrid_search(
            &conn,
            "distinctive keyword",
            &[1.0, 0.0, 0.0],
            &SearchFilter::default(),
        )
        .expect("hybrid_search");
        assert_eq!(hits[0].path, "a.md");
        // c.md never matched the BM25 query and is the least similar
        // embedding (orthogonal to the query vector) -- it still appears
        // (every embedded item is ranked by *some* similarity, there is no
        // relevance floor, matching agent-wkp's own RRF semantics), but
        // strictly last.
        let paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
        assert_eq!(paths.last(), Some(&"c.md"));
    }

    #[test]
    fn hybrid_search_reports_no_matching_dimension_when_nothing_is_embedded() {
        let item = item("a.md", "A", "some content");
        let conn = build_in_memory(&[item]).expect("build in-memory index");

        let err = hybrid_search(&conn, "content", &[1.0, 0.0], &SearchFilter::default())
            .expect_err("expected NoMatchingEmbeddingDimension");
        match err {
            HybridSearchError::NoMatchingEmbeddingDimension {
                query_dim,
                stored_dims,
            } => {
                assert_eq!(query_dim, 2);
                assert!(stored_dims.is_empty());
            }
            other => panic!("expected NoMatchingEmbeddingDimension, got {other:?}"),
        }
    }

    #[test]
    fn hybrid_search_reports_the_stored_dimension_on_a_mismatch() {
        let item = item("a.md", "A", "some content");
        let conn = build_in_memory(&[item]).expect("build in-memory index");
        set_embedding(&conn, "a.md", &[1.0, 2.0, 3.0]).expect("set_embedding");

        // The index has 3-dim embeddings; querying with a 2-dim vector
        // (a different embedding model) must not silently compare them.
        let err = hybrid_search(&conn, "content", &[1.0, 0.0], &SearchFilter::default())
            .expect_err("expected NoMatchingEmbeddingDimension");
        match err {
            HybridSearchError::NoMatchingEmbeddingDimension {
                query_dim,
                stored_dims,
            } => {
                assert_eq!(query_dim, 2);
                assert_eq!(stored_dims, vec![3]);
            }
            other => panic!("expected NoMatchingEmbeddingDimension, got {other:?}"),
        }
    }

    #[test]
    fn hybrid_search_respects_the_tier_filter() {
        let mut hidden = item("hidden.md", "Hidden", "shared keyword content");
        hidden.frontmatter.item_type = Some(ItemType::Reference); // tier 2
        let conn = build_in_memory(&[hidden]).expect("build in-memory index");
        set_embedding(&conn, "hidden.md", &[1.0, 0.0]).expect("set_embedding");

        let filter = SearchFilter {
            tier: Some(0),
            ..Default::default()
        };
        let err = hybrid_search(&conn, "shared keyword", &[1.0, 0.0], &filter)
            .expect_err("tier 0 filter must exclude the only (tier-2) embedded item");
        assert!(matches!(
            err,
            HybridSearchError::NoMatchingEmbeddingDimension { .. }
        ));
    }

    #[test]
    fn set_embedding_on_an_unknown_path_is_a_harmless_no_op() {
        let conn = build_in_memory(&[]).expect("build in-memory index");
        set_embedding(&conn, "does-not-exist.md", &[1.0, 2.0]).expect("set_embedding");
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

    #[test]
    fn update_index_builds_fresh_when_dest_does_not_exist() {
        let dir = temp_db_dir("update-fresh");
        let dest = dir.path().join("index.db");
        let items = vec![item("a.md", "A", "first version")];

        update_index(&dest, &items, &[]).expect("update_index on missing dest");

        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "first", &SearchFilter::default()).expect("search");
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn update_index_adds_modifies_and_deletes_without_touching_others() {
        let dir = temp_db_dir("update-incremental");
        let dest = dir.path().join("index.db");
        let initial = vec![
            item("keep.md", "Keep", "untouched content"),
            item("edit.md", "Edit", "original content"),
            item("remove.md", "Remove", "goes away"),
        ];
        build_index(&dest, &initial).expect("initial build");

        let upserts = vec![
            item("edit.md", "Edit", "updated content"),
            item("new.md", "New", "brand new content"),
        ];
        update_index(&dest, &upserts, &["remove.md".to_string()]).expect("update_index");

        let conn = open_index(&dest).expect("open index");

        let all = search(&conn, "content", &SearchFilter::default()).expect("search");
        let mut paths: Vec<&str> = all.iter().map(|h| h.path.as_str()).collect();
        paths.sort_unstable();
        assert_eq!(paths, vec!["edit.md", "keep.md", "new.md"]);

        let updated = search(&conn, "updated", &SearchFilter::default()).expect("search");
        assert_eq!(updated.len(), 1);
        assert_eq!(updated[0].path, "edit.md");

        let original = search(&conn, "original", &SearchFilter::default()).expect("search");
        assert!(
            original.is_empty(),
            "stale content for a modified path must not remain queryable"
        );
    }

    #[test]
    fn update_index_upsert_on_existing_path_does_not_duplicate_rows() {
        let dir = temp_db_dir("update-no-dup");
        let dest = dir.path().join("index.db");
        let initial = vec![item("a.md", "A", "version one")];
        build_index(&dest, &initial).expect("initial build");

        update_index(&dest, &[item("a.md", "A", "version two")], &[]).expect("update_index");

        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "version", &SearchFilter::default()).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one row for a.md, got {hits:?}"
        );
        assert_eq!(hits[0].path, "a.md");
    }

    #[test]
    fn update_index_duplicate_path_within_one_batch_keeps_last_write() {
        let dir = temp_db_dir("update-batch-dup");
        let dest = dir.path().join("index.db");
        build_index(&dest, &[]).expect("initial empty build");

        // delete_item+insert_item per upsert means a path appearing twice
        // in one batch is safe (last write wins), not a UNIQUE-constraint
        // error -- worth locking down explicitly since it's a natural
        // thing for a caller to hit (e.g. a file that shows up in both
        // "modified" and "renamed-to" for the same underlying change).
        let upserts = vec![
            item("b.md", "B", "first write"),
            item("b.md", "B", "second write"),
        ];
        update_index(&dest, &upserts, &[]).expect("update_index with a duplicate path");

        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "write", &SearchFilter::default()).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "expected exactly one row for b.md, got {hits:?}"
        );
        let second = search(&conn, "second", &SearchFilter::default()).expect("search");
        assert_eq!(
            second.len(),
            1,
            "last write in the batch should be the one that sticks"
        );
    }

    #[test]
    fn failed_update_leaves_previous_index_db_untouched() {
        let dir = temp_db_dir("update-crash");
        let dest = dir.path().join("index.db");
        // A file that exists but isn't a valid SQLite database: VACUUM
        // INTO's read of it as the update's source fails immediately,
        // before the temp file is ever renamed over dest.
        std::fs::write(&dest, b"not a sqlite database").expect("seed non-sqlite dest");

        let result = update_index(&dest, &[item("c.md", "C", "content")], &[]);
        assert!(
            result.is_err(),
            "expected VACUUM INTO to fail on a non-sqlite source"
        );

        let bytes_after = std::fs::read(&dest).expect("read dest after failed update");
        assert_eq!(
            bytes_after, b"not a sqlite database",
            "a failed update must not modify or truncate the existing dest file"
        );
    }
}
