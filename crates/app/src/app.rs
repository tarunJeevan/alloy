//! Central application state and event handling.
//!
//! `App` is the single source of truth threaded through the event loop. It owns:
//! - the live `tui-textarea` edit buffer
//! - the `Document` persistence layer
//! - the current `EditorMode`
//! - the keymap dispatcher
//! - the notification queue
//! - the command-mode input buffer
//!
//! Architecture note: live buffer vs persistence layer
//!
//! `tui-textarea`'s `TextArea` is the live edit buffer - it's the authoritative source for the text the user is currently editing and owns undo/redo history.
//!
//! `Document` is the persistence layer - it tracks the on-disk path, modified state, and last-saved content (as a `Rope`).
//! It is NOT kept in sync with every keystroke; sync only happens at:
//! - startup: `Document::content()` -> seeds `TextArea`
//! - save: `TextArea::lines().join("\n")` -> `Document::save()`

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossterm::event::KeyEvent;
use ratatui::style::{Color, Style};
use tui_textarea::{CursorMove, TextArea};

use alloy_core::{
    // errors::CoreError,
    EditorMode,
    config::Config,
    document::Document,
};

use crate::keymap::{EditorAction, KeymapDispatcher};

// --------------------------------------------------------------------
// Notification types
// --------------------------------------------------------------------

/// Severety level of a transient notification message
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NotificationLevel {
    Info,
    Warn,
    Error,
}

/// A transient message shown in the status bar until it expires.
#[derive(Debug, Clone)]
pub struct Notification {
    pub message: String,
    pub level: NotificationLevel,
    /// Absolute point in time after which this notification should no longer be displayed
    pub expires_at: Instant,
}

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

    /// Queue of transient status messages. Expired entries are drained in `tick()`.
    pub notifications: Vec<Notification>,

    /// Text being typed in Command mode (does NOT include the leading `:`).
    pub command_input: String,
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
            notifications: Vec::new(),
            command_input: String::new(),
        }
    }

    // --------------------------------------------------------------------
    // Notification queue
    // --------------------------------------------------------------------

    /// Push a new transient notification into the queue.
    ///
    /// `duration` controls how long it is visible before being drained by `tick()`.
    pub fn push_notification(
        &mut self,
        message: impl Into<String>,
        level: NotificationLevel,
        duration: Duration,
    ) {
        self.notifications.push(Notification {
            message: message.into(),
            level,
            expires_at: Instant::now() + duration,
        });
    }

    /// Convenience wrapper for INFO severity level.
    pub fn notify_info(&mut self, msg: impl Into<String>) {
        self.push_notification(msg, NotificationLevel::Info, Duration::from_secs(4));
    }

    /// Convenience wrapper for WARN severity level.
    pub fn notify_warn(&mut self, msg: impl Into<String>) {
        self.push_notification(msg, NotificationLevel::Warn, Duration::from_secs(5));
    }

    /// Convenience wrapper for ERROR severity level.
    pub fn notify_error(&mut self, msg: impl Into<String>) {
        self.push_notification(msg, NotificationLevel::Error, Duration::from_secs(6));
    }

    /// Return the most recent non-expired notification, if any.
    pub fn active_notification(&self) -> Option<&Notification> {
        let now = Instant::now();

        // Iterate in reverse so the latest notification wins.
        self.notifications.iter().rev().find(|n| n.expires_at > now)
    }

    // --------------------------------------------------------------------
    // Event handling
    // --------------------------------------------------------------------

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

    /// Call this once per event loop tick BEFORE polling for new events.
    ///
    /// Drain:
    /// - Pending keymap actions that have timed out
    /// - Expired notifications
    pub fn tick(&mut self) -> Result<()> {
        // Flush any timed-out pending keymap sequences.
        // Loop because a single tick() call flushes one key. Multiple pending keys may need to be flushed in the same tick.
        while let Some(action) = self.keymap.tick() {
            self.handle_action(action)?;
        }

        // Drain expired notifications.
        let now = Instant::now();
        self.notifications.retain(|n| n.expires_at > now);

        Ok(())
    }

    /// Execute a high-level `EditorAction` against the app state.
    pub fn handle_action(&mut self, action: EditorAction) -> Result<()> {
        match action {
            // Mode transitions
            EditorAction::EnterInsert => {
                self.mode = EditorMode::Insert;
                apply_insert_mode_style(&mut self.textarea, &self.config);
                tracing::debug!("mode -> INSERT");
            }
            EditorAction::ExitInsert => {
                // Shared exit path - exits both Insert and Command modes.
                if self.mode == EditorMode::Command {
                    self.command_input.clear();
                    tracing::debug!("command cancelled, mode -> NORMAL")
                } else {
                    tracing::debug!("mode -> NORMAL")
                }
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);
            }
            EditorAction::EnterCommand => {
                self.mode = EditorMode::Command;
                self.command_input.clear();
                tracing::debug!("mode -> COMMAND");
            }

            // Normal-mode motions
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

            // Normal-mode edit actions
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

            // Command-mode
            EditorAction::CommandInput(c) => {
                self.command_input.push(c);
            }
            EditorAction::CommandBackspace => {
                self.command_input.pop();
            }
            EditorAction::ExecuteCommand => {
                let cmd = self.command_input.trim().to_string();
                self.command_input.clear();
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);
                self.execute_command(&cmd);
            }

            // App-level (Normal-mode)
            EditorAction::Save => {
                if let Err(e) = self.save() {
                    self.notify_error(format!("Save failed: {e:#}"));
                }
            }
            EditorAction::Quit => {
                if self.document.modified {
                    self.notify_warn("Unsaved changes - use `:w` to save, `:q!` to force quit");
                } else {
                    self.should_quit = true;
                }
            }

            EditorAction::Unbound => {
                // Silently ignore unrecognised keys.
            }
        }

        Ok(())
    }

    // --------------------------------------------------------------------
    // Command execution
    // --------------------------------------------------------------------

    /// Parse and execute a command string (the text typed after `:`).
    ///
    /// All errors are pushed to the notifications queue - this method never propagates errors to the caller.
    pub fn execute_command(&mut self, cmd: &str) {
        tracing::debug!(cmd, "execute_command");

        let (verb, rest) = split_command(cmd);

        match verb {
            // :w OR :w <path>
            "w" => {
                if let Some(path) = rest {
                    // save-as: update document path then save
                    self.document.path = Some(PathBuf::from(path));
                }
                if let Err(e) = self.save() {
                    self.notify_error(format!("Save failed: {e:#}"));
                } else {
                    let name = self.document.display_name();
                    self.notify_info(format!("Saved \"{name}\""));
                }
            }
            // :q
            "q" => {
                if self.document.modified {
                    self.notify_error("Unsaved changes - use `:w` to save and `:q!` to foce quit");
                } else {
                    self.should_quit = true;
                }
            }
            // :q!
            "q!" => {
                self.should_quit = true;
            }
            // :wq
            "wq" => {
                if let Err(e) = self.save() {
                    self.notify_error(format!("Save failed: {e:#}"));
                } else {
                    self.should_quit = true;
                }
            }
            // :e <path>
            "e" => match rest {
                None => {
                    self.notify_error(":e requires a file path");
                }
                Some(path_str) => {
                    let path = PathBuf::from(path_str);
                    match Document::open(&path) {
                        Err(e) => {
                            self.notify_error(format!("Cannot open '{path_str}': {e:#}"));
                        }
                        Ok(doc) => {
                            let content = doc.content();
                            let lines: Vec<String> = if content.is_empty() {
                                vec![String::new()]
                            } else {
                                content.split('\n').map(String::from).collect()
                            };
                            self.document = doc;
                            self.textarea = TextArea::new(lines);
                            apply_normal_mode_style(&mut self.textarea, &self.config);
                            let name = self.document.display_name();
                            self.notify_info(format!("Opened \"{name}\""));
                        }
                    }
                }
            },
            // Unknown command
            other => {
                self.notify_error(format!("Unkown command: :{other}"));
            }
        }
    }

    // --------------------------------------------------------------------
    // Save
    // --------------------------------------------------------------------

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
// Command string parsing helper
// --------------------------------------------------------------------

/// Split a command string into `(verb, optional argument)`.
///
/// Examples;
/// - "w" -> ("w", None)
/// - "w foo.md" -> ("w", Some("foo.md"))
fn split_command(cmd: &str) -> (&str, Option<&str>) {
    let cmd = cmd.trim();
    match cmd.find(char::is_whitespace) {
        None => (cmd, None),
        Some(pos) => {
            let verb = &cmd[..pos];
            let rest = cmd[pos..].trim();
            (verb, if rest.is_empty() { None } else { Some(rest) })
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

// --------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // split_command

    #[test]
    fn split_w_no_arg() {
        assert_eq!(split_command("w"), ("w", None));
    }

    #[test]
    fn split_w_with_path() {
        assert_eq!(split_command("w foo.md"), ("w", Some("foo.md")));
    }

    #[test]
    fn split_e_with_path() {
        assert_eq!(
            split_command("e path/to/file.md"),
            ("e", Some("path/to/file.md"))
        );
    }

    #[test]
    fn split_q_bang() {
        assert_eq!(split_command("q!"), ("q!", None));
    }

    #[test]
    fn split_with_surrounding_whitespace() {
        assert_eq!(split_command("  w  bar.md  "), ("w", Some("bar.md")));
    }

    // App command execution (uses Document::new() so no disk I/O)

    fn make_app() -> App {
        App::new(Config::default(), Document::new())
    }

    #[test]
    fn execute_w_with_no_path_pushes_error_notification() {
        let mut app = make_app();
        // Document has no path → save fails → notification is pushed.
        app.execute_command("w");

        assert!(
            app.notifications
                .iter()
                .any(|n| n.level == NotificationLevel::Error),
            "expected an error notification when saving with no path"
        );
        assert!(!app.should_quit);
    }

    #[test]
    fn execute_q_with_modified_document_does_not_quit() {
        let mut app = make_app();
        app.document.modified = true;
        app.execute_command("q");

        assert!(!app.should_quit, ":q with unsaved changes must not quit");
        assert!(
            app.notifications
                .iter()
                .any(|n| n.level == NotificationLevel::Error),
            "expected an error notification"
        );
    }

    #[test]
    fn execute_q_bang_force_quits_regardless_of_modified() {
        let mut app = make_app();
        app.document.modified = true;
        app.execute_command("q!");

        assert!(app.should_quit, ":q! must set should_quit = true");
    }

    #[test]
    fn execute_unknown_command_pushes_error_notification() {
        let mut app = make_app();
        app.execute_command("frobnicate");

        assert!(
            app.notifications
                .iter()
                .any(|n| n.level == NotificationLevel::Error),
            "unknown command should push an error notification"
        );
    }

    #[test]
    fn push_notification_and_active_notification() {
        let mut app = make_app();

        assert!(app.active_notification().is_none());

        app.notify_info("hello");

        assert!(app.active_notification().is_some());
        assert_eq!(app.active_notification().unwrap().message, "hello");
    }

    #[test]
    fn enter_command_mode_clears_command_input() {
        let mut app = make_app();
        app.command_input = "leftover".to_string();
        app.handle_action(EditorAction::EnterCommand).unwrap();

        assert_eq!(app.mode, EditorMode::Command);
        assert!(app.command_input.is_empty());
    }

    #[test]
    fn command_input_appends_chars() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterCommand).unwrap();
        app.handle_action(EditorAction::CommandInput('w')).unwrap();
        app.handle_action(EditorAction::CommandInput('q')).unwrap();

        assert_eq!(app.command_input, "wq");
    }

    #[test]
    fn command_backspace_pops_char() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterCommand).unwrap();
        app.handle_action(EditorAction::CommandInput('w')).unwrap();
        app.handle_action(EditorAction::CommandInput('q')).unwrap();
        app.handle_action(EditorAction::CommandBackspace).unwrap();

        assert_eq!(app.command_input, "w");
    }

    #[test]
    fn esc_in_command_mode_returns_to_normal_and_clears_input() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterCommand).unwrap();
        app.handle_action(EditorAction::CommandInput('w')).unwrap();
        app.handle_action(EditorAction::ExitInsert).unwrap();

        assert_eq!(app.mode, EditorMode::Normal);
        assert!(app.command_input.is_empty());
    }

    // :wq integration: saves to temp file then sets should_quit
    #[test]
    fn execute_wq_saves_and_quits() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(b"# Test").unwrap();
        let path = tmp.path().to_path_buf();

        let doc = Document::open(&path).expect("open temp file");
        let mut app = App::new(Config::default(), doc);

        app.execute_command(&format!("wq"));

        // There's a valid path, so save should succeed and should_quit = true.
        // (The actual wq path-less case is tested by execute_w_with_no_path_pushes_error_notification)
        // Here we use the path set on the document from open().
        assert!(app.should_quit, ":wq on a file with a path should quit");
    }
}
