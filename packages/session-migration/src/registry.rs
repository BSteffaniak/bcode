use thiserror::Error;

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

const MIGRATION_STEPS: [MigrationStepDescriptor; 4] = [
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
    /// The store was written by a newer Bcode build and cannot be downgraded.
    #[error(
        "session writer epoch {source_writer_epoch} is newer than supported epoch {current_writer_epoch}"
    )]
    FutureWriter {
        /// Writer epoch recorded by the store.
        source_writer_epoch: u32,
        /// Writer epoch supported by this build.
        current_writer_epoch: u32,
    },
    /// No registered transition begins at the required epoch.
    #[error(
        "no session migration step continues from writer epoch {writer_epoch} toward epoch {target_writer_epoch}"
    )]
    MissingStep {
        /// Epoch where planning stopped.
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

/// Resolve a complete monotonic migration path against an explicit registry and target.
///
/// This is primarily useful for validating a registry before any migration uses it.
///
/// # Errors
///
/// Returns an error for future sources, missing edges, or non-monotonic steps.
pub fn plan_writer_epoch_migration_with_registry(
    source_writer_epoch: u32,
    target_writer_epoch: u32,
    steps_registry: &[MigrationStepDescriptor],
) -> Result<MigrationPlan, MigrationPlanError> {
    if source_writer_epoch > target_writer_epoch {
        return Err(MigrationPlanError::FutureWriter {
            source_writer_epoch,
            current_writer_epoch: target_writer_epoch,
        });
    }

    let mut steps = Vec::new();
    let mut writer_epoch = source_writer_epoch;
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
/// Returns an error when the source is newer than this build, a required edge
/// is not registered, or a registered edge is non-monotonic.
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
    fn every_released_writer_epoch_has_a_complete_monotonic_plan() {
        for source_writer_epoch in 1..=CURRENT_WRITER_EPOCH {
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
    fn writer_epoch_4_requires_corrective_epoch_5_migration() {
        let plan = plan_writer_epoch_migration(4).expect("epoch 4 corrective plan");
        assert_eq!(plan.steps.len(), 1);
        assert_eq!(plan.steps[0].id, "session-writer-epoch-4-to-5");
    }

    #[test]
    fn current_writer_has_an_empty_plan() {
        let plan = plan_writer_epoch_migration(CURRENT_WRITER_EPOCH).expect("current plan");
        assert!(plan.steps.is_empty());
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
