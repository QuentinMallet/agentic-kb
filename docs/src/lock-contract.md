# Lock Contract

This table records the current opener discipline in `src/` for C2/L1b.

`read-only` means `db::open_ro(...)` and therefore `PRAGMA query_only=ON`.
`write-under-lock` means `db::open_rw(paths, &lock)` with a live `paths.lock`
guard. `init` means `db::open_or_init(...)`. `scratch` means
`db::open_scratch(...)`. `legacy-wrapper pending L2/L3/L1c` means a file still
contains `db::open_db(...)`.

| File | Opener class | Backing reference |
| --- | --- | --- |
| `src/bin/kb-bench-fixture.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/bin/kb-bench-fixture.rs:71`. |
| `src/commands/add.rs` | mixed: write-under-lock + legacy-wrapper pending L2/L3/L1c | Production write open at `src/commands/add.rs:280`; remaining `open_db` calls are test coverage at `src/commands/add.rs:519` and `:750`. |
| `src/commands/cited_by.rs` | read-only | `db::open_ro` at `src/commands/cited_by.rs:96`; first-run empty-result behavior is covered by `tests/first_run_ux.rs`. |
| `src/commands/compact.rs` | mixed: write-under-lock + init + legacy-wrapper pending L2/L3/L1c | Locked opens at `src/commands/compact.rs:91` and `:330`; `open_or_init` and `open_db` remain in tests at `:1655`, `:1507`, `:1642`, `:1720`, `:1812`. |
| `src/commands/compress.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/compress.rs:58`, `:168`, `:440`, `:471`. |
| `src/commands/context.rs` | read-only | `db::open_ro` at `src/commands/context.rs:90`; `tests/first_run_ux.rs` covers the uninitialized-read contract shared with other read paths. |
| `src/commands/digest.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/digest.rs:224`, `:295`. |
| `src/commands/eval.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/eval.rs:124`. |
| `src/commands/expire.rs` | write-under-lock | `db::open_rw` at `src/commands/expire.rs:47`. |
| `src/commands/mcp.rs` | mixed: read-only + write-under-lock + init + legacy-wrapper pending L2/L3/L1c | Read opens at `src/commands/mcp.rs:244`, `:321`, `:539`, `:1814`, `:2016`; locked writes at `:959`, `:1027`, `:1045`, `:1307`, `:1473`, `:1559`, `:1939`, `:2076`; test fixture init at `:2307`; remaining `open_db` sites are legacy/test surfaces pending later tasks. |
| `src/commands/migrate_citations.rs` | mixed: write-under-lock + legacy-wrapper pending L2/L3/L1c | Locked migration write at `src/commands/migrate_citations.rs:253`; remaining `open_db` calls at `:83` and `:410`. |
| `src/commands/older_than.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/older_than.rs:37`. |
| `src/commands/peers.rs` | mixed: read-only + write-under-lock + init | Read opens at `src/commands/peers.rs:203`, `:274`, `:697`; locked peer mutations at `:109`, `:236`, `:451`, `:605`, `:729`, `:768`; test setup uses `open_or_init` at `:1003`, `:1066`. |
| `src/commands/rebuild.rs` | mixed: read-only + write-under-lock + scratch + init + legacy-wrapper pending L2/L3/L1c | Read opens at `src/commands/rebuild.rs:121`, `:211`; locked writes at `:266`, `:346`; scratch DB opens at `:394`, `:440`; test init at `:677`; schema-upgrade and test helper `open_db` sites remain at `:143`, `:701`, `:880`, `:923`, `:995`, `:1067`, `:1339`. |
| `src/commands/reembed.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/reembed.rs:44`; `check_embed_mode_vintage` stays a separate contract row below. |
| `src/commands/run.rs` | write-under-lock | `db::open_rw` at `src/commands/run.rs:61`. |
| `src/commands/search.rs` | mixed: read-only + init + legacy-wrapper pending L2/L3/L1c | Read federation opens at `src/commands/search.rs:120`, `:153`; test init at `:593`; remaining `open_db` calls at `:779`, `:862`, `:953`, `:1019`, `:1049` are test fixtures. |
| `src/commands/stale_check.rs` | mixed: write-under-lock + legacy-wrapper pending L2/L3/L1c | Locked auto-heal path at `src/commands/stale_check.rs:231`; legacy wrapper at `:191` stays pending. |
| `src/commands/test_add.rs` | write-under-lock | `db::open_rw` at `src/commands/test_add.rs:69`. |
| `src/commands/tests.rs` | legacy-wrapper pending L2/L3/L1c | `db::open_db` at `src/commands/tests.rs:31`. |
| `src/components/db.rs` | contract definitions: read-only + write-under-lock + scratch + init + legacy-wrapper pending L2/L3/L1c | Openers are defined at `src/components/db.rs:254`, `:298`, `:320`, `:389`, `:418`; `tests/open_split.rs` is the opener contract test suite. |
| `src/components/kb_core.rs` | mixed: write-under-lock + legacy-wrapper pending L2/L3/L1c | Production add path uses `db::open_rw` at `src/components/kb_core.rs:150`; remaining `open_db` sites at `:791`, `:1034`, `:1069` are tests. |
| `query_hits` telemetry exemption | out of scope: separate DB, not the KB DB | `src/commands/search.rs:253`, `src/commands/context.rs:126`, and `src/commands/mcp.rs:344` / `:373` write `paths.query_hits`, which is a different SQLite file rooted by `config.rs:249`. |
| `check_embed_mode_vintage` | write to `kb_meta`, contract deferred to L3 | `src/components/db.rs:873` writes via the caller's connection; current call sites are `src/commands/reembed.rs` and `src/commands/mcp.rs`, and L3 moves the write under the lock. |
| rebuild schema stamp | schema-upgrade lock, not `paths.lock`; scratch-class exception | `src/commands/rebuild.rs:146-149` writes `schema_version` while holding `schema-upgrade.lock`; this is outside the steady-state read/write opener split. |
| rebuild swap precondition | invariant row | At `src/commands/rebuild.rs:448-456`, the WAL-deletion comment must cite the invariant "no connection is open for write against the old inode at the point of rename". |
| read-path temp DB constraint | standing constraint | `db::open_ro` sets `PRAGMA query_only=ON` at `src/components/db.rs:271`, so no read path may rely on `CREATE TEMP TABLE` or any temp-database write. |
| ADR-7 rebuild `kb_meta` survival | rebuild swap invariant | Rebuild's scratch DB becomes the live DB at `src/commands/rebuild.rs:462`; per ADR-7, keys other than `schema_version` and `embed_text_mode` do not survive that swap. |

