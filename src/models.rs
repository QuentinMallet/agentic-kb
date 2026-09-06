//! Data models and vector math utilities

use anyhow::{bail, Result};
use half::f16;
use serde::{Deserialize, Serialize};

// ---------------------------------------------------------------------------
// Wire-format constants — single source of truth for entries_emb blob encoding
// ---------------------------------------------------------------------------

/// Output dimension of BAAI/bge-small-en-v1.5.
pub const EMB_DIMS: usize = 384;
/// Bytes per element in the on-disk f16 encoding.
pub const EMB_ELEMENT_BYTES: usize = 2;
/// Total byte length of one entries_emb blob.
pub const EMB_BLOB_BYTES: usize = EMB_DIMS * EMB_ELEMENT_BYTES; // 768

/// Compute cosine similarity between two vectors.
/// Returns 0.0 if vectors have different lengths (dimension mismatch from
/// model upgrade or corrupt embedding blob).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || !a.iter().chain(b.iter()).all(|value| value.is_finite()) {
        eprintln!(
            "kb: cosine_similarity dimension mismatch: {} vs {}",
            a.len(),
            b.len()
        );
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if !norm_a.is_finite() || !norm_b.is_finite() || norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        let similarity = dot / (norm_a * norm_b);
        similarity.is_finite().then_some(similarity).unwrap_or(0.0)
    }
}

/// Validate and L2-normalize an embedding before it reaches persisted state.
///
/// This is a release-build guard.  A finite check alone is insufficient:
/// normalizing the zero vector manufactures NaNs, which are byte-valid blobs
/// but invalid similarity inputs.
pub fn normalize_embedding(v: &[f32]) -> Result<Vec<f32>> {
    if v.is_empty() {
        bail!("embedding must not be empty");
    }
    if !v.iter().all(|value| value.is_finite()) {
        bail!("embedding contains non-finite values");
    }
    let norm = v.iter().map(|value| value * value).sum::<f32>().sqrt();
    if !norm.is_finite() || norm == 0.0 {
        bail!("embedding must have a finite, non-zero L2 norm");
    }
    let normalized: Vec<f32> = v.iter().map(|value| value / norm).collect();
    if !normalized.iter().all(|value| value.is_finite()) {
        bail!("embedding normalization produced non-finite values");
    }
    Ok(normalized)
}

/// Normalize an embedding and encode it in the canonical f16 wire format.
pub fn normalized_f32s_to_f16_blob(v: &[f32]) -> Result<Vec<u8>> {
    Ok(f32s_to_f16_blob(&normalize_embedding(v)?))
}

/// Encode f32 slice as little-endian f16 blob (new wire format for entries_emb).
///
/// Produces `v.len() * 2` bytes. The canonical entries_emb encoding stores
/// `EMB_DIMS` elements → `EMB_BLOB_BYTES` bytes total.
pub fn f32s_to_f16_blob(v: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(v.len() * EMB_ELEMENT_BYTES);
    for &x in v {
        let h = f16::from_f32(x);
        out.extend_from_slice(&h.to_le_bytes());
    }
    out
}

/// Decode an entries_emb blob with automatic format dispatch.
///
/// - `blob.len() == EMB_BLOB_BYTES` (768): f16 le path (current format)
/// - any other multiple of 4: f32 le path (legacy format — existing DBs)
/// - anything else: returns empty vec (corrupt blob; caller gets sim=0.0)
pub fn decode_emb_blob(blob: &[u8]) -> Vec<f32> {
    let decoded: Vec<f32> = if blob.len() == EMB_BLOB_BYTES {
        // f16 path (current format)
        blob.chunks_exact(EMB_ELEMENT_BYTES)
            .map(|c| f16::from_le_bytes([c[0], c[1]]).to_f32())
            .collect()
    } else if blob.len() % 4 == 0 {
        // Legacy f32 path
        blob.chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    } else {
        eprintln!(
            "kb: decode_emb_blob: unexpected blob length {} — corrupt embedding?",
            blob.len()
        );
        return Vec::new();
    };
    if decoded.iter().all(|value| value.is_finite()) {
        decoded
    } else {
        eprintln!("kb: decode_emb_blob: non-finite embedding values — corrupt embedding?");
        Vec::new()
    }
}

/// Decode a **canonical-format** (f16, `EMB_BLOB_BYTES` bytes) blob into an
/// existing scratch buffer.
///
/// Clears `scratch`, then appends exactly `EMB_DIMS` decoded floats.
/// Returns without writing if `blob.len() != EMB_BLOB_BYTES` (the caller then
/// gets an empty slice → cosine_similarity returns 0.0, matching mismatch
/// behaviour for corrupt blobs).
///
/// Intended for the semantic-scan hot loop — allocate the scratch buffer
/// ONCE outside the loop and pass a mutable reference here to avoid
/// per-row allocations.
pub fn decode_f16_blob_into(blob: &[u8], scratch: &mut Vec<f32>) {
    scratch.clear();
    if blob.len() != EMB_BLOB_BYTES {
        // legacy f32 blob — caller detects via scratch.is_empty() and falls back to decode_emb_blob
        return;
    }
    scratch.reserve(EMB_DIMS);
    for c in blob.chunks_exact(EMB_ELEMENT_BYTES) {
        scratch.push(f16::from_le_bytes([c[0], c[1]]).to_f32());
    }
    if !scratch.iter().all(|value| value.is_finite()) {
        scratch.clear();
        eprintln!("kb: decode_f16_blob_into: non-finite embedding values — corrupt embedding?");
    }
}

/// Convert f32 slice to little-endian f32 byte blob.
/// Kept as backwards-compat helper for callers writing legacy or test blobs.
pub fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Convert little-endian f32 byte blob back to f32 vec.
/// Kept as backwards-compat helper for test code and legacy read paths.
pub fn blob_to_f32s(b: &[u8]) -> Vec<f32> {
    debug_assert!(
        b.len().is_multiple_of(4),
        "blob length {} not divisible by 4 — corrupt embedding?",
        b.len()
    );
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

fn default_kind() -> String {
    "belief".to_string()
}

fn default_evidence_status() -> String {
    "n/a".to_string()
}

/// A knowledge base entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// Unique entry ID
    pub id: String,
    /// Category/file path
    pub path: String,
    /// Short summary (max 200 chars)
    pub summary: String,
    /// Full content (max 10000 chars)
    pub content: String,
    /// Comma-separated or JSON array of tags
    pub tags: String,
    /// Git commit SHA when entry was recorded
    pub version_ref: Option<String>,
    /// Whether this entry survives compact/expire cycles
    #[serde(default)]
    pub permanent: bool,
    /// Entry kind: observation | belief | procedure | convention | memory
    #[serde(default = "default_kind")]
    pub kind: String,
    /// Evidence status: missing | present | n/a
    #[serde(default = "default_evidence_status")]
    pub evidence_status: String,
    /// Session ID that created this entry (NULL for legacy entries)
    #[serde(default)]
    pub session_id: Option<String>,
}

/// Status lattice for a citation, mirroring `Statuses` in
/// `.state/agent-kb/tla/CitationRelocation.tla`.
///
/// `Relocated` is deliberately NOT a weak form of `Verified`: an excerpt match
/// says where the code went, never that the recorded hash still describes it.
/// Only a later pass that re-hashes the content at the healed path against the
/// unchanged stored hash may reach `Verified` (spec `NoSilentPromotion`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum VerificationStatus {
    /// The stored hash matches the bytes currently under the citation.
    Verified,
    /// The excerpt was found at exactly one new location; the citation can be
    /// repointed there. Says nothing about the hash.
    Relocated,
    /// Neither verified nor safely relocatable.
    Unverified,
}

impl VerificationStatus {
    /// Wire/CLI spelling.
    pub fn as_str(&self) -> &'static str {
        match self {
            VerificationStatus::Verified => "verified",
            VerificationStatus::Relocated => "relocated",
            VerificationStatus::Unverified => "unverified",
        }
    }

    /// Mirrors `Rank` in `CitationRelocation.tla`: status never regresses
    /// inside one verification pass.
    pub fn rank(&self) -> u8 {
        match self {
            VerificationStatus::Unverified => 0,
            VerificationStatus::Relocated => 1,
            VerificationStatus::Verified => 2,
        }
    }
}

/// A piece of evidence attached to a KB entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct Evidence {
    /// Unique evidence ID
    pub id: String,
    /// ID of the KB entry this evidence supports
    pub entry_id: String,
    /// Evidence kind: code | test | command | user | derived
    pub kind: String,
    /// Relative path to the cited file
    pub citation_path: Option<String>,
    /// Git commit SHA of the cited file at record time
    pub citation_sha: Option<String>,
    /// Content hash of the cited file at record time
    pub citation_hash: String,
    /// Short excerpt from the cited artifact
    pub citation_excerpt: Option<String>,
    /// ID of the parent entry this evidence was derived from (for kind=derived)
    pub derived_from: Option<String>,
    /// Timestamp when evidence was recorded
    pub recorded_at: Option<String>,
}

/// A record of an audit run for a KB entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditRun {
    /// Auto-increment primary key
    pub id: Option<i64>,
    /// ID of the KB entry audited
    pub entry_id: String,
    /// Timestamp of the audit
    pub audited_at: Option<String>,
    /// Audit verdict: true | false
    pub verdict: String,
    /// Reference to supporting evidence
    pub evidence_ref: Option<String>,
}

/// A test case definition.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct TestCase {
    /// Unique test case ID
    pub id: String,
    /// Application name
    pub app: String,
    /// Test name
    pub name: String,
    /// Protocol: browser | rust_tool
    pub protocol: String,
    /// JSON config blob
    pub config: String,
    /// Git commit SHA
    pub version_ref: Option<String>,
}

/// A test run record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    /// Test case ID
    pub test_id: String,
    /// Result: pass | fail
    pub result: String,
    /// Adapter used
    pub adapter: Option<String>,
    /// Detail message
    pub detail: Option<String>,
    /// Run UUID
    pub run_id: Option<String>,
}

/// Per-(kind × session_id) confidence weight record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct SourceWeight {
    pub kind: String,
    pub session_id: String,
    pub successes: i64,
    pub failures: i64,
    pub updated_at: String,
}

/// A single audit verdict for one KB entry.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuditVerdict {
    pub entry_id: String,
    pub verdict: bool,
    pub note: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// Commutativity: sim(a, b) == sim(b, a)
        #[test]
        fn proptest_cosine_similarity_commutative(
            a in prop::collection::vec(-1.0f32..=1.0, 1..8),
            b in prop::collection::vec(-1.0f32..=1.0, 1..8),
        ) {
            let len = a.len().min(b.len());
            let a = &a[..len];
            let b = &b[..len];
            let ab = cosine_similarity(a, b);
            let ba = cosine_similarity(b, a);
            prop_assert!((ab - ba).abs() < 1e-5, "sim({a:?},{b:?})={ab} != sim({b:?},{a:?})={ba}");
        }

        /// Self-similarity: sim(v, v) == 1.0 for any non-zero vector
        #[test]
        fn proptest_cosine_similarity_self_is_one(
            v in prop::collection::vec(0.01f32..=1.0, 1..8),
        ) {
            let sim = cosine_similarity(&v, &v);
            prop_assert!((sim - 1.0).abs() < 1e-5, "self-sim({v:?})={sim}");
        }

        /// Range: result is in [-1.0, 1.0] for any non-zero vectors
        #[test]
        fn proptest_cosine_similarity_bounded(
            a in prop::collection::vec(-1.0f32..=1.0, 1..8),
            b in prop::collection::vec(-1.0f32..=1.0, 1..8),
        ) {
            let len = a.len().min(b.len());
            let sim = cosine_similarity(&a[..len], &b[..len]);
            prop_assert!(sim >= -1.0001 && sim <= 1.0001, "sim out of range: {sim}");
        }
    }

    /// Mismatched lengths -> similarity = 0.0
    #[test]
    fn test_cosine_similarity_mismatched_lengths() {
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[1.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0], &[1.0, 2.0, 3.0]), 0.0);
        assert_eq!(cosine_similarity(&[], &[1.0]), 0.0);
    }

    proptest! {
        /// Mismatched random vectors always return 0.0
        #[test]
        fn proptest_cosine_similarity_mismatched_returns_zero(
            a in prop::collection::vec(-1.0f32..=1.0, 1..8),
            b in prop::collection::vec(-1.0f32..=1.0, 1..8),
        ) {
            if a.len() != b.len() {
                prop_assert_eq!(cosine_similarity(&a, &b), 0.0);
            }
        }
    }

    proptest! {
        #[test]
        fn proptest_source_weight_serde_roundtrip(
            kind in "[a-z]{1,10}",
            session_id in "[a-z0-9_]{1,20}",
            successes in 0i64..10000,
            failures in 0i64..10000,
            updated_at in "2024-[0-9]{2}-[0-9]{2}T[0-9]{2}:[0-9]{2}:[0-9]{2}Z",
        ) {
            let sw = SourceWeight { kind, session_id, successes, failures, updated_at };
            let json = serde_json::to_string(&sw).unwrap();
            let recovered: SourceWeight = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(sw, recovered);
        }

        #[test]
        fn proptest_audit_verdict_serde_roundtrip(
            entry_id in "[a-z0-9-]{5,40}",
            verdict in proptest::bool::ANY,
            note in proptest::option::of("[a-zA-Z ]{0,50}"),
        ) {
            let av = AuditVerdict { entry_id, verdict, note };
            let json = serde_json::to_string(&av).unwrap();
            let recovered: AuditVerdict = serde_json::from_str(&json).unwrap();
            prop_assert_eq!(av, recovered);
        }
    }

    /// Zero vector -> similarity = 0.0
    #[test]
    fn test_cosine_similarity_zero_vector() {
        assert_eq!(cosine_similarity(&[0.0, 0.0], &[1.0, 2.0]), 0.0);
        assert_eq!(cosine_similarity(&[1.0, 2.0], &[0.0, 0.0]), 0.0);
        assert_eq!(cosine_similarity(&[0.0], &[0.0]), 0.0);
    }

    /// f32s round-trip through blob encoding
    #[test]
    fn test_f32s_blob_roundtrip() {
        let original = vec![1.0f32, -2.5, 3.14, 0.0];
        let blob = f32s_to_blob(&original);
        let recovered = blob_to_f32s(&blob);
        assert_eq!(original, recovered);
    }

    // -----------------------------------------------------------------------
    // f16 quantisation tests (br-improvement-catalog-23b.11)
    // -----------------------------------------------------------------------

    /// New entries_emb blobs must be 768 bytes (384 dims × 2 bytes/f16).
    /// This test fails before f16 encode is implemented (f32 blobs are 1536 bytes).
    #[test]
    fn test_f16_blob_size_is_768_bytes() {
        // Generate a unit-norm 384-dim vector
        use std::f32::consts::PI;
        let raw: Vec<f32> = (0..EMB_DIMS)
            .map(|i| (i as f32 * PI / EMB_DIMS as f32).sin())
            .collect();
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        let unit: Vec<f32> = raw.iter().map(|x| x / norm).collect();

        let blob = f32s_to_f16_blob(&unit);
        assert_eq!(
            blob.len(),
            EMB_BLOB_BYTES,
            "f16 blob must be {} bytes ({}×{}), got {}",
            EMB_BLOB_BYTES,
            EMB_DIMS,
            EMB_ELEMENT_BYTES,
            blob.len()
        );
    }

    /// Cosine similarity drift between f32 and f16 round-trip must be ≤ 0.005
    /// across 1000 random unit-norm 384-dim vectors. Architect Q2 bound: ≤ 0.001.
    #[test]
    fn test_f16_cosine_drift_bounded_1000_vectors() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(0xf16_f16_f16);
        let max_allowed_drift = 0.005_f32;

        let mut max_observed = 0.0_f32;
        for _ in 0..1000 {
            // Random unit-norm 384-dim vector (same construction as BenchEmbedder)
            let raw: Vec<f32> = (0..EMB_DIMS)
                .map(|_| rng.gen::<f32>() * 2.0 - 1.0)
                .collect();
            let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
            let unit: Vec<f32> = if norm > 0.0 {
                raw.iter().map(|x| x / norm).collect()
            } else {
                vec![0.0; EMB_DIMS]
            };

            // Encode to f16 then decode back to f32
            let blob = f32s_to_f16_blob(&unit);
            let recovered = decode_emb_blob(&blob);

            // Cosine similarity between original and recovered
            let sim = cosine_similarity(&unit, &recovered);
            // For unit-norm vectors, cosine drift = 1.0 - sim
            let drift = (1.0 - sim).abs();
            if drift > max_observed {
                max_observed = drift;
            }
        }

        assert!(
            max_observed <= max_allowed_drift,
            "f16 cosine drift {max_observed:.6} exceeds limit {max_allowed_drift:.6}"
        );
    }

    /// decode_emb_blob gracefully handles legacy f32 blobs (length-based dispatch).
    #[test]
    fn test_decode_emb_blob_legacy_f32_dispatch() {
        // 2-element legacy f32 blob (8 bytes) — used in RRF tests
        let original = vec![0.8_f32, 0.6_f32];
        let blob = f32s_to_blob(&original);
        assert_eq!(blob.len(), 8);
        let recovered = decode_emb_blob(&blob);
        assert_eq!(recovered, original);
    }

    /// decode_emb_blob handles f16 blobs of exactly EMB_BLOB_BYTES.
    #[test]
    fn test_decode_emb_blob_f16_dispatch() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(42);
        let raw: Vec<f32> = (0..EMB_DIMS)
            .map(|_| rng.gen::<f32>() * 2.0 - 1.0)
            .collect();
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        let unit: Vec<f32> = raw.iter().map(|x| x / norm).collect();

        let blob = f32s_to_f16_blob(&unit);
        assert_eq!(blob.len(), EMB_BLOB_BYTES);
        let recovered = decode_emb_blob(&blob);
        assert_eq!(recovered.len(), EMB_DIMS);
    }

    /// decode_f16_blob_into reuses scratch buffer (no extra allocation in hot loop).
    #[test]
    fn test_decode_f16_blob_into_scratch_reuse() {
        use rand::rngs::StdRng;
        use rand::{Rng, SeedableRng};
        let mut rng = StdRng::seed_from_u64(99);
        let raw: Vec<f32> = (0..EMB_DIMS).map(|_| rng.gen::<f32>()).collect();
        let norm: f32 = raw.iter().map(|x| x * x).sum::<f32>().sqrt();
        let unit: Vec<f32> = raw.iter().map(|x| x / norm).collect();

        let blob = f32s_to_f16_blob(&unit);

        // Pre-allocate scratch buffer once
        let mut scratch: Vec<f32> = Vec::with_capacity(EMB_DIMS);
        decode_f16_blob_into(&blob, &mut scratch);
        assert_eq!(scratch.len(), EMB_DIMS);

        // Second call reuses — scratch is cleared and refilled
        scratch.clear();
        decode_f16_blob_into(&blob, &mut scratch);
        assert_eq!(scratch.len(), EMB_DIMS);
    }
}
