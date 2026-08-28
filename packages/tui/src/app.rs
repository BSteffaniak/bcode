//! TUI app state.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime};

use bcode_config::{
    TuiAccentTransitionCurve, TuiConfig, TuiDiffViewerConfig, TuiDiffViewerLayout, TuiThemeConfig,
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
use super::transcript_resident_window::{TranscriptResidentWindow, TranscriptWindowPolicy};
use super::transcript_viewport::TranscriptViewport;

const MANUAL_TRANSCRIPT_SCROLL_GRACE: Duration = Duration::from_millis(450);
const TRANSCRIPT_SCROLL_ANIMATION_DURATION: Duration = Duration::from_millis(180);
const TRANSCRIPT_SCROLL_ANIMATION_FRAME: Duration = Duration::from_millis(16);
const TRANSCRIPT_SCROLL_ANIMATION_INVALIDATION_KEY: &str = "transcript-scroll-animation";
const LATEST_BAR_ANIMATION_INVALIDATION_KEY: &str = "latest-bar-animation";
const THEME_TRANSITION_INVALIDATION_KEY: &str = "theme-transition";
const ACTIVITY_ANIMATION_INVALIDATION_KEY: &str = "activity-animation";
const ACTIVITY_ANIMATION_FRAME: Duration = Duration::from_millis(100);
const THEME_TRANSITION_FRAME: Duration = Duration::from_millis(16);
const LATEST_BAR_ACTIVE_WINDOW: Duration = Duration::from_millis(420);
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
struct TranscriptScrollAnimation {
    start_top_row: usize,
    target_top_row: usize,
    started_at: Instant,
    duration: Duration,
    next_frame_at: Instant,
}

impl TranscriptScrollAnimation {
    fn new(start_top_row: usize, target_top_row: usize, started_at: Instant) -> Self {
        Self {
            start_top_row,
            target_top_row,
            started_at,
            duration: TRANSCRIPT_SCROLL_ANIMATION_DURATION,
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

    fn set_target(&mut self, target: Color, config: &TuiThemeConfig, now: Instant) {
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
            self.next_frame_at = None;
            return;
        }
        self.source_accent = self.accent_at(now);
        self.displayed_accent = self.source_accent;
        self.target_accent = target;
        self.started_at = now;
        self.duration = Duration::from_millis(duration_ms);
        self.curve = config.accent_transition_curve;
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

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StableTranscriptAnchor {
    pub item_id: TranscriptPresentationEntryId,
    pub row_in_item: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingReasoningEffort {
    value: String,
    generation: u64,
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
    theme_catalog: Option<DiscoveredThemes>,
    #[cfg(test)]
    theme_catalog_discovery_count: usize,
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
    composer: TextInputState,
    input_history: InputHistory,
    transcript: TranscriptDocument,
    transcript_projection_revision: u64,
    active_interaction_layout: Option<(String, u16)>,
    session_view_terminal_adapter: SessionViewTerminalAdapter,
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
    viewport: TranscriptViewport,
    manual_transcript_scroll_until: Option<Instant>,
    transcript_scroll_animation: Option<TranscriptScrollAnimation>,
    scroll_mode: TranscriptScrollMode,
    pending_visual_overflow_bottom: Option<usize>,
    pending_stable_transcript_anchor: Option<StableTranscriptAnchor>,
    latest_hidden_activity_at: Option<Instant>,
    latest_hidden_activity_burst: u8,
    latest_bar_animation_started_at: Instant,
    latest_bar_next_frame_at: Option<Instant>,
    submitted_user_message_following: SubmittedUserMessageFollowing,
    assistant_scroll_anchor: AssistantScrollAnchorState,
    pending_assistant_stream_anchor: bool,
    pending_transcript_top_anchor_sequence: Option<u64>,
    older_history: OlderHistoryState,
    activity: ActivityState,
    activity_started_at: Instant,
    daemon_connection: DaemonConnectionState,
    execution_mode_indicator: Option<String>,
    status: String,
    key_hints: String,
    jump_to_latest_key_label: String,
    tui_config: TuiConfig,
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
    last_transcript_damage: TranscriptDocumentDamage,
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
        let initial_theme = super::theme::resolve_initial_theme();
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
            theme_catalog: None,
            #[cfg(test)]
            theme_catalog_discovery_count: 0,
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
            composer: TextInputState::new(TextEditBuffer::new()),
            input_history: InputHistory::from_entries(input_history),
            transcript: TranscriptDocument::default(),
            transcript_projection_revision: 0,
            active_interaction_layout: None,
            session_view_terminal_adapter: SessionViewTerminalAdapter::default(),
            session_view: bcode_session_view::SessionView::new(),
            transcript_window: TranscriptResidentWindow::default(),
            transcript_selection_pinned: false,
            latest_history_sequence: None,
            pending_submissions: PendingSubmissions::default(),
            transcript_layout: TranscriptLayoutCache::default(),
            transcript_markdown_cache:
                crate::transcript_markdown_cache::TranscriptMarkdownCache::default(),
            elapsed_layout_revision: 0,
            elapsed_dirty_visuals: BTreeSet::new(),
            transcript_dirty_items: BTreeSet::new(),
            viewport: TranscriptViewport::default(),
            manual_transcript_scroll_until: None,
            transcript_scroll_animation: None,
            scroll_mode: TranscriptScrollMode::BottomFollow,
            pending_visual_overflow_bottom: None,
            pending_stable_transcript_anchor: None,
            latest_hidden_activity_at: None,
            latest_hidden_activity_burst: 0,
            latest_bar_animation_started_at: now,
            latest_bar_next_frame_at: None,
            submitted_user_message_following: SubmittedUserMessageFollowing::Idle,
            assistant_scroll_anchor: AssistantScrollAnchorState::Idle,
            pending_assistant_stream_anchor: false,
            pending_transcript_top_anchor_sequence: None,
            older_history: OlderHistoryState::new(history, has_older_history),
            activity: ActivityState::Idle,
            activity_started_at: now,
            daemon_connection: DaemonConnectionState::Connecting,
            execution_mode_indicator: None,
            status: String::from("Connecting to daemon… Enter submits; Esc/Ctrl-C exits."),
            key_hints: String::from("enter send · escape interrupt · ctrl+d exit · ctrl+p palette"),
            jump_to_latest_key_label: "ctrl+end".to_owned(),
            tui_config: TuiConfig::default(),
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
            last_transcript_damage: TranscriptDocumentDamage::None,
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
        self.theme_catalog.clone_from(&source.theme_catalog);
        #[cfg(test)]
        {
            self.theme_catalog_discovery_count = source.theme_catalog_discovery_count;
        }
        self.take_theme_transition_state_from(source);
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
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.viewport.follow_anchor(row);
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
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.viewport.follow_anchor(row);
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
            let hint = match self.markdown_interaction.focused_contribution() {
                Some(
                    bcode_markdown_render::MarkdownContributionKind::Link { destination, .. }
                    | bcode_markdown_render::MarkdownContributionKind::GitHubIssue {
                        destination,
                        ..
                    },
                ) if crate::markdown_activation::markdown_destination_text(destination)
                    .is_some() =>
                {
                    "Alt+Enter opens; Ctrl+Alt+C copies; Ctrl/Alt+Tab moves"
                }
                _ => "Alt+Enter activates; Ctrl/Alt+Tab moves",
            };
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
        self.viewport.sync_max(
            total_rows.saturating_sub(usize::from(viewport_height)),
            0,
            total_rows,
            viewport_height,
            false,
            &mut self.older_history,
        );
        self.viewport.scroll_up(scroll_up, &mut self.older_history);
    }

    #[cfg(test)]
    fn transcript_viewport_top_row_for_test(
        &self,
        total_rows: usize,
        viewport_height: u16,
    ) -> usize {
        self.viewport.top_row(total_rows, viewport_height)
    }

    #[cfg(test)]
    pub(crate) fn frame_observation(&self) -> TranscriptFrameObservation {
        let total_rows = self.transcript_layout.total_rows();
        let viewport_top = self.viewport.top_row(total_rows, self.viewport.height());
        let anchor = self
            .transcript_layout
            .line_at_row(viewport_top)
            .and_then(|line| {
                (line.source == VisibleTranscriptSource::Transcript)
                    .then(|| {
                        self.transcript
                            .presentation_id(line.entry_index)
                            .map(|item_id| StableTranscriptAnchor {
                                item_id,
                                row_in_item: line.row_in_entry,
                            })
                    })
                    .flatten()
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
            damage: self.last_transcript_damage.clone(),
            viewport_top,
            scroll_mode: match self.scroll_mode {
                TranscriptScrollMode::BottomFollow => "bottom_follow",
                TranscriptScrollMode::TransitionToEntry { .. } => "transition_to_entry",
                TranscriptScrollMode::AnchoredToEntry { .. } => "anchored_to_entry",
                TranscriptScrollMode::ManualDetached => "manual_detached",
            },
            anchor,
        }
    }

    fn preserve_transcript_anchor_for_disclosure(&mut self) {
        self.transcript_scroll_animation = None;
        self.pending_visual_overflow_bottom = None;
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        let top_row = self
            .viewport
            .top_row(self.transcript_layout.total_rows(), self.viewport.height());
        self.viewport.materialize_top_row(top_row);
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

    /// Activate the currently focused Markdown contribution.
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

    /// Replace the resident transcript with a fresh bounded tail snapshot.
    pub fn replace_latest_transcript_window(
        &mut self,
        events: &[SessionEvent],
        has_older_history: bool,
    ) {
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.older_history = OlderHistoryState::new(events, has_older_history);
        self.rebuild_transcript_from_history();
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
        self.latest_history_sequence = events.last().map(|event| event.sequence);
        self.transcript_window.replace_window(events);
        self.older_history
            .replace_centered(events, has_older, has_newer, anchor_sequence);
        self.rebuild_transcript_from_history();
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
                self.theme_catalog_discovery_count =
                    self.theme_catalog_discovery_count.saturating_add(1);
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
        let overlays = self.tui_config.theme.overlays.clone();
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
        self.theme_catalog_discovery_count
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
        self.theme_transition
            .set_target(target.accent, &self.tui_config.theme, Instant::now());
        self.update_theme_animation(Instant::now());
    }

    /// Preview one already-resolved picker theme without repeating discovery or resolution.
    pub(crate) fn preview_resolved_theme(&mut self, theme_id: &str, target: &ResolvedTheme) {
        self.theme_preview_id = Some(theme_id.to_owned());
        self.target_theme = *target;
        self.theme_transition
            .set_target(target.accent, &self.tui_config.theme, Instant::now());
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
        self.theme_transition
            .set_target(target.accent, &self.tui_config.theme, Instant::now());
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

    /// Apply renderer-neutral presentation configuration.
    pub fn apply_presentation_config(&mut self, config: bcode_config::PresentationConfig) -> bool {
        self.apply_streaming_presentation_policy(config.streaming.policy())
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
        self.theme_transition
            .set_target(target_accent, &self.tui_config.theme, now);
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
            .set_target(target.accent, &self.tui_config.theme, now);
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

    /// Return the composer text input state mutably.
    pub const fn composer_state_mut(&mut self) -> &mut TextInputState {
        &mut self.composer
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
                display_name: self.selected_model_id().unwrap_or_default().to_owned(),
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

    #[cfg(test)]
    pub fn stable_transcript_anchor(&self) -> Option<(TranscriptPresentationEntryId, usize)> {
        let top_row = self.transcript_top_row(self.viewport.height());
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
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.transcript_scroll_animation = None;
        self.viewport.follow_anchor(row);
    }

    /// Return the resolved top row for latest-user-message anchoring.
    #[cfg(test)]
    pub fn latest_user_message_start_row_for_test(&self) -> Option<usize> {
        self.latest_user_message_start_row()
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
        self.scroll_mode = TranscriptScrollMode::ManualDetached;
        self.viewport.follow_anchor(top_row);
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

    /// Request loading older transcript history for tests and selection-owned viewport flows.
    #[cfg(test)]
    pub(crate) const fn request_older_history_for_test(&mut self, reveal_rows: usize) {
        self.older_history.request_load(reveal_rows);
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
        self.reasoning_display_mode = config.mode;
        self.session_view
            .set_reasoning_display_mode(reasoning_view_mode(config.mode));
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
            self.rebuild_transcript_from_history();
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
        let text = self.composer.buffer().text().to_owned();
        self.submitted_user_message_following = SubmittedUserMessageFollowing::Idle;
        self.assistant_scroll_anchor = AssistantScrollAnchorState::Idle;
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
            self.latest_bar_next_frame_at = None;
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
        self.latest_bar_next_frame_at = None;
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
        self.restore_stable_transcript_anchor();
        self.resolve_visual_overflow_follow(total_rows, now);
    }

    fn resolve_visual_overflow_follow(&mut self, total_rows: usize, now: Instant) {
        let Some(previous_bottom) = self.pending_visual_overflow_bottom.take() else {
            if !self.newer_transcript_content_below() {
                self.latest_hidden_activity_at = None;
                self.latest_hidden_activity_burst = 0;
                self.latest_bar_next_frame_at = None;
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
                self.latest_bar_next_frame_at = None;
            }
            return;
        }
        self.scroll_mode = TranscriptScrollMode::BottomFollow;
        self.latest_hidden_activity_at = None;
        self.latest_hidden_activity_burst = 0;
        self.latest_bar_next_frame_at = None;
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
        self.latest_bar_animation_started_at = now;
        let frame_duration = latest_bar_active_frame_duration(self.latest_hidden_activity_burst);
        let next_frame = next_latest_bar_frame_at(now, now, frame_duration);
        self.latest_bar_next_frame_at = Some(next_frame);
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
        let Some(index) = self.transcript.iter().rposition(|item| {
            item.role() == "Assistant" && item.streaming() && !item.text().is_empty()
        }) else {
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
        matches!(self.scroll_mode, TranscriptScrollMode::ManualDetached)
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
        self.transcript_window.append_history(events);
        for event in events {
            self.session_view.apply_event(event);
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.apply_session_view_terminal_adapter();
        self.trim_resident_transcript_window_if_needed();
    }

    fn rebuild_transcript_from_history(&mut self) {
        let events = self.transcript_window.events().to_vec();
        self.session_view.clear_history_window();
        self.session_view_terminal_adapter.reset();
        for event in &events {
            self.session_view.apply_event(event);
            self.apply_session_event(event, SessionEventApplication::Replay);
        }
        self.apply_session_view_terminal_adapter();
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

    /// Return whether resident transcript trimming is currently permitted.
    #[cfg(test)]
    #[must_use]
    pub fn can_trim_resident_transcript_window_for_test(&self) -> bool {
        self.can_trim_resident_transcript_window()
    }

    fn can_trim_resident_transcript_window(&self) -> bool {
        matches!(self.scroll_mode, TranscriptScrollMode::BottomFollow)
            && self.viewport.at_bottom_threshold()
            && self.transcript_scroll_animation.is_none()
            && !self.manual_transcript_scroll_active()
            && !self.transcript_selection_pinned
            && !self.active_tool_loop()
            && !self.pending_assistant_stream_anchor
            && !self.older_history.loading()
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
        self.apply_session_view_terminal_adapter();
        self.trim_resident_transcript_window_if_needed();
    }

    /// Absorb one live-only session event.
    pub fn absorb_session_live_event(&mut self, event: &SessionLiveEvent) {
        self.session_view.apply_live_event(event);
        self.apply_session_view_terminal_adapter();
        self.apply_session_live_event_side_effects(event);
    }

    fn apply_session_live_event_side_effects(&mut self, event: &SessionLiveEvent) {
        match &event.kind {
            SessionLiveEventKind::AssistantTextStreamUpdated { .. } => {
                let should_anchor = self.should_anchor_new_assistant_stream();
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.maybe_request_assistant_stream_anchor(should_anchor);
            }
            SessionLiveEventKind::AssistantTextDelta { text, .. } => {
                let should_anchor = self.should_anchor_new_assistant_stream();
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
                self.add_streaming_delta(text, SessionEventApplication::Live);
                self.maybe_request_assistant_stream_anchor(should_anchor);
            }
            SessionLiveEventKind::AssistantReasoningDelta { .. }
            | SessionLiveEventKind::AssistantReasoningTextStreamUpdated { .. }
            | SessionLiveEventKind::AssistantReasoningActivity { .. } => {
                self.pending_visual_overflow_bottom = Some(
                    self.viewport
                        .bottom_row(self.transcript_layout.total_rows()),
                );
                self.viewport.preserve_for_append();
            }
            SessionLiveEventKind::ToolContributionPlaced { .. }
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
                self.maybe_request_assistant_stream_anchor(should_anchor);
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
            SessionEventKind::SessionRenamed { .. } => self.apply_shared_session_renamed(),
            SessionEventKind::SkillSuggested {
                skill_id,
                reason: _,
                ..
            } => {
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
                next_periodic_deadline(self.activity_started_at, now, ACTIVITY_ANIMATION_FRAME),
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
                if self.activity_animation_active() {
                    damage.insert(TemporalDamage::Status);
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
        self.latest_hidden_activity_at
            .is_some_and(|at| now.saturating_duration_since(at) < LATEST_BAR_ACTIVE_WINDOW)
    }

    fn handle_transcript_scroll_animation(&mut self, now: Instant) -> bool {
        let Some(mut animation) = self.transcript_scroll_animation else {
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
                    self.latest_hidden_activity_burst = 0;
                    self.latest_bar_next_frame_at = None;
                    self.viewport.scroll_to_bottom(&mut self.older_history);
                }
                TranscriptScrollMode::AnchoredToEntry { .. }
                | TranscriptScrollMode::ManualDetached => {}
            }
        } else {
            animation.advance_frame(now);
            self.transcript_scroll_animation = Some(animation);
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
        let height = self.viewport.height();
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
        let accepted_pending_submission = self.pending_submissions.contains(&text);
        self.remove_pending_submission(&text);
        if accepted_pending_submission {
            self.submitted_user_message_following = SubmittedUserMessageFollowing::PendingAnchor;
            self.scroll_mode = TranscriptScrollMode::TransitionToEntry { sticky: false };
            self.pending_transcript_top_anchor_sequence = Some(sequence);
        }
        if application.live_activity() {
            self.set_activity(ActivityState::PreparingModelRequest);
        }
    }

    fn capture_stable_transcript_anchor(&mut self) {
        if self.viewport.follows_bottom() || self.pending_stable_transcript_anchor.is_some() {
            return;
        }
        let top_row = self
            .viewport
            .top_row(self.transcript_layout.total_rows(), self.viewport.height());
        let Some(line) = self
            .transcript_layout
            .visible_lines_from_top(top_row, 1)
            .first()
            .copied()
        else {
            return;
        };
        if line.source != VisibleTranscriptSource::Transcript {
            return;
        }
        let Some(item_id) = self.transcript.presentation_id(line.entry_index) else {
            return;
        };
        self.pending_stable_transcript_anchor = Some(StableTranscriptAnchor {
            item_id,
            row_in_item: line.row_in_entry,
        });
    }

    pub(crate) fn restore_stable_transcript_anchor(&mut self) {
        let Some(anchor) = self.pending_stable_transcript_anchor.take() else {
            return;
        };
        let Some(index) = self.transcript.presentation_index(anchor.item_id) else {
            return;
        };
        let Some(start_row) = self
            .transcript_layout
            .entry_start_row(VisibleTranscriptSource::Transcript, index)
        else {
            return;
        };
        self.viewport
            .follow_anchor(start_row.saturating_add(anchor.row_in_item));
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
            self.last_transcript_damage.clone_from(&damage);
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
}

pub const fn composer_policy() -> TextInputPolicy {
    TextInputPolicy::chat_composer()
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
    fn absorb(&mut self, usage: &bcode_session_models::SessionTokenUsage) {
        self.latest_cached_input_tokens = usage.cached_input_tokens;
        self.latest_cache_write_input_tokens = usage.cache_write_input_tokens;
    }

    fn apply_model_info(&mut self, model: Option<&bcode_model::ModelInfo>) {
        if let Some(model) = model {
            self.context_window = model.context_window;
        }
    }

    fn clear_model_info(&mut self) {
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
        for (currency, cost_micros) in &cost.totals_micros {
            if currency == "USD" {
                parts.push(format!("~{}", format_usd_micros(*cost_micros)));
            } else {
                parts.push(format!(
                    "~{currency} {:.6}",
                    *cost_micros as f64 / 1_000_000.0
                ));
            }
        }
        if cost.unavailable_usage_count > 0 {
            parts.push("cost partial".to_string());
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
        SessionEventKind::UserMessage { .. }
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
        | SessionEventKind::ToolInvocationLifecycle { .. }
        | SessionEventKind::RalphLifecycle { .. }
        | SessionEventKind::PluginStatusNote { .. }
        | SessionEventKind::AssistantReasoningDelta { .. }
        | SessionEventKind::AssistantReasoningMessage { .. }
        | SessionEventKind::AssistantReasoningActivity { .. }
        | SessionEventKind::ModelTurnFinished { .. }
        | SessionEventKind::InertHistory { .. } => true,
        SessionEventKind::SkillSuggested { reason, .. } => reason.is_some(),
        SessionEventKind::SessionCreated { .. }
        | SessionEventKind::ClientAttached { .. }
        | SessionEventKind::ClientDetached { .. }
        | SessionEventKind::ModelChanged { .. }
        | SessionEventKind::ReasoningChanged { .. }
        | SessionEventKind::AgentChanged { .. }
        | SessionEventKind::ModelTurnStarted { .. }
        | SessionEventKind::ModelFeatureFidelityNegotiated { .. }
        | SessionEventKind::ModelTurnCancelRequested { .. }
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
    use super::*;
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

        assert_eq!(
            app.transcript()
                .iter()
                .map(|item| item.text().to_owned())
                .collect::<Vec<_>>(),
            ["before", "send failed", "after"]
        );
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

        assert_eq!(
            app.transcript()
                .iter()
                .map(|item| item.text().to_owned())
                .collect::<Vec<_>>(),
            ["first", "notice one", "second", "notice two", "third"]
        );
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

        assert_eq!(
            app.transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["earlier", "later", "local issue"]
        );
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
        assert_eq!(
            before
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["local issue", "eight"]
        );

        let mut after =
            BmuxApp::new_with_history(Some(session_id), &[event(5, "five")], &[], false);
        after.push_ephemeral_system_plain("local issue".to_owned());
        after.replace_transcript_window(&[event(2, "two")], true, true, 2);
        assert_eq!(
            after
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["two", "local issue"]
        );
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

        assert_eq!(
            app.transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["one", "local issue", "three"]
        );
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

        assert_eq!(app.transcript_index_for_sequence(2), Some(2));
    }

    #[test]
    fn ephemeral_notice_has_explicit_local_provenance() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);

        app.push_ephemeral_system_plain("local issue".to_owned());

        assert!(matches!(
            app.transcript.origin(0),
            Some(TranscriptPresentationOrigin::Ephemeral { source, .. })
                if source == "bcode.tui"
        ));
        assert!(app.session_view_snapshot().transcript.items.is_empty());
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

        assert_eq!(
            reconstructed
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["before", "local issue"]
        );
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

        assert!(reconstructed.transcript().is_empty());
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

        assert!(opened.transcript().is_empty());
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

        let reconstructed = BmuxApp::new_with_history(Some(session_id), &history, &[], false);

        assert_eq!(reconstructed.transcript().len(), 1);
        assert_eq!(reconstructed.transcript()[0].text(), "canonical");
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

        assert_eq!(
            completed
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
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
        assert_eq!(app.transcript_index_for_sequence(1), Some(0));
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

        // Replaying the same canonical history reproduces only the durable note.
        let replayed = BmuxApp::new_with_history(
            Some(session_id),
            std::slice::from_ref(&durable_note),
            &[],
            false,
        );
        assert_eq!(
            replayed
                .transcript()
                .iter()
                .map(TranscriptItem::text)
                .collect::<Vec<_>>(),
            ["durable note"]
        );
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
        assert_eq!(
            app.transcript()
                .iter()
                .map(|item| (item.id(), item.text().to_owned()))
                .collect::<Vec<_>>(),
            transcript_before
        );
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
                accent_transition_ms: 100,
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
        assert_eq!(
            invalidation_deadline(
                &app,
                THEME_TRANSITION_INVALIDATION_KEY,
                queried_at + Duration::from_millis(200),
            ),
            None
        );
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
        assert_ne!(app.presented_theme().fingerprint, valid.fingerprint);
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
        assert_eq!(app.presented_theme().fingerprint, configured.fingerprint);
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
        assert_ne!(app.presented_theme().fingerprint, valid.fingerprint);
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

        assert!(!catalog.entries.is_empty());
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

        assert_eq!(app.theme_catalog_discovery_count(), 1);
    }

    #[test]
    fn theme_preview_apply_and_cancel_are_in_memory_transitions() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
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
        assert!(!app.preview_theme("does-not-exist"));
    }

    #[test]
    fn stream_and_tool_header_rendering_use_active_semantic_theme() {
        let source = include_str!("render.rs");
        let stream_start = source.find("if let Some(integrity)").expect("stream block");
        let stream_end = source[stream_start..]
            .find("match item.kind()")
            .map(|offset| stream_start + offset)
            .expect("stream block end");
        let tool_start = source
            .find("fn push_tool_block_header")
            .expect("tool header");
        let tool_end = source[tool_start..]
            .find("fn tool_block_title_with_timing")
            .map(|offset| tool_start + offset)
            .expect("tool header end");

        for block in [
            &source[stream_start..stream_end],
            &source[tool_start..tool_end],
        ] {
            assert!(block.contains("semantic_state_theme()"));
            assert!(!block.contains("Color::"));
        }
    }

    #[test]
    fn picker_render_paths_do_not_reintroduce_fixed_palette_colors() {
        for source in [
            include_str!("picker_render.rs"),
            include_str!("model_picker_render.rs"),
            include_str!("provider_picker_render.rs"),
            include_str!("session_picker_render.rs"),
            include_str!("skill_picker_render.rs"),
            include_str!("worktree_picker_render.rs"),
        ] {
            assert!(!source.contains("Color::"));
            assert!(!source.contains("PICKER_BG"));
        }
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
        assert_eq!(
            app.transcript_item_row_range(index)
                .expect("restored fallback range")
                .len(),
            fallback
        );
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
        assert_eq!(
            app.transcript_dirty_items_for_test(),
            &BTreeSet::from([first_id, second_id])
        );
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
        assert_eq!(app.transcript()[1].text(), "later");
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
        assert_eq!(app.status(), "Navigated to Markdown section");
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
        assert_eq!(app.transcript_viewport_top_row_for_test(100, 6), 38);
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
        assert_eq!(app.status(), "Markdown fragment target was not found");
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
        assert_eq!(app.scroll_offset(), 47 - top_row - 10);
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
        assert_eq!(app.markdown_presentation_revision(), initial + 1);
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
        assert_eq!(app.markdown_presentation_revision(), 2);
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
        assert_eq!(app.markdown_presentation_revision(), 2);
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
        assert_eq!(app.focused_markdown_contribution(), Some("reference"));
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
        assert!(app.footnote_return_targets.is_empty());
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
        assert_eq!(app.focused_markdown_contribution(), Some("reference-1"));
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
        assert_eq!(app.focused_markdown_contribution(), None);
    }

    #[test]
    fn animation_deadlines_catch_up_from_monotonic_origin() {
        let started_at = Instant::now();
        let mut scroll = TranscriptScrollAnimation::new(0, 10, started_at);
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
            now + LATEST_BAR_ACTIVE_WINDOW,
        );
        assert_eq!(finished, UiInvalidation::Paint);
        assert_eq!(damage, [TemporalDamage::LatestBar]);
        assert!(app.latest_bar_next_frame_at.is_none());
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
        assert!(app.drain_elapsed_dirty_visuals().is_empty());
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

        assert!(
            app.tool_elapsed_invalidation_requests(now, now_system)
                .next()
                .is_none()
        );
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
        assert_eq!(app.transcript, before);
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
        assert!(matches!(
            app.last_transcript_damage,
            TranscriptDocumentDamage::Items(ref ids) if ids.contains(&item_id)
        ));
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
        assert!(matches!(
            app.last_transcript_damage,
            TranscriptDocumentDamage::Items(_)
        ));
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
        assert_eq!(app.transcript, before);
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
        assert!(app.transcript.source_index_is_consistent());
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
        assert_eq!(app.transcript()[0].id(), render_id);
        assert_eq!(app.transcript()[0].text(), "first second");
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
        assert!(
            app.transcript()
                .iter()
                .all(|item| item.source_view_item_id() != Some(&id))
        );
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
        assert_eq!(second.len(), 36);
        assert!(app.drain_elapsed_dirty_visuals_bounded(64).is_empty());
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
        assert!(app.has_older_history());
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

        assert!(matches!(
            &app.session_view_snapshot().transcript.items[2].kind,
            bcode_session_view_models::TranscriptViewItemKind::AssistantMessage { message }
                if message.text == "live"
        ));
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

        assert!(app.session_view_snapshot().runtime_work.is_empty());
        assert!(app.session_view_snapshot().transcript.items.is_empty());
        assert!(app.transcript().is_empty());
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
                model_scope: None,
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
        assert_eq!(
            app.session_view_snapshot()
                .runtime
                .reasoning_summary
                .as_deref(),
            Some("detailed")
        );
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
        assert_eq!(app.reasoning_effort(), Some("high"));
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
        app.last_transcript_damage = TranscriptDocumentDamage::None;
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
        assert_eq!(app.reasoning_effort(), Some("high"));
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
        app.last_transcript_damage = TranscriptDocumentDamage::None;
        let before = app.frame_observation();
        app.apply_model_status(bcode_ipc::SessionModelStatus {
            provider_plugin_id: None,
            requested_model_id: None,
            effective_model_id: None,
            model_id: None,
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
        assert_eq!(app.reasoning_effort(), Some("high"));
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

        assert_eq!(
            app.transcript().iter().cloned().collect::<Vec<_>>(),
            before_items
        );
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
        app.last_transcript_damage = TranscriptDocumentDamage::None;
        let frame = app.frame_observation();
        let expected = [
            "minimal", "low", "medium", "high", "xhigh", "max", "none", "minimal", "low", "medium",
            "high", "xhigh", "max", "none",
        ];

        for expected in expected {
            assert_eq!(app.cycle_pending_reasoning_effort(), Some(expected));
            assert_eq!(app.frame_observation(), frame);
        }
    }

    #[test]
    fn subset_effort_range_skips_unsupported_values_and_wraps() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.reasoning_effort_values = vec!["low".to_owned(), "high".to_owned()];
        app.apply_reasoning_selection(Some("low".to_owned()), None);

        assert_eq!(app.cycle_pending_reasoning_effort(), Some("high"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("low"));
        assert_eq!(app.cycle_pending_reasoning_effort(), Some("high"));
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
        assert_eq!(app.reasoning_effort_generation, 4);
    }

    #[test]
    fn session_change_explicitly_clears_pending_reasoning() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_reasoning_effort("high".to_owned());

        app.clear_pending_reasoning_effort_for_session_change();

        assert_eq!(app.pending_reasoning_effort_generation(), None);
        assert_eq!(app.reasoning_effort(), None);
    }

    #[test]
    fn canonical_status_clears_only_matching_pending_reasoning() {
        let mut app = BmuxApp::new_with_history(None, &[], &[], false);
        app.set_pending_reasoning_effort("high".to_owned());
        app.reconcile_pending_reasoning_effort("low");
        assert_eq!(app.reasoning_effort(), Some("high"));

        app.reconcile_pending_reasoning_effort("high");
        assert_eq!(app.reasoning_effort(), None);
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
        assert_eq!(app.reasoning_effort(), None);
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
        assert_eq!(
            app.session_view_snapshot()
                .runtime
                .latest_usage
                .as_ref()
                .and_then(bcode_session_models::SessionTokenUsage::metered_total_tokens),
            Some(6_176)
        );
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
        assert!(app.transcript().is_empty());
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
        assert!(app.transcript().is_empty());
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
        assert!(app.transcript().is_empty());
    }
}
