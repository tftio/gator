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

- [ ] **No `gator plan delete` command**
  *Context:* Dogfooding (2026-02-16). Wanted to remove a stale meta-plan from
  the database after regenerating with a better prompt. Only option is direct
  SQL (`DELETE FROM plans WHERE id = '...'`).
  *TODO:* Add `gator plan delete <plan-id>` with a confirmation prompt. Should
  cascade-delete tasks, gate results, and agent events (the FK constraints
  already handle this).
