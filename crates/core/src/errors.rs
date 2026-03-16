use std::path::PathBuf;

use thiserror::Error;

/// All errors that can originate from teh `core` crate.
///
/// Callers in `app` should wrap these in `anyhow::Error` at the boundary.
#[derive(Debug, Error)]
pub enum CoreError {
    /// A config file existed but could not be parsed as valid TOML.
    #[error("Config error: Failed to parse config at {path}: {source}")]
    ConfigParse {
        path: PathBuf,
        #[source]
        source: toml::de::Error,
    },

    /// The config could not be serialized back to TOML (used when writing defaults).
    #[error("Config error: Failed to serialize default config: {0}")]
    ConfigSerialize(#[from] toml::ser::Error),

    /// Generic I/O error while reading or writing the config file.
    #[error("Config error: Config I/O error at {path}: {source}")]
    ConfigIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// Platform config directory could not be resolved (rare; e.g. $HOME unset on Linux).
    #[error("Config error: Cannot determine platform config directory")]
    ConfigDirUnresolvable,

    /// File I/O error while opening or saving a document.
    #[error("Document error: Document I/O error at {path}: {source}")]
    DocumentIo {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    /// The file exists but is not valid UTF-8.
    #[error("Document error: File is not valid UTF-8: {path}")]
    DocumentEncoding { path: PathBuf },

    /// The file does not exist - create new file at path.
    #[error("Document error: No file path set - use save_as() instead")]
    DocumentNoPath,
}
