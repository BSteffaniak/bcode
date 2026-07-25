#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Historical session persistence decoding and migration planning.
//!
//! The current session runtime deliberately does not know historical durable
//! event shapes. This crate owns frozen readers for formats emitted by older
//! Bcode writers and converts them into the current session domain model.

mod audit;
mod backup;
mod historical;
mod operation;
mod registry;
mod service;
mod storage;

pub use audit::SessionMigrationReceipt;
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

pub use historical::{
    HistoricalDecode, HistoricalEventMetadata, HistoricalSessionEventError, decode_for_migration,
    historical_conversion_counts, ordered_payload_digest,
};
pub use registry::{
    CURRENT_WRITER_EPOCH, MigrationPlan, MigrationPlanError, MigrationPlanService,
    MigrationStepDescriptor, plan_writer_epoch_migration,
    plan_writer_epoch_migration_with_registry,
};
