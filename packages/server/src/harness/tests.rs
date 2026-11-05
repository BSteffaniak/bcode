//! Behavioral application tests. Test-local state is not the production persistence adapter.

use super::{
    DeliveryClaim, HarnessOwner, ManagedHarnessError, ManagedHarnessRuntime, ManagedHarnessStore,
    ManagedHarnessTransport, ManagedSession,
};
use bcode_harness_models::*;
use bcode_session_models::SessionId;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

#[derive(Clone)]
struct Store {
    owner: HarnessOwner,
    prepared: bool,
    interrupt_support: HarnessSupportLevel,
    revocations: Arc<AtomicUsize>,
    claims: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
    completed: Arc<Mutex<std::collections::BTreeMap<String, Vec<u8>>>>,
}

impl Store {
    fn new() -> Self {
        Self {
            owner: HarnessOwner {
                session_id: SessionId::new(),
                generation: 1,
                artifact_id: "artifact".into(),
                daemon_id: "daemon".into(),
            },
            prepared: false,
            interrupt_support: HarnessSupportLevel::Supported,
            revocations: Arc::default(),
            claims: Arc::default(),
            completed: Arc::default(),
        }
    }
    fn verify(&self, owner: &HarnessOwner) -> Result<(), ManagedHarnessError> {
        if owner == &self.owner {
            Ok(())
        } else {
            Err(ManagedHarnessError::StaleOwner)
        }
    }
    fn binding(&self) -> HarnessSessionBinding {
        HarnessSessionBinding {
            schema_version: HARNESS_SCHEMA_VERSION,
            session_id: self.owner.session_id.to_string(),
            instance_id: "instance".into(),
            native_session_id: "native".into(),
            native_version: "1".into(),
            generation: 1,
        }
    }
}

impl Store {
    fn assistance_guard_result(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<Box<dyn Send + Sync>, ManagedHarnessError> {
        self.verify(owner)?;
        if matches!(&request.kind, HarnessRequestKind::Open(open) if open.parent_assistance.is_some())
            || matches!(&request.kind, HarnessRequestKind::Respond { response, .. }
                if matches!(&response.value, HarnessResponseValue::Assistance { .. }))
        {
            return Err(ManagedHarnessError::Unsupported);
        }
        if matches!(
            &request.kind,
            HarnessRequestKind::Interrupt(_) | HarnessRequestKind::Close(_)
        ) {
            self.revocations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(Box::new(()) as Box<dyn Send + Sync>)
    }

    fn load_result(&self, owner: &HarnessOwner) -> Result<ManagedSession, ManagedHarnessError> {
        self.verify(owner)?;
        let cap = HarnessCapability {
            support: HarnessSupportLevel::Supported,
            constraints: vec![],
        };
        Ok(ManagedSession {
            instance: HarnessInstance {
                instance_id: "instance".into(),
                plugin_id: "plugin".into(),
                executable: "/native".into(),
                config_namespace: "/config".into(),
                auth: HarnessAuthBinding {
                    provider_id: "auth".into(),
                    profile: "profile".into(),
                    credential_namespace: "account".into(),
                    custody: HarnessAuthCustody::NativeManaged,
                },
            },
            binding: (!self.prepared).then(|| self.binding()),
            target: Some(if self.prepared {
                HarnessSessionTarget::Prepared(HarnessPreparedSession {
                    open_id: "retained-open".into(),
                    session_id: self.owner.session_id.to_string(),
                    instance_id: "instance".into(),
                    generation: self.owner.generation,
                    native_version: "1".into(),
                    requested_native_session_id: None,
                })
            } else {
                HarnessSessionTarget::Confirmed(self.binding())
            }),
            capabilities: HarnessCapabilities {
                resume: cap.clone(),
                steering: cap.clone(),
                approval: cap.clone(),
                questions: cap.clone(),
                assistance: cap.clone(),
                interrupt: HarnessCapability {
                    support: self.interrupt_support,
                    constraints: vec![],
                },
                authoritative_settlement: cap.clone(),
                background_continuation: cap.clone(),
                authentication: cap,
            },
        })
    }

    fn claim_result(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> Result<DeliveryClaim, ManagedHarnessError> {
        self.verify(owner)?;
        let bytes = serde_json::to_vec(request).unwrap();
        let prior = {
            let mut claims = self.claims.lock().unwrap();
            let prior = claims.get(&request.request_id).cloned();
            if prior.is_none() {
                claims.insert(request.request_id.clone(), bytes.clone());
            }
            drop(claims);
            prior
        };
        if let Some(prior) = prior {
            return if prior == bytes {
                self.completed
                    .lock()
                    .unwrap()
                    .get(&request.request_id)
                    .map_or_else(
                        || Ok(DeliveryClaim::Uncertain),
                        |recorded| {
                            Ok(DeliveryClaim::Recorded(Box::new(
                                serde_json::from_slice(recorded).unwrap(),
                            )))
                        },
                    )
            } else {
                Err(ManagedHarnessError::Conflict)
            };
        }
        Ok(DeliveryClaim::New)
    }
}

impl ManagedHarnessStore for Store {
    type OwnershipGuard = ();
    fn acquire(
        &self,
        owner: &HarnessOwner,
    ) -> impl std::future::Future<Output = Result<(), ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.verify(owner)))
    }
    fn assistance_guard(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> impl std::future::Future<Output = Result<Box<dyn Send + Sync>, ManagedHarnessError>> + Send
    {
        std::future::poll_fn(move |_| {
            std::task::Poll::Ready(self.assistance_guard_result(owner, request))
        })
    }
    fn load(
        &self,
        owner: &HarnessOwner,
    ) -> impl std::future::Future<Output = Result<ManagedSession, ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.load_result(owner)))
    }
    fn claim(
        &self,
        owner: &HarnessOwner,
        request: &HarnessRequest,
    ) -> impl std::future::Future<Output = Result<DeliveryClaim, ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.claim_result(owner, request)))
    }
    fn commit(
        &self,
        owner: &HarnessOwner,
        _: &HarnessRequest,
        _: &HarnessServiceResponse,
    ) -> impl std::future::Future<Output = Result<(), ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.verify(owner)))
    }
    fn uncertain(
        &self,
        owner: &HarnessOwner,
        _: &HarnessRequest,
    ) -> impl std::future::Future<Output = Result<(), ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.verify(owner)))
    }
    fn interaction(
        &self,
        _: &HarnessOwner,
        _: &str,
    ) -> impl std::future::Future<Output = Result<HarnessInteraction, ManagedHarnessError>> + Send
    {
        std::future::poll_fn(move |_| std::task::Poll::Ready(Err(ManagedHarnessError::Conflict)))
    }
    fn receipt(
        &self,
        _: &HarnessOwner,
        _: &str,
        _: &str,
    ) -> impl std::future::Future<Output = Result<Option<HarnessReceipt>, ManagedHarnessError>> + Send
    {
        std::future::poll_fn(move |_| std::task::Poll::Ready(Ok(None)))
    }
    fn observations(
        &self,
        owner: &HarnessOwner,
        _: &HarnessObservationPage,
    ) -> impl std::future::Future<Output = Result<(), ManagedHarnessError>> + Send {
        std::future::poll_fn(move |_| std::task::Poll::Ready(self.verify(owner)))
    }
    fn history(
        &self,
        _: SessionId,
        _: Option<String>,
        _: u16,
    ) -> impl std::future::Future<Output = Result<HarnessObservationPage, ManagedHarnessError>> + Send
    {
        std::future::poll_fn(move |_| {
            std::task::Poll::Ready(Ok(HarnessObservationPage {
                observations: vec![],
                next_after: None,
                completeness: HarnessCompleteness::Partial,
                status: HarnessExecutionStatus {
                    connectivity: HarnessConnectivity::Unknown,
                    activity: HarnessActivity::Unknown,
                    attention: HarnessAttention::Unknown,
                    continuation: HarnessContinuation::Unknown,
                    completeness: HarnessCompleteness::Partial,
                },
            }))
        })
    }
}

#[derive(Clone, Default)]
struct LostAck(Arc<AtomicUsize>);
impl ManagedHarnessTransport for LostAck {
    fn invoke(
        &self,
        _: &HarnessRequest,
    ) -> impl std::future::Future<Output = Result<HarnessServiceResponse, ManagedHarnessError>> + Send
    {
        std::future::poll_fn(move |_| {
            self.0.fetch_add(1, Ordering::SeqCst);
            std::task::Poll::Ready(Err(ManagedHarnessError::Unavailable))
        })
    }
}

fn input(owner: &HarnessOwner) -> HarnessSubmission {
    HarnessSubmission {
        submission_id: "retained".into(),
        attempt_id: "attempt".into(),
        session_id: owner.session_id.to_string(),
        generation: owner.generation,
        text: "task".into(),
        placement: HarnessPlacement::NextTurn,
    }
}

#[tokio::test]
async fn resolved_response_retry_returns_retained_ack_without_native_delivery() {
    let store = Store::new();
    let owner = store.owner.clone();
    let response = HarnessResponse {
        response_id: "answer".into(),
        request_id: "question".into(),
        session_id: owner.session_id.to_string(),
        generation: owner.generation,
        value: HarnessResponseValue::Answer {
            choice_id: Some("0".into()),
            text: String::new(),
        },
    };
    let selected = store.load(&owner).await.unwrap();
    let request = HarnessRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "answer".into(),
        instance: selected.instance,
        kind: HarnessRequestKind::Respond {
            target: selected.target.unwrap(),
            response,
        },
    };
    store
        .claims
        .lock()
        .unwrap()
        .insert("answer".into(), serde_json::to_vec(&request).unwrap());
    let ack = HarnessServiceResponse {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: "answer".into(),
        operation: HarnessOperation::Respond,
        kind: HarnessServiceResponseKind::Acknowledged {
            mutation_id: "answer".into(),
        },
    };
    store
        .completed
        .lock()
        .unwrap()
        .insert("answer".into(), serde_json::to_vec(&ack).unwrap());
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store.clone(), native.clone());
    let HarnessRequestKind::Respond { mut response, .. } = request.kind else {
        panic!("fixture")
    };
    // Test store has no pending interaction: only exact canonical evidence may satisfy this retry.
    let retained = runtime.respond(&owner, response.clone()).await.unwrap();
    assert!(matches!(
        retained.kind,
        HarnessServiceResponseKind::Acknowledged { .. }
    ));
    let mut stale = owner.clone();
    stale.generation += 1;
    assert_eq!(
        runtime.respond(&stale, response.clone()).await.unwrap_err(),
        ManagedHarnessError::StaleOwner
    );
    store.completed.lock().unwrap().clear();
    assert_eq!(
        runtime.respond(&owner, response.clone()).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    response.value = HarnessResponseValue::Answer {
        choice_id: Some("1".into()),
        text: String::new(),
    };
    assert_eq!(
        runtime.respond(&owner, response).await.unwrap_err(),
        ManagedHarnessError::Conflict
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prepared_owner_comes_from_durable_state_without_inventing_a_native_binding() {
    use bcode_session_models::external_harness::{ExternalHarnessOwner, ExternalHarnessState};
    let mut store = Store::new();
    store.prepared = true;
    let owner = store.owner.clone();
    let selected = store.load(&owner).await.unwrap();
    let mut snapshot = ExternalHarnessState {
        owner: ExternalHarnessOwner {
            session_id: owner.session_id,
            generation: owner.generation,
            artifact_id: owner.artifact_id.clone(),
            daemon_id: owner.daemon_id.clone(),
        },
        instance: selected.instance,
        capabilities: selected.capabilities,
        binding: None,
        target: selected.target,
        observation_after: None,
        prepared_open: None,
    };
    assert_eq!(
        super::CanonicalHarnessStore::owner_from_snapshot(owner.session_id, &snapshot).unwrap(),
        owner
    );
    snapshot.owner.daemon_id = "foreign-daemon".into();
    // Discovery preserves the actual durable owner instead of adopting the serving daemon.
    assert_eq!(
        super::CanonicalHarnessStore::owner_from_snapshot(owner.session_id, &snapshot)
            .unwrap()
            .daemon_id,
        "foreign-daemon"
    );
    snapshot.binding = Some(store.binding());
    assert_eq!(
        super::CanonicalHarnessStore::owner_from_snapshot(owner.session_id, &snapshot).unwrap_err(),
        ManagedHarnessError::Conflict
    );
}

#[tokio::test]
async fn prepared_target_cannot_be_resumed_with_an_invented_binding() {
    let mut store = Store::new();
    store.prepared = true;
    let owner = store.owner.clone();
    let fake_resume = store.binding();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    let open = HarnessOpenRequest {
        open_id: "resume".into(),
        session_id: owner.session_id.to_string(),
        generation: owner.generation,
        working_directory: "/workspace".into(),
        resume: Some(fake_resume),
        parent_assistance: None,
    };
    assert_eq!(
        runtime.open(&owner, open).await.unwrap_err(),
        ManagedHarnessError::Conflict
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn prepared_transport_can_deliver_first_input_without_a_fake_native_binding() {
    let mut store = Store::new();
    store.prepared = true;
    let owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store.clone(), native.clone());
    assert!(store.load(&owner).await.unwrap().binding.is_none());
    assert_eq!(
        runtime.submit(&owner, input(&owner)).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
    drop(runtime);
    // Recreating the host runtime is not permission to recreate the lost prepared process.
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    assert_eq!(
        runtime.submit(&owner, input(&owner)).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn lost_ack_and_runtime_recreation_never_repeat_mutation() {
    let store = Store::new();
    let owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store.clone(), native.clone());
    assert_eq!(
        runtime.submit(&owner, input(&owner)).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    drop(runtime);
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    assert_eq!(
        runtime.submit(&owner, input(&owner)).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
    let mut conflicting = input(&owner);
    conflicting.text = "different task".into();
    assert_eq!(
        runtime.submit(&owner, conflicting).await.unwrap_err(),
        ManagedHarnessError::Conflict
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn stale_owner_and_generation_do_not_reach_native_transport() {
    let store = Store::new();
    let mut owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    owner.generation = 2;
    assert_eq!(
        runtime.submit(&owner, input(&owner)).await.unwrap_err(),
        ManagedHarnessError::StaleOwner
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn unsupported_interrupt_still_revokes_assistance_without_claim_or_native_ack() {
    let mut store = Store::new();
    store.interrupt_support = HarnessSupportLevel::Unsupported;
    let owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store.clone(), native.clone());
    assert_eq!(
        runtime.cancel(&owner, "cancel".into()).await.unwrap_err(),
        ManagedHarnessError::Unsupported
    );
    assert_eq!(store.revocations.load(Ordering::SeqCst), 1);
    assert!(store.claims.lock().unwrap().is_empty());
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
    let mut stale = owner;
    stale.generation += 1;
    assert_eq!(
        runtime
            .cancel(&stale, "cancel-stale".into())
            .await
            .unwrap_err(),
        ManagedHarnessError::StaleOwner
    );
    assert_eq!(store.revocations.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn interruption_uncertainty_does_not_become_settlement_or_repeat_signal() {
    let store = Store::new();
    let owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    assert_eq!(
        runtime.cancel(&owner, "cancel".into()).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    assert_eq!(
        runtime.cancel(&owner, "cancel".into()).await.unwrap_err(),
        ManagedHarnessError::Uncertain
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn offline_history_and_invalid_bounds_never_contact_native() {
    let store = Store::new();
    let owner = store.owner.clone();
    let native = LostAck::default();
    let runtime = ManagedHarnessRuntime::new(store, native.clone());
    assert!(runtime.history(owner.session_id, None, 8).await.is_ok());
    assert_eq!(
        runtime
            .history(owner.session_id, None, 0)
            .await
            .unwrap_err(),
        ManagedHarnessError::Invalid
    );
    assert_eq!(
        runtime
            .history(owner.session_id, None, 129)
            .await
            .unwrap_err(),
        ManagedHarnessError::Invalid
    );
    assert_eq!(native.0.load(Ordering::SeqCst), 0);
}
