//! The `MarkdownEngine` trait - a stable abstraction over Markdown parser backends.
//!
//! Design rationale:
//!
//! Both methods take `src: &str` and return owned values so the engine can be used from a backend thread without lifetime entanglement. `col_width` is passed into `render_terminal` because the renderer is responsible for line-wrapping.
//!
//! Thread safety:
//!
//! Implementations must be `Send + Sync` so that an `Arc<dyn MarkdownEngine>` can be shared with the preview worker thread.
//!
//! `render_terminal_with_links`:
//!
//! - The extended method that returns both the rendered `Text` and a `LinkIndex`. Engines that support link extraction override this.
//! - The default implementation delegates to `render_terminal` and returns an empty index so existing engines don't break.

use ratatui::text::Text;

use alloy_core::links::LinkIndex;

/// A Markdown parser/renderer backend.
///
/// The trait is object-safe. Callers hold it as `Arc<dyn MarkdownEngine>`.
pub trait MarkdownEngine: Send + Sync {
    /// Parse `src` and produce a ratatui `Text` suitable for display in the preview pane.
    ///
    /// `col_width` is the usable column count of the preview pane (border widths already subtracted by the caller). The renderer uses this value to hard-wrap long lines so that ratatui's own wrapping never splits a styled `Span` mid-token.
    fn render_terminal(&self, src: &str, col_width: u16) -> Text<'static>;

    /// Parse `src` and produce an HTML string.
    ///
    /// This is used by `PreviewMode::Html` (Phase 4). Implementations that don't yet support HTML output should return `unimplemented!()` or a commented-out placeholder. The calling code guards against that with the preview mode check.
    fn render_html(&self, src: &str) -> String;

    /// Parse `src`, produce `Text` AND extract a `LinkIndex` in a single pass.
    ///
    /// The default implemetation calls `render_terminal` and returns an empty `LinkIndex`. Engines that support link extraction (currently `PulldownEngine`) override this to avoid a second parse pass.
    ///
    /// The preview worker always calls this method so the link index ia available to the UI with zero extra cost.
    fn render_terminal_with_links(&self, src: &str, col_width: u16) -> (Text<'static>, LinkIndex) {
        (self.render_terminal(src, col_width), LinkIndex::new())
    }
}
