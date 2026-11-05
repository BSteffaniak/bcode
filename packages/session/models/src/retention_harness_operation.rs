//! Borrowed capacity accounting for canonical managed operation payloads.
use super::{Inspector, ObservationRetentionExceeded};
use crate::external_harness::{
    ExternalHarnessOperation, ExternalHarnessOperationRecord, ExternalHarnessRequest,
    ExternalHarnessRequestKind, ExternalHarnessResponseKind,
};
use bcode_harness_models::{
    HarnessCapabilities, HarnessInstance, HarnessPlacement, HarnessReceipt, HarnessReconciliation,
    HarnessResponseValue, HarnessSessionBinding, HarnessSessionTarget,
};
type Result = std::result::Result<(), ObservationRetentionExceeded>;
fn instance(i: &mut Inspector, v: &HarnessInstance) -> Result {
    for text in [
        &v.instance_id,
        &v.plugin_id,
        &v.executable,
        &v.config_namespace,
        &v.auth.provider_id,
        &v.auth.profile,
        &v.auth.credential_namespace,
    ] {
        i.text(text, 5)?;
    }
    Ok(())
}
fn binding(i: &mut Inspector, v: &HarnessSessionBinding) -> Result {
    i.nodes(2, 5)?;
    for text in [
        &v.session_id,
        &v.instance_id,
        &v.native_session_id,
        &v.native_version,
    ] {
        i.text(text, 6)?;
    }
    Ok(())
}
fn target(i: &mut Inspector, v: &HarnessSessionTarget) -> Result {
    match v {
        HarnessSessionTarget::Confirmed(v) => binding(i, v),
        HarnessSessionTarget::Prepared(v) => {
            i.nodes(1, 5)?;
            for text in [&v.open_id, &v.session_id, &v.instance_id, &v.native_version] {
                i.text(text, 6)?;
            }
            i.optional_text(v.requested_native_session_id.as_ref(), 6)
        }
    }
}
fn capabilities(i: &mut Inspector, v: &HarnessCapabilities) -> Result {
    for capability in [
        &v.resume,
        &v.steering,
        &v.approval,
        &v.questions,
        &v.assistance,
        &v.interrupt,
        &v.authoritative_settlement,
        &v.background_continuation,
        &v.authentication,
    ] {
        i.nodes(1, 5)?;
        i.collection(capability.constraints.capacity(), 6)?;
        for text in &capability.constraints {
            i.text(text, 7)?;
        }
    }
    Ok(())
}
fn response_value(i: &mut Inspector, v: &HarnessResponseValue) -> Result {
    match v {
        HarnessResponseValue::Approval { .. } | HarnessResponseValue::Cancel => i.nodes(1, 6)?,
        HarnessResponseValue::Answer { choice_id, text } => {
            i.optional_text(choice_id.as_ref(), 6)?;
            i.text(text, 6)?;
        }
        HarnessResponseValue::Assistance { text } => i.text(text, 6)?,
    }
    Ok(())
}
fn receipt(i: &mut Inspector, v: &HarnessReceipt) -> Result {
    for text in [&v.submission_id, &v.attempt_id, &v.session_id] {
        i.text(text, 6)?;
    }
    i.optional_text(v.native_request_id.as_ref(), 6)?;
    i.nodes(2, 5)
}
fn request(i: &mut Inspector, v: &ExternalHarnessRequest) -> Result {
    i.nodes(2, 4)?;
    i.text(&v.request_id, 5)?;
    instance(i, &v.instance)?;
    match &v.kind {
        ExternalHarnessRequestKind::Open(v) => {
            for text in [&v.open_id, &v.session_id, &v.working_directory] {
                i.text(text, 6)?;
            }
            i.nodes(2, 5)?;
            if let Some(v) = &v.resume {
                binding(i, v)?;
            }
            if let Some(v) = &v.parent_assistance {
                i.nodes(2, 6)?;
                for text in [
                    &v.admission_id,
                    &v.child_session_id,
                    &v.parent_session_id,
                    &v.scope,
                ] {
                    i.text(text, 7)?;
                }
            }
        }
        ExternalHarnessRequestKind::Submit {
            target: v,
            submission,
        } => {
            target(i, v)?;
            i.nodes(2, 5)?;
            for text in [
                &submission.submission_id,
                &submission.attempt_id,
                &submission.session_id,
                &submission.text,
            ] {
                i.text(text, 6)?;
            }
            if let HarnessPlacement::Steer {
                expected_native_turn_id,
            } = &submission.placement
            {
                i.text(expected_native_turn_id, 7)?;
            }
        }
        ExternalHarnessRequestKind::Respond {
            target: v,
            response,
        } => {
            target(i, v)?;
            i.nodes(1, 5)?;
            for text in [
                &response.response_id,
                &response.request_id,
                &response.session_id,
            ] {
                i.text(text, 6)?;
            }
            response_value(i, &response.value)?;
        }
        ExternalHarnessRequestKind::Interrupt(v) | ExternalHarnessRequestKind::Close(v) => {
            target(i, &v.target)?;
            i.text(&v.mutation_id, 6)?;
        }
        ExternalHarnessRequestKind::Reconcile(v) => {
            i.text(&v.session_id, 6)?;
            i.nodes(1, 5)?;
            for text in [&v.open_id, &v.submission_id, &v.attempt_id] {
                i.optional_text(text.as_ref(), 6)?;
            }
            if let Some(v) = &v.binding {
                binding(i, v)?;
            }
        }
    }
    Ok(())
}
pub(super) fn inspect(i: &mut Inspector, op: &ExternalHarnessOperation) -> Result {
    i.nodes(3, 3)?;
    i.text(&op.owner.artifact_id, 4)?;
    i.text(&op.owner.daemon_id, 4)?;
    match &op.record {
        ExternalHarnessOperationRecord::Configured {
            instance: v,
            capabilities: c,
            open_request,
        } => {
            instance(i, v)?;
            capabilities(i, c)?;
            if let Some(v) = open_request {
                request(i, v)?;
            }
        }
        ExternalHarnessOperationRecord::DeliveryClaimed { request: v } => request(i, v)?,
        ExternalHarnessOperationRecord::DeliveryCompleted {
            request: v,
            response,
        } => {
            request(i, v)?;
            i.nodes(2, 4)?;
            i.text(&response.request_id, 5)?;
            match &response.kind {
                ExternalHarnessResponseKind::Prepared(v) => {
                    i.nodes(1, 5)?;
                    for text in [&v.open_id, &v.session_id, &v.instance_id, &v.native_version] {
                        i.text(text, 6)?;
                    }
                    i.optional_text(v.requested_native_session_id.as_ref(), 6)?;
                }
                ExternalHarnessResponseKind::Opened(v) => binding(i, v)?,
                ExternalHarnessResponseKind::Receipt(v) => receipt(i, v)?,
                ExternalHarnessResponseKind::Acknowledged { mutation_id } => {
                    i.text(mutation_id, 6)?;
                }
                ExternalHarnessResponseKind::Reconciled(HarnessReconciliation::Known {
                    binding: v,
                    receipt: r,
                    ..
                }) => {
                    binding(i, v)?;
                    if let Some(v) = r {
                        receipt(i, v)?;
                    }
                    i.nodes(5, 5)?;
                }
                ExternalHarnessResponseKind::Reconciled(HarnessReconciliation::KnownPrepared {
                    prepared: v,
                    receipt: r,
                    ..
                }) => {
                    i.nodes(6, 5)?;
                    for text in [&v.open_id, &v.session_id, &v.instance_id, &v.native_version] {
                        i.text(text, 6)?;
                    }
                    i.optional_text(v.requested_native_session_id.as_ref(), 6)?;
                    if let Some(v) = r {
                        receipt(i, v)?;
                    }
                }
                ExternalHarnessResponseKind::Reconciled(_)
                | ExternalHarnessResponseKind::Failure(_) => i.nodes(1, 5)?,
            }
        }
        ExternalHarnessOperationRecord::ObservationPage {
            instance: v,
            after,
            page,
        } => {
            instance(i, v)?;
            i.optional_text(after.as_ref(), 4)?;
            i.optional_text(page.next_after.as_ref(), 5)?;
            i.nodes(7, 4)?;
            i.collection(page.observations.capacity(), 5)?;
            for observation in &page.observations {
                super::external::observation(i, observation)?;
            }
        }
    }
    Ok(())
}
