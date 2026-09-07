---
name: kb-audit
description: Two-step KB audit cycle — sample live entries, collect verifier verdicts, record results to calibrate confidence weights
triggers:
  - "audit kb"
  - "calibrate confidence"
  - "kb audit"
  - "audit knowledge base"
---

# /kb-audit — KB Audit Cycle

Runs the two-step audit protocol: sample live entries → collect verifier verdicts → record results.
Confidence weights (`source_weights` table) are updated after each audit cycle and surfaced as the
`confidence` field on every `kb_search` result.

## Protocol

### Step 1 — Sample

```json
{
  "method": "audit_run",
  "sample_size": 10
}
```

Response: `{ "run_id": "<uuid>", "samples": [...] }`

`samples` contains only live entries (`is_stale=0`) with evidence present (`evidence_status='present'`).
`sample_size` is clamped to 1–50. The `run_id` is required for Step 2.

**Audit eligibility:** only entries with `evidence_status='present'` are sampled, which covers
`kind ∈ {observation, belief, procedure}`. Entries with `kind='convention'` or `kind='memory'`
have `evidence_status='n/a'` and are never sampled — their `confidence` field stays at the
Beta(1,1) bootstrap value (0.5) indefinitely. To calibrate conventions, use a separate workflow
(e.g. team policy review) rather than the evidence-citation audit cycle.

### Step 2 — Collect verdicts and record

For each sampled entry, verify the cited evidence still accurately supports the summary.
Then record all verdicts in one call:

```json
{
  "method": "audit_record",
  "run_id": "<run_id from Step 1>",
  "verdicts": [
    { "entry_id": "<id>", "verdict": true },
    { "entry_id": "<id>", "verdict": false, "note": "citation no longer exists" }
  ]
}
```

- `verdict: true` → entry stays live; `source_weights.successes` incremented
- `verdict: false` → entry expired (JSONL event + `is_stale=1`); `source_weights.failures` incremented
- Replaying the same `(run_id, entry_id)` is a no-op (idempotent)
- Response: `{ "recorded": N, "expired": M }`

### Step 3 — Summarise (optional)

```json
{ "method": "audit_report" }
```

Response: `{ "per_kind_session_precision": [...], "last_run_at": "...", "total_runs": N }`

`per_kind_session_precision` rows have `{ kind, session_id, precision, n }`.

## Confidence formula

Beta(1,1) posterior (Laplace smoothing):

```
confidence = (successes + 1) / (successes + failures + 2)
```

- Fresh entry (no audits): `confidence = 0.5`, `audit_n = 0`
- After 1 success: `2/3 ≈ 0.667`
- After 10 successes, 0 failures: `11/12 ≈ 0.917`

`confidence` and `audit_n` appear on every `kb_search` result.

## Verifier independence

For unbiased calibration, route the verifier to a **different model or provider** than the agent
that wrote the entries. This prevents the writer from confirming its own work.

Example routing in `.claude/omc.jsonc`:
```json
{
  "team": {
    "roleRouting": {
      "verifier": { "provider": "openrouter", "model": "anthropic/claude-opus-4-5" }
    }
  }
}
```

Or use `/team 1:codex "verify KB entries …"` to route to Codex for the verdict pass.

## Provenance

Entries with `evidence.kind=derived` link back to source entries via `derived_from`.
Use `kb_provenance` to walk the DAG:

```json
{ "method": "provenance", "entry_id": "<id>", "max_depth": 64 }
```

Response: `{ "roots": [...], "dangling": [...], "graph": [{ "from": "B", "to": "A" }, ...], "truncated": false }`

When the queried entry has no provenance parents, it is itself the root and appears in `roots`. Missing parent ids referenced by `derived_from` appear in `dangling` instead of `roots`.

## Durability

`audit_record` and `audit_run` append durable events (`audit_record_batch`,
`audit_run_candidates_batch`), and `apply_event` has arms for both that
populate `audit_runs`, `source_weights`, and `audit_run_candidates` on
replay. Running `kb_rebuild` therefore reconstructs audit history and
confidence weights recorded by this release, the same as any other table —
it does not erase them.

Only rows written by an older binary that predates these event arms — audit
mutations that landed directly in the database with no corresponding
JSONL event — are lost on rebuild, because there is no event to replay them
from. There is no such gap for audit cycles run against this release.
