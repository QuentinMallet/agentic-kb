# Credential Redaction

Credential redaction is a write-time defense mechanism that automatically scrubs secrets from KB entries before persistence. It acts on all fields that might contain sensitive data: `citation_excerpt`, `content`, `summary`, and `tags`.

## When It Runs

Redaction is unconditional in `kb_core::add` and runs in all code paths:
- CLI `kb add` command
- MCP `kb_add` tool
- Bulk import via `kb_import`

There is no flag to disable it. Every entry is scanned before it reaches the database.

## Detection Patterns

The following patterns are detected and replaced with `<REDACTED>`:

| Pattern | Example | Notes |
|---------|---------|-------|
| OpenAI API key | `sk-proj-...` (20+ alphanum) | Covers both legacy `sk-...` and project-scoped keys |
| GitHub PAT | `ghp_1234567890...` (36 chars) | Personal access tokens; classic tokens not detected |
| GitLab token | `glpat-...` | Project and group access tokens |
| Slack bot token | `xoxb-...` | Bot user tokens |
| Slack user token | `xoxp-...` | User tokens |
| AWS access key | `AKIA...` (16 uppercase alphanum) | Paired with secret key detection in some contexts |
| JWT | `eyJ...` (3 base64url segments) | Detects structure; no signature validation |
| PEM blocks | `-----BEGIN CERTIFICATE-----` ... `-----END CERTIFICATE-----` | RSA, EC, and other key types |
| `.env`-style values | `KEY=<32+ base64 chars or 40+ hex>` | Detects high-entropy secrets in key=value pairs |

## Replacement

All matched secrets are replaced with the literal string `<REDACTED>`. The entry is still written to the KB; only the sensitive value is obscured.

Example before redaction:
```
This uses API key sk-proj-1234567890abcdefghij in production.
```

After redaction:
```
This uses API key <REDACTED> in production.
```

## Accepted Misses

The redactor is conservative to avoid false positives. The following are NOT detected:

| Case | Reason |
|------|--------|
| Short / low-entropy custom tokens | Difficult to distinguish from real identifiers without known prefix |
| Bearer tokens without known prefix | Would require semantic analysis |
| Multi-line `.env` files embedded in larger text | Context is unclear; may be documentation examples |

If a sensitive value passes through redaction, audit the detection patterns and open an issue.

## Audit and Exemption

The command `kb audit-log` emits `audit` and `expire` events (not upsert). These events bypass redaction because they describe the *absence* of data, not the data itself.

Source: `src/components/redactor.rs`, `tests/redaction_corpus.rs`

## Read-Time Complement

Redaction is the write-time half of the defense. At read time, `kb_search` results wrap `citation_excerpt` values in the envelope `<<UNTRUSTED_EXCERPT>>...<<END>>` to signal that data is sourced from user input and may contain injection payloads. These two layers work together:
- Write time: remove known secrets before they enter the store
- Read time: mark user-supplied excerpts as untrusted for the consumer to handle with care

## Configuration

No configuration is needed. Redaction is always on. To verify redaction is working:

```bash
kb add --path test/secret --summary "Testing secret sk-proj-1234567890abcdefghij"
kb info test/secret
# Output will show: "Testing secret <REDACTED>"
```
