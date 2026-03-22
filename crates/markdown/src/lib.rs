//! Markdown parsing and terminal/HTML rendering pipeline.
//!
//! Architecture:
//!
//! &str (Markdown source)
//! 	↓
//! MarkdownEngine::render_terminal(src, col_width)
//! 	|	-> pulldown-cmark events -> LineBuilder -> Text<'static>
//! 	↓
//! MarkdownEngine::render_html(src)
//! 	|	-> comrak / pulldown-cmark -> String (Phase 4)
//!
//! The `MarkdownEngine` trait is the public interface. The `PulldownEngine` struct is the default implementation used by the previous worker.

pub mod engine;
pub mod engines;

pub use engine::MarkdownEngine;
pub use engines::{
    comrak::{ComrakEngine, ComrakExtensions, html_to_lines},
    pulldown::PulldownEngine,
};
