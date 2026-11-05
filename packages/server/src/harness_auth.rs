//! Daemon-owned managed auth application adapter. Module/state/IPC registration is
//! coordinator-owned. No payload in this module belongs in session event history.

use std::time::Instant;

use bcode_provider_auth::managed::{
    AuthStepTicket, AuthWaiter, CoordinationError, JoinAuthFlow, ManagedAuthCommand,
    ManagedAuthCoordinator, ManagedAuthStep, ManagedFlowSnapshot,
};
use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthReadiness, AuthSensitiveInput, ManagedAuthHandle,
    SensitiveAuthInteraction,
};
use tokio::sync::Mutex;

/// Canonical facts resolved by existing session/harness admission, never IPC input.
///
/// Construction alone grants no permission: `HarnessAuthAuthority` must revalidate
/// current ownership and plugin bindings on every client operation.
pub struct VerifiedHarnessAuthScope {
    /// Current authorized session/generation/ownership fence.
    pub waiter: AuthWaiter,
    /// Actual credential namespace from the admitted instance binding.
    pub namespace: AuthNamespace,
    /// Explicit configured custody; ambient overrides are rejected by admission.
    pub custody: AuthCustodyMode,
    /// Enabled plugin's supported native method.
    pub method: String,
}

/// Implement using the canonical managed-session application boundary. Parentage,
/// client-supplied generation and display profile names must not establish authority.
pub trait HarnessAuthAuthority: Sync {
    /// Authorize the authenticated caller, current session ownership, instance binding,
    /// custody and native login capability. Reject disabled plugins/unsupported methods.
    /// Before a new begin after daemon startup, read-only native inspection must verify
    /// that the namespace has no unresolved prior login; do not infer inactivity from
    /// an empty in-memory registry. These checks grant no credential enumeration.
    /// # Errors
    /// Fails closed for missing permission, stale ownership or invalid bindings.
    fn resolve(
        &self,
        authenticated_caller: &str,
        session_id: &str,
        instance_id: &str,
    ) -> Result<VerifiedHarnessAuthScope, CoordinationError>;
}

/// Auth operation stage for secret-free error attribution.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TerminalAuthStage {
    /// Namespace reservation/preparation admission.
    Prepare,
    /// One-shot plan claim issuance.
    Claim,
    /// Exact client-child reap acknowledgment.
    Reap,
    /// Readiness continuation after reap.
    Continue,
}

/// Typed internal failure facts; never contains URLs, codes, input or native output.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalAuthFailure {
    /// Failed application stage, not inferred from display text.
    pub stage: TerminalAuthStage,
    /// Sanitized coordination category.
    pub cause: CoordinationError,
    /// Current authorized flow/claim state, when safely available.
    pub current: Option<bcode_provider_auth::managed::ClientTerminalDiagnostic>,
}

/// Correlation and sanitized failure for one terminal diagnostic lookup.
pub struct TerminalAuthFailureRequest<'a> {
    /// Exact live flow; never execution authority.
    pub handle: &'a ManagedAuthHandle,
    /// Operation that failed.
    pub stage: TerminalAuthStage,
    /// Sanitized domain rejection.
    pub cause: CoordinationError,
}

/// Exact prepared interaction and host-generated one-shot launch claim.
pub struct ClientTerminalClaimRequest<'a> {
    /// Live flow identity, not execution authority.
    pub handle: &'a ManagedAuthHandle,
    /// Exact pending terminal interaction.
    pub interaction_id: &'a str,
    /// Retained host claim identity, never reused for a second launch.
    pub claim_id: String,
}

/// One-shot bridge to the shared harness service contract. It retains only a
/// secret-free validation envelope after the outgoing request is taken.
pub struct HarnessAuthInvocation {
    issued: ManagedAuthCommand,
    validation: bcode_harness_models::HarnessRequest,
    outgoing: Option<bcode_harness_models::HarnessRequest>,
}

impl HarnessAuthInvocation {
    /// Convert a daemon-issued command to a plugin service request exactly once.
    /// Caller must mark the ticket uncertain if conversion/dispatch fails after issue.
    /// Native harness auth retains protocol state by boot-scoped flow ID; opaque
    /// enrollment state is not smuggled into the shared native protocol.
    /// # Errors
    /// Rejects wrong instance ownership/custody, unsupported state, or invalid bounds.
    pub fn prepare(
        mut issued: ManagedAuthCommand,
        instance: bcode_harness_models::HarnessInstance,
    ) -> Result<Self, CoordinationError> {
        use bcode_harness_models::{
            HARNESS_SCHEMA_VERSION, HarnessAuthRequest, HarnessRequest, HarnessRequestKind,
        };
        use bcode_provider_auth::managed::ManagedAuthAction;
        if instance.plugin_id != issued.namespace.plugin_id
            || instance.instance_id != issued.native_owner
            || instance.auth.credential_namespace != issued.namespace.credential_namespace
            || instance.auth.custody != issued.custody
            || issued.state.is_some()
        {
            return Err(CoordinationError::Conflict);
        }
        let flow_id = format!(
            "{}:{}",
            issued.ticket.handle.daemon_epoch, issued.ticket.handle.flow_id
        );
        let request_id = format!("{flow_id}:{}", issued.ticket.step);
        let action = std::mem::replace(&mut issued.action, ManagedAuthAction::Continue(None));
        let (kind, validation_kind) = match action {
            ManagedAuthAction::Begin => {
                let value = HarnessAuthRequest {
                    flow_id: flow_id.clone(),
                    method_id: issued.method.clone(),
                    input: None,
                };
                let validation = HarnessAuthRequest {
                    flow_id,
                    method_id: issued.method.clone(),
                    input: None,
                };
                (
                    HarnessRequestKind::AuthBegin(value),
                    HarnessRequestKind::AuthBegin(validation),
                )
            }
            ManagedAuthAction::Continue(input) => {
                let value = HarnessAuthRequest {
                    flow_id: flow_id.clone(),
                    method_id: issued.method.clone(),
                    input,
                };
                let validation = HarnessAuthRequest {
                    flow_id,
                    method_id: issued.method.clone(),
                    input: None,
                };
                (
                    HarnessRequestKind::AuthContinue(value),
                    HarnessRequestKind::AuthContinue(validation),
                )
            }
            ManagedAuthAction::Cancel => {
                let value = HarnessAuthRequest {
                    flow_id: flow_id.clone(),
                    method_id: issued.method.clone(),
                    input: None,
                };
                let validation = HarnessAuthRequest {
                    flow_id,
                    method_id: issued.method.clone(),
                    input: None,
                };
                (
                    HarnessRequestKind::AuthCancel(value),
                    HarnessRequestKind::AuthCancel(validation),
                )
            }
        };
        let outgoing = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request_id.clone(),
            instance: instance.clone(),
            kind,
        };
        outgoing
            .validate()
            .map_err(|_| CoordinationError::Invalid)?;
        let validation = HarnessRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id,
            instance,
            kind: validation_kind,
        };
        Ok(Self {
            issued,
            validation,
            outgoing: Some(outgoing),
        })
    }

    /// Consume the wire request for direct scoped, trace-exempt plugin dispatch.
    /// `None` means already taken, never permission to reconstruct or retry it.
    pub const fn take_request(&mut self) -> Option<bcode_harness_models::HarnessRequest> {
        self.outgoing.take()
    }
}

/// A single daemon-lifetime registry shared across all managed session entry points.
/// It survives frontend disconnect, but is deliberately not persisted across restart.
pub struct HarnessAuthService {
    coordinator: Mutex<ManagedAuthCoordinator>,
}

impl std::fmt::Debug for HarnessAuthService {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("HarnessAuthService { <redacted> }")
    }
}

impl HarnessAuthService {
    /// Install a bounded auth registry created with this daemon's unique boot epoch.
    #[must_use]
    pub const fn new(coordinator: ManagedAuthCoordinator) -> Self {
        Self {
            coordinator: Mutex::const_new(coordinator),
        }
    }

    /// Begin or share native login after current application authorization.
    /// Returned commands must go directly to the scoped plugin, with tracing disabled.
    /// # Errors
    /// Rejects unauthorized/stale callers and bounded coordination failures.
    pub async fn begin(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        coordinator.begin_or_join_owned(
            scope.namespace,
            scope.custody,
            scope.method,
            instance_id.to_owned(),
            scope.waiter,
            Instant::now(),
        )
    }

    /// Prepare a supported native-managed client-terminal flow without asserting
    /// native inactivity. Admission must verify the plugin's typed `ClientTerminal` mode
    /// and retain exclusive namespace/issued-claim fencing across daemon replacement.
    /// Returned Begin is preparation only: it must not launch native login.
    /// # Errors
    /// Rejects wrong custody, incompatible namespace owner, uncertainty or capacity.
    pub async fn begin_client_terminal(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        if scope.custody != AuthCustodyMode::NativeManaged {
            return Err(CoordinationError::Conflict);
        }
        coordinator.begin_client_terminal(
            scope.namespace,
            scope.method,
            instance_id.to_owned(),
            scope.waiter,
            Instant::now(),
        )
    }

    /// Claim exactly one terminal launch after verifying the plugin plan and before
    /// returning it. Persist the namespace claim before exposing any executable plan.
    /// # Errors
    /// Rejects stale authority, duplicate claims and nonterminal-preparation flows.
    pub async fn claim_client_terminal(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        request: ClientTerminalClaimRequest<'_>,
    ) -> Result<bcode_provider_auth::managed::ClientTerminalClaim, CoordinationError> {
        let ClientTerminalClaimRequest {
            handle,
            interaction_id,
            claim_id,
        } = request;
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        coordinator.expire(Instant::now());
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        coordinator.claim_client_terminal(
            handle,
            &scope.waiter,
            interaction_id,
            caller.to_owned(),
            claim_id,
            Instant::now(),
        )
    }

    /// Attribute a coordination failure to an explicit stage without changing live
    /// state. Canonical scope and exact native binding are checked before reading any
    /// diagnostic facts. Failure to reauthorize omits current state, never widens access.
    pub async fn terminal_failure(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        request: TerminalAuthFailureRequest<'_>,
    ) -> TerminalAuthFailure {
        let TerminalAuthFailureRequest {
            handle,
            stage,
            cause,
        } = request;
        let coordinator = self.coordinator.lock().await;
        let current = authority
            .resolve(caller, session_id, instance_id)
            .ok()
            .and_then(|scope| {
                Self::check_client_binding(&coordinator, handle, &scope, instance_id).ok()?;
                coordinator
                    .client_terminal_diagnostic(handle, caller, &scope.waiter)
                    .ok()
            });
        TerminalAuthFailure {
            stage,
            cause,
            current,
        }
    }

    /// Recover only the retained claim identity, never a replayable terminal plan.
    /// Daemon orchestration uses this after bounded lifecycle discovery to contact
    /// the original authenticated child owner for reap evidence.
    /// # Errors
    /// Rejects unknown boot/flow identity.
    pub async fn client_terminal_claim(
        &self,
        handle: &ManagedAuthHandle,
    ) -> Result<Option<bcode_provider_auth::managed::ClientTerminalClaim>, CoordinationError> {
        self.coordinator
            .lock()
            .await
            .client_terminal_claim(handle)
            .map(<Option<&bcode_provider_auth::managed::ClientTerminalClaim>>::cloned)
    }

    /// Record verified reap/no-launch evidence from the exact client process owner.
    /// Call only on the dedicated authenticated child-lifecycle route, not from an
    /// arbitrary client boolean. Disconnect or native status is never reap evidence.
    /// # Errors
    /// Rejects another client or mismatched live claim. Does not assert auth readiness.
    pub async fn client_terminal_reaped(
        &self,
        authenticated_client: &str,
        claim: &bcode_provider_auth::managed::ClientTerminalClaim,
    ) -> Result<(), CoordinationError> {
        if authenticated_client != claim.client_id {
            return Err(CoordinationError::Stale);
        }
        self.coordinator.lock().await.client_terminal_reaped(claim)
    }

    fn check_client_binding(
        coordinator: &ManagedAuthCoordinator,
        handle: &ManagedAuthHandle,
        scope: &VerifiedHarnessAuthScope,
        instance_id: &str,
    ) -> Result<(), CoordinationError> {
        coordinator.check_native_owner(handle, instance_id)?;
        coordinator.check_binding(handle, &scope.namespace, scope.custody, &scope.method)
    }

    /// Read live status on reconnect; restart requires fresh native inspection.
    /// # Errors
    /// Rejects stale boot/generation/ownership and nonmember callers.
    pub async fn status(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        handle: &ManagedAuthHandle,
    ) -> Result<ManagedFlowSnapshot, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        coordinator.expire(Instant::now());
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        coordinator.snapshot(handle, &scope.waiter)
    }

    /// Read callback material exclusively into the dedicated sensitive auth transport.
    /// MUST bypass normal IPC request/result logs, session events, telemetry and models.
    /// # Errors
    /// Rejects stale/unauthorized callers. Never exposes another session's live flow.
    pub async fn sensitive_interaction(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        handle: &ManagedAuthHandle,
    ) -> Result<Option<(u64, SensitiveAuthInteraction)>, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        coordinator.expire(Instant::now());
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        let revision = coordinator.snapshot(handle, &scope.waiter)?.revision;
        coordinator
            .interaction(handle, &scope.waiter)
            .map(|effect| effect.cloned().map(|effect| (revision, effect)))
    }

    /// Consume hidden callback/code input exactly once; never turn it into chat text.
    /// # Errors
    /// Rejects mismatched requests, stale callers and ambiguous/in-flight operations.
    pub async fn continue_flow(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        handle: &ManagedAuthHandle,
        input: Option<AuthSensitiveInput>,
    ) -> Result<ManagedAuthCommand, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        coordinator.continue_flow(handle, &scope.waiter, input, Instant::now())
    }

    /// Detach only this authorized waiter; daemon drives any returned native cancellation.
    /// A closed client socket must not invoke this method implicitly.
    /// # Errors
    /// Rejects stale/unauthorized callers and mismatched instance bindings.
    pub async fn cancel(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        handle: &ManagedAuthHandle,
    ) -> Result<Option<ManagedAuthCommand>, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        coordinator.expire(Instant::now());
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        coordinator.cancel_waiter(handle, &scope.waiter)
    }

    /// Deliver a response only from the authenticated plugin route for this exact
    /// issued command. The command is daemon-retained, never reconstructed from IPC.
    /// Returns deferred last-waiter cleanup for immediate daemon dispatch, if needed.
    /// # Errors
    /// Rejects wrong plugin owner, stale response and invalid step shape.
    pub async fn accept_plugin_response(
        &self,
        authenticated_plugin_id: &str,
        issued: &ManagedAuthCommand,
        response: ManagedAuthStep,
    ) -> Result<(ManagedFlowSnapshot, Option<ManagedAuthCommand>), CoordinationError> {
        if authenticated_plugin_id != issued.namespace.plugin_id {
            return Err(CoordinationError::Stale);
        }
        let mut coordinator = self.coordinator.lock().await;
        coordinator.check_native_owner(&issued.ticket.handle, &issued.native_owner)?;
        coordinator.check_binding(
            &issued.ticket.handle,
            &issued.namespace,
            issued.custody,
            &issued.method,
        )?;
        let mut snapshot = coordinator.accept(&issued.ticket, response, Instant::now())?;
        let cancel = coordinator.take_cancel(&issued.ticket.handle)?;
        drop(coordinator);
        if let Some(command) = &cancel {
            snapshot.phase = bcode_provider_auth::managed::ManagedFlowPhase::Cancelling;
            snapshot.revision = command.ticket.step.saturating_mul(4);
        }
        Ok((snapshot, cancel))
    }

    /// Consume a shared harness response after exact correlation and namespace checks.
    /// Native auth state remains in the plugin's live flow, never in durable history.
    /// # Errors
    /// Rejects wrong owner/correlation, already-unresolved delivery and malformed output.
    /// A service failure is conservatively uncertain, never an implicit login retry.
    pub async fn accept_harness_response(
        &self,
        authenticated_plugin_id: &str,
        invocation: &HarnessAuthInvocation,
        response: bcode_harness_models::HarnessServiceResponse,
    ) -> Result<(ManagedFlowSnapshot, Option<ManagedAuthCommand>), CoordinationError> {
        if invocation.outgoing.is_some()
            || authenticated_plugin_id != invocation.issued.namespace.plugin_id
        {
            return Err(CoordinationError::Stale);
        }
        if response.validate_for(&invocation.validation).is_err() {
            self.delivery_uncertain(&invocation.issued.ticket).await?;
            return Err(CoordinationError::Invalid);
        }
        let bcode_harness_models::HarnessServiceResponseKind::AuthStep(step) = response.kind else {
            self.delivery_uncertain(&invocation.issued.ticket).await?;
            return Err(CoordinationError::Busy);
        };
        // The current wire AuthStep has no positive native-inactive proof. A cancel
        // acknowledgment alone cannot authorize releasing the namespace reservation.
        let cancel_without_settlement_proof = matches!(
            invocation.validation.kind,
            bcode_harness_models::HarnessRequestKind::AuthCancel(_)
        );
        self.accept_plugin_response(
            authenticated_plugin_id,
            &invocation.issued,
            ManagedAuthStep {
                terminal: step.complete && !cancel_without_settlement_proof,
                readiness: step.status.readiness,
                state: None,
                interaction: step.interaction,
            },
        )
        .await
    }

    /// Lost delivery/response is uncertainty, not retry authorization.
    /// # Errors
    /// Rejects stale tickets and stable terminal outcomes.
    pub async fn delivery_uncertain(
        &self,
        ticket: &AuthStepTicket,
    ) -> Result<(), CoordinationError> {
        self.coordinator.lock().await.mark_uncertain(ticket)
    }

    /// Read-only native inspection must prove the prior auth process is inactive.
    /// This is a daemon-internal route, not a client-provided boolean.
    /// # Errors
    /// Rejects wrong owner/binding and flows not awaiting reconciliation.
    pub async fn reconciled_inactive(
        &self,
        authenticated_plugin_id: &str,
        issued: &ManagedAuthCommand,
        readiness: AuthReadiness,
    ) -> Result<(), CoordinationError> {
        if authenticated_plugin_id != issued.namespace.plugin_id {
            return Err(CoordinationError::Stale);
        }
        let mut coordinator = self.coordinator.lock().await;
        coordinator.reconcile_item_inactive(
            &bcode_provider_auth::managed::AuthLifecycleItem {
                ticket: issued.ticket.clone(),
                namespace: issued.namespace.clone(),
                native_owner: issued.native_owner.clone(),
                custody: issued.custody,
                method: issued.method.clone(),
                phase: bcode_provider_auth::managed::ManagedFlowPhase::Uncertain,
            },
            readiness,
        )
    }

    /// Bounded secret-free daemon maintenance discovery. The caller must inspect the
    /// exact native owner before resolving uncertainty; never replay these as commands.
    /// # Errors
    /// Rejects invalid page allowances.
    pub async fn lifecycle_page(
        &self,
        after_flow_id: Option<u64>,
        limit: usize,
    ) -> Result<bcode_provider_auth::managed::AuthLifecyclePage, CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        coordinator.expire(Instant::now());
        coordinator.lifecycle_page(after_flow_id, limit)
    }

    /// Daemon-internal result of positively verified native inactivity. Not a public
    /// client readiness claim: the authenticated plugin route must inspect the exact
    /// owner/namespace/flow and prove the prior operation ended before calling this.
    /// # Errors
    /// Rejects wrong owner or stale maintenance evidence; does not retry native login.
    pub async fn reconcile_lifecycle_inactive(
        &self,
        authenticated_plugin_id: &str,
        item: &bcode_provider_auth::managed::AuthLifecycleItem,
        readiness: AuthReadiness,
    ) -> Result<(), CoordinationError> {
        if authenticated_plugin_id != item.namespace.plugin_id {
            return Err(CoordinationError::Stale);
        }
        self.coordinator
            .lock()
            .await
            .reconcile_item_inactive(item, readiness)
    }

    /// Acknowledge terminal auth only after current canonical authorization. Other
    /// waiters remain independently reconnectable until acknowledgment or expiry.
    /// # Errors
    /// Rejects stale caller/instance or nonterminal state; never clears uncertainty.
    pub async fn acknowledge_terminal(
        &self,
        authority: &impl HarnessAuthAuthority,
        caller: &str,
        session_id: &str,
        instance_id: &str,
        handle: &ManagedAuthHandle,
    ) -> Result<(), CoordinationError> {
        let mut coordinator = self.coordinator.lock().await;
        let scope = authority.resolve(caller, session_id, instance_id)?;
        Self::check_client_binding(&coordinator, handle, &scope, instance_id)?;
        coordinator.acknowledge_terminal(handle, &scope.waiter)
    }

    /// Bounded daemon timer hook; clears callbacks but never assumes native settlement.
    pub async fn expire(&self) {
        self.coordinator.lock().await.expire(Instant::now());
    }

    /// Retire only authoritative terminal flows with no unacknowledged waiters.
    /// # Errors
    /// Rejects unresolved flows and retained terminal waiters; periodic maintenance
    /// must not remove a just-settled flow between continuation response and ACK.
    pub async fn retire(&self, handle: &ManagedAuthHandle) -> Result<(), CoordinationError> {
        self.coordinator.lock().await.retire(handle)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn service_debug_is_redacted_even_while_sensitive_state_is_locked() {
        let mut coordinator =
            ManagedAuthCoordinator::new("private-boot".into(), 2, 2, Duration::from_secs(30))
                .unwrap();
        let now = Instant::now();
        let joined = coordinator
            .begin_or_join_owned(
                AuthNamespace {
                    plugin_id: "plugin".into(),
                    credential_namespace: "private-namespace".into(),
                },
                AuthCustodyMode::NativeManaged,
                "native-login".into(),
                "instance".into(),
                AuthWaiter {
                    session_id: "session".into(),
                    generation: 1,
                    ownership_fence: "private-fence".into(),
                },
                now,
            )
            .unwrap();
        coordinator
            .accept(
                &joined.command.unwrap().ticket,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::LoginRequired,
                    state: Some("private-continuation".into()),
                    interaction: Some(SensitiveAuthInteraction::Browser {
                        request_id: "request".into(),
                        url: "https://example.test/private-callback".into(),
                        user_code: Some("private-code".into()),
                    }),
                },
                now,
            )
            .unwrap();
        let service = HarnessAuthService::new(coordinator);
        let _guard = service.coordinator.try_lock().unwrap();
        assert_eq!(format!("{service:?}"), "HarnessAuthService { <redacted> }");
        assert_eq!(format!("{service:#?}"), "HarnessAuthService { <redacted> }");
    }

    #[tokio::test]
    async fn terminal_preparation_is_native_managed_and_claim_is_client_bound() {
        let service = HarnessAuthService::new(
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap(),
        );
        let joined = service
            .begin_client_terminal(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        let command = joined.command.unwrap();
        assert_eq!(
            command.preparation,
            bcode_provider_auth_models::managed::AuthPreparationMode::ClientTerminal
        );
        service
            .accept_plugin_response(
                "plugin",
                &command,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::LoginRequired,
                    state: None,
                    interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                        request_id: "terminal".into(),
                    }),
                },
            )
            .await
            .unwrap();
        let claim = service
            .claim_client_terminal(
                &Authority,
                "authorized",
                "session",
                "instance",
                ClientTerminalClaimRequest {
                    handle: &joined.snapshot.handle,
                    interaction_id: "terminal",
                    claim_id: "claim".into(),
                },
            )
            .await
            .unwrap();
        assert!(
            service
                .client_terminal_reaped("foreign", &claim)
                .await
                .is_err()
        );
        assert!(
            service
                .continue_flow(
                    &Authority,
                    "authorized",
                    "session",
                    "instance",
                    &joined.snapshot.handle,
                    None
                )
                .await
                .is_err()
        );
        service
            .client_terminal_reaped("authorized", &claim)
            .await
            .unwrap();
        assert!(
            service
                .continue_flow(
                    &Authority,
                    "authorized",
                    "session",
                    "instance",
                    &joined.snapshot.handle,
                    None
                )
                .await
                .is_ok()
        );
    }

    async fn claimed_terminal_service() -> (
        HarnessAuthService,
        bcode_provider_auth::managed::ClientTerminalClaim,
    ) {
        let service = HarnessAuthService::new(
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap(),
        );
        let joined = service
            .begin_client_terminal(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        let command = joined.command.unwrap();
        service
            .accept_plugin_response(
                "plugin",
                &command,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::LoginRequired,
                    state: None,
                    interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                        request_id: "private-interaction".into(),
                    }),
                },
            )
            .await
            .unwrap();
        let claim = service
            .claim_client_terminal(
                &Authority,
                "authorized",
                "session",
                "instance",
                ClientTerminalClaimRequest {
                    handle: &joined.snapshot.handle,
                    interaction_id: "private-interaction",
                    claim_id: "private-claim".into(),
                },
            )
            .await
            .unwrap();
        (service, claim)
    }

    #[tokio::test]
    async fn terminal_failure_distinguishes_reap_and_continue_without_exposing_material() {
        let (service, claim) = claimed_terminal_service().await;
        let handle = &claim.handle;
        let before = service
            .terminal_failure(
                &Authority,
                "authorized",
                "session",
                "instance",
                TerminalAuthFailureRequest {
                    handle: &claim.handle,
                    stage: TerminalAuthStage::Reap,
                    cause: CoordinationError::Stale,
                },
            )
            .await;
        assert_eq!(before.stage, TerminalAuthStage::Reap);
        assert_eq!(
            before.current.as_ref().unwrap().terminal_phase,
            bcode_provider_auth::managed::ClientTerminalPhase::Issued
        );
        assert!(!format!("{before:?}").contains("private"));
        let denied = service
            .terminal_failure(
                &Authority,
                "foreign",
                "session",
                "instance",
                TerminalAuthFailureRequest {
                    handle: &claim.handle,
                    stage: TerminalAuthStage::Prepare,
                    cause: CoordinationError::Stale,
                },
            )
            .await;
        assert!(denied.current.is_none());
        service
            .client_terminal_reaped("authorized", &claim)
            .await
            .unwrap();
        let after = service
            .terminal_failure(
                &Authority,
                "authorized",
                "session",
                "instance",
                TerminalAuthFailureRequest {
                    handle: &claim.handle,
                    stage: TerminalAuthStage::Continue,
                    cause: CoordinationError::Busy,
                },
            )
            .await;
        assert_eq!(after.stage, TerminalAuthStage::Continue);
        assert_eq!(
            after.current.as_ref().unwrap().terminal_phase,
            bcode_provider_auth::managed::ClientTerminalPhase::Reaped
        );
        assert!(
            service
                .continue_flow(
                    &Authority,
                    "authorized",
                    "session",
                    "instance",
                    handle,
                    None
                )
                .await
                .is_ok()
        );
    }

    #[tokio::test]
    async fn background_retirement_preserves_terminal_owner_acknowledgment() {
        let service = HarnessAuthService::new(
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap(),
        );
        let joined = service
            .begin(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        service
            .accept_plugin_response(
                "plugin",
                &joined.command.unwrap(),
                ManagedAuthStep {
                    terminal: true,
                    readiness: AuthReadiness::Ready,
                    state: None,
                    interaction: None,
                },
            )
            .await
            .unwrap();
        let page = service.lifecycle_page(None, 16).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(matches!(
            service.retire(&page.items[0].ticket.handle).await,
            Err(CoordinationError::Busy)
        ));
        service
            .acknowledge_terminal(
                &Authority,
                "authorized",
                "session",
                "instance",
                &joined.snapshot.handle,
            )
            .await
            .unwrap();
        assert_eq!(
            service.lifecycle_page(None, 16).await.unwrap().items,
            Vec::new()
        );
    }

    struct Authority;
    impl HarnessAuthAuthority for Authority {
        fn resolve(
            &self,
            caller: &str,
            session_id: &str,
            instance_id: &str,
        ) -> Result<VerifiedHarnessAuthScope, CoordinationError> {
            if caller != "authorized" || session_id != "session" || instance_id != "instance" {
                return Err(CoordinationError::Stale);
            }
            Ok(VerifiedHarnessAuthScope {
                waiter: AuthWaiter {
                    session_id: session_id.into(),
                    generation: 1,
                    ownership_fence: "fence".into(),
                },
                namespace: AuthNamespace {
                    plugin_id: "plugin".into(),
                    credential_namespace: "native-home".into(),
                },
                custody: AuthCustodyMode::NativeManaged,
                method: "native-login".into(),
            })
        }
    }

    #[test]
    fn native_bridge_request_is_one_shot_and_contains_no_vault_enrollment() {
        let now = Instant::now();
        let mut coordinator =
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap();
        let scope = Authority
            .resolve("authorized", "session", "instance")
            .unwrap();
        let joined = coordinator
            .begin_or_join_owned(
                scope.namespace,
                scope.custody,
                scope.method,
                "instance".into(),
                scope.waiter,
                now,
            )
            .unwrap();
        let instance = bcode_harness_models::HarnessInstance {
            instance_id: "instance".into(),
            plugin_id: "plugin".into(),
            executable: "/native/agent".into(),
            config_namespace: "native-home".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "native".into(),
                profile: "selection-only".into(),
                credential_namespace: "native-home".into(),
                custody: AuthCustodyMode::NativeManaged,
            },
        };
        let mut invocation =
            HarnessAuthInvocation::prepare(joined.command.unwrap(), instance).unwrap();
        let request = invocation.take_request().unwrap();
        assert!(matches!(
            request.kind,
            bcode_harness_models::HarnessRequestKind::AuthBegin(_)
        ));
        assert!(request.kind.operation().is_sensitive());
        assert!(invocation.take_request().is_none());
    }

    #[tokio::test]
    async fn maintenance_reconciles_without_retaining_secret_commands_and_acknowledges_terminal() {
        let service = HarnessAuthService::new(
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap(),
        );
        let joined = service
            .begin(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        let issued = joined.command.unwrap();
        service.delivery_uncertain(&issued.ticket).await.unwrap();
        let page = service.lifecycle_page(None, 1).await.unwrap();
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].native_owner, "instance");
        assert!(
            service
                .reconcile_lifecycle_inactive(
                    "foreign-plugin",
                    &page.items[0],
                    AuthReadiness::Ready
                )
                .await
                .is_err()
        );
        assert!(
            service
                .acknowledge_terminal(
                    &Authority,
                    "authorized",
                    "session",
                    "instance",
                    &joined.snapshot.handle
                )
                .await
                .is_err()
        );
        service
            .reconcile_lifecycle_inactive("plugin", &page.items[0], AuthReadiness::Ready)
            .await
            .unwrap();
        assert!(
            service
                .acknowledge_terminal(
                    &Authority,
                    "foreign",
                    "session",
                    "instance",
                    &joined.snapshot.handle
                )
                .await
                .is_err()
        );
        service
            .acknowledge_terminal(
                &Authority,
                "authorized",
                "session",
                "instance",
                &joined.snapshot.handle,
            )
            .await
            .unwrap();
        assert_eq!(
            service.lifecycle_page(None, 1).await.unwrap().items,
            Vec::new()
        );
        let replacement = service
            .begin(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        assert_ne!(replacement.snapshot.handle, joined.snapshot.handle);
    }

    #[test]
    fn bridge_rejects_a_different_instance_even_with_identical_credentials() {
        let mut coordinator =
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap();
        let scope = Authority
            .resolve("authorized", "session", "instance")
            .unwrap();
        let joined = coordinator
            .begin_or_join_owned(
                scope.namespace,
                scope.custody,
                scope.method,
                "initiating-instance".into(),
                scope.waiter,
                Instant::now(),
            )
            .unwrap();
        let wrong = bcode_harness_models::HarnessInstance {
            instance_id: "different-instance".into(),
            plugin_id: "plugin".into(),
            executable: "/native/agent".into(),
            config_namespace: "native-home".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "native".into(),
                profile: "selection-only".into(),
                credential_namespace: "native-home".into(),
                custody: AuthCustodyMode::NativeManaged,
            },
        };
        assert!(HarnessAuthInvocation::prepare(joined.command.unwrap(), wrong).is_err());
    }

    #[tokio::test]
    async fn application_reauthorizes_and_checks_plugin_response_owner() {
        let service = HarnessAuthService::new(
            ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap(),
        );
        assert!(
            service
                .begin(&Authority, "foreign", "session", "instance")
                .await
                .is_err()
        );
        let flow = service
            .begin(&Authority, "authorized", "session", "instance")
            .await
            .unwrap();
        let command = flow.command.unwrap();
        assert!(
            service
                .status(
                    &Authority,
                    "foreign",
                    "session",
                    "instance",
                    &flow.snapshot.handle
                )
                .await
                .is_err()
        );
        assert!(
            service
                .status(
                    &Authority,
                    "authorized",
                    "session",
                    "instance",
                    &flow.snapshot.handle
                )
                .await
                .is_ok()
        );
        assert!(
            service
                .accept_plugin_response(
                    "foreign-plugin",
                    &command,
                    ManagedAuthStep {
                        terminal: true,
                        readiness: AuthReadiness::Verified,
                        state: None,
                        interaction: None,
                    }
                )
                .await
                .is_err()
        );
        let (snapshot, cancel) = service
            .accept_plugin_response(
                "plugin",
                &command,
                ManagedAuthStep {
                    terminal: true,
                    readiness: AuthReadiness::Ready,
                    state: None,
                    interaction: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(snapshot.readiness, AuthReadiness::Ready);
        assert!(cancel.is_none());
    }
}
