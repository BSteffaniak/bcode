//! Canonical transaction tests: adoption never delivers native work.
use super::*;
use bcode_workflow::{ResourceAccess, ResourceClaim, WorkflowObligationAttempt};

struct Fixture {
    _root: tempfile::TempDir,
    store: WorkflowStore,
    authority: WorkflowExecutionAuthority,
    request: WorkflowObligationAdoption,
    intent: serde_json::Value,
    destination_receipt: serde_json::Value,
}
fn fixture_block(operation: &str) -> WorkflowBlockDefinition {
    let schema = bcode_workflow::ValueSchema {
        type_name: "test.object/v1".into(),
        schema: serde_json::json!({"type":"object"}),
    };
    WorkflowBlockDefinition {
        block_id: operation.into(),
        block_version: 2,
        plugin_id: "test.harness".into(),
        operation: operation.into(),
        input: schema.clone(),
        output: schema,
        effect: bcode_workflow::WorkflowBlockEffect::Mutating,
        resources: vec![],
        authorization: bcode_workflow::WorkflowBlockAuthorization {
            capability: bcode_workflow::WorkflowToolCapability::Mutating,
            explicit_grant_required: true,
        },
        timeout_ms: 30000,
        cancellation_supported: true,
        reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
        automatic_retry: None,
        preparation_required: true,
    }
}

fn initialize_fixture_run(store: &mut WorkflowStore) -> WorkflowExecutionAuthority {
    let schema = bcode_workflow::ValueSchema {
        type_name: "test.object/v1".into(),
        schema: serde_json::json!({"type":"object"}),
    };
    let node = |id: &str, operation: &str| bcode_workflow::NodeDefinition {
        id: id.into(),
        name: id.into(),
        kind: bcode_workflow::NodeKind::PluginBlock,
        input: schema.clone(),
        output: schema.clone(),
        configuration: serde_json::to_value(fixture_block(operation)).unwrap(),
        resources: vec![ResourceClaim {
            resource: "workspace".into(),
            access: ResourceAccess::Write,
        }],
        dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
    };
    let graph = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "adoption".into(),
        input: schema.clone(),
        output: schema.clone(),
        nodes: BTreeMap::from([
            ("source".into(), node("source", "harness.execute")),
            ("destination".into(), node("destination", "harness.await")),
        ]),
        entries: vec!["source".into(), "destination".into()],
        exits: vec!["source".into(), "destination".into()],
        edges: vec![],
    };
    store.persist_definition("adoption", 1, &graph).unwrap();
    let authority = WorkflowExecutionAuthority {
        target_artifact_id: "artifact".into(),
        daemon_instance_id: "daemon".into(),
        generation: 1,
        fencing_token: "fence".into(),
    };
    store
        .create_run(&NewWorkflowRun {
            run_id: "adoption-run".into(),
            definition_id: "adoption".into(),
            definition_version: 1,
            workspace_snapshot: "workspace".into(),
            parent_session_id: Some("parent".into()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!({})),
            execution_authority: Some(authority.clone()),
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: WorkflowRunLimits::default(),
        })
        .unwrap();
    authority
}

fn authorize_fixture_destination(
    store: &mut WorkflowStore,
    destination: &PendingActivation,
) -> (
    bcode_workflow::WorkflowBlockPreparationResponse,
    serde_json::Value,
) {
    let preparation = bcode_workflow::WorkflowBlockPreparationResponse {
        version: bcode_workflow::WORKFLOW_BLOCK_PREPARATION_VERSION,
        input_sha256: bcode_workflow::workflow_block_input_sha256(
            destination.input.as_ref().unwrap(),
        )
        .unwrap(),
        owner_id: "test.harness".into(),
        operation_facts: serde_json::json!({"capability":"mutating"}),
        descriptor: serde_json::json!({"operation":"harness.await","admission":"deferred_until_adopted"}),
        diagnostics: vec![],
    };
    let scope = serde_json::to_value(bcode_workflow::WorkflowMutationGrantScope {
        version: bcode_workflow::WORKFLOW_MUTATION_GRANT_SCOPE_VERSION,
        definition_id: "adoption".into(),
        definition_version: 1,
        run_id: "adoption-run".into(),
        node_id: "destination".into(),
        activation_id: destination.activation_id.clone(),
        workspace_snapshot: "workspace".into(),
        plugin_id: "test.harness".into(),
        block_id: "harness.await".into(),
        block_version: 2,
        operation: "harness.await".into(),
        input_checksum_sha256: preparation.input_sha256.clone(),
        operation_facts: Some(preparation.operation_facts.clone()),
        preparation_descriptor_sha256: Some(digest(&preparation.descriptor).unwrap()),
        input_summary: serde_json::json!({}),
        resource_claims: destination.node.resources.clone(),
        reconciliation: bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus,
        capability: bcode_workflow::WorkflowToolCapability::Mutating,
    })
    .unwrap();
    store
        .persist_grant(&WorkflowGrant {
            grant_id: "destination-grant".into(),
            run_id: "adoption-run".into(),
            node_id: "destination".into(),
            scope: scope.clone(),
            granted_at_ms: 3,
            expires_at_ms: None,
            max_uses: Some(1),
            uses_consumed: 0,
        })
        .unwrap();
    (preparation, scope)
}

fn fixture() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let mut store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    let authority = initialize_fixture_run(&mut store);
    let pending = store
        .pending_activations_for_run("adoption-run", 10)
        .unwrap();
    let source = pending.iter().find(|p| p.node_id == "source").unwrap();
    let destination = pending.iter().find(|p| p.node_id == "destination").unwrap();
    let prepared = store
        .prepare_pending_activation(
            "adoption-run",
            "source",
            &source.activation_id,
            DispatchSideEffect::Mutating,
            serde_json::json!({}),
            2,
        )
        .unwrap()
        .unwrap();
    let source_receipt = serde_json::json!({"owner":"test.native","completion_requirement":"async_handoff",
        "run_id":"adoption-run", "node_id":"source", "activation_id":source.activation_id,
        "dispatch_identity":prepared.dispatch_identity, "session_id":"native-session", "generation":1,
        "parent_session_id":"parent", "plugin_id":"test.harness", "call":{"harness":{"kind":{"request":{"submission":{
            "submission_id":"submission","attempt_id":"attempt"}}}}}});
    store
        .persist_owned_dispatch_receipt(
            &DispatchReceipt {
                run_id: "adoption-run".into(),
                node_id: "source".into(),
                activation_id: source.activation_id.clone(),
                attempt: 1,
                dispatch_identity: prepared.dispatch_identity.clone(),
                receipt: source_receipt.clone(),
                admitted_at_ms: 3,
            },
            &authority,
        )
        .unwrap();
    store
        .link_execution_session(&WorkflowExecutionSessionLink::new(
            "adoption-run".into(),
            "source".into(),
            source.activation_id.clone(),
            1,
            "native-session".into(),
            "workspace".into(),
            3,
        ))
        .unwrap();
    let (preparation, scope) = authorize_fixture_destination(&mut store, destination);
    let request = WorkflowObligationAdoption { obligation_id:prepared.dispatch_identity.clone(),run_id:"adoption-run".into(),expected_revision:1,ownership_generation:1,
        source:WorkflowObligationAttempt { node_id:"source".into(),activation_id:source.activation_id.clone(),attempt:1,dispatch_identity:prepared.dispatch_identity },
        destination:WorkflowObligationAttempt { node_id:"destination".into(),activation_id:destination.activation_id.clone(),attempt:1,
            dispatch_identity:dispatch_identity("adoption-run","destination",&destination.activation_id,1) },
        source_receipt_sha256:digest(&source_receipt).unwrap(),evidence_subject:digest(&serde_json::json!({
            "session_id":"native-session","generation":1,"submission_id":"submission","attempt_id":"attempt"
        })).unwrap(),evidence_revision:3,evidence_sha256:"b".repeat(64),
        resource_lease_ids:store.resource_leases_for_run("adoption-run",100).unwrap().into_iter().map(|lease|lease.lease_id).collect(),
        destination_grant_id:"destination-grant".into(),destination_grant_scope_sha256:digest(&scope).unwrap() };
    let intent = serde_json::json!({"block":fixture_block("harness.await"),"input":destination.input,
        "preparation":preparation,"adoption":request});
    let mut destination_receipt = source_receipt;
    destination_receipt["completion_requirement"] = serde_json::json!("associated_work");
    destination_receipt["node_id"] = serde_json::json!("destination");
    destination_receipt["activation_id"] = serde_json::json!(request.destination.activation_id);
    destination_receipt["dispatch_identity"] =
        serde_json::json!(request.destination.dispatch_identity);
    Fixture {
        _root: root,
        store,
        authority,
        request,
        intent,
        destination_receipt,
    }
}

#[test]
fn adoption_carries_unchanged_authorization_in_pages_without_reviving_stale_bindings() {
    let f = fixture();
    let tx = database::begin(&f.store.connection, TransactionMode::Immediate).unwrap();
    // Multiple pages for the same node exercise both components of the cursor.
    for index in 0..205 {
        let activation = NewActivation {
            run_id: "adoption-run".into(),
            node_id: "source".into(),
            activation_id: format!("extra-{index:03}"),
            dependency_generation: index + 1,
            input: Some(serde_json::json!({})),
            created_at_ms: 2,
        };
        insert_activation(tx.as_ref(), &activation).unwrap();
    }
    database::commit(tx).unwrap();
    f.store
        .adopt_owned_obligation(
            &f.request,
            &f.authority,
            &f.intent,
            &f.destination_receipt,
            4,
        )
        .unwrap();
    for index in 0..205 {
        assert!(
            run_graph::reconciled_activation_exit(
                &f.store.connection,
                "adoption-run",
                "source",
                &format!("extra-{index:03}")
            )
            .unwrap()
        );
    }
    // A later unrelated revision without a retention record must remain fenced.
    database::block_on(
        f.store
            .connection
            .update("workflow_run_graphs")
            .value("revision", 3_i64)
            .where_eq("run_id", "adoption-run")
            .execute(&f.store.connection),
    )
    .unwrap();
    let tx = database::begin(&f.store.connection, TransactionMode::Immediate).unwrap();
    let mut next = f.request.clone();
    next.expected_revision = 3;
    preserve_adoption_graph_bindings(tx.as_ref(), &next, 4).unwrap();
    database::block_on(
        tx.update("workflow_run_graphs")
            .value("revision", 4_i64)
            .where_eq("run_id", "adoption-run")
            .execute(tx.as_ref()),
    )
    .unwrap();
    database::commit(tx).unwrap();
    assert!(
        run_graph::reconciled_activation_exit(
            &f.store.connection,
            "adoption-run",
            "source",
            "extra-204"
        )
        .is_err()
    );
}

#[test]
fn source_cannot_release_resources_before_adoption_and_competitors_remain_blocked() {
    let mut f = fixture();
    let source = AttemptReconciliationRequest {
        run_id: f.request.run_id.clone(),
        node_id: f.request.source.node_id.clone(),
        activation_id: f.request.source.activation_id.clone(),
        attempt: 1,
        dispatch_identity: f.request.source.dispatch_identity.clone(),
        side_effect: DispatchSideEffect::Mutating,
        receipt: f
            .store
            .dispatch_receipt(&f.request.source.dispatch_identity)
            .unwrap()
            .unwrap()
            .receipt,
    };
    let before = f
        .store
        .resource_leases_for_run("adoption-run", 100)
        .unwrap();
    let mut competitor = before[0].clone();
    competitor.lease_id = "competing-workspace-claim".into();
    assert!(
        f.store.acquire_resource_lease(&competitor).is_err(),
        "source workspace remains excluded before adoption"
    );
    assert_eq!(
        before,
        f.store
            .resource_leases_for_run("adoption-run", 100)
            .unwrap()
    );
    let tx = database::begin(&f.store.connection, TransactionMode::Immediate).unwrap();
    let mut summary = ReceiptReconciliationSummary::default();
    assert!(
        crate::receipts::apply_attempt_observation(
            tx.as_ref(),
            &source,
            AttemptObservation::Cancelled,
            4,
            &mut summary
        )
        .is_err()
    );
    drop(tx);
    assert!(
        f.store
            .prepare_pending_activation_with_authority(
                &f.request.run_id,
                &f.request.destination.node_id,
                &f.request.destination.activation_id,
                DispatchSideEffect::Mutating,
                f.intent.clone(),
                4,
                Some(&f.authority)
            )
            .unwrap()
            .is_none()
    );
    f.store
        .adopt_owned_obligation(
            &f.request,
            &f.authority,
            &f.intent,
            &f.destination_receipt,
            4,
        )
        .unwrap();
    let tx = database::begin(&f.store.connection, TransactionMode::Immediate).unwrap();
    crate::receipts::apply_attempt_observation(
        tx.as_ref(),
        &source,
        AttemptObservation::Cancelled,
        5,
        &mut summary,
    )
    .unwrap();
    database::commit(tx).unwrap();
    let held = f
        .store
        .resource_leases_for_run("adoption-run", 100)
        .unwrap();
    assert_eq!(held.len(), 1);
    assert_eq!(held[0].activation_id, f.request.destination.activation_id);
    assert_eq!(
        f.store
            .retained_obligation_adoption("adoption-run", &source.dispatch_identity)
            .unwrap()
            .unwrap()
            .adopted_revision,
        2
    );
}

#[test]
fn owner_reserved_admission_defers_before_any_attempt_or_resource_mutation() {
    let f = fixture();
    let before = f.store.attempt_history("adoption-run", None, 10).unwrap();
    let leases = f
        .store
        .resource_leases_for_run("adoption-run", 100)
        .unwrap();
    assert!(
        f.store
            .prepare_pending_activation_with_authority(
                &f.request.run_id,
                &f.request.destination.node_id,
                &f.request.destination.activation_id,
                DispatchSideEffect::Mutating,
                f.intent.clone(),
                4,
                Some(&f.authority)
            )
            .unwrap()
            .is_none()
    );
    assert_eq!(
        f.store.attempt_history("adoption-run", None, 10).unwrap(),
        before
    );
    assert_eq!(
        f.store
            .resource_leases_for_run("adoption-run", 100)
            .unwrap(),
        leases
    );
    let mut invalid = f.intent.clone();
    invalid["preparation"]["descriptor"]["admission"] =
        serde_json::json!("unknown_privileged_mode");
    assert!(
        f.store
            .prepare_pending_activation_with_authority(
                &f.request.run_id,
                &f.request.destination.node_id,
                &f.request.destination.activation_id,
                DispatchSideEffect::Mutating,
                invalid,
                4,
                Some(&f.authority)
            )
            .is_err()
    );
    assert_eq!(
        f.store.attempt_history("adoption-run", None, 10).unwrap(),
        before
    );
}

#[test]
fn adoption_atomically_moves_claim_and_recovers_lost_ack_without_native_effect() {
    let f = fixture();
    let proof = f
        .store
        .adopt_owned_obligation(
            &f.request,
            &f.authority,
            &f.intent,
            &f.destination_receipt,
            4,
        )
        .unwrap();
    assert_eq!(proof.adopted_revision, 2);
    let leases = f
        .store
        .resource_leases_for_run("adoption-run", 100)
        .unwrap();
    assert_eq!(leases.len(), 1);
    assert_eq!(leases[0].activation_id, f.request.destination.activation_id);
    assert!(
        f.store
            .dispatch_receipt(&f.request.destination.dispatch_identity)
            .unwrap()
            .is_some()
    );
    assert_eq!(f.store.run_graph_revision("adoption-run").unwrap(), Some(2));
    assert_eq!(
        f.store
            .adopt_owned_obligation(
                &f.request,
                &f.authority,
                &f.intent,
                &f.destination_receipt,
                5
            )
            .unwrap(),
        proof
    );
    assert_eq!(
        f.store
            .grant("destination-grant")
            .unwrap()
            .unwrap()
            .uses_consumed,
        1
    );
    assert_eq!(
        f.store
            .attempt_history("adoption-run", None, 10)
            .unwrap()
            .len(),
        2
    );
}

#[test]
fn schema43_requires_explicit_migration_before_adoption_contract_is_enabled() {
    let root = tempfile::tempdir().unwrap();
    let store = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    database::block_on(
        store
            .connection
            .update("workflow_store_contract")
            .value("schema_version", 43_i64)
            .where_eq("contract_id", 1_i64)
            .execute(&store.connection),
    )
    .unwrap();
    database::block_on(
        store
            .connection
            .update("workflow_storage_compatibility")
            .value("compatibility", 3_i64)
            .where_eq("contract_id", 1_i64)
            .execute(&store.connection),
    )
    .unwrap();
    drop(store);
    let receipt = WorkflowStore::migrate_to_current_in_state_dir(root.path(), 10).unwrap();
    assert_eq!(receipt.previous_schema_version, 43);
    assert_eq!(receipt.new_schema_version, 44);
    let reopened = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert_eq!(detected_typed_store_schema(&reopened.connection), Some(44));
    assert_eq!(
        read_typed_contract_value(
            &reopened.connection,
            "workflow_storage_compatibility",
            "compatibility"
        )
        .unwrap(),
        Some(4)
    );
}

#[test]
fn adopted_cancellation_is_durable_nonterminal_and_keeps_resource_lease() {
    let f = fixture();
    f.store
        .adopt_owned_obligation(
            &f.request,
            &f.authority,
            &f.intent,
            &f.destination_receipt,
            4,
        )
        .unwrap();
    database::block_on(
        f.store
            .connection
            .update("workflow_attempts")
            .value("status", "cancelling")
            .where_eq(
                "dispatch_identity",
                f.request.source.dispatch_identity.as_str(),
            )
            .execute(&f.store.connection),
    )
    .unwrap();
    let destination = f
        .store
        .propagate_owned_obligation_cancellation(
            "adoption-run",
            &f.request.source.dispatch_identity,
            &f.authority,
            5,
        )
        .unwrap();
    assert_eq!(destination, f.request.destination.dispatch_identity);
    assert_eq!(
        f.store
            .propagate_owned_obligation_cancellation(
                "adoption-run",
                &f.request.source.dispatch_identity,
                &f.authority,
                6
            )
            .unwrap(),
        destination
    );
    let attempts = f.store.attempt_history("adoption-run", None, 10).unwrap();
    assert_eq!(
        attempts
            .iter()
            .find(|a| a.dispatch_identity == destination)
            .unwrap()
            .status,
        "cancelling"
    );
    assert_eq!(
        f.store
            .resource_leases_for_run("adoption-run", 100)
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn adopted_receipt_recovers_after_store_restart_without_new_attempt_or_grant_use() {
    let Fixture {
        _root: root,
        store,
        authority,
        request,
        intent,
        destination_receipt,
    } = fixture();
    let proof = store
        .adopt_owned_obligation(&request, &authority, &intent, &destination_receipt, 4)
        .unwrap();
    drop(store);
    let reopened = WorkflowStore::open_in_state_dir(root.path()).unwrap();
    assert_eq!(
        reopened
            .adopt_owned_obligation(&request, &authority, &intent, &destination_receipt, 5)
            .unwrap(),
        proof
    );
    assert_eq!(
        reopened
            .attempt_history("adoption-run", None, 10)
            .unwrap()
            .len(),
        2
    );
    assert_eq!(
        reopened
            .grant("destination-grant")
            .unwrap()
            .unwrap()
            .uses_consumed,
        1
    );
    assert_eq!(
        reopened
            .resource_leases_for_run("adoption-run", 100)
            .unwrap()[0]
            .activation_id,
        request.destination.activation_id
    );
}

#[test]
fn exact_retry_recovers_after_graph_advance_and_cancellation_without_mutation() {
    let f = fixture();
    let proof = f
        .store
        .adopt_owned_obligation(
            &f.request,
            &f.authority,
            &f.intent,
            &f.destination_receipt,
            4,
        )
        .unwrap();
    database::block_on(
        f.store
            .connection
            .update("workflow_run_graphs")
            .value("revision", 3_i64)
            .where_eq("run_id", "adoption-run")
            .execute(&f.store.connection),
    )
    .unwrap();
    database::block_on(
        f.store
            .connection
            .update("workflow_runs")
            .value("cancellation_requested_at_ms", 5_i64)
            .where_eq("run_id", "adoption-run")
            .execute(&f.store.connection),
    )
    .unwrap();
    assert_eq!(
        f.store
            .recover_owned_obligation_adoption(&f.request, &f.authority)
            .unwrap(),
        Some(proof.clone())
    );
    assert_eq!(
        f.store
            .adopt_owned_obligation(
                &f.request,
                &f.authority,
                &f.intent,
                &f.destination_receipt,
                6
            )
            .unwrap(),
        proof
    );
    assert_eq!(f.store.run_graph_revision("adoption-run").unwrap(), Some(3));
    assert_eq!(
        f.store
            .grant("destination-grant")
            .unwrap()
            .unwrap()
            .uses_consumed,
        1
    );
    assert_eq!(
        f.store
            .attempt_history("adoption-run", None, 10)
            .unwrap()
            .len(),
        2
    );
    let mut foreign = f.authority.clone();
    foreign.fencing_token = "foreign".into();
    assert!(
        f.store
            .recover_owned_obligation_adoption(&f.request, &foreign)
            .is_err()
    );
}

#[test]
fn retained_proof_cannot_hide_modified_source_or_destination_body() {
    for source in [true, false] {
        let f = fixture();
        f.store
            .adopt_owned_obligation(
                &f.request,
                &f.authority,
                &f.intent,
                &f.destination_receipt,
                4,
            )
            .unwrap();
        let identity = if source {
            &f.request.source.dispatch_identity
        } else {
            &f.request.destination.dispatch_identity
        };
        let mut receipt = f.store.dispatch_receipt(identity).unwrap().unwrap().receipt;
        receipt["call"]["harness"]["kind"]["request"]["submission"]["submission_id"] =
            serde_json::json!("tampered");
        database::block_on(
            f.store
                .connection
                .update("workflow_attempts")
                .value("receipt_json", serde_json::to_string(&receipt).unwrap())
                .where_eq("dispatch_identity", identity.as_str())
                .execute(&f.store.connection),
        )
        .unwrap();
        assert!(
            f.store
                .recover_owned_obligation_adoption(&f.request, &f.authority)
                .is_err()
        );
        assert!(
            f.store
                .adopt_owned_obligation(
                    &f.request,
                    &f.authority,
                    &f.intent,
                    &f.destination_receipt,
                    5
                )
                .is_err()
        );
        assert_eq!(
            f.store
                .resource_leases_for_run("adoption-run", 100)
                .unwrap()
                .len(),
            1
        );
    }
}

#[test]
fn missing_or_changed_destination_approval_cannot_transfer_responsibility() {
    for missing in [true, false] {
        let f = fixture();
        if missing {
            database::block_on(
                f.store
                    .connection
                    .delete("workflow_grants")
                    .where_eq("grant_id", "destination-grant")
                    .execute(&f.store.connection),
            )
            .unwrap();
        } else {
            database::block_on(
                f.store
                    .connection
                    .update("workflow_grants")
                    .value("scope_json", "{}")
                    .where_eq("grant_id", "destination-grant")
                    .execute(&f.store.connection),
            )
            .unwrap();
        }
        assert!(
            f.store
                .adopt_owned_obligation(
                    &f.request,
                    &f.authority,
                    &f.intent,
                    &f.destination_receipt,
                    4
                )
                .is_err()
        );
        assert!(
            f.store
                .dispatch_receipt(&f.request.destination.dispatch_identity)
                .unwrap()
                .is_none()
        );
        assert_eq!(f.store.run_graph_revision("adoption-run").unwrap(), Some(1));
        assert_eq!(
            f.store
                .resource_leases_for_run("adoption-run", 100)
                .unwrap()[0]
                .activation_id,
            f.request.source.activation_id
        );
    }
}

#[test]
fn stale_revision_foreign_owner_and_incomplete_claim_set_do_not_mutate() {
    for case in 0..3 {
        let f = fixture();
        let mut request = f.request.clone();
        let mut authority = f.authority.clone();
        match case {
            0 => request.expected_revision = 2,
            1 => authority.fencing_token = "foreign".into(),
            _ => request.resource_lease_ids.clear(),
        }
        assert!(
            f.store
                .adopt_owned_obligation(&request, &authority, &f.intent, &f.destination_receipt, 4)
                .is_err()
        );
        assert_eq!(f.store.run_graph_revision("adoption-run").unwrap(), Some(1));
        assert!(
            f.store
                .dispatch_receipt(&f.request.destination.dispatch_identity)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            f.store
                .resource_leases_for_run("adoption-run", 100)
                .unwrap()[0]
                .activation_id,
            f.request.source.activation_id
        );
        assert_eq!(
            f.store
                .grant("destination-grant")
                .unwrap()
                .unwrap()
                .uses_consumed,
            0
        );
    }
}
