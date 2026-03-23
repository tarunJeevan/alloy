pub mod config;
pub mod document;
pub mod errors;
pub mod modes;
pub mod search;

pub use document::Document;
pub use errors::CoreError;
pub use modes::EditorMode;
pub use search::{LARGE_DOC_THRESHOLD_BYTES, Match, SearchKind, SearchState};
