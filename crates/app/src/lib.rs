//! `alloy_app` - the library target for the alloy binary.
//!
//! Splitting the application logic into a `lib` + `bin` pair lets `tests/` integration tests import `alloy_app::*` directly.
//!
//! `main.rs` is the thin entry point - it sets up the terminal, calls `run_event_loop`, and restores the terminal on exit. All runtime state and event dispatch live in these modules.
//!
//! Module visibility:
//! - All modules are `pub` so integration tests can access their types.
//! - Items that are implementation details (not part of the public API) are `pub(crate)` inside their module - they remain inaccessible to consumers of the lib but are visible to sibling modules via `crate::`.

pub mod app;
pub mod cli;
pub mod image_cache;
pub mod image_encoder;
pub mod image_proto;
pub mod keymap;
pub mod preview_worker;
pub mod ui;

pub use app::{App, Notification, NotificationLevel, PreviewMode};
pub use cli::CliArgs;
pub use image_proto::DetectedImageProtocol;
pub use keymap::EditorAction;
