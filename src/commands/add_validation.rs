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
}
