// Document model - The persistence and metadata layer for an open document.
//
// # Architecture Notes
// `Document` is NOT the live editing buffer - that role belongs to `tui-textarea` in the app layer (Chunk 2.2).
// `Document` is responsible for:
// - Loading content from disk into a `Rope`
// - Writing content back to disk efficiently via the `Rope`'s chunk iterator
// - Tracking path and modified state
// - Providing read access to content for the preview renderer
//
// The `Rope` is kept in sync with disk, not with the live edit buffer.
// On save, the app layer passess the current textarea content as `&str`, which replaces the `Rope`'s content and is written to disk atomically.

use std::path::{Path, PathBuf};

use ropey::Rope;
use tracing::instrument;

use crate::errors::CoreError;

/// A document loaded (or created) in the editor.
///
/// Invariant: `content` is always valid UTF-8. Non-UTF-8 files are rejected at load time with `CoreError::DocumentEncoding`.
#[derive(Debug, Clone)]
pub struct Document {
    /// Internal rope - used for efficient file I/O and preview feeding.
    /// NOT the live edit buffer.
    rope: Rope,

    /// Path on disk, if the document originated from or has been saved to a file.
    /// `None` for new, unsaved buffers.
    pub path: Option<PathBuf>,

    /// `true` if the content has been modified since the last save (if the live edit buffer differs from the last saved content).
    pub modified: bool,
}

impl Document {
    /// Create a new, empty document with no associated file path.
    pub fn new() -> Self {
        Self {
            rope: Rope::new(),
            path: None,
            modified: false,
        }
    }

    /// Open a file from disk using the `path`.
    ///
    /// ## Errors:
    /// - `CoreError::DocumentIo` - error reading the file.
    /// - `CoreError::DocumentEncoding` - the file is not valid UTF-8.
    #[instrument(skip_all, fields(path = %path.as_ref().display()))]
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CoreError> {
        let path = path.as_ref();

        // ropey reads via a BufReader internally - no full allocation needed.
        let rope =
            Rope::from_reader(
                std::fs::File::open(path).map_err(|e| CoreError::DocumentIo {
                    path: path.to_owned(),
                    source: e,
                })?,
            )
            .map_err(|_| CoreError::DocumentEncoding {
                path: path.to_owned(),
            })?;

        tracing::debug!(lines = rope.len_lines(), "document opened");

        Ok(Self {
            rope,
            path: Some(path.to_owned()),
            modified: false,
        })
    }

    /// Save the current content string back to disk.
    ///
    /// The `content` parameter is whatever `tui-textarea` currently holds.
    /// This replaces the internal Rope and writes to disk.
    ///
    /// Returns `CoreError::DocumentNoPath` if no path is set - callers should use `save_as` in that case.
    #[instrument(skip(self, content), fields(path = ?self.path))]
    pub fn save(&mut self, content: &str) -> Result<(), CoreError> {
        let path = self.path.clone().ok_or(CoreError::DocumentNoPath)?;
        self.save_as(content, path)
    }

    /// Save the current content to an explicit path and update `self.path`.
    #[instrument(skip(self, content), fields(path = %path.as_ref().display()))]
    pub fn save_as(&mut self, content: &str, path: impl AsRef<Path>) -> Result<(), CoreError> {
        let path = path.as_ref();

        // Write via a BufWrite - ropey's chunk iterator feeds it without a full string copy.
        let file = std::fs::File::create(path).map_err(|e| CoreError::DocumentIo {
            path: path.to_owned(),
            source: e,
        })?;
        let mut writer = std::io::BufWriter::new(file);

        // Replace the internal rope with the content to be saved. This keeps the rope consistent with disk.
        self.rope = Rope::from_str(content);

        // Write each chunk from the rope to avoid one large allocation.
        use std::io::Write;
        for chunk in self.rope.chunks() {
            writer
                .write_all(chunk.as_bytes())
                .map_err(|e| CoreError::DocumentIo {
                    path: path.to_owned(),
                    source: e,
                })?;
        }

        self.path = Some(path.to_owned());
        self.modified = false;

        tracing::debug!(bytes = self.rope.len_bytes(), "document saved");
        Ok(())
    }

    // Accessors

    /// Return the full document content as a newly allocated `String`.
    ///
    /// used to see tui-textarea on file open and to feed the preview renderer.
    /// Avoid calling on every frame - cache in the caller.
    pub fn content(&self) -> String {
        self.rope.to_string()
    }

    /// Total number of lines in the document (ropey counts the final line even without a trailing newline so it matches user expectations).
    /// An empty document has 1 line.
    pub fn line_count(&self) -> usize {
        // ropey's len_lines() counts an empty trailing 'line' after a trailing newline. Subtract 1 in that case for a user-facing count.
        let n = self.rope.len_lines();
        if n > 0 && self.rope.len_bytes() > 0 {
            let last_char = self.rope.char(self.rope.len_chars().saturating_sub(1));
            if last_char == '\n' {
                n.saturating_sub(1)
            } else {
                n
            }
        } else {
            n
        }
    }

    /// Total number of Unicode scalar values (chars) in the document.
    pub fn char_count(&self) -> usize {
        self.rope.len_chars()
    }

    /// Total number of bytes in the document.
    pub fn byte_count(&self) -> usize {
        self.rope.len_bytes()
    }

    /// A human-readable name for display in the status bar.
    ///
    /// Returns the file name component of the path when available, or `"[No File]"` for unsaved buffers.
    pub fn display_name(&self) -> String {
        // match &self.path {
        //     Some(p) => p
        //         .file_name()
        //         .map(|n| n.to_string_lossy().into_owned())
        //         .unwrap_or_else(|| p.display().to_string()),
        //     None => "[No File]".into(),
        // }
        self.path
            .as_deref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "[Unnamed Document]".to_string())
    }
}

impl Default for Document {
    fn default() -> Self {
        Self::new()
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
        let doc = Document::new();

        assert_eq!(doc.content(), "".to_string());
        assert_eq!(doc.display_name(), "[Unnamed Document]".to_string());
        assert!(!doc.modified);
        assert!(doc.path.is_none());
        assert_eq!(doc.line_count(), 1);
    }

    #[test]
    fn from_path_reads_utf8_content() {
        let dir = tmp();
        let path = dir.path().join("test.md");
        fs::write(&path, b"# Hello\n\nWorld\n").unwrap();

        let doc = Document::open(&path).expect("should load");

        assert_eq!(doc.content(), "# Hello\n\nWorld\n".to_string());
        assert!(!doc.modified);
        assert_eq!(doc.display_name(), "test.md".to_string());
        assert_eq!(doc.line_count(), 3);
    }

    #[test]
    fn from_path_rejects_non_utf8() {
        let dir = tmp();
        let path = dir.path().join("bad.md");
        fs::write(&path, b"\xff\xfe invalid utf8").unwrap();

        let err = Document::open(&path).expect_err("should fail");

        assert!(matches!(err, CoreError::DocumentEncoding { .. }));
    }

    #[test]
    fn from_path_missing_file_returns_io_error() {
        let path = Path::new("/nonexistent/path/file.md");
        let err = Document::open(path).expect_err("should fail");

        assert!(matches!(err, CoreError::DocumentIo { .. }));
    }

    #[test]
    fn display_name_for_path_document() {
        let dir = tmp();
        let path = dir.path().join("notes.md");
        fs::write(&path, b"hello").unwrap();

        let doc = Document::open(&path).unwrap();
        assert_eq!(doc.display_name(), "notes.md".to_string());
    }

    #[test]
    fn char_counts_unicode_scalars() {
        let dir = tmp();
        let path = dir.path().join("unicode.md");
        // "café" = c, a, f, é  → 4 scalar values
        fs::write(&path, "café".as_bytes()).unwrap();

        let doc = Document::open(&path).unwrap();
        assert_eq!(doc.char_count(), 4);
    }
}
