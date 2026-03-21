//! `PulldownEngine` - Markdown -> `ratatui::text::Text` via `pulldown-cmark`
//!
//! Event model:
//!
//! `pulldown-cmark` emits a flat stream of `Event` variants. Block-level constructs are bracketed by `Start(Tag)` / `End(TagEnd)` pairs; inline constructs nest inside them. We walk this stream with an explicit renderer state machine rather than recursion.
//!
//! Line-building model:
//!
//! We maintain a `LineBuilder` that accumulates `wrap_spans` before flushing so that ratatui never needs to split a styled span mid-token.
//!
//! Nesting:
//!
//! Block-quote depth and list indent depth are tracked on small stacks. Malformed nesting (e.g. an `End` without a matching `Start`) is logged as a `tracing::warn!` rather than causing a panic.

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use tracing::warn;

use crate::engine::MarkdownEngine;

// -----------------------------------------------------------
// Configuration
// -----------------------------------------------------------

/// Extension flags forwarded from `ExtensionConfig`.
///
/// Kept as a plain struct (no serde) so the `markdown` crate does not depend on `alloy-core`.
#[derive(Debug, Clone, Default)]
pub struct EngineExtensions {
    pub gfm: bool,
    pub footnotes: bool,
}

impl EngineExtensions {
    /// Return a `pulldown_cmark::Options` bitmask matching these flags.
    pub fn to_pulldown_options(&self) -> Options {
        let mut opts = Options::empty();

        if self.gfm {
            opts |= Options::ENABLE_TABLES;
            opts |= Options::ENABLE_TASKLISTS;
            opts |= Options::ENABLE_STRIKETHROUGH;
            // NOTE: ENABLE_SMART_PUNCTUATION is intentionally ommited - it can mangle code snippets that contain `--` or `...`.
        }
        if self.footnotes {
            opts |= Options::ENABLE_FOOTNOTES;
        }

        opts
    }
}

// -----------------------------------------------------------
// PulldownEngine
// -----------------------------------------------------------

/// The default Markdown rendering backend used for the terminal preview pane.
///
/// Uses `pulldown-cmark` for parsing and converts events to `ratatui::text::Text<'static>` directly (no ANSI intermediate).
///
/// THREAD SAFETY: `PulldownEngine` is `Send + Sync` because it holds only a plain `EngineExtensions` value (no internal mutability).
#[derive(Debug, Clone, Default)]
pub struct PulldownEngine {
    extensions: EngineExtensions,
}

impl PulldownEngine {
    pub fn new(extensions: EngineExtensions) -> Self {
        Self { extensions }
    }

    /// Convenience constructor for tests: GFM on, footnotes off.
    pub fn with_gfm() -> Self {
        Self::new(EngineExtensions {
            gfm: true,
            footnotes: false,
        })
    }
}

impl MarkdownEngine for PulldownEngine {
    fn render_terminal(&self, src: &str, col_width: u16) -> Text<'static> {
        let opts = self.extensions.to_pulldown_options();
        let parser = Parser::new_ext(src, opts);

        render_events(parser, col_width)
    }

    /// HTML output is deferred to Phase 4 (comrak integration).
    ///
    /// Returns a plain-text fallback so callers always get a `String`.
    fn render_html(&self, src: &str) -> String {
        // Phase 4 will replace this with comrak::markdown_to_html.
        // For now return the source wrapped in a <pre> so the HTML preview pane at least shows something useful.
        format!("<pre>\n{src}\n</pre>")
    }
}

// -----------------------------------------------------------
// Core rendering logic
// -----------------------------------------------------------

/// Converts a `pulldown-cmark` event iterator into `ratatui::text::Text`.
///
/// This function is deliberately standalone (not a method) so it can be unit-tested with arbitrary event iterators constructing a `PulldownEngine`.
pub(crate) fn render_events<'a>(
    events: impl Iterator<Item = Event<'a>>,
    col_width: u16,
) -> Text<'static> {
    let mut ctx = RenderContext::new(col_width);

    for event in events {
        ctx.handle_event(event);
    }

    // Flush any trailing content taht didn't end with an explicit block end.
    ctx.flush_line();

    Text::from(ctx.lines)
}

// -----------------------------------------------------------
// RenderContext - the renderer state machine
// -----------------------------------------------------------

/// Tracks all mutable state accumulated during a single render pass.
struct RenderContext {
    /// Finished lines ready to be put into `Text`.
    lines: Vec<Line<'static>>,

    /// Spans accumulating for the current logical line.
    current_spans: Vec<Span<'static>>,

    /// Style stack for nested inline constructs (bold inside italic, etc.).
    /// The top of the stack is the 'current' inline style.
    style_stack: Vec<Style>,

    /// How many levels of `BlockQuote` we are currently inside.
    blockquote_depth: u32,

    /// Stack of list types. Each entry is `true` for ordered and `false` for unordered. The length is the current nesting depth.
    list_stack: Vec<ListKind>,

    /// Prefix string to prepend to the first span of each new block.
    /// Set by heading/list item openers. Cleared after the first flush.
    block_prefix: Option<String>,

    /// `true` while we are inside a fenced code block.
    in_code_block: bool,

    /// Info string of the current fenced code block (e.g. `"rust"`).
    code_block_lang: Option<String>,

    /// Column width used for line-wrapping.
    col_width: u16,

    /// Accumulated item counter for the current ordered list level.
    /// Reset when entering a new list. Incremented on each `Item` open.
    ordered_item_num: Vec<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered,
}

impl RenderContext {
    fn new(col_width: u16) -> Self {
        Self {
            lines: Vec::new(),
            current_spans: Vec::new(),
            style_stack: vec![Style::default()],
            blockquote_depth: 0,
            list_stack: Vec::new(),
            block_prefix: None,
            in_code_block: false,
            code_block_lang: None,
            col_width,
            ordered_item_num: Vec::new(),
        }
    }

    // Style helpers

    fn current_style(&self) -> Style {
        *self.style_stack.last().unwrap_or(&Style::default())
    }

    fn push_style(&mut self, modifier: impl FnOnce(Style) -> Style) {
        let new = modifier(self.current_style());

        self.style_stack.push(new);
    }

    fn pop_style(&mut self) {
        if self.style_stack.len() > 1 {
            self.style_stack.pop();
        } else {
            warn!("style stack underflow - malformed Markdown nesting?");
        }
    }

    // Line building

    /// Push `text` as a span with the current style.
    fn push_text(&mut self, text: impl Into<String>) {
        let s = text.into();
        if s.is_empty() {
            return;
        }
        let style = self.current_style();

        self.current_spans.push(Span::styled(s, style));
    }

    /// Flush `current_spans` into a finished Line, then reset.
    ///
    /// If `blockquote_depth` > 0, prefix lines with a `▌ ` gutter.
    /// `block_prefix` (heading marker, list bullet) is prepended once per block.
    fn flush_line(&mut self) {
        // Prepend block prefix if present.
        let prefix = self.block_prefix.take();
        let prefix_style = if self.blockquote_depth > 0 {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default()
        };

        let mut spans: Vec<Span<'static>> = Vec::new();

        // Blockquote gutter: one `▌ ` per depth level.
        for _ in 0..self.blockquote_depth {
            spans.push(Span::styled(
                "▌ ",
                Style::default().fg(Color::Cyan).add_modifier(Modifier::DIM),
            ));
        }

        if let Some(p) = prefix {
            spans.push(Span::styled(p, prefix_style));
        }

        spans.extend(self.current_spans.drain(..));

        // Skip pushing truly empty lines in blockquotes (they already render as gutters).
        if spans.iter().all(|s| s.content.trim().is_empty()) && self.blockquote_depth == 0 {
            // Push a genuinely blank line to preserve paragraph spacing.
            self.lines.push(Line::from(vec![]));
        } else {
            // Hard-wrap if the assembled line exceeds col_width.
            let wrapped = wrap_line(spans, self.col_width);
            self.lines.extend(wrapped);
        }
    }

    /// Push a blank separator line (used between blocks).
    fn blank_line(&mut self) {
        self.lines.push(Line::from(vec![]));
    }

    /// Push a horizontal rule spanning col_width.
    fn push_rule(&mut self) {
        let width = self.col_width.max(1) as usize;
        let rule = "─".repeat(width);

        self.lines.push(Line::from(Span::styled(
            rule,
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Code block helpers

    fn begin_code_block(&mut self, lang: Option<String>) {
        self.in_code_block = true;
        self.code_block_lang = lang.clone();

        // Language tag line.
        let label = lang.as_deref().unwrap_or("text");
        let width = self.col_width.max(1) as usize;
        let header = format!(
            "┌─ {label} {}",
            "─".repeat(width.saturating_sub(label.len() + 4))
        );
        self.lines.push(Line::from(Span::styled(
            header,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));
    }

    fn end_code_block(&mut self) {
        // Flush any remaining code lines.
        if !self.current_spans.is_empty() {
            let spans: Vec<Span<'static>> = self.current_spans.drain(..).collect();
            self.lines.push(Line::from(spans));
        }

        // Footer rule.
        let width = self.col_width.max(1) as usize;
        let footer = "└".to_string() + &"─".repeat(width.saturating_sub(1));
        self.lines.push(Line::from(Span::styled(
            footer,
            Style::default()
                .fg(Color::DarkGray)
                .add_modifier(Modifier::DIM),
        )));

        self.in_code_block = false;
        self.code_block_lang = None;
        self.blank_line();
    }

    // Event dispatch

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            // Text content
            Event::Text(text) => {
                if self.in_code_block {
                    // Each line of a code block arrives as a separate Text event terminated by '\n'.
                    // Split and emit individual lines.
                    let code_style = Style::default().fg(Color::White).bg(Color::Reset);
                    let content = text.as_ref();
                    let lines: Vec<&str> = content.split('\n').collect();
                    for (i, line) in lines.iter().enumerate() {
                        self.current_spans
                            .push(Span::styled(format!("	{line}"), code_style));

                        // Flush between lines but not after the last fragment (the trailing '\n' produces an empty final element).
                        if i < lines.len().saturating_sub(1) {
                            let spans: Vec<Span<'static>> = self.current_spans.drain(..).collect();
                            if !spans.iter().all(|s| s.content == "  ") {
                                self.lines.push(Line::from(spans));
                            }
                        }
                    }
                } else {
                    self.push_text(text.into_string());
                }
            }

            Event::Code(text) => {
                // Inline code
                let style = Style::default()
                    .fg(Color::Yellow)
                    .bg(Color::Rgb(40, 40, 40));
                self.current_spans
                    .push(Span::styled(format!("`{}`", text.as_ref()), style));
            }

            Event::SoftBreak => {
                // In terminal rendering, a soft break becomes a space.
                self.push_text(" ");
            }

            Event::HardBreak => {
                self.flush_line();
            }

            Event::Rule => {
                self.flush_line();
                self.push_rule();
                self.blank_line();
            }

            // Block-level opens
            Event::Start(tag) => self.handle_start(tag),

            // Block-level closes
            Event::End(tag) => self.handle_end(tag),

            // Task list checkbox
            Event::TaskListMarker(checked) => {
                let mark = if checked { "☑ " } else { "☐ " };
                let style = if checked {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                self.current_spans.push(Span::styled(mark, style));
            }

            // Ignore HTML, footnote references, etc. for now.
            _ => {}
        }
    }

    fn handle_start(&mut self, tag: Tag<'_>) {
        match tag {
            // Heading
            Tag::Heading { level, .. } => {
                self.flush_line();
                let (prefix, style) = heading_style(level);
                self.block_prefix = Some(prefix);
                self.push_style(|_| style);
            }

            // Paragraph
            Tag::Paragraph => {
                // No visual prefix. Just ensure we're starting fresh.
                // A blank separator is added on End(Paragraph).
            }

            // BlockQuote
            Tag::BlockQuote(_kind) => {
                self.flush_line();
                self.blockquote_depth += 1;
                self.push_style(|s| {
                    s.fg(Color::Rgb(160, 160, 160))
                        .add_modifier(Modifier::ITALIC)
                });
            }

            // Code blocks
            Tag::CodeBlock(CodeBlockKind::Fenced(info)) => {
                self.flush_line();
                let lang = {
                    let s = info.as_ref().trim();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_owned())
                    }
                };
                self.begin_code_block(lang);
            }

            Tag::CodeBlock(CodeBlockKind::Indented) => {
                self.flush_line();
                self.begin_code_block(None);
            }

            // Lists
            Tag::List(start_num) => {
                // Flush whatever was open before the list.
                self.flush_line();
                match start_num {
                    None => {
                        self.list_stack.push(ListKind::Unordered);
                        self.ordered_item_num.push(0); // NOTE: Placeholder
                    }
                    Some(n) => {
                        self.list_stack.push(ListKind::Ordered);
                        self.ordered_item_num.push(n);
                    }
                }
            }

            Tag::Item => {
                // Flush the previous item's last line.
                self.flush_line();

                let depth = self.list_stack.len();
                let indent = " ".repeat(depth.saturating_sub(1));

                let kind = self
                    .list_stack
                    .last()
                    .copied()
                    .unwrap_or(ListKind::Unordered);
                let prefix = match kind {
                    ListKind::Unordered => {
                        let bullet = match depth {
                            1 => "•",
                            2 => "◦",
                            _ => "▸",
                        };
                        format!("{indent}{bullet} ")
                    }
                    ListKind::Ordered => {
                        let num = self.ordered_item_num.last_mut().unwrap();
                        let n = *num;
                        *num += 1;
                        format!("{indent}{n}. ")
                    }
                };

                self.block_prefix = Some(prefix);
                self.push_style(|s| s); // Inherit current style
            }

            // Table (GFM) - minimal: emit a dim header marker
            Tag::Table(_) => {
                self.flush_line();
                self.push_style(|s| s.fg(Color::Rgb(200, 200, 200)));
            }

            Tag::TableHead => {
                self.push_style(|s| s.add_modifier(Modifier::BOLD).fg(Color::Cyan));
            }

            Tag::TableRow | Tag::TableCell => {
                // Cell separator handled on End.
            }

            // Inline formatting
            Tag::Strong => {
                self.push_style(|s| s.add_modifier(Modifier::BOLD));
            }

            Tag::Emphasis => {
                self.push_style(|s| s.add_modifier(Modifier::ITALIC));
            }

            Tag::Strikethrough => {
                self.push_style(|s| s.add_modifier(Modifier::CROSSED_OUT).fg(Color::DarkGray));
            }

            // Links - display the link text as a dim note inline.
            // NOTE: Full link handling comes in Phase 6.
            Tag::Link { dest_url, .. } => {
                self.push_style(|s| s.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED));
                // Stash the URL so we can emit it after the link text on End.
                // We encode it into a synthetic span at End time. For now we just track it via a style annotation.
                let _ = dest_url; // NOTE: Phase 6 will use this
            }

            Tag::Image { .. } => {
                // NOTE: Phase 7 - inline image placeholder.
                self.push_style(|s| s.fg(Color::Magenta));
            }

            // Ignore meta-tags not relevant for terminal rendering.
            _ => {}
        }
    }

    fn handle_end(&mut self, tag: TagEnd) {
        match tag {
            // Headings
            TagEnd::Heading(_) => {
                self.flush_line();
                self.pop_style();
                self.blank_line();
            }

            // Paragraph
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank_line();
            }

            // BlockQuote
            TagEnd::BlockQuote(_) => {
                self.flush_line();
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                self.pop_style();

                if self.blockquote_depth == 0 {
                    self.blank_line();
                }
            }

            // Code blocks
            TagEnd::CodeBlock => {
                self.end_code_block();
            }

            // Lists
            TagEnd::List(_) => {
                self.flush_line();
                self.list_stack.pop();
                self.ordered_item_num.pop();

                if self.list_stack.is_empty() {
                    self.blank_line();
                }
            }

            TagEnd::Item => {
                // The item content is flushed when the next Item starts (or when the List ends). Pop the inherited style we pushed.
                self.pop_style();
            }

            // Table (GFM)
            TagEnd::Table => {
                self.flush_line();
                self.pop_style();
                self.blank_line();
            }

            TagEnd::TableHead => {
                self.flush_line();
                self.pop_style();
                // Underline separator row.
                let width = self.col_width.max(1) as usize;
                self.lines.push(Line::from(Span::styled(
                    "─".repeat(width),
                    Style::default().fg(Color::DarkGray),
                )));
            }

            TagEnd::TableRow => {
                self.flush_line();
            }

            TagEnd::TableCell => {
                // Separate cells with a dim pipe.
                self.current_spans
                    .push(Span::styled("  |  ", Style::default().fg(Color::DarkGray)));
            }

            // Inline
            TagEnd::Strong | TagEnd::Emphasis | TagEnd::Strikethrough => {
                self.pop_style();
            }

            TagEnd::Link => {
                self.pop_style();
            }

            TagEnd::Image => {
                self.pop_style();
            }

            _ => {}
        }
    }
}

// -----------------------------------------------------------
// Heading style helper
// -----------------------------------------------------------

fn heading_style(level: HeadingLevel) -> (String, Style) {
    let (prefix, fg) = match level {
        HeadingLevel::H1 => ("# ".to_owned(), Color::LightCyan),
        HeadingLevel::H2 => ("## ".to_owned(), Color::Cyan),
        HeadingLevel::H3 => ("### ".to_owned(), Color::Blue),
        HeadingLevel::H4 => ("#### ".to_owned(), Color::LightBlue),
        HeadingLevel::H5 => ("##### ".to_owned(), Color::DarkGray),
        HeadingLevel::H6 => ("###### ".to_owned(), Color::DarkGray),
    };
    let style = Style::default().fg(fg).add_modifier(Modifier::BOLD);

    (prefix, style)
}

// -----------------------------------------------------------
// Line wrapping
// -----------------------------------------------------------

/// Hard-wrap a sequence of styled spans into multiple `Line`s, each no wider than `col_width` terminal columns.
///
/// Wrapping occurs at the last space boundary before `col_width`. If a single word exceeds `col_width`, it's split at the column boundary (hard wrap).
///
/// Each wrapped continuation line inherits no block prefix (that is handled by the outer flush logic for the first line only).
fn wrap_line(spans: Vec<Span<'static>>, col_width: u16) -> Vec<Line<'static>> {
    if col_width == 0 {
        return vec![Line::from(spans)];
    }

    let max = col_width as usize;

    // Fast path - measure total visible lengths. if it fits, return as-is.
    let total: usize = spans
        .iter()
        .map(|s| visible_width(s.content.as_ref()))
        .sum();
    if total <= max {
        return vec![Line::from(spans)];
    }

    // Slow path - rebuild character by character, respecting span boundaries.
    let mut output_lines: Vec<Line<'static>> = Vec::new();
    let mut current_line: Vec<Span<'static>> = Vec::new();
    let mut current_width = 0usize;

    for span in spans {
        let style = span.style;
        let content = span.content.into_owned();

        // Split the span's content at word boundaries.
        let mut word_start = 0;
        let chars: Vec<char> = content.chars().collect();
        let mut i = 0;

        while i <= chars.len() {
            let is_boundary = i == chars.len()
                || (chars[i] == ' ' && i > word_start)
                || current_width + (i - word_start) > max;

            if is_boundary && i > word_start {
                let word: String = chars[word_start..i].iter().collect();
                let word_w = visible_width(&word);

                if current_width + word_w > max && !current_line.is_empty() {
                    // Flush the current line and start a new one.
                    output_lines.push(Line::from(current_line.drain(..).collect::<Vec<_>>()));
                    current_width = 0;
                }

                current_line.push(Span::styled(word, style));
                current_width += word_w;

                // Consume the trailing space (if any) as width.
                if i < chars.len() && chars[i] == ' ' {
                    current_line.push(Span::styled(" ", style));
                    current_width += 1;
                    i += 1;
                }

                word_start = i;
            } else {
                i += 1;
            }
        }
    }

    if !current_line.is_empty() {
        output_lines.push(Line::from(current_line));
    }

    if output_lines.is_empty() {
        output_lines.push(Line::from(vec![]));
    }

    output_lines
}

/// Count the visible width of a string in terminal columns.
///
/// For now this is simply the number of Unicode scalar values (chars). A full implementation would use `unicode-width` for wide characters.
/// NOTE: To be implemented in the future.
fn visible_width(s: &str) -> usize {
    s.chars().count()
}

// -----------------------------------------------------------
// Tests
// -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn render(src: &str) -> String {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal(src, 80);

        text_to_plain(&text)
    }

    /// Flatten `Text` to a plain string (content only, no styles) for snapshot-friendly assertions.
    pub(crate) fn text_to_plain(text: &Text<'_>) -> String {
        text.lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    // Structural smoke tests

    #[test]
    fn empty_input_does_not_panic() {
        let engine = PulldownEngine::default();
        let text = engine.render_terminal("", 80);

        assert!(text.lines.is_empty() || text.lines.iter().all(|l| l.spans.is_empty()));
    }

    #[test]
    fn heading_h1_contains_prefix_and_text() {
        let out = render("# Hello World\n");

        assert!(out.contains("# "), "H1 prefix missing: {out:?}");
        assert!(out.contains("Hello World"), "H1 text missing: {out:?}");
    }

    #[test]
    fn heading_h2_uses_double_hash() {
        let out = render("## Section\n");

        assert!(out.contains("## "), "H2 prefix missing: {out:?}");
    }

    #[test]
    fn heading_h3_uses_triple_hash() {
        let out = render("### Sub\n");

        assert!(out.contains("### "), "H3 prefix missing: {out:?}");
    }

    #[test]
    fn paragraph_text_appears_in_output() {
        let out = render("Hello, world.\n");

        assert!(
            out.contains("Hello, world."),
            "paragraph text missing: {out:?}"
        );
    }

    #[test]
    fn bold_text_appears_in_output() {
        let out = render("This is **bold** text.\n");

        assert!(out.contains("bold"), "bold text missing: {out:?}");
    }

    #[test]
    fn italic_text_appears_in_output() {
        let out = render("This is _italic_ text.\n");

        assert!(out.contains("italic"), "italic text missing: {out:?}");
    }

    #[test]
    fn inline_code_includes_backticks() {
        let out = render("Use `cargo build`.\n");

        assert!(
            out.contains("`cargo build`"),
            "inline code missing: {out:?}"
        );
    }

    #[test]
    fn unordered_list_bullet_prefix() {
        let out = render("- alpha\n- beta\n");

        assert!(out.contains("alpha"), "list item alpha missing: {out:?}");
        assert!(out.contains("beta"), "list item beta missing: {out:?}");
        assert!(
            out.contains('•') || out.contains('-') || out.contains('◦'),
            "list bullet missing: {out:?}"
        );
    }

    #[test]
    fn ordered_list_number_prefix() {
        let out = render("1. First\n2. Second\n");

        assert!(
            out.contains("First"),
            "ordered item 'First' missing: {out:?}"
        );
        assert!(
            out.contains("Second"),
            "ordered item 'Second' missing: {out:?}"
        );
    }

    #[test]
    fn blockquote_gutter_present() {
        let out = render("> A quoted line.\n");

        assert!(out.contains("▌"), "blockquote gutter missing: {out:?}");
        assert!(
            out.contains("A quoted line."),
            "blockquote text missing: {out:?}"
        );
    }

    #[test]
    fn fenced_code_block_shows_language_and_body() {
        let out = render("```rust\nfn main() {}\n```\n");

        assert!(out.contains("rust"), "language label missing: {out:?}");
        assert!(out.contains("fn main()"), "code body missing: {out:?}");
    }

    #[test]
    fn horizontal_rule_emits_line_of_dashes() {
        let out = render("---\n");

        assert!(out.contains('─'), "horizontal rule missing: {out:?}");
    }

    #[test]
    fn gfm_strikethrough_appears() {
        let out = render("~~gone~~\n");

        assert!(out.contains("gone"), "strikethrough text missing: {out:?}");
    }

    // Style checks (spawn-level)

    #[test]
    fn h1_span_is_bold_and_colored() {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal("# Title\n", 80);

        // Find the line containing "Title" and check its style.
        let title_line = text
            .lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("Title")));

        assert!(title_line.is_some(), "no line with 'Title' found");

        let title_span = title_line
            .unwrap()
            .spans
            .iter()
            .find(|s| s.content.contains("Title"))
            .unwrap();

        assert!(
            title_span.style.add_modifier.contains(Modifier::BOLD),
            "H1 title span should be bold: {:?}",
            title_span.style
        );
    }

    #[test]
    fn bold_span_has_bold_modifier() {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal("**important**\n", 80);

        let bold_span = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("important"));

        assert!(bold_span.is_some(), "bold span not found");
        assert!(
            bold_span
                .unwrap()
                .style
                .add_modifier
                .contains(Modifier::BOLD),
            "bold span missing BOLD modifier"
        );
    }

    #[test]
    fn italic_span_has_italic_modifier() {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal("*slanted*\n", 80);

        let span = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("slanted"));

        assert!(span.is_some(), "italic span not found");
        assert!(
            span.unwrap().style.add_modifier.contains(Modifier::ITALIC),
            "italic span missing ITALIC modifier"
        );
    }

    #[test]
    fn inline_code_span_has_yellow_fg() {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal("Use `foo`.\n", 80);

        let code_span = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("`foo`"));

        assert!(code_span.is_some(), "`foo` span not found");
        assert_eq!(
            code_span.unwrap().style.fg,
            Some(Color::Yellow),
            "inline code should have Yellow fg"
        );
    }

    // Wrap helpers unit tests

    #[test]
    fn wrap_short_line_unchanged() {
        let spans = vec![Span::raw("hello world")];
        let lines = wrap_line(spans, 80);

        assert_eq!(lines.len(), 1);
    }

    #[test]
    fn wrap_long_line_splits() {
        let long = "word ".repeat(30); // 150 chars
        let spans = vec![Span::raw(long)];
        let lines = wrap_line(spans, 40);

        assert!(
            lines.len() > 1,
            "long line should be split into multiple lines"
        );
    }

    #[test]
    fn wrap_zero_col_width_does_not_panic() {
        let spans = vec![Span::raw("hello")];
        let lines = wrap_line(spans, 0);

        assert!(!lines.is_empty());
    }

    // Insta snapshot tests

    #[test]
    fn snapshot_basic_md() {
        let src = include_str!("../../tests/fixtures/basic.md");
        let out = render(src);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_lists_md() {
        let src = include_str!("../../tests/fixtures/lists.md");
        let out = render(src);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_code_md() {
        let src = include_str!("../../tests/fixtures/code.md");
        let out = render(src);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_blockquote_md() {
        let src = include_str!("../../tests/fixtures/blockquote.md");
        let out = render(src);
        insta::assert_snapshot!(out);
    }
}
