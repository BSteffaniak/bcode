//! Async adapter for blocking session store operations.

use super::{
    SessionEventStore, SessionMigrationReport, SessionState, SessionStoreError, index,
    spawn_blocking,
};
use bcode_session_models::{
    SessionEvent, SessionHistoryPage, SessionHistoryQuery, SessionId, SessionInputHistoryEntry,
    SessionSummary,
};
use std::{collections::BTreeMap, path::PathBuf};

pub struct PersistedSessionMetadata {
    pub summary: SessionSummary,
    pub working_directory: PathBuf,
    pub next_sequence: u64,
    pub event_count: usize,
    pub has_user_message: bool,
    pub current_provider: Option<String>,
    pub current_model: Option<String>,
    pub current_agent: Option<String>,
    pub latest_compaction_sequence: Option<u64>,
    pub total_metered_tokens: u64,
    pub index_issues: Vec<index::SessionIndexIssue>,
}

impl PersistedSessionMetadata {
    pub fn from_state(state: &SessionState) -> Self {
        Self {
            summary: state.summary.clone(),
            working_directory: state.working_directory.clone(),
            next_sequence: state.next_sequence,
            event_count: state.event_count,
            has_user_message: state.has_user_message,
            current_provider: state.current_provider.clone(),
            current_model: state.current_model.clone(),
            current_agent: state.current_agent.clone(),
            latest_compaction_sequence: state.latest_compaction_sequence,
            total_metered_tokens: state.total_metered_tokens,
            index_issues: state.index_issues.clone(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct SessionStoreExecutor {
    store: SessionEventStore,
}

impl SessionStoreExecutor {
    pub const fn new(store: SessionEventStore) -> Self {
        Self { store }
    }

    pub async fn load_catalog(
        &self,
    ) -> Result<BTreeMap<SessionId, SessionState>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.load_catalog()).await?
    }

    pub async fn load_session(
        &self,
        session_id: SessionId,
    ) -> Result<Option<SessionState>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.load_session(session_id)).await?
    }

    pub async fn migrate_event_log_to_current(
        &self,
        session_id: SessionId,
    ) -> Result<SessionMigrationReport, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.migrate_event_log_to_current(session_id)).await?
    }

    pub async fn ensure_fresh_index(
        &self,
        session_id: SessionId,
    ) -> Result<index::SessionIndex, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.ensure_fresh_index(session_id)).await?
    }

    pub async fn delete(&self, session_id: SessionId) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.delete(session_id)).await?
    }

    pub async fn append_event_frame(&self, event: SessionEvent) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.append(&event).map(|_| ())).await?
    }

    pub async fn write_metadata_index(
        &self,
        metadata: PersistedSessionMetadata,
    ) -> Result<(), SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.write_metadata_index(&metadata)).await?
    }

    pub async fn read_session_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.read_session_events(session_id)).await?
    }

    pub async fn read_session_history_page(
        &self,
        session_id: SessionId,
        query: SessionHistoryQuery,
    ) -> Result<SessionHistoryPage, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.read_session_history_page(session_id, query)).await?
    }

    pub async fn read_session_input_history(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionInputHistoryEntry>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.read_session_input_history(session_id)).await?
    }

    pub async fn read_model_context_events(
        &self,
        session_id: SessionId,
    ) -> Result<Vec<SessionEvent>, SessionStoreError> {
        let store = self.store.clone();
        spawn_blocking(move || store.read_model_context_events(session_id)).await?
    }
}
