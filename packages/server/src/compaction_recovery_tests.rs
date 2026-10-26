//! Behavioral safe-point recovery tests; no provider dispatch is needed.
use super::*;

#[tokio::test]
async fn safe_point_persists_across_interrupted_future_and_retires_on_return() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let session = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let id = session.id;
    drop(session);
    let recovery = ModelTurnRecoveryState::default();
    let rounds = ModelTurnRoundState::new(Some(5));
    let entered = tokio::sync::Notify::new();
    {
        let operation = protect(
            &state,
            id,
            "interrupted",
            17,
            (&recovery, &rounds, 3, 9),
            async {
                entered.notify_one();
                std::future::pending::<()>().await;
            },
        );
        tokio::pin!(operation);
        tokio::select! {
            result = &mut operation => panic!("unexpected completion: {result:?}"),
            () = entered.notified() => {}
        }
    }
    let checkpoint = provider_retry_recovery::load(&state, id, "interrupted")
        .await
        .unwrap()
        .expect("durable safe point");
    assert!(matches!(checkpoint.compaction, Some(Phase::Safe)));
    assert_eq!(
        protect(
            &state,
            id,
            "finished",
            19,
            (&recovery, &rounds, 3, 9),
            async { 42 }
        )
        .await
        .unwrap(),
        42
    );
    assert!(
        provider_retry_recovery::load(&state, id, "finished")
            .await
            .unwrap()
            .is_none()
    );
    drop(state);
}

#[tokio::test]
async fn native_uncertainty_is_sticky_and_cannot_be_certified_by_local_fallback() {
    let state = crate::tests::test_server_state(bcode_session::SessionManager::default());
    let session = state
        .sessions
        .create_session(None, std::env::temp_dir())
        .await
        .unwrap();
    let id = session.id;
    drop(session);
    let recovery = ModelTurnRecoveryState::default();
    let rounds = ModelTurnRoundState::new(None);
    Box::pin(protect(
        &state,
        id,
        "native",
        1,
        (&recovery, &rounds, 0, 0),
        async {
            transition(&state, id, Phase::Native).await.unwrap();
            transition(&state, id, Phase::Safe).await.unwrap();
            let checkpoint = provider_retry_recovery::load(&state, id, "native")
                .await
                .unwrap()
                .expect("checkpoint");
            let phase = checkpoint.compaction.expect("phase");
            assert!(matches!(phase, Phase::Native));
            assert!(!can_resume(&state, id, &phase).await.unwrap());
        },
    ))
    .await
    .unwrap();
    drop(state);
}
