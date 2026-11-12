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
   - the same discovery walk as the CLI runs from the canonical directory
     (`Paths::discover_bounded`), so a worktree under
     `<repo>/.state/worktrees/` or a subdirectory selects the enclosing
     repository's store, with the same canonical-over-legacy layout precedence
     (B3). The walk is *bounded*: it never inspects a directory outside the
     widest allowlisted root containing the request — each candidate and each
     ancestor consulted by the managed-worktree check is tested for
     containment before any `stat`/`read` of it, and the walk stops at the
     boundary;
   - the discovered root, canonicalized, must equal an allowlisted root or be
     a descendant of one (component-wise `Path::starts_with`);
   - the store's physical paths (database and SQLite sidecars, lock, event
     log, compact state, query-hit log) are walked component by component
     with `lstat` semantics from the selected root, and **every existing
     entry of each store directory** is `lstat`ed, so sidecars the store code
     creates today or later (`.tmp`, `.generation`, `.deadletter`, rebuild
     locks, `.bak`) are covered without a list of names. A symlink is
     accepted only if it canonicalizes inside the selected root; a link
     leaving the root, a dangling link, or an uninspectable component is
     refused (`StoreEscapes`). Absent components are not created by this
     check. Temp files are created `O_EXCL`, after unlinking a stale one, so
     a link planted after the check fails instead of being followed;
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
over arbitrary relative component sequences, a table/proptest of symlinked
store components (`.state`, `.state/agent-kb`, db, lock, event log; to an
outside directory, existing file, or dangling target) asserting refusal with
the outside directory unchanged, and an inotify-observed test asserting that
discovery never opens sentinel files in ancestors outside the allowlist.

## Limits (what this does not protect against)

The containment check is a pathname policy evaluated before use, not an
fd-pinned one. Stated plainly:

- **TOCTOU.** Between the check and the later open of the lock, database or
  event log, a concurrent writer with filesystem access to an allowlisted
  project can replace a verified directory or file with a symlink (or rename
  directories) and redirect the open. No directory file descriptor pins the
  verified directory and the store is opened by pathname (SQLite and the
  existing lock/event-log code take paths), so `O_NOFOLLOW`/`openat` hardening
  is not applied. Anyone who can write inside an allowlisted project can
  already write that project's store; the residual risk is a *less trusted
  local process* redirecting the server to a store outside the allowlist.
  Mitigation is operational: allowlist only directories writable solely by
  the operator, or run mutually untrusted callers in separate processes.
- **Bind mounts and other mount tricks.** Containment is a path-namespace
  property. A bind mount (or another mount) inside an allowlisted project
  that exposes an outside directory is indistinguishable from a real
  subdirectory and is not detected. Case-insensitive filesystems are not
  specially handled.
- **Symlinks inside the root are allowed.** A store component that is a
  symlink resolving inside the selected root is accepted (legacy checkouts
  alias `agent-kb`); it may alias another in-root location, and the store
  and the repository identity can then differ within that root.
- **Concurrent writers.** The check does not coordinate with other
  processes. Two `kb` processes (CLI plus port) use the existing per-store
  lock; a store reached through two different in-root aliases would take two
  different lock files. This was not stress-tested.
- **Discovery reads at most seven bytes, never through a link.** Discovery
  and the check issue `stat`/`lstat` and list the store directories inside the
  allowlisted root before authorization of the *store* finishes. The only file
  whose content is read is a managed-worktree `.state/.git`, opened
  `O_NOFOLLOW` after an `lstat` regular-file check; a symlinked `.git` is
  treated as "not a gitlink" and its target is never opened.
- **Not covered by the entry rule.** Entries created in a store directory
  after the check (TOCTOU, above), and hard links (not distinguishable from
  files by `lstat`). SQLite opens its own files `O_NOFOLLOW`.

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
