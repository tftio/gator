# gator development TODO

Track observations, bugs, and improvement ideas discovered during development
and dogfooding. Each item has context (when/how it was found), a description,
and a status checkbox (unchecked = open).

## Items

- [ ] **Token usage events report placeholder values**
  *Context:* Dogfooding `gator plan generate` (2026-02-16). Dashboard showed
  `token_usage {"input_tokens":1,"output_tokens":1}` for events that clearly
  consumed more tokens.
  *TODO:* Investigate the harness event stream -- the Claude Code harness may
  not be extracting real token counts from the API response, or the event
  payload schema expects different fields.

- [ ] **Dashboard shows "running" after task transitions to "checking"**
  *Context:* Dogfooding `gator plan generate` (2026-02-16). The dashboard showed
  the write-plan task as "running" even after the agent completed and the task
  moved to `checking` (confirmed via `gator status`).
  *TODO:* Check dashboard refresh interval and whether the TUI polls task status
  frequently enough to catch state transitions promptly.

- [ ] **Plan status stuck at "running" after all tasks pass**
  *Context:* Dogfooding `gator plan generate` (2026-02-16). After approving the
  write-plan task (1/1 passed), `gator status` still shows the meta-plan as
  `running` instead of `completed`.
  *TODO:* The orchestrator or the plan-generate command path may not be
  transitioning the plan status to `completed` when all tasks pass. Check
  `orchestrator/mod.rs` completion detection and `plan generate` exit path.

- [ ] **`plan generate` lacks architectural context from user decisions**
  *Context:* Dogfooding `gator plan generate` (2026-02-16). We had decided to
  replace PostgreSQL entirely with SQLite (no dual backend). The generated plan
  instead created a trait abstraction with both backends -- a much more complex
  approach we explicitly rejected.
  *TODO:* Consider how `plan generate` can accept additional context (e.g. a
  design doc, prior conversation notes, or explicit constraints like "no trait
  abstraction, full replacement"). The `--file` flag reads a description from a
  file, but richer architectural context would produce better plans.

- [ ] **Wire up `gator plan refine` for iterative plan editing**
  *Context:* Dogfooding session (2026-02-16). Interactive `plan generate` is
  single-session only -- no way to resume a conversation to refine an existing
  plan. The `Harness::send()` method and `AgentHandle` stdin are designed for
  this but not yet implemented (T013).
  *TODO:* Add `gator plan refine <plan-file-or-id>` that spawns Claude Code in
  interactive mode with the existing plan TOML loaded as context. Two modes:
  (1) `--prompt "constraint text"` for a directed single-round refinement,
  (2) no args for open-ended dialectic refinement. Should use `--resume` or
  feed the plan via stdin to the agent session. Depends on wiring up
  `ClaudeCodeAdapter::send()`.

- [ ] **Integrate `prompter` for composable prompt management**
  *Context:* Exploring prompt architecture (2026-02-16). `build_system_prompt()`
  in `generate.rs` assembles prompts by concatenating hardcoded `const` strings
  (`SCHEMA_REFERENCE`, `DECOMPOSITION_GUIDELINES`) plus dynamic project context.
  This is exactly the problem `prompter` (~/Projects/rust/prompter) solves --
  composable, deduplicated prompt fragments with dependency resolution and
  validation.
  *TODO:* Replace hardcoded prompt assembly with `prompter` profiles. The plan
  generation prompt (schema ref, decomposition guidelines, role description) and
  task materialization context become fragments in a prompter library. Two
  integration paths: (1) subprocess -- shell out to `prompter run <profile>
  --json` and parse structured output, simpler and decoupled; (2) library --
  add `prompter` as a dependency to `gator-core`, call `render_to_writer()`
  directly, tighter but needs upstream changes for custom config/library paths
  (currently hardcoded to XDG defaults). Also subsumes the "lacks architectural
  context" item above -- user constraints become additional profile fragments
  composed into the generation prompt.

- [ ] **Accept names as well as UUIDs for tasks, plans, and invariants**
  *Context:* Dogfooding dispatch (2026-02-16). `gator gate rewrite-gator-db`
  rejected the task name with "invalid character: expected an optional prefix
  of `urn:uuid:`". Had to look up the UUID via `gator plan show`.
  *TODO:* All commands that accept a plan ID, task ID, or invariant ID should
  also accept the human-readable name. Resolve by querying the database for a
  matching name when the argument does not parse as a UUID. Error if the name
  is ambiguous (multiple matches). File paths (plan.toml) already work for
  plans; extend the same pattern to tasks and invariants.

- [ ] **Add crate-scoped invariants to Rust presets**
  *Context:* Dogfooding dispatch (2026-02-16). Workspace-wide invariants
  (`cargo build --workspace`) fail on intermediate tasks that only change one
  crate, because other crates still have pre-migration code. Had to manually
  register 8 crate-scoped invariants (`rust_build_db`, `rust_clippy_db`, etc.)
  to gate each task against its own crate only.
  *TODO:* The Rust preset (`gator invariant presets install`) should detect
  workspace members from `Cargo.toml` and auto-generate per-crate invariants
  (e.g. `rust_build_{crate}`, `rust_clippy_{crate}`) alongside the existing
  workspace-wide ones. Plans that change one crate at a time can then reference
  crate-scoped gates without manual registration.

- [ ] **Orchestrator deadlock vs pause behavior is inconsistent**
  *Context:* Dogfooding dispatch (2026-02-16). When tasks are waiting for human
  review, the orchestrator sometimes reports "plan deadlocked: pending tasks
  blocked by escalated dependencies" and fails the plan (observed with
  `rewrite-gator-db`, `update-gator-core`, `update-cli-config`), and other
  times correctly prints "Plan paused -- tasks awaiting human review" with
  instructions (observed with `update-documentation`). The difference may
  depend on whether other pending tasks exist beyond the human-review blocker.
  *TODO:* (1) Deadlock detection must distinguish escalated/failed deps (actual
  deadlock) from human_review/human_approve gates (expected wait). Should never
  declare deadlock when the only blocker is human review. (2) The "paused"
  path currently exits with code 2, which callers treat as failure. Use exit
  code 0 for clean pause, or a distinct code (e.g. 75) that scripts can
  distinguish from actual errors.

- [ ] **Stale worktrees reused across plan recreations**
  *Context:* Dogfooding dispatch (2026-02-16). After deleting and re-creating a
  plan with the same name, the orchestrator reused existing worktrees from the
  old plan (branch names matched: `gator/replace-postgres-with-sqlite/update-
  gator-core`). The agent found "no changes to commit" and exited instantly
  because the worktree either had stale changes from a previous agent or none
  at all. Gates then ran against unmodified code and failed.
  *TODO:* When the lifecycle creates a worktree and finds an existing one with
  the same branch name, it should verify the worktree belongs to the current
  plan/task ID (not just matching branch names). Options: (1) include the plan
  UUID in the branch name to guarantee uniqueness, (2) reset the worktree to
  the base branch HEAD before spawning the agent, (3) delete and recreate
  worktrees that don't match the current task's plan ID. Option 2 is simplest
  and also handles retries cleanly.

- [ ] **Retries reuse stale worktrees without resetting**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` failed its
  gate (workspace-wide `rust_fmt_check`), then retried 3 more times. Each retry
  reused the same worktree without resetting it. The agent completed in ~50ms
  each time ("no changes to commit") because the worktree already had its
  changes. Same gate failure, same instant retry, burned through `retry_max=3`
  in under 2 seconds with zero chance of a different outcome.
  *TODO:* Before each retry, the lifecycle should reset the worktree to the
  base branch HEAD (with dependency branches merged in). This gives the agent a
  clean workspace to work from. Related to the stale worktree reuse bug above
  but specific to the retry path. Without a reset, retries are deterministically
  identical and always produce the same failure.

- [ ] **Auto-gated tasks fail on unrelated workspace-wide invariants**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` (auto gate)
  had passing crate-scoped invariants (`rust_build_cli`, `rust_clippy_cli`) but
  failed on `rust_fmt_check` (workspace-wide). The formatting issue was in other
  crates modified by earlier tasks, not in the files this task touched. The task
  exhausted all retries and escalated despite its actual work being correct.
  *TODO:* Consider whether auto-gated tasks should pass when all crate-scoped
  invariants pass and only workspace-wide invariants fail. Alternatively, the
  plan author should be warned at `plan create` time if a task mixes crate-
  scoped and workspace-scoped invariants, since intermediate tasks in a multi-
  crate migration will always fail the workspace-scoped ones. At minimum, the
  gate result output should clearly distinguish which invariants are the actual
  blockers.

- [ ] **Invariants need blocking vs advisory distinction and task-phase awareness**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` had passing
  crate-scoped invariants (`rust_build_cli`, `rust_clippy_cli`) but failed on
  workspace-wide `rust_fmt_check`. The fmt issue was in other crates -- not in
  files this task touched. Under `gate = "auto"`, all invariants are blocking,
  so the task escalated despite correct work. Had to remove the invariant via
  direct SQL to unblock the plan.
  *TODO:* Introduce invariant categories:
  - **blocking** (default): must pass for the gate to pass. Build and crate-
    scoped checks should be blocking.
  - **advisory**: reported in gate results but does not block. Workspace-wide
    formatting, coverage thresholds, etc. Operators see the result but the task
    can still auto-pass.
  Additionally, consider task-phase invariants:
  - **pre-flight**: run before the agent starts. Validates the worktree is in a
    known-good state (e.g. "does it compile before I touch it?").
  - **gate** (current behavior): run after the agent completes. Validates the
    agent's work.
  The plan TOML format could express this as:
  ```toml
  invariants = ["rust_build_cli", "rust_clippy_cli"]
  advisory_invariants = ["rust_fmt_check"]
  ```
  or with inline attributes:
  ```toml
  [[tasks.invariants]]
  name = "rust_fmt_check"
  blocking = false
  ```
  This is a significant schema change but would have prevented multiple failures
  in this dogfooding session.

- [ ] **`retry --force` resets attempt counter to 0, enabling infinite retry loops**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` escalated
  at attempt 3. After `gator retry --force`, the attempt counter reset to 0.
  The next dispatch burned through attempts 0-3 again with identical failures
  (stale worktree on retries 1-3). The operator can keep doing `retry --force`
  forever, getting the same result each time. Attempt counter should increment
  from where it left off, not restart. Or at minimum, the force-retry should
  produce a different outcome (by resetting the worktree).

- [ ] **Cannot approve escalated tasks; `retry --force` doesn't reset worktree**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` was
  escalated after exhausting retries. `gator approve <id>` rejected it with
  "task is escalated, must be checking to approve". Had to `gator retry --force`
  to reset to pending, then `gator plan reset` to un-fail the plan, then
  manually `git worktree remove` the stale worktree, then re-dispatch. Four
  manual steps to recover from a task that actually had correct code.
  *TODO:* Two issues: (1) `gator approve` should work on escalated tasks --
  the operator is explicitly overriding the gate verdict. (2) `gator retry
  --force` should offer to reset/remove the task's worktree so the next agent
  gets a clean workspace. Currently the worktree persists with stale state.

- [ ] **No way to edit task invariants on a live plan**
  *Context:* Dogfooding dispatch (2026-02-16). `update-cli-commands` kept
  failing on `rust_fmt_check` (workspace-wide, unrelated to the task's work).
  Needed to remove that invariant from just this task, but there's no command
  to edit a task's invariants after plan creation. Only options: (1) delete the
  plan, edit TOML, re-create (loses all progress on passed tasks), (2) direct
  SQL against `task_invariants` table.
  *TODO:* Add `gator plan edit-task <task-id> --remove-invariant <name>` or
  similar. Alternatively, `gator plan update plan.toml` that diffs the TOML
  against the database and applies safe changes (invariant additions/removals,
  description updates) without resetting task status.

- [ ] **`gator merge` needs a post-merge gate**
  *Context:* Dogfooding merge (2026-02-16). After merging all 9 task branches,
  `cargo build --workspace` failed with 9 remaining `PgPool` imports in
  gator-core. Each task's invariants ran in isolation in its worktree and
  passed (or were force-approved). But the merge combines branches that were
  never tested together -- cross-task integration issues, merge conflicts, and
  incomplete agent work all slip through.
  *TODO:* `gator merge` should run a configurable set of post-merge invariants
  after combining all branches. The plan TOML could specify these:
  ```toml
  [plan]
  merge_invariants = ["rust_build", "rust_test", "rust_clippy", "rust_fmt_check"]
  ```
  If any fail, the merge is reported as incomplete and the operator can fix up
  before proceeding to `gator pr`. This is the integration test layer that sits
  above per-task gates. Without it, `gator merge` is just `git merge` with no
  quality guarantee. Could also support a `--no-verify` flag to skip (like we
  had to do via SQL this session).

- [ ] **No `gator plan delete` command**
  *Context:* Dogfooding (2026-02-16). Wanted to remove a stale meta-plan from
  the database after regenerating with a better prompt. Only option is direct
  SQL (`DELETE FROM plans WHERE id = '...'`).
  *TODO:* Add `gator plan delete <plan-id>` with a confirmation prompt. Should
  cascade-delete tasks, gate results, and agent events (the FK constraints
  already handle this).
