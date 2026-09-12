# agent-wkp — Workspace Knowledge Protocol

`agent-wkp` is being rewritten from a Python CLI into `wkp`, a single static
Rust binary that gives any agentic harness durable, cross-machine,
cross-harness memory: a git repository of markdown files, a derived SQLite
FTS5 index, and optional sync to a hosted hub.

This branch (`v2-rust`) is that rewrite. M0 through M5 are substantively
done: read-path parity (`wkp init`, `index`, `search`, `context`,
`materialize`, `hooks`), the write/audit path (`wkp remember`/`promote`,
signed commits), local multi-machine sync, age encryption for private
items, and a hosted hub (per-tenant pods, HTTPS with mutual TLS, device
registration and revocation) all exist and are exercised in CI. M6
(hardening and release: sandboxing, signed reproducible releases, a
Homebrew tap, an OCI image) has not started. See `docs/plan/milestones.md`
for exactly what each milestone's own exit criterion holds and what's
still open.

- [`CLAUDE.md`](CLAUDE.md) — the working agreement for anyone (human or
  agent) contributing code on this branch.
- [`docs/design/wkp-hub-design-v0.1.md`](docs/design/wkp-hub-design-v0.1.md) —
  the authoritative design.
- [`docs/plan/milestones.md`](docs/plan/milestones.md) — milestones and
  per-milestone task lists.
- [`AGENTS.md`](AGENTS.md) — how an agent uses the `wkp` CLI.

## The previous Python implementation

The original Python `agent-wkp` (progressive-disclosure knowledge index with
BM25/semantic search over SQLite) is frozen at the [`v0-python`
tag](https://github.com/williamcaban/agent-wkp/tree/v0-python) and on the
`main` branch's history. It is not maintained going forward; this rewrite
supersedes it.

## License

Apache 2.0 — see [LICENSE](LICENSE).
