//! Complete migration planning from released writer epochs to the current writer.

use crate::inventory::{
    CURRENT_WRITER_EPOCH, MIGRATION_STEPS, MigrationStepDescriptor, RELEASED_EVENT_VARIANTS,
    RELEASED_HISTORICAL_EVENT_SCHEMAS, RELEASED_HISTORICAL_ROOTS,
    RELEASED_HISTORICAL_WRITER_EPOCHS, RELEASED_RECORD_TREATMENTS, ReleasedEventTreatment,
    ReleasedFixtureCoverageGaps, ReleasedFixtureManifest, ReleasedRecordTreatment,
    ReleasedRootTreatment, released_fixture_coverage_gaps,
};
use thiserror::Error;

/// Failure to prove that released fixtures cover every mandatory migration dimension.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
#[error("released fixture coverage is incomplete")]
pub struct ReleasedFixtureCoverageError {
    /// Exact missing released inventory dimensions.
    pub gaps: Box<ReleasedFixtureCoverageGaps>,
}

/// Require complete released-format fixture coverage.
///
/// This includes every planned writer edge, writer/schema/event combination, migration-ledger
/// endpoint, and preserved authoritative record. Edge coverage follows the complete upgrade path
/// of each fixture; it is not an attestation that each intermediate writer produced a fixture.
/// Ledger-only formats may use migration-owned non-payload cases rather than JSONL event fixtures.
///
/// # Errors
///
/// Returns exact missing dimensions when the fixture inventory is incomplete.
pub fn validate_released_fixture_coverage(
    manifest: &ReleasedFixtureManifest,
) -> Result<(), ReleasedFixtureCoverageError> {
    let gaps = released_fixture_coverage_gaps(manifest);
    if gaps.is_empty() {
        Ok(())
    } else {
        Err(ReleasedFixtureCoverageError {
            gaps: Box::new(gaps),
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedEventTreatmentRow {
    /// Stable serde event-kind name.
    pub kind: &'static str,
    /// Required migration treatment.
    pub treatment: ReleasedEventTreatment,
}

/// One complete treatment row for a released persisted table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedRecordTreatmentRow {
    /// Durable table identity.
    pub table: &'static str,
    /// Required migration treatment.
    pub treatment: ReleasedRecordTreatment,
}

/// One complete treatment row for a released persisted root.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ReleasedRootTreatmentRow {
    /// Root path relative to the state directory.
    pub path: &'static str,
    /// Required migration treatment.
    pub treatment: ReleasedRootTreatment,
}

/// Complete released inventory treatments that do not vary by writer/schema matrix row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedInventoryTreatments {
    /// Historical root treatments.
    pub root_treatments: Vec<ReleasedRootTreatmentRow>,
}

/// Return complete treatments for released inventory dimensions outside writer/schema rows.
#[must_use]
pub fn released_inventory_treatments() -> ReleasedInventoryTreatments {
    ReleasedInventoryTreatments {
        root_treatments: RELEASED_HISTORICAL_ROOTS
            .iter()
            .map(|root| ReleasedRootTreatmentRow {
                path: root.path,
                treatment: root.treatment,
            })
            .collect(),
    }
}

/// One complete writer/schema migration-matrix row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleasedFormatMigrationMatrixRow {
    /// Released source writer epoch.
    pub source_writer_epoch: u32,
    /// Released historical event schema.
    pub event_schema: u16,
    /// Ordered writer transitions ending at the current writer.
    pub migration_step_ids: Vec<&'static str>,
    /// Event treatments required by the released inventory.
    pub event_treatments: Vec<ReleasedEventTreatmentRow>,
    /// Non-event record treatments required by the released inventory.
    pub record_treatments: Vec<ReleasedRecordTreatmentRow>,
}

/// Build the complete persistent writer/schema migration matrix.
///
/// Every row is guaranteed to end at the current writer because rows are produced only after
/// resolving a complete monotonic writer plan.
///
/// # Errors
///
/// Returns an error if any released writer lacks a complete path to the current writer.
pub fn released_format_migration_matrix()
-> Result<Vec<ReleasedFormatMigrationMatrixRow>, MigrationPlanError> {
    let mut matrix = Vec::with_capacity(
        RELEASED_HISTORICAL_WRITER_EPOCHS.len() * RELEASED_HISTORICAL_EVENT_SCHEMAS.len(),
    );
    for source_writer_epoch in RELEASED_HISTORICAL_WRITER_EPOCHS {
        let plan = plan_writer_epoch_migration(*source_writer_epoch)?;
        for event_schema in RELEASED_HISTORICAL_EVENT_SCHEMAS {
            matrix.push(ReleasedFormatMigrationMatrixRow {
                source_writer_epoch: *source_writer_epoch,
                event_schema: *event_schema,
                migration_step_ids: plan.steps.iter().map(|step| step.id).collect(),
                event_treatments: RELEASED_EVENT_VARIANTS
                    .iter()
                    .filter(|variant| variant.supports_schema(*event_schema))
                    .map(|variant| ReleasedEventTreatmentRow {
                        kind: variant.kind,
                        treatment: variant.treatment,
                    })
                    .collect(),
                record_treatments: RELEASED_RECORD_TREATMENTS
                    .iter()
                    .map(|record| ReleasedRecordTreatmentRow {
                        table: record.table,
                        treatment: record.treatment,
                    })
                    .collect(),
            });
        }
    }
    Ok(matrix)
}

/// Complete ordered writer migration selected for one session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationPlan {
    /// Writer epoch observed before migration.
    pub source_writer_epoch: u32,
    /// Writer epoch required by this build.
    pub target_writer_epoch: u32,
    /// Ordered monotonic transitions from source to target.
    pub steps: Vec<MigrationStepDescriptor>,
}

/// Failure to resolve a safe writer migration path.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum MigrationPlanError {
    /// The store was written by a future writer and cannot be downgraded.
    #[error(
        "session writer epoch {source_writer_epoch} is newer than current epoch {current_writer_epoch}"
    )]
    FutureWriter {
        /// Writer epoch found in storage.
        source_writer_epoch: u32,
        /// Current writer epoch supported by this build.
        current_writer_epoch: u32,
    },
    /// The source belongs to an unpublished, reserved writer contract.
    #[error("session writer epoch {writer_epoch} is reserved and unsupported")]
    ReservedWriter {
        /// Reserved writer observed in storage.
        writer_epoch: u32,
    },
    /// The migration graph has no edge from a required writer epoch.
    #[error(
        "no session migration step from writer epoch {writer_epoch} to target epoch {target_writer_epoch}"
    )]
    MissingStep {
        /// Writer epoch with no registered transition.
        writer_epoch: u32,
        /// Requested final epoch.
        target_writer_epoch: u32,
    },
    /// A registered step does not strictly advance the writer epoch.
    #[error(
        "invalid session migration step {step_id}: epoch {source_writer_epoch} does not advance to {target_writer_epoch}"
    )]
    NonMonotonicStep {
        /// Stable step identity.
        step_id: &'static str,
        /// Step source epoch.
        source_writer_epoch: u32,
        /// Step target epoch.
        target_writer_epoch: u32,
    },
}

/// Stateless migration planner used by server composition.
#[derive(Debug, Clone, Copy, Default)]
pub struct MigrationPlanService;

impl MigrationPlanService {
    /// Resolve the complete migration plan for a source writer epoch.
    ///
    /// # Errors
    ///
    /// Returns an error when no safe monotonic path reaches the current writer.
    pub fn plan(self, source_writer_epoch: u32) -> Result<MigrationPlan, MigrationPlanError> {
        plan_writer_epoch_migration(source_writer_epoch)
    }
}

/// Resolve a complete monotonic migration path against an explicit inventory and target.
///
/// # Errors
///
/// Returns an error for future sources, missing edges, or non-monotonic steps.
pub fn plan_writer_epoch_migration_with_registry(
    source_writer_epoch: u32,
    target_writer_epoch: u32,
    steps_registry: &[MigrationStepDescriptor],
) -> Result<MigrationPlan, MigrationPlanError> {
    if source_writer_epoch == 24 || target_writer_epoch == 24 {
        return Err(MigrationPlanError::ReservedWriter { writer_epoch: 24 });
    }
    if source_writer_epoch > target_writer_epoch {
        return Err(MigrationPlanError::FutureWriter {
            source_writer_epoch,
            current_writer_epoch: target_writer_epoch,
        });
    }

    let mut writer_epoch = source_writer_epoch;
    let mut steps = Vec::new();
    while writer_epoch < target_writer_epoch {
        let step = steps_registry
            .iter()
            .find(|step| step.source_writer_epoch == writer_epoch)
            .copied()
            .ok_or(MigrationPlanError::MissingStep {
                writer_epoch,
                target_writer_epoch,
            })?;
        if step.target_writer_epoch <= step.source_writer_epoch {
            return Err(MigrationPlanError::NonMonotonicStep {
                step_id: step.id,
                source_writer_epoch: step.source_writer_epoch,
                target_writer_epoch: step.target_writer_epoch,
            });
        }
        if step.target_writer_epoch > target_writer_epoch || step.target_writer_epoch == 24 {
            return Err(MigrationPlanError::MissingStep {
                writer_epoch,
                target_writer_epoch,
            });
        }
        writer_epoch = step.target_writer_epoch;
        steps.push(step);
    }

    Ok(MigrationPlan {
        source_writer_epoch,
        target_writer_epoch,
        steps,
    })
}

/// Resolve the complete monotonic migration path from `source_writer_epoch`.
///
/// # Errors
///
/// Returns an error when the source is newer than this build, a required edge is not registered,
/// or a registered edge is non-monotonic.
pub fn plan_writer_epoch_migration(
    source_writer_epoch: u32,
) -> Result<MigrationPlan, MigrationPlanError> {
    plan_writer_epoch_migration_with_registry(
        source_writer_epoch,
        CURRENT_WRITER_EPOCH,
        &MIGRATION_STEPS,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_model_edge_skips_reserved_writer_and_preserves_all_sources() {
        let candidate = MigrationStepDescriptor {
            id: "session-writer-epoch-23-to-25",
            source_writer_epoch: 23,
            target_writer_epoch: 25,
        };
        for source in RELEASED_HISTORICAL_WRITER_EPOCHS
            .iter()
            .filter(|source| **source < 25)
        {
            let plan = plan_writer_epoch_migration_with_registry(*source, 25, &MIGRATION_STEPS)
                .expect("supported source reaches selected-model writer");
            assert_eq!(plan.steps.last(), Some(&candidate));
            assert!(
                plan.steps.iter().all(|step| {
                    step.source_writer_epoch != 24 && step.target_writer_epoch != 24
                })
            );
            assert!(
                plan.steps
                    .windows(2)
                    .all(|pair| { pair[0].target_writer_epoch == pair[1].source_writer_epoch })
            );
        }
        let plan = plan_writer_epoch_migration_with_registry(23, 25, &MIGRATION_STEPS).unwrap();
        assert_eq!(plan.steps, [candidate]);
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(24, 25, &MIGRATION_STEPS),
            Err(MigrationPlanError::ReservedWriter { writer_epoch: 24 })
        ));
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(23, 24, &MIGRATION_STEPS),
            Err(MigrationPlanError::ReservedWriter { writer_epoch: 24 })
        ));
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(25, 23, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter { .. })
        ));
    }

    #[test]
    fn native_parity_edge_extends_every_released_source() {
        let parity = MigrationStepDescriptor {
            id: "session-writer-epoch-25-to-26",
            source_writer_epoch: 25,
            target_writer_epoch: 26,
        };
        for source in RELEASED_HISTORICAL_WRITER_EPOCHS
            .iter()
            .filter(|source| **source < 26)
        {
            let plan = plan_writer_epoch_migration_with_registry(*source, 26, &MIGRATION_STEPS)
                .expect("supported source reaches native-parity writer");
            assert_eq!(plan.steps.last(), Some(&parity));
        }
        assert_eq!(
            plan_writer_epoch_migration_with_registry(25, 26, &MIGRATION_STEPS)
                .unwrap()
                .steps,
            [parity]
        );
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(26, 25, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter { .. })
        ));
    }

    #[test]
    fn native_usage_projection_edge_extends_every_released_source() {
        let usage = MigrationStepDescriptor {
            id: "session-writer-epoch-26-to-27",
            source_writer_epoch: 26,
            target_writer_epoch: 27,
        };
        for source in RELEASED_HISTORICAL_WRITER_EPOCHS
            .iter()
            .filter(|source| **source < 27)
        {
            let plan = plan_writer_epoch_migration_with_registry(*source, 27, &MIGRATION_STEPS)
                .expect("supported source reaches native-usage projection writer");
            assert_eq!(plan.steps.last(), Some(&usage));
        }
        assert_eq!(
            plan_writer_epoch_migration_with_registry(26, 27, &MIGRATION_STEPS)
                .unwrap()
                .steps,
            [usage]
        );
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(27, 26, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter { .. })
        ));
    }

    #[test]
    fn account_reference_edge_extends_every_released_source() {
        let account = MigrationStepDescriptor {
            id: "session-writer-epoch-27-to-28",
            source_writer_epoch: 27,
            target_writer_epoch: 28,
        };
        for source in RELEASED_HISTORICAL_WRITER_EPOCHS {
            let plan = plan_writer_epoch_migration_with_registry(*source, 28, &MIGRATION_STEPS)
                .expect("supported source reaches account-reference writer");
            assert_eq!(plan.steps.last(), Some(&account));
        }
        assert_eq!(
            plan_writer_epoch_migration_with_registry(27, 28, &MIGRATION_STEPS)
                .unwrap()
                .steps,
            [account]
        );
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(28, 27, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter { .. })
        ));
    }

    #[test]
    fn every_unreserved_prior_writer_is_released_and_reaches_current() {
        const RESERVED_WRITER_EPOCHS: &[u32] = &[24];
        for epoch in 1..CURRENT_WRITER_EPOCH {
            if RESERVED_WRITER_EPOCHS.contains(&epoch) {
                assert!(!RELEASED_HISTORICAL_WRITER_EPOCHS.contains(&epoch));
                continue;
            }
            assert!(
                RELEASED_HISTORICAL_WRITER_EPOCHS.contains(&epoch),
                "writer epoch {epoch} precedes the current writer but is neither released nor reserved"
            );
            let plan = plan_writer_epoch_migration(epoch)
                .unwrap_or_else(|error| panic!("writer epoch {epoch} cannot migrate: {error}"));
            assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
        }
    }

    #[test]
    fn fixture_release_gate_accepts_complete_exact_coverage() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixtures");
        let manifest = crate::load_released_fixture_manifest(&root).expect("fixture manifest");
        validate_released_fixture_coverage(&manifest)
            .expect("permanent fixtures must cover every released inventory dimension");
    }

    #[test]
    fn released_format_matrix_is_complete_unique_and_current_writable() {
        let matrix = released_format_migration_matrix().expect("released matrix");
        assert_eq!(
            matrix.len(),
            RELEASED_HISTORICAL_WRITER_EPOCHS.len() * RELEASED_HISTORICAL_EVENT_SCHEMAS.len()
        );
        let identities = matrix
            .iter()
            .map(|row| (row.source_writer_epoch, row.event_schema))
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(identities.len(), matrix.len());
        for row in matrix {
            assert_eq!(
                row.event_treatments.len(),
                RELEASED_EVENT_VARIANTS
                    .iter()
                    .filter(|variant| variant.supports_schema(row.event_schema))
                    .count()
            );
            assert_eq!(
                row.record_treatments.len(),
                RELEASED_RECORD_TREATMENTS.len()
            );
            assert_eq!(
                row.event_treatments
                    .iter()
                    .map(|treatment| treatment.kind)
                    .collect::<Vec<_>>(),
                RELEASED_EVENT_VARIANTS
                    .iter()
                    .filter(|variant| variant.supports_schema(row.event_schema))
                    .map(|variant| variant.kind)
                    .collect::<Vec<_>>()
            );
            assert_eq!(
                row.record_treatments
                    .iter()
                    .map(|treatment| treatment.table)
                    .collect::<Vec<_>>(),
                RELEASED_RECORD_TREATMENTS
                    .iter()
                    .map(|record| record.table)
                    .collect::<Vec<_>>()
            );
            assert_ne!(row.migration_step_ids, [] as [&str; 0]);
            let plan = plan_writer_epoch_migration(row.source_writer_epoch).expect("writer plan");
            assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
            assert_eq!(
                row.migration_step_ids,
                plan.steps.iter().map(|step| step.id).collect::<Vec<_>>()
            );
        }
        let roots = released_inventory_treatments();
        assert_eq!(
            roots.root_treatments,
            [ReleasedRootTreatmentRow {
                path: "session-storage/writer-epoch-2",
                treatment: ReleasedRootTreatment::RelocateToCanonical,
            }]
        );
    }

    #[test]
    fn every_released_writer_epoch_has_a_complete_monotonic_plan() {
        for source_writer_epoch in crate::inventory::RELEASED_HISTORICAL_WRITER_EPOCHS
            .iter()
            .copied()
            .chain(std::iter::once(CURRENT_WRITER_EPOCH))
        {
            let plan = plan_writer_epoch_migration(source_writer_epoch)
                .expect("released writer epoch should have a migration plan");
            assert_eq!(plan.source_writer_epoch, source_writer_epoch);
            assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
            let mut expected_source = source_writer_epoch;
            for step in plan.steps {
                assert_eq!(step.source_writer_epoch, expected_source);
                assert!(step.target_writer_epoch > step.source_writer_epoch);
                expected_source = step.target_writer_epoch;
            }
            assert_eq!(expected_source, CURRENT_WRITER_EPOCH);
        }
    }

    #[test]
    fn explicit_registry_rejects_missing_and_non_monotonic_edges() {
        let missing = [MigrationStepDescriptor {
            id: "one-to-two",
            source_writer_epoch: 1,
            target_writer_epoch: 2,
        }];
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(1, 3, &missing),
            Err(MigrationPlanError::MissingStep {
                writer_epoch: 2,
                target_writer_epoch: 3,
            })
        ));

        let non_monotonic = [MigrationStepDescriptor {
            id: "stalled",
            source_writer_epoch: 1,
            target_writer_epoch: 1,
        }];
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(1, 2, &non_monotonic),
            Err(MigrationPlanError::NonMonotonicStep {
                step_id: "stalled",
                source_writer_epoch: 1,
                target_writer_epoch: 1,
            })
        ));
    }

    #[test]
    fn writer_epoch_4_requires_complete_migration_to_current_epoch() {
        let plan = plan_writer_epoch_migration(4).expect("epoch 4 corrective plan");
        let expected_edges = (4..23)
            .map(|source| (source, source + 1))
            .chain([(23, 25), (25, 26), (26, 27), (27, 28)])
            .collect::<Vec<_>>();
        assert_eq!(
            plan.steps
                .iter()
                .map(|step| (step.source_writer_epoch, step.target_writer_epoch))
                .collect::<Vec<_>>(),
            expected_edges
        );
        assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
        assert_eq!(plan.steps[0].id, "session-writer-epoch-4-to-5");
        assert_eq!(plan.steps[1].id, "session-writer-epoch-5-to-6");
        assert_eq!(plan.steps[2].id, "session-writer-epoch-6-to-7");
    }

    #[test]
    fn compaction_checkpoint_epoch_has_a_lossless_forward_only_path() {
        let plan = plan_writer_epoch_migration(16).expect("epoch 16 projection upgrade");
        assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
        assert_eq!(
            plan.steps,
            [
                MigrationStepDescriptor {
                    id: "session-writer-epoch-16-to-17",
                    source_writer_epoch: 16,
                    target_writer_epoch: 17,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-17-to-18",
                    source_writer_epoch: 17,
                    target_writer_epoch: 18,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-18-to-19",
                    source_writer_epoch: 18,
                    target_writer_epoch: 19,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-19-to-20",
                    source_writer_epoch: 19,
                    target_writer_epoch: 20,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-20-to-21",
                    source_writer_epoch: 20,
                    target_writer_epoch: 21,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-21-to-22",
                    source_writer_epoch: 21,
                    target_writer_epoch: 22,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-22-to-23",
                    source_writer_epoch: 22,
                    target_writer_epoch: 23,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-23-to-25",
                    source_writer_epoch: 23,
                    target_writer_epoch: 25,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-25-to-26",
                    source_writer_epoch: 25,
                    target_writer_epoch: 26,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-26-to-27",
                    source_writer_epoch: 26,
                    target_writer_epoch: 27,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-27-to-28",
                    source_writer_epoch: 27,
                    target_writer_epoch: 28,
                }
            ]
        );
        // An older artifact's inventory must not reinterpret the new writer contract.
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(17, 16, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter {
                source_writer_epoch: 17,
                current_writer_epoch: 16,
            })
        ));
    }

    #[test]
    fn external_backend_epoch_has_a_lossless_forward_only_projection_upgrade() {
        let plan = plan_writer_epoch_migration(17).expect("external locator projection upgrade");
        assert_eq!(plan.target_writer_epoch, CURRENT_WRITER_EPOCH);
        assert_eq!(
            plan.steps,
            [
                MigrationStepDescriptor {
                    id: "session-writer-epoch-17-to-18",
                    source_writer_epoch: 17,
                    target_writer_epoch: 18,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-18-to-19",
                    source_writer_epoch: 18,
                    target_writer_epoch: 19,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-19-to-20",
                    source_writer_epoch: 19,
                    target_writer_epoch: 20,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-20-to-21",
                    source_writer_epoch: 20,
                    target_writer_epoch: 21,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-21-to-22",
                    source_writer_epoch: 21,
                    target_writer_epoch: 22,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-22-to-23",
                    source_writer_epoch: 22,
                    target_writer_epoch: 23,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-23-to-25",
                    source_writer_epoch: 23,
                    target_writer_epoch: 25,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-25-to-26",
                    source_writer_epoch: 25,
                    target_writer_epoch: 26,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-26-to-27",
                    source_writer_epoch: 26,
                    target_writer_epoch: 27,
                },
                MigrationStepDescriptor {
                    id: "session-writer-epoch-27-to-28",
                    source_writer_epoch: 27,
                    target_writer_epoch: 28,
                },
            ]
        );
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(18, 17, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter {
                source_writer_epoch: 18,
                current_writer_epoch: 17
            })
        ));
    }

    #[test]
    fn terminal_evidence_epoch_has_an_explicit_forward_only_step() {
        let plan = plan_writer_epoch_migration_with_registry(18, 19, &MIGRATION_STEPS)
            .expect("terminal evidence upgrade");
        assert_eq!(plan.target_writer_epoch, 19);
        assert_eq!(
            plan.steps,
            [MigrationStepDescriptor {
                id: "session-writer-epoch-18-to-19",
                source_writer_epoch: 18,
                target_writer_epoch: 19,
            }]
        );
        assert!(matches!(
            plan_writer_epoch_migration_with_registry(19, 18, &MIGRATION_STEPS),
            Err(MigrationPlanError::FutureWriter {
                source_writer_epoch: 19,
                current_writer_epoch: 18,
            })
        ));
    }

    #[test]
    fn current_writer_has_an_empty_plan() {
        let plan = plan_writer_epoch_migration(CURRENT_WRITER_EPOCH).expect("current plan");
        assert_eq!(
            plan.steps,
            [] as [crate::inventory::MigrationStepDescriptor; 0]
        );
    }

    #[test]
    fn zero_epoch_is_rejected_as_an_unreleased_missing_edge() {
        assert!(matches!(
            plan_writer_epoch_migration(0),
            Err(MigrationPlanError::MissingStep {
                writer_epoch: 0,
                target_writer_epoch: CURRENT_WRITER_EPOCH,
            })
        ));
    }

    #[test]
    fn future_writer_is_never_downgraded() {
        assert!(matches!(
            plan_writer_epoch_migration(CURRENT_WRITER_EPOCH + 1),
            Err(MigrationPlanError::FutureWriter { .. })
        ));
    }
}
