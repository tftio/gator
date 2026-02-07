//! Database query functions for the `agent_events` table.

use anyhow::{Context, Result};
use serde_json::Value;
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::AgentEvent;

/// Parameters for inserting a new agent event row.
#[derive(Debug, Clone)]
pub struct NewAgentEvent {
    pub task_id: Uuid,
    pub attempt: i32,
    pub event_type: String,
    pub payload: Value,
}

/// Insert a new agent event row. Returns the inserted row with
/// server-generated defaults (id, recorded_at).
pub async fn insert_agent_event(pool: &PgPool, new: &NewAgentEvent) -> Result<AgentEvent> {
    let event = sqlx::query_as::<_, AgentEvent>(
        "INSERT INTO agent_events (task_id, attempt, event_type, payload) \
         VALUES ($1, $2, $3, $4) \
         RETURNING *",
    )
    .bind(new.task_id)
    .bind(new.attempt)
    .bind(&new.event_type)
    .bind(&new.payload)
    .fetch_one(pool)
    .await
    .with_context(|| {
        format!(
            "failed to insert agent event for task {} attempt {} type {}",
            new.task_id, new.attempt, new.event_type
        )
    })?;

    Ok(event)
}

/// Get all agent events for a given task and attempt, ordered by
/// recorded_at ASC.
pub async fn list_events_for_task(
    pool: &PgPool,
    task_id: Uuid,
    attempt: i32,
) -> Result<Vec<AgentEvent>> {
    let events = sqlx::query_as::<_, AgentEvent>(
        "SELECT * FROM agent_events \
         WHERE task_id = $1 AND attempt = $2 \
         ORDER BY recorded_at ASC",
    )
    .bind(task_id)
    .bind(attempt)
    .fetch_all(pool)
    .await
    .with_context(|| {
        format!(
            "failed to list agent events for task {} attempt {}",
            task_id, attempt
        )
    })?;

    Ok(events)
}

/// Get all agent events for a given task across all attempts, ordered by
/// attempt ASC then recorded_at ASC.
pub async fn list_all_events_for_task(
    pool: &PgPool,
    task_id: Uuid,
) -> Result<Vec<AgentEvent>> {
    let events = sqlx::query_as::<_, AgentEvent>(
        "SELECT * FROM agent_events \
         WHERE task_id = $1 \
         ORDER BY attempt ASC, recorded_at ASC",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .with_context(|| {
        format!(
            "failed to list all agent events for task {}",
            task_id
        )
    })?;

    Ok(events)
}

/// Count the number of agent events for a given task and attempt.
pub async fn count_events_for_task(
    pool: &PgPool,
    task_id: Uuid,
    attempt: i32,
) -> Result<i64> {
    let row: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM agent_events \
         WHERE task_id = $1 AND attempt = $2",
    )
    .bind(task_id)
    .bind(attempt)
    .fetch_one(pool)
    .await
    .with_context(|| {
        format!(
            "failed to count agent events for task {} attempt {}",
            task_id, attempt
        )
    })?;

    Ok(row.0)
}
