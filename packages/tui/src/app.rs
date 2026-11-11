//! TUI app state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

#[cfg(test)]
use bcode_config::TuiThemeConfig;
use bcode_config::{
    TuiAccentTransitionCurve, TuiConfig, TuiDiffViewerConfig, TuiDiffViewerLayout,
    TuiThinkingConfig,
};
use bcode_plugin_sdk::path::display;
#[cfg(test)]
use bcode_session_models::ModelTurnOutcome;
use bcode_session_models::{
    ProviderStreamEvent, SessionEvent, SessionEventKind, SessionId, SessionInputHistoryEntry,
    SessionLiveEvent, SessionLiveEventKind, SessionTraceEvent, SessionTracePayload,
    SessionTracePhase, ToolInvocationResult,
};
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::MouseEvent;
use bmux_tui::geometry::Rect;
use bmux_tui::style::Color;
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

use super::activity::{ActivityState, model_turn_outcome_label, runtime_work_detail};
use super::cursor_blink::CursorBlink;
use super::exit_state::ExitState;
use super::input_history::{InputHistory, InputHistoryOutcome};
use super::invalidation::{InvalidationKey, InvalidationRequest, UiInvalidation};
use super::keymap::{BmuxAction, BmuxKeyActivation, BmuxKeyBinding, BmuxScope};
use super::pending_submission::PendingSubmission;
use super::pending_submissions::PendingSubmissions;
use super::theme::definition::ResolvedThemeDefinition;
use super::theme::discovery::DiscoveredThemes;
use super::theme::{PresentedTheme, ResolvedTheme};
use super::timeline_dialog::TimelineEntry;
use super::transcript_resident_window::{TranscriptResidentWindow, TranscriptWindowPolicy};

const THEME_TRANSITION_INVALIDATION_KEY: &str = "theme-transition";
const ACTIVITY_ANIMATION_INVALIDATION_KEY: &str = "activity-animation";
const THEME_TRANSITION_FRAME: Duration = Duration::from_millis(16);
const RESIDENT_TRANSCRIPT_MAX_EVENTS: usize = 1_024;
const RESIDENT_TRANSCRIPT_TARGET_EVENTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PendingKeyActivation {
    scope: BmuxScope,
    stroke: bmux_keyboard::KeyStroke,
    action: BmuxAction,
    taps: u8,
    expires_at: Instant,
}

/// Result of evaluating key activation behavior.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyActivationOutcome {
    /// The binding action should run now.
    Activated(BmuxAction),
    /// More taps are required before the action should run.
    Pending,
}

/// Terminal presentation region affected by one timed invalidation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum TemporalDamage {
    /// Composer panel, including cursor paint.
    Composer,
    /// Status/activity chrome.
    Status,
    /// Unknown or global presentation region.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ThemeTransitionState {
    displayed_accent: Color,
    source_accent: Color,
    target_accent: Color,
    started_at: Instant,
    duration: Duration,
    curve: TuiAccentTransitionCurve,
    next_frame_at: Option<Instant>,
}

impl ThemeTransitionState {
    const fn new(accent: Color, now: Instant) -> Self {
        Self {
            displayed_accent: accent,
            source_accent: accent,
            target_accent: accent,
            started_at: now,
            duration: Duration::ZERO,
            curve: TuiAccentTransitionCurve::EaseOut,
            next_frame_at: None,
        }
    }

    /// Retarget the accent; a zero `duration` (motion `none`, `reduce_motion`,
    /// or `accent_transition = "immediate"`) applies it without scheduling
    /// frames and settles any transition already running.
    fn set_target(
        &mut self,
        target: Color,
        duration: Duration,
        curve: TuiAccentTransitionCurve,
        now: Instant,
    ) {
        if self.target_accent == target {
            if duration.is_zero() {
                self.finish();
            } else {
                self.displayed_accent = self.accent_at(now);
            }
            return;
        }
        if duration.is_zero() {
            self.displayed_accent = target;
            self.source_accent = target;
            self.target_accent = target;
            self.started_at = now;
            self.duration = Duration::ZERO;
            self.curve = curve;
            self.next_frame_at = None;
            return;
        }
        self.source_accent = self.accent_at(now);
        self.displayed_accent = self.source_accent;
        self.target_accent = target;
        self.started_at = now;
        self.duration = duration;
        self.curve = curve;
        self.next_frame_at = Some(now + THEME_TRANSITION_FRAME);
    }

    fn accent_at(&self, now: Instant) -> Color {
        if self.duration.is_zero() {
            return self.target_accent;
        }
        let duration_ms = u64::try_from(self.duration.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        let elapsed_ms = u64::try_from(now.saturating_duration_since(self.started_at).as_millis())
            .unwrap_or(u64::MAX)
            .min(duration_ms);
        if elapsed_ms >= duration_ms {
            return self.target_accent;
        }
        interpolate_color(
            self.source_accent,
            self.target_accent,
            elapsed_ms,
            duration_ms,
            self.curve,
        )
    }

    fn update(&mut self, now: Instant) -> Color {
        self.displayed_accent = self.accent_at(now);
        if self.is_active(now) {
            let elapsed = now.saturating_duration_since(self.started_at);
            let frame_count = elapsed.as_nanos() / THEME_TRANSITION_FRAME.as_nanos().max(1);
            let next_offset = THEME_TRANSITION_FRAME
                .as_nanos()
                .saturating_mul(frame_count.saturating_add(1));
            self.next_frame_at = Some(
                self.started_at
                    + Duration::from_nanos(u64::try_from(next_offset).unwrap_or(u64::MAX)),
            );
        } else {
            self.next_frame_at = None;
        }
        self.displayed_accent
    }

    fn is_active(&self, now: Instant) -> bool {
        !self.duration.is_zero()
            && now.saturating_duration_since(self.started_at) < self.duration
            && self.displayed_accent != self.target_accent
    }

    const fn finish(&mut self) {
        self.displayed_accent = self.target_accent;
        self.source_accent = self.target_accent;
        self.duration = Duration::ZERO;
        self.next_frame_at = None;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AgentMetadataHydration {
    Pending,
    Hydrated,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReasoningSupport {
    Unsupported,
    Supported,
}

impl ReasoningSupport {
    const fn is_supported(self) -> bool {
        matches!(self, Self::Supported)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingReasoningEffort {
    value: String,
    generation: u64,
}

#[derive(Debug, Clone)]
struct ChildWatchPresentation {
    windows: Vec<bcode_session_models::SessionChildQuery>,
    activity: Vec<bcode_session_models::SessionActivityTarget>,
    navigation: SessionId,
    relationships: BTreeMap<(SessionId, Option<SessionId>), Vec<SessionId>>,
}

impl Default for ChildWatchPresentation {
    fn default() -> Self {
        Self {
            windows: Vec::new(),
            activity: Vec::new(),
            navigation: SessionId::new(),
            relationships: BTreeMap::new(),
        }
    }
}

/// Captured native input and explicit placement retained by the composer.
#[derive(Debug, Clone, Default)]
pub struct ManagedComposerState {
    pub requests: std::collections::BTreeMap<String, bcode_session_models::ExternalHarnessRequest>,
    pub placement: Option<bcode_harness_models::HarnessPlacement>,
    execution: ManagedExecutionPolicy,
    settings: ManagedSessionSettings,
    /// Display name of the native executor, for example `Claude Code`.
    label: Option<String>,
}

/// Why a managed root asked to stop so its outer journey can continue without replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ManagedExitRequest {
    /// Reattach the same canonical owner after event continuity was lost.
    Reattach,
    /// The daemon no longer recognizes this owner's generation; recover, then reattach.
    Recover,
    /// Leave the managed conversation and open another session, or a new draft for `None`.
    Navigate(Option<SessionId>),
}

#[derive(Debug, Clone, Copy, Default)]
struct ManagedExecutionPolicy {
    session_id: Option<SessionId>,
    input_ready: bool,
    auth_requested: bool,
    exit_request: Option<ManagedExitRequest>,
}

/// Native settings acknowledged in this managed conversation (display only).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ManagedSessionSettings {
    /// Last acknowledged native permission mode, if changed in this attachment.
    pub permission_mode: Option<bcode_harness_models::HarnessPermissionMode>,
    /// Last acknowledged native model selector, if changed in this attachment.
    pub model_id: Option<String>,
    /// Model the native executor last reported in its usage evidence.
    pub observed_model: Option<String>,
}

/// State owned by the terminal user interface.
#[derive(Debug, Clone)]
pub struct BmuxApp {
    child_request_fence: crate::child_request_fence::ChildRequestFence,
    child_watch: ChildWatchPresentation,
    child_activity: Option<bcode_session_models::SessionActivityPage>,
    child_activity_error: Option<String>,
    child_panel: ChildPanelPresentation,
    child_panel_selected: usize,
    child_summaries: Vec<bcode_session_models::SessionSummary>,
    child_expanded: BTreeSet<bcode_session_view::navigation::SessionNavigationKey>,
    child_pages: BTreeMap<SessionId, bcode_session_models::SessionActivityPage>,
    child_cursors: BTreeMap<SessionId, Option<SessionId>>,
    session_id: Option<SessionId>,
    selected_auth_profile: Option<String>,
    selected_context_format_version: Option<u16>,
    selected_compatibility_key: Option<String>,
    current_agent_accent: Option<String>,
    pending_agent_id: Option<String>,
    pending_agent_accent: Option<String>,
    agent_metadata_hydration: AgentMetadataHydration,
    target_theme: ResolvedTheme,
    presented_theme: PresentedTheme,
    /// Effective manifest `[presentation]` declarations the plugin layers and
    /// target fields were last derived from; an unchanged catalog skips
    /// re-deriving and recompiling.
    plugin_presentation_declarations: BTreeMap<String, bcode_plugin::PluginPresentationDeclaration>,
    /// Renderer-neutral configured structural rules.
    configured_structure_rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    /// Interactive structural rules loaded from user state.
    user_structure_rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    /// Whether the terminal color query has been written.
    terminal_color_query_sent: bool,
    theme_catalog: Option<DiscoveredThemes>,
    theme_transition: ThemeTransitionState,
    theme_preview_id: Option<String>,
    thinking_label: String,
    reasoning_support: ReasoningSupport,
    reasoning_display_mode: bcode_config::TuiThinkingMode,
    reasoning_effort_values: Vec<String>,
    pending_reasoning_effort: Option<PendingReasoningEffort>,
    reasoning_effort_generation: u64,
    reasoning_default_effort: Option<String>,
    reasoning_default_summary: Option<String>,
    token_usage: TokenUsageMeter,
    /// Composer editor state, including source selection and scrolling.
    composer_input: TextInputState,
    pub(crate) composer_submit: super::composer_submit::ComposerSubmit,
    pub(crate) managed_composer: ManagedComposerState,
    input_history: InputHistory,
    /// Local presentation of an exact prepared recovery selection.
    pub queue_recovery: QueueRecoveryPresentation,
    /// Process-local transcript notices (errors, command output, plugin activity).
    notices: super::ephemeral_notices::EphemeralNotices,
    /// Per-tool default rules awaiting persistence to user state.
    pub(crate) pending_defaults: super::inspector::PendingDefaults,
    session_view: bcode_session_view::SessionView,
    transcript_window: TranscriptResidentWindow,
    transcript_selection_pinned: bool,
    latest_history_sequence: Option<u64>,
    pending_submissions: PendingSubmissions,
    activity_frame: usize,
    activity_elapsed: Duration,
    activity: ActivityState,
    activity_started_at: Instant,
    daemon_connection: DaemonConnectionState,
    execution_mode_indicator: Option<String>,
    #[cfg(test)]
    pending_external_override:
        Option<Vec<bcode_session_view_models::external::ExternalInteractionView>>,
    status: String,
    key_hints: String,
    tui_config: TuiConfig,
    /// Streaming-presentation policy last applied by the host shell (host configuration,
    /// shared by both transcript engines).
    streaming_presentation_policy: bcode_session_view_models::StreamingPresentationPolicy,
    effective_keymap: std::sync::Arc<bcode_plugin_sdk::tui::keymap::KeymapRegistry>,
    /// Advances on every effective keymap replacement (plugin visual key hints depend on it).
    effective_keymap_generation: u64,
    diff_viewer_layout_override: Option<TuiDiffViewerLayout>,
    exit: ExitState,
    cursor: CursorBlink,
    pending_key_activation: Option<PendingKeyActivation>,
    plugin_presentation: Option<Arc<crate::plugin_tui::PluginTuiPresentation>>,
    #[cfg(test)]
    observations: PresentationObservations,
}

/// Daemon connection state used to describe startup readiness in the status chrome.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DaemonConnectionState {
    /// The TUI has not yet completed a daemon-backed request.
    #[default]
    Connecting,
    /// The daemon is being started for an explicit foreground action.
    Starting,
    /// At least one daemon-backed request completed successfully.
    Connected,
    /// The daemon is intentionally offline/asleep while the TUI remains usable.
    IdleOffline,
    /// A daemon-backed startup request failed before any success was observed.
    Unavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct PermissionRequestInput<'a> {
    event_sequence: u64,
    permission_id: &'a str,
    tool_call_id: &'a str,
    tool_name: &'a str,
    arguments_json: &'a str,
    policy_source: Option<&'a str>,
    policy_reason: Option<&'a str>,
    application: SessionEventApplication,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SessionEventApplication {
    Replay,
    Live,
}

impl SessionEventApplication {
    const fn live_activity(self) -> bool {
        matches!(self, Self::Live)
    }
}

const fn reasoning_view_mode(
    mode: bcode_config::TuiThinkingMode,
) -> bcode_session_view_models::ReasoningDisplayMode {
    match mode {
        bcode_config::TuiThinkingMode::All => bcode_session_view_models::ReasoningDisplayMode::All,
        bcode_config::TuiThinkingMode::Summary => {
            bcode_session_view_models::ReasoningDisplayMode::Summary
        }
        bcode_config::TuiThinkingMode::Raw => bcode_session_view_models::ReasoningDisplayMode::Raw,
    }
}

#[cfg(test)]
#[derive(Debug, Clone, Default)]
struct PresentationObservations {
    theme_catalog_discovery_count: usize,
}

/// Ephemeral recovery confirmation presentation; never canonical execution state.
#[derive(Debug, Clone, Default)]
pub struct QueueRecoveryPresentation {
    /// Local fence invalidated by observations, commands and dismissal.
    pub generation: u64,
    /// Whether the exact settings fit the latest presentation.
    pub settings_visible: bool,
    /// Prepared selection awaiting explicit confirmation.
    pub confirmation: Option<bcode_session_models::SubmissionResumeConfirmation>,
}

impl BmuxApp {
    /// Create TUI state with replayed session data.
    #[must_use]
    pub fn new_with_history(
        session_id: Option<SessionId>,
        history: &[SessionEvent],
        input_history: &[SessionInputHistoryEntry],
    ) -> Self {
        Self::initial_session_state(session_id, input_history).with_initial_history(history)
    }

    fn initial_session_state(
        session_id: Option<SessionId>,
        input_history: &[SessionInputHistoryEntry],
    ) -> Self {
        let (now, initial_theme) = Self::initial_presentation();
        Self::initial_session_state_with_presentation(
            session_id,
            input_history,
            now,
            &initial_theme,
        )
    }

    fn initial_session_state_with_presentation(
        session_id: Option<SessionId>,
        input_history: &[SessionInputHistoryEntry],
        now: Instant,
        initial_theme: &ResolvedTheme,
    ) -> Self {
        Self {
            child_request_fence: crate::child_request_fence::ChildRequestFence::default(),
            child_watch: ChildWatchPresentation::default(),
            child_activity: None,
            child_activity_error: None,
            child_panel: ChildPanelPresentation::Hidden,
            child_panel_selected: 0,
            child_summaries: Vec::new(),
            child_expanded: BTreeSet::new(),
            child_pages: BTreeMap::new(),
            child_cursors: BTreeMap::new(),
            session_id,
            selected_auth_profile: None,
            selected_context_format_version: None,
            selected_compatibility_key: None,
            current_agent_accent: None,
            pending_agent_id: None,
            pending_agent_accent: None,
            agent_metadata_hydration: AgentMetadataHydration::Pending,
            target_theme: *initial_theme,
            presented_theme: (*initial_theme).into(),
            plugin_presentation_declarations: BTreeMap::new(),
            configured_structure_rules: Vec::new(),
            user_structure_rules: Vec::new(),
            terminal_color_query_sent: false,
            theme_catalog: None,
            theme_transition: ThemeTransitionState::new(initial_theme.accent, now),
            theme_preview_id: None,
            thinking_label: "reasoning output shown · unsupported".to_owned(),
            reasoning_support: ReasoningSupport::Unsupported,
            reasoning_display_mode: bcode_config::TuiThinkingMode::All,
            reasoning_effort_values: Vec::new(),
            pending_reasoning_effort: None,
            reasoning_effort_generation: 0,
            reasoning_default_effort: None,
            reasoning_default_summary: None,
            token_usage: TokenUsageMeter::default(),
            composer_input: TextInputState::new(TextEditBuffer::new()),
            composer_submit: super::composer_submit::ComposerSubmit::default(),
            managed_composer: ManagedComposerState::default(),
            input_history: InputHistory::from_entries(input_history),
            queue_recovery: QueueRecoveryPresentation::default(),
            notices: super::ephemeral_notices::EphemeralNotices::default(),
            pending_defaults: super::inspector::PendingDefaults::default(),
            session_view: bcode_session_view::SessionView::new(),
            transcript_window: TranscriptResidentWindow::default(),
            transcript_selection_pinned: false,
            latest_history_sequence: None,
            pending_submissions: PendingSubmissions::default(),
            activity_frame: 0,
            activity_elapsed: Duration::ZERO,
            activity: ActivityState::Idle,
            activity_started_at: now,
            daemon_connection: DaemonConnectionState::Connecting,
            execution_mode_indicator: None,
            #[cfg(test)]
            pending_external_override: None,
            status: String::from("Connecting to daemon…"),
            key_hints: String::new(),
            tui_config: TuiConfig::default(),
            // `SessionView` starts with immediate presentation until configuration applies one.
            streaming_presentation_policy:
                bcode_session_view_models::StreamingPresentationPolicy::immediate(),
            effective_keymap: std::sync::Arc::default(),
            effective_keymap_generation: 0,
            diff_viewer_layout_override: None,
            exit: ExitState::default(),
            cursor: CursorBlink::new(),
            pending_key_activation: None,
            plugin_presentation: None,
            #[cfg(test)]
            observations: PresentationObservations::default(),
        }
    }

    fn initial_presentation() -> (Instant, ResolvedTheme) {
        (Instant::now(), super::theme::resolve_initial_theme())
    }

    fn with_initial_history(mut self, history: &[SessionEvent]) -> Self {
        self.absorb_history(history);
        self
    }

    /// Move app-level presentation/runtime state from another app after recreating session state.
    pub(crate) fn take_cross_session_state_from(&mut self, source: &Self) {
        // Explicit managed entry is sticky across reconstruction; only a new
        // ordinary root may materialize model application access.
        self.managed_composer.execution = source.managed_composer.execution;
        self.managed_composer
            .settings
            .clone_from(&source.managed_composer.settings);
        self.managed_composer
            .requests
            .clone_from(&source.managed_composer.requests);
        self.managed_composer
            .label
            .clone_from(&source.managed_composer.label);
        if self.session_id == source.session_id {
            self.managed_composer
                .placement
                .clone_from(&source.managed_composer.placement);
        }
        self.apply_tui_config(source.tui_config().clone());
        self.set_daemon_connection(source.daemon_connection());
        self.set_agent_metadata_hydrated(source.is_agent_metadata_hydrated());
        self.plugin_presentation
            .clone_from(&source.plugin_presentation);
        self.sync_plugin_structure();
        self.theme_catalog.clone_from(&source.theme_catalog);
        #[cfg(test)]
        {
            self.observations.theme_catalog_discovery_count =
                source.observations.theme_catalog_discovery_count;
        }
        self.take_theme_transition_state_from(source);
    }

    /// Preserve process-local transcript presentation across reconstruction of the same session.
    pub(crate) fn take_same_session_transcript_state_from(&mut self, source: &Self) {
        if self.session_id != source.session_id || self.session_id.is_none() {
            return;
        }
        self.notices.copy_from(&source.notices);
    }

    /// Preserve pending reasoning presentation across a reconnect to the same session.
    pub(crate) fn take_same_session_reasoning_state_from(&mut self, source: &Self) {
        if self.session_id == source.session_id {
            self.pending_reasoning_effort
                .clone_from(&source.pending_reasoning_effort);
            self.reasoning_effort_generation = source.reasoning_effort_generation;
            self.reasoning_effort_values
                .clone_from(&source.reasoning_effort_values);
            self.reasoning_support = source.reasoning_support;
            self.reasoning_default_effort
                .clone_from(&source.reasoning_default_effort);
            self.reasoning_default_summary
                .clone_from(&source.reasoning_default_summary);
            self.refresh_thinking_label();
        }
    }

    /// Set the local plugin runtime used for client-side presentation projection.
    #[cfg(test)]
    pub fn set_plugin_host(&mut self, host: Arc<bcode_plugin::PluginHost>) {
        self.plugin_presentation = Some(Arc::new(
            crate::plugin_tui::PluginTuiPresentation::from_shared(host),
        ));
        self.sync_plugin_structure();
    }

    /// Shortcut label from the effective terminal registry.
    #[must_use]
    pub fn shortcut_hint(&self, scope: &str, action: &str) -> String {
        let keys = self.effective_keymap.keys_for_action(scope, action);
        if keys.is_empty() {
            "unbound".into()
        } else {
            keys.join(" / ")
        }
    }

    /// Replace the terminal frontend's effective keymap snapshot without rereading config.
    pub fn set_effective_keymap(
        &mut self,
        registry: std::sync::Arc<bcode_plugin_sdk::tui::keymap::KeymapRegistry>,
    ) {
        if !std::sync::Arc::ptr_eq(&self.effective_keymap, &registry)
            && self.effective_keymap != registry
        {
            self.effective_keymap = registry;
            self.effective_keymap_generation = self.effective_keymap_generation.wrapping_add(1);
            self.key_hints = format!(
                "{} send · {} interrupt · {} exit · {} palette",
                self.shortcut_hint("chat", crate::keymap::BmuxAction::InputSubmitSteering.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::AppInterrupt.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::AppExit.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::CommandPaletteOpen.id())
            );
        }
    }

    /// Effective bindings shared by visual rendering and terminal dispatch.
    #[must_use]
    pub fn effective_keymap(&self) -> &bcode_plugin_sdk::tui::keymap::KeymapRegistry {
        &self.effective_keymap
    }

    /// Effective bindings as a shared handle; cloning it never copies the bindings.
    #[must_use]
    pub fn shared_effective_keymap(
        &self,
    ) -> std::sync::Arc<bcode_plugin_sdk::tui::keymap::KeymapRegistry> {
        std::sync::Arc::clone(&self.effective_keymap)
    }

    /// Generation of the effective keymap: it advances whenever the keymap is replaced, so
    /// renderers caching key hints (the V2 plugin visual routing) know to refresh them.
    #[must_use]
    pub const fn effective_keymap_generation(&self) -> u64 {
        self.effective_keymap_generation
    }

    /// Set persistent local plugin presentation state.
    pub fn set_plugin_presentation(
        &mut self,
        presentation: Arc<crate::plugin_tui::PluginTuiPresentation>,
    ) {
        self.plugin_presentation = Some(presentation);
        self.sync_plugin_structure();
    }

    /// Adopt the loaded plugins' manifest `[presentation]` declarations, from which the
    /// transcript engine derives plugin rule layers, declared target fields, facts and property
    /// schemas. Returns whether the declarations changed.
    ///
    /// Runs whenever the plugin catalog changes: every catalog (re)load, from startup or a
    /// configuration reload, installs a new presentation through
    /// [`Self::set_plugin_presentation`].
    pub fn sync_plugin_structure(&mut self) -> bool {
        let declarations = self
            .plugin_presentation
            .as_deref()
            .map(crate::plugin_tui::PluginTuiPresentation::presentation_declarations)
            .unwrap_or_default();
        if declarations == self.plugin_presentation_declarations {
            return false;
        }
        self.plugin_presentation_declarations = declarations;
        true
    }

    /// Return the local plugin runtime used for client-side presentation projection.
    #[cfg(test)]
    #[must_use]
    pub const fn has_plugin_backend(&self) -> bool {
        self.plugin_presentation.is_some()
    }

    /// Return persistent local plugin presentation state.
    #[must_use]
    pub fn plugin_presentation(&self) -> Option<&crate::plugin_tui::PluginTuiPresentation> {
        self.plugin_presentation.as_deref()
    }

    /// Return timeline entries for committed user messages.
    ///
    /// Store source sequences, not resident indexes: the dialog can remain open
    /// while pagination or reconnect replaces its transcript window.
    #[must_use]
    pub fn timeline_entries(&self) -> Vec<TimelineEntry> {
        self.input_history
            .entries()
            .iter()
            .map(|entry| TimelineEntry::new(entry.sequence, entry.timestamp_ms, entry.text.clone()))
            .collect()
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionId> {
        self.session_id
    }

    /// Return the next renderer-neutral live text presentation deadline.
    #[must_use]
    pub fn next_streaming_presentation_deadline(&self, now: Instant) -> Option<Instant> {
        self.session_view.next_streaming_presentation_deadline(now)
    }

    /// Advance renderer-neutral live text presentation and adapt any visible changes.
    pub fn advance_streaming_presentation(&mut self, now: Instant) -> bool {
        if !self.session_view.advance_streaming_presentation(now) {
            return false;
        }
        true
    }

    /// Return the current session title, if known.
    #[must_use]
    pub fn session_title(&self) -> Option<&str> {
        self.session_view.snapshot().title.as_deref()
    }

    /// Header title: the session title, or, for a managed conversation whose input is not
    /// a Bcode user message, the title its first prompt gives it.
    #[must_use]
    pub fn display_session_title(&self) -> Option<String> {
        if let Some(title) = self.session_title() {
            return Some(title.to_owned());
        }
        self.managed_label()?;
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .find_map(|item| match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::UserMessage { message } => Some(
                    bcode_session_view::derive_session_title_from_prompt(&message.text),
                ),
                _ => None,
            })
    }

    /// Return the current working directory, if known.
    #[must_use]
    pub fn working_directory(&self) -> Option<&std::path::Path> {
        self.session_view.snapshot().working_directory.as_deref()
    }

    /// Replace bounded-history accounting with the complete canonical attach summary.
    pub fn apply_usage_summary(&mut self, summary: &bcode_session_models::SessionUsageSummary) {
        self.session_view.set_usage_summary(summary.clone());
    }

    /// Apply canonical session metadata from an attach/list response.
    ///
    /// Uses `SessionSummary::title()` as the single source of truth for the
    /// resolved display name (`name` → `explicit_name` → `derived_title`).
    /// This ensures that bounded first-event discovery on the server side
    /// (which may populate `explicit_name` or `derived_title`) is reflected
    /// in the TUI cache without duplicating fallback logic.
    pub(crate) fn stamp_child_effect(&mut self, effect: &mut super::effects::TuiEffect) -> bool {
        match effect {
            super::effects::TuiEffect::LoadSessionChildren {
                parent_id,
                generation,
                after,
                ..
            } => {
                self.retain_child_watch_window(*parent_id, *after);
                *generation = self.child_request_fence.begin(false, *parent_id);
                true
            }
            super::effects::TuiEffect::LoadChildActivity {
                session_id,
                after,
                generation,
            } => {
                *after = self.monitor_activity_cursor(*session_id);
                let target = bcode_session_models::SessionActivityTarget {
                    session_id: *session_id,
                    query: bcode_session_models::SessionActivityQuery {
                        scope: bcode_session_models::SessionActivityScope::DirectChildren,
                        after: *after,
                        limit: 64,
                    },
                };
                if !self.child_watch.activity.contains(&target) {
                    self.child_watch.activity.insert(0, target);
                    self.child_watch.activity.truncate(8);
                }
                *generation = self.child_request_fence.begin(true, *session_id);
                true
            }
            _ => false,
        }
    }

    pub(crate) fn invalidate_child_requests(&mut self) {
        self.child_request_fence = crate::child_request_fence::ChildRequestFence::default();
    }

    pub(crate) fn accepts_child_result(
        &self,
        activity: bool,
        session_id: SessionId,
        generation: u64,
    ) -> bool {
        self.child_request_fence
            .accepts(activity, session_id, generation)
    }

    /// Apply normalized observations without modifying canonical transcript or focus.
    pub fn apply_child_activity(
        &mut self,
        result: Result<bcode_session_models::SessionActivityPage, bcode_client::ClientError>,
    ) {
        match result {
            Ok(page) => {
                let now_ms = u64::try_from(
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_millis(),
                )
                .unwrap_or(u64::MAX);
                self.session_view.observe_child_card_activity(&page, now_ms);
                let selected = self.selected_monitor_session();
                self.child_activity = Some(page);
                if let Some(selected) = selected {
                    self.child_panel_selected = self
                        .monitor_tree()
                        .rows
                        .iter()
                        .position(|row| self.child_summaries[row.source_index].id == selected)
                        .unwrap_or(0);
                }
                self.child_activity_error = None;
            }
            Err(error) => {
                self.child_activity_error = Some(format!("children: stale/degraded · {error}"));
            }
        }
    }

    /// Retain only bounded loaded windows; explicit paging displaces the oldest window.
    fn retain_child_watch_window(&mut self, parent_id: SessionId, after: Option<SessionId>) {
        if self.session_id != Some(parent_id)
            && !self
                .child_summaries
                .iter()
                .any(|summary| summary.id == parent_id)
        {
            return;
        }
        let query = bcode_session_models::SessionChildQuery {
            parent_id,
            after,
            limit: 64,
        };
        if !self.child_watch.windows.contains(&query) {
            self.child_watch.windows.insert(0, query);
            self.child_watch.windows.truncate(4);
        }
    }

    fn retain_child_activity_continuation(
        &mut self,
        target: &bcode_session_models::SessionActivityTarget,
        after: SessionId,
    ) {
        let continuation = bcode_session_models::SessionActivityTarget {
            session_id: target.session_id,
            query: bcode_session_models::SessionActivityQuery {
                after: Some(after),
                ..target.query.clone()
            },
        };
        if self.child_watch.activity.len() < 8 && !self.child_watch.activity.contains(&continuation)
        {
            self.child_watch.activity.push(continuation);
        }
    }

    /// Apply a full replacement from the current activity watch, including reset.
    pub(crate) fn apply_child_watch_snapshot(
        &mut self,
        scope: &bcode_session_models::SessionActivityWatchScope,
        snapshot: bcode_session_models::SessionActivityWatchSnapshot,
    ) {
        if snapshot.activity.len() != scope.activity.len()
            || snapshot.children.len() != scope.children.len()
        {
            self.child_activity_error =
                Some("children: stale/degraded · invalid watch snapshot bounds".to_owned());
            return;
        }
        let mut pages: BTreeMap<SessionId, Vec<bcode_session_models::SessionActivityPage>> =
            BTreeMap::new();
        let mut errors = Vec::new();
        for (target, result) in scope.activity.iter().zip(snapshot.activity) {
            self.child_request_fence.begin(true, target.session_id);
            if self.session_id != Some(target.session_id)
                && !self
                    .child_summaries
                    .iter()
                    .any(|summary| summary.id == target.session_id)
            {
                continue;
            }
            match result {
                Ok(mut page) => {
                    if target.query.after.is_some()
                        && page.coverage == bcode_session_models::SessionActivityCoverage::Complete
                    {
                        page.coverage = bcode_session_models::SessionActivityCoverage::Partial;
                    }
                    if let Some(after) = page.next_after {
                        self.retain_child_activity_continuation(target, after);
                    }
                    pages.entry(target.session_id).or_default().push(page);
                }
                Err(error) => errors.push(error.message),
            }
        }
        // Complete replacements must not leave an old active/waiting fact behind.
        self.child_pages.clear();
        self.child_activity = None;
        self.session_view.invalidate_child_card_activity();
        for (id, windows) in pages {
            if let Some(page) =
                bcode_session_view::child_activity::combine_activity_windows(windows)
            {
                if Some(id) == self.session_id {
                    self.apply_child_activity(Ok(page));
                } else {
                    self.apply_monitor_activity(id, page);
                }
            }
        }
        self.child_watch.relationships.retain(|(parent, after), _| {
            scope
                .children
                .iter()
                .any(|query| query.parent_id == *parent && query.after == *after)
        });
        for (query, result) in scope.children.iter().zip(snapshot.children) {
            self.child_request_fence.begin(false, query.parent_id);
            match result {
                Ok(page) => {
                    let ids: Vec<_> = page.children.iter().map(|child| child.id).collect();
                    if page.complete
                        && page.issues.is_empty()
                        && let Some(previous) = self
                            .child_watch
                            .relationships
                            .insert((query.parent_id, query.after), ids.clone())
                    {
                        let removed: Vec<_> = previous
                            .into_iter()
                            .filter(|id| {
                                !ids.contains(id)
                                    && !self
                                        .child_watch
                                        .relationships
                                        .values()
                                        .any(|window| window.contains(id))
                            })
                            .collect();
                        self.child_summaries
                            .retain(|summary| !removed.contains(&summary.id));
                    }
                    if self.child_watch.windows.len() < 4
                        && let Some(after) = page.next_after
                    {
                        self.retain_child_watch_window(query.parent_id, Some(after));
                    }
                    self.apply_monitor_children(query.parent_id, &page);
                }
                Err(error) => errors.push(error.message),
            }
        }
        if !errors.is_empty() {
            self.child_activity_error =
                Some(format!("children: stale/degraded · {}", errors.join("; ")));
        }
    }

    /// Loaded windows and a bounded discovery frontier, independent of observation timestamps.
    pub(crate) fn child_watch_scope(
        &self,
    ) -> Option<bcode_session_models::SessionActivityWatchScope> {
        use bcode_session_models::{
            SessionActivityQuery, SessionActivityScope, SessionActivityTarget,
            SessionActivityWatchScope, SessionChildQuery,
        };
        let root = self.session_id?;
        let mut children = vec![SessionChildQuery {
            parent_id: root,
            after: None,
            limit: 64,
        }];
        let mut add = |query: SessionChildQuery| {
            if children.len() < 8 && !children.contains(&query) {
                children.push(query);
            }
        };
        for query in &self.child_watch.windows {
            add(query.clone());
        }
        for parent_id in self.expanded_monitor_sessions() {
            add(SessionChildQuery {
                parent_id,
                after: None,
                limit: 64,
            });
        }
        for parent_id in self.monitor_discovery_frontier() {
            add(SessionChildQuery {
                parent_id,
                after: None,
                limit: 64,
            });
        }
        let mut activity: Vec<_> = children
            .iter()
            .map(|query| SessionActivityTarget {
                session_id: query.parent_id,
                query: SessionActivityQuery {
                    scope: SessionActivityScope::DirectChildren,
                    after: query.after,
                    limit: query.limit,
                },
            })
            .collect();
        for target in &self.child_watch.activity {
            if self.session_id != Some(target.session_id)
                && !self
                    .child_summaries
                    .iter()
                    .any(|summary| summary.id == target.session_id)
            {
                continue;
            }
            if activity.len() < 16 && !activity.contains(target) {
                activity.push(target.clone());
            }
        }
        Some(SessionActivityWatchScope { activity, children })
    }

    /// Navigation identity fences A -> B -> A, including otherwise identical scopes.
    #[must_use]
    pub(crate) const fn child_watch_navigation(&self) -> SessionId {
        self.child_watch.navigation
    }

    /// Persistent known-active count, never an inferred subtree total.
    #[must_use]
    pub fn child_activity_text(&self) -> String {
        let relationship_known = !self.child_summaries.is_empty()
            || !self.child_pages.is_empty()
            || self.child_activity_error.is_some()
            || self
                .session_view
                .snapshot()
                .session_summary
                .as_ref()
                .is_some_and(|summary| summary.parent_id.is_some())
            || self
                .child_activity
                .as_ref()
                .is_some_and(|page| !page.descendants.is_empty());
        if !relationship_known && !self.child_panel_open() {
            return String::new();
        }
        let mut observed = BTreeMap::new();
        if let Some(page) = &self.child_activity {
            for activity in &page.descendants {
                observed.insert(activity.session_id, activity);
            }
        }
        for page in self.child_pages.values() {
            observed.insert(page.own.session_id, &page.own);
            for activity in &page.descendants {
                observed.insert(activity.session_id, activity);
            }
        }
        if !self.child_pages.is_empty() {
            let count = observed
                .values()
                .filter(|activity| {
                    bcode_session_view::child_activity::has_active_evidence(activity)
                })
                .count();
            let degraded = self.child_pages.values().any(|page| {
                page.coverage == bcode_session_models::SessionActivityCoverage::Degraded
            });
            return format!(
                "children: {count} known active observed subtree · {}{}",
                if degraded {
                    "partial/degraded"
                } else {
                    "partial"
                },
                self.child_activity_error
                    .as_ref()
                    .map_or(String::new(), |error| format!(" · {error}"))
            );
        }
        self.child_activity_error.clone().unwrap_or_else(|| {
            self.child_activity.as_ref().map_or_else(
                || {
                    format!(
                        "children: unknown · {} monitor",
                        self.shortcut_hint("chat", "tui.monitor.toggle")
                    )
                },
                bcode_session_view::child_activity::child_activity_label,
            )
        })
    }

    /// Open the child monitor with keyboard focus, retaining its loaded state.
    pub const fn open_child_panel(&mut self) {
        self.child_panel = ChildPanelPresentation::Monitor;
    }

    /// Toggle monitor visibility with explicitly requested keyboard focus.
    pub const fn toggle_child_panel(&mut self) {
        self.child_panel = if self.child_panel_focused() {
            ChildPanelPresentation::Hidden
        } else {
            ChildPanelPresentation::Monitor
        };
    }

    /// Release monitor keys without hiding its live overview.
    pub const fn release_child_panel_focus(&mut self) {
        if self.child_panel_open() {
            self.child_panel = ChildPanelPresentation::Composer;
        }
    }

    /// Whether explicit monitor navigation owns keys.
    #[must_use]
    pub const fn child_panel_focused(&self) -> bool {
        matches!(self.child_panel, ChildPanelPresentation::Monitor)
    }

    /// Whether the local monitor accepts explicit navigation keys.
    #[must_use]
    pub const fn child_panel_open(&self) -> bool {
        !matches!(self.child_panel, ChildPanelPresentation::Hidden)
    }

    /// Merge explicitly requested child metadata, preserving selection and observed coverage.
    pub fn apply_monitor_children(
        &mut self,
        parent_id: SessionId,
        page: &bcode_session_models::SessionChildPage,
    ) {
        if self.session_id != Some(parent_id)
            && !self
                .child_summaries
                .iter()
                .any(|summary| summary.id == parent_id)
        {
            return;
        }
        let selected = self.selected_monitor_session();
        self.child_cursors.insert(parent_id, page.next_after);
        for child in &page.children {
            if let Some(index) = self
                .child_summaries
                .iter()
                .position(|summary| summary.id == child.id)
            {
                self.child_summaries[index] = child.clone();
            } else {
                self.child_summaries.push(child.clone());
            }
        }
        // Sliding loaded window, not a descendant limit: evicted branches reload from their
        // direct parent, and orphaned loaded rows remain navigable with detached semantics.
        while self.child_summaries.len() > 256 {
            let index = self
                .child_summaries
                .iter()
                .position(|summary| {
                    Some(summary.id) != selected
                        && !page.children.iter().any(|child| child.id == summary.id)
                })
                .unwrap_or(0);
            let evicted = self.child_summaries.remove(index);
            self.child_pages.remove(&evicted.id);
            self.child_expanded.remove(
                &bcode_session_view::navigation::SessionNavigationKey::from_summary(&evicted),
            );
            self.child_cursors.remove(&evicted.id);
        }
        self.child_cursors.retain(|id, _| {
            Some(*id) == self.session_id
                || self.child_summaries.iter().any(|summary| summary.id == *id)
        });
        if let Some(selected) = selected {
            self.child_panel_selected = self
                .monitor_tree()
                .rows
                .iter()
                .position(|row| self.child_summaries[row.source_index].id == selected)
                .unwrap_or(0);
        }
        if !page.complete || !page.issues.is_empty() {
            self.child_activity_error = Some(format!(
                "children: degraded/incomplete · {}",
                page.issues.join("; ")
            ));
        }
    }

    /// Loaded expanded frontier roots, used for bounded rotating refresh.
    #[must_use]
    pub fn expanded_monitor_sessions(&self) -> Vec<SessionId> {
        self.child_expanded
            .iter()
            .map(|key| key.session_id)
            .collect()
    }

    /// Rotating loaded discovery frontier includes collapsed coordinators; no recursive server scan.
    #[must_use]
    pub fn monitor_discovery_frontier(&self) -> Vec<SessionId> {
        let mut frontier: Vec<_> = self
            .child_summaries
            .iter()
            .map(|summary| summary.id)
            .collect();
        frontier.sort_unstable();
        frontier
    }

    /// Loaded activity continuations are drained before the discovery window advances.
    #[must_use]
    pub fn monitor_pending_activity_sessions(&self) -> Vec<SessionId> {
        self.child_pages
            .iter()
            .filter_map(|(id, page)| page.next_after.map(|_| *id))
            .collect()
    }

    /// Continue the accepted bounded activity window; a terminal page restarts on the next refresh.
    #[allow(clippy::must_use_candidate)] // Option carries the result-use contract.
    pub fn monitor_activity_cursor(&self, session_id: SessionId) -> Option<SessionId> {
        if self.session_id == Some(session_id) {
            self.child_activity
                .as_ref()
                .and_then(|page| page.next_after)
        } else {
            self.child_pages
                .get(&session_id)
                .and_then(|page| page.next_after)
        }
    }

    /// Resume the bounded direct-child frontier after sparse pages.
    #[allow(clippy::must_use_candidate)] // Option already expresses the result-use contract.
    pub fn monitor_frontier_cursor(&self, parent_id: SessionId) -> Option<SessionId> {
        self.child_cursors.get(&parent_id).copied().flatten()
    }

    /// Surface failed coordinator observations without replacing accepted evidence with idle.
    pub fn monitor_observation_failed(&mut self, session_id: SessionId, error: &str) {
        self.session_view.invalidate_child_card_activity();
        self.child_activity_error = Some(format!("child {session_id}: stale/degraded · {error}"));
    }

    /// Retain a bounded observation for an explicitly expanded coordinator.
    pub fn apply_monitor_activity(
        &mut self,
        session_id: SessionId,
        page: bcode_session_models::SessionActivityPage,
    ) {
        let selected = self.selected_monitor_session();
        self.child_pages.insert(session_id, page);
        if let Some(selected) = selected {
            self.child_panel_selected = self
                .monitor_tree()
                .rows
                .iter()
                .position(|row| self.child_summaries[row.source_index].id == selected)
                .unwrap_or(0);
        }
        self.child_pages
            .retain(|id, _| self.child_summaries.iter().any(|summary| summary.id == *id));
    }

    fn monitor_tree(&self) -> bcode_session_view::navigation::SessionTreeProjection {
        bcode_session_view::navigation::project_session_tree(
            &self.child_summaries,
            &self.child_expanded,
            None,
            self.child_summaries.len(),
        )
    }

    /// Selected child identity for ordinary application-boundary opening.
    #[must_use]
    pub fn selected_monitor_session(&self) -> Option<SessionId> {
        let tree = self.monitor_tree();
        tree.rows
            .get(self.child_panel_selected)
            .map(|row| self.child_summaries[row.source_index].id)
    }

    /// Move within loaded semantic rows; preview paging follows selection.
    pub fn move_monitor_selection(&mut self, down: bool) {
        let count = self.monitor_tree().rows.len();
        self.child_panel_selected = if down {
            self.child_panel_selected
                .saturating_add(1)
                .min(count.saturating_sub(1))
        } else {
            self.child_panel_selected.saturating_sub(1)
        };
    }

    /// Expand selected coordinator and request only its direct-child frontier.
    pub fn expand_monitor_selected(&mut self) -> Option<SessionId> {
        let session_id = self.selected_monitor_session()?;
        let summary = self
            .child_summaries
            .iter()
            .find(|summary| summary.id == session_id)?;
        let key = bcode_session_view::navigation::SessionNavigationKey::from_summary(summary);
        if self.child_expanded.remove(&key) {
            self.child_request_fence.begin(false, session_id);
            self.child_request_fence.begin(true, session_id);
            return None;
        }
        self.child_expanded.insert(key);
        Some(session_id)
    }

    /// Selected branch continuation, or the attached root's continuation.
    #[must_use]
    pub fn monitor_next_page(&self) -> Option<(SessionId, SessionId)> {
        let parent_id = self.selected_monitor_session().or(self.session_id)?;
        self.child_cursors
            .get(&parent_id)
            .copied()
            .flatten()
            .map(|after| (parent_id, after))
            .or_else(|| {
                self.session_id.and_then(|root| {
                    self.child_cursors
                        .get(&root)
                        .copied()
                        .flatten()
                        .map(|after| (root, after))
                })
            })
    }

    /// Return the focused selection row within the bounded visible preview.
    #[must_use]
    pub fn monitor_selected_preview_row(&self) -> usize {
        self.child_panel_selected.min(4).saturating_add(1)
    }

    /// Select a loaded row by stable identity without opening its session.
    pub fn select_monitor_session(&mut self, session_id: SessionId) {
        if let Some(index) = self
            .monitor_tree()
            .rows
            .iter()
            .position(|row| self.child_summaries[row.source_index].id == session_id)
        {
            self.child_panel_selected = index;
            self.open_child_panel();
        }
    }

    /// Loaded known-descendant activity for monitor chrome, never complete subtree totals.
    #[must_use]
    pub fn monitor_context(&self) -> super::monitor_presentation::MonitorContext {
        use super::monitor_presentation::{MonitorContext, MonitorDescendantBadge};
        let observations = bcode_session_view::child_attention::newest_observations(
            self.child_activity.iter().chain(self.child_pages.values()),
        );
        let parents: BTreeMap<_, _> = self
            .child_summaries
            .iter()
            .map(|summary| (summary.id, summary.parent_id))
            .collect();
        let mut badges: BTreeMap<SessionId, MonitorDescendantBadge> = BTreeMap::new();
        for (activity, _) in observations.values() {
            let mut parent = activity.parent_id;
            let mut visited = BTreeSet::from([activity.session_id]);
            // Follow only loaded, verified relationship links; cycles and gaps stop traversal.
            for _ in 0..self.child_summaries.len() {
                let Some(id) = parent else {
                    break;
                };
                if !visited.insert(id) {
                    break;
                }
                let Some(next_parent) = parents.get(&id) else {
                    break;
                };
                let badge = badges.entry(id).or_default();
                badge.active += usize::from(
                    bcode_session_view::child_activity::has_active_evidence(activity),
                );
                badge.attention +=
                    usize::from(bcode_session_view::child_attention::attention_rank(activity) <= 1);
                parent = *next_parent;
            }
        }
        MonitorContext {
            current_session: self.session_id,
            descendant_badges: badges,
        }
    }

    /// Typed bounded terminal presentation, available to acceptance tests.
    #[must_use]
    pub fn monitor_presentation(&self) -> super::monitor_presentation::MonitorPresentation {
        use super::monitor_presentation::{MonitorPresentation, MonitorRow};
        let tree = self.monitor_tree();
        let observations = bcode_session_view::child_attention::newest_observations(
            self.child_activity.iter().chain(self.child_pages.values()),
        );
        let now = super::time_format::unix_time_millis(std::time::SystemTime::now());
        let start = self.child_panel_selected.saturating_sub(4);
        let rows: Vec<_> = tree
            .rows
            .iter()
            .enumerate()
            .skip(start)
            .take(8)
            .map(|(index, row)| {
                let summary = &self.child_summaries[row.source_index];
                let observation = observations.get(&summary.id);
                let continues = |depth| {
                    tree.rows
                        .iter()
                        .skip(index + 1)
                        .take_while(|next| next.depth >= depth)
                        .any(|next| next.depth == depth)
                };
                MonitorRow {
                    session_id: summary.id,
                    title: summary.display_title().to_owned(),
                    parent_id: summary.parent_id,
                    detached: row.detached && summary.parent_id != self.session_id,
                    depth: row.depth,
                    ancestor_continuations: (0..row.depth.min(4)).map(continues).collect(),
                    last_sibling: !continues(row.depth),
                    expanded: self.child_expanded.contains(
                        &bcode_session_view::navigation::SessionNavigationKey::from_summary(
                            summary,
                        ),
                    ),
                    states: observation.map_or_else(
                        || vec![bcode_session_models::SessionActivityState::Unknown],
                        |(activity, _)| {
                            if activity.states.is_empty() {
                                vec![bcode_session_models::SessionActivityState::Unknown]
                            } else {
                                activity.states.clone()
                            }
                        },
                    ),
                    evidence: observation.and_then(|(activity, _)| activity.evidence.clone()),
                    age: observation.map_or_else(
                        || "unobserved".to_owned(),
                        |(_, observed)| {
                            bcode_session_view::child_attention::observation_age_label(
                                *observed, now,
                            )
                        },
                    ),
                }
            })
            .collect();
        let selected = self
            .child_panel_selected
            .checked_sub(start)
            .filter(|index| *index < rows.len());
        MonitorPresentation {
            parent_title: self.session_title().unwrap_or("Session").to_owned(),
            overview: self.child_activity.as_ref().map_or_else(
                || "Attention unknown · descendants unscanned".to_owned(),
                |root| {
                    bcode_session_view::child_attention::overview_label(
                        root,
                        self.child_activity.iter().chain(self.child_pages.values()),
                    )
                },
            ),
            freshness: self.child_activity.as_ref().map_or_else(
                || "unobserved".to_owned(),
                |page| {
                    bcode_session_view::child_attention::observation_age_label(
                        page.observed_at_ms,
                        now,
                    )
                },
            ),
            details: selected.and_then(|index| rows.get(index)).cloned(),
            selected,
            rows,
            coverage: self.child_activity.as_ref().map(|page| page.coverage),
            error: self.child_activity_error.clone(),
            load_more: self.monitor_next_page().is_some(),
            footer: self.child_panel_focus_hint(),
        }
    }

    /// Bounded accepted observation rows; selection scrolls the preview rather than imposing a total limit.
    #[must_use]
    pub fn child_panel_rows(&self) -> Vec<String> {
        if !self.child_panel_open() {
            return Vec::new();
        }
        let overview = self.child_activity.as_ref().map_or_else(
            || "Attention unknown · descendants unscanned".to_owned(),
            |root| {
                bcode_session_view::child_attention::overview_label(
                    root,
                    self.child_activity.iter().chain(self.child_pages.values()),
                )
            },
        );
        let mut rows = vec![format!("{overview} · {}", self.child_panel_focus_hint())];
        let tree = self.monitor_tree();
        let start = self.child_panel_selected.saturating_sub(4);
        let observations = bcode_session_view::child_attention::newest_observations(
            self.child_activity.iter().chain(self.child_pages.values()),
        );
        let now_ms = u64::try_from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis(),
        )
        .unwrap_or(u64::MAX);
        for (index, row) in tree.rows.iter().enumerate().skip(start).take(8) {
            let summary = &self.child_summaries[row.source_index];
            let evidence = observations.get(&summary.id).map_or_else(
                || "unknown · unscanned".to_owned(),
                |(activity, observed)| {
                    let operation = activity
                        .evidence
                        .as_ref()
                        .and_then(|evidence| evidence.current_operation.as_deref())
                        .map_or(String::new(), |label| format!(" · {label}"));
                    let nested = self.child_pages.get(&summary.id).map_or(0, |page| {
                        page.descendants
                            .iter()
                            .filter(|child| {
                                bcode_session_view::child_activity::has_active_evidence(child)
                            })
                            .count()
                    });
                    format!(
                        "{}{}{} · {}",
                        bcode_session_view::child_activity::activity_evidence_label(activity),
                        operation,
                        if nested > 0 {
                            format!(" · {nested} known active descendants")
                        } else {
                            String::new()
                        },
                        bcode_session_view::child_attention::observation_age_label(
                            *observed, now_ms
                        )
                    )
                },
            );
            let depth = if row.depth > 4 {
                format!("[{}] ", row.depth)
            } else {
                "  ".repeat(row.depth)
            };
            rows.push(format!(
                "{}{depth}{} · {evidence}",
                if index == self.child_panel_selected {
                    "› "
                } else {
                    "  "
                },
                summary.display_title()
            ));
        }
        if let Some(page) = &self.child_activity {
            rows.push(format!(
                "own: {} · {} · descendants partial · acceptance unavailable",
                bcode_session_view::child_activity::activity_evidence_label(&page.own),
                bcode_session_view::child_attention::observation_age_label(
                    page.observed_at_ms,
                    now_ms
                )
            ));
            rows.extend(page.issues.iter().take(2).cloned());
        }
        if self.monitor_next_page().is_some() {
            rows.push(format!(
                "{}: load next bounded child page",
                self.shortcut_hint("monitor", "tui.monitor.nextPage")
            ));
        }
        rows
    }

    fn child_panel_focus_hint(&self) -> String {
        if self.child_panel_focused() {
            format!(
                "MONITOR FOCUS · {} select {} open {} expand {} parent {} more · {} composer",
                self.shortcut_hint("monitor", "tui.control.down"),
                self.shortcut_hint("monitor", "tui.control.submit"),
                self.shortcut_hint("monitor", "tui.control.right"),
                self.shortcut_hint("monitor", "tui.control.left"),
                self.shortcut_hint("monitor", "tui.monitor.nextPage"),
                self.shortcut_hint("monitor", "tui.control.cancel")
            )
        } else {
            format!(
                "COMPOSER FOCUS · click selects · {} focus",
                self.shortcut_hint("chat", "tui.monitor.toggle")
            )
        }
    }

    pub fn apply_session_summary(&mut self, summary: &bcode_session_models::SessionSummary) {
        if self.session_id != Some(summary.id) {
            self.child_activity = None;
            self.child_activity_error = None;
            self.child_summaries.clear();
            self.child_pages.clear();
            self.child_cursors.clear();
            self.child_watch.windows.clear();
            self.child_watch.activity.clear();
            self.child_watch.navigation = SessionId::new();
            self.child_watch.relationships.clear();
            self.child_request_fence = crate::child_request_fence::ChildRequestFence::default();
            self.child_expanded.clear();
            self.child_panel_selected = 0;
        }
        self.session_id = Some(summary.id);
        self.session_view.set_session_summary(summary.clone());
    }

    /// Deliver approved, secret-free plugin presentation fields from the resolved app config.
    /// This does not mutate configuration or execution policy.
    ///
    /// # Errors
    /// Returns a normalized error if the configuration cannot be encoded or an adapter rejects it.
    pub fn apply_plugin_presentation_config(
        &self,
        config: &bcode_config::BcodeConfig,
    ) -> Result<bool, String> {
        let output = bcode_config::ShellToolOutputConfig {
            format_commands: config.tools.shell.output.format_commands,
            presentation: config.tools.shell.output.presentation.clone(),
            ..bcode_config::ShellToolOutputConfig::default()
        };
        let configuration = bcode_plugin_sdk::tui::PluginTuiPresentationConfiguration {
            version: 1,
            config: serde_json::json!({"shell": {"output": output}}),
        };
        let Some(presentation) = self.plugin_presentation.as_ref() else {
            return Ok(false);
        };
        presentation.configure(configuration)
    }

    /// Apply terminal UI configuration.
    pub fn apply_tui_config(&mut self, config: TuiConfig) {
        self.apply_thinking_config(config.thinking);
        self.tui_config = config;
        self.theme_preview_id = None;
        self.invalidate_theme_catalog();
        self.sync_theme_target(Instant::now());
    }

    /// Return the discovered theme catalog, loading it once for the current app inputs.
    pub(crate) fn theme_catalog(&mut self) -> Option<&DiscoveredThemes> {
        if self.theme_catalog.is_none() {
            let discovered = super::theme::discover_theme_catalog(self).ok();
            #[cfg(test)]
            {
                self.observations.theme_catalog_discovery_count = self
                    .observations
                    .theme_catalog_discovery_count
                    .saturating_add(1);
            }
            self.theme_catalog = discovered;
        }
        self.theme_catalog.as_ref()
    }

    /// Resolve one selection from the current cached theme catalog.
    pub(crate) fn resolve_theme_selection_cached(
        &mut self,
        theme_id: &str,
    ) -> Option<ResolvedThemeDefinition> {
        let overlays = self.tui_config.theme.effective_overlays();
        let variant = super::theme::resolve_variant_for_config(self.tui_config.theme.variant);
        self.theme_catalog()?
            .catalog
            .resolve(
                &super::theme::definition::ThemeSelection::new(theme_id)
                    .overlays(overlays)
                    .variant(variant),
            )
            .ok()
    }

    #[cfg(test)]
    pub(crate) const fn theme_catalog_discovery_count(&self) -> usize {
        self.observations.theme_catalog_discovery_count
    }

    /// Clear cached discovery so the next resolution observes current authorized inputs.
    pub(crate) fn invalidate_theme_catalog(&mut self) {
        self.theme_catalog = None;
    }

    fn install_theme_preview(&mut self, theme_id: &str, resolved: &ResolvedThemeDefinition) {
        let accent = resolved
            .style("border.focused")
            .and_then(|style| style.fg)
            .or_else(|| resolved.color("accent"))
            .unwrap_or(super::theme::PENDING_AGENT_METADATA_ACCENT);
        let target = super::theme::resolved_definition_theme(Some(resolved), accent);
        self.theme_preview_id = Some(theme_id.to_owned());
        self.target_theme = target;
        self.retarget_theme_transition(target.accent, Instant::now());
        self.update_theme_animation(Instant::now());
    }

    /// Preview one already-resolved picker theme without repeating discovery or resolution.
    pub(crate) fn preview_resolved_theme(&mut self, theme_id: &str, target: &ResolvedTheme) {
        self.theme_preview_id = Some(theme_id.to_owned());
        self.target_theme = *target;
        self.retarget_theme_transition(target.accent, Instant::now());
        self.update_theme_animation(Instant::now());
    }

    /// Refresh the active configured or preview theme atomically from current inputs.
    ///
    /// Returns the active selection id when it resolved. The existing target
    /// remains untouched when discovery or resolution is invalid, providing
    /// the runtime's last-valid-theme guarantee.
    pub fn reload_theme_if_valid(&mut self) -> Option<&str> {
        self.invalidate_theme_catalog();
        let theme_id = self
            .theme_preview_id
            .as_deref()
            .unwrap_or(&self.tui_config.theme.name)
            .to_owned();
        let resolved = self.resolve_theme_selection_cached(&theme_id)?;
        let configured_theme_accent = super::theme::resolved_accent(Some(&resolved));
        let accent = super::theme::resolve_app_accent(self, configured_theme_accent);
        let target = super::theme::resolved_definition_theme(Some(&resolved), accent);
        if target == self.target_theme {
            return Some(
                self.theme_preview_id
                    .as_deref()
                    .unwrap_or(&self.tui_config.theme.name),
            );
        }
        self.target_theme = target;
        self.retarget_theme_transition(target.accent, Instant::now());
        self.update_theme_animation(Instant::now());
        Some(
            self.theme_preview_id
                .as_deref()
                .unwrap_or(&self.tui_config.theme.name),
        )
    }

    /// Preview one discovered theme for this app without mutating durable configuration.
    ///
    /// Returns `false` when the id is unknown or cannot resolve through the
    /// bounded bundled/user/project/explicit catalog.
    pub fn preview_theme(&mut self, theme_id: &str) -> bool {
        let Some(resolved) = self.resolve_theme_selection_cached(theme_id) else {
            return false;
        };
        self.install_theme_preview(theme_id, &resolved);
        true
    }

    /// Restore the configured theme after a non-persistent preview.
    pub fn cancel_theme_preview(&mut self) {
        self.theme_preview_id = None;
        self.sync_theme_target(Instant::now());
    }

    /// Apply one discovered theme to the in-memory configuration.
    ///
    /// Durable config persistence remains owned by the normal config/settings path.
    pub fn apply_theme(&mut self, theme_id: &str) -> bool {
        if !self.preview_theme(theme_id) {
            return false;
        }
        theme_id.clone_into(&mut self.tui_config.theme.name);
        self.theme_preview_id = None;
        self.sync_theme_target(Instant::now());
        true
    }

    /// Request the terminal color query once, only when the theme variant
    /// follows the terminal (`auto`); returns true when it should be written.
    pub fn take_terminal_color_query(&mut self) -> bool {
        self.tui_config.theme.variant == bcode_config::TuiThemeVariant::Auto
            && !std::mem::replace(&mut self.terminal_color_query_sent, true)
    }

    /// Re-resolve the theme after the terminal reported its colors; returns
    /// whether the presented theme changed.
    pub fn refresh_terminal_colors(&mut self) -> bool {
        let before = self.target_theme.fingerprint;
        self.invalidate_theme_catalog();
        self.sync_theme_target(Instant::now());
        self.target_theme.fingerprint != before
    }

    /// Restore the profile selection a picker opened with, even when the
    /// catalog does not list it (the configured value was live before the
    /// preview, so it is restored verbatim rather than validated).
    pub fn restore_profile(&mut self, profile: Option<&str>) {
        if self.tui_config.theme.profile.as_deref() == profile {
            return;
        }
        self.tui_config.theme.profile = profile.map(str::to_owned);
        self.sync_theme_target(Instant::now());
    }

    /// Select (or clear) the profile overlay; returns false for unknown profiles.
    pub fn apply_profile(&mut self, profile: Option<&str>) -> bool {
        if let Some(profile) = profile {
            let known = self.theme_catalog().is_some_and(|discovered| {
                discovered
                    .catalog
                    .definition(profile)
                    .is_some_and(|definition| {
                        definition.kind() == super::theme::definition::ThemeKind::Profile
                    })
            });
            if !known {
                return false;
            }
        }
        self.tui_config.theme.profile = profile.map(str::to_owned);
        self.sync_theme_target(Instant::now());
        true
    }

    /// Apply renderer-neutral structural rules from declarative configuration.
    pub fn apply_configured_structure_rules(
        &mut self,
        rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    ) -> bool {
        if rules == self.configured_structure_rules {
            return false;
        }
        self.configured_structure_rules = rules;
        true
    }

    /// Apply interactive structural rules loaded from user state.
    pub fn apply_user_structure_rules(
        &mut self,
        rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    ) -> bool {
        if rules == self.user_structure_rules {
            return false;
        }
        self.user_structure_rules = rules;
        true
    }

    /// Declarative `[presentation]` configuration rules (the V2 transcript host compiles
    /// its own structure configuration from the same layers).
    #[must_use]
    pub fn configured_structure_rules(
        &self,
    ) -> &[bcode_presentation_policy::models::PresentationRule] {
        &self.configured_structure_rules
    }

    /// Interactive structural rules loaded from user state.
    #[must_use]
    pub fn user_structure_rules(&self) -> &[bcode_presentation_policy::models::PresentationRule] {
        &self.user_structure_rules
    }

    /// The loaded plugins' effective `[presentation]` declarations, by plugin id.
    #[must_use]
    pub const fn plugin_presentation_declarations(
        &self,
    ) -> &BTreeMap<String, bcode_plugin::PluginPresentationDeclaration> {
        &self.plugin_presentation_declarations
    }

    /// Apply one normalized renderer-neutral streaming-presentation policy.
    pub fn apply_streaming_presentation_policy(
        &mut self,
        policy: bcode_session_view_models::StreamingPresentationPolicy,
    ) -> bool {
        self.streaming_presentation_policy = policy.normalized();
        self.session_view
            .set_streaming_presentation_policy(policy.normalized())
    }

    /// The streaming-presentation policy the host shell last applied (configuration, the
    /// streaming configurator's override or its reset), which every transcript engine's
    /// `SessionView` presents live text with.
    #[must_use]
    pub const fn streaming_presentation_policy(
        &self,
    ) -> bcode_session_view_models::StreamingPresentationPolicy {
        self.streaming_presentation_policy
    }

    /// Return terminal UI configuration.
    #[must_use]
    pub const fn tui_config(&self) -> &TuiConfig {
        &self.tui_config
    }

    /// Return the effective diff viewer configuration.
    #[must_use]
    pub fn effective_diff_viewer_config(&self) -> TuiDiffViewerConfig {
        TuiDiffViewerConfig {
            layout: self
                .diff_viewer_layout_override
                .unwrap_or(self.tui_config.diff_viewer.layout),
            side_by_side_breakpoint: self.tui_config.diff_viewer.side_by_side_breakpoint,
        }
    }

    /// Cycle the session-local diff layout override.
    pub fn cycle_diff_viewer_layout(&mut self) {
        let next = match self
            .diff_viewer_layout_override
            .unwrap_or(self.tui_config.diff_viewer.layout)
        {
            TuiDiffViewerLayout::Auto => TuiDiffViewerLayout::Unified,
            TuiDiffViewerLayout::Unified => TuiDiffViewerLayout::SideBySide,
            TuiDiffViewerLayout::SideBySide => TuiDiffViewerLayout::Auto,
        };
        self.diff_viewer_layout_override = Some(next);
        let label = match next {
            TuiDiffViewerLayout::Auto => "automatic",
            TuiDiffViewerLayout::Unified => "unified",
            TuiDiffViewerLayout::SideBySide => "side-by-side",
        };
        self.set_status(format!("Diff layout: {label}"));
    }

    /// Return the currently selected provider plugin id, if explicit.
    #[must_use]
    pub fn selected_provider_plugin_id(&self) -> Option<&str> {
        self.session_view
            .snapshot()
            .runtime
            .provider_plugin_id
            .as_deref()
    }

    /// Return the currently selected model id, if explicit.
    #[must_use]
    pub fn selected_model_id(&self) -> Option<&str> {
        self.session_view
            .snapshot()
            .runtime
            .requested_model_id
            .as_deref()
    }

    /// Return the current agent id.
    #[must_use]
    pub fn current_agent_id(&self) -> &str {
        self.session_view
            .snapshot()
            .runtime
            .agent_id
            .as_deref()
            .unwrap_or("build")
    }

    /// Return the hydrated owning agent without a permissive presentation default.
    pub fn owning_agent_id(&self) -> Option<&str> {
        self.session_view.snapshot().runtime.agent_id.as_deref()
    }

    /// Return the pending agent id for the next submission, if one is staged.
    #[must_use]
    pub fn pending_agent_id(&self) -> Option<&str> {
        self.pending_agent_id.as_deref()
    }

    /// Return true once daemon-backed agent presentation metadata has hydrated.
    #[must_use]
    pub const fn is_agent_metadata_hydrated(&self) -> bool {
        matches!(
            self.agent_metadata_hydration,
            AgentMetadataHydration::Hydrated
        )
    }

    /// Set whether daemon-backed agent presentation metadata has hydrated.
    pub fn set_agent_metadata_hydrated(&mut self, hydrated: bool) {
        self.agent_metadata_hydration = if hydrated {
            AgentMetadataHydration::Hydrated
        } else {
            AgentMetadataHydration::Pending
        };
        self.sync_theme_target(Instant::now());
    }

    /// Return the theme currently presented by the UI.
    #[must_use]
    pub const fn presented_theme(&self) -> PresentedTheme {
        self.presented_theme
    }

    /// Advance theme animations for the supplied time.
    pub fn update_theme_animation(&mut self, now: Instant) -> UiInvalidation {
        let accent = self.theme_transition.update(now);
        self.presented_theme = self.target_theme.presented(accent);
        UiInvalidation::Paint
    }

    /// Return the current animated accent color for rendering.
    #[cfg(test)]
    pub fn animated_accent(&mut self, target_accent: Color, now: Instant) -> Color {
        self.retarget_theme_transition(target_accent, now);
        self.update_theme_animation(now);
        self.presented_theme.accent
    }

    /// Return true when a theme transition should request more frames.
    #[must_use]
    pub fn theme_transition_active(&self, now: Instant) -> bool {
        self.theme_transition.is_active(now)
    }

    /// Move theme transition state from another app after recreating app state.
    pub(crate) fn take_theme_transition_state_from(&mut self, source: &Self) {
        self.target_theme = source.target_theme;
        self.presented_theme = source.presented_theme;
        self.theme_transition = source.theme_transition;
        self.sync_theme_target(Instant::now());
    }

    fn sync_theme_target(&mut self, now: Instant) {
        let target = super::theme::resolve_theme(self);
        self.target_theme = target;
        self.retarget_theme_transition(target.accent, now);
        self.update_theme_animation(now);
    }

    /// Move the accent toward `target` over the duration the target theme's
    /// motion, the explicit accent settings, and `reduce_motion` select.
    fn retarget_theme_transition(&mut self, target: Color, now: Instant) {
        let explicit = self
            .tui_config
            .theme
            .explicit_accent_transition_ms()
            .map(Duration::from_millis);
        let duration = super::theme::effects::MotionPolicy::new(
            self.target_theme.effects,
            self.tui_config.reduce_motion,
        )
        .accent_transition(explicit);
        let curve = self.tui_config.theme.accent_transition_curve;
        self.theme_transition
            .set_target(target, duration, curve, now);
    }

    /// Return the agent id that should be presented in the UI.
    #[must_use]
    pub fn display_agent_id(&self) -> &str {
        self.pending_agent_id
            .as_deref()
            .unwrap_or_else(|| self.current_agent_id())
    }

    /// Return the configured current agent accent, if known.
    #[must_use]
    pub fn current_agent_accent(&self) -> Option<&str> {
        self.current_agent_accent.as_deref()
    }

    /// Return the agent accent that should be presented in the UI.
    #[must_use]
    pub fn display_agent_accent(&self) -> Option<&str> {
        self.pending_agent_accent
            .as_deref()
            .or_else(|| self.current_agent_accent())
    }

    /// Set the current agent id.
    pub fn set_current_agent_id(&mut self, agent_id: impl Into<String>) {
        let agent_id = agent_id.into();
        self.session_view.set_agent_id(Some(agent_id));
        self.current_agent_accent = None;
        self.clear_pending_agent_fields();
        self.sync_theme_target(Instant::now());
    }

    /// Set the current agent id and optional configured accent.
    pub fn set_current_agent(&mut self, agent_id: impl Into<String>, accent: Option<String>) {
        let agent_id = agent_id.into();
        self.session_view.set_agent_id(Some(agent_id));
        self.current_agent_accent = accent;
        self.clear_pending_agent_fields();
        self.sync_theme_target(Instant::now());
    }

    /// Stage an agent selection for the next submitted message.
    pub fn set_pending_agent(&mut self, agent_id: impl Into<String>, accent: Option<String>) {
        self.pending_agent_id = Some(agent_id.into());
        self.pending_agent_accent = accent;
        self.sync_theme_target(Instant::now());
    }

    fn clear_pending_agent_fields(&mut self) {
        self.pending_agent_id = None;
        self.pending_agent_accent = None;
    }

    /// Commit the staged agent selection locally and return its id, if present.
    pub fn take_pending_agent(&mut self) -> Option<String> {
        let agent_id = self.pending_agent_id.take()?;
        self.session_view.set_agent_id(Some(agent_id.clone()));
        self.current_agent_accent = self.pending_agent_accent.take();
        self.sync_theme_target(Instant::now());
        Some(agent_id)
    }

    /// Commit only the selection acknowledged by this submission, preserving newer draft choices.
    pub fn commit_submitted_agent(&mut self, agent_id: &str) {
        if self.pending_agent_id() == Some(agent_id) {
            let _ = self.take_pending_agent();
        } else {
            self.session_view.set_agent_id(Some(agent_id.to_owned()));
        }
    }

    /// Return the current reasoning output label.
    #[must_use]
    pub fn thinking_label(&self) -> &str {
        &self.thinking_label
    }

    /// Return the model label shown in the header.
    #[must_use]
    pub fn model_header_label(&self) -> String {
        let model = self
            .managed_label()
            .and_then(|_| {
                let settings = &self.managed_composer.settings;
                settings
                    .model_id
                    .as_deref()
                    .or(settings.observed_model.as_deref())
            })
            .or_else(|| self.session_view.snapshot().runtime.display_name.as_deref())
            .or_else(|| self.selected_model_id())
            .unwrap_or("default");
        self.reasoning_header_label().map_or_else(
            || model.to_owned(),
            |reasoning| format!("{model} [{reasoning}]"),
        )
    }

    fn reasoning_header_label(&self) -> Option<&str> {
        self.reasoning_support.is_supported().then(|| {
            self.reasoning_effort()
                .or(self.reasoning_default_effort.as_deref())
                .unwrap_or("supported")
        })
    }

    /// Return the token/context footer summary.
    #[must_use]
    pub fn token_summary(&self) -> String {
        self.token_usage.footer_summary(
            self.session_view
                .snapshot()
                .runtime
                .context_occupancy
                .as_ref(),
            self.session_view
                .snapshot()
                .runtime
                .cumulative_metered_tokens,
            &self.session_view.snapshot().runtime.cost,
            self.managed_execution_session().is_some(),
        )
    }

    /// Return the complete composer panel area from the latest render.
    #[must_use]
    pub const fn composer_panel_area(&self) -> Rect {
        let content = self.composer_content_area();
        Rect::new(
            content.x.saturating_sub(2),
            content.y.saturating_sub(1),
            content.width.saturating_add(4),
            content.height.saturating_add(2),
        )
    }

    /// Return the composer content area from the latest render.
    #[must_use]
    pub const fn composer_content_area(&self) -> Rect {
        self.composer_input.content_area()
    }

    /// Store the composer content area from the latest render.
    pub fn set_composer_content_area(&mut self, area: Rect) {
        let panel_size = bmux_tui::geometry::Size::new(
            area.width.saturating_add(4),
            area.height.saturating_add(2),
        );
        self.composer_state_mut().set_trailing_exclusion(
            super::composer_submit::trailing_exclusion(panel_size),
            &composer_policy(),
        );
        self.composer_input
            .set_content_area(area, &composer_policy());
    }

    /// Return the composer scroll offset that should be used for the latest content area.
    pub fn composer_scroll_offset_for_render(&self) -> usize {
        let offset = if self.composer_input.vertical_scroll() == u64::MAX {
            self.composer_input
                .cursor_scroll_offset(&composer_policy())
                .unwrap_or(0)
        } else {
            self.composer_input.vertical_scroll()
        };
        usize::try_from(offset).unwrap_or(usize::MAX)
    }

    /// Return the composer text input state.
    #[must_use]
    pub const fn composer_state(&self) -> &TextInputState {
        &self.composer_input
    }

    /// Return the composer text input state mutably.
    pub const fn composer_state_mut(&mut self) -> &mut TextInputState {
        &mut self.composer_input
    }

    /// Return the composer buffer.
    #[must_use]
    pub const fn composer(&self) -> &TextEditBuffer {
        self.composer_input.buffer()
    }

    /// Return the composer buffer mutably.
    pub fn composer_mut(&mut self) -> &mut TextEditBuffer {
        self.composer_input.buffer_mut()
    }

    /// Insert pasted text into the composer.
    pub fn paste_composer_text(&mut self, text: &str) {
        TextInputControl::new(&composer_policy()).handle_paste(&mut self.composer_input, text);
    }

    /// Accept bounded queue status through the shared semantic projection.
    pub fn set_submission_queue(&mut self, status: bcode_session_models::SubmissionQueueStatus) {
        self.queue_recovery.generation = self.queue_recovery.generation.wrapping_add(1);
        if self
            .queue_recovery
            .confirmation
            .as_ref()
            .is_some_and(|confirmation| {
                !status.oldest.as_ref().is_some_and(|record| {
                    record.receipt.submission_id == confirmation.submission_id
                        && record.receipt.accepted_event_sequence
                            == confirmation.expected_accepted_sequence
                })
            })
        {
            self.queue_recovery.confirmation = None;
            self.queue_recovery.settings_visible = false;
        }
        self.session_view.set_submission_queue(status);
    }

    /// Invalidate all pending recovery completions and dismiss local confirmation.
    pub fn dismiss_queue_confirmation(&mut self) {
        self.queue_recovery.generation = self.queue_recovery.generation.wrapping_add(1);
        self.queue_recovery.confirmation = None;
        self.queue_recovery.settings_visible = false;
    }

    /// Surface failed queue hydration and invalidate operations based on older observations.
    pub fn set_submission_queue_unavailable(&mut self) {
        self.dismiss_queue_confirmation();
        self.session_view.set_submission_queue_unavailable();
    }

    /// Accept only a completion captured after the most recent observation or dismissal.
    pub fn apply_queue_recovery_result(
        &mut self,
        generation: u64,
        outcome: bcode_session_view_models::SessionViewActionOutcome,
        status: bcode_session_models::SubmissionQueueStatus,
    ) -> bool {
        if self.queue_recovery.generation != generation {
            return false;
        }
        self.set_submission_queue(status);
        if let bcode_session_view_models::SessionViewActionOutcome::SubmissionResumePrepared {
            confirmation,
        } = outcome
        {
            self.prepare_queue_confirmation(confirmation);
        } else {
            self.dismiss_queue_confirmation();
        }
        true
    }

    /// Retain a prepared selection only while the shared queue still permits this exact resume.
    pub fn prepare_queue_confirmation(
        &mut self,
        confirmation: bcode_session_models::SubmissionResumeConfirmation,
    ) {
        self.queue_recovery.settings_visible = false;
        self.queue_recovery.confirmation = self
            .session_view
            .snapshot()
            .runtime
            .queue_recovery()
            .filter(|recovery| {
                recovery.can_resume
                    && recovery.submission_id == confirmation.submission_id
                    && recovery.expected_accepted_sequence
                        == confirmation.expected_accepted_sequence
            })
            .map(|_| confirmation);
    }

    /// Return the shared renderer-neutral semantic session state.
    #[must_use]
    pub const fn session_view_snapshot(&self) -> &bcode_session_view_models::SessionViewSnapshot {
        self.session_view.snapshot()
    }

    /// Whether the resident session history holds the canonical transcript item at `sequence`.
    #[must_use]
    pub fn has_transcript_sequence(&self, sequence: u64) -> bool {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .any(|item| item.sequence == Some(sequence))
    }

    /// Process-local notices in presentation order, each with the canonical entry it follows.
    pub fn ephemeral_notices(
        &self,
    ) -> impl Iterator<Item = &super::ephemeral_notices::EphemeralNotice> + '_ {
        self.notices.iter()
    }

    /// The canonical chronology boundary a notice raised now follows: the newest shown
    /// canonical transcript item and the newest canonical event sequence.
    fn notice_boundary(&self) -> super::ephemeral_notices::NoticeBoundary {
        let snapshot = self.session_view.snapshot();
        let reasoning_visible = snapshot.thinking.visible;
        let mut items = snapshot.transcript.items.iter().filter(|item| {
            reasoning_visible
                || !matches!(
                    item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ReasoningMessage { .. }
                        | bcode_session_view_models::TranscriptViewItemKind::ReasoningActivity { .. }
                )
        });
        let after_sequence = items.clone().filter_map(|item| item.sequence).max();
        super::ephemeral_notices::NoticeBoundary {
            after: items.next_back().map(|item| item.id.clone()),
            after_sequence,
        }
    }

    /// Return whether one invocation has reached authoritative terminal state.
    #[must_use]
    pub fn tool_invocation_is_terminal(&self, invocation_id: &str) -> bool {
        self.session_view
            .snapshot()
            .tools
            .get(invocation_id)
            .is_some_and(|tool| {
                matches!(
                    tool.status,
                    bcode_session_view_models::ToolInvocationViewStatus::Finished
                        | bcode_session_view_models::ToolInvocationViewStatus::Cancelled
                        | bcode_session_view_models::ToolInvocationViewStatus::Failed
                )
            })
    }

    /// Extend composer selection with an editor motion.
    pub fn extend_composer_selection(&mut self, motion: TextMotion) {
        self.input_history.reset_navigation();
        let width = usize::from(self.composer_input.content_area().width.max(1));
        match motion {
            TextMotion::VisualUp => self.extend_composer_selection_to_visual_delta(width, -1),
            TextMotion::VisualDown => self.extend_composer_selection_to_visual_delta(width, 1),
            motion => self
                .composer_input
                .buffer_mut()
                .move_cursor_with_selection(motion, SelectionMode::Extend),
        }
        self.wake_cursor();
    }

    /// Handle a composer mouse event through the reusable text-input component.
    pub fn handle_composer_mouse(&mut self, mouse: MouseEvent) -> TextInputOutcome {
        let outcome =
            TextInputControl::new(&composer_policy()).handle_mouse(&mut self.composer_input, mouse);
        if matches!(outcome, TextInputOutcome::Edited | TextInputOutcome::Redraw) {
            self.input_history.reset_navigation();
            self.wake_cursor();
        }
        outcome
    }

    /// Return whether a composer mouse selection is active.
    #[must_use]
    pub const fn composer_mouse_selection_active(&self) -> bool {
        self.composer_input.mouse_selection_active()
    }

    /// Move the composer cursor one rendered row up, if possible.
    pub fn move_composer_visual_up(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row up without leaving history navigation.
    pub fn move_composer_visual_up_preserving_history(&mut self) -> bool {
        self.move_composer_visual_up_with_history_reset(false)
    }

    /// Move the composer cursor one rendered row down, if possible.
    pub fn move_composer_visual_down(&mut self) -> bool {
        self.move_composer_visual_down_with_history_reset(true)
    }

    /// Move the composer cursor one rendered row down without leaving history navigation.
    pub fn move_composer_visual_down_preserving_history(&mut self) -> bool {
        self.move_composer_visual_down_with_history_reset(false)
    }

    fn move_composer_visual_up_with_history_reset(&mut self, reset_history: bool) -> bool {
        let width = usize::from(self.composer_input.content_area().width.max(1));
        let layout = self.composer_input.buffer().wrapped_layout(width);
        if layout.cursor.row == 0 {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer_input
            .buffer_mut()
            .move_cursor_to_wrapped_position(
                width,
                layout.cursor.row.saturating_sub(1),
                layout.cursor.col,
            );
        self.wake_cursor();
        true
    }

    fn move_composer_visual_down_with_history_reset(&mut self, reset_history: bool) -> bool {
        let width = usize::from(self.composer_input.content_area().width.max(1));
        let layout = self.composer_input.buffer().wrapped_layout(width);
        if layout.cursor.row.saturating_add(1) >= layout.lines.len() {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer_input
            .buffer_mut()
            .move_cursor_to_wrapped_position(
                width,
                layout.cursor.row.saturating_add(1),
                layout.cursor.col,
            );
        self.wake_cursor();
        true
    }

    /// Apply restored session runtime selection to the app.
    pub fn apply_runtime_selection(
        &mut self,
        selection: bcode_ipc::SessionRuntimeSelection,
        agent_accent: Option<String>,
    ) {
        let provider_plugin_id = selection
            .provider_plugin_id
            .or_else(|| self.selected_provider_plugin_id().map(ToOwned::to_owned));
        let requested_model_id = selection
            .requested_model_id
            .or(selection.model_id)
            .or_else(|| self.selected_model_id().map(ToOwned::to_owned));
        let effective_model_id = selection.effective_model_id.or_else(|| {
            self.session_view
                .snapshot()
                .runtime
                .effective_model_id
                .clone()
        });
        let context_occupancy = self
            .session_view
            .snapshot()
            .runtime
            .context_occupancy
            .clone();
        self.session_view.set_runtime_selection(
            provider_plugin_id,
            requested_model_id,
            effective_model_id,
            selection.reasoning_effort,
            selection.reasoning_summary,
            context_occupancy,
        );
        if let Some(agent_id) = selection.agent_id {
            self.session_view.set_agent_id(Some(agent_id));
            self.current_agent_accent = agent_accent;
            self.sync_theme_target(Instant::now());
        }
        self.refresh_thinking_label();
    }

    /// Apply hydrated model metadata to the app.
    pub fn apply_model_status(&mut self, status: bcode_model::SessionModelStatus) {
        let provider_plugin_id = status
            .provider_plugin_id
            .clone()
            .or_else(|| self.selected_provider_plugin_id().map(ToOwned::to_owned));
        let requested_model_id = status
            .requested_model_id
            .clone()
            .or_else(|| status.model_id.clone())
            .or_else(|| self.selected_model_id().map(ToOwned::to_owned));
        let effective_model_id = status.effective_model_id.clone().or_else(|| {
            self.session_view
                .snapshot()
                .runtime
                .effective_model_id
                .clone()
        });
        self.session_view.set_model_selection(
            provider_plugin_id,
            requested_model_id,
            effective_model_id,
            status.display_name.clone(),
        );
        if let Some(effort) = status.reasoning_effort.as_deref() {
            self.reconcile_pending_reasoning_effort(effort);
        }
        self.selected_auth_profile.clone_from(&status.auth_profile);
        self.selected_context_format_version = status.context_format_version;
        self.selected_compatibility_key
            .clone_from(&status.compatibility_key);
        self.session_view.set_reasoning_selection(
            status.reasoning_effort.clone(),
            status.reasoning_summary.clone(),
        );
        self.reasoning_support = if status.reasoning.is_some() {
            ReasoningSupport::Supported
        } else {
            ReasoningSupport::Unsupported
        };
        self.reasoning_effort_values = status
            .reasoning
            .as_ref()
            .map_or_else(Vec::new, |reasoning| {
                bcode_model::ordered_reasoning_effort_values(&reasoning.effort_values)
            });
        self.reasoning_default_effort = status
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.default_effort.clone());
        self.reasoning_default_summary = status
            .reasoning
            .as_ref()
            .and_then(|reasoning| reasoning.default_summary.clone());
        self.refresh_thinking_label();
        let model = status
            .context_window
            .map(|context_window| bcode_model::ModelInfo {
                model_id: self.selected_model_id().unwrap_or_default().to_owned(),
                display_name: status
                    .display_name
                    .clone()
                    .or_else(|| self.selected_model_id().map(ToOwned::to_owned))
                    .unwrap_or_default(),
                is_default: false,
                context_window: Some(context_window),
                max_output_tokens: status.max_output_tokens,
                max_image_input_base64_bytes: None,
                capabilities: std::collections::BTreeSet::new(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: status.reasoning.clone(),
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: status.pricing.clone(),
                api_surface: None,
                visibility: bcode_model::ModelVisibility::Visible,
            });
        self.token_usage.apply_model_info(model.as_ref());
        self.token_usage.proactive_compaction_target = status
            .proactive_compaction_effective_threshold_tokens
            .filter(|_| {
                matches!(
                    status.compaction_mode.as_deref(),
                    Some("proactive" | "proactive_and_overflow")
                )
            });
        self.apply_context_occupancy(status.context_occupancy.map(|occupancy| *occupancy));
    }

    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub fn pending_submissions(&self) -> &[PendingSubmission] {
        self.pending_submissions.items()
    }

    /// Accepted activity animation tick; drawing itself does not sample wall time.
    #[must_use]
    pub const fn activity_frame(&self) -> usize {
        self.activity_frame
    }

    /// Effective motion: the presented theme's level, forced to `none` by
    /// `[tui] reduce_motion`.
    #[must_use]
    pub const fn motion_policy(&self) -> super::theme::effects::MotionPolicy {
        super::theme::effects::MotionPolicy::new(
            self.presented_theme.effects,
            self.tui_config.reduce_motion,
        )
    }

    /// Override the presented and target theme's motion and `reduce_motion`.
    #[cfg(test)]
    pub(crate) const fn set_motion_for_test(
        &mut self,
        motion: bcode_tui_theme::chrome::Motion,
        reduce_motion: bool,
    ) {
        self.presented_theme.effects.motion = motion;
        self.target_theme.effects.motion = motion;
        self.tui_config.reduce_motion = reduce_motion;
    }

    /// Spinner step for the current activity tick under the effective motion.
    #[must_use]
    pub const fn spinner_step(&self) -> usize {
        self.motion_policy().spinner_step(self.activity_frame)
    }

    /// Focus gradient phase for the current activity tick; `0` unless the
    /// theme's `sweep` focus effect animates under the effective motion.
    #[must_use]
    pub fn focus_phase(&self) -> u16 {
        self.motion_policy().focus_phase(self.activity_frame)
    }

    /// Elapsed activity time accepted on the animation invalidation boundary.
    #[must_use]
    pub const fn activity_elapsed(&self) -> Duration {
        self.activity_elapsed
    }

    /// Pin or unpin resident transcript history for a logical selection.
    pub const fn set_transcript_selection_pinned(&mut self, pinned: bool) {
        self.transcript_selection_pinned = pinned;
    }

    /// Return resident transcript-affecting event count.
    #[cfg(test)]
    pub const fn resident_transcript_event_count(&self) -> usize {
        self.transcript_window.len()
    }

    /// Return the current activity state.
    #[must_use]
    pub const fn activity(&self) -> &ActivityState {
        &self.activity
    }

    /// Return when the current activity phase began.
    #[must_use]
    pub const fn activity_started_at(&self) -> Instant {
        self.activity_started_at
    }

    /// Return daemon connection state for startup/readiness chrome.
    #[must_use]
    pub const fn daemon_connection(&self) -> DaemonConnectionState {
        self.daemon_connection
    }

    /// Store daemon connection state for startup/readiness chrome.
    pub const fn set_daemon_connection(&mut self, daemon_connection: DaemonConnectionState) {
        self.daemon_connection = daemon_connection;
    }

    /// Return the persistent launch execution-mode indicator.
    #[must_use]
    pub fn execution_mode_indicator(&self) -> Option<&str> {
        self.execution_mode_indicator.as_deref()
    }

    /// Set the persistent launch execution-mode indicator.
    pub fn set_execution_mode_indicator(&mut self, indicator: Option<String>) {
        self.execution_mode_indicator = indicator;
    }

    /// Return the current status line.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Replace active skills from the bounded attach/reconnect snapshot.
    pub fn set_active_skills(&mut self, skills: &[bcode_skill_models::SkillContextResponse]) {
        self.session_view.set_active_skill_ids(
            skills
                .iter()
                .map(|skill| skill.skill_id.to_string())
                .collect(),
        );
    }

    /// Return the number of active skills maintained by shared session state.
    #[must_use]
    pub fn active_skill_count(&self) -> usize {
        self.session_view.snapshot().active_skills.len()
    }

    /// Replace authoritative pending permission state in the shared session view.
    pub fn set_pending_permission_views(
        &mut self,
        permissions: Vec<bcode_session_view_models::PermissionView>,
    ) {
        self.session_view.set_pending_permissions(permissions);
    }

    /// Replace authoritative pending interaction state in the shared session view.
    pub fn set_pending_interactions(
        &mut self,
        interactions: Vec<bcode_session_view_models::InteractionViewSummary>,
    ) {
        self.session_view.set_pending_interactions(interactions);
    }

    /// Return active plugin-owned session status contributions.
    pub fn plugin_status(
        &self,
    ) -> impl Iterator<Item = &bcode_session_view_models::PluginStatusView> {
        self.session_view.snapshot().plugin_status.values()
    }

    /// Atomically replace active plugin-owned session status contributions.
    pub fn set_plugin_status(
        &mut self,
        plugin_status: Vec<bcode_session_view_models::PluginStatusView>,
    ) {
        let activity: Vec<_> = plugin_status
            .iter()
            .map(|status| {
                (
                    format!(
                        "bcode.plugin-activity:{}:{}",
                        status.plugin_id, status.note_id
                    ),
                    status.text.clone(),
                )
            })
            .collect();
        let boundary = self.notice_boundary();
        let _changed = self.notices.set_plugin_activity(&activity, &boundary);
        self.session_view.set_plugin_status(plugin_status);
    }

    /// Return configured key hints for the status line.
    #[must_use]
    pub fn key_hints(&self) -> &str {
        &self.key_hints
    }

    /// Clear any pending multi-tap key activation.
    pub const fn clear_pending_key_activation(&mut self) {
        self.pending_key_activation = None;
    }

    /// Evaluate whether a key binding should run now or wait for more taps.
    pub fn activate_key_binding(
        &mut self,
        scope: BmuxScope,
        binding: &BmuxKeyBinding,
    ) -> KeyActivationOutcome {
        self.activate_key_binding_at(scope, binding, Instant::now())
    }

    #[cfg(test)]
    pub(crate) fn activate_key_binding_for_test(
        &mut self,
        scope: BmuxScope,
        binding: &BmuxKeyBinding,
        now: Instant,
    ) -> KeyActivationOutcome {
        self.activate_key_binding_at(scope, binding, now)
    }

    fn activate_key_binding_at(
        &mut self,
        scope: BmuxScope,
        binding: &BmuxKeyBinding,
        now: Instant,
    ) -> KeyActivationOutcome {
        match binding.activation() {
            BmuxKeyActivation::Immediate => {
                self.pending_key_activation = None;
                KeyActivationOutcome::Activated(binding.action())
            }
            BmuxKeyActivation::MultiTap {
                required_taps,
                window_ms,
                prompt,
            } => self.activate_multi_tap_binding(
                scope,
                binding,
                *required_taps,
                *window_ms,
                prompt,
                now,
            ),
        }
    }

    fn activate_multi_tap_binding(
        &mut self,
        scope: BmuxScope,
        binding: &BmuxKeyBinding,
        required_taps: u8,
        window_ms: u64,
        prompt: &str,
        now: Instant,
    ) -> KeyActivationOutcome {
        let taps = self
            .pending_key_activation
            .filter(|pending| {
                pending.scope == scope
                    && pending.stroke == binding.stroke()
                    && pending.action == binding.action()
                    && pending.expires_at >= now
            })
            .map_or(1, |pending| pending.taps.saturating_add(1));

        if taps >= required_taps.max(1) {
            self.pending_key_activation = None;
            return KeyActivationOutcome::Activated(binding.action());
        }

        self.pending_key_activation = Some(PendingKeyActivation {
            scope,
            stroke: binding.stroke(),
            action: binding.action(),
            taps,
            expires_at: now + Duration::from_millis(window_ms),
        });
        self.set_status(prompt.to_owned());
        KeyActivationOutcome::Pending
    }

    /// Append a process-local plain-text system notice that is not canonical history.
    pub fn push_ephemeral_system_plain(&mut self, text: String) {
        self.push_ephemeral_system_notice(text, bcode_session_view_models::TextFormat::PlainText);
    }

    /// Append a process-local JSON system notice that is not canonical history.
    pub fn push_ephemeral_system_json(&mut self, text: String) {
        self.push_ephemeral_system_notice(text, bcode_session_view_models::TextFormat::Json);
    }

    /// Append a process-local Markdown system notice that is not canonical history.
    pub fn push_ephemeral_system_markdown(&mut self, text: String) {
        self.push_ephemeral_system_notice(text, bcode_session_view_models::TextFormat::Markdown);
    }

    fn push_ephemeral_system_notice(
        &mut self,
        text: String,
        format: bcode_session_view_models::TextFormat,
    ) {
        let boundary = self.notice_boundary();
        self.notices.push(text, format, boundary);
    }

    /// Replace the current status line.
    pub fn set_status(&mut self, status: String) {
        self.status = status;
    }

    /// Return whether reasoning transcript items are visible.
    #[must_use]
    pub const fn reasoning_visible(&self) -> bool {
        self.session_view.snapshot().thinking.visible
    }

    /// Set whether reasoning transcript items are visible.
    pub fn set_reasoning_visible(&mut self, visible: bool) {
        if self.reasoning_visible() == visible {
            return;
        }
        self.session_view.set_reasoning_visible(visible);
        self.refresh_thinking_label();
    }

    /// Apply selected reasoning request settings locally without changing transcript presentation.
    pub fn apply_reasoning_selection(&mut self, effort: Option<String>, summary: Option<String>) {
        self.session_view.set_reasoning_selection(effort, summary);
        self.refresh_thinking_label();
    }

    /// Stage the next semantically ordered advertised effort for the next message.
    pub fn cycle_pending_reasoning_effort(&mut self) -> Option<&str> {
        let next = bcode_model::next_reasoning_effort_value(
            &self.reasoning_effort_values,
            self.reasoning_effort(),
        )?;
        self.set_pending_reasoning_effort(next);
        self.reasoning_effort()
    }

    /// Stage an effort selection for the next submitted message.
    pub fn set_pending_reasoning_effort(&mut self, effort: String) -> u64 {
        self.reasoning_effort_generation = self.reasoning_effort_generation.saturating_add(1);
        let generation = self.reasoning_effort_generation;
        self.pending_reasoning_effort = Some(PendingReasoningEffort {
            value: effort,
            generation,
        });
        self.refresh_thinking_label();
        generation
    }

    /// Return the pending effort generation for submission reconciliation.
    #[must_use]
    pub fn pending_reasoning_effort_generation(&self) -> Option<u64> {
        self.pending_reasoning_effort
            .as_ref()
            .map(|pending| pending.generation)
    }

    /// Clear any pending effort when leaving its draft/session scope.
    pub fn clear_pending_reasoning_effort_for_session_change(&mut self) {
        if self.pending_reasoning_effort.take().is_some() {
            self.refresh_thinking_label();
        }
    }

    /// Clear a pending effort when canonical state confirms its exact value.
    pub fn reconcile_pending_reasoning_effort(&mut self, effort: &str) {
        if self
            .pending_reasoning_effort
            .as_ref()
            .is_some_and(|pending| pending.value == effort)
        {
            self.pending_reasoning_effort = None;
            self.refresh_thinking_label();
        }
    }

    /// Clear the exact pending effort captured by a completed submission.
    pub fn clear_pending_reasoning_effort(&mut self, generation: u64) {
        if self
            .pending_reasoning_effort
            .as_ref()
            .is_some_and(|pending| pending.generation == generation)
        {
            self.pending_reasoning_effort = None;
            self.refresh_thinking_label();
        }
    }

    /// Return the pending reasoning effort, if one overrides canonical session state.
    #[must_use]
    pub fn pending_reasoning_effort(&self) -> Option<&str> {
        self.pending_reasoning_effort
            .as_ref()
            .map(|pending| pending.value.as_str())
    }

    /// Return the effective reasoning effort for the next submission, if any.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.pending_reasoning_effort.as_ref().map_or_else(
            || {
                self.session_view
                    .snapshot()
                    .runtime
                    .reasoning_effort
                    .as_deref()
            },
            |pending| Some(pending.value.as_str()),
        )
    }

    /// Return the selected reasoning summary, if any.
    #[must_use]
    pub fn reasoning_summary(&self) -> Option<&str> {
        self.session_view
            .snapshot()
            .runtime
            .reasoning_summary
            .as_deref()
    }

    /// Apply configured reasoning output visibility.
    pub fn apply_thinking_config(&mut self, config: TuiThinkingConfig) {
        self.set_reasoning_display_mode(config.mode);
        self.set_reasoning_visible(config.show);
    }

    /// Return the local reasoning content display mode.
    #[must_use]
    pub const fn reasoning_display_mode(&self) -> bcode_config::TuiThinkingMode {
        self.reasoning_display_mode
    }

    /// Set the local reasoning content display mode.
    pub fn set_reasoning_display_mode(&mut self, mode: bcode_config::TuiThinkingMode) {
        if self.reasoning_display_mode != mode {
            self.reasoning_display_mode = mode;
            self.session_view
                .set_reasoning_display_mode(reasoning_view_mode(mode));
            self.refresh_thinking_label();
        }
    }

    fn refresh_thinking_label(&mut self) {
        let display = if self.reasoning_visible() {
            match self.reasoning_display_mode {
                bcode_config::TuiThinkingMode::All => "display: all",
                bcode_config::TuiThinkingMode::Summary => "display: summaries",
                bcode_config::TuiThinkingMode::Raw => "display: raw",
            }
        } else {
            "display: hidden"
        };
        if !self.reasoning_support.is_supported() {
            self.thinking_label = format!("{display} · unsupported by current model");
            return;
        }
        let effort = self
            .reasoning_effort()
            .or(self.reasoning_default_effort.as_deref())
            .unwrap_or("provider default");
        let summary = self
            .reasoning_summary()
            .or(self.reasoning_default_summary.as_deref())
            .unwrap_or("not requested");
        self.thinking_label =
            format!("{display} · request effort: {effort} · provider summary: {summary}");
    }

    /// Mark the app as waiting for turn cancellation.
    pub fn set_cancelling(&mut self) {
        self.set_activity(ActivityState::Cancelling);
    }

    /// Return the app to idle activity.
    pub fn set_idle(&mut self) {
        self.set_activity(ActivityState::Idle);
    }

    /// Store the current composer text as a pending submission and clear input.
    pub fn stage_submission(&mut self) {
        self.composer_submit.dismiss();
        let text = self.composer_input.buffer().text().to_owned();
        self.pending_submissions.stage(text);
        self.input_history.reset_navigation();
        self.composer_input.buffer_mut().clear();
    }

    /// Return the currently pending submission.
    pub fn take_pending_submission(&mut self) -> String {
        self.pending_submissions.take_staged()
    }

    /// Remove a pending submission that was handled outside the session transcript.
    pub fn clear_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
    }

    /// Mark the oldest pending submission as queued by the server.
    pub fn mark_pending_submission_queued(&mut self, queue_position: Option<u32>) {
        self.pending_submissions.mark_first_queued(queue_position);
    }

    /// Mark the oldest pending submission as sent to the server.
    pub fn mark_pending_submission_sent(&mut self) {
        self.pending_submissions.mark_first_sent();
    }

    /// Remove a pending submission and restore it into the composer.
    pub fn restore_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
        self.composer_input.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub fn previous_input_history(&mut self) -> bool {
        match self
            .input_history
            .previous(self.composer_input.buffer().text())
        {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_from_history(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_from_history(&text);
                "draft restored".clone_into(&mut self.status);
            }
            InputHistoryOutcome::Empty => {
                "no input history in this session".clone_into(&mut self.status);
            }
            InputHistoryOutcome::NotBrowsing => {
                "not browsing input history".clone_into(&mut self.status);
            }
        }
        true
    }

    /// Show the next input-history entry, or restore the draft.
    pub fn next_input_history(&mut self) -> bool {
        match self.input_history.next() {
            InputHistoryOutcome::Entry { index, total, text } => {
                self.replace_composer_from_history(&text);
                self.status = format!("input history {index}/{total}");
            }
            InputHistoryOutcome::DraftRestored(text) => {
                self.replace_composer_from_history(&text);
                "draft restored".clone_into(&mut self.status);
            }
            InputHistoryOutcome::Empty => {
                "no input history in this session".clone_into(&mut self.status);
            }
            InputHistoryOutcome::NotBrowsing => {
                "not browsing input history".clone_into(&mut self.status);
            }
        }
        true
    }

    /// Return whether input-history navigation is active.
    #[must_use]
    pub const fn input_history_navigation_active(&self) -> bool {
        self.input_history.is_browsing()
    }

    /// Reset active input-history navigation after direct composer editing.
    pub fn reset_input_history_navigation(&mut self) {
        self.input_history.reset_navigation();
    }

    fn active_tool_loop(&self) -> bool {
        self.session_view.snapshot().tools.values().any(|tool| {
            matches!(
                tool.status,
                bcode_session_view_models::ToolInvocationViewStatus::Requested
                    | bcode_session_view_models::ToolInvocationViewStatus::Running
                    | bcode_session_view_models::ToolInvocationViewStatus::Waiting
            )
        })
    }

    /// Absorb replayed history events.
    pub fn absorb_history(&mut self, events: &[SessionEvent]) {
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.append_history(events);
        for event in events {
            self.session_view.apply_event(event);
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.trim_resident_transcript_window_if_needed();
    }

    /// Replace the resident session history with the latest bounded window after a stream
    /// resynchronization; the host shell's session view is rebuilt from it.
    pub fn replace_session_history(&mut self, events: &[SessionEvent]) {
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.session_view.clear_history_window();
        self.session_view.apply_history(events);
        for event in events {
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
    }

    /// Rebuild the host shell's session view from the resident event window, keeping live state.
    fn rebuild_session_view_from_history(&mut self) {
        let events = self.transcript_window.events().to_vec();
        self.session_view.rebuild_history_window(&events);
        for event in &events {
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
    }

    fn trim_resident_transcript_window_if_needed(&mut self) {
        let trim = self
            .transcript_window
            .trim_if_allowed(self.resident_transcript_window_policy());
        if !trim.trimmed() {
            return;
        }
        self.rebuild_session_view_from_history();
    }

    fn resident_transcript_window_policy(&self) -> TranscriptWindowPolicy {
        TranscriptWindowPolicy {
            max_events: RESIDENT_TRANSCRIPT_MAX_EVENTS,
            target_events: RESIDENT_TRANSCRIPT_TARGET_EVENTS,
            allow_trim: self.can_trim_resident_transcript_window(),
        }
    }

    /// Return whether resident transcript trimming is currently permitted.
    #[cfg(test)]
    #[must_use]
    pub fn can_trim_resident_transcript_window_for_test(&self) -> bool {
        self.can_trim_resident_transcript_window()
    }

    fn can_trim_resident_transcript_window(&self) -> bool {
        // Rebuilding after a trim replays committed user messages, which forget pending
        // submissions by text; keep the window intact until every pending submission is
        // reconciled by the stream.
        !self.transcript_selection_pinned
            && !self.active_tool_loop()
            && self.pending_submissions.items().is_empty()
    }

    /// Absorb one live session event.
    #[allow(clippy::too_many_lines)]
    pub fn absorb_session_event(&mut self, event: &SessionEvent) {
        if matches!(
            &event.kind,
            SessionEventKind::TraceEvent { trace }
                if matches!(trace.payload, SessionTracePayload::ContextCompaction { .. })
        ) && event.sequence != 0
            && self
                .session_view
                .snapshot()
                .latest_sequence
                .is_some_and(|sequence| event.sequence <= sequence)
        {
            return;
        }
        if event_affects_transcript_rows(event)
            && self
                .latest_history_sequence
                .is_some_and(|sequence| event.sequence <= sequence)
        {
            tracing::debug!(target: "bcode_tui::session_stream", sequence = event.sequence,
                latest = ?self.latest_history_sequence, "live event already covered by history");
            return;
        }
        if event_affects_transcript_rows(event) {
            self.transcript_window.append_live_event(event);
        }
        self.session_view.apply_event(event);
        self.apply_session_event(event, SessionEventApplication::Live);
        self.trim_resident_transcript_window_if_needed();
    }

    /// Absorb one live-only session event.
    pub fn absorb_session_live_event(&mut self, event: &SessionLiveEvent) {
        self.session_view.apply_live_event(event);
        self.apply_session_live_event_side_effects(event);
    }

    fn apply_session_live_event_side_effects(&mut self, event: &SessionLiveEvent) {
        match &event.kind {
            SessionLiveEventKind::AssistantTextDelta { text, .. } => {
                self.add_streaming_delta(text, SessionEventApplication::Live);
            }
            SessionLiveEventKind::UsageSummaryChanged { .. } => {
                if let Some(usage) = &self.session_view.snapshot().runtime.latest_usage {
                    self.token_usage.absorb(usage);
                }
            }
            SessionLiveEventKind::AssistantTextStreamUpdated { .. }
            | SessionLiveEventKind::AssistantReasoningDelta { .. }
            | SessionLiveEventKind::AssistantReasoningTextStreamUpdated { .. }
            | SessionLiveEventKind::AssistantReasoningActivity { .. }
            | SessionLiveEventKind::ToolContributionPlaced { .. }
            | SessionLiveEventKind::ToolPresentationUpdated { .. }
            | SessionLiveEventKind::RequestContextOccupancyChanged { .. }
            | SessionLiveEventKind::ToolRequestDraft { .. } => {
                // SessionView already owns and projects the semantic update.
            }
            SessionLiveEventKind::ToolInvocationProgress { .. } => {
                self.apply_shared_runtime_work_activity();
            }
            SessionLiveEventKind::ProviderStreamProgress { event, .. } => {
                self.apply_shared_provider_stream_progress(event);
            }
        }
    }

    #[allow(clippy::too_many_lines)]
    fn apply_session_event(&mut self, event: &SessionEvent, application: SessionEventApplication) {
        if bcode_session_view::submission_queue_needs_refresh(&event.kind) {
            self.queue_recovery.generation = self.queue_recovery.generation.wrapping_add(1);
        }

        match &event.kind {
            SessionEventKind::ExternalHarnessOperation { .. }
            | SessionEventKind::ExternalBackendBound { .. }
            | SessionEventKind::ExternalSubmissionAccepted { .. }
            | SessionEventKind::ExternalDeliveryObserved { .. }
            | SessionEventKind::ExternalObservationRecorded { .. } => {
                if let Some(model) = reported_native_model(event) {
                    self.managed_composer.settings.observed_model = Some(model);
                }
                // Native history identifies the backend, not execution authority. Default Send
                // explicitly queues a follow-up; steering requires a user-selected native turn.
                // Only live background work is worth a status line; the lifecycle's evidence
                // detail stays available through the session's execution view.
                let lifecycle = &self.session_view_snapshot().execution_lifecycle;
                let running = lifecycle.live_background_work();
                if lifecycle.session_id.is_some() && running > 0 {
                    self.set_status(format!(
                        "{running} background task{} running",
                        if running == 1 { "" } else { "s" }
                    ));
                } else if self.status.ends_with(" running")
                    && self.status.contains(" background task")
                {
                    // The work it reported has finished.
                    self.status.clear();
                }
                if self.managed_composer.placement.is_none() {
                    self.managed_composer.placement =
                        Some(bcode_harness_models::HarnessPlacement::NextTurn);
                }
            }
            SessionEventKind::SubmissionAccepted { submission } => {
                self.pending_submissions.remove_id(&submission.id);
                self.set_status(match submission.requested_placement {
                    bcode_session_models::SubmissionPlacement::Steer => {
                        "Steering pending".to_owned()
                    }
                    bcode_session_models::SubmissionPlacement::FollowUp => {
                        "Follow-up pending".to_owned()
                    }
                });
            }
            SessionEventKind::SubmissionCancelled { .. } if application.live_activity() => {
                self.set_status("Submission cancelled before activation".to_owned());
            }
            SessionEventKind::SubmissionActivated {
                submission_id,
                text,
                ..
            } => {
                self.pending_submissions.remove_id(submission_id);
                self.input_history
                    .push_committed(event.sequence, event.timestamp_ms, text);
                if application.live_activity() {
                    self.set_status("Submission activated".to_owned());
                }
            }
            SessionEventKind::UserMessage { text, .. } => {
                self.apply_committed_user_message(
                    event.sequence,
                    text,
                    event.timestamp_ms,
                    application,
                );
            }
            SessionEventKind::AssistantDelta { text } => {
                self.add_streaming_delta(text, application);
            }
            SessionEventKind::AssistantMessage { .. }
            | SessionEventKind::AssistantResponseSegment { .. } => {
                if application.live_activity()
                    && matches!(self.activity, ActivityState::Streaming { .. })
                {
                    self.set_activity(ActivityState::FinalizingModelTurn);
                }
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
                ..
            } => {
                self.apply_tool_request_side_effects(tool_call_id, tool_name, arguments_json);
            }
            SessionEventKind::ToolInvocationResultRecorded { record } => {
                // A replayed managed conversation has no later Bcode turn event to end the
                // tool's activity, so its recorded result ends it.
                if application.live_activity() || self.managed_execution_session().is_some() {
                    self.set_follow_up_activity();
                }
                self.push_tool_result(
                    &record.invocation_id,
                    &record.model_output,
                    record.is_error,
                    record.result.as_ref(),
                    application,
                );
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
                policy_source,
                policy_reason,
                ..
            } => {
                self.push_permission_request(PermissionRequestInput {
                    event_sequence: event.sequence,
                    permission_id,
                    tool_call_id,
                    tool_name,
                    arguments_json,
                    policy_source: policy_source.as_deref(),
                    policy_reason: policy_reason.as_deref(),
                    application,
                });
            }
            SessionEventKind::PermissionResolved {
                permission_id,
                approved: _,
            } => {
                if application.live_activity() {
                    self.set_follow_up_activity();
                }
                self.set_permission_status(permission_id);
            }
            SessionEventKind::ModelChanged {
                provider, model, ..
            } => {
                self.apply_shared_model_changed(provider, model);
            }
            SessionEventKind::ReasoningChanged { effort, .. } => {
                if let Some(effort) = effort {
                    self.reconcile_pending_reasoning_effort(effort);
                }
                self.refresh_thinking_label();
            }
            SessionEventKind::ModelTurnStarted { .. } if application.live_activity() => {
                self.apply_shared_model_turn_started();
            }
            SessionEventKind::ModelTurnCancelRequested { .. } if application.live_activity() => {
                self.apply_shared_model_turn_cancel_requested();
            }
            SessionEventKind::ModelTurnFinished { .. } => {
                self.finish_shared_model_turn(application);
            }
            SessionEventKind::ModelUsage { turn_id, usage } => {
                self.push_model_usage(event.sequence, turn_id, usage, application);
            }
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory: _,
            } => self.apply_working_directory_changed(event.sequence, old_working_directory),
            SessionEventKind::SessionRenamed { .. }
            | SessionEventKind::AutomaticTitleUpdated { .. } => self.apply_shared_session_renamed(),
            SessionEventKind::SkillSuggested { skill_id, .. } => {
                self.status = format!("suggested skill: {skill_id}");
            }
            SessionEventKind::SkillActivated { skill_id, .. } => {
                self.apply_shared_skill_activated(skill_id);
            }
            SessionEventKind::SkillDeactivated { skill_id, .. } => {
                self.apply_shared_skill_deactivated(skill_id);
            }
            SessionEventKind::SkillContextLoaded {
                skill_id,
                bytes_loaded,
                truncated,
                ..
            } => {
                let suffix = if *truncated { " truncated" } else { "" };
                self.status =
                    format!("loaded skill context: {skill_id} ({bytes_loaded} bytes{suffix})");
            }
            SessionEventKind::AssistantReasoningMessage { .. }
                if application.live_activity()
                    && matches!(self.activity, ActivityState::Streaming { .. }) =>
            {
                self.set_activity(ActivityState::FinalizingModelTurn);
            }
            SessionEventKind::AgentChanged { .. } => {
                self.apply_shared_agent_changed();
            }
            SessionEventKind::TraceEvent { trace } if application.live_activity() => {
                self.apply_trace_event(trace);
            }
            SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => {
                if lifecycle.stage == bcode_session_models::ToolInvocationLifecycleStage::Started
                    && application.live_activity()
                    && let Some(tool_name) = self
                        .session_view
                        .snapshot()
                        .tools
                        .get(&lifecycle.invocation_id)
                        .and_then(|tool| tool.tool_name.clone())
                {
                    self.set_file_activity(&tool_name);
                    tool_name.clone_into(&mut self.status);
                }
                if application.live_activity() {
                    self.apply_shared_runtime_work_activity();
                }
            }
            SessionEventKind::RuntimeWorkStarted { .. }
            | SessionEventKind::RuntimeWorkCancelRequested { .. }
            | SessionEventKind::RuntimeWorkProgress { .. }
                if application.live_activity() =>
            {
                self.apply_shared_runtime_work_activity();
            }
            SessionEventKind::RuntimeWorkFinished {
                work_id,
                status,
                message,
                ..
            } => {
                if application.live_activity() {
                    self.apply_shared_runtime_work_activity();
                }
                let _ = (work_id, status, message);
            }
            _ => {}
        }
    }

    pub fn apply_runtime_work_snapshots(
        &mut self,
        snapshots: &[bcode_session_models::RuntimeWorkSnapshot],
    ) {
        self.session_view.set_runtime_work_snapshots(snapshots);
        self.apply_shared_runtime_work_activity();
    }

    /// Return whether the composer cursor should be visible.
    #[must_use]
    pub const fn cursor_visible(&self) -> bool {
        self.cursor.visible()
    }

    /// Reset cursor blink state after input.
    pub fn wake_cursor(&mut self) {
        self.cursor.wake();
    }

    /// Return currently requested timed invalidations.
    #[must_use]
    pub fn invalidation_requests(&self, now: Instant) -> Vec<InvalidationRequest> {
        let mut requests = vec![self.cursor.invalidation_request()];
        if let Some(next_frame_at) = self.theme_transition.next_frame_at {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(THEME_TRANSITION_INVALIDATION_KEY),
                next_frame_at,
            ));
        }
        if self.activity_animation_active() {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(ACTIVITY_ANIMATION_INVALIDATION_KEY),
                next_periodic_deadline(
                    self.activity_started_at,
                    now,
                    self.motion_policy().activity_frame_interval(),
                ),
            ));
        }
        requests
    }

    /// Handle generic timed invalidation keys.
    pub fn handle_invalidations(
        &mut self,
        keys: &[InvalidationKey],
        now: Instant,
    ) -> UiInvalidation {
        self.handle_invalidations_with_damage(keys, now).0
    }

    /// Handle timed invalidations and return their renderer-local terminal damage classes.
    pub fn handle_invalidations_with_damage(
        &mut self,
        keys: &[InvalidationKey],
        now: Instant,
    ) -> (UiInvalidation, Vec<TemporalDamage>) {
        let mut damage = BTreeSet::new();
        let invalidation = keys.iter().fold(UiInvalidation::None, |invalidation, key| {
            if self.cursor.handle_invalidation(key, now) {
                damage.insert(TemporalDamage::Composer);
                invalidation.merge(UiInvalidation::Paint)
            } else if is_theme_transition_invalidation(key) {
                self.update_theme_animation(now);
                if !self.theme_transition_active(now) {
                    self.theme_transition.finish();
                    self.presented_theme = self.target_theme.presented(self.target_theme.accent);
                }
                damage.insert(TemporalDamage::Full);
                invalidation.merge(UiInvalidation::Paint)
            } else if is_activity_animation_invalidation(key) {
                self.activity_frame = self.activity_frame.wrapping_add(1);
                self.activity_elapsed = now.saturating_duration_since(self.activity_started_at);
                if self.activity_animation_active() {
                    damage.insert(TemporalDamage::Status);
                    if self.motion_policy().focus_sweeps()
                        && super::render::focus_gradient_painted()
                    {
                        // Focused gradient borders live in overlays anywhere
                        // on screen; repaint only while one is painted.
                        damage.insert(TemporalDamage::Full);
                    }
                    invalidation.merge(UiInvalidation::Paint)
                } else {
                    invalidation
                }
            } else {
                invalidation
            }
        });
        (invalidation, damage.into_iter().collect())
    }

    const fn activity_animation_active(&self) -> bool {
        !matches!(self.activity, ActivityState::Idle)
            || matches!(
                self.daemon_connection,
                DaemonConnectionState::Connecting | DaemonConnectionState::Starting
            )
    }

    /// Return whether the TUI should exit.
    #[must_use]
    pub const fn should_exit(&self) -> bool {
        self.exit.requested()
    }

    /// Request TUI shutdown.
    pub const fn request_exit(&mut self) {
        self.exit.request();
    }

    /// Replace composer contents.
    pub fn replace_composer_with(&mut self, text: &str) {
        self.replace_composer_with_policy(text, true);
    }

    fn replace_composer_from_history(&mut self, text: &str) {
        self.replace_composer_with_policy(text, false);
    }

    fn replace_composer_with_policy(&mut self, text: &str, reset_history: bool) {
        self.composer_submit.dismiss();
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer_input.buffer_mut().clear();
        self.composer_input.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Apply a locally selected model before a persisted session exists.
    pub fn apply_local_model_selection(&mut self, provider: Option<String>, model: &str) {
        self.session_view.set_model_selection(
            provider,
            model_to_display_selection(model),
            None,
            None,
        );
        self.reasoning_support = ReasoningSupport::Unsupported;
        self.reasoning_effort_values.clear();
        self.pending_reasoning_effort = None;
        self.token_usage.clear_model_info();
        let provider_label = self.selected_provider_plugin_id().unwrap_or("auto");
        self.status = format!("model selected for next session: {provider_label}/{model}");
    }

    fn apply_shared_model_changed(&mut self, provider: &str, model: &str) {
        debug_assert_eq!(
            self.selected_provider_plugin_id(),
            provider_to_display_selection(provider).as_deref()
        );
        debug_assert_eq!(
            self.selected_model_id(),
            model_to_display_selection(model).as_deref()
        );
        self.reasoning_support = ReasoningSupport::Unsupported;
        self.reasoning_effort_values.clear();
        self.pending_reasoning_effort = None;
        self.token_usage.clear_model_info();
        self.status = format!("model: {provider}/{model}");
    }

    fn apply_shared_agent_changed(&mut self) {
        debug_assert!(self.session_view.snapshot().runtime.agent_id.is_some());
        self.current_agent_accent = None;
        self.clear_pending_agent_fields();
        self.sync_theme_target(Instant::now());
    }

    fn extend_composer_selection_to_visual_delta(&mut self, width: usize, delta: isize) {
        let layout = self.composer_input.buffer().wrapped_layout(width);
        let target_row = if delta.is_negative() {
            layout.cursor.row.saturating_sub(delta.unsigned_abs())
        } else {
            layout
                .cursor
                .row
                .saturating_add(delta.unsigned_abs())
                .min(layout.lines.len().saturating_sub(1))
        };
        self.composer_input.buffer_mut().select_to_wrapped_position(
            width,
            target_row,
            layout.cursor.col,
        );
    }

    fn apply_shared_session_renamed(&mut self) {
        self.status = self.session_title().map_or_else(
            || "session renamed".to_owned(),
            |name| format!("session: {name}"),
        );
    }

    fn apply_shared_skill_activated(&mut self, skill_id: &impl std::fmt::Display) {
        let skill_id = skill_id.to_string();
        if self
            .session_view
            .snapshot()
            .active_skills
            .contains(&skill_id)
        {
            self.status = format!("activated skill: {skill_id}");
        }
    }

    fn apply_shared_skill_deactivated(&mut self, skill_id: &impl std::fmt::Display) {
        let skill_id = skill_id.to_string();
        if !self
            .session_view
            .snapshot()
            .active_skills
            .contains(&skill_id)
        {
            self.status = format!("deactivated skill: {skill_id}");
        }
    }

    fn remove_pending_submission(&mut self, text: &str) {
        self.pending_submissions.remove(text);
    }

    /// Displayed text of the committed user message at `sequence`.
    fn committed_user_message_text(&self, sequence: u64, text: &str) -> String {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| item.sequence == Some(sequence))
            .and_then(|item| match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::UserMessage { message } => {
                    Some(message.text.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| text.to_owned())
    }

    /// The pending submission the committed user message at `sequence` accepts, if any. Asked
    /// before the message is applied, because applying it forgets the pending submission.
    #[must_use]
    pub fn pending_submission_accepted_by(
        &self,
        sequence: u64,
        text: &str,
    ) -> Option<&PendingSubmission> {
        let text = self.committed_user_message_text(sequence, text);
        self.pending_submissions
            .items()
            .iter()
            .find(|pending| pending.text() == text)
    }

    /// Whether the committed user message at `sequence` accepts one of this client's pending
    /// submissions. Asked before the message is applied, because applying it forgets the
    /// pending submission.
    #[must_use]
    pub fn accepts_own_pending_submission(&self, sequence: u64, text: &str) -> bool {
        self.pending_submission_accepted_by(sequence, text)
            .is_some()
    }

    fn apply_committed_user_message(
        &mut self,
        sequence: u64,
        text: &str,
        timestamp_ms: u64,
        application: SessionEventApplication,
    ) {
        let text = self.committed_user_message_text(sequence, text);
        self.input_history
            .push_committed(sequence, timestamp_ms, &text);
        self.remove_pending_submission(&text);
        if application.live_activity() {
            self.set_activity(ActivityState::PreparingModelRequest);
        }
    }

    /// Apply bounded authoritative submission lifecycle hydration.
    /// Remove only the optimistic item covered by a managed delivery receipt.
    /// Bind input policy before the first managed composer submission.
    pub fn set_managed_execution(&mut self, session_id: SessionId) {
        self.managed_composer.execution = ManagedExecutionPolicy {
            session_id: Some(session_id),
            input_ready: true,
            auth_requested: false,
            exit_request: None,
        };
        self.managed_composer.placement = Some(bcode_harness_models::HarnessPlacement::NextTurn);
        self.refresh_managed_indicator();
    }

    fn refresh_managed_indicator(&mut self) {
        // The header already names the executor and model; the status line only flags a
        // non-default permission mode, so token and cost segments keep their room.
        let indicator = self
            .managed_composer
            .settings
            .permission_mode
            .filter(|mode| *mode != bcode_harness_models::HarnessPermissionMode::Default)
            .map(|mode| super::managed_controls::permission_mode_label(mode).to_owned());
        self.set_execution_mode_indicator(indicator);
    }

    /// Record an acknowledged native setting and show it in the status indicator.
    pub fn apply_managed_setting(&mut self, setting: &bcode_harness_models::HarnessSessionSetting) {
        match setting {
            bcode_harness_models::HarnessSessionSetting::Model { model_id } => {
                self.managed_composer.settings.model_id = Some(model_id.clone());
            }
            bcode_harness_models::HarnessSessionSetting::PermissionMode { mode } => {
                self.managed_composer.settings.permission_mode = Some(*mode);
            }
        }
        if self.managed_composer.execution.session_id.is_some() {
            self.refresh_managed_indicator();
        }
    }

    /// Native settings acknowledged in this managed conversation.
    #[must_use]
    pub const fn managed_settings(&self) -> &ManagedSessionSettings {
        &self.managed_composer.settings
    }

    /// Ask the managed root to stop so the outer journey can reattach, recover or navigate.
    /// The first request wins; input is fenced until the outer journey continues.
    pub const fn request_managed_exit(&mut self, request: ManagedExitRequest) {
        if self.managed_composer.execution.session_id.is_some()
            && self.managed_composer.execution.exit_request.is_none()
        {
            self.managed_composer.execution.exit_request = Some(request);
            self.managed_composer.execution.input_ready = false;
        }
    }

    /// Pending managed exit request, if any.
    #[must_use]
    pub const fn managed_exit_request(&self) -> Option<ManagedExitRequest> {
        self.managed_composer.execution.exit_request
    }

    /// Unresolved managed native interactions projected by the shared session view.
    #[must_use]
    pub fn pending_external_interactions(
        &self,
    ) -> &[bcode_session_view_models::external::ExternalInteractionView] {
        #[cfg(test)]
        if let Some(pending) = &self.pending_external_override {
            return pending;
        }
        &self.session_view.snapshot().pending_external_interactions
    }

    #[cfg(test)]
    pub(crate) fn set_pending_external_interactions_for_test(
        &mut self,
        pending: Vec<bcode_session_view_models::external::ExternalInteractionView>,
    ) {
        self.pending_external_override = Some(pending);
    }

    /// Activity after a tool finishes or a permission resolves: an ordinary turn prepares its
    /// follow-up model request, while a managed executor continues natively, so the attached
    /// view returns to idle and native output reports any further activity.
    fn set_follow_up_activity(&mut self) {
        if self.managed_execution_session().is_some() {
            self.set_activity(ActivityState::Idle);
        } else {
            self.set_activity(ActivityState::PreparingFollowUpRequest);
        }
    }

    /// Name the native executor in chrome (header, composer prompt).
    pub fn set_managed_label(&mut self, label: String) {
        self.managed_composer.label = Some(label);
    }

    /// Display name of the attached native executor, if this is a managed conversation.
    #[must_use]
    pub fn managed_label(&self) -> Option<&str> {
        self.managed_composer
            .execution
            .session_id
            .and(self.managed_composer.label.as_deref())
    }

    /// Explicit managed entry remains managed even when transport/binding is unavailable.
    #[must_use]
    pub const fn managed_execution_session(&self) -> Option<SessionId> {
        self.managed_composer.execution.session_id
    }

    /// Fence managed input when canonical event continuity is lost.
    pub const fn suspend_managed_input(&mut self) {
        if self.managed_composer.execution.session_id.is_some() {
            self.managed_composer.execution.input_ready = false;
        }
    }

    #[must_use]
    pub const fn managed_input_ready(&self) -> bool {
        self.managed_composer.execution.input_ready
    }

    /// Request an explicit root-to-shell native authentication handoff.
    pub const fn request_managed_auth(&mut self) {
        if self.managed_composer.execution.session_id.is_some() {
            self.managed_composer.execution.auth_requested = true;
            self.managed_composer.execution.input_ready = false;
        }
    }

    /// Whether this root exited specifically for explicit native auth recovery.
    #[must_use]
    pub const fn managed_auth_requested(&self) -> bool {
        self.managed_composer.execution.auth_requested
    }

    pub fn acknowledge_managed_composer(&mut self, id: &bcode_session_models::SubmissionId) {
        self.pending_submissions.remove_id(id);
    }

    pub fn apply_submission_records(&mut self, records: &[bcode_session_models::SubmissionRecord]) {
        for record in records {
            self.pending_submissions.remove_id(&record.submission.id);
            self.session_view.apply_submission_record(record);
        }
    }

    fn apply_working_directory_changed(
        &mut self,
        _event_sequence: u64,
        old_working_directory: &std::path::Path,
    ) {
        self.invalidate_theme_catalog();
        if let Some(new_working_directory) = self.working_directory() {
            self.status = format!(
                "working directory: {}",
                display(new_working_directory, old_working_directory)
            );
        }
    }

    fn apply_tool_request_side_effects(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        arguments_json: &str,
    ) {
        let shared_tool = self.session_view.snapshot().tools.get(tool_call_id);
        let projected_tool_name = shared_tool
            .and_then(|tool| tool.tool_name.clone())
            .unwrap_or_else(|| tool_name.to_owned());
        let projected_arguments = shared_tool
            .and_then(|tool| tool.arguments_json.clone())
            .unwrap_or_else(|| arguments_json.to_owned());
        self.set_activity(ActivityState::RunningTool {
            name: projected_tool_name,
        });
        self.status =
            tool_request_status(&projected_arguments).unwrap_or_else(|| "started".to_owned());
    }

    fn push_permission_request(&mut self, input: PermissionRequestInput<'_>) {
        let shared_permission = self.shared_permission_view(input.permission_id).cloned();
        if input.application.live_activity() {
            let tool_name = shared_permission
                .as_ref()
                .map_or(input.tool_name, |permission| permission.tool_name.as_str());
            self.set_activity(ActivityState::WaitingPermission {
                name: tool_name.to_owned(),
            });
        }
        if input.application.live_activity() {
            let tool_call_id = shared_permission
                .as_ref()
                .map_or(input.tool_call_id, |permission| {
                    permission.tool_call_id.as_str()
                });
            let arguments_json = shared_permission
                .as_ref()
                .map_or(input.arguments_json, |permission| {
                    permission.arguments_json.as_str()
                });
            let tool_name = shared_permission
                .as_ref()
                .map_or(input.tool_name, |permission| permission.tool_name.as_str());
            self.status = Self::tool_call_file_status(tool_call_id).map_or_else(
                || tool_request_status(arguments_json).unwrap_or_else(|| tool_name.to_owned()),
                |status| format!("waiting permission · {status}"),
            );
        }
    }

    fn shared_permission_view(
        &self,
        permission_id: &str,
    ) -> Option<&bcode_session_view_models::PermissionView> {
        self.session_view
            .snapshot()
            .permissions
            .iter()
            .find(|permission| permission.permission_id == permission_id)
    }

    fn set_permission_status(&mut self, permission_id: &str) {
        let approved = self
            .session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::Permission { permission }
                    if permission.permission_id == permission_id && permission.resolved =>
                {
                    permission.approved
                }
                _ => None,
            })
            .unwrap_or(false);
        let status = if approved {
            "permission approved"
        } else {
            "permission denied"
        };
        status.clone_into(&mut self.status);
    }

    fn set_file_activity(&mut self, tool_name: &str) {
        self.set_activity(ActivityState::RunningTool {
            name: tool_name.to_owned(),
        });
    }

    const fn tool_call_file_status(_tool_call_id: &str) -> Option<String> {
        None
    }

    fn update_tool_result_status(
        &mut self,
        tool_call_id: &str,
        is_error: bool,
        application: SessionEventApplication,
    ) {
        if !application.live_activity() {
            return;
        }
        let is_error = self
            .session_view
            .snapshot()
            .tools
            .get(tool_call_id)
            .and_then(|tool| tool.is_error)
            .unwrap_or(is_error);
        if is_error {
            "failed".clone_into(&mut self.status);
        } else if let Some(status) = Self::tool_call_file_status(tool_call_id) {
            self.status = format!("applied · {status}");
        } else {
            "finished".clone_into(&mut self.status);
        }
    }

    fn push_tool_result(
        &mut self,
        tool_call_id: &str,
        _result: &str,
        is_error: bool,
        _semantic_result: Option<&ToolInvocationResult>,
        application: SessionEventApplication,
    ) {
        self.update_tool_result_status(tool_call_id, is_error, application);
    }

    fn push_model_usage(
        &mut self,
        _event_sequence: u64,
        _turn_id: &str,
        usage: &bcode_session_models::SessionTokenUsage,
        application: SessionEventApplication,
    ) {
        let projected_usage = self
            .session_view
            .snapshot()
            .runtime
            .latest_usage
            .clone()
            .unwrap_or_else(|| usage.clone());
        self.token_usage.absorb(&projected_usage);
        if application.live_activity()
            && let Some(tokens) = projected_usage.metered_total_tokens()
        {
            self.status = format!("tokens: {tokens}");
        }
    }

    fn apply_shared_model_turn_started(&mut self) {
        let runtime = &self.session_view.snapshot().runtime;
        debug_assert!(runtime.active_turn_id.is_some());
        debug_assert!(!runtime.cancelling);
        if runtime.active_turn_id.is_some() && !runtime.cancelling {
            self.set_activity(ActivityState::PreparingModelRequest);
        }
    }

    fn apply_shared_model_turn_cancel_requested(&mut self) {
        let runtime = &self.session_view.snapshot().runtime;
        debug_assert!(runtime.active_turn_id.is_some());
        debug_assert!(runtime.cancelling);
        if runtime.cancelling {
            self.set_cancelling();
            "cancellation requested".clone_into(&mut self.status);
        }
    }

    fn finish_shared_model_turn(&mut self, application: SessionEventApplication) {
        let runtime = &self.session_view.snapshot().runtime;
        if application.live_activity()
            && let Some(outcome) = runtime.last_turn_outcome
        {
            self.status = runtime.last_turn_message.as_deref().map_or_else(
                || model_turn_outcome_label(outcome).to_owned(),
                ToOwned::to_owned,
            );
        }
        if application.live_activity() {
            self.apply_shared_runtime_work_activity();
        }
    }

    fn apply_shared_runtime_work_activity(&mut self) {
        let snapshot = self.session_view.snapshot();
        let runtime_work = &snapshot.runtime_work;
        if runtime_work
            .iter()
            .any(|work| work.status == bcode_session_models::RuntimeWorkStatus::Cancelling)
        {
            self.set_cancelling();
        } else if let Some(detail) = runtime_work_detail(runtime_work) {
            self.set_activity(ActivityState::RuntimeWork { detail });
        } else if !snapshot.active_invocations.is_empty() {
            let count = snapshot.active_invocations.len();
            let detail = if count == 1 {
                "running tool".to_owned()
            } else {
                format!("running {count} tools")
            };
            self.set_activity(ActivityState::RuntimeWork { detail });
        } else {
            self.set_activity(ActivityState::Idle);
        }
    }

    fn apply_context_occupancy(
        &mut self,
        occupancy: Option<bcode_session_models::RequestContextOccupancy>,
    ) {
        self.session_view.set_context_occupancy(occupancy);
    }

    fn set_activity(&mut self, activity: ActivityState) {
        if self.activity != activity {
            if !self.activity.same_phase_as(&activity) {
                self.activity_started_at = Instant::now();
                self.activity_elapsed = Duration::ZERO;
            }
            self.activity = activity;
        }
    }

    fn add_streaming_delta(&mut self, text: &str, application: SessionEventApplication) {
        if !application.live_activity() {
            return;
        }
        let delta = text.chars().count();
        if let ActivityState::Streaming { chars } = &mut self.activity {
            *chars = chars.saturating_add(delta);
        } else {
            self.set_activity(ActivityState::Streaming { chars: delta });
        }
    }

    fn apply_provider_trace_event(&mut self, event_type: &str, detail: Option<&str>) {
        if event_type == bcode_session_models::REQUEST_CONTEXT_OBSERVATION_EVENT_TYPE {
            let valid = detail.is_some_and(|detail| {
                bcode_session_models::RequestContextObservationReference::parse(event_type, detail)
                    .is_ok()
            });
            let summary = if valid {
                "Request context observation: normalized metadata retained"
            } else {
                "Request context observation: invalid reference"
            };
            summary.clone_into(&mut self.status);
        } else if matches!(event_type, "warning" | "error") {
            let detail =
                detail.map_or_else(|| format!("provider event: {event_type}"), str::to_owned);
            self.set_activity(ActivityState::ProviderStream {
                detail: detail.clone(),
            });
            self.status = detail;
        }
    }

    fn apply_trace_event(&mut self, trace: &SessionTraceEvent) {
        match &trace.payload {
            SessionTracePayload::ProviderStreamEvent(event) => {
                self.apply_provider_stream_event(event);
            }
            SessionTracePayload::ProviderEvent { event_type, detail } => {
                self.apply_provider_trace_event(event_type, detail.as_deref());
            }
            SessionTracePayload::ContextCompaction { .. } => {
                self.apply_compaction_trace(trace);
            }
            SessionTracePayload::ModelRequestBuilt {
                provider,
                uses_previous_provider_response,
                message_count,
                metadata,
                ..
            } => {
                self.token_usage.apply_model_request(
                    *uses_previous_provider_response,
                    *message_count,
                    metadata
                        .get("sent_message_count")
                        .and_then(|value| value.parse().ok()),
                    metadata
                        .get("prompt_cache_points")
                        .and_then(|value| value.parse().ok()),
                );
                self.set_activity(ActivityState::StartingProviderRequest {
                    provider: provider.clone(),
                    round: None,
                });
            }
            SessionTracePayload::ProviderRound {
                provider,
                round,
                stop_reason,
                ..
            } => match trace.phase {
                SessionTracePhase::ModelProviderRoundStarted => {
                    self.set_activity(ActivityState::WaitingForProvider {
                        provider: provider.clone(),
                        round: *round,
                    });
                }
                SessionTracePhase::ModelProviderRoundFinished => {
                    self.set_activity(
                        if stop_reason.as_deref().is_some_and(|reason| {
                            reason.eq_ignore_ascii_case("tool_call")
                                || reason.eq_ignore_ascii_case("toolcall")
                                || reason.eq_ignore_ascii_case("tool_calls")
                        }) {
                            ActivityState::PreparingToolExecution {
                                name: "provider tool call".to_owned(),
                            }
                        } else {
                            ActivityState::FinalizingModelTurn
                        },
                    );
                }
                _ => {}
            },
            SessionTracePayload::ToolInvocationStarted { tool_name, .. } => {
                self.set_activity(ActivityState::RunningTool {
                    name: tool_name.clone(),
                });
            }
            SessionTracePayload::ToolPermissionWait {
                tool_call_id,
                approved,
                ..
            } => {
                let tool_name = self
                    .session_view
                    .snapshot()
                    .tools
                    .get(tool_call_id)
                    .and_then(|tool| tool.tool_name.clone())
                    .unwrap_or_else(|| "unknown tool".to_owned());
                if approved.is_none() {
                    self.set_activity(ActivityState::WaitingPermission { name: tool_name });
                } else {
                    self.set_follow_up_activity();
                }
            }
            SessionTracePayload::ToolInvocationFinished { .. } => {
                self.set_follow_up_activity();
            }
            SessionTracePayload::ToolPolicyEvaluated { .. } => {}
        }
    }

    fn apply_shared_provider_stream_progress(&mut self, event: &ProviderStreamEvent) {
        let progress = self
            .session_view
            .snapshot()
            .runtime
            .provider_progress
            .clone();
        match event {
            ProviderStreamEvent::ToolCallFinished { tool_name, .. } => {
                if matches!(self.activity, ActivityState::ProviderStream { .. }) {
                    self.set_activity(ActivityState::PreparingToolExecution {
                        name: tool_name.clone(),
                    });
                }
            }
            ProviderStreamEvent::RetryScheduled { .. } => {
                if let Some(progress) = progress {
                    self.set_activity(ActivityState::RetryWait {
                        message: progress.detail,
                        retry_at_unix: progress.retry_at_unix.unwrap_or_default(),
                    });
                }
            }
            _ => {
                if let Some(progress) = progress {
                    self.set_activity(ActivityState::ProviderStream {
                        detail: progress.detail,
                    });
                }
            }
        }
    }

    fn apply_provider_stream_event(&mut self, event: &ProviderStreamEvent) {
        match event {
            ProviderStreamEvent::TurnStarted => {
                self.set_activity(ActivityState::ProviderStream {
                    detail: "provider stream started".to_owned(),
                });
            }
            ProviderStreamEvent::ToolCallStarted { tool_name, .. } => {
                let detail = format!("provider stream tool started: {tool_name}");
                self.set_activity(ActivityState::ProviderStream { detail });
            }
            ProviderStreamEvent::ToolCallProgress {
                tool_name,
                argument_bytes,
                ..
            } => {
                let detail = format!(
                    "assembling {tool_name} arguments ({} received)",
                    format_provider_bytes(*argument_bytes)
                );
                self.set_activity(ActivityState::ProviderStream { detail });
            }
            ProviderStreamEvent::ToolCallFinished { tool_name, .. } => {
                if matches!(self.activity, ActivityState::ProviderStream { .. }) {
                    self.set_activity(ActivityState::PreparingToolExecution {
                        name: tool_name.clone(),
                    });
                }
            }
            ProviderStreamEvent::NoProgressWarning {
                idle_seconds,
                active_tool_call,
            } => {
                let detail = active_tool_call.as_ref().map_or_else(
                    || format!("provider stream idle for {idle_seconds}s"),
                    |tool| {
                        format!(
                            "provider stream idle for {idle_seconds}s while assembling {}",
                            tool.tool_name
                        )
                    },
                );
                self.set_activity(ActivityState::ProviderStream { detail });
            }
            ProviderStreamEvent::RetryScheduled {
                message,
                retry_at_unix,
            } => {
                self.set_activity(ActivityState::RetryWait {
                    message: message.clone(),
                    retry_at_unix: *retry_at_unix,
                });
            }
        }
    }

    fn apply_compaction_trace(&mut self, trace: &SessionTraceEvent) {
        use bcode_session_view::compaction_activity::{
            CompactionActivityUpdate, compaction_activity_update,
        };

        let update = compaction_activity_update(
            trace,
            &self.session_view.snapshot().runtime,
            matches!(self.activity, ActivityState::Compacting { .. }),
        );
        match update {
            Some(CompactionActivityUpdate::Active { detail }) => {
                self.set_activity(ActivityState::Compacting { detail });
            }
            Some(CompactionActivityUpdate::Failed { detail }) => {
                self.set_activity(ActivityState::Idle);
                self.status = detail;
            }
            Some(CompactionActivityUpdate::PreparingModelRequest) => {
                self.set_activity(ActivityState::PreparingModelRequest);
            }
            None => {}
        }
    }
}

pub const fn composer_policy() -> TextInputPolicy {
    bcode_tui_components::session_ui::composer_policy()
}

fn next_periodic_deadline(started_at: Instant, now: Instant, interval: Duration) -> Instant {
    let interval_nanos = interval.as_nanos().max(1);
    let period = now.saturating_duration_since(started_at).as_nanos() / interval_nanos;
    let next_offset = interval_nanos.saturating_mul(period.saturating_add(1));
    started_at + Duration::from_nanos(u64::try_from(next_offset).unwrap_or(u64::MAX))
}

fn is_theme_transition_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == THEME_TRANSITION_INVALIDATION_KEY
}

fn is_activity_animation_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == ACTIVITY_ANIMATION_INVALIDATION_KEY
}

fn interpolate_color(
    source: Color,
    target: Color,
    elapsed_ms: u64,
    duration_ms: u64,
    curve: TuiAccentTransitionCurve,
) -> Color {
    let source = color_rgb(source);
    let target = color_rgb(target);
    Color::Rgb(
        interpolate_channel(source[0], target[0], elapsed_ms, duration_ms, curve),
        interpolate_channel(source[1], target[1], elapsed_ms, duration_ms, curve),
        interpolate_channel(source[2], target[2], elapsed_ms, duration_ms, curve),
    )
}

fn interpolate_channel(
    source: u8,
    target: u8,
    elapsed_ms: u64,
    duration_ms: u64,
    curve: TuiAccentTransitionCurve,
) -> u8 {
    let (eased_numerator, denominator) = transition_progress(elapsed_ms, duration_ms, curve);
    let source = i128::from(source);
    let delta = i128::from(target).saturating_sub(source);
    let scaled_delta = delta.saturating_mul(i128::try_from(eased_numerator).unwrap_or(i128::MAX))
        / i128::try_from(denominator).unwrap_or(1);
    u8::try_from(source.saturating_add(scaled_delta).clamp(0, 255)).unwrap_or(u8::MAX)
}

fn transition_progress(
    elapsed_ms: u64,
    duration_ms: u64,
    curve: TuiAccentTransitionCurve,
) -> (u128, u128) {
    let duration = u128::from(duration_ms).max(1);
    let elapsed = u128::from(elapsed_ms).min(duration);
    match curve {
        TuiAccentTransitionCurve::Linear => (elapsed, duration),
        TuiAccentTransitionCurve::EaseIn => {
            (elapsed.saturating_pow(3), duration.saturating_pow(3).max(1))
        }
        TuiAccentTransitionCurve::EaseOut => ease_out_cubic_progress(elapsed, duration),
        TuiAccentTransitionCurve::EaseInOut => ease_in_out_cubic_progress(elapsed, duration),
    }
}

fn ease_out_cubic_progress(elapsed: u128, duration: u128) -> (u128, u128) {
    let denominator = duration.saturating_pow(3).max(1);
    let remaining = duration.saturating_sub(elapsed);
    (
        denominator.saturating_sub(remaining.saturating_pow(3)),
        denominator,
    )
}

fn ease_in_out_cubic_progress(elapsed: u128, duration: u128) -> (u128, u128) {
    let denominator = duration.saturating_pow(3).saturating_mul(2).max(1);
    let half_duration = duration / 2;
    if elapsed <= half_duration {
        (elapsed.saturating_pow(3).saturating_mul(4), denominator)
    } else {
        let remaining_twice = duration.saturating_sub(elapsed).saturating_mul(2);
        (
            denominator.saturating_sub(remaining_twice.saturating_pow(3)),
            denominator,
        )
    }
}

const fn color_rgb(color: Color) -> [u8; 3] {
    match color {
        Color::Black => [0, 0, 0],
        Color::Red => [205, 49, 49],
        Color::Green => [13, 188, 121],
        Color::Yellow => [229, 229, 16],
        Color::Blue => [36, 114, 200],
        Color::Magenta => [188, 63, 188],
        Color::Cyan => [17, 168, 205],
        Color::White => [229, 229, 229],
        Color::BrightBlack => [102, 102, 102],
        Color::BrightRed => [241, 76, 76],
        Color::BrightGreen => [35, 209, 139],
        Color::BrightYellow => [245, 245, 67],
        Color::BrightBlue => [59, 142, 234],
        Color::BrightMagenta => [214, 112, 214],
        Color::BrightCyan => [41, 184, 219],
        Color::BrightWhite => [255, 255, 255],
        Color::Rgb(red, green, blue) => [red, green, blue],
        Color::Indexed(_) | Color::Default => [100, 116, 139],
    }
}

fn tool_request_status(arguments_json: &str) -> Option<String> {
    let value = serde_json::from_str::<serde_json::Value>(arguments_json).ok()?;
    value
        .get("path")
        .or_else(|| value.get("cwd"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct TokenUsageMeter {
    latest_cached_input_tokens: Option<u32>,
    latest_cache_write_input_tokens: Option<u32>,
    provider_reuse_active: bool,
    latest_sent_message_count: Option<usize>,
    latest_total_message_count: Option<usize>,
    latest_prompt_cache_points: Option<usize>,
    context_window: Option<u32>,
    proactive_compaction_target: Option<u64>,
}

impl TokenUsageMeter {
    const fn absorb(&mut self, usage: &bcode_session_models::SessionTokenUsage) {
        self.latest_cached_input_tokens = usage.cached_input_tokens;
        self.latest_cache_write_input_tokens = usage.cache_write_input_tokens;
    }

    const fn apply_model_info(&mut self, model: Option<&bcode_model::ModelInfo>) {
        if let Some(model) = model {
            self.context_window = model.context_window;
        }
    }

    const fn clear_model_info(&mut self) {
        self.latest_cached_input_tokens = None;
        self.latest_cache_write_input_tokens = None;
        self.context_window = None;
        self.proactive_compaction_target = None;
    }

    const fn apply_model_request(
        &mut self,
        uses_previous_provider_response: bool,
        message_count: usize,
        metadata_sent_message_count: Option<usize>,
        metadata_prompt_cache_points: Option<usize>,
    ) {
        self.provider_reuse_active = uses_previous_provider_response;
        self.latest_total_message_count = Some(message_count);
        self.latest_sent_message_count = metadata_sent_message_count;
        self.latest_prompt_cache_points = metadata_prompt_cache_points;
    }

    fn footer_summary(
        &self,
        occupancy: Option<&bcode_session_models::RequestContextOccupancy>,
        cumulative_metered_tokens: u64,
        cost: &bcode_session_view_models::SessionCostSummary,
        managed: bool,
    ) -> String {
        let mut parts = vec![self.context_summary(occupancy)];
        if self.provider_reuse_active {
            parts.push("reuse on".to_string());
        }
        if let Some(cached) = self.latest_cached_input_tokens
            && cached > 0
        {
            parts.push(format!("cache read {} tok", compact_u64(u64::from(cached))));
        }
        if let Some(written) = self.latest_cache_write_input_tokens
            && written > 0
        {
            parts.push(format!(
                "cache write {} tok",
                compact_u64(u64::from(written))
            ));
        }
        if let (Some(sent), Some(total)) = (
            self.latest_sent_message_count,
            self.latest_total_message_count,
        ) && sent < total
        {
            parts.push(format!("sent {sent}/{total} msgs"));
        }
        if let Some(points) = self.latest_prompt_cache_points
            && points > 0
        {
            parts.push(format!("cache points {points}"));
        }
        parts.push(format!(
            "spent {} tok",
            compact_u64(cumulative_metered_tokens)
        ));
        if cost.original_usage_incomplete {
            parts.push("usage capture partial".into());
        }
        if cost.revision > 0 {
            parts.push(format!("cost rev {}", cost.revision));
        }
        for (currency, cost_micros) in &cost.totals_micros {
            if currency == "USD" {
                parts.push(format!(
                    "estimated {} (not billed)",
                    format_usd_micros(*cost_micros)
                ));
            } else {
                parts.push(format!(
                    "estimated {currency} {} (not billed)",
                    format_decimal_micros(*cost_micros)
                ));
            }
        }
        // A native executor that reports tokens but never prices them (a subscription) is not
        // a partial estimate; only warn when some pricing was expected.
        let unpriced_native =
            managed && cost.estimated_usage_count == 0 && cost.unavailable_usage_count == 0;
        if !unpriced_native
            && (cost.unavailable_usage_count > 0
                || cost.observed_usage_count > cost.estimated_usage_count)
        {
            parts.push(if cost.estimated_usage_count > 0 {
                format!(
                    "cost partial: {}/{} requests priced",
                    cost.estimated_usage_count, cost.observed_usage_count
                )
            } else {
                format!(
                    "cost unavailable: {} requests unpriced",
                    cost.observed_usage_count
                )
            });
        }
        parts.join(" · ")
    }

    fn context_summary(
        &self,
        occupancy: Option<&bcode_session_models::RequestContextOccupancy>,
    ) -> String {
        let display_limit = self
            .proactive_compaction_target
            .or_else(|| self.context_window.map(u64::from));
        match (occupancy, display_limit) {
            (Some(occupancy), Some(window)) if window > 0 => {
                let input = occupancy.observation.context_tokens.tokens();
                let percentage = context_window_percentage(input, window);
                format!(
                    "{}{}/{} {}",
                    if occupancy.observation.context_tokens.is_estimated() {
                        "~"
                    } else {
                        ""
                    },
                    format_context_count(input),
                    compact_context_window(window),
                    if percentage > 100 {
                        "100%+".to_string()
                    } else {
                        format!("{percentage}%")
                    }
                )
            }
            (None, Some(window)) if window > 0 => {
                format!("—/{} —%", compact_context_window(window))
            }
            _ => "—/— —%".to_owned(),
        }
    }
}

fn provider_to_display_selection(provider: &str) -> Option<String> {
    if provider == "<auto>" || provider.is_empty() {
        None
    } else {
        Some(provider.to_owned())
    }
}

fn model_to_display_selection(model: &str) -> Option<String> {
    if model == "<default>" || model.is_empty() {
        None
    } else {
        Some(model.to_owned())
    }
}

fn format_context_count(value: u64) -> String {
    let digits = value.to_string();
    let mut output = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, byte) in digits.bytes().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            output.push(',');
        }
        output.push(char::from(byte));
    }
    output
}

fn compact_context_window(value: u64) -> String {
    if value >= 1_000_000 {
        format!("{}m", value / 1_000_000)
    } else if value >= 1_000 {
        format!("{}k", value / 1_000)
    } else {
        value.to_string()
    }
}

fn compact_u64(value: u64) -> String {
    if value >= 1_000_000 {
        compact_decimal(value, 1_000_000, 'm')
    } else if value >= 1_000 {
        compact_decimal(value, 1_000, 'k')
    } else {
        value.to_string()
    }
}

fn compact_decimal(value: u64, unit: u64, suffix: char) -> String {
    let hundredths = value.saturating_mul(100) / unit;
    let whole = hundredths / 100;
    let fraction = hundredths % 100;
    if fraction == 0 {
        format!("{whole}{suffix}")
    } else if fraction.is_multiple_of(10) {
        format!("{whole}.{}{suffix}", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}{suffix}")
    }
}

fn format_decimal_micros(micros: u64) -> String {
    format!("{}.{:06}", micros / 1_000_000, micros % 1_000_000)
}

fn format_usd_micros(micros: u64) -> String {
    let dollars = micros / 1_000_000;
    let cents = (micros % 1_000_000) / 10_000;
    if dollars == 0 && cents == 0 && micros > 0 {
        "<$0.01".to_string()
    } else {
        format!("${dollars}.{cents:02}")
    }
}

fn format_provider_bytes(bytes: usize) -> String {
    const KIB: usize = 1024;
    const MIB: usize = KIB * 1024;
    if bytes >= MIB {
        let whole = bytes / MIB;
        let decimal = (bytes % MIB) * 10 / MIB;
        format!("{whole}.{decimal} MiB")
    } else if bytes >= KIB {
        let whole = bytes / KIB;
        let decimal = (bytes % KIB) * 10 / KIB;
        format!("{whole}.{decimal} KiB")
    } else {
        format!("{bytes} B")
    }
}

fn context_window_percentage(input_tokens: u64, context_window: u64) -> u32 {
    let numerator = input_tokens.saturating_mul(100);
    let denominator = context_window.max(1);
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
}

/// The model a native executor reported in the newest usage of a committed page.
fn reported_native_model(event: &SessionEvent) -> Option<String> {
    use bcode_session_models::external_harness::ExternalHarnessOperationRecord;
    let SessionEventKind::ExternalHarnessOperation { operation } = &event.kind else {
        return None;
    };
    let ExternalHarnessOperationRecord::ObservationPage { page, .. } = &operation.record else {
        return None;
    };
    page.observations
        .iter()
        .rev()
        .find_map(|observation| match &observation.kind {
            bcode_harness_models::HarnessObservationKind::Usage {
                model: Some(model), ..
            } => Some(model.clone()),
            _ => None,
        })
}

const fn event_affects_transcript_rows(event: &SessionEvent) -> bool {
    match &event.kind {
        // Shared SessionView projects these as inert canonical transcript content. Include them
        // in bounded window retention and below-viewport activity without dispatching native tools
        // or interpreting receipts/observations as Bcode turn completion.
        SessionEventKind::ExternalHarnessOperation { .. }
        | SessionEventKind::ExternalBackendBound { .. }
        | SessionEventKind::ExternalSubmissionAccepted { .. }
        | SessionEventKind::ExternalDeliveryObserved { .. }
        | SessionEventKind::ExternalObservationRecorded { .. }
        | SessionEventKind::ExternalInteractionResponded { .. }
        | SessionEventKind::SessionChildPublished { .. }
        | SessionEventKind::ChildSessionCreated { .. }
        | SessionEventKind::SubmissionAccepted { .. }
        | SessionEventKind::SubmissionActivated { .. }
        | SessionEventKind::SubmissionCancelled { .. }
        | SessionEventKind::SubmissionPendingCancelled { .. }
        | SessionEventKind::SubmissionContextObserved { .. }
        | SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::AssistantResponseSegment { .. }
        | SessionEventKind::PositionedAssistantResponseSegment { .. }
        | SessionEventKind::PositionedAssistantReasoningActivity { .. }
        | SessionEventKind::PositionedToolCallRequested { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolInvocationResultRecorded { .. }
        | SessionEventKind::PermissionRequested { .. }
        | SessionEventKind::PermissionResolved { .. }
        | SessionEventKind::ModelUsage { .. }
        | SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. }
        | SessionEventKind::RequestContextObserved { .. }
        | SessionEventKind::WorkingDirectoryChanged { .. }
        | SessionEventKind::SkillInvoked { .. }
        | SessionEventKind::SkillInvocationFailed { .. }
        | SessionEventKind::RuntimeWorkStarted { .. }
        | SessionEventKind::RuntimeWorkCancelRequested { .. }
        | SessionEventKind::RuntimeWorkProgress { .. }
        | SessionEventKind::RuntimeWorkFinished { .. }
        | SessionEventKind::ToolContribution { .. }
        | SessionEventKind::ToolContributionPlaced { .. }
        | SessionEventKind::ToolExchangeRequested { .. }
        | SessionEventKind::ToolExchangeResolved { .. }
        | SessionEventKind::DurableInteractionCommitted { .. }
        | SessionEventKind::TurnContinuationCommitted { .. }
        | SessionEventKind::ToolInvocationLifecycle { .. }
        | SessionEventKind::RalphLifecycle { .. }
        | SessionEventKind::PluginStatusNote { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::AssistantReasoningActivity { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::InertHistory { .. } => true,
        SessionEventKind::SkillSuggested { reason, .. } => reason.is_some(),
        SessionEventKind::SubmissionRecoveryAuthorized { .. }
        | SessionEventKind::SessionChildrenReconciled { .. }
        | SessionEventKind::SessionChildLinked { .. }
        | SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ReasoningChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelFeatureFidelityNegotiated { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
        | SessionEventKind::MilestoneEvaluationRequested { .. }
        | SessionEventKind::MilestonePublicationRequested { .. }
        | SessionEventKind::MilestonePublished { .. }
        | SessionEventKind::MilestoneEvaluationCompleted { .. }
        | SessionEventKind::AutomaticTitleUpdated { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SessionImported { .. }
        | SessionEventKind::SessionDerived { .. }
        | SessionEventKind::ExecutionSessionCreated { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn plugin_policy_delivery_is_secret_safe_and_reload_invalidates_once() {
        use bcode_plugin_sdk::tui::{
            PluginTuiPresentationConfiguration, PluginTuiRegistry, PluginTuiVisualAdapter,
            PluginTuiVisualRenderContext,
        };
        use std::sync::{Arc, Mutex};
        struct Capture(Arc<Mutex<Vec<serde_json::Value>>>);
        impl PluginTuiVisualAdapter for Capture {
            fn supports(&self, _kind: &str) -> bool {
                true
            }
            fn rows(
                &self,
                _kind: &str,
                _payload: &serde_json::Value,
                _context: &PluginTuiVisualRenderContext,
            ) -> Vec<bmux_tui::prelude::Line> {
                Vec::new()
            }
            fn configure(
                &self,
                configuration: &PluginTuiPresentationConfiguration,
            ) -> Result<(), String> {
                self.0.lock().unwrap().push(configuration.config.clone());
                Ok(())
            }
        }
        let host = bcode_plugin::PluginHost::load_static_plugins(&[]).unwrap();
        let presentation = Arc::new(crate::plugin_tui::PluginTuiPresentation::new(host));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let mut registry = PluginTuiRegistry::default();
        registry.register_visual_adapter(["test"], Box::new(Capture(Arc::clone(&calls))));
        presentation.install_registry_for_test("test", registry);
        let mut app = super::BmuxApp::new_with_history(None, &[], &[]);
        app.set_plugin_presentation(Arc::clone(&presentation));
        drop(presentation);
        let mut config = bcode_config::BcodeConfig::default();
        config
            .tools
            .shell
            .output
            .prelude_gates
            .push(bcode_config::ShellToolPreludeGateConfig {
                marker: "private-marker".to_owned(),
                ..bcode_config::ShellToolPreludeGateConfig::default()
            });
        assert!(app.apply_plugin_presentation_config(&config).unwrap());
        assert!(!app.apply_plugin_presentation_config(&config).unwrap());
        let payload = calls.lock().unwrap()[0].clone();
        assert!(!payload.to_string().contains("private-marker"));
        assert_eq!(payload.as_object().unwrap().len(), 1);
        assert!(payload["shell"].get("env").is_none());
        config.tools.shell.output.presentation.python = false;
        assert!(app.apply_plugin_presentation_config(&config).unwrap());
        drop(app);
        let updated = calls.lock().unwrap()[1].clone();
        let output: bcode_config::ShellToolOutputConfig =
            serde_json::from_value(updated["shell"]["output"].clone()).unwrap();
        assert!(!output.presentation.python);
        assert_eq!(
            output.format_commands,
            config.tools.shell.output.format_commands
        );
    }

    use super::*;

    #[test]
    fn queue_confirmation_rejects_stale_prepare_and_clears_replaced_outside_window_head() {
        use bcode_session_models::*;
        let confirmation = SubmissionResumeConfirmation {
            version: 1,
            submission_id: SubmissionId("pending".to_owned()),
            expected_accepted_sequence: 1,
            descriptor: SubmissionExecutionDescriptor {
                version: 2,
                provider: "provider-界".to_owned(),
                model: "model-👩‍💻-e\u{301}".to_owned(),
                model_profile: Some("selected profile".to_owned()),
                auth_profile: Some("account".to_owned()),
                reasoning: None,
                config_identity: "digest".to_owned(),
                recovery_recipe: None,
            },
        };
        let id = confirmation.submission_id.clone();
        let record = SubmissionRecord {
            receipt: SubmissionReceipt {
                submission_id: id.clone(),
                accepted_event_sequence: 1,
            },
            submission: PromptSubmission {
                id,
                client_id: ClientId::new(),
                text: "not in history".to_owned(),
                admission: TurnAdmissionMetadata::default(),
                requested_placement: SubmissionPlacement::FollowUp,
                target: SubmissionTarget::OwnTurn {
                    turn_id: TurnId("target".to_owned()),
                },
                execution_descriptor: None,
            },
            status: SubmissionStatus::Pending,
            context_observation: None,
            recovery_authorization: None,
        };
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.prepare_queue_confirmation(confirmation.clone());
        assert!(app.queue_recovery.confirmation.is_none());
        app.set_submission_queue(SubmissionQueueStatus {
            oldest: Some(record.clone()),
            blocker: Some(SubmissionQueueBlocker::MissingContext),
        });
        assert_eq!(
            app.session_view_snapshot().transcript.items,
            [] as [bcode_session_view_models::TranscriptViewItem; 0]
        );
        app.prepare_queue_confirmation(confirmation.clone());
        assert_eq!(app.queue_recovery.confirmation, Some(confirmation.clone()));
        assert!(!app.queue_recovery.settings_visible);
        app.queue_recovery.settings_visible = true;
        assert_queue_completion_fences(&mut app, &record, &confirmation);
        let mut changed = record;
        changed.receipt.accepted_event_sequence = 2;
        app.set_submission_queue(SubmissionQueueStatus {
            oldest: Some(changed),
            blocker: Some(SubmissionQueueBlocker::MissingContext),
        });
        assert!(app.queue_recovery.confirmation.is_none());
        assert!(!app.queue_recovery.settings_visible);
        app.prepare_queue_confirmation(confirmation);
        assert!(app.queue_recovery.confirmation.is_none());
        app.set_submission_queue(SubmissionQueueStatus {
            oldest: None,
            blocker: None,
        });
        let recovery_absent = app
            .session_view_snapshot()
            .runtime
            .queue_recovery()
            .is_none();
        drop(app);
        assert!(recovery_absent);
    }

    fn assert_queue_completion_fences(
        app: &mut BmuxApp,
        record: &bcode_session_models::SubmissionRecord,
        confirmation: &bcode_session_models::SubmissionResumeConfirmation,
    ) {
        use bcode_session_models::*;
        let before_refresh = app.queue_recovery.generation;
        app.set_submission_queue(SubmissionQueueStatus {
            oldest: Some(record.clone()),
            blocker: Some(SubmissionQueueBlocker::MissingContext),
        });
        assert!(!app.apply_queue_recovery_result(
            before_refresh,
            bcode_session_view_models::SessionViewActionOutcome::None,
            SubmissionQueueStatus {
                oldest: None,
                blocker: None
            }
        ));
        assert!(
            app.session_view_snapshot()
                .runtime
                .queue_recovery()
                .is_some()
        );
        let generation = app.queue_recovery.generation;
        app.dismiss_queue_confirmation();
        assert!(!app.apply_queue_recovery_result(
            generation,
            bcode_session_view_models::SessionViewActionOutcome::SubmissionResumePrepared {
                confirmation: confirmation.clone()
            },
            SubmissionQueueStatus {
                oldest: Some(record.clone()),
                blocker: Some(SubmissionQueueBlocker::MissingContext)
            }
        ));
        assert!(app.queue_recovery.confirmation.is_none());
        let generation = app.queue_recovery.generation;
        app.set_submission_queue(SubmissionQueueStatus {
            oldest: Some(record.clone()),
            blocker: Some(SubmissionQueueBlocker::WorkflowAuthorityRequired),
        });
        assert!(!app.apply_queue_recovery_result(
            generation,
            bcode_session_view_models::SessionViewActionOutcome::SubmissionResumePrepared {
                confirmation: confirmation.clone()
            },
            SubmissionQueueStatus {
                oldest: Some(record.clone()),
                blocker: Some(SubmissionQueueBlocker::MissingContext)
            }
        ));
        assert!(
            !app.session_view_snapshot()
                .runtime
                .queue_recovery()
                .unwrap()
                .can_resume
        );
        app.set_submission_queue_unavailable();
        assert!(
            app.session_view_snapshot()
                .runtime
                .submission_queue_unavailable
        );
        assert!(
            app.session_view_snapshot()
                .runtime
                .queue_recovery()
                .is_none()
        );
    }

    fn invalidation_deadline(app: &BmuxApp, key: &str, now: Instant) -> Option<Instant> {
        app.invalidation_requests(now)
            .into_iter()
            .find(|request| request.key.as_str() == key)
            .map(|request| request.at)
    }

    #[test]
    fn runtime_refresh_preserves_unsent_agent_selection() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.set_current_agent_id("plan");
        app.set_pending_agent("build", None);
        app.apply_runtime_selection(
            bcode_ipc::SessionRuntimeSelection {
                agent_id: Some("plan".to_owned()),
                ..Default::default()
            },
            Some("#6b7280".to_owned()),
        );
        assert_eq!(app.current_agent_accent(), Some("#6b7280"));
        assert_eq!(app.current_agent_id(), "plan");
        assert_eq!(app.display_agent_id(), "build");
        app.set_pending_agent("review", None);
        app.commit_submitted_agent("build");
        assert_eq!(app.current_agent_id(), "build");
        assert_eq!(app.display_agent_id(), "review");
        app.commit_submitted_agent("review");
        assert_eq!(app.current_agent_id(), "review");
        assert_eq!(app.pending_agent_id(), None);
        drop(app);
    }

    #[test]
    fn theme_transition_deadline_is_stable_until_the_frame_is_handled() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let immediate = TuiConfig {
            theme: TuiThemeConfig {
                accent_transition: bcode_config::TuiAccentTransitionMode::Immediate,
                ..TuiThemeConfig::default()
            },
            ..TuiConfig::default()
        };
        app.apply_tui_config(immediate);
        app.set_current_agent("plan", Some("#6b7280".to_owned()));
        app.apply_tui_config(TuiConfig {
            theme: TuiThemeConfig {
                accent_transition: bcode_config::TuiAccentTransitionMode::Transition,
                accent_transition_ms: Some(100),
                ..TuiThemeConfig::default()
            },
            ..TuiConfig::default()
        });
        app.set_current_agent("build", Some("#22d3ee".to_owned()));

        let queried_at = Instant::now();
        let first_deadline =
            invalidation_deadline(&app, THEME_TRANSITION_INVALIDATION_KEY, queried_at)
                .expect("transition should request a frame");
        assert_eq!(
            invalidation_deadline(
                &app,
                THEME_TRANSITION_INVALIDATION_KEY,
                queried_at + Duration::from_millis(10),
            ),
            Some(first_deadline),
            "unrelated scheduling passes must not postpone the pending frame"
        );

        app.handle_invalidations(
            &[InvalidationKey::new(THEME_TRANSITION_INVALIDATION_KEY)],
            first_deadline,
        );
        let next_deadline =
            invalidation_deadline(&app, THEME_TRANSITION_INVALIDATION_KEY, first_deadline)
                .expect("active transition should request its next frame");
        assert!(next_deadline > first_deadline);

        app.handle_invalidations(
            &[InvalidationKey::new(THEME_TRANSITION_INVALIDATION_KEY)],
            queried_at + Duration::from_millis(200),
        );
        assert_eq!(app.display_agent_id(), "build");
        assert_eq!(app.presented_theme().accent, Color::Rgb(34, 211, 238));
        let final_deadline = invalidation_deadline(
            &app,
            THEME_TRANSITION_INVALIDATION_KEY,
            queried_at + Duration::from_millis(200),
        );
        drop(app);
        assert_eq!(final_deadline, None);
    }

    #[test]
    fn external_schema_v1_theme_lifecycle_remains_compatible() {
        let temp = tempfile::tempdir().expect("theme root");
        let path = temp.path().join("external.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nid = \"external\"\nextends = [\"terminal-native\"]\n[palette]\naccent = \"#112233\"\n[styles.\"surface.raised\"]\nbg = \"#223344\"\n",
        )
        .expect("external theme");
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut config = app.tui_config().clone();
        config.theme.paths = vec![temp.path().to_path_buf()];
        config.theme.accent_transition = bcode_config::TuiAccentTransitionMode::Immediate;
        app.apply_tui_config(config);
        let configured = app.presented_theme();

        let catalog = crate::theme::catalog_view(&mut app);
        let external = catalog
            .entries
            .iter()
            .find(|entry| entry.id == "external")
            .expect("external theme discovered");
        assert_eq!(external.source, "explicit");
        assert_eq!(external.validation, "valid");

        assert!(app.preview_theme("external"));
        assert_ne!(app.presented_theme().fingerprint, configured.fingerprint);
        app.cancel_theme_preview();
        assert_eq!(app.presented_theme().fingerprint, configured.fingerprint);
        assert!(app.apply_theme("external"));
        let valid = app.presented_theme();
        assert_eq!(app.tui_config().theme.name, "external");

        std::fs::write(&path, "schema_version = 1\nid = [broken").expect("invalid revision");
        assert!(app.reload_theme_if_valid().is_none());
        assert_eq!(app.presented_theme(), valid);
        let diagnostics = crate::theme::catalog_view(&mut app).diagnostics;
        assert_eq!(diagnostics.len(), 1);
        assert!(diagnostics[0].contains("external.toml"));

        std::fs::write(
            &path,
            "schema_version = 1\nid = \"external\"\nextends = [\"terminal-native\"]\n[palette]\naccent = \"#445566\"\n[styles.\"surface.raised\"]\nbg = \"#334455\"\n",
        )
        .expect("corrected revision");
        assert_eq!(app.reload_theme_if_valid(), Some("external"));
        let fingerprint = app.presented_theme().fingerprint;
        drop(app);
        assert_ne!(fingerprint, valid.fingerprint);
    }

    #[test]
    fn invalidated_preview_can_restore_configured_theme() {
        let temp = tempfile::tempdir().expect("theme root");
        let path = temp.path().join("preview.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nid = \"preview\"\n[palette]\naccent = \"#112233\"\n",
        )
        .expect("preview theme");
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut config = app.tui_config().clone();
        config.theme.paths = vec![temp.path().to_path_buf()];
        app.apply_tui_config(config);
        let configured = app.presented_theme();

        assert!(app.preview_theme("preview"));
        assert_ne!(app.presented_theme(), configured);
        std::fs::remove_file(path).expect("preview disappears");
        assert!(app.reload_theme_if_valid().is_none());

        app.cancel_theme_preview();
        let fingerprint = app.presented_theme().fingerprint;
        drop(app);
        assert_eq!(fingerprint, configured.fingerprint);
    }

    #[test]
    fn theme_reload_retains_last_valid_then_applies_next_valid_revision() {
        let temp = tempfile::tempdir().expect("theme root");
        let path = temp.path().join("live.toml");
        std::fs::write(
            &path,
            "schema_version = 1\nid = \"live\"\n[palette]\naccent = \"#112233\"\n[styles.\"text.primary\"]\nfg = \"#112233\"\n",
        )
        .expect("initial theme");
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut config = app.tui_config().clone();
        config.theme.name = "live".to_owned();
        config.theme.paths = vec![temp.path().to_path_buf()];
        app.apply_tui_config(config);
        let valid = app.presented_theme();

        std::fs::write(&path, "not = [valid").expect("invalid transient edit");
        assert!(app.reload_theme_if_valid().is_none());
        assert_eq!(app.presented_theme(), valid);

        std::fs::write(
            &path,
            "schema_version = 1\nid = \"live\"\n[palette]\naccent = \"#445566\"\n[styles.\"text.primary\"]\nfg = \"#445566\"\n",
        )
        .expect("next valid theme");
        assert_eq!(app.reload_theme_if_valid(), Some("live"));
        let fingerprint = app.presented_theme().fingerprint;
        drop(app);
        assert_ne!(fingerprint, valid.fingerprint);
    }

    #[test]
    fn catalog_view_preserves_rejected_candidate_diagnostics() {
        let temp = tempfile::tempdir().expect("theme root");
        std::fs::write(temp.path().join("broken.toml"), "not = [valid").expect("broken theme");
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut config = app.tui_config().clone();
        config.theme.paths = vec![temp.path().to_path_buf()];
        app.apply_tui_config(config);

        let catalog = crate::theme::catalog_view(&mut app);
        drop(app);

        assert_ne!(catalog.entries, [] as [crate::theme::ThemeCatalogEntry; 0]);
        assert_eq!(catalog.diagnostics.len(), 1);
        assert!(catalog.diagnostics[0].contains("broken.toml"));
    }

    #[test]
    fn theme_catalog_entries_include_external_origin_and_current_selection() {
        let temp = tempfile::tempdir().expect("theme root");
        std::fs::write(
            temp.path().join("custom.toml"),
            "schema_version = 1\nid = \"custom\"\ndisplay_name = \"Custom\"\n[palette]\naccent = \"#112233\"\n",
        )
        .expect("external theme");
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut config = app.tui_config().clone();
        config.theme.name = "custom".to_owned();
        config.theme.paths = vec![temp.path().to_path_buf()];
        app.apply_tui_config(config);

        let entries = crate::theme::catalog_view(&mut app).entries;
        drop(app);
        let custom = entries
            .iter()
            .find(|entry| entry.id == "custom")
            .expect("custom catalog entry");

        assert_eq!(custom.source, "explicit");
        assert!(custom.selected);
        assert_eq!(custom.display_name, "Custom");
    }

    #[test]
    fn repeated_theme_previews_reuse_one_discovered_catalog() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);

        assert!(app.preview_theme("bcode-dark"));
        assert!(app.preview_theme("nord"));
        assert!(app.preview_theme("terminal-native"));

        let discovery_count = app.theme_catalog_discovery_count();
        drop(app);
        assert_eq!(discovery_count, 1);
    }

    #[test]
    fn theme_preview_apply_and_cancel_are_in_memory_transitions() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        // Immediate accents keep the comparison independent of how much wall
        // time passes between steps.
        app.apply_tui_config(TuiConfig {
            theme: TuiThemeConfig {
                accent_transition: bcode_config::TuiAccentTransitionMode::Immediate,
                ..TuiThemeConfig::default()
            },
            ..TuiConfig::default()
        });
        let configured = app.tui_config().theme.name.clone();
        let initial = app.presented_theme();

        assert!(app.preview_theme("bcode-dark"));
        assert_eq!(app.tui_config().theme.name, configured);
        assert_ne!(app.presented_theme(), initial);

        app.cancel_theme_preview();
        assert_eq!(app.presented_theme(), initial);
        assert_eq!(app.tui_config().theme.name, configured);

        assert!(app.apply_theme("bcode-light"));
        assert_eq!(app.tui_config().theme.name, "bcode-light");
        let unknown_theme_accepted = app.preview_theme("does-not-exist");
        drop(app);
        assert!(!unknown_theme_accepted);
    }

    fn snapshot(
        estimated: bool,
        input_tokens: u64,
    ) -> bcode_session_models::RequestContextObservation {
        bcode_session_models::RequestContextObservation {
            request: bcode_session_models::ModelRequestIdentity {
                provider_plugin_id: "provider".to_string(),
                requested_model_id: None,
                effective_model_id: "effective-model".to_string(),
                request_id: "request".to_string(),
                model_turn_id: "turn".to_string(),
                round: 0,
                request_fingerprint: "fingerprint".to_string(),
                effective_auth_profile: Some("routed-profile".to_string()),
                context_format_version: None,
                compatibility_key: None,
                context_epoch: 3,
            },
            context_through_sequence: 1,
            context_tokens: if estimated {
                bcode_session_models::RequestContextTokenCount::Estimated(input_tokens)
            } else {
                bcode_session_models::RequestContextTokenCount::ProviderExact(input_tokens)
            },
            local_estimate: bcode_session_models::LocalContextEstimate {
                tokens: input_tokens,
                algorithm_version: 1,
            },
        }
    }

    const MOTION_CASES: [(bcode_tui_theme::chrome::Motion, bool); 4] = [
        (bcode_tui_theme::chrome::Motion::None, false),
        (bcode_tui_theme::chrome::Motion::Subtle, false),
        (bcode_tui_theme::chrome::Motion::Full, false),
        (bcode_tui_theme::chrome::Motion::Full, true),
    ];

    fn accent_transition_under(
        motion: bcode_tui_theme::chrome::Motion,
        reduce_motion: bool,
        explicit_ms: Option<u64>,
    ) -> (Duration, Option<Instant>) {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.apply_tui_config(TuiConfig {
            theme: TuiThemeConfig {
                accent_transition_ms: explicit_ms,
                ..TuiThemeConfig::default()
            },
            reduce_motion,
            ..TuiConfig::default()
        });
        app.set_motion_for_test(motion, reduce_motion);
        let now = Instant::now();
        let target = Color::Rgb(1, 2, 3);
        let accent = app.animated_accent(target, now);
        let duration = app.theme_transition.duration;
        let deadline = invalidation_deadline(&app, THEME_TRANSITION_INVALIDATION_KEY, now);
        drop(app);
        assert_eq!(
            accent == target,
            duration.is_zero(),
            "an instant transition presents its target at once"
        );
        (duration, deadline)
    }

    #[test]
    fn accent_transitions_follow_motion_and_schedule_no_frames_when_instant() {
        use bcode_tui_theme::chrome::Motion;
        let mut durations = Vec::new();
        for (motion, reduce_motion) in MOTION_CASES {
            let (duration, deadline) = accent_transition_under(motion, reduce_motion, None);
            let instant = motion == Motion::None || reduce_motion;
            assert_eq!(duration.is_zero(), instant, "{motion:?}/{reduce_motion}");
            assert_eq!(deadline.is_none(), instant, "{motion:?}/{reduce_motion}");
            durations.push(duration);
        }
        assert!(durations[2] > durations[1], "full outlasts subtle");
    }

    #[test]
    fn explicit_accent_duration_beats_theme_motion_but_not_reduce_motion() {
        use bcode_tui_theme::chrome::Motion;
        for (motion, reduce_motion) in MOTION_CASES {
            let (duration, deadline) = accent_transition_under(motion, reduce_motion, Some(500));
            if reduce_motion {
                assert_eq!(duration, Duration::ZERO, "{motion:?}");
                assert_eq!(deadline, None, "{motion:?}");
            } else {
                assert_eq!(duration, Duration::from_millis(500), "{motion:?}");
                assert!(deadline.is_some(), "{motion:?}");
            }
        }
        let (duration, deadline) = accent_transition_under(Motion::Full, false, Some(0));
        assert_eq!((duration, deadline), (Duration::ZERO, None));
    }

    #[test]
    fn reduce_motion_settles_a_running_accent_transition() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let now = Instant::now();
        let target = Color::Rgb(1, 2, 3);
        assert_ne!(app.animated_accent(target, now), target);
        let mut config = app.tui_config().clone();
        config.reduce_motion = true;
        app.apply_tui_config(config);
        let later = now + Duration::from_millis(1);
        app.animated_accent(target, later);
        let deadline = invalidation_deadline(&app, THEME_TRANSITION_INVALIDATION_KEY, later);
        let active = app.theme_transition_active(later);
        drop(app);
        assert_eq!(deadline, None);
        assert!(!active);
    }

    #[test]
    fn activity_presentation_changes_only_on_accepted_ticks() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.set_activity(ActivityState::PreparingModelRequest);
        let now = app.activity_started_at + Duration::from_millis(250);
        assert_eq!(app.activity_elapsed(), Duration::ZERO);
        let frame = app.activity_frame();
        app.handle_invalidations_with_damage(
            &[InvalidationKey::new(ACTIVITY_ANIMATION_INVALIDATION_KEY)],
            now,
        );
        assert_eq!(app.activity_elapsed(), Duration::from_millis(250));
        assert_eq!(app.activity_frame(), frame + 1);
        app.set_activity(ActivityState::Idle);
        app.set_activity(ActivityState::PreparingModelRequest);
        assert_eq!(app.activity_elapsed(), Duration::ZERO);
        drop(app);
    }

    #[test]
    fn shared_session_view_tracks_tui_history_and_live_events() {
        let session_id = bcode_session_models::SessionId::new();
        let history = vec![
            bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 1,
                timestamp_ms: 1,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "hello".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            },
            bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence: 2,
                timestamp_ms: 2,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::AssistantMessage {
                    text: "hi".to_owned(),
                },
            },
        ];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);

        assert_eq!(app.session_view_snapshot().transcript.items.len(), 2);
        assert!(matches!(
            &app.session_view_snapshot().transcript.items[0].kind,
            bcode_session_view_models::TranscriptViewItemKind::UserMessage { message }
                if message.text == "hello"
        ));
        assert!(matches!(
            &app.session_view_snapshot().transcript.items[1].kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "hi"
        ));

        app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-2".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "live".to_owned(),
            },
        });

        let has_live_message = matches!(
            &app.session_view_snapshot().transcript.items[2].kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "live"
        );
        drop(app);
        assert!(has_live_message);
    }

    #[test]
    fn authoritative_active_skill_snapshot_drives_shared_tui_state() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.set_active_skills(&[bcode_skill_models::SkillContextResponse {
            skill_id: bcode_skill_models::SkillId::new("review"),
            context: String::new(),
            source: bcode_skill_models::SkillSource {
                kind: bcode_skill_models::SkillSourceKind::Plugin,
                label: "test".to_owned(),
                path: None,
                precedence: 0,
            },
            bytes_loaded: 0,
            truncated: false,
            model_policy: None,
        }]);

        assert_eq!(app.active_skill_count(), 1);
        let has_review = app.session_view_snapshot().active_skills.contains("review");
        drop(app);
        assert!(has_review);
    }

    #[test]
    fn skill_activation_status_consumes_shared_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let skill_id = bcode_skill_models::SkillId::new("event-skill");
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };

        app.absorb_session_event(&event(
            1,
            bcode_session_models::SessionEventKind::SkillActivated {
                skill_id: skill_id.clone(),
                source: None,
                mode: bcode_skill_models::SkillActivationMode::Explicit,
                activated_at_ms: 1,
            },
        ));
        assert!(
            app.session_view_snapshot()
                .active_skills
                .contains("event-skill")
        );
        assert_eq!(app.status(), "activated skill: event-skill");

        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms: 2,
            },
        ));
        assert!(
            !app.session_view_snapshot()
                .active_skills
                .contains("event-skill")
        );
        let status = app.status().to_owned();
        drop(app);
        assert_eq!(status, "deactivated skill: event-skill");
    }

    #[test]
    fn generic_lifecycle_drives_tui_activity_until_terminal_event() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let event = |sequence, stage| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence,
                    stage,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        };

        app.absorb_session_event(&event(
            1,
            bcode_session_models::ToolInvocationLifecycleStage::Started,
        ));
        assert!(matches!(
            app.activity(),
            ActivityState::RuntimeWork { detail } if detail == "running tool"
        ));

        app.absorb_session_event(&event(
            2,
            bcode_session_models::ToolInvocationLifecycleStage::Completed,
        ));
        let is_idle = matches!(app.activity(), ActivityState::Idle);
        drop(app);
        assert!(is_idle);
    }

    #[test]
    fn authoritative_runtime_work_snapshot_drives_tui_activity() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.apply_runtime_work_snapshots(&[bcode_session_models::RuntimeWorkSnapshot {
            work_id: bcode_session_models::WorkId::new("tool-1"),
            kind: bcode_session_models::RuntimeWorkKind::Tool,
            label: "shell".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            status: bcode_session_models::RuntimeWorkStatus::Running,
            cancellable: true,
            observation: None,
        }]);

        assert!(matches!(
            app.activity(),
            ActivityState::RuntimeWork { detail } if detail == "running tool: shell"
        ));
        let work = &app.session_view_snapshot().runtime_work[0];
        assert_eq!(work.label, "shell");
        assert!(work.cancellable);

        app.apply_runtime_work_snapshots(&[]);
        assert!(matches!(app.activity(), ActivityState::Idle));
        let work_is_empty = app.session_view_snapshot().runtime_work.is_empty();
        drop(app);
        assert!(work_is_empty);
    }

    #[test]
    fn history_with_reasoning_change_builds_without_recursive_rebuild() {
        let session_id = bcode_session_models::SessionId::new();
        let history = vec![bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ReasoningChanged {
                effort: Some("high".to_owned()),
                summary: Some("detailed".to_owned()),
                model_scope: None,
            },
        }];

        let app = BmuxApp::new_with_history(Some(session_id), &history, &[]);

        assert_eq!(app.reasoning_effort(), Some("high"));
        let summary = app.reasoning_summary().map(str::to_owned);
        drop(app);
        assert_eq!(summary.as_deref(), Some("detailed"));
    }

    #[test]
    fn reasoning_and_skill_projections_are_event_driven() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let skill_id = bcode_skill_models::SkillId::new("event-skill");
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };

        app.absorb_session_event(&event(
            1,
            bcode_session_models::SessionEventKind::ReasoningChanged {
                effort: Some("high".to_owned()),
                summary: Some("detailed".to_owned()),
                model_scope: None,
            },
        ));
        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::SkillActivated {
                skill_id: skill_id.clone(),
                source: None,
                mode: bcode_skill_models::SkillActivationMode::Explicit,
                activated_at_ms: 2,
            },
        ));

        assert_eq!(app.reasoning_effort(), Some("high"));
        assert_eq!(app.reasoning_summary(), Some("detailed"));
        assert_eq!(app.active_skill_count(), 1);

        app.absorb_session_event(&event(
            3,
            bcode_session_models::SessionEventKind::SkillDeactivated {
                skill_id,
                deactivated_at_ms: 3,
            },
        ));
        let active_skill_count = app.active_skill_count();
        drop(app);
        assert_eq!(active_skill_count, 0);
    }

    #[test]
    fn model_change_preserves_accumulated_shared_cost() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };
        app.absorb_session_event(&event(
            1,
            bcode_session_models::SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: bcode_session_models::SessionTokenUsage {
                    request_id: Some("request-1".to_owned()),
                    observation_id: Some("request-1:usage".to_owned()),
                    terminal: true,
                    cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                        currency: "USD".to_owned(),
                        total_micros: 25_000,
                        components: Vec::new(),
                        source: "fixture".to_owned(),
                        revision: Some("v1".to_owned()),
                    }),
                    ..bcode_session_models::SessionTokenUsage::default()
                },
            },
        ));
        let before = app.session_view_snapshot().runtime.cost.clone();

        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::ModelChanged {
                provider: "provider-2".to_owned(),
                model: "model-2".to_owned(),
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
            },
        ));

        let cost = app.session_view_snapshot().runtime.cost.clone();
        drop(app);
        assert_eq!(cost, before);
        assert!(
            TokenUsageMeter::default()
                .footer_summary(None, 0, &cost, false)
                .ends_with("spent 0 tok · estimated $0.02 (not billed)")
        );
    }

    #[test]
    fn model_reasoning_and_agent_events_consume_shared_runtime_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };

        app.absorb_session_event(&event(
            1,
            bcode_session_models::SessionEventKind::ModelChanged {
                provider: "<auto>".to_owned(),
                model: "<default>".to_owned(),
                selection_source: bcode_session_models::ModelSelectionSource::UserExplicit,
            },
        ));
        assert_eq!(app.selected_provider_plugin_id(), None);
        assert_eq!(app.selected_model_id(), None);
        assert_eq!(app.status(), "model: <auto>/<default>");

        app.set_pending_agent("review", Some("#ff00ff".to_owned()));
        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::AgentChanged {
                agent_id: "build".to_owned(),
            },
        ));
        assert_eq!(app.current_agent_id(), "build");
        assert_eq!(app.display_agent_accent(), None);

        app.absorb_session_event(&event(
            3,
            bcode_session_models::SessionEventKind::ReasoningChanged {
                effort: Some("high".to_owned()),
                summary: Some("detailed".to_owned()),
                model_scope: None,
            },
        ));
        assert_eq!(app.reasoning_effort(), Some("high"));
        assert_eq!(app.reasoning_summary(), Some("detailed"));
        assert_eq!(
            app.session_view_snapshot()
                .runtime
                .reasoning_effort
                .as_deref(),
            Some("high")
        );
        let summary = app
            .session_view_snapshot()
            .runtime
            .reasoning_summary
            .clone();
        drop(app);
        assert_eq!(summary.as_deref(), Some("detailed"));
    }

    #[test]
    fn context_summary_uses_effective_proactive_compaction_target() {
        let meter = TokenUsageMeter {
            context_window: Some(1_050_000),
            proactive_compaction_target: Some(272_000),
            ..TokenUsageMeter::default()
        };
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(true, 136_000),
        };

        assert_eq!(meter.context_summary(Some(&occupancy)), "~136,000/272k 50%");
    }

    #[test]
    fn context_summary_marks_estimated_overflow_without_exceeding_one_hundred_percent() {
        let meter = TokenUsageMeter {
            context_window: Some(372_000),
            ..TokenUsageMeter::default()
        };
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(true, 407_295),
        };

        assert_eq!(
            meter.context_summary(Some(&occupancy)),
            "~407,295/372k 100%+"
        );
    }

    #[test]
    fn hydrated_estimated_occupancy_preserves_source_and_approximate_rendering() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(true, 2_500),
        };
        app.apply_context_occupancy(Some(occupancy));
        let hydrated = app
            .session_view_snapshot()
            .runtime
            .context_occupancy
            .clone()
            .expect("hydrated occupancy");
        drop(app);
        let meter = TokenUsageMeter {
            context_window: Some(10_000),
            ..TokenUsageMeter::default()
        };

        assert!(hydrated.observation.context_tokens.is_estimated());
        assert_eq!(meter.context_summary(Some(&hydrated)), "~2,500/10k 25%");
    }

    #[test]
    fn live_exact_occupancy_preserves_exact_source_and_rendering() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(false, 2_500),
        };
        app.absorb_session_live_event(&SessionLiveEvent {
            session_id: SessionId::new(),
            kind: SessionLiveEventKind::RequestContextOccupancyChanged {
                occupancy: Box::new(Some(occupancy)),
            },
        });
        let live = app
            .session_view_snapshot()
            .runtime
            .context_occupancy
            .clone()
            .expect("live occupancy");
        drop(app);
        let meter = TokenUsageMeter {
            context_window: Some(10_000),
            ..TokenUsageMeter::default()
        };

        assert!(!live.observation.context_tokens.is_estimated());
        assert_eq!(meter.context_summary(Some(&live)), "2,500/10k 25%");
    }

    #[test]
    fn subscription_and_api_estimates_are_never_presented_as_billed_charges() {
        let usages = ["chatgpt_subscription", "api_key"].map(|auth_mode| {
            bcode_session_models::SessionTokenUsage {
                pricing_target: Some(Box::new(bcode_session_models::SessionPricingTarget {
                    provider: "openai".into(),
                    auth_mode: auth_mode.into(),
                    api_surface: if auth_mode == "chatgpt_subscription" {
                        "chatgpt_codex"
                    } else {
                        "responses"
                    }
                    .into(),
                    integration: Some("bcode".into()),
                })),
                cost: Some(bcode_session_models::SessionCostEstimate::Estimated {
                    currency: "USD".into(),
                    total_micros: 1_115_980_451,
                    components: Vec::new(),
                    source: "remote_catalog".into(),
                    revision: None,
                }),
                ..bcode_session_models::SessionTokenUsage::default()
            }
        });
        let meter = TokenUsageMeter::default();
        for usage in &usages {
            let cost =
                bcode_session_view_models::SessionCostSummary::rebuild(std::iter::once(usage));
            assert!(cost.has_complete_coverage());
            assert!(
                meter
                    .footer_summary(None, 0, &cost, false)
                    .contains("estimated $1115.98 (not billed)")
            );
        }
        let mixed = bcode_session_view_models::SessionCostSummary::rebuild(usages.iter());
        assert!(
            meter
                .footer_summary(None, 0, &mixed, false)
                .contains("estimated $2231.96 (not billed)")
        );
    }

    #[test]
    fn footer_renders_cost_currency_coverage_and_zero_semantics() {
        let meter = TokenUsageMeter::default();
        let complete = bcode_session_view_models::SessionCostSummary {
            original_usage_incomplete: false,
            revision: 0,
            totals_micros: BTreeMap::from([("USD".to_string(), 0), ("EUR".to_string(), 2_000_000)]),
            estimated_usage_count: 2,
            unavailable_usage_count: 0,
            observed_usage_count: 2,
        };
        let partial = bcode_session_view_models::SessionCostSummary {
            unavailable_usage_count: 1,
            observed_usage_count: 3,
            ..complete.clone()
        };
        let unavailable = bcode_session_view_models::SessionCostSummary {
            unavailable_usage_count: 1,
            observed_usage_count: 1,
            ..bcode_session_view_models::SessionCostSummary::default()
        };

        let complete_text = meter.footer_summary(None, 0, &complete, false);
        assert!(complete_text.contains("estimated $0.00 (not billed)"));
        assert!(complete_text.contains("estimated EUR 2.000000 (not billed)"));
        assert!(!complete_text.contains('~'));
        assert!(
            meter
                .footer_summary(None, 0, &partial, false)
                .contains("cost partial: 2/3 requests priced")
        );
        assert!(
            meter
                .footer_summary(None, 0, &unavailable, false)
                .contains("cost unavailable: 1 requests unpriced")
        );
        // A managed executor that never prices its usage is not reported as missing cost,
        // while an explicitly unavailable estimate still is.
        let unpriced = bcode_session_view_models::SessionCostSummary {
            observed_usage_count: 2,
            ..bcode_session_view_models::SessionCostSummary::default()
        };
        assert!(
            !meter
                .footer_summary(None, 0, &unpriced, true)
                .contains("cost")
        );
        assert!(
            meter
                .footer_summary(None, 0, &unavailable, true)
                .contains("cost unavailable")
        );
    }

    #[test]
    fn authoritative_live_occupancy_drives_and_clears_footer_state() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(true, 2_500),
        };
        app.absorb_session_live_event(&SessionLiveEvent {
            session_id: SessionId::new(),
            kind: SessionLiveEventKind::RequestContextOccupancyChanged {
                occupancy: Box::new(Some(occupancy)),
            },
        });

        assert_eq!(
            app.session_view_snapshot()
                .runtime
                .context_occupancy
                .as_ref()
                .map(|value| value.observation.context_tokens.tokens()),
            Some(2_500)
        );

        app.absorb_session_live_event(&SessionLiveEvent {
            session_id: SessionId::new(),
            kind: SessionLiveEventKind::RequestContextOccupancyChanged {
                occupancy: Box::new(None),
            },
        });
        let occupancy_cleared = app
            .session_view_snapshot()
            .runtime
            .context_occupancy
            .is_none();
        drop(app);
        assert!(occupancy_cleared);
    }

    #[test]
    fn subset_effort_range_skips_unsupported_values_and_wraps() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.reasoning_effort_values = vec!["low".to_owned(), "high".to_owned()];
        app.apply_reasoning_selection(Some("low".to_owned()), None);

        assert_eq!(app.cycle_pending_reasoning_effort(), Some("high"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("low"));
        let effort = app.cycle_pending_reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn pending_reasoning_cycle_advances_every_rapid_call_and_wraps() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.reasoning_effort_values = vec!["none".to_owned(), "low".to_owned(), "high".to_owned()];
        app.apply_reasoning_selection(Some("none".to_owned()), None);

        assert_eq!(app.cycle_pending_reasoning_effort(), Some("low"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("high"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("none"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("low"));
        let generation = app.reasoning_effort_generation;
        drop(app);
        assert_eq!(generation, 4);
    }

    #[test]
    fn session_change_explicitly_clears_pending_reasoning() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.set_pending_reasoning_effort("high".to_owned());

        app.clear_pending_reasoning_effort_for_session_change();

        assert_eq!(app.pending_reasoning_effort_generation(), None);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort, None);
    }

    #[test]
    fn canonical_status_clears_only_matching_pending_reasoning() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        app.set_pending_reasoning_effort("high".to_owned());
        app.reconcile_pending_reasoning_effort("low");
        assert_eq!(app.reasoning_effort(), Some("high"));

        app.reconcile_pending_reasoning_effort("high");
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort, None);
    }

    #[test]
    fn pending_reasoning_completion_does_not_clear_newer_selection() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let older = app.set_pending_reasoning_effort("low".to_owned());
        let newer = app.set_pending_reasoning_effort("high".to_owned());

        app.clear_pending_reasoning_effort(older);
        assert_eq!(app.reasoning_effort(), Some("high"));
        assert_eq!(app.pending_reasoning_effort_generation(), Some(newer));

        app.clear_pending_reasoning_effort(newer);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort, None);
    }

    #[test]
    fn model_turn_start_and_cancel_consume_shared_runtime_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn-shared".to_owned(),
            },
        ));

        let runtime = &app.session_view_snapshot().runtime;
        assert_eq!(runtime.active_turn_id.as_deref(), Some("turn-shared"));
        assert!(!runtime.cancelling);
        assert!(matches!(
            app.activity(),
            ActivityState::PreparingModelRequest
        ));

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            2,
            SessionEventKind::ModelTurnCancelRequested {
                turn_id: "turn-shared".to_owned(),
                requested_at_ms: Some(2),
                client_id: Some(bcode_session_models::ClientId::new()),
            },
        ));

        let runtime = &app.session_view_snapshot().runtime;
        assert_eq!(runtime.active_turn_id.as_deref(), Some("turn-shared"));
        assert!(runtime.cancelling);
        assert!(matches!(app.activity(), ActivityState::Cancelling));
        let status = app.status().to_owned();
        drop(app);
        assert_eq!(status, "cancellation requested");
    }

    #[test]
    fn workflow_remains_visible_after_model_turn_finishes() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::RuntimeWorkStarted {
                work_id: bcode_session_models::WorkId::new("workflow-live"),
                kind: bcode_session_models::RuntimeWorkKind::Workflow,
                label: "Checking completion".into(),
                tool_call_id: None,
                plugin_id: None,
                service_interface: None,
                operation: None,
                parent_work_id: None,
                started_at_ms: Some(1),
                cancellable: true,
            },
        ));
        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            2,
            SessionEventKind::ModelTurnFinished {
                turn_id: "evaluation".into(),
                outcome: ModelTurnOutcome::Completed,
                message: None,
            },
        ));
        let active = matches!(app.activity(), ActivityState::RuntimeWork { .. });
        drop(app);
        assert!(active);
    }

    #[test]
    fn session_rename_status_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::SessionRenamed {
                name: Some("renamed session".to_owned()),
            },
        ));

        assert_eq!(app.session_title(), Some("renamed session"));
        let status = app.status().to_owned();
        drop(app);
        assert_eq!(status, "session: renamed session");
    }

    #[test]
    fn active_tool_loop_follows_shared_tool_status() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
        assert!(app.active_tool_loop());
        assert!(matches!(
            app.session_view_snapshot()
                .tools
                .get("tool-shared")
                .expect("shared requested tool")
                .status,
            bcode_session_view_models::ToolInvocationViewStatus::Requested
        ));

        app.absorb_session_event(&event(
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "tool-shared".to_owned(),
                    model_output: "shared result".to_owned(),
                    is_error: false,
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "shared result".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        ));

        assert!(!app.active_tool_loop());
        let finished = matches!(
            app.session_view_snapshot()
                .tools
                .get("tool-shared")
                .expect("shared finished tool")
                .status,
            bcode_session_view_models::ToolInvocationViewStatus::Finished
        );
        drop(app);
        assert!(finished);
    }

    fn shared_projection_adapter_event(
        session_id: SessionId,
        sequence: u64,
        kind: SessionEventKind,
    ) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence.saturating_mul(10),
            session_id,
            provenance: None,
            kind,
        }
    }

    fn shared_tool_call_request_kind() -> SessionEventKind {
        SessionEventKind::ToolCallRequested {
            tool_call_id: "tool-shared".to_owned(),
            producer_plugin_id: Some("bcode.shell".to_owned()),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"printf shared"}"#.to_owned(),
            working_directory: None,
        }
    }
}

/// Local monitor presentation and keyboard ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildPanelPresentation {
    Hidden,
    Composer,
    Monitor,
}

#[cfg(test)]
mod notice_tests {
    use bcode_session_models::{
        CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionId,
    };

    use super::BmuxApp;

    fn notice_texts(app: &BmuxApp) -> Vec<String> {
        app.ephemeral_notices()
            .map(|notice| notice.text().to_owned())
            .collect()
    }

    fn assistant(session_id: SessionId, sequence: u64, text: &str) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        }
    }

    #[test]
    fn notices_follow_the_newest_canonical_item_when_raised() {
        let session_id = SessionId::new();
        let history = [
            assistant(session_id, 1, "first"),
            assistant(session_id, 2, "second"),
        ];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        app.push_ephemeral_system_plain("local issue".to_owned());
        let newest = app
            .session_view_snapshot()
            .transcript
            .items
            .last()
            .map(|item| item.id.clone());
        let notice = app.ephemeral_notices().next().cloned().expect("notice");
        drop(app);
        assert_eq!(notice.after(), newest.as_ref());
        assert_eq!(notice.after_sequence(), Some(2));
        assert_eq!(notice.role(), "System");
    }

    #[test]
    fn same_session_reconstruction_preserves_notices() {
        let session_id = SessionId::new();
        let history = [assistant(session_id, 1, "before")];
        let mut source = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        source.push_ephemeral_system_plain("local issue".to_owned());
        let mut reconstructed = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        reconstructed.take_same_session_transcript_state_from(&source);
        drop(source);
        let texts = notice_texts(&reconstructed);
        drop(reconstructed);
        assert_eq!(texts, ["local issue"]);
    }

    #[test]
    fn notices_do_not_transfer_across_sessions_or_from_a_draft() {
        let mut source = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[]);
        source.push_ephemeral_system_plain("local issue".to_owned());
        let mut other = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[]);
        other.take_same_session_transcript_state_from(&source);
        drop(source);
        let other_texts = notice_texts(&other);
        drop(other);

        let mut draft = BmuxApp::new_with_history(None, &[], &[]);
        draft.push_ephemeral_system_plain("draft issue".to_owned());
        let mut opened = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[]);
        opened.take_same_session_transcript_state_from(&draft);
        drop(draft);
        let opened_texts = notice_texts(&opened);
        drop(opened);
        assert_eq!(other_texts, Vec::<String>::new());
        assert_eq!(opened_texts, Vec::<String>::new());
    }

    #[test]
    fn reopening_the_attached_session_preserves_notices_through_the_intermediate_app() {
        let session_id = SessionId::new();
        let history = [assistant(session_id, 1, "canonical")];
        let mut attached = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        attached.push_ephemeral_system_plain("local issue".to_owned());
        // `start_switch_session_at_sequence` installs an empty intermediate app for the same
        // session before the attach result arrives.
        let mut intermediate = BmuxApp::new_with_history(Some(session_id), &[], &[]);
        intermediate.take_same_session_transcript_state_from(&attached);
        drop(attached);
        let intermediate_texts = notice_texts(&intermediate);
        let mut completed = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        completed.take_same_session_transcript_state_from(&intermediate);
        drop(intermediate);
        let completed_texts = notice_texts(&completed);
        drop(completed);
        assert_eq!(intermediate_texts, ["local issue"]);
        assert_eq!(completed_texts, ["local issue"]);
    }

    #[test]
    fn notices_stay_out_of_the_canonical_projection() {
        let session_id = SessionId::new();
        let history = [assistant(session_id, 1, "canonical")];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[]);
        app.push_ephemeral_system_plain("local issue".to_owned());
        let texts = notice_texts(&app);
        let canonical = app.session_view_snapshot().transcript.items.clone();
        drop(app);
        assert_eq!(texts, ["local issue"]);
        assert_eq!(canonical.len(), 1);
        assert!(!canonical.iter().any(|item| matches!(
            &item.kind,
            bcode_session_view_models::TranscriptViewItemKind::SystemMessage { message }
                if message.text.contains("local issue")
        )));
    }

    #[test]
    fn live_plugin_activity_updates_in_place_and_is_removed_when_absent() {
        let mut app = BmuxApp::new_with_history(None, &[], &[]);
        let mut status = bcode_session_view_models::PluginStatusView {
            plugin_id: "example".into(),
            note_id: "execution".into(),
            text: "Implementation running".into(),
            priority: 20,
            metadata: std::collections::BTreeMap::new(),
        };
        app.set_plugin_status(vec![status.clone()]);
        let first = app
            .ephemeral_notices()
            .next()
            .map(super::super::ephemeral_notices::EphemeralNotice::id);
        status.text = "Evaluation waiting".into();
        app.set_plugin_status(vec![status]);
        let updated = app.ephemeral_notices().next().cloned().expect("activity");
        app.set_plugin_status(Vec::new());
        let remaining = notice_texts(&app);
        drop(app);
        assert_eq!(Some(updated.id()), first);
        assert_eq!(updated.text(), "Evaluation waiting");
        assert_eq!(updated.role(), "Activity");
        assert_eq!(remaining, Vec::<String>::new());
    }
}
