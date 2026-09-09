//! The only crate in this workspace permitted `unsafe` code, per design 3.3
//! and the CLAUDE.md hard rule (`#![forbid(unsafe_code)]` everywhere else).
//!
//! Owns the one dependency on bundled SQLite (design 5.3: "statically
//! bundle SQLite... rather than linking the OS library", so FTS5 and the
//! `trigram` tokenizer are identical on every machine that rebuilds the
//! index). `rusqlite`'s public API is itself safe Rust, so no `unsafe`
//! block exists here yet; this crate is still where the dependency and any
//! future low-level FFI needs are meant to live, per design 3.3, so that
//! `wkp-core` and every other crate can keep `#![forbid(unsafe_code)]`
//! literal rather than as an aspiration.
//!
//! `wkp-core` depends on this crate rather than on `rusqlite` directly, so
//! there is exactly one place in the workspace that decides how SQLite is
//! linked and opened.

pub use rusqlite;

use std::path::Path;

/// Opens (creating if absent) a SQLite database file with the pragmas this
/// project always wants: foreign keys enforced. Journal mode is left at
/// SQLite's default (rollback journal, not WAL) deliberately — the index is
/// always rebuilt into a fresh temp file and swapped into place with
/// `rename(2)` (CLAUDE.md hard rule), and a WAL's `-wal`/`-shm` sidecar
/// files would complicate that single-file atomic swap for no benefit,
/// since nothing ever writes to `index.db` concurrently with a reader.
pub fn open(path: &Path) -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open(path)?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

/// Opens a private, in-memory database. Used by tests and by anything that
/// wants FTS5 query semantics without touching disk.
pub fn open_in_memory() -> rusqlite::Result<rusqlite::Connection> {
    let conn = rusqlite::Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", true)?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn opens_in_memory_and_creates_an_fts5_table() {
        let conn = open_in_memory().expect("open in-memory db");
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(body)")
            .expect("fts5 must be compiled in");
        conn.execute("INSERT INTO t(body) VALUES (?1)", ["hello world"])
            .expect("insert");
        let count: i64 = conn
            .query_row("SELECT count(*) FROM t WHERE t MATCH 'hello'", [], |row| {
                row.get(0)
            })
            .expect("fts5 match query");
        assert_eq!(count, 1);
    }

    #[test]
    fn trigram_tokenizer_is_available() {
        let conn = open_in_memory().expect("open in-memory db");
        conn.execute_batch("CREATE VIRTUAL TABLE t USING fts5(body, tokenize='trigram')")
            .expect("trigram tokenizer must be compiled in");
        conn.execute("INSERT INTO t(body) VALUES (?1)", ["src/wkp-git/lib.rs"])
            .expect("insert");
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM t WHERE t MATCH '\"wkp-git\"'",
                [],
                |row| row.get(0),
            )
            .expect("trigram substring match query");
        assert_eq!(count, 1);
    }
}
