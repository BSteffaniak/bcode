//! Daemon-lifetime authentication coordination.
//!
//! No vault enrollment, native protocol, event persistence, token exchange, or ambient
//! credential discovery occurs here. All callers must first authorize the session and
//! verify its current ownership fence.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use bcode_provider_auth_models::managed::{
    AuthCustodyMode, AuthNamespace, AuthPreparationMode, AuthReadiness, AuthSensitiveInput,
    ManagedAuthHandle, SensitiveAuthInteraction,
};
use zeroize::Zeroize as _;

/// Secret-safe coordination failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoordinationError {
    /// Invalid identity, bounds, or configured allowance.
    Invalid,
    /// No live flow or waiter (including daemon restart).
    NotFound,
    /// Caller/session ownership, generation, flow, or step no longer matches.
    Stale,
    /// Different custody or method cannot share an existing login.
    Conflict,
    /// Bounded daemon allowance exhausted.
    Capacity,
    /// An operation is already in flight or uncertainty requires inspection.
    Busy,
    /// Terminal flows cannot be reopened or overwritten.
    Terminal,
}

impl std::fmt::Display for CoordinationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "authentication coordination rejected: {self:?}")
    }
}
impl std::error::Error for CoordinationError {}

/// Canonical application-verified waiter identity; never constructed from ancestry.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct AuthWaiter {
    /// Session authorized by the application.
    pub session_id: String,
    /// Current canonical session generation.
    pub generation: u64,
    /// Current ownership fencing identity; never accepted merely because client supplied it.
    pub ownership_fence: String,
}

impl AuthWaiter {
    fn valid(&self) -> bool {
        valid_id(&self.session_id) && valid_id(&self.ownership_fence) && self.generation > 0
    }
}

pub use bcode_provider_auth_models::managed::{ManagedFlowPhase, ManagedFlowSnapshot};

/// One-shot step correlation. It is not execution or caller authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthStepTicket {
    /// Boot and flow identity.
    pub handle: ManagedAuthHandle,
    /// Monotonic step within the flow.
    pub step: u64,
}

/// Native semantic action; interpretation belongs exclusively to the plugin.
pub enum ManagedAuthAction {
    /// Begin exactly once through the supported native method.
    Begin,
    /// Continue with optional hidden input, never a model prompt.
    Continue(Option<AuthSensitiveInput>),
    /// Cancel native transient resources. Acknowledgment must be observed separately.
    Cancel,
}

/// Direct plugin invocation payload. Never log, persist, or automatically retry it.
pub struct ManagedAuthCommand {
    /// Correlation for exactly one response.
    pub ticket: AuthStepTicket,
    /// Actual verified credential namespace.
    pub namespace: AuthNamespace,
    /// Initiating native instance identity; every shared waiter must use this owner.
    pub native_owner: String,
    /// Verified execution boundary; `ClientTerminal` Begin prepares only, never launches.
    pub preparation: AuthPreparationMode,
    /// Explicit custody, never silently substituted.
    pub custody: AuthCustodyMode,
    /// Supported plugin-owned native login method.
    pub method: String,
    /// Semantic action, potentially secret-bearing.
    pub action: ManagedAuthAction,
    /// Plugin-owned opaque continuation material, ephemeral only.
    pub state: Option<String>,
}

impl std::fmt::Debug for ManagedAuthCommand {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ManagedAuthCommand { <redacted> }")
    }
}
impl Drop for ManagedAuthCommand {
    fn drop(&mut self) {
        if let Some(state) = &mut self.state {
            state.zeroize();
        }
    }
}

/// Validated native step outcome. Credentials are deliberately not accepted here;
/// vault writes use existing scoped custody operations, native login uses native storage.
pub struct ManagedAuthStep {
    /// Terminal protocol response, not merely a browser launch or cancel acknowledgment.
    pub terminal: bool,
    /// Local readiness or explicit verification observation from the owning plugin.
    pub readiness: AuthReadiness,
    /// Opaque ephemeral continuation required for a pending step.
    pub state: Option<String>,
    /// Dedicated sensitive interaction, never generic transcript content.
    pub interaction: Option<SensitiveAuthInteraction>,
}
impl Drop for ManagedAuthStep {
    fn drop(&mut self) {
        if let Some(state) = &mut self.state {
            state.zeroize();
        }
    }
}

/// Secret-free daemon maintenance item. Never carries callback/state/input material.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthLifecycleItem {
    /// Exact live step; stale inspection must not resolve a newer operation.
    pub ticket: AuthStepTicket,
    /// Verified credential namespace to inspect, not a vault enumeration grant.
    pub namespace: AuthNamespace,
    /// Exact initiating native owner to inspect.
    pub native_owner: String,
    /// Selected custody and plugin method.
    pub custody: AuthCustodyMode,
    /// Supported method identity.
    pub method: String,
    /// Uncertain or settled maintenance state.
    pub phase: ManagedFlowPhase,
}

/// Bounded maintenance page ordered by boot-local monotonic flow ID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthLifecyclePage {
    /// At most the requested number of secret-free entries.
    pub items: Vec<AuthLifecycleItem>,
    /// Continue unchanged; `None` means no further currently retained candidates.
    pub next_after_flow_id: Option<u64>,
}

/// Secret-free phase of client-terminal execution and recovery.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientTerminalPhase {
    /// Preparation has not issued a terminal plan.
    Prepared,
    /// Plan issued once; child reap/no-launch evidence is still required.
    Issued,
    /// Exact issued child is reaped; native readiness must still be inspected.
    Reaped,
}

/// Bounded internal diagnostic facts, never a terminal plan or execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTerminalDiagnostic {
    /// Current live state and step revision.
    pub snapshot: ManagedFlowSnapshot,
    /// Exact claim lifecycle, independent of readiness.
    pub terminal_phase: ClientTerminalPhase,
    /// Whether supplied authenticated client matches the retained claim owner.
    pub client_matches: bool,
    /// Whether supplied current canonical waiter matches the claim's issuance scope.
    pub waiter_matches: bool,
}

/// One-shot client execution claim. Correlation is not execution authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientTerminalClaim {
    /// Exact live flow.
    pub handle: ManagedAuthHandle,
    /// Host-generated retained ID; never reuse it for another launch.
    pub claim_id: String,
    /// Authenticated client owning the terminal child.
    pub client_id: String,
    /// Canonically authorized session owner at issuance.
    pub waiter: AuthWaiter,
}

/// Begin result: only the elected first waiter receives a native begin command.
pub struct JoinAuthFlow {
    /// Safe live status.
    pub snapshot: ManagedFlowSnapshot,
    /// One-shot initial invocation, absent when joining/reconnecting.
    pub command: Option<ManagedAuthCommand>,
}

/// Exact credential selection and supported preparation boundary for one flow.
struct PreparationBinding {
    namespace: AuthNamespace,
    custody: AuthCustodyMode,
    method: String,
    native_owner: String,
    preparation: AuthPreparationMode,
}

struct Flow {
    handle: ManagedAuthHandle,
    native_owner: String,
    custody: AuthCustodyMode,
    method: String,
    waiters: BTreeSet<AuthWaiter>,
    phase: ManagedFlowPhase,
    readiness: AuthReadiness,
    state: Option<String>,
    interaction: Option<SensitiveAuthInteraction>,
    request_ids: BTreeSet<String>,
    polling_interaction: Option<SensitiveAuthInteraction>,
    step: u64,
    deadline: Instant,
    cancel_requested: bool,
    preparation: AuthPreparationMode,
    terminal_claim: Option<ClientTerminalClaim>,
    terminal_reaped: bool,
}
impl Flow {
    fn snapshot(&self) -> ManagedFlowSnapshot {
        ManagedFlowSnapshot {
            handle: self.handle.clone(),
            revision: self
                .step
                .saturating_mul(4)
                .saturating_add(match self.phase {
                    ManagedFlowPhase::InFlight | ManagedFlowPhase::Cancelling => 0,
                    ManagedFlowPhase::Pending => 1,
                    ManagedFlowPhase::Uncertain => 2,
                    ManagedFlowPhase::Settled => 3,
                }),
            phase: self.phase,
            readiness: self.readiness,
            needs_sensitive_interaction: self.interaction.is_some(),
        }
    }
    fn clear_sensitive(&mut self) {
        if let Some(state) = &mut self.state {
            state.zeroize();
        }
        self.state = None;
        self.interaction = None;
        self.polling_interaction = None;
    }
}
impl Drop for Flow {
    fn drop(&mut self) {
        self.clear_sensitive();
    }
}

/// Bounded, exclusively daemon-owned ephemeral flow registry. A new daemon starts
/// empty and must inspect readiness; it never restores callback material from disk.
pub struct ManagedAuthCoordinator {
    daemon_epoch: String,
    next_flow: u64,
    max_flows: usize,
    max_waiters: usize,
    ttl: Duration,
    flows: BTreeMap<AuthNamespace, Flow>,
}

impl ManagedAuthCoordinator {
    /// Create a bounded registry with a unique daemon boot identity.
    /// # Errors
    /// Rejects unbounded/zero allowances and invalid boot identity.
    pub fn new(
        daemon_epoch: String,
        max_flows: usize,
        max_waiters: usize,
        ttl: Duration,
    ) -> Result<Self, CoordinationError> {
        if !valid_id(&daemon_epoch)
            || !(1..=1024).contains(&max_flows)
            || !(1..=1024).contains(&max_waiters)
            || ttl.is_zero()
            || ttl > Duration::from_secs(3600)
        {
            return Err(CoordinationError::Invalid);
        }
        Ok(Self {
            daemon_epoch,
            next_flow: 0,
            max_flows,
            max_waiters,
            ttl,
            flows: BTreeMap::new(),
        })
    }

    /// Compatibility entry point for non-native domain callers. Uses a distinct
    /// reserved owner key; native server dispatch always uses `begin_or_join_owned`.
    /// Admission must verify plugin ownership, explicit custody and supported method first.
    /// On startup an empty registry is NOT proof no native login is active: the caller
    /// must inspect/reconcile that namespace before permitting a fresh begin.
    /// # Errors
    /// Rejects stale/invalid identities, incompatible sharing and exhausted allowances.
    pub fn begin_or_join(
        &mut self,
        namespace: AuthNamespace,
        custody: AuthCustodyMode,
        method: String,
        waiter: AuthWaiter,
        now: Instant,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        self.begin_or_join_owned(
            namespace,
            custody,
            method,
            "namespace-only".into(),
            waiter,
            now,
        )
    }

    /// Begin/share with the exact initiating native instance. Compatibility callers
    /// cannot join a differently owner-bound flow.
    /// # Errors
    /// Rejects different native owners even when credentials share a namespace.
    pub fn begin_or_join_owned(
        &mut self,
        namespace: AuthNamespace,
        custody: AuthCustodyMode,
        method: String,
        native_owner: String,
        waiter: AuthWaiter,
        now: Instant,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        self.begin_prepared(
            PreparationBinding {
                namespace,
                custody,
                method,
                native_owner,
                preparation: AuthPreparationMode::NativeProtocol,
            },
            waiter,
            now,
        )
    }

    /// Reserve supported side-effect-free terminal preparation, not native inactivity.
    /// Caller must retain cross-daemon namespace/issued-claim fencing and verify the
    /// plugin's preparation-mode evidence. No process is launched by this method.
    /// # Errors
    /// Rejects incompatible sharing, uncertainty, invalid identity or capacity exhaustion.
    pub fn begin_client_terminal(
        &mut self,
        namespace: AuthNamespace,
        method: String,
        native_owner: String,
        waiter: AuthWaiter,
        now: Instant,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        self.begin_prepared(
            PreparationBinding {
                namespace,
                custody: AuthCustodyMode::NativeManaged,
                method,
                native_owner,
                preparation: AuthPreparationMode::ClientTerminal,
            },
            waiter,
            now,
        )
    }

    fn begin_prepared(
        &mut self,
        binding: PreparationBinding,
        waiter: AuthWaiter,
        now: Instant,
    ) -> Result<JoinAuthFlow, CoordinationError> {
        let PreparationBinding {
            namespace,
            custody,
            method,
            native_owner,
            preparation,
        } = binding;
        if !namespace.is_valid()
            || !valid_id(&method)
            || !valid_id(&native_owner)
            || !waiter.valid()
        {
            return Err(CoordinationError::Invalid);
        }
        self.expire(now);
        if let Some(flow) = self.flows.get_mut(&namespace) {
            if flow.custody != custody
                || flow.method != method
                || flow.native_owner != native_owner
                || flow.preparation != preparation
            {
                return Err(CoordinationError::Conflict);
            }
            if flow.phase == ManagedFlowPhase::Settled {
                return Err(CoordinationError::Terminal);
            }
            if flow.cancel_requested || flow.phase == ManagedFlowPhase::Uncertain {
                return Err(CoordinationError::Busy);
            }
            if flow
                .waiters
                .iter()
                .any(|existing| existing.session_id == waiter.session_id && existing != &waiter)
            {
                return Err(CoordinationError::Stale);
            }
            if !flow.waiters.contains(&waiter) && flow.waiters.len() >= self.max_waiters {
                return Err(CoordinationError::Capacity);
            }
            flow.waiters.insert(waiter);
            return Ok(JoinAuthFlow {
                snapshot: flow.snapshot(),
                command: None,
            });
        }
        if self.flows.len() >= self.max_flows {
            return Err(CoordinationError::Capacity);
        }
        let deadline = now
            .checked_add(self.ttl)
            .ok_or(CoordinationError::Invalid)?;
        self.next_flow = self
            .next_flow
            .checked_add(1)
            .ok_or(CoordinationError::Capacity)?;
        let handle = ManagedAuthHandle {
            daemon_epoch: self.daemon_epoch.clone(),
            flow_id: self.next_flow,
        };
        let flow = Flow {
            handle: handle.clone(),
            native_owner: native_owner.clone(),
            custody,
            method: method.clone(),
            waiters: BTreeSet::from([waiter]),
            phase: ManagedFlowPhase::InFlight,
            readiness: AuthReadiness::Unknown,
            state: None,
            interaction: None,
            request_ids: BTreeSet::new(),
            polling_interaction: None,
            step: 1,
            deadline,
            cancel_requested: false,
            preparation,
            terminal_claim: None,
            terminal_reaped: false,
        };
        let snapshot = flow.snapshot();
        self.flows.insert(namespace.clone(), flow);
        Ok(JoinAuthFlow {
            snapshot,
            command: Some(ManagedAuthCommand {
                ticket: AuthStepTicket { handle, step: 1 },
                namespace,
                native_owner,
                preparation,
                custody,
                method,
                action: ManagedAuthAction::Begin,
                state: None,
            }),
        })
    }

    /// Claim a prepared terminal interaction exactly once before returning its plan.
    /// Host must durably fence issued execution across daemon loss before plan delivery.
    /// # Errors
    /// Rejects wrong waiter, nonterminal method, stale interaction, or any earlier claim.
    pub fn claim_client_terminal(
        &mut self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
        interaction_id: &str,
        client_id: String,
        claim_id: String,
        now: Instant,
    ) -> Result<ClientTerminalClaim, CoordinationError> {
        self.expire(now);
        self.authorized(handle, waiter)?;
        if !valid_id(&client_id) || !valid_id(&claim_id) {
            return Err(CoordinationError::Invalid);
        }
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.preparation != AuthPreparationMode::ClientTerminal
            || flow.phase != ManagedFlowPhase::Pending
            || flow.cancel_requested
            || flow.terminal_claim.is_some()
            || !matches!(&flow.interaction, Some(SensitiveAuthInteraction::NativeHandoff { request_id }) if request_id == interaction_id)
        {
            return Err(CoordinationError::Busy);
        }
        let claim = ClientTerminalClaim {
            handle: handle.clone(),
            claim_id,
            client_id,
            waiter: waiter.clone(),
        };
        flow.terminal_claim = Some(claim.clone());
        Ok(claim)
    }

    /// Inspect claim phase for an already authorized exact native binding only.
    /// This does not require membership so a denied competitor can be diagnosed without
    /// altering waiters, claim ownership, expiry, or callback state. No identity is exposed.
    /// # Errors
    /// Rejects unknown flow or nonterminal preparation.
    pub fn client_terminal_diagnostic(
        &self,
        handle: &ManagedAuthHandle,
        client_id: &str,
        waiter: &AuthWaiter,
    ) -> Result<ClientTerminalDiagnostic, CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.preparation != AuthPreparationMode::ClientTerminal {
            return Err(CoordinationError::Conflict);
        }
        let terminal_phase = if flow.terminal_reaped {
            ClientTerminalPhase::Reaped
        } else if flow.terminal_claim.is_some() {
            ClientTerminalPhase::Issued
        } else {
            ClientTerminalPhase::Prepared
        };
        Ok(ClientTerminalDiagnostic {
            snapshot: flow.snapshot(),
            terminal_phase,
            client_matches: flow
                .terminal_claim
                .as_ref()
                .is_some_and(|claim| claim.client_id == client_id),
            waiter_matches: flow
                .terminal_claim
                .as_ref()
                .is_some_and(|claim| &claim.waiter == waiter),
        })
    }

    /// Read the retained secret-free terminal claim for bounded lifecycle recovery.
    /// Only daemon orchestration may use this; it grants no right to launch/replay.
    /// # Errors
    /// Rejects unknown boot/flow identity.
    pub fn client_terminal_claim(
        &self,
        handle: &ManagedAuthHandle,
    ) -> Result<Option<&ClientTerminalClaim>, CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        Ok(self
            .flows
            .get(&namespace)
            .ok_or(CoordinationError::NotFound)?
            .terminal_claim
            .as_ref())
    }

    /// Record authenticated client-child reap/no-launch proof for the exact issued claim.
    /// Native exit is NOT readiness. Caller verifies client/process ownership; a bare
    /// client boolean, socket disconnect, or local status is not sufficient evidence.
    /// Repeated exact reap reports are safe. A different client cannot inherit the claim.
    /// # Errors
    /// Rejects unknown boot/claim or conflicting retained process ownership.
    pub fn client_terminal_reaped(
        &mut self,
        claim: &ClientTerminalClaim,
    ) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(&claim.handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.terminal_claim.as_ref() != Some(claim) {
            return Err(CoordinationError::Stale);
        }
        flow.terminal_reaped = true;
        Ok(())
    }

    /// Check the exact initiating native instance before sharing status or dispatch.
    /// # Errors
    /// Rejects native instance substitution even within the same credential namespace.
    pub fn check_native_owner(
        &self,
        handle: &ManagedAuthHandle,
        native_owner: &str,
    ) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.native_owner != native_owner {
            return Err(CoordinationError::Conflict);
        }
        Ok(())
    }

    /// Verify that a live flow still belongs to the canonical admitted instance binding.
    /// # Errors
    /// Rejects stale handles, namespace changes, custody changes and method substitution.
    pub fn check_binding(
        &self,
        handle: &ManagedAuthHandle,
        namespace: &AuthNamespace,
        custody: AuthCustodyMode,
        method: &str,
    ) -> Result<(), CoordinationError> {
        let actual = self.namespace_for(handle)?;
        let flow = self.flows.get(&actual).ok_or(CoordinationError::NotFound)?;
        if &actual != namespace || flow.custody != custody || flow.method != method {
            return Err(CoordinationError::Conflict);
        }
        Ok(())
    }

    /// Rejoin a live flow only after reauthorizing the exact waiter fence.
    /// Disconnect is not cancellation; timeout never proves no native effects.
    /// # Errors
    /// Rejects stale boot, generation, ownership or missing live membership.
    pub fn snapshot(
        &self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
    ) -> Result<ManagedFlowSnapshot, CoordinationError> {
        Ok(self.authorized(handle, waiter)?.snapshot())
    }

    /// Borrow sensitive presentation for the dedicated trace-exempt auth channel only.
    /// # Errors
    /// Rejects unauthorized/stale waiter identity.
    pub fn interaction(
        &self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
    ) -> Result<Option<&SensitiveAuthInteraction>, CoordinationError> {
        Ok(self.authorized(handle, waiter)?.interaction.as_ref())
    }

    /// Issue one continuation; consume state/input once, never replay after lost ack.
    /// # Errors
    /// Rejects expired, concurrent, terminal, stale and mismatched sensitive input.
    pub fn continue_flow(
        &mut self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
        input: Option<AuthSensitiveInput>,
        now: Instant,
    ) -> Result<ManagedAuthCommand, CoordinationError> {
        self.expire(now);
        let namespace = self.namespace_for(handle)?;
        self.authorized(handle, waiter)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.phase != ManagedFlowPhase::Pending
            || flow.cancel_requested
            || (flow.terminal_claim.is_some() && !flow.terminal_reaped)
        {
            return Err(CoordinationError::Busy);
        }
        match (&flow.interaction, &input) {
            (Some(SensitiveAuthInteraction::Input { request_id }), Some(input))
                if input.is_valid() && input.request_id() == request_id => {}
            (Some(SensitiveAuthInteraction::Input { .. }), _) | (_, Some(_)) => {
                return Err(CoordinationError::Invalid);
            }
            _ => {}
        }
        if flow.step >= u64::MAX / 4 - 1 {
            return Err(CoordinationError::Capacity);
        }
        let step = flow
            .step
            .checked_add(1)
            .ok_or(CoordinationError::Capacity)?;
        flow.step = step;
        flow.phase = ManagedFlowPhase::InFlight;
        flow.interaction = None;
        Ok(ManagedAuthCommand {
            ticket: AuthStepTicket {
                handle: flow.handle.clone(),
                step,
            },
            namespace,
            custody: flow.custody,
            method: flow.method.clone(),
            native_owner: flow.native_owner.clone(),
            preparation: flow.preparation,
            action: ManagedAuthAction::Continue(input),
            state: flow.state.take(),
        })
    }

    /// Accept exactly the in-flight step from the authenticated owning plugin route.
    /// Acknowledgment of cancellation alone is not terminal; the plugin must reconcile.
    /// # Errors
    /// Rejects stale/duplicate steps, malformed effects and responses after uncertainty.
    pub fn accept(
        &mut self,
        ticket: &AuthStepTicket,
        mut response: ManagedAuthStep,
        now: Instant,
    ) -> Result<ManagedFlowSnapshot, CoordinationError> {
        self.expire(now);
        let namespace = self.namespace_for(&ticket.handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.step != ticket.step
            || !matches!(
                flow.phase,
                ManagedFlowPhase::InFlight | ManagedFlowPhase::Cancelling
            )
        {
            return Err(CoordinationError::Stale);
        }
        if response
            .state
            .as_ref()
            .is_some_and(|state| state.len() > 65536)
            || response.interaction.as_ref().is_some_and(|effect| {
                !effect.is_valid()
                    || (flow.request_ids.contains(effect.request_id())
                        && flow.polling_interaction.as_ref() != Some(effect))
                    || (!flow.request_ids.contains(effect.request_id())
                        && flow.request_ids.len() >= 64)
            })
            || (response.terminal && (response.state.is_some() || response.interaction.is_some()))
        {
            flow.clear_sensitive();
            flow.phase = ManagedFlowPhase::Uncertain;
            return Err(CoordinationError::Invalid);
        }
        if response.terminal && flow.terminal_claim.is_some() && !flow.terminal_reaped {
            flow.clear_sensitive();
            flow.phase = ManagedFlowPhase::Uncertain;
            return Err(CoordinationError::Busy);
        }
        if flow.preparation == AuthPreparationMode::ClientTerminal
            && response.interaction.as_ref().is_some_and(|effect| {
                !matches!(effect, SensitiveAuthInteraction::NativeHandoff { .. })
            })
        {
            flow.clear_sensitive();
            flow.phase = ManagedFlowPhase::Uncertain;
            return Err(CoordinationError::Invalid);
        }
        let was_cancelling = flow.phase == ManagedFlowPhase::Cancelling;
        flow.clear_sensitive();
        flow.readiness = response.readiness;
        if response.terminal {
            flow.phase = ManagedFlowPhase::Settled;
        } else if was_cancelling {
            // A cleanup acknowledgment without terminal native evidence cannot
            // authorize another cancel mutation or release the namespace.
            flow.phase = ManagedFlowPhase::Uncertain;
            flow.readiness = AuthReadiness::Unknown;
        } else {
            flow.phase = ManagedFlowPhase::Pending;
            flow.state = response.state.take();
            if !flow.cancel_requested {
                if let Some(interaction) = &response.interaction {
                    flow.request_ids.insert(interaction.request_id().to_owned());
                    if !matches!(interaction, SensitiveAuthInteraction::Input { .. }) {
                        flow.polling_interaction = Some(interaction.clone());
                    }
                }
                flow.interaction = response.interaction.take();
            }
        }
        Ok(flow.snapshot())
    }

    /// Cancel one waiter independently. Last waiter requests native cleanup but
    /// retains the namespace reservation until authoritative reconciliation.
    /// # Errors
    /// Rejects stale handles or non-member waiters. An in-flight command is never replayed.
    pub fn cancel_waiter(
        &mut self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
    ) -> Result<Option<ManagedAuthCommand>, CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        self.authorized(handle, waiter)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        flow.waiters.remove(waiter);
        if !flow.waiters.is_empty() {
            return Ok(None);
        }
        if flow.phase == ManagedFlowPhase::Settled {
            self.flows.remove(&namespace);
            return Ok(None);
        }
        flow.cancel_requested = true;
        flow.interaction = None;
        self.take_cancel(handle)
    }

    /// Drain a deferred last-waiter cancellation after an in-flight response settles.
    /// Must be called by daemon orchestration, independent of a connected client.
    /// # Errors
    /// Rejects unknown/stale flows. Returns no command while a previous step is unresolved.
    pub fn take_cancel(
        &mut self,
        handle: &ManagedAuthHandle,
    ) -> Result<Option<ManagedAuthCommand>, CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if !flow.cancel_requested || flow.phase != ManagedFlowPhase::Pending {
            return Ok(None);
        }
        if flow.terminal_claim.is_some() && !flow.terminal_reaped {
            flow.clear_sensitive();
            flow.phase = ManagedFlowPhase::Uncertain;
            return Ok(None);
        }
        if flow.step >= u64::MAX / 4 - 1 {
            return Err(CoordinationError::Capacity);
        }
        flow.step = flow
            .step
            .checked_add(1)
            .ok_or(CoordinationError::Capacity)?;
        flow.phase = ManagedFlowPhase::Cancelling;
        Ok(Some(ManagedAuthCommand {
            ticket: AuthStepTicket {
                handle: handle.clone(),
                step: flow.step,
            },
            namespace,
            custody: flow.custody,
            method: flow.method.clone(),
            native_owner: flow.native_owner.clone(),
            preparation: flow.preparation,
            action: ManagedAuthAction::Cancel,
            state: flow.state.take(),
        }))
    }

    /// Mark a lost response/delivery ambiguous; only read-only native inspection can
    /// resolve this reservation. Does not assert no effects or retry any command.
    /// # Errors
    /// Rejects stale tickets or terminal/non-in-flight transitions.
    pub fn mark_uncertain(&mut self, ticket: &AuthStepTicket) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(&ticket.handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if ticket.step != flow.step
            || !matches!(
                flow.phase,
                ManagedFlowPhase::InFlight | ManagedFlowPhase::Cancelling
            )
        {
            return Err(CoordinationError::Stale);
        }
        flow.clear_sensitive();
        flow.phase = ManagedFlowPhase::Uncertain;
        flow.readiness = AuthReadiness::Unknown;
        Ok(())
    }

    /// Clear an uncertain reservation only after plugin inspection verifies the old
    /// native auth operation is no longer active. This never retries native login.
    /// # Errors
    /// Rejects absent/stale handles and any non-uncertain state.
    pub fn reconcile_inactive(
        &mut self,
        handle: &ManagedAuthHandle,
        readiness: AuthReadiness,
    ) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.phase != ManagedFlowPhase::Uncertain
            || (flow.terminal_claim.is_some() && !flow.terminal_reaped)
        {
            return Err(CoordinationError::Busy);
        }
        flow.readiness = readiness;
        flow.phase = ManagedFlowPhase::Settled;
        Ok(())
    }

    /// Discover bounded daemon cleanup work without retaining issued secret payloads.
    /// Work scans only the configured bounded registry, not session history.
    /// # Errors
    /// Rejects limits outside 1..=128.
    pub fn lifecycle_page(
        &self,
        after_flow_id: Option<u64>,
        limit: usize,
    ) -> Result<AuthLifecyclePage, CoordinationError> {
        if !(1..=128).contains(&limit) {
            return Err(CoordinationError::Invalid);
        }
        let mut candidates: Vec<_> = self
            .flows
            .iter()
            .filter(|(_, flow)| {
                flow.handle.flow_id > after_flow_id.unwrap_or(0)
                    && matches!(
                        flow.phase,
                        ManagedFlowPhase::Uncertain | ManagedFlowPhase::Settled
                    )
            })
            .collect();
        candidates.sort_by_key(|(_, flow)| flow.handle.flow_id);
        let has_more = candidates.len() > limit;
        let items: Vec<_> = candidates
            .into_iter()
            .take(limit)
            .map(|(namespace, flow)| AuthLifecycleItem {
                ticket: AuthStepTicket {
                    handle: flow.handle.clone(),
                    step: flow.step,
                },
                namespace: namespace.clone(),
                native_owner: flow.native_owner.clone(),
                custody: flow.custody,
                method: flow.method.clone(),
                phase: flow.phase,
            })
            .collect();
        let next_after_flow_id = if has_more {
            items.last().map(|item| item.ticket.handle.flow_id)
        } else {
            None
        };
        Ok(AuthLifecyclePage {
            items,
            next_after_flow_id,
        })
    }

    /// Complete a read-only inactive inspection for the exact discovered flow step.
    /// Caller must verify plugin ownership and native inactivity, not just readiness.
    /// # Errors
    /// Rejects stale/tampered candidates and non-uncertain states without retiring them.
    pub fn reconcile_item_inactive(
        &mut self,
        item: &AuthLifecycleItem,
        readiness: AuthReadiness,
    ) -> Result<(), CoordinationError> {
        self.check_native_owner(&item.ticket.handle, &item.native_owner)?;
        self.check_binding(
            &item.ticket.handle,
            &item.namespace,
            item.custody,
            &item.method,
        )?;
        let flow = self
            .flows
            .get(&item.namespace)
            .ok_or(CoordinationError::NotFound)?;
        if flow.step != item.ticket.step || item.phase != ManagedFlowPhase::Uncertain {
            return Err(CoordinationError::Stale);
        }
        self.reconcile_inactive(&item.ticket.handle, readiness)
    }

    /// Acknowledge terminal auth for one exact waiter. Last acknowledgment retires
    /// only settled state; disconnect alone never acknowledges or cancels a flow.
    /// # Errors
    /// Rejects stale/nonmember waiters or unresolved state.
    pub fn acknowledge_terminal(
        &mut self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
    ) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        if self.authorized(handle, waiter)?.phase != ManagedFlowPhase::Settled {
            return Err(CoordinationError::Busy);
        }
        let flow = self
            .flows
            .get_mut(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        flow.waiters.remove(waiter);
        if flow.waiters.is_empty() {
            self.flows.remove(&namespace);
        }
        Ok(())
    }

    /// Explicitly retire authoritative terminal state with no unacknowledged waiters.
    /// # Errors
    /// Rejects unresolved flows and retained waiters: maintenance cannot race terminal ACK.
    pub fn retire(&mut self, handle: &ManagedAuthHandle) -> Result<(), CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        if self
            .flows
            .get(&namespace)
            .is_none_or(|flow| flow.phase != ManagedFlowPhase::Settled || !flow.waiters.is_empty())
        {
            return Err(CoordinationError::Busy);
        }
        self.flows.remove(&namespace);
        Ok(())
    }

    /// Bounded sweep; expiry invalidates sensitive callbacks without claiming native
    /// cancellation or making the namespace available for replay.
    pub fn expire(&mut self, now: Instant) {
        // Only authoritative terminal entries may age out. Uncertain entries retain
        // namespace reservations indefinitely until explicit inactive reconciliation.
        self.flows
            .retain(|_, flow| !(now >= flow.deadline && flow.phase == ManagedFlowPhase::Settled));
        for flow in self.flows.values_mut() {
            if now >= flow.deadline && flow.phase != ManagedFlowPhase::Settled {
                flow.clear_sensitive();
                flow.phase = ManagedFlowPhase::Uncertain;
                flow.readiness = AuthReadiness::Unknown;
            }
        }
    }

    fn namespace_for(
        &self,
        handle: &ManagedAuthHandle,
    ) -> Result<AuthNamespace, CoordinationError> {
        if handle.daemon_epoch != self.daemon_epoch {
            return Err(CoordinationError::Stale);
        }
        self.flows
            .iter()
            .find_map(|(namespace, flow)| (&flow.handle == handle).then(|| namespace.clone()))
            .ok_or(CoordinationError::NotFound)
    }

    fn authorized(
        &self,
        handle: &ManagedAuthHandle,
        waiter: &AuthWaiter,
    ) -> Result<&Flow, CoordinationError> {
        let namespace = self.namespace_for(handle)?;
        let flow = self
            .flows
            .get(&namespace)
            .ok_or(CoordinationError::NotFound)?;
        if !flow.waiters.contains(waiter) {
            return Err(CoordinationError::Stale);
        }
        Ok(flow)
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(char::is_control)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_provider_auth_models::managed::ManagedAuthFailure;

    fn registry() -> ManagedAuthCoordinator {
        ManagedAuthCoordinator::new("boot".into(), 2, 2, Duration::from_secs(30)).unwrap()
    }
    fn namespace() -> AuthNamespace {
        AuthNamespace {
            plugin_id: "plugin".into(),
            credential_namespace: "actual-native-home".into(),
        }
    }
    fn waiter(session: &str) -> AuthWaiter {
        AuthWaiter {
            session_id: session.into(),
            generation: 1,
            ownership_fence: "owner".into(),
        }
    }
    fn begin(registry: &mut ManagedAuthCoordinator, session: &str, now: Instant) -> JoinAuthFlow {
        registry
            .begin_or_join(
                namespace(),
                AuthCustodyMode::NativeManaged,
                "native-login".into(),
                waiter(session),
                now,
            )
            .unwrap()
    }
    fn pending(request_id: &str) -> ManagedAuthStep {
        ManagedAuthStep {
            terminal: false,
            readiness: AuthReadiness::LoginRequired,
            state: Some("private-state".into()),
            interaction: Some(SensitiveAuthInteraction::Input {
                request_id: request_id.into(),
            }),
        }
    }
    fn terminal(readiness: AuthReadiness) -> ManagedAuthStep {
        ManagedAuthStep {
            terminal: true,
            readiness,
            state: None,
            interaction: None,
        }
    }

    #[test]
    fn namespace_waiters_share_one_begin_and_cancel_independently() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        let second = begin(&mut registry, "b", now);
        assert!(second.command.is_none());
        assert_eq!(second.snapshot.handle, handle);
        assert!(
            registry
                .cancel_waiter(&handle, &waiter("a"))
                .unwrap()
                .is_none()
        );
        assert!(registry.snapshot(&handle, &waiter("a")).is_err());
        registry
            .accept(&command.ticket, pending("one"), now)
            .unwrap();
        assert!(
            registry
                .interaction(&handle, &waiter("b"))
                .unwrap()
                .is_some()
        );
        let cancel = registry
            .cancel_waiter(&handle, &waiter("b"))
            .unwrap()
            .unwrap();
        assert!(matches!(cancel.action, ManagedAuthAction::Cancel));
        assert!(registry.retire(&handle).is_err());
        registry
            .accept(&cancel.ticket, terminal(AuthReadiness::LoginRequired), now)
            .unwrap();
        registry.retire(&handle).unwrap();
    }

    #[test]
    fn cancellation_during_delivery_waits_for_response_then_cleans_up() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        assert!(
            registry
                .cancel_waiter(&handle, &waiter("a"))
                .unwrap()
                .is_none()
        );
        let snapshot = registry
            .accept(&command.ticket, pending("one"), now)
            .unwrap();
        assert!(!snapshot.needs_sensitive_interaction);
        let cancel = registry.take_cancel(&handle).unwrap().unwrap();
        assert!(matches!(cancel.action, ManagedAuthAction::Cancel));
        assert!(registry.take_cancel(&handle).unwrap().is_none());
        // Mere nonterminal cancellation acknowledgment does not retire native effects.
        registry
            .accept(
                &cancel.ticket,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::Unknown,
                    state: None,
                    interaction: None,
                },
                now,
            )
            .unwrap();
        assert!(registry.retire(&handle).is_err());
    }

    #[test]
    fn lost_ack_expiry_and_restart_never_replay_native_login() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        registry.mark_uncertain(&command.ticket).unwrap();
        assert!(
            registry
                .accept(&command.ticket, terminal(AuthReadiness::Verified), now)
                .is_err()
        );
        assert!(
            registry
                .begin_or_join(
                    namespace(),
                    AuthCustodyMode::NativeManaged,
                    "native-login".into(),
                    waiter("b"),
                    now
                )
                .is_err()
        );
        assert!(
            registry
                .continue_flow(&handle, &waiter("a"), None, now)
                .is_err()
        );
        assert!(registry.retire(&handle).is_err());
        registry
            .reconcile_inactive(&handle, AuthReadiness::Blocked(ManagedAuthFailure::Expired))
            .unwrap();
        assert_eq!(
            registry.snapshot(&handle, &waiter("a")).unwrap().readiness,
            AuthReadiness::Blocked(ManagedAuthFailure::Expired)
        );
        registry
            .acknowledge_terminal(&handle, &waiter("a"))
            .unwrap();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        registry
            .accept(&command.ticket, pending("one"), now)
            .unwrap();
        registry.expire(now + Duration::from_secs(31));
        let snapshot = registry
            .snapshot(&joined.snapshot.handle, &waiter("a"))
            .unwrap();
        assert_eq!(snapshot.phase, ManagedFlowPhase::Uncertain);
        assert!(!snapshot.needs_sensitive_interaction);
        let restarted =
            ManagedAuthCoordinator::new("new-boot".into(), 2, 2, Duration::from_secs(30)).unwrap();
        assert!(
            restarted
                .snapshot(&joined.snapshot.handle, &waiter("a"))
                .is_err()
        );
    }

    #[test]
    fn reconnect_requires_exact_current_fence_and_binding() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "a", now);
        let handle = joined.snapshot.handle;
        assert!(registry.snapshot(&handle, &waiter("a")).is_ok());
        let mut stale = waiter("a");
        stale.generation += 1;
        assert!(registry.snapshot(&handle, &stale).is_err());
        stale = waiter("a");
        stale.ownership_fence = "foreign".into();
        assert!(registry.snapshot(&handle, &stale).is_err());
        assert!(
            registry
                .check_binding(
                    &handle,
                    &namespace(),
                    AuthCustodyMode::External,
                    "native-login"
                )
                .is_err()
        );
        let mut different = namespace();
        different.credential_namespace = "different-home".into();
        assert!(
            registry
                .check_binding(
                    &handle,
                    &different,
                    AuthCustodyMode::NativeManaged,
                    "native-login"
                )
                .is_err()
        );
    }

    #[test]
    fn sensitive_input_is_consumed_once_and_snapshots_contain_no_callback_material() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        let snapshot = registry
            .accept(&command.ticket, pending("one"), now)
            .unwrap();
        assert!(!format!("{snapshot:?}").contains("private-state"));
        let wrong = AuthSensitiveInput::new("wrong".into(), "secret".into()).unwrap();
        assert!(
            registry
                .continue_flow(&handle, &waiter("a"), Some(wrong), now)
                .is_err()
        );
        let input = AuthSensitiveInput::new("one".into(), "secret".into()).unwrap();
        let continuation = registry
            .continue_flow(&handle, &waiter("a"), Some(input), now)
            .unwrap();
        assert!(!format!("{continuation:?}").contains("secret"));
        assert!(
            registry
                .continue_flow(&handle, &waiter("a"), None, now)
                .is_err()
        );
        assert!(
            registry
                .accept(&command.ticket, pending("old"), now)
                .is_err()
        );
        // A reused prompt ID cannot make a delayed old callback valid again.
        assert!(
            registry
                .accept(&continuation.ticket, pending("one"), now)
                .is_err()
        );
        assert_eq!(
            registry.snapshot(&handle, &waiter("a")).unwrap().phase,
            ManagedFlowPhase::Uncertain
        );
    }

    #[test]
    fn competitor_rejection_preserves_original_terminal_claim_revision_and_reap() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = registry
            .begin_client_terminal(
                namespace(),
                "native-login".into(),
                "instance".into(),
                waiter("owner"),
                now,
            )
            .unwrap();
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        registry
            .accept(
                &command.ticket,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::LoginRequired,
                    state: None,
                    interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                        request_id: "terminal".into(),
                    }),
                },
                now,
            )
            .unwrap();
        let claim = registry
            .claim_client_terminal(
                &handle,
                &waiter("owner"),
                "terminal",
                "owner-client".into(),
                "claim".into(),
                now,
            )
            .unwrap();
        let before = registry
            .client_terminal_diagnostic(&handle, "owner-client", &waiter("owner"))
            .unwrap();
        let mut competitor = waiter("owner");
        competitor.ownership_fence = "different-fence".into();
        assert!(
            registry
                .begin_client_terminal(
                    namespace(),
                    "native-login".into(),
                    "instance".into(),
                    competitor.clone(),
                    now
                )
                .is_err()
        );
        assert!(
            registry
                .claim_client_terminal(
                    &handle,
                    &competitor,
                    "terminal",
                    "competitor-client".into(),
                    "other-claim".into(),
                    now
                )
                .is_err()
        );
        assert_eq!(
            registry
                .client_terminal_diagnostic(&handle, "owner-client", &waiter("owner"))
                .unwrap(),
            before
        );
        assert_eq!(
            registry.client_terminal_claim(&handle).unwrap(),
            Some(&claim)
        );
        registry.client_terminal_reaped(&claim).unwrap();
        let after = registry
            .client_terminal_diagnostic(&handle, "owner-client", &waiter("owner"))
            .unwrap();
        assert_eq!(after.terminal_phase, ClientTerminalPhase::Reaped);
        assert!(after.client_matches && after.waiter_matches);
        assert_eq!(after.snapshot.revision, before.snapshot.revision);
        let poll = registry
            .continue_flow(&handle, &waiter("owner"), None, now)
            .unwrap();
        assert!(poll.ticket.step > command.ticket.step);
        let settled = registry
            .accept(&poll.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        assert_eq!(settled.phase, ManagedFlowPhase::Settled);
    }

    #[test]
    fn client_terminal_preparation_claim_reap_and_readiness_are_distinct() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = registry
            .begin_client_terminal(
                namespace(),
                "native-login".into(),
                "instance".into(),
                waiter("a"),
                now,
            )
            .unwrap();
        let command = joined.command.unwrap();
        assert_eq!(command.preparation, AuthPreparationMode::ClientTerminal);
        let handle = joined.snapshot.handle;
        let joined_waiter = registry
            .begin_client_terminal(
                namespace(),
                "native-login".into(),
                "instance".into(),
                waiter("b"),
                now,
            )
            .unwrap();
        assert!(joined_waiter.command.is_none());
        let pending_handoff = || ManagedAuthStep {
            terminal: false,
            readiness: AuthReadiness::LoginRequired,
            state: None,
            interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                request_id: "terminal".into(),
            }),
        };
        registry
            .accept(&command.ticket, pending_handoff(), now)
            .unwrap();
        let claim = registry
            .claim_client_terminal(
                &handle,
                &waiter("a"),
                "terminal",
                "client-a".into(),
                "claim-one".into(),
                now,
            )
            .unwrap();
        assert!(
            registry
                .claim_client_terminal(
                    &handle,
                    &waiter("b"),
                    "terminal",
                    "client-b".into(),
                    "claim-two".into(),
                    now
                )
                .is_err()
        );
        assert!(
            registry
                .continue_flow(&handle, &waiter("a"), None, now)
                .is_err()
        );
        let mut wrong = claim.clone();
        wrong.client_id = "client-b".into();
        assert!(registry.client_terminal_reaped(&wrong).is_err());
        registry.client_terminal_reaped(&claim).unwrap();
        registry.client_terminal_reaped(&claim).unwrap();
        assert_eq!(
            registry.snapshot(&handle, &waiter("a")).unwrap().readiness,
            AuthReadiness::LoginRequired
        );
        let poll = registry
            .continue_flow(&handle, &waiter("b"), None, now)
            .unwrap();
        registry
            .accept(&poll.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        assert_eq!(
            registry.snapshot(&handle, &waiter("a")).unwrap().phase,
            ManagedFlowPhase::Settled
        );
    }

    #[test]
    fn lost_terminal_claim_never_expires_or_cancels_into_another_launch() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = registry
            .begin_client_terminal(
                namespace(),
                "native-login".into(),
                "instance".into(),
                waiter("a"),
                now,
            )
            .unwrap();
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        registry
            .accept(
                &command.ticket,
                ManagedAuthStep {
                    terminal: false,
                    readiness: AuthReadiness::LoginRequired,
                    state: None,
                    interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                        request_id: "terminal".into(),
                    }),
                },
                now,
            )
            .unwrap();
        let claim = registry
            .claim_client_terminal(
                &handle,
                &waiter("a"),
                "terminal",
                "client-a".into(),
                "claim".into(),
                now,
            )
            .unwrap();
        assert!(
            registry
                .cancel_waiter(&handle, &waiter("a"))
                .unwrap()
                .is_none()
        );
        registry.expire(now + Duration::from_secs(300));
        assert!(registry.retire(&handle).is_err());
        assert!(
            registry
                .reconcile_inactive(&handle, AuthReadiness::Ready)
                .is_err()
        );
        assert!(
            registry
                .begin_client_terminal(
                    namespace(),
                    "native-login".into(),
                    "instance".into(),
                    waiter("a"),
                    now + Duration::from_secs(300)
                )
                .is_err()
        );
        assert_eq!(
            registry.client_terminal_claim(&handle).unwrap(),
            Some(&claim)
        );
        registry.client_terminal_reaped(&claim).unwrap();
        registry
            .reconcile_inactive(&handle, AuthReadiness::LoginRequired)
            .unwrap();
        registry.retire(&handle).unwrap();
        assert!(
            registry
                .begin_client_terminal(
                    namespace(),
                    "native-login".into(),
                    "instance".into(),
                    waiter("a"),
                    now + Duration::from_secs(301)
                )
                .unwrap()
                .command
                .is_some()
        );
        let restarted =
            ManagedAuthCoordinator::new("new-boot".into(), 2, 2, Duration::from_secs(30)).unwrap();
        assert!(restarted.client_terminal_claim(&handle).is_err());
        // A new boot still requires the caller's durable namespace/issued-claim fence;
        // this ephemeral registry never fabricates proof the prior child was reaped.
    }

    #[test]
    fn native_namespace_sharing_rejects_different_initiating_instances() {
        let now = Instant::now();
        let mut registry = registry();
        let first = registry
            .begin_or_join_owned(
                namespace(),
                AuthCustodyMode::NativeManaged,
                "native-login".into(),
                "instance-a".into(),
                waiter("a"),
                now,
            )
            .unwrap();
        let second = registry
            .begin_or_join_owned(
                namespace(),
                AuthCustodyMode::NativeManaged,
                "native-login".into(),
                "instance-a".into(),
                waiter("b"),
                now,
            )
            .unwrap();
        assert_eq!(first.snapshot.handle, second.snapshot.handle);
        assert!(second.command.is_none());
        let mismatch = registry.begin_or_join_owned(
            namespace(),
            AuthCustodyMode::NativeManaged,
            "native-login".into(),
            "instance-b".into(),
            waiter("b"),
            now,
        );
        assert!(matches!(mismatch, Err(CoordinationError::Conflict)));
        assert!(
            registry
                .check_native_owner(&first.snapshot.handle, "instance-b")
                .is_err()
        );
        let command = first.command.unwrap();
        assert_eq!(command.native_owner, "instance-a");
        registry
            .accept(&command.ticket, pending("one"), now)
            .unwrap();
        assert!(
            registry
                .cancel_waiter(&first.snapshot.handle, &waiter("a"))
                .unwrap()
                .is_none()
        );
        let cancel = registry
            .cancel_waiter(&first.snapshot.handle, &waiter("b"))
            .unwrap()
            .unwrap();
        assert_eq!(cancel.native_owner, "instance-a");
    }

    #[test]
    fn identical_pending_handoff_polls_are_idempotent_but_changed_payloads_are_not() {
        let now = Instant::now();
        let mut registry = registry();
        let first = begin(&mut registry, "a", now);
        let command = first.command.unwrap();
        let handle = first.snapshot.handle;
        let handoff = || ManagedAuthStep {
            terminal: false,
            readiness: AuthReadiness::LoginRequired,
            state: None,
            interaction: Some(SensitiveAuthInteraction::NativeHandoff {
                request_id: "native-one".into(),
            }),
        };
        registry.accept(&command.ticket, handoff(), now).unwrap();
        for _ in 0..70 {
            let poll = registry
                .continue_flow(&handle, &waiter("a"), None, now)
                .unwrap();
            let snapshot = registry.accept(&poll.ticket, handoff(), now).unwrap();
            assert_eq!(snapshot.phase, ManagedFlowPhase::Pending);
            assert!(snapshot.needs_sensitive_interaction);
        }
        let poll = registry
            .continue_flow(&handle, &waiter("a"), None, now)
            .unwrap();
        let changed = ManagedAuthStep {
            terminal: false,
            readiness: AuthReadiness::LoginRequired,
            state: None,
            interaction: Some(SensitiveAuthInteraction::Browser {
                request_id: "native-one".into(),
                url: "https://example.test/changed".into(),
                user_code: None,
            }),
        };
        assert!(registry.accept(&poll.ticket, changed, now).is_err());
        assert_eq!(
            registry.snapshot(&handle, &waiter("a")).unwrap().phase,
            ManagedFlowPhase::Uncertain
        );
    }

    #[test]
    fn maintenance_cannot_retire_settled_flow_before_owner_acknowledges() {
        let now = Instant::now();
        let mut registry = registry();
        let joined = begin(&mut registry, "owner", now);
        let command = joined.command.unwrap();
        let handle = joined.snapshot.handle;
        let settled = registry
            .accept(&command.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        assert_eq!(settled.phase, ManagedFlowPhase::Settled);
        // Same sequence as the daemon's periodic lifecycle sweep between Continue and ACK.
        for item in registry.lifecycle_page(None, 16).unwrap().items {
            assert!(matches!(
                registry.retire(&item.ticket.handle),
                Err(CoordinationError::Busy)
            ));
        }
        assert_eq!(
            registry.snapshot(&handle, &waiter("owner")).unwrap(),
            settled
        );
        registry
            .acknowledge_terminal(&handle, &waiter("owner"))
            .unwrap();
        assert!(registry.snapshot(&handle, &waiter("owner")).is_err());
    }

    #[test]
    fn terminal_acknowledgment_preserves_other_waiters_then_releases_namespace() {
        let now = Instant::now();
        let mut registry = registry();
        let first = begin(&mut registry, "a", now);
        let command = first.command.unwrap();
        let handle = first.snapshot.handle;
        begin(&mut registry, "b", now);
        assert!(
            registry
                .acknowledge_terminal(&handle, &waiter("a"))
                .is_err()
        );
        registry
            .accept(&command.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        registry
            .acknowledge_terminal(&handle, &waiter("a"))
            .unwrap();
        assert!(registry.snapshot(&handle, &waiter("a")).is_err());
        assert_eq!(
            registry.snapshot(&handle, &waiter("b")).unwrap().phase,
            ManagedFlowPhase::Settled
        );
        registry
            .acknowledge_terminal(&handle, &waiter("b"))
            .unwrap();
        assert!(registry.snapshot(&handle, &waiter("b")).is_err());
        let next = begin(&mut registry, "b", now);
        assert_ne!(next.snapshot.handle, handle);
        assert!(next.command.is_some());
    }

    #[test]
    fn maintenance_is_bounded_and_uncertainty_does_not_expire_into_replay() {
        let now = Instant::now();
        let mut registry = registry();
        let first = begin(&mut registry, "a", now);
        let command = first.command.unwrap();
        let mut other_namespace = namespace();
        other_namespace.credential_namespace = "second-home".into();
        let second = registry
            .begin_or_join_owned(
                other_namespace,
                AuthCustodyMode::NativeManaged,
                "native-login".into(),
                "instance-b".into(),
                waiter("b"),
                now,
            )
            .unwrap();
        let second_command = second.command.unwrap();
        registry
            .accept(&second_command.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        registry.mark_uncertain(&command.ticket).unwrap();
        let page = registry.lifecycle_page(None, 1).unwrap();
        assert_eq!(page.items.len(), 1);
        assert!(page.next_after_flow_id.is_some());
        let rest = registry.lifecycle_page(page.next_after_flow_id, 1).unwrap();
        assert_eq!(rest.items.len(), 1);
        assert!(rest.next_after_flow_id.is_none());
        assert!(!format!("{page:?}").contains("private-state"));
        assert!(registry.lifecycle_page(None, 0).is_err());
        let mut tampered = page.items[0].clone();
        tampered.ticket.step += 1;
        assert!(
            registry
                .reconcile_item_inactive(&tampered, AuthReadiness::Unknown)
                .is_err()
        );
        registry.expire(now + Duration::from_secs(300));
        assert!(
            registry
                .snapshot(&second.snapshot.handle, &waiter("b"))
                .is_err()
        );
        assert_eq!(
            registry
                .snapshot(&first.snapshot.handle, &waiter("a"))
                .unwrap()
                .phase,
            ManagedFlowPhase::Uncertain
        );
        assert!(registry.retire(&first.snapshot.handle).is_err());
        registry
            .reconcile_item_inactive(&page.items[0], AuthReadiness::LoginRequired)
            .unwrap();
        assert!(
            registry
                .reconcile_item_inactive(&page.items[0], AuthReadiness::Verified)
                .is_err()
        );
        registry.expire(now + Duration::from_secs(301));
        assert!(
            registry
                .snapshot(&first.snapshot.handle, &waiter("a"))
                .is_err()
        );
        assert!(
            begin(&mut registry, "a", now + Duration::from_secs(301))
                .command
                .is_some()
        );
    }

    #[test]
    fn allowances_are_bounded_and_terminal_readiness_is_not_invented() {
        let now = Instant::now();
        let mut registry =
            ManagedAuthCoordinator::new("boot".into(), 1, 1, Duration::from_secs(30)).unwrap();
        let joined = begin(&mut registry, "a", now);
        let command = joined.command.unwrap();
        assert!(
            registry
                .begin_or_join(
                    namespace(),
                    AuthCustodyMode::NativeManaged,
                    "native-login".into(),
                    waiter("b"),
                    now
                )
                .is_err()
        );
        let snapshot = registry
            .accept(&command.ticket, terminal(AuthReadiness::Ready), now)
            .unwrap();
        assert_eq!(snapshot.phase, ManagedFlowPhase::Settled);
        assert_eq!(snapshot.readiness, AuthReadiness::Ready);
        assert!(
            registry
                .accept(&command.ticket, terminal(AuthReadiness::Verified), now)
                .is_err()
        );
        assert!(ManagedAuthCoordinator::new("boot".into(), 0, 1, Duration::from_secs(30)).is_err());
    }
}
