//! Central application struct.
//!
//! `App` is the single source of truth threaded through the event loop. It owns:
//! - the `AppState`
//! - the live `tui-textarea` edit buffer
//! - the current `EditorMode`
//!
//! Architecture note: live buffer vs persistence layer
//!
//! `tui-textarea`'s `TextArea` is the live edit buffer - it's the authoritative source for the text the user is currently editing and owns undo/redo history.
//!
//! `Document` is the persistence layer - it tracks the on-disk path, modified state, and last-saved content (as a `Rope`).
//! It is NOT kept in sync with every keystroke; sync only happens at:
//! - startup: `Document::content()` -> seeds `TextArea`
//! - save: `TextArea::lines().join("\n")` -> `Document::save()`

use anyhow::{Context, Result};
use crossterm::event::KeyEvent;
use ratatui::style::{Color, Style};
use tui_textarea::{CursorMove, TextArea};

use alloy_core::{
    EditorMode,
    config::Config,
    document::Document,
    // errors::CoreError,
};

use crate::keymap::{EditorAction, KeymapDispatcher};

// --------------------------------------------------------------------
// App
// --------------------------------------------------------------------

/// The complete runtime state of the editor.
pub struct App {
    /// The currently open document (persistence / metadata layer).
    pub document: Document,

    /// Current modal state of the editor.
    pub mode: EditorMode,

    /// The live interactive editor surface backed by `tui-textarea`.
    ///
    /// `'static` lifetime is required by `tui-textarea`'s API.
    /// The `TextArea` owns its own `String` content and doesn't borrow from anywhere.
    pub textarea: TextArea<'static>,

    /// Loaded application configuration.
    pub config: Config,

    /// When `true` the event loop will break on the next iteration.
    pub should_quit: bool,

    /// Keymap dispatcher - holds the pending multi-key sequence state.
    pub keymap: KeymapDispatcher,
}

impl App {
    /// Construct a new `App` from a loaded config and document.
    ///
    /// Seeds the `TextArea` from `document.content()` and applies initial styling from config.
    pub fn new(config: Config, document: Document) -> Self {
        let content = document.content();

        // `TextArea::new()` accepts `Vec<String>` lines. Split on '\n'.
        // DO NOT include the trailing empty string that `str::lines()` would omit but a trailing '\n' produces with `split`.
        let lines: Vec<String> = if content.is_empty() {
            vec![String::new()]
        } else {
            // Use split('\n') so a trailing newline produces a trailing empty line that tui-textarea can represent correctly.
            content.split('\n').map(String::from).collect()
        };

        let mut textarea = TextArea::new(lines);

        // Apply initial styles based on starting mode (Normal).
        apply_normal_mode_style(&mut textarea, &config);

        let timeout_ms = config.editor.sequence_timeout_ms;

        Self {
            document,
            mode: EditorMode::Normal,
            textarea,
            config,
            should_quit: false,
            keymap: KeymapDispatcher::new(timeout_ms),
        }
    }

    // Event handling

    /// Process a raw key event.
    ///
    /// Returns any error that should be surfaced to the user (e.g. save failure). IO errors do not crash the app.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        // Dispatch the raw key through the keymap to get a high-level action.
        if let Some(action) = self.keymap.dispatch(key, &self.mode) {
            self.handle_action(action)?;
        }

        // None means the key was buffered (waiting for a multi-key sequence or timeout).
        Ok(())
    }

    /// Drain any pending keymap actions that have timed out.
    ///
    /// Call this once per event loop tick BEFORE polling for new events.
    pub fn tick(&mut self) -> Result<()> {
        // Loop because a single tick() call flushes one key. Multiple pending keys may need to be flushed in the same tick.
        while let Some(action) = self.keymap.tick() {
            self.handle_action(action)?;
        }

        Ok(())
    }

    /// Execute a high-level `EditorAction` against the app state.
    pub fn handle_action(&mut self, action: EditorAction) -> Result<()> {
        match action {
            EditorAction::EnterInsert => {
                self.mode = EditorMode::Insert;
                apply_insert_mode_style(&mut self.textarea, &self.config);
                tracing::debug!("mode -> INSERT");
            }
            EditorAction::ExitInsert => {
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);
                tracing::debug!("mode → NORMAL");
            }
            EditorAction::EnterCommand => {
                // Stub: command mode implemented in Chunk 2.3.
                // For now, do nothing — the `:` key won't crash or misbehave.
                tracing::debug!("EnterCommand (stub — Chunk 2.3)");
            }

            // Motions
            EditorAction::MoveLeft => {
                self.textarea.move_cursor(CursorMove::Back);
            }
            EditorAction::MoveDown => {
                self.textarea.move_cursor(CursorMove::Down);
            }
            EditorAction::MoveUp => {
                self.textarea.move_cursor(CursorMove::Up);
            }
            EditorAction::MoveRight => {
                self.textarea.move_cursor(CursorMove::Forward);
            }
            EditorAction::MoveWordForward => {
                self.textarea.move_cursor(CursorMove::WordForward);
            }
            EditorAction::MoveWordBackward => {
                self.textarea.move_cursor(CursorMove::WordBack);
            }
            EditorAction::MoveLineStart => {
                self.textarea.move_cursor(CursorMove::Head);
            }
            EditorAction::MoveLineEnd => {
                self.textarea.move_cursor(CursorMove::End);
            }
            EditorAction::MoveDocStart => {
                self.textarea.move_cursor(CursorMove::Top);
            }
            EditorAction::MoveDocEnd => {
                self.textarea.move_cursor(CursorMove::Bottom);
            }

            // Editing
            EditorAction::DeleteCharBackward => {
                self.textarea.delete_char();
                self.document.modified = true;
            }
            EditorAction::DeleteCharForward => {
                self.textarea.delete_next_char();
                self.document.modified = true;
            }

            // Insert-mode text input
            EditorAction::TextInput(input) => {
                self.textarea.input(input);
                self.document.modified = true;
            }

            // App-level
            EditorAction::Save => {
                self.save().context("save failed")?;
            }
            EditorAction::Quit => {
                self.should_quit = true;
            }

            EditorAction::Unbound => {
                // Silently ignore unrecognised keys.
            }
        }

        Ok(())
    }

    // Save

    /// Flush the live textarea content to disk via `Document::save`.
    ///
    /// Returns an error (to be shown in the notification queue) if no path
    /// is set or if the write fails. Does NOT crash the app.
    pub fn save(&mut self) -> Result<()> {
        let content = self.textarea_content();
        self.document
            .save(&content)
            .context("failed to write file")?;
        tracing::info!(path = ?self.document.path, "document saved");
        Ok(())
    }

    // Accessors

    /// Extract the current live content from the textarea as a single `String`.
    ///
    /// Uses `lines().join("\n")` — do NOT use `to_string()` on the textarea,
    /// which may introduce a trailing-newline inconsistency.
    pub fn textarea_content(&self) -> String {
        self.textarea.lines().join("\n")
    }

    /// Current cursor position as `(row, col)`, **1-indexed** for display.
    pub fn cursor_position(&self) -> (usize, usize) {
        let (row, col) = self.textarea.cursor();
        (row + 1, col + 1)
    }

    /// The string shown in the status bar for the open file.
    /// Appends `[+]` when the document has unsaved changes.
    pub fn status_filename(&self) -> String {
        let name = self.document.display_name();
        if self.document.modified {
            format!("{name} [+]")
        } else {
            name
        }
    }
}

// --------------------------------------------------------------------
// Mode-aware textarea styling helpers
// --------------------------------------------------------------------

/// Apply Normal-mode styling to the textarea:
/// - Block-style (thick) cursor
/// - Dim/inactive cursor color to reinforce "not typing" state
fn apply_normal_mode_style(textarea: &mut TextArea<'static>, _config: &Config) {
    use ratatui::widgets::Block;

    textarea.set_cursor_line_style(Style::default());
    textarea.set_cursor_style(Style::default().bg(Color::LightYellow).fg(Color::Black));
    // Remove any block so the border is set by ui::render, not here.
    textarea.set_block(Block::default());
}

/// Apply Insert-mode styling to the textarea:
/// - Bar/underline cursor to signal text-entry state
fn apply_insert_mode_style(textarea: &mut TextArea<'static>, _config: &Config) {
    use ratatui::widgets::Block;

    textarea.set_cursor_line_style(Style::default().bg(Color::Reset));
    textarea.set_cursor_style(Style::default().bg(Color::LightCyan).fg(Color::Black));
    textarea.set_block(Block::default());
}
