//! TUI layout and rendering.
//!
//! All Ratatui widget composition lives here. `main.rs` calls `render` once per frame.
//! No mutable state is stored in this module - everything is derived from `App` on each call.

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use alloy_core::EditorMode;

use crate::app::{App, NotificationLevel, PreviewMode};

/// Minimum total body width (columns) before the split is suppressed.
const MIN_SPLIT_WIDTH: u16 = 40;

// ------------------------------------------------------------------
// Top-level render entry point
// ------------------------------------------------------------------

/// Render the full UI for one frame.
///
/// Layout example (Normal/Insert modes):
/// ┌──────────────────────────────┐
/// │  editor body (tui-textarea)  │  ← Constraint::Min(1)
/// ├──────────────────────────────┤
/// │  status bar (1 line)         │  ← Constraint::Length(1)
/// └──────────────────────────────┘
///
/// Layout example (Command mode):
/// ┌──────────────────────────────┐
/// │  editor body (tui-textarea)  │  ← Constraint::Min(1)
/// ├──────────────────────────────┤
/// │  :command_input_here_        │  ← Constraint::Length(1)  (replaces status bar)
/// └──────────────────────────────┘
///
/// Layout example (preview mode):
/// ┌────────────────────────────────────────────────┐
/// │  editor pane (full width)                      │
/// ├────────────────────────────────────────────────┤
/// │  status bar                                    │
/// └────────────────────────────────────────────────┘
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let [body_area, bottom_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    // Horizontal: editor (left) + optional preview (right)
    let (editor_area, opt_preview_area) = split_body(app, body_area);

    render_editor(frame, app, editor_area);

    if let Some(preview_area) = opt_preview_area {
        render_preview(frame, app, preview_area);
    }

    if app.mode == EditorMode::Command {
        render_command_prompt(frame, app, bottom_area);
    } else {
        render_status(frame, app, bottom_area);
    }
}

// ------------------------------------------------------------------
// Layout helpers
// ------------------------------------------------------------------
// ------------------------------------------------------------------

/// Compute the editor area and an optional preview area from the body rect.
///
/// Returns `(editor_area, Some(preview_area))` when the preview is visible and there is enough horizontal space, or `(body_area, None)` otherwise.
fn split_body(app: &App, body: Rect) -> (Rect, Option<Rect>) {
    let preview_visible = app.preview_mode != PreviewMode::Hidden && body.width >= MIN_SPLIT_WIDTH;

    if !preview_visible {
        return (body, None);
    }

    let ratio = app.config.ui.split_ratio.clamp(10, 90) as u16;
    let [left, right] = Layout::horizontal([
        Constraint::Percentage(ratio),
        Constraint::Percentage(100 - ratio),
    ])
    .areas(body);

    (left, Some(right))
}

// Editor pane
// ------------------------------------------------------------------

fn render_editor(frame: &mut Frame, app: &mut App, area: Rect) {
    // Style the block border differently per mode so the user gets a clear peripheral signal about which mode they're in.
    let (border_color, border_modifier) = match app.mode {
        EditorMode::Insert => (Color::LightCyan, Modifier::BOLD),
        EditorMode::Command => (Color::Magenta, Modifier::BOLD),
        //* NOTE: Adjust color settings for other modes or import from user config
        _ => (Color::DarkGray, Modifier::empty()),
    };

    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(
            Style::default()
                .fg(border_color)
                .add_modifier(border_modifier),
        )
        .title(Span::styled(
            format!(" {} ", app.status_filename()),
            title_style,
        ));

    app.textarea.set_block(block);

    // `widget()` returns a reference type that implements `Widget`.
    // Using it avoids the `&mut` borrow ambiguity that arises with some versions of tui-textarea.
    frame.render_widget(&app.textarea, area);
}

// ------------------------------------------------------------------
// Preview pane
// ------------------------------------------------------------------

fn render_preview(frame: &mut Frame, app: &App, area: Rect) {
    let (title, border_color) = match app.preview_mode {
        PreviewMode::Rendered => (" Preview ", Color::DarkGray),
        PreviewMode::Html => (" HTML ", Color::DarkGray),
        PreviewMode::Hidden => unreachable!("render_preview called with Hidden mode"),
    };

    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, Style::default().fg(Color::DarkGray)));

    // Wrap::Word is intentionally NOT set here.
    // NOTE: The Markdown renderer (Chunk 3.2) handles line-wrapping at render time so that styled spans are never broken mid-token.
    // NOTE: The stub renderer produces plain lines that don't need wrapping.
    let widget = Paragraph::new(app.preview_text.clone())
        .block(block)
        .scroll((app.preview_scroll, 0));

    frame.render_widget(widget, area);
}

// ------------------------------------------------------------------
// Command prompt (replaces status bar in Command mode)
// ------------------------------------------------------------------

/// Render the single-line command prompt.
///
/// Format:
/// `:wq_`
fn render_command_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let prompt = Span::styled(
        format!(":{}_", app.command_input),
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 30, 30))
            .add_modifier(Modifier::BOLD),
    );

    let widget =
        Paragraph::new(Line::from(vec![prompt])).style(Style::default().bg(Color::Rgb(30, 30, 30)));
    frame.render_widget(widget, area);
}

// ------------------------------------------------------------------
// Status bar
// ------------------------------------------------------------------

/// Render the one-line status bar.
///
/// Format:
/// INSERT   filename.md [+]   |   42:7   |   1,234 words
///
/// When a notification is active, the right-hand segment is replaced by the notification message, colored by severity.
fn render_status(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    let (mode_bg, mode_fg) = mode_colors(&app.mode);

    let mode_span = Span::styled(
        format!(" {} ", app.mode.label()),
        Style::default()
            .fg(mode_fg)
            .bg(mode_bg)
            .add_modifier(Modifier::BOLD),
    );

    let sep = Span::styled(" | ", Style::default().fg(Color::DarkGray));

    let filename_span = Span::styled(
        format!(" {} ", app.status_filename()),
        Style::default().fg(Color::White),
    );

    let (row, col) = app.cursor_position();
    let pos_span = Span::styled(format!(" {row}:{col}"), Style::default().fg(Color::Yellow));

    // Preview mode indicator (e.g. "P:Rendered]")
    let preview_label = match app.preview_mode {
        PreviewMode::Rendered => "[P:Rendered]",
        PreviewMode::Html => "[P:HTML]",
        PreviewMode::Hidden => "[P:Off]",
    };
    let preview_span = Span::styled(
        format!(" {preview_label}"),
        Style::default().fg(Color::DarkGray),
    );

    // Right-hand segment: notification or word count.
    let right_span = if let Some(notif) = app.active_notification() {
        let color = match notif.level {
            NotificationLevel::Info => Color::Green,
            NotificationLevel::Warn => Color::Yellow,
            NotificationLevel::Error => Color::Red,
        };
        Span::styled(
            format!(" {} ", notif.message),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )
    } else {
        let words = word_count(app.textarea.lines());
        Span::styled(
            format!(" {words} words"),
            Style::default().fg(Color::DarkGray),
        )
    };

    let line = Line::from(vec![
        mode_span,
        filename_span,
        sep.clone(),
        pos_span,
        sep.clone(),
        preview_span,
        sep,
        right_span,
    ]);

    let widget = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));

    frame.render_widget(widget, area);
}

// ------------------------------------------------------------------
// Helpers
// ------------------------------------------------------------------

/// Background and foreground colors for the mode indicator pill.
fn mode_colors(mode: &EditorMode) -> (Color, Color) {
    match mode {
        EditorMode::Normal => (Color::Green, Color::Black),
        EditorMode::Insert => (Color::LightCyan, Color::Black),
        EditorMode::Search => (Color::Yellow, Color::Black),
        EditorMode::Command => (Color::Magenta, Color::Black),
        EditorMode::LinkSelect => (Color::Blue, Color::White),
    }
}

/// Count whitespace-delimited words across all textarea lines.
///
/// This is 0(chars) but called only once per frame, which is acceptable.
/// For very large documents, this could be cached and invalidated on edit.
pub fn word_count(lines: &[impl AsRef<str>]) -> usize {
    lines
        .iter()
        .flat_map(|l| l.as_ref().split_whitespace())
        .count()
}

// ------------------------------------------------------------------
// Tests
// ------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn word_count_empty() {
        let lines: Vec<&str> = vec![""];

        // word_count should return no words
        assert_eq!(word_count(&lines), 0);
    }

    #[test]
    fn word_count_single_line() {
        let lines = vec!["hello world foo"];

        // word_count should return 3 words
        assert_eq!(word_count(&lines), 3);
    }

    #[test]
    fn word_count_multi_line() {
        let lines = vec!["Title", "", "Some text here.", "Another line."];

        // word_count should return 6 words, disregarding whitespace and special characters
        assert_eq!(word_count(&lines), 6);
    }

    #[test]
    fn word_count_leading_trailing_whitespace() {
        let lines = vec!["  hello   world  "];

        // word_count should return 2 words, disregarding leading or trailing whitespace
        assert_eq!(word_count(&lines), 2);
    }

    #[test]
    fn word_count_whitespace_only() {
        assert_eq!(word_count(&["   ", "\t\t"]), 0);
    }

    // split_body logic - tested via the Rect math directly

    #[test]
    fn narrow_terminal_suppresses_split() {
        use crate::app::App;
        use alloy_core::{config::Config, document::Document};

        let app = App::new(Config::default(), Document::new());

        // Simulate a very narrow body rect
        let naroow = Rect::new(0, 0, MIN_SPLIT_WIDTH - 1, 20);
        let (_, preview) = split_body(&app, naroow);

        assert!(
            preview.is_none(),
            "preview should be suppressed when width < MIN_SPLIT_WIDTH"
        );
    }

    #[test]
    fn wide_terminal_produces_split() {
        use crate::app::App;
        use alloy_core::{config::Config, document::Document};

        let app = App::new(Config::default(), Document::new());

        let wide = Rect::new(0, 0, 120, 40);
        let (_, preview) = split_body(&app, wide);

        assert!(
            preview.is_some(),
            "preview should be visible when width > MIN_SPLIT_WIDTH"
        );
    }

    #[test]
    fn hidden_mode_suppresses_split_regardless_of_width() {
        use crate::app::App;
        use alloy_core::{config::Config, document::Document};

        let mut app = App::new(Config::default(), Document::new());
        app.preview_mode = PreviewMode::Hidden;

        let wide = Rect::new(0, 0, 200, 50);
        let (_, preview) = split_body(&app, wide);

        assert!(
            preview.is_none(),
            "preview should be suppressed when PreviewMode::Hidden"
        );
    }
}
