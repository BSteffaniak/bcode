//! Body-free, operation-correlated compaction diagnostics.

use super::*;
use bcode_session_models::CompactionDiagnostic;
use std::{cell::RefCell, future::Future};

tokio::task_local! {
    static CONTEXT: RefCell<CompactionDiagnostic>;
}

pub(super) async fn run<T: Send>(future: impl Future<Output = T> + Send) -> T {
    let future: std::pin::Pin<Box<dyn Future<Output = T> + Send + '_>> = Box::pin(future);
    CONTEXT
        .scope(
            RefCell::new(CompactionDiagnostic {
                version: 1,
                operation_id: None,
                activation_id: uuid::Uuid::new_v4().to_string(),
                request_id: None,
                contract: None,
                stage: "operation".into(),
                outcome: "started".into(),
                reason: None,
                chunk: None,
                attempt: None,
                response: None,
                marker_sequence: None,
            }),
            future,
        )
        .await
}

pub(super) fn position(chunk: Option<usize>, attempt: usize) {
    let _ = CONTEXT.try_with(|context| {
        let mut context = context.borrow_mut();
        context.chunk = chunk;
        context.attempt = Some(attempt);
        context.request_id = None;
    });
}

pub(super) fn contract(value: response::ResponseContract) {
    let name = match value {
        response::ResponseContract::Update => "update",
        response::ResponseContract::Replace => "replace",
        response::ResponseContract::Recollection => "recollection",
        response::ResponseContract::Consolidation => "consolidation",
    };
    let _ = CONTEXT.try_with(|context| context.borrow_mut().contract = Some(name.into()));
}

pub(super) fn request(id: &str) {
    let _ = CONTEXT.try_with(|context| context.borrow_mut().request_id = Some(id.into()));
}

pub(super) fn publication() -> Option<CompactionDiagnostic> {
    CONTEXT
        .try_with(|context| {
            let mut value = context.borrow().clone();
            value.stage = "publication".into();
            value.outcome = "committed".into();
            value.reason = None;
            value.response = None;
            value
        })
        .ok()
}

fn bind(key: &str) {
    let _ = CONTEXT.try_with(|context| {
        context.borrow_mut().operation_id = Some(hex::encode(sha2::Sha256::digest(key.as_bytes())));
    });
}

pub(super) async fn start(
    state: &ServerState,
    session_id: SessionId,
    key: &str,
) -> Result<(), CompactionError> {
    bind(key);
    emit(state, session_id, "operation", "started", None, None).await
}

pub(super) async fn finish<T: Sync>(
    state: &ServerState,
    session_id: SessionId,
    result: &Result<T, CompactionError>,
) -> Result<(), CompactionError> {
    // A successful commit already includes its outcome in the publication transaction.
    if result.is_ok() {
        return Ok(());
    }
    let outcome = match result {
        Err(CompactionError::Cancelled) => "cancelled",
        Err(CompactionError::OperationBlocked | CompactionError::RecoveryRequired(_)) => "blocked",
        _ => "failed",
    };
    emit(state, session_id, "operation", outcome, None, None).await
}

pub(super) async fn emit(
    state: &ServerState,
    session_id: SessionId,
    phase: &str,
    outcome: &str,
    reason: Option<&str>,
    response: Option<&str>,
) -> Result<(), CompactionError> {
    let Ok(mut value) = CONTEXT.try_with(|context| context.borrow().clone()) else {
        return Ok(());
    };
    if phase == "operation" {
        value.request_id = None;
        value.contract = None;
        value.chunk = None;
        value.attempt = None;
    }
    value.stage = phase.into();
    value.outcome = outcome.into();
    value.reason = reason.map(str::to_owned);
    value.response = response.map(|text| bcode_session_models::CompactionResponseDiagnostic {
        bytes: text.len(),
        sha256: hex::encode(sha2::Sha256::digest(text.as_bytes())),
    });
    let trace = value
        .into_trace(
            current_unix_millis(),
            compaction_trace_owner(state, session_id).await,
        )
        .map_err(CompactionError::Provider)?;
    let event = state.sessions.append_trace_event(session_id, trace).await?;
    publish_session_event(state, &event).await;
    Ok(())
}

pub(super) const fn reason(reason: synthesis::Rejection) -> &'static str {
    use synthesis::Rejection as R;
    match reason {
        R::WireBudget => "wire_budget",
        R::InvalidJson | R::JsonSyntax { .. } => "invalid_json",
        R::Schema { .. } => "schema",
        R::InvalidShape | R::UnknownField => "invalid_shape",
        R::UnsupportedFormat => "unsupported_format",
        R::EmptyContinuation => "empty_continuation",
        R::CarryBudget => "carry_budget",
        R::InvalidSource => "invalid_source",
        R::InvalidSupersession => "invalid_supersession",
        R::AmbiguousGroupSupersession => "ambiguous_group_supersession",
        R::UnauthorizedScopeChange => "unauthorized_scope_change",
        R::MissingScope => "missing_scope",
        R::InvalidReconciliation => "invalid_reconciliation",
        R::IncompleteOutput => "incomplete_output",
        R::PreviousAttemptUnavailable => "previous_attempt_unavailable",
    }
}

#[cfg(test)]
#[path = "context_compaction_diagnostics_tests.rs"]
mod tests;
