#[macro_use(defer_on_unwind)]
extern crate scopeguard;

mod app;
mod app_state;
mod cli;
mod keymap;
mod ui;

use std::io;
use std::process;
use std::time::Duration;

use anyhow::{Context, Result};
use crossterm::{
    event::{self, Event, KeyEventKind},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use tracing::info;

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
                let mut doc = Document::new();
                doc.path = Some(path.clone());
                doc
            } else {
                Document::open(path)
                    .with_context(|| format!("failed to open '{}'", path.display()))?
            }
        }
        None => Document::new(),
    };

    // Application state
    let mut app = App::new(config, document);

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
    while !app.should_quit {
        // 1. Drain any timed-out pending sequences before rendering.
        app.tick().context("tick error")?;

        // 2. Render.
        terminal.draw(|frame| ui::render(frame, &mut app))?;

        // 3. Poll for the next event with a short timeout so tick() can fire even when no keys are pressed (needed for sequence timeout).
        // Adjust the 16ms timer as needed.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    // Ignore key-release and key-repeat events - only act on key-press.
                    if key.kind == KeyEventKind::Press {
                        if let Err(e) = app.handle_key(key) {
                            // For now, log error and continue. Chunk 2.3 will surface errors in the notification queue.
                            tracing::error!("handle_key error: {e:#}");
                        }
                    }
                    // match (key.code, key.modifiers) {
                    //     // Quit on `q` (Normal mode placeholder) or Ctrl+c.
                    //     (KeyCode::Char('q'), KeyModifiers::NONE)
                    //     | (KeyCode::Char('c'), KeyModifiers::CONTROL) => {
                    //         state.should_quit = true;
                    //     }
                    //     _ => {}
                    // }
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
