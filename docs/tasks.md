# Gator: Implementation Tasks

> **Plan**: [docs/plans/initial-implementation-plan.md](plans/initial-implementation-plan.md)
> **Convention**: Each task is one agent session of focused work. Tasks are completed in dependency order. When a task is done, mark it `[x]` and record the completion date.

## Task Status

| ID | Name | Status | Depends On |
|----|------|--------|------------|
| T001 | Cargo workspace scaffold | [ ] | -- |
| T002 | Database schema and migrations | [ ] | T001 |
| T003 | gator init + connection pool | [ ] | T002 |
| T004 | Plan types, TOML parser, DB CRUD | [ ] | T003 |
| T005 | Plan CLI commands (create, show) | [ ] | T004 |
| T006 | Invariant types + DB CRUD | [ ] | T003 |
| T007 | Invariant CLI commands (add, list, test) | [ ] | T004, T006 |
| T008 | Plan materialization (DB -> TOML) | [ ] | T005 |
| T009 | Git worktree management | [ ] | T001 |
| T010 | Scoped token generation + validation | [ ] | T001 |
| T011 | Harness trait + AgentHandle | [ ] | T001 |
| T012 | ClaudeCodeAdapter | [ ] | T011 |
| T013 | Agent-mode CLI | [ ] | T010, T012 |
| T014 | Task state machine transitions | [ ] | T003, T009, T010, T011 |
| T015 | Gate runner | [ ] | T014 |
| T016 | End-to-end integration test | [ ] | T013, T015 |

---

## Phase 0: Scaffold & DB

---

### T001: Cargo Workspace Scaffold

**Plan reference**: [Phase 0](plans/initial-implementation-plan.md#phase-0-scaffold--db)

**Dependencies**: None (this is the root task)

**Description**: Create the Rust Cargo workspace with the initial crate structure, shared types, and build configuration.

**Implementation instructions**:

1. Initialize git repo (if not already) and create `.gitignore` for Rust (target/, *.swp, .env).

2. Create `Cargo.toml` at workspace root:
   ```toml
   [workspace]
   resolver = "2"
   members = ["crates/*"]

   [workspace.package]
   version = "0.1.0"
   edition = "2024"
   license = "MIT"
   rust-version = "1.85"

   [workspace.dependencies]
   # Async runtime
   tokio = { version = "1", features = ["full"] }
   # Database
   sqlx = { version = "0.8", features = ["runtime-tokio", "tls-rustls", "postgres", "uuid", "chrono", "json"] }
   # CLI
   clap = { version = "4", features = ["derive"] }
   # Serialization
   serde = { version = "1", features = ["derive"] }
   serde_json = "1"
   toml = "0.8"
   # Types
   uuid = { version = "1", features = ["v4", "serde"] }
   chrono = { version = "0.4", features = ["serde"] }
   # Error handling
   anyhow = "1"
   thiserror = "2"
   # Async traits
   async-trait = "0.1"
   # Logging
   tracing = "0.1"
   tracing-subscriber = { version = "0.3", features = ["env-filter"] }
   # Futures/streams
   futures = "0.3"
   tokio-stream = "0.1"
   ```

3. Create three crates:
   - `crates/gator-db/` -- Database layer. `Cargo.toml` depends on: sqlx, uuid, chrono, serde, anyhow, thiserror, tracing. `src/lib.rs` with a placeholder `pub mod migrations;`
   - `crates/gator-core/` -- Business logic. Depends on: gator-db, tokio, uuid, chrono, serde, serde_json, toml, anyhow, thiserror, async-trait, tracing, futures, tokio-stream. `src/lib.rs` with placeholder modules: `pub mod plan; pub mod task; pub mod invariant; pub mod harness; pub mod token; pub mod worktree; pub mod gate; pub mod state;`
   - `crates/gator-cli/` -- CLI binary. Depends on: gator-core, gator-db, clap, tokio, anyhow, tracing, tracing-subscriber, uuid. `src/main.rs` with clap skeleton.

4. Create the clap skeleton in `gator-cli/src/main.rs`:
   ```rust
   use clap::{Parser, Subcommand};

   #[derive(Parser)]
   #[command(name = "gator", about = "LLM coding agent fleet orchestrator")]
   struct Cli {
       #[command(subcommand)]
       command: Commands,
   }

   #[derive(Subcommand)]
   enum Commands {
       /// Initialize the gator database
       Init,
       /// Plan management
       Plan {
           #[command(subcommand)]
           command: PlanCommands,
       },
       /// Invariant management
       Invariant {
           #[command(subcommand)]
           command: InvariantCommands,
       },
       // Agent-mode commands (available when GATOR_AGENT_TOKEN is set)
       /// Read your assigned task (agent mode)
       Task,
       /// Run invariants for your task (agent mode)
       Check,
       /// Report progress (agent mode)
       Progress {
           message: String,
       },
       /// Signal task completion (agent mode)
       Done,
   }
   // ... subcommand enums for Plan, Invariant
   ```

5. Verify: `cargo build` succeeds. `cargo clippy` has no warnings. `cargo test` passes (no tests yet, but compilation is clean).

**Completion gates**:
- [ ] `cargo build --workspace` succeeds with no errors
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] Workspace has three crates: gator-db, gator-core, gator-cli
- [ ] `gator-cli` binary runs and shows help text
- [ ] `.gitignore` is present and correct

---

### T002: Database Schema and Migrations

**Plan reference**: [Phase 0, Section 3](plans/initial-implementation-plan.md#3-database-schema)

**Dependencies**: T001

**Description**: Create the complete PostgreSQL 18 schema as sqlx migrations and the Rust type definitions that mirror the schema.

**Implementation instructions**:

1. Add sqlx-cli as a dev dependency or document that it should be installed: `cargo install sqlx-cli --features postgres`.

2. Create migration directory at `crates/gator-db/migrations/`.

3. Create the initial migration `001_initial_schema.sql`:

   ```sql
   -- Plans
   CREATE TABLE plans (
       id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
       name TEXT NOT NULL,
       project_path TEXT NOT NULL,
       base_branch TEXT NOT NULL,
       status TEXT NOT NULL DEFAULT 'draft'
           CHECK (status IN ('draft', 'approved', 'running', 'completed', 'failed')),
       created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
       approved_at TIMESTAMPTZ,
       completed_at TIMESTAMPTZ
   );

   -- Tasks within a plan
   CREATE TABLE tasks (
       id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
       plan_id UUID NOT NULL REFERENCES plans(id) ON DELETE CASCADE,
       name TEXT NOT NULL,
       description TEXT NOT NULL,
       scope_level TEXT NOT NULL
           CHECK (scope_level IN ('narrow', 'medium', 'broad')),
       gate_policy TEXT NOT NULL
           CHECK (gate_policy IN ('auto', 'human_review', 'human_approve')),
       retry_max INTEGER NOT NULL DEFAULT 3,
       status TEXT NOT NULL DEFAULT 'pending'
           CHECK (status IN ('pending', 'assigned', 'running', 'checking', 'passed', 'failed', 'escalated')),
       assigned_harness TEXT,
       worktree_path TEXT,
       attempt INTEGER NOT NULL DEFAULT 0,
       created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
       started_at TIMESTAMPTZ,
       completed_at TIMESTAMPTZ
   );

   CREATE INDEX idx_tasks_plan_id ON tasks(plan_id);
   CREATE INDEX idx_tasks_status ON tasks(status);

   -- Task dependency DAG
   CREATE TABLE task_dependencies (
       task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
       depends_on UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
       PRIMARY KEY (task_id, depends_on),
       CHECK (task_id != depends_on)
   );

   -- Reusable invariant definitions
   CREATE TABLE invariants (
       id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
       name TEXT NOT NULL UNIQUE,
       description TEXT,
       kind TEXT NOT NULL
           CHECK (kind IN ('test_suite', 'typecheck', 'lint', 'coverage', 'custom')),
       command TEXT NOT NULL,
       args TEXT[] NOT NULL DEFAULT '{}',
       expected_exit_code INTEGER NOT NULL DEFAULT 0,
       threshold REAL,
       scope TEXT NOT NULL DEFAULT 'project'
           CHECK (scope IN ('global', 'project')),
       created_at TIMESTAMPTZ NOT NULL DEFAULT now()
   );

   -- Many-to-many: tasks <-> invariants
   CREATE TABLE task_invariants (
       task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
       invariant_id UUID NOT NULL REFERENCES invariants(id) ON DELETE CASCADE,
       PRIMARY KEY (task_id, invariant_id)
   );

   -- Gate results (per invariant, per attempt)
   CREATE TABLE gate_results (
       id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
       task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
       invariant_id UUID NOT NULL REFERENCES invariants(id) ON DELETE CASCADE,
       attempt INTEGER NOT NULL,
       passed BOOLEAN NOT NULL,
       exit_code INTEGER,
       stdout TEXT,
       stderr TEXT,
       duration_ms INTEGER,
       checked_at TIMESTAMPTZ NOT NULL DEFAULT now()
   );

   CREATE INDEX idx_gate_results_task ON gate_results(task_id, attempt);

   -- Agent event log
   CREATE TABLE agent_events (
       id BIGSERIAL PRIMARY KEY,
       task_id UUID NOT NULL REFERENCES tasks(id) ON DELETE CASCADE,
       attempt INTEGER NOT NULL,
       event_type TEXT NOT NULL,
       payload JSONB NOT NULL,
       recorded_at TIMESTAMPTZ NOT NULL DEFAULT now()
   );

   CREATE INDEX idx_agent_events_task ON agent_events(task_id, attempt);
   ```

4. Create Rust model types in `crates/gator-db/src/models.rs`:
   - `Plan` struct with all fields, derive `sqlx::FromRow, Serialize, Deserialize, Debug, Clone`
   - `Task` struct similarly
   - `Invariant` struct similarly
   - `TaskDependency` struct
   - `TaskInvariant` struct
   - `GateResult` struct
   - `AgentEvent` struct
   - Enum types for `PlanStatus`, `TaskStatus`, `ScopeLevel`, `GatePolicy`, `InvariantKind` with `Display`, `FromStr`, and sqlx `Type`/`Encode`/`Decode` derives

5. Export models from `crates/gator-db/src/lib.rs`.

6. Verify: `cargo build --workspace` succeeds. Types compile. Migration SQL is syntactically valid (will be tested against real DB in T003).

**Completion gates**:
- [ ] Migration file exists at `crates/gator-db/migrations/001_initial_schema.sql`
- [ ] All model types compile with correct derives
- [ ] Enum types implement `Display` and `FromStr`
- [ ] `cargo build --workspace` succeeds
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T003: gator init + Connection Pool

**Plan reference**: [Phase 0](plans/initial-implementation-plan.md#phase-0-scaffold--db)

**Dependencies**: T002

**Description**: Implement database connection pooling, configuration, and the `gator init` command that bootstraps the database.

**Implementation instructions**:

1. Create `crates/gator-db/src/config.rs`:
   - `DbConfig` struct: `database_url: String` (read from `GATOR_DATABASE_URL` env var or `--database-url` CLI flag)
   - Default: `postgresql://localhost:5432/gator`

2. Create `crates/gator-db/src/pool.rs`:
   - `create_pool(config: &DbConfig) -> Result<PgPool>` -- creates an sqlx `PgPool` with sensible defaults (max_connections: 5, acquire_timeout: 10s)
   - `run_migrations(pool: &PgPool) -> Result<()>` -- runs `sqlx::migrate!("./migrations")` (compile-time checked migrations)

3. Implement `gator init` in `gator-cli`:
   - Reads DB config (env var or flag)
   - Creates the database if it does not exist (connect to `postgres` default DB, run `CREATE DATABASE gator` if needed)
   - Creates connection pool
   - Runs migrations
   - Prints success message with table counts

4. Add a `.env.example` file at workspace root:
   ```
   GATOR_DATABASE_URL=postgresql://localhost:5432/gator
   ```

5. Write integration tests in `crates/gator-db/tests/`:
   - Test that migrations run against a fresh Postgres instance
   - Test that running migrations twice is idempotent
   - Use `sqlx::test` attribute or a test helper that creates/drops a temporary database

6. Document in README.md (create if needed): prerequisites (Postgres 18, Rust 1.85+), setup steps.

**Completion gates**:
- [ ] `gator init` successfully creates database and runs migrations against a local Postgres 18 instance
- [ ] Running `gator init` twice is idempotent (no errors on second run)
- [ ] Integration tests pass: `cargo test --workspace`
- [ ] `cargo clippy --workspace -- -D warnings` passes
- [ ] Connection pool creates and destroys cleanly

---

## Phase 1: Plan & Invariant Management

---

### T004: Plan Types, TOML Parser, DB CRUD

**Plan reference**: [Phase 1, Section 3.1](plans/initial-implementation-plan.md#31-tables)

**Dependencies**: T003

**Description**: Implement the plan domain types, plan.toml parser, and database CRUD operations for plans and tasks.

**Implementation instructions**:

1. Define the plan.toml format in `crates/gator-core/src/plan/toml_format.rs`:
   ```toml
   [plan]
   name = "Add user authentication"
   base_branch = "main"

   [[tasks]]
   name = "implement-jwt-module"
   description = """
   Implement JWT token generation and validation.
   - Create src/auth/jwt.rs
   - Implement sign() and verify() functions
   - Use RS256 algorithm
   """
   scope = "narrow"
   gate = "auto"
   retry_max = 3
   depends_on = []
   invariants = ["rust_build", "rust_test", "rust_clippy"]

   [[tasks]]
   name = "implement-login-endpoint"
   description = "..."
   scope = "medium"
   gate = "human_review"
   depends_on = ["implement-jwt-module"]
   invariants = ["rust_build", "rust_test"]
   ```

2. Create `crates/gator-core/src/plan/parser.rs`:
   - `PlanToml` struct (serde-deserializable from the TOML format above)
   - `TaskToml` struct
   - `parse_plan_toml(content: &str) -> Result<PlanToml>` -- parses and validates
   - Validation: check for dependency cycles (topological sort), validate scope/gate values, check that referenced invariants exist (names only at this stage)

3. Create `crates/gator-db/src/queries/plans.rs`:
   - `insert_plan(pool, plan) -> Result<Plan>` -- inserts plan row
   - `get_plan(pool, id) -> Result<Option<Plan>>` -- fetch by ID
   - `list_plans(pool) -> Result<Vec<Plan>>` -- list all
   - `update_plan_status(pool, id, status) -> Result<()>` -- status transition

4. Create `crates/gator-db/src/queries/tasks.rs`:
   - `insert_task(pool, task) -> Result<Task>` -- inserts task row
   - `get_task(pool, id) -> Result<Option<Task>>` -- fetch by ID
   - `list_tasks_for_plan(pool, plan_id) -> Result<Vec<Task>>` -- all tasks in a plan
   - `update_task_status(pool, id, status) -> Result<()>` -- status transition
   - `insert_task_dependency(pool, task_id, depends_on) -> Result<()>`
   - `get_task_dependencies(pool, task_id) -> Result<Vec<Uuid>>`
   - `link_task_invariant(pool, task_id, invariant_id) -> Result<()>`

5. Create `crates/gator-core/src/plan/service.rs`:
   - `create_plan_from_toml(pool, toml: &PlanToml) -> Result<Plan>` -- orchestrates: insert plan, insert tasks, insert dependencies, link invariants. All in a transaction.
   - `get_plan_with_tasks(pool, plan_id) -> Result<(Plan, Vec<Task>)>`

6. Write unit tests for TOML parsing (valid/invalid inputs, cycle detection) and integration tests for DB CRUD.

**Completion gates**:
- [ ] `parse_plan_toml` correctly parses the defined TOML format
- [ ] Cycle detection rejects plans with circular dependencies
- [ ] `create_plan_from_toml` inserts plan + tasks + dependencies in a single transaction
- [ ] DB queries round-trip correctly (insert then read back)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T005: Plan CLI Commands (create, show)

**Plan reference**: [Phase 1, Section 4.1](plans/initial-implementation-plan.md#41-operator-mode)

**Dependencies**: T004

**Description**: Implement `gator plan create <file>` and `gator plan show [plan-id]` CLI commands.

**Implementation instructions**:

1. Implement `gator plan create <file>` in `gator-cli`:
   - Read the plan.toml file from disk
   - Parse with `parse_plan_toml`
   - Validate that all referenced invariant names exist in the DB (warn if not, but allow creation -- invariants can be added later)
   - Call `create_plan_from_toml`
   - Print: plan ID, task count, dependency edges, any warnings
   - Exit 0 on success, non-zero on failure

2. Implement `gator plan show [plan-id]` in `gator-cli`:
   - If plan-id provided: show that plan's full details
   - If no plan-id: list all plans with summary (id, name, status, task count, created_at)
   - For detailed view: show plan metadata, then each task with status, scope, gate policy, dependencies, linked invariants
   - Use a clean, readable terminal format (consider a table or tree view)

3. Implement `gator plan approve <plan-id>`:
   - Transition plan status from `draft` to `approved`
   - Validate: all tasks have at least one invariant linked
   - Record `approved_at` timestamp

4. Write CLI integration tests that:
   - Create a plan from a test TOML file
   - Show the plan and verify output contains expected fields
   - Approve the plan

**Completion gates**:
- [ ] `gator plan create test.toml` successfully creates a plan and prints its ID
- [ ] `gator plan show` lists all plans
- [ ] `gator plan show <id>` shows detailed plan with tasks, dependencies, invariants
- [ ] `gator plan approve <id>` transitions status to approved
- [ ] Error cases handled: file not found, invalid TOML, cycle detected
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T006: Invariant Types + DB CRUD

**Plan reference**: [Phase 1, Section 3.1](plans/initial-implementation-plan.md#31-tables)

**Dependencies**: T003

**Description**: Implement invariant domain types and database CRUD operations.

**Implementation instructions**:

1. Create `crates/gator-core/src/invariant/types.rs`:
   - `InvariantDefinition` struct: name, description, kind, command, args, expected_exit_code, threshold, scope
   - Builder pattern or `new()` with required fields and optional fields

2. Create `crates/gator-db/src/queries/invariants.rs`:
   - `insert_invariant(pool, inv) -> Result<Invariant>` -- insert with ON CONFLICT check on name
   - `get_invariant(pool, id) -> Result<Option<Invariant>>`
   - `get_invariant_by_name(pool, name) -> Result<Option<Invariant>>`
   - `list_invariants(pool) -> Result<Vec<Invariant>>`
   - `delete_invariant(pool, id) -> Result<()>` -- only if not linked to any tasks
   - `get_invariants_for_task(pool, task_id) -> Result<Vec<Invariant>>`

3. Create `crates/gator-core/src/invariant/runner.rs` (stub for now):
   - `run_invariant(invariant: &Invariant, working_dir: &Path) -> Result<InvariantResult>`
   - `InvariantResult` struct: passed, exit_code, stdout, stderr, duration_ms
   - For now, implement only the shell execution: spawn the command with args in the working directory, capture output, compare exit code to expected

4. Write tests:
   - Unit tests for `run_invariant` with a simple `true` / `false` command
   - Integration tests for invariant DB CRUD

**Completion gates**:
- [ ] Invariant CRUD operations work correctly against Postgres
- [ ] `run_invariant` executes a command and captures output
- [ ] `run_invariant` correctly reports pass/fail based on exit code
- [ ] Unique name constraint enforced
- [ ] Cannot delete invariant linked to tasks
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T007: Invariant CLI Commands (add, list, test)

**Plan reference**: [Phase 1, Section 4.1](plans/initial-implementation-plan.md#41-operator-mode)

**Dependencies**: T004, T006

**Description**: Implement the `gator invariant` subcommands.

**Implementation instructions**:

1. Implement `gator invariant add`:
   ```
   gator invariant add rust_build \
     --kind typecheck \
     --command "cargo" \
     --args "build,--workspace" \
     --description "Verify Rust workspace builds"
   ```
   - Required: name, kind, command
   - Optional: args (comma-separated), description, expected-exit-code (default 0), threshold, scope (default project)
   - Insert into DB, print confirmation with ID

2. Implement `gator invariant list`:
   - Table format: name, kind, command, scope
   - Optional `--verbose` for full details

3. Implement `gator invariant test <name>`:
   - Look up invariant by name
   - Run it in the current directory using `run_invariant`
   - Print: pass/fail, exit code, stdout (truncated), stderr (truncated), duration
   - Exit 0 if passed, 1 if failed

4. Write CLI integration tests.

**Completion gates**:
- [ ] `gator invariant add` creates an invariant and prints its ID
- [ ] `gator invariant list` shows all invariants in table format
- [ ] `gator invariant test <name>` runs the invariant and reports pass/fail
- [ ] Duplicate name is rejected with clear error message
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T008: Plan Materialization (DB -> TOML)

**Plan reference**: [Phase 1](plans/initial-implementation-plan.md#phase-1-plan--invariant-management)

**Dependencies**: T005

**Description**: Implement materializing a plan from DB back to plan.toml format. This is the mechanism by which agents receive their context.

**Implementation instructions**:

1. Create `crates/gator-core/src/plan/materialize.rs`:
   - `materialize_plan(pool, plan_id) -> Result<String>` -- generates plan.toml content from DB state
   - Must be a faithful representation: plan metadata, all tasks with descriptions, dependencies, invariant names, scope, gate policy
   - Include task status in materialized output (so readers can see progress)

2. Create `materialize_task(pool, task_id) -> Result<String>`:
   - Generates a standalone task description file (markdown) for a single task
   - Includes: task name, description, invariant commands (so agent can run `gator check`), scope, dependencies and their statuses
   - Does NOT include: plan-level context, other tasks' details, database info

3. Add `gator plan export <plan-id> [--output <file>]`:
   - Materializes plan to TOML and writes to file (or stdout)

4. Write round-trip test:
   - Parse a plan.toml -> insert into DB -> materialize from DB -> parse again
   - Verify: task names, descriptions, dependencies, invariant links all match

**Completion gates**:
- [ ] Round-trip test passes: parse -> DB -> materialize -> parse produces equivalent plan
- [ ] `materialize_task` produces a clean, agent-readable markdown document
- [ ] `gator plan export` writes valid TOML to file or stdout
- [ ] Task status is included in materialized output
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

## Phase 2: Single Agent Dispatch

---

### T009: Git Worktree Management

**Plan reference**: [Phase 2, Section 2.1](plans/initial-implementation-plan.md#21-key-design-decisions)

**Dependencies**: T001

**Description**: Implement git worktree creation, cleanup, and management for agent isolation.

**Implementation instructions**:

1. Create `crates/gator-core/src/worktree/mod.rs`:
   - `WorktreeManager` struct: holds the main repo path and a configurable worktree base directory (default: `../<repo-name>-gator-worktrees/`)
   - `create_worktree(branch_name: &str) -> Result<WorktreeInfo>` -- runs `git worktree add <path> -b <branch>`, returns the worktree path
   - `remove_worktree(path: &Path) -> Result<()>` -- runs `git worktree remove <path>`
   - `list_worktrees() -> Result<Vec<WorktreeInfo>>` -- runs `git worktree list --porcelain`, parses output
   - `cleanup_stale() -> Result<()>` -- runs `git worktree prune`
   - `WorktreeInfo` struct: path, branch, head_commit

2. Branch naming convention: `gator/<plan-name>/<task-name>` (e.g., `gator/add-auth/implement-jwt-module`)

3. Error handling:
   - If worktree already exists for this task, return it (idempotent)
   - If branch name conflicts, append attempt number
   - Clean up worktree on failure (don't leave partial state)

4. Write tests:
   - Create a temporary git repo
   - Create/list/remove worktrees
   - Verify idempotency
   - Verify cleanup on failure

**Completion gates**:
- [ ] `create_worktree` creates an isolated worktree with correct branch
- [ ] `remove_worktree` cleans up completely
- [ ] `list_worktrees` returns accurate list
- [ ] Operations are idempotent
- [ ] Failed operations clean up after themselves
- [ ] `cargo test --workspace` passes (tests use temp git repos)
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T010: Scoped Token Generation + Validation

**Plan reference**: [Phase 2, Section 2.3](plans/initial-implementation-plan.md#23-trust-model)

**Dependencies**: T001

**Description**: Implement HMAC-based scoped token generation and validation for agent-mode authentication.

**Implementation instructions**:

1. Add `hmac` and `sha2` to workspace dependencies:
   ```toml
   hmac = "0.12"
   sha2 = "0.10"
   hex = "0.4"
   ```

2. Create `crates/gator-core/src/token/mod.rs`:
   - `TokenConfig` struct: `secret: Vec<u8>` (read from `GATOR_TOKEN_SECRET` env var, or generated and stored in DB on first `gator init`)
   - `generate_token(config, task_id: Uuid, attempt: u32) -> String`:
     - Format: `gator_at_<task_id>_<attempt>_<hmac_hex>`
     - HMAC-SHA256 over `<task_id>:<attempt>` with the secret
   - `validate_token(config, token: &str) -> Result<TokenClaims>`:
     - Parse the token format
     - Recompute HMAC, compare with constant-time equality
     - Return `TokenClaims { task_id: Uuid, attempt: u32 }`
   - `TokenClaims` struct

3. Create `crates/gator-core/src/token/guard.rs`:
   - `AgentModeGuard` -- checks if `GATOR_AGENT_TOKEN` is set
   - `require_operator_mode() -> Result<()>` -- fails if agent token is set (blocks agent from calling operator commands)
   - `require_agent_mode() -> Result<TokenClaims>` -- fails if agent token is NOT set or invalid

4. Write tests:
   - Generate token, validate it, confirm claims match
   - Tampered tokens are rejected
   - Expired/wrong-format tokens are rejected
   - Constant-time comparison (no timing oracle)

**Completion gates**:
- [ ] `generate_token` produces correctly formatted tokens
- [ ] `validate_token` accepts valid tokens and rejects tampered ones
- [ ] `require_operator_mode` blocks when agent token is set
- [ ] `require_agent_mode` blocks when no valid token is present
- [ ] Token validation is constant-time
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T011: Harness Trait + AgentHandle

**Plan reference**: [Phase 2, Section 5](plans/initial-implementation-plan.md#5-harness-adapter)

**Dependencies**: T001

**Description**: Define the `Harness` trait, `AgentHandle`, `AgentEvent`, and `MaterializedTask` types that form the adapter interface.

**Implementation instructions**:

1. Create `crates/gator-core/src/harness/types.rs`:
   - `AgentHandle` struct: pid (u32), stdin handle (for sending messages), task_id, attempt, harness_name
   - `AgentEvent` enum:
     - `Message { role: String, content: String }` -- agent text output
     - `ToolCall { tool: String, input: serde_json::Value }` -- agent called a tool
     - `ToolResult { tool: String, output: serde_json::Value }` -- tool returned
     - `TokenUsage { input_tokens: u64, output_tokens: u64 }` -- token accounting
     - `Error { message: String }` -- agent error
     - `Completed` -- agent finished
   - `MaterializedTask` struct: task_id, name, description, invariant_commands (Vec<String>), working_dir, env_vars (HashMap including GATOR_AGENT_TOKEN)

2. Create `crates/gator-core/src/harness/trait_def.rs`:
   ```rust
   #[async_trait]
   pub trait Harness: Send + Sync {
       fn name(&self) -> &str;
       async fn spawn(&self, task: &MaterializedTask) -> Result<AgentHandle>;
       fn events(&self, handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
       async fn send(&self, handle: &AgentHandle, message: &str) -> Result<()>;
       async fn kill(&self, handle: &AgentHandle) -> Result<()>;
       async fn is_running(&self, handle: &AgentHandle) -> bool;
   }
   ```

3. Create `crates/gator-core/src/harness/registry.rs`:
   - `HarnessRegistry` struct: holds a `HashMap<String, Box<dyn Harness>>`
   - `register(harness: impl Harness)` -- add a harness
   - `get(name: &str) -> Option<&dyn Harness>` -- look up by name
   - `list() -> Vec<&str>` -- available harness names

4. Write tests: verify trait is object-safe, registry works, types serialize/deserialize correctly.

**Completion gates**:
- [ ] `Harness` trait is object-safe (can be used as `dyn Harness`)
- [ ] `AgentEvent` variants cover all needed event types
- [ ] `MaterializedTask` includes all fields an agent needs
- [ ] `HarnessRegistry` registers and retrieves harnesses
- [ ] All types implement Debug, Clone where appropriate
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T012: ClaudeCodeAdapter

**Plan reference**: [Phase 2, Section 5](plans/initial-implementation-plan.md#5-harness-adapter)

**Dependencies**: T011

**Description**: Implement the Claude Code harness adapter that spawns `claude -p --output-format stream-json` as a subprocess and parses its JSONL output.

**Implementation instructions**:

1. Create `crates/gator-core/src/harness/claude_code.rs`:
   - `ClaudeCodeAdapter` struct: claude_binary_path (default: `claude`)
   - Implement `Harness` trait:
     - `spawn`: Build command: `claude -p --output-format stream-json --allowedTools "Bash,Read,Edit,Write,Glob,Grep" --append-system-prompt <task-instructions>`. Set env vars including `GATOR_AGENT_TOKEN`. Set working directory to worktree. Spawn as tokio `Command`. Return `AgentHandle` with pid and stdin.
     - `events`: Read stdout line-by-line. Parse each line as JSON. Map Claude Code's stream-json format to `AgentEvent` variants. Handle: `assistant` messages, `tool_use`, `tool_result`, `usage` fields.
     - `send`: Write to stdin (for conversation continuation via `--resume`)
     - `kill`: Send SIGTERM, wait briefly, then SIGKILL if needed
     - `is_running`: Check if process is still alive

2. Claude Code stream-json format (key fields to parse):
   - `{"type": "assistant", "message": {"content": [...], "usage": {...}}}`
   - `{"type": "tool_use", "tool": "Bash", "input": {...}}`
   - `{"type": "tool_result", "output": "..."}`
   - Map these to the corresponding `AgentEvent` variants

3. Error handling:
   - If `claude` binary not found, return clear error
   - If process exits unexpectedly, emit `AgentEvent::Error` and `AgentEvent::Completed`
   - Handle malformed JSON lines gracefully (log warning, skip)

4. Write tests:
   - Unit test with a mock subprocess (use `echo` or a test script that emits JSONL)
   - Test JSONL parsing for each event type
   - Test graceful handling of malformed lines

**Completion gates**:
- [ ] `ClaudeCodeAdapter` implements `Harness` trait
- [ ] Can spawn a subprocess and stream JSONL events
- [ ] All `AgentEvent` variants are correctly mapped from Claude Code output
- [ ] Graceful handling of process exit and malformed output
- [ ] `kill` terminates the subprocess cleanly
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T013: Agent-Mode CLI

**Plan reference**: [Phase 2, Section 4.2](plans/initial-implementation-plan.md#42-agent-mode)

**Dependencies**: T010, T012

**Description**: Implement the agent-mode CLI commands that agents use when `GATOR_AGENT_TOKEN` is set.

**Implementation instructions**:

1. In `gator-cli/src/main.rs`, add mode detection at the top of the dispatch:
   ```rust
   // If GATOR_AGENT_TOKEN is set, restrict to agent-mode commands
   if std::env::var("GATOR_AGENT_TOKEN").is_ok() {
       return run_agent_mode(cli).await;
   }
   ```

2. `run_agent_mode` function:
   - Validate the token via `require_agent_mode()` -- extract task_id and attempt
   - Only allow: `Task`, `Check`, `Progress`, `Done` commands
   - For any other command: print "Error: this command is not available in agent mode" and exit 1

3. Implement `gator task` (agent mode):
   - Read task_id from token
   - Look up task in DB (read-only query)
   - Print: task name, description, scope, linked invariant commands
   - Format as clean markdown that an LLM agent can consume

4. Implement `gator check` (agent mode):
   - Read task_id from token
   - Look up linked invariants
   - Run each invariant in the current working directory (the worktree)
   - Print results for each: name, pass/fail, output snippet
   - Exit 0 if ALL pass, exit 1 if ANY fail
   - Record results as agent events (progress)

5. Implement `gator progress "message"` (agent mode):
   - Read task_id and attempt from token
   - Insert an `agent_events` row with event_type="progress" and the message as payload
   - Print confirmation

6. Implement `gator done` (agent mode):
   - Read task_id and attempt from token
   - Insert an `agent_events` row with event_type="done_signal"
   - Print: "Completion signaled. Gator will now run gate checks."
   - Does NOT change task status (that's gator's job)
   - Exit 0

7. Write tests:
   - Test that operator commands are rejected when token is set
   - Test each agent-mode command with a valid token
   - Test that invalid/tampered tokens are rejected

**Completion gates**:
- [ ] `gator task` prints task description when valid token is set
- [ ] `gator check` runs all linked invariants and reports results
- [ ] `gator progress` records a progress event
- [ ] `gator done` signals completion without changing task status
- [ ] Operator commands (plan, dispatch, etc.) are rejected in agent mode
- [ ] Invalid tokens are rejected with clear error
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T014: Task State Machine Transitions

**Plan reference**: [Phase 2, Section 2.5](plans/initial-implementation-plan.md#25-task-state-machine)

**Dependencies**: T003, T009, T010, T011

**Description**: Implement the task state machine with validated transitions, all as database transactions.

**Implementation instructions**:

1. Create `crates/gator-core/src/state/mod.rs`:
   - `TaskStateMachine` struct (holds pool reference)
   - Define valid transitions:
     ```
     pending -> assigned
     assigned -> running
     running -> checking
     checking -> passed
     checking -> failed
     failed -> assigned (retry)
     failed -> escalated
     ```
   - `transition(pool, task_id, from: TaskStatus, to: TaskStatus) -> Result<()>`:
     - Validate the transition is legal
     - Update task status in a transaction
     - Set relevant timestamps (started_at on assigned->running, completed_at on passed/failed)
     - Increment attempt on retry (failed->assigned)
     - Return error if current status doesn't match `from` (optimistic locking)

2. Create `crates/gator-core/src/state/dispatch.rs`:
   - `assign_task(pool, task_id, harness: &str, worktree_path: &Path) -> Result<()>`:
     - Check dependencies are all `passed`
     - Transition pending -> assigned
     - Record harness and worktree_path
   - `start_task(pool, task_id) -> Result<()>`:
     - Transition assigned -> running
   - `begin_checking(pool, task_id) -> Result<()>`:
     - Transition running -> checking
   - `pass_task(pool, task_id) -> Result<()>`:
     - Transition checking -> passed
   - `fail_task(pool, task_id) -> Result<()>`:
     - Transition checking -> failed
   - `retry_task(pool, task_id) -> Result<()>`:
     - Check attempt < retry_max
     - Transition failed -> assigned, increment attempt
   - `escalate_task(pool, task_id) -> Result<()>`:
     - Transition failed -> escalated

3. Create `crates/gator-core/src/state/queries.rs`:
   - `get_ready_tasks(pool, plan_id) -> Result<Vec<Task>>` -- tasks whose dependencies are all `passed` and whose own status is `pending`
   - `get_plan_progress(pool, plan_id) -> Result<PlanProgress>` -- counts by status
   - `is_plan_complete(pool, plan_id) -> Result<bool>` -- all tasks passed?

4. Write tests:
   - Valid transitions succeed
   - Invalid transitions fail with clear error
   - Dependency checks work (cannot assign task if deps not passed)
   - Retry increments attempt counter
   - Retry fails when attempt >= retry_max
   - Optimistic locking prevents double-transition

**Completion gates**:
- [ ] All valid state transitions work correctly
- [ ] Invalid transitions are rejected with descriptive errors
- [ ] Dependency checking prevents premature task assignment
- [ ] Retry respects retry_max
- [ ] Timestamps set correctly on transitions
- [ ] Concurrent transitions don't corrupt state (optimistic locking)
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T015: Gate Runner

**Plan reference**: [Phase 2](plans/initial-implementation-plan.md#phase-2-single-agent-dispatch)

**Dependencies**: T014

**Description**: Implement the gate runner that evaluates invariants for a completed task and records the verdict.

**Implementation instructions**:

1. Create `crates/gator-core/src/gate/mod.rs`:
   - `GateRunner` struct: holds pool reference
   - `run_gate(pool, task_id) -> Result<GateVerdict>`:
     - Transition task to `checking`
     - Look up all invariants linked to the task
     - Run each invariant in the task's worktree directory
     - Record each result in `gate_results` table
     - `GateVerdict`: `Passed` (all invariants passed) | `Failed { failures: Vec<GateFailure> }`
     - `GateFailure`: invariant name, exit_code, stderr snippet

2. Create `crates/gator-core/src/gate/evaluator.rs`:
   - `evaluate_verdict(pool, task_id, verdict: &GateVerdict) -> Result<GateAction>`:
     - Look up the task's `gate_policy`
     - If `auto` and verdict is `Passed`: transition to `passed`
     - If `auto` and verdict is `Failed`: transition to `failed`, check retry eligibility
     - If `human_review` or `human_approve`: leave in `checking` state, return `GateAction::HumanRequired`
   - `GateAction` enum: `AutoPassed`, `AutoFailed { can_retry: bool }`, `HumanRequired`

3. Create `crates/gator-db/src/queries/gate_results.rs`:
   - `insert_gate_result(pool, result) -> Result<GateResult>`
   - `get_gate_results(pool, task_id, attempt) -> Result<Vec<GateResult>>`

4. Write tests:
   - All invariants pass -> auto gate -> task passes
   - One invariant fails -> auto gate -> task fails
   - Human review gate -> leaves task in checking state
   - Gate results recorded correctly in DB
   - Test with real shell commands (`true`, `false`)

**Completion gates**:
- [ ] Gate runs all linked invariants for a task
- [ ] Results recorded in `gate_results` table
- [ ] `auto` gate policy correctly passes or fails tasks
- [ ] `human_review`/`human_approve` leaves task for human
- [ ] Retry eligibility checked on failure
- [ ] `cargo test --workspace` passes
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

### T016: End-to-End Integration Test

**Plan reference**: [Phase 2 verification](plans/initial-implementation-plan.md#phase-2-single-agent)

**Dependencies**: T013, T015

**Description**: Create an end-to-end integration test that exercises the full single-agent dispatch cycle.

**Implementation instructions**:

1. Create `tests/integration/single_agent.rs` (workspace-level integration test):

2. Test scenario:
   a. `gator init` -- bootstrap test database
   b. Add invariants: `echo_test` (command: `echo "ok"`, exit 0) and `always_pass` (command: `true`)
   c. Create a plan.toml with one narrow-scope task that depends on both invariants
   d. `gator plan create` the plan
   e. `gator plan approve` the plan
   f. Manually simulate the dispatch cycle:
      - Get ready tasks
      - Create worktree
      - Generate scoped token
      - Assign task (pending -> assigned -> running)
      - In the worktree, verify `gator task` (with token) prints task description
      - Verify `gator check` (with token) runs invariants and passes
      - Verify `gator done` (with token) signals completion
      - Verify operator commands are rejected with the agent token
      - Run gate
      - Verify task status is `passed`
      - Verify gate results in DB

3. Use a test harness that:
   - Creates a temporary Postgres database
   - Creates a temporary git repo
   - Cleans up everything on test completion (even on failure)

4. Also write a negative test:
   - Task with an invariant that fails (command: `false`)
   - Verify gate fails, task status is `failed`
   - Verify retry works

**Completion gates**:
- [ ] Full happy-path test passes: init -> create plan -> approve -> dispatch -> agent commands -> gate -> passed
- [ ] Negative test passes: invariant failure -> task failed -> retry
- [ ] Agent-mode commands work with scoped token
- [ ] Operator commands rejected in agent mode
- [ ] All temp resources cleaned up
- [ ] `cargo test --workspace` passes (including this integration test)
- [ ] `cargo clippy --workspace -- -D warnings` passes

---

## Completion Log

Record task completions here as they happen:

| Task | Completed | Notes |
|------|-----------|-------|
| | | |
