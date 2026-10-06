//! Protocol-43 child application payload round trips through both supported codecs.

use super::*;
use bcode_session_models::{
    SessionActivity, SessionActivityCoverage, SessionActivityPage, SessionActivityQuery,
    SessionActivityScope, SessionActivityState, SessionChildCreateRequest, SessionChildPage,
    SessionChildQuery,
};

#[test]
fn child_application_requests_json_and_binary_round_trip() {
    let parent = SessionId::new();
    let child = SessionId::new();
    let requests = [
        Request::CreateChildSession {
            request: SessionChildCreateRequest {
                child_id: child,
                parent_id: parent,
                name: Some("child".into()),
                working_directory: PathBuf::from("/workspace"),
            },
        },
        Request::ReconcileSessionChildren {
            parent_id: parent,
            apply: true,
        },
        Request::SessionSummary { session_id: child },
        Request::SessionChildren {
            query: SessionChildQuery {
                parent_id: parent,
                after: Some(child),
                limit: 1,
            },
        },
        Request::SessionActivity {
            session_id: parent,
            query: SessionActivityQuery {
                scope: SessionActivityScope::DirectChildren,
                after: Some(child),
                limit: 2,
            },
        },
    ];
    assert_eq!(CURRENT_PROTOCOL_VERSION, 44);
    for request in requests {
        let json = serde_json::to_vec(&request).unwrap();
        assert_eq!(serde_json::from_slice::<Request>(&json).unwrap(), request);
        let envelope = request_envelope(1, &request).unwrap();
        assert_eq!(decode_request(&envelope.payload).unwrap(), request);
    }
}

#[test]
fn child_application_responses_json_and_binary_round_trip() {
    let parent = SessionId::new();
    let child = SessionId::new();
    let summary: SessionSummary = serde_json::from_value(serde_json::json!({
        "id": child, "parent_id": parent, "name": "child", "client_count": 0,
        "created_at_ms": 0, "updated_at_ms": 0, "working_directory": "/workspace",
    }))
    .unwrap();
    let responses = [
        Response::Ok(ResponsePayload::SessionChildrenReconciled {
            result: bcode_session_models::SessionChildReconciliation {
                parent_id: parent,
                verified_children: 2,
            },
        }),
        Response::Ok(ResponsePayload::SessionCreated {
            session: summary.clone(),
        }),
        Response::Ok(ResponsePayload::SessionSummary {
            session: summary.clone(),
        }),
        Response::Ok(ResponsePayload::SessionChildren {
            page: SessionChildPage {
                children: vec![summary],
                next_after: Some(child),
                complete: false,
                issues: vec!["degraded discovery".into()],
            },
        }),
        Response::Ok(ResponsePayload::SessionActivity {
            page: SessionActivityPage {
                own: SessionActivity {
                    session_id: parent,
                    parent_id: None,
                    states: vec![SessionActivityState::Unknown],
                    evidence: None,
                },
                descendants: vec![SessionActivity {
                    session_id: child,
                    parent_id: Some(parent),
                    states: vec![
                        SessionActivityState::Queued,
                        SessionActivityState::PermissionWait,
                    ],
                    evidence: None,
                }],
                next_after: Some(child),
                coverage: SessionActivityCoverage::Degraded,
                observed_at_ms: 42,
                issues: vec!["nonresident".into()],
            },
        }),
    ];
    for response in responses {
        let json = serde_json::to_vec(&response).unwrap();
        assert_eq!(serde_json::from_slice::<Response>(&json).unwrap(), response);
        let envelope = response_envelope(1, &response).unwrap();
        assert_eq!(decode_response(&envelope.payload).unwrap(), response);
    }
}
