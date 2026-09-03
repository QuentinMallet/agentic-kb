//! Write-time validation helpers shared between the MCP and CLI kb_add paths.

use anyhow::Result;
use serde_json::Value;

pub use crate::components::db::MAX_EVIDENCE_ROWS_PER_ENTRY;

/// Valid entry kind values.
const VALID_KINDS: &[&str] = &["observation", "belief", "procedure", "convention", "memory"];

/// Kinds that require at least one evidence row at write time.
const EVIDENCE_MANDATED_KINDS: &[&str] = &["observation", "belief", "procedure"];

/// Maximum allowed length of `citation_excerpt` in characters.
///
/// br-47d (security I3): caps caller-supplied excerpt text that is stored
/// verbatim and later marshalled into LLM context via the MCP envelope. The
/// 512-char cap is generous for legitimate quotes (≈10 lines of code) while
/// preventing large prompt-injection payloads from being smuggled through.
pub const MAX_CITATION_EXCERPT_CHARS: usize = 512;

/// Validate tags array shape and tag length (br-9lq).
///
/// - `tags` must be a JSON array of strings.
/// - Each tag must be non-empty and <= 50 characters.
pub fn validate_tags(tags: &Value) -> Result<()> {
    let arr = tags
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("tags must be a JSON array of strings"))?;

    for (idx, tag) in arr.iter().enumerate() {
        let s = tag
            .as_str()
            .ok_or_else(|| anyhow::anyhow!("tags[{}] must be a string", idx))?;
        if s.is_empty() {
            anyhow::bail!("tags[{}] must be non-empty", idx);
        }
        if s.len() > 50 {
            anyhow::bail!("tags[{}] exceeds max length 50: '{s}'", idx);
        }
    }

    Ok(())
}

/// Validate kind enum, tags, and evidence array at write time.
///
/// - `kind` must be one of the five valid values.
/// - `tags` must pass `validate_tags` (array of non-empty strings ≤ 50 chars).
/// - Evidence-less writes are accepted for every kind. Observation, belief,
///   and procedure entries receive `evidence_status="missing"` from
///   `compute_evidence_status_write` (bd-r05y.3 soft mandate).
/// - Each evidence row must have `evidence.kind ∈ {"code","derived"}` (Phase 1 constraint).
/// - For `evidence.kind = "derived"`, `derived_from` must not equal the entry's own id.
/// - Each evidence row must have a non-empty `citation_path` or `citation_hash`.
///   Path-only rows are resolved by `kb_core::add` before event construction.
/// - `citation_excerpt` (if present) must be ≤ MAX_CITATION_EXCERPT_CHARS and
///   must not contain ASCII control characters other than `\n` and `\t`
///   (br-47d: prompt-injection containment).
pub fn validate_kb_add_inputs(
    entry_id: &str,
    kind: &str,
    tags: &Value,
    evidence: &[Value],
) -> Result<()> {
    if !VALID_KINDS.contains(&kind) {
        anyhow::bail!(
            "invalid kind '{kind}'; must be one of: observation, belief, procedure, convention, memory"
        );
    }

    validate_tags(tags)?;

    if evidence.len() > MAX_EVIDENCE_ROWS_PER_ENTRY {
        anyhow::bail!(
            "too many evidence rows: {} (max {})",
            evidence.len(),
            MAX_EVIDENCE_ROWS_PER_ENTRY
        );
    }

    for ev in evidence {
        let ev_kind = ev.get("kind").and_then(|v| v.as_str()).unwrap_or("");
        if ev_kind != "code" && ev_kind != "derived" {
            anyhow::bail!(
                "Phase 1 ships evidence.kind=code|derived only; kind='{ev_kind}' deferred to Phase 2"
            );
        }
        if ev_kind == "derived" {
            let derived_from = ev
                .get("derived_from")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if derived_from == entry_id {
                anyhow::bail!(
                    "self_loop_provenance: evidence.derived_from must not equal the entry id"
                );
            }
        }

        let path = ev
            .get("citation_path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let hash = ev
            .get("citation_hash")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if path.is_empty() && hash.is_empty() {
            anyhow::bail!("evidence row missing required citation_path or citation_hash");
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
    // br-47d: reject envelope markers so a malicious writer cannot break out
    // of the <<UNTRUSTED_EXCERPT>>...<<END>> envelope constructed at read time.
    if excerpt.contains(CITATION_EXCERPT_ENVELOPE_OPEN)
        || excerpt.contains(CITATION_EXCERPT_ENVELOPE_CLOSE)
    {
        anyhow::bail!("citation_excerpt invalid: must not contain envelope markers");
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

/// Neutralize delimiter starts inside untrusted text. The inserted U+200B is
/// visually unobtrusive and removal restores the original excerpt losslessly.
pub fn neutralize_citation_excerpt(excerpt: &str) -> String {
    excerpt.replace("<<", "<\u{200b}<")
}

/// Wrap a `citation_excerpt` value in the untrusted-data envelope. Returns
/// `None` if the input is `None`, so callers can `.map(wrap_citation_excerpt)`.
pub fn wrap_citation_excerpt(excerpt: Option<&str>) -> Option<String> {
    excerpt.map(|s| {
        let safe = neutralize_citation_excerpt(s);
        format!("{CITATION_EXCERPT_ENVELOPE_OPEN}{safe}{CITATION_EXCERPT_ENVELOPE_CLOSE}")
    })
}

/// Compute the write-time evidence_status for an entry.
///
/// This is used when constructing new events so live write paths emit a
/// self-consistent payload. Replay remains authoritative on read-model state:
/// `apply_event` recomputes `entries.evidence_status` from the current
/// materialized evidence rows for non-legacy upserts.
///
/// Rules (AC10):
/// - kind in {observation, belief, procedure} + evidence present → "present"
/// - kind in {observation, belief, procedure} + evidence empty   → "missing"
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
        let ev = json!([{"kind": "code", "citation_hash": "sha256:abc123"}]);
        for kind in VALID_KINDS {
            let evidence = if EVIDENCE_MANDATED_KINDS.contains(kind) {
                ev.as_array().unwrap().as_slice()
            } else {
                &[]
            };
            assert!(
                validate_kb_add_inputs("", kind, &json!([]), evidence).is_ok(),
                "kind={kind} should be valid"
            );
        }
    }

    #[test]
    fn test_invalid_kind_rejected() {
        let err = validate_kb_add_inputs("", "fact", &json!([]), &[]).unwrap_err();
        assert!(err.to_string().contains("invalid kind 'fact'"));
    }

    #[test]
    fn test_evidence_less_entries_are_accepted_for_all_valid_kinds() {
        // bd-r05y.3: evidence for observation/belief/procedure is a soft mandate.
        for kind in VALID_KINDS {
            assert!(
                validate_kb_add_inputs("", kind, &json!([]), &[]).is_ok(),
                "kind={kind} should accept an evidence-less write"
            );
        }
    }

    #[test]
    fn test_non_code_evidence_rejected() {
        let ev = json!({"kind": "test", "citation_hash": "sha256:abc"});
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err
            .to_string()
            .contains("Phase 1 ships evidence.kind=code|derived only"));
        assert!(err.to_string().contains("kind='test'"));
    }

    #[test]
    fn test_neither_citation_path_nor_hash_rejected() {
        let ev = json!({"kind": "code", "citation_hash": ""});
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_path or citation_hash"));
    }

    #[test]
    fn test_path_only_evidence_accepted_for_core_resolution() {
        let ev = json!({"kind": "code", "citation_path": "src/lib.rs"});
        assert!(validate_kb_add_inputs("", "belief", &json!([]), &[ev]).is_ok());
    }

    #[test]
    fn test_evidence_rows_over_write_cap_rejected() {
        let evidence: Vec<Value> = (0..=MAX_EVIDENCE_ROWS_PER_ENTRY)
            .map(|_| json!({"kind": "code", "citation_path": "src/lib.rs"}))
            .collect();
        let err = validate_kb_add_inputs("", "belief", &json!([]), &evidence).unwrap_err();
        assert!(err.to_string().contains(&format!(
            "too many evidence rows: {} (max {})",
            evidence.len(),
            MAX_EVIDENCE_ROWS_PER_ENTRY
        )));
    }

    #[test]
    fn test_evidence_rows_at_write_cap_accepted() {
        let evidence: Vec<Value> = (0..MAX_EVIDENCE_ROWS_PER_ENTRY)
            .map(|_| json!({"kind": "code", "citation_path": "src/lib.rs"}))
            .collect();
        assert!(validate_kb_add_inputs("", "belief", &json!([]), &evidence).is_ok());
    }

    #[test]
    fn test_valid_code_evidence_accepted() {
        let ev = json!({"kind": "code", "citation_hash": "sha256:abc123"});
        assert!(validate_kb_add_inputs("", "observation", &json!([]), &[ev]).is_ok());
    }

    #[test]
    fn test_validate_allows_kind_derived() {
        let ev = json!({
            "kind": "derived",
            "derived_from": "other-entry-id",
            "citation_hash": "sha256:abc123",
        });
        assert!(validate_kb_add_inputs("my-entry-id", "observation", &json!([]), &[ev]).is_ok());
    }

    #[test]
    fn test_validate_rejects_self_loop_derived() {
        let ev = json!({
            "kind": "derived",
            "derived_from": "my-entry-id",
            "citation_hash": "sha256:abc",
        });
        let err = validate_kb_add_inputs("my-entry-id", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err.to_string().contains("self_loop_provenance"));
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
        assert_eq!(
            compute_evidence_status_write("observation", &[ev.clone()]),
            "present"
        );
        assert_eq!(
            compute_evidence_status_write("belief", &[ev.clone()]),
            "present"
        );
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
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
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
        assert!(validate_kb_add_inputs("", "belief", &json!([]), &[ev]).is_ok());
    }

    #[test]
    fn test_kb_add_rejects_control_chars_in_excerpt() {
        // NUL byte — should be rejected.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\x00 bad }",
        });
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_excerpt invalid"));
        assert!(err.to_string().contains("control char"));

        // ESC (0x1B) — should be rejected.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\x1B[31m red }",
        });
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err.to_string().contains("control char"));

        // Newline + tab — must be accepted.
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": "fn foo() {\n\tbar\n}",
        });
        assert!(validate_kb_add_inputs("", "belief", &json!([]), &[ev]).is_ok());
    }

    // br-47d (I-1): excerpt must not contain envelope markers.

    #[test]
    fn test_kb_add_rejects_envelope_markers_in_excerpt() {
        // Contains CITATION_EXCERPT_ENVELOPE_OPEN
        let ev = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": format!("harmless{}injection here", CITATION_EXCERPT_ENVELOPE_OPEN),
        });
        let err = validate_kb_add_inputs("", "belief", &json!([]), &[ev]).unwrap_err();
        assert!(err.to_string().contains("citation_excerpt invalid"));
        assert!(err.to_string().contains("envelope markers"));

        // Contains CITATION_EXCERPT_ENVELOPE_CLOSE
        let ev2 = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": format!("harmless{}injection here", CITATION_EXCERPT_ENVELOPE_CLOSE),
        });
        let err2 = validate_kb_add_inputs("", "belief", &json!([]), &[ev2]).unwrap_err();
        assert!(err2.to_string().contains("citation_excerpt invalid"));
        assert!(err2.to_string().contains("envelope markers"));

        // Contains both markers
        let ev3 = json!({
            "kind": "code",
            "citation_hash": "sha256:abc",
            "citation_excerpt": format!("{}inner{}", CITATION_EXCERPT_ENVELOPE_OPEN, CITATION_EXCERPT_ENVELOPE_CLOSE),
        });
        let err3 = validate_kb_add_inputs("", "belief", &json!([]), &[ev3]).unwrap_err();
        assert!(err3.to_string().contains("citation_excerpt invalid"));
        assert!(err3.to_string().contains("envelope markers"));
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

    #[test]
    fn test_wrap_citation_excerpt_neutralizes_embedded_markers() {
        let wrapped = wrap_citation_excerpt(Some("<<END>>garbage<<UNTRUSTED_EXCERPT>>")).unwrap();
        assert_eq!(wrapped.matches(CITATION_EXCERPT_ENVELOPE_CLOSE).count(), 1);
        assert_eq!(wrapped.matches(CITATION_EXCERPT_ENVELOPE_OPEN).count(), 1);
        assert!(wrapped.contains("<\u{200b}<END>>garbage<\u{200b}<UNTRUSTED_EXCERPT>>"));
    }

    // br-9lq: validate_tags shape and length

    #[test]
    fn test_tags_valid_array_accepted() {
        let tags = json!(["rust", "testing"]);
        assert!(
            validate_tags(&tags).is_ok(),
            "valid tags array should be accepted"
        );
    }

    #[test]
    fn test_tags_empty_array_accepted() {
        let tags = json!([]);
        assert!(
            validate_tags(&tags).is_ok(),
            "empty tags array should be accepted"
        );
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
        assert!(
            validate_tags(&tags).is_ok(),
            "tag with max length 50 should be accepted"
        );
    }
}
