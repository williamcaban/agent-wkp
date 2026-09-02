"""Hybrid search (vector + FTS RRF) and graph traversal queries."""
from __future__ import annotations

import sqlite3
from dataclasses import dataclass

import numpy as np


@dataclass
class SearchResult:
    path: str
    title: str | None
    type: str | None
    workspace: str | None
    tokens: int | None
    tier: int | None
    score: float
    hop_distance: int = 0


def _embed_query(text: str) -> bytes:
    from .indexer import _get_model

    vec = _get_model().encode(text, normalize_embeddings=True)
    return np.array(vec, dtype=np.float32).tobytes()


def search(
    conn: sqlite3.Connection,
    query: str,
    *,
    workspaces: list[str] | None = None,
    max_tier: int = 3,
    token_budget: int | None = None,
    k: int = 10,
) -> list[SearchResult]:
    """Hybrid vector + keyword search via Reciprocal Rank Fusion."""
    query_vec = _embed_query(query)
    workspace_filter = (
        f"AND ki.workspace IN ({','.join('?' * len(workspaces))})"
        if workspaces
        else ""
    )
    tier_filter = f"AND ki.tier <= {int(max_tier)}"
    token_filter = f"AND (ki.tokens IS NULL OR ki.tokens <= {int(token_budget)})" if token_budget else ""

    params_vec: list = [query_vec]
    params_fts: list = [query]
    params_join: list = [*((workspaces or []))]

    sql = f"""
    WITH vec_hits AS (
        SELECT ki.path,
               ROW_NUMBER() OVER (ORDER BY kv.distance) AS rank
        FROM knowledge_vec kv
        JOIN knowledge_items ki ON ki.rowid = kv.rowid
        WHERE kv.embedding MATCH ?
          AND kv.k = {k * 2}
    ),
    fts_hits AS (
        SELECT ki.path,
               ROW_NUMBER() OVER (ORDER BY knowledge_fts.rank DESC) AS rank
        FROM knowledge_fts
        JOIN knowledge_items ki ON ki.rowid = knowledge_fts.rowid
        WHERE knowledge_fts MATCH ?
        LIMIT {k * 2}
    ),
    rrf AS (
        SELECT path, SUM(1.0 / (60.0 + rank)) AS score
        FROM (
            SELECT path, rank FROM vec_hits
            UNION ALL
            SELECT path, rank FROM fts_hits
        )
        GROUP BY path
    )
    SELECT ki.path, ki.title, ki.type, ki.workspace,
           ki.tokens, ki.tier, rrf.score
    FROM rrf
    JOIN knowledge_items ki USING (path)
    WHERE ki.visibility = 'shared'
    {workspace_filter}
    {tier_filter}
    {token_filter}
    ORDER BY rrf.score DESC
    LIMIT {k}
    """

    rows = conn.execute(sql, params_vec + params_fts + params_join).fetchall()
    return [
        SearchResult(
            path=r["path"],
            title=r["title"],
            type=r["type"],
            workspace=r["workspace"],
            tokens=r["tokens"],
            tier=r["tier"],
            score=r["score"],
        )
        for r in rows
    ]


def traverse(
    conn: sqlite3.Connection,
    start_path: str,
    *,
    max_depth: int = 3,
    token_budget: int | None = None,
    edge_types: list[str] | None = None,
) -> list[SearchResult]:
    """BFS traversal from start_path following explicit edges."""
    types_filter = (
        f"AND e.edge_type IN ({','.join('?' * len(edge_types))})"
        if edge_types
        else ""
    )
    token_filter = f"AND (ki.tokens IS NULL OR ki.tokens <= {token_budget})" if token_budget else ""

    params: list = [start_path, max_depth, *(edge_types or [])]

    sql = f"""
    WITH RECURSIVE reachable(path, depth) AS (
        SELECT ?, 0
        UNION ALL
        SELECT e.target_path, r.depth + 1
        FROM knowledge_edges e
        JOIN reachable r ON e.source_path = r.path
        WHERE r.depth < ?
        {types_filter}
    )
    SELECT ki.path, ki.title, ki.type, ki.workspace,
           ki.tokens, ki.tier, MIN(r.depth) AS depth
    FROM reachable r
    JOIN knowledge_items ki ON ki.path = r.path
    WHERE ki.path != ?
    {token_filter}
    GROUP BY ki.path
    ORDER BY depth, ki.tier, ki.tokens
    """

    rows = conn.execute(sql, params + [start_path]).fetchall()
    return [
        SearchResult(
            path=r["path"],
            title=r["title"],
            type=r["type"],
            workspace=r["workspace"],
            tokens=r["tokens"],
            tier=r["tier"],
            score=1.0 / (1 + r["depth"]),
            hop_distance=r["depth"],
        )
        for r in rows
    ]


def context_assemble(
    conn: sqlite3.Connection,
    topic: str,
    *,
    tier: int = 2,
    token_budget: int = 8000,
    workspaces: list[str] | None = None,
) -> list[SearchResult]:
    """
    Tier-aware context assembly: combine search + traversal, deduplicate,
    enforce token budget. Returns items ordered for injection.
    """
    seen: set[str] = set()
    assembled: list[SearchResult] = []
    remaining = token_budget

    # Semantic search first
    hits = search(conn, topic, workspaces=workspaces, max_tier=tier, k=20)
    for hit in hits:
        cost = hit.tokens or 500
        if hit.path not in seen and cost <= remaining:
            seen.add(hit.path)
            assembled.append(hit)
            remaining -= cost

    # Traverse refs from top search results (depth 2)
    for hit in assembled[:3]:
        neighbors = traverse(conn, hit.path, max_depth=2, token_budget=remaining)
        for n in neighbors:
            cost = n.tokens or 500
            if n.path not in seen and (n.tier is None or n.tier <= tier) and cost <= remaining:
                seen.add(n.path)
                assembled.append(n)
                remaining -= cost

    return assembled
