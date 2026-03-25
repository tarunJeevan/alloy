pub mod config;
pub mod document;
pub mod errors;
pub mod links;
pub mod modes;
pub mod search;

pub use document::Document;
pub use errors::CoreError;
pub use links::{Link, LinkIndex, LinkTarget, normalize_anchor};
pub use modes::EditorMode;
pub use search::{LARGE_DOC_THRESHOLD_BYTES, Match, SearchKind, SearchState};
