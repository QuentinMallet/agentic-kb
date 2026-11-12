# ADR-6: Per-request project root for the MCP port

- Status: Accepted 2026-11-12
- Bead: bd-kb-mcp-per-request-project-root-byn9
- Decision: Option A — one server, an operator allowlist of project roots, and
  an optional `repo_root` envelope field on every port request

## Context

`kb mcp --db <repo>/.state/agent-kb/agent-kb.db` derives exactly one
repository root from the database path (`config::root_from_db`) and every
handler resolves citations against it: `handle_add` hashes evidence rows
against `root_from_db(&paths.db)` and `handle_cite` against `paths.root`.
`open_citation_descriptor` (`src/components/verification.rs`) then rejects any
`citation_path` that does not stay inside that root with
`citation path escapes repository root`.

The deployed fleet runs one long-lived MCP server per agent host, not per
checkout. The Hermes server is started with `cwd = machines_conf`
(`services.hermes-cli.kbProjectDir`), so a Hermes session working in
`~/Documents/boulot/gestion_crise` — which has its own
`.state/agent-kb/` store — cannot record a single evidenced finding: every
citation is resolved against `machines_conf` and rejected, and even an
evidence-free `kb_add` lands in the wrong project's store. AGENTS.md makes
`kb_add` of exploration findings mandatory, so this blocks a core workflow for
every project except the one the server happens to be rooted in.

The escape check itself is correct and must stay: citation resolution is a
containment path (ADR-5), and the MCP trust boundary is "the repository and
filesystem the operator selected" (`docs/src/security/mcp-authorization.md`).

## Options considered

- **(A) Per-request `repo_root` against an operator allowlist.** The request
  names the project; the server maps it to that project's own store
  (`Paths::discover` from that directory) and verifies, after
  canonicalization, that the discovered root lies inside a root the operator
  listed at launch. Everything downstream (hashing, verification, events,
  database, lock) already takes a `Paths`, so the handlers are unchanged.
- **(B) Route by the citation's git toplevel.** Rejected: it lets the
  *client* pick the filesystem boundary by crafting a path, which is exactly
  what the escape check exists to prevent; entries without evidence would
  still have no project; and `git rev-parse` per citation puts a subprocess
  on the containment path.
- **(C) Multi-DB registry with its own identifiers.** Rejected as more
  surface than the problem needs: a project is already identified by its
  checkout directory everywhere else in the toolchain (`Paths::discover`,
  `DbDiscovery`, peers' `source_repo`/`target_repo`), and the registry would
  still need an allowlist to be safe.
- **One server process per project.** Rejected for the Hermes gateway, which
  is one long-lived backend unit serving sessions in arbitrary checkouts; it
  cannot know the project set at start.

## Decision

1. `kb mcp` gains a repeatable `--project-root <dir>` flag, merged with the
   colon-separated `KB_PROJECT_ROOTS` environment variable. Each entry is
   canonicalized at startup; entries that do not exist are skipped with a
   warning (fail closed: fewer roots, never more). The root derived from
   `--db` is always allowlisted. The `ready` envelope lists the effective
   `project_roots`.
2. Every port request accepts an optional `repo_root: string` envelope field
   (declared in `request_struct!` next to `id` and `method`, so
   `deny_unknown_fields` still holds). Resolution, in order:
   - must be an absolute path that exists and canonicalizes
     (`fs::canonicalize`: symlinks and `..` are resolved, so neither can name
     a directory outside the allowlist);
   - the same discovery walk as the CLI (`Paths::discover_from`) runs from the
     canonical directory, so a worktree under `<repo>/.state/worktrees/` or a
     subdirectory selects the enclosing repository's store, with the same
     canonical-over-legacy layout precedence (B3);
   - the discovered root, canonicalized, must equal an allowlisted root or be
     a descendant of one (component-wise `Path::starts_with`);
   - otherwise the request is refused with `code: "project_root_rejected"`
     before any handler runs, any lock is taken, or any file is opened.
   A request without `repo_root` behaves exactly as before.
3. All existing per-root checks are unchanged: `citation_path` is still
   repository-relative to the *selected* root and still goes through
   `open_citation_descriptor` (syntactic `..` rejection, no-symlink open,
   post-open containment check). `kb_cite`, `kb_add` hash resolution, the
   `verified` flag and relocation keep their semantics; they now run against
   the project the caller named.
4. The Elixir server declares `repo_root` on every tool schema whose request
   reaches the Rust port and forwards it unchanged. `kb_rebuild` stays bound
   to the server's own store (the supervised rebuild worker is launched with
   the startup `--db`), which is documented rather than widened here.
5. Entries are written to the selected project's store, so no `cross-repo:`
   tag is introduced: an entry about `gestion_crise` lives in
   `gestion_crise/.state/agent-kb/`, where `kb search` in that checkout
   finds it. Cross-repo *reads* remain the peer graph's job (`kb peers`).

## Security argument

The operator still selects the filesystem boundary; it is now a set of
directories instead of one. A client can select only a directory inside that
set, by its canonical path, and can only make the server read files inside
the discovered repository root under that directory — the same reads it
could already make inside the single root. Nothing is read before the
allowlist check passes; the check compares canonical paths, so `..`,
absolute paths and symlinks pointing elsewhere cannot widen it. These
properties are pinned by `tests/mcp_project_root.rs`, including a proptest
over arbitrary relative component sequences.

No TLA+ module models root selection (`McpBoundary.tla` covers framing,
`CitationRelocation.tla` relocation within one root), and the change does not
alter any modeled invariant, so no spec refinement accompanies it.

## Consequences

- machines_conf must pass the project set to the Hermes server, e.g.
  `KB_PROJECT_ROOTS=/home/urist/Documents/perso:/home/urist/Documents/boulot`
  in the `agentic-kb-mcp` server `env`; until then the server behaves as
  today (single root, `repo_root` requests rejected).
- Clients (SOUL.md / kb-protocol.md guidance) should pass
  `repo_root: <checkout dir>` on `kb_add`, `kb_cite`, `kb_search`, `kb_get`
  when working outside the server's default project.
- A first `kb_add` against a project whose database does not exist yet
  initializes it under that project's lock, like the CLI does.
