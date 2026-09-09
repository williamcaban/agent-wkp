//! BM25 search, trigram substring search, graph traversal, tier-aware
//! context assembly, and `wkp materialize`'s tier rendering (design
//! 5.1/5.3/5.4, M1-4/M1-5/M1-6).

use wkp_sys::rusqlite;

use super::schema::{IndexError, BM25_WEIGHTS};
use super::Connection;

#[derive(Debug, Default, Clone)]
pub struct SearchFilter {
    pub item_type: Option<String>,
    pub workspace: Option<String>,
    pub visibility: Option<String>,
    pub scope: Option<String>,
    /// Keep only items at or below this tier (0 is the most restrictive).
    /// See `compute_tier`'s doc comment for what "tier" means today and
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

pub(super) fn apply_budget(hits: Vec<SearchHit>, budget: Option<u32>) -> Vec<SearchHit> {
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

/// Used only by [`super::hybrid::hybrid_search`], which needs a fused
/// path's title/tier/tokens when its score came entirely from the vector
/// side (never matched BM25 at all, so `search`'s own result set never
/// carried this metadata for it).
pub(super) fn fetch_item_metadata(
    conn: &Connection,
    path: &str,
) -> Result<Option<(String, u8, u32)>, IndexError> {
    use wkp_sys::rusqlite::OptionalExtension;
    conn.query_row(
        "SELECT title, tier, tokens_estimate FROM items WHERE path = ?1",
        [path],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )
    .optional()
    .map_err(IndexError::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frontmatter::{Confidence, ItemType};
    use crate::index::store::build_in_memory;
    use crate::index::test_support::item;

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
        knowledge.frontmatter.visibility = Some(crate::frontmatter::Visibility::Shared);

        let mut inbox = item("i.md", "Proposed fact", "search is BM25 by default too");
        inbox.frontmatter.item_type = Some(ItemType::Memory);
        inbox.frontmatter.visibility = Some(crate::frontmatter::Visibility::Private);

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
}
