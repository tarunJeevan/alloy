use anyhow::Result;
use crossterm::{
    event::{self, Event, KeyCode, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    DefaultTerminal,
    layout::{Alignment, Constraint, Direction, Layout},
    style::{Color, Style},
    text::Line,
    widgets::{Block, Borders, Paragraph},
};
use std::{
    io::{self},
    panic,
    time::Duration,
};
use tracing::info;
// ---------------------------------------------------------------------------
// Terminal lifecycle helpers
// ---------------------------------------------------------------------------

/// Restores the terminal to its original state.
/// Called both on clean exit AND from the panic hook  must be idempotent.
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stdout(), LeaveAlternateScreen);
}

/// A guard that calls `restore_terminal()` when dropped.
/// This guarantees cleanup even if an early `?` propagates before the main loop ends.
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        restore_terminal();
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() -> Result<()> {
    // Logging - initialise before anything else so all startup events are captured. Output goes to a file in debug builds to avoid corrupting the TUI surface.
    init_logging();

    // Panic hook - must be installed BEFORE raw mode so that any panic during setup is also handled cleanly.
    let original_hook = panic::take_hook();
    panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    // Enter raw mode + alternate screen
    enable_raw_mode()?;
    execute!(io::stdout(), EnterAlternateScreen)?;

    // TerminalGuard ensures LeaveAlternateScreen + disable_raw_mode on any exit path (including early ? returns below).
    let _guard = TerminalGuard;

    // Build the ratatui terminal and run the event loop.
    let terminal = ratatui::init();
    let result = run(terminal);

    // _guard drops here → restore_terminal() called automatically.

    // Surface any error AFTER terminal is restored so it prints cleanly.
    result
}

// ---------------------------------------------------------------------------
// Event loop
// ---------------------------------------------------------------------------

fn run(mut terminal: DefaultTerminal) -> Result<()> {
    info!("Alloy starting...");

    loop {
        terminal.draw(|frame| {
            let area = frame.area();

            // Outer block
            let block = Block::default()
                .title(" alloy-editor ")
                .title_alignment(Alignment::Center)
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray));

            let inner = block.inner(area);
            frame.render_widget(block, area);

            // Placeholder content
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(0), Constraint::Length(1)])
                .split(inner);

            let placeholder = Paragraph::new("No file open. Press 'q' to quit.")
                .alignment(Alignment::Center)
                .style(Style::default().fg(Color::Gray));
            frame.render_widget(placeholder, chunks[0]);

            let status = Paragraph::new(Line::from(vec![
                ratatui::text::Span::styled(
                    " NORMAL ",
                    Style::default().fg(Color::Black).bg(Color::Blue),
                ),
                ratatui::text::Span::raw(" tui-md-editor v0.1.0"),
            ]));
            frame.render_widget(status, chunks[1]);
        })?;

        // Poll with a timeout so the loop stays responsive
        if event::poll(Duration::from_millis(50))? {
            match event::read()? {
                Event::Key(key) if key.kind == KeyEventKind::Press => match key.code {
                    KeyCode::Char('q') => {
                        info!("Quit requested");
                        break;
                    }
                    _ => {}
                },
                _ => {}
            }
        }
    }

    Ok(())
}

// ---------------------------------------------------------------------------
// Logging setup
// ---------------------------------------------------------------------------

fn init_logging() {
    use tracing_subscriber::{EnvFilter, fmt};

    // In a TUI app, writing logs to stderr would corrupt the display. Write to a file instead; controlled via RUST_LOG env var.
    // Example: RUST_LOG=debug cargo run 2>alloy.log
    fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_writer(io::stderr)
        .with_ansi(false)
        .init();
}
