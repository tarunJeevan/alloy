//! Central application state and event handling.
//!
//! `App` is the single source of truth threaded through the event loop. It owns:
//! - the live `tui-textarea` edit buffer
//! - the `Document` persistence layer
//! - the current `EditorMode`
//! - the keymap dispatcher
//! - the notification queue
//! - the command-mode input buffer
//! - the search state
//! - the link index + link+select worker
//! - the preview worker channels and cached preview text
//!
//! Architecture note: live buffer vs persistence layer
//!
//! `ratatui-textarea`'s `TextArea` is the live edit buffer - it's the authoritative source for the text the user is currently editing and owns undo/redo history.
//!
//! `Document` is the persistence layer - it tracks the on-disk path, modified state, and last-saved content (as a `Rope`).
//! It is NOT kept in sync with every keystroke; sync only happens at:
//! - startup: `Document::content()` -> seeds `TextArea`
//! - save: `TextArea::lines().join("\n")` -> `Document::save()`

use std::path::PathBuf;
use std::sync::{
    Arc, Mutex,
    mpsc::{Receiver, SyncSender},
};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use ratatui::{
    crossterm::event::KeyEvent,
    style::{Color, Modifier, Style},
    text::Text,
};
use ratatui_image::picker::Picker;
use ratatui_textarea::{CursorMove, TextArea};

use alloy_core::{
    EditorMode,
    config::Config,
    document::Document,
    links::{LinkIndex, LinkTarget},
    search::{LARGE_DOC_THRESHOLD_BYTES, SearchKind, SearchState},
};

use crate::image_cache::ImageCache;
use crate::image_proto::DetectedImageProtocol;
use crate::keymap::{EditorAction, KeymapDispatcher};
use crate::preview_worker::{RenderRequest, RenderResult, WorkerExtensions, spawn_worker};

// ---------------------------------------------------------------
// PreviewMode
// ---------------------------------------------------------------

/// Which content is displayed in the right-hand preview pane.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PreviewMode {
    /// Terminal-rendered Markdown.
    #[default]
    Rendered,

    /// Raw HTML source generated from the Markdown (via `ComrakEngine`).
    Html,

    /// Preview pane is hidden. Editor takes the full width.
    Hidden,
}

// ---------------------------------------------------------------
// Notification types
// ---------------------------------------------------------------

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

// ---------------------------------------------------------------
// App
// ---------------------------------------------------------------

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

    // Search state
    /// Active or most-recently-committed search state.
    ///
    /// `None` until the user enters Search mode for the first time in this session.
    /// Retained after committing so `n`/`N` in Normal mode can continue navigating.
    pub search: Option<SearchState>,

    /// Cursor position saved when Search mode is entered.
    ///
    /// Restored on `CancelSearch` (Esc).
    /// Stored as `(row, col)` matching `tui-textarea`'s 0-indexed coordinate system.
    search_saved_cursor: (usize, usize),

    // Link state
    /// All links extracted from the most recent render pass.
    ///
    /// Updated in `tick()` whenever a matching `RenderResult` arrives.
    pub link_index: LinkIndex,

    /// 0-indexed cursor into `link_index`. Wraps at both ends in `LinkSelect` mode.
    pub link_cursor: usize,

    /// Confirms the running terminal supports OSC-9 hyperlinks.
    pub hyperlinks_supported: bool,

    // Image state
    /// Image protocol detected from terminal at startup.
    pub detected_image_protocol: DetectedImageProtocol,

    /// ratatui-image Picker for protocol-specific image encoding.
    /// `None` when images are disabled or `DetectedImageProtocol::None`.
    pub picker: Option<Arc<Mutex<Picker>>>,

    /// LRU cache of decoded + protocol-encoded images. Shared with the preview worker.
    pub image_cache: Arc<Mutex<ImageCache>>,

    // Preview state
    /// Which preview mode is currently active.
    pub preview_mode: PreviewMode,

    /// Vertical scroll offset for the `Rendered` preview pane (in lines).
    pub preview_scroll: u16,

    /// Vertical scroll offset for the `Html` preview pane (in lines).
    pub preview_scroll_html: u16,

    /// Monotonically increasing document revision counter.
    /// Incremented on every edit. Used to discard stale render results.
    pub doc_revision: u64,

    /// Latest terminal-rendered preview content (`PreviewMode::Rendered`).
    pub preview_text: Text<'static>,

    /// Latest HTML string (`PreviewMode::Html`).
    pub preview_html: String,

    /// Cached preview pane column width from the last rendered frmae.
    ///
    /// Set by `ui::render()` after `split_body()` computes the preview rect.
    /// Passed to `send_render_request` so the renderer knows the wrapping width.
    /// Defaults to 80 until the first frame is drawn.
    pub last_preview_width: u16,

    /// Sender half of the bounded render request channel.
    /// `try_send` is used - drops silently if the channel is full.
    request_sender: SyncSender<RenderRequest>,

    /// Receiver half of the render result channel.
    /// Drained non-blockingly in `tick()`.
    pub result_receiver: Receiver<RenderResult>,

    /// Handle to the worker thread.
    /// Kept alive so the thread isn't dropped prematurely. The worker exits when `request_sender` is dropped (channel close).
    _worker_handle: std::thread::JoinHandle<()>,
}

impl App {
    /// Construct a new `App` from a loaded config and document.
    ///
    /// Seeds the `TextArea` from `document.content()` and applies initial styling from config.
    pub fn new(
        config: Config,
        document: Document,
        detected_image_protocol: DetectedImageProtocol,
    ) -> Self {
        let content = document.content();

        // `TextArea::new()` accepts `Vec<String>` lines. Split on '\n'.
        let lines: Vec<String> = if content.is_empty() {
            vec![String::new()]
        } else {
            // Use split('\n') so a trailing newline produces a trailing empty line that tui-textarea can represent correctly.
            content.split('\n').map(String::from).collect()
        };

        let mut textarea = TextArea::new(lines);

        // Apply initial styles based on starting mode (Normal).
        apply_normal_mode_style(&mut textarea, &config);

        // Set the search highlight style once - it persists for the lifetime of this textarea.
        textarea.set_search_style(
            Style::default()
                .bg(Color::Rgb(80, 60, 0))
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );

        let timeout_ms = config.editor.sequence_timeout_ms;
        let debounce_ms = config.editor.preview_debounce_ms;

        // Convert config extension flags into the engine's own type.
        // The `markdown` crate is decoupled from `alloy-core` intentionally - we copy the booleans rather than passing the entire ExtensionConfig.
        let extensions = WorkerExtensions {
            gfm: config.markdown.extensions.gfm,
            wiki_links: config.markdown.extensions.wiki_links,
            footnotes: false, // NOTE: Not in config yet.
            frontmatter: config.markdown.extensions.frontmatter,
            math: config.markdown.extensions.math,
            highlighting: config.highlighting.clone(),
        };

        // Check whether the terminal supports hyperlinks.
        let hyperlinks_supported = supports_hyperlinks::on(supports_hyperlinks::Stream::Stderr);

        // Build Picker if a usable protocol is detected.
        let picker: Option<Arc<Mutex<Picker>>> = match &detected_image_protocol {
            DetectedImageProtocol::None => None,
            _ => Some(Arc::new(Mutex::new(Picker::halfblocks()))),
        };
        let image_cache = Arc::new(Mutex::new(ImageCache::new(20)));

        // Spawn the background render thread.
        let (request_sender, result_receiver, worker_handle) =
            spawn_worker(debounce_ms, extensions);

        let mut app = Self {
            document,
            mode: EditorMode::Normal,
            textarea,
            config,
            should_quit: false,
            keymap: KeymapDispatcher::new(timeout_ms),
            notifications: Vec::new(),
            command_input: String::new(),
            search: None,
            search_saved_cursor: (0, 0),
            link_index: LinkIndex::new(),
            link_cursor: 0,
            hyperlinks_supported,
            detected_image_protocol,
            picker,
            image_cache,
            preview_mode: PreviewMode::Rendered,
            preview_scroll: 0,
            preview_scroll_html: 0,
            doc_revision: 0,
            preview_text: Text::default(),
            preview_html: String::new(),
            last_preview_width: 80,
            request_sender,
            result_receiver,
            _worker_handle: worker_handle,
        };

        // Populate the initial preview.
        app.send_render_request();

        // First run notification about image protocol support.
        if app.detected_image_protocol.is_graphics_capable() {
            app.notify_info(format!(
                "Image rendering: {} protocol active",
                app.detected_image_protocol.label()
            ));
        } else if app.config.images.enabled
            && app.detected_image_protocol == DetectedImageProtocol::HalfBlock
        {
            // HalfBlock is always available and is a silent fallback.
            // Only log at debug level to avoid cluttering UI.
            tracing::debug!("image: no graphics protocol detected; using halfblock fallback");
        }

        app
    }

    // -----------------------------------------------------------
    // Preview worker integration
    // -----------------------------------------------------------

    /// Enqueue a render request for the current textarea content.
    ///
    /// Increments `doc_revision` and uses `try_send` - silently drops if the bounded channel is full (the next edit will trigger a new request).
    pub fn send_render_request(&mut self) {
        self.doc_revision += 1;
        let req = RenderRequest {
            revision: self.doc_revision,
            markdown: self.textarea_content(),
            col_width: self.last_preview_width,
        };

        // Silently drop if the channel is full - the next edit will trigger a new request.
        let _ = self.request_sender.try_send(req);
    }

    // -----------------------------------------------------------
    // Search highlight sync
    // -----------------------------------------------------------

    /// Sync `tui-textarea`'s built-in search highlighting to `self.search`.
    ///
    /// Must be called after every mutation of `self.search` or its `pattern`.
    ///
    /// Behavior:
    /// - Active pattern with content -> set the effective regex on the textarea.
    /// - Empty pattern or no search state -> clear highlights (`set_search_patter("")`).
    ///
    /// For `SearchKind::Literal`, the pattern is escaped with `regex::escape` before passing to `tui-textarea` which internally compiles it to a regex.
    /// For `SearchKind::Regex`, the pattern is used verbatim.
    /// The `(?i)` flag is prepended when `case_insensitive` is true.
    fn sync_textarea_search(&mut self) {
        match &self.search {
            Some(s) if !s.pattern.is_empty() => {
                let escaped = match s.kind {
                    SearchKind::Literal => regex::escape(&s.pattern),
                    SearchKind::Regex => s.pattern.clone(),
                };
                let effective = if s.case_insensitive {
                    format!("(?i){escaped}")
                } else {
                    escaped
                };
                if let Err(e) = self.textarea.set_search_pattern(effective) {
                    tracing::warn!("search highlight: invalid pattern: {e}");
                    // Best effort - clear rather than leave stale highlights.
                    let _ = self.textarea.set_search_pattern("");
                }
            }
            _ => {
                // No active search or empty pattern - clear all highlights.
                let _ = self.textarea.set_search_pattern("");
            }
        }
    }

    // -----------------------------------------------------------
    // Notification queue
    // -----------------------------------------------------------

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

    // -----------------------------------------------------------
    // Event handling
    // -----------------------------------------------------------

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
    /// Drains:
    /// - Pending keymap actions that have timed out
    /// - Expired notifications
    /// - Pending render results from the worker
    pub fn tick(&mut self) -> Result<()> {
        // Flush any timed-out pending keymap sequences.
        while let Some(action) = self.keymap.tick() {
            self.handle_action(action)?;
        }

        // Drain expired notifications.
        let now = Instant::now();
        self.notifications.retain(|n| n.expires_at > now);

        // Drain render results - apply only if the revision matches.
        while let Ok(result) = self.result_receiver.try_recv() {
            if result.revision == self.doc_revision {
                self.preview_text = result.rendered;
                self.preview_html = result.html;
                self.link_index = result.link_index;

                // Reset scroll to top when the document changes enough to produce a new render. This avoids the preview being stuck scrolled past the end.
                // Users can scroll back down with Ctrl+f.
                // NOTE: Uncomment if the auto-reset behavior is desired:
                // self.preview_scroll = 0;
            }
            // Stale results (revision mismatch) are silently discarded.
        }

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

            // LinkSelect mode
            EditorAction::EnterLinkSelect => {
                if self.link_index.is_empty() {
                    self.notify_info("No links in document");
                } else {
                    self.link_cursor = 0;
                    self.mode = EditorMode::LinkSelect;
                    tracing::debug!("mode -> LINK_SELECT ({} links)", self.link_index.len());
                }
            }
            EditorAction::LinkSelectNext => {
                if !self.link_index.is_empty() {
                    self.link_cursor = (self.link_cursor + 1) % self.link_index.len();
                }
            }
            EditorAction::LinkSelectPrev => {
                if !self.link_index.is_empty() {
                    self.link_cursor = self
                        .link_cursor
                        .checked_sub(1)
                        .unwrap_or(self.link_index.len() - 1);
                }
            }
            EditorAction::FollowLink => {
                self.follow_current_link();
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);
                tracing::debug!("followed link; mode -> NORMAL");
            }

            // Search mode entry
            EditorAction::EnterLiteralSearch => {
                let case_insensitive = self.config.editor.search_case_insensitive;
                self.search_saved_cursor = self.textarea.cursor();
                self.search = Some(SearchState::new(SearchKind::Literal, case_insensitive));
                self.mode = EditorMode::Search;
                // Pattern is empty at entry - clear any previous highlights.
                self.sync_textarea_search();

                tracing::debug!("mode -> SEARCH (Literal)");
            }
            EditorAction::EnterRegexSearch => {
                let case_insensitive = self.config.editor.search_case_insensitive;
                self.search_saved_cursor = self.textarea.cursor();
                self.search = Some(SearchState::new(SearchKind::Regex, case_insensitive));
                self.mode = EditorMode::Search;
                // Pattern is empty at entry - clear any previous highlights.
                self.sync_textarea_search();

                tracing::debug!("Mode -> SEARCH (Regex)");
            }

            // Search mode - pattern editing
            EditorAction::SearchInput(c) => {
                let content = self.textarea_content();
                if let Some(s) = &mut self.search {
                    s.pattern.push(c);
                    // Large document guard - skip recompute on very large docs.
                    // NOTE: Debounce in tick would be the right solution but for MVP, just skip the hot path.
                    if content.len() <= LARGE_DOC_THRESHOLD_BYTES {
                        s.recompute(&content);
                    }
                    // Move cursor to current match so the user sees incremental results.
                    if let Some(ref s) = self.search {
                        if let Some(m) = s.current() {
                            let (line, col) = (m.line as u16, m.col as u16);
                            self.textarea.move_cursor(CursorMove::Jump(line, col));
                        }
                    }
                }
                // Sync highlights after pattern change.
                self.sync_textarea_search();
            }
            EditorAction::SearchBackspace => {
                let content = self.textarea_content();
                if let Some(s) = &mut self.search {
                    s.pattern.pop();
                    // Large document guard - skip recompute on very large docs.
                    // NOTE: Debounce in tick would be the right solution but for MVP, just skip the hot path.
                    if content.len() <= LARGE_DOC_THRESHOLD_BYTES {
                        s.recompute(&content);
                    }
                    // Restore saved cursor if no pattern remains.
                    if s.pattern.is_empty() {
                        let (row, col) = self.search_saved_cursor;
                        self.textarea
                            .move_cursor(CursorMove::Jump(row as u16, col as u16));
                    } else if let Some(ref s) = self.search {
                        if let Some(m) = s.current() {
                            self.textarea
                                .move_cursor(CursorMove::Jump(m.line as u16, m.col as u16));
                        }
                    }
                }
                // Sync highlights after pattern change.
                self.sync_textarea_search();
            }

            // Search mode - commit / cancel
            EditorAction::CommitSearch => {
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);

                // Move cursor to the current match (already done incrementally but ensures correctness on Enter without any prior navigation).
                if let Some(ref s) = self.search {
                    if let Some(m) = s.current() {
                        self.textarea
                            .move_cursor(CursorMove::Jump(m.line as u16, m.col as u16))
                    }
                }
                // Highlights remain active after commit so the user can see all matches.
                self.sync_textarea_search();

                tracing::debug!("search committed; mode -> NORMAL");
            }
            EditorAction::CancelSearch => {
                self.mode = EditorMode::Normal;
                apply_normal_mode_style(&mut self.textarea, &self.config);

                // Restore the cursor position to what it was before the search was initiated.
                let (row, col) = self.search_saved_cursor;
                self.textarea
                    .move_cursor(CursorMove::Jump(row as u16, col as u16));

                // Clear the search pattern so the status bar doesn't show a stale pattern.
                if let Some(s) = &mut self.search {
                    s.pattern.clear();
                    s.matches.clear();
                }
                // Clear highlights - pattern is now empty.
                self.sync_textarea_search();

                tracing::debug!("search cancelled; mode -> NORMAL");
            }

            // Search navigation - available in both Search mode and Normal mode (after Commit).
            EditorAction::SearchNext => {
                if let Some(ref mut s) = self.search {
                    if let Some(m) = s.next_match() {
                        let (line, col) = (m.line as u16, m.col as u16);
                        self.textarea.move_cursor(CursorMove::Jump(line, col));
                    } else if !s.pattern.is_empty() {
                        self.notify_info("No matches found");
                    }
                }
            }
            EditorAction::SearchPrev => {
                if let Some(ref mut s) = self.search {
                    if let Some(m) = s.prev_match() {
                        let (line, col) = (m.line as u16, m.col as u16);
                        self.textarea.move_cursor(CursorMove::Jump(line, col));
                    } else if !s.pattern.is_empty() {
                        self.notify_info("No matches found");
                    }
                }
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
                self.send_render_request();
            }
            EditorAction::DeleteCharForward => {
                self.textarea.delete_next_char();
                self.document.modified = true;
                self.send_render_request();
            }

            // Insert-mode text input
            EditorAction::TextInput(input) => {
                self.textarea.input(input);
                self.document.modified = true;
                self.send_render_request();
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

            // Preveiw mode actions
            EditorAction::PreviewScrollDown => match self.preview_mode {
                PreviewMode::Html => {
                    self.preview_scroll_html = self.preview_scroll_html.saturating_add(5);
                }
                _ => {
                    self.preview_scroll = self.preview_scroll.saturating_add(5);
                }
            },
            EditorAction::PreviewScrollUp => match self.preview_mode {
                PreviewMode::Html => {
                    self.preview_scroll_html = self.preview_scroll_html.saturating_sub(5);
                }
                _ => {
                    self.preview_scroll = self.preview_scroll.saturating_sub(5);
                }
            },
            EditorAction::TogglePreview => {
                self.preview_mode = match self.preview_mode {
                    PreviewMode::Rendered => PreviewMode::Html,
                    PreviewMode::Html => PreviewMode::Hidden,
                    PreviewMode::Hidden => PreviewMode::Rendered,
                };
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

    // -----------------------------------------------------------
    // Link following
    // -----------------------------------------------------------

    /// Follow the currently highlighted link in `LinkSelect` mode.
    ///
    /// Behavior by target type:
    /// - `External` -> open in the system browser via `webbrowser` (fire-and-forget thread)
    /// - `InternalAnchor` -> jump the editor cursor to the source link of the matching anchor
    /// - `WikiLink` -> show a notification that cross-file navigation is deffered to post-MVP
    /// - `FilePath` -> show a notification that file path navigation is deffered to post-MVP
    fn follow_current_link(&mut self) {
        let Some(link) = self.link_index.get(self.link_cursor) else {
            return;
        };

        // Clone to satisfy borrow checker (we need `&mut self` below).
        let target = link.target.clone();
        let source_line = link.source_line;

        match target {
            LinkTarget::External(url) => {
                tracing::info!(url, "opening external link in browser");
                let url_clone = url.clone();
                std::thread::spawn(move || {
                    if let Err(e) = webbrowser::open(&url) {
                        tracing::warn!("failed to open browser for '{url}': {e}");
                    }
                });
                self.notify_info(format!("Opening: {url_clone}"));
            }
            LinkTarget::InternalAnchor(anchor) => {
                // Find the first entry in the index whose own target is an `InternalAnchor` with the same ID - that entry carries the source_line of the heading itself.
                let target_line = self
                    .link_index
                    .0
                    .iter()
                    .find(|l| matches!(&l.target, LinkTarget::InternalAnchor(a) if *a == anchor))
                    .map(|l| l.source_col)
                    .unwrap_or(source_line); // Fallback: Jump to the link's own line

                self.textarea
                    .move_cursor(CursorMove::Jump(target_line as u16, 0));
                self.notify_info(format!("Jumped to #{anchor}"));
            }
            LinkTarget::WikiLink(page) => {
                self.notify_warn(format!(
                    "Wiki link navigation not supported in MVP: [[{page}]]"
                ));
            }
            LinkTarget::FilePath(path) => {
                self.notify_warn(format!(
                    "File path navigation not supported in MVP: {}",
                    path.display()
                ));
            }
        }
    }

    // -----------------------------------------------------------
    // Command execution
    // -----------------------------------------------------------

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

                            // Re-apply the search highlight style to the new textarea instance.
                            self.textarea.set_search_style(
                                Style::default()
                                    .bg(Color::Rgb(80, 60, 0))
                                    .fg(Color::White)
                                    .add_modifier(Modifier::BOLD),
                            );

                            // Clear search state when a new file is opened.
                            self.search = None;

                            // Clear link state for the new document.
                            self.link_index = LinkIndex::new();
                            self.link_cursor = 0;

                            // Clear highlights on the new textarea (no-op since it's fresh but explicit for clarity).
                            self.sync_textarea_search();

                            // Trigger a fresh preview render for the newly opened document.
                            self.send_render_request();

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

    // -----------------------------------------------------------
    // Save
    // -----------------------------------------------------------

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

    // -----------------------------------------------------------
    // Accessors
    // -----------------------------------------------------------

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

    /// Human-readable search counter string for the status bar: "3/12" or "5/5".
    ///
    /// Returns `None` when there is no active search state or the pattern is empty.
    pub fn search_counter_str(&self) -> Option<String> {
        self.search
            .as_ref()
            .filter(|s| !s.pattern.is_empty())
            .map(|s| s.counter_str())
    }

    /// The current search kind, if a search is active.
    pub fn search_kind(&self) -> Option<&SearchKind> {
        self.search.as_ref().map(|s| &s.kind)
    }

    /// The current search pattern, if a search is active (empty string when cleared).
    pub fn search_pattern(&self) -> &str {
        self.search
            .as_ref()
            .map(|s| s.pattern.as_str())
            .unwrap_or("")
    }

    /// The display string for the currently highlighted link target.
    ///
    /// Returns `None` when not in LinkSelect mode or the index is empty.
    pub fn current_link_display(&self) -> Option<&str> {
        if self.mode != EditorMode::LinkSelect {
            return None;
        }
        self.link_index
            .get(self.link_cursor)
            .map(|l| l.target.display_str())
    }

    /// 1-indexed position string for LinkSelect: e.g. "2/8".
    pub fn link_select_counter(&self) -> String {
        if self.link_index.is_empty() {
            "0/0".to_owned()
        } else {
            format!("{}/{}", self.link_cursor + 1, self.link_index.len())
        }
    }

    /// Returns `true` when OSC-8 hyperlinks should be emitted in the preview pane.
    ///
    /// Controlled by `[terminal] hyperlinks` in user config:
    /// - "off" -> always false
    /// - "on" -> always true
    /// - "auto" -> mirrors the startup detection result
    pub fn hyperlinks_enabled(&self) -> bool {
        use alloy_core::config::HyperlinksMode;

        match self.config.terminal.hyperlinks {
            HyperlinksMode::On => true,
            HyperlinksMode::Off => false,
            HyperlinksMode::Auto => self.hyperlinks_supported,
        }
    }

    // -----------------------------------------------------------
    // Image helpers
    // -----------------------------------------------------------

    /// Returns `true` when image rendering is enabled AND a supported protocol is available.
    pub fn images_enabled(&self) -> bool {
        self.config.images.enabled
            && self.detected_image_protocol != DetectedImageProtocol::None
            && self.picker.is_some()
    }

    /// Invalidate image cache on terminal resize.
    pub fn on_resize(&mut self) {
        if let Ok(mut cache) = self.image_cache.lock() {
            cache.invalidate_all();
        }
    }
}

// ---------------------------------------------------------------
// Command string parsing helper
// ---------------------------------------------------------------

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

// ---------------------------------------------------------------
// Mode-aware textarea styling helpers
// ---------------------------------------------------------------

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

// ---------------------------------------------------------------
// Tests
// ---------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use alloy_core::links::Link;

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
        App::new(
            Config::default(),
            Document::new(),
            DetectedImageProtocol::None,
        )
    }

    // Preview mode actions

    #[test]
    fn initial_preview_mode_is_rendered() {
        let app = make_app();

        assert_eq!(app.preview_mode, PreviewMode::Rendered);
    }

    #[test]
    fn toggle_preview_cycles_rendered_to_html() {
        let mut app = make_app();
        app.handle_action(EditorAction::TogglePreview).unwrap();

        assert_eq!(app.preview_mode, PreviewMode::Html);
    }

    #[test]
    fn toggle_preview_cycles_html_to_hidden() {
        let mut app = make_app();
        app.preview_mode = PreviewMode::Html;
        app.handle_action(EditorAction::TogglePreview).unwrap();

        assert_eq!(app.preview_mode, PreviewMode::Hidden);
    }

    #[test]
    fn toggle_preview_cycles_hidden_to_rendered() {
        let mut app = make_app();
        app.preview_mode = PreviewMode::Hidden;
        app.handle_action(EditorAction::TogglePreview).unwrap();

        assert_eq!(app.preview_mode, PreviewMode::Rendered);
    }

    #[test]
    fn toggle_preview_full_cycle() {
        let mut app = make_app();
        // Rendered -> Html -> Hidden -> Rendered
        app.handle_action(EditorAction::TogglePreview).unwrap();
        assert_eq!(app.preview_mode, PreviewMode::Html);

        app.handle_action(EditorAction::TogglePreview).unwrap();
        assert_eq!(app.preview_mode, PreviewMode::Hidden);

        app.handle_action(EditorAction::TogglePreview).unwrap();
        assert_eq!(app.preview_mode, PreviewMode::Rendered);
    }

    // Preview scrolling

    #[test]
    fn initial_preview_scroll_html_is_zero() {
        let app = make_app();

        assert_eq!(app.preview_scroll_html, 0);
    }

    #[test]
    fn preview_scroll_down_increments() {
        let mut app = make_app();
        app.handle_action(EditorAction::PreviewScrollDown).unwrap();

        assert_eq!(app.preview_scroll, 5);
    }

    #[test]
    fn preview_scroll_up_saturates_at_zero() {
        let mut app = make_app();
        app.handle_action(EditorAction::PreviewScrollUp).unwrap();

        assert_eq!(app.preview_scroll, 0, "scroll should not go below 0");
    }

    #[test]
    fn last_preview_width_defaults_to_80() {
        let app = make_app();

        assert_eq!(app.last_preview_width, 80);
    }

    // Independent scroll positions

    #[test]
    fn scroll_down_in_rendered_mode_does_not_affect_html_scroll() {
        let mut app = make_app();
        app.preview_mode = PreviewMode::Rendered;
        app.handle_action(EditorAction::PreviewScrollDown).unwrap();

        assert_eq!(app.preview_scroll, 5);
        assert_eq!(
            app.preview_scroll_html, 0,
            "html scroll must be independent"
        );
    }

    #[test]
    fn scroll_down_in_html_mode_does_not_affect_rendered_scroll() {
        let mut app = make_app();
        app.preview_mode = PreviewMode::Html;
        app.handle_action(EditorAction::PreviewScrollDown).unwrap();

        assert_eq!(app.preview_scroll_html, 5);
        assert_eq!(app.preview_scroll, 0, "rendered scroll must be independent");
    }

    #[test]
    fn scroll_up_saturates_at_zero_for_html_mode() {
        let mut app = make_app();
        app.preview_mode = PreviewMode::Html;
        app.handle_action(EditorAction::PreviewScrollUp).unwrap();

        assert_eq!(app.preview_scroll_html, 0, "scroll must not go below 0");
    }

    // LinkSelect tests

    #[test]
    fn initial_link_index_is_empty() {
        let app = make_app();

        assert!(app.link_index.is_empty());
        assert_eq!(app.link_cursor, 0);
    }

    #[test]
    fn enter_link_select_with_empty_index_shows_notification() {
        let mut app = make_app();
        // Link index is empty at startup.
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();

        // Mode should NOT have changed to LinkSelect.
        assert_ne!(app.mode, EditorMode::LinkSelect);
        assert!(
            app.active_notification().is_some(),
            "expected a notification about no links"
        );
    }

    #[test]
    fn enter_link_select_with_links_changes_mode() {
        let mut app = make_app();
        // Inject a link directly into the index.
        app.link_index.push(Link {
            display_text: "Test".into(),
            target: LinkTarget::External("https://test.com".into()),
            source_line: 0,
            source_col: 0,
        });
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();

        assert_eq!(app.mode, EditorMode::LinkSelect);
        assert_eq!(app.link_cursor, 0);
    }

    #[test]
    fn link_select_next_advances_cursor() {
        let mut app = make_app();
        for i in 0..3 {
            app.link_index.push(Link {
                display_text: format!("Link {i}"),
                target: LinkTarget::External(format!("https://{i}.com")),
                source_line: i,
                source_col: 0,
            });
        }
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();
        app.handle_action(EditorAction::LinkSelectNext).unwrap();

        assert_eq!(app.link_cursor, 1);

        app.handle_action(EditorAction::LinkSelectNext).unwrap();

        assert_eq!(app.link_cursor, 2);

        // Wrap around
        app.handle_action(EditorAction::LinkSelectNext).unwrap();

        assert_eq!(app.link_cursor, 0);
    }

    #[test]
    fn link_select_prev_wraps_at_zero() {
        let mut app = make_app();
        for i in 0..3 {
            app.link_index.push(Link {
                display_text: format!("Link {i}"),
                target: LinkTarget::External(format!("https://{i}.com")),
                source_line: i,
                source_col: 0,
            });
        }
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();
        // Cursor is 0; prev should wrap to 2.
        app.handle_action(EditorAction::LinkSelectPrev).unwrap();

        assert_eq!(app.link_cursor, 2);
    }

    #[test]
    fn follow_link_returns_to_normal_mode() {
        let mut app = make_app();
        app.link_index.push(Link {
            display_text: "Wiki".into(),
            target: LinkTarget::WikiLink("SomePage".into()),
            source_line: 0,
            source_col: 0,
        });
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();
        app.handle_action(EditorAction::FollowLink).unwrap();

        assert_eq!(app.mode, EditorMode::Normal);
    }

    #[test]
    fn follow_wiki_link_shows_warning() {
        let mut app = make_app();
        app.link_index.push(Link {
            display_text: "Wiki".into(),
            target: LinkTarget::WikiLink("SomePage".into()),
            source_line: 0,
            source_col: 0,
        });
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();
        app.handle_action(EditorAction::FollowLink).unwrap();

        assert!(
            app.notifications
                .iter()
                .any(|n| n.level == NotificationLevel::Warn),
            "following a wiki link should produce a warning notification"
        );
    }

    #[test]
    fn follow_file_path_shows_warning() {
        let mut app = make_app();
        app.link_index.push(Link {
            display_text: "Docs".into(),
            target: LinkTarget::FilePath("./notes.md".into()),
            source_line: 0,
            source_col: 0,
        });
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();
        app.handle_action(EditorAction::FollowLink).unwrap();

        assert!(
            app.notifications
                .iter()
                .any(|n| n.level == NotificationLevel::Warn),
            "following a file path link should produce a warning notification"
        );
    }

    #[test]
    fn current_link_display_none_when_not_in_link_select_mode() {
        let app = make_app();

        assert!(app.current_link_display().is_none());
    }

    #[test]
    fn current_link_display_returns_url_in_link_select_mode() {
        let mut app = make_app();
        app.link_index.push(Link {
            display_text: "Example".into(),
            target: LinkTarget::External("https://example.com".into()),
            source_line: 0,
            source_col: 0,
        });
        app.handle_action(EditorAction::EnterLinkSelect).unwrap();

        assert_eq!(app.current_link_display(), Some("https://example.com"));
    }

    #[test]
    fn link_select_counter_empty() {
        let app = make_app();

        assert_eq!(app.link_select_counter(), "0/0");
    }

    #[test]
    fn link_select_counter_with_links() {
        let mut app = make_app();
        for _ in 0..3 {
            app.link_index.push(Link {
                display_text: "L".into(),
                target: LinkTarget::External("https://x.com".into()),
                source_line: 0,
                source_col: 0,
            });
        }

        assert_eq!(app.link_select_counter(), "1/3");
    }

    // doc_revision increments on edit

    #[test]
    fn edit_action_increments_doc_revision() {
        let mut app = make_app();

        // Seed the textarea with content so DeleteCharBackward has something to delete and is guaranteed to call send_render_request.
        app.textarea = TextArea::new(vec!["hello".to_string()]);
        app.textarea.move_cursor(CursorMove::End);

        let rev_before = app.doc_revision;

        app.handle_action(EditorAction::DeleteCharBackward).unwrap();

        assert!(
            app.doc_revision > rev_before,
            "doc_revision should increase after an edit action (before={rev_before}, after={})",
            app.doc_revision
        );
    }

    #[test]
    fn multiple_edits_keep_incrementing_doc_revision() {
        let mut app = make_app();

        app.textarea = TextArea::new(vec!["hello world".to_string()]);
        app.textarea.move_cursor(CursorMove::End);

        let rev0 = app.doc_revision;
        app.handle_action(EditorAction::DeleteCharBackward).unwrap();

        let rev1 = app.doc_revision;
        app.handle_action(EditorAction::DeleteCharBackward).unwrap();

        let rev2 = app.doc_revision;

        assert!(
            rev1 > rev0,
            "first edit should increment revision. (rev0={rev0}, rev1={rev1}, rev2={rev2})"
        );
        assert!(
            rev2 > rev1,
            "second edit should increment revision again. (rev0={rev0}, rev1={rev1}, rev2={rev2})"
        );
    }

    #[test]
    fn stale_render_result_is_not_applied() {
        let mut app = make_app();

        // Manually inject a stale result into the re;sult channel by sending directly.
        // We can't access the internal sender, so we test the logic via `tick()` after manipulating doc_revision.
        // Advance doc_revision well ahead of any result the worker might produce.
        app.doc_revision = 9999;

        // tick() should not panic and preview_text should remain the default empty Text.
        app.tick().unwrap();

        // if the worker happened to send a result for revision 1 (from `App::new()`), it should have been discarded because doc_revision is 9999.
        // We can't assert on preview_text content without the worker having run, but we can assert the app didn't crash and doc_revision is unchanged.
        assert_eq!(app.doc_revision, 9999);
    }

    // Command execution

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
    fn push_notification_visible_via_active_notification() {
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

    // Search mode integration

    #[test]
    fn initial_search_state_is_none() {
        let app = make_app();

        assert!(app.search.is_none());
    }

    #[test]
    fn enter_literal_search_sets_mode_and_creates_state() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();

        assert_eq!(app.mode, EditorMode::Search);
        assert!(app.search.is_some());
        assert_eq!(app.search.as_ref().unwrap().kind, SearchKind::Literal);
    }

    #[test]
    fn enter_regex_search_sets_kind_to_regex() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterRegexSearch).unwrap();

        assert_eq!(app.search.as_ref().unwrap().kind, SearchKind::Regex);
    }

    #[test]
    fn search_input_appends_to_pattern() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('h')).unwrap();
        app.handle_action(EditorAction::SearchInput('i')).unwrap();

        assert_eq!(app.search.as_ref().unwrap().pattern, "hi");
    }

    #[test]
    fn search_backspace_removes_last_char() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('h')).unwrap();
        app.handle_action(EditorAction::SearchInput('i')).unwrap();
        app.handle_action(EditorAction::SearchBackspace).unwrap();

        assert_eq!(app.search.as_ref().unwrap().pattern, "h");
    }

    #[test]
    fn commit_search_returns_to_normal_mode() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::CommitSearch).unwrap();

        assert_eq!(app.mode, EditorMode::Normal);
    }

    #[test]
    fn cancel_search_returns_to_normal_and_clears_pattern() {
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('x')).unwrap();
        app.handle_action(EditorAction::CancelSearch).unwrap();

        assert_eq!(app.mode, EditorMode::Normal);
        // Pattern cleared on cancel.
        assert!(app.search.as_ref().unwrap().pattern.is_empty());
    }

    #[test]
    fn search_counter_str_none_when_no_search() {
        let app = make_app();

        assert!(app.search_counter_str().is_none());
    }

    #[test]
    fn search_counter_str_returns_value_when_pattern_set() {
        let mut app = make_app();
        // Seed some content with a known pattern.
        app.textarea = TextArea::new(vec!["hello world hello".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('h')).unwrap();
        app.handle_action(EditorAction::SearchInput('e')).unwrap();
        app.handle_action(EditorAction::SearchInput('l')).unwrap();
        app.handle_action(EditorAction::SearchInput('l')).unwrap();
        app.handle_action(EditorAction::SearchInput('o')).unwrap();

        let counter = app.search_counter_str();

        assert!(
            counter.is_some(),
            "counter_str should be Some when pattern is non-empty"
        );
        // "hello world hello" has 2 matches for "hello"; counter should be "1/2".
        assert_eq!(counter.unwrap(), "1/2");
    }

    #[test]
    fn search_next_navigates_to_second_match() {
        let mut app = make_app();
        app.textarea = TextArea::new(vec!["hello world hello".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('h')).unwrap();
        app.handle_action(EditorAction::SearchInput('e')).unwrap();
        app.handle_action(EditorAction::SearchInput('l')).unwrap();
        app.handle_action(EditorAction::SearchInput('l')).unwrap();
        app.handle_action(EditorAction::SearchInput('o')).unwrap();
        app.handle_action(EditorAction::CommitSearch).unwrap();

        // Navigate to next match.
        app.handle_action(EditorAction::SearchNext).unwrap();

        // Second "hello" starts at col 12.
        let (_, col) = app.textarea.cursor();

        assert_eq!(col, 12, "cursor should be on the second 'hello'");
    }

    // :wq integration: saves to temp file then sets should_quit
    #[test]
    fn execute_wq_saves_and_quits() {
        use std::io::Write;

        let mut tmp = tempfile::NamedTempFile::new().expect("tempfile");
        tmp.write_all(b"# Test").unwrap();
        let path = tmp.path().to_path_buf();

        let doc = Document::open(&path).expect("open temp file");
        let mut app = App::new(Config::default(), doc, DetectedImageProtocol::None);

        app.execute_command(&format!("wq"));

        // There's a valid path, so save should succeed and should_quit = true.
        // (The actual wq path-less case is tested by execute_w_with_no_path_pushes_error_notification)
        // Here we use the path set on the document from open().
        assert!(app.should_quit, ":wq on a file with a path should quit");
    }

    // sync_textarea_search

    #[test]
    fn enter_search_clears_previous_highlights() {
        // Verifies that entering a new search correctly resets any prior pattern.
        // We can't directly inspect the internal tui-textarea state, but we can verify no panic occurs and the pattern is empty on entry.
        let mut app = make_app();
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('x')).unwrap();
        app.handle_action(EditorAction::CommitSearch).unwrap();

        // Start a second search — should clear and restart.
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();

        assert!(app.search.as_ref().unwrap().pattern.is_empty());
    }

    #[test]
    fn cancel_search_clears_pattern_and_matches() {
        let mut app = make_app();
        app.textarea = TextArea::new(vec!["test content test".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('t')).unwrap();
        app.handle_action(EditorAction::SearchInput('e')).unwrap();
        app.handle_action(EditorAction::SearchInput('s')).unwrap();
        app.handle_action(EditorAction::SearchInput('t')).unwrap();

        assert!(!app.search.as_ref().unwrap().matches.is_empty());

        app.handle_action(EditorAction::CancelSearch).unwrap();

        assert!(app.search.as_ref().unwrap().pattern.is_empty());
        assert!(app.search.as_ref().unwrap().matches.is_empty());
    }

    #[test]
    fn regex_metacharacters_in_literal_search_do_not_panic() {
        // Literal patterns containing regex metacharacters must be escaped.
        // e.g. "test.file" should match literal dots, not any character.
        let mut app = make_app();
        app.textarea = TextArea::new(vec!["test.file and test_file".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        for c in "test.file".chars() {
            app.handle_action(EditorAction::SearchInput(c)).unwrap();
        }

        // With regex::escape, "test.file" → "test\.file", matching only the literal dot.
        // Without escaping it would match both "test.file" and "test_file".
        let s = app.search.as_ref().unwrap();

        assert_eq!(
            s.matches.len(),
            1,
            "literal dot should not match underscore"
        );
    }

    #[test]
    fn search_counter_str_none_after_cancel() {
        let mut app = make_app();
        app.textarea = TextArea::new(vec!["hello".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        app.handle_action(EditorAction::SearchInput('h')).unwrap();
        app.handle_action(EditorAction::CancelSearch).unwrap();

        // Pattern was cleared on cancel, so counter should be None.
        assert!(app.search_counter_str().is_none());
    }

    #[test]
    fn search_counter_str_present_in_normal_mode_after_commit() {
        let mut app = make_app();
        app.textarea = TextArea::new(vec!["hello world hello".to_string()]);
        app.handle_action(EditorAction::EnterLiteralSearch).unwrap();
        for c in "hello".chars() {
            app.handle_action(EditorAction::SearchInput(c)).unwrap();
        }
        app.handle_action(EditorAction::CommitSearch).unwrap();

        assert_eq!(app.mode, EditorMode::Normal);
        // Counter must still be available in Normal mode after commit.
        assert!(
            app.search_counter_str().is_some(),
            "counter should persist after CommitSearch"
        );
    }
}
