//! Durable scheduler cursor regression using public store boundaries only.
use super::*;
use bcode_workflow_store::{
    AsyncAttemptStatusObserver, AttemptObservation, AttemptReconciliationRequest, DispatchReceipt,
    NewWorkflowRun, WorkflowStore,
};

struct ReceiptObserver {
    seen: std::sync::Mutex<BTreeSet<String>>,
    cancelled: bool,
}

impl AsyncAttemptStatusObserver for ReceiptObserver {
    fn observe_async<'a>(
        &'a self,
        request: &'a AttemptReconciliationRequest,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<AttemptObservation, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            assert!(
                self.seen
                    .lock()
                    .expect("observer")
                    .insert(request.dispatch_identity.clone()),
                "one sweep must not reobserve its unresolved prefix"
            );
            Ok(if self.cancelled {
                AttemptObservation::Cancelled
            } else {
                AttemptObservation::Running
            })
        })
    }
}

fn receipt_run(
    count: usize,
    prepare: bool,
) -> (
    tempfile::TempDir,
    PathBuf,
    bcode_workflow_store::WorkflowExecutionAuthority,
) {
    let root = tempfile::tempdir().expect("store root");
    let mut store = WorkflowStore::open_in_state_dir(root.path()).expect("store");
    let schema = bcode_workflow::ValueSchema::of::<u32>();
    let nodes: BTreeMap<_, _> = (0..count)
        .map(|index| {
            let id = format!("worker-{index:04}");
            (
                id.clone(),
                bcode_workflow::NodeDefinition {
                    id: id.clone(),
                    name: id,
                    kind: bcode_workflow::NodeKind::Task,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema.clone(),
                    resources: Vec::new(),
                    configuration: serde_json::Value::Null,
                },
            )
        })
        .collect();
    let definition = bcode_workflow::WorkflowDefinition {
        schema_version: bcode_workflow::WORKFLOW_DEFINITION_SCHEMA_VERSION,
        name: "receipt-scale".into(),
        input: schema.clone(),
        output: schema,
        entries: nodes.keys().cloned().collect(),
        exits: nodes.keys().cloned().collect(),
        nodes,
        edges: Vec::new(),
    };
    store
        .persist_definition("receipt-scale", 1, &definition)
        .expect("definition");
    let authority = test_workflow_execution_authority();
    store
        .create_run(&NewWorkflowRun {
            run_id: "receipt-scale".into(),
            definition_id: "receipt-scale".into(),
            definition_version: 1,
            workspace_snapshot: "snapshot".into(),
            parent_session_id: Some("scheduler-parent".into()),
            parent_session_generation: None,
            binding: None,
            authored_provenance: None,
            input: Some(serde_json::json!(1)),
            execution_authority: Some(authority.clone()),
            created_at_ms: 1,
            authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
                version: 1,
                provider_id: "test-policy".into(),
                profile_id: "build".into(),
                policy_digest_sha256: "a".repeat(64),
            },
            authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
            limits: bcode_workflow_store::WorkflowRunLimits::default(),
        })
        .expect("run");
    for pending in store
        .pending_activations_for_run("receipt-scale", if prepare { count } else { 1 })
        .expect("pending")
    {
        if !prepare {
            break;
        }
        let prepared = store
            .prepare_pending_activation(
                &pending.run_id,
                &pending.node_id,
                &pending.activation_id,
                bcode_workflow::DispatchSideEffect::ReadOnly,
                serde_json::json!({"owner":"synthetic"}),
                2,
            )
            .expect("prepare")
            .expect("admitted without implicit cap");
        store
            .persist_dispatch_receipt(&DispatchReceipt {
                run_id: prepared.activation.run_id,
                node_id: prepared.activation.node_id,
                activation_id: prepared.activation.activation_id,
                attempt: 1,
                dispatch_identity: prepared.dispatch_identity,
                receipt: serde_json::json!({"owner":"synthetic"}),
                admitted_at_ms: 3,
            })
            .expect("receipt");
    }
    let path = store.path().to_path_buf();
    (root, path, authority)
}

#[tokio::test]
async fn receipt_cursors_cover_running_prefix_after_reopen_and_cancel_at_scale() {
    for count in [32, 100, 257] {
        let (_root, path, authority) = receipt_run(count, true);
        for cancelled in [false, true] {
            if cancelled {
                WorkflowStore::open_at_path(&path)
                    .expect("reopen")
                    .request_cancellation("receipt-scale", 5)
                    .expect("cancel intent");
            }
            let observer = ReceiptObserver {
                seen: std::sync::Mutex::new(BTreeSet::new()),
                cancelled,
            };
            let mut cursor = None;
            let mut cancelled_count = 0;
            loop {
                // Reopen for every page: correctness must not depend on connection-local state.
                let mut store = WorkflowStore::open_at_path(&path).expect("reopen page");
                let (summary, next) = store
                    .reconcile_owned_receipts_page_async(
                        &observer,
                        "receipt-scale",
                        &authority,
                        (cursor.as_deref(), 17),
                        6,
                    )
                    .await
                    .expect("reconcile page");
                cancelled_count += summary.cancelled.len();
                cursor = next;
                if cursor.is_none() {
                    break;
                }
            }
            assert_eq!(observer.seen.lock().expect("observer").len(), count);
            assert_eq!(cancelled_count, if cancelled { count } else { 0 });
        }
    }
}

struct UnsupportedPrefix;
impl bcode_workflow_store::ActivationDispatchOwner for UnsupportedPrefix {
    fn plan<'a>(
        &'a self,
        activation: &'a bcode_workflow_store::PendingActivation,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = Result<
                        Option<bcode_workflow_store::ActivationDispatchPlan>,
                        WorkflowStoreError,
                    >,
                > + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            Ok((activation.node_id == "worker-0256").then(|| {
                bcode_workflow_store::ActivationDispatchPlan {
                    side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
                    intent: serde_json::json!({"owner":"synthetic"}),
                }
            }))
        })
    }
    fn dispatch<'a>(
        &'a self,
        _: &'a bcode_workflow_store::PreparedActivationDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async { Ok(serde_json::json!({"owner":"synthetic"})) })
    }
}

#[tokio::test]
async fn pending_cursor_reaches_supported_work_beyond_two_unsupported_pages() {
    let (_root, path, authority) = receipt_run(257, false);
    let mut cursor = None;
    let mut unsupported = BTreeSet::new();
    let mut admitted = Vec::new();
    let mut pages = 0;
    loop {
        let mut store = WorkflowStore::open_at_path(&path).expect("reopen page");
        let (summary, next) = store
            .dispatch_owned_pending_activations_page_for_run(
                &UnsupportedPrefix,
                "receipt-scale",
                &authority,
                (cursor.as_deref(), WORKFLOW_SCHEDULER_PAGE_SIZE),
                3,
            )
            .await
            .expect("dispatch page");
        for identity in summary.unsupported {
            assert!(
                unsupported.insert(identity),
                "blocked prefix must not repeat"
            );
        }
        admitted.extend(summary.admitted);
        pages += 1;
        cursor = next;
        if cursor.is_none() {
            break;
        }
        assert!(pages < 4, "bounded scan must advance");
    }
    assert_eq!(pages, 3);
    assert_eq!(unsupported.len(), 256);
    assert_eq!(admitted.len(), 1);
}
