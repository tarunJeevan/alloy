//! Search mode state machine for incremental in-document search.
//!
//! Architecture:
//!
//! - `SearchState` is owned by `App` and is `None` until the user first starts a search with `/` or `?`.
//! - Once created, it persists across mode transitions so `n`/`N` in Normal mode can navigate without re-entering Search mode.
//! - `SearchState` owns the incremental search pattern, the compiled match list, and navigation cursor.
//! - `App` drives it by calling `recompute` after each keystroke and `next_match`/`prev_match` on navigation.
//!
//! Match coordinates:
//!
//! - `Match::line` and `Match::col` are CHAR-INDEXED (not byte-indexed) so they map directly to `tui-textarea`'s `CursorMove::Jump(row, col)` API.
//! - `byte_start`/`byte-end` are retained for future highlight-range use (Chunk 5.2).
//!
//! Large document guard:
//!
//! - `recompute` accepts a `&str`. Callers should pass `textarea_content()`.
//! - For documents exceeding `LARGE_DOC_THRESHOLD_BYTES` the method still runs but the calling site in `app.rs` should debounce rather than calling on every keystroke.
//!
//! Regex caching:
//!
//! - `SearchState` caches the last compiled Regex as `Options<(String, Arc<Regex>)>`
//! - Using `Arc` keeps `SearchState: Clone` while avoiding recompilation on every keystroke.
//!
//! Case sensitivity:
//!
//! - Controlled by `case_insensitive: bool`, which mirrors `config.editor.search_case_insensitive`.
//! - For literal search, both the pattern and the haystack are lowercased before matching.
//! - For regex search, the `(?i)` flag is prepended to the pattern when insensitive.

use regex::Regex;
use tracing::warn;

pub const LARGE_DOC_THRESHOLD_BYTES: usize = 512 * 1024; // 512 KB

// ------------------------------------------------------------
// Public types
// ------------------------------------------------------------

/// A single match position within the document.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Match {
    /// 0-indexed line number (maps to `tui-textarea` row).
    pub line: usize,

    /// 0-indexed char offset within the line (maps to `tui-textarea` col).
    pub col: usize,

    /// Byte offset of the match start in the full document string.
    pub byte_start: usize,

    /// Byte offset of the match end (exclusive) in the full document string.
    pub byte_end: usize,
}

/// Which kind of search is active.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum SearchKind {
    /// Literal string match. Fast and uses `str::match_indices`.
    #[default]
    Literal,

    /// Regex match. Uses the `regex` crate (guaranteed linear time).
    Regex,
}

/// All mutable state for an in-progress (or committed) search.
///
/// Retained on `App` after `CommitSearch` so `n`/`N` in Normal mode can navigate. The pattern and matches are cleared on `CancelSearch`.
#[derive(Debug, Clone, Default)]
pub struct SearchState {
    /// The current search pattern string as the user typed it.
    pub pattern: String,

    /// Search mode (Literal or Regex).
    pub kind: SearchKind,

    /// All match positions in the document, sorted by byte offset.
    pub matches: Vec<Match>,

    /// Index into `matches` pointing at the 'current' match.
    /// Wraps around at both ends.
    pub current_match: usize,

    /// Whether to wrap around at document boundaries (always `tru` for MVP).
    pub wrap: bool,

    /// Mirrors `config.editor.search_case_insensitive`.
    pub case_insensitive: bool,

    /// Cached compiled regex so we don't recompile on every keystroke.
    /// Stores `(pattern_string, compiled_regex)`. Invalidated when `pattern` changes.
    ///
    /// Uses `Arc` so `SearchState` can be Cloned despite `Regex` not implementing `Clone`.
    regex_cache: Option<(String, Regex)>,
}

impl SearchState {
    /// Construct a new `SearchState` with an empty pattern.
    pub fn new(kind: SearchKind, case_insensitive: bool) -> Self {
        Self {
            kind,
            case_insensitive,
            wrap: true,
            ..Default::default()
        }
    }

    // Mutation

    /// Recompute `self.matches` from `self.pattern` against `text`.
    ///
    /// - If `pattern` is empty, `matches` is cleared and `current_match` is reset to 0.
    /// - After recomputing, clamps `current_match` to the valid range (new `matches` length).
    /// - Callers should debounce this for documents larger than `LARGE_DOC_THRESHOLD_BYTES`.
    pub fn recompute(&mut self, text: &str) {
        if self.pattern.is_empty() {
            self.matches.clear();
            self.current_match = 0;
            return;
        }

        self.matches = match self.kind {
            SearchKind::Literal => self.compute_literal(text),
            SearchKind::Regex => self.compute_regex(text),
        };

        // Clamp current_match into the valid range.
        if self.matches.is_empty() {
            self.current_match = 0;
        } else {
            self.current_match = self.current_match.min(self.matches.len() - 1);
        }
    }

    // Navigation

    /// Advance to the next match (with wrap-around) and return a reference to it.
    ///
    /// Returns `None` if there are no matches.
    pub fn next_match(&mut self) -> Option<&Match> {
        if self.matches.is_empty() {
            return None;
        }
        if self.wrap || self.current_match + 1 < self.matches.len() {
            self.current_match = (self.current_match + 1) % self.matches.len();
        }
        self.matches.get(self.current_match)
    }

    /// Move to the preview match (with wrap-around) and return a reference to it.
    ///
    /// Returns `None` if there are no matches.
    pub fn prev_match(&mut self) -> Option<&Match> {
        if self.matches.is_empty() {
            return None;
        }
        if self.current_match == 0 {
            if self.wrap {
                self.current_match = self.matches.len() - 1;
            }
        } else {
            self.current_match -= 1;
        }
        self.matches.get(self.current_match)
    }

    /// Return the current match without advancing the index.
    pub fn current(&self) -> Option<&Match> {
        self.matches.get(self.current_match)
    }

    /// `true` when there are no matches for the current pattern.
    pub fn is_empty(&self) -> bool {
        self.matches.is_empty()
    }

    /// Total match count. Convenience wrapper used by the status bar.
    pub fn match_count(&self) -> usize {
        self.matches.len()
    }

    /// Human-readable match counter string: "3/12" or "0/0".
    pub fn counter_str(&self) -> String {
        if self.matches.is_empty() {
            "0/0".to_owned()
        } else {
            format!("{}/{}", self.current_match + 1, self.matches.len())
        }
    }

    // Internal helpers

    /// Compute matches for a literal search pattern.
    fn compute_literal(&self, text: &str) -> Vec<Match> {
        if self.case_insensitive {
            // Case-fold both sides.
            // NOTE: This is not full Unicode case-folding but handles ASCII + common European chars.
            // NOTE: This allocates but is acceptable for the MVP
            let haystack_lower = text.to_lowercase();
            let needle_lower = self.pattern.to_lowercase();

            collect_matches(text, &haystack_lower, &needle_lower)
        } else {
            collect_matches(text, text, &self.pattern)
        }
    }

    /// Compile (or reuse cached) regex and collect matches.
    fn compute_regex(&mut self, text: &str) -> Vec<Match> {
        // Build the effective pattern string (with (?i) prefix if case-insensitive).
        let pattern_str = if self.case_insensitive {
            format!("(?i){}", self.pattern)
        } else {
            self.pattern.clone()
        };

        // Check cache validity. Reuse if pattern hasn't changed.
        let need_recompile = self
            .regex_cache
            .as_ref()
            .map(|(p, _)| p != &pattern_str)
            .unwrap_or(true);

        if need_recompile {
            match Regex::new(&pattern_str) {
                Ok(re) => {
                    self.regex_cache = Some((pattern_str, re));
                }
                Err(e) => {
                    warn!("search: invalid regex '{}': {e}", self.pattern);
                    self.regex_cache = None;
                    return Vec::new();
                }
            }
        }

        let re = match &self.regex_cache {
            Some((_, re)) => re,
            None => return Vec::new(),
        };

        // Use `find_iter` to collect all non-overlapping matches.
        let mut matches = Vec::new();
        for m in re.find_iter(text) {
            if let Some(mat) = byte_offset_to_match(text, m.start(), m.end()) {
                matches.push(mat);
            }
        }
        matches
    }
}

// ------------------------------------------------------------
// Helpers
// ------------------------------------------------------------

/// Collect literal matches of `needle` inside `haystack`, annotated with the ORIGINAL text's byte offsets.
///
/// `haystack` and `text` have the same byte layout - `haystack` may be a case-folded copy.
/// We use `match_indices` on `haystack` but report byte offsets that are valid in `text`.
fn collect_matches(text: &str, haystack: &str, needle: &str) -> Vec<Match> {
    if needle.is_empty() {
        return Vec::new();
    }

    let mut matches = Vec::new();

    for (byte_start, _) in haystack.match_indices(needle) {
        let byte_end = byte_start + needle.len();
        if let Some(mat) = byte_offset_to_match(text, byte_start, byte_end) {
            matches.push(mat);
        }
    }

    matches
}

/// Convert byte offsets in `text` to a `Match` with char-indexed `line` and `col`.
///
/// Returns `None` only if `byte_start` is out of range of `text` (should never happen with correct callers, but guarded defensively).
fn byte_offset_to_match(text: &str, byte_start: usize, byte_end: usize) -> Option<Match> {
    if byte_start > text.len() || byte_end > text.len() {
        warn!(
            "search: byte offset {}..{} out of bounds for text len {}",
            byte_start,
            byte_end,
            text.len()
        );
        return None;
    }

    // Count newlines before `byte_start` to determine the line number.
    let before = &text[..byte_start];
    let line = before.chars().filter(|&c| c == '\n').count();

    // Column = char offset from the start of the line.
    let line_start_byte = before.rfind('\n').map(|i| i + 1).unwrap_or(0);
    let col = text[line_start_byte..byte_start].chars().count();

    Some(Match {
        line,
        col,
        byte_start,
        byte_end,
    })
}

// ------------------------------------------------------------
// Tests
// ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // Helpers

    fn literal_state(pattern: &str, case_insensitive: bool) -> SearchState {
        let mut s = SearchState::new(SearchKind::Literal, case_insensitive);
        s.pattern = pattern.to_owned();

        s
    }

    fn regex_state(pattern: &str, case_insensitive: bool) -> SearchState {
        let mut s = SearchState::new(SearchKind::Regex, case_insensitive);
        s.pattern = pattern.to_owned();

        s
    }

    // recompute (basic correctness)

    #[test]
    fn empty_pattern_produces_no_matches() {
        let mut s = SearchState::new(SearchKind::Literal, true);
        s.recompute("hello world");

        assert!(s.matches.is_empty());
        assert_eq!(s.current_match, 0);
    }

    #[test]
    fn literal_single_match() {
        let mut s = literal_state("world", false);
        s.recompute("hello world");

        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.matches[0].line, 0);
        assert_eq!(s.matches[0].col, 6);
        assert_eq!(s.matches[0].byte_start, 6);
    }

    #[test]
    fn literal_multiple_matches_on_same_line() {
        let mut s = literal_state("ab", false);
        s.recompute("ab cd ab ef ab");

        assert_eq!(s.matches.len(), 3);
        assert_eq!(s.matches[0].col, 0);
        assert_eq!(s.matches[1].col, 6);
        assert_eq!(s.matches[2].col, 12);
    }

    #[test]
    fn literal_multiline_match() {
        let text = "line one\nline two\nline three";
        let mut s = literal_state("line", false);
        s.recompute(text);

        assert_eq!(s.matches.len(), 3);
        assert_eq!(s.matches[0].line, 0);
        assert_eq!(s.matches[1].line, 1);
        assert_eq!(s.matches[2].line, 2);

        // All start at col 0 on their respective lines.
        for m in &s.matches {
            assert_eq!(m.col, 0);
        }
    }

    #[test]
    fn literal_case_insensitive_finds_mixed_case() {
        let mut s = literal_state("HELLO", true);
        s.recompute("hello world HELLO");

        assert_eq!(s.matches.len(), 2, "case-insensitive should find both");
    }

    #[test]
    fn literal_case_sensitive_only_finds_exact() {
        let mut s = literal_state("HELLO", false);
        s.recompute("hello world HELLO");

        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.matches[0].col, 12);
    }

    #[test]
    fn no_matches_returns_empty_vec() {
        let mut s = literal_state("xyz", false);
        s.recompute("hello world");

        assert!(s.matches.is_empty());
    }

    // recompute - regex

    #[test]
    fn regex_word_boundary_match() {
        let mut s = regex_state(r"\bword\b", false);
        s.recompute("a word in a sentence, not swordfish");

        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.matches[0].col, 2);
    }

    #[test]
    fn regex_case_insensitive_prefix() {
        let mut s = regex_state("hello", true);
        s.recompute("Hello HELLO hello");

        assert_eq!(s.matches.len(), 3);
    }

    #[test]
    fn invalid_regex_produces_no_matches_no_panic() {
        let mut s = regex_state("[[[", false);
        s.recompute("hello world");

        assert!(
            s.matches.is_empty(),
            "invalid regex should produce no matches, not panic"
        );
    }

    // Navigation

    #[test]
    fn next_match_advances_with_wrap() {
        let mut s = literal_state("a", false);
        s.recompute("a b a c a");

        assert_eq!(s.matches.len(), 3);

        // Start at 0; next -> 1.
        let m = s.next_match().unwrap();

        assert_eq!(m.col, 4);
        assert_eq!(s.current_match, 1);

        // next -> 2.
        s.next_match();

        assert_eq!(s.current_match, 2);

        // next wraps -> 0.
        s.next_match();

        assert_eq!(s.current_match, 0);
    }

    #[test]
    fn prev_match_decrements_with_wrap() {
        let mut s = literal_state("a", false);
        s.recompute("a b a c a");

        // At 0; prev wraps -> 2.
        s.prev_match();

        assert_eq!(s.current_match, 2);

        // prev -> 1.
        s.prev_match();

        assert_eq!(s.current_match, 1);
    }

    #[test]
    fn navigation_no_op_when_no_matches() {
        let mut s = literal_state("xyz", false);
        s.recompute("hello world");

        assert!(s.next_match().is_none());
        assert!(s.prev_match().is_none());
        assert_eq!(s.current_match, 0);
    }

    #[test]
    fn navigation_single_match_stays_at_zero() {
        let mut s = literal_state("world", false);
        s.recompute("hello world");

        // next wraps back to 0.
        s.next_match();

        assert_eq!(s.current_match, 0);

        // prev also stays at 0.
        s.prev_match();

        assert_eq!(s.current_match, 0);
    }

    #[test]
    fn current_returns_match_at_cursor() {
        let mut s = literal_state("a", false);
        s.recompute("a b a");

        assert_eq!(s.current().unwrap().col, 0);

        s.next_match();

        assert_eq!(s.current().unwrap().col, 4);
    }

    // counter_str

    #[test]
    fn counter_str_no_matches() {
        let s = SearchState::new(SearchKind::Literal, true);

        assert_eq!(s.counter_str(), "0/0");
    }

    #[test]
    fn counter_str_with_matches() {
        let mut s = literal_state("a", false);
        s.recompute("a b a c a");

        // current_match starts at 0 -> "1/3".
        assert_eq!(s.counter_str(), "1/3");

        s.next_match();

        assert_eq!(s.counter_str(), "2/3");
    }

    // current_match clamping after recompute

    #[test]
    fn current_match_clamped_after_fewer_matches() {
        let mut s = literal_state("a", false);
        // 3 matches; navigate to last.
        s.recompute("a b a c a");
        s.current_match = 2;

        // Now recompute with a pattern that only finds 1 match.
        s.pattern = "b".to_owned();
        s.recompute("a b a c a");

        assert_eq!(s.matches.len(), 1);
        assert_eq!(s.current_match, 0, "current_match must be clamped");
    }

    // Unicode correctness

    #[test]
    fn col_is_char_indexed_not_byte_indexed() {
        // "café" is 5 bytes (c=1, a=1, f=1, é=2) but 4 chars.
        let text = "café world";
        let mut s = literal_state("world", false);
        s.recompute(text);

        assert_eq!(s.matches.len(), 1);
        // "world" starts at char index 5 (c,a,f,é,space = 5 chars).
        assert_eq!(s.matches[0].col, 5, "col should be char-indexed");
    }

    #[test]
    fn multiline_col_reset_per_line() {
        let text = "abc\nxyz abc";
        let mut s = literal_state("abc", false);
        s.recompute(text);

        assert_eq!(s.matches.len(), 2);
        assert_eq!(s.matches[0].line, 0);
        assert_eq!(s.matches[0].col, 0);
        assert_eq!(s.matches[1].line, 1);
        assert_eq!(s.matches[1].col, 4); // "xyz " = 4 chars
    }
}
