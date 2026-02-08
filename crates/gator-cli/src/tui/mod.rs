//! Interactive TUI dashboard for monitoring and managing gator plans.

pub mod app;
mod ui;

use std::io;

use anyhow::Result;
use crossterm::event::{self, Event, KeyCode, KeyModifiers};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::backend::CrosstermBackend;
use ratatui::Terminal;
use sqlx::PgPool;

use app::App;

/// Launch the interactive TUI dashboard.
pub async fn run_dashboard(pool: PgPool) -> Result<()> {
    // Set up terminal.
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend)?;

    let mut app = App::new(pool);

    // Initial data load.
    app.refresh().await?;

    let result = run_event_loop(&mut terminal, &mut app).await;

    // Restore terminal.
    disable_raw_mode()?;
    execute!(terminal.backend_mut(), LeaveAlternateScreen)?;
    terminal.show_cursor()?;

    result
}

async fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    app: &mut App,
) -> Result<()> {
    let tick_rate = app.tick_rate;

    loop {
        // Render.
        terminal.draw(|f| ui::render(f, app))?;

        // Poll for events with a timeout matching the tick rate.
        if event::poll(tick_rate)? {
            if let Event::Key(key) = event::read()? {
                // Clear status message on any keypress.
                app.status_message = None;

                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => {
                        app.navigate_back();
                    }
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.should_quit = true;
                    }
                    KeyCode::Enter => {
                        app.navigate_enter();
                        app.refresh().await?;
                    }
                    KeyCode::Char('j') | KeyCode::Down => {
                        app.move_down();
                    }
                    KeyCode::Char('k') | KeyCode::Up => {
                        app.move_up();
                    }
                    KeyCode::Tab => {
                        app.cycle_view();
                        app.refresh().await?;
                    }
                    KeyCode::Char('a') => {
                        if let Err(e) = app.approve_selected().await {
                            app.status_message = Some(format!("Approve failed: {e}"));
                        }
                    }
                    KeyCode::Char('r') => {
                        if let Err(e) = app.reject_selected().await {
                            app.status_message = Some(format!("Reject failed: {e}"));
                        }
                    }
                    KeyCode::Char('R') => {
                        if let Err(e) = app.retry_selected().await {
                            app.status_message = Some(format!("Retry failed: {e}"));
                        }
                    }
                    KeyCode::Char('?') => {
                        app.show_help();
                    }
                    _ => {}
                }
            }
        } else {
            // Tick: refresh data from DB.
            app.refresh().await?;
        }

        if app.should_quit {
            return Ok(());
        }
    }
}
