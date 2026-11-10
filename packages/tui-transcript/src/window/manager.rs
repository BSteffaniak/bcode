//! Resident window manager (spec section 5, rules W1–W13 and W7a).
//!
//! [`WindowManager`] owns the resident canonical events, BMUX `resident_window` bookkeeping
//! keyed by event sequence (weight one event each), and the public [`ResidentWindow`] summary
//! the reducer reads. It plans page and window requests as data, incorporates their results
//! into the engine's `SessionView`, applies live events, buffers live events while a `Newer`
//! page is in flight (W11), and evicts whole turns outside the protected band (W7). It performs
//! no I/O and reads no clock.
//!
//! Results are incorporated with the `SessionView` window operations (spec 3.1, SD1–SD7): an
//! `Older` page is prepended from an isolated sub-projection, a `Newer` page is appended, eviction
//! removes whole turns, and only window replacements (and the SD2 fallback for a page that cannot
//! be spliced) reduce a whole window. Every operation reports its own [`TranscriptChanges`];
//! [`WindowWork`] counts the work so tests can prove paging never rebuilds.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use bcode_session_models::{SessionEvent, SessionLiveEvent};
use bcode_session_view::SessionView;
use bcode_session_view::window_ops::WindowOpError;
use bcode_session_view_models::{TranscriptChanges, TranscriptViewItem};
use bmux_tui_components::resident_window::{
    self as rw, EdgeAvailability, EdgeRows, EvictionBudget, EvictionInput, EvictionSides, ItemRows,
    JumpTarget, PageResult, PositionHints, PrefetchPolicy, ProtectedBand, ReplaceResult,
    ResidentItem, RowDensity, SideDemand, ViewportPlacement, WeightCeiling, WindowSide,
};

use super::turns::{align_newer_page, align_older_page, align_window_result, is_safe_window_start};
use super::{
    HistoryPageResult, HistoryWindowResult, IssuedRequest, LIVE_BUFFER_BYTES, LIVE_BUFFER_EVENTS,
    PAGE_EVENTS, PageDirection, PageRequest, RESIDENT_CEILING_EVENTS, RESIDENT_MAX_EVENTS,
    RESIDENT_TARGET_EVENTS, ResidentWindow, WindowMode, WindowRequest, prefetch_rows,
};
use crate::units::{BoundaryEdge, UnitKey, Units};

/// Window-operation work since the manager was created (spec 9.2; phase 5 exit: no full rebuild
/// on paging).
///
/// `replacements` counts every whole-window reduction: `Head`/`Tail`/`Around` results and SD2
/// fallbacks. Paging and eviction add to their own counters only, and `events_reduced` grows by
/// the page's events alone (no resident replay).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub struct WindowWork {
    /// Whole-window reductions (window results and SD2 fallbacks).
    pub replacements: u64,
    /// Pages that could not be spliced and fell back to a replacement (SD2, W4).
    pub fallbacks: u64,
    /// `Older` pages prepended without replay.
    pub prepends: u64,
    /// `Newer` pages appended without replay.
    pub appends: u64,
    /// Evictions applied to the projection.
    pub evictions: u64,
    /// Durable events handed to the reducer (pages, windows, live and buffered events).
    pub events_reduced: u64,
    /// Eviction plans computed (W7). An update whose eviction inputs equal those of the last
    /// plan that found nothing to evict reuses that result instead of planning again.
    pub eviction_plans: u64,
}

/// A live input for the attached session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveInput {
    /// A canonical durable event delivered live.
    Durable(Box<SessionEvent>),
    /// A live-only event (streams, drafts, live reasoning).
    Stream(Box<SessionLiveEvent>),
}

/// What happened to a live input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LiveOutcome {
    /// Applied to the projection (live window).
    Applied(TranscriptChanges),
    /// Buffered while a `Newer` page or live-state request is in flight (W11).
    Buffered,
    /// Not applied: the window is detached (W2); newer activity was recorded.
    Detached,
    /// A durable event already resident; never re-applied (SD1).
    Duplicate,
}

/// Why a page or window result was dropped. The window is unchanged.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum StaleResult {
    /// The result was requested for another window generation.
    Generation,
    /// No matching request is in flight (cancelled, superseded or already applied).
    NotInFlight,
    /// The window edge changed after the request was issued.
    EdgeChanged,
}

/// A page result after incorporation (W3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PageIncorporated {
    /// Generation the page was requested for.
    pub generation: u64,
    /// Page direction.
    pub direction: PageDirection,
    /// Whether that side is now exhausted.
    pub exhausted: bool,
    /// Item-level changes of the incorporation.
    pub changes: TranscriptChanges,
    /// The window mode changed (`Detached → Live`, W11).
    pub mode_changed: bool,
    /// Live-state rehydration the host must execute (W11), if any.
    pub follow_up: Option<IssuedRequest<WindowRequest>>,
}

/// A window result after incorporation (W3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowIncorporated {
    /// Generation the window was requested for.
    pub generation: u64,
    /// Which window.
    pub target: WindowRequest,
    /// Item-level changes of the incorporation.
    pub changes: TranscriptChanges,
    /// The window mode changed.
    pub mode_changed: bool,
}

/// Inputs for eviction (W7), taken from navigation and the viewport after incorporation.
#[derive(Debug, Clone, Copy)]
pub struct EvictionContext<'a> {
    /// Collection row (in the measured index) shown on the viewport top row.
    pub viewport_top: u64,
    /// Viewport height in rows.
    pub viewport_height: u64,
    /// Reading anchor unit, when navigation has one. Never evicted.
    pub anchor: Option<&'a UnitKey>,
    /// Further units that must stay resident: motion target, selection endpoints.
    pub pinned: &'a [UnitKey],
    /// Eviction is deferred (selection gesture or selection active, motion running). The W7a
    /// ceiling still applies: a window above 1,536 events is trimmed back to the ceiling.
    pub deferred: bool,
    /// Exact measured rows of the resident units. With the other inputs it identifies an
    /// unchanged layout, so an eviction already found impossible is not planned again.
    pub resident_rows: u64,
}

/// The inputs of the last eviction plan that found nothing to evict (W7).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EvictionProbe {
    keys_revision: u64,
    transcript_revision: u64,
    last_extended: Option<PageDirection>,
    viewport_top: u64,
    viewport_height: u64,
    resident_rows: u64,
    anchor: Option<UnitKey>,
    pinned: Vec<UnitKey>,
    target: u64,
}

/// An eviction after application (W7, W9).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EvictionOutcome {
    /// Side trimmed.
    pub side: PageDirection,
    /// Canonical events removed.
    pub removed_events: u64,
    /// The cut was not at a turn start (one turn larger than the target, W7).
    pub unaligned: bool,
    /// Item-level changes of the eviction.
    pub changes: TranscriptChanges,
    /// The window switched `Live → Detached` (W9).
    pub mode_changed: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct LiveBuffer {
    inputs: VecDeque<LiveInput>,
    bytes: usize,
    overflowed: bool,
    /// The host's live input lost continuity while the request was in flight: inputs between
    /// the request and now are missing, so neither the buffer nor a live-state read issued
    /// before the loss may take the window live (W11).
    discontinuous: bool,
    /// A live-only stream was active when the request was issued (W11).
    stream_active_at_issue: bool,
}

impl LiveBuffer {
    fn push(&mut self, input: LiveInput) {
        if self.overflowed {
            return;
        }
        let bytes = encoded_len(&input);
        if self.inputs.len() >= LIVE_BUFFER_EVENTS || self.bytes + bytes > LIVE_BUFFER_BYTES {
            self.overflowed = true;
            self.inputs.clear();
            self.bytes = 0;
            return;
        }
        self.bytes += bytes;
        self.inputs.push_back(input);
    }
}

fn encoded_len(input: &LiveInput) -> usize {
    let encoded = match input {
        LiveInput::Durable(event) => serde_json::to_vec(event.as_ref()),
        LiveInput::Stream(event) => serde_json::to_vec(event.as_ref()),
    };
    // An event that cannot be encoded cannot be bounded; count it as filling the buffer.
    encoded.map_or(LIVE_BUFFER_BYTES, |bytes| bytes.len())
}

/// Exact rows per resident event and the safe window starts, for eviction planning.
struct EventRows<'a> {
    rows: &'a BTreeMap<u64, u64>,
    starts: &'a BTreeSet<u64>,
    aligned: bool,
}

impl ItemRows<u64> for EventRows<'_> {
    fn rows(&self, key: &u64) -> u64 {
        self.rows.get(key).copied().unwrap_or(0)
    }

    fn is_window_start(&self, key: &u64) -> bool {
        !self.aligned || self.starts.contains(key)
    }
}

const fn side_of(direction: PageDirection) -> WindowSide {
    match direction {
        PageDirection::Older => WindowSide::Before,
        PageDirection::Newer => WindowSide::After,
    }
}

const fn availability(more: bool) -> EdgeAvailability {
    if more {
        EdgeAvailability::MoreAvailable
    } else {
        EdgeAvailability::Exhausted
    }
}

/// The resident window manager.
#[derive(Debug, Clone)]
pub struct WindowManager {
    summary: ResidentWindow,
    keys: rw::ResidentWindow<u64>,
    events: BTreeMap<u64, SessionEvent>,
    work: WindowWork,
    replace: Option<(rw::RequestToken, WindowRequest)>,
    live_state: Option<u64>,
    buffer: Option<LiveBuffer>,
    stream_activity: bool,
    last_extended: Option<PageDirection>,
    density: RowDensity,
    /// A window result has been incorporated (before that, availability is unknown).
    attached: bool,
    /// Inputs of the last eviction plan that found nothing to evict.
    no_eviction: Option<EvictionProbe>,
}

impl Default for WindowManager {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowManager {
    /// Create an empty live window with nothing resident. The host attaches by requesting
    /// the tail window with [`Self::request_window`] (W10).
    #[must_use]
    pub fn new() -> Self {
        let mut keys = rw::ResidentWindow::new();
        keys.set_availability(WindowSide::After, EdgeAvailability::Exhausted);
        let mut manager = Self {
            summary: ResidentWindow::new(),
            keys,
            events: BTreeMap::new(),
            work: WindowWork::default(),
            replace: None,
            live_state: None,
            buffer: None,
            stream_activity: false,
            last_extended: None,
            density: RowDensity::new(1.0),
            attached: false,
            no_eviction: None,
        };
        manager.refresh();
        manager
    }

    /// The window summary the reducer reads.
    #[must_use]
    pub const fn summary(&self) -> &ResidentWindow {
        &self.summary
    }

    /// Resident canonical events in sequence order.
    pub fn events(&self) -> impl ExactSizeIterator<Item = &SessionEvent> + '_ {
        self.events.values()
    }

    /// Resident canonical events (W7 budget unit).
    #[must_use]
    pub const fn event_count(&self) -> u64 {
        self.keys.total_weight()
    }

    /// Whether a canonical sequence is resident.
    #[must_use]
    pub fn contains_sequence(&self, sequence: u64) -> bool {
        self.events.contains_key(&sequence)
    }

    /// Presentation-only BMUX position estimate for the scrollbar (W12, VP4).
    #[must_use]
    pub fn bmux_estimate(&self, resident_rows: u64) -> rw::PositionEstimate {
        self.keys
            .position_estimate(resident_rows, self.density.value())
    }

    /// Fold the measured resident rows into the rows-per-event estimate (W12).
    pub fn observe_rows(&mut self, resident_rows: u64) {
        self.density.observe(resident_rows, self.event_count());
        self.summary.rows_per_event_estimate = self.density.value();
    }

    /// Window-operation work since the manager was created.
    #[must_use]
    pub const fn work(&self) -> WindowWork {
        self.work
    }

    /// The host's live input lost continuity (its feed skipped inputs, W2, W11). A `Newer`
    /// page or `LiveState` read in flight can then no longer take the window live with the
    /// inputs buffered since its request: the buffer is discarded, the page's end rehydrates
    /// live state instead, and a live-state read in flight is incorporated like a newer page
    /// (the window stays detached and newer paging continues). A live window is the host's to
    /// resynchronize (W10). Returns whether an in-flight request was affected.
    pub fn interrupt_live_buffer(&mut self) -> bool {
        let Some(buffer) = self.buffer.as_mut() else {
            return false;
        };
        buffer.inputs.clear();
        buffer.bytes = 0;
        buffer.overflowed = true;
        buffer.discontinuous = true;
        true
    }

    /// Drain the item changes of a host-driven `SessionView` change outside the window
    /// operations (for example a reasoning visibility toggle).
    pub fn sync_document(&mut self, session: &mut SessionView) -> TranscriptChanges {
        session.drain_transcript_changes()
    }

    // ---- requests ----

    /// Request a window replacement (W10) or live-state rehydration (W11).
    ///
    /// `Head`, `Tail` and `Around` bump the generation and cancel in-flight pages and any
    /// earlier replacement (W5). `LiveState` does not replace the durable window.
    pub fn request_window(&mut self, request: WindowRequest) -> IssuedRequest<WindowRequest> {
        if request == WindowRequest::LiveState {
            self.live_state = Some(self.summary.generation);
            self.buffer = Some(LiveBuffer::default());
            self.refresh();
            return IssuedRequest {
                generation: self.summary.generation,
                request,
            };
        }
        self.summary.generation += 1;
        let target = match request {
            WindowRequest::Around { sequence } => JumpTarget::Around(sequence),
            WindowRequest::Head => JumpTarget::Head,
            WindowRequest::Tail | WindowRequest::LiveState => JumpTarget::Tail,
        };
        let issued = self.keys.request_replace(target);
        self.replace = Some((issued.token, request));
        self.live_state = None;
        self.buffer = None;
        self.refresh();
        IssuedRequest {
            generation: self.summary.generation,
            request,
        }
    }

    /// Cancel the in-flight window replacement (a jump cancelled by manual input, T22, C14).
    ///
    /// The generation is bumped so the cancelled result is dropped as stale when it arrives,
    /// and prefetch may request pages again. Returns whether a replacement was in flight.
    pub fn cancel_window(&mut self) -> bool {
        if self.replace.take().is_none() {
            return false;
        }
        let _ = self.keys.cancel_replace();
        self.summary.generation += 1;
        self.refresh();
        true
    }

    /// Prefetch pages for both sides (W5, W6, W7a).
    ///
    /// `rows` are the exact resident rows beyond each viewport edge. A side is requested when
    /// fewer than `prefetch_rows(viewport_height)` rows remain and it has more history, at most
    /// one request per side, none while a replacement is in flight, and none while the ceiling
    /// leaves no room for another page.
    pub fn poll_prefetch(
        &mut self,
        rows: EdgeRows,
        viewport_height: u64,
    ) -> Vec<IssuedRequest<PageRequest>> {
        let distance = prefetch_rows(viewport_height);
        let after = if self.summary.mode == WindowMode::Detached {
            distance
        } else {
            0
        };
        let policy = PrefetchPolicy {
            before_rows: distance,
            after_rows: after,
            ..PrefetchPolicy::default()
        }
        .ceiling(Self::ceiling());
        self.poll(rows, &policy, &[PageDirection::Older, PageDirection::Newer])
    }

    /// Request one page in a direction now, for pending movement at a boundary (N8), subject
    /// to the same in-flight, replacement and ceiling limits as prefetch (W7a).
    pub fn request_page(&mut self, direction: PageDirection) -> Option<IssuedRequest<PageRequest>> {
        if direction == PageDirection::Newer && self.summary.mode == WindowMode::Live {
            return None;
        }
        let (before_rows, after_rows) = match direction {
            PageDirection::Older => (1, 0),
            PageDirection::Newer => (0, 1),
        };
        let policy = PrefetchPolicy {
            before_rows,
            after_rows,
            ..PrefetchPolicy::default()
        }
        .ceiling(Self::ceiling());
        self.poll(EdgeRows::default(), &policy, &[direction])
            .into_iter()
            .next()
    }

    const fn ceiling() -> WeightCeiling {
        WeightCeiling {
            max_weight: RESIDENT_CEILING_EVENTS,
            page_reserve: PAGE_EVENTS,
        }
    }

    fn poll(
        &mut self,
        rows: EdgeRows,
        policy: &PrefetchPolicy,
        sides: &[PageDirection],
    ) -> Vec<IssuedRequest<PageRequest>> {
        let demands = self.keys.poll_demands(rows, policy);
        // W7a: a side that needs rows but has no room under the ceiling shows `loading` until
        // eviction frees room and the page is requested.
        let blocked = |demand: &SideDemand<u64>| {
            matches!(demand, SideDemand::Blocked(rw::DemandBlock::Ceiling))
        };
        if sides.contains(&PageDirection::Older) {
            self.summary.ceiling_blocked.older = blocked(&demands.before);
        }
        if sides.contains(&PageDirection::Newer) {
            self.summary.ceiling_blocked.newer = blocked(&demands.after);
        }
        let mut issued = Vec::new();
        for demand in [demands.before, demands.after] {
            let SideDemand::Requested(request) = demand else {
                continue;
            };
            let page = match request.side {
                WindowSide::Before => PageRequest::Older {
                    before_sequence: request.edge_key,
                },
                WindowSide::After => {
                    self.buffer = Some(LiveBuffer {
                        stream_active_at_issue: self.stream_activity,
                        ..LiveBuffer::default()
                    });
                    PageRequest::Newer {
                        after_sequence: request.edge_key,
                    }
                }
            };
            issued.push(IssuedRequest {
                generation: self.summary.generation,
                request: page,
            });
        }
        self.refresh();
        issued
    }

    // ---- incorporation ----

    /// Incorporate a page result (W3, W4, W11).
    ///
    /// # Errors
    ///
    /// * [`StaleResult::Generation`] if the result belongs to another window generation.
    /// * [`StaleResult::NotInFlight`] if no page in that direction is in flight.
    /// * [`StaleResult::EdgeChanged`] if the window edge changed since the request.
    pub fn apply_page(
        &mut self,
        result: &HistoryPageResult,
        session: &mut SessionView,
    ) -> Result<PageIncorporated, StaleResult> {
        if result.generation != self.summary.generation {
            return Err(StaleResult::Generation);
        }
        let side = side_of(result.direction);
        let token = self.keys.in_flight(side).ok_or(StaleResult::NotInFlight)?;
        let aligned = match (result.direction, result.unaligned) {
            (_, true) => super::turns::Aligned {
                start: 0,
                end: result.events.len(),
                has_older: result.has_more,
                has_newer: result.has_more,
            },
            (PageDirection::Older, false) => align_older_page(&result.events, result.has_more),
            (PageDirection::Newer, false) => align_newer_page(&result.events, result.has_more),
        };
        let more = match result.direction {
            PageDirection::Older => aligned.has_older,
            PageDirection::Newer => aligned.has_newer,
        };
        // Page ranges are disjoint from resident ranges; overlapping events are never
        // re-applied (SD1).
        let kept = result.events[aligned.start..aligned.end]
            .iter()
            .filter(|event| match result.direction {
                PageDirection::Older => self
                    .summary
                    .first_sequence
                    .is_none_or(|first| event.sequence < first),
                PageDirection::Newer => self
                    .summary
                    .last_sequence
                    .is_none_or(|last| event.sequence > last),
            })
            .cloned()
            .collect::<Vec<_>>();
        let items = kept.iter().map(|event| ResidentItem::new(event.sequence));
        if let Err(stale) = self
            .keys
            .apply_page(token, PageResult::new(items, availability(more)))
        {
            self.refresh();
            return Err(match stale.reason {
                rw::StaleReason::NotInFlight => StaleResult::NotInFlight,
                rw::StaleReason::EdgeChanged => StaleResult::EdgeChanged,
            });
        }
        for event in &kept {
            self.events.insert(event.sequence, event.clone());
        }
        self.last_extended = Some(result.direction);

        let mut mode_changed = false;
        let mut follow_up = None;
        let mut changes = match result.direction {
            PageDirection::Older => {
                if !more {
                    self.summary.first_available_sequence = self.first_sequence();
                }
                let prepended =
                    self.measured(session, |session| session.prepend_history_page(&kept));
                self.page_or_fallback(prepended, PageDirection::Older, session)
            }
            PageDirection::Newer => {
                let appended = self.measured(session, |session| session.append_history_page(&kept));
                self.page_or_fallback(appended, PageDirection::Newer, session)
            }
        };
        if result.direction == PageDirection::Newer {
            let buffer = self.buffer.take();
            if !more {
                let buffer = buffer.unwrap_or_default();
                if buffer.overflowed || buffer.stream_active_at_issue {
                    follow_up = Some(self.request_window(WindowRequest::LiveState));
                } else {
                    self.set_mode(WindowMode::Live);
                    mode_changed = true;
                    self.apply_buffered(buffer, session);
                    append_changes(&mut changes, session.drain_transcript_changes());
                }
            }
        }
        self.refresh();
        Ok(PageIncorporated {
            generation: result.generation,
            direction: result.direction,
            exhausted: !more,
            changes,
            mode_changed,
            follow_up,
        })
    }

    /// Incorporate a window result (W3, W8, W10, W11).
    ///
    /// # Errors
    ///
    /// * [`StaleResult::Generation`] if the result belongs to another window generation.
    /// * [`StaleResult::NotInFlight`] if no request for that target is in flight.
    pub fn apply_window(
        &mut self,
        result: &HistoryWindowResult,
        session: &mut SessionView,
    ) -> Result<WindowIncorporated, StaleResult> {
        if result.generation != self.summary.generation {
            return Err(StaleResult::Generation);
        }
        if result.target == WindowRequest::LiveState {
            return self.apply_live_state(result, session);
        }
        let Some((token, target)) = self.replace else {
            return Err(StaleResult::NotInFlight);
        };
        if target != result.target {
            return Err(StaleResult::NotInFlight);
        }
        // W8, W10: bounded to one window's events and aligned to whole turns, keeping the
        // target's turn of an `Around` window.
        let aligned =
            align_window_result(&result.events, target, result.has_older, result.has_newer);
        let kept = &result.events[aligned.start..aligned.end];
        let mode = if aligned.has_newer {
            WindowMode::Detached
        } else {
            WindowMode::Live
        };
        let hints = match (
            result.first_available_sequence,
            result.last_available_sequence,
            kept.first(),
            kept.last(),
        ) {
            (Some(first_available), Some(last_available), Some(first), Some(last)) => {
                PositionHints::known(
                    first.sequence.saturating_sub(first_available),
                    last_available.saturating_sub(last.sequence),
                )
            }
            _ => PositionHints::unknown(),
        };
        let replaced = self.keys.apply_replace(
            token,
            ReplaceResult {
                items: kept
                    .iter()
                    .map(|event| ResidentItem::new(event.sequence))
                    .collect(),
                before: availability(aligned.has_older),
                after: availability(aligned.has_newer),
                hints,
            },
        );
        if replaced.is_err() {
            self.replace = None;
            self.refresh();
            return Err(StaleResult::NotInFlight);
        }
        self.replace = None;
        self.attached = true;
        self.summary.ceiling_blocked = super::CeilingBlocked::default();
        self.events = kept
            .iter()
            .map(|event| (event.sequence, event.clone()))
            .collect();
        // W12: session bounds the result does not report keep what is already known.
        let known_last = self
            .summary
            .last_available_sequence
            .max(self.summary.latest_known_sequence);
        self.summary.first_available_sequence = result
            .first_available_sequence
            .or_else(|| {
                (!aligned.has_older)
                    .then(|| self.first_sequence())
                    .flatten()
            })
            .or(self.summary.first_available_sequence);
        self.summary.last_available_sequence = result
            .last_available_sequence
            .or(known_last)
            .max(self.events.keys().next_back().copied());
        self.last_extended = None;
        self.buffer = None;
        self.live_state = None;
        self.stream_activity = false;
        let mode_changed = self.summary.mode != mode;
        self.set_mode(mode);

        let changes = self.replace(session);
        self.refresh();
        Ok(WindowIncorporated {
            generation: result.generation,
            target,
            changes,
            mode_changed,
        })
    }

    fn apply_live_state(
        &mut self,
        result: &HistoryWindowResult,
        session: &mut SessionView,
    ) -> Result<WindowIncorporated, StaleResult> {
        if self.live_state != Some(result.generation) {
            return Err(StaleResult::NotInFlight);
        }
        self.live_state = None;
        let discontinuous = self
            .buffer
            .as_ref()
            .is_some_and(|buffer| buffer.discontinuous);
        if result.has_newer || discontinuous {
            // W11: more events arrived than one bounded read returns (or the host's live
            // input lost continuity since the read was issued), so this read does not reach
            // the session tail. Going live here would leave a gap between the read and
            // later live events; the read is incorporated like a newer page instead, the window
            // stays detached with newer history available, and newer paging continues (the next
            // `Newer` request buffers live events again).
            for event in &result.events {
                self.append_durable(event.clone(), session);
            }
            self.buffer = None;
            self.keys
                .set_availability(WindowSide::After, EdgeAvailability::MoreAvailable);
            let changes = session.drain_transcript_changes();
            self.refresh();
            return Ok(WindowIncorporated {
                generation: result.generation,
                target: WindowRequest::LiveState,
                changes,
                mode_changed: false,
            });
        }
        let mode_changed = self.summary.mode != WindowMode::Live;
        // The durable window stays; only the live state is applied on top (W11).
        self.set_mode(WindowMode::Live);
        for event in &result.events {
            self.append_durable(event.clone(), session);
        }
        let buffer = self.buffer.take().unwrap_or_default();
        self.apply_buffered(buffer, session);
        let changes = session.drain_transcript_changes();
        self.refresh();
        Ok(WindowIncorporated {
            generation: result.generation,
            target: WindowRequest::LiveState,
            changes,
            mode_changed,
        })
    }

    /// Apply one live input (W2, W11).
    pub fn apply_live(&mut self, input: LiveInput, session: &mut SessionView) -> LiveOutcome {
        if let LiveInput::Durable(event) = &input
            && self.events.contains_key(&event.sequence)
        {
            return LiveOutcome::Duplicate;
        }
        if self.summary.mode == WindowMode::Detached || self.live_state.is_some() {
            // W2: newer activity is recorded whether or not the input is buffered, because a
            // buffer is discarded when its page reports more history (W11). A stream seen
            // while a page is in flight is therefore still in flight when the next `Newer`
            // request is issued, and that request rehydrates live state.
            match &input {
                LiveInput::Durable(event) => {
                    self.summary.latest_known_sequence = Some(
                        self.summary
                            .latest_known_sequence
                            .map_or(event.sequence, |known| known.max(event.sequence)),
                    );
                }
                LiveInput::Stream(_) => self.stream_activity = true,
            }
            if let Some(buffer) = self.buffer.as_mut() {
                buffer.push(input);
                return LiveOutcome::Buffered;
            }
            return LiveOutcome::Detached;
        }
        match input {
            LiveInput::Durable(event) => {
                if self
                    .summary
                    .last_sequence
                    .is_some_and(|last| event.sequence <= last)
                {
                    return LiveOutcome::Duplicate;
                }
                self.append_durable(*event, session);
            }
            LiveInput::Stream(event) => session.apply_live_event(&event),
        }
        let changes = session.drain_transcript_changes();
        self.refresh();
        LiveOutcome::Applied(changes)
    }

    fn append_durable(&mut self, event: SessionEvent, session: &mut SessionView) {
        if self
            .summary
            .last_sequence
            .is_some_and(|last| event.sequence <= last)
        {
            return;
        }
        let reduced = session.window_op_counters().events_reduced;
        session.apply_window_event(&event);
        self.count_reduced(reduced, session);
        let _ = self.keys.push_back([ResidentItem::new(event.sequence)]);
        self.summary.last_sequence = Some(event.sequence);
        self.summary.last_available_sequence = Some(
            self.summary
                .last_available_sequence
                .map_or(event.sequence, |known| known.max(event.sequence)),
        );
        self.events.insert(event.sequence, event);
        self.last_extended = Some(PageDirection::Newer);
    }

    fn apply_buffered(&mut self, buffer: LiveBuffer, session: &mut SessionView) {
        for input in buffer.inputs {
            match input {
                LiveInput::Durable(event) => self.append_durable(*event, session),
                LiveInput::Stream(event) => session.apply_live_event(&event),
            }
        }
    }

    // ---- eviction ----

    /// Evict whole turns after incorporation (W7, W7a, W9, SD3).
    ///
    /// Runs only while more than 1,024 events are resident and eviction is not deferred. It
    /// trims the side opposite to the side last extended, down toward 512 events, never
    /// touching the anchor unit, pinned units, streaming items or the protected band
    /// (`prefetch_rows` beyond each viewport edge, capped at 512 events, nearest first). Cuts
    /// are at turn starts; a single turn larger than the target is cut unaligned. Tail-side
    /// eviction in `Live` mode switches to `Detached` and discards live-only items (W9).
    pub fn evict(
        &mut self,
        context: &EvictionContext<'_>,
        units: &Units,
        height: &dyn Fn(&UnitKey) -> u64,
        session: &mut SessionView,
    ) -> Option<EvictionOutcome> {
        let over_ceiling = self.event_count() > RESIDENT_CEILING_EVENTS;
        if (context.deferred && !over_ceiling)
            || self.event_count() <= RESIDENT_MAX_EVENTS
            || self.events.is_empty()
        {
            return None;
        }
        // W7a: while eviction is deferred, durable events applied without a page request
        // (buffered catch-up, live-state reads, live events) can still pass the ceiling; trim
        // only back to it so as little as possible changes during the motion or selection.
        let target = if context.deferred {
            RESIDENT_CEILING_EVENTS
        } else {
            RESIDENT_TARGET_EVENTS
        };
        let probe = self.eviction_probe(context, session, target);
        if self.no_eviction.as_ref() == Some(&probe) {
            return None;
        }
        self.work.eviction_plans += 1;
        let Some((plan, side, unaligned)) =
            self.plan_eviction(context, units, height, session, target)
        else {
            self.no_eviction = Some(probe);
            return None;
        };
        let had_in_flight = self.keys.in_flight(side).is_some();
        let splices = self.keys.apply_eviction(&plan).ok()?;
        let mut removed_events = 0;
        let mut first_removed: Option<u64> = None;
        for splice in &splices {
            for sequence in &splice.removed {
                self.events.remove(sequence);
                removed_events += 1;
                first_removed = Some(first_removed.map_or(*sequence, |first| first.min(*sequence)));
            }
        }
        if had_in_flight {
            // The trimmed side's page was cancelled; fence its result by generation.
            self.summary.generation += 1;
            if side == WindowSide::After {
                self.buffer = None;
            }
        }
        if side == WindowSide::After && self.live_state.take().is_some() {
            // W11: a live-state read continues after the old tail. Its result would leave a gap
            // after the trimmed tail, so it is cancelled (its result is dropped as not in
            // flight) and newer paging resumes from the new tail.
            self.buffer = None;
        }
        let direction = match side {
            WindowSide::Before => PageDirection::Older,
            WindowSide::After => PageDirection::Newer,
        };
        let mode_changed = side == WindowSide::After && self.summary.mode == WindowMode::Live;
        if mode_changed {
            self.set_mode(WindowMode::Detached);
            self.keys
                .set_availability(WindowSide::After, EdgeAvailability::MoreAvailable);
        }
        // Whole turns leave the projection by key; no resident event is replayed (SD3).
        let changes = match side {
            WindowSide::Before => {
                session.evict_before_sequence(self.first_sequence().unwrap_or(u64::MAX))
            }
            WindowSide::After => session.evict_from_sequence(first_removed.unwrap_or(u64::MAX)),
        };
        self.work.evictions += 1;
        self.refresh();
        Some(EvictionOutcome {
            side: direction,
            removed_events,
            unaligned,
            changes,
            mode_changed,
        })
    }

    /// Plan an eviction: aligned at turn starts first, unaligned when one turn larger than the
    /// target leaves no aligned cut (W7). Returns the plan, the trimmed side and whether the
    /// cut is unaligned.
    fn plan_eviction(
        &self,
        context: &EvictionContext<'_>,
        units: &Units,
        height: &dyn Fn(&UnitKey) -> u64,
        session: &SessionView,
        target: u64,
    ) -> Option<(rw::EvictionPlan, WindowSide, bool)> {
        let rows = units.rows_by_sequence(height);
        let starts = self
            .events
            .values()
            .filter(|event| is_safe_window_start(event))
            .map(|event| event.sequence)
            .collect::<BTreeSet<_>>();
        let boundary_rows = height(&UnitKey::Boundary(BoundaryEdge::Older));
        let event_top = i128::from(context.viewport_top) - i128::from(boundary_rows);
        let anchor = context
            .anchor
            .and_then(|key| units.sequence(key))
            .filter(|sequence| self.events.contains_key(sequence))
            .unwrap_or_else(|| self.sequence_at_row(&rows, event_top));
        let anchor_top = self
            .events
            .keys()
            .take_while(|sequence| **sequence < anchor)
            .map(|sequence| i128::from(rows.get(sequence).copied().unwrap_or(0)))
            .sum::<i128>();
        let top_from_anchor = i64::try_from(event_top - anchor_top).unwrap_or(i64::MAX);
        let mut pinned = context
            .pinned
            .iter()
            .filter_map(|key| units.sequence(key))
            .collect::<Vec<_>>();
        pinned.extend(streaming_sequences(&session.snapshot().transcript.items));

        let side = self.eviction_side();
        let band = ProtectedBand::symmetric(prefetch_rows(context.viewport_height), usize::MAX)
            .max_weight(RESIDENT_TARGET_EVENTS);
        let budget = EvictionBudget {
            trigger_weight: RESIDENT_MAX_EVENTS,
            target_weight: target,
            sides: EvictionSides::Only(side),
        };
        let placement = ViewportPlacement {
            anchor: &anchor,
            top_from_anchor,
            height: context.viewport_height,
        };
        let plan_with = |aligned: bool| {
            self.keys.plan_eviction(&EvictionInput {
                placement,
                band,
                budget,
                pinned: &pinned,
                rows: &EventRows {
                    rows: &rows,
                    starts: &starts,
                    aligned,
                },
            })
        };
        let mut unaligned = false;
        let mut plan = plan_with(true).ok()?;
        if plan.is_empty() {
            plan = plan_with(false).ok()?;
            unaligned = true;
        }
        if plan.is_empty() {
            return None;
        }
        Some((plan, side, unaligned))
    }

    /// The side eviction trims: opposite to the side last extended (W7).
    const fn eviction_side(&self) -> WindowSide {
        match self.last_extended {
            Some(PageDirection::Older) => WindowSide::After,
            Some(PageDirection::Newer) | None => WindowSide::Before,
        }
    }

    fn eviction_probe(
        &self,
        context: &EvictionContext<'_>,
        session: &SessionView,
        target: u64,
    ) -> EvictionProbe {
        EvictionProbe {
            keys_revision: self.keys.revision(),
            transcript_revision: session.snapshot().transcript.revision,
            last_extended: self.last_extended,
            viewport_top: context.viewport_top,
            viewport_height: context.viewport_height,
            resident_rows: context.resident_rows,
            anchor: context.anchor.cloned(),
            pinned: context.pinned.to_vec(),
            target,
        }
    }

    fn sequence_at_row(&self, rows: &BTreeMap<u64, u64>, row: i128) -> u64 {
        let mut top = 0_i128;
        let mut last = 0;
        for sequence in self.events.keys() {
            let height = i128::from(rows.get(sequence).copied().unwrap_or(0));
            last = *sequence;
            if height > 0 && row < top + height {
                return *sequence;
            }
            top += height;
        }
        last
    }

    // ---- internals ----

    fn first_sequence(&self) -> Option<u64> {
        self.events.keys().next().copied()
    }

    fn set_mode(&mut self, mode: WindowMode) {
        self.summary.mode = mode;
        if mode == WindowMode::Live {
            self.keys
                .set_availability(WindowSide::After, EdgeAvailability::Exhausted);
            self.summary.latest_known_sequence = None;
            self.stream_activity = false;
        }
    }

    /// Replace the projection with the resident events (window results and the SD2 fallback).
    /// Live windows keep live-only items; detached windows drop them (W2, SD3).
    fn replace(&mut self, session: &mut SessionView) -> TranscriptChanges {
        let events = self.events.values().cloned().collect::<Vec<_>>();
        let mode = self.summary.mode;
        self.work.replacements += 1;
        self.measured(session, |session| match mode {
            WindowMode::Live => session.replace_history_window(&events),
            WindowMode::Detached => session.replace_history_window_detached(&events),
        })
    }

    /// Incorporate a page operation's result, falling back to a replacement of the resident
    /// events when the page cannot be spliced (SD2: unaligned boundary, W4; any other rejection
    /// is answered the same way so the projection always matches the resident events).
    fn page_or_fallback(
        &mut self,
        result: Result<TranscriptChanges, WindowOpError>,
        direction: PageDirection,
        session: &mut SessionView,
    ) -> TranscriptChanges {
        let Ok(changes) = result else {
            self.work.fallbacks += 1;
            return self.replace(session);
        };
        match direction {
            PageDirection::Older => self.work.prepends += 1,
            PageDirection::Newer => self.work.appends += 1,
        }
        changes
    }

    /// Run a `SessionView` operation and add the events it reduced to [`WindowWork`].
    fn measured<T>(
        &mut self,
        session: &mut SessionView,
        operation: impl FnOnce(&mut SessionView) -> T,
    ) -> T {
        let reduced = session.window_op_counters().events_reduced;
        let output = operation(session);
        self.count_reduced(reduced, session);
        output
    }

    const fn count_reduced(&mut self, before: u64, session: &SessionView) {
        self.work.events_reduced += session
            .window_op_counters()
            .events_reduced
            .saturating_sub(before);
    }

    fn refresh(&mut self) {
        let summary = &mut self.summary;
        summary.first_sequence = self.events.keys().next().copied();
        summary.last_sequence = self.events.keys().next_back().copied();
        summary.event_count = self.keys.total_weight();
        summary.has_older = self.attached
            && self.keys.availability(WindowSide::Before) == EdgeAvailability::MoreAvailable;
        summary.has_newer = summary.mode == WindowMode::Detached
            && self.keys.availability(WindowSide::After) == EdgeAvailability::MoreAvailable;
        summary.in_flight.older = self.keys.in_flight(WindowSide::Before).is_some();
        summary.in_flight.newer =
            self.keys.in_flight(WindowSide::After).is_some() || self.live_state.is_some();
        summary.in_flight.replace = self.replace.map(|(_, request)| request);
    }
}

/// Append `later` to `changes`, keeping the combined revision range.
fn append_changes(changes: &mut TranscriptChanges, later: TranscriptChanges) {
    if later.is_empty() && later.revision == changes.revision {
        return;
    }
    changes.changes.extend(later.changes);
    changes.revision = later.revision;
}

fn streaming_sequences(items: &[TranscriptViewItem]) -> impl Iterator<Item = u64> + '_ {
    items
        .iter()
        .filter(|item| item.streaming)
        .filter_map(|item| item.sequence)
}
