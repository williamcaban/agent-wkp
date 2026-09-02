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
2. `wkp index` embeds each file (CPU-only, `all-MiniLM-L6-v2`, ~40ms/file) and stores the embeddings alongside metadata and explicit graph edges in a single SQLite file (`.wkp/index.db`).
3. `wkp search "topic"` runs hybrid retrieval: vector similarity + BM25 keyword search merged via Reciprocal Rank Fusion, filtered by tier and token budget.
4. `wkp materialize --tier 0` pre-assembles the always-on context into `.wkp/tier0.md`.
5. A git post-commit hook keeps the index current — only re-embeds files whose git blob SHA changed.

## Quickstart

```bash
pip install agent-wkp    # or: pipx install agent-wkp

cd your-workspace
wkp init           # creates .wkp/, adds to .gitignore
wkp index          # full index (downloads ~22MB model on first run)
wkp materialize --tier 0
wkp hooks --framework claude_code --post-commit

# Search from any agent or shell
wkp search "rfe creation workflow"
wkp context "evalhub adapter" --tier 2 --budget 8000
wkp traverse memory/my-file.md --depth 2
wkp analyze        # PageRank — suggests Tier 1 promotions
```

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
wkp search QUERY                  Hybrid semantic + keyword search
  --tier INT                      Max tier to include (default: 2)
  --budget INT                    Token budget (default: 8000)
  -k INT                          Max results (default: 10)
  --format [text|json|paths]
wkp context TOPIC                 Tier-aware context assembly
  --tier INT  --budget INT
wkp traverse PATH                 BFS traversal from PATH via explicit edges
  --depth INT  --budget INT
wkp materialize --tier [0|1]      Pre-assemble static context file
wkp hooks --framework claude_code Install SessionStart hook
  --post-commit                   Also install git post-commit hook
wkp analyze                       PageRank — suggest Tier 1 promotions
  --top INT                       Number of candidates (default: 10)
```

## Requirements

- Python 3.12+
- `sqlite-vec` (vector similarity in SQLite — no server required)
- `sentence-transformers` (downloads `all-MiniLM-L6-v2` on first index run, ~22MB)
- `networkx` (graph analytics — only loaded by `wkp analyze`)
- Git (for blob SHA cache invalidation)

No database server. No Docker. The entire index is a single file: `.wkp/index.db`.

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
