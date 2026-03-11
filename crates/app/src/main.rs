#[macro_use(defer_on_unwind)]
extern crate scopeguard;

mod app_state;
mod cli;

use std::io;
use std::process;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use tracing::info;

use alloy_core::{config::Config, document::Document};
use app_state::AppState;
use cli::CliArgs;

// Terminal lifecycle

/// Restore the terminal to its normal state.
///
/// Called from both the clean exit path and the panic hook.
/// Safe to call multiple times (crossterm silently ignores double-disable).
fn restore_terminal() {
    let _ = disable_raw_mode();
    let _ = execute!(io::stderr(), LeaveAlternateScreen);
}

// Entry point

fn main() {
    // 1. Logging - initialise before anything else so boot messages are captured.
    //    Controlled via RUST_LOG env var.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("warn")),
        )
        // Log to stderr so it does not interfere with the alternate screen.
        .with_writer(io::stderr)
        .init();

    // 2. Panic hook - must be installed before raw mode is entered.
    std::panic::set_hook(Box::new(|info| {
        restore_terminal();
        eprintln!("\nalloy panicked: {info}");
    }));

    if let Err(err) = run() {
        restore_terminal();
        eprintln!("error: {err:#}");
        process::exit(1);
    }
}

fn run() -> Result<()> {
    // CLI args
    let args = CliArgs::parse();

    // Config loading
    let config = Config::load().context("failed to load configuration")?;

    info!(engine = ?config.markdown.engine, "config loaded");

    // Document loading
    let document = match &args.file {
        Some(path) => {
            if !path.exists() {
                // Non-existent path: create a new empty document associated with that path.
                // The file will be created on first save.
                info!(path = %path.display(), "file not found; starting empty buffer");
                let mut doc = Document::empty();
                doc.path = Some(path.clone());
                doc
            } else {
                Document::from_path(path)
                    .with_context(|| format!("failed to open '{}'", path.display()))?
            }
        }
        None => Document::empty(),
    };

    // Application state
    let mut state = AppState::new(config, document);

    // Terminal setup
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen).context("failed to enter alternate screen")?;

    // Use a scopeguard so the terminal is always restored even if an error propagates out of the event loop.
    defer_on_unwind! {
        restore_terminal();
    }

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    // Event loop
    while !state.should_quit {
        terminal.draw(|frame| render(frame, &state))?;

        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    match (key.code, key.modifiers) {
                        // Quit on `q` (Normal mode placeholder) or Ctrl+c.
                        (KeyCode::Char('q'), KeyModifiers::NONE)
                        | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                            state.should_quit = true;
                        }
                        _ => {}
                    }
                }
                Event::Resize(_, _) => {
                    // Ratatui redraws on the next frame automatically.
                }
                _ => {}
            }
        }
    }

    // Clean exit
    restore_terminal();
    Ok(())
}

// Rendering

fn render(frame: &mut ratatui::Frame, state: &AppState) {
    let area = frame.area();

    // Vertical split: editor body (top) + status bar (bottom, 1 line).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(1), Constraint::Length(1)])
        .split(area);

    // Editor placeholder
    let editor_block = Block::default().borders(Borders::ALL).title(Span::styled(
        format!(" {} ", state.status_filename()),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    ));

    let help_text = Paragraph::new(vec![
        Line::from(""),
        Line::from(vec![
            Span::raw("  "),
            Span::styled("alloy", Style::default().add_modifier(Modifier::BOLD)),
            Span::raw(" - TUI Markdown editor"),
        ]),
        Line::from(""),
        Line::from("  Press q to quit."),
        Line::from(""),
        Line::from(vec![
            Span::raw("  Engine: "),
            Span::styled(
                format!("{:?}", state.config.markdown.engine),
                Style::default().fg(Color::Yellow),
            ),
        ]),
        Line::from(vec![
            Span::raw("  GFM:    "),
            Span::styled(
                state.config.markdown.extensions.gfm.to_string(),
                Style::default().fg(Color::Green),
            ),
        ]),
    ])
    .block(editor_block)
    .wrap(Wrap { trim: false });

    frame.render_widget(help_text, chunks[0]);

    // Status bar
    let mode_label = Span::styled(
        " NORMAL ",
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD),
    );

    let file_label = Span::styled(
        format!(" {} ", state.status_filename()),
        Style::default().fg(Color::White),
    );

    let stats_label = Span::styled(
        format!(
            " {}L {}C ",
            state.document.line_count(),
            state.document.char_count(),
        ),
        Style::default().fg(Color::DarkGray),
    );

    let status_bar = Paragraph::new(Line::from(vec![mode_label, file_label, stats_label]))
        .style(Style::default().bg(Color::DarkGray));

    frame.render_widget(status_bar, chunks[1]);
}
