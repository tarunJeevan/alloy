//! TestBackend integration tests for the full UI rendering pipeline.
//!
//! These tests render actual Ratatui frames into an in-memory `TestBackend` buffer and assert on the resulting text, which lets us verify:
//! - The app boots without panicking.
//! - Mode transitions are reflected in the status bar.
//! - The command-mode prompt replaces the status bar correctly.
//! - The preview pane renders without panicking across all three modes.
//!
//! `TestBackend` never touches the real terminal so these tests are safe to run on all platforms and in CI without a PTY.

use alloy_app::{App, DetectedImageProtocol, EditorAction, PreviewMode};
use alloy_core::{config::Config, document::Document};

use ratatui::{Terminal, backend::TestBackend};

// ------------------------------------------------------------
// Helpers
// ------------------------------------------------------------

/// Create a minimal `App` suitable for headless rendering tests.
///
/// Uses `DetectedImageProtocol::None` so no image protocol detection runs and `Config::default()` so no disk I/O is needed.
fn make_test_app() -> App {
    App::new(
        Config::default(),
        Document::new(),
        DetectedImageProtocol::None,
    )
}

/// Collect all call symbols from the terminal buffer into a single `String`.
///
/// This flattens the 2D buffer into a flat string. Ratatui fills unused calls with a space so `contains("FOO")` is reliable for short substrings that appear on the same row.
fn buffer_text(terminal: &Terminal<TestBackend>) -> String {
    terminal
        .backend()
        .buffer()
        .content()
        .iter()
        .map(|cell| cell.symbol())
        .collect()
}

// ------------------------------------------------------------
// Smoke tests - the app must not panic on render
// ------------------------------------------------------------

#[test]
fn app_renders_without_panic() {
    let mut app = make_test_app();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    // Should not panic regardless of the (empty) initial state.
    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn app_renders_on_very_narrow_terminal() {
    // Verify the layout guard (MIN_SPLIT_WIDTH) prevents a panic on tiny widths.
    let mut app = make_test_app();
    let backend = TestBackend::new(30, 10);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn app_renders_with_document_content() {
    // Seed content by writing a temp file and opening it via Document::open.
    use std::io::Write;
    let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
    writeln!(
        tmp,
        "# Heading\n\nSome paragraph text.\n\n- item 1\n- item 2"
    )
    .unwrap();
    let path = tmp.path().to_path_buf();

    let doc = Document::open(&path).expect("open temp file");
    let mut app = App::new(Config::default(), doc, DetectedImageProtocol::None);

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();
}

// ------------------------------------------------------------
// Mode label tests - status bar must reflect the current EditorMode
// ------------------------------------------------------------

#[test]
fn normal_mode_label_appears_in_status_bar() {
    let mut app = make_test_app();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    let text = buffer_text(&terminal);

    assert!(
        text.contains("NORMAL"),
        "status bar should display 'NORMAL' mode indicator; buffer: {text:?}"
    );
}

#[test]
fn insert_mode_label_appears_after_enter_insert() {
    let mut app = make_test_app();
    app.handle_action(EditorAction::EnterInsert).unwrap();

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    let text = buffer_text(&terminal);

    assert!(
        text.contains("INSERT"),
        "status bar should display 'INSERT' mode indicator; buffer: {text:?}"
    );
}

#[test]
fn mode_returns_to_normal_after_exit_insert() {
    let mut app = make_test_app();
    app.handle_action(EditorAction::EnterInsert).unwrap();
    app.handle_action(EditorAction::ExitInsert).unwrap();

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    let text = buffer_text(&terminal);

    assert!(
        text.contains("NORMAL"),
        "should return to NORMAL after ExitInsert; buffer: {text:?}"
    );
    assert!(
        !text.contains("INSERT"),
        "INSERT label should not appear after ExitInsert; buffer: {text:?}"
    );
}

// ------------------------------------------------------------
// Command mode - prompt replaces the status bar
// ------------------------------------------------------------

#[test]
fn command_mode_renders_colon_prompt() {
    let mut app = make_test_app();
    app.handle_action(EditorAction::EnterCommand).unwrap();

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    // Command prompt renders ":" followed by the (empty) command_input.
    let text = buffer_text(&terminal);

    assert!(
        text.contains(':'),
        "command mode should render ':' prompt; buffer: {text:?}"
    );
}

#[test]
fn command_mode_shows_typed_characters() {
    let mut app = make_test_app();
    app.handle_action(EditorAction::EnterCommand).unwrap();
    app.handle_action(EditorAction::CommandInput('w')).unwrap();
    app.handle_action(EditorAction::CommandInput('q')).unwrap();

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    let text = buffer_text(&terminal);

    // The command ":wq" should be visible in the bottom row.
    assert!(
        text.contains("wq"),
        "command input 'wq' should be visible in the prompt; buffer: {text:?}"
    );
}

// -------------------------------------------------------------
// Preview mode rendering — no panic across all three states
// -------------------------------------------------------------

#[test]
fn rendered_preview_mode_renders_both_panes() {
    let mut app = make_test_app();

    assert_eq!(app.preview_mode, PreviewMode::Rendered);

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn html_preview_mode_renders_without_panic() {
    let mut app = make_test_app();
    app.preview_mode = PreviewMode::Html;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();
}

#[test]
fn hidden_preview_mode_renders_full_width_editor() {
    let mut app = make_test_app();
    app.preview_mode = PreviewMode::Hidden;

    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    terminal
        .draw(|frame| alloy_app::ui::render(frame, &mut app))
        .unwrap();

    // In hidden mode, NORMAL should still appear (the editor has full width).
    let text = buffer_text(&terminal);

    assert!(
        text.contains("NORMAL"),
        "hidden preview mode should still render the status bar; buffer: {text:?}"
    );
}

#[test]
fn preview_toggle_cycle_renders_all_three_modes_without_panic() {
    let mut app = make_test_app();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    for _ in 0..3 {
        app.handle_action(EditorAction::TogglePreview).unwrap();
        terminal
            .draw(|frame| alloy_app::ui::render(frame, &mut app))
            .unwrap();
    }

    // After 3 toggles we're back to Rendered.
    assert_eq!(app.preview_mode, PreviewMode::Rendered);
}

// -------------------------------------------------------------
// Mode-transtion round-trip
// -------------------------------------------------------------

#[test]
fn multiple_mode_transitions_render_cleanly() {
    let mut app = make_test_app();
    let backend = TestBackend::new(120, 40);
    let mut terminal = Terminal::new(backend).unwrap();

    let transitions: &[EditorAction] = &[
        EditorAction::EnterInsert,
        EditorAction::ExitInsert,
        EditorAction::EnterCommand,
        EditorAction::ExitInsert, // exits Command mode too
        EditorAction::EnterLiteralSearch,
        EditorAction::CancelSearch,
    ];

    for action in transitions {
        app.handle_action(action.clone()).unwrap();
        // Each intermediate state must render without panicking.
        terminal
            .draw(|frame| alloy_app::ui::render(frame, &mut app))
            .unwrap();
    }

    // After all transitions we should be back in Normal mode.
    let text = buffer_text(&terminal);

    assert!(
        text.contains("NORMAL"),
        "final state should be NORMAL; buffer: {text:?}"
    );
}
