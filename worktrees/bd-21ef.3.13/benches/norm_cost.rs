//! Measures the per-query cost attributable to recomputing stored-vector norms.
//!
//! This deliberately does not benchmark a persisted-format change. Both inputs
//! are decoded from today's f16 blobs; the dot-product variant is normalized
//! once before Criterion enters the timed region.

use criterion::{black_box, criterion_group, criterion_main, Criterion, SamplingMode};
use kb::bench_fixture::{seed_db, BenchEmbedder, DEFAULT_SEED};
use kb::components::{db, embedder::Embedder};
use kb::models::{cosine_similarity, decode_emb_blob};
use rusqlite::Connection;
use std::time::Duration;

const CORPUS_SIZES: [usize; 2] = [1_000, 10_000];
const MMR_LIMIT: usize = 10;

fn normalize(vector: &[f32]) -> Vec<f32> {
    let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
    assert!(norm.is_finite() && norm > 0.0);
    vector.iter().map(|value| value / norm).collect()
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(left, right)| left * right).sum()
}

fn load_vectors(conn: &Connection, sql: &str) -> Vec<Vec<f32>> {
    let mut statement = conn.prepare(sql).unwrap();
    let rows = statement
        .query_map([], |row| row.get::<_, Vec<u8>>(0))
        .unwrap();
    rows
        .map(|blob| decode_emb_blob(&blob.unwrap()))
        .collect()
}

fn semantic_pass(query: &[f32], vectors: &[Vec<f32>], use_dot: bool) -> f32 {
    vectors
        .iter()
        .map(|stored| {
            if use_dot {
                dot(query, stored)
            } else {
                cosine_similarity(query, stored)
            }
        })
        .sum()
}

fn cue_pass(query: &[f32], vectors: &[Vec<f32>], use_dot: bool) -> f32 {
    // The fixture has one cue per entry. Retaining the best-score update keeps
    // this shaped like the production best-cue-per-entry loop while ensuring
    // the only difference between variants is the similarity operation.
    let mut best = vec![f32::NEG_INFINITY; vectors.len()];
    for (entry_index, stored) in vectors.iter().enumerate() {
        let score = if use_dot {
            dot(query, stored)
        } else {
            cosine_similarity(query, stored)
        };
        best[entry_index] = best[entry_index].max(score);
    }
    best.into_iter().sum()
}

fn mmr_pass(vectors: &[Vec<f32>], use_dot: bool) -> f32 {
    // Production MMR starts with limit*2 candidates and, for each remaining
    // candidate, scans every already-selected vector for its maximum cosine.
    let mut remaining: Vec<usize> = (1..vectors.len()).collect();
    let mut selected = vec![0usize];
    let mut checksum = 0.0;
    while !remaining.is_empty() {
        let mut best_position = 0;
        let mut best_penalty = f32::INFINITY;
        for (position, &candidate) in remaining.iter().enumerate() {
            let max_similarity = selected
                .iter()
                .map(|&chosen| {
                    if use_dot {
                        dot(&vectors[candidate], &vectors[chosen])
                    } else {
                        cosine_similarity(&vectors[candidate], &vectors[chosen])
                    }
                })
                .fold(0.0f32, f32::max);
            if max_similarity < best_penalty {
                best_penalty = max_similarity;
                best_position = position;
            }
        }
        checksum += best_penalty;
        selected.push(remaining.remove(best_position));
    }
    checksum
}

fn norm_cost(c: &mut Criterion) {
    for corpus_size in CORPUS_SIZES {
        let embedder = BenchEmbedder::new(DEFAULT_SEED);
        let connection = db::open_db_memory().unwrap();
        seed_db(&connection, &embedder, corpus_size, DEFAULT_SEED).unwrap();

        let query = embedder.embed("architecture latency").unwrap();
        let semantic = load_vectors(
            &connection,
            "SELECT emb.embedding FROM entries_emb emb ORDER BY emb.rowid",
        );
        let cues = load_vectors(
            &connection,
            "SELECT embedding FROM cues WHERE embedding IS NOT NULL ORDER BY entry_id, id",
        );
        assert_eq!(semantic.len(), corpus_size);
        assert_eq!(cues.len(), corpus_size);

        // Simulate the proposed representation outside every timed region.
        let normalized_query = normalize(&query);
        let normalized_semantic: Vec<Vec<f32>> =
            semantic.iter().map(|vector| normalize(vector)).collect();
        let normalized_cues: Vec<Vec<f32>> =
            cues.iter().map(|vector| normalize(vector)).collect();
        let mmr_pool_size = MMR_LIMIT * 2;
        let mmr = &semantic[..mmr_pool_size];
        let normalized_mmr = &normalized_semantic[..mmr_pool_size];

        let mut group = c.benchmark_group(format!("norm_cost/{corpus_size}"));
        group.sampling_mode(SamplingMode::Flat);
        group.sample_size(15);
        group.measurement_time(Duration::from_secs(3));

        group.bench_function("semantic/cosine_recompute_norm_b", |b| {
            b.iter(|| black_box(semantic_pass(black_box(&query), black_box(&semantic), false)))
        });
        group.bench_function("semantic/dot_pre_normalized", |b| {
            b.iter(|| {
                black_box(semantic_pass(
                    black_box(&normalized_query),
                    black_box(&normalized_semantic),
                    true,
                ))
            })
        });

        group.bench_function("cue/cosine_recompute_norm_b", |b| {
            b.iter(|| black_box(cue_pass(black_box(&query), black_box(&cues), false)))
        });
        group.bench_function("cue/dot_pre_normalized", |b| {
            b.iter(|| {
                black_box(cue_pass(
                    black_box(&normalized_query),
                    black_box(&normalized_cues),
                    true,
                ))
            })
        });

        group.bench_function("mmr/cosine_recompute_norm_b", |b| {
            b.iter(|| black_box(mmr_pass(black_box(mmr), false)))
        });
        group.bench_function("mmr/dot_pre_normalized", |b| {
            b.iter(|| black_box(mmr_pass(black_box(normalized_mmr), true)))
        });
        group.finish();

        for site in ["semantic", "cue", "mmr"] {
            eprintln!(
                "norm_cost summary: site={site} corpus={corpus_size} norm_b_cost_pct=((cosine_ns-dot_ns)/cosine_ns)*100; copy Criterion estimates into docs/benchmarks/p2-prenorm-measurement.md"
            );
        }
    }
}

criterion_group!(benches, norm_cost);
criterion_main!(benches);
