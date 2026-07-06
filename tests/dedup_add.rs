//! Near-duplicate detection on add (Memora pickup .5 acceptance gate).
//!
//! Memora's builder queries similar existing memories (cosine >= threshold)
//! before insert and lets an LLM decide update-vs-new. Here the KB only
//! *reports*: `kb_core::add` returns `similar_existing` (id, path, summary,
//! score) for live entries whose embedding cosine >= cutoff; the calling
//! agent decides merge / expire / proceed. The add always goes through.
//!
//! Invariants:
//!   1. A semantically identical existing entry is reported with score >= cutoff.
//!   2. An orthogonal entry is not reported.
//!   3. The new entry itself is never in the list (self-match excluded).
//!   4. cutoff=None disables the probe (no similar_existing, no extra embed).
//!   5. replace_path=true: entries at the same path are about to be expired —
//!      they must not be reported as duplicates.
//!   6. NoopEmbedder: probe silently disabled (no error).

use kb::components::embedder::Embedder;
use kb::components::kb_core::{add, AddArgs};
use kb::config::Paths;
use std::fs;
use tempfile::tempdir;

const DIM: usize = 384;

/// Deterministic two-cluster embedder: text containing "alpha" maps to basis
/// e0, text containing "beta" maps to basis e1. Same cluster → cosine 1.0,
/// cross cluster → 0.0.
struct ClusterEmbedder;

impl Embedder for ClusterEmbedder {
    fn embed(&self, text: &str) -> anyhow::Result<Vec<f32>> {
        let mut v = vec![0.0f32; DIM];
        if text.contains("alpha") {
            v[0] = 1.0;
        } else {
            v[1] = 1.0;
        }
        Ok(v)
    }
    fn is_noop(&self) -> bool {
        false
    }
}

fn setup() -> (tempfile::TempDir, Paths) {
    let dir = tempdir().unwrap();
    fs::create_dir_all(dir.path().join(".state/agent-kb")).unwrap();
    let paths = Paths::from_root(dir.path());
    (dir, paths)
}

fn args(id: &str, path: &str, summary: &str, dedup_cutoff: Option<f32>) -> AddArgs {
    AddArgs {
        id: id.into(),
        path: path.into(),
        summary: summary.into(),
        content: format!("content for {id}"),
        tags: serde_json::json!([]),
        version_ref: None,
        permanent: false,
        replace_path: false,
        kind: "belief".into(),
        evidence_status: "missing".into(),
        evidence_rows: vec![],
        ts: "2024-01-01T00:00:00Z".into(),
        session: "test".into(),
        session_id: None,
        expire_reason: String::new(),
        dedup_cutoff,
        cues: vec![],
    }
}

/// Invariants 1+2+3: same-cluster entry reported, orthogonal not, self never.
#[test]
fn test_similar_existing_reports_near_duplicate() {
    let (_dir, paths) = setup();
    let emb = ClusterEmbedder;

    add(&paths, &emb, args("dup-a", "notes/a", "alpha fact one", None)).unwrap();
    add(&paths, &emb, args("far-b", "notes/b", "beta unrelated", None)).unwrap();

    let out = add(&paths, &emb, args("dup-c", "notes/c", "alpha fact restated", Some(0.85))).unwrap();

    let ids: Vec<&str> = out.similar_existing.iter().map(|s| s.id.as_str()).collect();
    assert!(ids.contains(&"dup-a"), "same-cluster entry must be reported, got {ids:?}");
    assert!(!ids.contains(&"far-b"), "orthogonal entry must not be reported, got {ids:?}");
    assert!(!ids.contains(&"dup-c"), "the new entry must never self-match, got {ids:?}");

    let hit = out.similar_existing.iter().find(|s| s.id == "dup-a").unwrap();
    assert!(hit.score >= 0.85, "reported score must be >= cutoff, got {}", hit.score);
    assert_eq!(hit.path, "notes/a");
    assert_eq!(hit.summary, "alpha fact one");
}

/// Invariant 4: cutoff=None disables the probe entirely.
#[test]
fn test_dedup_disabled_when_cutoff_none() {
    let (_dir, paths) = setup();
    let emb = ClusterEmbedder;

    add(&paths, &emb, args("d1", "notes/1", "alpha original", None)).unwrap();
    let out = add(&paths, &emb, args("d2", "notes/2", "alpha again", None)).unwrap();
    assert!(out.similar_existing.is_empty(), "probe must be off when cutoff is None");
}

/// Invariant 5: replace_path expires same-path entries — they are not dupes.
#[test]
fn test_replace_path_same_path_not_reported() {
    let (_dir, paths) = setup();
    let emb = ClusterEmbedder;

    add(&paths, &emb, args("r1", "notes/replaced", "alpha v1", None)).unwrap();

    let mut a = args("r2", "notes/replaced", "alpha v2", Some(0.85));
    a.replace_path = true;
    let out = add(&paths, &emb, a).unwrap();
    assert!(
        out.similar_existing.iter().all(|s| s.path != "notes/replaced"),
        "entries being replaced at the same path must not be reported: {:?}",
        out.similar_existing.iter().map(|s| &s.id).collect::<Vec<_>>()
    );
}

/// Config accessor: cutoff outside (0.0, 1.0] disables the probe; unset
/// defaults to 0.85 (review finding: <=0 would flag everything as similar).
#[test]
fn test_dedup_cutoff_accessor_bounds() {
    use kb::config::KbConfig;
    let mut cfg = KbConfig::default();
    assert_eq!(cfg.dedup_cutoff(), Some(0.85), "unset -> default 0.85");
    cfg.dedup_cosine_cutoff = Some(0.9);
    assert_eq!(cfg.dedup_cutoff(), Some(0.9));
    cfg.dedup_cosine_cutoff = Some(0.0);
    assert_eq!(cfg.dedup_cutoff(), None, "0.0 must disable");
    cfg.dedup_cosine_cutoff = Some(-0.5);
    assert_eq!(cfg.dedup_cutoff(), None, "negative must disable");
    cfg.dedup_cosine_cutoff = Some(1.5);
    assert_eq!(cfg.dedup_cutoff(), None, ">1.0 must disable");
}

/// Invariant 6: NoopEmbedder disables the probe without error.
#[test]
fn test_noop_embedder_probe_disabled() {
    use kb::components::embedder::NoopEmbedder;
    let (_dir, paths) = setup();

    add(&paths, &NoopEmbedder, args("n1", "notes/n1", "alpha", None)).unwrap();
    let out = add(&paths, &NoopEmbedder, args("n2", "notes/n2", "alpha", Some(0.85))).unwrap();
    assert!(out.similar_existing.is_empty());
}
