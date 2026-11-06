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
fn open(i: &mut Inspector, v: &bcode_harness_models::HarnessOpenRequest) -> Result {
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
    Ok(())
}
pub(super) fn selected_open(
    i: &mut Inspector,
    selected: &crate::external_harness::ExternalHarnessSelectedOpen,
) -> Result {
    i.nodes(3, 5)?;
    i.text(&selected.request.request_id, 6)?;
    instance(i, &selected.request.instance)?;
    if let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { open: v, selection } =
        &selected.request.kind
    {
        open(i, v)?;
        i.text(&selection.model_id, 7)?;
    }
    Ok(())
}

fn request(i: &mut Inspector, v: &ExternalHarnessRequest) -> Result {
    i.nodes(2, 4)?;
    i.text(&v.request_id, 5)?;
    instance(i, &v.instance)?;
    match &v.kind {
        ExternalHarnessRequestKind::Open(v) => open(i, v)?,
        ExternalHarnessRequestKind::OpenSelected(selected) => selected_open(i, selected)?,
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
fn inventory_commitments(i: &mut Inspector, op: &ExternalHarnessOperation) -> Result {
    i.collection(op.inventory_commitments.capacity(), 4)?;
    for commitment in &op.inventory_commitments {
        i.nodes(10, 5)?;
        i.text(&commitment.snapshot_key, 6)?;
        i.optional_text(commitment.root.as_ref(), 6)?;
    }
    Ok(())
}

fn transport_release(
    i: &mut Inspector,
    release: &crate::external_harness::ExternalHarnessTransportRelease,
) -> Result {
    i.nodes(3, 4)?;
    instance(i, &release.instance)?;
    binding(i, &release.binding)?;
    for text in [
        &release.close_request_id,
        &release.reap_id,
        &release.lifetime.daemon_instance_id,
        &release.lifetime.artifact_id,
        &release.lifetime.state_location_id,
    ] {
        i.text(text, 6)?;
    }
    Ok(())
}

fn recovery(
    i: &mut Inspector,
    recovery: &crate::external_harness::ExternalHarnessRecovery,
) -> Result {
    transport_release(i, &recovery.release)?;
    i.nodes(2, 4)?;
    i.text(&recovery.previous_owner.artifact_id, 5)?;
    i.text(&recovery.previous_owner.daemon_id, 5)?;
    capabilities(i, &recovery.capabilities)?;
    request(i, &recovery.resume_request)
}

fn configured(
    i: &mut Inspector,
    value: &HarnessInstance,
    policy: &HarnessCapabilities,
    open: Option<&ExternalHarnessRequest>,
) -> Result {
    instance(i, value)?;
    capabilities(i, policy)?;
    if let Some(open) = open {
        request(i, open)?;
    }
    Ok(())
}

fn selected_result(
    i: &mut Inspector,
    v: &HarnessSessionTarget,
    evidence: &bcode_harness_models::HarnessModelEvidence,
) -> Result {
    i.nodes(1, 5)?;
    target(i, v)?;
    i.text(&evidence.requested_model_id, 6)?;
    i.optional_text(evidence.effective_model_id.as_ref(), 6)
}

pub(super) fn inspect(i: &mut Inspector, op: &ExternalHarnessOperation) -> Result {
    i.nodes(3, 3)?;
    i.text(&op.owner.artifact_id, 4)?;
    i.text(&op.owner.daemon_id, 4)?;
    inventory_commitments(i, op)?;
    record(i, &op.record)
}

fn record(i: &mut Inspector, record: &ExternalHarnessOperationRecord) -> Result {
    match record {
        ExternalHarnessOperationRecord::TransportReleased { release } => {
            transport_release(i, release)?;
        }
        ExternalHarnessOperationRecord::RecoveryPrepared { recovery } => {
            self::recovery(i, recovery)?;
        }
        ExternalHarnessOperationRecord::Configured {
            instance: v,
            capabilities: c,
            open_request,
        } => configured(i, v, c, open_request.as_ref())?,
        ExternalHarnessOperationRecord::DeliveryClaimed { request: v } => request(i, v)?,
        ExternalHarnessOperationRecord::DeliveryCompleted {
            request: v,
            response,
        } => {
            request(i, v)?;
            i.nodes(2, 4)?;
            i.text(&response.request_id, 5)?;
            match &response.kind {
                ExternalHarnessResponseKind::Selected {
                    target: v,
                    evidence,
                } => selected_result(i, v, evidence)?,
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
            if let Some(retention) = &page.retention {
                i.nodes(3, 5)?;
                i.text(&retention.connection_id, 6)?;
                i.text(&retention.page_id, 6)?;
                i.text(&retention.observations_digest, 6)?;
            }
            i.collection(page.hydration_requests.capacity(), 5)?;
            for request in &page.hydration_requests {
                i.nodes(4, 6)?;
                i.text(&request.request_id, 7)?;
                i.text(&request.reference.native_id, 7)?;
                i.optional_text(request.reference.native_turn_id.as_ref(), 7)?;
            }
            if let Some(inventory) = &page.inventory_page {
                i.nodes(4, 5)?;
                i.text(&inventory.scan_id, 6)?;
                i.optional_text(inventory.next_cursor.as_ref(), 6)?;
            }
            i.nodes(7, 4)?;
            i.collection(page.observations.capacity(), 5)?;
            for observation in &page.observations {
                super::external::observation(i, observation)?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod selected_tests {
    use super::*;

    fn inspector() -> Inspector {
        Inspector {
            usage: crate::ObservationRetentionUsage::default(),
            limits: crate::ObservationRetentionLimits {
                scalar_capacity_bytes: usize::MAX,
                collection_entries: usize::MAX,
                value_nodes: usize::MAX,
                depth: usize::MAX,
                copies: usize::MAX,
            },
        }
    }

    #[test]
    fn selected_evidence_accounts_for_effective_model_spare_capacity() {
        let target = HarnessSessionTarget::Prepared(bcode_harness_models::HarnessPreparedSession {
            open_id: "open".into(),
            session_id: "session".into(),
            instance_id: "instance".into(),
            generation: 1,
            native_version: "version".into(),
            requested_native_session_id: None,
        });
        let mut evidence = bcode_harness_models::HarnessModelEvidence {
            requested_model_id: "requested-model".into(),
            effective_model_id: Some("effective-model".into()),
        };
        let mut before = inspector();
        selected_result(&mut before, &target, &evidence).unwrap();
        let effective = evidence.effective_model_id.as_mut().unwrap();
        let old_capacity = effective.capacity();
        effective.reserve(4096);
        let added_capacity = effective.capacity() - old_capacity;
        let mut after = inspector();
        selected_result(&mut after, &target, &evidence).unwrap();
        assert_eq!(
            after.usage.scalar_capacity_bytes - before.usage.scalar_capacity_bytes,
            added_capacity
        );
        assert_eq!(after.usage.value_nodes, before.usage.value_nodes);
        assert_eq!(after.usage.depth, before.usage.depth);
    }

    #[test]
    fn selected_request_accounts_for_spare_model_and_nested_envelope_capacity() {
        let instance = HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/native".into(),
            config_namespace: "namespace".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "provider".into(),
                profile: "profile".into(),
                credential_namespace: "credentials".into(),
                custody: bcode_harness_models::HarnessAuthCustody::NativeManaged,
            },
        };
        let mut retained = ExternalHarnessRequest::from_models_request(
            &bcode_harness_models::HarnessModelsRequest {
                schema_version: 1,
                request_id: "open".into(),
                instance,
                kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                    open: Box::new(bcode_harness_models::HarnessOpenRequest {
                        open_id: "open".into(),
                        session_id: "session".into(),
                        generation: 1,
                        working_directory: "/workspace".into(),
                        resume: None,
                        parent_assistance: None,
                    }),
                    selection: bcode_harness_models::HarnessModelSelection {
                        model_id: "model".into(),
                    },
                },
            },
        )
        .unwrap();
        let mut before = inspector();
        request(&mut before, &retained).unwrap();
        let ExternalHarnessRequestKind::OpenSelected(selected) = &mut retained.kind else {
            unreachable!()
        };
        let bcode_harness_models::HarnessModelsRequestKind::OpenSelected { selection, .. } =
            &mut selected.request.kind
        else {
            unreachable!()
        };
        let old_capacity = selection.model_id.capacity();
        selection.model_id.reserve(4096);
        let added_capacity = selection.model_id.capacity() - old_capacity;
        let mut after = inspector();
        request(&mut after, &retained).unwrap();
        assert_eq!(
            after.usage.scalar_capacity_bytes - before.usage.scalar_capacity_bytes,
            added_capacity
        );
        assert_eq!(after.usage.value_nodes, before.usage.value_nodes);
        let ExternalHarnessRequestKind::OpenSelected(selected) = &mut retained.kind else {
            unreachable!()
        };
        let old_capacity = selected.request.instance.auth.profile.capacity();
        selected.request.instance.auth.profile.reserve(2048);
        let added_capacity = selected.request.instance.auth.profile.capacity() - old_capacity;
        let mut nested = inspector();
        request(&mut nested, &retained).unwrap();
        assert_eq!(
            nested.usage.scalar_capacity_bytes - after.usage.scalar_capacity_bytes,
            added_capacity
        );
    }
}
