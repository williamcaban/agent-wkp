# WKP — Workspace Knowledge Protocol

Progressive disclosure knowledge index for AI agent workspaces. WKP turns a directory of markdown files into a searchable, tier-aware knowledge store that any agent harness can consume — without heavy infrastructure.

## The problem it solves

Agent context windows are finite. Loading everything you know into every session wastes tokens, degrades model performance, and inflates cost. But loading nothing means the agent starts cold every time.

WKP enforces a four-tier disclosure model so the agent always has the minimum useful context, and can fetch more on demand:

| Tier | Token budget | When it loads | Mechanism |
|------|-------------|--------------|-----------|
| 0 | ≤4k | Every session, guaranteed | Static file injected by SessionStart hook |
| 1 | ≤8k | Session start, topic-gated | Lightweight index assembled by hook |
| 2 | ≤16k | Mid-session, on demand | `wkp search` or `wkp context` via Bash |
| 3 | Unlimited | Explicit fetch only | Direct file read |

Tier 0 is **structurally guaranteed** — it is never retrieved, never skipped, and does not depend on any search system being available.

## How it works

1. You write markdown files with [OKF frontmatter](#okf-frontmatter) declaring `type`, `workspace`, `tokens`, and `refs`.
2. `wkp index` embeds each file (CPU-only, `all-MiniLM-L6-v2`, ~40ms/file) and stores embeddings, metadata, and explicit graph edges in a single SQLite file (`.wkp/index.db`).
3. `wkp search "topic"` runs **BM25 keyword search (FTS5) by default** — instant, no model load. Add `--embed-url` for hybrid semantic + keyword search via Reciprocal Rank Fusion against an OpenAI-compatible embeddings endpoint (Ollama, LM Studio, vLLM, OpenAI, etc.).
4. `wkp materialize --tier 0` pre-assembles the always-on context into `.wkp/tier0.md`.
5. The index stays current via git blob SHA — only files whose content actually changed are re-embedded on the next `wkp index` run.
6. Query embeddings are cached in SQLite (`query_cache` table) — repeated queries skip the model entirely.

## Storage

Everything lives in `.wkp/` at the workspace root — two files, nothing else:

```
your-workspace/
  .wkp/
    index.db    ← single SQLite file: embeddings + metadata + FTS + graph (typically 1–10 MB)
    tier0.md    ← pre-assembled Tier 0 context injected at session start
```

Both files are in `.gitignore`. They are derived artifacts — deleting `.wkp/` and running `wkp init && wkp index` reconstructs everything from your markdown files in seconds.

Sub-workspaces each get their own `.wkp/` shard. `wkp search --all-workspaces` federates across all shards.

## Quickstart

```bash
pip install agent-wkp    # or: pipx install agent-wkp

cd your-workspace
wkp init                          # creates .wkp/, adds to .gitignore
wkp index                         # full index (downloads ~22MB model on first run)
wkp materialize --tier 0          # pre-assemble Tier 0 context file
wkp hooks --framework claude_code # install SessionStart hook for Claude Code

# Search from any agent or shell
wkp search "rfe creation workflow"
wkp context "evalhub adapter" --tier 2 --budget 8000
wkp traverse memory/my-file.md --depth 2
wkp analyze                       # PageRank — suggests Tier 1 promotions
```

## Keeping the index fresh

The index only re-embeds files whose git blob SHA changed — unchanged files are skipped in milliseconds. The question is *what triggers* a re-index run.

**Choose the trigger that matches your commit frequency:**

### Option A — SessionStart hook (recommended for knowledge bases)

Re-index any changed files at the start of every agent session, before the first prompt. Best for repos where commits happen infrequently (days or weeks apart).

```bash
wkp hooks --framework claude_code   # generates .claude/hooks/wkp-session-start.sh
```

Then update the hook script to re-index before injecting Tier 0:

```bash
#!/bin/bash
# .claude/hooks/wkp-session-start.sh
cd "$(git rev-parse --show-toplevel)" || exit 0

# Re-index any files changed since the last index run (skips unchanged via SHA)
wkp index --quiet 2>/dev/null || true

echo "<wkp-context tier=\"0\">"
cat .wkp/tier0.md
echo "</wkp-context>"
```

Wire it in `.claude/settings.local.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "hooks": [{ "type": "command", "command": "bash .claude/hooks/wkp-session-start.sh" }] }
    ]
  }
}
```

### Option B — git post-commit hook (recommended for active code repos)

Re-index only the files changed in each commit. Best for repos where commits happen frequently (multiple times per day).

```bash
wkp hooks --framework claude_code --post-commit
```

### Option C — Manual

Run `wkp index` whenever you want a fresh index. Useful for large batch changes or initial setup.

```bash
wkp index            # re-index everything changed since last run
wkp index file.md    # re-index a single file immediately
```

### Hot files: UserPromptSubmit hook

If specific files change frequently mid-session (e.g. a Claude Code memory directory), re-index them on every prompt:

```json
{
  "hooks": {
    "UserPromptSubmit": [
      { "hooks": [{ "type": "command",
          "command": "wkp index ~/.claude/projects/<your-project>/memory/ 2>/dev/null || true" }] }
    ]
  }
}
```

This is fast (~100ms) because SHA comparison skips unchanged files.

## OKF Frontmatter

WKP reads **OKF (Open Knowledge Frontmatter)** from every markdown file:

```yaml
---
title: Short descriptive title
type: reference          # project-state | knowledge | reference | feedback | skill
workspace: root          # root | eval-hub | trustworthy-ai | ...
visibility: shared       # shared | private
tokens: ~800             # rough token estimate (used for budget enforcement)
tags: [rfe, jira, pm]
refs:
  - ../other-workspace/knowledge/shared/related-file.md
updated: 2026-09-02
---
```

**Type → tier mapping** (enforced in the SQLite schema):

| OKF type | Tier |
|---|---|
| `feedback` | 1 |
| `project-state` | 1 |
| `skill` | 1 |
| `knowledge` | 2 |
| `reference` | 2 |

Files without OKF frontmatter are indexed with `type = NULL` (treated as Tier 2).

## Using with Claude Code

After `wkp hooks --framework claude_code`, the SessionStart hook injects Tier 0 context before your first message. For Tier 2 on-demand retrieval, call `wkp` from a skill:

```bash
# Inside a skill instruction or directly in Claude Code:
wkp search "nemo guardrails" --tier 2 --budget 6000
wkp context "rfe creation" --tier 2
```

No MCP tool quota is consumed. `wkp` runs as a Bash subprocess.

To wire the hook into Claude Code add to `.claude/settings.local.json`:

```json
{
  "hooks": {
    "SessionStart": [
      { "command": "bash .claude/hooks/wkp-session-start.sh" }
    ]
  }
}
```

## CLI reference

```
wkp init                          Initialise index in this workspace
wkp index [FILES]                 Index all (or specified) markdown files
  --force                         Re-embed even if git blob SHA unchanged
wkp search QUERY                  BM25 keyword search (default: instant, no model load)
  --embed-url URL                 Enable hybrid semantic+keyword search via RRF.
                                  Reads WKP_EMBED_URL env var.
  --embed-api-key KEY             API key for endpoint. Prefer WKP_EMBED_API_KEY env var.
  --embed-model MODEL             Model name (e.g. nomic-embed-text). Reads WKP_EMBED_MODEL.
  --tier INT                      Max tier to include (default: 2)
  --budget INT                    Token budget (default: 8000)
  -k INT                          Max results (default: 10)
  --format [text|json|paths]
wkp context TOPIC                 Tier-aware context assembly
  --embed-url / --embed-api-key / --embed-model   same as search
  --tier INT  --budget INT
wkp traverse PATH                 BFS traversal from PATH via explicit edges
  --depth INT  --budget INT
wkp materialize --tier [0|1]      Pre-assemble static context file
wkp hooks --framework claude_code Install SessionStart hook
  --post-commit                   Also install git post-commit hook
wkp analyze                       PageRank — suggest Tier 1 promotions
  --top INT                       Number of candidates (default: 10)
```

### Semantic search with Ollama

```bash
# Start Ollama and pull a small embedding model
ollama pull nomic-embed-text

# One-time: set env vars
export WKP_EMBED_URL=http://localhost:11434/v1
export WKP_EMBED_MODEL=nomic-embed-text

# Hybrid search: semantic + keyword via RRF
wkp search "evaluation drift detection"

# Or per-call:
wkp search "evaluation drift" --embed-url http://localhost:11434/v1 --embed-model nomic-embed-text
```

For endpoints that require an API key (OpenAI, hosted Ollama, etc.):

```bash
export WKP_EMBED_API_KEY=sk-...   # avoid --embed-api-key to keep key out of shell history
export WKP_EMBED_URL=https://api.openai.com/v1
export WKP_EMBED_MODEL=text-embedding-3-small
wkp search "rfe creation workflow"
```

> **Note on embedding consistency**: vector search is most meaningful when the index was built with the same model used at query time. The embedding model for indexing is controlled separately in `indexer.py` (currently always `all-MiniLM-L6-v2`). If your remote model has a different dimension than the index (384), WKP automatically falls back to BM25-only rather than returning meaningless results.

## Requirements

- Python 3.12+
- `sqlite-vec` (vector similarity in SQLite — no server required)
- `sentence-transformers` (downloads `all-MiniLM-L6-v2` on first `wkp index` run, ~22MB — required for indexing, not for search)
- `networkx` (graph analytics — only loaded by `wkp analyze`)
- `httpx` (optional — only required for `--embed-url` remote embeddings): `pip install 'agent-wkp[embed]'`
- Git (for blob SHA cache invalidation)

No database server. No Docker. The entire index is a single file: `.wkp/index.db`.

> **Search without the model**: `wkp search` uses BM25 by default — the embedding model is only loaded during `wkp index`. If you never call `wkp search --embed-url`, sentence-transformers is loaded only at index time, not during agent sessions.

## Scalability

WKP uses a `VectorBackend` protocol so the storage layer can be swapped without changing CLI or skill code:

| Scale | Backend | Switch |
|---|---|---|
| <100k items | `SqliteVecBackend` (default) | none |
| 100k+ or multi-agent | `ChromaDBBackend` | `WKP_BACKEND=chromadb` |
| Enterprise / RHOAI | `PGVectorBackend` | `WKP_BACKEND=memoryhub` |

See [docs/architecture.md](docs/architecture.md) for the full design.

## License

Apache 2.0 — see [LICENSE](LICENSE).
