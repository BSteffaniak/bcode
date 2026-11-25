//! Native Bcode child-session delegation for authenticated tool invocations.
//!
//! A plugin tool invoked in a parent session reaches these operations through the host-owned
//! [`NATIVE_DELEGATION_INTERFACE_ID`] route. Authority is bound to the invocation the host is
//! executing, never to the payload: the caller is always the invoking session, and a child is
//! controllable only when its canonical parent is that session and its canonical creation
//! retains a [`SessionDelegationIntent`].
//!
//! * **Same daemon and location.** Children are created and run through the ordinary session
//!   application paths of this daemon; nothing is launched as a subprocess.
//! * **Authorization.** Each child turn is admitted with a permission mode derived from the
//!   retained intent: `inherit` bypasses only while the parent's delegating turn and session
//!   are in bypass (and narrows with the parent), `agent_profile` enforces, `always_ask` asks
//!   before every permission-gated action.
//! * **Interactions.** Child permission requests and questions are routed to the parent's
//!   view and stay answerable only by the user.
//! * **Cancellation.** Children delegated with `cancel_with_turn` are interrupted when the
//!   delegating parent turn is cancelled ([`cancel_turn_dependents`]). Others keep running.
//! * **Bounded reads.** Summaries read activity, one creation event and a bounded history tail;
//!   they never replay a child's history.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bcode_session_models::delegation::{
    MAX_DELEGATION_PENDING, MAX_DELEGATION_RESULT_PREVIEW_BYTES,
    NATIVE_DELEGATION_CONTEXT_OPERATION, NATIVE_DELEGATION_CONTINUE_OPERATION,
    NATIVE_DELEGATION_DELEGATE_OPERATION, NATIVE_DELEGATION_INTERFACE_ID,
    NATIVE_DELEGATION_INTERRUPT_OPERATION, NATIVE_DELEGATION_RESULT_OPERATION,
    NATIVE_DELEGATION_WAIT_OPERATION, NativeAdmissionResponse, NativeChildStatus,
    NativeChildSummary, NativeContextRequest, NativeContextResponse, NativeContinueRequest,
    NativeControlResponse, NativeDelegateRequest, NativeDelegationAgent, NativeDelegationLimits,
    NativeInterruptRequest, NativePendingInteraction, NativePendingKind, NativeResultRequest,
    NativeSummariesResponse, NativeWaitRequest, NativeWaitUntil, SESSION_DELEGATION_INTENT_VERSION,
    SessionDelegationIntent, SessionDelegationPermissions,
};
use bcode_session_models::{
    ClientId, ExecutionInteractionRoute, ModelTurnOutcome, SessionActivityQuery,
    SessionActivityScope, SessionActivityState, SessionChildCreateRequest, SessionEventKind,
    SessionHistoryDirection, SessionHistoryQuery, SessionId, SessionSpawnRequest, SessionSummary,
    TurnAdmission, TurnAdmissionMetadata, TurnExecutionOptions, TurnOrigin, TurnPermissionMode,
    TurnPriority,
};
use bcode_tool::{ToolInvocationServiceRequest, ToolInvocationServiceResolution};

use crate::{ServerState, TurnCancelState};

/// Producer recorded on turns admitted by native delegation.
pub const TURN_PRODUCER: &str = "bcode.native-delegation";

const HISTORY_PAGE: usize = 64;
const MAX_HISTORY_PAGES: usize = 4;
const CHILD_PAGE: usize = 128;
const MAX_CHILD_PAGES: usize = 16;
const MAX_CANCEL_SCAN_PAGES: usize = 2;
const MAX_CONTEXT_CHILDREN: usize = 16;
const MAX_CONTEXT_DEPTH_WALK: u32 = 256;
const WAIT_POLL: Duration = Duration::from_secs(2);

/// A typed operation failure returned to the invoking plugin.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Failure {
    code: &'static str,
    message: String,
}

impl Failure {
    fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }

    fn resolution(self) -> ToolInvocationServiceResolution {
        ToolInvocationServiceResolution::Failed {
            code: self.code.into(),
            message: self.message,
        }
    }
}

fn session_failure(error: &bcode_session::SessionError) -> Failure {
    match error {
        bcode_session::SessionError::NotFound(_) => {
            Failure::new("native_delegation_not_found", "session not found")
        }
        _ => Failure::new(
            "native_delegation_unavailable",
            "session state could not be read",
        ),
    }
}

/// The delegating invocation, bound by the host from the tool call it is executing.
#[derive(Debug, Clone, Copy)]
pub struct Caller<'a> {
    /// Session whose tool invocation is calling.
    pub session_id: SessionId,
    /// Plugin owning the invoked tool.
    pub plugin_id: &'a str,
    /// Tool invocation identity.
    pub invocation_id: &'a str,
    /// The invocation's turn cancellation state.
    pub cancel_state: &'a TurnCancelState,
}

impl<'a> Caller<'a> {
    /// The invocation `invocation_id` of `plugin_id`'s tool in `session_id`.
    #[must_use]
    pub const fn new(
        session_id: SessionId,
        plugin_id: &'a str,
        invocation_id: &'a str,
        cancel_state: &'a TurnCancelState,
    ) -> Self {
        Self {
            session_id,
            plugin_id,
            invocation_id,
            cancel_state,
        }
    }
}

/// Execute one native delegation request for an authenticated invocation.
pub async fn invocation(
    state: &Arc<ServerState>,
    caller: Caller<'_>,
    request: ToolInvocationServiceRequest,
) -> ToolInvocationServiceResolution {
    if request.interface_id != NATIVE_DELEGATION_INTERFACE_ID
        || request
            .route_id
            .as_deref()
            .is_some_and(|route| route != NATIVE_DELEGATION_INTERFACE_ID)
    {
        return ToolInvocationServiceResolution::Unsupported;
    }
    if request.invocation_id != caller.invocation_id || caller.cancel_state.is_cancelled() {
        return ToolInvocationServiceResolution::Cancelled;
    }
    if let Err(failure) =
        crate::session_operations::require_selected_location(state, caller.session_id).await
    {
        return ToolInvocationServiceResolution::Failed {
            code: failure.code,
            message: failure.message,
        };
    }
    let outcome = match request.operation.as_str() {
        NATIVE_DELEGATION_DELEGATE_OPERATION => match decode(request.payload) {
            Ok(input) => encode(delegate(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        NATIVE_DELEGATION_CONTINUE_OPERATION => match decode(request.payload) {
            Ok(input) => encode(continue_child(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        NATIVE_DELEGATION_RESULT_OPERATION => match decode(request.payload) {
            Ok(input) => encode(result(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        NATIVE_DELEGATION_WAIT_OPERATION => match decode(request.payload) {
            Ok(input) => encode(wait(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        NATIVE_DELEGATION_INTERRUPT_OPERATION => match decode(request.payload) {
            Ok(input) => encode(interrupt(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        NATIVE_DELEGATION_CONTEXT_OPERATION => match decode(request.payload) {
            Ok(input) => encode(context(state, caller, input).await),
            Err(failure) => Err(failure),
        },
        _ => return ToolInvocationServiceResolution::Unsupported,
    };
    match outcome {
        Ok(payload) => ToolInvocationServiceResolution::Responded { payload },
        Err(failure) => failure.resolution(),
    }
}

fn decode<T: serde::de::DeserializeOwned>(payload: serde_json::Value) -> Result<T, Failure> {
    serde_json::from_value(payload)
        .map_err(|_| Failure::new("native_delegation_invalid_request", "invalid request"))
}

fn encode<T: serde::Serialize>(outcome: Result<T, Failure>) -> Result<serde_json::Value, Failure> {
    outcome.and_then(|value| {
        serde_json::to_value(value).map_err(|_| {
            Failure::new(
                "native_delegation_unavailable",
                "response could not be encoded",
            )
        })
    })
}

/// The delegation intent retained in `session_id`'s canonical creation, with its parent.
///
/// Reads only the first canonical event.
///
/// # Errors
/// Returns canonical read errors; a session without a retained intent yields `None`.
pub async fn delegation_intent(
    state: &ServerState,
    session_id: SessionId,
) -> Result<Option<(SessionId, SessionDelegationIntent)>, bcode_session::SessionError> {
    let events = state
        .sessions
        .session_events_range(session_id, 1, 1, 1)
        .await?;
    Ok(events.into_iter().find_map(|event| match event.kind {
        SessionEventKind::ChildSessionCreated {
            spawning: Some(spawning),
            ..
        } => spawning
            .delegation
            .map(|intent| (spawning.creation.parent_id, *intent)),
        _ => None,
    }))
}

/// Verify that `child` is a delegated child of the calling session.
async fn require_delegated_child(
    state: &ServerState,
    caller: SessionId,
    child: SessionId,
) -> Result<(SessionSummary, SessionDelegationIntent), Failure> {
    let summary = state
        .sessions
        .session_summary_non_owning(child)
        .await
        .map_err(|error| session_failure(&error))?;
    let not_delegated = || {
        Failure::new(
            "native_delegation_not_delegated_child",
            "session is not a delegated child of the calling session",
        )
    };
    if summary.parent_id != Some(caller) {
        return Err(not_delegated());
    }
    match delegation_intent(state, child)
        .await
        .map_err(|error| session_failure(&error))?
    {
        Some((parent, intent)) if parent == caller => Ok((summary, intent)),
        _ => Err(not_delegated()),
    }
}

/// The calling invocation's turn, when it is the parent's current turn.
async fn delegating_turn(
    state: &ServerState,
    caller: Caller<'_>,
) -> Option<(String, TurnPermissionMode)> {
    let handle = state
        .session_runtimes
        .lock()
        .await
        .get(&caller.session_id)
        .cloned()?;
    let current = handle.current_turn.lock().await;
    current
        .as_ref()
        .filter(|turn| std::ptr::eq(Arc::as_ptr(&turn.cancel_state), caller.cancel_state))
        .map(|turn| {
            let mode = turn
                .execution
                .as_ref()
                .map_or(TurnPermissionMode::Enforce, |execution| {
                    execution.permission_mode
                });
            (turn.turn_id.clone(), mode)
        })
}

/// Whether the parent's delegating turn may currently pass bypass to a child.
fn parent_bypasses(
    state: &ServerState,
    caller: Caller<'_>,
    turn_mode: Option<TurnPermissionMode>,
) -> bool {
    turn_mode == Some(TurnPermissionMode::Bypass)
        && !caller.cancel_state.bypass_revoked()
        && crate::permission_mode::current(state, caller.session_id) == TurnPermissionMode::Bypass
}

/// Permission mode for a child turn under `permissions`.
const fn child_permission_mode(
    permissions: SessionDelegationPermissions,
    parent_bypasses: bool,
) -> TurnPermissionMode {
    match permissions {
        SessionDelegationPermissions::Inherit if parent_bypasses => TurnPermissionMode::Bypass,
        SessionDelegationPermissions::Inherit | SessionDelegationPermissions::AgentProfile => {
            TurnPermissionMode::Enforce
        }
        SessionDelegationPermissions::AlwaysAsk => TurnPermissionMode::AlwaysAsk,
    }
}

/// Apply `mode` as the child's runtime mode and track inheritance so the parent can narrow it.
async fn apply_child_mode(
    state: &ServerState,
    parent: SessionId,
    child: SessionId,
    permissions: SessionDelegationPermissions,
    mode: TurnPermissionMode,
) {
    if permissions != SessionDelegationPermissions::Inherit {
        return;
    }
    if let Ok(mut links) = state.delegated_inherit_children.lock() {
        links.entry(parent).or_default().insert(child);
    }
    let session_mode = if mode == TurnPermissionMode::Bypass {
        TurnPermissionMode::Bypass
    } else {
        TurnPermissionMode::Enforce
    };
    if crate::permission_mode::current(state, child) != session_mode {
        Box::pin(crate::permission_mode::set(state, child, session_mode)).await;
    }
}

/// Children whose runtime bypass is inherited from `parent`.
pub fn inherited_children(state: &ServerState, parent: SessionId) -> Vec<SessionId> {
    state
        .delegated_inherit_children
        .lock()
        .ok()
        .and_then(|links| links.get(&parent).cloned())
        .map(|children| children.into_iter().collect())
        .unwrap_or_default()
}

/// Forget delegation links for a session that went away.
pub fn forget(state: &ServerState, session_id: SessionId) {
    if let Ok(mut links) = state.delegated_inherit_children.lock() {
        links.remove(&session_id);
        for children in links.values_mut() {
            children.remove(&session_id);
        }
    }
    if let Ok(mut dependents) = state.delegated_turn_dependents.lock() {
        dependents.retain(|(parent, _), children| {
            children.remove(&session_id);
            *parent != session_id && !children.is_empty()
        });
    }
}

/// Execution options for a delegated child turn: interactions surface in the parent's view.
fn execution_options(
    caller: SessionId,
    mode: TurnPermissionMode,
    agent: Option<String>,
    model: Option<String>,
) -> TurnExecutionOptions {
    TurnExecutionOptions {
        interaction_route: Some(ExecutionInteractionRoute {
            version: 1,
            destination_session_id: caller,
        }),
        permission_mode: mode,
        agent_profile: agent,
        model_id: model,
        ..TurnExecutionOptions::default()
    }
}

/// Depth of `summary` below its root, walking at most `bound` ancestors.
///
/// Returns `None` when the walk exceeds `bound`.
async fn depth_within(
    state: &ServerState,
    summary: &SessionSummary,
    bound: u32,
) -> Result<Option<u32>, Failure> {
    let mut depth = 0_u32;
    let mut parent = summary.parent_id;
    while let Some(next) = parent {
        depth = depth.saturating_add(1);
        if depth > bound {
            return Ok(None);
        }
        parent = state
            .sessions
            .session_summary_non_owning(next)
            .await
            .map_err(|error| session_failure(&error))?
            .parent_id;
    }
    Ok(Some(depth))
}

fn is_active(states: &[SessionActivityState]) -> bool {
    states.iter().any(|state| {
        matches!(
            state,
            SessionActivityState::Running
                | SessionActivityState::Queued
                | SessionActivityState::PermissionWait
                | SessionActivityState::InputWait
        )
    })
}

/// Count active direct children of `parent`, stopping once `bound` is reached.
async fn active_children(
    state: &ServerState,
    parent: SessionId,
    excluding: SessionId,
    bound: u32,
) -> Result<u32, Failure> {
    let mut active = 0_u32;
    let mut after = None;
    for _ in 0..MAX_CHILD_PAGES {
        let page = crate::session_operations::activity(
            state,
            parent,
            SessionActivityQuery {
                scope: SessionActivityScope::DirectChildren,
                after,
                limit: CHILD_PAGE,
            },
        )
        .await
        .map_err(|error| session_failure(&error))?;
        active = active.saturating_add(
            u32::try_from(
                page.descendants
                    .iter()
                    .filter(|child| child.session_id != excluding && is_active(&child.states))
                    .count(),
            )
            .unwrap_or(u32::MAX),
        );
        if active >= bound || page.next_after.is_none() {
            break;
        }
        after = page.next_after;
    }
    Ok(active)
}

async fn enforce_limits(
    state: &ServerState,
    parent: &SessionSummary,
    child_id: SessionId,
    limits: NativeDelegationLimits,
) -> Result<(), Failure> {
    if let Some(max_depth) = limits.max_depth {
        // The child sits one level below the parent.
        let parent_bound = max_depth.saturating_sub(1);
        if depth_within(state, parent, parent_bound).await?.is_none() {
            return Err(Failure::new(
                "native_delegation_depth_exceeded",
                format!("delegation would exceed the configured maximum depth of {max_depth}"),
            ));
        }
    }
    let active = active_children(state, parent.id, child_id, limits.max_active_children).await?;
    if active >= limits.max_active_children {
        return Err(Failure::new(
            "native_delegation_active_children_exhausted",
            format!(
                "the session already has {active} active delegated or child sessions (limit {}); wait for some to settle",
                limits.max_active_children
            ),
        ));
    }
    Ok(())
}

fn register_turn_dependent(
    state: &ServerState,
    parent: SessionId,
    turn_id: &str,
    child: SessionId,
) {
    if let Ok(mut dependents) = state.delegated_turn_dependents.lock() {
        dependents
            .entry((parent, turn_id.to_owned()))
            .or_default()
            .insert(child);
    }
}

/// Composed spawn request for a delegated child; interactions route to the parent.
fn spawn_request(
    caller: Caller<'_>,
    input: NativeDelegateRequest,
    parent: &SessionSummary,
    intent: SessionDelegationIntent,
    mode: TurnPermissionMode,
    agent: Option<String>,
) -> SessionSpawnRequest {
    let working_directory = input
        .working_directory
        .unwrap_or_else(|| parent.working_directory.clone());
    SessionSpawnRequest {
        version: 1,
        creation: SessionChildCreateRequest {
            child_id: input.child_id,
            parent_id: caller.session_id,
            name: input.title.clone(),
            working_directory: working_directory.clone(),
        },
        worktree_directory: input.worktree.then_some(working_directory),
        input: input.prompt,
        admission: TurnAdmissionMetadata {
            origin: Some(TurnOrigin {
                producer: TURN_PRODUCER.into(),
                correlation_id: Some(caller.invocation_id.to_owned()),
                display_label: input.title,
            }),
            priority: TurnPriority::Interactive,
            idempotency_key: Some(input.child_id.to_string()),
            execution: execution_options(caller.session_id, mode, agent, input.model),
            ..TurnAdmissionMetadata::default()
        },
        auth_profile: None,
        managed: None,
        delegation: Some(Box::new(intent)),
    }
}

async fn delegate(
    state: &Arc<ServerState>,
    caller: Caller<'_>,
    input: NativeDelegateRequest,
) -> Result<NativeAdmissionResponse, Failure> {
    input
        .validate()
        .map_err(|message| Failure::new("native_delegation_invalid_request", message))?;
    if input.child_id == caller.session_id {
        return Err(Failure::new(
            "native_delegation_invalid_request",
            "a session cannot delegate to itself",
        ));
    }
    let parent = state
        .sessions
        .session_summary(caller.session_id)
        .await
        .map_err(|error| session_failure(&error))?;
    // An identical retry of a recorded delegation reconciles instead of re-checking limits.
    let existing = delegation_intent(state, input.child_id)
        .await
        .ok()
        .flatten();
    if existing.is_none() {
        enforce_limits(state, &parent, input.child_id, input.limits).await?;
    }
    let turn = delegating_turn(state, caller).await;
    let intent = SessionDelegationIntent {
        version: SESSION_DELEGATION_INTENT_VERSION,
        producer_plugin_id: caller.plugin_id.to_owned(),
        invocation_id: caller.invocation_id.to_owned(),
        delegating_turn_id: turn.as_ref().map(|(turn_id, _)| turn_id.clone()),
        permissions: input.permissions,
        cancel_with_turn: input.cancel_with_turn,
    };
    if let Some((recorded_parent, recorded)) = &existing
        && (*recorded_parent != caller.session_id || recorded != &intent)
    {
        return Err(Failure::new(
            "native_delegation_conflict",
            "the child identity is already used by a different delegation",
        ));
    }
    let mode = child_permission_mode(
        input.permissions,
        parent_bypasses(state, caller, turn.as_ref().map(|(_, mode)| *mode)),
    );
    let agent = match &input.agent {
        Some(agent) => Some(crate::resolve_agent_id(state, agent).await.ok_or_else(|| {
            Failure::new(
                "native_delegation_unknown_agent",
                format!("unknown agent profile {agent:?}"),
            )
        })?),
        None => None,
    };
    let request = spawn_request(caller, input, &parent, intent.clone(), mode, agent);
    let runtime = crate::workflow_runtime_context(state, caller.session_id).await;
    let spawned = Box::pin(crate::session_spawning::spawn_delegated(
        state, runtime, request,
    ))
    .await;
    if let Some(failure) = spawned.failure {
        return Err(Failure {
            code: "native_delegation_spawn_failed",
            message: format!("{} ({})", failure.message, failure.code),
        });
    }
    let session = spawned.session.ok_or_else(|| {
        Failure::new(
            "native_delegation_spawn_failed",
            "the child could not be verified",
        )
    })?;
    apply_child_mode(
        state,
        caller.session_id,
        session.id,
        intent.permissions,
        mode,
    )
    .await;
    if intent.cancel_with_turn
        && let Some(turn_id) = &intent.delegating_turn_id
    {
        register_turn_dependent(state, caller.session_id, turn_id, session.id);
    }
    let reused =
        existing.is_some() || matches!(spawned.admission, Some(TurnAdmission::Existing(_)));
    Ok(NativeAdmissionResponse {
        child: summarize(state, &session).await,
        reused,
    })
}

async fn continue_child(
    state: &Arc<ServerState>,
    caller: Caller<'_>,
    input: NativeContinueRequest,
) -> Result<NativeAdmissionResponse, Failure> {
    input
        .validate()
        .map_err(|message| Failure::new("native_delegation_invalid_request", message))?;
    let (summary, intent) =
        require_delegated_child(state, caller.session_id, input.session_id).await?;
    let turn = delegating_turn(state, caller).await;
    let mode = child_permission_mode(
        intent.permissions,
        parent_bypasses(state, caller, turn.as_ref().map(|(_, mode)| *mode)),
    );
    apply_child_mode(
        state,
        caller.session_id,
        summary.id,
        intent.permissions,
        mode,
    )
    .await;
    if intent.cancel_with_turn
        && let Some((turn_id, _)) = &turn
    {
        register_turn_dependent(state, caller.session_id, turn_id, summary.id);
    }
    let admission = TurnAdmissionMetadata {
        origin: Some(TurnOrigin {
            producer: TURN_PRODUCER.into(),
            correlation_id: Some(caller.invocation_id.to_owned()),
            display_label: None,
        }),
        priority: TurnPriority::Interactive,
        idempotency_key: Some(input.idempotency_key.clone()),
        execution: execution_options(caller.session_id, mode, None, None),
        ..TurnAdmissionMetadata::default()
    };
    let runtime = crate::workflow_runtime_context(state, caller.session_id).await;
    let mut ownership = None;
    let admitted = Box::pin(crate::session_operations::submit_turn_with_runtime_context(
        state,
        summary.id,
        ClientId::new(),
        input.prompt,
        admission,
        &mut ownership,
        runtime,
    ))
    .await;
    drop(ownership);
    let admission = admitted.map_err(|error| match error {
        crate::session_operations::SubmitTurnError::Admission(_) => Failure::new(
            "native_delegation_admission_failed",
            "the next turn was not admitted",
        ),
        crate::session_operations::SubmitTurnError::Dispatch(_) => Failure::new(
            "native_delegation_outcome_unknown",
            "the next turn was admitted but dispatch failed; check the child before retrying",
        ),
    })?;
    if let TurnAdmission::Rejected(reason) = &admission {
        return Err(Failure::new(
            "native_delegation_admission_rejected",
            format!("the next turn was rejected: {reason:?}"),
        ));
    }
    Ok(NativeAdmissionResponse {
        child: summarize(state, &summary).await,
        reused: matches!(admission, TurnAdmission::Existing(_)),
    })
}

async fn result(
    state: &ServerState,
    caller: Caller<'_>,
    input: NativeResultRequest,
) -> Result<NativeSummariesResponse, Failure> {
    bcode_session_models::delegation::validate_batch(&input.session_ids)
        .map_err(|message| Failure::new("native_delegation_invalid_request", message))?;
    let children = verified_children(state, caller.session_id, &input.session_ids).await?;
    let mut summaries = Vec::with_capacity(children.len());
    for child in &children {
        summaries.push(summarize(state, child).await);
    }
    Ok(NativeSummariesResponse {
        children: summaries,
        timed_out: false,
    })
}

async fn verified_children(
    state: &ServerState,
    caller: SessionId,
    session_ids: &[SessionId],
) -> Result<Vec<SessionSummary>, Failure> {
    let mut children = Vec::with_capacity(session_ids.len());
    for session_id in session_ids {
        children.push(require_delegated_child(state, caller, *session_id).await?.0);
    }
    Ok(children)
}

fn wait_satisfied(until: NativeWaitUntil, summaries: &[NativeChildSummary]) -> bool {
    match until {
        NativeWaitUntil::Any => summaries
            .iter()
            .any(|summary| summary.status.is_terminal_for_wait()),
        NativeWaitUntil::All => summaries
            .iter()
            .all(|summary| summary.status.is_terminal_for_wait()),
    }
}

async fn wait(
    state: &ServerState,
    caller: Caller<'_>,
    input: NativeWaitRequest,
) -> Result<NativeSummariesResponse, Failure> {
    input
        .validate()
        .map_err(|message| Failure::new("native_delegation_invalid_request", message))?;
    let children = verified_children(state, caller.session_id, &input.session_ids).await?;
    let deadline = Instant::now() + Duration::from_millis(input.timeout_ms);
    let mut changes = state.activity_changes.subscribe();
    loop {
        let mut summaries = Vec::with_capacity(children.len());
        for child in &children {
            summaries.push(summarize(state, child).await);
        }
        if wait_satisfied(input.until, &summaries) {
            return Ok(NativeSummariesResponse {
                children: summaries,
                timed_out: false,
            });
        }
        let now = Instant::now();
        if now >= deadline {
            return Ok(NativeSummariesResponse {
                children: summaries,
                timed_out: true,
            });
        }
        let pause = (deadline - now).min(WAIT_POLL);
        tokio::select! {
            () = caller.cancel_state.cancelled() => {
                return Err(Failure::new("native_delegation_cancelled", "the wait was cancelled"));
            }
            // Any activity change re-evaluates; lagging or closing still falls back to polling.
            _ = changes.recv() => {}
            () = tokio::time::sleep(pause) => {}
        }
    }
}

/// Interrupt `child`'s active turn and clear its queue through its runtime actor.
///
/// Returns whether a live runtime accepted the request.
async fn interrupt_child(state: &ServerState, child: SessionId) -> bool {
    let handle = state.session_runtimes.lock().await.get(&child).cloned();
    let Some(handle) = handle else {
        return false;
    };
    let expected_turn_id = handle
        .current_turn
        .lock()
        .await
        .as_ref()
        .map(|turn| turn.turn_id.clone());
    let (response, completion) = tokio::sync::oneshot::channel();
    if handle
        .cancel_commands
        .send(crate::CancelCommand {
            expected_turn_id,
            clear_queue: true,
            requested_by: None,
            response,
        })
        .await
        .is_err()
    {
        return false;
    }
    completion.await.unwrap_or(false)
}

async fn interrupt(
    state: &ServerState,
    caller: Caller<'_>,
    input: NativeInterruptRequest,
) -> Result<NativeControlResponse, Failure> {
    let (summary, _) = require_delegated_child(state, caller.session_id, input.session_id).await?;
    interrupt_child(state, summary.id).await;
    Ok(NativeControlResponse {
        child: summarize(state, &summary).await,
    })
}

async fn context(
    state: &ServerState,
    caller: Caller<'_>,
    input: NativeContextRequest,
) -> Result<NativeContextResponse, Failure> {
    let summary = state
        .sessions
        .session_summary(caller.session_id)
        .await
        .map_err(|error| session_failure(&error))?;
    let depth = depth_within(state, &summary, MAX_CONTEXT_DEPTH_WALK)
        .await?
        .unwrap_or(MAX_CONTEXT_DEPTH_WALK);
    let selection = crate::session_model_selection(state, caller.session_id).await;
    let current_agent = crate::session_agent_selection(state, caller.session_id).await;
    let agents = crate::list_profiles(state, None)
        .await
        .into_iter()
        .map(|agent| NativeDelegationAgent {
            default: agent.id == current_agent,
            id: agent.id,
        })
        .collect();
    let page = state
        .sessions
        .query_child_sessions(bcode_session_models::SessionChildQuery {
            parent_id: caller.session_id,
            after: None,
            limit: MAX_CONTEXT_CHILDREN,
        })
        .await
        .map_err(|error| session_failure(&error))?;
    let mut children = Vec::with_capacity(page.children.len());
    for child in &page.children {
        children.push(summarize(state, child).await);
    }
    Ok(NativeContextResponse {
        session_id: summary.id,
        parent_id: summary.parent_id,
        depth,
        working_directory: summary.working_directory,
        model: selection.model_id,
        agents,
        children,
        children_truncated: page.next_after.is_some(),
        limits: input.limits,
    })
}

fn bounded(text: &str, max_bytes: usize) -> (String, bool) {
    if text.len() <= max_bytes {
        return (text.to_owned(), false);
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    (text[..end].to_owned(), true)
}

/// The child's latest model turn as recorded in a bounded history tail.
#[derive(Debug, Default, PartialEq, Eq)]
struct LatestTurn {
    /// Assistant text of the latest turn, in segment order.
    text: Option<String>,
    /// How the latest turn finished, when it has.
    outcome: Option<ModelTurnOutcome>,
    /// Whether work was admitted after the latest finished turn.
    pending_work: bool,
}

impl LatestTurn {
    /// Fold events in newest-first order; returns `true` once the latest turn is complete.
    fn observe(
        &mut self,
        turn: &mut Option<String>,
        segments: &mut Vec<(u32, String)>,
        kind: SessionEventKind,
    ) -> bool {
        match kind {
            SessionEventKind::ModelTurnFinished {
                turn_id, outcome, ..
            } if turn.is_none() => {
                *turn = Some(turn_id);
                self.outcome = Some(outcome);
            }
            SessionEventKind::PositionedAssistantResponseSegment {
                turn_id,
                segment_order,
                text,
                ..
            } if turn.as_deref().is_none_or(|current| current == turn_id) => {
                turn.get_or_insert(turn_id);
                segments.push((segment_order, text));
            }
            SessionEventKind::AssistantMessage { text }
                if segments.is_empty() && !text.trim().is_empty() =>
            {
                segments.push((0, text));
            }
            SessionEventKind::ModelTurnStarted { turn_id } => {
                if turn.as_deref().is_none_or(|current| current == turn_id) {
                    return true;
                }
            }
            SessionEventKind::SubmissionAccepted { .. } | SessionEventKind::UserMessage { .. }
                if turn.is_none() && segments.is_empty() =>
            {
                self.pending_work = true;
            }
            _ => {}
        }
        false
    }
}

/// Read the child's latest turn from at most [`MAX_HISTORY_PAGES`] backward pages.
async fn latest_turn(
    state: &ServerState,
    session_id: SessionId,
) -> Result<LatestTurn, bcode_session::SessionError> {
    let mut latest = LatestTurn::default();
    let mut turn = None;
    let mut segments = Vec::new();
    let mut cursor = None;
    'pages: for _ in 0..MAX_HISTORY_PAGES {
        let page = state
            .sessions
            .session_history_page(
                session_id,
                SessionHistoryQuery {
                    cursor,
                    limit: HISTORY_PAGE,
                    direction: SessionHistoryDirection::Backward,
                },
            )
            .await?;
        // Pages may be returned in either chronological order; scan newest first.
        let mut events = page.events;
        events.sort_by_key(|event| std::cmp::Reverse(event.sequence));
        for event in events {
            if latest.observe(&mut turn, &mut segments, event.kind) {
                break 'pages;
            }
        }
        if !page.has_more {
            break;
        }
        cursor = page.next_cursor;
    }
    if !segments.is_empty() {
        segments.sort_by_key(|(order, _)| *order);
        let text = segments
            .into_iter()
            .map(|(_, text)| text)
            .collect::<Vec<_>>()
            .join("\n\n");
        latest.text = (!text.trim().is_empty()).then_some(text);
    }
    Ok(latest)
}

/// Status implied by the latest recorded turn when live activity cannot be verified.
const fn recorded_status(latest: &LatestTurn) -> Option<NativeChildStatus> {
    if latest.pending_work {
        return None;
    }
    match latest.outcome {
        Some(ModelTurnOutcome::Completed) => Some(NativeChildStatus::Settled),
        Some(ModelTurnOutcome::Cancelled) => Some(NativeChildStatus::Interrupted),
        Some(_) => Some(NativeChildStatus::Failed),
        None => None,
    }
}

async fn pending_interactions(
    state: &ServerState,
    session_id: SessionId,
) -> Vec<NativePendingInteraction> {
    let mut pending = crate::interaction_operations::list_permissions(state)
        .await
        .into_iter()
        .filter(|permission| permission.session_id == session_id)
        .map(|permission| NativePendingInteraction {
            request_id: permission.permission_id,
            kind: NativePendingKind::Permission,
            tool_name: Some(bounded(&permission.tool_name, 128).0),
            prompt: None,
        })
        .take(MAX_DELEGATION_PENDING)
        .collect::<Vec<_>>();
    let remaining = MAX_DELEGATION_PENDING.saturating_sub(pending.len());
    pending.extend(
        crate::interaction_operations::list_pending_tool_exchanges(state)
            .await
            .into_iter()
            .filter(|exchange| exchange.session_id == session_id)
            .take(remaining)
            .map(|exchange| NativePendingInteraction {
                request_id: exchange.request.exchange_id,
                kind: NativePendingKind::Question,
                tool_name: Some(bounded(&exchange.request.producer_id, 128).0),
                prompt: None,
            }),
    );
    pending
}

/// Bounded, non-mutating summary of one child.
async fn summarize(state: &ServerState, child: &SessionSummary) -> NativeChildSummary {
    let activity = crate::session_operations::activity(
        state,
        child.id,
        SessionActivityQuery {
            scope: SessionActivityScope::Own,
            after: None,
            limit: 1,
        },
    )
    .await;
    let mut issue = activity
        .is_err()
        .then(|| "activity could not be observed".to_owned());
    let observed = activity.map(|page| page.own.states).unwrap_or_default();
    let pending = pending_interactions(state, child.id).await;
    let mut status = if pending.is_empty() {
        NativeChildStatus::from_states(&observed)
    } else {
        NativeChildStatus::NeedsAttention
    };
    let latest = latest_turn(state, child.id).await.unwrap_or_else(|_| {
        issue.get_or_insert_with(|| "history could not be read".to_owned());
        LatestTurn::default()
    });
    // A settled child's scheduler may already be released, leaving live activity
    // unverifiable; its recorded turn outcome is then the best available evidence.
    if status == NativeChildStatus::Unknown
        && let Some(recorded) = recorded_status(&latest)
    {
        status = recorded;
    }
    let (latest_result, latest_result_truncated) = latest.text.map_or((None, false), |text| {
        let (text, truncated) = bounded(&text, MAX_DELEGATION_RESULT_PREVIEW_BYTES);
        (Some(text), truncated)
    });
    NativeChildSummary {
        session_id: child.id,
        title: child.title().map(|title| bounded(title, 256).0),
        status,
        latest_result,
        latest_result_truncated,
        pending,
        issue,
    }
}

/// Interrupt children delegated with `cancel_with_turn` by `turn_id` of `parent`.
///
/// Children recorded by this daemon are found directly. Active children not recorded (for
/// example after a daemon restart) are found through a bounded activity scan and verified
/// against their retained creation intent.
pub async fn cancel_turn_dependents(state: &ServerState, parent: SessionId, turn_id: &str) {
    let mut candidates: BTreeSet<SessionId> = state
        .delegated_turn_dependents
        .lock()
        .ok()
        .and_then(|mut dependents| dependents.remove(&(parent, turn_id.to_owned())))
        .unwrap_or_default();
    let recorded = candidates.clone();
    let mut after = None;
    for _ in 0..MAX_CANCEL_SCAN_PAGES {
        let Ok(page) = crate::session_operations::activity(
            state,
            parent,
            SessionActivityQuery {
                scope: SessionActivityScope::DirectChildren,
                after,
                limit: CHILD_PAGE,
            },
        )
        .await
        else {
            break;
        };
        candidates.extend(
            page.descendants
                .iter()
                .filter(|child| is_active(&child.states))
                .map(|child| child.session_id),
        );
        if page.next_after.is_none() {
            break;
        }
        after = page.next_after;
    }
    for child in candidates {
        let dependent = recorded.contains(&child)
            || matches!(
                delegation_intent(state, child).await,
                Ok(Some((recorded_parent, intent)))
                    if recorded_parent == parent
                        && intent.cancel_with_turn
                        && intent.delegating_turn_id.as_deref() == Some(turn_id)
            );
        if dependent && interrupt_child(state, child).await {
            tracing::info!(%parent, %child, turn_id, "interrupted delegated child with its turn");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inherited_bypass_requires_the_parent_to_bypass() {
        assert_eq!(
            child_permission_mode(SessionDelegationPermissions::Inherit, true),
            TurnPermissionMode::Bypass
        );
        assert_eq!(
            child_permission_mode(SessionDelegationPermissions::Inherit, false),
            TurnPermissionMode::Enforce
        );
        assert_eq!(
            child_permission_mode(SessionDelegationPermissions::AgentProfile, true),
            TurnPermissionMode::Enforce
        );
        assert_eq!(
            child_permission_mode(SessionDelegationPermissions::AlwaysAsk, true),
            TurnPermissionMode::AlwaysAsk
        );
    }

    fn fold(kinds: Vec<SessionEventKind>) -> LatestTurn {
        let mut latest = LatestTurn::default();
        let (mut turn, mut segments) = (None, Vec::new());
        for kind in kinds {
            if latest.observe(&mut turn, &mut segments, kind) {
                break;
            }
        }
        if !segments.is_empty() {
            segments.sort_by_key(|(order, _)| *order);
            latest.text = Some(
                segments
                    .into_iter()
                    .map(|(_, text)| text)
                    .collect::<Vec<_>>()
                    .join("|"),
            );
        }
        latest
    }

    fn segment(turn: &str, order: u32, text: &str) -> SessionEventKind {
        SessionEventKind::PositionedAssistantResponseSegment {
            turn_id: turn.into(),
            output_position: bcode_session_models::TurnOutputPosition::new(u64::from(order)),
            segment_id: format!("{turn}-{order}"),
            segment_order: order,
            text: text.into(),
        }
    }

    #[test]
    fn latest_turn_reads_only_the_newest_turn_and_its_outcome() {
        // Newest first: the second turn finished with two segments; the first is ignored.
        let latest = fold(vec![
            SessionEventKind::ModelTurnFinished {
                turn_id: "t2".into(),
                outcome: ModelTurnOutcome::Completed,
                message: None,
            },
            segment("t2", 1, "second"),
            segment("t2", 0, "first"),
            SessionEventKind::ModelTurnStarted {
                turn_id: "t2".into(),
            },
            segment("t1", 0, "older"),
        ]);
        assert_eq!(latest.text.as_deref(), Some("first|second"));
        assert_eq!(recorded_status(&latest), Some(NativeChildStatus::Settled));

        let cancelled = fold(vec![SessionEventKind::ModelTurnFinished {
            turn_id: "t".into(),
            outcome: ModelTurnOutcome::Cancelled,
            message: None,
        }]);
        assert_eq!(
            recorded_status(&cancelled),
            Some(NativeChildStatus::Interrupted)
        );
    }

    #[test]
    fn previews_are_bounded_on_character_boundaries() {
        let (text, truncated) = bounded("héllo", 2);
        assert_eq!(text, "h");
        assert!(truncated);
        assert_eq!(bounded("abc", 8), ("abc".to_owned(), false));
    }

    fn summary(status: NativeChildStatus) -> NativeChildSummary {
        NativeChildSummary {
            session_id: SessionId::new(),
            title: None,
            status,
            latest_result: None,
            latest_result_truncated: false,
            pending: Vec::new(),
            issue: None,
        }
    }

    #[test]
    fn waits_complete_on_any_or_all_terminal_children() {
        let mixed = [
            summary(NativeChildStatus::Running),
            summary(NativeChildStatus::Settled),
        ];
        assert!(wait_satisfied(NativeWaitUntil::Any, &mixed));
        assert!(!wait_satisfied(NativeWaitUntil::All, &mixed));
        let attention = [
            summary(NativeChildStatus::NeedsAttention),
            summary(NativeChildStatus::Failed),
        ];
        assert!(wait_satisfied(NativeWaitUntil::All, &attention));
        assert!(!wait_satisfied(
            NativeWaitUntil::Any,
            &[summary(NativeChildStatus::Running)]
        ));
    }
}
