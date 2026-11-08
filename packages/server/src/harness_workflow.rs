//! Durable managed-session workflow operation owner.
//!
//! Plugins prepare semantic operations; this adapter fences dispatch, links execution sessions,
//! and reconciles canonical evidence without vendor branches or a replay journal.

use crate::ServerState;
use bcode_harness_models::{
    HarnessCompleteness, HarnessObservationKind, HarnessRequestKind, HarnessServiceResponse,
    HarnessServiceResponseKind, HarnessSessionTarget, HarnessTerminalDisposition,
};
use bcode_ipc::harness::ManagedSessionCall;
use bcode_session_models::SessionId;
use bcode_workflow_store::{
    AttemptObservation, AttemptReconciliationRequest, DispatchReceipt, PreparedActivationDispatch,
    WorkflowExecutionSessionLink, WorkflowStoreError,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::sync::Arc;

#[path = "harness/obligation.rs"]
mod obligation;
pub use obligation::adopt_execution;

#[cfg(test)]
pub(crate) use obligation::with_after_planning as with_adoption_after_planning;
#[path = "harness/execution_outcome.rs"]
mod execution_outcome;

/// Portable compatibility identity; unknown receipt owners are not interpreted as model turns.
pub(crate) const HARNESS_WORKFLOW_OWNER: &str = "bcode.server.managed-harness/v1";
const EXECUTE_OPERATION: &str = "harness.execute";
const AWAIT_OPERATION: &str = "harness.await";
const MANAGED_INTERFACE: &str = "bcode.managed-session/v1";

/// Selected ordinary prompt assignment, distinct from plugin-prepared block contracts.
pub(crate) const SELECTED_PROMPT_OWNER: &str = "bcode.server.selected-harness-prompt/v1";

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct SelectedPromptReceipt {
    owner: String,
    dispatch_identity: String,
    run_id: String,
    node_id: String,
    activation_id: String,
    attempt: u32,
    session_id: SessionId,
    start: bcode_ipc::harness::ManagedSelectedStartRequest,
    resolved: bcode_model::execution::ResolvedExecutionModel,
    configuration: bcode_workflow::WorkflowPromptConfiguration,
    submission: bcode_harness_models::HarnessSubmission,
    input: Value,
    output: bcode_workflow::ValueSchema,
}

impl SelectedPromptReceipt {
    fn decode(value: &Value) -> Result<Self, WorkflowStoreError> {
        if serde_json::to_vec(value)?.len() > bcode_workflow::MAX_WORKFLOW_BLOCK_PREPARATION_BYTES {
            return Err(invalid("selected workflow receipt exceeds bound"));
        }
        let receipt: Self = serde_json::from_value(value.clone())?;
        receipt
            .start
            .validate()
            .map_err(|_| invalid("invalid selected workflow Start"))?;
        receipt
            .configuration
            .validate()
            .map_err(|_| invalid("invalid selected workflow configuration"))?;
        receipt
            .submission
            .validate()
            .map_err(|_| invalid("invalid selected workflow assignment"))?;
        let checked = crate::workflow_operations::execution_selection::resolve(
            &receipt.configuration,
            std::slice::from_ref(&receipt.resolved.target),
        )?
        .ok_or_else(|| invalid("selected workflow configuration missing selection"))?;
        let expected_start =
            crate::workflow_operations::execution_selection::selected_start_request(
                &receipt.resolved,
                receipt.start.instance.clone(),
                receipt.start.open.clone(),
                receipt
                    .start
                    .parent_session_id
                    .ok_or_else(|| invalid("selected workflow parent missing"))?,
            )?;
        if serde_json::to_value(expected_start)? != serde_json::to_value(&receipt.start)? {
            return Err(invalid("selected workflow Start changed resolved instance"));
        }
        if receipt.owner != SELECTED_PROMPT_OWNER
            || checked.requested != receipt.resolved.requested
            || checked.eligibility != receipt.resolved.eligibility
            || receipt.start.selection.model_id != receipt.resolved.target.model.model_id
            || receipt.start.selection.route.as_ref() != Some(&receipt.resolved.target.route)
            || receipt.start.selection.account_reference
                != receipt.resolved.target.account_reference
            || receipt.start.open.session_id != receipt.session_id.to_string()
            || receipt.start.open.open_id != format!("{}:open", receipt.dispatch_identity)
            || receipt.submission.placement != bcode_harness_models::HarnessPlacement::NextTurn
            || receipt.submission.session_id != receipt.start.open.session_id
            || receipt.submission.generation != 1
            || receipt.submission.submission_id != receipt.dispatch_identity
            || receipt.submission.attempt_id != receipt.dispatch_identity
            || receipt.dispatch_identity
                != bcode_workflow_store::dispatch_identity(
                    &receipt.run_id,
                    &receipt.node_id,
                    &receipt.activation_id,
                    receipt.attempt,
                )
        {
            return Err(invalid("selected workflow receipt correlation mismatch"));
        }
        Ok(receipt)
    }
}

/// Verify pre-Open discovery scope from the exact prepared prompt intent and workflow authority.
/// This grants read-only discovery, not selected Open or assignment dispatch.
pub(crate) async fn authorize_selected_discovery(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
    parent_session_id: SessionId,
) -> Result<crate::workflow_operations::AuthorityGuard, WorkflowStoreError> {
    let guard = settlement_authority(state, run_id)
        .await?
        .ok_or_else(|| invalid("selected discovery authority required"))?;
    let intent =
        crate::workflow_representation::owner::dispatch_intent(state, run_id, dispatch_identity)
            .await?;
    let configuration: bcode_workflow::WorkflowPromptConfiguration = serde_json::from_value(
        intent
            .get("configuration")
            .cloned()
            .ok_or_else(|| invalid("selected discovery configuration missing"))?,
    )?;
    configuration
        .validate()
        .map_err(|_| invalid("invalid selected discovery configuration"))?;
    if configuration.execution_model_selection.is_none() {
        return Err(invalid("selected discovery requires exact model selection"));
    }
    let run = crate::workflow_representation::status(state, run_id)
        .await
        .map_err(|_| invalid("selected discovery run unavailable"))?
        .ok_or_else(|| invalid("selected discovery run missing"))?;
    if run.parent_session_id.as_deref() != Some(parent_session_id.to_string().as_str()) {
        let stored = crate::workflow_representation::owner::execution_receipt(
            state,
            run_id,
            dispatch_identity,
        )
        .await?
        .ok_or_else(|| invalid("selected discovery parent mismatch"))?;
        let receipt = SelectedPromptReceipt::decode(&stored.receipt)?;
        if receipt.session_id != parent_session_id {
            return Err(invalid("selected discovery session mismatch"));
        }
        return authorize_selected_start(
            state,
            run_id,
            dispatch_identity,
            receipt.session_id,
            &receipt.start.instance.plugin_id,
            &receipt.start,
        )
        .await;
    }
    Ok(guard)
}

/// Hold current workflow authority and validate exact immutable selected Open before host I/O.
/// Host admission must independently verify instance, workspace, policy and native capabilities.
pub(crate) async fn authorize_selected_start(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
    session_id: SessionId,
    plugin_id: &str,
    request: &bcode_ipc::harness::ManagedSelectedStartRequest,
) -> Result<crate::workflow_operations::AuthorityGuard, WorkflowStoreError> {
    let guard = settlement_authority(state, run_id)
        .await?
        .ok_or_else(|| invalid("selected workflow authority required"))?;
    let stored =
        crate::workflow_representation::owner::execution_receipt(state, run_id, dispatch_identity)
            .await?
            .ok_or_else(|| invalid("selected workflow receipt required"))?;
    let receipt = SelectedPromptReceipt::decode(&stored.receipt)?;
    let intent =
        crate::workflow_representation::owner::dispatch_intent(state, run_id, dispatch_identity)
            .await?;
    if intent.get("configuration") != Some(&serde_json::to_value(&receipt.configuration)?)
        || intent.get("input") != Some(&receipt.input)
        || intent.get("node").and_then(|node| node.get("output"))
            != Some(&serde_json::to_value(&receipt.output)?)
    {
        return Err(invalid(
            "selected workflow receipt differs from prepared intent",
        ));
    }
    if receipt.run_id != run_id
        || receipt.dispatch_identity != dispatch_identity
        || receipt.session_id != session_id
        || receipt.start.instance.plugin_id != plugin_id
        || serde_json::to_value(&receipt.start)? != serde_json::to_value(request)?
    {
        return Err(invalid("selected workflow Start grant mismatch"));
    }
    let link = crate::workflow_representation::owner::execution_session_link(
        state,
        run_id,
        &receipt.node_id,
        &receipt.activation_id,
        receipt.attempt,
    )
    .await?
    .ok_or_else(|| invalid("selected workflow link required"))?;
    let summary = state
        .sessions
        .session_summary(session_id)
        .await
        .map_err(|_| invalid("selected workflow child unavailable"))?;
    let provenance = summary
        .execution
        .as_ref()
        .map(|execution| &execution.provenance)
        .ok_or_else(|| invalid("selected workflow child provenance missing"))?;
    if link.session_id != session_id.to_string()
        || summary.parent_id != receipt.start.parent_session_id
        || summary.working_directory.to_string_lossy() != receipt.start.open.working_directory
        || provenance.owner != "bcode.workflow"
        || provenance.run_id != receipt.run_id
        || provenance.node_id != receipt.node_id
        || provenance.activation_id.as_ref() != Some(&receipt.activation_id)
        || provenance.attempt != receipt.attempt
    {
        return Err(invalid("selected workflow Start link mismatch"));
    }
    Ok(guard)
}

/// Persist correlation before selected native Open. An ambiguous delivery is observation-owned;
/// this function never retries a native mutation or falls back to a provider.
pub(crate) async fn dispatch_selected_prompt(
    state: &Arc<ServerState>,
    request: &PreparedActivationDispatch,
    resolved: bcode_model::execution::ResolvedExecutionModel,
    instance: bcode_harness_models::HarnessInstance,
    session_ids: (SessionId, SessionId),
    configuration: bcode_workflow::WorkflowPromptConfiguration,
    text: String,
) -> Result<Value, WorkflowStoreError> {
    let (session_id, parent_session_id) = session_ids;
    let guard = settlement_authority(state, &request.activation.run_id)
        .await?
        .ok_or_else(|| invalid("selected prompt authority required"))?;
    let summary = state
        .sessions
        .session_summary(session_id)
        .await
        .map_err(|_| invalid("selected prompt child unavailable"))?;
    let open_id = format!("{}:open", request.dispatch_identity);
    let start = crate::workflow_operations::execution_selection::selected_start_request(
        &resolved,
        instance,
        bcode_harness_models::HarnessOpenRequest {
            open_id,
            session_id: session_id.to_string(),
            generation: 1,
            working_directory: summary.working_directory.to_string_lossy().into_owned(),
            resume: None,
            parent_assistance: None,
        },
        parent_session_id,
    )?;
    let receipt = SelectedPromptReceipt {
        owner: SELECTED_PROMPT_OWNER.into(),
        dispatch_identity: request.dispatch_identity.clone(),
        run_id: request.activation.run_id.clone(),
        node_id: request.activation.node_id.clone(),
        activation_id: request.activation.activation_id.clone(),
        attempt: request.attempt,
        session_id,
        start,
        resolved,
        configuration,
        submission: bcode_harness_models::HarnessSubmission {
            submission_id: request.dispatch_identity.clone(),
            attempt_id: request.dispatch_identity.clone(),
            session_id: session_id.to_string(),
            generation: 1,
            text,
            placement: bcode_harness_models::HarnessPlacement::NextTurn,
        },
        input: request
            .activation
            .input
            .clone()
            .ok_or_else(|| invalid("selected prompt input missing"))?,
        output: request.activation.node.output.clone(),
    };
    let value = serde_json::to_value(&receipt)?;
    SelectedPromptReceipt::decode(&value)?;
    crate::workflow_representation::owner::persist_receipt(
        state,
        &DispatchReceipt {
            run_id: receipt.run_id.clone(),
            node_id: receipt.node_id.clone(),
            activation_id: receipt.activation_id.clone(),
            attempt: receipt.attempt,
            dispatch_identity: receipt.dispatch_identity.clone(),
            receipt: value.clone(),
            admitted_at_ms: crate::current_unix_millis(),
        },
        &guard.authority,
    )
    .await?;
    if crate::workflow_representation::owner::cancellation_intended(
        state,
        &receipt.run_id,
        &receipt.dispatch_identity,
    )
    .await?
    {
        return Ok(value);
    }
    let caller = crate::harness_dispatch::ManagedCaller::Workflow {
        run_id: receipt.run_id.clone(),
        dispatch_identity: receipt.dispatch_identity.clone(),
        session_id,
        plugin_id: receipt.start.instance.plugin_id.clone(),
    };
    let response =
        crate::harness::execution_catalog::start_selected(state, &caller, receipt.start.clone())
            .await;
    let Ok(response) = response else {
        return Ok(value);
    };
    let target = crate::workflow_operations::execution_selection::accepted_start_target(
        &receipt.resolved,
        &receipt.start,
        &response,
    );
    let Ok(target) = target else {
        return Ok(value);
    };
    submit_selected_assignment(state, &caller, &receipt, target).await;
    Ok(value)
}

async fn submit_selected_assignment(
    state: &Arc<ServerState>,
    caller: &crate::harness_dispatch::ManagedCaller,
    receipt: &SelectedPromptReceipt,
    target: HarnessSessionTarget,
) {
    // Prepared is a live transport correlation only. Native host admission must explicitly
    // admit a first submission; no confirmed native identity is synthesized here.
    let call = selected_submission_call(receipt, target);
    if let Some(backend) = state.managed_sessions.get() {
        let _ = backend.execute(caller, call).await;
    }
}

fn selected_submission_call(
    receipt: &SelectedPromptReceipt,
    target: HarnessSessionTarget,
) -> ManagedSessionCall {
    ManagedSessionCall {
        session_id: Some(receipt.session_id),
        parent_session_id: None,
        assistance_destination_session_id: None,
        harness: bcode_harness_models::HarnessRequest {
            schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
            request_id: receipt.submission.submission_id.clone(),
            instance: receipt.start.instance.clone(),
            kind: HarnessRequestKind::Submit {
                target,
                submission: receipt.submission.clone(),
            },
        },
    }
}

/// Observe only canonical, exactly accepted selected assignments. Never reopen or resubmit.
fn selected_native_turn(
    receipt: &SelectedPromptReceipt,
    observations: &[bcode_harness_models::HarnessObservation],
) -> Result<Result<Option<String>, AttemptObservation>, WorkflowStoreError> {
    let exact = bcode_harness_models::HarnessAttemptIdentity {
        submission_id: receipt.submission.submission_id.clone(),
        attempt_id: receipt.submission.attempt_id.clone(),
    };
    if let Some(verdict) = execution_outcome::evaluate_assignment(
        observations,
        receipt.session_id,
        1,
        &exact,
        bcode_harness_models::HarnessCompletionRequirement::AssociatedWork,
    )? {
        return match verdict {
            execution_outcome::AssignmentVerdict::Pending(observation) => Ok(Err(observation)),
            execution_outcome::AssignmentVerdict::Completed { output, .. } => {
                complete_selected_prompt(receipt, &output).map(Err)
            }
        };
    }
    let mut native = None;
    for observation in observations {
        if let HarnessObservationKind::SubmissionAccepted {
            submission_id,
            attempt_id,
            native_turn_id,
        } = &observation.kind
        {
            if observation.session_id != receipt.session_id.to_string()
                || observation.generation != 1
                || observation.completeness != HarnessCompleteness::Complete
                || submission_id != &receipt.submission.submission_id
                || attempt_id != &receipt.submission.attempt_id
                || native
                    .as_ref()
                    .is_some_and(|previous| previous != native_turn_id)
            {
                return Ok(Err(deferred(
                    "selected workflow acceptance is incomplete or conflicting",
                )));
            }
            native = Some(native_turn_id.clone());
        }
    }
    Ok(Ok(native))
}

pub(crate) async fn observe_selected_prompt(
    state: &ServerState,
    request: &AttemptReconciliationRequest,
) -> Result<AttemptObservation, WorkflowStoreError> {
    let receipt = SelectedPromptReceipt::decode(&request.receipt)?;
    if receipt.run_id != request.run_id
        || receipt.node_id != request.node_id
        || receipt.activation_id != request.activation_id
        || receipt.attempt != request.attempt
        || receipt.dispatch_identity != request.dispatch_identity
    {
        return Err(invalid("selected workflow observation mismatch"));
    }
    let _guard = authorize_selected_start(
        state,
        &receipt.run_id,
        &receipt.dispatch_identity,
        receipt.session_id,
        &receipt.start.instance.plugin_id,
        &receipt.start,
    )
    .await?;
    let owner = canonical_session_owner(state, receipt.session_id, 1)?;
    let current = state
        .sessions
        .external_harness_state(&owner)
        .await
        .map_err(|_| invalid("selected workflow state unavailable"))?;
    if current.instance != receipt.start.instance {
        return Err(invalid("selected workflow instance changed"));
    }
    let Some(HarnessSessionTarget::Confirmed(target)) = current.target else {
        return Ok(deferred(
            "selected workflow native session remains unconfirmed",
        ));
    };
    let evidence = state
        .sessions
        .session_external_harness_submission_evidence(
            receipt.session_id,
            &receipt.submission.submission_id,
            &receipt.submission.attempt_id,
        )
        .await
        .map_err(|_| invalid("selected workflow assignment evidence unavailable"))?;
    let Some((evidence_owner, page)) = evidence else {
        return Ok(deferred(
            "selected workflow assignment acceptance unavailable",
        ));
    };
    if evidence_owner != owner {
        return Err(invalid("selected workflow evidence owner changed"));
    }
    if page.completeness == HarnessCompleteness::Gap
        || page.status.completeness == HarnessCompleteness::Gap
        || page.status.attention != bcode_harness_models::HarnessAttention::None
    {
        return Ok(deferred(
            "selected workflow evidence gap or interaction coverage unresolved",
        ));
    }
    let native = match selected_native_turn(&receipt, &page.observations)? {
        Ok(native) => native,
        Err(observation) => return Ok(observation),
    };
    let Some(native) = native else {
        return Ok(deferred("selected workflow assignment not accepted"));
    };
    // Canonical runtime owns draining. No native turn IDs are guessed or derived from a request.
    let _ = target;
    match settled_assignment_result(&page.observations, receipt.session_id, 1, &native)? {
        Err(observation) => Ok(observation),
        Ok(result) => complete_selected_prompt(&receipt, &result),
    }
}

fn complete_selected_prompt(
    receipt: &SelectedPromptReceipt,
    result: &str,
) -> Result<AttemptObservation, WorkflowStoreError> {
    let output = selected_prompt_output(receipt, result)?;
    Ok(AttemptObservation::Succeeded {
        output: bcode_workflow_store::ValidatedOutput {
            output_id: format!("{}:selected-output", receipt.dispatch_identity),
            run_id: receipt.run_id.clone(),
            node_id: receipt.node_id.clone(),
            activation_id: receipt.activation_id.clone(),
            schema_id: receipt.output.type_name.clone(),
            schema_version: 1,
            artifact_reference: None,
            value: output,
            created_at_ms: crate::current_unix_millis(),
        },
    })
}

fn selected_prompt_output(
    receipt: &SelectedPromptReceipt,
    result: &str,
) -> Result<Value, WorkflowStoreError> {
    let output = if receipt.configuration.output
        == bcode_workflow::WorkflowPromptOutputPolicy::PreserveInput
    {
        receipt.input.clone()
    } else {
        let contribution: Value = serde_json::from_str(result)?;
        if let Some(policy) = receipt.configuration.output.structured() {
            policy
                .schema
                .validate_value("selected assignment", &contribution)
                .map_err(|_| invalid("selected assignment violates structured output schema"))?;
        }
        receipt
            .configuration
            .output
            .retain_contribution(Some(&receipt.input), contribution)
            .map_err(invalid)?
    };
    crate::validate_workflow_output_semantics(&receipt.output, &output).map_err(invalid)?;
    Ok(output)
}

/// Signal cancellation only after canonical workflow intent; acknowledgment is not settlement.
pub(crate) async fn cancel_selected_prompt(
    state: &ServerState,
    attempt: &bcode_workflow_store::ActiveAttemptCancellation,
) -> Result<(), WorkflowStoreError> {
    let receipt = SelectedPromptReceipt::decode(
        attempt
            .receipt
            .as_ref()
            .ok_or_else(|| invalid("selected cancellation receipt missing"))?,
    )?;
    if receipt.dispatch_identity != attempt.dispatch_identity || receipt.run_id != attempt.run_id {
        return Err(invalid("selected cancellation identity mismatch"));
    }
    let _guard = authorize_selected_start(
        state,
        &receipt.run_id,
        &receipt.dispatch_identity,
        receipt.session_id,
        &receipt.start.instance.plugin_id,
        &receipt.start,
    )
    .await?;
    if !crate::workflow_representation::owner::cancellation_intended(
        state,
        &receipt.run_id,
        &receipt.dispatch_identity,
    )
    .await?
    {
        return Err(invalid("selected cancellation intent required"));
    }
    let owner = canonical_session_owner(state, receipt.session_id, 1)?;
    let current = state
        .sessions
        .external_harness_state(&owner)
        .await
        .map_err(|_| invalid("selected cancellation state unavailable"))?;
    if current.instance != receipt.start.instance {
        return Err(invalid("selected cancellation instance changed"));
    }
    let target = current
        .target
        .ok_or_else(|| invalid("selected cancellation target unconfirmed"))?;
    let mut call = selected_submission_call(&receipt, target.clone());
    call.harness.request_id = format!("{}:interrupt", receipt.dispatch_identity);
    call.harness.kind =
        HarnessRequestKind::Interrupt(bcode_harness_models::HarnessControlRequest {
            target,
            mutation_id: call.harness.request_id.clone(),
        });
    let backend = state
        .managed_sessions
        .get()
        .ok_or_else(|| invalid("managed backend unavailable"))?;
    backend
        .execute(
            &crate::harness_dispatch::ManagedCaller::Workflow {
                run_id: receipt.run_id,
                dispatch_identity: receipt.dispatch_identity,
                session_id: receipt.session_id,
                plugin_id: receipt.start.instance.plugin_id,
            },
            call,
        )
        .await
        .map_err(|_| invalid("selected cancellation unresolved"))?;
    Ok(())
}

/// Stable recovery handle, published before native delivery. Its existence is not acceptance.
#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct HarnessWorkflowReceipt {
    owner: String,
    dispatch_identity: String,
    run_id: String,
    node_id: String,
    activation_id: String,
    parent_session_id: SessionId,
    session_id: SessionId,
    generation: u64,
    /// Exact plugin-prepared application request. Retains caller mutation IDs and binding;
    /// none of these portable fields substitute for current workflow/session authority.
    call: ManagedSessionCall,
    plugin_id: String,
    output_schema: bcode_workflow::ValueSchema,
    node: bcode_workflow::NodeDefinition,
    block: bcode_workflow::WorkflowBlockDefinition,
    complete_input: Value,
    #[serde(default)]
    workflow_obligation_adoption: Option<bcode_workflow::WorkflowObligationAdoptionProof>,
    #[serde(default)]
    completion_requirement: bcode_harness_models::HarnessCompletionRequirement,
}

fn invalid(message: impl Into<String>) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}
fn deferred(reason: &str) -> AttemptObservation {
    AttemptObservation::Deferred {
        reason: reason.into(),
    }
}

impl HarnessWorkflowReceipt {
    pub(crate) fn decode(value: &Value) -> Result<Self, WorkflowStoreError> {
        if serde_json::to_vec(value)?.len() > bcode_workflow::MAX_WORKFLOW_BLOCK_PREPARATION_BYTES {
            return Err(invalid("managed workflow receipt exceeds bound"));
        }
        let receipt: Self = serde_json::from_value(value.clone())?;
        if receipt.owner != HARNESS_WORKFLOW_OWNER
            || receipt.generation == 0
            || receipt.dispatch_identity.trim().is_empty()
            || receipt.dispatch_identity.len() > 4096
        {
            return Err(invalid("unsupported managed workflow receipt"));
        }
        if receipt.plugin_id != receipt.block.plugin_id
            || !owns_block(&receipt.block)
            || receipt.node.id != receipt.node_id
            || receipt.node.output != receipt.output_schema
            || receipt.node.configuration != serde_json::to_value(&receipt.block)?
        {
            return Err(invalid("managed workflow receipt contract mismatch"));
        }
        for identity in [
            &receipt.run_id,
            &receipt.node_id,
            &receipt.activation_id,
            &receipt.plugin_id,
        ] {
            if identity.trim().is_empty()
                || identity.len() > 4096
                || identity.chars().any(char::is_control)
            {
                return Err(invalid("invalid managed workflow owner identity"));
            }
        }
        validate_call(&receipt.call)?;
        if !matches!(
            (
                receipt.block.block_version,
                receipt.block.output.type_name.as_str()
            ),
            (1, "bcode.harness.assignment-result/v1") | (2, "bcode.harness.assignment-result/v2")
        ) || (receipt.block.block_version == 1
            && receipt.completion_requirement
                != bcode_harness_models::HarnessCompletionRequirement::AssociatedWork)
        {
            return Err(invalid("unsupported managed workflow block compatibility"));
        }
        let prepared = bcode_workflow::prepare_workflow_node_dataflow(
            receipt.node.dataflow,
            &receipt.node.input,
            &receipt.block.input,
            &receipt.complete_input,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let retained_requirement: bcode_harness_models::HarnessCompletionRequirement =
            serde_json::from_value(
                prepared
                    .owner_input()
                    .get("completion_requirement")
                    .cloned()
                    .unwrap_or_else(|| json!("associated_work")),
            )?;
        if retained_requirement != receipt.completion_requirement {
            return Err(invalid("managed completion requirement changed"));
        }
        if receipt.completion_requirement
            == bcode_harness_models::HarnessCompletionRequirement::WorkspaceRelease
        {
            return Err(invalid(
                "workspace release requires separate host authorization",
            ));
        }
        let HarnessRequestKind::Submit { submission, .. } = &receipt.call.harness.kind else {
            return Err(invalid("managed execution receipt requires submission"));
        };
        if submission.session_id != receipt.session_id.to_string()
            || submission.generation != receipt.generation
        {
            return Err(invalid("managed workflow receipt owner mismatch"));
        }
        if receipt.block.operation == AWAIT_OPERATION
            && (receipt
                .workflow_obligation_adoption
                .as_ref()
                .is_none_or(|proof| {
                    proof.request.destination.dispatch_identity != receipt.dispatch_identity
                        || proof.request.run_id != receipt.run_id
                })
                || receipt.completion_requirement
                    != bcode_harness_models::HarnessCompletionRequirement::AssociatedWork)
        {
            return Err(invalid(
                "await receipt requires durable adoption and associated-work responsibility",
            ));
        }
        Ok(receipt)
    }
}

fn validate_call(call: &ManagedSessionCall) -> Result<(), WorkflowStoreError> {
    call.harness
        .validate()
        .map_err(|_| invalid("invalid managed workflow request"))?;
    let HarnessRequestKind::Submit { submission, .. } = &call.harness.kind else {
        return Err(invalid("managed execution requires a retained submission"));
    };
    if call.harness.request_id != submission.submission_id
        || call.parent_session_id.is_some()
        || call
            .session_id
            .is_some_and(|id| id.to_string() != submission.session_id)
    {
        return Err(invalid(
            "managed execution correlation or association mismatch",
        ));
    }
    Ok(())
}

/// Recognize the portable prepared application contract, never a native harness vendor.
pub(crate) fn owns_block(block: &bcode_workflow::WorkflowBlockDefinition) -> bool {
    matches!(
        block.operation.as_str(),
        EXECUTE_OPERATION | AWAIT_OPERATION
    ) && block.input.type_name == "bcode.harness.managed-operation/v1"
}

fn prepared_call(
    request: &PreparedActivationDispatch,
    block: &bcode_workflow::WorkflowBlockDefinition,
) -> Result<ManagedSessionCall, WorkflowStoreError> {
    if request.intent.get("block") != Some(&serde_json::to_value(block)?)
        || request.intent.get("input") != request.activation.input.as_ref()
    {
        return Err(invalid(
            "managed workflow dispatch differs from retained intent",
        ));
    }
    let preparation: bcode_workflow::WorkflowBlockPreparationResponse = serde_json::from_value(
        request
            .intent
            .get("preparation")
            .cloned()
            .ok_or_else(|| invalid("managed execution requires preparation"))?,
    )?;
    let owner_input = crate::workflow_plugin_block_input(request, block)?;
    crate::validate_workflow_preparation_binding(block, &owner_input, &preparation)?;
    if preparation.descriptor["interface_id"].as_str() != Some(MANAGED_INTERFACE)
        || preparation.descriptor["operation"].as_str() != Some(EXECUTE_OPERATION)
        || preparation.descriptor["execution"] != json!(true)
    {
        return Err(invalid("managed execution descriptor mismatch"));
    }
    let requirement: bcode_harness_models::HarnessCompletionRequirement = serde_json::from_value(
        owner_input
            .get("completion_requirement")
            .cloned()
            .unwrap_or_else(|| json!("associated_work")),
    )?;
    if preparation
        .descriptor
        .get("completion_requirement")
        .is_some_and(|value| value != &json!(requirement))
    {
        return Err(invalid("managed prepared completion requirement changed"));
    }
    if requirement == bcode_harness_models::HarnessCompletionRequirement::WorkspaceRelease {
        return Err(invalid(
            "workspace release requires separate host authorization",
        ));
    }
    let encoded = owner_input
        .get("request_json")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid("managed execution request missing"))?;
    if encoded.len() > bcode_workflow::MAX_WORKFLOW_BLOCK_PREPARATION_BYTES {
        return Err(invalid("managed execution input exceeds bound"));
    }
    let payload: Value = serde_json::from_str(encoded)?;
    if preparation.descriptor.get("payload") != Some(&payload) {
        return Err(invalid("managed execution preparation changed"));
    }
    let call = serde_json::from_value(payload)?;
    validate_call(&call)?;
    Ok(call)
}

/// Dispatch one plugin-prepared managed execution through the installed application backend.
/// Returns `None` for unrelated blocks. The ordinary planner has already evaluated exact
/// manifest registration, tool policy and mutation approval before this function is called.
pub(crate) async fn dispatch_block(
    state: &Arc<ServerState>,
    request: &PreparedActivationDispatch,
) -> Result<Option<Value>, WorkflowStoreError> {
    let block: bcode_workflow::WorkflowBlockDefinition =
        serde_json::from_value(request.activation.node.configuration.clone())?;
    if !owns_block(&block) {
        return Ok(None);
    }
    if block.operation == AWAIT_OPERATION {
        return Err(invalid(
            "await nodes are admitted only by explicit canonical obligation adoption",
        ));
    }
    if block.operation == EXECUTE_OPERATION
        && (block.block_version != 2
            || block.output.type_name != "bcode.harness.assignment-result/v2"
            || !block.preparation_required
            || block.reconciliation != bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus)
    {
        return Err(invalid(
            "managed execution requires its exact durable output/preparation contract",
        ));
    }
    block
        .validate()
        .map_err(|_| invalid("invalid managed execution block"))?;
    if !state
        .plugins
        .registry()
        .workflow_blocks()
        .into_iter()
        .any(|candidate| candidate == block)
    {
        return Err(invalid(
            "managed execution manifest is unavailable or changed",
        ));
    }
    let call = prepared_call(request, &block)?;
    let guard = crate::workflow_operations::execution_authority(state, &request.activation.run_id)
        .await
        .map_err(|_| invalid("managed workflow authority unavailable"))?
        .ok_or_else(|| invalid("managed workflow authority required"))?;
    if let Some(value) = crate::workflow_representation::owner::check_owner_admission(
        state,
        request,
        &guard.authority,
    )
    .await?
    {
        let receipt = HarnessWorkflowReceipt::decode(&value)?;
        if receipt.dispatch_identity != request.dispatch_identity
            || receipt.complete_input
                != request
                    .activation
                    .input
                    .clone()
                    .ok_or_else(|| invalid("managed input missing"))?
            || serde_json::to_value(&receipt.call)? != serde_json::to_value(&call)?
        {
            return Err(invalid("conflicting managed workflow receipt"));
        }
        return Ok(Some(value)); // Recovery observes only; never redeliver an existing handle.
    }
    let parent = parent(state, &request.activation.run_id).await?;
    let HarnessRequestKind::Submit { submission, .. } = &call.harness.kind else {
        return Err(invalid("managed submission missing"));
    };
    let session_id: SessionId = submission
        .session_id
        .parse()
        .map_err(|_| invalid("invalid managed session identity"))?;
    let receipt = HarnessWorkflowReceipt {
        owner: HARNESS_WORKFLOW_OWNER.into(),
        dispatch_identity: request.dispatch_identity.clone(),
        run_id: request.activation.run_id.clone(),
        node_id: request.activation.node_id.clone(),
        activation_id: request.activation.activation_id.clone(),
        parent_session_id: parent,
        session_id,
        generation: submission.generation,
        call,
        plugin_id: block.plugin_id.clone(),
        output_schema: request.activation.node.output.clone(),
        workflow_obligation_adoption: None,
        completion_requirement: serde_json::from_value(
            crate::workflow_plugin_block_input(request, &block)?
                .get("completion_requirement")
                .cloned()
                .unwrap_or_else(|| json!("associated_work")),
        )?,
        node: request.activation.node.clone(),
        block: block.clone(),
        complete_input: request
            .activation
            .input
            .clone()
            .ok_or_else(|| invalid("managed execution input missing"))?,
    };
    let value = publish_execution_receipt(state, request, &guard.authority, &receipt).await?;
    deliver_published_execution(state, request, &receipt).await?;
    Ok(Some(value))
}

// Delivery remains under dispatch_block's retained workflow guard. Publication has already
// committed the recovery handle, so ambiguous delivery stays observation-owned, not failed.
async fn deliver_published_execution(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    receipt: &HarnessWorkflowReceipt,
) -> Result<(), WorkflowStoreError> {
    // Publishing the handle before native I/O guarantees cancellation/recovery can locate an
    // ambiguous effect. A crash here does not authorize automatic replay of the submission.
    if crate::workflow_representation::owner::cancellation_intended(
        state,
        &request.activation.run_id,
        &request.dispatch_identity,
    )
    .await?
    {
        return Ok(());
    }
    let response = execute(
        state,
        &request.activation.run_id,
        receipt,
        clone_call(&receipt.call)?,
    )
    .await;
    // After acceptance of the recovery handle, delivery failures are observation-owned.
    // Returning an error here could incorrectly terminalize work with native effects.
    match response {
        Ok(response) => {
            // Invalid evidence cannot terminalize an assignment whose effects may exist.
            if response.validate_for(&receipt.call.harness).is_err() {
                tracing::warn!(
                    stage = "workflow_managed_response",
                    "managed workflow delivery evidence invalid; observation required"
                );
            }
        }
        Err(_) => {
            // Fixed diagnostic only: no plugin payload, text, credentials or request JSON.
            tracing::warn!(
                stage = "workflow_managed_delivery",
                "managed workflow application rejected or could not reconcile delivery; retained receipt requires observation"
            );
        }
    }
    Ok(())
}

// Keep session verification, execution linkage and recovery-handle publication in one
// ordered phase. The caller retains workflow authority across this phase and native I/O.
async fn publish_execution_receipt(
    state: &ServerState,
    request: &PreparedActivationDispatch,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    receipt: &HarnessWorkflowReceipt,
) -> Result<Value, WorkflowStoreError> {
    verify_session(state, receipt).await?;
    let workspace = state
        .sessions
        .session_summary(receipt.session_id)
        .await
        .map_err(|_| invalid("managed execution session unavailable"))?
        .working_directory;
    let link = WorkflowExecutionSessionLink::new(
        request.activation.run_id.clone(),
        request.activation.node_id.clone(),
        request.activation.activation_id.clone(),
        request.attempt,
        receipt.session_id.to_string(),
        workspace.to_string_lossy().into_owned(),
        request.activation.created_at_ms,
    );
    crate::workflow_representation::owner::link_execution_session(state, authority, &link).await?;
    if crate::workflow_representation::owner::cancellation_intended(
        state,
        &link.run_id,
        &request.dispatch_identity,
    )
    .await?
    {
        return Err(invalid("managed workflow cancellation already requested"));
    }
    let value = serde_json::to_value(receipt)?;
    HarnessWorkflowReceipt::decode(&value)?;
    crate::workflow_representation::owner::persist_receipt(
        state,
        &DispatchReceipt {
            run_id: link.run_id,
            node_id: link.node_id,
            activation_id: link.activation_id,
            attempt: request.attempt,
            dispatch_identity: request.dispatch_identity.clone(),
            receipt: value.clone(),
            admitted_at_ms: crate::current_unix_millis(),
        },
        authority,
    )
    .await?;
    Ok(value)
}

fn clone_call(call: &ManagedSessionCall) -> Result<ManagedSessionCall, WorkflowStoreError> {
    Ok(serde_json::from_value(serde_json::to_value(call)?)?)
}

async fn parent(state: &ServerState, run_id: &str) -> Result<SessionId, WorkflowStoreError> {
    let run = crate::workflow_representation::status(state, run_id)
        .await
        .map_err(|_| invalid("managed workflow status unavailable"))?
        .ok_or_else(|| invalid("managed workflow missing"))?;
    run.parent_session_id
        .as_deref()
        .ok_or_else(|| invalid("managed workflow parent missing"))?
        .parse()
        .map_err(|_| invalid("invalid managed workflow parent"))
}

async fn verify_session(
    state: &ServerState,
    receipt: &HarnessWorkflowReceipt,
) -> Result<(), WorkflowStoreError> {
    crate::session_operations::require_selected_location(state, receipt.session_id)
        .await
        .map_err(|_| invalid("managed workflow location unavailable"))?;
    let summary = state
        .sessions
        .session_summary(receipt.session_id)
        .await
        .map_err(|_| invalid("managed workflow session unavailable"))?;
    let parent = state
        .sessions
        .session_summary(receipt.parent_session_id)
        .await
        .map_err(|_| invalid("managed workflow parent unavailable"))?;
    if summary.parent_id != Some(receipt.parent_session_id)
        || summary.working_directory != parent.working_directory
    {
        return Err(invalid(
            "managed execution requires its admitted ordinary child and workspace",
        ));
    }
    current_target(state, receipt).await?;
    Ok(())
}

// Session ownership is keyed by the daemon lease build fingerprint, not its executable
// ArtifactId. Workflow authority is verified separately before these exact actor reads;
// the actor still compares the complete persisted owner and rejects foreign/stale state.
fn canonical_session_owner(
    state: &ServerState,
    session_id: SessionId,
    generation: u64,
) -> Result<bcode_session_models::external_harness::ExternalHarnessOwner, WorkflowStoreError> {
    if state.daemon_status.artifact_id.is_none() {
        return Err(invalid("managed workflow artifact missing"));
    }
    let owner = bcode_session_models::external_harness::ExternalHarnessOwner {
        session_id,
        generation,
        artifact_id: state.daemon_status.build_fingerprint.clone(),
        daemon_id: state.daemon_status.instance_id.clone(),
    };
    owner
        .validate()
        .map_err(|_| invalid("invalid managed workflow session owner"))?;
    Ok(owner)
}

// Read the actor-owned target, including a prepared transport that has no native ID yet.
// A workflow receipt retains the original submission target forever; promotion affects only
// later observation/control routing and never rewrites or replays the first submission.
async fn current_target(
    state: &ServerState,
    receipt: &HarnessWorkflowReceipt,
) -> Result<HarnessSessionTarget, WorkflowStoreError> {
    let owner = canonical_session_owner(state, receipt.session_id, receipt.generation)?;
    let current = state
        .sessions
        .external_harness_state(&owner)
        .await
        .map_err(|_| invalid("managed workflow target unavailable"))?;
    if current.instance != receipt.call.harness.instance {
        return Err(invalid("managed workflow instance changed"));
    }
    let target = current
        .target
        .ok_or_else(|| invalid("managed workflow target missing"))?;
    target
        .validate_owner(&receipt.session_id.to_string(), receipt.generation)
        .map_err(|_| invalid("managed workflow target owner changed"))?;
    let HarnessRequestKind::Submit {
        target: original, ..
    } = &receipt.call.harness.kind
    else {
        return Err(invalid("managed workflow submission target missing"));
    };
    validate_target_transition(original, &target)?;
    Ok(target)
}

fn validate_target_transition(
    original: &HarnessSessionTarget,
    target: &HarnessSessionTarget,
) -> Result<(), WorkflowStoreError> {
    match (original, target) {
        (HarnessSessionTarget::Prepared(prepared), HarnessSessionTarget::Confirmed(binding)) => {
            original
                .validate_confirmation(&prepared.open_id, binding)
                .map_err(|_| {
                    invalid("managed workflow confirmation contradicts prepared target")
                })?;
        }
        _ if original == target => {}
        _ => {
            return Err(invalid(
                "managed workflow target changed without valid confirmation",
            ));
        }
    }
    Ok(())
}

async fn settlement_authority(
    state: &ServerState,
    run_id: &str,
) -> Result<Option<crate::workflow_operations::AuthorityGuard>, WorkflowStoreError> {
    if let Some(guard) = crate::workflow_representation::owner::settlement_authority(state, run_id)
        .await
        .map_err(|_| invalid("managed workflow authority unavailable"))?
    {
        return Ok(Some(guard));
    }
    // None is legacy routing, not permission to claim extension/foreign work. Preserve the
    // same no-takeover rule while retaining the legacy parent's canonical ownership hold.
    crate::workflow_representation::require_legacy(state, run_id)
        .await
        .map_err(|_| invalid("managed workflow representation cannot grant legacy authority"))?;
    let authority = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .execution_authority(run_id)?;
    let Some(authority) = authority
        .filter(|owner| crate::workflow_operations::authority_targets_current_daemon(state, owner))
    else {
        return Ok(None);
    };
    let parent = parent(state, run_id).await?;
    let ownership = state
        .sessions
        .acquire_session_ownership(parent, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .map_err(|_| invalid("managed workflow parent ownership unavailable"))?;
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .verify_execution_authority(run_id, &authority)?;
    Ok(Some(crate::workflow_operations::AuthorityGuard {
        authority,
        _session_ownership: Some(ownership),
    }))
}

fn adopted_link_identity<'a>(
    receipt: &'a HarnessWorkflowReceipt,
    dispatch_identity: &str,
) -> Option<(&'a String, &'a String, u32)> {
    receipt
        .workflow_obligation_adoption
        .as_ref()
        .filter(|proof| proof.request.source.dispatch_identity == dispatch_identity)
        .map(|proof| {
            (
                &proof.request.destination.node_id,
                &proof.request.destination.activation_id,
                proof.request.destination.attempt,
            )
        })
}

/// Verify the exact workflow-owned application grant for lane3 admission. The caller must
/// retain the returned authority guard through the native operation; parentage is not a grant.
/// Resolve the ordinary tool ceiling only from a currently authorized exact receipt.
/// Native arguments and parent session configuration cannot raise this capability.
pub(crate) async fn managed_tool_ceiling(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
    session_id: SessionId,
    plugin_id: &str,
    call: &ManagedSessionCall,
) -> Result<bcode_session_models::TurnExecutionOptions, WorkflowStoreError> {
    let _guard = authorize_call(
        state,
        run_id,
        dispatch_identity,
        session_id,
        plugin_id,
        call,
    )
    .await?;
    let stored =
        crate::workflow_representation::owner::execution_receipt(state, run_id, dispatch_identity)
            .await?
            .ok_or_else(|| invalid("managed tool receipt unavailable"))?;
    if stored.receipt.get("owner").and_then(Value::as_str) == Some(SELECTED_PROMPT_OWNER) {
        let receipt = SelectedPromptReceipt::decode(&stored.receipt)?;
        return Ok(bcode_session_models::TurnExecutionOptions {
            tools: match receipt.configuration.tool_capability {
                bcode_workflow::WorkflowToolCapability::Disabled => {
                    bcode_session_models::TurnToolPolicy::Disabled
                }
                bcode_workflow::WorkflowToolCapability::ReadOnly => {
                    bcode_session_models::TurnToolPolicy::ReadOnly
                }
                bcode_workflow::WorkflowToolCapability::Mutating => {
                    bcode_session_models::TurnToolPolicy::Enabled
                }
            },
            agent_profile: Some(receipt.configuration.agent_profile),
            allow_user_questions: receipt.configuration.allow_user_questions,
            tool_allowlist: (!receipt.configuration.tool_allowlist.is_empty())
                .then_some(receipt.configuration.tool_allowlist),
            permission_mode: bcode_session_models::TurnPermissionMode::Enforce,
            ..Default::default()
        });
    }
    let receipt = HarnessWorkflowReceipt::decode(&stored.receipt)?;
    let tools = match receipt.block.authorization.capability {
        bcode_workflow::WorkflowToolCapability::Disabled => {
            bcode_session_models::TurnToolPolicy::Disabled
        }
        bcode_workflow::WorkflowToolCapability::ReadOnly => {
            bcode_session_models::TurnToolPolicy::ReadOnly
        }
        bcode_workflow::WorkflowToolCapability::Mutating => {
            bcode_session_models::TurnToolPolicy::Enabled
        }
    };
    Ok(bcode_session_models::TurnExecutionOptions {
        tools,
        allow_user_questions: false,
        permission_mode: bcode_session_models::TurnPermissionMode::Enforce,
        ..Default::default()
    })
}

async fn validate_selected_call(
    state: &ServerState,
    session_id: SessionId,
    receipt: &SelectedPromptReceipt,
    call: &ManagedSessionCall,
) -> Result<(), WorkflowStoreError> {
    if call.harness.instance != receipt.start.instance
        || call.parent_session_id.is_some()
        || call.assistance_destination_session_id.is_some()
        || call.session_id != Some(session_id)
    {
        return Err(invalid("selected workflow application grant mismatch"));
    }
    let owner = canonical_session_owner(state, session_id, 1)?;
    let current = state
        .sessions
        .external_harness_state(&owner)
        .await
        .map_err(|_| invalid("selected workflow target unavailable"))?;
    if current.instance != receipt.start.instance {
        return Err(invalid("selected workflow instance changed"));
    }
    let current_target = current
        .target
        .ok_or_else(|| invalid("selected workflow target missing"))?;
    match &call.harness.kind {
        HarnessRequestKind::Submit { target, submission } => {
            if target != &current_target {
                return Err(invalid("selected workflow target changed"));
            }
            target
                .validate_owner(&session_id.to_string(), 1)
                .map_err(|_| invalid("selected workflow target changed"))?;
            if submission != &receipt.submission
                || call.harness.request_id != submission.submission_id
            {
                return Err(invalid("selected workflow assignment changed"));
            }
        }
        HarnessRequestKind::Observe(query) if query.target == current_target => {}
        HarnessRequestKind::Reconcile(query)
            if query.session_id == receipt.session_id.to_string()
                && query.generation == 1
                && query.open_id.as_ref() == Some(&receipt.start.open.open_id)
                && query.submission_id.as_ref() == Some(&receipt.submission.submission_id)
                && query.attempt_id.as_ref() == Some(&receipt.submission.attempt_id) => {}
        HarnessRequestKind::Interrupt(control)
            if control.target == current_target
                && control.mutation_id == format!("{}:interrupt", receipt.dispatch_identity)
                && call.harness.request_id == control.mutation_id => {}
        _ => return Err(invalid("selected workflow operation unsupported")),
    }
    Ok(())
}

fn validate_legacy_call_identity(
    receipt: &HarnessWorkflowReceipt,
    run_id: &str,
    dispatch_identity: &str,
    session_id: SessionId,
    plugin_id: &str,
    call: &ManagedSessionCall,
) -> Result<(), WorkflowStoreError> {
    if receipt.run_id != run_id
        || receipt.dispatch_identity != dispatch_identity
        || receipt.session_id != session_id
        || plugin_id != receipt.plugin_id
        || call.harness.instance != receipt.call.harness.instance
        || call.parent_session_id.is_some()
        || call.assistance_destination_session_id != receipt.call.assistance_destination_session_id
    {
        return Err(invalid("managed workflow application grant mismatch"));
    }
    Ok(())
}

async fn validate_legacy_execution_link(
    state: &ServerState,
    receipt: &HarnessWorkflowReceipt,
    stored: &bcode_workflow_store::DispatchReceipt,
    run_id: &str,
    dispatch_identity: &str,
    session_id: SessionId,
) -> Result<(), WorkflowStoreError> {
    let link_identity = adopted_link_identity(receipt, dispatch_identity);
    let (link_node, link_activation, link_attempt) =
        link_identity.unwrap_or((&stored.node_id, &stored.activation_id, stored.attempt));
    let link = crate::workflow_representation::owner::execution_session_link(
        state,
        run_id,
        link_node,
        link_activation,
        link_attempt,
    )
    .await?
    .ok_or_else(|| invalid("managed workflow execution link missing"))?;
    if link.session_id != session_id.to_string() {
        return Err(invalid("managed workflow execution link mismatch"));
    }
    Ok(())
}

pub(crate) async fn authorize_call(
    state: &ServerState,
    run_id: &str,
    dispatch_identity: &str,
    session_id: SessionId,
    plugin_id: &str,
    call: &ManagedSessionCall,
) -> Result<crate::workflow_operations::AuthorityGuard, WorkflowStoreError> {
    let guard = settlement_authority(state, run_id)
        .await
        .map_err(|_| invalid("managed workflow admission authority unavailable"))?
        .ok_or_else(|| invalid("managed workflow admission authority required"))?;
    let stored =
        crate::workflow_representation::owner::execution_receipt(state, run_id, dispatch_identity)
            .await?
            .ok_or_else(|| invalid("managed workflow admission receipt required"))?;
    if stored.receipt.get("owner").and_then(Value::as_str) == Some(SELECTED_PROMPT_OWNER) {
        let receipt = SelectedPromptReceipt::decode(&stored.receipt)?;
        let _start_guard = authorize_selected_start(
            state,
            run_id,
            dispatch_identity,
            session_id,
            plugin_id,
            &receipt.start,
        )
        .await?;
        validate_selected_call(state, session_id, &receipt, call).await?;
        return Ok(guard);
    }
    let receipt = HarnessWorkflowReceipt::decode(&stored.receipt)?;
    validate_legacy_call_identity(
        &receipt,
        run_id,
        dispatch_identity,
        session_id,
        plugin_id,
        call,
    )?;
    validate_legacy_execution_link(
        state,
        &receipt,
        &stored,
        run_id,
        dispatch_identity,
        session_id,
    )
    .await?;
    let HarnessRequestKind::Submit { submission, .. } = &receipt.call.harness.kind else {
        return Err(invalid("managed workflow submission missing"));
    };
    match &call.harness.kind {
        HarnessRequestKind::Submit { .. }
            if receipt.block.operation == AWAIT_OPERATION
                || receipt.workflow_obligation_adoption.is_some() =>
        {
            return Err(invalid("adopted obligation cannot resubmit native work"));
        }
        HarnessRequestKind::Submit { .. }
            if serde_json::to_value(call)? == serde_json::to_value(&receipt.call)? =>
        {
            if crate::workflow_representation::owner::cancellation_intended(
                state,
                run_id,
                dispatch_identity,
            )
            .await?
            {
                return Err(invalid(
                    "managed workflow submission fenced by cancellation intent",
                ));
            }
        }
        HarnessRequestKind::Observe(query)
            if query.target == current_target(state, &receipt).await? => {}
        HarnessRequestKind::Reconcile(query)
            if query.session_id == submission.session_id
                && query.generation == submission.generation
                && query.submission_id.as_ref() == Some(&submission.submission_id)
                && query.attempt_id.as_ref() == Some(&submission.attempt_id) => {}
        HarnessRequestKind::Interrupt(control)
            if control.target == current_target(state, &receipt).await? =>
        {
            let expected = bcode_workflow::workflow_canonical_value_sha256(
                &json!({"dispatch":dispatch_identity,"operation":"interrupt"}),
            )
            .map_err(invalid)?;
            if control.mutation_id != expected
                || !crate::workflow_representation::owner::cancellation_intended(
                    state,
                    run_id,
                    dispatch_identity,
                )
                .await?
            {
                return Err(invalid("managed workflow interruption lacks exact intent"));
            }
        }
        _ => {
            return Err(invalid(
                "managed workflow operation not granted by dispatch",
            ));
        }
    }
    verify_session(state, &receipt).await?;
    Ok(guard)
}

async fn execute(
    state: &ServerState,
    run_id: &str,
    receipt: &HarnessWorkflowReceipt,
    call: ManagedSessionCall,
) -> Result<HarnessServiceResponse, WorkflowStoreError> {
    let backend = state
        .managed_sessions
        .get()
        .ok_or_else(|| invalid("managed application unavailable"))?;
    let caller = crate::harness_dispatch::ManagedCaller::Workflow {
        run_id: run_id.into(),
        dispatch_identity: receipt.dispatch_identity.clone(),
        session_id: receipt.session_id,
        plugin_id: receipt.plugin_id.clone(),
    };
    backend.execute(&caller, call).await.map_err(|_| {
        invalid("managed application rejected or could not reconcile workflow operation")
    })
}

/// Observe a bounded indexed canonical submission snapshot, then drain one bounded native page.
/// Native/plugin unavailability is deferred; daemon loss never becomes a terminal result.
pub(crate) async fn observe_receipt(
    state: &ServerState,
    request: &AttemptReconciliationRequest,
) -> Result<AttemptObservation, WorkflowStoreError> {
    let receipt = HarnessWorkflowReceipt::decode(&request.receipt)?;
    if receipt.block.operation == AWAIT_OPERATION
        && receipt
            .workflow_obligation_adoption
            .as_ref()
            .is_none_or(|proof| {
                proof.request.destination.dispatch_identity != request.dispatch_identity
            })
    {
        return Err(invalid(
            "await observation requires exact canonical adoption proof",
        ));
    }
    if receipt.dispatch_identity != request.dispatch_identity
        || receipt.run_id != request.run_id
        || receipt.node_id != request.node_id
        || receipt.activation_id != request.activation_id
    {
        return Err(invalid("managed workflow observation identity mismatch"));
    }
    let Ok(Some(_guard)) = settlement_authority(state, &request.run_id).await else {
        return Ok(deferred("managed workflow authority unavailable"));
    };
    if verify_session(state, &receipt).await.is_err() {
        return Ok(deferred(
            "managed workflow session owner unavailable or changed",
        ));
    }
    match canonical_outcome(state, &receipt).await? {
        AttemptObservation::Deferred { .. } => {}
        outcome => return Ok(outcome),
    }
    let mut call = clone_call(&receipt.call)?;
    let target = current_target(state, &receipt).await?;
    let poll_id = bcode_workflow::workflow_canonical_value_sha256(
        &json!({"dispatch":receipt.dispatch_identity,"operation":"observe"}),
    )
    .map_err(invalid)?;
    call.harness.request_id = poll_id;
    let after = canonical_poll_cursor(state, receipt.session_id, receipt.generation).await?;
    let owner = canonical_session_owner(state, receipt.session_id, receipt.generation)?;
    let retention = state
        .sessions
        .session_harness_retention_context(&owner, &target, after.as_deref())
        .await
        .map_err(|_| invalid("canonical managed retention context unavailable"))?;
    call.harness.kind = HarnessRequestKind::Observe(bcode_harness_models::HarnessObserveRequest {
        target,
        after,
        limit: 128,
        wait_millis: 0,
        retention,
    });
    // Runtime drains continuously and commits normalized observations canonically. Re-reading
    // evidence after this poll does not interpret an empty page as completion.
    let _ = execute(state, &request.run_id, &receipt, call).await;
    canonical_outcome(state, &receipt).await
}

// Native acknowledgement uses the actor's exact atomically committed cursor, never an
// inferred last history event. This remains bounded independently of session history size.
async fn canonical_poll_cursor(
    state: &ServerState,
    session_id: SessionId,
    generation: u64,
) -> Result<Option<String>, WorkflowStoreError> {
    let owner = canonical_session_owner(state, session_id, generation)?;
    let current = state
        .sessions
        .external_harness_state(&owner)
        .await
        .map_err(|_| invalid("managed observation cursor unavailable"))?;
    Ok(current.observation_after)
}

async fn canonical_outcome(
    state: &ServerState,
    receipt: &HarnessWorkflowReceipt,
) -> Result<AttemptObservation, WorkflowStoreError> {
    let receipt = obligation::refresh_receipt(state, receipt).await?;
    canonical_outcome_snapshot(state, &receipt).await
}

async fn canonical_outcome_snapshot(
    state: &ServerState,
    receipt: &HarnessWorkflowReceipt,
) -> Result<AttemptObservation, WorkflowStoreError> {
    if receipt.completion_requirement
        == bcode_harness_models::HarnessCompletionRequirement::AsyncHandoff
        && receipt.workflow_obligation_adoption.is_none()
    {
        return Ok(deferred("async handoff awaits durable canonical adoption"));
    }
    let HarnessRequestKind::Submit { submission, .. } = &receipt.call.harness.kind else {
        return Err(invalid("missing managed submission"));
    };
    let owner = canonical_session_owner(state, receipt.session_id, receipt.generation)?;
    if let Some(delivery) = state
        .sessions
        .external_harness_receipt(&owner, &submission.submission_id, &submission.attempt_id)
        .await
        .map_err(|_| invalid("managed submission receipt unavailable"))?
        && delivery.state == bcode_harness_models::HarnessDeliveryState::Rejected
    {
        return Ok(AttemptObservation::Failed {
            message: "managed submission rejected".into(),
        });
    }
    let evidence = state
        .sessions
        .session_external_harness_submission_evidence(
            receipt.session_id,
            &submission.submission_id,
            &submission.attempt_id,
        )
        .await
        .map_err(|_| invalid("managed submission evidence unavailable"))?;
    let Some((evidence_owner, page)) = evidence else {
        return Ok(deferred("managed submission evidence unavailable"));
    };
    if evidence_owner != owner {
        return Ok(deferred("managed submission evidence owner changed"));
    }
    if page.completeness == HarnessCompleteness::Gap
        || page.status.completeness == HarnessCompleteness::Gap
    {
        return Ok(deferred("managed evidence gap"));
    }
    if page
        .observations
        .iter()
        .any(|observation| matches!(&observation.kind, HarnessObservationKind::Execution { .. }))
        && receipt.completion_requirement
            == bcode_harness_models::HarnessCompletionRequirement::AssociatedWork
        && page.status.attention != bcode_harness_models::HarnessAttention::None
    {
        return Ok(deferred("managed interaction coverage unresolved"));
    }
    if receipt.block.block_version == 1
        && receipt.block.output.type_name == "bcode.harness.assignment-result/v1"
        && page.observations.iter().any(|observation| {
            matches!(&observation.kind, HarnessObservationKind::Execution { .. })
        })
    {
        return Ok(deferred(
            "legacy managed receipt cannot adopt v3 execution semantics",
        ));
    }
    if let Some(verdict) = execution_outcome::evaluate(&page.observations, receipt)? {
        return Ok(verdict);
    }
    let mut acceptances = page.observations.iter().filter(|observation| {
        matches!(
            &observation.kind,
            HarnessObservationKind::SubmissionAccepted { .. }
        )
    });
    let Some(acceptance) = acceptances.next() else {
        return Ok(deferred(
            "managed submission has no authoritative native turn correlation",
        ));
    };
    if acceptances.next().is_some() {
        return Err(invalid("conflicting managed submission acceptance"));
    }
    let native_id = accepted_turn(acceptance, receipt)?;
    // The canonical indexed snapshot includes exact acceptance, terminal evidence and
    // applicable loss fences. Never restart a bounded scan at the history prefix.
    outcome(&page.observations, receipt, &native_id)
}

// Acceptance establishes only the submission/attempt-to-turn association, never settlement.
fn accepted_turn(
    observation: &bcode_harness_models::HarnessObservation,
    receipt: &HarnessWorkflowReceipt,
) -> Result<String, WorkflowStoreError> {
    observation
        .validate()
        .map_err(|_| invalid("invalid managed acceptance observation"))?;
    let HarnessRequestKind::Submit { submission, .. } = &receipt.call.harness.kind else {
        return Err(invalid("missing managed submission"));
    };
    let HarnessObservationKind::SubmissionAccepted {
        submission_id,
        attempt_id,
        native_turn_id,
    } = &observation.kind
    else {
        return Err(invalid(
            "managed acceptance lookup returned a different observation",
        ));
    };
    if observation.session_id != receipt.session_id.to_string()
        || observation.generation != receipt.generation
        || observation.completeness != HarnessCompleteness::Complete
        || submission_id != &submission.submission_id
        || attempt_id != &submission.attempt_id
    {
        return Err(invalid(
            "foreign, stale or incomplete managed submission acceptance",
        ));
    }
    Ok(native_turn_id.clone())
}

fn outcome(
    observations: &[bcode_harness_models::HarnessObservation],
    receipt: &HarnessWorkflowReceipt,
    native_id: &str,
) -> Result<AttemptObservation, WorkflowStoreError> {
    match settled_assignment_result(
        observations,
        receipt.session_id,
        receipt.generation,
        native_id,
    )? {
        Ok(result) => complete_assignment_output(receipt, Some(native_id), &result),
        Err(observation) => Ok(observation),
    }
}

// Shared evidence interpretation for plugin blocks and ordinary selected prompt assignments.
fn settled_assignment_result(
    observations: &[bcode_harness_models::HarnessObservation],
    session_id: SessionId,
    generation: u64,
    native_id: &str,
) -> Result<Result<String, AttemptObservation>, WorkflowStoreError> {
    let mut settled = false;
    let mut terminal = None;
    let mut result = None;
    for observation in observations {
        observation
            .validate()
            .map_err(|_| invalid("invalid managed observation"))?;
        if observation.session_id != session_id.to_string() || observation.generation != generation
        {
            return Ok(Err(deferred("foreign or stale managed observation")));
        }
        // Partial progress is not incomplete terminal evidence. Explicit loss remains a
        // fence even when a producer also supplies a complete result and settlement.
        if observation.completeness == HarnessCompleteness::Gap
            || matches!(&observation.kind, HarnessObservationKind::Status { status }
                if status.completeness == HarnessCompleteness::Gap)
        {
            return Ok(Err(deferred("managed evidence gap")));
        }
    }
    for observation in observations {
        if observation.completeness != HarnessCompleteness::Complete {
            if observation.completeness == HarnessCompleteness::Partial
                && matches!(
                    &observation.kind,
                    HarnessObservationKind::Text { .. }
                        | HarnessObservationKind::Tool { .. }
                        | HarnessObservationKind::Subagent { .. }
                        | HarnessObservationKind::Status { .. }
                )
            {
                continue;
            }
            return Ok(Err(deferred("managed evidence incomplete")));
        }
        match &observation.kind {
            HarnessObservationKind::NativeTurnTerminal {
                native_turn_id,
                disposition,
            } if native_turn_id == native_id => {
                if terminal.is_some_and(|previous| previous != *disposition) {
                    return Ok(Err(deferred(
                        "conflicting managed native terminal evidence",
                    )));
                }
                terminal = Some(*disposition);
            }
            HarnessObservationKind::Settlement { native_turn_id }
                if native_turn_id == native_id =>
            {
                settled = true;
            }
            HarnessObservationKind::Result { text }
                if observation
                    .native_correlations
                    .iter()
                    .any(|id| id == native_id) =>
            {
                result = Some(text.clone());
            }
            _ => {}
        }
    }
    if !settled {
        return Ok(Err(deferred(
            "awaiting authoritative assignment-correlated native settlement",
        )));
    }
    let Some(terminal) = terminal else {
        return Ok(Err(deferred(
            "awaiting authoritative native terminal disposition",
        )));
    };
    match terminal {
        HarnessTerminalDisposition::Interrupted => return Ok(Err(AttemptObservation::Cancelled)),
        HarnessTerminalDisposition::Failed => {
            return Ok(Err(AttemptObservation::Failed {
                message: "managed native turn failed".into(),
            }));
        }
        HarnessTerminalDisposition::Completed => {}
    }
    let Some(result) = result else {
        return Ok(Err(deferred(
            "settled native turn has no bounded assignment result",
        )));
    };
    Ok(Ok(result))
}

// Terminal evidence is checked by outcome before entering ordinary workflow dataflow.
fn complete_assignment_output(
    receipt: &HarnessWorkflowReceipt,
    native_id: Option<&str>,
    result: &str,
) -> Result<AttemptObservation, WorkflowStoreError> {
    let HarnessRequestKind::Submit { submission, .. } = &receipt.call.harness.kind else {
        return Err(invalid("missing managed submission"));
    };
    let mut output = json!({"session_id":receipt.session_id,"submission_id":submission.submission_id,"native_turn_id":native_id,"result":result});
    if matches!(
        receipt.completion_requirement,
        bcode_harness_models::HarnessCompletionRequirement::Foreground
            | bcode_harness_models::HarnessCompletionRequirement::AsyncHandoff
    ) {
        // Foreground handles retain correlation only. Async handles additionally name
        // the canonical destination receipt which now owns resource responsibility.
        let adopted = receipt
            .workflow_obligation_adoption
            .as_ref()
            .filter(|proof| proof.request.source.dispatch_identity == receipt.dispatch_identity);
        output["execution_handle"] = json!({
            "session_id":receipt.session_id, "generation":receipt.generation,
            "submission_id":submission.submission_id, "attempt_id":submission.attempt_id,
            "dispatch_identity":adopted.map_or(receipt.dispatch_identity.as_str(),
                |proof| proof.request.destination.dispatch_identity.as_str()),
        });
        if let Some(proof) = adopted {
            output["execution_handle"]["obligation_id"] = json!(proof.request.obligation_id);
            output["execution_handle"]["adoption_revision"] = json!(proof.adopted_revision);
        }
    }
    crate::validate_workflow_output_semantics(&receipt.block.output, &output).map_err(invalid)?;
    let prepared = bcode_workflow::prepare_workflow_node_dataflow(
        receipt.node.dataflow,
        &receipt.node.input,
        &receipt.block.input,
        &receipt.complete_input,
    )
    .map_err(|error| invalid(error.to_string()))?;
    let output = bcode_workflow::complete_workflow_node_dataflow(
        prepared,
        &receipt.block.output,
        &receipt.node.output,
        output,
    )
    .map_err(|error| invalid(error.to_string()))?;
    crate::validate_workflow_output_semantics(&receipt.output_schema, &output).map_err(invalid)?;
    Ok(AttemptObservation::Succeeded {
        output: bcode_workflow_store::ValidatedOutput {
            output_id: format!("output:{}", receipt.dispatch_identity),
            run_id: receipt.run_id.clone(),
            node_id: receipt.node_id.clone(),
            activation_id: receipt.activation_id.clone(),
            schema_id: receipt.output_schema.type_name.clone(),
            schema_version: 1,
            value: output,
            artifact_reference: None,
            created_at_ms: crate::current_unix_millis(),
        },
    })
}

/// Deliver cancellation only after canonical intent. Acknowledgement never settles an attempt.
pub(crate) async fn cancel_receipt(
    state: &ServerState,
    attempt: &bcode_workflow_store::ActiveAttemptCancellation,
) -> Result<(), WorkflowStoreError> {
    let stored = crate::workflow_representation::owner::execution_receipt(
        state,
        &attempt.run_id,
        &attempt.dispatch_identity,
    )
    .await?
    .ok_or_else(|| invalid("managed cancellation receipt missing"))?;
    let current = HarnessWorkflowReceipt::decode(&stored.receipt)?;
    if let Some(proof) = &current.workflow_obligation_adoption
        && proof.request.source.dispatch_identity == attempt.dispatch_identity
    {
        let guard = settlement_authority(state, &attempt.run_id)
            .await?
            .ok_or_else(|| invalid("adopted cancellation authority unavailable"))?;
        state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .propagate_owned_obligation_cancellation(
                &attempt.run_id,
                &attempt.dispatch_identity,
                &guard.authority,
                crate::current_unix_millis(),
            )?;
        let destination = crate::workflow_representation::owner::execution_receipt(
            state,
            &attempt.run_id,
            &proof.request.destination.dispatch_identity,
        )
        .await?
        .ok_or_else(|| invalid("adopted cancellation destination unavailable"))?;
        let forwarded = bcode_workflow_store::ActiveAttemptCancellation {
            run_id: destination.run_id,
            node_id: destination.node_id,
            activation_id: destination.activation_id,
            attempt: destination.attempt,
            dispatch_identity: destination.dispatch_identity,
            receipt: Some(destination.receipt),
        };
        return Box::pin(cancel_receipt_inner(state, &forwarded)).await;
    }
    cancel_receipt_inner(state, attempt).await
}

async fn cancel_receipt_inner(
    state: &ServerState,
    attempt: &bcode_workflow_store::ActiveAttemptCancellation,
) -> Result<(), WorkflowStoreError> {
    let receipt = HarnessWorkflowReceipt::decode(
        attempt
            .receipt
            .as_ref()
            .ok_or_else(|| invalid("managed cancellation receipt missing"))?,
    )?;
    if receipt.dispatch_identity != attempt.dispatch_identity {
        return Err(invalid("managed cancellation identity mismatch"));
    }
    let _guard = settlement_authority(state, &attempt.run_id)
        .await
        .map_err(|_| invalid("managed cancellation authority unavailable"))?
        .ok_or_else(|| invalid("managed cancellation authority required"))?;
    if !crate::workflow_representation::owner::cancellation_intended(
        state,
        &attempt.run_id,
        &attempt.dispatch_identity,
    )
    .await?
    {
        return Err(invalid("managed cancellation intent missing"));
    }
    verify_session(state, &receipt).await?;
    let mut call = clone_call(&receipt.call)?;
    let target = current_target(state, &receipt).await?;
    // Hash the exact dispatch identity into a bounded, deterministic control mutation ID.
    let mutation_id = bcode_workflow::workflow_canonical_value_sha256(
        &json!({"dispatch":attempt.dispatch_identity,"operation":"interrupt"}),
    )
    .map_err(invalid)?;
    call.harness.request_id = mutation_id.clone();
    call.harness.kind =
        HarnessRequestKind::Interrupt(bcode_harness_models::HarnessControlRequest {
            target,
            mutation_id: mutation_id.clone(),
        });
    // Typed semantic evidence is renderer-neutral; only explicit assignment settlement
    // can complete this block. Generic status/result observations remain non-terminal.
    let response = execute(state, &attempt.run_id, &receipt, call).await?;
    match response.kind {
        HarnessServiceResponseKind::Acknowledged { mutation_id: id } if id == mutation_id => Ok(()),
        _ => Err(invalid(
            "managed cancellation uncertain; retain intent and observe without replay",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn receipt() -> HarnessWorkflowReceipt {
        let session = SessionId::new();
        let schema = bcode_workflow::ValueSchema {
            type_name: "bcode.harness.assignment-result/v2".into(),
            schema: json!({"type":"object"}),
        };
        let block = bcode_workflow::WorkflowBlockDefinition {
            block_id: EXECUTE_OPERATION.into(),
            block_version: 2,
            plugin_id: "test.generic-harness".into(),
            operation: EXECUTE_OPERATION.into(),
            input: bcode_workflow::ValueSchema {
                type_name: "bcode.harness.managed-operation/v1".into(),
                schema: json!({"type":"object"}),
            },
            output: schema.clone(),
            effect: bcode_workflow::WorkflowBlockEffect::Mutating,
            resources: Vec::new(),
            authorization: bcode_workflow::WorkflowBlockAuthorization {
                capability: bcode_workflow::WorkflowToolCapability::Mutating,
                explicit_grant_required: true,
            },
            timeout_ms: 30000,
            cancellation_supported: true,
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
            automatic_retry: None,
            preparation_required: true,
        };
        let node: bcode_workflow::NodeDefinition = serde_json::from_value(json!({
            "id":"external", "name":"External", "kind":"plugin_block", "input":block.input,
            "output":schema,"configuration":block,
        }))
        .unwrap();
        let call: ManagedSessionCall = serde_json::from_value(json!({
            "session_id":session,"parent_session_id":null,"assistance_destination_session_id":null,
            "harness":{"schema_version":1,"request_id":"submission", "instance":{
                "instance_id":"instance","plugin_id":"test.vendor","executable":"/harness","config_namespace":"isolated",
                "auth":{"provider_id":"test.auth","profile":"native","credential_namespace":"isolated","custody":"native_managed"}},
                "kind":{"operation":"submit","request":{
                    "target":{"state":"confirmed","session":{"schema_version":1,"session_id":session,"instance_id":"instance","native_session_id":"native-session","native_version":"1.0.0","generation":2}},
                    "submission":{"submission_id":"submission","attempt_id":"attempt","session_id":session,"generation":2,"text":"Implement","placement":{"kind":"next_turn"}}
                }}}
        })).unwrap();
        HarnessWorkflowReceipt {
            owner: HARNESS_WORKFLOW_OWNER.into(),
            dispatch_identity: "dispatch".into(),
            run_id: "run".into(),
            node_id: "external".into(),
            activation_id: "activation".into(),
            parent_session_id: SessionId::new(),
            session_id: session,
            generation: 2,
            call,
            plugin_id: block.plugin_id.clone(),
            output_schema: schema,
            node,
            block,
            complete_input: json!({}),
            workflow_obligation_adoption: None,
            completion_requirement: bcode_harness_models::HarnessCompletionRequirement::default(),
        }
    }

    fn observation(
        receipt: &HarnessWorkflowReceipt,
        kind: HarnessObservationKind,
    ) -> bcode_harness_models::HarnessObservation {
        bcode_harness_models::HarnessObservation {
            observation_id: "observed".into(),
            session_id: receipt.session_id.to_string(),
            generation: 2,
            native_correlations: vec!["turn".into()],
            completeness: HarnessCompleteness::Complete,
            kind,
        }
    }

    fn terminal(
        receipt: &HarnessWorkflowReceipt,
        disposition: HarnessTerminalDisposition,
    ) -> bcode_harness_models::HarnessObservation {
        observation(
            receipt,
            HarnessObservationKind::NativeTurnTerminal {
                native_turn_id: "turn".into(),
                disposition,
            },
        )
    }

    fn selected_receipt() -> SelectedPromptReceipt {
        use bcode_model::execution::*;
        let legacy = receipt();
        let session_id = legacy.session_id;
        let instance = legacy.call.harness.instance;
        let output = bcode_workflow::ValueSchema {
            type_name: "selected-result/v1".into(),
            schema: json!({"type":"object","additionalProperties":false,
                "required":["ok"],"properties":{"ok":{"type":"boolean"}}}),
        };
        let route = ExecutionRoute::ManagedHarness {
            instance_id: instance.instance_id.clone(),
            plugin_id: instance.plugin_id.clone(),
        };
        let selection = ExecutionModelSelection {
            model_id: "observed-model".into(),
            route: Some(route.clone()),
            account_reference: Some(instance.auth.profile.clone()),
        };
        let configuration = bcode_workflow::WorkflowPromptConfiguration::structured(
            "build",
            output.clone(),
            "task",
        )
        .with_execution_model_selection(selection.clone());
        let entry = ExecutionModel {
            model: bcode_model::ModelInfo {
                model_id: selection.model_id.clone(),
                display_name: "Observed".into(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                max_image_input_base64_bytes: None,
                capabilities: std::collections::BTreeSet::default(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: None,
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                api_surface: None,
                visibility: bcode_model::ModelVisibility::Visible,
            },
            route,
            account_reference: selection.account_reference,
            billing_route: ExecutionBillingRoute::Unknown,
            native_model_id: "observed-model".into(),
            confirmed_model_id: None,
            provenance: ExecutionModelProvenance::HarnessDiscovery,
            readiness: ExecutionModelReadiness::Ready,
        };
        let resolved =
            crate::workflow_operations::execution_selection::resolve(&configuration, &[entry])
                .unwrap()
                .unwrap();
        let dispatch_identity =
            bcode_workflow_store::dispatch_identity("run", "selected", "activation", 1);
        let start = crate::workflow_operations::execution_selection::selected_start_request(
            &resolved,
            instance,
            bcode_harness_models::HarnessOpenRequest {
                open_id: format!("{dispatch_identity}:open"),
                session_id: session_id.to_string(),
                generation: 1,
                working_directory: "/workspace".into(),
                resume: None,
                parent_assistance: None,
            },
            legacy.parent_session_id,
        )
        .unwrap();
        SelectedPromptReceipt {
            owner: SELECTED_PROMPT_OWNER.into(),
            run_id: "run".into(),
            node_id: "selected".into(),
            activation_id: "activation".into(),
            attempt: 1,
            session_id,
            submission: bcode_harness_models::HarnessSubmission {
                submission_id: dispatch_identity.clone(),
                attempt_id: dispatch_identity.clone(),
                session_id: session_id.to_string(),
                generation: 1,
                text: "task".into(),
                placement: bcode_harness_models::HarnessPlacement::NextTurn,
            },
            dispatch_identity,
            start,
            resolved,
            configuration,
            input: json!({"retained":true}),
            output,
        }
    }

    #[test]
    fn selected_prompt_receipt_and_result_boundaries_fail_closed() {
        let receipt = selected_receipt();
        let wire = serde_json::to_value(&receipt).unwrap();
        SelectedPromptReceipt::decode(&wire).unwrap();
        for field in ["dispatch_identity", "activation_id", "run_id"] {
            let mut changed = wire.clone();
            changed[field] = json!("foreign");
            assert!(SelectedPromptReceipt::decode(&changed).is_err());
        }
        let mut changed = wire;
        changed["start"]["instance"]["auth"]["profile"] = json!("foreign-account");
        assert!(SelectedPromptReceipt::decode(&changed).is_err());
        assert_eq!(
            selected_prompt_output(&receipt, r#"{"ok":true}"#).unwrap(),
            json!({"ok":true})
        );
        assert!(selected_prompt_output(&receipt, r#"{"ok":"not-a-boolean"}"#).is_err());
        assert!(selected_prompt_output(&receipt, "not-json").is_err());
        assert!(matches!(
            settled_assignment_result(&[], receipt.session_id, 1, "observed-turn").unwrap(),
            Err(AttemptObservation::Deferred { .. })
        ));
    }

    #[test]
    fn typed_acceptance_requires_exact_attempt_generation_and_complete_evidence() {
        let receipt = receipt();
        let mut acceptance = observation(
            &receipt,
            HarnessObservationKind::SubmissionAccepted {
                submission_id: "submission".into(),
                attempt_id: "attempt".into(),
                native_turn_id: "native-turn".into(),
            },
        );
        assert_eq!(accepted_turn(&acceptance, &receipt).unwrap(), "native-turn");
        assert!(matches!(
            outcome(&[acceptance.clone()], &receipt, "native-turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        acceptance.generation = 1;
        assert!(accepted_turn(&acceptance, &receipt).is_err());
        acceptance.generation = 2;
        acceptance.completeness = HarnessCompleteness::Partial;
        assert!(accepted_turn(&acceptance, &receipt).is_err());
        acceptance.completeness = HarnessCompleteness::Complete;
        if let HarnessObservationKind::SubmissionAccepted { attempt_id, .. } = &mut acceptance.kind
        {
            *attempt_id = "foreign-attempt".into();
        }
        assert!(accepted_turn(&acceptance, &receipt).is_err());
        let result = observation(
            &receipt,
            HarnessObservationKind::Result {
                text: "not acceptance".into(),
            },
        );
        assert!(accepted_turn(&result, &receipt).is_err());
    }

    #[test]
    fn idle_and_result_without_settlement_do_not_complete_mixed_graph_node() {
        let receipt = receipt();
        let result = observation(
            &receipt,
            HarnessObservationKind::Result {
                text: "done".into(),
            },
        );
        assert!(matches!(
            outcome(&[result], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        assert!(matches!(
            outcome(&[], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
    }

    #[test]
    fn correlated_authoritative_settlement_produces_normal_workflow_output() {
        let receipt = receipt();
        let events = vec![
            observation(
                &receipt,
                HarnessObservationKind::Result {
                    text: "verified result".into(),
                },
            ),
            terminal(&receipt, HarnessTerminalDisposition::Completed),
            observation(
                &receipt,
                HarnessObservationKind::Settlement {
                    native_turn_id: "turn".into(),
                },
            ),
        ];
        let AttemptObservation::Succeeded { output } = outcome(&events, &receipt, "turn").unwrap()
        else {
            panic!("expected ordinary workflow output");
        };
        assert_eq!(output.run_id, "run");
        assert_eq!(output.node_id, "external");
        assert_eq!(output.activation_id, "activation");
        assert_eq!(output.value["result"], "verified result");
        assert_eq!(output.value["submission_id"], "submission");
    }

    #[test]
    fn partial_progress_does_not_poison_complete_result_and_settlement() {
        let receipt = receipt();
        let progress = [
            HarnessObservationKind::Text {
                segment_id: None,
                text: "streaming".into(),
            },
            HarnessObservationKind::Tool {
                input_json: None,
                native_tool_id: "tool".into(),
                name: "read".into(),
                status: "running".into(),
            },
            HarnessObservationKind::Status {
                status: bcode_harness_models::HarnessExecutionStatus {
                    connectivity: bcode_harness_models::HarnessConnectivity::Connected,
                    activity: bcode_harness_models::HarnessActivity::Running,
                    attention: bcode_harness_models::HarnessAttention::None,
                    continuation: bcode_harness_models::HarnessContinuation::Unknown,
                    completeness: HarnessCompleteness::Partial,
                },
            },
        ];
        let mut events = progress
            .into_iter()
            .map(|kind| {
                let mut event = observation(&receipt, kind);
                event.completeness = HarnessCompleteness::Partial;
                event
            })
            .collect::<Vec<_>>();
        events.push(observation(
            &receipt,
            HarnessObservationKind::Result {
                text: "complete answer".into(),
            },
        ));
        events.push(terminal(&receipt, HarnessTerminalDisposition::Completed));
        events.push(observation(
            &receipt,
            HarnessObservationKind::Settlement {
                native_turn_id: "turn".into(),
            },
        ));
        let AttemptObservation::Succeeded { output } = outcome(&events, &receipt, "turn").unwrap()
        else {
            panic!("partial progress must not invalidate complete terminal evidence");
        };
        assert_eq!(output.value["result"], "complete answer");
    }

    #[test]
    fn incomplete_terminal_evidence_and_unknown_progress_remain_deferred() {
        let receipt = receipt();
        for completeness in [HarnessCompleteness::Partial, HarnessCompleteness::Unknown] {
            for incomplete_index in 0..3 {
                let mut events = [
                    terminal(&receipt, HarnessTerminalDisposition::Completed),
                    observation(
                        &receipt,
                        HarnessObservationKind::Result {
                            text: "answer".into(),
                        },
                    ),
                    observation(
                        &receipt,
                        HarnessObservationKind::Settlement {
                            native_turn_id: "turn".into(),
                        },
                    ),
                ];
                events[incomplete_index].completeness = completeness;
                assert!(matches!(
                    outcome(&events, &receipt, "turn").unwrap(),
                    AttemptObservation::Deferred { .. }
                ));
            }
        }
        let mut progress = observation(
            &receipt,
            HarnessObservationKind::Text {
                segment_id: None,
                text: "unknown coverage".into(),
            },
        );
        progress.completeness = HarnessCompleteness::Unknown;
        let events = [
            progress,
            terminal(&receipt, HarnessTerminalDisposition::Completed),
            observation(
                &receipt,
                HarnessObservationKind::Result {
                    text: "answer".into(),
                },
            ),
            observation(
                &receipt,
                HarnessObservationKind::Settlement {
                    native_turn_id: "turn".into(),
                },
            ),
        ];
        assert!(matches!(
            outcome(&events, &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
    }

    #[test]
    fn explicit_progress_gaps_fence_complete_terminal_evidence() {
        let receipt = receipt();
        let mut gap = observation(
            &receipt,
            HarnessObservationKind::Text {
                segment_id: None,
                text: "lost progress".into(),
            },
        );
        gap.completeness = HarnessCompleteness::Gap;
        let status_gap = observation(
            &receipt,
            HarnessObservationKind::Status {
                status: bcode_harness_models::HarnessExecutionStatus {
                    connectivity: bcode_harness_models::HarnessConnectivity::Connected,
                    activity: bcode_harness_models::HarnessActivity::Running,
                    attention: bcode_harness_models::HarnessAttention::None,
                    continuation: bcode_harness_models::HarnessContinuation::Unknown,
                    completeness: HarnessCompleteness::Gap,
                },
            },
        );
        for mut progress in [gap, status_gap] {
            // Generation-wide loss cannot be hidden by an absent turn correlation.
            progress.native_correlations.clear();
            for gap_position in 0..=3 {
                let mut events = vec![
                    terminal(&receipt, HarnessTerminalDisposition::Completed),
                    observation(
                        &receipt,
                        HarnessObservationKind::Result {
                            text: "answer".into(),
                        },
                    ),
                    observation(
                        &receipt,
                        HarnessObservationKind::Settlement {
                            native_turn_id: "turn".into(),
                        },
                    ),
                ];
                events.insert(gap_position, progress.clone());
                assert!(matches!(
                    outcome(&events, &receipt, "turn").unwrap(),
                    AttemptObservation::Deferred { .. }
                ));
            }
        }
    }

    #[test]
    fn interrupted_and_failed_turns_require_settlement_but_not_result() {
        let receipt = receipt();
        for disposition in [
            HarnessTerminalDisposition::Interrupted,
            HarnessTerminalDisposition::Failed,
        ] {
            let terminal = terminal(&receipt, disposition);
            assert!(matches!(
                outcome(std::slice::from_ref(&terminal), &receipt, "turn").unwrap(),
                AttemptObservation::Deferred { .. }
            ));
            let events = [
                terminal,
                observation(
                    &receipt,
                    HarnessObservationKind::Settlement {
                        native_turn_id: "turn".into(),
                    },
                ),
            ];
            let mut gap_events = events.clone();
            gap_events[1].completeness = HarnessCompleteness::Gap;
            assert!(matches!(
                outcome(&gap_events, &receipt, "turn").unwrap(),
                AttemptObservation::Deferred { .. }
            ));
            match outcome(&events, &receipt, "turn").unwrap() {
                AttemptObservation::Cancelled => {
                    assert_eq!(disposition, HarnessTerminalDisposition::Interrupted);
                }
                AttemptObservation::Failed { message } => {
                    assert_eq!(disposition, HarnessTerminalDisposition::Failed);
                    assert_eq!(message, "managed native turn failed");
                }
                other => panic!("expected settled terminal disposition, got {other:?}"),
            }
        }
    }

    #[test]
    fn result_attribution_does_not_replace_native_terminal_disposition() {
        let receipt = receipt();
        let events = [
            observation(
                &receipt,
                HarnessObservationKind::SubmissionConsumed {
                    submission_id: "submission".into(),
                    attempt_id: "attempt".into(),
                    native_frame_id: "reply".into(),
                },
            ),
            observation(
                &receipt,
                HarnessObservationKind::SubmissionResultObserved {
                    submission_id: "submission".into(),
                    attempt_id: "attempt".into(),
                    native_frame_id: "result".into(),
                    is_error: false,
                    queued_user_sends: Some(0),
                },
            ),
            observation(
                &receipt,
                HarnessObservationKind::Result {
                    text: "attributed answer".into(),
                },
            ),
            observation(
                &receipt,
                HarnessObservationKind::Settlement {
                    native_turn_id: "turn".into(),
                },
            ),
        ];
        assert!(matches!(
            outcome(&events, &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        assert!(accepted_turn(&events[0], &receipt).is_err());
        assert!(accepted_turn(&events[1], &receipt).is_err());
    }

    #[test]
    fn missing_foreign_incomplete_and_conflicting_terminal_evidence_defers() {
        let receipt = receipt();
        let base = vec![
            observation(
                &receipt,
                HarnessObservationKind::Result {
                    text: "answer is not terminal proof".into(),
                },
            ),
            observation(
                &receipt,
                HarnessObservationKind::Settlement {
                    native_turn_id: "turn".into(),
                },
            ),
        ];
        assert!(matches!(
            outcome(&base, &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        let mut foreign = terminal(&receipt, HarnessTerminalDisposition::Completed);
        foreign.kind = HarnessObservationKind::NativeTurnTerminal {
            native_turn_id: "foreign-turn".into(),
            disposition: HarnessTerminalDisposition::Completed,
        };
        let mut partial = terminal(&receipt, HarnessTerminalDisposition::Completed);
        partial.completeness = HarnessCompleteness::Partial;
        for terminal in [foreign, partial] {
            let mut events = base.clone();
            events.push(terminal);
            assert!(matches!(
                outcome(&events, &receipt, "turn").unwrap(),
                AttemptObservation::Deferred { .. }
            ));
        }
        for dispositions in [
            [
                HarnessTerminalDisposition::Completed,
                HarnessTerminalDisposition::Failed,
            ],
            [
                HarnessTerminalDisposition::Interrupted,
                HarnessTerminalDisposition::Completed,
            ],
        ] {
            let mut events = base.clone();
            events.extend(dispositions.map(|disposition| terminal(&receipt, disposition)));
            assert!(matches!(
                outcome(&events, &receipt, "turn").unwrap(),
                AttemptObservation::Deferred { .. }
            ));
        }
        let mut stale = terminal(&receipt, HarnessTerminalDisposition::Interrupted);
        stale.generation += 1;
        let mut events = base;
        events.push(stale);
        assert!(matches!(
            outcome(&events, &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
    }

    #[test]
    fn stale_partial_and_foreign_turn_evidence_never_settles() {
        let receipt = receipt();
        let result = observation(
            &receipt,
            HarnessObservationKind::Result {
                text: "result".into(),
            },
        );
        let mut settled = observation(
            &receipt,
            HarnessObservationKind::Settlement {
                native_turn_id: "turn".into(),
            },
        );
        settled.generation = 1;
        assert!(matches!(
            outcome(&[result.clone(), settled.clone()], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        settled.generation = 2;
        settled.completeness = HarnessCompleteness::Gap;
        assert!(matches!(
            outcome(&[result.clone(), settled.clone()], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        settled.completeness = HarnessCompleteness::Complete;
        settled.kind = HarnessObservationKind::Settlement {
            native_turn_id: "foreign-turn".into(),
        };
        assert!(matches!(
            outcome(&[result, settled], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
    }

    #[test]
    fn first_submission_retains_prepared_target_without_inventing_native_identity() {
        let mut receipt = receipt();
        let HarnessRequestKind::Submit { target, .. } = &mut receipt.call.harness.kind else {
            panic!("submission expected");
        };
        let HarnessSessionTarget::Confirmed(binding) = target.clone() else {
            panic!("confirmed fixture expected");
        };
        let prepared =
            HarnessSessionTarget::Prepared(bcode_harness_models::HarnessPreparedSession {
                open_id: "open".into(),
                session_id: binding.session_id.clone(),
                instance_id: binding.instance_id.clone(),
                generation: binding.generation,
                native_version: binding.native_version.clone(),
                requested_native_session_id: None,
            });
        *target = prepared.clone();
        assert!(validate_call(&receipt.call).is_ok());
        assert!(validate_target_transition(&prepared, &prepared).is_ok());
        let confirmed = HarnessSessionTarget::Confirmed(binding.clone());
        assert!(validate_target_transition(&prepared, &confirmed).is_ok());
        assert!(validate_target_transition(&confirmed, &prepared).is_err());
        let mut stale = binding.clone();
        stale.generation += 1;
        assert!(
            validate_target_transition(&prepared, &HarnessSessionTarget::Confirmed(stale)).is_err()
        );
        let confirmation = observation(
            &receipt,
            HarnessObservationKind::BindingConfirmed {
                open_id: "open".into(),
                binding,
            },
        );
        assert!(matches!(
            outcome(&[confirmation], &receipt, "turn").unwrap(),
            AttemptObservation::Deferred { .. }
        ));
        let restored =
            HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).unwrap();
        let HarnessRequestKind::Submit { target, .. } = restored.call.harness.kind else {
            panic!("submission expected");
        };
        assert_eq!(target, prepared);
    }

    #[test]
    fn legacy_receipt_stays_legacy_and_cannot_select_foreground() {
        let mut receipt = receipt();
        receipt.block.block_version = 1;
        receipt.block.output.type_name = "bcode.harness.assignment-result/v1".into();
        receipt.node.output = receipt.block.output.clone();
        receipt.output_schema = receipt.node.output.clone();
        receipt.node.configuration = serde_json::to_value(&receipt.block).unwrap();
        assert!(HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).is_ok());
        receipt.completion_requirement =
            bcode_harness_models::HarnessCompletionRequirement::Foreground;
        receipt.complete_input = json!({"completion_requirement":"foreground"});
        assert!(HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).is_err());
    }

    #[test]
    fn async_source_cannot_complete_before_canonical_adoption() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessCompletionRequirement, HarnessExecutionEvent,
        };
        let mut receipt = receipt();
        receipt.completion_requirement = HarnessCompletionRequirement::AsyncHandoff;
        let event = observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal {
                    attempt: HarnessAttemptIdentity {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                    },
                    native_turn_id: None,
                    native_frame_id: Some("frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some("answer".into()),
                },
            },
        );
        assert!(matches!(
            execution_outcome::evaluate(&[event], &receipt).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
    }

    #[test]
    fn completion_requirement_cannot_change_after_receipt_publication() {
        use bcode_harness_models::HarnessCompletionRequirement;
        let receipt = receipt();
        let mut legacy = serde_json::to_value(&receipt).unwrap();
        legacy
            .as_object_mut()
            .unwrap()
            .remove("completion_requirement");
        let retained = HarnessWorkflowReceipt::decode(&legacy).unwrap();
        assert_eq!(
            retained.completion_requirement,
            HarnessCompletionRequirement::AssociatedWork
        );
        let mut receipt = receipt;
        receipt.completion_requirement = HarnessCompletionRequirement::Foreground;
        assert!(HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).is_err());
        receipt.complete_input = json!({"completion_requirement":"foreground"});
        assert!(HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).is_ok());
        receipt.completion_requirement = HarnessCompletionRequirement::WorkspaceRelease;
        receipt.complete_input = json!({"completion_requirement":"workspace_release"});
        assert!(HarnessWorkflowReceipt::decode(&serde_json::to_value(&receipt).unwrap()).is_err());
    }

    #[test]
    fn execution_foreground_is_exact_without_fabricated_native_turn() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessCompletionRequirement, HarnessExecutionEvent,
        };
        let mut receipt = receipt();
        receipt.completion_requirement = HarnessCompletionRequirement::Foreground;
        let event = observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal {
                    attempt: HarnessAttemptIdentity {
                        submission_id: "submission".into(),
                        attempt_id: "attempt".into(),
                    },
                    native_turn_id: None,
                    native_frame_id: Some("native-result-frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some("foreground answer".into()),
                },
            },
        );
        let Some(AttemptObservation::Succeeded { output }) =
            execution_outcome::evaluate(std::slice::from_ref(&event), &receipt).unwrap()
        else {
            panic!("exact foreground must complete");
        };
        assert!(output.value["native_turn_id"].is_null());
        receipt.completion_requirement = HarnessCompletionRequirement::AssociatedWork;
        assert!(matches!(
            execution_outcome::evaluate(std::slice::from_ref(&event), &receipt).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
        let mut stale = event;
        stale.generation += 1;
        assert!(matches!(
            execution_outcome::evaluate(&[stale], &receipt).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
    }

    #[test]
    fn execution_foreground_non_success_needs_no_result_or_native_turn() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessCompletionRequirement, HarnessExecutionEvent,
        };
        let mut receipt = receipt();
        receipt.completion_requirement = HarnessCompletionRequirement::Foreground;
        for disposition in [
            HarnessTerminalDisposition::Interrupted,
            HarnessTerminalDisposition::Failed,
        ] {
            let event = observation(
                &receipt,
                HarnessObservationKind::Execution {
                    event: HarnessExecutionEvent::SubmissionTerminal {
                        attempt: HarnessAttemptIdentity {
                            submission_id: "submission".into(),
                            attempt_id: "attempt".into(),
                        },
                        native_turn_id: None,
                        native_frame_id: Some("frame".into()),
                        disposition,
                        output: None,
                    },
                },
            );
            match execution_outcome::evaluate(&[event], &receipt).unwrap() {
                Some(AttemptObservation::Cancelled) => {
                    assert_eq!(disposition, HarnessTerminalDisposition::Interrupted);
                }
                Some(AttemptObservation::Failed { message }) => {
                    assert_eq!(disposition, HarnessTerminalDisposition::Failed);
                    assert_eq!(message, "managed foreground failed");
                }
                other => panic!("exact native non-success must terminalize: {other:?}"),
            }
        }
    }

    #[test]
    fn execution_foreign_attempt_and_gap_do_not_complete_associated_work() {
        use bcode_harness_models::{HarnessAttemptIdentity, HarnessExecutionEvent};
        let receipt = receipt();
        let mut event = observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: HarnessExecutionEvent::SubmissionTerminal {
                    attempt: HarnessAttemptIdentity {
                        submission_id: "foreign".into(),
                        attempt_id: "attempt".into(),
                    },
                    native_turn_id: None,
                    native_frame_id: Some("frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some("answer".into()),
                },
            },
        );
        assert!(matches!(
            execution_outcome::evaluate(std::slice::from_ref(&event), &receipt).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
        event.completeness = HarnessCompleteness::Gap;
        assert!(matches!(
            execution_outcome::evaluate(&[event], &receipt).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
    }

    #[test]
    fn execution_associated_work_requires_coverage_and_preserves_foreground() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessExecutionEvent as E, HarnessNotificationState as N,
            HarnessTrackingCoverage as C, HarnessWorkKind, HarnessWorkState as W,
        };
        let receipt = receipt();
        let attempt = HarnessAttemptIdentity {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
        };
        let terminal = observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: E::SubmissionTerminal {
                    attempt: attempt.clone(),
                    native_turn_id: None,
                    native_frame_id: Some("native-frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some("answer".into()),
                },
            },
        );
        let coverage = observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: E::Coverage {
                    attempt: attempt.clone(),
                    background: C::Complete,
                    continuations: C::Complete,
                },
            },
        );
        let mut events = vec![terminal.clone(), coverage];
        assert!(matches!(
            execution_outcome::evaluate(&events, &receipt).unwrap(),
            Some(AttemptObservation::Succeeded { .. })
        ));
        events.push(observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: E::BackgroundWork {
                    work_id: "work".into(),
                    native_task_id: None,
                    origin: Some(attempt),
                    parent_work_id: None,
                    work_kind: HarnessWorkKind::Command,
                    label: "native command".into(),
                    state: W::Failed,
                    notification: N::NotExpected,
                },
            },
        ));
        assert!(matches!(
            execution_outcome::evaluate(&events, &receipt).unwrap(),
            Some(AttemptObservation::Failed { .. })
        ));
        assert_eq!(events[0], terminal);
        let mut foreign = self::receipt();
        foreign.generation += 1;
        assert!(matches!(
            execution_outcome::evaluate(&events, &foreign).unwrap(),
            Some(AttemptObservation::Deferred { .. })
        ));
    }

    #[test]
    fn execution_snapshot_bound_counts_items_not_terminal_and_coverage_records() {
        use bcode_harness_models::{
            HarnessAttemptIdentity, HarnessExecutionEvent as E, HarnessNotificationState as N,
            HarnessTrackingCoverage as C, HarnessWorkKind, HarnessWorkState as W,
            MAX_HARNESS_EXECUTION_ITEMS,
        };
        let receipt = receipt();
        let attempt = HarnessAttemptIdentity {
            submission_id: "submission".into(),
            attempt_id: "attempt".into(),
        };
        let mut events = vec![observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: E::SubmissionTerminal {
                    attempt: attempt.clone(),
                    native_turn_id: None,
                    native_frame_id: Some("frame".into()),
                    disposition: HarnessTerminalDisposition::Completed,
                    output: Some("answer".into()),
                },
            },
        )];
        for index in 0..MAX_HARNESS_EXECUTION_ITEMS {
            events.push(observation(
                &receipt,
                HarnessObservationKind::Execution {
                    event: E::BackgroundWork {
                        work_id: format!("work-{index}"),
                        native_task_id: None,
                        origin: Some(attempt.clone()),
                        parent_work_id: None,
                        work_kind: HarnessWorkKind::Command,
                        label: "completed command".into(),
                        state: W::Completed,
                        notification: N::NotExpected,
                    },
                },
            ));
        }
        events.push(observation(
            &receipt,
            HarnessObservationKind::Execution {
                event: E::Coverage {
                    attempt,
                    background: C::Complete,
                    continuations: C::Complete,
                },
            },
        ));
        assert!(matches!(
            execution_outcome::evaluate(&events, &receipt).unwrap(),
            Some(AttemptObservation::Succeeded { .. })
        ));
    }

    #[test]
    fn state_envelope_keeps_native_and_model_graph_state_in_one_dataflow() {
        let mut receipt = receipt();
        receipt.node.dataflow = bcode_workflow::WorkflowNodeDataflowPolicy::StateEnvelopeV1;
        receipt.node.input = bcode_workflow::ValueSchema {
            type_name: "state-input".into(),
            schema: json!({"type":"object"}),
        };
        receipt.node.output = bcode_workflow::ValueSchema {
            type_name: "state-output".into(),
            schema: json!({"type":"object"}),
        };
        receipt.output_schema = receipt.node.output.clone();
        receipt.complete_input =
            json!({"schema_version":1,"state":{"model_predecessor":"retained"},"value":{}});
        let events = vec![
            observation(
                &receipt,
                HarnessObservationKind::Result {
                    text: "native result".into(),
                },
            ),
            terminal(&receipt, HarnessTerminalDisposition::Completed),
            observation(
                &receipt,
                HarnessObservationKind::Settlement {
                    native_turn_id: "turn".into(),
                },
            ),
        ];
        let AttemptObservation::Succeeded { output } = outcome(&events, &receipt, "turn").unwrap()
        else {
            panic!("expected adapted output");
        };
        assert_eq!(output.value["state"]["model_predecessor"], "retained");
        assert_eq!(output.value["value"]["result"], "native result");
        assert_eq!(output.schema_id, "state-output");
    }

    #[test]
    fn retained_receipt_round_trip_preserves_submission_and_rejects_changed_owner() {
        let receipt = receipt();
        let mut encoded = serde_json::to_value(&receipt).unwrap();
        let restored = HarnessWorkflowReceipt::decode(&encoded).unwrap();
        assert_eq!(restored.dispatch_identity, receipt.dispatch_identity);
        assert_eq!(
            serde_json::to_value(restored.call).unwrap(),
            serde_json::to_value(receipt.call).unwrap()
        );
        encoded["owner"] = json!("unknown-future-owner");
        assert!(HarnessWorkflowReceipt::decode(&encoded).is_err());
    }
}
