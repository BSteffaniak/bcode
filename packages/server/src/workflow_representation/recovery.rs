//! Recovery-only takeover authenticated by the existing daemon/session owner resolver.
use super::{
    ServerError, ServerState, open, representation, retained_store, unavailable, unsupported,
};
use crate::workflow_operations::{self, PriorOwnerLiveness};
use representation::{
    attempts::owner_boundary,
    safety::{AuthorityHandoff, AuthorityHandoffResolver, HandoffEvidence},
};

struct VerifiedDaemonResolver<'a> {
    state: &'a ServerState,
    session_id: crate::SessionId,
    transition: AuthorityHandoff,
}
impl AuthorityHandoffResolver for VerifiedDaemonResolver<'_> {
    fn verify<'a>(
        &'a self,
        transition: &'a AuthorityHandoff,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<HandoffEvidence, representation::RepresentationError>,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            if transition != &self.transition
                || !workflow_operations::authority_targets_current_daemon(
                    self.state,
                    &transition.replacement,
                )
            {
                return Ok(HandoffEvidence::Unknown);
            }
            match workflow_operations::prior_owner_liveness(
                self.state,
                self.session_id,
                &transition.expected,
            )
            .await
            {
                Ok(PriorOwnerLiveness::ObservedEnded) => Ok(HandoffEvidence::Verified {
                    transition: Box::new(transition.clone()),
                    receipt_id: format!("daemon-recovery:{}", transition.replacement.fencing_token),
                    verified_at_ms: crate::current_unix_millis(),
                }),
                Ok(PriorOwnerLiveness::Live | PriorOwnerLiveness::Unverifiable) | Err(_) => {
                    Ok(HandoffEvidence::Unknown)
                }
            }
        })
    }
}

/// Acquire only after exact old-owner cessation and current executable capability validation.
pub async fn takeover(
    state: &ServerState,
    run_id: &str,
) -> Result<workflow_operations::AuthorityGuard, ServerError> {
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let expected = owner_boundary::execution_authority(&db, run_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unsupported)?;
    let session_id = representation::runs::association(&db, run_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unsupported)?
        .parse::<crate::SessionId>()
        .map_err(|_| unavailable())?;
    let (_, definition) = representation::graph_revision::current(&db, run_id)
        .await
        .map_err(|_| unavailable())?;
    if representation::definition_dispatch_availability(&definition).map_err(|_| unsupported())?
        != representation::DispatchAvailability::AgentControlLifecycle
    {
        return Err(unsupported());
    }
    workflow_operations::validate_workflow_definition_for_production(state, &definition)?;
    if workflow_operations::prior_owner_liveness(state, session_id, &expected).await?
        != PriorOwnerLiveness::ObservedEnded
    {
        return Err(unsupported());
    }
    let ownership = state
        .sessions
        .acquire_session_ownership(session_id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .map_err(|_| unavailable())?;
    let replacement = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: workflow_operations::current_artifact_id(state),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: expected.generation.checked_add(1).ok_or_else(unavailable)?,
        fencing_token: uuid::Uuid::new_v4().to_string(),
    };
    let transition = AuthorityHandoff {
        run_id: run_id.into(),
        expected,
        replacement: replacement.clone(),
    };
    let resolver = VerifiedDaemonResolver {
        state,
        session_id,
        transition: transition.clone(),
    };
    representation::safety::handoff_for_recovery(&db, &transition, &resolver)
        .await
        .map_err(|_| unsupported())?;
    owner_boundary::verify(&db, run_id, &replacement)
        .await
        .map_err(|_| unavailable())?;
    Ok(workflow_operations::AuthorityGuard {
        authority: replacement,
        _session_ownership: Some(ownership),
    })
}

/// Reconcile bounded receipt pages; missing receipts remain ambiguous and are never redispatched.
pub async fn reconcile(
    state: &ServerState,
    run_id: &str,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> Result<(), ServerError> {
    use bcode_workflow_store::AsyncAttemptStatusObserver;
    let retained = retained_store(state).await?;
    let db = open(retained.path(), true)?;
    let mut after = None;
    loop {
        let (attempts, next) =
            owner_boundary::recovery_attempts(&db, run_id, authority, after.as_deref(), 32)
                .await
                .map_err(|_| unavailable())?;
        for attempt in attempts {
            let Some(receipt) = attempt.receipt else {
                continue;
            };
            owner_boundary::verify(&db, run_id, authority)
                .await
                .map_err(|_| unavailable())?;
            let observation = crate::WorkflowTurnReceiptObserver { state }
                .observe_async(&bcode_workflow_store::AttemptReconciliationRequest {
                    run_id: receipt.run_id,
                    node_id: receipt.node_id.clone(),
                    activation_id: receipt.activation_id.clone(),
                    attempt: receipt.attempt,
                    dispatch_identity: receipt.dispatch_identity.clone(),
                    side_effect: attempt.prepared.side_effect,
                    receipt: receipt.receipt,
                })
                .await?;
            let revision = representation::graph_pages::revision(&db, run_id)
                .await
                .map_err(|_| unavailable())?;
            let node = representation::graph_pages::node(&db, run_id, revision, &receipt.node_id)
                .await
                .map_err(|_| unavailable())?
                .ok_or_else(unavailable)?;
            let request = bcode_workflow_store::PreparedActivationDispatch {
                activation: bcode_workflow_store::PendingActivation {
                    run_id: run_id.into(),
                    node_id: receipt.node_id,
                    activation_id: receipt.activation_id,
                    dependency_generation: 0,
                    input: None,
                    node,
                    created_at_ms: attempt.prepared.prepared_at_ms,
                },
                attempt: attempt.prepared.attempt,
                dispatch_identity: receipt.dispatch_identity,
                intent: attempt.prepared.intent,
            };
            match observation {
                bcode_workflow_store::AttemptObservation::Succeeded { .. }
                | bcode_workflow_store::AttemptObservation::Failed { .. }
                | bcode_workflow_store::AttemptObservation::Cancelled => {
                    super::owner::settle_observation(state, &request, authority, observation)
                        .await?;
                }
                _ => {} // No terminal proof: retain the original intent, receipt and barrier.
            }
        }
        after = next;
        if after.is_none() {
            break;
        }
        tokio::task::yield_now().await;
    }
    if owner_boundary::recovery_attempts(&db, run_id, authority, None, 1)
        .await
        .map_err(|_| unavailable())?
        .0
        .is_empty()
    {
        representation::safety::finish_recovery(&db, run_id, authority)
            .await
            .map_err(|_| unavailable())?;
    }
    Ok(())
}

/// Allow foreign receipt observation only after independently verifying its accepting owner ended.
pub async fn can_observe(
    state: &ServerState,
    request: &bcode_workflow_store::AttemptReconciliationRequest,
) -> Result<Option<bool>, ServerError> {
    let retained = retained_store(state).await?;
    match super::lookup_at(retained.path(), &request.run_id).await? {
        super::RunLookup::Legacy | super::RunLookup::Missing => return Ok(None),
        super::RunLookup::Available(_) => {}
        _ => return Err(unsupported()),
    }
    let db = open(retained.path(), false)?;
    if !representation::safety::recovery_required(&db, &request.run_id)
        .await
        .map_err(|_| unavailable())?
    {
        return Ok(Some(false));
    }
    let authority = owner_boundary::execution_authority(&db, &request.run_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unsupported)?;
    if !workflow_operations::authority_targets_current_daemon(state, &authority) {
        return Ok(Some(false));
    }
    let receipt = crate::workflow_receipts::AgentTurnReceipt::decode(&request.receipt)
        .map_err(|_| unavailable())?;
    let (Some(artifact), Some(instance)) =
        (receipt.owner_artifact_id, receipt.owner_daemon_instance_id)
    else {
        return Ok(Some(false));
    };
    let link = owner_boundary::execution_session_link(
        &db,
        &request.run_id,
        &request.node_id,
        &request.activation_id,
        request.attempt,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unsupported)?;
    if link.session_id != receipt.session_id.to_string() {
        return Ok(Some(false));
    }
    let prior = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: artifact,
        daemon_instance_id: instance,
        generation: 0,
        fencing_token: String::new(),
    };
    let ended = workflow_operations::prior_owner_liveness(state, receipt.session_id, &prior)
        .await?
        == PriorOwnerLiveness::ObservedEnded;
    owner_boundary::verify(&db, &request.run_id, &authority)
        .await
        .map_err(|_| unavailable())?;
    Ok(Some(ended))
}

/// Offer one bounded canonical discovery page; discovery never establishes owner cessation.
pub async fn discover(
    state: &ServerState,
    after: &mut Option<String>,
    limit: usize,
) -> Result<Vec<String>, ServerError> {
    let retained = retained_store(state).await?;
    let db = open(retained.path(), false)?;
    if representation::admission_family(&db)
        .await
        .map_err(|_| unavailable())?
        != representation::AdmissionFamily::Installed
    {
        *after = None;
        return Ok(Vec::new());
    }
    let (runs, next) = owner_boundary::recoverable_runs(
        &db,
        after.as_deref(),
        u16::try_from(limit.clamp(1, 100)).map_err(|_| unavailable())?,
    )
    .await
    .map_err(|_| unavailable())?;
    *after = next;
    Ok(runs.into_iter().map(|run| run.run_id).collect())
}

pub async fn is_represented(state: &ServerState, run_id: &str) -> Result<bool, ServerError> {
    let retained = retained_store(state).await?;
    match super::lookup_at(retained.path(), run_id).await? {
        super::RunLookup::Legacy | super::RunLookup::Missing => Ok(false),
        super::RunLookup::Available(_) => Ok(true),
        _ => Err(unsupported()),
    }
}
