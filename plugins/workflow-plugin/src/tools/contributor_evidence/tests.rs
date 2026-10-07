use super::*;

fn request() -> ExecutionContributorEvidenceRequest {
    parse(json!({"expected_revision":1,"action":{"kind":"discover","after_sequence":null,"limit":10}})).unwrap()
}

#[test]
fn portable_envelope_normalizes_and_preserves_strict_typed_validation() {
    let tool = definition();
    let normalized = bcode_model_schema::normalize(
        &tool.input_schema,
        &bcode_model_schema::SchemaDialect {
            object_properties: bcode_model_schema::ObjectPropertyPolicy::RequireAllAndClose,
            one_of: bcode_model_schema::OneOfPolicy::CollapseAnnotatedConstants,
            reference_siblings:
                bcode_model_schema::ReferenceSiblingPolicy::RemoveAnnotationsRejectSemantic,
            ..bcode_model_schema::SchemaDialect::default()
        },
    )
    .expect("portable evidence envelope");
    let validator = jsonschema::validator_for(&normalized).unwrap();
    for arguments in [
        json!({"expected_revision":1,"action":{"kind":"discover","after_sequence":null,"limit":10}}),
        json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":72,"result_sequence":78,"byte_limit":8192}}),
    ] {
        let wrapped = json!({"request_json":arguments.to_string()});
        assert!(validator.validate(&wrapped).is_ok());
        assert_eq!(parse(wrapped).unwrap(), parse(arguments).unwrap());
    }
    assert!(parse(json!({"request_json":"not JSON"})).is_err());
    assert!(parse(json!({"request_json":123})).is_err());
    let mut arguments = serde_json::to_value(request()).unwrap();
    arguments["session_id"] = json!("forged");
    assert!(parse(json!({"request_json":arguments.to_string()})).is_err());
    assert!(parse(json!({"request_json":serde_json::to_value(request()).unwrap().to_string(),"expected_revision":1})).is_err());
    let invalid_bound = json!({"expected_revision":1,"action":{"kind":"discover","after_sequence":null,"limit":101}});
    assert!(parse(json!({"request_json":invalid_bound.to_string()})).is_err());
}

#[test]
fn preparation_binds_exact_action_revision_and_operation() {
    let payload = serde_json::to_value(request()).unwrap();
    let descriptor = json!({"operation":OPERATION,"edit":payload,"route_id":"authenticated"});
    assert!(prepared_matches(&descriptor, &payload));
    let mut changed = payload.clone();
    changed["expected_revision"] = json!(2);
    assert!(!prepared_matches(&descriptor, &changed));
    changed = payload.clone();
    changed["action"]["limit"] = json!(1);
    assert!(!prepared_matches(&descriptor, &changed));
    let mut wrong_operation = descriptor;
    wrong_operation["operation"] = json!("execution_context");
    assert!(!prepared_matches(&wrong_operation, &payload));
}

#[test]
fn bounded_actions_reject_selectors_unknown_fields_and_invalid_pairs() {
    let discover = request();
    assert_eq!(discover.expected_revision, 1);
    for selector in ["session_id", "contributor", "run_id"] {
        let mut arguments = serde_json::to_value(&discover).unwrap();
        arguments[selector] = json!("forged");
        assert!(parse(arguments).is_err());
    }
    assert!(parse(json!({"expected_revision":1,"action":{"kind":"discover","after_sequence":null,"limit":101}})).is_err());
    assert!(parse(json!({"expected_revision":0,"action":{"kind":"discover","after_sequence":null,"limit":1}})).is_err());
    assert!(parse(json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":78,"result_sequence":72,"byte_limit":100}})).is_err());
    assert!(parse(json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":72,"result_sequence":78,"byte_limit":65537}})).is_err());
    let exact = parse(json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":72,"result_sequence":78,"byte_limit":8192}})).unwrap();
    assert!(matches!(
        exact.action,
        ContributorEvidenceAction::Read {
            request_sequence: 72,
            result_sequence: 78,
            ..
        }
    ));
}

fn response() -> ExecutionContributorEvidenceResponse {
    ExecutionContributorEvidenceResponse {
        contract: CONTRACT_ID.into(),
        status: ContributorEvidenceStatus::Available,
        scope: Some(
            bcode_workflow::contributor_evidence::ContributorEvidenceScope {
                run_id: "run".into(),
                consumer_activation_id: "evaluate".into(),
                graph_revision: 1,
                node_id: "implement".into(),
                activation_id: "implementation".into(),
                attempt: 1,
                session_id: "canonical".into(),
                output_id: "output".into(),
                output_checksum_sha256: "checksum".into(),
            },
        ),
        events: Vec::new(),
        scanned_events: 0,
        next_sequence: None,
        evidence: None,
    }
}

#[test]
fn response_contract_and_scope_are_checked_not_guessed() {
    let request = request();
    let mut response = response();
    assert!(valid_response(&response, &request));
    response.contract = "bcode.workflow.execution_contributor_evidence.v2".into();
    assert!(!valid_response(&response, &request));
    response.contract = CONTRACT_ID.into();
    response.scope.as_mut().unwrap().graph_revision = 2;
    assert!(!valid_response(&response, &request));
    response.status = ContributorEvidenceStatus::Unsupported;
    assert!(!valid_response(&response, &request));
    response.scope = None;
    assert!(valid_response(&response, &request));
}

#[test]
fn discovery_cannot_exceed_scan_bound_or_claim_a_pair() {
    let request = request();
    let mut response = response();
    response.scanned_events = 11;
    assert!(!valid_response(&response, &request));
    response.scanned_events = 1;
    response.events = vec![
        bcode_workflow::contributor_evidence::ContributorEventReference {
            sequence: 72,
            invocation_id: "shell".into(),
            role: bcode_workflow::contributor_evidence::ContributorEventRole::Request,
        };
        2
    ];
    assert!(!valid_response(&response, &request));
}

#[test]
fn producer_shaped_exact_read_survives_invocation_response_boundary() {
    use bcode_workflow::contributor_evidence::{
        ContributorEvidenceText, ContributorInvocationEvidence,
    };
    let query = parse(json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":72,"result_sequence":78,"byte_limit":8192}})).unwrap();
    let mut producer = response();
    producer.scanned_events = 2;
    let arguments = "{\"command\":\"printf ok\"}";
    producer.evidence = Some(ContributorInvocationEvidence {
        invocation_id: "shell".into(),
        request_sequence: 72,
        result_sequence: 78,
        producer_plugin_id: Some("bcode.shell".into()),
        tool_name: "shell.run".into(),
        arguments_json: ContributorEvidenceText {
            text: arguments.into(),
            original_bytes: arguments.len(),
            retained_bytes: arguments.len(),
            truncated: false,
        },
        model_output: ContributorEvidenceText {
            text: "ok".into(),
            original_bytes: 2,
            retained_bytes: 2,
            truncated: false,
        },
        is_error: false,
        producer_capture_complete: None,
    });
    let wire = ServiceBridgeResponse::Service(ToolInvocationServiceResolution::Responded {
        payload: serde_json::to_value(&producer).unwrap(),
    });
    let serialized = serde_json::to_vec(&wire).unwrap();
    let decoded: ServiceBridgeResponse = serde_json::from_slice(&serialized).unwrap();
    let invoked = invocation_response(decoded, &query);
    assert!(
        invoked.error.is_none(),
        "producer-shaped successful pair must be readable"
    );
    let tool: bcode_tool::ToolInvocationResponse =
        serde_json::from_slice(&invoked.payload).unwrap();
    assert!(!tool.is_error);
    let displayed: ExecutionContributorEvidenceResponse =
        serde_json::from_str(&tool.output).unwrap();
    assert_eq!(displayed, producer);
    producer.scanned_events = 0;
    assert!(!valid_response(&producer, &query));
}

#[test]
fn exact_pair_checks_identity_text_bounds_and_truncation() {
    use bcode_workflow::contributor_evidence::{
        ContributorEvidenceText, ContributorInvocationEvidence,
    };
    let request = parse(json!({"expected_revision":1,"action":{"kind":"read","invocation_id":"shell","request_sequence":72,"result_sequence":78,"byte_limit":4}})).unwrap();
    let mut response = response();
    response.scanned_events = 2;
    let text = ContributorEvidenceText {
        text: "ok".into(),
        original_bytes: 2,
        retained_bytes: 2,
        truncated: false,
    };
    response.evidence = Some(ContributorInvocationEvidence {
        invocation_id: "shell".into(),
        request_sequence: 72,
        result_sequence: 78,
        producer_plugin_id: None,
        tool_name: "shell.run".into(),
        arguments_json: text.clone(),
        model_output: text,
        is_error: false,
        producer_capture_complete: None,
    });
    assert!(valid_response(&response, &request));
    response.evidence.as_mut().unwrap().result_sequence = 79;
    assert!(!valid_response(&response, &request));
    response.evidence.as_mut().unwrap().result_sequence = 78;
    response.evidence.as_mut().unwrap().model_output.truncated = true;
    assert!(!valid_response(&response, &request));
    response
        .evidence
        .as_mut()
        .unwrap()
        .model_output
        .original_bytes = 3;
    assert!(valid_response(&response, &request));
    response
        .evidence
        .as_mut()
        .unwrap()
        .model_output
        .text
        .push('!');
    assert!(!valid_response(&response, &request));
}
