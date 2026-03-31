//! Application configuration.
//!
//! The config is stored as TOML and lives at the platform-appropriate path:
//!
//! | Platform | Path |
//! |----------|------|
//! | Linux    | `$XDG_CONFIG_HOME/alloy/config.toml` (falls back to `~/.config`) |
//! | macOS    | `$~/Library/Application Support/alloy/config.toml` |
//! | Windows  | `%APPDATA%\alloy\config.toml` |
//!
//! On first run, the default config is written to disk so the user has a template to edit.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::errors::CoreError;

// -----------------------------------------------------------------------
// App identity
// -----------------------------------------------------------------------

const APP_NAME: &str = "alloy";

// -----------------------------------------------------------------------
// Top-level config
// -----------------------------------------------------------------------

/// Root configuration struct. All fields have defaults via `Config::default()`.
///
/// The `config_version` field is reserved for future migration handling. If the version found on disk differs from [`CURRENT_CONFIG_VERSION`], a warning is logged and defaults are returned (no silent data corruption).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Config {
    /// Bump this when the schema changes in a breaking way.
    pub config_version: u32,

    pub theme: ThemeConfig,
    pub keymap: KeymapConfig,
    pub editor: EditorConfig,
    pub markdown: MarkdownConfig,
    pub ui: UiConfig,
    pub terminal: TerminalConfig,
    pub images: ImagesConfig,
    #[serde(default)]
    pub highlighting: HighlightingConfig,
}

/// The config schema version written by this build.
pub const CURRENT_CONFIG_VERSION: u32 = 1;

impl Default for Config {
    fn default() -> Self {
        Self {
            config_version: CURRENT_CONFIG_VERSION,
            theme: ThemeConfig::default(),
            keymap: KeymapConfig::default(),
            editor: EditorConfig::default(),
            markdown: MarkdownConfig::default(),
            ui: UiConfig::default(),
            terminal: TerminalConfig::default(),
            images: ImagesConfig::default(),
            highlighting: HighlightingConfig::default(),
        }
    }
}

// -----------------------------------------------------------------------
// Theme Config
// -----------------------------------------------------------------------

/// Visual appearance settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ThemeConfig {
    /// Name of the built-in color theme to use.
    /// Available: "default", "nord", "gruvbox-dark"
    pub name: String,

    /// Name of the syntext theme used to highlight fenced code blocks.
    /// Any theme bundled in the syntect default set is valid.
    pub code_theme: String,
}

impl Default for ThemeConfig {
    fn default() -> Self {
        Self {
            name: "default".into(),
            code_theme: "base16-ocean.dark".into(),
        }
    }
}

// -----------------------------------------------------------------------
// Keymap Config
// -----------------------------------------------------------------------

/// Keybinding overrides.
///
/// Each value is a space-separated sequence of key tokens that will be parsed at startup into a `Vec<KeyEvent>`. For example, `"g g"` means press `g` twice in quick succession.
///
/// The defaults reflect Vim/Helix conventions. All fields are `Option<String>` so the user only needs to specify the bindings they want to override; `None` means "use the built-in default".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeymapConfig {
    /// Enter insert mode (default: `"i"`)
    pub enter_insert: Option<String>,

    /// Return to normal mode (default: `"Esc"`)
    pub exit_insert: Option<String>,

    /// Save the current file (default: `"w"`)
    pub save: Option<String>,

    /// Quit (default: `"q"`)
    pub quit: Option<String>,

    /// Toggle preview mode (default: `"t p"`)
    pub toggle_preview: Option<String>,

    /// Enter link-select mode (default: `"f l"`)
    pub link_select: Option<String>,

    /// Enter search mode - literal (default: `"/"`)
    pub search_literal: Option<String>,

    /// Enter search mode - regex (default: `"?"`)
    pub search_regex: Option<String>,
}

// -----------------------------------------------------------------------
// Editor Config
// -----------------------------------------------------------------------

/// Editing-behavior settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorConfig {
    /// Milliseconds to wait after the last keystroke before triggering a preview re-render.
    /// Lower values feel more responsive; higher values reduce CPU usage on slow machines.
    pub preview_debounce_ms: u64,

    /// Milliseconds to wait for a second key in a multi-key Normal-mode sequence (e.g. `g g`),
    /// If the timeout expires before the next key arrives, the pending key is dispatched individually.
    pub sequence_timeout_ms: u64,

    /// Show line numbers in the editor pane.
    pub line_numbers: bool,

    /// Insert `n` spaces instead of a tab character.
    /// Set to `0` to insert real tabs.
    pub tab_width: u8,

    /// Search is case-insensitive by default.
    /// Set to `false` for case-sensitive search.
    pub search_case_insensitive: bool,
}

impl Default for EditorConfig {
    fn default() -> Self {
        Self {
            preview_debounce_ms: 150,
            sequence_timeout_ms: 500,
            line_numbers: true,
            tab_width: 4,
            search_case_insensitive: true,
        }
    }
}

// -----------------------------------------------------------------------
// Markdown Config
// -----------------------------------------------------------------------

/// Markdown parsing and rendering settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct MarkdownConfig {
    /// The parser backend used for terminal preview rendering.
    pub engine: MarkdownEngine,

    /// Dialect extension toggles.
    pub extensions: ExtensionConfig,
}

impl Default for MarkdownConfig {
    fn default() -> Self {
        Self {
            engine: MarkdownEngine::PulldownCmark,
            extensions: ExtensionConfig::default(),
        }
    }
}

/// Available Markdown parser backends.
///
/// `PulldownCmark` is the default and recommended choice for the terminal preview (fast event-streaming). `Comrak` is used unconditionally for HTML output regardless of this setting.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownEngine {
    #[default]
    PulldownCmark,
    Comrak,
    MarkdownRs,
}

/// Feature flags for Markdown dialect extensions.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExtensionConfig {
    /// GitHub Flavored Markdown (tables, task lists, strikethrough, autolinks).
    /// On by default.
    pub gfm: bool,

    /// `[[wiki]]` and `[[wiki|title]]` style links.
    /// Off in MVP.
    pub wiki_links: bool,

    /// YAML/TOML frontmatter blocks.
    /// Off in MVP.
    pub frontmatter: bool,

    /// LaTeX math blocks (`$...$`, `$$...$$`).
    /// Off in MVP.
    pub math: bool,
}

impl Default for ExtensionConfig {
    fn default() -> Self {
        Self {
            gfm: true,
            wiki_links: false,
            frontmatter: false,
            math: false,
        }
    }
}

// -----------------------------------------------------------------------
// UI Config
// -----------------------------------------------------------------------

/// Layout and visual-behavior settings for the TUI.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiConfig {
    /// Percentage of the terminal width given to the editor pane when the preview is visible. The preview takes the remainder.
    /// Valid range: 10-98
    pub split_ratio: u8,

    /// Initial preview mode on startup.
    /// Options: "rendered", "html", and "hidden"
    pub initial_preview_mode: PreviewModeConfig,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            split_ratio: 50,
            initial_preview_mode: PreviewModeConfig::Rendered,
        }
    }
}

/// Serializable form of the initial preview mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum PreviewModeConfig {
    #[default]
    Rendered,
    Html,
    Hidden,
}

// -----------------------------------------------------------------------
// Terminal Config
// -----------------------------------------------------------------------

/// Terminal-capability settings.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct TerminalConfig {
    /// OSC-8 terminal hyperlink emission in the preview pane.
    ///
    /// - "off" (default): never emit hyperlinks
    /// - "auto": emit only when `supports-hyperlinks` detects supprt.
    /// - "on": always emit (use when auto-detection gives a false negative).
    pub hyperlinks: HyperlinksMode,
}

impl Default for TerminalConfig {
    fn default() -> Self {
        Self {
            hyperlinks: HyperlinksMode::Off,
        }
    }
}

// -----------------------------------------------------------------------
// Images Config
// -----------------------------------------------------------------------

/// Image settings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ImagesConfig {
    /// Enable image rendering in the preview pane.
    pub enabled: bool,

    /// Which graphics protocol to use.
    /// "auto" = detect at startup | "kitty" | "iterm2" | "sixel" | "off" = force/disable
    pub protocol: ImageProtocol,

    /// Allow fetching remote images over HTTP/HTTPS.
    /// Defaults to false for privacy/security reasons.
    pub fetch_remote: bool,
}

impl Default for ImagesConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            protocol: ImageProtocol::Auto,
            fetch_remote: false,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ImageProtocol {
    #[default]
    Auto,
    Kitty,
    Iterm2,
    Sixel,
    Off,
}

/// Controls whether OSC-8 terminal hyperlinks are emitted in the preview pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum HyperlinksMode {
    /// Never emit OSC-8 sequences (safe default until validated per-terminal).
    #[default]
    Off,

    /// Emit when `supports-hyperlinks` detects support at startup.
    Auto,

    /// Always emit regardless of detection result.
    On,
}

// -----------------------------------------------------------------------
// Config Path Resolution
// -----------------------------------------------------------------------

/// Returns the canonical path to the config file for the current platform.
///
/// Does NOT create the file or directory.
pub fn config_file_path() -> Result<PathBuf, CoreError> {
    let base = dirs::config_dir().ok_or(CoreError::ConfigDirUnresolvable)?;

    Ok(base.join(APP_NAME).join("config.toml"))
}

// -----------------------------------------------------------------------
// Highlighting Config
// -----------------------------------------------------------------------

/// Syntax highlighting settings for fenced code blocks in the preview pane.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HighlightingConfig {
    /// Enable syntax highlighting for fenced code blocks.
    /// When `false`, code blocks use plain monospace styling.
    pub enabled: bool,

    /// Syntect theme name to use for code highlighting.
    /// An unknown theme name silently falls back to "base16-ocean.dark"
    pub theme: String,

    /// Style applied to code blocks with an unknown or empty language tag.
    pub fallback_style: FallbackStyle,
}

impl Default for HighlightingConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            theme: "base16-ocean.dark".into(),
            fallback_style: FallbackStyle::Dimmed,
        }
    }
}

/// Controls the appearance of code blcoks when the language tag is unknown or unspecified.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum FallbackStyle {
    /// Apply DIM modifier - visually de-emphasizes plain/unknown code blocks.
    #[default]
    Dimmed,

    /// No additional styling - plain monospace text at the default foreground color.
    Plain,
}

// -----------------------------------------------------------------------
// Load / Save
// -----------------------------------------------------------------------

impl Config {
    /// Load the configuration from the platform config file.
    ///
    /// Behavior:
    /// - If the file does not exist, the default config is written to disk and returned.
    /// - If the file exists but has a different `config_version`, a warning is logged and the default config is returned (no mutation of the file on disk).
    /// - If the file is malformed TOML, a `CoreError::ConfigParse` is returned.
    pub fn load() -> Result<Self, CoreError> {
        let path = config_file_path()?;

        Self::load_from(&path)
    }

    /// Like `load()` but reads from an explicit path.
    /// Used in tests.
    pub fn load_from(path: &Path) -> Result<Self, CoreError> {
        if !path.exists() {
            debug!(path = %path.display(), "config not found; writing defaults");
            let defaults = Config::default();
            defaults.write_to(path)?;

            return Ok(defaults);
        }

        let raw = std::fs::read_to_string(path).map_err(|source| CoreError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;

        let cfg: Config = toml::from_str(&raw).map_err(|source| CoreError::ConfigParse {
            path: path.to_owned(),
            source,
        })?;

        if cfg.config_version != CURRENT_CONFIG_VERSION {
            warn!(
                found = cfg.config_version,
                expected = CURRENT_CONFIG_VERSION,
                "config version mismatch - falling back to defaults"
            );
            return Ok(Config::default());
        }

        debug!(path = %path.display(), "config loaded successfully");
        Ok(cfg)
    }

    /// Serialize this config to TOML and write it to `path`, creating any intermediate directories as needed.
    pub fn write_to(&self, path: &Path) -> Result<(), CoreError> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|source| CoreError::ConfigIo {
                path: parent.to_owned(),
                source,
            })?;
        }

        let toml_str = toml::to_string_pretty(self)?;

        std::fs::write(path, toml_str).map_err(|source| CoreError::ConfigIo {
            path: path.to_owned(),
            source,
        })?;

        Ok(())
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

    // Round trip
    #[test]
    fn default_config_round_trips() {
        let cfg = Config::default();
        let toml_str = toml::to_string_pretty(&cfg).expect("serialize failed");
        let back: Config = toml::from_str(&toml_str).expect("deserialize failed");

        // Original config and deserialized config should be the same
        assert_eq!(cfg, back);
    }

    #[test]
    fn default_config_version_is_current() {
        assert_eq!(Config::default().config_version, CURRENT_CONFIG_VERSION);
    }

    // Load: missing config file writes defaults
    #[test]
    fn load_missing_file_writes_defaults_and_returns_them() {
        let dir = tmp();
        let path = dir.path().join("alloy").join("config.toml");

        assert!(!path.exists(), "file should not exist before load");

        let cfg = Config::load_from(&path).expect("load failed");

        // Config written to disk should have default values
        assert_eq!(cfg, Config::default());
        assert!(path.exists(), "defaults should have been written to disk");
    }

    // Load: valid file parses correctly
    #[test]
    fn load_valid_file_returns_parsed_config() {
        let dir = tmp();
        let path = dir.path().join("config.toml");

        let mut original = Config::default();
        original.editor.preview_debounce_ms = 300;
        original.ui.split_ratio = 60;

        original.write_to(&path).expect("write failed");

        let loaded = Config::load_from(&path).expect("load failed");

        // Loaded config should have the adjusted values
        assert_eq!(loaded.editor.preview_debounce_ms, 300);
        assert_eq!(loaded.ui.split_ratio, 60);
    }

    // Load: malformed TOML returns CoreError::ConfigParse
    #[test]
    fn load_malformed_toml_returns_config_parse_error() {
        let dir = tmp();
        let path = dir.path().join("config.toml");
        fs::write(&path, b"this is [[[not valid toml").expect("write failed");

        // Loading config should result in ConfigParse error
        let err = Config::load_from(&path).expect_err("should fail");
        assert!(
            matches!(err, CoreError::ConfigParse { .. }),
            "unexpected error variant: {err:?}"
        );

        // Error message should mention the path
        let msg = err.to_string();
        assert!(msg.contains("config.toml"), "expected path in error: {msg}");
    }

    // Load: version mismatch falls back to defaults
    #[test]
    fn load_version_mismatch_returns_defaults() {
        let dir = tmp();
        let path = dir.path().join("config.toml");

        // Create default config with theoretical future config version
        let mut cfg = Config {
            config_version: 999,
            ..Default::default()
        };
        cfg.editor.tab_width = 2;
        cfg.write_to(&path).expect("write failed");

        let loaded = Config::load_from(&path).expect("load should succeed");

        // Should silently return defaults, not the mutated version
        assert_eq!(loaded, Config::default());
    }

    // ExtensionConfig defaults
    #[test]
    fn default_extensions_gfm_on_others_off() {
        let ext = ExtensionConfig::default();
        assert!(ext.gfm);
        assert!(!ext.wiki_links);
        assert!(!ext.frontmatter);
        assert!(!ext.math);
    }

    // split_ratio default
    #[test]
    fn default_split_ratio_is_50() {
        assert_eq!(Config::default().ui.split_ratio, 50);
    }

    // ImagesConfig defaults

    #[test]
    fn default_images_config_enabled_auto_no_remote() {
        let cfg = ImagesConfig::default();

        assert!(cfg.enabled);
        assert_eq!(cfg.protocol, ImageProtocol::Auto);
        assert!(!cfg.fetch_remote);
    }

    #[test]
    fn images_config_round_trips() {
        let mut cfg = Config::default();
        cfg.images.enabled = false;
        cfg.images.protocol = ImageProtocol::Kitty;
        cfg.images.fetch_remote = true;

        let toml_str = toml::to_string_pretty(&cfg).expect("serialize failed");
        let back: Config = toml::from_str(&toml_str).expect("deserialize failed");

        assert!(!back.images.enabled);
        assert_eq!(back.images.protocol, ImageProtocol::Kitty);
        assert!(back.images.fetch_remote);
    }

    #[test]
    fn images_protocol_off_serializes_correctly() {
        let mut cfg = Config::default();
        cfg.images.protocol = ImageProtocol::Off;
        let toml_str = toml::to_string_pretty(&cfg).expect("serialize failed");

        assert!(
            toml_str.contains("protocol = \"off\""),
            "expected 'off' in toml: {toml_str}"
        );
    }

    // HighlightingConfig tests

    #[test]
    fn default_highlighting_config_is_enabled_with_ocean_theme() {
        let cfg = HighlightingConfig::default();

        assert!(cfg.enabled);
        assert_eq!(cfg.theme, "base16-ocean.dark");
        assert_eq!(cfg.fallback_style, FallbackStyle::Dimmed);
    }

    #[test]
    fn highlighting_config_round_trips() {
        let mut cfg = Config::default();
        cfg.highlighting.enabled = false;
        cfg.highlighting.theme = "Solarized (dark)".into();
        cfg.highlighting.fallback_style = FallbackStyle::Plain;

        let toml_str = toml::to_string_pretty(&cfg).expect("serialize failed");
        let back: Config = toml::from_str(&toml_str).expect("deserialize failed");

        assert!(!back.highlighting.enabled);
        assert_eq!(back.highlighting.theme, "Solarized (dark)");
        assert_eq!(back.highlighting.fallback_style, FallbackStyle::Plain);
    }

    #[test]
    fn config_with_no_highlighting_section_uses_defaults() {
        // Simulate a config file written before Chunk 8.1 (no [highlighting] section).
        // Thanks to #[serde(default)], this should deserialize cleanly.
        let toml_without_highlighting = r#"
config_version = 1
[theme]
name = "default"
code_theme = "base16-ocean.dark"
[keymap]
[editor]
preview_debounce_ms = 150
sequence_timeout_ms = 500
line_numbers = true
tab_width = 4
search_case_insensitive = true
[markdown]
engine = "pulldown_cmark"
[markdown.extensions]
gfm = true
wiki_links = false
frontmatter = false
math = false
[ui]
split_ratio = 50
initial_preview_mode = "rendered"
[terminal]
hyperlinks = "off"
[images]
enabled = true
protocol = "auto"
fetch_remote = false
"#;
        let cfg: Config = toml::from_str(toml_without_highlighting)
            .expect("should parse without [highlighting] section");

        // Should get defaults for the missing section.
        assert!(
            cfg.highlighting.enabled,
            "default highlighting should be enabled"
        );
        assert_eq!(cfg.highlighting.theme, "base16-ocean.dark");
        assert_eq!(cfg.highlighting.fallback_style, FallbackStyle::Dimmed);
    }

    #[test]
    fn fallback_style_serializes_correctly() {
        let cfg = HighlightingConfig {
            enabled: true,
            theme: "InspiredGitHub".into(),
            fallback_style: FallbackStyle::Plain,
        };
        let toml_str = toml::to_string_pretty(&cfg).expect("serialize failed");

        assert!(
            toml_str.contains("fallback_style = \"plain\""),
            "expected 'plain' in toml: {toml_str}"
        );
    }
}
