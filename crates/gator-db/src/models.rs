use std::fmt;
use std::str::FromStr;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::FromRow;
use uuid::Uuid;

// ---------------------------------------------------------------------------
// Enums
// ---------------------------------------------------------------------------

/// Status of a plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Draft,
    Approved,
    Running,
    Completed,
    Failed,
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Draft => "draft",
            Self::Approved => "approved",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
        };
        f.write_str(s)
    }
}

impl FromStr for PlanStatus {
    type Err = PlanStatusParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "draft" => Ok(Self::Draft),
            "approved" => Ok(Self::Approved),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            other => Err(PlanStatusParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`PlanStatus`] string.
#[derive(Debug, Clone)]
pub struct PlanStatusParseError(pub String);

impl fmt::Display for PlanStatusParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid plan status: {:?}", self.0)
    }
}

impl std::error::Error for PlanStatusParseError {}

// ---------------------------------------------------------------------------

/// Status of a task.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    Pending,
    Assigned,
    Running,
    Checking,
    Passed,
    Failed,
    Escalated,
}

impl fmt::Display for TaskStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Pending => "pending",
            Self::Assigned => "assigned",
            Self::Running => "running",
            Self::Checking => "checking",
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Escalated => "escalated",
        };
        f.write_str(s)
    }
}

impl FromStr for TaskStatus {
    type Err = TaskStatusParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "pending" => Ok(Self::Pending),
            "assigned" => Ok(Self::Assigned),
            "running" => Ok(Self::Running),
            "checking" => Ok(Self::Checking),
            "passed" => Ok(Self::Passed),
            "failed" => Ok(Self::Failed),
            "escalated" => Ok(Self::Escalated),
            other => Err(TaskStatusParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`TaskStatus`] string.
#[derive(Debug, Clone)]
pub struct TaskStatusParseError(pub String);

impl fmt::Display for TaskStatusParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid task status: {:?}", self.0)
    }
}

impl std::error::Error for TaskStatusParseError {}

// ---------------------------------------------------------------------------

/// Scope level of a task -- determines the gating strategy.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum ScopeLevel {
    Narrow,
    Medium,
    Broad,
}

impl fmt::Display for ScopeLevel {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Narrow => "narrow",
            Self::Medium => "medium",
            Self::Broad => "broad",
        };
        f.write_str(s)
    }
}

impl FromStr for ScopeLevel {
    type Err = ScopeLevelParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "narrow" => Ok(Self::Narrow),
            "medium" => Ok(Self::Medium),
            "broad" => Ok(Self::Broad),
            other => Err(ScopeLevelParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`ScopeLevel`] string.
#[derive(Debug, Clone)]
pub struct ScopeLevelParseError(pub String);

impl fmt::Display for ScopeLevelParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid scope level: {:?}", self.0)
    }
}

impl std::error::Error for ScopeLevelParseError {}

// ---------------------------------------------------------------------------

/// Gate policy that determines how a task's completion is verified.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum GatePolicy {
    Auto,
    HumanReview,
    HumanApprove,
}

impl fmt::Display for GatePolicy {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Auto => "auto",
            Self::HumanReview => "human_review",
            Self::HumanApprove => "human_approve",
        };
        f.write_str(s)
    }
}

impl FromStr for GatePolicy {
    type Err = GatePolicyParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "auto" => Ok(Self::Auto),
            "human_review" => Ok(Self::HumanReview),
            "human_approve" => Ok(Self::HumanApprove),
            other => Err(GatePolicyParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`GatePolicy`] string.
#[derive(Debug, Clone)]
pub struct GatePolicyParseError(pub String);

impl fmt::Display for GatePolicyParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid gate policy: {:?}", self.0)
    }
}

impl std::error::Error for GatePolicyParseError {}

// ---------------------------------------------------------------------------

/// Kind of invariant check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum InvariantKind {
    TestSuite,
    Typecheck,
    Lint,
    Coverage,
    Custom,
}

impl fmt::Display for InvariantKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::TestSuite => "test_suite",
            Self::Typecheck => "typecheck",
            Self::Lint => "lint",
            Self::Coverage => "coverage",
            Self::Custom => "custom",
        };
        f.write_str(s)
    }
}

impl FromStr for InvariantKind {
    type Err = InvariantKindParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "test_suite" => Ok(Self::TestSuite),
            "typecheck" => Ok(Self::Typecheck),
            "lint" => Ok(Self::Lint),
            "coverage" => Ok(Self::Coverage),
            "custom" => Ok(Self::Custom),
            other => Err(InvariantKindParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`InvariantKind`] string.
#[derive(Debug, Clone)]
pub struct InvariantKindParseError(pub String);

impl fmt::Display for InvariantKindParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid invariant kind: {:?}", self.0)
    }
}

impl std::error::Error for InvariantKindParseError {}

// ---------------------------------------------------------------------------

/// Scope of an invariant -- global or project-level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, sqlx::Type)]
#[sqlx(type_name = "text", rename_all = "snake_case")]
#[serde(rename_all = "snake_case")]
pub enum InvariantScope {
    Global,
    Project,
}

impl fmt::Display for InvariantScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::Global => "global",
            Self::Project => "project",
        };
        f.write_str(s)
    }
}

impl FromStr for InvariantScope {
    type Err = InvariantScopeParseError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "global" => Ok(Self::Global),
            "project" => Ok(Self::Project),
            other => Err(InvariantScopeParseError(other.to_owned())),
        }
    }
}

/// Error returned when parsing an invalid [`InvariantScope`] string.
#[derive(Debug, Clone)]
pub struct InvariantScopeParseError(pub String);

impl fmt::Display for InvariantScopeParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "invalid invariant scope: {:?}", self.0)
    }
}

impl std::error::Error for InvariantScopeParseError {}

// ---------------------------------------------------------------------------
// Row structs
// ---------------------------------------------------------------------------

/// A plan -- the top-level unit of work.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Plan {
    pub id: Uuid,
    pub name: String,
    pub project_path: String,
    pub base_branch: String,
    pub status: PlanStatus,
    pub token_budget: Option<i64>,
    pub created_at: DateTime<Utc>,
    pub approved_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// A task -- a unit of work within a plan.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Task {
    pub id: Uuid,
    pub plan_id: Uuid,
    pub name: String,
    pub description: String,
    pub scope_level: ScopeLevel,
    pub gate_policy: GatePolicy,
    pub retry_max: i32,
    pub status: TaskStatus,
    pub assigned_harness: Option<String>,
    pub worktree_path: Option<String>,
    pub attempt: i32,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
}

/// An edge in the task dependency DAG.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TaskDependency {
    pub task_id: Uuid,
    pub depends_on: Uuid,
}

/// A reusable invariant definition.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct Invariant {
    pub id: Uuid,
    pub name: String,
    pub description: Option<String>,
    pub kind: InvariantKind,
    pub command: String,
    pub args: Vec<String>,
    pub expected_exit_code: i32,
    pub threshold: Option<f32>,
    pub scope: InvariantScope,
    pub created_at: DateTime<Utc>,
}

/// Join row linking a task to an invariant.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct TaskInvariant {
    pub task_id: Uuid,
    pub invariant_id: Uuid,
}

/// Result of running an invariant gate check.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct GateResult {
    pub id: Uuid,
    pub task_id: Uuid,
    pub invariant_id: Uuid,
    pub attempt: i32,
    pub passed: bool,
    pub exit_code: Option<i32>,
    pub stdout: Option<String>,
    pub stderr: Option<String>,
    pub duration_ms: Option<i32>,
    pub checked_at: DateTime<Utc>,
}

/// An event recorded from an agent's execution stream.
#[derive(Debug, Clone, Serialize, Deserialize, FromRow)]
pub struct AgentEvent {
    pub id: i64,
    pub task_id: Uuid,
    pub attempt: i32,
    pub event_type: String,
    pub payload: serde_json::Value,
    pub recorded_at: DateTime<Utc>,
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_status_display_roundtrip() {
        let variants = [
            PlanStatus::Draft,
            PlanStatus::Approved,
            PlanStatus::Running,
            PlanStatus::Completed,
            PlanStatus::Failed,
        ];
        for v in &variants {
            let s = v.to_string();
            let parsed: PlanStatus = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn plan_status_invalid() {
        let result = "bogus".parse::<PlanStatus>();
        assert!(result.is_err());
    }

    #[test]
    fn task_status_display_roundtrip() {
        let variants = [
            TaskStatus::Pending,
            TaskStatus::Assigned,
            TaskStatus::Running,
            TaskStatus::Checking,
            TaskStatus::Passed,
            TaskStatus::Failed,
            TaskStatus::Escalated,
        ];
        for v in &variants {
            let s = v.to_string();
            let parsed: TaskStatus = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn task_status_invalid() {
        let result = "nope".parse::<TaskStatus>();
        assert!(result.is_err());
    }

    #[test]
    fn scope_level_display_roundtrip() {
        let variants = [ScopeLevel::Narrow, ScopeLevel::Medium, ScopeLevel::Broad];
        for v in &variants {
            let s = v.to_string();
            let parsed: ScopeLevel = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn scope_level_invalid() {
        let result = "tiny".parse::<ScopeLevel>();
        assert!(result.is_err());
    }

    #[test]
    fn gate_policy_display_roundtrip() {
        let variants = [
            GatePolicy::Auto,
            GatePolicy::HumanReview,
            GatePolicy::HumanApprove,
        ];
        for v in &variants {
            let s = v.to_string();
            let parsed: GatePolicy = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn gate_policy_invalid() {
        let result = "robot".parse::<GatePolicy>();
        assert!(result.is_err());
    }

    #[test]
    fn invariant_kind_display_roundtrip() {
        let variants = [
            InvariantKind::TestSuite,
            InvariantKind::Typecheck,
            InvariantKind::Lint,
            InvariantKind::Coverage,
            InvariantKind::Custom,
        ];
        for v in &variants {
            let s = v.to_string();
            let parsed: InvariantKind = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn invariant_kind_invalid() {
        let result = "magic".parse::<InvariantKind>();
        assert!(result.is_err());
    }

    #[test]
    fn invariant_scope_display_roundtrip() {
        let variants = [InvariantScope::Global, InvariantScope::Project];
        for v in &variants {
            let s = v.to_string();
            let parsed: InvariantScope = s.parse().expect("should parse");
            assert_eq!(*v, parsed);
        }
    }

    #[test]
    fn invariant_scope_invalid() {
        let result = "local".parse::<InvariantScope>();
        assert!(result.is_err());
    }
}
