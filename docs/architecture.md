# WKP Architecture

## Design principles

**1. Tier 0 is structurally guaranteed, never retrieved.**
Tier 0 context is written to a static file at index time and injected by a SessionStart hook before the first model token. It cannot be "missed" by a failing search. No semantic system can exclude it.

**2. Skills + CLI beat MCP tools for context loading.**
MCP tools consume the model's tool quota (empirically degrades past ~10 tools) and add round-trip latency. WKP exposes itself as a CLI (`wkp`), called via Bash — a tool already in the tool list. Zero additional tool definitions required.

**3. Git blob SHA is the cache invalidation key.**
No polling, no file watchers, no timestamps. `git hash-object <file>` returns a content-addressed SHA. If the SHA matches the stored value, skip re-embedding. The post-commit hook runs incremental indexing only on changed files.

**4. SQLite is the only database.**
`sqlite-vec` adds vector similarity search (ANN via HNSW) to SQLite as a loadable extension (~300KB). FTS5 (full-text search with BM25) is built into every SQLite distribution. The graph (explicit `refs:` links) is stored in a regular table with recursive CTE traversal. The result is a single file (`.wkp/index.db`) with no server, no daemon, no port.

**5. NetworkX is optional and analytics-only.**
Graph analytics (PageRank, community detection) load NetworkX from the SQLite edges table on demand. They run only during `wkp analyze` — never in the search or context assembly hot path.

---

## Layer diagram

```
┌──────────────────────────────────────────────────────────────────┐
│  Agent frameworks                                                 │
│  Claude Code │ OpenCode │ Hermes │ OpenClaw │ ...                │
└──────┬────────┴──────┬───┴───┬────┴──────┬───┴───────────────────┘
       │               │       │            │
       │  SessionStart hook    │            │  (each framework reads
       │  cat .wkp/tier0.md   │            │   its own native config)
       │                       │            │
       └───────────────────────┴────────────┘
                               │
                    bash: wkp search "topic"
                    bash: wkp context "topic" --tier 2
                               │
                    ┌──────────▼──────────┐
                    │    wkp CLI           │  Click commands
                    │    (wkp/cli.py)      │
                    └──────────┬──────────┘
                               │
              ┌────────────────┼────────────────┐
              │                │                │
    ┌─────────▼──────┐ ┌───────▼──────┐ ┌──────▼────────┐
    │ retriever.py   │ │ indexer.py   │ │materializer.py│
    │ hybrid search  │ │ embed+upsert │ │ tier assembly │
    │ RRF + traverse │ │ git SHA check│ │ hook gen      │
    └─────────┬──────┘ └───────┬──────┘ └───────────────┘
              │                │
              └────────┬───────┘
                       │
              ┌────────▼────────┐
              │  db.py          │  SQLite + sqlite-vec
              │  .wkp/index.db  │
              │                 │
              │  knowledge_items│  OKF metadata mirror
              │  knowledge_vec  │  float[384] embeddings (HNSW)
              │  knowledge_fts  │  BM25 full-text (FTS5)
              │  knowledge_edges│  explicit refs graph
              └─────────────────┘
                       │
              ┌────────▼────────┐
              │  graph.py       │  NetworkX (analytics only)
              │  PageRank       │  loaded on-demand from edges table
              │  communities    │
              └─────────────────┘
```

---

## Database schema

### `knowledge_items` — metadata mirror

```sql
CREATE TABLE knowledge_items (
    path         TEXT PRIMARY KEY,       -- repo-relative path
    git_blob_sha TEXT NOT NULL DEFAULT '',  -- cache invalidation key
    title        TEXT,
    type         TEXT,                   -- OKF type field
    workspace    TEXT,                   -- OKF workspace field
    visibility   TEXT DEFAULT 'shared',
    tokens       INTEGER,                -- OKF token estimate
    tags         TEXT,                   -- JSON array
    refs         TEXT,                   -- JSON array (raw OKF; normalised into edges)
    updated      TEXT,
    content      TEXT,                   -- body text for FTS and snippet generation
    tier INTEGER GENERATED ALWAYS AS (
        CASE type
            WHEN 'feedback'      THEN 1
            WHEN 'project-state' THEN 1
            WHEN 'skill'         THEN 1
            ELSE 2
        END
    ) STORED
);
```

The `tier` column is a deterministic computed column — it never needs updating when the type mapping is fixed.

### `knowledge_vec` — vector index

```sql
CREATE VIRTUAL TABLE knowledge_vec USING vec0(embedding float[384]);
```

sqlite-vec implements approximate nearest-neighbour search (HNSW) on this table. Joined to `knowledge_items` via `rowid`. Queried with:

```sql
SELECT rowid, distance
FROM knowledge_vec
WHERE embedding MATCH vec_f32(?)
  AND k = 20
ORDER BY distance
```

### `knowledge_fts` — full-text search

```sql
CREATE VIRTUAL TABLE knowledge_fts USING fts5(
    path UNINDEXED, title, content, tags,
    content=knowledge_items, content_rowid=rowid
);
```

FTS5 content table — no data duplication. Triggers on `knowledge_items` keep it in sync. BM25 ranking via the `rank` column.

### `knowledge_edges` — explicit graph

```sql
CREATE TABLE knowledge_edges (
    source_path TEXT NOT NULL,
    target_path TEXT NOT NULL,
    edge_type   TEXT NOT NULL,  -- 'refs' | 'depends_on' | 'mentions'
    weight      REAL DEFAULT 1.0,
    PRIMARY KEY (source_path, target_path, edge_type)
);
```

Edges come from:
- OKF `refs:` field → `edge_type = 'refs'`
- `[[wikilink]]` patterns in body → `edge_type = 'mentions'`
- Skill manifest `dependencies` → `edge_type = 'depends_on'`

Traversal uses SQLite recursive CTEs (no graph library required at query time):

```sql
WITH RECURSIVE reachable(path, depth) AS (
    SELECT ?, 0
    UNION ALL
    SELECT e.target_path, r.depth + 1
    FROM knowledge_edges e
    JOIN reachable r ON e.source_path = r.path
    WHERE r.depth < ?
)
SELECT ki.* FROM reachable r JOIN knowledge_items ki ON ki.path = r.path
ORDER BY r.depth, ki.tier, ki.tokens;
```

---

## Retrieval: Hybrid RRF

Search combines two signals via Reciprocal Rank Fusion (k=60):

```
score(item) = Σ  1 / (60 + rank_in_signal)
              signals
```

- **Signal 1 (semantic)**: vector similarity via `knowledge_vec` ANN search
- **Signal 2 (keyword)**: BM25 via `knowledge_fts` MATCH

Both signals run in a single SQL query with a CTE. No application-layer merge. Post-RRF filtering applies tier ceiling, visibility, and token budget constraints.

After search, the `context_assemble` function additionally traverses explicit edges from the top-3 results (depth 2) to pull in directly-referenced items within the remaining token budget.

---

## Git integration

### Cache invalidation

```python
sha = subprocess.run(["git", "hash-object", path], ...).stdout.strip()
if db.get_blob_sha(path) == sha:
    return  # skip — content unchanged
```

### Post-commit hook flow

```
git commit
  → post-commit hook
      → git diff --name-only HEAD~1 HEAD | grep '\.md$'
      → wkp index [changed files]   # re-embeds only changed files
      → wkp materialize --tier 0    # regenerates static context file
```

### SessionStart hook flow

```
Session start
  → .claude/hooks/wkp-session-start.sh
      → cat .wkp/tier0.md
      → stdout injected as <wkp-context tier="0"> block
         before first prompt
```

Tier 0 injection has zero latency (file read, no subprocess, no network) because the file is pre-assembled.

---

## OKF Frontmatter standard

Every knowledge file should carry OKF frontmatter:

```yaml
---
title: Short descriptive title
type: reference          # project-state | knowledge | reference | feedback | skill
workspace: root          # logical sub-workspace name
visibility: shared       # shared | private
tokens: ~800             # rough token estimate
tags: [tag1, tag2]
refs:
  - ../other-workspace/knowledge/related.md
updated: 2026-09-02
---
```

WKP is tolerant of missing or malformed frontmatter — files without it are indexed as `type = NULL`, `tier = 2`, `visibility = 'shared'`.

---

## VectorBackend protocol

All storage operations flow through the `VectorBackend` protocol (`backends/__init__.py`), enabling backend swaps without changing CLI or skill code:

```python
class VectorBackend(Protocol):
    def upsert(self, path, embedding, metadata, content) -> None: ...
    def search(self, query_embedding, filters, k) -> list[dict]: ...
    def delete(self, path) -> None: ...
    def rebuild_from(self, source_dirs) -> None: ...
```

| Backend | Use case | Dependency |
|---|---|---|
| `SqliteVecBackend` | Default — solo, <100k items | `sqlite-vec` |
| `ChromaDBBackend` | Larger corpora | `chromadb` |
| `PGVectorBackend` | Multi-agent, RHOAI cluster | `memory-hub` |

Switch: `WKP_BACKEND=chromadb wkp search "topic"` (env var respected by `db.py`).

---

## Progressive disclosure contract

The tier model is a formal contract, not a convention:

| Tier | Delivery guarantee | Agent action needed |
|------|-------------------|-------------------|
| 0 | Structural — always present, no tool call | None |
| 1 | Best-effort semantic — injected at session start | None |
| 2 | On demand — `wkp search` or `wkp context` | Bash tool call |
| 3 | Explicit only — direct file read | Read tool call |

Tier 0's guarantee is maintained by the bootstrap config generated at `wkp hooks` time. No semantic retrieval system can exclude Tier 0 content because it is never retrieved — it is written into the hook script as a static path.
