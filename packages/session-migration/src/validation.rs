use crate::{CURRENT_WRITER_EPOCH, MigrationPlanError, plan_writer_epoch_migration};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// Migration-owned classification of an observed durable writer epoch.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterEpochCompatibility {
    /// The store already uses the current writer epoch.
    Current,
    /// The store was produced by a released writer with a complete migration path.
    ReleasedHistorical,
    /// The store was produced by an unknown future writer.
    UnknownFuture,
    /// The epoch is neither current nor a released historical writer.
    Unsupported,
}

/// Classify an observed writer epoch without leaking the migration graph into current runtime code.
#[must_use]
pub fn classify_writer_epoch(writer_epoch: u32) -> WriterEpochCompatibility {
    if writer_epoch == CURRENT_WRITER_EPOCH {
        return WriterEpochCompatibility::Current;
    }
    match plan_writer_epoch_migration(writer_epoch) {
        Ok(_) => WriterEpochCompatibility::ReleasedHistorical,
        Err(MigrationPlanError::FutureWriter { .. }) => WriterEpochCompatibility::UnknownFuture,
        Err(
            MigrationPlanError::MissingStep { .. } | MigrationPlanError::NonMonotonicStep { .. },
        ) => WriterEpochCompatibility::Unsupported,
    }
}

/// Failure to finalize the durable writer epoch after target validation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Error)]
pub enum WriterFinalizationError {
    /// Validation did not establish a strictly current writable target.
    #[error("session migration target validation is incomplete")]
    ValidationIncomplete,
    /// The caller attempted to install an epoch other than this build's current writer.
    #[error("session migration cannot finalize writer epoch {requested}; expected {current}")]
    NonCurrentTarget {
        /// Requested writer epoch.
        requested: u32,
        /// Current writer epoch required by this build.
        current: u32,
    },
}

/// Validate that writer finalization is the last step after strict target validation.
///
/// # Errors
///
/// Returns an error unless strict current validation succeeded and the requested epoch is current.
pub const fn validate_writer_finalization(
    requested_writer_epoch: u32,
    target_validation_complete: bool,
) -> Result<(), WriterFinalizationError> {
    if !target_validation_complete {
        return Err(WriterFinalizationError::ValidationIncomplete);
    }
    if requested_writer_epoch != CURRENT_WRITER_EPOCH {
        return Err(WriterFinalizationError::NonCurrentTarget {
            requested: requested_writer_epoch,
            current: CURRENT_WRITER_EPOCH,
        });
    }
    Ok(())
}

/// Canonical evidence used for either side of a completed migration receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationCanonicalReceiptEvidence {
    /// Canonical event count.
    pub event_count: u64,
    /// Canonical tail, if the session contains events.
    pub event_tail: Option<u64>,
    /// Digest over ordered canonical payloads.
    pub event_digest_sha256: String,
}

/// Inputs required to construct a validated migration receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionMigrationReceiptRequest {
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u32,
    /// Source canonical evidence captured before normalization.
    pub source: SessionMigrationCanonicalReceiptEvidence,
    /// Target canonical evidence captured after normalization.
    pub target: SessionMigrationCanonicalReceiptEvidence,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
    /// Completion time in Unix milliseconds.
    pub completed_at_ms: u64,
}

/// Durable audit receipt for one completed session migration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMigrationReceipt {
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u32,
    /// Writer epoch installed after validation.
    pub target_writer_epoch: u32,
    /// Ordered migration steps applied by the operation.
    pub migration_step_ids: Vec<String>,
    /// Canonical source event count.
    pub source_event_count: u64,
    /// Canonical source tail, if the session contains events.
    pub source_event_tail: Option<u64>,
    /// Digest over the ordered source canonical payloads.
    pub source_event_digest_sha256: String,
    /// Canonical target event count.
    pub target_event_count: u64,
    /// Canonical target tail, if the session contains events.
    pub target_event_tail: Option<u64>,
    /// Digest over the ordered target canonical payloads.
    pub target_event_digest_sha256: String,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
    /// Completion time in Unix milliseconds.
    pub completed_at_ms: u64,
}

/// Build a receipt using the complete migration plan selected by migration-owned policy.
///
/// # Errors
///
/// Returns an error when the source writer has no safe monotonic path to the current writer.
pub fn build_session_migration_receipt(
    request: SessionMigrationReceiptRequest,
) -> Result<SessionMigrationReceipt, MigrationPlanError> {
    let plan = plan_writer_epoch_migration(request.source_writer_epoch)?;
    Ok(SessionMigrationReceipt {
        operation_id: request.operation_id,
        source_writer_epoch: request.source_writer_epoch,
        target_writer_epoch: CURRENT_WRITER_EPOCH,
        migration_step_ids: plan.steps.iter().map(|step| step.id.to_owned()).collect(),
        source_event_count: request.source.event_count,
        source_event_tail: request.source.event_tail,
        source_event_digest_sha256: request.source.event_digest_sha256,
        target_event_count: request.target.event_count,
        target_event_tail: request.target.event_tail,
        target_event_digest_sha256: request.target.event_digest_sha256,
        converted_events: request.converted_events,
        retired_known_events: request.retired_known_events,
        completed_at_ms: request.completed_at_ms,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writer_epoch_classification_and_finalization_are_migration_owned() {
        assert_eq!(
            classify_writer_epoch(CURRENT_WRITER_EPOCH),
            WriterEpochCompatibility::Current
        );
        assert_eq!(
            classify_writer_epoch(2),
            WriterEpochCompatibility::ReleasedHistorical
        );
        assert_eq!(
            classify_writer_epoch(CURRENT_WRITER_EPOCH + 1),
            WriterEpochCompatibility::UnknownFuture
        );
        assert_eq!(
            classify_writer_epoch(0),
            WriterEpochCompatibility::Unsupported
        );
        assert_eq!(
            validate_writer_finalization(CURRENT_WRITER_EPOCH, false),
            Err(WriterFinalizationError::ValidationIncomplete)
        );
        assert!(validate_writer_finalization(CURRENT_WRITER_EPOCH, true).is_ok());
        assert!(matches!(
            validate_writer_finalization(CURRENT_WRITER_EPOCH - 1, true),
            Err(WriterFinalizationError::NonCurrentTarget { .. })
        ));
    }

    #[test]
    fn receipt_builder_uses_complete_migration_owned_plan() {
        let receipt = build_session_migration_receipt(SessionMigrationReceiptRequest {
            operation_id: "operation".to_owned(),
            source_writer_epoch: 2,
            source: SessionMigrationCanonicalReceiptEvidence {
                event_count: 3,
                event_tail: Some(2),
                event_digest_sha256: "source".to_owned(),
            },
            target: SessionMigrationCanonicalReceiptEvidence {
                event_count: 3,
                event_tail: Some(2),
                event_digest_sha256: "target".to_owned(),
            },
            converted_events: BTreeMap::from([("28:tool_call_finished".to_owned(), 1)]),
            retired_known_events: BTreeMap::new(),
            completed_at_ms: 10,
        })
        .expect("receipt");

        assert_eq!(receipt.source_writer_epoch, 2);
        assert_eq!(receipt.target_writer_epoch, CURRENT_WRITER_EPOCH);
        assert_eq!(
            receipt.migration_step_ids,
            [
                "session-writer-epoch-2-to-3",
                "session-writer-epoch-3-to-4",
                "session-writer-epoch-4-to-5",
            ]
        );
        assert_eq!(receipt.source_event_digest_sha256, "source");
        assert_eq!(receipt.target_event_digest_sha256, "target");
    }
}
