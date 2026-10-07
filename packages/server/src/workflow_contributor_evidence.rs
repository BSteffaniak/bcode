//! Bounded model-only reads after an authenticated direct-contributor proof.
//!
//! The wrapper must re-prove caller authority and contributor identity before calling and
//! before releasing the response. This module cannot select a contributor session.
use bcode_session_models::{
    EXECUTION_SESSION_PROVENANCE_VERSION, ExecutionSessionProvenance, SessionHistoryCursor,
    SessionHistoryDirection, SessionHistoryPage, SessionHistoryQuery, SessionId,
};
use bcode_workflow::contributor_evidence::{
    CONTRACT_ID, ContributorEventReference, ContributorEventRole, ContributorEvidenceAction,
    ContributorEvidenceScope, ContributorEvidenceStatus, ContributorEvidenceText,
    ContributorInvocationEvidence, ExecutionContributorEvidenceRequest,
    ExecutionContributorEvidenceResponse,
};
use bcode_workflow_store::WorkflowExecutionSessionLink;
use bcode_workflow_store::representation::contributor_evidence::{
    DirectContributorObservation, DirectContributorReference,
};

use crate::{ServerError, ServerState, TurnCancelState, workflow_contributor_events as events};

fn cancelled(cancellation: &TurnCancelState) -> Result<(), ServerError> {
    if cancellation.is_cancelled() {
        Err(ServerError::WorkflowComputationCancelled(
            "contributor evidence".into(),
        ))
    } else {
        Ok(())
    }
}

fn unavailable() -> ExecutionContributorEvidenceResponse {
    ExecutionContributorEvidenceResponse {
        contract: CONTRACT_ID.into(),
        status: ContributorEvidenceStatus::ProvenanceUnavailable,
        scope: None,
        events: Vec::new(),
        scanned_events: 0,
        next_sequence: None,
        evidence: None,
    }
}

fn scope(reference: &DirectContributorReference) -> ContributorEvidenceScope {
    ContributorEvidenceScope {
        run_id: reference.run_id.clone(),
        consumer_activation_id: reference.consumer_activation_id.clone(),
        graph_revision: reference.graph_revision,
        node_id: reference.execution.node_id.clone(),
        activation_id: reference.execution.activation_id.clone(),
        attempt: reference.execution.attempt,
        session_id: reference.execution.session_id.clone(),
        output_id: reference.output_id.clone(),
        output_checksum_sha256: reference.output_checksum_sha256.clone(),
    }
}

fn matches_provenance(
    provenance: &ExecutionSessionProvenance,
    link: &WorkflowExecutionSessionLink,
) -> bool {
    provenance.version == EXECUTION_SESSION_PROVENANCE_VERSION
        && provenance.owner == "bcode.workflow"
        && provenance.run_id == link.run_id
        && provenance.node_id == link.node_id
        && provenance.activation_id.as_deref() == Some(link.activation_id.as_str())
        && provenance.attempt == link.attempt
        && provenance.workspace_snapshot.as_deref() == Some(link.workspace_snapshot.as_str())
}

async fn verify_summary(
    state: &ServerState,
    link: &WorkflowExecutionSessionLink,
    id: SessionId,
    cancellation: &TurnCancelState,
) -> Result<bool, ServerError> {
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ServerError::WorkflowComputationCancelled("contributor evidence".into())),
        result = state.sessions.session_summary(id) => result,
    };
    cancelled(cancellation)?;
    Ok(result.is_ok_and(|summary| {
        summary.id == id
            && summary
                .execution
                .as_ref()
                .is_some_and(|execution| matches_provenance(&execution.provenance, link))
    }))
}

async fn page(
    state: &ServerState,
    id: SessionId,
    sequence: u64,
    limit: usize,
    cancellation: &TurnCancelState,
) -> Result<Option<SessionHistoryPage>, ServerError> {
    let query = SessionHistoryQuery {
        cursor: Some(SessionHistoryCursor { sequence }),
        limit,
        direction: SessionHistoryDirection::Forward,
    };
    let result = tokio::select! {
        biased;
        () = cancellation.cancelled() => return Err(ServerError::WorkflowComputationCancelled("contributor evidence".into())),
        result = state.sessions.session_history_page(id, query) => result,
    };
    cancelled(cancellation)?;
    Ok(result.ok())
}

fn text(value: events::EvidenceText) -> ContributorEvidenceText {
    ContributorEvidenceText {
        text: value.text,
        original_bytes: value.original_bytes,
        retained_bytes: value.retained_bytes,
        truncated: value.truncated,
    }
}

fn invocation(value: events::InvocationEvidence) -> ContributorInvocationEvidence {
    ContributorInvocationEvidence {
        invocation_id: value.invocation_id,
        request_sequence: value.request_sequence,
        result_sequence: value.result_sequence,
        producer_plugin_id: value.producer_plugin_id,
        tool_name: value.tool_name,
        arguments_json: text(value.arguments_json),
        model_output: text(value.model_output),
        is_error: value.is_error,
        producer_capture_complete: value.producer_capture_complete,
    }
}

fn discovery(
    value: events::Discovery,
    reference: &DirectContributorReference,
) -> ExecutionContributorEvidenceResponse {
    ExecutionContributorEvidenceResponse {
        contract: CONTRACT_ID.into(),
        status: ContributorEvidenceStatus::Available,
        scope: Some(scope(reference)),
        scanned_events: value.scanned_events,
        next_sequence: value.next_sequence,
        events: value
            .events
            .into_iter()
            .map(|event| ContributorEventReference {
                sequence: event.sequence,
                invocation_id: event.invocation_id,
                role: match event.role {
                    events::EventRole::Request => ContributorEventRole::Request,
                    events::EventRole::Result => ContributorEventRole::Result,
                },
            })
            .collect(),
        evidence: None,
    }
}

fn read_response(
    value: events::InvocationEvidence,
    reference: &DirectContributorReference,
) -> ExecutionContributorEvidenceResponse {
    ExecutionContributorEvidenceResponse {
        contract: CONTRACT_ID.into(),
        status: ContributorEvidenceStatus::Available,
        scope: Some(scope(reference)),
        scanned_events: 2,
        next_sequence: None,
        events: Vec::new(),
        evidence: Some(invocation(value)),
    }
}

/// Read only the execution session in a caller-authenticated contributor reference.
///
/// The authenticated wrapper must re-prove authority/revision after this returns and before
/// releasing any response. Session/projection failures are explicitly unavailable; cancellation
/// is terminal. No error includes tool content, and no command or artifact access occurs.
pub async fn compose(
    state: &ServerState,
    reference: &DirectContributorReference,
    request: &ExecutionContributorEvidenceRequest,
    cancellation: &TurnCancelState,
) -> Result<ExecutionContributorEvidenceResponse, ServerError> {
    cancelled(cancellation)?;
    if request.validate().is_err()
        || reference.graph_revision != request.expected_revision
        || reference.run_id != reference.execution.run_id
        || reference.execution.version
            != bcode_workflow_store::WORKFLOW_EXECUTION_SESSION_LINK_VERSION
    {
        return Ok(unavailable());
    }
    let link = &reference.execution;
    let Ok(id) = link.session_id.parse::<SessionId>() else {
        return Ok(unavailable());
    };
    if !verify_summary(state, link, id, cancellation).await? {
        return Ok(unavailable());
    }
    let projected = match &request.action {
        ContributorEvidenceAction::Discover {
            after_sequence,
            limit,
        } => {
            let after = after_sequence.unwrap_or(0);
            let Some(history) = page(state, id, after, usize::from(*limit), cancellation).await?
            else {
                return Ok(unavailable());
            };
            events::discover(id, &history, after, usize::from(*limit))
                .map_or_else(|_| unavailable(), |value| discovery(value, reference))
        }
        ContributorEvidenceAction::Read {
            invocation_id,
            request_sequence,
            result_sequence,
            byte_limit,
        } => {
            let Some(request_page) = page(state, id, *request_sequence, 1, cancellation).await?
            else {
                return Ok(unavailable());
            };
            let Some(result_page) = page(state, id, *result_sequence, 1, cancellation).await?
            else {
                return Ok(unavailable());
            };
            events::read_pair(
                &events::PairQuery {
                    session_id: id,
                    invocation_id,
                    request_sequence: *request_sequence,
                    result_sequence: *result_sequence,
                    byte_limit: usize::try_from(*byte_limit).map_err(|_| denied())?,
                },
                &request_page,
                &result_page,
            )
            .map_or_else(|_| unavailable(), |value| read_response(value, reference))
        }
    };
    if !verify_summary(state, link, id, cancellation).await? {
        return Ok(unavailable());
    }
    cancelled(cancellation)?;
    Ok(projected)
}

fn denied() -> ServerError {
    ServerError::WorkflowApplicationOperationUnauthorized(
        "contributor evidence requires a current workflow execution".into(),
    )
}

async fn caller_provenance(
    state: &ServerState,
    session_id: SessionId,
) -> Result<ExecutionSessionProvenance, ServerError> {
    let session = state
        .sessions
        .session_summary(session_id)
        .await
        .map_err(|_| denied())?;
    let provenance = session.execution.ok_or_else(denied)?.provenance;
    if session.id != session_id
        || provenance.version != EXECUTION_SESSION_PROVENANCE_VERSION
        || provenance.owner != "bcode.workflow"
        || provenance.activation_id.is_none()
    {
        return Err(denied());
    }
    Ok(provenance)
}

/// Authenticate the invoking execution, fence cancellation, and re-prove identical scope
/// after bounded evidence retrieval. Legacy execution has no supported proof and never falls back.
pub async fn from_invocation(
    state: &ServerState,
    session_id: SessionId,
    request: &ExecutionContributorEvidenceRequest,
    cancellation: &TurnCancelState,
) -> Result<ExecutionContributorEvidenceResponse, ServerError> {
    cancelled(cancellation)?;
    if request.validate().is_err() {
        return Ok(unavailable());
    }
    state.require_workflow_store()?;
    let operation = async {
        let provenance = caller_provenance(state, session_id).await?;
        // Same gate as execution_context: cancellation closes its token before waiting for
        // the gate. Reads are cancellation-raced; no persistence transaction spans the reads.
        let commit = cancellation.marker_commit.lock().await;
        cancelled(cancellation)?;
        let Some(execution) =
            crate::workflow_representation::graph::authenticate(state, session_id, &provenance)
                .await?
        else {
            let mut response = unavailable();
            response.status = ContributorEvidenceStatus::Unsupported;
            return Ok(response);
        };
        let observed = execution
            .direct_contributor(request.expected_revision)
            .await?;
        let reference = match observed {
            DirectContributorObservation::Available(reference) => reference,
            DirectContributorObservation::Unsupported => {
                let mut response = unavailable();
                response.status = ContributorEvidenceStatus::Unsupported;
                return Ok(response);
            }
            DirectContributorObservation::ProvenanceUnavailable => return Ok(unavailable()),
        };
        let response = compose(state, &reference, request, cancellation).await?;
        let current = caller_provenance(state, session_id).await?;
        if current != provenance {
            return Ok(unavailable());
        }
        let Some(reauthenticated) =
            crate::workflow_representation::graph::authenticate(state, session_id, &current)
                .await?
        else {
            return Ok(unavailable());
        };
        let reobserved = reauthenticated
            .direct_contributor(request.expected_revision)
            .await?;
        if reobserved != DirectContributorObservation::Available(reference) {
            return Ok(unavailable());
        }
        cancelled(cancellation)?;
        drop(commit);
        Ok(response)
    };
    tokio::select! {
        biased;
        () = cancellation.cancelled() => Err(ServerError::WorkflowComputationCancelled("contributor evidence".into())),
        result = operation => result,
    }
}

#[cfg(test)]
mod tests;
