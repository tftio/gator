# Gator: Initial Implementation Plan

> **Status**: Draft
> **Last updated**: 2026-02-07
> **Tracks progress in**: [docs/tasks.md](../tasks.md)

## 1. What Gator Is

Gator is a Rust CLI that orchestrates fleets of LLM coding agents. It sits above individual agent harnesses (Claude Code, Codex CLI, OpenCode) and manages the full lifecycle: plan, dispatch, execute, gate, merge.

**Core thesis**: As task scope narrows, automated invariants (tests, types, lint) are sufficient to judge agent work. As scope broadens, human attention is required. Gator manages this gradient. Agents never self-certify; gator is the authority.

## 2. Architecture

```
+--------------------------------------------------+
|  Frontends: gator CLI, gator-tui, CC plugin      |
+--------------------------------------------------+
|  Core (gator-core): Plans, tasks, scheduling,     |
|  invariants, state machine, harness adapters,      |
|  token generation                                  |
+--------------------------------------------------+
|  Database (gator-db): PostgreSQL 18                |
|  Single source of truth for all project state      |
+--------------------------------------------------+
|  Harness Adapters: Claude Code, Codex, OpenCode    |
|  (subprocess + JSONL, behind Harness trait)         |
+--------------------------------------------------+
```

### 2.1 Key Design Decisions

- **Language**: Rust. Single binary, strong concurrency via tokio, rock-solid.
- **Database**: PostgreSQL 18. Source of truth. Plan files are materializations from DB, not the authority.
- **Harness-agnostic**: `Harness` trait with subprocess+JSONL adapters. Claude Code first.
- **Isolation**: Git worktrees per agent. `Isolation` trait allows container-based isolation later.
- **Trust model**: Agents are untrusted. Modal CLI with scoped tokens restricts what agents can see and do.

### 2.2 Data Flow

```
Operator (human) ---> gator (operator mode) ---> PostgreSQL 18
                                                      |
                                          Materializes task spec
                                          + generates scoped token
                                                      |
                                                      v
                                          Agent (in worktree, agent mode)
                                          - gator task / check / progress / done
                                          - Cannot see plan, DB, other agents
                                                      |
                                          gator collects results
                                          Runs invariants independently
                                          Records verdict in DB
```

### 2.3 Trust Model

**Operator mode** (default): Full command surface. Reads/writes Postgres.

**Agent mode** (`GATOR_AGENT_TOKEN` set): Scoped to one (task_id, attempt) pair. Can only:
- `gator task` -- read own task description
- `gator check` -- run own invariants
- `gator progress "msg"` -- report progress
- `gator done` -- signal completion (does NOT approve)

Token format: `gator_at_<task_id>_<attempt>_<hmac>`

### 2.4 Scope-Driven Gating

| Scope | Example | Gate Policy | Human Involvement |
|-------|---------|-------------|-------------------|
| narrow | Implement a function | auto | None if invariants pass |
| medium | Build a feature | human_review | Human sees summary |
| broad | Architecture decision | human_approve | Human must approve |

### 2.5 Task State Machine

```
pending --> assigned --> running --> checking --> passed (merge-ready)
                                       |
                                       v
                                     failed --> retry or escalate
```

Each transition is a DB transaction with a log entry.

## 3. Database Schema

### 3.1 Tables

**plans**: Top-level unit of work.
- id, name, project_path, base_branch, status (draft/approved/running/completed/failed), timestamps

**tasks**: Units of work within a plan.
- id, plan_id (FK), name, description, scope_level, gate_policy, retry_max, status, assigned_harness, worktree_path, attempt, timestamps

**task_dependencies**: DAG edges.
- task_id (FK), depends_on (FK), composite PK

**invariants**: Reusable check definitions (n:m with tasks).
- id, name (unique), description, kind (test_suite/typecheck/lint/coverage/custom), command, args, expected_exit_code, threshold, scope

**task_invariants**: Join table.
- task_id (FK), invariant_id (FK), composite PK

**gate_results**: One per invariant per attempt.
- id, task_id (FK), invariant_id (FK), attempt, passed, exit_code, stdout, stderr, duration_ms, checked_at

**agent_events**: JSONL event log from agent streams.
- id (bigserial), task_id (FK), attempt, event_type, payload (jsonb), recorded_at

### 3.2 Full DDL

See task T002 for the complete CREATE TABLE statements with constraints and defaults.

## 4. CLI Command Surface

### 4.1 Operator Mode

```
gator init                        # Bootstrap database
gator plan create <file>          # Import plan.toml into DB
gator plan show [plan-id]         # Show plan status
gator plan approve [plan-id]      # Approve for execution

gator invariant add <name> ...    # Define reusable invariant
gator invariant list              # List library
gator invariant test <name>       # Dry-run

gator dispatch [plan-id]          # Assign tasks to agents
gator status                      # Fleet dashboard
gator log [task-id]               # Stream agent event log

gator gate [task-id]              # View/trigger gate
gator approve [task-id]           # Human approves
gator retry [task-id]             # Re-dispatch

gator merge [plan-id]             # Merge passed branches
gator report [plan-id]            # Analytics
```

### 4.2 Agent Mode

```
gator task                        # Read my task
gator check                       # Run my invariants
gator progress "message"          # Report progress
gator done                        # Signal completion
```

## 5. Harness Adapter

```rust
#[async_trait]
pub trait Harness: Send + Sync {
    async fn spawn(&self, worktree: &Path, task: &MaterializedTask) -> Result<AgentHandle>;
    fn events(&self, handle: &AgentHandle) -> Pin<Box<dyn Stream<Item = AgentEvent> + Send>>;
    async fn send(&self, handle: &AgentHandle, message: &str) -> Result<()>;
    async fn kill(&self, handle: &AgentHandle) -> Result<()>;
}
```

First adapter: `ClaudeCodeAdapter` -- spawns `claude -p --output-format stream-json`.

## 6. Implementation Phases

### Phase 0: Scaffold & DB
Create Cargo workspace, database schema, migrations, connection pooling, `gator init`.

**Completion**: `gator init` creates all tables in a fresh Postgres 18 instance. `cargo test` passes.

**Tasks**: T001, T002, T003

### Phase 1: Plan & Invariant Management
Parse plan.toml, CRUD for plans and invariants, plan materialization, CLI commands.

**Completion**: Round-trip test passes (plan.toml -> DB -> materialized plan.toml). All plan/invariant CLI commands work. `cargo test` passes.

**Tasks**: T004, T005, T006, T007, T008

### Phase 2: Single Agent Dispatch
Worktree management, scoped tokens, Harness trait, ClaudeCodeAdapter, agent-mode CLI, event streaming, gate runner, state machine.

**Completion**: Dispatch a single narrow-scope task to Claude Code. Agent works in worktree, runs invariants via `gator check`, signals `gator done`. Gator runs gate independently, records pass/fail. Scoped token rejects operator commands.

**Tasks**: T009, T010, T011, T012, T013, T014, T015, T016

### Phase 3: Fleet Orchestration
DAG scheduler, concurrent agents, retry-with-feedback, budget enforcement.

**Completion**: Dispatch a plan with 5+ tasks including dependencies. Tasks execute in correct topological order. Failed tasks retry. Budgets terminate runaways.

**Tasks**: To be planned after Phase 2 completion.

### Phase 4: TUI & Review Workflow
Terminal dashboard, human review queue, merge, reports.

**Tasks**: To be planned after Phase 3 completion.

### Phase 5: Additional Harnesses
Codex, OpenCode adapters. Claude Code plugin. Container isolation.

**Tasks**: To be planned after Phase 4 completion.

## 7. Dependency Graph (Phases 0-2)

```
T001 (workspace scaffold)
  |
  +---> T002 (DB schema + migrations)
  |       |
  |       +---> T003 (gator init + connection pool)
  |               |
  |               +---> T004 (plan types + TOML parser + DB CRUD)
  |               |       |
  |               |       +---> T005 (plan CLI: create, show)
  |               |       |       |
  |               |       |       +---> T008 (plan materialization)
  |               |       |
  |               |       +---> T007 (invariant CLI: add, list, test)
  |               |
  |               +---> T006 (invariant types + DB CRUD)
  |                       |
  |                       +---> T007
  |
  +---> T009 (worktree management) ---+
  |                                    |
  +---> T010 (scoped tokens) ---------+---> T014 (task state machine)
  |                                    |       |
  +---> T011 (Harness trait) ---------+       +---> T015 (gate runner)
          |                            |               |
          +---> T012 (ClaudeCode       |               +---> T016 (integration test)
                 adapter)              |
                  |                    |
                  +-----> T013 (agent-mode CLI)
                            |
                            +---> T016 (integration test)
```
