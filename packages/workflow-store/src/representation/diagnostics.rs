//! Bounded explanatory admission observations; never authorization or a dispatch decision.
use super::attempts::admission::FreshLeafAdmission;
use super::{Database, RepresentationError as Error, scheduling};

/// Verified reason a fresh attempt cannot be admitted at the observed instant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Blocker {
    /// Shared fresh-leaf admission predicate rejected this candidate.
    Admission(super::attempts::admission::FreshLeafBlock),
    /// Local run's canonical occupied admission slots meet its explicit cap.
    Capacity { occupied: u64, limit: u32 },
    /// Exact conflicting normalized claim in this run's reservation scope.
    Resource { resource: String },
    /// Explicit user pause, not merely a paused durable lifecycle.
    UserPaused,
    /// Durable cancellation intent.
    Cancellation,
    /// A canonical control failure prevents admission.
    ControlFailure,
    /// Verified recovery barrier prevents new admission.
    Recovery,
}

/// One exact activation's bounded reasons. Empty reasons never establish readiness.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActivationExplanation {
    pub node_id: String,
    pub activation_id: String,
    pub blockers: Vec<Blocker>,
    /// This exact activation was materialized by canonical dependency construction.
    /// Does not establish readiness of controller members or nodes without activations.
    pub dependency_inputs_materialized: bool,
    /// At least one necessary admission predicate was not inspected or is unsupported.
    pub incomplete: bool,
}

/// One database snapshot, reusing canonical scheduling observations without duplicating counts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiagnosticSnapshot {
    pub scheduling: scheduling::SchedulingSnapshot,
    pub explanations: Vec<ActivationExplanation>,
}

/// Read canonical scheduling and bounded admission explanations in one read transaction.
/// At most eight claims per activation are inspected; omitted predicates remain explicit.
/// # Errors
/// Rejects malformed state, stale cursors, invalid bounds and canonical predicate failures.
pub async fn snapshot(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    after: Option<&str>,
    limit: u16,
) -> Result<DiagnosticSnapshot, Error> {
    snapshot_at(db, run, expected_revision, after, limit, now_ms()?).await
}

fn now_ms() -> Result<u64, Error> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()
        .and_then(|duration| u64::try_from(duration.as_millis()).ok())
        .ok_or(Error::MaintenanceRequired)
}

/// Observe fresh-leaf admission at an explicit clock instant without consuming allowances.
/// # Errors
/// Same failures as snapshot; caller must use a trustworthy current clock for live observations.
pub async fn snapshot_at(
    db: &dyn Database,
    run: &str,
    expected_revision: Option<u64>,
    after: Option<&str>,
    limit: u16,
    now_ms: u64,
) -> Result<DiagnosticSnapshot, Error> {
    let tx = db.begin_transaction().await?;
    let scheduling = scheduling::snapshot_in(&*tx, run, expected_revision, after, limit).await?;
    let barriers = super::safety::admission_blocks(&*tx, run).await?;
    let mut explanations = Vec::with_capacity(scheduling.activations.len());
    for activation in &scheduling.activations {
        let mut explanation = ActivationExplanation {
            node_id: activation.node_id.clone(),
            activation_id: activation.activation_id.clone(),
            dependency_inputs_materialized: true,
            blockers: vec![],
            incomplete: true,
        };
        if activation.status == scheduling::ActivationState::Unresolved
            && activation.latest_attempt.is_none()
        {
            if let Some(reason) = capacity_blocker(&scheduling, activation) {
                explanation.blockers.push(reason);
            }
            for (blocked, reason) in [
                (barriers.user_paused, Blocker::UserPaused),
                (barriers.cancelled, Blocker::Cancellation),
                (barriers.control_failure, Blocker::ControlFailure),
                (barriers.recovery, Blocker::Recovery),
            ] {
                if blocked {
                    explanation.blockers.push(reason);
                }
            }
            let node = super::graph::execution_node(&*tx, run, &activation.node_id).await?;
            for claim in node.resources.iter().take(8) {
                if let Some(resource) =
                    super::safety::conflicting_resource(&*tx, run, claim).await?
                {
                    explanation.blockers.push(Blocker::Resource { resource });
                }
            }
            match super::attempts::admission::fresh_leaf_in(
                &*tx,
                run,
                &activation.node_id,
                &activation.activation_id,
                now_ms,
            )
            .await?
            {
                FreshLeafAdmission::Ready => explanation.incomplete = false,
                FreshLeafAdmission::Blocked(reason) => {
                    explanation.blockers.push(Blocker::Admission(reason));
                }
                FreshLeafAdmission::Unknown(_) => {}
            }
            // Materialized activation inputs already satisfied construction-time dependencies.
            // Pending controller dependencies and provider backpressure are not represented by
            // that fact; never manufacture complete eligibility from absence of these blockers.
        }
        explanations.push(explanation);
    }
    tx.commit().await?;
    Ok(DiagnosticSnapshot {
        scheduling,
        explanations,
    })
}

fn capacity_blocker(
    snapshot: &scheduling::SchedulingSnapshot,
    activation: &scheduling::ActivationObservation,
) -> Option<Blocker> {
    // Existing reservations and terminal attempts awaiting settlement are not fresh candidates.
    if activation.status != scheduling::ActivationState::Unresolved
        || activation.latest_attempt.is_some()
    {
        return None;
    }
    snapshot
        .concurrency_cap
        .filter(|cap| snapshot.occupied >= u64::from(*cap))
        .map(|limit| Blocker::Capacity {
            occupied: snapshot.occupied,
            limit,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resource_conflict_is_exact_and_does_not_fabricate_capacity_or_readiness() {
        let db = super::super::tests::database().await;
        super::super::install(&*db).await.unwrap();
        let mut definition = bcode_workflow::WorkflowBuilder::new(
            "diagnostics",
            bcode_workflow::Step::task("worker", |v: u32, _| async move { Ok(v) }),
        )
        .build()
        .unwrap()
        .definition()
        .clone();
        definition.nodes.get_mut("worker").unwrap().resources =
            vec![bcode_workflow::ResourceClaim {
                resource: "workspace".into(),
                access: bcode_workflow::ResourceAccess::Write,
            }];
        let mut run = super::super::tests::run("diagnostics", false);
        let id =
            bcode_workflow::WorkflowDefinitionIdentity::for_definition("diagnostics", &definition)
                .unwrap();
        run.definition_id = id.definition_id;
        run.definition_version = id.definition_version;
        super::super::admit(&*db, &run, &definition).await.unwrap();
        let owner = crate::WorkflowExecutionAuthority {
            target_artifact_id: "artifact".into(),
            daemon_instance_id: "daemon".into(),
            generation: 1,
            fencing_token: "fence".into(),
        };
        super::super::attempts::claim_authority(&*db, "diagnostics", &owner)
            .await
            .unwrap();
        let before = snapshot_at(&*db, "diagnostics", None, None, 1, 2)
            .await
            .unwrap();
        assert!(!before.explanations[0].incomplete);
        assert!(before.explanations[0].blockers.is_empty());
        let tx = db
            .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
            .await
            .unwrap();
        super::super::safety::reserve(
            &*tx,
            "diagnostics",
            "other",
            &[bcode_workflow::ResourceClaim {
                resource: "workspace".into(),
                access: bcode_workflow::ResourceAccess::Write,
            }],
        )
        .await
        .unwrap();
        tx.commit().await.unwrap();
        let blocked = snapshot(&*db, "diagnostics", None, None, 1).await.unwrap();
        assert!(
            blocked.explanations[0]
                .blockers
                .contains(&Blocker::Resource {
                    resource: "workspace".into()
                })
        );
        assert!(blocked.explanations[0].incomplete);
        let tx = db
            .begin_transaction_with_mode(switchy_database::TransactionMode::Immediate)
            .await
            .unwrap();
        super::super::safety::release(&*tx, "diagnostics", "other")
            .await
            .unwrap();
        tx.commit().await.unwrap();
        let clear = snapshot(&*db, "diagnostics", None, None, 1).await.unwrap();
        assert!(clear.explanations[0].blockers.is_empty());
        assert!(!clear.explanations[0].incomplete);
        assert_eq!(clear.scheduling.occupied, before.scheduling.occupied);
    }

    fn observed() -> scheduling::SchedulingSnapshot {
        scheduling::SchedulingSnapshot {
            run_id: "run".into(),
            revision: 1,
            run_status: super::super::runs::RunState::Paused,
            concurrency_cap: Some(2),
            occupied: 2,
            user_paused: false,
            activations: vec![],
            next_activation_id: None,
        }
    }
    #[test]
    fn occupied_cap_is_not_a_resource_conflict_or_a_blocker_for_existing_attempt() {
        let mut activation = scheduling::ActivationObservation {
            node_id: "worker".into(),
            activation_id: "activation".into(),
            status: scheduling::ActivationState::Unresolved,
            latest_attempt: None,
        };
        assert_eq!(
            capacity_blocker(&observed(), &activation),
            Some(Blocker::Capacity {
                occupied: 2,
                limit: 2
            })
        );
        activation.latest_attempt = Some(scheduling::AttemptObservation {
            attempt: 1,
            dispatch_identity: "dispatch".into(),
            receipt_present: false,
            terminal: None,
        });
        assert_eq!(capacity_blocker(&observed(), &activation), None);
        let explanation = ActivationExplanation {
            node_id: activation.node_id,
            activation_id: activation.activation_id,
            dependency_inputs_materialized: true,
            blockers: vec![],
            incomplete: true,
        };
        assert!(explanation.incomplete);
        // There is deliberately no Ready/eligible result in this explanatory contract.
        assert!(explanation.blockers.is_empty());
    }
}
