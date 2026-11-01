//! Independent recursive execution acceptance through authorized application adapters.
//! Immediate registered-definition descendants use observed start; self-recursive
//! packages retain an authored input barrier until observed package admission exists.
//! Public variants exercise authenticated `ClientConnection` observed admission;
//! these fake-provider fixtures do not certify live-provider quality.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::Duration;

use bcode_workflow as workflow;
use bcode_workflow::WorkflowRunApplication;
use bcode_workflow_store::representation;
use bcode_xray_models::execution_watch as dto;
use serde_json::json;
use sha2::{Digest as _, Sha256};
use switchy_database::Database as _;

const RUN: &str = "xray-real-recursive";

struct Fixture(Arc<crate::ServerState>);
impl Fixture {
    async fn finish(&self) {
        self.0
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.0.shutdown.send(());
        let task = self.0.workflow_driver_task.lock().await.take();
        if let Some(task) = task {
            task.await.expect("join owned workflow driver");
        }
        assert!(
            self.0.runtime_work.active_all().await.is_empty(),
            "no unjoined execution at acceptance completion"
        );
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        self.0
            .shutdown_requested
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let _ = self.0.shutdown.send(());
    }
}

struct IpcFixture {
    stop: Option<tokio::sync::oneshot::Sender<()>>,
    task: Option<tokio::task::JoinHandle<()>>,
}
impl IpcFixture {
    async fn finish(mut self) {
        let _ = self.stop.take().expect("owned IPC stop").send(());
        tokio::time::timeout(
            Duration::from_secs(10),
            self.task.take().expect("owned IPC task"),
        )
        .await
        .expect("bounded IPC join")
        .expect("IPC supervisor");
    }
}
impl Drop for IpcFixture {
    fn drop(&mut self) {
        if let Some(stop) = self.stop.take() {
            let _ = stop.send(());
        }
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

async fn serve_connections(
    listener: bcode_ipc::LocalIpcListener,
    state: Arc<crate::ServerState>,
    mut stop: tokio::sync::oneshot::Receiver<()>,
) {
    let mut clients = tokio::task::JoinSet::new();
    loop {
        tokio::select! {
            _ = &mut stop => break,
            result = clients.join_next(), if !clients.is_empty() => { result.expect("client completed").expect("client handler"); }
            stream = listener.accept() => {
                let stream = stream.expect("real IPC accept");
                let state = Arc::clone(&state);
                clients.spawn(async move { crate::handle_client(stream, state).await.expect("normal authenticated request routing"); });
            }
        }
    }
    clients.abort_all();
    while let Some(result) = clients.join_next().await {
        if let Err(error) = result {
            assert!(error.is_cancelled(), "client handler panic: {error}");
        }
    }
}

async fn isolated_acceptance(test_name: &str) -> bool {
    const CHILD: &str = "BCODE_SERVER_ISOLATED_TEST";
    if std::env::var(CHILD).as_deref() == Ok(test_name) {
        return false;
    }
    let directory = tempfile::Builder::new()
        .prefix("xray-execution-child-")
        .tempdir()
        .expect("private child log root")
        .keep();
    let stdout = std::fs::File::create(directory.join("stdout.log")).expect("private stdout");
    let stderr = std::fs::File::create(directory.join("stderr.log")).expect("private stderr");
    let mut child = tokio::process::Command::new(std::env::current_exe().expect("test artifact"))
        .args(["--exact", test_name, "--test-threads=1", "--nocapture"])
        .env(CHILD, test_name)
        .stdout(stdout)
        .stderr(stderr)
        .kill_on_drop(true)
        .spawn()
        .expect("owned isolated acceptance child");
    eprintln!(
        "private recursive child logs retained: {}",
        directory.display()
    );
    let status = tokio::time::timeout(Duration::from_secs(180), child.wait())
        .await
        .expect("isolated acceptance deadline")
        .expect("child exit");
    assert!(
        status.success(),
        "isolated recursive acceptance failed; full private stdout/stderr retained at {}",
        directory.display()
    );
    true
}

async fn retain_diagnostics(
    state: &crate::ServerState,
    root: &std::path::Path,
    runs: &BTreeSet<String>,
    events: &[dto::ExecutionWatchEvent],
) {
    let mut summaries = Vec::new();
    for run in runs.iter().take(8) {
        let status = tokio::time::timeout(
            Duration::from_secs(3),
            crate::workflow_representation::status(state, run),
        )
        .await;
        let inspection = tokio::time::timeout(
            Duration::from_secs(3),
            crate::workflow_representation::inspection::inspect(state, run, 16),
        )
        .await;
        summaries.push(json!({"run_id": run,
            "status": status.ok().and_then(Result::ok).flatten(), "inspection": inspection.ok().and_then(Result::ok).flatten()}));
    }
    let mut names = BTreeMap::<String, usize>::new();
    for event in events {
        *names
            .entry(format!(
                "{}.{}",
                event.envelope.event.domain, event.envelope.event.name
            ))
            .or_default() += 1;
    }
    let retained: Vec<_> = events.iter().take(512).map(|event| json!({
        "run_id": event.run_id, "session_id": event.session_id,
        "observation_id": event.envelope.observation_id, "sequence": event.envelope.sequence,
        "domain": event.envelope.event.domain, "name": event.envelope.event.name,
        "correlation": event.envelope.event.correlation,
    })).collect();
    // Only bounded fixture evidence; no provider payload, credentials or config dump.
    let value = json!({"runs": summaries, "event_counts": names,
        "total_events": events.len(), "retained_event_headers": retained});
    let path = root.join("acceptance-diagnostics.json");
    let bytes = serde_json::to_vec_pretty(&value).expect("bounded fixture diagnostics");
    assert!(
        bytes.len() <= 2 * 1024 * 1024,
        "bounded acceptance diagnostics"
    );
    std::fs::write(&path, bytes).expect("retain fixture diagnostics");
    eprintln!(
        "private recursive diagnostic artifact retained: {}",
        path.display()
    );
}

fn recursive_definition() -> workflow::WorkflowDefinition {
    let config: workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            workflow::ValueSchema::of::<u32>(),
            workflow::PromptContextTarget::FreshIsolated,
        ))
        .expect("real structured fake-provider agent contract");
    let mut definition = workflow::WorkflowBuilder::new(
        "xray-recursive",
        workflow::Step::<u32, u32>::input("activate")
            .then(workflow::Step::<u32, u32>::agent("agent", &config).expect("agent")),
    )
    .build()
    .expect("gated definition")
    .definition()
    .clone();
    let mut branch = definition.nodes["agent"].clone();
    branch.id = "branch".into();
    branch.name = "branch".into();
    branch.kind = workflow::NodeKind::Branch;
    let predicate = workflow::field::<u32>("").eq(0_u32).expression().clone();
    branch.configuration = json!({"predicate_version": workflow::WORKFLOW_PREDICATE_VERSION,
        "predicate": predicate, "true_entries": ["agent"], "false_entries": ["recurse"],
        "true_nodes": ["agent"], "false_nodes": ["recurse"]});
    let mut recurse = definition.nodes["agent"].clone();
    recurse.id = "recurse".into();
    recurse.name = "recurse".into();
    recurse.kind = workflow::NodeKind::WorkflowCall;
    recurse.configuration = serde_json::to_value(workflow::WorkflowCallConfiguration {
        version: workflow::WORKFLOW_CALL_VERSION,
        target: workflow::WorkflowCallTarget::PackageLocal {
            reference: workflow::WorkflowPackageLocalCall {
                version: 1,
                member_id: "recursive".into(),
                workflow_id: "recursive".into(),
                input: definition.input.clone(),
                output: definition.output.clone(),
            },
        },
        input: None,
        output: None,
    })
    .expect("canonical recursive call");
    let mut result = definition.nodes["agent"].clone();
    result.id = "result".into();
    result.name = "result".into();
    definition.nodes.insert("result".into(), result);
    definition.nodes.insert("branch".into(), branch);
    definition.nodes.insert("recurse".into(), recurse);
    definition.exits = vec!["result".into()];
    definition.edges = vec![
        workflow::EdgeDefinition {
            from: "activate".into(),
            to: "branch".into(),
            kind: workflow::EdgeKind::Direct,
            transform: None,
        },
        workflow::EdgeDefinition {
            from: "branch".into(),
            to: "agent".into(),
            kind: workflow::EdgeKind::Conditional {
                predicate: predicate.clone(),
                expected: true,
            },
            transform: None,
        },
        workflow::EdgeDefinition {
            from: "branch".into(),
            to: "recurse".into(),
            kind: workflow::EdgeKind::Conditional {
                predicate,
                expected: false,
            },
            transform: Some(workflow::WorkflowTransform {
                version: workflow::WORKFLOW_TRANSFORM_VERSION,
                expression: workflow::WorkflowTransformExpression::Increment {
                    value: Box::new(workflow::WorkflowTransformExpression::Input {
                        source: workflow::WORKFLOW_TRANSFORM_SOURCE_CURRENT.into(),
                        path: String::new(),
                    }),
                    by: -1,
                },
                output: definition.input.clone(),
            }),
        },
    ];
    definition
        .edges
        .extend(["agent", "recurse"].map(|from| workflow::EdgeDefinition {
            from: from.into(),
            to: "result".into(),
            kind: workflow::EdgeKind::Direct,
            transform: None,
        }));
    definition
}

fn package_lock(identity: &workflow::WorkflowDefinitionIdentity) -> workflow::WorkflowPackageLock {
    workflow::WorkflowPackageLock {
        version: workflow::WORKFLOW_PACKAGE_LOCK_VERSION,
        package_id: "acceptance/xray-recursive".into(),
        package_source_digest_sha256: "a".repeat(64),
        imports: vec![],
        exports: vec![workflow::WorkflowPackageLockedExport {
            export: "run".into(),
            member_id: "recursive".into(),
            definition_identity: identity.clone(),
            published_revision: None,
        }],
        members: vec![workflow::WorkflowPackageLockedMember {
            member_id: "recursive".into(),
            source_digest_sha256: "b".repeat(64),
            executable_digest_sha256: identity
                .definition_id
                .rsplit_once('@')
                .expect("content-derived identity")
                .1
                .into(),
            definition_identity: identity.clone(),
            published_revision: None,
            dependency_closure: vec![],
        }],
    }
}

fn first_request_fixture() -> String {
    "independent recursive IPC payload 中文 👩‍💻\n".repeat(1000)
}

fn immediate_definitions(
    shared_parent: bool,
) -> Vec<(
    workflow::WorkflowDefinitionIdentity,
    workflow::WorkflowDefinition,
)> {
    let mut config: workflow::WorkflowPromptConfiguration =
        serde_json::from_value(crate::tests::test_workflow_prompt_configuration(
            workflow::ValueSchema::of::<u32>(),
            if shared_parent {
                workflow::PromptContextTarget::SharedParentSequential
            } else {
                workflow::PromptContextTarget::FreshIsolated
            },
        ))
        .expect("real structured agent contract");
    config.system_prompt = first_request_fixture();
    let leaf = workflow::WorkflowBuilder::new(
        "xray-immediate-leaf",
        workflow::Step::<u32, u32>::agent("agent", &config)
            .expect("first leaf call")
            .then(workflow::Step::<u32, u32>::agent("result", &config).expect("leaf result")),
    )
    .build()
    .expect("leaf")
    .definition()
    .clone();
    let leaf_identity =
        workflow::WorkflowDefinitionIdentity::for_definition("xray-immediate-leaf", &leaf)
            .expect("leaf identity");
    let mut definitions = vec![(leaf_identity, leaf)];
    for name in ["xray-immediate-child", "xray-immediate-root"] {
        let (child_identity, child) = definitions.last().expect("exact registered child");
        let mut parent = workflow::WorkflowBuilder::new(
            name,
            workflow::Step::<u32, u32>::agent("recurse", &config)
                .expect("call template")
                .then(workflow::Step::<u32, u32>::agent("result", &config).expect("parent result")),
        )
        .build()
        .expect("parent")
        .definition()
        .clone();
        let call = parent.nodes.get_mut("recurse").expect("call node");
        call.kind = workflow::NodeKind::WorkflowCall;
        call.configuration = serde_json::to_value(workflow::WorkflowCallConfiguration {
            version: workflow::WORKFLOW_CALL_VERSION,
            target: workflow::WorkflowCallTarget::Definition {
                identity: child_identity.clone(),
            },
            input: None,
            output: None,
        })
        .expect("exact child call");
        assert_eq!(parent.input, child.input);
        assert_eq!(parent.output, child.output);
        let identity = workflow::WorkflowDefinitionIdentity::for_definition(name, &parent)
            .expect("parent identity");
        definitions.push((identity, parent));
    }
    definitions
}

#[test]
fn real_recursive_package_watch_precedes_first_provider_request_for_each_canonical_run() {
    // Direct gated package admission exercises the debug scheduler on this thread,
    // unlike the public variants' separately spawned IPC handlers. Give this fixture
    // an explicit stack budget, as other server debug integration fixtures do;
    // do not change process-global RUST_MIN_STACK or execution/recursion policy.
    std::thread::Builder::new()
        .name("xray-gated-package-acceptance".into())
        .stack_size(16 * 1024 * 1024)
        .spawn(|| {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("owned gated acceptance runtime")
                .block_on(acceptance(false, false, false, "xray_execution_acceptance_tests::real_recursive_package_watch_precedes_first_provider_request_for_each_canonical_run"));
        })
        .expect("owned gated acceptance thread")
        .join()
        .expect("gated acceptance must complete without panic");
}

#[tokio::test]
async fn immediate_observed_start_captures_first_provider_calls_in_child_and_grandchild() {
    Box::pin(acceptance(true, false, false, "xray_execution_acceptance_tests::immediate_observed_start_captures_first_provider_calls_in_child_and_grandchild")).await;
}

#[tokio::test]
async fn public_connection_observed_start_preserves_recursive_first_call_evidence() {
    Box::pin(acceptance(true, true, false, "xray_execution_acceptance_tests::public_connection_observed_start_preserves_recursive_first_call_evidence")).await;
}

#[tokio::test]
async fn public_shared_parent_observed_start_preserves_activation_identity_and_foreign_isolation() {
    Box::pin(acceptance(true, true, true, "xray_execution_acceptance_tests::public_shared_parent_observed_start_preserves_activation_identity_and_foreign_isolation")).await;
}

type Approval = Arc<std::sync::Mutex<Option<(String, String, String)>>>;

struct Transport {
    caller: crate::ClientId,
    foreign: crate::ClientId,
    ipc_fixture: Option<IpcFixture>,
    ordinary_client: Option<bcode_client::BcodeClient>,
    connections: Option<(
        bcode_client::ClientConnection,
        bcode_client::ClientConnection,
    )>,
    socket_directory: tempfile::TempDir,
}
struct AcceptanceFixture {
    fixture: Fixture,
    root: std::path::PathBuf,
    parent: bcode_session_models::SessionId,
    db: switchy_database::rusqlite::RusqliteDatabase,
    scope: dto::ExecutionWatchScope,
    immediate: bool,
    shared_parent: bool,
    transport: Transport,
}
struct Driven {
    cursor: u64,
    events: Vec<dto::ExecutionWatchEvent>,
    runs: BTreeSet<String>,
}
macro_rules! diagnostic {
    ($fixture:ident, $method:ident, $principal:expr, $request:expr) => {{
        let principal = $principal;
        let request = $request;
        match $fixture.transport.connections.as_mut() {
            Some((owner, other)) => {
                let connection = if principal == $fixture.transport.caller {
                    owner
                } else {
                    other
                };
                connection
                    .$method(request)
                    .await
                    .map_err(|_| "public diagnostic operation rejected")
            }
            None => $fixture
                .fixture
                .0
                .$method(principal, request)
                .await
                .map_err(|_| "application diagnostic operation rejected"),
        }
    }};
}
macro_rules! observed_start {
    ($fixture:ident, $principal:expr, $start:expr, $options:expr) => {{
        let principal = $principal;
        let start = $start;
        let options = $options;
        match $fixture.transport.connections.as_mut() {
            Some((owner, other)) => {
                let connection = if principal == $fixture.transport.caller {
                    owner
                } else {
                    other
                };
                connection
                    .start_workflow_run_observed(start, options)
                    .await
                    .map_err(|_| "public observed admission rejected")
            }
            None => $fixture
                .fixture
                .0
                .start_workflow_run_observed(principal, start, options)
                .await
                .map_err(|_| "application observed admission rejected"),
        }
    }};
}
fn configure_state(
    sessions: crate::SessionManager,
    store: bcode_workflow_store::WorkflowStore,
    public: bool,
    scope: &dto::ExecutionWatchScope,
) -> (crate::ServerState, Approval) {
    let mut state = crate::tests::test_server_state_with_workflow_authorization(sessions, store);
    state
        .selected_provider_context
        .settings
        .insert("fake_structured_output_json".into(), "9".into());
    let approved = Arc::new(std::sync::Mutex::new(None::<(String, String, String)>));
    let policy_approval = Arc::clone(&approved);
    let approved_scope = (*scope).clone();
    if !public {
        state.daemon_status.state_location_id = Some("xray-acceptance-location".into());
    }
    state.workflow_application_authorization =
        crate::workflow_operations::WorkflowApplicationAuthorizationPolicy {
            evaluator: Arc::new(move |facts| {
                use crate::workflow_operations::WorkflowApplicationAuthorizationDecision as Decision;
                let Some(observation) = &facts.execution_observation else {
                    return crate::workflow_operations::authorize_local_workflow_application_operation(facts);
                };
                let approved = policy_approval.lock().expect("host diagnostic policy");
                let authorized = facts.validate().is_ok()
                    && approved
                        .as_ref()
                        .is_some_and(|(principal, namespace, location)| {
                            facts.actor.actor_id == *principal
                                && observation.namespace == *namespace
                                && observation.state_location_id == *location
                        })
                    && observation.root_run_id == approved_scope.root_run_id
                    && observation.recursive == approved_scope.recursive
                    && observation.deep_capture == approved_scope.deep_capture;
                drop(approved);
                if authorized {
                    Decision::Allow
                } else {
                    Decision::Deny {
                        reason: "no matching explicit fixture diagnostic grant".into(),
                    }
                }
            }),
        };
    (state, approved)
}
async fn connect_fixture(
    state: &Arc<crate::ServerState>,
    public: bool,
    approved: &Approval,
) -> Transport {
    // Match normal server startup: initial synchronous root drive is not the
    // continuation scheduler. Child dispatch publishes to this owned driver.
    state.start_workflow_driver().await;
    let socket_directory = tempfile::Builder::new()
        .prefix("xr-ipc-")
        .tempdir_in("/tmp")
        .expect("short private IPC root");
    let mut caller = crate::ClientId::new();
    let mut foreign = crate::ClientId::new();
    let mut ipc_fixture = None;
    let mut ordinary_client = None;
    let connections = if public {
        use std::os::unix::fs::PermissionsExt as _;

        // The listener validates existing parents; only this fixture owns and
        // prepares this directory, without weakening the production policy.
        std::fs::set_permissions(
            socket_directory.path(),
            std::fs::Permissions::from_mode(0o700),
        )
        .expect("private IPC root mode");
        let socket_root = socket_directory
            .path()
            .canonicalize()
            .expect("physical private IPC root");
        let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_root.join("s"));
        let listener =
            bcode_ipc::LocalIpcListener::bind(&endpoint).expect("isolated real IPC listener");
        let (stop, stopped) = tokio::sync::oneshot::channel();
        ipc_fixture = Some(IpcFixture {
            stop: Some(stop),
            task: Some(tokio::spawn(serve_connections(
                listener,
                Arc::clone(state),
                stopped,
            ))),
        });
        let client = bcode_client::BcodeClient::new(endpoint);
        let owner = client
            .connect("xray-public-acceptance-owner")
            .await
            .expect("ordinary verified Hello");
        caller = owner
            .client_id()
            .expect("server-authenticated owner identity");
        let other = client
            .connect("xray-public-acceptance-foreign")
            .await
            .expect("independent verified Hello");
        foreign = other
            .client_id()
            .expect("server-authenticated foreign identity");
        ordinary_client = Some(client);
        Some((owner, other))
    } else {
        for client in [caller, foreign] {
            state.clients.lock().await.insert(client);
            state
                .client_session_namespaces
                .lock()
                .await
                .insert(client, "xray-acceptance".into());
        }
        None
    };
    let namespace = state
        .client_session_namespaces
        .lock()
        .await
        .get(&caller)
        .expect("registered namespace")
        .clone();
    *approved.lock().expect("explicit host grant") = Some((
        caller.to_string(),
        namespace,
        state
            .daemon_status
            .state_location_id
            .clone()
            .expect("verified state location"),
    ));
    Transport {
        caller,
        foreign,
        ipc_fixture,
        ordinary_client,
        connections,
        socket_directory,
    }
}
async fn acceptance(immediate: bool, public: bool, shared_parent: bool, test_name: &str) {
    let guard = crate::tests::WORKFLOW_RUNTIME_TEST_LOCK.lock().await;
    if isolated_acceptance(test_name).await {
        return;
    }
    let mut fixture = Box::pin(AcceptanceFixture::setup(immediate, public, shared_parent)).await;
    let (start, lock, digest) = fixture.register().await;
    let ready = Box::pin(fixture.admit(start, &lock, &digest)).await;
    fixture.verify_admission(&ready).await;
    let driven = Box::pin(fixture.drive(&ready)).await;
    fixture.drive_foreign().await;
    let runs = driven.runs.clone();
    let (events, cutoff, cursor) = fixture.finalize(&ready, driven).await;
    fixture
        .verify_evidence(&ready, &events, &cutoff, &runs)
        .await;
    fixture.verify_late_and_revoke(ready, cursor).await;
    fixture.finish().await;
    drop(fixture);
    drop(guard);
}

impl AcceptanceFixture {
    async fn setup(immediate: bool, public: bool, shared_parent: bool) -> Self {
        let root = tempfile::Builder::new()
            .prefix("xray-execution-state-")
            .tempdir()
            .expect("isolated workflow state")
            .keep();
        let session_root = tempfile::Builder::new()
            .prefix("xray-execution-sessions-")
            .tempdir()
            .expect("isolated canonical sessions")
            .keep();
        eprintln!(
            "private recursive acceptance state retained: {}; canonical sessions: {}",
            root.display(),
            session_root.display()
        );
        let sessions = crate::SessionManager::persistent_lazy(&session_root);
        let parent = sessions
            .create_session(Some("xray-recursive-owner".into()), root.clone())
            .await
            .expect("parent session");
        let store = bcode_workflow_store::WorkflowStore::create_coexistence_baseline(&root)
            .expect("typed workflow store");
        let scope = dto::ExecutionWatchScope {
            root_run_id: RUN.into(),
            recursive: true,
            deep_capture: true,
        };
        let (state, approved) = configure_state(sessions, store, public, &scope);
        let fixture = Fixture(Arc::new(state));
        let transport = connect_fixture(&fixture.0, public, &approved).await;
        drop(approved);
        // Typed structured schema/query APIs only; no SQL text or driver escape hatches.
        let db = switchy_database_connection::open_sqlite_rusqlite(
            &crate::workflow_store_path(&fixture.0),
            &switchy_database_connection::SqliteOpenOptions {
                access: switchy_database_connection::SqliteAccess::ReadWrite,
                allow_uri: false,
                busy_timeout: Duration::ZERO,
                foreign_keys: true,
                connections: std::num::NonZeroUsize::MIN,
            },
        )
        .expect("typed database connection");
        Self {
            fixture,
            root,
            parent: parent.id,
            db,
            scope,
            immediate,
            shared_parent,
            transport,
        }
    }
    async fn register(
        &self,
    ) -> (
        workflow::WorkflowRunStartRequest,
        workflow::WorkflowPackageLock,
        String,
    ) {
        let definition = recursive_definition();
        let identity =
            workflow::WorkflowDefinitionIdentity::for_definition("recursive", &definition)
                .expect("recursive identity");
        let lock = package_lock(&identity);
        lock.validate().expect("recursive package lock");
        let digest = lock.digest_sha256().expect("lock digest");
        self.fixture
            .0
            .workflow_store
            .lock()
            .expect("store")
            .persist_definition(
                &identity.definition_id,
                identity.definition_version,
                &definition,
            )
            .expect("registered executable definition");
        if !self.immediate {
            self.db
                .insert("workflow_package_publications")
                .value("package_id", lock.package_id.as_str())
                .value("package_lock_digest_sha256", digest.as_str())
                .value(
                    "lock_json",
                    serde_json::to_string(&lock).expect("lock JSON"),
                )
                .value("receipt_json", "{}")
                .value("published_at_ms", 1_i64)
                .execute(&self.db)
                .await
                .expect("publish immutable recursive fixture");
        }
        let limits = workflow::WorkflowRunLimits {
            recursion_depth_cap: 8,
            descendant_cap: 8,
            node_execution_cap: 48,
            ..workflow::WorkflowRunLimits::default()
        };
        let start = workflow::WorkflowRunStartRequest {
            definition_id: identity.definition_id,
            definition_version: identity.definition_version,
            run_id: Some(RUN.into()),
            workspace_snapshot: self.root.display().to_string(),
            parent_session_id: self.parent,
            parent_session_generation: None,
            binding: None,
            input: Some(json!(2)),
            limits,
        };
        (start, lock, digest)
    }
    async fn admit(
        &mut self,
        start: workflow::WorkflowRunStartRequest,
        lock: &workflow::WorkflowPackageLock,
        digest: &str,
    ) -> dto::ExecutionWatchReady {
        if self.immediate {
            self.admit_immediate(start).await
        } else {
            Box::pin(self.admit_gated(start, lock, digest)).await
        }
    }
    async fn admit_immediate(
        &mut self,
        start: workflow::WorkflowRunStartRequest,
    ) -> dto::ExecutionWatchReady {
        let definitions = immediate_definitions(self.shared_parent);
        for (identity, definition) in &definitions {
            self.fixture
                .0
                .workflow_store
                .lock()
                .expect("store")
                .persist_definition(
                    &identity.definition_id,
                    identity.definition_version,
                    definition,
                )
                .expect("register exact immediate child definition");
        }
        let (identity, _) = definitions.last().expect("root definition");
        let mut start = start;
        start.definition_id = identity.definition_id.clone();
        start.definition_version = identity.definition_version;
        let options = dto::ExecutionWatchStartOptions {
            schema_version: 1,
            recursive: true,
            deep_capture: true,
        };
        assert!(
            observed_start!(self, self.transport.foreign, start.clone(), options.clone()).is_err(),
            "diagnostic authorization must precede observed admission and dispatch"
        );
        assert!(
            crate::workflow_representation::status(&self.fixture.0, RUN)
                .await
                .expect("denied admission inspection")
                .is_none(),
            "denied observed start must not create or execute a run"
        );
        for (recursive, deep_capture) in [(false, true), (true, false), (false, false)] {
            assert!(
                observed_start!(
                    self,
                    self.transport.caller,
                    start.clone(),
                    dto::ExecutionWatchStartOptions {
                        schema_version: 1,
                        recursive,
                        deep_capture,
                    }
                )
                .is_err(),
                "an exact recursive/deep grant cannot be reinterpreted as another scope"
            );
            assert!(
                crate::workflow_representation::status(&self.fixture.0, RUN)
                    .await
                    .expect("scope-denied admission inspection")
                    .is_none(),
                "mismatched scope cannot admit execution"
            );
        }
        let duplicate_start = start.clone();
        let (started, ready) = observed_start!(self, self.transport.caller, start, options.clone())
            .expect("actual observed admission before immediate scheduler dispatch");
        assert!(
            observed_start!(self, self.transport.caller, duplicate_start, options).is_err(),
            "duplicate observed admission cannot re-drive work or fabricate another gap-free segment"
        );
        assert_eq!(started.run.run_id, RUN);
        assert!(
            !ready.prior_execution_gap,
            "fresh observed admission must not be mistaken for a late watch"
        );
        ready
    }
    async fn admit_gated(
        &mut self,
        start: workflow::WorkflowRunStartRequest,
        lock: &workflow::WorkflowPackageLock,
        digest: &str,
    ) -> dto::ExecutionWatchReady {
        crate::workflow_operations::start_run_with_package(
            &self.fixture.0,
            start,
            None,
            Some((&lock.package_id, digest)),
        )
        .await
        .expect("real gated recursive run admission");
        let waits = representation::graph::gate_waits::page(&self.db, RUN, 8)
            .await
            .expect("canonical initial gate");
        assert_eq!(
            waits.gates.len(),
            1,
            "root must be durably waiting before watch admission"
        );
        assert_eq!(waits.gates[0].node_id, "activate");
        let ready = diagnostic!(
            self,
            open_execution_watch,
            self.transport.caller,
            dto::OpenExecutionWatchRequest {
                schema_version: 1,
                scope: self.scope.clone()
            }
        )
        .expect("explicit authorized watch before any provider dispatch");
        assert!(
            ready.prior_execution_gap,
            "normal-start attachment cannot certify earlier execution even behind an authored gate"
        );
        ready
    }
    async fn verify_admission(&mut self, ready: &dto::ExecutionWatchReady) {
        assert_eq!(ready.scope, self.scope);
        assert_eq!(ready.cursor, 0);
        assert!(
            diagnostic!(
                self,
                open_execution_watch,
                self.transport.foreign,
                dto::OpenExecutionWatchRequest {
                    schema_version: 1,
                    scope: self.scope.clone()
                }
            )
            .is_err(),
            "ordinary same-namespace access is not diagnostic authorization"
        );
        for limit in [
            0,
            ready
                .max_page_items
                .checked_add(1)
                .expect("bounded page maximum"),
        ] {
            assert!(
                diagnostic!(
                    self,
                    read_execution_watch,
                    self.transport.caller,
                    dto::ReadExecutionWatchRequest {
                        schema_version: 1,
                        watch_id: ready.watch_id.clone(),
                        cursor: 0,
                        limit,
                    }
                )
                .is_err(),
                "public watch page bounds must fail closed"
            );
        }
        assert!(
            diagnostic!(
                self,
                read_execution_watch,
                self.transport.foreign,
                dto::ReadExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    cursor: 0,
                    limit: 1,
                }
            )
            .is_err(),
            "foreign principal cannot read selected watch"
        );
        assert!(
            diagnostic!(
                self,
                finalize_execution_watch,
                self.transport.foreign,
                dto::FinalizeExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    offset: 0,
                    limit: 1,
                }
            )
            .is_err(),
            "foreign principal cannot cut off selected capture"
        );
    }
    async fn drive(&mut self, ready: &dto::ExecutionWatchReady) -> Driven {
        let mut cursor = 0;
        let mut events = Vec::new();
        let mut runs = BTreeSet::from([RUN.to_owned()]);
        let mut resolved = BTreeSet::new();
        let completion = tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            for run in runs.clone() {
                let children = representation::hierarchy::children(&self.db, &run, None, 8).await.expect("canonical child membership");
                for child in children.children { runs.insert(child.child_run_id); }
                let gates = representation::graph::gate_waits::page(&self.db, &run, 8).await.expect("authored execution gates").gates;
                if self.immediate { assert!(gates.is_empty(), "observed start must capture immediate provider calls without an authored barrier"); }
                for gate in gates {
                    if resolved.insert((run.clone(), gate.activation_id.clone())) {
                        let result = crate::workflow_operations::provide_input(&self.fixture.0, &run, &gate.node_id, &gate.activation_id, gate.input).await.expect("ordinary execution activation after watch ready");
                        assert_eq!(result.outcome, "accepted");
                    }
                }
            }
            let page = diagnostic!(self,read_execution_watch, self.transport.caller, dto::ReadExecutionWatchRequest { schema_version: 1, watch_id: ready.watch_id.clone(), cursor, limit: ready.max_page_items }).expect("authorized live watch page");
            assert_eq!(page.omitted_links, 0);
            assert_eq!(page.omitted_events, 0);
            assert_eq!(page.contention_gaps, 0);
            cursor = page.next_cursor;
            events.extend(page.events);
            if runs.len() == 3 && crate::workflow_representation::status(&self.fixture.0, RUN).await.expect("root status").expect("root exists").status == workflow::RunStatus::Completed { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await;
        retain_diagnostics(&self.fixture.0, &self.root, &runs, &events).await;
        completion.expect("finite real recursive execution must complete; retained acceptance-diagnostics.json records canonical progress");
        assert_eq!(
            runs.len(),
            3,
            "real root, child and grandchild, not synthesized run labels"
        );
        assert_eq!(
            resolved.len(),
            if self.immediate { 0 } else { 3 },
            "immediate observed admission cannot be implemented by silently adding execution gates"
        );
        Driven {
            cursor,
            events,
            runs,
        }
    }
    async fn drive_foreign(&self) {
        if self.shared_parent {
            let config = crate::tests::test_workflow_prompt_configuration(
                workflow::ValueSchema::of::<u32>(),
                workflow::PromptContextTarget::SharedParentSequential,
            );
            let config: workflow::WorkflowPromptConfiguration =
                serde_json::from_value(config).expect("shared foreign contract");
            let definition = workflow::WorkflowBuilder::new(
                "xray-foreign-shared",
                workflow::Step::<u32, u32>::agent("foreign", &config).expect("foreign agent"),
            )
            .build()
            .expect("foreign definition")
            .definition()
            .clone();
            let identity = workflow::WorkflowDefinitionIdentity::for_definition(
                "xray-foreign-shared",
                &definition,
            )
            .expect("foreign identity");
            self.fixture
                .0
                .workflow_store
                .lock()
                .expect("store")
                .persist_definition(
                    &identity.definition_id,
                    identity.definition_version,
                    &definition,
                )
                .expect("register foreign shared execution");
            let foreign_start = workflow::WorkflowRunStartRequest {
                definition_id: identity.definition_id,
                definition_version: identity.definition_version,
                run_id: Some("xray-foreign-shared".into()),
                workspace_snapshot: self.root.display().to_string(),
                parent_session_id: self.parent,
                parent_session_generation: None,
                binding: None,
                input: Some(json!(7)),
                limits: workflow::WorkflowRunLimits::default(),
            };
            WorkflowRunApplication::start_workflow_run(
                self.transport
                    .ordinary_client
                    .as_ref()
                    .expect("existing public fixture client"),
                foreign_start,
            )
            .await
            .expect("ordinary unrelated execution on same parent");
            self.verify_foreign_completion().await;
        }
    }
    async fn verify_foreign_completion(&self) {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if crate::workflow_representation::status(&self.fixture.0, "xray-foreign-shared")
                    .await
                    .expect("foreign status")
                    .expect("foreign run")
                    .status
                    == workflow::RunStatus::Completed
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("real unrelated shared-parent provider call must complete");
        let attempts =
            representation::inspection_attempts::page(&self.db, "xray-foreign-shared", None, 8)
                .await
                .expect("foreign canonical attempt");
        assert_eq!(attempts.len(), 1);
        let receipt = representation::inspection_attempts::diagnostic_receipt(
            &self.db,
            "xray-foreign-shared",
            &attempts[0].dispatch_identity,
        )
        .await
        .expect("foreign receipt")
        .expect("real admitted foreign turn");
        let admitted = crate::workflow_receipts::AgentTurnReceipt::decode(&receipt.receipt)
            .expect("foreign receipt contract");
        assert_eq!(admitted.session_id, self.parent);
        let evidence = self
            .fixture
            .0
            .sessions
            .session_turn_evidence(self.parent, &admitted.turn_id)
            .await
            .expect("foreign real provider evidence");
        assert!(
            evidence.iter().any(|event| matches!(&event.kind,
            bcode_session_models::SessionEventKind::ModelTurnFinished {
                turn_id, outcome: bcode_session_models::ModelTurnOutcome::Completed, ..
            } if turn_id == &admitted.turn_id)),
            "unrelated shared-parent turn must have exact canonical completed outcome"
        );
    }
    async fn finalize(
        &mut self,
        ready: &dto::ExecutionWatchReady,
        driven: Driven,
    ) -> (
        Vec<dto::ExecutionWatchEvent>,
        dto::ExecutionWatchFinalEvidence,
        u64,
    ) {
        let Driven {
            mut cursor,
            mut events,
            ..
        } = driven;
        let cutoff = diagnostic!(
            self,
            finalize_execution_watch,
            self.transport.caller,
            dto::FinalizeExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone(),
                offset: 0,
                limit: ready.max_page_items
            }
        )
        .expect("observer-only cutoff");
        assert!(
            cutoff.next_offset.is_none(),
            "three-run cutoff must fit fixture page"
        );
        assert_eq!(
            cutoff.prior_execution_gap, !self.immediate,
            "cutoff preserves fresh-admission versus late-attachment evidence"
        );
        assert_eq!(cutoff.omitted_links, 0);
        assert_eq!(cutoff.omitted_events, 0);
        assert_eq!(cutoff.contention_gaps, 0);
        loop {
            let page = diagnostic!(
                self,
                read_execution_watch,
                self.transport.caller,
                dto::ReadExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    cursor,
                    limit: ready.max_page_items
                }
            )
            .expect("drain cutoff envelopes");
            cursor = page.next_cursor;
            if page.events.is_empty() {
                break;
            }
            events.extend(page.events);
        }
        (events, cutoff, cursor)
    }
    async fn verify_evidence(
        &mut self,
        ready: &dto::ExecutionWatchReady,
        events: &[dto::ExecutionWatchEvent],
        cutoff: &dto::ExecutionWatchFinalEvidence,
        runs: &BTreeSet<String>,
    ) {
        let mut first = BTreeMap::new();
        for event in events {
            assert!(
                runs.contains(&event.run_id),
                "watch must not collect unrelated runs"
            );
            assert_eq!(
                event.envelope.event.correlation.session_id,
                event.session_id
            );
            if event.envelope.event.domain == "provider.fake"
                && event.envelope.event.name == "adapted_request"
            {
                first
                    .entry((
                        event.run_id.clone(),
                        event
                            .envelope
                            .event
                            .correlation
                            .activation_id
                            .clone()
                            .expect("provider activation identity"),
                    ))
                    .or_insert(event);
            }
        }
        assert_eq!(
            first.len(),
            4,
            "root and child results plus grandchild agent/result must each include their first real provider request"
        );
        let represented_runs: BTreeSet<_> = first.keys().map(|(run, _)| run.clone()).collect();
        assert_eq!(
            &represented_runs, runs,
            "every actual canonical run must have provider evidence"
        );
        let mut first_nodes = BTreeMap::<String, BTreeSet<String>>::new();
        for ((run, activation), event) in first {
            let correlation = &event.envelope.event.correlation;
            assert_eq!(correlation.run_id.as_deref(), Some(run.as_str()));
            assert!(correlation.attempt_id.is_some());
            assert!(correlation.activation_id.is_some());
            assert!(correlation.turn_id.is_some());
            if self.immediate {
                self.verify_payload(ready, &run, event).await;
            }
            let node = self
                .verify_canonical(events, cutoff, &run, &activation, event)
                .await;
            first_nodes.entry(run).or_default().insert(node);
        }
        self.verify_lineage(&first_nodes).await;
    }
    async fn verify_payload(
        &mut self,
        ready: &dto::ExecutionWatchReady,
        run: &str,
        event: &dto::ExecutionWatchEvent,
    ) {
        let references = event
            .envelope
            .payload_references()
            .expect("explicit real provider attachment");
        assert_eq!(
            references.len(),
            1,
            "large first provider request requires actual retained bytes"
        );
        let reference = &references[0];
        for length in [0, u32::MAX] {
            assert!(
                diagnostic!(
                    self,
                    export_execution_watch_payload,
                    self.transport.caller,
                    dto::ExportExecutionWatchPayloadRequest {
                        schema_version: 1,
                        watch_id: ready.watch_id.clone(),
                        run_id: run.to_owned(),
                        payload: bcode_xray_models::XrayReadPayloadRequest {
                            schema_version: 1,
                            observation_id: event.envelope.observation_id.clone(),
                            session_id: event.session_id.clone(),
                            payload_id: reference.payload_id.clone(),
                            offset: 0,
                            length,
                        },
                    }
                )
                .is_err(),
                "retained first-call range bounds must reject invalid lengths"
            );
        }
        let bytes = self.export_ranges(ready, run, event, reference).await;
        assert_eq!(
            u64::try_from(bytes.len()).expect("length"),
            reference.byte_len
        );
        assert_eq!(hex::encode(Sha256::digest(&bytes)), reference.sha256);
        let request: serde_json::Value =
            serde_json::from_slice(&bytes).expect("real provider request projection");
        // The authored workflow objective is a user message, not the host's
        // system prompt. Decode the provider's actual normalized message schema
        // and require the entire fixture, including every Unicode repetition.
        let messages: Vec<bcode_model::ModelMessage> =
            serde_json::from_value(request["messages"].clone())
                .expect("typed first-provider messages");
        assert!(messages.iter().any(|message| message.role == bcode_model::MessageRole::User
                && message.content.iter().any(|block| matches!(block,
                    bcode_model::ContentBlock::Text { text } if text.starts_with(&first_request_fixture())
                        && text.contains("<workflow-input-json>")))),
                "retained first request must contain the complete authored objective and structured input");
        assert!(
            diagnostic!(
                self,
                export_execution_watch_payload,
                self.transport.foreign,
                dto::ExportExecutionWatchPayloadRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    run_id: run.to_owned(),
                    payload: bcode_xray_models::XrayReadPayloadRequest {
                        schema_version: 1,
                        observation_id: event.envelope.observation_id.clone(),
                        session_id: event.session_id.clone(),
                        payload_id: reference.payload_id.clone(),
                        offset: 0,
                        length: 1
                    },
                }
            )
            .is_err(),
            "foreign authenticated connection cannot export retained first-call bytes"
        );
    }
    async fn export_ranges(
        &mut self,
        ready: &dto::ExecutionWatchReady,
        run: &str,
        event: &dto::ExecutionWatchEvent,
        reference: &bcode_xray_models::XrayPayloadRef,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        loop {
            let range = diagnostic!(
                self,
                export_execution_watch_payload,
                self.transport.caller,
                dto::ExportExecutionWatchPayloadRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone(),
                    run_id: run.to_owned(),
                    payload: bcode_xray_models::XrayReadPayloadRequest {
                        schema_version: 1,
                        observation_id: event.envelope.observation_id.clone(),
                        session_id: event.session_id.clone(),
                        payload_id: reference.payload_id.clone(),
                        offset: u64::try_from(bytes.len()).expect("bounded range offset"),
                        length: 16 * 1024,
                    },
                }
            )
            .expect("authorized actual first-request payload export");
            assert_eq!(&range.payload, reference);
            assert_eq!(range.observation_id, event.envelope.observation_id);
            assert_eq!(range.offset, u64::try_from(bytes.len()).expect("offset"));
            assert!(
                !range.bytes.is_empty(),
                "nonempty retained request cannot stall export"
            );
            bytes.extend_from_slice(&range.bytes);
            assert!(bytes.len() <= 1024 * 1024, "bounded fixture assembly");
            if range.eof {
                break;
            }
        }
        bytes
    }
    async fn verify_canonical(
        &self,
        events: &[dto::ExecutionWatchEvent],
        cutoff: &dto::ExecutionWatchFinalEvidence,
        run: &str,
        activation: &str,
        event: &dto::ExecutionWatchEvent,
    ) -> String {
        let correlation = &event.envelope.event.correlation;
        let session = event
            .session_id
            .parse()
            .expect("canonical execution session identity");
        let attempts = representation::inspection_attempts::page(&self.db, run, None, 16)
            .await
            .expect("bounded canonical attempts");
        let attempt = attempts
            .iter()
            .find(|attempt| attempt.activation_id == activation)
            .expect("first provider activation must exist in canonical ledger");
        assert_eq!(attempt.attempt, 1, "first attempt, not a retry");
        assert_eq!(
            correlation.attempt_id.as_deref(),
            Some(attempt.dispatch_identity.as_str()),
            "provider dispatch identity must match canonical first attempt"
        );
        let receipt = representation::inspection_attempts::diagnostic_receipt(
            &self.db,
            run,
            &attempt.dispatch_identity,
        )
        .await
        .expect("exact immutable owner receipt")
        .expect("admitted first turn");
        let admitted = crate::workflow_receipts::AgentTurnReceipt::decode(&receipt.receipt)
            .expect("typed canonical agent-turn receipt");
        assert_eq!(admitted.session_id, session);
        assert_eq!(
            Some(admitted.turn_id.as_str()),
            correlation.turn_id.as_deref(),
            "later turns cannot stand in for the admitted activation's first call"
        );

        if self.shared_parent {
            assert_eq!(
                session, self.parent,
                "shared target must use the actual parent session"
            );
        } else {
            let summary = self
                .fixture
                .0
                .sessions
                .session_summary(session)
                .await
                .expect("bounded canonical execution session");
            let execution = summary.execution.expect("canonical execution provenance");
            assert_eq!(execution.provenance.run_id, run);
            assert_eq!(execution.provenance.node_id, attempt.node_id);
            assert_eq!(
                execution.provenance.activation_id,
                correlation.activation_id
            );
            assert_eq!(execution.provenance.attempt, attempt.attempt);
        }
        let link = representation::attempts::owner_boundary::execution_session_link(
            &self.db,
            run,
            &attempt.node_id,
            activation,
            attempt.attempt,
        )
        .await
        .expect("canonical execution link")
        .expect("persisted execution link");
        assert_eq!(link.session_id, event.session_id);
        self.verify_turn_evidence(events, cutoff, run, event, &admitted)
            .await;
        attempt.node_id.clone()
    }
    async fn verify_turn_evidence(
        &self,
        events: &[dto::ExecutionWatchEvent],
        cutoff: &dto::ExecutionWatchFinalEvidence,
        run: &str,
        event: &dto::ExecutionWatchEvent,
        admitted: &crate::workflow_receipts::AgentTurnReceipt,
    ) {
        let session = admitted.session_id;
        let correlation = &event.envelope.event.correlation;
        let history = self
            .fixture
            .0
            .sessions
            .session_history_page(
                session,
                bcode_session_models::SessionHistoryQuery {
                    cursor: None,
                    limit: 128,
                    direction: bcode_session_models::SessionHistoryDirection::Forward,
                },
            )
            .await
            .expect("bounded first-turn history");
        let first_turn = history
            .events
            .iter()
            .filter_map(|event| match &event.kind {
                bcode_session_models::SessionEventKind::ModelTurnStarted { turn_id } => {
                    Some(turn_id.as_str())
                }
                _ => None,
            })
            .find(|turn_id| !self.shared_parent || *turn_id == admitted.turn_id)
            .expect("first admitted canonical model request must be in bounded prefix");
        assert_eq!(
            Some(first_turn),
            correlation.turn_id.as_deref(),
            "late observed model turns cannot satisfy first-call acceptance"
        );
        assert!(
            events.iter().any(|candidate| candidate.run_id == run
                && candidate.envelope.observation_id == event.envelope.observation_id
                && candidate.envelope.sequence < event.envelope.sequence
                && candidate.envelope.event.domain == "xray"
                && candidate.envelope.event.name == "ready"),
            "same capture's ready barrier must precede first request"
        );
        // The bounded turn-evidence API returns terminal/output/retry evidence,
        // not start markers. The separate history check above proves start;
        // require the exact admitted turn's successful terminal outcome here.
        let evidence = self
            .fixture
            .0
            .sessions
            .session_turn_evidence(session, correlation.turn_id.as_deref().expect("turn"))
            .await
            .expect("canonical turn evidence");
        assert!(
            evidence.iter().any(|event| matches!(
                &event.kind,
                bcode_session_models::SessionEventKind::ModelTurnFinished {
                    turn_id, outcome: bcode_session_models::ModelTurnOutcome::Completed, ..
                } if turn_id == &admitted.turn_id
            )),
            "first observed request must belong to the exact canonically completed admitted turn"
        );
        assert_eq!(
            crate::workflow_representation::status(&self.fixture.0, run)
                .await
                .expect("status")
                .expect("run")
                .status,
            workflow::RunStatus::Completed
        );
        assert!(
            cutoff.cutoffs.iter().any(|boundary| boundary.run_id == run
                && boundary.session_id == event.session_id
                && boundary.finalization.is_some()),
            "matching engine cutoff required for each run"
        );
    }
    async fn verify_lineage(&self, first_nodes: &BTreeMap<String, BTreeSet<String>>) {
        let children = representation::hierarchy::children(&self.db, RUN, None, 8)
            .await
            .expect("root's canonical children");
        assert_eq!(children.children.len(), 1);
        assert_eq!(children.children[0].root_run_id, RUN);
        assert_eq!(children.children[0].parent_run_id, RUN);
        assert_eq!(children.children[0].parent_attempt, 1);
        assert!(!children.children[0].parent_activation_id.is_empty());
        let child_run = &children.children[0].child_run_id;
        let grandchildren = representation::hierarchy::children(&self.db, child_run, None, 8)
            .await
            .expect("child's canonical children");
        assert_eq!(grandchildren.children.len(), 1);
        assert_eq!(grandchildren.children[0].root_run_id, RUN);
        assert_eq!(&grandchildren.children[0].parent_run_id, child_run);
        assert_eq!(grandchildren.children[0].parent_attempt, 1);
        assert!(!grandchildren.children[0].parent_activation_id.is_empty());
        let grandchild_run = &grandchildren.children[0].child_run_id;
        assert!(
            representation::hierarchy::children(&self.db, grandchild_run, None, 8)
                .await
                .expect("finite recursive leaf")
                .children
                .is_empty()
        );
        assert_eq!(
            crate::workflow_representation::child::parent_run(&self.fixture.0, child_run)
                .await
                .expect("canonical parent"),
            Some(RUN.to_owned())
        );
        assert_eq!(
            crate::workflow_representation::child::parent_run(&self.fixture.0, grandchild_run)
                .await
                .expect("canonical grandparent link"),
            Some(child_run.clone())
        );
        assert_eq!(first_nodes[RUN], BTreeSet::from(["result".to_owned()]));
        assert_eq!(
            first_nodes[child_run],
            BTreeSet::from(["result".to_owned()])
        );
        assert_eq!(
            first_nodes[grandchild_run],
            BTreeSet::from(["agent".to_owned(), "result".to_owned()]),
            "grandchild's first leaf call must not be missing behind its later result call"
        );
    }
    async fn verify_late_and_revoke(&mut self, ready: dto::ExecutionWatchReady, cursor: u64) {
        assert!(
            diagnostic!(
                self,
                revoke_execution_watch,
                self.transport.foreign,
                dto::RevokeExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id.clone()
                }
            )
            .is_err(),
            "watch principal cannot be forged"
        );
        diagnostic!(
            self,
            revoke_execution_watch,
            self.transport.caller,
            dto::RevokeExecutionWatchRequest {
                schema_version: 1,
                watch_id: ready.watch_id.clone()
            }
        )
        .expect("capture revocation without execution cancellation");
        assert!(
            diagnostic!(
                self,
                read_execution_watch,
                self.transport.caller,
                dto::ReadExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id,
                    cursor,
                    limit: 1,
                }
            )
            .is_err(),
            "revoked watch must not remain readable"
        );
        self.verify_late_attachment().await;
    }
    async fn verify_late_attachment(&mut self) {
        let late = diagnostic!(
            self,
            open_execution_watch,
            self.transport.caller,
            dto::OpenExecutionWatchRequest {
                schema_version: 1,
                scope: dto::ExecutionWatchScope {
                    root_run_id: RUN.into(),
                    recursive: true,
                    deep_capture: true
                },
            }
        )
        .expect("explicit late attachment to settled run");
        assert!(
            late.prior_execution_gap,
            "late watch must never inherit fresh-admission coverage"
        );
        let late_page = diagnostic!(
            self,
            read_execution_watch,
            self.transport.caller,
            dto::ReadExecutionWatchRequest {
                schema_version: 1,
                watch_id: late.watch_id.clone(),
                cursor: 0,
                limit: late.max_page_items,
            }
        )
        .expect("bounded late read");
        assert!(late_page.prior_execution_gap);
        assert!(
            late_page.events.is_empty(),
            "settled provider calls cannot be reconstructed as live first-call evidence"
        );
        let late_cutoff = diagnostic!(
            self,
            finalize_execution_watch,
            self.transport.caller,
            dto::FinalizeExecutionWatchRequest {
                schema_version: 1,
                watch_id: late.watch_id.clone(),
                offset: 0,
                limit: late.max_page_items,
            }
        )
        .expect("late observer cutoff");
        assert!(late_cutoff.prior_execution_gap);
        assert!(
            late_cutoff.cutoffs.is_empty(),
            "no fabricated historical capture cutoffs"
        );
        diagnostic!(
            self,
            revoke_execution_watch,
            self.transport.caller,
            dto::RevokeExecutionWatchRequest {
                schema_version: 1,
                watch_id: late.watch_id
            }
        )
        .expect("revoke late watch");
    }
    async fn finish(&mut self) {
        drop(self.transport.connections.take());
        drop(self.transport.ordinary_client.take());
        if let Some(ipc) = self.transport.ipc_fixture.take() {
            ipc.finish().await;
        }
        self.fixture.finish().await;
        assert!(
            self.transport.socket_directory.path().exists(),
            "owned IPC root retained through join"
        );
    }
}
