# Plan: opsec-review cold skill

**Status:** pending approval
**Slug:** opsec-review
**Spec:** .omc/specs/deep-interview-opsec-review.md (18% ambiguity)
**Repo:** machines_conf

---

## ADR

**Decision:** Single-pass cold skill (SKILL.md + bash + PATH tools) with 4 sequential scan components, read-only detection, advisory remediation, machine-greppable output.

**Drivers:**
- Pseudonymous OSS contributors need a reliable pre-publication identity-leak audit
- Must integrate into existing machines_conf cold-skill system (skills.nix, homes/urist.nix)
- Detection must never mutate state; remediation must require explicit agent action

**Alternatives considered:**
- Subcommand model (detect/remediate split): rejected — adds invocation complexity with no gain for the typical single-pass use case; the dry-run contract is satisfied structurally
- Inline credential regex instead of gitleaks: rejected — gitleaks has a maintained, comprehensive ruleset; reimplementing it would be incomplete and harder to update

**Consequences:**
- Two new PATH tools required (gitleaks, mat2); both added to homes/urist.nix
- git-filter-repo preferred over BFG (BFG unmaintained since 2023); skill notes both
- Registration as cold skill only (not hot) — on-demand, not loaded at every session start

**Follow-ups:**
- Consider a fixture repo under the skill dir for regression testing
- Consider adding OPSEC-PASS/FAIL output to a future pre-publish hook

---

## RALPLAN-DR

**Principles:**
1. Read-only detection — skill never mutates repository state
2. Identity signals from ~/.gitconfig + hostname — no per-project config
3. Machine-greppable output (OPSEC-FAIL/WARN/PASS prefixes) + signed-off checklist
4. Remediation advisory-only — commands shown, never auto-executed
5. Commit identity audit first — highest signal, irreversible after public push

**Decision Drivers:**
1. Single SKILL.md + bash + PATH tools (existing skill pattern)
2. gitleaks + mat2 not in homes/urist.nix — both must be added
3. localSkills in skills.nix is the registration point; cold by default (not in hotSkillNames)

---

## Task Flow

### 1. Create lib/fb-hm/skills/opsec-review/SKILL.md

New file. Content:

```markdown
---
name: opsec-review
description: Pre-publication identity-leak audit for pseudonymous open-source repos — scans commit authorship, file content, credential history, and binary metadata
requires: oh-my-claudecode
triggers:
  - "opsec-review"
  - "opsec review"
  - "make repo public"
  - "pseudonym check"
  - "identity audit"
tags:
  - security
  - opsec
  - privacy
  - pseudonymity
---

# /opsec-review Skill

Audit a git repository for personal identity leaks and credentials before making it public
under a pseudonym. Produces a signed-off checklist: each finding is a `[ ]` item the agent
marks resolved or accepted-as-risk.

**Detection is read-only — this skill never rewrites history or modifies files.**
Remediation commands are shown but never auto-executed.

Output format:
- `OPSEC-FAIL: [HIGH]` — must be resolved before publishing
- `OPSEC-WARN: [MEDIUM]` — advisory; resolve or accept-as-risk with rationale
- `OPSEC-PASS:` — component clear

## Prerequisites

- Must run inside a git repo (`git rev-parse --show-toplevel` must succeed)
- `gitleaks` in PATH (add `pkgs.gitleaks` to `homes/urist.nix` if missing)
- `mat2` in PATH (add `pkgs.python3Packages.mat2` to `homes/urist.nix` if missing)

## Pipeline

### Step 0: Load Identity Profile

```bash
# Identity is set per-repo (intentional workflow — no global git config).
# Caller must provide real identity explicitly via env vars.
REAL_NAME="${OPSEC_REAL_NAME:-}"
REAL_EMAIL="${OPSEC_REAL_EMAIL:-}"
REAL_HOSTNAME=$(hostname 2>/dev/null || echo "")
# System username for binary path scanning (/home/USERNAME/)
REAL_USERNAME=$(id -un 2>/dev/null || echo "$USER")

if [[ -z "$REAL_NAME" && -z "$REAL_EMAIL" ]]; then
  echo "ERROR: Real identity not provided. Set env vars before running:"
  echo "  OPSEC_REAL_NAME='Your Real Name' OPSEC_REAL_EMAIL='real@email.com' /opsec-review"
  exit 1
fi
echo "Identity profile loaded: name='$REAL_NAME' email='$REAL_EMAIL' hostname='$REAL_HOSTNAME' username='$REAL_USERNAME'"
```

### Step 1: Commit Identity Audit  ← run first (irreversible after public push)

```bash
echo ""
echo "=== STEP 1: Commit Identity Audit ==="
COMMIT_FINDINGS=0

while IFS='|' read -r author_email author_name committer_email committer_name commit_hash; do
  MATCH=""
  [[ -n "$REAL_NAME"  && "$author_name"     == *"$REAL_NAME"*  ]] && MATCH="author_name:$author_name"
  [[ -n "$REAL_EMAIL" && "$author_email"    == *"$REAL_EMAIL"* ]] && MATCH="${MATCH:+$MATCH, }author_email:$author_email"
  # Check committer separately (rebases/cherry-picks can differ from author)
  [[ -n "$REAL_NAME"  && "$committer_name"  == *"$REAL_NAME"*  && "$committer_name"  != "$author_name"  ]] && MATCH="${MATCH:+$MATCH, }committer_name:$committer_name"
  [[ -n "$REAL_EMAIL" && "$committer_email" == *"$REAL_EMAIL"* && "$committer_email" != "$author_email" ]] && MATCH="${MATCH:+$MATCH, }committer_email:$committer_email"
  if [[ -n "$MATCH" ]]; then
    echo "OPSEC-FAIL: [HIGH] Commit $commit_hash — real identity in commit ($MATCH)"
    echo "  Remediation — DESTRUCTIVE, back up first:"
    echo "    Step 1 (backup): git clone --mirror . \"../\$(basename \"\$PWD\")-backup-\$(date +%Y%m%d-%H%M%S).git\""
    echo "    Step 2 (rewrite): git filter-repo --email-callback 'return email.replace(b\"$REAL_EMAIL\", b\"pseudo@example.com\")'"
    echo "    Step 3 (verify):  git log --all --format='%ae %an %ce %cn' | sort -u  # should show no real identity"
    echo "    Alternative (BFG): bfg --replace-text <patterns-file> && git reflog expire --expire=now --all && git gc --prune=now --aggressive"
    echo "  [ ] Resolved  [ ] Accepted-as-risk"
    COMMIT_FINDINGS=$((COMMIT_FINDINGS + 1))
  fi
done < <(git log --all --format='%ae|%an|%ce|%cn|%H' 2>/dev/null)

[[ $COMMIT_FINDINGS -eq 0 ]] && echo "OPSEC-PASS: Commit identity — no real identity found in any commit authorship"
```

### Step 2a: Identity Strings in Tracked Files

```bash
echo ""
echo "=== STEP 2a: Identity Strings in Tracked Files ==="
CONTENT_FINDINGS=0

for pattern in "$REAL_NAME" "$REAL_EMAIL" "$REAL_HOSTNAME"; do
  [[ -z "$pattern" ]] && continue
  while IFS=: read -r file line_num match; do
    [[ -z "$file" ]] && continue
    echo "OPSEC-FAIL: [HIGH] '$pattern' found in $file:$line_num"
    echo "  Match: $match"
    echo "  Remediation: edit $file to remove/replace, then if in history:"
    echo "    git filter-repo --path \"$file\" --force"
    echo "  [ ] Resolved  [ ] Accepted-as-risk"
    CONTENT_FINDINGS=$((CONTENT_FINDINGS + 1))
  done < <(git grep -In "$pattern" HEAD -- 2>/dev/null || true)
done

[[ $CONTENT_FINDINGS -eq 0 ]] && echo "OPSEC-PASS: Identity strings — no matches in tracked files (HEAD)"
```

### Step 2b: Credential Scan via gitleaks

```bash
echo ""
echo "=== STEP 2b: Credential Scan (gitleaks) ==="
if ! command -v gitleaks >/dev/null 2>&1; then
  echo "OPSEC-FAIL: [HIGH] gitleaks not in PATH — credential scan skipped"
  echo "  Fix: add pkgs.gitleaks to homes/urist.nix and run: home-manager switch"
  echo "  [ ] Resolved  [ ] Accepted-as-risk"
else
  GITLEAKS_TMP=$(mktemp /tmp/opsec-gitleaks-XXXXXX.json)
  gitleaks detect --source . --report-format json --report-path "$GITLEAKS_TMP" --no-banner 2>/dev/null || true
  GITLEAKS_COUNT=$(jq 'if type == "array" then length else 0 end' "$GITLEAKS_TMP" 2>/dev/null || echo "0")
  if [[ "$GITLEAKS_COUNT" -gt 0 ]]; then
    jq -r '.[] | "OPSEC-FAIL: [HIGH] Credential: \(.Description) in \(.File):\(.StartLine) (rule: \(.RuleID))\n  Commit: \(.Commit)\n  Remediation: git filter-repo --path \"\(.File)\" --force  # then rotate the credential\n  [ ] Resolved  [ ] Accepted-as-risk"' \
      "$GITLEAKS_TMP"
  else
    echo "OPSEC-PASS: Credentials — gitleaks found no leaks"
  fi
  rm -f "$GITLEAKS_TMP"
fi
```

### Step 3a: File Metadata via mat2

```bash
echo ""
echo "=== STEP 3a: File Metadata (mat2) ==="
if ! command -v mat2 >/dev/null 2>&1; then
  echo "OPSEC-WARN: [MEDIUM] mat2 not in PATH — metadata scan skipped"
  echo "  Fix: add pkgs.python3Packages.mat2 to homes/urist.nix"
  echo "  [ ] Resolved  [ ] Accepted-as-risk"
else
  META_FINDINGS=0
  while IFS= read -r file; do
    [[ -f "$file" ]] || continue
    META=$(mat2 --show "$file" 2>/dev/null \
      | grep -v '^$\|No metadata\|Unsupported\|Unknown' || true)
    if [[ -n "$META" ]]; then
      echo "OPSEC-WARN: [MEDIUM] Metadata present in $file"
      echo "$META" | sed 's/^/  /'
      echo "  Remediation: mat2 --inplace \"$file\"  # strips all metadata in place"
      echo "  [ ] Resolved  [ ] Accepted-as-risk"
      META_FINDINGS=$((META_FINDINGS + 1))
    fi
  done < <(git ls-files | grep -iE '\.(jpg|jpeg|png|webp|pdf|docx|doc|odt)$')
  [[ $META_FINDINGS -eq 0 ]] && echo "OPSEC-PASS: File metadata — no metadata in tracked image/doc files"
fi
```

### Step 3b: Embedded Paths in Compiled Binaries

```bash
echo ""
echo "=== STEP 3b: Embedded Paths in Binaries ==="
BINARY_FINDINGS=0

if [[ -n "$REAL_USERNAME" ]]; then
  while IFS= read -r binary; do
    [[ -f "$binary" ]] || continue
    if strings "$binary" 2>/dev/null | grep -qF "/home/$REAL_USERNAME/"; then
      echo "OPSEC-WARN: [MEDIUM] Embedded path '/home/$REAL_USERNAME/' in binary $binary"
      echo "  Remediation: remove binary from tracking:"
      echo "    git rm --cached \"$binary\" && echo '$binary' >> .gitignore && git commit -m 'chore: untrack binary with embedded paths'"
      echo "  [ ] Resolved  [ ] Accepted-as-risk"
      BINARY_FINDINGS=$((BINARY_FINDINGS + 1))
    fi
  done < <(git ls-files | while IFS= read -r f; do
    [[ -f "$f" ]] && file "$f" 2>/dev/null | grep -qE 'ELF|Mach-O|PE32' && echo "$f"
  done)
fi

[[ $BINARY_FINDINGS -eq 0 ]] && echo "OPSEC-PASS: Binary paths — no embedded real home path in tracked binaries"
```

### Step 4: Summary

```bash
echo ""
echo "================================================"
echo "OPSEC-REVIEW COMPLETE"
TOTAL_FAILS=$(grep -c "OPSEC-FAIL:" <<< "$OUTPUT" 2>/dev/null || echo "0")
TOTAL_WARNS=$(grep -c "OPSEC-WARN:" <<< "$OUTPUT" 2>/dev/null || echo "0")
echo "  HIGH findings: $TOTAL_FAILS"
echo "  MEDIUM findings: $TOTAL_WARNS"
echo ""
if [[ "$TOTAL_FAILS" -eq 0 && "$TOTAL_WARNS" -eq 0 ]]; then
  echo "OPSEC-PASS: All components clear — repo is safe to publish under pseudonym"
else
  echo "Review each [ ] finding above. Mark as:"
  echo "  [x] Resolved — fix applied and verified"
  echo "  [~] Accepted — acknowledged, accepted-as-risk"
  echo ""
  echo "Re-run /opsec-review after applying fixes to confirm clean pass."
fi
echo "================================================"
```

## Notes

- **Identity profile**: caller provides real identity via `OPSEC_REAL_NAME` and `OPSEC_REAL_EMAIL` env vars. No global git config is used — identity is intentionally set per-repo only, so there is no global source to read from. Hard fail if both are empty.
- **Preferred history rewrite tool**: `git filter-repo` (`pkgs.gitAndTools.git-filter-repo`). Maintained, Python-based, upstream-recommended over deprecated `git filter-branch`. BFG is shown as an alternative for familiarity.
- **After any history rewrite**: force-push is required. All forks must re-clone. Coordinate before making the repo public.
- **Related skills**: `secret-add` — rotate a leaked credential to OpenBao. `threat-model` — design-time identity exposure analysis.
```

### 2. Update lib/fb-hm/user-conf/skills.nix

Add to `localSkills` list (after `pentest` entry, before closing `]`):

```nix
{
  name = "opsec-review";
  path = ../skills/opsec-review;
}
```

Do NOT add to `hotSkillNames` — cold skill by default.

### 3. Update homes/urist.nix

Add to `home.packages` list:

```nix
pkgs.gitleaks
pkgs.python3Packages.mat2
pkgs.git-filter-repo
```

Group together near other privacy/security tools if any; otherwise add after existing entries.

---

## Acceptance Criteria (from spec)

- [ ] SKILL.md exists at `lib/fb-hm/skills/opsec-review/SKILL.md`
- [ ] Skill reads real identity from `OPSEC_REAL_NAME`/`OPSEC_REAL_EMAIL` env vars; hard fails with instructions if both are empty
- [ ] Step 1 scans `git log --all --format='%ae|%an|%H'` and flags author matches → HIGH
- [ ] Step 2a scans tracked files for identity strings via `git grep`
- [ ] Step 2b invokes gitleaks; emits clear error if not in PATH
- [ ] Step 3a runs `mat2 --show` on image/doc files; flags non-empty metadata → MEDIUM
- [ ] Step 3b greps compiled binaries for embedded `/home/$REAL_NAME/` paths → MEDIUM
- [ ] Every finding has severity label (HIGH/MEDIUM) and copy-pasteable remediation command
- [ ] Output uses OPSEC-FAIL/WARN/PASS prefixes
- [ ] `opsec-review` entry in `localSkills` in `skills.nix`
- [ ] `pkgs.gitleaks` in `homes/urist.nix`
- [ ] `pkgs.python3Packages.mat2` in `homes/urist.nix` (if not already present)
- [ ] `pkgs.git-filter-repo` in `homes/urist.nix`
- [ ] Skill is discoverable as cold skill after `home-manager switch`
