//! Released session-format inventory and migration-edge declarations.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use thiserror::Error;

/// Manifest of permanent sanitized released-format fixtures.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedFixtureManifest {
    /// Manifest schema version.
    pub format_version: u32,
    /// Exhaustive fixture declarations.
    pub fixtures: Vec<ReleasedFixtureDescriptor>,
}

/// One permanent sanitized fixture and its required migration coverage.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedFixtureDescriptor {
    /// Path relative to the fixture root.
    pub path: PathBuf,
    /// Released source writer epochs represented by this fixture.
    pub source_writer_epochs: Vec<u32>,
    /// Event schemas represented by this fixture.
    pub event_schemas: Vec<u16>,
    /// Exact canonical event count.
    pub expected_event_count: usize,
    /// Exact migration classifications expected from the fixture.
    pub expected_classifications: ReleasedFixtureClassificationCounts,
    /// Exact event-kind inventory represented by the fixture.
    pub covered_event_kinds: Vec<String>,
    /// Preserved authoritative non-event records covered by this fixture.
    #[serde(default)]
    pub covered_authoritative_records: Vec<String>,
}

/// Expected normalization treatment counts for a fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedFixtureClassificationCounts {
    /// Explicit semantic conversions.
    pub converted: usize,
    /// Recognized inert current history.
    pub retired_known: usize,
    /// Payloads accepted by the strict current decoder.
    pub current_passthrough: usize,
}

/// Failure to validate the permanent released-format fixture inventory.
#[derive(Debug, Error)]
pub enum ReleasedFixtureInventoryError {
    /// Manifest JSON is malformed.
    #[error("released fixture manifest is malformed: {0}")]
    Json(#[from] serde_json::Error),
    /// Filesystem inventory failed.
    #[error("released fixture inventory I/O failed: {0}")]
    Io(#[from] std::io::Error),
    /// Manifest schema is unsupported.
    #[error("unsupported released fixture manifest version {0}")]
    UnsupportedManifestVersion(u32),
    /// The manifest is empty.
    #[error("released fixture manifest is empty")]
    EmptyManifest,
    /// A fixture path is duplicated.
    #[error("duplicate released fixture path {}", .0.display())]
    DuplicatePath(PathBuf),
    /// Manifest and fixture-directory paths differ.
    #[error("released fixture manifest paths do not match fixture directory")]
    PathInventoryMismatch,
    /// A fixture declares no source writer epochs.
    #[error("released fixture {} declares no source writer epochs", .0.display())]
    MissingWriterEpoch(PathBuf),
    /// A fixture references an unreleased writer epoch.
    #[error("released fixture {} references unsupported writer epoch {writer_epoch}", path.display())]
    UnsupportedWriterEpoch {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported writer epoch.
        writer_epoch: u32,
    },
    /// A fixture references an unknown event schema.
    #[error("released fixture {} references unknown event schema {event_schema}", path.display())]
    UnsupportedEventSchema {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported event schema.
        event_schema: u16,
    },
    /// A fixture contains no canonical event schemas.
    #[error("released fixture {} declares no event schemas", .0.display())]
    MissingEventSchema(PathBuf),
    /// A fixture contains no event-kind coverage.
    #[error("released fixture {} declares no event kinds", .0.display())]
    MissingEventKind(PathBuf),
    /// A fixture declares a duplicate writer epoch, schema, event kind, or authoritative record.
    #[error("released fixture {} contains duplicate {field} value {value}", path.display())]
    DuplicateCoverageValue {
        /// Fixture path.
        path: PathBuf,
        /// Manifest field containing the duplicate.
        field: &'static str,
        /// Duplicate rendered value.
        value: String,
    },
    #[error("released fixture {} references unsupported authoritative record {record}", path.display())]
    UnsupportedAuthoritativeRecord {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported record table.
        record: String,
    },
}

fn validate_fixture_values_unique(
    path: &Path,
    field: &'static str,
    values: impl IntoIterator<Item = String>,
) -> Result<(), ReleasedFixtureInventoryError> {
    let mut unique = BTreeSet::new();
    for value in values {
        if !unique.insert(value.clone()) {
            return Err(ReleasedFixtureInventoryError::DuplicateCoverageValue {
                path: path.to_path_buf(),
                field,
                value,
            });
        }
    }
    Ok(())
}

/// Load and validate the permanent fixture manifest against released inventory and disk paths.
///
/// # Errors
///
/// Returns an error for malformed manifests, missing/extra fixture files, duplicate paths, or
/// references to unknown writer epochs or event schemas.
pub fn load_released_fixture_manifest(
    fixture_root: &Path,
) -> Result<ReleasedFixtureManifest, ReleasedFixtureInventoryError> {
    let manifest = serde_json::from_slice::<ReleasedFixtureManifest>(&std::fs::read(
        fixture_root.join("manifest.json"),
    )?)?;
    if manifest.format_version != 1 {
        return Err(ReleasedFixtureInventoryError::UnsupportedManifestVersion(
            manifest.format_version,
        ));
    }
    if manifest.fixtures.is_empty() {
        return Err(ReleasedFixtureInventoryError::EmptyManifest);
    }
    let mut listed_paths = BTreeSet::new();
    for fixture in &manifest.fixtures {
        if !listed_paths.insert(fixture.path.clone()) {
            return Err(ReleasedFixtureInventoryError::DuplicatePath(
                fixture.path.clone(),
            ));
        }
        if fixture.source_writer_epochs.is_empty() {
            return Err(ReleasedFixtureInventoryError::MissingWriterEpoch(
                fixture.path.clone(),
            ));
        }
        if fixture.event_schemas.is_empty() {
            return Err(ReleasedFixtureInventoryError::MissingEventSchema(
                fixture.path.clone(),
            ));
        }
        if fixture.covered_event_kinds.is_empty() {
            return Err(ReleasedFixtureInventoryError::MissingEventKind(
                fixture.path.clone(),
            ));
        }
        validate_fixture_values_unique(
            &fixture.path,
            "source_writer_epochs",
            fixture.source_writer_epochs.iter().map(ToString::to_string),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "event_schemas",
            fixture.event_schemas.iter().map(ToString::to_string),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_event_kinds",
            fixture.covered_event_kinds.iter().cloned(),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_authoritative_records",
            fixture.covered_authoritative_records.iter().cloned(),
        )?;
        for writer_epoch in &fixture.source_writer_epochs {
            if RELEASED_HISTORICAL_WRITER_EPOCHS
                .binary_search(writer_epoch)
                .is_err()
            {
                return Err(ReleasedFixtureInventoryError::UnsupportedWriterEpoch {
                    path: fixture.path.clone(),
                    writer_epoch: *writer_epoch,
                });
            }
        }
        for record in &fixture.covered_authoritative_records {
            if !RELEASED_RECORD_TREATMENTS.iter().any(|descriptor| {
                descriptor.table == record
                    && descriptor.treatment == ReleasedRecordTreatment::Preserve
            }) {
                return Err(
                    ReleasedFixtureInventoryError::UnsupportedAuthoritativeRecord {
                        path: fixture.path.clone(),
                        record: record.clone(),
                    },
                );
            }
        }
        for event_schema in &fixture.event_schemas {
            if *event_schema != CURRENT_EVENT_SCHEMA
                && !is_released_historical_event_schema(*event_schema)
            {
                return Err(ReleasedFixtureInventoryError::UnsupportedEventSchema {
                    path: fixture.path.clone(),
                    event_schema: *event_schema,
                });
            }
        }
    }
    let actual_paths = std::fs::read_dir(fixture_root.join("stores"))?
        .map(|entry| entry.map(|entry| PathBuf::from("stores").join(entry.file_name())))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if listed_paths != actual_paths {
        return Err(ReleasedFixtureInventoryError::PathInventoryMismatch);
    }
    Ok(manifest)
}

/// Return fixture coverage by released writer epoch.
#[must_use]
pub fn released_fixture_writer_coverage(
    manifest: &ReleasedFixtureManifest,
) -> BTreeMap<u32, usize> {
    let mut coverage = BTreeMap::new();
    for fixture in &manifest.fixtures {
        for writer_epoch in &fixture.source_writer_epochs {
            *coverage.entry(*writer_epoch).or_insert(0) += 1;
        }
    }
    coverage
}

/// Return fixture coverage by event schema.
#[must_use]
pub fn released_fixture_schema_coverage(
    manifest: &ReleasedFixtureManifest,
) -> BTreeMap<u16, usize> {
    let mut coverage = BTreeMap::new();
    for fixture in &manifest.fixtures {
        for event_schema in &fixture.event_schemas {
            *coverage.entry(*event_schema).or_insert(0) += 1;
        }
    }
    coverage
}

/// Return preserved authoritative-record fixture coverage.
#[must_use]
pub fn released_fixture_authoritative_record_coverage(
    manifest: &ReleasedFixtureManifest,
) -> BTreeMap<String, usize> {
    let mut coverage = BTreeMap::new();
    for fixture in &manifest.fixtures {
        for record in &fixture.covered_authoritative_records {
            *coverage.entry(record.clone()).or_insert(0) += 1;
        }
    }
    coverage
}

/// Migration treatment for one authoritative historical non-event record.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedRecordTreatment {
    /// Rebuild the row from normalized canonical history.
    RebuildFromCanonical,
    /// Preserve the row exactly across migration.
    Preserve,
    /// Replace the row with migration-owned current contract/audit state.
    FinalizeCurrent,
    /// Global-domain state is outside per-session migration.
    GlobalOnly,
    /// Historical derived state is intentionally discarded and rebuilt elsewhere.
    RetireDerived,
}

/// One authoritative or derived non-event record found in released storage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedRecordDescriptor {
    /// Durable table identity.
    pub table: &'static str,
    /// Migration treatment for the table's rows.
    pub treatment: ReleasedRecordTreatment,
}

/// Complete treatment inventory for released persisted tables.
pub const RELEASED_RECORD_TREATMENTS: &[ReleasedRecordDescriptor] = &[
    ReleasedRecordDescriptor {
        table: "artifact_references",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "composer_drafts",
        treatment: ReleasedRecordTreatment::GlobalOnly,
    },
    ReleasedRecordDescriptor {
        table: "context_occupancy_projection",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "events",
        treatment: ReleasedRecordTreatment::Preserve,
    },
    ReleasedRecordDescriptor {
        table: "input_messages",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "model_context_entries",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "model_context_projection_state",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "projection_checkpoints",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "runtime_work",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "session_compatibility_issues",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "session_compatibility_state",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "session_drafts",
        treatment: ReleasedRecordTreatment::Preserve,
    },
    ReleasedRecordDescriptor {
        table: "session_migration_receipts",
        treatment: ReleasedRecordTreatment::FinalizeCurrent,
    },
    ReleasedRecordDescriptor {
        table: "session_state",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "session_storage_contract",
        treatment: ReleasedRecordTreatment::FinalizeCurrent,
    },
    ReleasedRecordDescriptor {
        table: "sessions",
        treatment: ReleasedRecordTreatment::GlobalOnly,
    },
    ReleasedRecordDescriptor {
        table: "snapshots",
        treatment: ReleasedRecordTreatment::RetireDerived,
    },
    ReleasedRecordDescriptor {
        table: "tool_runs",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "transcript_items",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
    ReleasedRecordDescriptor {
        table: "turn_receipts",
        treatment: ReleasedRecordTreatment::RebuildFromCanonical,
    },
];

/// Treatment required for one released persisted event variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedEventTreatment {
    /// Strict current decoding preserves active semantics.
    CurrentEquivalent,
    /// Migration must explicitly convert historical semantics.
    ExplicitConversion,
    /// Migration preserves the event as recognized inert current history.
    RetiredKnown,
}

/// One released persisted event variant and its migration treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedEventVariantDescriptor {
    /// Stable serde event-kind name.
    pub kind: &'static str,
    /// Required migration treatment.
    pub treatment: ReleasedEventTreatment,
}

/// Persisted event variants represented by the current schema and historical adapters.
///
/// Historical schema-28 variants are listed with explicit conversion/inert treatment. All other
/// variants are current-equivalent and remain subject to per-schema fixture completion.
pub const RELEASED_EVENT_VARIANTS: &[ReleasedEventVariantDescriptor] = &[
    ReleasedEventVariantDescriptor {
        kind: "agent_changed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "assistant_delta",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "assistant_message",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "assistant_reasoning_activity",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "assistant_reasoning_delta",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "assistant_reasoning_message",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "client_attached",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "client_detached",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "context_compacted",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "context_usage_observed",
        treatment: ReleasedEventTreatment::ExplicitConversion,
    },
    ReleasedEventVariantDescriptor {
        kind: "execution_session_created",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "model_changed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "model_turn_cancel_requested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "model_turn_finished",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "model_turn_started",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "model_usage",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "opaque_event",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "permission_requested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "permission_resolved",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "plugin_status_note",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "provider_context_compacted",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "ralph_lifecycle",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "reasoning_changed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "request_context_observed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "runtime_work_cancel_requested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "runtime_work_finished",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "runtime_work_progress",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "runtime_work_started",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "session_created",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "session_forked",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "session_imported",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "session_renamed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_activated",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_context_loaded",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_deactivated",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_invocation_failed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_invoked",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "skill_suggested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "system_message",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_call_finished",
        treatment: ReleasedEventTreatment::ExplicitConversion,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_call_requested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_contribution",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_contribution_placed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_exchange_requested",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_exchange_resolved",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_invocation_lifecycle",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_invocation_result_recorded",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "tool_invocation_stream",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "trace_event",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "user_message",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
    ReleasedEventVariantDescriptor {
        kind: "working_directory_changed",
        treatment: ReleasedEventTreatment::CurrentEquivalent,
    },
];

/// Migration-ledger domain that owns one released migration identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedMigrationDomain {
    /// Per-session canonical and projection storage.
    Session,
    /// Global catalog/composer storage, not a per-session migration input.
    Global,
}

/// One migration identity observed in released Git history.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedMigrationDescriptor {
    /// Stable durable migration identity.
    pub id: &'static str,
    /// Storage domain that owns the ledger entry.
    pub domain: ReleasedMigrationDomain,
    /// Whether this identity is still present in the current schema materializer.
    pub current: bool,
}

/// Complete released migration-ID inventory observed across Git history.
pub const RELEASED_MIGRATION_IDS: &[ReleasedMigrationDescriptor] = &[
    ReleasedMigrationDescriptor {
        id: "001_events_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "001_global_catalog",
        domain: ReleasedMigrationDomain::Global,
        current: false,
    },
    ReleasedMigrationDescriptor {
        id: "001_global_sessions_table",
        domain: ReleasedMigrationDomain::Global,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "001_session_event_store_and_projections",
        domain: ReleasedMigrationDomain::Session,
        current: false,
    },
    ReleasedMigrationDescriptor {
        id: "002_events_event_type_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "002_global_sessions_updated_at_index",
        domain: ReleasedMigrationDomain::Global,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "003_global_composer_drafts_table",
        domain: ReleasedMigrationDomain::Global,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "003_session_state_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "004_input_messages_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "005_input_messages_event_seq_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "006_transcript_items_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "007_transcript_items_event_range_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "008_tool_runs_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "009_tool_runs_status_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "010_projection_checkpoints_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "011_snapshots_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "012_runtime_work_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "013_runtime_work_status_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "014_runtime_work_parent_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "015_session_drafts_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "016_session_state_reasoning_effort_column",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "017_session_state_reasoning_summary_column",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "018_model_context_projection_state_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "019_model_context_entries_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "020_model_context_entries_event_type_index",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "021_artifact_references_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "022_context_occupancy_projection_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "023_reset_legacy_context_occupancy_projection",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "024_reset_request_context_occupancy_projection",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "025_turn_receipts_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "026_session_storage_contract_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "027_initialize_session_storage_contract",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "028_repair_context_occupancy_checkpoint_version",
        domain: ReleasedMigrationDomain::Session,
        current: false,
    },
    ReleasedMigrationDescriptor {
        id: "028_session_compatibility_state",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "029_session_compatibility_issues",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "030_session_state_visibility_column",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "031_session_state_execution_provenance_column",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
    ReleasedMigrationDescriptor {
        id: "032_session_migration_receipts_table",
        domain: ReleasedMigrationDomain::Session,
        current: true,
    },
];

/// Historical tables observed in released session/catalog migrations.
pub const RELEASED_PERSISTED_TABLES: &[&str] = &[
    "artifact_references",
    "composer_drafts",
    "context_occupancy_projection",
    "events",
    "input_messages",
    "model_context_entries",
    "model_context_projection_state",
    "projection_checkpoints",
    "runtime_work",
    "session_compatibility_issues",
    "session_compatibility_state",
    "session_drafts",
    "session_migration_receipts",
    "session_state",
    "session_storage_contract",
    "sessions",
    "snapshots",
    "tool_runs",
    "transcript_items",
    "turn_receipts",
];

/// Writer epoch produced by the corrected migration contract.
pub const CURRENT_WRITER_EPOCH: u32 = 5;

/// One monotonic writer-contract transition supported by this build.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationStepDescriptor {
    /// Stable audit identity for the transition.
    pub id: &'static str,
    /// Writer epoch accepted by the step.
    pub source_writer_epoch: u32,
    /// Writer epoch produced by the step.
    pub target_writer_epoch: u32,
}

pub const MIGRATION_STEPS: [MigrationStepDescriptor; 4] = [
    MigrationStepDescriptor {
        id: "session-writer-epoch-1-to-2",
        source_writer_epoch: 1,
        target_writer_epoch: 2,
    },
    MigrationStepDescriptor {
        id: "session-writer-epoch-2-to-3",
        source_writer_epoch: 2,
        target_writer_epoch: 3,
    },
    MigrationStepDescriptor {
        id: "session-writer-epoch-3-to-4",
        source_writer_epoch: 3,
        target_writer_epoch: 4,
    },
    MigrationStepDescriptor {
        id: "session-writer-epoch-4-to-5",
        source_writer_epoch: 4,
        target_writer_epoch: 5,
    },
];

/// Released historical writer epochs that must migrate to [`CURRENT_WRITER_EPOCH`].
pub const RELEASED_HISTORICAL_WRITER_EPOCHS: &[u32] = &[1, 2, 3, 4];

/// Released historical event schemas currently evidenced by Git history.
///
/// Schemas 33, 34, and 36 were never declared. Schema 40 is the current format and therefore is
/// not historical migration input.
pub const RELEASED_HISTORICAL_EVENT_SCHEMAS: &[u16] = &[
    1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26,
    27, 28, 29, 30, 31, 32, 35, 37, 38, 39,
];

/// Current event schema emitted by this build.
pub const CURRENT_EVENT_SCHEMA: u16 = 40;

/// Return whether a schema is evidenced as a released historical format.
#[must_use]
pub fn is_released_historical_event_schema(schema: u16) -> bool {
    RELEASED_HISTORICAL_EVENT_SCHEMAS
        .binary_search(&schema)
        .is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_released_table_has_exactly_one_record_treatment() {
        assert_eq!(
            RELEASED_RECORD_TREATMENTS.len(),
            RELEASED_PERSISTED_TABLES.len()
        );
        assert!(
            RELEASED_RECORD_TREATMENTS
                .windows(2)
                .all(|pair| pair[0].table < pair[1].table)
        );
        assert_eq!(
            RELEASED_RECORD_TREATMENTS
                .iter()
                .map(|record| record.table)
                .collect::<Vec<_>>(),
            RELEASED_PERSISTED_TABLES
        );
        assert_eq!(
            RELEASED_RECORD_TREATMENTS
                .iter()
                .filter(|record| record.treatment == ReleasedRecordTreatment::Preserve)
                .map(|record| record.table)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["events", "session_drafts"])
        );
    }

    #[test]
    fn released_event_variant_treatments_are_sorted_unique_and_total() {
        assert!(
            RELEASED_EVENT_VARIANTS
                .windows(2)
                .all(|pair| pair[0].kind < pair[1].kind)
        );
        assert_eq!(RELEASED_EVENT_VARIANTS.len(), 51);
        let explicit = RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|variant| variant.treatment == ReleasedEventTreatment::ExplicitConversion)
            .map(|variant| variant.kind)
            .collect::<BTreeSet<_>>();
        assert_eq!(
            explicit,
            BTreeSet::from(["context_usage_observed", "tool_call_finished"])
        );
        let retired = RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|variant| variant.treatment == ReleasedEventTreatment::RetiredKnown)
            .map(|variant| variant.kind)
            .collect::<Vec<_>>();
        assert_eq!(retired, ["tool_invocation_stream"]);
    }

    #[test]
    fn released_migration_and_table_inventories_are_sorted_unique_and_domain_complete() {
        assert_eq!(RELEASED_MIGRATION_IDS.len(), 38);
        assert!(
            RELEASED_MIGRATION_IDS
                .windows(2)
                .all(|pair| pair[0].id < pair[1].id)
        );
        assert_eq!(RELEASED_PERSISTED_TABLES.len(), 20);
        assert!(RELEASED_PERSISTED_TABLES.is_sorted());
        assert!(
            RELEASED_PERSISTED_TABLES
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(
            RELEASED_MIGRATION_IDS
                .iter()
                .filter(|migration| migration.domain == ReleasedMigrationDomain::Session)
                .count(),
            34
        );
        assert_eq!(
            RELEASED_MIGRATION_IDS
                .iter()
                .filter(|migration| !migration.current)
                .map(|migration| migration.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "001_global_catalog",
                "001_session_event_store_and_projections",
                "028_repair_context_occupancy_checkpoint_version",
            ])
        );
    }

    #[test]
    fn permanent_fixture_manifest_is_exhaustive_and_inventory_valid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = load_released_fixture_manifest(&root).expect("fixture inventory");
        assert_eq!(manifest.format_version, 1);
        assert_eq!(
            released_fixture_writer_coverage(&manifest).get(&2),
            Some(&1)
        );
        assert_eq!(
            released_fixture_schema_coverage(&manifest).get(&28),
            Some(&1)
        );
        assert_eq!(
            released_fixture_authoritative_record_coverage(&manifest).get("session_drafts"),
            Some(&1)
        );
    }

    #[test]
    fn permanent_fixture_manifest_rejects_duplicate_and_empty_coverage() {
        let root = tempfile::tempdir().expect("fixture root");
        std::fs::create_dir(root.path().join("stores")).expect("stores");
        std::fs::write(root.path().join("stores/fixture.jsonl"), "{}\n").expect("fixture");
        let manifest_path = root.path().join("manifest.json");
        let manifest = serde_json::json!({
            "format_version": 1,
            "fixtures": [{
                "path": "stores/fixture.jsonl",
                "source_writer_epochs": [2, 2],
                "event_schemas": [28],
                "expected_event_count": 1,
                "expected_classifications": {
                    "converted": 0,
                    "retired_known": 0,
                    "current_passthrough": 1
                },
                "covered_event_kinds": ["session_created"]
            }]
        });
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::DuplicateCoverageValue {
                field: "source_writer_epochs",
                ..
            })
        ));

        let mut manifest = manifest;
        manifest["fixtures"][0]["source_writer_epochs"] = serde_json::json!([2]);
        manifest["fixtures"][0]["event_schemas"] = serde_json::json!([]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::MissingEventSchema(_))
        ));
    }

    #[test]
    fn released_schema_inventory_is_sorted_unique_and_excludes_unreleased_gaps() {
        assert!(RELEASED_HISTORICAL_EVENT_SCHEMAS.is_sorted());
        assert!(
            RELEASED_HISTORICAL_EVENT_SCHEMAS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        for unreleased in [33, 34, 36, CURRENT_EVENT_SCHEMA] {
            assert!(!is_released_historical_event_schema(unreleased));
        }
        for released in RELEASED_HISTORICAL_EVENT_SCHEMAS {
            assert!(is_released_historical_event_schema(*released));
        }
    }

    #[test]
    fn every_released_writer_epoch_has_exactly_one_outgoing_edge() {
        for source in RELEASED_HISTORICAL_WRITER_EPOCHS {
            let edges = MIGRATION_STEPS
                .iter()
                .filter(|step| step.source_writer_epoch == *source)
                .count();
            assert_eq!(edges, 1, "writer epoch {source} must have one edge");
        }
    }
}
