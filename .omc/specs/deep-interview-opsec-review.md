# Deep Interview Spec: opsec-review cold skill

## Metadata
- Interview ID: di-opsec-review-20260529
- Rounds: 9
- Final Ambiguity Score: 18%
- Type: brownfield (machines_conf skill system)
- Generated: 2026-05-29
- Threshold: 0.20
- Threshold Source: default
- Initial Context Summarized: no
- Status: PASSED

## Clarity Breakdown
| Dimension | Score | Weight | Weighted |
|-----------|-------|--------|----------|
| Goal Clarity | 0.85 | 35% | 0.298 |
| Constraint Clarity | 0.90 | 25% | 0.225 |
| Success Criteria | 0.70 | 25% | 0.175 |
| Context Clarity | 0.80 | 15% | 0.120 |
| **Total Clarity** | | | **0.818** |
| **Ambiguity** | | | **18%** |

## Topology
| Component | Status | Description | Coverage |
|-----------|--------|-------------|----------|
| Commit identity audit | active | Scan full git history + unpublished commits for real name/email from ~/.gitconfig in commit authorship | All commits: `git log --all --format='%ae|%an'` matched against gitconfig identity |
| File content scan | active | Scan tracked files and history for identity strings (name, email, hostname) + credential patterns via gitleaks | Identity strings via grep; credentials delegated to gitleaks |
| Binary & metadata artifacts | active | Detect identity-revealing metadata in images (EXIF), compiled binaries (embedded paths), and docs (PDF/docx author fields) | mat2 for inspection; checked file types: JPEG/PNG/WEBP, binaries, PDF, docx |
| Remediation guidance | active | For each finding: emit specific fix command (BFG for history rewrites, mat2 for metadata, gitleaks for creds) | Report + recommend; agent signs off each finding; no auto-apply |

## Goal
Audit a git repository for personal identity leaks and credentials that could deanonymize a pseudonymous open-source contributor, covering the full history and any unpublished commits. For each finding, emit a severity-labeled entry and a specific remediation command. The agent reviews and signs off every finding (resolved or accepted-as-risk) before the repo is made public.

## Constraints
- **Identity source**: read `user.name`, `user.email`, `hostname` from `~/.gitconfig` and system hostname at scan time; these are the "real identity" signals to match against
- **Scan depth**: both full history (`git log --all`) and working tree / staged / unpublished commits
- **Credential scanner**: delegate to `gitleaks` (must be in PATH; skill emits an error if missing)
- **gitleaks availability**: add `gitleaks` to `homes/urist.nix` as part of this skill's deployment
- **File metadata scanner**: `mat2` (must be in PATH)
- **History rewriter**: BFG Repo Cleaner for commit identity rewrites
- **Binary artifact types**: JPEG/PNG/WEBP (EXIF), compiled binaries with embedded debug paths, PDF, docx
- **Timezone/timing analysis**: out of scope (too noisy)
- **Credential scanning**: in scope — both identity strings AND credential patterns covered in one skill
- **Credential integration**: delegate to gitleaks, not inline regex

## Non-Goals
- Jupyter notebook metadata scanning (out of scope)
- Automatic fix application (skill recommends commands; agent executes with approval)
- Credential scanning without gitleaks (skill fails loudly rather than implementing fallback regex)
- Timezone/commit timing deanonymization analysis
- Scanning repos not under git version control

## Acceptance Criteria
- [ ] Skill reads `user.name`, `user.email`, and hostname from system at invocation time
- [ ] Skill scans `git log --all --format='%ae|%an|%H'` and flags any commit whose author name or email matches the real identity
- [ ] Skill scans staged files and working tree for identity strings (name, email, hostname) via grep
- [ ] Skill invokes `gitleaks detect --source . --no-git=false` (or equivalent) and includes its findings in the report; emits a clear error if gitleaks not in PATH
- [ ] Skill runs `mat2 --show <file>` on all JPEG/PNG/WEBP, PDF, and docx files in the repo and flags any that contain non-empty metadata fields
- [ ] Skill scans tracked binary files for embedded real paths (`/home/<realname>/` substrings)
- [ ] Every finding has a severity label: HIGH (real name/email in commit author, credential leak) or MEDIUM (personal path in artifact, metadata in doc)
- [ ] Every finding includes a specific, copy-pasteable remediation command (BFG, mat2, or gitleaks fix guidance)
- [ ] Skill outputs a signed-off checklist format: each finding is a `[ ]` item the agent marks resolved or accepted-as-risk
- [ ] Skill is registered in `machines_conf/lib/fb-hm/claude-plugins/assets.nix` as a cold skill with tier "cold"
- [ ] `gitleaks` is added to `homes/urist.nix` packages
- [ ] Skill triggers on keywords: `opsec-review`, `opsec review`, `make repo public`, `pseudonym check`

## Assumptions Exposed & Resolved
| Assumption | Challenge | Resolution |
|------------|-----------|------------|
| Scan depth | Full history vs. forward-looking only | Both: full history for existing leaks + unpublished for forward guard |
| Identity source | How does the skill know what "real" looks like? | Read from `~/.gitconfig` at scan time |
| Remediation model | Report only vs. automated fixes | Report + specific fix commands; agent signs off each finding |
| Binary artifacts | EXIF/metadata is an assumed risk vs. concrete | Confirmed concrete risk; kept in scope |
| Credential scanning | Identity-only vs. credentials included | Both in scope; delegate to gitleaks |
| History rewrite tool | BFG vs. git filter-repo | BFG |
| Metadata scrub tool | mat2 vs. exiftool | mat2 |
| Timing/timezone analysis | In scope or too noisy? | Out of scope |

## Technical Context
**Skill conventions (from brownfield exploration):**
- Single directory: `lib/fb-hm/skills/opsec-review/`
- One `SKILL.md` with YAML frontmatter (`name`, `description`, `requires`, `triggers`, `tags`)
- All workflow logic inline in markdown — no external script files
- bash + git + core utils only; tools invoked via PATH
- Registered in `lib/fb-hm/claude-plugins/assets.nix` via `mkSkillFiles`
- Cold skills discovered at session start via `cold-skill-hint` hook

**New NixOS dependency:**
- `homes/urist.nix`: add `pkgs.gitleaks` to user packages
- `homes/urist.nix`: `mat2` already assumed in PATH (confirm or add)

**Related existing skills:**
- `pentest/` — security engagement planning (different threat model)
- `secret-add/` — agenix secret creation (credential storage, not scanning)
- `threat-model/` — STRIDE modeling (design-time; this skill is repo-time)

## Ontology (Key Entities)
| Entity | Type | Fields | Relationships |
|--------|------|--------|---------------|
| Repository | core domain | path, remote_url, branch, is_public | contains Commits, TrackedFiles |
| Commit | core domain | hash, author_name, author_email, timestamp, message | belongs to Repository; may trigger Finding |
| IdentityProfile | supporting | real_name, real_email, hostname | derived from ~/.gitconfig; used by all scan components |
| TrackedFile | supporting | path, type (text/binary/image/doc), size | belongs to Repository; may trigger Finding |
| Finding | core domain | severity (HIGH/MEDIUM), component, description, evidence, fix_command | produced by scan; signed off by agent |
| ArtifactType | supporting | name, scanner_tool, patterns | categorizes TrackedFile for binary/metadata scan |
| Fix | supporting | tool (BFG/mat2/gitleaks), command, manual_steps | attached to Finding |

## Ontology Convergence
| Round | Entity Count | New | Changed | Stable | Stability Ratio |
|-------|-------------|-----|---------|--------|----------------|
| 1 | 3 | 3 | — | — | N/A |
| 2 | 5 | 2 | 0 | 3 | 60% |
| 3 | 6 | 1 | 0 | 5 | 83% |
| 4 | 6 | 0 | 0 | 6 | 100% |
| 5 | 7 | 1 | 0 | 6 | 86% |
| 6–9 | 7 | 0 | 0 | 7 | 100% |

## Interview Transcript
<details>
<summary>Full Q&A (9 rounds)</summary>

### Round 0 (Topology)
**Q:** Topology confirmation — 4 components proposed
**A:** Looks right

### Round 1
**Q:** Does the skill need to audit the full git history, or only the current/forward-looking state?
**A:** Both — audit full history AND flag unpublished (severity tiers: historical vs unpublished)
**Ambiguity:** 72%

### Round 2
**Q:** When the skill finds an issue, what should it do? (report only / report+recommend / report+automate)
**A:** Report + recommend — emit findings WITH specific fix commands
**Ambiguity:** 68%

### Round 3
**Q:** How does the skill know what your real identity looks like?
**A:** Derived — read from global git config at scan time
**Ambiguity:** 61%

### Round 4 (Contrarian)
**Q:** What if binary/metadata artifact scanning isn't necessary? Is it a concrete or assumed risk?
**A:** Concrete risk — keep it in scope
**Ambiguity:** 59%

### Round 5
**Q:** Which artifact types should the binary/metadata scan cover?
**A:** Images (JPEG/PNG/WEBP), compiled binaries, generated/exported docs (PDF/docx)
**Ambiguity:** 51%

### Round 6 (Simplifier)
**Q:** What's the simplest definition of PASS that gives enough confidence to make the repo public?
**A:** Signed-off checklist — agent reviews each finding and explicitly marks it resolved or accepted
**Ambiguity:** 35%

### Round 7
**Q:** Which tools — metadata scrub + history rewriter?
**A:** mat2 + BFG Repo Cleaner
**Ambiguity:** 30%

### Round 8
**Q:** Identity strings only, or also credentials in content scan?
**A:** Both in scope — one skill covering identity AND credentials
**Ambiguity:** 24%

### Round 9
**Q:** Credential scanning approach + timezone/timing analysis?
**A:** Delegate to gitleaks (add to homes/urist.nix); timezone analysis not mentioned (out of scope)
**Ambiguity:** 18% ✅

</details>
