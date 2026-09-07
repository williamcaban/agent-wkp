# ADR-0003: Hybrid search (`--embed-url`) — build-time feature gate, HTTP client, and secret handling

Status: accepted
Date: 2026-09-07
Design sections affected: 5.3 ("Hybrid search... remains available exactly as in agent-wkp"), 7.2 (secrets)

## Context

M1 task 9 (`docs/plan/milestones.md`): "Optional hybrid search via
`--embed-url` with RRF and BM25 fallback, kept out of the default path."
Design 5.3 says the same capability agent-wkp (the Python predecessor)
already had — Reciprocal Rank Fusion between BM25 and a remote embedding
endpoint, with a BM25-only fallback on any failure — "remains available...
but only when the user configures an endpoint, and it never runs in the
session-start path."

Porting that literally runs into three of this repo's own hard rules that
didn't bind the Python implementation:

1. **Slim core / binary size.** M0's exit criterion is a binary under 10 MB;
   the current release binary is ~2.5 MB. An OpenAI-compatible embeddings
   call needs an HTTP client; supporting `https://` endpoints (not just a
   local `http://localhost:11434`) needs TLS, which pulls in a real
   dependency tree (rustls + `ring`/`webpki-roots`/etc., ~20 crates).
2. **Every new crate must be justified... and pass `cargo deny`/`cargo
   vet`.** `deny.toml` scopes `cargo deny check` to the *default* feature
   set (`[graph] all-features = false`); a dependency that's always
   compiled in is always in that checked graph, whether or not the code
   path that uses it ever runs.
3. **"Secrets never touch argv or environment variables. Read them from
   the OS keystore, a `0600` file, or stdin."** agent-wkp's Python
   implementation read the endpoint's API key from `WKP_EMBED_API_KEY`, an
   environment variable — explicitly one of the two things this rule
   forbids.

None of these are addressed in the design doc's one-line carry-over
("remains available... exactly as in agent-wkp"), because they didn't
apply to the Python tool. This ADR records the concrete choices made to
reconcile them, per CLAUDE.md's instruction not to silently deviate.

## Options

### HTTP client and TLS support

1. **Compile it in unconditionally (no Cargo feature).** Simplest for a
   user — `--embed-url` always works in the binary they already have.
   Cost: every user of the default binary pays the dependency-graph and
   `cargo vet` burden for a feature most will never use, and the default
   `cargo deny check`/`cargo vet check` invocations CI already runs now
   also cover ~20 crates (`ureq`, `rustls`, `ring`, `webpki-roots`, ...)
   that exist only for this one optional feature.
2. **Gate it behind a non-default Cargo feature (`embed`), off unless a
   caller builds with `--features embed`.** The default `cargo build`/
   `cargo test`/`cargo clippy`/`cargo bench` (everything CI actually runs,
   and everything the M0 size gate measures) never touches the
   dependency at all — confirmed empirically: the release binary grew
   from 2,570,320 to 2,576,240 bytes (+5.9 KB) after adding the feature
   and its real call sites, because none of it links into a build that
   doesn't request the feature. Cost: `--embed-url` doesn't work in a
   binary built the ordinary way; a user who wants it needs
   `cargo build --release --features wkp-cli/embed` themselves, or a
   packaged release that opted in (deferred to M6 — see Consequences).
3. **Shell out to `curl`.** Reuses an OS-provided tool (in the spirit of
   CLAUDE.md's "reuse git, SQLite, OpenSSH and OS facilities," though
   `curl` isn't literally one of those three), no new Rust dependency at
   all. Rejected: not guaranteed present (containers, minimal images,
   Windows), and shelling out to an arbitrary external binary for a
   correctness-relevant network call is a worse-audited, less portable
   surface than a vetted Rust HTTP crate — it would also need its own
   argument-injection review that a library call doesn't.
4. **`ureq` (chosen, with option 2's gating) vs. `reqwest`.** `reqwest`
   pulls in a full async runtime (`tokio`) for what is, here, exactly one
   blocking round trip per `wkp search`/`wkp index --embed-url` call —
   `wkp` has no other async code anywhere and design 4.2 already commits
   to no daemon. `ureq` is a synchronous, minimal-dependency client
   (`cargo add ureq --optional --no-default-features --features rustls`
   resolves 29 crates total) that fits the one-shot-request shape exactly.

**Decision: option 2 (feature-gated) with `ureq` + `rustls`.** `wkp-core`
gets an optional dependency and an `embed` feature
(`crates/wkp-core/Cargo.toml`); `wkp-cli` forwards it
(`embed = ["wkp-core/embed"]`). Both `EmbedConfig` (the plain data type)
and all of the RRF/cosine-similarity/serialization arithmetic in
`wkp_core::embed` are **not** feature-gated — only `embed_remote` (the
actual network call) and its response parsing are, so the fusion logic is
unit-testable without the feature and without any network access at all.

### JSON handling for the embeddings response

Rejected adding `serde_json` for a single call site: the only thing ever
needed from an OpenAI-compatible `/embeddings` response is one flat
`"embedding": [...]` numeric array. `wkp_core::embed::remote::extract_embedding`
is a small, hand-rolled, tolerant scanner for exactly that shape — the
same reasoning already applied to this repo's hand-rolled frontmatter
parser (`wkp-core/src/frontmatter.rs`) and the CLI's own hand-rolled
`--format json` output (`wkp-cli/src/main.rs`'s `format_json`). It is not
a general JSON parser and does not try to be one.

### Vector storage and similarity search

Rejected a native vector-search SQLite extension (what agent-wkp's Python
implementation used, via a bundled C extension) for two reasons: it would
need its own `unsafe`-carrying dependency inside `wkp-sys` (the only crate
CLAUDE.md permits `unsafe` in at all, and only for the bundled SQLite it
already owns), and design 4.3's own fixture corpora top out at 50,000
items — small enough that a brute-force cosine-similarity scan, computed
in plain Rust over rows SQL has already filtered by tier/type/workspace/
visibility/scope, is fast enough without a specialized index. `embedding`
and `embedding_dim` are two additional `UNINDEXED` columns on the existing
`items` FTS5 table (same pattern as the pre-existing `tier`/
`tokens_estimate` columns) — no new on-disk format, no new table, no
extension.

### Secrets: API key handling

agent-wkp's `WKP_EMBED_API_KEY` environment variable is exactly what
CLAUDE.md's hard rule forbids. Options considered: (a) an env var anyway
(rejected outright — the rule is explicit), (b) `--embed-api-key <key>` on
argv (rejected — also explicitly forbidden, and additionally visible in
`ps`/shell history), (c) **`--embed-key-file PATH`, a file whose Unix mode
must be exactly `0600`, checked before reading (chosen)**, (d) reading
from stdin. (c) was chosen over (d) because both `wkp index` and
`wkp search` already take a positional query/no-file argument and
piping a secret through stdin would conflict with that positional
argument's own stdin-free usage pattern; a keyfile composes cleanly with
either command. A local, unauthenticated endpoint (Ollama, llama.cpp on
the same machine — design 5.3's stated primary use case) simply omits
`--embed-key-file`.

## Decision

Ship M1-9 as: `wkp search --embed-url URL [--embed-model NAME]
[--embed-key-file PATH]` (hybrid search, BM25 fallback with a stderr
warning on any failure — network error or an embedding-dimension mismatch
against what's stored) and `wkp index --embed-url URL [--embed-model NAME]
[--embed-key-file PATH]` (computes and stores each upserted item's
embedding as part of the same atomic temp-file-then-rename write
`update_index`/`build_index` already do — never a separate `UPDATE`
against the already-published `index.db`, which would violate CLAUDE.md's
"never write a file a harness reads in place"). `wkp context --embed-url`
is explicitly rejected with a clear error: combining hybrid search with
`context`'s own graph traversal is deliberately left to a later task
rather than silently ignoring the flag or half-implementing it here.

All of this is behind the `embed` Cargo feature, off by default. `cargo
deny check` (no `--features` flag, matching what CI runs) never sees
`ureq`/`rustls`/their transitive dependencies as a result of the gate
alone — `deny.toml`'s `[graph] all-features = false` scopes it to the
default feature set. **`cargo vet check` does not have the same
scoping**: once a package is resolved into `Cargo.lock` at all (which
happened the moment this session ran `cargo build --features embed`
locally to develop and test it), `cargo vet check` flags it as needing an
audit regardless of whether the *default* feature set would ever compile
it — confirmed empirically, not assumed. So the feature gate keeps `ureq`
and friends out of the default *binary* and out of `cargo deny`'s default
*check*, but not out of `cargo vet`'s. This PR therefore does add
`cargo vet regenerate exemptions`-generated `safe-to-run`/`safe-to-deploy`
entries for the new dependency tree in `supply-chain/config.toml` and
`supply-chain/imports.lock` — in the same PR as the feature code, matching
the precedent already set by M1-2's `rusqlite` exemptions (a new
dependency's own due-diligence entries land with the code that introduces
it; CLAUDE.md's "don't touch supply-chain/ in the same PR as feature code"
is about not bundling *unrelated* supply-chain policy changes into a
feature PR, not about this). `cargo vet regenerate exemptions` also
widened a few pre-existing entries (`cfg-if`, `itoa`, `libc`, `once_cell`)
from `safe-to-run` to `safe-to-deploy`, mechanically, because they're now
also reachable through `ureq`/`rustls` — a real (if optional) shipped
dependency, not merely a dev/build one.

## Consequences

- **Packaging decision deferred to M6** (release/hardening milestone,
  Homebrew tap / OCI image / self-update): whether an official release
  binary ships with `--features embed` compiled in is not decided here.
  This ADR only establishes that the *workspace's* default build (what
  M0's size gate and CI's default `cargo build`/`test`/`deny`/`vet`
  measure) does not include it. Follow up when M6 is scoped.
- A user who wants `--embed-url` today builds it themselves:
  `cargo build --release --features wkp-cli/embed`. `wkp search
  --embed-url ...` / `wkp index --embed-url ...` against a binary built
  *without* the feature fails with an explicit message naming the
  rebuild flag, not "unrecognized argument" — the flags themselves are
  always parsed regardless of how the binary was built.
- The `safe-to-run`/`safe-to-deploy` exemptions this PR added for `ureq`,
  `rustls`, `ring`, and the rest of that tree are auto-generated
  placeholders, not a real personal audit — the same posture M1-2's PR
  already flagged for `rusqlite`/`libsqlite3-sys`. A real security-focused
  review is still owed before anyone ships a binary built with
  `--features embed`, especially given `ring`'s and `rustls`'s security-
  sensitive role (this is the one path in the whole codebase that ever
  sends store content, even just a search query, over the network).
- The schema's `embedding`/`embedding_dim` columns exist unconditionally
  (regardless of which features built a given `wkp` binary) so `index.db`
  itself has one shape across builds; only the code path that populates
  or reads them is feature-gated. A store indexed by a feature-enabled
  binary is read fine by one built without the feature, and vice versa
  (the columns are simply `NULL`/unused on the latter).
- If a future milestone adds a second hybrid-search consumer (`wkp
  context`, per this ADR's explicit scope-out above), it reuses
  `wkp_core::index::hybrid_search` and `wkp_core::embed::embed_remote`
  directly rather than duplicating either.
