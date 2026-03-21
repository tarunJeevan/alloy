//! The `MarkdownEngine` trait - a stable abstraction over Markdown parser backends.
//!
//! Design rationale:
//!
//! Both methods take `src: &str` and return owned values so the engine can be used from a backend thread without lifetime entanglement. `col_width` is passed into `render_terminal` because the renderer is responsible for line-wrapping.
//!
//! Thread safety:
//!
//! Implementations must be `Send + Sync` so that an `Arc<dyn MarkdownEngine>` can be shared with the preview worker thread.

use ratatui::text::Text;

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
}
