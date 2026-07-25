//! Released session-format inventory and migration-edge declarations.

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
