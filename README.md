# agent-wkp — Workspace Knowledge Protocol

`agent-wkp` is being rewritten from a Python CLI into `wkp`, a single static
Rust binary that gives any agentic harness durable, cross-machine,
cross-harness memory: a git repository of markdown files, a derived SQLite
FTS5 index, and optional sync to a hosted hub.

This branch (`v2-rust`) is that rewrite in progress. The Cargo workspace,
CI baseline, and the git minimum-version gate are in place (M0); read-path
parity with the old tool (`wkp init`, `index`, `search`, `context`,
`materialize`, `hooks`) lands in M1. See:

- [`CLAUDE.md`](CLAUDE.md) — the working agreement for anyone (human or
  agent) contributing code on this branch.
- [`docs/design/wkp-hub-design-v0.1.md`](docs/design/wkp-hub-design-v0.1.md) —
  the authoritative design.
- [`docs/plan/milestones.md`](docs/plan/milestones.md) — milestones and
  per-milestone task lists.
- [`AGENTS.md`](AGENTS.md) — how an agent uses the `wkp` CLI once it exists
  (currently describes the Python tool; rewritten for the Rust CLI in M1).

## The previous Python implementation

The original Python `agent-wkp` (progressive-disclosure knowledge index with
BM25/semantic search over SQLite) is frozen at the [`v0-python`
tag](https://github.com/williamcaban/agent-wkp/tree/v0-python) and on the
`main` branch's history. It is not maintained going forward; this rewrite
supersedes it.

## License

Apache 2.0 — see [LICENSE](LICENSE).
