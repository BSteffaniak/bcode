//! Session forking and event-copy behavior.

use crate::{SessionError, SessionManager, normalize_session_name};
use bcode_session_models::{
    ExecutionSessionProvenance, SessionEvent, SessionEventKind, SessionEventProvenance,
    SessionForkKind, SessionForkResult, SessionForkSummary, SessionHistoryDirection,
    SessionHistoryQuery, SessionId, SessionSummary,
};
use std::{collections::BTreeMap, path::PathBuf};

impl SessionManager {
    /// Fork a session from a selected user prompt into a new session.
    ///
    /// The selected prompt is returned as draft text and is not appended to the new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist, the prompt cannot be found,
    /// or the copied events cannot be persisted.
    pub async fn fork_session_from_prompt(
        &self,
        source_session_id: SessionId,
        prompt_sequence: u64,
        name: Option<String>,
    ) -> Result<SessionForkResult, SessionError> {
        let source = self.session_summary(source_session_id).await?;
        let events = self.session_history(source_session_id).await?;
        let Some(prompt_event) = events
            .iter()
            .find(|event| event.sequence == prompt_sequence)
        else {
            return Err(SessionError::ForkPromptNotFound {
                session_id: source_session_id,
                sequence: prompt_sequence,
            });
        };
        let SessionEventKind::UserMessage { text: draft, .. } = &prompt_event.kind else {
            return Err(SessionError::ForkPromptNotFound {
                session_id: source_session_id,
                sequence: prompt_sequence,
            });
        };
        let copied_events = events
            .iter()
            .filter(|event| event.sequence < prompt_sequence)
            .cloned()
            .collect::<Vec<_>>();
        let source_title = Some(source.display_title().to_string());
        let forked_at_ms = self.next_activity_timestamp_ms();
        let fork_name = normalize_session_name(name)
            .or_else(|| Some(format!("[fork] {}", source.display_title())));
        let session = self
            .copy_session_events(
                fork_name,
                source.working_directory,
                copied_events,
                SessionEventKind::SessionForked {
                    source_session_id,
                    source_title,
                    source_cutoff_sequence: prompt_sequence.checked_sub(1),
                    source_prompt_sequence: Some(prompt_sequence),
                    forked_at_ms,
                    kind: SessionForkKind::Fork,
                },
            )
            .await?;
        Ok(SessionForkResult {
            session,
            draft: Some(draft.clone()),
        })
    }

    /// Clone a session's complete event history into a new session.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist or the copied events cannot be
    /// persisted.
    pub async fn clone_session(
        &self,
        source_session_id: SessionId,
        name: Option<String>,
    ) -> Result<SessionForkResult, SessionError> {
        self.clone_session_at_generation(source_session_id, name, None)
            .await
    }

    /// Clone a session's complete history if its snapshot matches an expected generation.
    ///
    /// # Errors
    ///
    /// Returns an error when the source session does not exist, the source generation differs
    /// from `expected_generation`, or copied events cannot be persisted.
    pub async fn clone_session_at_generation(
        &self,
        source_session_id: SessionId,
        name: Option<String>,
        expected_generation: Option<u64>,
    ) -> Result<SessionForkResult, SessionError> {
        let events = self.session_history(source_session_id).await?;
        let source_cutoff_sequence = events.last().map_or(0, |event| event.sequence);
        if let Some(expected) = expected_generation
            && source_cutoff_sequence != expected
        {
            return Err(SessionError::CloneGenerationChanged {
                session_id: source_session_id,
                expected,
                current: source_cutoff_sequence,
            });
        }
        let source = self.session_summary(source_session_id).await?;
        if let Some(expected) = expected_generation {
            let current = self
                .session_history_page(
                    source_session_id,
                    SessionHistoryQuery {
                        cursor: None,
                        limit: 1,
                        direction: SessionHistoryDirection::Backward,
                    },
                )
                .await?
                .events
                .first()
                .map_or(0, |event| event.sequence);
            if current != expected {
                return Err(SessionError::CloneGenerationChanged {
                    session_id: source_session_id,
                    expected,
                    current,
                });
            }
        }
        let source_title = Some(source.display_title().to_string());
        let source_cutoff_sequence = events.last().map(|event| event.sequence);
        let forked_at_ms = self.next_activity_timestamp_ms();
        let clone_name = normalize_session_name(name)
            .or_else(|| Some(format!("[clone] {}", source.display_title())));
        let session = self
            .copy_session_events(
                clone_name,
                source.working_directory,
                events,
                SessionEventKind::SessionForked {
                    source_session_id,
                    source_title,
                    source_cutoff_sequence,
                    source_prompt_sequence: None,
                    forked_at_ms,
                    kind: SessionForkKind::Clone,
                },
            )
            .await?;
        Ok(SessionForkResult {
            session,
            draft: None,
        })
    }

    async fn copy_session_events(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        events: Vec<SessionEvent>,
        marker: SessionEventKind,
    ) -> Result<SessionSummary, SessionError> {
        self.copy_session_events_with_execution(name, working_directory, events, marker, None)
            .await
    }

    pub(crate) async fn copy_session_events_with_execution(
        &self,
        name: Option<String>,
        working_directory: PathBuf,
        events: Vec<SessionEvent>,
        marker: SessionEventKind,
        execution: Option<ExecutionSessionProvenance>,
    ) -> Result<SessionSummary, SessionError> {
        let session = self
            .create_session_record(name, working_directory, execution)
            .await?;
        let handle = self.session_handle(session.id).await?;
        let mut sequence_map = BTreeMap::new();
        for event in events {
            if !is_copyable_fork_event(&event.kind) {
                continue;
            }
            let kind = rewrite_copied_event_kind(event.kind.clone(), &sequence_map);
            let copied = handle
                .append_event_with_provenance(
                    kind,
                    Some(copy_event_provenance(&event)),
                    self.next_activity_timestamp_ms(),
                )
                .await?;
            sequence_map.insert(event.sequence, copied.sequence);
        }
        let marker_event = handle
            .append_event(marker.clone(), self.next_activity_timestamp_ms())
            .await?;
        let mut summary = handle.summary().await?;
        self.release_persistent_idle_session_resources(session.id)
            .await;
        summary.fork = session_fork_summary_from_marker(&marker);
        self.publish_committed_mutation(marker_event, summary.clone());
        Ok(summary)
    }
}

fn session_fork_summary_from_marker(marker: &SessionEventKind) -> Option<SessionForkSummary> {
    if let SessionEventKind::SessionForked {
        source_session_id,
        source_title,
        source_cutoff_sequence,
        source_prompt_sequence,
        forked_at_ms,
        kind,
    } = marker
    {
        Some(SessionForkSummary {
            source_session_id: *source_session_id,
            source_title: source_title.clone(),
            source_cutoff_sequence: *source_cutoff_sequence,
            source_prompt_sequence: *source_prompt_sequence,
            forked_at_ms: *forked_at_ms,
            kind: *kind,
        })
    } else {
        None
    }
}

fn copy_event_provenance(event: &SessionEvent) -> SessionEventProvenance {
    let source_locator = format!(
        "bcode://session/{}/event/{}",
        event.session_id, event.sequence
    );
    SessionEventProvenance {
        source_event_id: Some(event.sequence.to_string()),
        source_timestamp_ms: None,
        source_locator: Some(source_locator),
    }
}

const fn is_copyable_fork_event(kind: &SessionEventKind) -> bool {
    !matches!(
        kind,
        SessionEventKind::SessionCreated { .. }
            | SessionEventKind::ClientAttached { .. }
            | SessionEventKind::ClientDetached { .. }
            | SessionEventKind::SessionForked { .. }
    )
}

pub fn rewrite_copied_event_kind(
    kind: SessionEventKind,
    sequence_map: &BTreeMap<u64, u64>,
) -> SessionEventKind {
    match kind {
        SessionEventKind::ContextCompacted {
            summary,
            compacted_through_sequence,
        } => SessionEventKind::ContextCompacted {
            summary,
            compacted_through_sequence: sequence_map
                .get(&compacted_through_sequence)
                .copied()
                .unwrap_or(compacted_through_sequence),
        },
        SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence,
        } => SessionEventKind::ProviderContextCompacted {
            snapshot,
            compacted_through_sequence: sequence_map
                .get(&compacted_through_sequence)
                .copied()
                .unwrap_or(compacted_through_sequence),
        },
        other => other,
    }
}
