use std::net::SocketAddr;

use anyhow::Result;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;
use sqlx::PgPool;
use tower_http::cors::CorsLayer;
use uuid::Uuid;

use axum::response::Html;
use gator_db::models::{AgentEvent, Invariant, Plan, Task};
use gator_db::queries::tasks::PlanProgress;
use gator_db::queries::{
    agent_events,
    gate_results::{self, GateResultWithName},
    invariants as invariant_db, plans as plan_db, tasks as task_db,
};

// ---------------------------------------------------------------------------
// Error type
// ---------------------------------------------------------------------------

pub struct AppError {
    status: StatusCode,
    message: String,
}

impl AppError {
    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    pub fn internal(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{err:#}"),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let body = serde_json::json!({ "error": self.message });
        (self.status, Json(body)).into_response()
    }
}

// ---------------------------------------------------------------------------
// Response types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct ProgressResponse {
    pub pending: i64,
    pub assigned: i64,
    pub running: i64,
    pub checking: i64,
    pub passed: i64,
    pub failed: i64,
    pub escalated: i64,
    pub total: i64,
}

impl From<PlanProgress> for ProgressResponse {
    fn from(p: PlanProgress) -> Self {
        Self {
            pending: p.pending,
            assigned: p.assigned,
            running: p.running,
            checking: p.checking,
            passed: p.passed,
            failed: p.failed,
            escalated: p.escalated,
            total: p.total,
        }
    }
}

#[derive(Debug, Serialize)]
pub struct TokenUsageResponse {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub total_tokens: i64,
}

#[derive(Debug, Serialize)]
pub struct PlanSummaryResponse {
    #[serde(flatten)]
    pub plan: Plan,
    pub progress: ProgressResponse,
}

#[derive(Debug, Serialize)]
pub struct PlanDetailResponse {
    #[serde(flatten)]
    pub plan: Plan,
    pub progress: ProgressResponse,
    pub token_usage: TokenUsageResponse,
    pub tasks: Vec<Task>,
}

#[derive(Debug, Serialize)]
pub struct TaskDetailResponse {
    #[serde(flatten)]
    pub task: Task,
    pub dependencies: Vec<Uuid>,
    pub invariants: Vec<Invariant>,
    pub events: Vec<AgentEvent>,
    pub gate_results: Vec<GateResultWithName>,
    pub token_usage: TokenUsageResponse,
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

pub fn build_router(pool: PgPool) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/api/plans", get(list_plans))
        .route("/api/plans/{id}", get(get_plan_detail))
        .route("/api/tasks/{id}", get(get_task_detail))
        .route("/api/invariants", get(list_invariants_handler))
        .layer(CorsLayer::permissive())
        .with_state(pool)
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub async fn run_serve(pool: PgPool, bind: &str, port: u16) -> Result<()> {
    let app = build_router(pool);
    let addr: SocketAddr = format!("{bind}:{port}").parse()?;
    tracing::info!("gator serve listening on http://{addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await?;
    tracing::info!("gator serve shut down");
    Ok(())
}

async fn shutdown_signal() {
    tokio::signal::ctrl_c()
        .await
        .expect("failed to install Ctrl+C handler");
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn index(State(pool): State<PgPool>) -> Result<axum::response::Response, AppError> {
    let plans = plan_db::list_plans(&pool)
        .await
        .map_err(AppError::internal)?;

    let rows = if plans.is_empty() {
        "<tr><td colspan=\"3\">No plans found.</td></tr>".to_string()
    } else {
        plans
            .iter()
            .map(|p| {
                format!(
                    "<tr><td><a href=\"/api/plans/{id}\">{name}</a></td><td>{status}</td><td>{id}</td></tr>",
                    id = p.id,
                    name = p.name,
                    status = p.status,
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    };

    let html = format!(
        "<!DOCTYPE html>\
<html><head><title>gator</title></head><body>\
<h1>gator</h1>\
<p><a href=\"/api/plans\">/api/plans</a> | <a href=\"/api/invariants\">/api/invariants</a></p>\
<table><tr><th>Plan</th><th>Status</th><th>ID</th></tr>{rows}</table>\
</body></html>"
    );

    Ok(Html(html).into_response())
}

async fn list_plans(State(pool): State<PgPool>) -> Result<axum::response::Response, AppError> {
    let plans = plan_db::list_plans(&pool)
        .await
        .map_err(AppError::internal)?;

    let mut results = Vec::with_capacity(plans.len());
    for plan in plans {
        let progress = task_db::get_plan_progress(&pool, plan.id)
            .await
            .map_err(AppError::internal)?;
        results.push(PlanSummaryResponse {
            plan,
            progress: progress.into(),
        });
    }

    Ok(Json(results).into_response())
}

async fn get_plan_detail(
    State(pool): State<PgPool>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, AppError> {
    let plan = plan_db::get_plan(&pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("plan {id} not found")))?;

    let progress = task_db::get_plan_progress(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let tasks = task_db::list_tasks_for_plan(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let (input_tokens, output_tokens) = agent_events::get_token_usage_for_plan(&pool, id)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(PlanDetailResponse {
        plan,
        progress: progress.into(),
        token_usage: TokenUsageResponse {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        },
        tasks,
    })
    .into_response())
}

async fn get_task_detail(
    State(pool): State<PgPool>,
    Path(id): Path<Uuid>,
) -> Result<axum::response::Response, AppError> {
    let task = task_db::get_task(&pool, id)
        .await
        .map_err(AppError::internal)?
        .ok_or_else(|| AppError::not_found(format!("task {id} not found")))?;

    let dependencies = task_db::get_task_dependencies(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let invariants = invariant_db::get_invariants_for_task(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let events = agent_events::list_all_events_for_task(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let gate_results = gate_results::get_latest_gate_results(&pool, id)
        .await
        .map_err(AppError::internal)?;

    let (input_tokens, output_tokens) = agent_events::get_token_usage_for_task(&pool, id)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(TaskDetailResponse {
        task,
        dependencies,
        invariants,
        events,
        gate_results,
        token_usage: TokenUsageResponse {
            input_tokens,
            output_tokens,
            total_tokens: input_tokens + output_tokens,
        },
    })
    .into_response())
}

async fn list_invariants_handler(
    State(pool): State<PgPool>,
) -> Result<axum::response::Response, AppError> {
    let invariants = invariant_db::list_invariants(&pool)
        .await
        .map_err(AppError::internal)?;

    Ok(Json(invariants).into_response())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use sqlx::PgPool;
    use tower::ServiceExt;

    use gator_db::models::{InvariantKind, InvariantScope};
    use gator_db::queries::invariants::{NewInvariant, insert_invariant};
    use gator_db::queries::plans::insert_plan;
    use gator_db::queries::tasks::insert_task;
    use gator_test_utils::{create_test_db, drop_test_db};

    // -----------------------------------------------------------------------
    // HTTP helpers
    // -----------------------------------------------------------------------

    async fn send_request(pool: PgPool, uri: &str) -> axum::response::Response {
        let app = super::build_router(pool);
        app.oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
            .await
            .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), 1_048_576)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    // -----------------------------------------------------------------------
    // Tests
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn test_index_returns_html() {
        let (pool, db_name) = create_test_db().await;

        let resp = send_request(pool.clone(), "/").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let content_type = resp
            .headers()
            .get("content-type")
            .expect("should have content-type header")
            .to_str()
            .unwrap();
        assert!(
            content_type.contains("text/html"),
            "content-type should contain text/html, got: {content_type}"
        );

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_list_plans_empty() {
        let (pool, db_name) = create_test_db().await;

        let resp = send_request(pool.clone(), "/api/plans").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json, serde_json::json!([]));

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_list_plans_with_data() {
        let (pool, db_name) = create_test_db().await;

        let plan = insert_plan(
            &pool,
            "test-plan",
            "/tmp/project",
            "main",
            None,
            "claude-code",
            "worktree",
            None,
        )
        .await
        .expect("insert_plan should succeed");

        let resp = send_request(pool.clone(), "/api/plans").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let arr = json.as_array().expect("response should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], plan.name);
        assert!(
            arr[0].get("progress").is_some(),
            "each plan should have a progress object"
        );
        assert!(
            arr[0]["progress"].get("total").is_some(),
            "progress should have a total field"
        );

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_get_plan_detail() {
        let (pool, db_name) = create_test_db().await;

        let plan = insert_plan(
            &pool,
            "detail-plan",
            "/tmp/project",
            "main",
            None,
            "claude-code",
            "worktree",
            None,
        )
        .await
        .expect("insert_plan should succeed");

        let _task = insert_task(
            &pool,
            plan.id,
            "task-one",
            "a test task",
            "narrow",
            "auto",
            3,
            None,
        )
        .await
        .expect("insert_task should succeed");

        let resp = send_request(pool.clone(), &format!("/api/plans/{}", plan.id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["name"], "detail-plan");
        let tasks = json["tasks"].as_array().expect("should have tasks array");
        assert_eq!(tasks.len(), 1);
        assert!(
            json.get("progress").is_some(),
            "should have progress object"
        );
        assert!(
            json.get("token_usage").is_some(),
            "should have token_usage object"
        );

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_get_plan_not_found() {
        let (pool, db_name) = create_test_db().await;

        let random_id = uuid::Uuid::new_v4();
        let resp = send_request(pool.clone(), &format!("/api/plans/{random_id}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_get_task_detail() {
        let (pool, db_name) = create_test_db().await;

        let plan = insert_plan(
            &pool,
            "task-detail-plan",
            "/tmp/project",
            "main",
            None,
            "claude-code",
            "worktree",
            None,
        )
        .await
        .expect("insert_plan should succeed");

        let task = insert_task(
            &pool,
            plan.id,
            "my-task",
            "a detailed task",
            "narrow",
            "auto",
            3,
            None,
        )
        .await
        .expect("insert_task should succeed");

        let resp = send_request(pool.clone(), &format!("/api/tasks/{}", task.id)).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json["name"], "my-task");
        assert!(
            json.get("dependencies").is_some(),
            "should have dependencies field"
        );
        assert!(
            json.get("invariants").is_some(),
            "should have invariants field"
        );
        assert!(json.get("events").is_some(), "should have events field");
        assert!(
            json.get("gate_results").is_some(),
            "should have gate_results field"
        );
        assert!(
            json.get("token_usage").is_some(),
            "should have token_usage field"
        );

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_get_task_not_found() {
        let (pool, db_name) = create_test_db().await;

        let random_id = uuid::Uuid::new_v4();
        let resp = send_request(pool.clone(), &format!("/api/tasks/{random_id}")).await;
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_list_invariants_empty() {
        let (pool, db_name) = create_test_db().await;

        let resp = send_request(pool.clone(), "/api/invariants").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        assert_eq!(json, serde_json::json!([]));

        pool.close().await;
        drop_test_db(&db_name).await;
    }

    #[tokio::test]
    async fn test_list_invariants_with_data() {
        let (pool, db_name) = create_test_db().await;

        let new_inv = NewInvariant {
            name: "cargo-check",
            description: Some("Run cargo check"),
            kind: InvariantKind::Typecheck,
            command: "cargo",
            args: &["check".to_string(), "--workspace".to_string()],
            expected_exit_code: 0,
            threshold: None,
            scope: InvariantScope::Project,
            timeout_secs: 300,
        };
        insert_invariant(&pool, &new_inv)
            .await
            .expect("insert_invariant should succeed");

        let resp = send_request(pool.clone(), "/api/invariants").await;
        assert_eq!(resp.status(), StatusCode::OK);
        let json = body_json(resp).await;
        let arr = json.as_array().expect("response should be an array");
        assert_eq!(arr.len(), 1);
        assert_eq!(arr[0]["name"], "cargo-check");

        pool.close().await;
        drop_test_db(&db_name).await;
    }
}
