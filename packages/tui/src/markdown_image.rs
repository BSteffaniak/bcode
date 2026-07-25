//! Bounded Markdown image payload validation and decoding.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{BufRead, Read, Seek};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
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

/// Capability and policy inputs controlling image presentation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MarkdownImagePresentationPolicy {
    /// Whether loading is running from an interactive resident transcript frame.
    /// Discovery/open/attach/history reconstruction must pass `false`.
    pub interactive_resident_frame: bool,
    /// Whether HTTP(S) fetching is allowed.
    pub network_enabled: bool,
    /// Whether BMUX reports terminal image transport support.
    pub terminal_supported: bool,
}

/// Per-contribution Markdown image presentation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownImagePresentationState {
    /// No load has started.
    Idle,
    /// A bounded load is active.
    Loading,
    /// Validated decoded pixels are ready for BMUX presentation.
    Ready(DecodedMarkdownImage),
    /// Loading or decoding failed with a readable diagnostic.
    Failed(String),
    /// A remote source is readable but network loading is disabled.
    NetworkDisabled,
    /// The terminal cannot present images.
    TerminalUnsupported,
}

/// Fixed terminal rows reserved for one image before loading begins.
pub const RESERVED_IMAGE_ROWS: u16 = 4;

/// Return stable terminal row reservation for every image presentation state.
#[must_use]
pub const fn markdown_image_reserved_rows(_state: &MarkdownImagePresentationState) -> u16 {
    RESERVED_IMAGE_ROWS
}

/// One current image contribution used to reconcile presentation ownership.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownImagePresentationInput {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Versioned source and capability key; changes reset presentation state.
    pub cache_key: MarkdownImageCacheKey,
    /// Classified image destination.
    pub destination: MarkdownDestination,
    /// Whether this resident contribution is eligible to start bounded work.
    pub residency: MarkdownImageResidency,
}

/// Resident visibility class used to gate non-eager loading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MarkdownImageResidency {
    /// Resident but outside the visible/prefetch projection.
    Hidden,
    /// Currently intersects the transcript viewport.
    Visible,
    /// Included in the caller's bounded prefetch window.
    Prefetch,
}

impl MarkdownImageResidency {
    const fn may_load(self) -> bool {
        matches!(self, Self::Visible | Self::Prefetch)
    }
}

/// One newly scheduled image load.
#[derive(Debug, Clone)]
pub struct MarkdownImageLoadRequest {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Deduplicated decoded-payload key.
    pub cache_key: MarkdownImageCacheKey,
    /// Classified destination passed to the bounded loader.
    pub destination: MarkdownDestination,
    /// Cancellation token tied to current resident ownership.
    pub cancellation: MarkdownImageCancellationToken,
}

/// Resident per-contribution image presentation state.
#[derive(Debug, Default)]
pub struct MarkdownImagePresentationStore {
    entries: BTreeMap<String, MarkdownImagePresentationEntry>,
}

#[derive(Debug)]
struct MarkdownImagePresentationEntry {
    cache_key: MarkdownImageCacheKey,
    state: MarkdownImagePresentationState,
}

impl MarkdownImagePresentationStore {
    /// Reconcile state with the bounded resident contribution projection.
    ///
    /// State survives redraw, scrolling, and resize while contribution identity
    /// and its versioned cache key remain unchanged. Removed or replaced owners
    /// are dropped immediately.
    pub fn reconcile(
        &mut self,
        inputs: &[MarkdownImagePresentationInput],
        policy: MarkdownImagePresentationPolicy,
    ) {
        let active = inputs
            .iter()
            .map(|input| input.contribution_id.as_str())
            .collect::<BTreeSet<_>>();
        self.entries
            .retain(|contribution_id, _| active.contains(contribution_id.as_str()));
        for input in inputs {
            let reset = self
                .entries
                .get(&input.contribution_id)
                .is_none_or(|entry| entry.cache_key != input.cache_key);
            if reset {
                self.entries.insert(
                    input.contribution_id.clone(),
                    MarkdownImagePresentationEntry {
                        cache_key: input.cache_key.clone(),
                        state: MarkdownImagePresentationState::initial(&input.destination, policy),
                    },
                );
            }
        }
    }

    /// Reconcile ownership and cancel in-flight work no longer referenced by a resident owner.
    pub fn reconcile_with_inflight(
        &mut self,
        inputs: &[MarkdownImagePresentationInput],
        policy: MarkdownImagePresentationPolicy,
        inflight: &mut MarkdownImageInflight,
    ) {
        self.reconcile(inputs, policy);
        let active_keys = inputs
            .iter()
            .map(|input| input.cache_key.clone())
            .collect::<BTreeSet<_>>();
        inflight.retain(&active_keys);
    }

    /// Schedule loads only for visible or explicitly bounded-prefetch residents.
    ///
    /// Remote work remains disabled unless the explicit policy allows network
    /// access. Existing in-flight cache keys are deduplicated.
    #[must_use]
    pub fn schedule_loads(
        &mut self,
        inputs: &[MarkdownImagePresentationInput],
        policy: MarkdownImagePresentationPolicy,
        inflight: &mut MarkdownImageInflight,
    ) -> Vec<MarkdownImageLoadRequest> {
        let mut requests = Vec::new();
        for input in inputs.iter().filter(|input| input.residency.may_load()) {
            let Some(entry) = self.entries.get_mut(&input.contribution_id) else {
                continue;
            };
            if !matches!(entry.state, MarkdownImagePresentationState::Idle)
                || !entry.state.start_loading(&input.destination, policy)
            {
                continue;
            }
            if !inflight.start(input.cache_key.clone()) {
                entry.state = MarkdownImagePresentationState::Idle;
                continue;
            }
            let Some(cancellation) = inflight.cancellation_token(&input.cache_key) else {
                entry.state = MarkdownImagePresentationState::Idle;
                continue;
            };
            requests.push(MarkdownImageLoadRequest {
                contribution_id: input.contribution_id.clone(),
                cache_key: input.cache_key.clone(),
                destination: input.destination.clone(),
                cancellation,
            });
        }
        requests
    }

    /// Complete a load and update every resident owner sharing its cache key.
    pub fn complete_load(
        &mut self,
        key: &MarkdownImageCacheKey,
        result: Result<DecodedMarkdownImage, MarkdownImageLoadFailure>,
        cache: &mut MarkdownImageCache,
        inflight: &mut MarkdownImageInflight,
    ) {
        inflight.finish(key);
        match result {
            Ok(image) => {
                cache.insert(key.clone(), image.clone());
                for entry in self
                    .entries
                    .values_mut()
                    .filter(|entry| &entry.cache_key == key)
                {
                    entry.state.ready(image.clone());
                }
            }
            Err(failure) => {
                for entry in self
                    .entries
                    .values_mut()
                    .filter(|entry| &entry.cache_key == key)
                {
                    entry.state.failed(&failure);
                }
            }
        }
    }

    /// Hydrate idle resident states from the bounded decoded cache.
    pub fn hydrate_from_cache(&mut self, cache: &mut MarkdownImageCache) {
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| matches!(entry.state, MarkdownImagePresentationState::Idle))
        {
            if let Some(image) = cache.get(&entry.cache_key) {
                entry.state.ready(image);
            }
        }
    }

    /// Emit ready pixels into the current BMUX frame with stable identity and clipped placement.
    pub fn present_ready(
        &self,
        contribution_id: &str,
        destination: Rect,
        clip: Rect,
        frame: &mut bmux_tui::frame::Frame<'_>,
    ) -> bool {
        let Some(MarkdownImagePresentationState::Ready(image)) = self.state(contribution_id) else {
            return false;
        };
        let clipped = destination.intersection(clip);
        if clipped.is_empty() {
            return false;
        }
        frame.push_image(image.bmux_contribution(
            format!("markdown:{contribution_id}"),
            destination,
            clip,
        ));
        true
    }

    /// Emit explicit removals for stable keys no longer present in this frame.
    pub fn remove_from_frame(contribution_id: &str, frame: &mut bmux_tui::frame::Frame<'_>) {
        frame.push_image(ImageContribution::Remove(ImageKey::new(format!(
            "markdown:{contribution_id}"
        ))));
    }

    /// Return presentation state for one resident contribution.
    #[must_use]
    pub fn state(&self, contribution_id: &str) -> Option<&MarkdownImagePresentationState> {
        self.entries.get(contribution_id).map(|entry| &entry.state)
    }

    /// Return mutable presentation state for one resident contribution.
    pub fn state_mut(
        &mut self,
        contribution_id: &str,
    ) -> Option<&mut MarkdownImagePresentationState> {
        self.entries
            .get_mut(contribution_id)
            .map(|entry| &mut entry.state)
    }

    /// Return the number of retained contribution states.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Return whether no contribution state is retained.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

impl MarkdownImagePresentationState {
    /// Derive the initial non-eager state from typed destination and capabilities.
    #[must_use]
    pub fn initial(
        destination: &MarkdownDestination,
        policy: MarkdownImagePresentationPolicy,
    ) -> Self {
        if !policy.interactive_resident_frame {
            return Self::Idle;
        }
        if !policy.terminal_supported {
            return Self::TerminalUnsupported;
        }
        if !policy.network_enabled
            && matches!(
                markdown_image_load_decision(destination),
                MarkdownImageLoadDecision::Remote(_)
            )
        {
            return Self::NetworkDisabled;
        }
        Self::Idle
    }

    /// Transition to loading only when policy permits work for the destination.
    pub fn start_loading(
        &mut self,
        destination: &MarkdownDestination,
        policy: MarkdownImagePresentationPolicy,
    ) -> bool {
        if !policy.interactive_resident_frame {
            *self = Self::Idle;
            return false;
        }
        let initial = Self::initial(destination, policy);
        if !matches!(initial, Self::Idle)
            || markdown_image_load_decision(destination) == MarkdownImageLoadDecision::Reject
        {
            *self = initial;
            return false;
        }
        *self = Self::Loading;
        true
    }

    /// Store successful validated pixels.
    pub fn ready(&mut self, image: DecodedMarkdownImage) {
        *self = Self::Ready(image);
    }

    /// Store a stable readable loading failure.
    pub fn failed(&mut self, failure: &MarkdownImageLoadFailure) {
        *self = Self::Failed(failure.to_string());
    }

    /// Return placeholder text preserving alt text and a safe source in every non-ready state.
    #[must_use]
    pub fn fallback(&self, alt: &str, source: &MarkdownDestination) -> String {
        let state = match self {
            Self::Idle => "image idle",
            Self::Loading => "image loading",
            Self::Failed(_) => "image failed",
            Self::NetworkDisabled => "image network disabled",
            Self::TerminalUnsupported => "image unsupported by terminal",
            Self::Ready(_) => return String::new(),
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

/// Stable key for one normalized Markdown image request.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct MarkdownImageCacheKey(String);

impl MarkdownImageCacheKey {
    /// Create a cache key from normalized source and decode-affecting context.
    ///
    /// Viewport width and terminal protocol are intentionally excluded: decoded
    /// source pixels are reusable when placement changes during scroll/resize or
    /// when BMUX selects a different transport protocol.
    #[must_use]
    pub fn new(source: &str, context: &str) -> Self {
        Self(format!("markdown-image-v1:{source}:{context}"))
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
    ///
    /// Placement is supplied per frame, so scroll and resize update geometry
    /// without changing or decoding the cached pixel payload.
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
    loads: BTreeMap<MarkdownImageCacheKey, MarkdownImageCancellationToken>,
}

impl MarkdownImageInflight {
    /// Start work for `key`; returns false when identical work is already active.
    pub fn start(&mut self, key: MarkdownImageCacheKey) -> bool {
        if self.loads.contains_key(&key) {
            return false;
        }
        self.loads
            .insert(key, MarkdownImageCancellationToken::default());
        true
    }

    /// Return the cancellation token for active work owned by `key`.
    #[must_use]
    pub fn cancellation_token(
        &self,
        key: &MarkdownImageCacheKey,
    ) -> Option<MarkdownImageCancellationToken> {
        self.loads.get(key).cloned()
    }

    /// Finish completed work for `key` without cancelling it.
    pub fn finish(&mut self, key: &MarkdownImageCacheKey) {
        self.loads.remove(key);
    }

    /// Cancel and remove work that is no longer owned by a resident/current item.
    pub fn retain(&mut self, active: &BTreeSet<MarkdownImageCacheKey>) {
        self.loads.retain(|key, cancellation| {
            if active.contains(key) {
                true
            } else {
                cancellation.cancel();
                false
            }
        });
    }

    /// Cancel and remove work whose source or owning item changed.
    pub fn cancel(&mut self, key: &MarkdownImageCacheKey) {
        if let Some(cancellation) = self.loads.remove(key) {
            cancellation.cancel();
        }
    }

    /// Cancel and remove every active load.
    pub fn cancel_all(&mut self) {
        for cancellation in self.loads.values() {
            cancellation.cancel();
        }
        self.loads.clear();
    }

    /// Return active in-flight work count.
    #[must_use]
    pub fn len(&self) -> usize {
        self.loads.len()
    }

    /// Return whether no work is active.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.loads.is_empty()
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
        MarkdownImageCancellationToken, MarkdownImageError, MarkdownImageInflight,
        MarkdownImageLoadDecision, MarkdownImageLoadError, MarkdownImageLoadFailure,
        MarkdownImageLoadGuard, MarkdownImageLoader, MarkdownImagePresentationInput,
        MarkdownImagePresentationPolicy, MarkdownImagePresentationState,
        MarkdownImagePresentationStore, MarkdownImageResidency, RESERVED_IMAGE_ROWS,
        decode_markdown_image, markdown_image_load_decision, markdown_image_reserved_rows,
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
        MarkdownImageCacheKey::new(&format!("image-{index}"), "context")
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
        let removed = inflight.cancellation_token(&key(1)).unwrap();
        let retained = inflight.cancellation_token(&key(2)).unwrap();

        inflight.retain(&BTreeSet::from([key(2)]));

        assert!(removed.is_cancelled());
        assert!(!retained.is_cancelled());
        assert!(inflight.cancellation_token(&key(1)).is_none());
        assert!(inflight.start(key(1)));
        inflight.finish(&key(1));
        inflight.finish(&key(2));
        assert!(inflight.is_empty());
    }

    #[test]
    fn changed_and_all_inflight_work_are_actively_cancelled() {
        let mut inflight = MarkdownImageInflight::default();
        assert!(inflight.start(key(1)));
        assert!(inflight.start(key(2)));
        let changed = inflight.cancellation_token(&key(1)).unwrap();
        let remaining = inflight.cancellation_token(&key(2)).unwrap();

        inflight.cancel(&key(1));
        assert!(changed.is_cancelled());
        assert!(!remaining.is_cancelled());
        assert_eq!(inflight.len(), 1);

        inflight.cancel_all();
        assert!(remaining.is_cancelled());
        assert!(inflight.is_empty());
    }

    #[test]
    fn decoded_cache_key_is_independent_of_viewport_and_terminal_protocol() {
        assert_eq!(
            MarkdownImageCacheKey::new("https://example.com/image.png", "document"),
            MarkdownImageCacheKey::new("https://example.com/image.png", "document")
        );
        assert_ne!(
            MarkdownImageCacheKey::new("https://example.com/image.png", "document"),
            MarkdownImageCacheKey::new("https://example.com/other.png", "document")
        );
    }

    #[test]
    fn every_presentation_state_reserves_identical_rows_before_and_after_loading() {
        let image = image(1, 4);
        let states = [
            MarkdownImagePresentationState::Idle,
            MarkdownImagePresentationState::Loading,
            MarkdownImagePresentationState::Ready(image),
            MarkdownImagePresentationState::Failed("failure".to_owned()),
            MarkdownImagePresentationState::NetworkDisabled,
            MarkdownImagePresentationState::TerminalUnsupported,
        ];
        assert!(
            states
                .iter()
                .all(|state| { markdown_image_reserved_rows(state) == RESERVED_IMAGE_ROWS })
        );
    }

    #[test]
    fn load_completion_updates_shared_owners_cache_and_failure_state() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        let key = MarkdownImageCacheKey::new("image.png", "document");
        let inputs = ["first", "second"].map(|id| MarkdownImagePresentationInput {
            contribution_id: id.to_owned(),
            cache_key: key.clone(),
            destination: remote.clone(),
            residency: MarkdownImageResidency::Visible,
        });
        let mut store = MarkdownImagePresentationStore::default();
        let mut inflight = MarkdownImageInflight::default();
        let mut cache = MarkdownImageCache::default();
        store.reconcile_with_inflight(&inputs, policy, &mut inflight);
        let requests = store.schedule_loads(&inputs, policy, &mut inflight);
        assert_eq!(requests.len(), 1);
        store.complete_load(&key, Ok(image(7, 4)), &mut cache, &mut inflight);
        assert!(matches!(
            store.state("first"),
            Some(MarkdownImagePresentationState::Ready(_))
        ));
        assert!(matches!(
            store.state("second"),
            Some(MarkdownImagePresentationState::Ready(_))
        ));
        assert!(cache.get(&key).is_some());
        assert!(inflight.cancellation_token(&key).is_none());

        let failed_key = MarkdownImageCacheKey::new("failed.png", "document");
        let failed = [MarkdownImagePresentationInput {
            contribution_id: "failed".to_owned(),
            cache_key: failed_key.clone(),
            destination: remote,
            residency: MarkdownImageResidency::Visible,
        }];
        store.reconcile_with_inflight(&failed, policy, &mut inflight);
        assert_eq!(
            store.schedule_loads(&failed, policy, &mut inflight).len(),
            1
        );
        store.complete_load(
            &failed_key,
            Err(MarkdownImageLoadFailure::Load(
                MarkdownImageLoadError::TimedOut,
            )),
            &mut cache,
            &mut inflight,
        );
        assert!(matches!(
            store.state("failed"),
            Some(MarkdownImagePresentationState::Failed(message)) if message.contains("timed out")
        ));
    }

    #[test]
    fn reconstruction_reuses_decoded_cache_without_scheduling_new_work() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        let key = MarkdownImageCacheKey::new("image.png", "document");
        let input = [MarkdownImagePresentationInput {
            contribution_id: "reconstructed".to_owned(),
            cache_key: key.clone(),
            destination: remote,
            residency: MarkdownImageResidency::Visible,
        }];
        let mut cache = MarkdownImageCache::default();
        cache.insert(key, image(9, 4));
        let mut store = MarkdownImagePresentationStore::default();
        let mut inflight = MarkdownImageInflight::default();
        store.reconcile_with_inflight(&input, policy, &mut inflight);
        store.hydrate_from_cache(&mut cache);

        assert!(matches!(
            store.state("reconstructed"),
            Some(MarkdownImagePresentationState::Ready(_))
        ));
        assert!(
            store
                .schedule_loads(&input, policy, &mut inflight)
                .is_empty()
        );
    }

    #[test]
    fn ready_store_emits_stable_clipped_bmux_frame_contribution() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        let input = MarkdownImagePresentationInput {
            contribution_id: "owner:image:1".to_owned(),
            cache_key: MarkdownImageCacheKey::new("image.png", "document"),
            destination: remote,
            residency: MarkdownImageResidency::Visible,
        };
        let mut store = MarkdownImagePresentationStore::default();
        store.reconcile(&[input], policy);
        store
            .state_mut("owner:image:1")
            .expect("resident state")
            .ready(DecodedMarkdownImage {
                width: 2,
                height: 2,
                rgba: vec![255; 16],
            });
        let terminal = Rect::new(0, 0, 20, 10);
        let destination = Rect::new(4, 3, 8, 4);
        let clip = Rect::new(6, 4, 3, 2);
        let mut buffer = bmux_tui::buffer::Buffer::empty(terminal);
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);

        assert!(store.present_ready("owner:image:1", destination, clip, &mut frame));
        let [ImageContribution::Present(placement)] = frame.images() else {
            panic!("expected one presented image");
        };
        assert_eq!(placement.key.as_str(), "markdown:owner:image:1");
        assert_eq!(placement.destination, destination);
        assert_eq!(placement.clip, clip);
        assert!(!store.present_ready(
            "owner:image:1",
            Rect::new(15, 8, 2, 2),
            Rect::new(0, 0, 1, 1),
            &mut frame,
        ));
        MarkdownImagePresentationStore::remove_from_frame("owner:image:1", &mut frame);
        assert!(matches!(
            frame.images().last(),
            Some(ImageContribution::Remove(key)) if key.as_str() == "markdown:owner:image:1"
        ));
    }

    #[test]
    fn scheduler_starts_only_visible_or_bounded_prefetch_work_and_honors_network_policy() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let local = MarkdownDestination::LocalPath(std::path::PathBuf::from("/trusted/image.png"));
        let input =
            |id: &str, source: &str, destination, residency| MarkdownImagePresentationInput {
                contribution_id: id.to_owned(),
                cache_key: MarkdownImageCacheKey::new(source, "document"),
                destination,
                residency,
            };
        let inputs = vec![
            input(
                "hidden",
                "hidden.png",
                local.clone(),
                MarkdownImageResidency::Hidden,
            ),
            input(
                "visible",
                "visible.png",
                local,
                MarkdownImageResidency::Visible,
            ),
            input(
                "prefetch",
                "prefetch.png",
                remote.clone(),
                MarkdownImageResidency::Prefetch,
            ),
        ];
        let mut store = MarkdownImagePresentationStore::default();
        let mut inflight = MarkdownImageInflight::default();
        let enabled = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        store.reconcile_with_inflight(&inputs, enabled, &mut inflight);
        let requests = store.schedule_loads(&inputs, enabled, &mut inflight);
        assert_eq!(
            requests
                .iter()
                .map(|request| request.contribution_id.as_str())
                .collect::<Vec<_>>(),
            ["visible", "prefetch"]
        );
        assert_eq!(
            store.state("hidden"),
            Some(&MarkdownImagePresentationState::Idle)
        );

        let remote_only = [input(
            "network-disabled",
            "remote.png",
            remote,
            MarkdownImageResidency::Visible,
        )];
        let disabled = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: false,
            terminal_supported: true,
        };
        store.reconcile_with_inflight(&remote_only, disabled, &mut inflight);
        assert!(
            store
                .schedule_loads(&remote_only, disabled, &mut inflight)
                .is_empty()
        );
        assert_eq!(
            store.state("network-disabled"),
            Some(&MarkdownImagePresentationState::NetworkDisabled)
        );
        assert!(
            requests
                .iter()
                .all(|request| request.cancellation.is_cancelled())
        );
        assert!(
            inflight
                .cancellation_token(&requests[0].cache_key)
                .is_none()
        );
    }

    #[test]
    fn presentation_store_preserves_current_state_and_drops_replaced_or_removed_owners() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        let input = |id: &str, source: &str| MarkdownImagePresentationInput {
            contribution_id: id.to_owned(),
            cache_key: MarkdownImageCacheKey::new(source, "document"),
            destination: remote.clone(),
            residency: MarkdownImageResidency::Visible,
        };
        let mut store = MarkdownImagePresentationStore::default();
        store.reconcile(&[input("owner:image:1", "a.png")], policy);
        assert_eq!(store.len(), 1);
        assert!(
            store
                .state_mut("owner:image:1")
                .expect("resident state")
                .start_loading(&remote, policy)
        );

        // Scroll/resize only changes BMUX placement, not decoded-payload state.
        store.reconcile(&[input("owner:image:1", "a.png")], policy);
        assert_eq!(
            store.state("owner:image:1"),
            Some(&MarkdownImagePresentationState::Loading)
        );

        // A changed source/cache identity resets state.
        store.reconcile(&[input("owner:image:1", "changed.png")], policy);
        assert_eq!(
            store.state("owner:image:1"),
            Some(&MarkdownImagePresentationState::Idle)
        );

        store.reconcile(&[input("replacement:image:1", "changed.png")], policy);
        assert!(store.state("owner:image:1").is_none());
        assert_eq!(store.len(), 1);
        store.reconcile(&[], policy);
        assert!(store.is_empty());
    }

    #[test]
    fn presentation_state_is_capability_aware_and_preserves_fallback_context() {
        let remote = resolve_markdown_destination("https://example.com/image.png", None);
        let local = MarkdownDestination::LocalPath(std::path::PathBuf::from("/trusted/image.png"));
        let enabled = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };

        let mut state = MarkdownImagePresentationState::initial(&remote, enabled);
        assert_eq!(state, MarkdownImagePresentationState::Idle);
        let history_policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: false,
            network_enabled: true,
            terminal_supported: true,
        };
        let mut history_state = MarkdownImagePresentationState::initial(&remote, history_policy);
        assert!(!history_state.start_loading(&remote, history_policy));
        assert_eq!(history_state, MarkdownImagePresentationState::Idle);
        assert!(state.start_loading(&remote, enabled));
        assert_eq!(state, MarkdownImagePresentationState::Loading);
        state.ready(DecodedMarkdownImage {
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        });
        assert!(matches!(state, MarkdownImagePresentationState::Ready(_)));

        assert_eq!(
            MarkdownImagePresentationState::initial(
                &remote,
                MarkdownImagePresentationPolicy {
                    interactive_resident_frame: true,
                    network_enabled: false,
                    terminal_supported: true,
                },
            ),
            MarkdownImagePresentationState::NetworkDisabled
        );
        assert_eq!(
            MarkdownImagePresentationState::initial(
                &local,
                MarkdownImagePresentationPolicy {
                    interactive_resident_frame: true,
                    network_enabled: true,
                    terminal_supported: false,
                },
            ),
            MarkdownImagePresentationState::TerminalUnsupported
        );

        let mut failed = MarkdownImagePresentationState::Loading;
        failed.failed(&MarkdownImageLoadFailure::Load(
            MarkdownImageLoadError::TimedOut,
        ));
        let fallback = failed.fallback("diagram", &remote);
        assert!(fallback.contains("diagram"));
        assert!(fallback.contains("image load timed out"));
        assert!(fallback.contains("https://example.com/image.png"));

        let secret = MarkdownDestination::Inert {
            reason: MarkdownDestinationRejection::UnsupportedScheme,
        };
        let inert_fallback = MarkdownImagePresentationState::Idle.fallback("private", &secret);
        assert_eq!(inert_fallback, "[private — image idle]");
        assert!(!inert_fallback.contains("source:"));

        let destination = resolve_markdown_destination("https://example.com/build", None);
        let badge = MarkdownImagePresentationState::TerminalUnsupported.linked_badge_fallback(
            "Build",
            &remote,
            Some(&destination),
        );
        assert!(badge.contains("Build"));
        assert!(badge.contains("https://example.com/image.png"));
        assert!(badge.ends_with("→ https://example.com/build"));
        let unsafe_destination = MarkdownDestination::Inert {
            reason: MarkdownDestinationRejection::UnsupportedScheme,
        };
        assert!(
            !MarkdownImagePresentationState::Idle
                .linked_badge_fallback("Build", &remote, Some(&unsafe_destination))
                .contains('→')
        );
    }

    #[test]
    fn rejected_destinations_never_enter_loading_state() {
        let destination = MarkdownDestination::UnresolvedRelative("image.png".to_owned());
        let policy = MarkdownImagePresentationPolicy {
            interactive_resident_frame: true,
            network_enabled: true,
            terminal_supported: true,
        };
        let mut state = MarkdownImagePresentationState::initial(&destination, policy);

        assert!(!state.start_loading(&destination, policy));
        assert_eq!(state, MarkdownImagePresentationState::Idle);
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
