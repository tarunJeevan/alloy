//! Markdown parsing and terminal/HTML rendering pipeline.
//!
//! Architecture:
//!
//! &str (Markdown source)
//!     ↓
//! MarkdownEngine::render_terminal(src, col_width)
//!     |   -> pulldown-cmark events -> LineBuilder -> Text<'static>
//!     |   -> syntect highlighting for fenced code blocks
//!     ↓
//! MarkdownEngine::render_html(src)
//!     |   -> comrak / pulldown-cmark -> String
//!
//! `MarkdownEngine` trait is the public interface.
//! `PulldownEngine` struct is the default terminal renderer.
//! `ComrakEngine` handles HTML output.
//! `Highlighter` provides syntect-based code block highlighting.

pub mod engine;
pub mod engines;
pub mod highlight;

pub use engine::MarkdownEngine;
pub use engines::{
    comrak::{ComrakEngine, ComrakExtensions, html_to_lines},
    pulldown::PulldownEngine,
};
pub use highlight::Highlighter;
