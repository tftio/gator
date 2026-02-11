# gator

Rust CLI that orchestrates fleets of LLM coding agents with DAG-based task
scheduling, invariant-based gating, and scoped trust. You write a plan (a TOML
file describing tasks and their dependencies), gator dispatches each task to an
isolated agent, runs your invariants (build, test, lint) as gate checks, and
manages the full lifecycle through to merge and PR creation.

## Prerequisites

- **PostgreSQL 18** -- gator's single source of truth
- **Rust 1.85+** (cargo)
- **Claude Code CLI** (or another supported harness: Codex CLI, OpenCode)
- **git** -- gator creates worktrees for task isolation

## Install

### From GitHub Releases (recommended)

```bash
curl -fsSL https://raw.githubusercontent.com/tftio/gator/main/scripts/install.sh | bash
```

Options via environment variables:

```bash
# Install a specific version
GATOR_VERSION=v0.1.0 curl -fsSL https://raw.githubusercontent.com/tftio/gator/main/scripts/install.sh | bash

# Install to a custom directory
INSTALL_DIR=/opt/bin curl -fsSL https://raw.githubusercontent.com/tftio/gator/main/scripts/install.sh | bash
```

Pre-built binaries are available for:
- Linux x86_64 / aarch64
- macOS x86_64 / aarch64 (Apple Silicon)

### From source

```
cargo install --path crates/gator-cli
```

## Quickstart

```bash
# 1. Write the config file (~/.config/gator/config.toml).
#    Generates a random token secret and stores the DB URL.
gator init --db-url postgresql://localhost:5432/gator

# 2. Create and migrate the database.
gator db-init

# 3. Define invariants (reusable checks that gate every task).
gator invariant add rust_build \
  --kind typecheck --command cargo --args "build,--workspace"

gator invariant add rust_test \
  --kind test_suite --command cargo --args "test,--workspace"

gator invariant add rust_clippy \
  --kind lint --command cargo --args "clippy,--workspace,--,--deny,warnings"

# 4. Verify an invariant works in your project directory.
gator invariant test rust_build

# 5. Write a plan file (see Plan Format below, or docs/examples/).
#    Then import it:
gator plan create plan.toml

# 6. Review and approve the plan.
gator plan show <plan-id>
gator plan approve <plan-id>

# 7. Dispatch -- gator assigns tasks to agents, respecting the DAG.
gator dispatch <plan-id>

# 8. Monitor progress.
gator status <plan-id>       # one-shot summary
gator dashboard              # interactive TUI

# 9. Review tasks that need human approval.
gator gate <task-id>         # view gate results
gator approve <task-id>      # approve
gator reject <task-id>       # reject (triggers retry or escalation)

# 10. When all tasks pass, merge and create a PR.
gator merge <plan-id>
gator pr <plan-id>
```

## Plan format

Plans are TOML files with a `[plan]` header and one or more `[[tasks]]` entries.

### `[plan]` -- plan metadata

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | -- | Human-readable plan name |
| `base_branch` | yes | -- | Git branch to branch from for each task |
| `token_budget` | no | unlimited | Total token cap (input + output) across all agents |
| `default_harness` | no | `"claude-code"` | Harness for tasks that don't override it |
| `isolation` | no | `"worktree"` | Isolation strategy: `"worktree"` or `"container"` |

### `[[tasks]]` -- task entries

| Field | Required | Default | Description |
|-------|----------|---------|-------------|
| `name` | yes | -- | Unique name within the plan (used in `depends_on`) |
| `description` | yes | -- | What the agent should do (multi-line OK) |
| `scope` | yes | -- | `"narrow"`, `"medium"`, or `"broad"` |
| `gate` | yes | -- | `"auto"`, `"human_review"`, or `"human_approve"` |
| `retry_max` | no | `3` | Max retries before escalation |
| `depends_on` | no | `[]` | Names of tasks that must pass first (forms a DAG) |
| `invariants` | no | `[]` | Names of invariants to run as gate checks |
| `harness` | no | plan default | Override the harness for this task |

### Scope and gate semantics

| Scope | Meaning | Recommended gate |
|-------|---------|-----------------|
| `narrow` | Touches 1-2 files, well-defined | `auto` |
| `medium` | Crosses module boundaries | `human_review` |
| `broad` | Architectural changes | `human_approve` |

- **`auto`**: invariants pass → task passes; invariants fail → task fails (retry eligible).
- **`human_review`**: invariants run, then a human reviews the results and approves or rejects.
- **`human_approve`**: same as human_review, but intended for broad-scope changes requiring explicit sign-off.

### Validation rules

- At least one task is required.
- Task names must be unique.
- `depends_on` must reference existing task names.
- The dependency graph must be acyclic (DAG).
- Scope must be `narrow`, `medium`, or `broad`.
- Gate must be `auto`, `human_review`, or `human_approve`.

### Annotated example

```toml
[plan]
name = "Add user authentication"
base_branch = "main"
token_budget = 500000

[[tasks]]
name = "define-types"
description = """
Create src/auth/types.rs with UserId, HashedPassword,
Claims, and AuthError types.
"""
scope = "narrow"
gate = "auto"
invariants = ["rust_build", "rust_test", "rust_clippy"]

[[tasks]]
name = "impl-jwt"
description = "Implement JWT sign/verify in src/auth/jwt.rs."
scope = "narrow"
gate = "auto"
depends_on = ["define-types"]
invariants = ["rust_build", "rust_test", "rust_clippy"]

[[tasks]]
name = "impl-password"
description = "Implement password hashing in src/auth/password.rs."
scope = "narrow"
gate = "auto"
depends_on = ["define-types"]
invariants = ["rust_build", "rust_test", "rust_clippy"]

[[tasks]]
name = "add-login-endpoint"
description = "Wire up POST /login using jwt and password modules."
scope = "medium"
gate = "human_review"
depends_on = ["impl-jwt", "impl-password"]
invariants = ["rust_build", "rust_test", "rust_clippy"]
```

This creates a diamond DAG: `define-types` runs first, then `impl-jwt` and
`impl-password` run in parallel, and `add-login-endpoint` runs after both
complete. See `docs/examples/` for more.

## Command reference

All commands accept `--database-url <URL>` to override the database connection.

### Setup

**`gator init`** -- Write a config file.

```
gator init [--db-url <URL>] [--force]
```

Creates `~/.config/gator/config.toml` with the database URL and a randomly
generated token secret. Use `--force` to overwrite an existing config.

**`gator db-init`** -- Initialize the database.

```
gator db-init
```

Creates the gator database (if it doesn't exist) and runs migrations.

### Plan management

**`gator plan create`** -- Import a plan from a TOML file.

```
gator plan create <file>
```

Parses and validates the TOML file, inserts the plan and tasks into the
database, and links invariants by name. Warns if referenced invariants don't
exist yet.

**`gator plan show`** -- Show plan details or list all plans.

```
gator plan show [plan-id]
```

Without an argument, lists all plans. With a plan ID, shows full details
including tasks, dependencies, invariants, and status.

**`gator plan approve`** -- Approve a plan for execution.

```
gator plan approve <plan-id>
```

Transitions the plan from `draft` to `approved`. Fails if any task has zero
linked invariants.

**`gator plan export`** -- Export a plan from the database as TOML.

```
gator plan export <plan-id> [--output <file>]
```

Materializes the plan from the database. Writes to stdout by default.

### Invariants

**`gator invariant add`** -- Define a reusable invariant.

```
gator invariant add <name> --kind <kind> --command <cmd> [options]
```

Options:
- `--kind` -- `test_suite`, `typecheck`, `lint`, `coverage`, or `custom`
- `--command` -- executable to run (e.g. `cargo`)
- `--args` -- comma-separated arguments (e.g. `test,--workspace`)
- `--description` -- human-readable description
- `--expected-exit-code` -- exit code that means success (default: `0`)
- `--threshold` -- numeric threshold (e.g. coverage percentage)
- `--scope` -- `global` or `project` (default: `project`)
- `--timeout` -- timeout in seconds (default: `300`)

**`gator invariant list`** -- List all invariants.

```
gator invariant list [--verbose]
```

**`gator invariant test`** -- Test-run an invariant in the current directory.

```
gator invariant test <name>
```

### Execution

**`gator dispatch`** -- Dispatch a plan for execution.

```
gator dispatch <plan-id> [--max-agents <N>] [--timeout <secs>]
```

Assigns tasks to agents in DAG order. Defaults: 4 concurrent agents, 1800s
timeout per task.

**`gator status`** -- Show plan status and task progress.

```
gator status [plan-id]
```

Without an argument, lists all plans. With a plan ID, shows per-task status.

**`gator dashboard`** -- Launch interactive TUI dashboard.

```
gator dashboard
```

**`gator log`** -- Show agent event log for a task.

```
gator log <task-id> [--attempt <N>]
```

### Review

**`gator gate`** -- View gate results for a task.

```
gator gate <task-id>
```

Shows invariant check results (pass/fail, exit code, output snippets).

**`gator approve`** -- Approve a task awaiting human review.

```
gator approve <task-id>
```

**`gator reject`** -- Reject a task (sends to failed for retry/escalation).

```
gator reject <task-id>
```

**`gator retry`** -- Retry a failed or escalated task.

```
gator retry <task-id> [--force]
```

Resets the task to pending. Use `--force` to override the retry limit.

### Completion

**`gator report`** -- Show token usage and duration report for a plan.

```
gator report <plan-id>
```

**`gator cleanup`** -- Remove worktrees for completed tasks.

```
gator cleanup <plan-id> [--all]
```

By default removes worktrees only for passed tasks. Use `--all` for all tasks.

**`gator merge`** -- Merge passed task branches into the base branch.

```
gator merge <plan-id> [--dry-run]
```

**`gator pr`** -- Create a GitHub PR from a completed plan.

```
gator pr <plan-id> [--draft] [--base <branch>]
```

## Configuration

### Config file

Location: `~/.config/gator/config.toml`

```toml
[database]
url = "postgresql://localhost:5432/gator"

[auth]
token_secret = "a1b2c3d4..."  # 64 hex chars (32 bytes), auto-generated by gator init
```

File permissions are set to `0600` (owner read/write only).

### Resolution order

| Setting | CLI flag | Environment variable | Config file | Default |
|---------|----------|---------------------|-------------|---------|
| Database URL | `--database-url` | `GATOR_DATABASE_URL` | `database.url` | `postgresql://localhost:5432/gator` |
| Token secret | -- | `GATOR_TOKEN_SECRET` | `auth.token_secret` | (required) |

CLI flags take highest priority, then environment variables, then the config
file, then defaults.

## Agent mode

When `GATOR_AGENT_TOKEN` is set in the environment, gator restricts itself to
four commands. This is the interface that LLM agents use -- they never see the
operator commands.

### Commands

| Command | Description |
|---------|-------------|
| `gator task` | Read the assigned task (name, description, scope, invariants) |
| `gator check` | Run all linked invariants and report pass/fail |
| `gator progress "msg"` | Record a progress event |
| `gator done` | Signal task completion (gator then runs gate checks) |

### Token format

```
gator_at_<task-id>_<attempt>_<hmac-sha256-hex>
```

Tokens are scoped to exactly one (task, attempt) pair. They are HMAC-SHA256
signed with the token secret from the config file. Gator generates and injects
the token automatically when dispatching tasks -- agents don't need to create
tokens.

### Trust model

Agents are untrusted. The token scopes them to a single task. They cannot read
other tasks, modify plan state, or access operator commands. The only way a task
advances through the state machine is via gator's orchestrator, which validates
invariant results before transitioning state.

## Architecture

```
gator-cli          gator-core              gator-db
(binary)           (business logic)        (PostgreSQL)
   |                    |                       |
   +-- config.rs        +-- plan/               +-- models.rs
   +-- agent.rs         |   +-- toml_format.rs  +-- queries/
   +-- plan_cmds.rs     |   +-- parser.rs       |   +-- plans.rs
   +-- dispatch_cmd.rs  |   +-- service.rs      |   +-- tasks.rs
   +-- tui.rs           |   +-- materialize.rs  |   +-- invariants.rs
   +-- ...              +-- state/              |   +-- gate_results.rs
                        |   +-- mod.rs (FSM)    +-- pool.rs
                        |   +-- dispatch.rs     +-- migrations/
                        +-- gate/
                        |   +-- evaluator.rs
                        +-- harness/
                        |   +-- trait_def.rs
                        |   +-- claude_code.rs
                        +-- token/
                        |   +-- mod.rs (HMAC)
                        |   +-- guard.rs
                        +-- orchestrator/
                        +-- worktree/
                        +-- isolation/
```

- **PostgreSQL is the single source of truth.** Plan files are imports; the
  database is authoritative. `gator plan export` materializes back to TOML.
- **Harness trait** (`harness/trait_def.rs`) is the adapter interface. Each
  harness (Claude Code, Codex CLI, etc.) implements `spawn`, `events`, `send`,
  `kill`, and `is_running`.
- **Git worktree isolation**: each task gets its own worktree branched from
  `base_branch`, so agents work in parallel without conflicts.
- **Task state machine**:

```
pending --> assigned --> running --> checking --> passed
                                       |
                                       +--> failed --> assigned (retry)
                                       |           \-> escalated --> pending (operator override)
```

## License

MIT
