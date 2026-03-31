//! Syntax highlighting for fenced code blocks via `syntect`.
//!
//! Architecture:
//!
//! - `Highlighter` owns the loaded `SyntaxSet` and `ThemeSet`, both of which are expensive to construct (~5-20ms, depending on hardware) and immutable once built.
//! - Construct once at worker spawn time and share via `Arc`.
//!
//! Color mapping:
//!
//! - syntect produces `StyleRange` values with `syntect::highlighting::Color` (RGBA) and `FontStyle` (bitflags).
//! - We map these directly to `ratatui::style::Color::Rgb` and `ratatui::style::Modifier` without any ANSI intermediate, avoiding the `ansi-to-tui` dependency entirely.
//!
//! Theme fallback:
//!
//! - If the user-configured theme name is not found, we silently fall back to "base16-ocean.dark", which is always present in syntect's bundled defaults.
//! - Never panic on a missing theme.
//!
//! Language fallback:
//!
//! - If no langauge tag is provided or the tag doesn't match any known syntax, render the block with a FallbackStyle.
//! - The code content is still shown - we never discard it.
//!
//! Thread safety:
//!
//! - `Highlighter` is `Send + Sync` because `SyntaxSet` and `ThemeSet` are both `Send + Sync`.
//! - An `Arc<Highlighter>` is safe to share with the preview worker thread.

use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use syntect::{
    easy::HighlightLines,
    highlighting::{FontStyle, Style as SyntectStyle, ThemeSet},
    parsing::SyntaxSet,
    util::LinesWithEndings,
};

use alloy_core::config::FallbackStyle;

// -----------------------------------------------------------
// Default theme constant
// -----------------------------------------------------------

const FALLBACK_THEME: &str = "base16-ocean.dark";

// -----------------------------------------------------------
// Highlighter
// -----------------------------------------------------------

/// Owns the bundled syntect syntax and theme sets.
///
/// Construct once via `Highlighter::load_defaults` and share via `Arc`.
pub struct Highlighter {
    syntax_set: SyntaxSet,
    theme_set: ThemeSet,
}

impl Highlighter {
    /// Load the bundled default syntax and theme sets.
    pub fn load_defaults() -> Self {
        Self {
            syntax_set: SyntaxSet::load_defaults_newlines(),
            theme_set: ThemeSet::load_defaults(),
        }
    }

    /// Highlight `code` for the given `lang` tag using `theme_name`.
    ///
    /// Returns a `Vec<Line<'static>>` ready to extend a `Text<'static>`.
    /// Each source line in `code` procudes exactly one `Line`.
    /// Styled spans within the line correspond to syntect token ranges.
    ///
    /// Fallback behavior:
    ///
    /// - Unknown theme name -> use FALLBACK_THEME silently.
    /// - `lang` is `None` or unknown -> render all tokens with `fallback_style`.
    /// - Empty `code` -> returns an empty `Vec`.
    pub fn highlight_block(
        &self,
        code: &str,
        lang: Option<&str>,
        theme_name: &str,
        fallback_style: &FallbackStyle,
    ) -> Vec<Line<'static>> {
        if code.is_empty() {
            return Vec::new();
        }

        // Resolve theme - fall back silently if unknown.
        let theme = self
            .theme_set
            .themes
            .get(theme_name)
            .or_else(|| self.theme_set.themes.get(FALLBACK_THEME))
            .expect("base16-ocean.dark must always be present in syntect defaults");

        // Resolve syntax for the language tag.
        let syntax_ref = lang.and_then(|l| {
            self.syntax_set.find_syntax_by_token(l)
            // Some language tags use the full name ("python") and some use extensions ("py").
            // `find_syntax_by_token` checks both.
        });

        match syntax_ref {
            Some(syntax) => {
                // Happy path - perform full token-level highlighting.
                let mut highlighter = HighlightLines::new(syntax, theme);
                self.render_highlighted(code, &mut highlighter)
            }
            None => {
                // Unknown / empty language - render plain with fallback style.
                self.render_plain(code, fallback_style)
            }
        }
    }

    // Internal helpers

    /// Render code with full syntect token highlighting.
    fn render_highlighted(&self, code: &str, hl: &mut HighlightLines<'_>) -> Vec<Line<'static>> {
        let mut lines: Vec<Line<'static>> = Vec::new();

        for line_str in LinesWithEndings::from(code) {
            // Highlight this source line.
            // `highlight_line` returns a Vec of (Style, &str) token pairs.
            // We convert each to a Ratatui span.
            let ranges = match hl.highlight_line(line_str, &self.syntax_set) {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!("syntect highlight_line error: {e}");
                    // Emit the raw line as a plain span rather than dropping it.
                    lines.push(plain_line(line_str));
                    continue;
                }
            };

            // Strip the trailing newline from display.
            let stripped = line_str.trim_end_matches('\n');

            let spans = build_spans_for_line(&ranges, stripped);
            lines.push(Line::from(spans));
        }

        lines
    }

    /// Render code as plain text with a fallback style modifier.
    fn render_plain(&self, code: &str, fallback: &FallbackStyle) -> Vec<Line<'static>> {
        let base_style = fallback_to_style(fallback);

        LinesWithEndings::from(code)
            .map(|line_str| {
                let text = line_str.trim_end_matches('\n').to_owned();
                Line::from(Span::styled(text, base_style))
            })
            .collect()
    }
}

// -----------------------------------------------------------
// Conversion helpers
// -----------------------------------------------------------

/// Convert a syntect RGBA `Color` to a Ratatui `Color::Rgb`.
///
/// The alpha channel is discarded - terminals don't support per-character alpha.
#[inline]
fn syntect_color_to_ratatui(c: syntect::highlighting::Color) -> Color {
    Color::Rgb(c.r, c.g, c.b)
}

/// Convert syntect `FontStyle` bitflags to Ratatui `Modifier` bitflags.
#[inline]
fn syntect_fontstyle_to_modifier(fs: FontStyle) -> Modifier {
    let mut m = Modifier::empty();

    if fs.contains(FontStyle::BOLD) {
        m |= Modifier::BOLD;
    }
    if fs.contains(FontStyle::ITALIC) {
        m |= Modifier::ITALIC;
    }
    if fs.contains(FontStyle::UNDERLINE) {
        m |= Modifier::UNDERLINED;
    }

    m
}

/// Convert a syntect `Style` to a Ratatui `Style`.
#[inline]
fn syntect_style_to_ratatui(s: SyntectStyle) -> Style {
    let fg = syntect_color_to_ratatui(s.foreground);
    let modifier = syntect_fontstyle_to_modifier(s.font_style);

    let mut style = Style::default().fg(fg).add_modifier(modifier);

    // Only apply the background when syntect says it's non-transparent.
    if s.background.a > 0 {
        style = style.bg(syntect_color_to_ratatui(s.background));
    }

    style
}

/// Convert a `FallbackStyle` config value to a Ratatui `Style`.
#[inline]
fn fallback_to_style(f: &FallbackStyle) -> Style {
    match f {
        FallbackStyle::Dimmed => Style::default().fg(Color::Gray).add_modifier(Modifier::DIM),
        FallbackStyle::Plain => Style::default().fg(Color::White),
    }
}

// -----------------------------------------------------------
// Builders
// -----------------------------------------------------------

/// Build a `Vec<Span<'static>>` from a syntect token range slice covering the visible content of `stripped_line`.
///
/// `ranges` contains `(SyntectStyle, &str)` pairs covering the ORIGINAL line including the trailing '\n'.
/// We accumulate character offsets to map from the range's `&str` slices back into `stripped_line`.
fn build_spans_for_line(
    ranges: &[(SyntectStyle, &str)],
    strippled_line: &str,
) -> Vec<Span<'static>> {
    let mut spans: Vec<Span<'static>> = Vec::with_capacity(ranges.len());
    let mut byte_pos: usize = 0;
    let line_len = strippled_line.len(); // byte length of visible content

    for (style, token) in ranges {
        if byte_pos >= line_len {
            // We've consumed all visible characters. The rest is part of the trailing newline.
            break;
        }

        // Clamp the token to the visible portion of the line.
        let token_bytes = token.len();
        let available = line_len - byte_pos;
        let visible_bytes = token_bytes.min(available);

        if visible_bytes == 0 {
            byte_pos += token_bytes;
            continue;
        }

        let visible_text = &strippled_line[byte_pos..byte_pos + visible_bytes];

        if !visible_text.is_empty() {
            spans.push(Span::styled(
                visible_text.to_owned(),
                syntect_style_to_ratatui(*style),
            ));
        }

        byte_pos += token_bytes;
    }

    spans
}

/// Build a single plain-text `Line` for error/fallback case.
fn plain_line(s: &str) -> Line<'static> {
    Line::from(Span::raw(s.trim_end_matches('\n').to_owned()))
}

// -----------------------------------------------------------
// Tests
// -----------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn hl() -> Highlighter {
        Highlighter::load_defaults()
    }

    // -----------------------------------------------------------
    // Color / Style conversion tests
    // -----------------------------------------------------------

    #[test]
    fn syntect_color_maps_rgb_correctly() {
        let c = syntect::highlighting::Color {
            r: 100,
            g: 150,
            b: 200,
            a: 255,
        };

        assert_eq!(syntect_color_to_ratatui(c), Color::Rgb(100, 150, 200));
    }

    #[test]
    fn syntect_fontstyle_bold_maps_to_modifier() {
        let fs = FontStyle::BOLD;

        assert!(syntect_fontstyle_to_modifier(fs).contains(Modifier::BOLD));
        assert!(!syntect_fontstyle_to_modifier(fs).contains(Modifier::ITALIC));
    }

    #[test]
    fn syntect_fontstyle_italic_maps_to_modifier() {
        let fs = FontStyle::ITALIC;

        assert!(syntect_fontstyle_to_modifier(fs).contains(Modifier::ITALIC));
    }

    #[test]
    fn syntect_fontstyle_underline_maps_to_modifier() {
        let fs = FontStyle::UNDERLINE;

        assert!(syntect_fontstyle_to_modifier(fs).contains(Modifier::UNDERLINED));
    }

    #[test]
    fn syntect_fontstyle_combined_maps_to_modifier() {
        let fs = FontStyle::BOLD | FontStyle::ITALIC;
        let m = syntect_fontstyle_to_modifier(fs);

        assert!(m.contains(Modifier::BOLD));
        assert!(m.contains(Modifier::ITALIC));
        assert!(!m.contains(Modifier::UNDERLINED));
    }

    #[test]
    fn syntect_style_transparent_bg_not_applied() {
        let s = SyntectStyle {
            foreground: syntect::highlighting::Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
            background: syntect::highlighting::Color {
                r: 0,
                g: 0,
                b: 0,
                a: 0, // transparent
            },
            font_style: FontStyle::empty(),
        };
        let style = syntect_style_to_ratatui(s);

        // Background should NOT be applied when alpha=0
        assert_eq!(style.bg, None);
    }

    #[test]
    fn syntect_style_opaque_bg_applied() {
        let s = SyntectStyle {
            foreground: syntect::highlighting::Color {
                r: 255,
                g: 255,
                b: 255,
                a: 255,
            },
            background: syntect::highlighting::Color {
                r: 40,
                g: 44,
                b: 52,
                a: 255, // opaque
            },
            font_style: FontStyle::empty(),
        };
        let style = syntect_style_to_ratatui(s);

        assert_eq!(style.bg, Some(Color::Rgb(40, 44, 52)));
    }

    #[test]
    fn fallback_dimmed_produces_dim_modifier() {
        let s = fallback_to_style(&FallbackStyle::Dimmed);

        assert!(s.add_modifier.contains(Modifier::DIM));
    }

    #[test]
    fn fallback_plain_produces_no_modifier() {
        let s = fallback_to_style(&FallbackStyle::Plain);

        assert_eq!(s.add_modifier, Modifier::empty());
    }

    // -----------------------------------------------------------
    // highlight_block integration tests
    // -----------------------------------------------------------

    #[test]
    fn highlight_block_empty_code_returns_empty() {
        let h = hl();
        let lines = h.highlight_block(
            "",
            Some("rust"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        assert!(lines.is_empty());
    }

    #[test]
    fn highlight_block_rust_produces_colored_spans() {
        let h = hl();
        let code = "fn main() {\n    println!(\"hello\");\n}\n";
        let lines = h.highlight_block(
            code,
            Some("rust"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        // Should produce 3 lines (one per source line, trailing \n stripped)
        assert_eq!(lines.len(), 3, "expected 3 lines for 3-line Rust snippet");

        // First line should have multiple spans (tokens: "fn", " ", "main", etc.)
        assert!(
            lines[0].spans.len() > 1,
            "Rust `fn main()` line should have multiple styled tokens, got: {:?}",
            lines[0].spans
        );

        // Every span should have a non-default foreground color (syntect always assigns fg)
        for line in &lines {
            for span in &line.spans {
                assert!(
                    span.style.fg.is_some(),
                    "highlighted span should have explicit fg color, got: {:?}",
                    span.style
                );
            }
        }
    }

    #[test]
    fn highlight_block_python_produces_lines() {
        let h = hl();
        let code = "def greet(name: str) -> str:\n    return f\"Hello, {name}!\"\n";
        let lines = h.highlight_block(
            code,
            Some("python"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        assert_eq!(lines.len(), 2);
    }

    #[test]
    fn highlight_block_unknown_lang_uses_fallback() {
        let h = hl();
        let code = "some unknown language\nwith two lines\n";
        let lines = h.highlight_block(
            code,
            Some("brainfuck_xyz_does_not_exist"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        assert_eq!(lines.len(), 2);

        // All spans should have the DIM fallback modifier
        for line in &lines {
            for span in &line.spans {
                assert!(
                    span.style.add_modifier.contains(Modifier::DIM),
                    "unknown lang span should have DIM modifier, got: {:?}",
                    span.style
                );
            }
        }
    }

    #[test]
    fn highlight_block_no_lang_uses_fallback() {
        let h = hl();
        let code = "plain text block\n";
        let lines = h.highlight_block(code, None, "base16-ocean.dark", &FallbackStyle::Plain);

        assert_eq!(lines.len(), 1);

        // Plain fallback — no DIM modifier
        for span in &lines[0].spans {
            assert!(
                !span.style.add_modifier.contains(Modifier::DIM),
                "plain fallback should NOT have DIM modifier"
            );
        }
    }

    #[test]
    fn highlight_block_unknown_theme_falls_back_gracefully() {
        // An unknown theme name must not panic — it silently uses FALLBACK_THEME.
        let h = hl();
        let code = "let x = 1;\n";
        let lines = h.highlight_block(
            code,
            Some("rust"),
            "ThisThemeDoesNotExist_xyz",
            &FallbackStyle::Dimmed,
        );

        // Should still produce highlighted output (via FALLBACK_THEME)
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].spans.is_empty());
    }

    #[test]
    fn highlight_block_single_line_no_trailing_newline_in_spans() {
        let h = hl();
        let code = "let x = 42;\n";
        let lines = h.highlight_block(
            code,
            Some("rust"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        // No span content should contain a raw newline character
        for line in &lines {
            for span in &line.spans {
                assert!(
                    !span.content.contains('\n'),
                    "span content must NOT contain newlines: {:?}",
                    span.content
                );
            }
        }
    }

    #[test]
    fn highlight_block_preserves_all_content() {
        // The concatenated plain text of all spans should equal the source code
        // (minus trailing newlines per line).
        let h = hl();
        let code = "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n";
        let lines = h.highlight_block(
            code,
            Some("rust"),
            "base16-ocean.dark",
            &FallbackStyle::Dimmed,
        );

        let reconstructed: String = lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");

        let expected = code.lines().collect::<Vec<_>>().join("\n");

        assert_eq!(
            reconstructed, expected,
            "reconstructed content should match source code"
        );
    }
}
