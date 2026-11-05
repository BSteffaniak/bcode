//! Local routing writers coordinate without weakening the durable foreign-writer fence.
use super::*;
use std::{future::Future, pin::Pin, task::Poll};

async fn fixture(root: &Path) -> (ServerState, SessionId) {
    let sessions = bcode_session::SessionManager::persistent_lazy(root);
    let session = sessions.create_session(None, root.into()).await.unwrap().id;
    let mut state = crate::tests::test_server_state(sessions);
    state.state_root = root.into();
    let mut config = bcode_config::BcodeConfig {
        active_context: Some("routing-writer-test".into()),
        ..bcode_config::BcodeConfig::default()
    };
    config.auth.profiles.insert(
        "work".into(),
        bcode_config::AuthProfileConfig {
            owner_plugin_id: Some("test.plugin".into()),
            backend: "unavailable-test-backend".into(),
            ..bcode_config::AuthProfileConfig::default()
        },
    );
    state.session_configs.lock().await.insert(session, config);
    state
        .session_config_launches
        .lock()
        .await
        .insert(session, bcode_config::ConfigLaunchContext::default());
    state.session_model_selections.lock().await.insert(
        session,
        SessionModelSelection {
            provider_plugin_id: Some("test.plugin".into()),
            ..SessionModelSelection::default()
        },
    );
    (state, session)
}

// Poll the real operation until it has joined this exact queue. No elapsed-time
// inference, sleeps, provider calls, or task scheduling assumptions are involved.
async fn waiting<F: Future>(future: Pin<&mut F>, queue: &Arc<tokio::sync::Mutex<()>>) {
    let mut future = future;
    let retained_before = Arc::strong_count(queue);
    std::future::poll_fn(|cx| {
        assert!(
            future.as_mut().poll(cx).is_pending(),
            "writer bypassed queue"
        );
        if Arc::strong_count(queue) > retained_before {
            Poll::Ready(())
        } else {
            Poll::Pending
        }
    })
    .await;
}

#[tokio::test]
async fn same_key_across_servers_waits_and_preserves_both_session_routes() {
    let root = tempfile::tempdir().unwrap();
    let (first, first_session) = fixture(root.path()).await;
    let (mut second, second_session) = fixture(root.path()).await;
    // An alias must use the same canonical root identity.
    second.state_root = root.path().join(".");
    let key = context(&first, first_session).await.unwrap().key;
    assert_eq!(key, context(&second, second_session).await.unwrap().key);
    let queue = writer_queue(writer_root(root.path()).await.unwrap(), &key);
    let held = Arc::clone(&queue).lock_owned().await;
    let first_write = initialize(&first, first_session);
    tokio::pin!(first_write);
    waiting(first_write.as_mut(), &queue).await;
    let second_write = initialize(&second, second_session);
    tokio::pin!(second_write);
    waiting(second_write.as_mut(), &queue).await;
    assert!(
        account_settings::load(root.path(), &key)
            .unwrap()
            .sessions
            .is_empty()
    );
    drop(held);
    let (first_result, second_result) = tokio::join!(first_write, second_write);
    first_result.unwrap();
    second_result.unwrap();
    let settings = account_settings::load(root.path(), &key).unwrap();
    assert_eq!(settings.sessions.len(), 2);
    for session in [first_session, second_session] {
        assert_eq!(
            settings.sessions[&session.to_string()],
            AccountRoute::FollowConfiguration
        );
    }
}

#[tokio::test]
async fn different_configuration_and_state_root_writers_make_progress() {
    let root = tempfile::tempdir().unwrap();
    let other_root = tempfile::tempdir().unwrap();
    let (first, first_session) = fixture(root.path()).await;
    let (second, second_session) = fixture(root.path()).await;
    second
        .session_configs
        .lock()
        .await
        .get_mut(&second_session)
        .unwrap()
        .active_context = Some("independent-routing-key".into());
    let (third, third_session) = fixture(other_root.path()).await;
    let key = context(&first, first_session).await.unwrap().key;
    assert_ne!(key, context(&second, second_session).await.unwrap().key);
    assert_eq!(key, context(&third, third_session).await.unwrap().key);
    let held = routing_writer(root.path(), &key).await.unwrap();
    initialize(&second, second_session).await.unwrap();
    initialize(&third, third_session).await.unwrap();
    assert!(
        account_settings::load(root.path(), &key)
            .unwrap()
            .sessions
            .is_empty()
    );
    assert!(
        account_settings::load(other_root.path(), &key)
            .unwrap()
            .sessions
            .contains_key(&third_session.to_string())
    );
    drop(held);
}

#[tokio::test]
async fn freeze_rechecks_route_after_waiting_instead_of_using_stale_defaults() {
    let root = tempfile::tempdir().unwrap();
    let (state, session) = fixture(root.path()).await;
    let key = context(&state, session).await.unwrap().key;
    let queue = writer_queue(writer_root(root.path()).await.unwrap(), &key);
    let held = Arc::clone(&queue).lock_owned().await;
    let mut selection = SessionModelSelection::default();
    {
        let freeze_turn = freeze(&state, session, &mut selection);
        tokio::pin!(freeze_turn);
        waiting(freeze_turn.as_mut(), &queue).await;
        // A newly selected, unavailable account must be rejected, not replaced by
        // the previously observed FollowConfiguration route or ambient credentials.
        account_settings::update(root.path(), &key, |settings| {
            settings.sessions.insert(
                session.to_string(),
                AccountRoute::Only {
                    account: AccountId {
                        provider_id: "missing.provider".into(),
                        profile: "missing-profile".into(),
                    },
                },
            );
            Ok(())
        })
        .unwrap();
        drop(held);
        assert!(freeze_turn.await.is_err());
    }
    assert!(!selection.provider_context.account_routing_frozen);
    let settings = account_settings::load(root.path(), &key).unwrap();
    assert!(matches!(
        settings.sessions[&session.to_string()],
        AccountRoute::Only { .. }
    ));
}

#[derive(Clone, Copy, Debug)]
enum Mutation {
    Initialize,
    Freeze,
    Route,
    Metadata,
    Usage,
}

async fn mutate(state: &ServerState, session: SessionId, mutation: Mutation) {
    let account = AccountId {
        provider_id: "test.plugin".into(),
        profile: "work".into(),
    };
    match mutation {
        Mutation::Initialize => initialize(state, session).await.unwrap(),
        Mutation::Freeze => {
            freeze(state, session, &mut SessionModelSelection::default())
                .await
                .unwrap();
        }
        Mutation::Route => {
            set_route(
                state,
                session,
                SetAccountRoutingRequest {
                    scope: AccountRoutingScope::Session,
                    route: AccountRoute::FollowConfiguration,
                },
            )
            .await
            .unwrap();
        }
        Mutation::Metadata => {
            rename(
                state,
                session,
                RenameAccountRequest {
                    account,
                    label: Some("writer-test".into()),
                },
            )
            .await
            .unwrap();
        }
        Mutation::Usage => {
            record_usage(
                state,
                session,
                &bcode_model::ModelAvailabilityObservation {
                    model_id: "fixture-model".into(),
                    status: bcode_model::ModelCheckStatus::Working,
                    reason: bcode_model::ModelCheckReason::ObservedSuccess,
                    scope: bcode_model::ModelCheckScope {
                        target: bcode_model::ModelCatalogSupportHint {
                            provider: "test".into(),
                            auth_mode: "fixture".into(),
                            api_surface: "fixture".into(),
                            integration: None,
                        },
                        profile: Some(account.profile),
                        endpoint_fingerprint: "fixture-endpoint".into(),
                        account_fingerprint: "fixture-account".into(),
                    },
                    attempt_started_at_ms: 1,
                    attempt_ordinal: 1,
                    observed_at_ms: 2,
                },
            )
            .await;
        }
    }
}

#[tokio::test]
async fn every_mutation_waits_for_the_configuration_writer() {
    for mutation in [
        Mutation::Initialize,
        Mutation::Freeze,
        Mutation::Route,
        Mutation::Metadata,
        Mutation::Usage,
    ] {
        let root = tempfile::tempdir().unwrap();
        let (state, session) = fixture(root.path()).await;
        let key = context(&state, session).await.unwrap().key;
        let queue = writer_queue(writer_root(root.path()).await.unwrap(), &key);
        let held = Arc::clone(&queue).lock_owned().await;
        let operation = mutate(&state, session, mutation);
        tokio::pin!(operation);
        waiting(operation.as_mut(), &queue).await;
        assert_eq!(
            account_settings::load(root.path(), &key).unwrap(),
            AccountSettings::default()
        );
        drop(held);
        operation.await;
        let settings = account_settings::load(root.path(), &key).unwrap();
        match mutation {
            Mutation::Initialize | Mutation::Freeze | Mutation::Route => {
                assert_eq!(
                    settings.sessions[&session.to_string()],
                    AccountRoute::FollowConfiguration
                );
            }
            Mutation::Metadata => {
                assert_eq!(settings.accounts[0].label.as_deref(), Some("writer-test"));
            }
            Mutation::Usage => {
                assert_eq!(settings.last_used[&session.to_string()].observed_at_ms, 2);
            }
        }
    }
}

#[tokio::test]
async fn absent_state_root_identity_does_not_create_directories() {
    let root = tempfile::tempdir().unwrap();
    let absent = root.path().join("new-state/nested");
    let expected = root.path().canonicalize().unwrap().join("new-state/nested");
    assert_eq!(writer_root(&absent).await.unwrap(), expected);
    let held = routing_writer(&absent, "fixture-key").await.unwrap();
    assert!(!root.path().join("new-state").exists());
    drop(held);
}

#[cfg(unix)]
#[tokio::test]
async fn symlink_state_root_uses_the_same_writer_without_touching_other_roots() {
    let root = tempfile::tempdir().unwrap();
    let aliases = tempfile::tempdir().unwrap();
    let alias = aliases.path().join("state-alias");
    std::os::unix::fs::symlink(root.path(), &alias).unwrap();
    let queue = writer_queue(writer_root(root.path()).await.unwrap(), "fixture-key");
    let held = Arc::clone(&queue).lock_owned().await;
    let contender = routing_writer(&alias, "fixture-key");
    tokio::pin!(contender);
    waiting(contender.as_mut(), &queue).await;
    drop(held);
    drop(contender.await.unwrap());
}

#[tokio::test]
async fn foreign_file_lock_and_corruption_still_deny_without_replacement() {
    let root = tempfile::tempdir().unwrap();
    let (state, session) = fixture(root.path()).await;
    let key = context(&state, session).await.unwrap().key;
    let directory = root.path().join("auth/account-routing");
    std::fs::create_dir_all(&directory).unwrap();
    let path = directory.join(format!("{key}.json"));
    let lock = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path.with_extension("lock"))
        .unwrap();
    lock.try_lock().unwrap();
    assert!(initialize(&state, session).await.is_err());
    let mut selection = SessionModelSelection::default();
    assert!(freeze(&state, session, &mut selection).await.is_err());
    assert!(!selection.provider_context.account_routing_frozen);
    assert!(!path.exists());
    drop(lock);
    std::fs::write(&path, b"{corrupt-routing-state").unwrap();
    assert!(initialize(&state, session).await.is_err());
    assert!(freeze(&state, session, &mut selection).await.is_err());
    assert!(!selection.provider_context.account_routing_frozen);
    assert_eq!(std::fs::read(&path).unwrap(), b"{corrupt-routing-state");
}
