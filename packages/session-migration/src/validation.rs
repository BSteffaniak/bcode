use crate::{CURRENT_WRITER_EPOCH, MigrationPlanError, plan_writer_epoch_migration};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use thiserror::Error;

/// One current projection checkpoint observed after migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationProjectionValidation {
    /// Stable projection identity.
    pub projection: String,
    /// Projection schema observed in current storage.
    pub actual_schema_version: Option<u64>,
    /// Current projection schema required by the target API.
    pub expected_schema_version: u64,
    /// Last canonical sequence projected, when initialized.
    pub checkpoint: Option<u64>,
}

/// Current-format target facts collected by the migration-target API after rebuilding state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationTargetValidation {
    /// Canonical target tail, if the session contains events.
    pub canonical_tail: Option<u64>,
    /// Checkpointed current materialized projections.
    pub projections: Vec<MigrationProjectionValidation>,
    /// Model-context projection schema, when initialized.
    pub model_context_schema_version: Option<u64>,
    /// Current model-context schema required by the target API.
    pub expected_model_context_schema_version: u64,
    /// Model-context checkpoint, when initialized.
    pub model_context_checkpoint: Option<u64>,
    /// Compatibility projection schema, when initialized.
    pub compatibility_schema_version: Option<u64>,
    /// Current compatibility projection schema required by the target API.
    pub expected_compatibility_schema_version: u64,
    /// Compatibility projection checkpoint, when initialized.
    pub compatibility_checkpoint: Option<u64>,
    /// Whether current compatibility validation found no unresolved history.
    pub compatibility_resolved: bool,
}

/// Failure to validate a rebuilt current-format migration target.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MigrationTargetValidationError {
    /// A required materialized projection is absent or behind canonical history.
    #[error(
        "migration projection {projection} is stale: checkpoint={checkpoint:?} expected={expected:?}"
    )]
    ProjectionStale {
        /// Stable projection identity.
        projection: String,
        /// Observed checkpoint.
        checkpoint: Option<u64>,
        /// Canonical tail required by validation.
        expected: Option<u64>,
    },
    /// A required materialized projection uses a non-current schema.
    #[error(
        "migration projection {projection} schema is incompatible: actual={actual:?} expected={expected}"
    )]
    ProjectionIncompatible {
        /// Stable projection identity.
        projection: String,
        /// Observed schema version.
        actual: Option<u64>,
        /// Required current schema version.
        expected: u64,
    },
    /// The model-context projection is absent or behind canonical history.
    #[error(
        "migration model-context projection is stale: checkpoint={checkpoint:?} expected={expected:?}"
    )]
    ModelContextStale {
        /// Observed checkpoint.
        checkpoint: Option<u64>,
        /// Canonical tail required by validation.
        expected: Option<u64>,
    },
    /// The model-context projection uses a non-current schema.
    #[error(
        "migration model-context schema is incompatible: actual={actual:?} expected={expected}"
    )]
    ModelContextIncompatible {
        /// Observed schema version.
        actual: Option<u64>,
        /// Required current schema version.
        expected: u64,
    },
    /// Current compatibility validation found unresolved canonical history.
    #[error("migration target retains unresolved compatibility state")]
    CompatibilityUnresolved,
}

/// Validate projection and compatibility facts produced by the current migration-target API.
///
/// # Errors
///
/// Returns an error when any required current projection is absent, stale, incompatible, or when
/// compatibility state remains unresolved.
pub fn validate_migration_target(
    target: &MigrationTargetValidation,
) -> Result<(), MigrationTargetValidationError> {
    if target.canonical_tail.is_some() {
        for projection in &target.projections {
            if projection.actual_schema_version != Some(projection.expected_schema_version) {
                return Err(MigrationTargetValidationError::ProjectionIncompatible {
                    projection: projection.projection.clone(),
                    actual: projection.actual_schema_version,
                    expected: projection.expected_schema_version,
                });
            }
            if projection.checkpoint != target.canonical_tail {
                return Err(MigrationTargetValidationError::ProjectionStale {
                    projection: projection.projection.clone(),
                    checkpoint: projection.checkpoint,
                    expected: target.canonical_tail,
                });
            }
        }
        if target.model_context_schema_version != Some(target.expected_model_context_schema_version)
        {
            return Err(MigrationTargetValidationError::ModelContextIncompatible {
                actual: target.model_context_schema_version,
                expected: target.expected_model_context_schema_version,
            });
        }
        if target.model_context_checkpoint != target.canonical_tail {
            return Err(MigrationTargetValidationError::ModelContextStale {
                checkpoint: target.model_context_checkpoint,
                expected: target.canonical_tail,
            });
        }
    }
    if target.canonical_tail.is_some() {
        if target.compatibility_schema_version != Some(target.expected_compatibility_schema_version)
        {
            return Err(MigrationTargetValidationError::ProjectionIncompatible {
                projection: "session_compatibility".to_owned(),
                actual: target.compatibility_schema_version,
                expected: target.expected_compatibility_schema_version,
            });
        }
        if target.compatibility_checkpoint != target.canonical_tail {
            return Err(MigrationTargetValidationError::ProjectionStale {
                projection: "session_compatibility".to_owned(),
                checkpoint: target.compatibility_checkpoint,
                expected: target.canonical_tail,
            });
        }
    }
    if !target.compatibility_resolved {
        return Err(MigrationTargetValidationError::CompatibilityUnresolved);
    }
    Ok(())
}

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
    fn strict_target_validation_rejects_stale_incompatible_and_unresolved_state() {
        let valid = MigrationTargetValidation {
            canonical_tail: Some(9),
            projections: vec![MigrationProjectionValidation {
                projection: "session_state".to_owned(),
                actual_schema_version: Some(1),
                expected_schema_version: 1,
                checkpoint: Some(9),
            }],
            model_context_schema_version: Some(2),
            expected_model_context_schema_version: 2,
            model_context_checkpoint: Some(9),
            compatibility_schema_version: Some(1),
            expected_compatibility_schema_version: 1,
            compatibility_checkpoint: Some(9),
            compatibility_resolved: true,
        };
        assert!(validate_migration_target(&valid).is_ok());

        let mut stale = valid.clone();
        stale.projections[0].checkpoint = Some(8);
        assert!(matches!(
            validate_migration_target(&stale),
            Err(MigrationTargetValidationError::ProjectionStale { .. })
        ));
        let mut incompatible = valid.clone();
        incompatible.model_context_schema_version = Some(1);
        assert!(matches!(
            validate_migration_target(&incompatible),
            Err(MigrationTargetValidationError::ModelContextIncompatible { .. })
        ));
        let mut unresolved = valid;
        unresolved.compatibility_resolved = false;
        assert_eq!(
            validate_migration_target(&unresolved),
            Err(MigrationTargetValidationError::CompatibilityUnresolved)
        );
    }

    #[test]
    fn empty_target_requires_resolved_compatibility_without_projection_rows() {
        let target = MigrationTargetValidation {
            canonical_tail: None,
            projections: Vec::new(),
            model_context_schema_version: None,
            expected_model_context_schema_version: 2,
            model_context_checkpoint: None,
            compatibility_schema_version: None,
            expected_compatibility_schema_version: 1,
            compatibility_checkpoint: None,
            compatibility_resolved: true,
        };
        assert!(validate_migration_target(&target).is_ok());
    }

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
