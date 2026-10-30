//! Session-owned durable controller orchestration, shared by terminal and browser.
//!
//! The canonical adapter is deliberately separate from pending-exchange caches. It
//! must use exact indexed reads and the session actor's execution-fenced transaction.
//! No adapter may reconstruct authority by scanning events or accepting a client binding.

use std::time::Duration;

use bcode_plugin::interaction_controller::{
    complete_durable_controller, export_durable_controller, restore_durable_controller,
    validate_controller_size,
};
use bcode_plugin::{PluginInvocationScope, PluginRuntimeHost};
use bcode_plugin_models::interaction_controller::{
    DURABLE_INTERACTION_VERSION, DurableInteractionBinding, DurableInteractionInput,
    DurableInteractionReceipt, DurableInteractionView, INTERACTION_CONTROLLER_INTERFACE,
    INTERACTION_CONTROLLER_VERSION, InteractionControllerError, InteractionControllerRequest,
    InteractionControllerSnapshot, InteractionControllerTransition, InteractionOutput,
    ProducerInteractionCheckpoint,
};
use bcode_plugin_sdk::ServiceCancellation;
use bcode_session_models::{
    DurableInteractionKey, DurableInteractionOperation, DurableInteractionPayload,
    DurableInteractionRecord, DurableInteractionTransition, SessionId, ToolExchangeResolution,
    ToolExchangeResolutionEvent, ToolInvocationResultRecord,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};

/// Enable only alongside the coordinator's writer/schema compatibility upgrade.
pub const PUBLICATION_ENABLED: bool = false;
const ENVELOPE: &str = "bcode.host.durable-controller";
const OPEN_OPERATION: &str = "host:open";
const CANCEL_OPERATION: &str = "host:cancel-recovered:v1";

/// Exact canonical original invocation/exchange, read under current execution ownership.
/// The adapter obtains every field independently of the consumer-supplied binding.
pub struct CanonicalSource {
    pub binding: DurableInteractionBinding,
    pub exchange_request_event_sequence: u64,
    pub exchange_payload: Value,
    /// Latest canonical presentation retained with the original invocation result.
    pub presentation: Option<bcode_tool::ToolPresentationUpdate>,
}

/// An exact canonical operation and its transaction receipt; never a latest-view alias.
pub struct RetainedOperation {
    pub operation: DurableInteractionOperation,
    pub receipt: bcode_session_models::DurableInteractionReceipt,
}

impl RetainedOperation {
    fn validate(&self) -> Result<CommittedExport, InteractionControllerError> {
        self.operation
            .validate()
            .map_err(|_| InteractionControllerError::InvalidContract)?;
        let export = operation_export(&self.operation)?;
        if self.receipt.version != bcode_session_models::DURABLE_INTERACTION_VERSION
            || self.operation.key.turn_id != export.view.binding.turn_id
            || self.operation.key.invocation_id != export.view.binding.invocation_id
            || self.operation.key.interaction_id != export.view.binding.exchange_id
            || self.operation.producer_plugin_id != export.view.binding.capability.route.producer_id
            || self.operation.request_event_sequence != export.view.binding.request_sequence
            || self.receipt.key != self.operation.key
            || self.receipt.operation_id != self.operation.operation_id
            || self.receipt.revision != export.view.revision
            || self.receipt.event_sequence == 0
            || self.operation.expected_revision.unwrap_or(0).checked_add(1)
                != Some(self.receipt.revision)
            || self.receipt.tool_result_event_sequence.is_some()
                != terminal(&export.view.snapshot.output)
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(export)
    }
}

impl CanonicalSource {
    fn validate_record(
        &self,
        record: &DurableInteractionRecord,
    ) -> Result<(), InteractionControllerError> {
        if record.version != bcode_session_models::DURABLE_INTERACTION_VERSION
            || record.key != self.key()
            || record.receipt.key != self.key()
            || record.producer_plugin_id != self.binding.capability.route.producer_id
            || record.request_event_sequence != self.binding.request_sequence
            || record.exchange_request_event_sequence != self.exchange_request_event_sequence
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(())
    }

    fn key(&self) -> DurableInteractionKey {
        DurableInteractionKey {
            turn_id: self.binding.turn_id.clone(),
            invocation_id: self.binding.invocation_id.clone(),
            interaction_id: self.binding.exchange_id.clone(),
        }
    }
}

/// Required canonical session integration surface. Reads are bounded, non-replaying and exact.
///
/// `source` proves original session/turn/invocation/exchange/producer/schema/request
/// digest and current execution ownership, including after terminal completion.
/// `operation` returns the original operation, not the latest interaction record.
/// `commit` rechecks that same source and execution ownership inside the session CAS transaction,
/// fences retries with `request_digest`, and returns the retained winning operation.
/// A Complete transaction includes outcome, original exchange resolution and original
/// invocation result. No tool execution or separate terminal publication is permitted.
pub trait DurableInteractionStore: Sync {
    fn source(
        &self,
        session_id: SessionId,
        binding: &DurableInteractionBinding,
    ) -> impl std::future::Future<Output = Result<CanonicalSource, InteractionControllerError>> + Send;

    fn latest(
        &self,
        session_id: SessionId,
        key: &DurableInteractionKey,
    ) -> impl std::future::Future<
        Output = Result<Option<DurableInteractionRecord>, InteractionControllerError>,
    > + Send;

    fn operation(
        &self,
        session_id: SessionId,
        key: &DurableInteractionKey,
        operation_id: &str,
    ) -> impl std::future::Future<
        Output = Result<Option<RetainedOperation>, InteractionControllerError>,
    > + Send;

    fn commit(
        &self,
        session_id: SessionId,
        source: &CanonicalSource,
        operation: DurableInteractionOperation,
    ) -> impl std::future::Future<Output = Result<RetainedOperation, InteractionControllerError>> + Send;
}

/// Production canonical adapter. Sequence locators select records, never prove identity.
/// Initial admission needs the exact exchange event sequence from its canonical writer;
/// subsequent admission obtains it from the retained durable interaction record.
pub struct SessionDurableInteractionStore {
    state: std::sync::Arc<crate::ServerState>,
    initial_exchange_sequence: Option<u64>,
    recovered: Option<std::sync::Arc<RecoveredTurnBinding>>,
}

/// Admission owner for one canonical recovered turn. This reserves the existing
/// turn-admission boundary and persistent session lease; it is not a new turn.
#[derive(Debug)]
pub struct RecoveredTurnBinding {
    session_id: SessionId,
    key: DurableInteractionKey,
    _admission: tokio::sync::OwnedMutexGuard<()>,
    _ownership: bcode_session::SessionOwnershipGuard,
    revoked: std::sync::atomic::AtomicBool,
    commit_boundary: std::sync::Arc<tokio::sync::Mutex<()>>,
}

impl RecoveredTurnBinding {
    /// Original turn retained by this admission owner, never a replacement turn.
    #[must_use]
    pub const fn turn_id(&self) -> &str {
        self.key.turn_id.as_str()
    }

    fn validate(
        &self,
        session_id: SessionId,
        key: &DurableInteractionKey,
    ) -> Result<(), InteractionControllerError> {
        if self.session_id != session_id
            || self.key != *key
            || self.revoked.load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        Ok(())
    }
}

/// Cancel only an explicitly user-cancelled recovered turn, by its exact registry owner.
/// No detach/shutdown caller may use this path. No legacy invocation is resolved here.
/// The coordinator must retain the owner through this call, then install original-turn
/// terminalization, revoke/remove that same owner and release admission before entering
/// the existing interrupted-turn admission boundary. Errors preserve registry ownership.
///
/// # Errors
/// Ambiguous owners, mismatched turns, stale authority, invalid producer cancellation,
/// storage uncertainty and conflicting retries preserve the original unresolved work.
pub async fn cancel_recovered(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    expected_turn_id: &str,
) -> Result<Option<CommittedCompletion>, InteractionControllerError> {
    if !PUBLICATION_ENABLED {
        return Ok(None);
    }
    let owner = {
        let owners = state.recovered_interaction_owners.lock().await;
        let (session, owner) = {
            let mut candidates = owners
                .range((session_id, String::new())..)
                .take_while(|((session, _), _)| *session == session_id);
            let Some(((session, invocation), owner)) = candidates.next() else {
                return Ok(None);
            };
            if candidates.next().is_some()
                || owner.turn_id() != expected_turn_id
                || owner.key.invocation_id != *invocation
            {
                return Err(InteractionControllerError::Conflict);
            }
            drop(candidates);
            (session, owner)
        };
        owner.validate(*session, &owner.key)?;
        std::sync::Arc::clone(owner)
    };
    let store = SessionDurableInteractionStore::from_recovered(state, &owner);
    let key = owner.key.clone();
    drop(owner);
    let record = store
        .latest(session_id, &key)
        .await?
        .ok_or(InteractionControllerError::InvalidContract)?;
    let export = CommittedExport::read(record.outcome.as_ref().unwrap_or(&record.checkpoint))?;
    let source = Box::pin(store.source(session_id, &export.view.binding)).await?;
    if source.key() != key {
        return Err(InteractionControllerError::Conflict);
    }
    source.validate_record(&record)?;
    store.execution(session_id).await?;
    // Terminal outcomes are absorbing, including an answer that won the cancel race.
    // Read its exact original result, never ask the producer to Complete a second time.
    if record.outcome.is_none() {
        let cancellation = ServiceCancellation::default();
        let controller = DurableController {
            store: &store,
            plugins: &state.plugins,
            session_id,
            scope: PluginInvocationScope::session(session_id.to_string())
                .with_turn_id(expected_turn_id),
            cancellation: &cancellation,
        };
        let retained = store.operation(session_id, &key, CANCEL_OPERATION).await?;
        let input = if let Some(retained) = retained {
            let input = retained
                .validate()?
                .input
                .ok_or(InteractionControllerError::InvalidContract)?;
            if input != recovered_cancel_input(&source.binding, input.operation.expected_revision) {
                return Err(InteractionControllerError::Conflict);
            }
            input
        } else {
            let view = Box::pin(controller.open(&source.binding)).await?;
            recovered_cancel_input(&source.binding, view.revision)
        };
        let receipt = Box::pin(controller.input(&input)).await?;
        drop(controller);
        if receipt.view.snapshot.output != InteractionOutput::Cancelled {
            return Err(InteractionControllerError::InvalidContract);
        }
    }
    Box::pin(store.recovered_completion()).await.map(Some)
}

fn recovered_cancel_input(
    binding: &DurableInteractionBinding,
    revision: u64,
) -> DurableInteractionInput {
    DurableInteractionInput {
        version: DURABLE_INTERACTION_VERSION,
        binding: binding.clone(),
        operation: bcode_plugin_models::interaction_controller::DurableInteractionOperation {
            operation_id: CANCEL_OPERATION.into(),
            expected_revision: revision,
        },
        input: bcode_plugin_models::interaction_controller::InteractionInput::Cancel,
    }
}

/// Claim the original waiting turn's admission/ownership boundary after restart.
/// Retain `recovered_binding()` through answer CAS and original model continuation;
/// request-local stores may be dropped without releasing that retained owner.
/// No model request, turn event, or tool is created. A second invocation in the same
/// turn currently conflicts on admission; shared multi-invocation recovery is not
/// supported and must preserve/defer that work rather than resolve it.
/// Coordinator cancellation must call `revoke_recovered` before releasing this owner.
pub async fn claim_recovered_wait(
    state: &std::sync::Arc<crate::ServerState>,
    proof: &CommittedWait,
) -> Result<SessionDurableInteractionStore, InteractionControllerError> {
    require_publication()?;
    let admission = crate::turn_admission_lock(state, proof.session_id)
        .await
        .try_lock_owned()
        .map_err(|_| InteractionControllerError::Conflict)?;
    let ownership = state
        .sessions
        .acquire_session_ownership(
            proof.session_id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|_| InteractionControllerError::Unauthorized)?;
    // Never displace a live original or newer runtime turn.
    if state.session_current_turn(proof.session_id).await.is_some() {
        return Err(InteractionControllerError::Conflict);
    }
    validate_wait_source(state, proof).await?;
    let recovered = RecoveredTurnBinding {
        session_id: proof.session_id,
        key: proof.record.key.clone(),
        _admission: admission,
        _ownership: ownership,
        revoked: std::sync::atomic::AtomicBool::new(false),
        commit_boundary: std::sync::Arc::new(tokio::sync::Mutex::new(())),
    };
    Ok(SessionDurableInteractionStore {
        state: std::sync::Arc::clone(state),
        initial_exchange_sequence: Some(proof.record.exchange_request_event_sequence),
        recovered: Some(std::sync::Arc::new(recovered)),
    })
}

impl SessionDurableInteractionStore {
    #[must_use]
    pub fn new(
        state: &std::sync::Arc<crate::ServerState>,
        initial_exchange_sequence: Option<u64>,
    ) -> Self {
        Self {
            state: std::sync::Arc::clone(state),
            initial_exchange_sequence,
            recovered: None,
        }
    }

    /// Return only the admission/lease owner for the coordinator registry. The
    /// binding contains no `ServerState` `Arc`, so retaining it cannot form a state cycle.
    #[must_use]
    pub fn recovered_binding(&self) -> Option<std::sync::Arc<RecoveredTurnBinding>> {
        self.recovered.clone()
    }

    /// Reconstruct a request-local store from a previously canonically claimed owner.
    /// Full session/invocation identity and revocation are checked on each operation.
    #[must_use]
    pub fn from_recovered(
        state: &std::sync::Arc<crate::ServerState>,
        binding: &std::sync::Arc<RecoveredTurnBinding>,
    ) -> Self {
        Self {
            state: std::sync::Arc::clone(state),
            initial_exchange_sequence: None,
            recovered: Some(std::sync::Arc::clone(binding)),
        }
    }

    /// Close recovered admission only after canonical cancellation/completion and
    /// coordinator terminalization are installed. Never revoke before answer CAS.
    /// Existing actor-held commit guards observe revocation; no answer is invented.
    pub async fn revoke_recovered(&self) {
        if let Some(binding) = &self.recovered {
            binding
                .revoked
                .store(true, std::sync::atomic::Ordering::SeqCst);
            let _settled = binding.commit_boundary.lock().await;
        }
    }

    /// Read the one already-committed original result while retaining recovered
    /// admission. The coordinator feeds this into the original turn continuation,
    /// never dispatches the tool and never appends a second result/resolution.
    pub async fn recovered_completion(
        &self,
    ) -> Result<CommittedCompletion, InteractionControllerError> {
        let _boundary = self
            .recovered
            .as_ref()
            .ok_or(InteractionControllerError::Unauthorized)?
            .commit_boundary
            .lock()
            .await;
        let (session_id, key) = self
            .recovered
            .as_ref()
            .map(|owner| (owner.session_id, owner.key.clone()))
            .ok_or(InteractionControllerError::Unauthorized)?;
        self.recovered
            .as_ref()
            .ok_or(InteractionControllerError::Unauthorized)?
            .validate(session_id, &key)?;
        let execution = self.execution(session_id).await?;
        let fence = if let Some(provenance) = execution {
            let authority = self.verify_workflow(session_id, &provenance)?;
            Some(
                self.state
                    .workflow_store
                    .lock()
                    .map_err(|_| InteractionControllerError::Unavailable)?
                    .execution_fence(&provenance.run_id, &authority)
                    .map_err(|_| InteractionControllerError::Unauthorized)?,
            )
        } else {
            None
        };
        let completion =
            committed_completion_for_invocation(&self.state, session_id, &key.invocation_id)
                .await?
                .ok_or(InteractionControllerError::NotPending)?;
        if completion.view.binding.turn_id != key.turn_id
            || completion.view.binding.exchange_id != key.interaction_id
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        if let Some(fence) = &fence {
            fence
                .validate()
                .map_err(|_| InteractionControllerError::Unauthorized)?;
        }
        self.recovered
            .as_ref()
            .ok_or(InteractionControllerError::Unauthorized)?
            .validate(session_id, &key)?;
        drop(fence);
        Ok(completion)
    }

    async fn execution(
        &self,
        session_id: SessionId,
    ) -> Result<Option<bcode_session_models::ExecutionSessionProvenance>, InteractionControllerError>
    {
        let summary = self
            .state
            .sessions
            .session_summary(session_id)
            .await
            .map_err(|_| InteractionControllerError::Unavailable)?;
        let Some(execution) = summary.execution else {
            return Ok(None);
        };
        let provenance = execution.provenance;
        if provenance.version != bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION
            || provenance.owner != "bcode.workflow"
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        self.verify_workflow(session_id, &provenance)?;
        Ok(Some(provenance))
    }

    // No claim, transfer, or fallback: only the exact already-owned run is admissible.
    fn verify_workflow(
        &self,
        session_id: SessionId,
        provenance: &bcode_session_models::ExecutionSessionProvenance,
    ) -> Result<bcode_workflow_store::WorkflowExecutionAuthority, InteractionControllerError> {
        let activation = provenance
            .activation_id
            .as_deref()
            .ok_or(InteractionControllerError::Unauthorized)?;
        let store = self
            .state
            .workflow_store
            .lock()
            .map_err(|_| InteractionControllerError::Unavailable)?;
        let link = store
            .execution_session_link(
                &provenance.run_id,
                &provenance.node_id,
                activation,
                provenance.attempt,
            )
            .map_err(|_| InteractionControllerError::Unavailable)?
            .ok_or(InteractionControllerError::Unauthorized)?;
        if link.session_id != session_id.to_string()
            || Some(link.workspace_snapshot.as_str()) != provenance.workspace_snapshot.as_deref()
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        let authority = store
            .execution_authority(&provenance.run_id)
            .map_err(|_| InteractionControllerError::Unavailable)?
            .ok_or(InteractionControllerError::Unauthorized)?;
        if authority.daemon_instance_id != self.state.daemon_status.instance_id
            || authority.target_artifact_id
                != crate::workflow_operations::current_artifact_id(&self.state)
        {
            return Err(InteractionControllerError::Unauthorized);
        }
        store
            .verify_execution_authority(&provenance.run_id, &authority)
            .map_err(|_| InteractionControllerError::Unauthorized)?;
        drop(store);
        Ok(authority)
    }

    async fn admit_source(
        &self,
        session_id: SessionId,
        requested: &DurableInteractionBinding,
    ) -> Result<Option<DurableInteractionRecord>, InteractionControllerError> {
        requested.validate()?;
        if requested.session_id != session_id.to_string() {
            return Err(InteractionControllerError::Unauthorized);
        }
        let key = DurableInteractionKey {
            turn_id: requested.turn_id.clone(),
            invocation_id: requested.invocation_id.clone(),
            interaction_id: requested.exchange_id.clone(),
        };
        if let Some(binding) = &self.recovered {
            binding.validate(session_id, &key)?;
        }
        let existing = self.latest(session_id, &key).await?;
        if self.recovered.is_some() {
            let record = existing
                .as_ref()
                .ok_or(InteractionControllerError::NotPending)?;
            if record.outcome.is_none() {
                validate_wait_source(
                    &self.state,
                    &CommittedWait {
                        session_id,
                        record: record.clone(),
                    },
                )
                .await?;
            }
        }
        Ok(existing)
    }

    async fn validate_pending_source(
        &self,
        session_id: SessionId,
        source: &CanonicalSource,
    ) -> Result<(), InteractionControllerError> {
        let pending = self.state.pending_tool_exchanges.lock().await;
        let exchange = pending
            .get(&source.binding.exchange_id)
            .ok_or(InteractionControllerError::NotPending)?;
        if exchange.summary.session_id != session_id
            || exchange.summary.request.invocation_id != source.binding.invocation_id
            || exchange.summary.request.producer_id != source.binding.capability.route.producer_id
            || exchange.summary.request.schema != source.binding.capability.route.exchange_schema
            || exchange.summary.request.schema_version != source.binding.exchange_schema_version
            || exchange.summary.request.payload != source.exchange_payload
        {
            return Err(InteractionControllerError::Conflict);
        }
        drop(pending);
        Ok(())
    }

    async fn validate_commit_source(
        &self,
        session_id: SessionId,
        source: &CanonicalSource,
        operation: &DurableInteractionOperation,
    ) -> Result<(), InteractionControllerError> {
        // Re-read source after producer work; never hash the checkpoint as the caller request.
        let facts = self
            .state
            .sessions
            .durable_interaction_facts(
                session_id,
                source.binding.request_sequence,
                source.exchange_request_event_sequence,
            )
            .await
            .map_err(|_| InteractionControllerError::InvalidContract)?;
        if facts
            .source_digest()
            .map_err(|_| InteractionControllerError::InvalidContract)?
            != source.binding.request_digest
            || operation.key != source.key()
            || operation.request_event_sequence != source.binding.request_sequence
            || operation.exchange_request_event_sequence != source.exchange_request_event_sequence
            || operation.producer_plugin_id != source.binding.capability.route.producer_id
        {
            return Err(InteractionControllerError::Conflict);
        }
        Ok(())
    }

    async fn commit_turn(
        &self,
        session_id: SessionId,
        source: &CanonicalSource,
    ) -> Result<
        Option<tokio::sync::OwnedMutexGuard<Option<crate::RuntimeCurrentTurn>>>,
        InteractionControllerError,
    > {
        if let Some(binding) = &self.recovered {
            binding.validate(session_id, &source.key())?;
            let proof = CommittedWait {
                session_id,
                record: self
                    .latest(session_id, &source.key())
                    .await?
                    .ok_or(InteractionControllerError::NotPending)?,
            };
            validate_wait_source(&self.state, &proof).await?;
            Ok(None)
        } else {
            Ok(Some(
                self.current_turn(session_id, &source.binding.turn_id)
                    .await?,
            ))
        }
    }

    fn retained_presentation(
        &self,
        session_id: SessionId,
        invocation_id: &str,
    ) -> Result<Option<bcode_tool::ToolPresentationUpdate>, InteractionControllerError> {
        Ok(self
            .state
            .active_presentation_updates
            .lock()
            .map_err(|_| InteractionControllerError::Unavailable)?
            .get(&(
                session_id,
                invocation_id.to_owned(),
                bcode_tool::ToolPresentationIdentity::Primary,
            ))
            .filter(|update| {
                update.retention == bcode_tool::ToolPresentationRetention::RetainLatest
            })
            .cloned())
    }

    async fn current_turn(
        &self,
        session_id: SessionId,
        turn_id: &str,
    ) -> Result<
        tokio::sync::OwnedMutexGuard<Option<crate::RuntimeCurrentTurn>>,
        InteractionControllerError,
    > {
        let current = self
            .state
            .session_runtimes
            .lock()
            .await
            .get(&session_id)
            .map(|runtime| std::sync::Arc::clone(&runtime.current_turn))
            .ok_or(InteractionControllerError::NotPending)?;
        let current = current.lock_owned().await;
        if !current.as_ref().is_some_and(|turn| {
            turn.turn_id == turn_id
                && turn.kind == crate::RuntimeOperationKind::ModelTurn
                && !turn.cancel_state.is_cancelled()
        }) {
            return Err(InteractionControllerError::NotPending);
        }
        Ok(current)
    }
}

/// The live turn cannot be replaced while this guard is retained by the session actor.
/// Workflow authority additionally remains reserved cross-process by its DB transaction.
#[derive(Debug)]
struct InteractionCommitGuard {
    turn: Option<tokio::sync::OwnedMutexGuard<Option<crate::RuntimeCurrentTurn>>>,
    recovered: Option<std::sync::Arc<RecoveredTurnBinding>>,
    recovered_commit: Option<tokio::sync::OwnedMutexGuard<()>>,
    turn_id: String,
    workflow: Option<bcode_workflow_store::WorkflowExecutionFence>,
}

impl bcode_session::DurableInteractionCommitGuard for InteractionCommitGuard {
    fn validate(&self) -> Result<(), bcode_session::SessionError> {
        let denied = || {
            bcode_session::SessionError::EventSerialization(
                "durable interaction execution authority unavailable".into(),
            )
        };
        match (&self.turn, &self.recovered) {
            (Some(turn), None)
                if (**turn).as_ref().is_some_and(|turn| {
                    turn.turn_id == self.turn_id
                        && turn.kind == crate::RuntimeOperationKind::ModelTurn
                        && !turn.cancel_state.is_cancelled()
                }) => {}
            (None, Some(binding))
                if self.recovered_commit.is_some()
                    && binding.key.turn_id == self.turn_id
                    && !binding.revoked.load(std::sync::atomic::Ordering::SeqCst) => {}
            _ => return Err(denied()),
        }
        if let Some(fence) = &self.workflow {
            fence.validate().map_err(|_| denied())?;
        }
        Ok(())
    }
}

impl DurableInteractionStore for SessionDurableInteractionStore {
    async fn source(
        &self,
        session_id: SessionId,
        requested: &DurableInteractionBinding,
    ) -> Result<CanonicalSource, InteractionControllerError> {
        let existing = self.admit_source(session_id, requested).await?;
        let facts = if let Some(exchange_sequence) = existing
            .as_ref()
            .map(|record| record.exchange_request_event_sequence)
            .or(self.initial_exchange_sequence)
        {
            self.state
                .sessions
                .durable_interaction_facts(
                    session_id,
                    requested.request_sequence,
                    exchange_sequence,
                )
                .await
        } else {
            self.state
                .sessions
                .durable_interaction_source(
                    session_id,
                    requested.invocation_id.clone(),
                    requested.exchange_id.clone(),
                )
                .await
        }
        .map_err(|_| InteractionControllerError::InvalidContract)?;
        let source_digest = facts
            .source_digest()
            .map_err(|_| InteractionControllerError::InvalidContract)?;
        let bcode_session_models::SessionEventKind::PositionedToolCallRequested {
            turn_id,
            tool_call_id,
            producer_plugin_id: Some(producer),
            tool_name,
            ..
        } = &facts.invocation.kind
        else {
            return Err(InteractionControllerError::InvalidContract);
        };
        let bcode_session_models::SessionEventKind::ToolExchangeRequested { request } =
            &facts.exchange.kind
        else {
            return Err(InteractionControllerError::InvalidContract);
        };
        self.execution(session_id).await?;
        if existing.is_none() {
            // Initial admission requires live invocation ownership. Existing canonical
            // checkpoints may be inspected/restored before runtime reconstruction;
            // commit still requires an owned original-turn continuation guard.
            drop(self.current_turn(session_id, turn_id).await?);
            // Initial admission precedes registry visibility. Exact source facts plus
            // the original live turn are checked again under the commit guard.
        }
        let cancellation = ServiceCancellation::default();
        let capability = bcode_plugin::interaction_controller::discover_durable_controller(
            &self.state.plugins,
            producer,
            &request.schema,
            request.schema_version,
            PluginInvocationScope::session(session_id.to_string()).with_turn_id(turn_id),
            &cancellation,
        )
        .await?;
        let binding = DurableInteractionBinding {
            session_id: session_id.to_string(),
            turn_id: turn_id.clone(),
            invocation_id: tool_call_id.clone(),
            request_sequence: facts.invocation.sequence,
            exchange_id: request.exchange_id.clone(),
            tool_name: tool_name.clone(),
            request_digest: source_digest,
            exchange_schema_version: request.schema_version,
            capability,
        };
        if binding != *requested {
            return Err(InteractionControllerError::Conflict);
        }
        let source = CanonicalSource {
            binding,
            exchange_request_event_sequence: facts.exchange.sequence,
            exchange_payload: request.payload.clone(),
            presentation: self.retained_presentation(session_id, tool_call_id)?,
        };
        if let Some(record) = existing {
            source.validate_record(&record)?;
        }
        Ok(source)
    }

    async fn latest(
        &self,
        session_id: SessionId,
        key: &DurableInteractionKey,
    ) -> Result<Option<DurableInteractionRecord>, InteractionControllerError> {
        self.state
            .sessions
            .durable_interaction(session_id, key.clone())
            .await
            .map_err(|_| InteractionControllerError::Unavailable)
    }

    async fn operation(
        &self,
        session_id: SessionId,
        key: &DurableInteractionKey,
        operation_id: &str,
    ) -> Result<Option<RetainedOperation>, InteractionControllerError> {
        let retained = self
            .state
            .sessions
            .durable_interaction_operation(session_id, key.clone(), operation_id.into())
            .await
            .map_err(|_| InteractionControllerError::Unavailable)?;
        let retained = retained
            .map(|retained| {
                let retained = RetainedOperation {
                    operation: retained.operation,
                    receipt: retained.receipt,
                };
                retained.validate()?;
                Ok::<_, InteractionControllerError>(retained)
            })
            .transpose()?;
        if let Some(retained) = &retained
            && matches!(
                retained.operation.transition,
                DurableInteractionTransition::Complete { .. }
            )
        {
            let completion =
                committed_completion_for_invocation(&self.state, session_id, &key.invocation_id)
                    .await?
                    .ok_or(InteractionControllerError::InvalidContract)?;
            let DurableInteractionTransition::Complete {
                result,
                exchange_resolution,
                ..
            } = &retained.operation.transition
            else {
                return Err(InteractionControllerError::InvalidContract);
            };
            if completion.receipt != retained.receipt
                || completion.result != **result
                || completion.exchange_resolution != *exchange_resolution
                || completion.view != retained.validate()?.view
            {
                return Err(InteractionControllerError::InvalidContract);
            }
        }
        Ok(retained)
    }

    async fn commit(
        &self,
        session_id: SessionId,
        source: &CanonicalSource,
        operation: DurableInteractionOperation,
    ) -> Result<RetainedOperation, InteractionControllerError> {
        self.validate_commit_source(session_id, source, &operation)
            .await?;
        let execution = self.execution(session_id).await?;
        // Share the existing semantic-controller completion boundary so an ephemeral
        // resolver cannot terminalize the original wait concurrently with this CAS.
        let _controllers = self.state.interaction_controllers.lock().await;
        if operation.expected_revision.is_some() && self.recovered.is_none() {
            self.validate_pending_source(session_id, source).await?;
        }
        let cancellation;
        // False positive: this Option retains a marker-commit mutex guard, not
        // collection data. Its lifetime fences cancellation through CAS/readback.
        #[allow(
            clippy::collection_is_never_read,
            reason = "retained cancellation mutex guard is a semantic commit fence, not a collection"
        )]
        let _commit_boundary;
        let guard: std::sync::Arc<dyn bcode_session::DurableInteractionCommitGuard> = {
            let mut guard = InteractionCommitGuard {
                turn: self.commit_turn(session_id, source).await?,
                recovered: self.recovered.clone(),
                recovered_commit: None,
                turn_id: source.binding.turn_id.clone(),
                workflow: None,
            };
            cancellation = guard
                .turn
                .as_ref()
                .and_then(|turn| (**turn).as_ref())
                .map(|turn| std::sync::Arc::clone(&turn.cancel_state));
            _commit_boundary = if let Some(cancellation) = &cancellation {
                Some(cancellation.marker_commit.lock().await)
            } else {
                None
            };
            // No global workflow Mutex survives an await. The owned fence reserves its
            // writer transaction until storage and the exact retained receipt read finish.
            guard.recovered_commit = if let Some(binding) = &self.recovered {
                Some(
                    std::sync::Arc::clone(&binding.commit_boundary)
                        .lock_owned()
                        .await,
                )
            } else {
                None
            };
            guard.workflow = if let Some(provenance) = execution {
                let authority = self.verify_workflow(session_id, &provenance)?;
                let fence = self
                    .state
                    .workflow_store
                    .lock()
                    .map_err(|_| InteractionControllerError::Unavailable)?
                    .execution_fence(&provenance.run_id, &authority)
                    .map_err(|_| InteractionControllerError::Unauthorized)?;
                Some(fence)
            } else {
                None
            };
            std::sync::Arc::new(guard)
        };
        guard
            .validate()
            .map_err(|_| InteractionControllerError::Unauthorized)?;
        let committed = self
            .state
            .sessions
            .commit_durable_interaction(
                session_id,
                operation.clone(),
                std::sync::Arc::clone(&guard),
            )
            .await
            .map_err(|_| InteractionControllerError::Conflict)?;
        let retained = self
            .operation(session_id, &operation.key, &operation.operation_id)
            .await?
            .ok_or(InteractionControllerError::Unavailable)?;
        if retained.operation != operation || retained.receipt != committed {
            return Err(InteractionControllerError::Conflict);
        }
        if let DurableInteractionTransition::Complete {
            exchange_resolution,
            ..
        } = &retained.operation.transition
        {
            // Storage already atomically appended both terminal records. This only
            // releases the original live waiter under the shared semantic boundary.
            // A recovered invocation has no original stack to wake; continuation
            // consumes the retained result through recovered_completion instead.
            crate::interaction_operations::complete_tool_exchange_under_controller_lock(
                &self.state,
                &source.binding.exchange_id,
                exchange_resolution.resolution.clone(),
            )
            .await;
        }
        drop(guard);
        Ok(retained)
    }
}

/// Canonical waiting observation, not execution authority or a tool dispatch permit.
/// Private fields ensure lifecycle callers cannot construct proofs from client snapshots.
pub struct CommittedWait {
    session_id: SessionId,
    record: DurableInteractionRecord,
}

impl CommittedWait {
    #[must_use]
    pub const fn key(&self) -> &DurableInteractionKey {
        &self.record.key
    }

    #[must_use]
    pub const fn revision(&self) -> u64 {
        self.record.receipt.revision
    }
}

/// Producer-restored semantics and exact original exchange for lifecycle registration.
/// Registering this wait must not dispatch or re-execute the original tool.
pub struct RestoredWait {
    view: DurableInteractionView,
    request: bcode_session_models::ToolExchangeRequest,
}

impl RestoredWait {
    #[must_use]
    pub const fn view(&self) -> &DurableInteractionView {
        &self.view
    }

    #[must_use]
    pub const fn request(&self) -> &bcode_session_models::ToolExchangeRequest {
        &self.request
    }
}

/// Exact already-committed terminal material for original invocation continuation.
/// The lifecycle owner consumes this instead of calling the tool or producer Complete
/// again. It must not append another resolution/result or allocate a replacement turn.
pub struct CommittedCompletion {
    pub receipt: bcode_session_models::DurableInteractionReceipt,
    pub view: DurableInteractionView,
    pub result: ToolInvocationResultRecord,
    pub exchange_resolution: ToolExchangeResolutionEvent,
}

pub async fn committed_completion_for_invocation(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    invocation_id: &str,
) -> Result<Option<CommittedCompletion>, InteractionControllerError> {
    require_publication()?;
    let Some(record) = state
        .sessions
        .durable_interaction_for_invocation(session_id, invocation_id.into())
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?
    else {
        return Ok(None);
    };
    if record.key.invocation_id != invocation_id {
        return Err(InteractionControllerError::InvalidContract);
    }
    let Some(outcome) = &record.outcome else {
        return Ok(None);
    };
    let facts = state
        .sessions
        .durable_interaction_facts(
            session_id,
            record.request_event_sequence,
            record.exchange_request_event_sequence,
        )
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    let binding = binding_from_facts(state, session_id, &facts).await?;
    let source = CanonicalSource {
        binding: binding.clone(),
        exchange_request_event_sequence: facts.exchange.sequence,
        exchange_payload: Value::Null,
        presentation: None,
    };
    source.validate_record(&record)?;
    let retained = state
        .sessions
        .durable_interaction_operation(
            session_id,
            record.key.clone(),
            record.receipt.operation_id.clone(),
        )
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?
        .ok_or(InteractionControllerError::InvalidContract)?;
    let retained = RetainedOperation {
        operation: retained.operation,
        receipt: retained.receipt,
    };
    let export = retained.validate()?;
    let stored = CommittedExport::read(outcome)?;
    if record.receipt != retained.receipt
        || export.view.binding != binding
        || export.view != stored.view
        || export.checkpoint != stored.checkpoint
        || export.input != stored.input
        || !terminal(&export.view.snapshot.output)
        || record.tool_result_event_sequence != retained.receipt.tool_result_event_sequence
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    let DurableInteractionTransition::Complete {
        result,
        exchange_resolution,
        ..
    } = retained.operation.transition
    else {
        return Err(InteractionControllerError::InvalidContract);
    };
    Ok(Some(CommittedCompletion {
        receipt: retained.receipt,
        view: export.view,
        result: *result,
        exchange_resolution,
    }))
}

const fn require_publication() -> Result<(), InteractionControllerError> {
    if !PUBLICATION_ENABLED {
        return Err(InteractionControllerError::Unavailable);
    }
    Ok(())
}

/// Prove one waiting checkpoint by exact invocation lookup before abandoned-work
/// terminalization or last-consumer detach. `None` means no durable opt-in: preserve
/// the caller's existing legacy/unknown-tool policy. Errors mean preserve-and-report,
/// never permission to terminalize work whose storage could not be verified.
pub async fn committed_wait_for_invocation(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    invocation_id: &str,
) -> Result<Option<CommittedWait>, InteractionControllerError> {
    require_publication()?;
    let record = state
        .sessions
        .durable_interaction_for_invocation(session_id, invocation_id.into())
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?;
    let Some(record) = record else {
        return Ok(None);
    };
    if record.key.invocation_id != invocation_id {
        return Err(InteractionControllerError::InvalidContract);
    }
    if record.outcome.is_some() {
        return Ok(None);
    }
    let proof = CommittedWait { session_id, record };
    validate_wait_source(state, &proof).await?;
    Ok(Some(proof))
}

/// Last-consumer detach policy for a live original exchange. Disabled publication
/// preserves legacy behavior; enabled storage/producer ambiguity must be surfaced by
/// the lifecycle owner without resolving or discarding the canonical wait.
pub async fn retain_wait_on_detach(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
) -> Result<bool, InteractionControllerError> {
    if !PUBLICATION_ENABLED {
        return Ok(false);
    }
    let Some(proof) =
        committed_wait_for_invocation(state, session_id, &request.invocation_id).await?
    else {
        return Ok(false);
    };
    let (_, original) = validate_wait_source(state, &proof).await?;
    if original != *request {
        return Err(InteractionControllerError::Conflict);
    }
    Ok(true)
}

/// Public committed discovery, including terminal outcomes. No producer Restore,
/// pending registry state, or checkpoint bytes enter this response.
pub async fn committed_views_page(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    query: bcode_plugin_models::interaction_controller::DurableInteractionQuery,
) -> Result<
    bcode_plugin_models::interaction_controller::DurableInteractionPage,
    InteractionControllerError,
> {
    query.validate()?;
    // Negotiate unsupported durable hosting only for a real canonical session.
    // Storage/admission failures must never masquerade as legacy compatibility.
    state
        .sessions
        .session_summary(session_id)
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?;
    if !PUBLICATION_ENABLED {
        return Err(InteractionControllerError::Unsupported);
    }
    let storage_query = bcode_session_models::DurableInteractionQuery {
        limit: usize::try_from(query.limit)
            .map_err(|_| InteractionControllerError::LimitExceeded)?,
        after_event_sequence: query.after_event_sequence,
        expected_generation: query.expected_generation,
    };
    let page = state
        .sessions
        .durable_interaction_page(session_id, storage_query)
        .await
        .map_err(|_| InteractionControllerError::Conflict)?;
    let mut views = Vec::with_capacity(page.entries.len());
    for locator in page.entries {
        let record = state
            .sessions
            .durable_interaction(session_id, locator.key.clone())
            .await
            .map_err(|_| InteractionControllerError::Unavailable)?
            .ok_or(InteractionControllerError::InvalidContract)?;
        if record.key != locator.key
            || record.receipt.event_sequence != locator.event_sequence
            || record.outcome.is_some() != locator.terminal
        {
            return Err(InteractionControllerError::Conflict);
        }
        let view = committed_record_view(state, session_id, &record).await?;
        views.push(view);
    }
    // Payload reads are separate bounded transactions. A final pinned read rejects
    // concurrent canonical edits rather than presenting a mixed-generation page.
    state
        .sessions
        .durable_interaction_page(
            session_id,
            bcode_session_models::DurableInteractionQuery {
                limit: 1,
                after_event_sequence: None,
                expected_generation: Some(page.generation),
            },
        )
        .await
        .map_err(|_| InteractionControllerError::Conflict)?;
    let result = bcode_plugin_models::interaction_controller::DurableInteractionPage {
        generation: page.generation,
        views,
        next_after_event_sequence: page.next_after_event_sequence,
    };
    result.validate(&session_id.to_string(), &query)?;
    Ok(result)
}

async fn committed_record_view(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    record: &DurableInteractionRecord,
) -> Result<DurableInteractionView, InteractionControllerError> {
    let facts = state
        .sessions
        .durable_interaction_source(
            session_id,
            record.key.invocation_id.clone(),
            record.key.interaction_id.clone(),
        )
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    let binding = binding_from_facts(state, session_id, &facts).await?;
    let source = CanonicalSource {
        binding: binding.clone(),
        exchange_request_event_sequence: facts.exchange.sequence,
        exchange_payload: Value::Null,
        presentation: None,
    };
    source.validate_record(record)?;
    let retained = state
        .sessions
        .durable_interaction_operation(
            session_id,
            record.key.clone(),
            record.receipt.operation_id.clone(),
        )
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?
        .ok_or(InteractionControllerError::InvalidContract)?;
    let retained = RetainedOperation {
        operation: retained.operation,
        receipt: retained.receipt,
    };
    let export = retained.validate()?;
    let stored = CommittedExport::read(record.outcome.as_ref().unwrap_or(&record.checkpoint))?;
    if retained.receipt != record.receipt
        || export.view.binding != binding
        || export.view != stored.view
        || export.checkpoint != stored.checkpoint
        || export.input != stored.input
        || terminal(&export.view.snapshot.output) != record.outcome.is_some()
        || record.tool_result_event_sequence != retained.receipt.tool_result_event_sequence
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    Ok(export.view)
}

async fn binding_from_facts(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    facts: &bcode_session::DurableInteractionFacts,
) -> Result<DurableInteractionBinding, InteractionControllerError> {
    let request_digest = facts
        .source_digest()
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    let bcode_session_models::SessionEventKind::PositionedToolCallRequested {
        turn_id,
        tool_call_id,
        producer_plugin_id: Some(producer),
        tool_name,
        ..
    } = &facts.invocation.kind
    else {
        return Err(InteractionControllerError::InvalidContract);
    };
    let bcode_session_models::SessionEventKind::ToolExchangeRequested { request } =
        &facts.exchange.kind
    else {
        return Err(InteractionControllerError::InvalidContract);
    };
    if facts.invocation.session_id != session_id || facts.exchange.session_id != session_id {
        return Err(InteractionControllerError::InvalidContract);
    }
    SessionDurableInteractionStore::new(state, None)
        .execution(session_id)
        .await?;
    let cancellation = ServiceCancellation::default();
    let capability = bcode_plugin::interaction_controller::discover_durable_controller(
        &state.plugins,
        producer,
        &request.schema,
        request.schema_version,
        PluginInvocationScope::session(session_id.to_string()).with_turn_id(turn_id),
        &cancellation,
    )
    .await?;
    let binding = DurableInteractionBinding {
        session_id: session_id.to_string(),
        turn_id: turn_id.clone(),
        invocation_id: tool_call_id.clone(),
        request_sequence: facts.invocation.sequence,
        exchange_id: request.exchange_id.clone(),
        tool_name: tool_name.clone(),
        request_digest,
        exchange_schema_version: request.schema_version,
        capability,
    };
    binding.validate()?;
    Ok(binding)
}

async fn validate_wait_source(
    state: &std::sync::Arc<crate::ServerState>,
    proof: &CommittedWait,
) -> Result<(CommittedExport, bcode_session_models::ToolExchangeRequest), InteractionControllerError>
{
    let latest = state
        .sessions
        .durable_interaction(proof.session_id, proof.record.key.clone())
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?
        .ok_or(InteractionControllerError::InvalidContract)?;
    if latest != proof.record || latest.outcome.is_some() {
        return Err(InteractionControllerError::Conflict);
    }
    // This is the indexed, bounded turn-evidence API, not an event-history scan.
    let evidence = state
        .sessions
        .session_turn_evidence(proof.session_id, &latest.key.turn_id)
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?;
    if evidence.iter().any(|event| {
        matches!(&event.kind,
        bcode_session_models::SessionEventKind::ModelTurnFinished { turn_id, .. }
            if turn_id == &latest.key.turn_id)
    }) {
        return Err(InteractionControllerError::NotPending);
    }
    let facts = state
        .sessions
        .durable_interaction_facts(
            proof.session_id,
            latest.request_event_sequence,
            latest.exchange_request_event_sequence,
        )
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    let binding = binding_from_facts(state, proof.session_id, &facts).await?;
    let export = CommittedExport::read(&latest.checkpoint)?;
    let source = CanonicalSource {
        binding: binding.clone(),
        exchange_request_event_sequence: facts.exchange.sequence,
        exchange_payload: Value::Null,
        presentation: None,
    };
    source.validate_record(&latest)?;
    if export.view.binding != binding
        || export.view.revision != latest.receipt.revision
        || terminal(&export.view.snapshot.output)
        || latest.tool_result_event_sequence.is_some()
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    let retained = state
        .sessions
        .durable_interaction_operation(
            proof.session_id,
            latest.key.clone(),
            latest.receipt.operation_id.clone(),
        )
        .await
        .map_err(|_| InteractionControllerError::Unavailable)?
        .ok_or(InteractionControllerError::InvalidContract)?;
    let retained = RetainedOperation {
        operation: retained.operation,
        receipt: retained.receipt,
    };
    let committed = retained.validate()?;
    if retained.receipt != latest.receipt
        || committed.view != export.view
        || committed.checkpoint != export.checkpoint
        || committed.input != export.input
    {
        return Err(InteractionControllerError::InvalidContract);
    }
    let bcode_session_models::SessionEventKind::ToolExchangeRequested { request } =
        facts.exchange.kind
    else {
        return Err(InteractionControllerError::InvalidContract);
    };
    Ok((export, request))
}

/// Restore a proven committed wait without requiring a transient live `current_turn`.
/// This is observation/producer restoration only. The coordinator must claim the
/// original canonical turn continuation before accepting mutations or scheduling work.
pub async fn restore_wait(
    state: &std::sync::Arc<crate::ServerState>,
    proof: &CommittedWait,
) -> Result<RestoredWait, InteractionControllerError> {
    require_publication()?;
    let (export, request) = validate_wait_source(state, proof).await?;
    let cancellation = ServiceCancellation::default();
    let restored = restore_durable_controller(
        &state.plugins,
        &export.view.binding,
        &export.checkpoint,
        PluginInvocationScope::session(proof.session_id.to_string())
            .with_turn_id(&export.view.binding.turn_id),
        &cancellation,
    )
    .await?;
    if terminal(&restored.output) {
        return Err(InteractionControllerError::InvalidContract);
    }
    // A concurrent canonical edit during producer work invalidates this registration proposal.
    validate_wait_source(state, proof).await?;
    let mut view = export.view;
    view.snapshot.snapshot = restored.snapshot;
    view.snapshot.output = restored.output;
    view.validate()?;
    Ok(RestoredWait { view, request })
}

/// Commit the initial explicit producer export before original tool readiness.
/// The canonical exchange must already be appended. Initial admission commits
/// before registry insertion or notification under its original live turn. Only Unsupported permits legacy fallback;
/// every other error must prevent advertising a durable-ready interaction.
pub async fn checkpoint_before_ready(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    invocation_sequence: u64,
    exchange_sequence: u64,
) -> Result<DurableInteractionView, InteractionControllerError> {
    require_publication()?;
    let facts = state
        .sessions
        .durable_interaction_facts(session_id, invocation_sequence, exchange_sequence)
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    let binding = binding_from_facts(state, session_id, &facts).await?;
    let store = SessionDurableInteractionStore::new(state, Some(exchange_sequence));
    let cancellation = ServiceCancellation::default();
    DurableController {
        store: &store,
        plugins: &state.plugins,
        session_id,
        scope: PluginInvocationScope::session(session_id.to_string())
            .with_turn_id(&binding.turn_id),
        cancellation: &cancellation,
    }
    .open(&binding)
    .await
}

/// Prepare canonical durable registration without publishing a pending request.
/// Disabled publication and an explicit Unsupported producer retain legacy behavior.
/// A missing locator for an opted-in producer is an error, never an ephemeral downgrade.
pub async fn prepare_wait_before_ready(
    state: &std::sync::Arc<crate::ServerState>,
    session_id: SessionId,
    request: &bcode_session_models::ToolExchangeRequest,
    sequences: Option<(u64, u64)>,
) -> Result<bool, InteractionControllerError> {
    if !PUBLICATION_ENABLED {
        return Ok(false);
    }
    let Some((invocation_sequence, exchange_sequence)) = sequences else {
        let turn = state
            .session_current_turn(session_id)
            .await
            .ok_or(InteractionControllerError::NotPending)?;
        let cancellation = ServiceCancellation::default();
        match bcode_plugin::interaction_controller::discover_durable_controller(
            &state.plugins,
            &request.producer_id,
            &request.schema,
            request.schema_version,
            PluginInvocationScope::session(session_id.to_string()).with_turn_id(&turn.turn_id),
            &cancellation,
        )
        .await
        {
            Err(InteractionControllerError::Unsupported) => return Ok(false),
            Err(error) => return Err(error),
            Ok(_) => return Err(InteractionControllerError::Unavailable),
        }
    };
    let facts = state
        .sessions
        .durable_interaction_facts(session_id, invocation_sequence, exchange_sequence)
        .await
        .map_err(|_| InteractionControllerError::InvalidContract)?;
    if !matches!(&facts.exchange.kind,
        bcode_session_models::SessionEventKind::ToolExchangeRequested { request: original } if original == request)
    {
        return Err(InteractionControllerError::Conflict);
    }
    match checkpoint_before_ready(state, session_id, invocation_sequence, exchange_sequence).await {
        Ok(_) => Ok(true),
        Err(InteractionControllerError::Unsupported) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Only deliberate producer exports and exact public receipts are retained. In
/// particular, `InteractionControllerTransition::state` is never serialized here.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CommittedExport {
    checkpoint: ProducerInteractionCheckpoint,
    view: DurableInteractionView,
    input: Option<DurableInteractionInput>,
}

impl CommittedExport {
    fn payload(&self) -> Result<DurableInteractionPayload, InteractionControllerError> {
        self.validate()?;
        let json =
            serde_json::to_string(self).map_err(|_| InteractionControllerError::InvalidContract)?;
        if json.len() > bcode_session_models::MAX_DURABLE_INTERACTION_PAYLOAD_BYTES {
            return Err(InteractionControllerError::LimitExceeded);
        }
        Ok(DurableInteractionPayload {
            contract_id: ENVELOPE.into(),
            contract_version: 1,
            json,
        })
    }

    fn read(payload: &DurableInteractionPayload) -> Result<Self, InteractionControllerError> {
        if payload.contract_id != ENVELOPE
            || payload.contract_version != 1
            || payload.json.len() > bcode_session_models::MAX_DURABLE_INTERACTION_PAYLOAD_BYTES
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        let export: Self = serde_json::from_str(&payload.json)
            .map_err(|_| InteractionControllerError::InvalidContract)?;
        export.validate()?;
        Ok(export)
    }

    fn validate(&self) -> Result<(), InteractionControllerError> {
        self.checkpoint.validate()?;
        self.view.validate()?;
        if self.checkpoint.binding != self.view.binding
            || self.checkpoint.revision != self.view.revision
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        if let Some(input) = &self.input {
            DurableInteractionReceipt {
                version: DURABLE_INTERACTION_VERSION,
                input: input.clone(),
                view: self.view.clone(),
            }
            .validate()?;
        }
        Ok(())
    }
}

fn operation_export(
    operation: &DurableInteractionOperation,
) -> Result<CommittedExport, InteractionControllerError> {
    let payload = match &operation.transition {
        DurableInteractionTransition::Checkpoint { payload } => payload,
        DurableInteractionTransition::Complete { outcome, .. } => outcome,
        DurableInteractionTransition::PublishContinuation { .. } => {
            return Err(InteractionControllerError::InvalidContract);
        }
    };
    CommittedExport::read(payload)
}

fn digest(value: &impl Serialize) -> Result<String, InteractionControllerError> {
    let bytes =
        serde_json::to_vec(value).map_err(|_| InteractionControllerError::InvalidContract)?;
    Ok(hex::encode(Sha256::digest(bytes)))
}

fn receipt(
    retained: &RetainedOperation,
    input: &DurableInteractionInput,
) -> Result<DurableInteractionReceipt, InteractionControllerError> {
    let export = retained.validate()?;
    let operation = &retained.operation;
    if operation.operation_id != input.operation.operation_id
        || operation.request_digest != digest(input)?
        || export.input.as_ref() != Some(input)
        || operation.expected_revision != Some(input.operation.expected_revision)
    {
        return Err(InteractionControllerError::Conflict);
    }
    let receipt = DurableInteractionReceipt {
        version: DURABLE_INTERACTION_VERSION,
        input: input.clone(),
        view: export.view,
    };
    receipt.validate()?;
    Ok(receipt)
}

/// A per-request host handle: no attachment-local controller state is authoritative.
pub struct DurableController<'a, S> {
    pub store: &'a S,
    pub plugins: &'a PluginRuntimeHost,
    pub session_id: SessionId,
    pub scope: PluginInvocationScope,
    pub cancellation: &'a ServiceCancellation,
}

impl<S: DurableInteractionStore> DurableController<'_, S> {
    async fn source(
        &self,
        requested: &DurableInteractionBinding,
    ) -> Result<CanonicalSource, InteractionControllerError> {
        requested.validate()?;
        if requested.session_id != self.session_id.to_string() {
            return Err(InteractionControllerError::Unauthorized);
        }
        let source = self.store.source(self.session_id, requested).await?;
        source.binding.validate()?;
        if source.binding != *requested || source.exchange_request_event_sequence == 0 {
            return Err(InteractionControllerError::Conflict);
        }
        Ok(source)
    }

    async fn transition(
        &self,
        request: &InteractionControllerRequest,
        binding: &DurableInteractionBinding,
    ) -> Result<InteractionControllerTransition, InteractionControllerError> {
        validate_controller_size(request)?;
        let transition: InteractionControllerTransition = self
            .plugins
            .invoke_service_json_scoped_cancellable(
                &binding.capability.route.producer_id,
                INTERACTION_CONTROLLER_INTERFACE,
                "transition",
                request,
                self.scope.clone(),
                Duration::from_secs(10),
                self.cancellation,
            )
            .await
            .map_err(|_| InteractionControllerError::Unavailable)?;
        validate_controller_size(&transition)?;
        if transition.version != INTERACTION_CONTROLLER_VERSION
            || transition.interaction_kind != binding.capability.route.interaction_kind
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(transition)
    }

    async fn export(
        &self,
        binding: &DurableInteractionBinding,
        revision: u64,
        transition: &InteractionControllerTransition,
        input: Option<DurableInteractionInput>,
    ) -> Result<CommittedExport, InteractionControllerError> {
        let checkpoint = export_durable_controller(
            self.plugins,
            binding,
            revision,
            transition,
            self.scope.clone(),
            self.cancellation,
        )
        .await?;
        let restored = restore_durable_controller(
            self.plugins,
            binding,
            &checkpoint,
            self.scope.clone(),
            self.cancellation,
        )
        .await?;
        let view = DurableInteractionView {
            version: DURABLE_INTERACTION_VERSION,
            binding: binding.clone(),
            revision,
            snapshot: InteractionControllerSnapshot {
                version: INTERACTION_CONTROLLER_VERSION,
                exchange_id: binding.exchange_id.clone(),
                route: binding.capability.route.clone(),
                schema_version: binding.exchange_schema_version,
                revision,
                snapshot: restored.snapshot,
                output: restored.output,
            },
        };
        let export = CommittedExport {
            checkpoint,
            view,
            input,
        };
        export.validate()?;
        Ok(export)
    }

    fn operation(
        source: &CanonicalSource,
        operation_id: String,
        request_digest: String,
        expected_revision: Option<u64>,
        transition: DurableInteractionTransition,
    ) -> DurableInteractionOperation {
        DurableInteractionOperation {
            version: bcode_session_models::DURABLE_INTERACTION_VERSION,
            key: source.key(),
            operation_id,
            request_digest,
            producer_plugin_id: source.binding.capability.route.producer_id.clone(),
            request_event_sequence: source.binding.request_sequence,
            exchange_request_event_sequence: source.exchange_request_event_sequence,
            expected_revision,
            transition,
        }
    }

    pub async fn open(
        &self,
        binding: &DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        let source = self.source(binding).await?;
        if let Some(record) = self.store.latest(self.session_id, &source.key()).await? {
            source.validate_record(&record)?;
            return self.record_view(binding, &record).await;
        }
        let transition = self
            .transition(
                &InteractionControllerRequest::Open {
                    version: INTERACTION_CONTROLLER_VERSION,
                    route: binding.capability.route.clone(),
                    schema_version: binding.exchange_schema_version,
                    payload: source.exchange_payload.clone(),
                },
                binding,
            )
            .await?;
        let export = self.export(binding, 1, &transition, None).await?;
        if terminal(&export.view.snapshot.output) {
            return Err(InteractionControllerError::NotPending);
        }
        let operation = Self::operation(
            &source,
            OPEN_OPERATION.into(),
            digest(binding)?,
            None,
            DurableInteractionTransition::Checkpoint {
                payload: export.payload()?,
            },
        );
        let committed = self
            .store
            .commit(self.session_id, &source, operation)
            .await?;
        let committed = committed.validate()?;
        if committed.view.binding != *binding
            || committed.view.revision != 1
            || committed.input.is_some()
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(committed.view)
    }

    pub async fn view(
        &self,
        binding: &DurableInteractionBinding,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        let source = self.source(binding).await?;
        let record = self
            .store
            .latest(self.session_id, &source.key())
            .await?
            .ok_or(InteractionControllerError::NotPending)?;
        source.validate_record(&record)?;
        self.record_view(binding, &record).await
    }

    async fn record_view(
        &self,
        binding: &DurableInteractionBinding,
        record: &DurableInteractionRecord,
    ) -> Result<DurableInteractionView, InteractionControllerError> {
        let export = CommittedExport::read(record.outcome.as_ref().unwrap_or(&record.checkpoint))?;
        if export.view.binding != *binding
            || export.view.revision != record.receipt.revision
            || terminal(&export.view.snapshot.output) != record.outcome.is_some()
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        restore_durable_controller(
            self.plugins,
            binding,
            &export.checkpoint,
            self.scope.clone(),
            self.cancellation,
        )
        .await?;
        Ok(export.view)
    }

    async fn input_transition(
        &self,
        source: &CanonicalSource,
        input: &DurableInteractionInput,
        export: &CommittedExport,
        payload: DurableInteractionPayload,
    ) -> Result<DurableInteractionTransition, InteractionControllerError> {
        // Host cancellation must be absorbing. A producer that ignores Cancel may
        // not persist another checkpoint or fabricate a cancelled invocation result.
        if input.operation.operation_id == CANCEL_OPERATION
            && (input.input
                != bcode_plugin_models::interaction_controller::InteractionInput::Cancel
                || export.view.snapshot.output != InteractionOutput::Cancelled)
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        Ok(if terminal(&export.view.snapshot.output) {
            let completion = complete_durable_controller(
                self.plugins,
                &source.binding,
                &export.checkpoint,
                &input.operation.operation_id,
                self.scope.clone(),
                self.cancellation,
            )
            .await?;
            let response: bcode_tool::ToolInvocationResponse =
                serde_json::from_value(completion.response)
                    .map_err(|_| InteractionControllerError::InvalidContract)?;
            let resolution = match &export.view.snapshot.output {
                InteractionOutput::Submitted { payload } => ToolExchangeResolution::Responded {
                    payload: payload.clone(),
                },
                InteractionOutput::Cancelled => ToolExchangeResolution::Cancelled,
                _ => return Err(InteractionControllerError::InvalidContract),
            };
            DurableInteractionTransition::Complete {
                outcome: payload,
                result: Box::new(ToolInvocationResultRecord {
                    invocation_id: source.binding.invocation_id.clone(),
                    model_output: response.output,
                    is_error: response.is_error,
                    content: crate::session_tool_result_content(&response.content),
                    presentation: source.presentation.clone(),
                    result: response.result.map(crate::service_tool_result_to_session),
                }),
                exchange_resolution: ToolExchangeResolutionEvent {
                    invocation_id: source.binding.invocation_id.clone(),
                    exchange_id: source.binding.exchange_id.clone(),
                    resolution,
                },
            }
        } else {
            DurableInteractionTransition::Checkpoint { payload }
        })
    }

    pub async fn input(
        &self,
        input: &DurableInteractionInput,
    ) -> Result<DurableInteractionReceipt, InteractionControllerError> {
        input.validate()?;
        if input.operation.operation_id == OPEN_OPERATION {
            return Err(InteractionControllerError::Conflict);
        }
        let source = self.source(&input.binding).await?;
        // Exact retry precedes latest-revision/terminal checks. Never substitute a newer view.
        if let Some(operation) = self
            .store
            .operation(
                self.session_id,
                &source.key(),
                &input.operation.operation_id,
            )
            .await?
        {
            return receipt(&operation, input);
        }
        let record = self
            .store
            .latest(self.session_id, &source.key())
            .await?
            .ok_or(InteractionControllerError::NotPending)?;
        source.validate_record(&record)?;
        if record.outcome.is_some() || record.receipt.revision != input.operation.expected_revision
        {
            return Err(InteractionControllerError::Conflict);
        }
        let previous = CommittedExport::read(&record.checkpoint)?;
        if previous.view.binding != source.binding
            || previous.view.revision != record.receipt.revision
        {
            return Err(InteractionControllerError::InvalidContract);
        }
        // Restore on every mutation, never take a client or ephemeral cache state.
        let restored = restore_durable_controller(
            self.plugins,
            &source.binding,
            &previous.checkpoint,
            self.scope.clone(),
            self.cancellation,
        )
        .await?;
        if terminal(&restored.output) {
            return Err(InteractionControllerError::NotPending);
        }
        let transition = self
            .transition(
                &InteractionControllerRequest::Input {
                    version: INTERACTION_CONTROLLER_VERSION,
                    route: source.binding.capability.route.clone(),
                    state: restored.state,
                    input: input.input.clone(),
                },
                &source.binding,
            )
            .await?;
        let revision = record
            .receipt
            .revision
            .checked_add(1)
            .ok_or(InteractionControllerError::LimitExceeded)?;
        let export = self
            .export(&source.binding, revision, &transition, Some(input.clone()))
            .await?;
        let payload = export.payload()?;
        let transition = self
            .input_transition(&source, input, &export, payload)
            .await?;
        let operation = Self::operation(
            &source,
            input.operation.operation_id.clone(),
            digest(input)?,
            Some(input.operation.expected_revision),
            transition,
        );
        let committed = self
            .store
            .commit(self.session_id, &source, operation)
            .await?;
        receipt(&committed, input)
    }
}

const fn terminal(output: &InteractionOutput) -> bool {
    matches!(
        output,
        InteractionOutput::Submitted { .. } | InteractionOutput::Cancelled
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_plugin_models::interaction_controller::{
        DurableInteractionCapability, InteractionControllerRoute, InteractionInput,
    };
    use serde_json::json;

    fn fixture_binding() -> DurableInteractionBinding {
        DurableInteractionBinding {
            session_id: SessionId::new().to_string(),
            turn_id: "turn".into(),
            invocation_id: "call".into(),
            request_sequence: 7,
            exchange_id: "exchange".into(),
            tool_name: "tool".into(),
            request_digest: "a".repeat(64),
            exchange_schema_version: 1,
            capability: DurableInteractionCapability {
                route: InteractionControllerRoute {
                    producer_id: "producer".into(),
                    exchange_schema: "request".into(),
                    min_schema_version: 1,
                    max_schema_version: 1,
                    interaction_kind: "kind".into(),
                },
                checkpoint_schema: "checkpoint".into(),
                checkpoint_version: 1,
                continuation_schema: "continuation".into(),
                continuation_version: 1,
            },
        }
    }

    fn retained(terminal: bool) -> (RetainedOperation, DurableInteractionInput) {
        let binding = fixture_binding();
        let input = DurableInteractionInput {
            version: DURABLE_INTERACTION_VERSION,
            binding: binding.clone(),
            operation: bcode_plugin_models::interaction_controller::DurableInteractionOperation {
                operation_id: "operation".into(),
                expected_revision: 1,
            },
            input: InteractionInput::Submit,
        };
        let export = CommittedExport {
            checkpoint: ProducerInteractionCheckpoint {
                binding: binding.clone(),
                revision: 2,
                payload: json!({"producer": "deliberate"}),
            },
            view: DurableInteractionView {
                version: DURABLE_INTERACTION_VERSION,
                binding: binding.clone(),
                revision: 2,
                snapshot: InteractionControllerSnapshot {
                    version: INTERACTION_CONTROLLER_VERSION,
                    exchange_id: binding.exchange_id.clone(),
                    route: binding.capability.route.clone(),
                    schema_version: 1,
                    revision: 2,
                    snapshot: json!({"public": true}),
                    output: if terminal {
                        InteractionOutput::Cancelled
                    } else {
                        InteractionOutput::None
                    },
                },
            },
            input: Some(input.clone()),
        };
        let payload = export.payload().unwrap();
        let source = CanonicalSource {
            binding,
            exchange_request_event_sequence: 8,
            exchange_payload: json!({}),
            presentation: None,
        };
        let transition = if terminal {
            DurableInteractionTransition::Complete {
                outcome: payload,
                result: Box::new(ToolInvocationResultRecord {
                    invocation_id: "call".into(),
                    model_output: "cancelled".into(),
                    is_error: false,
                    content: vec![],
                    presentation: None,
                    result: None,
                }),
                exchange_resolution: ToolExchangeResolutionEvent {
                    invocation_id: "call".into(),
                    exchange_id: "exchange".into(),
                    resolution: ToolExchangeResolution::Cancelled,
                },
            }
        } else {
            DurableInteractionTransition::Checkpoint { payload }
        };
        let operation = DurableController::<FixtureStore>::operation(
            &source,
            "operation".into(),
            digest(&input).unwrap(),
            Some(1),
            transition,
        );
        let receipt = bcode_session_models::DurableInteractionReceipt {
            version: bcode_session_models::DURABLE_INTERACTION_VERSION,
            key: source.key(),
            operation_id: "operation".into(),
            revision: 2,
            event_sequence: 10,
            tool_result_event_sequence: terminal.then_some(12),
        };
        (RetainedOperation { operation, receipt }, input)
    }

    // No database, daemon, producer, or active session is touched by these fixtures.
    struct FixtureStore;
    impl DurableInteractionStore for FixtureStore {
        fn source(
            &self,
            _: SessionId,
            _: &DurableInteractionBinding,
        ) -> impl std::future::Future<Output = Result<CanonicalSource, InteractionControllerError>> + Send
        {
            std::future::ready(Err(InteractionControllerError::Unavailable))
        }
        fn latest(
            &self,
            _: SessionId,
            _: &DurableInteractionKey,
        ) -> impl std::future::Future<
            Output = Result<Option<DurableInteractionRecord>, InteractionControllerError>,
        > + Send {
            std::future::ready(Err(InteractionControllerError::Unavailable))
        }
        fn operation(
            &self,
            _: SessionId,
            _: &DurableInteractionKey,
            _: &str,
        ) -> impl std::future::Future<
            Output = Result<Option<RetainedOperation>, InteractionControllerError>,
        > + Send {
            std::future::ready(Err(InteractionControllerError::Unavailable))
        }
        fn commit(
            &self,
            _: SessionId,
            _: &CanonicalSource,
            _: DurableInteractionOperation,
        ) -> impl std::future::Future<
            Output = Result<RetainedOperation, InteractionControllerError>,
        > + Send {
            std::future::ready(Err(InteractionControllerError::Unavailable))
        }
    }

    #[tokio::test]
    async fn standalone_commit_guard_retains_turn_and_rechecks_cancellation() {
        use bcode_session::DurableInteractionCommitGuard as _;
        let cancellation = std::sync::Arc::new(crate::TurnCancelState::default());
        let current =
            std::sync::Arc::new(tokio::sync::Mutex::new(Some(crate::RuntimeCurrentTurn {
                interaction_route: None,
                kind: crate::RuntimeOperationKind::ModelTurn,
                client_id: bcode_session_models::ClientId::new(),
                turn_id: "standalone".into(),
                cancel_state: std::sync::Arc::clone(&cancellation),
                model: None,
                agent_profile: None,
                execution: None,
                selection: None,
            })));
        let guard = InteractionCommitGuard {
            turn: Some(std::sync::Arc::clone(&current).lock_owned().await),
            recovered: None,
            recovered_commit: None,
            turn_id: "standalone".into(),
            workflow: None,
        };
        assert!(guard.validate().is_ok());
        assert!(current.try_lock().is_err());
        cancellation.close();
        assert!(guard.validate().is_err());
        drop(guard);
        assert!(current.try_lock().is_ok());
    }

    #[test]
    fn recovered_cancel_input_is_stable_and_revision_fenced() {
        let (_, original) = retained(false);
        let first = recovered_cancel_input(&original.binding, 4);
        first.validate().unwrap();
        assert_eq!(first, recovered_cancel_input(&original.binding, 4));
        assert_ne!(first, recovered_cancel_input(&original.binding, 5));
        assert_eq!(first.operation.operation_id, CANCEL_OPERATION);
        assert_eq!(
            first.input,
            bcode_plugin_models::interaction_controller::InteractionInput::Cancel
        );
    }

    #[tokio::test]
    async fn disabled_recovered_cancel_preserves_legacy_admission() {
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        assert!(matches!(
            cancel_recovered(&state, SessionId::new(), "original").await,
            Ok(None)
        ));
        assert!(state.recovered_interaction_owners.lock().await.is_empty());
    }

    #[test]
    fn public_discovery_view_keeps_terminal_receipt_without_checkpoint() {
        let (retained, input) = retained(true);
        let view = retained.validate().unwrap().view;
        let query = bcode_plugin_models::interaction_controller::DurableInteractionQuery {
            limit: 1,
            after_event_sequence: None,
            expected_generation: Some(10),
        };
        let page = bcode_plugin_models::interaction_controller::DurableInteractionPage {
            generation: 10,
            views: vec![view],
            next_after_event_sequence: None,
        };
        page.validate(&input.binding.session_id, &query).unwrap();
        assert_eq!(page.views[0], receipt(&retained, &input).unwrap().view);
        assert_eq!(page.views[0].snapshot.output, InteractionOutput::Cancelled);
        let encoded = serde_json::to_value(&page).unwrap();
        assert!(encoded["views"][0].get("checkpoint").is_none());
        assert!(encoded["views"][0]["snapshot"].get("state").is_none());
    }

    #[tokio::test]
    async fn disabled_publication_does_not_discover_ephemeral_waits() {
        let state = std::sync::Arc::new(crate::tests::test_server_state(
            bcode_session::SessionManager::default(),
        ));
        let session = state
            .sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let query = bcode_plugin_models::interaction_controller::DurableInteractionQuery {
            limit: 1,
            after_event_sequence: None,
            expected_generation: None,
        };
        assert_eq!(
            committed_views_page(&state, SessionId::new(), query.clone()).await,
            Err(InteractionControllerError::Unavailable)
        );
        let result = committed_views_page(&state, session.id, query).await;
        drop(state);
        assert_eq!(result, Err(InteractionControllerError::Unsupported));
    }

    #[test]
    fn retained_terminal_receipt_does_not_need_pending_exchange() {
        let (operation, input) = retained(true);
        let first = receipt(&operation, &input).unwrap();
        assert_eq!(receipt(&operation, &input).unwrap(), first);
        assert_eq!(first.view.revision, 2);
        assert_eq!(first.view.snapshot.output, InteractionOutput::Cancelled);
    }

    #[test]
    fn conflicting_operation_reuse_is_rejected_after_terminal() {
        let (operation, mut input) = retained(true);
        input.operation.expected_revision = 2;
        assert_eq!(
            receipt(&operation, &input),
            Err(InteractionControllerError::Conflict)
        );
    }

    #[test]
    fn canonical_digest_fences_input_even_with_matching_public_view() {
        let (mut operation, input) = retained(false);
        operation.operation.request_digest = "b".repeat(64);
        assert_eq!(
            receipt(&operation, &input),
            Err(InteractionControllerError::Conflict)
        );
    }

    #[test]
    fn mismatched_transaction_receipt_is_not_acknowledged() {
        let (mut operation, input) = retained(true);
        operation.receipt.revision = 3;
        assert_eq!(
            receipt(&operation, &input),
            Err(InteractionControllerError::InvalidContract)
        );
    }

    #[test]
    fn unknown_checkpoint_envelope_fails_closed() {
        let (operation, _) = retained(false);
        let mut export = operation.validate().unwrap().payload().unwrap();
        export.contract_version = 2;
        assert!(matches!(
            CommittedExport::read(&export),
            Err(InteractionControllerError::InvalidContract)
        ));
    }
}
