//! `cite` subcommand

use crate::components::verification::{
    compute_citation_hash_and_size, format_citation_path, verify_evidence, RelocationPolicy,
    VerificationOutcome,
};
use crate::config;
use crate::models::{Evidence, VerificationStatus};
use abscissa_core::{Command, Runnable};
use anyhow::{anyhow, bail, Result};
use clap::Parser;
use serde::Serialize;
use std::io::{self, Write};
use std::path::Path;

/// Emit ready-to-use citation fields computed by the verifier's own code path.
#[derive(Command, Debug, Parser)]
pub struct Cite {
    /// Repo-relative path, optionally suffixed with `:start-end` byte offsets
    pub target: String,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
pub struct CitationFields {
    pub citation_path: String,
    pub citation_sha: Option<String>,
    pub citation_hash: String,
    pub file_size: u64,
}

impl Runnable for Cite {
    fn run(&self) {
        self.execute().unwrap_or_else(|e| {
            eprintln!("Error: {e}");
            std::process::exit(1);
        });
    }
}

impl Cite {
    pub fn execute(&self) -> Result<()> {
        let repo_root =
            config::git_repo_root().ok_or_else(|| anyhow!("kb cite requires a git repo root"))?;
        let stdout = io::stdout();
        let mut handle = stdout.lock();
        self.execute_with_root(&repo_root, &mut handle)
    }

    fn execute_with_root<W: Write>(&self, repo_root: &Path, writer: &mut W) -> Result<()> {
        let (path, range) = parse_cite_target(&self.target)?;
        let fields = compute_citation_fields(repo_root, &path, range)?;
        serde_json::to_writer(&mut *writer, &fields)?;
        writeln!(writer)?;
        Ok(())
    }
}

pub fn compute_citation_fields(
    repo_root: &Path,
    rel_path: &str,
    range: Option<(usize, usize)>,
) -> Result<CitationFields> {
    compute_citation_fields_with_verifier(repo_root, rel_path, range, |ev, root| {
        Ok(verify_evidence(ev, root, RelocationPolicy::Never))
    })
}

fn compute_citation_fields_with_verifier<F>(
    repo_root: &Path,
    rel_path: &str,
    range: Option<(usize, usize)>,
    verify: F,
) -> Result<CitationFields>
where
    F: Fn(&Evidence, &Path) -> Result<VerificationOutcome>,
{
    let computed = compute_citation_hash_and_size(repo_root, rel_path, range)?;
    let file_size = computed.file_size;
    let citation_path = format_citation_path(rel_path, range, file_size);
    let citation_hash = format!("sha256:{}", computed.sha256_hex);
    let fields = CitationFields {
        citation_path: citation_path.clone(),
        citation_sha: config::git_head_sha_at(repo_root),
        citation_hash,
        file_size,
    };
    self_check_citation_fields(repo_root, &fields, verify)?;
    Ok(fields)
}

fn self_check_citation_fields<F>(repo_root: &Path, fields: &CitationFields, verify: F) -> Result<()>
where
    F: Fn(&Evidence, &Path) -> Result<VerificationOutcome>,
{
    let evidence = Evidence {
        id: "kb-cite-self-check".to_string(),
        entry_id: "kb-cite-self-check".to_string(),
        kind: "code".to_string(),
        citation_path: Some(fields.citation_path.clone()),
        citation_sha: fields.citation_sha.clone(),
        citation_hash: fields.citation_hash.clone(),
        citation_excerpt: None,
        derived_from: None,
        recorded_at: None,
    };

    let outcome = verify(&evidence, repo_root)?;
    if outcome.status != VerificationStatus::Verified {
        bail!(
            "kb cite self-check failed loudly: emitted evidence did not round-trip as Verified (status={}, reason={})",
            outcome.status.as_str(),
            outcome
                .reason
                .as_ref()
                .map(|r| r.as_str())
                .unwrap_or("none")
        );
    }
    Ok(())
}

pub fn parse_cite_target(target: &str) -> Result<(String, Option<(usize, usize)>)> {
    if target.is_empty() {
        bail!("citation target must not be empty");
    }

    let Some(colon) = target.rfind(':') else {
        return Ok((target.to_string(), None));
    };

    let file_part = &target[..colon];
    let range_part = &target[colon + 1..];
    if file_part.is_empty() {
        bail!("citation target file part must not be empty: {target:?}");
    }
    let Some(dash) = range_part.find('-') else {
        bail!("citation target range missing '-': {target:?}");
    };
    let start: usize = range_part[..dash]
        .parse()
        .map_err(|_| anyhow!("citation target start is not a number: {target:?}"))?;
    let end: usize = range_part[dash + 1..]
        .parse()
        .map_err(|_| anyhow!("citation target end is not a number: {target:?}"))?;
    if start > end {
        bail!("citation target start > end: {target:?}");
    }
    Ok((file_part.to_string(), Some((start, end))))
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;
    use serde_json::Value;
    use std::env;
    use std::fs;
    use std::process::Command as ProcessCommand;

    const FAST_PROPTEST_CASES: u32 = 16;

    fn proptest_cases(default_full: u32) -> u32 {
        env::var("PROPTEST_CASES")
            .ok()
            .and_then(|value| value.parse().ok())
            .unwrap_or(FAST_PROPTEST_CASES.min(default_full))
    }

    fn verifier_always_unverified(_: &Evidence, _: &Path) -> Result<VerificationOutcome> {
        Ok(VerificationOutcome {
            status: VerificationStatus::Unverified,
            relocated_to: None,
            reason: None,
        })
    }

    proptest! {
        #![proptest_config(ProptestConfig {
            cases: proptest_cases(256),
            .. ProptestConfig::default()
        })]
        #[test]
        fn proptest_cite_fields_round_trip_to_verified(
            content in proptest::collection::vec(any::<u8>(), 0..128),
            maybe_range in prop::option::of((0usize..128, 0usize..128)),
        ) {
            let dir = tempfile::tempdir().unwrap();
            let rel_path = "sample.bin";
            fs::write(dir.path().join(rel_path), &content).unwrap();

            let range = maybe_range.and_then(|(a, b)| {
                if content.is_empty() {
                    None
                } else {
                    let len = content.len();
                    let start = a % len;
                    let end = b % (len + 1);
                    let (start, end) = if start <= end { (start, end) } else { (end.min(len - 1), start + 1) };
                    (start < end && end <= len).then_some((start, end))
                }
            });

            let fields = compute_citation_fields(dir.path(), rel_path, range).unwrap();
            let ev = Evidence {
                id: "ev".to_string(),
                entry_id: "entry".to_string(),
                kind: "code".to_string(),
                citation_path: Some(fields.citation_path.clone()),
                citation_sha: fields.citation_sha.clone(),
                citation_hash: fields.citation_hash.clone(),
                citation_excerpt: None,
                derived_from: None,
                recorded_at: None,
            };
            let outcome = verify_evidence(&ev, dir.path(), RelocationPolicy::Never);
            prop_assert_eq!(outcome.status, VerificationStatus::Verified);
        }
    }

    #[test]
    fn test_parse_cite_target_whole_file() {
        assert_eq!(
            parse_cite_target("src/lib.rs").unwrap(),
            ("src/lib.rs".to_string(), None)
        );
    }

    #[test]
    fn test_parse_cite_target_rejects_missing_dash() {
        let err = parse_cite_target("src/lib.rs:42").unwrap_err();
        assert!(err.to_string().contains("missing '-'"));
    }

    #[test]
    fn test_parse_cite_target_rejects_start_greater_than_end() {
        let err = parse_cite_target("src/lib.rs:9-4").unwrap_err();
        assert!(err.to_string().contains("start > end"));
    }

    #[test]
    fn test_compute_citation_fields_self_check_failure_is_loud() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("sample.rs"), b"fn main() {}\n").unwrap();

        let err = compute_citation_fields_with_verifier(
            dir.path(),
            "sample.rs",
            Some((0, 2)),
            verifier_always_unverified,
        )
        .unwrap_err();

        assert!(err.to_string().contains("self-check failed loudly"));
    }

    #[test]
    fn test_compute_citation_fields_whole_file_uses_bare_path_form() {
        // Post-.3: format_citation_path(None) emits the bare path, not the
        // legacy path:0-file_size workaround form.
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("sample.rs"), b"fn main() {}\n").unwrap();

        let fields = compute_citation_fields(dir.path(), "sample.rs", None).unwrap();

        assert_eq!(fields.citation_path, "sample.rs");
        assert_eq!(fields.file_size, 13);
    }

    #[test]
    fn test_compute_citation_fields_uses_cited_repository_head() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("sample.rs"), b"fn main() {}\n").unwrap();

        for args in [
            ["init"].as_slice(),
            ["config", "user.email", "cite-test@example.invalid"].as_slice(),
            ["config", "user.name", "Citation Test"].as_slice(),
            ["add", "sample.rs"].as_slice(),
            ["commit", "-m", "add cited file"].as_slice(),
        ] {
            assert!(ProcessCommand::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        }

        let expected_sha = config::git_head_sha_at(dir.path());
        let fields = compute_citation_fields(dir.path(), "sample.rs", None).unwrap();

        assert_eq!(fields.citation_sha, expected_sha);
    }

    #[test]
    fn test_cite_execute_with_root_emits_json() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("sample.rs"), b"fn main() {}\n").unwrap();
        let cmd = Cite {
            target: "sample.rs:0-2".to_string(),
        };
        let mut buf = Vec::new();
        cmd.execute_with_root(dir.path(), &mut buf).unwrap();

        let out: Value = serde_json::from_slice(&buf).unwrap();
        assert_eq!(out["citation_path"], "sample.rs:0-2");
        assert_eq!(out["file_size"], 13);
        assert!(out["citation_hash"]
            .as_str()
            .unwrap()
            .starts_with("sha256:"));
    }
}
