# WKP Hub: Design Document

**Working title:** WKP Hub (local `wkp` + hosted `hub`)
**Status:** Draft v0.1 for review, 2026-09-05
**Author:** drafted with Claude for William Caban
**Scope:** cross-harness agent knowledge and memory service, local-first, optional hosted sync

---

## 0. How to read this document

Each section states a decision, then the reasoning, then the trade-offs and the failure modes the decision accepts. Where a claim rests on a primary source, the source is linked inline. Where a claim is my judgment rather than something the literature or a spec establishes, it is labeled **[judgment]**. Where I lack a direct citation, it is labeled **[unverified]** and should be treated as a working assumption.

Confidence labels used: **established** (spec, official docs, or peer-reviewed), **practice** (widely adopted, documented by maintainers), **emerging** (early evidence), **speculation**.

---

## 1. Problem statement and goals

### 1.1 The problem

Every agentic harness invents its own memory: Claude Code has `CLAUDE.md`, project memory directories and hooks; OpenCode reads `AGENTS.md`; Hermes Agent, dsh and others each carry their own conventions. The memory a user or agent accumulates in one harness (who the user is, how a project works, facts uncovered mid-session, instructions, feedback) is stranded there. It does not follow the user to a second machine, to a second harness, or to a second instance of the same harness with a different working directory.

agent-wkp already solves the intra-harness half of this: a directory of markdown files becomes a tier-aware, BM25-searchable store injected at session start via a hook, with more fetched on demand through `wkp search` over Bash, consuming no MCP tool-definition tokens and requiring no infrastructure ([agent-wkp README](https://github.com/williamcaban/agent-wkp)). The gap is portability and continuity across harnesses and machines.

### 1.2 Goals (in priority order, per the brief)

1. **Latency and performance.** Session-start injection and `wkp search` must be fast enough that harnesses call them freely. Targets in section 4.
2. **Security.** The memory store holds the most sensitive text a user produces (project internals, personal facts, possibly credentials pasted by accident). The design must hold from a single laptop to a multi-tenant hosted service with no security "cliff" between modes.
3. **Slim core.** Reuse OS-native and already-audited tools (git, SQLite, OpenSSH, OS keychains, OS sandboxing) rather than reimplementing them. The core should embed inside a harness with negligible overhead.
4. **Harness neutrality.** Any harness that can run a shell command and read a file can participate. Anything richer (MCP, plugins) is an optional adapter, never a requirement.
5. **Two modes, one codebase.** Local mode (native binary or container, data stays local, works air-gapped) and hosted mode (sync to a service, multi-workstation, multi-harness). Local can register with hosted later without migration.

### 1.3 Non-goals (v1)

- Replacing the harness's own working memory (scratchpads, in-session summaries). WKP is the durable, cross-session layer.
- Real-time collaborative editing between concurrent agents. Sync is asynchronous with explicit merge.
- A web UI beyond account, device and billing management.
- Semantic search as the default retrieval path (see 5.3 for when it is the right option).

---

## 2. Positioning relative to existing memory layers

The [best-of-Agent-Harnesses memory-layers comparison](https://github.com/RyanAlberts/best-of-Agent-Harnesses/blob/main/comparisons/memory-layers.md) frames the field around one question: who owns the memory, the application or the agent? It contrasts Mem0 (extracted facts behind an API), claude-mem (session capture, compression, injection on resume) and Letta (agent-managed memory hierarchy inside a runtime).

WKP Hub takes a fourth position: **the user owns the memory, as files, under version control.** Consequences:

| Property | API-owned (Mem0-style) | Runtime-owned (Letta-style) | Plugin-owned (claude-mem-style) | WKP Hub |
|---|---|---|---|---|
| Storage | Vendor DB | Runtime DB | Harness-local | Markdown in a git repo the user holds |
| Portability across harnesses | Via SDK per harness | Locked to runtime | Locked to harness | Any harness that can run a shell command |
| Audit trail | Vendor-dependent | Vendor-dependent | Minimal | `git log`, signed commits |
| Offline / air-gapped | No | Partial | Yes | Yes, by construction |
| Retrieval default | Vector | Vector + agent | Compression + injection | BM25 (FTS5), tiered budget |
| Exit cost | High | High | Medium | Zero (it is a directory of markdown) |

The last row is the strategic bet **[judgment]**: for a micro-SaaS aimed at technical users who run several harnesses, zero exit cost is a feature that drives trust and adoption, and the hosted value is sync, backup, identity and cross-device search rather than lock-in.

---

## 3. Architecture overview

### 3.1 Components

```
┌──────────────────────────────── laptop / workstation ────────────────────────────────┐
│                                                                                       │
│  harness A (Claude Code)   harness B (OpenCode)   harness C (Hermes / dsh / other)    │
│        │ hook + Bash             │ hook + Bash             │ Bash / optional MCP      │
│        ▼                         ▼                         ▼                          │
│  ┌───────────────────────────── wkp (single static binary) ─────────────────────┐    │
│  │  read path:  index.db (SQLite FTS5, BM25)  → tier0.md / search results        │    │
│  │  write path: markdown files → git plumbing (hash-object, commit-tree, sign)   │    │
│  │  sync path:  git fetch/push over SSH or HTTPS, or git bundle (air-gapped)     │    │
│  │  optional wkpd: file watcher, sync scheduler, Unix-socket server for harnesses│    │
│  └───────────────────────────────────────────────────────────────────────────────┘    │
│        │                                                                              │
│  ~/.wkp/store/  (git repo: knowledge + memory, age-encrypted where private)           │
│  ~/.wkp/index/  (derived SQLite, never synced)                                        │
└───────────────────────────────────────┬───────────────────────────────────────────────┘
                                        │ SSH (ed25519 device key) or HTTPS smart protocol
                                        ▼
┌──────────────────────────────── hosted hub ───────────────────────────────────────────┐
│  OpenSSH sshd (AuthorizedKeysCommand + ForceCommand) ── git-receive-pack / upload-pack│
│  reverse proxy ── git http-backend (CGI shipped with git)                             │
│  per-tenant bare repo ── post-receive → wkp index (per-tenant SQLite) [opt-in tier]   │
│  control plane: accounts, devices, keys, subscriptions (Postgres) ── MoR for billing  │
└───────────────────────────────────────────────────────────────────────────────────────┘
```

### 3.2 Core decisions at a glance

| # | Decision | Section |
|---|---|---|
| D1 | Core is one static binary written in Rust; the Python package is retired | 4.1 |
| D2 | No mandatory daemon; `wkpd` is optional and speaks only over a Unix domain socket | 4.2 |
| D3 | Store is a git repository of markdown; git is the versioning, audit and sync engine | 5.1 |
| D4 | Index is a derived, never-synced SQLite file; BM25 via FTS5 is the default retrieval | 5.2, 5.3 |
| D5 | Sync is git transport; conflicts resolved by structure (one subject per file) plus a custom merge driver | 6 |
| D6 | Private content is encrypted client-side (age) before it enters git; hosted default is zero-knowledge | 7.2 |
| D7 | Hosted git front end is OpenSSH + git's own server binaries, not a custom protocol server | 8.1 |
| D8 | Per-tenant SQLite indexes on the hub, Postgres only for the control plane | 8.2 |
| D9 | Memory written by agents is provenance-tagged and never enters Tier 0 without a human-signed commit | 7.4 |
| D10 | Pipeline treats agentic maintainers as untrusted contributors with strong, automated gates | 9 |

### 3.3 Repository structure

One Cargo workspace monorepo. The hub reuses the laptop's index, search, merge-driver, age-filter and frontmatter code by design (D8), and the injection regression suite (9.4) must run against both sides; splitting them across repos invites version skew on exactly the code paths that carry the security guarantees. For agentic maintainers, one checkout gives the whole system in context, one CI gives one machine-readable verdict per PR, and one release gives one SBOM and one set of attestations.

```
agent-wkp/                       (Cargo workspace)
  crates/
    wkp-core/      store model, frontmatter, tiers, index, FTS5 search, merge driver
    wkp-crypto/    age filter, key handling, signing, allowed_signers   (CODEOWNERS: human)
    wkp-git/       plumbing wrapper, bundles, sync                       (CODEOWNERS: human)
    wkp-cli/       the `wkp` binary (`wkpd` is a subcommand)
    wkp-hub/       `wkp hub` mode: wkp-shell, key-to-tenant, post-receive indexer, control plane
    wkp-sys/       the only crate allowed `unsafe` (SQLite bundling)
  adapters/        hook templates per harness, printed by `wkp hooks`
  deploy/          Containerfiles, sshd_config, systemd/launchd units, compose for the hub
  fuzz/            cargo-fuzz targets
  tests/injection-corpus/
  docs/            this design doc, ADRs
  .github/workflows/
```

`wkp-hub` is a separate binary target with its own container image, so the laptop binary never links the Postgres client or control-plane code; feature flags and separate `[[bin]]` targets keep the size budget in 9.6 intact. `wkp hooks --framework <name>` is the only "installer": it prints the exact hook text for an agent or a human to apply, and the binary never writes outside its own store.

Two repos live outside the workspace by convention: `homebrew-wkp` (Homebrew requires a tap in its own `homebrew-<tap>` repo; the release workflow updates the formula with a scoped token) and, if and when it exists, the account and billing web surface (different language, toolchain, scanner set, deploy target and blast radius; 8.2 already walls it off from tenant content). A separate repo for the hub "for security" is deliberately rejected: the secrets that matter live in CI environments and CODEOWNERS-gated workflows, not in repo boundaries.

---

## 4. Core runtime

### 4.1 D1: Rust single static binary

**Decision.** Rewrite the core (`wkp`) in Rust as one statically linked binary. No Python package or `pip install` compatibility is maintained: the current Python implementation has a single user (the author), so the PyPI channel is retired rather than carried as a wrapper. Effort goes into the binary's size, startup and integration surface, not into installers.

**Reasoning.**

- *Latency.* The hot path is a subprocess spawned by a harness hook or Bash call. Interpreter start plus imports is a fixed tax paid on every call; a static native binary starts in low single-digit milliseconds. This is the single largest lever on the p50 targets below. The current Python implementation defers `sentence-transformers` to index time to avoid this cost ([agent-wkp README](https://github.com/williamcaban/agent-wkp)); Rust removes the tax entirely.
- *Security.* Memory safety without a garbage collector, `#![forbid(unsafe_code)]` enforceable at the crate level, and a dependency ecosystem with first-class supply-chain tooling (`cargo-audit`, `cargo-deny`, `cargo-vet`, section 9).
- *Slim core and distribution.* One file, no runtime, no `pip`/`venv` drift on the user's machine, trivially embeddable inside a harness container image. Cross-compiled for `x86_64`/`aarch64` on Linux and macOS.
- *Agentic maintainers.* **[judgment]** A strict compiler and type system is the cheapest continuous reviewer of agent-generated code. The peer-reviewed evidence that AI-assisted code is measurably more likely to contain vulnerabilities (Pearce et al., "Asleep at the Keyboard?", IEEE S&P 2022, [arXiv:2108.09293](https://arxiv.org/abs/2108.09293); Perry et al., "Do Users Write More Insecure Code with AI Assistants?", ACM CCS 2023, [arXiv:2211.03622](https://arxiv.org/abs/2211.03622)) argues for a language where a large class of memory and concurrency defects cannot compile.

**Alternatives considered.**

- *Go.* Comparable startup and distribution story, simpler language for agents to write. Loses memory safety guarantees only in `unsafe`/cgo boundaries, which is where SQLite bindings live (pure-Go SQLite ports exist but lag upstream FTS5 fixes). Reasonable second choice; the trade is mostly SQLite binding maturity and supply-chain tooling depth. **[judgment]**
- *Keep Python.* Lowest migration cost, highest per-call latency, weakest sandboxing story, and the packaging drift already visible in the current dependency list (sentence-transformers, networkx, sqlite-vec).

**Accepted failure modes.** Rust compile times slow the agentic edit-test loop; mitigated with `cargo check`, incremental builds and a small crate graph. Fewer agent-maintainers are fluent in Rust than Python; mitigated by strong CI feedback (section 9) that turns compiler and lint output into the review signal.

### 4.2 D2: No mandatory daemon; optional `wkpd` over a Unix domain socket

**Decision.** `wkp search`, `wkp context`, `wkp remember` and `wkp materialize` work as standalone subprocesses with no background process. An optional `wkpd` provides file watching (re-index on change), scheduled sync, and a local API for harnesses that prefer a socket to spawning a process. `wkpd` listens **only** on a Unix domain socket with mode `0600`, verifies the peer's UID (`SO_PEERCRED` on Linux, `LOCAL_PEERCRED`/`getpeereid` on macOS), and never binds a TCP port.

**Reasoning.**

- *Latency.* A daemonless design already meets the targets (below) because the index is a single SQLite file that opens in microseconds; a daemon only helps if process spawn dominates, which the Rust rewrite removes.
- *Security.* A loopback HTTP port is reachable by every process on the machine, including browsers via DNS rebinding and any compromised local tool. A UDS with filesystem permissions and peer-credential checks confines access to the user's own processes. This is the same boundary the OS already trusts for `ssh-agent` and Docker.
- *Slim core.* The harness contract stays "run a command, read stdout". No client library required.

**`wkpd` sync cycle (borrowed from Mutagen's watch-triggered model).** When present, `wkpd` runs a cycle on filesystem change rather than on a timer: OS watcher (FSEvents on macOS, inotify on Linux, via the `notify` crate, or git's builtin fsmonitor where available) → debounce (default 2 s) → incremental re-index → auto-commit to a per-device branch `sync/<device-id>` → opportunistic push with a short timeout. The per-device branch is the equivalent of Mutagen's alpha/beta endpoint separation: no two machines ever write the same ref, so the hub never sees a non-fast-forward push and merging into `main` happens on the device, where the provenance rules in 7.4 apply. Mutagen's design is the reference for this shape ([Mutagen synchronization docs](https://mutagen.io/documentation/synchronization)); its engine is not adopted because it keeps no history, no signatures and no encryption at rest (see 6.3).

**Atomic apply on the read path.** Harnesses read `tier0.md` at session start and `index.db` on every search, possibly while a sync or re-index is running. Both are always written to a temporary file in the same directory and swapped in with `rename(2)`, so a reader never sees a torn file. Same discipline as Mutagen's stage-then-apply.

**Container local mode.** The container mounts the store directory and, if the user wants `wkpd`, the socket path. No published ports. Image is built from `scratch` with the static binary, runs as non-root, read-only root filesystem, and is signed (section 9.6).

### 4.3 Latency and resource targets

| Operation | Target (p50 / p95) | Notes |
|---|---|---|
| Session-start injection (`cat tier0.md`) | < 1 ms / 2 ms | Static file; no WKP code runs |
| Incremental index check (`wkp index`) | < 30 ms / 100 ms | `git hash-object` stat-cache compare, changed files only |
| `wkp search` cold process | < 10 ms / 25 ms | Binary start + FTS5 query on ≤ 50k items |
| `wkp remember` (write + commit) | < 50 ms / 150 ms | Plumbing commit, no hooks in the hot path |
| Peak RSS for `wkp search` | < 30 MB | Excludes optional embedding client |

These are engineering targets, not measurements **[unverified]**. They should be enforced by a benchmark gate in CI (section 9.5) so regressions are caught by the pipeline rather than by users.

---

## 5. Storage and retrieval

### 5.1 D3: Git is the store, the version history, the audit log and the sync engine

**Decision.** The store is a git repository (`~/.wkp/store`, or a per-project store alongside the code). Every knowledge or memory item is a markdown file with frontmatter. Every mutation is a commit made through git plumbing (`git hash-object`, `git update-index`, `git write-tree`, `git commit-tree`) rather than porcelain, and is signed with the writer's SSH key using git's native SSH signing (`gpg.format = ssh`, available since git 2.34, [git-config docs](https://git-scm.com/docs/git-config#Documentation/git-config.txt-gpgformat)).

**Reasoning.**

- *Reuse over reimplementation.* Versioning, diffing, three-way merge, tamper-evident hashing, signed history, incremental transport, bundles for offline transfer, hooks, and a mature server ecosystem all come for free and are audited by a very large community. Building a fraction of this from scratch would dwarf the rest of the codebase.
- *Audit.* `git log --show-signature` is the audit log. Each commit's signer identity distinguishes the human from each agent identity (section 7.3), and the commit trailer records harness, session and model. This is verifiable offline by anyone with the repo, with no trust in the service.
- *Air-gapped operation.* `git bundle create` / `git bundle verify` ([git-bundle docs](https://git-scm.com/docs/git-bundle)) moves history over removable media with integrity checking built in.
- *Security.* Git's object model is content-addressed; the hub cannot silently alter history without breaking signatures and hashes the client verifies on fetch.

**Why plumbing, not porcelain.** Porcelain commands read user config, run hooks, and can prompt. The write path must be deterministic, non-interactive, and immune to a user's global hooks or aliases. Plumbing gives that and is the stable scripting interface git documents for this purpose ([git internals, plumbing and porcelain](https://git-scm.com/book/en/v2/Git-Internals-Plumbing-and-Porcelain)).

**Why shell out to `git` rather than link libgit2 or gitoxide.** The brief prioritizes reusing native tools and a slim core. Git is present on effectively every developer machine and container base image, the plumbing interface is stable, and git is never on the search hot path (only on write and sync). The cost is a process spawn per write (well within the 50 ms target) and a hard dependency on `git ≥ 2.34` for SSH signing. **[judgment]** If profiling later shows write latency matters (bulk imports), a gitoxide backend can be added behind the same trait without changing the on-disk format.

**Change detection: the index stat cache, not per-file hashing.** agent-wkp detects change by running `git hash-object` on every file. That is a full content read per file per scan. The replacement is git's own index: `git update-index --refresh` followed by `git status --porcelain=v2` (or `git diff-index --cached`) answers "which files changed" from cached stat metadata (size, mtime, inode), and only those files are hashed and re-indexed. Where the builtin fsmonitor daemon is available (`core.fsmonitor=true`, macOS and Windows since git 2.37; Linux builtin support is newer and must be verified against the release notes of the minimum supported git version **[unverified]**), even the stat walk is skipped. This is the same technique that makes Mutagen's scans fast, and it is the main lever for the 30 ms incremental-index target in 4.3.

**Git settings applied by `wkp init`.** `protocol.version=2` (no full ref advertisement on fetch, [git protocol v2](https://git-scm.com/docs/protocol-v2)), `git maintenance start` with `commit-graph` enabled so `git log` on the audit path stays fast on long histories ([git-maintenance](https://git-scm.com/docs/git-maintenance)), `receive.fsckObjects` and `transfer.fsckObjects` on, `core.untrackedCache=true`.

**Accepted failure modes.** Git is poor at very large binary blobs; WKP stores markdown only and rejects attachments above a configurable size. Git history grows monotonically; `wkp gc` wraps `git gc` and, for privacy deletion, a documented history-rewrite procedure (section 7.6).

### 5.2 D4a: The index is derived, local, and never synced

**Decision.** `index.db` (SQLite) is rebuilt from the store on any machine in seconds and is `.gitignore`d, exactly as agent-wkp does today. Only the store syncs.

**Reasoning.** Syncing a binary index would reintroduce merge conflicts on a derived artifact, leak a plaintext copy of encrypted content, and couple every client to one SQLite version. Keeping the index derived means the store is the single source of truth and the index can be redesigned without a migration.

### 5.3 D4b: BM25 via SQLite FTS5 is the default; semantic search is an opt-in adapter

**Decision.** Retrieval is FTS5 with BM25 ranking ([SQLite FTS5 docs](https://www.sqlite.org/fts5.html)), `unicode61` tokenizer with `porter` stemming for the `content` column and an additional `trigram` tokenizer column for substring matches on identifiers and paths. Frontmatter fields are regular columns used for filtering (`type`, `workspace`, `visibility`, `scope`, `tags`) and for a small, explicit boost on `title` and `tags`. Hybrid search (Reciprocal Rank Fusion with a remote embedding endpoint) remains available exactly as in agent-wkp, but only when the user configures an endpoint, and it never runs in the session-start path.

**Reasoning.**

- *Latency.* FTS5 queries on tens of thousands of short documents complete in single-digit milliseconds inside the same process; an embedding call adds a model load or a network round trip and a nondeterministic dependency.
- *Fit for the corpus.* Agent memory is short, keyword-dense, and written in the user's own vocabulary (project names, file paths, ticket IDs, people). Lexical retrieval is strong precisely on this kind of query. The literature on hybrid retrieval shows BM25 remains a robust baseline and that dense retrievers underperform out of domain without adaptation (Thakur et al., BEIR, NeurIPS 2021 Datasets and Benchmarks, [arXiv:2104.08663](https://arxiv.org/abs/2104.08663)). This is **established** for general retrieval; its transfer to agent-memory corpora specifically is **emerging** and should be validated with the user's own query logs.
- *Security and offline.* No model files to trust or update, no query text leaving the machine by default.
- *When semantic is right.* Paraphrase-heavy recall across a large knowledge corpus (not memory), and only when a low-latency embedding service is available: local (Ollama, llama.cpp server on the same machine) or a hosted endpoint with a measured p95 under the search budget. The RRF fusion, dimension check and BM25 fallback already in agent-wkp carry over.

**Static SQLite vs. system SQLite.** Statically bundle SQLite (via `rusqlite` with the `bundled` feature) rather than linking the OS library. This is a deliberate exception to "reuse native tools": FTS5 and the `trigram` tokenizer availability, and query-planner behavior, vary across OS-shipped SQLite versions, and the index format must be identical on every machine that rebuilds it. **[judgment]** The cost is tracking SQLite CVEs in our own SBOM instead of the OS's (section 9.3).

**Alternative considered.** Tantivy (Rust Lucene-like) is faster on large corpora and supports richer scoring, but adds a second on-disk format, a larger dependency tree and no SQL for metadata filtering. Not justified below roughly a million items. **[judgment]**

### 5.4 Data model: extending the OKF frontmatter

agent-wkp's OKF frontmatter (`title`, `type`, `workspace`, `visibility`, `tokens`, `tags`, `refs`, `updated`) stays as-is for compatibility. WKP Hub adds fields required for cross-harness, cross-machine and multi-writer operation:

```yaml
---
title: Postgres chosen for control plane
type: knowledge            # project-state | knowledge | reference | feedback | skill | instruction | memory
scope: project             # user | project | org        (new: who the item is about / for)
workspace: wkp-hub
visibility: private        # shared | private            (private => client-side encrypted)
provenance:                # new: who wrote it, from where
  actor: agent:claude-code # human:<id> | agent:<harness>[:<model>]
  session: 7f3c…           # opaque, harness-provided
  source: conversation     # conversation | file | tool | import
confidence: stated         # stated | inferred | proposed  (new: mirrors the "did the user say it" test)
expires: null              # new: optional ISO date for facts that go stale
tokens: ~120
tags: [architecture, decision]
refs: []
updated: 2026-09-05
---
```

**Reasoning.** `provenance` and `confidence` exist because memory written by agents across harnesses is untrusted input to every other harness (section 7.4). `scope` exists because the same store must answer "what do I know about the user" and "what do I know about this project" without mixing them, and because org scope is the multi-user extension point. `expires` exists because facts uncovered during sessions rot; the indexer excludes expired items from Tier 0 and Tier 1 automatically. The `instruction` and `memory` types are added so Tier 0 assembly can apply stricter rules to instruction-like content (section 7.4).

**Store layout (convention, not enforced).**

```
store/
  user/            scope: user     (profile, preferences, people, interests)
  projects/<name>/ scope: project  (project-state, decisions, knowledge)
  org/<name>/      scope: org      (shared team knowledge; hosted multi-user only)
  inbox/           agent-written, unreviewed memory (Tier 2 only until promoted)
  .wkp/config.toml
```

One subject per file is a hard convention because it is the primary conflict-avoidance mechanism for sync (section 6.2).

---

## 6. Sync model

### 6.1 D5: Git transport is the sync protocol

**Decision.** Sync is `git fetch` / `git push` against a remote (the hub, or any git server the user already trusts, including a bare repo on a NAS or a private GitHub repository). Air-gapped sync is `git bundle`. The hub adds identity, device management, backup and optional server-side indexing; it does not add a protocol.

**Reasoning.** Every alternative (custom REST sync, CRDT log, rsync) either rebuilds what git already does or loses the audit and signature properties. Using the git smart protocol also means the local mode gains multi-machine sync **without** the hosted service, which is honest to the "user owns the memory" positioning and removes a reason to distrust the product. The hosted value must stand on convenience, identity and backup.

### 6.2 Conflict strategy: avoid, then merge structurally, then ask

1. **Avoid.** One subject per file, append-oriented writes, and agent writes routed to `inbox/<actor>/<date>-<slug>.md` (unique paths, so two harnesses on two machines never write the same file concurrently).
2. **Merge structurally.** A custom git merge driver (`.gitattributes: *.md merge=wkp`) that understands frontmatter: union of `tags`, max of `updated`, keep both bodies with provenance markers when the body conflicts. Git's merge-driver mechanism is documented and stable ([gitattributes, defining a custom merge driver](https://git-scm.com/docs/gitattributes#_defining_a_custom_merge_driver)).
3. **Ask.** Remaining conflicts are left as standard conflict markers and surfaced by `wkp sync status`; the harness can be asked to resolve them as a normal task, since resolving a markdown conflict is well within what an agent does routinely.

**Safe-mode semantics (adopted from Mutagen).** The merge driver's defaults follow Mutagen's `two-way-safe` rule: a conflict is resolved automatically only when nothing is lost, and a deletion always loses to a modification. Concretely: modify-vs-delete keeps the modification (the deletion is re-proposed as an inbox item for the human to confirm); add-vs-add on the same path keeps both bodies with provenance markers; frontmatter merges by field rule (union of `tags`, max of `updated`, keep both `provenance` entries). Nothing is ever discarded by the driver. Mutagen's `one-way-replica` mode is the right shape for org-scope reference knowledge distributed hub → devices: implemented as a fetch-only remote plus a protected ref, with no new code.

**Why not CRDTs.** A CRDT would guarantee automatic convergence, at the cost of a second data model, a custom storage format, loss of human-readable history, and a large dependency. Memory writes are low-frequency and mostly disjoint; git's three-way merge plus structural avoidance covers the realistic cases. **[judgment]** Revisit only if telemetry shows conflict rates above a threshold that users actually notice.

### 6.3 Why not a Mutagen-style continuous file sync engine

Mutagen (and similar tools) solve continuous, sub-second, bidirectional mirroring of an arbitrary working tree using a three-way reconciliation against a persisted "most-recently agreed-upon" snapshot, rsync-style deltas, and watch-triggered cycles ([Mutagen synchronization docs](https://mutagen.io/documentation/synchronization)). Compared with git for this workload:

| Property | Mutagen-style engine | Git (this design) |
|---|---|---|
| Reconciliation | Three-way against an in-memory ancestor snapshot | Three-way against the merge base, in persisted history |
| History and audit | None | Signed, offline-verifiable (D3) |
| Encryption at rest | None | age filter (D6) |
| Transfer efficiency | rsync deltas; strong on large binaries | Packfile deltas; strong on many small text files |
| Remote requirement | Pushes its own agent binary over SSH | `git` on the remote, so any NAS or git host works unmodified |
| Air-gapped | No | `git bundle` |
| Sub-second propagation | Yes | No; commit granularity |

The one property a continuous engine wins is sub-second propagation between concurrently running agents on different machines, which is a v1 non-goal (1.3). Adopting such an engine would mean a second reconciliation model with no history, signatures or encryption, which is the security cliff the design exists to avoid. The techniques worth having (stat-cache scanning, watch-triggered cycles, safe-mode conflict defaults, stage-then-atomic-apply) are adopted in 5.1, 4.2 and 6.2 using git's own mechanisms. **[judgment]**

### 6.4 Registration and tiering

- `wkp hub register` runs the OAuth 2.0 Device Authorization Grant ([RFC 8628](https://www.rfc-editor.org/rfc/rfc8628)) so the CLI never handles a password or a browser redirect; the device then generates an `ed25519` key stored in the OS keystore (macOS Keychain; Linux `secret-service` via the freedesktop API, falling back to `ssh-agent`) and uploads only the public key.
- The store's `origin` is set to the hub; local mode continues to work unchanged. Sync is opportunistic: `wkpd` (if present) or the SessionStart hook attempts a fetch with a short timeout and never blocks the session on network availability.
- Multiple workstations are simply multiple clones with their own device keys. Revoking a device on the hub removes its key from `AuthorizedKeysCommand` output (section 8.1) and takes effect on the next connection.

---

## 7. Security architecture (local through hosted)

### 7.1 Threat model

**Assets.** Memory content (may include PII, project secrets, accidentally pasted credentials); integrity of Tier 0 (whatever is injected at session start shapes every agent action); device keys; hub credentials; the maintainers' signing keys and CI.

**Adversaries and vectors.**

| Adversary | Vector | Primary controls |
|---|---|---|
| Malicious or compromised local process | Reads store or index, talks to `wkpd` | File permissions `0700`, UDS peer-cred check, OS sandbox for `wkp` (7.5) |
| Hostile content reaching an agent | Prompt injection via a synced memory file that another harness wrote or that arrived from a web page | Provenance gating for Tier 0 (7.4), inbox quarantine, structural wrapping of injected context |
| Hub operator or hub compromise | Reads tenant data at rest, alters history | Zero-knowledge default (7.2), client-verified signatures and hashes, per-tenant isolation (8.2) |
| Network attacker | MITM on sync | SSH host-key pinning at registration, TLS 1.3 only for HTTPS transport |
| Stolen laptop | Offline read of store | Store encrypted at rest for `private` items; keys in OS keystore, not on disk in plaintext |
| Compromised or careless agentic maintainer | Ships vulnerable or malicious code | Pipeline gates, human co-sign rule, signed reproducible releases (9) |
| Supply chain | Vulnerable or hijacked dependency | `cargo-vet`, `cargo-deny`, SBOM + scanners, pinned and hash-locked builds (9.3) |

Prompt injection through agent-consumed content is **established** as a practical attack class (Greshake et al., "Not what you've signed up for", AISec 2023, [arXiv:2302.12173](https://arxiv.org/abs/2302.12173)); a shared memory layer is a natural carrier for it because content authored in one context is injected as trusted context into another. This is the threat unique to this product and the one the design spends the most on.

### 7.2 D6: Client-side encryption for private content; zero-knowledge hosted by default

**Decision.** Items with `visibility: private` are encrypted before they enter git, using [age](https://github.com/FiloSottile/age) (X25519 recipients, ChaCha20-Poly1305) through a git clean/smudge filter, implemented in-process with the `rage` crate ([str4d/rage](https://github.com/str4d/rage)). Recipients are the user's device keys plus an optional recovery key. In the hosted default, the hub stores only ciphertext for private items and plaintext for `shared` ones; the user can set a store-wide policy to encrypt everything (fully zero-knowledge).

**Reasoning.**

- *No security cliff.* The same encryption applies whether the remote is the hub, a NAS, or GitHub. Choosing hosted mode does not change what a third party can read.
- *age over PGP/gpg.* Small, modern, audited primitives, no web-of-trust or agent daemon, native multi-recipient support (which maps directly to "my devices"), and a Rust implementation so the core does not shell out to `gpg`.
- *age over git-crypt.* git-crypt uses deterministic AES-CTR with an HMAC-derived nonce so identical plaintext yields identical ciphertext (stable blobs, good for dedup, but leaks equality). age uses random nonces: every re-encryption produces a new blob, so unchanged private files must not be re-filtered (the indexer's blob-SHA cache already avoids this). **[judgment]** Equality leakage matters more than a few extra blobs for a memory store.

**Accepted failure modes.**

- The hub cannot index or search ciphertext. Cross-device search therefore works by having each device hold a clone and its own index; a device without a clone gets search only for `shared` items (or none in fully zero-knowledge mode). This is the honest trade: server-side search over private memory requires the server to hold plaintext or a key. An opt-in "hub-indexed" tier that holds a per-tenant key in a KMS/HSM and indexes in an isolated worker can be offered later; it must be presented as a downgrade of confidentiality, not as a feature upgrade.
- Loss of all device keys and the recovery key is loss of private content. The recovery key is generated at registration, shown once, and its storage is the user's responsibility; same posture as SSH keys.

### 7.3 Identity and signing

- Every writer (the human, each harness/agent identity) has an `ed25519` key. Humans use their existing SSH key; agent identities get keys generated per harness with names like `agent/claude-code@host`. Agent keys are stored in the keystore with an ACL that permits `wkp` to use them without exposing the private key to the harness process itself (macOS Keychain ACLs; Linux via `ssh-agent` confirmation or a dedicated agent socket).
- Commits are SSH-signed; `wkp verify` checks every commit on fetch against an `allowed_signers` file that the store carries ([git ssh signing, allowedSignersFile](https://git-scm.com/docs/git-config#Documentation/git-config.txt-gpgsshallowedSignersFile)). Unsigned or unknown-signer commits are accepted into history but their content is treated as `confidence: proposed` and excluded from Tier 0 and Tier 1.

### 7.4 D9: Provenance-gated injection (the prompt-injection control)

**Decision.** Tier 0 and Tier 1 (content injected at session start without a search) are assembled only from items whose latest commit is signed by a key marked `role: human` in `allowed_signers`, **and** whose `type` is not `instruction` unless the same condition holds and the file is under `user/` or `projects/<current>/`. Everything written by an agent lands in `inbox/` with `confidence: inferred|proposed`, is Tier 2 only (reachable by explicit `wkp search`), and is rendered inside a clearly delimited, provenance-labeled block (`<wkp-item provenance="agent:opencode" confidence="proposed">`). `wkp promote <path>` moves an inbox item into the durable tree with a human-signed commit.

**Reasoning.** This makes the memory layer a one-way valve for instruction-like content: agents can propose, only a human-signed commit can make something load unconditionally into another agent's context. It does not eliminate prompt injection (a human can promote a poisoned item, and Tier 2 results are still model input), but it removes the automatic, silent path from "content written by harness A" to "system-level context in harness B". Structural delimiting and provenance labels are the mitigations the injection literature and vendor guidance converge on; they are **practice**, not proof.

**Accepted failure mode.** Friction: a user who wants agents to self-improve their durable memory without review must opt into a policy (`promote: auto` for a specific harness key). The default is deliberately the safe one.

### 7.5 Process hardening for the local binary

- `wkp` drops capabilities it does not need: on Linux, a [Landlock](https://docs.kernel.org/userspace-api/landlock.html) ruleset restricting filesystem access to the store, the index and git's object directory, plus `seccomp` filtering of the syscall set (practice on modern kernels); on macOS, `sandbox-exec` profiles are deprecated but still functional for CLI tools and are used where available, with the hardened-runtime entitlements applied to the signed binary **[unverified for the specific macOS version in use; validate at build time]**.
- Secrets never touch argv or environment: keys are read from the keystore or a `0600` file, and hub tokens are passed over stdin.
- `wkpd`, when used, runs as the user under `launchd` (macOS) or a `systemd --user` unit with `ProtectSystem=strict`, `ProtectHome=read-only` plus explicit `ReadWritePaths`, `NoNewPrivileges=yes`, `PrivateNetwork=yes` when sync is disabled ([systemd.exec hardening options](https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html)).
- The container image is `FROM scratch`, non-root UID, read-only root FS, no shell.

### 7.6 Secrets in memory, deletion and privacy

- **Write-path secret detection.** `wkp remember` and the pre-commit path run [gitleaks](https://github.com/gitleaks/gitleaks) rules (bundled as data, evaluated in-process) and refuse to commit content that matches credential patterns, returning a redacted preview so the harness can retry. Reuses a widely deployed rule set rather than inventing one.
- **Deletion.** `wkp forget <path>` removes the file and, for private items, rotates the age recipients so old ciphertext becomes unreadable to revoked devices. True erasure from history requires `wkp purge`, a wrapped, documented `git filter-repo` procedure followed by forced push and re-clone on every device; the hub honors a purge by deleting unreachable objects immediately and its backups on their retention schedule. This is stated plainly to users rather than hidden.
- **Retention.** Index files and hub-side caches are derived and deleted with the tenant. Hub backups are encrypted with a service key and retained for a documented window.

---

## 8. Hosted hub

### 8.1 D7: OpenSSH + git's own server binaries as the transport front end

**Decision.** The hub's git endpoint is unmodified `sshd` with `AuthorizedKeysCommand` (looks up the presented public key in the control plane and returns an `authorized_keys` line with `command=`, `restrict` options) and a small `wkp-shell` that maps the key to a tenant and executes only `git-receive-pack` / `git-upload-pack` on that tenant's bare repository ([sshd_config](https://man.openbsd.org/sshd_config), [git-shell](https://git-scm.com/docs/git-shell)). HTTPS transport uses `git http-backend`, the CGI program shipped with git ([git-http-backend](https://git-scm.com/docs/git-http-backend)), behind a reverse proxy that validates a device-scoped bearer token and sets `REMOTE_USER`.

**Reasoning.** This is the gitolite pattern: the parts that face the network are OpenSSH and git, two of the most scrutinized codebases in existence, and the custom surface is a few hundred lines of key-to-tenant mapping. It also inherits every hardening option of `sshd` (`PermitRootLogin no`, `PasswordAuthentication no`, `AllowAgentForwarding no`, `AllowTcpForwarding no`, `PermitTTY no`, key-type restriction to `ed25519`).

**Alternatives considered.**

- *Forgejo/Gitea or Soft Serve.* Full git hosting with UI and API; far more surface than needed (web UI, issues, wiki, OAuth providers) and a much larger CVE stream to track. Reasonable if a hosted UI over the repos becomes a product requirement. **[judgment]**
- *Custom protocol server in Rust (russh + gitoxide).* Full control and no process spawn per connection, at the cost of owning an SSH implementation's security. Not justified at micro-SaaS scale.

### 8.2 D8: Per-tenant isolation with per-tenant SQLite; Postgres only for the control plane

**Decision.** Each tenant gets a bare repo under its own Unix user (or a per-tenant user namespace in the container runtime), a `post-receive` hook that runs `wkp index` into a per-tenant `index.db` for the `shared` (plaintext) subset, and quotas enforced by filesystem quota. The control plane (accounts, devices, keys, subscriptions, audit events) is a small Postgres schema behind the same Rust binary running in `hub` mode. Billing is delegated to a Merchant of Record (Paddle or Lemon Squeezy) via webhooks; the core never sees card data.

**Reasoning.**

- *Isolation is the multi-tenancy model.* One bare repo, one index file, one UID per tenant means a bug in the search code cannot cross tenants without a kernel-level escape, tenant deletion is `rm -rf` plus a control-plane row, and there is no shared table for a query bug to leak across. Per-tenant SQLite files also mean the hub's search path is literally the same code as the laptop's, which halves the code to secure.
- *Postgres only where multi-tenant transactions are real.* Accounts, device keys and subscription state need a real transactional store with backups; that is a few tables, well served by managed Postgres (Supabase or RDS both fit; Supabase Auth can back the device-grant flow, which is where the prior micro-SaaS analysis remains valid).
- *The FastAPI + Next.js + Supabase pattern.* It is a sound pattern for the account and billing web surface, and it is deliberately kept out of the core: the web app talks to the control plane, never to tenant repos or indexes. A compromise of the web tier yields account metadata, not memory content.

### 8.3 Hub hardening summary

| Layer | Control |
|---|---|
| Network | Only 22 (sshd) and 443 (proxy) exposed; proxy terminates TLS 1.3, rate-limits, forwards to `http-backend` over a UDS |
| Auth | Key-only SSH; device-scoped tokens for HTTPS; device revocation immediate |
| Tenant | Own UID, own quota, own bare repo, own index; `receive.fsckObjects=true`, `transfer.fsckObjects=true` so malformed objects are rejected on push |
| Process | Indexing worker is a separate sandboxed process (Landlock + seccomp) with no network |
| Data | Ciphertext for private items; plaintext `shared` items only; backups encrypted |
| Observability | Structured audit events (who pushed what ref when) to the control plane; no content logging |

---

## 9. Development process and pipeline (agentic maintainers)

### 9.1 Principle: agents are untrusted contributors with excellent tooling

Agentic maintainers (Hermes Agent, OpenCode, dsh, Claude Code) get the same rights as an external contributor: they can open PRs, run CI, and read review feedback. They cannot merge, release, sign, or hold long-lived secrets. Every gate below is automated so the review signal is machine-readable and can be fed back to the agent as the next task. A human must co-sign any change that touches: the crypto, the sync/merge driver, the sandbox rules, the hub's key-to-tenant mapping, or the CI workflows themselves (CODEOWNERS enforced).

### 9.2 Repository and branch controls

- Branch protection with required status checks, linear history, signed commits required, and no force-push on `main`.
- GitHub Actions pinned to full commit SHAs, `permissions:` set to least privilege per job, and no secrets exposed to workflows triggered from forks.
- Agent runners execute inside ephemeral containers with no credentials beyond a short-lived, PR-scoped token; they never see release-signing keys.
- [OpenSSF Scorecard](https://github.com/ossf/scorecard) run on every push with a minimum score gate; [Allstar](https://github.com/ossf/allstar) or equivalent policy enforcement for repo settings drift.

### 9.3 Dependency and supply-chain gates (CVE detection)

| Gate | Tool | What it catches |
|---|---|---|
| Known-vulnerable crates | [cargo-audit](https://github.com/rustsec/rustsec) against the RustSec DB | CVEs and unmaintained advisories |
| License, source, duplicate and ban policy | [cargo-deny](https://github.com/EmbarkStudios/cargo-deny) | Disallowed licenses, unknown registries, banned crates, advisories |
| Human/agent audit of dependency diffs | [cargo-vet](https://github.com/mozilla/cargo-vet) | New or updated crates must carry an audit record; imports Mozilla/Google audits |
| Cross-ecosystem vulnerability match | [OSV-Scanner](https://github.com/google/osv-scanner) on the lockfile and SBOM | Vulnerabilities in transitive deps, including bundled SQLite |
| Container image and OS package scan | [Trivy](https://github.com/aquasecurity/trivy) and [Grype](https://github.com/anchore/grype) | Image CVEs, misconfigurations, secrets in layers |
| SBOM generation | [Syft](https://github.com/anchore/syft) producing CycloneDX and SPDX, attached to every release | Inventory for continuous rescans after release |
| Continuous rescan | Scheduled job re-runs OSV/Grype against every supported release's SBOM | New CVEs in already-shipped versions; opens an issue automatically |
| Lockfile integrity | `Cargo.lock` committed; `--locked` in CI; Renovate/Dependabot PRs with grouped, auto-tested updates | Silent dependency drift |

Bundling SQLite (5.3) means SQLite CVEs are our responsibility; the SBOM makes them visible to the rescan job, and the `bundled` crate version is pinned and updated through the same PR gate.

### 9.4 Static and dynamic analysis (proactive vulnerability detection)

- **Compiler and lints.** `#![forbid(unsafe_code)]` in every crate except an explicitly named `sys` crate; `clippy` with `pedantic` and the security-relevant lints as errors; `cargo miri` on any crate that does contain `unsafe`.
- **SAST.** [Semgrep](https://github.com/semgrep/semgrep) with the Rust and secrets rulesets plus custom rules for this codebase (for example: any `Command::new` must go through the `git::Plumbing` wrapper; no `std::env::var` for secrets). [CodeQL](https://codeql.github.com/) for the Rust and the small TypeScript surface.
- **Fuzzing.** [cargo-fuzz](https://github.com/rust-fuzz/cargo-fuzz) targets for every parser that consumes untrusted input: frontmatter, FTS5 query construction, merge-driver input, age filter I/O, `wkp-shell` argument parsing, hub webhook payloads. Continuous fuzzing via [OSS-Fuzz](https://github.com/google/oss-fuzz) once the project is public.
- **Secrets.** gitleaks in pre-commit and CI; the same rules the product uses at runtime (7.6), so the rule set is tested by its own pipeline.
- **Property tests.** `proptest` for the merge driver (merge is commutative and idempotent on the structural fields) and for the tier assembler (never exceeds budget, never includes an item failing the provenance gate).
- **Injection regression suite.** A corpus of known prompt-injection payloads is committed as memory files; a CI test asserts none of them can reach Tier 0 or Tier 1 output without a human-signed commit. This converts the product's central security claim into a test.

### 9.5 Performance and correctness gates

- Criterion benchmarks for the operations in 4.3, with a regression threshold that fails the PR.
- Golden tests for `tier0.md` output across harness adapters.
- An integration test that spins up `sshd` + `wkp-shell` in a container and exercises register, push, revoke, push-must-fail.

### 9.6 Release integrity

- Reproducible builds (`cargo` with pinned toolchain via `rust-toolchain.toml`, `SOURCE_DATE_EPOCH`, `--locked`), verified by building twice on independent runners and comparing hashes.
- Artifacts (binaries, container images, SBOMs) signed with [Sigstore cosign](https://github.com/sigstore/cosign) using keyless OIDC identities tied to the release workflow, with [SLSA](https://slsa.dev/) provenance attestations generated by the build (target SLSA Build L3 via the official generators). The `wkp` binary verifies its own update channel against these signatures before applying an update.
- **Distribution channels (decision).** Exactly three: GitHub Releases (signed archives plus SBOM and attestations, the trust root for everything else), a Homebrew tap (macOS and Linux; brew verifies checksums and carries updates), and multi-arch OCI images (`linux/amd64`, `linux/arm64`) from `scratch`. Rejected for now: PyPI wheel (no audience), `curl | sh` installer (a trust-on-first-use root that adds surface without adding users at this stage), OS package repos (revisit when there are users on `dnf`/`apt` who ask). The binary's own update check verifies release signatures, so a user on an old build learns about a fix regardless of channel.
- **Binary optimization targets.** Release profile with `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`, `opt-level = 3` (or `"s"` where measured startup is unaffected), symbols stripped, no allocator or async runtime unless a measured need appears; target under 10 MB on disk and the startup and RSS numbers in 4.3. Size and startup are tracked by the same CI benchmark gate as query latency.

### 9.7 Vulnerability handling

- `SECURITY.md` with a private reporting channel (GitHub private vulnerability reporting), a 90-day disclosure norm, and CVE issuance through the GitHub CNA.
- An advisory in the RustSec DB for the `wkp` crates if they are published to crates.io.
- Every fixed vulnerability gets a regression test and, where it was a parser bug, a new fuzz seed.

### 9.8 Working agreement for agentic maintainers

- Task specs live in the repo as issues with acceptance criteria that CI can check; agents are pointed at issues, not at prose instructions in chat.
- An agent's PR description must include the commands it ran and their output (test, clippy, audit); CI re-runs them regardless.
- Agents review each other's PRs, but a human signs off on the CODEOWNERS paths. Rationale: the code-security evidence in 4.1 shows AI-generated code needs an independent check, and a second agent with the same training biases is a weak independent check. **[judgment]**
- The harness the agent runs in must itself be sandboxed (no network except the git remote and the package registries, no access to signing keys). The project dogfoods its own local mode: the agents' memory of the codebase is a WKP store, so architecture decisions and past incidents load at session start.

---

## 10. Harness adapters

The core contract is: a hook that runs `wkp index --quiet && cat "$(wkp path tier0)"` at session start, and a Bash-callable `wkp search` / `wkp context` mid-session, exactly as agent-wkp does for Claude Code today via `.claude/settings.local.json` and `.claude/hooks/`.

| Harness | Injection | On-demand | Notes |
|---|---|---|---|
| Claude Code | `SessionStart` hook, `UserPromptSubmit` for hot files | Bash | Existing adapter; `wkp hooks --framework claude_code` |
| OpenCode | `AGENTS.md` include of the materialized Tier 0 file plus plugin hook where available | Bash | **[unverified]** exact hook API; validate against OpenCode docs before implementation |
| Hermes Agent, dsh | Startup command or system-prompt file include | Bash | **[unverified]** integration surfaces; I do not have primary documentation for either at hand |
| Any harness with MCP but no hooks | Optional stdio MCP server built into the same binary (`wkp mcp`) | MCP tools | Costs tool-definition tokens per session; offered as fallback only |
| Harness in a container | Mount store + optional `wkpd` socket | Bash | No ports |

Memory writes from a harness use `wkp remember --actor agent:<harness> --session <id> <<EOF ... EOF` (stdin, never argv), which lands in `inbox/` under the provenance rules in 7.4. Importers for existing harness memory formats (`~/.claude/projects/*/memory/`, `CLAUDE.md`, `AGENTS.md`) run once at `wkp init` and tag imported items with `source: import`.

---

## 11. Open questions and research items

1. **Retrieval quality on memory corpora.** BM25's strength on this corpus is an inference from general retrieval literature, not measured. Instrument `wkp search` (opt-in, local only) to log query/click pairs and evaluate BM25 vs. hybrid on real logs before revisiting 5.3.
2. **Provenance gate usability.** The human co-sign requirement for Tier 0 promotion may be too much friction for solo users. Measure promotion latency and the rate at which users switch to `promote: auto`.
3. **macOS sandboxing.** `sandbox-exec` deprecation status and the practical alternative for a CLI tool need verification on the current macOS release.
4. **Hub-indexed tier.** Whether the confidentiality downgrade is acceptable to enough users to justify building the KMS-backed indexing worker.
5. **Merge-driver semantics for agent-written facts.** Whether "keep both with provenance markers" produces readable files in practice, or whether inbox-only writes make the driver rarely exercised.
6. **Org scope.** Multi-user sharing introduces authorization inside a repo (who may read which path). Git has no per-path ACL; the options are one repo per scope, or encryption recipients per path. Deferred.

---

## 12. Decision log summary

| Decision | Chosen | Rejected | Deciding factor |
|---|---|---|---|
| Core language | Rust static binary | Go, Python | Startup latency, memory safety, supply-chain tooling |
| Local process model | Daemonless + optional UDS `wkpd` | Loopback HTTP daemon | Local attack surface |
| Store | Git repo of markdown | Custom DB, CRDT log | Reuse of versioning, audit, sync, bundles |
| Git access | Shell out to plumbing | libgit2, gitoxide | Native-tool reuse; git never on hot path |
| Index | Derived SQLite, never synced | Synced index | Single source of truth, no plaintext leak |
| Retrieval | FTS5 BM25, trigram column | Vector-first, Tantivy | Latency, offline, corpus fit |
| SQLite linkage | Bundled | System library | FTS5/trigram consistency across machines |
| Sync | Git transport + bundles | REST sync, CRDT, Mutagen-style continuous engine | No new protocol; works without the hub; history, signatures and encryption preserved |
| Change detection | Git index stat cache, fsmonitor where available | Per-file `git hash-object` | Scan cost proportional to changes, not corpus |
| Repo structure | Single Cargo workspace monorepo | Per-component repos | Shared security-critical crates; one CI verdict per PR |
| Encryption | age via clean/smudge, zero-knowledge default | git-crypt, server-side keys | Equality leakage, no security cliff |
| Injection control | Human-signed gate for Tier 0/1 | Trust all synced content | Cross-harness prompt injection |
| Hub transport | OpenSSH + git-shell + http-backend | Forgejo, custom SSH server | Minimum custom network surface |
| Hub tenancy | Per-tenant UID, repo, SQLite | Shared Postgres for content | Isolation by construction |
| Billing | Merchant of Record | Direct Stripe | Tax and compliance offload; no card data in core |
| Distribution | GitHub Releases, Homebrew tap, OCI image | PyPI wheel, `curl \| sh` installer, OS repos | Single current user; optimize the binary, not the installer |
| Pipeline | Automated gates + human co-sign on critical paths | Agent-only review | Evidence on AI-generated code security |

---

## References (primary sources)

- agent-wkp repository and README: https://github.com/williamcaban/agent-wkp
- agent-wkp architecture: https://github.com/williamcaban/agent-wkp/blob/main/docs/architecture.md
- Memory layers comparison: https://github.com/RyanAlberts/best-of-Agent-Harnesses/blob/main/comparisons/memory-layers.md
- SQLite FTS5: https://www.sqlite.org/fts5.html
- Git plumbing and porcelain: https://git-scm.com/book/en/v2/Git-Internals-Plumbing-and-Porcelain
- Git SSH signing (`gpg.format`, `gpg.ssh.allowedSignersFile`): https://git-scm.com/docs/git-config
- Git bundle: https://git-scm.com/docs/git-bundle
- Git custom merge driver: https://git-scm.com/docs/gitattributes
- git-shell: https://git-scm.com/docs/git-shell ; git-http-backend: https://git-scm.com/docs/git-http-backend
- OpenSSH sshd_config: https://man.openbsd.org/sshd_config
- OAuth 2.0 Device Authorization Grant, RFC 8628: https://www.rfc-editor.org/rfc/rfc8628
- age: https://github.com/FiloSottile/age ; rage: https://github.com/str4d/rage
- Landlock: https://docs.kernel.org/userspace-api/landlock.html
- systemd.exec hardening: https://www.freedesktop.org/software/systemd/man/latest/systemd.exec.html
- Mutagen synchronization: https://mutagen.io/documentation/synchronization
- Git protocol v2: https://git-scm.com/docs/protocol-v2 ; git-maintenance: https://git-scm.com/docs/git-maintenance ; git-update-index (fsmonitor, untracked cache): https://git-scm.com/docs/git-update-index
- Thakur et al., BEIR (NeurIPS 2021 D&B): https://arxiv.org/abs/2104.08663
- Greshake et al., indirect prompt injection (AISec 2023): https://arxiv.org/abs/2302.12173
- Pearce et al., "Asleep at the Keyboard?" (IEEE S&P 2022): https://arxiv.org/abs/2108.09293
- Perry et al., "Do Users Write More Insecure Code with AI Assistants?" (CCS 2023): https://arxiv.org/abs/2211.03622
- RustSec / cargo-audit: https://github.com/rustsec/rustsec ; cargo-deny: https://github.com/EmbarkStudios/cargo-deny ; cargo-vet: https://github.com/mozilla/cargo-vet
- OSV-Scanner: https://github.com/google/osv-scanner ; Trivy: https://github.com/aquasecurity/trivy ; Grype: https://github.com/anchore/grype ; Syft: https://github.com/anchore/syft
- Semgrep: https://github.com/semgrep/semgrep ; CodeQL: https://codeql.github.com/ ; cargo-fuzz: https://github.com/rust-fuzz/cargo-fuzz ; OSS-Fuzz: https://github.com/google/oss-fuzz
- gitleaks: https://github.com/gitleaks/gitleaks
- OpenSSF Scorecard: https://github.com/ossf/scorecard ; Allstar: https://github.com/ossf/allstar
- Sigstore cosign: https://github.com/sigstore/cosign ; SLSA: https://slsa.dev/
