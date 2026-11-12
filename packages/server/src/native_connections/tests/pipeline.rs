use super::*;
use bcode_harness_models::native_connections::NativeConnectionRecord;

pub(super) async fn approve(state: &ServerState) -> NativeConnectionRecord {
    let NativeConnectionResponse::Record { record } = execute(
        state,
        NativeConnectionRequest::Approve {
            candidate: publication().candidate,
            expected_revision: 0,
            mutation_id: "approve".into(),
        },
    )
    .await
    else {
        panic!("approval failed");
    };
    *record
}
pub(super) fn prepare(revision: u64) -> NativeConnectionRequest {
    NativeConnectionRequest::Prepare {
        candidate_id: "candidate".into(),
        expected_revision: revision,
        mutation_id: "prepare".into(),
    }
}
#[tokio::test]
async fn application_discover_approve_prepare_restart_and_instance_resolution() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture_state(root.path());
    let NativeConnectionResponse::Candidates { candidates } = execute(
        &state,
        NativeConnectionRequest::Discover {
            plugin_id: "fixture.native".into(),
        },
    )
    .await
    else {
        panic!("discovery failed");
    };
    assert_eq!(candidates, vec![publication().candidate]);
    let record = approve(&state).await;
    assert!(record.approved && record.prepared.is_none());
    assert!(
        lookup(
            &state,
            "fixture.native",
            NativeConnectionLookupRequest::Resolve {
                instance_id: "fixture-instance".into()
            }
        )
        .is_err()
    );
    let NativeConnectionResponse::Record { record } = execute(&state, prepare(1)).await else {
        panic!("preparation failed");
    };
    assert_eq!(record.prepared.as_ref(), Some(&publication()));
    drop(state);
    let restarted = Arc::new(fixture_state(root.path()));
    let resolved = lookup(
        &restarted,
        "fixture.native",
        NativeConnectionLookupRequest::Resolve {
            instance_id: "fixture-instance".into(),
        },
    )
    .unwrap();
    assert_eq!(resolved.records, vec![*record]);
    drop(resolved);
    let bridge = bridge_for_instance(Arc::clone(&restarted), publication().instance);
    drop(restarted);
    assert!(
        bridge
            .request(request("fixture-instance"), ServiceCancellation::default())
            .is_ok()
    );
}
#[tokio::test]
async fn application_denies_prepare_without_consent_and_forged_candidate() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture_state(root.path());
    assert!(matches!(
        execute(&state, prepare(0)).await,
        NativeConnectionResponse::Failed { .. }
    ));
    let mut candidate = publication().candidate;
    candidate.executable = "/fixture/forged".into();
    assert!(matches!(
        execute(
            &state,
            NativeConnectionRequest::Approve {
                candidate,
                expected_revision: 0,
                mutation_id: "forged".into()
            }
        )
        .await,
        NativeConnectionResponse::Failed {
            error: NativeConnectionError::Conflict
        }
    ));
    assert_eq!(
        NativeConnectionStore::new(root.path()).list().unwrap(),
        Vec::new()
    );
    drop(state);
}
#[tokio::test]
async fn application_revoke_blocks_reprepare_and_stale_publication() {
    let root = tempfile::tempdir().unwrap();
    let state = fixture_state(root.path());
    approve(&state).await;
    assert!(matches!(
        execute(&state, prepare(1)).await,
        NativeConnectionResponse::Record { .. }
    ));
    assert!(matches!(
        execute(
            &state,
            NativeConnectionRequest::Revoke {
                candidate_id: "candidate".into(),
                expected_revision: 2,
                mutation_id: "revoke".into()
            }
        )
        .await,
        NativeConnectionResponse::Record { .. }
    ));
    assert!(matches!(
        execute(&state, prepare(3)).await,
        NativeConnectionResponse::Failed {
            error: NativeConnectionError::NotApproved
        }
    ));
    assert!(
        NativeConnectionStore::new(root.path())
            .publish(&publication(), 2, "late")
            .is_err()
    );
    assert!(
        lookup(
            &state,
            "fixture.native",
            NativeConnectionLookupRequest::Resolve {
                instance_id: "fixture-instance".into()
            }
        )
        .is_err()
    );
}
