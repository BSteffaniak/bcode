use crate::{CURRENT_WRITER_EPOCH, MigrationPlanError, plan_writer_epoch_migration};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
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

/// One durable migration-ledger row collected from a source store.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationLedgerRow {
    /// Stable migration identifier.
    pub id: String,
    /// Durable migration status.
    pub status: String,
}

/// Convert raw ledger rows into completed migration identifiers.
///
/// # Errors
///
/// Returns an error when any source migration is not completed.
pub fn completed_migration_ids(
    rows: impl IntoIterator<Item = MigrationLedgerRow>,
) -> Result<BTreeSet<String>, MigrationLedgerValidationError> {
    rows.into_iter()
        .map(|row| {
            if row.status != "completed" {
                return Err(MigrationLedgerValidationError::IncompleteMigration {
                    id: row.id,
                    status: row.status,
                });
            }
            Ok(row.id)
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationLedgerFacts {
    /// Ordered migration identifiers known to the current target schema.
    pub known_migration_ids: Vec<String>,
    /// Completed migration identifiers observed in the source store.
    pub completed_migration_ids: BTreeSet<String>,
}

/// Result of validating source migration-ledger shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ValidatedMigrationLedger {
    /// Number of completed migrations forming the known prefix.
    pub completed_prefix_len: usize,
    /// Total number of migrations required by the current target schema.
    pub current_migration_count: usize,
}

/// Validate one released ledger-prefix fixture case independently from the current target ledger.
///
/// Materialized-current cases must contain an ordered prefix ending at their declared endpoint.
/// Retired-superseded cases must contain only that historical identity. This validation is used by
/// permanent fixture coverage and does not interpret either shape as the live current ledger.
///
/// # Errors
///
/// Returns an error when the endpoint is unknown, global-only, has a mismatched treatment, or its
/// completed identifiers do not form the required case shape.
pub fn validate_released_ledger_prefix_fixture_case(
    case: &crate::ReleasedLedgerPrefixFixtureCase,
) -> Result<(), MigrationLedgerValidationError> {
    let Some(endpoint) = crate::RELEASED_MIGRATION_IDS
        .iter()
        .find(|migration| migration.id == case.endpoint)
    else {
        return Err(MigrationLedgerValidationError::UnknownMigration(
            case.endpoint.to_owned(),
        ));
    };
    if endpoint.domain != crate::ReleasedMigrationDomain::Session
        || endpoint.treatment != case.endpoint_treatment
    {
        return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
            endpoint: case.endpoint.to_owned(),
        });
    }
    let Some(expected_case) = crate::released_session_ledger_prefix_fixture_cases()
        .into_iter()
        .find(|expected| expected.endpoint == case.endpoint)
    else {
        return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
            endpoint: case.endpoint.to_owned(),
        });
    };
    if expected_case.completed_migration_ids != case.completed_migration_ids {
        return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
            endpoint: case.endpoint.to_owned(),
        });
    }
    match endpoint.treatment {
        crate::ReleasedMigrationTreatment::MaterializeCurrent => {
            if case.completed_migration_ids.last() != Some(&case.endpoint)
                || case.completed_migration_ids.iter().any(|id| {
                    !crate::RELEASED_MIGRATION_IDS.iter().any(|migration| {
                        migration.id == *id
                            && migration.domain == crate::ReleasedMigrationDomain::Session
                            && migration.treatment
                                == crate::ReleasedMigrationTreatment::MaterializeCurrent
                    })
                })
            {
                return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
                    endpoint: case.endpoint.to_owned(),
                });
            }
        }
        crate::ReleasedMigrationTreatment::RetiredSuperseded => {
            if case.completed_migration_ids != [case.endpoint] {
                return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
                    endpoint: case.endpoint.to_owned(),
                });
            }
        }
        crate::ReleasedMigrationTreatment::GlobalOnly => {
            return Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase {
                endpoint: case.endpoint.to_owned(),
            });
        }
    }
    Ok(())
}

/// Failure to interpret a source migration ledger.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MigrationLedgerValidationError {
    /// A source migration is not durably complete.
    #[error("migration {id} has status {status}")]
    IncompleteMigration { id: String, status: String },
    /// A released fixture case does not match its migration treatment or endpoint.
    #[error("invalid released ledger fixture case ending at {endpoint}")]
    InvalidReleasedFixtureCase { endpoint: String },
    /// A completed migration is not known by this build.
    #[error("unknown migration {0}")]
    UnknownMigration(String),
    /// Completed migrations do not form an ordered known prefix.
    #[error("completed migrations are not a contiguous known prefix")]
    NonContiguousPrefix,
}

/// Validate migration-ledger membership and prefix ordering.
///
/// # Errors
///
/// Returns an error for unknown migration identifiers or a non-contiguous completed prefix.
pub fn validate_migration_ledger(
    facts: &MigrationLedgerFacts,
) -> Result<ValidatedMigrationLedger, MigrationLedgerValidationError> {
    let known = facts
        .known_migration_ids
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>();
    if let Some(unknown) = facts
        .completed_migration_ids
        .iter()
        .find(|id| !known.contains(*id))
    {
        return Err(MigrationLedgerValidationError::UnknownMigration(
            unknown.clone(),
        ));
    }
    let completed_prefix_len = facts
        .known_migration_ids
        .iter()
        .take_while(|id| facts.completed_migration_ids.contains(*id))
        .count();
    if facts.completed_migration_ids.len() != completed_prefix_len {
        return Err(MigrationLedgerValidationError::NonContiguousPrefix);
    }
    Ok(ValidatedMigrationLedger {
        completed_prefix_len,
        current_migration_count: facts.known_migration_ids.len(),
    })
}

/// Classify policy-free target facts using migration-owned released-format policy.
///
/// # Errors
///
/// Returns an error for unknown or non-contiguous ledgers, inconsistent current contracts, and
/// unsupported writer epochs.
pub fn classify_target_storage(
    facts: &bcode_session_migration_target::StorageCompatibilityFacts,
) -> Result<
    bcode_session_migration_target::StorageCompatibility,
    bcode_session_migration_target::StorageCompatibilityError,
> {
    let completed_migration_ids =
        completed_migration_ids(facts.migration_rows.iter().map(|row| MigrationLedgerRow {
            id: row.id.clone(),
            status: row.status.clone(),
        }))
        .map_err(|_| bcode_session_migration_target::StorageCompatibilityError::IncompleteLedger)?;
    let ledger = validate_migration_ledger(&MigrationLedgerFacts {
        known_migration_ids: facts.current_migration_ids.clone(),
        completed_migration_ids,
    })
    .map_err(|error| {
        bcode_session_migration_target::StorageCompatibilityError::Classification(error.to_string())
    })?;
    let compatibility = classify_source_storage(
        ledger,
        StorageContractFacts {
            table_exists: facts.contract_table_exists,
            contract: facts.writer_epoch.map(|writer_epoch| StorageContractRow {
                schema_version: facts.contract_schema_version.unwrap_or_default(),
                writer_epoch,
            }),
        },
        facts.expected_contract_schema_version,
        facts.legacy_writer_epoch,
    )
    .map_err(|error| match error {
        SourceStorageCompatibilityError::WriterEpoch { actual, expected } => {
            bcode_session_migration_target::StorageCompatibilityError::WriterEpoch {
                actual,
                expected,
            }
        }
        error => bcode_session_migration_target::StorageCompatibilityError::Classification(
            error.to_string(),
        ),
    })?;
    Ok(match compatibility {
        SourceStorageCompatibility::Current { writer_epoch } => {
            bcode_session_migration_target::StorageCompatibility::Current { writer_epoch }
        }
        SourceStorageCompatibility::ReleasedHistorical { writer_epoch } => {
            bcode_session_migration_target::StorageCompatibility::MigrationRequired { writer_epoch }
        }
    })
}

/// Storage-contract facts collected without historical compatibility policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageContractFacts {
    /// Whether the current contract table exists.
    pub table_exists: bool,
    /// Current contract row facts, when present.
    pub contract: Option<StorageContractRow>,
}

/// Durable storage-contract row facts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct StorageContractRow {
    /// Contract schema version.
    pub schema_version: u64,
    /// Writer epoch recorded by the source store.
    pub writer_epoch: u64,
}

/// Migration-owned source storage classification.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceStorageCompatibility {
    /// The source implements the complete current contract.
    Current { writer_epoch: u64 },
    /// The source is a released historical store with a migration path.
    ReleasedHistorical { writer_epoch: u64 },
}

/// Failure to classify a source storage contract.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SourceStorageCompatibilityError {
    /// The ledger is current but its required contract table is absent.
    #[error("migration history claims the storage contract exists, but its table is missing")]
    MissingContractTable,
    /// The ledger is current but its required contract row is absent.
    #[error(
        "migration history claims the storage contract was initialized, but its row is missing"
    )]
    MissingContractRow,
    /// The source contract schema is unsupported.
    #[error("unsupported storage contract schema {actual}; expected {expected}")]
    ContractSchema { actual: u64, expected: u64 },
    /// The source writer epoch is unsupported by this build.
    #[error("unsupported session writer epoch {actual}; expected {expected}")]
    WriterEpoch { actual: u64, expected: u64 },
}

/// Classify source storage from ledger and contract facts.
///
/// # Errors
///
/// Returns an error for inconsistent current ledgers, unsupported contract schemas, and writers
/// without a released migration path.
pub fn classify_source_storage(
    ledger: ValidatedMigrationLedger,
    contract: StorageContractFacts,
    expected_contract_schema: u64,
    legacy_writer_epoch: u32,
) -> Result<SourceStorageCompatibility, SourceStorageCompatibilityError> {
    let ledger_current = ledger.completed_prefix_len == ledger.current_migration_count;
    if !contract.table_exists {
        return if ledger_current {
            Err(SourceStorageCompatibilityError::MissingContractTable)
        } else {
            Ok(SourceStorageCompatibility::ReleasedHistorical {
                writer_epoch: u64::from(legacy_writer_epoch),
            })
        };
    }
    let Some(contract) = contract.contract else {
        return if ledger_current {
            Err(SourceStorageCompatibilityError::MissingContractRow)
        } else {
            Ok(SourceStorageCompatibility::ReleasedHistorical {
                writer_epoch: u64::from(legacy_writer_epoch),
            })
        };
    };
    if contract.schema_version != expected_contract_schema {
        return Err(SourceStorageCompatibilityError::ContractSchema {
            actual: contract.schema_version,
            expected: expected_contract_schema,
        });
    }
    let Ok(writer_epoch) = u32::try_from(contract.writer_epoch) else {
        return Err(SourceStorageCompatibilityError::WriterEpoch {
            actual: contract.writer_epoch,
            expected: u64::from(CURRENT_WRITER_EPOCH),
        });
    };
    match classify_writer_epoch(writer_epoch) {
        WriterEpochCompatibility::Current if ledger_current => {
            Ok(SourceStorageCompatibility::Current {
                writer_epoch: contract.writer_epoch,
            })
        }
        WriterEpochCompatibility::Current | WriterEpochCompatibility::ReleasedHistorical => {
            Ok(SourceStorageCompatibility::ReleasedHistorical {
                writer_epoch: contract.writer_epoch,
            })
        }
        WriterEpochCompatibility::UnknownFuture | WriterEpochCompatibility::Unsupported => {
            Err(SourceStorageCompatibilityError::WriterEpoch {
                actual: contract.writer_epoch,
                expected: u64::from(CURRENT_WRITER_EPOCH),
            })
        }
    }
}

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

/// Classification totals produced while normalizing canonical history.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MigrationClassificationEvidence {
    /// Digest over ordered source payloads before normalization.
    pub source_payload_digest_sha256: String,
    /// Converted event counts keyed by `schema:kind`.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts keyed by `schema:kind`.
    pub retired_known_events: BTreeMap<String, u64>,
}

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
    fn released_ledger_fixture_validation_rejects_mismatched_case_shapes() {
        let invalid = crate::ReleasedLedgerPrefixFixtureCase {
            endpoint: "023_reset_legacy_context_occupancy_projection",
            completed_migration_ids: vec!["023_reset_legacy_context_occupancy_projection"],
            endpoint_treatment: crate::ReleasedMigrationTreatment::MaterializeCurrent,
        };
        assert!(matches!(
            validate_released_ledger_prefix_fixture_case(&invalid),
            Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase { .. })
        ));
        let retired = crate::ReleasedLedgerPrefixFixtureCase {
            endpoint: "001_session_event_store_and_projections",
            completed_migration_ids: vec![
                "001_session_event_store_and_projections",
                "001_events_table",
            ],
            endpoint_treatment: crate::ReleasedMigrationTreatment::RetiredSuperseded,
        };
        assert!(matches!(
            validate_released_ledger_prefix_fixture_case(&retired),
            Err(MigrationLedgerValidationError::InvalidReleasedFixtureCase { .. })
        ));
    }

    #[test]
    fn source_storage_classification_owns_contract_and_writer_policy() {
        let partial = ValidatedMigrationLedger {
            completed_prefix_len: 2,
            current_migration_count: 3,
        };
        let current = ValidatedMigrationLedger {
            completed_prefix_len: 3,
            current_migration_count: 3,
        };
        assert_eq!(
            classify_source_storage(
                partial,
                StorageContractFacts {
                    table_exists: false,
                    contract: None,
                },
                1,
                2,
            ),
            Ok(SourceStorageCompatibility::ReleasedHistorical { writer_epoch: 2 })
        );
        assert_eq!(
            classify_source_storage(
                current,
                StorageContractFacts {
                    table_exists: true,
                    contract: Some(StorageContractRow {
                        schema_version: 1,
                        writer_epoch: u64::from(CURRENT_WRITER_EPOCH),
                    }),
                },
                1,
                2,
            ),
            Ok(SourceStorageCompatibility::Current {
                writer_epoch: u64::from(CURRENT_WRITER_EPOCH),
            })
        );
        assert_eq!(
            classify_source_storage(
                current,
                StorageContractFacts {
                    table_exists: false,
                    contract: None,
                },
                1,
                2,
            ),
            Err(SourceStorageCompatibilityError::MissingContractTable)
        );
        assert!(matches!(
            classify_source_storage(
                partial,
                StorageContractFacts {
                    table_exists: true,
                    contract: Some(StorageContractRow {
                        schema_version: 1,
                        writer_epoch: u64::from(CURRENT_WRITER_EPOCH + 1),
                    }),
                },
                1,
                2,
            ),
            Err(SourceStorageCompatibilityError::WriterEpoch { .. })
        ));
    }

    #[test]
    fn migration_ledger_validation_rejects_unknown_and_non_contiguous_history() {
        assert_eq!(
            completed_migration_ids([
                MigrationLedgerRow {
                    id: "001".to_owned(),
                    status: "completed".to_owned(),
                },
                MigrationLedgerRow {
                    id: "002".to_owned(),
                    status: "dirty".to_owned(),
                },
            ]),
            Err(MigrationLedgerValidationError::IncompleteMigration {
                id: "002".to_owned(),
                status: "dirty".to_owned(),
            })
        );
        let known = vec!["001".to_owned(), "002".to_owned(), "003".to_owned()];
        assert_eq!(
            validate_migration_ledger(&MigrationLedgerFacts {
                known_migration_ids: known.clone(),
                completed_migration_ids: BTreeSet::from(["001".to_owned(), "002".to_owned()]),
            })
            .expect("prefix"),
            ValidatedMigrationLedger {
                completed_prefix_len: 2,
                current_migration_count: 3,
            }
        );
        assert!(matches!(
            validate_migration_ledger(&MigrationLedgerFacts {
                known_migration_ids: known.clone(),
                completed_migration_ids: BTreeSet::from(["999".to_owned()]),
            }),
            Err(MigrationLedgerValidationError::UnknownMigration(id)) if id == "999"
        ));
        assert_eq!(
            validate_migration_ledger(&MigrationLedgerFacts {
                known_migration_ids: known,
                completed_migration_ids: BTreeSet::from(["001".to_owned(), "003".to_owned()]),
            }),
            Err(MigrationLedgerValidationError::NonContiguousPrefix)
        );
    }

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
