//! TUI app state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bcode_config::{
    TuiAccentTransitionCurve, TuiConfig, TuiDiffViewerConfig, TuiDiffViewerLayout, TuiThemeConfig,
    TuiThinkingConfig,
};
use bcode_plugin_sdk::path::{display, display_from_current_dir};
use bcode_session_models::{
    LiveToolArgumentPreview, ModelTurnOutcome, ProviderStreamEvent, RuntimeWorkStatus,
    SessionEvent, SessionEventKind, SessionHistoryCursor, SessionId, SessionInputHistoryEntry,
    SessionLiveEvent, SessionLiveEventKind, SessionTraceEvent, SessionTracePayload,
    SessionTracePhase, ToolInvocationProjection, ToolInvocationResult, ToolInvocationStreamEvent,
    ToolOutputStream, apply_tool_invocation_projection_event,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LiveToolPreviewState {
    pub tool_name: String,
    pub argument_bytes: usize,
    pub preview: LiveToolArgumentPreview,
    pub working_directory: Option<std::path::PathBuf>,
    pub revision: u64,
    pub snapshots_received: u64,
    pub duplicates_skipped: u64,
    pub truncated_snapshots: u64,
}
use bcode_skill_models::SkillSource;
use bmux_text_edit::{SelectionMode, TextEditBuffer, TextMotion};
use bmux_tui::event::MouseEvent;
use bmux_tui::geometry::Rect;
use bmux_tui::style::Color;
use bmux_tui_components::text_input::{
    TextInputControl, TextInputOutcome, TextInputPolicy, TextInputState,
};

use super::activity::{ActivityState, model_turn_outcome_label};
use super::cursor_blink::CursorBlink;
use super::exit_state::ExitState;
use super::input_history::{InputHistory, InputHistoryOutcome};
use super::invalidation::{InvalidationKey, InvalidationRequest, UiInvalidation};
use super::keymap::{BmuxAction, BmuxKeyActivation, BmuxKeyBinding, BmuxScope};
use super::older_history::OlderHistoryState;
use super::pending_submission::PendingSubmission;
use super::pending_submissions::PendingSubmissions;
use super::theme::{PresentedTheme, ResolvedTheme};
use super::timeline_dialog::TimelineEntry;
use super::tool_render_projection::semantic_result_supersedes_live_preview;
use super::transcript::{
    TranscriptItem, TranscriptItemKind, display_tool_result_text,
    generic_tool_result_item_from_projection, live_tool_preview_anchor_item, model_usage_item,
    permission_result_item, semantic_tool_result_item_from_raw, streaming_tool_output_item,
    streaming_tool_visual_item, terminal_item_from_shared, tool_request_item_from_projection,
    tool_result_item,
};
use super::transcript_document::TranscriptDocument;
use super::transcript_layout::{TranscriptLayoutCache, VisibleTranscriptSource};
use super::transcript_resident_window::{TranscriptResidentWindow, TranscriptWindowPolicy};
use super::transcript_viewport::TranscriptViewport;

const MANUAL_TRANSCRIPT_SCROLL_GRACE: Duration = Duration::from_millis(450);
const LIVE_PREVIEW_FRAME_INVALIDATION_KEY: &str = "live-preview-frame";
const LIVE_PREVIEW_FRAME_INTERVAL: Duration = Duration::from_millis(16);
const TRANSCRIPT_SCROLL_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const TRANSCRIPT_SCROLL_ANIMATION_FRAME: Duration = Duration::from_millis(16);
const TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY: &str = "transcript-scroll-animation";
const LATEST_BAR_ANIMATION_INVALIDATION_KEY: &str = "latest-bar-animation";
const THEME_TRANSITION_INVALIDATION_KEY: &str = "theme-transition";
const THEME_TRANSITION_FRAME: Duration = Duration::from_millis(16);
const LATEST_BAR_ACTIVE_WINDOW: Duration = Duration::from_millis(420);
const TOOL_ELAPSED_INVALIDATION_PREFIX: &str = "tool-elapsed";
const TOOL_ELAPSED_INVALIDATION_MAX_INTERVAL: Duration = Duration::from_secs(1);
const RESIDENT_TRANSCRIPT_MAX_EVENTS: usize = 1_024;
const RESIDENT_TRANSCRIPT_TARGET_EVENTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct LivePreviewFrameState {
    dirty: bool,
    next_frame_at: Option<Instant>,
}

impl LivePreviewFrameState {
    const fn new() -> Self {
        Self {
            dirty: false,
            next_frame_at: None,
        }
    }
}

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TranscriptScrollAnimation {
    start_top_row: usize,
    target_top_row: usize,
    started_at: Instant,
    duration: Duration,
}

impl TranscriptScrollAnimation {
    const fn new(start_top_row: usize, target_top_row: usize, started_at: Instant) -> Self {
        Self {
            start_top_row,
            target_top_row,
            started_at,
            duration: TRANSCRIPT_SCROLL_ANIMATION_DURATION,
        }
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
        }
    }

    fn set_target(&mut self, target: Color, config: TuiThemeConfig, now: Instant) {
        if self.target_accent == target {
            self.displayed_accent = self.accent_at(now);
            return;
        }
        let duration_ms = config.effective_accent_transition_ms();
        if duration_ms == 0 {
            self.displayed_accent = target;
            self.source_accent = target;
            self.target_accent = target;
            self.started_at = now;
            self.duration = Duration::ZERO;
            self.curve = config.accent_transition_curve;
            return;
        }
        self.source_accent = self.accent_at(now);
        self.displayed_accent = self.source_accent;
        self.target_accent = target;
        self.started_at = now;
        self.duration = Duration::from_millis(duration_ms);
        self.curve = config.accent_transition_curve;
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

/// State owned by the terminal user interface.
#[derive(Debug, Clone)]
pub struct BmuxApp {
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
    theme_transition: ThemeTransitionState,
    thinking_label: String,
    reasoning_support: ReasoningSupport,
    reasoning_default_effort: Option<String>,
    reasoning_default_summary: Option<String>,
    token_usage: TokenUsageMeter,
    composer: TextInputState,
    input_history: InputHistory,
    transcript: TranscriptDocument,
    session_view: bcode_session_view::SessionView,
    transcript_window: TranscriptResidentWindow,
    latest_history_sequence: Option<u64>,
    tool_call_contexts: BTreeMap<String, ToolCallContext>,
    tool_invocation_projections: BTreeMap<String, ToolInvocationProjection>,
    streamed_tool_results: BTreeMap<String, StreamedToolResultContext>,
    active_artifact_revisions: BTreeMap<(String, String, String), u64>,
    transient_contribution_items:
        BTreeMap<String, (u64, Option<crate::transcript::TranscriptItemId>)>,
    live_tool_previews: BTreeMap<String, LiveToolPreviewState>,
    live_preview_revision: u64,
    live_preview_frames_requested: u64,
    live_preview_duplicates_skipped: u64,
    live_preview_truncated_snapshots: u64,
    pending_submissions: PendingSubmissions,
    transcript_layout: TranscriptLayoutCache,
    viewport: TranscriptViewport,
    manual_transcript_scroll_until: Option<Instant>,
    transcript_scroll_animation: Option<TranscriptScrollAnimation>,
    scroll_mode: TranscriptScrollMode,
    pending_visual_overflow_bottom: Option<usize>,
    latest_hidden_activity_at: Option<Instant>,
    latest_hidden_activity_burst: u8,
    latest_bar_animation_started_at: Instant,
    submitted_user_message_following: SubmittedUserMessageFollowing,
    assistant_scroll_anchor: AssistantScrollAnchorState,
    active_tool_calls: BTreeSet<String>,
    active_plugin_visuals: BTreeMap<String, bcode_session_models::PluginVisualDescriptor>,
    tool_activity_seen: bool,
    pending_assistant_stream_anchor: bool,
    pending_transcript_top_anchor_sequence: Option<u64>,
    older_history: OlderHistoryState,
    activity: ActivityState,
    activity_started_at: Instant,
    daemon_connection: DaemonConnectionState,
    status: String,
    key_hints: String,
    jump_to_latest_key_label: String,
    tui_config: TuiConfig,
    diff_viewer_layout_override: Option<TuiDiffViewerLayout>,
    exit: ExitState,
    cursor: CursorBlink,
    live_preview_frame: LivePreviewFrameState,
    pending_key_activation: Option<PendingKeyActivation>,
    plugin_presentation: Option<Arc<crate::plugin_tui::PluginTuiPresentation>>,
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
enum TranscriptScrollMode {
    #[default]
    BottomFollow,
    TransitionToEntry {
        sticky: bool,
    },
    AnchoredToEntry {
        sticky: bool,
    },
    ManualDetached,
}

impl TranscriptScrollMode {
    const fn allows_overflow_catch(self) -> bool {
        matches!(
            self,
            Self::BottomFollow
                | Self::TransitionToEntry { sticky: false }
                | Self::AnchoredToEntry { sticky: false }
        )
    }

    const fn allows_assistant_stream_anchor(self) -> bool {
        !matches!(self, Self::ManualDetached)
    }
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
    Pending {
        index: usize,
    },
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
            Self::Pending { index } | Self::Anchored { index } | Self::Interrupted { index } => {
                Some(index)
            }
        }
    }

    const fn is_pending(self) -> bool {
        matches!(self, Self::Pending { .. })
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallContext {
    tool_name: String,
    arguments_json: String,
    working_directory: Option<std::path::PathBuf>,
    request_visual: Option<bcode_session_models::PluginVisualDescriptor>,
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
enum FinishedStreamedToolOutput {
    PlainToolResult,
    Visual,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct StreamedToolResultContext {
    index: Option<usize>,
    columns: u16,
    rows: u16,
    started_at_ms: Option<u64>,
    saw_output: bool,
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

impl BmuxApp {
    /// Create TUI state with replayed session data.
    #[must_use]
    pub fn new_with_history(
        session_id: Option<SessionId>,
        history: &[SessionEvent],
        input_history: &[SessionInputHistoryEntry],
        has_older_history: bool,
    ) -> Self {
        let now = Instant::now();
        let initial_theme = ResolvedTheme {
            accent: super::theme::PENDING_AGENT_METADATA_ACCENT,
        };
        let mut app = Self {
            session_id,
            selected_auth_profile: None,
            selected_context_format_version: None,
            selected_compatibility_key: None,
            current_agent_accent: None,
            pending_agent_id: None,
            pending_agent_accent: None,
            agent_metadata_hydration: AgentMetadataHydration::Pending,
            target_theme: initial_theme,
            presented_theme: initial_theme.into(),
            theme_transition: ThemeTransitionState::new(initial_theme.accent, now),
            thinking_label: "reasoning output shown · unsupported".to_owned(),
            reasoning_support: ReasoningSupport::Unsupported,
            reasoning_default_effort: None,
            reasoning_default_summary: None,
            token_usage: TokenUsageMeter::default(),
            composer: TextInputState::new(TextEditBuffer::new()),
            input_history: InputHistory::from_entries(input_history),
            transcript: TranscriptDocument::default(),
            session_view: bcode_session_view::SessionView::new(),
            transcript_window: TranscriptResidentWindow::default(),
            latest_history_sequence: None,
            tool_call_contexts: BTreeMap::new(),
            tool_invocation_projections: BTreeMap::new(),
            streamed_tool_results: BTreeMap::new(),
            active_artifact_revisions: BTreeMap::new(),
            transient_contribution_items: BTreeMap::new(),
            live_tool_previews: BTreeMap::new(),
            live_preview_revision: 0,
            live_preview_frames_requested: 0,
            live_preview_duplicates_skipped: 0,
            live_preview_truncated_snapshots: 0,
            pending_submissions: PendingSubmissions::default(),
            transcript_layout: TranscriptLayoutCache::default(),
            viewport: TranscriptViewport::default(),
            manual_transcript_scroll_until: None,
            transcript_scroll_animation: None,
            scroll_mode: TranscriptScrollMode::BottomFollow,
            pending_visual_overflow_bottom: None,
            latest_hidden_activity_at: None,
            latest_hidden_activity_burst: 0,
            latest_bar_animation_started_at: now,
            submitted_user_message_following: SubmittedUserMessageFollowing::Idle,
            assistant_scroll_anchor: AssistantScrollAnchorState::Idle,
            active_tool_calls: BTreeSet::new(),
            active_plugin_visuals: BTreeMap::new(),
            tool_activity_seen: false,
            pending_assistant_stream_anchor: false,
            pending_transcript_top_anchor_sequence: None,
            older_history: OlderHistoryState::new(history, has_older_history),
            activity: ActivityState::Idle,
            activity_started_at: now,
            daemon_connection: DaemonConnectionState::Connecting,
            status: String::from("Connecting to daemon… Enter submits; Esc/Ctrl-C exits."),
            key_hints: String::from("enter send · escape interrupt · ctrl+d exit · ctrl+p palette"),
            jump_to_latest_key_label: "ctrl+end".to_owned(),
            tui_config: TuiConfig::default(),
            diff_viewer_layout_override: None,
            exit: ExitState::default(),
            cursor: CursorBlink::new(),
            live_preview_frame: LivePreviewFrameState::new(),
            pending_key_activation: None,
            plugin_presentation: None,
        };
        app.absorb_history(history);
        app
    }

    /// Move app-level presentation/runtime state from another app after recreating session state.
    pub(crate) fn take_cross_session_state_from(&mut self, source: &Self) {
        self.apply_tui_config(source.tui_config().clone());
        self.set_daemon_connection(source.daemon_connection());
        self.set_agent_metadata_hydrated(source.is_agent_metadata_hydrated());
        self.plugin_presentation
            .clone_from(&source.plugin_presentation);
        self.take_theme_transition_state_from(source);
    }

    /// Set the local plugin runtime used for client-side presentation projection.
    #[cfg(test)]
    pub fn set_plugin_host(&mut self, host: Arc<bcode_plugin::PluginHost>) {
        self.plugin_presentation = Some(Arc::new(
            crate::plugin_tui::PluginTuiPresentation::from_shared(host),
        ));
    }

    /// Set persistent local plugin presentation state.
    pub fn set_plugin_presentation(
        &mut self,
        presentation: Arc<crate::plugin_tui::PluginTuiPresentation>,
    ) {
        self.plugin_presentation = Some(presentation);
    }

    /// Return the local plugin runtime used for client-side presentation projection.
    #[cfg(test)]
    #[must_use]
    pub fn plugin_host(&self) -> Option<&bcode_plugin::PluginHost> {
        self.plugin_presentation
            .as_deref()
            .map(crate::plugin_tui::PluginTuiPresentation::host)
    }

    /// Return persistent local plugin presentation state.
    #[must_use]
    pub fn plugin_presentation(&self) -> Option<&crate::plugin_tui::PluginTuiPresentation> {
        self.plugin_presentation.as_deref()
    }

    #[cfg(test)]
    #[must_use]
    pub(crate) const fn tool_invocation_projections(
        &self,
    ) -> &BTreeMap<String, ToolInvocationProjection> {
        &self.tool_invocation_projections
    }

    /// Return timeline entries for committed user messages.
    #[must_use]
    pub fn timeline_entries(&self) -> Vec<TimelineEntry> {
        self.input_history
            .entries()
            .iter()
            .map(|entry| {
                TimelineEntry::new(
                    self.transcript_index_for_sequence(entry.sequence),
                    entry.sequence,
                    entry.timestamp_ms,
                    entry.text.clone(),
                )
            })
            .collect()
    }

    pub fn transcript_index_for_sequence(&self, sequence: u64) -> Option<usize> {
        self.transcript
            .iter()
            .position(|item| item.event_sequence() == Some(sequence))
    }

    /// Replace the resident transcript with a bounded replay window.
    pub fn replace_transcript_window(
        &mut self,
        events: &[SessionEvent],
        has_older: bool,
        has_newer: bool,
        anchor_sequence: u64,
    ) {
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.older_history
            .replace_centered(events, has_older, has_newer, anchor_sequence);
        self.rebuild_transcript_from_history();
        self.reconcile_tool_state_with_resident_transcript();
        self.pending_visual_overflow_bottom = None;
    }

    /// Defer top-anchoring a transcript event sequence until the layout cache is current.
    pub const fn request_transcript_top_anchor_sequence(&mut self, sequence: u64) {
        self.pending_transcript_top_anchor_sequence = Some(sequence);
        self.transcript_scroll_animation = None;
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
    }

    /// Jump to a committed transcript item and top-anchor it in the viewport.
    pub fn jump_to_transcript_index(&mut self, index: usize) -> bool {
        let Some(top_row) = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)
        else {
            return false;
        };
        self.transcript_scroll_animation = None;
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.viewport.follow_anchor(top_row);
        true
    }

    /// Return the active session id, if one was provided.
    #[must_use]
    pub const fn session_id(&self) -> Option<SessionId> {
        self.session_id
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

    /// Apply canonical session metadata from an attach/list response.
    ///
    /// Uses `SessionSummary::title()` as the single source of truth for the
    /// resolved display name (`name` → `explicit_name` → `derived_title`).
    /// This ensures that bounded first-event discovery on the server side
    /// (which may populate `explicit_name` or `derived_title`) is reflected
    /// in the TUI cache without duplicating fallback logic.
    pub fn apply_session_summary(&mut self, summary: &bcode_session_models::SessionSummary) {
        self.session_id = Some(summary.id);
        self.session_view.set_session_summary(summary.clone());
    }

    /// Apply terminal UI configuration.
    pub fn apply_tui_config(&mut self, config: TuiConfig) {
        self.apply_thinking_config(config.thinking);
        self.tui_config = config;
        self.sync_theme_target(Instant::now());
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
        self.presented_theme = PresentedTheme { accent };
        UiInvalidation::Paint
    }

    /// Return the current animated accent color for rendering.
    #[cfg(test)]
    pub fn animated_accent(&mut self, target_accent: Color, now: Instant) -> Color {
        self.theme_transition
            .set_target(target_accent, self.tui_config.theme, now);
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
        self.theme_transition
            .set_target(target.accent, self.tui_config.theme, now);
        self.update_theme_animation(now);
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

    /// Return the current reasoning output label.
    #[must_use]
    pub fn thinking_label(&self) -> &str {
        &self.thinking_label
    }

    /// Return the model label shown in the header.
    #[must_use]
    pub fn model_header_label(&self) -> String {
        let model = self.selected_model_id().unwrap_or("default");
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
        )
    }

    /// Return the composer content area from the latest render.
    #[must_use]
    pub const fn composer_content_area(&self) -> Rect {
        self.composer.content_area()
    }

    /// Store the composer content area from the latest render.
    pub fn set_composer_content_area(&mut self, area: Rect) {
        self.composer.set_content_area(area, &composer_policy());
    }

    /// Return the composer scroll offset that should be used for the latest content area.
    pub fn composer_scroll_offset_for_render(&self) -> usize {
        if self.composer.vertical_scroll() == usize::MAX {
            self.composer
                .cursor_scroll_offset(&composer_policy())
                .unwrap_or(0)
        } else {
            self.composer.vertical_scroll()
        }
    }

    /// Return the composer text input state.
    #[must_use]
    pub const fn composer_state(&self) -> &TextInputState {
        &self.composer
    }

    /// Return the composer buffer.
    #[must_use]
    pub const fn composer(&self) -> &TextEditBuffer {
        self.composer.buffer()
    }

    /// Return the composer buffer mutably.
    pub const fn composer_mut(&mut self) -> &mut TextEditBuffer {
        self.composer.buffer_mut()
    }

    /// Insert pasted text into the composer.
    pub fn paste_composer_text(&mut self, text: &str) {
        TextInputControl::new(&composer_policy()).handle_paste(&mut self.composer, text);
    }

    /// Return renderer-neutral semantic session state used for parity migration.
    #[cfg(test)]
    #[must_use]
    pub const fn session_view_snapshot(&self) -> &bcode_session_view_models::SessionViewSnapshot {
        self.session_view.snapshot()
    }

    /// Return transcript items.
    #[must_use]
    pub fn transcript(&self) -> &[TranscriptItem] {
        self.transcript.items()
    }

    /// Return revision for layout-affecting transcript collection changes.
    #[must_use]
    pub const fn transcript_projection_revision(&self) -> u64 {
        self.transcript
            .revision()
            .saturating_add(self.live_preview_revision)
            .saturating_add(self.live_preview_frames_requested)
    }

    /// Return revision for layout-affecting pending submission changes.
    #[must_use]
    pub const fn pending_submissions_projection_revision(&self) -> u64 {
        self.pending_submissions.revision()
    }

    /// Return live preview state by tool call id.
    #[must_use]
    pub const fn live_tool_previews(&self) -> &BTreeMap<String, LiveToolPreviewState> {
        &self.live_tool_previews
    }

    /// Extend composer selection with an editor motion.
    pub fn extend_composer_selection(&mut self, motion: TextMotion) {
        self.input_history.reset_navigation();
        let width = usize::from(self.composer.content_area().width.max(1));
        match motion {
            TextMotion::VisualUp => self.extend_composer_selection_to_visual_delta(width, -1),
            TextMotion::VisualDown => self.extend_composer_selection_to_visual_delta(width, 1),
            motion => self
                .composer
                .buffer_mut()
                .move_cursor_with_selection(motion, SelectionMode::Extend),
        }
        self.wake_cursor();
    }

    /// Handle a composer mouse event through the reusable text-input component.
    pub fn handle_composer_mouse(&mut self, mouse: MouseEvent) -> TextInputOutcome {
        let outcome =
            TextInputControl::new(&composer_policy()).handle_mouse(&mut self.composer, mouse);
        if matches!(outcome, TextInputOutcome::Edited | TextInputOutcome::Redraw) {
            self.input_history.reset_navigation();
            self.wake_cursor();
        }
        outcome
    }

    /// Return whether a composer mouse selection is active.
    #[must_use]
    pub const fn composer_mouse_selection_active(&self) -> bool {
        self.composer.mouse_selection_active()
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
        let width = usize::from(self.composer.content_area().width.max(1));
        let layout = self.composer.buffer().wrapped_layout(width);
        if layout.cursor.row == 0 {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer.buffer_mut().move_cursor_to_wrapped_position(
            width,
            layout.cursor.row.saturating_sub(1),
            layout.cursor.col,
        );
        self.wake_cursor();
        true
    }

    fn move_composer_visual_down_with_history_reset(&mut self, reset_history: bool) -> bool {
        let width = usize::from(self.composer.content_area().width.max(1));
        let layout = self.composer.buffer().wrapped_layout(width);
        if layout.cursor.row.saturating_add(1) >= layout.lines.len() {
            return false;
        }
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer.buffer_mut().move_cursor_to_wrapped_position(
            width,
            layout.cursor.row.saturating_add(1),
            layout.cursor.col,
        );
        self.wake_cursor();
        true
    }

    /// Apply restored session runtime selection to the app.
    pub fn apply_runtime_selection(&mut self, selection: bcode_ipc::SessionRuntimeSelection) {
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
            self.set_current_agent_id(agent_id);
        }
        self.refresh_thinking_label();
    }

    /// Apply hydrated model metadata to the app.
    pub fn apply_model_status(&mut self, status: bcode_ipc::SessionModelStatus) {
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
        );
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
                display_name: self.selected_model_id().unwrap_or_default().to_owned(),
                is_default: false,
                context_window: Some(context_window),
                max_output_tokens: status.max_output_tokens,
                capabilities: std::collections::BTreeSet::new(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: status.reasoning.clone(),
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: status.pricing.clone(),
                visibility: bcode_model::ModelVisibility::Visible,
            });
        self.token_usage.apply_model_info(model.as_ref());
        self.apply_context_occupancy(status.context_occupancy.map(|occupancy| *occupancy));
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

    /// Return the number of transcript rows hidden below the viewport.
    #[must_use]
    pub const fn scroll_offset(&self) -> usize {
        self.viewport.offset()
    }

    /// Return the number of virtual transcript rows below the newest content.
    #[must_use]
    pub const fn bottom_overscroll(&self) -> usize {
        self.viewport.bottom_overscroll()
    }

    /// Return whether there is a newer transcript entry fully below the viewport.
    #[must_use]
    pub fn newer_transcript_content_below(&self) -> bool {
        self.hidden_entry_start_row_below_viewport().is_some()
    }

    fn hidden_entry_start_row_below_viewport(&self) -> Option<usize> {
        let total_rows = self.transcript_layout.total_rows();
        let viewport_bottom = self.viewport.bottom_row(total_rows);
        if viewport_bottom >= total_rows {
            return None;
        }
        self.transcript_layout
            .first_entry_start_at_or_after_row(viewport_bottom)
    }

    /// Return the most recent time hidden transcript content changed.
    #[must_use]
    pub const fn latest_hidden_activity_at(&self) -> Option<Instant> {
        self.latest_hidden_activity_at
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

    /// Return the transcript row that should render at the top of the viewport.
    #[must_use]
    pub fn transcript_top_row(&self, viewport_height: u16) -> usize {
        if let Some(animation) = self.transcript_scroll_animation {
            return animation.top_row_at(Instant::now());
        }
        self.viewport
            .top_row(self.transcript_layout.total_rows(), viewport_height)
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

    /// Return resident tool-call context count.
    #[cfg(test)]
    pub fn resident_tool_call_context_count(&self) -> usize {
        self.tool_call_contexts.keys().count()
    }

    /// Return resident streamed tool result context count.
    #[cfg(test)]
    pub fn resident_streamed_tool_result_count(&self) -> usize {
        self.streamed_tool_results.keys().count()
    }

    /// Return whether older history may be available.
    #[must_use]
    pub const fn has_older_history(&self) -> bool {
        self.older_history.has_older_history()
    }

    /// Return whether an older-history request is in flight.
    #[must_use]
    pub const fn loading_older_history(&self) -> bool {
        self.older_history.loading()
    }

    /// Mark older history as loading or idle.
    pub const fn set_loading_older_history(&mut self, loading: bool) {
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

    /// Mark newer history as loading or idle.
    pub const fn set_loading_newer_history(&mut self, loading: bool) {
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
        self.session_view.set_plugin_status(plugin_status);
    }

    /// Return configured key hints for the status line.
    #[must_use]
    pub fn key_hints(&self) -> &str {
        &self.key_hints
    }

    /// Store configured key hints for the status line.
    pub fn set_key_hints(&mut self, key_hints: String) {
        self.key_hints = key_hints;
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

    /// Store the configured key label for jumping to latest transcript content.
    pub fn set_jump_to_latest_key_label(&mut self, key_label: String) {
        self.jump_to_latest_key_label = key_label;
    }

    /// Append a system-style transcript note.
    pub fn push_system_note(&mut self, text: String) {
        self.transcript.push(TranscriptItem::new("System", text));
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
        self.session_view.set_reasoning_visible(visible);
        self.refresh_thinking_label();
        self.rebuild_transcript_from_history();
    }

    /// Apply selected reasoning settings locally.
    pub fn apply_reasoning_selection(
        &mut self,
        effort: Option<String>,
        summary: Option<String>,
        visible: bool,
    ) {
        self.session_view.set_reasoning_selection(effort, summary);
        self.set_reasoning_visible(visible);
    }

    /// Return the selected reasoning effort, if any.
    #[must_use]
    pub fn reasoning_effort(&self) -> Option<&str> {
        self.session_view
            .snapshot()
            .runtime
            .reasoning_effort
            .as_deref()
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
        self.set_reasoning_visible(config.show);
    }

    fn refresh_thinking_label(&mut self) {
        let display = if self.reasoning_visible() {
            "reasoning output shown"
        } else {
            "reasoning output hidden"
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
        self.thinking_label = format!("{display} · effort: {effort} · visible summary: {summary}");
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
        let text = self.composer.buffer().text().to_owned();
        self.submitted_user_message_following = if text.trim().is_empty() {
            SubmittedUserMessageFollowing::Idle
        } else {
            SubmittedUserMessageFollowing::PendingAnchor
        };
        self.scroll_mode = TranscriptScrollMode::TransitionToEntry { sticky: false };
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.active_tool_calls.clear();
        self.active_plugin_visuals.clear();
        self.tool_activity_seen = false;
        self.pending_submissions.stage(text);
        self.input_history.reset_navigation();
        self.composer.buffer_mut().clear();
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
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
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
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Show the previous input-history entry, if available.
    pub fn previous_input_history(&mut self) -> bool {
        match self.input_history.previous(self.composer.buffer().text()) {
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
        self.cancel_transcript_scroll_animation_for_manual_scroll();
        self.mark_manual_transcript_scroll();
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.viewport.scroll_up(rows, &mut self.older_history)
    }

    /// Scroll transcript down by rendered rows.
    pub fn scroll_transcript_down(&mut self, rows: usize) -> bool {
        self.cancel_transcript_scroll_animation_for_manual_scroll();
        self.mark_manual_transcript_scroll();
        let changed = self.viewport.scroll_down(rows, &mut self.older_history);
        if self.viewport.at_bottom_threshold() {
            self.scroll_mode = TranscriptScrollMode::BottomFollow;
            self.latest_hidden_activity_at = None;
            self.latest_hidden_activity_burst = 0;
        } else {
            self.scroll_mode = TranscriptScrollMode::ManualDetached;
        }
        changed
    }

    /// Pin transcript to the newest rows.
    pub const fn scroll_transcript_to_bottom(&mut self) -> bool {
        self.transcript_scroll_animation = None;
        self.manual_transcript_scroll_until = None;
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
        self.latest_hidden_activity_at = None;
        self.latest_hidden_activity_burst = 0;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        self.viewport.scroll_to_bottom(&mut self.older_history)
    }

    /// Animate transcript to the newest rows.
    pub fn transition_transcript_to_bottom(&mut self) -> bool {
        self.manual_transcript_scroll_until = None;
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        let total_rows = self.transcript_layout.total_rows();
        let viewport_height = self.viewport.height();
        let start_top_row = self.viewport.top_row(total_rows, viewport_height);
        let target_top_row = total_rows.saturating_sub(usize::from(viewport_height));
        if start_top_row == target_top_row {
            return self.scroll_transcript_to_bottom();
        }
        self.transcript_scroll_animation = Some(TranscriptScrollAnimation::new(
            start_top_row,
            target_top_row,
            Instant::now(),
        ));
        true
    }

    fn cancel_transcript_scroll_animation_for_manual_scroll(&mut self) {
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.pending_assistant_stream_anchor = false;
        self.pending_visual_overflow_bottom = None;
        self.interrupt_current_assistant_anchor();
        let Some(animation) = self.transcript_scroll_animation.take() else {
            return;
        };
        let top_row = animation.top_row_at(Instant::now());
        self.viewport.materialize_top_row(top_row);
    }

    fn mark_manual_transcript_scroll(&mut self) {
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

    /// Sync cached rendered transcript scroll bounds from the latest frame.
    pub fn sync_transcript_scroll_max(
        &mut self,
        max_scroll_offset: usize,
        max_bottom_overscroll: usize,
        total_rows: usize,
        viewport_height: u16,
    ) {
        let now = Instant::now();
        if let Some(animation) = self.transcript_scroll_animation {
            let top_row = animation.top_row_at(now);
            if animation.finished(now) {
                self.transcript_scroll_animation = None;
                self.viewport.follow_anchor(animation.target_top_row);
                match self.scroll_mode {
                    TranscriptScrollMode::TransitionToEntry { sticky } => {
                        self.scroll_mode = TranscriptScrollMode::AnchoredToEntry { sticky };
                    }
                    TranscriptScrollMode::BottomFollow => {
                        self.latest_hidden_activity_at = None;
                        self.latest_hidden_activity_burst = 0;
                        self.viewport.scroll_to_bottom(&mut self.older_history);
                    }
                    TranscriptScrollMode::AnchoredToEntry { .. }
                    | TranscriptScrollMode::ManualDetached => {}
                }
            } else {
                self.viewport.materialize_top_row(top_row);
                self.transcript_scroll_animation = Some(animation);
            }
        }
        self.viewport.sync_max(
            max_scroll_offset,
            max_bottom_overscroll,
            total_rows,
            viewport_height,
            self.manual_transcript_scroll_active(),
            &mut self.older_history,
        );
        self.resolve_visual_overflow_follow(total_rows, now);
    }

    fn resolve_visual_overflow_follow(&mut self, total_rows: usize, now: Instant) {
        let Some(previous_bottom) = self.pending_visual_overflow_bottom.take() else {
            if !self.newer_transcript_content_below() {
                self.latest_hidden_activity_at = None;
                self.latest_hidden_activity_burst = 0;
            }
            return;
        };
        let hidden_entry_start = self.hidden_entry_start_row_below_viewport();
        let changed_hidden_entry_rows = hidden_entry_start.map_or(0, |entry_start| {
            total_rows.saturating_sub(previous_bottom.max(entry_start))
        });
        if changed_hidden_entry_rows > 0 {
            self.record_latest_hidden_activity(now, changed_hidden_entry_rows);
        }
        let overflowed = total_rows > previous_bottom;
        if self.manual_transcript_scroll_active()
            || self.transcript_scroll_animation.is_some()
            || !self.scroll_mode.allows_overflow_catch()
        {
            return;
        }
        if !overflowed {
            if !self.newer_transcript_content_below() {
                self.latest_hidden_activity_at = None;
                self.latest_hidden_activity_burst = 0;
            }
            return;
        }
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
        self.latest_hidden_activity_at = None;
        self.latest_hidden_activity_burst = 0;
        self.viewport.scroll_to_bottom(&mut self.older_history);
    }

    fn record_latest_hidden_activity(&mut self, now: Instant, changed_rows: usize) {
        let previous_activity_at = self.latest_hidden_activity_at;
        if previous_activity_at
            .is_none_or(|at| now.saturating_duration_since(at) >= LATEST_BAR_ACTIVE_WINDOW)
        {
            self.latest_hidden_activity_burst = 0;
        }
        let elapsed_ms = previous_activity_at
            .map_or_else(
                || LATEST_BAR_ACTIVE_WINDOW.as_millis(),
                |at| now.saturating_duration_since(at).as_millis(),
            )
            .max(1);
        self.latest_hidden_activity_at = Some(now);
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
                self.scroll_mode = TranscriptScrollMode::ManualDetached;
                self.viewport.follow_anchor(top_row);
            }
            return;
        }
        if self.submitted_user_message_following == SubmittedUserMessageFollowing::PendingAnchor {
            if let Some(top_row) = self.latest_user_message_start_row() {
                self.submitted_user_message_following = SubmittedUserMessageFollowing::Anchored;
                self.scroll_mode = TranscriptScrollMode::TransitionToEntry { sticky: false };
                self.start_transcript_scroll_animation(top_row);
            }
            return;
        }
        if self.pending_assistant_stream_anchor
            && let AssistantScrollAnchorState::Pending { index } = self.assistant_scroll_anchor
            && let Some(top_row) = self
                .transcript_layout
                .entry_start_row(VisibleTranscriptSource::Transcript, index)
        {
            self.start_transcript_scroll_animation(top_row);
            self.scroll_mode = TranscriptScrollMode::TransitionToEntry { sticky: true };
            self.assistant_scroll_anchor = AssistantScrollAnchorState::Anchored { index };
            self.pending_assistant_stream_anchor = false;
        }
    }

    const fn downgrade_sticky_entry_anchor(&mut self) {
        if matches!(
            self.scroll_mode,
            TranscriptScrollMode::AnchoredToEntry { sticky: true }
        ) {
            self.scroll_mode = TranscriptScrollMode::AnchoredToEntry { sticky: false };
        }
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

    fn should_anchor_new_assistant_stream(&self) -> bool {
        self.scroll_mode.allows_assistant_stream_anchor()
            && !self.manual_transcript_scroll_active()
            && self.transcript_scroll_animation.is_none()
            && self.submitted_user_message_following != SubmittedUserMessageFollowing::PendingAnchor
            && !self.assistant_scroll_anchor.is_pending()
    }

    fn maybe_request_assistant_stream_anchor(&mut self, should_anchor: bool) {
        let Some(index) = self
            .transcript
            .iter()
            .rposition(|item| item.role() == "Assistant" && item.streaming())
        else {
            return;
        };
        if self.assistant_scroll_anchor.index() == Some(index) {
            return;
        }
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
        if !should_anchor || self.active_tool_loop() {
            return;
        }
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Pending { index };
        self.pending_assistant_stream_anchor = true;
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
        if !self.active_tool_loop() {
            self.tool_activity_seen = false;
        }
    }

    /// Return active plugin visual descriptors by invocation id.
    #[must_use]
    pub fn active_plugin_visuals(
        &self,
    ) -> Vec<(String, bcode_session_models::PluginVisualDescriptor)> {
        self.active_plugin_visuals
            .iter()
            .map(|(tool_call_id, visual)| (tool_call_id.clone(), visual.clone()))
            .collect()
    }

    fn active_tool_loop(&self) -> bool {
        !self.active_tool_calls.is_empty()
    }

    fn start_transcript_scroll_animation(&mut self, top_row: usize) {
        if let Some((start_top_row, target_top_row)) =
            self.viewport.start_follow_anchor_animation(top_row)
        {
            self.transcript_scroll_animation = Some(TranscriptScrollAnimation::new(
                start_top_row,
                target_top_row,
                Instant::now(),
            ));
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
        self.session_view.apply_history(events);
        self.transcript_window.append_history(events);
        for event in events {
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.trim_resident_transcript_window_if_needed();
    }

    fn rebuild_transcript_from_history(&mut self) {
        let events = self.transcript_window.events().to_vec();
        self.session_view.rebuild_history_window(&events);
        self.transcript.replace(Vec::new());
        self.tool_call_contexts.clear();
        self.tool_invocation_projections.clear();
        self.live_tool_previews.clear();
        self.streamed_tool_results.clear();
        self.active_artifact_revisions.clear();
        self.active_tool_calls.clear();
        self.active_plugin_visuals.clear();
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
        if let Some(new_oldest_sequence) = trim.new_oldest_sequence {
            self.older_history
                .mark_dropped_history_before(new_oldest_sequence);
        }
        self.rebuild_transcript_from_history();
        self.reconcile_tool_state_with_resident_transcript();
        self.viewport.scroll_to_bottom(&mut self.older_history);
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

    fn can_trim_resident_transcript_window(&self) -> bool {
        matches!(self.scroll_mode, TranscriptScrollMode::BottomFollow)
            && self.viewport.at_bottom_threshold()
            && self.transcript_scroll_animation.is_none()
            && !self.manual_transcript_scroll_active()
            && self.active_tool_calls.is_empty()
            && !self.pending_assistant_stream_anchor
            && !self.older_history.loading()
    }

    fn reconcile_tool_state_with_resident_transcript(&mut self) {
        let referenced = referenced_tool_call_ids(self.transcript.items());
        self.tool_call_contexts
            .retain(|tool_call_id, _| referenced.contains(tool_call_id));
        self.tool_invocation_projections
            .retain(|tool_call_id, _| referenced.contains(tool_call_id));
        self.live_tool_previews
            .retain(|tool_call_id, _| referenced.contains(tool_call_id));
        self.streamed_tool_results.clear();
        self.active_artifact_revisions.clear();
    }

    /// Prepend older history and preserve the current viewport.
    pub fn prepend_older_history(&mut self, events: &[SessionEvent], has_more: bool) {
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
        self.rebuild_transcript_from_history();
        self.reconcile_tool_state_with_resident_transcript();
        self.older_history.update_cursor(events, has_more);
        self.older_history.set_loading(false);
        if self.older_history.has_older_history() {
            "loaded older history".clone_into(&mut self.status);
        } else {
            "start of session history".clone_into(&mut self.status);
        }
    }

    /// Append newer history and preserve bounded window state.
    pub fn append_newer_history(&mut self, events: &[SessionEvent], has_more: bool) {
        if events.is_empty() {
            self.older_history.update_newer_cursor(&[], false);
            self.older_history.set_loading_newer(false);
            "latest session history".clone_into(&mut self.status);
            return;
        }

        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.append_history(events);
        self.rebuild_transcript_from_history();
        self.reconcile_tool_state_with_resident_transcript();
        self.older_history.update_newer_cursor(events, has_more);
        self.older_history.set_loading_newer(false);
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
        self.trim_resident_transcript_window_if_needed();
    }

    /// Absorb one live-only session event.
    pub fn absorb_session_live_event(&mut self, event: &SessionLiveEvent) {
        self.session_view.apply_live_event(event);
        match &event.kind {
            SessionLiveEventKind::AssistantTextDelta { text, .. } => {
                let should_anchor = self.should_anchor_new_assistant_stream();
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.push_live_assistant_delta(text, SessionEventApplication::Live);
                self.maybe_request_assistant_stream_anchor(should_anchor);
            }
            SessionLiveEventKind::AssistantReasoningDelta { text, .. } => {
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.push_live_reasoning_delta(text, SessionEventApplication::Live);
            }
            SessionLiveEventKind::ToolOutputDelta { event } => {
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.apply_tool_stream_event(event, SessionEventApplication::Live);
            }
            SessionLiveEventKind::ToolContribution {
                event: contribution,
            } => self.apply_live_contribution(contribution),
            SessionLiveEventKind::ToolArgumentPreview {
                tool_call_id,
                tool_name,
                argument_bytes,
                preview,
                ..
            } => {
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.apply_live_tool_argument_preview(
                    tool_call_id,
                    tool_name,
                    *argument_bytes,
                    preview,
                );
            }
            SessionLiveEventKind::RequestContextOccupancyChanged { .. } => {}
            SessionLiveEventKind::ProviderStreamProgress { event, .. } => {
                self.apply_shared_provider_stream_progress(event);
            }
        }
    }

    fn apply_live_contribution(
        &mut self,
        contribution: &bcode_session_models::ToolContributionEvent,
    ) {
        let key = format!(
            "{}:{}",
            contribution.invocation_id, contribution.contribution_id
        );
        if self
            .transient_contribution_items
            .get(&key)
            .is_some_and(|(sequence, _)| contribution.sequence <= *sequence)
        {
            return;
        }
        let backs_active_visual = self.contribution_backs_active_visual(contribution);
        match contribution.operation {
            bcode_session_models::ToolContributionOperation::Remove => {
                if let Some((_, Some(id))) = self.transient_contribution_items.remove(&key) {
                    self.transcript.retain(|item| item.id() != id);
                }
                self.transient_contribution_items
                    .insert(key, (contribution.sequence, None));
            }
            bcode_session_models::ToolContributionOperation::Upsert
            | bcode_session_models::ToolContributionOperation::Append
                if backs_active_visual =>
            {
                if let Some((_, Some(id))) = self.transient_contribution_items.remove(&key) {
                    self.transcript.retain(|item| item.id() != id);
                }
                self.transient_contribution_items
                    .insert(key, (contribution.sequence, None));
            }
            bcode_session_models::ToolContributionOperation::Upsert
            | bcode_session_models::ToolContributionOperation::Append => {
                let shared_item = self.shared_tool_contribution_item(contribution);
                let text = shared_item.as_ref().map_or_else(
                    || {
                        serde_json::to_string_pretty(contribution)
                            .unwrap_or_else(|_| contribution.payload.to_string())
                    },
                    |item| item.text().to_owned(),
                );
                if let Some((_, Some(id))) = self.transient_contribution_items.get(&key).copied() {
                    self.transcript.mutate_rev_find(
                        |item| item.id() == id,
                        |item| item.replace_text(text.clone()),
                    );
                    self.transient_contribution_items
                        .insert(key, (contribution.sequence, Some(id)));
                } else {
                    let item = shared_item
                        .unwrap_or_else(|| TranscriptItem::new("Tool contribution", text));
                    let id = item.id();
                    self.transient_contribution_items
                        .insert(key, (contribution.sequence, Some(id)));
                    self.transcript.push(item);
                }
            }
        }
    }

    fn contribution_backs_active_visual(
        &self,
        contribution: &bcode_session_models::ToolContributionEvent,
    ) -> bool {
        let Some(artifact) = contribution.artifact.as_ref() else {
            return false;
        };
        let Some(visual) = self.active_plugin_visuals.get(&contribution.invocation_id) else {
            return false;
        };
        let Some(presentation) = self.plugin_presentation() else {
            return false;
        };
        let producer_plugin_id = visual
            .producer_plugin_id
            .as_deref()
            .unwrap_or(&contribution.producer_id);
        presentation.accepts_artifact_reference(
            producer_plugin_id,
            &visual.schema,
            visual.schema_version,
            &artifact.reference_key,
            artifact.content_type.as_deref(),
        )
    }

    #[allow(clippy::too_many_lines)]
    fn apply_session_event(&mut self, event: &SessionEvent, application: SessionEventApplication) {
        apply_tool_invocation_projection_event(&mut self.tool_invocation_projections, event);
        if event_breaks_sticky_entry_anchor(event) {
            self.downgrade_sticky_entry_anchor();
        }
        if event_affects_transcript_rows(event) {
            self.pending_visual_overflow_bottom = Some(
                self.viewport
                    .bottom_row(self.transcript_layout.total_rows()),
            );
        }
        let should_anchor = self.should_anchor_new_assistant_stream();
        if event_affects_transcript_rows(event) {
            self.viewport.preserve_for_append();
        }
        match &event.kind {
            SessionEventKind::UserMessage { text, .. } => {
                self.active_tool_calls.clear();
                self.active_plugin_visuals.clear();
                self.tool_activity_seen = false;
                self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
                self.pending_assistant_stream_anchor = false;
                self.push_committed_user_message(
                    event.sequence,
                    text,
                    event.timestamp_ms,
                    application,
                );
            }
            SessionEventKind::AssistantDelta { text } => {
                self.push_live_assistant_delta(text, application);
                self.maybe_request_assistant_stream_anchor(should_anchor);
            }
            SessionEventKind::AssistantMessage { text } => {
                let text = self
                    .latest_shared_terminal_text("Assistant")
                    .unwrap_or_else(|| text.clone());
                self.finish_streaming_item("Assistant", &text, application);
                self.finish_assistant_stream_anchor();
            }
            SessionEventKind::SystemMessage { .. } | SessionEventKind::PluginStatusNote { .. } => {
                self.push_shared_terminal_item(event.sequence);
            }
            SessionEventKind::ToolCallRequested {
                tool_call_id,
                tool_name,
                arguments_json,
                working_directory,
                request_visual,
                legacy_request_presentation: _legacy_legacy_request_presentation,
                ..
            } => {
                self.record_shared_active_tool_requested(tool_call_id);
                self.tool_activity_seen = true;
                self.push_tool_request(
                    (event.sequence, event.timestamp_ms),
                    tool_call_id,
                    tool_name,
                    arguments_json,
                    working_directory.clone(),
                    request_visual.as_ref(),
                );
            }
            SessionEventKind::ToolCallFinished {
                tool_call_id,
                result,
                is_error,
                semantic_result,
                ..
            } => {
                if application.live_activity() {
                    self.set_activity(ActivityState::PreparingFollowUpRequest);
                }
                self.finish_shared_active_tool_call(tool_call_id);
                self.push_tool_result(
                    tool_call_id,
                    result,
                    *is_error,
                    semantic_result.as_ref(),
                    application,
                );
            }
            SessionEventKind::ToolInvocationStream { event } => {
                self.apply_tool_stream_event(event, application);
            }
            SessionEventKind::PermissionRequested {
                permission_id,
                tool_call_id,
                tool_name,
                arguments_json,
                legacy_request_presentation: _legacy_legacy_request_presentation,
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
            SessionEventKind::ModelChanged { provider, model } => {
                self.apply_shared_model_changed(provider, model);
            }
            SessionEventKind::ReasoningChanged { .. } => {
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
                if matches!(
                    self.session_view.snapshot().runtime.last_turn_outcome,
                    Some(ModelTurnOutcome::Error)
                ) && !self.push_shared_terminal_item(event.sequence)
                {
                    let message = self
                        .session_view
                        .snapshot()
                        .runtime
                        .last_turn_message
                        .as_deref()
                        .unwrap_or("no details recorded")
                        .to_owned();
                    self.push_system_message(&format!("Model turn failed: {message}"));
                }
            }
            SessionEventKind::ModelUsage { turn_id, usage } => {
                self.push_model_usage(event.sequence, turn_id, usage, application);
            }
            SessionEventKind::ContextCompacted { summary, .. } => {
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.push_compaction(summary);
                }
            }
            SessionEventKind::ProviderContextCompacted { snapshot, .. } => {
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.push_provider_compaction(snapshot);
                }
            }
            SessionEventKind::WorkingDirectoryChanged {
                old_working_directory,
                new_working_directory: _,
            } => self.apply_working_directory_changed(event.sequence, old_working_directory),
            SessionEventKind::SessionRenamed { .. } => self.apply_shared_session_renamed(),
            SessionEventKind::SkillInvoked {
                skill_id,
                arguments,
                source,
                ..
            } => {
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.push_skill_invoked(skill_id, arguments, source.as_ref());
                }
            }
            SessionEventKind::SkillSuggested {
                skill_id, reason, ..
            } => {
                self.status = format!("suggested skill: {skill_id}");
                if reason.is_some() && !self.push_shared_terminal_item(event.sequence) {
                    self.push_skill_suggested(skill_id, reason.as_deref());
                }
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
                source,
                preview,
                ..
            } => {
                let suffix = if *truncated { " truncated" } else { "" };
                self.status =
                    format!("loaded skill context: {skill_id} ({bytes_loaded} bytes{suffix})");
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.set_skill_context_status(
                        skill_id,
                        *bytes_loaded,
                        *truncated,
                        source.as_ref(),
                        preview.as_deref(),
                    );
                }
            }
            SessionEventKind::SkillInvocationFailed {
                skill_id, error, ..
            } => {
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.push_skill_error(skill_id, error);
                }
            }
            SessionEventKind::AssistantReasoningDelta { text } => {
                self.push_live_reasoning_delta(text, application);
            }
            SessionEventKind::AssistantReasoningMessage { text } if self.reasoning_visible() => {
                let text = self
                    .latest_shared_terminal_text("Reasoning summary")
                    .unwrap_or_else(|| text.clone());
                self.finish_streaming_item("Reasoning summary", &text, application);
            }
            SessionEventKind::AgentChanged { .. } => {
                self.apply_shared_agent_changed();
            }
            SessionEventKind::TraceEvent { trace } if application.live_activity() => {
                self.apply_trace_event(trace);
            }
            SessionEventKind::ToolContribution {
                event: contribution,
            } => {
                let key = format!(
                    "{}:{}",
                    contribution.invocation_id, contribution.contribution_id
                );
                if let Some((_, Some(id))) = self.transient_contribution_items.remove(&key) {
                    self.transcript.retain(|item| item.id() != id);
                }
                if !self.push_shared_terminal_item(event.sequence) {
                    let fallback = serde_json::to_string_pretty(contribution)
                        .unwrap_or_else(|_| contribution.payload.to_string());
                    self.transcript
                        .push(TranscriptItem::new("Tool contribution", fallback));
                }
            }
            SessionEventKind::ToolInvocationLifecycle { event: lifecycle } => {
                if lifecycle.stage == bcode_session_models::ToolInvocationLifecycleStage::Started
                    && let Some(context) = self
                        .tool_call_contexts
                        .get(&lifecycle.invocation_id)
                        .cloned()
                {
                    self.apply_tool_started(
                        &lifecycle.invocation_id,
                        &context.tool_name,
                        None,
                        None,
                        Some(event.timestamp_ms),
                        application,
                    );
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
                let shared_work = self.shared_runtime_work_item(&work_id.0);
                let status = shared_work.as_ref().map_or(*status, |work| work.status);
                if work_id.0.starts_with("ralph:")
                    && matches!(
                        status,
                        RuntimeWorkStatus::Failed
                            | RuntimeWorkStatus::TimedOut
                            | RuntimeWorkStatus::Cancelled
                    )
                {
                    let message = shared_work
                        .as_ref()
                        .and_then(|work| work.message.as_deref())
                        .or(message.as_deref())
                        .unwrap_or("no details recorded");
                    self.push_system_message(&format!("Ralph work {status:?}: {message}"));
                }
            }
            SessionEventKind::RalphLifecycle {
                loop_name,
                kind,
                message,
                state_dir,
                ..
            } => {
                let projected = self.push_shared_terminal_item(event.sequence);
                if !projected {
                    self.push_system_message(&format!(
                        "Ralph {kind}\n* Loop: {loop_name}\n* {message}\n* State: {}",
                        self.working_directory().map_or_else(
                            || display_from_current_dir(state_dir),
                            |working_directory| display(state_dir, working_directory),
                        )
                    ));
                }
            }
            _ => {}
        }
    }

    pub fn apply_runtime_work_snapshots(&mut self, snapshots: &[bcode_ipc::RuntimeWorkSnapshot]) {
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
        if self.transcript_scroll_animation.is_some() {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY),
                now + TRANSCRIPT_SCROLL_ANIMATION_FRAME,
            ));
        }
        if self.newer_transcript_content_below() && self.latest_bar_active(now) {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(LATEST_BAR_ANIMATION_INVALIDATION_KEY),
                self.next_latest_bar_invalidation(now),
            ));
        }
        if self.live_preview_frame.dirty {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(LIVE_PREVIEW_FRAME_INVALIDATION_KEY),
                self.live_preview_frame.next_frame_at.unwrap_or(now),
            ));
        }
        if self.theme_transition_active(now) {
            requests.push(InvalidationRequest::new(
                InvalidationKey::new(THEME_TRANSITION_INVALIDATION_KEY),
                now + THEME_TRANSITION_FRAME,
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
        keys.iter().fold(UiInvalidation::None, |invalidation, key| {
            if self.cursor.handle_invalidation(key, now) {
                invalidation.merge(UiInvalidation::Paint)
            } else if is_transcript_scroll_animation_invalidation(key) {
                if self.handle_transcript_scroll_animation(now) {
                    invalidation.merge(UiInvalidation::Layout)
                } else {
                    invalidation
                }
            } else if is_latest_bar_animation_invalidation(key) {
                invalidation.merge(UiInvalidation::Paint)
            } else if is_live_preview_frame_invalidation(key) {
                self.live_preview_frame.dirty = false;
                self.live_preview_frame.next_frame_at = None;
                invalidation.merge(UiInvalidation::Layout)
            } else if is_theme_transition_invalidation(key) {
                self.update_theme_animation(now);
                if !self.theme_transition_active(now) {
                    self.theme_transition.finish();
                    self.presented_theme = PresentedTheme {
                        accent: self.target_theme.accent,
                    };
                }
                invalidation.merge(UiInvalidation::Paint)
            } else if is_tool_elapsed_invalidation(key) {
                invalidation.merge(UiInvalidation::Layout)
            } else {
                invalidation
            }
        })
    }

    fn mark_live_preview_dirty(&mut self) {
        self.live_preview_frame.dirty = true;
        self.live_preview_frames_requested = self.live_preview_frames_requested.saturating_add(1);
        if self.live_preview_frame.next_frame_at.is_none() {
            self.live_preview_frame.next_frame_at =
                Some(Instant::now() + LIVE_PREVIEW_FRAME_INTERVAL);
        }
    }

    fn latest_bar_active(&self, now: Instant) -> bool {
        self.latest_hidden_activity_at
            .is_some_and(|at| now.saturating_duration_since(at) < LATEST_BAR_ACTIVE_WINDOW)
    }

    fn next_latest_bar_invalidation(&self, now: Instant) -> Instant {
        debug_assert!(self.latest_bar_active(now));
        now + latest_bar_active_frame_duration(self.latest_hidden_activity_burst)
    }

    fn handle_transcript_scroll_animation(&mut self, now: Instant) -> bool {
        let Some(animation) = self.transcript_scroll_animation else {
            return false;
        };
        if animation.finished(now) {
            self.transcript_scroll_animation = None;
            self.viewport.follow_anchor(animation.target_top_row);
            match self.scroll_mode {
                TranscriptScrollMode::TransitionToEntry { sticky } => {
                    self.scroll_mode = TranscriptScrollMode::AnchoredToEntry { sticky };
                }
                TranscriptScrollMode::BottomFollow => {
                    self.latest_hidden_activity_at = None;
                    self.viewport.scroll_to_bottom(&mut self.older_history);
                }
                TranscriptScrollMode::AnchoredToEntry { .. }
                | TranscriptScrollMode::ManualDetached => {}
            }
        }
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
        if reset_history {
            self.input_history.reset_navigation();
        }
        self.composer.buffer_mut().clear();
        self.composer.buffer_mut().insert_str(text);
        self.wake_cursor();
    }

    /// Apply a locally selected model before a persisted session exists.
    pub fn apply_local_model_selection(&mut self, provider: Option<String>, model: &str) {
        self.session_view
            .set_model_selection(provider, model_to_display_selection(model), None);
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
        self.token_usage.clear_model_info();
        self.status = format!("model: {provider}/{model}");
    }

    fn apply_shared_agent_changed(&mut self) {
        debug_assert!(self.session_view.snapshot().runtime.agent_id.is_some());
        self.current_agent_accent = None;
        self.clear_pending_agent_fields();
        self.sync_theme_target(Instant::now());
    }

    fn tool_elapsed_invalidation_requests(
        &self,
        now: Instant,
        now_system: SystemTime,
    ) -> impl Iterator<Item = InvalidationRequest> {
        self.transcript
            .iter()
            .filter_map(move |item| {
                let timing = item.tool_timing()?;
                if !item.streaming() || timing.started_at_ms.is_none() {
                    return None;
                }
                let at = super::temporal::next_elapsed_invalidation_capped(
                    timing.started_at_ms?,
                    timing.finished_at_ms,
                    now,
                    now_system,
                    TOOL_ELAPSED_INVALIDATION_MAX_INTERVAL,
                )?;
                Some(InvalidationRequest::new(
                    InvalidationKey::new(format!(
                        "{TOOL_ELAPSED_INVALIDATION_PREFIX}:{}",
                        item.id().get()
                    )),
                    at,
                ))
            })
            .collect::<Vec<_>>()
            .into_iter()
    }

    fn extend_composer_selection_to_visual_delta(&mut self, width: usize, delta: isize) {
        let layout = self.composer.buffer().wrapped_layout(width);
        let target_row = if delta.is_negative() {
            layout.cursor.row.saturating_sub(delta.unsigned_abs())
        } else {
            layout
                .cursor
                .row
                .saturating_add(delta.unsigned_abs())
                .min(layout.lines.len().saturating_sub(1))
        };
        self.composer
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

    fn push_committed_user_message(
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
        if application.live_activity() {
            self.push_live_user_message(sequence, &text, timestamp_ms);
        } else {
            self.push_user_message(sequence, &text, timestamp_ms);
        }
    }

    fn push_live_user_message(&mut self, sequence: u64, text: &str, timestamp_ms: u64) {
        self.set_activity(ActivityState::PreparingModelRequest);
        self.push_user_message(sequence, text, timestamp_ms);
    }

    fn push_live_assistant_delta(&mut self, text: &str, application: SessionEventApplication) {
        self.add_streaming_delta(text, application);
        if !self.push_shared_streaming_terminal_item("Assistant") {
            self.push_streaming_item("Assistant", text);
        }
    }

    fn push_live_reasoning_delta(&mut self, text: &str, application: SessionEventApplication) {
        self.add_streaming_delta(text, application);
        if self.reasoning_visible()
            && !self.push_shared_streaming_terminal_item("Reasoning summary")
        {
            self.push_streaming_item("Reasoning summary", text);
        }
    }

    fn push_user_message(&mut self, sequence: u64, text: &str, timestamp_ms: u64) {
        self.remove_pending_submission(text);
        if !self.push_shared_terminal_item(sequence) {
            self.transcript.push(
                TranscriptItem::new("You", text.to_owned())
                    .with_event_metadata(sequence, timestamp_ms),
            );
        }
    }

    fn push_shared_terminal_item(&mut self, sequence: u64) -> bool {
        let Some(item) = self.shared_terminal_item(sequence) else {
            return false;
        };
        self.transcript.push(item);
        true
    }

    fn push_shared_streaming_terminal_item(&mut self, role: &'static str) -> bool {
        let Some(item) = self.latest_shared_streaming_terminal_item(role) else {
            return false;
        };
        if let Some(last) = self.transcript.last_mut()
            && last.role == role
            && last.streaming
        {
            last.replace_text(item.text);
            return true;
        }
        self.transcript.push(item);
        true
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

    fn shared_tool_request_item(
        &self,
        sequence: u64,
        tool_call_id: &str,
    ) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                item.sequence == Some(sequence)
                    && matches!(
                        &item.kind,
                        bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                            if tool.tool_call_id == tool_call_id
                                && matches!(
                                    tool.status,
                                    bcode_session_view_models::ToolInvocationViewStatus::Requested
                                        | bcode_session_view_models::ToolInvocationViewStatus::Running
                                )
                    )
            })
            .map(terminal_item_from_shared)
            .filter(|item| {
                matches!(
                    item.kind(),
                    TranscriptItemKind::ToolRequest {
                        tool_call_id: item_tool_call_id,
                        ..
                    } if item_tool_call_id == tool_call_id
                )
            })
    }

    fn shared_tool_output_item(&self, tool_call_id: &str) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                    | bcode_session_view_models::TranscriptViewItemKind::ToolRequest { tool }
                        if tool.tool_call_id == tool_call_id
                            && tool.result.is_none()
                            && tool.output.as_ref().is_some_and(|output| !output.text.is_empty())
                )
            })
            .map(terminal_item_from_shared)
            .filter(|item| {
                matches!(
                    item.kind(),
                    TranscriptItemKind::ToolResult {
                        tool_call_id: item_tool_call_id,
                        ..
                    } if item_tool_call_id == tool_call_id
                )
            })
    }

    fn shared_tool_result_item(&self, tool_call_id: &str) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolInvocation { tool }
                        if tool.tool_call_id == tool_call_id
                            && matches!(
                                tool.status,
                                bcode_session_view_models::ToolInvocationViewStatus::Finished
                            )
                )
            })
            .map(terminal_item_from_shared)
    }

    fn shared_tool_contribution_item(
        &self,
        contribution: &bcode_session_models::ToolContributionEvent,
    ) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolContribution {
                        contribution: item_contribution,
                    } if item_contribution.invocation_id == contribution.invocation_id
                        && item_contribution.contribution_id == contribution.contribution_id
                        && item_contribution.sequence == contribution.sequence
                )
            })
            .map(terminal_item_from_shared)
    }

    fn shared_runtime_work_item(
        &self,
        work_id: &str,
    ) -> Option<bcode_session_view_models::RuntimeWorkView> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::RuntimeWork { work }
                    if work.work_id.0 == work_id =>
                {
                    Some(work.clone())
                }
                _ => None,
            })
    }

    fn push_system_message(&mut self, text: &str) {
        self.transcript
            .push(TranscriptItem::new("System", text.to_owned()));
    }

    fn apply_working_directory_changed(
        &mut self,
        event_sequence: u64,
        old_working_directory: &std::path::Path,
    ) {
        let projected = self.push_shared_terminal_item(event_sequence);
        debug_assert!(
            projected,
            "working-directory event must have a shared terminal transcript item"
        );
        if let Some(new_working_directory) = self.working_directory() {
            self.status = format!(
                "working directory: {}",
                display(new_working_directory, old_working_directory)
            );
        }
    }

    fn push_streaming_item(&mut self, role: &'static str, text: &str) {
        self.transcript.push_streaming_item(role, text);
    }

    fn finish_streaming_item(
        &mut self,
        role: &'static str,
        text: &str,
        application: SessionEventApplication,
    ) {
        self.transcript.finish_streaming_item(role, text);
        if application.live_activity() && matches!(self.activity, ActivityState::Streaming { .. }) {
            self.set_activity(ActivityState::FinalizingModelTurn);
        }
    }

    fn projected_tool_request_context(
        &self,
        tool_call_id: &str,
        tool_name: &str,
        arguments_json: &str,
        working_directory: Option<std::path::PathBuf>,
        request_visual: Option<&bcode_session_models::PluginVisualDescriptor>,
    ) -> ToolCallContext {
        let shared_tool = self.session_view.snapshot().tools.get(tool_call_id);
        let request_visual = request_visual
            .cloned()
            .or_else(|| {
                shared_tool
                    .and_then(|tool| tool.request_visual.as_ref())
                    .map(|visual| visual.descriptor.clone())
            })
            .or_else(|| {
                self.live_tool_previews
                    .get(tool_call_id)
                    .map(|state| state.preview.visual.clone())
            });
        ToolCallContext {
            tool_name: shared_tool
                .and_then(|tool| tool.tool_name.clone())
                .unwrap_or_else(|| tool_name.to_owned()),
            arguments_json: shared_tool
                .and_then(|tool| tool.arguments_json.clone())
                .unwrap_or_else(|| arguments_json.to_owned()),
            working_directory: shared_tool
                .and_then(|tool| tool.working_directory.clone())
                .or(working_directory),
            request_visual,
        }
    }

    fn record_shared_active_tool_requested(&mut self, tool_call_id: &str) {
        let Some(tool) = self.session_view.snapshot().tools.get(tool_call_id) else {
            self.active_tool_calls.insert(tool_call_id.to_owned());
            return;
        };
        if !matches!(
            tool.status,
            bcode_session_view_models::ToolInvocationViewStatus::Finished
        ) {
            self.active_tool_calls.insert(tool_call_id.to_owned());
        }
    }

    fn finish_shared_active_tool_call(&mut self, tool_call_id: &str) {
        let finished = self
            .session_view
            .snapshot()
            .tools
            .get(tool_call_id)
            .is_none_or(|tool| {
                matches!(
                    tool.status,
                    bcode_session_view_models::ToolInvocationViewStatus::Finished
                )
            });
        if finished {
            self.active_tool_calls.remove(tool_call_id);
        }
    }

    fn push_tool_request(
        &mut self,
        event_metadata: (u64, u64),
        tool_call_id: &str,
        tool_name: &str,
        arguments_json: &str,
        working_directory: Option<std::path::PathBuf>,
        request_visual: Option<&bcode_session_models::PluginVisualDescriptor>,
    ) {
        let projected_context = self.projected_tool_request_context(
            tool_call_id,
            tool_name,
            arguments_json,
            working_directory,
            request_visual,
        );
        let effective_request_visual = projected_context.request_visual.clone();
        self.tool_call_contexts
            .insert(tool_call_id.to_owned(), projected_context.clone());
        let mut item = self
            .shared_tool_request_item(event_metadata.0, tool_call_id)
            .unwrap_or_else(|| {
                self.tool_invocation_projections
                    .get(tool_call_id)
                    .map_or_else(
                        || {
                            tool_request_item_from_projection(&ToolInvocationProjection {
                                tool_call_id: tool_call_id.to_owned(),
                                tool_name: Some(projected_context.tool_name.clone()),
                                arguments_json: Some(projected_context.arguments_json.clone()),
                                working_directory: projected_context.working_directory.clone(),
                                request_visual: effective_request_visual.clone(),
                                ..ToolInvocationProjection::default()
                            })
                        },
                        tool_request_item_from_projection,
                    )
                    .with_event_metadata(event_metadata.0, event_metadata.1)
            });
        if let Some(visual) = effective_request_visual.clone() {
            item.set_tool_request_visual(visual, false);
        }
        if let Some(context) = self.streamed_tool_results.get(tool_call_id)
            && let Some(mut visual) = effective_request_visual
        {
            enrich_tool_request_visual_runtime(
                &mut visual,
                tool_call_id,
                Some(context.columns),
                Some(context.rows),
            );
            item.set_tool_request_visual(visual, true);
            item.set_tool_started_at_ms(context.started_at_ms);
        }
        let has_live_preview_anchor = self
            .transcript
            .iter()
            .any(|item| item.is_live_preview_anchor_for(tool_call_id));
        if has_live_preview_anchor && request_visual.is_none() {
            self.finish_tool_request_preview(tool_call_id);
            self.set_activity(ActivityState::RunningTool {
                name: projected_context.tool_name.clone(),
            });
            self.status = tool_request_status(&projected_context.arguments_json)
                .unwrap_or_else(|| "started".to_owned());
            return;
        }
        if has_live_preview_anchor {
            self.transcript.mutate_rev_find(
                |existing| existing.is_live_preview_anchor_for(tool_call_id),
                |existing| *existing = item,
            );
            self.live_tool_previews.remove(tool_call_id);
            self.mark_live_preview_dirty();
            self.set_activity(ActivityState::RunningTool {
                name: projected_context.tool_name.clone(),
            });
            self.status = tool_request_status(&projected_context.arguments_json)
                .unwrap_or_else(|| "started".to_owned());
            return;
        }
        let replaced = self.transcript.mutate_rev_find(
            |existing| {
                matches!(
                    existing.kind(),
                    TranscriptItemKind::ToolRequest {
                        tool_call_id: item_tool_call_id,
                        ..
                    } if item_tool_call_id == tool_call_id
                )
            },
            |existing| *existing = item.clone(),
        );
        if replaced.is_none() {
            self.transcript.push(item);
        }
        self.set_activity(ActivityState::RunningTool {
            name: projected_context.tool_name,
        });
        self.status = tool_request_status(&projected_context.arguments_json)
            .unwrap_or_else(|| "started".to_owned());
    }

    fn push_permission_request(&mut self, input: PermissionRequestInput<'_>) {
        let shared_permission = self.shared_permission_view(input.permission_id).cloned();
        if !self.push_shared_terminal_item(input.event_sequence) {
            self.transcript
                .push(super::transcript::permission_request_item(
                    input.permission_id,
                    input.tool_call_id,
                    input.tool_name,
                    input.arguments_json,
                    input.policy_source,
                    input.policy_reason,
                ));
        }
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
        if !approved && let Some(tool_call_id) = self.permission_tool_call_id(permission_id) {
            self.finish_tool_request_preview(&tool_call_id);
        }
        status.clone_into(&mut self.status);
        self.transcript.push(
            self.shared_permission_result_item(permission_id)
                .unwrap_or_else(|| permission_result_item(permission_id, approved)),
        );
    }

    fn shared_permission_result_item(&self, permission_id: &str) -> Option<TranscriptItem> {
        self.session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::Permission { permission }
                        if permission.permission_id == permission_id && permission.resolved
                )
            })
            .map(terminal_item_from_shared)
    }

    fn set_file_activity(&mut self, tool_name: &str) {
        self.set_activity(ActivityState::RunningTool {
            name: tool_name.to_owned(),
        });
    }

    fn set_activity_for_tool_call(&mut self, tool_call_id: &str, fallback_tool_name: &str) {
        if let Some(context) = self.tool_call_contexts.get(tool_call_id) {
            let tool_name = context.tool_name.clone();
            self.set_file_activity(&tool_name);
        } else {
            self.set_file_activity(fallback_tool_name);
        }
    }

    const fn tool_call_file_status(_tool_call_id: &str) -> Option<String> {
        None
    }

    fn record_live_preview_state(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        argument_bytes: usize,
        preview: &LiveToolArgumentPreview,
    ) -> bool {
        let truncated = live_tool_preview_truncated(preview);
        if let Some(state) = self.live_tool_previews.get_mut(tool_call_id) {
            state.snapshots_received = state.snapshots_received.saturating_add(1);
            if state.preview == *preview && state.argument_bytes == argument_bytes {
                state.duplicates_skipped = state.duplicates_skipped.saturating_add(1);
                self.live_preview_duplicates_skipped =
                    self.live_preview_duplicates_skipped.saturating_add(1);
                return false;
            }
            tool_name.clone_into(&mut state.tool_name);
            state.argument_bytes = argument_bytes;
            state.preview = preview.clone();
            state.revision = state.revision.saturating_add(1);
            if truncated {
                state.truncated_snapshots = state.truncated_snapshots.saturating_add(1);
                self.live_preview_truncated_snapshots =
                    self.live_preview_truncated_snapshots.saturating_add(1);
            }
        } else {
            self.live_tool_previews.insert(
                tool_call_id.to_owned(),
                LiveToolPreviewState {
                    tool_name: tool_name.to_owned(),
                    argument_bytes,
                    preview: preview.clone(),
                    working_directory: self
                        .tool_call_contexts
                        .get(tool_call_id)
                        .and_then(|context| context.working_directory.clone()),
                    revision: 1,
                    snapshots_received: 1,
                    duplicates_skipped: 0,
                    truncated_snapshots: u64::from(truncated),
                },
            );
            if truncated {
                self.live_preview_truncated_snapshots =
                    self.live_preview_truncated_snapshots.saturating_add(1);
            }
        }
        self.live_preview_revision = self.live_preview_revision.saturating_add(1);
        true
    }

    fn finish_live_tool_output(
        &mut self,
        tool_call_id: &str,
        is_error: Option<bool>,
    ) -> Option<FinishedStreamedToolOutput> {
        let context = self.streamed_tool_results.get(tool_call_id)?;
        if !context.saw_output && is_error.is_some() {
            return None;
        }
        let index = context.index?;
        let output_kind = self.transcript.get(index).map_or(
            FinishedStreamedToolOutput::PlainToolResult,
            |item| {
                if matches!(
                    item.kind(),
                    TranscriptItemKind::ToolResult {
                        artifact: Some(_),
                        ..
                    }
                ) {
                    FinishedStreamedToolOutput::Visual
                } else {
                    FinishedStreamedToolOutput::PlainToolResult
                }
            },
        );
        if let Some(item) = self.transcript.get_mut(index) {
            if let Some(projection) = self.tool_invocation_projections.get(tool_call_id) {
                item.set_tool_started_at_ms(projection.started_at_ms);
                item.set_tool_finished_at_ms(projection.finished_at_ms);
            }
            item.finish_streaming();
        }
        Some(output_kind)
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
        result: &str,
        is_error: bool,
        semantic_result: Option<&ToolInvocationResult>,
        application: SessionEventApplication,
    ) {
        let tool_name = self
            .tool_call_contexts
            .get(tool_call_id)
            .map(|context| context.tool_name.clone());
        let arguments_json = self
            .tool_call_contexts
            .get(tool_call_id)
            .map(|context| context.arguments_json.clone());
        if semantic_result.is_some()
            && self
                .streamed_tool_results
                .get(tool_call_id)
                .is_some_and(|context| context.index.is_some() && !context.saw_output)
        {
            self.remove_streamed_tool_result(tool_call_id);
            self.streamed_tool_results.remove(tool_call_id);
        }
        if semantic_result.is_none() {
            if self
                .finish_live_tool_output(tool_call_id, Some(is_error))
                .is_some()
            {
                self.update_tool_result_status(tool_call_id, is_error, application);
                self.finish_tool_request_preview(tool_call_id);
                return;
            }
        } else if self
            .finish_live_tool_output(tool_call_id, Some(is_error))
            .is_some()
        {
            if semantic_result.is_some_and(artifact_result_replaces_streamed_visual) {
                self.remove_streamed_tool_result(tool_call_id);
            } else {
                self.update_tool_result_status(tool_call_id, is_error, application);
                self.finish_tool_request_preview(tool_call_id);
                return;
            }
        }
        self.supersede_matching_live_preview(tool_call_id, semantic_result);
        let mut item = self
            .shared_tool_result_item(tool_call_id)
            .unwrap_or_else(|| {
                if let Some(semantic_result) = semantic_result {
                    semantic_tool_result_item_from_raw(
                        tool_call_id,
                        tool_name.as_deref(),
                        arguments_json.as_deref(),
                        self.tool_call_contexts
                            .get(tool_call_id)
                            .and_then(|context| context.working_directory.as_deref()),
                        semantic_result,
                        is_error,
                    )
                } else {
                    self.tool_invocation_projections
                        .get(tool_call_id)
                        .and_then(generic_tool_result_item_from_projection)
                        .unwrap_or_else(|| {
                            tool_result_item(
                                tool_call_id,
                                tool_name.as_deref(),
                                arguments_json.as_deref(),
                                &display_tool_result_text(result),
                                is_error,
                            )
                        })
                }
            });
        if let Some(projection) = self.tool_invocation_projections.get(tool_call_id) {
            item.set_tool_started_at_ms(projection.started_at_ms);
            item.set_tool_finished_at_ms(projection.finished_at_ms);
        }
        if item
            .tool_timing()
            .and_then(|timing| timing.timed_out)
            .is_none()
        {
            item.set_tool_timed_out(semantic_result.and_then(tool_result_timed_out));
        }
        self.transcript.push(item);
        self.update_tool_result_status(tool_call_id, is_error, application);
        self.finish_tool_request_preview(tool_call_id);
    }

    fn apply_tool_stream_event(
        &mut self,
        event: &ToolInvocationStreamEvent,
        application: SessionEventApplication,
    ) {
        match event {
            ToolInvocationStreamEvent::OutputDelta {
                tool_call_id,
                stream,
                text,
                ..
            } => self.push_tool_output_delta(tool_call_id, *stream, text),
            ToolInvocationStreamEvent::Status { message, .. } => {
                if application.live_activity() {
                    message.clone_into(&mut self.status);
                }
            }
            ToolInvocationStreamEvent::VisualUpdate {
                tool_call_id,
                visual,
                streaming,
                ..
            } => self.apply_tool_visual_update(tool_call_id, visual, *streaming),
            ToolInvocationStreamEvent::ArtifactUpdate {
                tool_call_id,
                artifact_id,
                reference_key,
                revision,
                ..
            } => {
                let key = (
                    tool_call_id.clone(),
                    artifact_id.clone(),
                    reference_key.clone(),
                );
                if self
                    .active_artifact_revisions
                    .get(&key)
                    .is_none_or(|current| revision > current)
                {
                    self.active_artifact_revisions.insert(key, *revision);
                }
            }
            ToolInvocationStreamEvent::Started {
                tool_call_id,
                tool_name,
                columns,
                rows,
                started_at_ms,
                ..
            } => self.apply_tool_started(
                tool_call_id,
                tool_name,
                *columns,
                *rows,
                *started_at_ms,
                application,
            ),
            ToolInvocationStreamEvent::LegacyPresentation { .. }
            | ToolInvocationStreamEvent::LegacyTransientPruned { .. } => {
                Self::legacy_discard_tool_presentation_stream_event();
            }
            ToolInvocationStreamEvent::Finished {
                tool_call_id,
                is_error,
                finished_at_ms,
                ..
            } => self.apply_tool_finished(tool_call_id, *is_error, *finished_at_ms, application),
        }
    }

    #[cfg(test)]
    #[must_use]
    fn active_artifact_revision(
        &self,
        tool_call_id: &str,
        artifact_id: &str,
        reference_key: &str,
    ) -> Option<u64> {
        self.active_artifact_revisions
            .get(&(
                tool_call_id.to_owned(),
                artifact_id.to_owned(),
                reference_key.to_owned(),
            ))
            .copied()
    }

    fn apply_tool_visual_update(
        &mut self,
        tool_call_id: &str,
        visual: &bcode_session_models::PluginVisualDescriptor,
        streaming: bool,
    ) {
        if streaming {
            self.active_plugin_visuals
                .insert(tool_call_id.to_owned(), visual.clone());
        } else {
            self.active_plugin_visuals.remove(tool_call_id);
        }
        let tool_name = self
            .tool_call_contexts
            .get(tool_call_id)
            .map(|context| context.tool_name.as_str());
        let working_directory = self
            .tool_call_contexts
            .get(tool_call_id)
            .and_then(|context| context.working_directory.as_deref());
        let mut item = streaming_tool_visual_item(
            tool_call_id,
            tool_name,
            working_directory,
            visual,
            streaming,
        );
        if let Some(context) = self.streamed_tool_results.get(tool_call_id) {
            item.set_tool_started_at_ms(context.started_at_ms);
        }
        item.set_tool_timeout_ms(tool_visual_timeout_ms(visual));
        let index = self.transcript.upsert_tool_visual_item(item);
        self.live_tool_previews.remove(tool_call_id);
        self.mark_live_preview_dirty();
        let context = self
            .streamed_tool_results
            .entry(tool_call_id.to_owned())
            .or_insert_with(|| StreamedToolResultContext {
                index: Some(index),
                columns: 0,
                rows: 0,
                started_at_ms: None,
                saw_output: true,
            });
        context.index = Some(index);
        context.saw_output = true;
    }

    fn apply_tool_started(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        columns: Option<u16>,
        rows: Option<u16>,
        started_at_ms: Option<u64>,
        application: SessionEventApplication,
    ) {
        let context = self
            .streamed_tool_results
            .entry(tool_call_id.to_owned())
            .or_insert_with(|| StreamedToolResultContext {
                index: None,
                columns: columns.unwrap_or(0),
                rows: rows.unwrap_or(0),
                started_at_ms,
                saw_output: false,
            });
        context.columns = columns.unwrap_or(context.columns);
        context.rows = rows.unwrap_or(context.rows);
        context.started_at_ms = started_at_ms;
        self.active_tool_calls.insert(tool_call_id.to_owned());
        if let Some(mut visual) = self
            .tool_call_contexts
            .get(tool_call_id)
            .and_then(|context| context.request_visual.clone())
        {
            enrich_tool_request_visual_runtime(&mut visual, tool_call_id, columns, rows);
            if let Some(context) = self.tool_call_contexts.get_mut(tool_call_id) {
                context.request_visual = Some(visual.clone());
            }
            self.transcript.mutate_rev_find(
                |item| {
                    matches!(
                        item.kind(),
                        TranscriptItemKind::ToolRequest {
                            tool_call_id: item_tool_call_id,
                            ..
                        } if item_tool_call_id == tool_call_id
                    )
                },
                |item| item.set_tool_request_visual(visual.clone(), true),
            );
            self.active_plugin_visuals
                .entry(tool_call_id.to_owned())
                .or_insert(visual);
        }
        self.tool_activity_seen = true;
        if application.live_activity() {
            self.set_activity_for_tool_call(tool_call_id, tool_name);
            if let Some(status) = Self::tool_call_file_status(tool_call_id) {
                self.status = status;
            } else {
                tool_name.clone_into(&mut self.status);
            }
        }
    }

    fn apply_tool_finished(
        &mut self,
        tool_call_id: &str,
        is_error: bool,
        finished_at_ms: Option<u64>,
        application: SessionEventApplication,
    ) {
        self.active_tool_calls.remove(tool_call_id);
        self.active_plugin_visuals.remove(tool_call_id);
        if let Some(context) = self.streamed_tool_results.get_mut(tool_call_id)
            && let Some(index) = context.index
            && let Some(item) = self.transcript.get_mut(index)
        {
            item.set_tool_started_at_ms(context.started_at_ms);
            item.set_tool_finished_at_ms(finished_at_ms);
            item.finish_streaming();
        }
        self.finish_tool_request_preview(tool_call_id);
        if is_error {
            if application.live_activity() {
                "failed".clone_into(&mut self.status);
            }
        } else if let Some(status) = Self::tool_call_file_status(tool_call_id) {
            if application.live_activity() {
                self.status = format!("applied · {status}");
            }
        } else if application.live_activity() {
            "finished".clone_into(&mut self.status);
        }
    }

    const fn legacy_discard_tool_presentation_stream_event() {}

    fn push_tool_output_delta(
        &mut self,
        tool_call_id: &str,
        _stream: ToolOutputStream,
        text: &str,
    ) {
        if text.is_empty() {
            return;
        }
        let shared_item = self.shared_tool_output_item(tool_call_id);
        if let Some(context) = self.streamed_tool_results.get(tool_call_id)
            && let Some(index) = context.index
            && let Some(item) = self.transcript.get_mut(index)
        {
            self.tool_activity_seen = true;
            if let Some(shared_item) = shared_item {
                item.copy_tool_timing_from(&shared_item);
                item.replace_text(shared_item.text);
            } else {
                item.append_text(text);
            }
            return;
        }
        let context = self.tool_call_contexts.get(tool_call_id);
        self.tool_activity_seen = true;
        let item = shared_item.unwrap_or_else(|| {
            streaming_tool_output_item(
                tool_call_id,
                context.map(|context| context.tool_name.as_str()),
                context.map(|context| context.arguments_json.as_str()),
                text,
            )
        });
        self.transcript.push(item);
        self.streamed_tool_results.insert(
            tool_call_id.to_owned(),
            StreamedToolResultContext {
                index: Some(self.transcript.len().saturating_sub(1)),
                columns: 0,
                rows: 0,
                started_at_ms: None,
                saw_output: true,
            },
        );
    }

    fn remove_streamed_tool_result(&mut self, tool_call_id: &str) {
        self.transcript.retain(|item| {
            !matches!(
                item.kind(),
                TranscriptItemKind::ToolResult {
                    tool_call_id: item_tool_call_id,
                    ..
                } if item_tool_call_id == tool_call_id
            )
        });
    }

    fn supersede_matching_live_preview(
        &mut self,
        tool_call_id: &str,
        semantic_result: Option<&ToolInvocationResult>,
    ) {
        let superseded = self
            .live_tool_previews
            .get(tool_call_id)
            .is_some_and(|state| {
                semantic_result_supersedes_live_preview(
                    tool_call_id,
                    &state.preview,
                    semantic_result,
                )
            });
        if !superseded {
            return;
        }
        self.live_tool_previews.remove(tool_call_id);
        self.transcript
            .retain(|item| !item.is_live_preview_anchor_for(tool_call_id));
        self.mark_live_preview_dirty();
    }

    fn apply_live_tool_argument_preview(
        &mut self,
        tool_call_id: &str,
        tool_name: &str,
        argument_bytes: usize,
        preview: &LiveToolArgumentPreview,
    ) {
        if !self.record_live_preview_state(tool_call_id, tool_name, argument_bytes, preview) {
            return;
        }
        self.ensure_live_tool_preview_anchor(tool_call_id, tool_name);
        let bytes = format_provider_bytes(argument_bytes);
        let status = preview.streaming_status.clone().map_or_else(
            || format!("streaming {tool_name} · {bytes} received"),
            |status| render_visual_status_template(&status, preview, &bytes),
        );
        self.set_activity(ActivityState::ProviderStream {
            detail: status.clone(),
        });
        self.status = status;
        self.mark_live_preview_dirty();
    }

    fn ensure_live_tool_preview_anchor(&mut self, tool_call_id: &str, tool_name: &str) {
        if self
            .transcript
            .iter()
            .any(|item| item.is_live_preview_anchor_for(tool_call_id))
        {
            return;
        }
        self.transcript
            .push(live_tool_preview_anchor_item(tool_call_id, tool_name));
    }

    fn permission_tool_call_id(&self, permission_id: &str) -> Option<String> {
        self.session_view
            .snapshot()
            .permissions
            .iter()
            .find(|permission| permission.permission_id == permission_id)
            .map(|permission| permission.tool_call_id.clone())
            .or_else(|| {
                self.session_view
                    .snapshot()
                    .transcript
                    .items
                    .iter()
                    .rev()
                    .find_map(|item| match &item.kind {
                        bcode_session_view_models::TranscriptViewItemKind::Permission {
                            permission,
                        } if permission.permission_id == permission_id => {
                            Some(permission.tool_call_id.clone())
                        }
                        _ => None,
                    })
            })
    }

    fn finish_tool_request_preview(&mut self, tool_call_id: &str) {
        self.transcript.mutate_rev_find(
            |item| {
                matches!(
                    item.kind(),
                    TranscriptItemKind::ToolRequest {
                        tool_call_id: item_tool_call_id,
                        ..
                    } if item_tool_call_id == tool_call_id
                ) || item.is_live_preview_anchor_for(tool_call_id)
            },
            TranscriptItem::finish_streaming,
        );
    }

    fn push_model_usage(
        &mut self,
        event_sequence: u64,
        turn_id: &str,
        usage: &bcode_session_models::SessionTokenUsage,
        application: SessionEventApplication,
    ) {
        let projected_usage = self
            .session_view
            .snapshot()
            .transcript
            .items
            .iter()
            .rev()
            .find_map(|item| match &item.kind {
                bcode_session_view_models::TranscriptViewItemKind::Usage { usage }
                    if usage.turn_id == turn_id =>
                {
                    Some(usage.usage.clone())
                }
                _ => None,
            })
            .unwrap_or_else(|| usage.clone());
        self.token_usage.absorb(&projected_usage);
        if application.live_activity()
            && let Some(tokens) = projected_usage.metered_total_tokens()
        {
            self.status = format!("tokens: {tokens}");
        }
        if !self.push_shared_terminal_item(event_sequence) {
            self.transcript
                .push(model_usage_item(turn_id, &projected_usage));
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
        if let Some(last) = self.transcript.last_mut()
            && last.role == "Assistant"
        {
            last.finish_streaming();
        }
        if application.live_activity() {
            self.set_activity(ActivityState::Idle);
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
        } else if let Some(detail) =
            bcode_session_view_models::runtime_work_status_label(runtime_work)
        {
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

    fn push_compaction(&mut self, summary: &str) {
        self.transcript.push(TranscriptItem::new(
            "Compaction",
            format!("local context compaction: {summary}"),
        ));
    }

    fn push_provider_compaction(
        &mut self,
        snapshot: &bcode_session_models::ProviderContextSnapshot,
    ) {
        let origin = provider_compaction_origin_label(snapshot.origin);
        self.transcript.push(TranscriptItem::new(
            "Compaction",
            format!(
                "{origin} context compaction ({})",
                snapshot.provider_plugin_id
            ),
        ));
    }

    fn set_activity(&mut self, activity: ActivityState) {
        if self.activity != activity {
            if !self.activity.same_phase_as(&activity) {
                self.activity_started_at = Instant::now();
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

    fn apply_trace_event(&mut self, trace: &SessionTraceEvent) {
        match &trace.payload {
            SessionTracePayload::ProviderStreamEvent(event) => {
                self.apply_provider_stream_event(event);
            }
            SessionTracePayload::ProviderEvent { event_type, detail } => {
                if matches!(event_type.as_str(), "warning" | "error") {
                    let detail = detail
                        .clone()
                        .unwrap_or_else(|| format!("provider event: {event_type}"));
                    self.set_activity(ActivityState::ProviderStream {
                        detail: detail.clone(),
                    });
                    self.status = detail;
                }
            }
            SessionTracePayload::ContextCompaction { message, .. } => {
                self.apply_compaction_trace(trace.phase, message.as_deref());
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
                let tool_name = self.tool_call_contexts.get(tool_call_id).map_or_else(
                    || "unknown tool".to_owned(),
                    |context| context.tool_name.clone(),
                );
                self.set_activity(if approved.is_none() {
                    ActivityState::WaitingPermission { name: tool_name }
                } else {
                    ActivityState::PreparingFollowUpRequest
                });
            }
            SessionTracePayload::ToolInvocationFinished { .. } => {
                self.set_activity(ActivityState::PreparingFollowUpRequest);
            }
            SessionTracePayload::ToolPolicyEvaluated { .. }
            | SessionTracePayload::ToolInvocationStreamEvent(_) => {}
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

    fn apply_compaction_trace(&mut self, phase: SessionTracePhase, message: Option<&str>) {
        match phase {
            SessionTracePhase::ContextCompactionStarted => {
                let detail = message.map_or_else(|| "older context".to_owned(), ToOwned::to_owned);
                self.set_activity(ActivityState::Compacting { detail });
            }
            SessionTracePhase::ContextCompactionFinished => {
                if matches!(self.activity, ActivityState::Compacting { .. }) {
                    self.set_activity(ActivityState::PreparingModelRequest);
                }
            }
            SessionTracePhase::ContextCompactionSkipped => {
                if matches!(self.activity, ActivityState::Compacting { .. }) {
                    self.set_activity(ActivityState::PreparingModelRequest);
                }
            }
            SessionTracePhase::ContextCompactionDiagnostic
            | SessionTracePhase::ModelRequestBuilt
            | SessionTracePhase::ModelProviderRoundStarted
            | SessionTracePhase::ModelProviderRoundFinished
            | SessionTracePhase::ModelProviderEvent
            | SessionTracePhase::ToolInvocationStarted
            | SessionTracePhase::ToolPolicyEvaluated
            | SessionTracePhase::ToolPermissionWaitStarted
            | SessionTracePhase::ToolPermissionWaitFinished
            | SessionTracePhase::ToolInvocationFinished
            | SessionTracePhase::ToolInvocationOutput
            | SessionTracePhase::SkillInvoked
            | SessionTracePhase::SkillSuggested
            | SessionTracePhase::SkillActivated
            | SessionTracePhase::SkillDeactivated
            | SessionTracePhase::SkillContextLoaded
            | SessionTracePhase::SkillInvocationFailed => {}
        }
    }

    fn set_skill_context_status(
        &mut self,
        skill_id: &impl std::fmt::Display,
        bytes_loaded: usize,
        truncated: bool,
        source: Option<&SkillSource>,
        preview: Option<&str>,
    ) {
        let suffix = if truncated { " truncated" } else { "" };
        self.status = format!("loaded skill context: {skill_id} ({bytes_loaded} bytes{suffix})");
        let source_text = source.map_or_else(String::new, |source| {
            let path = source
                .path
                .as_deref()
                .map_or_else(String::new, |path| format!("\nFile: {path}"));
            format!("\nSource: {}{path}", source.label)
        });
        let preview_text = preview.map_or_else(String::new, |preview| {
            if preview.trim().is_empty() {
                String::new()
            } else {
                format!("\n\nPreview:\n{preview}")
            }
        });
        self.transcript.push(TranscriptItem::new(
            "Skill context",
            format!("loaded {skill_id}{source_text}\nBytes: {bytes_loaded}{suffix}{preview_text}"),
        ));
    }

    fn push_skill_invoked(
        &mut self,
        skill_id: &impl std::fmt::Display,
        arguments: &str,
        source: Option<&SkillSource>,
    ) {
        let source =
            source.map_or_else(String::new, |source| format!("\nSource: {}", source.label));
        self.transcript.push(TranscriptItem::new(
            "Skill",
            format!("invoked {skill_id}{source}\nArguments: {arguments}"),
        ));
    }

    fn push_skill_suggested(&mut self, skill_id: &impl std::fmt::Display, reason: Option<&str>) {
        self.status = format!("suggested skill: {skill_id}");
        if let Some(reason) = reason {
            self.transcript.push(TranscriptItem::new(
                "Skill",
                format!("suggested {skill_id}\nReason: {reason}"),
            ));
        }
    }

    fn push_skill_error(&mut self, skill_id: &impl std::fmt::Display, error: &str) {
        self.transcript.push(TranscriptItem::new(
            "Skill error",
            format!("{skill_id}: {error}"),
        ));
    }
}

const fn artifact_result_replaces_streamed_visual(result: &ToolInvocationResult) -> bool {
    matches!(result, ToolInvocationResult::Artifact { .. })
}

fn tool_result_timed_out(result: &ToolInvocationResult) -> Option<bool> {
    let ToolInvocationResult::Artifact { artifact } = result else {
        return None;
    };
    artifact
        .metadata
        .get("timed_out")
        .and_then(serde_json::Value::as_bool)
}

fn tool_visual_timeout_ms(visual: &bcode_session_models::PluginVisualDescriptor) -> Option<u64> {
    visual
        .payload
        .get("_bcode_runtime")
        .and_then(|runtime| runtime.get("timeout_ms"))
        .and_then(serde_json::Value::as_u64)
}

pub const fn composer_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
}

fn latest_bar_active_frame_duration(burst: u8) -> Duration {
    Duration::from_millis(
        210_u64
            .saturating_sub(u64::from(burst).saturating_mul(21))
            .max(36),
    )
}

fn is_tool_elapsed_invalidation(key: &InvalidationKey) -> bool {
    key.as_str().starts_with(TOOL_ELAPSED_INVALIDATION_PREFIX)
}

fn is_latest_bar_animation_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == LATEST_BAR_ANIMATION_INVALIDATION_KEY
}

fn is_live_preview_frame_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == LIVE_PREVIEW_FRAME_INVALIDATION_KEY
}

fn is_theme_transition_invalidation(key: &InvalidationKey) -> bool {
    key.as_str() == THEME_TRANSITION_INVALIDATION_KEY
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
    session_cost_micros: Option<u64>,
    latest_cached_input_tokens: Option<u32>,
    latest_cache_write_input_tokens: Option<u32>,
    provider_reuse_active: bool,
    latest_sent_message_count: Option<usize>,
    latest_total_message_count: Option<usize>,
    latest_prompt_cache_points: Option<usize>,
    context_window: Option<u32>,
    pricing: Option<bcode_model::ModelPricingInfo>,
}

impl TokenUsageMeter {
    fn absorb(&mut self, usage: &bcode_session_models::SessionTokenUsage) {
        if let Some(pricing) = &self.pricing {
            let usage = bcode_model::TokenUsage {
                input_tokens: usage.input_tokens,
                output_tokens: usage.output_tokens,
                total_tokens: usage.total_tokens,
                cached_input_tokens: usage.cached_input_tokens,
                cache_write_input_tokens: usage.cache_write_input_tokens,
                reasoning_tokens: usage.reasoning_tokens,
            };
            if let Some(cost) = pricing.estimate_cost(&usage) {
                self.session_cost_micros = Some(
                    self.session_cost_micros
                        .unwrap_or_default()
                        .saturating_add(cost.total_micros),
                );
            }
        }
        self.latest_cached_input_tokens = usage.cached_input_tokens;
        self.latest_cache_write_input_tokens = usage.cache_write_input_tokens;
    }

    fn apply_model_info(&mut self, model: Option<&bcode_model::ModelInfo>) {
        if let Some(model) = model {
            self.context_window = model.context_window;
            self.pricing.clone_from(&model.pricing);
        }
    }

    fn clear_model_info(&mut self) {
        self.latest_cached_input_tokens = None;
        self.latest_cache_write_input_tokens = None;
        self.context_window = None;
        self.pricing = None;
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
        if let Some(cost_micros) = self.session_cost_micros {
            parts.push(format!("~{}", format_usd_micros(cost_micros)));
        }
        parts.join(" · ")
    }

    fn context_summary(
        &self,
        occupancy: Option<&bcode_session_models::RequestContextOccupancy>,
    ) -> String {
        match (occupancy, self.context_window) {
            (Some(occupancy), Some(window)) if window > 0 => {
                let input = u32::try_from(occupancy.observation.context_tokens.tokens())
                    .unwrap_or(u32::MAX);
                let percentage = context_window_percentage(input, window);
                format!(
                    "{}{}/{} {}",
                    if occupancy.observation.context_tokens.is_estimated() {
                        "~"
                    } else {
                        ""
                    },
                    format_context_count(u64::from(input)),
                    compact_context_window(u64::from(window)),
                    if percentage > 100 {
                        "100%+".to_string()
                    } else {
                        format!("{percentage}%")
                    }
                )
            }
            (None, Some(window)) if window > 0 => {
                format!("—/{} —%", compact_context_window(u64::from(window)))
            }
            _ => "—/— —%".to_owned(),
        }
    }
}

const fn provider_compaction_origin_label(
    origin: bcode_session_models::ProviderContextSnapshotOrigin,
) -> &'static str {
    match origin {
        bcode_session_models::ProviderContextSnapshotOrigin::Explicit => "explicit provider-native",
        bcode_session_models::ProviderContextSnapshotOrigin::ProviderManaged => "provider-managed",
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

fn format_usd_micros(micros: u64) -> String {
    let dollars = micros / 1_000_000;
    let cents = (micros % 1_000_000) / 10_000;
    if dollars == 0 && cents == 0 && micros > 0 {
        "<$0.01".to_string()
    } else {
        format!("${dollars}.{cents:02}")
    }
}

fn render_visual_status_template(
    template: &str,
    preview: &LiveToolArgumentPreview,
    bytes: &str,
) -> String {
    let mut rendered = template.replace(concat!("{", "bytes", "}"), bytes);
    if let Some(object) = preview.visual.payload.as_object() {
        for (key, value) in object {
            if let Some(value) = value.as_str() {
                rendered = rendered.replace(&format!("{{{key}}}"), value);
            }
        }
    }
    clean_unresolved_status_placeholders(&rendered)
}

fn clean_unresolved_status_placeholders(value: &str) -> String {
    let mut cleaned = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '{' {
            let mut placeholder = String::new();
            let mut closed = false;
            for next in chars.by_ref() {
                if next == '}' {
                    closed = true;
                    break;
                }
                placeholder.push(next);
            }
            if closed && !placeholder.is_empty() {
                continue;
            }
            cleaned.push('{');
            cleaned.push_str(&placeholder);
            if closed {
                cleaned.push('}');
            }
        } else {
            cleaned.push(ch);
        }
    }
    cleaned
        .split('·')
        .map(|segment| segment.split_whitespace().collect::<Vec<_>>().join(" "))
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>()
        .join(" · ")
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

fn context_window_percentage(input_tokens: u32, context_window: u32) -> u32 {
    let numerator = u64::from(input_tokens).saturating_mul(100);
    let denominator = u64::from(context_window).max(1);
    u32::try_from(numerator / denominator).unwrap_or(u32::MAX)
}

fn enrich_tool_request_visual_runtime(
    visual: &mut bcode_session_models::PluginVisualDescriptor,
    tool_call_id: &str,
    columns: Option<u16>,
    rows: Option<u16>,
) {
    let runtime = visual
        .payload
        .as_object_mut()
        .map(|payload| {
            payload
                .entry("_bcode_runtime")
                .or_insert_with(|| serde_json::json!({}))
        })
        .and_then(serde_json::Value::as_object_mut);
    if let Some(runtime) = runtime {
        runtime.insert(
            "live_state_key".to_owned(),
            serde_json::Value::String(tool_call_id.to_owned()),
        );
        if let Some(columns) = columns {
            runtime.insert("columns".to_owned(), serde_json::json!(columns));
        }
        if let Some(rows) = rows {
            runtime.insert("rows".to_owned(), serde_json::json!(rows));
        }
        runtime.insert(
            "output".to_owned(),
            serde_json::Value::String(String::new()),
        );
        runtime.insert("streaming".to_owned(), serde_json::Value::Bool(true));
    }
}

const fn live_tool_preview_truncated(_preview: &LiveToolArgumentPreview) -> bool {
    false
}

const fn event_breaks_sticky_entry_anchor(event: &SessionEvent) -> bool {
    matches!(
        &event.kind,
        SessionEventKind::ToolCallRequested { .. }
            | SessionEventKind::ToolInvocationStream { .. }
            | SessionEventKind::PermissionRequested { .. }
    )
}

fn referenced_tool_call_ids(items: &[TranscriptItem]) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    for item in items {
        match item.kind() {
            TranscriptItemKind::ToolRequest { tool_call_id, .. }
            | TranscriptItemKind::LiveToolPreviewAnchor { tool_call_id, .. }
            | TranscriptItemKind::ToolResult { tool_call_id, .. }
            | TranscriptItemKind::PermissionRequest { tool_call_id, .. } => {
                ids.insert(tool_call_id.clone());
            }
            TranscriptItemKind::UserMessage
            | TranscriptItemKind::AssistantMessage
            | TranscriptItemKind::ReasoningMessage
            | TranscriptItemKind::Usage { .. }
            | TranscriptItemKind::PermissionResult { .. }
            | TranscriptItemKind::System
            | TranscriptItemKind::Meta
            | TranscriptItemKind::Skill
            | TranscriptItemKind::SkillError
            | TranscriptItemKind::ToolContribution { .. }
            | TranscriptItemKind::Generic => {}
        }
    }
    ids
}

const fn event_affects_transcript_rows(event: &SessionEvent) -> bool {
    match &event.kind {
        SessionEventKind::UserMessage { .. }
        | SessionEventKind::AssistantDelta { .. }
        | SessionEventKind::AssistantMessage { .. }
        | SessionEventKind::SystemMessage { .. }
        | SessionEventKind::ToolCallRequested { .. }
        | SessionEventKind::ToolCallFinished { .. }
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
        | SessionEventKind::ToolExchangeRequested { .. }
        | SessionEventKind::ToolExchangeResolved { .. }
        | SessionEventKind::ToolInvocationLifecycle { .. }
        | SessionEventKind::ToolInvocationStream { .. }
        | SessionEventKind::RalphLifecycle { .. }
        | SessionEventKind::PluginStatusNote { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::LegacyEvent { .. } => true,
        SessionEventKind::SkillSuggested { reason, .. } => reason.is_some(),
        SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ReasoningChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
        | SessionEventKind::SessionRenamed { .. }
        | SessionEventKind::SessionImported { .. }
        | SessionEventKind::SessionForked { .. }
        | SessionEventKind::SkillActivated { .. }
        | SessionEventKind::SkillDeactivated { .. }
        | SessionEventKind::SkillContextLoaded { .. }
        | SessionEventKind::TraceEvent { .. } => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
                text: "live".to_owned(),
            },
        });

        assert!(matches!(
            &app.session_view_snapshot().transcript.items[2].kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "live"
        ));
    }

    #[test]
    #[allow(clippy::too_many_lines)]
    fn shared_projection_matches_tui_semantics_for_migration_slices() {
        let session_id = bcode_session_models::SessionId::new();
        let event = |sequence, kind| bcode_session_models::SessionEvent {
            schema_version: bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: sequence,
            session_id,
            provenance: None,
            kind,
        };
        let events = [
            event(
                1,
                SessionEventKind::AssistantReasoningMessage {
                    text: "reasoned".to_owned(),
                },
            ),
            event(
                2,
                SessionEventKind::ToolCallRequested {
                    tool_call_id: "tool-1".to_owned(),
                    producer_plugin_id: Some("shell".to_owned()),
                    tool_name: "shell.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    working_directory: None,
                    request_visual: None,
                    legacy_request_presentation: None,
                },
            ),
            event(
                3,
                SessionEventKind::ToolInvocationStream {
                    event: ToolInvocationStreamEvent::OutputDelta {
                        tool_call_id: "tool-1".to_owned(),
                        sequence: 1,
                        stream: ToolOutputStream::Stdout,
                        text: "tool output".to_owned(),
                        byte_len: 11,
                    },
                },
            ),
            event(
                4,
                SessionEventKind::PermissionRequested {
                    permission_id: "permission-1".to_owned(),
                    tool_call_id: "tool-1".to_owned(),
                    producer_plugin_id: Some("shell".to_owned()),
                    tool_name: "shell.run".to_owned(),
                    arguments_json: "{}".to_owned(),
                    legacy_request_presentation: None,
                    batch: None,
                    policy_source: None,
                    policy_reason: None,
                },
            ),
            event(
                5,
                SessionEventKind::PermissionResolved {
                    permission_id: "permission-1".to_owned(),
                    approved: true,
                },
            ),
            event(
                6,
                SessionEventKind::RuntimeWorkStarted {
                    work_id: bcode_session_models::WorkId::new("work-1"),
                    kind: bcode_session_models::RuntimeWorkKind::Tool,
                    label: "shell".to_owned(),
                    tool_call_id: Some("tool-1".to_owned()),
                    plugin_id: Some("shell".to_owned()),
                    service_interface: None,
                    operation: None,
                    parent_work_id: None,
                    started_at_ms: Some(6),
                    cancellable: true,
                },
            ),
            event(
                7,
                SessionEventKind::RuntimeWorkProgress {
                    work_id: bcode_session_models::WorkId::new("work-1"),
                    message: "halfway".to_owned(),
                    completed_units: Some(1),
                    total_units: Some(2),
                    progress_at_ms: Some(7),
                },
            ),
        ];
        let mut app = BmuxApp::new_with_history(Some(session_id), &events[..5], &[], false);
        for event in &events[5..] {
            app.absorb_session_event(event);
        }
        let legacy = app.transcript();
        let shared = app.session_view_snapshot();

        assert!(legacy.iter().any(|item| matches!(
            item.kind(),
            super::super::transcript::TranscriptItemKind::ReasoningMessage
        )));
        assert!(shared.transcript.items.iter().any(|item| matches!(
            &item.kind,
            bcode_session_view_models::TranscriptViewItemKind::ReasoningMessage { message }
                if message.text == "reasoned"
        )));
        assert!(legacy.iter().any(|item| matches!(
            item.kind(),
            super::super::transcript::TranscriptItemKind::ToolRequest { tool_call_id, .. }
                if tool_call_id == "tool-1"
        )));
        assert_eq!(
            shared
                .tools
                .get("tool-1")
                .and_then(|tool| tool.output.as_ref())
                .map(|output| output.text.as_str()),
            Some("tool output")
        );
        assert!(legacy.iter().any(|item| matches!(
            item.kind(),
            super::super::transcript::TranscriptItemKind::PermissionResult { approved: true }
        )));
        assert!(shared.permissions.is_empty());
        assert!(shared.transcript.items.iter().any(|item| matches!(
            &item.kind,
            bcode_session_view_models::TranscriptViewItemKind::Permission { permission }
                if permission.permission_id == "permission-1"
                    && permission.resolved
                    && permission.approved == Some(true)
        )));
        assert_eq!(
            bcode_session_view_models::runtime_work_status_label(&shared.runtime_work).as_deref(),
            Some("running tool: shell — halfway")
        );
        assert!(shared.runtime_work.iter().any(|work| {
            work.work_id == bcode_session_models::WorkId::new("work-1")
                && work.message.as_deref() == Some("halfway")
                && work.completed_units == Some(1)
                && work.total_units == Some(2)
        }));
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

        let app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);
        assert_eq!(app.transcript.len(), 1);
        let item = &app.transcript.items()[0];
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
        assert!(app.session_view_snapshot().active_skills.contains("review"));
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
        assert_eq!(app.status(), "deactivated skill: event-skill");
    }

    #[test]
    fn ralph_runtime_failure_notice_consumes_shared_projection() {
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

        let shared = app
            .shared_runtime_work_item("ralph:loop")
            .expect("shared runtime work item");
        assert_eq!(shared.status, RuntimeWorkStatus::Failed);
        assert_eq!(shared.message.as_deref(), Some("boom"));
        let terminal = app.transcript().iter().last().expect("ralph notice");
        assert_eq!(terminal.role(), "System");
        assert_eq!(terminal.text(), "Ralph work Failed: boom");
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
        assert!(matches!(app.activity(), ActivityState::Idle));
    }

    #[test]
    fn authoritative_runtime_work_snapshot_drives_tui_activity() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.apply_runtime_work_snapshots(&[bcode_ipc::RuntimeWorkSnapshot {
            work_id: bcode_session_models::WorkId::new("tool-1"),
            kind: bcode_session_models::RuntimeWorkKind::Tool,
            label: "shell".to_owned(),
            tool_call_id: Some("call-1".to_owned()),
            status: bcode_session_models::RuntimeWorkStatus::Running,
            cancellable: true,
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
        assert!(app.session_view_snapshot().runtime_work.is_empty());
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
            },
        }];

        let app = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

        assert_eq!(app.reasoning_effort(), Some("high"));
        assert_eq!(app.reasoning_summary(), Some("detailed"));
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
        assert_eq!(app.active_skill_count(), 0);
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
        let actual = app
            .transcript()
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

        let terminal = app.transcript().iter().collect::<Vec<_>>();
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
        assert_eq!(
            app.status(),
            "loaded skill context: review (42 bytes truncated)"
        );
        assert_eq!(terminal[2].role(), "Skill error");
        assert_eq!(terminal[2].text(), "review: boom");
        assert_eq!(terminal[2].kind(), &TranscriptItemKind::SkillError);
        assert_eq!(
            app.session_view_snapshot()
                .transcript
                .items
                .iter()
                .map(terminal_item_from_shared)
                .map(|item| (item.role(), item.text().to_owned(), item.kind().clone()))
                .collect::<Vec<_>>(),
            terminal
                .iter()
                .map(|item| (item.role(), item.text().to_owned(), item.kind().clone()))
                .collect::<Vec<_>>()
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
        assert_eq!(
            app.session_view_snapshot()
                .runtime
                .reasoning_summary
                .as_deref(),
            Some("detailed")
        );
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
            .as_ref()
            .expect("hydrated occupancy");
        let meter = TokenUsageMeter {
            context_window: Some(10_000),
            ..TokenUsageMeter::default()
        };

        assert!(hydrated.observation.context_tokens.is_estimated());
        assert_eq!(meter.context_summary(Some(hydrated)), "~2,500/10k 25%");
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
            .as_ref()
            .expect("live occupancy");
        let meter = TokenUsageMeter {
            context_window: Some(10_000),
            ..TokenUsageMeter::default()
        };

        assert!(!live.observation.context_tokens.is_estimated());
        assert_eq!(meter.context_summary(Some(live)), "2,500/10k 25%");
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
        assert!(
            app.session_view_snapshot()
                .runtime
                .context_occupancy
                .is_none()
        );
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
        assert_eq!(app.status(), "cancellation requested");
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
        let actual = app.transcript().iter().last().expect("terminal error item");
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
        assert_eq!(app.status(), "session: renamed session");
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
        let actual = app.transcript().iter().last().expect("terminal item");
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
        let terminal = app.transcript().iter().last().expect("compaction item");
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(8))
            .expect("shared compaction item");
        let expected = terminal_item_from_shared(shared);
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
            .expect("provider compaction item");
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(9))
            .expect("shared provider compaction item");
        let expected = terminal_item_from_shared(shared);
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

        assert!(app.pending_submissions().is_empty());
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
        let actual = app.transcript().iter().last().expect("user item");
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
        assert_eq!(app.transcript().len(), 1);
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
        let actual = app.transcript().iter().last().expect("assistant item");
        assert_eq!(actual.role(), "Assistant");
        assert_eq!(actual.text(), "final shared");
        assert!(!actual.streaming());
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
        let actual = app.transcript().iter().last().expect("reasoning item");
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
        assert_eq!(
            app.session_view_snapshot()
                .transcript
                .items
                .iter()
                .map(terminal_item_from_shared)
                .map(|item| (item.role(), item.text().to_owned(), item.streaming()))
                .collect::<Vec<_>>(),
            terminal
                .iter()
                .map(|item| (item.role(), item.text().to_owned(), item.streaming()))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn generic_terminal_items_are_adapted_from_shared_projection() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let events = shared_projection_terminal_adapter_events(SessionId::new());
        for event in &events {
            app.absorb_session_event(event);
        }

        let terminal = app.transcript().iter().collect::<Vec<_>>();
        assert_eq!(terminal.len(), events.len());
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
    fn active_tool_calls_follow_shared_tool_status() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
        assert!(app.active_tool_calls.contains("tool-shared"));
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
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-shared".to_owned(),
                result: "shared result".to_owned(),
                is_error: false,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "shared result".to_owned(),
                }),
                output: None,
            },
        ));

        assert!(!app.active_tool_calls.contains("tool-shared"));
        assert!(matches!(
            app.session_view_snapshot()
                .tools
                .get("tool-shared")
                .expect("shared finished tool")
                .status,
            bcode_session_view_models::ToolInvocationViewStatus::Finished
        ));
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
        let context = app
            .tool_call_contexts
            .get("tool-shared")
            .expect("terminal tool context");
        assert_eq!(
            context.tool_name,
            tool.tool_name.as_deref().unwrap_or_default()
        );
        assert_eq!(
            context.arguments_json,
            tool.arguments_json.as_deref().unwrap_or_default()
        );
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| item.sequence == Some(1))
            .expect("shared tool item");
        let expected = terminal_item_from_shared(shared);
        let actual = app.transcript().iter().last().expect("terminal tool item");
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
    }

    #[test]
    fn tool_output_delta_consumes_shared_projection_text() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "tool-shared".to_owned(),
                    sequence: 1,
                    stream: ToolOutputStream::Stdout,
                    text: "hello ".to_owned(),
                    byte_len: 6,
                },
            },
        ));
        app.absorb_session_event(&event(
            3,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::OutputDelta {
                    tool_call_id: "tool-shared".to_owned(),
                    sequence: 2,
                    stream: ToolOutputStream::Stdout,
                    text: "world".to_owned(),
                    byte_len: 5,
                },
            },
        ));

        let shared = app
            .shared_tool_output_item("tool-shared")
            .expect("shared tool output item");
        assert_eq!(shared.text(), "hello world");
        let actual = app
            .transcript()
            .iter()
            .last()
            .expect("terminal output item");
        assert_eq!(actual.role(), shared.role());
        assert_eq!(actual.text(), shared.text());
        assert_eq!(actual.kind(), shared.kind());
        assert!(actual.streaming());
    }

    #[test]
    fn tool_result_status_consumes_shared_projection() {
        let session_id = SessionId::new();
        let mut app = BmuxApp::new_with_history(Some(session_id), &[], &[], false);
        let event = |sequence, kind| shared_projection_adapter_event(session_id, sequence, kind);

        app.absorb_session_event(&event(1, shared_tool_call_request_kind()));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-shared".to_owned(),
                result: "shared failure".to_owned(),
                is_error: true,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "shared failure".to_owned(),
                }),
                output: None,
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
        let actual = app.transcript().iter().last().expect("terminal tool item");
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
            SessionEventKind::ToolCallFinished {
                tool_call_id: "tool-shared".to_owned(),
                result: "shared result".to_owned(),
                is_error: false,
                semantic_result: Some(ToolInvocationResult::Text {
                    text: "shared result".to_owned(),
                }),
                output: None,
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
                            )
                )
            })
            .expect("shared finished tool item");
        let expected = terminal_item_from_shared(shared);
        let actual = terminal.last().expect("terminal result item");
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.event_sequence(), shared.sequence);
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
        let actual = app.transcript().iter().last().expect("terminal item");
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
            .expect("terminal permission item");
        assert_eq!(actual.role(), expected.role());
        assert_eq!(actual.text(), expected.text());
        assert_eq!(actual.kind(), expected.kind());
        assert_eq!(actual.event_sequence(), shared.sequence);
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
            request_visual: None,
            legacy_request_presentation: None,
        }
    }

    fn shared_permission_request_kind() -> SessionEventKind {
        SessionEventKind::PermissionRequested {
            permission_id: "permission-shared".to_owned(),
            tool_call_id: "tool-shared".to_owned(),
            producer_plugin_id: Some("bcode.shell".to_owned()),
            tool_name: "shell.run".to_owned(),
            arguments_json: r#"{"command":"printf shared"}"#.to_owned(),
            legacy_request_presentation: None,
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
    fn tool_started_enriches_request_visual_for_artifact_driven_live_rendering() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let session_id = SessionId::new();
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
            SessionEventKind::ToolCallRequested {
                tool_call_id: "call".to_owned(),
                producer_plugin_id: Some("bcode.shell".to_owned()),
                tool_name: "shell.run".to_owned(),
                arguments_json: r#"{"command":"printf hello"}"#.to_owned(),
                working_directory: None,
                request_visual: Some(bcode_session_models::PluginVisualDescriptor {
                    visual_id: None,
                    producer_plugin_id: Some("bcode.shell".to_owned()),
                    schema: "bcode.tool.request.shell.run".to_owned(),
                    schema_version: 1,
                    title: Some("Shell command".to_owned()),
                    subtitle: None,
                    payload: serde_json::json!({"command": "printf hello"}),
                }),
                legacy_request_presentation: None,
            },
        ));
        app.absorb_session_event(&event(
            2,
            SessionEventKind::ToolInvocationStream {
                event: ToolInvocationStreamEvent::Started {
                    tool_call_id: "call".to_owned(),
                    tool_name: "shell.run".to_owned(),
                    sequence: 1,
                    terminal: true,
                    columns: Some(91),
                    rows: Some(37),
                    started_at_ms: None,
                },
            },
        ));
        let visuals = app.active_plugin_visuals();
        let runtime = visuals[0]
            .1
            .payload
            .get("_bcode_runtime")
            .expect("runtime metadata");
        assert_eq!(
            runtime
                .get("live_state_key")
                .and_then(serde_json::Value::as_str),
            Some("call")
        );
        assert_eq!(
            runtime.get("columns").and_then(serde_json::Value::as_u64),
            Some(91)
        );
        assert_eq!(
            runtime.get("rows").and_then(serde_json::Value::as_u64),
            Some(37)
        );
        assert_eq!(
            runtime
                .get("streaming")
                .and_then(serde_json::Value::as_bool),
            Some(true)
        );
        assert_eq!(
            runtime.get("output").and_then(serde_json::Value::as_str),
            Some("")
        );
        let request_runtime = app
            .transcript()
            .iter()
            .find_map(|item| match item.kind() {
                TranscriptItemKind::ToolRequest {
                    request_visual: Some(visual),
                    ..
                } => visual.payload.get("_bcode_runtime"),
                _ => None,
            })
            .expect("transcript request runtime metadata");
        assert_eq!(
            request_runtime
                .get("live_state_key")
                .and_then(serde_json::Value::as_str),
            Some("call")
        );
        assert!(app.transcript()[0].streaming());
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
        let fallback = app.transcript().last().expect("contribution transcript");
        assert_eq!(fallback.role(), "Tool contribution");
        assert!(fallback.text().contains("future.unknown/schema"));
        assert!(fallback.text().contains("opaque-tui"));
    }

    #[test]
    fn transient_contribution_updates_and_removes_one_live_fallback() {
        let session_id = bcode_session_models::SessionId::new();
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let live = |sequence, operation, sentinel| bcode_session_models::SessionLiveEvent {
            session_id,
            kind: SessionLiveEventKind::ToolContribution {
                event: bcode_session_models::ToolContributionEvent {
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
        assert_eq!(app.transcript().len(), 1);
        assert!(app.transcript()[0].text().contains("second"));
        assert!(!app.transcript()[0].text().contains("first"));
        let shared = app
            .session_view_snapshot()
            .transcript
            .items
            .iter()
            .find(|item| {
                matches!(
                    &item.kind,
                    bcode_session_view_models::TranscriptViewItemKind::ToolContribution {
                        contribution,
                    } if contribution.sequence == 2
                )
            })
            .expect("shared live contribution transcript item");
        let expected = terminal_item_from_shared(shared);
        assert_eq!(app.transcript()[0].role(), expected.role());
        assert_eq!(app.transcript()[0].text(), expected.text());

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
        assert!(app.transcript().is_empty());
    }

    #[test]
    fn lifecycle_start_enriches_the_canonical_tool_request_visual() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let session_id = bcode_session_models::SessionId::new();
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
            bcode_session_models::SessionEventKind::ToolCallRequested {
                tool_call_id: "call".to_owned(),
                tool_name: "shell.run".to_owned(),
                arguments_json: "{\"command\":\"echo test\"}".to_owned(),
                working_directory: None,
                producer_plugin_id: Some("bcode.shell".to_owned()),
                request_visual: Some(bcode_session_models::PluginVisualDescriptor {
                    visual_id: None,
                    producer_plugin_id: Some("bcode.shell".to_owned()),
                    schema: "bcode.tool.request.shell.run".to_owned(),
                    schema_version: 1,
                    title: None,
                    subtitle: None,
                    payload: serde_json::json!({"command":"echo test"}),
                }),
                legacy_request_presentation: None,
            },
        ));
        app.absorb_session_event(&event(
            2,
            bcode_session_models::SessionEventKind::ToolInvocationLifecycle {
                event: bcode_session_models::ToolInvocationLifecycleEvent {
                    invocation_id: "call".to_owned(),
                    sequence: 0,
                    stage: bcode_session_models::ToolInvocationLifecycleStage::Started,
                    message: None,
                    metadata: serde_json::Value::Null,
                },
            },
        ));

        let context = app
            .tool_call_contexts
            .get("call")
            .expect("tool call context");
        assert_eq!(
            context
                .request_visual
                .as_ref()
                .and_then(|visual| visual.payload.get("_bcode_runtime"))
                .and_then(|runtime| runtime.get("live_state_key"))
                .and_then(serde_json::Value::as_str),
            Some("call")
        );
    }

    #[test]
    fn shell_contribution_uses_terminal_json_fallback() {
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
        let fallback = app
            .transcript()
            .last()
            .expect("shell contribution transcript");
        assert_eq!(fallback.role(), "Tool contribution");
        assert!(fallback.text().contains("bcode.shell.run.summary"));
        assert!(fallback.text().contains("shell-render-sentinel"));
    }

    #[test]
    fn active_artifact_revisions_ignore_stale_and_duplicate_updates() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        let event = |revision| SessionLiveEvent {
            session_id: SessionId::new(),
            kind: SessionLiveEventKind::ToolOutputDelta {
                event: ToolInvocationStreamEvent::ArtifactUpdate {
                    tool_call_id: "call".to_owned(),
                    sequence: revision,
                    artifact_id: "artifact".to_owned(),
                    reference_key: "recording".to_owned(),
                    producer_plugin_id: "plugin".to_owned(),
                    schema: "plugin.recording".to_owned(),
                    schema_version: 1,
                    content_type: None,
                    storage_uri: String::new(),
                    committed_bytes: revision,
                    revision,
                    availability: None,
                    finalized: false,
                },
            },
        };
        app.absorb_session_live_event(&event(3));
        app.absorb_session_live_event(&event(2));
        app.absorb_session_live_event(&event(3));
        assert_eq!(
            app.active_artifact_revision("call", "artifact", "recording"),
            Some(3)
        );
    }
}
