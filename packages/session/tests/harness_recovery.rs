//! Canonical configuration, transport release and recovery transfer; no plugins or natives.
use bcode_harness_models::*;
use bcode_session::{SessionManager, lease::SessionLeaseOwnerContext};
use bcode_session_models::external_harness::{
    ExternalHarnessLifetime, ExternalHarnessRecovery, ExternalHarnessRequest,
    ExternalHarnessTransportRelease,
};
use bcode_session_models::{ExternalHarnessClaim, ExternalHarnessOwner, SessionId};

const ARTIFACT: &str = "artifact";

fn instance() -> HarnessInstance {
    HarnessInstance {
        instance_id: "fixture".into(),
        plugin_id: "fixture".into(),
        executable: "/not-invoked".into(),
        config_namespace: "fixture".into(),
        auth: HarnessAuthBinding {
            provider_id: "fixture".into(),
            profile: "native".into(),
            credential_namespace: "fixture".into(),
            custody: HarnessAuthCustody::NativeManaged,
        },
    }
}

fn capabilities() -> HarnessCapabilities {
    let supported = HarnessCapability {
        support: HarnessSupportLevel::Supported,
        constraints: vec![],
    };
    HarnessCapabilities {
        model_switch: supported.clone(),
        permission_mode: supported.clone(),
        resume: HarnessCapability {
            support: HarnessSupportLevel::Constrained,
            constraints: vec!["exact released transport".into()],
        },
        steering: supported.clone(),
        approval: supported.clone(),
        questions: supported.clone(),
        assistance: supported.clone(),
        interrupt: supported.clone(),
        authoritative_settlement: supported.clone(),
        background_continuation: supported.clone(),
        authentication: supported,
    }
}

fn owner(session_id: SessionId, generation: u64, daemon: &str) -> ExternalHarnessOwner {
    ExternalHarnessOwner {
        session_id,
        generation,
        artifact_id: ARTIFACT.into(),
        daemon_id: daemon.into(),
    }
}

fn manager(root: &std::path::Path, daemon: &str) -> SessionManager {
    SessionManager::persistent_with_metrics_and_lease_owner(
        root,
        bcode_metrics::MetricsRegistry::disabled(),
        SessionLeaseOwnerContext {
            build_fingerprint: Some(ARTIFACT.into()),
            daemon_instance_id: Some(daemon.into()),
            ..SessionLeaseOwnerContext::default()
        },
    )
    .unwrap()
}

fn binding(owner: &ExternalHarnessOwner) -> HarnessSessionBinding {
    HarnessSessionBinding {
        schema_version: HARNESS_SCHEMA_VERSION,
        session_id: owner.session_id.to_string(),
        instance_id: "fixture".into(),
        native_session_id: "11111111-1111-4111-8111-111111111111".into(),
        native_version: "fixture-v1".into(),
        generation: owner.generation,
    }
}

fn request(request_id: &str, kind: HarnessRequestKind) -> HarnessRequest {
    HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request_id.into(),
        instance: instance(),
        kind,
    }
}

fn response(request: &HarnessRequest, kind: HarnessServiceResponseKind) -> HarnessServiceResponse {
    HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.request_id.clone(),
        operation: request.kind.operation(),
        kind,
    }
}

async fn deliver(
    sessions: &SessionManager,
    owner: &ExternalHarnessOwner,
    request: &HarnessRequest,
    kind: HarnessServiceResponseKind,
) {
    assert_eq!(
        sessions
            .claim_external_harness_request(owner, request)
            .await
            .unwrap(),
        ExternalHarnessClaim::New
    );
    sessions
        .complete_external_harness_request(owner, request, &response(request, kind))
        .await
        .unwrap();
}

/// A configured session whose native identity is confirmed by an exact Open.
async fn confirmed(root: &std::path::Path, sessions: &SessionManager) -> ExternalHarnessOwner {
    let session = sessions.create_session(None, root.into()).await.unwrap();
    let owner = owner(session.id, 1, "daemon-1");
    sessions
        .configure_external_harness(&owner, instance(), capabilities())
        .await
        .unwrap();
    let open = request(
        "open",
        HarnessRequestKind::Open(HarnessOpenRequest {
            open_id: "open".into(),
            session_id: owner.session_id.to_string(),
            generation: 1,
            working_directory: root.to_str().unwrap().into(),
            resume: None,
            parent_assistance: None,
        }),
    );
    deliver(
        sessions,
        &owner,
        &open,
        HarnessServiceResponseKind::Opened(binding(&owner)),
    )
    .await;
    owner
}

fn control(owner: &ExternalHarnessOwner, mutation_id: &str) -> HarnessControlRequest {
    HarnessControlRequest {
        target: HarnessSessionTarget::Confirmed(binding(owner)),
        mutation_id: mutation_id.into(),
    }
}

fn release(owner: &ExternalHarnessOwner, close_id: &str) -> ExternalHarnessTransportRelease {
    ExternalHarnessTransportRelease {
        close_request_id: close_id.into(),
        instance: instance(),
        binding: binding(owner),
        lifetime: ExternalHarnessLifetime {
            daemon_instance_id: owner.daemon_id.clone(),
            artifact_id: "exact-artifact".into(),
            state_location_id: "location".into(),
        },
        reap_id: "reaped".into(),
    }
}

#[tokio::test]
async fn configuration_is_claimed_completed_and_retry_fenced() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path(), "daemon-1");
    let owner = confirmed(root.path(), &sessions).await;
    let _guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    let configure_request = |setting| {
        request(
            "configure-model",
            HarnessRequestKind::Configure(HarnessConfigureRequest {
                target: HarnessSessionTarget::Confirmed(binding(&owner)),
                mutation_id: "configure-model".into(),
                setting,
            }),
        )
    };
    let configure = configure_request(HarnessSessionSetting::Model {
        model_id: "claude-fixture".into(),
    });
    deliver(
        &sessions,
        &owner,
        &configure,
        HarnessServiceResponseKind::Acknowledged {
            mutation_id: "configure-model".into(),
        },
    )
    .await;
    assert!(matches!(
        sessions
            .claim_external_harness_request(&owner, &configure)
            .await
            .unwrap(),
        ExternalHarnessClaim::Recorded(_)
    ));
    // A different setting under the same retained mutation identity is a conflict.
    let conflicting = configure_request(HarnessSessionSetting::PermissionMode {
        mode: HarnessPermissionMode::Plan,
    });
    assert!(
        sessions
            .claim_external_harness_request(&owner, &conflicting)
            .await
            .is_err()
    );
    let state = sessions.external_harness_state(&owner).await.unwrap();
    assert_eq!(
        state.target,
        Some(HarnessSessionTarget::Confirmed(binding(&owner))),
        "configuration never rebinds the native identity"
    );
}

#[tokio::test]
async fn transport_release_requires_exact_acknowledged_close_and_ends_the_owner() {
    let root = tempfile::tempdir().unwrap();
    let sessions = manager(root.path(), "daemon-1");
    let owner = confirmed(root.path(), &sessions).await;
    let guard = sessions
        .acquire_external_harness_ownership(&owner)
        .await
        .unwrap();
    // Without a retained Close acknowledgement, physical reap evidence is not release.
    assert!(
        sessions
            .release_external_harness_transport(&owner, release(&owner, "close"), |_| Ok(()))
            .await
            .is_err()
    );
    let close = request("close", HarnessRequestKind::Close(control(&owner, "close")));
    deliver(
        &sessions,
        &owner,
        &close,
        HarnessServiceResponseKind::Acknowledged {
            mutation_id: "close".into(),
        },
    )
    .await;
    // A changed binding or failed host reap proof is rejected.
    let mut foreign = release(&owner, "close");
    foreign.binding.native_session_id = "22222222-2222-4222-8222-222222222222".into();
    assert!(
        sessions
            .release_external_harness_transport(&owner, foreign, |_| Ok(()))
            .await
            .is_err()
    );
    assert!(
        sessions
            .release_external_harness_transport(&owner, release(&owner, "close"), |_| Err(
                "reap unproven".into()
            ))
            .await
            .is_err()
    );
    sessions
        .release_external_harness_transport(&owner, release(&owner, "close"), |_| Ok(()))
        .await
        .unwrap();
    let (sequence, retained) = sessions
        .session_external_harness_transport_release(&owner)
        .await
        .unwrap()
        .expect("retained release");
    assert!(sequence > 0);
    assert_eq!(retained, release(&owner, "close"));
    // A released owner can neither release again nor deliver anything further.
    assert!(
        sessions
            .release_external_harness_transport(&owner, release(&owner, "close"), |_| Ok(()))
            .await
            .is_err()
    );
    let interrupt = request(
        "late-interrupt",
        HarnessRequestKind::Interrupt(control(&owner, "late-interrupt")),
    );
    assert!(
        sessions
            .claim_external_harness_request(&owner, &interrupt)
            .await
            .is_err()
    );
    drop(guard);
}

fn recovery(
    previous: &ExternalHarnessOwner,
    next: &ExternalHarnessOwner,
    sequence: u64,
    working_directory: &str,
) -> ExternalHarnessRecovery {
    recovery_with(
        previous,
        next,
        sequence,
        working_directory,
        "close",
        "recover",
    )
}

fn recovery_with(
    previous: &ExternalHarnessOwner,
    next: &ExternalHarnessOwner,
    sequence: u64,
    working_directory: &str,
    close_id: &str,
    recover_id: &str,
) -> ExternalHarnessRecovery {
    let mut resumed = binding(previous);
    resumed.generation = next.generation;
    ExternalHarnessRecovery {
        previous_owner: previous.clone(),
        release_event_sequence: sequence,
        release: release(previous, close_id),
        capabilities: capabilities(),
        resume_request: ExternalHarnessRequest::from_request(&request(
            recover_id,
            HarnessRequestKind::Open(HarnessOpenRequest {
                open_id: recover_id.into(),
                session_id: next.session_id.to_string(),
                generation: next.generation,
                working_directory: working_directory.into(),
                resume: Some(resumed),
                parent_assistance: None,
            }),
        ))
        .unwrap(),
    }
}

fn empty_page(retained: Option<&str>) -> HarnessObservationPage {
    HarnessObservationPage {
        retention: retained.map(|page_id| HarnessRetentionPage {
            connection_id: "connection".into(),
            page_id: page_id.into(),
            observations_digest: harness_observations_digest(&[]).unwrap(),
        }),
        hydration_requests: Vec::new(),
        inventory_page: None,
        observations: Vec::new(),
        next_after: None,
        completeness: HarnessCompleteness::Complete,
        status: HarnessExecutionStatus {
            connectivity: HarnessConnectivity::Connected,
            activity: HarnessActivity::Idle,
            attention: HarnessAttention::None,
            continuation: HarnessContinuation::Available,
            completeness: HarnessCompleteness::Complete,
        },
        live: Vec::new(),
    }
}

#[tokio::test]
async fn recovery_transfers_a_released_generation_exactly_once_to_a_new_daemon() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().to_str().unwrap().to_owned();
    let previous = {
        let sessions = manager(root.path(), "daemon-1");
        let owner = confirmed(root.path(), &sessions).await;
        let guard = sessions
            .acquire_external_harness_ownership(&owner)
            .await
            .unwrap();
        // The released generation leaves a retained current page behind.
        sessions
            .commit_external_harness_page(&owner, None, &empty_page(Some("gen-1-page")))
            .await
            .unwrap();
        let close = request("close", HarnessRequestKind::Close(control(&owner, "close")));
        deliver(
            &sessions,
            &owner,
            &close,
            HarnessServiceResponseKind::Acknowledged {
                mutation_id: "close".into(),
            },
        )
        .await;
        sessions
            .release_external_harness_transport(&owner, release(&owner, "close"), |_| Ok(()))
            .await
            .unwrap();
        drop(guard);
        sessions
            .release_idle_session_resources(owner.session_id)
            .await
            .unwrap();
        owner
    };
    let sessions = manager(root.path(), "daemon-2");
    let (sequence, _) = sessions
        .session_external_harness_transport_release(&previous)
        .await
        .unwrap()
        .expect("retained release");
    let next = owner(previous.session_id, 2, "daemon-2");
    // A stale release sequence or a failed host lifetime proof never transfers ownership.
    assert!(
        sessions
            .prepare_external_harness_recovery(
                &next,
                recovery(&previous, &next, sequence + 1, &directory),
                |_| Ok(())
            )
            .await
            .is_err()
    );
    assert!(
        sessions
            .prepare_external_harness_recovery(
                &next,
                recovery(&previous, &next, sequence, &directory),
                |_| Err("old daemon lifetime is live".into())
            )
            .await
            .is_err()
    );
    sessions
        .prepare_external_harness_recovery(
            &next,
            recovery(&previous, &next, sequence, &directory),
            |_| Ok(()),
        )
        .await
        .unwrap();
    let state = sessions.external_harness_state(&next).await.unwrap();
    assert_eq!(state.owner, next);
    assert!(state.target.is_none(), "resume is not yet confirmed");
    assert_eq!(
        state.binding.as_ref().map(|value| value.generation),
        Some(2)
    );
    assert!(
        sessions.external_harness_state(&previous).await.is_err(),
        "the released generation can never be reused"
    );
    // Exactly once: the same intent cannot be prepared again.
    let skipped = owner(previous.session_id, 3, "daemon-3");
    assert!(
        sessions
            .prepare_external_harness_recovery(
                &next,
                recovery(&previous, &next, sequence, &directory),
                |_| Ok(())
            )
            .await
            .is_err()
    );
    assert!(sessions.external_harness_state(&skipped).await.is_err());
    // The retained resume intent is the only Open the new generation may claim.
    let _guard = sessions
        .acquire_external_harness_ownership(&next)
        .await
        .unwrap();
    let resume = recovery(&previous, &next, sequence, &directory)
        .resume_request
        .request()
        .unwrap();
    let mut resumed = binding(&previous);
    resumed.generation = 2;
    deliver(
        &sessions,
        &next,
        &resume,
        HarnessServiceResponseKind::Opened(resumed.clone()),
    )
    .await;
    let state = sessions.external_harness_state(&next).await.unwrap();
    let target = HarnessSessionTarget::Confirmed(resumed);
    assert_eq!(state.target.as_ref(), Some(&target));
    // The previous owner's retained page is not a predecessor of the recovered connection:
    // the first observe carries no acknowledgment, and its page commits without one.
    assert_eq!(
        sessions
            .session_harness_retention_context(&next, &target, None)
            .await
            .unwrap(),
        None
    );
    assert!(
        sessions
            .session_harness_retention_context(&next, &target, Some("stale-cursor"))
            .await
            .is_err()
    );
    sessions
        .commit_external_harness_page(&next, None, &empty_page(None))
        .await
        .unwrap();
    assert_eq!(
        sessions
            .session_harness_retention_context(&next, &target, None)
            .await
            .unwrap(),
        None
    );
}

/// Release the session's lease so another daemon can open it, waiting for dropped guards.
async fn release_session(sessions: &SessionManager, session_id: SessionId) {
    for _ in 0..100 {
        // Dropped guards release asynchronously; an already-unowned actor is also free.
        if !matches!(
            sessions
                .release_session_ownership(session_id)
                .await
                .unwrap(),
            bcode_session::SessionOwnershipRelease::Blocked(_)
        ) {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    }
    panic!("session resources were never released");
}

/// Close and release `owner` gracefully, then free the session so another daemon can recover it.
async fn close_and_release(
    sessions: &SessionManager,
    owner: &ExternalHarnessOwner,
    close_id: &str,
) {
    let close = request(
        close_id,
        HarnessRequestKind::Close(control(owner, close_id)),
    );
    deliver(
        sessions,
        owner,
        &close,
        HarnessServiceResponseKind::Acknowledged {
            mutation_id: close_id.into(),
        },
    )
    .await;
    sessions
        .release_external_harness_transport(owner, release(owner, close_id), |_| Ok(()))
        .await
        .unwrap();
}

/// Recover `previous` into `next` and confirm the resume Open, returning the new target.
async fn recover_and_resume(
    sessions: &SessionManager,
    previous: &ExternalHarnessOwner,
    next: &ExternalHarnessOwner,
    directory: &str,
    close_id: &str,
    recover_id: &str,
) -> HarnessSessionTarget {
    let (sequence, _) = sessions
        .session_external_harness_transport_release(previous)
        .await
        .unwrap()
        .expect("retained release");
    let prepared = recovery_with(previous, next, sequence, directory, close_id, recover_id);
    sessions
        .prepare_external_harness_recovery(next, prepared.clone(), |_| Ok(()))
        .await
        .unwrap();
    let resume = prepared.resume_request.request().unwrap();
    let mut resumed = binding(previous);
    resumed.generation = next.generation;
    deliver(
        sessions,
        next,
        &resume,
        HarnessServiceResponseKind::Opened(resumed.clone()),
    )
    .await;
    HarnessSessionTarget::Confirmed(resumed)
}

#[tokio::test]
async fn consecutive_recoveries_without_an_intervening_page_keep_the_session_usable() {
    let root = tempfile::tempdir().unwrap();
    let directory = root.path().to_str().unwrap().to_owned();
    let first = {
        let sessions = manager(root.path(), "daemon-1");
        let owner = confirmed(root.path(), &sessions).await;
        let guard = sessions
            .acquire_external_harness_ownership(&owner)
            .await
            .unwrap();
        // Generation 1 owns the current page locator.
        sessions
            .commit_external_harness_page(&owner, None, &empty_page(Some("gen-1-page")))
            .await
            .unwrap();
        close_and_release(&sessions, &owner, "close-1").await;
        drop(guard);
        release_session(&sessions, owner.session_id).await;
        owner
    };
    // Generation 2 resumes and is released again before it commits any page.
    let second = owner(first.session_id, 2, "daemon-2");
    {
        let sessions = manager(root.path(), "daemon-2");
        let target = recover_and_resume(
            &sessions,
            &first,
            &second,
            &directory,
            "close-1",
            "recover-2",
        )
        .await;
        let guard = sessions
            .acquire_external_harness_ownership(&second)
            .await
            .unwrap();
        assert_eq!(
            sessions
                .external_harness_state(&second)
                .await
                .unwrap()
                .target,
            Some(target)
        );
        close_and_release(&sessions, &second, "close-2").await;
        drop(guard);
        release_session(&sessions, second.session_id).await;
    }
    // Generation 3 still finds generation 1's page as released history, not corruption.
    let third = owner(first.session_id, 3, "daemon-3");
    let sessions = manager(root.path(), "daemon-3");
    let target = recover_and_resume(
        &sessions,
        &second,
        &third,
        &directory,
        "close-2",
        "recover-3",
    )
    .await;
    let _guard = sessions
        .acquire_external_harness_ownership(&third)
        .await
        .unwrap();
    let state = sessions.external_harness_state(&third).await.unwrap();
    assert_eq!(state.target.as_ref(), Some(&target));
    assert_eq!(state.observation_after, None);
    assert_eq!(
        sessions
            .session_harness_retention_context(&third, &target, None)
            .await
            .unwrap(),
        None
    );
    sessions
        .commit_external_harness_page(&third, None, &empty_page(None))
        .await
        .unwrap();
    assert!(sessions.external_harness_state(&third).await.is_ok());
}
