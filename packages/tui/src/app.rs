//! TUI app state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

#[cfg(test)]
use bcode_config::TuiThemeConfig;
use bcode_config::{
    TuiAccentTransitionCurve, TuiConfig, TuiDiffViewerConfig, TuiDiffViewerLayout,
    TuiThinkingConfig,
};
use bcode_plugin_sdk::path::display;
#[cfg(test)]
use bcode_session_models::{ModelTurnOutcome, RuntimeWorkStatus};
use bcode_session_models::{
    ProviderStreamEvent, SessionEvent, SessionEventKind, SessionHistoryCursor, SessionId,
    SessionInputHistoryEntry, SessionLiveEvent, SessionLiveEventKind, SessionTraceEvent,
    SessionTracePayload, SessionTracePhase, ToolInvocationResult,
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
use super::older_history::OlderHistoryState;
use super::pending_submission::PendingSubmission;
use super::pending_submissions::PendingSubmissions;
use super::theme::definition::ResolvedThemeDefinition;
use super::theme::discovery::DiscoveredThemes;
use super::theme::{PresentedTheme, ResolvedTheme};
use super::timeline_dialog::TimelineEntry;
use super::transcript::{TranscriptItem, terminal_item_from_shared};
#[cfg(test)]
use super::transcript_document::TranscriptPresentationOrigin;
use super::transcript_document::{
    SessionViewTerminalAdapter, TranscriptDocument, TranscriptDocumentDamage, TranscriptItems,
    TranscriptPresentationEntryId,
};
use super::transcript_layout::{TranscriptLayoutCache, VisibleTranscriptSource};
use super::transcript_markdown_cache::TranscriptMarkdownCache;
use super::transcript_resident_window::{TranscriptResidentWindow, TranscriptWindowPolicy};
use super::transcript_viewport::TranscriptViewport;

const MANUAL_TRANSCRIPT_SCROLL_GRACE: Duration = Duration::from_millis(450);
const TRANSCRIPT_SCROLL_ANIMATION_FRAME: Duration = Duration::from_millis(16);
const TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY: &str = "transcript-scroll-animation";
const LATEST_BAR_ANIMATION_INVALIDATION_KEY: &str = "latest-bar-animation";
const THEME_TRANSITION_INVALIDATION_KEY: &str = "theme-transition";
const ACTIVITY_ANIMATION_INVALIDATION_KEY: &str = "activity-animation";
const THEME_TRANSITION_FRAME: Duration = Duration::from_millis(16);
const TOOL_ELAPSED_INVALIDATION_PREFIX: &str = "tool-elapsed";
const TOOL_ELAPSED_INVALIDATION_MAX_INTERVAL: Duration = Duration::from_secs(1);
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
    /// Animated new-content indicator.
    LatestBar,
    /// Complete transcript viewport.
    Transcript,
    /// Unknown or global presentation region.
    Full,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptAnimationTarget {
    LatestBottom,
    ItemTop(TranscriptPresentationEntryId),
    /// Retain the requested position if no item correspondence is available.
    Row,
}

/// Scroll animation uses real time unless a frame harness explicitly freezes it.
#[derive(Debug, Clone, Default)]
enum TranscriptScrollClock {
    #[default]
    Realtime,
    #[cfg(test)]
    Fixed(Instant),
}

impl TranscriptScrollClock {
    fn now(&self) -> Instant {
        match self {
            Self::Realtime => Instant::now(),
            #[cfg(test)]
            Self::Fixed(now) => *now,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptScrollAnimation {
    start_top_row: usize,
    target_top_row: usize,
    target: TranscriptAnimationTarget,
    started_at: Instant,
    duration: Duration,
    next_frame_at: Instant,
}

impl TranscriptScrollAnimation {
    fn new(
        start_top_row: usize,
        target_top_row: usize,
        started_at: Instant,
        duration: Duration,
    ) -> Self {
        Self {
            start_top_row,
            target_top_row,
            target: TranscriptAnimationTarget::Row,
            started_at,
            duration,
            next_frame_at: started_at + TRANSCRIPT_SCROLL_ANIMATION_FRAME,
        }
    }

    fn advance_frame(&mut self, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started_at);
        let frame_count = elapsed.as_nanos() / TRANSCRIPT_SCROLL_ANIMATION_FRAME.as_nanos().max(1);
        let next_offset = TRANSCRIPT_SCROLL_ANIMATION_FRAME
            .as_nanos()
            .saturating_mul(frame_count.saturating_add(1));
        self.next_frame_at =
            self.started_at + Duration::from_nanos(u64::try_from(next_offset).unwrap_or(u64::MAX));
    }

    fn top_row_at(self, now: Instant) -> usize {
        let duration_ms = self.duration.as_millis().max(1);
        let elapsed_ms = now
            .saturating_duration_since(self.started_at)
            .as_millis()
            .min(duration_ms);
        let remaining_ms = duration_ms.saturating_sub(elapsed_ms);
        let denominator = duration_ms.saturating_pow(3);
        let numerator = denominator.saturating_sub(remaining_ms.saturating_pow(3));
        let start = i128::try_from(self.start_top_row).unwrap_or(i128::MAX);
        let target = i128::try_from(self.target_top_row).unwrap_or(i128::MAX);
        let delta = target.saturating_sub(start);
        let eased_delta = delta.saturating_mul(i128::try_from(numerator).unwrap_or(i128::MAX))
            / i128::try_from(denominator).unwrap_or(1);
        usize::try_from(start.saturating_add(eased_delta).max(0)).unwrap_or(usize::MAX)
    }

    fn finished(self, now: Instant) -> bool {
        now.saturating_duration_since(self.started_at) >= self.duration
    }
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

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TranscriptFrameObservation {
    pub semantic_items: Vec<(
        bcode_session_view_models::TranscriptViewItemId,
        bcode_session_view_models::ViewRevision,
    )>,
    pub terminal_items: Vec<(
        super::transcript::TranscriptItemId,
        Option<bcode_session_view_models::TranscriptViewItemId>,
        Option<bcode_session_view_models::ViewRevision>,
    )>,
    pub terminal_rows: Vec<(
        Option<bcode_session_view_models::TranscriptViewItemId>,
        usize,
    )>,
    pub damage: TranscriptDocumentDamage,
    pub viewport_top: usize,
    pub scroll_mode: &'static str,
    pub anchor: Option<StableTranscriptAnchor>,
}

#[derive(Debug, Clone)]
pub struct StableTranscriptAnchor {
    pub item_id: TranscriptPresentationEntryId,
    pub row_in_item: usize,
    pub content: Option<(String, usize)>,
    pub fallback_index: usize,
    pub source_position: Option<usize>,
    /// Rows between the viewport top and the anchored content (e.g. a history banner).
    pub viewport_row_offset: usize,
}

impl PartialEq for StableTranscriptAnchor {
    fn eq(&self, other: &Self) -> bool {
        self.item_id == other.item_id
            && self.row_in_item == other.row_in_item
            && self.content == other.content
            && self.source_position == other.source_position
            && self.viewport_row_offset == other.viewport_row_offset
    }
}
impl Eq for StableTranscriptAnchor {}

impl StableTranscriptAnchor {
    fn resolve_row(&self, layout: &TranscriptLayoutCache, index: usize, rows: usize) -> usize {
        if let Some(row) = self
            .source_position
            .and_then(|position| layout.source_row(index, position))
        {
            return row.min(rows.saturating_sub(1));
        }
        self.content
            .as_ref()
            .and_then(|(key, offset)| layout.resolve_content_anchor(index, key, *offset))
            .unwrap_or(self.row_in_item)
            .min(rows.saturating_sub(1))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingReasoningEffort {
    value: String,
    generation: u64,
}

/// Navigation state consumed while preparing one candidate presentation.
/// Canonical session state and accepted item layouts are deliberately excluded.
#[derive(Debug, Clone)]
struct TranscriptNavigationCheckpoint {
    viewport: TranscriptViewport,
    animation: Option<TranscriptScrollAnimation>,
    anchor: Option<StableTranscriptAnchor>,
    submitted: SubmittedUserMessageFollowing,
    assistant: AssistantScrollAnchorState,
    pending_assistant: bool,
    pending_sequence: Option<u64>,
    automatic_item: Option<super::transcript::TranscriptItemId>,
    admitted_items: BTreeSet<super::transcript::TranscriptItemId>,
    history: OlderHistoryState,
    hidden_activity_at: Option<Instant>,
    hidden_activity_burst: u8,
    bar_started_at: Instant,
    bar_next_frame_at: Option<Instant>,
}

#[derive(Debug, Clone)]
struct SavedSessionReadingPosition {
    sequence: u64,
    row_in_item: usize,
    source_position: Option<usize>,
    content: Option<(String, usize)>,
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
}

/// State owned by the terminal user interface.
#[derive(Debug, Clone)]
pub struct BmuxApp {
    pending_session_link: Option<SessionId>,
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
    session_reading_positions: BTreeMap<SessionId, SavedSessionReadingPosition>,
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
    /// Structural presentation policy and per-item disclosure overrides.
    structure: super::structure::StructureState,
    /// Theme-contributed structural rule layers from the last theme resolution.
    theme_structure_layers: Vec<bcode_presentation_policy::models::RuleLayer>,
    /// Plugin manifest default rule layers, each scoped to its plugin's units.
    plugin_structure_layers: Vec<bcode_presentation_policy::models::RuleLayer>,
    /// Effective manifest `[presentation]` declarations the plugin layers and
    /// target fields were last derived from; an unchanged catalog skips
    /// re-deriving and recompiling.
    plugin_presentation_declarations: BTreeMap<String, bcode_plugin::PluginPresentationDeclaration>,
    /// Renderer-neutral configured structural rules.
    configured_structure_rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    /// Interactive structural rules loaded from user state.
    user_structure_rules: Vec<bcode_presentation_policy::models::PresentationRule>,
    /// Keyboard focus over transcript disclosure targets.
    transcript_focus: super::transcript_focus::TranscriptFocus,
    /// Structure inspector, detail cycling, and pending per-tool defaults.
    pub(crate) transcript_tools: super::inspector::TranscriptTools,
    /// Transcript residency (length and boundary ids) last reconciled with
    /// disclosure overrides and focus.
    reconciled_residency: Option<(usize, Option<u64>, Option<u64>)>,
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
    session_ui: bcode_tui_components::session_ui::SessionUi,
    pub(crate) composer_submit: super::composer_submit::ComposerSubmit,
    pub(crate) managed_composer: ManagedComposerState,
    input_history: InputHistory,
    transcript: TranscriptDocument,
    transcript_projection_revision: u64,
    active_interaction_layout: Option<(String, u16)>,
    session_view_terminal_adapter: SessionViewTerminalAdapter,
    /// Local presentation of an exact prepared recovery selection.
    pub queue_recovery: QueueRecoveryPresentation,
    session_view: bcode_session_view::SessionView,
    transcript_window: TranscriptResidentWindow,
    transcript_selection_pinned: bool,
    latest_history_sequence: Option<u64>,
    pending_submissions: PendingSubmissions,
    transcript_layout: TranscriptLayoutCache,
    transcript_markdown_cache: crate::transcript_markdown_cache::TranscriptMarkdownCache,
    elapsed_layout_revision: u64,
    elapsed_dirty_visuals: BTreeSet<String>,
    transcript_dirty_items: BTreeSet<TranscriptPresentationEntryId>,
    manual_transcript_scroll_until: Option<Instant>,
    transcript_scroll_animation: Option<TranscriptScrollAnimation>,
    transcript_scroll_clock: TranscriptScrollClock,
    pending_visual_overflow_bottom: Option<usize>,
    pending_stable_transcript_anchor: Option<StableTranscriptAnchor>,
    presented_transcript_anchor: Option<StableTranscriptAnchor>,
    navigation_checkpoint: Option<TranscriptNavigationCheckpoint>,
    automatic_item: Option<super::transcript::TranscriptItemId>,
    admitted_items: BTreeSet<super::transcript::TranscriptItemId>,
    latest_hidden_activity_at: Option<Instant>,
    latest_hidden_activity_burst: u8,
    latest_bar_animation_started_at: Instant,
    activity_frame: usize,
    activity_elapsed: Duration,
    latest_bar_next_frame_at: Option<Instant>,
    submitted_user_message_following: SubmittedUserMessageFollowing,
    assistant_scroll_anchor: AssistantScrollAnchorState,
    pending_assistant_stream_anchor: bool,
    pending_transcript_top_anchor_sequence: Option<u64>,
    older_history: OlderHistoryState,
    history_window_generation: u64,
    activity: ActivityState,
    activity_started_at: Instant,
    daemon_connection: DaemonConnectionState,
    execution_mode_indicator: Option<String>,
    #[cfg(test)]
    pending_external_override:
        Option<Vec<bcode_session_view_models::external::ExternalInteractionView>>,
    status: String,
    key_hints: String,
    jump_to_latest_key_label: String,
    tui_config: TuiConfig,
    effective_keymap: bcode_plugin_sdk::tui::keymap::KeymapRegistry,
    diff_viewer_layout_override: Option<TuiDiffViewerLayout>,
    exit: ExitState,
    cursor: CursorBlink,
    pending_key_activation: Option<PendingKeyActivation>,
    plugin_presentation: Option<Arc<crate::plugin_tui::PluginTuiPresentation>>,
    markdown_interaction: crate::markdown_interaction::MarkdownInteractionState,
    markdown_source_view: Option<(String, String)>,
    markdown_details_open: BTreeMap<String, bool>,
    markdown_presentation_revision: u64,
    markdown_semantics_reconciled: Option<(u64, u64, u16)>,
    footnote_return_targets: BTreeMap<String, String>,
    markdown_footnote_rows: BTreeMap<String, usize>,
    pending_markdown_focus: Option<String>,
    markdown_fragment_rows: BTreeMap<String, usize>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum SubmittedUserMessageFollowing {
    #[default]
    Idle,
    PendingAnchor,
    Anchored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum AssistantScrollAnchorState {
    #[default]
    Idle,
    Anchored {
        index: usize,
    },
    Interrupted {
        index: usize,
    },
}

impl AssistantScrollAnchorState {
    const fn index(self) -> Option<usize> {
        match self {
            Self::Idle => None,
            Self::Anchored { index } | Self::Interrupted { index } => Some(index),
        }
    }
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
    last_transcript_damage: TranscriptDocumentDamage,
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
        has_older_history: bool,
    ) -> Self {
        Self::initial_session_state(
            session_id,
            input_history,
            OlderHistoryState::new(history, has_older_history),
        )
        .with_initial_history(history)
    }

    fn initial_session_state(
        session_id: Option<SessionId>,
        input_history: &[SessionInputHistoryEntry],
        older_history: OlderHistoryState,
    ) -> Self {
        let (now, initial_theme) = Self::initial_presentation();
        Self::initial_session_state_with_presentation(
            session_id,
            input_history,
            older_history,
            now,
            &initial_theme,
        )
    }

    #[expect(
        clippy::too_many_lines,
        reason = "exhaustive application-state initialization keeps all initial field values together"
    )]
    fn initial_session_state_with_presentation(
        session_id: Option<SessionId>,
        input_history: &[SessionInputHistoryEntry],
        older_history: OlderHistoryState,
        now: Instant,
        initial_theme: &ResolvedTheme,
    ) -> Self {
        Self {
            pending_session_link: None,
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
            session_reading_positions: BTreeMap::new(),
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
            structure: super::structure::StructureState::default(),
            theme_structure_layers: Vec::new(),
            plugin_structure_layers: Vec::new(),
            plugin_presentation_declarations: BTreeMap::new(),
            configured_structure_rules: Vec::new(),
            user_structure_rules: Vec::new(),
            transcript_focus: super::transcript_focus::TranscriptFocus::default(),
            transcript_tools: super::inspector::TranscriptTools::default(),
            reconciled_residency: None,
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
            session_ui: bcode_tui_components::session_ui::SessionUi::new(),
            composer_submit: super::composer_submit::ComposerSubmit::default(),
            managed_composer: ManagedComposerState::default(),
            input_history: InputHistory::from_entries(input_history),
            transcript: TranscriptDocument::default(),
            transcript_projection_revision: 0,
            active_interaction_layout: None,
            session_view_terminal_adapter: SessionViewTerminalAdapter::default(),
            queue_recovery: QueueRecoveryPresentation::default(),
            session_view: bcode_session_view::SessionView::new(),
            transcript_window: TranscriptResidentWindow::default(),
            transcript_selection_pinned: false,
            latest_history_sequence: None,
            pending_submissions: PendingSubmissions::default(),
            transcript_layout: TranscriptLayoutCache::default(),
            transcript_markdown_cache: TranscriptMarkdownCache::default(),
            elapsed_layout_revision: 0,
            elapsed_dirty_visuals: BTreeSet::new(),
            transcript_dirty_items: BTreeSet::new(),
            manual_transcript_scroll_until: None,
            transcript_scroll_animation: None,
            transcript_scroll_clock: TranscriptScrollClock::default(),
            pending_visual_overflow_bottom: None,
            pending_stable_transcript_anchor: None,
            presented_transcript_anchor: None,
            navigation_checkpoint: None,
            automatic_item: None,
            admitted_items: BTreeSet::new(),
            latest_hidden_activity_at: None,
            latest_hidden_activity_burst: 0,
            latest_bar_animation_started_at: now,
            activity_frame: 0,
            activity_elapsed: Duration::ZERO,
            latest_bar_next_frame_at: None,
            submitted_user_message_following: SubmittedUserMessageFollowing::Idle,
            assistant_scroll_anchor: AssistantScrollAnchorState::Idle,
            pending_assistant_stream_anchor: false,
            pending_transcript_top_anchor_sequence: None,
            older_history,
            history_window_generation: 0,
            activity: ActivityState::Idle,
            activity_started_at: now,
            daemon_connection: DaemonConnectionState::Connecting,
            execution_mode_indicator: None,
            #[cfg(test)]
            pending_external_override: None,
            status: String::from("Connecting to daemon…"),
            key_hints: String::new(),
            jump_to_latest_key_label: "ctrl+end".to_owned(),
            tui_config: TuiConfig::default(),
            effective_keymap: bcode_plugin_sdk::tui::keymap::KeymapRegistry::default(),
            diff_viewer_layout_override: None,
            exit: ExitState::default(),
            cursor: CursorBlink::new(),
            pending_key_activation: None,
            plugin_presentation: None,
            markdown_interaction: crate::markdown_interaction::MarkdownInteractionState::default(),
            markdown_source_view: None,
            markdown_details_open: BTreeMap::new(),
            markdown_presentation_revision: 0,
            markdown_semantics_reconciled: None,
            footnote_return_targets: BTreeMap::new(),
            markdown_footnote_rows: BTreeMap::new(),
            pending_markdown_focus: None,
            markdown_fragment_rows: BTreeMap::new(),
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
        if self.session_id == source.session_id {
            self.managed_composer
                .placement
                .clone_from(&source.managed_composer.placement);
        }
        self.session_reading_positions
            .clone_from(&source.session_reading_positions);
        if source.scroll_offset() > 0
            && let Some(session_id) = source.session_id
            && let Some(line) = source
                .transcript_layout
                .visible_lines_from_top(
                    source.transcript_top_row(source.session_ui.viewport.height()),
                    1,
                )
                .first()
            && line.source == VisibleTranscriptSource::Transcript
            && let Some(sequence) = source
                .transcript
                .items()
                .get(line.entry_index)
                .and_then(super::transcript::TranscriptItem::event_sequence)
        {
            self.session_reading_positions.insert(
                session_id,
                SavedSessionReadingPosition {
                    sequence,
                    row_in_item: line.row_in_entry,
                    source_position: source
                        .transcript_layout
                        .source_position(line.entry_index, line.row_in_entry),
                    content: source
                        .transcript_layout
                        .content_anchor(line.entry_index, line.row_in_entry)
                        .map(|(key, offset)| (key.to_owned(), offset)),
                },
            );
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

    /// Retained source position for a bounded return attach.
    #[must_use]
    pub fn saved_reading_sequence(&self, session_id: SessionId) -> Option<u64> {
        self.session_reading_positions
            .get(&session_id)
            .map(|position| position.sequence)
    }

    /// Restore exact accepted source coordinates after a bounded centered return attach.
    pub(crate) fn restore_saved_reading_position(&mut self) {
        let Some(position) = self
            .session_id
            .and_then(|id| self.session_reading_positions.get(&id))
            .cloned()
        else {
            return;
        };
        let Some(index) = self
            .transcript
            .items()
            .iter()
            .position(|item| item.event_sequence() == Some(position.sequence))
        else {
            return;
        };
        let Some(item_id) = self.transcript.presentation_id(index) else {
            return;
        };
        // A newly attached app starts in follow-bottom mode, which deliberately ignores
        // correspondence. Returning to an explicit reading position is anchored navigation;
        // the accepted BMUX source mapping resolves its exact row during preparation.
        self.session_ui.viewport.follow_anchor(0);
        self.pending_stable_transcript_anchor = Some(StableTranscriptAnchor {
            item_id,
            fallback_index: index,
            row_in_item: position.row_in_item,
            viewport_row_offset: 0,
            source_position: position.source_position,
            content: position.content,
        });
    }

    /// Preserve process-local transcript presentation across reconstruction of the same session.
    pub(crate) fn take_same_session_transcript_state_from(&mut self, source: &Self) {
        if self.session_id != source.session_id || self.session_id.is_none() {
            return;
        }
        self.transcript.copy_ephemeral_from(&source.transcript);
        self.transcript_projection_revision = self.transcript_projection_revision.saturating_add(1);
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
        registry: bcode_plugin_sdk::tui::keymap::KeymapRegistry,
    ) {
        if self.effective_keymap != registry {
            self.capture_stable_transcript_anchor();
            self.effective_keymap = registry;
            self.transcript_layout = TranscriptLayoutCache::default();
            self.key_hints = format!(
                "{} send · {} interrupt · {} exit · {} palette",
                self.shortcut_hint("chat", crate::keymap::BmuxAction::InputSubmitSteering.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::AppInterrupt.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::AppExit.id()),
                self.shortcut_hint("chat", crate::keymap::BmuxAction::CommandPaletteOpen.id())
            );
            self.elapsed_layout_revision = self.elapsed_layout_revision.saturating_add(1);
        }
    }

    /// Effective bindings shared by visual rendering and terminal dispatch.
    #[must_use]
    pub const fn effective_keymap(&self) -> &bcode_plugin_sdk::tui::keymap::KeymapRegistry {
        &self.effective_keymap
    }

    /// Set persistent local plugin presentation state.
    pub fn set_plugin_presentation(
        &mut self,
        presentation: Arc<crate::plugin_tui::PluginTuiPresentation>,
    ) {
        self.plugin_presentation = Some(presentation);
        self.sync_plugin_structure();
    }

    /// Adopt the loaded plugins' manifest `[presentation]` declarations: their
    /// default rules and property defaults become plugin layers, their
    /// declared target fields feed the `target` fact, and their declared
    /// facts and property schemas are registered with the structure. Returns
    /// whether the structure changed.
    ///
    /// Runs whenever the plugin catalog changes: every catalog (re)load, from
    /// startup or a configuration reload, installs a new presentation through
    /// [`Self::set_plugin_presentation`]. The effective declarations are the
    /// fingerprint: when they are unchanged nothing is re-derived or
    /// recompiled, and changed declarations that yield the same layers only
    /// update target fields.
    pub fn sync_plugin_structure(&mut self) -> bool {
        let declarations = self
            .plugin_presentation
            .as_deref()
            .map(crate::plugin_tui::PluginTuiPresentation::presentation_declarations)
            .unwrap_or_default();
        if declarations == self.plugin_presentation_declarations {
            return false;
        }
        let fields_changed = self.apply_tool_target_fields(
            bcode_plugin::presentation::presentation_target_fields(&declarations),
        ) | self.structure.set_plugin_presentation(&declarations);
        let layers = bcode_plugin::presentation::presentation_rule_layers(&declarations);
        self.plugin_presentation_declarations = declarations;
        if layers == self.plugin_structure_layers {
            return fields_changed;
        }
        self.plugin_structure_layers = layers;
        self.recompile_structure() || fields_changed
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

    /// Return whether resident Markdown semantic indexes need reconciliation.
    pub fn begin_markdown_semantics_reconciliation(&mut self, width: u16) -> bool {
        let key = (
            self.transcript.revision(),
            self.markdown_presentation_revision,
            width,
        );
        if self.markdown_semantics_reconciled == Some(key) {
            return false;
        }
        self.markdown_semantics_reconciled = Some(key);
        true
    }

    /// Reconcile keyboard focus with visible actionable Markdown contributions.
    pub fn reconcile_markdown_interactions(
        &mut self,
        visible: Vec<crate::markdown_interaction::VisibleMarkdownContribution>,
    ) {
        self.markdown_interaction.reconcile(visible);
        if let Some(target) = self.pending_markdown_focus.take()
            && !self.markdown_interaction.focus_target(&target)
        {
            self.pending_markdown_focus = Some(target);
        }
        if self
            .markdown_source_view
            .as_ref()
            .is_some_and(|(id, _)| self.markdown_interaction.contribution(id).is_none())
        {
            self.markdown_source_view = None;
        }
        self.footnote_return_targets
            .retain(|definition, reference| {
                self.markdown_interaction.contribution(definition).is_some()
                    && self.markdown_interaction.contribution(reference).is_some()
            });
    }

    /// Reconcile retained details state with all resident typed contributions.
    pub fn reconcile_markdown_details(&mut self, resident_ids: &BTreeSet<String>) {
        let before = self.markdown_details_open.len();
        self.markdown_details_open
            .retain(|id, _| resident_ids.contains(id));
        if self.markdown_details_open.len() != before {
            self.markdown_presentation_revision =
                self.markdown_presentation_revision.saturating_add(1);
        }
    }

    /// Replace resident footnote contribution targets with transcript-global rows.
    pub fn reconcile_markdown_footnote_rows(&mut self, rows: BTreeMap<String, usize>) {
        self.markdown_footnote_rows = rows;
        self.footnote_return_targets
            .retain(|definition, reference| {
                self.markdown_footnote_rows.contains_key(definition)
                    && self.markdown_footnote_rows.contains_key(reference)
            });
        if self
            .pending_markdown_focus
            .as_ref()
            .is_some_and(|target| !self.markdown_footnote_rows.contains_key(target))
        {
            self.pending_markdown_focus = None;
        }
    }

    fn navigate_markdown_contribution(&mut self, contribution_id: &str) -> bool {
        let Some(row) = self.markdown_footnote_rows.get(contribution_id).copied() else {
            return false;
        };
        self.transcript_scroll_animation = None;
        self.pending_visual_overflow_bottom = None;
        self.session_ui.viewport.detach();
        self.session_ui.viewport.follow_anchor(row);
        self.presented_transcript_anchor = None;
        self.pending_stable_transcript_anchor = None;
        self.pending_markdown_focus = Some(contribution_id.to_owned());
        true
    }

    /// Replace resident internal Markdown fragment targets with transcript-global rows.
    pub fn reconcile_markdown_fragments(&mut self, fragments: BTreeMap<String, usize>) {
        self.markdown_fragment_rows = fragments;
    }

    fn navigate_markdown_fragment(&mut self, fragment: &str) -> bool {
        let normalized = fragment.strip_prefix('#').unwrap_or(fragment);
        let Some(row) = self.markdown_fragment_rows.get(normalized).copied() else {
            self.set_status("Markdown fragment target was not found".to_owned());
            return true;
        };
        self.transcript_scroll_animation = None;
        self.pending_visual_overflow_bottom = None;
        self.session_ui.viewport.detach();
        self.session_ui.viewport.follow_anchor(row);
        self.presented_transcript_anchor = None;
        self.pending_stable_transcript_anchor = None;
        self.set_status("Navigated to Markdown section".to_owned());
        true
    }

    /// Move Markdown focus in visible document order.
    pub fn move_markdown_focus(&mut self, reverse: bool) -> bool {
        let changed = if reverse {
            self.markdown_interaction.focus_previous()
        } else {
            self.markdown_interaction.focus_next()
        };
        if changed && self.markdown_interaction.focused().is_some() {
            let hint = format!(
                "{} activates; {} copies; {} moves",
                self.shortcut_hint("chat", "tui.markdown.activate"),
                self.shortcut_hint("chat", "tui.markdown.copyDestination"),
                self.shortcut_hint("chat", "tui.markdown.focusNext")
            );
            self.set_status(format!("Markdown contribution focused; {hint}"));
        }
        changed
    }

    /// Focus one currently visible Markdown contribution.
    pub fn focus_markdown_contribution(&mut self, contribution_id: &str) -> bool {
        self.markdown_interaction.focus(contribution_id)
    }

    /// Return the focused Markdown contribution ID.
    #[must_use]
    pub fn focused_markdown_contribution(&self) -> Option<&str> {
        self.markdown_interaction.focused()
    }

    /// Return the typed payload for a visible Markdown contribution.
    #[must_use]
    pub fn visible_markdown_contribution(
        &self,
        contribution_id: &str,
    ) -> Option<&bcode_markdown_render::MarkdownContributionKind> {
        self.markdown_interaction.contribution(contribution_id)
    }

    /// Return explicit details disclosure state for Markdown rendering.
    #[must_use]
    pub const fn markdown_details_open(&self) -> &BTreeMap<String, bool> {
        &self.markdown_details_open
    }

    /// Return the revision of layout-affecting Markdown presentation state.
    #[must_use]
    pub const fn markdown_presentation_revision(&self) -> u64 {
        self.markdown_presentation_revision
    }

    #[cfg(test)]
    pub(crate) fn set_transcript_viewport_for_details_test(
        &mut self,
        total_rows: usize,
        viewport_height: u16,
        scroll_up: usize,
    ) {
        self.session_ui.viewport.sync_max(
            total_rows.saturating_sub(usize::from(viewport_height)),
            0,
            total_rows,
            viewport_height,
            false,
            &mut self.older_history,
        );
        self.session_ui
            .viewport
            .scroll_up(scroll_up, &mut self.older_history);
    }

    #[cfg(test)]
    fn transcript_viewport_top_row_for_test(
        &self,
        total_rows: usize,
        viewport_height: u16,
    ) -> usize {
        self.session_ui
            .viewport
            .top_row(total_rows, viewport_height)
    }

    /// Capture the visible reader's position, including a banner row when it
    /// precedes the first source-backed transcript item.
    #[cfg(test)]
    pub(crate) fn frame_observation(&self) -> TranscriptFrameObservation {
        let total_rows = self.transcript_layout.total_rows();
        let viewport_top = self
            .session_ui
            .viewport
            .top_row(total_rows, self.session_ui.viewport.height());
        let anchor = self
            .transcript_layout
            .visible_lines_from_top(viewport_top, self.session_ui.viewport.height().max(1))
            .into_iter()
            .find(|line| line.source == VisibleTranscriptSource::Transcript)
            .and_then(|line| {
                self.transcript
                    .presentation_id(line.entry_index)
                    .map(|item_id| StableTranscriptAnchor {
                        item_id,
                        fallback_index: line.entry_index,
                        source_position: self
                            .transcript_layout
                            .source_position(line.entry_index, line.row_in_entry),
                        row_in_item: line.row_in_entry,
                        viewport_row_offset: line.row_index.saturating_sub(viewport_top),
                        content: self
                            .transcript_layout
                            .content_anchor(line.entry_index, line.row_in_entry)
                            .map(|(key, offset)| (key.to_owned(), offset)),
                    })
            });
        TranscriptFrameObservation {
            semantic_items: self
                .session_view
                .snapshot()
                .transcript
                .items
                .iter()
                .map(|item| (item.id.clone(), item.revision))
                .collect(),
            terminal_items: self
                .transcript
                .items()
                .iter()
                .map(|item| {
                    (
                        item.id(),
                        item.source_view_item_id().cloned(),
                        item.source_view_item_revision(),
                    )
                })
                .collect(),
            terminal_rows: (0..total_rows)
                .filter_map(|row| self.transcript_layout.line_at_row(row))
                .map(|line| {
                    let item_id = (line.source == VisibleTranscriptSource::Transcript)
                        .then(|| {
                            self.transcript
                                .get(line.entry_index)?
                                .source_view_item_id()
                                .cloned()
                        })
                        .flatten();
                    (item_id, line.row_in_entry)
                })
                .collect(),
            damage: self.observations.last_transcript_damage.clone(),
            viewport_top,
            scroll_mode: if self.transcript_scroll_animation.is_some() {
                "transition_to_entry"
            } else {
                self.session_ui.viewport.intent_name()
            },
            anchor,
        }
    }

    fn preserve_transcript_anchor_for_disclosure(&mut self) {
        self.transcript_scroll_animation = None;
        self.pending_visual_overflow_bottom = None;
        self.session_ui.viewport.detach();
        let top_row = self.session_ui.viewport.top_row(
            self.transcript_layout.total_rows(),
            self.session_ui.viewport.height(),
        );
        self.session_ui.viewport.materialize_top_row(top_row);
    }

    /// Activate a currently visible Markdown contribution through its typed payload.
    pub fn activate_markdown_contribution(&mut self, contribution_id: &str) -> bool {
        use bcode_markdown_render::MarkdownContributionKind;

        let contribution = self.visible_markdown_contribution(contribution_id).cloned();
        if let Some(MarkdownContributionKind::Details { default_open, .. }) = contribution {
            self.preserve_transcript_anchor_for_disclosure();
            let open = self
                .markdown_details_open
                .get(contribution_id)
                .copied()
                .unwrap_or(default_open);
            self.markdown_details_open
                .insert(contribution_id.to_owned(), !open);
            self.markdown_presentation_revision =
                self.markdown_presentation_revision.saturating_add(1);
            self.set_status(if open {
                "Details collapsed".to_owned()
            } else {
                "Details expanded".to_owned()
            });
            return true;
        }
        if let Some(MarkdownContributionKind::Mermaid { source, .. }) = contribution {
            if self
                .markdown_source_view()
                .is_some_and(|(active_id, _)| active_id == contribution_id)
            {
                self.markdown_source_view = None;
                self.set_status("Mermaid source view closed".to_owned());
            } else {
                self.markdown_source_view = Some((contribution_id.to_owned(), source));
                self.set_status("Mermaid source view opened; activate again to close".to_owned());
            }
            return true;
        }
        if let Some(MarkdownContributionKind::FootnoteReference { target_id, .. }) = &contribution {
            if target_id.is_empty()
                || !(self.markdown_interaction.focus_target(target_id)
                    || self.navigate_markdown_contribution(target_id))
            {
                self.set_status("Footnote definition is not available".to_owned());
                return true;
            }
            self.footnote_return_targets
                .insert(target_id.clone(), contribution_id.to_owned());
            self.set_status("Footnote definition focused; activate to return".to_owned());
            return true;
        }
        if let Some(MarkdownContributionKind::FootnoteDefinition { reference_ids, .. }) =
            &contribution
        {
            let target = self
                .footnote_return_targets
                .remove(contribution_id)
                .or_else(|| reference_ids.first().cloned());
            if let Some(target) = target
                && (self.markdown_interaction.focus_target(&target)
                    || self.navigate_markdown_contribution(&target))
            {
                self.set_status("Returned to footnote reference".to_owned());
            } else {
                self.set_status("Footnote reference is not available".to_owned());
            }
            return true;
        }
        let destination = match contribution {
            Some(
                MarkdownContributionKind::Link { destination, .. }
                | MarkdownContributionKind::GitHubIssue { destination, .. },
            ) => destination,
            Some(
                MarkdownContributionKind::Details { .. }
                | MarkdownContributionKind::FootnoteReference { .. }
                | MarkdownContributionKind::FootnoteDefinition { .. }
                | MarkdownContributionKind::Image { .. }
                | MarkdownContributionKind::InlineMath { .. }
                | MarkdownContributionKind::DisplayMath { .. }
                | MarkdownContributionKind::Mermaid { .. },
            )
            | None => return false,
        };

        self.activate_markdown_link(destination)
    }

    fn activate_markdown_link(
        &mut self,
        destination: bcode_markdown_render::MarkdownDestination,
    ) -> bool {
        if let bcode_markdown_render::MarkdownDestination::Fragment(fragment) = &destination
            && let Some(child) = fragment.strip_prefix("bcode-session-")
            && let Ok(session_id) = child.parse()
        {
            self.pending_session_link = Some(session_id);
            return true;
        }
        match crate::markdown_activation::activate_markdown_destination(&destination) {
            Ok(crate::markdown_activation::MarkdownActivation::External) => {
                self.set_status("Opened Markdown destination".to_owned());
            }
            Ok(crate::markdown_activation::MarkdownActivation::Fragment) => {
                let bcode_markdown_render::MarkdownDestination::Fragment(fragment) = destination
                else {
                    unreachable!("fragment activation must preserve typed destination")
                };
                return self.navigate_markdown_fragment(&fragment);
            }
            Ok(crate::markdown_activation::MarkdownActivation::Inert) => return false,
            Err(error) => {
                self.set_status(format!("Could not open Markdown destination: {error}"));
            }
        }
        true
    }

    /// Return the active Mermaid source-view text.
    #[must_use]
    pub fn markdown_source_view(&self) -> Option<(&str, &str)> {
        self.markdown_source_view
            .as_ref()
            .map(|(id, source)| (id.as_str(), source.as_str()))
    }

    /// Consume an internal navigation action produced by a rendered semantic link.
    pub(crate) const fn take_session_link_request(&mut self) -> Option<SessionId> {
        self.pending_session_link.take()
    }

    /// Activate the currently focused visible Markdown contribution.
    pub fn activate_focused_markdown_contribution(&mut self) -> bool {
        let Some(contribution_id) = self.focused_markdown_contribution().map(str::to_owned) else {
            return false;
        };
        self.activate_markdown_contribution(&contribution_id)
    }

    /// Copy the safe destination of the focused Markdown contribution.
    pub fn copy_focused_markdown_destination(&mut self) -> bool {
        use bcode_markdown_render::MarkdownContributionKind;

        let Some(contribution_id) = self.focused_markdown_contribution().map(str::to_owned) else {
            return false;
        };
        let destination = match self.visible_markdown_contribution(&contribution_id) {
            Some(
                MarkdownContributionKind::Link { destination, .. }
                | MarkdownContributionKind::GitHubIssue { destination, .. },
            ) => destination.clone(),
            _ => return false,
        };
        if crate::markdown_activation::markdown_destination_text(&destination).is_none() {
            return false;
        }
        match crate::markdown_activation::copy_markdown_destination(&destination) {
            Ok(true) => self.set_status("Copied Markdown destination".to_owned()),
            Ok(false) => return false,
            Err(error) => self.set_status(format!("Could not copy Markdown destination: {error}")),
        }
        true
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

    pub fn transcript_index_for_sequence(&self, sequence: u64) -> Option<usize> {
        self.transcript
            .iter()
            .position(|item| item.event_sequence() == Some(sequence))
    }

    /// Replace the resident transcript with a fresh bounded tail snapshot.
    pub fn replace_latest_transcript_window(
        &mut self,
        events: &[SessionEvent],
        has_older_history: bool,
    ) {
        self.history_window_generation = self.history_window_generation.wrapping_add(1);
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.older_history = OlderHistoryState::new(events, has_older_history);
        self.rebuild_transcript_from_history(false);
        self.pending_visual_overflow_bottom = None;
    }

    /// Replace the resident transcript with a bounded replay window.
    pub fn replace_transcript_window(
        &mut self,
        events: &[SessionEvent],
        has_older: bool,
        has_newer: bool,
        anchor_sequence: u64,
    ) {
        self.history_window_generation = self.history_window_generation.wrapping_add(1);
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.older_history
            .replace_centered(events, has_older, has_newer, anchor_sequence);
        self.rebuild_transcript_from_history(false);
        self.pending_visual_overflow_bottom = None;
    }

    /// Defer top-anchoring a transcript event sequence until the layout cache is current.
    pub fn request_transcript_top_anchor_sequence(&mut self, sequence: u64) {
        self.navigation_checkpoint = None;
        self.pending_transcript_top_anchor_sequence = Some(sequence);
        self.transcript_scroll_animation = None;
        self.session_ui.viewport.detach();
    }

    /// Jump to a committed transcript item and top-anchor it in the viewport.
    pub fn jump_to_transcript_index(&mut self, index: usize) -> bool {
        self.navigation_checkpoint = None;
        let Some(top_row) = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)
        else {
            return false;
        };
        self.transcript_scroll_animation = None;
        self.session_ui.viewport.detach();
        self.session_ui.viewport.follow_anchor(top_row);
        true
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
        let _ = self.apply_session_view_terminal_adapter();
        true
    }

    /// Return the current session title, if known.
    #[must_use]
    pub fn session_title(&self) -> Option<&str> {
        self.session_view.snapshot().title.as_deref()
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
                self.apply_session_view_terminal_adapter();
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
        self.apply_session_view_terminal_adapter();
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
        self.apply_session_view_terminal_adapter();
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
        &mut self,
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
        let result = presentation.configure(configuration);
        if !matches!(result, Ok(false)) {
            self.elapsed_layout_revision = self.elapsed_layout_revision.saturating_add(1);
            self.transcript_layout = TranscriptLayoutCache::default();
        }
        result
    }

    /// Apply terminal UI configuration.
    pub fn apply_tui_config(&mut self, config: TuiConfig) {
        let markdown_changed = self.tui_config.markdown != config.markdown;
        self.apply_thinking_config(config.thinking);
        self.tui_config = config;
        self.theme_preview_id = None;
        self.invalidate_theme_catalog();
        if markdown_changed {
            self.markdown_presentation_revision =
                self.markdown_presentation_revision.saturating_add(1);
        }
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

    /// Apply renderer-neutral presentation configuration.
    pub fn apply_presentation_config(&mut self, config: bcode_config::PresentationConfig) -> bool {
        let streaming = self.apply_streaming_presentation_policy(config.streaming.policy());
        self.configured_structure_rules = config.rules;
        let structure = self.recompile_structure();
        streaming || structure
    }

    /// Recompile structure from every layer, lowest precedence first: host
    /// defaults (chrome density), plugin manifest defaults, theme and profile
    /// sections, configuration, then user state, stacked by the same
    /// [`bcode_tui_theme::selection::structure_layers`] as `bcode theme explain`.
    ///
    /// The configuration layer starts with the legacy reasoning visibility
    /// (`tui.thinking.show`, or its runtime toggle) mapped onto reasoning
    /// disclosure, followed by `[[presentation.rules]]`.
    fn recompile_structure(&mut self) -> bool {
        let reasoning_visible = self.reasoning_visible();
        let mut config_rules = TuiThinkingConfig::visibility_rules(reasoning_visible);
        config_rules.extend(self.configured_structure_rules.iter().cloned());
        let layers = bcode_tui_theme::selection::structure_layers(
            &self.theme_structure_layers,
            &self.plugin_structure_layers,
            &config_rules,
            &self.user_structure_rules,
        );
        let suppressed = self.structure.set_reasoning_suppressed(!reasoning_visible);
        self.structure.set_layers(&layers) || suppressed
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
        self.recompile_structure()
    }

    /// Apply producer-declared target argument fields (from plugin manifests)
    /// for the `target` fact; returns whether they changed.
    pub fn apply_tool_target_fields(
        &mut self,
        fields: super::structure::DeclaredTargetFields,
    ) -> bool {
        self.structure.set_target_fields(fields)
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
        self.recompile_structure()
    }

    /// Rule that makes the disclosure of `item_id`'s tool the user's default.
    ///
    /// Returns `None` for items without a named tool. Persisting the rule is
    /// the caller's responsibility (user state only, never configuration;
    /// see [`Self::make_focused_default`]).
    #[must_use]
    pub fn default_disclosure_rule(
        &self,
        item_id: u64,
    ) -> Option<bcode_presentation_policy::models::PresentationRule> {
        let item = self
            .transcript()
            .iter()
            .find(|item| item.id().get() == item_id)?;
        let facts = super::structure::unit_facts(item)?;
        let tool = facts.tool?;
        let disclosure = self.structure.snapshot().presentation(item)?.disclosure;
        let mut rule = bcode_presentation_policy::models::PresentationRule::default();
        rule.matcher
            .kind
            .0
            .push(bcode_presentation_policy::models::UnitKind::Tool);
        rule.matcher.tool.0.push(tool);
        rule.set.disclosure = Some(disclosure);
        Some(rule)
    }

    /// Snapshot of structural presentation for row generation.
    #[must_use]
    pub fn structure_snapshot(&self) -> super::structure::StructureSnapshot {
        self.structure.snapshot()
    }

    /// Refresh turn recency from the resident transcript and forget disclosure
    /// state for items that left it.
    pub fn refresh_structure_turns(&mut self) {
        let turns = super::structure::TurnIndex::from_items(self.transcript().iter());
        self.structure.set_turns(turns);
        self.reconcile_disclosure_state();
    }

    /// Drop overrides and focus whose items are no longer resident.
    ///
    /// Residency is keyed by length and boundary ids, so unchanged windows
    /// cost nothing per layout sync.
    fn reconcile_disclosure_state(&mut self) {
        let items = self.transcript();
        let residency = (
            items.len(),
            items.iter().next().map(|item| item.id().get()),
            items.iter().next_back().map(|item| item.id().get()),
        );
        if self.reconciled_residency == Some(residency) {
            return;
        }
        self.reconciled_residency = Some(residency);
        if !self.structure.has_overrides() && self.transcript_focus.focused().is_none() {
            return;
        }
        let resident: BTreeSet<u64> = self
            .transcript()
            .iter()
            .map(|item| item.id().get())
            .collect();
        self.structure.reconcile(&resident);
        self.transcript_focus.retain(|id| resident.contains(&id));
        self.refresh_focus_entry_hint();
    }

    /// Structural presentation state.
    #[must_use]
    pub const fn structure(&self) -> &super::structure::StructureState {
        &self.structure
    }

    fn transcript_index_of_item(&self, item_id: u64) -> Option<usize> {
        self.transcript()
            .iter()
            .position(|item| item.id().get() == item_id)
    }

    /// Recompute groups over the resident transcript; returns the indexes of
    /// items whose group role changed and must be laid out again.
    pub fn sync_structure_groups(&mut self) -> BTreeSet<usize> {
        let items: Vec<&TranscriptItem> = self.transcript.items().iter().collect();
        self.structure.sync_groups(&items)
    }

    /// Transcript item indexes whose rendering depends on `target`'s disclosure.
    ///
    /// Units and sections map to their own item. Groups map to every member
    /// and bridged item, in transcript order.
    #[must_use]
    pub fn items_for_target(&self, target: &super::structure::DisclosureTarget) -> Vec<usize> {
        use super::structure::DisclosureTarget;
        let DisclosureTarget::Group(group) = target else {
            return self
                .transcript_index_of_item(target.item_id())
                .into_iter()
                .collect();
        };
        let items = self.transcript();
        self.structure
            .snapshot()
            .group_items(*group)
            .into_iter()
            .filter_map(|(id, position)| {
                // Positions come from the last group sync; verify them and fall
                // back to a scan if the window moved since.
                position
                    .filter(|&index| items.get(index).is_some_and(|item| item.id().get() == id))
                    .or_else(|| self.transcript_index_of_item(id))
            })
            .collect()
    }

    /// First member item id of the group containing unit `item_id`, if any.
    #[must_use]
    pub fn group_of_item(&self, item_id: u64) -> Option<u64> {
        self.structure.snapshot().group_of(item_id)
    }

    /// Last item of `group` in transcript order: its last member, bridged, or
    /// attached item.
    fn group_last_item(&self, group: u64) -> Option<u64> {
        let snapshot = self.structure.snapshot();
        let view = snapshot.group_view(group)?;
        let items = self.transcript();
        view.members
            .iter()
            .chain(&view.bridged)
            .chain(&view.attached)
            .filter_map(|id| {
                items
                    .iter()
                    .position(|item| item.id().get() == *id)
                    .map(|index| (index, *id))
            })
            .max_by_key(|(index, _)| *index)
            .map(|(_, id)| id)
    }

    /// Facts of the group whose first member is `first_member`, when that
    /// group exists in the resident window.
    fn group_facts(
        &self,
        first_member: u64,
    ) -> Option<bcode_presentation_policy::models::UnitFacts> {
        self.structure
            .snapshot()
            .group_view(first_member)
            .map(|view| view.facts.clone())
    }

    /// Effective disclosure of `target` with the facts of the unit owning it,
    /// or `None` when the target is not a resident structural unit.
    #[must_use]
    pub fn target_disclosure(
        &self,
        target: &super::structure::DisclosureTarget,
    ) -> Option<(
        bcode_presentation_policy::models::Disclosure,
        bcode_presentation_policy::models::UnitFacts,
    )> {
        use super::structure::DisclosureTarget;
        let snapshot = self.structure.snapshot();
        match target {
            DisclosureTarget::Unit(id) => {
                let item = self.transcript().get(self.transcript_index_of_item(*id)?)?;
                let facts = super::structure::unit_facts(item)?;
                Some((snapshot.presentation(item)?.disclosure, facts))
            }
            DisclosureTarget::Section(id, name) => {
                let item = self.transcript().get(self.transcript_index_of_item(*id)?)?;
                let facts = super::structure::unit_facts(item)?;
                Some((snapshot.section(item, name).disclosure, facts))
            }
            DisclosureTarget::Group(first) => {
                let facts = self.group_facts(*first)?;
                Some((snapshot.group_disclosure(*first)?, facts))
            }
        }
    }

    fn mark_targets_dirty<'a>(
        &mut self,
        targets: impl IntoIterator<Item = &'a super::structure::DisclosureTarget>,
    ) {
        let indexes: BTreeSet<usize> = targets
            .into_iter()
            .flat_map(|target| self.items_for_target(target))
            .collect();
        for index in indexes {
            self.mark_transcript_item_dirty(index);
        }
    }

    fn disclosure_changed(
        &mut self,
        target: &super::structure::DisclosureTarget,
        disclosure: bcode_presentation_policy::models::Disclosure,
    ) {
        use super::structure::DisclosureTarget;
        self.preserve_transcript_anchor_for_disclosure();
        self.mark_targets_dirty([target]);
        let label = disclosure_label(disclosure);
        self.set_status(match target {
            DisclosureTarget::Unit(_) => format!("Tool {label}"),
            DisclosureTarget::Group(_) => format!("Group {label}"),
            DisclosureTarget::Section(_, name) => format!("Section {name} {label}"),
        });
    }

    /// Toggle `target` between expanded and collapsed (summarized or hidden
    /// targets expand first); returns whether anything changed.
    ///
    /// Targets of units awaiting a decision never collapse.
    pub fn toggle_target(&mut self, target: &super::structure::DisclosureTarget) -> bool {
        let Some((current, facts)) = self.target_disclosure(target) else {
            return false;
        };
        let Some(disclosure) = self.structure.toggle_target(target, current, &facts) else {
            return false;
        };
        self.disclosure_changed(target, disclosure);
        true
    }

    /// Set `target` to `disclosure`; returns whether anything changed.
    ///
    /// Targets of units awaiting a decision never collapse.
    pub fn set_target_disclosure(
        &mut self,
        target: &super::structure::DisclosureTarget,
        disclosure: bcode_presentation_policy::models::Disclosure,
    ) -> bool {
        let Some((current, facts)) = self.target_disclosure(target) else {
            return false;
        };
        if current == disclosure || !self.structure.set_target(target, disclosure, &facts) {
            return false;
        }
        self.disclosure_changed(target, disclosure);
        true
    }

    /// Toggle the disclosure of the transcript item with `item_id`.
    pub fn toggle_item_disclosure(&mut self, item_id: u64) -> bool {
        self.toggle_target(&super::structure::DisclosureTarget::Unit(item_id))
    }

    /// Drop manual disclosure toggles, returning to theme and configured defaults.
    ///
    /// In focus mode only the focused target's subtree is reset.
    pub fn reset_disclosure(&mut self) -> bool {
        if let Some(target) = self.transcript_focus.focused().cloned() {
            let subtree = self.target_subtree(&target);
            if !self.structure.clear_targets(|candidate| {
                subtree.iter().any(|root| target_contains(root, candidate))
            }) {
                return false;
            }
            self.preserve_transcript_anchor_for_disclosure();
            self.mark_targets_dirty(&subtree);
            self.set_status("Focused disclosure reset to defaults".to_owned());
            return true;
        }
        if !self.structure.clear_overrides() {
            return false;
        }
        self.preserve_transcript_anchor_for_disclosure();
        self.transcript_layout = TranscriptLayoutCache::default();
        self.set_status("Tool disclosure reset to defaults".to_owned());
        true
    }

    /// Expand or collapse every unit and group in the transcript, or only
    /// the focused target's subtree in focus mode.
    pub fn set_all_disclosure(
        &mut self,
        disclosure: bcode_presentation_policy::models::Disclosure,
    ) -> bool {
        if let Some(target) = self.transcript_focus.focused().cloned() {
            return self.set_subtree_disclosure(&target, disclosure);
        }
        let items: Vec<(usize, TranscriptItem)> =
            self.transcript().iter().cloned().enumerate().collect();
        let changed = self
            .structure
            .set_all(items.iter().map(|(index, item)| (*index, item)), disclosure);
        if changed.is_empty() {
            return false;
        }
        self.preserve_transcript_anchor_for_disclosure();
        for index in changed {
            self.mark_transcript_item_dirty(index);
        }
        self.transcript_tools.note_global_detail(disclosure);
        self.set_status(format!("All units {}", disclosure_label(disclosure)));
        true
    }

    /// Unit-level roots under `target`: the target itself plus, for groups,
    /// each member unit (bridged items are not members).
    fn target_subtree(
        &self,
        target: &super::structure::DisclosureTarget,
    ) -> Vec<super::structure::DisclosureTarget> {
        use super::structure::DisclosureTarget;
        let mut subtree = vec![target.clone()];
        if let DisclosureTarget::Group(group) = target
            && let Some(view) = self.structure.snapshot().group_view(*group)
        {
            subtree.extend(view.members.iter().copied().map(DisclosureTarget::Unit));
        }
        subtree
    }

    /// Set `target` and everything under it to `disclosure`.
    ///
    /// Sections follow only when expanding: a collapsed unit already hides
    /// them, and collapsing should not discard which sections were open.
    fn set_subtree_disclosure(
        &mut self,
        target: &super::structure::DisclosureTarget,
        disclosure: bcode_presentation_policy::models::Disclosure,
    ) -> bool {
        use super::structure::DisclosureTarget;
        use bcode_presentation_policy::models::Disclosure;
        let mut targets = Vec::new();
        for root in self.target_subtree(target) {
            if disclosure == Disclosure::Expanded
                && let DisclosureTarget::Unit(id) = root
            {
                targets.extend(self.unit_sections(id));
            }
            targets.push(root);
        }
        let mut changed = Vec::new();
        for candidate in targets {
            let Some((current, facts)) = self.target_disclosure(&candidate) else {
                continue;
            };
            if current != disclosure && self.structure.set_target(&candidate, disclosure, &facts) {
                changed.push(candidate);
            }
        }
        if changed.is_empty() {
            return false;
        }
        self.preserve_transcript_anchor_for_disclosure();
        self.mark_targets_dirty(&changed);
        self.set_status(format!(
            "Focused target {} with its contents",
            disclosure_label(disclosure)
        ));
        true
    }

    /// Section targets of unit `id` known to the policy or to overrides.
    fn unit_sections(&self, id: u64) -> Vec<super::structure::DisclosureTarget> {
        use super::structure::DisclosureTarget;
        let snapshot = self.structure.snapshot();
        let mut names: BTreeSet<String> = snapshot
            .override_targets()
            .filter_map(|target| match target {
                DisclosureTarget::Section(item, name) if *item == id => Some(name.clone()),
                _ => None,
            })
            .collect();
        if let Some(item) = self
            .transcript_index_of_item(id)
            .and_then(|index| self.transcript().get(index))
            && let Some(presentation) = snapshot.presentation(item)
        {
            names.extend(presentation.sections.into_keys());
        }
        names
            .into_iter()
            .map(|name| DisclosureTarget::Section(id, name))
            .collect()
    }

    /// Whether keyboard focus mode owns the keyboard.
    #[must_use]
    pub const fn transcript_focus_active(&self) -> bool {
        self.transcript_focus.is_active()
    }

    /// Disclosure target painted with the selection style in focus mode.
    #[must_use]
    pub const fn focused_disclosure_target(&self) -> Option<&super::structure::DisclosureTarget> {
        self.transcript_focus.focused()
    }

    /// Disclosure anchors of the retained transcript layout.
    #[must_use]
    ///
    /// Items are located by id through the focused target's verified entry
    /// hint first, so painting the focused target each frame does not scan
    /// the resident window; other ids fall back to a scan.
    pub fn disclosure_anchors(
        &self,
    ) -> super::transcript_focus::LayoutAnchors<'_, impl Fn(u64) -> Option<usize> + '_> {
        let items = self.transcript();
        let hint = self.transcript_focus.entry_hint();
        super::transcript_focus::LayoutAnchors::new(
            &self.transcript_layout,
            items.len(),
            move |id| {
                hint.filter(|&index| items.get(index).is_some_and(|item| item.id().get() == id))
                    .or_else(|| items.iter().position(|item| item.id().get() == id))
            },
        )
    }

    /// Re-locate the focused target's item when the recorded hint no longer
    /// points at it (focus moved or the resident window changed).
    fn refresh_focus_entry_hint(&mut self) {
        let Some(id) = self
            .transcript_focus
            .focused()
            .map(super::structure::DisclosureTarget::item_id)
        else {
            self.transcript_focus.set_entry_hint(None);
            return;
        };
        let items = self.transcript();
        if self
            .transcript_focus
            .entry_hint()
            .and_then(|index| items.get(index))
            .is_some_and(|item| item.id().get() == id)
        {
            return;
        }
        let index = self.transcript_index_of_item(id);
        self.transcript_focus.set_entry_hint(index);
    }

    /// Global transcript row of `target`'s anchor, when it is laid out.
    #[must_use]
    pub fn disclosure_target_row(
        &self,
        target: &super::structure::DisclosureTarget,
    ) -> Option<usize> {
        super::transcript_focus::target_row(&self.disclosure_anchors(), target)
    }

    /// Enter focus mode on the last visible disclosure target; returns
    /// whether the UI changed.
    pub fn enter_transcript_focus(&mut self) -> bool {
        let height = self.session_ui.viewport.height();
        let top_row = self.transcript_top_row(height);
        let Some(target) =
            super::transcript_focus::initial_target(&self.disclosure_anchors(), top_row, height)
        else {
            self.set_status("No expandable transcript items to focus".to_owned());
            return true;
        };
        self.release_child_panel_focus();
        self.transcript_focus.enter(target);
        self.reveal_focused_target();
        self.set_status(self.transcript_focus_hint());
        true
    }

    /// Leave focus mode, closing the inspector that follows focus; returns
    /// whether it was active.
    pub fn exit_transcript_focus(&mut self) -> bool {
        if !self.transcript_focus.exit() {
            return false;
        }
        // The inspector explains the focused target and its keys live in the
        // focus scope, so it cannot outlive focus mode.
        let _closed = self.close_inspector();
        self.set_status("Transcript focus closed".to_owned());
        true
    }

    /// Footer hint listing the effective focus-mode bindings.
    #[must_use]
    pub fn transcript_focus_hint(&self) -> String {
        use super::transcript_focus::{FOCUS_SCOPE, FocusAction};
        format!(
            "UNIT FOCUS · {} next {} previous {} toggle {} expand {} collapse {} expand all · {} leave",
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Next.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Previous.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Toggle.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Expand.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Collapse.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::ExpandAll.id()),
            self.shortcut_hint(FOCUS_SCOPE, FocusAction::Exit.id()),
        )
    }

    /// Scroll so the focused target's anchor row is visible.
    fn reveal_focused_target(&mut self) {
        self.reveal_focused_target_from(None);
    }

    /// Scroll the focused target into view. After a move the viewport edge
    /// the focus approached follows the whole target; otherwise its anchor row
    /// is revealed at the nearest edge.
    fn reveal_focused_target_from(&mut self, motion: Option<super::transcript_focus::FocusMotion>) {
        use super::structure::DisclosureTarget;
        self.refresh_focus_entry_hint();
        let Some(target) = self.transcript_focus.focused().cloned() else {
            return;
        };
        let height = self.session_ui.viewport.height();
        let top_row = self.transcript_top_row(height);
        let next_top = motion.map_or_else(
            || {
                self.disclosure_target_row(&target)
                    .and_then(|row| super::transcript_focus::reveal_top_row(row, top_row, height))
            },
            |motion| {
                let last_item = match &target {
                    DisclosureTarget::Group(group) => self.group_last_item(*group),
                    DisclosureTarget::Unit(_) | DisclosureTarget::Section(..) => None,
                };
                super::transcript_focus::target_extent(
                    &self.disclosure_anchors(),
                    &target,
                    last_item,
                )
                .and_then(|extent| {
                    super::transcript_focus::follow_top_row(&extent, top_row, height, motion)
                })
            },
        );
        if let Some(top_row) = next_top {
            self.transcript_scroll_animation = None;
            self.session_ui.viewport.detach();
            self.session_ui.viewport.follow_anchor(top_row);
        }
    }

    /// Run one focus-mode action; returns whether the UI changed.
    pub fn handle_transcript_focus_action(
        &mut self,
        action: super::transcript_focus::FocusAction,
    ) -> bool {
        use super::transcript_focus::{self as focus, FocusAction};
        if !self.transcript_focus.is_active() {
            return false;
        }
        if action == FocusAction::Exit {
            return self.close_inspector() || self.exit_transcript_focus();
        }
        if let Some(changed) = self.handle_transcript_tool_action(action) {
            return changed;
        }
        let Some(target) = self.transcript_focus.focused().cloned() else {
            // The focused item left the resident window: refocus from the viewport.
            let height = self.session_ui.viewport.height();
            let top_row = self.transcript_top_row(height);
            let Some(target) = focus::initial_target(&self.disclosure_anchors(), top_row, height)
            else {
                return false;
            };
            self.transcript_focus.focus(target);
            self.reveal_focused_target();
            return true;
        };
        match action {
            FocusAction::Next | FocusAction::Previous => {
                let Some(next) = focus::step(
                    &self.disclosure_anchors(),
                    &target,
                    action == FocusAction::Next,
                ) else {
                    return false;
                };
                self.transcript_focus.focus(next);
                self.reveal_focused_target_from(Some(if action == FocusAction::Next {
                    focus::FocusMotion::Down
                } else {
                    focus::FocusMotion::Up
                }));
                true
            }
            FocusAction::Toggle => self.toggle_target(&target),
            FocusAction::Expand => self
                .target_disclosure(&target)
                .and_then(|(current, _)| focus::expand_level(&target, current))
                .is_some_and(|next| self.set_target_disclosure(&target, next)),
            FocusAction::Collapse => {
                let Some((current, _)) = self.target_disclosure(&target) else {
                    return false;
                };
                // A target the floor keeps open (a failed unit stays at
                // summary) moves focus to its parent instead.
                if let Some(next) = focus::collapse_level(&target, current)
                    && self.set_target_disclosure(&target, next)
                {
                    return true;
                }
                let Some(parent) = focus::parent(&target, |id| self.group_of_item(id)) else {
                    return false;
                };
                self.transcript_focus.focus(parent);
                self.reveal_focused_target();
                true
            }
            FocusAction::ExpandAll => self.set_subtree_disclosure(
                &target,
                bcode_presentation_policy::models::Disclosure::Expanded,
            ),
            FocusAction::Exit
            | FocusAction::CycleDetail
            | FocusAction::Inspect
            | FocusAction::InspectorScrollDown
            | FocusAction::InspectorScrollUp
            | FocusAction::MakeDefault => unreachable!("handled above"),
        }
    }

    /// Apply one normalized renderer-neutral streaming-presentation policy.
    pub fn apply_streaming_presentation_policy(
        &mut self,
        policy: bcode_session_view_models::StreamingPresentationPolicy,
    ) -> bool {
        let changed = self
            .session_view
            .set_streaming_presentation_policy(policy.normalized());
        if changed {
            let _ = self.apply_session_view_terminal_adapter();
        }
        changed
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
        let (target, layers) = super::theme::resolve_theme_with_structure(self);
        self.target_theme = target;
        if layers != self.theme_structure_layers {
            self.theme_structure_layers = layers;
            self.recompile_structure();
        }
        self.retarget_theme_transition(target.accent, now);
        self.update_theme_animation(now);
        self.settle_motion_if_instant(now);
    }

    /// Settle running transcript animations once the effective motion turns
    /// instant (motion `none` or `reduce_motion`, from a theme change, a
    /// config apply, or a runtime toggle): a scroll animation lands at its
    /// target and the latest-bar glow shows the still bar, and neither
    /// schedules further frames. Accent transitions settle in
    /// [`Self::retarget_theme_transition`].
    ///
    /// The hidden-activity time is kept: it also records that unseen content
    /// arrived below the viewport, which the latest bar and its jump target
    /// depend on regardless of motion. When motion returns while that
    /// activity is still inside the glow window, the glow resumes with fresh
    /// frames rather than painting one frozen frame.
    fn settle_motion_if_instant(&mut self, now: Instant) {
        let policy = self.motion_policy();
        if policy.transcript_scroll().is_zero()
            && let Some(animation) = self.transcript_scroll_animation.take()
        {
            let row = match animation.target {
                TranscriptAnimationTarget::LatestBottom => None,
                TranscriptAnimationTarget::ItemTop(id) => Some(
                    self.transcript
                        .presentation_index(id)
                        .and_then(|index| {
                            self.transcript_layout
                                .entry_start_row(VisibleTranscriptSource::Transcript, index)
                        })
                        .unwrap_or(animation.target_top_row),
                ),
                TranscriptAnimationTarget::Row => Some(animation.target_top_row),
            };
            if let Some(row) = row {
                self.session_ui.viewport.materialize_top_row(row);
            } else {
                self.session_ui
                    .viewport
                    .scroll_to_bottom(&mut self.older_history);
            }
        }
        if policy.latest_bar_window().is_zero() {
            self.latest_hidden_activity_burst = 0;
            self.latest_bar_next_frame_at = None;
        } else if self.latest_bar_next_frame_at.is_none() && self.latest_bar_active(now) {
            self.latest_bar_next_frame_at = Some(next_latest_bar_frame_at(
                self.latest_bar_animation_started_at,
                now,
                latest_bar_active_frame_duration(self.latest_hidden_activity_burst),
            ));
        }
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
            .session_view
            .snapshot()
            .runtime
            .display_name
            .as_deref()
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
        self.session_ui.composer.content_area()
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
        self.session_ui
            .composer
            .set_content_area(area, &composer_policy());
    }

    /// Return the composer scroll offset that should be used for the latest content area.
    pub fn composer_scroll_offset_for_render(&self) -> usize {
        let offset = if self.session_ui.composer.vertical_scroll() == u64::MAX {
            self.session_ui
                .composer
                .cursor_scroll_offset(&composer_policy())
                .unwrap_or(0)
        } else {
            self.session_ui.composer.vertical_scroll()
        };
        usize::try_from(offset).unwrap_or(usize::MAX)
    }

    /// Return the composer text input state.
    #[must_use]
    pub const fn composer_state(&self) -> &TextInputState {
        &self.session_ui.composer
    }

    /// Return the composer text input state mutably.
    pub const fn composer_state_mut(&mut self) -> &mut TextInputState {
        &mut self.session_ui.composer
    }

    /// Return the composer buffer.
    #[must_use]
    pub const fn composer(&self) -> &TextEditBuffer {
        self.session_ui.composer.buffer()
    }

    /// Return the composer buffer mutably.
    pub fn composer_mut(&mut self) -> &mut TextEditBuffer {
        self.session_ui.composer.buffer_mut()
    }

    /// Insert pasted text into the composer.
    pub fn paste_composer_text(&mut self, text: &str) {
        self.session_ui.update(
            bcode_tui_components::session_ui::SessionUiAction::Paste(text.to_owned()),
            &mut self.older_history,
        );
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

    /// Return terminal presentation items, including TUI-owned ephemeral notices.
    #[must_use]
    pub fn transcript(&self) -> TranscriptItems<'_> {
        self.transcript.items()
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

    /// Return revision for elapsed-time labels in active transcript entries.
    #[must_use]
    pub const fn elapsed_layout_revision(&self) -> u64 {
        self.elapsed_layout_revision
    }

    /// Return revision for transcript collection changes.
    #[must_use]
    pub const fn transcript_projection_revision(&self) -> u64 {
        self.transcript_projection_revision
    }

    /// Drain dirty presentation identities and resolve them against current document order.
    pub fn drain_transcript_dirty_items(&mut self) -> BTreeSet<usize> {
        std::mem::take(&mut self.transcript_dirty_items)
            .into_iter()
            .filter_map(|id| self.transcript.presentation_index(id))
            .collect()
    }

    #[cfg(test)]
    #[must_use]
    pub const fn transcript_dirty_items_for_test(
        &self,
    ) -> &BTreeSet<TranscriptPresentationEntryId> {
        &self.transcript_dirty_items
    }

    pub fn drain_elapsed_dirty_visuals_bounded(&mut self, limit: usize) -> BTreeSet<String> {
        let mut drained = BTreeSet::new();
        for _ in 0..limit {
            let Some(invocation_id) = self.elapsed_dirty_visuals.pop_first() else {
                break;
            };
            drained.insert(invocation_id);
        }
        drained
    }

    /// Drain all transcript invocation identifiers whose elapsed-time label changed.
    #[cfg(test)]
    pub fn drain_elapsed_dirty_visuals(&mut self) -> BTreeSet<String> {
        std::mem::take(&mut self.elapsed_dirty_visuals)
    }

    /// Return revision for layout-affecting pending submission changes.
    #[must_use]
    pub const fn pending_submissions_projection_revision(&self) -> u64 {
        self.pending_submissions.revision()
    }

    /// Extend composer selection with an editor motion.
    pub fn extend_composer_selection(&mut self, motion: TextMotion) {
        self.input_history.reset_navigation();
        let width = usize::from(self.session_ui.composer.content_area().width.max(1));
        match motion {
            TextMotion::VisualUp => self.extend_composer_selection_to_visual_delta(width, -1),
            TextMotion::VisualDown => self.extend_composer_selection_to_visual_delta(width, 1),
            motion => self
                .session_ui
                .composer
                .buffer_mut()
                .move_cursor_with_selection(motion, SelectionMode::Extend),
        }
        self.wake_cursor();
    }

    /// Handle a composer mouse event through the reusable text-input component.
    pub fn handle_composer_mouse(&mut self, mouse: MouseEvent) -> TextInputOutcome {
        let outcome = TextInputControl::new(&composer_policy())
            .handle_mouse(&mut self.session_ui.composer, mouse);
        if matches!(outcome, TextInputOutcome::Edited | TextInputOutcome::Redraw) {
            self.input_history.reset_navigation();
            self.wake_cursor();
        }
        outcome
    }

    /// Return whether a composer mouse selection is active.
    #[must_use]
    pub const fn composer_mouse_selection_active(&self) -> bool {
        self.session_ui.composer.mouse_selection_active()
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
        let width = usize::from(self.session_ui.composer.content_area().width.max(1));
        let layout = self.session_ui.composer.buffer().wrapped_layout(width);
        if layout.cursor.row == 0 {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.session_ui
            .composer
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
        let width = usize::from(self.session_ui.composer.content_area().width.max(1));
        let layout = self.session_ui.composer.buffer().wrapped_layout(width);
        if layout.cursor.row.saturating_add(1) >= layout.lines.len() {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.session_ui
            .composer
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

    /// Mark one transcript presentation entry dirty after a presentation completion.
    pub fn mark_transcript_item_dirty(&mut self, index: usize) {
        if let Some(id) = self.transcript.presentation_id(index) {
            self.transcript_dirty_items.insert(id);
            self.transcript_projection_revision =
                self.transcript_projection_revision.saturating_add(1);
        }
    }

    /// Return pending submissions that have not been committed by the session stream.
    #[must_use]
    pub fn pending_submissions(&self) -> &[PendingSubmission] {
        self.pending_submissions.items()
    }

    /// Return cached transcript layout.
    #[must_use]
    pub const fn transcript_layout(&self) -> &TranscriptLayoutCache {
        &self.transcript_layout
    }

    /// Return mutable cached transcript layout.
    #[must_use]
    pub const fn transcript_layout_mut(&mut self) -> &mut TranscriptLayoutCache {
        &mut self.transcript_layout
    }

    /// Return the retained Markdown projection cache.
    #[must_use]
    pub const fn transcript_markdown_cache(
        &self,
    ) -> &crate::transcript_markdown_cache::TranscriptMarkdownCache {
        &self.transcript_markdown_cache
    }

    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub const fn scroll_offset(&self) -> usize {
        self.session_ui.viewport.offset()
    }

    /// Return the number of virtual transcript rows below the newest content.
    #[must_use]
    pub const fn bottom_overscroll(&self) -> usize {
        self.session_ui.viewport.bottom_overscroll()
    }

    /// Return whether there is a newer transcript entry fully below the viewport.
    #[must_use]
    pub fn newer_transcript_content_below(&self) -> bool {
        self.hidden_entry_start_row_below_viewport().is_some()
    }

    fn resolve_pending_transcript_anchor(&self) -> Option<usize> {
        let anchor = self.pending_stable_transcript_anchor.as_ref()?;
        let index = self
            .transcript
            .presentation_index(anchor.item_id)
            .unwrap_or_else(|| {
                anchor
                    .fallback_index
                    .min(self.transcript.items().len().saturating_sub(1))
            });
        let start = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)?;
        let rows = self
            .transcript_layout
            .entry_row_count(VisibleTranscriptSource::Transcript, index)?;
        let row = if self.transcript.presentation_id(index) == Some(anchor.item_id) {
            anchor.resolve_row(&self.transcript_layout, index, rows)
        } else {
            0
        };
        Some(
            start
                .saturating_add(row)
                .saturating_sub(anchor.viewport_row_offset),
        )
    }

    /// Measure latest-content chrome without consuming navigation state.
    pub(crate) fn needs_latest_bar_at_height(&self, height: u16) -> bool {
        let total_rows = self.transcript_layout.total_rows();
        let mut viewport = self.session_ui.viewport;
        let mut history = self.older_history.clone();
        let anchor = self.resolve_pending_transcript_anchor();
        viewport.sync_with_anchor(
            (
                total_rows.saturating_sub(usize::from(height)),
                usize::from(height).saturating_sub(1),
                total_rows,
                height,
            ),
            anchor,
            self.older_history_page_rows(),
            self.newer_history_page_rows(),
            &mut history,
        );
        let bottom = viewport.bottom_row(total_rows);
        bottom < total_rows
            && (self.transcript_layout.changed_content_below(bottom)
                || self.latest_hidden_activity_at.is_some()
                || self
                    .transcript_layout
                    .first_entry_start_at_or_after_row(bottom)
                    .is_some())
    }

    fn hidden_entry_start_row_below_viewport(&self) -> Option<usize> {
        let total_rows = self.transcript_layout.total_rows();
        let viewport_bottom = self.session_ui.viewport.bottom_row(total_rows);
        if viewport_bottom >= total_rows {
            return None;
        }
        if self
            .transcript_layout
            .changed_content_below(viewport_bottom)
            || self.latest_hidden_activity_at.is_some()
        {
            Some(viewport_bottom)
        } else {
            self.transcript_layout
                .first_entry_start_at_or_after_row(viewport_bottom)
        }
    }

    /// Return the most recent time hidden transcript content changed.
    #[must_use]
    pub const fn latest_hidden_activity_at(&self) -> Option<Instant> {
        self.latest_hidden_activity_at
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

    /// Duration of the running transcript scroll animation, if any.
    #[cfg(test)]
    pub(crate) fn transcript_scroll_animation_duration_for_test(&self) -> Option<Duration> {
        self.transcript_scroll_animation
            .map(|animation| animation.duration)
    }

    /// Whether a transcript scroll animation frame is currently requested.
    #[cfg(test)]
    pub(crate) fn transcript_scroll_frame_requested_for_test(&self) -> bool {
        self.invalidation_requests(Instant::now(), SystemTime::now())
            .iter()
            .any(|request| is_transcript_scroll_animation_invalidation(&request.key))
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

    /// Return the current hidden transcript activity burst intensity.
    #[must_use]
    pub const fn latest_hidden_activity_burst(&self) -> u8 {
        self.latest_hidden_activity_burst
    }

    /// Return the latest-bar animation origin.
    #[must_use]
    pub const fn latest_bar_animation_started_at(&self) -> Instant {
        self.latest_bar_animation_started_at
    }

    /// Return the key label for jumping to the latest transcript content.
    #[must_use]
    pub fn jump_to_latest_key_label(&self) -> &str {
        &self.jump_to_latest_key_label
    }

    #[cfg(test)]
    pub fn stable_transcript_anchor(&self) -> Option<(TranscriptPresentationEntryId, usize)> {
        let top_row = self.transcript_top_row(self.session_ui.viewport.height());
        let line = self
            .transcript_layout
            .visible_lines_from_top(top_row, 1)
            .first()
            .copied()?;
        if line.source != VisibleTranscriptSource::Transcript {
            return None;
        }
        Some((
            self.transcript.presentation_id(line.entry_index)?,
            line.row_in_entry,
        ))
    }

    #[cfg(test)]
    pub fn transcript_row_for_source_position_for_test(
        &self,
        sequence: u64,
        position: usize,
    ) -> Option<usize> {
        let index = self
            .transcript
            .items()
            .iter()
            .position(|item| item.event_sequence() == Some(sequence))?;
        self.transcript_layout.source_row(index, position)
    }

    #[cfg(test)]
    pub fn stable_transcript_source_position_for_test(&self) -> Option<(u64, usize)> {
        let top_row = self.transcript_top_row(self.session_ui.viewport.height());
        let line = self
            .transcript_layout
            .visible_lines_from_top(top_row, 1)
            .first()
            .copied()?;
        if line.source != VisibleTranscriptSource::Transcript {
            return None;
        }
        Some((
            self.transcript
                .items()
                .get(line.entry_index)?
                .event_sequence()?,
            self.transcript_layout
                .source_position(line.entry_index, line.row_in_entry)?,
        ))
    }

    /// Return stable presentation identity at one visible index.
    #[cfg(test)]
    pub fn transcript_presentation_id_for_test(
        &self,
        index: usize,
    ) -> Option<TranscriptPresentationEntryId> {
        self.transcript.presentation_id(index)
    }

    /// Return presentation provenance at one visible index.
    #[cfg(test)]
    pub fn transcript_origin_for_test(
        &self,
        index: usize,
    ) -> Option<&TranscriptPresentationOrigin> {
        self.transcript.origin(index)
    }

    /// Detach the viewport and anchor it at one absolute transcript row.
    #[cfg(test)]
    pub fn scroll_transcript_to_row_for_test(&mut self, row: usize) {
        self.session_ui.viewport.detach();
        self.transcript_scroll_animation = None;
        self.session_ui.viewport.follow_anchor(row);
    }

    /// Expire any active scroll transition; the next frame performs normal target reconciliation.
    #[cfg(test)]
    pub(crate) fn settle_transcript_scroll_for_test(&mut self) {
        if let Some(animation) = &mut self.transcript_scroll_animation {
            animation.started_at = Instant::now()
                .checked_sub(animation.duration)
                .expect("scroll duration must fit the test clock");
        }
    }

    /// Return the resolved top row for latest-user-message anchoring.
    #[cfg(test)]
    pub fn latest_user_message_start_row_for_test(&self) -> Option<usize> {
        self.latest_user_message_start_row()
    }

    /// Return the last measured transcript viewport height for frame-level tests.
    #[cfg(test)]
    #[must_use]
    pub const fn transcript_viewport_height_for_test(&self) -> u16 {
        self.session_ui.viewport.height()
    }

    fn transcript_scroll_animation_now(&self) -> Instant {
        self.transcript_scroll_clock.now()
    }

    /// Freeze scroll-animation time until explicitly advanced by the frame harness.
    #[cfg(test)]
    pub(crate) fn freeze_transcript_scroll_animation_for_test(&mut self) {
        if matches!(
            self.transcript_scroll_clock,
            TranscriptScrollClock::Realtime
        ) {
            self.transcript_scroll_clock = TranscriptScrollClock::Fixed(Instant::now());
        }
    }

    /// Advance the frozen scroll-animation clock without sleeping or changing scroll intent.
    #[cfg(test)]
    pub(crate) fn advance_transcript_scroll_animation_for_test(&mut self, duration: Duration) {
        self.freeze_transcript_scroll_animation_for_test();
        self.transcript_scroll_clock = TranscriptScrollClock::Fixed(
            self.transcript_scroll_animation_now()
                .checked_add(duration)
                .expect("frame harness scroll-animation time must fit in Instant"),
        );
    }

    /// Return the transcript row that should render at the top of the viewport.
    #[must_use]
    pub fn transcript_top_row(&self, viewport_height: u16) -> usize {
        if let Some(animation) = self.transcript_scroll_animation {
            return animation.top_row_at(self.transcript_scroll_animation_now());
        }
        self.session_ui
            .viewport
            .top_row(self.transcript_layout.total_rows(), viewport_height)
    }

    /// Return the terminal transcript index for one interaction id.
    #[must_use]
    pub fn interaction_transcript_index(&self, interaction_id: &str) -> Option<usize> {
        self.transcript().iter().position(|item| {
            item.interaction()
                .is_some_and(|interaction| interaction.interaction_id == interaction_id)
        })
    }

    /// Restore one active interaction to the top of the transcript viewport.
    pub fn focus_interaction_in_transcript(&mut self, interaction_id: &str) -> bool {
        let Some(index) = self.interaction_transcript_index(interaction_id) else {
            return false;
        };
        let Some(top_row) = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)
        else {
            return false;
        };
        self.transcript_scroll_animation = None;
        self.session_ui.viewport.detach();
        self.session_ui.viewport.follow_anchor(top_row);
        true
    }

    /// Return global transcript row range for one terminal transcript item.
    #[must_use]
    pub fn transcript_item_row_range(&self, index: usize) -> Option<std::ops::Range<usize>> {
        let start = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)?;
        let rows = self
            .transcript_layout
            .entry_row_count(VisibleTranscriptSource::Transcript, index)?;
        Some(start..start.saturating_add(rows))
    }

    /// Return active inline interaction identity and reserved rows.
    #[must_use]
    pub fn active_interaction_layout(&self) -> Option<(&str, u16)> {
        self.active_interaction_layout
            .as_ref()
            .map(|(interaction_id, rows)| (interaction_id.as_str(), *rows))
    }

    /// Set active inline interaction layout state and invalidate only when it changes.
    pub fn set_active_interaction_layout(&mut self, active: Option<(String, u16)>) {
        if self.active_interaction_layout == active {
            return;
        }
        let previous_id = self
            .active_interaction_layout
            .as_ref()
            .map(|(interaction_id, _)| interaction_id.clone());
        let next_id = active
            .as_ref()
            .map(|(interaction_id, _)| interaction_id.clone());
        self.active_interaction_layout = active;
        for interaction_id in [previous_id, next_id].into_iter().flatten() {
            if let Some(index) = self.interaction_transcript_index(&interaction_id) {
                self.mark_transcript_item_dirty(index);
            }
        }
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

    /// Return oldest resident transcript-affecting event sequence.
    #[cfg(test)]
    pub fn resident_transcript_oldest_sequence(&self) -> Option<u64> {
        self.transcript_window.oldest_sequence()
    }

    /// Return whether older history may be available.
    #[must_use]
    pub const fn has_older_history(&self) -> bool {
        self.older_history.has_older_history()
    }

    /// Identify the current resident window across replacements of the same session.
    #[must_use]
    pub const fn history_window_generation(&self) -> u64 {
        self.history_window_generation
    }

    /// Check that a completed page still belongs to this window and boundary.
    #[must_use]
    pub const fn accepts_history_page(
        &self,
        generation: u64,
        cursor: SessionHistoryCursor,
        older: bool,
    ) -> bool {
        if generation != self.history_window_generation {
            return false;
        }
        if older {
            self.older_history.loading()
                && matches!(self.older_history.cursor(), Some(current) if current.sequence == cursor.sequence)
        } else {
            self.older_history.loading_newer()
                && matches!(self.older_history.newer_cursor(), Some(current) if current.sequence == cursor.sequence)
        }
    }

    /// Validate canonical page identity and compatibility before artifact hydration.
    #[must_use]
    pub fn accepts_history_page_payload(
        &self,
        cursor: SessionHistoryCursor,
        older: bool,
        page: &bcode_session_models::SessionHistoryPage,
    ) -> bool {
        Some(page.session_id) == self.session_id
            && page.compatibility_issues.is_empty()
            && (!page.has_more || !page.events.is_empty())
            && page.next_cursor.is_none_or(|next| {
                page.has_more
                    && if older {
                        page.events
                            .first()
                            .is_some_and(|first| next.sequence < first.sequence)
                    } else {
                        page.events
                            .last()
                            .is_some_and(|last| next.sequence > last.sequence)
                    }
            })
            && self.accepts_history_page_events(cursor, older, &page.events, page.has_more)
    }

    /// Validate a fetched canonical page before changing resident history or artifact state.
    /// History queries include their cursor, while resident windows may omit events that
    /// do not contribute transcript rows; adjacency of sequence numbers is not assumed.
    #[must_use]
    pub fn accepts_history_page_events(
        &self,
        cursor: SessionHistoryCursor,
        older: bool,
        events: &[SessionEvent],
        has_more: bool,
    ) -> bool {
        if events.is_empty() {
            return !has_more;
        }
        let valid = events
            .iter()
            .all(|event| Some(event.session_id) == self.session_id)
            && events
                .windows(2)
                .all(|pair| pair[0].sequence < pair[1].sequence);
        if !valid {
            return false;
        }
        if older {
            events
                .last()
                .is_some_and(|last| last.sequence <= cursor.sequence)
                && self.transcript_window.events().first().is_none_or(|first| {
                    events
                        .last()
                        .is_some_and(|last| last.sequence < first.sequence)
                })
        } else {
            events
                .first()
                .is_some_and(|first| first.sequence >= cursor.sequence)
                && self.transcript_window.events().last().is_none_or(|last| {
                    events
                        .first()
                        .is_some_and(|first| first.sequence > last.sequence)
                })
        }
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub const fn loading_older_history(&self) -> bool {
        self.older_history.loading()
    }

    /// Mark older history as loading or idle.
    pub fn set_loading_older_history(&mut self, loading: bool) {
        self.restore_uncommitted_navigation();
        self.older_history.set_loading(loading);
    }

    /// Return the cursor for loading older history.
    #[must_use]
    pub const fn older_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history.cursor()
    }

    /// Return whether an older-history request should be started.
    #[must_use]
    pub const fn should_load_older_history(&self) -> bool {
        self.older_history.should_load()
    }

    /// Request loading older transcript history for tests and selection-owned viewport flows.
    #[cfg(test)]
    pub(crate) const fn request_older_history_for_test(&mut self, reveal_rows: usize) {
        self.older_history.request_load(reveal_rows);
    }

    /// Mark newer history as loading or idle.
    pub fn set_loading_newer_history(&mut self, loading: bool) {
        self.restore_uncommitted_navigation();
        self.older_history.set_loading_newer(loading);
    }

    /// Return the cursor for loading newer history.
    #[must_use]
    pub const fn newer_history_cursor(&self) -> Option<SessionHistoryCursor> {
        self.older_history.newer_cursor()
    }

    /// Return whether a newer-history request should be started.
    #[must_use]
    pub const fn should_load_newer_history(&self) -> bool {
        self.older_history.should_load_newer()
    }

    /// Request loading newer transcript history for tests and selection-owned viewport flows.
    #[cfg(test)]
    pub(crate) const fn request_newer_history_for_test(&mut self, reveal_rows: usize) {
        self.older_history.request_load_newer(reveal_rows);
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
        self.apply_session_view_terminal_adapter();
    }

    /// Return active plugin-owned session status contributions.
    pub fn plugin_status(
        &self,
    ) -> impl Iterator<Item = &bcode_session_view_models::PluginStatusView> {
        self.session_view.snapshot().plugin_status.values()
    }

    /// Resolve a plugin-owned inspection surface from a current activity entry.
    pub fn activity_inspection(&self, index: usize) -> Option<(String, String, serde_json::Value)> {
        let source = self.transcript.plugin_activity_source(index)?;
        let status = self.plugin_status().find(|status| {
            source
                == format!(
                    "bcode.plugin-activity:{}:{}",
                    status.plugin_id, status.note_id
                )
        })?;
        let surface = status.metadata.get("inspection_surface")?.as_str()?;
        let run = status.metadata.get("run_id")?.as_str()?;
        Some((
            status.plugin_id.clone(),
            surface.to_owned(),
            serde_json::json!({"run_id": run}),
        ))
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
        if self.transcript.set_plugin_activity(&activity) {
            self.transcript_projection_revision =
                self.transcript_projection_revision.saturating_add(1);
        }
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
        self.transcript.push_ephemeral(
            "bcode.tui".to_owned(),
            TranscriptItem::with_format("System", text, format),
        );
        self.transcript_projection_revision = self.transcript_projection_revision.saturating_add(1);
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
        self.recompile_structure();
        self.apply_session_view_terminal_adapter();
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
            self.apply_session_view_terminal_adapter();
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
        self.restore_uncommitted_navigation();
        // Freeze the displayed top before clearing the composer changes body height.
        // This is an automatic hold, not a manual detach; acceptance owns its reveal.
        if self.session_ui.viewport.allows_reveal() {
            self.session_ui.viewport.reveal(true);
        }
        let text = self.session_ui.composer.buffer().text().to_owned();
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_submissions.stage(text);
        self.input_history.reset_navigation();
        self.session_ui.composer.buffer_mut().clear();
    }

    /// Return the currently pending submission.
    pub fn take_pending_submission(&mut self) -> String {
        self.pending_submissions.take_staged()
    }

    /// Remove a pending submission that was handled outside the session transcript.
    pub fn clear_pending_submission(&mut self, text: &str) {
        self.pending_submissions.clear_staged_if(text);
        self.remove_pending_submission(text);
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.session_ui
            .viewport
            .scroll_to_bottom(&mut self.older_history);
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
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
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.session_ui
            .viewport
            .scroll_to_bottom(&mut self.older_history);
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.session_ui.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub fn previous_input_history(&mut self) -> bool {
        match self
            .input_history
            .previous(self.session_ui.composer.buffer().text())
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

    /// Scroll transcript up by rendered rows.
    pub fn scroll_transcript_up(&mut self, rows: usize) -> bool {
        self.navigation_checkpoint = None;
        self.cancel_transcript_scroll_animation_for_manual_scroll();
        self.mark_manual_transcript_scroll();
        self.session_ui
            .viewport
            .scroll_up(rows, &mut self.older_history)
    }

    /// Scroll transcript down by rendered rows.
    pub fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        self.navigation_checkpoint = None;
        self.cancel_transcript_scroll_animation_for_manual_scroll();
        self.mark_manual_transcript_scroll();
        let changed = self
            .session_ui
            .viewport
            .scroll_down(rows, &mut self.older_history);
        if self.session_ui.viewport.at_bottom_threshold() {
            self.session_ui
                .viewport
                .scroll_to_bottom(&mut self.older_history);
            self.latest_hidden_activity_at = None;
            self.latest_hidden_activity_burst = 0;
            self.latest_bar_next_frame_at = None;
        } else {
            self.session_ui.viewport.detach();
        }
        changed
    }

    /// Pin transcript to the newest rows.
    pub fn scroll_transcript_to_bottom(&mut self) -> bool {
        self.restore_uncommitted_navigation();
        self.presented_transcript_anchor = None;
        self.pending_stable_transcript_anchor = None;
        self.transcript_scroll_animation = None;
        self.manual_transcript_scroll_until = None;
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.latest_hidden_activity_at = None;
        self.latest_hidden_activity_burst = 0;
        self.latest_bar_next_frame_at = None;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        self.pending_transcript_top_anchor_sequence = None;
        self.pending_submissions.cancel_reveals();
        self.session_ui
            .viewport
            .scroll_to_bottom(&mut self.older_history)
    }

    /// Animate transcript to the newest rows.
    pub fn transition_transcript_to_bottom(&mut self) -> bool {
        self.pending_transcript_top_anchor_sequence = None;
        self.pending_submissions.cancel_reveals();
        self.navigation_checkpoint = None;
        let start_top_row = self.session_ui.viewport.top_row(
            self.transcript_layout.total_rows(),
            self.session_ui.viewport.height(),
        );
        self.presented_transcript_anchor = None;
        self.pending_stable_transcript_anchor = None;
        self.manual_transcript_scroll_until = None;
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.session_ui
            .viewport
            .scroll_to_bottom(&mut self.older_history);
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        let total_rows = self.transcript_layout.total_rows();
        let viewport_height = self.session_ui.viewport.height();
        let target_top_row = total_rows.saturating_sub(usize::from(viewport_height));
        if start_top_row == target_top_row {
            return self.scroll_transcript_to_bottom();
        }
        let duration = self.motion_policy().transcript_scroll();
        if duration.is_zero() {
            // Instant motion jumps; the top row did move.
            self.scroll_transcript_to_bottom();
            return true;
        }
        let mut animation = TranscriptScrollAnimation::new(
            start_top_row,
            target_top_row,
            self.transcript_scroll_animation_now(),
            duration,
        );
        animation.target = TranscriptAnimationTarget::LatestBottom;
        self.transcript_scroll_animation = Some(animation);
        true
    }

    fn cancel_transcript_scroll_animation_for_manual_scroll(&mut self) {
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        self.interrupt_current_assistant_anchor();
        let Some(animation) = self.transcript_scroll_animation.take() else {
            return;
        };
        let top_row = animation.top_row_at(self.transcript_scroll_animation_now());
        self.session_ui.viewport.materialize_top_row(top_row);
    }

    fn mark_manual_transcript_scroll(&mut self) {
        self.navigation_checkpoint = None;
        // A pending correspondence belongs to the previous navigation intent.
        // It must not undo input received between semantic updates and preparation.
        self.pending_stable_transcript_anchor = None;
        self.presented_transcript_anchor = None;
        self.pending_transcript_top_anchor_sequence = None;
        self.pending_submissions.cancel_reveals();
        self.manual_transcript_scroll_until = Some(Instant::now() + MANUAL_TRANSCRIPT_SCROLL_GRACE);
    }

    #[cfg(test)]
    pub const fn expire_manual_transcript_scroll_for_test(&mut self) {
        self.manual_transcript_scroll_until = None;
    }

    fn manual_transcript_scroll_active(&self) -> bool {
        self.manual_transcript_scroll_until
            .is_some_and(|until| Instant::now() < until)
    }

    /// Capture the geometry boundary once per preparation, irrespective of event kind.
    pub(crate) fn begin_transcript_geometry_update(&mut self) {
        self.pending_visual_overflow_bottom = Some(
            self.session_ui
                .viewport
                .bottom_row(self.transcript_layout.total_rows()),
        );
    }

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub fn sync_transcript_scroll_max(
        &mut self,
        max_scroll_offset: usize,
        max_bottom_overscroll: usize,
        total_rows: usize,
        viewport_height: u16,
    ) {
        let now = Instant::now();
        let mut animated_top = None;
        if let Some(mut animation) = self.transcript_scroll_animation {
            let animation_now = self.transcript_scroll_animation_now();
            if let TranscriptAnimationTarget::ItemTop(id) = animation.target
                && let Some(index) = self.transcript.presentation_index(id)
                && let Some(row) = self
                    .transcript_layout
                    .entry_start_row(VisibleTranscriptSource::Transcript, index)
            {
                animation.target_top_row = row;
            } else if animation.target == TranscriptAnimationTarget::LatestBottom {
                animation.target_top_row = total_rows.saturating_sub(usize::from(viewport_height));
            }
            let top_row = animation.top_row_at(animation_now);
            if animation.finished(animation_now) {
                self.transcript_scroll_animation = None;
                let follows_tail = animation.target == TranscriptAnimationTarget::LatestBottom;
                if follows_tail {
                    self.session_ui
                        .viewport
                        .scroll_to_bottom(&mut self.older_history);
                } else {
                    animated_top = Some(animation.target_top_row);
                }
            } else {
                animated_top = Some(top_row);
                self.transcript_scroll_animation = Some(animation);
            }
        }
        let newer_page_rows = self.newer_history_page_rows();
        let older_page_rows = self.older_history_page_rows();
        let anchor = self.resolve_pending_transcript_anchor();
        self.pending_stable_transcript_anchor = None;
        self.session_ui.viewport.sync_with_anchor(
            (
                max_scroll_offset,
                max_bottom_overscroll,
                total_rows,
                viewport_height,
            ),
            anchor,
            older_page_rows,
            newer_page_rows,
            &mut self.older_history,
        );
        if let Some(top_row) = animated_top {
            self.session_ui.viewport.materialize_top_row(top_row);
        }
        self.resolve_visual_overflow_follow(total_rows, now);
    }

    fn older_history_page_rows(&self) -> usize {
        let Some((first, last)) = self.older_history.ready_older_source_range() else {
            return 0;
        };
        self.transcript
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.event_sequence()
                    .is_some_and(|sequence| first <= sequence && sequence <= last)
            })
            .filter_map(|(index, _)| {
                self.transcript_layout
                    .entry_row_count(VisibleTranscriptSource::Transcript, index)
            })
            .sum()
    }

    fn newer_history_page_rows(&self) -> usize {
        let Some((first, last)) = self.older_history.ready_newer_source_range() else {
            return 0;
        };
        // The newest source item can be updated in place by events in a page.
        // Only entries whose source identity starts inside the accepted page
        // are new navigable rows; existing entries may have reflowed.
        self.transcript
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                item.event_sequence()
                    .is_some_and(|sequence| first <= sequence && sequence <= last)
            })
            .filter_map(|(index, _)| {
                self.transcript_layout
                    .entry_row_count(VisibleTranscriptSource::Transcript, index)
            })
            .sum()
    }

    fn resolve_visual_overflow_follow(&mut self, _total_rows: usize, now: Instant) {
        let Some(previous_bottom) = self.pending_visual_overflow_bottom.take() else {
            if !self.newer_transcript_content_below() {
                self.latest_hidden_activity_at = None;
                self.latest_hidden_activity_burst = 0;
                self.latest_bar_next_frame_at = None;
            }
            return;
        };
        if self
            .transcript_layout
            .changed_content_below(previous_bottom)
        {
            self.record_latest_hidden_activity(now, 1);
        }
        let allowed = self.submitted_user_message_following
            != SubmittedUserMessageFollowing::PendingAnchor
            && !self.manual_transcript_scroll_active()
            && self.transcript_scroll_animation.is_none()
            && self.session_ui.viewport.allows_overflow();
        if self.session_ui.viewport.reconcile_overflow(
            self.session_ui
                .viewport
                .bottom_row(self.transcript_layout.total_rows()),
            allowed,
            &mut self.older_history,
        ) {
            self.session_ui
                .viewport
                .scroll_to_bottom(&mut self.older_history);
        }
        if !self.newer_transcript_content_below() {
            self.latest_hidden_activity_at = None;
            self.latest_hidden_activity_burst = 0;
            self.latest_bar_next_frame_at = None;
        }
    }

    fn record_latest_hidden_activity(&mut self, now: Instant, changed_rows: usize) {
        let window = self.motion_policy().latest_bar_window();
        let previous_activity_at = self.latest_hidden_activity_at;
        if previous_activity_at.is_none_or(|at| now.saturating_duration_since(at) >= window) {
            self.latest_hidden_activity_burst = 0;
        }
        let elapsed_ms = previous_activity_at
            .map_or_else(
                || window.as_millis(),
                |at| now.saturating_duration_since(at).as_millis(),
            )
            .max(1);
        self.latest_hidden_activity_at = Some(now);
        self.latest_bar_animation_started_at = now;
        // Instant motion shows the still bar: no glow frames are scheduled.
        self.latest_bar_next_frame_at = (!window.is_zero()).then(|| {
            let frame_duration =
                latest_bar_active_frame_duration(self.latest_hidden_activity_burst);
            next_latest_bar_frame_at(now, now, frame_duration)
        });
        let velocity_rows_per_second = u128::try_from(changed_rows)
            .unwrap_or(u128::MAX)
            .saturating_mul(1_000)
            / elapsed_ms;
        let row_score = u8::try_from(changed_rows.min(8)).unwrap_or(8);
        let velocity_score = u8::try_from(velocity_rows_per_second.min(8)).unwrap_or(8);
        let activity = row_score.max(velocity_score).max(1);
        self.latest_hidden_activity_burst = self
            .latest_hidden_activity_burst
            .saturating_add(activity)
            .min(8);
    }

    /// Resolve deferred user-message and live-stream top anchoring against the latest cached layout.
    pub fn sync_transcript_anchor_requests(&mut self) {
        let resident = self
            .transcript
            .iter()
            .map(super::transcript::TranscriptItem::id)
            .collect::<BTreeSet<_>>();
        self.admitted_items.retain(|id| resident.contains(id));
        let newest = self
            .transcript
            .iter()
            .enumerate()
            .filter(|(_, item)| item.role() != "Assistant" || !item.text().is_empty())
            .filter(|(_, item)| !self.admitted_items.contains(&item.id()))
            .map(|(index, item)| (index, item.id(), item.role() == "Assistant"))
            .next_back();
        if self.automatic_item.is_none() || !self.session_ui.viewport.allows_reveal() {
            self.admitted_items.extend(
                self.transcript
                    .iter()
                    .filter(|item| item.role() != "Assistant" || !item.text().is_empty())
                    .map(super::transcript::TranscriptItem::id),
            );
        }
        if let Some((index, id, assistant)) = newest {
            let changed = self.automatic_item.is_some();
            if self.automatic_item.is_none() || !self.session_ui.viewport.allows_reveal() {
                self.automatic_item = Some(id);
            }
            if changed
                && self.session_ui.viewport.allows_reveal()
                && !self.manual_transcript_scroll_active()
                && self.submitted_user_message_following
                    != SubmittedUserMessageFollowing::PendingAnchor
            {
                self.pending_transcript_top_anchor_sequence = None;
                self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
                self.pending_assistant_stream_anchor = false;
                self.admitted_items.extend(
                    self.transcript
                        .iter()
                        .take(index + 1)
                        .filter(|item| item.role() != "Assistant" || !item.text().is_empty())
                        .map(super::transcript::TranscriptItem::id),
                );
                if assistant {
                    if let Some(top) = self
                        .transcript_layout
                        .entry_start_row(VisibleTranscriptSource::Transcript, index)
                    {
                        self.reveal_assistant_item(index, top);
                    }
                } else {
                    self.automatic_item = Some(id);
                    self.release_item_hold_for_overflow();
                }
                return;
            }
        }
        self.sync_submission_anchor_requests();
    }

    fn sync_submission_anchor_requests(&mut self) {
        if self.manual_transcript_scroll_active() || self.transcript_scroll_animation.is_some() {
            return;
        }
        if let Some(sequence) = self.pending_transcript_top_anchor_sequence {
            if let Some(index) = self.transcript_index_for_sequence(sequence)
                && let Some(top_row) = self
                    .transcript_layout
                    .entry_start_row(VisibleTranscriptSource::Transcript, index)
            {
                self.pending_transcript_top_anchor_sequence = None;
                self.transcript_scroll_animation = None;
                if self.submitted_user_message_following
                    == SubmittedUserMessageFollowing::PendingAnchor
                {
                    self.submitted_user_message_following = SubmittedUserMessageFollowing::Anchored;
                    self.session_ui.viewport.reveal(false);
                    self.start_transcript_scroll_animation(top_row);
                    self.set_animation_item_target(index);
                    self.admitted_items.extend(
                        self.transcript
                            .iter()
                            .take(index + 1)
                            .map(super::transcript::TranscriptItem::id),
                    );
                    self.automatic_item = self
                        .transcript
                        .get(index)
                        .map(super::transcript::TranscriptItem::id);
                } else {
                    self.session_ui.viewport.detach();
                    self.session_ui.viewport.follow_anchor(top_row);
                }
                self.presented_transcript_anchor = None;
                self.pending_stable_transcript_anchor = None;
            }
            return;
        }
        if self.submitted_user_message_following == SubmittedUserMessageFollowing::PendingAnchor
            && let Some(top_row) = self.latest_user_message_start_row()
        {
            self.submitted_user_message_following = SubmittedUserMessageFollowing::Anchored;
            self.session_ui.viewport.reveal(false);
            self.start_transcript_scroll_animation(top_row);
        }
    }

    fn set_animation_item_target(&mut self, index: usize) {
        if let Some(animation) = &mut self.transcript_scroll_animation {
            animation.target = self.transcript.presentation_id(index).map_or(
                TranscriptAnimationTarget::Row,
                TranscriptAnimationTarget::ItemTop,
            );
        }
    }

    /// Enable overflow following without consuming the space below a short item.
    fn release_item_hold_for_overflow(&mut self) {
        self.session_ui.viewport.release_sticky_reveal();
        let total = self.transcript_layout.total_rows();
        if self.transcript_scroll_animation.is_none()
            && total > self.session_ui.viewport.bottom_row(total)
        {
            let checkpoint = self.navigation_checkpoint.take();
            self.transition_transcript_to_bottom();
            self.navigation_checkpoint = checkpoint;
        }
    }

    /// Install one assistant hold, superseding every earlier automatic reveal.
    fn reveal_assistant_item(&mut self, index: usize, top_row: usize) {
        self.pending_transcript_top_anchor_sequence = None;
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.pending_assistant_stream_anchor = false;
        self.automatic_item = self
            .transcript
            .get(index)
            .map(super::transcript::TranscriptItem::id);
        self.transcript_scroll_animation = None;
        self.start_transcript_scroll_animation(top_row);
        // Use the semantic target, never reverse-map animated/clamped coordinates.
        if let Some(animation) = &mut self.transcript_scroll_animation {
            animation.target = self.transcript.presentation_id(index).map_or(
                TranscriptAnimationTarget::Row,
                TranscriptAnimationTarget::ItemTop,
            );
        }
        self.session_ui.viewport.reveal(true);
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Anchored { index };
    }

    const fn downgrade_sticky_entry_anchor(&mut self) {
        self.session_ui.viewport.release_sticky_reveal();
    }

    fn interrupt_current_assistant_anchor(&mut self) {
        if let Some(index) = self.assistant_scroll_anchor.index()
            && self
                .transcript
                .get(index)
                .is_some_and(|item| item.role() == "Assistant" && item.streaming())
        {
            self.assistant_scroll_anchor = AssistantScrollAnchorState::Interrupted { index };
        } else {
            self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        }
    }

    fn finish_assistant_stream_anchor(&mut self) {
        if let Some(index) = self.assistant_scroll_anchor.index()
            && self
                .transcript
                .get(index)
                .is_some_and(|item| item.role() == "Assistant" && !item.streaming())
        {
            self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        }
    }

    #[cfg(test)]
    pub const fn has_assistant_stream_anchor(&self) -> bool {
        self.assistant_scroll_anchor.index().is_some()
    }

    #[cfg(test)]
    pub const fn assistant_stream_anchor_index(&self) -> Option<usize> {
        self.assistant_scroll_anchor.index()
    }

    #[cfg(test)]
    pub const fn manually_detached(&self) -> bool {
        !self.session_ui.viewport.allows_reveal()
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

    fn start_transcript_scroll_animation(&mut self, top_row: usize) {
        self.presented_transcript_anchor = None;
        self.pending_stable_transcript_anchor = None;
        if let Some((start_top_row, target_top_row)) = self
            .session_ui
            .viewport
            .start_follow_anchor_animation(top_row)
        {
            let duration = self.motion_policy().transcript_scroll();
            if duration.is_zero() {
                // Instant motion lands where a finished animation would.
                self.session_ui.viewport.materialize_top_row(target_top_row);
                return;
            }
            let mut animation = TranscriptScrollAnimation::new(
                start_top_row,
                target_top_row,
                self.transcript_scroll_animation_now(),
                duration,
            );
            animation.target = self
                .transcript_layout
                .line_at_row(target_top_row)
                .filter(|line| line.source == VisibleTranscriptSource::Transcript)
                .and_then(|line| self.transcript.presentation_id(line.entry_index))
                .map_or(
                    TranscriptAnimationTarget::Row,
                    TranscriptAnimationTarget::ItemTop,
                );
            self.transcript_scroll_animation = Some(animation);
        }
    }

    fn latest_user_message_start_row(&self) -> Option<usize> {
        if !self.pending_submissions().is_empty() {
            return self.transcript_layout.entry_start_row(
                VisibleTranscriptSource::Pending,
                self.pending_submissions().len().saturating_sub(1),
            );
        }
        let index = self
            .transcript
            .iter()
            .rposition(|item| item.role() == "You")?;
        self.transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)
    }

    /// Absorb replayed history events.
    pub fn absorb_history(&mut self, events: &[SessionEvent]) {
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.append_history(events);
        for event in events {
            self.session_view.apply_event(event);
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.apply_session_view_terminal_adapter();
        self.trim_resident_transcript_window_if_needed();
    }

    fn rebuild_transcript_from_history(&mut self, preserve_live: bool) {
        let events = self.transcript_window.events().to_vec();
        if preserve_live {
            self.session_view.rebuild_history_window(&events);
        } else {
            self.session_view.clear_history_window();
            self.session_view.apply_history(&events);
        }
        self.session_view_terminal_adapter.reset();
        for event in &events {
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.apply_session_view_terminal_adapter();
    }

    /// Drop complete older turns only when all visible sources and their
    /// preceding scrolling buffer survive the cut.
    fn trim_offscreen_older_transcript_if_needed(&mut self) {
        if self.transcript_window.events().len() <= RESIDENT_TRANSCRIPT_MAX_EVENTS
            || self.transcript_selection_pinned
            || self.transcript_scroll_animation.is_some()
            || self.older_history.loading()
            || self.session_ui.viewport.follows_bottom()
        {
            return;
        }
        let top = self.session_ui.viewport.top_row(
            self.transcript_layout.total_rows(),
            self.session_ui.viewport.height(),
        );
        let Some(first_visible_sequence) = self
            .transcript_layout
            .visible_lines_from_top(top, self.session_ui.viewport.height().max(1))
            .iter()
            .filter(|line| line.source == VisibleTranscriptSource::Transcript)
            .filter_map(|line| self.transcript.get(line.entry_index))
            .filter_map(super::transcript::TranscriptItem::event_sequence)
            .min()
        else {
            return;
        };
        if let Some(new_oldest_sequence) = self.transcript_window.trim_older_offscreen(
            RESIDENT_TRANSCRIPT_MAX_EVENTS,
            RESIDENT_TRANSCRIPT_TARGET_EVENTS,
            first_visible_sequence,
            usize::from(self.session_ui.viewport.height()).saturating_mul(4),
        ) {
            self.capture_stable_transcript_anchor();
            self.older_history
                .mark_dropped_history_before(new_oldest_sequence);
            self.rebuild_transcript_from_history(true);
            self.pending_visual_overflow_bottom = None;
        }
    }

    /// Drop complete off-screen newer turns after an older page has been accepted.
    /// The retained reader identity is resolved again at the next presentation.
    fn trim_offscreen_newer_transcript_if_needed(&mut self) {
        if self.transcript_window.events().len() <= RESIDENT_TRANSCRIPT_MAX_EVENTS
            || self.transcript_selection_pinned
            || self.transcript_scroll_animation.is_some()
            || self.older_history.loading_newer()
            || self.session_ui.viewport.follows_bottom()
        {
            return;
        }
        let top = self.session_ui.viewport.top_row(
            self.transcript_layout.total_rows(),
            self.session_ui.viewport.height(),
        );
        let visible = self
            .transcript_layout
            .visible_lines_from_top(top, self.session_ui.viewport.height().max(1));
        let Some(last_visible_sequence) = visible
            .iter()
            .filter(|line| line.source == VisibleTranscriptSource::Transcript)
            .filter_map(|line| self.transcript.get(line.entry_index))
            .filter_map(super::transcript::TranscriptItem::event_sequence)
            .max()
        else {
            return;
        };
        // The last visible source must survive even when a single event wraps
        // to many rows. Keep additional complete events for ordinary scrolling.
        let first_dropped = self.transcript_window.trim_newer_offscreen(
            RESIDENT_TRANSCRIPT_MAX_EVENTS,
            RESIDENT_TRANSCRIPT_TARGET_EVENTS,
            last_visible_sequence,
            usize::from(self.session_ui.viewport.height()).saturating_mul(4),
        );
        if let Some(first_dropped) = first_dropped {
            self.capture_stable_transcript_anchor();
            self.older_history.mark_dropped_history_after(first_dropped);
            self.rebuild_transcript_from_history(true);
            self.pending_visual_overflow_bottom = None;
        }
    }

    fn trim_resident_transcript_window_if_needed(&mut self) {
        let trim = self
            .transcript_window
            .trim_if_allowed(self.resident_transcript_window_policy());
        if !trim.trimmed() {
            return;
        }
        if let Some(new_oldest_sequence) = trim.new_oldest_sequence {
            self.older_history
                .mark_dropped_history_before(new_oldest_sequence);
        }
        self.rebuild_transcript_from_history(true);
        self.session_ui
            .viewport
            .scroll_to_bottom(&mut self.older_history);
        self.pending_visual_overflow_bottom = None;
    }

    fn resident_transcript_window_policy(&self) -> TranscriptWindowPolicy {
        TranscriptWindowPolicy {
            max_events: RESIDENT_TRANSCRIPT_MAX_EVENTS,
            target_events: RESIDENT_TRANSCRIPT_TARGET_EVENTS,
            allow_trim: self.can_trim_resident_transcript_window()
                && !self.older_history.has_newer_history(),
        }
    }

    /// Return whether resident transcript trimming is currently permitted.
    #[cfg(test)]
    #[must_use]
    pub fn can_trim_resident_transcript_window_for_test(&self) -> bool {
        self.can_trim_resident_transcript_window()
    }

    fn can_trim_resident_transcript_window(&self) -> bool {
        self.session_ui.viewport.follows_bottom()
            && self.session_ui.viewport.at_bottom_threshold()
            && self.transcript_scroll_animation.is_none()
            && !self.manual_transcript_scroll_active()
            && !self.transcript_selection_pinned
            && !self.active_tool_loop()
            && !self.pending_assistant_stream_anchor
            && !self.older_history.loading()
    }

    /// Prepend older history and preserve the current viewport.
    pub fn prepend_older_history(&mut self, events: &[SessionEvent], has_more: bool) {
        self.restore_uncommitted_navigation();
        if events.is_empty() {
            self.older_history.update_cursor(&[], false);
            self.older_history.set_loading(false);
            "start of session history".clone_into(&mut self.status);
            return;
        }

        let input_messages = events.iter().filter_map(|event| match &event.kind {
            SessionEventKind::UserMessage { text, .. } => Some(SessionInputHistoryEntry {
                sequence: event.sequence,
                timestamp_ms: event.timestamp_ms,
                text: text.clone(),
            }),
            _ => None,
        });
        self.input_history.prepend_committed(input_messages);

        self.transcript_window.prepend_older_history(events);
        if self.latest_history_sequence.is_none() {
            self.latest_history_sequence = events.last().map(|event| event.sequence);
        }
        self.rebuild_transcript_from_history(true);
        self.older_history.update_cursor(events, has_more);
        self.older_history.set_loading(false);
        self.trim_offscreen_newer_transcript_if_needed();
        if self.older_history.has_older_history() {
            "loaded older history".clone_into(&mut self.status);
        } else {
            "start of session history".clone_into(&mut self.status);
        }
    }

    /// Append newer history and preserve bounded window state.
    pub fn append_newer_history(&mut self, events: &[SessionEvent], has_more: bool) {
        self.restore_uncommitted_navigation();
        if events.is_empty() {
            self.older_history.update_newer_cursor(&[], false);
            self.older_history.set_loading_newer(false);
            "latest session history".clone_into(&mut self.status);
            return;
        }

        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.append_history(events);
        self.rebuild_transcript_from_history(true);
        self.older_history.update_newer_cursor(events, has_more);
        self.older_history.set_loading_newer(false);
        self.trim_offscreen_older_transcript_if_needed();
        if self.older_history.has_newer_history() {
            "loaded newer history".clone_into(&mut self.status);
        } else {
            "latest session history".clone_into(&mut self.status);
        }
        self.trim_resident_transcript_window_if_needed();
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
        self.restore_uncommitted_navigation();
        if event_affects_transcript_rows(event)
            && self
                .latest_history_sequence
                .is_some_and(|sequence| event.sequence <= sequence)
        {
            return;
        }
        if event_affects_transcript_rows(event) && self.older_history.has_newer_history() {
            self.older_history
                .mark_newer_available_after(self.transcript_window.newest_sequence());
            self.latest_history_sequence = Some(event.sequence);
            self.set_status("new activity below".to_owned());
            return;
        }
        if event_affects_transcript_rows(event) {
            self.transcript_window.append_live_event(event);
        }
        self.session_view.apply_event(event);
        self.apply_session_event(event, SessionEventApplication::Live);
        self.apply_session_view_terminal_adapter();
        self.trim_resident_transcript_window_if_needed();
    }

    /// Absorb one live-only session event.
    pub fn absorb_session_live_event(&mut self, event: &SessionLiveEvent) {
        self.restore_uncommitted_navigation();
        self.session_view.apply_live_event(event);
        self.apply_session_view_terminal_adapter();
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

        if event_breaks_sticky_entry_anchor(event) {
            self.downgrade_sticky_entry_anchor();
        }
        match &event.kind {
            SessionEventKind::ExternalHarnessOperation { .. }
            | SessionEventKind::ExternalBackendBound { .. }
            | SessionEventKind::ExternalSubmissionAccepted { .. }
            | SessionEventKind::ExternalDeliveryObserved { .. }
            | SessionEventKind::ExternalObservationRecorded { .. } => {
                // Native history identifies the backend, not execution authority. Default Send
                // explicitly queues a follow-up; steering requires a user-selected native turn.
                if self
                    .session_view_snapshot()
                    .execution_lifecycle
                    .session_id
                    .is_some()
                {
                    self.set_status(self.session_view_snapshot().execution_lifecycle.summary());
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
                self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
                self.pending_assistant_stream_anchor = false;
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
                self.finish_assistant_stream_anchor();
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
                if application.live_activity() {
                    self.set_activity(ActivityState::PreparingFollowUpRequest);
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
                    self.set_activity(ActivityState::PreparingFollowUpRequest);
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
    pub fn invalidation_requests(
        &self,
        now: Instant,
        now_system: SystemTime,
    ) -> Vec<InvalidationRequest> {
        let mut requests = vec![self.cursor.invalidation_request()];
        if let Some(animation) = self.transcript_scroll_animation {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY),
                animation.next_frame_at,
            ));
        }
        if self.newer_transcript_content_below()
            && let Some(next_frame_at) = self.latest_bar_next_frame_at
        {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(LATEST_BAR_ANIMATION_INVALIDATION_KEY),
                next_frame_at,
            ));
        }
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
        requests.extend(self.tool_elapsed_invalidation_requests(now, now_system));
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
            } else if is_transcript_scroll_animation_invalidation(key) {
                if self.handle_transcript_scroll_animation(now) {
                    damage.insert(TemporalDamage::Transcript);
                    invalidation.merge(UiInvalidation::Paint)
                } else {
                    invalidation
                }
            } else if is_latest_bar_animation_invalidation(key) {
                let active = self.latest_bar_active(now);
                self.latest_bar_next_frame_at = active.then(|| {
                    next_latest_bar_frame_at(
                        self.latest_bar_animation_started_at,
                        now,
                        latest_bar_active_frame_duration(self.latest_hidden_activity_burst),
                    )
                });
                damage.insert(TemporalDamage::LatestBar);
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
            } else if let Some(invocation_id) = tool_elapsed_invalidation_invocation_id(key) {
                self.elapsed_layout_revision = self.elapsed_layout_revision.saturating_add(1);
                self.elapsed_dirty_visuals.insert(invocation_id.to_owned());
                damage.insert(TemporalDamage::Transcript);
                invalidation.merge(UiInvalidation::Paint)
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

    fn latest_bar_active(&self, now: Instant) -> bool {
        let window = self.motion_policy().latest_bar_window();
        self.latest_hidden_activity_at
            .is_some_and(|at| now.saturating_duration_since(at) < window)
    }

    fn handle_transcript_scroll_animation(&mut self, now: Instant) -> bool {
        let Some(mut animation) = self.transcript_scroll_animation else {
            return false;
        };
        // Deadlines request presentation; geometry reconciliation alone resolves
        // identity-based targets and terminal animation state.
        animation.advance_frame(now);
        self.transcript_scroll_animation = Some(animation);
        true
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
        self.session_ui.composer.buffer_mut().clear();
        self.session_ui.composer.buffer_mut().insert_str(text);
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

    fn visible_transcript_entry_indexes(&self) -> BTreeSet<usize> {
        let height = self.session_ui.viewport.height();
        if height == 0 {
            return (0..self.transcript().len()).collect();
        }
        self.transcript_layout
            .visible_transcript_entry_indexes(self.transcript_top_row(height), height)
    }

    fn tool_elapsed_invalidation_requests(
        &self,
        now: Instant,
        now_system: SystemTime,
    ) -> impl Iterator<Item = InvalidationRequest> {
        let visible = self.visible_transcript_entry_indexes();
        self.transcript
            .iter()
            .enumerate()
            .filter_map(move |(index, item)| {
                if !visible.contains(&index) || !item.tool_is_active() {
                    return None;
                }
                let invocation_id = item.visual_invocation_id()?;
                let timing = item.tool_timing()?;
                let at = super::temporal::next_elapsed_invalidation_capped(
                    timing.started_at_ms?,
                    None,
                    now,
                    now_system,
                    TOOL_ELAPSED_INVALIDATION_MAX_INTERVAL,
                )?;
                Some(InvalidationRequest::new(
                    InvalidationKey::new(format!(
                        "{TOOL_ELAPSED_INVALIDATION_PREFIX}:{invocation_id}"
                    )),
                    at,
                ))
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn extend_composer_selection_to_visual_delta(&mut self, width: usize, delta: isize) {
        let layout = self.session_ui.composer.buffer().wrapped_layout(width);
        let target_row = if delta.is_negative() {
            layout.cursor.row.saturating_sub(delta.unsigned_abs())
        } else {
            layout
                .cursor
                .row
                .saturating_add(delta.unsigned_abs())
                .min(layout.lines.len().saturating_sub(1))
        };
        self.session_ui
            .composer
            .buffer_mut()
            .select_to_wrapped_position(width, target_row, layout.cursor.col);
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

    fn apply_committed_user_message(
        &mut self,
        sequence: u64,
        text: &str,
        timestamp_ms: u64,
        application: SessionEventApplication,
    ) {
        let text = self
            .shared_terminal_text_for_sequence(sequence, "You")
            .unwrap_or_else(|| text.to_owned());
        self.input_history
            .push_committed(sequence, timestamp_ms, &text);
        let accepted_pending_submission = self.pending_submissions.reveals_on_acceptance(&text);
        self.remove_pending_submission(&text);
        if accepted_pending_submission {
            self.submitted_user_message_following = SubmittedUserMessageFollowing::PendingAnchor;
            self.session_ui.viewport.reveal(false);
            self.pending_transcript_top_anchor_sequence = Some(sequence);
        }
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
        let mut indicator = String::from("Managed conversation; native permissions and billing");
        if let Some(mode) = self.managed_composer.settings.permission_mode {
            indicator.push_str(" · ");
            indicator.push_str(super::managed_controls::permission_mode_label(mode));
        }
        if let Some(model) = &self.managed_composer.settings.model_id {
            indicator.push_str(" · model ");
            indicator.push_str(model);
        }
        self.set_execution_mode_indicator(Some(indicator));
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
        let _ = self.apply_session_view_terminal_adapter();
    }

    /// Start a candidate from the last acknowledged navigation state.
    /// Repeated or failed preparations never consume reveal requests twice.
    fn restore_uncommitted_navigation(&mut self) {
        if let Some(checkpoint) = self.navigation_checkpoint.take() {
            self.session_ui.viewport = checkpoint.viewport;
            self.transcript_scroll_animation = checkpoint.animation;
            self.pending_stable_transcript_anchor = checkpoint.anchor;
            self.submitted_user_message_following = checkpoint.submitted;
            self.assistant_scroll_anchor = checkpoint.assistant;
            self.pending_assistant_stream_anchor = checkpoint.pending_assistant;
            self.pending_transcript_top_anchor_sequence = checkpoint.pending_sequence;
            self.automatic_item = checkpoint.automatic_item;
            self.admitted_items = checkpoint.admitted_items;
            self.older_history = checkpoint.history;
            self.latest_hidden_activity_at = checkpoint.hidden_activity_at;
            self.latest_hidden_activity_burst = checkpoint.hidden_activity_burst;
            self.latest_bar_animation_started_at = checkpoint.bar_started_at;
            self.latest_bar_next_frame_at = checkpoint.bar_next_frame_at;
        }
    }

    pub(crate) fn begin_transcript_presentation(&mut self) {
        self.restore_uncommitted_navigation();
        // Capture against the old accepted geometry before a candidate may replace
        // it. A retry must not recapture old coordinates from the new layout.
        self.capture_stable_transcript_anchor();
        self.navigation_checkpoint = Some(TranscriptNavigationCheckpoint {
            automatic_item: self.automatic_item,
            admitted_items: self.admitted_items.clone(),
            viewport: self.session_ui.viewport,
            animation: self.transcript_scroll_animation,
            anchor: self.pending_stable_transcript_anchor.clone(),
            submitted: self.submitted_user_message_following,
            assistant: self.assistant_scroll_anchor,
            pending_assistant: self.pending_assistant_stream_anchor,
            pending_sequence: self.pending_transcript_top_anchor_sequence,
            history: self.older_history.clone(),
            hidden_activity_at: self.latest_hidden_activity_at,
            hidden_activity_burst: self.latest_hidden_activity_burst,
            bar_started_at: self.latest_bar_animation_started_at,
            bar_next_frame_at: self.latest_bar_next_frame_at,
        });
    }

    /// Advance correspondence only after the terminal presenter acknowledges a frame.
    pub fn commit_transcript_presentation(&mut self) {
        self.transcript_layout.clear_content_changes();
        self.navigation_checkpoint = None;
        let previous_anchor = self.presented_transcript_anchor.take();
        self.pending_stable_transcript_anchor = None;
        self.capture_stable_transcript_anchor();
        let mut accepted = self.pending_stable_transcript_anchor.take();
        if let (Some(previous), Some(current)) = (&previous_anchor, &mut accepted)
            && previous.item_id == current.item_id
            && previous.viewport_row_offset == current.viewport_row_offset
            && let Some(position) = previous.source_position
            && let Some(index) = self.transcript.presentation_index(current.item_id)
            && self.transcript_layout.source_row(index, position) == Some(current.row_in_item)
        {
            // Reflow changes the first byte of a visual row, not the source
            // position the reader was following. Keep it until manual input
            // moves to another row (which clears the presented anchor).
            current.source_position = Some(position);
        }
        self.presented_transcript_anchor = accepted;
    }

    pub(crate) fn capture_stable_transcript_anchor(&mut self) {
        if self.session_ui.viewport.follows_bottom() {
            if let Some(presentation) = &self.plugin_presentation {
                presentation.clear_content_positions();
            }
            return;
        }
        if self.pending_stable_transcript_anchor.is_some()
            || self.transcript_scroll_animation.is_some()
        {
            return;
        }
        if let Some(anchor) = &self.presented_transcript_anchor
            && self.transcript.presentation_index(anchor.item_id).is_some()
        {
            self.pending_stable_transcript_anchor = Some(anchor.clone());
            if let Some((identity, offset)) = &anchor.content
                && let Some(presentation) = &self.plugin_presentation
            {
                presentation.retain_content_position(identity, *offset);
            }
            return;
        }
        let top_row = self.session_ui.viewport.top_row(
            self.transcript_layout.total_rows(),
            self.session_ui.viewport.height(),
        );
        let Some(line) = self
            .transcript_layout
            .visible_lines_from_top(top_row, self.session_ui.viewport.height().max(1))
            .into_iter()
            .find(|line| line.source == VisibleTranscriptSource::Transcript)
        else {
            return;
        };
        let Some(item_id) = self.transcript.presentation_id(line.entry_index) else {
            return;
        };
        self.pending_stable_transcript_anchor = Some(StableTranscriptAnchor {
            item_id,
            fallback_index: line.entry_index,
            viewport_row_offset: line.row_index.saturating_sub(top_row),
            source_position: self
                .transcript_layout
                .source_position(line.entry_index, line.row_in_entry),
            row_in_item: line.row_in_entry,
            content: self
                .transcript_layout
                .content_anchor(line.entry_index, line.row_in_entry)
                .map(|(key, offset)| (key.to_owned(), offset)),
        });
    }

    fn apply_session_view_terminal_adapter(&mut self) -> TranscriptDocumentDamage {
        self.capture_stable_transcript_anchor();
        let snapshot = self.session_view.snapshot();
        let damage = self
            .session_view_terminal_adapter
            .apply(snapshot, &mut self.transcript);
        if let TranscriptDocumentDamage::Items(ids) = &damage {
            self.transcript_dirty_items
                .extend(ids.iter().filter_map(|id| {
                    self.transcript
                        .source_index(id)
                        .and_then(|index| self.transcript.presentation_id(index))
                }));
        }
        #[cfg(test)]
        {
            self.observations.last_transcript_damage.clone_from(&damage);
        }
        self.transcript_projection_revision = self.transcript_projection_revision.saturating_add(1);
        damage
    }

    fn shared_terminal_item(&self, sequence: u64) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| item.sequence == Some(sequence))
            .map(terminal_item_from_shared)
    }

    fn shared_terminal_text_for_sequence(
        &self,
        sequence: u64,
        role: &'static str,
    ) -> Option<String> {
        self.shared_terminal_item(sequence)
            .filter(|item| item.role == role)
            .map(|item| item.text)
    }

    #[cfg(test)]
    fn latest_shared_terminal_text(&self, role: &'static str) -> Option<String> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .map(terminal_item_from_shared)
            .find(|item| item.role == role)
            .map(|item| item.text)
    }

    #[cfg(test)]
    fn latest_shared_streaming_terminal_item(&self, role: &'static str) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .map(terminal_item_from_shared)
            .find(|item| item.role == role && item.streaming)
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
                self.set_activity(if approved.is_none() {
                    ActivityState::WaitingPermission { name: tool_name }
                } else {
                    ActivityState::PreparingFollowUpRequest
                });
            }
            SessionTracePayload::ToolInvocationFinished { .. } => {
                self.set_activity(ActivityState::PreparingFollowUpRequest);
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

fn latest_bar_active_frame_duration(burst: u8) -> Duration {
    Duration::from_millis(
        210_u64
            .saturating_sub(u64::from(burst).saturating_mul(21))
            .max(36),
    )
}

fn next_latest_bar_frame_at(started_at: Instant, now: Instant, frame: Duration) -> Instant {
    let frame_nanos = frame.as_nanos().max(1);
    let frame_count = now.saturating_duration_since(started_at).as_nanos() / frame_nanos;
    let next_offset = frame_nanos.saturating_mul(frame_count.saturating_add(1));
    started_at + Duration::from_nanos(u64::try_from(next_offset).unwrap_or(u64::MAX))
}

fn tool_elapsed_invalidation_invocation_id(key: &InvalidationKey) -> Option<&str> {
    key.as_str()
        .strip_prefix(TOOL_ELAPSED_INVALIDATION_PREFIX)?
        .strip_prefix(':')
        .filter(|invocation_id| !invocation_id.is_empty())
}

fn is_latest_bar_animation_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == LATEST_BAR_ANIMATION_INVALIDATION_KEY
}

fn is_theme_transition_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == THEME_TRANSITION_INVALIDATION_KEY
}

fn is_activity_animation_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == ACTIVITY_ANIMATION_INVALIDATION_KEY
}

fn is_transcript_scroll_animation_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY
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
        if cost.unavailable_usage_count > 0
            || cost.observed_usage_count > cost.estimated_usage_count
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

const fn event_breaks_sticky_entry_anchor(event: &SessionEvent) -> bool {
    matches!(&event.kind, SessionEventKind::PermissionRequested { .. })
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
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
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
        let before = app.elapsed_layout_revision();
        assert!(app.apply_plugin_presentation_config(&config).unwrap());
        assert_eq!(app.elapsed_layout_revision(), before + 1);
        assert!(!app.apply_plugin_presentation_config(&config).unwrap());
        let payload = calls.lock().unwrap()[0].clone();
        assert!(!payload.to_string().contains("private-marker"));
        assert_eq!(payload.as_object().unwrap().len(), 1);
        assert!(payload["shell"].get("env").is_none());
        config.tools.shell.output.presentation.python = false;
        assert!(app.apply_plugin_presentation_config(&config).unwrap());
        assert_eq!(app.elapsed_layout_revision(), before + 2);
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

    #[test]
    fn older_item_becoming_tail_cannot_supersede_assistant_hold() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        let old = super::super::transcript::TranscriptItem::new("You", "old prompt".to_owned());
        let assistant =
            super::super::transcript::TranscriptItem::new("Assistant", "reply".to_owned());
        app.transcript
            .push_ephemeral("test".to_owned(), old.clone());
        app.transcript
            .push_ephemeral("test".to_owned(), assistant.clone());
        app.automatic_item = Some(assistant.id());
        app.admitted_items.extend([old.id(), assistant.id()]);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.session_ui.viewport.reveal(true);
        app.session_ui.viewport.materialize_top_row(10);
        // Reconciliation removes the tail; the previously admitted user item is
        // now last. This is not a newly arrived user message or a tail request.
        app.transcript = super::super::transcript_document::TranscriptDocument::default();
        app.transcript.push_ephemeral("test".to_owned(), old);
        app.sync_transcript_anchor_requests();
        assert!(app.transcript_scroll_animation.is_none());
        assert!(!app.session_ui.viewport.allows_overflow());
        assert_eq!(app.session_ui.viewport.top_row(30, 10), 10);
        drop(app);
    }

    #[test]
    fn completed_row_animation_preserves_hold_without_item_correspondence() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.session_ui.viewport.reveal(true);
        let now = std::time::Instant::now();
        let mut animation =
            super::TranscriptScrollAnimation::new(0, 12, now, Duration::from_millis(180));
        // Missing correspondence remains a row target, not a tail command.
        animation.started_at = now
            .checked_sub(std::time::Duration::from_secs(1))
            .expect("clock permits test offset");
        app.transcript_scroll_animation = Some(animation);
        app.sync_transcript_scroll_max(20, 9, 30, 10);
        assert_eq!(app.session_ui.viewport.top_row(30, 10), 12);
        assert!(!app.session_ui.viewport.allows_overflow());
        app.sync_transcript_scroll_max(25, 9, 35, 10);
        assert_eq!(app.session_ui.viewport.top_row(35, 10), 12);
        drop(app);
    }

    #[test]
    fn manual_scroll_cancels_deferred_sequence_and_submission_reveals() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.pending_submissions.stage("pending".to_owned());
        app.request_transcript_top_anchor_sequence(42);
        assert!(app.scroll_transcript_up(2));
        app.expire_manual_transcript_scroll_for_test();
        app.sync_transcript_anchor_requests();
        assert!(app.pending_transcript_top_anchor_sequence.is_none());
        assert!(!app.pending_submissions.reveals_on_acceptance("pending"));
        assert_eq!(app.pending_submissions.items()[0].text(), "pending");
        app.pending_submissions.stage("later".to_owned());
        assert!(app.pending_submissions.reveals_on_acceptance("later"));
        drop(app);
    }

    #[test]
    fn pages_must_progress_without_overlapping_the_resident_window() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: format!("message {sequence}"),
            },
        };
        let mut app = super::BmuxApp::new_with_history(Some(session_id), &[event(10)], &[], true);
        let older = bcode_session_models::SessionHistoryCursor { sequence: 9 };
        assert!(app.accepts_history_page_events(older, true, &[event(2), event(8)], true));
        for invalid in [
            vec![event(2), event(10)],
            vec![event(8), event(2)],
            vec![event(11)],
        ] {
            assert!(!app.accepts_history_page_events(older, true, &invalid, true));
        }
        assert!(!app.accepts_history_page_events(older, true, &[], true));
        assert!(app.accepts_history_page_events(older, true, &[], false));
        let mut page = bcode_session_models::SessionHistoryPage {
            session_id,
            events: vec![event(8)],
            compatibility_issues: Vec::new(),
            next_cursor: Some(bcode_session_models::SessionHistoryCursor { sequence: 7 }),
            has_more: true,
        };
        assert!(app.accepts_history_page_payload(older, true, &page));
        page.next_cursor = None;
        assert!(app.accepts_history_page_payload(older, true, &page));
        page.next_cursor = Some(bcode_session_models::SessionHistoryCursor { sequence: 8 });
        assert!(!app.accepts_history_page_payload(older, true, &page));
        page.next_cursor = Some(bcode_session_models::SessionHistoryCursor { sequence: 7 });
        page.has_more = false;
        assert!(!app.accepts_history_page_payload(older, true, &page));
        page.has_more = true;
        page.session_id = bcode_session_models::SessionId::new();
        assert!(!app.accepts_history_page_payload(older, true, &page));
        page.session_id = session_id;
        page.compatibility_issues
            .push(bcode_session_models::SessionEventCompatibilityIssue {
                sequence: 7,
                event_kind: "future".to_owned(),
                schema_version: u16::MAX,
                compatibility: bcode_session_models::SessionEventCompatibilityKind::FutureSchema,
                remediation: "upgrade Bcode".to_owned(),
            });
        assert!(!app.accepts_history_page_payload(older, true, &page));
        app.replace_transcript_window(&[event(10)], true, true, 10);
        let newer = bcode_session_models::SessionHistoryCursor { sequence: 11 };
        assert!(app.accepts_history_page_events(newer, false, &[event(12), event(14)], false));
        assert!(!app.accepts_history_page_events(newer, false, &[event(10)], true));
        assert!(!app.accepts_history_page_events(newer, false, &[event(14), event(12)], true));
        drop(app);
    }

    #[test]
    fn replacement_rejects_in_flight_pages_for_same_session() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: format!("message {sequence}"),
            },
        };
        let mut app = super::BmuxApp::new_with_history(Some(session_id), &[event(10)], &[], true);
        let generation = app.history_window_generation();
        let cursor = app.older_history_cursor().expect("older boundary");
        app.set_loading_older_history(true);
        assert!(app.accepts_history_page(generation, cursor, true));
        app.replace_latest_transcript_window(&[event(10)], true);
        app.set_loading_older_history(true);
        // Even an identical cursor and session after reconnect are a new window.
        assert!(!app.accepts_history_page(generation, cursor, true));
        assert!(app.accepts_history_page(app.history_window_generation(), cursor, true));
        app.replace_transcript_window(&[event(10)], true, true, 10);
        assert!(!app.accepts_history_page(generation, cursor, true));
        drop(app);
    }

    #[test]
    fn newer_boundary_requests_survive_app_bottom_policy_and_page_arrival() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        let session_id = bcode_session_models::SessionId::new();
        let event = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 10,
            timestamp_ms: 10,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "history".to_owned(),
            },
        };
        app.older_history.replace_centered(&[event], true, true, 10);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.session_ui.viewport.follow_anchor(18);
        assert!(app.scroll_transcript_down(3));
        assert_eq!(app.session_ui.viewport.top_row(30, 10), 20);
        assert!(app.should_load_newer_history());
        app.older_history.set_loading_newer(true);
        assert!(app.scroll_transcript_down(2));
        app.session_ui.viewport.sync_with_anchor(
            (20, 9, 30, 10),
            Some(20),
            10,
            0,
            &mut app.older_history,
        );
        assert_eq!(app.older_history.newer_reveal_request(), Some(3));
        app.older_history.update_newer_cursor(&[], false);
        app.older_history.set_loading_newer(false);
        app.session_ui.viewport.sync_with_anchor(
            (30, 9, 40, 10),
            Some(20),
            10,
            0,
            &mut app.older_history,
        );
        // The terminal empty page adds no navigable rows. Unrelated layout
        // growth cannot satisfy the outstanding boundary movement.
        assert_eq!(app.session_ui.viewport.top_row(40, 10), 20);
        assert_eq!(app.older_history.newer_reveal_request(), None);
        drop(app);
    }

    #[test]
    fn uncommitted_retries_do_not_accumulate_hidden_activity() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        let initial = app.latest_hidden_activity_burst;
        app.begin_transcript_presentation();
        app.record_latest_hidden_activity(std::time::Instant::now(), 4);
        assert!(app.latest_hidden_activity_burst > initial);
        app.begin_transcript_presentation();
        assert_eq!(app.latest_hidden_activity_burst, initial);
        assert!(app.latest_hidden_activity_at.is_none());
        drop(app);
    }

    #[test]
    fn manual_scroll_supersedes_uncommitted_navigation() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.begin_transcript_presentation();
        app.session_ui.viewport.reveal(true);
        assert!(app.scroll_transcript_up(2));
        let manual = app.session_ui.viewport;
        app.begin_transcript_presentation();
        assert_eq!(app.session_ui.viewport, manual);
        assert!(!app.session_ui.viewport.allows_reveal());
        drop(app);
    }

    #[test]
    fn uncommitted_navigation_retries_do_not_consume_reveal_intent() {
        let mut app = super::BmuxApp::new_with_history(None, &[], &[], false);
        app.session_ui
            .viewport
            .sync_max(20, 9, 30, 10, false, &mut app.older_history);
        app.session_ui
            .viewport
            .scroll_down(4, &mut app.older_history);
        let before = app.session_ui.viewport;
        app.begin_transcript_presentation();
        app.session_ui.viewport.sync_with_anchor(
            (30, 9, 40, 10),
            None,
            10,
            0,
            &mut app.older_history,
        );
        assert!(app.session_ui.viewport.follows_bottom());
        app.begin_transcript_presentation();
        assert_eq!(app.session_ui.viewport, before);
        app.session_ui.viewport.sync_with_anchor(
            (30, 9, 40, 10),
            None,
            10,
            0,
            &mut app.older_history,
        );
        app.commit_transcript_presentation();
        app.begin_transcript_presentation();
        assert!(app.session_ui.viewport.follows_bottom());
        drop(app);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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

    use crate::transcript::TranscriptItemKind;
    use crate::transcript_layout::{TranscriptLayoutInvalidation, VisibleTranscriptSource};

    fn invalidation_deadline(app: &BmuxApp, key: &str, now: Instant) -> Option<Instant> {
        app.invalidation_requests(now, SystemTime::UNIX_EPOCH)
            .into_iter()
            .find(|request| request.key.as_str() == key)
            .map(|request| request.at)
    }

    #[test]
    fn local_system_notice_stays_at_its_chronological_insertion_point() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app =
            BmuxApp::new_with_history(Some(session_id), &[event(1, "before")], &[], false);

        app.push_ephemeral_system_plain("send failed".to_owned());
        app.absorb_session_event(&event(2, "after"));

        let transcript = app
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(transcript, ["before", "send failed", "after"]);
    }

    #[test]
    fn multiple_local_system_notices_keep_chronological_order() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(1, "first")], &[], false);

        app.push_ephemeral_system_plain("notice one".to_owned());
        app.absorb_session_event(&event(2, "second"));
        app.push_ephemeral_system_plain("notice two".to_owned());
        app.absorb_session_event(&event(3, "third"));

        let transcript = app
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(
            transcript,
            ["first", "notice one", "second", "notice two", "third"]
        );
    }

    #[test]
    fn live_plugin_activity_updates_in_place_and_is_removed_when_absent() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], true);
        let mut status = bcode_session_view_models::PluginStatusView {
            plugin_id: "example".into(),
            note_id: "execution".into(),
            text: "Implementation running".into(),
            priority: 20,
            metadata: std::collections::BTreeMap::new(),
        };
        app.set_plugin_status(vec![status.clone()]);
        let first = app.transcript.iter().last().unwrap().id();
        status.text = "Evaluation waiting".into();
        app.set_plugin_status(vec![status]);
        assert_eq!(app.transcript.iter().last().unwrap().id(), first);
        assert_eq!(
            app.transcript.iter().last().unwrap().text(),
            "Evaluation waiting"
        );
        assert_eq!(
            app.transcript
                .iter()
                .filter(|item| item.text() == "Implementation running")
                .count(),
            0
        );
        app.set_plugin_status(Vec::new());
        let removed = !app.transcript.iter().any(|item| item.id() == first);
        drop(app);
        assert!(removed);
    }

    #[test]
    fn ephemeral_notice_keeps_chronology_when_older_history_is_prepended() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let later = event(2, "later");
        let mut app =
            BmuxApp::new_with_history(Some(session_id), std::slice::from_ref(&later), &[], true);
        app.push_ephemeral_system_plain("local issue".to_owned());

        app.replace_latest_transcript_window(&[event(1, "earlier"), later], false);

        let transcript = app
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(transcript, ["earlier", "later", "local issue"]);
    }

    #[test]
    fn ephemeral_notice_has_deterministic_placement_outside_resident_window() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut before =
            BmuxApp::new_with_history(Some(session_id), &[event(5, "five")], &[], false);
        before.push_ephemeral_system_plain("local issue".to_owned());
        before.replace_transcript_window(&[event(8, "eight")], true, true, 8);
        let transcript = before
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(before);
        assert_eq!(transcript, ["local issue", "eight"]);

        let mut after =
            BmuxApp::new_with_history(Some(session_id), &[event(5, "five")], &[], false);
        after.push_ephemeral_system_plain("local issue".to_owned());
        after.replace_transcript_window(&[event(2, "two")], true, true, 2);
        let transcript = after
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(after);
        assert_eq!(transcript, ["two", "local issue"]);
    }

    #[test]
    fn ephemeral_notice_uses_sequence_fallback_when_exact_anchor_leaves_window() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app = BmuxApp::new_with_history(
            Some(session_id),
            &[event(1, "one"), event(2, "two")],
            &[],
            false,
        );
        app.push_ephemeral_system_plain("local issue".to_owned());

        app.replace_latest_transcript_window(&[event(1, "one"), event(3, "three")], false);

        let transcript = app
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(transcript, ["one", "local issue", "three"]);
    }

    #[test]
    fn sequence_lookup_returns_visible_index_across_ephemeral_notice() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(1, "one")], &[], false);
        app.push_ephemeral_system_plain("local issue".to_owned());
        app.absorb_session_event(&event(2, "two"));

        let index = app.transcript_index_for_sequence(2);
        drop(app);
        assert_eq!(index, Some(2));
    }

    #[test]
    fn ephemeral_notice_has_explicit_local_provenance() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);

        app.push_ephemeral_system_plain("local issue".to_owned());

        let has_local_provenance = matches!(
            app.transcript.origin(0),
            Some(TranscriptPresentationOrigin::Ephemeral { source, .. })
                if source == "bcode.tui"
        );
        let canonical_transcript_is_empty = app.session_view_snapshot().transcript.items.is_empty();
        drop(app);
        assert!(has_local_provenance);
        assert!(canonical_transcript_is_empty);
    }

    #[test]
    fn same_session_reconstruction_preserves_ephemeral_notices() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "before".to_owned(),
            },
        }];
        let mut source = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        source.push_ephemeral_system_plain("local issue".to_owned());
        let mut reconstructed = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

        reconstructed.take_same_session_transcript_state_from(&source);
        drop(source);

        let transcript = reconstructed
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(reconstructed);
        assert_eq!(transcript, ["before", "local issue"]);
    }

    #[test]
    fn cross_session_reconstruction_does_not_transfer_ephemeral_notices() {
        let mut source = BmuxApp::new_with_history(
            Some(bcode_session_models::SessionId::new()),
            &[],
            &[],
            false,
        );
        source.push_ephemeral_system_plain("local issue".to_owned());
        let mut reconstructed = BmuxApp::new_with_history(
            Some(bcode_session_models::SessionId::new()),
            &[],
            &[],
            false,
        );

        reconstructed.take_same_session_transcript_state_from(&source);
        drop(source);

        let is_empty = reconstructed.transcript().is_empty();
        drop(reconstructed);
        assert!(is_empty);
    }

    #[test]
    fn sessionless_draft_notice_does_not_transfer_into_opened_session() {
        let mut draft = BmuxApp::new_with_history(None, &[], &[], false);
        draft.push_ephemeral_system_plain("draft issue".to_owned());
        let mut opened = BmuxApp::new_with_history(
            Some(bcode_session_models::SessionId::new()),
            &[],
            &[],
            false,
        );

        opened.take_same_session_transcript_state_from(&draft);
        drop(draft);

        let is_empty = opened.transcript().is_empty();
        drop(opened);
        assert!(is_empty);
    }

    #[test]
    fn reconstructed_canonical_history_does_not_recreate_ephemeral_notice() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "canonical".to_owned(),
            },
        }];
        let mut live = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        live.push_ephemeral_system_plain("local issue".to_owned());
        let live_transcript = live
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(live);
        assert_eq!(live_transcript, ["canonical", "local issue"]);

        let reconstructed = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let transcript = reconstructed
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(reconstructed);
        assert_eq!(transcript, ["canonical"]);
    }

    #[test]
    fn reopening_the_attached_session_preserves_notices_through_the_intermediate_app() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "canonical".to_owned(),
            },
        }];
        let mut attached = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        attached.push_ephemeral_system_plain("local issue".to_owned());

        // `start_switch_session_at_sequence` installs an empty intermediate app for the same
        // session before the attach result arrives. Losing notices here would make reopening the
        // current session silently drop them even though the final app transfers state.
        let mut intermediate = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        intermediate.take_same_session_transcript_state_from(&attached);
        drop(attached);
        assert_eq!(
            intermediate
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["local issue"]
        );

        let mut completed = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        completed.take_same_session_transcript_state_from(&intermediate);
        drop(intermediate);
        let transcript = completed
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(completed);

        assert_eq!(
            transcript,
            ["canonical", "local issue"],
            "the notice survives the full reopen chain and reanchors to canonical history"
        );
    }

    #[test]
    fn ephemeral_notices_stay_out_of_every_canonical_and_context_bearing_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "canonical".to_owned(),
            },
        }];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        app.push_ephemeral_system_plain("local issue".to_owned());
        assert_eq!(app.transcript().len(), 2);

        // The shared semantic projection is the only source used for canonical history reads,
        // canonical export, model context, migration, and repair. A notice that never enters it
        // cannot reach any of those paths.
        let snapshot = app.session_view_snapshot();
        assert_eq!(snapshot.transcript.items.len(), 1);
        assert!(
            !snapshot.transcript.items.iter().any(|item| matches!(
                &item.kind,
                bcode_session_view_models::TranscriptViewItemKind::SystemMessage { message }
                    if message.text.contains("local issue")
            )),
            "no canonical transcript item carries the ephemeral notice text"
        );

        // Ephemeral entries also carry no canonical identity or sequence, so nothing can address
        // them as canonical history.
        let notice = app
            .transcript()
            .iter()
            .find(|item| item.text() == "local issue")
            .expect("notice");
        assert!(notice.source_view_item_id().is_none());
        assert!(notice.event_sequence().is_none());
        let canonical_index = app.transcript_index_for_sequence(1);
        drop(app);
        assert_eq!(canonical_index, Some(0));
    }

    #[test]
    fn durable_presentation_notes_replay_as_canonical_entries_beside_ephemeral_notices() {
        let session_id = bcode_session_models::SessionId::new();
        let durable_note = bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::PluginStatusNote {
                plugin_id: "bcode.tui".to_owned(),
                note_id: "note-1".to_owned(),
                text: "durable note".to_owned(),
                metadata: std::collections::BTreeMap::from([(
                    "presentation_only".to_owned(),
                    serde_json::Value::Bool(true),
                )]),
            },
        };
        let mut app = BmuxApp::new_with_history(
            Some(session_id),
            std::slice::from_ref(&durable_note),
            &[],
            false,
        );
        app.push_ephemeral_system_plain("local issue".to_owned());

        assert_eq!(
            app.transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["durable note", "local issue"]
        );

        // The durable note came from canonical history, so it owns canonical provenance and a
        // sequence; the process-local notice owns neither.
        assert!(matches!(
            app.transcript.origin(0),
            Some(TranscriptPresentationOrigin::Canonical { .. })
        ));
        assert_eq!(app.transcript()[0].event_sequence(), Some(1));
        assert!(matches!(
            app.transcript.origin(1),
            Some(TranscriptPresentationOrigin::Ephemeral { .. })
        ));
        assert_eq!(app.session_view_snapshot().transcript.items.len(), 1);
        drop(app);

        // Replaying the same canonical history reproduces only the durable note.
        let replayed = BmuxApp::new_with_history(
            Some(session_id),
            std::slice::from_ref(&durable_note),
            &[],
            false,
        );
        let transcript = replayed
            .transcript()
            .iter()
            .map(|item| item.text().to_owned())
            .collect::<Vec<_>>();
        drop(replayed);
        assert_eq!(transcript, ["durable note"]);
    }

    #[test]
    fn theme_changes_invalidate_presentation_without_mutating_session_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: "# Stable history".to_owned(),
            },
        }];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let transcript_revision = app.transcript_projection_revision();
        let markdown_revision = app.markdown_presentation_revision();
        let transcript_before = app
            .transcript()
            .iter()
            .map(|item| (item.id(), item.text().to_owned()))
            .collect::<Vec<_>>();
        let fingerprint_before = app.presented_theme().fingerprint;

        assert!(app.apply_theme("bcode-dark"));
        assert_ne!(app.presented_theme().fingerprint, fingerprint_before);
        assert_eq!(app.transcript_projection_revision(), transcript_revision);
        assert_eq!(app.markdown_presentation_revision(), markdown_revision);
        let transcript_after = app
            .transcript()
            .iter()
            .map(|item| (item.id(), item.text().to_owned()))
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(transcript_after, transcript_before);
    }

    #[test]
    fn runtime_refresh_preserves_unsent_agent_selection() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);

        assert!(app.preview_theme("bcode-dark"));
        assert!(app.preview_theme("nord"));
        assert!(app.preview_theme("terminal-native"));

        let discovery_count = app.theme_catalog_discovery_count();
        drop(app);
        assert_eq!(discovery_count, 1);
    }

    #[test]
    fn theme_preview_apply_and_cancel_are_in_memory_transitions() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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

    #[test]
    fn effective_keymap_replacement_invalidates_cached_rows_once() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_interactions(vec![interaction("keymap")]);
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        assert!(app.transcript_layout().total_rows() > 0);
        let before = app.elapsed_layout_revision();
        let registry = crate::keymap::BmuxKeyMap::from_config(&bcode_config::TuiConfig::default())
            .registry()
            .clone();
        app.set_effective_keymap(registry.clone());
        assert_eq!(app.transcript_layout().total_rows(), 0);
        assert_eq!(app.elapsed_layout_revision(), before + 1);
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        let rows = app.transcript_layout().total_rows();
        assert!(rows > 0);
        app.set_effective_keymap(registry);
        assert_eq!(app.transcript_layout().total_rows(), rows);
        assert_eq!(app.elapsed_layout_revision(), before + 1);

        let mut config = bcode_config::TuiConfig::default();
        config.keybindings.scopes.insert(
            "chat".into(),
            std::collections::BTreeMap::from([("not-a-valid-key".into(), "app.exit".into())]),
        );
        let invalid = crate::keymap::BmuxKeyMap::from_config(&config);
        assert!(invalid.error().is_some());
        app.set_effective_keymap(invalid.registry().clone());
        assert_eq!(app.transcript_layout().total_rows(), 0);
        assert_eq!(
            app.key_hints,
            "unbound send · unbound interrupt · unbound exit · unbound palette"
        );
        assert_eq!(
            app.effective_keymap()
                .action_for_key("chat", bmux_keyboard::parse_key_stroke("enter").unwrap(),),
            None,
        );
        for width in [0, 1, 2, 80] {
            crate::transcript_projection::sync_layout_for_test(&mut app, width);
        }
        drop(app);
    }

    #[test]
    fn active_interaction_row_range_tracks_lifecycle_height_changes() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_interactions(vec![interaction("first")]);
        let index = app.interaction_transcript_index("first").expect("first");
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        let fallback = app
            .transcript_layout()
            .entry_row_count(VisibleTranscriptSource::Transcript, index)
            .expect("fallback rows");

        for height in [9, 10, 14, 21, 7] {
            app.set_active_interaction_layout(Some(("first".to_owned(), height)));
            crate::transcript_projection::sync_layout_for_test(&mut app, 80);
            assert_eq!(
                app.transcript_item_row_range(index)
                    .expect("interaction row range")
                    .len(),
                usize::from(height)
            );
        }

        app.set_active_interaction_layout(None);
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        let restored_height = app
            .transcript_item_row_range(index)
            .expect("restored fallback range")
            .len();
        drop(app);
        assert_eq!(restored_height, fallback);
    }

    #[test]
    fn active_interaction_layout_replaces_cached_fallback_rows_incrementally() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_interactions(vec![interaction("first"), interaction("second")]);
        let first = app.interaction_transcript_index("first").expect("first");
        let second = app.interaction_transcript_index("second").expect("second");

        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        let fallback_first = app
            .transcript_layout()
            .entry_row_count(VisibleTranscriptSource::Transcript, first)
            .expect("fallback first rows");
        let fallback_second = app
            .transcript_layout()
            .entry_row_count(VisibleTranscriptSource::Transcript, second)
            .expect("fallback second rows");
        app.transcript_layout_mut().drain_sync_stats();

        app.set_active_interaction_layout(Some(("first".to_owned(), 17)));
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);

        assert_eq!(
            app.transcript_layout()
                .entry_row_count(VisibleTranscriptSource::Transcript, first),
            Some(17)
        );
        assert_eq!(
            app.transcript_layout()
                .entry_row_count(VisibleTranscriptSource::Transcript, second),
            Some(fallback_second)
        );
        assert_ne!(fallback_first, 17);
        let stats = app.transcript_layout_mut().drain_sync_stats();
        drop(app);
        assert_eq!(stats.len(), 1);
        assert_eq!(
            stats[0].invalidation,
            TranscriptLayoutInvalidation::Incremental
        );
        assert_eq!(stats[0].entries_scanned, 1);
        assert_eq!(stats[0].entries_rebuilt, 1);
        assert_eq!(stats[0].rows_regenerated, 17);
    }

    #[test]
    fn active_interaction_layout_marks_only_previous_and_next_entries_dirty() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_interactions(vec![interaction("first"), interaction("second")]);
        let first = app.interaction_transcript_index("first").expect("first");
        let second = app.interaction_transcript_index("second").expect("second");
        let first_id = app.transcript.presentation_id(first).expect("first id");
        let second_id = app.transcript.presentation_id(second).expect("second id");

        app.set_active_interaction_layout(Some(("first".to_owned(), 5)));
        assert_eq!(
            app.transcript_dirty_items_for_test(),
            &BTreeSet::from([first_id])
        );
        app.drain_transcript_dirty_items();
        app.set_active_interaction_layout(Some(("second".to_owned(), 7)));
        let dirty_items = app.transcript_dirty_items_for_test().clone();
        drop(app);
        assert_eq!(dirty_items, BTreeSet::from([first_id, second_id]));
    }

    #[test]
    fn dirty_presentation_identity_resolves_after_earlier_insertion() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let later = event(2, "later");
        let mut app =
            BmuxApp::new_with_history(Some(session_id), std::slice::from_ref(&later), &[], true);
        app.mark_transcript_item_dirty(0);

        app.replace_latest_transcript_window(&[event(1, "earlier"), later], false);

        assert_eq!(app.drain_transcript_dirty_items(), BTreeSet::from([1]));
        let text = app.transcript()[1].text().to_owned();
        drop(app);
        assert_eq!(text, "later");
    }

    fn interaction(id: &str) -> bcode_session_view_models::InteractionViewSummary {
        bcode_session_view_models::InteractionViewSummary {
            interaction_id: id.to_owned(),
            producer_id: Some("example.plugin".to_owned()),
            exchange_schema: Some("example.request".to_owned()),
            exchange_schema_version: Some(1),
            kind: "example.interaction".to_owned(),
            tool_call_id: Some(format!("call-{id}")),
            title: Some(id.to_owned()),
            required: true,
            snapshot: None,
            state: bcode_session_view_models::InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        }
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

    fn markdown_details(
        id: &str,
        default_open: bool,
    ) -> crate::markdown_interaction::VisibleMarkdownContribution {
        crate::markdown_interaction::VisibleMarkdownContribution {
            id: id.to_owned(),
            kind: bcode_markdown_render::MarkdownContributionKind::Details {
                summary: "More".to_owned(),
                body: "Body".to_owned(),
                default_open,
            },
        }
    }

    #[test]
    fn fragment_activation_navigates_to_typed_resident_anchor() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_transcript_viewport_for_details_test(80, 10, 0);
        app.reconcile_markdown_fragments(BTreeMap::from([("target".to_owned(), 24)]));
        app.reconcile_markdown_interactions(vec![
            crate::markdown_interaction::VisibleMarkdownContribution {
                id: "fragment-link".to_owned(),
                kind: bcode_markdown_render::MarkdownContributionKind::Link {
                    link: bcode_markdown_render::MarkdownLink {
                        destination: "#target".to_owned(),
                        title: None,
                        reference_id: None,
                        kind: bcode_markdown_render::MarkdownLinkKind::Inline,
                    },
                    label: "Target".to_owned(),
                    destination: bcode_markdown_render::MarkdownDestination::Fragment(
                        "target".to_owned(),
                    ),
                },
            },
        ]);

        assert!(app.activate_markdown_contribution("fragment-link"));
        assert_eq!(app.transcript_viewport_top_row_for_test(80, 10), 24);
        let status = app.status().to_owned();
        drop(app);
        assert_eq!(status, "Navigated to Markdown section");
    }

    #[test]
    fn fragment_navigation_remains_stable_after_resize_and_scroll() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_transcript_viewport_for_details_test(100, 12, 20);
        app.reconcile_markdown_fragments(BTreeMap::from([("target".to_owned(), 38)]));
        app.reconcile_markdown_interactions(vec![
            crate::markdown_interaction::VisibleMarkdownContribution {
                id: "fragment-link".to_owned(),
                kind: bcode_markdown_render::MarkdownContributionKind::Link {
                    link: bcode_markdown_render::MarkdownLink {
                        destination: "#target".to_owned(),
                        title: None,
                        reference_id: None,
                        kind: bcode_markdown_render::MarkdownLinkKind::Inline,
                    },
                    label: "Target".to_owned(),
                    destination: bcode_markdown_render::MarkdownDestination::Fragment(
                        "target".to_owned(),
                    ),
                },
            },
        ]);
        assert!(app.activate_markdown_contribution("fragment-link"));
        assert_eq!(app.transcript_viewport_top_row_for_test(100, 12), 38);

        app.sync_transcript_scroll_max(94, 0, 100, 6);
        assert_eq!(app.transcript_viewport_top_row_for_test(100, 6), 38);
        assert!(app.scroll_transcript_down(3));
        assert_eq!(app.transcript_viewport_top_row_for_test(100, 6), 41);
        assert!(app.activate_markdown_contribution("fragment-link"));
        let top_row = app.transcript_viewport_top_row_for_test(100, 6);
        drop(app);
        assert_eq!(top_row, 38);
    }

    #[test]
    fn missing_fragment_remains_safe_and_reports_failure() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_fragments(BTreeMap::new());
        app.reconcile_markdown_interactions(vec![
            crate::markdown_interaction::VisibleMarkdownContribution {
                id: "fragment-link".to_owned(),
                kind: bcode_markdown_render::MarkdownContributionKind::Link {
                    link: bcode_markdown_render::MarkdownLink {
                        destination: "#missing".to_owned(),
                        title: None,
                        reference_id: None,
                        kind: bcode_markdown_render::MarkdownLinkKind::Inline,
                    },
                    label: "Missing".to_owned(),
                    destination: bcode_markdown_render::MarkdownDestination::Fragment(
                        "missing".to_owned(),
                    ),
                },
            },
        ]);

        assert!(app.activate_markdown_contribution("fragment-link"));
        let status = app.status().to_owned();
        drop(app);
        assert_eq!(status, "Markdown fragment target was not found");
    }

    #[test]
    fn details_activation_preserves_detached_transcript_top_row_across_height_change() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_transcript_viewport_for_details_test(40, 10, 12);
        let top_row = app.transcript_top_row(10);
        app.reconcile_markdown_interactions(vec![markdown_details("details", false)]);

        assert!(app.activate_markdown_contribution("details"));
        app.sync_transcript_scroll_max(37, 0, 47, 10);

        assert_eq!(app.transcript_top_row(10), top_row);
        let scroll_offset = app.scroll_offset();
        drop(app);
        assert_eq!(scroll_offset, 47 - top_row - 10);
    }

    #[test]
    fn markdown_capability_changes_invalidate_presentation_layout() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let initial = app.markdown_presentation_revision();
        let mut config = app.tui_config().clone();
        config.markdown.network_images = true;
        app.apply_tui_config(config.clone());
        assert_eq!(app.markdown_presentation_revision(), initial + 1);

        app.apply_tui_config(config);
        let revision = app.markdown_presentation_revision();
        drop(app);
        assert_eq!(revision, initial + 1);
    }

    #[test]
    fn details_activation_toggles_from_source_default_and_updates_layout_revision() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_interactions(vec![markdown_details("details", true)]);

        assert_eq!(app.markdown_details_open().get("details"), None);
        assert_eq!(app.markdown_presentation_revision(), 0);
        assert!(app.activate_markdown_contribution("details"));
        assert_eq!(app.markdown_details_open().get("details"), Some(&false));
        assert_eq!(app.markdown_presentation_revision(), 1);
        assert!(app.focus_markdown_contribution("details"));
        assert!(app.activate_focused_markdown_contribution());
        assert_eq!(app.markdown_details_open().get("details"), Some(&true));
        let revision = app.markdown_presentation_revision();
        drop(app);
        assert_eq!(revision, 2);
    }

    #[test]
    fn details_state_survives_visibility_changes_and_is_removed_with_resident_owner() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_interactions(vec![markdown_details("details", false)]);
        assert!(app.activate_markdown_contribution("details"));

        app.reconcile_markdown_interactions(Vec::new());
        assert_eq!(app.markdown_details_open().get("details"), Some(&true));
        app.reconcile_markdown_details(&BTreeSet::from(["details".to_owned()]));
        assert_eq!(app.markdown_details_open().get("details"), Some(&true));

        app.reconcile_markdown_details(&BTreeSet::new());
        assert!(app.markdown_details_open().is_empty());
        let revision = app.markdown_presentation_revision();
        drop(app);
        assert_eq!(revision, 2);
    }

    fn markdown_footnote_reference(
        id: &str,
        target_id: &str,
    ) -> crate::markdown_interaction::VisibleMarkdownContribution {
        crate::markdown_interaction::VisibleMarkdownContribution {
            id: id.to_owned(),
            kind: bcode_markdown_render::MarkdownContributionKind::FootnoteReference {
                label: "note".to_owned(),
                target_id: target_id.to_owned(),
            },
        }
    }

    fn markdown_footnote_definition(
        id: &str,
        reference_ids: &[&str],
    ) -> crate::markdown_interaction::VisibleMarkdownContribution {
        crate::markdown_interaction::VisibleMarkdownContribution {
            id: id.to_owned(),
            kind: bcode_markdown_render::MarkdownContributionKind::FootnoteDefinition {
                label: "note".to_owned(),
                reference_ids: reference_ids
                    .iter()
                    .map(|reference| (*reference).to_owned())
                    .collect(),
            },
        }
    }

    #[test]
    fn offscreen_footnote_navigation_scrolls_and_focuses_after_reconciliation() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_transcript_viewport_for_details_test(80, 10, 0);
        app.reconcile_markdown_footnote_rows(BTreeMap::from([
            ("reference".to_owned(), 4),
            ("definition".to_owned(), 30),
        ]));
        app.reconcile_markdown_interactions(vec![markdown_footnote_reference(
            "reference",
            "definition",
        )]);

        assert!(app.activate_markdown_contribution("reference"));
        assert_eq!(app.transcript_viewport_top_row_for_test(80, 10), 30);
        assert_eq!(app.focused_markdown_contribution(), None);
        app.reconcile_markdown_interactions(vec![markdown_footnote_definition(
            "definition",
            &["reference"],
        )]);
        assert_eq!(app.focused_markdown_contribution(), Some("definition"));

        assert!(app.activate_focused_markdown_contribution());
        assert_eq!(app.transcript_viewport_top_row_for_test(80, 10), 4);
        app.reconcile_markdown_interactions(vec![markdown_footnote_reference(
            "reference",
            "definition",
        )]);
        let focused = app.focused_markdown_contribution().map(str::to_owned);
        drop(app);
        assert_eq!(focused.as_deref(), Some("reference"));
    }

    #[test]
    fn footnote_activation_returns_to_the_reference_that_navigated_to_definition() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_interactions(vec![
            markdown_footnote_reference("reference-1", "definition"),
            markdown_footnote_reference("reference-2", "definition"),
            markdown_footnote_definition("definition", &["reference-1", "reference-2"]),
        ]);

        assert!(app.activate_markdown_contribution("reference-2"));
        assert_eq!(app.focused_markdown_contribution(), Some("definition"));
        assert!(app.activate_focused_markdown_contribution());
        assert_eq!(app.focused_markdown_contribution(), Some("reference-2"));
        let return_targets_empty = app.footnote_return_targets.is_empty();
        drop(app);
        assert!(return_targets_empty);
    }

    #[test]
    fn footnote_definition_uses_first_reference_without_a_navigation_origin() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_interactions(vec![
            markdown_footnote_reference("reference-1", "definition"),
            markdown_footnote_reference("reference-2", "definition"),
            markdown_footnote_definition("definition", &["reference-1", "reference-2"]),
        ]);

        assert!(app.activate_markdown_contribution("definition"));
        let focused = app.focused_markdown_contribution().map(str::to_owned);
        drop(app);
        assert_eq!(focused.as_deref(), Some("reference-1"));
    }

    #[test]
    fn footnote_return_origin_is_removed_with_invisible_contributions() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reconcile_markdown_interactions(vec![
            markdown_footnote_reference("reference", "definition"),
            markdown_footnote_definition("definition", &["reference"]),
        ]);
        assert!(app.activate_markdown_contribution("reference"));
        assert_eq!(app.footnote_return_targets.len(), 1);

        app.reconcile_markdown_interactions(Vec::new());

        assert!(app.footnote_return_targets.is_empty());
        let focused = app.focused_markdown_contribution().map(str::to_owned);
        drop(app);
        assert_eq!(focused, None);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
    fn latest_bar_glow_follows_motion_and_schedules_no_frames_when_instant() {
        use bcode_tui_theme::chrome::Motion;
        let mut windows = Vec::new();
        for (motion, reduce_motion) in MOTION_CASES {
            let mut app = BmuxApp::new_with_history(None, &[], &[], false);
            app.set_motion_for_test(motion, reduce_motion);
            let now = Instant::now();
            app.record_latest_hidden_activity(now, 4);
            let window = app.motion_policy().latest_bar_window();
            let scheduled = app.latest_bar_next_frame_at.is_some();
            let active_now = app.latest_bar_active(now);
            let instant = motion == Motion::None || reduce_motion;
            if !instant {
                assert!(
                    app.latest_bar_active(now + window.saturating_sub(Duration::from_millis(1)))
                );
                assert!(!app.latest_bar_active(now + window));
            }
            drop(app);
            assert_eq!(window.is_zero(), instant, "{motion:?}/{reduce_motion}");
            assert_eq!(scheduled, !instant, "{motion:?}/{reduce_motion}");
            assert_eq!(active_now, !instant, "{motion:?}/{reduce_motion}");
            windows.push(window);
        }
        assert_eq!(windows[1], Duration::from_millis(420));
        assert!(windows[2] > windows[1], "full glows longer than subtle");
    }

    fn reduce_motion_now(app: &mut BmuxApp) {
        let mut config = app.tui_config().clone();
        config.reduce_motion = true;
        app.apply_tui_config(config);
    }

    #[test]
    fn reduce_motion_settles_a_running_scroll_animation_at_its_target() {
        use crate::frame_sequence_harness::{
            TranscriptFrameInput, TranscriptFrameSequence, TranscriptFrameStep,
        };
        let session_id = bcode_session_models::SessionId::new();
        let events = (1..31)
            .map(|sequence| bcode_session_models::SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                sequence,
                timestamp_ms: sequence,
                session_id,
                provenance: None,
                kind: bcode_session_models::SessionEventKind::AssistantMessage {
                    text: format!("message {sequence}"),
                },
            })
            .collect::<Vec<_>>();
        let mut app = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
        app.set_motion_for_test(bcode_tui_theme::chrome::Motion::Subtle, false);
        TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "reader at top",
            input: TranscriptFrameInput::ScrollUp(usize::MAX / 4),
        }]);
        assert!(app.transition_transcript_to_bottom());
        assert!(app.transcript_scroll_frame_requested_for_test());

        reduce_motion_now(&mut app);
        let running = app.transcript_scroll_animation_duration_for_test();
        let requested = app.transcript_scroll_frame_requested_for_test();
        let frames = TranscriptFrameSequence::new(&mut app, 80, 12).run([TranscriptFrameStep {
            label: "settled",
            input: TranscriptFrameInput::Observe,
        }]);
        let offset = app.scroll_offset();
        drop(app);
        assert_eq!(running, None);
        assert!(!requested);
        assert_eq!(frames[0].observation.scroll_mode, "bottom_follow");
        assert_eq!(offset, 0);
    }

    #[test]
    fn reduce_motion_settles_a_running_latest_bar_glow() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_motion_for_test(bcode_tui_theme::chrome::Motion::Full, false);
        let now = Instant::now();
        app.record_latest_hidden_activity(now, 4);
        assert!(app.latest_bar_next_frame_at.is_some());
        assert!(app.latest_bar_active(now));

        reduce_motion_now(&mut app);
        let deadline = app.latest_bar_next_frame_at;
        let burst = app.latest_hidden_activity_burst;
        let still = !app.latest_bar_active(now);
        // Settling keeps the record of unseen activity below the viewport.
        let activity = app.latest_hidden_activity_at;
        // Motion returning inside the glow window resumes the glow with
        // scheduled frames instead of one frozen frame.
        let mut config = app.tui_config().clone();
        config.reduce_motion = false;
        app.apply_tui_config(config);
        let resumed = app.latest_bar_active(now);
        let resumed_deadline = app.latest_bar_next_frame_at;
        drop(app);
        assert_eq!(deadline, None);
        assert_eq!(burst, 0);
        assert!(still);
        assert_eq!(activity, Some(now));
        assert!(resumed);
        assert!(resumed_deadline.is_some_and(|at| at > now));
    }

    #[test]
    fn subtle_motion_keeps_running_animations_on_config_apply() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_motion_for_test(bcode_tui_theme::chrome::Motion::Full, false);
        let now = Instant::now();
        app.record_latest_hidden_activity(now, 4);
        let config = app.tui_config().clone();
        app.apply_tui_config(config);
        let deadline = app.latest_bar_next_frame_at;
        drop(app);
        assert!(deadline.is_some());
    }

    #[test]
    fn animation_deadlines_catch_up_from_monotonic_origin() {
        let started_at = Instant::now();
        let mut scroll =
            TranscriptScrollAnimation::new(0, 10, started_at, Duration::from_millis(180));
        let delayed = started_at + Duration::from_millis(73);
        scroll.advance_frame(delayed);
        assert_eq!(scroll.next_frame_at, started_at + Duration::from_millis(80));
        assert!(scroll.next_frame_at > delayed);

        assert_eq!(
            next_latest_bar_frame_at(started_at, delayed, Duration::from_millis(40)),
            started_at + Duration::from_millis(80)
        );
    }

    #[test]
    fn activity_presentation_changes_only_on_accepted_ticks() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
    fn temporal_invalidations_report_precise_animation_damage() {
        let now = Instant::now();
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_activity(ActivityState::PreparingModelRequest);

        let (activity, damage) = app.handle_invalidations_with_damage(
            &[InvalidationKey::new(ACTIVITY_ANIMATION_INVALIDATION_KEY)],
            now,
        );
        assert_eq!(activity, UiInvalidation::Paint);
        assert_eq!(damage, [TemporalDamage::Status]);

        app.latest_hidden_activity_at = Some(now);
        app.latest_hidden_activity_burst = 4;
        app.latest_bar_animation_started_at = now;
        app.latest_bar_next_frame_at = Some(now);
        let (latest, damage) = app.handle_invalidations_with_damage(
            &[InvalidationKey::new(LATEST_BAR_ANIMATION_INVALIDATION_KEY)],
            now + Duration::from_millis(50),
        );
        assert_eq!(latest, UiInvalidation::Paint);
        assert_eq!(damage, [TemporalDamage::LatestBar]);
        assert!(app.latest_bar_next_frame_at.is_some());

        let (finished, damage) = app.handle_invalidations_with_damage(
            &[InvalidationKey::new(LATEST_BAR_ANIMATION_INVALIDATION_KEY)],
            now + app.motion_policy().latest_bar_window(),
        );
        let deadline_cleared = app.latest_bar_next_frame_at.is_none();
        drop(app);
        assert_eq!(finished, UiInvalidation::Paint);
        assert_eq!(damage, [TemporalDamage::LatestBar]);
        assert!(deadline_cleared);
    }

    #[test]
    fn tool_elapsed_invalidation_targets_visual_projection_entry() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let before = app.elapsed_layout_revision;
        let invalidation = app.handle_invalidations(
            &[InvalidationKey::new(format!(
                "{TOOL_ELAPSED_INVALIDATION_PREFIX}:call-item"
            ))],
            Instant::now(),
        );

        assert_eq!(invalidation, UiInvalidation::Paint);
        assert_eq!(app.elapsed_layout_revision, before.saturating_add(1));
        assert_eq!(
            app.drain_elapsed_dirty_visuals(),
            BTreeSet::from(["call-item".to_owned()])
        );
        let remaining_dirty_visuals = app.drain_elapsed_dirty_visuals();
        drop(app);
        assert!(remaining_dirty_visuals.is_empty());
    }

    #[test]
    fn tool_elapsed_deadline_is_not_starved_by_unrelated_reconciliation() {
        let now = Instant::now();
        let now_system = std::time::UNIX_EPOCH + Duration::from_secs(3);
        let mut registry = super::super::invalidation::TemporalRegistry::default();
        let app = running_tool_app_for_elapsed_test();
        let initial = app.invalidation_requests(now, now_system);
        let elapsed = initial
            .iter()
            .find(|request| tool_elapsed_invalidation_invocation_id(&request.key) == Some("call-1"))
            .expect("running tool schedules elapsed invalidation");
        let deadline = elapsed.at;
        registry.reconcile(initial);

        for offset in 1..=50 {
            let advanced = Duration::from_millis(offset);
            registry.reconcile(app.invalidation_requests(now + advanced, now_system + advanced));
        }
        drop(app);

        assert!(
            registry
                .take_due(deadline)
                .iter()
                .any(|key| { tool_elapsed_invalidation_invocation_id(key) == Some("call-1") })
        );
    }

    fn running_tool_app_for_elapsed_test() -> BmuxApp {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, timestamp_ms, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms,
            session_id,
            provenance: None,
            kind,
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.absorb_session_event(&event(
            1,
            1_000,
            bcode_session_models::SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        app.absorb_session_event(&event(
            2,
            2_000,
            bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        app
    }

    #[test]
    fn tool_elapsed_invalidation_requires_authoritative_running_status() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, timestamp_ms, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms,
            session_id,
            provenance: None,
            kind,
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.absorb_session_event(&event(
            1,
            1_000,
            bcode_session_models::SessionEventKind::ToolCallRequested {
                tool_call_id: "call-1".to_owned(),
                producer_plugin_id: Some("test.plugin".to_owned()),
                tool_name: "test.tool".to_owned(),
                arguments_json: "{}".to_owned(),
                working_directory: None,
            },
        ));
        app.absorb_session_event(&event(
            2,
            2_000,
            bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 1,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));
        app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Progress,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call-1".to_owned(),
                        contribution_id: "progress".to_owned(),
                        sequence: 1,
                        producer_id: "test.plugin".to_owned(),
                        schema: "test.progress".to_owned(),
                        schema_version: 1,
                        operation: bcode_session_models::ToolContributionOperation::Upsert,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"progress": true}),
                    },
                ),
            },
        });

        let now = Instant::now();
        let now_system = std::time::UNIX_EPOCH + Duration::from_secs(3);
        assert!(
            app.tool_elapsed_invalidation_requests(now, now_system)
                .any(|request| {
                    tool_elapsed_invalidation_invocation_id(&request.key) == Some("call-1")
                })
        );

        app.absorb_session_event(&event(
            3,
            2_500,
            bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call-1".to_owned(),
                    sequence: 2,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Completed,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));

        let elapsed_requests_cleared = app
            .tool_elapsed_invalidation_requests(now, now_system)
            .next()
            .is_none();
        drop(app);
        assert!(elapsed_requests_cleared);
    }

    #[test]
    fn raw_durable_handlers_cannot_create_or_finalize_terminal_rows() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };

        app.apply_session_event(
            &event(
                0,
                bcode_session_models::SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "raw user".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            SessionEventApplication::Live,
        );
        assert!(app.transcript.items().is_empty());

        app.session_view
            .apply_live_event(&bcode_session_models::SessionLiveEvent {
                session_id,
                kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                    turn_id: "turn-1".to_owned(),
                    segment_id: "segment-0".to_owned(),
                    segment_order: 0,
                    text: "streaming".to_owned(),
                },
            });
        app.apply_session_view_terminal_adapter();
        let before = app.transcript.clone();
        app.apply_session_event(
            &event(
                1,
                bcode_session_models::SessionEventKind::ModelTurnFinished {
                    turn_id: "turn-1".to_owned(),
                    outcome: bcode_session_models::ModelTurnOutcome::Completed,
                    message: None,
                },
            ),
            SessionEventApplication::Live,
        );
        let transcript = std::mem::take(&mut app.transcript);
        drop(app);
        assert_eq!(transcript, before);
    }

    #[test]
    fn legacy_thinking_visibility_feeds_reasoning_disclosure_in_the_config_layer() {
        use bcode_presentation_policy::models::{Disclosure, UnitFacts, UnitKind};
        let mut app = BmuxApp::new_with_history(Some(SessionId::new()), &[], &[], false);
        let reasoning = UnitFacts {
            kind: Some(UnitKind::Reasoning),
            ..UnitFacts::default()
        };
        let disclosure = |app: &BmuxApp| app.structure.snapshot().resolve(&reasoning).disclosure;
        assert_eq!(disclosure(&app), Disclosure::Expanded);
        app.apply_thinking_config(TuiThinkingConfig {
            show: false,
            ..TuiThinkingConfig::default()
        });
        assert!(!app.reasoning_visible());
        assert_eq!(disclosure(&app), Disclosure::Hidden);
        let item = TranscriptItem::new("Reasoning summary", "thinking".to_owned());
        let explained = app.structure.explain(&item).unwrap();
        assert!(
            explained
                .origins
                .iter()
                .any(|origin| origin.property == "disclosure"
                    && origin.source == bcode_presentation_policy::models::LayerSource::Config),
            "{explained:?}"
        );
        // Explicit configuration rules refine reasoning once it is shown again.
        app.apply_configured_structure_rules(
            toml::from_str::<bcode_presentation_policy::models::PresentationSection>(
                "[[rules]]\nmatch = { kind = \"reasoning\" }\nset = { disclosure = \"collapsed\" }",
            )
            .unwrap()
            .rules,
        );
        assert_eq!(disclosure(&app), Disclosure::Collapsed);
        app.set_reasoning_visible(true);
        assert_eq!(disclosure(&app), Disclosure::Collapsed);
        app.apply_configured_structure_rules(Vec::new());
        assert_eq!(disclosure(&app), Disclosure::Expanded);
        drop(app);
    }

    #[test]
    fn tui_stream_presentation_advances_only_shared_adapter_item_and_preserves_viewport() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        assert!(!app.apply_presentation_config(bcode_config::PresentationConfig::default()));
        let live = SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: 1,
                    revision: 1,
                    operation: bcode_session_models::TextStreamOperation::Append {
                        expected_offset: 0,
                        text: "smooth output".to_owned(),
                    },
                },
            },
        };
        app.absorb_session_live_event(&live);
        let item_id = bcode_session_view_models::TranscriptViewItemId::new(
            "assistant-turn:turn-1:segment:segment-1",
        );
        assert_eq!(app.transcript().len(), 1);
        let before_scroll = app.scroll_offset();
        let deadline = app
            .next_streaming_presentation_deadline(Instant::now())
            .expect("pending presentation deadline");
        assert!(app.advance_streaming_presentation(deadline + Duration::from_millis(50)));
        assert_eq!(app.transcript().len(), 1);
        assert!(app.transcript().iter().any(|item| {
            item.text().contains("smooth output")
                && matches!(item.kind(), &TranscriptItemKind::AssistantMessage)
        }));
        assert_eq!(app.scroll_offset(), before_scroll);
        let targeted_item = matches!(
            app.observations.last_transcript_damage,
            TranscriptDocumentDamage::Items(ref ids) if ids.contains(&item_id)
        );
        drop(app);
        assert!(targeted_item);
    }

    #[test]
    fn tui_disabling_stream_presentation_flushes_through_shared_adapter() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        assert!(!app.apply_presentation_config(bcode_config::PresentationConfig::default()));
        app.absorb_session_live_event(&SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-1".to_owned(),
                segment_order: 0,
                text: "pending output".to_owned(),
            },
        });
        let mut presentation = bcode_config::PresentationConfig::default();
        presentation.streaming.enabled = false;
        assert!(app.apply_presentation_config(presentation));
        assert!(
            app.next_streaming_presentation_deadline(Instant::now())
                .is_none()
        );
        let item_damage = matches!(
            app.observations.last_transcript_damage,
            TranscriptDocumentDamage::Items(_)
        );
        drop(app);
        assert!(item_damage);
    }

    #[test]
    fn raw_live_handlers_cannot_create_or_finalize_terminal_rows() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let live = bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "raw live assistant".to_owned(),
            },
        };

        app.apply_session_live_event_side_effects(&live);
        assert!(app.transcript.items().is_empty());

        app.session_view.apply_live_event(&live);
        app.apply_session_view_terminal_adapter();
        let before = app.transcript.clone();
        app.apply_session_live_event_side_effects(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: "turn-1".to_owned(),
                text: "raw reasoning".to_owned(),
            },
        });
        let transcript = std::mem::take(&mut app.transcript);
        drop(app);
        assert_eq!(transcript, before);
    }

    #[test]
    fn adapter_reports_item_damage_and_escalates_index_mismatch_to_full_reset() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let live = |text: &str| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        };
        app.session_view.apply_live_event(&live("first"));
        assert_eq!(
            app.apply_session_view_terminal_adapter(),
            TranscriptDocumentDamage::Structural
        );
        app.session_view.apply_live_event(&live(" second"));
        assert!(matches!(
            app.apply_session_view_terminal_adapter(),
            TranscriptDocumentDamage::Items(ids) if ids.len() == 1
        ));
        let id = bcode_session_view_models::TranscriptViewItemId::new(
            "assistant-turn:turn-1:segment:segment-0",
        );
        app.transcript.corrupt_source_index_for_test(id, usize::MAX);
        assert_eq!(
            app.apply_session_view_terminal_adapter(),
            TranscriptDocumentDamage::FullReset
        );
        let index_is_consistent = app.transcript.source_index_is_consistent();
        drop(app);
        assert!(index_is_consistent);
    }

    #[test]
    fn compatible_shared_replacement_preserves_terminal_identity() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let live = |text: &str| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        };
        app.absorb_session_live_event(&live("first"));
        let render_id = app.transcript()[0].id();
        app.absorb_session_live_event(&live(" second"));
        let transcript = std::mem::take(&mut app.transcript);
        drop(app);
        assert_eq!(transcript.items()[0].id(), render_id);
        assert_eq!(transcript.items()[0].text(), "first second");
    }

    #[test]
    fn session_view_terminal_adapter_reconciles_replaced_and_removed_message_items_by_stable_id() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let live = |text: &str| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        };

        app.absorb_session_live_event(&live("first"));
        let id = bcode_session_view_models::TranscriptViewItemId::new(
            "assistant-turn:turn-1:segment:segment-0",
        );
        let first_revision = app
            .transcript()
            .iter()
            .find(|item| item.source_view_item_id() == Some(&id))
            .expect("primary item")
            .source_view_item_revision()
            .expect("primary revision");

        app.absorb_session_live_event(&live(" second"));
        let replacement = app
            .transcript()
            .iter()
            .find(|item| item.source_view_item_id() == Some(&id))
            .expect("replacement item");
        assert!(
            replacement
                .source_view_item_revision()
                .is_some_and(|revision| revision > first_revision)
        );
        assert_eq!(
            app.transcript()
                .iter()
                .filter(|item| item.source_view_item_id() == Some(&id))
                .count(),
            1
        );

        app.session_view.clear_history_window();
        app.apply_session_view_terminal_adapter();
        let removed = app
            .transcript()
            .iter()
            .all(|item| item.source_view_item_id() != Some(&id));
        drop(app);
        assert!(removed);
    }

    #[test]
    fn elapsed_dirty_visual_drain_is_bounded_and_retains_overflow() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.elapsed_dirty_visuals = (0..100).map(|index| format!("call-{index:03}")).collect();
        let first = app.drain_elapsed_dirty_visuals_bounded(64);
        assert_eq!(first.len(), 64);
        assert_eq!(first.first().map(String::as_str), Some("call-000"));
        assert_eq!(first.last().map(String::as_str), Some("call-063"));
        let second = app.drain_elapsed_dirty_visuals_bounded(64);
        let remaining = app.drain_elapsed_dirty_visuals_bounded(64);
        drop(app);
        assert_eq!(second.len(), 36);
        assert!(remaining.is_empty());
    }

    #[test]
    fn replacing_latest_window_discards_stale_live_state_and_preserves_input_history() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };
        let initial = vec![event(
            1,
            bcode_session_models::SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "prompt".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &initial, &[], false);
        app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "stale partial".to_owned(),
            },
        });

        let replacement = vec![
            initial[0].clone(),
            event(
                2,
                bcode_session_models::SessionEventKind::AssistantMessage {
                    text: "durable answer".to_owned(),
                },
            ),
        ];
        app.replace_latest_transcript_window(&replacement, true);

        let transcript = &app.session_view_snapshot().transcript.items;
        assert_eq!(transcript.len(), 2);
        assert!(transcript.iter().any(|item| matches!(
            &item.kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "durable answer"
        )));
        assert!(!transcript.iter().any(|item| matches!(
            &item.kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text.contains("stale partial")
        )));
        assert_eq!(app.timeline_entries().len(), 1);
        let has_older_history = app.has_older_history();
        drop(app);
        assert!(has_older_history);
    }

    #[test]
    fn older_page_keeps_terminal_live_stream_and_blocks_stale_updates() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let stream = |revision, operation| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextStreamUpdated {
                output_position: None,
                turn_id: "active-turn".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                update: bcode_session_models::TextStreamUpdate {
                    generation: 0,
                    first_revision: revision,
                    revision,
                    operation,
                },
            },
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(2, "recent")], &[], true);
        app.absorb_session_live_event(&stream(
            1,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: 0,
                text: "finished live text".to_owned(),
            },
        ));
        app.absorb_session_live_event(&stream(
            2,
            bcode_session_models::TextStreamOperation::Terminal {
                status: bcode_session_models::TextStreamTerminalStatus::Completed,
            },
        ));
        app.prepend_older_history(&[event(1, "older")], false);
        app.absorb_session_live_event(&stream(
            3,
            bcode_session_models::TextStreamOperation::Append {
                expected_offset: "finished live text".len(),
                text: " stale".to_owned(),
            },
        ));
        assert_eq!(
            app.transcript()
                .iter()
                .filter(|item| item.text() == "finished live text")
                .count(),
            1
        );
        assert!(
            !app.transcript()
                .iter()
                .any(|item| item.text().contains("stale"))
        );
        drop(app);
    }

    #[test]
    fn older_history_page_preserves_live_stream_in_shared_and_terminal_views() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, text: &str| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::AssistantMessage {
                text: text.to_owned(),
            },
        };
        let mut app = BmuxApp::new_with_history(Some(session_id), &[event(2, "recent")], &[], true);
        app.absorb_session_live_event(&bcode_session_models::SessionLiveEvent {
            session_id,
            kind: bcode_session_models::SessionLiveEventKind::AssistantTextDelta {
                turn_id: "active-turn".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: "still streaming".to_owned(),
            },
        });
        assert!(
            app.transcript()
                .iter()
                .any(|item| item.text() == "still streaming")
        );

        app.prepend_older_history(&[event(1, "older")], false);
        assert!(app.session_view_snapshot().transcript.items.iter().any(|item| {
            matches!(&item.kind, bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "still streaming")
        }));
        assert!(
            app.transcript()
                .iter()
                .any(|item| item.text() == "still streaming")
        );
        drop(app);
    }

    #[test]
    fn native_observations_invalidate_and_retain_transcript_without_bcode_execution() {
        use bcode_harness_models::{
            HarnessCompleteness, HarnessInteraction, HarnessInteractionKind, HarnessObservation,
            HarnessObservationKind, HarnessPlacement, HarnessSubmission,
        };
        use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let kinds = [
            SessionEventKind::ExternalSubmissionAccepted {
                version: 1,
                submission: Box::new(HarnessSubmission {
                    submission_id: "native-input".into(),
                    attempt_id: "attempt".into(),
                    session_id: session_id.to_string(),
                    generation: 1,
                    text: "native submitted instructions".into(),
                    placement: HarnessPlacement::NextTurn,
                }),
            },
            SessionEventKind::ExternalObservationRecorded {
                version: 1,
                observation: Box::new(HarnessObservation {
                    observation_id: "native-tool".into(),
                    session_id: session_id.to_string(),
                    generation: 1,
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Partial,
                    kind: HarnessObservationKind::Tool {
                        input_json: None,
                        native_tool_id: "tool".into(),
                        name: "native tool marker".into(),
                        status: "observed".into(),
                    },
                }),
            },
            SessionEventKind::ExternalObservationRecorded {
                version: 1,
                observation: Box::new(HarnessObservation {
                    observation_id: "native-question".into(),
                    session_id: session_id.to_string(),
                    generation: 1,
                    native_correlations: vec![],
                    completeness: HarnessCompleteness::Partial,
                    kind: HarnessObservationKind::Interaction {
                        interaction: HarnessInteraction {
                            input_preview: None,
                            tool_name: None,
                            request_id: "question".into(),
                            session_id: session_id.to_string(),
                            generation: 1,
                            kind: HarnessInteractionKind::Question,
                            prompt: "native question marker".into(),
                            scope: "workspace".into(),
                            choices: vec![],
                        },
                    },
                }),
            },
        ];
        for (index, kind) in kinds.into_iter().enumerate() {
            let event = SessionEvent {
                schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                session_id,
                sequence: u64::try_from(index).unwrap() + 1,
                timestamp_ms: 1,
                provenance: None,
                kind,
            };
            assert!(super::event_affects_transcript_rows(&event));
            assert!(!super::event_breaks_sticky_entry_anchor(&event));
            app.absorb_session_event(&event);
            app.absorb_session_event(&event);
        }
        assert_eq!(app.session_view_snapshot().transcript.items.len(), 3);
        for marker in ["native submitted instructions", "native question marker"] {
            assert!(
                app.transcript()
                    .iter()
                    .any(|item| item.text().contains(marker))
            );
        }
        // The observed native tool is a generic tool card named by the native tool.
        assert!(app.transcript().iter().any(|item| matches!(
            item.kind(),
            crate::transcript::TranscriptItemKind::ToolRequest { tool_name, .. }
                if tool_name == "native tool marker"
        )));
        assert!(app.session_view_snapshot().tools.is_empty());
        assert_eq!(app.session_view_snapshot().permissions, Vec::new());
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

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
    fn provider_compaction_tui_hides_opaque_payloads() {
        let secret = "secret-opaque-tui-value";
        let session_id = bcode_session_models::SessionId::new();
        let history = [bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence: 0,
                snapshot: bcode_session_models::ProviderContextSnapshot {
                    format_version: 1,
                    request_fingerprint: None,
                    request_id: None,
                    provider_plugin_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    compatibility_key: "surface".to_owned(),
                    auth_profile: None,
                    origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                    messages_json: format!(r#"[{{"encrypted":"{secret}"}}]"#),
                    portable_summary: "portable summary".to_owned(),
                },
            },
        }];

        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let transcript = std::mem::take(&mut app.transcript);
        drop(app);
        assert_eq!(transcript.len(), 1);
        let item = &transcript.items()[0];
        assert!(item.text().contains("context compaction"));
        assert!(!item.text().contains(secret));
        assert!(!item.text().contains("portable summary"));
    }

    #[test]
    fn authoritative_active_skill_snapshot_drives_shared_tui_state() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
    fn terminal_runtime_failure_does_not_create_transcript_content() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.absorb_session_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::RuntimeWorkFinished {
                work_id: bcode_session_models::WorkId::new("ralph:loop"),
                status: RuntimeWorkStatus::Failed,
                finished_at_ms: Some(1),
                message: Some("boom".to_owned()),
            },
        });

        assert_eq!(
            app.session_view_snapshot().runtime_work,
            [] as [bcode_session_view_models::RuntimeWorkView; 0]
        );
        assert_eq!(
            app.session_view_snapshot().transcript.items,
            [] as [bcode_session_view_models::TranscriptViewItem; 0]
        );
        let transcript_is_empty = app.transcript().is_empty();
        drop(app);
        assert!(transcript_is_empty);
    }

    #[test]
    fn generic_lifecycle_drives_tui_activity_until_terminal_event() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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

        let app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

        assert_eq!(app.reasoning_effort(), Some("high"));
        let summary = app.reasoning_summary().map(str::to_owned);
        drop(app);
        assert_eq!(summary.as_deref(), Some("detailed"));
    }

    #[test]
    fn reasoning_and_skill_projections_are_event_driven() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
    fn ralph_lifecycle_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::SessionCreated {
                name: None,
                working_directory: std::path::PathBuf::from("/tmp/project"),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::RalphLifecycle {
                loop_name: "loop".to_owned(),
                state_dir: std::path::PathBuf::from("/tmp/project/.bcode/ralph/loop"),
                kind: "started".to_owned(),
                message: "running".to_owned(),
                occurred_at_ms: 2,
            },
        ));

        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(2))
            .expect("shared ralph lifecycle item");
        let expected = terminal_item_from_shared(shared);
        let transcript = std::mem::take(&mut app.transcript);
        drop(app);
        let actual = transcript
            .items()
            .iter()
            .last()
            .expect("ralph lifecycle item");
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(
            actual.text(),
            "Ralph started\n* Loop: loop\n* running\n* State: .bcode/ralph/loop"
        );
    }

    #[test]
    fn skill_transcript_rows_consume_shared_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
            bcode_session_models::SessionEventKind::SkillInvoked {
                skill_id: bcode_skill_models::SkillId::new("review"),
                arguments: "{}".to_owned(),
                source: None,
                invoked_at_ms: 1,
            },
        ));
        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::SkillContextLoaded {
                skill_id: bcode_skill_models::SkillId::new("review"),
                bytes_loaded: 42,
                truncated: true,
                source: Some(bcode_skill_models::SkillSource {
                    kind: bcode_skill_models::SkillSourceKind::User,
                    label: "user skills".to_owned(),
                    path: Some("/skills/review/SKILL.md".to_owned()),
                    precedence: 10,
                }),
                preview: Some("preview".to_owned()),
                loaded_at_ms: 2,
            },
        ));
        app.absorb_session_event(&event(
            3,
            bcode_session_models::SessionEventKind::SkillInvocationFailed {
                skill_id: bcode_skill_models::SkillId::new("review"),
                error: "boom".to_owned(),
                failed_at_ms: 3,
            },
        ));

        let transcript = std::mem::take(&mut app.transcript);
        let status = app.status().to_owned();
        let shared_items = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .map(terminal_item_from_shared)
            .map(|item| (item.role(), item.text().to_owned(), item.kind().clone()))
            .collect::<Vec<_>>();
        drop(app);
        let terminal = transcript.items().iter().collect::<Vec<_>>();
        assert_eq!(terminal.len(), 3);
        assert_eq!(terminal[0].role(), "Skill");
        assert_eq!(terminal[0].text(), "invoked review\nArguments: {}");
        assert_eq!(terminal[0].kind(), &TranscriptItemKind::Skill);
        assert_eq!(terminal[1].role(), "Skill context");
        assert_eq!(
            terminal[1].text(),
            "loaded review\nSource: user skills\nFile: /skills/review/SKILL.md\nBytes: 42 truncated\n\nPreview:\npreview"
        );
        assert_eq!(terminal[1].kind(), &TranscriptItemKind::Generic);
        assert_eq!(status, "loaded skill context: review (42 bytes truncated)");
        assert_eq!(terminal[2].role(), "Skill error");
        assert_eq!(terminal[2].text(), "review: boom");
        assert_eq!(terminal[2].kind(), &TranscriptItemKind::SkillError);
        assert_eq!(
            shared_items,
            terminal
                .iter()
                .map(|item| (item.role(), item.text().to_owned(), item.kind().clone()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn model_change_preserves_accumulated_shared_cost() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
                .footer_summary(None, 0, &cost)
                .ends_with("spent 0 tok · estimated $0.02 (not billed)")
        );
    }

    #[test]
    fn model_reasoning_and_agent_events_consume_shared_runtime_projection() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
                    .footer_summary(None, 0, &cost)
                    .contains("estimated $1115.98 (not billed)")
            );
        }
        let mixed = bcode_session_view_models::SessionCostSummary::rebuild(usages.iter());
        assert!(
            meter
                .footer_summary(None, 0, &mixed)
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

        let complete_text = meter.footer_summary(None, 0, &complete);
        assert!(complete_text.contains("estimated $0.00 (not billed)"));
        assert!(complete_text.contains("estimated EUR 2.000000 (not billed)"));
        assert!(!complete_text.contains('~'));
        assert!(
            meter
                .footer_summary(None, 0, &partial)
                .contains("cost partial: 2/3 requests priced")
        );
        assert!(
            meter
                .footer_summary(None, 0, &unavailable)
                .contains("cost unavailable: 1 requests unpriced")
        );
    }

    #[test]
    fn authoritative_live_occupancy_drives_and_clears_footer_state() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
    fn effort_selection_does_not_rebuild_or_change_transcript_presentation() {
        let session_id = SessionId::new();
        let history = [shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "stable transcript".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let before_items = app.transcript().iter().cloned().collect::<Vec<_>>();
        let before_frame = app.frame_observation();
        let before_scroll = app.scroll_offset();
        let before_visible = app.reasoning_visible();

        app.apply_reasoning_selection(Some("high".to_owned()), None);

        assert_eq!(
            app.transcript().iter().cloned().collect::<Vec<_>>(),
            before_items
        );
        assert_eq!(app.frame_observation(), before_frame);
        assert_eq!(app.scroll_offset(), before_scroll);
        assert_eq!(app.reasoning_visible(), before_visible);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn reasoning_changed_event_preserves_transcript_frame() {
        let session_id = SessionId::new();
        let history = [shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "stable transcript".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        app.observations.last_transcript_damage = TranscriptDocumentDamage::None;
        let before = app.frame_observation();

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            2,
            SessionEventKind::ReasoningChanged {
                effort: Some("high".to_owned()),
                summary: None,
                model_scope: None,
            },
        ));

        assert_eq!(app.frame_observation(), before);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn model_status_reasoning_update_preserves_transcript_frame() {
        let session_id = SessionId::new();
        let history = [shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "stable transcript".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        app.observations.last_transcript_damage = TranscriptDocumentDamage::None;
        let before = app.frame_observation();
        app.apply_model_status(bcode_model::SessionModelStatus {
            provider_plugin_id: None,
            requested_model_id: None,
            effective_model_id: None,
            model_id: None,
            display_name: None,
            context_window: None,
            context_occupancy: None,
            request_context_error: None,
            auth_profile: None,
            context_format_version: None,
            compatibility_key: None,
            max_output_tokens: None,
            reasoning: Some(bcode_model::ModelReasoningInfo {
                effort_values: vec!["low".to_owned(), "high".to_owned()],
                ..bcode_model::ModelReasoningInfo::default()
            }),
            reasoning_effort: Some("high".to_owned()),
            reasoning_summary: None,
            prompt_cache_mode: None,
            conversation_reuse_mode: None,
            compaction_mode: None,
            compaction_backend: None,
            proactive_compaction_threshold_percent: None,
            proactive_compaction_threshold_tokens: None,
            proactive_compaction_effective_threshold_tokens: None,
            proactive_compaction_threshold_source: None,
            cache: None,
            metadata_source: None,
            pricing: None,
        });

        assert_eq!(app.frame_observation(), before);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort.as_deref(), Some("high"));
    }

    #[test]
    fn unchanged_reasoning_visibility_is_a_strict_presentation_noop() {
        let session_id = SessionId::new();
        let history = [shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::AssistantReasoningMessage {
                text: "stable reasoning".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let before_items = app.transcript().iter().cloned().collect::<Vec<_>>();

        app.set_reasoning_visible(app.reasoning_visible());

        let after_items = app.transcript().iter().cloned().collect::<Vec<_>>();
        drop(app);
        assert_eq!(after_items, before_items);
    }

    #[test]
    fn full_effort_range_cycles_two_wraps_without_frame_changes() {
        let session_id = SessionId::new();
        let history = [shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::AssistantMessage {
                text: "stable transcript".to_owned(),
            },
        )];
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        app.reasoning_effort_values = ["none", "minimal", "low", "medium", "high", "xhigh", "max"]
            .into_iter()
            .map(ToOwned::to_owned)
            .collect();
        app.apply_reasoning_selection(Some("none".to_owned()), None);
        app.observations.last_transcript_damage = TranscriptDocumentDamage::None;
        let frame = app.frame_observation();
        let expected = [
            "minimal", "low", "medium", "high", "xhigh", "max", "none", "minimal", "low", "medium",
            "high", "xhigh", "max", "none",
        ];

        let observed = expected.map(|_| {
            let effort = app.cycle_pending_reasoning_effort().map(str::to_owned);
            (effort, app.frame_observation())
        });
        drop(app);
        for ((effort, observed_frame), expected) in observed.into_iter().zip(expected) {
            assert_eq!(effort.as_deref(), Some(expected));
            assert_eq!(observed_frame, frame);
        }
    }

    #[test]
    fn subset_effort_range_skips_unsupported_values_and_wraps() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_reasoning_effort("high".to_owned());

        app.clear_pending_reasoning_effort_for_session_change();

        assert_eq!(app.pending_reasoning_effort_generation(), None);
        let effort = app.reasoning_effort().map(str::to_owned);
        drop(app);
        assert_eq!(effort, None);
    }

    #[test]
    fn canonical_status_clears_only_matching_pending_reasoning() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

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
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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
    fn model_turn_finish_consumes_shared_runtime_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::ModelTurnStarted {
                turn_id: "turn-shared".to_owned(),
            },
        ));
        assert!(matches!(
            app.activity(),
            ActivityState::PreparingModelRequest
        ));

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            2,
            SessionEventKind::ModelTurnFinished {
                turn_id: "turn-shared".to_owned(),
                outcome: ModelTurnOutcome::Error,
                message: Some("provider failed".to_owned()),
            },
        ));

        let runtime = &app.session_view_snapshot().runtime;
        assert_eq!(runtime.active_turn_id, None);
        assert_eq!(runtime.last_turn_outcome, Some(ModelTurnOutcome::Error));
        assert_eq!(
            runtime.last_turn_message.as_deref(),
            Some("provider failed")
        );
        assert_eq!(app.status(), "provider failed");
        assert!(matches!(app.activity(), ActivityState::Idle));

        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(2))
            .expect("shared model-turn error item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal error item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn session_rename_status_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

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
    fn working_directory_status_consumes_shared_projection() {
        let session_id = SessionId::new();
        let old_working_directory = std::path::PathBuf::from("/tmp/bcode-old");
        let new_working_directory = std::path::PathBuf::from("/tmp/bcode-old/subdir");
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory: old_working_directory.clone(),
                new_working_directory: new_working_directory.clone(),
            },
        ));

        assert_eq!(
            app.working_directory(),
            Some(new_working_directory.as_path())
        );
        assert_eq!(
            app.status(),
            format!(
                "working directory: {}",
                display(&new_working_directory, &old_working_directory)
            )
        );
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(1))
            .expect("shared working-directory item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn context_compaction_clears_occupancy_through_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let occupancy = bcode_session_models::RequestContextOccupancy {
            context_epoch: 3,
            observation_sequence: 7,
            observation: snapshot(true, 2_500),
        };
        app.apply_context_occupancy(Some(occupancy));

        app.absorb_session_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 8,
            timestamp_ms: 8,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ContextCompacted {
                compacted_through_sequence: 7,
                summary: "summary".to_owned(),
            },
        });

        assert!(
            app.session_view_snapshot()
                .runtime
                .context_occupancy
                .is_none()
        );
        let terminal = app
            .transcript()
            .iter()
            .last()
            .expect("compaction item")
            .clone();
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(8))
            .expect("shared compaction item");
        let expected = terminal_item_from_shared(shared);
        drop(app);
        assert_eq!(terminal.role(), "Compaction");
        assert_eq!(terminal.text(), "local context compaction: summary");
        assert_eq!(terminal.role(), expected.role());
        assert_eq!(terminal.text(), expected.text());
        assert_eq!(terminal.kind(), expected.kind());
    }

    #[test]
    fn provider_context_compaction_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);

        app.absorb_session_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 9,
            timestamp_ms: 9,
            session_id,
            provenance: None,
            kind: bcode_session_models::SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence: 8,
                snapshot: bcode_session_models::ProviderContextSnapshot {
                    format_version: 1,
                    request_fingerprint: None,
                    request_id: None,
                    provider_plugin_id: "provider".to_owned(),
                    model_id: "model".to_owned(),
                    compatibility_key: "compat".to_owned(),
                    auth_profile: None,
                    origin: bcode_session_models::ProviderContextSnapshotOrigin::Explicit,
                    messages_json: "[]".to_owned(),
                    portable_summary: "summary".to_owned(),
                },
            },
        });

        let terminal = app
            .transcript()
            .iter()
            .last()
            .expect("provider compaction item")
            .clone();
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(9))
            .expect("shared provider compaction item");
        let expected = terminal_item_from_shared(shared);
        drop(app);
        assert_eq!(terminal.role(), "Compaction");
        assert_eq!(
            terminal.text(),
            "explicit provider-native context compaction (provider)"
        );
        assert_eq!(terminal.role(), expected.role());
        assert_eq!(terminal.text(), expected.text());
        assert_eq!(terminal.kind(), expected.kind());
    }

    #[test]
    fn user_message_commit_consumes_shared_projection_text() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);
        app.replace_composer_with("shared user");
        app.stage_submission();
        assert_eq!(app.pending_submissions().len(), 1);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::UserMessage {
                client_id: bcode_session_models::ClientId::new(),
                text: "shared user".to_owned(),
                admission: bcode_session_models::TurnAdmissionMetadata::default(),
            },
        ));

        assert_eq!(app.pending_submissions(), []);
        assert_eq!(
            app.shared_terminal_text_for_sequence(1, "You").as_deref(),
            Some("shared user")
        );
        let entry = app
            .timeline_entries()
            .into_iter()
            .next()
            .expect("timeline entry");
        assert_eq!(entry.text(), "shared user");
        let actual = app.transcript().iter().last().expect("user item").clone();
        drop(app);
        assert_eq!(actual.role(), "You");
        assert_eq!(actual.text(), "shared user");
        assert_eq!(actual.event_sequence(), Some(1));
    }

    #[test]
    fn assistant_streaming_delta_consumes_shared_projection_text() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::AssistantDelta {
                text: "shared ".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::AssistantDelta {
                text: "stream".to_owned(),
            },
        ));

        assert_eq!(
            app.latest_shared_streaming_terminal_item("Assistant")
                .as_ref()
                .map(TranscriptItem::text),
            Some("shared stream")
        );
        let actual = app.transcript().iter().last().expect("assistant item");
        assert_eq!(actual.role(), "Assistant");
        assert_eq!(actual.text(), "shared stream");
        assert!(actual.streaming());
        let item_count = app.transcript().len();
        drop(app);
        assert_eq!(item_count, 1);
    }

    #[test]
    fn assistant_final_message_consumes_shared_projection_text() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::AssistantDelta {
                text: "draft".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::AssistantMessage {
                text: "final shared".to_owned(),
            },
        ));

        assert_eq!(
            app.latest_shared_terminal_text("Assistant").as_deref(),
            Some("final shared")
        );
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("assistant item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), "Assistant");
        assert_eq!(actual.text(), "final shared");
        assert!(!actual.streaming());
    }

    #[test]
    fn assistant_stream_identity_survives_interleaved_usage_and_finalization() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let live = |text: &str| SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::AssistantTextDelta {
                turn_id: "turn-1".to_owned(),
                segment_id: "segment-0".to_owned(),
                segment_order: 0,
                text: text.to_owned(),
            },
        };

        app.absorb_session_live_event(&live("cargo check passed "));
        let initial = app
            .transcript()
            .iter()
            .find(|item| item.role() == "Assistant")
            .expect("streaming assistant item");
        let render_id = initial.id();
        let source_id = initial
            .source_view_item_id()
            .expect("shared source identity")
            .clone();

        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            1,
            SessionEventKind::ModelUsage {
                turn_id: "turn-1".to_owned(),
                usage: bcode_session_models::SessionTokenUsage {
                    input_tokens: Some(6_154),
                    output_tokens: Some(22),
                    ..bcode_session_models::SessionTokenUsage::default()
                },
            },
        ));
        app.absorb_session_live_event(&live("successfully"));
        app.absorb_session_event(&shared_projection_adapter_event(
            session_id,
            2,
            SessionEventKind::AssistantMessage {
                text: "cargo check passed successfully".to_owned(),
            },
        ));

        let assistants = app
            .transcript()
            .iter()
            .filter(|item| item.role() == "Assistant")
            .collect::<Vec<_>>();
        assert_eq!(assistants.len(), 1);
        assert_eq!(assistants[0].text(), "cargo check passed successfully");
        assert!(!assistants[0].streaming());
        assert_eq!(assistants[0].id(), render_id);
        assert_eq!(assistants[0].source_view_item_id(), Some(&source_id));
        let metered_total = app
            .session_view_snapshot()
            .runtime
            .latest_usage
            .as_ref()
            .and_then(bcode_session_models::SessionTokenUsage::metered_total_tokens);
        drop(app);
        assert_eq!(metered_total, Some(6_176));
    }

    #[test]
    fn live_reasoning_rich_updates_preserve_markdown_projection_and_finalize_once() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        app.set_reasoning_visible(true);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::AssistantReasoningDelta {
                text: "## Why\n\n- **safe**".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::AssistantReasoningDelta {
                text: "\n- fast".to_owned(),
            },
        ));

        let reasoning = app
            .transcript()
            .iter()
            .filter(|item| item.role().starts_with("Reasoning"))
            .collect::<Vec<_>>();
        assert_eq!(reasoning.len(), 1);
        assert!(reasoning[0].streaming());
        assert_eq!(
            reasoning[0].text_format(),
            bcode_session_view_models::TextFormat::Markdown
        );
        let streaming_rows = crate::render::transcript_item_rows_from_item(
            reasoning[0],
            48,
            None,
            bcode_config::TuiDiffViewerConfig::default(),
        );
        let streaming_body = streaming_rows[1..].to_vec();
        let text = streaming_rows
            .iter()
            .flat_map(|line| &line.spans)
            .map(|span| span.content.as_str())
            .collect::<String>();
        assert!(text.contains("Why"));
        assert!(text.contains("safe"));
        assert!(text.contains("fast"));

        app.absorb_session_event(&event(
            3,
            SessionEventKind::AssistantReasoningMessage {
                text: "## Why\n\n- **safe**\n- fast".to_owned(),
            },
        ));
        let reasoning = app
            .transcript()
            .iter()
            .filter(|item| item.role().starts_with("Reasoning"))
            .collect::<Vec<_>>();
        assert_eq!(reasoning.len(), 1);
        assert!(!reasoning[0].streaming());
        let finalized_rows = crate::render::transcript_item_rows_from_item(
            reasoning[0],
            48,
            None,
            bcode_config::TuiDiffViewerConfig::default(),
        );
        drop(app);
        assert_eq!(streaming_body, finalized_rows[1..]);
    }

    #[test]
    fn reasoning_final_message_consumes_shared_projection_text() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::AssistantReasoningDelta {
                text: "draft reasoning".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::AssistantReasoningMessage {
                text: "final reasoning".to_owned(),
            },
        ));

        assert_eq!(
            app.latest_shared_terminal_text("Reasoning summary")
                .as_deref(),
            Some("final reasoning")
        );
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("reasoning item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), "Reasoning summary");
        assert_eq!(actual.text(), "final reasoning");
        assert!(!actual.streaming());
    }

    #[test]
    fn split_reasoning_stream_consumes_shared_projection_without_aggregate_overwrite() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(
            1,
            SessionEventKind::AssistantReasoningDelta {
                text: "first thought".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::SystemMessage {
                text: "tool output".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            3,
            SessionEventKind::AssistantReasoningDelta {
                text: "second thought".to_owned(),
            },
        ));
        app.absorb_session_event(&event(
            4,
            SessionEventKind::AssistantReasoningMessage {
                text: "first thought second thought final aggregate".to_owned(),
            },
        ));

        let terminal = app.transcript().iter().collect::<Vec<_>>();
        assert_eq!(terminal.len(), 3);
        assert_eq!(terminal[0].role(), "Reasoning summary");
        assert_eq!(terminal[0].text(), "first thought");
        assert!(!terminal[0].streaming());
        assert_eq!(terminal[1].role(), "System");
        assert_eq!(terminal[1].text(), "tool output");
        assert_eq!(terminal[2].role(), "Reasoning summary");
        assert_eq!(terminal[2].text(), "second thought");
        assert!(!terminal[2].streaming());
        let shared_items = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .map(terminal_item_from_shared)
            .map(|item| (item.role(), item.text().to_owned(), item.streaming()))
            .collect::<Vec<_>>();
        let terminal_items = terminal
            .iter()
            .map(|item| (item.role(), item.text().to_owned(), item.streaming()))
            .collect::<Vec<_>>();
        drop(app);
        assert_eq!(shared_items, terminal_items);
    }

    #[test]
    fn environment_context_is_hidden_in_terminal_updates_and_reopen() {
        let session_id = SessionId::new();
        let events = [
            shared_projection_adapter_event(
                session_id,
                1,
                SessionEventKind::SystemMessage {
                    text: format!(
                        "{}Current directory: /workspace",
                        bcode_session_models::TURN_ENVIRONMENT_SNAPSHOT_PREFIX
                    ),
                },
            ),
            shared_projection_adapter_event(
                session_id,
                2,
                SessionEventKind::SystemMessage {
                    text: "ordinary visible status".into(),
                },
            ),
        ];
        let mut live = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        for event in &events {
            live.absorb_session_event(event);
        }
        let replayed = BmuxApp::new_with_history(Some(session_id), &events, &[], false);
        for app in [&live, &replayed] {
            let items = app.transcript().iter().collect::<Vec<_>>();
            assert_eq!(items.len(), 1);
            assert_eq!(items[0].text(), "ordinary visible status");
        }
        drop(live);
        drop(replayed);
    }

    #[test]
    fn generic_terminal_items_are_adapted_from_shared_projection() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let events = shared_projection_terminal_adapter_events(SessionId::new());
        for event in &events {
            app.absorb_session_event(event);
        }

        let terminal = app.transcript().iter().collect::<Vec<_>>();
        assert_eq!(
            terminal.len(),
            app.session_view_snapshot().transcript.items.len()
        );
        for (terminal, shared) in terminal
            .iter()
            .zip(&app.session_view_snapshot().transcript.items)
        {
            let expected = terminal_item_from_shared(shared);
            assert_eq!(terminal.role(), expected.role());
            assert_eq!(terminal.text(), expected.text());
            assert_eq!(terminal.kind(), expected.kind());
            assert_eq!(terminal.event_sequence(), shared.sequence);
        }
    }

    #[test]
    fn active_tool_loop_follows_shared_tool_status() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
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

    #[test]
    fn tool_request_state_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));

        let tool = app
            .session_view_snapshot()
            .tools
            .get("tool-shared")
            .expect("shared tool view");
        assert_eq!(tool.tool_name.as_deref(), Some("shell.run"));
        assert_eq!(
            app.activity(),
            &ActivityState::RunningTool {
                name: tool.tool_name.clone().expect("tool name")
            }
        );
        assert_eq!(
            app.status(),
            tool_request_status(tool.arguments_json.as_deref().expect("arguments"))
                .unwrap_or_else(|| "started".to_owned())
        );
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(1))
            .expect("shared tool item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal tool item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn tool_result_status_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::ToolInvocationResultRecorded {
                record: bcode_session_models::ToolInvocationResultRecord {
                    invocation_id: "tool-shared".to_owned(),
                    model_output: "shared failure".to_owned(),
                    is_error: true,
                    presentation: None,
                    result: Some(ToolInvocationResult::Text {
                        text: "shared failure".to_owned(),
                    }),
                    content: Vec::new(),
                },
            },
        ));

        let tool = app
            .session_view_snapshot()
            .tools
            .get("tool-shared")
            .expect("shared tool view");
        assert_eq!(tool.is_error, Some(true));
        assert_eq!(app.status(), "failed");
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                        if tool.tool_call_id == "tool-shared"
                )
            })
            .expect("shared tool item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal tool item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn finished_tool_result_uses_shared_terminal_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);
        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
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

        let terminal = app.transcript().iter().collect::<Vec<_>>();
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                        if tool.tool_call_id == "tool-shared"
                            && matches!(
                                tool.status,
                                bcode_session_view_models::ToolInvocationViewStatus::Finished
                                    | bcode_session_view_models::ToolInvocationViewStatus::Cancelled
                                    | bcode_session_view_models::ToolInvocationViewStatus::Failed
                            )
                )
            })
            .expect("shared finished tool item");
        let expected = terminal_item_from_shared(shared);
        let actual = (*terminal.last().expect("terminal result item")).clone();
        let sequence = shared.sequence;
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.event_sequence(), sequence);
    }

    #[test]
    fn permission_request_activity_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_permission_request_kind()));

        let permission = app
            .shared_permission_view("permission-shared")
            .expect("shared permission view");
        assert_eq!(permission.tool_name, "shell.run");
        assert_eq!(
            app.activity(),
            &ActivityState::WaitingPermission {
                name: permission.tool_name.clone()
            }
        );
        assert_eq!(
            app.status(),
            tool_request_status(&permission.arguments_json)
                .unwrap_or_else(|| permission.tool_name.clone())
        );
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(1))
            .expect("shared permission item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal item")
            .clone();
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn resolved_permission_result_uses_shared_terminal_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);
        app.absorb_session_event(&event(1, shared_permission_request_kind()));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::PermissionResolved {
                permission_id: "permission-shared".to_owned(),
                approved: true,
            },
        ));

        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::Permission { permission }
                        if permission.permission_id == "permission-shared" && permission.resolved
                )
            })
            .expect("shared resolved permission item");
        let expected = terminal_item_from_shared(shared);
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal permission item")
            .clone();
        let sequence = shared.sequence;
        drop(app);
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.event_sequence(), sequence);
    }

    fn shared_projection_terminal_adapter_events(session_id: SessionId) -> Vec<SessionEvent> {
        [
            shared_projection_adapter_event(
                session_id,
                1,
                SessionEventKind::UserMessage {
                    client_id: bcode_session_models::ClientId::new(),
                    text: "shared user".to_owned(),
                    admission: bcode_session_models::TurnAdmissionMetadata::default(),
                },
            ),
            shared_projection_adapter_event(
                session_id,
                2,
                SessionEventKind::SystemMessage {
                    text: "shared system".to_owned(),
                },
            ),
            shared_projection_adapter_event(
                session_id,
                3,
                SessionEventKind::ModelUsage {
                    turn_id: "turn-1".to_owned(),
                    usage: bcode_session_models::SessionTokenUsage {
                        input_tokens: Some(2),
                        output_tokens: Some(3),
                        ..bcode_session_models::SessionTokenUsage::default()
                    },
                },
            ),
            shared_projection_adapter_event(session_id, 4, shared_tool_call_request_kind()),
            shared_projection_adapter_event(session_id, 5, shared_permission_request_kind()),
            shared_projection_adapter_event(
                session_id,
                7,
                SessionEventKind::ModelTurnFinished {
                    turn_id: "turn-shared".to_owned(),
                    outcome: ModelTurnOutcome::Error,
                    message: Some("provider failed".to_owned()),
                },
            ),
            shared_projection_adapter_event(session_id, 8, shared_tool_contribution_kind()),
            shared_projection_adapter_event(session_id, 9, shared_plugin_status_note_kind()),
        ]
        .into()
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

    fn shared_permission_request_kind() -> SessionEventKind {
        SessionEventKind::PermissionRequested {
            permission_id: "permission-shared".to_owned(),
            tool_call_id: "tool-shared".to_owned(),
            producer_plugin_id: Some("bcode.shell".to_owned()),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"printf shared"}"#.to_owned(),
            batch: None,
            policy_source: Some("ask".to_owned()),
            policy_reason: Some("needs confirmation".to_owned()),
        }
    }

    fn shared_tool_contribution_kind() -> SessionEventKind {
        SessionEventKind::ToolContribution {
            event: bcode_session_models::ToolContributionEvent {
                invocation_id: "tool-shared".to_owned(),
                contribution_id: "status".to_owned(),
                sequence: 1,
                producer_id: "bcode.test".to_owned(),
                schema: "bcode.test.status".to_owned(),
                schema_version: 1,
                operation: bcode_session_models::ToolContributionOperation::Upsert,
                persistence: bcode_session_models::ToolContributionPersistence::Durable,
                artifact: None,
                payload: serde_json::json!({"status": "ok"}),
            },
        }
    }

    fn shared_plugin_status_note_kind() -> SessionEventKind {
        SessionEventKind::PluginStatusNote {
            plugin_id: "bcode.test".to_owned(),
            note_id: "note-1".to_owned(),
            text: "plugin status".to_owned(),
            metadata: BTreeMap::new(),
        }
    }

    #[test]
    fn unknown_contribution_uses_terminal_generic_json_fallback() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.absorb_session_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: "call".to_owned(),
                    contribution_id: "surface".to_owned(),
                    sequence: 1,
                    producer_id: "future.producer".to_owned(),
                    schema: "future.unknown/schema".to_owned(),
                    schema_version: 77,
                    operation: bcode_session_models::ToolContributionOperation::Append,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"sentinel": "opaque-tui"}),
                },
            },
        });
        let transcript_empty = app.transcript().is_empty();
        drop(app);
        assert!(transcript_empty);
    }

    #[test]
    fn transient_contribution_updates_and_removes_one_live_fallback() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let live = |sequence, operation, sentinel| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContributionPlaced {
                envelope: bcode_session_models::ToolContributionEnvelope::new(
                    bcode_session_models::ToolContributionPlacement::Hidden,
                    bcode_session_models::ToolContributionEvent {
                        invocation_id: "call".to_owned(),
                        contribution_id: "surface".to_owned(),
                        sequence,
                        producer_id: "future.producer".to_owned(),
                        schema: "future.unknown/schema".to_owned(),
                        schema_version: 77,
                        operation,
                        persistence: bcode_session_models::ToolContributionPersistence::Transient,
                        artifact: None,
                        payload: serde_json::json!({"sentinel": sentinel}),
                    },
                ),
            },
        };

        app.absorb_session_live_event(&live(
            1,
            bcode_session_models::ToolContributionOperation::Upsert,
            "first",
        ));
        app.absorb_session_live_event(&live(
            2,
            bcode_session_models::ToolContributionOperation::Append,
            "second",
        ));
        assert!(app.transcript().is_empty());
        assert_eq!(
            app.session_view_snapshot().contributions["call:surface"].sequence,
            2
        );

        app.absorb_session_live_event(&live(
            3,
            bcode_session_models::ToolContributionOperation::Remove,
            "removed",
        ));
        app.absorb_session_live_event(&live(
            2,
            bcode_session_models::ToolContributionOperation::Upsert,
            "stale",
        ));
        let transcript_empty = app.transcript().is_empty();
        drop(app);
        assert!(transcript_empty);
    }

    #[test]
    fn legacy_shell_contribution_has_no_terminal_json_fallback() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.absorb_session_event(&bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: 1,
            timestamp_ms: 1,
            session_id: bcode_session_models::SessionId::new(),
            provenance: None,
            kind: SessionEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
                    invocation_id: "shell-call".to_owned(),
                    contribution_id: "shell-run-summary".to_owned(),
                    sequence: 1,
                    producer_id: "bcode.shell".to_owned(),
                    schema: "bcode.shell.run.summary".to_owned(),
                    schema_version: 1,
                    operation: bcode_session_models::ToolContributionOperation::Upsert,
                    persistence: bcode_session_models::ToolContributionPersistence::Durable,
                    artifact: None,
                    payload: serde_json::json!({"output": "shell-render-sentinel"}),
                },
            },
        });
        let transcript_empty = app.transcript().is_empty();
        drop(app);
        assert!(transcript_empty);
    }
}

/// Local monitor presentation and keyboard ownership.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildPanelPresentation {
    Hidden,
    Composer,
    Monitor,
}

/// Whether `candidate`'s override belongs to the subtree rooted at `root`:
/// the root itself, or a section of a unit root.
fn target_contains(
    root: &super::structure::DisclosureTarget,
    candidate: &super::structure::DisclosureTarget,
) -> bool {
    use super::structure::DisclosureTarget;
    root == candidate
        || matches!(
            (root, candidate),
            (DisclosureTarget::Unit(unit), DisclosureTarget::Section(item, _)) if unit == item
        )
}

const fn disclosure_label(
    disclosure: bcode_presentation_policy::models::Disclosure,
) -> &'static str {
    use bcode_presentation_policy::models::Disclosure;
    match disclosure {
        Disclosure::Expanded => "expanded",
        Disclosure::Summary => "summarized",
        Disclosure::Collapsed => "collapsed",
        Disclosure::Hidden => "hidden",
    }
}

#[cfg(test)]
pub mod disclosure_focus_tests {
    use bcode_presentation_policy::models::Disclosure;
    use bcode_session_models::{SessionEvent, SessionEventKind, SessionId};

    use super::BmuxApp;
    use crate::structure::DisclosureTarget;
    use crate::transcript_focus::FocusAction;

    fn tool_event(session_id: SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind: SessionEventKind::ToolCallRequested {
                tool_call_id: format!("call-{sequence}"),
                producer_plugin_id: None,
                tool_name: "read".to_owned(),
                arguments_json: r#"{"path":"src/lib.rs"}"#.to_owned(),
                working_directory: None,
            },
        }
    }

    /// Three tool units whose retained layout anchors their headers, plus an
    /// `output` section label on the middle unit.
    pub fn focus_app(tools: u64) -> (BmuxApp, Vec<u64>) {
        let session_id = SessionId::new();
        let history: Vec<_> = (1..=tools)
            .map(|sequence| tool_event(session_id, sequence))
            .collect();
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let ids: Vec<u64> = app
            .transcript()
            .iter()
            .map(|item| item.id().get())
            .collect();
        let entries: Vec<_> = ids
            .iter()
            .enumerate()
            .map(|(index, id)| {
                let mut anchors = vec![(0, DisclosureTarget::Unit(*id).to_hit_id())];
                if index == 1 {
                    anchors.push((
                        2,
                        DisclosureTarget::Section(*id, "output".to_owned()).to_hit_id(),
                    ));
                }
                (4, anchors)
            })
            .collect();
        *app.transcript_layout_mut() = crate::transcript_focus::tests::layout(&entries);
        app.set_transcript_viewport_for_details_test(entries.len() * 4, 40, 0);
        (app, ids)
    }

    fn disclosure(app: &BmuxApp, target: &DisclosureTarget) -> Option<Disclosure> {
        app.target_disclosure(target)
            .map(|(disclosure, _)| disclosure)
    }

    #[test]
    fn focus_mode_walks_targets_and_moves_disclosure_one_level_at_a_time() {
        let (mut app, ids) = focus_app(3);
        assert_eq!(ids.len(), 3);
        let unit = |index: usize| DisclosureTarget::Unit(ids[index]);
        let output = DisclosureTarget::Section(ids[1], "output".to_owned());
        assert!(!app.transcript_focus_active());
        assert!(!app.handle_transcript_focus_action(FocusAction::Next));

        assert!(app.enter_transcript_focus());
        assert!(app.transcript_focus_active());
        assert_eq!(app.focused_disclosure_target(), Some(&unit(2)));
        assert!(app.status().contains("UNIT FOCUS"));

        assert!(app.handle_transcript_focus_action(FocusAction::Previous));
        assert_eq!(app.focused_disclosure_target(), Some(&output));
        // The focused item's index is recorded, so painting the focus row
        // locates it without scanning; a stale hint still resolves correctly.
        assert_eq!(app.transcript_focus.entry_hint(), Some(1));
        assert_eq!(app.disclosure_target_row(&output), Some(4 + 2));
        app.transcript_focus.set_entry_hint(Some(0));
        assert_eq!(app.disclosure_target_row(&output), Some(4 + 2));
        app.transcript_focus.set_entry_hint(Some(99));
        assert_eq!(app.disclosure_target_row(&output), Some(4 + 2));
        // `h` collapses the section, then climbs to its unit.
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(disclosure(&app, &output), Some(Disclosure::Collapsed));
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(app.focused_disclosure_target(), Some(&unit(1)));

        assert!(app.handle_transcript_focus_action(FocusAction::Toggle));
        assert_eq!(disclosure(&app, &unit(1)), Some(Disclosure::Collapsed));
        // No group holds the unit, so there is no parent to climb to.
        assert!(!app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(app.focused_disclosure_target(), Some(&unit(1)));

        assert!(app.handle_transcript_focus_action(FocusAction::Expand));
        assert_eq!(disclosure(&app, &unit(1)), Some(Disclosure::Expanded));
        assert!(!app.handle_transcript_focus_action(FocusAction::Expand));

        // `e` reopens the unit together with its collapsed section.
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert!(app.handle_transcript_focus_action(FocusAction::ExpandAll));
        assert_eq!(disclosure(&app, &unit(1)), Some(Disclosure::Expanded));
        assert_eq!(disclosure(&app, &output), Some(Disclosure::Expanded));
        assert!(!app.handle_transcript_focus_action(FocusAction::ExpandAll));

        assert!(app.handle_transcript_focus_action(FocusAction::Previous));
        assert_eq!(app.focused_disclosure_target(), Some(&unit(0)));
        assert!(!app.handle_transcript_focus_action(FocusAction::Previous));

        assert!(app.handle_transcript_focus_action(FocusAction::Exit));
        assert!(!app.transcript_focus_active());
        assert_eq!(app.focused_disclosure_target(), None);
    }

    #[test]
    fn bulk_actions_act_on_the_focused_subtree_only_in_focus_mode() {
        let (mut app, ids) = focus_app(3);
        let unit = |index: usize| DisclosureTarget::Unit(ids[index]);
        let output = DisclosureTarget::Section(ids[1], "output".to_owned());
        assert!(app.enter_transcript_focus());
        app.handle_transcript_focus_action(FocusAction::Previous);
        app.handle_transcript_focus_action(FocusAction::Collapse);
        app.handle_transcript_focus_action(FocusAction::Collapse);
        assert_eq!(app.focused_disclosure_target(), Some(&unit(1)));

        assert!(app.set_all_disclosure(Disclosure::Collapsed));
        assert_eq!(disclosure(&app, &unit(1)), Some(Disclosure::Collapsed));
        for other in [0, 2] {
            assert_eq!(disclosure(&app, &unit(other)), Some(Disclosure::Expanded));
        }
        // Reset in focus mode clears the unit and its section only.
        assert!(app.toggle_target(&unit(2)));
        assert!(app.reset_disclosure());
        assert_eq!(disclosure(&app, &unit(1)), Some(Disclosure::Expanded));
        assert_eq!(disclosure(&app, &output), Some(Disclosure::Expanded));
        assert_eq!(disclosure(&app, &unit(2)), Some(Disclosure::Collapsed));
        assert!(!app.reset_disclosure());

        assert!(app.exit_transcript_focus());
        assert!(app.set_all_disclosure(Disclosure::Collapsed));
        for index in 0..3 {
            assert_eq!(disclosure(&app, &unit(index)), Some(Disclosure::Collapsed));
        }
        assert!(app.reset_disclosure());
        for index in 0..3 {
            assert_eq!(disclosure(&app, &unit(index)), Some(Disclosure::Expanded));
        }
    }

    #[test]
    fn targets_toggle_from_legacy_hit_ids_and_ungrouped_units_have_no_group() {
        let (mut app, ids) = focus_app(2);
        let legacy = DisclosureTarget::from_hit_id(&format!("disclosure:{}", ids[0]))
            .expect("legacy hit id parses");
        assert!(app.toggle_target(&legacy));
        assert_eq!(
            disclosure(&app, &DisclosureTarget::Unit(ids[0])),
            Some(Disclosure::Collapsed)
        );
        // The default policy groups nothing, so no group target exists.
        let group = DisclosureTarget::Group(ids[0]);
        assert_eq!(
            app.sync_structure_groups(),
            std::collections::BTreeSet::new()
        );
        assert_eq!(disclosure(&app, &group), None);
        assert!(!app.toggle_target(&group));
        assert_eq!(app.items_for_target(&group), Vec::<usize>::new());
        assert_eq!(app.group_of_item(ids[1]), None);

        assert!(!app.toggle_target(&DisclosureTarget::Unit(u64::MAX)));
        assert!(!app.toggle_target(&DisclosureTarget::Group(u64::MAX)));
    }

    fn named_tool_event(session_id: SessionId, sequence: u64, tool: &str) -> SessionEvent {
        let mut event = tool_event(session_id, sequence);
        if let SessionEventKind::ToolCallRequested { tool_name, .. } = &mut event.kind {
            tool.clone_into(tool_name);
        }
        event
    }

    /// An app whose `read` tools group (`same_tool`) under `group_rule`,
    /// followed by `bash` tools that do not group, laid out at width 80.
    fn grouped_app(reads: u64, bashes: u64, group_rule: &str) -> (BmuxApp, Vec<u64>) {
        let session_id = SessionId::new();
        let history: Vec<_> = (1..=reads + bashes)
            .map(|sequence| {
                named_tool_event(
                    session_id,
                    sequence,
                    if sequence <= reads { "read" } else { "bash" },
                )
            })
            .collect();
        let mut app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        let rules = format!(
            "[[rules]]\nmatch = {{ kind = \"tool\", tool = \"read\" }}\nset = {{ group = \"consecutive_same_tool\" }}\n{group_rule}"
        );
        app.apply_configured_structure_rules(
            toml::from_str::<bcode_presentation_policy::models::PresentationSection>(&rules)
                .unwrap()
                .rules,
        );
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        let ids = app
            .transcript()
            .iter()
            .map(|item| item.id().get())
            .collect();
        (app, ids)
    }

    #[test]
    fn focus_moves_scroll_the_viewport_from_the_edge_they_approach() {
        use crate::transcript_focus::FocusAction;
        let (mut app, _) = grouped_app(0, 12, "");
        let total = app.transcript_layout().total_rows();
        let height = 6_u16;
        assert!(total > usize::from(height) * 2, "{total} rows");
        app.set_transcript_viewport_for_details_test(total, height, 0);
        assert!(app.enter_transcript_focus());
        let extent = |app: &BmuxApp| {
            let target = app.transcript_focus.focused().cloned().expect("focused");
            crate::transcript_focus::target_extent(&app.disclosure_anchors(), &target, None)
                .expect("focused extent")
        };

        // Walk up to the first target: every scroll lands the focused
        // target's first row on the top edge.
        let mut scrolled_up = false;
        for _ in 0..64 {
            let top_before = app.transcript_top_row(height);
            if !app.handle_transcript_focus_action(FocusAction::Previous) {
                break;
            }
            let top = app.transcript_top_row(height);
            let extent = extent(&app);
            assert!(extent.start >= top && extent.start < top + usize::from(height));
            if top != top_before {
                scrolled_up = true;
                assert_eq!(extent.start, top, "moving up hugs the top edge");
            }
        }
        assert!(scrolled_up, "moving up scrolled the viewport");
        assert_eq!(app.transcript_top_row(height), 0);

        // Walk back down: a scroll puts the focused target's last row on the
        // bottom edge (or its first row on top when it is taller than the
        // viewport).
        let mut scrolled_down = false;
        for _ in 0..64 {
            let top_before = app.transcript_top_row(height);
            if !app.handle_transcript_focus_action(FocusAction::Next) {
                break;
            }
            let top = app.transcript_top_row(height);
            let extent = extent(&app);
            if top != top_before {
                scrolled_down = true;
                let bottom = top + usize::from(height);
                assert!(
                    extent.end == bottom || extent.start == top,
                    "extent {extent:?} in {top}..{bottom}"
                );
            }
            assert!(extent.start >= top);
        }
        assert!(scrolled_down, "moving down scrolled the viewport");
    }

    #[test]
    fn group_headers_toggle_from_their_hit_ids_and_expose_their_members() {
        let collapsed =
            "[[rules]]\nmatch = { kind = \"group\" }\nset = { disclosure = \"collapsed\" }";
        let (mut app, ids) = grouped_app(3, 1, collapsed);
        let group = DisclosureTarget::Group(ids[0]);
        let source = crate::transcript_layout::VisibleTranscriptSource::Transcript;
        assert_eq!(
            app.transcript_layout()
                .content_anchor_row(0, &group.to_hit_id()),
            Some(0)
        );
        assert_eq!(app.transcript_layout().entry_row_count(source, 1), Some(0));
        assert_eq!(app.items_for_target(&group), [0, 1, 2]);
        assert_eq!(app.group_of_item(ids[2]), Some(ids[0]));
        assert_eq!(app.group_of_item(ids[3]), None);
        assert_eq!(disclosure(&app, &group), Some(Disclosure::Collapsed));

        let clicked = DisclosureTarget::from_hit_id(&group.to_hit_id()).expect("group hit id");
        assert!(app.toggle_target(&clicked));
        assert_eq!(disclosure(&app, &group), Some(Disclosure::Expanded));
        crate::transcript_projection::sync_layout_for_test(&mut app, 80);
        assert!(
            app.transcript_layout()
                .entry_row_count(source, 1)
                .is_some_and(|rows| rows > 0)
        );
        assert_eq!(
            app.transcript_layout()
                .content_anchor_row(1, &DisclosureTarget::Unit(ids[1]).to_hit_id()),
            Some(0)
        );
    }

    #[test]
    fn focus_climbs_from_a_member_to_its_group() {
        let (mut app, ids) = grouped_app(2, 0, "");
        let group = DisclosureTarget::Group(ids[0]);
        app.transcript_focus.enter(DisclosureTarget::Unit(ids[1]));
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(
            disclosure(&app, &DisclosureTarget::Unit(ids[1])),
            Some(Disclosure::Collapsed)
        );
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(app.focused_disclosure_target(), Some(&group));
        // Groups collapse one level at a time: expanded, summary, collapsed.
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(disclosure(&app, &group), Some(Disclosure::Summary));
        assert!(app.handle_transcript_focus_action(FocusAction::Collapse));
        assert_eq!(disclosure(&app, &group), Some(Disclosure::Collapsed));
        // `e` on the group opens it and every member under it.
        assert!(app.handle_transcript_focus_action(FocusAction::ExpandAll));
        assert_eq!(disclosure(&app, &group), Some(Disclosure::Expanded));
        assert_eq!(
            disclosure(&app, &DisclosureTarget::Unit(ids[1])),
            Some(Disclosure::Expanded)
        );
    }

    #[test]
    fn collapsing_a_group_above_the_viewport_keeps_the_reading_position() {
        let (mut app, ids) = grouped_app(6, 8, "");
        let body = bmux_tui::geometry::Rect::new(0, 0, 80, 12);
        crate::transcript_projection::prepare_for_body(&mut app, body);
        let height = crate::render::transcript_area_for_body(&app, body).height;
        let source = crate::transcript_layout::VisibleTranscriptSource::Transcript;
        let target_start = app
            .transcript_layout()
            .entry_start_row(source, 10)
            .expect("laid out");
        let total = app.transcript_layout().total_rows();
        let scroll_up = total
            .saturating_sub(usize::from(height))
            .saturating_sub(target_start);
        app.set_transcript_viewport_for_details_test(total, height, scroll_up);
        crate::transcript_projection::prepare_for_body(&mut app, body);
        let top_line = |app: &BmuxApp| {
            let top = app.transcript_top_row(height);
            app.transcript_layout()
                .visible_lines_from_top(top, height)
                .first()
                .map(|line| (ids[line.entry_index], line.row_in_entry))
        };
        let before = top_line(&app);
        let below_group = before
            .and_then(|(id, _)| ids.iter().position(|candidate| *candidate == id))
            .is_some_and(|index| index >= 6);
        assert!(below_group, "reading below the group: {before:?}");

        assert!(app.set_target_disclosure(&DisclosureTarget::Group(ids[0]), Disclosure::Collapsed));
        crate::transcript_projection::prepare_for_body(&mut app, body);
        assert!(
            app.transcript_layout().total_rows() < total,
            "the group folded"
        );
        assert_eq!(top_line(&app), before);
    }

    #[test]
    fn focus_mode_stays_closed_without_disclosure_targets() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        assert!(app.enter_transcript_focus());
        assert!(!app.transcript_focus_active());
        assert!(!app.handle_transcript_focus_action(FocusAction::Toggle));
        assert!(!app.exit_transcript_focus());
        drop(app);
    }
}

#[cfg(test)]
mod structure_layer_tests {
    use bcode_presentation_policy::models::{
        Disclosure, FactMatcher, LayerSource, OneOrMany, PresentationRule, PropertyPatch,
        RuleLayer, Spacing, UnitFacts, UnitKind,
    };

    use super::BmuxApp;

    fn rule(matcher: FactMatcher, set: PropertyPatch) -> PresentationRule {
        PresentationRule { matcher, set }
    }

    fn tool(tool: &str, producer: &str) -> UnitFacts {
        UnitFacts {
            kind: Some(UnitKind::Tool),
            tool: Some(tool.to_owned()),
            producer: Some(producer.to_owned()),
            ..UnitFacts::default()
        }
    }

    #[test]
    fn layers_stack_host_density_then_plugin_then_theme() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let spacing = |before, after| Spacing { before, after };
        let density = crate::theme::chrome::UnitChrome {
            density: bcode_tui_theme::chrome::Density::Compact,
            ..crate::theme::chrome::UnitChrome::default()
        }
        .density_layer()
        .expect("compact density layer");
        let theme = RuleLayer {
            source: LayerSource::Theme("test".to_owned()),
            rules: vec![rule(
                FactMatcher {
                    tool: OneOrMany(vec!["x".to_owned()]),
                    ..FactMatcher::default()
                },
                PropertyPatch {
                    disclosure: Some(Disclosure::Expanded),
                    ..PropertyPatch::default()
                },
            )],
        };
        // The theme resolution supplies the density layer ahead of theme layers.
        app.theme_structure_layers = vec![density, theme];
        app.plugin_structure_layers = vec![
            bcode_plugin::PluginPresentationDeclaration {
                tools: Vec::new(),
                rules: vec![rule(
                    FactMatcher::default(),
                    PropertyPatch {
                        disclosure: Some(Disclosure::Collapsed),
                        spacing: Some(spacing(1, 1)),
                        ..PropertyPatch::default()
                    },
                )],
                ..bcode_plugin::PluginPresentationDeclaration::default()
            }
            .rule_layer("p"),
        ];
        app.recompile_structure();
        let snapshot = app.structure_snapshot();

        // Plugin defaults override host density for the plugin's own units...
        let own = snapshot.resolve(&tool("y", "p"));
        assert_eq!(own.disclosure, Disclosure::Collapsed);
        assert_eq!(own.spacing, spacing(1, 1));
        // ...and theme rules override plugin defaults.
        assert_eq!(
            snapshot.resolve(&tool("x", "p")).disclosure,
            Disclosure::Expanded
        );
        // Other producers' units see only host density.
        let other = snapshot.resolve(&tool("y", "q"));
        assert_eq!(other.spacing, spacing(0, 0));
        assert_ne!(other.disclosure, Disclosure::Collapsed);
        drop(app);
    }

    #[test]
    fn apps_without_plugins_add_no_plugin_layers() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        assert!(!app.sync_plugin_structure());
        assert_eq!(app.plugin_structure_layers, Vec::new());
        drop(app);
    }

    /// Manifest with a plugin default rule appended to the bundled shell
    /// manifest (`[[presentation.rules]]` may follow any table).
    const SHELL_WITH_RULES: &str = concat!(
        include_str!("../../../plugins/shell-plugin/bcode-plugin.toml"),
        "\n[[presentation.rules]]\nmatch = { kind = \"tool\" }\nset = { disclosure = \"collapsed\" }\n",
    );

    /// A production (runtime-backed) presentation over `plugins`.
    fn runtime_presentation(
        plugins: &[bcode_plugin::StaticBundledPlugin],
    ) -> std::sync::Arc<crate::plugin_tui::PluginTuiPresentation> {
        let selected = bcode_plugin::filter_selected_static_plugins(
            plugins,
            &bcode_plugin::PluginSelection::all_enabled(),
        )
        .expect("bundled manifests parse");
        let host = bcode_plugin::PluginHost::load_static_plugins_best_effort(&selected);
        std::sync::Arc::new(
            crate::plugin_tui::PluginTuiPresentation::with_runtime_config_and_extensions(
                host.into(),
                bcode_config::TuiVisualAdapterConfig::default(),
                &[],
            ),
        )
    }

    #[test]
    fn plugin_catalog_changes_recompile_plugin_layers_and_target_fields() {
        let catalog = [
            bcode_plugin::StaticBundledPlugin::new(
                include_str!("../../../plugins/filesystem-plugin/bcode-plugin.toml"),
                bcode_filesystem_plugin::static_plugin(),
            ),
            bcode_plugin::StaticBundledPlugin::new(
                SHELL_WITH_RULES,
                bcode_shell_plugin::static_plugin(),
            ),
        ];
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let shell_unit = tool("shell.run", "bcode.shell");
        // `filesystem.grep` declares `pattern` before `path`; host fields try
        // `path` first, so the target shows which fields applied.
        let grep_target = |app: &BmuxApp| {
            app.structure
                .snapshot()
                .facts(&bcode_tui_components::transcript::item::tool_request_item(
                    "call",
                    Some("bcode.filesystem"),
                    "filesystem.grep",
                    r#"{"pattern":"todo","path":"src"}"#,
                    None,
                ))
                .and_then(|facts| facts.target)
        };

        app.set_plugin_presentation(runtime_presentation(&[]));
        assert_eq!(app.plugin_structure_layers, Vec::new());
        let before = app.structure_snapshot().resolve(&shell_unit).disclosure;
        assert_ne!(before, Disclosure::Collapsed);
        assert_eq!(grep_target(&app).as_deref(), Some("src"));

        // A later catalog brings plugins with rules and target fields.
        let revision = app.structure.revision();
        app.set_plugin_presentation(runtime_presentation(&catalog));
        assert_eq!(
            app.plugin_structure_layers
                .iter()
                .map(|layer| layer.source.clone())
                .collect::<Vec<_>>(),
            [LayerSource::Plugin("bcode.shell".to_owned())]
        );
        assert!(app.structure.revision() > revision);
        assert_eq!(
            app.structure_snapshot().resolve(&shell_unit).disclosure,
            Disclosure::Collapsed
        );
        assert_eq!(grep_target(&app).as_deref(), Some("todo"));

        // Reloading an identical catalog re-derives and recompiles nothing.
        let revision = app.structure.revision();
        app.plugin_presentation = Some(runtime_presentation(&catalog));
        assert!(!app.sync_plugin_structure());
        assert_eq!(app.structure.revision(), revision);

        // Dropping the plugins removes their layer and fields again.
        app.set_plugin_presentation(runtime_presentation(&[]));
        assert_eq!(app.plugin_structure_layers, Vec::new());
        assert_eq!(
            app.structure_snapshot().resolve(&shell_unit).disclosure,
            before
        );
        assert_eq!(grep_target(&app).as_deref(), Some("src"));
        drop(app);
    }
}
