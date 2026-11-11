//! Bcode-owned root runtime message and model contracts.
//!
//! These types establish the application boundary before orchestration migrates from the existing
//! chat loop. BMUX treats messages and model state as opaque application data.

use std::collections::{BTreeMap, VecDeque};
use std::time::{Duration, Instant};

use bmux_tui::event::Event;

use super::TuiError;
use super::artifact_stream::ActiveArtifactFetchCompletion;
use super::chat_loop::{ChatLoopState, DraftAutosave, TuiRuntimeSettings};
use super::effects::TuiEffectResult;
use super::history_flow;
use super::invalidation::{InvalidationKey, TemporalRegistry};
use super::session_flow::ActiveChat;

/// Typed Bcode event admitted to the root TUI runtime.
#[allow(dead_code)]
pub enum BcodeRuntimeMessage {
    /// Install root-runtime subscriptions after all application state is owned by the program.
    Bootstrap {
        /// Runtime control handle retained for live cadence reconfiguration.
        handle: bmux_tui_runtime::RuntimeHandle<Self>,
    },
    /// Terminal input backend failure surfaced through the serialized root update path.
    TerminalInputFailed(std::io::Error),
    /// Reliable terminal input after BMUX decoding and admission classification.
    Terminal(Event),
    /// Ordered canonical/session-view stream update.
    SessionStream(Box<history_flow::SessionStreamUpdate>),
    /// Full bounded activity replacement, fenced to a local scope generation.
    ChildWatch {
        generation: u64,
        result: Result<bcode_session_models::SessionActivityWatchDelivery, String>,
    },
    /// Combined replacement from all admitted watch batches.
    ChildWatchProjection {
        generation: u64,
        result: Result<bcode_session_models::SessionActivityWatchSnapshot, String>,
    },
    /// Completed revision-bound plugin composer search.
    ComposerSearchCompleted(super::composer_interaction::ComposerCompletion),
    /// Completed artifact fetch for the active presentation generation.
    ArtifactFetchCompleted(Box<ActiveArtifactFetchCompletion>),
    /// Plugin surface host invalidation became pending.
    PluginSurfaceInvalidated,
    /// Completed Bcode-owned background effect.
    EffectCompleted(Box<TuiEffectResult>),
    /// Completed V2 transcript Markdown image or Mermaid worker request.
    TranscriptV2Visual(Box<super::transcript_v2_host::visuals::VisualCompletion>),
    /// Completed attempt to open the next inline interactive surface.
    InteractiveSurfaceOpened(Result<super::interactive_surface::InteractiveSurfaceState, String>),
    /// A committed native interaction update, scoped to its original session and exchange.
    NativeDurableChanged {
        session_id: bcode_session_models::SessionId,
        exchange_id: String,
        terminal: bool,
        error: Option<bcode_plugin_sdk::interaction_application::InteractionControllerError>,
    },
    /// Completed inline interactive surface resolution request.
    InteractiveSurfaceResolved(Result<bool, bcode_client::ClientError>),
    /// Due Bcode-owned semantic invalidations.
    Invalidations(Vec<InvalidationKey>),
    /// Draft autosave deadline.
    DraftSaveDue,
    /// Interactive-surface retry deadline.
    InteractionRetryDue,
    /// Streaming-configurator source or shared-presentation deadline.
    StreamingConfiguratorDue,
    /// Streaming-presentation interpolation deadline.
    StreamingPresentationDue,
    /// Client telemetry flush deadline.
    TelemetryFlushDue,
}

impl BcodeRuntimeMessage {
    #[allow(dead_code)]
    fn latest_key(&self) -> Option<bmux_tui_runtime::MessageKey> {
        match self {
            Self::StreamingConfiguratorDue => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.streaming_configurator",
            )),
            Self::StreamingPresentationDue => Some(bmux_tui_runtime::MessageKey::new(
                "bcode.streaming_presentation",
            )),
            Self::TelemetryFlushDue => {
                Some(bmux_tui_runtime::MessageKey::new("bcode.telemetry_flush"))
            }
            Self::Bootstrap { .. }
            | Self::TerminalInputFailed(_)
            | Self::Terminal(_)
            | Self::SessionStream(_)
            | Self::ChildWatch { .. }
            | Self::ChildWatchProjection { .. }
            | Self::ArtifactFetchCompleted(_)
            | Self::ComposerSearchCompleted(_)
            | Self::PluginSurfaceInvalidated
            | Self::EffectCompleted(_)
            | Self::TranscriptV2Visual(_)
            | Self::InteractiveSurfaceOpened(_)
            | Self::InteractiveSurfaceResolved(_)
            | Self::NativeDurableChanged { .. }
            | Self::Invalidations(_)
            | Self::DraftSaveDue
            | Self::InteractionRetryDue => None,
        }
    }
}

/// Admit one Bcode message using its domain-owned reliability classification.
///
/// # Errors
///
/// Returns an admission error when the runtime is closed or keyed latest-value capacity is full.
#[allow(dead_code)]
pub async fn admit(
    handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    message: BcodeRuntimeMessage,
) -> Result<(), BcodeRuntimeAdmissionError> {
    if let Some(key) = message.latest_key() {
        handle
            .send_latest(key, message)
            .map(|_| ())
            .map_err(|error| match error {
                bmux_tui_runtime::LatestSendError::Full(_) => BcodeRuntimeAdmissionError::Full,
                bmux_tui_runtime::LatestSendError::Closed(_) => BcodeRuntimeAdmissionError::Closed,
            })
    } else {
        handle
            .send(message)
            .await
            .map_err(|_| BcodeRuntimeAdmissionError::Closed)
    }
}

/// Normalized Bcode root-runtime admission failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum BcodeRuntimeAdmissionError {
    /// Distinct-key latest-value capacity is exhausted.
    Full,
    /// Runtime admission has closed.
    Closed,
}

#[derive(Default)]
struct OrderedPresentationQueue {
    pending: std::collections::BTreeMap<
        bcode_session_models::SessionId,
        VecDeque<super::effects::TuiEffect>,
    >,
    active: std::collections::BTreeSet<bcode_session_models::SessionId>,
}

impl OrderedPresentationQueue {
    fn append(
        &mut self,
        notes: std::collections::BTreeMap<
            bcode_session_models::SessionId,
            VecDeque<super::effects::TuiEffect>,
        >,
    ) {
        for (session_id, mut pending) in notes {
            self.pending
                .entry(session_id)
                .or_default()
                .append(&mut pending);
        }
    }

    fn complete(&mut self, session_id: bcode_session_models::SessionId) {
        self.active.remove(&session_id);
    }

    fn take_ready(&mut self) -> Vec<super::effects::TuiEffect> {
        let ready = self
            .pending
            .keys()
            .filter(|session_id| !self.active.contains(session_id))
            .copied()
            .collect::<Vec<_>>();
        let mut effects = Vec::with_capacity(ready.len());
        for session_id in ready {
            if let Some(effect) = self
                .pending
                .get_mut(&session_id)
                .and_then(VecDeque::pop_front)
            {
                self.active.insert(session_id);
                effects.push(effect);
            }
            if self
                .pending
                .get(&session_id)
                .is_some_and(VecDeque::is_empty)
            {
                self.pending.remove(&session_id);
            }
        }
        effects
    }
}

///
/// Presentation caches and nested screen state remain Bcode-owned. This model deliberately does
/// not expose Bcode types through BMUX contracts.
#[allow(dead_code)]
pub struct BcodeRuntimeModel {
    /// Canonical application/session owner used by the existing chat path.
    /// Heap-owned so generic runtime futures do not copy the large retained app
    /// into every success/error settlement state on a native thread's stack.
    pub chat: Box<ActiveChat>,
    /// Bcode-specific effect, plugin-surface, projection, artifact, image, and cache state.
    pub loop_state: Box<ChatLoopState>,
    /// Reloadable keymap, cadence, plugin, and launch settings.
    pub settings: TuiRuntimeSettings,
    /// Draft autosave generation and deadline state.
    pub draft_autosave: DraftAutosave,
    /// Current top-level navigation/screen state.
    pub screen: BcodeRuntimeScreen,
    /// Previously active root screen retained while temporary native-session navigation runs.
    suspended_screen: Option<BcodeRuntimeScreen>,
    /// Runtime control handle used for live cadence replacement.
    pub runtime_handle: Option<bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>>,
    /// Deferred application messages blocked by an explicit paint or navigation barrier.
    pub deferred: VecDeque<BcodeRuntimeMessage>,
    /// Bcode-owned per-session ordered presentation-note scheduler.
    ordered_notes: OrderedPresentationQueue,
    /// Current merged Bcode semantic presentation damage.
    pub invalidation: super::invalidation::UiInvalidation,
    /// Generic terminal-space damage selected by Bcode for the next presentation.
    presentation_damage: bmux_tui::damage::Damage,
    /// A composer edit since the last presentation repaints the composer region alone
    /// (F5, D6). Kept until a frame presents it, so later updates cannot drop the region.
    composer_damage: ComposerDamage,
    /// Last successfully committed terminal hit map used for pointer routing.
    pub committed_hits: bmux_tui::hit::HitMap,
    /// Last successfully committed logical content-selection scene.
    pub committed_selection: bmux_tui::selection::SelectionScene,
    committed_visual_targets: Vec<(bmux_tui::geometry::Rect, String)>,
    /// Canonical plain-text export for the current logical selection.
    pub selected_plain_text: Option<String>,
    /// Last successfully committed terminal frame area.
    pub committed_area: bmux_tui::geometry::Rect,
    /// Lifecycle adapter selected by the existing foreground/focus authority.
    focused_input_owner: Option<FocusedInputOwner>,
    composer_field: bcode_plugin_sdk::tui::FocusedTextFieldAdapter,
    focused_input_command: Option<bmux_tui_runtime::Command<BcodeRuntimeMessage>>,
    canonical_interaction: Option<(bcode_session_models::SessionId, String)>,
    focused_visual: Option<String>,
    /// Last successfully committed frame layout used for regional damage projection.
    committed_layout: Option<super::render::FrameLayout>,
    /// Last successfully committed presentation timestamp.
    pub last_presented_at: Option<Instant>,
    /// Whether closing the root plugin surface should terminate this runtime invocation.
    exit_after_plugin_surface: bool,
    /// Standalone plugin close result returned to the launcher.
    plugin_surface_result: Option<(String, Option<serde_json::Value>)>,
    /// Whether the root program should terminate after its dirty state is committed.
    pub exit_requested: bool,
    pub(crate) resurrection: Option<super::resurrection::Publisher>,
    theme_input_signature: u64,
    theme_reload_at: Instant,
    child_watch_scope: Option<bcode_session_models::SessionActivityWatchScope>,
    child_watch_navigation: Option<bcode_session_models::SessionId>,
    child_watch_generation: u64,
    child_watch_picker_generation: Option<u64>,
    scheduled_deadlines: BTreeMap<bmux_tui_runtime::TimerId, Instant>,
    temporal_registry: TemporalRegistry,
    selection_clear_at: Option<Instant>,
    selection_clear_generation: u64,
    selection_session: Option<bcode_session_models::SessionId>,
    selection_policy: (bool, bool, u64),
    transcript_clipboard: fn(&str) -> Result<(), String>,
    /// The transcript engine host: ingest, input and painting of the transcript.
    transcript_host: Box<super::transcript_v2_host::TranscriptV2Host>,
}

enum RootTimer {
    Invalidations,
    ArtifactRetry,
    StreamingConfigurator,
    StreamingPresentation,
    DraftSave,
    InteractiveSurfaceRetry,
    TelemetryFlush,
    ThemeReload,
    /// V2 transcript engine timers (motion frames, grace expiry).
    TranscriptV2,
    SelectionClear(u64),
}

impl RootTimer {
    fn id(self) -> bmux_tui_runtime::TimerId {
        if let Self::SelectionClear(generation) = self {
            return bmux_tui_runtime::TimerId::new(format!("bcode.selection_clear.{generation}"));
        }
        bmux_tui_runtime::TimerId::new(match self {
            Self::Invalidations => "bcode.invalidations",
            Self::ArtifactRetry => "bcode.artifact_retry",
            Self::StreamingConfigurator => "bcode.streaming_configurator",
            Self::StreamingPresentation => "bcode.streaming_presentation",
            Self::DraftSave => "bcode.draft_save",
            Self::InteractiveSurfaceRetry => "bcode.interactive_surface_retry",
            Self::TelemetryFlush => "bcode.telemetry_flush",
            Self::ThemeReload => "bcode.theme_reload",
            Self::TranscriptV2 => "bcode.transcript_v2",
            Self::SelectionClear(_) => unreachable!("selection timer handled above"),
        })
    }
}

/// Whether the composer region needs presenting since the last presentation (F5, D6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ComposerDamage {
    /// Nothing pending.
    None,
    /// A plain composer edit repaints the composer region.
    Edited,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FocusedInputOwner {
    Composer,
    RootSurface,
    InteractiveSurface,
}

impl BcodeRuntimeModel {
    /// Compose the transient interaction exactly once before the owner's ordinary input.
    fn compose_focused_input_event(&mut self, event: &Event) -> bmux_tui::event::EventOutcome {
        if let Event::Key(stroke) = event {
            // Application-level interrupts and navigation retain priority over completion.
            if stroke.key != bmux_keyboard::KeyCode::Escape
                && self.root_interactive_surface_host_key(*stroke).is_some()
            {
                return bmux_tui::event::EventOutcome::Ignored;
            }
            return self.composer_interaction_key_outcome(*stroke);
        }
        if let Event::Mouse(mouse) = event {
            let outcome = self.loop_state.composer_interaction.list_event(event);
            if matches!(
                outcome,
                bmux_tui_components::selectable_list::SelectableListOutcome::Selected(_)
            ) {
                self.accept_composer_candidate();
            }
            if !matches!(
                outcome,
                bmux_tui_components::selectable_list::SelectableListOutcome::Ignored
            ) {
                return bmux_tui::event::EventOutcome::Redraw;
            }
            // An outside press dismisses the transient interaction, then reaches the
            // ordinary owner once. It is never replayed against a new hit map.
            if self.loop_state.composer_interaction.visible()
                && matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_))
            {
                self.loop_state.composer_interaction.dismiss();
                self.invalidation = self
                    .invalidation
                    .merge(super::invalidation::UiInvalidation::Structural);
            }
        }
        bmux_tui::event::EventOutcome::Ignored
    }

    fn composer_interaction_key_outcome(
        &mut self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> bmux_tui::event::EventOutcome {
        use bcode_plugin_sdk::tui::keymap::controls::{ControlAction, ControlInput};
        use bmux_tui_components::completion::CompletionCommand;
        if (self.loop_state.foreground().is_some()
            && self.loop_state.foreground() != Some(super::foreground::Foreground::Plugin))
            || !self.loop_state.composer_interaction.visible()
        {
            return bmux_tui::event::EventOutcome::Ignored;
        }
        if let Some(binding) = self
            .settings
            .keymap()
            .registry()
            .action_for_key("composer_interaction", stroke)
            && self.loop_state.composer_interaction.invoke_action(binding)
        {
            // The normal post-input synchronization consumes the pending activation.
            // Do not observe or mutate the editor here, or bypass its current owner.
            self.loop_state
                .composer_interaction
                .resolve_hints(self.settings.keymap().registry());
            self.invalidation = self
                .invalidation
                .merge(super::invalidation::UiInvalidation::Structural);
            return bmux_tui::event::EventOutcome::Handled;
        }
        let input = ControlInput::from_registry(
            self.settings.keymap().registry(),
            "composer_interaction",
            stroke,
            &[
                ControlAction::Cancel,
                ControlAction::Submit,
                ControlAction::FocusNext,
                ControlAction::Up,
                ControlAction::Down,
            ],
            false,
        );
        let command = match input {
            ControlInput::Action(ControlAction::Cancel) => CompletionCommand::Dismiss,
            ControlInput::Action(ControlAction::Submit | ControlAction::FocusNext) => {
                CompletionCommand::Accept
            }
            ControlInput::Action(ControlAction::Up) => CompletionCommand::Previous,
            ControlInput::Action(ControlAction::Down) => CompletionCommand::Next,
            _ => return bmux_tui::event::EventOutcome::Ignored,
        };
        let (outcome, accept) = self.loop_state.composer_interaction.command(command);
        if command == CompletionCommand::Dismiss {
            self.loop_state.composer_interaction.dismiss();
        } else if accept {
            self.accept_composer_candidate();
        }
        if accept {
            bmux_tui::event::EventOutcome::Redraw
        } else {
            outcome
        }
    }

    /// Resolve the live field through the retained lifecycle adapter, never its plugin kind.
    fn active_focused_field(&self) -> Option<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot> {
        match self.focused_input_owner? {
            FocusedInputOwner::Composer => self.composer_field.snapshot(self.chat.app.composer()),
            FocusedInputOwner::RootSurface => self.loop_state.root_focused_text_field(),
            FocusedInputOwner::InteractiveSurface => {
                self.loop_state.interactive_focused_text_field()
            }
        }
    }

    fn apply_active_focused_edit(
        &mut self,
        edit: &bcode_plugin_sdk::tui::FocusedTextFieldEdit,
    ) -> bcode_plugin_sdk::tui::FocusedTextFieldEditResult {
        use bcode_plugin_sdk::tui::FocusedTextFieldEditResult;
        let rejected = FocusedTextFieldEditResult::Rejected(
            bcode_plugin_sdk::composer::ComposerValidationError::Stale,
        );
        match self.focused_input_owner {
            Some(FocusedInputOwner::RootSurface) => {
                let Some(client) = self.loop_state.foreground_client() else {
                    return rejected;
                };
                let launch = self.fresh_session_settings();
                let Some(result) = self
                    .loop_state
                    .apply_root_focused_text_field_edit(edit, &client, launch)
                else {
                    return rejected;
                };
                let damage = self.apply_root_surface_event_result(result.event);
                self.invalidation = self.invalidation.merge(damage);
                result.edit_result
            }
            Some(FocusedInputOwner::InteractiveSurface) => {
                let (result, outcome) = self
                    .loop_state
                    .apply_interactive_focused_text_field_edit(edit);
                if let super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
                    resolution,
                ) = &outcome
                    && let Some(interaction_id) = self
                        .loop_state
                        .active_interactive_surface_id()
                        .map(ToOwned::to_owned)
                {
                    self.focused_input_command = Some(self.interactive_surface_resolution_command(
                        interaction_id,
                        resolution.clone(),
                    ));
                }
                if matches!(
                    outcome,
                    super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
                        | super::interactive_surface::InteractiveSurfaceEventOutcome::Relayout
                ) {
                    self.invalidation = self
                        .invalidation
                        .merge(super::invalidation::UiInvalidation::Structural);
                }
                result
            }
            Some(FocusedInputOwner::Composer) => {
                let result = self
                    .composer_field
                    .apply(self.chat.app.composer_mut(), edit);
                if result == FocusedTextFieldEditResult::Applied {
                    self.chat.app.reset_input_history_navigation();
                    self.chat.app.wake_cursor();
                    self.draft_autosave.observe(&self.chat, Instant::now());
                }
                result
            }
            _ => rejected,
        }
    }

    fn reject_composer_candidate(&mut self) {
        self.loop_state.composer_interaction.dismiss();
        self.chat
            .app
            .set_status("Suggestion could not be applied; draft unchanged".to_owned());
    }

    /// An acceptance intent is consumed even when its validated edit cannot be committed.
    fn accept_composer_candidate(&mut self) {
        if let Some(edit) = self.loop_state.composer_interaction.focused_edit() {
            let Some(current) = self.active_focused_field() else {
                self.reject_composer_candidate();
                return;
            };
            match edit.validate(&current) {
                Ok(Some(next)) => {
                    if self.apply_active_focused_edit(&edit)
                        == bcode_plugin_sdk::tui::FocusedTextFieldEditResult::Applied
                    {
                        self.loop_state
                            .composer_interaction
                            .finish_focused_edit(next);
                    } else {
                        self.reject_composer_candidate();
                    }
                }
                Ok(None) => self.loop_state.composer_interaction.dismiss(),
                Err(_) => self.reject_composer_candidate(),
            }
            return;
        }
        self.loop_state.composer_interaction.dismiss();
    }

    /// Reconcile the complete composer observation before replies/input and after every root update.
    fn synchronize_composer_interaction(&mut self) {
        self.synchronize_composer_interaction_with_paint(false);
    }

    fn synchronize_composer_interaction_with_paint(&mut self, after_paint: bool) {
        let previous_owner = self.focused_input_owner;
        let standalone =
            self.loop_state.foreground() == Some(super::foreground::Foreground::Plugin);
        let interactive_owner = standalone
            || (self.loop_state.has_interactive_surface()
                && self
                    .loop_state
                    .routes_non_mouse_event_to_interactive_surface(
                        self.chat.app.tui_config().interactions,
                    ));
        self.focused_input_owner = if standalone {
            Some(FocusedInputOwner::RootSurface)
        } else if self.loop_state.foreground().is_some()
            || (!matches!(self.screen, BcodeRuntimeScreen::Chat) && !interactive_owner)
        {
            None
        } else if interactive_owner {
            Some(FocusedInputOwner::InteractiveSurface)
        } else {
            Some(FocusedInputOwner::Composer)
        };
        self.composer_field.observe(
            self.chat.app.composer(),
            matches!(self.focused_input_owner, Some(FocusedInputOwner::Composer)),
        );
        let field = self.active_focused_field();
        if self.chat.app.plugin_presentation().is_none()
            || (!matches!(self.screen, BcodeRuntimeScreen::Chat) && !interactive_owner)
            || (self.loop_state.foreground().is_some() && !standalone)
            || (interactive_owner && field.is_none() && after_paint)
            || self.runtime_handle.is_none()
        {
            // Tool-owned input does not observe the composer or upgrade its damage.
            // Cancellation still invalidates pending replies even without a text change.
            if self.loop_state.composer_interaction.visible() {
                self.invalidation = self
                    .invalidation
                    .merge(super::invalidation::UiInvalidation::Structural);
            }
            self.loop_state.composer_interaction.cancel();
            return;
        }
        let Some(scope) = self.composer_completion_scope(interactive_owner) else {
            self.loop_state.composer_interaction.cancel();
            return;
        };
        let Some(field) = field else {
            // Owners invalidate measured snapshots on edits. Keep presentation through
            // that gap, but revoke pending replies/edits immediately. A committed paint
            // with no snapshot (or a changed owner) is authoritative loss of eligibility.
            if after_paint || previous_owner != self.focused_input_owner {
                self.loop_state.composer_interaction.cancel();
            } else if matches!(self.focused_input_owner, Some(FocusedInputOwner::Composer)) {
                self.loop_state
                    .composer_interaction
                    .observe_unpainted_composer(scope, self.chat.app.composer());
            } else {
                self.loop_state
                    .composer_interaction
                    .await_field_paint(&scope);
            }
            return;
        };
        let changed = self
            .loop_state
            .composer_interaction
            .observe_focused_field(scope.clone(), field);
        if changed {
            self.invalidation = self
                .invalidation
                .merge(super::invalidation::UiInvalidation::Structural);
        }
        if !changed && !self.loop_state.composer_interaction.needs_activation() {
            return;
        }
        let reactivation = self.loop_state.composer_interaction.take_reactivation();
        let activation = {
            let Some(presentation) = self.chat.app.plugin_presentation() else {
                return;
            };
            let Some(document) = self.loop_state.composer_interaction.document() else {
                return;
            };
            reactivation.map_or_else(
                || presentation.activate_composer(document),
                |route| {
                    presentation
                        .reactivate_composer(&route, document)
                        .map(|activation| (route, activation))
                },
            )
        };
        let Some((route, activation)) = activation else {
            self.loop_state.composer_interaction.dismiss();
            return;
        };
        let Some(request) = self
            .loop_state
            .composer_interaction
            .start(route.clone(), activation)
        else {
            return;
        };
        self.dispatch_composer_search(scope, route, request);
    }

    /// Prepare the root frame's retained selection and temporal damage policy together.
    fn prepare_root_presentation(
        &mut self,
    ) -> (Option<std::time::Duration>, bmux_tui::damage::Damage) {
        self.synchronize_selection_lifecycle();
        let interval = self.settings.bmux_runtime_config().frame_interval;
        self.composer_damage = ComposerDamage::None;
        let damage = std::mem::replace(
            &mut self.presentation_damage,
            bmux_tui::damage::Damage::None,
        );
        let launch = self.fresh_session_settings();
        self.loop_state.set_render_launch(launch);
        (interval, damage)
    }

    /// Resolve contextual hints without inventing authority when session metadata is absent.
    fn composer_completion_scope(
        &self,
        owner_field: bool,
    ) -> Option<super::composer_interaction::ComposerScope> {
        let presentation = self.chat.app.plugin_presentation()?;
        let session = self.chat.attached_session_id();
        if owner_field && session.is_none() && !self.loop_state.has_root_plugin_surface() {
            return None;
        }
        let workspace = if session.is_some() {
            self.chat
                .app
                .working_directory()
                .filter(|path| path.is_absolute())
                .map(std::path::Path::to_path_buf)
        } else {
            let launch = self.settings.launch_working_directory();
            if !launch.is_absolute() {
                return None;
            }
            Some(launch.to_path_buf())
        };
        let agent = if owner_field {
            self.chat.app.owning_agent_id()
        } else {
            self.chat
                .app
                .pending_agent_id()
                .or_else(|| self.chat.app.owning_agent_id())
        };
        Some(super::composer_interaction::ComposerScope {
            session,
            workspace,
            agent_id: agent.map(str::to_owned),
            registry_identity: presentation.composer_identity(),
            registry_revision: presentation.revision(),
        })
    }

    /// Dispatch one bounded search, retaining the original client identity for cancellation.
    fn dispatch_composer_search(
        &mut self,
        scope: super::composer_interaction::ComposerScope,
        route: bcode_plugin_sdk::composer::PluginOwnedComposerContribution,
        request: bcode_plugin_sdk::composer::ComposerSearchRequest,
    ) {
        let Some(handle) = self.runtime_handle.clone() else {
            return;
        };
        let Some(client) = self.loop_state.foreground_client() else {
            return;
        };
        let task_client = client.clone();
        let context = bcode_ipc::PluginServiceContext {
            invocation_id: request.request_id.clone(),
            workspace: bcode_plugin_sdk::WorkspaceContext {
                session_id: scope.session.map(|session| session.to_string()),
                launch_root: scope.workspace,
            },
            agent_id: if self
                .loop_state
                .composer_interaction
                .focused_field()
                .is_some()
            {
                None
            } else {
                scope.agent_id
            },
            deadline_ms: 2_000,
        };
        let task = tokio::spawn(async move {
            let snapshot = match serde_json::to_vec(&request) {
                Ok(payload) => task_client
                    .invoke_plugin_service_contextual(
                        context,
                        route.plugin_id,
                        route.contribution.service_interface_id,
                        bcode_plugin_sdk::composer::OP_COMPOSER_SEARCH.to_owned(),
                        payload,
                    )
                    .await
                    .ok()
                    .filter(|response| {
                        response.error.is_none() && response.payload.len() <= 2 * 1024 * 1024
                    })
                    .and_then(|response| serde_json::from_slice(&response.payload).ok()),
                Err(_) => None,
            };
            let _ = handle
                .send(BcodeRuntimeMessage::ComposerSearchCompleted(
                    super::composer_interaction::ComposerCompletion {
                        request_id: request.request_id,
                        snapshot,
                    },
                ))
                .await;
        });
        self.loop_state
            .composer_interaction
            .retain_task(task.abort_handle(), client);
    }

    fn fresh_session_settings(&self) -> super::plugin_surface_host::FreshSessionSettings {
        super::plugin_surface_host::FreshSessionSettings {
            keymap: self.settings.keymap().registry().clone(),
            working_directory: self
                .chat
                .app
                .working_directory()
                .unwrap_or_else(|| self.settings.launch_working_directory())
                .to_path_buf(),
            provider: self
                .chat
                .app
                .selected_provider_plugin_id()
                .map(ToOwned::to_owned),
            model: self.chat.app.selected_model_id().map(ToOwned::to_owned),
            agent: Some(
                self.chat
                    .app
                    .pending_agent_id()
                    .unwrap_or_else(|| self.chat.app.current_agent_id())
                    .to_owned(),
            ),
            effort: self.chat.app.reasoning_effort().map(ToOwned::to_owned),
            summary: self.chat.app.reasoning_summary().map(ToOwned::to_owned),
            diff_layout: Some(super::plugin_surface_host::plugin_diff_layout(
                self.chat.app.effective_diff_viewer_config(),
            )),
        }
    }

    #[allow(dead_code)]
    pub fn new(
        chat: ActiveChat,
        settings: TuiRuntimeSettings,
        mut loop_state: ChatLoopState,
    ) -> Self {
        loop_state
            .set_session_picker_launch_directory(settings.launch_working_directory().to_path_buf());
        loop_state.set_slash_palette_settings(settings.completion_settings());
        let draft_autosave = DraftAutosave::new(
            settings.launch_working_directory().to_path_buf(),
            chat.app.composer().text().to_owned(),
        );
        let theme_input_signature = super::theme::active_theme_input_signature(&chat.app);
        let theme_reload_at = Instant::now() + Duration::from_millis(750);
        let selection_session = chat.app.session_id();
        let selection_policy = (
            settings.selection().auto_copy_on_mouse_release,
            settings.selection().clear_after_copy,
            settings.selection().clear_delay_ms,
        );
        Self {
            transcript_host: Box::default(),
            selection_clear_at: None,
            selection_clear_generation: 0,
            selection_session,
            selection_policy,
            transcript_clipboard: {
                #[cfg(not(test))]
                let copy: fn(&str) -> Result<(), String> = |text| {
                    super::markdown_activation::copy_text(text).map_err(|error| error.to_string())
                };
                #[cfg(test)]
                let copy: fn(&str) -> Result<(), String> =
                    |_| Err("test clipboard not injected".to_owned());
                copy
            },
            chat: Box::new(chat),
            loop_state: Box::new(loop_state),
            settings,
            draft_autosave,
            screen: BcodeRuntimeScreen::Chat,
            suspended_screen: None,
            runtime_handle: None,
            deferred: VecDeque::new(),
            ordered_notes: OrderedPresentationQueue::default(),
            invalidation: super::invalidation::UiInvalidation::Full,
            presentation_damage: bmux_tui::damage::Damage::Full,
            committed_hits: bmux_tui::hit::HitMap::default(),
            committed_selection: bmux_tui::selection::SelectionScene::new(),
            committed_visual_targets: Vec::new(),
            selected_plain_text: None,
            committed_area: bmux_tui::geometry::Rect::new(0, 0, 0, 0),
            focused_input_owner: None,
            focused_input_command: None,
            canonical_interaction: None,
            composer_field: bcode_plugin_sdk::tui::FocusedTextFieldAdapter::new("chat", "composer"),
            focused_visual: None,
            committed_layout: None,
            composer_damage: ComposerDamage::None,
            last_presented_at: None,
            exit_after_plugin_surface: false,
            plugin_surface_result: None,
            exit_requested: false,
            resurrection: None,
            theme_input_signature,
            theme_reload_at,
            child_watch_scope: None,
            child_watch_navigation: None,
            child_watch_generation: 0,
            child_watch_picker_generation: None,
            scheduled_deadlines: BTreeMap::new(),
            temporal_registry: TemporalRegistry::default(),
        }
    }

    fn active_screen(&self) -> BcodeRuntimeScreen {
        self.loop_state.active_root_screen()
    }

    fn synchronize_screen(&mut self) -> super::invalidation::UiInvalidation {
        self.synchronize_selection_lifecycle();
        let next = self.active_screen();
        if next == self.screen {
            return super::invalidation::UiInvalidation::None;
        }
        self.screen = next;
        super::invalidation::UiInvalidation::Structural
    }

    const fn suspend_screen_for_session_navigation(&mut self) {
        self.suspended_screen = Some(self.screen);
        self.screen = BcodeRuntimeScreen::Chat;
    }

    fn finish_plugin_session_attach(
        &mut self,
        session_id: bcode_session_models::SessionId,
        result: Result<(), String>,
    ) {
        if result.is_ok() {
            // Attach success begins the linked chat visit; it is not a return.
            let _attached = self
                .loop_state
                .mark_root_plugin_session_attached(session_id);
        } else {
            let _completed = self
                .loop_state
                .complete_root_plugin_session_navigation(session_id, result);
            self.resume_screen_after_session_navigation();
        }
    }

    fn resume_screen_after_session_navigation(&mut self) {
        let expected = self.suspended_screen.take();
        let active = self.active_screen();
        self.screen = expected
            .filter(|screen| *screen == active)
            .unwrap_or(active);
    }

    pub fn handle_ralph_surface_action(
        &mut self,
        action: super::ralph_launcher::RalphHomeAction,
    ) -> super::invalidation::UiInvalidation {
        use super::ralph_flow::RalphRootAction;

        let action = match action {
            super::ralph_launcher::RalphHomeAction::Plan => RalphRootAction::Plan,
            super::ralph_launcher::RalphHomeAction::SaveDraft => RalphRootAction::SaveDraft,
            super::ralph_launcher::RalphHomeAction::ViewDraft => RalphRootAction::ViewDraft,
            super::ralph_launcher::RalphHomeAction::ReviseDraft => RalphRootAction::ReviseDraft,
            super::ralph_launcher::RalphHomeAction::ApproveDraft => RalphRootAction::ApproveDraft,
            super::ralph_launcher::RalphHomeAction::ApplyDraftToLoop => {
                RalphRootAction::ApplyDraftToLoop
            }
            super::ralph_launcher::RalphHomeAction::CreateFromDraft => {
                RalphRootAction::CreateFromDraft
            }
            super::ralph_launcher::RalphHomeAction::Run
            | super::ralph_launcher::RalphHomeAction::Goal => RalphRootAction::Run,
            super::ralph_launcher::RalphHomeAction::Approve => RalphRootAction::Approve,
            super::ralph_launcher::RalphHomeAction::Stop => RalphRootAction::Stop,
            super::ralph_launcher::RalphHomeAction::Resume => RalphRootAction::Resume,
            super::ralph_launcher::RalphHomeAction::Status => RalphRootAction::ShowStatus,
            super::ralph_launcher::RalphHomeAction::Runs => RalphRootAction::ListRuns,
            super::ralph_launcher::RalphHomeAction::Iterations => RalphRootAction::ListIterations,
            super::ralph_launcher::RalphHomeAction::Open => RalphRootAction::OpenProgress,
            super::ralph_launcher::RalphHomeAction::Audit => RalphRootAction::Audit,
            super::ralph_launcher::RalphHomeAction::Replan => RalphRootAction::Replan,
            super::ralph_launcher::RalphHomeAction::RebuildLoopContext => {
                self.chat
                    .app
                    .set_status("Ralph rebuild is unavailable from this surface".to_owned());
                return super::invalidation::UiInvalidation::Structural;
            }
            super::ralph_launcher::RalphHomeAction::Start => {
                self.loop_state.open_ralph_start_dialog(
                    self.settings.launch_working_directory(),
                    &mut self.chat,
                );
                return super::invalidation::UiInvalidation::Structural;
            }
        };
        if action.requires_client() {
            let repo_root = self.chat.app.working_directory().map_or_else(
                || self.settings.launch_working_directory().to_path_buf(),
                std::path::Path::to_path_buf,
            );
            self.chat
                .replace_effect(super::effects::TuiEffect::RalphAction { repo_root, action });
            self.chat.app.set_status("running Ralph action…".to_owned());
        } else if let Err(error) =
            super::ralph_flow::execute_root_local_action(&mut self.chat, action)
        {
            self.chat
                .app
                .set_status(format!("Ralph action failed: {error}"));
        }
        super::invalidation::UiInvalidation::Structural
    }

    /// Return whether a root plugin surface is active.
    #[cfg(test)]
    pub const fn has_plugin_surface(&self) -> bool {
        self.loop_state.has_root_plugin_surface()
    }

    /// Queue a plugin-owned surface that should be active when the root runtime starts.
    pub fn queue_plugin_surface(
        &mut self,
        plugin_id: impl Into<String>,
        surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    ) {
        self.loop_state
            .queue_root_plugin_surface(plugin_id, surface);
        let _invalidation = self.synchronize_screen();
        self.invalidation = super::invalidation::UiInvalidation::Full;
    }

    /// Queue a plugin-owned surface as the complete standalone product screen.
    pub fn queue_standalone_plugin_surface(
        &mut self,
        plugin_id: impl Into<String>,
        surface: bcode_plugin_sdk::tui::BoxedPluginTuiSurface,
    ) {
        self.queue_plugin_surface(plugin_id, surface);
        self.exit_after_plugin_surface = true;
    }

    /// Take the close result produced by a standalone plugin surface.
    pub const fn take_plugin_surface_result(
        &mut self,
    ) -> Option<(String, Option<serde_json::Value>)> {
        self.plugin_surface_result.take()
    }

    fn handle_permission_key(
        &mut self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> super::invalidation::UiInvalidation {
        // Ctrl+C on a managed approval stops the native turn: the request is denied and the
        // turn interrupted together, instead of leaving the native agent waiting.
        let interrupt = stroke.key == bmux_keyboard::KeyCode::Char('c')
            && stroke.modifiers.ctrl
            && self
                .loop_state
                .permission_dialog
                .as_ref()
                .is_some_and(|dialog| {
                    matches!(
                        dialog.origin(),
                        super::permission_dialog::PermissionOrigin::Managed { .. }
                    )
                });
        if interrupt {
            if let Some(super::permission_dialog::PermissionDialogOutcome::Resolve(resolution)) =
                self.loop_state
                    .permission_dialog
                    .as_mut()
                    .map(|dialog| dialog.handle_action(super::keymap::BmuxAction::PermissionDeny))
            {
                self.queue_permission_resolution(resolution);
            }
            super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
            return super::invalidation::UiInvalidation::Structural;
        }
        let Some(action) = self
            .settings
            .keymap()
            .action_for_key(super::keymap::BmuxScope::Permission, stroke)
        else {
            return super::invalidation::UiInvalidation::None;
        };
        let Some(dialog) = self.loop_state.permission_dialog.as_mut() else {
            return super::invalidation::UiInvalidation::None;
        };
        match dialog.handle_action(action) {
            super::permission_dialog::PermissionDialogOutcome::FocusChanged(label) => {
                self.chat
                    .app
                    .set_status(format!("permission choice: {label}"));
            }
            super::permission_dialog::PermissionDialogOutcome::Resolve(resolution) => {
                self.queue_permission_resolution(resolution);
            }
            super::permission_dialog::PermissionDialogOutcome::Ignored => {
                return super::invalidation::UiInvalidation::None;
            }
        }
        super::invalidation::UiInvalidation::Structural
    }

    fn queue_permission_resolution(
        &mut self,
        resolution: super::permission_dialog::PermissionDialogResolution,
    ) {
        super::managed_flow::dispatch_permission_resolution(
            &mut self.chat,
            &mut self.loop_state,
            &resolution,
        );
    }

    fn apply_plugin_surface_close(
        &mut self,
        plugin_id: &str,
        outcome: Option<serde_json::Value>,
    ) -> super::invalidation::UiInvalidation {
        if self.exit_after_plugin_surface {
            self.plugin_surface_result = Some((plugin_id.to_owned(), outcome));
            self.exit_requested = true;
            return super::invalidation::UiInvalidation::Structural;
        }
        if plugin_id == "bcode.ralph" {
            let action = outcome
                .and_then(|value| value.get("ralph_action").cloned())
                .and_then(|value| serde_json::from_value(value).ok());
            if let Some(action) = action {
                return self.handle_ralph_surface_action(action);
            }
            self.chat.app.set_status("Ralph UI closed".to_owned());
            return super::invalidation::UiInvalidation::Structural;
        }
        super::palette_flow::apply_plugin_surface_outcome(&mut self.chat, plugin_id, outcome);
        super::invalidation::UiInvalidation::Structural
    }

    fn finish_terminal_route_update(
        &mut self,
        damage: super::invalidation::UiInvalidation,
    ) -> super::invalidation::UiInvalidation {
        damage.merge(self.synchronize_screen())
    }

    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn route_terminal_event(&mut self, event: Event) -> super::invalidation::UiInvalidation {
        let damage = self.handle_basic_terminal_event(event);
        // Rendered `#bcode-session-<id>` links switch sessions.
        let session_link = self.transcript_host.take_session_link_request();
        if let Some(session_id) = session_link {
            super::session_flow::start_switch_session(
                &mut self.chat,
                session_id,
                super::history_flow::initial_transcript_window_request(self.committed_area),
            );
        }
        self.finish_terminal_route_update(damage)
    }

    fn return_from_borrowed_chat(&mut self, event: &Event) -> bool {
        if self.loop_state.root_plugin_session_is_attached()
            && let Event::Key(stroke) = event
            && self
                .settings
                .keymap()
                .action_for_key(super::keymap::BmuxScope::Chat, *stroke)
                == Some(super::keymap::BmuxAction::AppExit)
            && let Some(session_id) = self
                .loop_state
                .root_plugin_surface_pending_session_navigation()
        {
            let _completed = self
                .loop_state
                .complete_root_plugin_session_navigation(session_id, Ok(()));
            self.resume_screen_after_session_navigation();
            return true;
        }
        false
    }

    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn apply_root_surface_event_result(
        &mut self,
        result: bcode_plugin_sdk::tui::PluginTuiEventResult,
    ) -> super::invalidation::UiInvalidation {
        if self.chat.app.managed_execution_session().is_some() {
            self.chat
                .app
                .set_status("Plugin execution is disabled in this managed conversation".into());
            return super::invalidation::UiInvalidation::Structural;
        }
        let has_effect = result.outcome == bmux_tui::event::EventOutcome::Redraw
            || !matches!(result.action, bcode_plugin_sdk::tui::PluginTuiAction::None);
        match result.action {
            bcode_plugin_sdk::tui::PluginTuiAction::None
            | bcode_plugin_sdk::tui::PluginTuiAction::Redraw => {}
            bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome } => {
                if let Some((plugin_id, outcome)) = self
                    .loop_state
                    .close_root_plugin_surface_with_outcome(outcome)
                {
                    return self.apply_plugin_surface_close(&plugin_id, outcome);
                }
            }
            bcode_plugin_sdk::tui::PluginTuiAction::OpenSession { session_id } => {
                self.suspend_screen_for_session_navigation();
                self.loop_state
                    .suspend_root_plugin_surface_for_session(session_id);
                super::session_flow::start_switch_session(
                    &mut self.chat,
                    session_id,
                    super::history_flow::initial_transcript_window_request(self.committed_area),
                );
            }
            bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface {
                plugin_id,
                surface_id,
                options,
            } => {
                self.chat
                    .replace_effect(super::effects::TuiEffect::OpenPluginSurface {
                        plugin_id,
                        surface_kind: surface_id.clone(),
                        instance_id: format!("workflow-related-{surface_id}"),
                        options,
                        working_directory: self.settings.launch_working_directory().to_path_buf(),
                        session_id: self.chat.attached_session_id(),
                    });
            }
            bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns => {
                let (updates, requests) = super::plugin_surface_host::subscribe_workflow_views(
                    self.loop_state
                        .foreground_client()
                        .expect("ordinary plugin action requires model application access"),
                    self.loop_state
                        .root_plugin_surface_invalidation()
                        .expect("plugin surface invalidation"),
                );
                self.loop_state
                    .attach_root_plugin_surface_updates(updates, requests);
            }
            bcode_plugin_sdk::tui::PluginTuiAction::SelectWorkflowRun { run_id } => {
                self.loop_state.request_root_workflow_run(run_id);
            }
            bcode_plugin_sdk::tui::PluginTuiAction::UpdateWorkflowCatalogQuery {
                filter,
                sort,
                group,
                search,
            } => {
                self.loop_state
                    .request_root_workflow_query(filter, sort, group, search);
            }
            bcode_plugin_sdk::tui::PluginTuiAction::LoadMoreWorkflowRuns { cursor } => {
                self.loop_state.request_more_root_workflow_runs(cursor);
            }
            bcode_plugin_sdk::tui::PluginTuiAction::InvokePluginCommand {
                plugin_id,
                command_id,
                arguments,
            } => {
                self.chat
                    .replace_effect(super::effects::TuiEffect::InvokePluginCommand {
                        plugin_id,
                        command_id,
                        arguments,
                        working_directory: self.settings.launch_working_directory().to_path_buf(),
                        session_id: self.chat.attached_session_id(),
                    });
            }
            bcode_plugin_sdk::tui::PluginTuiAction::RunCommand { command } => {
                self.chat.app.replace_composer_with(&command);
                self.loop_state.close_root_plugin_surface();
            }
        }
        if has_effect {
            super::invalidation::UiInvalidation::Structural
        } else {
            // Explicit Handled/None stays in the owning surface, without manufacturing damage.
            super::invalidation::UiInvalidation::None
        }
    }

    // Retain the existing scoped lint exceptions for the unchanged terminal
    // fallback dispatcher; focused-input composition lives outside this handler.
    #[allow(clippy::let_and_return, clippy::too_many_lines)]
    fn handle_basic_terminal_event(&mut self, event: Event) -> super::invalidation::UiInvalidation {
        if self.chat.app.managed_execution_session().is_some() {
            if let Event::Key(stroke) = &event {
                let action = self.settings.keymap().action("chat", *stroke);
                if action == Some("app.interrupt") {
                    super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                    return super::invalidation::UiInvalidation::Structural;
                }
                if action == Some("tui.agent.cycle") {
                    // The agent cycle selects native permission modes in a managed chat.
                    super::managed_flow::cycle_permission_mode(
                        &mut self.chat,
                        &mut self.loop_state,
                    );
                    return super::invalidation::UiInvalidation::Structural;
                }
                if action.is_some_and(managed_blocked_chat_action) {
                    self.chat.app.set_status(
                        "Model/provider controls are unavailable in this managed conversation"
                            .into(),
                    );
                    return super::invalidation::UiInvalidation::Structural;
                }
            }
            self.focused_visual = None;
        }
        self.synchronize_selection_lifecycle();
        if self.return_from_borrowed_chat(&event) {
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_root_plugin_surface()
            && !self.loop_state.root_plugin_surface_is_suspended()
            && self.loop_state.foreground_client().is_some()
        {
            let result = self
                .loop_state
                .handle_root_plugin_surface_event(
                    &event,
                    &self
                        .loop_state
                        .foreground_client()
                        .expect("ordinary plugin action requires model application access"),
                    self.fresh_session_settings(),
                )
                .expect("plugin surface was present");
            // The foreground surface owns input even when it ignores it. Ignored
            // pointer motion must not select, scroll, or edit the covered chat.
            return self.apply_root_surface_event_result(result);
        }
        if self.loop_state.has_session_picker() {
            if let Event::Mouse(mouse) = &event
                && matches!(
                    mouse.kind,
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
                )
                && let Some(id) = super::mouse_flow::mouse_hit_id(&self.committed_hits, *mouse)
                && self
                    .loop_state
                    .handle_session_catalog_control(&mut self.chat, &id)
            {
                return super::invalidation::UiInvalidation::Structural;
            }
            match self.loop_state.handle_session_picker_event(
                &mut self.chat,
                self.settings.keymap(),
                &event,
            ) {
                super::chat_loop::SessionPickerRootOutcome::Create => {
                    super::session_flow::switch_to_draft_session(&mut self.chat);
                    self.chat
                        .replace_effect(super::effects::TuiEffect::LoadDraftStatus {
                            launch_working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                        });
                }
                super::chat_loop::SessionPickerRootOutcome::SearchHit(hit) => {
                    match super::session_flow::start_switch_session_from_search_hit(
                        &mut self.chat,
                        &hit,
                        super::history_flow::initial_transcript_window_request(self.committed_area),
                    ) {
                        Ok(()) => self
                            .chat
                            .app
                            .set_status("opening search result…".to_owned()),
                        Err(error) => self
                            .chat
                            .app
                            .set_status(format!("search result unavailable: {error:?}")),
                    }
                }
                super::chat_loop::SessionPickerRootOutcome::Select(session_id) => {
                    super::session_flow::start_switch_session(
                        &mut self.chat,
                        session_id,
                        super::history_flow::initial_transcript_window_request(self.committed_area),
                    );
                    self.chat.app.set_status("opening session…".to_owned());
                }
                super::chat_loop::SessionPickerRootOutcome::Canceled => {
                    self.chat.app.set_status("session picker closed".to_owned());
                }
                super::chat_loop::SessionPickerRootOutcome::Handled => {}
                super::chat_loop::SessionPickerRootOutcome::Unhandled => {
                    unreachable!("session picker was present");
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_auth_pool_picker() {
            match self
                .loop_state
                .handle_auth_pool_picker_event(&event, self.settings.keymap())
            {
                super::chat_loop::AuthPoolPickerRootOutcome::Continue => {}
                super::chat_loop::AuthPoolPickerRootOutcome::Cancel => {
                    self.chat.app.set_status("accounts closed".to_owned());
                }
                super::chat_loop::AuthPoolPickerRootOutcome::Apply(action) => {
                    if let Some(session_id) = self.chat.attached_session_id() {
                        self.chat
                            .start_effect(super::effects::TuiEffect::SetAuthPoolPreference {
                                session_id,
                                action,
                            });
                        self.chat
                            .app
                            .set_status("Updating account settings…".to_owned());
                    }
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_model_picker() {
            if let Some((provider_plugin_id, action)) = self
                .loop_state
                .handle_model_picker_event(self.settings.keymap(), &event)
            {
                match action {
                    super::model_flow::ModelPickerAction::Continue => {
                        if self.loop_state.has_model_picker() {
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        self.chat
                            .replace_effect(super::effects::TuiEffect::LoadModelPicker {
                                provider_plugin_id,
                            });
                        self.chat.app.set_status("loading models…".to_owned());
                    }
                    super::model_flow::ModelPickerAction::Cancel => {
                        self.chat.app.set_status("model picker closed".to_owned());
                    }
                    super::model_flow::ModelPickerAction::Check(model_id) => {
                        if let Some(generation) = self.loop_state.start_selected_model_check() {
                            self.chat
                                .start_effect(super::effects::TuiEffect::CheckSelectedModel {
                                    session_id: self.chat.attached_session_id(),
                                    provider_plugin_id,
                                    model_id,
                                    generation,
                                });
                        }
                    }
                    super::model_flow::ModelPickerAction::Select(model_id)
                        if self.chat.app.managed_execution_session().is_some() =>
                    {
                        let model_id = self
                            .loop_state
                            .managed_model_selector(&model_id)
                            .unwrap_or(model_id);
                        super::managed_flow::start_configure(
                            &mut self.chat,
                            &mut self.loop_state,
                            bcode_harness_models::HarnessSessionSetting::Model { model_id },
                        );
                    }
                    super::model_flow::ModelPickerAction::Select(model_id) => {
                        if let Some(session_id) = self.chat.attached_session_id() {
                            self.chat
                                .start_effect(super::effects::TuiEffect::SetSessionModel {
                                    session_id,
                                    provider_plugin_id,
                                    model_id,
                                });
                            self.chat.app.set_status("applying model…".to_owned());
                        } else {
                            self.chat
                                .app
                                .apply_local_model_selection(provider_plugin_id, &model_id);
                        }
                    }
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_skill_picker() {
            let action = self
                .loop_state
                .handle_skill_picker_event(self.settings.keymap(), &event)
                .expect("skill picker was present");
            match action {
                super::skill_picker::SkillPickerAction::Continue => {}
                super::skill_picker::SkillPickerAction::Cancel => {
                    self.chat.app.set_status("skill picker closed".to_owned());
                }
                super::skill_picker::SkillPickerAction::Help(skill_id) => {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::DescribeSkill { skill_id });
                    self.chat
                        .app
                        .set_status("loading skill details…".to_owned());
                }
                super::skill_picker::SkillPickerAction::Activate(skill_id) => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Activate,
                        skill_id,
                        String::new(),
                    );
                }
                super::skill_picker::SkillPickerAction::Deactivate(skill_id) => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Deactivate,
                        skill_id,
                        String::new(),
                    );
                }
                super::skill_picker::SkillPickerAction::Invoke {
                    skill_id,
                    arguments,
                } => {
                    self.start_root_skill_action(
                        super::effects::SkillActionKind::Invoke,
                        skill_id,
                        arguments,
                    );
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_ralph_start_dialog() {
            match self
                .loop_state
                .handle_ralph_start_dialog_event(&event, self.settings.keymap())
                .expect("Ralph start dialog was present")
            {
                super::ralph_start_dialog::RalphStartDialogOutcome::Handled => {}
                super::ralph_start_dialog::RalphStartDialogOutcome::Canceled => {
                    self.chat.app.set_status("Ralph start canceled".to_owned());
                }
                super::ralph_start_dialog::RalphStartDialogOutcome::Submit => {
                    let mut dialog = self
                        .loop_state
                        .take_ralph_start_dialog()
                        .expect("Ralph start dialog was present");
                    if dialog.loop_name_text().is_empty() {
                        dialog.set_status("Ralph loop name is required");
                        self.loop_state.restore_ralph_start_dialog(dialog);
                    } else {
                        let repo_root = self.chat.app.working_directory().map_or_else(
                            || self.settings.launch_working_directory().to_path_buf(),
                            std::path::Path::to_path_buf,
                        );
                        self.chat
                            .start_effect(super::effects::TuiEffect::RalphStart {
                                request: super::ralph_flow::RalphStartRequest {
                                    loop_name: dialog.loop_name_text(),
                                    repo_root,
                                    session_id: self.chat.attached_session_id(),
                                    session_title: self
                                        .chat
                                        .app
                                        .session_title()
                                        .map(ToOwned::to_owned),
                                    work_area_path: dialog.work_area_path_text(),
                                    branch: dialog.branch_text(),
                                    validation_commands: dialog.validation_command_texts(),
                                },
                            });
                        self.chat.app.set_status("creating Ralph loop…".to_owned());
                    }
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_working_directory_dialog() {
            match self
                .loop_state
                .handle_working_directory_dialog_event(self.settings.keymap(), &event)
            {
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Apply(path) => {
                    let Some(session_id) = self.chat.attached_session_id() else {
                        self.chat.app.set_status("no active session".to_owned());
                        return super::invalidation::UiInvalidation::Structural;
                    };
                    self.chat
                        .start_effect(super::effects::TuiEffect::AttachWorktree {
                            session_id,
                            path,
                        });
                }
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Canceled => {
                    self.chat
                        .app
                        .set_status("working directory change canceled".to_owned());
                }
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Handled => {}
                super::chat_loop::WorkingDirectoryDialogRootOutcome::Unhandled => {
                    unreachable!("working-directory dialog was present")
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        if self.loop_state.has_worktree_create_dialog() {
            match self
                .loop_state
                .handle_worktree_create_dialog_event(self.settings.keymap(), &event)
            {
                super::chat_loop::WorktreeCreateDialogRootOutcome::Create {
                    name,
                    target,
                    base,
                } => {
                    let attach_session_id = match target {
                        super::wt_create_dialog::WorktreeCreateTarget::CurrentSession => {
                            self.chat.attached_session_id()
                        }
                        super::wt_create_dialog::WorktreeCreateTarget::NewSession => None,
                    };
                    self.chat
                        .start_effect(super::effects::TuiEffect::CreateWorktree {
                            operation_id: format!(
                                "tui-worktree-{}-{}",
                                std::process::id(),
                                std::time::SystemTime::now()
                                    .duration_since(std::time::UNIX_EPOCH)
                                    .unwrap_or_default()
                                    .as_nanos()
                            ),
                            request: bcode_worktree_models::WorktreeCreateRequest {
                                name,
                                cwd: Some(
                                    self.chat
                                        .app
                                        .working_directory()
                                        .unwrap_or_else(|| self.settings.launch_working_directory())
                                        .to_path_buf(),
                                ),
                                path: None,
                                branch: None,
                                new_branch: None,
                                base_ref: Some(base.model()),
                                detach: false,
                                force: false,
                                attach_session_id,
                                new_session: target
                                    == super::wt_create_dialog::WorktreeCreateTarget::NewSession,
                                no_setup: false,
                            },
                        });
                    self.chat.app.set_status("creating worktree…".to_owned());
                }
                super::chat_loop::WorktreeCreateDialogRootOutcome::Canceled => {
                    self.chat
                        .app
                        .set_status("worktree creation canceled".to_owned());
                }
                super::chat_loop::WorktreeCreateDialogRootOutcome::Handled => {}
                super::chat_loop::WorktreeCreateDialogRootOutcome::Unhandled => {
                    unreachable!("worktree dialog was present")
                }
            }
            return super::invalidation::UiInvalidation::Structural;
        }
        let damage = match event {
            Event::Resize(_) => {
                self.chat.app.composer_submit.dismiss();
                self.loop_state.composer_interaction.painted_list = None;
                self.loop_state.composer_interaction.painted_layout = None;
                super::invalidation::UiInvalidation::Full
            }
            Event::Focus(bmux_tui::event::FocusEvent::Lost) => {
                self.chat.app.composer_submit.dismiss();
                self.loop_state.composer_interaction.dismiss();
                super::invalidation::UiInvalidation::Structural
            }
            Event::Tick => {
                if !self.loop_state.modal_foreground() {
                    let result = self
                        .chat
                        .app
                        .plugin_presentation()
                        .map(super::plugin_tui::PluginTuiPresentation::drain_frontend_text);
                    match result {
                        Some(Ok(texts)) if !texts.is_empty() => {
                            for text in texts {
                                self.chat.app.reset_input_history_navigation();
                                self.chat.app.paste_composer_text(&text);
                            }
                            self.chat.app.wake_cursor();
                            self.draft_autosave.observe(&self.chat, Instant::now());
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        Some(Err(message)) => self.chat.app.set_status(message),
                        _ => {}
                    }
                }
                super::invalidation::UiInvalidation::Paint
            }
            Event::Focus(_) => super::invalidation::UiInvalidation::Paint,
            Event::Paste(text) => {
                self.chat.app.composer_submit.dismiss();
                if self.loop_state.modal_foreground() {
                    return super::invalidation::UiInvalidation::None;
                }
                self.chat.app.reset_input_history_navigation();
                self.chat.app.paste_composer_text(&text);
                self.chat.app.wake_cursor();
                self.loop_state.refresh_slash_palette(&mut self.chat);
                self.draft_autosave.observe(&self.chat, Instant::now());
                super::invalidation::UiInvalidation::Structural
            }
            Event::Key(stroke) => {
                let submit_menu_open = self.chat.app.composer_submit.is_open();
                self.chat.app.composer_submit.dismiss();
                if !self.loop_state.modal_foreground()
                    && self.settings.keymap().action("chat", stroke) == Some("tui.monitor.toggle")
                {
                    self.chat.app.toggle_child_panel();
                    if let Some(session_id) = self.chat.attached_session_id() {
                        self.chat
                            .replace_effect(super::effects::TuiEffect::LoadChildActivity {
                                session_id,
                                after: None,
                                generation: 0,
                            });
                        self.chat
                            .replace_effect(super::effects::TuiEffect::LoadSessionChildren {
                                parent_id: session_id,
                                after: None,
                                generation: 0,
                            });
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if !self.loop_state.modal_foreground()
                    && self.chat.app.child_panel_focused()
                    && self.settings.keymap().action("monitor", stroke).is_some()
                {
                    match self.settings.keymap().action("monitor", stroke) {
                        Some("tui.control.up") => self.chat.app.move_monitor_selection(false),
                        Some("tui.control.down") => self.chat.app.move_monitor_selection(true),
                        Some("tui.control.right") => {
                            if let Some(session_id) = self.chat.app.expand_monitor_selected() {
                                self.chat.replace_effect(
                                    super::effects::TuiEffect::LoadSessionChildren {
                                        parent_id: session_id,
                                        after: None,
                                        generation: 0,
                                    },
                                );
                                self.chat.replace_effect(
                                    super::effects::TuiEffect::LoadChildActivity {
                                        session_id,
                                        after: None,
                                        generation: 0,
                                    },
                                );
                            }
                        }
                        Some("tui.monitor.nextPage") => {
                            if let Some((parent_id, after)) = self.chat.app.monitor_next_page() {
                                self.chat.replace_effect(
                                    super::effects::TuiEffect::LoadSessionChildren {
                                        parent_id,
                                        after: Some(after),
                                        generation: 0,
                                    },
                                );
                            }
                        }
                        Some("tui.control.submit" | "tui.control.left") => {
                            let target = if self.settings.keymap().action("monitor", stroke)
                                == Some("tui.control.left")
                            {
                                self.chat
                                    .app
                                    .session_view_snapshot()
                                    .session_summary
                                    .as_ref()
                                    .and_then(|summary| summary.parent_id)
                            } else {
                                self.chat.app.selected_monitor_session()
                            };
                            if let Some(session_id) = target {
                                super::session_flow::start_switch_session(
                                    &mut self.chat,
                                    session_id,
                                    super::history_flow::initial_transcript_window_request(
                                        self.committed_area,
                                    ),
                                );
                            }
                        }
                        Some("tui.control.cancel") => self.chat.app.release_child_panel_focus(),
                        _ => return super::invalidation::UiInvalidation::None,
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if !self.loop_state.modal_foreground()
                    && let Some(identity) = &self.focused_visual
                {
                    if self.settings.keymap().action("visual", stroke) == Some("tui.control.cancel")
                    {
                        self.focused_visual = None;
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    let host = super::plugin_surface_host::root_host(
                        bmux_tui_runtime::InvalidationSignal::new(),
                        self.loop_state
                            .foreground_client()
                            .expect("ordinary plugin action requires model application access"),
                        self.fresh_session_settings(),
                        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                    );
                    if self
                        .chat
                        .app
                        .plugin_presentation()
                        .is_some_and(|presentation| {
                            presentation.content_event_with_host(
                                identity,
                                &Event::Key(stroke),
                                &host,
                            )
                        })
                    {
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    self.focused_visual = None;
                }
                if self.loop_state.permission_dialog.is_some() {
                    return self.handle_permission_key(stroke);
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Streaming)
                    && self.loop_state.handle_streaming_configurator_key(
                        &mut self.chat,
                        stroke,
                        self.settings.keymap(),
                    )
                {
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Streaming) {
                    return super::invalidation::UiInvalidation::None;
                }
                match self.loop_state.handle_timeline_dialog_key(
                    &mut self.chat,
                    stroke,
                    self.settings.keymap(),
                ) {
                    super::chat_loop::TimelineDialogRootOutcome::Handled => {
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::TimelineDialogRootOutcome::Jump(entry) => {
                        self.apply_root_timeline_jump(&entry);
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::TimelineDialogRootOutcome::Unhandled => {}
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Timeline) {
                    return super::invalidation::UiInvalidation::None;
                }
                match self.loop_state.handle_thinking_dialog_key(
                    &mut self.chat,
                    stroke,
                    self.settings.keymap(),
                ) {
                    super::chat_loop::ThinkingDialogRootOutcome::Handled => {
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::ThinkingDialogRootOutcome::Apply {
                        effort,
                        summary,
                        visible,
                        mode,
                    } => {
                        self.apply_root_thinking_dialog(effort, summary, visible, mode);
                        return super::invalidation::UiInvalidation::Structural;
                    }
                    super::chat_loop::ThinkingDialogRootOutcome::Unhandled => {}
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Thinking) {
                    return super::invalidation::UiInvalidation::None;
                }
                if self.loop_state.handle_appearance_picker_key(
                    &mut self.chat,
                    stroke,
                    self.settings.keymap(),
                ) {
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.has_command_palette() {
                    if let Some(action) = self
                        .loop_state
                        .handle_command_palette_key(stroke, self.settings.keymap())
                    {
                        self.apply_root_command_action(action);
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.has_slash_palette() {
                    match self.loop_state.handle_slash_palette_key(
                        &mut self.chat,
                        stroke,
                        self.settings.keymap(),
                    ) {
                        super::chat_loop::SlashPaletteRootOutcome::Handled => {
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        super::chat_loop::SlashPaletteRootOutcome::Submit => {
                            self.stage_root_submission(bcode_ipc::PromptPlacement::Steering);
                            return super::invalidation::UiInvalidation::Structural;
                        }
                        super::chat_loop::SlashPaletteRootOutcome::Unhandled => {}
                    }
                }
                if self.loop_state.modal_foreground() {
                    return super::invalidation::UiInvalidation::None;
                }
                if let Some(damage) = self.route_transcript_input(&Event::Key(stroke)) {
                    return damage;
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::ClipboardPasteImage)
                {
                    self.paste_clipboard_image();
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::TranscriptClearSelection)
                {
                    self.clear_transcript_selection();
                    return super::invalidation::UiInvalidation::Paint;
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::TranscriptCopySelection)
                {
                    self.copy_transcript_selection();
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::CommandPaletteOpen)
                {
                    self.loop_state.open_command_palette(&mut self.chat);
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self
                    .settings
                    .keymap()
                    .action_for_key(super::keymap::BmuxScope::Chat, stroke)
                    == Some(super::keymap::BmuxAction::SessionSearchOpen)
                {
                    self.loop_state.open_session_search(&mut self.chat);
                    return super::invalidation::UiInvalidation::Structural;
                }
                let slash_palette_before = self.loop_state.has_slash_palette();
                let outcome =
                    super::input::handle_key(&mut self.chat.app, self.settings.keymap(), stroke);
                match outcome.request {
                    super::input::KeyRequest::None => {}
                    super::input::KeyRequest::Interrupt => {
                        super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                    }
                    super::input::KeyRequest::CycleAgent => {
                        super::chat_loop::cycle_session_agent(&mut self.chat);
                    }
                    super::input::KeyRequest::CycleThinkingEffort => {
                        super::thinking_flow::cycle_thinking_effort(&mut self.chat);
                    }
                    super::input::KeyRequest::Submit { placement } => {
                        self.stage_root_submission(placement);
                    }
                }
                self.loop_state.refresh_slash_palette(&mut self.chat);
                self.draft_autosave.observe(&self.chat, Instant::now());
                // A plain composer edit repaints the composer alone (F5, D6). Requests, opened
                // menus and palettes stay structural, and an edit that resizes the composer
                // changes the chrome layout, which repaints fully at draw time.
                if outcome.redraw
                    && !submit_menu_open
                    && matches!(outcome.request, super::input::KeyRequest::None)
                    && !slash_palette_before
                    && !self.loop_state.has_slash_palette()
                    && !self.chat.app.composer_submit.is_open()
                {
                    self.composer_damage = ComposerDamage::Edited;
                    super::invalidation::UiInvalidation::Paint
                } else if outcome.redraw || submit_menu_open {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            Event::Mouse(mouse) => {
                if !self.loop_state.modal_foreground()
                    && !self.loop_state.has_command_palette()
                    && !self.loop_state.has_slash_palette()
                {
                    let enabled = !self.chat.app.composer().text().trim().is_empty()
                        && self.chat.opening_session_id().is_none();
                    let outcome =
                        self.chat
                            .app
                            .composer_submit
                            .mouse(&self.committed_hits, mouse, enabled);
                    if let Some(placement) = outcome.placement {
                        self.chat.app.stage_submission();
                        self.stage_root_submission(placement);
                        self.loop_state.refresh_slash_palette(&mut self.chat);
                        self.draft_autosave.observe(&self.chat, Instant::now());
                    }
                    if outcome.handled {
                        return if outcome.structural {
                            super::invalidation::UiInvalidation::Structural
                        } else if outcome.redraw {
                            super::invalidation::UiInvalidation::Paint
                        } else {
                            super::invalidation::UiInvalidation::None
                        };
                    }
                }
                if !self.loop_state.modal_foreground()
                    && self.chat.app.child_panel_open()
                    && matches!(
                        mouse.kind,
                        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
                    )
                    && super::mouse_flow::mouse_hit_id(&self.committed_hits, mouse).as_deref()
                        == Some("child-monitor-more")
                    && let Some((parent_id, after)) = self.chat.app.monitor_next_page()
                {
                    self.chat
                        .start_effect(super::effects::TuiEffect::LoadSessionChildren {
                            parent_id,
                            after: Some(after),
                            generation: 0,
                        });
                    return super::invalidation::UiInvalidation::Structural;
                }
                if !self.loop_state.modal_foreground()
                    && self.chat.app.child_panel_open()
                    && matches!(
                        mouse.kind,
                        bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left)
                    )
                    && let Some(hit) = super::mouse_flow::mouse_hit_id(&self.committed_hits, mouse)
                    && let Some(id) = hit
                        .strip_prefix("child-monitor:")
                        .or_else(|| hit.strip_prefix("child-monitor-expand:"))
                    && let Ok(session_id) = id.parse()
                {
                    self.chat.app.select_monitor_session(session_id);
                    if hit.starts_with("child-monitor-expand:")
                        && let Some(parent_id) = self.chat.app.expand_monitor_selected()
                    {
                        self.chat
                            .start_effect(super::effects::TuiEffect::LoadSessionChildren {
                                parent_id,
                                after: None,
                                generation: 0,
                            });
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if !self.loop_state.modal_foreground()
                    && matches!(
                        mouse.kind,
                        bmux_tui::event::MouseEventKind::ScrollLeft
                            | bmux_tui::event::MouseEventKind::ScrollRight
                            | bmux_tui::event::MouseEventKind::Down(
                                bmux_tui::event::MouseButton::Right
                            )
                    )
                    && let Some(identity) =
                        self.committed_visual_targets
                            .iter()
                            .find_map(|(area, identity)| {
                                area.contains(mouse.position).then_some(identity)
                            })
                    && let Some(presentation) = self.chat.app.plugin_presentation()
                    && self.loop_state.foreground_client().is_some()
                    && presentation.content_event_with_host(
                        identity,
                        &Event::Mouse(mouse),
                        &super::plugin_surface_host::root_host(
                            bmux_tui_runtime::InvalidationSignal::new(),
                            self.loop_state
                                .foreground_client()
                                .expect("ordinary plugin action requires model application access"),
                            self.fresh_session_settings(),
                            std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
                        ),
                    )
                {
                    if matches!(mouse.kind, bmux_tui::event::MouseEventKind::Down(_)) {
                        self.focused_visual = Some(identity.clone());
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Permission) {
                    let hit_id = super::mouse_flow::mouse_hit_id(&self.committed_hits, mouse);
                    let changed = super::mouse_flow::handle_permission_action_mouse(
                        hit_id.as_deref(),
                        &mut self.chat,
                        &mut self.loop_state,
                        mouse,
                    );
                    return if changed {
                        super::invalidation::UiInvalidation::Structural
                    } else {
                        super::invalidation::UiInvalidation::None
                    };
                }
                if let Some(damage) = self.route_transcript_input(&Event::Mouse(mouse)) {
                    return damage;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Streaming)
                    && self.loop_state.handle_streaming_configurator_mouse(
                        &mut self.chat,
                        mouse,
                        self.committed_area,
                    )
                {
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Appearance)
                    && self.loop_state.handle_appearance_picker_mouse(
                        &mut self.chat,
                        mouse,
                        self.committed_area,
                    )
                {
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Commands) {
                    if let Some(action) = self
                        .loop_state
                        .handle_command_palette_mouse(mouse, self.committed_area)
                    {
                        self.apply_root_command_action(action);
                    }
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.foreground() == Some(super::foreground::Foreground::Slash) {
                    let _handled = self.loop_state.handle_slash_palette_mouse(
                        &mut self.chat,
                        mouse,
                        self.committed_area,
                    );
                    return super::invalidation::UiInvalidation::Structural;
                }
                if self.loop_state.modal_foreground() {
                    return super::invalidation::UiInvalidation::None;
                }
                let hit_id = super::mouse_flow::mouse_hit_id(&self.committed_hits, mouse);
                let changed = if self.loop_state.permission_dialog.is_some() {
                    super::mouse_flow::handle_permission_action_mouse(
                        hit_id.as_deref(),
                        &mut self.chat,
                        &mut self.loop_state,
                        mouse,
                    )
                } else {
                    super::mouse_flow::handle_non_permission_mouse(
                        hit_id.as_deref(),
                        &mut self.chat,
                        mouse,
                    )
                };
                if changed {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            Event::User(tag) if tag == bmux_tui_runtime::TERMINAL_COLORS_EVENT => {
                if self.chat.app.refresh_terminal_colors() {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            Event::User(_) => {
                self.deferred
                    .push_back(BcodeRuntimeMessage::Terminal(event));
                super::invalidation::UiInvalidation::None
            }
        };
        damage
    }

    fn refresh_selected_plain_text(&mut self) {
        // The transcript engine owns its selection and exports its logical source text.
        self.selected_plain_text = self
            .transcript_host
            .selected_text()
            .filter(|text| !text.is_empty());
        self.chat
            .app
            .set_transcript_selection_pinned(self.selected_plain_text.is_some());
    }

    fn start_root_skill_action(
        &mut self,
        action: super::effects::SkillActionKind,
        skill_id: bcode_skill_models::SkillId,
        arguments: String,
    ) {
        super::skill_flow::start_skill_action(
            self.settings.launch_working_directory(),
            self.settings.launch_options(),
            &mut self.chat,
            action,
            skill_id,
            arguments,
        );
    }

    fn apply_root_timeline_jump(&mut self, entry: &super::timeline_dialog::TimelineEntry) {
        // The jump is a navigation intent, resident or not (W8).
        let now = self.transcript_host.now();
        let _changed = self.transcript_host.jump_to_sequence(entry.sequence(), now);
    }

    fn synchronize_selection_lifecycle(&mut self) {
        let session = self.chat.app.session_id();
        let config = self.settings.selection();
        let policy = (
            config.auto_copy_on_mouse_release,
            config.clear_after_copy,
            config.clear_delay_ms,
        );
        if self.selection_session != session {
            self.clear_transcript_selection();
            self.committed_selection = bmux_tui::selection::SelectionScene::new();
            self.selection_session = session;
        }
        if self.selection_policy != policy {
            self.cancel_selection_clear();
            self.selection_policy = policy;
        }
    }

    /// The clock the post-copy selection clear is scheduled on: the transcript host clock
    /// (scripted in scenarios).
    fn selection_clock_now(&self) -> Instant {
        self.transcript_host.now()
    }

    fn cancel_selection_clear(&mut self) {
        let id = RootTimer::SelectionClear(self.selection_clear_generation).id();
        if let Some(handle) = &self.runtime_handle {
            let _cancelled = handle.cancel_timer(&id);
        }
        self.scheduled_deadlines.remove(&id);
        self.selection_clear_at = None;
        self.selection_clear_generation = self.selection_clear_generation.wrapping_add(1);
    }

    fn clear_transcript_selection(&mut self) {
        self.cancel_selection_clear();
        let _ = self.transcript_host.clear_selection();
        self.invalidation = self
            .invalidation
            .merge(super::invalidation::UiInvalidation::Paint);
        self.selected_plain_text = None;
        self.chat.app.set_transcript_selection_pinned(false);
        self.presentation_damage = bmux_tui::damage::Damage::Full;
    }

    fn clear_transcript_selection_if_due(
        &mut self,
        now: Instant,
    ) -> super::invalidation::UiInvalidation {
        self.synchronize_selection_lifecycle();
        if self
            .selection_clear_at
            .is_some_and(|deadline| deadline <= now)
        {
            self.clear_transcript_selection();
            super::invalidation::UiInvalidation::Paint
        } else {
            super::invalidation::UiInvalidation::None
        }
    }

    fn copy_transcript_selection(&mut self) {
        self.synchronize_selection_lifecycle();
        self.refresh_selected_plain_text();
        self.cancel_selection_clear();
        let Some(text) = self
            .selected_plain_text
            .as_deref()
            .filter(|text| !text.is_empty())
        else {
            self.chat
                .app
                .set_status("no transcript selection".to_owned());
            return;
        };
        let result = (self.transcript_clipboard)(text);
        self.report_transcript_copy_result(result);
    }

    /// Copy a Markdown destination (`tui.markdown.copyDestination`) through the transcript
    /// clipboard path.
    fn copy_markdown_destination(&mut self, text: &str) {
        match (self.transcript_clipboard)(text) {
            Ok(()) => self
                .chat
                .app
                .set_status("Copied Markdown destination".to_owned()),
            Err(error) => self
                .chat
                .app
                .set_status(format!("Could not copy Markdown destination: {error}")),
        }
    }

    fn report_transcript_copy_result<E: std::fmt::Display>(&mut self, result: Result<(), E>) {
        match result {
            Ok(()) => {
                self.chat
                    .app
                    .set_status("transcript selection copied".to_owned());
                if self.settings.selection().clear_after_copy {
                    let delay = self.settings.selection().clear_delay_ms;
                    if delay == 0 {
                        self.clear_transcript_selection();
                    } else {
                        self.selection_clear_at = self
                            .selection_clock_now()
                            .checked_add(Duration::from_millis(delay));
                    }
                }
            }
            Err(error) => self.chat.app.set_status(format!("copy failed: {error}")),
        }
    }

    fn paste_clipboard_image(&mut self) {
        let working_directory = self.chat.app.working_directory().map_or_else(
            || self.settings.launch_working_directory().to_path_buf(),
            std::path::Path::to_path_buf,
        );
        match super::clipboard_image::save_clipboard_image(
            self.chat.app.session_id(),
            &working_directory,
        ) {
            Ok(artifact) => {
                let text = super::clipboard_image::pasted_image_text(&artifact.model);
                self.chat.app.reset_input_history_navigation();
                self.chat.app.paste_composer_text(&text);
                self.chat.app.wake_cursor();
                self.chat.app.set_status(format!(
                    "Image pasted: {}; source saved in session artifacts",
                    bcode_plugin_sdk::path::display_from_current_dir(&artifact.model)
                ));
            }
            Err(error) => self
                .chat
                .app
                .set_status(format!("image paste failed: {error}")),
        }
    }

    fn apply_root_thinking_dialog(
        &mut self,
        effort: Option<String>,
        summary: Option<String>,
        visible: bool,
        mode: bcode_config::TuiThinkingMode,
    ) {
        self.chat.app.set_reasoning_visible(visible);
        self.chat.app.set_reasoning_display_mode(mode);
        let effort_generation = effort
            .as_ref()
            .filter(|effort| self.chat.app.reasoning_effort() != Some(effort.as_str()))
            .map(|effort| self.chat.app.set_pending_reasoning_effort(effort.clone()));
        if let Some(session_id) = self.chat.attached_session_id() {
            self.chat
                .start_effect(super::effects::TuiEffect::SetSessionReasoning {
                    session_id,
                    effort,
                    summary,
                    effort_generation,
                    status: "reasoning output settings applied".to_owned(),
                });
            self.chat.app.set_status("setting thinking…".to_owned());
        } else {
            self.chat.app.apply_reasoning_selection(effort, summary);
            self.chat.app.set_status(format!(
                "reasoning output settings applied: {}",
                self.chat.app.thinking_label()
            ));
        }
    }

    fn stage_root_submission(&mut self, placement: bcode_ipc::PromptPlacement) {
        let launch_working_directory = self.settings.launch_working_directory().to_path_buf();
        match super::composer_flow::stage_root_submission(
            &launch_working_directory,
            self.settings.launch_options(),
            &mut self.chat,
            placement,
        ) {
            super::composer_flow::RootSubmission::MessageStaged(staged) => {
                if staged {
                    self.draft_autosave.mark_dirty_now();
                }
            }
            super::composer_flow::RootSubmission::SlashCommand(message) => {
                if self.chat.app.managed_execution_session().is_some()
                    && message.split_whitespace().eq(["/auth", "login"])
                {
                    self.chat.app.request_managed_auth();
                    self.exit_requested = true;
                    return;
                }
                if matches!(
                    message.trim(),
                    "/queue-resume"
                        | "/queue-cancel"
                        | "/queue-confirm"
                        | "/queue-dismiss"
                        | "/queue-end-interrupted"
                ) {
                    self.stage_queue_recovery(message.trim());
                    return;
                }
                let working_directory = self
                    .chat
                    .app
                    .working_directory()
                    .unwrap_or_else(|| self.settings.launch_working_directory())
                    .to_path_buf();
                self.chat
                    .replace_effect(super::effects::TuiEffect::ExecuteSlashCommand {
                        session_id: self.chat.attached_session_id(),
                        working_directory,
                        current_agent_id: self.chat.app.current_agent_id().to_owned(),
                        reasoning_display_mode: self.chat.app.reasoning_display_mode(),
                        reasoning_visible: self.chat.app.reasoning_visible(),
                        message,
                    });
                self.chat
                    .app
                    .set_status("running slash command…".to_owned());
            }
        }
    }

    fn stage_queue_recovery(&mut self, command: &str) {
        use bcode_session_view_models::SessionViewAction;
        if command == "/queue-dismiss" {
            self.chat.app.dismiss_queue_confirmation();
            return;
        }
        let Some(session_id) = self.chat.attached_session_id() else {
            return;
        };
        let Some(recovery) = self
            .chat
            .app
            .session_view_snapshot()
            .runtime
            .queue_recovery()
        else {
            self.chat
                .app
                .set_status("No queued message needs recovery".to_owned());
            self.chat.app.dismiss_queue_confirmation();
            return;
        };
        let action = match command {
            "/queue-end-interrupted" if recovery.interrupted_turn_id.is_some() => {
                SessionViewAction::EndInterruptedTurn {
                    session_id,
                    turn_id: recovery
                        .interrupted_turn_id
                        .expect("guarded interrupted identity"),
                }
            }
            "/queue-resume" if recovery.can_resume => SessionViewAction::PrepareSubmissionResume {
                session_id,
                submission_id: recovery.submission_id,
                expected_accepted_sequence: recovery.expected_accepted_sequence,
            },
            "/queue-cancel" if recovery.can_cancel => SessionViewAction::CancelPendingSubmission {
                session_id,
                operation_id: format!(
                    "cancel:{}:{}",
                    recovery.submission_id.0, recovery.expected_accepted_sequence
                ),
                submission_id: recovery.submission_id,
                expected_accepted_sequence: recovery.expected_accepted_sequence,
            },
            "/queue-confirm" if recovery.can_resume => {
                if !self.chat.app.queue_recovery.settings_visible {
                    self.chat.app.set_status(
                        "Enlarge the terminal to review all resume settings before confirming"
                            .to_owned(),
                    );
                    return;
                }
                let Some(confirmation) = self.chat.app.queue_recovery.confirmation.take() else {
                    return;
                };
                if confirmation.submission_id != recovery.submission_id
                    || confirmation.expected_accepted_sequence
                        != recovery.expected_accepted_sequence
                {
                    return;
                }
                SessionViewAction::ResumeSubmission {
                    session_id,
                    operation_id: uuid::Uuid::new_v4().to_string(),
                    confirmation,
                }
            }
            _ => {
                self.chat.app.set_status(recovery.reason);
                return;
            }
        };
        self.chat.app.queue_recovery.generation =
            self.chat.app.queue_recovery.generation.wrapping_add(1);
        let generation = self.chat.app.queue_recovery.generation;
        self.chat
            .start_effect(super::effects::TuiEffect::QueueRecovery {
                generation,
                session_id,
                action: Box::new(action),
            });
    }

    #[allow(clippy::too_many_lines)]
    fn apply_root_command_action(&mut self, request: super::chat_loop::CommandDispatchRequest) {
        let managed = self.chat.app.managed_execution_session().is_some();
        if managed
            && !matches!(
                &request.action,
                bcode_command::CommandAction::Host { route } if managed_host_route(route)
            )
        {
            self.chat.app.set_status(
                "Model and plugin commands are disabled in this managed conversation".into(),
            );
            return;
        }
        let super::chat_loop::CommandDispatchRequest { action, session } = request;
        match action {
            bcode_command::CommandAction::Host { route } => match route.as_str() {
                "session.new" => {
                    super::session_flow::switch_to_draft_session(&mut self.chat);
                    if managed {
                        return;
                    }
                    self.chat
                        .replace_effect(super::effects::TuiEffect::LoadDraftStatus {
                            launch_working_directory: self
                                .settings
                                .launch_working_directory()
                                .to_path_buf(),
                        });
                }
                "session.switch" | "session.rename" | "session.delete" => {
                    self.loop_state.open_session_picker(&mut self.chat);
                }
                "session.search" => self.loop_state.open_session_search(&mut self.chat),
                "turn.cancel" => {
                    super::chat_loop::start_cancel_turn(&mut self.chat, &mut self.loop_state);
                }
                "context.compact" => {
                    if let Some(session_id) = self.chat.attached_session_id() {
                        self.chat
                            .start_effect(super::effects::TuiEffect::CompactContext { session_id });
                        self.chat.app.set_status("compacting context…".to_owned());
                    } else {
                        self.chat.app.set_status("No active session".to_owned());
                    }
                }
                route @ ("theme.select" | "profile.select" | "appearance.select") => {
                    let axis = super::appearance_picker::AppearanceAxis::for_route(route);
                    self.loop_state.open_appearance_picker(&mut self.chat, axis);
                }
                "transcript.inspect"
                | "transcript.cycleDetail"
                | "transcript.makeDefault"
                | "transcript.focusUnits"
                | "tools.expandAll"
                | "tools.collapseAll"
                | "tools.resetDisclosure" => {
                    self.run_transcript_route(&route);
                }
                "streaming.configure" => {
                    self.loop_state.open_streaming_configurator(&mut self.chat);
                }
                "help" => {
                    self.chat.append_durable_presentation_note(
                        "bcode.host",
                        self.settings.keymap().help_markdown(),
                        bcode_command::CommandTextFormat::Markdown,
                    );
                    self.chat.app.set_status("help shown".to_owned());
                }
                route => self
                    .chat
                    .app
                    .set_status(format!("unsupported host command route: {route}")),
            },
            bcode_command::CommandAction::Plugin {
                plugin_id,
                command_id,
            } => {
                let session_id = self.chat.attached_session_id();
                if let Some(refusal) = session.refusal(session_id.is_some()) {
                    self.chat.app.set_status(refusal.message().to_owned());
                    return;
                }
                if let Some(result) = self
                    .chat
                    .app
                    .plugin_presentation()
                    .and_then(|presentation| {
                        presentation.invoke_frontend_command(&plugin_id, &command_id)
                    })
                {
                    self.chat.app.set_status(match result {
                        Ok(()) => "local frontend action accepted".to_owned(),
                        Err(message) => message,
                    });
                    return;
                }
                let working_directory = self
                    .chat
                    .app
                    .working_directory()
                    .unwrap_or_else(|| self.settings.launch_working_directory())
                    .to_path_buf();
                self.chat
                    .start_effect(super::effects::TuiEffect::InvokePluginCommand {
                        plugin_id,
                        command_id,
                        arguments: None,
                        working_directory,
                        session_id,
                    });
                self.chat
                    .app
                    .set_status("running plugin command…".to_owned());
            }
        }
    }

    fn root_interactive_surface_host_key(
        &self,
        stroke: bmux_keyboard::KeyStroke,
    ) -> Option<super::keymap::BmuxAction> {
        let action = self
            .settings
            .keymap()
            .action_for_key(super::keymap::BmuxScope::Chat, stroke)?;
        (matches!(
            action,
            super::keymap::BmuxAction::AppExit
                | super::keymap::BmuxAction::AppInterrupt
                | super::keymap::BmuxAction::InteractionFocusActive
        ) || matches!(
            action,
            super::keymap::BmuxAction::TranscriptPageUp
                | super::keymap::BmuxAction::TranscriptPageDown
                | super::keymap::BmuxAction::TranscriptTop
                | super::keymap::BmuxAction::TranscriptBottom
                | super::keymap::BmuxAction::TranscriptLineUp
                | super::keymap::BmuxAction::TranscriptLineDown
        ))
        .then_some(action)
    }

    fn interactive_surface_resolution_command(
        &mut self,
        interaction_id: String,
        resolution: bcode_session_models::ToolExchangeResolution,
    ) -> bmux_tui_runtime::Command<BcodeRuntimeMessage> {
        if super::managed_flow::resolve_surface(
            &mut self.chat,
            &mut self.loop_state,
            &interaction_id,
            &resolution,
        ) {
            // The managed response (if any) is a separate effect; the surface closes now and
            // reopens only through explicit review, never by automatic replay.
            return bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.managed_surface_detach"),
                async { Some(BcodeRuntimeMessage::InteractiveSurfaceResolved(Ok(true))) },
            );
        }
        if self
            .canonical_interaction
            .as_ref()
            .is_some_and(|(_, exchange)| exchange == &interaction_id)
        {
            return bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.canonical_surface_detach"),
                async { Some(BcodeRuntimeMessage::InteractiveSurfaceResolved(Ok(true))) },
            );
        }
        let client = self.loop_state.foreground_client();
        bmux_tui_runtime::Command::start_if_idle(
            bmux_tui_runtime::CommandKey::new("bcode.interactive_surface_resolution"),
            async move {
                let Some(client) = client else {
                    return Some(BcodeRuntimeMessage::InteractiveSurfaceResolved(Err(
                        bcode_client::ClientError::Protocol(
                            "Managed interactions require native response admission".into(),
                        ),
                    )));
                };
                let result = bcode_session_view::execute_session_view_action(
                    &client,
                    bcode_session_view_models::SessionViewAction::ResolveExchange {
                        interaction_id,
                        resolution,
                    },
                )
                .await
                .and_then(|outcome| match outcome {
                    bcode_session_view_models::SessionViewActionOutcome::InteractionResolved {
                        resolved,
                    } => Ok(resolved),
                    _ => Err(bcode_client::ClientError::UnexpectedResponse),
                });
                Some(BcodeRuntimeMessage::InteractiveSurfaceResolved(result))
            },
        )
    }

    fn handle_session_changed(&mut self) {
        self.canonical_interaction = None;
        self.composer_field.reset();
        self.clear_transcript_selection();
        self.selection_session = self.chat.app.session_id();
        self.committed_selection = bmux_tui::selection::SelectionScene::new();
        self.loop_state
            .session_changed(self.chat.viewing_session_id());
        self.draft_autosave.reset_for_session_change();
    }

    fn collect_runtime_work(
        &mut self,
        handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    ) -> Vec<bmux_tui_runtime::Command<BcodeRuntimeMessage>> {
        let (mut commands, notes) = self.loop_state.take_pending_effects(&mut self.chat, handle);
        commands.extend(self.transcript_visual_commands());
        if let Some(command) = self.collect_child_watch_work(handle) {
            commands.push(command);
        }
        if let Some(command) = self.focused_input_command.take() {
            commands.push(command);
        }
        self.ordered_notes.append(notes);
        for effect in self.ordered_notes.take_ready() {
            commands.push(self.loop_state.ordered_effect_command(effect, handle));
        }
        if let Some(request) = self.loop_state.next_surface_open_request() {
            let keymap = self.settings.keymap().clone();
            let client = self.loop_state.foreground_client();
            let session_id = self.chat.attached_session_id();
            let runtime_handle = handle.clone();
            commands.push(bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.interactive_surface_open"),
                async move {
                    let result = match super::plugin_tui::load_default_runtime_with_static_bundled(
                        &super::static_bundled_plugins(),
                    ) {
                        Ok(runtime)
                            if super::managed_controls::parse_surface_interaction_id(
                                request.interaction_id(),
                            )
                            .is_some() =>
                        {
                            // Managed native questions use the presentation-only surface; the
                            // answer is delivered through the sealed managed handle.
                            super::interactive_surface::InteractiveSurfaceState::open_request(
                                &runtime, &request, &keymap,
                            )
                            .await
                            .map_err(|error| error.to_string())
                        }
                        Ok(runtime) => {
                            if let (Some(session_id), Some(client)) = (session_id, client) {
                                let exchange_id = request.interaction_id().to_owned();
                                let changed = std::sync::Arc::new(move |terminal, error| {
                                    let runtime_handle = runtime_handle.clone();
                                    let exchange_id = exchange_id.clone();
                                    drop(tokio::spawn(async move {
                                        let _ = runtime_handle.send(BcodeRuntimeMessage::NativeDurableChanged {
                                            session_id, exchange_id, terminal, error,
                                        }).await;
                                    }));
                                });
                                super::interactive_surface::InteractiveSurfaceState::open_canonical_request(
                                    &runtime, &request, &keymap, &client, session_id, changed,
                                ).await
                            } else {
                                Err("canonical interaction requires an attached session".to_owned())
                            }
                        }
                        Err(error) => Err(error.to_string()),
                    };
                    Some(BcodeRuntimeMessage::InteractiveSurfaceOpened(result))
                },
            ));
        }
        if let Some(invalidation) = self.loop_state.root_plugin_surface_invalidation() {
            commands.push(bmux_tui_runtime::Command::start_if_idle(
                bmux_tui_runtime::CommandKey::new("bcode.plugin_surface_invalidation"),
                async move {
                    invalidation.wait().await;
                    Some(BcodeRuntimeMessage::PluginSurfaceInvalidated)
                },
            ));
        } else {
            commands.push(bmux_tui_runtime::Command::cancel(
                bmux_tui_runtime::CommandKey::new("bcode.plugin_surface_invalidation"),
            ));
        }
        commands
    }

    fn reconcile_temporal_registry(&mut self) {
        let now = Instant::now();
        self.temporal_registry
            .reconcile(self.chat.app.invalidation_requests(now));
    }

    fn schedule_deadlines(&mut self) {
        self.synchronize_selection_lifecycle();
        self.reconcile_temporal_registry();
        let invalidation_at = self.temporal_registry.next_at();
        let Some(handle) = &self.runtime_handle else {
            return;
        };
        let selection_id = RootTimer::SelectionClear(self.selection_clear_generation).id();
        let obsolete: Vec<_> = self
            .scheduled_deadlines
            .keys()
            .filter(|id| {
                id.as_str().starts_with("bcode.selection_clear.")
                    && (**id != selection_id || self.selection_clear_at.is_none())
            })
            .cloned()
            .collect();
        for id in obsolete {
            let _cancelled = handle.cancel_timer(&id);
            self.scheduled_deadlines.remove(&id);
        }
        let now = Instant::now();
        let deadlines = [
            (RootTimer::Invalidations, invalidation_at),
            (
                RootTimer::ArtifactRetry,
                self.loop_state.next_artifact_retry_at(),
            ),
            (
                RootTimer::StreamingConfigurator,
                self.loop_state.streaming_configurator_deadline(now),
            ),
            (
                RootTimer::StreamingPresentation,
                self.chat.app.next_streaming_presentation_deadline(now),
            ),
            (
                RootTimer::SelectionClear(self.selection_clear_generation),
                self.selection_clear_at,
            ),
            (RootTimer::DraftSave, self.draft_autosave.next_save_at()),
            (
                RootTimer::InteractiveSurfaceRetry,
                self.loop_state.next_interactive_surface_retry_at(),
            ),
            (
                RootTimer::TelemetryFlush,
                self.loop_state.next_telemetry_flush_at(),
            ),
            (RootTimer::ThemeReload, Some(self.theme_reload_at)),
            (
                RootTimer::TranscriptV2,
                match (
                    self.transcript_host.next_deadline(),
                    self.transcript_host.presentation_deadline(),
                ) {
                    (Some(engine), Some(glow)) => Some(engine.min(glow)),
                    (engine, glow) => engine.or(glow),
                },
            ),
        ];
        for (timer, deadline) in deadlines {
            let id = timer.id();
            match deadline {
                Some(deadline) if self.scheduled_deadlines.get(&id) != Some(&deadline) => {
                    handle.schedule_timer(id.clone(), deadline);
                    self.scheduled_deadlines.insert(id, deadline);
                }
                Some(_) => {}
                None => {
                    let _cancelled = handle.cancel_timer(&id);
                    self.scheduled_deadlines.remove(&id);
                }
            }
        }
    }

    fn select_presentation_damage(
        &self,
        invalidation: super::invalidation::UiInvalidation,
        temporal_damage: &[super::app::TemporalDamage],
    ) -> bmux_tui::damage::Damage {
        if invalidation == super::invalidation::UiInvalidation::Paint
            && let Some(layout) = self.committed_layout
        {
            let regions = temporal_damage.iter().filter_map(|damage| match damage {
                super::app::TemporalDamage::Composer => Some(layout.composer()),
                super::app::TemporalDamage::Status => Some(layout.status()),
                super::app::TemporalDamage::Full => None,
            });
            let composer =
                (self.composer_damage == ComposerDamage::Edited).then(|| layout.composer());
            // The transcript repaints its own region alone (F5, D6); it stays damaged until a
            // frame painted it, so later updates cannot drop the region.
            let transcript_v2 = self
                .transcript_host
                .is_invalidated()
                .then(|| super::transcript_v2_host::paint::transcript_region(layout));
            if !temporal_damage.contains(&super::app::TemporalDamage::Full) {
                let damage = bmux_tui::damage::Damage::regions(
                    regions.chain(transcript_v2).chain(composer).chain(
                        self.chat
                            .app
                            .composer_submit
                            .pointer_damage
                            .borrow()
                            .iter()
                            .copied(),
                    ),
                    self.committed_area,
                    bmux_tui::damage::DamagePolicy {
                        max_regions: 64,
                        max_area_percent: 100,
                    },
                );
                if !damage.is_none() {
                    return damage;
                }
            }
        }
        bmux_tui::damage::Damage::Full
    }

    fn desired_child_watch_scope(&self) -> Option<bcode_session_models::SessionActivityWatchScope> {
        use bcode_session_models::{
            SessionActivityQuery, SessionActivityScope, SessionActivityTarget,
            SessionActivityWatchScope,
        };
        let picker = self.loop_state.child_picker_watch_windows();
        let mut scope = self
            .chat
            .attached_session_id()
            .and_then(|_| self.chat.app.child_watch_scope())
            .unwrap_or(SessionActivityWatchScope {
                activity: Vec::new(),
                children: Vec::new(),
            });
        // Keep every selected loaded window. Admission partitions the combined demand;
        // a picker never silently evicts a monitor window.
        for query in picker {
            if !scope.children.contains(&query) {
                scope.children.push(query.clone());
            }
            let target = SessionActivityTarget {
                session_id: query.parent_id,
                query: SessionActivityQuery {
                    scope: SessionActivityScope::DirectChildren,
                    after: query.after,
                    limit: query.limit,
                },
            };
            if !scope.activity.contains(&target) {
                scope.activity.push(target);
            }
        }
        (!scope.children.is_empty()).then_some(scope)
    }

    fn collect_child_watch_work(
        &mut self,
        handle: &bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
    ) -> Option<bmux_tui_runtime::Command<BcodeRuntimeMessage>> {
        let scope = self.desired_child_watch_scope();
        let navigation = scope
            .as_ref()
            .map(|_| self.chat.app.child_watch_navigation());
        let picker_generation = self.loop_state.child_picker_watch_generation();
        if scope == self.child_watch_scope
            && navigation == self.child_watch_navigation
            && picker_generation == self.child_watch_picker_generation
        {
            return None;
        }
        self.child_watch_scope.clone_from(&scope);
        self.child_watch_picker_generation = picker_generation;
        self.child_watch_navigation = navigation;
        self.child_watch_generation = self.child_watch_generation.wrapping_add(1);
        let generation = self.child_watch_generation;
        let key = bmux_tui_runtime::CommandKey::new("bcode.child_activity_watch");
        let Some(scope) = scope else {
            return Some(bmux_tui_runtime::Command::cancel(key));
        };
        let Some(client) = self.loop_state.foreground_client() else {
            return Some(bmux_tui_runtime::Command::cancel(key));
        };
        let handle = handle.clone();
        Some(bmux_tui_runtime::Command::replace(
            key,
            super::effects::run_activity_watches(client, scope, generation, handle),
        ))
    }

    fn handle_timer(
        &mut self,
        timer: &bmux_tui_runtime::TimerId,
    ) -> (
        super::invalidation::UiInvalidation,
        Vec<super::app::TemporalDamage>,
    ) {
        self.scheduled_deadlines.remove(timer);
        let now = Instant::now();
        if timer.as_str().starts_with("bcode.selection_clear.") {
            let damage =
                if *timer == RootTimer::SelectionClear(self.selection_clear_generation).id() {
                    let now = self.selection_clock_now();
                    self.clear_transcript_selection_if_due(now)
                } else {
                    super::invalidation::UiInvalidation::None
                };
            return (damage, Vec::new());
        }
        let invalidation = match timer.as_str() {
            "bcode.invalidations" => {
                let due = self.temporal_registry.take_due(now);
                return self.chat.app.handle_invalidations_with_damage(&due, now);
            }
            "bcode.artifact_retry" => {
                self.loop_state.start_due_artifact_fetches(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.streaming_configurator" => {
                if self.loop_state.advance_streaming_configurator(now) {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            "bcode.streaming_presentation" => {
                // The host shell's view advances; the transcript engine paces the visible
                // transcript on its own timer (`bcode.transcript_v2`).
                let _advanced = self.chat.app.advance_streaming_presentation(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.draft_save" => {
                super::chat_loop::start_draft_save(&mut self.chat, &mut self.draft_autosave);
                super::invalidation::UiInvalidation::None
            }
            "bcode.transcript_v2" => {
                let now = self.transcript_host.now();
                self.transcript_host.advance(now);
                let _glow_frame = self.transcript_host.advance_presentation(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.telemetry_flush" => {
                if let Some(handle) = &self.runtime_handle {
                    self.loop_state.record_runtime_stats(&handle.stats());
                }
                self.loop_state.flush_telemetry_if_due(now);
                super::invalidation::UiInvalidation::None
            }
            "bcode.theme_reload" => {
                self.theme_reload_at = now + Duration::from_millis(750);
                let signature = super::theme::active_theme_input_signature(&self.chat.app);
                if signature == self.theme_input_signature {
                    return (super::invalidation::UiInvalidation::None, Vec::new());
                }
                self.theme_input_signature = signature;
                self.chat.app.invalidate_theme_catalog();
                if let Some(id) = self.chat.app.reload_theme_if_valid().map(str::to_owned) {
                    self.chat.app.set_status(format!("theme {id} reloaded"));
                } else {
                    self.chat.app.cancel_theme_preview();
                    self.loop_state.cancel_appearance_picker(&mut self.chat);
                    self.chat
                        .app
                        .set_status("preview invalidated; restored configured theme".to_owned());
                }
                super::invalidation::UiInvalidation::Full
            }
            _ => super::invalidation::UiInvalidation::None,
        };
        (invalidation, Vec::new())
    }

    async fn settle_plugin_operations(&mut self) -> Result<(), TuiError> {
        for (plugin_id, action) in Box::pin(self.loop_state.settle_root_plugin_effects()).await? {
            // Shutdown observes completions, but must not dispatch new navigation/commands.
            if let bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome } = action {
                self.plugin_surface_result = Some((plugin_id, outcome));
            }
        }
        Ok(())
    }

    pub fn shutdown_owned_work(&mut self) {
        if let Some(presentation) = self.chat.app.plugin_presentation() {
            presentation.shutdown_frontend_commands();
        }
        self.loop_state.abort_all_effects();
        if let Some(event_task) = self.chat.event_task.take() {
            event_task.abort();
        }
    }
}

/// Synchronous root presenter preserving Bcode frame, hit-map, cursor, and image commit ordering.
///
/// The callback is independently higher-ranked over the writer borrow: tying it
/// to `'b` prevents generalizing the presenter inside Send runtime futures.
#[allow(dead_code)]
pub type BcodeRuntimePresenter<'a, 'b, W> =
    bmux_tui_runtime::terminal_presenter::BorrowedTerminalPresenter<
        'a,
        &'b mut W,
        for<'model, 'terminal, 'writer> fn(
            &'model mut BcodeRuntimeModel,
            &'terminal mut bmux_tui::terminal::Terminal<&'writer mut W>,
        )
            -> Result<bmux_tui_runtime::PresentReport, TuiError>,
    >;

/// Compose Bcode's synchronous frame policy with BMUX terminal lifecycle.
#[must_use]
pub fn bcode_runtime_presenter<'a, 'b, W: std::io::Write>(
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
) -> BcodeRuntimePresenter<'a, 'b, W> {
    bmux_tui_runtime::terminal_presenter::BorrowedTerminalPresenter::new(
        terminal,
        present_root_frame::<W>,
    )
}

fn present_root_frame<W: std::io::Write>(
    program: &mut BcodeRuntimeModel,
    terminal: &mut bmux_tui::terminal::Terminal<&mut W>,
) -> Result<bmux_tui_runtime::PresentReport, TuiError> {
    // Async effects temporarily own the plugin, not its visible scene. Do not
    // prepare or commit a replacement frame until it can render again. This also
    // leaves resize damage pending; BMUX will repaint at the new size on return.
    // Other foregrounds still use the normal presentation path.
    if program.loop_state.root_plugin_paint_pending() {
        return Ok(bmux_tui_runtime::PresentReport {
            changed_cells: 0,
            full_repaint: false,
        });
    }
    // BMUX resolves against the prepared scene's focus scope (or the committed
    // scope when no frame is prepared); PointerContext.scope is not consulted
    // by that resolver. Bcode exposes no separate HitId capture owner.
    terminal.set_pointer_context(bmux_tui::terminal::PointerContext {
        scope: None,
        capture: None,
    });
    if program.chat.app.take_terminal_color_query() {
        // Replies arrive asynchronously through terminal input; terminals that
        // do not support the query never answer and nothing waits on them.
        let _ = terminal.write_control(bmux_tui::capabilities::TerminalColorReport::QUERY);
    }
    let depth = super::theme::terminal_color_depth(program.chat.app.tui_config().color_depth);
    if terminal.color_depth() != depth {
        terminal.set_color_depth(depth);
        terminal.reset();
    }
    let (frame_interval, damage) = program.prepare_root_presentation();
    let (draw_stats, layout) = super::chat_loop::draw_chat_frame(
        terminal,
        &mut program.chat,
        &mut program.loop_state,
        Duration::ZERO,
        frame_interval,
        damage,
        program.committed_layout,
        &mut program.transcript_host,
    )?;
    commit_root_presentation(program, terminal, layout);
    Ok(bmux_tui_runtime::PresentReport {
        changed_cells: draw_stats.changed_cells,
        full_repaint: draw_stats.full_repaint,
    })
}

/// Publish only the interaction geometry belonging to successfully flushed output.
fn commit_root_presentation<W: std::io::Write>(
    program: &mut BcodeRuntimeModel,
    terminal: &bmux_tui::terminal::Terminal<&mut W>,
    layout: Option<super::render::FrameLayout>,
) {
    program.synchronize_composer_interaction_with_paint(true);
    program.committed_hits = terminal.hits().clone();
    program.committed_selection = terminal.selection().clone();
    // Plugin visual targets come from the presented frame's retained geometry, and the reading
    // position's plugin content is retained across reflow (E1, E3).
    program.committed_visual_targets = program.transcript_host.committed_visual_targets();
    if let Some(presentation) = program.chat.app.plugin_presentation() {
        program.transcript_host.sync_content_positions(presentation);
    }
    if program.focused_visual.as_ref().is_some_and(|focused| {
        !program
            .committed_visual_targets
            .iter()
            .any(|(_, identity)| identity == focused)
    }) {
        program.focused_visual = None;
    }
    program.committed_area = terminal.area();
    if matches!(
        program.focused_input_owner,
        Some(FocusedInputOwner::Composer)
    ) {
        let area = program.chat.app.composer_content_area();
        program
            .composer_field
            .painted(program.chat.app.composer(), area, area);
    }
    program.committed_layout = layout;
}

#[cfg(test)]
impl BcodeRuntimeModel {
    /// Run one command-palette host route as the palette does after a selection.
    pub(crate) fn run_command_route_for_test(&mut self, route: &str) {
        self.apply_root_command_action(super::chat_loop::CommandDispatchRequest {
            action: bcode_command::CommandAction::Host {
                route: route.to_owned(),
            },
            session: bcode_command::CommandSessionRequirement::Optional,
        });
    }

    /// Run the timeline-dialog selection path for `sequence` and request a full redraw, as the
    /// dialog's update does after a selection.
    pub(crate) fn timeline_jump_for_test(&mut self, sequence: u64) {
        let entry = super::timeline_dialog::TimelineEntry::new(sequence, sequence, "scenario");
        self.apply_root_timeline_jump(&entry);
        self.request_full_redraw_for_test();
    }

    /// Request a full structural redraw, as a timer-driven update with visible changes does.
    pub(crate) fn request_full_redraw_for_test(&mut self) {
        self.invalidation = self
            .invalidation
            .merge(super::invalidation::UiInvalidation::Structural);
        self.presentation_damage = bmux_tui::damage::Damage::Full;
    }

    /// Replace the system clipboard writer used by transcript copying.
    pub(crate) fn set_transcript_clipboard_for_test(
        &mut self,
        copy: fn(&str) -> Result<(), String>,
    ) {
        self.transcript_clipboard = copy;
    }

    /// Wall-clock deadline of the pending post-copy selection clear.
    pub(crate) const fn selection_clear_at_for_test(&self) -> Option<Instant> {
        self.selection_clear_at
    }

    /// Timer the runtime delivers for the current post-copy selection clear.
    pub(crate) fn selection_clear_timer_for_test(&self) -> bmux_tui_runtime::TimerId {
        RootTimer::SelectionClear(self.selection_clear_generation).id()
    }
}

/// Transcript routing: ingest, input, painting and commit of the transcript host.
impl BcodeRuntimeModel {
    /// Whether the transcript holds keyboard focus.
    fn transcript_focus_active(&self) -> bool {
        self.transcript_host.transcript_focus_active()
    }

    /// Route a key or pointer event to the transcript. `None` lets the host shell handle it.
    fn route_transcript_input(
        &mut self,
        event: &Event,
    ) -> Option<super::invalidation::UiInvalidation> {
        use super::transcript_v2_host::input::TranscriptInputOutcome;
        let modal = self.loop_state.modal_foreground()
            || self.loop_state.has_command_palette()
            || self.loop_state.has_slash_palette();
        let host = &mut self.transcript_host;
        let now = host.now();
        let outcome = match event {
            // User-bound `tui.tools.*` keys run their palette route on V2 (FO6).
            Event::Key(stroke) => match host.handle_tools_key(self.settings.keymap(), *stroke, now)
            {
                Some(true) => TranscriptInputOutcome::Redraw,
                Some(false) => TranscriptInputOutcome::Consumed,
                None => host.handle_key(self.settings.keymap(), *stroke, now),
            },
            Event::Mouse(mouse) if !modal => {
                let top_hit = super::mouse_flow::mouse_hit_id(&self.committed_hits, *mouse);
                host.handle_pointer(
                    *mouse,
                    super::transcript_v2_host::input::PointerInput {
                        wheel_rows: self.settings.mouse_scroll_rows(),
                        top_hit: top_hit.as_deref(),
                        scene: Some(&self.committed_selection),
                        autoscroll: self.settings.selection_autoscroll(),
                    },
                    now,
                )
            }
            _ => TranscriptInputOutcome::Ignored,
        };
        let events = host.take_selection_events();
        let damage = host.take_damage();
        if matches!(event, Event::Key(_)) && outcome != TranscriptInputOutcome::Ignored {
            // FO10: a key the transcript consumed is not a tap of a chat multi-tap sequence.
            self.chat.app.clear_pending_key_activation();
        }
        let markdown_copy = host.take_markdown_copy();
        let copied = self.apply_transcript_v2_selection_events(events)
            | markdown_copy.is_some_and(|text| {
                self.copy_markdown_destination(&text);
                true
            });
        outcome.consumed_redraw().map(|redraw| {
            if copied {
                // The copy reports through the status row.
                super::invalidation::UiInvalidation::Structural
            } else if redraw {
                Self::transcript_v2_invalidation(damage)
            } else {
                super::invalidation::UiInvalidation::None
            }
        })
    }

    /// The invalidation of a V2 transcript change of `damage` scope: the transcript region
    /// alone repaints at paint level, everything else structurally (F5, D6).
    const fn transcript_v2_invalidation(
        damage: super::transcript_v2_host::paint::TranscriptDamage,
    ) -> super::invalidation::UiInvalidation {
        use super::transcript_v2_host::paint::TranscriptDamage;
        match damage {
            TranscriptDamage::None => super::invalidation::UiInvalidation::None,
            TranscriptDamage::Transcript => super::invalidation::UiInvalidation::Paint,
            TranscriptDamage::Full => super::invalidation::UiInvalidation::Structural,
        }
    }

    /// Apply transcript selection gestures to the host shell's copy state (C35): a new gesture cancels a pending post-copy clear, the selected text follows the
    /// drag, and a completed drag copies when auto-copy on release is enabled. Returns whether
    /// a copy ran.
    fn apply_transcript_v2_selection_events(
        &mut self,
        events: super::transcript_v2_host::input::SelectionEvents,
    ) -> bool {
        if events.started {
            self.cancel_selection_clear();
            self.selected_plain_text = None;
            self.chat.app.set_transcript_selection_pinned(false);
        }
        if events.changed || events.completed {
            self.refresh_selected_plain_text();
        }
        if events.completed && self.settings.selection().auto_copy_on_mouse_release {
            self.copy_transcript_selection();
            return true;
        }
        false
    }

    /// Run a transcript command-palette route on the transcript host.
    fn run_transcript_route(&mut self, route: &str) {
        let now = self.transcript_host.now();
        let _outcome = self.transcript_host.run_transcript_route(route, now);
    }

    /// FO3: with transcript focus, a paste or a key the transcript does not bind returns
    /// focus to the composer before shell routing, so the composer receives it.
    fn release_transcript_focus_for(&mut self, event: &Event) {
        if self.loop_state.modal_foreground() {
            return;
        }
        let _released = self
            .transcript_host
            .release_focus_for_event(self.settings.keymap(), event);
    }

    /// Ingest a session stream update into the transcript engine ahead of the shared
    /// host-shell path.
    fn ingest_transcript_stream_update(
        &mut self,
        update: &super::history_flow::SessionStreamUpdate,
    ) -> bool {
        let host = &mut self.transcript_host;
        // Decided before the host shell applies the update, which forgets the pending
        // submission it accepts.
        let own_submission = match update {
            super::history_flow::SessionStreamUpdate::Event(event) => match event.as_ref() {
                bcode_ipc::Event::Session(event) => match &event.kind {
                    bcode_session_models::SessionEventKind::UserMessage { text, .. } => {
                        if Some(event.session_id) == host.session()
                            && let Some(pending) = self
                                .chat
                                .app
                                .pending_submission_accepted_by(event.sequence, text)
                        {
                            // LR4: its row stays until the engine shows the message.
                            host.hand_off_submission(pending, event.sequence);
                        }
                        self.chat
                            .app
                            .accepts_own_pending_submission(event.sequence, text)
                    }
                    _ => false,
                },
                _ => false,
            },
            _ => false,
        };
        let now = host.now();
        host.ingest_stream_update(update, own_submission, now)
    }

    /// Apply an effect result owned by the transcript host.
    fn apply_transcript_effect_result(
        &mut self,
        result: TuiEffectResult,
    ) -> super::invalidation::UiInvalidation {
        // Transcript history reads are daemon reads: connectivity follows their outcome.
        let observation = result.daemon_observation();
        let failed = !matches!(
            observation,
            super::effects::DaemonObservation::None | super::effects::DaemonObservation::Success
        );
        self.loop_state.observe_daemon(&mut self.chat, &observation);
        let now = self.transcript_host.now();
        let changed = self.transcript_host.apply_effect_result(result, now);
        if changed || failed {
            super::invalidation::UiInvalidation::Structural
        } else {
            super::invalidation::UiInvalidation::None
        }
    }

    /// End of every update: follow the attached session, bring the render and structure
    /// inputs up to date, start the transcript host's history effects and report whether the
    /// transcript needs repainting.
    fn flush_transcript(&mut self) -> super::invalidation::UiInvalidation {
        let host = &mut self.transcript_host;
        let now = host.now();
        host.note_opening_anchor(
            self.chat
                .attachment
                .opening_session_id()
                .map(|session| (session, self.chat.attachment.opening_anchor_sequence())),
        );
        host.sync_session(self.chat.attached_session_id(), now);
        // The engine's `SessionView` paces live text with the host shell's policy (D13).
        host.sync_streaming_policy(self.chat.app.streaming_presentation_policy(), now);
        let _ = host.sync_render_inputs(&mut self.chat.app, now);
        // Pending submissions and process-local notices are local rows (LR1–LR7).
        host.sync_local_rows(&self.chat.app, now);
        // Transcript tooling reports through the host shell: status notices, and per-tool
        // defaults persisted to user state through the shell's effect (A3).
        let notice = host.take_notice().map(|notice| {
            let text = notice.text(&self.chat.app);
            self.chat.app.set_status(text);
        });
        // Markdown activation reports through the status row.
        host.set_markdown_activate_label(
            &self.chat.app.shortcut_hint("chat", "tui.markdown.activate"),
        );
        let markdown_notice = host.take_markdown_notice().map(|notice| {
            let text = notice.text(&self.chat.app);
            self.chat.app.set_status(text);
        });
        let notice = notice.is_some() || markdown_notice.is_some();
        for rule in host.take_default_rules() {
            self.chat.app.queue_default_rule(rule);
        }
        host.set_visual_workers(|| self.loop_state.markdown_visual_workers());
        let _queued = host.schedule_markdown_visuals(
            transcript_v2_visual_policy(&self.chat.app, &self.loop_state),
            now,
        );
        // Spinner and elapsed labels advance on the host shell's accepted activity frames
        // (F7, D7), never on wall time while drawing.
        host.sync_activity(self.chat.app.spinner_step());
        // Each history effect supersedes earlier work in its lane; superseded requests are
        // no longer awaited by the engine.
        for effect in host.drain_effects() {
            self.chat.replace_effect(effect);
        }
        // A selection the engine dropped (its unit left the window) is no longer copyable.
        let selection_lost =
            self.selected_plain_text.is_some() && host.engine().selection().is_none();
        // The transcript stays invalidated until a frame paints it (regional damage, F5).
        let damage = host.take_damage();
        if selection_lost {
            self.selected_plain_text = None;
            self.chat.app.set_transcript_selection_pinned(false);
        }
        if notice {
            // Tooling notices report through the status row.
            return super::invalidation::UiInvalidation::Structural;
        }
        Self::transcript_v2_invalidation(damage)
    }

    /// Runtime commands for the transcript host's started Markdown visual requests: one keyed
    /// lane per worker kind, so at most one request per lane runs (D2; the host keeps the
    /// pending one).
    fn transcript_visual_commands(
        &mut self,
    ) -> Vec<bmux_tui_runtime::Command<BcodeRuntimeMessage>> {
        self.transcript_host
            .drain_visual_work()
            .into_iter()
            .map(|work| {
                bmux_tui_runtime::Command::replace(
                    bmux_tui_runtime::CommandKey::new(format!(
                        "bcode.transcript_v2.visual.{}",
                        work.lane().name()
                    )),
                    async move {
                        Some(BcodeRuntimeMessage::TranscriptV2Visual(Box::new(
                            work.run().await,
                        )))
                    },
                )
            })
            .collect()
    }

    /// The transcript host (scenario and test observation seam).
    #[cfg(test)]
    pub(crate) fn transcript_host_for_test(&self) -> &super::transcript_v2_host::TranscriptV2Host {
        &self.transcript_host
    }

    /// The transcript host, mutably (scenario clock and observation seam).
    #[cfg(test)]
    pub(crate) fn transcript_host_mut_for_test(
        &mut self,
    ) -> &mut super::transcript_v2_host::TranscriptV2Host {
        &mut self.transcript_host
    }

    /// Record the presented frame (CK3) and queue the history requests the commit issued
    /// (prefetch after the presented position, W5). Returns whether the transcript needs
    /// another frame and whether history effects were queued.
    fn commit_transcript_presentation(&mut self) -> (bool, bool) {
        let host = &mut self.transcript_host;
        host.commit();
        // The committed frame decides which Markdown visuals are visible (and prefetched).
        let now = host.now();
        let visuals_queued = host.schedule_markdown_visuals(
            transcript_v2_visual_policy(&self.chat.app, &self.loop_state),
            now,
        );
        let effects = host.drain_effects();
        let redraw = host.is_invalidated();
        let queued = !effects.is_empty() || visuals_queued;
        for effect in effects {
            self.chat.replace_effect(effect);
        }
        (redraw, queued)
    }
}

impl bmux_tui_runtime::Program for BcodeRuntimeModel {
    type Message = BcodeRuntimeMessage;
    type Error = TuiError;

    fn presentation_committed(
        &mut self,
        _report: bmux_tui_runtime::PresentReport,
    ) -> bmux_tui_runtime::Update<Self::Message> {
        self.invalidation = super::invalidation::UiInvalidation::None;
        self.last_presented_at = Some(Instant::now());
        let (transcript_redraw, transcript_effects) = self.commit_transcript_presentation();
        self.loop_state.mark_presentation_committed();
        let deferred = self
            .loop_state
            .apply_deferred_session_stream_updates(&mut self.chat);
        self.schedule_deadlines();
        let mut update = if deferred {
            self.invalidation = super::invalidation::UiInvalidation::Structural;
            self.presentation_damage = bmux_tui::damage::Damage::Full;
            bmux_tui_runtime::Update::redraw()
        } else if transcript_redraw {
            // Work the commit ran (an accepted Markdown visual, a settled motion) repaints at
            // its own damage scope: the transcript region alone unless it changed more (F5).
            let invalidation = Self::transcript_v2_invalidation(self.transcript_host.take_damage());
            self.invalidation = invalidation;
            self.presentation_damage = self.select_presentation_damage(invalidation, &[]);
            bmux_tui_runtime::Update::redraw()
        } else {
            bmux_tui_runtime::Update::none()
        };
        // Transcript commits queue work here; other effects start at the end of `update`.
        if transcript_effects && let Some(handle) = self.runtime_handle.clone() {
            for command in self.collect_runtime_work(&handle) {
                update = update.with_command(command);
            }
        }
        update
    }

    #[allow(clippy::too_many_lines)]
    fn update(
        &mut self,
        event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
    ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
        let event = match event {
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::ChildWatch {
                generation,
                result,
            }) => {
                bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::ChildWatchProjection {
                    generation,
                    result: result.map(|delivery| delivery.snapshot),
                })
            }
            event => event,
        };
        self.chat
            .app
            .composer_submit
            .pointer_damage
            .borrow_mut()
            .clear();
        self.synchronize_selection_lifecycle();
        self.synchronize_composer_interaction();
        let mut temporal_damage = Vec::new();
        let damage = match event {
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::ComposerSearchCompleted(completion),
            ) => {
                if self.loop_state.composer_interaction.complete(completion) {
                    self.loop_state
                        .composer_interaction
                        .resolve_hints(self.settings.keymap().registry());
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Bootstrap { handle }) => {
                self.runtime_handle = Some(handle.clone());
                let (_replacement_sender, replacement_receiver) = tokio::sync::mpsc::channel(1);
                let mut session_stream =
                    std::mem::replace(&mut self.chat.event_receiver, replacement_receiver);
                let mut artifact_completions = self.loop_state.take_artifact_completion_receiver();
                let session_subscription = bmux_tui_runtime::Subscription::new(
                    bmux_tui_runtime::SubscriptionKey::new("bcode.session_stream"),
                    move |sender| async move {
                        while let Some(update) = session_stream.recv().await {
                            if sender
                                .send(BcodeRuntimeMessage::SessionStream(Box::new(update)))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                let artifact_subscription = bmux_tui_runtime::Subscription::new(
                    bmux_tui_runtime::SubscriptionKey::new("bcode.artifact_completions"),
                    move |sender| async move {
                        while let Some(completion) = artifact_completions.recv().await {
                            if sender
                                .send(BcodeRuntimeMessage::ArtifactFetchCompleted(Box::new(
                                    completion,
                                )))
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                    },
                );
                self.schedule_deadlines();
                let mut update = bmux_tui_runtime::Update::redraw()
                    .with_subscription(session_subscription)
                    .with_subscription(artifact_subscription);
                for command in self.collect_runtime_work(&handle) {
                    update = update.with_command(command);
                }
                return Ok(update);
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::TerminalInputFailed(
                error,
            )) => {
                return Err(std::io::Error::new(
                    error.kind(),
                    format!("reading terminal input: {error}"),
                )
                .into());
            }
            bmux_tui_runtime::RuntimeEvent::Terminal(event)
            | bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Terminal(event)) => {
                if self.chat.app.managed_execution_session().is_some()
                    && let Event::Key(stroke) = &event
                    && self.settings.keymap().action("chat", *stroke) == Some("tui.agent.cycle")
                    && !self.loop_state.modal_foreground()
                {
                    super::managed_flow::cycle_permission_mode(
                        &mut self.chat,
                        &mut self.loop_state,
                    );
                    return Ok(bmux_tui_runtime::Update::redraw());
                }
                if self.chat.app.managed_execution_session().is_some()
                    && let Event::Key(stroke) = &event
                    && self
                        .settings
                        .keymap()
                        .action("chat", *stroke)
                        .is_some_and(managed_blocked_chat_action)
                {
                    self.chat.app.set_status(
                        "Model and unrelated plugin controls are disabled in managed mode".into(),
                    );
                    return Ok(bmux_tui_runtime::Update::redraw());
                }
                if self.return_from_borrowed_chat(&event) {
                    return Ok(bmux_tui_runtime::Update::redraw());
                }
                self.release_transcript_focus_for(&event);
                // Transcript focus mode owns keys ahead of inline interactions and completion.
                let transcript_focus_owns = matches!(event, Event::Key(_))
                    && self.transcript_focus_active()
                    && !self.loop_state.modal_foreground();
                if self.loop_state.has_interactive_surface()
                    && !self.loop_state.modal_foreground()
                    && !transcript_focus_owns
                {
                    if let Event::Key(stroke) = event
                        && !(stroke.key == bmux_keyboard::KeyCode::Escape
                            && self.loop_state.composer_interaction.visible())
                        && let Some(action) = self.root_interactive_surface_host_key(stroke)
                    {
                        match action {
                            super::keymap::BmuxAction::AppInterrupt => {
                                let Some((interaction_id, resolution)) =
                                    self.loop_state.dismiss_interactive_surface()
                                else {
                                    return Ok(bmux_tui_runtime::Update::none());
                                };
                                let command = self.interactive_surface_resolution_command(
                                    interaction_id,
                                    resolution,
                                );
                                return Ok(bmux_tui_runtime::Update::redraw().with_command(command));
                            }
                            super::keymap::BmuxAction::AppExit => self.chat.app.request_exit(),
                            super::keymap::BmuxAction::InteractionFocusActive => {
                                // The transcript reveals its reserved interaction unit (C37).
                                if self.chat.app.tui_config().interactions.placement
                                    == bcode_config::TuiInteractionPlacement::Transcript
                                {
                                    let now = self.transcript_host.now();
                                    let _changed =
                                        self.transcript_host.reveal_inline_interaction(now);
                                }
                            }
                            // The transcript scrolls under the surface (Page Up/Down,
                            // ctrl+up/down, ctrl+home/end).
                            _ => {
                                let changed =
                                    self.route_transcript_input(&event).is_some_and(|damage| {
                                        damage != super::invalidation::UiInvalidation::None
                                    });
                                if changed {
                                    self.invalidation = self
                                        .invalidation
                                        .merge(super::invalidation::UiInvalidation::Structural);
                                    self.presentation_damage = bmux_tui::damage::Damage::Full;
                                }
                            }
                        }
                        return Ok(bmux_tui_runtime::Update::redraw());
                    }
                    if let Event::Mouse(mouse) = event
                        && matches!(
                            mouse.kind,
                            bmux_tui::event::MouseEventKind::ScrollUp
                                | bmux_tui::event::MouseEventKind::ScrollDown
                        )
                    {
                        // The transcript scrolls under the surface.
                        let changed = self.route_transcript_input(&event).map_or_else(
                            || {
                                super::mouse_flow::handle_non_permission_mouse(
                                    None,
                                    &mut self.chat,
                                    mouse,
                                )
                            },
                            |damage| damage != super::invalidation::UiInvalidation::None,
                        );
                        if changed {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Structural);
                            self.presentation_damage = bmux_tui::damage::Damage::Full;
                        }
                        return Ok(bmux_tui_runtime::Update::redraw());
                    }
                }
                let outcome = if transcript_focus_owns {
                    bmux_tui::event::EventOutcome::Ignored
                } else {
                    self.compose_focused_input_event(&event)
                };
                if outcome != bmux_tui::event::EventOutcome::Ignored {
                    if outcome == bmux_tui::event::EventOutcome::Handled {
                        return Ok(bmux_tui_runtime::Update::none());
                    }
                    self.invalidation = self
                        .invalidation
                        .merge(super::invalidation::UiInvalidation::Structural);
                    self.presentation_damage = bmux_tui::damage::Damage::Full;
                    let command = self.focused_input_command.take();
                    let update = bmux_tui_runtime::Update::redraw();
                    return Ok(command.map_or(update, |command| {
                        bmux_tui_runtime::Update::redraw().with_command(command)
                    }));
                }
                let route_to_surface = !self.loop_state.modal_foreground()
                    && !transcript_focus_owns
                    && match event {
                        Event::Mouse(mouse) if self.loop_state.has_interactive_surface() => self
                            .loop_state
                            .active_interactive_surface_geometry()
                            .is_some_and(|geometry| geometry.destination.contains(mouse.position)),
                        _ => self
                            .loop_state
                            .routes_non_mouse_event_to_interactive_surface(
                                self.chat.app.tui_config().interactions,
                            ),
                    };
                if route_to_surface {
                    match self.loop_state.handle_interactive_surface_event(&event) {
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored => {}
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Consumed => {
                            return Ok(bmux_tui_runtime::Update::none());
                        }
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw => {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Paint);
                            let geometry = self
                                .loop_state
                                .active_interactive_surface_geometry()
                                .filter(|geometry| !geometry.destination.is_empty());
                            self.presentation_damage =
                                geometry.map_or(bmux_tui::damage::Damage::Full, |geometry| {
                                    bmux_tui::damage::Damage::regions(
                                        [geometry.destination],
                                        self.committed_area,
                                        bmux_tui::damage::DamagePolicy {
                                            max_regions: 1,
                                            max_area_percent: 100,
                                        },
                                    )
                                });
                            return Ok(bmux_tui_runtime::Update::redraw());
                        }
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Relayout => {
                            self.invalidation = self
                                .invalidation
                                .merge(super::invalidation::UiInvalidation::Items);
                            self.presentation_damage = bmux_tui::damage::Damage::Full;
                            return Ok(bmux_tui_runtime::Update::redraw());
                        }
                        super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
                            resolution,
                        ) => {
                            let Some(interaction_id) = self
                                .loop_state
                                .active_interactive_surface_id()
                                .map(ToOwned::to_owned)
                            else {
                                return Ok(bmux_tui_runtime::Update::none());
                            };
                            let command = self
                                .interactive_surface_resolution_command(interaction_id, resolution);
                            return Ok(bmux_tui_runtime::Update::redraw().with_command(command));
                        }
                    }
                }
                self.route_terminal_event(event)
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::Invalidations(keys)) => {
                self.chat.app.handle_invalidations(&keys, Instant::now())
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::StreamingConfiguratorDue,
            ) => {
                if self
                    .loop_state
                    .advance_streaming_configurator(Instant::now())
                {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::StreamingPresentationDue,
            ) => {
                if self.chat.app.advance_streaming_presentation(Instant::now()) {
                    super::invalidation::UiInvalidation::Paint
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::ChildWatch { .. }) => {
                unreachable!("legacy delivery normalized before dispatch")
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::ChildWatchProjection { generation, result },
            ) => {
                if generation != self.child_watch_generation
                    || self.child_watch_picker_generation
                        != self.loop_state.child_picker_watch_generation()
                    || self.child_watch_navigation
                        != self
                            .desired_child_watch_scope()
                            .as_ref()
                            .map(|_| self.chat.app.child_watch_navigation())
                    || self.child_watch_scope != self.desired_child_watch_scope()
                {
                    super::invalidation::UiInvalidation::None
                } else if let Some(scope) = self.child_watch_scope.clone() {
                    match result {
                        Ok(snapshot) => {
                            self.loop_state
                                .apply_child_watch_to_picker(&scope, &snapshot);
                            self.chat.app.apply_child_watch_snapshot(&scope, snapshot);
                        }
                        Err(error) => {
                            self.loop_state.child_watch_failed(&error);
                            if let Some(session_id) = self.chat.attached_session_id() {
                                self.chat.app.monitor_observation_failed(session_id, &error);
                            }
                        }
                    }
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(update)) => {
                let refresh_permissions = matches!(
                    update.as_ref(),
                    super::history_flow::SessionStreamUpdate::Event(event)
                        if matches!(
                            event.as_ref(),
                            bcode_ipc::Event::Session(session_event)
                                if matches!(
                                    session_event.kind,
                                    bcode_session_models::SessionEventKind::PermissionRequested { .. }
                                        | bcode_session_models::SessionEventKind::PermissionResolved { .. }
                                )
                        )
                );
                match update.as_ref() {
                    super::history_flow::SessionStreamUpdate::ResyncStarted { .. } => {
                        self.chat.app.suspend_managed_input();
                        // Bounded reattachment is observation only: no input or Start replay.
                        self.chat
                            .app
                            .request_managed_exit(super::app::ManagedExitRequest::Reattach);
                    }
                    super::history_flow::SessionStreamUpdate::ManagedOwnershipLost { .. } => {
                        self.chat.app.suspend_managed_input();
                        self.chat
                            .app
                            .request_managed_exit(super::app::ManagedExitRequest::Recover);
                    }
                    _ => {}
                }
                let transcript_v2_changed = self.ingest_transcript_stream_update(&update);
                let changed = self
                    .loop_state
                    .apply_session_stream_update(&mut self.chat, *update)
                    || transcript_v2_changed;
                let changed = super::managed_flow::reconcile_interactions(
                    &mut self.chat,
                    &mut self.loop_state,
                ) || changed;
                if refresh_permissions {
                    self.chat
                        .replace_effect(super::effects::TuiEffect::ListPermissions);
                }
                if changed {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::PluginSurfaceInvalidated,
            ) => {
                let Some(client) = self.loop_state.foreground_client() else {
                    return Ok(bmux_tui_runtime::Update::none());
                };
                let action = self
                    .loop_state
                    .poll_root_plugin_surface(&client, self.fresh_session_settings());
                // Surface/host invalidations and drain completion each request their own wakeup.
                self.loop_state.verify_plugin_operations()?;
                // Do not self-invalidate on every poll: that would spin while an effect is pending.
                match action {
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::Close { outcome }) => {
                        if let Some((plugin_id, outcome)) = self
                            .loop_state
                            .close_root_plugin_surface_with_outcome(outcome)
                        {
                            self.apply_plugin_surface_close(&plugin_id, outcome)
                        } else {
                            super::invalidation::UiInvalidation::Structural
                        }
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSession { session_id }) => {
                        let initial_window_request =
                            super::history_flow::initial_transcript_window_request(
                                super::render::transcript_area_for_frame(
                                    &self.chat.app,
                                    self.committed_area,
                                ),
                            );
                        self.suspend_screen_for_session_navigation();
                        self.loop_state
                            .suspend_root_plugin_surface_for_session(session_id);
                        super::session_flow::start_switch_session(
                            &mut self.chat,
                            session_id,
                            initial_window_request,
                        );
                        self.handle_session_changed();
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::OpenSurface {
                        plugin_id,
                        surface_id,
                        options,
                    }) => {
                        self.chat
                            .replace_effect(super::effects::TuiEffect::OpenPluginSurface {
                                plugin_id,
                                surface_kind: surface_id.clone(),
                                instance_id: format!("workflow-related-{surface_id}"),
                                options,
                                working_directory: self
                                    .settings
                                    .launch_working_directory()
                                    .to_path_buf(),
                                session_id: self.chat.attached_session_id(),
                            });
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::SubscribeWorkflowRuns) => {
                        let (updates, requests) =
                            super::plugin_surface_host::subscribe_workflow_views(
                                self.loop_state.foreground_client().expect(
                                    "ordinary plugin action requires model application access",
                                ),
                                self.loop_state
                                    .root_plugin_surface_invalidation()
                                    .expect("plugin surface invalidation"),
                            );
                        self.loop_state
                            .attach_root_plugin_surface_updates(updates, requests);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::SelectWorkflowRun { run_id }) => {
                        self.loop_state.request_root_workflow_run(run_id);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::UpdateWorkflowCatalogQuery {
                        filter,
                        sort,
                        group,
                        search,
                    }) => {
                        self.loop_state
                            .request_root_workflow_query(filter, sort, group, search);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::LoadMoreWorkflowRuns {
                        cursor,
                    }) => {
                        self.loop_state.request_more_root_workflow_runs(cursor);
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::InvokePluginCommand {
                        plugin_id,
                        command_id,
                        arguments,
                    }) => {
                        self.chat
                            .replace_effect(super::effects::TuiEffect::InvokePluginCommand {
                                plugin_id,
                                command_id,
                                arguments,
                                working_directory: self
                                    .settings
                                    .launch_working_directory()
                                    .to_path_buf(),
                                session_id: self.chat.attached_session_id(),
                            });
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::RunCommand { command }) => {
                        self.chat.app.replace_composer_with(&command);
                        self.loop_state.close_root_plugin_surface();
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::Redraw) => {
                        super::invalidation::UiInvalidation::Structural
                    }
                    Some(bcode_plugin_sdk::tui::PluginTuiAction::None) | None => {
                        super::invalidation::UiInvalidation::None
                    }
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::ArtifactFetchCompleted(completion),
            ) => {
                if self
                    .loop_state
                    .apply_artifact_completion(&self.chat, *completion)
                {
                    super::invalidation::UiInvalidation::Items
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
                result,
            )) if super::transcript_v2_host::ingest::owns_effect_result(&result) => {
                self.apply_transcript_effect_result(*result)
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::TranscriptV2Visual(
                completion,
            )) => {
                let now = self.transcript_host.now();
                let changed = self
                    .transcript_host
                    .complete_markdown_visual(*completion, now);
                if changed {
                    super::invalidation::UiInvalidation::Structural
                } else {
                    super::invalidation::UiInvalidation::None
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::EffectCompleted(
                result,
            )) => {
                let navigation_session = self
                    .loop_state
                    .root_plugin_surface_pending_session_navigation();
                let navigation_result = match result.as_ref() {
                    TuiEffectResult::SessionOpened {
                        session_id, result, ..
                    } if navigation_session == Some(*session_id) => Some((
                        *session_id,
                        result
                            .as_ref()
                            .as_ref()
                            .map(|_| ())
                            .map_err(ToString::to_string),
                    )),
                    _ => None,
                };
                if let TuiEffectResult::AppendPresentationNote { session_id, .. } = result.as_ref()
                {
                    self.ordered_notes.complete(*session_id);
                }
                let previous_frame_interval = self.settings.bmux_runtime_config().frame_interval;
                let previous_session_id = self.chat.viewing_session_id();
                let observation = result.daemon_observation();
                self.loop_state.observe_daemon(&mut self.chat, &observation);
                super::chat_loop::apply_effect_result(
                    &mut self.settings,
                    &mut self.chat,
                    &mut self.draft_autosave,
                    &mut self.loop_state,
                    *result,
                );
                let _presented = super::managed_flow::reconcile_interactions(
                    &mut self.chat,
                    &mut self.loop_state,
                );
                if previous_session_id != self.chat.viewing_session_id() {
                    self.handle_session_changed();
                }
                if let Some((session_id, navigation_result)) = navigation_result {
                    self.finish_plugin_session_attach(session_id, navigation_result);
                } else {
                    let _invalidation = self.synchronize_screen();
                }
                let frame_interval = self.settings.bmux_runtime_config().frame_interval;
                if frame_interval != previous_frame_interval
                    && let Some(handle) = &self.runtime_handle
                {
                    handle.set_frame_interval(frame_interval);
                }
                super::invalidation::UiInvalidation::Structural
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractiveSurfaceOpened(result),
            ) => {
                let result = result.and_then(|surface| {
                    if surface.canonical_session().is_some_and(|session_id| {
                        self.chat.attached_session_id() != Some(session_id)
                    }) {
                        Err("stale canonical interaction session scope".to_owned())
                    } else {
                        Ok(surface)
                    }
                });
                let canonical = result
                    .as_ref()
                    .ok()
                    .filter(|surface| surface.is_canonical())
                    .and_then(|surface| {
                        self.chat
                            .attached_session_id()
                            .map(|session_id| (session_id, surface.interaction_id().to_owned()))
                    });
                match self.loop_state.complete_interactive_surface_open(result) {
                    super::chat_loop::InteractiveSurfaceOpenCompletion::Opened => {
                        self.canonical_interaction = canonical;
                        super::invalidation::UiInvalidation::Structural
                    }
                    super::chat_loop::InteractiveSurfaceOpenCompletion::Stale => {
                        super::invalidation::UiInvalidation::None
                    }
                    super::chat_loop::InteractiveSurfaceOpenCompletion::Failed => {
                        self.chat
                            .app
                            .set_status("Interactive request unavailable; retrying".to_owned());
                        super::invalidation::UiInvalidation::Paint
                    }
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::NativeDurableChanged {
                    session_id,
                    exchange_id,
                    terminal,
                    error,
                },
            ) => {
                if self.chat.attached_session_id() != Some(session_id)
                    || self.loop_state.active_interactive_surface_id() != Some(exchange_id.as_str())
                {
                    super::invalidation::UiInvalidation::None
                } else {
                    if terminal {
                        self.loop_state
                            .complete_interactive_surface_resolution(true);
                    }
                    if let Some(error) = error {
                        self.chat.app.set_status(format!(
                            "canonical interaction: {error:?}; input retained"
                        ));
                    }
                    super::invalidation::UiInvalidation::Structural
                }
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractiveSurfaceResolved(result),
            ) => match result {
                Ok(resolved) => {
                    self.loop_state
                        .complete_interactive_surface_resolution(resolved);
                    self.chat.app.set_status(if resolved {
                        "interactive request resolved".to_owned()
                    } else {
                        "interactive request was already resolved by another client".to_owned()
                    });
                    super::invalidation::UiInvalidation::Structural
                }
                Err(error) => {
                    self.loop_state
                        .complete_interactive_surface_resolution(false);
                    self.chat
                        .app
                        .set_status(format!("Interactive response failed; retry: {error}"));
                    super::invalidation::UiInvalidation::Paint
                }
            },
            bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::DraftSaveDue) => {
                super::chat_loop::start_draft_save(&mut self.chat, &mut self.draft_autosave);
                super::invalidation::UiInvalidation::None
            }
            bmux_tui_runtime::RuntimeEvent::Message(
                BcodeRuntimeMessage::InteractionRetryDue | BcodeRuntimeMessage::TelemetryFlushDue,
            ) => super::invalidation::UiInvalidation::None,
            bmux_tui_runtime::RuntimeEvent::Timer(timer) => {
                let (damage, due_temporal_damage) = self.handle_timer(&timer);
                temporal_damage = due_temporal_damage;
                damage
            }
        };
        self.synchronize_composer_interaction();
        self.loop_state.verify_plugin_operations()?;
        self.invalidation = self.invalidation.merge(damage);
        let transcript_invalidation = self.flush_transcript();
        self.invalidation = self.invalidation.merge(transcript_invalidation);
        let route_invalidation = self.synchronize_screen();
        self.invalidation = self.invalidation.merge(route_invalidation);
        self.draft_autosave.observe(&self.chat, Instant::now());
        if let Some(publisher) = &mut self.resurrection {
            publisher.observe(self.chat.attachment);
        }
        let housekeeping = self
            .loop_state
            .prepare_runtime_work(&self.chat, self.committed_area);
        self.invalidation = self.invalidation.merge(housekeeping);
        self.presentation_damage =
            self.select_presentation_damage(self.invalidation, &temporal_damage);
        let mut update = match self.invalidation {
            super::invalidation::UiInvalidation::None => bmux_tui_runtime::Update::none(),
            super::invalidation::UiInvalidation::Full => bmux_tui_runtime::Update::reset(),
            super::invalidation::UiInvalidation::Paint
            | super::invalidation::UiInvalidation::Items
            | super::invalidation::UiInvalidation::Structural => bmux_tui_runtime::Update::redraw(),
        };
        if self.exit_requested
            || self.chat.app.should_exit()
            || self.chat.app.managed_exit_request().is_some()
            || self.loop_state.managed_open_requested()
            || self.loop_state.execution_picker.is_some()
        {
            update = update.merge(bmux_tui_runtime::Update::exit());
        }
        self.schedule_deadlines();
        if let Some(handle) = self.runtime_handle.clone() {
            for command in self.collect_runtime_work(&handle) {
                update = update.with_command(command);
            }
        }
        Ok(update)
    }
}

/// Markdown visual policy of the V2 transcript: terminal image support and
/// `[tui.markdown] network_images`.
fn transcript_v2_visual_policy(
    app: &super::app::BmuxApp,
    loop_state: &super::chat_loop::ChatLoopState,
) -> super::transcript_v2_host::visuals::VisualPolicy {
    super::transcript_v2_host::visuals::VisualPolicy {
        terminal_images: loop_state.markdown_images_supported(),
        network_images: app.tui_config().markdown.network_images,
    }
}

/// Chat actions that require model/provider/plugin execution and stay disabled in managed
/// conversations. Host navigation (palette, sessions, search, monitor) remains available,
/// and the agent cycle selects native permission modes instead.
fn managed_blocked_chat_action(action: &str) -> bool {
    matches!(
        action,
        "tui.thinking.effort.cycle"
            | "tui.skill.invoke"
            | "tui.skill.activate"
            | "tui.skill.deactivate"
    )
}

/// Host command routes that a managed conversation may run without model execution.
fn managed_host_route(route: &str) -> bool {
    matches!(
        route,
        "session.new"
            | "session.switch"
            | "session.rename"
            | "session.delete"
            | "session.search"
            | "turn.cancel"
            | "theme.select"
            | "profile.select"
            | "streaming.configure"
            | "help"
    )
}

/// Record one live root-runtime statistics snapshot into Bcode telemetry.
pub fn record_runtime_stats(model: &mut BcodeRuntimeModel, stats: &bmux_tui_runtime::RuntimeStats) {
    model.loop_state.record_runtime_stats(stats);
    if model.loop_state.next_telemetry_flush_at().is_some() {
        model.loop_state.flush_telemetry_if_due(Instant::now());
    }
}

/// Consume a successful root-runtime output after recording its final neutral statistics.
#[allow(dead_code)]
pub fn finish_runtime<P>(
    mut output: bmux_tui_runtime::RuntimeOutput<BcodeRuntimeModel, P>,
) -> BcodeRuntimeModel {
    record_runtime_stats(&mut output.program, &output.stats);
    output.program.shutdown_owned_work();
    output.program
}

/// Run a constructed root runtime, map failures, record final statistics, and stop owned work.
#[allow(dead_code)]
pub fn run<W: std::io::Write>(
    runtime: bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'_, '_, W>>,
    handle: bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) -> impl std::future::Future<Output = Result<BcodeRuntimeModel, TuiError>> {
    let runtime = Box::pin(runtime.run());
    async move {
        let mut input = bmux_tui_runtime::TerminalInput::start_with_colors::<BcodeRuntimeModel>(
            handle,
            BcodeRuntimeMessage::TerminalInputFailed,
            super::theme::terminal_colors(),
        );
        let outcome = runtime.await;
        input.shutdown().await;
        Box::pin(finish_runtime_result(outcome)).await
    }
}

type RootRuntimeResult<P> = Result<
    bmux_tui_runtime::RuntimeOutput<BcodeRuntimeModel, P>,
    bmux_tui_runtime::RuntimeError<TuiError, TuiError, BcodeRuntimeModel, P>,
>;

async fn finish_runtime_result<P>(
    outcome: RootRuntimeResult<P>,
) -> Result<BcodeRuntimeModel, TuiError> {
    match outcome {
        Ok(mut output) => {
            let settlement = Box::pin(output.program.settle_plugin_operations()).await;
            let model = finish_runtime(output);
            settlement.map(|()| model)
        }
        Err(
            bmux_tui_runtime::RuntimeError::Program {
                error,
                cleanup_error,
                mut output,
            }
            | bmux_tui_runtime::RuntimeError::Presenter {
                error,
                cleanup_error,
                mut output,
            },
        ) => {
            record_runtime_stats(&mut output.program, &output.stats);
            let settlement = Box::pin(output.program.settle_plugin_operations()).await;
            output.program.shutdown_owned_work();
            Err(runtime_finalization_error(
                error,
                cleanup_error,
                settlement.err(),
            ))
        }
    }
}

/// Preserve settlement precedence without losing runtime or terminal restoration failures.
pub fn runtime_finalization_error(
    runtime_error: TuiError,
    cleanup_error: Option<TuiError>,
    settlement_error: Option<TuiError>,
) -> TuiError {
    let (primary, runtime_error) = match settlement_error {
        Some(error) => (error, Some(runtime_error)),
        None if cleanup_error.is_none() => return runtime_error,
        None => (runtime_error, None),
    };
    std::io::Error::other(RuntimeFinalizationError {
        primary,
        runtime_error,
        cleanup_error,
    })
    .into()
}

#[derive(Debug)]
struct RuntimeFinalizationError {
    primary: TuiError,
    runtime_error: Option<TuiError>,
    cleanup_error: Option<TuiError>,
}

impl std::fmt::Display for RuntimeFinalizationError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{}", self.primary)?;
        if let Some(error) = &self.runtime_error {
            write!(formatter, "; runtime also failed: {error}")?;
        }
        if let Some(error) = &self.cleanup_error {
            write!(formatter, "; terminal cleanup also failed: {error}")?;
        }
        Ok(())
    }
}

impl std::error::Error for RuntimeFinalizationError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.primary)
    }
}

/// Construct the root runtime and its bounded admission handle.
#[allow(dead_code)]
pub fn runtime<'a, 'b, W: std::io::Write>(
    terminal: &'a mut bmux_tui::terminal::Terminal<&'b mut W>,
    model: BcodeRuntimeModel,
) -> (
    bmux_tui_runtime::Runtime<BcodeRuntimeModel, BcodeRuntimePresenter<'a, 'b, W>>,
    bmux_tui_runtime::RuntimeHandle<BcodeRuntimeMessage>,
) {
    let config = model.settings.bmux_runtime_config();
    let (runtime, handle) =
        bmux_tui_runtime::Runtime::new(model, bcode_runtime_presenter(terminal), config);
    let bootstrap = BcodeRuntimeMessage::Bootstrap {
        handle: handle.clone(),
    };
    assert!(
        handle.try_send(bootstrap).is_ok(),
        "new root runtime accepts bootstrap message"
    );
    (runtime, handle)
}

/// Root route whose state, input, rendering, and effects remain owned by Bcode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BcodeRuntimeScreen {
    /// Normal chat/session presentation.
    #[default]
    Chat,
    /// Plugin-contributed terminal surface, including Ralph home flows.
    PluginSurface,
    /// Session catalog and transcript-search picker.
    SessionPicker,
    /// Provider/model selection flow.
    ModelPicker,
    /// Skill selection and invocation flow.
    SkillPicker,
    /// Session working-directory change flow.
    WorkingDirectory,
    /// Worktree creation flow.
    WorktreeCreate,
    /// Ralph loop creation flow.
    RalphStart,
    /// Host/plugin command palette.
    CommandPalette,
    /// Slash completion palette.
    SlashPalette,
    /// Permission interaction overlay.
    Permission,
    /// Reasoning/thinking configuration overlay.
    Thinking,
    /// Timeline navigation overlay.
    Timeline,
    /// Theme selection overlay.
    Theme,
    /// Streaming presentation configurator.
    Streaming,
    /// Provider authentication pool picker.
    AuthPool,
    /// Session-owned plugin interaction surface.
    InteractiveSurface,
}

#[cfg(test)]
#[path = "child_product_tests.rs"]
mod child_product_tests;

#[cfg(test)]
mod tests {
    use super::{BcodeRuntimeAdmissionError, BcodeRuntimeMessage, OrderedPresentationQueue, admit};

    use bcode_command::CommandTextFormat;
    use bcode_session_models::SessionId;
    use std::collections::{BTreeMap, VecDeque};
    use std::convert::Infallible;

    fn assert_runtime_message_is_send<T: Send + 'static>() {}

    #[tokio::test]
    async fn milestone_root_close_reopen_rejects_stale_success_and_failure() {
        use bmux_keyboard::KeyCode;
        let mut model = root_test_model_with_history(bcode_session_models::SessionId::new(), &[]);
        milestone_root_open(&mut model);
        milestone_root_key(&mut model, KeyCode::Char('m'));
        let old = milestone_root_request(&mut model).expect("first page request");
        milestone_root_key(&mut model, KeyCode::Escape);
        milestone_root_open(&mut model);
        milestone_root_key(&mut model, KeyCode::Char('m'));
        let current = milestone_root_request(&mut model).expect("reopened page request");
        assert_ne!(old, current);
        let status = model.chat.app.status().to_owned();
        milestone_root_complete(&mut model, milestone_root_page(old, Some(999)));
        milestone_root_complete(
            &mut model,
            super::super::effects::TuiEffectResult::MilestonesLoaded {
                request: old,
                result: Err(bcode_client::ClientError::UnexpectedResponse),
            },
        );
        assert_eq!(model.chat.app.status(), status);
        // Loading Enter must neither dismiss the dialog nor schedule a source jump.
        milestone_root_key(&mut model, KeyCode::Enter);
        assert!(milestone_root_request(&mut model).is_none());
        milestone_root_complete(&mut model, milestone_root_page(current, Some(42)));
        assert_eq!(model.chat.app.status(), "milestones: more pages available");
        milestone_root_key(&mut model, KeyCode::Char('n'));
        let next = milestone_root_request(&mut model).expect("dialog survived loading Enter");
        drop(model);
        assert_eq!(next.after_sequence, Some(42));
    }

    #[tokio::test]
    async fn milestone_root_rapid_filter_and_page_keys_preserve_current_request() {
        use bmux_keyboard::KeyCode;
        let session_id = bcode_session_models::SessionId::new();
        let mut prompt = root_test_history_event(session_id, 10);
        prompt.kind = bcode_session_models::SessionEventKind::UserMessage {
            client_id: bcode_session_models::ClientId::new(),
            text: "resident prompt".into(),
            admission: bcode_session_models::TurnAdmissionMetadata::default(),
        };
        let mut model = root_test_model_with_history(session_id, std::slice::from_ref(&prompt));
        milestone_root_open(&mut model);
        milestone_root_key(&mut model, KeyCode::Char('m'));
        let first = milestone_root_request(&mut model).expect("first request");
        milestone_root_key(&mut model, KeyCode::Char('p'));
        milestone_root_key(&mut model, KeyCode::Char('n'));
        assert!(milestone_root_request(&mut model).is_none());
        milestone_root_key(&mut model, KeyCode::Char('m'));
        let replacement = milestone_root_request(&mut model).expect("replacement request");
        milestone_root_key(&mut model, KeyCode::Char('n'));
        assert!(milestone_root_request(&mut model).is_none());
        milestone_root_complete(&mut model, milestone_root_page(first, Some(999)));
        milestone_root_complete(&mut model, milestone_root_page(replacement, Some(7)));
        milestone_root_key(&mut model, KeyCode::Char('n'));
        let next = milestone_root_request(&mut model).expect("next request");
        assert_eq!(next.after_sequence, Some(7));
        milestone_root_key(&mut model, KeyCode::Char('n'));
        assert!(milestone_root_request(&mut model).is_none());
        milestone_root_key(&mut model, KeyCode::Char('m'));
        let restarted = milestone_root_request(&mut model).expect("restart first page");
        assert_eq!(restarted.after_sequence, None);
        milestone_root_complete(&mut model, milestone_root_page(next, Some(999)));
        milestone_root_complete(&mut model, milestone_root_page(restarted, None));
        assert_eq!(model.chat.app.status(), "milestones: final page");
        milestone_root_key(&mut model, KeyCode::Char('n'));
        assert!(milestone_root_request(&mut model).is_none());
        milestone_root_key(&mut model, KeyCode::Char('p'));
        milestone_root_key(&mut model, KeyCode::Enter);
        // The selection jumps the transcript to the prompt's canonical source and closes the
        // dialog; no further page is requested.
        assert!(milestone_root_request(&mut model).is_none());
        drop(model);
    }

    fn milestone_root_key(model: &mut super::BcodeRuntimeModel, key: bmux_keyboard::KeyCode) {
        let _invalidation = model.handle_basic_terminal_event(bmux_tui::event::Event::Key(
            bmux_keyboard::KeyStroke::simple(key),
        ));
    }

    fn milestone_root_complete(
        model: &mut super::BcodeRuntimeModel,
        result: super::super::effects::TuiEffectResult,
    ) {
        bmux_tui_runtime::Program::update(
            model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::EffectCompleted(
                Box::new(result),
            )),
        )
        .expect("milestone root completion");
    }

    fn milestone_root_open(model: &mut super::BcodeRuntimeModel) {
        milestone_root_complete(
            model,
            super::super::effects::TuiEffectResult::SlashCommandExecuted {
                message: "/timeline".into(),
                result: Ok(super::super::slash_commands::SlashCommandOutcome::OpenTimeline),
            },
        );
    }

    fn milestone_root_request(
        model: &mut super::BcodeRuntimeModel,
    ) -> Option<super::super::timeline_dialog::MilestonePageRequest> {
        let (effects, notes) = model.chat.pending_effects.drain_runtime();
        assert!(notes.is_empty());
        let mut requests = effects.into_iter().filter_map(|(_, effect)| match effect {
            super::super::effects::TuiEffect::LoadMilestones { request } => Some(request),
            // The transcript host loads its own windows alongside the dialog.
            super::super::effects::TuiEffect::LoadTranscriptV2History { .. } => None,
            _ => panic!("unexpected effect during milestone navigation"),
        });
        let request = requests.next();
        assert!(requests.next().is_none());
        request
    }

    fn milestone_root_page(
        request: super::super::timeline_dialog::MilestonePageRequest,
        next_after_sequence: Option<u64>,
    ) -> super::super::effects::TuiEffectResult {
        super::super::effects::TuiEffectResult::MilestonesLoaded {
            request,
            result: Ok(bcode_session_models::SessionMilestonePage {
                milestones: Vec::new(),
                next_after_sequence,
            }),
        }
    }

    // Failure-only diagnostics preserve the real admission error that public IPC intentionally
    // normalizes. A successful unregistered DB read alone cannot establish server readability.

    fn root_test_history_event(
        session_id: bcode_session_models::SessionId,
        sequence: u64,
    ) -> bcode_session_models::SessionEvent {
        bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: format!("message {sequence}"),
            },
        }
    }

    #[test]
    fn borrowed_stdout_root_runtime_future_is_send() {
        fn assert_send<T: Send>(_: T) {}

        // Compile the same borrowed-writer runtime shape that crosses the CLI's
        // boxed Send future boundary, without starting a runtime or terminal I/O.
        assert_send(async {
            let mut stdout = std::io::stdout();
            let mut terminal = bmux_tui::terminal::Terminal::new(
                &mut stdout,
                bmux_tui::geometry::Rect::new(0, 0, 80, 24),
            );
            let (runtime, handle) = super::runtime(&mut terminal, root_test_model());
            super::run(runtime, handle).await
        });
    }

    struct FlushFailureOutput {
        fail: std::rc::Rc<std::cell::Cell<bool>>,
        bytes: Vec<u8>,
    }

    impl std::io::Write for FlushFailureOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail.get() {
                Err(std::io::Error::other("injected frame flush failure"))
            } else {
                Ok(())
            }
        }
    }

    #[tokio::test]
    async fn root_flush_failure_does_not_publish_candidates() {
        use bmux_tui_runtime::Presenter;

        let mut model = root_test_model();
        let fail = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut output = FlushFailureOutput {
            fail: fail.clone(),
            bytes: Vec::new(),
        };
        let mut terminal = bmux_tui::terminal::Terminal::new(
            &mut output,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        );
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("initial committed frame");
        let committed_area = model.committed_area;
        let committed_body = model
            .committed_layout
            .map(super::super::render::FrameLayout::body);
        let committed_hits = model.committed_hits.clone();
        let committed_selection = model.committed_selection.clone();
        let committed_images = terminal.image_scene().clone();
        model.focused_visual = Some("retain-until-success".to_owned());
        model.chat.app.replace_composer_with("not delivered");
        model.presentation_damage = model.select_presentation_damage(
            super::super::invalidation::UiInvalidation::Paint,
            &[super::super::app::TemporalDamage::Composer],
        );
        fail.set(true);
        assert!(
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .is_err()
        );
        assert_eq!(model.committed_area, committed_area);
        assert_eq!(
            model
                .committed_layout
                .map(super::super::render::FrameLayout::body),
            committed_body
        );
        assert_eq!(model.committed_hits, committed_hits);
        assert_eq!(model.committed_selection, committed_selection);
        assert_eq!(terminal.image_scene(), &committed_images);
        assert_eq!(
            model.focused_visual.as_deref(),
            Some("retain-until-success")
        );
        fail.set(false);
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("recovery frame");
        assert!(model.focused_visual.is_none());
        drop(model);
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn root_image_failures_repair_before_reupload() {
        use bmux_tui::geometry::Rect;
        use bmux_tui::image::{
            ImageContribution, ImageFit, ImageKey, ImageLifecycle, ImagePayload, ImagePixelFormat,
            ImagePlacement,
        };
        use bmux_tui_runtime::Presenter;

        struct ImageSurface(std::sync::Arc<std::sync::atomic::AtomicU8>);
        impl bcode_plugin_sdk::tui::PluginTuiSurface for ImageSurface {
            fn id(&self) -> &'static str {
                "image-failure-fixture"
            }
            fn title(&self) -> &'static str {
                "Image failure fixture"
            }
            fn render(&mut self, area: Rect, frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {
                frame.push_image(ImageContribution::Present(ImagePlacement {
                    key: ImageKey::new("root-failure-image"),
                    payload: ImagePayload::Pixels {
                        bytes: vec![self.0.load(std::sync::atomic::Ordering::Relaxed), 0, 0, 255],
                        width: 1,
                        height: 1,
                        format: ImagePixelFormat::Rgba8,
                    },
                    destination: area,
                    clip: area,
                    lifecycle: ImageLifecycle::Frame,
                    fit: ImageFit::Stretch,
                    allocation: None,
                }));
            }
            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        for partial_output in [false, true] {
            let mut model = root_test_model();
            let mut config = model.chat.app.tui_config().clone();
            config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
            model.chat.app.apply_tui_config(config);
            let pixel = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(1));
            model.loop_state.install_interactive_surface_for_test(
                super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                    "root-image-fixture",
                    Box::new(ImageSurface(pixel.clone())),
                    model.settings.keymap(),
                ),
            );
            let capabilities = bmux_image::HostImageCapabilities {
                kitty_graphics: true,
                ..Default::default()
            };
            model
                .loop_state
                .image_presentation_for_test(capabilities.clone());
            let mut output = ImageFailureOutput::default();
            let mut terminal =
                bmux_tui::terminal::Terminal::new(&mut output, Rect::new(0, 0, 80, 24));
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("initial image frame");
            assert_eq!(terminal.image_scene().placements().len(), 1);
            assert!(String::from_utf8_lossy(&terminal.writer().bytes).contains("a=t"));
            let scene = terminal.image_scene().clone();
            let terminal_hits = terminal.hits().clone();
            let terminal_selection = terminal.selection().clone();
            let hits = model.committed_hits.clone();
            let selection = model.committed_selection.clone();
            let layout = model
                .committed_layout
                .map(super::super::render::FrameLayout::body);
            pixel.store(2, std::sync::atomic::Ordering::Relaxed);
            terminal.writer_mut().bytes.clear();
            terminal.writer_mut().fail_upload = partial_output;
            terminal.writer_mut().fail_flush = !partial_output;
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            assert!(
                super::bcode_runtime_presenter(&mut terminal)
                    .present(&mut model)
                    .is_err()
            );
            assert!(String::from_utf8_lossy(&terminal.writer().bytes).contains("a=t"));
            assert_eq!(terminal.image_scene(), &scene);
            assert_eq!(terminal.hits(), &terminal_hits);
            assert_eq!(terminal.selection(), &terminal_selection);
            assert_eq!(model.committed_hits, hits);
            assert_eq!(model.committed_selection, selection);
            assert_eq!(
                model
                    .committed_layout
                    .map(super::super::render::FrameLayout::body),
                layout
            );

            // Probe a clone: repair debt must survive without consuming the real retry.
            let mut repair = model
                .loop_state
                .image_presentation_for_test(capabilities.clone())
                .clone();
            let mut cleanup = Vec::new();
            repair
                .cleanup(&mut cleanup, &capabilities)
                .expect("repair cleanup probe");
            // The committed image plus the candidate-only host ID must both
            // be repaired; deleting only the old resident is insufficient.
            assert!(String::from_utf8_lossy(&cleanup).matches("d=I").count() >= 2);
            terminal.writer_mut().fail_upload = false;
            terminal.writer_mut().fail_flush = false;
            terminal.writer_mut().bytes.clear();
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("image recovery frame");
            let retry = String::from_utf8_lossy(&terminal.writer().bytes);
            assert!(
                retry.find("d=I").expect("uncertain IDs removed")
                    < retry.find("a=t").expect("image reuploaded")
            );
            assert!(retry.contains("a=p"));
            assert_ne!(terminal.image_scene(), &scene);
            assert_eq!(terminal.image_scene().placements().len(), 1);
            terminal.writer_mut().bytes.clear();
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("warm recovered frame");
            let warm = String::from_utf8_lossy(&terminal.writer().bytes);
            assert!(!warm.contains("a=t"));
            assert!(!warm.contains("d=I"));
            drop(model);
        }
    }

    #[tokio::test]
    async fn status_partial_presentation_matches_full_production_presenter() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial_model = root_test_model();
        let mut full_model = root_test_model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);
        {
            super::bcode_runtime_presenter(&mut partial_terminal)
                .present(&mut partial_model)
                .expect("initial partial frame");
            super::bcode_runtime_presenter(&mut full_terminal)
                .present(&mut full_model)
                .expect("initial full frame");
        }

        for model in [&mut partial_model, &mut full_model] {
            model.chat.app.set_status("timer advanced".to_owned());
            model.invalidation = super::super::invalidation::UiInvalidation::Paint;
        }
        partial_model.presentation_damage = partial_model.select_presentation_damage(
            super::super::invalidation::UiInvalidation::Paint,
            &[super::super::app::TemporalDamage::Status],
        );
        full_model.presentation_damage = bmux_tui::damage::Damage::Full;

        super::bcode_runtime_presenter(&mut partial_terminal)
            .present(&mut partial_model)
            .expect("partial status frame");
        super::bcode_runtime_presenter(&mut full_terminal)
            .present(&mut full_model)
            .expect("full status frame");
        // Keep teardown (which can join workers) out of the paired frame captures.
        drop((partial_model, full_model));

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.image_scene(), full_terminal.image_scene());
    }

    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn modal_foreground_owns_input_and_metadata_across_frames() {
        use bmux_tui_runtime::{Presenter, Program};
        let mut model = root_test_model();
        let surface = question_surface_for_root_test(model.settings.keymap()).await;
        model
            .loop_state
            .install_interactive_surface_for_test(surface);
        model.chat.app.replace_composer_with("preserved draft");
        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("chat");
        let background_targets: Vec<_> = terminal
            .hits()
            .regions()
            .iter()
            .map(|hit| hit.id.clone())
            .collect();
        model.loop_state.open_command_palette(&mut model.chat);
        model.synchronize_screen();
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("modal");
        assert!(
            terminal
                .hits()
                .regions()
                .iter()
                .all(|hit| hit.id.as_str() != "composer")
        );
        assert!(
            terminal
                .hits()
                .regions()
                .iter()
                .all(|hit| { !background_targets.contains(&hit.id) })
        );
        assert_eq!(
            terminal
                .focus()
                .active_scope()
                .map(bmux_tui::hit::HitId::as_str),
            Some("bcode.foreground")
        );
        assert!(terminal.hits().regions().iter().all(|hit| {
            hit.focus_scope
                .as_ref()
                .is_none_or(|scope| Some(scope) == terminal.focus().active_scope())
        }));
        assert_eq!(terminal.selection().fragments(), []);
        assert_eq!(terminal.image_scene().placements().len(), 0);
        let snapshot = terminal.retained_buffer().cloned();
        let cursor = terminal.cursor();
        model
            .update(bmux_tui_runtime::RuntimeEvent::Terminal(
                bmux_tui::event::Event::Paste("must not reach draft".into()),
            ))
            .expect("modal paste");
        assert_eq!(model.chat.app.composer().text(), "preserved draft");
        let frozen_work = model
            .loop_state
            .active_interactive_surface_work_shape_for_test();
        for _ in 0..3 {
            model.presentation_damage = model.select_presentation_damage(
                super::super::invalidation::UiInvalidation::Paint,
                &[super::super::app::TemporalDamage::Composer],
            );
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("modal timer");
            assert_eq!(terminal.retained_buffer(), snapshot.as_ref());
            assert_eq!(terminal.cursor(), cursor);
            assert_eq!(terminal.selection().fragments(), []);
        }
        assert_eq!(
            model
                .loop_state
                .active_interactive_surface_work_shape_for_test(),
            frozen_work
        );
        terminal.reset();
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("reset modal");
        assert_eq!(terminal.retained_buffer(), snapshot.as_ref());
        assert_eq!(terminal.cursor(), cursor);
        let escape = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Escape,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        model
            .update(bmux_tui_runtime::RuntimeEvent::Terminal(escape))
            .expect("close modal");
        assert!(!model.loop_state.has_command_palette());
        assert!(model.loop_state.has_interactive_surface());
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("restored chat");
        assert!(
            terminal
                .hits()
                .regions()
                .iter()
                .any(|hit| hit.id.as_str() == "composer")
        );
        drop(model);
    }

    #[tokio::test]
    async fn modal_resize_and_replacement_reconcile_foreground_scene() {
        use bmux_tui_runtime::Presenter;
        let mut model = root_test_model();
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            &mut bytes,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        );
        model.loop_state.open_command_palette(&mut model.chat);
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("palette");
        model.loop_state.open_session_picker(&mut model.chat);
        assert_eq!(
            model.loop_state.foreground(),
            Some(crate::foreground::Foreground::Sessions)
        );
        model.synchronize_screen();
        terminal.resize(bmux_tui::geometry::Rect::new(0, 0, 100, 30));
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("resized sessions");
        let partial = terminal.retained_buffer().cloned();
        let hits = terminal.hits().clone();
        let semantics = terminal.semantics().clone();
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("reference sessions");
        assert_eq!(terminal.retained_buffer(), partial.as_ref());
        assert_eq!(terminal.hits(), &hits);
        assert_eq!(terminal.semantics(), &semantics);
        assert!(
            terminal
                .hits()
                .regions()
                .iter()
                .all(|hit| hit.id.as_str() != "composer")
        );
        drop(model);
    }

    #[tokio::test]
    async fn sessions_cursor_frames_preserve_foreground_composition() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial = root_test_model();
        let mut full = root_test_model();
        for model in [&mut partial, &mut full] {
            model.loop_state.open_session_picker(&mut model.chat);
        }
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);
        super::bcode_runtime_presenter(&mut partial_terminal)
            .present(&mut partial)
            .expect("initial picker");
        super::bcode_runtime_presenter(&mut full_terminal)
            .present(&mut full)
            .expect("initial reference picker");
        let picker_cursor = partial_terminal.cursor();
        for tick in 1..=4 {
            let now = std::time::Instant::now() + std::time::Duration::from_secs(tick);
            for model in [&mut partial, &mut full] {
                let key = model
                    .chat
                    .app
                    .invalidation_requests(now)
                    .into_iter()
                    .next()
                    .expect("cursor timer")
                    .key;
                model.chat.app.handle_invalidations(&[key], now);
            }
            partial.presentation_damage = partial.select_presentation_damage(
                super::super::invalidation::UiInvalidation::Paint,
                &[super::super::app::TemporalDamage::Composer],
            );
            full.presentation_damage = bmux_tui::damage::Damage::Full;
            super::bcode_runtime_presenter(&mut partial_terminal)
                .present(&mut partial)
                .expect("partial picker");
            super::bcode_runtime_presenter(&mut full_terminal)
                .present(&mut full)
                .expect("full picker");
            assert_eq!(
                partial_terminal.retained_buffer(),
                full_terminal.retained_buffer()
            );
            assert_eq!(partial_terminal.cursor(), picker_cursor);
            assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
            assert_eq!(partial_terminal.semantics(), full_terminal.semantics());
            assert_eq!(partial_terminal.image_scene(), full_terminal.image_scene());
        }
        drop((partial, full));
    }

    #[tokio::test]
    async fn cursor_partial_presentation_matches_full_production_presenter() {
        use bmux_tui_runtime::Presenter;

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut partial_model = root_test_model();
        let mut full_model = root_test_model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);

        {
            let mut presenter = super::bcode_runtime_presenter(&mut partial_terminal);
            presenter
                .present(&mut partial_model)
                .expect("initial partial-side frame presents");
        }
        {
            let mut presenter = super::bcode_runtime_presenter(&mut full_terminal);
            presenter
                .present(&mut full_model)
                .expect("initial full-side frame presents");
        }

        let now = std::time::Instant::now() + std::time::Duration::from_secs(1);
        for model in [&mut partial_model, &mut full_model] {
            let cursor_key = model
                .chat
                .app
                .invalidation_requests(now)
                .into_iter()
                .next()
                .expect("cursor invalidation is always scheduled")
                .key;
            assert_eq!(
                model.chat.app.handle_invalidations(&[cursor_key], now),
                super::super::invalidation::UiInvalidation::Paint
            );
            model.invalidation = super::super::invalidation::UiInvalidation::Paint;
        }
        let temporal_damage = [super::super::app::TemporalDamage::Composer];
        partial_model.presentation_damage = partial_model.select_presentation_damage(
            super::super::invalidation::UiInvalidation::Paint,
            &temporal_damage,
        );
        full_model.presentation_damage = bmux_tui::damage::Damage::Full;

        {
            let mut presenter = super::bcode_runtime_presenter(&mut partial_terminal);
            presenter
                .present(&mut partial_model)
                .expect("cursor partial frame presents");
        }
        {
            let mut presenter = super::bcode_runtime_presenter(&mut full_terminal);
            presenter
                .present(&mut full_model)
                .expect("cursor full frame presents");
        }
        drop((partial_model, full_model));

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.image_scene(), full_terminal.image_scene());
        for point in [
            bmux_tui::geometry::Point::new(0, 0),
            bmux_tui::geometry::Point::new(2, 22),
            bmux_tui::geometry::Point::new(79, 23),
        ] {
            assert_eq!(
                partial_terminal
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned()),
                full_terminal
                    .hits()
                    .hit_test(point)
                    .map(|hit| hit.id().as_str().to_owned())
            );
        }
    }

    #[tokio::test]
    async fn stable_interaction_redraw_matches_full_presentation() {
        use bmux_tui::{geometry::Rect, paint::PaintCx, prelude::Line};
        struct CountingSurface(usize);

        impl bcode_plugin_sdk::tui::PluginTuiSurface for CountingSurface {
            fn id(&self) -> &'static str {
                "counting-test"
            }

            fn title(&self) -> &'static str {
                "Counting test"
            }

            fn render(&mut self, area: Rect, frame: &mut PaintCx<'_, '_>) {
                frame.write_line(
                    bmux_tui::paint::LocalRect::terminal(area),
                    &Line::from(self.0.to_string()),
                );
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if matches!(event, bmux_tui::event::Event::Key(_)) {
                    self.0 = self.0.saturating_add(1);
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        fn model() -> super::BcodeRuntimeModel {
            let mut chat = root_test_chat();
            let mut config = chat.app.tui_config().clone();
            config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
            chat.app.apply_tui_config(config);
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let mut loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            loop_state.install_interactive_surface_for_test(
                super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                    "counting-interaction",
                    Box::new(CountingSurface(0)),
                    settings.keymap(),
                ),
            );
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        }

        let area = Rect::new(0, 0, 80, 24);
        let mut partial = model();
        let mut full = model();
        let mut partial_bytes = Vec::new();
        let mut full_bytes = Vec::new();
        let mut partial_terminal = bmux_tui::terminal::Terminal::new(&mut partial_bytes, area);
        let mut full_terminal = bmux_tui::terminal::Terminal::new(&mut full_bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut partial_terminal),
            &mut partial,
        )
        .expect("initial partial presentation");
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut full_terminal),
            &mut full,
        )
        .expect("initial full presentation");

        let event = || {
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ))
        };
        bmux_tui_runtime::Program::update(&mut partial, event()).expect("partial input");
        bmux_tui_runtime::Program::update(&mut full, event()).expect("full input");
        full.presentation_damage = bmux_tui::damage::Damage::Full;

        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut partial_terminal),
            &mut partial,
        )
        .expect("partial redraw");
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut full_terminal),
            &mut full,
        )
        .expect("full redraw");
        drop((partial, full));

        assert_eq!(
            partial_terminal.retained_buffer(),
            full_terminal.retained_buffer()
        );
        assert_eq!(partial_terminal.cursor(), full_terminal.cursor());
        assert_eq!(partial_terminal.hits(), full_terminal.hits());
    }

    #[tokio::test]
    async fn stable_question_redraw_is_independent_of_unrelated_transcript_length() {
        async fn model(history_items: usize) -> super::BcodeRuntimeModel {
            let session_id = bcode_session_models::SessionId::new();
            let history = (1..=u64::try_from(history_items).unwrap_or(u64::MAX))
                .map(|sequence| bcode_session_models::SessionEvent {
                    schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                    sequence,
                    timestamp_ms: sequence,
                    session_id,
                    provenance: None,
                    kind: bcode_session_models::SessionEventKind::UserMessage {
                        client_id: bcode_session_models::ClientId::new(),
                        text: format!("unrelated transcript message {sequence}"),
                        admission: bcode_session_models::TurnAdmissionMetadata::default(),
                    },
                })
                .collect::<Vec<_>>();
            let mut model = root_test_model_with_history(session_id, &history);
            let mut config = model.chat.app.tui_config().clone();
            config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
            model.chat.app.apply_tui_config(config);
            let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
            model.loop_state.install_interactive_surface_for_test(
                question_surface_with_questions_for_root_test(
                    &keymap,
                    serde_json::json!([{
                        "header": null,
                        "question": "Choose one",
                        "options": [
                            {"label": "One", "value": "one", "description": null},
                            {"label": "Two", "value": "two", "description": "Second choice"}
                        ],
                        "control": "radio",
                        "selection_mode": "single",
                        "custom": false,
                        "custom_mode": "additional",
                        "required": true
                    }]),
                )
                .await,
            );
            model
        }

        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut short = Box::pin(model(0)).await;
        let mut long = Box::pin(model(1_000)).await;
        let mut short_bytes = Vec::new();
        let mut long_bytes = Vec::new();
        let mut short_terminal = bmux_tui::terminal::Terminal::new(&mut short_bytes, area);
        let mut long_terminal = bmux_tui::terminal::Terminal::new(&mut long_bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut short_terminal),
            &mut short,
        )
        .expect("short initial frame");
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut long_terminal),
            &mut long,
        )
        .expect("long initial frame");

        let event = || {
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ))
        };
        bmux_tui_runtime::Program::update(&mut short, event()).expect("short question input");
        bmux_tui_runtime::Program::update(&mut long, event()).expect("long question input");
        assert_eq!(short.presentation_damage, long.presentation_damage);
        drop(short);
        drop(long);
    }

    #[tokio::test]
    async fn stable_interaction_redraw_falls_back_without_committed_geometry() {
        struct RedrawSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RedrawSurface {
            fn id(&self) -> &'static str {
                "redraw-test"
            }

            fn title(&self) -> &'static str {
                "Redraw test"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            }
        }

        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let mut loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            loop_state.install_interactive_surface_for_test(
                super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                    "redraw-interaction",
                    Box::new(RedrawSurface),
                    settings.keymap(),
                ),
            );
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Down,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
        )
        .expect("redraw without committed geometry");

        assert!(model.presentation_damage.is_full());
        drop(model);
    }

    #[derive(Default)]
    struct ImageFailureOutput {
        bytes: Vec<u8>,
        fail_upload: bool,
        fail_flush: bool,
    }

    impl std::io::Write for ImageFailureOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            // Fail after some upload bytes reached the host, not during cell output.
            if self.fail_upload && self.bytes.windows(3).any(|part| part == b"a=t") {
                return Err(std::io::Error::other("injected partial image output"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            if self.fail_flush {
                Err(std::io::Error::other("injected image frame flush"))
            } else {
                Ok(())
            }
        }
    }

    // Keep the failure/recovery matrix and its ordered wire assertions in one scenario.

    /// E1 under V2: the shell plugin's live alternate-screen replay is presented by the V2
    /// engine, its rows are published as visual targets from the presented frame, a
    /// right-button press focuses the visual, the focused visual's keys (`shell.terminal.*`)
    /// reach the adapter, which scrolls its screen, and Escape releases it.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn v2_shell_alternate_screen_takes_pointer_focus_and_terminal_keys() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui_runtime::{Presenter, Program, RuntimeEvent, TimerId};
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model_with_history(session_id, &[]);
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .unwrap();
        model.chat.app.set_plugin_host(std::sync::Arc::new(
            bcode_plugin::PluginHost::load_static_plugins(&selected).unwrap(),
        ));
        model
            .chat
            .app
            .set_effective_keymap(model.settings.keymap().registry().clone());
        let payload = serde_json::json!({"arguments":{"command":"fixture"}, "_bcode_runtime": {
            "live_state_key":"shell-navigation-v2", "columns":80, "rows":24, "streaming":true,
            "output":"\u{1b}[?1049habcdefghijklmnopqrstuvwxyz0123456789"}});
        let registry = model
            .chat
            .app
            .plugin_presentation()
            .unwrap()
            .registry("bcode.shell")
            .unwrap();
        assert!(
            registry
                .visual_projection(
                    "shell-run-request-card",
                    "bcode.tool.request.shell.run",
                    &payload,
                    &bcode_plugin_sdk::tui::PluginTuiVisualRenderContext::new(
                        24,
                        bcode_plugin_sdk::tui::PluginTuiDiffLayout::Unified,
                        None,
                    ),
                )
                .is_some()
        );
        drop(registry);
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "shell-navigation-v2".to_owned(),
                    model_output: "fallback".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(bcode_session_models::ToolInvocationResult::Artifact {
                        artifact: Box::new(bcode_session_models::ToolArtifact {
                            artifact_id: "shell-navigation-v2".to_owned(),
                            producer_plugin_id: "bcode.shell".to_owned(),
                            schema: "bcode.tool.request.shell.run".to_owned(),
                            schema_version: 1,
                            title: None,
                            tool_call_id: Some("shell-navigation-v2".to_owned()),
                            metadata: payload,
                            refs: Vec::new(),
                        }),
                    }),
                    content: Vec::new(),
                },
            },
        };
        let host = super::super::transcript_v2_host::test_fixtures::attached_with_events(
            session_id,
            vec![event],
            std::time::Instant::now(),
        );
        model.transcript_host = Box::new(host);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, Rect::new(0, 0, 24, 40));
        let mut present = |model: &mut super::BcodeRuntimeModel| {
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            let report = super::bcode_runtime_presenter(&mut terminal)
                .present(model)
                .unwrap();
            let _ = model.presentation_committed(report);
            let buffer = terminal.retained_buffer().expect("retained buffer");
            (0..buffer.area().height)
                .map(|row| buffer.row_symbols(row).unwrap_or_default())
                .collect::<Vec<_>>()
                .join("\n")
        };
        let update = |model: &mut super::BcodeRuntimeModel, event| {
            Program::update(model, event).expect("update");
        };
        // The end-of-update sync routes the plugin visual; two frames settle the geometry the
        // adapter renders at.
        update(
            &mut model,
            RuntimeEvent::Timer(TimerId::new("bcode.transcript_v2")),
        );
        let _ = present(&mut model);
        update(
            &mut model,
            RuntimeEvent::Timer(TimerId::new("bcode.transcript_v2")),
        );
        let before = present(&mut model);
        assert!(!before.contains("fallback"), "{before}");
        let (target, identity) = model
            .committed_visual_targets
            .iter()
            .find(|(_, identity)| identity.contains(":terminal:"))
            .cloned()
            .unwrap_or_else(|| {
                panic!(
                    "visible shell terminal target: {:?}\n{before}",
                    model.committed_visual_targets
                )
            });
        assert!(target.height >= 1 && target.right() <= 24);
        update(
            &mut model,
            RuntimeEvent::Terminal(Event::Mouse(MouseEvent::new(
                MouseEventKind::Down(MouseButton::Right),
                Point::new(target.x, target.y),
            ))),
        );
        assert_eq!(model.focused_visual.as_deref(), Some(identity.as_str()));
        update(
            &mut model,
            RuntimeEvent::Terminal(Event::Key(bmux_keyboard::KeyStroke {
                key: bmux_keyboard::KeyCode::Right,
                modifiers: bmux_keyboard::Modifiers::default(),
            })),
        );
        assert_eq!(model.focused_visual.as_deref(), Some(identity.as_str()));
        let after = present(&mut model);
        assert_ne!(before, after, "the adapter scrolled its terminal screen");
        update(
            &mut model,
            RuntimeEvent::Terminal(Event::Key(bmux_keyboard::KeyStroke {
                key: bmux_keyboard::KeyCode::Escape,
                modifiers: bmux_keyboard::Modifiers::default(),
            })),
        );
        assert_eq!(model.focused_visual, None, "Escape releases the visual");
        drop(model);
    }

    /// E1 under V2: a running request of a plugin-owned tool renders through the declaring
    /// plugin's request adapter (`shell.run` routes to its request card), not the
    /// canonical fallback, and the adapter renders at the unit's body width so its rows are
    /// never clipped by the tool chrome.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn v2_running_plugin_tool_request_routes_its_request_adapter_at_the_body_width() {
        use bmux_tui::geometry::Rect;
        use bmux_tui_runtime::{Presenter, Program, RuntimeEvent, TimerId};
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model_with_history(session_id, &[]);
        let bundled = [bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
            bcode_shell_plugin::static_plugin(),
        )];
        let selected = bcode_plugin::filter_selected_static_plugins(
            &bundled,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .unwrap();
        model.chat.app.set_plugin_host(std::sync::Arc::new(
            bcode_plugin::PluginHost::load_static_plugins(&selected).unwrap(),
        ));
        model
            .chat
            .app
            .set_effective_keymap(model.settings.keymap().registry().clone());
        let requested = |sequence, call: &str, producer: Option<&str>| {
            super::super::transcript_v2_host::test_fixtures::event(
                session_id,
                sequence,
                bcode_session_models::SessionEventKind::ToolCallRequested {
                    tool_call_id: call.to_owned(),
                    producer_plugin_id: producer.map(str::to_owned),
                    tool_name: "shell.run".to_owned(),
                    arguments_json: serde_json::json!({ "command": "echo running" }).to_string(),
                    working_directory: None,
                },
            )
        };
        let host = super::super::transcript_v2_host::test_fixtures::attached_with_events(
            session_id,
            vec![
                requested(1, "call-owned", Some("bcode.shell")),
                // Another producer's tool of the same name keeps the generic presentation.
                requested(2, "call-foreign", Some("example.harness")),
            ],
            std::time::Instant::now(),
        );
        model.transcript_host = Box::new(host);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, Rect::new(0, 0, 40, 30));
        for _ in 0..2 {
            Program::update(
                &mut model,
                RuntimeEvent::Timer(TimerId::new("bcode.transcript_v2")),
            )
            .expect("update");
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            let report = super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .unwrap();
            let _ = model.presentation_committed(report);
        }
        let host = model.transcript_host_for_test();
        let engine = host.engine();
        let items = &engine.session_view().snapshot().transcript.items;
        let item = |call: &str| {
            items
                .iter()
                .find(|item| {
                    matches!(
                        &item.kind,
                        bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                            if tool.tool_call_id == call
                    )
                })
                .unwrap_or_else(|| panic!("{call} item"))
        };
        let owned = item("call-owned");
        let visual = engine
            .render_config()
            .plugin_visuals
            .accepted(&owned.id)
            .expect("the running request routes its plugin request adapter");
        assert!(
            visual.adapter.starts_with("bcode.shell:"),
            "{}",
            visual.adapter
        );
        assert!(
            engine
                .render_config()
                .plugin_visuals
                .accepted(&item("call-foreign").id)
                .is_none(),
            "a foreign producer's tool keeps the generic presentation"
        );
        let structure =
            engine
                .structure()
                .unit_presentation(&bcode_tui_transcript::units::UnitKey::Item(
                    owned.id.clone(),
                ));
        let body = bcode_tui_transcript::layout::producers::tool::body_width(
            owned,
            structure,
            &engine.render_config().theme,
            engine.geometry().width,
        );
        let widest = visual
            .rows
            .iter()
            .map(|row| u64::try_from(row.width()).unwrap_or(u64::MAX))
            .max()
            .unwrap_or_default();
        assert!(
            widest <= body,
            "adapter rows fit the body ({widest} > {body})"
        );
        // Adapters render key hints from the effective keymap: replacing it routes again.
        let owned_id = owned.id.clone();
        let revision = visual.revision;
        model
            .chat
            .app
            .set_effective_keymap(bcode_plugin_sdk::tui::keymap::KeymapRegistry::default());
        Program::update(
            &mut model,
            RuntimeEvent::Timer(TimerId::new("bcode.transcript_v2")),
        )
        .expect("update");
        let rerouted = model
            .transcript_host_for_test()
            .engine()
            .render_config()
            .plugin_visuals
            .accepted(&owned_id)
            .map(|visual| visual.revision);
        assert!(
            rerouted.is_some_and(|rerouted| rerouted != revision),
            "a keymap replacement re-routes the adapter ({rerouted:?})"
        );
        drop(model);
    }

    /// X9 through the root update path (FO2, FO3, FO10): `alt+u` enters V2 transcript focus
    /// ahead of the shell, a focus key stays with the transcript, an unbound key returns focus
    /// and reaches the composer, and Escape consumed by the transcript is not a tap of the
    /// chat double-Escape sequence.
    #[tokio::test]
    async fn v2_focus_keys_route_ahead_of_the_shell_and_unbound_keys_reach_the_composer() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model_with_history(session_id, &[]);
        let host = super::super::transcript_v2_host::test_fixtures::attached_to(
            session_id,
            3,
            std::time::Instant::now(),
        );
        model.transcript_host = Box::new(host);
        let key = |model: &mut super::BcodeRuntimeModel, key: &str| {
            bmux_tui_runtime::Program::update(
                model,
                bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                    bmux_keyboard::parse_key_stroke(key).expect("key"),
                )),
            )
            .expect("update");
        };
        let focused = |model: &super::BcodeRuntimeModel| {
            model.transcript_host_for_test().transcript_focus_active()
        };

        key(&mut model, "alt+u");
        assert!(focused(&model), "alt+u enters transcript focus");
        key(&mut model, "k");
        assert!(focused(&model));
        assert_eq!(
            model.chat.app.composer().text(),
            "",
            "focus keys stay in the transcript"
        );
        key(&mut model, "x");
        assert!(
            !focused(&model),
            "an unbound key returns focus to the composer"
        );
        assert_eq!(model.chat.app.composer().text(), "x", "and reaches it");

        // FO10: a composer Escape starts the double-Escape sequence; entering transcript focus
        // and the Escape that leaves it are not taps of it, so the next composer Escape starts
        // a new sequence instead of interrupting.
        key(&mut model, "escape");
        assert_eq!(model.chat.app.status(), "repeat escape to cancel");
        key(&mut model, "alt+u");
        key(&mut model, "escape");
        assert!(!focused(&model));
        model.chat.app.set_status("sentinel".to_owned());
        key(&mut model, "escape");
        assert_eq!(
            model.chat.app.status(),
            "repeat escape to cancel",
            "a new sequence started"
        );
        drop(model);
    }

    /// FO2/FO3 through the command palette: "Focus transcript units" toggles transcript focus,
    /// and every transcript tooling route runs on the transcript host.
    #[tokio::test]
    async fn v2_palette_transcript_routes_reach_the_v2_host_only() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model_with_history(session_id, &[]);
        let host = super::super::transcript_v2_host::test_fixtures::attached_to(
            session_id,
            3,
            std::time::Instant::now(),
        );
        model.transcript_host = Box::new(host);
        let route = |model: &mut super::BcodeRuntimeModel, route: &str| {
            model.apply_root_command_action(super::super::chat_loop::CommandDispatchRequest {
                action: bcode_command::CommandAction::Host {
                    route: route.to_owned(),
                },
                session: bcode_command::CommandSessionRequirement::Optional,
            });
        };
        let focused = |model: &super::BcodeRuntimeModel| {
            model.transcript_host_for_test().transcript_focus_active()
        };

        route(&mut model, "transcript.focusUnits");
        assert!(focused(&model), "the palette enters V2 transcript focus");
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::parse_key_stroke("x").expect("key"),
            )),
        )
        .expect("update");
        assert!(!focused(&model), "an unbound key leaves V2 focus");
        assert_eq!(
            model.chat.app.composer().text(),
            "x",
            "and reaches the composer"
        );

        route(&mut model, "transcript.focusUnits");
        route(&mut model, "transcript.focusUnits");
        assert!(
            !focused(&model),
            "the palette action toggles focus off again"
        );

        // Every transcript tooling route runs on the transcript host.
        route(&mut model, "transcript.inspect");
        let host = model.transcript_host_for_test();
        assert!(host.inspector_open(), "the palette opens the V2 inspector");
        assert!(host.transcript_focus_active(), "entering focus on the way");
        for route_id in [
            "tools.expandAll",
            "tools.collapseAll",
            "tools.resetDisclosure",
            "transcript.cycleDetail",
            "transcript.makeDefault",
        ] {
            route(&mut model, route_id);
            assert!(
                !model.chat.app.status().contains("not available"),
                "{route_id}: {}",
                model.chat.app.status()
            );
        }
        drop(model);
    }

    /// With transcript placement configured, the interactive surface is placed from the
    /// engine's reserved interaction unit: without a resident interaction unit it has no
    /// visible rows.
    #[tokio::test]
    async fn v2_places_the_interactive_surface_from_the_v2_transcript() {
        use bmux_tui_runtime::Presenter;

        struct Surface;
        impl bcode_plugin_sdk::tui::PluginTuiSurface for Surface {
            fn id(&self) -> &'static str {
                "docked"
            }
            fn title(&self) -> &'static str {
                "docked"
            }
            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }
            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model_with_history(session_id, &[]);
        assert_eq!(
            model.chat.app.tui_config().interactions.placement,
            bcode_config::TuiInteractionPlacement::Transcript
        );
        model.loop_state.install_interactive_surface_for_test(
            super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                "interaction",
                Box::new(Surface),
                &super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
            ),
        );
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            &mut bytes,
            bmux_tui::geometry::Rect::new(0, 0, 40, 16),
        );
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("present");
        let geometry = model
            .loop_state
            .active_interactive_surface_geometry()
            .expect("surface geometry");
        assert_eq!(
            geometry.placement,
            super::super::chat_loop::InteractiveSurfacePlacement::Transcript
        );
        assert!(
            geometry.destination.is_empty(),
            "no resident interaction unit, no rows: {geometry:?}"
        );
        drop(model);
    }

    /// The transcript region is painted by the transcript host from the windows it loads
    /// itself, never from the host shell's attach replay; the shell still paints.
    #[tokio::test]
    async fn v2_engine_owns_the_presented_transcript_region() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "host replay only".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }];
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let screen = |model: &mut super::BcodeRuntimeModel| {
            let mut bytes = Vec::new();
            let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
            let report = super::bcode_runtime_presenter(&mut terminal)
                .present(model)
                .expect("frame presents");
            let _update = bmux_tui_runtime::Program::presentation_committed(model, report);
            let buffer = terminal.retained_buffer().expect("retained frame");
            (0..area.height)
                .map(|y| {
                    (0..area.width)
                        .filter_map(|x| buffer.get(bmux_tui::geometry::Point::new(x, y)))
                        .map(|cell| cell.symbol.as_str())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        };

        let mut v2 = root_test_model_with_history(session_id, &history);
        let painted = screen(&mut v2);
        // The host shell's attach replay is not transcript content: the engine presents only
        // the windows it loads itself.
        assert!(!painted.contains("host replay only"), "{painted}");
        assert_eq!(v2.committed_selection.fragments().len(), 0);
        let body = v2.committed_layout.expect("committed layout").body;
        let host = v2.transcript_host_for_test();
        let geometry = host.engine().geometry();
        assert_eq!(
            (geometry.width, geometry.height),
            (u64::from(body.width), u64::from(body.height))
        );
        assert_eq!(
            host.engine().committed_frame().map(|frame| frame.0),
            Some(1)
        );
        drop(v2);
    }

    /// F5: transcript work the commit itself runs (accepting a Markdown visual, a render
    /// configuration flush) repaints the transcript region alone, not the whole screen.
    #[tokio::test]
    async fn v2_work_run_at_commit_repaints_only_the_transcript_region() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let mut model = root_test_model_with_history(session_id, &[]);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        let report = super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("frame presents");
        let _update = bmux_tui_runtime::Program::presentation_committed(&mut model, report);
        let report = super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("frame presents");
        // A unit-scoped render change flushed by the commit's visual scheduling.
        assert!(
            model
                .transcript_host_mut_for_test()
                .set_markdown_details("transcript:item:x:details:0".to_owned(), true)
        );
        let _update = bmux_tui_runtime::Program::presentation_committed(&mut model, report);
        let layout = model.committed_layout.expect("committed layout");
        let region = super::super::transcript_v2_host::paint::transcript_region(layout);
        assert!(
            !model.presentation_damage.is_full(),
            "{:?}",
            model.presentation_damage
        );
        assert_eq!(
            model.presentation_damage.retained_regions(),
            [region],
            "{:?}",
            model.presentation_damage
        );
        drop(model);
    }

    #[tokio::test]
    async fn v2_attach_loads_its_own_tail_window_and_presents_it() {
        use bmux_tui_runtime::Presenter;

        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "loaded by the v2 window".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        }];
        let area = bmux_tui::geometry::Rect::new(0, 0, 40, 12);
        let mut model = root_test_model_with_history(session_id, &history);
        let _ = model.flush_transcript();
        let loads = model.chat.pending_effects.queued_transcript_v2_history();
        let [load] = loads.as_slice() else {
            panic!("one attach request, got {}", loads.len());
        };
        assert_eq!(load.session_id, session_id);
        assert!(matches!(
            load.request,
            bcode_tui_transcript::engine::HostRequest::Window(issued)
                if issued.request == bcode_tui_transcript::window::WindowRequest::Tail
        ));
        let daemon_page = bcode_session_models::SessionHistoryPage {
            session_id,
            events: history.to_vec(),
            compatibility_issues: Vec::new(),
            next_cursor: None,
            has_more: false,
        };
        let result = super::super::transcript_v2_host::effects::load_history(*load, |_| {
            std::future::ready(Ok(daemon_page.clone()))
        })
        .await;
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::EffectCompleted(
                Box::new(
                    super::super::effects::TuiEffectResult::TranscriptV2HistoryLoaded {
                        load: *load,
                        result,
                    },
                ),
            )),
        )
        .expect("V2 history completion");
        let host = model.transcript_host_for_test();
        assert_eq!(host.engine().window().last_sequence, Some(1));
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        let report = super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("frame presents");
        let _update = bmux_tui_runtime::Program::presentation_committed(&mut model, report);
        drop(model);
        let buffer = terminal.retained_buffer().expect("retained frame");
        let painted = (0..area.height)
            .map(|y| {
                (0..area.width)
                    .filter_map(|x| buffer.get(bmux_tui::geometry::Point::new(x, y)))
                    .map(|cell| cell.symbol.as_str())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(painted.contains("loaded by the v2 window"), "{painted}");
    }

    #[tokio::test]
    async fn active_transcript_selection_pins_resident_history() {
        let mut model = root_test_model();
        assert!(
            model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );

        model.chat.app.set_transcript_selection_pinned(true);

        assert!(
            !model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );
        model.chat.app.set_transcript_selection_pinned(false);
        assert!(
            model
                .chat
                .app
                .can_trim_resident_transcript_window_for_test()
        );
    }

    #[tokio::test]
    async fn clipboard_failure_reports_user_visible_status() {
        let mut model = root_test_model();

        model.report_transcript_copy_result(Err("clipboard unavailable"));

        assert_eq!(
            model.chat.app.status(),
            "copy failed: clipboard unavailable"
        );
        drop(model);
    }

    #[test]
    fn clear_selection_is_bindable_without_a_default_binding() {
        use super::super::keymap::{BmuxAction, BmuxKeyMap, BmuxScope};
        let mut config = bcode_config::TuiConfig::default();
        let key = bmux_keyboard::parse_key_stroke("ctrl+shift+x").expect("key");
        assert_eq!(
            BmuxKeyMap::from_config(&config).action_for_key(BmuxScope::Chat, key),
            None
        );
        config.keybindings.chat.insert(
            "ctrl+shift+x".to_owned(),
            "tui.transcript.clearSelection".to_owned(),
        );
        assert_eq!(
            BmuxKeyMap::from_config(&config).action_for_key(BmuxScope::Chat, key),
            Some(BmuxAction::TranscriptClearSelection)
        );
    }

    #[test]
    fn default_ctrl_shift_c_copies_transcript_selection() {
        let keymap =
            super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default());
        let stroke = bmux_keyboard::parse_key_stroke("ctrl+shift+c").expect("valid key");

        assert_eq!(
            keymap.action_for_key(super::super::keymap::BmuxScope::Chat, stroke),
            Some(super::super::keymap::BmuxAction::TranscriptCopySelection)
        );
    }

    #[test]
    fn root_message_contract_is_runtime_admissible() {
        assert_runtime_message_is_send::<BcodeRuntimeMessage>();
    }

    #[test]
    fn root_runtime_and_presenter_types_compose() {
        fn assert_runtime<P, R>()
        where
            P: bmux_tui_runtime::Program<Message = BcodeRuntimeMessage>,
            R: bmux_tui_runtime::Presenter<P>,
        {
        }

        assert_runtime::<
            super::BcodeRuntimeModel,
            super::BcodeRuntimePresenter<'static, 'static, Vec<u8>>,
        >();
    }

    fn note(session_id: SessionId, note_id: &str) -> super::super::effects::TuiEffect {
        super::super::effects::TuiEffect::AppendPresentationNote {
            session_id,
            source_id: "test".to_owned(),
            note_id: note_id.to_owned(),
            text: note_id.to_owned(),
            format: CommandTextFormat::PlainText,
        }
    }

    struct StandaloneFocusedOwnerFixture {
        model: super::BcodeRuntimeModel,
        field: std::sync::Arc<std::sync::Mutex<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot>>,
        submits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
        editor_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }

    fn standalone_focused_owner_fixture() -> StandaloneFocusedOwnerFixture {
        standalone_focused_owner_fixture_with_outcome(bmux_tui::event::EventOutcome::Ignored)
    }

    fn standalone_focused_field()
    -> std::sync::Arc<std::sync::Mutex<bcode_plugin_sdk::tui::FocusedTextFieldSnapshot>> {
        use bcode_plugin_sdk::tui::FocusedTextFieldSnapshot;
        std::sync::Arc::new(std::sync::Mutex::new(FocusedTextFieldSnapshot {
            identity: bcode_plugin_sdk::tui::FocusedTextFieldIdentity {
                owner_id: "standalone".into(),
                field_id: "input".into(),
                lifetime: 1,
            },
            generation: 0,
            document: bcode_plugin_sdk::composer::ComposerDocument {
                document_id: "standalone-input".into(),
                revision: 0,
                text: "@topic".into(),
                cursor: 6,
                selection: None,
            },
            eligible: true,
            content_rect: bmux_tui::geometry::Rect::new(0, 4, 40, 1),
            anchor: bmux_tui::geometry::Rect::new(6, 4, 1, 1),
        }))
    }

    fn standalone_focused_owner_fixture_with_outcome(
        editor_outcome: bmux_tui::event::EventOutcome,
    ) -> StandaloneFocusedOwnerFixture {
        use bcode_plugin_sdk::tui::{
            FocusedTextFieldEdit, FocusedTextFieldSnapshot, PluginTuiAction, PluginTuiEventResult,
        };
        use bmux_tui::event::{Event, EventOutcome};
        struct Owner {
            field: std::sync::Arc<std::sync::Mutex<FocusedTextFieldSnapshot>>,
            submits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            editor_calls: std::sync::Arc<std::sync::atomic::AtomicUsize>,
            editor_outcome: EventOutcome,
        }
        impl bcode_plugin_sdk::tui::PluginTuiSurface for Owner {
            fn id(&self) -> &'static str {
                "test.focused-owner"
            }
            fn title(&self) -> &'static str {
                "Focused owner"
            }
            fn render(
                &mut self,
                _: bmux_tui::geometry::Rect,
                _: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }
            fn handle_event(
                &mut self,
                event: &Event,
                _: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> PluginTuiAction {
                if matches!(event, Event::Key(key) if key.key == bmux_keyboard::KeyCode::Enter) {
                    self.submits
                        .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                }
                PluginTuiAction::None
            }
            fn focused_text_field_event(
                &mut self,
                _: &Event,
                _: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> PluginTuiEventResult {
                self.editor_calls
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                PluginTuiEventResult {
                    outcome: self.editor_outcome,
                    action: PluginTuiAction::None,
                }
            }
            fn focused_text_field(&self) -> Option<FocusedTextFieldSnapshot> {
                Some(self.field.lock().unwrap().clone())
            }
            fn apply_focused_text_field_edit_result(
                &mut self,
                edit: &FocusedTextFieldEdit,
                _: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult {
                let mut field = self.field.lock().unwrap();
                let document = edit.validate(&field);
                if let Ok(Some(document)) = document {
                    field.document = document;
                    field.generation += 1;
                    drop(field);
                    bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult {
                        edit_result: bcode_plugin_sdk::tui::FocusedTextFieldEditResult::Applied,
                        event: PluginTuiEventResult {
                            outcome: EventOutcome::Redraw,
                            action: PluginTuiAction::Redraw,
                        },
                    }
                } else {
                    drop(field);
                    bcode_plugin_sdk::tui::PluginTuiFocusedTextFieldEditResult::rejected(
                        bcode_plugin_sdk::composer::ComposerValidationError::Stale,
                    )
                }
            }
        }
        let mut model = root_test_model();
        let field = standalone_focused_field();
        let submits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let editor_calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        model.loop_state.queue_root_plugin_surface(
            "test.owner",
            Box::new(Owner {
                field: field.clone(),
                submits: submits.clone(),
                editor_calls: editor_calls.clone(),
                editor_outcome,
            }),
        );
        StandaloneFocusedOwnerFixture {
            model,
            field,
            submits,
            editor_calls,
        }
    }

    #[tokio::test]
    async fn standalone_completion_scope_uses_launch_workspace_without_session() {
        let mut fixture = standalone_focused_owner_fixture();
        let launch = std::env::current_dir().unwrap();
        fixture.model.settings =
            super::super::chat_loop::TuiRuntimeSettings::bootstrap(launch.clone(), &[]);
        fixture
            .model
            .chat
            .app
            .set_plugin_presentation(std::sync::Arc::new(
                super::super::plugin_tui::PluginTuiPresentation::new(
                    bcode_plugin::PluginHost::default(),
                ),
            ));
        let scope = fixture.model.composer_completion_scope(true).unwrap();
        assert_eq!(scope.session, None);
        assert_eq!(scope.workspace, Some(launch));
        fixture.model.loop_state.close_root_plugin_surface();
        assert!(fixture.model.composer_completion_scope(true).is_none());
        drop(fixture);
    }

    #[tokio::test]
    async fn standalone_completion_escape_dismisses_before_parent_navigation() {
        let mut fixture = standalone_focused_owner_fixture();
        fixture.model.focused_input_owner = Some(super::FocusedInputOwner::RootSurface);
        fixture
            .model
            .loop_state
            .composer_interaction
            .observe_focused_field(
                super::super::composer_interaction::ComposerScope {
                    session: fixture.model.chat.attached_session_id(),
                    agent_id: None,
                    workspace: None,
                    registry_identity: 1,
                    registry_revision: 0,
                },
                fixture.field.lock().unwrap().clone(),
            );
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut fixture.model.loop_state.composer_interaction,
        );
        fixture.model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        );
        let escape =
            bmux_tui::event::Event::Key(bmux_keyboard::parse_key_stroke("escape").unwrap());
        assert_ne!(
            fixture.model.compose_focused_input_event(&escape),
            bmux_tui::event::EventOutcome::Ignored,
        );
        assert!(!fixture.model.loop_state.composer_interaction.visible());
        assert!(fixture.model.loop_state.has_root_plugin_surface());
        assert_eq!(
            fixture
                .editor_calls
                .load(std::sync::atomic::Ordering::SeqCst),
            0
        );
        assert_eq!(fixture.submits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            fixture.model.compose_focused_input_event(&escape),
            bmux_tui::event::EventOutcome::Ignored,
        );
        drop(fixture);
    }

    #[tokio::test]
    async fn standalone_focused_completion_routes_before_owner_submit() {
        let StandaloneFocusedOwnerFixture {
            mut model,
            field,
            submits,
            ..
        } = standalone_focused_owner_fixture();
        model.focused_input_owner = Some(super::FocusedInputOwner::RootSurface);
        let observed = field.lock().unwrap().clone();
        model.loop_state.composer_interaction.observe_focused_field(
            super::super::composer_interaction::ComposerScope {
                session: model.chat.attached_session_id(),
                agent_id: None,
                workspace: None,
                registry_identity: 1,
                registry_revision: 0,
            },
            observed,
        );
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        );
        model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        );
        composer_root_key(&mut model, "enter");
        assert_eq!(field.lock().unwrap().document.text, "主题 👩‍💻");
        drop(field);
        assert_eq!(submits.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(model.chat.app.take_pending_submission(), "");
        assert!(!model.loop_state.composer_interaction.visible());
        composer_root_key(&mut model, "enter");
        drop(model);
        assert_eq!(submits.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn ordinary_focused_input_is_delivered_once_and_only_ignored_reaches_surface() {
        use bmux_tui::event::{Event, EventOutcome};
        use bmux_tui_runtime::Program;
        for outcome in [
            EventOutcome::Ignored,
            EventOutcome::Handled,
            EventOutcome::Redraw,
        ] {
            let mut fixture = standalone_focused_owner_fixture_with_outcome(outcome);
            fixture
                .model
                .update(bmux_tui_runtime::RuntimeEvent::Terminal(Event::Key(
                    bmux_keyboard::parse_key_stroke("enter").unwrap(),
                )))
                .unwrap();
            assert_eq!(
                fixture
                    .editor_calls
                    .load(std::sync::atomic::Ordering::SeqCst),
                1
            );
            assert_eq!(
                fixture.submits.load(std::sync::atomic::Ordering::SeqCst),
                usize::from(outcome == EventOutcome::Ignored)
            );
            assert_eq!(fixture.model.chat.app.take_pending_submission(), "");
            drop(fixture);
        }
    }

    fn begin_root_composer(
        model: &mut super::BcodeRuntimeModel,
    ) -> bcode_plugin_sdk::composer::ComposerSearchRequest {
        model.focused_input_owner = Some(super::FocusedInputOwner::Composer);
        model
            .composer_field
            .observe(model.chat.app.composer(), true);
        let area = bmux_tui::geometry::Rect::new(0, 16, 90, 2);
        model
            .composer_field
            .painted(model.chat.app.composer(), area, area);
        model.loop_state.composer_interaction.observe_focused_field(
            super::super::composer_interaction::tests::scope(),
            model
                .composer_field
                .snapshot(model.chat.app.composer())
                .unwrap(),
        );
        super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        )
    }

    fn composer_root_fixture(config: &bcode_config::TuiConfig) -> super::BcodeRuntimeModel {
        let mut model = root_test_model_with_tui_config(config);
        model.chat.app.replace_composer_with("#topic");
        let request = begin_root_composer(&mut model);
        let mut reply = super::super::composer_interaction::tests::completion(&request);
        let candidates = &mut reply.snapshot.as_mut().unwrap().candidates;
        let mut second = candidates[0].clone();
        second.id = "second".into();
        second.label = "Second topic".into();
        candidates.push(second);
        model.loop_state.composer_interaction.complete(reply);
        paint_composer_root_fixture(&mut model, 90, 18);
        model
    }

    fn paint_composer_root_fixture(model: &mut super::BcodeRuntimeModel, width: u16, height: u16) {
        let area = bmux_tui::geometry::Rect::new(0, 0, width, height);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        super::super::composer_interaction_render::paint(
            &mut model.loop_state.composer_interaction,
            bmux_tui::geometry::Rect::new(0, height.saturating_sub(2), width, 2),
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
            super::super::render::TuiTheme::for_theme_id("terminal-native"),
        );
    }

    fn composer_root_key(model: &mut super::BcodeRuntimeModel, key: &str) {
        let event = bmux_tui::event::Event::Key(bmux_keyboard::parse_key_stroke(key).unwrap());
        if model.compose_focused_input_event(&event) == bmux_tui::event::EventOutcome::Ignored {
            model.handle_basic_terminal_event(event);
        }
    }

    #[tokio::test]
    async fn composer_root_live_actions_use_effective_keys_and_replay_during_loading() {
        for (overrides, active_key, hint) in [
            (
                std::collections::BTreeMap::new(),
                Some("ctrl+r"),
                Some("ctrl+r"),
            ),
            (
                std::collections::BTreeMap::from([
                    ("ctrl+r".into(), String::new()),
                    ("ctrl+x".into(), "example.refresh".into()),
                ]),
                Some("ctrl+x"),
                Some("ctrl+x"),
            ),
            (
                std::collections::BTreeMap::from([("ctrl+r".into(), String::new())]),
                None,
                None,
            ),
        ] {
            exercise_live_composer_action(overrides, active_key, hint);
        }
    }

    fn live_composer_action_fixture(
        overrides: std::collections::BTreeMap<String, String>,
    ) -> (
        super::BcodeRuntimeModel,
        super::super::composer_interaction::ComposerCompletion,
    ) {
        use bcode_plugin_sdk::composer::{ComposerAction, ComposerPresentation};
        use bcode_plugin_sdk::tui::keymap::KeymapAction;
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .scopes
            .insert("composer_interaction".into(), overrides);
        let mut model = composer_root_fixture(&config);
        model
            .settings
            .integrate_keymap_contributors(Ok(vec![KeymapAction {
                id: "example.refresh".into(),
                scope: "composer_interaction".into(),
                description: "Refresh".into(),
                defaults: vec!["ctrl+r".into()],
            }]));
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        );
        let mut reply = super::super::composer_interaction::tests::completion(&request);
        let snapshot = reply.snapshot.as_mut().unwrap();
        snapshot
            .interaction_state
            .insert("mode".into(), "first".into());
        snapshot.presentation = Some(ComposerPresentation {
            version: 1,
            title: "Topics".into(),
            hint: None,
            actions: vec![ComposerAction {
                id: "refresh".into(),
                label: "Refresh".into(),
                binding: Some("example.refresh".into()),
            }],
        });
        assert!(
            model
                .loop_state
                .composer_interaction
                .complete(reply.clone())
        );
        model
            .loop_state
            .composer_interaction
            .resolve_hints(model.settings.keymap().registry());
        (model, reply)
    }

    fn exercise_live_composer_action(
        overrides: std::collections::BTreeMap<String, String>,
        active_key: Option<&str>,
        hint: Option<&str>,
    ) {
        use bmux_tui::event::EventOutcome;
        let (mut model, reply) = live_composer_action_fixture(overrides);
        let before = model.chat.app.composer().edit_state();
        let history = (
            model.chat.app.composer().can_undo(),
            model.chat.app.composer().can_redo(),
        );
        if let Some(hint) = hint {
            assert!(model.loop_state.composer_interaction.hint().contains(hint));
        } else {
            assert_eq!(model.loop_state.composer_interaction.hint(), "");
        }
        if active_key != Some("ctrl+r") {
            assert_eq!(
                model.composer_interaction_key_outcome(
                    bmux_keyboard::parse_key_stroke("ctrl+r").unwrap()
                ),
                EventOutcome::Ignored
            );
            assert!(!model.loop_state.composer_interaction.needs_activation());
        }
        if let Some(key) = active_key {
            assert_eq!(
                model.compose_focused_input_event(&bmux_tui::event::Event::Key(
                    bmux_keyboard::parse_key_stroke(key).unwrap()
                )),
                EventOutcome::Handled
            );
            assert!(model.loop_state.composer_interaction.visible());
            assert!(
                model
                    .loop_state
                    .composer_interaction
                    .selected_edit()
                    .is_none()
            );
            assert!(
                !model
                    .loop_state
                    .composer_interaction
                    .complete(reply.clone())
            );
            let next = super::super::composer_interaction::tests::begin_observed(
                &mut model.loop_state.composer_interaction,
            );
            assert_eq!(next.action_id.as_deref(), Some("refresh"));
            assert_eq!(
                next.interaction_state.get("mode").map(String::as_str),
                Some("first")
            );
            assert_eq!(next.document.text, before.text);
            assert_ne!(next.request_id, reply.request_id);
            // The retained declaration remains usable while this request is loading.
            assert_eq!(
                model.composer_interaction_key_outcome(
                    bmux_keyboard::parse_key_stroke(key).unwrap()
                ),
                EventOutcome::Handled
            );
            let last = super::super::composer_interaction::tests::begin_observed(
                &mut model.loop_state.composer_interaction,
            );
            assert_eq!(last.action_id.as_deref(), Some("refresh"));
            assert_eq!(last.document, next.document);
            assert_ne!(last.request_id, next.request_id);
            assert!(
                !model
                    .loop_state
                    .composer_interaction
                    .complete(super::super::composer_interaction::tests::completion(&next))
            );
            assert_eq!(
                model.composer_interaction_key_outcome(
                    bmux_keyboard::parse_key_stroke("enter").unwrap()
                ),
                EventOutcome::Handled
            );
            assert!(model.loop_state.composer_interaction.hint().contains(key));
        }
        assert_eq!(model.chat.app.composer().edit_state(), before);
        assert_eq!(
            (
                model.chat.app.composer().can_undo(),
                model.chat.app.composer().can_redo()
            ),
            history
        );
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_default_keys_accept_navigate_dismiss_without_chat_fallthrough() {
        for key in ["enter", "tab"] {
            let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
            composer_root_key(&mut model, key);
            assert_eq!(model.chat.app.composer().text(), "主题 👩‍💻");
            assert_eq!(model.chat.app.take_pending_submission(), "");
            assert!(!model.loop_state.composer_interaction.visible());
            drop(model);
        }
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        composer_root_key(&mut model, "down");
        assert_eq!(
            model.loop_state.composer_interaction.list.focused(),
            Some(1)
        );
        composer_root_key(&mut model, "up");
        assert_eq!(
            model.loop_state.composer_interaction.list.focused(),
            Some(0)
        );
        assert_eq!(model.chat.app.composer().text(), "#topic");
        assert!(!model.chat.app.input_history_navigation_active());
        composer_root_key(&mut model, "escape");
        assert!(!model.loop_state.composer_interaction.visible());
        assert_eq!(model.chat.app.composer().text(), "#topic");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_remapped_actions_and_literal_typing_use_effective_registry() {
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "composer_interaction".into(),
            std::collections::BTreeMap::from([
                ("ctrl+n".into(), "tui.control.down".into()),
                ("ctrl+y".into(), "tui.control.submit".into()),
                ("ctrl+x".into(), "tui.control.cancel".into()),
            ]),
        );
        let mut model = composer_root_fixture(&config);
        composer_root_key(&mut model, "ctrl+n");
        assert_eq!(
            model.loop_state.composer_interaction.list.focused(),
            Some(1)
        );
        composer_root_key(&mut model, "ctrl+y");
        assert_eq!(model.chat.app.composer().text(), "主题 👩‍💻");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
        let mut model = composer_root_fixture(&config);
        composer_root_key(&mut model, "ctrl+x");
        assert!(!model.loop_state.composer_interaction.visible());
        drop(model);
        let mut model = composer_root_fixture(&config);
        composer_root_key(&mut model, "x");
        assert_eq!(model.chat.app.composer().text(), "#topicx");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_acceptance_is_one_undoable_transaction() {
        for (undo, redo) in [
            ("ctrl+z", "ctrl+shift+z"),
            ("super+z", "super+shift+z"),
            ("ctrl+z", "ctrl+y"),
        ] {
            let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
            model.chat.app.composer_mut().clear_history();
            let before = model.chat.app.composer().edit_state();
            composer_root_key(&mut model, "enter");
            let after = model.chat.app.composer().edit_state();
            assert_eq!(after.text, "主题 👩‍💻");
            assert_ne!(before, after);
            composer_root_key(&mut model, undo);
            assert_eq!(model.chat.app.composer().edit_state(), before);
            assert!(!model.chat.app.composer().can_undo());
            assert!(model.chat.app.composer().can_redo());
            composer_root_key(&mut model, redo);
            assert_eq!(model.chat.app.composer().edit_state(), after);
            assert!(!model.chat.app.composer().can_redo());
            assert_eq!(model.chat.app.take_pending_submission(), "");
        }
    }

    #[tokio::test]
    async fn composer_root_history_remaps_and_independent_unbinds() {
        let mut config = bcode_config::TuiConfig::default();
        config
            .keybindings
            .chat
            .insert("ctrl+r".into(), "tui.editor.undo".into());
        config
            .keybindings
            .chat
            .insert("ctrl+t".into(), "tui.editor.redo".into());
        let mut model = composer_root_fixture(&config);
        composer_root_key(&mut model, "enter");
        let after = model.chat.app.composer().edit_state();
        for key in ["ctrl+z", "super+z"] {
            composer_root_key(&mut model, key);
            assert_eq!(model.chat.app.composer().edit_state(), after);
        }
        composer_root_key(&mut model, "ctrl+r");
        assert_eq!(model.chat.app.composer().text(), "#topic");
        for key in ["ctrl+y", "ctrl+shift+z", "super+shift+z"] {
            composer_root_key(&mut model, key);
            assert_eq!(model.chat.app.composer().text(), "#topic");
        }
        composer_root_key(&mut model, "ctrl+t");
        assert_eq!(model.chat.app.composer().edit_state(), after);

        for (disabled, enabled) in [("ctrl+z", "super+z"), ("super+z", "ctrl+z")] {
            let mut config = bcode_config::TuiConfig::default();
            config
                .keybindings
                .chat
                .insert(disabled.into(), String::new());
            let mut model = composer_root_fixture(&config);
            composer_root_key(&mut model, "enter");
            let after = model.chat.app.composer().edit_state();
            composer_root_key(&mut model, disabled);
            assert_eq!(model.chat.app.composer().edit_state(), after);
            composer_root_key(&mut model, enabled);
            assert_eq!(model.chat.app.composer().text(), "#topic");
        }
        for disabled in ["ctrl+y", "ctrl+shift+z", "super+shift+z"] {
            let mut config = bcode_config::TuiConfig::default();
            config
                .keybindings
                .chat
                .insert(disabled.into(), String::new());
            let mut model = composer_root_fixture(&config);
            composer_root_key(&mut model, "enter");
            let after = model.chat.app.composer().edit_state();
            composer_root_key(&mut model, "ctrl+z");
            composer_root_key(&mut model, disabled);
            assert_eq!(model.chat.app.composer().text(), "#topic");
            let enabled = if disabled == "ctrl+y" {
                "ctrl+shift+z"
            } else {
                "ctrl+y"
            };
            composer_root_key(&mut model, enabled);
            assert_eq!(model.chat.app.composer().edit_state(), after);
        }
    }

    #[tokio::test]
    async fn composer_root_empty_history_commands_are_consumed_without_edits() {
        let mut model = root_test_model();
        model.chat.app.replace_composer_with("主题 👩‍💻");
        model.chat.app.composer_mut().clear_history();
        model.chat.app.composer_mut().select_all();
        let before = model.chat.app.composer().edit_state();
        for key in [
            "ctrl+z",
            "super+z",
            "ctrl+y",
            "ctrl+shift+z",
            "super+shift+z",
        ] {
            composer_root_key(&mut model, key);
            assert_eq!(model.chat.app.composer().edit_state(), before);
            assert!(!model.chat.app.composer().can_undo());
            assert!(!model.chat.app.composer().can_redo());
            assert_eq!(model.chat.app.take_pending_submission(), "");
        }
    }

    #[tokio::test]
    async fn composer_root_history_restores_selected_text_and_cursor() {
        let mut model = root_test_model();
        model.chat.app.replace_composer_with("主题 👩‍💻");
        model.chat.app.composer_mut().clear_history();
        model.chat.app.composer_mut().select_all();
        let before = model.chat.app.composer().edit_state();
        composer_root_key(&mut model, "x");
        let after = model.chat.app.composer().edit_state();
        assert_eq!(after.text, "x");
        composer_root_key(&mut model, "ctrl+z");
        assert_eq!(model.chat.app.composer().edit_state(), before);
        composer_root_key(&mut model, "ctrl+y");
        assert_eq!(model.chat.app.composer().edit_state(), after);
    }

    #[tokio::test]
    async fn composer_root_noop_acceptance_dismisses_without_consuming_redo() {
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        model.chat.app.composer_mut().clear_history();
        model.chat.app.composer_mut().insert_str("!");
        let future = model.chat.app.composer().edit_state();
        assert!(model.chat.app.composer_mut().undo());
        let before = model.chat.app.composer().edit_state();
        let request = begin_root_composer(&mut model);
        let mut reply = super::super::composer_interaction::tests::completion(&request);
        let edit = &mut reply.snapshot.as_mut().unwrap().candidates[0].edit;
        edit.replacement = request.document.text.clone();
        edit.cursor = request.document.cursor;
        assert!(model.loop_state.composer_interaction.complete(reply));
        paint_composer_root_fixture(&mut model, 90, 18);
        composer_root_key(&mut model, "enter");
        assert_eq!(model.chat.app.composer().edit_state(), before);
        assert!(!model.loop_state.composer_interaction.visible());
        assert_eq!(model.chat.app.take_pending_submission(), "");
        assert!(!model.chat.app.composer().can_undo());
        assert!(model.chat.app.composer().can_redo());
        composer_root_key(&mut model, "ctrl+y");
        let restored = model.chat.app.composer().edit_state();
        drop(model);
        assert_eq!(restored, future);
    }

    #[tokio::test]
    async fn composer_root_rejected_acceptance_consumes_enter_and_tab() {
        for key in ["enter", "tab"] {
            let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
            model.chat.app.composer_mut().clear_history();
            let original = model.chat.app.composer().edit_state();
            model.chat.app.composer_mut().insert_str("!");
            let changed = model.chat.app.composer().edit_state();
            assert!(model.chat.app.composer_mut().undo());
            assert_eq!(model.chat.app.composer().edit_state(), original);
            // Cursor-only staleness must not consume an existing redo branch.
            model
                .chat
                .app
                .composer_mut()
                .move_cursor(bmux_text_edit::TextMotion::Start);
            let before = model.chat.app.composer().edit_state();
            composer_root_key(&mut model, key);
            assert_eq!(model.chat.app.composer().edit_state(), before);
            assert!(!model.chat.app.composer().can_undo());
            assert!(model.chat.app.composer().can_redo());
            assert!(model.chat.app.composer_mut().redo());
            assert_eq!(model.chat.app.composer().edit_state(), changed);
            assert_eq!(model.chat.app.take_pending_submission(), "");
            assert!(model.chat.app.status().contains("draft unchanged"));
            assert!(!model.loop_state.composer_interaction.visible());
            drop(model);
        }
    }

    #[tokio::test]
    async fn composer_root_clipped_clicks_and_empty_resize_cannot_accept() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::Point;
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        let list = model.loop_state.composer_interaction.painted_list.unwrap();
        // Border/outside list click never accepts an invisible row.
        for point in [
            Point::new(list.x.saturating_sub(1), list.y),
            Point::new(list.x, list.bottom()),
        ] {
            for kind in [
                MouseEventKind::Down(MouseButton::Left),
                MouseEventKind::Up(MouseButton::Left),
            ] {
                model.compose_focused_input_event(&Event::Mouse(MouseEvent::new(kind, point)));
            }
            assert_eq!(model.chat.app.composer().text(), "#topic");
        }
        paint_composer_root_fixture(&mut model, 0, 0);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            model.compose_focused_input_event(&Event::Mouse(MouseEvent::new(
                kind,
                Point::new(list.x, list.y),
            )));
        }
        assert_eq!(model.chat.app.composer().text(), "#topic");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_visible_row_click_accepts_only_measured_item() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        let list = model.loop_state.composer_interaction.painted_list.unwrap();
        assert_eq!(model.loop_state.composer_interaction.list.selected(), None);
        assert_eq!(
            model.loop_state.composer_interaction.list.focused(),
            Some(0)
        );
        let point = bmux_tui::geometry::Point::new(list.x, list.y);
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            model.compose_focused_input_event(&Event::Mouse(MouseEvent::new(kind, point)));
        }
        assert_eq!(model.chat.app.composer().text(), "主题 👩‍💻");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_mouse_click_accepts_a_navigated_candidate_after_redraw() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        composer_root_key(&mut model, "down");
        assert_eq!(
            model.loop_state.composer_interaction.list.focused(),
            Some(1)
        );
        assert_eq!(model.loop_state.composer_interaction.list.selected(), None);
        paint_composer_root_fixture(&mut model, 90, 18);
        let list = model.loop_state.composer_interaction.painted_list.unwrap();
        let point = bmux_tui::geometry::Point::new(list.x, list.y.saturating_add(1));
        model.compose_focused_input_event(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            point,
        )));
        // Production redraw between press and release must retain pointer state.
        paint_composer_root_fixture(&mut model, 90, 18);
        model.compose_focused_input_event(&Event::Mouse(MouseEvent::new(
            MouseEventKind::Up(MouseButton::Left),
            point,
        )));
        assert_eq!(model.chat.app.composer().text(), "主题 👩‍💻");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_normal_typing_is_observed_without_an_interactive_surface() {
        use bmux_tui_runtime::Program;

        let mut model = root_test_model();
        // Production bootstrap supplies a validated absolute launch root, unlike
        // the general root fixture's deliberately relative directory.
        model.settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::env::current_dir().unwrap(),
            &[],
        );
        model.chat.app.set_plugin_presentation(std::sync::Arc::new(
            super::super::plugin_tui::PluginTuiPresentation::new(
                bcode_plugin::PluginHost::default(),
            ),
        ));
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            root_test_model(),
            bmux_tui_runtime::HeadlessPresenter::default(),
            bmux_tui_runtime::RuntimeConfig::default(),
        );
        model.runtime_handle = Some(handle);
        assert!(!model.loop_state.has_interactive_surface());
        for key in ["@", "r"] {
            model
                .update(bmux_tui_runtime::RuntimeEvent::Terminal(
                    bmux_tui::event::Event::Key(bmux_keyboard::parse_key_stroke(key).unwrap()),
                ))
                .unwrap();
        }
        let observed = model
            .loop_state
            .composer_interaction
            .document()
            .expect("normal typing reaches contributor observation")
            .text
            .clone();
        drop(model);
        drop(runtime);
        assert_eq!(observed, "@r");
    }

    #[tokio::test]
    async fn composer_root_retains_popup_until_edited_owner_has_painted() {
        let mut model = root_test_model();
        model.settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::env::current_dir().unwrap(),
            &[],
        );
        model.chat.app.set_plugin_presentation(std::sync::Arc::new(
            super::super::plugin_tui::PluginTuiPresentation::new(
                bcode_plugin::PluginHost::default(),
            ),
        ));
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            root_test_model(),
            bmux_tui_runtime::HeadlessPresenter::default(),
            bmux_tui_runtime::RuntimeConfig::default(),
        );
        model.runtime_handle = Some(handle);
        model.synchronize_composer_interaction();
        model.chat.app.replace_composer_with("#topic");
        let scope = model.composer_completion_scope(false).unwrap();
        model
            .loop_state
            .composer_interaction
            .observe(scope, model.chat.app.composer());
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        );
        assert!(model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        ));
        paint_composer_root_fixture(&mut model, 90, 18);
        let geometry = model.loop_state.composer_interaction.painted_list;
        model.chat.app.replace_composer_with("#topics");
        model.synchronize_composer_interaction();
        assert!(model.loop_state.composer_interaction.visible());
        assert!(
            model
                .loop_state
                .composer_interaction
                .selected_edit()
                .is_none()
        );
        assert!(!model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        ));
        paint_composer_root_fixture(&mut model, 90, 18);
        assert_eq!(model.loop_state.composer_interaction.painted_list, geometry);
        // A committed frame still lacking an eligible field really dismisses it.
        model.synchronize_composer_interaction_with_paint(true);
        assert!(!model.loop_state.composer_interaction.visible());
        drop(model);
        drop(runtime);
    }

    #[tokio::test]
    async fn composer_root_pending_reply_is_fenced_when_plugin_backend_is_disabled() {
        let mut model = root_test_model();
        model.chat.app.replace_composer_with("#topic");
        let request = begin_root_composer(&mut model);
        // No enabled plugin presentation exists after disable/unload. Root reconciliation
        // must cancel the retained pending request even when the text has not changed.
        assert!(model.chat.app.plugin_presentation().is_none());
        model.synchronize_composer_interaction();
        assert!(!model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request)
        ));
        assert_eq!(model.chat.app.composer().text(), "#topic");
        assert!(!model.loop_state.composer_interaction.visible());
        drop(model);
    }

    #[tokio::test]
    async fn composer_root_modal_foreground_has_priority_over_acceptance() {
        let mut model = composer_root_fixture(&bcode_config::TuiConfig::default());
        model.loop_state.open_command_palette(&mut model.chat);
        assert_eq!(
            model.compose_focused_input_event(&bmux_tui::event::Event::Key(
                bmux_keyboard::parse_key_stroke("enter").unwrap(),
            )),
            bmux_tui::event::EventOutcome::Ignored,
        );
        assert_eq!(model.chat.app.composer().text(), "#topic");
        assert_eq!(model.chat.app.take_pending_submission(), "");
        drop(model);
    }

    #[test]
    fn managed_root_blocks_model_controls_without_constructing_any_model_client() {
        let owner = bcode_session_models::SessionId::new();
        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "chat".into(),
            [
                ("f6".into(), "tui.agent.cycle".into()),
                ("f7".into(), "tui.thinking.effort.cycle".into()),
                ("f8".into(), "app.command_palette".into()),
            ]
            .into(),
        );
        let mut settings =
            crate::chat_loop::TuiRuntimeSettings::bootstrap(std::path::PathBuf::from("."), &[]);
        settings.apply_tui_config(&config);
        let mut loop_state = crate::chat_loop::ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(1);
        let mut model = super::BcodeRuntimeModel::new(
            {
                let mut chat = root_test_chat_with_history(owner, &[]);
                chat.app.set_managed_execution(owner);
                chat
            },
            settings,
            loop_state,
        );
        let agent = model.chat.app.current_agent_id().to_owned();
        let reasoning = model.chat.app.reasoning_effort().map(str::to_owned);
        for key in [
            bmux_keyboard::KeyCode::F(6),
            bmux_keyboard::KeyCode::F(7),
            bmux_keyboard::KeyCode::F(8),
        ] {
            model.handle_basic_terminal_event(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke::simple(key),
            ));
            let effects = model.chat.pending_effects.drain_runtime().0;
            match key {
                // The agent cycle selects the next native permission mode only.
                bmux_keyboard::KeyCode::F(6) => assert!(matches!(
                    &effects[..],
                    [(
                        _,
                        crate::effects::TuiEffect::ConfigureManagedSession {
                            generation: 1,
                            setting: bcode_harness_models::HarnessSessionSetting::PermissionMode {
                                mode: bcode_harness_models::HarnessPermissionMode::AcceptEdits
                            },
                            ..
                        }
                    )]
                )),
                // Host navigation may load host data, never model or plugin execution.
                bmux_keyboard::KeyCode::F(8) => assert!(
                    effects
                        .iter()
                        .all(|(_, effect)| effect.is_managed_host_operation())
                ),
                _ => assert!(effects.is_empty()),
            }
            assert_eq!(model.chat.app.current_agent_id(), agent);
            assert_eq!(model.chat.app.reasoning_effort(), reasoning.as_deref());
            assert!(model.loop_state.foreground_client().is_none());
            model.loop_state.close_command_palette_for_test();
        }
        model.chat.app.suspend_managed_input();
        model
            .chat
            .app
            .replace_composer_with("retained during disconnect");
        model.chat.app.stage_submission();
        assert!(!crate::composer_flow::stage_session_message(
            std::path::Path::new("."),
            crate::TuiLaunchOptions::default(),
            &mut model.chat,
            bcode_ipc::PromptPlacement::FollowUp
        ));
        assert!(model.chat.pending_effects.drain_runtime().0.is_empty());
        assert_eq!(
            model.chat.app.composer().text(),
            "retained during disconnect"
        );
        assert_eq!(model.chat.app.managed_execution_session(), Some(owner));
        drop(model);
    }

    fn managed_root_test_model(owner: bcode_session_models::SessionId) -> super::BcodeRuntimeModel {
        let settings =
            crate::chat_loop::TuiRuntimeSettings::bootstrap(std::path::PathBuf::from("."), &[]);
        let mut loop_state = crate::chat_loop::ChatLoopState::new_access(None, None, None, false);
        loop_state.set_managed_generation_for_test(1);
        super::BcodeRuntimeModel::new(
            {
                let mut chat = root_test_chat_with_history(owner, &[]);
                chat.app.set_managed_execution(owner);
                chat
            },
            settings,
            loop_state,
        )
    }

    #[tokio::test]
    async fn managed_composer_forwards_only_allow_listed_native_slash_commands() {
        let owner = bcode_session_models::SessionId::new();
        let mut model = managed_root_test_model(owner);
        model
            .chat
            .app
            .replace_composer_with("/compact keep the decisions");
        model.chat.app.stage_submission();
        model.stage_root_submission(bcode_ipc::PromptPlacement::FollowUp);
        let effects = model.chat.pending_effects.drain_runtime().0;
        assert!(
            matches!(
                &effects[..],
                [(_, crate::effects::TuiEffect::SubmitMessage { request })]
                    if request.message == "/compact keep the decisions"
                        && request.session_id == Some(owner)
            ),
            "/compact is ordinary managed input: {} effects, status {:?}",
            effects.len(),
            model.chat.app.status()
        );
        for command in ["/clear", "/skills"] {
            model.chat.app.replace_composer_with(command);
            model.chat.app.stage_submission();
            model.stage_root_submission(bcode_ipc::PromptPlacement::FollowUp);
            let effects = model.chat.pending_effects.drain_runtime().0;
            assert!(
                matches!(
                    &effects[..],
                    [(_, crate::effects::TuiEffect::ExecuteSlashCommand { message, .. })]
                        if message == command
                ),
                "{command} must stay a host slash command, never native input"
            );
        }
        drop(model);
    }

    #[test]
    fn managed_slash_completions_offer_only_managed_commands() {
        let owner = bcode_session_models::SessionId::new();
        let mut model = managed_root_test_model(owner);
        model.chat.app.replace_composer_with("/");
        let _changed = model.loop_state.refresh_slash_palette(&mut model.chat);
        let palette = model
            .loop_state
            .slash_palette_for_test()
            .expect("managed completions");
        let commands = palette.candidate_commands_for_test();
        assert!(commands.contains(&"/model"));
        assert!(commands.contains(&"/compact"));
        assert!(!commands.contains(&"/skills"));
        assert!(!commands.contains(&"/worktree"));
        // No discovery effect is needed (or allowed) for managed completions.
        assert!(model.chat.pending_effects.drain_runtime().0.is_empty());
        drop(model);
    }

    #[test]
    fn managed_root_navigation_exits_instead_of_opening_sessions_inline() {
        let owner = bcode_session_models::SessionId::new();
        let mut model = managed_root_test_model(owner);
        let other = bcode_session_models::SessionId::new();
        super::super::session_flow::start_switch_session(
            &mut model.chat,
            other,
            super::super::history_flow::initial_transcript_window_request(
                bmux_tui::geometry::Rect::new(0, 0, 80, 24),
            ),
        );
        assert!(model.chat.pending_effects.drain_runtime().0.is_empty());
        assert_eq!(model.chat.attached_session_id(), Some(owner));
        assert_eq!(
            model.chat.app.managed_exit_request(),
            Some(super::super::app::ManagedExitRequest::Navigate(Some(other)))
        );
        assert!(!model.chat.app.managed_input_ready());
    }

    #[test]
    fn managed_stream_loss_requests_reattach_or_recovery() {
        let owner = bcode_session_models::SessionId::new();
        for (update, expected) in [
            (
                super::super::history_flow::SessionStreamUpdate::ManagedOwnershipLost {
                    session_id: owner,
                },
                super::super::app::ManagedExitRequest::Recover,
            ),
            (
                super::super::history_flow::SessionStreamUpdate::ResyncStarted {
                    session_id: owner,
                },
                super::super::app::ManagedExitRequest::Reattach,
            ),
        ] {
            let mut model = managed_root_test_model(owner);
            let _update = bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Message(BcodeRuntimeMessage::SessionStream(
                    Box::new(update),
                )),
            )
            .unwrap();
            assert_eq!(model.chat.app.managed_exit_request(), Some(expected));
            assert!(!model.chat.app.managed_input_ready());
            drop(model);
        }
    }

    #[tokio::test]
    async fn foreground_plugin_ignored_input_never_reaches_chat_or_requests_damage() {
        use bmux_tui::event::Event;

        struct IgnoringSurface;
        impl bcode_plugin_sdk::tui::PluginTuiSurface for IgnoringSurface {
            fn id(&self) -> &'static str {
                "ignored-input"
            }
            fn title(&self) -> &'static str {
                "Ignored input"
            }
            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }
            fn handle_event(
                &mut self,
                _event: &Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }
        let mut model = root_test_model();
        model.chat.app.replace_composer_with("underlying draft");
        model.queue_plugin_surface("test", Box::new(IgnoringSurface));
        let signal = model.loop_state.root_plugin_surface_invalidation().unwrap();
        let _ = signal.take();
        for event in [
            Event::Key(bmux_keyboard::parse_key_stroke("x").unwrap()),
            Event::Paste("must not enter chat".into()),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(4, 3),
            )),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(8, 3),
            )),
            Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(8, 3),
            )),
        ] {
            assert_eq!(
                model.handle_basic_terminal_event(event),
                super::super::invalidation::UiInvalidation::None
            );
            assert_eq!(model.chat.app.composer().text(), "underlying draft");
            assert_eq!(model.transcript_host.selected_text(), None);
            assert!(!signal.take());
        }
    }

    #[tokio::test]
    async fn foreground_plugin_selection_gesture_stays_with_its_owner() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui::selection::{
            SelectionController, SelectionGesturePhase, SelectionScene, SelectionScope,
        };
        struct SelectingSurface {
            scene: SelectionScene,
            controller: std::sync::Arc<std::sync::Mutex<SelectionController>>,
        }
        impl bcode_plugin_sdk::tui::PluginTuiSurface for SelectingSurface {
            fn id(&self) -> &'static str {
                "selection-owner"
            }
            fn title(&self) -> &'static str {
                "Selection owner"
            }
            fn render(&mut self, _area: Rect, _frame: &mut bmux_tui::paint::PaintCx<'_, '_>) {}
            fn handle_event(
                &mut self,
                event: &Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if let Event::Mouse(mouse) = event {
                    self.controller
                        .lock()
                        .unwrap()
                        .handle_mouse(&self.scene, *mouse);
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }
        let mut scene = SelectionScene::new();
        let area = Rect::new(0, 0, 20, 1);
        scene.push_scope(SelectionScope::new("plugin.selection", area));
        for fragment in bmux_tui::selection::plain_text_fragments(
            "plugin.selection",
            "source",
            area,
            0,
            "selected plugin text",
            0,
            1,
        ) {
            scene.push_fragment(fragment);
        }
        let controller = std::sync::Arc::new(std::sync::Mutex::new(SelectionController::default()));
        let mut model = root_test_model();
        model.queue_plugin_surface(
            "test",
            Box::new(SelectingSurface {
                scene,
                controller: controller.clone(),
            }),
        );
        for (kind, x, phase) in [
            (
                MouseEventKind::Down(MouseButton::Left),
                1,
                SelectionGesturePhase::Armed,
            ),
            (
                MouseEventKind::Drag(MouseButton::Left),
                7,
                SelectionGesturePhase::Dragging,
            ),
            (
                MouseEventKind::Up(MouseButton::Left),
                7,
                SelectionGesturePhase::Complete,
            ),
        ] {
            assert_eq!(
                model.handle_basic_terminal_event(Event::Mouse(MouseEvent::new(
                    kind,
                    Point::new(x, 0)
                ))),
                super::super::invalidation::UiInvalidation::Structural
            );
            assert_eq!(controller.lock().unwrap().phase(), phase);
            assert_eq!(model.transcript_host.selected_text(), None);
        }
    }

    #[tokio::test]
    async fn unchanged_foreground_plugin_frame_diffs_without_clear_or_full_rewrite() {
        use bmux_tui::event::Event;
        use bmux_tui_runtime::Presenter;
        struct StableSurface;
        impl bcode_plugin_sdk::tui::PluginTuiSurface for StableSurface {
            fn id(&self) -> &'static str {
                "stable-frame"
            }
            fn title(&self) -> &'static str {
                "Stable frame"
            }
            fn render(
                &mut self,
                area: bmux_tui::geometry::Rect,
                frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
                frame.set_cell(
                    i32::from(area.x),
                    i64::from(area.y),
                    "S",
                    bmux_tui::style::Style::new(),
                );
                frame.push_selection_scope(bmux_tui::selection::SelectionScope::new(
                    "test.plugin",
                    area,
                ));
                for fragment in bmux_tui::selection::plain_text_fragments(
                    "test.plugin",
                    "source",
                    area,
                    0,
                    "stable plugin text",
                    0,
                    1,
                ) {
                    frame.push_selection_fragment(fragment);
                }
            }
            fn handle_event(
                &mut self,
                _event: &Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }
        let mut model = root_test_model();
        model.queue_plugin_surface("test", Box::new(StableSurface));
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            &mut bytes,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        );
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        assert_ne!(terminal.selection().scopes(), []);
        let selection = terminal.selection().clone();
        terminal.writer_mut().clear();
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        let report = super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        drop(model);
        assert_eq!(report.changed_cells, 0);
        assert!(!report.full_repaint);
        assert_eq!(terminal.selection(), &selection);
        let output = terminal.writer_mut();
        assert!(!output.windows(4).any(|bytes| bytes == b"\x1b[2J"));
        assert!(!output.windows(4).any(|bytes| bytes == b"\x1b[3J"));
        assert!(
            !output.contains(&b'S'),
            "unchanged cells must not be rewritten"
        );
        assert!(
            output.len() < 128,
            "only bounded cursor/style/sync controls are expected"
        );
    }

    struct GatedPluginPaint {
        release: Option<tokio::sync::oneshot::Receiver<()>>,
        completed: bool,
    }

    impl bcode_plugin_sdk::tui::PluginTuiSurface for GatedPluginPaint {
        fn id(&self) -> &'static str {
            "gated-paint"
        }
        fn title(&self) -> &'static str {
            "Gated paint"
        }
        fn render(
            &mut self,
            area: bmux_tui::geometry::Rect,
            cx: &mut bmux_tui::paint::PaintCx<'_, '_>,
        ) {
            cx.write_line(
                bmux_tui::paint::LocalRect::new(0, 0, area.width, 1),
                &bmux_tui::text::Line::from(if self.completed {
                    "Completed operation"
                } else {
                    "Review stays visible"
                }),
            );
        }
        fn handle_event(
            &mut self,
            _: &bmux_tui::event::Event,
            _: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        }
        fn drain_effects<'a>(
            &'a mut self,
            _: &'a dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = bcode_plugin_sdk::tui::PluginTuiAction>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                if let Some(release) = self.release.take() {
                    let _ = release.await;
                }
                self.completed = true;
                bcode_plugin_sdk::tui::PluginTuiAction::Redraw
            })
        }
    }

    #[tokio::test]
    async fn foreground_plugin_pending_operation_retains_committed_frame_until_completion() {
        use bmux_tui_runtime::Presenter;
        let (release, receiver) = tokio::sync::oneshot::channel();
        let mut model = root_test_model();
        model.queue_plugin_surface(
            "test",
            Box::new(GatedPluginPaint {
                release: Some(receiver),
                completed: false,
            }),
        );
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(
            &mut bytes,
            bmux_tui::geometry::Rect::new(0, 0, 80, 24),
        );
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        let committed = terminal.retained_buffer().unwrap().clone();
        let client = bcode_client::BcodeClient::default_endpoint();
        let launch = super::super::plugin_surface_host::FreshSessionSettings::default();
        model
            .loop_state
            .poll_root_plugin_surface(&client, launch.clone());
        assert!(model.loop_state.root_plugin_paint_pending());
        terminal.writer_mut().clear();
        for _ in 0..3 {
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            let report = super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .unwrap();
            assert_eq!(report.changed_cells, 0);
            assert!(!report.full_repaint);
            assert_eq!(terminal.retained_buffer(), Some(&committed));
            assert_eq!(terminal.writer_mut().as_slice(), &[] as &[u8]);
        }
        // A resize while borrowed must also wait, without consuming redraw damage.
        terminal.resize(bmux_tui::geometry::Rect::new(0, 0, 48, 16));
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        assert_eq!(terminal.writer_mut().as_slice(), &[] as &[u8]);
        release.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while model.loop_state.root_plugin_paint_pending() {
                tokio::task::yield_now().await;
                model
                    .loop_state
                    .poll_root_plugin_surface(&client, launch.clone());
            }
        })
        .await
        .unwrap();
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        drop(model);
        let buffer = terminal.retained_buffer().unwrap();
        assert_eq!(buffer.area().width, 48);
        assert!(
            buffer
                .row_symbols(0)
                .unwrap()
                .contains("Completed operation")
        );
    }

    fn root_test_model() -> super::BcodeRuntimeModel {
        root_test_model_with_tui_config(&bcode_config::TuiConfig::default())
    }

    fn root_test_model_with_tui_config(
        tui_config: &bcode_config::TuiConfig,
    ) -> super::BcodeRuntimeModel {
        let chat = root_test_chat();
        let mut settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        settings.apply_tui_config(tui_config);
        let mut actions = bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec();
        actions.extend_from_slice(bcode_shell_plugin::shell_tui_registry().keymap_actions());
        settings.integrate_keymap_contributors(Ok(actions));
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        super::BcodeRuntimeModel::new(chat, settings, loop_state)
    }

    #[test]
    fn response_local_selection_clamps_before_sibling_transcript_items() {
        let mut scene = bmux_tui::selection::SelectionScene::new();
        let body = bmux_tui::geometry::Rect::new(0, 0, 12, 3);
        scene.push_scope(bmux_tui::selection::SelectionScope::new(
            "bcode.transcript",
            body,
        ));
        for (scope, content, y, order, text) in [
            ("item.first", "item.first.plain", 0, 0, "first"),
            ("item.second", "item.second.plain", 2, 1, "second"),
        ] {
            scene.push_scope(
                bmux_tui::selection::SelectionScope::new(
                    scope,
                    bmux_tui::geometry::Rect::new(0, y, 12, 1),
                )
                .parent("bcode.transcript")
                .order(order),
            );
            for fragment in bmux_tui::selection::plain_text_fragments(
                scope,
                content,
                bmux_tui::geometry::Rect::new(0, y, 12, 1),
                order,
                text,
                0,
                0,
            ) {
                scene.push_fragment(fragment);
            }
        }

        let mut controller = bmux_tui::selection::SelectionController::new();
        assert_eq!(
            controller.handle_mouse(
                &scene,
                bmux_tui::event::MouseEvent::new(
                    bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                    bmux_tui::geometry::Point::new(1, 0),
                ),
            ),
            bmux_tui::selection::SelectionOutcome::Armed
        );
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(5, 2),
            ),
        );

        let snapshot = controller.snapshot(&scene).expect("selection snapshot");
        assert_eq!(snapshot.scope_id.as_str(), "item.first");
        assert!(
            snapshot
                .slices
                .iter()
                .all(|slice| slice.content_id.as_str() == "item.first.plain")
        );
    }

    #[test]
    fn transcript_chrome_selection_spans_items_in_document_order() {
        let mut scene = bmux_tui::selection::SelectionScene::new();
        let body = bmux_tui::geometry::Rect::new(0, 0, 12, 3);
        scene.push_scope(bmux_tui::selection::SelectionScope::new(
            "bcode.transcript",
            body,
        ));
        for (scope, content, y, order, text) in [
            ("item.first", "item.first.plain", 0, 0, "first"),
            ("item.second", "item.second.plain", 2, 1, "second"),
        ] {
            scene.push_scope(
                bmux_tui::selection::SelectionScope::new(
                    scope,
                    bmux_tui::geometry::Rect::new(0, y, 12, 1),
                )
                .parent("bcode.transcript")
                .order(order),
            );
            for fragment in bmux_tui::selection::plain_text_fragments(
                scope,
                content,
                bmux_tui::geometry::Rect::new(0, y, 12, 1),
                order,
                text,
                0,
                0,
            ) {
                scene.push_fragment(fragment);
            }
        }

        let mut controller = bmux_tui::selection::SelectionController::new();
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(0, 1),
            ),
        );
        let _ = controller.handle_mouse(
            &scene,
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(5, 2),
            ),
        );

        let snapshot = controller.snapshot(&scene).expect("selection snapshot");
        assert_eq!(snapshot.scope_id.as_str(), "bcode.transcript");
        assert_eq!(snapshot.slices.len(), 2);
        assert_eq!(snapshot.slices[0].content_id.as_str(), "item.first.plain");
        assert_eq!(snapshot.slices[1].content_id.as_str(), "item.second.plain");
    }

    #[tokio::test]
    async fn root_model_starts_with_empty_committed_selection_state() {
        let model = root_test_model();

        assert_eq!(model.committed_selection.scopes(), []);
        assert_eq!(model.committed_selection.fragments(), []);
        drop(model);
    }

    // Keep stationary modal transitions and their failed-flush authority checks together.
    #[allow(clippy::too_many_lines)]
    #[tokio::test]
    async fn stationary_pointer_tracks_root_modal_open_close_and_failed_flush() {
        use bmux_tui::event::{Event, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui::pointer::{PointerIntent, PointerShape, resolve_pointer_intent};
        use bmux_tui_runtime::{Presenter, Program};

        let mut model = root_test_model();
        let fail = std::rc::Rc::new(std::cell::Cell::new(false));
        let mut output = FlushFailureOutput {
            fail: fail.clone(),
            bytes: Vec::new(),
        };
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut output, Rect::new(0, 0, 80, 24));
        terminal.set_pointer_capability(
            bmux_terminal_protocol::pointer_output::PointerShapeCapability::Set,
        );
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("initial root scene");
        let composer = terminal
            .hits()
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "composer")
            .expect("committed composer hit")
            .area;
        let point = Point::new(composer.x, composer.bottom().saturating_sub(1));
        // Observe exactly one motion through the production lifecycle adapter.
        assert!(
            super::bcode_runtime_presenter(&mut terminal).observe_terminal_event(&Event::Mouse(
                MouseEvent::new(MouseEventKind::Move, point)
            ),)
        );
        let intent = |terminal: &bmux_tui::terminal::Terminal<&mut FlushFailureOutput>| {
            resolve_pointer_intent(
                terminal.hits(),
                Some(point),
                terminal.focus().active_scope(),
                None,
            )
        };
        assert_eq!(intent(&terminal), PointerIntent::Shape(PointerShape::Text));

        let contains = |bytes: &[u8], expected: &[u8]| {
            bytes
                .windows(expected.len())
                .any(|window| window == expected)
        };
        model.presentation_damage = bmux_tui::damage::Damage::None;
        terminal.writer_mut().bytes.clear();
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("stationary composer pointer");
        assert!(contains(&terminal.writer().bytes, b"\x1b]22;text\x1b\\"));

        for _ in 0..2 {
            terminal.writer_mut().bytes.clear();
            model.loop_state.open_command_palette(&mut model.chat);
            model.synchronize_screen();
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            fail.set(true);
            assert!(
                super::bcode_runtime_presenter(&mut terminal)
                    .present(&mut model)
                    .is_err()
            );
            assert!(terminal.focus().active_scope().is_none());
            assert_eq!(intent(&terminal), PointerIntent::Shape(PointerShape::Text));
            fail.set(false);
            terminal.writer_mut().bytes.clear();
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("stationary modal open");
            assert!(contains(&terminal.writer().bytes, b"\x1b]22;default\x1b\\"));
            terminal.writer_mut().bytes.clear();
            assert_eq!(
                terminal
                    .focus()
                    .active_scope()
                    .map(bmux_tui::hit::HitId::as_str),
                Some("bcode.foreground")
            );
            assert_eq!(
                intent(&terminal),
                PointerIntent::Shape(PointerShape::Default)
            );
            assert!(
                terminal
                    .hits()
                    .regions()
                    .iter()
                    .all(|hit| hit.id.as_str() != "composer")
            );

            model
                .update(bmux_tui_runtime::RuntimeEvent::Terminal(Event::Key(
                    bmux_keyboard::KeyStroke {
                        key: bmux_keyboard::KeyCode::Escape,
                        modifiers: bmux_keyboard::Modifiers::NONE,
                    },
                )))
                .expect("close modal input");
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            fail.set(true);
            assert!(
                super::bcode_runtime_presenter(&mut terminal)
                    .present(&mut model)
                    .is_err()
            );
            // Failed close cannot expose the background's text pointer authority.
            assert_eq!(
                intent(&terminal),
                PointerIntent::Shape(PointerShape::Default)
            );
            assert!(terminal.focus().active_scope().is_some());
            fail.set(false);
            terminal.writer_mut().bytes.clear();
            model.presentation_damage = bmux_tui::damage::Damage::Full;
            super::bcode_runtime_presenter(&mut terminal)
                .present(&mut model)
                .expect("stationary modal close recovery");
            assert!(contains(&terminal.writer().bytes, b"\x1b]22;text\x1b\\"));
            assert!(terminal.focus().active_scope().is_none());
            assert_eq!(intent(&terminal), PointerIntent::Shape(PointerShape::Text));
        }
        drop(model);
    }

    // Keep modal open, temporal reuse, reset, and close assertions in one lifecycle scenario.

    #[test]
    fn ordered_note_queue_releases_one_per_session_until_typed_completion() {
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let mut notes = BTreeMap::new();
        notes.insert(
            first_session,
            VecDeque::from([note(first_session, "0001"), note(first_session, "0002")]),
        );
        notes.insert(
            second_session,
            VecDeque::from([note(second_session, "0003")]),
        );
        let mut queue = OrderedPresentationQueue::default();
        queue.append(notes);

        let first = queue.take_ready();
        assert_eq!(first.len(), 2);
        assert!(queue.take_ready().is_empty());

        queue.complete(first_session);
        let second = queue.take_ready();
        assert_eq!(second.len(), 1);
        assert!(queue.take_ready().is_empty());
    }

    fn root_test_chat_with_input_history(
        input_history: &[bcode_session_models::SessionInputHistoryEntry],
    ) -> super::super::session_flow::ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(None, &[], input_history),
            agents: super::super::session_flow::AgentCatalog::default(),
            attachment: super::super::session_flow::ChatSessionAttachment::Draft,
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    fn root_test_chat_with_history(
        session_id: bcode_session_models::SessionId,
        history: &[bcode_session_models::SessionEvent],
    ) -> super::super::session_flow::ActiveChat {
        let (event_sender, event_receiver) = super::super::history_flow::session_stream_channel();
        super::super::session_flow::ActiveChat {
            app: super::super::app::BmuxApp::new_with_history(Some(session_id), history, &[]),
            agents: super::super::session_flow::AgentCatalog::default(),
            attachment: super::super::session_flow::ChatSessionAttachment::Attached { session_id },
            event_sender,
            event_receiver,
            event_task: None,
            opening_session_progress: None,
            pending_effects: super::super::effects::TuiEffectQueue::default(),
        }
    }

    pub(super) fn root_test_model_with_history(
        session_id: bcode_session_models::SessionId,
        history: &[bcode_session_models::SessionEvent],
    ) -> super::BcodeRuntimeModel {
        let chat = root_test_chat_with_history(session_id, history);
        let mut settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let mut actions = bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec();
        actions.extend_from_slice(bcode_shell_plugin::shell_tui_registry().keymap_actions());
        settings.integrate_keymap_contributors(Ok(actions));
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let loop_state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        super::BcodeRuntimeModel::new(chat, settings, loop_state)
    }

    fn root_test_chat() -> super::super::session_flow::ActiveChat {
        root_test_chat_with_input_history(&[])
    }

    fn paint_focused_owner_for_root_test(
        model: &mut super::BcodeRuntimeModel,
        terminal: &mut bmux_tui::terminal::Terminal<&mut Vec<u8>>,
    ) {
        bmux_tui_runtime::Presenter::present(&mut super::bcode_runtime_presenter(terminal), model)
            .expect("real focused owner presentation");
    }

    #[tokio::test]
    async fn focused_completion_accepts_through_question_owner_without_submission() {
        use bmux_tui::event::Event;
        use bmux_tui::geometry::Rect;
        let mut model = root_test_model();
        let mut config = model.chat.app.tui_config().clone();
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
        model.chat.app.apply_tui_config(config);
        let surface = question_surface_with_questions_for_root_test(
            model.settings.keymap(),
            serde_json::json!([{
                "header": null, "question": "Path", "options": [],
                "control": "radio", "selection_mode": "single", "custom": true,
                "custom_mode": "additional", "required": true
            }]),
        )
        .await;
        model
            .loop_state
            .install_interactive_surface_for_test(surface);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, Rect::new(0, 0, 80, 30));
        paint_focused_owner_for_root_test(&mut model, &mut terminal);
        assert!(model.loop_state.interactive_focused_text_field().is_some());
        for key in ["@", "a"] {
            bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Terminal(Event::Key(
                    bmux_keyboard::parse_key_stroke(key).unwrap(),
                )),
            )
            .unwrap();
        }
        // Commit owner presentation after typing, before observing its measured active field.
        paint_focused_owner_for_root_test(&mut model, &mut terminal);
        let field = model
            .loop_state
            .interactive_focused_text_field()
            .expect("visible question field after real paint");
        assert_eq!(field.document.text, "@a");
        let destination = model
            .loop_state
            .active_interactive_surface_geometry()
            .expect("presented owner geometry")
            .destination;
        assert!(!destination.is_empty());
        assert_eq!(field.anchor.intersection(destination), field.anchor);
        assert_eq!(
            field.content_rect.intersection(destination),
            field.content_rect
        );
        let scope = super::super::composer_interaction::ComposerScope {
            session: model.chat.attached_session_id(),
            agent_id: Some("build".into()),
            workspace: Some(std::path::PathBuf::from("/workspace")),
            registry_identity: 1,
            registry_revision: 0,
        };
        model
            .loop_state
            .composer_interaction
            .observe_focused_field(scope, field);
        model.focused_input_owner = Some(super::FocusedInputOwner::InteractiveSurface);
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        );
        model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        );
        assert_ne!(
            model.compose_focused_input_event(&Event::Key(
                bmux_keyboard::parse_key_stroke("enter").unwrap(),
            )),
            bmux_tui::event::EventOutcome::Ignored,
        );
        assert_eq!(model.chat.app.take_pending_submission(), "");
        assert!(
            model
                .loop_state
                .latched_surface_outcome_for_test()
                .is_none()
        );
        assert!(!model.loop_state.composer_interaction.visible());
        assert_completion_escape_retains_question(&mut model);
        // A subsequent explicit owner submit proves the semantic answer, not a detached buffer, changed.
        let outcome = model
            .loop_state
            .handle_interactive_surface_event(&Event::Key(
                bmux_keyboard::parse_key_stroke("enter").unwrap(),
            ));
        drop(model);
        let super::super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
            bcode_session_models::ToolExchangeResolution::Responded { payload },
        ) = outcome
        else {
            panic!("owner submission must resolve");
        };
        assert!(payload.to_string().contains("主题"));
    }

    fn assert_completion_escape_retains_question(model: &mut super::BcodeRuntimeModel) {
        let request = super::super::composer_interaction::tests::begin_observed(
            &mut model.loop_state.composer_interaction,
        );
        model.loop_state.composer_interaction.complete(
            super::super::composer_interaction::tests::completion(&request),
        );
        bmux_tui_runtime::Program::update(
            model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::parse_key_stroke("escape").unwrap(),
            )),
        )
        .unwrap();
        assert!(model.loop_state.has_interactive_surface());
        assert!(
            model
                .loop_state
                .latched_surface_outcome_for_test()
                .is_none()
        );
        assert!(!model.loop_state.composer_interaction.visible());
    }

    async fn question_surface_with_questions_for_root_test(
        keymap: &super::super::keymap::BmuxKeyMap,
        questions: serde_json::Value,
    ) -> super::super::interactive_surface::InteractiveSurfaceState {
        let plugin = bcode_plugin::StaticBundledPlugin::new(
            include_str!("../../../plugins/question-plugin/bcode-plugin.toml"),
            bcode_question_plugin::static_plugin(),
        );
        let runtime = bcode_plugin::PluginRuntimeHost::load_defaults_with_static_bundled(
            &bcode_plugin::PluginSelection::all_enabled(),
            &[plugin],
        )
        .expect("question plugin runtime");
        let mut effective_keymap = keymap.clone();
        effective_keymap.integrate_contributors(Ok(bcode_question_plugin::question_tui_registry()
            .keymap_actions()
            .to_vec()));
        super::super::interactive_surface::InteractiveSurfaceState::open(
            &runtime,
            "question-root-test",
            "bcode.question.inline",
            &serde_json::json!({ "questions": questions }).to_string(),
            &effective_keymap,
        )
        .await
        .expect("question surface")
    }

    async fn question_surface_for_root_test(
        keymap: &super::super::keymap::BmuxKeyMap,
    ) -> super::super::interactive_surface::InteractiveSurfaceState {
        question_surface_with_questions_for_root_test(
            keymap,
            serde_json::json!([{
                "header": null,
                "question": "Choose one",
                "options": [
                    {"label": "One", "value": "one", "description": null},
                    {"label": "Two", "value": "two", "description": null}
                ],
                "control": "radio",
                "selection_mode": "single",
                "custom": false,
                "custom_mode": "additional",
                "required": true
            }]),
        )
        .await
    }

    #[tokio::test]
    async fn failed_delivery_preserves_surface_and_clears_only_pending_resolution() {
        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        loop_state.install_interactive_surface_for_test(
            question_surface_for_root_test(settings.keymap()).await,
        );
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Enter,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
        ));
        let tab = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Tab,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let enter = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Enter,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _ = loop_state.handle_interactive_surface_event(&tab);
        let _ = loop_state.handle_interactive_surface_event(&tab);
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&enter),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(_)
        ));
        assert!(loop_state.latched_surface_outcome_for_test().is_some());

        loop_state.complete_interactive_surface_resolution(false);
        assert_eq!(
            loop_state.active_interactive_surface_id(),
            Some("question-root-test")
        );
        assert!(loop_state.latched_surface_outcome_for_test().is_none());

        let _ = loop_state.handle_interactive_surface_event(&enter);
        assert!(loop_state.latched_surface_outcome_for_test().is_some());
        loop_state.complete_interactive_surface_resolution(true);
        assert!(loop_state.active_interactive_surface_id().is_none());
    }

    #[tokio::test]
    async fn hidden_transcript_geometry_applies_retain_and_suspend_routing() {
        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        loop_state.set_interactive_surface_geometry_for_test(Some(
            super::super::chat_loop::InteractiveSurfaceGeometry {
                placement: super::super::chat_loop::InteractiveSurfacePlacement::Transcript,
                logical_height: 20,
                visible_logical_offset: 0,
                destination: bmux_tui::geometry::Rect::new(0, 0, 80, 0),
            },
        ));
        let mut config = bcode_config::TuiInteractionConfig::default();
        assert!(loop_state.routes_non_mouse_event_to_interactive_surface(config));
        config.offscreen_focus = bcode_config::TuiInteractionOffscreenFocus::Suspend;
        assert!(!loop_state.routes_non_mouse_event_to_interactive_surface(config));

        loop_state.set_interactive_surface_geometry_for_test(Some(
            super::super::chat_loop::InteractiveSurfaceGeometry {
                placement: super::super::chat_loop::InteractiveSurfacePlacement::Pinned,
                logical_height: 20,
                visible_logical_offset: 0,
                destination: bmux_tui::geometry::Rect::new(0, 0, 80, 0),
            },
        ));
        assert!(loop_state.routes_non_mouse_event_to_interactive_surface(config));
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One production-route scenario exercises both clipping boundaries and the complete pointer gesture.
    async fn committed_geometry_routes_top_and_bottom_clipped_pointer_input() {
        use std::sync::{Arc, Mutex};

        struct RecordingSurface(
            Arc<Mutex<Vec<(bmux_tui::event::MouseEventKind, bmux_tui::geometry::Point)>>>,
        );

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RecordingSurface {
            fn id(&self) -> &'static str {
                "recording"
            }

            fn title(&self) -> &'static str {
                "recording"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if let bmux_tui::event::Event::Mouse(mouse) = event {
                    self.0
                        .lock()
                        .expect("events")
                        .push((mouse.kind, mouse.position));
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut loop_state = super::super::chat_loop::ChatLoopState::new(&client, &passive, false);
        let events = Arc::new(Mutex::new(Vec::new()));
        loop_state.install_interactive_surface_for_test(
            super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                "interaction",
                Box::new(RecordingSurface(Arc::clone(&events))),
                &super::super::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default()),
            ),
        );

        for (visible_logical_offset, destination, screen_point, expected_logical_point) in [
            (
                0,
                bmux_tui::geometry::Rect::new(5, 7, 20, 4),
                bmux_tui::geometry::Point::new(8, 10),
                bmux_tui::geometry::Point::new(3, 3),
            ),
            (
                26,
                bmux_tui::geometry::Rect::new(5, 7, 20, 4),
                bmux_tui::geometry::Point::new(8, 9),
                bmux_tui::geometry::Point::new(3, 28),
            ),
        ] {
            loop_state.set_interactive_surface_geometry_for_test(Some(
                super::super::chat_loop::InteractiveSurfaceGeometry {
                    placement: super::super::chat_loop::InteractiveSurfacePlacement::Transcript,
                    logical_height: 30,
                    visible_logical_offset,
                    destination,
                },
            ));

            for kind in [
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::event::MouseEventKind::Drag(bmux_tui::event::MouseButton::Left),
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
            ] {
                let event = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
                    kind,
                    screen_point,
                ));
                assert!(matches!(
                    loop_state.handle_interactive_surface_event(&event),
                    super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
                ));
            }

            let outside = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
                bmux_tui::geometry::Point::new(destination.x.saturating_sub(1), screen_point.y),
            ));
            assert!(matches!(
                loop_state.handle_interactive_surface_event(&outside),
                super::super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored
            ));

            let recorded = events.lock().expect("events");
            let recent = &recorded[recorded.len().saturating_sub(3)..];
            assert!(
                recent
                    .iter()
                    .all(|(_, point)| *point == expected_logical_point)
            );
            drop(recorded);
        }

        assert_eq!(events.lock().expect("events").len(), 6);
        loop_state.set_interactive_surface_geometry_for_test(None);
        let event = bmux_tui::event::Event::Mouse(bmux_tui::event::MouseEvent::new(
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::geometry::Point::new(8, 9),
        ));
        assert!(matches!(
            loop_state.handle_interactive_surface_event(&event),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Ignored
        ));
    }

    #[tokio::test]
    async fn interaction_host_keys_keep_application_and_transcript_precedence() {
        let model = root_test_model();
        let key = |key, modifiers| bmux_keyboard::KeyStroke { key, modifiers };
        let ctrl = bmux_keyboard::Modifiers {
            ctrl: true,
            ..bmux_keyboard::Modifiers::NONE
        };

        for (stroke, expected) in [
            (
                key(
                    bmux_keyboard::KeyCode::PageUp,
                    bmux_keyboard::Modifiers::NONE,
                ),
                super::super::keymap::BmuxAction::TranscriptPageUp,
            ),
            (
                key(
                    bmux_keyboard::KeyCode::PageDown,
                    bmux_keyboard::Modifiers::NONE,
                ),
                super::super::keymap::BmuxAction::TranscriptPageDown,
            ),
            (
                key(bmux_keyboard::KeyCode::Home, ctrl),
                super::super::keymap::BmuxAction::TranscriptTop,
            ),
            (
                key(bmux_keyboard::KeyCode::End, ctrl),
                super::super::keymap::BmuxAction::TranscriptBottom,
            ),
            (
                key(bmux_keyboard::KeyCode::Up, ctrl),
                super::super::keymap::BmuxAction::TranscriptLineUp,
            ),
            (
                key(bmux_keyboard::KeyCode::Down, ctrl),
                super::super::keymap::BmuxAction::TranscriptLineDown,
            ),
            (
                key(bmux_keyboard::KeyCode::Char('d'), ctrl),
                super::super::keymap::BmuxAction::AppExit,
            ),
        ] {
            assert_eq!(
                model.root_interactive_surface_host_key(stroke),
                Some(expected)
            );
        }
        assert_eq!(
            model.root_interactive_surface_host_key(key(
                bmux_keyboard::KeyCode::Up,
                bmux_keyboard::Modifiers::NONE,
            )),
            None
        );
        drop(model);
    }

    #[tokio::test]
    async fn stale_surface_open_completion_does_not_report_a_retry_failure() {
        let mut model = root_test_model();
        model.chat.app.set_status("steady".to_owned());
        let surface = question_surface_for_root_test(model.settings.keymap()).await;

        let _update = bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(
                super::BcodeRuntimeMessage::InteractiveSurfaceOpened(Ok(surface)),
            ),
        )
        .expect("stale open completion is handled");

        assert_eq!(model.chat.app.status(), "steady");
        assert!(!model.loop_state.has_interactive_surface());
        drop(model);
    }

    #[tokio::test]
    #[allow(clippy::too_many_lines)] // One matrix keeps semantic inputs and both production placements identical.
    async fn normal_question_path_covers_radio_checkbox_custom_validation_and_both_placements() {
        for placement in [
            bcode_config::TuiInteractionPlacement::Transcript,
            bcode_config::TuiInteractionPlacement::Pinned,
        ] {
            let mut model = root_test_model();
            let mut config = model.chat.app.tui_config().clone();
            config.interactions.placement = placement;
            model.chat.app.apply_tui_config(config);
            let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
            model.loop_state.install_interactive_surface_for_test(
                question_surface_with_questions_for_root_test(
                    &keymap,
                    serde_json::json!([
                        {
                            "header": null,
                            "question": "Choose radio",
                            "options": [
                                {"label": "One", "value": "one", "description": null},
                                {"label": "Two", "value": "two", "description": null}
                            ],
                            "control": "radio",
                            "selection_mode": "single",
                            "custom": false,
                            "custom_mode": "additional",
                            "required": true
                        },
                        {
                            "header": null,
                            "question": "Choose checks",
                            "options": [
                                {"label": "Alpha", "value": "alpha", "description": null},
                                {"label": "Beta", "value": "beta", "description": null}
                            ],
                            "control": "checkbox",
                            "selection_mode": "multiple",
                            "custom": false,
                            "custom_mode": "additional",
                            "required": false
                        },
                        {
                            "header": null,
                            "question": "Explain",
                            "options": [],
                            "control": "radio",
                            "selection_mode": "single",
                            "custom": true,
                            "custom_mode": "additional",
                            "required": true
                        }
                    ]),
                )
                .await,
            );
            let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
            let mut bytes = Vec::new();
            let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
            bmux_tui_runtime::Presenter::present(
                &mut super::bcode_runtime_presenter(&mut terminal),
                &mut model,
            )
            .expect("initial question matrix frame");

            let event = |key| {
                bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                    bmux_keyboard::KeyStroke {
                        key,
                        modifiers: bmux_keyboard::Modifiers::NONE,
                    },
                ))
            };
            for key in [
                bmux_keyboard::KeyCode::Enter,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Space,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Tab,
                bmux_keyboard::KeyCode::Char('x'),
            ] {
                bmux_tui_runtime::Program::update(&mut model, event(key))
                    .expect("question matrix input");
                bmux_tui_runtime::Presenter::present(
                    &mut super::bcode_runtime_presenter(&mut terminal),
                    &mut model,
                )
                .expect("question matrix committed presentation");
            }
            drop(model);
        }

        let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
            std::path::PathBuf::from("."),
            &[],
        );
        let mut validation_surface = question_surface_with_questions_for_root_test(
            settings.keymap(),
            serde_json::json!([{
                "header": null,
                "question": "Required",
                "options": [],
                "control": "radio",
                "selection_mode": "single",
                "custom": true,
                "custom_mode": "additional",
                "required": true
            }]),
        )
        .await;
        let before = validation_surface.preferred_height(40);
        assert!(matches!(
            validation_surface.handle_event_outcome(&bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Enter,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Relayout
        ));
        assert!(validation_surface.preferred_height(40) > before);
    }

    #[tokio::test]
    async fn interactive_surface_navigation_repaints_without_composer_history_fallthrough() {
        let history = [bcode_session_models::SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }];
        let mut model = {
            let mut chat = root_test_chat_with_input_history(&history);
            chat.app.replace_composer_with("draft prompt");
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let mut loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            loop_state.install_interactive_surface_for_test(
                question_surface_for_root_test(settings.keymap()).await,
            );
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };

        let down = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Down,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(down),
        )
        .expect("root update handles question navigation");

        assert_eq!(model.chat.app.composer().text(), "draft prompt");
        assert!(!model.chat.app.input_history_navigation_active());
        assert_eq!(
            model
                .loop_state
                .active_interactive_surface_native_id_for_test(),
            Some("question-inline")
        );

        let key = |key| {
            bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
                key,
                modifiers: bmux_keyboard::Modifiers::NONE,
            })
        };
        assert!(matches!(
            model
                .loop_state
                .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Enter)),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
        ));
        assert!(matches!(
            model
                .loop_state
                .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Tab)),
            super::super::interactive_surface::InteractiveSurfaceEventOutcome::Redraw
        ));
        let outcome = model
            .loop_state
            .handle_interactive_surface_event(&key(bmux_keyboard::KeyCode::Enter));
        drop(model);
        let super::super::interactive_surface::InteractiveSurfaceEventOutcome::Resolved(
            bcode_session_models::ToolExchangeResolution::Responded { payload },
        ) = outcome
        else {
            panic!("question should submit after selecting the second option");
        };
        assert_eq!(payload["questions"][0]["selected"][0], "two");
    }

    #[tokio::test]
    async fn consumed_interaction_navigation_does_not_reach_composer_history() {
        struct ConsumingSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for ConsumingSurface {
            fn id(&self) -> &'static str {
                "consuming-test"
            }

            fn title(&self) -> &'static str {
                "Consuming test"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                if matches!(
                    event,
                    bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
                        key: bmux_keyboard::KeyCode::Up | bmux_keyboard::KeyCode::Down,
                        ..
                    })
                ) {
                    bcode_plugin_sdk::tui::PluginTuiAction::Redraw
                } else {
                    bcode_plugin_sdk::tui::PluginTuiAction::None
                }
            }
        }

        let history = [bcode_session_models::SessionInputHistoryEntry {
            timestamp_ms: 1,
            sequence: 1,
            text: "previous prompt".to_owned(),
        }];
        let mut model = {
            let mut chat = root_test_chat_with_input_history(&history);
            chat.app.replace_composer_with("draft prompt");
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let mut loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            loop_state.install_interactive_surface_for_test(
                super::super::interactive_surface::InteractiveSurfaceState::from_surface_for_test(
                    "test-interaction",
                    Box::new(ConsumingSurface),
                    settings.keymap(),
                ),
            );
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };
        model.invalidation = super::super::invalidation::UiInvalidation::None;

        let _update = bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Up,
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            )),
        )
        .expect("root update handles consumed interaction input");

        assert_eq!(model.chat.app.composer().text(), "draft prompt");
        assert!(!model.chat.app.input_history_navigation_active());
        assert_eq!(
            model.invalidation,
            super::super::invalidation::UiInvalidation::Paint
        );
        drop(model);
    }

    #[tokio::test]
    #[ignore = "manual already-built question interaction committed-presentation latency probe"]
    #[allow(clippy::too_many_lines)] // The emitted artifact keeps timing and work-shape attribution together.
    async fn question_interaction_to_committed_presentation_latency_report() {
        const SAMPLES: usize = 50;
        const LOCKED_FRAME_BUDGET: std::time::Duration = std::time::Duration::from_millis(17);
        let mut model = root_test_model();
        let mut config = model.chat.app.tui_config().clone();
        config.interactions.placement = bcode_config::TuiInteractionPlacement::Pinned;
        model.chat.app.apply_tui_config(config);
        let keymap = super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
        model
            .loop_state
            .install_interactive_surface_for_test(question_surface_for_root_test(&keymap).await);
        let area = bmux_tui::geometry::Rect::new(0, 0, 80, 24);
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, area);
        bmux_tui_runtime::Presenter::present(
            &mut super::bcode_runtime_presenter(&mut terminal),
            &mut model,
        )
        .expect("initial question frame");

        let request_bytes = serde_json::to_vec(&serde_json::json!({
            "questions": [{
                "header": null,
                "question": "Choose one",
                "options": [
                    {"label": "One", "value": "one", "description": null},
                    {"label": "Two", "value": "two", "description": "Second choice"}
                ],
                "control": "radio",
                "selection_mode": "single",
                "custom": false,
                "custom_mode": "additional",
                "required": true
            }]
        }))
        .expect("question fixture serialization")
        .len();
        let initial_geometry = model
            .loop_state
            .active_interactive_surface_geometry()
            .expect("committed question geometry");
        let initial_height = initial_geometry.logical_height;
        let visible_rows = initial_geometry.destination.height;
        let mut samples = Vec::with_capacity(SAMPLES);
        let mut draw_samples = Vec::with_capacity(SAMPLES);
        let mut changed_cells = Vec::with_capacity(SAMPLES);
        let mut full_repaints = 0_usize;
        for index in 0..SAMPLES {
            let started = std::time::Instant::now();
            let event = bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: if index.is_multiple_of(2) {
                        bmux_keyboard::KeyCode::Down
                    } else {
                        bmux_keyboard::KeyCode::Up
                    },
                    modifiers: bmux_keyboard::Modifiers::NONE,
                },
            ));
            bmux_tui_runtime::Program::update(&mut model, event).expect("question input");
            let draw_started = std::time::Instant::now();
            let report = bmux_tui_runtime::Presenter::present(
                &mut super::bcode_runtime_presenter(&mut terminal),
                &mut model,
            )
            .expect("question presentation");
            samples.push(started.elapsed());
            draw_samples.push(draw_started.elapsed());
            changed_cells.push(report.changed_cells);
            full_repaints = full_repaints.saturating_add(usize::from(report.full_repaint));
        }
        let work_shape = model
            .loop_state
            .active_interactive_surface_work_shape_for_test();
        drop(model);
        let summary = latency_summary(&samples);
        let draw_summary = latency_summary(&draw_samples);
        println!(
            "{}",
            serde_json::json!({
                "kind": "bcode_question_interaction_committed_presentation_latency",
                "profile": if cfg!(debug_assertions) { "debug" } else { "release" },
                "sample_count": SAMPLES,
                "locked_p99_budget_ms": LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0,
                "latency": summary,
                "draw_latency": draw_summary,
                "work_shape": {
                    "fixture_request_bytes": request_bytes,
                    "snapshot_materializations": work_shape.snapshot_materializations,
                    "copied_request_bytes": request_bytes * usize::try_from(work_shape.snapshot_materializations).unwrap_or(usize::MAX),
                    "preferred_height_measurements": work_shape.preferred_height_measurements,
                    "focused_row_measurements": work_shape.focused_row_measurements,
                    "wrapped_rows_visited": work_shape.wrapped_rows_visited,
                    "semantic_events": SAMPLES,
                    "logical_rows": initial_height,
                    "visible_rows_rendered_per_frame": visible_rows,
                    "broad_frame_preparations": 0,
                    "transcript_entries_scanned": 0,
                    "transcript_entries_rebuilt": 0,
                    "transcript_rows_regenerated": 0,
                    "scheduling_delay_ms": 0
                },
                "changed_cells": changed_cells,
                "full_repaints": full_repaints,
            })
        );
        assert!(
            summary["p99_ms"].as_f64().expect("numeric p99")
                <= LOCKED_FRAME_BUDGET.as_secs_f64() * 1_000.0
        );
        assert_eq!(full_repaints, 0);
    }

    #[tokio::test]
    async fn composer_send_button_uses_existing_submission_flow_and_rejects_duplicate_release() {
        let mut model = root_test_model();
        model
            .chat
            .app
            .replace_composer_with("hello from the Send button");
        let area = bmux_tui::geometry::Rect::new(0, 0, 60, 15);
        let mut buffer = bmux_tui::buffer::Buffer::empty(area);
        let mut frame = bmux_tui::frame::Frame::new(&mut buffer);
        super::super::render::render(
            &mut model.chat.app,
            &mut bmux_tui::paint::PaintCx::new(&mut frame),
        );
        model.committed_hits = frame.hits().clone();
        let send = model
            .committed_hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "composer.send")
            .expect("Send button")
            .area;
        let position = bmux_tui::geometry::Point::new(send.x, send.y);
        for kind in [
            bmux_tui::event::MouseEventKind::Down(bmux_tui::event::MouseButton::Left),
            bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
        ] {
            model.handle_basic_terminal_event(bmux_tui::event::Event::Mouse(
                bmux_tui::event::MouseEvent::new(kind, position),
            ));
        }
        assert!(model.chat.app.composer().is_empty());
        let (effects, _) = model.chat.pending_effects.drain_runtime();
        let submissions: Vec<_> = effects
            .into_iter()
            .filter_map(|(_, effect)| {
                if let super::super::effects::TuiEffect::SubmitMessage { request } = effect {
                    Some(request)
                } else {
                    None
                }
            })
            .collect();
        assert_eq!(submissions.len(), 1);
        assert_eq!(submissions[0].message, "hello from the Send button");
        // A second release cannot submit an empty draft or duplicate the request.
        model.handle_basic_terminal_event(bmux_tui::event::Event::Mouse(
            bmux_tui::event::MouseEvent::new(
                bmux_tui::event::MouseEventKind::Up(bmux_tui::event::MouseButton::Left),
                position,
            ),
        ));
        let (effects, _) = model.chat.pending_effects.drain_runtime();
        drop(model);
        assert!(!effects.iter().any(|(_, effect)| matches!(
            effect,
            super::super::effects::TuiEffect::SubmitMessage { .. }
        )));
    }

    #[tokio::test]
    async fn submission_menu_hover_damages_only_the_popup_and_repeated_hover_is_idle() {
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui_runtime::Presenter;
        let mut model = root_test_model();
        model.chat.app.replace_composer_with("hello");
        let mut output = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut output, Rect::new(0, 0, 60, 15));
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        let arrow = model
            .committed_hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "composer.send.more")
            .unwrap()
            .area;
        for kind in [
            MouseEventKind::Down(MouseButton::Left),
            MouseEventKind::Up(MouseButton::Left),
        ] {
            model.handle_basic_terminal_event(Event::Mouse(MouseEvent::new(
                kind,
                Point::new(arrow.x, arrow.y),
            )));
        }
        model.invalidation = super::super::invalidation::UiInvalidation::Structural;
        model.presentation_damage = bmux_tui::damage::Damage::Full;
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .unwrap();
        let popup = model
            .committed_hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "composer.send.menu.bounds")
            .unwrap()
            .area;
        let item = model
            .committed_hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "composer.send.menu.queue")
            .unwrap()
            .area;
        let event = Event::Mouse(MouseEvent::new(
            MouseEventKind::Move,
            Point::new(item.x, item.y),
        ));
        let invalidation = model.handle_basic_terminal_event(event.clone());
        assert_eq!(
            invalidation,
            super::super::invalidation::UiInvalidation::Paint
        );
        let damage = model.select_presentation_damage(invalidation, &[]);
        assert!(matches!(damage, bmux_tui::damage::Damage::Regions(_)));
        assert_eq!(
            *model.chat.app.composer_submit.pointer_damage.borrow(),
            [arrow, popup]
        );
        assert_eq!(
            model.handle_basic_terminal_event(event),
            super::super::invalidation::UiInvalidation::None
        );
        drop(model);
    }

    #[tokio::test]
    async fn root_submission_queues_runtime_effect_work() {
        let mut model = {
            let mut chat = root_test_chat();
            chat.app.replace_composer_with("hello root runtime");
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };
        model.chat.app.stage_submission();

        model.stage_root_submission(bcode_ipc::PromptPlacement::Steering);

        assert_eq!(model.chat.queued_effect_count(), 1);
        assert_eq!(model.chat.app.composer().text(), "");
        drop(model);
    }

    #[tokio::test]
    async fn root_screen_tracks_owned_routes_and_returns_to_chat() {
        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };

        model.loop_state.open_command_palette(&mut model.chat);
        assert!(model.synchronize_screen().needs_render());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::CommandPalette);

        let escape = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Escape,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _damage = model.route_terminal_event(escape);
        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        drop(model);
    }

    #[tokio::test]
    async fn session_scope_keyboard_and_measured_mouse_controls_replace_loads() {
        use super::super::effects::TuiEffect;
        use bmux_tui::event::{Event, MouseButton, MouseEvent, MouseEventKind};
        use bmux_tui::geometry::{Point, Rect};
        use bmux_tui_runtime::Presenter as _;
        let _scope_guard = super::super::session_picker::SCOPE_TEST_LOCK
            .lock()
            .unwrap();
        let mut model = root_test_model();
        model.loop_state.open_session_picker(&mut model.chat);
        model
            .loop_state
            .handle_session_catalog_control(&mut model.chat, "session.scope.local");
        let _initial = model.chat.pending_effects.drain_runtime();
        let _damage = model.route_terminal_event(Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Tab,
            modifiers: bmux_keyboard::Modifiers::NONE,
        }));
        let (effects, _) = model.chat.pending_effects.drain_runtime();
        assert!(effects.iter().any(|(_, effect)| matches!(
            effect,
            TuiEffect::LoadSessionPicker {
                scope: bcode_session_models::SessionDiscoveryScope::AllWorkingDirectories,
                ..
            }
        )));
        let mut bytes = Vec::new();
        let mut terminal = bmux_tui::terminal::Terminal::new(&mut bytes, Rect::new(0, 0, 100, 24));
        let _invalidation = model.synchronize_screen();
        super::bcode_runtime_presenter(&mut terminal)
            .present(&mut model)
            .expect("picker");
        let hit = model
            .committed_hits
            .regions()
            .iter()
            .find(|hit| hit.id.as_str() == "session.scope.local")
            .expect("local control");
        let point = Point::new(hit.area.x, hit.area.y);
        let _damage = model.route_terminal_event(Event::Mouse(MouseEvent::new(
            MouseEventKind::Down(MouseButton::Left),
            point,
        )));
        let (effects, _) = model.chat.pending_effects.drain_runtime();
        assert!(effects.iter().any(|(_, effect)| matches!(
            effect,
            TuiEffect::LoadSessionPicker {
                scope: bcode_session_models::SessionDiscoveryScope::WorkingDirectory(_),
                ..
            }
        )));
        drop(model);
    }

    #[tokio::test]
    async fn root_session_picker_is_a_retained_route_and_escape_returns_to_chat() {
        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };

        model.loop_state.open_session_picker(&mut model.chat);
        assert!(model.synchronize_screen().needs_render());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::SessionPicker);
        let escape = bmux_tui::event::Event::Key(bmux_keyboard::KeyStroke {
            key: bmux_keyboard::KeyCode::Escape,
            modifiers: bmux_keyboard::Modifiers::NONE,
        });
        let _damage = model.route_terminal_event(escape);

        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        assert!(!model.loop_state.has_session_picker());
        drop(model);
    }

    #[tokio::test]
    async fn root_plugin_session_navigation_retains_and_resumes_surface() {
        struct NavigationSurface {
            resumed: std::sync::Arc<std::sync::Mutex<Vec<bcode_session_models::SessionId>>>,
        }

        impl bcode_plugin_sdk::tui::PluginTuiSurface for NavigationSurface {
            fn id(&self) -> &'static str {
                "test-navigation"
            }

            fn title(&self) -> &'static str {
                "Test Navigation"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }

            fn session_navigation_finished(
                &mut self,
                session_id: bcode_session_models::SessionId,
                result: Result<(), String>,
            ) {
                result.expect("navigation succeeds");
                self.resumed.lock().expect("resumed lock").push(session_id);
            }
        }

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        let resumed = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        state.queue_root_plugin_surface(
            "test.plugin",
            Box::new(NavigationSurface {
                resumed: std::sync::Arc::clone(&resumed),
            }),
        );
        let session_id = bcode_session_models::SessionId::new();

        assert!(state.suspend_root_plugin_surface_for_session(session_id));
        assert!(state.root_plugin_surface_is_suspended());
        assert!(state.complete_root_plugin_session_navigation(session_id, Ok(())));
        assert!(!state.root_plugin_surface_is_suspended());
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("test-navigation")
        );
        assert_eq!(*resumed.lock().expect("resumed lock"), [session_id]);
    }

    #[tokio::test]
    async fn nested_plugin_surface_close_restores_parent_with_workflow_subscription() {
        struct TestSurface(&'static str);

        impl bcode_plugin_sdk::tui::PluginTuiSurface for TestSurface {
            fn id(&self) -> &'static str {
                self.0
            }

            fn title(&self) -> &'static str {
                self.0
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let client = bcode_client::BcodeClient::default_endpoint();
        let passive_client = client
            .clone()
            .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
        let mut state =
            super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
        state.queue_root_plugin_surface("bcode.workflow", Box::new(TestSurface("workflow.status")));
        let (_update_sender, updates) = tokio::sync::mpsc::channel(1);
        let (request_sender, mut requests) = tokio::sync::mpsc::channel(1);
        state.attach_root_plugin_surface_updates(updates, request_sender);

        state.queue_root_plugin_surface("bcode.workflow", Box::new(TestSurface("workflow.author")));
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("workflow.author")
        );
        let closed = state
            .close_root_plugin_surface_with_outcome(None)
            .expect("authoring surface closes");
        assert_eq!(closed.0, "bcode.workflow");
        assert_eq!(
            state.active_root_plugin_surface_id(),
            Some("workflow.status")
        );

        state.request_root_workflow_run("run-7".to_string());
        assert!(matches!(
            requests.try_recv(),
            Ok(super::super::chat_loop::WorkflowViewRequest::SelectRun(run_id))
                if run_id == "run-7"
        ));
    }

    #[tokio::test]
    async fn root_request_draft_handoff_waits_for_committed_paint() {
        let session_id = bcode_session_models::SessionId::new();
        let mut model = root_test_model();
        model.chat.mark_attached(session_id);
        model.chat.app = super::super::app::BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let draft = |revision, text: &str| {
            super::super::history_flow::SessionStreamUpdate::Event(Box::new(
                bcode_ipc::Event::SessionLive(bcode_session_models::SessionLiveEvent {
                    session_id,
                    kind: bcode_session_models::SessionLiveEventKind::ToolRequestDraft {
                        event: bcode_session_models::ToolRequestDraftEvent {
                            output_position: None,
                            turn_id: "turn-live".to_owned(),
                            tool_call_id: "call-write".to_owned(),
                            tool_name: "filesystem.write".to_owned(),
                            producer_plugin_id: Some("bcode.filesystem".to_owned()),
                            schema: "bcode.filesystem.request-draft.write".to_owned(),
                            schema_version: 1,
                            placement: bcode_session_models::ToolContributionPlacement::Request,
                            generation: 1,
                            revision,
                            operation:
                                bcode_session_models::ToolRequestDraftOperation::Checkpoint {
                                    start_offset: 0,
                                    text: text.to_owned(),
                                },
                            argument_bytes: text.len(),
                            truncated: false,
                        },
                    },
                }),
            ))
        };

        for update in [draft(1, ""), draft(2, r#"{"path":"live.txt""#)] {
            bmux_tui_runtime::Program::update(
                &mut model,
                bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::SessionStream(
                    Box::new(update),
                )),
            )
            .expect("root update");
        }
        let first = model.chat.app.session_view_snapshot().tools["call-write"]
            .request_draft
            .as_ref()
            .expect("first draft awaits presentation");
        assert_eq!(first.preview, "");
        assert_eq!(
            bmux_tui_runtime::Program::presentation_committed(
                &mut model,
                bmux_tui_runtime::PresentReport::default(),
            )
            .invalidation,
            bmux_tui_runtime::Invalidation::Redraw
        );
        let second = model.chat.app.session_view_snapshot().tools["call-write"]
            .request_draft
            .as_ref()
            .expect("deferred draft applied");
        assert_eq!(second.preview, r#"{"path":"live.txt""#);
    }

    #[tokio::test]
    async fn ralph_surface_effect_updates_root_screen_state() {
        struct RalphSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for RalphSurface {
            fn id(&self) -> &'static str {
                "ralph-home"
            }

            fn title(&self) -> &'static str {
                "Ralph"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            super::BcodeRuntimeModel::new(
                chat,
                settings,
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false),
            )
        };
        bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Message(super::BcodeRuntimeMessage::EffectCompleted(
                Box::new(
                    super::super::effects::TuiEffectResult::PluginSurfaceOpened {
                        plugin_id: "bcode.ralph".to_owned(),
                        result: Ok(Box::new(RalphSurface)),
                    },
                ),
            )),
        )
        .expect("root update");

        assert!(model.has_plugin_surface());
        assert_eq!(model.screen, super::BcodeRuntimeScreen::PluginSurface);
        drop(model);
    }

    #[tokio::test]
    async fn temporary_plugin_navigation_restores_root_route() {
        struct NavigationSurface;

        impl bcode_plugin_sdk::tui::PluginTuiSurface for NavigationSurface {
            fn id(&self) -> &'static str {
                "test-navigation"
            }

            fn title(&self) -> &'static str {
                "Test Navigation"
            }

            fn render(
                &mut self,
                _area: bmux_tui::geometry::Rect,
                _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
            ) {
            }

            fn handle_event(
                &mut self,
                _event: &bmux_tui::event::Event,
                _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
            ) -> bcode_plugin_sdk::tui::PluginTuiAction {
                bcode_plugin_sdk::tui::PluginTuiAction::None
            }
        }

        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };
        model.queue_plugin_surface("test.plugin", Box::new(NavigationSurface));
        let session_id = bcode_session_models::SessionId::new();

        model.suspend_screen_for_session_navigation();
        assert!(
            model
                .loop_state
                .suspend_root_plugin_surface_for_session(session_id)
        );
        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        model.finish_plugin_session_attach(session_id, Ok(()));
        let _damage = model.synchronize_screen();
        assert_eq!(model.screen, super::BcodeRuntimeScreen::Chat);
        assert!(model.loop_state.root_plugin_surface_is_suspended());
        assert!(model.loop_state.root_plugin_session_is_attached());
        assert!(!model.chat.app.should_exit());
        let _update = bmux_tui_runtime::Program::update(
            &mut model,
            bmux_tui_runtime::RuntimeEvent::Terminal(bmux_tui::event::Event::Key(
                bmux_keyboard::KeyStroke {
                    key: bmux_keyboard::KeyCode::Char('d'),
                    modifiers: bmux_keyboard::Modifiers {
                        ctrl: true,
                        ..bmux_keyboard::Modifiers::NONE
                    },
                },
            )),
        )
        .expect("borrowed chat return is admitted without setting exit");

        assert_eq!(model.screen, super::BcodeRuntimeScreen::PluginSurface);
        assert!(!model.loop_state.root_plugin_surface_is_suspended());
        assert!(!model.loop_state.root_plugin_session_is_attached());
        assert!(!model.chat.app.should_exit());
        drop(model);
    }

    struct ShutdownSurface {
        release: Option<tokio::sync::oneshot::Receiver<()>>,
        commits: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl bcode_plugin_sdk::tui::PluginTuiSurface for ShutdownSurface {
        fn id(&self) -> &'static str {
            "shutdown-test"
        }
        fn title(&self) -> &'static str {
            "Shutdown"
        }
        fn render(
            &mut self,
            _area: bmux_tui::geometry::Rect,
            _frame: &mut bmux_tui::paint::PaintCx<'_, '_>,
        ) {
        }
        fn handle_event(
            &mut self,
            _event: &bmux_tui::event::Event,
            _host: &dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> bcode_plugin_sdk::tui::PluginTuiAction {
            bcode_plugin_sdk::tui::PluginTuiAction::None
        }
        fn drain_effects<'a>(
            &'a mut self,
            _host: &'a dyn bcode_plugin_sdk::tui::PluginTuiHost,
        ) -> std::pin::Pin<
            Box<
                dyn std::future::Future<Output = bcode_plugin_sdk::tui::PluginTuiAction>
                    + Send
                    + 'a,
            >,
        > {
            Box::pin(async move {
                self.release
                    .take()
                    .expect("accepted operation")
                    .await
                    .expect("release");
                self.commits
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                bcode_plugin_sdk::tui::PluginTuiAction::Close {
                    outcome: Some(serde_json::json!({"settled": true})),
                }
            })
        }
    }

    #[test]
    fn runtime_finalization_retains_all_failures_with_settlement_precedence() {
        for cleanup in [false, true] {
            for settlement in [false, true] {
                let error = super::runtime_finalization_error(
                    std::io::Error::other("primary runtime failure").into(),
                    cleanup.then(|| std::io::Error::other("cleanup failure").into()),
                    settlement.then(|| std::io::Error::other("settlement failure").into()),
                );
                let diagnostic = error.to_string();
                assert!(diagnostic.contains("primary runtime failure"));
                assert_eq!(diagnostic.contains("cleanup failure"), cleanup);
                assert_eq!(diagnostic.contains("settlement failure"), settlement);
                if cleanup || settlement {
                    let super::TuiError::Io(error) = &error else {
                        panic!("combined error carrier")
                    };
                    let combined = error
                        .get_ref()
                        .unwrap()
                        .downcast_ref::<super::RuntimeFinalizationError>()
                        .unwrap();
                    assert!(combined.primary.to_string().contains(if settlement {
                        "settlement failure"
                    } else {
                        "primary runtime failure"
                    }));
                    assert_eq!(combined.runtime_error.is_some(), settlement);
                    assert_eq!(combined.cleanup_error.is_some(), cleanup);
                    assert!(std::error::Error::source(combined).is_some());
                } else {
                    let super::TuiError::Io(error) = error else {
                        panic!("original I/O error")
                    };
                    assert_eq!(error.to_string(), "primary runtime failure");
                }
            }
        }
    }

    fn assert_root_shutdown_result(
        exit: &str,
        result: Result<super::BcodeRuntimeModel, crate::TuiError>,
    ) {
        match result {
            Ok(mut model) => {
                assert_eq!(exit, "normal");
                assert_eq!(
                    model.take_plugin_surface_result(),
                    Some((
                        "test".to_string(),
                        Some(serde_json::json!({"settled": true}))
                    ))
                );
                drop(model);
            }
            Err(error) => {
                assert_ne!(exit, "normal");
                let diagnostic = error.to_string();
                if exit == "cleanup" {
                    assert!(diagnostic.contains("original failure"));
                    assert!(diagnostic.contains("restoration failure"));
                } else {
                    assert!(diagnostic.contains(if exit.starts_with("program") {
                        "terminal input failed"
                    } else {
                        "terminal writer failed"
                    }));
                    assert_eq!(
                        diagnostic.contains("terminal restoration failed"),
                        exit.ends_with("cleanup")
                    );
                }
                drop(error);
            }
        }
    }

    #[tokio::test]
    async fn root_shutdown_waits_for_plugin_results_on_normal_and_fatal_exit() {
        for exit in [
            "normal",
            "cleanup",
            "program",
            "presenter",
            "program_cleanup",
            "presenter_cleanup",
        ] {
            let commits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let (release, receiver) = tokio::sync::oneshot::channel();
            let mut outcome = Some({
                let mut retained_model = Some(root_test_model());
                {
                    let model = retained_model.as_mut().expect("fixture model available");
                    model.queue_plugin_surface(
                        "test",
                        Box::new(ShutdownSurface {
                            release: Some(receiver),
                            commits: commits.clone(),
                        }),
                    );
                    let client = bcode_client::BcodeClient::default_endpoint();
                    let launch = model.fresh_session_settings();
                    let _action = model.loop_state.poll_root_plugin_surface(&client, launch);
                }
                let output = bmux_tui_runtime::RuntimeOutput {
                    program: retained_model
                        .take()
                        .expect("fixture model transfers to runtime"),
                    presenter: (),
                    stats: bmux_tui_runtime::RuntimeStats::default(),
                };
                drop(retained_model);
                match exit {
                    "program" | "program_cleanup" => Err(bmux_tui_runtime::RuntimeError::Program {
                        error: std::io::Error::other("terminal input failed").into(),
                        cleanup_error: (exit == "program_cleanup")
                            .then(|| std::io::Error::other("terminal restoration failed").into()),
                        output: Box::new(output),
                    }),
                    "cleanup" => Err(bmux_tui_runtime::RuntimeError::Program {
                        error: std::io::Error::other("original failure").into(),
                        cleanup_error: Some(std::io::Error::other("restoration failure").into()),
                        output: Box::new(output),
                    }),
                    "presenter" | "presenter_cleanup" => {
                        Err(bmux_tui_runtime::RuntimeError::Presenter {
                            error: std::io::Error::other("terminal writer failed").into(),
                            cleanup_error: (exit == "presenter_cleanup").then(|| {
                                std::io::Error::other("terminal restoration failed").into()
                            }),
                            output: Box::new(output),
                        })
                    }
                    _ => Ok(output),
                }
            });
            let finish = super::finish_runtime_result(
                outcome
                    .take()
                    .expect("runtime outcome transfers to settlement"),
            );
            drop(outcome);
            tokio::pin!(finish);
            assert!(
                tokio::time::timeout(std::time::Duration::from_millis(10), &mut finish)
                    .await
                    .is_err()
            );
            assert_eq!(commits.load(std::sync::atomic::Ordering::SeqCst), 0);
            release.send(()).expect("accepted operation still owned");
            let mut retained_result = Some(finish.await);
            assert_eq!(commits.load(std::sync::atomic::Ordering::SeqCst), 1);
            let result = retained_result.take().expect("outcome transfers once");
            drop(retained_result);
            assert_root_shutdown_result(exit, result);
        }
    }

    #[tokio::test]
    async fn standalone_plugin_surface_close_exits_with_owned_outcome() {
        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };
        let expected = serde_json::json!({"selected": "workspace"});
        model.exit_after_plugin_surface = true;

        let invalidation = model.apply_plugin_surface_close("test.plugin", Some(expected.clone()));

        assert!(invalidation.needs_render());
        assert!(model.exit_requested);
        assert_eq!(
            model.take_plugin_surface_result(),
            Some(("test.plugin".to_owned(), Some(expected)))
        );
        drop(model);
    }

    #[tokio::test]
    async fn root_ralph_local_action_returns_to_chat_without_generic_outcome_decode() {
        let mut model = {
            let chat = root_test_chat();
            let settings = super::super::chat_loop::TuiRuntimeSettings::bootstrap(
                std::path::PathBuf::from("."),
                &[],
            );
            let client = bcode_client::BcodeClient::default_endpoint();
            let passive_client = client
                .clone()
                .with_daemon_availability(bcode_client::DaemonAvailability::RequireRunning);
            let loop_state =
                super::super::chat_loop::ChatLoopState::new(&client, &passive_client, false);
            super::BcodeRuntimeModel::new(chat, settings, loop_state)
        };

        let invalidation = model.apply_plugin_surface_close(
            "bcode.ralph",
            Some(serde_json::json!({"ralph_action": "open"})),
        );

        assert!(invalidation.needs_render());
        assert!(model.chat.app.status().contains("no Ralph loops"));
        drop(model);
    }

    #[derive(Default)]
    struct LatencyAcceptanceProgram {
        application_messages: usize,
        terminal_latency: Option<std::time::Duration>,
        timer_latency: Option<std::time::Duration>,
        probe_started: std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>,
        runtime_active: std::sync::Arc<tokio::sync::Notify>,
    }

    #[derive(Default)]
    struct CommittedLatencyProgram;

    impl bmux_tui_runtime::Program for CommittedLatencyProgram {
        type Message = ();
        type Error = Infallible;

        fn update(
            &mut self,
            _event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            Ok(bmux_tui_runtime::Update::redraw().merge(bmux_tui_runtime::Update::exit()))
        }
    }

    struct CommittedLatencyPresenter {
        admitted_at: std::time::Instant,
        committed_latency: std::sync::Arc<std::sync::Mutex<Option<std::time::Duration>>>,
    }

    impl bmux_tui_runtime::Presenter<CommittedLatencyProgram> for CommittedLatencyPresenter {
        type Error = Infallible;

        fn reset(&mut self, _reason: bmux_tui_runtime::ResetReason) {}

        fn present(
            &mut self,
            _program: &mut CommittedLatencyProgram,
        ) -> Result<bmux_tui_runtime::PresentReport, Self::Error> {
            *self.committed_latency.lock().expect("latency lock") =
                Some(self.admitted_at.elapsed());
            Ok(bmux_tui_runtime::PresentReport {
                changed_cells: 1,
                full_repaint: false,
            })
        }
    }

    async fn committed_latency_sample(terminal: bool) -> std::time::Duration {
        let committed_latency = std::sync::Arc::new(std::sync::Mutex::new(None));
        let admitted_at = std::time::Instant::now();
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            CommittedLatencyProgram,
            CommittedLatencyPresenter {
                admitted_at,
                committed_latency: std::sync::Arc::clone(&committed_latency),
            },
            bmux_tui_runtime::RuntimeConfig {
                frame_interval: None,
                messages_per_turn: 1,
                ..bmux_tui_runtime::RuntimeConfig::default()
            },
        );
        if terminal {
            handle
                .try_send_terminal(bmux_tui::event::Event::Tick)
                .expect("terminal admitted");
        } else {
            handle.schedule_timer(
                bmux_tui_runtime::TimerId::new("product-latency-probe"),
                admitted_at,
            );
        }
        runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("latency probe runtime succeeds"));
        let latency = *committed_latency.lock().expect("latency lock");
        latency.expect("presentation committed")
    }

    fn latency_summary(samples: &[std::time::Duration]) -> serde_json::Value {
        let mut values = samples
            .iter()
            .map(|duration| duration.as_secs_f64() * 1_000.0)
            .collect::<Vec<_>>();
        values.sort_by(f64::total_cmp);
        let percentile = |percent: usize| {
            let rank = values.len().saturating_mul(percent).div_ceil(100);
            values[rank.saturating_sub(1).min(values.len() - 1)]
        };
        serde_json::json!({
            "samples_ms": values,
            "min_ms": values[0],
            "p50_ms": percentile(50),
            "p95_ms": percentile(95),
            "p99_ms": percentile(99),
            "max_ms": values[values.len() - 1],
        })
    }

    #[tokio::test]
    #[ignore = "manual already-built product latency distribution probe"]
    async fn product_input_and_timer_to_committed_presentation_latency_report() {
        const SAMPLES: usize = 50;
        const LOCKED_P99_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
        let mut terminal_samples = Vec::with_capacity(SAMPLES);
        let mut timer_samples = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            terminal_samples.push(committed_latency_sample(true).await);
            timer_samples.push(committed_latency_sample(false).await);
        }
        let terminal = latency_summary(&terminal_samples);
        let timer = latency_summary(&timer_samples);
        println!(
            "BCODE_PERF_CASE {}",
            serde_json::json!({
                "domain": "product_committed_presentation_latency",
                "profile": "test",
                "sample_count": SAMPLES,
                "locked_p99_budget_ms": LOCKED_P99_BUDGET.as_millis(),
                "terminal": terminal,
                "timer": timer,
            })
        );
        for (name, samples) in [("terminal", terminal_samples), ("timer", timer_samples)] {
            let mut samples = samples;
            samples.sort_unstable();
            let p99 = samples[SAMPLES.saturating_mul(99).div_ceil(100) - 1];
            assert!(
                p99 <= LOCKED_P99_BUDGET,
                "{name} p99 {p99:?} exceeded {LOCKED_P99_BUDGET:?}"
            );
        }
    }

    impl bmux_tui_runtime::Program for LatencyAcceptanceProgram {
        type Message = u64;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            self.runtime_active.notify_one();
            let probe_started = *self.probe_started.lock().expect("probe lock");
            match event {
                bmux_tui_runtime::RuntimeEvent::Message(_) => {
                    self.application_messages = self.application_messages.saturating_add(1);
                }
                bmux_tui_runtime::RuntimeEvent::Terminal(_) => {
                    self.terminal_latency = probe_started.map(|started| started.elapsed());
                }
                bmux_tui_runtime::RuntimeEvent::Timer(_) => {
                    self.timer_latency = probe_started.map(|started| started.elapsed());
                }
            }
            Ok(
                if self.application_messages == 10_000
                    && self.terminal_latency.is_some()
                    && self.timer_latency.is_some()
                {
                    bmux_tui_runtime::Update::exit()
                } else {
                    bmux_tui_runtime::Update::none()
                },
            )
        }
    }

    #[tokio::test]
    #[ignore = "run directly outside parallel test-suite contention via capture-tui-product-latency.sh"]
    async fn terminal_and_timer_latency_stay_within_flood_acceptance_budget() {
        const ACCEPTANCE_BUDGET: std::time::Duration = std::time::Duration::from_millis(100);
        let probe_started = std::sync::Arc::new(std::sync::Mutex::new(None));
        let runtime_active = std::sync::Arc::new(tokio::sync::Notify::new());
        let config = bmux_tui_runtime::RuntimeConfig {
            reliable_capacity: 20_000,
            messages_per_turn: 4,
            processing_time_per_turn: std::time::Duration::from_millis(1),
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            LatencyAcceptanceProgram {
                probe_started: std::sync::Arc::clone(&probe_started),
                runtime_active: std::sync::Arc::clone(&runtime_active),
                ..LatencyAcceptanceProgram::default()
            },
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        for value in 0..10_000 {
            handle.try_send(value).expect("configured flood fits");
        }
        let runtime_task = tokio::spawn(runtime.run());
        runtime_active.notified().await;
        *probe_started.lock().expect("probe lock") = Some(std::time::Instant::now());
        handle
            .try_send_terminal(bmux_tui::event::Event::Tick)
            .expect("terminal admission remains independent");
        handle.schedule_timer(
            bmux_tui_runtime::TimerId::new("acceptance-latency"),
            std::time::Instant::now(),
        );

        let output = runtime_task
            .await
            .expect("runtime task joins")
            .unwrap_or_else(|_| panic!("runtime acceptance succeeds"));
        let terminal_latency = output.program.terminal_latency.expect("terminal delivered");
        let timer_latency = output.program.timer_latency.expect("timer delivered");
        assert!(
            terminal_latency <= ACCEPTANCE_BUDGET,
            "terminal latency {terminal_latency:?} exceeded {ACCEPTANCE_BUDGET:?}"
        );
        assert!(
            timer_latency <= ACCEPTANCE_BUDGET,
            "timer latency {timer_latency:?} exceeded {ACCEPTANCE_BUDGET:?}"
        );
    }

    #[derive(Default)]
    struct AdmissionProgram {
        received: usize,
    }

    impl bmux_tui_runtime::Program for AdmissionProgram {
        type Message = BcodeRuntimeMessage;
        type Error = Infallible;

        fn update(
            &mut self,
            event: bmux_tui_runtime::RuntimeEvent<Self::Message>,
        ) -> Result<bmux_tui_runtime::Update<Self::Message>, Self::Error> {
            if matches!(event, bmux_tui_runtime::RuntimeEvent::Message(_)) {
                self.received += 1;
            }
            Ok(if self.received == 3 {
                bmux_tui_runtime::Update::exit()
            } else {
                bmux_tui_runtime::Update::none()
            })
        }
    }

    #[tokio::test]
    async fn streaming_configurator_apply_cancel_and_reset_own_input_and_effects() {
        let mut apply = root_test_model();
        let apply_keymap =
            super::super::keymap::BmuxKeyMap::from_config(apply.chat.app.tui_config());
        apply
            .loop_state
            .open_streaming_configurator(&mut apply.chat);
        assert!(apply.loop_state.handle_streaming_configurator_key(
            &mut apply.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('x')),
            &apply_keymap,
        ));
        assert!(apply.loop_state.handle_streaming_configurator_key(
            &mut apply.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
            &apply_keymap,
        ));
        assert!(apply.chat.pending_effects.contains_effect(
            &super::super::effects::TuiEffect::PersistStreamingPresentation {
                policy: bcode_session_view_models::StreamingPresentationPolicy::default(),
            },
        ));
        assert!(
            apply
                .loop_state
                .streaming_configurator_deadline(std::time::Instant::now())
                .is_none()
        );
        drop(apply);

        let mut cancel = root_test_model();
        let cancel_keymap =
            super::super::keymap::BmuxKeyMap::from_config(cancel.chat.app.tui_config());
        cancel
            .loop_state
            .open_streaming_configurator(&mut cancel.chat);
        assert!(cancel.loop_state.handle_streaming_configurator_key(
            &mut cancel.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Escape),
            &cancel_keymap,
        ));
        assert_eq!(cancel.chat.queued_effect_count(), 0);
        drop(cancel);

        let mut reset = root_test_model();
        let reset_keymap =
            super::super::keymap::BmuxKeyMap::from_config(reset.chat.app.tui_config());
        reset
            .loop_state
            .open_streaming_configurator(&mut reset.chat);
        for _ in 0..4 {
            assert!(reset.loop_state.handle_streaming_configurator_key(
                &mut reset.chat,
                bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Down),
                &reset_keymap,
            ));
        }
        assert!(reset.loop_state.handle_streaming_configurator_key(
            &mut reset.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Space),
            &reset_keymap,
        ));
        assert!(reset.loop_state.handle_streaming_configurator_key(
            &mut reset.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Enter),
            &reset_keymap,
        ));
        assert!(
            reset
                .chat
                .pending_effects
                .contains_effect(&super::super::effects::TuiEffect::ClearStreamingPresentation,)
        );
        drop(reset);
    }

    #[tokio::test]
    async fn streaming_configurator_deadline_stops_on_pause_and_close() {
        let mut model = root_test_model();
        let model_keymap =
            super::super::keymap::BmuxKeyMap::from_config(model.chat.app.tui_config());
        model
            .loop_state
            .open_streaming_configurator(&mut model.chat);
        let now = std::time::Instant::now();
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_some()
        );
        assert!(model.loop_state.handle_streaming_configurator_key(
            &mut model.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Char('p')),
            &model_keymap,
        ));
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_none()
        );
        assert!(model.loop_state.handle_streaming_configurator_key(
            &mut model.chat,
            bmux_keyboard::KeyStroke::simple(bmux_keyboard::KeyCode::Escape),
            &model_keymap,
        ));
        assert!(
            model
                .loop_state
                .streaming_configurator_deadline(now)
                .is_none()
        );
        drop(model);
    }

    #[tokio::test]
    async fn domain_owned_admission_separates_reliable_and_latest_messages() {
        let config = bmux_tui_runtime::RuntimeConfig {
            frame_interval: None,
            ..bmux_tui_runtime::RuntimeConfig::default()
        };
        let (runtime, handle) = bmux_tui_runtime::Runtime::new(
            AdmissionProgram::default(),
            bmux_tui_runtime::HeadlessPresenter::default(),
            config,
        );
        admit(&handle, BcodeRuntimeMessage::DraftSaveDue)
            .await
            .expect("reliable message admitted");
        admit(&handle, BcodeRuntimeMessage::StreamingPresentationDue)
            .await
            .expect("latest message admitted");
        admit(&handle, BcodeRuntimeMessage::StreamingConfiguratorDue)
            .await
            .expect("configurator latest message admitted");
        let output = runtime
            .run()
            .await
            .unwrap_or_else(|_| panic!("runtime succeeds"));
        assert_eq!(output.program.received, 3);
        assert_eq!(output.stats.reliable_processed, 1);
        assert_eq!(output.stats.latest_processed, 2);
        assert_ne!(
            BcodeRuntimeAdmissionError::Full,
            BcodeRuntimeAdmissionError::Closed
        );
    }
}
