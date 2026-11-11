//! Markdown images and Mermaid diagrams of the V2 transcript (L8, D2, F4, B3).
//!
//! The Markdown producer reserves fixed placeholder rows for every image and (with
//! `[tui.markdown] mermaid`) every Mermaid diagram and exposes them as
//! [`VisualSlot`]s in the retained item geometry. This module runs the existing host-side
//! workers for those slots outside drawing and feeds accepted results back into the engine:
//!
//! * Scheduling ([`TranscriptV2Host::schedule_markdown_visuals`]) runs at the end of every
//!   update and after every committed frame. Candidates are the slots of the last prepared
//!   frame's visible units whose reserved rows intersect the viewport, then the slots of up to
//!   [`PREFETCH_UNITS`] resident units on each side (bounded prefetch), in document order.
//! * Work is bounded per lane (Mermaid, images) by the D2 shape: one active request plus one
//!   replaceable pending request. A newer candidate replaces the pending one; the pending
//!   request starts when the active one completes. An active request whose unit leaves the
//!   resident window, or whose slot no longer exists, is cancelled.
//! * Workers are the host's: [`render_mermaid_image`] (WASM-isolated render plus bounded
//!   rasterization) on a blocking task, and [`MarkdownImageLoader::load_cancellable`] (bounded,
//!   redirect-validating, decode-limited). The host only shares their handles.
//! * A completion is installed only on an exact match ([`TranscriptV2Host::complete_markdown_visual`]):
//!   it must be the lane's active request, its unit must still be resident at the same layout
//!   revision and its slot must still exist with the same contribution id and source key. A
//!   result that cannot be installed still enters the bounded result cache, so the next
//!   schedule installs it without work.
//! * Acceptance is a unit revision: the accepted state is part of the render configuration
//!   (`MarkdownOptions::visuals`), the owning unit's fingerprint changes and exactly that unit
//!   is re-projected. Ready images become BMUX image placements that the Markdown body
//!   registers from its own retained node while painting, so only visible, correctly
//!   positioned content contributes placements. Failures (and images the terminal or the
//!   network policy cannot show) become notice rows with the host's fallback text.
//! * Accepted visuals of units that left the resident window are dropped.
//!
//! Without terminal image support no work starts: Mermaid diagrams keep their highlighted,
//! source-backed code block and images show the "unsupported" notice. Remote images show
//! the "network disabled" notice unless `[tui.markdown] network_images` is set.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Instant;

use bcode_markdown_render::MarkdownDestination;
use bcode_tui_transcript::layout::producers::markdown::{
    AcceptedMarkdownVisual, MarkdownVisual, MarkdownVisualState, VisualSlot, VisualSlotRequest,
};
use bcode_tui_transcript::units::UnitKey;
use bmux_mermaid_render::{MermaidCancellationToken, WasmMermaidRenderer};
use bmux_tui::image::ImagePayload;

use super::TranscriptV2Host;
use crate::markdown_image::{
    MarkdownImageCancellationToken, MarkdownImageLoadDecision, MarkdownImageLoader,
    MarkdownImagePresentationState, markdown_image_load_decision,
};
use crate::markdown_mermaid::{
    DecodedMermaidImage, MarkdownMermaidPresentationState, render_mermaid_image,
};

/// Resident units on each side of the visible units whose slots are prefetched.
pub const PREFETCH_UNITS: usize = 2;
/// Most cached worker results.
pub const MAX_CACHED_RESULTS: usize = 32;
/// Most decoded pixel bytes held by the result cache.
pub const MAX_CACHED_BYTES: usize = 64 * 1024 * 1024;

/// Handles to the host-side workers (both are reference counted).
#[derive(Clone, Default)]
pub struct VisualWorkers {
    /// Bounded Markdown image loader, when it could be constructed.
    pub images: Option<MarkdownImageLoader>,
    /// Embedded Mermaid renderer, when it could be initialized.
    pub mermaid: Option<WasmMermaidRenderer>,
}

impl std::fmt::Debug for VisualWorkers {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualWorkers")
            .field("images", &self.images.is_some())
            .field("mermaid", &self.mermaid.is_some())
            .finish()
    }
}

/// Configuration and capabilities the schedule depends on.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VisualPolicy {
    /// Whether the terminal can present images.
    pub terminal_images: bool,
    /// Whether remote (HTTP(S)) images may be fetched (`[tui.markdown] network_images`).
    pub network_images: bool,
}

/// Worker lane of a request; each lane has one active and one pending request (D2).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VisualLane {
    /// Mermaid diagrams.
    Mermaid,
    /// Markdown images.
    Image,
}

impl VisualLane {
    /// Stable name (command keys, diagnostics).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Mermaid => "mermaid",
            Self::Image => "image",
        }
    }

    const fn of(request: &VisualSlotRequest) -> Self {
        match request {
            VisualSlotRequest::Mermaid { .. } => Self::Mermaid,
            VisualSlotRequest::Image { .. } => Self::Image,
        }
    }
}

/// One request for one slot of one resident unit, at one layout revision.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisualRequest {
    /// Host-unique request id.
    pub id: u64,
    /// Unit owning the slot.
    pub unit: UnitKey,
    /// Unit layout revision the slot was found at.
    pub revision: u64,
    /// Owner-qualified contribution id.
    pub contribution_id: String,
    /// What to render.
    pub request: VisualSlotRequest,
    /// [`VisualSlotRequest::source_key`] of `request`.
    pub source_key: String,
}

impl VisualRequest {
    /// Whether `other` asks for the same rendering of the same slot.
    fn same_target(&self, unit: &UnitKey, slot: &VisualSlot, source_key: &str) -> bool {
        &self.unit == unit
            && self.contribution_id == slot.contribution_id
            && self.source_key == source_key
    }
}

#[derive(Debug, Clone)]
enum Cancellation {
    Mermaid(MermaidCancellationToken),
    Image(MarkdownImageCancellationToken),
}

impl Cancellation {
    fn cancel(&self) {
        match self {
            Self::Mermaid(token) => token.cancel(),
            Self::Image(token) => token.cancel(),
        }
    }
}

/// One started worker request; [`Self::run`] executes it outside drawing.
pub struct VisualWork {
    request: VisualRequest,
    job: Job,
}

enum Job {
    Mermaid {
        renderer: WasmMermaidRenderer,
        cancellation: MermaidCancellationToken,
    },
    Image {
        loader: MarkdownImageLoader,
        cancellation: MarkdownImageCancellationToken,
    },
}

impl std::fmt::Debug for VisualWork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VisualWork")
            .field("request", &self.request)
            .finish_non_exhaustive()
    }
}

impl VisualWork {
    /// Lane of the work.
    #[must_use]
    pub const fn lane(&self) -> VisualLane {
        VisualLane::of(&self.request.request)
    }

    /// The request being executed.
    #[must_use]
    pub const fn request(&self) -> &VisualRequest {
        &self.request
    }

    /// Execute the request: Mermaid renders on a blocking task, images load through the
    /// bounded loader.
    pub async fn run(self) -> VisualCompletion {
        let lane = self.lane();
        let result = match (self.job, &self.request.request) {
            (
                Job::Mermaid {
                    renderer,
                    cancellation,
                },
                VisualSlotRequest::Mermaid { source, cache_key },
            ) => {
                let (source, cache_key) = (source.clone(), cache_key.clone());
                tokio::task::spawn_blocking(move || {
                    render_mermaid_image(&renderer, source, &cache_key, &cancellation)
                        .map(DecodedMermaidImage::into_payload)
                })
                .await
                .unwrap_or_else(|error| Err(error.to_string()))
            }
            (
                Job::Image {
                    loader,
                    cancellation,
                },
                VisualSlotRequest::Image { source, .. },
            ) => loader
                .load_cancellable(source, &cancellation)
                .await
                .map(crate::markdown_image::DecodedMarkdownImage::into_payload)
                .map_err(|error| error.to_string()),
            _ => Err("visual worker does not match its request".to_owned()),
        };
        VisualCompletion {
            lane,
            request: self.request,
            result: result.map(Arc::new).map_err(Arc::from),
        }
    }
}

/// Decoded pixels of a worker request, or a readable diagnostic.
pub type VisualResult = Result<Arc<ImagePayload>, Arc<str>>;

/// A finished worker request.
#[derive(Debug, Clone)]
pub struct VisualCompletion {
    /// Lane the request ran in.
    pub lane: VisualLane,
    /// The request.
    pub request: VisualRequest,
    /// Decoded pixels, or a readable diagnostic.
    pub result: VisualResult,
}

#[derive(Debug, Default)]
struct Lane {
    active: Option<(VisualRequest, Cancellation)>,
    pending: Option<VisualRequest>,
}

/// Bounded least-recently-used worker results by source key.
#[derive(Debug, Default)]
struct ResultCache {
    entries: BTreeMap<String, (u64, VisualResult)>,
    clock: u64,
}

const fn payload_bytes(payload: &ImagePayload) -> usize {
    match payload {
        ImagePayload::Png { bytes, .. } | ImagePayload::Pixels { bytes, .. } => bytes.len(),
    }
}

impl ResultCache {
    fn get(&mut self, key: &str) -> Option<VisualResult> {
        self.clock += 1;
        let clock = self.clock;
        self.entries.get_mut(key).map(|(used, result)| {
            *used = clock;
            result.clone()
        })
    }

    fn bytes(&self) -> usize {
        self.entries
            .values()
            .filter_map(|(_, result)| result.as_ref().ok())
            .map(|payload| payload_bytes(payload))
            .sum()
    }

    fn insert(&mut self, key: String, result: VisualResult) {
        if result
            .as_ref()
            .is_ok_and(|payload| payload_bytes(payload) > MAX_CACHED_BYTES)
        {
            return;
        }
        self.clock += 1;
        self.entries.insert(key, (self.clock, result));
        while self.entries.len() > MAX_CACHED_RESULTS || self.bytes() > MAX_CACHED_BYTES {
            let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (used, _))| *used)
                .map(|(key, _)| key.clone())
            else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }
}

/// Host state of the V2 Markdown visuals.
#[derive(Debug, Default)]
pub struct MarkdownVisuals {
    /// Accepted visuals, handed to the engine with the render configuration.
    accepted: MarkdownVisualState,
    /// Unit owning each accepted visual (residency reconciliation).
    owners: BTreeMap<String, UnitKey>,
    /// The accepted state changed since the render configuration was last built.
    changed: bool,
    next_request: u64,
    next_revision: u64,
    lanes: BTreeMap<VisualLane, Lane>,
    results: ResultCache,
    /// Started requests not yet handed to the runtime.
    queued: Vec<VisualWork>,
    workers: Option<VisualWorkers>,
    policy: Option<VisualPolicy>,
}

impl MarkdownVisuals {
    /// Accepted visuals (the render configuration's `MarkdownOptions::visuals`).
    #[must_use]
    pub const fn accepted(&self) -> &MarkdownVisualState {
        &self.accepted
    }

    /// Return and clear whether the accepted state changed.
    pub const fn take_changed(&mut self) -> bool {
        std::mem::replace(&mut self.changed, false)
    }

    /// Active request of `lane`, if any.
    #[must_use]
    pub fn active(&self, lane: VisualLane) -> Option<&VisualRequest> {
        self.lanes
            .get(&lane)
            .and_then(|lane| lane.active.as_ref())
            .map(|(request, _)| request)
    }

    /// Pending request of `lane`, if any.
    #[must_use]
    pub fn pending(&self, lane: VisualLane) -> Option<&VisualRequest> {
        self.lanes.get(&lane).and_then(|lane| lane.pending.as_ref())
    }

    fn accept(&mut self, unit: &UnitKey, request: &VisualRequest, visual: MarkdownVisual) {
        if self
            .accepted
            .accepted(&request.contribution_id)
            .is_some_and(|accepted| {
                accepted.source_key == request.source_key && accepted.visual == visual
            })
        {
            return;
        }
        self.next_revision += 1;
        self.accepted.accept(
            request.contribution_id.clone(),
            AcceptedMarkdownVisual {
                source_key: request.source_key.clone(),
                revision: self.next_revision,
                visual,
            },
        );
        self.owners
            .insert(request.contribution_id.clone(), unit.clone());
        self.changed = true;
    }
}

/// One candidate slot of the schedule.
struct Candidate {
    unit: UnitKey,
    revision: u64,
    slot: VisualSlot,
}

/// Notice rows of a failed request (the host's fallback text).
fn failure_notice(request: &VisualSlotRequest, diagnostic: &str) -> Arc<str> {
    match request {
        VisualSlotRequest::Mermaid { source, .. } => {
            MarkdownMermaidPresentationState::Failed(diagnostic.to_owned()).fallback(source)
        }
        VisualSlotRequest::Image {
            alt,
            source,
            linked_destination,
        } => image_notice(
            &MarkdownImagePresentationState::Failed(diagnostic.to_owned()),
            alt,
            source,
            linked_destination.as_ref(),
        ),
    }
    .into()
}

fn image_notice(
    state: &MarkdownImagePresentationState,
    alt: &str,
    source: &MarkdownDestination,
    linked: Option<&MarkdownDestination>,
) -> String {
    if linked.is_some() {
        state.linked_badge_fallback(alt, source, linked)
    } else {
        state.fallback(alt, source)
    }
}

/// What the schedule does for a slot that has no accepted visual.
enum Plan {
    /// Accept a visual now (cache hit, policy notice).
    Accept(MarkdownVisual),
    /// Request worker output.
    Request,
    /// Leave the placeholder.
    Skip,
}

fn plan(policy: VisualPolicy, workers: &VisualWorkers, request: &VisualSlotRequest) -> Plan {
    match request {
        VisualSlotRequest::Mermaid { .. } => {
            if policy.terminal_images && workers.mermaid.is_some() {
                Plan::Request
            } else {
                Plan::Skip
            }
        }
        VisualSlotRequest::Image {
            alt,
            source,
            linked_destination,
        } => {
            let notice = |state: MarkdownImagePresentationState| {
                Plan::Accept(MarkdownVisual::Notice(
                    image_notice(&state, alt, source, linked_destination.as_ref()).into(),
                ))
            };
            if !policy.terminal_images {
                return notice(MarkdownImagePresentationState::TerminalUnsupported);
            }
            match markdown_image_load_decision(source) {
                MarkdownImageLoadDecision::Remote(_) if !policy.network_images => {
                    notice(MarkdownImagePresentationState::NetworkDisabled)
                }
                MarkdownImageLoadDecision::Reject => Plan::Skip,
                MarkdownImageLoadDecision::Remote(_) | MarkdownImageLoadDecision::Local(_)
                    if workers.images.is_some() =>
                {
                    Plan::Request
                }
                MarkdownImageLoadDecision::Remote(_) | MarkdownImageLoadDecision::Local(_) => {
                    Plan::Skip
                }
            }
        }
    }
}

impl TranscriptV2Host {
    /// Install the host-side worker handles (once; later calls keep the first handles).
    pub fn set_visual_workers(&mut self, workers: impl FnOnce() -> VisualWorkers) {
        if self.visuals.workers.is_none() {
            self.visuals.workers = Some(workers());
        }
    }

    /// Markdown visual state (scenario and test observation).
    #[must_use]
    pub const fn markdown_visuals(&self) -> &MarkdownVisuals {
        &self.visuals
    }

    /// Reconcile and schedule Markdown visuals for the last prepared frame under `policy`
    /// (module documentation). Accepted visuals that changed are applied to the engine before
    /// this returns. Returns whether worker requests were started ([`Self::drain_visual_work`]).
    pub fn schedule_markdown_visuals(&mut self, policy: VisualPolicy, at: Instant) -> bool {
        if self.visuals.policy != Some(policy) {
            // Policy notices and results depend on the policy: start over.
            if self.visuals.policy.is_some() && !self.visuals.accepted.is_empty() {
                self.visuals.accepted = MarkdownVisualState::new();
                self.visuals.owners.clear();
                self.visuals.changed = true;
            }
            self.visuals.policy = Some(policy);
        }
        self.reconcile_visual_residency();
        let workers = self.visuals.workers.clone().unwrap_or_default();
        self.plan_visuals(policy, &workers);
        let queued = self.promote_visual_requests(&workers);
        if queued {
            // A lane that just started its request plans its next pending one.
            self.plan_visuals(policy, &workers);
        }
        self.flush_render_config(at);
        queued
    }

    /// Accept what needs no work and choose each lane's pending request from the candidates.
    fn plan_visuals(&mut self, policy: VisualPolicy, workers: &VisualWorkers) {
        let mut planned = BTreeSet::new();
        for candidate in self.visual_candidates() {
            let source_key = candidate.slot.request.source_key();
            if self.visuals.accepted.for_slot(&candidate.slot).is_some() {
                continue;
            }
            let lane = VisualLane::of(&candidate.slot.request);
            let state = self.visuals.lanes.entry(lane).or_default();
            if state.active.as_ref().is_some_and(|(active, _)| {
                active.same_target(&candidate.unit, &candidate.slot, &source_key)
            }) {
                continue;
            }
            // The policy decides first; a slot it lets render reuses a cached result.
            let plan = match plan(policy, workers, &candidate.slot.request) {
                Plan::Request => match self.visuals.results.get(&source_key) {
                    Some(Ok(payload)) => Plan::Accept(MarkdownVisual::Image(payload)),
                    Some(Err(diagnostic)) => Plan::Accept(MarkdownVisual::Notice(failure_notice(
                        &candidate.slot.request,
                        &diagnostic,
                    ))),
                    None => Plan::Request,
                },
                other => other,
            };
            let request = VisualRequest {
                id: 0,
                unit: candidate.unit.clone(),
                revision: candidate.revision,
                contribution_id: candidate.slot.contribution_id.clone(),
                request: candidate.slot.request.clone(),
                source_key,
            };
            match plan {
                Plan::Accept(visual) => self.visuals.accept(&candidate.unit, &request, visual),
                Plan::Skip => {}
                Plan::Request => {
                    // The highest-priority candidate of a lane becomes its pending request,
                    // replacing an older pending one (D2: one active, one replaceable pending).
                    if !planned.insert(lane) {
                        continue;
                    }
                    let state = self.visuals.lanes.entry(lane).or_default();
                    if state.pending.as_ref().is_none_or(|pending| {
                        !pending.same_target(&candidate.unit, &candidate.slot, &request.source_key)
                    }) {
                        self.visuals.next_request += 1;
                        state.pending = Some(VisualRequest {
                            id: self.visuals.next_request,
                            ..request
                        });
                    }
                }
            }
        }
    }

    /// Started worker requests not yet handed to the runtime.
    pub fn drain_visual_work(&mut self) -> Vec<VisualWork> {
        std::mem::take(&mut self.visuals.queued)
    }

    /// Install a worker result on an exact match (module documentation) and start the lane's
    /// pending request. Returns whether the transcript changed.
    pub fn complete_markdown_visual(&mut self, completion: VisualCompletion, at: Instant) -> bool {
        let VisualCompletion {
            lane,
            request,
            result,
        } = completion;
        let state = self.visuals.lanes.entry(lane).or_default();
        if state
            .active
            .as_ref()
            .is_none_or(|(active, _)| active.id != request.id)
        {
            // Cancelled or replaced: late results are discarded.
            return false;
        }
        state.active = None;
        self.visuals
            .results
            .insert(request.source_key.clone(), result.clone());
        let current = self
            .engine
            .layouts()
            .current(&request.unit)
            .is_some_and(|layout| {
                self.engine.units().contains(&request.unit)
                    && layout.key.revision == request.revision
                    && layout.geometry.visual_slots.iter().any(|slot| {
                        slot.contribution_id == request.contribution_id
                            && slot.request.source_key() == request.source_key
                    })
            });
        if current {
            let visual = match result {
                Ok(payload) => MarkdownVisual::Image(payload),
                Err(diagnostic) => {
                    MarkdownVisual::Notice(failure_notice(&request.request, &diagnostic))
                }
            };
            let unit = request.unit.clone();
            self.visuals.accept(&unit, &request, visual);
        }
        let workers = self.visuals.workers.clone().unwrap_or_default();
        let _queued = self.promote_visual_requests(&workers);
        self.flush_render_config(at)
    }

    /// Drop accepted visuals of units that left the resident window; cancel and drop requests
    /// whose unit or slot is gone.
    fn reconcile_visual_residency(&mut self) {
        let engine = &self.engine;
        let slot_exists = |request: &VisualRequest| {
            engine.units().contains(&request.unit)
                && engine
                    .layouts()
                    .current(&request.unit)
                    .is_some_and(|layout| {
                        layout.geometry.visual_slots.iter().any(|slot| {
                            slot.contribution_id == request.contribution_id
                                && slot.request.source_key() == request.source_key
                        })
                    })
        };
        for state in self.visuals.lanes.values_mut() {
            if state
                .active
                .as_ref()
                .is_some_and(|(active, _)| !slot_exists(active))
                && let Some((_, cancellation)) = state.active.take()
            {
                cancellation.cancel();
            }
            if state
                .pending
                .as_ref()
                .is_some_and(|pending| !slot_exists(pending))
            {
                state.pending = None;
            }
        }
        let owners = &mut self.visuals.owners;
        let before = owners.len();
        owners.retain(|_, unit| engine.units().contains(unit));
        if owners.len() != before {
            let owners = &*owners;
            self.visuals.accepted.retain(|id| owners.contains_key(id));
            self.visuals.changed = true;
        }
    }

    /// Candidate slots in priority order: visible slots of the last prepared frame, then the
    /// slots of up to [`PREFETCH_UNITS`] resident units on each side.
    fn visual_candidates(&self) -> Vec<Candidate> {
        let Some(frame) = self.last_frame.as_ref() else {
            return Vec::new();
        };
        let Some(viewport) = frame.viewport.as_ref() else {
            return Vec::new();
        };
        let height = i64::try_from(frame.geometry.height).unwrap_or(i64::MAX);
        let layouts = self.engine.layouts();
        let mut candidates = Vec::new();
        let mut push = |unit: &UnitKey, visible_top: Option<i64>| {
            let Some(layout) = layouts.current(unit) else {
                return;
            };
            for slot in &layout.geometry.visual_slots {
                if let Some(top) = visible_top {
                    let start = top.saturating_add(i64::try_from(slot.rect.y).unwrap_or(i64::MAX));
                    let end =
                        start.saturating_add(i64::try_from(slot.rect.height).unwrap_or(i64::MAX));
                    if end <= 0 || start >= height {
                        continue;
                    }
                }
                candidates.push(Candidate {
                    unit: unit.clone(),
                    revision: layout.key.revision,
                    slot: slot.clone(),
                });
            }
        };
        for unit in &viewport.units {
            // Only units painted from their current layout are correctly positioned.
            if layouts
                .current(&unit.key)
                .is_some_and(|layout| Some(&layout.key) == unit.layout.as_ref())
            {
                push(&unit.key, Some(unit.top));
            }
        }
        let keys = self.engine.units().keys();
        let indexes = viewport
            .units
            .iter()
            .filter_map(|unit| self.engine.units().index_of(&unit.key));
        let (first, last) = indexes.fold((usize::MAX, 0), |(first, last), index| {
            (first.min(index), last.max(index))
        });
        if first != usize::MAX {
            let after = keys
                .iter()
                .skip(last.saturating_add(1))
                .take(PREFETCH_UNITS);
            let before = keys[..first].iter().rev().take(PREFETCH_UNITS);
            for unit in after.chain(before) {
                push(unit, None);
            }
        }
        candidates
    }

    /// Start the pending request of every idle lane. Returns whether work was queued.
    fn promote_visual_requests(&mut self, workers: &VisualWorkers) -> bool {
        let mut queued = false;
        for (lane, state) in &mut self.visuals.lanes {
            if state.active.is_some() {
                continue;
            }
            let Some(request) = state.pending.take() else {
                continue;
            };
            let (job, cancellation) = match (lane, workers) {
                (
                    VisualLane::Mermaid,
                    VisualWorkers {
                        mermaid: Some(renderer),
                        ..
                    },
                ) => {
                    let token = MermaidCancellationToken::default();
                    (
                        Job::Mermaid {
                            renderer: renderer.clone(),
                            cancellation: token.clone(),
                        },
                        Cancellation::Mermaid(token),
                    )
                }
                (
                    VisualLane::Image,
                    VisualWorkers {
                        images: Some(loader),
                        ..
                    },
                ) => {
                    let token = MarkdownImageCancellationToken::default();
                    (
                        Job::Image {
                            loader: loader.clone(),
                            cancellation: token.clone(),
                        },
                        Cancellation::Image(token),
                    )
                }
                _ => continue,
            };
            state.active = Some((request.clone(), cancellation));
            self.visuals.queued.push(VisualWork { request, job });
            queued = true;
        }
        queued
    }
}

#[cfg(test)]
#[path = "visuals_tests.rs"]
mod tests;
