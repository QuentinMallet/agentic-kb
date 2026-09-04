# Decision: citation_path range semantics — optional range, byte offsets, verify fold

**Date:** 2026-09-02
**Epic:** bd-cnh8 — citation-range-semantics (ADR-3 follow-up)
**Spec task:** bd-cnh8.2
**Upstream ADR:** `.state/.omc/plans/evidence-storage-integrity.md` §ADR-3
**Gates:** bd-cnh8.1 (kb-cite tool), bd-cnh8.3 (parser + fold), bd-cnh8.4 (size band)
**Code at decision time:** HEAD `12f44d5`, `src/components/verification.rs`

ADR-3 fixed the *direction* (range becomes optional; byte-offset semantics retained for explicit
ranges; line-number reinterpretation rejected outright) and deferred the *contract*. This note is
that contract. Everything below is binding on .1, .3, .4 and informative for .5 (migration) and
.7 (docs).

---

## D1 — Parse contract

**Rule.**

```
citation_path := <file>                       whole file
              | <file> ":" <start> "-" <end>  byte range, 0 <= start < end
```

The discriminator is the **last** colon (`rfind(':')`, as today):

- **No colon anywhere** ⇒ whole-file citation. Hash is `sha256(all bytes of the file)`.
- **Colon present** ⇒ everything after the last colon MUST parse as `<digits>-<digits>` with
  `start < end`. Anything else is a hard rejection — never a silent fallback to whole-file.

The no-fallback half is the entire point. If a typo'd range degraded into a whole-file citation,
`src/foo.rs:42-5B` would be cheerfully verified against the wrong bytes and report `Verified`.
A malformed range must stay loud.

### Edge cases, decided

**Filenames containing `:`.** Today `parse_citation_path("a:b.rs")` is `Err`
(`rfind(':')` splits to `("a", "b.rs")`, then `range missing '-'`). Under D1 it stays a rejection —
colon present, suffix does not parse. **Rejected alternative:** "colon with non-numeric suffix is
part of the filename ⇒ whole file". That reading is *not* backward compatible in the direction that
matters: it converts today's loud `Err` into a silent whole-file verification, which is exactly the
failure mode D1 exists to prevent, and it is undecidable in general (`a:1-2` is a legal filename).

The escape hatch already works and needs no code: `rfind` takes the *last* colon, so a
colon-bearing file is citable with an explicit range — `weird:name.rs:0-42` parses to
`("weird:name.rs", 0, 42)`. Only the *bare whole-file* form is unavailable for such files. On the
current corpus that costs nothing (census below: zero colon-bearing paths), and `:` is illegal in
filenames on Windows/NTFS anyway.

**Empty `file` part.** `citation_path = ""` (or `":0-4"`) is a rejection. Without this rule the
optional-range change turns `Some("")` — previously an `Err` — into a whole-file citation of the
repo root, and `safe_join(root, "")` canonicalizes to the root directory, which `File::open`
*succeeds* on under Linux. Reject at parse; do not let it reach `hash_check_at_citation`.

**Non-regular files.** The whole-file form newly makes `citation_path = "src"` syntactically valid
where it was an `Err` before. `File::open` on a directory succeeds on Linux and the read then fails
with an opaque `ReadError`. `hash_check_at_citation` must add a `metadata.is_file()` check and
report `FileMissing` for anything that is not a regular file. This is a new surface opened by D1,
not a pre-existing bug.

**Empty file (0 bytes), whole-file form.** Legal and meaningful. Hashes to
`e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855` (sha256 of the empty string).
It is a real assertion: if the file later gains any byte, the whole-file hash changes and the row
goes `HashMismatch`. No special case in code.

**Empty range `N-N`.** **Rejected** — `start < end` is required, not `start <= end`
(today's parser only rejects `start > end`). An `N-N` citation hashes zero bytes, so it asserts
nothing: it reports `Verified` against any file of at least `N` bytes, including one whose entire
content has been replaced. That is a silent false-verify. Corpus impact is zero (census below), and
under D1 there is now a correct way to say "this whole file" — the bare path — so no legitimate use
for `N-N` remains.

The asymmetry with the empty-file case is deliberate and is the point: whole-file-of-an-empty-file
asserts *"this file is still empty"* and breaks when that stops being true; `N-N` asserts nothing
and never breaks.

**Corpus census** (`.state/agent-kb/agent-kb-events.jsonl`, 7 citation-bearing evidence rows,
2026-09-02):

| citation_path | rows |
|---|---|
| `src/commands/compact.rs:0-38237` | 4 |
| `src/components/db.rs:0-165384` | 2 |
| `.state/agent-kb/agent-kb-events.jsonl:0-21041` | 1 |

All three are `path:0-<size>` whole-file workarounds — the exact shape bd-cnh8.5 heals. Zero
`N-N` ranges, zero colon-bearing filenames, zero bare paths. Every D1 edge rule above is therefore
corpus-neutral. Note for .5: the third row cites the *event log itself*, which grows on every
`kb_add`, so it is unhealable by hash and must be dropped or re-cited rather than rewritten.

---

## D2 — Verify fold contract

**Rule.** `verify_evidence` loses its `Result` entirely:

```rust
pub fn verify_evidence(
    ev: &Evidence,
    repo_root: &Path,
    policy: RelocationPolicy,
) -> VerificationOutcome
```

**Which conditions become `Unverified` vs stay `Err`:** *none stay `Err`*. There is no error
channel left. The only condition that reached `Err` today — a malformed `citation_path` — becomes
a new reason variant:

```rust
UnverifiedReason::MalformedCitationPath  // as_str() => "malformed_citation"
```

**The wire spelling is `"malformed_citation"`, not `"malformed_citation_path"`**, deliberately:
that is the exact string `stale_check.rs:540` already synthesizes today, so the stale-check
JSON/CLI surface is byte-identical before and after. The fold moves; the output does not.

A genuine programming bug should panic, not thread an `Err` through 16 call sites. The doc comment
on `verify_evidence` ("Returns `Err` only for a malformed `citation_path` (a programming bug, not
a runtime condition)") had it backwards — a malformed path in a *stored* row is data, and data
arrives from the corpus, so it is precisely a runtime condition.

`parse_citation_path` itself **keeps** its `anyhow::Result`. It is private to the module, its
messages are good, and its six unit tests keep working unchanged; `verify_evidence` maps
`Err(_) => MalformedCitationPath` at the single call site (`verification.rs:380`). Its return type
widens to carry the optional range:

```rust
fn parse_citation_path(s: &str) -> Result<(&str, Option<(usize, usize)>)>
```

### The 16 call sites

**3 production sites.**

| Site | Today | Change shape |
|---|---|---|
| `src/commands/stale_check.rs:515` | `match verify_evidence(..) { Ok(o) => (o, false), Err(_) => (synthetic Unverified, true) }`, plus a `malformed_citation` bool threaded to `.or(Some("malformed_citation"))` at `:540` | Pure deletion. Direct call; drop the tuple, the bool, and the `.or(..)` fallback — `reason` is now always `Some` for an `Unverified` row. Output unchanged (see wire-spelling note). |
| `src/commands/cited_by.rs:153` | `match verify_evidence(..) { Ok(outcome) => match outcome.status {..}, Err(_) => Unverified }` | Drop the outer `match`; keep the inner status match. **Plus a second, non-mechanical change — see below.** |
| `src/components/db.rs:2196` | `verify_evidence(..).unwrap_or(VerificationOutcome { status: Unverified, relocated_to: None, reason: None })`, nested inside `if let Some(root) = root_ref { .. } else { .. }` | Delete the `.unwrap_or(..)` only. **Keep** the `if let Some(root)` and its `else` arm — that arm is "no repo root", not a verify failure. |

**`cited_by.rs` needs more than the fold.** Its filter at `:163`:

```rust
fn is_ranged_citation_for_file(citation_path: &str, file: &str) -> bool {
    citation_path != file
        && citation_path.strip_prefix(file).is_some_and(|s| s.starts_with(':'))
}
```

explicitly excludes `citation_path == file`, which under D1 *is* the whole-file citation of that
file. Left as-is, `kb cited-by <file>` silently reports `Deferred` for every whole-file citation —
i.e. the feature stops working for exactly the citations this epic introduces. Required in .3:
accept the equality case and rename to `is_citation_for_file`. This is the one call site whose
change is behavioral rather than mechanical.

**Known wart, recorded not fixed.** `VerificationOutcome`'s doc says `reason` is `Some` exactly
when `status == Unverified`; the `db.rs:2196` else-arm (no repo root) already violates it with
`reason: None`. Out of scope here. If .3 wants it closed, the fix is a `NoRepoRoot` reason variant,
not a change to the fold.

**13 test sites in `verification.rs`** — lines 748, 774, 790, 815, 876, 895, 917, 933, 962, 991,
1018, 1141, 1209. All mechanical: drop `.unwrap()` / `.expect(..)` on the `verify_evidence` result.
3 + 13 = the 16 the plan counts.

---

## D3 — Size band: whole-file citations are bounded by `MAX_FILE_BYTES`

**Rule.** Option (a), with a mandatory rider.

- `MAX_RANGE_BYTES` (4 MiB, `verification.rs:32`) applies **only** to explicit ranges.
  `RangeTooLarge` stays reachable only for `file:start-end`.
- `MAX_FILE_BYTES` (64 MiB, `verification.rs:28`) is the sole bound on whole-file citations, and
  it already applies to every citation regardless of form (`verification.rs:~265`).
- **No new `UnverifiedReason`.** The 4–64 MiB band that ADR-3 flagged simply becomes verifiable.

**Rider — the whole-file read MUST stream.** Naive option (a) is a memory regression, not a
neutral relaxation. `hash_check_at_citation` slurps the whole range:
`let mut buffer = vec![0u8; range_size as usize];`. Today `MAX_RANGE_BYTES` caps that allocation at
4 MiB; letting whole-file reads run to `MAX_FILE_BYTES` raises peak resident memory to 64 MiB
*per concurrent verification* — and `db.rs:2189` runs a worker pool, so the real figure is
64 MiB × `pool_size`. That is a genuine OOM surface on the inline-verify-k path.

.4 must therefore feed the hasher from a fixed-size buffer (64 KiB is fine) for the whole-file
branch, making its memory O(1) in file size. `MAX_FILE_BYTES` then bounds *work*, which is all it
ever needed to bound. Unifying the explicit-range branch onto the same streaming reader is
optional and welcome, but not required: `MAX_RANGE_BYTES` already bounds that allocation.

**Rejected — option (b)**, reject whole-file citations above `MAX_RANGE_BYTES` with a distinct
reason. It preserves the wart ADR-3 asked us to resolve, invents a reason variant to explain an
implementation detail, and makes the largest files — the ones most expensive to re-cite by hand —
precisely the ones that cannot be cited whole.

---

## D4 — TLA+: no spec change required

**Decision: no amendment to `CitationRelocation.tla`.** Recorded here in the style of
`stale-check-no-spec.md`, per the `AGENTS.md §Formal methods` documented-skip clause.

**Spec provenance — read this first.** `CitationRelocation.tla` and `.cfg` were **deleted from the
code branch** in `12f44d5` ("chore(repo): remove branch-local generated artifacts") and were
**never migrated to the `agentic` branch**, so at the start of this task the file existed only in
git history at `3ed5a1f`, while `verification.rs:8` still doc-references it by path. This task
restored both files from `3ed5a1f` to `.state/agent-kb/tla/`, where the rest of the live specs
live. Without that restore, D4 could only have been asserted; with it, it is checked.

**Why no amendment.** The spec is *structurally range-agnostic*. Its `EvidenceRow` is
`[status, storedHash, contentHash, candidates, excerptStrong]` — there is no byte range, no file
size, no path, and no parse step anywhere in the model. `HashMatch(row)` is
`storedHash = contentHash`, derived and never assigned. Whole-file citations change only *which
bytes feed `contentHash`*, which is below the model's abstraction boundary.

Checked action by action:

| Action / invariant | Effect of D1–D3 |
|---|---|
| `Verify` | Guard is `HashMatch`. Still a hash comparison; whole-file only changes the preimage. Unchanged. |
| `ReVerify` | Models content change + re-hash against an unchanged `storedHash`. Unchanged. |
| `Heal` | Repoints the path, never asserts the hash. See relocation note below. Unchanged. |
| `MalformedCitationPath` (D2) | A new *reason*, not a new *state*. It lands in the existing `"Unverified"` status, which `Init` already ranges over. The model does not carry reasons at all. No new action. |
| `VerifiedImpliesHashMatch` | Holds — the predicate is unchanged. |
| `StoredHashImmutable` | Holds — nothing in D1–D3 writes `citation_hash`. |
| `NoHealOnVerified`, `NoSilentPromotion`, `Monotonicity`, `NonUniqueUnverified`, `WeakExcerptUnverified` | Untouched; none mention ranges. |

**Relocation of a whole-file citation — decided here because .3 hits it.** Today `Heal` anchors the
original range length at the match offset (`new_end = new_start + (end - start)`,
`verification.rs:436-437`). A whole-file citation has no length to anchor. **Decision: a whole-file
citation whose excerpt is found uniquely at `newpath` relocates to bare `newpath`** — the
whole-file form at the new location. This is the natural generalization and it sits inside the
model's existing `Heal` semantics exactly: status becomes `Relocated`, `contentHash` at the new
path is nondeterministic, and promotion to `Verified` requires a later `ReVerify`. The
`new_path == raw_path` decay guard (`verification.rs:445`) keeps working unchanged on bare-vs-bare
comparison. No spec amendment; no new invariant.

**TLC evidence.** `CitationRelocation.cfg` (`MaxRows = 3`, `MaxCandidates = 2`, all 8 invariants)
re-run on the restored spec at this decision, 2026-09-02:

```
Model checking completed. No error has been found.
195495264 states generated, 5009184 distinct states found, 0 states left on queue.
The depth of the complete state graph search is 6.
Finished in 03min 30s
```

The model text is byte-identical to `3ed5a1f`. Since D1–D3 introduce no new action, no new state
component and no new invariant, this exhaustive clean run *is* the evidence for the no-amendment
decision — there is nothing left to re-check. Note the run needs `-workers auto`; single-threaded
it does not finish inside 10 minutes, because `previousRows` is a full row-vector snapshot and the
state space is ~5M distinct states.

**If a reviewer disagrees**, the smallest sufficient amendment is a boolean `wholeFile` field on
`EvidenceRow` plus a `Heal` branch that skips range anchoring — but it would add no reachable
state, because `Heal`'s outcome (`Relocated`, nondeterministic `contentHash`) is identical on both
branches. That is the concrete reason the amendment is not worth making.

---

## D5 — kb-cite emission form and the .1/.3 ordering

**Rule.** `.1` and `.3` ship in the **same crate and the same binary**, so there is no version skew
to defend against — the tool always parses with the parser it was compiled against. No runtime
probe and no cargo feature is needed; both would be machinery guarding an impossible state.

Instead, `.1` never formats a citation itself. `verification.rs` gains one shared emitter, owned by
the same module that owns the parser:

```rust
/// The only place a citation_path string is constructed.
pub(crate) fn format_citation_path(rel_path: &str, range: Option<(usize, usize)>) -> String
```

- **Before .3 lands:** `range = None` emits `format!("{rel_path}:0-{file_size}")` — the workaround
  form today's parser verifies.
- **After .3 lands:** `range = None` emits `rel_path`. **One-line body change, inside .3.**

`.1` calls `format_citation_path(..)` and is correct in both worlds, with zero diff when .3 lands.
The two tasks may therefore land in either order. If .3 lands first, .1 is simply born emitting
bare paths.

**The property that actually makes the ordering safe** is not the emitter, it is the round-trip
self-check: `kb cite` computes `citation_hash` through the verifier's own code path (that is the
point of bd-cnh8.1), so it must additionally assert that the row it is about to write comes back
`VerificationStatus::Verified` from `verify_evidence` **before** emitting it. Any emitter/parser
disagreement then fails loudly at cite time on the author's machine, rather than silently entering
the corpus to be discovered later by a stale-check. Require this assertion in .1's acceptance
criteria.

`format_citation_path` should also be used by the relocation path (`verification.rs:444`) and by
the .5 migration, so that "how a citation is spelled" has exactly one definition in the tree.

---

## Consequences for downstream tasks

- **.1 (kb-cite):** use `format_citation_path`; assert `Verified` round-trip before emit; may land
  before or after .3.
- **.3 (parser + fold):** D1 (incl. empty-file-part, non-regular-file, and `start < end` rules),
  D2 (drop `Result`, add `MalformedCitationPath` spelled `"malformed_citation"`, 3 production +
  13 test sites), the `cited_by.rs` `is_citation_for_file` behavioral fix, and D4's bare-path
  relocation branch.
- **.4 (size band):** D3 — `MAX_RANGE_BYTES` for explicit ranges only, streaming whole-file read.
  Not merely a constant change.
- **.5 (migration):** corpus is 3 distinct paths / 7 rows, all `path:0-<size>`. The
  `agent-kb-events.jsonl` self-citation cannot be healed by hash and needs a separate call.
- **.7 (docs):** `mcp_server.ex:107` prose is already fixed (w3xo.3); document D1's grammar,
  the colon-in-filename escape hatch, and the `N-N` rejection.

## Related

- `.state/.omc/plans/evidence-storage-integrity.md` §ADR-3 — direction decision this refines.
- `.state/agent-kb/tla/decisions/stale-check-no-spec.md` — precedent for D4's form.
- `.state/agent-kb/tla/CitationRelocation.tla` — restored by this task from `3ed5a1f`.
- `src/components/verification.rs` — parser (`:195`), fold (`:380`), bands (`:28`, `:32`),
  relocation anchoring (`:436`).
