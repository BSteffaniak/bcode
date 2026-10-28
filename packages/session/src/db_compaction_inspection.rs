//! Bounded, non-mutating normalization of compaction evidence.

use super::{
    SessionDb, SessionDbError, SessionDbResult, SortDirection, required_non_negative_u64,
    seq_to_value,
};
use bcode_session_models::compaction_inspection::{
    COMPACTION_PROGRESS_OBSERVATION_EVENT, CompactionInspectionBlocker, CompactionInspectionPhase,
    CompactionProgressObservation, MAX_COMPACTION_INSPECTION_CANDIDATES,
    MAX_COMPACTION_PROGRESS_OBSERVATION_BYTES, SessionCompactionInspectionEntry,
    SessionCompactionInspectionPage, SessionCompactionInspectionQuery,
};
use bcode_session_models::{
    CompactionSynthesisCheckpoint, SessionEvent, SessionEventKind, SessionHistoryCursor,
    SessionTracePayload, SessionTracePhase,
};
use switchy::database::query::FilterableQuery as _;

impl SessionDb {
    async fn compaction_inspection_candidates(
        &self,
        query: SessionCompactionInspectionQuery,
    ) -> SessionDbResult<Vec<(u64, switchy::database::Row)>> {
        // Equality ranges use the existing event-type index with the rowid sequence suffix.
        // Avoid an IN query that could sort unbounded matches. Fetch only bounded metadata.
        let mut candidates = Vec::new();
        for event_type in [
            "trace_event",
            "context_compacted",
            "provider_context_compacted",
        ] {
            let mut select = self
                .db
                .select("events")
                .project_as(
                    switchy::database::query::identifier("event_seq"),
                    "event_seq",
                )
                .project_as(
                    switchy::database::query::identifier("schema_version"),
                    "schema_version",
                )
                .project_as(
                    switchy::database::query::byte_length(switchy::database::query::identifier(
                        "payload",
                    )),
                    "payload_bytes",
                )
                .where_eq("event_type", event_type)
                .sort("event_seq", SortDirection::Desc)
                .limit(query.limit.saturating_add(1));
            if let Some(cursor) = query.before {
                select = select.where_lte("event_seq", seq_to_value(cursor.sequence));
            }
            for row in select.execute(&**self.db).await? {
                let sequence = required_non_negative_u64(&row, "event_seq")?;
                candidates.push((sequence, row));
            }
        }
        candidates.sort_by_key(|(sequence, _)| std::cmp::Reverse(*sequence));
        Ok(candidates)
    }

    /// Inspect indexed compaction/trace candidates without replay, repair or checkpoint restore.
    ///
    /// Work is bounded by the candidate allowance even for unrelated trace history. The page
    /// returns historical observations, not current execution authority or an active-worker view.
    /// No host checkpoint content or arbitrary trace text crosses this boundary.
    ///
    /// # Errors
    /// Rejects an invalid allowance or database/index read failures. Invalid canonical payloads
    /// become content-free degraded entries rather than leaking decoder errors or partial data.
    pub async fn compaction_inspection_page(
        &self,
        query: SessionCompactionInspectionQuery,
    ) -> SessionDbResult<SessionCompactionInspectionPage> {
        if query.limit == 0 || query.limit > MAX_COMPACTION_INSPECTION_CANDIDATES {
            return Err(SessionDbError::HistoryReadLimitExceeded {
                requested: query.limit,
                maximum: MAX_COMPACTION_INSPECTION_CANDIDATES,
            });
        }
        let candidates = self.compaction_inspection_candidates(query).await?;
        let next_cursor = candidates
            .get(query.limit)
            .map(|(sequence, _)| SessionHistoryCursor {
                sequence: *sequence,
            });
        let scanned_events = candidates.len().min(query.limit);
        let mut entries = Vec::new();
        let mut remaining_physical_bytes = 2 * 1024 * 1024_u64;
        let mut decode_budget = inspection_decode_budget();
        let mut degraded = false;
        for (sequence, row) in candidates.iter().take(query.limit) {
            let schema = required_non_negative_u64(row, "schema_version")?;
            let entry = if schema
                == u64::from(bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION)
            {
                let bytes = required_non_negative_u64(row, "payload_bytes")?;
                if bytes > remaining_physical_bytes {
                    entries.push(invalid_entry(
                        *sequence,
                        CompactionInspectionBlocker::InspectionBudgetExceeded,
                    ));
                    degraded = true;
                    continue;
                }
                remaining_physical_bytes -= bytes;
                let payload_rows = self
                    .db
                    .select("events")
                    .columns(&["event_seq", "payload"])
                    .where_eq("event_seq", seq_to_value(*sequence))
                    .limit(1)
                    .execute(&**self.db)
                    .await?;
                let Some(payload_row) = payload_rows.first() else {
                    entries.push(invalid_entry(
                        *sequence,
                        CompactionInspectionBlocker::InvalidEvidence,
                    ));
                    degraded = true;
                    continue;
                };
                match crate::db_event_store::strict_event_from_row_with_budget(
                    payload_row,
                    self.session_id,
                    &mut decode_budget,
                ) {
                    Ok(event) => normalize(&event),
                    Err(crate::db_event_store::BudgetedEventRowError::ResourceLimit) => {
                        Some(invalid_entry(
                            *sequence,
                            CompactionInspectionBlocker::InspectionBudgetExceeded,
                        ))
                    }
                    Err(_) => Some(invalid_entry(
                        *sequence,
                        CompactionInspectionBlocker::InvalidEvidence,
                    )),
                }
            } else {
                Some(invalid_entry(
                    *sequence,
                    CompactionInspectionBlocker::UnsupportedFormat,
                ))
            };
            if let Some(entry) = entry {
                degraded |= matches!(
                    entry.blocker,
                    Some(
                        CompactionInspectionBlocker::InvalidEvidence
                            | CompactionInspectionBlocker::UnsupportedFormat
                            | CompactionInspectionBlocker::InspectionBudgetExceeded
                    )
                );
                entries.push(entry);
            }
        }
        entries.reverse();
        Ok(SessionCompactionInspectionPage {
            version: 1,
            session_id: self.session_id,
            entries,
            scanned_events,
            degraded,
            next_cursor,
            has_more: next_cursor.is_some(),
        })
    }
}

/// Inspect borrowed ephemeral history with a bounded reverse window and shared normalization.
///
/// # Errors
/// Rejects invalid candidate allowances or invalid structural budget configuration.
pub fn memory_compaction_inspection_page(
    session_id: bcode_session_models::SessionId,
    history: &[SessionEvent],
    query: SessionCompactionInspectionQuery,
) -> SessionDbResult<SessionCompactionInspectionPage> {
    if query.limit == 0 || query.limit > MAX_COMPACTION_INSPECTION_CANDIDATES {
        return Err(SessionDbError::HistoryReadLimitExceeded {
            requested: query.limit,
            maximum: MAX_COMPACTION_INSPECTION_CANDIDATES,
        });
    }
    // Binary seek then bounded borrowed traversal: no replay, cloning or index rebuild.
    // Unlike persisted indexed candidates, every ephemeral event consumes the scan allowance.
    let end = query.before.map_or(history.len(), |cursor| {
        history.partition_point(|event| event.sequence <= cursor.sequence)
    });
    let start = end.saturating_sub(query.limit);
    let limits = bcode_session_models::SessionObservationSourceLimits {
        max_scalar_capacity_bytes: 2 * 1024 * 1024,
        max_collection_entries: 32_768,
        max_value_nodes: 65_536,
        max_depth: 64,
        max_queued_events: MAX_COMPACTION_INSPECTION_CANDIDATES,
        max_in_flight_events: 0,
    };
    let retention_limits = limits
        .retention_limits()
        .map_err(|_| SessionDbError::InvalidData("invalid compaction inspection bounds".into()))?;
    let mut usage = bcode_session_models::retention::ObservationRetentionUsage::default();
    let mut entries = Vec::new();
    let mut degraded = false;
    for event in history[start..end].iter().rev() {
        if !matches!(
            event.kind,
            SessionEventKind::TraceEvent { .. }
                | SessionEventKind::ContextCompacted { .. }
                | SessionEventKind::ProviderContextCompacted { .. }
        ) {
            continue;
        }
        let entry =
            if event.schema_version == bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION {
                match bcode_session_models::retention_events::inspect_durable(event, &limits)
                    .and_then(|next| usage.try_add(next, retention_limits))
                {
                    Ok(()) => normalize(event),
                    Err(_) => Some(invalid_entry(
                        event.sequence,
                        CompactionInspectionBlocker::InspectionBudgetExceeded,
                    )),
                }
            } else {
                Some(invalid_entry(
                    event.sequence,
                    CompactionInspectionBlocker::UnsupportedFormat,
                ))
            };
        if let Some(entry) = entry {
            degraded |= matches!(
                entry.blocker,
                Some(
                    CompactionInspectionBlocker::InvalidEvidence
                        | CompactionInspectionBlocker::UnsupportedFormat
                        | CompactionInspectionBlocker::InspectionBudgetExceeded
                )
            );
            entries.push(entry);
        }
    }
    entries.reverse();
    let next_cursor = start.checked_sub(1).map(|index| SessionHistoryCursor {
        sequence: history[index].sequence,
    });
    Ok(SessionCompactionInspectionPage {
        version: 1,
        session_id,
        entries,
        scanned_events: end - start,
        degraded,
        next_cursor,
        has_more: next_cursor.is_some(),
    })
}

const fn invalid_entry(
    sequence: u64,
    blocker: CompactionInspectionBlocker,
) -> SessionCompactionInspectionEntry {
    SessionCompactionInspectionEntry {
        sequence,
        phase: None,
        progress: None,
        blocker: Some(blocker),
    }
}

fn normalize(event: &SessionEvent) -> Option<SessionCompactionInspectionEntry> {
    let sequence = event.sequence;
    let mut entry = SessionCompactionInspectionEntry {
        sequence,
        phase: None,
        progress: None,
        blocker: None,
    };
    match &event.kind {
        SessionEventKind::ContextCompacted { .. }
        | SessionEventKind::ProviderContextCompacted { .. } => {
            entry.phase = Some(CompactionInspectionPhase::Committed);
        }
        SessionEventKind::TraceEvent { trace } => match &trace.payload {
            SessionTracePayload::ContextCompaction {
                compacted, message, ..
            } => {
                entry.phase = match trace.phase {
                    SessionTracePhase::ContextCompactionStarted => {
                        Some(CompactionInspectionPhase::Started)
                    }
                    SessionTracePhase::ContextCompactionFinished => {
                        Some(CompactionInspectionPhase::Finished)
                    }
                    SessionTracePhase::ContextCompactionSkipped => {
                        Some(CompactionInspectionPhase::Skipped)
                    }
                    SessionTracePhase::ContextCompactionDiagnostic => None,
                    _ => {
                        return Some(invalid_entry(
                            sequence,
                            CompactionInspectionBlocker::InvalidEvidence,
                        ));
                    }
                };
                entry.blocker =
                    if trace.phase == SessionTracePhase::ContextCompactionFinished && !compacted {
                        Some(
                            if message.as_deref().is_some_and(|message| {
                                message.contains("deadline exhausted")
                                    || message.contains("allowance exhausted")
                                    || message.contains("budget exhausted")
                            }) {
                                CompactionInspectionBlocker::BudgetExhausted
                            } else {
                                CompactionInspectionBlocker::ProgressNotRecorded
                            },
                        )
                    } else if *compacted {
                        None
                    } else {
                        Some(CompactionInspectionBlocker::ProgressNotRecorded)
                    };
            }
            SessionTracePayload::ProviderEvent { event_type, detail }
                if event_type == COMPACTION_PROGRESS_OBSERVATION_EVENT =>
            {
                if trace.phase != SessionTracePhase::ModelProviderEvent {
                    return Some(invalid_entry(
                        sequence,
                        CompactionInspectionBlocker::InvalidEvidence,
                    ));
                }
                let progress = match decode_observation(detail.as_deref()) {
                    Ok(progress) => progress,
                    Err(blocker) => return Some(invalid_entry(sequence, blocker)),
                };
                entry.phase = Some(progress.phase);
                entry.blocker = progress.blocker;
                entry.progress = Some(progress);
            }
            SessionTracePayload::ProviderEvent { event_type, .. }
                if event_type == bcode_session_models::COMPACTION_SYNTHESIS_CHECKPOINT_EVENT =>
            {
                let Ok(Some(checkpoint)) = CompactionSynthesisCheckpoint::from_event(event) else {
                    return Some(invalid_entry(
                        sequence,
                        CompactionInspectionBlocker::InvalidEvidence,
                    ));
                };
                entry.phase = Some(if checkpoint.host_payload.is_some() {
                    entry.blocker = Some(CompactionInspectionBlocker::CheckpointNotValidated);
                    CompactionInspectionPhase::CheckpointRetained
                } else {
                    CompactionInspectionPhase::CheckpointRetired
                });
            }
            SessionTracePayload::ProviderEvent { event_type, .. }
                if event_type.starts_with("compaction_progress_")
                    || event_type.starts_with("compaction_synthesis_checkpoint_") =>
            {
                return Some(invalid_entry(
                    sequence,
                    CompactionInspectionBlocker::UnsupportedFormat,
                ));
            }
            _ => return None,
        },
        _ => return None,
    }
    Some(entry)
}

fn decode_observation(
    detail: Option<&str>,
) -> Result<CompactionProgressObservation, CompactionInspectionBlocker> {
    let detail = detail
        .filter(|detail| detail.len() <= MAX_COMPACTION_PROGRESS_OBSERVATION_BYTES)
        .ok_or(CompactionInspectionBlocker::InvalidEvidence)?;
    let progress: CompactionProgressObservation =
        serde_json::from_str(detail).map_err(|_| CompactionInspectionBlocker::UnsupportedFormat)?;
    progress.validate()?;
    Ok(progress)
}

#[cfg(test)]
#[path = "db_compaction_inspection_tests.rs"]
mod tests;

const fn inspection_decode_budget() -> crate::persisted::EventDecodeBudget {
    crate::persisted::EventDecodeBudget::new(
        2 * 1024 * 1024,
        bcode_session_models::retention::ObservationRetentionLimits {
            scalar_capacity_bytes: 2 * 1024 * 1024,
            collection_entries: 32_768,
            value_nodes: 65_536,
            depth: 64,
            copies: MAX_COMPACTION_INSPECTION_CANDIDATES,
        },
    )
}
