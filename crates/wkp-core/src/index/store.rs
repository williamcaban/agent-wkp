//! Building and incrementally updating `index.db` (design 5.1, 5.2):
//! everything that writes to the database, always via the
//! temp-file-then-rename pattern CLAUDE.md requires.

use std::path::{Path, PathBuf};

use wkp_sys::rusqlite;
use wkp_sys::rusqlite::OptionalExtension;

use super::schema::{create_schema, IndexError, Item};
use super::tier::{compute_tier, estimate_tokens};
use super::Connection;

fn insert_item(conn: &Connection, item: &Item) -> rusqlite::Result<()> {
    let fm = &item.frontmatter;
    let tier = compute_tier(&item.path, fm, item.human_signed);
    let tokens_estimate = estimate_tokens(fm, &item.body);
    conn.execute("INSERT INTO paths (path) VALUES (?1)", [&item.path])?;
    // `paths.path` is a real B-tree (indexed) lookup; `items`/`items_trigram`
    // are FTS5 tables whose non-full-text columns (`path UNINDEXED`) are
    // *not* indexed for equality lookups -- a `WHERE path = ?` against them
    // is a full-corpus scan (benchmarked ~28ms/~17ms respectively at 50k
    // items, the actual bottleneck behind ADR-0002's incremental-index miss,
    // not the temp-file-copy mechanism ADR-0002 originally suspected).
    // Explicitly assigning `paths`' own auto-assigned rowid as both FTS5
    // tables' rowid lets `delete_item` delete by rowid instead -- an indexed,
    // O(1) lookup -- while `paths.path` still gives every other caller a
    // real indexed way to find that rowid from a path.
    let rowid = conn.last_insert_rowid();
    let embedding_bytes = item
        .embedding
        .as_ref()
        .map(|v| crate::embed::serialize_embedding(v));
    let embedding_dim = item.embedding.as_ref().map(|v| v.len() as i64);
    conn.execute(
        "INSERT INTO items (
            rowid, path, title, content, tags, item_type, workspace, visibility,
            scope, confidence, provenance_source, tokens, updated, expires,
            refs, tier, tokens_estimate, embedding, embedding_dim
        ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19)",
        rusqlite::params![
            rowid,
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
        "INSERT INTO items_trigram (rowid, path, content) VALUES (?1, ?2, ?3)",
        rusqlite::params![rowid, item.path, trigram_content],
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
    // Look up the rowid via `paths`' real B-tree index on `path`, then
    // delete from both FTS5 tables by rowid rather than by `path` -- see
    // `insert_item`'s comment on why an unindexed FTS5 `WHERE path = ?`
    // delete is an O(corpus) scan, not the O(1) this needs.
    let rowid: Option<i64> = conn
        .query_row("SELECT rowid FROM paths WHERE path = ?1", [path], |row| {
            row.get(0)
        })
        .optional()?;
    if let Some(rowid) = rowid {
        conn.execute("DELETE FROM paths WHERE rowid = ?1", [rowid])?;
        conn.execute("DELETE FROM items WHERE rowid = ?1", [rowid])?;
        conn.execute("DELETE FROM items_trigram WHERE rowid = ?1", [rowid])?;
    }
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
/// If `dest` doesn't exist yet, this builds a fresh index instead via
/// [`build_index`] (temp-file-then-rename, same as any first-ever build).
///
/// If `dest` exists, per ADR-0002 (accepted): `index.db` is the one named
/// exception to CLAUDE.md's temp-and-rename hard rule, since SQLite's own
/// transaction log already gives a reader the same "never see a torn
/// file, a crash rolls back cleanly" guarantee that rule exists to
/// provide for a plain file with no engine underneath it. The changes are
/// applied directly to `dest` inside one `BEGIN IMMEDIATE` transaction,
/// committed only once every delete/upsert/edge-recompute step succeeds;
/// any failure (including losing a race for `dest`'s write lock) drops the
/// transaction, which SQLite rolls back on its own, leaving `dest` exactly
/// as it was. This replaces the previous `VACUUM INTO`-based copy, which
/// cloned and compacted the *entire* file on every call regardless of
/// change count — benchmarked at ~460ms for a 10-item change against a
/// 50k-item corpus, missing design 4.3's incremental-index target by
/// roughly an order of magnitude.
///
/// Removing that copy alone only got this to ~370ms, not the target:
/// profiling found the real cost was `delete_item`'s `WHERE path = ?`
/// deletes against `items`/`items_trigram`, both FTS5 tables whose `path`
/// column is `UNINDEXED` (excluded from the full-text index, so equality
/// lookups against it are a full-table scan, not an indexed one) —
/// ~28ms/~17ms per delete at 50k items, the dominant cost either way this
/// function writes `dest`. `insert_item` now assigns each row the same
/// rowid `paths` (a real B-tree, indexed by `path`) already auto-assigned
/// it, so `delete_item` deletes from both FTS5 tables by rowid instead —
/// O(1), not O(corpus). Cost here is now genuinely proportional to the
/// number of changed rows, not corpus size.
pub fn update_index(
    dest: &Path,
    upserts: &[Item],
    deleted_paths: &[String],
) -> Result<(), IndexError> {
    if !dest.exists() {
        return build_index(dest, upserts);
    }

    let mut conn = wkp_sys::open(dest)?;
    let tx = conn.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;

    for path in deleted_paths {
        delete_item(&tx, path)?;
    }
    for item in upserts {
        // An upsert on a path already present must replace, not
        // duplicate, its row -- clear it first regardless of whether
        // the caller classified it as "added" or "modified".
        delete_item(&tx, &item.path)?;
        insert_item(&tx, item)?;
    }
    // Same reasoning as `populate`: edges recomputed only after every
    // upserted item's content row exists, so a wikilink in one changed
    // item can resolve against another changed item in the same batch.
    // Unchanged items keep whatever edges the transaction hasn't touched.
    for item in upserts {
        insert_edges_for_item(&tx, item)?;
    }

    tx.commit()?;
    Ok(())
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::ItemType;
    use crate::index::search::{search, SearchFilter};
    use crate::index::test_support::{item, temp_db_dir};

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
    fn edges_are_recomputed_when_an_item_is_updated() {
        let dir = temp_db_dir("edges-update");
        let dest = dir.path().join("index.db");
        let mut a = item("a.md", "A", "first version");
        a.frontmatter.refs = vec!["b.md".to_string()];
        let b = item("b.md", "B", "target");
        build_index(&dest, &[a, b]).expect("initial build");

        {
            let conn = open_index(&dest).expect("open index");
            let neighbors = crate::index::search::traverse(&conn, "a.md", 1).expect("traverse");
            assert_eq!(neighbors.len(), 1);
        }

        // a.md no longer refs b.md.
        let a_updated = item("a.md", "A", "no longer links anywhere");
        update_index(&dest, &[a_updated], &[]).expect("update_index");

        let conn = open_index(&dest).expect("open index");
        let neighbors =
            crate::index::search::traverse(&conn, "a.md", 1).expect("traverse after update");
        assert!(
            neighbors.is_empty(),
            "stale edge must not survive an update: {neighbors:?}"
        );
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
        // A file that exists but isn't a valid SQLite database: the very
        // first statement inside update_index's transaction fails trying
        // to read it, before anything is ever committed to dest.
        std::fs::write(&dest, b"not a sqlite database").expect("seed non-sqlite dest");

        let result = update_index(&dest, &[item("c.md", "C", "content")], &[]);
        assert!(
            result.is_err(),
            "expected update_index to fail against a non-sqlite dest"
        );

        let bytes_after = std::fs::read(&dest).expect("read dest after failed update");
        assert_eq!(
            bytes_after, b"not a sqlite database",
            "a failed update must not modify or truncate the existing dest file"
        );
    }

    /// ADR-0002's own consequence: now that `update_index` writes `dest`
    /// directly instead of copying it first, a failure genuinely
    /// mid-transaction (not just an upfront corrupt file) must still roll
    /// back cleanly. Holding `dest`'s write lock from a second connection
    /// forces `update_index`'s own `BEGIN IMMEDIATE` to fail acquiring it
    /// (default busy_timeout is 0, so this fails immediately rather than
    /// hanging) -- a real contention failure, not a simulated one.
    #[test]
    fn update_index_rolls_back_cleanly_when_it_cannot_acquire_the_write_lock() {
        let dir = temp_db_dir("update-locked");
        let dest = dir.path().join("index.db");
        build_index(&dest, &[item("a.md", "A", "first version")]).expect("initial build");
        let original_bytes = std::fs::read(&dest).expect("read original index.db");

        let blocker = wkp_sys::open(&dest).expect("open blocking connection");
        blocker
            .execute_batch("BEGIN IMMEDIATE;")
            .expect("acquire dest's write lock from the blocking connection");

        let result = update_index(&dest, &[item("b.md", "B", "second item")], &[]);
        assert!(
            result.is_err(),
            "expected update_index to fail while dest's write lock is held elsewhere"
        );

        drop(blocker);

        let bytes_after = std::fs::read(&dest).expect("read dest after failed update");
        assert_eq!(
            original_bytes, bytes_after,
            "a failed update must not modify the previously published index.db"
        );

        let conn = open_index(&dest).expect("open index");
        let hits = search(&conn, "first", &SearchFilter::default()).expect("search");
        assert_eq!(
            hits.len(),
            1,
            "previous content must still be queryable after the rollback"
        );
    }
}
