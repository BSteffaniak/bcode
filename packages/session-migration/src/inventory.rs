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
    /// Exact writer/schema combinations represented by this fixture.
    pub covered_writer_schema_pairs: Vec<ReleasedFixtureWriterSchemaPair>,
    /// Synthesized lifecycle matrix coverage supplied by this fixture.
    #[serde(default)]
    pub lifecycle_matrix: ReleasedFixtureLifecycleMatrix,
    /// Released migration-ledger endpoints represented by this fixture.
    #[serde(default)]
    pub migration_ledger_endpoints: Vec<String>,
    /// Exact migration-ledger prefix endpoints represented by this fixture.
    #[serde(default)]
    pub covered_migration_ledger_prefixes: Vec<String>,
    /// Every canonical event schema represented by the fixture. Complete-store fixtures must
    /// declare these exactly; classification-only fixtures may additionally declare schema
    /// coverage in `covered_schema_event_pairs` when one representative payload is proven
    /// structurally identical across multiple released schemas.
    pub event_schemas: Vec<u16>,
    /// Whether this fixture is a complete migratable store input rather than classification-only
    /// canonical payload coverage.
    #[serde(default)]
    pub migratable_store: bool,
    /// Whether this complete store fixture contains canonical payloads requiring historical event
    /// normalization rather than only a legacy storage contract/ledger.
    #[serde(default)]
    pub historical_payloads: bool,
    /// Exact canonical event count.
    pub expected_event_count: usize,
    /// Exact migration classifications expected from the fixture.
    pub expected_classifications: ReleasedFixtureClassificationCounts,
    /// Exact event-kind inventory represented by the fixture.
    pub covered_event_kinds: Vec<String>,
    /// Exact event schema/kind combinations represented by this fixture.
    pub covered_schema_event_pairs: Vec<ReleasedFixtureSchemaEventPair>,
    /// Preserved authoritative non-event records covered by this fixture.
    #[serde(default)]
    pub covered_authoritative_records: Vec<String>,
    /// Released historical roots whose treatment this fixture exercises.
    #[serde(default)]
    pub covered_roots: Vec<String>,
    /// Released historical root/writer combinations represented by this fixture.
    #[serde(default)]
    pub covered_root_writer_pairs: Vec<ReleasedFixtureRootWriterPair>,
    /// Released persisted tables whose treatment this fixture exercises.
    #[serde(default)]
    pub covered_tables: Vec<String>,
    /// Exact table/treatment combinations represented by this fixture.
    #[serde(default)]
    pub covered_table_treatments: Vec<ReleasedFixtureTableTreatment>,
}

/// Synthesized lifecycle matrix coverage owned by one permanent fixture.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReleasedFixtureLifecycleMatrix {
    /// Released writer epochs under which the payload fixture is materialized.
    pub source_writer_epochs: Vec<u32>,
    /// Whether the fixture owns writer/schema coverage across its schemas.
    pub owns_writer_schema: bool,
    /// Whether the fixture owns writer/schema/event coverage across its declared pairs.
    pub owns_writer_schema_event: bool,
    /// Setup or corroborating pairs excluded because another fixture owns them.
    pub schema_event_exclusions: Vec<ReleasedFixtureSchemaEventPair>,
}

/// One exact table/treatment combination represented by a fixture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleasedFixtureTableTreatment {
    /// Released persisted table.
    pub table: String,
    /// Stable treatment name declared by the released inventory.
    pub treatment: String,
}

/// One exact historical root/writer combination represented by a fixture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleasedFixtureRootWriterPair {
    /// Historical root relative to the state directory.
    pub root: String,
    /// Released source writer epoch stored under the root.
    pub writer_epoch: u32,
}

/// One exact writer/schema combination represented by a fixture.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleasedFixtureWriterSchemaPair {
    /// Released source writer epoch.
    pub writer_epoch: u32,
    /// Released historical event schema.
    pub event_schema: u16,
}

/// One exact event schema/kind combination represented by a fixture.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ReleasedFixtureSchemaEventPair {
    /// Released event schema.
    pub event_schema: u16,
    /// Persisted serde event kind.
    pub event_kind: String,
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
    /// A classification-only fixture claims store-level migration dimensions.
    #[error("classification-only released fixture {} claims store-level coverage", .0.display())]
    ClassificationOnlyClaimsStoreCoverage(PathBuf),
    /// Historical canonical payload coverage may only be claimed by a complete store fixture.
    #[error("historical-payload fixture {} is not a complete migratable store", .0.display())]
    HistoricalPayloadsRequireStore(PathBuf),
    /// A fixture references a migration endpoint absent from released inventory.
    #[error("released fixture {} references unknown migration endpoint {migration_id}", path.display())]
    UnknownMigrationEndpoint {
        /// Fixture path.
        path: PathBuf,
        /// Unknown migration identifier.
        migration_id: String,
    },
    /// A fixture claims a migration endpoint outside per-session storage.
    #[error("released fixture {} references non-session migration endpoint {migration_id}", path.display())]
    NonSessionMigrationEndpoint {
        /// Fixture path.
        path: PathBuf,
        /// Non-session migration identifier.
        migration_id: String,
    },
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
    /// A fixture references an event kind absent from released inventory.
    #[error("released fixture {} references unsupported event kind {event_kind}", path.display())]
    UnsupportedEventKind {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported event kind.
        event_kind: String,
    },
    /// Declared ledger-prefix endpoints are not present in fixture ledger coverage.
    #[error("released fixture {} has inconsistent migration-ledger prefix coverage", .0.display())]
    LedgerPrefixCoverageMismatch(PathBuf),
    /// A fixture references a root absent from released inventory.
    #[error("released fixture {} references unsupported historical root {root}", path.display())]
    UnsupportedRoot {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported root.
        root: String,
    },
    /// Declared root/writer pairs do not match fixture root and writer coverage.
    #[error("released fixture {} has inconsistent historical root/writer coverage", .0.display())]
    RootWriterCoverageMismatch(PathBuf),
    /// A fixture references a table absent from released inventory.
    #[error("released fixture {} references unsupported persisted table {table}", path.display())]
    UnsupportedTable {
        /// Fixture path.
        path: PathBuf,
        /// Unsupported table.
        table: String,
    },
    /// Declared writer/schema pairs do not exactly match fixture writer and schema coverage.
    #[error("released fixture {} has inconsistent writer/schema coverage", .0.display())]
    WriterSchemaCoverageMismatch(PathBuf),
    /// Declared table/treatment pairs do not match released table treatment inventory.
    #[error("released fixture {} has inconsistent table treatment coverage", .0.display())]
    TableTreatmentCoverageMismatch(PathBuf),
    /// A fixture pairs an event kind with a schema that never persisted it.
    #[error(
        "released fixture {} pairs event kind {event_kind} with unreleased schema {event_schema}",
        path.display()
    )]
    EventKindNotReleasedInSchema {
        /// Fixture path.
        path: PathBuf,
        /// Event schema claimed by the fixture.
        event_schema: u16,
        /// Event kind claimed by the fixture.
        event_kind: String,
    },
    /// Declared schema/event pairs do not exactly match fixture schema and kind coverage.
    #[error("released fixture {} has inconsistent schema/event coverage", .0.display())]
    SchemaEventCoverageMismatch(PathBuf),
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
    /// Two fixtures claim the same exact released inventory dimension.
    #[error("released fixtures duplicate exact {field} coverage {value}")]
    DuplicateExactCoverage {
        /// Exact coverage dimension.
        field: &'static str,
        /// Duplicate rendered value.
        value: String,
    },
    /// Fixture classification totals do not equal the exact canonical event count.
    #[error("released fixture {} classification totals do not match expected event count", .0.display())]
    ClassificationCountMismatch(PathBuf),
    /// A fixture claims a non-authoritative or non-preserved record.
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

fn insert_exact_coverage(
    unique: &mut BTreeSet<String>,
    field: &'static str,
    value: String,
) -> Result<(), ReleasedFixtureInventoryError> {
    if unique.insert(value.clone()) {
        Ok(())
    } else {
        Err(ReleasedFixtureInventoryError::DuplicateExactCoverage { field, value })
    }
}

fn validate_exact_fixture_coverage_unique(
    manifest: &ReleasedFixtureManifest,
) -> Result<(), ReleasedFixtureInventoryError> {
    let mut writer_schema_event = BTreeSet::new();
    let mut explicit_writer_schema = BTreeSet::new();
    let mut matrix_writer_schema = BTreeSet::new();
    let mut ledger_prefix = BTreeSet::new();
    let mut root_writer = BTreeSet::new();
    let mut table = BTreeSet::new();
    for fixture in &manifest.fixtures {
        for pair in &fixture.covered_writer_schema_pairs {
            let writer_schema_value = format!("{}:{}", pair.writer_epoch, pair.event_schema);
            if fixture.lifecycle_matrix.owns_writer_schema_event {
                explicit_writer_schema.insert(writer_schema_value);
            } else {
                insert_exact_coverage(
                    &mut explicit_writer_schema,
                    "writer_schema",
                    writer_schema_value,
                )?;
            }
            for schema_event in fixture
                .covered_schema_event_pairs
                .iter()
                .filter(|schema_event| schema_event.event_schema == pair.event_schema)
            {
                let value = format!(
                    "{}:{}:{}",
                    pair.writer_epoch, schema_event.event_schema, schema_event.event_kind
                );
                writer_schema_event.insert(value);
            }
        }
        if fixture.lifecycle_matrix.owns_writer_schema {
            for writer_epoch in &fixture.lifecycle_matrix.source_writer_epochs {
                for event_schema in &fixture.event_schemas {
                    insert_exact_coverage(
                        &mut matrix_writer_schema,
                        "writer_schema",
                        format!("{writer_epoch}:{event_schema}"),
                    )?;
                }
            }
        }
        if fixture.lifecycle_matrix.owns_writer_schema_event {
            for writer_epoch in &fixture.lifecycle_matrix.source_writer_epochs {
                for schema_event in &fixture.covered_schema_event_pairs {
                    if fixture.covered_writer_schema_pairs.iter().any(|pair| {
                        pair.writer_epoch == *writer_epoch
                            && pair.event_schema == schema_event.event_schema
                    }) || fixture
                        .lifecycle_matrix
                        .schema_event_exclusions
                        .contains(schema_event)
                    {
                        continue;
                    }
                    insert_exact_coverage(
                        &mut writer_schema_event,
                        "writer_schema_event",
                        format!(
                            "{}:{}:{}",
                            writer_epoch, schema_event.event_schema, schema_event.event_kind
                        ),
                    )?;
                }
            }
        }
        for prefix in &fixture.covered_migration_ledger_prefixes {
            insert_exact_coverage(&mut ledger_prefix, "ledger_prefix", prefix.clone())?;
        }
        for pair in &fixture.covered_root_writer_pairs {
            insert_exact_coverage(
                &mut root_writer,
                "root_writer",
                format!("{}:{}", pair.root, pair.writer_epoch),
            )?;
        }
        for covered_table in &fixture.covered_tables {
            insert_exact_coverage(&mut table, "table", covered_table.clone())?;
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
#[allow(clippy::too_many_lines)]
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
        if fixture.migratable_store
            && fixture.source_writer_epochs.is_empty()
            && fixture.lifecycle_matrix.source_writer_epochs.is_empty()
        {
            return Err(ReleasedFixtureInventoryError::MissingWriterEpoch(
                fixture.path.clone(),
            ));
        }
        if (fixture.lifecycle_matrix.owns_writer_schema
            || fixture.lifecycle_matrix.owns_writer_schema_event)
            && fixture.lifecycle_matrix.source_writer_epochs.is_empty()
        {
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
        if !fixture.migratable_store
            && (!fixture.source_writer_epochs.is_empty()
                || (!fixture.lifecycle_matrix.owns_writer_schema_event
                    && !fixture.covered_writer_schema_pairs.is_empty())
                || !fixture.migration_ledger_endpoints.is_empty()
                || !fixture.covered_migration_ledger_prefixes.is_empty()
                || !fixture.covered_authoritative_records.is_empty()
                || !fixture.covered_roots.is_empty()
                || !fixture.covered_root_writer_pairs.is_empty()
                || !fixture.covered_tables.is_empty())
        {
            return Err(
                ReleasedFixtureInventoryError::ClassificationOnlyClaimsStoreCoverage(
                    fixture.path.clone(),
                ),
            );
        }
        if fixture.historical_payloads && !fixture.migratable_store {
            return Err(
                ReleasedFixtureInventoryError::HistoricalPayloadsRequireStore(fixture.path.clone()),
            );
        }
        validate_fixture_values_unique(
            &fixture.path,
            "writer_schema_event_matrix_exclusions",
            fixture
                .lifecycle_matrix
                .schema_event_exclusions
                .iter()
                .map(|pair| format!("{}:{}", pair.event_schema, pair.event_kind)),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "lifecycle_source_writer_epochs",
            fixture
                .lifecycle_matrix
                .source_writer_epochs
                .iter()
                .map(ToString::to_string),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "source_writer_epochs",
            fixture.source_writer_epochs.iter().map(ToString::to_string),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_writer_schema_pairs",
            fixture
                .covered_writer_schema_pairs
                .iter()
                .map(|pair| format!("{}:{}", pair.writer_epoch, pair.event_schema)),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "migration_ledger_endpoints",
            fixture.migration_ledger_endpoints.iter().cloned(),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_migration_ledger_prefixes",
            fixture.covered_migration_ledger_prefixes.iter().cloned(),
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
            "covered_schema_event_pairs",
            fixture
                .covered_schema_event_pairs
                .iter()
                .map(|pair| format!("{}:{}", pair.event_schema, pair.event_kind)),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_authoritative_records",
            fixture.covered_authoritative_records.iter().cloned(),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_roots",
            fixture.covered_roots.iter().cloned(),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_root_writer_pairs",
            fixture
                .covered_root_writer_pairs
                .iter()
                .map(|pair| format!("{}:{}", pair.root, pair.writer_epoch)),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_tables",
            fixture.covered_tables.iter().cloned(),
        )?;
        validate_fixture_values_unique(
            &fixture.path,
            "covered_table_treatments",
            fixture
                .covered_table_treatments
                .iter()
                .map(|pair| format!("{}:{}", pair.table, pair.treatment)),
        )?;
        if fixture.expected_classifications.converted
            + fixture.expected_classifications.retired_known
            + fixture.expected_classifications.current_passthrough
            != fixture.expected_event_count
        {
            return Err(ReleasedFixtureInventoryError::ClassificationCountMismatch(
                fixture.path.clone(),
            ));
        }
        for migration_id in &fixture.migration_ledger_endpoints {
            let Some(migration) = RELEASED_MIGRATION_IDS
                .iter()
                .find(|migration| migration.id == migration_id)
            else {
                return Err(ReleasedFixtureInventoryError::UnknownMigrationEndpoint {
                    path: fixture.path.clone(),
                    migration_id: migration_id.clone(),
                });
            };
            if migration.domain != ReleasedMigrationDomain::Session {
                return Err(ReleasedFixtureInventoryError::NonSessionMigrationEndpoint {
                    path: fixture.path.clone(),
                    migration_id: migration_id.clone(),
                });
            }
        }
        if !fixture
            .covered_migration_ledger_prefixes
            .iter()
            .all(|prefix| fixture.migration_ledger_endpoints.contains(prefix))
        {
            return Err(ReleasedFixtureInventoryError::LedgerPrefixCoverageMismatch(
                fixture.path.clone(),
            ));
        }
        for writer_epoch in fixture
            .source_writer_epochs
            .iter()
            .chain(&fixture.lifecycle_matrix.source_writer_epochs)
        {
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
        if fixture.migratable_store {
            let declared_writers = fixture
                .source_writer_epochs
                .iter()
                .chain(&fixture.lifecycle_matrix.source_writer_epochs)
                .copied()
                .collect::<BTreeSet<_>>();
            let paired_writers = fixture
                .covered_writer_schema_pairs
                .iter()
                .map(|pair| pair.writer_epoch)
                .collect::<BTreeSet<_>>();
            let paired_schemas = fixture
                .covered_writer_schema_pairs
                .iter()
                .map(|pair| pair.event_schema)
                .collect::<BTreeSet<_>>();
            if (!fixture.covered_writer_schema_pairs.is_empty()
                && paired_writers != declared_writers)
                || (!fixture.covered_writer_schema_pairs.is_empty()
                    && paired_schemas != fixture.event_schemas.iter().copied().collect())
            {
                return Err(ReleasedFixtureInventoryError::WriterSchemaCoverageMismatch(
                    fixture.path.clone(),
                ));
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
        for root in &fixture.covered_roots {
            if !RELEASED_HISTORICAL_ROOTS
                .iter()
                .any(|descriptor| descriptor.path == root)
            {
                return Err(ReleasedFixtureInventoryError::UnsupportedRoot {
                    path: fixture.path.clone(),
                    root: root.clone(),
                });
            }
        }
        let paired_roots = fixture
            .covered_root_writer_pairs
            .iter()
            .map(|pair| pair.root.clone())
            .collect::<BTreeSet<_>>();
        for pair in &fixture.covered_root_writer_pairs {
            if !fixture.covered_roots.contains(&pair.root)
                || !(fixture.source_writer_epochs.contains(&pair.writer_epoch)
                    || fixture
                        .lifecycle_matrix
                        .source_writer_epochs
                        .contains(&pair.writer_epoch))
            {
                return Err(ReleasedFixtureInventoryError::RootWriterCoverageMismatch(
                    fixture.path.clone(),
                ));
            }
        }
        if paired_roots != fixture.covered_roots.iter().cloned().collect() {
            return Err(ReleasedFixtureInventoryError::RootWriterCoverageMismatch(
                fixture.path.clone(),
            ));
        }
        for table in &fixture.covered_tables {
            if !RELEASED_RECORD_TREATMENTS
                .iter()
                .any(|descriptor| descriptor.table == table)
            {
                return Err(ReleasedFixtureInventoryError::UnsupportedTable {
                    path: fixture.path.clone(),
                    table: table.clone(),
                });
            }
        }
        let declared_tables = fixture
            .covered_table_treatments
            .iter()
            .map(|pair| pair.table.clone())
            .collect::<BTreeSet<_>>();
        if declared_tables != fixture.covered_tables.iter().cloned().collect() {
            return Err(
                ReleasedFixtureInventoryError::TableTreatmentCoverageMismatch(fixture.path.clone()),
            );
        }
        for pair in &fixture.covered_table_treatments {
            let Some(descriptor) = RELEASED_RECORD_TREATMENTS
                .iter()
                .find(|descriptor| descriptor.table == pair.table)
            else {
                return Err(ReleasedFixtureInventoryError::UnsupportedTable {
                    path: fixture.path.clone(),
                    table: pair.table.clone(),
                });
            };
            if descriptor.treatment.as_str() != pair.treatment {
                return Err(
                    ReleasedFixtureInventoryError::TableTreatmentCoverageMismatch(
                        fixture.path.clone(),
                    ),
                );
            }
        }
        for event_kind in &fixture.covered_event_kinds {
            if !RELEASED_EVENT_VARIANTS
                .iter()
                .any(|descriptor| descriptor.kind == event_kind)
            {
                return Err(ReleasedFixtureInventoryError::UnsupportedEventKind {
                    path: fixture.path.clone(),
                    event_kind: event_kind.clone(),
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
        let declared_pairs = fixture
            .covered_schema_event_pairs
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let paired_schemas = declared_pairs
            .iter()
            .map(|pair| pair.event_schema)
            .collect::<BTreeSet<_>>();
        let paired_kinds = declared_pairs
            .iter()
            .map(|pair| pair.event_kind.clone())
            .collect::<BTreeSet<_>>();
        let fixture_schemas = fixture
            .event_schemas
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let fixture_kinds = fixture
            .covered_event_kinds
            .iter()
            .cloned()
            .collect::<BTreeSet<_>>();
        let invalid_schema_coverage = if fixture.migratable_store {
            paired_schemas != fixture_schemas
        } else {
            !fixture_schemas.is_subset(&paired_schemas)
        };
        if invalid_schema_coverage || paired_kinds != fixture_kinds {
            return Err(ReleasedFixtureInventoryError::SchemaEventCoverageMismatch(
                fixture.path.clone(),
            ));
        }
        for pair in &fixture.covered_schema_event_pairs {
            if (fixture.migratable_store && !fixture.event_schemas.contains(&pair.event_schema))
                || !fixture.covered_event_kinds.contains(&pair.event_kind)
            {
                return Err(ReleasedFixtureInventoryError::SchemaEventCoverageMismatch(
                    fixture.path.clone(),
                ));
            }
            let Some(descriptor) = RELEASED_EVENT_VARIANTS
                .iter()
                .find(|descriptor| descriptor.kind == pair.event_kind)
            else {
                return Err(ReleasedFixtureInventoryError::UnsupportedEventKind {
                    path: fixture.path.clone(),
                    event_kind: pair.event_kind.clone(),
                });
            };
            if !descriptor.supports_schema(pair.event_schema) {
                return Err(
                    ReleasedFixtureInventoryError::EventKindNotReleasedInSchema {
                        path: fixture.path.clone(),
                        event_schema: pair.event_schema,
                        event_kind: pair.event_kind.clone(),
                    },
                );
            }
        }
    }
    validate_exact_fixture_coverage_unique(&manifest)?;
    let actual_paths = std::fs::read_dir(fixture_root.join("stores"))?
        .map(|entry| entry.map(|entry| PathBuf::from("stores").join(entry.file_name())))
        .collect::<Result<BTreeSet<_>, _>>()?;
    if listed_paths != actual_paths {
        return Err(ReleasedFixtureInventoryError::PathInventoryMismatch);
    }
    Ok(manifest)
}

/// Missing permanent fixture coverage relative to released inventory.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReleasedFixtureCoverageGaps {
    /// Released writer epochs absent from all fixtures.
    pub writer_epochs: BTreeSet<u32>,
    /// Released writer/schema combinations absent from exact manifest declarations. Store-level
    /// lifecycle tests may prove additional synthesized cross-product coverage separately.
    pub writer_schema_pairs: BTreeSet<(u32, u16)>,
    /// Released writer/schema/event combinations absent from exact manifest declarations. Only
    /// event variants actually released in the schema are required. Store-level lifecycle tests
    /// may prove additional synthesized cross-product coverage without changing this exact-claim
    /// inventory.
    pub writer_schema_event_combinations: BTreeSet<(u32, u16, String)>,
    /// Released writer migration edges absent from all fixtures.
    pub writer_edges: BTreeSet<(u32, u32)>,
    /// Released root treatments absent from exact fixture declarations.
    pub roots: BTreeSet<String>,
    /// Released root/writer combinations absent from exact fixture declarations.
    pub root_writer_pairs: BTreeSet<(String, u32)>,
    /// Released persisted-table treatments absent from exact fixture declarations.
    pub tables: BTreeSet<String>,
    /// Released migration-ledger endpoints absent from fixture declarations and migration-owned
    /// non-payload ledger cases.
    pub migration_ledger_endpoints: BTreeSet<String>,
    /// Released migration-ledger prefix endpoints absent from exact manifest declarations or
    /// migration-owned non-payload ledger cases.
    pub migration_ledger_prefixes: BTreeSet<String>,
    /// Released event schemas absent from all fixtures.
    pub event_schemas: BTreeSet<u16>,
    /// Released event variants absent from all fixtures.
    pub event_kinds: BTreeSet<String>,
    /// Released historical event families absent from permanent payload fixtures.
    pub historical_store_event_kinds: BTreeSet<String>,
    /// Preserved per-session authoritative records absent from all fixtures.
    pub authoritative_records: BTreeSet<String>,
}

impl ReleasedFixtureCoverageGaps {
    /// Return whether every released fixture dimension is covered.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.writer_epochs.is_empty()
            && self.writer_schema_pairs.is_empty()
            && self.writer_schema_event_combinations.is_empty()
            && self.writer_edges.is_empty()
            && self.roots.is_empty()
            && self.root_writer_pairs.is_empty()
            && self.tables.is_empty()
            && self.migration_ledger_endpoints.is_empty()
            && self.migration_ledger_prefixes.is_empty()
            && self.event_schemas.is_empty()
            && self.event_kinds.is_empty()
            && self.historical_store_event_kinds.is_empty()
            && self.authoritative_records.is_empty()
    }
}

fn fixture_writer_schema_pairs(manifest: &ReleasedFixtureManifest) -> BTreeSet<(u32, u16)> {
    manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            let explicit = fixture
                .covered_writer_schema_pairs
                .iter()
                .map(|pair| (pair.writer_epoch, pair.event_schema))
                .collect::<BTreeSet<_>>();
            let matrix = fixture
                .lifecycle_matrix
                .owns_writer_schema
                .then(|| {
                    fixture
                        .lifecycle_matrix
                        .source_writer_epochs
                        .iter()
                        .flat_map(|writer_epoch| {
                            fixture
                                .event_schemas
                                .iter()
                                .map(move |event_schema| (*writer_epoch, *event_schema))
                        })
                })
                .into_iter()
                .flatten();
            explicit.into_iter().chain(matrix)
        })
        .collect()
}

fn required_writer_schema_pairs() -> BTreeSet<(u32, u16)> {
    RELEASED_HISTORICAL_WRITER_EPOCHS
        .iter()
        .flat_map(|writer_epoch| {
            RELEASED_HISTORICAL_EVENT_SCHEMAS
                .iter()
                .map(move |event_schema| (*writer_epoch, *event_schema))
        })
        .collect()
}

fn fixture_writer_edges(manifest: &ReleasedFixtureManifest) -> BTreeSet<(u32, u32)> {
    manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .source_writer_epochs
                .iter()
                .chain(&fixture.lifecycle_matrix.source_writer_epochs)
                .filter_map(|writer_epoch| {
                    MIGRATION_STEPS
                        .iter()
                        .find(|step| step.source_writer_epoch == *writer_epoch)
                        .map(|step| (step.source_writer_epoch, step.target_writer_epoch))
                })
        })
        .collect()
}

fn required_writer_edges() -> BTreeSet<(u32, u32)> {
    MIGRATION_STEPS
        .iter()
        .map(|step| (step.source_writer_epoch, step.target_writer_epoch))
        .collect()
}

fn is_released_historical_event_variant(variant: &ReleasedEventVariantDescriptor) -> bool {
    RELEASED_HISTORICAL_EVENT_SCHEMAS
        .iter()
        .any(|schema| variant.supports_schema(*schema))
}

fn required_writer_schema_event_combinations() -> BTreeSet<(u32, u16, String)> {
    RELEASED_HISTORICAL_WRITER_EPOCHS
        .iter()
        .flat_map(|writer_epoch| {
            RELEASED_HISTORICAL_EVENT_SCHEMAS
                .iter()
                .flat_map(move |event_schema| {
                    RELEASED_EVENT_VARIANTS
                        .iter()
                        .filter(move |event_variant| event_variant.supports_schema(*event_schema))
                        .map(move |event_variant| {
                            (*writer_epoch, *event_schema, event_variant.kind.to_owned())
                        })
                })
        })
        .collect()
}

fn released_root_coverage_gaps(manifest: &ReleasedFixtureManifest) -> BTreeSet<String> {
    let covered = manifest
        .fixtures
        .iter()
        .flat_map(|fixture| fixture.covered_roots.iter().cloned())
        .collect::<BTreeSet<_>>();
    RELEASED_HISTORICAL_ROOTS
        .iter()
        .map(|root| root.path.to_owned())
        .collect::<BTreeSet<_>>()
        .difference(&covered)
        .cloned()
        .collect()
}

fn released_root_writer_coverage_gaps(
    manifest: &ReleasedFixtureManifest,
) -> BTreeSet<(String, u32)> {
    let covered = manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .covered_root_writer_pairs
                .iter()
                .map(|pair| (pair.root.clone(), pair.writer_epoch))
        })
        .collect::<BTreeSet<_>>();
    RELEASED_HISTORICAL_ROOTS
        .iter()
        .map(|root| (root.path.to_owned(), root.source_writer_epoch))
        .collect::<BTreeSet<_>>()
        .difference(&covered)
        .cloned()
        .collect()
}

fn released_table_coverage_gaps(manifest: &ReleasedFixtureManifest) -> BTreeSet<String> {
    let covered = manifest
        .fixtures
        .iter()
        .flat_map(|fixture| fixture.covered_tables.iter().cloned())
        .collect::<BTreeSet<_>>();
    RELEASED_RECORD_TREATMENTS
        .iter()
        .map(|record| record.table.to_owned())
        .collect::<BTreeSet<_>>()
        .difference(&covered)
        .cloned()
        .collect()
}

fn fixture_writer_schema_event_combinations(
    manifest: &ReleasedFixtureManifest,
) -> BTreeSet<(u32, u16, String)> {
    manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            let explicit = fixture
                .covered_writer_schema_pairs
                .iter()
                .flat_map(|writer_schema| {
                    fixture
                        .covered_schema_event_pairs
                        .iter()
                        .filter(move |schema_event| {
                            schema_event.event_schema == writer_schema.event_schema
                        })
                        .map(move |schema_event| {
                            (
                                writer_schema.writer_epoch,
                                schema_event.event_schema,
                                schema_event.event_kind.clone(),
                            )
                        })
                });
            let matrix = fixture
                .lifecycle_matrix
                .owns_writer_schema_event
                .then(move || {
                    fixture
                        .lifecycle_matrix
                        .source_writer_epochs
                        .iter()
                        .flat_map(move |writer_epoch| {
                            fixture
                                .covered_schema_event_pairs
                                .iter()
                                .filter(move |schema_event| {
                                    !fixture.covered_writer_schema_pairs.iter().any(|pair| {
                                        pair.writer_epoch == *writer_epoch
                                            && pair.event_schema == schema_event.event_schema
                                    }) && !fixture
                                        .lifecycle_matrix
                                        .schema_event_exclusions
                                        .contains(schema_event)
                                })
                                .map(move |schema_event| {
                                    (
                                        *writer_epoch,
                                        schema_event.event_schema,
                                        schema_event.event_kind.clone(),
                                    )
                                })
                        })
                })
                .into_iter()
                .flatten();
            explicit.chain(matrix)
        })
        .collect()
}

fn released_historical_event_kinds() -> BTreeSet<String> {
    RELEASED_EVENT_VARIANTS
        .iter()
        .filter(|variant| is_released_historical_event_variant(variant))
        .map(|variant| variant.kind.to_owned())
        .collect()
}

fn historical_store_event_coverage_gaps(manifest: &ReleasedFixtureManifest) -> BTreeSet<String> {
    let covered = manifest
        .fixtures
        .iter()
        .flat_map(|fixture| fixture.covered_event_kinds.iter().cloned())
        .collect::<BTreeSet<_>>();
    released_historical_event_kinds()
        .difference(&covered)
        .cloned()
        .collect()
}

fn fixture_schema_coverage(manifest: &ReleasedFixtureManifest) -> BTreeSet<u16> {
    manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            fixture.event_schemas.iter().copied().chain(
                fixture
                    .covered_schema_event_pairs
                    .iter()
                    .map(|pair| pair.event_schema),
            )
        })
        .collect()
}

fn covered_ledger_fixture_endpoints(manifest: &ReleasedFixtureManifest) -> BTreeSet<String> {
    manifest
        .fixtures
        .iter()
        .flat_map(|fixture| {
            fixture
                .migration_ledger_endpoints
                .iter()
                .chain(&fixture.covered_migration_ledger_prefixes)
                .cloned()
        })
        .chain(
            released_session_ledger_prefix_fixture_cases()
                .into_iter()
                .map(|case| case.endpoint.to_owned()),
        )
        .collect()
}

/// Return exact released inventory dimensions not represented by permanent fixtures.
#[must_use]
pub fn released_fixture_coverage_gaps(
    manifest: &ReleasedFixtureManifest,
) -> ReleasedFixtureCoverageGaps {
    let covered_writers = released_fixture_writer_coverage(manifest)
        .into_keys()
        .collect::<BTreeSet<_>>();
    let covered_schemas = fixture_schema_coverage(manifest);
    let covered_writer_schema_pairs = fixture_writer_schema_pairs(manifest);
    let covered_writer_schema_event_combinations =
        fixture_writer_schema_event_combinations(manifest);
    let required_writer_schema_pairs = required_writer_schema_pairs();
    let required_writer_schema_event_combinations = required_writer_schema_event_combinations();
    let covered_writer_edges = fixture_writer_edges(manifest);
    let required_writer_edges = required_writer_edges();
    let covered_ledger_endpoints = covered_ledger_fixture_endpoints(manifest);
    let covered_kinds = manifest
        .fixtures
        .iter()
        .flat_map(|fixture| fixture.covered_event_kinds.iter().cloned())
        .collect::<BTreeSet<_>>();
    let covered_records = released_fixture_authoritative_record_coverage(manifest)
        .into_keys()
        .collect::<BTreeSet<_>>();
    let required_records = RELEASED_RECORD_TREATMENTS
        .iter()
        .filter(|record| record.treatment == ReleasedRecordTreatment::Preserve)
        .filter(|record| record.table != "events")
        .map(|record| record.table.to_owned())
        .collect::<BTreeSet<_>>();
    ReleasedFixtureCoverageGaps {
        writer_epochs: RELEASED_HISTORICAL_WRITER_EPOCHS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .difference(&covered_writers)
            .copied()
            .collect(),
        writer_schema_pairs: required_writer_schema_pairs
            .difference(&covered_writer_schema_pairs)
            .copied()
            .collect(),
        writer_schema_event_combinations: required_writer_schema_event_combinations
            .difference(&covered_writer_schema_event_combinations)
            .cloned()
            .collect(),
        writer_edges: required_writer_edges
            .difference(&covered_writer_edges)
            .copied()
            .collect(),
        roots: released_root_coverage_gaps(manifest),
        root_writer_pairs: released_root_writer_coverage_gaps(manifest),
        tables: released_table_coverage_gaps(manifest),
        migration_ledger_endpoints: RELEASED_MIGRATION_IDS
            .iter()
            .filter(|migration| migration.domain == ReleasedMigrationDomain::Session)
            .map(|migration| migration.id.to_owned())
            .collect::<BTreeSet<_>>()
            .difference(&covered_ledger_endpoints)
            .cloned()
            .collect(),
        migration_ledger_prefixes: RELEASED_MIGRATION_IDS
            .iter()
            .filter(|migration| migration.domain == ReleasedMigrationDomain::Session)
            .map(|migration| migration.id.to_owned())
            .collect::<BTreeSet<_>>()
            .difference(&covered_ledger_endpoints)
            .cloned()
            .collect(),
        event_schemas: RELEASED_HISTORICAL_EVENT_SCHEMAS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .difference(&covered_schemas)
            .copied()
            .collect(),
        event_kinds: RELEASED_EVENT_VARIANTS
            .iter()
            .filter(|variant| is_released_historical_event_variant(variant))
            .map(|variant| variant.kind.to_owned())
            .collect::<BTreeSet<_>>()
            .difference(&covered_kinds)
            .cloned()
            .collect(),
        historical_store_event_kinds: historical_store_event_coverage_gaps(manifest),
        authoritative_records: required_records
            .difference(&covered_records)
            .cloned()
            .collect(),
    }
}

#[must_use]
pub fn released_fixture_writer_coverage(
    manifest: &ReleasedFixtureManifest,
) -> BTreeMap<u32, usize> {
    let mut coverage = BTreeMap::new();
    for fixture in &manifest.fixtures {
        for writer_epoch in fixture
            .source_writer_epochs
            .iter()
            .chain(&fixture.lifecycle_matrix.source_writer_epochs)
        {
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

impl ReleasedRecordTreatment {
    /// Stable manifest name for this treatment.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::RebuildFromCanonical => "rebuild_from_canonical",
            Self::Preserve => "preserve",
            Self::FinalizeCurrent => "finalize_current",
            Self::GlobalOnly => "global_only",
            Self::RetireDerived => "retire_derived",
        }
    }
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

impl ReleasedEventVariantDescriptor {
    /// Return whether this event kind was persisted by the supplied released schema.
    #[must_use]
    pub fn supports_schema(self, schema: u16) -> bool {
        let (first, last) = released_event_schema_range(self.kind);
        schema >= first && schema <= last
    }
}

fn released_event_schema_range(kind: &str) -> (u16, u16) {
    match kind {
        "assistant_reasoning_activity" => (40, 40),
        "agent_changed" => (3, 39),
        "assistant_reasoning_delta"
        | "assistant_reasoning_message"
        | "skill_activated"
        | "skill_context_loaded"
        | "skill_deactivated"
        | "skill_invocation_failed"
        | "skill_invoked"
        | "skill_suggested" => (9, 39),
        "context_compacted" => (6, 39),
        "context_usage_observed" => (26, 31),
        "provider_context_compacted" => (26, 39),
        "execution_session_created" | "opaque_event" => (39, 39),
        "interactive_tool_request_created" | "interactive_tool_request_resolved" => (25, 35),
        "legacy_event"
        | "legacy_turn_finished"
        | "legacy_turn_started"
        | "request_context_observed" => (32, 39),
        "legacy_tool_invocation_presentation" | "reasoning_changed" => (25, 39),
        "model_turn_cancel_requested" => (17, 39),
        "model_turn_finished" | "model_turn_started" => (4, 39),
        "model_usage" => (5, 39),
        "permission_requested" | "permission_resolved" => (2, 39),
        "plugin_automation_turn_finished" | "plugin_automation_turn_started" => (29, 32),
        "plugin_status_note" => (29, 39),
        "ralph_lifecycle" => (23, 39),
        "runtime_work_cancel_requested" | "runtime_work_finished" | "runtime_work_started" => {
            (11, 39)
        }
        "runtime_work_progress" => (18, 39),
        "session_forked" => (22, 39),
        "session_imported" => (16, 39),
        "session_renamed" | "trace_event" => (7, 39),
        "tool_contribution"
        | "tool_exchange_requested"
        | "tool_exchange_resolved"
        | "tool_invocation_lifecycle" => (35, 39),
        "tool_contribution_placed" => (38, 39),
        "tool_invocation_presentation" => (21, 25),
        "tool_invocation_result_recorded" => (37, 39),
        "tool_invocation_stream" => (12, 39),
        "working_directory_changed" => (15, 39),
        "assistant_delta"
        | "assistant_message"
        | "client_attached"
        | "client_detached"
        | "model_changed"
        | "session_created"
        | "system_message"
        | "tool_call_finished"
        | "tool_call_requested"
        | "user_message" => (1, 39),
        _ => unreachable!("released event inventory must declare a schema range"),
    }
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
        kind: "interactive_tool_request_created",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "interactive_tool_request_resolved",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "legacy_event",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "legacy_tool_invocation_presentation",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "legacy_turn_finished",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "legacy_turn_started",
        treatment: ReleasedEventTreatment::RetiredKnown,
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
        kind: "plugin_automation_turn_finished",
        treatment: ReleasedEventTreatment::RetiredKnown,
    },
    ReleasedEventVariantDescriptor {
        kind: "plugin_automation_turn_started",
        treatment: ReleasedEventTreatment::RetiredKnown,
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
        kind: "tool_invocation_presentation",
        treatment: ReleasedEventTreatment::RetiredKnown,
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

/// Required treatment for one released migration-ledger identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedMigrationTreatment {
    /// Materialize the migration as part of the current target schema.
    MaterializeCurrent,
    /// Recognize the historical identity but do not recreate its superseded schema operation.
    RetiredSuperseded,
    /// Keep the global-domain migration outside per-session migration.
    GlobalOnly,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedMigrationDescriptor {
    /// Stable durable migration identity.
    pub id: &'static str,
    /// Storage domain that owns the ledger entry.
    pub domain: ReleasedMigrationDomain,
    /// Required treatment when the identity is observed.
    pub treatment: ReleasedMigrationTreatment,
}

/// One explicit ledger-prefix fixture requirement.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedLedgerPrefixFixtureCase {
    /// Released endpoint represented by this case.
    pub endpoint: &'static str,
    /// Ordered completed migration IDs that form the source ledger.
    pub completed_migration_ids: Vec<&'static str>,
    /// Required treatment for the endpoint itself.
    pub endpoint_treatment: ReleasedMigrationTreatment,
}

/// Return one deterministic fixture case for every released per-session ledger endpoint.
///
/// Materialized current migrations form one ordered prefix in current-schema order. Retired
/// superseded identities are represented as standalone historical-ledger cases because they never
/// belonged to that current ordered ledger.
#[must_use]
pub fn released_session_ledger_prefix_fixture_cases() -> Vec<ReleasedLedgerPrefixFixtureCase> {
    const MATERIALIZED_LEDGER_ORDER: &[&str] = &[
        "001_events_table",
        "002_events_event_type_index",
        "003_session_state_table",
        "004_input_messages_table",
        "005_input_messages_event_seq_index",
        "006_transcript_items_table",
        "007_transcript_items_event_range_index",
        "008_tool_runs_table",
        "009_tool_runs_status_index",
        "010_projection_checkpoints_table",
        "011_snapshots_table",
        "012_runtime_work_table",
        "013_runtime_work_status_index",
        "014_runtime_work_parent_index",
        "015_session_drafts_table",
        "016_session_state_reasoning_effort_column",
        "017_session_state_reasoning_summary_column",
        "018_model_context_projection_state_table",
        "019_model_context_entries_table",
        "020_model_context_entries_event_type_index",
        "021_artifact_references_table",
        "022_context_occupancy_projection_table",
        "023_reset_legacy_context_occupancy_projection",
        "024_reset_request_context_occupancy_projection",
        "025_turn_receipts_table",
        "026_session_storage_contract_table",
        "027_initialize_session_storage_contract",
        "028_session_compatibility_state",
        "029_session_compatibility_issues",
        "030_session_state_visibility_column",
        "031_session_state_execution_provenance_column",
        "032_session_migration_receipts_table",
    ];
    let mut cases = MATERIALIZED_LEDGER_ORDER
        .iter()
        .enumerate()
        .map(|(index, endpoint)| ReleasedLedgerPrefixFixtureCase {
            endpoint,
            completed_migration_ids: MATERIALIZED_LEDGER_ORDER[..=index].to_vec(),
            endpoint_treatment: ReleasedMigrationTreatment::MaterializeCurrent,
        })
        .collect::<Vec<_>>();
    cases.extend(
        RELEASED_MIGRATION_IDS
            .iter()
            .filter(|migration| {
                migration.domain == ReleasedMigrationDomain::Session
                    && migration.treatment == ReleasedMigrationTreatment::RetiredSuperseded
            })
            .map(|migration| ReleasedLedgerPrefixFixtureCase {
                endpoint: migration.id,
                completed_migration_ids: vec![migration.id],
                endpoint_treatment: migration.treatment,
            }),
    );
    cases
}

/// Complete released migration-ID inventory observed across Git history.
pub const RELEASED_MIGRATION_IDS: &[ReleasedMigrationDescriptor] = &[
    ReleasedMigrationDescriptor {
        id: "001_events_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "001_global_catalog",
        domain: ReleasedMigrationDomain::Global,
        treatment: ReleasedMigrationTreatment::GlobalOnly,
    },
    ReleasedMigrationDescriptor {
        id: "001_global_sessions_table",
        domain: ReleasedMigrationDomain::Global,
        treatment: ReleasedMigrationTreatment::GlobalOnly,
    },
    ReleasedMigrationDescriptor {
        id: "001_session_event_store_and_projections",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::RetiredSuperseded,
    },
    ReleasedMigrationDescriptor {
        id: "002_events_event_type_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "002_global_sessions_updated_at_index",
        domain: ReleasedMigrationDomain::Global,
        treatment: ReleasedMigrationTreatment::GlobalOnly,
    },
    ReleasedMigrationDescriptor {
        id: "003_global_composer_drafts_table",
        domain: ReleasedMigrationDomain::Global,
        treatment: ReleasedMigrationTreatment::GlobalOnly,
    },
    ReleasedMigrationDescriptor {
        id: "003_session_state_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "004_input_messages_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "005_input_messages_event_seq_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "006_transcript_items_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "007_transcript_items_event_range_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "008_tool_runs_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "009_tool_runs_status_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "010_projection_checkpoints_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "011_snapshots_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "012_runtime_work_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "013_runtime_work_status_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "014_runtime_work_parent_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "015_session_drafts_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "016_session_state_reasoning_effort_column",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "017_session_state_reasoning_summary_column",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "018_model_context_projection_state_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "019_model_context_entries_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "020_model_context_entries_event_type_index",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "021_artifact_references_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "022_context_occupancy_projection_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "023_reset_legacy_context_occupancy_projection",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "024_reset_request_context_occupancy_projection",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "025_turn_receipts_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "026_session_storage_contract_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "027_initialize_session_storage_contract",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "028_repair_context_occupancy_checkpoint_version",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::RetiredSuperseded,
    },
    ReleasedMigrationDescriptor {
        id: "028_session_compatibility_state",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "029_session_compatibility_issues",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "030_session_state_visibility_column",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "031_session_state_execution_provenance_column",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
    ReleasedMigrationDescriptor {
        id: "032_session_migration_receipts_table",
        domain: ReleasedMigrationDomain::Session,
        treatment: ReleasedMigrationTreatment::MaterializeCurrent,
    },
];

/// Treatment assigned to one retired persisted root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReleasedRootTreatment {
    /// Relocate an unambiguous session atomically into canonical storage.
    RelocateToCanonical,
}

/// One historical persisted root and its mandatory treatment.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedRootDescriptor {
    /// Root path relative to the state directory.
    pub path: &'static str,
    /// Writer epoch encoded by the historical root.
    pub source_writer_epoch: u32,
    /// Migration treatment for sessions discovered under the root.
    pub treatment: ReleasedRootTreatment,
}

/// Historical persisted roots observed in released storage layouts.
pub const RELEASED_HISTORICAL_ROOTS: &[ReleasedRootDescriptor] = &[ReleasedRootDescriptor {
    path: "session-storage/writer-epoch-2",
    source_writer_epoch: 2,
    treatment: ReleasedRootTreatment::RelocateToCanonical,
}];

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
pub const CURRENT_WRITER_EPOCH: u32 = bcode_session_migration_target::CURRENT_WRITER_EPOCH;

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

/// One released writer/schema combination observed together in Git history.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct ReleasedWriterSchemaDescriptor {
    /// Released session storage writer epoch.
    pub writer_epoch: u32,
    /// Released event schema emitted by that writer.
    pub event_schema: u16,
}

/// Exact writer/schema combinations observed across all available refs.
pub const RELEASED_WRITER_SCHEMA_COMBINATIONS: &[ReleasedWriterSchemaDescriptor] = &[
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 1,
        event_schema: 32,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 2,
        event_schema: 32,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 2,
        event_schema: 35,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 3,
        event_schema: 37,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 3,
        event_schema: 38,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 4,
        event_schema: 38,
    },
    ReleasedWriterSchemaDescriptor {
        writer_epoch: 4,
        event_schema: 39,
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
pub const CURRENT_EVENT_SCHEMA: u16 = bcode_session_migration_target::CURRENT_EVENT_SCHEMA;

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
    fn every_released_session_ledger_endpoint_has_one_valid_fixture_case() {
        let cases = released_session_ledger_prefix_fixture_cases();
        let expected = RELEASED_MIGRATION_IDS
            .iter()
            .filter(|migration| migration.domain == ReleasedMigrationDomain::Session)
            .map(|migration| migration.id)
            .collect::<BTreeSet<_>>();
        let actual = cases
            .iter()
            .map(|case| case.endpoint)
            .collect::<BTreeSet<_>>();
        assert_eq!(cases.len(), expected.len());
        assert_eq!(actual, expected);
        let materialized_ledger = cases
            .iter()
            .filter(|case| {
                case.endpoint_treatment == ReleasedMigrationTreatment::MaterializeCurrent
            })
            .max_by_key(|case| case.completed_migration_ids.len())
            .expect("materialized ledger")
            .completed_migration_ids
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        for case in &cases {
            crate::validate_released_ledger_prefix_fixture_case(case)
                .unwrap_or_else(|error| panic!("ledger fixture {}: {error}", case.endpoint));
            if case.endpoint_treatment == ReleasedMigrationTreatment::MaterializeCurrent {
                let validated = crate::validate_migration_ledger(&crate::MigrationLedgerFacts {
                    known_migration_ids: materialized_ledger.clone(),
                    completed_migration_ids: case
                        .completed_migration_ids
                        .iter()
                        .map(ToString::to_string)
                        .collect(),
                })
                .unwrap_or_else(|error| {
                    panic!("materialized ledger fixture {}: {error}", case.endpoint)
                });
                assert_eq!(
                    validated.completed_prefix_len,
                    case.completed_migration_ids.len()
                );
                assert_eq!(validated.current_migration_count, materialized_ledger.len());
            }
        }
    }

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
    fn released_event_schema_ranges_match_all_ref_inventory_boundaries() {
        for variant in RELEASED_EVENT_VARIANTS {
            let (first, last) = released_event_schema_range(variant.kind);
            assert!(is_released_historical_event_schema(first) || first == CURRENT_EVENT_SCHEMA);
            assert!(is_released_historical_event_schema(last) || last == CURRENT_EVENT_SCHEMA);
            assert!(first <= last);
            assert!(variant.supports_schema(first));
            assert!(variant.supports_schema(last));
            for schema in RELEASED_HISTORICAL_EVENT_SCHEMAS
                .iter()
                .copied()
                .chain(std::iter::once(CURRENT_EVENT_SCHEMA))
            {
                assert_eq!(
                    variant.supports_schema(schema),
                    schema >= first && schema <= last,
                    "schema membership for {} at {schema}",
                    variant.kind
                );
            }
        }
        assert!(
            RELEASED_EVENT_VARIANTS
                .iter()
                .find(|variant| variant.kind == "assistant_message")
                .expect("assistant message")
                .supports_schema(1)
        );
        assert!(
            !RELEASED_EVENT_VARIANTS
                .iter()
                .find(|variant| variant.kind == "tool_invocation_stream")
                .expect("tool invocation stream")
                .supports_schema(11)
        );
        assert!(
            !RELEASED_EVENT_VARIANTS
                .iter()
                .find(|variant| variant.kind == "plugin_automation_turn_started")
                .expect("plugin automation")
                .supports_schema(35)
        );
    }

    #[test]
    fn released_event_variant_treatments_are_sorted_unique_and_total() {
        assert!(
            RELEASED_EVENT_VARIANTS
                .windows(2)
                .all(|pair| pair[0].kind < pair[1].kind)
        );
        assert_eq!(RELEASED_EVENT_VARIANTS.len(), 60);
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
        assert_eq!(
            retired,
            [
                "interactive_tool_request_created",
                "interactive_tool_request_resolved",
                "legacy_event",
                "legacy_tool_invocation_presentation",
                "legacy_turn_finished",
                "legacy_turn_started",
                "plugin_automation_turn_finished",
                "plugin_automation_turn_started",
                "tool_invocation_presentation",
                "tool_invocation_stream",
            ],
            "every inventoried retired variant must be an explicit reviewed decision"
        );
    }

    #[test]
    fn released_historical_root_inventory_is_sorted_unique_and_exact() {
        assert!(
            RELEASED_HISTORICAL_ROOTS
                .windows(2)
                .all(|pair| pair[0].path < pair[1].path)
        );
        assert_eq!(
            RELEASED_HISTORICAL_ROOTS,
            [ReleasedRootDescriptor {
                path: "session-storage/writer-epoch-2",
                source_writer_epoch: 2,
                treatment: ReleasedRootTreatment::RelocateToCanonical,
            }]
        );
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
                .filter(|migration| {
                    migration.treatment == ReleasedMigrationTreatment::RetiredSuperseded
                })
                .map(|migration| migration.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "001_session_event_store_and_projections",
                "028_repair_context_occupancy_checkpoint_version",
            ])
        );
        assert_eq!(
            RELEASED_MIGRATION_IDS
                .iter()
                .filter(|migration| migration.treatment == ReleasedMigrationTreatment::GlobalOnly)
                .map(|migration| migration.id)
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([
                "001_global_catalog",
                "001_global_sessions_table",
                "002_global_sessions_updated_at_index",
                "003_global_composer_drafts_table",
            ])
        );
    }

    #[test]
    fn permanent_fixture_manifest_is_exhaustive_and_inventory_valid() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = load_released_fixture_manifest(&root).expect("fixture inventory");
        assert_eq!(manifest.format_version, 1);
        assert_eq!(
            released_fixture_writer_coverage(&manifest)
                .into_keys()
                .collect::<BTreeSet<_>>(),
            BTreeSet::from([1, 2, 3, 4])
        );
        assert_eq!(
            released_fixture_schema_coverage(&manifest).get(&28),
            Some(&2)
        );
        assert_eq!(
            released_fixture_authoritative_record_coverage(&manifest).get("session_drafts"),
            Some(&1)
        );
        let gaps = released_fixture_coverage_gaps(&manifest);
        assert!(gaps.is_empty(), "fixture coverage gaps: {gaps:?}");
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
                "covered_writer_schema_pairs": [
                    {"writer_epoch": 2, "event_schema": 28}
                ],
                "migration_ledger_endpoints": ["023_reset_legacy_context_occupancy_projection"],
                "covered_migration_ledger_prefixes": ["023_reset_legacy_context_occupancy_projection"],
                "event_schemas": [28],
                "migratable_store": true,
                "historical_payloads": true,
                "expected_event_count": 1,
                "expected_classifications": {
                    "converted": 0,
                    "retired_known": 0,
                    "current_passthrough": 1
                },
                "covered_event_kinds": ["session_created"],
                "covered_schema_event_pairs": [
                    {"event_schema": 28, "event_kind": "session_created"}
                ]
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

        let mut global_manifest = manifest.clone();
        global_manifest["fixtures"][0]["source_writer_epochs"] = serde_json::json!([2]);
        global_manifest["fixtures"][0]["migration_ledger_endpoints"] =
            serde_json::json!(["001_global_catalog"]);
        global_manifest["fixtures"][0]["covered_migration_ledger_prefixes"] =
            serde_json::json!(["001_global_catalog"]);
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&global_manifest).expect("JSON"),
        )
        .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::NonSessionMigrationEndpoint {
                migration_id,
                ..
            }) if migration_id == "001_global_catalog"
        ));

        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        let mut manifest = manifest;
        manifest["fixtures"][0]["source_writer_epochs"] = serde_json::json!([2]);
        manifest["fixtures"][0]["event_schemas"] = serde_json::json!([]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::MissingEventSchema(_))
        ));
        manifest["fixtures"][0]["event_schemas"] = serde_json::json!([28]);
        manifest["fixtures"][0]["covered_event_kinds"] = serde_json::json!(["not_released"]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::UnsupportedEventKind {
                event_kind,
                ..
            }) if event_kind == "not_released"
        ));

        manifest["fixtures"][0]["event_schemas"] = serde_json::json!([1]);
        manifest["fixtures"][0]["covered_writer_schema_pairs"] = serde_json::json!([{
            "writer_epoch": 2,
            "event_schema": 1
        }]);
        manifest["fixtures"][0]["covered_event_kinds"] =
            serde_json::json!(["tool_invocation_stream"]);
        manifest["fixtures"][0]["covered_schema_event_pairs"] = serde_json::json!([{
            "event_schema": 1,
            "event_kind": "tool_invocation_stream"
        }]);
        std::fs::write(&manifest_path, serde_json::to_vec(&manifest).expect("JSON"))
            .expect("manifest");
        assert!(matches!(
            load_released_fixture_manifest(root.path()),
            Err(ReleasedFixtureInventoryError::EventKindNotReleasedInSchema {
                event_schema: 1,
                event_kind,
                ..
            }) if event_kind == "tool_invocation_stream"
        ));
    }

    #[test]
    fn exact_fixture_coverage_rejects_cross_fixture_duplicates() {
        let descriptor = |path: &str| ReleasedFixtureDescriptor {
            path: PathBuf::from(path),
            source_writer_epochs: vec![2],
            covered_writer_schema_pairs: vec![ReleasedFixtureWriterSchemaPair {
                writer_epoch: 2,
                event_schema: 28,
            }],
            lifecycle_matrix: ReleasedFixtureLifecycleMatrix::default(),
            migration_ledger_endpoints: vec![],
            covered_migration_ledger_prefixes: vec![],
            event_schemas: vec![28],
            migratable_store: true,
            historical_payloads: true,
            expected_event_count: 1,
            expected_classifications: ReleasedFixtureClassificationCounts {
                converted: 0,
                retired_known: 0,
                current_passthrough: 1,
            },
            covered_event_kinds: vec!["session_created".to_owned()],
            covered_schema_event_pairs: vec![ReleasedFixtureSchemaEventPair {
                event_schema: 28,
                event_kind: "session_created".to_owned(),
            }],
            covered_authoritative_records: vec![],
            covered_roots: vec![],
            covered_root_writer_pairs: vec![],
            covered_tables: vec![],
            covered_table_treatments: vec![],
        };
        let duplicate = ReleasedFixtureManifest {
            format_version: 1,
            fixtures: vec![descriptor("stores/a.jsonl"), descriptor("stores/b.jsonl")],
        };
        assert!(matches!(
            validate_exact_fixture_coverage_unique(&duplicate),
            Err(ReleasedFixtureInventoryError::DuplicateExactCoverage {
                field: "writer_schema",
                ..
            })
        ));
    }

    #[test]
    fn released_writer_schema_combinations_are_sorted_unique_and_exact() {
        assert!(
            RELEASED_WRITER_SCHEMA_COMBINATIONS
                .windows(2)
                .all(|pair| pair[0] < pair[1])
        );
        assert_eq!(
            RELEASED_WRITER_SCHEMA_COMBINATIONS,
            [
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 1,
                    event_schema: 32,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 2,
                    event_schema: 32,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 2,
                    event_schema: 35,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 3,
                    event_schema: 37,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 3,
                    event_schema: 38,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 4,
                    event_schema: 38,
                },
                ReleasedWriterSchemaDescriptor {
                    writer_epoch: 4,
                    event_schema: 39,
                },
            ]
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
