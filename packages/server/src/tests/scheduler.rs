//! Durable scheduler cursor regression using public store boundaries only.
use super::*;
use bcode_workflow_store::{
    AsyncAttemptStatusObserver, AttemptObservation, AttemptReconciliationRequest, DispatchReceipt,
    NewWorkflowRun, WorkflowStore,
};

#[tokio::test]
async fn scheduling_application_pages_cover_all_activations_and_reject_wrong_run() {
    let (_root, path, _) = receipt_run(257, true);
    let mut state = test_server_state(SessionManager::default());
    state.workflow_store = StdMutex::new(WorkflowStore::open_at_path(&path).unwrap()).into();
    *state.workflow_store_unavailable.lock().unwrap() = None;
    let mut request = bcode_workflow::WorkflowSchedulingPageRequest {
        version: 1,
        run_id: "receipt-scale".into(),
        limit: 100,
        cursor: None,
    };
    let mut ids = BTreeSet::new();
    let mut pages = 0;
    loop {
        let page = workflow_operations::scheduling_page(&state, request.clone()).unwrap();
        page.validate("receipt-scale", Some(1)).unwrap();
        assert_eq!(page.observation.next_cursor, page.next_cursor);
        for activation in &page.observation.activations {
            assert!(ids.insert(activation.activation_id.clone()));
        }
        pages += 1;
        let Some(cursor) = page.next_cursor else {
            break;
        };
        let mut invalid = request.clone();
        invalid.run_id = "different-run".into();
        invalid.cursor = Some(cursor.clone());
        assert!(workflow_operations::scheduling_page(&state, invalid).is_err());
        let mut outdated = request.clone();
        outdated.cursor = Some(hex::encode(
            serde_json::to_vec(&serde_json::json!({
                "version":1,"run_id":"receipt-scale","revision":page.graph_revision + 1,
                "representation":1,"after":"untrusted-but-bounded-key"
            }))
            .unwrap(),
        ));
        assert!(workflow_operations::scheduling_page(&state, outdated).is_err());
        let mut malformed = request.clone();
        malformed.cursor = Some("not-hex".into());
        assert!(workflow_operations::scheduling_page(&state, malformed).is_err());
        request.cursor = Some(cursor);
    }
    drop(state);
    assert_eq!(pages, 3);
    assert_eq!(ids.len(), 257);
}

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
    configured_run(count, prepare, None, false)
}

fn configured_run(
    count: usize,
    prepare: bool,
    cap: Option<u32>,
    shared_prefix: bool,
) -> (
    tempfile::TempDir,
    PathBuf,
    bcode_workflow_store::WorkflowExecutionAuthority,
) {
    let root = tempfile::tempdir().expect("store root");
    let mut store = WorkflowStore::open_in_state_dir(root.path()).expect("store");
    let schema = bcode_workflow::ValueSchema::of::<u32>();
    let independent = (0..count)
        .map(|index| format!("worker-{index:04}"))
        .max_by_key(|id| bcode_workflow_store::activation_identity("receipt-scale", id, 0));
    let nodes: BTreeMap<_, _> = (0..count)
        .map(|index| {
            let id = format!("worker-{index:04}");
            (
                id.clone(),
                bcode_workflow::NodeDefinition {
                    id: id.clone(),
                    name: id.clone(),
                    kind: bcode_workflow::NodeKind::Task,
                    dataflow: bcode_workflow::WorkflowNodeDataflowPolicy::Direct,
                    input: schema.clone(),
                    output: schema.clone(),
                    resources: if shared_prefix && Some(&id) != independent.as_ref() {
                        vec![bcode_workflow::ResourceClaim::write("shared-prefix")]
                    } else {
                        Vec::new()
                    },
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
            limits: bcode_workflow_store::WorkflowRunLimits {
                concurrency_cap: cap,
                ..bcode_workflow_store::WorkflowRunLimits::default()
            },
        })
        .expect("run");
    if prepare {
        prepare_receipts(&mut store, count);
    }
    let path = store.path().to_path_buf();
    (root, path, authority)
}

fn prepare_receipts(store: &mut WorkflowStore, count: usize) {
    for pending in store
        .pending_activations_for_run("receipt-scale", count)
        .expect("pending")
    {
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

#[derive(Default)]
struct RecordingOwner {
    planned: std::sync::Mutex<BTreeSet<String>>,
    dispatched: std::sync::Mutex<BTreeSet<String>>,
}

impl bcode_workflow_store::ActivationDispatchOwner for RecordingOwner {
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
            assert!(
                self.planned
                    .lock()
                    .expect("planned")
                    .insert(activation.node_id.clone()),
                "a sweep must advance past blocked work"
            );
            Ok(Some(bcode_workflow_store::ActivationDispatchPlan {
                side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
                intent: serde_json::json!({"owner":"recording"}),
            }))
        })
    }

    fn dispatch<'a>(
        &'a self,
        request: &'a bcode_workflow_store::PreparedActivationDispatch,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<Output = Result<serde_json::Value, WorkflowStoreError>>
                + Send
                + 'a,
        >,
    > {
        Box::pin(async move {
            assert!(
                self.dispatched
                    .lock()
                    .expect("dispatched")
                    .insert(request.activation.node_id.clone())
            );
            Ok(serde_json::json!({"owner":"recording"}))
        })
    }
}

async fn dispatch_sweep(
    path: &Path,
    authority: &bcode_workflow_store::WorkflowExecutionAuthority,
    owner: &RecordingOwner,
) -> usize {
    let mut cursor = None;
    let mut admitted = 0;
    for _ in 0..32 {
        let (summary, next) = WorkflowStore::open_at_path(path)
            .expect("reopen")
            .dispatch_owned_pending_activations_page_for_run(
                owner,
                "receipt-scale",
                authority,
                (cursor.as_deref(), 17),
                4,
            )
            .await
            .expect("dispatch page");
        admitted += summary.admitted.len();
        if next.is_none() {
            return admitted;
        }
        assert_ne!(cursor, next, "cursor must advance even at capacity");
        cursor = next;
    }
    panic!("bounded fixture scan failed to exhaust");
}

#[tokio::test]
async fn resource_blocked_prefix_does_not_starve_independent_tail_at_scale() {
    for count in [32, 100, 257] {
        let (_root, path, authority) = configured_run(count, false, None, true);
        let independent = WorkflowStore::open_at_path(&path)
            .expect("store")
            .pending_activations_for_run("receipt-scale", count)
            .expect("pending")
            .into_iter()
            .find(|pending| pending.node.resources.is_empty())
            .expect("independent tail");
        let ordered = WorkflowStore::open_at_path(&path)
            .expect("store")
            .pending_activations_page_for_run("receipt-scale", (None, 1))
            .expect("first page")
            .0;
        assert_ne!(ordered[0].activation_id, independent.activation_id);
        let all_pending = WorkflowStore::open_at_path(&path)
            .expect("store")
            .pending_activations_for_run("receipt-scale", count)
            .expect("pending");
        assert!(
            all_pending
                .iter()
                .all(|pending| pending.activation_id <= independent.activation_id),
            "independent work must follow the entire resource-blocked prefix in cursor order"
        );
        let owner = RecordingOwner::default();
        assert_eq!(dispatch_sweep(&path, &authority, &owner).await, 2);
        assert_eq!(owner.planned.lock().expect("planned").len(), count);
        let dispatched = owner.dispatched.lock().expect("dispatched");
        assert_eq!(dispatched.len(), 2);
        assert!(dispatched.contains(&independent.node_id));
        drop(dispatched);
        let store = WorkflowStore::open_at_path(&path).expect("reopen");
        assert_eq!(
            store
                .pending_activations_for_run("receipt-scale", count)
                .expect("pending")
                .len(),
            count - 2
        );
        assert_eq!(
            store
                .attempt_history("receipt-scale", None, count)
                .expect("history")
                .len(),
            2
        );
    }
}

#[tokio::test]
async fn durable_dispatch_caps_and_uncapped_scan_entire_capacity_blocked_workload() {
    for count in [32, 100, 257] {
        for cap in [None, Some(7)] {
            let (_root, path, authority) = configured_run(count, false, cap, false);
            let owner = RecordingOwner::default();
            let expected = cap.map_or(count, |value| usize::try_from(value).expect("cap"));
            assert_eq!(dispatch_sweep(&path, &authority, &owner).await, expected);
            assert_eq!(owner.planned.lock().expect("planned").len(), count);
            assert_eq!(owner.dispatched.lock().expect("dispatched").len(), expected);
            let store = WorkflowStore::open_at_path(&path).expect("reopen");
            let attempts = store
                .attempt_history("receipt-scale", None, count)
                .expect("attempts");
            assert_eq!(
                attempts.len(),
                expected,
                "blocked candidates cannot create attempts"
            );
            assert!(
                attempts
                    .iter()
                    .all(|attempt| matches!(attempt.status.as_str(), "admitted" | "running"))
            );
            assert_eq!(
                store
                    .pending_activations_for_run("receipt-scale", count)
                    .expect("pending")
                    .len(),
                count - expected
            );
        }
    }
}

#[tokio::test]
async fn ownership_loss_between_pages_fences_stale_dispatch_before_owner_calls() {
    let (_root, path, authority) = configured_run(257, false, None, false);
    let owner = RecordingOwner::default();
    let (first, cursor) = WorkflowStore::open_at_path(&path)
        .expect("store")
        .dispatch_owned_pending_activations_page_for_run(
            &owner,
            "receipt-scale",
            &authority,
            (None, 17),
            3,
        )
        .await
        .expect("first page");
    assert_eq!(first.admitted.len(), 17);
    assert!(cursor.is_some());
    let mut successor = authority.clone();
    successor.generation += 1;
    successor.daemon_instance_id = "scheduler-successor".into();
    successor.fencing_token = "scheduler-successor-fence".into();
    WorkflowStore::open_at_path(&path)
        .expect("store")
        .transfer_execution_authority("receipt-scale", &authority, &successor, 4)
        .expect("verified transfer fixture");
    let result = WorkflowStore::open_at_path(&path)
        .expect("reopen")
        .dispatch_owned_pending_activations_page_for_run(
            &owner,
            "receipt-scale",
            &authority,
            (cursor.as_deref(), 17),
            5,
        )
        .await;
    assert!(result.is_err());
    assert_eq!(owner.planned.lock().expect("planned").len(), 17);
    assert_eq!(owner.dispatched.lock().expect("dispatched").len(), 17);
    let (next, _) = WorkflowStore::open_at_path(&path)
        .expect("reopen")
        .dispatch_owned_pending_activations_page_for_run(
            &owner,
            "receipt-scale",
            &successor,
            (cursor.as_deref(), 17),
            6,
        )
        .await
        .expect("new authority can continue exact cursor");
    assert_eq!(next.admitted.len(), 17);
    assert_eq!(owner.dispatched.lock().expect("dispatched").len(), 34);
}
