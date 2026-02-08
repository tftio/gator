//! TUI rendering using ratatui.

use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table};
use ratatui::Frame;

use gator_db::models::{PlanStatus, TaskStatus};

use super::app::{App, View};

/// Render the current view.
pub fn render(f: &mut Frame, app: &App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(3),    // main content
            Constraint::Length(1), // status bar
        ])
        .split(f.area());

    match &app.current_view {
        View::PlanList => render_plan_list(f, app, chunks[0]),
        View::PlanDetail(plan_id) => render_plan_detail(f, app, *plan_id, chunks[0]),
        View::TaskDetail(task_id) => render_task_detail(f, app, *task_id, chunks[0]),
        View::ReviewQueue => render_review_queue(f, app, chunks[0]),
        View::Help => render_help(f, chunks[0]),
    }

    render_status_bar(f, app, chunks[1]);
}

fn render_plan_list(f: &mut Frame, app: &App, area: Rect) {
    let header_cells = ["Name", "Status", "Progress", "Tasks", "Budget", "Created"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow)));
    let header = Row::new(header_cells).height(1);

    let rows = app.plans.iter().enumerate().map(|(i, pr)| {
        let prog = &pr.progress;
        let progress_str = format!("{}/{}", prog.passed, prog.total);
        let budget_str = pr
            .plan
            .token_budget
            .map(|b| format!("{b}"))
            .unwrap_or_else(|| "-".to_string());
        let created = pr.plan.created_at.format("%Y-%m-%d %H:%M").to_string();

        let style = if i == app.selected_plan {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        Row::new(vec![
            Cell::from(pr.plan.name.clone()),
            Cell::from(status_colored(&pr.plan.status)),
            Cell::from(progress_str),
            Cell::from(format!("{}", prog.total)),
            Cell::from(budget_str),
            Cell::from(created),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(25),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(6),
            Constraint::Length(10),
            Constraint::Length(18),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Plans "),
    );

    f.render_widget(table, area);
}

fn render_plan_detail(f: &mut Frame, app: &App, plan_id: uuid::Uuid, area: Rect) {
    let plan_name = app
        .plans
        .iter()
        .find(|pr| pr.plan.id == plan_id)
        .map(|pr| pr.plan.name.as_str())
        .unwrap_or("Unknown");

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(5)])
        .split(area);

    // Plan header.
    let plan_info = app.plans.iter().find(|pr| pr.plan.id == plan_id);
    let header_text = if let Some(pr) = plan_info {
        let prog = &pr.progress;
        format!(
            " {} | {} | {}/{} passed | Budget: {}",
            plan_name,
            pr.plan.status,
            prog.passed,
            prog.total,
            pr.plan
                .token_budget
                .map(|b| b.to_string())
                .unwrap_or_else(|| "unlimited".to_string()),
        )
    } else {
        format!(" {plan_name}")
    };

    let header = Paragraph::new(header_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Plan "),
    );
    f.render_widget(header, chunks[0]);

    // Task table.
    let task_header_cells = ["Name", "Status", "Attempt", "Scope", "Gate", "Harness"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow)));
    let task_header = Row::new(task_header_cells).height(1);

    let task_rows = app.tasks.iter().enumerate().map(|(i, task)| {
        let style = if i == app.selected_task {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        Row::new(vec![
            Cell::from(task.name.clone()),
            Cell::from(task_status_colored(&task.status)),
            Cell::from(format!("{}/{}", task.attempt, task.retry_max)),
            Cell::from(task.scope_level.to_string()),
            Cell::from(task.gate_policy.to_string()),
            Cell::from(
                task.assigned_harness
                    .clone()
                    .unwrap_or_else(|| "-".to_string()),
            ),
        ])
        .style(style)
    });

    let task_table = Table::new(
        task_rows,
        [
            Constraint::Percentage(30),
            Constraint::Length(12),
            Constraint::Length(10),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(14),
        ],
    )
    .header(task_header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Tasks "),
    );

    f.render_widget(task_table, chunks[1]);
}

fn render_task_detail(f: &mut Frame, app: &App, task_id: uuid::Uuid, area: Rect) {
    let task = app.tasks.iter().find(|t| t.id == task_id);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(5), // task info
            Constraint::Min(5),   // gate results
            Constraint::Min(5),   // events
        ])
        .split(area);

    // Task info.
    let info_text = if let Some(t) = task {
        vec![
            Line::from(vec![
                Span::styled("Task: ", Style::default().fg(Color::Yellow)),
                Span::raw(&t.name),
            ]),
            Line::from(vec![
                Span::styled("Status: ", Style::default().fg(Color::Yellow)),
                Span::raw(t.status.to_string()),
                Span::raw(format!("  Attempt: {}/{}", t.attempt, t.retry_max)),
                Span::raw(format!("  Scope: {}  Gate: {}", t.scope_level, t.gate_policy)),
            ]),
        ]
    } else {
        vec![Line::from("Task not found")]
    };

    let info = Paragraph::new(info_text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Task Detail "),
    );
    f.render_widget(info, chunks[0]);

    // Gate results.
    let gate_header_cells = ["Invariant", "Passed", "Exit", "Duration"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow)));
    let gate_header = Row::new(gate_header_cells).height(1);

    let gate_rows = app.gate_results.iter().map(|gr| {
        let pass_style = if gr.passed {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        };

        Row::new(vec![
            Cell::from(gr.invariant_name.clone()),
            Cell::from(if gr.passed { "PASS" } else { "FAIL" }).style(pass_style),
            Cell::from(
                gr.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "-".to_string()),
            ),
            Cell::from(
                gr.duration_ms
                    .map(|d| format!("{d}ms"))
                    .unwrap_or_else(|| "-".to_string()),
            ),
        ])
    });

    let gate_table = Table::new(
        gate_rows,
        [
            Constraint::Percentage(40),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(10),
        ],
    )
    .header(gate_header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Gate Results "),
    );
    f.render_widget(gate_table, chunks[1]);

    // Recent events.
    let event_lines: Vec<Line> = app
        .events
        .iter()
        .take(10)
        .map(|ev| {
            let time = ev.recorded_at.format("%H:%M:%S").to_string();
            Line::from(vec![
                Span::styled(
                    format!("[{time}] "),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(
                    format!("{} ", ev.event_type),
                    Style::default().fg(Color::Cyan),
                ),
                Span::raw(truncate(&ev.payload.to_string(), 80)),
            ])
        })
        .collect();

    let events = Paragraph::new(event_lines).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Recent Events "),
    );
    f.render_widget(events, chunks[2]);
}

fn render_review_queue(f: &mut Frame, app: &App, area: Rect) {
    let header_cells = ["Task", "Plan", "Scope", "Gate Policy", "Attempt"]
        .iter()
        .map(|h| Cell::from(*h).style(Style::default().fg(Color::Yellow)));
    let header = Row::new(header_cells).height(1);

    let rows = app.review_tasks.iter().enumerate().map(|(i, rt)| {
        let style = if i == app.selected_review {
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };

        Row::new(vec![
            Cell::from(rt.name.clone()),
            Cell::from(rt.plan_name.clone()),
            Cell::from(rt.scope_level.to_string()),
            Cell::from(rt.gate_policy.to_string()),
            Cell::from(format!("{}/{}", rt.attempt, rt.retry_max)),
        ])
        .style(style)
    });

    let table = Table::new(
        rows,
        [
            Constraint::Percentage(30),
            Constraint::Percentage(25),
            Constraint::Length(8),
            Constraint::Length(14),
            Constraint::Length(10),
        ],
    )
    .header(header)
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " Review Queue ({}) ",
                app.review_tasks.len()
            )),
    );

    f.render_widget(table, area);
}

fn render_help(f: &mut Frame, area: Rect) {
    let text = vec![
        Line::from(""),
        Line::from(vec![
            Span::styled("  Navigation", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from("    j/Down    Move down"),
        Line::from("    k/Up      Move up"),
        Line::from("    Enter     Drill into selected"),
        Line::from("    Esc/q     Back / Quit"),
        Line::from("    Tab       Toggle Plans / Review Queue"),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Actions", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from("    a         Approve selected task (if checking)"),
        Line::from("    r         Reject selected task (if checking)"),
        Line::from("    R         Retry selected task (if failed/escalated)"),
        Line::from(""),
        Line::from(vec![
            Span::styled("  Other", Style::default().fg(Color::Yellow).add_modifier(Modifier::BOLD)),
        ]),
        Line::from("    ?         Show this help"),
        Line::from(""),
    ];

    let help = Paragraph::new(text).block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Help "),
    );
    f.render_widget(help, area);
}

fn render_status_bar(f: &mut Frame, app: &App, area: Rect) {
    let view_name = match &app.current_view {
        View::PlanList => "Plans",
        View::PlanDetail(_) => "Plan Detail",
        View::TaskDetail(_) => "Task Detail",
        View::ReviewQueue => "Review Queue",
        View::Help => "Help",
    };

    let review_count: usize = app
        .plans
        .iter()
        .map(|pr| pr.progress.checking as usize)
        .sum();

    let status_msg = app
        .status_message
        .as_deref()
        .unwrap_or("");

    let bar = Line::from(vec![
        Span::styled(
            format!(" {view_name} "),
            Style::default().bg(Color::Blue).fg(Color::White),
        ),
        Span::raw("  "),
        if review_count > 0 {
            Span::styled(
                format!("{review_count} awaiting review"),
                Style::default().fg(Color::Yellow),
            )
        } else {
            Span::styled("no tasks awaiting review", Style::default().fg(Color::DarkGray))
        },
        Span::raw("  "),
        Span::styled(status_msg, Style::default().fg(Color::Green)),
        Span::raw("  q:quit  ?:help  Tab:switch view"),
    ]);

    f.render_widget(Paragraph::new(bar), area);
}

// -- Helpers --

fn status_colored(status: &PlanStatus) -> Span<'static> {
    let (text, color) = match status {
        PlanStatus::Draft => ("draft", Color::DarkGray),
        PlanStatus::Approved => ("approved", Color::Cyan),
        PlanStatus::Running => ("running", Color::Blue),
        PlanStatus::Completed => ("completed", Color::Green),
        PlanStatus::Failed => ("failed", Color::Red),
    };
    Span::styled(text.to_string(), Style::default().fg(color))
}

fn task_status_colored(status: &TaskStatus) -> Span<'static> {
    let (text, color) = match status {
        TaskStatus::Pending => ("pending", Color::DarkGray),
        TaskStatus::Assigned => ("assigned", Color::Cyan),
        TaskStatus::Running => ("running", Color::Blue),
        TaskStatus::Checking => ("checking", Color::Yellow),
        TaskStatus::Passed => ("passed", Color::Green),
        TaskStatus::Failed => ("failed", Color::Red),
        TaskStatus::Escalated => ("escalated", Color::Magenta),
    };
    Span::styled(text.to_string(), Style::default().fg(color))
}

fn truncate(s: &str, max: usize) -> String {
    if s.len() <= max {
        s.to_string()
    } else {
        format!("{}...", &s[..max])
    }
}
