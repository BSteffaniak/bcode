mod fixture;

use super::*;
use bcode_session_models::{
    CURRENT_SESSION_EVENT_SCHEMA_VERSION, ExecutionSessionContextMode, SessionEvent,
    SessionEventKind, ToolInvocationResultRecord,
};

fn reference() -> DirectContributorReference {
    DirectContributorReference {
        run_id: "run".into(),
        consumer_activation_id: "consumer".into(),
        graph_revision: 1,
        edge_ordinal: 1,
        output_id: "output".into(),
        output_checksum_sha256: "checksum".into(),
        dispatch_identity: "dispatch".into(),
        execution: WorkflowExecutionSessionLink::new(
            "run".into(),
            "source".into(),
            "activation".into(),
            1,
            SessionId::new().to_string(),
            "workspace".into(),
            1,
        ),
    }
}

fn provenance(link: &WorkflowExecutionSessionLink) -> ExecutionSessionProvenance {
    ExecutionSessionProvenance {
        version: EXECUTION_SESSION_PROVENANCE_VERSION,
        owner: "bcode.workflow".into(),
        run_id: link.run_id.clone(),
        node_id: link.node_id.clone(),
        activation_id: Some(link.activation_id.clone()),
        attempt: link.attempt,
        parent_session_id: SessionId::new(),
        context_mode: ExecutionSessionContextMode::FreshIsolated,
        workspace_snapshot: Some(link.workspace_snapshot.clone()),
        parent_generation: None,
    }
}

#[test]
fn summary_provenance_must_match_every_execution_identity() {
    let reference = reference();
    let original = provenance(&reference.execution);
    assert!(matches_provenance(&original, &reference.execution));
    for field in 0..7 {
        let mut changed = original.clone();
        match field {
            0 => changed.version += 1,
            1 => changed.owner = "foreign".into(),
            2 => changed.run_id = "foreign".into(),
            3 => changed.node_id = "foreign".into(),
            4 => changed.activation_id = None,
            5 => changed.attempt += 1,
            _ => changed.workspace_snapshot = None,
        }
        assert!(!matches_provenance(&changed, &reference.execution));
    }
}

#[test]
fn unavailable_contract_does_not_return_partial_evidence() {
    let result = unavailable();
    assert_eq!(result.contract, CONTRACT_ID);
    assert_eq!(
        result.status,
        ContributorEvidenceStatus::ProvenanceUnavailable
    );
    assert!(result.scope.is_none());
    assert!(result.events.is_empty());
    assert!(result.evidence.is_none());
}

#[test]
fn cancellation_prevents_evidence_release() {
    let token = TurnCancelState::default();
    assert!(cancelled(&token).is_ok());
    token.close();
    assert!(matches!(
        cancelled(&token),
        Err(ServerError::WorkflowComputationCancelled(_))
    ));
}

#[tokio::test]
async fn application_authenticates_and_reads_original_canonical_receipt_without_rerun() {
    let fixture = Box::pin(fixture::Fixture::new()).await;
    let token = TurnCancelState::default();
    let history_query = SessionHistoryQuery {
        cursor: None,
        limit: 100,
        direction: SessionHistoryDirection::Forward,
    };
    let before = fixture
        .state
        .sessions
        .session_history_page(fixture.source, history_query)
        .await
        .expect("history before");
    let discovered = from_invocation(
        &fixture.state,
        fixture.caller,
        &ExecutionContributorEvidenceRequest {
            expected_revision: 1,
            action: ContributorEvidenceAction::Discover {
                after_sequence: None,
                limit: 100,
            },
        },
        &token,
    )
    .await
    .expect("authenticated discovery");
    assert_eq!(discovered.status, ContributorEvidenceStatus::Available);
    assert!(
        discovered
            .events
            .iter()
            .any(|event| event.sequence == fixture.request_sequence
                && event.role == ContributorEventRole::Request)
    );
    assert!(
        discovered
            .events
            .iter()
            .any(|event| event.sequence == fixture.result_sequence
                && event.role == ContributorEventRole::Result)
    );
    let response = from_invocation(
        &fixture.state,
        fixture.caller,
        &fixture.read_request(),
        &token,
    )
    .await
    .expect("authenticated read");
    assert_eq!(response.status, ContributorEvidenceStatus::Available);
    assert_eq!(
        response.scope.expect("proved scope").session_id,
        fixture.source.to_string()
    );
    let receipt = response.evidence.expect("canonical receipt");
    assert_eq!(receipt.request_sequence, fixture.request_sequence);
    assert_eq!(receipt.result_sequence, fixture.result_sequence);
    assert_eq!(receipt.tool_name, "shell.run");
    assert_eq!(
        receipt.arguments_json.text,
        "{\"command\":\"read persisted intermediate; validate\"}"
    );
    assert_eq!(
        receipt.model_output.text,
        "{\"exit_code\":0,\"output\":\"persisted intermediate consumed; PASS\"}"
    );
    assert!(!receipt.is_error);
    let after = fixture
        .state
        .sessions
        .session_history_page(fixture.source, history_query)
        .await
        .expect("history after");
    drop(fixture);
    assert_eq!(
        before, after,
        "evidence retrieval must not append another invocation"
    );
}

#[tokio::test]
async fn application_foreign_authority_cancel_and_stale_revision_fail_closed() {
    let mut fixture = Box::pin(fixture::Fixture::new()).await;
    let token = TurnCancelState::default();
    let mut stale = fixture.read_request();
    stale.expected_revision = 2;
    let response = from_invocation(&fixture.state, fixture.caller, &stale, &token)
        .await
        .expect("stale observation");
    assert_eq!(
        response.status,
        ContributorEvidenceStatus::ProvenanceUnavailable
    );
    assert!(response.evidence.is_none());
    fixture.state.daemon_status.instance_id = "foreign-daemon".into();
    assert!(
        from_invocation(
            &fixture.state,
            fixture.caller,
            &fixture.read_request(),
            &token
        )
        .await
        .is_err()
    );
    token.close();
    assert!(matches!(
        from_invocation(
            &fixture.state,
            fixture.caller,
            &fixture.read_request(),
            &token
        )
        .await,
        Err(ServerError::WorkflowComputationCancelled(_))
    ));
    drop(fixture);
}

#[test]
fn canonical_pair_composes_typed_evidence_without_rerunning_operation() {
    let reference = reference();
    let id = reference
        .execution
        .session_id
        .parse::<SessionId>()
        .expect("fixture id");
    let make_page = |sequence, kind| SessionHistoryPage {
        session_id: id,
        events: vec![SessionEvent {
            schema_version: CURRENT_SESSION_EVENT_SCHEMA_VERSION,
            session_id: id,
            sequence,
            timestamp_ms: 1,
            provenance: None,
            kind,
        }],
        compatibility_issues: Vec::new(),
        next_cursor: None,
        has_more: false,
    };
    let request = make_page(
        10,
        SessionEventKind::ToolCallRequested {
            tool_call_id: "call".into(),
            producer_plugin_id: Some("fixture".into()),
            tool_name: "fixture.validate".into(),
            arguments_json: "{\"input\":\"persisted intermediate\"}".into(),
            working_directory: None,
        },
    );
    let result = make_page(
        70,
        SessionEventKind::ToolInvocationResultRecorded {
            record: ToolInvocationResultRecord {
                invocation_id: "call".into(),
                model_output: "read persisted intermediate; validation passed".into(),
                is_error: false,
                content: Vec::new(),
                presentation: None,
                result: None,
            },
        },
    );
    let evidence = events::read_pair(
        &events::PairQuery {
            session_id: id,
            invocation_id: "call",
            request_sequence: 10,
            result_sequence: 70,
            byte_limit: 1_024,
        },
        &request,
        &result,
    )
    .expect("canonical receipt pair");
    let response = read_response(evidence, &reference);
    assert_eq!(response.status, ContributorEvidenceStatus::Available);
    assert_eq!(response.scope.expect("scope").activation_id, "activation");
    let receipt = response.evidence.expect("receipt");
    assert_eq!(
        receipt.model_output.text,
        "read persisted intermediate; validation passed"
    );
    assert_eq!(receipt.producer_capture_complete, None);
    assert_eq!(receipt.request_sequence, 10);
    assert_eq!(receipt.result_sequence, 70);
}
