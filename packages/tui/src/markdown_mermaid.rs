//! Stable Mermaid diagram presentation lifecycle for the transcript TUI.

use std::collections::{BTreeMap, BTreeSet};

use bcode_mermaid_render::{
    MermaidCancellationToken, MermaidRenderCache, MermaidRenderError, MermaidRendered,
    MermaidRenderedOutput,
};
use bmux_tui::geometry::Rect;
use bmux_tui::image::{
    ImageContribution, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat, ImagePlacement,
};

/// Maximum resident off-screen Mermaid contributions eligible for prefetch.
pub const MAX_MERMAID_PREFETCH: usize = 4;

/// Fixed rows reserved before and after Mermaid rendering.
pub const RESERVED_MERMAID_ROWS: u16 = 8;

/// Runtime Mermaid presentation state owned by the interactive chat loop.
#[derive(Debug)]
pub struct MarkdownMermaidRuntime {
    /// Resident per-contribution lifecycle state.
    pub presentations: MarkdownMermaidPresentationStore,
    /// Bounded successful render cache.
    pub cache: MermaidRenderCache,
    /// Process-isolated renderer adapter.
    pub renderer: bcode_mermaid_render::IsolatedMermaidRenderer,
}

impl MarkdownMermaidRuntime {
    /// Create runtime state using the renderer packaged beside Bcode.
    ///
    /// # Errors
    ///
    /// Returns a typed unavailable error when packaged renderer discovery fails.
    pub fn packaged() -> Result<Self, MermaidRenderError> {
        Ok(Self {
            presentations: MarkdownMermaidPresentationStore::default(),
            cache: MermaidRenderCache::default(),
            renderer: bcode_mermaid_render::IsolatedMermaidRenderer::packaged()?,
        })
    }

    /// Reconcile resident owners, hydrate cache hits, and start newly eligible renders.
    ///
    /// Returns removed contribution IDs and blocking worker tasks. Callers must
    /// emit removals in the next BMUX frame and poll every returned task.
    pub fn reconcile_and_start(
        &mut self,
        inputs: &[MarkdownMermaidInput],
        terminal_supported: bool,
    ) -> (
        Vec<String>,
        Vec<tokio::task::JoinHandle<MarkdownMermaidCompletion>>,
    ) {
        let removed = self.presentations.reconcile(inputs, terminal_supported);
        self.presentations.hydrate_from_cache(&mut self.cache);
        let work = self.presentations.schedule(inputs);
        let tasks = spawn_mermaid_work(&self.renderer, work);
        (removed, tasks)
    }

    /// Apply one asynchronous worker completion if its owner is still current.
    pub fn complete(&mut self, completion: MarkdownMermaidCompletion) -> bool {
        self.presentations
            .complete(&completion.cache_key, completion.result, &mut self.cache)
    }

    /// Cancel all active workers before the interactive runtime is dropped.
    pub fn cancel_all(&mut self) {
        self.presentations.reconcile(&[], true);
    }
}

/// Rasterized Mermaid image suitable for BMUX protocol-neutral transport.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodedMermaidImage {
    /// Pixel width.
    pub width: u32,
    /// Pixel height.
    pub height: u32,
    /// RGBA8 pixels.
    pub rgba: Vec<u8>,
}

impl DecodedMermaidImage {
    fn contribution(
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

/// SVG rasterization failure.
#[derive(Debug, thiserror::Error)]
pub enum MermaidImageError {
    /// SVG could not be parsed safely.
    #[error("invalid Mermaid SVG: {0}")]
    InvalidSvg(String),
    /// SVG dimensions are invalid or exceed fixed limits.
    #[error("Mermaid SVG dimensions are invalid")]
    InvalidDimensions,
    /// Pixel allocation failed.
    #[error("Mermaid image allocation failed")]
    AllocationFailed,
}

/// Convert successful worker SVG into bounded RGBA pixels for BMUX.
///
/// # Errors
///
/// Returns an error for malformed SVG, invalid dimensions, excessive decoded
/// pixels, or failed pixel allocation.
pub fn rasterize_mermaid_svg(svg: &[u8]) -> Result<DecodedMermaidImage, MermaidImageError> {
    const MAX_DIMENSION: u32 = 4096;
    const MAX_PIXELS: u64 = 16_000_000;

    let tree = resvg::usvg::Tree::from_data(svg, &resvg::usvg::Options::default())
        .map_err(|error| MermaidImageError::InvalidSvg(error.to_string()))?;
    let size = tree.size().to_int_size();
    let width = size.width();
    let height = size.height();
    if width == 0
        || height == 0
        || width > MAX_DIMENSION
        || height > MAX_DIMENSION
        || u64::from(width).saturating_mul(u64::from(height)) > MAX_PIXELS
    {
        return Err(MermaidImageError::InvalidDimensions);
    }
    let mut pixmap =
        resvg::tiny_skia::Pixmap::new(width, height).ok_or(MermaidImageError::AllocationFailed)?;
    resvg::render(
        &tree,
        resvg::tiny_skia::Transform::identity(),
        &mut pixmap.as_mut(),
    );
    Ok(DecodedMermaidImage {
        width,
        height,
        rgba: pixmap.take(),
    })
}

/// Per-contribution Mermaid presentation state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MarkdownMermaidPresentationState {
    /// Rendering has not started.
    Idle,
    /// A bounded worker is active.
    Rendering,
    /// Successful RGBA pixels are ready for BMUX image transport.
    Ready(DecodedMermaidImage),
    /// Rendering failed with a visible typed diagnostic.
    Failed(String),
    /// BMUX cannot present images on this terminal.
    TerminalUnsupported,
}

impl MarkdownMermaidPresentationState {
    /// Return stable row reservation for every state.
    #[must_use]
    pub const fn reserved_rows(&self) -> u16 {
        RESERVED_MERMAID_ROWS
    }

    /// Return source-preserving terminal fallback text.
    #[must_use]
    pub fn fallback(&self, source: &str) -> String {
        match self {
            Self::Idle => format!("[Mermaid diagram idle]\n{source}"),
            Self::Rendering => format!("[Rendering Mermaid diagram…]\n{source}"),
            Self::Ready(_) => source.to_owned(),
            Self::Failed(diagnostic) => {
                format!("[Mermaid rendering failed: {diagnostic}]\n{source}")
            }
            Self::TerminalUnsupported => {
                format!("[Mermaid image unsupported; source follows]\n{source}")
            }
        }
    }
}

/// One resident Mermaid contribution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MarkdownMermaidInput {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Versioned renderer request cache key.
    pub cache_key: String,
    /// Source retained for fallback/source-view activation.
    pub source: String,
    /// Whether the contribution currently intersects the viewport.
    pub visible: bool,
    /// Whether the contribution is in the caller's bounded prefetch projection.
    pub prefetch: bool,
}

#[derive(Debug)]
struct MermaidEntry {
    cache_key: String,
    source: String,
    state: MarkdownMermaidPresentationState,
}

/// Resident Mermaid state and cancellation ownership.
#[derive(Debug, Default)]
pub struct MarkdownMermaidPresentationStore {
    entries: BTreeMap<String, MermaidEntry>,
    workers: BTreeMap<String, MermaidCancellationToken>,
}

impl MarkdownMermaidPresentationStore {
    /// Reconcile the bounded resident projection, cancel stale workers, and return removed owners.
    pub fn reconcile(
        &mut self,
        inputs: &[MarkdownMermaidInput],
        terminal_supported: bool,
    ) -> Vec<String> {
        let previous = self.entries.keys().cloned().collect::<BTreeSet<_>>();
        let active_ids = inputs
            .iter()
            .map(|input| input.contribution_id.clone())
            .collect::<BTreeSet<_>>();
        let active_keys = inputs
            .iter()
            .map(|input| input.cache_key.as_str())
            .collect::<BTreeSet<_>>();
        let mut removed = previous
            .difference(&active_ids)
            .cloned()
            .collect::<BTreeSet<_>>();
        self.entries.retain(|id, _| active_ids.contains(id));
        self.workers.retain(|key, cancellation| {
            if active_keys.contains(key.as_str()) {
                true
            } else {
                cancellation.cancel();
                false
            }
        });
        for input in inputs {
            let reset = self
                .entries
                .get(&input.contribution_id)
                .is_none_or(|entry| entry.cache_key != input.cache_key);
            if reset {
                if self.entries.contains_key(&input.contribution_id) {
                    removed.insert(input.contribution_id.clone());
                }
                self.entries.insert(
                    input.contribution_id.clone(),
                    MermaidEntry {
                        cache_key: input.cache_key.clone(),
                        source: input.source.clone(),
                        state: if terminal_supported {
                            MarkdownMermaidPresentationState::Idle
                        } else {
                            MarkdownMermaidPresentationState::TerminalUnsupported
                        },
                    },
                );
            } else if !terminal_supported {
                if let Some(cancellation) = self.workers.remove(&input.cache_key) {
                    cancellation.cancel();
                }
                if let Some(entry) = self.entries.get_mut(&input.contribution_id) {
                    entry.state = MarkdownMermaidPresentationState::TerminalUnsupported;
                }
            } else if self
                .entries
                .get(&input.contribution_id)
                .is_some_and(|entry| {
                    matches!(
                        entry.state,
                        MarkdownMermaidPresentationState::TerminalUnsupported
                    )
                })
                && let Some(entry) = self.entries.get_mut(&input.contribution_id)
            {
                entry.state = MarkdownMermaidPresentationState::Idle;
            }
        }
        removed.into_iter().collect()
    }

    /// Start workers only for visible/bounded-prefetch idle contributions.
    #[must_use]
    pub fn schedule(&mut self, inputs: &[MarkdownMermaidInput]) -> Vec<MarkdownMermaidWork> {
        let mut work = Vec::new();
        let mut prefetched = 0_usize;
        for input in inputs.iter().filter(|input| {
            if input.visible {
                true
            } else if input.prefetch && prefetched < MAX_MERMAID_PREFETCH {
                prefetched = prefetched.saturating_add(1);
                true
            } else {
                false
            }
        }) {
            let Some(entry) = self.entries.get_mut(&input.contribution_id) else {
                continue;
            };
            if !matches!(entry.state, MarkdownMermaidPresentationState::Idle) {
                continue;
            }
            if self.workers.contains_key(&entry.cache_key) {
                continue;
            }
            let cancellation = MermaidCancellationToken::default();
            self.workers
                .insert(entry.cache_key.clone(), cancellation.clone());
            entry.state = MarkdownMermaidPresentationState::Rendering;
            work.push(MarkdownMermaidWork {
                contribution_id: input.contribution_id.clone(),
                cache_key: entry.cache_key.clone(),
                source: entry.source.clone(),
                cancellation,
            });
        }
        work
    }

    /// Hydrate unchanged requests from the bounded Mermaid cache.
    pub fn hydrate_from_cache(&mut self, cache: &mut MermaidRenderCache) {
        for entry in self
            .entries
            .values_mut()
            .filter(|entry| matches!(entry.state, MarkdownMermaidPresentationState::Idle))
        {
            if let Some(rendered) = cache.get(&entry.cache_key) {
                let MermaidRenderedOutput::Svg(svg) = rendered.output;
                if let Ok(image) = rasterize_mermaid_svg(&svg) {
                    entry.state = MarkdownMermaidPresentationState::Ready(image);
                }
            }
        }
    }

    /// Complete one worker and update every owner sharing its cache key.
    ///
    /// Returns `true` when the completed key still belonged to resident state.
    /// Late results for cancelled or replaced keys are discarded without
    /// repopulating the cache.
    pub fn complete(
        &mut self,
        key: &str,
        result: Result<MermaidRendered, MermaidRenderError>,
        cache: &mut MermaidRenderCache,
    ) -> bool {
        if self.workers.remove(key).is_none() {
            return false;
        }
        match result {
            Ok(rendered) => {
                cache.insert(rendered.clone());
                let MermaidRenderedOutput::Svg(svg) = rendered.output;
                match rasterize_mermaid_svg(&svg) {
                    Ok(image) => {
                        for entry in self
                            .entries
                            .values_mut()
                            .filter(|entry| entry.cache_key == key)
                        {
                            entry.state = MarkdownMermaidPresentationState::Ready(image.clone());
                        }
                    }
                    Err(error) => {
                        for entry in self
                            .entries
                            .values_mut()
                            .filter(|entry| entry.cache_key == key)
                        {
                            entry.state =
                                MarkdownMermaidPresentationState::Failed(error.to_string());
                        }
                    }
                }
            }
            Err(error) => {
                for entry in self
                    .entries
                    .values_mut()
                    .filter(|entry| entry.cache_key == key)
                {
                    entry.state = MarkdownMermaidPresentationState::Failed(error.to_string());
                }
            }
        }
        true
    }

    /// Return state for one resident contribution.
    #[must_use]
    pub fn state(&self, contribution_id: &str) -> Option<&MarkdownMermaidPresentationState> {
        self.entries.get(contribution_id).map(|entry| &entry.state)
    }

    /// Return the number of active worker cache keys.
    #[must_use]
    pub fn active_worker_count(&self) -> usize {
        self.workers.len()
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

    /// Return source for source-view activation.
    #[must_use]
    pub fn source(&self, contribution_id: &str) -> Option<&str> {
        self.entries
            .get(contribution_id)
            .map(|entry| entry.source.as_str())
    }

    /// Return readable source-preserving fallback text for one contribution.
    #[must_use]
    pub fn fallback(&self, contribution_id: &str) -> Option<String> {
        let entry = self.entries.get(contribution_id)?;
        Some(entry.state.fallback(&entry.source))
    }

    /// Emit explicit removal for a stable diagram key no longer present in this frame.
    pub fn remove_from_frame(contribution_id: &str, frame: &mut bmux_tui::frame::Frame<'_>) {
        frame.push_image(ImageContribution::Remove(ImageKey::new(format!(
            "markdown-mermaid:{contribution_id}"
        ))));
    }

    /// Return clipped BMUX destination geometry for a ready diagram.
    #[must_use]
    pub fn ready_placement(
        &self,
        contribution_id: &str,
        destination: Rect,
        clip: Rect,
    ) -> Option<ImageContribution> {
        let MarkdownMermaidPresentationState::Ready(image) = self.state(contribution_id)? else {
            return None;
        };
        (!destination.intersection(clip).is_empty()).then(|| {
            image.contribution(
                format!("markdown-mermaid:{contribution_id}"),
                destination,
                clip,
            )
        })
    }
}

/// Complete one scheduled process-isolated Mermaid render.
#[derive(Debug)]
pub struct MarkdownMermaidCompletion {
    /// Versioned request cache key.
    pub cache_key: String,
    /// Typed renderer result.
    pub result: Result<MermaidRendered, MermaidRenderError>,
}

/// Start scheduled Mermaid work on blocking tasks outside the frame renderer.
#[must_use]
pub fn spawn_mermaid_work(
    renderer: &bcode_mermaid_render::IsolatedMermaidRenderer,
    work: Vec<MarkdownMermaidWork>,
) -> Vec<tokio::task::JoinHandle<MarkdownMermaidCompletion>> {
    work.into_iter()
        .map(|work| {
            let renderer = renderer.clone();
            tokio::task::spawn_blocking(move || {
                let request =
                    bcode_mermaid_render::MermaidRenderRequest::svg(work.source, 1600, 1200);
                let cache_key = work.cache_key;
                let result = if request.cache_key() == cache_key {
                    renderer.render(&request, &work.cancellation)
                } else {
                    Err(MermaidRenderError::InvalidWorkerResponse {
                        message: "scheduled Mermaid cache key does not match its source".to_owned(),
                    })
                };
                MarkdownMermaidCompletion { cache_key, result }
            })
        })
        .collect()
}

/// One newly scheduled Mermaid worker request.
#[derive(Debug, Clone)]
pub struct MarkdownMermaidWork {
    /// Stable owner-qualified contribution identity.
    pub contribution_id: String,
    /// Versioned request cache key.
    pub cache_key: String,
    /// Diagram source.
    pub source: String,
    /// Cancellation tied to resident ownership.
    pub cancellation: MermaidCancellationToken,
}

#[cfg(test)]
mod tests {
    use bcode_mermaid_render::{
        MermaidRenderCache, MermaidRenderError, MermaidRenderRequest, MermaidRendered,
        MermaidRenderedOutput,
    };
    use bmux_tui::buffer::Buffer;
    use bmux_tui::frame::Frame;
    use bmux_tui::geometry::Rect;
    use bmux_tui::image::{ImageContribution, ImagePayload, ImagePixelFormat};

    use super::{
        MAX_MERMAID_PREFETCH, MarkdownMermaidInput, MarkdownMermaidPresentationState,
        MarkdownMermaidPresentationStore, MarkdownMermaidRuntime, RESERVED_MERMAID_ROWS,
        spawn_mermaid_work,
    };

    fn input(id: &str, key: &str, may_render: bool) -> MarkdownMermaidInput {
        MarkdownMermaidInput {
            contribution_id: id.to_owned(),
            cache_key: key.to_owned(),
            source: "flowchart LR\nA --> B".to_owned(),
            visible: may_render,
            prefetch: false,
        }
    }

    fn rendered(key: &str) -> MermaidRendered {
        MermaidRendered {
            output: MermaidRenderedOutput::Svg(
                br#"<svg xmlns="http://www.w3.org/2000/svg" width="2" height="2"><rect width="2" height="2" fill="red"/></svg>"#.to_vec(),
            ),
            cache_key: key.to_owned(),
            diagnostics: Vec::new(),
        }
    }

    #[test]
    fn large_history_metrics_and_trimming_keep_workers_state_and_cache_bounded() {
        let inputs = (0..1_000)
            .map(|index| MarkdownMermaidInput {
                contribution_id: format!("diagram-{index}"),
                cache_key: format!("key-{index}"),
                source: "flowchart LR\nA --> B".to_owned(),
                visible: false,
                prefetch: true,
            })
            .collect::<Vec<_>>();
        let mut store = MarkdownMermaidPresentationStore::default();
        let mut cache = MermaidRenderCache::default();
        store.reconcile(&inputs, true);
        let work = store.schedule(&inputs);
        for request in &work {
            cache.insert(rendered(&request.cache_key));
        }
        assert_eq!(store.active_worker_count(), MAX_MERMAID_PREFETCH);
        assert_eq!(store.len(), 1_000);
        assert!(cache.len() <= bcode_mermaid_render::MAX_CACHE_ENTRIES);
        assert!(cache.payload_bytes() <= bcode_mermaid_render::MAX_CACHE_BYTES);

        let retained = &inputs[900..];
        store.reconcile(retained, true);
        assert!(
            work.iter()
                .all(|request| request.cancellation.is_cancelled())
        );
        assert_eq!(store.active_worker_count(), 0);
        assert_eq!(store.len(), retained.len());
    }

    #[test]
    fn mermaid_prefetch_is_bounded_independently_of_resident_history_size() {
        let inputs = (0..1_000)
            .map(|index| MarkdownMermaidInput {
                contribution_id: format!("diagram-{index}"),
                cache_key: format!("key-{index}"),
                source: "flowchart LR\nA --> B".to_owned(),
                visible: false,
                prefetch: true,
            })
            .collect::<Vec<_>>();
        let mut store = MarkdownMermaidPresentationStore::default();
        store.reconcile(&inputs, true);

        assert_eq!(store.schedule(&inputs).len(), MAX_MERMAID_PREFETCH);
    }

    #[test]
    fn lifecycle_is_stable_bounded_and_source_preserving() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let inputs = [input("hidden", "k1", false), input("visible", "k2", true)];
        store.reconcile(&inputs, true);
        assert_eq!(store.schedule(&inputs).len(), 1);
        assert_eq!(
            store.state("hidden"),
            Some(&MarkdownMermaidPresentationState::Idle)
        );
        assert_eq!(
            store.state("visible"),
            Some(&MarkdownMermaidPresentationState::Rendering)
        );
        let mut cache = MermaidRenderCache::default();
        assert!(store.complete("k2", Ok(rendered("k2")), &mut cache));
        assert!(matches!(
            store.state("visible"),
            Some(MarkdownMermaidPresentationState::Ready(_))
        ));
        assert_eq!(
            store.state("visible").expect("state").reserved_rows(),
            RESERVED_MERMAID_ROWS
        );
        assert_eq!(store.source("visible"), Some("flowchart LR\nA --> B"));
        let contribution = store
            .ready_placement("visible", Rect::new(2, 3, 20, 8), Rect::new(4, 4, 10, 4))
            .expect("ready placement");
        let ImageContribution::Present(placement) = contribution else {
            panic!("expected presented Mermaid image");
        };
        assert_eq!(placement.key.as_str(), "markdown-mermaid:visible");
        assert!(matches!(
            placement.payload,
            ImagePayload::Pixels {
                width: 2,
                height: 2,
                format: ImagePixelFormat::Rgba8,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn runtime_reconcile_starts_and_completes_current_worker_failure() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 1600, 1200);
        let inputs = [input("diagram", &request.cache_key(), true)];
        let mut runtime = MarkdownMermaidRuntime {
            presentations: MarkdownMermaidPresentationStore::default(),
            cache: MermaidRenderCache::default(),
            renderer: bcode_mermaid_render::IsolatedMermaidRenderer::from_path(
                "/definitely/missing/diagram-renderer",
            ),
        };
        let (removed, tasks) = runtime.reconcile_and_start(&inputs, true);
        assert!(removed.is_empty());
        let [task] = tasks.try_into().expect("one worker task");

        assert!(runtime.complete(task.await.unwrap()));
        assert!(matches!(
            runtime.presentations.state("diagram"),
            Some(MarkdownMermaidPresentationState::Failed(_))
        ));
    }

    #[tokio::test]
    async fn runtime_discards_worker_completion_after_owner_removal() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 1600, 1200);
        let inputs = [input("diagram", &request.cache_key(), true)];
        let mut runtime = MarkdownMermaidRuntime {
            presentations: MarkdownMermaidPresentationStore::default(),
            cache: MermaidRenderCache::default(),
            renderer: bcode_mermaid_render::IsolatedMermaidRenderer::from_path(
                "/definitely/missing/diagram-renderer",
            ),
        };
        let (_, tasks) = runtime.reconcile_and_start(&inputs, true);
        let [task] = tasks.try_into().expect("one worker task");
        let (removed, _) = runtime.reconcile_and_start(&[], true);
        assert_eq!(removed, ["diagram"]);

        assert!(!runtime.complete(task.await.unwrap()));
        assert!(runtime.cache.is_empty());
    }

    #[tokio::test]
    async fn isolated_worker_start_failure_completes_as_typed_visible_diagnostic() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 1600, 1200);
        let inputs = [input("diagram", &request.cache_key(), true)];
        let mut store = MarkdownMermaidPresentationStore::default();
        store.reconcile(&inputs, true);
        let work = store.schedule(&inputs);
        let renderer = bcode_mermaid_render::IsolatedMermaidRenderer::from_path(
            "/definitely/missing/diagram-renderer",
        );
        let [task] = spawn_mermaid_work(&renderer, work)
            .try_into()
            .expect("one worker task");
        let completion = task.await.expect("worker task");
        let mut cache = MermaidRenderCache::default();
        assert!(store.complete(&completion.cache_key, completion.result, &mut cache));

        let state = store.state("diagram").expect("diagram state");
        assert!(matches!(state, MarkdownMermaidPresentationState::Failed(_)));
        assert!(state.fallback("flowchart").contains("worker unavailable"));
    }

    #[test]
    fn store_fallback_preserves_typed_mermaid_failure_and_source() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let inputs = [input("diagram", "key", true)];
        store.reconcile(&inputs, true);
        assert_eq!(store.schedule(&inputs).len(), 1);
        let mut cache = MermaidRenderCache::default();
        assert!(store.complete(
            "key",
            Err(MermaidRenderError::InvalidDiagram {
                message: "invalid node".to_owned(),
            }),
            &mut cache,
        ));

        let fallback = store.fallback("diagram").expect("resident fallback");
        assert!(fallback.contains("invalid node"));
        assert!(fallback.contains("flowchart LR"));
    }

    #[test]
    fn removed_owner_emits_explicit_stable_bmux_removal() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 4));
        let mut frame = Frame::new(&mut buffer);

        MarkdownMermaidPresentationStore::remove_from_frame("owner:diagram:1", &mut frame);

        assert!(matches!(
            frame.images(),
            [ImageContribution::Remove(key)]
                if key.as_str() == "markdown-mermaid:owner:diagram:1"
        ));
    }

    #[test]
    fn late_cancelled_worker_completion_is_discarded_without_cache_repopulation() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let inputs = [input("diagram", "key", true)];
        store.reconcile(&inputs, true);
        assert_eq!(store.schedule(&inputs).len(), 1);
        store.reconcile(&[], true);
        let mut cache = MermaidRenderCache::default();

        assert!(!store.complete("key", Ok(rendered("key")), &mut cache));
        assert!(cache.is_empty());
    }

    #[test]
    fn stale_workers_cancel_cache_reuses_and_failures_stay_visible() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let active = [input("diagram", "key", true)];
        store.reconcile(&active, true);
        let work = store.schedule(&active).pop().expect("scheduled work");
        let removed = store.reconcile(&[], true);
        assert_eq!(removed, ["diagram"]);
        assert!(work.cancellation.is_cancelled());

        let mut cache = MermaidRenderCache::default();
        cache.insert(rendered("cached"));
        let cached = [input("reconstructed", "cached", true)];
        store.reconcile(&cached, true);
        store.hydrate_from_cache(&mut cache);
        assert!(matches!(
            store.state("reconstructed"),
            Some(MarkdownMermaidPresentationState::Ready(_))
        ));
        assert!(store.schedule(&cached).is_empty());

        let failed = [input("failed", "failed", true)];
        store.reconcile(&failed, true);
        assert_eq!(store.schedule(&failed).len(), 1);
        assert!(store.complete(
            "failed",
            Err(MermaidRenderError::InvalidDiagram {
                message: "bad diagram".to_owned(),
            }),
            &mut cache,
        ));
        let state = store.state("failed").expect("failure state");
        assert!(matches!(state, MarkdownMermaidPresentationState::Failed(_)));
        assert!(state.fallback("flowchart").contains("bad diagram"));
        assert!(state.fallback("flowchart").contains("flowchart"));
    }

    #[test]
    fn lifecycle_matrix_covers_replacement_resize_scroll_clip_and_unsupported_protocol() {
        let mut store = MarkdownMermaidPresentationStore::default();
        let initial = [input("diagram", "v1", true)];
        store.reconcile(&initial, true);
        assert_eq!(store.schedule(&initial).len(), 1);
        let mut cache = MermaidRenderCache::default();
        assert!(store.complete("v1", Ok(rendered("v1")), &mut cache));
        let unchanged = store.reconcile(&initial, true);
        assert!(unchanged.is_empty());

        let first = store
            .ready_placement("diagram", Rect::new(2, 3, 20, 8), Rect::new(4, 4, 10, 4))
            .expect("first placement");
        let ImageContribution::Present(first) = first else {
            panic!("expected image placement");
        };
        assert_eq!(first.destination, Rect::new(2, 3, 20, 8));
        assert_eq!(first.clip, Rect::new(4, 4, 10, 4));

        let moved = store
            .ready_placement("diagram", Rect::new(2, 9, 12, 8), Rect::new(0, 10, 8, 3))
            .expect("moved placement");
        let ImageContribution::Present(moved) = moved else {
            panic!("expected moved image placement");
        };
        assert_eq!(moved.key, first.key);
        assert_eq!(moved.destination, Rect::new(2, 9, 12, 8));
        assert_eq!(moved.clip, Rect::new(0, 10, 8, 3));

        let replacement = [input("diagram", "v2", true)];
        let removed = store.reconcile(&replacement, true);
        assert_eq!(removed, ["diagram"]);
        assert_eq!(
            store.state("diagram"),
            Some(&MarkdownMermaidPresentationState::Idle)
        );
        assert_eq!(store.schedule(&replacement).len(), 1);

        store.reconcile(&replacement, false);
        assert_eq!(
            store.state("diagram"),
            Some(&MarkdownMermaidPresentationState::TerminalUnsupported)
        );
        assert!(store.reconcile(&replacement, true).is_empty());
        assert_eq!(
            store.state("diagram"),
            Some(&MarkdownMermaidPresentationState::Idle)
        );
        store.reconcile(&replacement, false);
        assert!(
            store
                .ready_placement("diagram", Rect::new(0, 0, 4, 4), Rect::new(0, 0, 4, 4))
                .is_none()
        );
        assert!(
            store
                .state("diagram")
                .expect("unsupported state")
                .fallback("flowchart LR\nA --> B")
                .contains("flowchart LR")
        );
    }

    #[test]
    fn renderer_key_changes_reset_state_and_unsupported_terminals_keep_source() {
        let request = MermaidRenderRequest::svg("flowchart LR\nA --> B", 800, 600);
        let first = [input("diagram", &request.cache_key(), true)];
        let mut store = MarkdownMermaidPresentationStore::default();
        store.reconcile(&first, true);
        assert_eq!(store.schedule(&first).len(), 1);

        let changed = [input("diagram", "mermaid-v-next", true)];
        store.reconcile(&changed, false);
        let state = store.state("diagram").expect("unsupported state");
        assert_eq!(
            state,
            &MarkdownMermaidPresentationState::TerminalUnsupported
        );
        assert!(state.fallback("flowchart LR").contains("flowchart LR"));
        assert!(store.schedule(&changed).is_empty());
    }
}
