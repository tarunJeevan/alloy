//! `ComrakEngine` - HTML output backend using `comrak`.
//!
//! This engine is used exclusively for `PreviewMode::Html` output. Terminal rendering is delegated to an embedded `PulldownEngine` so there is one canonical terminal render path across the whole application.
//!
//! Extension flags:
//!
//! - `ComrakExtensions` mirrors `EngineExtensions` but is kept as a separate type so the `markdown` crate has no dependency on `alloy-core`. The caller `App::new` via `spawn_worker` copies the relevant booleans from `Config::markdown.extensions` into this struct.
//!
//! HTML line truncation:
//!
//! - Very large documents can produce thousands of HTML lines. The preview pane caps display at `HTML_LINE_LIMIT` and appends a `// [truncated]` tail line.
//! - The full HTML string is still stored on App for any future use (e.g. copy-to-clipboard).

use comrak::{Options, markdown_to_html};
use ratatui::text::Text;

use crate::engine::MarkdownEngine;
use crate::engines::pulldown::{EngineExtensions, PulldownEngine};

/// Maximum number of HTML lines sent to the preview `Paragraph` widget.
/// Lines beyond this limit are truncated with a trailing indicator.
const HTML_LINE_LIMIT: usize = 2000;

// ---------------------------------------------------
// Extension flags
// ---------------------------------------------------

/// GFM/dialect extension flags used to configure `comrak::Options`.
///
/// Kept separate from `pulldown::EngineExtensions` so that this crate does not import from `alloy-core`.
#[derive(Debug, Clone, Default)]
pub struct ComrakExtensions {
    pub gfm: bool,
    pub wiki_links: bool,
    pub footnotes: bool,
    pub frontmatter: bool,
    pub math: bool,
}

impl ComrakExtensions {
    /// Convert to a `comrak::Options` value with the appropriate extension flags set.
    pub fn to_comrak_options(&self) -> Options<'static> {
        let mut opts = Options::default();

        if self.gfm {
            opts.extension.table = true;
            opts.extension.tasklist = true;
            opts.extension.strikethrough = true;
            opts.extension.autolink = true;
        }

        if self.wiki_links {
            opts.extension.wikilinks_title_after_pipe = true;
        }

        if self.footnotes {
            opts.extension.footnotes = true;
        }

        if self.frontmatter {
            // comrak supports frontmatter via a delimiter string
            opts.extension.front_matter_delimiter = Some("---".into());
        }

        if self.math {
            opts.extension.math_code = true;
            opts.extension.math_dollars = true;
        }

        // SAFETY: We always want to produce well-formed HTML/
        opts.render.r#unsafe = false;

        opts
    }

    /// Build a matching `EngineExtensions` for the embedded `PulldownEngine`.
    pub fn to_pulldown_extensions(&self) -> EngineExtensions {
        EngineExtensions {
            gfm: self.gfm,
            footnotes: self.footnotes,
            wiki_links: self.wiki_links,
        }
    }
}

// ---------------------------------------------------
// ComrakEngine
// ---------------------------------------------------

/// Markdown backend that produces HTML via `comrak` and delegates terminal rendering to an embedded `PulldownEngine`.
///
/// THREAD SAFETY
///
/// `ComrakEngine` is `Send + Sync` because:
/// - `comrak::Options` contains only plain data (no interior mutability)
/// `PulldownEngine` is already `Send + Sync`
pub struct ComrakEngine {
    opts: Options<'static>,
    terminal_engine: PulldownEngine,
}

impl ComrakEngine {
    /// Construct with explicit extension flags.
    pub fn new(extensions: ComrakExtensions) -> Self {
        let terminal_engine = PulldownEngine::new(extensions.to_pulldown_extensions());
        let opts = extensions.to_comrak_options();

        Self {
            opts,
            terminal_engine,
        }
    }

    /// Convenience constructor for tests - GFM on, everything else off.
    pub fn with_gfm() -> Self {
        Self::new(ComrakExtensions {
            gfm: true,
            ..Default::default()
        })
    }
}

impl MarkdownEngine for ComrakEngine {
    /// Delegate to the embedded `PulldownEngine` - one canonical terminal render path across the whole app.
    fn render_terminal(&self, src: &str, col_width: u16) -> Text<'static> {
        self.terminal_engine.render_terminal(src, col_width)
    }

    /// Render the Markdown source to an HTML string using `comrak`.
    fn render_html(&self, src: &str) -> String {
        markdown_to_html(src, &self.opts)
    }
}

// ---------------------------------------------------
// HTML -> displayable line helper
// ---------------------------------------------------

/// Convert an HTML string into individual `&str` lines, capped at `HTML_LINE_LIMIT`.
///
/// Returns an owned `Vec<String>` for use in `ui::render_preview`.
///
/// This function is `pub (crate)` so `ui.rs` can call it via the engine's `render_html` output without re-importing comrak.
pub fn html_to_lines(html: &str) -> Vec<String> {
    let mut lines: Vec<String> = html.lines().map(str::to_owned).collect();

    if lines.len() > HTML_LINE_LIMIT {
        lines.truncate(HTML_LINE_LIMIT);
        lines.push("// [truncated - document too large for inline HTML view]".to_owned());
    }

    lines
}

// ---------------------------------------------------
// Tests
// ---------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn engine() -> ComrakEngine {
        ComrakEngine::with_gfm()
    }

    // HTML output

    #[test]
    fn render_html_produces_h1_tag() {
        let html = engine().render_html("# Hello\n");

        assert!(
            html.contains("<h1>") || html.contains("<h1 "),
            "expected <h1> in output: {html:?}"
        );
        assert!(html.contains("Hello"), "expected heading text: {html:?}");
    }

    #[test]
    fn render_html_produces_paragraph() {
        let html = engine().render_html("Some text.\n");

        assert!(html.contains("<p>"), "expected <p> tag: {html:?}");
        assert!(
            html.contains("Some text"),
            "expected paragraph content: {html:?}"
        );
    }

    #[test]
    fn render_html_produces_bold() {
        let html = engine().render_html("**bold**\n");

        assert!(html.contains("<strong>"), "expected <strong>: {html:?}");
    }

    #[test]
    fn render_html_produces_code_block() {
        let html = engine().render_html("```rust\nfn main() {}\n```\n");

        assert!(html.contains("<code"), "expected <code> tag: {html:?}");
        assert!(html.contains("fn main"), "expected code content: {html:?}");
    }

    #[test]
    fn render_html_gfm_table() {
        let src = "| A | B |\n|---|---|\n| 1 | 2 |\n";
        let html = engine().render_html(src);

        assert!(html.contains("<table"), "expected <table>: {html:?}");
    }

    #[test]
    fn render_html_gfm_task_list() {
        let src = "- [x] Done\n- [ ] Todo\n";
        let html = engine().render_html(src);

        assert!(
            html.contains("checkbox") || html.contains("checked"),
            "expected checkbox markup: {html:?}"
        );
    }

    #[test]
    fn render_html_empty_input() {
        let html = engine().render_html("");

        // comrak returns empty string or just whitespace for empty input
        assert!(
            html.trim().is_empty() || html.len() < 20,
            "expected minimal output for empty input: {html:?}"
        );
    }

    // Terminal delegation

    #[test]
    fn render_terminal_delegates_to_pulldown() {
        let text = engine().render_terminal("# Title\n\nParagraph.\n", 80);
        let plain: String = text
            .lines
            .iter()
            .flat_map(|l| l.spans.iter())
            .map(|s| s.content.as_ref())
            .collect::<Vec<_>>()
            .join("");

        assert!(plain.contains("Title"), "expected heading text: {plain:?}");
        assert!(
            plain.contains("Paragraph"),
            "expected paragraph text: {plain:?}"
        );
    }

    // html_to_lines

    #[test]
    fn html_to_lines_basic_split() {
        let html = "<p>line 1</p>\n<p>line 2</p>";
        let lines = html_to_lines(html);

        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0], "<p>line 1</p>");
    }

    #[test]
    fn html_to_lines_truncates_at_limit() {
        let many_lines = (0..HTML_LINE_LIMIT + 50)
            .map(|i| format!("<p>{i}</p>"))
            .collect::<Vec<_>>()
            .join("\n");
        let lines = html_to_lines(&many_lines);

        // Should be HTML_LINE_LIMIT + 1 (the truncation marker)
        assert_eq!(lines.len(), HTML_LINE_LIMIT + 1);
        assert!(
            lines.last().unwrap().contains("[truncated"),
            "expected truncation marker"
        );
    }

    // Snapshot tests

    #[test]
    fn snapshot_basic_md_html() {
        let src = include_str!("../../tests/fixtures/basic.md");
        let html = engine().render_html(src);

        insta::assert_snapshot!(html);
    }

    #[test]
    fn snapshot_code_md_html() {
        let src = include_str!("../../tests/fixtures/code.md");
        let html = engine().render_html(src);

        insta::assert_snapshot!(html);
    }

    #[test]
    fn snapshot_lists_md_html() {
        let src = include_str!("../../tests/fixtures/lists.md");
        let html = engine().render_html(src);

        insta::assert_snapshot!(html);
    }
}
