//! Write-time validation helpers shared between the MCP and CLI kb_add paths.

use anyhow::Result;
use serde_json::Value;

/// Valid entry kind values.
const VALID_KINDS: &[&str] = &[
    "observation",
    "belief",
    "procedure",
    "convention",
    "memory",
];

/// Kinds that carry an evidence mandate (missing evidence → status="missing").
const EVIDENCE_MANDATED_KINDS: &[&str] = &["observation", "belief", "procedure"];

/// Validate tags array shape and tag length (br-9lq).
///
/// - `tags` must be a JSON array of strings.
/// - Each tag must be non-empty and <= 50 characters.
pub fn validate_tags(tags: &Value) -> Result<()> {
    let arr = tags
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tags must be a JSON array of strings"))?;

    for (idx, tag) in arr.iter().enumerate() {
        let s = tag.as_str().ok_or_else(|| {
            anyhow::anyhow!("tags[{}] must be a string", idx)
        })?;
        if s.is_empty() {
            anyhow::bail!("tags[{}] must be non-empty", idx);
        }
        if s.len() > 50 {
            anyhow::bail!("tags[{}] exceeds max length 50: '{s}'", idx);
        }
    }

    Ok(())
}

/// Validate kind enum and evidence array at write time.
///
/// - `kind` must be one of the five valid values.
/// - Each evidence row must have `evidence.kind = "code"` (Phase 1 constraint).
/// - Each evidence row must have a non-empty `citation_hash`.
pub fn validate_kb_add_inputs(kind: &str, evidence: &[Value]) -> Result<()> {
    if !VALID_KINDS.contains(&kind) {
        anyhow::bail!(
            "invalid kind '{kind}'; must be one of: observation, belief, procedure, convention, memory"
        );
    }

    for ev in evidence {
        let ev_kind = ev
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if ev_kind != "code" {
            anyhow::bail!(
                "Phase 1 ships evidence.kind=code only; kind='{ev_kind}' deferred to Phase 2"
            );
        }

        let hash = ev
            .get("citation_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if hash.is_empty() {
            anyhow::bail!("evidence row missing required citation_hash");
        }
    }

    Ok(())
}

/// Compute the write-time evidence_status for an entry.
///
/// Rules (AC10):
/// - kind in {observation, belief, procedure} + evidence empty  → "missing"
/// - kind in {observation, belief, procedure} + evidence present → "present"
/// - otherwise                                                    → "n/a"
pub fn compute_evidence_status_write(kind: &str, evidence: &[Value]) -> &'static str {
    if EVIDENCE_MANDATED_KINDS.contains(&kind) {
        if evidence.is_empty() {
            "missing"
        } else {
            "present"
        }
    } else {
        "n/a"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_valid_kinds_accepted() {
        for kind in VALID_KINDS {
            assert!(validate_kb_add_inputs(kind, &[]).is_ok(), "kind={kind} should be valid");
        }
    }

    #[test]
    fn test_invalid_kind_rejected() {
        let err = validate_kb_add_inputs("fact", &[]).unwrap_err();
        assert!(err.to_string().contains("invalid kind 'fact'"));
    }

    #[test]
    fn test_non_code_evidence_rejected() {
        let ev = json!({"kind": "test", "citation_hash": "sha256:abc"});
        let err = validate_kb_add_inputs("belief", &[ev]).unwrap_err();
        assert!(err.to_string().contains("Phase 1 ships evidence.kind=code only"));
        assert!(err.to_string().contains("kind='test'"));
    }

    #[test]
    fn test_empty_citation_hash_rejected() {
        let ev = json!({"kind": "code", "citation_hash": ""});
        let err = validate_kb_add_inputs("belief", &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_hash"));
    }

    #[test]
    fn test_valid_code_evidence_accepted() {
        let ev = json!({"kind": "code", "citation_hash": "sha256:abc123"});
        assert!(validate_kb_add_inputs("observation", &[ev]).is_ok());
    }

    #[test]
    fn test_evidence_status_missing() {
        assert_eq!(compute_evidence_status_write("observation", &[]), "missing");
        assert_eq!(compute_evidence_status_write("belief", &[]), "missing");
        assert_eq!(compute_evidence_status_write("procedure", &[]), "missing");
    }

    #[test]
    fn test_evidence_status_present() {
        let ev = json!({"kind": "code", "citation_hash": "sha256:abc"});
        assert_eq!(compute_evidence_status_write("observation", &[ev.clone()]), "present");
        assert_eq!(compute_evidence_status_write("belief", &[ev.clone()]), "present");
        assert_eq!(compute_evidence_status_write("procedure", &[ev]), "present");
    }

    #[test]
    fn test_evidence_status_na() {
        assert_eq!(compute_evidence_status_write("convention", &[]), "n/a");
        assert_eq!(compute_evidence_status_write("memory", &[]), "n/a");
    }

    // br-9lq: validate_tags shape and length

    #[test]
    fn test_tags_valid_array_accepted() {
        let tags = json!(["rust", "testing"]);
        assert!(validate_tags(&tags).is_ok(), "valid tags array should be accepted");
    }

    #[test]
    fn test_tags_empty_array_accepted() {
        let tags = json!([]);
        assert!(validate_tags(&tags).is_ok(), "empty tags array should be accepted");
    }

    #[test]
    fn test_tags_not_array_rejected() {
        let tags = json!("just a string");
        let err = validate_tags(&tags).unwrap_err();
        assert!(err.to_string().contains("tags must be a JSON array"));
    }

    #[test]
    fn test_tags_non_string_element_rejected() {
        let tags = json!(["rust", 42, "testing"]);
        let err = validate_tags(&tags).unwrap_err();
        assert!(err.to_string().contains("tags[1] must be a string"));
    }

    #[test]
    fn test_tags_empty_string_rejected() {
        let tags = json!(["rust", "", "testing"]);
        let err = validate_tags(&tags).unwrap_err();
        assert!(err.to_string().contains("tags[1] must be non-empty"));
    }

    #[test]
    fn test_tags_length_exceeded_rejected() {
        let long_tag = "x".repeat(51); // 51 chars, exceeds max 50
        let tags = json!(["rust", long_tag.as_str()]);
        let err = validate_tags(&tags).unwrap_err();
        assert!(err.to_string().contains("exceeds max length 50"));
        assert!(err.to_string().contains("tags[1]"));
    }

    #[test]
    fn test_tags_max_length_accepted() {
        let max_tag = "x".repeat(50); // exactly 50 chars
        let tags = json!(["rust", max_tag.as_str()]);
        assert!(validate_tags(&tags).is_ok(), "tag with max length 50 should be accepted");
    }
}
