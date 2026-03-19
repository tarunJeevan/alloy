//! Terminal markdown editor entry point.
//!
//! Responsibilities:
//! - Parse CLI args
//! - Load user config
//! - Open the initial document (or create an empty one)
//! - Set up the terminal (raw mode, alternate screen, panic hook)
//! - Run the event loop
//! - Restore the terminal on exit (clean path AND panic path)

// #[macro_use(defer_on_unwind)]
// extern crate scopeguard;

mod app;
mod cli;
mod keymap;
mod ui;

use std::{io, time::Duration};

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};

use alloy_core::{config::Config, document::Document};
use app::App;
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

fn main() -> Result<()> {
    // Logging - initialise before anything else so boot messages are captured.
    tracing_subscriber::fmt()
        .with_writer(io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive(tracing::Level::WARN.into()),
        )
        .init();

    // CLI + Config - parse CLI args and generate config
    let args = CliArgs::parse();
    let config = Config::load().unwrap_or_else(|e| {
        tracing::warn!("Config load failed: ({e:#}); using defaults");
        Config::default()
    });

    // Open initial document
    let document = match &args.file {
        Some(path) => {
            Document::open(path).with_context(|| format!("failed to open '{}'", path.display()))?
        }
        None => Document::new(),
    };

    // Build app - initialize app state with config and opened document
    let mut app = App::new(config, document);

    // Terminal setup - enable raw mode and enter alt screen
    enable_raw_mode().context("failed to enable raw mode")?;
    let mut stderr = io::stderr();
    execute!(stderr, EnterAlternateScreen).context("failed to enter alternate screen")?;

    // Panic hook - must be installed before raw mode is entered.
    let original_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        original_hook(info);
    }));

    let backend = CrosstermBackend::new(io::stderr());
    let mut terminal = Terminal::new(backend).context("failed to create terminal")?;

    // Event loop - start main event loop
    let result = run_event_loop(&mut terminal, &mut app);

    // Clean exit - restore terminal before returning
    restore_terminal();
    result
}

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
) -> Result<()> {
    while !app.should_quit {
        // 1. Drain any timed-out pending key sequences and expired notifications before rendering.
        app.tick().context("tick error")?;

        // 2. Render.
        terminal.draw(|frame| ui::render(frame, app))?;

        // 3. Poll for the next event with a short timeout so tick() can fire even when no keys are pressed (needed for sequence timeout).
        // Adjust the 16ms timer as needed.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    // Ignore key-release and key-repeat events - only act on key-press.
                    if key.kind == KeyEventKind::Press {
                        if let Err(e) = app.handle_key(key) {
                            // Surface errors in the notification queue.
                            app.notify_error(format!("{e:#}"));
                            tracing::error!("handle_key error: {e:#}");
                        }
                    }
                }
                Event::Resize(_, _) => {
                    // Ratatui redraws on the next frame automatically.
                }
                _ => {}
            }
        }
    }

    Ok(())
}
