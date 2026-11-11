//! Bounded Markdown image payload validation and decoding.

use std::io::{BufRead, Read, Seek};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use bcode_markdown_render::MarkdownDestination;
use bmux_tui::image::{ImagePayload, ImagePixelFormat};
use image::ImageDecoder;

/// Maximum encoded image payload accepted by the Markdown image loader.
pub const MAX_ENCODED_BYTES: usize = 8 * 1024 * 1024;
/// Maximum width or height accepted after reading image metadata.
pub const MAX_DIMENSION: u32 = 4096;
/// Maximum decoded pixel count accepted by the Markdown image loader.
pub const MAX_DECODED_PIXELS: u64 = 32_000_000;
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

/// Per-contribution Markdown image presentation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownImagePresentationState {
    /// Loading or decoding failed with a readable diagnostic.
    Failed(String),
    /// A remote source is readable but network loading is disabled.
    NetworkDisabled,
    /// The terminal cannot present images.
    TerminalUnsupported,
}

impl MarkdownImagePresentationState {
    /// Return placeholder text preserving alt text and a safe source in every non-ready state.
    #[must_use]
    pub fn fallback(&self, alt: &str, source: &MarkdownDestination) -> String {
        let state = match self {
            Self::Failed(_) => "image failed",
            Self::NetworkDisabled => "image network disabled",
            Self::TerminalUnsupported => "image unsupported by terminal",
        };
        let alt = if alt.trim().is_empty() {
            "image"
        } else {
            alt.trim()
        };
        let source = markdown_image_source_fallback(source);
        match (self, source) {
            (Self::Failed(error), Some(source)) => {
                format!("[{alt} — {state}: {error}; source: {source}]")
            }
            (Self::Failed(error), None) => format!("[{alt} — {state}: {error}]"),
            (_, Some(source)) => format!("[{alt} — {state}; source: {source}]"),
            (_, None) => format!("[{alt} — {state}]"),
        }
    }

    /// Return a compact fallback for an image nested in a link.
    #[must_use]
    pub fn linked_badge_fallback(
        &self,
        alt: &str,
        source: &MarkdownDestination,
        linked_destination: Option<&MarkdownDestination>,
    ) -> String {
        let image = self.fallback(alt, source);
        let Some(destination) = linked_destination.and_then(markdown_image_source_fallback) else {
            return image;
        };
        let label = if alt.trim().is_empty() {
            "image"
        } else {
            alt.trim()
        };
        if image.is_empty() {
            format!("[{label} → {destination}]")
        } else {
            format!("{image} → {destination}")
        }
    }
}

fn markdown_image_source_fallback(source: &MarkdownDestination) -> Option<String> {
    match source {
        MarkdownDestination::Web(url) => Some(url.as_str().to_owned()),
        MarkdownDestination::LocalPath(path) => Some(path.to_string_lossy().into_owned()),
        MarkdownDestination::Fragment(fragment) => Some(format!("#{fragment}")),
        MarkdownDestination::UnresolvedRelative(source) => Some(source.clone()),
        MarkdownDestination::Inert { .. } => None,
    }
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
    /// Loading was cancelled because its owner changed or left the resident window.
    #[error("image load cancelled")]
    Cancelled,
    /// The local image could not be opened or read.
    #[error("image I/O failed: {0}")]
    Io(String),
    /// The remote image request failed.
    #[error("image request failed: {0}")]
    Network(String),
}

/// Cancellation flag for one Markdown image load.
#[derive(Debug, Clone, Default)]
pub struct MarkdownImageCancellationToken(Arc<AtomicBool>);

impl MarkdownImageCancellationToken {
    /// Request cancellation of associated loading work.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    /// Return whether cancellation was requested.
    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
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
    #[cfg(test)]
    pub async fn load(
        &self,
        destination: &MarkdownDestination,
    ) -> Result<DecodedMarkdownImage, MarkdownImageLoadFailure> {
        self.load_cancellable(destination, &MarkdownImageCancellationToken::default())
            .await
    }

    /// Load and decode one destination with cooperative cancellation.
    ///
    /// # Errors
    ///
    /// Returns the same failures as [`Self::load`], plus cancellation when the
    /// supplied token is cancelled before a transport or decode stage.
    pub async fn load_cancellable(
        &self,
        destination: &MarkdownDestination,
        cancellation: &MarkdownImageCancellationToken,
    ) -> Result<DecodedMarkdownImage, MarkdownImageLoadFailure> {
        ensure_not_cancelled(cancellation)?;
        let started = Instant::now();
        let permit = self.permits.acquire();
        tokio::pin!(permit);
        let _permit = loop {
            tokio::select! {
                result = &mut permit => {
                    break result.map_err(|_| MarkdownImageLoadError::Network("image loader closed".to_owned()))?;
                }
                () = tokio::time::sleep(Duration::from_millis(10)) => {
                    ensure_not_cancelled(cancellation)?;
                    let _ = remaining_load_time(started)?;
                }
            }
        };
        ensure_not_cancelled(cancellation)?;
        let encoded = match markdown_image_load_decision(destination) {
            MarkdownImageLoadDecision::Remote(url) => {
                self.load_remote(url, started, cancellation).await?
            }
            MarkdownImageLoadDecision::Local(path) => {
                let task = tokio::task::spawn_blocking(move || read_local_image(&path));
                wait_for_task(task, started, cancellation).await??
            }
            MarkdownImageLoadDecision::Reject => {
                return Err(MarkdownImageLoadError::RejectedDestination.into());
            }
        };
        ensure_not_cancelled(cancellation)?;
        let task = tokio::task::spawn_blocking(move || {
            decode_markdown_image(std::io::Cursor::new(encoded))
        });
        wait_for_task(task, started, cancellation)
            .await?
            .map_err(Into::into)
    }

    async fn load_remote(
        &self,
        mut url: url::Url,
        started: Instant,
        cancellation: &MarkdownImageCancellationToken,
    ) -> Result<Vec<u8>, MarkdownImageLoadFailure> {
        let mut redirects = 0_usize;
        loop {
            ensure_not_cancelled(cancellation)?;
            let request = self.http.get(url.clone()).send();
            tokio::pin!(request);
            let response = loop {
                tokio::select! {
                    result = &mut request => {
                        break result.map_err(|error| MarkdownImageLoadError::Network(error.to_string()))?;
                    }
                    () = tokio::time::sleep(Duration::from_millis(10)) => {
                        ensure_not_cancelled(cancellation)?;
                        let _ = remaining_load_time(started)?;
                    }
                }
            };
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
            return read_remote_image(response, started, cancellation).await;
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

fn ensure_not_cancelled(
    cancellation: &MarkdownImageCancellationToken,
) -> Result<(), MarkdownImageLoadError> {
    if cancellation.is_cancelled() {
        Err(MarkdownImageLoadError::Cancelled)
    } else {
        Ok(())
    }
}

async fn wait_for_task<T>(
    mut task: tokio::task::JoinHandle<T>,
    started: Instant,
    cancellation: &MarkdownImageCancellationToken,
) -> Result<T, MarkdownImageLoadError> {
    loop {
        tokio::select! {
            result = &mut task => {
                return result.map_err(|error| MarkdownImageLoadError::Io(error.to_string()));
            }
            () = tokio::time::sleep(Duration::from_millis(10)) => {
                if let Err(error) = ensure_not_cancelled(cancellation) {
                    task.abort();
                    return Err(error);
                }
                if let Err(error) = remaining_load_time(started) {
                    task.abort();
                    return Err(error);
                }
            }
        }
    }
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
    cancellation: &MarkdownImageCancellationToken,
) -> Result<Vec<u8>, MarkdownImageLoadFailure> {
    if response
        .content_length()
        .is_some_and(|length| length > u64::try_from(MAX_ENCODED_BYTES).unwrap_or(u64::MAX))
    {
        return Err(MarkdownImageError::EncodedTooLarge.into());
    }
    let mut bytes = Vec::new();
    loop {
        ensure_not_cancelled(cancellation)?;
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
    /// The validated pixels as a BMUX protocol-neutral image payload.
    #[must_use]
    pub fn into_payload(self) -> ImagePayload {
        ImagePayload::Pixels {
            bytes: self.rgba,
            width: self.width,
            height: self.height,
            format: ImagePixelFormat::Rgba8,
        }
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
    use super::*;
    use bcode_markdown_render::{
        MarkdownDestination, MarkdownDestinationRejection, resolve_markdown_destination,
    };
    use image::ImageEncoder;

    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};

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

    #[tokio::test]
    async fn cancelled_load_stops_before_network_or_decode_work() {
        let loader = MarkdownImageLoader::new().unwrap();
        let cancellation = MarkdownImageCancellationToken::default();
        cancellation.cancel();
        let destination = resolve_markdown_destination("https://example.com/image.png", None);

        assert!(matches!(
            loader.load_cancellable(&destination, &cancellation).await,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::Cancelled
            ))
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

    #[tokio::test]
    async fn loader_rejects_malformed_remote_payload() {
        let origin = serve(1, |_, stream| {
            response(
                stream,
                "200 OK",
                &[("Content-Type", "image/png")],
                b"not an image",
            );
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        assert!(matches!(
            loader.load(&destination).await,
            Err(MarkdownImageLoadFailure::Image(
                MarkdownImageError::Invalid(_)
            ))
        ));
    }

    #[tokio::test]
    async fn loader_rejects_non_successful_remote_status() {
        let origin = serve(1, |_, stream| {
            response(stream, "404 Not Found", &[], b"missing");
        });
        let loader = MarkdownImageLoader::new().unwrap();
        let destination = resolve_markdown_destination(&origin, None);

        assert!(matches!(
            loader.load(&destination).await,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::Network(message)
            )) if message == "HTTP 404 Not Found"
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
