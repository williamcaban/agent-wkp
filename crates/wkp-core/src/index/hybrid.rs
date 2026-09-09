//! Optional hybrid (BM25 + vector) search (design 5.3, M1-9).

use std::fmt;

use wkp_sys::rusqlite;

use super::schema::IndexError;
use super::search::{apply_budget, fetch_item_metadata, search, SearchFilter, SearchHit};
use super::Connection;

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::ItemType;
    use crate::index::store::build_in_memory;
    use crate::index::test_support::item;

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
}
