//! Editor modal state.
//!
//! `EditorMode` is owned by `App` in the `app` crate but it lives in `core` so that future crates (e.g. a language-server bridge) can depend on it without pulling in the full `app` crate.

/// The current editing mode of the editor surface.
///
/// The state machine is:
/// - Normal --(i)--> Insert --(Esc)--> Normal
/// - Normal --(:)--> Command --(Esc/Enter)--> Normal [Chunk 2.3]
/// - Normal --(/)--> Literal Search --(Esc/Enter)--> Normal [Phase 5]
/// - Normal --(?)--> Regex Search --(Esc/Enter)--> Normal [Phase 5]
/// - Normal --(fl)--> LinkSelect --(Esc)--> Normal [Phase 6]
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum EditorMode {
    /// Default mode. Navigation and action keys are active.
    /// Text input is NOT forwarded to the textarea.
    #[default]
    Normal,

    ///Text is being typed into the textarea.
    /// All printable keys (plus Backspace, Enter, etc.) are forwared to the `TextInput`.
    Insert,

    /// Incremental search is active.
    //* NOTE: Implemented in Phase 5
    Search,

    /// Link-selection overlay is active.
    //* Implemented in Phase 6.
    LinkSelect,

    /// The `:` command prompt is open.
    //* Implemented in Chunk 2.3.
    Command,
}

impl EditorMode {
    /// Short label used in the status bar, e.g., "NORMAL", "INSERT", "SEARCH", etc.
    pub fn label(&self) -> &'static str {
        match self {
            EditorMode::Command => "COMMAND",
            EditorMode::Insert => "INSERT",
            EditorMode::LinkSelect => "LINKS",
            EditorMode::Normal => "NORMAL",
            EditorMode::Search => "SEARCH",
        }
    }
}
