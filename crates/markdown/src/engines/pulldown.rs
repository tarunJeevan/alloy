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
//!
//! Link extraction:
//!
//! Links are accumulated in `RenderContext::link_index`. The index is populated during event dispatch:
//! - `Start(Tag::Link { dest_url, .. })` stashes the URL and records the current source position.
//! - Subsequent Text events inside the link accumulate `pending_link_text`.
//! - `End(TagEnd::Link)` finalizes and pushes the `Link` entry.
//! - `Start(Tag::Heading)` + `End(TagEnd::Heading)` record an `InternalAnchor` target.
//! - `[[wiki]]` / `[[wiki|title]]` patterns in raw Text events are parsed and pushed as `WikiLink` entries.
//!
//! Image handling:
//!
//! Images use `ratatui-image` widgets for protocol-based rendering with a placeholder path as a fallback for:
//! - when images are disabled
//! - the protocol is unsupported
//! - the image fails to load
//!
//! Syntax highlighting:
//!
//! - When a `Highlighter` is provided, fenced code blocks whose language tag matches a known syntax are highlighted using syntect.
//! - Unknown tags fall back to the configured `FallbackStyle`.
//! - Highlighting is applied inside `end_code_block` using the accumulated `code_block_content` buffer.
//! - The `RenderContext` receives a reference to the `Highlighter` and the `HighlightingConfig` for the duration of each render call.

use std::sync::Arc;

use pulldown_cmark::{CodeBlockKind, Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
};
use tracing::warn;

use alloy_core::{
    config::{FallbackStyle, HighlightingConfig},
    links::{Link, LinkIndex, LinkTarget, normalize_anchor},
};

use crate::{engine::MarkdownEngine, highlight::Highlighter};

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
    pub wiki_links: bool,
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
#[derive(Clone, Default)]
pub struct PulldownEngine {
    extensions: EngineExtensions,

    /// Shared syntax highlighter.
    ///
    /// NOTE: Always `Some` in normal operation. `None` only in unit tests that use `default()` or `with_gfm()`.
    highlighter: Option<Arc<Highlighter>>,

    /// Highlighting configuration (theme name, enabled flag, fallback style).
    highlighting_config: HighlightingConfig,
}

impl std::fmt::Debug for PulldownEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PulldownEngine")
            .field("extensions", &self.extensions)
            .field("highlighting_config", &self.highlighting_config)
            .field("theme", &self.highlighting_config.theme)
            .finish()
    }
}

impl PulldownEngine {
    /// Construct with explicit extension flags and no highlighter.
    ///
    /// Code blocks will use plain monospace styling regardless of `highlighting_config.enabled`.
    pub fn new(extensions: EngineExtensions) -> Self {
        Self {
            extensions,
            highlighter: None,
            highlighting_config: HighlightingConfig {
                enabled: false,
                ..Default::default()
            },
        }
    }

    /// Construct with extension flags AND a shared `Highlighter`.
    pub fn new_with_highlighting(
        extensions: EngineExtensions,
        highlighter: Arc<Highlighter>,
        highlighting_config: HighlightingConfig,
    ) -> Self {
        Self {
            extensions,
            highlighter: Some(highlighter),
            highlighting_config,
        }
    }

    /// Convenience constructor for tests: GFM on, footnotes off, no highlighting.
    pub fn with_gfm() -> Self {
        Self::new(EngineExtensions {
            gfm: true,
            footnotes: false,
            wiki_links: false,
        })
    }

    /// Convenience constructor for highlighting tests.
    ///
    /// Loads bundled syntect defaults and uses the given theme name.
    pub fn with_highlighting(theme: &str) -> Self {
        let highlighter = Arc::new(Highlighter::load_defaults());
        let config = HighlightingConfig {
            enabled: true,
            theme: theme.to_owned(),
            fallback_style: FallbackStyle::Dimmed,
        };

        Self::new_with_highlighting(
            EngineExtensions {
                gfm: true,
                ..Default::default()
            },
            highlighter,
            config,
        )
    }
}

impl MarkdownEngine for PulldownEngine {
    fn render_terminal(&self, src: &str, col_width: u16) -> Text<'static> {
        let (text, _) = self.render_terminal_with_links(src, col_width);

        text
    }

    fn render_terminal_with_links(&self, src: &str, col_width: u16) -> (Text<'static>, LinkIndex) {
        let opts = self.extensions.to_pulldown_options();
        let parser = Parser::new_ext(src, opts);

        render_events(
            parser,
            col_width,
            self.extensions.wiki_links,
            self.highlighter.as_deref(),
            &self.highlighting_config,
        )
    }

    /// HTML output is deferred to Phase 4 (comrak integration).
    ///
    /// Returns a plain-text fallback so callers always get a `String`.
    fn render_html(&self, src: &str) -> String {
        // NOTE: Replace this with comrak::markdown_to_html.
        // For now return the source wrapped in a <pre> so the HTML preview pane at least shows something useful.
        format!("<pre>\n{src}\n</pre>")
    }
}

// -----------------------------------------------------------
// Core rendering logic
// -----------------------------------------------------------

/// Converts a `pulldown-cmark` event iterator into `(ratatui::text::Text, LinkIndex)`.
///
/// This function is deliberately standalone (not a method) so it can be unit-tested with arbitrary event iterators constructing a `PulldownEngine`.
pub(crate) fn render_events<'a>(
    events: impl Iterator<Item = Event<'a>>,
    col_width: u16,
    wiki_links: bool,
    highlighter: Option<&Highlighter>,
    hl_config: &HighlightingConfig,
) -> (Text<'static>, LinkIndex) {
    let mut ctx = RenderContext::new(col_width, wiki_links, highlighter, hl_config);

    for event in events {
        ctx.handle_event(event);
    }

    // Flush any trailing content taht didn't end with an explicit block end.
    ctx.flush_line();

    (Text::from(ctx.lines), ctx.link_index)
}

// -----------------------------------------------------------
// RenderContext - the renderer state machine
// -----------------------------------------------------------

/// Tracks all mutable state accumulated during a single render pass.
struct RenderContext<'h> {
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

    /// Accumulated links discovered during this render pass.
    link_index: LinkIndex,

    /// The URL/href of the link currently being rendered (set on `Start(Tag::Link)`).
    pending_link_href: Option<String>,

    /// Text accumulated within the current link (reset on `Start(Tag::Link)`).
    pending_link_text: String,

    /// The source line at the time we opened the current link.
    pending_link_source_line: usize,

    /// The source column at the time we opened the current link.
    pending_link_source_col: usize,

    /// Whether we are currently inside a heading element.
    in_heading: bool,

    /// Text accumulated within the current heading (used to derive the anchor ID).
    pending_heading_text: String,

    /// `true` if the engine should scan raw `Text` events for `[[wiki]]` patterns.
    wiki_links_enabled: bool,

    /// Accumulates raw text content within the current paragraph for wiki link scanning.
    /// Only used when `wiki_links_enabled` is true. Cleared on paragraph end.
    paragraph_accumulator: String,

    /// `true` while inside an image tag.
    ///
    /// Used to suppress normal Text event processing inside the image so that alt text is captured into `pending_image_alt` rather than emitted as visible spans.
    in_image: bool,

    /// The URL/path of the image currently being rendered.
    /// Set on `Start(Tag::Image)` and cleared on `End(TagEnd::Image)`.
    pending_image_url: String,

    /// Alt text accumulated from `Text` events inside the image tag.
    /// pulldown-cmark emits the alt text as one or more Text events between `Start(Tag::Image)` and `End(TagEng::Image)`.
    /// Cleared on `End(TagEnd::Image)`.
    pending_image_alt: String,

    /// Accumulated raw text lines inside a fenced code block.
    /// Collected during `Event::Text` and consumed in `end_code_block`.
    code_block_content: String,

    /// Reference to the shared syntax highlighter (if any).
    highlighter: Option<&'h Highlighter>,

    /// Highlighting config for this render pass.
    hl_config: &'h HighlightingConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ListKind {
    Unordered,
    Ordered,
}

impl<'h> RenderContext<'h> {
    fn new(
        col_width: u16,
        wiki_links_enabled: bool,
        highlighter: Option<&'h Highlighter>,
        hl_config: &'h HighlightingConfig,
    ) -> Self {
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
            link_index: LinkIndex::new(),
            pending_link_href: None,
            pending_link_text: String::new(),
            pending_link_source_line: 0,
            pending_link_source_col: 0,
            in_heading: false,
            pending_heading_text: String::new(),
            wiki_links_enabled,
            paragraph_accumulator: String::new(),
            in_image: false,
            pending_image_url: String::new(),
            pending_image_alt: String::new(),
            code_block_content: String::new(),
            highlighter,
            hl_config,
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

        spans.append(&mut self.current_spans);

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
        self.code_block_content.clear(); // Reset text accumulator

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

        // Highlighting path
        let should_highlight = self.hl_config.enabled
            && self.highlighter.is_some()
            && !self.code_block_content.is_empty();

        if should_highlight {
            let hl = self.highlighter.unwrap();
            let lang = self.code_block_lang.as_deref();
            let highlighted_lines = hl.highlight_block(
                &self.code_block_content,
                lang,
                &self.hl_config.theme,
                &self.hl_config.fallback_style,
            );

            // Each highlighted line is endented with a tab to match the prior plain rendering style and distinguish code from prose.
            for hl_line in highlighted_lines {
                let mut indented_spans: Vec<Span<'static>> =
                    Vec::with_capacity(hl_line.spans.len() + 1);
                indented_spans.push(Span::raw("\t"));
                indented_spans.extend(hl_line.spans.into_iter());
                self.lines.push(Line::from(indented_spans));
            }
        } else {
            // Plain style fallback - render the accumulated content as monospace.
            let code_style = Style::default().fg(Color::White).bg(Color::Reset);
            for line_str in self.code_block_content.lines() {
                self.lines.push(Line::from(Span::styled(
                    format!("\t{line_str}"),
                    code_style,
                )));
            }
            // Emit one trailing blank line inside the block to match prior behavior.
            if !self.code_block_content.is_empty() {
                self.lines.push(Line::from(Span::styled("\t", code_style)));
            }
        }

        self.code_block_content.clear();

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

    /// Current source position (approximated by line count so far).
    fn current_source_line(&self) -> usize {
        // We use the number of flushed lines as a proxy for source position.
        // This is an approximation - rendered lines may differ from source lines due to wrapping, blank lines inserted between blocks, etc.
        // The value is good enough for cursor-jump purposes.
        self.lines.len()
    }

    // Link index helpers

    /// Called when we enter a `Tag::Link`. Stashes the URL and resets the pending text accumulator.
    fn begin_link(&mut self, href: &str) {
        self.pending_link_href = Some(href.to_owned());
        self.pending_link_text.clear();
        self.pending_link_source_line = self.current_source_line();
        self.pending_link_source_col = 0;
    }

    /// Called when we exit `TagEnd::Link`. Finalizes and pushes the link.
    fn end_link(&mut self) {
        if let Some(href) = self.pending_link_href.take() {
            let target = LinkTarget::from_href(&href);
            self.link_index.push(Link {
                display_text: self.pending_link_text.clone(),
                target,
                source_line: self.pending_link_source_line,
                source_col: self.pending_link_source_col,
            });
        }
        self.pending_link_text.clear();
    }

    /// Called when we exit a heading. Registers the heading text as an anchor target.
    fn end_heading(&mut self) {
        let anchor = normalize_anchor(&self.pending_heading_text);
        if !anchor.is_empty() {
            self.link_index.push(Link {
                display_text: self.pending_heading_text.clone(),
                target: LinkTarget::InternalAnchor(anchor),
                source_line: self.current_source_line().saturating_sub(1),
                source_col: 0,
            });
        }
        self.pending_heading_text.clear();
    }

    /// Scan a raw text fragment for `[[wiki]]` or `[[wiki|title]]` patterns and push `WikiLink` entries into the index.
    fn scan_wiki_links(&mut self, text: &str) {
        let mut rest = text;
        while let Some(open) = rest.find("[[") {
            rest = &rest[open + 2..];
            if let Some(close) = rest.find("]]") {
                let inner = &rest[..close];
                let (page, _title) = inner.split_once('|').unwrap_or((inner, inner));
                let page = page.trim().to_owned();
                if !page.is_empty() {
                    self.link_index.push(Link {
                        display_text: inner.to_owned(),
                        target: LinkTarget::WikiLink(page),
                        source_line: self.current_source_line(),
                        source_col: 0,
                    });
                }
                rest = &rest[close + 2..];
            } else {
                break;
            }
        }
    }

    // Image helpers

    /// Called on `Start(Tag::Image { dest_url, .. })`.
    ///
    /// Records the image URL and switches into image-accumulation mode.
    /// While `in_image` is true, Text events are captured into `pending_image_alt` rather than emitted as visible spans.
    fn begin_image(&mut self, url: &str) {
        self.in_image = true;
        self.pending_image_url = url.to_owned();
        self.pending_image_alt.clear();
    }

    /// Called on `End(TagEnd::Image)`.
    ///
    /// 1. Pushes a `LinkTarget::Image` entry into `link_index` so `render_preview_images` in `ui.rs` can find the image source line and URL without a second parse pass.
    /// 2. Emits a styled placeholder span that acts as a layout anchor: the actual `StatefulImage` widget is drawn on top of it by `render_preview_images`.
    fn end_image(&mut self) {
        let url = self.pending_image_url.clone();
        let alt = self.pending_image_alt.trim().to_owned();

        // Register in the link index so the UI renderer can locate images.
        self.link_index.push(Link {
            display_text: alt.clone(),
            target: LinkTarget::Image {
                url: url.clone(),
                alt: alt.clone(),
            },
            source_line: self.current_source_line(),
            source_col: 0,
        });

        // Emit a placeholder span that reserves vertical space.
        let label = if alt.is_empty() {
            format!("[Image: {url}]")
        } else {
            format!("[Image: {alt} ({url})]",)
        };

        self.current_spans.push(Span::styled(
            label,
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::DIM),
        ));

        // Reset image state
        self.in_image = false;
        self.pending_image_url.clear();
        self.pending_image_alt.clear();
    }

    // Event dispatch

    fn handle_event(&mut self, event: Event<'_>) {
        match event {
            // Text content
            Event::Text(text) => {
                let s = text.as_ref();

                if self.in_code_block {
                    // Each line of a code block arrives as a separate Text event terminated by '\n'.
                    // Accumulate into buffer and emit highlighted lines in `end_code_block`.
                    self.code_block_content.push_str(s);
                } else if self.in_image {
                    // Alt text inside an image tag - accumulate, don't emit.
                    self.pending_image_alt.push_str(s);
                } else {
                    // Accumulate text for heading anchor and link display text.
                    if self.in_heading {
                        self.pending_heading_text.push_str(s);
                    }
                    if self.pending_link_href.is_some() {
                        self.pending_link_text.push_str(s);
                    }
                    // Accumulate for wiki link scanning (scanned in bulk on End(Paragraph)).
                    if self.wiki_links_enabled {
                        self.paragraph_accumulator.push_str(s);
                    }
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
                // NOTE: Use ✅ and ⬜ if preferred
                let mark = if checked { "✓ " } else { "☐ " };
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
                self.in_heading = true;
                self.pending_heading_text.clear();
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
                            1 => "●",
                            2 => "○",
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
            Tag::Link { dest_url, .. } => {
                self.begin_link(dest_url.as_ref());
                self.push_style(|s| s.fg(Color::Cyan).add_modifier(Modifier::UNDERLINED));
            }

            Tag::Image { dest_url, .. } => {
                self.begin_image(dest_url.as_ref());
            }

            // Ignore meta-tags not relevant for terminal rendering.
            _ => {}
        }
    }

    fn handle_end(&mut self, tag: TagEnd) {
        match tag {
            // Headings
            TagEnd::Heading(_) => {
                self.in_heading = false;
                self.end_heading();
                self.flush_line();
                self.pop_style();
                self.blank_line();
            }

            // Paragraph
            TagEnd::Paragraph => {
                self.flush_line();
                self.blank_line();
                if self.wiki_links_enabled && !self.paragraph_accumulator.is_empty() {
                    let accumulated = std::mem::take(&mut self.paragraph_accumulator);
                    self.scan_wiki_links(&accumulated);
                }
                self.paragraph_accumulator.clear(); // no-op if take() was used but better to be safe
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
                self.end_link();
                self.pop_style();
            }

            TagEnd::Image => {
                self.end_image();
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
                    // output_lines.push(Line::from(current_line.drain(..).collect::<Vec<_>>()));
                    output_lines.push(Line::from(std::mem::take(&mut current_line)));
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

    fn render_with_links(src: &str) -> (String, LinkIndex) {
        let engine = PulldownEngine::with_gfm();
        let (text, links) = engine.render_terminal_with_links(src, 80);

        (text_to_plain(&text), links)
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
            out.contains('●') || out.contains('-') || out.contains('○'),
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

    // Image checks

    #[test]
    fn image_with_alt_text_renders_placeholder() {
        let out = render("![A cat sitting](cat.png)\n");

        assert!(
            out.contains("[Image:"),
            "image placeholder prefix missing: {out:?}"
        );
        assert!(
            out.contains("A cat sitting"),
            "image alt text missing from placeholder: {out:?}"
        );
        assert!(
            out.contains("cat.png"),
            "image URL missing from placeholder: {out:?}"
        );
    }

    #[test]
    fn image_without_alt_text_renders_url_only() {
        let out = render("![](diagram.svg)\n");

        assert!(
            out.contains("[Image: diagram.svg]"),
            "expected bare URL placeholder for no-alt image: {out:?}"
        );
    }

    #[test]
    fn image_with_remote_url_renders_placeholder() {
        let out = render("![Logo](https://example.com/logo.png)\n");

        assert!(
            out.contains("https://example.com/logo.png"),
            "remote URL missing from placeholder: {out:?}"
        );
    }

    #[test]
    fn image_alt_text_not_emitted_as_visible_text() {
        // The alt text should appear only inside the [Image: ...] placeholder, not
        // as a separate run of text.
        let out = render("![Secret Alt](img.png)\n");

        // Should contain exactly one occurrence of "Secret Alt" (inside the placeholder).
        let count = out.matches("Secret Alt").count();

        assert_eq!(count, 1, "alt text should appear exactly once: {out:?}");
    }

    #[test]
    fn image_span_has_magenta_style() {
        let engine = PulldownEngine::with_gfm();
        let text = engine.render_terminal("![Alt](img.png)\n", 80);

        let image_span = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("[Image:"));

        assert!(image_span.is_some(), "image placeholder span not found");
        assert_eq!(
            image_span.unwrap().style.fg,
            Some(Color::Magenta),
            "image placeholder should have Magenta fg"
        );
    }

    #[test]
    fn image_does_not_corrupt_surrounding_text() {
        let out = render("Before.\n\n![Alt](img.png)\n\nAfter.\n");

        assert!(
            out.contains("Before."),
            "text before image missing: {out:?}"
        );
        assert!(out.contains("After."), "text after image missing: {out:?}");
        assert!(
            out.contains("[Image:"),
            "image placeholder missing: {out:?}"
        );
    }

    #[test]
    fn multiple_images_all_rendered_as_placeholders() {
        let out = render("![One](one.png)\n\n![Two](two.png)\n");

        assert!(out.contains("one.png"), "first image missing: {out:?}");
        assert!(out.contains("two.png"), "second image missing: {out:?}");
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

    // Link extraction tests

    #[test]
    fn inline_link_extracted_correctly() {
        let (_, links) = render_with_links("[Example](https://example.com)\n");

        assert_eq!(links.len(), 1);

        let link = links.get(0).unwrap();

        assert_eq!(link.display_text, "Example");
        assert!(matches!(&link.target, LinkTarget::External(u) if u == "https://example.com"));
    }

    #[test]
    fn internal_anchor_link_extracted() {
        let (_, links) = render_with_links("[Go to section](#my-section)\n");

        // Should have both the link and the anchor (if heading exists) — we're only checking the link here
        let anchor_link = links
            .0
            .iter()
            .find(|l| matches!(&l.target, LinkTarget::InternalAnchor(a) if a == "my-section"));

        assert!(anchor_link.is_some(), "internal anchor link not found");
    }

    #[test]
    fn heading_registers_as_anchor_target() {
        let (_, links) = render_with_links("# My Heading\n\nSome text.\n");

        let anchor = links
            .0
            .iter()
            .find(|l| matches!(&l.target, LinkTarget::InternalAnchor(a) if a == "my-heading"));

        assert!(anchor.is_some(), "heading anchor not registered: {links:?}");
    }

    #[test]
    fn multiple_links_all_extracted() {
        let src = "[One](https://one.com) and [Two](https://two.com)\n";
        let (_, links) = render_with_links(src);

        let externals: Vec<_> = links
            .0
            .iter()
            .filter(|l| matches!(&l.target, LinkTarget::External(_)))
            .collect();

        assert_eq!(externals.len(), 2, "expected 2 external links: {links:?}");
    }

    #[test]
    fn no_links_in_plain_paragraph() {
        let (_, links) = render_with_links("Just plain text, no links here.\n");
        let external_count = links
            .0
            .iter()
            .filter(|l| matches!(&l.target, LinkTarget::External(_)))
            .count();

        assert_eq!(external_count, 0);
    }

    #[test]
    fn file_path_link_classified_correctly() {
        let (_, links) = render_with_links("[Notes](./notes.md)\n");

        let file_link = links
            .0
            .iter()
            .find(|l| matches!(&l.target, LinkTarget::FilePath(_)));

        assert!(file_link.is_some(), "file path link not found: {links:?}");
    }

    #[test]
    fn wiki_link_extracted_when_enabled() {
        let engine = PulldownEngine::new(EngineExtensions {
            gfm: true,
            footnotes: false,
            wiki_links: true,
        });
        let (_, links) = engine.render_terminal_with_links("See [[HomePage]] for details.\n", 80);

        let wiki = links
            .0
            .iter()
            .find(|l| matches!(&l.target, LinkTarget::WikiLink(p) if p == "HomePage"));

        assert!(wiki.is_some(), "wiki link not extracted: {links:?}");
    }

    #[test]
    fn wiki_link_with_title_extracted() {
        let engine = PulldownEngine::new(EngineExtensions {
            gfm: true,
            footnotes: false,
            wiki_links: true,
        });
        let (_, links) = engine.render_terminal_with_links("[[MyPage|Custom Title]]\n", 80);

        let wiki = links
            .0
            .iter()
            .find(|l| matches!(&l.target, LinkTarget::WikiLink(p) if p == "MyPage"));

        assert!(
            wiki.is_some(),
            "wiki link with title not extracted: {links:?}"
        );
    }

    #[test]
    fn wiki_links_not_extracted_when_disabled() {
        let engine = PulldownEngine::with_gfm(); // wiki_links = false
        let (_, links) = engine.render_terminal_with_links("See [[HomePage]] for details.\n", 80);

        let wiki_count = links
            .0
            .iter()
            .filter(|l| matches!(&l.target, LinkTarget::WikiLink(_)))
            .count();

        assert_eq!(
            wiki_count, 0,
            "wiki links should not be extracted when disabled"
        );
    }

    #[test]
    fn link_display_text_captured() {
        let (_, links) = render_with_links("[Click Here](https://example.com)\n");

        assert_eq!(links.get(0).unwrap().display_text, "Click Here");
    }

    // Highlighting integration tests

    #[test]
    fn highlighted_rust_block_contains_code_content() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal("```rust\nfn main() {}\n```\n", 80);
        let plain = text_to_plain(&text);

        assert!(
            plain.contains("fn main()"),
            "highlighted block should contain code: {plain:?}"
        );
    }

    #[test]
    fn highlighted_rust_block_has_colored_spans() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal(
            "```rust\nfn add(a: i32, b: i32) -> i32 { a + b }\n```\n",
            80,
        );

        // Find spans inside the code block (skip header/footer lines).
        // Header line contains "┌─ rust", footer contains "└".
        let code_lines: Vec<_> = text
            .lines
            .iter()
            .filter(|l| {
                let content: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
                !content.contains("┌─") && !content.contains("└") && !content.trim().is_empty()
            })
            .collect();

        assert!(
            !code_lines.is_empty(),
            "expected at least one code content line"
        );

        // At least some spans should have non-default (non-white) fg colors from syntect.
        let has_colored_span = code_lines.iter().any(|line| {
            line.spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))))
        });

        assert!(
            has_colored_span,
            "highlighted Rust block should have at least one Rgb-colored span"
        );
    }

    #[test]
    fn highlighted_python_block_contains_code_content() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let src = "```python\ndef greet(name):\n    return f\"Hello, {name}!\"\n```\n";
        let text = engine.render_terminal(src, 80);
        let plain = text_to_plain(&text);

        assert!(
            plain.contains("def greet"),
            "python code should appear in output: {plain:?}"
        );
    }

    #[test]
    fn unknown_lang_uses_fallback_not_panic() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let src = "```brainfuck_unknown\n+-><\n```\n";
        // Must not panic; content must appear in output.
        let text = engine.render_terminal(src, 80);
        let plain = text_to_plain(&text);

        assert!(
            plain.contains("+->"),
            "unknown lang content should still appear: {plain:?}"
        );
    }

    #[test]
    fn empty_lang_tag_uses_fallback_not_panic() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let src = "```\nplain code block\n```\n";
        let text = engine.render_terminal(src, 80);
        let plain = text_to_plain(&text);

        assert!(
            plain.contains("plain code block"),
            "empty lang content should appear: {plain:?}"
        );
    }

    #[test]
    fn highlighting_disabled_falls_through_to_plain() {
        // PulldownEngine::with_gfm() has no highlighter — code blocks use plain styling.
        let engine = PulldownEngine::with_gfm();
        let src = "```rust\nlet x = 1;\n```\n";
        let text = engine.render_terminal(src, 80);
        let plain = text_to_plain(&text);
        assert!(
            plain.contains("let x = 1;"),
            "plain engine should still show code: {plain:?}"
        );

        // No Rgb spans in plain mode.
        let has_rgb = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .any(|s| matches!(s.style.fg, Some(Color::Rgb(_, _, _))));

        assert!(
            !has_rgb,
            "plain engine should not produce Rgb-colored spans"
        );
    }

    #[test]
    fn no_newlines_in_highlighted_span_content() {
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let src = "```rust\nlet a = 1;\nlet b = 2;\n```\n";
        let text = engine.render_terminal(src, 80);

        for line in &text.lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\n'),
                    "span must not contain newlines: {:?}",
                    span.content
                );
            }
        }
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

    #[test]
    fn snapshot_highlight_rust() {
        let src = include_str!("../../tests/fixtures/highlight_rust.md");
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal(src, 80);
        let out = text_to_plain(&text);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_highlight_python() {
        let src = include_str!("../../tests/fixtures/highlight_python.md");
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal(src, 80);
        let out = text_to_plain(&text);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_highlight_unknown_lang() {
        let src = include_str!("../../tests/fixtures/highlight_unknown_lang.md");
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal(src, 80);
        let out = text_to_plain(&text);

        insta::assert_snapshot!(out);
    }

    #[test]
    fn snapshot_highlight_empty_lang() {
        let src = include_str!("../../tests/fixtures/highlight_empty_lang.md");
        let engine = PulldownEngine::with_highlighting("base16-ocean.dark");
        let text = engine.render_terminal(src, 80);
        let out = text_to_plain(&text);

        insta::assert_snapshot!(out);
    }
}
