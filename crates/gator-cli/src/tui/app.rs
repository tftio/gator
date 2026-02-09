//! TUI application state and data model.

use std::time::Duration;

use anyhow::Result;
use sqlx::PgPool;
use uuid::Uuid;

use gator_db::models::{Plan, Task};
use gator_db::queries::agent_events;
use gator_db::queries::gate_results::{self, GateResultWithName};
use gator_db::queries::plans as plan_db;
use gator_db::queries::tasks as task_db;

/// Which view the TUI is currently showing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum View {
    PlanList,
    PlanDetail(Uuid),
    TaskDetail(Uuid),
    ReviewQueue,
    Help,
}

/// Aggregated plan info for the list view.
#[derive(Debug, Clone)]
pub struct PlanRow {
    pub plan: Plan,
    pub progress: task_db::PlanProgress,
}

/// Re-export from gator-db for the review queue.
pub use gator_db::queries::tasks::TaskWithPlanName;

/// Application state for the TUI.
pub struct App {
    pub pool: PgPool,
    pub current_view: View,
    pub plans: Vec<PlanRow>,
    pub selected_plan: usize,
    pub tasks: Vec<Task>,
    pub selected_task: usize,
    pub gate_results: Vec<GateResultWithName>,
    pub events: Vec<gator_db::models::AgentEvent>,
    pub review_tasks: Vec<TaskWithPlanName>,
    pub selected_review: usize,
    pub tick_rate: Duration,
    pub should_quit: bool,
    pub status_message: Option<String>,
}

impl App {
    pub fn new(pool: PgPool) -> Self {
        Self {
            pool,
            current_view: View::PlanList,
            plans: Vec::new(),
            selected_plan: 0,
            tasks: Vec::new(),
            selected_task: 0,
            gate_results: Vec::new(),
            events: Vec::new(),
            review_tasks: Vec::new(),
            selected_review: 0,
            tick_rate: Duration::from_secs(1),
            should_quit: false,
            status_message: None,
        }
    }

    /// Refresh data from the database based on the current view.
    pub async fn refresh(&mut self) -> Result<()> {
        match &self.current_view {
            View::PlanList => {
                self.refresh_plans().await?;
            }
            View::PlanDetail(plan_id) => {
                let plan_id = *plan_id;
                self.tasks = task_db::list_tasks_for_plan(&self.pool, plan_id).await?;
                if self.selected_task >= self.tasks.len() && !self.tasks.is_empty() {
                    self.selected_task = self.tasks.len() - 1;
                }
            }
            View::TaskDetail(task_id) => {
                let task_id = *task_id;
                self.gate_results =
                    gate_results::get_latest_gate_results(&self.pool, task_id).await?;
                self.events =
                    agent_events::get_recent_events_for_task(&self.pool, task_id, None, 20).await?;
            }
            View::ReviewQueue => {
                self.refresh_review_queue().await?;
            }
            View::Help => {}
        }
        Ok(())
    }

    async fn refresh_plans(&mut self) -> Result<()> {
        let plans = plan_db::list_plans(&self.pool).await?;
        let mut plan_rows = Vec::with_capacity(plans.len());
        for plan in plans {
            let progress = task_db::get_plan_progress(&self.pool, plan.id).await?;
            plan_rows.push(PlanRow { plan, progress });
        }
        self.plans = plan_rows;
        if self.selected_plan >= self.plans.len() && !self.plans.is_empty() {
            self.selected_plan = self.plans.len() - 1;
        }
        Ok(())
    }

    async fn refresh_review_queue(&mut self) -> Result<()> {
        self.review_tasks = task_db::list_checking_tasks(&self.pool).await?;
        if self.selected_review >= self.review_tasks.len() && !self.review_tasks.is_empty() {
            self.selected_review = self.review_tasks.len() - 1;
        }
        Ok(())
    }

    // -- Navigation --

    pub fn navigate_back(&mut self) {
        match &self.current_view {
            View::PlanList => self.should_quit = true,
            View::PlanDetail(_) => self.current_view = View::PlanList,
            View::TaskDetail(_) => {
                // Go back to the plan that owns this task.
                if let Some(task) = self.tasks.first() {
                    self.current_view = View::PlanDetail(task.plan_id);
                } else {
                    self.current_view = View::PlanList;
                }
            }
            View::ReviewQueue => self.current_view = View::PlanList,
            View::Help => self.current_view = View::PlanList,
        }
    }

    pub fn navigate_enter(&mut self) {
        match &self.current_view {
            View::PlanList => {
                if let Some(plan_row) = self.plans.get(self.selected_plan) {
                    self.current_view = View::PlanDetail(plan_row.plan.id);
                    self.selected_task = 0;
                }
            }
            View::PlanDetail(_) => {
                if let Some(task) = self.tasks.get(self.selected_task) {
                    self.current_view = View::TaskDetail(task.id);
                }
            }
            _ => {}
        }
    }

    pub fn move_up(&mut self) {
        match &self.current_view {
            View::PlanList => {
                if self.selected_plan > 0 {
                    self.selected_plan -= 1;
                }
            }
            View::PlanDetail(_) => {
                if self.selected_task > 0 {
                    self.selected_task -= 1;
                }
            }
            View::ReviewQueue => {
                if self.selected_review > 0 {
                    self.selected_review -= 1;
                }
            }
            _ => {}
        }
    }

    pub fn move_down(&mut self) {
        match &self.current_view {
            View::PlanList => {
                if !self.plans.is_empty() && self.selected_plan < self.plans.len() - 1 {
                    self.selected_plan += 1;
                }
            }
            View::PlanDetail(_) => {
                if !self.tasks.is_empty() && self.selected_task < self.tasks.len() - 1 {
                    self.selected_task += 1;
                }
            }
            View::ReviewQueue => {
                if !self.review_tasks.is_empty()
                    && self.selected_review < self.review_tasks.len() - 1
                {
                    self.selected_review += 1;
                }
            }
            _ => {}
        }
    }

    pub fn cycle_view(&mut self) {
        self.current_view = match &self.current_view {
            View::PlanList => View::ReviewQueue,
            View::ReviewQueue => View::PlanList,
            other => other.clone(),
        };
    }

    pub fn show_help(&mut self) {
        self.current_view = View::Help;
    }

    // -- Actions --

    pub async fn approve_selected(&mut self) -> Result<()> {
        let task_id = self.selected_checking_task_id();
        if let Some(id) = task_id {
            gator_core::state::dispatch::approve_task(&self.pool, id).await?;
            self.status_message = Some("Task approved".to_string());
            self.refresh().await?;
        }
        Ok(())
    }

    pub async fn reject_selected(&mut self) -> Result<()> {
        let task_id = self.selected_checking_task_id();
        if let Some(id) = task_id {
            gator_core::state::dispatch::reject_task(&self.pool, id).await?;
            self.status_message = Some("Task rejected".to_string());
            self.refresh().await?;
        }
        Ok(())
    }

    pub async fn retry_selected(&mut self) -> Result<()> {
        let task_id = self.selected_actionable_task_id();
        if let Some(id) = task_id {
            gator_core::state::dispatch::operator_retry_task(&self.pool, id, false).await?;
            self.status_message = Some("Task queued for retry".to_string());
            self.refresh().await?;
        }
        Ok(())
    }

    /// Get the task ID of the currently selected checking task (if any).
    fn selected_checking_task_id(&self) -> Option<Uuid> {
        match &self.current_view {
            View::ReviewQueue => self.review_tasks.get(self.selected_review).map(|rt| rt.id),
            View::PlanDetail(_) => self
                .tasks
                .get(self.selected_task)
                .filter(|t| t.status == gator_db::models::TaskStatus::Checking)
                .map(|t| t.id),
            _ => None,
        }
    }

    /// Get the task ID of the currently selected task if it's actionable
    /// (failed or escalated for retry).
    fn selected_actionable_task_id(&self) -> Option<Uuid> {
        match &self.current_view {
            View::PlanDetail(_) => self
                .tasks
                .get(self.selected_task)
                .filter(|t| {
                    t.status == gator_db::models::TaskStatus::Failed
                        || t.status == gator_db::models::TaskStatus::Escalated
                })
                .map(|t| t.id),
            View::ReviewQueue => self
                .review_tasks
                .get(self.selected_review)
                .filter(|rt| {
                    rt.status == gator_db::models::TaskStatus::Failed
                        || rt.status == gator_db::models::TaskStatus::Escalated
                })
                .map(|rt| rt.id),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_navigation() {
        let plan_id = Uuid::new_v4();

        // PlanDetail -> back -> PlanList
        let view = View::PlanDetail(plan_id);
        assert_ne!(view, View::PlanList);

        // Help -> back
        let view = View::Help;
        assert_ne!(view, View::PlanList);

        // ReviewQueue cycles to PlanList
        let view = View::ReviewQueue;
        assert_ne!(view, View::PlanList);
    }
}
