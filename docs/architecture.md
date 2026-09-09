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

### `query_cache` — query embedding cache

```sql
CREATE TABLE query_cache (
    query     TEXT PRIMARY KEY,
    embedding BLOB NOT NULL,
    created   TEXT DEFAULT (datetime('now'))
);
```

Caches query embeddings to avoid reloading the embedding model on repeated queries. The `query` key is compound: `"local|{text}"` for the local sentence-transformers model, or `"{url}|{model}|{text}"` for remote endpoints. Entries from different models never collide. Not consulted for BM25-only search.

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

## Retrieval

### Default: BM25 keyword search (FTS5)

By default, `wkp search` uses BM25 via the FTS5 virtual table — instant, zero model load:

```sql
SELECT ki.path, ki.title, ki.type, ki.workspace, ki.tokens, ki.tier,
       (-knowledge_fts.rank) AS score
FROM knowledge_fts
JOIN knowledge_items ki ON ki.rowid = knowledge_fts.rowid
WHERE knowledge_fts MATCH ?
  AND ki.visibility = 'shared'
  AND ki.tier <= ?
ORDER BY knowledge_fts.rank DESC
LIMIT ?
```

FTS5 `rank` is a negative BM25 score (less negative = better match). Negating gives a positive score for display. Query strings are sanitized via `_sanitize_fts5()` before passing to FTS5 to strip special characters that cause syntax errors (e.g. periods in version numbers like `3.6`).

### Hybrid: vector + BM25 via RRF (optional)

When `--embed-url` is provided (or `WKP_EMBED_URL` is set), search adds a semantic vector signal merged via Reciprocal Rank Fusion (k=60):

```
score(item) = Σ  1 / (60 + rank_in_signal)
              signals
```

- **Signal 1 (semantic)**: vector similarity via `knowledge_vec` ANN search — query embedded via remote OpenAI-compatible endpoint
- **Signal 2 (keyword)**: BM25 via `knowledge_fts` MATCH

Both signals run in a single SQL query with CTEs. No application-layer merge. If the vector search fails (e.g. embedding dimension mismatch between the query model and index model), WKP automatically falls back to BM25-only.

### Remote embeddings endpoint

`EmbedConfig` holds the endpoint URL, optional API key, and model name:

```python
@dataclass
class EmbedConfig:
    url: str             # e.g. "http://localhost:11434/v1"
    api_key: str | None  # read from WKP_EMBED_API_KEY — keep out of shell history
    model: str | None    # e.g. "nomic-embed-text" — omit for server default
```

The CLI reads these from env vars (`WKP_EMBED_URL`, `WKP_EMBED_API_KEY`, `WKP_EMBED_MODEL`) or flags. The `httpx` library makes the POST to `/v1/embeddings`. Requires `pip install 'agent-wkp[embed]'`.

**Embedding consistency**: vector search is meaningful only when the query model matches the model used during `wkp index`. The indexer currently always uses `all-MiniLM-L6-v2` (384-dim). If a remote model produces a different dimension, the vector leg silently falls back to BM25-only rather than returning garbage results.

### Query embedding cache

Query embeddings are cached in `query_cache` (a regular SQLite table in `index.db`):

```sql
CREATE TABLE query_cache (
    query     TEXT PRIMARY KEY,  -- compound key: "local|{text}" or "{url}|{model}|{text}"
    embedding BLOB NOT NULL,
    created   TEXT DEFAULT (datetime('now'))
);
```

The compound key encodes the embedding source so entries from different models don't collide. Cache lookup happens before any model load — if the query was seen before (with the same source), no model call is made. For BM25-only search, the cache is not consulted (no embedding needed).

### Context assembly

After search, `context_assemble` traverses explicit edges from the top-3 results (depth 2) to pull in directly-referenced items within the remaining token budget. Both search and traversal respect the tier ceiling and token budget constraints.

---

## Git integration

### Cache invalidation

Git blob SHA is the cache invalidation key. `git hash-object <file>` returns a content-addressed hash. If the stored SHA matches the current SHA, the file is skipped — no re-embedding, no disk read of the content.

```python
sha = subprocess.run(["git", "hash-object", path], ...).stdout.strip()
if db.get_blob_sha(path) == sha:
    return  # skip — content unchanged
```

This means `wkp index` is always safe to run at any frequency. The cost is proportional to what actually changed, not the total corpus size.

### Update triggers — choose by commit frequency

The git blob SHA mechanism is the *how*. The *when* depends on how often commits happen in your workspace:

**Knowledge bases (infrequent commits — days or weeks apart)**

Use the SessionStart hook to re-index before each session:

```
Session start
  → .claude/hooks/wkp-session-start.sh
      → wkp index          # skips unchanged files via SHA; re-embeds anything new
      → wkp materialize --tier 0   # regenerate tier0 only if Tier 1 items changed
      → cat .wkp/tier0.md
      → stdout injected as <wkp-context tier="0"> before first prompt
```

For a corpus of 200–500 files with few changes, the SHA scan takes <200ms and the session starts with a fully current index.

**Active code repos (frequent commits — multiple times per day)**

The post-commit hook is more efficient — it only re-indexes files touched in each commit:

```
git commit
  → post-commit hook
      → git diff --name-only HEAD~1 HEAD | grep '\.md$'
      → wkp index [changed files]   # targeted re-embed
      → wkp materialize --tier 0
```

**Hot files changed mid-session**

For files edited frequently without committing (e.g. agent memory directories), use a UserPromptSubmit hook to re-index on every prompt:

```
User sends prompt
  → UserPromptSubmit hook
      → wkp index ~/.claude/projects/<project>/memory/
         (SHA check makes this ~100ms even for large directories)
```

### Tier 0 injection

```
Session start
  → .claude/hooks/wkp-session-start.sh
      → stdout: <wkp-context tier="0"> ... </wkp-context>
         injected before the first prompt
```

`tier0.md` is a static pre-assembled file — injection is a simple file read with zero subprocess overhead. If the SessionStart hook also runs `wkp index`, the materialize step regenerates `tier0.md` if any Tier 1 items changed.

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
