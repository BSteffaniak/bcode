//! Generic bounded session derivation mechanics.

use crate::{SessionError, SessionManager, db};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SessionDerivationRequest,
    SessionDerivationTerminalOutcome, SessionEvent, SessionEventKind, SessionEventProvenance,
    SessionHistoryCursor, SessionHistoryDirection, SessionHistoryQuery, SessionId, SessionSummary,
};
use std::collections::BTreeMap;
use std::path::Path;

const DERIVATION_EVENT_PAGE_SIZE: usize = 256;
const STAGING_DIRECTORY: &str = ".derivation-staging";

impl SessionManager {
    /// Derive a new session from one exact bounded source prefix.
    ///
    /// The destination is built under a non-canonical staging root. It becomes visible only after
    /// all event pages, generic lineage, and the authoritative composer draft validate, the
    /// database closes, and the complete session directory is atomically renamed into its one
    /// canonical path.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid requests, source-generation changes, incompatible source
    /// history, persistence failures, validation failures, or publication conflicts.
    pub async fn derive_session(
        &self,
        request: SessionDerivationRequest,
    ) -> Result<SessionDerivationTerminalOutcome, SessionError> {
        request
            .validate()
            .map_err(|error| SessionError::InvalidDerivationRequest(error.to_string()))?;
        let source_generation = self
            .current_session_generation(request.source.session_id)
            .await?;
        if source_generation != request.source.generation {
            return Err(SessionError::DerivationGenerationChanged {
                session_id: request.source.session_id,
                expected: request.source.generation,
                current: source_generation,
            });
        }
        let root = self
            .session_store_root()
            .ok_or(SessionError::DerivationRequiresPersistentStore)?;
        let destination_id = SessionId::new();
        let staging_root = root
            .join(STAGING_DIRECTORY)
            .join(request.operation_id.to_string());
        if staging_root.exists() {
            std::fs::remove_dir_all(&staging_root)?;
        }
        let result = self
            .build_staged_derivation(&request, destination_id, &staging_root)
            .await;
        if result.is_err() {
            let _cleanup = std::fs::remove_dir_all(&staging_root);
        }
        let summary = result?;
        let staged_dir = db::session_dir_path(&staging_root, destination_id);
        let canonical_dir = db::session_dir_path(&root, destination_id);
        if canonical_dir.exists() {
            let _cleanup = std::fs::remove_dir_all(&staging_root);
            return Err(SessionError::DerivationPublicationConflict(destination_id));
        }
        std::fs::rename(&staged_dir, &canonical_dir)?;
        let _cleanup = std::fs::remove_dir_all(&staging_root);
        self.adopt_derived_session(summary.clone()).await?;
        Ok(SessionDerivationTerminalOutcome::Succeeded {
            session: Box::new(summary),
        })
    }

    #[allow(clippy::too_many_lines)]
    async fn build_staged_derivation(
        &self,
        request: &SessionDerivationRequest,
        destination_id: SessionId,
        staging_root: &Path,
    ) -> Result<SessionSummary, SessionError> {
        let db = db::SessionDb::initialize_turso_in_root(destination_id, staging_root).await?;
        let created_at_ms = self.next_activity_timestamp_ms();
        let mut destination_sequence = 0_u64;
        let created = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: destination_sequence,
            timestamp_ms: created_at_ms,
            session_id: destination_id,
            provenance: None,
            kind: SessionEventKind::SessionCreated {
                name: request.destination_name.clone(),
                working_directory: request.source.working_directory.clone(),
            },
        };
        db.append_event_batch(&[(created, Some(created_at_ms))])
            .await?;
        destination_sequence += 1;

        let mut sequence_map = BTreeMap::new();
        let mut cursor = None;
        loop {
            let page = self
                .session_history_page(
                    request.source.session_id,
                    SessionHistoryQuery {
                        cursor,
                        limit: DERIVATION_EVENT_PAGE_SIZE,
                        direction: SessionHistoryDirection::Forward,
                    },
                )
                .await?;
            let mut batch = Vec::with_capacity(page.events.len());
            let mut reached_cutoff = false;
            for source_event in page.events {
                if source_event.sequence > request.cutoff_sequence {
                    reached_cutoff = true;
                    break;
                }
                cursor = Some(SessionHistoryCursor {
                    sequence: source_event.sequence.saturating_add(1),
                });
                if !is_copyable_derivation_event(&source_event.kind) {
                    continue;
                }
                let kind = crate::fork::rewrite_copied_event_kind(
                    source_event.kind.clone(),
                    &sequence_map,
                );
                sequence_map.insert(source_event.sequence, destination_sequence);
                batch.push((
                    SessionEvent {
                        schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
                        sequence: destination_sequence,
                        timestamp_ms: source_event.timestamp_ms,
                        session_id: destination_id,
                        provenance: Some(copy_event_provenance(&source_event)),
                        kind,
                    },
                    Some(source_event.timestamp_ms),
                ));
                destination_sequence += 1;
            }
            db.append_event_batch(&batch).await?;
            if reached_cutoff || !page.has_more || cursor.is_none() {
                break;
            }
        }
        let current_generation = self
            .current_session_generation(request.source.session_id)
            .await?;
        if current_generation != request.source.generation {
            db.close().await?;
            return Err(SessionError::DerivationGenerationChanged {
                session_id: request.source.session_id,
                expected: request.source.generation,
                current: current_generation,
            });
        }
        let derived_at_ms = self.next_activity_timestamp_ms();
        let lineage = SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            sequence: destination_sequence,
            timestamp_ms: derived_at_ms,
            session_id: destination_id,
            provenance: None,
            kind: SessionEventKind::SessionDerived {
                source_session_id: request.source.session_id,
                source_generation: request.source.generation,
                source_cutoff_sequence: request.cutoff_sequence,
                producer: request.lineage.producer.clone(),
                operation_kind: request.lineage.operation_kind.clone(),
                selected_source_sequence: request.lineage.selected_source_sequence,
                derived_at_ms,
            },
        };
        db.append_event_batch(&[(lineage, Some(derived_at_ms))])
            .await?;
        if let Some(draft) = &request.initial_draft {
            db.set_session_composer_draft(draft, derived_at_ms).await?;
        }
        db.validate_write_readiness().await?;
        let state = self.load_db_session_state(destination_id, &db).await?;
        let summary = state.summary();
        db.close().await?;
        write_staged_manifest(staging_root, &summary)?;
        Ok(summary)
    }

    async fn adopt_derived_session(&self, summary: SessionSummary) -> Result<(), SessionError> {
        let Some(store) = &self.store else {
            return Err(SessionError::DerivationRequiresPersistentStore);
        };
        store.write_session_manifest(summary.clone()).await?;
        store.schedule_catalog_summary(summary.clone()).await;
        let state = self
            .load_db_session_state(
                summary.id,
                &db::SessionDb::open_existing_turso_in_root(summary.id, &store.root_path()).await?,
            )
            .await?;
        let handle = crate::actor::SessionHandle::new(state, Some(store.clone()), None);
        self.inner.lock().await.sessions.insert(summary.id, handle);
        Ok(())
    }
}

fn write_staged_manifest(root: &Path, summary: &SessionSummary) -> Result<(), SessionError> {
    let store = crate::store::SessionStore::new(root.to_path_buf());
    store.write_session_manifest(summary)?;
    Ok(())
}

fn copy_event_provenance(event: &SessionEvent) -> SessionEventProvenance {
    SessionEventProvenance {
        source_event_id: Some(event.sequence.to_string()),
        source_timestamp_ms: Some(event.timestamp_ms),
        source_locator: Some(format!(
            "bcode://session/{}/event/{}",
            event.session_id, event.sequence
        )),
    }
}

const fn is_copyable_derivation_event(kind: &SessionEventKind) -> bool {
    !matches!(
        kind,
        SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ClientAttached { .. }
            | SessionEventKind::ClientDetached { .. }
            | SessionEventKind::SessionForked { .. }
            | SessionEventKind::SessionDerived { .. }
            | SessionEventKind::ExecutionSessionCreated { .. }
    )
}
