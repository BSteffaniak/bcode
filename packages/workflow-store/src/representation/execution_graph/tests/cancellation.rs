use super::*;
use bcode_workflow::WorkflowRunGraphPublicationStatus as Status;

#[tokio::test]
async fn cancellation_acceptance_is_exact_and_waits_for_receipt_terminal() {
    cancellation_case(false).await;
}

#[tokio::test]
async fn cancellation_conflict_preserves_intents_and_requires_new_candidate() {
    cancellation_case(true).await;
}

async fn cancellation_case(conflict: bool) {
    let (db, run, owner, link, definition, _) = fixture_with_sibling(true).await;
    let activation = graph::activation_id(&*db, &run.run_id, "sibling")
        .await
        .unwrap()
        .unwrap();
    let prepared = crate::PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: "sibling".into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 3,
    };
    let dispatch = attempts::prepare(&*db, &owner, &prepared).await.unwrap();
    let batch = cancel_batch(&run.run_id, &link, &activation);
    stage_run_graph_edit_from_execution(&*db, &batch, &owner, &link, 4)
        .await
        .unwrap();
    assert!(
        accept_publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .is_err()
    );
    let receipt = crate::DispatchReceipt {
        run_id: run.run_id.clone(),
        node_id: "sibling".into(),
        activation_id: activation,
        attempt: 1,
        dispatch_identity: dispatch.clone(),
        receipt: serde_json::json!({}),
        admitted_at_ms: 4,
    };
    attempts::attach_receipt(&*db, &owner, &receipt)
        .await
        .unwrap();
    for _ in 0..2 {
        assert_eq!(
            accept_publication_from_execution(&*db, &batch, &owner, &link)
                .await
                .unwrap(),
            Status::Pending {
                expected_revision: 1
            }
        );
    }
    assert!(
        publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .is_err()
    );
    let mut changed = batch.clone();
    changed.edits.clear();
    assert!(
        accept_publication_from_execution(&*db, &changed, &owner, &link)
            .await
            .is_err()
    );
    if conflict {
        assert_conflict(
            &*db,
            &batch,
            &owner,
            &link,
            &definition,
            &receipt,
            &dispatch,
        )
        .await;
        return;
    }
    attempts::settle(
        &*db,
        &owner,
        &run.run_id,
        &dispatch,
        attempts::TerminalOutcome::Cancelled,
        5,
    )
    .await
    .unwrap();
    assert_eq!(
        accept_publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .unwrap(),
        Status::Committed { revision: 2 }
    );
    assert_eq!(
        accept_publication_from_execution(&*db, &batch, &owner, &link)
            .await
            .unwrap(),
        Status::Committed { revision: 2 }
    );
}

async fn assert_conflict(
    db: &dyn Database,
    batch: &WorkflowRunGraphEditBatch,
    owner: &WorkflowExecutionAuthority,
    link: &WorkflowExecutionSessionLink,
    definition: &bcode_workflow::WorkflowDefinition,
    receipt: &crate::DispatchReceipt,
    dispatch: &str,
) {
    let mut other = batch.clone();
    other.mutation_id = "other".into();
    other.reconciliation[1] = WorkflowRunGraphReconciliation::Retain {
        activation_id: receipt.activation_id.clone(),
    };
    let mut end = definition.nodes["end"].clone();
    end.name = "changed".into();
    other.edits = vec![bcode_workflow::WorkflowRunGraphEdit::ReplaceNode {
        node: end,
        entry: false,
        exit: true,
    }];
    stage_run_graph_edit_from_execution(db, &other, owner, link, 5)
        .await
        .unwrap();
    publication_from_execution(db, &other, owner, link)
        .await
        .unwrap();
    assert_eq!(
        accept_publication_from_execution(db, batch, owner, link)
            .await
            .unwrap(),
        Status::Conflicted {
            expected_revision: 1,
            current_revision: 2
        }
    );
    assert!(
        attempts::owner_boundary::cancellation_intended(db, &batch.run_id, dispatch, owner)
            .await
            .unwrap()
    );
}

fn cancel_batch(
    run_id: &str,
    link: &WorkflowExecutionSessionLink,
    activation: &str,
) -> WorkflowRunGraphEditBatch {
    WorkflowRunGraphEditBatch {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_VERSION,
        run_id: run_id.into(),
        expected_revision: 1,
        mutation_id: "cancel-sibling".into(),
        edits: vec![bcode_workflow::WorkflowRunGraphEdit::RemoveNode {
            node_id: "sibling".into(),
        }],
        reconciliation: vec![
            WorkflowRunGraphReconciliation::Retain {
                activation_id: link.activation_id.clone(),
            },
            WorkflowRunGraphReconciliation::Cancel {
                activation_id: activation.into(),
            },
        ],
    }
}
