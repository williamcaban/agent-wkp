"""NetworkX analytics: PageRank-based tier promotion, community detection.

Loaded on-demand only — not in the hot path for search or indexing.

CLI surface:
  find_communities() and impact_paths() are library-only for now.
  Exposed via CLI: suggest_tier_promotions → `wkp analyze`
  Planned: `wkp impact <path>` and `wkp communities`
"""
from __future__ import annotations

import sqlite3
from dataclasses import dataclass


@dataclass
class TierPromotion:
    path: str
    title: str | None
    current_tier: int
    pagerank_score: float
    reason: str


def build_graph(conn: sqlite3.Connection):  # -> nx.DiGraph (avoid import at module level)
    """Load knowledge graph from SQLite into a NetworkX DiGraph."""
    import networkx as nx

    G: nx.DiGraph = nx.DiGraph()

    for row in conn.execute(
        "SELECT path, type, workspace, tokens, tier FROM knowledge_items"
    ):
        G.add_node(
            row["path"],
            type=row["type"],
            workspace=row["workspace"],
            tokens=row["tokens"],
            tier=row["tier"],
        )

    for row in conn.execute(
        "SELECT source_path, target_path, edge_type, weight FROM knowledge_edges"
    ):
        if G.has_node(row["source_path"]) and G.has_node(row["target_path"]):
            G.add_edge(
                row["source_path"],
                row["target_path"],
                type=row["edge_type"],
                weight=row["weight"],
            )

    return G


def suggest_tier_promotions(
    conn: sqlite3.Connection,
    top_n: int = 10,
) -> list[TierPromotion]:
    """
    Run PageRank on the knowledge graph and suggest Tier 2 items
    that are highly referenced and warrant promotion to Tier 1.
    """
    import networkx as nx

    G = build_graph(conn)
    if G.number_of_nodes() == 0:
        return []

    scores = nx.pagerank(G, weight="weight")

    candidates: list[TierPromotion] = []
    for path, score in sorted(scores.items(), key=lambda x: x[1], reverse=True):
        node = G.nodes[path]
        if node.get("tier", 2) > 1:
            in_degree = G.in_degree(path)
            title = conn.execute(
                "SELECT title FROM knowledge_items WHERE path = ?", (path,)
            ).fetchone()
            candidates.append(
                TierPromotion(
                    path=path,
                    title=title["title"] if title else None,
                    current_tier=node.get("tier", 2),
                    pagerank_score=score,
                    reason=f"referenced by {in_degree} items",
                )
            )
        if len(candidates) >= top_n:
            break

    return candidates


def find_communities(conn: sqlite3.Connection) -> list[list[str]]:
    """
    Detect communities in the knowledge graph (undirected).
    Returns list of node-path lists; each inner list is a community.
    Useful for spotting topics that might warrant a new sub-workspace.
    """
    import networkx as nx

    G = build_graph(conn).to_undirected()
    if G.number_of_nodes() == 0:
        return []

    communities = nx.community.greedy_modularity_communities(G)
    return [sorted(c) for c in communities]


def impact_paths(
    conn: sqlite3.Connection,
    changed_path: str,
    max_depth: int = 3,
) -> list[str]:
    """
    Return all items that transitively reference changed_path.
    Used by the post-commit hook to flag items needing review.
    """
    sql = """
    WITH RECURSIVE impacted(path, depth) AS (
        SELECT ?, 0
        UNION ALL
        SELECT e.source_path, i.depth + 1
        FROM knowledge_edges e
        JOIN impacted i ON e.target_path = i.path
        WHERE i.depth < ?
    )
    SELECT DISTINCT path FROM impacted WHERE path != ?
    ORDER BY depth
    """
    rows = conn.execute(sql, (changed_path, max_depth, changed_path)).fetchall()
    return [r["path"] for r in rows]
