//! TUI layout and rendering.
//!
//! All Ratatui widget composition lives here. `main.rs` calls `render` once per frame.
//! No mutable state is stored in this module - everything is derived from `App` on each call.

use ratatui::{
    Frame,
    layout::{Constraint, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph},
};

use alloy_core::EditorMode;

use crate::app::App;

// ------------------------------------------------------------------
// Top-level render entry point
// ------------------------------------------------------------------

/// Render the full UI for one frame.
///
/// Layout example (vertical split):
/// ┌──────────────────────────────┐
/// │  editor body (tui-textarea)  │  ← Constraint::Min(1)
/// ├──────────────────────────────┤
/// │  status bar (1 line)         │  ← Constraint::Length(1)
/// └──────────────────────────────┘
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let [body_area, status_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    render_editor(frame, app, body_area);
    render_status(frame, app, status_area);
}

// ------------------------------------------------------------------
// Editor pane
// ------------------------------------------------------------------

fn render_editor(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
    // Style the block border differently per mode so the user gets a clear peripheral signal about which mode they're in.
    let (border_color, border_style_modifier) = match app.mode {
        EditorMode::Insert => (Color::LightCyan, Modifier::BOLD),
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
                .add_modifier(border_style_modifier),
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
// Status bar
// ------------------------------------------------------------------

/// Render the one-line status bar.
///
/// Format:
/// INSERT   filename.md [+]   |   42:7   |   1,234 words
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

    let words = word_count(app.textarea.lines());
    let word_span = Span::styled(
        format!(" {words} words"),
        Style::default().fg(Color::DarkGray),
    );

    let line = Line::from(vec![
        mode_span,
        filename_span,
        sep.clone(),
        pos_span,
        sep,
        word_span,
    ]);

    let status_bar = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));

    frame.render_widget(status_bar, area);
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
}
