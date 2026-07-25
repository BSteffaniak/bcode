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
