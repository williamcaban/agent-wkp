"""Hybrid search (vector + FTS RRF) and graph traversal queries."""
from __future__ import annotations

import re
import sqlite3
from dataclasses import dataclass
from typing import Any

import numpy as np

_FTS5_SPECIAL = re.compile(r'[()"\*\.\:\-]+')


def _sanitize_fts5(query: str) -> str:
    """Strip FTS5 special characters (e.g. dots in version numbers) to prevent syntax errors."""
    cleaned = _FTS5_SPECIAL.sub(' ', query).strip()
    return cleaned or '""'


@dataclass
class EmbedConfig:
    """Configuration for an OpenAI-compatible embeddings endpoint."""
    url: str
    api_key: str | None = None
    model: str | None = None

    def cache_key(self, text: str) -> str:
        return f"{self.url}|{self.model or ''}|{text}"


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


def _embed_remote(text: str, config: EmbedConfig, conn: sqlite3.Connection | None = None) -> bytes:
    """Embed text via OpenAI-compatible /v1/embeddings endpoint, with SQLite cache.

    API key is read from config.api_key (set from WKP_EMBED_API_KEY env var by the CLI).
    Prefer the env var over --embed-api-key to avoid the key appearing in shell history.
    """
    cache_key = config.cache_key(text)

    if conn is not None:
        row = conn.execute(
            "SELECT embedding FROM query_cache WHERE query = ?", (cache_key,)
        ).fetchone()
        if row:
            return bytes(row[0])

    try:
        import httpx
    except ImportError as e:
        raise ImportError(
            "httpx is required for remote embeddings. "
            "Install with: pip install 'agent-wkp[embed]'"
        ) from e

    url = config.url.rstrip('/') + '/embeddings'
    headers: dict[str, str] = {'Content-Type': 'application/json'}
    if config.api_key:
        headers['Authorization'] = f'Bearer {config.api_key}'

    body: dict[str, Any] = {'input': text}
    if config.model:
        body['model'] = config.model

    resp = httpx.post(url, json=body, headers=headers, timeout=30.0)
    resp.raise_for_status()
    data = resp.json()
    vec = data['data'][0]['embedding']
    emb = np.array(vec, dtype=np.float32).tobytes()

    if conn is not None:
        conn.execute(
            "INSERT OR REPLACE INTO query_cache(query, embedding) VALUES (?, ?)",
            (cache_key, emb),
        )
        conn.commit()

    return emb


def _embed_query(text: str, conn: sqlite3.Connection | None = None) -> bytes:
    """Embed text using the local sentence-transformers model, with SQLite cache.

    Used by the indexer and as a fallback. Loads the model on first call (slow).
    Prefer remote embeddings via EmbedConfig for zero-latency search.
    """
    cache_key = f"local|{text}"

    if conn is not None:
        row = conn.execute(
            "SELECT embedding FROM query_cache WHERE query = ?", (cache_key,)
        ).fetchone()
        if row:
            return bytes(row[0])

    from .indexer import _get_model

    vec = _get_model().encode(text, normalize_embeddings=True)
    emb = np.array(vec, dtype=np.float32).tobytes()

    if conn is not None:
        conn.execute(
            "INSERT OR REPLACE INTO query_cache(query, embedding) VALUES (?, ?)",
            (cache_key, emb),
        )
        conn.commit()

    return emb


def search(
    conn: sqlite3.Connection,
    query: str,
    *,
    embed_config: EmbedConfig | None = None,
    workspaces: list[str] | None = None,
    max_tier: int = 3,
    token_budget: int | None = None,
    k: int = 10,
) -> list[SearchResult]:
    """Search the knowledge index.

    Default (embed_config=None): BM25 keyword search via FTS5 — instant, no model load.
    With embed_config: hybrid vector + BM25 via RRF. Falls back to FTS5-only if the
    vector index dimension mismatches the embedding model (e.g. different model at index time).
    """
    workspace_filter = (
        f"AND ki.workspace IN ({','.join('?' * len(workspaces))})"
        if workspaces
        else ""
    )
    tier_filter = f"AND ki.tier <= {int(max_tier)}"
    token_filter = (
        f"AND (ki.tokens IS NULL OR ki.tokens <= {int(token_budget)})" if token_budget else ""
    )
    fts_query = _sanitize_fts5(query)
    params_join: list = [*(workspaces or [])]

    rows = None

    if embed_config is not None:
        try:
            query_vec = _embed_remote(query, embed_config, conn)
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
            rows = conn.execute(sql, [query_vec, fts_query] + params_join).fetchall()
        except Exception as exc:
            import sys
            exc_str = str(exc)
            if "dimension" in exc_str.lower() or "float[" in exc_str:
                detail = (
                    "embedding dimension mismatch — the remote model produces a different "
                    "dimension than the index (built with all-MiniLM-L6-v2, 384-dim). "
                    "Re-index with the same model or omit --embed-url."
                )
            elif isinstance(exc, ImportError):
                detail = "httpx not installed — run: pip install 'agent-wkp[embed]'"
            else:
                detail = exc_str
            print(
                f"wkp: warning: semantic search unavailable ({type(exc).__name__}): {detail}\n"
                f"     Falling back to BM25 keyword search.",
                file=sys.stderr,
            )
            rows = None

    if rows is None:
        # FTS5-only (BM25): default path — no model load, instant
        sql = f"""
        SELECT ki.path, ki.title, ki.type, ki.workspace,
               ki.tokens, ki.tier,
               (-knowledge_fts.rank) AS score
        FROM knowledge_fts
        JOIN knowledge_items ki ON ki.rowid = knowledge_fts.rowid
        WHERE knowledge_fts MATCH ?
          AND ki.visibility = 'shared'
          {tier_filter}
          {token_filter}
          {workspace_filter}
        ORDER BY knowledge_fts.rank DESC
        LIMIT {k}
        """
        rows = conn.execute(sql, [fts_query] + params_join).fetchall()

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
    embed_config: EmbedConfig | None = None,
) -> list[SearchResult]:
    """
    Tier-aware context assembly: combine search + traversal, deduplicate,
    enforce token budget. Returns items ordered for injection.
    """
    seen: set[str] = set()
    assembled: list[SearchResult] = []
    remaining = token_budget

    hits = search(
        conn, topic,
        embed_config=embed_config,
        workspaces=workspaces,
        max_tier=tier,
        k=20,
    )
    for hit in hits:
        cost = hit.tokens or 500
        if hit.path not in seen and cost <= remaining:
            seen.add(hit.path)
            assembled.append(hit)
            remaining -= cost

    for hit in assembled[:3]:
        neighbors = traverse(conn, hit.path, max_depth=2, token_budget=remaining)
        for n in neighbors:
            cost = n.tokens or 500
            if n.path not in seen and (n.tier is None or n.tier <= tier) and cost <= remaining:
                seen.add(n.path)
                assembled.append(n)
                remaining -= cost

    return assembled
