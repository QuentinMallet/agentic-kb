# First-run behavior

Pure reads do not initialize a repository database. When `db::open_ro` reports
`db::DbUninitialized`, the normal read surfaces return their empty
representation, leave the database absent, and call `db::note_uninitialized`
to write a one-line note to stderr. This is pinned for `kb search` by
`kb_search_on_a_fresh_repo_is_empty_and_succeeds`, for `kb context` by
`kb_context_on_a_fresh_repo_is_empty_and_succeeds`, for `kb cited-by` by
`kb_cited_by_on_a_fresh_repo_is_empty_and_succeeds` and
`kb_cited_by_json_on_a_fresh_repo_is_an_empty_array`, for `kb tests` by
`kb_tests_on_a_fresh_repo_is_empty_and_succeeds`, for `kb eval` by
`kb_eval_on_a_fresh_repo_is_empty_and_succeeds`, and for default non-healing
`kb stale-check` by `kb_stale_check_on_a_fresh_repo_is_empty_and_succeeds`, all
in `tests/first_run_ux.rs`.

The Rust port handlers follow the same no-creation rule when invoked against an
uninitialized database. `handle_kb_get` returns its not-found envelope,
`handle_provenance` returns an empty graph, and `handle_audit_report` returns an
empty report; each calls `db::note_uninitialized`. These cases are pinned by
`handle_kb_get_on_uninitialized_db_reports_not_found_without_creating_it`,
`handle_provenance_on_uninitialized_db_returns_an_empty_graph`, and
`handle_audit_report_on_uninitialized_db_returns_an_empty_report` in
`src/commands/mcp.rs`.

Initialization belongs to writers and recovery entry points. `kb add` delegates
from `Add::execute_with` to `kb_core::add`, whose write path uses `db::open_rw`
under `add::acquire_lock`. Before mutating CLI commands run,
`EntryPoint::run` enters through `db::open_or_init`; its `init_locked` branch
creates the schema and stamp under the repository lock. `Mcp::execute` uses the
same initializer at port startup. See [Lock Contract](./lock-contract.md) for
the complete command-to-opener classification.

`OlderThan::execute` also leaves an absent database untouched and returns an
empty stdout result, but currently returns before calling
`db::note_uninitialized`; unlike the surfaces above, it has no first-run case in
`tests/first_run_ux.rs`.
