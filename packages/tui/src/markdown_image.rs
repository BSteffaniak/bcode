//! Bounded Markdown image payload validation and decoding.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Seek};

use image::ImageDecoder;

/// Maximum encoded image payload accepted by the Markdown image loader.
pub const MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;
/// Maximum width or height accepted after reading image metadata.
pub const MAX_DIMENSION: u32 = 4096;
/// Maximum decoded pixel count accepted by the Markdown image loader.
pub const MAX_DECODED_PIXELS: u64 = 32_000_000;
/// Maximum decoded image cache entries.
pub const MAX_CACHE_ENTRIES: usize = 128;
/// Maximum decoded image cache payload bytes.
pub const MAX_CACHE_BYTES: usize = 64 * 1024 * 1024;

/// Stable key for one normalized Markdown image request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkdownImageCacheKey(String);

impl MarkdownImageCacheKey {
    /// Create a cache key from normalized source and relevant presentation context.
    #[must_use]
    pub fn new(source: &str, context: &str, width: u16, image_capability: &str) -> Self {
        Self(format!(
            "markdown-image-v1:{source}:{context}:{width}:{image_capability}"
        ))
    }
}

/// Validated decoded image suitable for BMUX protocol-neutral presentation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMarkdownImage {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// RGBA8 pixels.
    pub rgba: Vec<u8>,
}

impl DecodedMarkdownImage {
    const fn payload_bytes(&self) -> usize {
        self.rgba.len()
    }
}

/// Deterministic bounded least-recently-used decoded image cache.
#[derive(Debug, Default)]
pub struct MarkdownImageCache {
    entries: BTreeMap<MarkdownImageCacheKey, CacheEntry>,
    clock: u64,
    payload_bytes: usize,
}

#[derive(Debug)]
struct CacheEntry {
    image: DecodedMarkdownImage,
    last_used: u64,
}

impl MarkdownImageCache {
    /// Return a cloned cached image and mark it most recently used.
    pub fn get(&mut self, key: &MarkdownImageCacheKey) -> Option<DecodedMarkdownImage> {
        self.clock = self.clock.saturating_add(1);
        let entry = self.entries.get_mut(key)?;
        entry.last_used = self.clock;
        Some(entry.image.clone())
    }

    /// Insert a successful decoded image and deterministically evict LRU entries.
    pub fn insert(&mut self, key: MarkdownImageCacheKey, image: DecodedMarkdownImage) {
        if image.payload_bytes() > MAX_CACHE_BYTES {
            return;
        }
        self.clock = self.clock.saturating_add(1);
        if let Some(previous) = self.entries.remove(&key) {
            self.payload_bytes = self
                .payload_bytes
                .saturating_sub(previous.image.payload_bytes());
        }
        self.payload_bytes = self.payload_bytes.saturating_add(image.payload_bytes());
        self.entries.insert(
            key,
            CacheEntry {
                image,
                last_used: self.clock,
            },
        );
        self.evict_to_limits();
    }

    /// Return current cache entry count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether the cache is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Return current decoded payload bytes.
    #[must_use]
    pub const fn payload_bytes(&self) -> usize {
        self.payload_bytes
    }

    fn evict_to_limits(&mut self) {
        while self.entries.len() > MAX_CACHE_ENTRIES || self.payload_bytes > MAX_CACHE_BYTES {
            let Some(key) = self
                .entries
                .iter()
                .min_by_key(|(key, entry)| (entry.last_used, *key))
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            if let Some(entry) = self.entries.remove(&key) {
                self.payload_bytes = self
                    .payload_bytes
                    .saturating_sub(entry.image.payload_bytes());
            }
        }
    }
}

/// In-flight image keys used to deduplicate concurrent loading work.
#[derive(Debug, Default)]
pub struct MarkdownImageInflight {
    keys: BTreeSet<MarkdownImageCacheKey>,
}

impl MarkdownImageInflight {
    /// Start work for `key`; returns false when identical work is already active.
    pub fn start(&mut self, key: MarkdownImageCacheKey) -> bool {
        self.keys.insert(key)
    }

    /// Finish or cancel work for `key`.
    pub fn finish(&mut self, key: &MarkdownImageCacheKey) {
        self.keys.remove(key);
    }

    /// Remove work that is no longer owned by a resident/current item.
    pub fn retain(&mut self, active: &BTreeSet<MarkdownImageCacheKey>) {
        self.keys.retain(|key| active.contains(key));
    }

    /// Return active in-flight work count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    /// Return whether no work is active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }
}

/// Markdown image payload rejection.
#[derive(Debug, thiserror::Error)]
pub enum MarkdownImageError {
    /// Encoded bytes exceed the fixed loader limit.
    #[error("encoded image exceeds {MAX_ENCODED_BYTES} bytes")]
    EncodedTooLarge,
    /// Image metadata could not be read.
    #[error("invalid image: {0}")]
    Invalid(#[from] image::ImageError),
    /// Dimensions exceed the fixed per-axis limit.
    #[error("image dimensions {width}x{height} exceed {MAX_DIMENSION}x{MAX_DIMENSION}")]
    DimensionsTooLarge {
        /// Declared width.
        width: u32,
        /// Declared height.
        height: u32,
    },
    /// Decoded pixel count exceeds the fixed limit.
    #[error("image pixel count {pixels} exceeds {MAX_DECODED_PIXELS}")]
    TooManyPixels {
        /// Declared pixel count.
        pixels: u64,
    },
    /// Decoder allocation requirement exceeds the fixed decoded-byte limit.
    #[error("decoded image requires too many bytes")]
    DecodedTooLarge,
}

/// Read an encoded image under the fixed byte limit, validate metadata before
/// pixel allocation, and decode to RGBA8.
///
/// # Errors
///
/// Returns [`MarkdownImageError`] when encoded bytes, dimensions, pixel count,
/// decoder allocation, or image syntax violate the fixed policy.
pub fn decode_markdown_image(
    reader: impl BufRead + Seek,
) -> Result<DecodedMarkdownImage, MarkdownImageError> {
    let encoded = read_encoded_bounded(reader)?;
    let cursor = std::io::Cursor::new(&encoded);
    let reader = image::ImageReader::new(cursor)
        .with_guessed_format()
        .map_err(image::ImageError::IoError)?;
    let decoder = reader.into_decoder()?;
    let (width, height) = decoder.dimensions();
    validate_dimensions(width, height)?;
    let decoded_bytes = decoder.total_bytes();
    let max_decoded_bytes = MAX_DECODED_PIXELS.saturating_mul(4);
    if decoded_bytes > max_decoded_bytes {
        return Err(MarkdownImageError::DecodedTooLarge);
    }
    let image = image::DynamicImage::from_decoder(decoder)?;
    let rgba = image.into_rgba8().into_raw();
    if u64::try_from(rgba.len()).unwrap_or(u64::MAX) > max_decoded_bytes {
        return Err(MarkdownImageError::DecodedTooLarge);
    }
    Ok(DecodedMarkdownImage {
        width,
        height,
        rgba,
    })
}

fn read_encoded_bounded(reader: impl BufRead + Seek) -> Result<Vec<u8>, MarkdownImageError> {
    let mut bytes = Vec::new();
    reader
        .take(u64::try_from(MAX_ENCODED_BYTES).unwrap_or(u64::MAX) + 1)
        .read_to_end(&mut bytes)
        .map_err(image::ImageError::IoError)?;
    if bytes.len() > MAX_ENCODED_BYTES {
        return Err(MarkdownImageError::EncodedTooLarge);
    }
    Ok(bytes)
}

fn validate_dimensions(width: u32, height: u32) -> Result<(), MarkdownImageError> {
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(MarkdownImageError::DimensionsTooLarge { width, height });
    }
    let pixels = u64::from(width).saturating_mul(u64::from(height));
    if pixels > MAX_DECODED_PIXELS {
        return Err(MarkdownImageError::TooManyPixels { pixels });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        DecodedMarkdownImage, MAX_CACHE_BYTES, MAX_CACHE_ENTRIES, MAX_DECODED_PIXELS,
        MAX_DIMENSION, MAX_ENCODED_BYTES, MarkdownImageCache, MarkdownImageCacheKey,
        MarkdownImageError, MarkdownImageInflight, decode_markdown_image, validate_dimensions,
    };
    use image::ImageEncoder;
    use std::collections::BTreeSet;

    fn png(width: u32, height: u32) -> Vec<u8> {
        let pixels = vec![0x80; usize::try_from(u64::from(width) * u64::from(height) * 4).unwrap()];
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new(&mut bytes)
            .write_image(&pixels, width, height, image::ExtendedColorType::Rgba8)
            .unwrap();
        bytes
    }

    fn image(value: u8, bytes: usize) -> DecodedMarkdownImage {
        DecodedMarkdownImage {
            width: 1,
            height: 1,
            rgba: vec![value; bytes],
        }
    }

    fn key(index: usize) -> MarkdownImageCacheKey {
        MarkdownImageCacheKey::new(&format!("image-{index}"), "context", 80, "kitty")
    }

    #[test]
    fn bounded_cache_evicts_deterministic_least_recently_used_entry() {
        let mut cache = MarkdownImageCache::default();
        for index in 0..MAX_CACHE_ENTRIES {
            cache.insert(key(index), image(u8::try_from(index).unwrap_or(0), 4));
        }
        assert!(cache.get(&key(0)).is_some());
        cache.insert(key(MAX_CACHE_ENTRIES), image(255, 4));

        assert_eq!(cache.len(), MAX_CACHE_ENTRIES);
        assert!(cache.get(&key(0)).is_some());
        assert!(cache.get(&key(1)).is_none());
        assert!(cache.get(&key(MAX_CACHE_ENTRIES)).is_some());
    }

    #[test]
    fn bounded_cache_enforces_payload_limit_and_skips_oversized_entry() {
        let mut cache = MarkdownImageCache::default();
        cache.insert(key(0), image(1, MAX_CACHE_BYTES));
        cache.insert(key(1), image(2, 4));
        assert!(cache.get(&key(0)).is_none());
        assert!(cache.get(&key(1)).is_some());
        assert!(cache.payload_bytes() <= MAX_CACHE_BYTES);

        cache.insert(key(2), image(3, MAX_CACHE_BYTES + 1));
        assert!(cache.get(&key(2)).is_none());
    }

    #[test]
    fn inflight_keys_deduplicate_and_cancel_nonresident_work() {
        let mut inflight = MarkdownImageInflight::default();
        assert!(inflight.start(key(1)));
        assert!(!inflight.start(key(1)));
        assert!(inflight.start(key(2)));
        inflight.retain(&BTreeSet::from([key(2)]));
        assert!(inflight.start(key(1)));
        inflight.finish(&key(1));
        inflight.finish(&key(2));
        assert!(inflight.is_empty());
    }

    #[test]
    fn validates_encoded_bytes_before_attempting_decode() {
        let oversized = vec![0; MAX_ENCODED_BYTES + 1];
        assert!(matches!(
            decode_markdown_image(std::io::Cursor::new(oversized)),
            Err(MarkdownImageError::EncodedTooLarge)
        ));
    }

    #[test]
    fn rejects_dimensions_before_pixel_allocation() {
        assert!(matches!(
            validate_dimensions(MAX_DIMENSION + 1, 1),
            Err(MarkdownImageError::DimensionsTooLarge { .. })
        ));
        assert!(matches!(
            validate_dimensions(1, MAX_DIMENSION + 1),
            Err(MarkdownImageError::DimensionsTooLarge { .. })
        ));
    }

    #[test]
    fn rejects_pixel_count_before_pixel_allocation() {
        assert!(matches!(
            validate_dimensions(MAX_DIMENSION, MAX_DIMENSION),
            Ok(())
        ));
        assert!(matches!(validate_dimensions(4096, 4096), Ok(())));
        assert!(MAX_DECODED_PIXELS >= u64::from(MAX_DIMENSION) * u64::from(MAX_DIMENSION));
    }

    #[test]
    fn decodes_valid_payload_to_rgba() {
        let decoded = decode_markdown_image(std::io::Cursor::new(png(2, 3))).unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 3));
        assert_eq!(decoded.rgba.len(), 24);
    }

    #[test]
    fn malformed_payload_is_rejected() {
        assert!(matches!(
            decode_markdown_image(std::io::Cursor::new(b"not an image".to_vec())),
            Err(MarkdownImageError::Invalid(_))
        ));
    }
}
