//! Explicit same-run adoption through the existing workflow owner and canonical session evidence.
use super::{
    AWAIT_OPERATION, Arc, HARNESS_WORKFLOW_OWNER, HarnessCompleteness, HarnessObservationKind,
    HarnessRequestKind, HarnessTerminalDisposition, HarnessWorkflowReceipt, ManagedSessionCall,
    ServerState, Value, WorkflowStoreError, canonical_session_owner, invalid, json,
    settlement_authority, verify_session,
};
use bcode_workflow::{WorkflowObligationAdoption, WorkflowObligationAdoptionProof};
use bcode_workflow_store::ActivationDispatchOwner as _;

#[cfg(test)]
tokio::task_local! {
    static AFTER_PLANNING: std::sync::Arc<dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync>;
}

#[cfg(test)]
pub async fn with_after_planning<F: std::future::Future>(
    hook: std::sync::Arc<
        dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>> + Send + Sync,
    >,
    future: F,
) -> F::Output {
    AFTER_PLANNING.scope(hook, future).await
}

pub(super) async fn refresh_receipt(
    state: &ServerState,
    original: &HarnessWorkflowReceipt,
) -> Result<HarnessWorkflowReceipt, WorkflowStoreError> {
    let stored = crate::workflow_representation::owner::execution_receipt(
        state,
        &original.run_id,
        &original.dispatch_identity,
    )
    .await?
    .ok_or_else(|| invalid("canonical obligation receipt missing"))?;
    let current = HarnessWorkflowReceipt::decode(&stored.receipt)?;
    let mut expected = serde_json::to_value(original)?;
    let mut actual = serde_json::to_value(&current)?;
    expected
        .as_object_mut()
        .ok_or_else(|| invalid("invalid obligation receipt"))?
        .remove("workflow_obligation_adoption");
    actual
        .as_object_mut()
        .ok_or_else(|| invalid("invalid obligation receipt"))?
        .remove("workflow_obligation_adoption");
    if actual != expected {
        return Err(invalid(
            "canonical managed receipt changed outside adoption",
        ));
    }
    if let Some(proof) = &current.workflow_obligation_adoption {
        let source = &proof.request.source;
        let destination = &proof.request.destination;
        if current.run_id != proof.request.run_id
            || ![
                source.dispatch_identity.as_str(),
                destination.dispatch_identity.as_str(),
            ]
            .contains(&current.dispatch_identity.as_str())
        {
            return Err(invalid("canonical adoption participant mismatch"));
        }
        let authority = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execution_authority(&current.run_id)?
            .filter(|authority| {
                crate::workflow_operations::authority_targets_current_daemon(state, authority)
            })
            .ok_or_else(|| invalid("canonical adoption observation authority unavailable"))?;
        let recovered = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .recover_owned_obligation_adoption(&proof.request, &authority)?;
        if recovered.as_ref() != Some(proof) {
            return Err(invalid(
                "canonical adoption receipt bodies could not be verified",
            ));
        }
    }
    Ok(current)
}

fn validate_adoption_destination(
    state: &ServerState,
    destination: &bcode_workflow_store::PendingActivation,
    intent: &Value,
    source: &HarnessWorkflowReceipt,
    request: &WorkflowObligationAdoption,
) -> Result<(bcode_workflow::WorkflowBlockDefinition, ManagedSessionCall), WorkflowStoreError> {
    let block: bcode_workflow::WorkflowBlockDefinition =
        serde_json::from_value(destination.node.configuration.clone())?;
    if block.operation != AWAIT_OPERATION || block.plugin_id != source.plugin_id {
        return Err(invalid(
            "obligation destination must be an explicit same-owner await node",
        ));
    }
    let owner_input = crate::workflow_plugin_block_input_from_activation(destination, &block)?;
    let encoded = owner_input
        .get("request_json")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("obligation destination missing exact request"))?;
    let call: ManagedSessionCall = serde_json::from_str(encoded)?;
    if serde_json::to_value(&call)? != serde_json::to_value(&source.call)? {
        return Err(invalid("obligation destination changed native submission"));
    }
    let requirement: bcode_harness_models::HarnessCompletionRequirement = serde_json::from_value(
        owner_input
            .get("completion_requirement")
            .cloned()
            .unwrap_or_else(|| json!("associated_work")),
    )?;
    if requirement != bcode_harness_models::HarnessCompletionRequirement::AssociatedWork {
        return Err(invalid(
            "obligation destination must retain associated-work responsibility",
        ));
    }
    let preparation: bcode_workflow::WorkflowBlockPreparationResponse = serde_json::from_value(
        intent
            .get("preparation")
            .cloned()
            .ok_or_else(|| invalid("adoption destination preparation missing"))?,
    )?;
    let scope =
        crate::workflow_mutation_grant_scope(state, destination, &block, Some(&preparation))?;
    let scope_value = serde_json::to_value(scope)?;
    let scope_digest =
        bcode_workflow::workflow_canonical_value_sha256(&scope_value).map_err(invalid)?;
    let grant = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .grant(&request.destination_grant_id)?
        .ok_or_else(|| invalid("adoption exact grant missing"))?;
    if grant.run_id != request.run_id
        || grant.node_id != request.destination.node_id
        || grant.scope != scope_value
        || scope_digest != request.destination_grant_scope_sha256
    {
        return Err(invalid(
            "adoption exact destination grant does not approve this operation",
        ));
    }
    Ok((block, call))
}

fn adoption_destination_receipt(
    destination: bcode_workflow_store::PendingActivation,
    block: bcode_workflow::WorkflowBlockDefinition,
    call: ManagedSessionCall,
    request: &WorkflowObligationAdoption,
    source: &HarnessWorkflowReceipt,
) -> Result<Value, WorkflowStoreError> {
    let receipt = HarnessWorkflowReceipt {
        owner: HARNESS_WORKFLOW_OWNER.into(),
        dispatch_identity: request.destination.dispatch_identity.clone(),
        run_id: request.run_id.clone(),
        node_id: request.destination.node_id.clone(),
        activation_id: request.destination.activation_id.clone(),
        parent_session_id: source.parent_session_id,
        session_id: source.session_id,
        generation: source.generation,
        call,
        plugin_id: source.plugin_id.clone(),
        output_schema: destination.node.output.clone(),
        node: destination.node,
        block,
        complete_input: destination
            .input
            .ok_or_else(|| invalid("obligation destination input missing"))?,
        completion_requirement: bcode_harness_models::HarnessCompletionRequirement::AssociatedWork,
        workflow_obligation_adoption: None,
    };
    Ok(serde_json::to_value(receipt)?)
}

async fn adoption_source_receipt(
    state: &Arc<ServerState>,
    request: &WorkflowObligationAdoption,
) -> Result<HarnessWorkflowReceipt, WorkflowStoreError> {
    let stored = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .dispatch_receipt(&request.source.dispatch_identity)?
        .ok_or_else(|| invalid("obligation source receipt missing"))?;
    let source = HarnessWorkflowReceipt::decode(&stored.receipt)?;
    verify_session(state, &source).await?;
    if source.workflow_obligation_adoption.is_none()
        && bcode_workflow::workflow_canonical_value_sha256(&stored.receipt).map_err(invalid)?
            != request.source_receipt_sha256
    {
        return Err(invalid("obligation source receipt checksum changed"));
    }
    if source.run_id != request.run_id || stored.run_id != request.run_id {
        return Err(invalid("cross-run obligation adoption is unsupported"));
    }
    if source.workflow_obligation_adoption.is_some() {
        return Err(invalid(
            "retained adoption proof could not be recovered under current authority",
        ));
    }
    if source.completion_requirement
        != bcode_harness_models::HarnessCompletionRequirement::AsyncHandoff
        || source.run_id != request.run_id
        || source.dispatch_identity != request.source.dispatch_identity
    {
        return Err(invalid("source did not explicitly authorize async handoff"));
    }
    Ok(source)
}

async fn inspect_adoption_source(
    state: &Arc<ServerState>,
    request: &WorkflowObligationAdoption,
    source: HarnessWorkflowReceipt,
) -> Result<
    (
        HarnessWorkflowReceipt,
        bcode_session_models::ExternalHarnessOwner,
        bcode_harness_models::HarnessSubmission,
        bcode_session_models::HarnessInventoryPage,
        bcode_session_models::HarnessInventorySummaryRead,
    ),
    WorkflowStoreError,
> {
    let HarnessRequestKind::Submit { submission, .. } = &source.call.harness.kind else {
        return Err(invalid("obligation source submission missing"));
    };
    let subject = bcode_workflow::workflow_canonical_value_sha256(&json!({
        "session_id":source.session_id,"generation":source.generation,
        "submission_id":submission.submission_id,"attempt_id":submission.attempt_id,
    }))
    .map_err(invalid)?;
    if subject != request.evidence_subject {
        return Err(invalid("obligation evidence subject changed"));
    }
    let owner = canonical_session_owner(state, source.session_id, source.generation)?;
    let terminal = state
        .sessions
        .session_harness_inventory_page(
            &owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(
                bcode_harness_models::HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                },
            ),
            bcode_session_models::HarnessInventoryKind::Terminal,
            None,
        )
        .await
        .map_err(|_| invalid("obligation evidence revision unavailable"))?;
    let initial_summary = state
        .sessions
        .session_harness_inventory_summary(
            &owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(
                bcode_harness_models::HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                },
            ),
            bcode_session_models::HarnessInventoryKind::Terminal,
        )
        .await
        .map_err(|_| invalid("obligation canonical evidence summary unavailable"))?;
    let checksum =
        bcode_workflow::workflow_canonical_value_sha256(&serde_json::to_value(&initial_summary)?)
            .map_err(invalid)?;
    if initial_summary.owner != owner || initial_summary.commitment != terminal.commitment {
        return Err(invalid(
            "obligation terminal snapshot changed during inspection",
        ));
    }
    if terminal.next_cursor.is_some()
        || terminal.entries.len() != 1
        || terminal.entries[0].observation.completeness != HarnessCompleteness::Complete
        || !matches!(&terminal.entries[0].observation.kind,
            HarnessObservationKind::Execution { event: bcode_harness_models::HarnessExecutionEvent::SubmissionTerminal {
                attempt, disposition: HarnessTerminalDisposition::Completed, output: Some(_), .. } }
                if attempt.submission_id == submission.submission_id && attempt.attempt_id == submission.attempt_id)
    {
        return Err(invalid(
            "async handoff requires exact successful foreground terminal and output",
        ));
    }
    if initial_summary.observed_through_sequence != request.evidence_revision
        || checksum != request.evidence_sha256
    {
        return Err(invalid("obligation evidence revision changed"));
    }
    let submission = submission.clone();
    Ok((source, owner, submission, terminal, initial_summary))
}

#[cfg(test)]
async fn run_after_planning_hook() {
    #[cfg(test)]
    if let Ok(hook) = AFTER_PLANNING.try_with(std::sync::Arc::clone) {
        hook().await;
    }
}

fn recover_adoption(
    state: &ServerState,
    request: &WorkflowObligationAdoption,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> Result<Option<WorkflowObligationAdoptionProof>, WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .recover_owned_obligation_adoption(request, authority)
}

/// Host-only adoption boundary. No plugin input or session association supplies authority.
/// The destination must already be a pending `harness.await` node in this canonical run.
/// # Errors
/// Rejects represented/cross-run work until its owner supports the same atomic operation,
/// changed authority/evidence/receipts, missing ordinary destination approval or invalid claims.
pub async fn adopt_execution(
    state: &Arc<ServerState>,
    request: &WorkflowObligationAdoption,
) -> Result<WorkflowObligationAdoptionProof, WorkflowStoreError> {
    crate::workflow_representation::require_legacy(state, &request.run_id)
        .await
        .map_err(|_| invalid("obligation adoption unsupported by this run representation"))?;
    let guard = settlement_authority(state, &request.run_id)
        .await?
        .ok_or_else(|| invalid("obligation adoption authority unavailable"))?;
    let recovered = recover_adoption(state, request, &guard.authority)?;
    if let Some(proof) = recovered {
        return Ok(proof);
    }
    let source = adoption_source_receipt(state, request).await?;
    // A held canonical session owner prevents foreign generation adoption. This is not
    // native transport adoption and cannot turn process loss into permission to resend.
    let _session = state
        .sessions
        .acquire_session_ownership(
            source.session_id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|_| invalid("obligation session owner unavailable"))?;
    verify_session(state, &source).await?;
    let (source, owner, submission, terminal, initial_summary) =
        inspect_adoption_source(state, request, source).await?;
    let destination = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_obligation_destination(
            &request.run_id,
            &request.destination.node_id,
            &request.destination.activation_id,
            &guard.authority,
        )?
        .ok_or_else(|| invalid("adoption destination is not pending"))?;
    let Some(mut plan) = (crate::WorkflowActivationOwner { state })
        .plan(&destination)
        .await?
    else {
        return Err(invalid("obligation destination lacks canonical approval"));
    };
    let (block, call) =
        validate_adoption_destination(state, &destination, &plan.intent, &source, request)?;
    plan.intent["adoption"] = serde_json::to_value(request)?;
    let destination_receipt =
        adoption_destination_receipt(destination, block, call, request, &source)?;
    #[cfg(test)]
    run_after_planning_hook().await;
    verify_session(state, &source).await?;
    let final_evidence = state
        .sessions
        .session_harness_inventory_page(
            &owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(
                bcode_harness_models::HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                },
            ),
            bcode_session_models::HarnessInventoryKind::Terminal,
            None,
        )
        .await
        .map_err(|_| invalid("obligation final evidence unavailable"))?;
    let final_summary = state
        .sessions
        .session_harness_inventory_summary(
            &owner,
            &bcode_session_models::HarnessInventoryScope::Attempt(
                bcode_harness_models::HarnessAttemptIdentity {
                    submission_id: submission.submission_id.clone(),
                    attempt_id: submission.attempt_id.clone(),
                },
            ),
            bcode_session_models::HarnessInventoryKind::Terminal,
        )
        .await
        .map_err(|_| invalid("obligation final evidence revision unavailable"))?;
    if final_evidence.owner != owner
        || final_evidence.commitment != terminal.commitment
        || final_summary != initial_summary
    {
        return Err(invalid(
            "obligation evidence or session owner changed during destination authorization",
        ));
    }
    // The final pinned evidence is historical proof, not a future-silence guarantee.
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .adopt_owned_obligation(
            request,
            &guard.authority,
            &plan.intent,
            &destination_receipt,
            crate::current_unix_millis(),
        )
}
