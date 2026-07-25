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
mod historical;
mod inventory;
mod operation;
mod planning;
mod service;
mod storage;
mod validation;

pub use backup::{
    BackupProgressCallback, MigrationBackupCanonicalEvidence, MigrationBackupError,
    MigrationBackupFileEvidence, MigrationBackupManifest, MigrationBackupRequest,
    RetainedMigrationBackup, RetainedMigrationBackupDiagnosis, VerifiedMigrationBackup,
    create_retained_migration_backup, create_verified_migration_backup,
    latest_retained_migration_backup,
};
pub use operation::{SessionMigrationOperation, SessionMigrationOperations};
pub use service::SessionMigrationService;
pub use storage::{
    HistoricalStorageDiagnosis, HistoricalStorageDiagnosisStatus, HistoricalStorageError,
    HistoricalStorageInspectionReport, HistoricalStorageRecoveryReport,
    HistoricalStorageRelocation, accidental_epoch_session_root,
    diagnose_accidental_epoch_session_root, inspect_accidental_epoch_session_root,
    recover_accidental_epoch_session_root,
};
pub use validation::SessionMigrationReceipt;

pub use classification::{HistoricalDecode, HistoricalEventMetadata};
pub use diagnosis::{
    SessionDiagnosisClassification, SessionDiagnosisCompatibility, classify_session_diagnosis,
};
pub use historical::{
    HistoricalSessionEventError, decode_for_migration, historical_conversion_counts,
    ordered_payload_digest,
};
pub use inventory::{
    CURRENT_EVENT_SCHEMA, CURRENT_WRITER_EPOCH, MIGRATION_STEPS, MigrationStepDescriptor,
    RELEASED_HISTORICAL_EVENT_SCHEMAS, RELEASED_HISTORICAL_WRITER_EPOCHS,
    is_released_historical_event_schema,
};
pub use planning::{
    MigrationPlan, MigrationPlanError, MigrationPlanService, plan_writer_epoch_migration,
    plan_writer_epoch_migration_with_registry,
};
