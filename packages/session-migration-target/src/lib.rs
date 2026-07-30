#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Policy-free current session persistence capabilities used by historical migration.
//!
//! This package declares only operations implemented by the current session runtime. Historical
//! format inventory, source classification, migration planning, and orchestration do not belong
//! here.

use async_trait::async_trait;
use bcode_session_models::{SessionEvent, SessionId};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;

use std::sync::Arc;

/// Policy-owned canonical normalizer supplied to a current migration target.
pub type CanonicalNormalizer =
    Arc<dyn Fn(&CanonicalRow) -> Result<NormalizedCanonicalRow, String> + Send + Sync + 'static>;

/// Policy-free replay audit facts collected while materializing current storage.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReplayEvidence {
    /// Ordered source payload digest.
    pub source_payload_digest_sha256: String,
    /// Converted records keyed by migration-owned source identity.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known records keyed by migration-owned source identity.
    pub retired_known_events: BTreeMap<String, u64>,
}

/// Policy-free facts supplied to the migration-owned receipt builder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationReceiptFacts {
    /// Stable operation identity, when execution is operation-backed.
    pub operation_id: Option<String>,
    /// Migrated session identity.
    pub session_id: SessionId,
    /// Source writer epoch observed before migration.
    pub source_writer_epoch: u64,
    /// Canonical event count after current materialization.
    pub event_count: u64,
    /// Canonical tail after current materialization.
    pub event_tail: Option<u64>,
    /// Ordered target payload digest.
    pub target_payload_digest_sha256: String,
    /// Replay classification facts produced by the policy-owned normalizer.
    pub replay: ReplayEvidence,
    /// Completion timestamp.
    pub completed_at_ms: u64,
}

/// Migration-owned receipt construction supplied to a current target.
pub type MigrationReceiptBuilder =
    Arc<dyn Fn(MigrationReceiptFacts) -> Result<MigrationReceipt, String> + Send + Sync + 'static>;

/// Migration-owned callbacks consumed by policy-free current target execution.
#[derive(Clone)]
pub struct MigrationPolicyCallbacks {
    /// Historical/current canonical normalizer.
    pub normalize: CanonicalNormalizer,
    /// Durable receipt builder.
    pub build_receipt: MigrationReceiptBuilder,
}

impl std::fmt::Debug for MigrationPolicyCallbacks {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("MigrationPolicyCallbacks")
    }
}

/// One canonical row normalized by migration-owned policy for current-target ingestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NormalizedCanonicalRow {
    /// Strict current event.
    pub event: SessionEvent,
    /// Stable migration metric selected by the policy owner, when applicable.
    pub metric_counter: Option<String>,
    /// Historical source identity when conversion occurred.
    pub historical_source: Option<String>,
    /// Whether the historical source is retained as inert current history.
    pub retired_known: bool,
}

/// One durable canonical event row exposed by the current target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalRow {
    /// Canonical sequence identity.
    pub sequence: u64,
    /// Stable durable event-kind identity.
    pub event_kind: String,
    /// Durable event schema declared by the row.
    pub schema_version: u16,
    /// Complete durable JSON payload.
    pub payload: String,
}

/// One current projection validation fact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectionValidation {
    /// Stable projection identity.
    pub projection: String,
    /// Projection schema observed in current storage.
    pub actual_schema_version: Option<u64>,
    /// Current projection schema required by the target.
    pub expected_schema_version: u64,
    /// Last canonical sequence projected, when initialized.
    pub checkpoint: Option<u64>,
}

/// Strict current-format validation facts returned after target finalization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StrictValidation {
    /// Canonical target tail, when history is non-empty.
    pub canonical_tail: Option<u64>,
    /// Current materialized projection facts.
    pub projections: Vec<ProjectionValidation>,
    /// Model-context projection schema, when initialized.
    pub model_context_schema_version: Option<u64>,
    /// Current model-context projection schema.
    pub expected_model_context_schema_version: u64,
    /// Model-context checkpoint, when initialized.
    pub model_context_checkpoint: Option<u64>,
}

/// Migration-owned audit receipt data accepted by the current target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MigrationReceipt {
    /// Stable migration operation identity.
    pub operation_id: String,
    /// Migrated session identity.
    pub session_id: SessionId,
    /// Released writer epoch observed before migration.
    pub source_writer_epoch: u32,
    /// Current writer epoch produced by migration.
    pub target_writer_epoch: u32,
    /// Ordered migration-step identities.
    pub migration_step_ids: Vec<String>,
    /// Canonical source event count.
    pub source_event_count: u64,
    /// Canonical source tail.
    pub source_event_tail: Option<u64>,
    /// Stable ordered source payload digest.
    #[serde(rename = "source_event_digest_sha256")]
    pub source_payload_digest_sha256: String,
    /// Canonical target event count.
    pub target_event_count: u64,
    /// Canonical target tail.
    pub target_event_tail: Option<u64>,
    /// Stable ordered target payload digest.
    #[serde(rename = "target_event_digest_sha256")]
    pub target_payload_digest_sha256: String,
    /// Converted historical event counts by schema/kind.
    pub converted_events: BTreeMap<String, u64>,
    /// Retired-known event counts by schema/kind.
    pub retired_known_events: BTreeMap<String, u64>,
    /// Completion timestamp in Unix milliseconds.
    pub completed_at_ms: u64,
}

/// Policy-free current persistence operations used by historical migration.
///
/// Implementations must execute these operations against one exclusively owned migration
/// transaction. This interface contains no released format, epoch-plan, backup, or source
/// classification policy.
#[async_trait]
pub trait MigrationTarget: Send {
    /// Implementation-specific failure.
    type Error: Error + Send + Sync + 'static;

    /// Materialize the complete current SQL schema in the migration transaction.
    async fn materialize_current_schema(&mut self) -> Result<(), Self::Error>;

    /// Return a bounded canonical page starting at `start_sequence`.
    async fn canonical_page(
        &mut self,
        start_sequence: u64,
        limit: usize,
    ) -> Result<Vec<CanonicalRow>, Self::Error>;

    /// Replace one canonical row with its normalized current representation.
    async fn replace_canonical_row(&mut self, row: CanonicalRow) -> Result<(), Self::Error>;

    /// Write migration-owned authoritative current state.
    async fn write_authoritative_state(
        &mut self,
        context_epoch: u64,
        context_occupancy_json: Option<String>,
    ) -> Result<(), Self::Error>;

    /// Ingest one normalized current event into all current projectors.
    async fn ingest_projectors(&mut self, event: &SessionEvent) -> Result<(), Self::Error>;

    /// Finalize current projectors at the canonical tail.
    async fn finalize_projectors(&mut self, canonical_tail: Option<u64>)
    -> Result<(), Self::Error>;

    /// Return strict current validation facts after finalization.
    async fn validate_strict_current(&mut self) -> Result<StrictValidation, Self::Error>;

    /// Persist one complete durable migration receipt.
    async fn persist_migration_receipt(
        &mut self,
        receipt: &MigrationReceipt,
    ) -> Result<(), Self::Error>;

    /// Finalize the current writer contract after strict validation.
    async fn finalize_writer_contract(&mut self) -> Result<(), Self::Error>;
}

/// Policy-free source storage classification exposed by the current target boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StorageCompatibility {
    /// The store implements the complete current contract.
    Current { writer_epoch: u64 },
    /// The store is non-current and requires migration orchestration.
    MigrationRequired { writer_epoch: u64 },
}

/// One raw durable current migration-ledger row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationLedgerRow {
    /// Stable migration identity.
    pub id: String,
    /// Durable migration status.
    pub status: String,
}

/// Raw current-ledger and storage-contract facts collected without historical policy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StorageCompatibilityFacts {
    /// Ordered migration identifiers in the current target schema.
    pub current_migration_ids: Vec<String>,
    /// Durable source migration rows.
    pub migration_rows: Vec<MigrationLedgerRow>,
    /// Whether the current contract table exists.
    pub contract_table_exists: bool,
    /// Current contract schema, when a row exists.
    pub contract_schema_version: Option<u64>,
    /// Durable source writer epoch, when a row exists.
    pub writer_epoch: Option<u64>,
    /// Current contract schema required by the target.
    pub expected_contract_schema_version: u64,
    /// Writer epoch assigned when a released source predates the contract row.
    pub legacy_writer_epoch: u32,
}

/// Failure returned by migration-owned storage compatibility classification.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum StorageCompatibilityError {
    /// A source ledger row is not complete.
    #[error("source migration ledger contains an incomplete row")]
    IncompleteLedger,
    /// The writer epoch is unsupported by this build.
    #[error("unsupported session writer epoch {actual}; expected {expected}")]
    WriterEpoch {
        /// Observed durable writer epoch.
        actual: u64,
        /// Current writer epoch required by this build.
        expected: u64,
    },
    /// Migration-owned classification rejected the raw facts.
    #[error("source storage classification failed: {0}")]
    Classification(String),
}

/// Classify raw facts against the exact current target contract.
///
/// This performs no released-format interpretation: an exact current ledger and contract are
/// current, a clean known prefix is non-current, and unknown, dirty, or inconsistent facts fail.
///
/// # Errors
///
/// Returns an error for dirty/unknown ledgers, inconsistent contracts, or future writer epochs.
pub fn classify_storage_facts(
    facts: &StorageCompatibilityFacts,
) -> Result<StorageCompatibility, StorageCompatibilityError> {
    let mut completed = Vec::with_capacity(facts.migration_rows.len());
    for row in &facts.migration_rows {
        if row.status != "completed" {
            return Err(StorageCompatibilityError::IncompleteLedger);
        }
        completed.push(row.id.as_str());
    }
    if completed.len() > facts.current_migration_ids.len()
        || !completed
            .iter()
            .zip(&facts.current_migration_ids)
            .all(|(actual, expected)| *actual == expected)
    {
        return Err(StorageCompatibilityError::Classification(
            "source migration ledger is not a known current-schema prefix".to_owned(),
        ));
    }
    let expected_writer = u64::from(CURRENT_WRITER_EPOCH);
    if facts.contract_table_exists {
        let schema = facts.contract_schema_version.ok_or_else(|| {
            StorageCompatibilityError::Classification(
                "storage contract table has no contract row".to_owned(),
            )
        })?;
        let writer = facts.writer_epoch.ok_or_else(|| {
            StorageCompatibilityError::Classification(
                "storage contract row has no writer epoch".to_owned(),
            )
        })?;
        if schema != facts.expected_contract_schema_version {
            return Err(StorageCompatibilityError::Classification(format!(
                "unsupported storage contract schema {schema}"
            )));
        }
        if writer > expected_writer {
            return Err(StorageCompatibilityError::WriterEpoch {
                actual: writer,
                expected: expected_writer,
            });
        }
        if completed.len() == facts.current_migration_ids.len() && writer == expected_writer {
            return Ok(StorageCompatibility::Current {
                writer_epoch: writer,
            });
        }
        return Ok(StorageCompatibility::MigrationRequired {
            writer_epoch: writer,
        });
    }
    if facts.writer_epoch.is_some() || facts.contract_schema_version.is_some() {
        return Err(StorageCompatibilityError::Classification(
            "storage contract facts are inconsistent".to_owned(),
        ));
    }
    if completed.len() == facts.current_migration_ids.len() {
        return Err(StorageCompatibilityError::Classification(
            "current migration ledger is missing its required storage contract".to_owned(),
        ));
    }
    Ok(StorageCompatibility::MigrationRequired {
        writer_epoch: u64::from(facts.legacy_writer_epoch),
    })
}

/// Complete current migration-target capabilities required by historical migration.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum CurrentMigrationTargetCapability {
    /// Materialize the complete current SQL schema.
    MaterializeCurrentSchema,
    /// Read canonical rows in bounded sequence pages.
    ReadBoundedCanonicalPage,
    /// Replace one canonical row with its current encoding.
    ReplaceCanonicalRow,
    /// Write authoritative current state derived from canonical history.
    WriteAuthoritativeState,
    /// Ingest canonical events into current projectors.
    IngestProjectors,
    /// Finalize current projector checkpoints and authoritative state.
    FinalizeProjectors,
    /// Strictly validate canonical, projection, compatibility, and write readiness.
    ValidateStrictCurrent,
    /// Persist the durable migration receipt.
    PersistMigrationReceipt,
    /// Finalize the current writer contract after validation.
    FinalizeWriterContract,
}

/// Return the exact current migration-target capability surface.
#[must_use]
pub fn current_migration_target_capabilities() -> BTreeSet<CurrentMigrationTargetCapability> {
    BTreeSet::from([
        CurrentMigrationTargetCapability::MaterializeCurrentSchema,
        CurrentMigrationTargetCapability::ReadBoundedCanonicalPage,
        CurrentMigrationTargetCapability::ReplaceCanonicalRow,
        CurrentMigrationTargetCapability::WriteAuthoritativeState,
        CurrentMigrationTargetCapability::IngestProjectors,
        CurrentMigrationTargetCapability::FinalizeProjectors,
        CurrentMigrationTargetCapability::ValidateStrictCurrent,
        CurrentMigrationTargetCapability::PersistMigrationReceipt,
        CurrentMigrationTargetCapability::FinalizeWriterContract,
    ])
}

/// Current event schema accepted and emitted by the target runtime.
pub const CURRENT_EVENT_SCHEMA: u16 = bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION;

/// Current durable writer epoch finalized by the target runtime.
pub const CURRENT_WRITER_EPOCH: u32 = bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_surface_is_exact_and_current_contracts_match_models() {
        assert_eq!(current_migration_target_capabilities().len(), 9);
        assert_eq!(
            CURRENT_EVENT_SCHEMA,
            bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
        );
        assert_eq!(
            CURRENT_WRITER_EPOCH,
            bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH
        );
    }

    #[test]
    fn target_api_models_are_policy_free_current_capabilities() {
        let row = CanonicalRow {
            sequence: 7,
            event_kind: "assistant_message".to_owned(),
            schema_version: CURRENT_EVENT_SCHEMA,
            payload: "{}".to_owned(),
        };
        assert_eq!(row.sequence, 7);
        let validation = StrictValidation {
            canonical_tail: Some(7),
            projections: vec![ProjectionValidation {
                projection: "session_state".to_owned(),
                actual_schema_version: Some(1),
                expected_schema_version: 1,
                checkpoint: Some(7),
            }],
            model_context_schema_version: Some(2),
            expected_model_context_schema_version: 2,
            model_context_checkpoint: Some(7),
        };
        assert_eq!(validation.canonical_tail, Some(7));
    }
}
