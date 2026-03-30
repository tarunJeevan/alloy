//! TUI layout and rendering.
//!
//! All Ratatui widget composition lives here. `main.rs` calls `render` once per frame.
//! No mutable state is stored in this module - everything is derived from `App` on each call.

use std::collections::HashSet;

use ratatui::{
    Frame,
    layout::{Constraint, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Paragraph},
};

use alloy_core::{
    EditorMode,
    links::{LinkIndex, LinkTarget},
};

use crate::app::{App, NotificationLevel, PreviewMode};

/// Minimum total body width (columns) before the split is suppressed.
const MIN_SPLIT_WIDTH: u16 = 40;

// ---------------------------------------------------------------
// Top-level render entry point
// ---------------------------------------------------------------

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
/// │  editor pane (50%) / preview pane (50%)        │
/// ├────────────────────────────────────────────────┤
/// │  status bar                                    │
/// └────────────────────────────────────────────────┘
///
/// Layout example (search mode):
/// ┌────────────────────────────────────────────────┐
/// │  editor body (tui-textarea)                    │
/// ├────────────────────────────────────────────────┤
/// │  status bar / search prompt                    │
/// └────────────────────────────────────────────────┘
pub fn render(frame: &mut Frame, app: &mut App) {
    let area = frame.area();

    let [body_area, bottom_area] =
        Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(area);

    // Horizontal: editor (left) + optional preview (right)
    let (editor_area, opt_preview_area) = split_body(app, body_area);

    // Update the cached preview width before any render call that might queue a new render request.
    if let Some(preview_area) = opt_preview_area {
        // Subtract 2 for left+right borders. Clamp to a min of 20.
        app.last_preview_width = preview_area.width.saturating_sub(2).max(20);
        render_editor(frame, app, editor_area);
        render_preview(frame, app, preview_area);
    } else {
        app.last_preview_width = 80; // Sensible fallback when preview is hidden
        render_editor(frame, app, editor_area);
    }

    match app.mode {
        EditorMode::Command => render_command_prompt(frame, app, bottom_area),
        EditorMode::Search => render_search_prompt(frame, app, bottom_area),
        EditorMode::LinkSelect => render_link_select_prompt(frame, app, bottom_area),
        _ => render_status(frame, app, bottom_area),
    }
}

// ---------------------------------------------------------------
// Layout helpers
// ---------------------------------------------------------------

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

// ---------------------------------------------------------------
// Editor pane
// ---------------------------------------------------------------

fn render_editor(frame: &mut Frame, app: &mut App, area: Rect) {
    // Style the block border differently per mode so the user gets a clear peripheral signal about which mode they're in.
    let (border_color, border_modifier) = match app.mode {
        EditorMode::Insert => (Color::LightCyan, Modifier::BOLD),
        EditorMode::Command => (Color::Magenta, Modifier::BOLD),
        EditorMode::Search => (Color::Yellow, Modifier::BOLD),
        EditorMode::LinkSelect => (Color::Blue, Modifier::BOLD),
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

// ---------------------------------------------------------------
// Preview pane
// ---------------------------------------------------------------

fn render_preview(frame: &mut Frame, app: &mut App, area: Rect) {
    match app.preview_mode {
        PreviewMode::Rendered => render_preview_rendered(frame, app, area),
        PreviewMode::Html => render_preview_html(frame, app, area),
        PreviewMode::Hidden => unreachable!("render_preview called with Hidden mode"),
    }
}

/// Render the terminal-rendered Markdown preview.
fn render_preview_rendered(frame: &mut Frame, app: &mut App, area: Rect) {
    let block = preview_block(" Preview ", Color::DarkGray);
    let inner = block.inner(area);

    // Conditionally inject OSC-8 hyperlinks when the terminal supports them.
    let text = if app.hyperlinks_enabled() {
        inject_osc8(app.preview_text.clone(), &app.link_index)
    } else {
        app.preview_text.clone()
    };

    let widget = Paragraph::new(text)
        .block(block)
        .scroll((app.preview_scroll, 0));

    frame.render_widget(widget, area);

    // Attempt image rendering overlay when images are enabled.
    // This renders images over the placeholder span lines.
    if app.images_enabled() {
        render_preview_images(frame, app, inner);
    }
}

/// Render the raw HTML source preview.
///
/// NOTE: The HTML string is split on newlines, each line becomes an unstyled `Line`. A line cap of 2000 is enforced by `html_to_lines()` in the `markdown` crate before the result is stored on `App`.
///
/// Performs the conversion here at render time from `app.preview_html` (a String) because storing a pre-built `Text<'static>` for HTML would require an extra allocation in `App::tick()` on every render cycle even when the user is in Rendered mode.
fn render_preview_html(frame: &mut Frame, app: &App, area: Rect) {
    let block = preview_block(" HTML ", Color::DarkGray);

    // Build `Text` from the stored HTML string - unstyled, plain lines.
    let text = html_string_to_text(&app.preview_html);

    let widget = Paragraph::new(text)
        .block(block)
        .scroll((app.preview_scroll_html, 0));

    frame.render_widget(widget, area);
}

/// Build an unstyled `Text<'static>` from a raw HTML string.
///
/// NOTE: Each newline-delimited line becomes one `Line`. This is intentionally plain text - syntax highlighting the HTML source is deferred post-MVP (see Decision Log for more details).
fn html_string_to_text(html: &str) -> Text<'static> {
    use markdown::html_to_lines;

    let lines = html_to_lines(html);
    let ratatui_lines: Vec<Line<'static>> = lines
        .into_iter()
        .map(|l| Line::from(Span::raw(l)))
        .collect();

    Text::from(ratatui_lines)
}

/// Build a standard preview block with a given title and border color.
fn preview_block(title: &'static str, border_color: Color) -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(border_color))
        .title(Span::styled(title, Style::default().fg(Color::DarkGray)))
}

/// Render inline images into the preview area by overlaying `StatefulImage` widgets on top of the placeholder spans already drawn by `render_preview_rendered`.
///
/// NOTE: This is an intentionally simple image rendering method - "good enough" for MVP. A pixel-accurate layout would require the renderer to emit explicit image placement metadata.
fn render_preview_images(frame: &mut Frame, app: &mut App, inner: Rect) {
    use alloy_core::links::LinkTarget;
    use ratatui_image::{StatefulImage, protocol::StatefulProtocol};

    // Collect image URLs from the link index (filtering out heading anchors and regular links).
    let image_urls: Vec<(usize, String)> = app
        .link_index
        .0
        .iter()
        .filter_map(|link| {
            match &link.target {
                // Local file paths - check file extension for image types.
                LinkTarget::FilePath(path) => {
                    let ext = path.extension()?.to_str()?.to_lowercase();
                    matches!(
                        ext.as_str(),
                        "png" | "jpg" | "jpeg" | "gif" | "webp" | "bmp"
                    )
                    .then(|| (link.source_line, path.to_string_lossy().into_owned()))
                }
                // External URLs - check extensinon heuristic.
                LinkTarget::External(url) => {
                    let lower = url.to_lowercase();
                    (lower.ends_with(".png")
                        || lower.ends_with(".jpg")
                        || lower.ends_with(".jpeg")
                        || lower.ends_with(".gif")
                        || lower.ends_with("webp"))
                    .then(|| (link.source_line, url.clone()))
                }
                _ => None,
            }
        })
        .collect();

    if image_urls.is_empty() {
        return;
    }

    let Some(picker_arc) = &app.picker else {
        return;
    };
    let Ok(mut picker) = picker_arc.lock() else {
        return;
    };
    let Ok(mut cache) = app.image_cache.lock() else {
        return;
    };

    let fetch_remote = app.config.images.fetch_remote;
    let base_dir = app.document.path.as_deref().and_then(|p| p.parent());
    let scroll = app.preview_scroll as usize;

    for (source_line, url) in &image_urls {
        // Estimate the rendered row for this image.
        let row = source_line.saturating_sub(scroll);
        if row >= inner.height as usize {
            continue; // Image scrolled off screen
        }

        // Reserve a rendering area for image.
        let img_row = (inner.y + row as u16 + 1).min(inner.y + inner.height.saturating_sub(1));
        let img_height = 8u16.min(inner.y + inner.height - img_row);
        if img_height == 0 {
            continue;
        }

        let img_rect = Rect {
            x: inner.x,
            y: img_row,
            width: inner.width,
            height: img_height,
        };

        // Attempt cache lookup + load.
        if let Err(e) = cache.get_or_load(url, &mut picker, fetch_remote, base_dir) {
            tracing::debug!(url, error = %e, "image: load failed; using placeholder");
            continue;
        }

        // Mutable re-lookup for render_stateful_widget.
        if let Some(entry) = cache.get_mut(url) {
            let widget = StatefulImage::<StatefulProtocol>::default();
            frame.render_stateful_widget(widget, img_rect, &mut entry.protocol);
        }
    }
}

// ---------------------------------------------------------------
// Command prompt (replaces status bar in Command mode)
// ---------------------------------------------------------------

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

// ---------------------------------------------------------------
// Search prompt (replaces status bar in Search mode)
// ---------------------------------------------------------------

/// Render the single-line search prompt.
///
/// Format:
/// `/` OR `?` prefix, then the pattern, then `_` cursor, then the match counter right-aligned.
///
/// Example: `/ hello_					[2/4]`
fn render_search_prompt(frame: &mut Frame, app: &App, area: Rect) {
    use alloy_core::search::SearchKind;

    let prefix = match app.search_kind() {
        Some(SearchKind::Regex) => "?",
        _ => "/",
    };

    let pattern = app.search_pattern();
    let counter = app.search_counter_str().unwrap_or_else(|| "0/0".to_owned());

    // Left portion: `/ pattern_`
    let prompt_str = format!("{prefix} {pattern}_");
    let prompt_span = Span::styled(
        prompt_str,
        Style::default()
            .fg(Color::Yellow)
            .bg(Color::Rgb(30, 30, 30))
            .add_modifier(Modifier::BOLD),
    );

    // Right portion: `[2/3]
    let counter_str = format!("[{counter}]");
    let counter_span = Span::styled(
        &counter_str,
        Style::default().fg(Color::Cyan).bg(Color::Rgb(30, 30, 30)),
    );

    // Pad the middle with spaces so the counter appears right-aligned.
    // prompt_str + spaces + counter_str == area.width (approximately)
    let prompt_width = prefix.len() + 1 + pattern.len() + 1; // "/ pattern_"
    let counter_width = counter_str.len();
    let total = area.width as usize;
    let padding = total
        .saturating_sub(prompt_width)
        .saturating_sub(counter_width);

    let pad_span = Span::styled(
        " ".repeat(padding),
        Style::default().bg(Color::Rgb(30, 30, 30)),
    );

    let line = Line::from(vec![prompt_span, pad_span, counter_span]);
    let widget = Paragraph::new(line).style(Style::default().bg(Color::Rgb(30, 30, 30)));

    frame.render_widget(widget, area);
}

// ---------------------------------------------------------------
// LinkSelect prompt (replaces status bar in LinkSelect mode)
// ---------------------------------------------------------------

/// Render the single-line link-selection prompt.
///
/// Format:
/// `[LINKS 2/4] -> https://example.com		j/k:nav  Enter:follow  Esc:cancel`
fn render_link_select_prompt(frame: &mut Frame, app: &App, area: Rect) {
    let counter = app.link_select_counter();
    let target_str = app.current_link_display().unwrap_or("-");

    // Link type indicator
    let kind_label = app
        .link_index
        .get(app.link_cursor)
        .map(|l| match &l.target {
            LinkTarget::External(_) => "EXTERNAL",
            LinkTarget::InternalAnchor(_) => "ANCHOR",
            LinkTarget::WikiLink(_) => "WIKILINK",
            LinkTarget::FilePath(_) => "FILEPATH",
        })
        .unwrap_or("-");

    // Left portion: `[LINKS 2/3] EXTERNAL -> https://example.com`
    let left_str = format!("[LINKS {counter}] {kind_label} → {target_str}");
    let left_span = Span::styled(
        left_str.clone(),
        Style::default()
            .fg(Color::White)
            .bg(Color::Rgb(20, 40, 80))
            .add_modifier(Modifier::BOLD),
    );

    // Right portion: keyboard hints (shown only if there's room)
    let hint_str = " j/k:nav | Enter:follow | Esc:cancel ";
    let hint_span = Span::styled(
        hint_str,
        Style::default()
            .fg(Color::DarkGray)
            .bg(Color::Rgb(20, 40, 80)),
    );

    let left_width = left_str.len();
    let hint_width = hint_str.len();
    let total = area.width as usize;
    let padding = total.saturating_sub(left_width).saturating_sub(hint_width);

    let pad_span = Span::styled(
        " ".repeat(padding),
        Style::default().bg(Color::Rgb(20, 40, 80)),
    );

    // If the terminal is too narrow to show hints, omit them gracefully.
    let line = if total > left_width + hint_width + 2 {
        Line::from(vec![left_span, pad_span, hint_span])
    } else {
        Line::from(vec![left_span])
    };

    let widget = Paragraph::new(line).style(Style::default().bg(Color::Rgb(20, 40, 80)));

    frame.render_widget(widget, area);
}

// ---------------------------------------------------------------
// Status bar
// ---------------------------------------------------------------

/// Render the one-line status bar.
///
/// Format:
/// INSERT   filename.md [+]   |   42:7   |   [P:Rendered]   |   1,234 words
///
/// When a notification is active, the right-hand segment is replaced by the notification message, colored by severity.
fn render_status(frame: &mut Frame, app: &mut App, area: Rect) {
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
    } else if let Some(counter) = app.search_counter_str() {
        // Search match counter - visible in Normal mode after CommitSearch until the search is cancelled or a new search is started.
        Span::styled(
            format!(" [{counter}] "),
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
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

// ---------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------

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

/// Wrap link-text spans in OSC-8 escape sequences so supporting terminals render them as clickable hyperlinks.
///
/// The approach injects raw escape bytes into `Span` content. Ratatui passes the content string verbatim to the backend so the sequences reach the terminal without interpretation.
///
/// Matching is content-based: a span whose trimmed content equals a known external URL gets wrapped. This is approximate - it only catches spans where the renderer emitted the bare URL as content. It will not catch spans whose content is the link's display text (e.g. "[Click here"). A precise implementation would require renderer-level span tagging (deferred post-MVP).
///
/// SAFETY:
/// This function is only called when `app.hyperlinks_supported` is true. Do NOT call on terminals that don't support OSC-8 - the raw bytes will appear as garbage characters.
fn inject_osc8(text: Text<'static>, link_index: &LinkIndex) -> Text<'static> {
    // Build a fast lookup of known external links.
    let urls: HashSet<String> = link_index
        .0
        .iter()
        .filter_map(|l| {
            if let LinkTarget::External(url) = &l.target {
                Some(url.clone())
            } else {
                None
            }
        })
        .collect();

    if urls.is_empty() {
        return text;
    }

    let lines = text
        .lines
        .into_iter()
        .map(|line| {
            let spans = line
                .spans
                .into_iter()
                .map(|span| {
                    let content = span.content.trim().to_owned();
                    if urls.contains(&content) {
                        // wrap in OSC-8 open + close sequences.
                        let wrapped = format!(
                            "\x1b]8;;{url}\x1b\\{text}\x1b]8;;\x1b\\",
                            url = content,
                            text = span.content
                        );
                        Span::styled(wrapped, span.style)
                    } else {
                        span
                    }
                })
                .collect::<Vec<_>>();
            Line::from(spans)
        })
        .collect::<Vec<_>>();

    Text::from(lines)
}

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image_proto::DetectedImageProtocol;

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

    // html_string_to_text

    #[test]
    fn html_string_to_text_splits_on_newlines() {
        let html = "<p>line 1</p>\n<p>line 2</p>";
        let text = html_string_to_text(html);

        assert_eq!(text.lines.len(), 2);
        assert_eq!(text.lines[0].spans[0].content, "<p>line 1</p>");
        assert_eq!(text.lines[1].spans[0].content, "<p>line 2</p>");
    }

    #[test]
    fn html_string_to_text_empty_input() {
        let text = html_string_to_text("");

        // html_to_lines on empty string should produce 0 lines
        assert!(
            text.lines.is_empty()
                || text
                    .lines
                    .iter()
                    .all(|l| l.spans.is_empty() || l.spans[0].content.is_empty())
        );
    }

    // split_body logic - tested via the Rect math directly

    #[test]
    fn narrow_terminal_suppresses_split() {
        use crate::app::App;
        use alloy_core::{config::Config, document::Document};

        let app = App::new(
            Config::default(),
            Document::new(),
            DetectedImageProtocol::Kitty,
        );

        // Simulate a very narrow body rect
        let narrow = Rect::new(0, 0, MIN_SPLIT_WIDTH - 1, 20);
        let (_, preview) = split_body(&app, narrow);

        assert!(
            preview.is_none(),
            "preview should be suppressed when width < MIN_SPLIT_WIDTH"
        );
    }

    #[test]
    fn wide_terminal_produces_split() {
        use crate::app::App;
        use alloy_core::{config::Config, document::Document};

        let app = App::new(
            Config::default(),
            Document::new(),
            DetectedImageProtocol::Kitty,
        );

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

        let mut app = App::new(
            Config::default(),
            Document::new(),
            DetectedImageProtocol::Kitty,
        );
        app.preview_mode = PreviewMode::Hidden;

        let wide = Rect::new(0, 0, 200, 50);
        let (_, preview) = split_body(&app, wide);

        assert!(
            preview.is_none(),
            "preview should be suppressed when PreviewMode::Hidden"
        );
    }
}
