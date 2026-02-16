# How to Use Gator

Gator orchestrates fleets of LLM coding agents. You decompose a feature into
tasks, define quality gates, and gator dispatches each task to an isolated
agent, runs your invariants, and manages the lifecycle through to merge.

This guide walks through every stage of that workflow in detail.

## 1. Initial Setup

### Install

```bash
# Pre-built binary (recommended)
curl -fsSL https://raw.githubusercontent.com/tftio/gator/main/scripts/install.sh | bash

# Or from source
cargo install --path crates/gator-cli
```

### Initialize

```bash
gator init --db-url postgresql://localhost:5432/gator
```

This creates `~/.config/gator/config.toml` with:
- The database URL
- A randomly generated HMAC token secret (64 hex chars)

File permissions are set to `0600` (owner-only). Run `gator init --force` to
overwrite an existing config.

### Create the database

```bash
gator db-init
```

Creates the database if it doesn't exist and runs all migrations. Safe to run
repeatedly -- migrations are idempotent.

## 2. Invariants

Invariants are reusable quality checks that gate every task. Define them once,
reference them from any plan. An agent's work doesn't pass until all linked
invariants succeed.

### Register invariants manually

```bash
gator invariant add rust_build \
  --kind typecheck --command cargo --args "build,--workspace"

gator invariant add rust_test \
  --kind test_suite --command cargo --args "test,--workspace"

gator invariant add rust_clippy \
  --kind lint --command cargo --args "clippy,--workspace,--,--deny,warnings"
```

**Kinds:** `test_suite`, `typecheck`, `lint`, `coverage`, `custom`

**Options:**
- `--description` -- human-readable explanation
- `--expected-exit-code` -- what exit code means success (default: `0`)
- `--threshold` -- numeric threshold (e.g. coverage percentage)
- `--scope` -- `global` or `project` (default: `project`)
- `--timeout` -- seconds before the check is killed (default: `300`)

### Use presets instead

Gator ships with built-in presets for common project types. This is the fastest
path:

```bash
# See what's available
gator invariant presets list
gator invariant presets list --project-type rust

# Auto-detect project type and register matching presets
gator invariant presets install
```

Supported project types: **rust**, **node**, **python**, **go**. Detection is
based on marker files (`Cargo.toml`, `package.json`, `pyproject.toml`/`setup.py`,
`go.mod`).

Preset invariants for Rust:

| Name | Kind | Command |
|------|------|---------|
| `rust_build` | typecheck | `cargo build --workspace` |
| `rust_test` | test_suite | `cargo test --workspace` |
| `rust_clippy` | lint | `cargo clippy --workspace -- -D warnings` |
| `rust_fmt_check` | lint | `cargo fmt --all -- --check` |

### Verify an invariant works

```bash
gator invariant test rust_build
```

Runs the invariant in the current directory and shows the result. Always do
this before using an invariant in a plan -- a failing invariant will block
every task that references it.

### List and manage

```bash
gator invariant list           # all invariants
gator invariant list --verbose # with full details
```

## 3. Writing Plans

A plan is a TOML file that decomposes a feature into tasks with dependencies,
scope, gate policy, and invariant references. Gator respects the dependency
DAG -- independent tasks run in parallel, dependent tasks wait.

### Three ways to create a plan

**Scaffold with presets:**
```bash
gator plan init my-feature
```
Auto-detects project type, creates `my-feature.toml` with sensible defaults,
and registers preset invariants. Use `--no-register` to skip database
registration, `--project-type rust` to override detection.

**Generate with Claude:**
```bash
gator plan generate "add user authentication with JWT"
```
Spawns Claude Code to decompose the description into a plan TOML. Outputs to
`plan.toml` by default (`-o` to change). Use `--dry-run` to see the prompt
without spawning. Omit the description to run interactively.

**Write by hand:**
Create a `.toml` file following the format below.

### Plan format

```toml
[plan]
name = "Add user authentication"
base_branch = "main"
token_budget = 500000              # optional, omit for unlimited
default_harness = "claude-code"    # optional, this is the default
isolation = "worktree"             # or "container"
container_image = "ubuntu:24.04"   # required when isolation = "container"

[[tasks]]
name = "define-types"
description = """
Create src/auth/types.rs with UserId, HashedPassword, Claims, AuthError.
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
```

**Required task fields:** `name`, `description`, `scope`, `gate`, `invariants`

**Optional task fields:**
- `depends_on` -- list of task names this task waits for
- `retry_max` -- max retries before escalation (default: `3`)
- `harness` -- override the plan's default harness for this task

### Scope and gate policy

| Scope | Meaning | Recommended gate |
|-------|---------|-----------------|
| `narrow` | Touches 1-2 files, well-defined | `auto` |
| `medium` | Crosses module boundaries | `human_review` |
| `broad` | Architectural changes | `human_approve` |

- **`auto`** -- invariants pass = task passes. No human in the loop.
- **`human_review`** -- invariants run, then you review results and approve/reject.
- **`human_approve`** -- same flow, but signals the change warrants explicit sign-off.

### Tips for writing good plans

**Task descriptions are prompts.** The description is literally what the agent
sees. Be specific: name files, functions, types, and expected behavior. Include
acceptance criteria. The more precise the description, the better the output.

**Narrow scope = higher success rate.** Tasks scoped to 1-2 files with a
clear deliverable succeed on the first attempt far more often than broad tasks.
When in doubt, decompose further.

**Use the DAG.** Put foundational work (types, interfaces, schemas) in early
tasks with no dependencies. Implementation tasks depend on those. Integration
tasks depend on implementation. This gives agents a solid base to build on.

**Reference invariants liberally.** Every task should have at least one
invariant. A task with zero invariants cannot be approved (`gator plan approve`
rejects plans with unlinked tasks).

### Expect to refine plans

Generated plans almost always need human refinement. During dogfooding, `plan
generate` produced over-engineered designs (trait abstractions, dual backends)
when we wanted a straight replacement. Even hand-written plans benefit from
review -- ambiguous task descriptions lead to agents guessing, and missing
invariants leave tasks ungated.

After generating or writing a plan:

1. **Read every task description as if you were the agent.** Is there enough
   context to complete the work without asking questions? Are file paths,
   function signatures, and edge cases specified?
2. **Pin design decisions.** If a task offers multiple approaches ("you could
   use X or Y"), pick one. Agents perform better with a single clear direction.
3. **Check invariant coverage.** Early tasks that can't pass `rust_test` yet
   (because downstream code hasn't changed) should use lighter invariants like
   `rust_fmt_check`. Tasks near the end of the DAG should include the full
   suite.
4. **Watch for scope creep.** A `medium` task touching 16 files is really
   `broad`. Consider splitting it.

### Validate before importing

```bash
gator plan validate my-feature.toml
```

Checks structure, task name uniqueness, dependency references, DAG acyclicity,
and scope/gate values -- without touching the database. Fix errors here before
importing.

## 4. Importing and Approving Plans

### Import

```bash
gator plan create my-feature.toml
```

Parses the TOML, inserts the plan and tasks into the database, and links
invariants by name. Warns if referenced invariants don't exist yet (you can
add them before approving). Returns the plan UUID.

The UUID is also written back into your TOML file (as `uuid = "..."` in the
`[plan]` section), so you can reference the plan by file path in subsequent
commands:

```bash
gator plan show my-feature.toml    # same as gator plan show <uuid>
```

### Review

```bash
gator plan show                    # list all plans
gator plan show <plan-id>          # full details: tasks, deps, invariants
```

### Approve

```bash
gator plan approve <plan-id>
```

Transitions from `draft` to `approved`. Rejects the plan if any task has
zero linked invariants.

### Export

```bash
gator plan export <plan-id>              # to stdout
gator plan export <plan-id> -o plan.toml # to file
```

Materializes the plan from the database back to TOML.

## 5. Dispatching

```bash
gator dispatch <plan-id>
gator dispatch <plan-id> --max-agents 4    # default concurrency
gator dispatch <plan-id> --timeout 1800    # per-task timeout in seconds
```

Gator walks the DAG and assigns tasks to agents as their dependencies are
satisfied. For each task it:

1. Creates an isolated workspace (git worktree or container)
2. Generates a scoped HMAC token for the agent
3. Materializes the task description
4. Spawns the agent via the configured harness
5. Streams events with timeout
6. Runs gate checks (all linked invariants)
7. Evaluates the verdict based on gate policy
8. Transitions the task state

**Isolation modes:**
- **Worktree** (default): each task gets a git worktree branched from
  `base_branch`. Fast, lightweight, works everywhere.
- **Container**: each task runs in a Docker container. Use when you need
  a clean environment or different OS. Requires `container_image` in the plan.

## 6. Monitoring

### One-shot status

```bash
gator status                 # list all plans with status
gator status <plan-id>       # per-task breakdown
```

### Interactive dashboard

```bash
gator dashboard
```

TUI with live updates. Shows plans, tasks, status, and progress.

### Event log

```bash
gator log <task-id>              # current attempt
gator log <task-id> --attempt 2  # specific attempt
```

Shows the agent's event stream for debugging.

### HTTP API

```bash
gator serve                        # localhost:3000
gator serve --port 8080 --bind 0.0.0.0
```

Read-only HTTP API for integrations and custom dashboards.

### Reports

```bash
gator report <plan-id>              # token usage and duration
gator export csv <plan-id>          # task data as CSV
gator export csv                    # all plans
```

## 7. Reviewing and Approving Tasks

Tasks with `gate = "human_review"` or `gate = "human_approve"` pause after
gate checks run. You review the results and decide.

### View gate results

```bash
gator gate <task-id>
```

Shows each invariant's pass/fail status, exit code, and output snippets.

### Approve or reject

```bash
gator approve <task-id>    # task passes, continues the DAG
gator reject <task-id>     # task fails, triggers retry or escalation
```

### Retry

```bash
gator retry <task-id>          # retry a failed task (increments attempt)
gator retry <task-id> --force  # override the retry limit
```

## 8. Handling Failures

### Task failure and retry

When a task fails (invariants don't pass), gator automatically retries up to
`retry_max` times (default: 3). Each retry:
- Increments the attempt counter
- Resets the workspace
- Spawns a fresh agent

After exhausting retries, the task moves to `escalated` status and the
orchestrator stops dispatching tasks that depend on it.

### Operator override for escalated tasks

```bash
gator retry <task-id> --force
```

Resets the task to `pending` with an incremented attempt counter. The
orchestrator picks it up again on the next dispatch.

### Plan failure and reset

If a plan enters `failed` status (unrecoverable task failures), you can
reset it:

```bash
gator plan reset <plan-id>
```

This resets the plan to `approved` and all non-passed tasks to `pending`.
Tasks that already passed are left alone. You can then re-dispatch.

### Task state machine

```
pending --> assigned --> running --> checking --> passed
                                       |
                                       +--> failed --> assigned (retry)
                                       |           \-> escalated --> pending (operator override)
```

## 9. Merging and Shipping

Once all tasks pass:

### Merge task branches

```bash
gator merge <plan-id>           # merge all task branches into base_branch
gator merge <plan-id> --dry-run # preview without merging
```

### Create a PR

```bash
gator pr <plan-id>
gator pr <plan-id> --draft          # draft PR
gator pr <plan-id> --base develop   # override base branch
```

### Clean up worktrees

```bash
gator cleanup <plan-id>       # remove worktrees for passed tasks
gator cleanup <plan-id> --all # remove all worktrees
```

## 10. Agent Mode

When gator dispatches a task, it injects `GATOR_AGENT_TOKEN` into the agent's
environment. This restricts the agent to four commands:

| Command | What it does |
|---------|-------------|
| `gator task` | Read the assigned task description |
| `gator check` | Run all linked invariants |
| `gator progress "msg"` | Record a progress event |
| `gator done` | Signal task completion |

Agents never see operator commands. The token is scoped to exactly one
(task, attempt) pair and is HMAC-SHA256 signed. Gator generates and injects
tokens automatically -- agents don't create them.

## Shell Completions

```bash
# Generate and install (fish example)
gator completions fish > ~/.config/fish/completions/gator.fish

# Other shells
gator completions bash > /etc/bash_completion.d/gator
gator completions zsh > ~/.zsh/completions/_gator
```

## End-to-End Example

Here's the complete flow for adding a feature to a Rust project:

```bash
# Setup (once)
gator init --db-url postgresql://localhost:5432/gator
gator db-init
gator invariant presets install  # auto-detect Rust, register presets

# Create a plan
gator plan generate "add rate limiting middleware to the API"
# Edit the generated plan.toml if needed
gator plan validate plan.toml
gator plan create plan.toml

# Dispatch
gator plan approve plan.toml
gator dispatch plan.toml --max-agents 4

# Monitor (in another terminal)
gator dashboard

# Review human_review tasks as they complete
gator gate <task-id>
gator approve <task-id>

# Ship
gator merge plan.toml
gator pr plan.toml
gator cleanup plan.toml
```
