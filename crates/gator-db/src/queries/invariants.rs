//! Database query functions for the `invariants` table.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use crate::models::{Invariant, InvariantKind, InvariantScope};

/// Parameters for inserting a new invariant row.
#[derive(Debug, Clone)]
pub struct NewInvariant<'a> {
    pub name: &'a str,
    pub description: Option<&'a str>,
    pub kind: InvariantKind,
    pub command: &'a str,
    pub args: &'a [String],
    pub expected_exit_code: i32,
    pub threshold: Option<f32>,
    pub scope: InvariantScope,
}

/// Insert a new invariant. Returns the inserted row with server-generated
/// defaults (id, created_at).
///
/// If an invariant with the same name already exists, the insert is rejected
/// via the UNIQUE constraint and an error is returned.
pub async fn insert_invariant(pool: &PgPool, new: &NewInvariant<'_>) -> Result<Invariant> {
    let invariant = sqlx::query_as::<_, Invariant>(
        "INSERT INTO invariants (name, description, kind, command, args, \
         expected_exit_code, threshold, scope) \
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8) \
         RETURNING *",
    )
    .bind(new.name)
    .bind(new.description)
    .bind(new.kind)
    .bind(new.command)
    .bind(new.args)
    .bind(new.expected_exit_code)
    .bind(new.threshold)
    .bind(new.scope)
    .fetch_one(pool)
    .await
    .with_context(|| format!("failed to insert invariant {:?}", new.name))?;

    Ok(invariant)
}

/// Fetch an invariant by its UUID.
pub async fn get_invariant(pool: &PgPool, id: Uuid) -> Result<Option<Invariant>> {
    let invariant = sqlx::query_as::<_, Invariant>(
        "SELECT * FROM invariants WHERE id = $1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .context("failed to fetch invariant")?;

    Ok(invariant)
}

/// Fetch an invariant by its unique name.
pub async fn get_invariant_by_name(pool: &PgPool, name: &str) -> Result<Option<Invariant>> {
    let invariant = sqlx::query_as::<_, Invariant>(
        "SELECT * FROM invariants WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await
    .with_context(|| format!("failed to fetch invariant by name {:?}", name))?;

    Ok(invariant)
}

/// List all invariants, ordered by name.
pub async fn list_invariants(pool: &PgPool) -> Result<Vec<Invariant>> {
    let invariants = sqlx::query_as::<_, Invariant>(
        "SELECT * FROM invariants ORDER BY name",
    )
    .fetch_all(pool)
    .await
    .context("failed to list invariants")?;

    Ok(invariants)
}

/// Delete an invariant by its UUID.
///
/// This will fail if the invariant is still linked to any tasks via the
/// `task_invariants` table (foreign key constraint prevents orphaned
/// references).
pub async fn delete_invariant(pool: &PgPool, id: Uuid) -> Result<()> {
    // Check whether the invariant is linked to any tasks.
    let linked: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM task_invariants WHERE invariant_id = $1",
    )
    .bind(id)
    .fetch_one(pool)
    .await
    .context("failed to check invariant task links")?;

    if linked.0 > 0 {
        anyhow::bail!(
            "cannot delete invariant {id}: it is linked to {} task(s)",
            linked.0,
        );
    }

    let result = sqlx::query("DELETE FROM invariants WHERE id = $1")
        .bind(id)
        .execute(pool)
        .await
        .context("failed to delete invariant")?;

    if result.rows_affected() == 0 {
        anyhow::bail!("invariant {id} not found");
    }

    Ok(())
}

/// Get all invariants linked to a given task.
pub async fn get_invariants_for_task(pool: &PgPool, task_id: Uuid) -> Result<Vec<Invariant>> {
    let invariants = sqlx::query_as::<_, Invariant>(
        "SELECT i.* FROM invariants i \
         JOIN task_invariants ti ON ti.invariant_id = i.id \
         WHERE ti.task_id = $1 \
         ORDER BY i.name",
    )
    .bind(task_id)
    .fetch_all(pool)
    .await
    .with_context(|| format!("failed to get invariants for task {task_id}"))?;

    Ok(invariants)
}

/// Link a task to an invariant. Idempotent (ON CONFLICT DO NOTHING).
pub async fn link_task_invariant(
    pool: &PgPool,
    task_id: Uuid,
    invariant_id: Uuid,
) -> Result<()> {
    sqlx::query(
        "INSERT INTO task_invariants (task_id, invariant_id) \
         VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(task_id)
    .bind(invariant_id)
    .execute(pool)
    .await
    .context("failed to link task to invariant")?;

    Ok(())
}
