// Document model - Chunk 1.2 stub
//
// This module holds the current in-memory representation of an open file.
// In Chunk 2.1, the `String` backing store will be replaced with a `ropey::Rope` and full undo/redo functionality will be added.
// For now, the goal is a clean public API that the rest of the app can depend on without needing to change call sites when the internals change.

use std::path::{Path, PathBuf};

use crate::errors::CoreError;

/// A document loaded (or created) in the editor.
///
/// Invariant: `content` is always valid UTF-8. Non-UTF-8 files are rejected at load time with `CoreError::DocumentEncoding`.
#[derive(Debug, Clone)]
pub struct Document {
    /// Raw text content.
    /// Will become `ropey::Rope` in Chunk 2.1.
    pub content: String,

    /// Path on disk, if the document originated from or has been saved to a file.
    /// `None` for new, unsaved buffers.
    pub path: Option<PathBuf>,

    /// `true` if the content has been modified since the last save.
    pub modified: bool,
}

impl Document {
    // Constructors

    /// Create a new, empty document with no associated file path.
    pub fn empty() -> Self {
        Self {
            content: String::new(),
            path: None,
            modified: false,
        }
    }

    /// Load a document from `path`.
    ///
    /// ## Errors
    /// - `CoreError::DocumentIo` - the file could not be read.
    /// - `CoreError::DocumentEncoding` - the file is not valid UTF-8.
    pub fn from_path(path: &Path) -> Result<Self, CoreError> {
        let bytes = std::fs::read(path).map_err(|source| CoreError::DocumentIo {
            path: path.to_owned(),
            source,
        })?;

        let content = String::from_utf8(bytes).map_err(|_| CoreError::DocumentEncoding {
            path: path.to_owned(),
        })?;

        Ok(Self {
            content,
            path: Some(path.to_owned()),
            modified: false,
        })
    }

    // Accessors

    /// The full text content of the document.
    pub fn as_str(&self) -> &str {
        &self.content
    }

    /// A human-readable name for display in the status bar.
    ///
    /// Returns the file name component of the path when available, or `"[No File]"` for unsaved buffers.
    pub fn display_name(&self) -> String {
        match &self.path {
            Some(p) => p
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| p.display().to_string()),
            None => "[No File]".into(),
        }
    }

    /// Total number of lines in the document. An empty document has 1 line.
    pub fn line_count(&self) -> usize {
        if self.content.is_empty() {
            return 1;
        }
        self.content.lines().count()
    }

    /// Total number of Unicode scalar values (chars) in the document.
    pub fn char_count(&self) -> usize {
        self.content.chars().count()
    }
}

// -----------------------------------------------------------------------
// Tests
// -----------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn tmp() -> TempDir {
        tempfile::tempdir().expect("failed to create temp dir")
    }

    #[test]
    fn empty_document_has_expected_defaults() {
        let doc = Document::empty();

        assert_eq!(doc.as_str(), "");
        assert_eq!(doc.display_name(), "[No File]");
        assert!(!doc.modified);
        assert!(doc.path.is_none());
        assert_eq!(doc.line_count(), 1);
    }

    #[test]
    fn from_path_reads_utf8_content() {
        let dir = tmp();
        let path = dir.path().join("test.md");
        fs::write(&path, b"# Hello\n\nWorld\n").unwrap();

        let doc = Document::from_path(&path).expect("should load");

        assert_eq!(doc.as_str(), "# Hello\n\nWorld\n");
        assert!(!doc.modified);
        assert_eq!(doc.display_name(), "test.md");
        assert_eq!(doc.line_count(), 3);
    }

    #[test]
    fn from_path_rejects_non_utf8() {
        let dir = tmp();
        let path = dir.path().join("bad.md");
        fs::write(&path, b"\xff\xfe invalid utf8").unwrap();

        let err = Document::from_path(&path).expect_err("should fail");

        assert!(matches!(err, CoreError::DocumentEncoding { .. }));
    }

    #[test]
    fn from_path_missing_file_returns_io_error() {
        let path = Path::new("/nonexistent/path/file.md");
        let err = Document::from_path(path).expect_err("should fail");

        assert!(matches!(err, CoreError::DocumentIo { .. }));
    }

    #[test]
    fn display_name_for_path_document() {
        let dir = tmp();
        let path = dir.path().join("notes.md");
        fs::write(&path, b"hello").unwrap();

        let doc = Document::from_path(&path).unwrap();
        assert_eq!(doc.display_name(), "notes.md");
    }

    #[test]
    fn char_counts_unicode_scalars() {
        let dir = tmp();
        let path = dir.path().join("unicode.md");
        // "café" = c, a, f, é  → 4 scalar values
        fs::write(&path, "café".as_bytes()).unwrap();

        let doc = Document::from_path(&path).unwrap();
        assert_eq!(doc.char_count(), 4);
    }
}
