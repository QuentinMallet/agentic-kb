//! V1 — citation relocation engine.
//!
//! Every test here maps to an invariant of `agent-kb/tla/CitationRelocation.tla`
//! or to an explicit AC of `.omc/plans/kb-delivery.md` §8 V1.  The mapping is
//! named in each test's doc comment so a spec revision has an obvious blast
//! radius.

use kb::components::db::{apply_event, open_db_memory, SEARCH_PATH_RELOCATION_POLICY};
use kb::components::embedder::{Embedder, NoopEmbedder};
use kb::components::events::citation_healed_event;
use kb::components::verification::{
    verify_evidence, RelocationPolicy, UnverifiedReason, MIN_EXCERPT_BYTES, MIN_EXCERPT_LINES,
};
use kb::models::{Evidence, VerificationStatus};
use rusqlite::Connection;
use sha2::{Digest, Sha256};
use std::fs;
use std::path::Path;

/// A 66-byte, 3-line excerpt: clears both floors (≥64 bytes, ≥2 lines).
const STRONG_EXCERPT: &str =
    "fn relocate_me(input: &str) -> usize {\n    input.as_bytes().len()\n}";

fn sha256_hex(b: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(b);
    format!("{:x}", h.finalize())
}

fn write_file(root: &Path, rel: &str, content: &str) {
    let abs = root.join(rel);
    if let Some(parent) = abs.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(abs, content).unwrap();
}

/// Build an evidence row citing `rel[start..end]` with a hash of `hashed_bytes`.
fn evidence(rel: &str, start: usize, end: usize, hashed: &[u8], excerpt: Option<&str>) -> Evidence {
    Evidence {
        id: "ev-1".to_string(),
        entry_id: "entry-1".to_string(),
        kind: "code".to_string(),
        citation_path: Some(format!("{rel}:{start}-{end}")),
        citation_sha: None,
        citation_hash: sha256_hex(hashed),
        citation_excerpt: excerpt.map(str::to_string),
        derived_from: None,
        recorded_at: Some("2026-01-01T00:00:00Z".to_string()),
    }
}

/// Repo where the cited file no longer holds the excerpt and `moved_to` does.
///
/// The citation range is exactly the excerpt, so an anchored relocation
/// reconstructs a byte-identical range and a LATER pass can re-hash it.
struct MovedRepo {
    dir: tempfile::TempDir,
    ev: Evidence,
}

fn moved_repo(moved_to: &str, decoys: &[&str]) -> MovedRepo {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // The cited file survives, and stays long enough for the recorded range to
    // remain in bounds, but the excerpt is gone: the hash check reaches a
    // genuine mismatch rather than an out-of-range read.
    write_file(
        root,
        "src/old.rs",
        &"// the cited code moved away\n".repeat(4),
    );
    write_file(
        root,
        moved_to,
        &format!("// preamble\n{STRONG_EXCERPT}\n// trailer\n"),
    );
    for (i, d) in decoys.iter().enumerate() {
        write_file(root, d, &format!("// decoy {i}\n{STRONG_EXCERPT}\n"));
    }
    let ev = evidence(
        "src/old.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );
    MovedRepo { dir, ev }
}

// ---------------------------------------------------------------------------
// Excerpt floor — AC "excerpt ≥64 bytes AND ≥2 lines", spec WeakExcerptUnverified
// ---------------------------------------------------------------------------

/// Excerpt one byte under the floor is never relocated (`WeakExcerptUnverified`).
#[test]
fn excerpt_of_63_bytes_is_too_weak() {
    let repo = moved_repo("src/new.rs", &[]);
    // 63 bytes, still 2 lines: only the byte floor is violated.
    let short: String = {
        let mut s = String::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"); // 31
        s.push_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"); // 32 → 63
        s
    };
    assert_eq!(short.len(), MIN_EXCERPT_BYTES - 1);
    assert!(short.lines().count() >= MIN_EXCERPT_LINES);

    let mut ev = repo.ev.clone();
    ev.citation_excerpt = Some(short);
    let out = verify_evidence(&ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Unverified);
    assert_eq!(out.reason, Some(UnverifiedReason::ExcerptTooWeak));
}

/// Exactly at the byte floor, with the line floor met, relocation is allowed.
#[test]
fn excerpt_of_64_bytes_clears_the_byte_floor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let excerpt: String = {
        let mut s = String::from("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\n"); // 32
        s.push_str("bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"); // 32 → 64
        s
    };
    assert_eq!(excerpt.len(), MIN_EXCERPT_BYTES);

    write_file(root, "src/old.rs", "// moved away\n// nothing\n");
    write_file(root, "src/new.rs", &format!("// head\n{excerpt}\n"));
    let ev = evidence(
        "src/old.rs",
        0,
        excerpt.len(),
        excerpt.as_bytes(),
        Some(&excerpt),
    );

    let out = verify_evidence(&ev, root, RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated, "{out:?}");
    assert!(out.relocated_to.unwrap().starts_with("src/new.rs:"));
}

/// A single-line excerpt is too weak even when it is long (`WeakExcerptUnverified`).
#[test]
fn single_line_excerpt_is_too_weak() {
    let repo = moved_repo("src/new.rs", &[]);
    let one_line = "x".repeat(200);
    assert_eq!(one_line.lines().count(), MIN_EXCERPT_LINES - 1);

    let mut ev = repo.ev.clone();
    ev.citation_excerpt = Some(one_line);
    let out = verify_evidence(&ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.reason, Some(UnverifiedReason::ExcerptTooWeak));
}

/// Two lines is the floor, not three.
#[test]
fn two_line_excerpt_clears_the_line_floor() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let excerpt = format!("{}\n{}", "a".repeat(40), "b".repeat(40));
    assert_eq!(excerpt.lines().count(), MIN_EXCERPT_LINES);

    write_file(root, "src/old.rs", "// moved away\n// nothing\n");
    write_file(root, "src/new.rs", &format!("// head\n{excerpt}\n"));
    let ev = evidence(
        "src/old.rs",
        0,
        excerpt.len(),
        excerpt.as_bytes(),
        Some(&excerpt),
    );

    let out = verify_evidence(&ev, root, RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated, "{out:?}");
}

/// A missing excerpt is the weakest excerpt there is.
#[test]
fn absent_excerpt_is_too_weak() {
    let repo = moved_repo("src/new.rs", &[]);
    let mut ev = repo.ev.clone();
    ev.citation_excerpt = None;
    let out = verify_evidence(&ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.reason, Some(UnverifiedReason::ExcerptTooWeak));
}

/// Path-escaping citations never enter relocation; the original failure wins.
#[test]
fn path_escape_skips_relocation_and_preserves_the_original_reason() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "src/new.rs", &format!("// preamble\n{STRONG_EXCERPT}\n"));
    write_file(root, "../outside.rs", &format!("{STRONG_EXCERPT}\n"));

    let ev = evidence(
        "../outside.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );

    for policy in [RelocationPolicy::FileOnly, RelocationPolicy::FileThenRepo] {
        let out = verify_evidence(&ev, root, policy).unwrap();
        assert_eq!(out.status, VerificationStatus::Unverified);
        assert_eq!(out.reason, Some(UnverifiedReason::PathEscape));
        assert!(out.relocated_to.is_none());
    }
}

/// Missing files also skip relocation outright; a repo candidate must not mask that.
#[test]
fn file_missing_skips_relocation_and_preserves_the_original_reason() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "src/new.rs", &format!("// preamble\n{STRONG_EXCERPT}\n"));

    let ev = evidence(
        "src/missing.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );

    for policy in [RelocationPolicy::FileOnly, RelocationPolicy::FileThenRepo] {
        let out = verify_evidence(&ev, root, policy).unwrap();
        assert_eq!(out.status, VerificationStatus::Unverified);
        assert_eq!(out.reason, Some(UnverifiedReason::FileMissing));
        assert!(out.relocated_to.is_none());
    }
}

/// Non-content decay reasons must survive weak excerpts under relocation-enabled policies.
#[test]
fn weak_excerpt_preserves_non_hash_failure_reasons() {
    use std::fs::File;

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let file_path = root.join("big.bin");
    let f = File::create(&file_path).unwrap();
    f.set_len(64 * 1024 * 1024 + 1).unwrap();
    drop(f);

    let mut ev = evidence("big.bin", 0, 1, b"x", Some("x"));
    ev.citation_excerpt = Some("x".to_string());

    for policy in [RelocationPolicy::FileOnly, RelocationPolicy::FileThenRepo] {
        let out = verify_evidence(&ev, root, policy).unwrap();
        assert_eq!(out.status, VerificationStatus::Unverified);
        assert_eq!(out.reason, Some(UnverifiedReason::FileTooLarge));
        assert!(out.relocated_to.is_none());
    }
}

// ---------------------------------------------------------------------------
// Policy semantics
// ---------------------------------------------------------------------------

/// `Never` performs no relocation search at all: a moved citation stays
/// `unverified` with the raw hash-check reason.
#[test]
fn never_policy_reports_hash_mismatch_without_searching() {
    let repo = moved_repo("src/new.rs", &[]);
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::Never).unwrap();
    assert_eq!(out.status, VerificationStatus::Unverified);
    assert_eq!(out.reason, Some(UnverifiedReason::HashMismatch));
    assert!(out.relocated_to.is_none());
}

/// `FileOnly` searches the cited file and stops; a cross-file move is not found.
#[test]
fn file_only_policy_does_not_walk_the_repo() {
    let repo = moved_repo("src/new.rs", &[]);
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileOnly).unwrap();
    assert_eq!(out.status, VerificationStatus::Unverified);
    assert_eq!(out.reason, Some(UnverifiedReason::NoCandidate));
}

/// A move WITHIN the cited file is found by `FileOnly`.
#[test]
fn file_only_policy_finds_an_in_file_move() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let prefix = "// a newly inserted header comment\n";
    write_file(root, "src/old.rs", &format!("{prefix}{STRONG_EXCERPT}\n"));
    let ev = evidence(
        "src/old.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );

    let out = verify_evidence(&ev, root, RelocationPolicy::FileOnly).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated, "{out:?}");
    assert_eq!(
        out.relocated_to.unwrap(),
        format!(
            "src/old.rs:{}-{}",
            prefix.len(),
            prefix.len() + STRONG_EXCERPT.len()
        )
    );
}

/// Exactly one candidate repo-wide relocates.
#[test]
fn unique_repo_candidate_relocates() {
    let repo = moved_repo("src/moved/new.rs", &[]);
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated, "{out:?}");
    assert!(out
        .relocated_to
        .as_deref()
        .unwrap()
        .starts_with("src/moved/new.rs:"));
}

/// Two candidates are never guessed between; the multiplicity is reported
/// (`NonUniqueUnverified`, pre-mortem S1).
#[test]
fn multiple_candidates_report_multiplicity() {
    let repo = moved_repo("src/new.rs", &["src/copy.rs"]);
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Unverified);
    match out.reason {
        Some(UnverifiedReason::NonUnique { candidates }) => assert!(candidates >= 2),
        other => panic!("expected NonUnique, got {other:?}"),
    }
}

/// Zero candidates is not a relocation.
#[test]
fn zero_candidates_is_unverified() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "src/old.rs", "// everything is gone now\n// really\n");
    let ev = evidence(
        "src/old.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );
    let out = verify_evidence(&ev, root, RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.reason, Some(UnverifiedReason::NoCandidate));
}

/// Excluded directories are outside the search scope: a match inside `target/`
/// is not a candidate.
#[test]
fn excluded_directories_are_not_searched() {
    for excluded in [".git", "target", "node_modules"] {
        let repo = moved_repo(&format!("{excluded}/build/new.rs"), &[]);
        let out =
            verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
        assert_eq!(
            out.reason,
            Some(UnverifiedReason::NoCandidate),
            "a match inside {excluded}/ must not be a candidate"
        );
    }
}

/// A plain directory name in `.gitignore` is excluded from the walk.
#[test]
fn gitignored_directory_is_not_searched() {
    let repo = moved_repo("vendor/new.rs", &[]);
    write_file(repo.dir.path(), ".gitignore", "vendor/\n*.tmp\n");
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.reason, Some(UnverifiedReason::NoCandidate));
}

/// The search path is pinned to `Never` (pre-mortem S2 head-of-line blocking).
#[test]
fn search_path_relocation_policy_is_never() {
    assert_eq!(SEARCH_PATH_RELOCATION_POLICY, RelocationPolicy::Never);
}

// ---------------------------------------------------------------------------
// Spec: NoHealOnVerified / VerifiedImpliesHashMatch
// ---------------------------------------------------------------------------

/// A matching hash verifies directly — no relocation search is performed, so a
/// verified row can never be healed (`NoHealOnVerified`).  Decoy copies of the
/// excerpt elsewhere in the tree would make the search non-unique; the outcome
/// is `Verified` regardless, which is only possible if no search ran.
#[test]
fn hash_match_verifies_without_searching() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write_file(root, "src/old.rs", &format!("{STRONG_EXCERPT}\n"));
    write_file(root, "src/decoy_a.rs", &format!("{STRONG_EXCERPT}\n"));
    write_file(root, "src/decoy_b.rs", &format!("{STRONG_EXCERPT}\n"));
    let ev = evidence(
        "src/old.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );

    for policy in [
        RelocationPolicy::Never,
        RelocationPolicy::FileOnly,
        RelocationPolicy::FileThenRepo,
    ] {
        let out = verify_evidence(&ev, root, policy).unwrap();
        assert_eq!(
            out.status,
            VerificationStatus::Verified,
            "policy {policy:?}"
        );
        assert!(out.relocated_to.is_none(), "policy {policy:?}");
    }
}

// ---------------------------------------------------------------------------
// Spec: NoSilentPromotion — a relocated row is never promoted in the same pass
// ---------------------------------------------------------------------------

/// The pass that relocates reports `Relocated`.  Only a LATER pass, re-hashing
/// the content at the healed path against the UNCHANGED stored hash, may reach
/// `Verified`.
#[test]
fn relocation_pass_never_promotes_to_verified() {
    let repo = moved_repo("src/new.rs", &[]);
    let out = verify_evidence(&repo.ev, repo.dir.path(), RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated);

    // Simulate the heal: the citation now points at the new location.
    let mut healed = repo.ev.clone();
    let new_path = out.relocated_to.clone().unwrap();
    healed.citation_path = Some(new_path);
    assert_eq!(
        healed.citation_hash, repo.ev.citation_hash,
        "heal must not rewrite the stored hash"
    );

    // A fresh pass re-hashes; here the moved bytes are identical, so it verifies.
    let second = verify_evidence(&healed, repo.dir.path(), RelocationPolicy::Never).unwrap();
    assert_eq!(second.status, VerificationStatus::Verified);
}

/// A heal whose reconstructed range does not re-hash stays `unverified` — the
/// excerpt match is never carried over as an assertion about the full range.
#[test]
fn heal_without_a_matching_rehash_does_not_verify() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    // Cited range = excerpt + a tail that does NOT survive the move.
    let original_tail = "\n// tail AAAAAAAAAAAA\n";
    let moved_tail = "\n// tail BBBBBBBBBBBB\n";
    assert_eq!(original_tail.len(), moved_tail.len());
    let original_range = format!("{STRONG_EXCERPT}{original_tail}");

    write_file(root, "src/old.rs", "// gone\n// gone\n");
    write_file(root, "src/new.rs", &format!("{STRONG_EXCERPT}{moved_tail}"));
    let ev = evidence(
        "src/old.rs",
        0,
        original_range.len(),
        original_range.as_bytes(),
        Some(STRONG_EXCERPT),
    );

    let out = verify_evidence(&ev, root, RelocationPolicy::FileThenRepo).unwrap();
    assert_eq!(out.status, VerificationStatus::Relocated);

    let mut healed = ev.clone();
    healed.citation_path = out.relocated_to.clone();
    let second = verify_evidence(&healed, root, RelocationPolicy::Never).unwrap();
    assert_eq!(
        second.status,
        VerificationStatus::Unverified,
        "an excerpt match must never launder into a hash assertion"
    );
}

// ---------------------------------------------------------------------------
// Spec: StoredHashImmutable — the citation_healed event round-trip
// ---------------------------------------------------------------------------

fn seed_entry_with_evidence(conn: &Connection, embedder: &dyn Embedder) -> Vec<serde_json::Value> {
    let upsert = serde_json::json!({
        "action": "upsert", "table": "entries",
        "id": "entry-1", "path": "architecture/relocation",
        "summary": "seed", "content": "seed content",
        "tags": ["v1"], "kind": "observation", "evidence_status": "present",
        "permanent": false, "is_stale": false, "ts": "2026-01-01T00:00:00Z",
    });
    let ev = evidence(
        "src/old.rs",
        0,
        STRONG_EXCERPT.len(),
        STRONG_EXCERPT.as_bytes(),
        Some(STRONG_EXCERPT),
    );
    let ev_add = kb::components::events::evidence_add_event("entry-1", &ev, Some("deadbeef"));
    let events = vec![upsert, ev_add];
    for e in &events {
        apply_event(conn, embedder, e).unwrap();
    }
    events
}

fn read_citation(conn: &Connection) -> (String, String) {
    conn.query_row(
        "SELECT citation_path, citation_hash FROM evidence WHERE id='ev-1'",
        [],
        |r| Ok((r.get(0)?, r.get(1)?)),
    )
    .unwrap()
}

/// `citation_healed` rewrites the path and only the path; replaying the same
/// event log into a fresh DB reproduces the healed path with the stored hash
/// untouched (P2 / F1 — the event log is the durable substrate).
#[test]
fn citation_healed_event_round_trips_through_replay() {
    let embedder = NoopEmbedder;
    let live = open_db_memory().unwrap();
    let mut events = seed_entry_with_evidence(&live, &embedder);
    let (old_path, original_hash) = read_citation(&live);

    let heal = citation_healed_event(
        "entry-1",
        "ev-1",
        &old_path,
        "src/new.rs:11-77",
        &original_hash,
        Some("cafebabe"),
    );
    apply_event(&live, &embedder, &heal).unwrap();
    events.push(heal);

    let (healed_path, live_hash) = read_citation(&live);
    assert_eq!(healed_path, "src/new.rs:11-77");
    assert_eq!(
        live_hash, original_hash,
        "heal must not write citation_hash"
    );

    // Replay the whole log into a fresh DB — this is what `kb rebuild` does.
    let replayed = open_db_memory().unwrap();
    for e in &events {
        apply_event(&replayed, &embedder, e).unwrap();
    }
    let (replayed_path, replayed_hash) = read_citation(&replayed);
    assert_eq!(replayed_path, healed_path);
    assert_eq!(replayed_hash, original_hash);
}

/// The event carries the audit trail: old path, new path, and the unchanged hash.
#[test]
fn citation_healed_event_records_the_audit_trail() {
    let e = citation_healed_event(
        "entry-1",
        "ev-1",
        "src/old.rs:0-66",
        "src/new.rs:11-77",
        "sha256:abc",
        None,
    );
    assert_eq!(e["action"], "citation_healed");
    assert_eq!(e["table"], "evidence");
    assert_eq!(e["entry_id"], "entry-1");
    assert_eq!(e["evidence_id"], "ev-1");
    assert_eq!(e["old_path"], "src/old.rs:0-66");
    assert_eq!(e["new_path"], "src/new.rs:11-77");
    assert_eq!(e["citation_hash"], "sha256:abc");
}

// ---------------------------------------------------------------------------
// Property tests mirroring the spec invariants
// ---------------------------------------------------------------------------

mod props {
    use super::*;
    use proptest::prelude::*;

    fn policies() -> impl Strategy<Value = RelocationPolicy> {
        prop_oneof![
            Just(RelocationPolicy::Never),
            Just(RelocationPolicy::FileOnly),
            Just(RelocationPolicy::FileThenRepo),
        ]
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 24, .. ProptestConfig::default() })]

        /// `NoHealOnVerified` + `VerifiedImpliesHashMatch`: when the stored hash
        /// matches the bytes under the citation, the outcome is `Verified` and
        /// no relocation search is attempted — however many decoys exist.
        #[test]
        fn prop_hash_match_never_searches(decoys in 0usize..4, policy in policies()) {
            let dir = tempfile::tempdir().unwrap();
            let root = dir.path();
            write_file(root, "src/old.rs", &format!("{STRONG_EXCERPT}\n"));
            for i in 0..decoys {
                write_file(root, &format!("src/decoy{i}.rs"), &format!("{STRONG_EXCERPT}\n"));
            }
            let ev = evidence("src/old.rs", 0, STRONG_EXCERPT.len(),
                              STRONG_EXCERPT.as_bytes(), Some(STRONG_EXCERPT));
            let out = verify_evidence(&ev, root, policy).unwrap();
            prop_assert_eq!(out.status, VerificationStatus::Verified);
            prop_assert!(out.relocated_to.is_none());
        }

        /// `NonUniqueUnverified`: two or more candidates are never relocated.
        #[test]
        fn prop_non_unique_is_never_relocated(extra in 1usize..4) {
            let decoys: Vec<String> = (0..extra).map(|i| format!("src/copy{i}.rs")).collect();
            let refs: Vec<&str> = decoys.iter().map(String::as_str).collect();
            let repo = moved_repo("src/new.rs", &refs);
            let out = verify_evidence(&repo.ev, repo.dir.path(),
                                      RelocationPolicy::FileThenRepo).unwrap();
            prop_assert_ne!(out.status, VerificationStatus::Relocated);
            let is_non_unique = matches!(out.reason, Some(UnverifiedReason::NonUnique { .. }));
            prop_assert!(is_non_unique, "expected NonUnique, got {:?}", out.reason);
        }

        /// `WeakExcerptUnverified`: below either floor, nothing is relocated —
        /// even when the excerpt has exactly one candidate in the tree.
        #[test]
        fn prop_weak_excerpt_is_never_relocated(
            bytes in 0usize..MIN_EXCERPT_BYTES,
            multiline in any::<bool>(),
        ) {
            let repo = moved_repo("src/new.rs", &[]);
            let weak = if multiline && bytes >= 2 {
                format!("{}\n{}", "a".repeat(bytes - 2), "b")
            } else {
                "a".repeat(bytes)
            };
            let mut ev = repo.ev.clone();
            ev.citation_excerpt = Some(weak);
            let out = verify_evidence(&ev, repo.dir.path(),
                                      RelocationPolicy::FileThenRepo).unwrap();
            prop_assert_ne!(out.status, VerificationStatus::Relocated);
            prop_assert_eq!(out.reason, Some(UnverifiedReason::ExcerptTooWeak));
        }

        /// Relocation is idempotent and status is monotone on an unchanged tree:
        /// repeating the pass yields the identical outcome (`Monotonicity`).
        #[test]
        fn prop_relocation_is_idempotent(decoys in 0usize..3, policy in policies()) {
            let names: Vec<String> = (0..decoys).map(|i| format!("src/copy{i}.rs")).collect();
            let refs: Vec<&str> = names.iter().map(String::as_str).collect();
            let repo = moved_repo("src/new.rs", &refs);
            let first = verify_evidence(&repo.ev, repo.dir.path(), policy).unwrap();
            let second = verify_evidence(&repo.ev, repo.dir.path(), policy).unwrap();
            prop_assert_eq!(&first, &second);
            prop_assert_eq!(first.status.rank(), second.status.rank());
        }

        /// `StoredHashImmutable`: no sequence of heals rewrites the stored hash.
        #[test]
        fn prop_heal_never_writes_the_stored_hash(
            paths in prop::collection::vec("src/[a-z]{1,6}\\.rs:[0-9]{1,3}-[0-9]{3,4}", 1..5)
        ) {
            let embedder = NoopEmbedder;
            let conn = open_db_memory().unwrap();
            seed_entry_with_evidence(&conn, &embedder);
            let (_, original_hash) = read_citation(&conn);

            for new_path in &paths {
                let (old_path, _) = read_citation(&conn);
                let heal = citation_healed_event(
                    "entry-1", "ev-1", &old_path, new_path, &original_hash, None);
                apply_event(&conn, &embedder, &heal).unwrap();
                let (current_path, current_hash) = read_citation(&conn);
                prop_assert_eq!(&current_path, new_path);
                prop_assert_eq!(&current_hash, &original_hash);
            }
        }
    }
}
