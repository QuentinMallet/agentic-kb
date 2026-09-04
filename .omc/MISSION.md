# Mission: agentic-kb

## Purpose

Provide coding agents (Claude Code, opencode, peers) with an event-logged, evidence-grounded knowledge base whose contents are defensible at retrieval time — not just an append-only log of past beliefs.

The KB is queried before nearly every non-trivial agent decision and is written to after most non-trivial agent sessions. Reads condition writes (skills prompt "query KB first"), producing a positive-feedback structure. Without truth-preserving mechanisms, drift accumulates and corroborates itself.

## What we serve

- **Agents at session start** — KB content shapes priors loaded into `claudeMd` context.
- **Skills mid-session** — `/kb-explorer`, `/post-impl`, `/threat-model`, and others routing decisions through `kb_search`.
- **Operators across machines** — the agentic orphan branch is the cross-machine source of truth; SQLite is local materialization.

## What success looks like

A measurable defensibility number: of N entries sampled and re-verified, what fraction are still true. This number is published and drives confidence priors at retrieval time. The KB ships as **evidence-grounded knowledge** rather than "log of past agent beliefs."

Operational signals:
- **F1 truth-decay** (cited code rotted under the citation) is detected on the read path, not silently corroborated.
- **Provenance** (causal chain from belief to grounding evidence) is queryable.
- **Per-kind precision** (observation vs belief vs procedure vs convention) is measured and trends are visible.

## Architecture invariants

- **Event log on `agentic` orphan branch is single source of truth.** SQLite (`agent-kb.db`) is materialization, rebuildable from JSONL by `kb_rebuild`.
- **MCP-only contract for agents.** Agents call `kb_*` MCP tools; never shell out to the `kb` CLI binary. Shell hooks (SessionStart, PreToolUse) may use the CLI — they are infrastructure, not agent decisions.
- **Rust core + Elixir MCP** is the language split. Rust owns the event log, SQLite, FTS5, embedding, and verification. Elixir owns MCP transport.
- **Embeddings via bge-small-en-v1.5** (BAAI), default cache-bound. No remote inference dependencies in the hot path.
- **Per-path ownership rules** (e.g. `architecture/*` written only by kb-explorer; `conventions/<lang>` only by code-reviewer) are documented in agent CLAUDE.md and enforced through agent prompts, not the DB.

## Phased roadmap (from defensibility plan)

| Phase | What it adds | Status |
|-------|--------------|--------|
| 0 | Data model: `kind` enum on entries, `evidence` table, `audit_runs` table | next push |
| 1 | Citation opt-in: `kb_add` accepts evidence; `kb_search` returns evidence + HEAD-byte-hash `verified` flag | next push (code-kind only) |
| 2 | Confidence-aware retrieval; cost-validation harness graduates to background verification if needed | future |
| 3 | Contradiction detection at write (NLI) | future |
| 4 | Mandatory evidence for `belief`/`procedure`/`observation` | future |
| 5 | Provenance API + `/kb-audit` weekly skill | future |

## Non-goals

- **Replacing hybrid retrieval.** FTS+cosine stays. Defensibility is an additive layer, not a redesign.
- **Distributed consensus.** Cross-machine sync rides on the agentic git branch. No new transport.
- **Real-time cross-machine contradiction detection.** Contradiction check at write is local.
- **Formal correctness proof of the knowledge.** This is an empirical knowledge system, not a theorem prover. Confidence is calibrated, not derived.
- **SaaS dependencies.** No remote LLM call in any hot path. Local models only (bge-small for embedding; small local NLI for Phase 3+).
- **Removing the operator.** Operator stays in the loop for ownership-rule edge cases and audit-verdict overrides.

## Out-of-band consumers

This repo's KB is consumed by other projects' agent sessions. Cross-project knowledge transfer happens via:
- The `agentic` orphan branch (per-repo).
- Promoted seeds at `~/.local/share/agent-kb/promoted-seeds/<domain>.json` (manually curated, written by `/kb-review`).

Cross-project source-weighting (a `code-reviewer` finding in repo A as evidence for repo B) is not yet modeled; it's a known gap, deferred to Phase 2+.
