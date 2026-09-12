//! M0 task 5: benchmark harness scaffold.
//!
//! The operations design 4.3 sets latency targets for -- cold `wkp search`,
//! incremental index, `wkp materialize` -- don't exist yet; they land
//! starting M1 (see docs/plan/milestones.md). Until then, this harness
//! benchmarks the one real operation available today: generating the
//! fixture corpus those M1 benches will consume. That proves the harness,
//! `benches/baseline.json`, and the CI wiring all work before there's a
//! "real" number to gate on, instead of faking placeholder numbers for
//! functionality that isn't built.
//!
//! Replace/extend the groups below with cold-search / incremental-index /
//! materialize benches as each lands in M1, using the same 5k/50k corpora
//! from `support::generate_corpus`. See `benches/README.md` for the
//! regression-threshold methodology.

mod support;

use criterion::{criterion_group, criterion_main, Criterion};
use std::hint::black_box;
use std::path::{Path, PathBuf};

/// Creates a fresh, unpredictably-named directory and returns its path.
/// Named directories in a shared `/tmp` are a classic symlink-attack vector
/// (CWE-377: predictable name + another user or process pre-creates it
/// first); mixing in the time alongside the pid, and requiring exclusive
/// creation (`create_dir`, not `create_dir_all`) rather than silently
/// reusing whatever is already at that path, closes that off without
/// adding a `rand`/`tempfile` dependency for a benchmark helper.
///
/// Prefers `/dev/shm` (tmpfs) over `std::env::temp_dir()` when writable:
/// this benchmark's own dominant cost is thousands of small file writes,
/// which turned out to be the actual source of the CI flakiness this
/// function was written to fix (see benches/README.md's second baseline
/// incident) -- disk-backed `/tmp` on a shared, multi-tenant GitHub-hosted
/// runner has highly variable I/O latency (observed: the *same* commit
/// measured between 93ms and 244ms for the 5k corpus across different runs,
/// a >160% swing with no code change). tmpfs removes that variable by
/// keeping the benchmark's dominant cost in-memory, which is far more
/// consistent on a shared vCPU than the disk I/O layer is. Falls back to
/// `std::env::temp_dir()` when `/dev/shm` doesn't exist (e.g. local macOS
/// development) or isn't writable.
fn bench_dir(label: &str) -> PathBuf {
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock is before the Unix epoch")
        .as_nanos();
    let shm = Path::new("/dev/shm");
    let base = if shm.is_dir() && exclusive_write_probe(shm) {
        shm.to_path_buf()
    } else {
        std::env::temp_dir() // nosemgrep: rust.lang.security.temp-dir.temp-dir -- name below is unpredictable and create_dir() is exclusive, closing the symlink-race the rule flags
    };
    let dir = base.join(format!(
        "wkp-bench-corpus-{label}-{}-{nonce:x}",
        std::process::id()
    ));
    std::fs::create_dir(&dir)
        .unwrap_or_else(|e| panic!("failed to exclusively create bench dir {dir:?}: {e}"));
    dir
}

/// Whether `dir` accepts a new file write, by actually trying one and
/// cleaning it up -- simpler and more honest than inspecting permission
/// bits, which don't account for mount options (e.g. a read-only bind
/// mount) or MAC policies that a bit check alone would miss.
fn exclusive_write_probe(dir: &Path) -> bool {
    let probe = dir.join(format!(".wkp-bench-write-probe-{}", std::process::id()));
    let ok = std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

fn fixture_corpus_generate_5k(c: &mut Criterion) {
    let dir = bench_dir("5k");
    c.bench_function("fixture_corpus_generate_5k", |b| {
        b.iter(|| {
            let _ = std::fs::remove_dir_all(&dir);
            support::generate_corpus(black_box(&dir), black_box(5_000), black_box(42)).unwrap();
        })
    });
    let _ = std::fs::remove_dir_all(&dir);
}

fn fixture_corpus_generate_50k(c: &mut Criterion) {
    let dir = bench_dir("50k");
    // 50k files/iteration is expensive; fewer samples keeps this bounded.
    let mut group = c.benchmark_group("fixture_corpus_generate_50k");
    group.sample_size(10);
    group.bench_function("run", |b| {
        b.iter(|| {
            let _ = std::fs::remove_dir_all(&dir);
            support::generate_corpus(black_box(&dir), black_box(50_000), black_box(42)).unwrap();
        })
    });
    group.finish();
    let _ = std::fs::remove_dir_all(&dir);
}

/// M1-3: "incremental index check scales with changed-file count, not
/// corpus size." Builds a 50k-item index once (outside the timed loop),
/// then times [`wkp_core::index::update_index`] re-applying a small,
/// fixed set of changed items against it -- the scenario `wkp index`
/// hits on every run after the first: most of the corpus is untouched,
/// a handful of files changed.
///
/// Honest caveat, documented rather than hidden: `update_index` still
/// copies the whole `index.db` file via `VACUUM INTO` to honor the
/// never-write-in-place rule (CLAUDE.md), so the on-disk I/O is
/// proportional to corpus size, not change count. What *is* proportional
/// to change count -- and was the actual cost the old Python tool paid
/// per file on every run (design 5.1: "detects change by running
/// `git hash-object` on every file") -- is the parsing/tokenizing work
/// this benchmark's setup does once per changed item, not once per corpus
/// item. See `crates/wkp-core/src/index.rs`'s `update_index` doc comment.
///
/// This benchmark measured 383-462ms mean on the 50k fixture across two
/// separate ubuntu-latest CI runs (tmpfs, 20 samples) for a 10-item change
/// -- design 4.3's incremental-index target (p50 < 30ms / p95 < 100ms) is
/// missed by roughly an order of magnitude either way, because `VACUUM
/// INTO`'s copy dominates. This is a real design-vs-reality conflict, not a
/// bug: see
/// `docs/adr/0002-incremental-index-write-mechanism.md` for the options and
/// the (not yet made) decision. `benches/baseline.json`'s entry for this
/// bench exists to catch a *further* regression on top of this
/// already-known-slow path, not to imply the target is currently met.
fn incremental_update_50k_corpus(c: &mut Criterion) {
    let dir = bench_dir("incremental-50k");
    let dest = dir.join("index.db");

    let corpus_dir = bench_dir("incremental-50k-source");
    support::generate_corpus(&corpus_dir, 50_000, 7).expect("generate 50k fixture corpus");
    let initial: Vec<wkp_core::index::Item> =
        (0..50_000).map(|i| corpus_item(&corpus_dir, i)).collect();
    wkp_core::index::build_index(&dest, &initial).expect("build initial 50k index");

    // A realistic incremental run: a handful of items changed, the other
    // 49,990 untouched.
    let changed: Vec<wkp_core::index::Item> = (0..10)
        .map(|i| {
            let mut item = corpus_item(&corpus_dir, i);
            item.body.push_str("\nedited for the incremental bench\n");
            item
        })
        .collect();

    let mut group = c.benchmark_group("incremental_update_50k_corpus");
    group.sample_size(20);
    group.bench_function("10_changed", |b| {
        b.iter(|| {
            wkp_core::index::update_index(black_box(&dest), black_box(&changed), &[]).unwrap();
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&corpus_dir);
}

fn corpus_item(dir: &Path, i: usize) -> wkp_core::index::Item {
    let path = format!("item-{i:06}.md");
    let contents = std::fs::read_to_string(dir.join(&path)).expect("read fixture item");
    let parsed = wkp_core::frontmatter::parse(&contents);
    wkp_core::index::Item {
        path,
        frontmatter: parsed.frontmatter,
        body: parsed.body,
        embedding: None,
        human_signed: true,
    }
}

/// M1-4: "cold `wkp search` process" (design 4.3, p50 < 10ms / p95 < 25ms).
/// `wkp` never keeps `index.db` open across invocations (design 4.2: no
/// daemon), so each real `wkp search` call opens a fresh connection. This
/// benchmark reproduces that per-iteration open cost against a 50k-item
/// index, missing only the OS-level process-spawn and dynamic-linking
/// overhead a real subprocess pays -- that part isn't measurable from
/// inside a Criterion harness in the same process, and was measured
/// separately with the actual release binary (reported in the PR, not
/// gated in CI, since it needs the built binary rather than `cargo bench`
/// alone): ~2-3ms wall-clock for `wkp search` end to end on this fixture,
/// comfortably within budget on top of whatever this benchmark reports.
fn cold_search_50k_corpus(c: &mut Criterion) {
    let dir = bench_dir("cold-search-50k");
    let dest = dir.join("index.db");
    let items: Vec<wkp_core::index::Item> = (0..50_000)
        .map(|i| synthetic_item(i, i == 12_345))
        .collect();
    wkp_core::index::build_index(&dest, &items).expect("build 50k index");

    let mut group = c.benchmark_group("cold_search_50k_corpus");
    group.sample_size(50);
    group.bench_function("open_and_query", |b| {
        b.iter(|| {
            let conn = wkp_core::index::open_index(black_box(&dest)).unwrap();
            let hits = wkp_core::index::search(
                &conn,
                black_box("distinctive"),
                &wkp_core::index::SearchFilter::default(),
            )
            .unwrap();
            black_box(hits);
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&dir);
}

/// A synthetic item with generic filler words, except item `12345` (or
/// whichever index `distinctive_content` is set for), which gets one rare
/// term so a query for it matches exactly one document out of 50,000 --
/// closer to a realistic targeted search than a query matching the whole
/// corpus equally.
fn synthetic_item(i: usize, distinctive_content: bool) -> wkp_core::index::Item {
    let mut rng = support::Xorshift64::new(i as u64 + 1);
    let body = if distinctive_content {
        "this item has a distinctive term nobody else shares".to_string()
    } else {
        support::synthetic_body(&mut rng, 30)
    };
    wkp_core::index::Item {
        path: format!("item-{i:06}.md"),
        frontmatter: wkp_core::frontmatter::Frontmatter::default(),
        body,
        embedding: None,
        human_signed: true,
    }
}

/// M1-6 (`wkp materialize --tier 0|1`, M0-5's originally-named
/// `materialize_tier0` target). Builds a 50k-item index where 1 in 100
/// items (500 total) are `type: project-state` and human-signed -- tier 0
/// unconditionally per [`compute_tier`](crate::index::tier) -- and the
/// rest are untyped, landing at tier 2. That ratio approximates a real
/// store's shape: most content is reference/knowledge material a harness
/// pulls in on demand, only a small, deliberately-curated slice is tier 0
/// content injected into every session. Materializing the full 50k corpus
/// as if it were all tier 0 would measure a scenario this design never
/// intends to hit in practice.
fn materialize_tier0_50k_corpus(c: &mut Criterion) {
    let dir = bench_dir("materialize-tier0-50k");
    let dest = dir.join("index.db");
    let items: Vec<wkp_core::index::Item> = (0..50_000)
        .map(|i| materialize_bench_item(i, i % 100 == 0))
        .collect();
    wkp_core::index::build_index(&dest, &items).expect("build 50k index");

    let mut group = c.benchmark_group("materialize_tier0_50k_corpus");
    group.sample_size(50);
    group.bench_function("materialize", |b| {
        b.iter(|| {
            let conn = wkp_core::index::open_index(black_box(&dest)).unwrap();
            let out = wkp_core::index::materialize(&conn, black_box(0)).unwrap();
            black_box(out);
        })
    });
    group.finish();

    let _ = std::fs::remove_dir_all(&dir);
}

fn materialize_bench_item(i: usize, tier0: bool) -> wkp_core::index::Item {
    let mut rng = support::Xorshift64::new(i as u64 + 1);
    let mut frontmatter = wkp_core::frontmatter::Frontmatter::default();
    if tier0 {
        frontmatter.item_type = Some(wkp_core::frontmatter::ItemType::ProjectState);
        frontmatter.title = Some(format!("project state {i}"));
    }
    wkp_core::index::Item {
        path: format!("item-{i:06}.md"),
        frontmatter,
        body: support::synthetic_body(&mut rng, 30),
        embedding: None,
        human_signed: true,
    }
}

criterion_group!(
    benches,
    fixture_corpus_generate_5k,
    fixture_corpus_generate_50k,
    incremental_update_50k_corpus,
    cold_search_50k_corpus,
    materialize_tier0_50k_corpus
);
criterion_main!(benches);
