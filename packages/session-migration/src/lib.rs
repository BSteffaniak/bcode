#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Historical session persistence decoding and migration planning.
//!
//! The current session runtime deliberately does not know historical durable
//! event shapes. This crate owns frozen readers for formats emitted by older
//! Bcode writers and converts them into the current session domain model.

mod backup;
mod classification;
mod codec;
mod diagnosis;
mod execution;
mod inventory;
mod operation;
mod planning;
mod relocation;
mod service;
mod storage;
mod target;
mod validation;

pub use backup::{
    BackupProgressCallback, MigrationBackupCanonicalEvidence, MigrationBackupError,
    MigrationBackupFileEvidence, MigrationBackupManifest, MigrationBackupRequest,
    MigrationBackupRequestError, MigrationBackupRequestPlan, MigrationSourceEvidence,
    RetainedMigrationBackup, RetainedMigrationBackupDiagnosis, VerifiedMigrationBackup,
    build_migration_backup_request, create_retained_migration_backup,
    create_verified_migration_backup, latest_retained_migration_backup,
};
pub use operation::{
    SessionMigrationOperation, SessionMigrationOperations, SessionMigrationProgressReporter,
};
pub use relocation::{
    RelocationStagingEntry, RelocationStagingReport, SessionRelocationBlock,
    SessionRelocationEntry, SessionRelocationError, SessionRelocationOwnership,
    SessionRelocationPlan, SessionRelocationReport, plan_session_relocation,
    prune_relocation_staging, relocate_sessions, session_artifact_dir,
};
pub use service::SessionMigrationService;
pub use storage::{
    HistoricalStorageDiagnosis, HistoricalStorageDiagnosisStatus, HistoricalStorageError,
    HistoricalStorageInspectionReport, HistoricalStorageRecoveryReport,
    HistoricalStorageRelocation, accidental_epoch_session_root,
    diagnose_accidental_epoch_session_root, inspect_accidental_epoch_session_root,
    recover_accidental_epoch_session_root,
};
pub use target::{
    MigrationTargetExecutionError, finalize_validated_target, validate_strict_target,
};
pub use validation::{
    MigrationClassificationEvidence, MigrationLedgerFacts, MigrationLedgerRow,
    MigrationLedgerValidationError, MigrationProjectionValidation, MigrationTargetValidation,
    MigrationTargetValidationError, SessionMigrationCanonicalReceiptEvidence,
    SessionMigrationReceipt, SessionMigrationReceiptRequest, SourceStorageCompatibility,
    SourceStorageCompatibilityError, StorageContractFacts, StorageContractRow,
    ValidatedMigrationLedger, WriterEpochCompatibility, WriterFinalizationError,
    build_session_migration_receipt, classify_source_storage, classify_target_storage,
    classify_writer_epoch, completed_migration_ids, validate_migration_ledger,
    validate_migration_target, validate_released_ledger_prefix_fixture_case,
    validate_writer_finalization,
};

pub use bcode_session_migration_target::{
    CURRENT_EVENT_SCHEMA as CURRENT_TARGET_EVENT_SCHEMA,
    CURRENT_WRITER_EPOCH as CURRENT_TARGET_WRITER_EPOCH, CurrentMigrationTargetCapability,
    current_migration_target_capabilities,
};

pub use classification::HistoricalEventMetadata;
pub use codec::HistoricalEnvelope;
pub use diagnosis::{
    SessionDiagnosisClassification, SessionDiagnosisCompatibility, SessionMigrationDiagnosis,
    SessionMigrationOwnerDiagnosis, classify_session_diagnosis,
};
pub use execution::{
    AuthoritativeMigrationState, CanonicalNormalizationSummary, HistoricalSessionEventError,
    NormalizedCanonicalEvent, build_target_receipt, metric, normalize_canonical_event,
    normalize_canonical_row, ordered_payload_digest,
};
pub use inventory::{
    CURRENT_EVENT_SCHEMA, CURRENT_WRITER_EPOCH, LATEST_HISTORICAL_EVENT_SCHEMA, MIGRATION_STEPS,
    MigrationStepDescriptor, NEVER_RELEASED_EVENT_SCHEMAS, RELEASED_EVENT_VARIANTS,
    RELEASED_HISTORICAL_EVENT_SCHEMAS, RELEASED_HISTORICAL_ROOTS,
    RELEASED_HISTORICAL_WRITER_EPOCHS, RELEASED_MIGRATION_IDS, RELEASED_PERSISTED_TABLES,
    RELEASED_RECORD_TREATMENTS, RELEASED_WRITER_SCHEMA_COMBINATIONS,
    ReleasedEventKindClassification, ReleasedEventTreatment, ReleasedEventVariantDescriptor,
    ReleasedFixtureClassificationCounts, ReleasedFixtureCoverageGaps, ReleasedFixtureDescriptor,
    ReleasedFixtureInventoryError, ReleasedFixtureManifest, ReleasedFixtureRootWriterPair,
    ReleasedFixtureSchemaEventPair, ReleasedFixtureTableTreatment, ReleasedFixtureWriterSchemaPair,
    ReleasedLedgerPrefixFixtureCase, ReleasedMigrationDescriptor, ReleasedMigrationDomain,
    ReleasedMigrationTreatment, ReleasedRecordDescriptor, ReleasedRecordTreatment,
    ReleasedRootDescriptor, ReleasedRootTreatment, ReleasedWriterSchemaDescriptor,
    classify_event_kind_schema, is_released_event_kind_schema, is_released_historical_event_schema,
    load_released_fixture_manifest, released_fixture_authoritative_record_coverage,
    released_fixture_coverage_gaps, released_fixture_schema_coverage,
    released_fixture_writer_coverage, released_session_ledger_prefix_fixture_cases,
};
pub use planning::{
    MigrationPlan, MigrationPlanError, MigrationPlanService, ReleasedEventTreatmentRow,
    ReleasedFixtureCoverageError, ReleasedFormatMigrationMatrixRow, ReleasedInventoryTreatments,
    ReleasedRecordTreatmentRow, ReleasedRootTreatmentRow, plan_writer_epoch_migration,
    plan_writer_epoch_migration_with_registry, released_format_migration_matrix,
    released_inventory_treatments, validate_released_fixture_coverage,
};
