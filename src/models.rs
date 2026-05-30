//! Data models and vector math utilities

use serde::{Deserialize, Serialize};

/// Compute cosine similarity between two vectors.
/// Returns 0.0 if vectors have different lengths (dimension mismatch from
/// model upgrade or corrupt embedding blob).
pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
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
    if norm_a == 0.0 || norm_b == 0.0 {
        0.0
    } else {
        dot / (norm_a * norm_b)
    }
}

/// Convert f32 slice to little-endian byte blob (for SQLite storage).
pub fn f32s_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// Convert little-endian byte blob back to f32 vec.
pub fn blob_to_f32s(b: &[u8]) -> Vec<f32> {
    debug_assert!(b.len().is_multiple_of(4), "blob length {} not divisible by 4 — corrupt embedding?", b.len());
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
    /// ID of the evidence this was derived from (for kind=derived)
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

}
