//! Keymap dispatcher - translates raw `crossterm` `KeyEvent`s into high-level `EditorAction`s, with support for multi-key Normal-mode sequences (e.g. `g g`).
//!
//! Design:
//! - Insert mode: Every key is forwarded as `EditorAction::TextInput`, except `Esc` which becomes `EditorAction::ExitInsert`. No buffering.
//! - Normal mode: Keys are accumulated in a pending buffer. After each key, the buffer is compared against the known sequence table. Three outcomes:
//!   1. Definite match - return the action and clear the buffer.
//!   2. Prefix match - a longer sequence starting with the current buffer exists; hold and wait for the next key (or timeout).
//!   3. No match - flush the buffer; dispatch the first pending key individually.
//! - Timeout: If `sequence_timeout_ms` elapses after the last key, the pending buffer is flushed as individual keys (each evaluated independently).

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
    /// Normal -> Insert
    EnterInsert,

    /// Insert -> Normal
    ExitInsert,

    /// Normal -> Command (`:` prompt)
    //* Implemented in Chunk 2.3.
    EnterCommand,

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

    // Normal-mode app-level actions
    Save,
    Quit,

    // Insert-mode passthrough
    /// A key that should be forwarded verbatim to `tui-textarea::TextArea::input`.
    TextInput(Input),

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
enum NormalKey {
    Char(char),
    Left,
    Right,
    Up,
    Down,
    CtrlS,
    CtrlW,
    Colon,
    Backspace,
    Delete,
}

/// Variant-only action enum (no payloads) for the static binding table.
#[derive(Debug, Clone, Copy)]
enum NormalAction {
    EnterInsert,
    EnterCommand,
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
    Save,
    Quit,
}

impl From<NormalAction> for EditorAction {
    fn from(value: NormalAction) -> Self {
        match value {
            NormalAction::EnterInsert => EditorAction::EnterInsert,
            NormalAction::EnterCommand => EditorAction::EnterCommand,
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
            NormalAction::Save => EditorAction::Save,
            NormalAction::Quit => EditorAction::Quit,
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
            keys: vec![Char('9')],
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
        // Editing
        Binding {
            keys: vec![Char('x')],
            action: DeleteCharForward,
        },
        Binding {
            keys: vec![Char('X')],
            action: DeleteCharBackward,
        },
        Binding {
            keys: vec![Backspace],
            action: DeleteCharBackward,
        },
        Binding {
            keys: vec![Delete],
            action: DeleteCharForward,
        },
        Binding {
            keys: vec![CtrlS],
            action: Save,
        },
        Binding {
            keys: vec![CtrlW],
            action: Quit,
        },
        Binding {
            keys: vec![Char('q')],
            action: Quit,
        },
    ]
}

/// Convert a crossterm `KeyEvent` to our simplified `NormalKey`, returning `None` for keys we don't handle in Normal-mode (they become `Unbound`).
fn to_normal_key(key: &KeyEvent) -> Option<NormalKey> {
    match (key.code, key.modifiers) {
        (KeyCode::Char('s'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlS),
        (KeyCode::Char('c'), KeyModifiers::CONTROL) => Some(NormalKey::CtrlW),
        (KeyCode::Char(':'), KeyModifiers::NONE) | (KeyCode::Char(':'), KeyModifiers::SHIFT) => {
            Some(NormalKey::Colon)
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

/// Stateful dispatched that maps raw key events to `EditorActions`s.
///
/// One dispatcher instance lives for the lifetime of the application.
/// Call `dispatch` for every key event from the crossterm event loop.
pub struct KeymapDispatcher {
    /// Keys accumulated while waiting to resolve a multi-key sequence.
    pending: Vec<NormalKey>,

    /// When the first key of the current pending sequence was pressed.
    last_key_at: Option<Instant>,

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
            //* Other modes are stubs; treat like Normal for now
            _ => self.dispatch_normal(key),
        }
    }

    // Insert mode dispatch
    fn dispatch_insert(&self, key: KeyEvent) -> Option<EditorAction> {
        if key.code == KeyCode::Esc {
            return Some(EditorAction::ExitInsert);
        }
        // Forward everything else verbatim to tui-textarea.
        Some(EditorAction::TextInput(Input::from(key)))
    }

    // Normal mode dispatch
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
    fn normal_g_alone_returns_none_pending() {
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

    #[test]
    fn normal_q_quits() {
        let mut d = dispatcher();

        assert!(matches!(
            d.dispatch(key(KeyCode::Char('q')), &EditorMode::Normal),
            Some(EditorAction::Quit)
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
