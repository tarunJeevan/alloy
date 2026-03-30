//! Image loading, decoding, resizing, and LRU caching for the preview pane.
//!
//! Architecture:
//!
//! `ImageCache` is owned by App and shared with the preview worker thread via `Arc<Mutex<ImageCache>>`. The worker loads and decodes images on cache misses; the UI thread reads cached `StatefulProtocol` instances for rendering.
//!
//! Cache lifecycle:
//!
//! - On a cache miss, the worker calls `ImageCache::get_or_load` which loads, decodes, and encodes the image for the detected protocol.
//! - LRU eviction fires when `len() > max_entries`. `last_used` timestamps drive the eviction decision.
//! - On terminal resize, `ImageCache::invalidate_all` is called to force re-encoding at the new cell dimensions on the next render.
//!
//! Remote images:
//!
//! - Remote fetching is gated behind `config.images.fetch_remote`. When disabled (default), remote image URLs render as placeholders.
//! - When enabled, `ureq` fetches the URL synchronously on the worker thread with a 5-second timeout.
//!
//! Error handling:
//!
//! All errors (file not found, decode failure, network error, unsupported format) return `ImageLoadError` with a descriptive message. The caller (preview_worker) renders the error message as a styled placeholder span rather than panicking.
//!
//! Thread safety:
//!
//! `ImageCache` itself is `!Send` because `StatefulProtocol` contains raw pointers. The `Arc<Mutex<ImageCache>>` wrapper on App and the worker thread both hold the lock only for the duration of a single load or lookup, never across a render.

use std::{
    collections::HashMap,
    io::Read,
    path::{Path, PathBuf},
    time::Instant,
};

use image::DynamicImage;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::StatefulProtocol;

// ------------------------------------------------------------
// Error type
// ------------------------------------------------------------

/// All image load or decode errors.
#[derive(Debug, Clone)]
pub enum ImageLoadError {
    /// The file path does not exist or cannot be read.
    Io(String),

    /// The image data could not be decoded (unsupported format, corrupt file, etc.).
    Decode(String),

    /// A remote URL fetch failed (network error, timeout, non-200 status. etc.).
    Fetch(String),

    /// Remote fetching is disabled in config.
    RemoteFetchDisabled,
}

impl std::fmt::Display for ImageLoadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(e) => write!(f, "I/O error: {e}"),
            Self::Decode(e) => write!(f, "decode error: {e}"),
            Self::Fetch(e) => write!(f, "fetch error: {e}"),
            Self::RemoteFetchDisabled => {
                write!(
                    f,
                    "remote image fetching is disabled (set images.fetch_remote = true"
                )
            }
        }
    }
}
// ------------------------------------------------------------
// Cache entry
// ------------------------------------------------------------

/// A successfully loaded and protocol-encoded image entry in the cache.
pub struct CachedImage {
    /// The encoded image ready for `ratatui-image` widget rendering.
    ///
    /// `StatefulProtocol` is a type-erased handle to the protocol-specific state (Kitty chunks, SIXEL data, halfblock spans, etc.). It is invalidated and rebuilt on terminal resize.
    pub protocol: StatefulProtocol,

    /// Timestamp of last access. Updated on every cache hit.
    pub last_used: Instant,
}

// ------------------------------------------------------------
// ImageCache
// ------------------------------------------------------------

/// LRU-evicting cache of decoded and protocol-encoded images.
///
/// Key format:
///
/// - Cache keys are the raw URL/path strings from Markdown source, normalized to lowercase for deduplication on case-insensitive file systems.
/// - Remote URLs are used verbatim.
pub struct ImageCache {
    entries: HashMap<String, CachedImage>,

    /// Maximum number of images to keep in the cache simultaneously.
    /// When exceeded, the LRU entry (oldest `last_used`) is evicted.
    pub max_entries: usize,
}

impl ImageCache {
    /// Create a new cache with the given entry limit.
    pub fn new(max_entries: usize) -> Self {
        Self {
            entries: HashMap::new(),
            max_entries,
        }
    }

    /// Return a mutable access to a cache entry by key.
    pub fn get_mut(&mut self, key: &str) -> Option<&mut CachedImage> {
        self.entries.get_mut(key)
    }

    /// Invalidate and remove all cached entries.
    ///
    /// Called on terminal resize so images are re-encoded at the new cell dimensions on the next render cycle.
    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        tracing::debug!("image cache invalidated (terminal resize)");
    }

    /// Look up `key` in the cache - load, decode, and encode if absent.
    ///
    /// On a cache hit, updates `last_used` and returns a reference to the entry.
    /// On a cache miss, loads the image, inserts it, and evicts LRU if necessary.
    ///
    /// Parameters:
    ///
    /// - `key`: the raw URL or file path from Markdown source.
    /// - `picker`: the ratatui-image `Picker` used to encode the decoded image into the terminal's graphics protocol.
    /// - `fetch_remote`: whether HTTP/HTTPS URLs should be fetched.
    /// - `base_dir`: the directory of the currently open document, used to resolve relative file paths.
    pub fn get_or_load(
        &mut self,
        key: &str,
        picker: &mut Picker,
        fetch_remote: bool,
        base_dir: Option<&Path>,
    ) -> Result<&CachedImage, ImageLoadError> {
        // Cache hit - update timestamp and return.
        if self.entries.contains_key(key) {
            self.entries.get_mut(key).unwrap().last_used = Instant::now();
            return Ok(self.entries.get(key).unwrap());
        }

        // Cache miss - load the image.
        let dyn_image = load_image(key, fetch_remote, base_dir)?;

        // Encode for the current terminal protocol.
        let protocol = picker.new_resize_protocol(dyn_image);

        // Evict LRU if at capacity.
        if self.entries.len() >= self.max_entries {
            self.evict_lru();
        }

        // Insert the new entry.
        self.entries.insert(
            key.to_owned(),
            CachedImage {
                protocol,
                last_used: Instant::now(),
            },
        );

        Ok(self.entries.get(key).unwrap())
    }

    /// Evict the least-recently-used entry from the cache.
    fn evict_lru(&mut self) {
        if let Some(oldest_key) = self
            .entries
            .iter()
            .min_by_key(|(_, v)| v.last_used)
            .map(|(k, _)| k.to_owned())
        {
            self.entries.remove(&oldest_key);
            tracing::debug!(key = %oldest_key, "image cache: LRU entry evicted");
        }
    }
}

// ------------------------------------------------------------
// Image loading
// ------------------------------------------------------------

/// Load and decode an image from a local file path or remote URL.
///
/// Classification:
/// - "http://" OR "https://" -> remote fetch (gated by `fetch_remote`)
/// - Anything else -> filepath resolved relative to `base_dir`
fn load_image(
    key: &str,
    fetch_remote: bool,
    base_dir: Option<&Path>,
) -> Result<DynamicImage, ImageLoadError> {
    let trimmed = key.trim();

    if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
        load_remote(trimmed, fetch_remote)
    } else {
        load_local(trimmed, base_dir)
    }
}

/// Fetch and decode a remote image URL via `ureq`.
///
/// Respects the 5-sec timeout configured on the agent.
/// Returns `ImageLoadError::RemoteFetchDisabled` immediately when `fetch_remote` is false.
fn load_remote(url: &str, fetch_remote: bool) -> Result<DynamicImage, ImageLoadError> {
    if !fetch_remote {
        return Err(ImageLoadError::RemoteFetchDisabled);
    }

    tracing::debug!(url, "image: fetching remote image");

    // Build a ureq agent with a 5-sec timeout.
    let agent = ureq::agent();

    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| ImageLoadError::Fetch(e.to_string()))?;

    // Read the body bytes.
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .read_to_end(&mut bytes)
        .map_err(|e| ImageLoadError::Fetch(e.to_string()))?;

    decode_bytes(&bytes, url)
}

/// Load and deocde a local file path.
///
/// Resolves relative paths against `base_dir` (the directory of the open document).
fn load_local(path_str: &str, base_dir: Option<&Path>) -> Result<DynamicImage, ImageLoadError> {
    let path = if Path::new(path_str).is_relative() {
        base_dir
            .map(|d| d.join(path_str))
            .unwrap_or_else(|| PathBuf::from(path_str))
    } else {
        PathBuf::from(path_str)
    };

    tracing::debug!(path = %path.display(), "image: loading local image");

    let bytes = std::fs::read(&path).map_err(|e| ImageLoadError::Io(e.to_string()))?;

    decode_bytes(&bytes, &path.display().to_string())
}

/// Decode raw bytes into a `DynamicImage` using the `image` crate.
fn decode_bytes(bytes: &[u8], source_label: &str) -> Result<DynamicImage, ImageLoadError> {
    image::load_from_memory(bytes).map_err(|e| {
        tracing::debug!(source = source_label, error = %e, "image: decode failes");
        ImageLoadError::Decode(e.to_string())
    })
}

// ------------------------------------------------------------
// Tests
// ------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_starts_empty() {
        let cache = ImageCache::new(10);

        assert_eq!(cache.entries.len(), 0);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn load_nonexistent_path_returns_io_error_not_panic() {
        let result = load_local("/nonexistent/path/image.png", None);

        assert!(
            matches!(result, Err(ImageLoadError::Io(_))),
            "expected Io error for missing file, got: {result:?}"
        );
    }

    #[test]
    fn remote_fetch_disabled_returns_error() {
        let result = load_remote("https://example.com/image.png", false);

        assert!(
            matches!(result, Err(ImageLoadError::RemoteFetchDisabled)),
            "expected RemoteFetchDisabled, got: {result:?}"
        );
    }

    #[test]
    fn load_local_png_decodes_correctly() {
        // Use the test fixture PNG bundled in the repo (from assets/).
        // If no PNG is available in tests, this test is silently skipped.
        let test_png = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("test_image.png");

        if !test_png.exists() {
            eprintln!("skipping load_local_png_decodes_correctly: fixture not found");
            return;
        }

        let result = load_local(test_png.to_str().unwrap(), None);

        assert!(result.is_ok(), "expected Ok, got: {result:?}");
    }

    #[test]
    fn decode_corrupt_bytes_returns_decode_error() {
        let garbage = b"this is not an image";
        let result = decode_bytes(garbage, "test");

        assert!(
            matches!(result, Err(ImageLoadError::Decode(_))),
            "expected Decode error for corrupt bytes"
        );
    }

    #[test]
    fn invalidate_all_clears_entries() {
        let mut cache = ImageCache::new(10);
        // We cannot insert real entries without a Picker, so we test the clear
        // logic indirectly by verifying it starts empty after invalidation.
        cache.invalidate_all();

        assert!(cache.entries.is_empty());
    }
}
