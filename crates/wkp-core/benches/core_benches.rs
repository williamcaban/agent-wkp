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

criterion_group!(
    benches,
    fixture_corpus_generate_5k,
    fixture_corpus_generate_50k
);
criterion_main!(benches);
