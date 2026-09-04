use criterion::{criterion_group, criterion_main, BatchSize, Criterion, SamplingMode};
use kb::bench_fixture::{seed_db, BenchEmbedder, DEFAULT_SEED};
use kb::components::{db, embedder::NoopEmbedder, events, kb_core};
use kb::config;
use serde_json::{json, Value};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use tempfile::TempDir;

const FIXTURE_SIZE: usize = 10_000;

struct AddFixture {
    _dir: TempDir,
    paths: config::Paths,
    args: kb_core::AddArgs,
}

struct EventFixture {
    _dir: TempDir,
    paths: config::Paths,
    batch: Vec<Value>,
}

struct ApplyFixture {
    _dir: TempDir,
    conn: rusqlite::Connection,
    batch: Vec<Value>,
}

/// Recursively copy `src` into `dst` (both directories; `dst` must not yet exist).
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    fs::create_dir_all(dst)?;
    for entry in fs::read_dir(src)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        let dst_path = dst.join(entry.file_name());
        if ty.is_dir() {
            copy_dir_recursive(&entry.path(), &dst_path)?;
        } else {
            fs::copy(entry.path(), &dst_path)?;
        }
    }
    Ok(())
}

/// Build the `FIXTURE_SIZE`-entry seeded `.state/agent-kb` directory once per bench
/// process and reuse it as a template — reseeding via `seed_db` (one `apply_event` per
/// row) on every `iter_batched` setup call made this benchmark take hours; a plain file
/// copy of the pre-built template is orders of magnitude cheaper per iteration.
fn template_state_dir() -> &'static Path {
    static TEMPLATE: OnceLock<TempDir> = OnceLock::new();
    TEMPLATE
        .get_or_init(|| {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            fs::create_dir_all(root.join(".state/agent-kb")).unwrap();
            let paths = config::Paths::from_root(root);
            let conn = db::open_db(&paths.db).unwrap();
            let emb = BenchEmbedder::new(DEFAULT_SEED);
            seed_db(&conn, &emb, FIXTURE_SIZE, DEFAULT_SEED).unwrap();
            conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
                .unwrap();
            drop(conn);
            dir
        })
        .path()
}

fn seeded_repo() -> (TempDir, config::Paths) {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    copy_dir_recursive(
        &template_state_dir().join(".state/agent-kb"),
        &root.join(".state/agent-kb"),
    )
    .unwrap();
    let paths = config::Paths::from_root(root);
    (dir, paths)
}

fn add_args() -> kb_core::AddArgs {
    kb_core::AddArgs {
        id: "bench-write-lane".to_string(),
        path: "bench/write/added-entry".to_string(),
        summary: "bench write lane".to_string(),
        content: "write path benchmark entry for log append and db apply".to_string(),
        tags: json!(["bench", "write"]),
        version_ref: Some("bench-write".to_string()),
        permanent: false,
        replace_path: false,
        kind: "belief".to_string(),
        evidence_status: "missing".to_string(),
        evidence_rows: vec![],
        ts: "2026-09-04T00:00:00Z".to_string(),
        session: "bench".to_string(),
        session_id: None,
        expire_reason: "replaced by benchmark".to_string(),
        dedup_cutoff: None,
        cues: vec![],
    }
}

fn add_batch() -> Vec<Value> {
    vec![json!({
        "action": "upsert",
        "table": "entries",
        "id": "bench-write-lane",
        "path": "bench/write/added-entry",
        "summary": "bench write lane",
        "content": "write path benchmark entry for log append and db apply",
        "tags": ["bench", "write"],
        "version_ref": "bench-write",
        "permanent": false,
        "kind": "belief",
        "evidence_status": "missing",
        "session_id": null,
        "ts": "2026-09-04T00:00:00Z",
        "session": "bench"
    })]
}

fn setup_add_fixture() -> AddFixture {
    let (_dir, paths) = seeded_repo();
    AddFixture {
        _dir,
        paths,
        args: add_args(),
    }
}

fn setup_event_fixture() -> EventFixture {
    let (_dir, paths) = seeded_repo();
    EventFixture {
        _dir,
        paths,
        batch: add_batch(),
    }
}

fn setup_apply_fixture() -> ApplyFixture {
    let (dir, paths) = seeded_repo();
    let conn = db::open_db(&paths.db).unwrap();
    ApplyFixture {
        _dir: dir,
        conn,
        batch: add_batch(),
    }
}

fn write_path(c: &mut Criterion) {
    let noop = NoopEmbedder;
    let mut group = c.benchmark_group("write_path/10000");
    group.sampling_mode(SamplingMode::Flat);
    group.sample_size(30);
    group.bench_function("kb_core_add_no_embed", |b| {
        b.iter_batched(
            setup_add_fixture,
            |fixture| kb_core::add(&fixture.paths, &noop, fixture.args).unwrap(),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("append_events_batch_only", |b| {
        b.iter_batched(
            setup_event_fixture,
            |fixture| events::append_events_batch(&fixture.paths.events, &fixture.batch).unwrap(),
            BatchSize::SmallInput,
        )
    });
    group.bench_function("db_apply_batch_only", |b| {
        b.iter_batched(
            setup_apply_fixture,
            |fixture| {
                for event in &fixture.batch {
                    db::apply_event(&fixture.conn, &noop, event).unwrap();
                }
            },
            BatchSize::SmallInput,
        )
    });
    group.finish();
}

criterion_group!(write_path_benches, write_path);
criterion_main!(write_path_benches);
