use criterion::{criterion_group, criterion_main, Criterion, SamplingMode};
use kb::bench_fixture::{seed_db, BenchEmbedder, DEFAULT_SEED};
use kb::commands::{cited_by, context};
use kb::components::db;
use std::fs;
use std::process::Command;
use std::time::Duration;
use tempfile::TempDir;

fn git(args: &[&str], root: &std::path::Path) {
    assert!(Command::new("git")
        .arg("-C")
        .arg(root)
        .args(args)
        .status()
        .unwrap()
        .success());
}

fn git_fixture() -> TempDir {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("src")).unwrap();
    fs::write(dir.path().join("src/hot.rs"), "alpha\nbeta\ngamma\n").unwrap();
    fs::write(dir.path().join("src/support.rs"), "alpha\nbeta\ngamma\n").unwrap();
    git(
        &["init", "-q", "-b", "bench/architecture-latency"],
        dir.path(),
    );
    git(
        &["config", "user.email", "bench@example.invalid"],
        dir.path(),
    );
    git(&["config", "user.name", "kb benchmark"], dir.path());
    git(&["add", "."], dir.path());
    git(&["commit", "-qm", "fixture"], dir.path());
    fs::write(dir.path().join("src/hot.rs"), "alpha\nbeta\ngamma\ndirty\n").unwrap();
    fs::write(dir.path().join("staged.txt"), "staged\n").unwrap();
    git(&["add", "staged.txt"], dir.path());
    dir
}

fn pure_selection(c: &mut Criterion) {
    let candidates: Vec<_> = (0..10_000)
        .map(|i| {
            (
                format!("candidate-{i:05}"),
                10 + i % 80,
                (i % 101) as f32 / 101.0,
                i % 3 != 0,
            )
        })
        .collect();
    c.bench_function("context/pure_selection/10000", |b| {
        b.iter(|| context::benchmark_greedy_select(&candidates, 1_000))
    });
}

fn db_surfaces(c: &mut Criterion) {
    let repo = git_fixture();
    let emb = BenchEmbedder::new(DEFAULT_SEED);
    let conn = db::open_db_memory().unwrap();
    seed_db(&conn, &emb, 10_000, DEFAULT_SEED).unwrap();
    let mut group = c.benchmark_group("interactive_db/10000");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(15));
    group.bench_function("context_scoring_fixed_git", |b| {
        b.iter(|| context::benchmark_context_path(&conn, repo.path(), 1_000).unwrap())
    });
    group.bench_function("cited_by_file_only", |b| {
        b.iter(|| cited_by::benchmark_cited_by(&conn, "src/hot.rs", repo.path()).unwrap())
    });
    group.finish();
}

fn cue_lane(c: &mut Criterion) {
    let emb = BenchEmbedder::new(DEFAULT_SEED);
    let opts = db::SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: true,
        inline_verify_k: 0,
        ..db::SearchOptions::default()
    };
    let mut group = c.benchmark_group("cue_lane/10000");
    group.sample_size(30);
    group.measurement_time(Duration::from_secs(15));
    let heavy = db::open_db_memory().unwrap();
    seed_db(&heavy, &emb, 10_000, DEFAULT_SEED).unwrap();
    group.bench_function("cue_heavy", |b| {
        b.iter(|| db::search_entries(&heavy, &emb, "architecture latency", &opts).unwrap())
    });
    let free = db::open_db_memory().unwrap();
    seed_db(&free, &emb, 10_000, DEFAULT_SEED).unwrap();
    free.execute("DELETE FROM cues", []).unwrap();
    group.bench_function("cue_free", |b| {
        b.iter(|| db::search_entries(&free, &emb, "architecture latency", &opts).unwrap())
    });
    group.finish();
}

fn large(c: &mut Criterion) {
    let repo = git_fixture();
    let emb = BenchEmbedder::new(DEFAULT_SEED);
    let conn = db::open_db_memory().unwrap();
    seed_db(&conn, &emb, 100_000, DEFAULT_SEED).unwrap();
    let opts = db::SearchOptions {
        limit: 10,
        do_fts: true,
        do_semantic: true,
        inline_verify_k: 0,
        ..db::SearchOptions::default()
    };
    let mut group = c.benchmark_group("interactive_db/100000");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(10);
    group.measurement_time(Duration::from_secs(30));
    group.bench_function("context_scoring_fixed_git", |b| {
        b.iter(|| context::benchmark_context_path(&conn, repo.path(), 1_000).unwrap())
    });
    group.bench_function("cited_by_file_only", |b| {
        b.iter(|| cited_by::benchmark_cited_by(&conn, "src/hot.rs", repo.path()).unwrap())
    });
    group.finish();
    let mut cues = c.benchmark_group("cue_lane/100000");
    cues.sampling_mode(SamplingMode::Flat);
    cues.sample_size(10);
    cues.measurement_time(Duration::from_secs(30));
    cues.bench_function("cue_heavy", |b| {
        b.iter(|| db::search_entries(&conn, &emb, "architecture latency", &opts).unwrap())
    });
    conn.execute("DELETE FROM cues", []).unwrap();
    cues.bench_function("cue_free", |b| {
        b.iter(|| db::search_entries(&conn, &emb, "architecture latency", &opts).unwrap())
    });
    cues.finish();
}

criterion_group!(interactive, pure_selection, db_surfaces, cue_lane);
criterion_group!(interactive_large, large);
criterion_main!(interactive, interactive_large);
