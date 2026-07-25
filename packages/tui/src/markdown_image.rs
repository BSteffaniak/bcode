//! Bounded Markdown image payload validation and decoding.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Seek};
use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_markdown_render::MarkdownDestination;
use bmux_tui::geometry::Rect;
use bmux_tui::image::{
    ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
};
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
/// Maximum redirects followed by one Markdown image request.
pub const MAX_REDIRECTS: usize = 5;
/// Maximum wall-clock duration for one Markdown image request.
pub const LOAD_TIMEOUT: Duration = Duration::from_secs(10);
/// Maximum simultaneous Markdown image loads.
pub const MAX_CONCURRENT_LOADS: usize = 4;

/// One bounded image-loading policy decision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownImageLoadDecision {
    /// Fetch an HTTP(S) destination through a redirect-validating client.
    Remote(url::Url),
    /// Read a path already resolved under trusted document context.
    Local(std::path::PathBuf),
    /// Do not load an unsafe, unresolved, or fragment-only destination.
    Reject,
}

/// Classify an already-resolved Markdown image destination for loading.
#[must_use]
pub fn markdown_image_load_decision(
    destination: &MarkdownDestination,
) -> MarkdownImageLoadDecision {
    match destination {
        MarkdownDestination::Web(url) if matches!(url.scheme(), "http" | "https") => {
            MarkdownImageLoadDecision::Remote(url.clone())
        }
        MarkdownDestination::LocalPath(path) => MarkdownImageLoadDecision::Local(path.clone()),
        MarkdownDestination::Fragment(_)
        | MarkdownDestination::UnresolvedRelative(_)
        | MarkdownDestination::Inert { .. }
        | MarkdownDestination::Web(_) => MarkdownImageLoadDecision::Reject,
    }
}

/// Redirect and timeout guard shared by concrete image-loading adapters.
#[derive(Debug, Clone)]
pub struct MarkdownImageLoadGuard {
    started: Instant,
    redirects: usize,
}

impl Default for MarkdownImageLoadGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl MarkdownImageLoadGuard {
    /// Start a bounded image load.
    #[must_use]
    pub fn new() -> Self {
        Self {
            started: Instant::now(),
            redirects: 0,
        }
    }

    /// Revalidate and account for one redirect destination.
    ///
    /// # Errors
    ///
    /// Returns an error when the deadline elapsed, the redirect limit was
    /// reached, or the destination is not HTTP(S).
    pub fn follow_redirect(
        &mut self,
        destination: &MarkdownDestination,
    ) -> Result<url::Url, MarkdownImageLoadError> {
        self.check_deadline()?;
        if self.redirects >= MAX_REDIRECTS {
            return Err(MarkdownImageLoadError::TooManyRedirects);
        }
        let MarkdownImageLoadDecision::Remote(url) = markdown_image_load_decision(destination)
        else {
            return Err(MarkdownImageLoadError::RejectedDestination);
        };
        self.redirects = self.redirects.saturating_add(1);
        Ok(url)
    }

    /// Revalidate the final destination without consuming a redirect.
    ///
    /// # Errors
    ///
    /// Returns an error when the deadline elapsed or the destination is not loadable.
    pub fn final_destination(
        &self,
        destination: &MarkdownDestination,
    ) -> Result<MarkdownImageLoadDecision, MarkdownImageLoadError> {
        self.check_deadline()?;
        let decision = markdown_image_load_decision(destination);
        if decision == MarkdownImageLoadDecision::Reject {
            Err(MarkdownImageLoadError::RejectedDestination)
        } else {
            Ok(decision)
        }
    }

    fn check_deadline(&self) -> Result<(), MarkdownImageLoadError> {
        if self.started.elapsed() >= LOAD_TIMEOUT {
            Err(MarkdownImageLoadError::TimedOut)
        } else {
            Ok(())
        }
    }
}

/// Image request policy failure.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum MarkdownImageLoadError {
    /// Destination is unsafe or lacks trusted resolution context.
    #[error("image destination is not loadable")]
    RejectedDestination,
    /// Redirect count exceeded the fixed policy limit.
    #[error("image redirect limit exceeded")]
    TooManyRedirects,
    /// Request exceeded the fixed wall-clock deadline.
    #[error("image load timed out")]
    TimedOut,
    /// The local image could not be opened or read.
    #[error("image I/O failed: {0}")]
    Io(String),
    /// The remote image request failed.
    #[error("image request failed: {0}")]
    Network(String),
}

/// Concurrency-limited Markdown image loader.
#[derive(Debug, Clone)]
pub struct MarkdownImageLoader {
    http: reqwest::Client,
    permits: Arc<tokio::sync::Semaphore>,
}

impl MarkdownImageLoader {
    /// Create a loader with redirects disabled in the HTTP client so each hop is
    /// reclassified by Bcode before it is followed.
    ///
    /// # Errors
    ///
    /// Returns an error when the bounded HTTP client cannot be constructed.
    pub fn new() -> Result<Self, MarkdownImageLoadError> {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(LOAD_TIMEOUT)
            .build()
            .map_err(|error| MarkdownImageLoadError::Network(error.to_string()))?;
        Ok(Self {
            http,
            permits: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_LOADS)),
        })
    }

    /// Load and decode one already-classified Markdown image destination.
    ///
    /// Local files are read through the same encoded-byte limit as remote
    /// responses. Remote redirects are followed manually and every `Location`
    /// is resolved and reclassified before another request is sent.
    ///
    /// # Errors
    ///
    /// Returns an error for rejected destinations, redirect/timeout/concurrency
    /// failures, I/O or network errors, oversized payloads, or invalid images.
    pub async fn load(
        &self,
        destination: &MarkdownDestination,
    ) -> Result<DecodedMarkdownImage, MarkdownImageLoadFailure> {
        let started = Instant::now();
        let _permit = tokio::time::timeout(remaining_load_time(started)?, self.permits.acquire())
            .await
            .map_err(|_| MarkdownImageLoadError::TimedOut)?
            .map_err(|_| MarkdownImageLoadError::Network("image loader closed".to_owned()))?;
        let encoded = match markdown_image_load_decision(destination) {
            MarkdownImageLoadDecision::Remote(url) => self.load_remote(url, started).await?,
            MarkdownImageLoadDecision::Local(path) => tokio::time::timeout(
                remaining_load_time(started)?,
                tokio::task::spawn_blocking(move || read_local_image(&path)),
            )
            .await
            .map_err(|_| MarkdownImageLoadError::TimedOut)?
            .map_err(|error| MarkdownImageLoadError::Io(error.to_string()))??,
            MarkdownImageLoadDecision::Reject => {
                return Err(MarkdownImageLoadError::RejectedDestination.into());
            }
        };
        tokio::time::timeout(
            remaining_load_time(started)?,
            tokio::task::spawn_blocking(move || {
                decode_markdown_image(std::io::Cursor::new(encoded))
            }),
        )
        .await
        .map_err(|_| MarkdownImageLoadError::TimedOut)?
        .map_err(|error| MarkdownImageLoadError::Io(error.to_string()))?
        .map_err(Into::into)
    }

    async fn load_remote(
        &self,
        mut url: url::Url,
        started: Instant,
    ) -> Result<Vec<u8>, MarkdownImageLoadFailure> {
        let mut redirects = 0_usize;
        loop {
            let response = tokio::time::timeout(
                remaining_load_time(started)?,
                self.http.get(url.clone()).send(),
            )
            .await
            .map_err(|_| MarkdownImageLoadError::TimedOut)?
            .map_err(|error| MarkdownImageLoadError::Network(error.to_string()))?;
            if response.status().is_redirection() {
                if redirects >= MAX_REDIRECTS {
                    return Err(MarkdownImageLoadError::TooManyRedirects.into());
                }
                let location = response
                    .headers()
                    .get(reqwest::header::LOCATION)
                    .ok_or(MarkdownImageLoadError::RejectedDestination)?
                    .to_str()
                    .map_err(|_| MarkdownImageLoadError::RejectedDestination)?;
                let next = url
                    .join(location)
                    .map_err(|_| MarkdownImageLoadError::RejectedDestination)?;
                let destination = MarkdownDestination::Web(next);
                let MarkdownImageLoadDecision::Remote(next) =
                    markdown_image_load_decision(&destination)
                else {
                    return Err(MarkdownImageLoadError::RejectedDestination.into());
                };
                url = next;
                redirects = redirects.saturating_add(1);
                continue;
            }
            if !response.status().is_success() {
                return Err(
                    MarkdownImageLoadError::Network(format!("HTTP {}", response.status())).into(),
                );
            }
            return read_remote_image(response, started).await;
        }
    }
}

/// Complete Markdown image loading or decoding failure.
#[derive(Debug, thiserror::Error)]
pub enum MarkdownImageLoadFailure {
    /// Loading policy or transport failure.
    #[error(transparent)]
    Load(#[from] MarkdownImageLoadError),
    /// Encoded payload or decoded image rejection.
    #[error(transparent)]
    Image(#[from] MarkdownImageError),
}

fn remaining_load_time(started: Instant) -> Result<Duration, MarkdownImageLoadError> {
    LOAD_TIMEOUT
        .checked_sub(started.elapsed())
        .filter(|remaining| !remaining.is_zero())
        .ok_or(MarkdownImageLoadError::TimedOut)
}

fn read_local_image(path: &std::path::Path) -> Result<Vec<u8>, MarkdownImageLoadFailure> {
    let file =
        std::fs::File::open(path).map_err(|error| MarkdownImageLoadError::Io(error.to_string()))?;
    read_encoded_bounded(std::io::BufReader::new(file)).map_err(Into::into)
}

async fn read_remote_image(
    mut response: reqwest::Response,
    started: Instant,
) -> Result<Vec<u8>, MarkdownImageLoadFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(MAX_ENCODED_BYTES).unwrap_or(u64::MAX))
    {
        return Err(MarkdownImageError::EncodedTooLarge.into());
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = tokio::time::timeout(remaining_load_time(started)?, response.chunk())
            .await
            .map_err(|_| MarkdownImageLoadError::TimedOut)?
            .map_err(|error| MarkdownImageLoadError::Network(error.to_string()))?;
        let Some(chunk) = chunk else {
            return Ok(bytes);
        };
        if bytes.len().saturating_add(chunk.len()) > MAX_ENCODED_BYTES {
            return Err(MarkdownImageError::EncodedTooLarge.into());
        }
        bytes.extend_from_slice(&chunk);
    }
}

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

    /// Adapt validated pixels into BMUX's protocol-neutral frame contribution.
    #[must_use]
    pub fn bmux_contribution(
        &self,
        key: impl Into<String>,
        destination: Rect,
        clip: Rect,
    ) -> ImageContribution {
        ImageContribution::Present(ImagePlacement {
            key: ImageKey::new(key),
            payload: ImagePayload::Pixels {
                bytes: self.rgba.clone(),
                width: self.width,
                height: self.height,
                format: ImagePixelFormat::Rgba8,
            },
            destination,
            clip,
            lifecycle: ImageLifecycle::Frame,
        })
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
        MAX_DIMENSION, MAX_ENCODED_BYTES, MAX_REDIRECTS, MarkdownImageCache, MarkdownImageCacheKey,
        MarkdownImageError, MarkdownImageInflight, MarkdownImageLoadDecision,
        MarkdownImageLoadError, MarkdownImageLoadFailure, MarkdownImageLoadGuard,
        MarkdownImageLoader, decode_markdown_image, markdown_image_load_decision,
        validate_dimensions,
    };
    use bcode_markdown_render::{
        MarkdownDestination, MarkdownDestinationRejection, resolve_markdown_destination,
    };
    use image::ImageEncoder;
    use std::collections::BTreeSet;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{ImageContribution, ImageLifecycle, ImagePayload, ImagePixelFormat};

    fn response(stream: &mut TcpStream, status: &str, headers: &[(&str, &str)], body: &[u8]) {
        let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
        for (name, value) in headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("Connection: close\r\n\r\n");
        stream.write_all(head.as_bytes()).unwrap();
        stream.write_all(body).unwrap();
    }

    fn serve(
        request_count: usize,
        handler: impl Fn(usize, &mut TcpStream) + Send + 'static,
    ) -> String {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for (index, incoming) in listener.incoming().take(request_count).enumerate() {
                let mut stream = incoming.unwrap();
                let mut request = [0_u8; 2048];
                let _ = stream.read(&mut request).unwrap();
                handler(index, &mut stream);
            }
        });
        format!("http://{address}/image")
    }

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
    fn decoded_image_adapts_to_protocol_neutral_bmux_pixels() {
        let decoded = DecodedMarkdownImage {
            width: 2,
            height: 3,
            rgba: vec![0x80; 24],
        };
        let contribution = decoded.bmux_contribution(
            "markdown-image:item:1",
            Rect::new(2, 4, 20, 6),
            Rect::new(0, 2, 80, 20),
        );
        let ImageContribution::Present(placement) = contribution else {
            panic!("expected presented image")
        };
        assert_eq!(placement.key.as_str(), "markdown-image:item:1");
        assert_eq!(placement.destination, Rect::new(2, 4, 20, 6));
        assert_eq!(placement.clip, Rect::new(0, 2, 80, 20));
        assert_eq!(placement.lifecycle, ImageLifecycle::Frame);
        assert!(matches!(
            placement.payload,
            ImagePayload::Pixels {
                bytes,
                width: 2,
                height: 3,
                format: ImagePixelFormat::Rgba8,
            } if bytes == vec![0x80; 24]
        ));
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
    fn image_loading_policy_allows_only_classified_http_https_and_trusted_local_paths() {
        let https = resolve_markdown_destination("https://example.com/image.png", None);
        assert!(matches!(
            markdown_image_load_decision(&https),
            MarkdownImageLoadDecision::Remote(url) if url.scheme() == "https"
        ));
        let local = MarkdownDestination::LocalPath(std::path::PathBuf::from("/trusted/image.png"));
        assert_eq!(
            markdown_image_load_decision(&local),
            MarkdownImageLoadDecision::Local(std::path::PathBuf::from("/trusted/image.png"))
        );

        for rejected in [
            MarkdownDestination::Fragment("image".to_owned()),
            MarkdownDestination::UnresolvedRelative("image.png".to_owned()),
            MarkdownDestination::Inert {
                reason: MarkdownDestinationRejection::UnsupportedScheme,
            },
        ] {
            assert_eq!(
                markdown_image_load_decision(&rejected),
                MarkdownImageLoadDecision::Reject
            );
        }
    }

    #[test]
    fn every_redirect_and_final_destination_is_revalidated() {
        let safe = resolve_markdown_destination("https://example.com/image.png", None);
        let unsafe_destination = resolve_markdown_destination("file://remote/image.png", None);
        let mut guard = MarkdownImageLoadGuard::new();
        for _ in 0..MAX_REDIRECTS {
            assert!(guard.follow_redirect(&safe).is_ok());
        }
        assert_eq!(
            guard.follow_redirect(&safe),
            Err(MarkdownImageLoadError::TooManyRedirects)
        );

        let mut guard = MarkdownImageLoadGuard::new();
        assert_eq!(
            guard.follow_redirect(&unsafe_destination),
            Err(MarkdownImageLoadError::RejectedDestination)
        );
        assert_eq!(
            guard.final_destination(&unsafe_destination),
            Err(MarkdownImageLoadError::RejectedDestination)
        );
        assert!(matches!(
            guard.final_destination(&safe),
            Ok(MarkdownImageLoadDecision::Remote(_))
        ));
    }

    #[tokio::test]
    async fn loader_reads_trusted_local_image_and_rejects_unresolved_source() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("image.png");
        std::fs::write(&path, png(2, 3)).unwrap();
        let loader = MarkdownImageLoader::new().unwrap();

        let decoded = loader
            .load(&MarkdownDestination::LocalPath(path))
            .await
            .unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 3));
        assert!(matches!(
            loader
                .load(&MarkdownDestination::UnresolvedRelative(
                    "image.png".to_owned()
                ))
                .await,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::RejectedDestination
            ))
        ));
    }

    #[tokio::test]
    async fn loader_follows_bounded_http_redirects_and_decodes_final_payload() {
        let image = png(2, 3);
        let origin = serve(2, move |index, stream| {
            if index == 0 {
                response(stream, "302 Found", &[("Location", "/final")], &[]);
            } else {
                response(stream, "200 OK", &[("Content-Type", "image/png")], &image);
            }
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        let decoded = loader.load(&destination).await.unwrap();
        assert_eq!((decoded.width, decoded.height), (2, 3));
    }

    #[tokio::test]
    async fn loader_revalidates_redirect_scheme_before_following_it() {
        let origin = serve(1, |_, stream| {
            response(
                stream,
                "302 Found",
                &[("Location", "file:///tmp/secret.png")],
                &[],
            );
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        assert!(matches!(
            loader.load(&destination).await,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::RejectedDestination
            ))
        ));
    }

    #[tokio::test]
    async fn loader_stops_after_fixed_redirect_limit() {
        let origin = serve(MAX_REDIRECTS + 1, |_, stream| {
            response(stream, "302 Found", &[("Location", "/again")], &[]);
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        assert!(matches!(
            loader.load(&destination).await,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::TooManyRedirects
            ))
        ));
    }

    #[tokio::test]
    async fn loader_rejects_oversized_remote_payload_before_decode() {
        let origin = serve(1, |_, stream| {
            response(
                stream,
                "200 OK",
                &[("Content-Type", "image/png")],
                &vec![0_u8; MAX_ENCODED_BYTES + 1],
            );
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        assert!(matches!(
            loader.load(&destination).await,
            Err(MarkdownImageLoadFailure::Image(
                MarkdownImageError::EncodedTooLarge
            ))
        ));
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
