//! Keymap dispatcher - translates raw `crossterm` `KeyEvent`s into high-level `EditorAction`s, with support for multi-key Normal-mode sequences (e.g. `g g`).
//!
//! Design:
//! - Insert mode: Every key is forwarded as `EditorAction::TextInput`, except `Esc` which becomes `EditorAction::ExitInsert`. No buffering.
//! - Normal mode: Keys are accumulated in a pending buffer. After each key, the buffer is compared against the known sequence table. Three outcomes:
//!   1. Definite match - return the action and clear the buffer.
//!   2. Prefix match - a longer sequence starting with the current buffer exists; hold and wait for the next key (or timeout).
//!   3. No match - flush the buffer; dispatch the first pending key individually.
//! - Timeout: If `sequence_timeout_ms` elapses after the last key in Normal mode, the pending buffer is flushed as individual keys (each evaluated independently).
//! - Command mode: Keys are accumulated in a command buffer for processing and execution. Mappings are:
//!   1. Printable keys -> CommandInput(c)
//!   2. Backspace -> CommandBackspace
//!   3. Enter -> ExecuteCommand
//!   4. Esc -> ExitInsert (used to exit Command mode)
//! - Saerch mode: Similar to Command mode, but produces search-specific actions.
//!   1. Printable keys -> SearchInput(c)
//!   2. Backspace -> SearchBackspace
//!   3. Enter -> CommitSearch
//!   4. Esc -> CancelSearch
//!   5. Right / n -> SearchNext
//!   6. Left / N -> SearchPrev

use std::time::{Duration, Instant};

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use tui_textarea::Input;

use alloy_core::EditorMode;

// --------------------------------------------------------------------------
// EditorAction
// --------------------------------------------------------------------------

/// High-level editor actions, fully decoupled from ray key events.
///
/// The event loop matches on these variants to mutate `App` state and drive `tui-textarea`.
#[derive(Debug, Clone)]
pub enum EditorAction {
    // Mode transitions
    /// Normal / Command -> Insert
    EnterInsert,

    /// Insert -> Normal
    ExitInsert,

    /// Normal -> Command (`:` prompt)
    EnterCommand,

    /// Normal -> Search (Literal)
    EnterLiteralSearch,

    /// Normal -> Search (Regex),
    EnterRegexSearch,

    // Normal-mode motions
    MoveLeft,
    MoveRight,
    MoveDown,
    MoveUp,
    MoveWordForward,
    MoveWordBackward,
    MoveLineStart,
    MoveLineEnd,
    MoveDocStart,
    MoveDocEnd,

    // Normal-mode actions
    /// Delete the character before the cursor (equivalent to Backspace)
    DeleteCharBackward,

    /// Delete the character after the cursor (equivalent to Delete)
    DeleteCharForward,

    // Preview controls
    /// Scroll the preview pane down by a fixed number of lines
    PreviewScrollDown,

    /// Scroll the preview pane up by a fixed number of lines
    PreviewScrollUp,

    /// Cycle the preview mode (Rendered -> Hidden -> Rendered)
    TogglePreview,

    // Normal-mode app-level actions
    Save,
    Quit,

    // Sarch navigation (active after CommitSearch)
    /// Move to the next search match
    SearchNext,

    /// Move to the previous search match
    SearchPrev,

    // Insert-mode passthrough
    /// A key that should be forwarded verbatim to `tui-textarea::TextArea::input`
    TextInput(Input),

    // Command-mode actions
    /// Append a printable character to `App::command_input`
    CommandInput(char),

    /// Remove the last character from `App::command_input` (Backspace in Command mode)
    CommandBackspace,

    /// Execute the current contents of `App::command_input`
    ExecuteCommand,

    // Search-mode actions
    /// Append a character to the search pattern
    SearchInput(char),

    /// Remove the last character from the search pattern
    SearchBackspace,

    /// Commit the search and return to Normal mode
    CommitSearch,

    /// Cancel the search and restore the cursor to its pre-search position
    CancelSearch,

    // Catch all
    /// A key that has no bound action in the current mode. Silently ignored.
    Unbound,
}

// --------------------------------------------------------------------------
// Internal sequence table
// --------------------------------------------------------------------------

/// A compiled entry in the Normal-mode keymap.
#[derive(Debug)]
struct Binding {
    /// The key sequence that triggers this binding (1 or more keys).
    keys: Vec<NormalKey>,
    action: NormalAction,
}

/// A simplified key representation for Normal-mode matching.
/// We only need to distinguish a small set of keys in Normal-mode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NormalKey {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    CtrlS,
    CtrlW,
    CtrlF,
    CtrlB,
    Colon,
    Backspace,
    Delete,
    Slash,
    Question,
}

/// Variant-only action enum (no payloads) for the static binding table.
#[derive(Debug, Clone, Copy)]
enum NormalAction {
    EnterInsert,
    EnterCommand,
    EnterLiteralSearch,
    EnterRegexSearch,
    MoveLeft,
    MoveRight,
    MoveUp,
    MoveDown,
    MoveWordBackward,
    MoveWordForward,
    MoveLineEnd,
    MoveLineStart,
    MoveDocEnd,
    MoveDocStart,
    DeleteCharBackward,
    DeleteCharForward,
    PreviewScrollDown,
    PreviewScrollUp,
    TogglePreview,
    Save,
    Quit,
    SearchNext,
    SearchPrev,
}

impl From<NormalAction> for EditorAction {
    fn from(value: NormalAction) -> Self {
        match value {
            NormalAction::EnterInsert => EditorAction::EnterInsert,
            NormalAction::EnterCommand => EditorAction::EnterCommand,
            NormalAction::EnterLiteralSearch => EditorAction::EnterLiteralSearch,
            NormalAction::EnterRegexSearch => EditorAction::EnterRegexSearch,
            NormalAction::MoveLeft => EditorAction::MoveLeft,
            NormalAction::MoveRight => EditorAction::MoveRight,
            NormalAction::MoveUp => EditorAction::MoveUp,
            NormalAction::MoveDown => EditorAction::MoveDown,
            NormalAction::MoveWordBackward => EditorAction::MoveWordBackward,
            NormalAction::MoveWordForward => EditorAction::MoveWordForward,
            NormalAction::MoveLineEnd => EditorAction::MoveLineEnd,
            NormalAction::MoveLineStart => EditorAction::MoveLineStart,
            NormalAction::MoveDocEnd => EditorAction::MoveDocEnd,
            NormalAction::MoveDocStart => EditorAction::MoveDocStart,
            NormalAction::DeleteCharBackward => EditorAction::DeleteCharBackward,
            NormalAction::DeleteCharForward => EditorAction::DeleteCharForward,
            NormalAction::PreviewScrollDown => EditorAction::PreviewScrollDown,
            NormalAction::PreviewScrollUp => EditorAction::PreviewScrollUp,
            NormalAction::TogglePreview => EditorAction::TogglePreview,
            NormalAction::Save => EditorAction::Save,
            NormalAction::Quit => EditorAction::Quit,
            NormalAction::SearchNext => EditorAction::SearchNext,
            NormalAction::SearchPrev => EditorAction::SearchPrev,
        }
    }
}

fn default_normal_bindings() -> Vec<Binding> {
    use NormalAction::*;
    use NormalKey::*;

    vec![
        // Mode transitions
        Binding {
            keys: vec![Char('i')],
            action: EnterInsert,
        },
        Binding {
            keys: vec![Colon],
            action: EnterCommand,
        },
        Binding {
            keys: vec![Slash],
            action: EnterLiteralSearch,
        },
        Binding {
            keys: vec![Question],
            action: EnterRegexSearch,
        },
        // Arrow keys (always available regardless of hjkl preference)
        Binding {
            keys: vec![Left],
            action: MoveLeft,
        },
        Binding {
            keys: vec![Right],
            action: MoveRight,
        },
        Binding {
            keys: vec![Up],
            action: MoveUp,
        },
        Binding {
            keys: vec![Down],
            action: MoveDown,
        },
        // Vim-motion keys
        Binding {
            keys: vec![Char('h')],
            action: MoveLeft,
        },
        Binding {
            keys: vec![Char('l')],
            action: MoveRight,
        },
        Binding {
            keys: vec![Char('k')],
            action: MoveUp,
        },
        Binding {
            keys: vec![Char('j')],
            action: MoveDown,
        },
        Binding {
            keys: vec![Char('w')],
            action: MoveWordForward,
        },
        Binding {
            keys: vec![Char('b')],
            action: MoveWordBackward,
        },
        Binding {
            keys: vec![Char('0')],
            action: MoveLineStart,
        },
        Binding {
            keys: vec![Char('1')],
            action: MoveLineEnd,
        },
        // Two-key sequences
        Binding {
            keys: vec![Char('g'), Char('g')],
            action: MoveDocStart,
        },
        Binding {
            keys: vec![Char('g'), Char('e')],
            action: MoveDocEnd,
        },
        // Preview
        Binding {
            keys: vec![CtrlF],
            action: PreviewScrollDown,
        },
        Binding {
            keys: vec![CtrlB],
            action: PreviewScrollUp,
        },
        Binding {
            keys: vec![Char('t'), Char('p')],
            action: TogglePreview,
        },
        // Editing
        Binding {
            keys: vec![Backspace],
            action: DeleteCharBackward,
        },
        Binding {
            keys: vec![Delete],
            action: DeleteCharForward,
        },
        // App-level actions
        Binding {
            keys: vec![CtrlS],
            action: Save,
        },
        Binding {
            keys: vec![CtrlW],
            action: Quit,
        },
        // Search navigation (available in Normal mode after CommitSearch)
        Binding {
            keys: vec![Char('n')],
            action: SearchNext,
        },
        Binding {
            keys: vec![Char('N')],
            action: SearchPrev,
        },
    ]
}

/// Converts a crossterm `KeyEvent` to the simplified `NormalKey`.
///
/// Returns `None` for keys not handled in Normal-mode (they become `Unbound`).
fn to_normal_key(key: &KeyEvent) -> Option<NormalKey> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlS),
        (KeyCode::Char('w'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlW),
        (KeyCode::Char('f'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlF),
        (KeyCode::Char('b'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlB),
        (KeyCode::Char(':'), KeyModifiers::NONE) | (KeyCode::Char(':'), KeyModifiers::SHIFT) => {
            Some(NormalKey::Colon)
        }
        (KeyCode::Char('/'), KeyModifiers::NONE) => Some(NormalKey::Slash),
        (KeyCode::Char('?'), KeyModifiers::NONE) | (KeyCode::Char('?'), KeyModifiers::SHIFT) => {
            Some(NormalKey::Question)
        }
        (KeyCode::Char(c), KeyModifiers::NONE) | (KeyCode::Char(c), KeyModifiers::SHIFT) => {
            Some(NormalKey::Char(c))
        }
        (KeyCode::Left, KeyModifiers::NONE) => Some(NormalKey::Left),
        (KeyCode::Right, KeyModifiers::NONE) => Some(NormalKey::Right),
        (KeyCode::Up, KeyModifiers::NONE) => Some(NormalKey::Up),
        (KeyCode::Down, KeyModifiers::NONE) => Some(NormalKey::Down),
        (KeyCode::Backspace, KeyModifiers::NONE) => Some(NormalKey::Backspace),
        (KeyCode::Delete, KeyModifiers::NONE) => Some(NormalKey::Delete),
        _ => None,
    }
}

// --------------------------------------------------------------------------
// KeymapDispatcher
// --------------------------------------------------------------------------

/// Stateful dispatcher that maps raw key events to `EditorActions`s.
///
/// One dispatcher instance lives for the lifetime of the application.
/// Call `dispatch` for every key event from the crossterm event loop.
pub struct KeymapDispatcher {
    /// Keys accumulated while waiting to resolve a multi-key sequence.
    pub(crate) pending: Vec<NormalKey>,

    /// When the first key of the current pending sequence was pressed.
    pub(crate) last_key_at: Option<Instant>,

    /// How long to wait for a completing key before flushing the pending buffer.
    sequence_timeout: Duration,

    /// Compiled binding table for Normal mode.
    bindings: Vec<Binding>,
}

impl KeymapDispatcher {
    /// Construct a dispatcher with the given sequence timeout.
    pub fn new(sequence_timeout_ms: u64) -> Self {
        Self {
            pending: Vec::new(),
            last_key_at: None,
            sequence_timeout: Duration::from_millis(sequence_timeout_ms),
            bindings: default_normal_bindings(),
        }
    }

    /// Must be called on every event-loop tick BEFORE checking for new key events. Returns an action if a pending sequence has timed out.
    ///
    /// When a timeout fires, we flush the FIRST pending key as `Unbound` (it had no definite match) and keep the rest in the buffer for re-evaluation on the next tick.
    /// Callers should loop until this returns `None`.
    pub fn tick(&mut self) -> Option<EditorAction> {
        let timed_out = self
            .last_key_at
            .is_some_and(|t| t.elapsed() >= self.sequence_timeout);

        if timed_out && !self.pending.is_empty() {
            // Drop the first key (it never completed a sequence) and reset.
            self.pending.remove(0);
            self.last_key_at = if self.pending.is_empty() {
                None
            } else {
                Some(Instant::now())
            };

            // After flush we return Unbound; the caller will call tick() again to drain any remaining pending keys.
            return Some(EditorAction::Unbound);
        }

        None
    }

    /// Dispatch a raw key event given the current editor mode.
    ///
    /// Returns `Some(action)` if an action should be executed now or `None` if the key has been buffered (waiting for a sequence completion).
    pub fn dispatch(&mut self, key: KeyEvent, mode: &EditorMode) -> Option<EditorAction> {
        match mode {
            EditorMode::Insert => self.dispatch_insert(key),
            EditorMode::Normal => self.dispatch_normal(key),
            EditorMode::Command => self.dispatch_command(key),
            EditorMode::Search => self.dispatch_search(key),
            // NOTE: Other modes are stubs; treat like Normal-mode for now
            _ => self.dispatch_normal(key),
        }
    }

    /// Insert mode dispatch
    fn dispatch_insert(&self, key: KeyEvent) -> Option<EditorAction> {
        if key.code == KeyCode::Esc {
            return Some(EditorAction::ExitInsert);
        }
        // Forward everything else verbatim to tui-textarea.
        Some(EditorAction::TextInput(Input::from(key)))
    }

    /// Normal mode dispatch
    fn dispatch_normal(&mut self, key: KeyEvent) -> Option<EditorAction> {
        let nk = match to_normal_key(&key) {
            Some(k) => k,
            None => {
                // Key is not representable in our Normal-mode table; clear pending and return Unbound.
                self.pending.clear();
                self.last_key_at = None;
                return Some(EditorAction::Unbound);
            }
        };

        self.pending.push(nk);
        self.last_key_at = Some(Instant::now());
        self.evaluate_pending()
    }

    /// Command mode dispatch
    fn dispatch_command(&self, key: KeyEvent) -> Option<EditorAction> {
        match key.code {
            KeyCode::Esc => Some(EditorAction::ExitInsert),
            KeyCode::Enter => Some(EditorAction::ExecuteCommand),
            KeyCode::Backspace => Some(EditorAction::CommandBackspace),
            KeyCode::Char(c) => Some(EditorAction::CommandInput(c)),
            // All other keys (arrows, function keys, etc.) are ignored in Command mode.
            _ => Some(EditorAction::Unbound),
        }
    }

    /// Search mode dispatch.
    fn dispatch_search(&self, key: KeyEvent) -> Option<EditorAction> {
        match key.code {
            KeyCode::Esc => Some(EditorAction::CancelSearch),
            KeyCode::Enter => Some(EditorAction::CommitSearch),
            KeyCode::Backspace => Some(EditorAction::SearchBackspace),
            // Navigation within Search mode (same keys as Normal mode post-commit).
            KeyCode::Right => Some(EditorAction::SearchNext),
            KeyCode::Left => Some(EditorAction::SearchPrev),
            KeyCode::Char('n') if key.modifiers == KeyModifiers::NONE => {
                Some(EditorAction::SearchNext)
            }
            KeyCode::Char('N') | KeyCode::Char('n') if key.modifiers == KeyModifiers::SHIFT => {
                Some(EditorAction::SearchPrev)
            }
            // All other printable keys append to the pattern
            KeyCode::Char(c) => Some(EditorAction::SearchInput(c)),
            _ => Some(EditorAction::Unbound),
        }
    }

    /// Try to match the curent pending buffer against the binding table.
    fn evaluate_pending(&mut self) -> Option<EditorAction> {
        let pending = &self.pending;

        // Check for a definite match
        for binding in &self.bindings {
            if binding.keys == *pending {
                let action = EditorAction::from(binding.action);
                self.pending.clear();
                self.last_key_at = None;
                return Some(action);
            }
        }

        // Check whether any binding STARTS WITH the current pending buffer (i.e. the pending keys are a valid prefix of some longer sequence)
        let has_prefix_match = self
            .bindings
            .iter()
            .any(|b| b.keys.len() > pending.len() && b.keys.starts_with(pending));

        if has_prefix_match {
            // Wait for the next key (or timeout)
            return None;
        }

        // No match and no valid prefix - flush the first pending key as Unbound, keeping the rest for re-evaluation
        self.pending.clear();
        self.last_key_at = if self.pending.is_empty() {
            None
        } else {
            Some(Instant::now())
        };
        Some(EditorAction::Unbound)
    }
}

// --------------------------------------------------------------------------
// Tests
// --------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers};

    // Helpers

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent {
            code,
            modifiers: KeyModifiers::NONE,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn key_mod(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent {
            code,
            modifiers,
            kind: KeyEventKind::Press,
            state: KeyEventState::NONE,
        }
    }

    fn dispatcher() -> KeymapDispatcher {
        KeymapDispatcher::new(500)
    }

    // Normal-mode single-key

    #[test]
    fn normal_i_enters_insert() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('i')), &EditorMode::Normal),
            Some(EditorAction::EnterInsert)
        ));
    }

    #[test]
    fn normal_colon_enters_command() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char(':')), &EditorMode::Normal),
            Some(EditorAction::EnterCommand)
        ));
    }

    #[test]
    fn normal_slash_enters_literal_search() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('/')), &EditorMode::Normal),
            Some(EditorAction::EnterLiteralSearch)
        ));
    }

    #[test]
    fn normal_question_enters_regex_search() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('?')), &EditorMode::Normal),
            Some(EditorAction::EnterRegexSearch)
        ));
    }

    #[test]
    fn normal_n_searches_next() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('n')), &EditorMode::Normal),
            Some(EditorAction::SearchNext)
        ));
    }

    #[test]
    fn normal_shift_n_searches_prev() {
        let mut d = dispatcher();

        // `N` is Shift+n; crossterm delivers KeyCode::Char('N') with SHIFT modifier.
        assert!(matches!(
            d.dispatch(
                key_mod(KeyCode::Char('N'), KeyModifiers::SHIFT),
                &EditorMode::Normal
            ),
            Some(EditorAction::SearchPrev)
        ));
    }

    #[test]
    fn normal_hjkl_move_cursor() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('h')), &EditorMode::Normal),
            Some(EditorAction::MoveLeft)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Char('j')), &EditorMode::Normal),
            Some(EditorAction::MoveDown)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Char('k')), &EditorMode::Normal),
            Some(EditorAction::MoveUp)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Char('l')), &EditorMode::Normal),
            Some(EditorAction::MoveRight)
        ));
    }

    #[test]
    fn normal_arrow_keys_move_cursor() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Left), &EditorMode::Normal),
            Some(EditorAction::MoveLeft)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Right), &EditorMode::Normal),
            Some(EditorAction::MoveRight)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Up), &EditorMode::Normal),
            Some(EditorAction::MoveUp)
        ));
        assert!(matches!(
            d.dispatch(key(KeyCode::Down), &EditorMode::Normal),
            Some(EditorAction::MoveDown)
        ));
    }

    #[test]
    fn normal_g_alone_is_buffered() {
        let mut d = dispatcher();
        // Single `g` is a prefix for `g g` — should be buffered, not dispatched.
        let action = d.dispatch(key(KeyCode::Char('g')), &EditorMode::Normal);

        assert!(action.is_none(), "single 'g' should be buffered");
        assert_eq!(d.pending.len(), 1);
    }

    #[test]
    fn normal_gg_moves_to_doc_start() {
        let mut d = dispatcher();

        assert!(
            d.dispatch(key(KeyCode::Char('g')), &EditorMode::Normal)
                .is_none()
        );

        // Second half of the sequence `g g`
        let action = d.dispatch(key(KeyCode::Char('g')), &EditorMode::Normal);

        assert!(
            matches!(action, Some(EditorAction::MoveDocStart)),
            "g g should produce MoveDocStart"
        );
        assert!(d.pending.is_empty());
    }

    #[test]
    fn normal_ge_moves_to_doc_end() {
        let mut d = dispatcher();

        assert!(
            d.dispatch(key(KeyCode::Char('g')), &EditorMode::Normal)
                .is_none()
        );

        // Second half of the sequence `g e`
        let action = d.dispatch(
            key_mod(KeyCode::Char('e'), KeyModifiers::SHIFT),
            &EditorMode::Normal,
        );

        assert!(
            matches!(action, Some(EditorAction::MoveDocEnd)),
            "g e should produce MoveDocEnd"
        );
        assert!(d.pending.is_empty());
    }

    // Preview control tests

    #[test]
    fn ctrl_f_scrolls_preview_down() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(
                key_mod(KeyCode::Char('f'), KeyModifiers::CONTROL),
                &EditorMode::Normal,
            ),
            Some(EditorAction::PreviewScrollDown)
        ));
    }

    #[test]
    fn ctrl_b_scrolls_preview_down() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(
                key_mod(KeyCode::Char('b'), KeyModifiers::CONTROL),
                &EditorMode::Normal,
            ),
            Some(EditorAction::PreviewScrollUp)
        ));
    }

    #[test]
    fn t_alone_is_buffered() {
        let mut d = dispatcher();
        let action = d.dispatch(key(KeyCode::Char('t')), &EditorMode::Normal);

        assert!(
            action.is_none(),
            "'t' alone should be buffered (prefix of `t p`"
        );
    }

    #[test]
    fn tp_toggles_preview() {
        let mut d = dispatcher();

        assert!(
            d.dispatch(key(KeyCode::Char('t')), &EditorMode::Normal)
                .is_none()
        );
        assert!(matches!(
            d.dispatch(key(KeyCode::Char('p')), &EditorMode::Normal),
            Some(EditorAction::TogglePreview)
        ));
    }

    #[test]
    fn normal_ctrl_s_saves() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(
                key_mod(KeyCode::Char('s'), KeyModifiers::CONTROL),
                &EditorMode::Normal,
            ),
            Some(EditorAction::Save)
        ));
    }

    // Insert mode

    #[test]
    fn insert_regular_key_becomes_text_input() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('a')), &EditorMode::Insert),
            Some(EditorAction::TextInput(_))
        ));
    }

    #[test]
    fn insert_esc_exits_insert() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Esc), &EditorMode::Insert),
            Some(EditorAction::ExitInsert)
        ));
    }

    #[test]
    fn insert_does_not_buffer_keys() {
        // In Insert mode, `g` should immediately become TextInput, not be buffered.
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('g')), &EditorMode::Insert),
            Some(EditorAction::TextInput(_))
        ));
        assert!(d.pending.is_empty());
    }

    // Command mode

    #[test]
    fn command_printable_produces_command_input() {
        let mut d = dispatcher();

        assert!(
            matches!(
                d.dispatch(key(KeyCode::Char('w')), &EditorMode::Command),
                Some(EditorAction::CommandInput('w'))
            ),
            "printable key in Command mode should be CommandInput"
        );
    }

    #[test]
    fn command_enter_produces_execute_command() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Enter), &EditorMode::Command),
            Some(EditorAction::ExecuteCommand)
        ));
    }

    #[test]
    fn command_backspace_produces_command_backspace() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Backspace), &EditorMode::Command),
            Some(EditorAction::CommandBackspace)
        ));
    }

    #[test]
    fn command_esc_produces_exit_insert() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Esc), &EditorMode::Command),
            Some(EditorAction::ExitInsert)
        ));
    }

    #[test]
    fn command_space_is_command_input() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char(' ')), &EditorMode::Command),
            Some(EditorAction::CommandInput(' '))
        ));
    }

    // Search

    #[test]
    fn search_printable_produces_search_input() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('a')), &EditorMode::Search),
            Some(EditorAction::SearchInput('a'))
        ));
    }

    #[test]
    fn search_backspace_produces_search_backspace() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Backspace), &EditorMode::Search),
            Some(EditorAction::SearchBackspace)
        ));
    }

    #[test]
    fn search_enter_commits() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Enter), &EditorMode::Search),
            Some(EditorAction::CommitSearch)
        ));
    }

    #[test]
    fn search_esc_cancels() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Esc), &EditorMode::Search),
            Some(EditorAction::CancelSearch)
        ));
    }

    #[test]
    fn search_right_navigates_next() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Right), &EditorMode::Search),
            Some(EditorAction::SearchNext)
        ));
    }

    #[test]
    fn search_left_navigates_prev() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Left), &EditorMode::Search),
            Some(EditorAction::SearchPrev)
        ));
    }

    #[test]
    fn search_n_navigates_next() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('n')), &EditorMode::Search),
            Some(EditorAction::SearchNext)
        ));
    }

    #[test]
    fn search_shift_n_navigates_prev() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(
                key_mod(KeyCode::Char('N'), KeyModifiers::SHIFT),
                &EditorMode::Search
            ),
            Some(EditorAction::SearchPrev)
        ));
    }

    // Timeout

    #[test]
    fn sequence_timeout_flushes_pending() {
        // Create dispatcher with 0ms timeout timer so it expires immediately
        let mut d = KeymapDispatcher::new(0);

        // Buffer a 'g'.
        assert!(
            d.dispatch(key(KeyCode::Char('g')), &EditorMode::Normal)
                .is_none()
        );
        assert_eq!(d.pending.len(), 1);

        // A tiny sleep to ensure the 0ms timeout has elapsed.
        std::thread::sleep(Duration::from_millis(5));

        // tick() should now flush the pending key.
        let flushed = d.tick();

        assert!(matches!(flushed, Some(EditorAction::Unbound)));
        assert!(d.pending.is_empty());
    }
}
