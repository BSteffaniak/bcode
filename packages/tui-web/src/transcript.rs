//! The browser host of the V2 transcript engine (TUI V2 specification section 3.0, browser
//! host section).
//!
//! The engine runs in the browser exactly as it runs natively. This module owns what the
//! engine deliberately does not: the feed it is fed from, the frames it painted and the
//! browser's render inputs.
//!
//! * Live input ([`crate::feed::LIVE_ROUTE`]): the host subscribes first, then attaches (the
//!   bounded tail window), so no input between the two is lost; durable duplicates are dropped
//!   by the engine by sequence. A gap in the feed reloads the tail of a live window (W10), as
//!   native resynchronization does. Live input arriving while a window replacement is in
//!   flight is held and applied after it (W3).
//! * History requests ([`crate::history`]): every engine request runs the shared bounded
//!   loader; failed reads are re-issued with backoff while the engine still awaits them.
//! * Local rows (LR1–LR7): the one unresolved submission of this browser is a pending row
//!   until its durable user message is incorporated; an accepted submission reveals its unit
//!   (T2, T27).
//! * Render inputs (L2, L3, L8): the exported resolved theme, the `<details>` state and the
//!   accepted plugin visuals ([`crate::visuals`]); structure from the theme's policy layers.
//! * Frames (CK1–CK4): the last prepared frame and the last committed one; pointer input
//!   resolves only against the committed frame.

use std::collections::BTreeMap;
use std::sync::Arc;

use bcode_session_view_models::{
    ChatMessageView, TextFormat, TranscriptViewItem, TranscriptViewItemId, TranscriptViewItemKind,
};
use bcode_tui_theme::definition::ResolvedThemeDefinition;
use bcode_tui_transcript::engine::{EngineUpdate, HostRequest, TranscriptEngine};
use bcode_tui_transcript::frame::{FrameClock, FrameSnapshot};
use bcode_tui_transcript::layout::RenderConfig;
use bcode_tui_transcript::layout::context::MarkdownOptions;
use bcode_tui_transcript::navigation::MotionConfig;
use bcode_tui_transcript::units::UnitKey;
use bcode_tui_transcript::units::local::{LocalKey, LocalPlacement, LocalRow};
use bcode_tui_transcript::units::structure::PolicyLayers;
use bcode_tui_transcript::viewport::ViewportGeometry;
use bcode_tui_transcript::window::{
    HistoryLoadError, HistoryLoaded, LIVE_BUFFER_EVENTS, LiveInput, SessionId, WindowMode,
};
use bmux_tui_components::theme::ComponentTheme;
use web_time::Instant;

use crate::feed::{
    Continuity, FeedError, HISTORY_EVENT_TOO_LARGE, LiveCursor, LiveRequest, decode_history_page,
    decode_live_batch,
};
use crate::history::{Completed, HistoryPump, HistoryRead, HistoryRetries};
use crate::visuals::BrowserVisuals;

/// Label of this browser's unresolved submission row.
const PENDING_LABEL: &str = "sending";

/// A submission this browser sent whose user message is not incorporated yet.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Handoff {
    row: LocalRow,
    /// Canonical sequence of the accepting user message, when the receipt names it.
    sequence: Option<u64>,
}

/// The engine, its feed and its frames.
pub struct BrowserTranscript {
    engine: TranscriptEngine,
    session_id: SessionId,
    pub(crate) visuals: BrowserVisuals,
    pump: HistoryPump,
    retries: HistoryRetries,
    /// Engine requests not yet started as loader jobs.
    requests: Vec<HostRequest>,
    /// Live inputs that arrived while a window replacement was in flight.
    held: Vec<LiveInput>,
    held_overflowed: bool,
    /// Resume cursor of the live feed; `None` until subscribed.
    cursor: Option<LiveCursor>,
    /// The history request that failed permanently (an event beyond the browser's response
    /// bound): the transcript is degraded until a later request loads.
    unavailable: Option<HostRequest>,
    /// The engine requested its attach window.
    attached: bool,
    /// Theme the engine renders with.
    theme: Arc<ResolvedThemeDefinition>,
    /// `<details>` open state by content key.
    details_open: BTreeMap<String, bool>,
    /// The render configuration must be rebuilt.
    render_dirty: bool,
    /// The unresolved submission shown as a pending row (identity, text).
    pending: Option<(String, String)>,
    handoffs: Vec<Handoff>,
    /// Local rows last handed to the engine.
    local_rows: Option<Vec<LocalRow>>,
    /// Identity and receipt sequence of a settled submission whose message is not applied yet.
    awaiting_acceptance: Option<(String, u64)>,
    last_frame: Option<FrameSnapshot>,
    committed_frame: Option<FrameSnapshot>,
}

impl BrowserTranscript {
    /// A detached engine for `session_id`; it attaches once the live feed is subscribed.
    #[must_use]
    pub fn new(session_id: SessionId) -> Self {
        let mut engine =
            TranscriptEngine::new(ViewportGeometry::new(0, 0), MotionConfig::default());
        let at = Instant::now();
        let _ = engine.set_latest_bar_enabled(true, at);
        let _ = engine.set_streaming_policy(
            bcode_session_view_models::StreamingPresentationPolicy::default(),
            at,
        );
        Self {
            engine,
            session_id,
            visuals: BrowserVisuals::default(),
            pump: HistoryPump::new(session_id),
            retries: HistoryRetries::default(),
            requests: Vec::new(),
            held: Vec::new(),
            held_overflowed: false,
            cursor: None,
            unavailable: None,
            attached: false,
            theme: Arc::new(ResolvedThemeDefinition::empty("default", "Default")),
            details_open: BTreeMap::new(),
            render_dirty: true,
            pending: None,
            handoffs: Vec::new(),
            local_rows: None,
            awaiting_acceptance: None,
            last_frame: None,
            committed_frame: None,
        }
    }

    /// The engine (read-only).
    #[must_use]
    pub const fn engine(&self) -> &TranscriptEngine {
        &self.engine
    }

    /// The engine, for inputs whose update the caller absorbs ([`Self::absorb`]).
    pub const fn engine_mut(&mut self) -> &mut TranscriptEngine {
        &mut self.engine
    }

    /// The engine and the plugin visuals, for image admission.
    pub const fn engine_and_visuals(&mut self) -> (&TranscriptEngine, &mut BrowserVisuals) {
        (&self.engine, &mut self.visuals)
    }

    /// Record an engine update: queue its history requests. Returns whether it changed
    /// anything (an invalidation or a request).
    pub fn absorb(&mut self, update: EngineUpdate) -> bool {
        let changed = update.invalidated || !update.requests.is_empty();
        self.requests.extend(update.requests);
        changed
    }

    /// Resident transcript item `id`.
    #[must_use]
    pub fn item(&self, id: &TranscriptViewItemId) -> Option<&TranscriptViewItem> {
        crate::visuals::resident_item(&self.engine, id)
    }

    // ---- live feed ------------------------------------------------------------------------

    /// Body of the next live poll: the resume cursor, or a subscription before the first.
    ///
    /// # Errors
    ///
    /// Returns an error if the request cannot be serialized.
    pub fn live_request(&self) -> Result<String, &'static str> {
        serde_json::to_string(&LiveRequest {
            session_id: self.session_id,
            cursor: self.cursor,
        })
        .map_err(|_| "invalid live request")
    }

    /// Accept one live batch (the original response bytes). A subscription attaches the
    /// engine; a gap reloads the tail of a live window. Returns whether the transcript changed.
    ///
    /// # Errors
    ///
    /// Rejects error envelopes, unknown wire versions, oversized and malformed responses.
    pub fn accept_live(&mut self, bytes: &[u8], at: Instant) -> Result<bool, &'static str> {
        let batch = decode_live_batch(bytes).map_err(|_| "invalid live batch")?;
        let mut changed = false;
        if batch.continuity == Continuity::Gap {
            changed |= self.resynchronize(at);
        }
        self.cursor = Some(batch.cursor);
        if !self.attached {
            self.attached = true;
            let update = self.engine.attach(at);
            changed |= self.absorb(update);
        }
        for input in batch.inputs {
            changed |= self.apply_live_input(input, at);
        }
        Ok(changed)
    }

    fn apply_live_input(&mut self, input: LiveInput, at: Instant) -> bool {
        if self.engine.window().in_flight.replace.is_some() {
            if self.held.len() < LIVE_BUFFER_EVENTS {
                self.held.push(input);
            } else {
                self.held_overflowed = true;
            }
            return false;
        }
        let durable = match &input {
            LiveInput::Durable(event) => Some(event.sequence),
            LiveInput::Stream(_) => None,
        };
        let update = self.engine.apply_live(input, at);
        let mut changed = self.absorb(update);
        if durable.is_some_and(|sequence| {
            self.awaiting_acceptance
                .as_ref()
                .is_some_and(|(_, awaited)| sequence >= *awaited)
        }) {
            changed |= self.reveal_incorporated_submission(at);
        }
        changed
    }

    fn release_held(&mut self, at: Instant) -> bool {
        if self.engine.window().in_flight.replace.is_some() {
            return false;
        }
        let mut changed = false;
        for input in std::mem::take(&mut self.held) {
            changed |= self.apply_live_input(input, at);
        }
        if std::mem::replace(&mut self.held_overflowed, false)
            && self.engine.window().mode == WindowMode::Live
        {
            let update = self.engine.attach(at);
            changed |= self.absorb(update);
        }
        changed
    }

    /// Continuity was lost (the feed skipped inputs):
    ///
    /// * A window replacement in flight: the inputs held for it are no longer contiguous, so
    ///   they are dropped and a window that lands live reloads the tail (W3, W10).
    /// * A detached window: a `Newer` page or live-state read in flight rehydrates live state
    ///   instead of going live with inputs buffered across the gap (W11); otherwise newer
    ///   history loads on demand.
    /// * A live window reloads the tail (W10), which also rehydrates in-flight live state.
    fn resynchronize(&mut self, at: Instant) -> bool {
        if !self.attached {
            return false;
        }
        if self.engine.window().in_flight.replace.is_some() {
            self.held.clear();
            self.held_overflowed = true;
            return false;
        }
        if self.engine.window().mode != WindowMode::Live {
            let _ = self.engine.interrupt_live_buffer();
            return false;
        }
        self.held.clear();
        self.held_overflowed = false;
        let update = self.engine.attach(at);
        self.absorb(update)
    }

    // ---- history requests -----------------------------------------------------------------

    /// Start the engine's awaited requests as loader jobs and drop jobs it no longer awaits.
    /// Returns whether the transcript changed (a request that needed no read completes here).
    pub fn start_requests(&mut self, at: Instant) -> bool {
        let engine = &self.engine;
        self.pump.retain(|request| engine.is_awaiting(request));
        let mut changed = false;
        for request in std::mem::take(&mut self.requests) {
            if !self.engine.is_awaiting(&request) || self.pump.is_loading(&request) {
                continue;
            }
            let resident_last = self.engine.window().last_sequence;
            if let Some(completed) = self.pump.start(request, resident_last) {
                changed |= self.complete(completed, at);
            }
        }
        changed
    }

    /// The next history read to transport, after starting newly issued requests.
    ///
    /// # Errors
    ///
    /// Returns an error if a request body cannot be serialized.
    pub fn next_history_read(&mut self, at: Instant) -> Result<Option<HistoryRead>, &'static str> {
        let _ = self.start_requests(at);
        self.pump.next_read()
    }

    /// Answer history read `claim` with the original response bytes, or with `None` when the
    /// transport failed. Returns whether the transcript changed.
    ///
    /// # Errors
    ///
    /// Rejects claims that are not in flight (superseded or already answered).
    pub fn accept_history_page(
        &mut self,
        claim: u64,
        bytes: Option<&[u8]>,
        at: Instant,
    ) -> Result<bool, &'static str> {
        if !self.pump.owns(claim) {
            return Err("stale history claim");
        }
        let page = bytes.map_or(Err(crate::feed::FeedError::Malformed), decode_history_page);
        let Some(completed) = self.pump.deliver(claim, page) else {
            return Ok(false);
        };
        Ok(self.complete(completed, at))
    }

    fn complete(&mut self, completed: Completed, at: Instant) -> bool {
        let Completed { request, result } = completed;
        let loaded = match result {
            Ok(loaded) => loaded,
            Err(HistoryLoadError::Read(FeedError::Server(code)))
                if code == HISTORY_EVENT_TOO_LARGE =>
            {
                // Retrying cannot succeed: the transcript is degraded until another request
                // (a jump elsewhere, a reattachment) loads (session persistence: surface a
                // degraded state, never retry without end).
                self.retries.clear(&request);
                return self.unavailable.replace(request).is_none();
            }
            Err(HistoryLoadError::Read(_) | HistoryLoadError::InconsistentPage) => {
                // The engine keeps the request in flight (it has no failure input): re-issue it
                // with backoff while it is still awaited.
                if self.engine.is_awaiting(&request) {
                    self.retries.schedule(request, at);
                }
                return false;
            }
        };
        self.retries.clear(&request);
        let mut changed = self.unavailable.take().is_some();
        let applied = match &loaded {
            HistoryLoaded::Page(page) => self.engine.apply_page(page, at),
            HistoryLoaded::Window(window) => self.engine.apply_window(window, at),
        };
        changed |= match applied {
            Ok(update) => {
                self.absorb(update);
                true
            }
            Err(_stale) => false,
        };
        changed |= self.release_held(at);
        changed |= self.reveal_incorporated_submission(at);
        changed
    }

    /// A history request failed permanently: the transcript is degraded (an event exceeds
    /// the browser's response bound) until a later request loads.
    #[must_use]
    pub const fn history_unavailable(&self) -> bool {
        self.unavailable.is_some()
    }

    // ---- timers ---------------------------------------------------------------------------

    /// Earliest engine timer, paced live text or history retry to wake for.
    #[must_use]
    pub fn next_deadline(&self, now: Instant) -> Option<Instant> {
        [
            self.engine.next_deadline(),
            self.retries.next_due(),
            self.engine.next_streaming_deadline(now),
        ]
        .into_iter()
        .flatten()
        .min()
    }

    /// Fire due timers, release paced live text and re-issue due history retries. Returns
    /// whether anything changed.
    pub fn advance(&mut self, now: Instant) -> bool {
        let update = self.engine.advance(now);
        let mut changed = self.absorb(update);
        let update = self.engine.advance_streaming(now);
        changed |= self.absorb(update);
        for request in self.retries.take_due(now) {
            if self.engine.is_awaiting(&request) {
                self.requests.push(request);
                changed = true;
            }
        }
        changed
    }

    // ---- submissions ----------------------------------------------------------------------

    /// Show `pending` (identity, text) as this browser's unresolved submission row.
    pub fn set_pending_submission(&mut self, pending: Option<(String, String)>) {
        self.pending = pending;
    }

    /// The submission `identity` with `text` was durably accepted; its user message is the
    /// canonical `sequence` when the receipt names one. The row stays until the message is
    /// incorporated, then the accepted unit is revealed (T2, T27). Returns whether the
    /// transcript changed.
    pub fn submission_settled(
        &mut self,
        identity: &str,
        text: &str,
        sequence: Option<u64>,
        at: Instant,
    ) -> bool {
        self.pending = None;
        let Some(sequence) = sequence else {
            return false;
        };
        self.handoffs.push(Handoff {
            row: pending_row(identity, text),
            sequence: Some(sequence),
        });
        self.awaiting_acceptance = Some((identity.to_owned(), sequence));
        self.reveal_incorporated_submission(at)
    }

    fn incorporated(&self, sequence: u64) -> bool {
        let window = self.engine.window();
        window.first_sequence.is_some_and(|first| first <= sequence)
            && window.last_sequence.is_some_and(|last| last >= sequence)
    }

    fn reveal_incorporated_submission(&mut self, at: Instant) -> bool {
        match self.awaiting_acceptance.take() {
            Some((identity, sequence)) if self.incorporated(sequence) => {
                self.reveal_submission(&identity, sequence, at)
            }
            awaiting => {
                self.awaiting_acceptance = awaiting;
                false
            }
        }
    }

    /// Reveal the accepted unit of submission `identity`: its submission item, else the user
    /// message at the receipt's `sequence` (T2).
    fn reveal_submission(&mut self, identity: &str, sequence: u64, at: Instant) -> bool {
        let submission = TranscriptViewItemId::new(format!("submission:{identity}"));
        let items = &self.engine.session_view().snapshot().transcript.items;
        let unit = items
            .iter()
            .find(|item| item.id == submission)
            .or_else(|| {
                items.iter().find(|item| {
                    item.sequence.is_some_and(|item| item >= sequence)
                        && matches!(item.kind, TranscriptViewItemKind::UserMessage { .. })
                })
            })
            .map_or_else(
                || UnitKey::Item(TranscriptViewItemId::event(sequence)),
                |item| UnitKey::Item(item.id.clone()),
            );
        let update = self.engine.submission_accepted(unit, at);
        self.absorb(update)
    }

    /// Hand the engine the local rows of the pending submission and of settled submissions
    /// whose message is not incorporated yet. Unchanged rows are a no-op. Returns whether the
    /// transcript changed.
    pub fn sync_local_rows(&mut self, at: Instant) -> bool {
        let window = self.engine.window();
        let incorporated = |sequence: u64| {
            window.first_sequence.is_some_and(|first| first <= sequence)
                && window.last_sequence.is_some_and(|last| last >= sequence)
        };
        self.handoffs.retain(|handoff| {
            handoff
                .sequence
                .is_some_and(|sequence| !incorporated(sequence))
        });
        let rows = self
            .handoffs
            .iter()
            .map(|handoff| handoff.row.clone())
            .chain(
                self.pending
                    .as_ref()
                    .map(|(identity, text)| pending_row(identity, text)),
            )
            .collect::<Vec<_>>();
        if self.local_rows.as_ref() == Some(&rows) {
            return false;
        }
        self.local_rows = Some(rows.clone());
        let update = self.engine.set_local_rows(rows, at);
        self.absorb(update)
    }

    // ---- render inputs --------------------------------------------------------------------

    /// Install an effective presentation policy: its resolved theme (and its motion), the
    /// structure its policy layers compile to and the producer set its visual policy enables.
    ///
    /// # Errors
    ///
    /// Rejects visual policies beyond the registry's bounds.
    pub fn set_policy(
        &mut self,
        policy: &crate::policy::BrowserPolicy,
        at: Instant,
    ) -> Result<(), &'static str> {
        self.visuals.configure(&policy.visual_policy)?;
        let theme = policy.resolved_theme();
        let transcript = policy.transcript;
        let (structure, _diagnostics) = PolicyLayers {
            theme: Some(&theme),
            reasoning_visible: transcript.reasoning_visible(),
            ..PolicyLayers::default()
        }
        .compile_with_host_parts();
        let motion = MotionConfig::for_motion(theme.chrome.motion, transcript.reduce_motion);
        self.theme = theme;
        let update = self.engine.set_structure_config(structure, at);
        self.absorb(update);
        let update = self.engine.set_motion(motion, at);
        self.absorb(update);
        let update = self
            .engine
            .set_streaming_policy(transcript.streaming.normalized(), at);
        self.absorb(update);
        self.render_dirty = true;
        Ok(())
    }

    /// Open or close one `<details>` block by content key; applied at the next sync.
    pub fn set_details(&mut self, key: String, open: bool) {
        if self.details_open.get(&key) != Some(&open) {
            self.details_open.insert(key, open);
            self.render_dirty = true;
        }
    }

    /// Bring accepted plugin visuals and the render configuration up to date, outside drawing.
    /// `theme` and `style` are the component theme adapters render with and its identity.
    /// Returns whether a configuration was applied.
    pub fn sync_render(&mut self, theme: Option<&ComponentTheme>, style: u64, at: Instant) -> bool {
        // Visual routing measures body widths with the current render theme.
        if self.render_dirty {
            self.apply_render_config(at);
        }
        if self.visuals.sync(&self.engine, theme, style) {
            self.apply_render_config(at);
            return true;
        }
        false
    }

    fn apply_render_config(&mut self, at: Instant) {
        self.render_dirty = false;
        let config = RenderConfig {
            theme: Arc::clone(&self.theme),
            markdown: MarkdownOptions {
                details_open: self.details_open.clone(),
                ..MarkdownOptions::default()
            },
            plugin_visuals: self.visuals.accepted().clone(),
            ..RenderConfig::default()
        };
        let update = self.engine.set_render_config(config, at);
        self.absorb(update);
    }

    /// Plugin visuals still wait for routing beyond the last sync's bound.
    #[must_use]
    pub const fn visuals_pending(&self) -> bool {
        self.visuals.pending()
    }

    // ---- frames ---------------------------------------------------------------------------

    /// Size the transcript region (latest-bar row included) before a frame is prepared.
    /// Returns whether it changed.
    pub fn sync_geometry(&mut self, width: u16, height: u16, at: Instant) -> bool {
        let geometry = ViewportGeometry::new(u64::from(width), u64::from(height));
        if geometry == self.engine.region() {
            return false;
        }
        let update = self.engine.resize(geometry, at);
        self.absorb(update);
        true
    }

    /// Prepare the frame painted next (CK1: navigation, window and units are unchanged).
    pub fn prepare_frame(&mut self, clock: FrameClock) -> &FrameSnapshot {
        let snapshot = self.engine.prepare_frame(clock);
        self.last_frame.insert(snapshot)
    }

    /// Forget the prepared frame (an empty surface paints no transcript).
    pub fn clear_frame(&mut self) {
        self.last_frame = None;
    }

    /// The last prepared frame.
    #[must_use]
    pub const fn last_frame(&self) -> Option<&FrameSnapshot> {
        self.last_frame.as_ref()
    }

    /// The last presented frame: pointer input resolves against it (CK4).
    #[must_use]
    pub const fn committed_frame(&self) -> Option<&FrameSnapshot> {
        self.committed_frame.as_ref()
    }

    /// Record that the last prepared frame was presented (CK3).
    pub fn commit(&mut self) {
        let Some(frame) = self.last_frame.clone() else {
            self.committed_frame = None;
            return;
        };
        let update = self.engine.commit(frame.id);
        self.absorb(update);
        self.committed_frame = Some(frame);
    }
}

/// The local row of a submission this browser sent.
fn pending_row(identity: &str, text: &str) -> LocalRow {
    LocalRow::new(
        LocalKey::Submission(identity.to_owned()),
        LocalPlacement::Tail,
        TranscriptViewItem {
            id: TranscriptViewItemId::new("local"),
            revision: 1,
            sequence: None,
            timestamp_ms: None,
            output_location: None,
            streaming: false,
            kind: TranscriptViewItemKind::UserMessage {
                message: ChatMessageView {
                    submission_context: None,
                    submission_status: None,
                    activity: None,
                    text: text.to_owned(),
                    display_label: Some(PENDING_LABEL.to_owned()),
                    format: TextFormat::Markdown,
                },
            },
        },
    )
}
