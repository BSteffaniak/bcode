mod generated;
use super::super::{
    ContributorEvidenceAction, EXECUTION_SESSION_PROVENANCE_VERSION,
    ExecutionContributorEvidenceRequest, ExecutionSessionProvenance, ServerState, SessionId,
    WorkflowExecutionSessionLink,
};
use bcode_session::SessionManager;
use bcode_session_models::{
    ExecutionSessionContextMode, SessionEventKind, ToolInvocationResultRecord,
};
use bcode_workflow_store::{
    DispatchReceipt, NewWorkflowRun, PreparedAttempt, ValidatedOutput, WorkflowExecutionAuthority,
    WorkflowStore,
    representation::{self, attempts, graph},
};
use generated::generated_publication;

pub struct Fixture {
    pub state: ServerState,
    pub caller: SessionId,
    pub source: SessionId,
    pub request_sequence: u64,
    pub result_sequence: u64,
    _root: tempfile::TempDir,
    path: std::path::PathBuf,
}

fn admission(parent: SessionId) -> (NewWorkflowRun, bcode_workflow::WorkflowDefinition) {
    let definition = bcode_workflow::WorkflowBuilder::new(
        "contributor-application",
        bcode_workflow::Step::task("source", |value: u32, _| async move { Ok(value) }).then(
            bcode_workflow::Step::task("evaluate", |value: u32, _| async move { Ok(value) }),
        ),
    )
    .build()
    .expect("workflow")
    .definition()
    .clone();
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        "contributor-application",
        &definition,
    )
    .expect("identity");
    let run = NewWorkflowRun {
        run_id: "contributor-application".into(),
        definition_id: identity.definition_id,
        definition_version: identity.definition_version,
        workspace_snapshot: "workspace".into(),
        parent_session_id: Some(parent.to_string()),
        parent_session_generation: None,
        binding: None,
        authored_provenance: None,
        input: Some(serde_json::json!(1)),
        execution_authority: None,
        created_at_ms: 1,
        authorization_profile: bcode_workflow::WorkflowAuthorizationProfileIdentity {
            version: 1,
            provider_id: "fixture".into(),
            profile_id: "build".into(),
            policy_digest_sha256: "a".repeat(64),
        },
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::ReadOnly,
        limits: bcode_workflow::WorkflowRunLimits::default(),
    };
    (run, definition)
}

async fn dispatch(
    state: &ServerState,
    db: &dyn switchy_database::Database,
    owner: &WorkflowExecutionAuthority,
    run: &NewWorkflowRun,
    node: &str,
    parent: SessionId,
) -> WorkflowExecutionSessionLink {
    let activation = graph::activation_id(db, &run.run_id, node)
        .await
        .expect("activation query")
        .expect("activation");
    let prepared = PreparedAttempt {
        run_id: run.run_id.clone(),
        node_id: node.into(),
        activation_id: activation.clone(),
        attempt: 1,
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        intent: serde_json::json!({}),
        prepared_at_ms: 1,
    };
    let identity = attempts::prepare(db, owner, &prepared)
        .await
        .expect("prepare");
    let session = state
        .sessions
        .create_fresh_execution_session(
            None,
            ExecutionSessionProvenance {
                version: EXECUTION_SESSION_PROVENANCE_VERSION,
                owner: "bcode.workflow".into(),
                run_id: run.run_id.clone(),
                node_id: node.into(),
                activation_id: Some(activation.clone()),
                attempt: 1,
                parent_session_id: parent,
                context_mode: ExecutionSessionContextMode::FreshIsolated,
                workspace_snapshot: Some(run.workspace_snapshot.clone()),
                parent_generation: None,
            },
            None,
        )
        .await
        .expect("execution session");
    let link = WorkflowExecutionSessionLink::new(
        run.run_id.clone(),
        node.into(),
        activation,
        1,
        session.id.to_string(),
        run.workspace_snapshot.clone(),
        2,
    );
    attempts::owner_boundary::persist_execution_session_link(db, owner, &link)
        .await
        .expect("session link");
    attempts::attach_receipt(
        db,
        owner,
        &DispatchReceipt {
            run_id: link.run_id.clone(),
            node_id: link.node_id.clone(),
            activation_id: link.activation_id.clone(),
            attempt: 1,
            dispatch_identity: identity,
            receipt: serde_json::json!({}),
            admitted_at_ms: 2,
        },
    )
    .await
    .expect("receipt");
    link
}

async fn seed_receipt(state: &ServerState, source: SessionId) -> (u64, u64) {
    let request = state
        .sessions
        .append_event(
            source,
            SessionEventKind::ToolCallRequested {
                tool_call_id: "validated-operation".into(),
                producer_plugin_id: Some("fixture.shell".into()),
                tool_name: "shell.run".into(),
                arguments_json: "{\"command\":\"read persisted intermediate; validate\"}".into(),
                working_directory: None,
            },
        )
        .await
        .expect("canonical request");
    let result = state
        .sessions
        .append_tool_invocation_result(
            source,
            ToolInvocationResultRecord {
                invocation_id: "validated-operation".into(),
                model_output:
                    "{\"exit_code\":0,\"output\":\"persisted intermediate consumed; PASS\"}".into(),
                is_error: false,
                content: Vec::new(),
                presentation: None,
                result: None,
            },
        )
        .await
        .expect("canonical result");
    (request.sequence, result.sequence)
}

/// Configure the fixture topology and keep its admission identity in sync.
fn configured_admission(
    parent: SessionId,
    generated: bool,
) -> (NewWorkflowRun, bcode_workflow::WorkflowDefinition) {
    let (mut run, mut definition) = admission(parent);
    if generated {
        let mut sibling = definition.nodes["source"].clone();
        sibling.id = "sibling".into();
        definition.nodes.insert("sibling".into(), sibling);
        definition.entries.push("sibling".into());
        let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
            "contributor-application",
            &definition,
        )
        .unwrap();
        run.definition_id = identity.definition_id;
        run.definition_version = identity.definition_version;
    }
    (run, definition)
}

impl Fixture {
    pub async fn new() -> Self {
        Box::pin(Self::create(false)).await
    }

    pub async fn generated() -> Self {
        Box::pin(Self::create(true)).await
    }

    async fn create(generated: bool) -> Self {
        let root = tempfile::tempdir().expect("root");
        let sessions = SessionManager::default();
        let parent = sessions
            .create_session(None, root.path().to_path_buf())
            .await
            .expect("parent");
        let store = WorkflowStore::create_coexistence_baseline(root.path()).expect("store");
        let path = store.path().to_path_buf();
        let state =
            crate::tests::test_server_state_with_fake_provider_and_workflow_store(sessions, store);
        let db = switchy_database_connection::open_sqlite_rusqlite(
            &path,
            &switchy_database_connection::SqliteOpenOptions {
                access: switchy_database_connection::SqliteAccess::ReadWrite,
                allow_uri: false,
                busy_timeout: std::time::Duration::ZERO,
                foreign_keys: true,
                connections: std::num::NonZeroUsize::MIN,
            },
        )
        .expect("database");
        representation::install(&db).await.expect("representation");
        let (run, definition) = configured_admission(parent.id, generated);
        representation::admit(&db, &run, &definition)
            .await
            .expect("admit");
        let owner = WorkflowExecutionAuthority {
            target_artifact_id: crate::workflow_operations::current_artifact_id(&state),
            daemon_instance_id: state.daemon_status.instance_id.clone(),
            generation: 1,
            fencing_token: "token".into(),
        };
        attempts::claim_authority(&db, &run.run_id, &owner)
            .await
            .expect("authority");
        let source_link = dispatch(&state, &db, &owner, &run, "source", parent.id).await;
        let source = source_link
            .session_id
            .parse::<SessionId>()
            .expect("source id");
        if generated {
            generated_publication(&db, &owner, &source_link, &definition).await;
        }
        let (mut request_sequence, mut result_sequence) = seed_receipt(&state, source).await;
        graph::completion::complete(
            &db,
            &owner,
            if generated { 2 } else { 1 },
            &bcode_workflow_store::dispatch_identity(
                &run.run_id,
                "source",
                &source_link.activation_id,
                1,
            ),
            &ValidatedOutput {
                output_id: "source-output".into(),
                run_id: run.run_id.clone(),
                node_id: "source".into(),
                activation_id: source_link.activation_id,
                schema_id: definition.nodes["source"].output.type_name.clone(),
                schema_version: 1,
                value: serde_json::json!(1),
                artifact_reference: None,
                created_at_ms: 3,
            },
        )
        .await
        .expect("source complete");
        let mut evidence_source = source;
        if generated {
            (evidence_source, request_sequence, result_sequence) =
                generated::seed_contributors(&state, &db, &owner, &run, parent.id, &definition)
                    .await;
        }
        let caller = dispatch(&state, &db, &owner, &run, "evaluate", parent.id)
            .await
            .session_id
            .parse()
            .expect("caller id");
        Self {
            state,
            caller,
            source: evidence_source,
            request_sequence,
            result_sequence,
            _root: root,
            path,
        }
    }

    pub fn read_request(&self) -> ExecutionContributorEvidenceRequest {
        ExecutionContributorEvidenceRequest {
            expected_revision: 1,
            action: ContributorEvidenceAction::Read {
                contributor_node_id: None,
                invocation_id: "validated-operation".into(),
                request_sequence: self.request_sequence,
                result_sequence: self.result_sequence,
                byte_limit: 4_096,
            },
        }
    }
}
