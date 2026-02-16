# Dev Log: Replace PostgreSQL with SQLite

Session: 2026-02-16
Branch: `jfb/add-sqlite`

## Goal

Make gator work out of the box without requiring PostgreSQL. Replace PostgreSQL
entirely with SQLite as the default (and only) database backend. No dual
backend, no trait abstraction, no DbPool enum -- just swap PgPool for SqlitePool
and rewrite the SQL.

## Decisions taken

1. **Full replacement, not dual backend.** We explicitly rejected keeping
   PostgreSQL alongside SQLite. The branch adds SQLite; PostgreSQL support can
   be re-added behind a feature flag later if needed.

2. **SQLite file location: `~/.local/share/gator/gator.db`** (XDG_DATA_HOME).
   Config file stays at `~/.config/gator/config.toml`.

3. **Dogfooding approach.** We're using gator itself to orchestrate this work:
   write a plan.toml, run it through `gator plan create` / `approve` /
   `dispatch`, watch via `gator dashboard`, and fix bugs as they surface.

## What happened

### Codebase exploration

Thorough analysis of PostgreSQL coupling across the workspace. Key findings:

- **No compile-time query macros** -- all queries use `sqlx::query()` /
  `sqlx::query_as()` (runtime), not `sqlx::query!()`. This means no live
  database needed at build time.

- **PostgreSQL-specific patterns** concentrated in:
  - `gator-db/src/pool.rs` -- PgPool, PgPoolOptions, ensure_database_exists,
    pg_tables queries
  - `gator-db/src/config.rs` -- PostgreSQL URL parsing, maintenance_url()
  - `gator-db/src/queries/*.rs` -- `$1,$2` placeholders, `NOW()`, `::text`
    casts, `->>'field'` JSON operators, `TEXT[]` array type
  - `gator-db/migrations/` -- gen_random_uuid(), TIMESTAMPTZ, BIGSERIAL,
    JSONB, TEXT[]
  - `gator-core/src/plan/service.rs` -- transactions with PgPool
  - `gator-test-utils/` -- testcontainers, Docker, PostgreSQL container

- **~30 query functions** across 5 query modules need parameter placeholder
  changes (`$N` -> `?N`)

- **RETURNING clause** used extensively -- supported in SQLite 3.35+ (bundled
  by sqlx 0.8)

- **ON CONFLICT DO NOTHING** -- works in SQLite as-is

### Plan generation attempts

#### Attempt 1: `gator plan generate "replace PostgreSQL with SQLite..."`

Produced a **trait abstraction** plan: `GatorDb` trait, `PostgresDb` and
`SqliteDb` implementations, migrate everything to `&dyn GatorDb` /
`Arc<dyn GatorDb>`. 9 tasks, 12 dep edges. Way over-engineered for a full
replacement.

#### Attempt 2: sharper prompt with "no dual backend, no trait abstraction"

Produced a **DbPool enum** plan: `DbPool { Postgres(PgPool), Sqlite(SqlitePool) }`
with `match pool { ... }` dispatch in every query function. 13 tasks, 22 dep
edges. Still dual-backend, still not what we asked for.

**Conclusion:** `plan generate` can't incorporate architectural constraints well
enough. The plan.toml needs to be written by hand using the task breakdown we
designed during planning.

### Bugs found during dogfooding

Tracked in `docs/todo.md`:

1. **Token usage events report placeholder values** -- dashboard showed
   `input_tokens:1, output_tokens:1` for events that clearly consumed more.

2. **Dashboard shows "running" after task transitions to "checking"** --
   refresh timing issue in the TUI.

3. **Plan status stuck at "running" after all tasks pass** -- meta-plan never
   transitioned to `completed` even though 1/1 tasks passed.

4. **`plan generate` lacks architectural context** -- no way to pass design
   decisions or constraints to the agent generating the plan.

5. **No `gator plan delete` command** -- only way to remove stale plans is
   direct SQL.

## Correct task breakdown (to be written as plan.toml)

The plan.toml should have ~11 tasks, straight replacement:

```
rewrite-migrations -----> no deps (rewrite 5 files in place for SQLite)
update-db-config -------> no deps (PG URL -> SQLite path)
rewrite-pool -----------> depends on: rewrite-migrations, update-db-config
update-models ----------> depends on: rewrite-migrations
update-queries ---------> depends on: rewrite-pool, update-models
update-core ------------> depends on: update-queries
update-cli -------------> depends on: update-core
rewrite-test-utils -----> depends on: rewrite-pool
update-tests -----------> depends on: rewrite-test-utils, update-queries
update-docs ------------> depends on: update-cli
update-ci --------------> depends on: update-docs
```

### Per-task summary

**rewrite-migrations** -- Rewrite all 5 migration files in
`crates/gator-db/migrations/` for SQLite: UUID->TEXT, TIMESTAMPTZ->TEXT,
TEXT[]->TEXT (JSON), BIGSERIAL->INTEGER PRIMARY KEY AUTOINCREMENT, JSONB->TEXT,
gen_random_uuid()->app-side, now()->datetime('now').

**update-db-config** -- `crates/gator-db/src/config.rs`: default path
`~/.local/share/gator/gator.db`, connection URL `sqlite:///path?mode=rwc`,
remove `database_name()`, `maintenance_url()`.

**rewrite-pool** -- `crates/gator-db/src/pool.rs`: PgPool->SqlitePool,
PgPoolOptions->SqlitePoolOptions, PRAGMA foreign_keys=ON + journal_mode=WAL +
busy_timeout=5000 via after_connect, simplify ensure_database_exists to
create_dir_all, rewrite table_counts for sqlite_master.

**update-models** -- `crates/gator-db/src/models.rs`: `Invariant.args`
Vec<String> -> Json<Vec<String>> (for SQLite TEXT storage), verify Uuid and
DateTime<Utc> work with SQLite TEXT columns via sqlx features.

**update-queries** -- All 5 files in `crates/gator-db/src/queries/`: $1->?1
parameter placeholders, NOW()->datetime('now'), remove ::text casts,
payload->>'field' -> json_extract(payload,'$.field'), explicit UUID generation
for INSERTs, bind args as Json for invariants.

**update-core** -- All gator-core files referencing PgPool: mechanical
replacement with SqlitePool (or DbPool type alias). plan/service.rs
transactions work the same way.

**update-cli** -- config.rs: database.url->database.path, resolution chain
update. main.rs: remove --database-url, simplify Init (no --db-url), simplify
db-init.

**rewrite-test-utils** -- Remove testcontainers, use temp SQLite files.
create_test_db() returns (SqlitePool, PathBuf).

**update-tests** -- All test files across workspace for new signatures.

**update-docs** -- README.md, docs/how-to.md, CLAUDE.md: remove PG
prerequisite, update examples.

**update-ci** -- `.github/workflows/ci.yml`: remove PostgreSQL service
container, remove PG env vars.

## SQL translation reference

| PostgreSQL | SQLite |
|---|---|
| `gen_random_uuid()` | App-side `Uuid::new_v4()` |
| `TIMESTAMPTZ DEFAULT now()` | `TEXT DEFAULT (datetime('now'))` |
| `TEXT[] DEFAULT '{}'` | `TEXT DEFAULT '[]'` (JSON) |
| `BIGSERIAL PRIMARY KEY` | `INTEGER PRIMARY KEY AUTOINCREMENT` |
| `JSONB` | `TEXT` (JSON string) |
| `$1, $2, ...` | `?1, ?2, ...` |
| `NOW()` | `datetime('now')` |
| `status::text` | `status` (remove cast) |
| `payload->>'field'` | `json_extract(payload, '$.field')` |
| `(x)::bigint` | `CAST(x AS INTEGER)` |
| `ON CONFLICT DO NOTHING` | Same (works in SQLite) |
| `RETURNING *` | Same (SQLite 3.35+) |
| `PgPool` | `SqlitePool` |
| `PgPoolOptions` | `SqlitePoolOptions` |

## SQLite PRAGMAs needed

Set per-connection (e.g. via `after_connect` on the pool):
- `PRAGMA foreign_keys = ON` -- not persisted, must set every connection
- `PRAGMA journal_mode = WAL` -- enables concurrent readers
- `PRAGMA busy_timeout = 5000` -- retry on lock contention

## Key files (by change scope)

**Rewrite:**
- `crates/gator-db/src/pool.rs`
- `crates/gator-db/migrations/001_initial_schema.sql` (+ 002-005)
- `crates/gator-test-utils/src/lib.rs`

**Heavy edits:**
- `crates/gator-db/src/config.rs`
- `crates/gator-db/src/queries/agent_events.rs` (JSON operators)
- `crates/gator-db/src/queries/plans.rs`
- `crates/gator-db/src/queries/tasks.rs`
- `crates/gator-db/src/queries/invariants.rs`
- `crates/gator-db/src/queries/gate_results.rs`
- `crates/gator-core/src/plan/service.rs`
- `crates/gator-cli/src/main.rs`
- `crates/gator-cli/src/config.rs`

**Mechanical (PgPool -> SqlitePool):**
- `crates/gator-core/src/state/mod.rs`
- `crates/gator-core/src/state/queries.rs`
- `crates/gator-core/src/state/dispatch.rs`
- `crates/gator-core/src/orchestrator/mod.rs`
- `crates/gator-core/src/lifecycle/mod.rs`
- `crates/gator-core/src/gate/mod.rs`
- `crates/gator-core/src/gate/evaluator.rs`
- `crates/gator-core/src/plan/materialize.rs`
- All CLI command handler files

**Docs/CI:**
- `README.md`
- `docs/how-to.md`
- `CLAUDE.md`
- `.github/workflows/ci.yml`

## Workspace dependencies

**Remove:** `testcontainers`, `testcontainers-modules`
**Change:** sqlx features `"postgres"` -> `"sqlite"`

## Next steps

1. Write plan.toml by hand using the task breakdown above
2. `gator plan create plan.toml && gator plan approve plan.toml`
3. `gator dispatch plan.toml`
4. Watch via `gator dashboard`, fix bugs as they surface
5. Or: just implement directly if the orchestration keeps fighting us
