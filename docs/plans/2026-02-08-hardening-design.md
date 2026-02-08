# Hardening: Graceful Shutdown, Invariant Timeouts, Crash Recovery

> **Status**: Approved
> **Date**: 2026-02-08

## Context

Phases 0-5 are complete. The system works end-to-end but lacks production robustness in four areas identified by audit.

## H1: Graceful Shutdown

**Problem**: No signal handling during `gator dispatch`. Ctrl+C kills the process, orphaning in-flight agents. Tasks recover on next run via restart recovery, but agents keep running as zombie processes.

**Design**:

- Add `tokio_util` dependency for `CancellationToken`.
- `dispatch_cmd.rs` creates a `CancellationToken`, spawns a background task that listens for `tokio::signal::ctrl_c()` and Unix SIGTERM via `tokio::signal::unix::signal(SignalKind::terminate())`. On first signal: cancel the token. On second signal: `process::exit(130)`.
- `run_orchestrator` accepts `cancel: CancellationToken` parameter. The main loop adds a `select!` branch on `cancel.cancelled()`. When triggered:
  1. Stop spawning new tasks.
  2. SIGTERM all in-flight agents via `harness.kill()`.
  3. Wait up to 10 seconds for lifecycle results to drain.
  4. Mark any remaining in-flight tasks as `failed` in DB.
  5. Return `OrchestratorResult::Interrupted`.
- `dispatch_cmd.rs` handles `Interrupted` with exit code 130, prints summary.

**Files**: `Cargo.toml` (workspace), `gator-core/Cargo.toml`, `orchestrator/mod.rs`, `dispatch_cmd.rs`, `main.rs`.

## H2: Invariant Timeouts

**Problem**: `run_invariant` waits indefinitely. A hung `cargo test` blocks gate checks forever.

**Design**:

- Migration `004_invariant_timeout.sql`: `ALTER TABLE invariants ADD COLUMN timeout_secs INTEGER NOT NULL DEFAULT 300;`
- Add `timeout_secs: i32` to `Invariant` model.
- `run_invariant` wraps subprocess execution in `tokio::time::timeout()`. On timeout: kill child, return `InvariantResult { passed: false }` with "timed out after Xs" in stderr.
- `gator invariant add` gets `--timeout <secs>` flag (default 300).
- `insert_invariant` and `NewInvariant` updated to include timeout_secs.

**Files**: `004_invariant_timeout.sql` (new), `models.rs`, `queries/invariants.rs`, `invariant/runner.rs`, `invariant_cmds.rs`.

## H3: TUI Panic Recovery

**Problem**: If the TUI panics during rendering, the terminal is left in raw mode with the alternate screen active.

**Design**:

- At the top of `run_dashboard()`, before `enable_raw_mode()`, install a panic hook that restores the terminal.
- Chain with the existing panic hook so the panic message still prints.

**Files**: `tui/mod.rs`.

## H4: Remove `.expect()` from Orchestrator Spawn

**Problem**: `orchestrator/mod.rs` line 287 has `.expect("harness should be registered")` inside `tokio::spawn`. A panic here silently kills the spawned task.

**Design**:

- Replace `.expect()` with a match. On `None`, log an error and send a failure result through the channel. This is defensive -- the harness is validated before spawn -- but panics inside `tokio::spawn` are unacceptable.

**Files**: `orchestrator/mod.rs`.

## Dependencies

None between tasks. All four can be implemented in any order.

## Verification

1. `cargo build --workspace`
2. `cargo test --workspace`
3. `cargo clippy --workspace -- -D warnings`
