//! Shared fixture-generation support for wkp-core benchmarks. Not part of
//! the public library API. Deliberately dependency-free (a tiny inline
//! xorshift PRNG, no `rand` crate, no `tempfile` crate) since fixture
//! generation for a benchmark harness doesn't need either, and every new
//! dependency needs justification per CLAUDE.md.

use std::fs;
use std::io;
use std::path::Path;

struct Xorshift64(u64);

impl Xorshift64 {
    fn new(seed: u64) -> Self {
        Xorshift64(seed | 1)
    }

    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
}

const WORDS: &[&str] = &[
    "index",
    "search",
    "tier",
    "provenance",
    "frontmatter",
    "commit",
    "signature",
    "workspace",
    "harness",
    "session",
    "budget",
    "context",
    "materialize",
    "store",
    "corpus",
    "latency",
    "benchmark",
    "regression",
    "threshold",
    "criterion",
];

fn synthetic_body(rng: &mut Xorshift64, word_count: usize) -> String {
    (0..word_count)
        .map(|_| WORDS[(rng.next_u64() as usize) % WORDS.len()])
        .collect::<Vec<_>>()
        .join(" ")
}

/// Writes `count` deterministic synthetic markdown+frontmatter files into
/// `dir` (created if missing), for use as a fixture corpus in benches.
/// Deterministic for a given `(count, seed)` so reruns produce byte-identical
/// output and benchmark numbers aren't skewed by fixture-content variance.
pub fn generate_corpus(dir: &Path, count: usize, seed: u64) -> io::Result<()> {
    fs::create_dir_all(dir)?;
    let mut rng = Xorshift64::new(seed);
    for i in 0..count {
        let tokens = 50 + (rng.next_u64() % 200);
        let body = synthetic_body(&mut rng, (tokens / 5) as usize);
        let contents = format!(
            "---\ntype: note\ntokens: {tokens}\nscope: bench-fixture\n---\n\n\
             # Fixture item {i}\n\n{body}\n"
        );
        fs::write(dir.join(format!("item-{i:06}.md")), contents)?;
    }
    Ok(())
}
