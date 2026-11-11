//! BMUX rendering and input of one browser session over the V2 transcript engine.
//!
//! * Prepare (outside paint): local rows, the inline interaction reservation, accepted plugin
//!   visuals and the render configuration ([`crate::transcript::BrowserTranscript`]).
//! * Paint: one layout of the session chrome (lifecycle summary, transcript region, pinned
//!   question, composer), the engine frame prepared for the transcript region at the frame
//!   clock, the latest bar, the selection highlight and the question surface (inline over its
//!   reserved rows or pinned above the composer).
//! * Input resolves only against the last committed scene: hit regions registered from the
//!   committed frame, endpoints of its selection scene, and the engine's committed frame for
//!   item hits (CK4). Any preparation revokes transcript activation until the next commit.
use bcode_tui_components::permission::{
    decision::{PermissionDialogOutcome, PermissionDialogResolution, PermissionDialogState},
    permission_modal,
};
use bcode_tui_components::plugin_visual::action::VisualAction;
use bcode_tui_components::{
    composer::ComposerStyle,
    session_ui::{self, SessionComposerStyle, SessionUiIds},
};
use bcode_tui_transcript::engine::ItemHit;
use bcode_tui_transcript::frame::FrameClock;
use bcode_tui_transcript::layout::geometry::ItemAction;
use bcode_tui_transcript::markdown::MarkdownActivation;
use bcode_tui_transcript::navigation::{InputSource, NavIntent, Placement};
use bcode_tui_transcript::selection::{
    SELECTION_SCOPE, TranscriptSelection, TranscriptSelectionEndpoint,
};
use bcode_tui_transcript::units::UnitKey;
use bcode_tui_transcript::units::structure::InlineInteraction;
use bmux_tui::composition::TextBlock;
use bmux_tui::composition::{Column, Flex, SizeBox};
use bmux_tui::hit::{HitRegion, HitRole};
use bmux_tui::paint::LocalRect;
use bmux_tui::selection::{
    SelectionAffinity, SelectionGranularity, SelectionScene, SelectionScopeId,
};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutId},
    damage::Damage,
    event::{Event, MouseButton, MouseEvent, MouseEventKind},
    geometry::{Point, Rect, Size},
    paint::PaintCx,
    presentation::PresentationLimits,
    style::Style,
};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use bmux_tui_components::modal_frame::{ModalFrameComponent, ModalTheme};
use bmux_tui_components::text_input::{TextInputControl, TextInputState};
use bmux_web_renderer::{LocalSurface, input::BrowserInput};
use std::cell::{Cell, RefCell};
use web_time::Instant;

use crate::SnapshotState;
use crate::transcript::BrowserTranscript;

/// Hit id of the latest bar.
const LATEST: &str = "bcode-web/latest";
/// Hit id prefix of transcript item regions (`<prefix><index>` into the frame's regions).
const ITEM_HIT_PREFIX: &str = "bcode-web/transcript-item:";
const EDITOR: &str = "bcode-web/editor";
const TRANSCRIPT_REGION: &str = "bcode-web/transcript";
/// Status notice of a degraded transcript (a history read that cannot load in the browser).
const HISTORY_UNAVAILABLE: &str =
    "Transcript history unavailable here: an event exceeds the browser transfer limit";
const QUESTION_PINNED: &str = "bcode-web/question-pinned";
const IDS: SessionUiIds<'static> = SessionUiIds {
    root: "bcode-web/root",
    composer: "bcode-web/composer",
    editor: EDITOR,
    composer_size: "bcode-web/composer-size",
};
/// Rows one wheel tick scrolls.
const WHEEL_ROWS: i64 = 3;
/// Most visual artifact candidates one discovery pass considers.
const MAX_VISUAL_CANDIDATES: usize = 64;

/// Concrete local surface. This is not a portable application contract.
pub struct SessionSurface {
    pub(crate) local: LocalSurface,
    pub(crate) inner: SurfaceState,
}

/// Identity a producer artifact page was requested for.
struct PendingVisualArtifact {
    item_id: String,
    item_revision: u64,
    reference_key: String,
    offset: u64,
    length: u32,
    observation: u64,
    width: u16,
    claim: Option<String>,
}

#[derive(Default)]
struct VisualFetchState {
    offset: u64,
    attempts: u8,
    retry_at: u64,
    done: bool,
    pages: u8,
    version: Option<(u64, u64, bool)>,
    budget: u64,
}

type VisualCandidate = (String, String, String, Option<u64>);

struct VisualFetchLease {
    claim: String,
    key: String,
    expires: u64,
}

#[derive(serde::Deserialize)]
struct VisualArtifactRange {
    artifact_id: String,
    reference_key: String,
    content_type: Option<String>,
    offset: u64,
    total_bytes: u64,
    bytes: Vec<u8>,
    reference_revision: u64,
    finalized: bool,
}

/// Terminal rectangles of one prepared frame.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
struct Regions {
    /// The transcript viewport the engine paints.
    viewport: Option<Rect>,
    /// The latest bar, while shown.
    latest_bar: Option<Rect>,
}

/// A transcript selection gesture in progress: its locked scope and logical anchor.
struct SelectionDrag {
    scope: SelectionScopeId,
    anchor: TranscriptSelectionEndpoint,
}

pub struct SurfaceState {
    pub(crate) semantic: SnapshotState,
    composer: TextInputState,
    area: Rect,
    pub(crate) transcript: BrowserTranscript,
    regions: Regions,
    committed_regions: Regions,
    drag: Option<SelectionDrag>,
    focused: bool,
    focused_hit: Option<String>,
    latest_button: Cell<ButtonState>,
    bindings: crate::bindings::Bindings,
    copy_requested: bool,
    intents: Vec<serde_json::Value>,
    permission: Option<PermissionDialogState>,
    question: crate::question::QuestionSurface,
    pending_image: Option<crate::image::PendingImage>,
    pending_visual_artifact: Option<PendingVisualArtifact>,
    visual_fetches: std::collections::BTreeMap<String, VisualFetchState>,
    visual_lease: Option<VisualFetchLease>,
    next_visual_claim: u64,
    visual_cursor: usize,
    visual_discovery: Option<(Option<u64>, u64)>,
    /// Bumped by every transcript content change (visual discovery fence).
    content_generation: u64,
    permission_inflight: bool,
    committed_permission: Option<bcode_session_view_models::PermissionView>,
    committed_turn: Option<String>,
    committed_revision: Option<u64>,
    presentation_generation: u64,
    committed_generation: Option<u64>,
    cancel_pending: Option<String>,
    committed_lifecycle_summary: String,
    permission_buttons: [Cell<ButtonState>; 6],
}

impl SessionSurface {
    /// Create a session-bound local surface, with bounded presentation allocations.
    ///
    /// # Errors
    /// Rejects invalid session identifiers and excessively large cell grids.
    pub fn new(session: String, width: u16, height: u16) -> Result<Self, &'static str> {
        let area = checked_area(width, height)?;
        let session_id = session
            .parse::<bcode_tui_transcript::window::SessionId>()
            .map_err(|_| "invalid session identity")?;
        Ok(Self {
            local: LocalSurface::new(
                area,
                PresentationLimits {
                    max_cells: 131_072,
                    max_entries: 16_384,
                    max_bytes: 16 * 1024 * 1024,
                },
            ),
            inner: SurfaceState {
                semantic: SnapshotState::new(session)?,
                composer: TextInputState::new(bmux_text_edit::TextEditBuffer::new()),
                area,
                transcript: BrowserTranscript::new(session_id),
                regions: Regions::default(),
                committed_regions: Regions::default(),
                drag: None,
                focused: true,
                focused_hit: None,
                latest_button: Cell::new(ButtonState::new()),
                bindings: crate::bindings::Bindings::new(),
                copy_requested: false,
                intents: Vec::new(),
                permission: None,
                question: crate::question::QuestionSurface::default(),
                pending_image: None,
                pending_visual_artifact: None,
                visual_fetches: std::collections::BTreeMap::new(),
                visual_lease: None,
                next_visual_claim: 0,
                visual_cursor: 0,
                visual_discovery: None,
                content_generation: 0,
                permission_inflight: false,
                committed_permission: None,
                committed_turn: None,
                committed_revision: None,
                presentation_generation: 0,
                committed_generation: None,
                cancel_pending: None,
                committed_lifecycle_summary: String::new(),
                permission_buttons: std::array::from_fn(|_| Cell::new(ButtonState::new())),
            },
        })
    }

    /// Replace the accepted non-transcript observation (the snapshot variant without
    /// transcript items: permissions, interactions, queue, runtime and lifecycle) and install
    /// a changed effective presentation policy.
    ///
    /// # Errors
    /// See [`SnapshotState::accept`].
    pub fn accept_snapshot(&mut self, json: &str) -> Result<bool, &'static str> {
        if json.len() > crate::MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let value = serde_json::from_str::<serde_json::Value>(json).ok();
        let policy = value
            .as_ref()
            .and_then(|value| value.get("policy"))
            .map(|value| crate::policy::BrowserPolicy::parse(&value.to_string()))
            .transpose()?;
        if policy.is_none() {
            // The backend omits a policy whose revision this surface named
            // ([`Self::policy_revision`]); anything else must carry the complete policy.
            let named = value
                .as_ref()
                .and_then(|value| value.get("policy_revision"))
                .and_then(serde_json::Value::as_str);
            let installed = self.inner.bindings.effective.policy.as_ref();
            let unchanged = named.is_some_and(|named| {
                installed.is_some_and(|installed| installed.revision == named)
            });
            if !unchanged && (named.is_some() || installed.is_some()) {
                return Err("effective presentation policy missing");
            }
        }
        let changed = self.inner.semantic.accept(json)?;
        let policy_changed = policy.as_ref().is_some_and(|policy| {
            self.inner
                .bindings
                .effective
                .policy
                .as_ref()
                .is_none_or(|previous| previous.revision != policy.revision)
        });
        if let Some(policy) = policy.filter(|_| policy_changed) {
            self.inner.transcript.set_policy(&policy, Instant::now())?;
            self.inner.bindings.effective.replace(policy);
            self.cancel_pointer();
        }
        if changed {
            let pending = self
                .inner
                .semantic
                .snapshot()
                .and_then(|snapshot| {
                    snapshot
                        .permissions
                        .iter()
                        .find(|permission| !permission.resolved)
                })
                .cloned();
            if self
                .inner
                .permission
                .as_ref()
                .map(PermissionDialogState::permission)
                != pending.as_ref()
            {
                self.inner.permission_inflight = false;
                self.inner.permission = pending.map(|permission| {
                    let batch = permission.batch.is_some();
                    let mut dialog = PermissionDialogState::new(permission);
                    if batch {
                        dialog.focus_action(1);
                    }
                    dialog
                });
            }
            if let Some(snapshot) = self.inner.semantic.snapshot() {
                self.inner.question.accept(&snapshot.interactions);
            }
        }
        if changed || policy_changed {
            self.prepare();
        }
        Ok(changed)
    }

    /// Revision of the installed presentation policy, named in snapshot requests so the
    /// backend omits an unchanged policy; `None` before one is installed.
    #[must_use]
    pub fn policy_revision(&self) -> Option<&str> {
        self.inner
            .bindings
            .effective
            .policy
            .as_ref()
            .map(|policy| policy.revision.as_str())
    }

    /// Body of the next live poll ([`crate::feed::LIVE_ROUTE`]).
    ///
    /// # Errors
    /// Returns an error if the request cannot be serialized.
    pub fn live_request(&self) -> Result<String, &'static str> {
        self.inner.transcript.live_request()
    }

    /// Accept one live batch (original response bytes). The first batch attaches the engine.
    /// Returns whether the transcript changed.
    ///
    /// # Errors
    /// Rejects error envelopes, unknown wire versions, oversized and malformed responses.
    pub fn accept_live(&mut self, json: &str) -> Result<bool, &'static str> {
        let changed = self
            .inner
            .transcript
            .accept_live(json.as_bytes(), Instant::now())?;
        self.transcript_changed(changed);
        Ok(changed)
    }

    /// Claim the next bounded history read the engine's requests need, as
    /// `{"claim","route","body"}`; JS transports the opaque body only.
    ///
    /// # Errors
    /// Returns an error if a request cannot be serialized.
    pub fn next_host_request(&mut self) -> Result<Option<String>, &'static str> {
        let Some(read) = self.inner.transcript.next_history_read(Instant::now())? else {
            return Ok(None);
        };
        serde_json::to_string(&serde_json::json!({
            "claim": read.claim.to_string(),
            "route": crate::feed::HISTORY_PAGE_ROUTE,
            "body": read.body,
        }))
        .map(Some)
        .map_err(|_| "invalid history claim")
    }

    /// Answer history read `claim` with the original response bytes (typed error envelopes
    /// included); true requests a redraw.
    ///
    /// # Errors
    /// Rejects claims that are not in flight.
    pub fn accept_history_page(&mut self, claim: &str, json: &str) -> Result<bool, &'static str> {
        let claim = claim.parse::<u64>().map_err(|_| "stale history claim")?;
        let changed = self.inner.transcript.accept_history_page(
            claim,
            Some(json.as_bytes()),
            Instant::now(),
        )?;
        self.transcript_changed(changed);
        Ok(changed)
    }

    /// Release history read `claim` after a transport failure; the engine request is
    /// re-issued with backoff while it is still awaited.
    pub fn fail_history_page(&mut self, claim: &str) {
        if let Ok(claim) = claim.parse::<u64>() {
            let _ = self
                .inner
                .transcript
                .accept_history_page(claim, None, Instant::now());
        }
    }

    /// Milliseconds until the transcript needs [`Self::advance`] and a redraw (engine timers,
    /// paced live text, history retries, visuals beyond one sync's bound); `None` when idle.
    #[must_use]
    pub fn next_deadline_ms(&self) -> Option<f64> {
        let now = Instant::now();
        if self.inner.transcript.visuals_pending() {
            return Some(0.0);
        }
        self.inner
            .transcript
            .next_deadline(now)
            .map(|deadline| deadline.saturating_duration_since(now).as_secs_f64() * 1000.0)
    }

    /// Fire due transcript timers. Returns whether a redraw is needed.
    pub fn advance(&mut self) -> bool {
        let changed = self.inner.transcript.advance(Instant::now());
        let changed = changed || self.inner.transcript.visuals_pending();
        self.transcript_changed(changed);
        changed
    }

    fn transcript_changed(&mut self, changed: bool) {
        if changed {
            self.inner.content_generation = self.inner.content_generation.wrapping_add(1);
            self.prepare();
        }
    }

    /// Explicitly reopen the refreshed permission presentation after an ambiguous
    /// control. This only changes local gating; no decision is sent or replayed.
    pub fn acknowledge_control(&mut self) {
        self.inner.cancel_pending = None;
        self.inner.committed_turn = None;
        self.inner.permission_inflight = false;
        self.inner.question.acknowledge();
        self.inner.committed_permission = None;
        self.cancel_pointer();
    }

    /// Change the cell grid. Old pointer capture is cancelled immediately.
    ///
    /// # Errors
    /// Rejects grids exceeding the bounded cell budget.
    pub fn resize(&mut self, width: u16, height: u16) -> Result<(), &'static str> {
        let area = checked_area(width, height)?;
        self.inner.question.invalidate_geometry();
        self.inner.bindings.effective.cancel_chord();
        self.cancel_pointer();
        if area.is_empty() {
            self.inner.clear_selection();
            self.inner.focused_hit = None;
        }
        self.inner.area = area;
        self.local.resize(area);
        self.prepare();
        Ok(())
    }

    /// Cancel pointer capture (including blur/disposal).
    pub fn cancel_pointer(&mut self) {
        self.local.cancel_pointer();
        self.inner.drag = None;
        // A gesture cancelled mid-drag keeps what it selected but stops extending it.
        let selection = self.inner.transcript.engine().selection().cloned();
        let update = self.inner.transcript.engine_mut().set_selection(
            selection.filter(|selection| !selection.is_collapsed()),
            false,
        );
        self.inner.transcript.absorb(update);
        // Reset editor gesture state while retaining real text and source selection.
        let state = &mut self.inner.composer;
        let area = state.content_area();
        let position = Point::new(area.x, area.y);
        TextInputControl::new(&session_ui::composer_policy()).handle_mouse(
            state,
            MouseEvent::new(MouseEventKind::Up(MouseButton::Left), position),
        );
    }

    /// Supply the platform monotonic clock immediately before an input event.
    /// This carries no application authority; it only times native multi-tap bindings.
    pub fn set_event_time(&mut self, millis: u64) {
        self.inner.bindings.effective.set_event_time(millis);
    }

    /// Route a bounded bridge event against the last successfully displayed scene.
    /// Returns true when the shared editor requests a submit.
    ///
    /// # Errors
    /// Rejects oversized or malformed bridge input.
    pub fn input(&mut self, json: &str) -> Result<bool, &'static str> {
        let area = self.inner.area;
        let input = BrowserInput::decode(json, 128 * 1024)
            .map_err(|_| "invalid browser input")?
            .normalize(64 * 1024, |x, y| map_point(area, x, y))
            .map_err(|_| "invalid browser input")?;
        let mut submit = false;
        let mut transcript = false;
        for route in self.local.route_input(input) {
            self.inner.bindings.effective.begin_event();
            if let Event::Focus(focus) = route.event {
                self.inner.focused = focus == bmux_tui::event::FocusEvent::Gained;
                if !self.inner.focused {
                    self.inner.bindings.effective.cancel_chord();
                    self.cancel_pointer();
                }
                // Focus belongs to the surface even while a modal owns input.
                continue;
            }
            if !self.inner.focused {
                continue;
            }
            if let Some(focus) = &route.focus_changed {
                self.inner.focused_hit = Some(focus.as_str().to_owned());
            }
            if self.inner.route_question(&route) || route.traversal_consumed {
                continue;
            }
            if self.inner.permission.is_some() {
                self.inner.route_permission(&route);
                continue;
            }
            if route.is_activation()
                && let Some(target) = route
                    .target
                    .as_ref()
                    .map(|target| target.as_str().to_owned())
                && (target == LATEST || target.starts_with(ITEM_HIT_PREFIX))
            {
                self.inner.drag = None;
                transcript |= self.inner.activate(&target);
                continue;
            }
            match route.event {
                Event::Key(key) => {
                    transcript |= self.inner.route_key(key, &route, &mut submit);
                }
                Event::Paste(text) => {
                    TextInputControl::new(&session_ui::composer_policy())
                        .handle_paste(&mut self.inner.composer, &text);
                }
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    let rows = if mouse.kind == MouseEventKind::ScrollUp {
                        -WHEEL_ROWS
                    } else {
                        WHEEL_ROWS
                    };
                    transcript |= self.inner.navigate(NavIntent::ScrollBy {
                        rows,
                        source: InputSource::Wheel,
                    });
                }
                Event::Mouse(mouse) if route.targets(&bmux_tui::hit::HitId::new(EDITOR)) => {
                    TextInputControl::new(&session_ui::composer_policy())
                        .handle_mouse(&mut self.inner.composer, mouse);
                }
                Event::Mouse(mouse) => {
                    // Selection is paint-only: it never revokes committed activation.
                    let scene = self.local.presentation().selection();
                    let _ = self.inner.pointer(scene, mouse);
                }
                _ => {}
            }
        }
        if transcript {
            self.prepare();
        }
        Ok(submit)
    }

    /// Drain bounded application intents; no platform authority is exercised here.
    /// # Errors
    /// Returns an error if serialization fails.
    pub fn take_intents(&mut self) -> Result<String, &'static str> {
        serde_json::to_string(&std::mem::take(&mut self.inner.intents))
            .map_err(|_| "invalid surface intents")
    }

    /// Accessibility derives only from successfully committed visible BMUX metadata.
    /// # Errors
    /// Returns an error if committed metadata cannot be serialized.
    pub fn accessibility(&self) -> Result<String, &'static str> {
        let hidden = self
            .inner
            .bindings
            .effective
            .policy
            .as_ref()
            .is_some_and(|policy| policy.styles.values().any(|style| style.hidden));
        let regions = self.local.presentation().semantics().regions();
        let regions = regions
            .iter()
            .filter(|region| !region.area.is_empty())
            .take(256)
            .map(|region| {
                let metadata = region.accessibility.as_ref();
                let sensitive = hidden || metadata.is_some_and(|value| value.sensitive);
                let hit = self.local.presentation().hits().regions().iter()
                    .find(|hit| hit.id.as_str() == region.id && hit.visible);
                let label = match region.id.as_str() {
                    LATEST => Some("New messages below; return to latest"),
                    id if id.starts_with(ITEM_HIT_PREFIX) => id
                        .strip_prefix(ITEM_HIT_PREFIX)
                        .and_then(|index| index.parse::<usize>().ok())
                        .and_then(|index| self.inner.transcript.committed_frame()?.hit_regions.get(index))
                        .map(|region| match region.hit.action {
                            ItemAction::ToggleDisclosure(_) => "Expand or collapse",
                            ItemAction::Activate(_) => "Activate transcript content",
                            ItemAction::Visual { .. } => "Producer action",
                        }),
                    EDITOR => Some("Session composer. Ordinary native input queues a follow-up. For exact native steering type /native steer followed by the expected native turn and message. Unresolved submissions retain their exact target and attempt for retry"),
                    id if id.starts_with("bcode-web/permission/") => self.inner.committed_permission
                        .as_ref()
                        .and_then(|permission| {
                            let index = id.strip_prefix("bcode-web/permission/")?.parse().ok()?;
                            let mut dialog = PermissionDialogState::new(permission.clone());
                            dialog.focus_action(index).then(|| dialog.focused_label())
                        }),
                    _ => None,
                };
                serde_json::json!({
                    "id": region.id,
                    "role": region.role,
                    "area": {"x": region.area.x, "y": region.area.y,
                        "width": region.area.width, "height": region.area.height},
                    "label": (!sensitive).then(|| metadata.and_then(|value| value.label.as_deref()).or(label)).flatten(),
                    "description": metadata.filter(|_| !sensitive).and_then(|value| value.description.as_deref()),
                    "text": metadata.filter(|_| !sensitive).and_then(|value| value.text.as_deref()),
                    "disabled": metadata.is_some_and(|value| value.disabled) || hit.is_none_or(|hit| !hit.enabled),
                    "focused": metadata.is_some_and(|value| value.focused),
                    "selected": metadata.and_then(|value| value.selected),
                })
            })
            .collect::<Vec<_>>();
        serde_json::to_string(&serde_json::json!({
            "label": "Bcode session", "description": if hidden { "" } else { &self.inner.committed_lifecycle_summary }, "regions": regions,
        }))
        .map_err(|_| "invalid accessibility metadata")
    }

    /// Prepare the transcript outside the paint callback: local rows, the inline interaction
    /// reservation, plugin visuals and the render configuration. Revokes transcript
    /// activation until the next committed frame.
    pub(crate) fn prepare(&mut self) {
        let inner = &mut self.inner;
        let at = Instant::now();
        inner.presentation_generation = inner.presentation_generation.saturating_add(1);
        inner.committed_generation = None;
        inner.question.prepare(&inner.bindings.effective);
        let transcript = &mut inner.transcript;
        transcript.set_pending_submission(inner.semantic.pending_submission());
        let _ = transcript.sync_local_rows(at);
        let inline = inner
            .bindings
            .effective
            .policy
            .as_ref()
            .is_none_or(|policy| {
                matches!(
                    policy.interaction.placement,
                    crate::policy::InteractionPlacement::Transcript
                )
            });
        let reservation = if inline && !inner.area.is_empty() {
            let rows = inner.question.height(inner.area.width);
            inner
                .question
                .interaction_id()
                .filter(|_| rows > 0)
                .map(|id| InlineInteraction::new(id.to_owned(), u32::from(rows)))
        } else {
            None
        };
        let update = transcript
            .engine_mut()
            .set_inline_interaction(reservation, at);
        transcript.absorb(update);
        inner.question.inline_item = transcript
            .engine()
            .structure()
            .inline_interaction_item()
            .cloned();
        let _ = inner.sync_region(at);
        let policy = inner.bindings.effective.policy.as_ref();
        let theme = policy.map(crate::policy::BrowserPolicy::component_theme);
        let style = policy.map_or(0, |policy| {
            bcode_tui_transcript::layout::context::stable_hash(&policy.revision)
        });
        let _ = inner.transcript.sync_render(theme.as_ref(), style, at);
    }

    fn visual_candidates(&self) -> Result<Vec<VisualCandidate>, &'static str> {
        let transcript = &self.inner.transcript;
        let engine = transcript.engine();
        let mut candidates = Vec::new();
        let mut ordinal = 0usize;
        for item in &engine.session_view().snapshot().transcript.items {
            if candidates.len() >= MAX_VISUAL_CANDIDATES {
                break;
            }
            if !transcript.visuals.has_visual(engine, item) {
                continue;
            }
            let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } =
                &item.kind
            else {
                continue;
            };
            let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) =
                &tool.result
            else {
                continue;
            };
            for reference in &artifact.artifact.refs {
                if candidates.len() >= MAX_VISUAL_CANDIDATES {
                    break;
                }
                if reference.byte_len == Some(0) {
                    continue;
                }
                ordinal = ordinal.saturating_add(1);
                if ordinal <= self.inner.visual_cursor {
                    continue;
                }
                let key = serde_json::to_string(&(
                    item.id.get(),
                    item.revision,
                    &reference.key,
                    self.inner
                        .bindings
                        .effective
                        .policy
                        .as_ref()
                        .map(|policy| &policy.revision),
                ))
                .map_err(|_| "invalid visual identity")?;
                let Some(budget) = transcript.visuals.artifact_preview_limit(
                    engine,
                    item,
                    &reference.key,
                    reference.content_type.as_deref(),
                ) else {
                    continue;
                };
                candidates.push((
                    key,
                    item.id.get().to_owned(),
                    reference.key.clone(),
                    Some(reference.byte_len.unwrap_or(budget).min(budget)),
                ));
            }
        }
        Ok(candidates)
    }

    /// Claim the next bounded producer artifact page. One lease is outstanding;
    /// failed/expired work retries at most twice with monotonic backoff.
    ///
    /// # Errors
    ///
    /// Rejects unavailable observations or artifact request serialization failures.
    pub fn next_visual_artifact(&mut self, now: u64) -> Result<Option<String>, &'static str> {
        if self
            .inner
            .visual_lease
            .as_ref()
            .is_some_and(|lease| now < lease.expires)
        {
            return Ok(None);
        }
        if let Some(expired) = self.inner.visual_lease.take() {
            self.inner.pending_visual_artifact = None;
            if let Some(state) = self.inner.visual_fetches.get_mut(&expired.key) {
                state.retry_at = now.saturating_add(1000);
            }
        }
        let discovery = (
            self.inner.semantic.revision(),
            self.inner.content_generation,
        );
        if Some(discovery) != self.inner.visual_discovery {
            self.inner.visual_cursor = 0;
            self.inner.visual_discovery = Some(discovery);
        }
        if !self.inner.visual_fetches.is_empty()
            && self
                .inner
                .visual_fetches
                .values()
                .all(|state| state.done || state.attempts >= 2)
        {
            self.inner.visual_cursor = self
                .inner
                .visual_cursor
                .saturating_add(self.inner.visual_fetches.len());
            self.inner.visual_fetches.clear();
        }
        let candidates = self.visual_candidates()?;
        self.inner
            .visual_fetches
            .retain(|key, _| candidates.iter().any(|candidate| &candidate.0 == key));
        for (key, item, reference, total) in candidates {
            let state = self.inner.visual_fetches.entry(key.clone()).or_default();
            if state.done || state.attempts >= 2 || now < state.retry_at {
                continue;
            }
            let offset = state.offset;
            state.budget = total.unwrap_or(64 * 1024).min(64 * 1024);
            let remaining = state.budget.saturating_sub(offset);
            if remaining == 0 || state.pages >= 16 {
                state.done = true;
                continue;
            }
            state.attempts = state.attempts.saturating_add(1);
            let length = u32::try_from(remaining.min(4096)).map_err(|_| "invalid visual range")?;
            let request = self.prepare_visual_artifact(&item, &reference, offset, length)?;
            self.inner.next_visual_claim = self.inner.next_visual_claim.saturating_add(1);
            let claim = self.inner.next_visual_claim.to_string();
            if let Some(pending) = &mut self.inner.pending_visual_artifact {
                pending.claim = Some(claim.clone());
            }
            self.inner.visual_lease = Some(VisualFetchLease {
                claim: claim.clone(),
                key,
                expires: now.saturating_add(30_000),
            });
            return serde_json::to_string(
                &serde_json::json!({"claim":claim,"request_json":request}),
            )
            .map(Some)
            .map_err(|_| "invalid visual claim");
        }
        Ok(None)
    }

    /// Release an exact failed claim without replaying bytes or spinning retries.
    pub fn fail_visual_artifact(&mut self, claim: &str, now: u64) {
        if self
            .inner
            .visual_lease
            .as_ref()
            .is_none_or(|lease| lease.claim != claim)
        {
            return;
        }
        if let Some(lease) = self.inner.visual_lease.take()
            && let Some(state) = self.inner.visual_fetches.get_mut(&lease.key)
        {
            state.retry_at = now.saturating_add(1000);
        }
        self.inner.pending_visual_artifact = None;
    }

    /// Settle only a live exact claim; returns whether accepted producer state needs redraw.
    ///
    /// # Errors
    /// Rejects expired/foreign claims and invalid canonical artifact responses.
    pub fn accept_visual_artifact_claim(
        &mut self,
        claim: &str,
        json: &str,
        now: u64,
    ) -> Result<bool, &'static str> {
        if !self
            .inner
            .visual_lease
            .as_ref()
            .is_some_and(|lease| lease.claim == claim && now <= lease.expires)
            || self
                .inner
                .pending_visual_artifact
                .as_ref()
                .is_none_or(|pending| pending.claim.as_deref() != Some(claim))
        {
            return Err("stale visual claim");
        }
        let result = self.accept_visual_artifact(json);
        let lease = self
            .inner
            .visual_lease
            .take()
            .ok_or("missing visual lease")?;
        if let Some(state) = self.inner.visual_fetches.get_mut(&lease.key) {
            match &result {
                Ok(true) => {
                    let range: VisualArtifactRange =
                        serde_json::from_str(json).map_err(|_| "invalid visual artifact")?;
                    let end = range.offset.saturating_add(range.bytes.len() as u64);
                    state.pages = state.pages.saturating_add(1);
                    state.done = range.bytes.is_empty()
                        || end >= range.total_bytes
                        || end >= state.budget
                        || state.pages >= 16;
                    state.offset = end;
                    state.attempts = 0;
                }
                Ok(false) => state.done = true,
                Err(_) => state.retry_at = now.saturating_add(1000),
            }
        }
        result
    }

    /// Prepare bounded bytes for a producer-owned visual, using the existing
    /// canonical artifact port rather than granting filesystem/service access.
    ///
    /// # Errors
    /// Rejects missing/stale references and oversized ranges.
    pub fn prepare_visual_artifact(
        &mut self,
        item_id: &str,
        reference_key: &str,
        offset: u64,
        length: u32,
    ) -> Result<String, &'static str> {
        if length > 64 * 1024 {
            return Err("visual artifact range too large");
        }
        let transcript = &self.inner.transcript;
        let item = transcript
            .item(&bcode_session_view_models::TranscriptViewItemId::new(
                item_id,
            ))
            .ok_or("missing visual item")?;
        let request = self
            .inner
            .semantic
            .prepare_artifact(item, reference_key, offset, length)?;
        self.inner.pending_visual_artifact = Some(PendingVisualArtifact {
            item_id: item_id.to_owned(),
            item_revision: item.revision,
            reference_key: reference_key.to_owned(),
            offset,
            length,
            observation: self
                .inner
                .semantic
                .revision()
                .ok_or("missing observation")?,
            width: crate::visuals::body_width(transcript.engine(), item),
            claim: None,
        });
        Ok(request)
    }

    /// Deliver only the exact pending canonical range to its selected producer.
    ///
    /// # Errors
    /// Rejects stale observations, foreign references and malformed ranges.
    pub fn accept_visual_artifact(&mut self, json: &str) -> Result<bool, &'static str> {
        let pending = self
            .inner
            .pending_visual_artifact
            .take()
            .ok_or("missing visual artifact request")?;
        let transcript = &self.inner.transcript;
        let item = transcript
            .item(&bcode_session_view_models::TranscriptViewItemId::new(
                &pending.item_id,
            ))
            .ok_or("stale visual artifact")?;
        if json.len() > 512 * 1024
            || self.inner.semantic.revision() != Some(pending.observation)
            || item.revision != pending.item_revision
            || crate::visuals::body_width(transcript.engine(), item) != pending.width
        {
            return Err("stale visual artifact");
        }
        let range: VisualArtifactRange =
            serde_json::from_str(json).map_err(|_| "invalid visual artifact")?;
        if range.offset != pending.offset
            || range.reference_key != pending.reference_key
            || range.bytes.len() > pending.length as usize
            || range
                .offset
                .checked_add(range.bytes.len() as u64)
                .is_none_or(|end| end > range.total_bytes)
        {
            return Err("invalid visual artifact range");
        }
        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } = &item.kind
        else {
            return Err("unsupported visual artifact");
        };
        let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) = &tool.result
        else {
            return Err("unsupported visual artifact");
        };
        let reference = artifact
            .artifact
            .refs
            .iter()
            .find(|reference| reference.key == pending.reference_key)
            .ok_or("stale visual artifact")?;
        if artifact.artifact.artifact_id != range.artifact_id
            || reference.content_type != range.content_type
            || reference
                .byte_len
                .is_some_and(|len| len != range.total_bytes)
        {
            return Err("foreign visual artifact");
        }
        // Pin the immutable reference version before producer delivery; even a
        // producer that declines out-of-preview pages cannot bypass this fence.
        if let Some(lease) = &self.inner.visual_lease {
            let state = self
                .inner
                .visual_fetches
                .get_mut(&lease.key)
                .ok_or("missing visual state")?;
            let version = (range.total_bytes, range.reference_revision, range.finalized);
            if state.version.is_some_and(|pinned| pinned != version) {
                state.done = true;
                return Err("visual artifact version changed");
            }
            state.version = Some(version);
        }
        let accepted = transcript.visuals.deliver_artifact(
            transcript.engine(),
            item,
            &bcode_tui_components::plugin_visual::portable::PortableArtifactChunk {
                invocation_id: &tool.tool_call_id,
                artifact_id: &range.artifact_id,
                reference_key: &range.reference_key,
                producer_plugin_id: &artifact.artifact.producer_plugin_id,
                schema: &artifact.artifact.schema,
                schema_version: artifact.artifact.schema_version,
                content_type: range.content_type.as_deref(),
                offset: range.offset,
                total_bytes: range.total_bytes,
                revision: range.reference_revision,
                finalized: range.finalized,
                bytes: &range.bytes,
            },
        );
        if accepted {
            self.prepare();
        }
        Ok(accepted)
    }

    /// Prepare a bounded canonical artifact range of a resident transcript item.
    ///
    /// # Errors
    /// Rejects missing or non-artifact items, foreign references and invalid bounds.
    pub fn prepare_artifact(
        &self,
        item_id: &str,
        reference_key: &str,
        offset: u64,
        length: u32,
    ) -> Result<String, &'static str> {
        let item = self
            .inner
            .transcript
            .item(&bcode_session_view_models::TranscriptViewItemId::new(
                item_id,
            ))
            .ok_or("stale artifact")?;
        self.inner
            .semantic
            .prepare_artifact(item, reference_key, offset, length)
    }

    /// Capture current semantic identity before loading one bounded artifact image.
    #[must_use]
    pub fn prepare_image(&mut self, item_id: &str, reference: &str) -> bool {
        let transcript = &self.inner.transcript;
        self.inner.pending_image = crate::image::prepare_image(
            transcript.engine(),
            &transcript.visuals,
            item_id,
            reference,
        );
        self.inner.pending_image.is_some()
    }

    /// Accept decoded RGBA pixels only while every captured input still matches.
    #[must_use]
    pub fn accept_image(&mut self, bytes: Vec<u8>, width: u32, height: u32) -> bool {
        let Some(pending) = self.inner.pending_image.take() else {
            return false;
        };
        let transcript = &mut self.inner.transcript;
        let accepted = {
            let (engine, visuals) = transcript.engine_and_visuals();
            crate::image::accept_image(engine, visuals, &pending, bytes, width, height)
        };
        if accepted {
            self.prepare();
        }
        accepted
    }

    /// Copy the logical source of the current transcript selection.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        self.inner.transcript.engine().selected_text()
    }

    /// Configure explicit application command overrides (bounded JSON key map).
    ///
    /// # Errors
    /// Rejects malformed or oversized keymap configuration.
    pub fn configure_bindings(&mut self, json: &str) -> Result<(), &'static str> {
        self.inner.bindings.configure(json)
    }

    /// Take a requested canonical clipboard payload. JS owns platform access.
    pub fn take_clipboard(&mut self) -> Option<String> {
        if !std::mem::take(&mut self.inner.copy_requested) {
            return None;
        }
        self.selected_text()
    }

    /// Current real BMUX draft source.
    #[must_use]
    pub fn draft(&self) -> &str {
        self.inner.composer.buffer().text()
    }

    /// Export bounded caller-owned editor state and exact unresolved submission.
    /// Only text/cursor/selection survive; geometry and semantic observations do not.
    ///
    /// # Errors
    /// Rejects oversized drafts or serialization failures.
    pub fn export_recovery(&self) -> Result<String, &'static str> {
        if self.draft().len() > 64 * 1024 {
            return Err("draft exceeds recovery limit");
        }
        serde_json::to_string(&serde_json::json!({
            "schema_version": 1,
            "editor": self.inner.composer.snapshot()["buffer"],
            "submission": self.inner.semantic.export_recovery()?,
        }))
        .map_err(|_| "invalid surface recovery")
    }

    /// Restore ephemeral recovery into an unused surface, atomically.
    ///
    /// # Errors
    /// Rejects malformed/oversized state or replacing initialized application state.
    pub fn restore_recovery(&mut self, json: &str) -> Result<(), &'static str> {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Recovery {
            schema_version: u32,
            editor: serde_json::Value,
            submission: String,
        }
        if json.len() > 256 * 1024 || !self.draft().is_empty() {
            return Err("invalid surface recovery");
        }
        let recovery: Recovery =
            serde_json::from_str(json).map_err(|_| "invalid surface recovery")?;
        if recovery.schema_version != 1
            || recovery
                .editor
                .get("text")
                .and_then(serde_json::Value::as_str)
                .is_none_or(|text| text.len() > 64 * 1024)
        {
            return Err("invalid editor recovery");
        }
        let mut editor = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
        let mut snapshot = editor.snapshot();
        snapshot["buffer"] = recovery.editor;
        editor
            .restore_snapshot(&snapshot)
            .map_err(|_| "invalid editor recovery")?;
        self.inner.semantic.restore_recovery(&recovery.submission)?;
        self.inner.composer = editor;
        self.cancel_pointer();
        Ok(())
    }

    /// Retain the exact submission of the current draft under `identity` (generated once by
    /// the platform). While a submission is unresolved, returns its exact original payload.
    ///
    /// # Errors
    /// See [`SnapshotState::prepare_submission`].
    pub fn prepare_submission(&mut self, identity: String) -> Result<String, &'static str> {
        let draft = self.draft().to_owned();
        let body = self.inner.semantic.prepare_submission(identity, draft)?;
        self.prepare();
        Ok(body)
    }

    /// Validate a durable receipt and clear only its unchanged draft. The accepted message
    /// is revealed once the transcript incorporates it (T2).
    ///
    /// # Errors
    /// See [`SnapshotState::settle_submission`].
    pub fn settle_submission(&mut self, json: &str) -> Result<(), &'static str> {
        let settled = self.inner.semantic.settle_submission(json)?;
        if self.draft() == settled.draft {
            self.inner.composer = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
        }
        let _ = self.inner.transcript.submission_settled(
            &settled.identity,
            &settled.text,
            settled.sequence,
            Instant::now(),
        );
        self.transcript_changed(true);
        Ok(())
    }

    pub(crate) fn commit_output(&mut self) {
        self.inner.committed_generation = Some(self.inner.presentation_generation);
        self.inner.committed_revision = self.inner.semantic.revision();
        self.inner.committed_regions = self.inner.regions;
        self.inner.transcript.commit();
        if self.inner.committed_turn
            != self
                .inner
                .semantic
                .snapshot()
                .and_then(|snapshot| snapshot.runtime.active_turn_id.clone())
        {
            self.inner.cancel_pending = None;
        }
        self.inner.committed_turn = self
            .inner
            .semantic
            .snapshot()
            .and_then(|snapshot| snapshot.runtime.active_turn_id.clone());
        let retain_hidden = self.inner.question.inline_item.is_some()
            && self
                .inner
                .bindings
                .effective
                .policy
                .as_ref()
                .is_none_or(|policy| {
                    matches!(
                        policy.interaction.offscreen_focus,
                        crate::policy::OffscreenFocus::Retain
                    )
                });
        self.inner.question.commit(
            (self.inner.question.visible || retain_hidden)
                && !self.inner.area.is_empty()
                && self.inner.permission.is_none(),
        );
        self.inner.committed_lifecycle_summary = self.inner.lifecycle_summary();
        self.inner.committed_permission = if self
            .local
            .presentation()
            .hits()
            .regions()
            .iter()
            .any(|region| region.id.as_str().starts_with("bcode-web/permission/"))
        {
            self.inner
                .permission
                .as_ref()
                .map(|dialog| dialog.permission().clone())
        } else {
            None
        };
    }

    pub(crate) fn stage_question(&mut self) -> crate::question::QuestionSurface {
        let candidate = self.inner.question.fork();
        // Keep the original untouched while the synchronous/async output path
        // exclusively borrows the surface and paints its speculative fork.
        std::mem::replace(&mut self.inner.question, candidate)
    }

    pub(crate) fn restore_question(&mut self, checkpoint: crate::question::QuestionSurface) {
        self.inner.question = checkpoint;
    }

    /// Prepare and render to a caller-owned local presentation sink (no network frame
    /// decode), as the browser's Canvas presentation does.
    ///
    /// # Errors
    /// Returns bounded presentation or output errors.
    pub fn present<E>(
        &mut self,
        output: impl FnOnce(&bmux_tui::presentation::PreparedPresentation) -> Result<(), E>,
    ) -> Result<bool, bmux_tui::presentation::PresentationError<E>> {
        let checkpoint = self.stage_question();
        self.prepare();
        let inner = &mut self.inner;
        let result = self
            .local
            .present(Damage::Full, |cx| inner.paint(cx), output);
        if matches!(result, Ok(true)) {
            self.commit_output();
        } else {
            self.restore_question(checkpoint);
        }
        result
    }
}

fn checked_area(width: u16, height: u16) -> Result<Rect, &'static str> {
    if usize::from(width) * usize::from(height) > 131_072 {
        return Err("surface too large");
    }
    Ok(Rect::new(0, 0, width, height))
}

#[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
fn map_point(area: Rect, x: f64, y: f64) -> Option<Point> {
    (x.is_finite()
        && y.is_finite()
        && x >= 0.0
        && y >= 0.0
        && x < f64::from(area.width)
        && y < f64::from(area.height))
    .then(|| Point::new(x.floor() as u16, y.floor() as u16))
}

/// Whether `scope` is the transcript's selection scope or one of its unit scopes.
fn is_transcript_scope(scope: &SelectionScopeId) -> bool {
    scope
        .as_str()
        .strip_prefix(SELECTION_SCOPE)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('/'))
}

/// `rect` (viewport cells) in terminal cells of `viewport`.
fn terminal_rect(viewport: Rect, x: u64, y: u64, width: u64, height: u64) -> Rect {
    let cells = |value: u64| u16::try_from(value).unwrap_or(u16::MAX);
    Rect::new(
        viewport.x.saturating_add(cells(x)),
        viewport.y.saturating_add(cells(y)),
        cells(width),
        cells(height),
    )
}

impl SurfaceState {
    fn navigate(&mut self, intent: NavIntent) -> bool {
        let update = self.transcript.engine_mut().intent(intent, Instant::now());
        self.transcript.absorb(update)
    }

    /// Activate a committed transcript region: the latest bar jumps to the latest content; an
    /// item region of the committed frame activates through the engine (X1, E2). Every
    /// preparation since the last commit revokes activation.
    fn activate(&mut self, target: &str) -> bool {
        if self.committed_generation != Some(self.presentation_generation) {
            return false;
        }
        if target == LATEST {
            return self.navigate(NavIntent::JumpToLatest);
        }
        let Some(index) = target
            .strip_prefix(ITEM_HIT_PREFIX)
            .and_then(|index| index.parse::<usize>().ok())
        else {
            return false;
        };
        // The region belongs to the frame the engine committed (CK3, CK4).
        let Some(frame) = self.transcript.committed_frame() else {
            return false;
        };
        if self.transcript.engine().committed_frame() != Some(frame.id) {
            return false;
        }
        let Some(hit) = frame
            .hit_regions
            .get(index)
            .map(|region| region.hit.clone())
        else {
            return false;
        };
        let at = Instant::now();
        match &hit.action {
            ItemAction::ToggleDisclosure(target) => {
                let (update, _) = self.transcript.engine_mut().toggle_disclosure(target, at);
                self.transcript.absorb(update)
            }
            ItemAction::Activate(key) => self.activate_markdown(&hit, key, at),
            ItemAction::Visual { .. } => {
                // Resolved against the visual accepted now: a replaced adapter or revision
                // resolves nothing, and nothing falls back to a retained payload.
                if let Some(action) = self.transcript.engine().visual_action(&hit).cloned() {
                    self.visual_action(action);
                }
                false
            }
        }
    }

    fn activate_markdown(&mut self, hit: &ItemHit, key: &str, at: Instant) -> bool {
        let Some(target) = self
            .transcript
            .engine()
            .markdown_target_for_action(&hit.unit, key)
        else {
            return false;
        };
        let engine = self.transcript.engine_mut();
        let mut changed = engine.focus_markdown(&target);
        let (update, activation) = engine.activate_markdown(&target, at);
        changed |= self.transcript.absorb(update);
        match activation {
            MarkdownActivation::Details { key, open } => {
                self.transcript.set_details(key, open);
                changed = true;
            }
            MarkdownActivation::Link(bcode_markdown_render::MarkdownDestination::Web(url)) => {
                self.push_intent(serde_json::json!({"type":"navigate", "url":url.as_str()}));
            }
            MarkdownActivation::Link(bcode_markdown_render::MarkdownDestination::Fragment(
                fragment,
            )) => {
                let (update, _) = self.transcript.engine_mut().navigate_markdown_fragment(
                    Some(&hit.unit),
                    &fragment,
                    at,
                );
                changed |= self.transcript.absorb(update);
            }
            MarkdownActivation::Link(_) => self.unavailable_destination(),
            MarkdownActivation::Mermaid { .. }
            | MarkdownActivation::FootnoteDefinition { .. }
            | MarkdownActivation::FootnoteReference { .. }
            | MarkdownActivation::Unavailable => {}
        }
        changed
    }

    fn unavailable_destination(&mut self) {
        // Local paths never acquire browser host access.
        self.push_intent(serde_json::json!({"type":"unavailable", "message":"This destination is not available through the browser's narrow navigation port; no host resource was opened."}));
    }

    fn visual_action(&mut self, action: VisualAction) {
        match action {
            VisualAction::Open(bcode_markdown_render::MarkdownDestination::Web(url)) => {
                self.push_intent(serde_json::json!({"type":"navigate", "url":url.as_str()}));
            }
            VisualAction::Copy(text) => {
                self.push_intent(serde_json::json!({"type":"clipboard", "text":text}));
            }
            VisualAction::Artifact { artifact_id } => self.artifact_intent(&artifact_id),
            // Producer translation does not grant arbitrary controller or
            // service authority to this browser boundary.
            VisualAction::Plugin { .. } => self.push_intent(serde_json::json!({"type":"unavailable", "message":"This producer action has no authorized browser capability; no action was sent."})),
            VisualAction::Open(_) => self.unavailable_destination(),
        }
    }

    fn artifact_intent(&mut self, artifact_id: &str) {
        let reference =
            self.transcript
                .engine()
                .session_view()
                .snapshot()
                .transcript
                .items
                .iter()
                .find_map(|item| {
                    let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool } =
                        &item.kind
                    else {
                        return None;
                    };
                    let Some(bcode_session_view_models::ToolResultView::Artifact { artifact }) =
                        &tool.result
                    else {
                        return None;
                    };
                    if artifact.artifact.artifact_id != artifact_id {
                        return None;
                    }
                    let reference = artifact.artifact.refs.first()?;
                    let image = reference.content_type.as_deref().is_some_and(|mime| {
                        matches!(mime, "image/png" | "image/jpeg" | "image/webp")
                    });
                    Some((item.id.get().to_owned(), reference.key.clone(), image))
                });
        if let Some((item_id, reference_key, image)) = reference {
            self.push_intent(serde_json::json!({"type":if image {"image"} else {"artifact"}, "item_id":item_id, "reference_key":reference_key}));
        }
    }

    fn permission_intent(&mut self, resolution: PermissionDialogResolution) {
        let Some(dialog) = &self.permission else {
            return;
        };
        let permission = dialog.permission();
        if permission.batch.is_some() && !resolution.apply_to_batch {
            return;
        }
        let action = if resolution.apply_to_batch {
            let Some(batch) = &permission.batch else {
                return;
            };
            serde_json::json!({"type":"permission_batch","batch_id":batch.batch_id,"approved":resolution.approved})
        } else {
            serde_json::json!({"type":"permission","permission_id":permission.permission_id,"approved":resolution.approved,"remember":resolution.remember})
        };
        self.push_intent(serde_json::json!({"type":"control","action_json":action.to_string()}));
        self.permission_inflight = true;
    }

    fn push_intent(&mut self, intent: serde_json::Value) {
        if self.intents.len() < 16 && !self.intents.contains(&intent) {
            self.intents.push(intent);
        }
    }

    // ---- selection ------------------------------------------------------------------------

    fn clear_selection(&mut self) -> bool {
        self.drag = None;
        let update = self.transcript.engine_mut().set_selection(None, false);
        self.transcript.absorb(update)
    }

    /// One pointer event outside hit regions and the editor: left-button gestures select
    /// transcript text against the committed scene (C34). Returns whether the selection
    /// changed.
    fn pointer(&mut self, scene: &SelectionScene, mouse: MouseEvent) -> bool {
        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let mut changed = self.clear_selection();
                if let Some(drag) = self.selection_anchor(scene, mouse.position) {
                    let collapsed = TranscriptSelection::collapsed(drag.anchor.clone());
                    self.drag = Some(drag);
                    let update = self
                        .transcript
                        .engine_mut()
                        .set_selection(Some(collapsed), true);
                    changed |= self.transcript.absorb(update);
                }
                changed
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                self.extend_selection(scene, mouse.position, true)
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let changed = self.extend_selection(scene, mouse.position, false);
                self.drag = None;
                if self
                    .transcript
                    .engine()
                    .selection()
                    .is_some_and(TranscriptSelection::is_collapsed)
                {
                    let update = self.transcript.engine_mut().set_selection(None, false);
                    self.transcript.absorb(update);
                }
                changed
            }
            _ => false,
        }
    }

    fn selection_anchor(&self, scene: &SelectionScene, point: Point) -> Option<SelectionDrag> {
        let scope = scene.initiation_scope(point)?;
        if !is_transcript_scope(&scope.id) {
            return None;
        }
        let endpoint = scene.endpoint_at_granularity(
            &scope.id,
            point,
            SelectionAffinity::Before,
            SelectionGranularity::Grapheme,
        )?;
        let frame = self.transcript.committed_frame()?;
        let anchor = self
            .transcript
            .engine()
            .selection_endpoint(frame, &endpoint)?;
        Some(SelectionDrag {
            scope: scope.id.clone(),
            anchor,
        })
    }

    fn extend_selection(&mut self, scene: &SelectionScene, point: Point, gesture: bool) -> bool {
        let Some(drag) = &self.drag else {
            return false;
        };
        let Some(frame) = self.transcript.committed_frame() else {
            return false;
        };
        let engine = self.transcript.engine();
        let resolve = |affinity| {
            let endpoint = scene.endpoint_at_granularity(
                &drag.scope,
                point,
                affinity,
                SelectionGranularity::Grapheme,
            )?;
            engine.selection_endpoint(frame, &endpoint)
        };
        let focus = resolve(SelectionAffinity::After).and_then(|after| {
            if engine.compare_selection_endpoints(&after, &drag.anchor)
                == Some(std::cmp::Ordering::Less)
            {
                resolve(SelectionAffinity::Before)
            } else {
                Some(after)
            }
        });
        let focus = focus.unwrap_or_else(|| {
            engine.selection().map_or_else(
                || drag.anchor.clone(),
                |selection| selection.focus().clone(),
            )
        });
        let selection = TranscriptSelection::new(drag.anchor.clone(), focus);
        let update = self
            .transcript
            .engine_mut()
            .set_selection(Some(selection), gesture);
        self.transcript.absorb(update)
    }

    // ---- paint ----------------------------------------------------------------------------

    fn paint_permission(&self, cx: &mut PaintCx<'_, '_>) -> bool {
        if let Some(dialog) = &self.permission {
            let style = self
                .bindings
                .effective
                .policy
                .as_ref()
                .map_or(Style::new(), |policy| policy.style("text"));
            let theme = ModalTheme::new(style, style, style, style, style, style);
            let permission = dialog.permission();
            let mut content = Column::new().child(
                SizeBox::new(TextBlock::new(
                    permission
                        .title
                        .clone()
                        .unwrap_or_else(|| "Permission requested".to_owned()),
                ))
                .height(1),
            );
            for index in 0..dialog.action_count() {
                let mut candidate = dialog.clone();
                candidate.focus_action(index);
                // Scoped canonical CAS admits only complete batched decisions.
                // Keep the shared labels/resolution; suppress unsupported choices.
                if permission.batch.is_some() && !candidate.focused_batch() {
                    continue;
                }
                let mut state = self.permission_buttons[index].get();
                state.set_focused(dialog.focused_action_index() == index);
                state.set_disabled(self.permission_inflight);
                self.permission_buttons[index].set(state);
                content = content.child(ButtonComponent::new(
                    format!("bcode-web/permission/{index}"),
                    candidate.focused_label(),
                    &self.permission_buttons[index],
                ));
            }
            content = content.flex(Flex::new(
                1,
                TextBlock::new(format!(
                    "{}\n{}",
                    permission.tool_name, permission.arguments_json
                )),
            ));
            let modal =
                ModalFrameComponent::new("bcode-web/permission", permission_modal(theme), content);
            let layout = modal.layout(
                Constraints::tight(Size::new(self.area.width, self.area.height)),
                &mut LayoutCx::new(),
            );
            modal.paint(&layout, cx);
            return true;
        }
        false
    }

    fn presentation_styles(&self) -> [Style; 5] {
        let policy = self.bindings.effective.policy.as_ref();
        let text = policy.map_or(Style::new(), |policy| policy.style("text"));
        ["text", "surface", "border", "selection", "muted"]
            .map(|role| policy.map_or(text, |policy| policy.style(role)))
    }

    /// Paint the question surface inline over its reserved rows of the prepared frame, else
    /// pinned above the composer.
    fn paint_question(
        &mut self,
        layout: &bmux_tui::component::LayoutNode,
        question_height: u16,
        pinned_height: u16,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let inline = self.question.inline_item.is_some();
        let region = if inline {
            let viewport = self.regions.viewport;
            let placement = self
                .transcript
                .last_frame()
                .and_then(|frame| self.transcript.engine().inline_interaction_placement(frame));
            viewport.zip(placement).and_then(|(viewport, placement)| {
                let height = i64::try_from(placement.height).ok()?;
                let start = placement.top.max(0);
                let end = placement
                    .top
                    .saturating_add(height)
                    .min(i64::from(viewport.height));
                (start < end).then(|| {
                    (
                        u16::try_from(start - placement.top).unwrap_or(u16::MAX),
                        Rect::new(
                            viewport.x,
                            viewport
                                .y
                                .saturating_add(u16::try_from(start).unwrap_or(u16::MAX)),
                            viewport.width,
                            u16::try_from(end - start).unwrap_or(0).min(512),
                        ),
                    )
                })
            })
        } else {
            layout
                .find_logical_rect(&LayoutId::new(QUESTION_PINNED))
                .map(|rect| {
                    let focused = self.question.focused_rows(self.area.width);
                    let offset = focused
                        .end
                        .saturating_sub(pinned_height)
                        .min(question_height.saturating_sub(pinned_height));
                    (
                        offset,
                        Rect::new(
                            u16::try_from(rect.x).unwrap_or(0),
                            u16::try_from(rect.y).unwrap_or(0),
                            self.area.width,
                            pinned_height,
                        ),
                    )
                })
        };
        if let Some((offset, area)) = region.filter(|(_, area)| !area.is_empty()) {
            self.question.offset = offset;
            self.question.paint_slice(offset, area, cx);
            self.question.visible = true;
        }
    }

    fn question_extents(&mut self) -> (u16, u16) {
        let question_height = self.question.height(self.area.width);
        let pinned_height = if self.question.active() && self.question.inline_item.is_none() {
            question_height
                .min(self.area.height.saturating_sub(3))
                .min(512)
        } else {
            0
        };
        (question_height, pinned_height)
    }

    /// The status line above the transcript: the execution lifecycle, and the degraded
    /// transcript notice while a history read cannot load.
    fn lifecycle_summary(&self) -> String {
        let lifecycle = self
            .semantic
            .snapshot()
            .filter(|snapshot| snapshot.execution_lifecycle.session_id.is_some())
            .map(|snapshot| snapshot.execution_lifecycle.summary())
            .unwrap_or_default();
        if !self.transcript.history_unavailable() {
            return lifecycle;
        }
        if lifecycle.is_empty() {
            HISTORY_UNAVAILABLE.to_owned()
        } else {
            format!("{HISTORY_UNAVAILABLE} · {lifecycle}")
        }
    }

    /// Paint the prepared transcript frame into `region`: the engine's viewport (cells,
    /// selection fragments and highlight, item hit regions from the same snapshot) and the
    /// latest bar below it.
    fn paint_transcript(&mut self, region: Rect, cx: &mut PaintCx<'_, '_>) {
        self.regions = Regions::default();
        let Some(snapshot) = self.transcript.last_frame() else {
            return;
        };
        let height = u16::try_from(snapshot.geometry.height)
            .unwrap_or(u16::MAX)
            .min(region.height);
        let viewport = Rect::new(region.x, region.y, region.width, height);
        self.regions.viewport = Some(viewport);
        let selection = self.bindings.effective.policy.as_ref().map_or_else(
            || Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED),
            |policy| policy.style("selection"),
        );
        let canvas = self
            .bindings
            .effective
            .policy
            .as_ref()
            .map_or_else(Style::new, |policy| policy.style("surface"));
        let engine = self.transcript.engine();
        cx.with_child(
            i32::from(viewport.x),
            i64::from(viewport.y),
            LocalRect::new(0, 0, viewport.width, viewport.height),
            |child| {
                // Items sit on the canvas: a surface or fill that sets no background of its
                // own (a tool frame, a speaker bar) keeps the canvas instead of clearing it.
                child.with_style(canvas, |child| {
                    engine.paint_frame(snapshot, child);
                });
                engine.paint_selection(snapshot, child, selection);
                for (index, hit) in snapshot.hit_regions.iter().enumerate() {
                    let rect = terminal_rect(
                        Rect::default(),
                        hit.rect.x,
                        hit.rect.y,
                        hit.rect.width,
                        hit.rect.height,
                    );
                    if !rect.is_empty() {
                        let id = format!("{ITEM_HIT_PREFIX}{index}");
                        child.push_semantic(bmux_tui::semantic::SemanticRegion::new(
                            id.clone(),
                            rect,
                            "button",
                        ));
                        child.push_hit(HitRegion::new(id, rect).role(HitRole::Action).layer(1));
                    }
                }
            },
        );
        let bar_row = viewport.bottom();
        if snapshot.latest_bar.visible && bar_row < region.bottom() {
            let bar = Rect::new(region.x, bar_row, region.width, 1);
            self.regions.latest_bar = Some(bar);
            let button = ButtonComponent::new(
                LATEST,
                "New messages below · return to latest",
                &self.latest_button,
            );
            let layout = button.layout(
                Constraints::tight(Size::new(bar.width, 1)),
                &mut LayoutCx::new(),
            );
            cx.with_child(
                i32::from(bar.x),
                i64::from(bar.y),
                LocalRect::new(0, 0, bar.width, 1),
                |child| button.paint(&layout, child),
            );
        }
    }

    /// Lay out the session chrome at the current area and call `f` with it: the composed
    /// root, its layout and the transcript region. The composer state is lent to the layout
    /// for the duration of `f`.
    fn with_chrome<R>(
        &mut self,
        pinned_height: u16,
        f: impl FnOnce(&mut Self, &Column<'_>, &bmux_tui::component::LayoutNode, Rect) -> R,
    ) -> R {
        let editor = RefCell::new(std::mem::replace(
            &mut self.composer,
            TextInputState::new(bmux_text_edit::TextEditBuffer::new()),
        ));
        let policy = session_ui::composer_policy();
        let composer_height = TextInputControl::new(&policy)
            .visible_rows_for_width(&editor.borrow(), self.area.width.saturating_sub(2))
            .clamp(1, 6)
            .saturating_add(2)
            .min(self.area.height);
        let [text, surface, border, selection, muted] = self.presentation_styles();
        let lifecycle_summary = self.lifecycle_summary();
        let transcript_panel = Column::new()
            .child(
                SizeBox::new(TextBlock::new(lifecycle_summary.clone()))
                    .height(u64::from(!lifecycle_summary.is_empty())),
            )
            .flex(Flex::new(
                1,
                SizeBox::new(TextBlock::new("")).id(TRANSCRIPT_REGION),
            ))
            .child(
                SizeBox::new(TextBlock::new(""))
                    .id(QUESTION_PINNED)
                    .height(u64::from(pinned_height)),
            );
        let root = session_ui::compose(
            &IDS,
            transcript_panel,
            session_ui::composer(
                &IDS,
                &editor,
                &policy,
                SessionComposerStyle {
                    panel: ComposerStyle { border, surface },
                    text,
                    selection,
                    muted,
                },
                self.focused,
            ),
            composer_height,
        );
        let constraints = Constraints::tight(Size::new(self.area.width, self.area.height));
        let layout = root.layout(constraints, &mut LayoutCx::new());
        let region = layout
            .find_logical_rect(&LayoutId::new(TRANSCRIPT_REGION))
            .map(|rect| {
                Rect::new(
                    u16::try_from(rect.x).unwrap_or(0),
                    u16::try_from(rect.y).unwrap_or(0),
                    u16::try_from(rect.width).unwrap_or(0),
                    u16::try_from(rect.height).unwrap_or(0),
                )
            })
            .unwrap_or_default();
        let result = f(self, &root, &layout, region);
        drop(root);
        self.composer = editor.into_inner();
        result
    }

    /// Size the engine's transcript region from the chrome layout (a navigation input applied
    /// before frames are prepared, outside paint).
    fn sync_region(&mut self, at: Instant) -> bool {
        if self.area.is_empty() || self.permission.is_some() {
            return false;
        }
        let (_, pinned_height) = self.question_extents();
        let region = self.with_chrome(pinned_height, |_, _, _, region| region);
        self.transcript
            .sync_geometry(region.width, region.height, at)
    }

    pub(crate) fn paint(&mut self, cx: &mut PaintCx<'_, '_>) {
        self.question.prepare(&self.bindings.effective);
        self.question.invalidate_geometry();
        self.regions = Regions::default();
        if self.area.is_empty() || self.paint_permission(cx) {
            self.transcript.clear_frame();
            return;
        }
        let at = Instant::now();
        let (question_height, pinned_height) = self.question_extents();
        self.sync_latest_focus();
        self.with_chrome(pinned_height, |this, root, layout, region| {
            root.paint(layout, cx);
            // The region size is a navigation input applied before the frame is prepared;
            // `prepare` normally applied it already.
            let _ = this
                .transcript
                .sync_geometry(region.width, region.height, at);
            if region.is_empty() {
                this.transcript.clear_frame();
            } else {
                let clock = cx.clock().map_or(FrameClock { now: at }, FrameClock::from);
                let _ = this.transcript.prepare_frame(clock);
                this.paint_transcript(region, cx);
            }
            this.paint_question(layout, question_height, pinned_height, cx);
        });
    }

    fn sync_latest_focus(&self) {
        let mut value = self.latest_button.get();
        value.set_focused(self.focused && self.focused_hit.as_deref() == Some(LATEST));
        self.latest_button.set(value);
    }

    // ---- routed input ---------------------------------------------------------------------

    fn route_question(&mut self, route: &bmux_tui::interaction::InteractionRoute) -> bool {
        let interaction_routes = self.question.visible
            || self
                .bindings
                .effective
                .policy
                .as_ref()
                .is_none_or(|policy| {
                    matches!(
                        policy.interaction.offscreen_focus,
                        crate::policy::OffscreenFocus::Retain
                    )
                });
        let focus_active = matches!(route.event, Event::Key(key) if matches!(self.bindings.command(key), Some(crate::bindings::Command::FocusInteraction)));
        if self.permission.is_none()
            && self.question.active()
            && self.question.owns_pointer(&route.event)
            && interaction_routes
            && !focus_active
        {
            if let Some(intent) = self.question.input(&route.event, &self.bindings.effective) {
                self.push_intent(intent);
            }
            return true;
        }
        false
    }

    fn route_permission(&mut self, route: &bmux_tui::interaction::InteractionRoute) {
        if !self.permission_inflight
            && self
                .permission
                .as_ref()
                .map(PermissionDialogState::permission)
                == self.committed_permission.as_ref()
        {
            let resolution = if route.is_activation() {
                route
                    .target
                    .as_ref()
                    .and_then(|id| id.as_str().strip_prefix("bcode-web/permission/"))
                    .and_then(|index| index.parse::<usize>().ok())
                    .and_then(|index| self.permission.as_mut()?.activate_action(index))
            } else {
                None
            };
            if let Some(resolution) = resolution {
                self.permission_intent(resolution);
            } else if let Event::Key(key) = route.event {
                if let Some(index) = route
                    .focus_changed
                    .as_ref()
                    .and_then(|id| id.as_str().strip_prefix("bcode-web/permission/"))
                    .and_then(|index| index.parse::<usize>().ok())
                    && let Some(permission) = self.permission.as_mut()
                {
                    permission.focus_action(index);
                }
                let command = self.bindings.permission_action(key);
                let Some(permission) = self.permission.as_mut() else {
                    return;
                };
                if let PermissionDialogOutcome::Resolve(resolution) =
                    permission.handle_action(command)
                {
                    self.permission_intent(resolution);
                }
            }
        }
    }

    /// One key outside modal surfaces. Returns whether the transcript changed.
    fn route_key(
        &mut self,
        key: bmux_keyboard::KeyStroke,
        route: &bmux_tui::interaction::InteractionRoute,
        submit: &mut bool,
    ) -> bool {
        use crate::bindings::Command;
        match self.bindings.command(key) {
            Some(Command::Submit) => {
                *submit = true;
                false
            }
            Some(Command::Newline) => {
                TextInputControl::new(&session_ui::composer_policy())
                    .handle_paste(&mut self.composer, "\n");
                false
            }
            Some(Command::PageUp) => self.navigate(NavIntent::PageBy { pages: -1 }),
            Some(Command::PageDown) => self.navigate(NavIntent::PageBy { pages: 1 }),
            Some(Command::Latest) => self.navigate(NavIntent::JumpToLatest),
            Some(Command::CancelTurn) => {
                let turn = self
                    .semantic
                    .snapshot()
                    .and_then(|snapshot| snapshot.runtime.active_turn_id.clone());
                if self.cancel_pending.is_none()
                    && !self.area.is_empty()
                    && self.committed_revision == self.semantic.revision()
                    && turn.is_some()
                    && turn == self.committed_turn
                {
                    self.push_intent(serde_json::json!({"type":"control","action_json":serde_json::json!({"type":"cancel_turn","turn_id":turn}).to_string()}));
                    self.cancel_pending = turn;
                }
                false
            }
            Some(Command::FocusInteraction) => {
                let Some(item) = self.question.inline_item.clone() else {
                    return false;
                };
                self.navigate(NavIntent::JumpToItem {
                    item: UnitKey::Item(item),
                    placement: Placement::ItemTopAtTop,
                })
            }
            Some(Command::Copy) => {
                self.copy_requested = true;
                false
            }
            Some(
                Command::Ignore
                | Command::PermissionApprove
                | Command::PermissionDeny
                | Command::SelectUp
                | Command::SelectDown
                | Command::SelectConfirm
                | Command::SelectCancel,
            ) => false,
            None => {
                if !route.targets(&bmux_tui::hit::HitId::new(LATEST)) {
                    self.bindings.edit(&mut self.composer, key);
                }
                false
            }
        }
    }
}
