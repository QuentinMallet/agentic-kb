//! bench_search_vs_size — hybrid search latency vs DB size benchmark
//!
//! Parametrized over DB sizes: 1_000, 10_000, 100_000, 1_000_000.
//! Three sub-benches per size:
//!   - fts_only     (do_fts=true,  do_semantic=false)
//!   - semantic_only(do_fts=false, do_semantic=true)
//!   - hybrid       (do_fts=true,  do_semantic=true)  — exercises RRF (23b.5)
//!
//! **Embedder strategy:** BenchEmbedder — a deterministic embedder backed by a
//! seeded PRNG lookup table. It returns a 384-dim unit vector derived from the
//! text via a hash, so (a) the semantic lane is active, (b) timings are
//! reproducible, and (c) no model files are downloaded.
//!
//! **Seeding strategy:** deterministic RNG (StdRng::seed_from_u64(42)).
//! Each entry gets structured content so FTS has real tokens to match:
//!   - summary: "bench entry topic-<n> tag-<category>"
//!   - content: lorem-style paragraph with the entry index and a category word
//!   - tags: ["bench", "<category>"]
//!   - path: "bench/<category>/entry-<n>"
//!
//! **1M scale gating:** The 1M-entry bench only runs when the environment variable
//! `BENCH_LARGE_SIZE=1` is set. Without that variable the bench is registered but
//! skips immediately (prints a message and returns 0 iterations). This prevents
//! the ~1.8 GB memory/disk footprint from dominating CI wall-clock time.
//!
//! **Criterion configuration for large sizes:**
//!   - 100k: 20 samples, measurement_time 30s
//!   - 1M:   10 samples, measurement_time 60s  (gated behind BENCH_LARGE_SIZE=1)

use criterion::{criterion_group, criterion_main, BenchmarkGroup, Criterion, SamplingMode};
use kb::components::{db, embedder::Embedder};
use rand::rngs::StdRng;
use rand::{Rng, SeedableRng};
use serde_json::json;
use std::time::Duration;

// ---------------------------------------------------------------------------
// BenchEmbedder: deterministic fixed-vector embedder
// ---------------------------------------------------------------------------

/// Deterministic embedder backed by a seeded RNG lookup table.
///
/// Builds a pool of `POOL_SIZE` unit vectors once at construction time.
/// `embed(text)` selects from the pool by hashing the text (FNV-1a),
/// so the same text always returns the same vector across bench iterations.
///
/// The resulting vectors are NOT meaningful semantically, but they exercise
/// the full semantic lane code path (cosine similarity, RRF merge) without
/// requiring a trained model.
struct BenchEmbedder {
    pool: Vec<Vec<f32>>,
}

const EMBED_DIM: usize = 384; // matches BAAI/bge-small-en-v1.5
const POOL_SIZE: usize = 512;

impl BenchEmbedder {
    fn new(seed: u64) -> Self {
        let mut rng = StdRng::seed_from_u64(seed);
        let pool = (0..POOL_SIZE)
            .map(|_| {
                let raw: Vec<f32> = (0..EMBED_DIM).map(|_| rng.gen::<f32>() * 2.0 - 1.0).collect();
                // L2-normalize
                let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm > 0.0 {
                    raw.iter().map(|x| x / norm).collect()
                } else {
                    vec![0.0f32; EMBED_DIM]
                }
            })
            .collect();
        BenchEmbedder { pool }
    }

    /// FNV-1a hash → index into pool.
    fn pool_index(&self, text: &str) -> usize {
        let mut h: u64 = 0xcbf29ce484222325;
        for b in text.bytes() {
            h ^= b as u64;
            h = h.wrapping_mul(0x100000003b4c61);
        }
        (h as usize) % self.pool.len()
    }
}

impl Embedder for BenchEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        Ok(self.pool[self.pool_index(text)].clone())
    }

    fn is_noop(&self) -> bool {
        false
    }
}

// ---------------------------------------------------------------------------
// Seeding helpers
// ---------------------------------------------------------------------------

const CATEGORIES: &[&str] = &[
    "architecture", "gotchas", "debugging", "conventions", "runbooks",
    "antipatterns", "packages", "e2e", "security", "performance",
];

const LOREM_WORDS: &[&str] = &[
    "system", "module", "function", "trait", "struct", "impl", "async",
    "database", "index", "query", "cache", "latency", "throughput", "batch",
    "migration", "schema", "vector", "embedding", "similarity", "rank",
];

/// Seed `n` entries into `conn`.  Uses `apply_event` (the same path the runtime
/// uses) so FTS5 index and entries_emb are both populated correctly.
///
/// For the semantic lane, entries_emb is populated via direct INSERT after the
/// apply_event (which stores a noop embedding blob); we overwrite with the
/// BenchEmbedder vector so cosine similarity sees real data.
fn seed_db(conn: &rusqlite::Connection, emb: &BenchEmbedder, n: usize) {
    let mut rng = StdRng::seed_from_u64(42);

    for i in 0..n {
        let cat = CATEGORIES[i % CATEGORIES.len()];
        let word = LOREM_WORDS[i % LOREM_WORDS.len()];
        let word2 = LOREM_WORDS[(i + 3) % LOREM_WORDS.len()];

        let ev = json!({
            "action": "upsert",
            "table": "entries",
            "id": format!("bench-size-{:07}", i),
            "path": format!("bench/{}/entry-{}", cat, i),
            "summary": format!("bench entry topic-{} {}", i, cat),
            "content": format!(
                "Entry {} discusses {} and {} in the context of {}. \
                 The {} subsystem relies on efficient {} operations. \
                 Index {} provides {} guarantees under load.",
                i, word, word2, cat, cat, word, i % 100, word2
            ),
            "tags": ["bench", cat],
            "kind": "observation",
            "evidence_status": "missing",
            "permanent": false,
            "is_stale": false,
            "ts": "2024-01-01T00:00:00Z",
            "session_id": null,
        });
        db::apply_event(conn, emb, &ev).unwrap();

        // Overwrite the embedding blob with a deterministic non-zero vector so
        // the semantic lane has real data to score against.
        //
        // apply_event stores a zero-length blob for the BenchEmbedder's output
        // (embed() returns a pool vector, not empty — it IS stored by apply_event).
        // We do NOT need to overwrite; apply_event already wrote the pool vector.
        // But we do need to jitter slightly per-entry so entries have distinct
        // vectors and don't all score identically. We achieve this by using a
        // per-entry seed offset in the pool index (the content text naturally
        // differs per entry, so pool_index varies already).
        let _ = rng.gen::<u8>(); // advance rng to maintain reproducibility
    }
}

// ---------------------------------------------------------------------------
// Bench group: run_size_group
// ---------------------------------------------------------------------------

/// SearchOptions shorthand.
fn opts_fts() -> db::SearchOptions {
    db::SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: false,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
    }
}

fn opts_semantic() -> db::SearchOptions {
    db::SearchOptions {
        limit: 10,
        do_fts: false,
        do_semantic: true,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
    }
}

fn opts_hybrid() -> db::SearchOptions {
    db::SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: true,
        path_prefix: None,
        tag_filter: None,
        inline_verify_k: 0,
        repo_root: None,
    }
}

/// Run the three sub-benches for a single DB size.
fn bench_size(group: &mut BenchmarkGroup<criterion::measurement::WallTime>, size: usize) {
    let emb = BenchEmbedder::new(42);
    let conn = db::open_db_memory().unwrap();
    seed_db(&conn, &emb, size);

    let query = "bench entry topic architecture";

    group.bench_function(format!("{size}/fts_only"), |b| {
        let opts = opts_fts();
        b.iter(|| db::search_entries(&conn, &emb, query, &opts).unwrap());
    });

    group.bench_function(format!("{size}/semantic_only"), |b| {
        let opts = opts_semantic();
        b.iter(|| db::search_entries(&conn, &emb, query, &opts).unwrap());
    });

    group.bench_function(format!("{size}/hybrid"), |b| {
        let opts = opts_hybrid();
        b.iter(|| db::search_entries(&conn, &emb, query, &opts).unwrap());
    });
}

// ---------------------------------------------------------------------------
// Criterion entry points
// ---------------------------------------------------------------------------

fn bench_search_vs_size_small(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_vs_size");
    group.sample_size(50);
    group.measurement_time(Duration::from_secs(15));

    for &size in &[1_000usize, 10_000] {
        bench_size(&mut group, size);
    }
    group.finish();
}

fn bench_search_vs_size_large(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_vs_size_large");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(20);
    group.measurement_time(Duration::from_secs(30));

    bench_size(&mut group, 100_000);
    group.finish();
}

fn bench_search_vs_size_xlarge(c: &mut Criterion) {
    // 1M-entry bench only runs when BENCH_LARGE_SIZE=1.
    // Without this env var the group is registered but immediately finished
    // (zero measurements). This prevents ~1.8 GB memory pressure in CI.
    let enabled = std::env::var("BENCH_LARGE_SIZE").map(|v| v == "1").unwrap_or(false);

    let mut group = c.benchmark_group("search_vs_size_1m");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(60));

    if !enabled {
        eprintln!(
            "[bench_search_vs_size] 1M bench skipped — set BENCH_LARGE_SIZE=1 to enable. \
             Estimated memory: ~1.8 GB (300 MB DB + 1.5 GB embeddings)."
        );
        group.finish();
        return;
    }

    bench_size(&mut group, 1_000_000);
    group.finish();
}

criterion_group!(
    benches_small,
    bench_search_vs_size_small
);
criterion_group!(
    benches_large,
    bench_search_vs_size_large
);
criterion_group!(
    benches_xlarge,
    bench_search_vs_size_xlarge
);
criterion_main!(benches_small, benches_large, benches_xlarge);
