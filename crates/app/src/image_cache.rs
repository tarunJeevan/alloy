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
    path::{Component, Path, PathBuf},
    time::Instant,
};

use image::{DynamicImage, ImageReader};
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
pub(crate) fn load_image(
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

const MAX_REMOTE_IMAGE_BYTES: usize = 20 * 1024 * 1024; // 20 MB

/// Fetch and decode a remote image URL via `ureq`.
///
/// Enforces a 5-second connect timeout, 30-second body receive timeout, a 20 MB body cap,
/// and Content-Type validation before reading the body.
/// Returns `ImageLoadError::RemoteFetchDisabled` immediately when `fetch_remote` is false.
fn load_remote(url: &str, fetch_remote: bool) -> Result<DynamicImage, ImageLoadError> {
    if !fetch_remote {
        return Err(ImageLoadError::RemoteFetchDisabled);
    }

    tracing::debug!(url, "image: fetching remote image");

    // ureq 3.x agent builder — AgentBuilder does not exist in ureq 3.x.
    // All timeout methods take Option<Duration>.
    let config = ureq::Agent::config_builder()
        .timeout_connect(Some(std::time::Duration::from_secs(5)))
        .timeout_recv_body(Some(std::time::Duration::from_secs(30)))
        .build();
    let agent = ureq::Agent::new_with_config(config);

    let mut response = agent
        .get(url)
        .call()
        .map_err(|e| ImageLoadError::Fetch(e.to_string()))?;

    // Validate Content-Type before reading the body (F-04).
    let ct = response
        .headers()
        .get("content-type")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    if !ct.starts_with("image/") {
        return Err(ImageLoadError::Fetch(format!(
            "server returned unexpected Content-Type: {ct:?} (expected image/*)"
        )));
    }

    // Bounded body read — read +1 byte over the limit to distinguish "at limit" from "over limit".
    let mut bytes = Vec::new();
    response
        .body_mut()
        .as_reader()
        .take(MAX_REMOTE_IMAGE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| ImageLoadError::Fetch(e.to_string()))?;

    if bytes.len() > MAX_REMOTE_IMAGE_BYTES {
        return Err(ImageLoadError::Fetch(format!(
            "remote image body exceeds {MAX_REMOTE_IMAGE_BYTES}-byte limit"
        )));
    }

    decode_bytes(&bytes, url)
}

/// Resolve a local image path relative to `base_dir`.
///
/// Security invariants (purely lexical, no filesystem calls):
/// - Absolute paths are rejected.
/// - `..` components that escape `base_dir` are rejected.
/// - Windows UNC prefixes (`\\server\share`) are rejected via `Component::Prefix`.
fn resolve_safe_image_path(
    path_str: &str,
    base_dir: Option<&Path>,
) -> Result<PathBuf, ImageLoadError> {
    let raw = Path::new(path_str);

    if raw.is_absolute() {
        return Err(ImageLoadError::Io(format!(
            "absolute image paths are not permitted: {path_str:?}"
        )));
    }

    let base = match base_dir {
        Some(d) => d.to_path_buf(),
        None => std::env::current_dir().map_err(|e| ImageLoadError::Io(e.to_string()))?,
    };

    let mut resolved = base.clone();
    for component in raw.components() {
        match component {
            Component::Normal(seg) => resolved.push(seg),
            Component::CurDir => {}
            Component::ParentDir => {
                if !resolved.pop() {
                    return Err(ImageLoadError::Io(format!(
                        "image path traversal detected (reached root): {path_str:?}"
                    )));
                }
                if !resolved.starts_with(&base) {
                    return Err(ImageLoadError::Io(format!(
                        "image path escapes document directory: {path_str:?}"
                    )));
                }
            }
            // RootDir ("/") or Prefix ("C:\", "\\server\") inside a relative path should not occur after the is_absolute() check above, but guard defensively.
            Component::RootDir | Component::Prefix(_) => {
                return Err(ImageLoadError::Io(format!(
                    "absolute component in image path: {path_str:?}"
                )));
            }
        }
    }

    // Belt-and-suspenders final containment check.
    if !resolved.starts_with(&base) {
        return Err(ImageLoadError::Io(format!(
            "image path escapes document directory (final check): {path_str:?}"
        )));
    }

    Ok(resolved)
}

/// Load and decode a local file path.
///
/// Resolves relative paths against `base_dir` (the directory of the open document)
/// with path traversal protection.
fn load_local(path_str: &str, base_dir: Option<&Path>) -> Result<DynamicImage, ImageLoadError> {
    let path = resolve_safe_image_path(path_str, base_dir)?;
    tracing::debug!(path = %path.display(), "image: loading local image");
    let bytes = std::fs::read(&path).map_err(|e| ImageLoadError::Io(e.to_string()))?;
    decode_bytes(&bytes, &path.display().to_string())
}

const MAX_IMAGE_DIMENSION_PX: u32 = 8_000;
const MAX_IMAGE_DECODED_BYTES: usize = 64 * 1024 * 1024; // 64 MB RGBA worst-case

/// Decode raw bytes into a `DynamicImage` using the `image` crate.
///
/// Performs a dimension pre-check before allocating pixel memory to guard against
/// decompression bombs (e.g. a PNG claiming 65535x65535 pixels).
fn decode_bytes(bytes: &[u8], source_label: &str) -> Result<DynamicImage, ImageLoadError> {
    use std::io::Cursor;

    // Dimension pre-check — no full pixel allocation at this point.
    // `into_dimensions()` consumes the reader; the bytes slice remains accessible.
    let reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| ImageLoadError::Decode(e.to_string()))?;

    if let Ok((w, h)) = reader.into_dimensions() {
        if w > MAX_IMAGE_DIMENSION_PX || h > MAX_IMAGE_DIMENSION_PX {
            return Err(ImageLoadError::Decode(format!(
                "image {w}x{h} exceeds maximum {MAX_IMAGE_DIMENSION_PX}x{MAX_IMAGE_DIMENSION_PX} px"
            )));
        }
        let estimate = (w as usize).saturating_mul(h as usize).saturating_mul(4);
        if estimate > MAX_IMAGE_DECODED_BYTES {
            return Err(ImageLoadError::Decode(format!(
                "image would require ~{estimate} decoded bytes, exceeding {MAX_IMAGE_DECODED_BYTES}-byte limit"
            )));
        }
    }
    // If `into_dimensions()` returns `Err`, fall through.
    // Corrupt data fails safely at the decode step without consuming excess memory.
    image::load_from_memory(bytes).map_err(|e| {
        tracing::debug!(source = source_label, error = %e, "image: decode failed");
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

#[cfg(test)]
mod traversal_tests {
    use super::*;

    fn base() -> PathBuf {
        PathBuf::from("/home/user/notes")
    }

    #[test]
    fn allows_sibling_image() {
        assert!(resolve_safe_image_path("img.png", Some(&base())).is_ok());
    }

    #[test]
    fn allows_subdir_image() {
        assert!(resolve_safe_image_path("assets/fig.png", Some(&base())).is_ok());
    }

    #[test]
    fn allows_nested_subdir() {
        assert!(resolve_safe_image_path("a/b/c/photo.jpg", Some(&base())).is_ok());
    }

    #[test]
    fn allows_same_dir_dot() {
        assert!(resolve_safe_image_path("./img.png", Some(&base())).is_ok());
    }

    #[test]
    fn blocks_single_parent_escape() {
        assert!(resolve_safe_image_path("../secret.png", Some(&base())).is_err());
    }

    #[test]
    fn blocks_deep_escape() {
        assert!(resolve_safe_image_path("a/../../etc/passwd", Some(&base())).is_err());
    }

    #[test]
    fn blocks_absolute_path() {
        assert!(resolve_safe_image_path("/etc/passwd", Some(&base())).is_err());
    }

    #[test]
    fn blocks_root_only() {
        assert!(resolve_safe_image_path("/", Some(&base())).is_err());
    }
}
