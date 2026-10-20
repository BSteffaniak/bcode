//! In-memory state for one current session runtime.

use crate::{
    ClientId, SessionLiveEvent, SessionSummary, db, normalize_working_directory,
    title_from_first_prompt,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionImportSummary,
    SessionTitleSource,
};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use tokio::sync::broadcast;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionLoadStatusKind {
    Current,
    SummaryOnly,
}

/// Maximum number of per-model reasoning selections retained for one session.
///
/// This memory is derived presentation/runtime convenience state, so it is bounded rather than
/// growing with every model a long-lived session touches.
pub const REASONING_MEMORY_CAPACITY: usize = 16;

/// Reasoning selection remembered for one provider/model pair.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RememberedReasoning {
    /// Remembered reasoning effort, when one was explicitly set.
    pub effort: Option<String>,
    /// Remembered reasoning summary, when one was explicitly set.
    pub summary: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum LiveTextStreamKey {
    Assistant {
        turn_id: String,
        segment_id: String,
    },
    Reasoning {
        turn_id: String,
        activity_id: String,
        part_id: String,
    },
}

impl LiveTextStreamKey {
    pub fn turn_id(&self) -> &str {
        match self {
            Self::Assistant { turn_id, .. } | Self::Reasoning { turn_id, .. } => turn_id,
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionState {
    pub(crate) summary: SessionSummary,
    pub(crate) working_directory: PathBuf,
    pub(crate) clients: BTreeSet<ClientId>,
    pub(crate) events: Option<Vec<SessionEvent>>,
    pub(crate) next_sequence: u64,
    pub(crate) event_count: usize,
    pub(crate) has_user_message: bool,
    pub(crate) current_provider: Option<String>,
    pub(crate) current_model: Option<String>,
    pub(crate) model_selection_source: bcode_session_models::ModelSelectionSource,
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) reasoning_summary: Option<String>,
    /// Reasoning selections remembered per provider/model pair.
    ///
    /// Derived, disposable, and bounded to [`REASONING_MEMORY_CAPACITY`] entries so attach and
    /// refresh remain bounded regardless of how many models a session touches.
    pub(crate) reasoning_by_model:
        BTreeMap<bcode_session_models::ModelScopeKey, RememberedReasoning>,
    pub(crate) reasoning_by_model_order: Vec<bcode_session_models::ModelScopeKey>,
    pub(crate) current_agent: Option<String>,
    pub(crate) latest_compaction_sequence: Option<u64>,
    pub(crate) context_epoch: u64,
    pub(crate) context_occupancy: Option<bcode_session_models::RequestContextOccupancy>,
    pub(crate) turn_evidence: BTreeMap<String, Vec<SessionEvent>>,
    pub(crate) turn_receipts: BTreeMap<(String, String), bcode_session_models::TurnReceipt>,
    pub(crate) total_metered_tokens: u64,
    pub(crate) load_status: SessionLoadStatusKind,
    pub(crate) sender: broadcast::Sender<SessionEvent>,
    pub(crate) live_events: SessionLiveEventBroker,
    pub(crate) live_text_checkpoints: BTreeMap<LiveTextStreamKey, SessionLiveEvent>,
    pub(crate) live_text_checkpoint_order: Vec<LiveTextStreamKey>,
    pub(crate) live_text_tombstones: BTreeMap<LiveTextStreamKey, (u64, u64)>,
    pub(crate) live_text_tombstone_order: Vec<LiveTextStreamKey>,
}

#[derive(Debug, Clone)]
pub struct SessionLiveEventBroker {
    pub(crate) sender: broadcast::Sender<SessionLiveEvent>,
    pub(crate) published: Arc<AtomicU64>,
    pub(crate) dropped_no_receivers: Arc<AtomicU64>,
    observations: Arc<ObservationRegistry>,
}

impl SessionLiveEventBroker {
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            published: Arc::new(AtomicU64::new(0)),
            dropped_no_receivers: Arc::new(AtomicU64::new(0)),
            observations: Arc::new(ObservationRegistry::default()),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SessionLiveEvent> {
        self.sender.subscribe()
    }

    pub(crate) fn publish(&self, event: SessionLiveEvent) -> Option<SessionLiveEvent> {
        self.observations.publish(BorrowedObservation::Live(&event));
        if self.sender.receiver_count() == 0 {
            self.dropped_no_receivers.fetch_add(1, Ordering::Relaxed);
            return None;
        }
        let _ = self.sender.send(event.clone());
        self.published.fetch_add(1, Ordering::Relaxed);
        Some(event)
    }
}

impl SessionState {
    pub(crate) fn from_catalog_summary(summary: SessionSummary) -> Self {
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
        let working_directory = normalize_working_directory(&summary.working_directory);
        Self {
            summary,
            working_directory,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: 0,
            event_count: 0,
            has_user_message: false,
            current_provider: None,
            current_model: None,
            model_selection_source: bcode_session_models::ModelSelectionSource::default(),
            reasoning_effort: None,
            reasoning_summary: None,
            reasoning_by_model: BTreeMap::new(),
            reasoning_by_model_order: Vec::new(),
            current_agent: None,
            latest_compaction_sequence: None,
            context_epoch: 0,
            context_occupancy: None,
            turn_evidence: BTreeMap::new(),
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::SummaryOnly,
            sender,
            live_events,
            live_text_checkpoints: BTreeMap::new(),
            live_text_checkpoint_order: Vec::new(),
            live_text_tombstones: BTreeMap::new(),
            live_text_tombstone_order: Vec::new(),
        }
    }

    pub(crate) fn from_db_state(
        state: db::SessionDbState,
        created_at_ms: u64,
        updated_at_ms: u64,
    ) -> Self {
        let (sender, _) = broadcast::channel(512);
        let live_events = SessionLiveEventBroker::new(512);
        let working_directory = normalize_working_directory(&state.working_directory);
        let title_source = if state.explicit_name.is_some() {
            SessionTitleSource::Explicit
        } else if state.automatic_title.is_some() {
            SessionTitleSource::AutomaticMilestone
        } else if state.derived_title.is_some() {
            SessionTitleSource::FirstUserMessage
        } else {
            SessionTitleSource::EmptyDraft
        };
        Self {
            summary: SessionSummary {
                id: state.session_id,
                name: state.title.clone(),
                explicit_name: state.explicit_name.clone(),
                derived_title: state.derived_title.clone(),
                automatic_title: state.automatic_title.clone(),
                automatic_title_source_sequence: state.automatic_title_source_sequence,
                title_source,
                client_count: 0,
                created_at_ms,
                updated_at_ms,
                working_directory: working_directory.clone(),
                import: None,
                parent_id: state.parent_id,
                execution: state.execution.map(|provenance| {
                    Box::new(bcode_session_models::ExecutionSessionSummary {
                        provenance,
                        visibility: state.visibility,
                    })
                }),
                location: None,
            },
            working_directory,
            clients: BTreeSet::new(),
            events: None,
            next_sequence: state.last_event_seq.saturating_add(1),
            event_count: usize::try_from(state.last_event_seq.saturating_add(1))
                .unwrap_or(usize::MAX),
            has_user_message: state.has_user_message,
            current_provider: state.current_provider,
            current_model: state.current_model,
            model_selection_source: state.model_selection_source,
            reasoning_effort: state.reasoning_effort,
            reasoning_summary: state.reasoning_summary,
            reasoning_by_model: state.reasoning_by_model,
            reasoning_by_model_order: state.reasoning_by_model_order,
            current_agent: state.current_agent,
            latest_compaction_sequence: state.latest_compaction_sequence,
            context_epoch: state.latest_compaction_sequence.unwrap_or_default(),
            context_occupancy: None,
            turn_evidence: BTreeMap::new(),
            turn_receipts: BTreeMap::new(),
            total_metered_tokens: 0,
            load_status: SessionLoadStatusKind::Current,
            sender,
            live_events,
            live_text_checkpoints: BTreeMap::new(),
            live_text_checkpoint_order: Vec::new(),
            live_text_tombstones: BTreeMap::new(),
            live_text_tombstone_order: Vec::new(),
        }
    }

    /// Remember a reasoning selection for one provider/model pair.
    ///
    /// Entries are evicted in least-recently-updated order once
    /// [`REASONING_MEMORY_CAPACITY`] is reached so this derived memory stays bounded.
    pub(crate) fn remember_reasoning(
        &mut self,
        scope: bcode_session_models::ModelScopeKey,
        reasoning: RememberedReasoning,
    ) {
        self.reasoning_by_model_order
            .retain(|existing| existing != &scope);
        if reasoning == RememberedReasoning::default() {
            self.reasoning_by_model.remove(&scope);
            return;
        }
        self.reasoning_by_model.insert(scope.clone(), reasoning);
        self.reasoning_by_model_order.push(scope);
        while self.reasoning_by_model_order.len() > REASONING_MEMORY_CAPACITY {
            let evicted = self.reasoning_by_model_order.remove(0);
            self.reasoning_by_model.remove(&evicted);
        }
    }

    pub(crate) fn subscribe_bounded(
        &self,
        limits: bcode_session_models::SessionObservationSourceLimits,
    ) -> BoundedSessionEventSubscription {
        self.live_events
            .observations
            .subscribe(&self.summary, limits)
    }

    pub(crate) fn summary(&self) -> SessionSummary {
        let mut summary = self.summary.clone();
        summary.name = summary.title().map(ToOwned::to_owned);
        summary
    }

    pub(crate) const fn build_next_event(
        &self,
        kind: SessionEventKind,
        timestamp_ms: u64,
    ) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: self.next_sequence,
            timestamp_ms,
            session_id: self.summary.id,
            provenance: None,
            kind,
        }
    }

    #[allow(clippy::too_many_lines)]
    pub(crate) fn apply_persisted_event(
        &mut self,
        event: SessionEvent,
        activity_timestamp_ms: u64,
    ) {
        self.summary.updated_at_ms = activity_timestamp_ms;
        self.next_sequence += 1;
        self.event_count = self.event_count.saturating_add(1);
        match &event.kind {
            SessionEventKind::ChildSessionCreated { request, .. } => {
                self.summary.parent_id = Some(request.parent_id);
            }
            SessionEventKind::ExecutionSessionCreated {
                provenance,
                visibility,
            } => {
                self.summary.execution =
                    Some(Box::new(bcode_session_models::ExecutionSessionSummary {
                        provenance: (**provenance).clone(),
                        visibility: *visibility,
                    }));
            }
            SessionEventKind::AutomaticTitleUpdated { title, .. } => {
                self.summary.automatic_title = Some(title.clone());
                self.summary.automatic_title_source_sequence = Some(event.sequence);
                if self.summary.explicit_name.is_none() {
                    self.summary.title_source = SessionTitleSource::AutomaticMilestone;
                }
            }
            SessionEventKind::SessionRenamed { name } => {
                self.summary.name.clone_from(name);
                self.summary.explicit_name.clone_from(name);
                if name.is_some() {
                    self.summary.title_source = SessionTitleSource::Explicit;
                } else if self.summary.automatic_title.is_some() {
                    self.summary.title_source = SessionTitleSource::AutomaticMilestone;
                } else if self.summary.derived_title.is_some() {
                    self.summary.title_source = SessionTitleSource::FirstUserMessage;
                } else {
                    self.summary.title_source = SessionTitleSource::EmptyDraft;
                }
            }
            SessionEventKind::SessionImported {
                source_id,
                source_display_name,
                external_session_id,
                imported_at_ms,
            } => {
                self.summary.import = Some(SessionImportSummary {
                    source_id: source_id.clone(),
                    source_display_name: source_display_name.clone(),
                    external_session_id: external_session_id.clone(),
                    imported_at_ms: *imported_at_ms,
                });
                if self.summary.explicit_name.is_none() && self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(external_session_id.clone());
                    self.summary.name.clone_from(&self.summary.derived_title);
                    self.summary.title_source = SessionTitleSource::Imported;
                }
            }
            SessionEventKind::UserMessage {
                text, admission, ..
            } => {
                if let Some(agent_id) = &admission.execution.agent_profile {
                    self.current_agent = Some(agent_id.clone());
                }
                self.has_user_message = true;
                if self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(title_from_first_prompt(text));
                    if self.summary.explicit_name.is_none() {
                        self.summary.name.clone_from(&self.summary.derived_title);
                        self.summary.title_source = SessionTitleSource::FirstUserMessage;
                    }
                }
            }
            SessionEventKind::SubmissionAccepted { submission } => {
                self.has_user_message = true;
                if self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(title_from_first_prompt(&submission.text));
                    if self.summary.explicit_name.is_none() {
                        self.summary.name.clone_from(&self.summary.derived_title);
                        self.summary.title_source = SessionTitleSource::FirstUserMessage;
                    }
                }
            }
            SessionEventKind::WorkingDirectoryChanged {
                new_working_directory,
                ..
            } => {
                self.working_directory = normalize_working_directory(new_working_directory);
                self.summary
                    .working_directory
                    .clone_from(&self.working_directory);
            }
            SessionEventKind::ModelChanged {
                provider,
                model,
                selection_source,
            } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
                self.model_selection_source = *selection_source;
                self.context_epoch = event.sequence;
                self.context_occupancy = None;
                // Restore the reasoning selection previously remembered for this exact
                // provider/model pair. When the pair has no remembered selection, clear the
                // active values so the incoming model resolves its own default rather than
                // inheriting the outgoing model's effort.
                let scope = bcode_session_models::ModelScopeKey::new(provider, model);
                let remembered = self.reasoning_by_model.get(&scope).cloned();
                let remembered = remembered.unwrap_or_default();
                self.reasoning_effort = remembered.effort;
                self.reasoning_summary = remembered.summary;
            }
            SessionEventKind::ReasoningChanged {
                effort,
                summary,
                model_scope,
            } => {
                self.reasoning_effort.clone_from(effort);
                self.reasoning_summary.clone_from(summary);
                // Remember the selection against its declared scope, falling back to the
                // currently active model for history persisted before scoping existed.
                let scope = model_scope.clone().or_else(|| {
                    self.current_provider
                        .as_ref()
                        .zip(self.current_model.as_ref())
                        .map(|(provider, model)| {
                            bcode_session_models::ModelScopeKey::new(provider, model)
                        })
                });
                if let Some(scope) = scope {
                    self.remember_reasoning(
                        scope,
                        RememberedReasoning {
                            effort: effort.clone(),
                            summary: summary.clone(),
                        },
                    );
                }
            }
            SessionEventKind::AgentChanged { agent_id } => {
                self.current_agent = Some(agent_id.clone());
            }
            SessionEventKind::ContextCompacted {
                compacted_through_sequence,
                ..
            }
            | SessionEventKind::ProviderContextCompacted {
                compacted_through_sequence,
                ..
            } => {
                self.latest_compaction_sequence = Some(*compacted_through_sequence);
                self.context_epoch = event.sequence;
                self.context_occupancy = None;
            }
            SessionEventKind::RequestContextObserved { observation } => {
                self.context_occupancy = bcode_session_models::RequestContextOccupancy::reconcile(
                    self.context_occupancy.as_ref(),
                    self.context_epoch,
                    event.sequence,
                    observation.clone(),
                );
            }
            SessionEventKind::ModelUsage { usage, .. } => {
                if let Some(total) = usage.metered_total_tokens() {
                    self.total_metered_tokens =
                        self.total_metered_tokens.saturating_add(u64::from(total));
                }
            }
            _ => {}
        }
        if self.events.is_some() {
            match &event.kind {
                SessionEventKind::TraceEvent { trace }
                    if trace.turn_id.is_some()
                        && matches!(&trace.payload, bcode_session_models::SessionTracePayload::ProviderEvent { event_type, .. } if event_type == bcode_session_models::PROVIDER_RETRY_CHECKPOINT_EVENT) =>
                {
                    let evidence = self
                        .turn_evidence
                        .entry(trace.turn_id.clone().expect("checked turn"))
                        .or_default();
                    evidence
                        .retain(|item| !matches!(item.kind, SessionEventKind::TraceEvent { .. }));
                    evidence.push(event.clone());
                }
                SessionEventKind::ModelTurnFinished { turn_id, .. } => {
                    let evidence = self.turn_evidence.entry(turn_id.clone()).or_default();
                    if !evidence
                        .iter()
                        .any(|item| matches!(item.kind, SessionEventKind::ModelTurnFinished { .. }))
                    {
                        evidence.push(event.clone());
                    }
                }
                SessionEventKind::AssistantResponseSegment {
                    turn_id,
                    segment_order,
                    ..
                }
                | SessionEventKind::PositionedAssistantResponseSegment {
                    turn_id,
                    segment_order,
                    ..
                } => {
                    let evidence = self.turn_evidence.entry(turn_id.clone()).or_default();
                    if !evidence
                        .iter()
                        .any(|item| matches!(item.kind, SessionEventKind::ModelTurnFinished { .. }))
                    {
                        let prior_order = evidence.iter().find_map(|item| match &item.kind {
                            SessionEventKind::AssistantResponseSegment {
                                segment_order, ..
                            }
                            | SessionEventKind::PositionedAssistantResponseSegment {
                                segment_order,
                                ..
                            } => Some(*segment_order),
                            _ => None,
                        });
                        if prior_order.is_none_or(|order| order <= *segment_order) {
                            evidence.retain(|item| {
                                matches!(item.kind, SessionEventKind::TraceEvent { .. })
                            });
                            evidence.push(event.clone());
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(events) = &mut self.events {
            events.push(event.clone());
        }
        self.live_events
            .observations
            .publish(BorrowedObservation::Durable(&event));
        let _ = self.sender.send(event);
    }
}

/// Loss observed when this bounded source stopped delivering updates.
///
/// Counts cover the discarded queue and the triggering publication only. They do
/// not count future publications after termination. `None` means unknown, never zero.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BoundedSessionEventLoss {
    /// Known number of committed durable events discarded at termination.
    pub durable_events: Option<u64>,
    /// Known number of live-only events discarded at termination.
    pub live_events: Option<u64>,
    /// Lowest discarded durable sequence, if known; not a contiguous-range claim.
    pub first_durable_sequence: Option<u64>,
    /// Highest discarded durable sequence, if known; live events have no sequence.
    pub last_durable_sequence: Option<u64>,
}

impl BoundedSessionEventLoss {
    const fn empty() -> Self {
        Self {
            durable_events: Some(0),
            live_events: Some(0),
            first_durable_sequence: None,
            last_durable_sequence: None,
        }
    }

    fn record(&mut self, event: BorrowedObservation<'_>) {
        match event {
            BorrowedObservation::Durable(event) => {
                self.durable_events = self.durable_events.and_then(|count| count.checked_add(1));
                self.first_durable_sequence = Some(
                    self.first_durable_sequence
                        .map_or(event.sequence, |sequence| sequence.min(event.sequence)),
                );
                self.last_durable_sequence = Some(
                    self.last_durable_sequence
                        .map_or(event.sequence, |sequence| sequence.max(event.sequence)),
                );
            }
            BorrowedObservation::Live(_) => {
                self.live_events = self.live_events.and_then(|count| count.checked_add(1));
            }
        }
    }
}

/// Terminal status for an observation-only source subscriber.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BoundedSessionEventError {
    /// A queued update could not be retained; resubscribe from a bounded snapshot.
    Gap(BoundedSessionEventLoss),
    /// A structural reservation or configured count could not be admitted.
    LimitReached,
    /// The source or subscription was closed.
    Closed,
}

/// Event payload owned under a source admission lease.
#[derive(Debug)]
pub enum BoundedSessionEvent {
    /// A committed durable event.
    Durable(Box<SessionEvent>),
    /// A live-only event.
    Live(Box<SessionLiveEvent>),
}

/// An update retaining its admission reservation until projector admission/drop.
///
/// The payload is borrowed, not extractable: moving it out would release the
/// reservation before the consumer has admitted its own retained representation.
#[derive(Debug)]
pub struct BoundedSessionEventUpdate {
    event: BoundedSessionEvent,
    lease: SessionObservationAdmissionLease,
}

impl BoundedSessionEvent {
    fn borrowed(&self) -> BorrowedObservation<'_> {
        match self {
            Self::Durable(event) => BorrowedObservation::Durable(event),
            Self::Live(event) => BorrowedObservation::Live(event),
        }
    }
}

impl BoundedSessionEventUpdate {
    /// Borrow the admitted event for a bounded projector.
    #[must_use]
    pub const fn event(&self) -> &BoundedSessionEvent {
        &self.event
    }

    /// Structural reservation held until this update and its payload are dropped.
    #[must_use]
    pub const fn retained_usage(&self) -> bcode_session_models::ObservationRetentionUsage {
        self.lease.usage
    }
}

#[derive(Debug)]
struct SessionObservationAdmissionLease {
    slot: Arc<ObservationSlot>,
    usage: bcode_session_models::ObservationRetentionUsage,
    counts_in_flight: bool,
}

impl Drop for SessionObservationAdmissionLease {
    fn drop(&mut self) {
        let mut state = self.slot.lock();
        if state.usage.try_release(self.usage).is_err() {
            state.terminal = Some(BoundedSessionEventError::LimitReached);
        }
        if self.counts_in_flight {
            state.in_flight = state.in_flight.checked_sub(1).unwrap_or_else(|| {
                state.terminate(BoundedSessionEventError::LimitReached);
                0
            });
        }
        drop(state);
        self.slot.notify.notify_one();
    }
}

#[derive(Debug)]
struct PendingObservation {
    event: BoundedSessionEvent,
    usage: bcode_session_models::ObservationRetentionUsage,
}

#[derive(Debug, Default)]
struct ObservationQueue {
    pending: std::collections::VecDeque<PendingObservation>,
    usage: bcode_session_models::ObservationRetentionUsage,
    in_flight: usize,
    terminal: Option<BoundedSessionEventError>,
}

impl ObservationQueue {
    fn terminate(&mut self, error: BoundedSessionEventError) {
        self.terminal.get_or_insert(error);
        while let Some(pending) = self.pending.pop_front() {
            // The queued payload is destroyed while its reservation is still
            // held. In-flight payload reservations remain until their drops.
            let PendingObservation { event, usage } = pending;
            drop(event);
            if self.usage.try_release(usage).is_err() {
                self.terminal = Some(BoundedSessionEventError::LimitReached);
            }
        }
    }
}

#[derive(Debug)]
struct ObservationSlot {
    limits: bcode_session_models::SessionObservationSourceLimits,
    state: std::sync::Mutex<ObservationQueue>,
    notify: tokio::sync::Notify,
}

impl ObservationSlot {
    fn lock(&self) -> std::sync::MutexGuard<'_, ObservationQueue> {
        match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.terminate(BoundedSessionEventError::LimitReached);
                state
            }
        }
    }

    fn is_terminal(&self) -> bool {
        self.lock().terminal.is_some()
    }

    fn retention_limits(
        &self,
    ) -> Result<
        bcode_session_models::ObservationRetentionLimits,
        bcode_session_models::ObservationRetentionExceeded,
    > {
        let mut limits = self.limits.retention_limits()?;
        // The retained summary is independent of queued/in-flight event counts.
        limits.copies = limits
            .copies
            .checked_add(1)
            .ok_or(bcode_session_models::ObservationRetentionExceeded)?;
        Ok(limits)
    }

    fn inspect(
        &self,
        event: BorrowedObservation<'_>,
    ) -> Result<
        bcode_session_models::ObservationRetentionUsage,
        bcode_session_models::ObservationRetentionExceeded,
    > {
        let inspected = match event {
            BorrowedObservation::Durable(event) => {
                bcode_session_models::inspect_durable(event, &self.limits)
            }
            BorrowedObservation::Live(event) => {
                bcode_session_models::inspect_live(event, &self.limits)
            }
        };
        inspected.and_then(|mut usage| {
            let limits = self.retention_limits()?;
            // One boxed payload owns one collection slot and a wrapper node.
            // Admit that storage and its containment depth BEFORE Box::new.
            usage.depth = usage
                .depth
                .checked_add(1)
                .ok_or(bcode_session_models::ObservationRetentionExceeded)?;
            usage.try_add(
                bcode_session_models::ObservationRetentionUsage {
                    collection_entries: 1,
                    value_nodes: 1,
                    ..bcode_session_models::ObservationRetentionUsage::default()
                },
                limits,
            )?;
            Ok(usage)
        })
    }

    fn publish(&self, event: BorrowedObservation<'_>) {
        let mut state = self.lock();
        if state.terminal.is_some() {
            return;
        }
        if state.pending.len() >= self.limits.max_queued_events {
            let mut loss = BoundedSessionEventLoss::empty();
            for pending in &state.pending {
                loss.record(pending.event.borrowed());
            }
            loss.record(event);
            state.terminate(BoundedSessionEventError::Gap(loss));
            drop(state);
            self.notify.notify_one();
            return;
        }
        let inspected = self.inspect(event);
        let Ok(usage) = inspected else {
            state.terminate(BoundedSessionEventError::LimitReached);
            drop(state);
            self.notify.notify_one();
            return;
        };
        if let Ok(limits) = self.retention_limits() {
            if state.usage.try_add(usage, limits).is_ok() {
                // No source event clone is performed before BOTH structural
                // inspection and aggregate reservation have succeeded.
                let event = match event {
                    BorrowedObservation::Durable(event) => {
                        BoundedSessionEvent::Durable(Box::new(event.clone()))
                    }
                    BorrowedObservation::Live(event) => {
                        BoundedSessionEvent::Live(Box::new(event.clone()))
                    }
                };
                state.pending.push_back(PendingObservation { event, usage });
            } else {
                state.terminate(BoundedSessionEventError::LimitReached);
            }
        } else {
            state.terminate(BoundedSessionEventError::LimitReached);
        }
        drop(state);
        self.notify.notify_one();
    }
}

#[derive(Clone, Copy)]
enum BorrowedObservation<'a> {
    Durable(&'a SessionEvent),
    Live(&'a SessionLiveEvent),
}

/// A bounded observation subscription independent of ordinary broadcasts.
///
/// Dequeue transfers a queued reservation into an in-flight lease; it never
/// releases structural usage. Dropping the subscription drains queued payloads
/// and closes this subscriber only. Already returned updates retain their leases
/// until dropped, even after the subscription has been dropped.
#[derive(Debug)]
pub struct BoundedSessionEventSubscription {
    // Payload precedes its lease so normal field destruction cannot release early.
    session: Option<SessionSummary>,
    summary_lease: Option<SessionObservationAdmissionLease>,
    slot: Arc<ObservationSlot>,
}

impl BoundedSessionEventSubscription {
    /// Borrow the summary while its source reservation remains held.
    ///
    /// # Errors
    /// Returns the terminal source status when summary admission failed or it was closed.
    pub fn session(&self) -> Result<&SessionSummary, BoundedSessionEventError> {
        self.session.as_ref().ok_or_else(|| {
            self.slot
                .lock()
                .terminal
                .unwrap_or(BoundedSessionEventError::Closed)
        })
    }

    /// Current aggregate source retention, including summary and returned updates.
    #[must_use]
    pub fn retained_usage(&self) -> bcode_session_models::ObservationRetentionUsage {
        self.slot.lock().usage
    }

    /// Release the summary and queued payloads, without releasing returned updates.
    pub fn close(&mut self) {
        self.slot.lock().terminate(BoundedSessionEventError::Closed);
        self.session = None;
        self.summary_lease = None;
        self.slot.notify.notify_one();
    }

    /// Admit one bounded decoded history event before creating its retained source copy.
    ///
    /// The caller must bound decoding separately. History, summary, queued live/durable
    /// updates and returned event leases all share this subscription's structural ledger.
    ///
    /// # Errors
    /// Returns the stable terminal status or `LimitReached` on identity, aggregate
    /// or in-flight denial.
    pub fn admit_history_event(
        &self,
        event: &SessionEvent,
    ) -> Result<BoundedSessionEventUpdate, BoundedSessionEventError> {
        let mut state = self.slot.lock();
        if let Some(error) = state.terminal {
            return Err(error);
        }
        if self
            .session
            .as_ref()
            .is_none_or(|summary| summary.id != event.session_id)
        {
            state.terminate(BoundedSessionEventError::LimitReached);
            drop(state);
            self.slot.notify.notify_one();
            return Err(BoundedSessionEventError::LimitReached);
        }
        let admitted = self
            .slot
            .inspect(BorrowedObservation::Durable(event))
            .and_then(|usage| {
                if state.in_flight >= self.slot.limits.max_in_flight_events {
                    return Err(bcode_session_models::ObservationRetentionExceeded);
                }
                state.usage.try_add(usage, self.slot.retention_limits()?)?;
                Ok(usage)
            });
        let Ok(usage) = admitted else {
            state.terminate(BoundedSessionEventError::LimitReached);
            drop(state);
            self.slot.notify.notify_one();
            return Err(BoundedSessionEventError::LimitReached);
        };
        state.in_flight += 1;
        let update = BoundedSessionEventUpdate {
            event: BoundedSessionEvent::Durable(Box::new(event.clone())),
            lease: SessionObservationAdmissionLease {
                slot: Arc::clone(&self.slot),
                usage,
                counts_in_flight: true,
            },
        };
        drop(state);
        Ok(update)
    }

    /// Receive the next admission-leased durable or live update.
    ///
    /// # Errors
    /// Reports a stable terminal gap, structural limit, or closed source. When
    /// the in-flight bound is occupied, waits for a previous update to be dropped.
    pub async fn recv(&mut self) -> Result<BoundedSessionEventUpdate, BoundedSessionEventError> {
        loop {
            let notified = self.slot.notify.notified();
            {
                let mut state = self.slot.lock();
                if let Some(error) = state.terminal {
                    return Err(error);
                }
                if state.in_flight < self.slot.limits.max_in_flight_events
                    && let Some(pending) = state.pending.pop_front()
                {
                    state.in_flight += 1;
                    return Ok(BoundedSessionEventUpdate {
                        event: pending.event,
                        lease: SessionObservationAdmissionLease {
                            slot: Arc::clone(&self.slot),
                            usage: pending.usage,
                            counts_in_flight: true,
                        },
                    });
                }
            }
            notified.await;
        }
    }
}

impl Drop for BoundedSessionEventSubscription {
    fn drop(&mut self) {
        self.close();
    }
}

#[derive(Debug, Default)]
struct ObservationRegistry {
    slots: std::sync::Mutex<Vec<std::sync::Weak<ObservationSlot>>>,
}

impl ObservationRegistry {
    fn subscribe(
        &self,
        session: &SessionSummary,
        limits: bcode_session_models::SessionObservationSourceLimits,
    ) -> BoundedSessionEventSubscription {
        let terminal = if limits.retention_limits().is_err()
            || limits.max_queued_events == 0
            || limits.max_in_flight_events == 0
        {
            Some(BoundedSessionEventError::LimitReached)
        } else {
            None
        };
        let slot = Arc::new(ObservationSlot {
            limits,
            state: std::sync::Mutex::new(ObservationQueue {
                terminal,
                ..ObservationQueue::default()
            }),
            notify: tokio::sync::Notify::new(),
        });
        let admitted_summary = (|| {
            let structural_limits = slot.retention_limits()?;
            let mut usage = bcode_session_models::retention_events::inspect_session_summary(
                session,
                &structural_limits,
            )?;
            // Normalizing the display name creates one additional scalar clone while
            // the raw name is still owned. Reserve it before either clone occurs.
            usage.try_add(
                bcode_session_models::ObservationRetentionUsage {
                    scalar_capacity_bytes: session.title().map_or(0, str::len),
                    copies: 1,
                    ..bcode_session_models::ObservationRetentionUsage::default()
                },
                structural_limits,
            )?;
            let mut state = slot.lock();
            if state.terminal.is_some() {
                return Err(bcode_session_models::ObservationRetentionExceeded);
            }
            state.usage.try_add(usage, structural_limits)?;
            let mut summary = session.clone();
            summary.name = session.title().map(ToOwned::to_owned);
            drop(state);
            Ok((
                summary,
                SessionObservationAdmissionLease {
                    slot: Arc::clone(&slot),
                    usage,
                    counts_in_flight: false,
                },
            ))
        })();
        let (session, summary_lease) = if let Ok((summary, lease)) = admitted_summary {
            (Some(summary), Some(lease))
        } else {
            slot.lock()
                .terminate(BoundedSessionEventError::LimitReached);
            (None, None)
        };
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.retain(|slot| slot.strong_count() > 0);
        if !slot.is_terminal() {
            slots.push(Arc::downgrade(&slot));
        }
        BoundedSessionEventSubscription {
            session,
            summary_lease,
            slot,
        }
    }

    fn publish(&self, event: BorrowedObservation<'_>) {
        // Only bounded observer work takes this lock. Neither reservations nor
        // terminal statuses flow back into canonical persistence.
        let mut slots = self
            .slots
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        slots.retain(|slot| {
            let Some(slot) = slot.upgrade() else {
                return false;
            };
            slot.publish(event);
            !slot.is_terminal()
        });
    }
}

impl Drop for ObservationRegistry {
    fn drop(&mut self) {
        let slots = self
            .slots
            .get_mut()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for slot in slots.iter().filter_map(std::sync::Weak::upgrade) {
            slot.lock().terminate(BoundedSessionEventError::Closed);
            slot.notify.notify_one();
        }
    }
}

#[cfg(test)]
mod bounded_observation_tests {
    use super::*;
    use bcode_session_models::SessionObservationSourceLimits;

    fn limits() -> SessionObservationSourceLimits {
        SessionObservationSourceLimits {
            max_scalar_capacity_bytes: 1024,
            max_collection_entries: 1024,
            max_value_nodes: 1024,
            max_depth: 16,
            max_queued_events: 1,
            max_in_flight_events: 1,
        }
    }

    fn live(text: String) -> SessionLiveEvent {
        SessionLiveEvent {
            session_id: crate::SessionId::new(),
            kind: crate::SessionLiveEventKind::AssistantReasoningDelta {
                turn_id: String::new(),
                text,
            },
        }
    }

    fn slot(limits: SessionObservationSourceLimits) -> Arc<ObservationSlot> {
        Arc::new(ObservationSlot {
            limits,
            state: std::sync::Mutex::new(ObservationQueue::default()),
            notify: tokio::sync::Notify::new(),
        })
    }

    #[tokio::test]
    async fn manager_subscription_delivers_durable_and_live_under_lease() {
        let manager = crate::SessionManager::default();
        let session = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let mut subscription = manager
            .subscribe_session_events_bounded(session.id, limits())
            .await
            .unwrap();
        let mut legacy = manager.subscribe_session_events(session.id).await.unwrap();
        manager
            .append_assistant_message(session.id, "durable".to_owned())
            .await
            .unwrap();
        let first = subscription.recv().await.unwrap();
        assert!(matches!(first.event(), BoundedSessionEvent::Durable(_)));
        assert_eq!(subscription.slot.lock().in_flight, 1);
        assert_eq!(subscription.slot.lock().usage.copies, 2);
        assert!(legacy.events.recv().await.is_ok());
        manager
            .publish_live_event(
                session.id,
                crate::SessionLiveEventKind::AssistantReasoningDelta {
                    turn_id: "turn".to_owned(),
                    text: "live".to_owned(),
                },
            )
            .await
            .unwrap();
        assert_eq!(subscription.slot.lock().pending.len(), 1);
        assert_eq!(subscription.slot.lock().usage.copies, 3);
        drop(first);
        let second = subscription.recv().await.unwrap();
        assert!(matches!(second.event(), BoundedSessionEvent::Live(_)));
        drop(subscription);
        assert_eq!(second.lease.slot.lock().usage.copies, 1);
        let slot = Arc::clone(&second.lease.slot);
        drop(second);
        assert_eq!(slot.lock().usage.copies, 0);
    }

    fn durable(session_id: crate::SessionId, sequence: u64) -> SessionEvent {
        SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence,
            timestamp_ms: 0,
            session_id,
            provenance: None,
            kind: SessionEventKind::AssistantMessage {
                text: "durable".to_owned(),
            },
        }
    }

    #[tokio::test]
    async fn queue_loss_retains_durable_live_counts_and_only_durable_sequences() {
        for queued_durable in [false, true] {
            let slot = slot(limits());
            let event = durable(crate::SessionId::new(), 17);
            let live = live("live".to_owned());
            if queued_durable {
                slot.publish(BorrowedObservation::Durable(&event));
                slot.publish(BorrowedObservation::Live(&live));
            } else {
                slot.publish(BorrowedObservation::Live(&live));
                slot.publish(BorrowedObservation::Durable(&event));
            }
            let expected = BoundedSessionEventError::Gap(BoundedSessionEventLoss {
                durable_events: Some(1),
                live_events: Some(1),
                first_durable_sequence: Some(17),
                last_durable_sequence: Some(17),
            });
            let mut subscription = BoundedSessionEventSubscription {
                session: None,
                summary_lease: None,
                slot: Arc::clone(&slot),
            };
            assert_eq!(subscription.recv().await.unwrap_err(), expected);
            slot.publish(BorrowedObservation::Durable(&durable(
                event.session_id,
                100,
            )));
            assert_eq!(subscription.recv().await.unwrap_err(), expected);
            assert_eq!(
                slot.lock().usage,
                bcode_session_models::ObservationRetentionUsage::default()
            );
        }
        let slot = slot(limits());
        let id = crate::SessionId::new();
        slot.publish(BorrowedObservation::Durable(&durable(id, 21)));
        slot.publish(BorrowedObservation::Durable(&durable(id, 25)));
        assert_eq!(
            slot.lock().terminal,
            Some(BoundedSessionEventError::Gap(BoundedSessionEventLoss {
                durable_events: Some(2),
                live_events: Some(0),
                first_durable_sequence: Some(21),
                last_durable_sequence: Some(25),
            }))
        );
    }

    #[tokio::test]
    async fn admitted_summary_is_normalized_and_source_close_keeps_summary_lease_until_drop() {
        let manager = crate::SessionManager::default();
        let mut summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        summary.name = Some("legacy".to_owned());
        summary.explicit_name = Some("normalized".to_owned());
        let registry = ObservationRegistry::default();
        let mut subscription = registry.subscribe(&summary, limits());
        assert_eq!(
            subscription.session().unwrap().name.as_deref(),
            Some("normalized")
        );
        let usage = subscription.retained_usage();
        let slot = Arc::clone(&subscription.slot);
        registry.publish(BorrowedObservation::Live(&live("queued".to_owned())));
        let queued_depth = subscription.retained_usage().depth;
        drop(registry);
        assert_eq!(
            subscription.recv().await.unwrap_err(),
            BoundedSessionEventError::Closed
        );
        // Depth remains a conservative high-water mark while summary nodes are retained.
        assert_eq!(
            subscription.retained_usage(),
            bcode_session_models::ObservationRetentionUsage {
                depth: usage.depth.max(queued_depth),
                ..usage
            }
        );
        drop(subscription);
        assert_eq!(
            slot.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
        drop(slot);
        drop(manager);
    }

    #[tokio::test]
    async fn summary_denial_never_retains_a_payload_or_registers_an_observer() {
        let manager = crate::SessionManager::default();
        let mut summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let mut title = String::with_capacity(4096);
        title.push('x');
        summary.explicit_name = Some(title);
        let registry = ObservationRegistry::default();
        let mut subscription = registry.subscribe(&summary, limits());
        assert_eq!(
            subscription.session().unwrap_err(),
            BoundedSessionEventError::LimitReached
        );
        assert_eq!(
            subscription.recv().await.unwrap_err(),
            BoundedSessionEventError::LimitReached
        );
        assert!(subscription.summary_lease.is_none());
        assert!(registry.slots.lock().unwrap().is_empty());
        assert_eq!(
            subscription.retained_usage(),
            bcode_session_models::ObservationRetentionUsage::default()
        );
        drop(subscription);
        drop(registry);
        drop(manager);
    }

    #[tokio::test]
    async fn summary_history_and_live_share_admission_and_close_preserves_returned_leases() {
        let manager = crate::SessionManager::default();
        let summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let registry = ObservationRegistry::default();
        let mut subscription = registry.subscribe(&summary, limits());
        assert_eq!(subscription.session().unwrap().id, summary.id);
        let summary_usage = subscription.retained_usage();
        assert_eq!(summary_usage.copies, 1);
        let event = durable(summary.id, 9);
        let history = subscription.admit_history_event(&event).unwrap();
        assert_eq!(subscription.retained_usage().copies, 2);
        registry.publish(BorrowedObservation::Live(&live("queued".to_owned())));
        assert_eq!(subscription.retained_usage().copies, 3);
        subscription.close();
        assert_eq!(subscription.retained_usage(), history.retained_usage());
        assert_eq!(
            subscription.session().unwrap_err(),
            BoundedSessionEventError::Closed
        );
        assert_eq!(
            subscription.recv().await.unwrap_err(),
            BoundedSessionEventError::Closed
        );
        drop(history);
        assert_eq!(
            subscription.retained_usage(),
            bcode_session_models::ObservationRetentionUsage::default()
        );
        subscription.close();
        drop(subscription);
        drop(registry);
        drop(manager);
    }

    #[tokio::test]
    async fn aggregate_summary_reservation_blocks_history_before_clone() {
        let manager = crate::SessionManager::default();
        let summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let registry = ObservationRegistry::default();
        let probe = registry.subscribe(&summary, limits());
        let summary_usage = probe.retained_usage();
        drop(probe);
        let mut policy = limits();
        policy.max_value_nodes = summary_usage.value_nodes;
        let mut subscription = registry.subscribe(&summary, policy);
        assert!(subscription.session().is_ok());
        assert_eq!(
            subscription
                .admit_history_event(&durable(summary.id, 3))
                .unwrap_err(),
            BoundedSessionEventError::LimitReached
        );
        assert_eq!(subscription.retained_usage(), summary_usage);
        subscription.close();
        assert_eq!(
            subscription.retained_usage(),
            bcode_session_models::ObservationRetentionUsage::default()
        );
        drop(subscription);
        drop(registry);
        drop(manager);
    }

    #[tokio::test]
    async fn cancelled_receive_does_not_release_summary_or_inflight_usage() {
        let manager = crate::SessionManager::default();
        let summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let registry = ObservationRegistry::default();
        let mut subscription = registry.subscribe(&summary, limits());
        let history = subscription
            .admit_history_event(&durable(summary.id, 3))
            .unwrap();
        let usage = subscription.retained_usage();
        {
            let receive = subscription.recv();
            tokio::pin!(receive);
            tokio::select! {
                biased;
                result = &mut receive => panic!("no queued event should be ready: {result:?}"),
                () = std::future::ready(()) => {}
            }
        }
        assert_eq!(subscription.retained_usage(), usage);
        let slot = Arc::clone(&subscription.slot);
        drop(subscription);
        assert_eq!(slot.lock().usage, history.retained_usage());
        drop(history);
        assert_eq!(
            slot.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
        drop(slot);
        drop(registry);
        drop(manager);
    }

    #[test]
    fn loss_count_overflow_is_unknown_without_erasing_known_provenance() {
        let mut loss = BoundedSessionEventLoss {
            durable_events: Some(u64::MAX),
            live_events: Some(0),
            first_durable_sequence: None,
            last_durable_sequence: None,
        };
        loss.record(BorrowedObservation::Durable(&durable(
            crate::SessionId::new(),
            27,
        )));
        assert_eq!(loss.durable_events, None);
        assert_eq!(loss.live_events, Some(0));
        assert_eq!(loss.first_durable_sequence, Some(27));
        assert_eq!(loss.last_durable_sequence, Some(27));
    }

    #[tokio::test]
    async fn history_inflight_denial_keeps_existing_lease_and_releases_on_drop() {
        let manager = crate::SessionManager::default();
        let summary = manager
            .create_session(None, std::env::current_dir().unwrap())
            .await
            .unwrap();
        let registry = ObservationRegistry::default();
        let mut subscription = registry.subscribe(&summary, limits());
        let history = subscription
            .admit_history_event(&durable(summary.id, 3))
            .unwrap();
        let usage = subscription.retained_usage();
        assert_eq!(
            subscription
                .admit_history_event(&durable(summary.id, 4))
                .unwrap_err(),
            BoundedSessionEventError::LimitReached
        );
        assert_eq!(subscription.retained_usage(), usage);
        assert_eq!(
            subscription.recv().await.unwrap_err(),
            BoundedSessionEventError::LimitReached
        );
        subscription.close();
        let slot = Arc::clone(&subscription.slot);
        assert_eq!(slot.lock().usage, history.retained_usage());
        drop(history);
        assert_eq!(
            slot.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
        drop(subscription);
        drop(registry);
        drop(manager);
    }

    #[test]
    fn boxed_payload_slot_is_admitted_before_allocation_and_retained() {
        let mut policy = limits();
        policy.max_collection_entries = 0;
        let rejected = slot(policy);
        rejected.publish(BorrowedObservation::Live(&live(String::new())));
        assert_eq!(
            rejected.lock().terminal,
            Some(BoundedSessionEventError::LimitReached)
        );
        assert!(rejected.lock().pending.is_empty());
        assert_eq!(rejected.lock().usage.copies, 0);

        policy.max_collection_entries = 1;
        let admitted = slot(policy);
        admitted.publish(BorrowedObservation::Live(&live(String::new())));
        let state = admitted.lock();
        assert!(state.terminal.is_none());
        assert_eq!(state.usage.collection_entries, 1);
        assert_eq!(state.usage.copies, 1);
        assert_eq!(state.pending.len(), 1);
    }

    #[test]
    fn oversized_borrowed_event_never_enters_queue() {
        let slot = slot(limits());
        let mut text = String::with_capacity(4096);
        text.push('x');
        slot.publish(BorrowedObservation::Live(&live(text)));
        let state = slot.lock();
        assert_eq!(state.terminal, Some(BoundedSessionEventError::LimitReached));
        assert!(state.pending.is_empty());
        assert_eq!(
            state.usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
    }

    #[test]
    fn slow_subscriber_does_not_terminate_another_or_legacy_broadcast() {
        let broker = SessionLiveEventBroker::new(4);
        let slow = slot(limits());
        let fast = slot(limits());
        broker
            .observations
            .slots
            .lock()
            .unwrap()
            .extend([Arc::downgrade(&slow), Arc::downgrade(&fast)]);
        let mut legacy = broker.subscribe();
        let event = live("first".to_owned());
        assert!(broker.publish(event.clone()).is_some());
        {
            let mut state = fast.lock();
            let pending = state.pending.pop_front().unwrap();
            state.usage.try_release(pending.usage).unwrap();
        }
        assert!(broker.publish(event).is_some());
        assert_eq!(
            slow.lock().terminal,
            Some(BoundedSessionEventError::Gap(BoundedSessionEventLoss {
                durable_events: Some(0),
                live_events: Some(2),
                first_durable_sequence: None,
                last_durable_sequence: None,
            }))
        );
        assert_eq!(
            slow.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
        assert!(fast.lock().terminal.is_none());
        assert_eq!(fast.lock().pending.len(), 1);
        assert!(legacy.try_recv().is_ok());
        assert!(legacy.try_recv().is_ok());
    }

    #[test]
    fn live_observation_exists_without_legacy_receivers() {
        let broker = SessionLiveEventBroker::new(1);
        let slot = slot(limits());
        broker
            .observations
            .slots
            .lock()
            .unwrap()
            .push(Arc::downgrade(&slot));
        assert!(broker.publish(live("text".to_owned())).is_none());
        assert_eq!(slot.lock().pending.len(), 1);
        drop(broker);
        assert_eq!(slot.lock().terminal, Some(BoundedSessionEventError::Closed));
        assert_eq!(
            slot.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
    }

    #[test]
    fn dequeued_lease_remains_reserved_until_payload_drop() {
        let slot = slot(limits());
        slot.publish(BorrowedObservation::Live(&live("text".to_owned())));
        let pending = {
            let mut state = slot.lock();
            state.in_flight += 1;
            state.pending.pop_front().unwrap()
        };
        let usage = pending.usage;
        let update = BoundedSessionEventUpdate {
            event: pending.event,
            lease: SessionObservationAdmissionLease {
                slot: Arc::clone(&slot),
                usage,
                counts_in_flight: true,
            },
        };
        assert_eq!(slot.lock().usage, usage);
        slot.lock().terminate(BoundedSessionEventError::Closed);
        assert_eq!(slot.lock().usage, usage);
        drop(update);
        assert_eq!(
            slot.lock().usage,
            bcode_session_models::ObservationRetentionUsage::default()
        );
        assert_eq!(slot.lock().in_flight, 0);
    }
}
