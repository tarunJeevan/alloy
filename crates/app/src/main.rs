//! Terminal markdown editor entry point.
//!
//! Responsibilities:
//! - Parse CLI args
//! - Load user config
//! - Open the initial document (or create an empty one)
//! - Detect the terminal image protocol and build a `Picker`
//! - Set up the terminal (raw mode, alternate screen, panic hook)
//! - Run the event loop
//! - Restore the terminal on exit (clean path AND panic path)

use std::{io, time::Duration};

use anyhow::{Context, Result};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    crossterm::{
        event::{self, Event, KeyEventKind},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    },
};
use ratatui_image::picker::Picker;

use alloy_app::{App, CliArgs, image_proto::detect_from_env, ui};
use alloy_core::{config::Config, document::Document};

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

    // Detect image protocols supported by the terminal and use the most suitable one.
    let picker = {
        if !config.images.enabled
            || config.images.protocol == alloy_core::config::ImageProtocol::Off
        {
            // Detection disabled by config.
            tracing::debug!("image: protocol detection skipped (disabled in config)");
            None
        } else {
            // Attempt full ratatui-image Picker detection with a timeout.
            detect_picker(Duration::from_millis(200))
        }
    };

    tracing::debug!(protocol = ?picker.as_ref().map(|p| p.protocol_type()), "image protocol detection complete");

    // Build app - initialize app state with config and opened document
    let mut app = App::new(config, document, picker);

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

// ------------------------------------------------------------
// Image protocol detection
// ------------------------------------------------------------

/// Attempt image protocol detection via `ratatui-image`'s Picker, with a thread + timeout guard.
///
/// On success, returns the detected `Picker`.
/// On timeout or error (common in tmux, screen), falls back to env-var heuristics.
fn detect_picker(timeout: Duration) -> Option<Picker> {
    use std::sync::mpsc;

    let (tx, rx) = mpsc::channel::<Result<Picker, String>>();

    // Run 'Picker::from_query_stdio()' on a dedicated thread so we can enforce a timeout.
    // The thread sends its result back via the channel.
    std::thread::spawn(move || {
        let result = Picker::from_query_stdio().map_err(|e| e.to_string());
        let _ = tx.send(result);
    });

    match rx.recv_timeout(timeout) {
        Ok(Ok(picker)) => {
            tracing::debug!(
            protocol = ?picker.protocol_type(),
            font_size = ?picker.font_size(),
            "image: Picker::from_query_stdio succeeded"
            );
            Some(picker)
        }
        Ok(Err(e)) => {
            // Picker query failed - fall back to env-var heuristics.
            tracing::debug!(
                "image: Picker::from_query_stdio failed ({e}); falling back to env-var detection"
            );
            picker_from_env()
        }
        Err(_timeout) => {
            // Timed out (likely tmux/screen) - fall back to env-var heuristics.
            tracing::warn!(
                "image: protocol detection timed out after {}ms; using env-var heuristics",
                timeout.as_millis()
            );
            picker_from_env()
        }
    }
}

/// Construct a `Picker` based solely on env-var heuristics.
///
/// Used as a fallback when `Picker::from_query_stdio` is unavailable or times out.
/// Font size defaults to (8, 16). Images will still render but may be proportioned slightly differently.
fn picker_from_env() -> Option<Picker> {
    use alloy_app::image_proto::DetectedImageProtocol;
    use ratatui_image::picker::ProtocolType;

    let proto = detect_from_env();
    tracing::debug!(?proto, "image: env-var fallback protocol");

    // Start from a halfblocks base (always safe default), then override the protocol type to match what the env vars suggest.
    let mut picker = Picker::halfblocks();
    let protocol_type = match proto {
        DetectedImageProtocol::Kitty => ProtocolType::Kitty,
        DetectedImageProtocol::Iterm2 => ProtocolType::Iterm2,
        DetectedImageProtocol::Sixel => ProtocolType::Sixel,
        DetectedImageProtocol::HalfBlock => ProtocolType::Halfblocks,
        DetectedImageProtocol::None => return None,
    };

    picker.set_protocol_type(protocol_type);
    Some(picker)
}

// ------------------------------------------------------------
// Event loop
// ------------------------------------------------------------

fn run_event_loop(
    terminal: &mut Terminal<CrosstermBackend<io::Stderr>>,
    app: &mut App,
) -> Result<()> {
    while !app.should_quit {
        // 1. Drain timed-out key sequences, expired notifications, and preview results.
        app.tick().context("tick error")?;

        // 2. Render.
        terminal.draw(|frame| ui::render(frame, app))?;

        // 3. Poll for the next event with a short timeout so tick() can fire even when no keys are pressed (needed for sequence timeout).
        // Adjust the 16ms timer as needed.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(key) => {
                    // Ignore key-release and key-repeat events - only act on key-press.
                    if key.kind == KeyEventKind::Press
                        && let Err(e) = app.handle_key(key)
                    {
                        // Surface errors in the notification queue.
                        app.notify_error(format!("{e:#}"));
                        tracing::error!("handle_key error: {e:#}");
                    }
                }
                Event::Resize(_, _) => {
                    // Ratatui redraws on the next frame automatically.
                    // Invalidate image cache on resize so images are re-encoded at the new cell dimensions on the next render.
                    app.on_resize();
                }
                _ => {}
            }
        }
    }

    Ok(())
}
