# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## MANDATORY: Use td for Task Management

Run td usage --new-session at conversation start (or after /clear). This tells you what to work on next.

Sessions are automatic (based on terminal/agent context). Optional:
- td session "name" to label the current session
- td session --new to force a new session in the same context

Use td usage -q after first read.

## Build & Development

```bash
# Build the workspace
cargo build --workspace

# Install the binary locally
cargo install --path crates/gator-cli

# Lint (must pass with zero warnings)
cargo clippy --workspace -- -D warnings

# Format
cargo fmt --all
cargo fmt --all -- --check   # check only
```

## Testing

Tests require Docker (PostgreSQL 18 container). Two ways to run:

```bash
# Preferred: nextest (starts PG container automatically via setup script)
cargo nextest run --workspace

# Run a single test
cargo nextest run --workspace -E 'test(test_name_here)'

# Run tests for one crate
cargo nextest run -p gator-db

# Alternative: cargo test (spins up container via testcontainers per binary)
cargo test --workspace
```

**How it works:** The nextest setup script (`scripts/start-test-pg.sh`) starts a shared PostgreSQL 18 container and exports `GATOR_TEST_PG_URL`. Each test calls `gator_test_utils::create_test_db()` which creates a unique `gator_test_{uuid}` database with migrations applied. Tests clean up via `drop_test_db()`. When using plain `cargo test`, the `gator-test-utils` crate spins up a container per binary via `OnceCell` instead of the setup script.

## Usage Workflow

Gator orchestrates LLM agents to implement a feature plan. Here is the workflow:

### One-time setup

```bash
gator init --db-url postgresql://localhost:5432/gator   # write config, generate HMAC secret
gator db-init                                            # create DB and run migrations

# Define reusable invariants (gate checks)
gator invariant add rust_build  --kind typecheck  --command cargo --args "build,--workspace"
gator invariant add rust_test   --kind test_suite --command cargo --args "test,--workspace"
gator invariant add rust_clippy --kind lint       --command cargo --args "clippy,--workspace,--,--deny,warnings"
```

### Per-feature

1. **Write a plan TOML** decomposing the feature into tasks with dependencies, scope, and gate policy. See `README.md` for the plan format and an annotated example.

2. **Import and approve:**
   ```bash
   gator plan create plan.toml
   gator plan approve <plan-id>
   ```

3. **Dispatch** -- gator assigns tasks to agents in DAG order, each in an isolated worktree:
   ```bash
   gator dispatch <plan-id> --max-agents 4
   ```
   For each task: create worktree, generate scoped token, spawn agent, stream events, run gate checks on completion. `auto`-gated tasks pass/fail automatically; `human_review`/`human_approve` tasks pause for operator action.

4. **Monitor and review:**
   ```bash
   gator status <plan-id>       # one-shot summary
   gator dashboard              # interactive TUI
   gator gate <task-id>         # view invariant results
   gator approve <task-id>      # approve a human-review task
   ```

5. **Merge and ship:**
   ```bash
   gator merge <plan-id>        # merge task branches into base_branch
   gator pr <plan-id>           # create GitHub PR
   gator cleanup <plan-id>      # remove worktrees
   ```

## Architecture

Four-crate workspace:

| Crate | Role |
|-------|------|
| `gator-db` | PostgreSQL models, queries, migrations, pool |
| `gator-core` | Business logic: orchestrator, state machine, harness, isolation, gates |
| `gator-cli` | Binary (`gator`). Clap commands, TUI dashboard, HTTP server |
| `gator-test-utils` | Shared test infrastructure (PG container, DB creation/teardown) |

### Key abstractions

- **`Harness` trait** (`gator-core/src/harness/trait_def.rs`) -- adapter interface for LLM agents. Object-safe (`Box<dyn Harness>`). Claude Code implementation in `claude_code.rs`.
- **`Isolation` trait** (`gator-core/src/isolation/mod.rs`) -- workspace backends. `WorktreeIsolation` uses git worktrees. `ContainerIsolation` uses sandboxed Docker with copy-in/copy-out (no bind mounts).
- **Task state machine** (`gator-core/src/state/mod.rs`) -- `pending -> assigned -> running -> checking -> passed/failed`. Failed tasks retry up to `retry_max` (back to `assigned`), then `escalated`. Operators can override escalation back to `pending`. Optimistic locking on all transitions.
- **Gate system** (`gator-core/src/gate/`) -- runs linked invariants against task output, evaluates verdict based on gate policy (auto/human_review/human_approve).
- **Orchestrator** (`gator-core/src/orchestrator/`) -- DAG-aware fleet execution with semaphore-based concurrency control.
- **HMAC tokens** (`gator-core/src/token/`) -- scoped to (task_id, attempt). Agents get `GATOR_AGENT_TOKEN` injected; presence triggers agent-mode CLI restriction.

### Trust model

Two modes determined by `GATOR_AGENT_TOKEN` env var:
- **Operator mode** (no token): full CLI command surface
- **Agent mode** (token set): restricted to `task`, `check`, `progress`, `done`

### Database

PostgreSQL 18 is the single source of truth. One database instance supports multiple projects simultaneously (each plan carries its own `project_path`). Plan TOML files are imports; the DB is authoritative.

- Migrations: `crates/gator-db/migrations/` (001-005)
- Runtime migrator via `sqlx::migrate::Migrator` -- no compile-time `sqlx::migrate!` macro, no `DATABASE_URL` needed at build time
- Connection pool: `gator-db/src/pool.rs` (max 5 connections, 10s acquire timeout)
- Config: `GATOR_DATABASE_URL` env var, falls back to `postgresql://localhost:5432/gator`

### Agent lifecycle (single task)

1. Create workspace (worktree or container)
2. Generate scoped HMAC token
3. Materialize task description
4. Spawn agent via Harness
5. Stream events with timeout
6. Extract results (container: copy-out)
7. Run gate checks on host
8. Evaluate verdict, transition state

## Environment Variables

| Variable | Purpose |
|----------|---------|
| `GATOR_DATABASE_URL` | PostgreSQL connection string |
| `GATOR_TOKEN_SECRET` | HMAC secret (64 hex chars) |
| `GATOR_AGENT_TOKEN` | Scoped agent token (triggers agent mode) |
| `GATOR_TEST_PG_URL` | Test PostgreSQL URL (set by nextest setup script) |

Resolution: CLI flags > env vars > config file (`~/.config/gator/config.toml`) > defaults.

## CI

GitHub Actions (`.github/workflows/ci.yml`): format check, clippy, nextest with `--profile ci`. CI uses a PostgreSQL 18 service container directly (no setup script).
