//! `gator log` command: show agent events for a task.

use anyhow::{Context, Result};
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::AgentEvent;
use gator_db::queries::agent_events;
use gator_db::queries::tasks as task_db;

/// Run the log command.
pub async fn run_log(pool: &PgPool, task_id_str: &str, attempt: Option<i32>) -> Result<()> {
    let task_id =
        Uuid::parse_str(task_id_str).with_context(|| format!("invalid task ID: {task_id_str}"))?;

    let task = task_db::get_task(pool, task_id)
        .await?
        .with_context(|| format!("task {task_id} not found"))?;

    println!("Task: {} ({})", task.name, task.id);
    println!("Status: {} (attempt {})", task.status, task.attempt);
    println!();

    let events: Vec<AgentEvent> = match attempt {
        Some(a) => agent_events::list_events_for_task(pool, task_id, a).await?,
        None => agent_events::list_all_events_for_task(pool, task_id).await?,
    };

    if events.is_empty() {
        println!("No events recorded.");
        return Ok(());
    }

    println!("Events ({}):", events.len());
    for event in &events {
        let time = event.recorded_at.format("%H:%M:%S%.3f");
        let summary = summarize_event_payload(&event.event_type, &event.payload);
        println!(
            "  [{time}] [attempt {}] {}: {summary}",
            event.attempt, event.event_type
        );
    }

    Ok(())
}

/// Generate a one-line summary from an event's type and payload.
fn summarize_event_payload(event_type: &str, payload: &serde_json::Value) -> String {
    match event_type {
        "message" => {
            let role = payload["role"].as_str().unwrap_or("?");
            let content = payload["content"].as_str().unwrap_or("");
            let truncated = if content.len() > 80 {
                format!("{}...", &content[..77])
            } else {
                content.to_string()
            };
            format!("[{role}] {truncated}")
        }
        "tool_call" => {
            let tool = payload["tool"].as_str().unwrap_or("?");
            format!("call {tool}")
        }
        "tool_result" => {
            let tool = payload["tool"].as_str().unwrap_or("?");
            format!("result from {tool}")
        }
        "token_usage" => {
            let input = payload["input_tokens"].as_u64().unwrap_or(0);
            let output = payload["output_tokens"].as_u64().unwrap_or(0);
            format!("in={input} out={output}")
        }
        "error" => {
            let msg = payload["message"].as_str().unwrap_or("unknown error");
            if msg.len() > 80 {
                format!("{}...", &msg[..77])
            } else {
                msg.to_string()
            }
        }
        "completed" => "agent finished".to_string(),
        _ => format!("{}", payload),
    }
}
