//! BMUX rendering/input using retained shared native Markdown components.
use bcode_tui_components::permission::{
    decision::{PermissionDialogOutcome, PermissionDialogResolution, PermissionDialogState},
    permission_modal,
};
use bmux_tui::composition::TextBlock;
use bmux_tui::composition::{Column, Flex, Row, SizeBox};
use bmux_tui_components::button::{ButtonComponent, ButtonState};
use bmux_tui_components::modal_frame::{ModalFrameComponent, ModalTheme};
use std::cell::{Cell, RefCell};

use crate::transcript::{Transcript, TranscriptComponent};
use bcode_tui_components::{
    composer::ComposerStyle,
    session_ui::{
        self, SessionComposerStyle, SessionUi, SessionUiAction, SessionUiEffect, SessionUiIds,
    },
    transcript_history::TranscriptHistory,
};
use bmux_tui::{
    component::{Component, Constraints, LayoutCx, LayoutId},
    damage::Damage,
    event::{Event, MouseEventKind},
    geometry::{Point, Rect, Size},
    paint::PaintCx,
    presentation::PresentationLimits,
    style::Style,
};
use bmux_tui_components::text_input::{TextInputControl, TextInputState};
use bmux_web_renderer::{LocalSurface, input::BrowserInput};

use crate::SnapshotState;

const OLDER: &str = "bcode-web/history-older";
const NEWER: &str = "bcode-web/history-newer";
const LATEST: &str = "bcode-web/latest";
const EDITOR: &str = "bcode-web/editor";
const IDS: SessionUiIds<'static> = SessionUiIds {
    root: "bcode-web/root",
    composer: "bcode-web/composer",
    editor: EDITOR,
    composer_size: "bcode-web/composer-size",
};

/// Concrete local surface. This is not a portable application contract.
pub struct SessionSurface {
    pub(crate) local: LocalSurface,
    pub(crate) inner: SurfaceState,
}

struct PendingVisualArtifact {
    item_id: String,
    reference_key: String,
    offset: u64,
    length: u32,
    observation: u64,
    preparation: bcode_tui_components::transcript::accepted::VisualPreparation,
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

pub struct SurfaceState {
    pub(crate) semantic: SnapshotState,
    ui: SessionUi,
    area: Rect,
    transcript: Transcript,
    selection: bmux_tui::selection::SelectionController,
    height: u16,
    anchor: Option<(String, usize)>,
    focused: bool,
    focused_hit: Option<String>,
    navigation_buttons: [Cell<ButtonState>; 3],
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
    visual_discovery_revision: Option<u64>,
    permission_inflight: bool,
    committed_permission: Option<bcode_session_view_models::PermissionView>,
    committed_turn: Option<String>,
    committed_revision: Option<u64>,
    presentation_generation: u64,
    committed_generation: Option<u64>,
    cancel_pending: Option<String>,
    permission_buttons: [Cell<ButtonState>; 6],
}

impl SessionSurface {
    /// Create a session-bound local surface, with bounded presentation allocations.
    ///
    /// # Errors
    /// Rejects invalid session identifiers and excessively large cell grids.
    pub fn new(session: String, width: u16, height: u16) -> Result<Self, &'static str> {
        let area = checked_area(width, height)?;
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
                ui: SessionUi::new(),
                area,
                transcript: Transcript::new(),
                selection: bmux_tui::selection::SelectionController::new(),
                height: 0,
                anchor: None,
                focused: true,
                focused_hit: None,
                navigation_buttons: std::array::from_fn(|_| Cell::new(ButtonState::new())),
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
                visual_discovery_revision: None,
                permission_inflight: false,
                committed_permission: None,
                committed_turn: None,
                committed_revision: None,
                presentation_generation: 0,
                committed_generation: None,
                cancel_pending: None,
                permission_buttons: std::array::from_fn(|_| Cell::new(ButtonState::new())),
            },
        })
    }

    /// Replace the accepted semantic observation without flattening plugin snapshots.
    ///
    /// # Errors
    /// See [`SnapshotState::accept`].
    pub fn accept_snapshot(&mut self, json: &str) -> Result<bool, &'static str> {
        if json.len() > crate::MAX_RESPONSE_BYTES {
            return Err("response too large");
        }
        let policy = serde_json::from_str::<serde_json::Value>(json)
            .ok()
            .and_then(|value| value.get("policy").cloned())
            .map(|value| crate::policy::BrowserPolicy::parse(&value.to_string()))
            .transpose()?;
        if policy.is_none() && self.inner.bindings.effective.policy.is_some() {
            return Err("effective presentation policy missing");
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
            self.inner
                .transcript
                .accepted
                .set_presentation(policy.transcript_presentation());
            self.inner
                .transcript
                .configure_visuals(&policy.visual_policy)?;
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

    /// Accept a converging bounded history window and preserve a stable source
    /// anchor where it overlaps. Disjoint backward/forward windows land at their
    /// adjacent edge; tail explicitly returns to following.
    ///
    /// # Errors
    /// Rejects invalid, stale or non-progressing history observations.
    pub fn accept_history(&mut self, json: &str, direction: &str) -> Result<bool, &'static str> {
        self.inner.semantic.validate_history(json, direction)?;
        let changed = self.accept_snapshot(json)?;
        if direction == "tail" {
            self.inner.anchor = None;
            self.inner
                .ui
                .update(SessionUiAction::FollowTail, &mut WindowHistory::default());
        } else {
            let top = self
                .inner
                .anchor
                .as_ref()
                .and_then(|anchor| self.inner.transcript.resolve_anchor(anchor))
                .unwrap_or_else(|| {
                    if direction == "older" {
                        self.inner
                            .transcript
                            .rows
                            .saturating_sub(usize::from(self.inner.height))
                    } else {
                        0
                    }
                });
            self.inner.anchor = self.inner.transcript.anchor(top);
            self.inner.ui.viewport.restore_anchor(top);
        }
        Ok(changed)
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
            self.inner.selection.clear();
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
        self.inner.selection.set_obscured(
            true,
            bmux_tui::selection::SelectionObscurationPolicy::PreserveCompleted,
        );
        self.inner.selection.set_obscured(
            false,
            bmux_tui::selection::SelectionObscurationPolicy::PreserveCompleted,
        );
        // Reset editor gesture state while retaining real text and source selection.
        let state = &mut self.inner.ui.composer;
        let area = state.content_area();
        let position = Point::new(area.x, area.y);
        TextInputControl::new(&session_ui::composer_policy()).handle_mouse(
            state,
            bmux_tui::event::MouseEvent::new(
                MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                position,
            ),
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
            if self.inner.route_question(&route) {
                continue;
            }
            if route.traversal_consumed {
                continue;
            }
            if self.inner.permission.is_some() {
                self.inner.route_permission(&route);
                continue;
            }
            let mut history = WindowHistory::from_snapshot(self.inner.semantic.snapshot());
            if self.inner.route_navigation(&route, &mut history) {
                continue;
            }
            if self.route_transcript_action(&route) {
                continue;
            }
            if route.is_activation()
                && self.inner.committed_generation == Some(self.inner.presentation_generation)
                && self.inner.committed_revision == self.inner.semantic.revision()
                && let Some(action) = route.target.as_ref().and_then(|target| {
                    self.inner
                        .transcript
                        .accepted
                        .activate_local(target.as_str())
                })
            {
                use bcode_tui_components::transcript::accepted::TranscriptLocalAction;
                match action {
                    TranscriptLocalAction::Reprepare => self.prepare(),
                    TranscriptLocalAction::ScrollTo { row, .. } => {
                        self.inner.ui.viewport.restore_anchor(row);
                        self.inner.anchor = self.inner.transcript.anchor(row);
                    }
                    TranscriptLocalAction::External(action) => self.inner.transcript_action(action),
                }
                continue;
            }
            let action = match route.event {
                Event::Key(key) => self.inner.route_key(key, &route, &history, &mut submit),
                Event::Paste(text) => Some(SessionUiAction::Paste(text)),
                Event::Mouse(mouse)
                    if matches!(
                        mouse.kind,
                        MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
                    ) =>
                {
                    Some(if mouse.kind == MouseEventKind::ScrollUp {
                        SessionUiAction::ScrollUp(3)
                    } else {
                        SessionUiAction::ScrollDown(3)
                    })
                }
                Event::Mouse(mouse) if route.targets(&bmux_tui::hit::HitId::new(EDITOR)) => {
                    TextInputControl::new(&session_ui::composer_policy())
                        .handle_mouse(&mut self.inner.ui.composer, mouse);
                    None
                }
                Event::Mouse(mouse) => {
                    self.inner
                        .selection
                        .handle_mouse(self.local.presentation().selection(), mouse);
                    None
                }
                _ => None,
            };
            if let Some(action) = action {
                submit |=
                    self.inner.ui.update(action, &mut history) == SessionUiEffect::SubmitRequested;
            }
            if let Some(direction) = history.requested {
                self.inner
                    .push_intent(serde_json::json!({"type":"history","direction":direction}));
            }
        }
        self.inner.capture_anchor();
        Ok(submit)
    }

    fn route_transcript_action(&mut self, route: &bmux_tui::interaction::InteractionRoute) -> bool {
        if route.is_activation()
            && self.inner.committed_generation == Some(self.inner.presentation_generation)
            && self.inner.committed_revision == self.inner.semantic.revision()
            && let Some(provenance) = route.target.as_ref().and_then(|target| {
                self.inner
                    .transcript
                    .accepted
                    .action_provenance(target.as_str())
            })
        {
            let theme = self
                .inner
                .bindings
                .effective
                .policy
                .as_ref()
                .map(crate::policy::BrowserPolicy::component_theme);
            if let Some(action) = self
                .inner
                .transcript
                .translate_action(&provenance, theme.as_ref())
            {
                self.inner.transcript_action(action);
            }
            // Rejected/stale producer translation never falls back to its
            // retained payload or an inferred service invocation.
            return true;
        }
        false
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
                    OLDER => Some("Load older history"),
                    NEWER => Some("Load newer history"),
                    LATEST => Some("New messages below; return to latest"),
                    EDITOR => Some("Session composer"),
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
            "label": "Bcode session", "description": "", "regions": regions,
        }))
        .map_err(|_| "invalid accessibility metadata")
    }

    /// Prepare projections outside the paint callback.
    pub(crate) fn prepare(&mut self) {
        self.inner.presentation_generation = self.inner.presentation_generation.saturating_add(1);
        self.inner.committed_generation = None;
        self.inner.question.prepare(&self.inner.bindings.effective);
        if let Some(id) = self.inner.question.inline_item.take() {
            let _ = self.inner.transcript.accepted.remove_visual(&id);
        }
        if let Some(snapshot) = self.inner.semantic.snapshot() {
            self.inner
                .transcript
                .prepare_snapshot(snapshot, self.inner.area.width);
            self.inner.transcript.prepare_visuals(
                self.inner
                    .bindings
                    .effective
                    .policy
                    .as_ref()
                    .map(crate::policy::BrowserPolicy::component_theme),
            );
            self.inner.transcript.rows = self.inner.transcript.accepted.layout.total_rows();
            self.inner.question.inline_item = None;
            let inline = self
                .inner
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
            if inline {
                let id = self
                    .inner
                    .question
                    .interaction_id()
                    .and_then(|interaction_id| {
                        self.inner
                            .transcript
                            .accepted
                            .document
                            .iter()
                            .find_map(|item| {
                                (item.interaction()?.interaction_id == interaction_id)
                                    .then(|| item.source_view_item_id().cloned())
                                    .flatten()
                            })
                    });
                if let Some(id) = id {
                    let rows = self.inner.question.height(self.inner.area.width);
                    if let Some(item) = snapshot.transcript.items.iter().find(|item| item.id == id)
                    {
                        let _ = self.inner.transcript.accepted.accept_visual(id.clone(),
                            bcode_tui_components::transcript::accepted::AcceptedVisual {
                                item_revision: item.revision,
                                width: self.inner.area.width,
                                revision: item.revision,
                                rows: bcode_tui_components::transcript::transcript_layout::TranscriptLayoutRows::BlankSpan(usize::from(rows)),
                            });
                        self.inner
                            .transcript
                            .prepare_snapshot(snapshot, self.inner.area.width);
                        self.inner.question.inline_item = Some(id);
                    }
                }
            }
        }
    }

    fn visual_candidates(&self) -> Result<Vec<VisualCandidate>, &'static str> {
        let snapshot = self
            .inner
            .semantic
            .snapshot()
            .ok_or("missing observation")?;
        let mut candidates = Vec::new();
        let mut ordinal = 0usize;
        for item in &snapshot.transcript.items {
            if candidates.len() >= 64 {
                break;
            }
            if !self
                .inner
                .transcript
                .has_visual(item.id.get(), self.inner.area.width)
            {
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
                if candidates.len() >= 64 {
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
                let Some(budget) = self.inner.transcript.artifact_preview_limit(
                    item.id.get(),
                    self.inner.area.width,
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
        let revision = self.inner.semantic.revision();
        if revision != self.inner.visual_discovery_revision {
            self.inner.visual_cursor = 0;
            self.inner.visual_discovery_revision = revision;
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
        let request =
            self.inner
                .semantic
                .prepare_artifact(item_id, reference_key, offset, length)?;
        self.inner.pending_visual_artifact = Some(PendingVisualArtifact {
            item_id: item_id.to_owned(),
            reference_key: reference_key.to_owned(),
            offset,
            length,
            observation: self
                .inner
                .semantic
                .revision()
                .ok_or("missing observation")?,
            preparation: self
                .inner
                .transcript
                .accepted
                .visual_preparation(&bcode_session_view_models::TranscriptViewItemId::new(
                    item_id,
                ))
                .ok_or("missing visual item")?,
            width: self.inner.area.width,
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
        if json.len() > 512 * 1024
            || self.inner.semantic.revision() != Some(pending.observation)
            || self
                .inner
                .transcript
                .accepted
                .visual_preparation(&bcode_session_view_models::TranscriptViewItemId::new(
                    &pending.item_id,
                ))
                .as_ref()
                != Some(&pending.preparation)
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
        let snapshot = self
            .inner
            .semantic
            .snapshot()
            .ok_or("missing observation")?;
        let item = snapshot
            .transcript
            .items
            .iter()
            .find(|item| item.id.get() == pending.item_id)
            .ok_or("stale visual artifact")?;
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
        let accepted = self.inner.transcript.deliver_artifact(
            &pending.item_id,
            pending.width,
            &bcode_tui_components::transcript::tool_visual::portable::PortableArtifactChunk {
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

    /// Capture current semantic identity before loading one bounded artifact image.
    #[must_use]
    pub fn prepare_image(&mut self, item_id: &str, reference: &str) -> bool {
        self.inner.pending_image = self.inner.semantic.snapshot().and_then(|snapshot| {
            self.inner
                .transcript
                .prepare_image(snapshot, item_id, reference, self.inner.area.width)
        });
        self.inner.pending_image.is_some()
    }

    /// Accept decoded RGBA pixels only while every captured input still matches.
    #[must_use]
    pub fn accept_image(&mut self, bytes: Vec<u8>, width: u32, height: u32) -> bool {
        self.inner.pending_image.take().is_some_and(|pending| {
            self.inner
                .transcript
                .accept_image(&pending, bytes, width, height)
        })
    }

    /// Copy canonical accepted Markdown source for the current logical selection.
    #[must_use]
    pub fn selected_text(&self) -> Option<String> {
        let snapshot = self
            .inner
            .selection
            .snapshot(self.local.presentation().selection())?;
        self.inner.transcript.copy(&snapshot)
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
        self.inner.ui.composer.buffer().text()
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
            "editor": self.inner.ui.composer.snapshot()["buffer"],
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
        self.inner.ui.composer = editor;
        self.cancel_pointer();
        Ok(())
    }

    /// Validate a durable receipt and clear only its unchanged draft.
    ///
    /// # Errors
    /// See [`SnapshotState::settle_submission`].
    pub fn settle_submission(&mut self, json: &str) -> Result<(), &'static str> {
        let accepted = self.inner.semantic.settle_submission(json)?;
        if self.draft() == accepted {
            self.inner.ui.composer = TextInputState::new(bmux_text_edit::TextEditBuffer::new());
            self.inner
                .ui
                .update(SessionUiAction::FollowTail, &mut WindowHistory::default());
        }
        Ok(())
    }

    pub(crate) fn commit_output(&mut self) {
        self.inner.committed_generation = Some(self.inner.presentation_generation);
        self.inner.committed_revision = self.inner.semantic.revision();
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

    /// Render to a caller-owned local presentation sink (no network frame decode).
    ///
    /// # Errors
    /// Returns bounded presentation or output errors.
    pub fn present<E>(
        &mut self,
        output: impl FnOnce(&bmux_tui::presentation::PreparedPresentation) -> Result<(), E>,
    ) -> Result<bool, bmux_tui::presentation::PresentationError<E>> {
        let checkpoint = self.stage_question();
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

impl SurfaceState {
    fn transcript_action(
        &mut self,
        action: bcode_tui_components::transcript::actions::TranscriptAction,
    ) {
        use bcode_tui_components::transcript::actions::TranscriptAction;
        match action {
            TranscriptAction::Open(bcode_markdown_render::MarkdownDestination::Web(url)) => {
                self.push_intent(serde_json::json!({"type":"navigate", "url":url.as_str()}));
            }
            TranscriptAction::Copy(text) => {
                self.push_intent(serde_json::json!({"type":"clipboard", "text":text}));
            }
            TranscriptAction::Artifact { artifact_id } => {
                let reference = self.semantic.snapshot().and_then(|snapshot| {
                    snapshot.transcript.items.iter().find_map(|item| {
                        let bcode_session_view_models::TranscriptViewItemKind::ToolInvocation {
                            tool,
                        } = &item.kind
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
                        Some((
                            item.id.get().to_owned(),
                            artifact.artifact.refs.first()?.key.clone(),
                        ))
                    })
                });
                if let Some((item_id, reference_key)) = reference {
                    let image = self.semantic.snapshot().is_some_and(|snapshot| snapshot.transcript.items.iter().any(|item| {
                        item.id.get() == item_id && matches!(&item.kind, bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                            if matches!(&tool.result, Some(bcode_session_view_models::ToolResultView::Artifact { artifact })
                                if artifact.artifact.refs.iter().any(|reference| reference.key == reference_key && reference.content_type.as_deref().is_some_and(|mime| matches!(mime,"image/png"|"image/jpeg"|"image/webp")))))
                    }));
                    self.push_intent(serde_json::json!({"type":if image {"image"} else {"artifact"}, "item_id":item_id, "reference_key":reference_key}));
                }
            }
            // Producer translation does not grant arbitrary controller or
            // service authority to this browser boundary.
            TranscriptAction::Plugin { .. } => self.push_intent(serde_json::json!({"type":"unavailable", "message":"This producer action has no authorized browser capability; no action was sent."})),
            // Local paths never acquire browser host access. Fragment/disclosure
            // navigation requires shared semantic target resolution, not guessed rows.
            TranscriptAction::Open(_) | TranscriptAction::Presentation { .. } => self.push_intent(serde_json::json!({"type":"unavailable", "message":"This destination is not available through the browser's narrow navigation port; no host resource was opened."})),
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

    fn capture_anchor(&mut self) {
        let top = self.ui.viewport.top_row(self.transcript.rows, self.height);
        self.anchor = self.transcript.anchor(top);
    }

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

    fn paint_selection(&self, cx: &mut PaintCx<'_, '_>) {
        if let Some(selection) = self.selection.snapshot(cx.selection()) {
            cx.paint_selection(
                &selection,
                Style::new().add_modifier(bmux_tui::style::Modifier::REVERSED),
            );
        }
    }

    fn sync_viewport(
        ui: &mut SessionUi,
        transcript: &Transcript,
        anchor: Option<&(String, usize)>,
        height: u16,
        snapshot: Option<&bcode_session_view_models::SessionViewSnapshot>,
    ) {
        let anchor = anchor.and_then(|anchor| transcript.resolve_anchor(anchor));
        ui.viewport.sync_with_anchor(
            (
                transcript.rows.saturating_sub(usize::from(height)),
                0,
                transcript.rows,
                height,
            ),
            anchor,
            0,
            0,
            &mut WindowHistory::from_snapshot(snapshot),
        );
    }

    fn paint_question(
        &mut self,
        layout: &bmux_tui::component::LayoutNode,
        top: usize,
        question_height: u16,
        pinned_height: u16,
        cx: &mut PaintCx<'_, '_>,
    ) {
        let inline = self.question.inline_item.as_ref().and_then(|id| {
            let index = self.transcript.accepted.document.source_index(id)?;
            self.transcript.accepted.layout.entry_start_row(
                bcode_tui_components::transcript::transcript_layout::VisibleTranscriptSource::Transcript, index)
        });
        let region = if let Some(start) = inline {
            layout
                .find_logical_rect(&LayoutId::new("bcode-web/transcript"))
                .and_then(|rect| {
                    let end = start.saturating_add(usize::from(question_height));
                    let visible_start = start.max(top);
                    let visible_end = end.min(top.saturating_add(usize::from(self.height)));
                    (visible_start < visible_end).then(|| {
                        (
                            u16::try_from(visible_start.saturating_sub(start)).unwrap_or(u16::MAX),
                            Rect::new(
                                u16::try_from(rect.x).unwrap_or(0),
                                u16::try_from(rect.y).unwrap_or(0).saturating_add(
                                    u16::try_from(visible_start.saturating_sub(top))
                                        .unwrap_or(u16::MAX),
                                ),
                                self.area.width,
                                u16::try_from(visible_end - visible_start)
                                    .unwrap_or(0)
                                    .min(512),
                            ),
                        )
                    })
                })
        } else {
            layout
                .find_logical_rect(&LayoutId::new("bcode-web/question-pinned"))
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

    pub(crate) fn paint(&mut self, cx: &mut PaintCx<'_, '_>) {
        self.question.prepare(&self.bindings.effective);
        self.question.invalidate_geometry();
        if self.area.is_empty() || self.paint_permission(cx) {
            return;
        }
        let (question_height, pinned_height) = self.question_extents();
        let editor = RefCell::new(std::mem::replace(
            &mut self.ui.composer,
            TextInputState::new(bmux_text_edit::TextEditBuffer::new()),
        ));
        let policy = session_ui::composer_policy();
        let composer_height = TextInputControl::new(&policy)
            .visible_rows_for_width(&editor.borrow(), self.area.width.saturating_sub(2))
            .clamp(1, 6)
            .saturating_add(2)
            .min(self.area.height);
        let [
            style,
            surface_style,
            border_style,
            selection_style,
            muted_style,
        ] = self.presentation_styles();
        let transcript = &self.transcript;
        let focused = self.focused;
        let [older_state, newer_state, latest_state] = &self.navigation_buttons;
        self.sync_navigation_focus();
        let history = WindowHistory::from_snapshot(self.semantic.snapshot());
        let show_latest = history.newer || !self.ui.viewport.follows_bottom();
        let build = |top| {
            let mut navigation = Row::new();
            if history.older {
                navigation =
                    navigation.child(ButtonComponent::new(OLDER, "Older history", older_state));
            }
            if history.newer {
                navigation =
                    navigation.child(ButtonComponent::new(NEWER, "Newer history", newer_state));
            }
            if show_latest {
                navigation = navigation.child(ButtonComponent::new(
                    LATEST,
                    "New messages below",
                    latest_state,
                ));
            }
            let transcript_panel = Column::new()
                .child(
                    SizeBox::new(navigation)
                        .height(u64::from(history.older || history.newer || show_latest)),
                )
                .flex(Flex::new(1, TranscriptComponent { transcript, top }))
                .child(
                    SizeBox::new(TextBlock::new(""))
                        .id("bcode-web/question-pinned")
                        .height(u64::from(pinned_height)),
                );
            session_ui::compose(
                &IDS,
                transcript_panel,
                session_ui::composer(
                    &IDS,
                    &editor,
                    &policy,
                    SessionComposerStyle {
                        panel: ComposerStyle {
                            border: border_style,
                            surface: surface_style,
                        },
                        text: style,
                        selection: selection_style,
                        muted: muted_style,
                    },
                    focused,
                ),
                composer_height,
            )
        };
        let constraints = Constraints::tight(Size::new(self.area.width, self.area.height));
        let layout = build(0).layout(constraints, &mut LayoutCx::new());
        self.height = layout
            .find_logical_rect(&LayoutId::new("bcode-web/transcript"))
            .and_then(|rect| u16::try_from(rect.height).ok())
            .unwrap_or(0);
        Self::sync_viewport(
            &mut self.ui,
            &self.transcript,
            self.anchor.as_ref(),
            self.height,
            self.semantic.snapshot(),
        );
        let top = self.ui.viewport.top_row(self.transcript.rows, self.height);
        build(top).paint(&layout, cx);
        self.paint_question(&layout, top, question_height, pinned_height, cx);
        self.ui.composer = editor.into_inner();
        self.paint_selection(cx);
        self.capture_anchor();
    }
}

// Resident bounded window metadata feeds the shared native navigation state machine.
#[derive(Default)]
struct WindowHistory {
    older: bool,
    newer: bool,
    requested: Option<&'static str>,
}
impl WindowHistory {
    fn from_snapshot(snapshot: Option<&bcode_session_view_models::SessionViewSnapshot>) -> Self {
        Self {
            older: snapshot.is_some_and(|s| s.transcript.has_older_history),
            newer: snapshot.is_some_and(|s| s.transcript.has_newer_history),
            requested: None,
        }
    }
}
impl TranscriptHistory for WindowHistory {
    fn has_older_history(&self) -> bool {
        self.older
    }
    fn has_newer_history(&self) -> bool {
        self.newer
    }
    fn reveal_request(&self) -> Option<usize> {
        None
    }
    fn newer_reveal_request(&self) -> Option<usize> {
        None
    }
    fn cancel_newer_reveal(&mut self) {}
    fn cancel_older_reveal(&mut self) {}
    fn request_load(&mut self, _: usize) {
        self.requested = Some("older");
    }
    fn request_load_newer(&mut self, _: usize) {
        self.requested = Some("newer");
    }
    fn clear_reveal_request(&mut self) {}
    fn take_ready_pages(&mut self) -> (bool, bool, bool) {
        (false, false, false)
    }
    fn consume_reveal(&mut self, _: bool, _: usize) {}
}

impl SurfaceState {
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
}

impl SurfaceState {
    fn sync_navigation_focus(&self) {
        let [older_state, newer_state, latest_state] = &self.navigation_buttons;
        for (id, state) in [
            (OLDER, older_state),
            (NEWER, newer_state),
            (LATEST, latest_state),
        ] {
            let mut value = state.get();
            value.set_focused(self.focused && self.focused_hit.as_deref() == Some(id));
            state.set(value);
        }
    }
}

impl SurfaceState {
    fn route_navigation(
        &mut self,
        route: &bmux_tui::interaction::InteractionRoute,
        history: &mut WindowHistory,
    ) -> bool {
        let activation = route.is_activation();
        let target = [OLDER, NEWER, LATEST]
            .into_iter()
            .find(|id| route.targets(&bmux_tui::hit::HitId::new(*id)));
        if activation && let Some(target) = target {
            if target == LATEST {
                self.ui.update(SessionUiAction::FollowTail, history);
                if history.newer {
                    self.push_intent(serde_json::json!({"type":"history","direction":"tail"}));
                }
            } else if (target == OLDER && history.older) || (target == NEWER && history.newer) {
                self.push_intent(serde_json::json!({"type":"history","direction":if target == OLDER { "older" } else { "newer" }}));
            }
            return true;
        }

        false
    }
}

impl SurfaceState {
    fn route_key(
        &mut self,
        key: bmux_keyboard::KeyStroke,
        route: &bmux_tui::interaction::InteractionRoute,
        history: &WindowHistory,
        submit: &mut bool,
    ) -> Option<SessionUiAction> {
        use crate::bindings::Command;
        match self.bindings.command(key) {
            Some(Command::Submit) => {
                *submit = true;
                None
            }
            Some(Command::Newline) => {
                TextInputControl::new(&session_ui::composer_policy())
                    .handle_paste(&mut self.ui.composer, "\n");
                None
            }
            Some(Command::PageUp) => {
                Some(SessionUiAction::ScrollUp(usize::from(self.height.max(1))))
            }
            Some(Command::PageDown) => {
                Some(SessionUiAction::ScrollDown(usize::from(self.height.max(1))))
            }
            Some(Command::Latest) => {
                if history.newer {
                    self.push_intent(serde_json::json!({"type":"history","direction":"tail"}));
                }
                Some(SessionUiAction::FollowTail)
            }
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
                None
            }
            Some(Command::FocusInteraction) => {
                if let Some(id) = &self.question.inline_item
                    && let Some(index) = self.transcript.accepted.document.source_index(id)
                    && let Some(start) = self.transcript.accepted.layout.entry_start_row(bcode_tui_components::transcript::transcript_layout::VisibleTranscriptSource::Transcript, index)
                {
                    let focus = self.question.focused_rows(self.area.width);
                    self.ui.viewport.restore_anchor(start.saturating_add(usize::from(focus.start)));
                }
                None
            }
            Some(Command::Copy) => {
                self.copy_requested = true;
                None
            }
            Some(
                Command::Ignore
                | Command::PermissionApprove
                | Command::PermissionDeny
                | Command::SelectUp
                | Command::SelectDown
                | Command::SelectConfirm
                | Command::SelectCancel,
            ) => None,
            None => {
                if ![OLDER, NEWER, LATEST]
                    .into_iter()
                    .any(|id| route.targets(&bmux_tui::hit::HitId::new(id)))
                {
                    self.bindings.edit(&mut self.ui.composer, key);
                }
                None
            }
        }
    }
}
