//! Historical migration orchestration against the policy-free current target API.

use bcode_session_migration_target::{MigrationTarget, StrictValidation};

/// Failure while executing migration against a current target implementation.
#[derive(Debug, thiserror::Error)]
pub enum MigrationTargetExecutionError<E> {
    /// A current target operation failed.
    #[error("current migration target operation failed: {0}")]
    Target(E),
    /// Strict current validation failed after projector finalization.
    #[error("strict current migration target validation failed: {0}")]
    Validation(#[from] crate::MigrationTargetValidationError),
}

/// Validate target facts using migration-owned strict policy.
///
/// # Errors
///
/// Returns an error when any current projection is absent, stale, incompatible, or unresolved.
pub fn validate_strict_target(
    validation: &StrictValidation,
) -> Result<(), crate::MigrationTargetValidationError> {
    crate::validate_migration_target(&crate::MigrationTargetValidation {
        canonical_tail: validation.canonical_tail,
        projections: validation
            .projections
            .iter()
            .map(|projection| crate::MigrationProjectionValidation {
                projection: projection.projection.clone(),
                actual_schema_version: projection.actual_schema_version,
                expected_schema_version: projection.expected_schema_version,
                checkpoint: projection.checkpoint,
            })
            .collect(),
        model_context_schema_version: validation.model_context_schema_version,
        expected_model_context_schema_version: validation.expected_model_context_schema_version,
        model_context_checkpoint: validation.model_context_checkpoint,
    })
}

/// Finalize an already-normalized target only after strict current validation.
///
/// Historical normalization and projector ingestion occur before this boundary. This helper owns
/// the ordering invariant that projector finalization precedes strict validation and writer
/// finalization occurs last.
///
/// # Errors
///
/// Returns an error when projector finalization, strict validation, or writer finalization fails.
pub async fn finalize_validated_target<T: MigrationTarget>(
    target: &mut T,
    canonical_tail: Option<u64>,
) -> Result<(), MigrationTargetExecutionError<T::Error>> {
    target
        .finalize_projectors(canonical_tail)
        .await
        .map_err(MigrationTargetExecutionError::Target)?;
    let validation = target
        .validate_strict_current()
        .await
        .map_err(MigrationTargetExecutionError::Target)?;
    validate_strict_target(&validation)?;
    target
        .finalize_writer_contract()
        .await
        .map_err(MigrationTargetExecutionError::Target)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use bcode_session_migration_target::{CanonicalRow, MigrationReceipt, ProjectionValidation};
    use bcode_session_models::SessionEvent;

    #[derive(Debug, thiserror::Error)]
    #[error("target failure")]
    struct TargetError;

    #[derive(Default)]
    struct RecordingTarget {
        calls: Vec<&'static str>,
        interrupt_projectors: bool,
        stale_evidence: bool,
    }

    #[async_trait]
    impl MigrationTarget for RecordingTarget {
        type Error = TargetError;

        async fn materialize_current_schema(&mut self) -> Result<(), Self::Error> {
            self.calls.push("schema");
            Ok(())
        }

        async fn canonical_page(
            &mut self,
            _start_sequence: u64,
            _limit: usize,
        ) -> Result<Vec<CanonicalRow>, Self::Error> {
            Ok(Vec::new())
        }

        async fn replace_canonical_row(&mut self, _row: CanonicalRow) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn write_authoritative_state(
            &mut self,
            _context_epoch: u64,
            _context_occupancy_json: Option<String>,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn ingest_projectors(&mut self, _event: &SessionEvent) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn finalize_projectors(
            &mut self,
            _canonical_tail: Option<u64>,
        ) -> Result<(), Self::Error> {
            self.calls.push("finalize_projectors");
            if self.interrupt_projectors {
                return Err(TargetError);
            }
            Ok(())
        }

        async fn validate_strict_current(&mut self) -> Result<StrictValidation, Self::Error> {
            self.calls.push("validate");
            Ok(StrictValidation {
                canonical_tail: Some(7),
                projections: vec![ProjectionValidation {
                    projection: "turn_evidence".to_owned(),
                    actual_schema_version: Some(1),
                    expected_schema_version: 1,
                    checkpoint: if self.stale_evidence {
                        Some(6)
                    } else {
                        Some(7)
                    },
                }],
                model_context_schema_version: Some(2),
                expected_model_context_schema_version: 2,
                model_context_checkpoint: Some(7),
            })
        }

        async fn persist_migration_receipt(
            &mut self,
            _receipt: &MigrationReceipt,
        ) -> Result<(), Self::Error> {
            Ok(())
        }

        async fn finalize_writer_contract(&mut self) -> Result<(), Self::Error> {
            self.calls.push("writer");
            Ok(())
        }
    }

    #[tokio::test]
    async fn interrupted_evidence_rebuild_never_advances_writer_and_can_retry() {
        let mut target = RecordingTarget {
            interrupt_projectors: true,
            ..RecordingTarget::default()
        };
        assert!(matches!(
            finalize_validated_target(&mut target, Some(7)).await,
            Err(MigrationTargetExecutionError::Target(_))
        ));
        assert_eq!(target.calls, ["finalize_projectors"]);
        target.calls.clear();
        target.interrupt_projectors = false;
        finalize_validated_target(&mut target, Some(7))
            .await
            .expect("retry");
        assert_eq!(target.calls, ["finalize_projectors", "validate", "writer"]);
    }

    #[tokio::test]
    async fn stale_evidence_rebuild_never_advances_writer() {
        let mut target = RecordingTarget {
            stale_evidence: true,
            ..RecordingTarget::default()
        };
        assert!(matches!(
            finalize_validated_target(&mut target, Some(7)).await,
            Err(MigrationTargetExecutionError::Validation(
                crate::MigrationTargetValidationError::ProjectionStale { .. }
            ))
        ));
        assert_eq!(target.calls, ["finalize_projectors", "validate"]);
    }

    #[tokio::test]
    async fn writer_finalization_occurs_only_after_strict_current_validation() {
        let mut target = RecordingTarget::default();
        finalize_validated_target(&mut target, Some(7))
            .await
            .expect("strict finalization");
        assert_eq!(target.calls, ["finalize_projectors", "validate", "writer"]);
    }
}
