//! Typed IPC application-boundary coverage without an external daemon.

use super::*;
use bcode_session_models::{
    SessionActivityCoverage, SessionActivityQuery, SessionActivityScope, SessionActivityState,
    SessionChildCreateRequest, SessionChildQuery,
};

async fn exchange(stream: &mut LocalIpcStream, id: u64, request: Request) -> Response {
    let envelope = bcode_ipc::request_envelope(id, &request).unwrap();
    bcode_ipc::send_envelope(stream, &envelope).await.unwrap();
    let reply = tokio::time::timeout(Duration::from_secs(10), bcode_ipc::recv_envelope(stream))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(reply.request_id, id);
    bcode_ipc::decode_response(&reply.payload).unwrap()
}

fn created(response: Response) -> SessionSummary {
    let Response::Ok(ResponsePayload::SessionCreated { session }) = response else {
        panic!("expected creation, got {response:?}");
    };
    session
}

async fn assert_maintenance_requires_confirmation(stream: &mut LocalIpcStream, parent: SessionId) {
    let response = exchange(
        stream,
        40,
        Request::ReconcileSessionChildren {
            parent_id: parent,
            apply: false,
        },
    )
    .await;
    assert!(
        matches!(response, Response::Err(error) if error.code == "maintenance_confirmation_required")
    );
    assert_eq!(children(stream, parent).await.len(), 2);
}

async fn children(stream: &mut LocalIpcStream, parent_id: SessionId) -> Vec<SessionSummary> {
    let mut after = None;
    let mut result = Vec::new();
    for request_id in 20..30 {
        let response = exchange(
            stream,
            request_id,
            Request::SessionChildren {
                query: SessionChildQuery {
                    parent_id,
                    after,
                    limit: 1,
                },
            },
        )
        .await;
        let Response::Ok(ResponsePayload::SessionChildren { page }) = response else {
            panic!("expected children");
        };
        assert!(page.children.len() <= 1);
        result.extend(page.children);
        let Some(next) = page.next_after else {
            return result;
        };
        assert!(after.is_none_or(|previous| next > previous));
        after = Some(next);
    }
    panic!("bounded fixture traversal did not finish");
}

async fn create_tree(
    mut stream: LocalIpcStream,
) -> (LocalIpcStream, SessionId, SessionId, SessionId) {
    let cwd = std::env::current_dir().unwrap();
    let parent = created(
        exchange(
            &mut stream,
            1,
            Request::CreateSession {
                name: Some("parent".into()),
                working_directory: cwd.clone(),
            },
        )
        .await,
    );
    let request = SessionChildCreateRequest {
        child_id: SessionId::new(),
        parent_id: parent.id,
        name: Some("child".into()),
        working_directory: cwd.clone(),
    };
    let child = created(
        exchange(
            &mut stream,
            2,
            Request::CreateChildSession {
                request: request.clone(),
            },
        )
        .await,
    );
    let retry = created(
        exchange(
            &mut stream,
            3,
            Request::CreateChildSession {
                request: request.clone(),
            },
        )
        .await,
    );
    assert_eq!(retry, child);
    let grandchild = created(
        exchange(
            &mut stream,
            4,
            Request::CreateChildSession {
                request: SessionChildCreateRequest {
                    child_id: SessionId::new(),
                    parent_id: child.id,
                    name: Some("grandchild".into()),
                    working_directory: cwd,
                },
            },
        )
        .await,
    );
    assert_eq!(child.parent_id, Some(parent.id));
    assert_eq!(grandchild.parent_id, Some(child.id));
    let mut conflict = request;
    conflict.name = Some("conflicting immutable creation".into());
    assert!(matches!(
        exchange(
            &mut stream,
            5,
            Request::CreateChildSession { request: conflict }
        )
        .await,
        Response::Err(_)
    ));
    let sibling = created(
        exchange(
            &mut stream,
            6,
            Request::CreateChildSession {
                request: SessionChildCreateRequest {
                    child_id: SessionId::new(),
                    parent_id: parent.id,
                    name: Some("sibling".into()),
                    working_directory: std::env::current_dir().unwrap(),
                },
            },
        )
        .await,
    );
    verify_child_pages(&mut stream, parent.id, child.id, sibling.id, grandchild.id).await;
    (stream, parent.id, child.id, grandchild.id)
}

async fn verify_child_pages(
    stream: &mut LocalIpcStream,
    parent_id: SessionId,
    child_id: SessionId,
    sibling_id: SessionId,
    grandchild_id: SessionId,
) {
    let mut direct = children(stream, parent_id)
        .await
        .iter()
        .map(|summary| summary.id)
        .collect::<Vec<_>>();
    direct.sort();
    let mut expected = vec![child_id, sibling_id];
    expected.sort();
    assert_eq!(direct, expected);
    assert_eq!(
        children(stream, child_id)
            .await
            .iter()
            .map(|summary| summary.id)
            .collect::<Vec<_>>(),
        vec![grandchild_id]
    );
}

#[tokio::test]
async fn child_sessions_typed_ipc_creation_retry_navigation_and_activity() {
    let state = Arc::new(crate::tests::test_server_state(SessionManager::default()));
    let socket_dir = tempfile::tempdir().unwrap();
    let endpoint = bcode_ipc::IpcEndpoint::unix_socket(socket_dir.path().join("children.sock"));
    let listener = LocalIpcListener::bind(&endpoint).unwrap();
    let serving = Arc::clone(&state);
    let server = tokio::spawn(async move {
        let connection = listener.accept().await.unwrap();
        handle_client(connection, serving).await.unwrap();
    });
    let stream = LocalIpcStream::connect(&endpoint).await.unwrap();
    let (mut stream, parent_id, child_id, grandchild_id) = create_tree(stream).await;
    let navigation = exchange(
        &mut stream,
        40,
        Request::SessionSummary {
            session_id: grandchild_id,
        },
    )
    .await;
    assert!(
        matches!(navigation, Response::Ok(ResponsePayload::SessionSummary { session }) if session.parent_id == Some(child_id))
    );
    assert_maintenance_requires_confirmation(&mut stream, parent_id).await;
    for id in [parent_id, child_id, grandchild_id] {
        let response = exchange(
            &mut stream,
            41,
            Request::SessionActivity {
                session_id: id,
                query: SessionActivityQuery {
                    scope: SessionActivityScope::DirectChildren,
                    after: None,
                    limit: 1,
                },
            },
        )
        .await;
        let Response::Ok(ResponsePayload::SessionActivity { page }) = response else {
            panic!("expected activity");
        };
        assert_eq!(page.own.states, vec![SessionActivityState::Unknown]);
        assert_eq!(page.coverage, SessionActivityCoverage::Degraded);
        assert!(
            page.descendants
                .iter()
                .all(|activity| activity.states.contains(&SessionActivityState::Unknown))
        );
        assert_eq!(
            state.runtime_work.activity_count_for_session(id, 1).await,
            (0, true)
        );
    }
    let no_scheduler = state.session_runtimes.lock().await.is_empty();
    let no_parent_control = state.active_session_namespaces.lock().await.is_empty();
    let no_attachment = state.attached_client_sessions.lock().await.is_empty();
    drop(state);
    drop(stream);
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap();
    drop(socket_dir);
    assert!(no_scheduler && no_parent_control && no_attachment);
}
