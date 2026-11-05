//! Compatibility-defined host operation owners. Receipt identity selects a capability,
//! never authority; each handler verifies its durable ownership before effects or observation.

use crate::ServerState;
use bcode_workflow_store::{
    ActiveAttemptCancellation, AttemptObservation, AttemptReconciliationRequest, WorkflowStoreError,
};
use std::{future::Future, pin::Pin};

type Observation<'a> =
    Pin<Box<dyn Future<Output = Result<AttemptObservation, WorkflowStoreError>> + Send + 'a>>;
type Cancellation<'a> = Pin<Box<dyn Future<Output = Result<(), WorkflowStoreError>> + Send + 'a>>;

struct OperationOwner {
    contract: &'static str,
    observe: for<'a> fn(&'a ServerState, &'a AttemptReconciliationRequest) -> Observation<'a>,
    cancel: for<'a> fn(&'a ServerState, &'a ActiveAttemptCancellation) -> Cancellation<'a>,
}

// Host application capabilities, not enabled vendor plugins or plugin scheduling state.
// Registration changes neither canonical receipt identity nor the handler's ownership fence.
static OWNERS: &[OperationOwner] = &[OperationOwner {
    contract: crate::harness_workflow::HARNESS_WORKFLOW_OWNER,
    observe: managed_observation,
    cancel: managed_cancellation,
}];

fn managed_observation<'a>(
    state: &'a ServerState,
    request: &'a AttemptReconciliationRequest,
) -> Observation<'a> {
    Box::pin(crate::harness_workflow::observe_receipt(state, request))
}

fn managed_cancellation<'a>(
    state: &'a ServerState,
    request: &'a ActiveAttemptCancellation,
) -> Cancellation<'a> {
    Box::pin(crate::harness_workflow::cancel_receipt(state, request))
}

fn owner(receipt: &serde_json::Value) -> Option<&'static OperationOwner> {
    let contract = receipt.get("owner")?.as_str()?;
    OWNERS.iter().find(|owner| owner.contract == contract)
}

/// Observe a receipt through its registered host owner, or leave unknown contracts to the
/// caller's existing compatibility path. Registration is not execution or response authority.
///
/// # Errors
/// Returns the owner's bounded contract/storage error. Unknown owners return `Ok(None)`;
/// known but malformed receipts fail through their owner rather than falling back.
pub async fn observe_operation_receipt(
    state: &ServerState,
    request: &AttemptReconciliationRequest,
) -> Result<Option<AttemptObservation>, WorkflowStoreError> {
    let Some(owner) = owner(&request.receipt) else {
        return Ok(None);
    };
    (owner.observe)(state, request).await.map(Some)
}

/// Route cancellation through the registered host capability. `true` means the handler
/// accepted the signal, not terminal settlement. The handler requires durable cancellation
/// intent and current authority independently of registry selection.
///
/// # Errors
/// Returns the owner's contract, authority or control error; unknown owners return `false`.
pub async fn cancel_operation_receipt(
    state: &ServerState,
    request: &ActiveAttemptCancellation,
) -> Result<bool, WorkflowStoreError> {
    let Some(owner) = request.receipt.as_ref().and_then(owner) else {
        return Ok(false);
    };
    (owner.cancel)(state, request).await?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn unknown_contract_defers_and_known_malformed_contract_never_falls_back() {
        let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
        let mut request = AttemptReconciliationRequest {
            run_id: "run".into(),
            node_id: "node".into(),
            activation_id: "activation".into(),
            attempt: 1,
            dispatch_identity: "dispatch".into(),
            side_effect: bcode_workflow::DispatchSideEffect::Mutating,
            receipt: json!({"owner":"unknown.future/v1"}),
        };
        assert!(
            observe_operation_receipt(&state, &request)
                .await
                .unwrap()
                .is_none()
        );
        request.receipt = json!({"owner":crate::harness_workflow::HARNESS_WORKFLOW_OWNER});
        assert!(observe_operation_receipt(&state, &request).await.is_err());
        let cancellation = ActiveAttemptCancellation {
            run_id: request.run_id,
            node_id: request.node_id,
            activation_id: request.activation_id,
            attempt: request.attempt,
            dispatch_identity: request.dispatch_identity,
            receipt: Some(request.receipt),
        };
        assert!(
            cancel_operation_receipt(&state, &cancellation)
                .await
                .is_err()
        );
        drop(state);
    }
}
