//! Terminal image protocol detection.
//!
//! Architecture:
//!
//! Detection has two phases, both called from `main.rs` BEFORE `enable_raw_mode`.
//! 1. FULL QUERY: sends escape sequences to the terminal and reads the response.
//!   - Gives the most accurate result.
//!   - Runs on a dedicated thread with a 200ms timeout guard for multiplexer environments (tmux, screen) where the response may never arrive.
//! 2. ENV-VAR FALLBACK: used when the full query times out or fails. Covers the most common modern terminals via `KITTY_WINDOW_ID`, `TERM_PROGRAM`, and `TERM`.
//!   - Exposed as `pub` so `main.rs` can call it directly on fallback.
//!   - The result is stored on `App` and is never mutated at runtime. Image cache entries are keyed by URL/path and remain valid across the session; they are invalidated on terminal resize.

// ------------------------------------------------------------
// Public types
// ------------------------------------------------------------

/// The image rendering protocol detected (or configured) at startup.
///
/// Stored on App and checked during preview rendering to decide whether to emit image widget output or a styled placeholder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DetectedImageProtocol {
    /// Kitty graphics protocol - supported by Kitty, WezTerm, Ghostty.
    Kitty,

    /// iTerm2 inline image protocol - supported by iTerm2 and WezTerm.
    Iterm2,

    /// SIXEL bitmap protocol - supported by foot, mlterm, and some xterm builds.
    Sixel,

    /// Unicode half-block fallback - works everywhere but lower quality.
    HalfBlock,

    /// Image rendering is disabled in user config.
    None,
}

impl DetectedImageProtocol {
    /// Returns `true` when a real graphics protocol is available (not halfblock or none).
    pub fn is_graphics_capable(&self) -> bool {
        matches!(self, Self::Kitty | Self::Iterm2 | Self::Sixel)
    }

    /// A short label for the first-run notification and status bar diagnostics.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Kitty => "Kitty",
            Self::Iterm2 => "iTerm2",
            Self::Sixel => "SIXEL",
            Self::HalfBlock => "HalfBlock (fallback)",
            Self::None => "disabled",
        }
    }
}

// ------------------------------------------------------------
// Environment variable heuristics
// ------------------------------------------------------------

/// Guess the supported protocol from well-known terminal env vars.
///
/// Priority order (from most specific to least):
/// 1. `KITTY_WINDOW_ID` - set exclusively by Kitty
/// 2. `TERM_PROGRAM` - set by many GUI terminals to identify themselves
/// 3. `TERM` - older/generic
/// 4. Default - `HalfBlock` (always works but lowest quality)
pub fn detect_from_env() -> DetectedImageProtocol {
    // 1. Kitty exclusive env var
    if std::env::var("KITTY_WINDOW_ID").is_ok() {
        tracing::debug!("image: KITTY_WINDOW_ID set -> Kitty protocol");
        return DetectedImageProtocol::Kitty;
    }

    // 2. TERM_PROGRAM - reliable on most modern terminals
    if let Ok(term_program) = std::env::var("TERM_PROGRAM") {
        tracing::debug!(term_program, "image, checking TERM_PROGRAM");
        match term_program.to_lowercase().as_str() {
            "iterm.app" => return DetectedImageProtocol::Iterm2,
            // WezTerm supports both Kitty and iTerm2 but prefer Kitty as it's more capable.
            "wezterm" | "ghostty" => return DetectedImageProtocol::Kitty,
            _ => {}
        }
    }

    // 3. TERM - legacy signal used by some SIXEL terminals
    if let Ok(term) = std::env::var("TERM") {
        tracing::debug!(term, "image: checking TERM");
        if term.contains("kitty") {
            return DetectedImageProtocol::Kitty;
        }

        // Some xterm/mlterm builds support SIXEL.
        if term == "mlterm" || term.starts_with("xterm-") {
            return DetectedImageProtocol::Sixel;
        }
    }

    // 4. Default fallback - unicode half-blocks
    tracing::debug!("image: no protocol detected via env vars; using HalfBlock fallback");
    DetectedImageProtocol::HalfBlock
}

// ------------------------------------------------------------
// Tests
// ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_graphics_capable_true_for_real_protocols() {
        assert!(DetectedImageProtocol::Kitty.is_graphics_capable());
        assert!(DetectedImageProtocol::Iterm2.is_graphics_capable());
        assert!(DetectedImageProtocol::Sixel.is_graphics_capable());
    }

    #[test]
    fn is_graphics_capable_false_for_fallback_and_none() {
        assert!(!DetectedImageProtocol::HalfBlock.is_graphics_capable());
        assert!(!DetectedImageProtocol::None.is_graphics_capable());
    }
}
