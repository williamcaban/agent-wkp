# AGENTS.md — WKP for AI Agents

This file describes how AI agents should use the `wkp` CLI. Read it before calling any `wkp` command. `CLAUDE.md` is a different document: it tells contributors how to *build* `wkp`, not how to use it.

## What WKP is

WKP is a durable, cross-machine, cross-harness memory store: a git repository of markdown files with a small YAML-like frontmatter block, and a derived SQLite FTS5 index for full-text search. It is a single static binary — no daemon, no server, no MCP tool quota. You call it via the Bash tool.

There is no embedding model and no network call anywhere in the default path. Search is BM25 full-text search over the local index, plus explicit graph edges (`refs:` frontmatter and `[[wikilink]]` mentions in the body).

## The tiers

| Tier | How it reaches you | Contains |
|------|--------------------|----------|
| 0 | Already in context — a harness's `SessionStart` hook `cat`s `.wkp/tier0.md` before your first message | `type: project-state` and `type: instruction` items |
| 1 | Call `wkp materialize --tier 1` then read `.wkp/tier1.md`, or `wkp search --tier 1` | `type: feedback` and `type: knowledge` items |
| 2 | On demand — call `wkp search` or `wkp context` | Everything else, including anything under `inbox/` |
| 3 | Explicit file read — use the Read tool directly on a path `wkp` returned | Full file content, any tier |

**Tier 0 is already present.** Never search for it — it was injected before this conversation started. If you don't see a `<wkp-context tier="0">` block, the hook isn't wired into `.claude/settings.local.json` yet; run `wkp hooks --framework claude_code` and apply the printed JSON.

Tier is computed from frontmatter, not chosen by hand: `project-state`/`instruction` → tier 0, `feedback`/`knowledge` → tier 1, everything else → tier 2. Anything under `inbox/`, or with `confidence: proposed`/`inferred`, is forced to tier 2 regardless of its `type` — agent-written or imported memory never self-promotes into tier 0/1 (see "Writing memory" below).

## Core commands

### Initialize a store

```bash
wkp init [path]
```

Makes `path` (default: cwd) a git repository with a `.wkp/` directory holding the derived, gitignored `index.db`. Also runs the one-time import described below. Safe to re-run — idempotent.

### Search by topic

```bash
wkp search "rfe creation workflow" --tier 2 --budget 6000
```

BM25 full-text search over `index.db`. Flags: `--tier N` (filter to a tier), `--budget N` (stop once estimated tokens exceed N), `-k`/`--limit N` (max hits), `--format text|paths|json` (default `text`), `--path DIR` (store root, default cwd).

Use `--format paths` to get bare paths for Read tool calls:

```bash
wkp search "guardrails nemo" --format paths -k 5
```

### Assemble context for a topic

```bash
wkp context "evalhub adapter patterns" --tier 2 --budget 8000
```

Same flags as `search`. Combines the BM25 hits with a graph traversal of the `refs:`/`[[wikilink]]` edges from those hits, under one result set. Better than `search` when you want a hit's directly-linked neighbors included, not just the hit itself.

### Traverse explicit references from a known file

```bash
wkp traverse memory/reference_rfe.md --depth 2
```

Follows `refs:` and `[[wikilink]]` edges outward from a specific file, no search query involved. `--depth N` (default 2) caps hop count. Output includes hop distance (`+1`, `+2`, ...) from the starting file. Same `--format`/`--path` flags as `search`.

### Re-index the store

```bash
wkp index [path]
```

Rescans the store rooted at `path` (default cwd) and updates `index.db` incrementally: uses git's own change detection to hash only files that are new, modified, or deleted since the last run — cost is proportional to what changed, not to corpus size. Only `.md` files are indexed. Safe to run at any time.

There is no per-file re-index command — `wkp index` always operates on the whole store, but because it's incremental, running it after editing one file is cheap.

### Materialize a tier to disk

```bash
wkp materialize --tier 0
wkp materialize --tier 1
```

Writes `.wkp/tier{N}.md`, atomically (temp file + rename, never in place). This is what a harness's `SessionStart` hook reads for tier 0; you would only call this yourself to inspect tier 0/1 content directly, or after editing frontmatter and wanting materialized output to reflect it (run `wkp index` first, since materialize reads from `index.db`).

### Print the SessionStart hook text

```bash
wkp hooks --framework claude_code
```

Prints the exact JSON to merge into `.claude/settings.local.json`. It re-indexes quietly (best-effort — a broken index never blocks session start) and then prints `.wkp/tier0.md`, also best-effort (a store with no materialized tier 0 yet produces nothing, not an error). This command never touches git or the store; it only prints static text.

### Import existing harness memory

```bash
wkp import [path]
```

One-shot, idempotent migration of pre-existing memory into `inbox/import/`: `CLAUDE.md`/`AGENTS.md` at the store root (tagged `type: instruction`), and every `<home>/.claude/projects/*/memory/*.md` file this harness itself wrote (tagged from that file's own `metadata.type`). Every imported item gets `confidence: proposed` — nothing here has been through review, so nothing here reaches tier 0/1 (see "Writing memory"). Already runs once automatically as part of `wkp init`; call it again by hand if new source files show up later. A destination file that already exists is left alone, never overwritten — a human may have edited their imported copy since.

## Reading output

`wkp search`/`wkp context`/`wkp traverse` text output:

```
[T2] Title of the file  (score=0.031, ~800t)
     path/to/file.md
```

- `T2` = tier 2 (use the Read tool to get full content)
- `score` = BM25 score (higher = more relevant)
- `~800t` = estimated token cost (from frontmatter `tokens:` if present, otherwise ~4 chars/token)

A hit reached only through graph traversal (`context`, or any `traverse` result) carries a hop distance:

```
[T2 +1] Directly referenced file  (score=0.500, ~200t)
     path/to/direct.md
```

`+1` = one `refs:`/`[[wikilink]]` hop from a direct hit (traversal-only score is `1/(1+hops)`, not a BM25 score); `+2` = two hops, and so on. No `+N` suffix means it matched the query directly.

`--format json` gives one array of objects, each with exactly: `path`, `title`, `score`, `tier`, `tokens`, `hop_distance` (string/string/number/integer/integer/integer).

## Typical retrieval pattern

```bash
# 1. Find relevant files
wkp search "topic" --format paths -k 5

# 2. Read the most relevant one
# (use the Read tool on the path returned above)

# 3. Follow explicit references from that file
wkp traverse path/to/file.md --depth 1

# 4. If context is still thin, broaden
wkp context "topic" --tier 2 --budget 8000
```

## When to call wkp

Call `wkp search` or `wkp context` when:
- The user asks about a topic you don't have enough context on.
- You need to find which files cover a subject before reading them.
- You want to discover related files via explicit references (`wkp traverse`).

Do **not** call `wkp search` for every message — only when you genuinely need to retrieve knowledge you don't already have. Do not call it to check whether a specific file exists; use the Read tool directly if you already know the path.

## Writing memory

There is no `wkp remember` yet (M2). Until then, do not hand-edit files outside `inbox/` and do not hand-write `confidence:` as anything other than `proposed` — Tier 0 and Tier 1 promotion is a human decision made through a signed commit, not something an agent or import step can do to itself. If you write a markdown file to record something learned mid-session, put it under `inbox/` with `confidence: proposed` and let a human promote it later.

## Storage locations

```
your-workspace/
  .wkp/
    index.db      — SQLite: FTS5 content index + graph edges (gitignored, derived)
    tier0.md      — materialized tier 0, read by the SessionStart hook (gitignored, derived)
    tier1.md      — materialized tier 1 (gitignored, derived)
  inbox/
    import/       — output of `wkp import`: proposed, tier-2-only until a human promotes it
```

`.wkp/index.db` and `.wkp/tier{0,1}.md` are gitignored derived artifacts, written atomically. Deleting `.wkp/` and running `wkp init && wkp index` rebuilds them from scratch; nothing under `.wkp/` is ever the source of truth.

## What not to do

- Do not pass more than ~10k tokens of search results to the model at once — use `--budget` to stay within budget.
- Do not rely on `wkp search`/`wkp context` for Tier 0 content — it is already in your context.
- Do not hand-write or hand-edit `index.db` or `tier{0,1}.md` — they are derived; edit the markdown source and re-run `wkp index`/`wkp materialize`.
- Do not set `confidence: proposed`/`inferred` items to a stated confidence yourself, or move a file out of `inbox/` yourself — that promotion is a human-signed action.
