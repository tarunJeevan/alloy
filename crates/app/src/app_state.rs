//! Central application state.
//!
//! `AppState` is the single source of truth threaded through the event loop.
//! It owns the `Config` and the current `Document` and tracks whether the app should exit.

use alloy_core::{config::Config, document::Document};

/// The complete runtime state of the editor.
#[derive(Debug)]
#[allow(dead_code)]
pub struct AppState {
    /// Loaded (or default) configuration.
    pub config: Config,

    /// The currently open document.
    pub document: Document,

    /// When `true` the event loop will break and the app wil exit.
    pub should_quit: bool,
}

impl AppState {
    /// Create a new `AppState` with the given config and document.
    #[allow(dead_code)]
    pub fn new(config: Config, document: Document) -> Self {
        Self {
            config,
            document,
            should_quit: false,
        }
    }

    /// The string shown in the status bar for the open file.
    ///
    /// Appends `[+]` when the document has unsaved changes.
    #[allow(dead_code)]
    pub fn status_filename(&self) -> String {
        let name = self.document.display_name();
        if self.document.modified {
            format!("[+] {name}")
        } else {
            name
        }
    }
}
