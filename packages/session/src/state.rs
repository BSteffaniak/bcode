//! In-memory state for one current session runtime.

use crate::{
    ClientId, SessionLiveEvent, SessionSummary, db, normalize_working_directory,
    title_from_first_prompt,
};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionEvent, SessionEventKind, SessionForkSummary,
    SessionImportSummary, SessionTitleSource,
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

#[derive(Debug)]
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
    pub(crate) reasoning_effort: Option<String>,
    pub(crate) reasoning_summary: Option<String>,
    pub(crate) current_agent: Option<String>,
    pub(crate) latest_compaction_sequence: Option<u64>,
    pub(crate) context_epoch: u64,
    pub(crate) context_occupancy: Option<bcode_session_models::RequestContextOccupancy>,
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
}

impl SessionLiveEventBroker {
    pub(crate) fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender,
            published: Arc::new(AtomicU64::new(0)),
            dropped_no_receivers: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<SessionLiveEvent> {
        self.sender.subscribe()
    }

    pub(crate) fn publish(&self, event: SessionLiveEvent) -> Option<SessionLiveEvent> {
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
            reasoning_effort: None,
            reasoning_summary: None,
            current_agent: None,
            latest_compaction_sequence: None,
            context_epoch: 0,
            context_occupancy: None,
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
        let title_source = if state.title.is_some() {
            SessionTitleSource::Explicit
        } else {
            SessionTitleSource::EmptyDraft
        };
        Self {
            summary: SessionSummary {
                id: state.session_id,
                name: state.title.clone(),
                explicit_name: state.title,
                derived_title: None,
                title_source,
                client_count: 0,
                created_at_ms,
                updated_at_ms,
                working_directory: working_directory.clone(),
                import: None,
                fork: None,
                execution: state.execution.map(|provenance| {
                    Box::new(bcode_session_models::ExecutionSessionSummary {
                        provenance,
                        visibility: state.visibility,
                    })
                }),
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
            reasoning_effort: state.reasoning_effort,
            reasoning_summary: state.reasoning_summary,
            current_agent: state.current_agent,
            latest_compaction_sequence: state.latest_compaction_sequence,
            context_epoch: state.latest_compaction_sequence.unwrap_or_default(),
            context_occupancy: None,
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

    pub(crate) fn summary(&self) -> SessionSummary {
        let mut summary = self.summary.clone();
        if summary.name.is_none() {
            summary.name = summary
                .explicit_name
                .clone()
                .or_else(|| summary.derived_title.clone());
        }
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
            SessionEventKind::SessionRenamed { name } => {
                self.summary.name.clone_from(name);
                self.summary.explicit_name.clone_from(name);
                if name.is_some() {
                    self.summary.title_source = SessionTitleSource::Explicit;
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
            SessionEventKind::SessionForked {
                source_session_id,
                source_title,
                source_cutoff_sequence,
                source_prompt_sequence,
                forked_at_ms,
                kind,
            } => {
                self.summary.fork = Some(SessionForkSummary {
                    source_session_id: *source_session_id,
                    source_title: source_title.clone(),
                    source_cutoff_sequence: *source_cutoff_sequence,
                    source_prompt_sequence: *source_prompt_sequence,
                    forked_at_ms: *forked_at_ms,
                    kind: *kind,
                });
            }
            SessionEventKind::UserMessage { text, .. } => {
                self.has_user_message = true;
                if self.summary.derived_title.is_none() {
                    self.summary.derived_title = Some(title_from_first_prompt(text));
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
            SessionEventKind::ModelChanged { provider, model } => {
                self.current_provider = Some(provider.clone());
                self.current_model = Some(model.clone());
                self.context_epoch = event.sequence;
                self.context_occupancy = None;
            }
            SessionEventKind::ReasoningChanged { effort, summary } => {
                self.reasoning_effort.clone_from(effort);
                self.reasoning_summary.clone_from(summary);
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
        if let Some(events) = &mut self.events {
            events.push(event.clone());
        }
        let _ = self.sender.send(event);
    }
}
