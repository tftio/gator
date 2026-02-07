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
