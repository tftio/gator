use anyhow::Context;
use sqlx::PgPool;

/// Export plan/task data as CSV.
pub async fn run_export_csv(
    pool: &PgPool,
    plan_id: Option<&str>,
    output: Option<&str>,
) -> anyhow::Result<()> {
    use std::io::Write;

    let plan_id = plan_id.map(crate::resolve::resolve_plan_id).transpose()?;

    // Query tasks, optionally filtered by plan
    let rows = if let Some(pid) = plan_id {
        sqlx::query_as::<_, TaskRow>(
            "SELECT t.id, t.plan_id, t.name, t.status, t.attempt, t.created_at
             FROM tasks t WHERE t.plan_id = $1 ORDER BY t.created_at",
        )
        .bind(pid)
        .fetch_all(pool)
        .await?
    } else {
        sqlx::query_as::<_, TaskRow>(
            "SELECT t.id, t.plan_id, t.name, t.status, t.attempt, t.created_at
             FROM tasks t ORDER BY t.plan_id, t.created_at",
        )
        .fetch_all(pool)
        .await?
    };

    let mut writer: Box<dyn Write> = if let Some(path) = output {
        Box::new(
            std::fs::File::create(path)
                .with_context(|| format!("cannot create output file: {path}"))?,
        )
    } else {
        Box::new(std::io::stdout().lock())
    };

    // Header
    writeln!(writer, "id,plan_id,name,status,attempt,created_at")?;

    for row in &rows {
        writeln!(
            writer,
            "{},{},{},{},{},{}",
            row.id, row.plan_id, row.name, row.status, row.attempt, row.created_at,
        )?;
    }

    if let Some(path) = output {
        println!("Exported {} rows to {path}", rows.len());
    }

    Ok(())
}

#[derive(sqlx::FromRow)]
struct TaskRow {
    id: uuid::Uuid,
    plan_id: uuid::Uuid,
    name: String,
    status: String,
    attempt: i32,
    created_at: chrono::DateTime<chrono::Utc>,
}
