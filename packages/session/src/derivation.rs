//! Generic bounded session derivation mechanics.

use crate::{SessionError, SessionManager, db};
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, SESSION_DERIVATION_CONTRACT_VERSION,
    SessionDerivationOperationId, SessionDerivationOperationSnapshot, SessionDerivationPhase,
    SessionDerivationProgress, SessionDerivationRequest, SessionDerivationTerminalOutcome,
    SessionEvent, SessionEventKind, SessionEventProvenance, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryQuery, SessionId, SessionSummary,
};
use sha2::{Digest as _, Sha256};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

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
        let fingerprint = derivation_request_fingerprint(&request)?;
        let cancellation = {
            let mut operations = self.derivation_operations.lock().await;
            if let Some(existing) = operations.get(&request.operation_id) {
                if existing.request_fingerprint != fingerprint {
                    return Err(SessionError::DerivationOperationConflict(
                        request.operation_id,
                    ));
                }
                if let Some(outcome) = &existing.snapshot.outcome {
                    return Ok(outcome.clone());
                }
                return Err(SessionError::DerivationOperationConflict(
                    request.operation_id,
                ));
            }
            let cancellation = Arc::new(AtomicBool::new(false));
            operations.insert(
                request.operation_id,
                crate::DerivationOperationState {
                    request_fingerprint: fingerprint,
                    snapshot: initial_operation_snapshot(&request),
                    cancellation: Arc::clone(&cancellation),
                },
            );
            cancellation
        };
        let result = self.derive_session_inner(&request, &cancellation).await;
        let outcome = match result {
            Ok(summary) => SessionDerivationTerminalOutcome::Succeeded {
                session: Box::new(summary),
            },
            Err(SessionError::DerivationCancelled(_)) => {
                SessionDerivationTerminalOutcome::Cancelled
            }
            Err(error) => {
                self.finish_derivation_operation(
                    request.operation_id,
                    SessionDerivationTerminalOutcome::Failed {
                        code: derivation_error_code(&error).to_owned(),
                        message: error.to_string(),
                    },
                )
                .await;
                return Err(error);
            }
        };
        self.finish_derivation_operation(request.operation_id, outcome.clone())
            .await;
        Ok(outcome)
    }

    async fn derive_session_inner(
        &self,
        request: &SessionDerivationRequest,
        cancellation: &AtomicBool,
    ) -> Result<SessionSummary, SessionError> {
        ensure_not_cancelled(request.operation_id, cancellation)?;
        self.update_derivation_progress(
            request.operation_id,
            SessionDerivationPhase::Validating,
            0,
            0,
        )
        .await;
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
            .build_staged_derivation(request, destination_id, &staging_root, cancellation)
            .await;
        if result.is_err() {
            let _cleanup = std::fs::remove_dir_all(&staging_root);
        }
        let summary = result?;
        ensure_not_cancelled(request.operation_id, cancellation)?;
        self.update_derivation_progress(
            request.operation_id,
            SessionDerivationPhase::Publishing,
            0,
            0,
        )
        .await;
        let staged_dir = db::session_dir_path(&staging_root, destination_id);
        let canonical_dir = db::session_dir_path(&root, destination_id);
        if canonical_dir.exists() {
            let _cleanup = std::fs::remove_dir_all(&staging_root);
            return Err(SessionError::DerivationPublicationConflict(destination_id));
        }
        std::fs::rename(&staged_dir, &canonical_dir)?;
        let _cleanup = std::fs::remove_dir_all(&staging_root);
        self.adopt_derived_session(summary.clone()).await?;
        Ok(summary)
    }

    /// Return the latest immutable-aware status snapshot for one derivation operation.
    ///
    /// # Errors
    ///
    /// Returns [`SessionError::DerivationOperationNotFound`] when the operation is unknown.
    pub async fn session_derivation_status(
        &self,
        operation_id: SessionDerivationOperationId,
    ) -> Result<SessionDerivationOperationSnapshot, SessionError> {
        self.derivation_operations
            .lock()
            .await
            .get(&operation_id)
            .map(|state| state.snapshot.clone())
            .ok_or(SessionError::DerivationOperationNotFound(operation_id))
    }

    /// Request cancellation for a running derivation operation.
    ///
    /// Returns `false` for an unknown or already-terminal operation. Once terminal, operation
    /// state remains immutable.
    pub async fn cancel_session_derivation(
        &self,
        operation_id: SessionDerivationOperationId,
    ) -> bool {
        let operations = self.derivation_operations.lock().await;
        let Some(state) = operations.get(&operation_id) else {
            return false;
        };
        if state.snapshot.outcome.is_some() {
            return false;
        }
        state.cancellation.store(true, Ordering::Release);
        true
    }

    async fn update_derivation_progress(
        &self,
        operation_id: SessionDerivationOperationId,
        phase: SessionDerivationPhase,
        copied_events: u64,
        copied_bytes: u64,
    ) {
        if let Some(state) = self
            .derivation_operations
            .lock()
            .await
            .get_mut(&operation_id)
            && state.snapshot.outcome.is_none()
        {
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            state.snapshot.progress.phase = phase;
            state.snapshot.progress.copied_events = copied_events;
            state.snapshot.progress.copied_bytes = copied_bytes;
        }
    }

    async fn finish_derivation_operation(
        &self,
        operation_id: SessionDerivationOperationId,
        outcome: SessionDerivationTerminalOutcome,
    ) {
        if let Some(state) = self
            .derivation_operations
            .lock()
            .await
            .get_mut(&operation_id)
            && state.snapshot.outcome.is_none()
        {
            state.snapshot.revision = state.snapshot.revision.saturating_add(1);
            state.snapshot.outcome = Some(outcome);
        }
    }

    #[allow(clippy::too_many_lines)]
    async fn build_staged_derivation(
        &self,
        request: &SessionDerivationRequest,
        destination_id: SessionId,
        staging_root: &Path,
        cancellation: &AtomicBool,
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
        let mut copied_events = 0_u64;
        let mut copied_bytes = 0_u64;
        loop {
            ensure_not_cancelled(request.operation_id, cancellation)?;
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
                copied_bytes = copied_bytes.saturating_add(
                    u64::try_from(
                        serde_json::to_vec(&source_event)
                            .map_err(|error| {
                                SessionError::DerivationSerialization(error.to_string())
                            })?
                            .len(),
                    )
                    .unwrap_or(u64::MAX),
                );
                copied_events = copied_events.saturating_add(1);
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
            self.update_derivation_progress(
                request.operation_id,
                SessionDerivationPhase::Copying,
                copied_events,
                copied_bytes,
            )
            .await;
            if reached_cutoff || !page.has_more || cursor.is_none() {
                break;
            }
        }
        ensure_not_cancelled(request.operation_id, cancellation)?;
        self.update_derivation_progress(
            request.operation_id,
            SessionDerivationPhase::Finalizing,
            copied_events,
            copied_bytes,
        )
        .await;
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

fn derivation_request_fingerprint(
    request: &SessionDerivationRequest,
) -> Result<String, SessionError> {
    let encoded = serde_json::to_vec(request)
        .map_err(|error| SessionError::DerivationSerialization(error.to_string()))?;
    Ok(format!("{:x}", Sha256::digest(encoded)))
}

const fn initial_operation_snapshot(
    request: &SessionDerivationRequest,
) -> SessionDerivationOperationSnapshot {
    SessionDerivationOperationSnapshot {
        version: SESSION_DERIVATION_CONTRACT_VERSION,
        operation_id: request.operation_id,
        revision: 0,
        progress: SessionDerivationProgress {
            phase: SessionDerivationPhase::Validating,
            copied_events: 0,
            copied_bytes: 0,
            source_cutoff_sequence: request.cutoff_sequence,
        },
        outcome: None,
    }
}

fn ensure_not_cancelled(
    operation_id: SessionDerivationOperationId,
    cancellation: &AtomicBool,
) -> Result<(), SessionError> {
    if cancellation.load(Ordering::Acquire) {
        Err(SessionError::DerivationCancelled(operation_id))
    } else {
        Ok(())
    }
}

const fn derivation_error_code(error: &SessionError) -> &'static str {
    match error {
        SessionError::DerivationCancelled(_) => "cancelled",
        SessionError::DerivationGenerationChanged { .. } => "source_generation_changed",
        SessionError::InvalidDerivationRequest(_) => "invalid_request",
        SessionError::DerivationPublicationConflict(_) => "publication_conflict",
        _ => "derivation_failed",
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
