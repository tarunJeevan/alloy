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

use crate::app::{App, NotificationLevel};

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
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let [body_area, bottom_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    render_editor(frame, app, body_area);

    if app.mode == EditorMode::Command {
        render_command_prompt(frame, app, bottom_area);
    } else {
        render_status(frame, app, bottom_area);
    }
}

// ------------------------------------------------------------------
// Editor pane
// ------------------------------------------------------------------

fn render_editor(frame: &mut Frame, app: &mut App, area: ratatui::layout::Rect) {
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
// Command prompt (replaces status bar in Command mode)
// ------------------------------------------------------------------

/// Render the single-line command prompt.
///
/// Format:
/// `:wq_`
fn render_command_prompt(frame: &mut Frame, app: &App, area: ratatui::layout::Rect) {
    let prompt = Span::styled(
        format!(":{}_", app.command_input),
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(30, 30, 30))
            .add_modifier(Modifier::BOLD),
    );

    let line = Line::from(vec![prompt]);
    let widget = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));
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
}
