//! Tests for the `agent_events` query module.

use std::time::Duration;

use sqlx::postgres::PgPoolOptions;
use sqlx::{Executor, PgPool};
use uuid::Uuid;

use gator_db::queries::agent_events::{
    self, NewAgentEvent,
};

// ===========================================================================
// Test harness
// ===========================================================================

async fn create_temp_db() -> (PgPool, String) {
    let database_url =
        std::env::var("GATOR_DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://localhost:5432/gator".to_string()
        });

    // Connect to maintenance database to create temp DB.
    let maint_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/postgres", &database_url[..pos]),
        None => panic!("cannot parse GATOR_DATABASE_URL"),
    };

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect to maintenance database");

    let db_name = format!("gator_test_{}", Uuid::new_v4().simple());
    let stmt = format!("CREATE DATABASE {db_name}");
    maint_pool
        .execute(stmt.as_str())
        .await
        .unwrap_or_else(|e| panic!("failed to create temp database: {e}"));
    maint_pool.close().await;

    let temp_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/{db_name}", &database_url[..pos]),
        None => panic!("cannot parse GATOR_DATABASE_URL"),
    };

    let pool = PgPoolOptions::new()
        .max_connections(5)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&temp_url)
        .await
        .unwrap_or_else(|e| panic!("failed to connect to temp db: {e}"));

    let migrations_path = gator_db::pool::default_migrations_path();
    gator_db::pool::run_migrations(&pool, migrations_path)
        .await
        .expect("migrations should succeed");

    (pool, db_name)
}

async fn drop_temp_db(db_name: &str) {
    let database_url =
        std::env::var("GATOR_DATABASE_URL").unwrap_or_else(|_| {
            "postgresql://localhost:5432/gator".to_string()
        });

    let maint_url = match database_url.rfind('/') {
        Some(pos) => format!("{}/postgres", &database_url[..pos]),
        None => return,
    };

    let maint_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(10))
        .connect(&maint_url)
        .await
        .expect("failed to connect for cleanup");

    let terminate = format!(
        "SELECT pg_terminate_backend(pid) \
         FROM pg_stat_activity \
         WHERE datname = '{db_name}' AND pid <> pg_backend_pid()"
    );
    let _ = maint_pool.execute(terminate.as_str()).await;
    let stmt = format!("DROP DATABASE IF EXISTS {db_name}");
    let _ = maint_pool.execute(stmt.as_str()).await;
    maint_pool.close().await;
}

/// Create a plan and task so we have a valid task_id for FK constraints.
async fn create_test_task(pool: &PgPool) -> Uuid {
    let plan = gator_db::queries::plans::insert_plan(
        pool,
        "test-plan",
        "/tmp/test",
        "main",
    )
    .await
    .expect("insert plan");

    let task = gator_db::queries::tasks::insert_task(
        pool,
        plan.id,
        "test-task",
        "A test task",
        "narrow",
        "auto",
        3,
    )
    .await
    .expect("insert task");

    task.id
}

// ===========================================================================
// Tests
// ===========================================================================

#[tokio::test]
async fn insert_returns_correct_fields() {
    let (pool, db_name) = create_temp_db().await;
    let task_id = create_test_task(&pool).await;

    let new = NewAgentEvent {
        task_id,
        attempt: 0,
        event_type: "message".to_string(),
        payload: serde_json::json!({"role": "assistant", "content": "hello"}),
    };

    let event = agent_events::insert_agent_event(&pool, &new)
        .await
        .expect("insert should succeed");

    assert_eq!(event.task_id, task_id);
    assert_eq!(event.attempt, 0);
    assert_eq!(event.event_type, "message");
    assert_eq!(event.payload["role"], "assistant");
    assert!(event.id > 0);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn list_ordered_by_recorded_at() {
    let (pool, db_name) = create_temp_db().await;
    let task_id = create_test_task(&pool).await;

    // Insert 3 events in order.
    for i in 0..3 {
        let new = NewAgentEvent {
            task_id,
            attempt: 0,
            event_type: format!("event_{i}"),
            payload: serde_json::json!({"index": i}),
        };
        agent_events::insert_agent_event(&pool, &new)
            .await
            .expect("insert should succeed");
    }

    let events = agent_events::list_events_for_task(&pool, task_id, 0)
        .await
        .expect("list should succeed");

    assert_eq!(events.len(), 3);
    // Verify ordering by checking recorded_at is non-decreasing.
    for window in events.windows(2) {
        assert!(window[0].recorded_at <= window[1].recorded_at);
    }
    // Verify event types match insertion order.
    assert_eq!(events[0].event_type, "event_0");
    assert_eq!(events[1].event_type, "event_1");
    assert_eq!(events[2].event_type, "event_2");

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn list_empty_for_nonexistent_task() {
    let (pool, db_name) = create_temp_db().await;

    let bogus_id = Uuid::new_v4();
    let events = agent_events::list_events_for_task(&pool, bogus_id, 0)
        .await
        .expect("list should succeed even for nonexistent task");

    assert!(events.is_empty());

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn list_all_events_across_attempts() {
    let (pool, db_name) = create_temp_db().await;
    let task_id = create_test_task(&pool).await;

    // Insert events for attempt 0 and attempt 1.
    for attempt in 0..2 {
        for i in 0..2 {
            let new = NewAgentEvent {
                task_id,
                attempt,
                event_type: format!("event_{attempt}_{i}"),
                payload: serde_json::json!({"attempt": attempt, "index": i}),
            };
            agent_events::insert_agent_event(&pool, &new)
                .await
                .expect("insert should succeed");
        }
    }

    let all_events = agent_events::list_all_events_for_task(&pool, task_id)
        .await
        .expect("list_all should succeed");

    assert_eq!(all_events.len(), 4);
    // First two should be attempt 0, last two attempt 1.
    assert_eq!(all_events[0].attempt, 0);
    assert_eq!(all_events[1].attempt, 0);
    assert_eq!(all_events[2].attempt, 1);
    assert_eq!(all_events[3].attempt, 1);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn count_returns_correct_values() {
    let (pool, db_name) = create_temp_db().await;
    let task_id = create_test_task(&pool).await;

    // Initially zero.
    let count = agent_events::count_events_for_task(&pool, task_id, 0)
        .await
        .expect("count should succeed");
    assert_eq!(count, 0);

    // Insert 3 events.
    for i in 0..3 {
        let new = NewAgentEvent {
            task_id,
            attempt: 0,
            event_type: format!("event_{i}"),
            payload: serde_json::json!({}),
        };
        agent_events::insert_agent_event(&pool, &new)
            .await
            .expect("insert should succeed");
    }

    let count = agent_events::count_events_for_task(&pool, task_id, 0)
        .await
        .expect("count should succeed");
    assert_eq!(count, 3);

    // Different attempt should be zero.
    let count_other = agent_events::count_events_for_task(&pool, task_id, 1)
        .await
        .expect("count should succeed");
    assert_eq!(count_other, 0);

    pool.close().await;
    drop_temp_db(&db_name).await;
}

#[tokio::test]
async fn multiple_event_types_insert_correctly() {
    let (pool, db_name) = create_temp_db().await;
    let task_id = create_test_task(&pool).await;

    let types = ["message", "tool_call", "tool_result", "token_usage", "error", "completed"];
    for event_type in &types {
        let new = NewAgentEvent {
            task_id,
            attempt: 0,
            event_type: event_type.to_string(),
            payload: serde_json::json!({"type": event_type}),
        };
        agent_events::insert_agent_event(&pool, &new)
            .await
            .expect("insert should succeed");
    }

    let events = agent_events::list_events_for_task(&pool, task_id, 0)
        .await
        .expect("list should succeed");

    assert_eq!(events.len(), 6);
    let event_types: Vec<&str> = events.iter().map(|e| e.event_type.as_str()).collect();
    for t in &types {
        assert!(event_types.contains(t), "should contain event type {t}");
    }

    pool.close().await;
    drop_temp_db(&db_name).await;
}
