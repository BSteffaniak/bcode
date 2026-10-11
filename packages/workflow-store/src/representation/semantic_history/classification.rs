//! Exact semantic classification after source-fact verification.
use super::{DatabaseValue, Error, Row, integer, text};
use bcode_workflow::WorkflowSemanticHistoryPayload as Payload;

pub(super) fn validate(
    before: Option<&Row>,
    expected: &Row,
    prepared: &crate::PreparedAttempt,
    payload: &Payload,
    recorded_at_ms: u64,
) -> Result<(), Error> {
    for field in [
        "dispatch_identity",
        "run_id",
        "prepared_json",
        "node_id",
        "activation_id",
        "attempt_number",
    ] {
        if before.is_some_and(|old| old.get(field) != expected.get(field)) {
            return Err(Error::MaintenanceRequired);
        }
    }
    let valid = match payload {
        Payload::AttemptPrepared { side_effect } => {
            before.is_none()
                && *side_effect == prepared.side_effect
                && expected.get("receipt_json") == Some(DatabaseValue::Null)
                && expected.get("outcome_json") == Some(DatabaseValue::Null)
                && expected.get("terminal_at_ms") == Some(DatabaseValue::Null)
                && recorded_at_ms == prepared.prepared_at_ms
        }
        Payload::ReceiptCommitted {} => {
            before.is_some_and(|old| {
                old.get("receipt_json") == Some(DatabaseValue::Null)
                    && old.get("outcome_json") == Some(DatabaseValue::Null)
                    && expected.get("outcome_json") == Some(DatabaseValue::Null)
                    && expected.get("terminal_at_ms") == Some(DatabaseValue::Null)
            }) && serde_json::from_str::<crate::DispatchReceipt>(&text(expected, "receipt_json")?)
                .is_ok_and(|receipt| {
                    receipt.dispatch_identity == prepared.dispatch_identity()
                        && receipt.run_id == prepared.run_id
                        && receipt.node_id == prepared.node_id
                        && receipt.activation_id == prepared.activation_id
                        && receipt.attempt == prepared.attempt
                        && receipt.admitted_at_ms == recorded_at_ms
                })
        }
        Payload::AttemptTerminal { outcome } => {
            let normalized = match outcome {
                bcode_workflow::WorkflowSemanticAttemptOutcome::Succeeded => {
                    super::super::attempts::TerminalOutcome::Succeeded
                }
                bcode_workflow::WorkflowSemanticAttemptOutcome::Failed => {
                    super::super::attempts::TerminalOutcome::Failed
                }
                bcode_workflow::WorkflowSemanticAttemptOutcome::Cancelled => {
                    super::super::attempts::TerminalOutcome::Cancelled
                }
            };
            before.is_some_and(|old| {
                old.get("outcome_json") == Some(DatabaseValue::Null)
                    && old.get("receipt_json") != Some(DatabaseValue::Null)
                    && old.get("receipt_json") == expected.get("receipt_json")
            }) && text(expected, "outcome_json")?
                == serde_json::to_string(&normalized).map_err(|_| Error::InvalidAdmission)?
                && integer(expected, "terminal_at_ms")?
                    == i64::try_from(recorded_at_ms).map_err(|_| Error::InvalidAdmission)?
        }
    };
    if valid {
        Ok(())
    } else {
        Err(Error::MaintenanceRequired)
    }
}
