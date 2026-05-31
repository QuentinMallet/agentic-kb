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

/// Maximum allowed length of `citation_excerpt` in characters.
///
/// br-47d (security I3): caps caller-supplied excerpt text that is stored
/// verbatim and later marshalled into LLM context via the MCP envelope. The
/// 512-char cap is generous for legitimate quotes (≈10 lines of code) while
/// preventing large prompt-injection payloads from being smuggled through.
pub const MAX_CITATION_EXCERPT_CHARS: usize = 512;

/// Validate kind enum and evidence array at write time.
///
/// - `kind` must be one of the five valid values.
/// - Each evidence row must have `evidence.kind = "code"` (Phase 1 constraint).
/// - Each evidence row must have a non-empty `citation_hash`.
/// - `citation_excerpt` (if present) must be ≤ MAX_CITATION_EXCERPT_CHARS and
///   must not contain ASCII control characters other than `\n` and `\t`
///   (br-47d: prompt-injection containment).
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

        if let Some(excerpt) = ev.get("citation_excerpt").and_then(|v| v.as_str()) {
            validate_citation_excerpt(excerpt)?;
        }
    }

    Ok(())
}

/// Reject `citation_excerpt` payloads that are too long or contain ASCII
/// control characters other than newline and tab (br-47d).
///
/// Control characters carry no legitimate textual meaning in a citation
/// excerpt and have historically been used to confuse downstream LLM
/// tokenizers / split prompt boundaries. Allowing only `\n` and `\t`
/// preserves normal whitespace formatting.
fn validate_citation_excerpt(excerpt: &str) -> Result<()> {
    let char_count = excerpt.chars().count();
    if char_count > MAX_CITATION_EXCERPT_CHARS {
        anyhow::bail!(
            "citation_excerpt invalid: length {char_count} chars exceeds cap of {MAX_CITATION_EXCERPT_CHARS}"
        );
    }
    for (i, c) in excerpt.chars().enumerate() {
        if c.is_control() && c != '\n' && c != '\t' {
            anyhow::bail!(
                "citation_excerpt invalid: control char U+{:04X} at position {i}",
                c as u32
            );
        }
    }
    Ok(())
}

/// Envelope markers used to wrap `citation_excerpt` text returned from
/// `kb_search` so downstream LLM agents treat enclosed bytes as data, not
/// instructions (br-47d). Documented in the MCP tool description.
pub const CITATION_EXCERPT_ENVELOPE_OPEN: &str = "<<UNTRUSTED_EXCERPT>>";
pub const CITATION_EXCERPT_ENVELOPE_CLOSE: &str = "<<END>>";

/// Wrap a `citation_excerpt` value in the untrusted-data envelope. Returns
/// `None` if the input is `None`, so callers can `.map(wrap_citation_excerpt)`.
pub fn wrap_citation_excerpt(excerpt: Option<&str>) -> Option<String> {
    excerpt.map(|s| {
        format!(
            "{CITATION_EXCERPT_ENVELOPE_OPEN}{s}{CITATION_EXCERPT_ENVELOPE_CLOSE}"
        )
    })
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

    // br-47d: citation_excerpt validation + envelope.

    #[test]
    fn test_kb_add_rejects_long_excerpt() {
        let long: String = "a".repeat(MAX_CITATION_EXCERPT_CHARS + 1);
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": long,
        });
        let err = validate_kb_add_inputs("belief", &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_excerpt invalid"));
        assert!(err.to_string().contains("exceeds cap"));
    }

    #[test]
    fn test_kb_add_accepts_excerpt_at_cap() {
        let at_cap: String = "a".repeat(MAX_CITATION_EXCERPT_CHARS);
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": at_cap,
        });
        assert!(validate_kb_add_inputs("belief", &[ev]).is_ok());
    }

    #[test]
    fn test_kb_add_rejects_control_chars_in_excerpt() {
        // NUL byte — should be rejected.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\x00 bad }",
        });
        let err = validate_kb_add_inputs("belief", &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_excerpt invalid"));
        assert!(err.to_string().contains("control char"));

        // ESC (0x1B) — should be rejected.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\x1B[31m red }",
        });
        let err = validate_kb_add_inputs("belief", &[ev]).unwrap_err();
        assert!(err.to_string().contains("control char"));

        // Newline + tab — must be accepted.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\n\tbar\n}",
        });
        assert!(validate_kb_add_inputs("belief", &[ev]).is_ok());
    }

    #[test]
    fn test_wrap_citation_excerpt_wraps_value() {
        let wrapped = wrap_citation_excerpt(Some("fn foo() {}")).unwrap();
        assert!(wrapped.starts_with(CITATION_EXCERPT_ENVELOPE_OPEN));
        assert!(wrapped.ends_with(CITATION_EXCERPT_ENVELOPE_CLOSE));
        assert!(wrapped.contains("fn foo() {}"));
    }

    #[test]
    fn test_wrap_citation_excerpt_none_passthrough() {
        assert_eq!(wrap_citation_excerpt(None), None);
    }
}
