//! Bounded provider retries for one frozen local compaction operation.
//!
//! Reservations are never refunded. Only an observed normalized pre-output provider error can
//! authorize a retry; transport loss, cancellation and process loss leave a blocked reservation.
use super::*;

const MAX_EXTRA_REQUESTS: u32 = 8;
const RETRY_WINDOW_MS: u64 = 300_000;

/// Host payload compatibility boundary: older hosts reject this checkpoint's unknown field.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct RequestState {
    version: u32,
    pub(super) extra_requests: u32,
    pub(super) inherited_requests: u32,
    deadline_ms: u64,
    pub(super) request_id: Option<String>,
    outcome: Outcome,
    settled_failures: BTreeSet<String>,
    attempts: BTreeMap<String, u64>,
    consecutive: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
enum Outcome {
    Ready,
    Pending,
    Returned,
    ProviderFailed,
    Blocked,
}

impl RequestState {
    pub(super) fn new() -> Self {
        Self {
            version: 1,
            extra_requests: 0,
            inherited_requests: 0,
            deadline_ms: current_unix_millis().saturating_add(RETRY_WINDOW_MS),
            request_id: None,
            outcome: Outcome::Ready,
            settled_failures: BTreeSet::new(),
            attempts: BTreeMap::new(),
            consecutive: 0,
        }
    }

    pub(super) fn valid(&self) -> bool {
        self.version == 1
            && self.extra_requests <= MAX_EXTRA_REQUESTS
            && self.inherited_requests <= 8192
            && self.deadline_ms > 0
            && self.attempts.len() <= 8
            && self.attempts.values().all(|count| *count <= 8)
            && self.attempts.values().copied().sum::<u64>() == self.consecutive
            && self.consecutive <= u64::from(self.extra_requests)
            && self.settled_failures.len() <= 9
            && self
                .settled_failures
                .iter()
                .all(|id| !id.is_empty() && id.len() <= 256)
            && self
                .request_id
                .as_ref()
                .is_none_or(|id| !id.is_empty() && id.len() <= 256)
            && (self.outcome == Outcome::Ready) == self.request_id.is_none()
    }

    pub(super) const fn blocked(&self) -> bool {
        matches!(
            self.outcome,
            Outcome::Pending | Outcome::ProviderFailed | Outcome::Blocked
        )
    }

    pub(super) const fn failures(&self) -> &BTreeSet<String> {
        &self.settled_failures
    }

    pub(super) fn reserve_pending(&mut self) {
        // A reserved but not dispatched request is deliberately unresolved after process loss.
        self.request_id = Some("reserved".into());
        self.outcome = Outcome::Pending;
    }

    pub(super) fn reserve_extra(&mut self) -> Result<(), CompactionError> {
        if self.extra_requests >= MAX_EXTRA_REQUESTS || current_unix_millis() >= self.deadline_ms {
            self.outcome = Outcome::Blocked;
            return Err(CompactionError::OperationBlocked);
        }
        self.extra_requests += 1;
        self.reserve_pending();
        Ok(())
    }
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub(super) fn collect<'a>(
    state: &'a ServerState,
    session_id: SessionId,
    selection: &'a SessionModelSelection,
    transcript: &'a CompactionTranscript,
    prompt: &'a str,
    contract: response::ResponseContract,
    commands: Option<&'a mut RuntimeCommandContext<'_>>,
    cancel: &'a TurnCancelState,
    policy: &'a budget::ResolvedSummaryPolicy,
    checkpoint: &'a mut Option<(&mut checkpoint::LocalCheckpoint, &str)>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<String, CompactionError>> + Send + 'a>>
{
    Box::pin(collect_inner(
        state, session_id, selection, transcript, prompt, contract, commands, cancel, policy,
        checkpoint,
    ))
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn collect_inner(
    state: &ServerState,
    session_id: SessionId,
    selection: &SessionModelSelection,
    transcript: &CompactionTranscript,
    prompt: &str,
    contract: response::ResponseContract,
    mut commands: Option<&mut RuntimeCommandContext<'_>>,
    cancel: &TurnCancelState,
    policy: &budget::ResolvedSummaryPolicy,
    checkpoint: &mut Option<(&mut checkpoint::LocalCheckpoint, &str)>,
) -> Result<String, CompactionError> {
    loop {
        if cancel.is_cancelled() {
            return Err(CompactionError::Cancelled);
        }
        let (request, mut attempt) = Box::pin(prepare_synthesis_request(
            state, session_id, selection, transcript, prompt, contract, cancel, policy,
        ))
        .await?;
        if let Some((checkpoint, key)) = checkpoint.as_mut()
            && let Some(lifecycle) = checkpoint.request_state.as_mut()
        {
            lifecycle.request_id = Some(attempt.identity.request_id.clone());
            lifecycle.outcome = Outcome::Pending;
            checkpoint.save(state, session_id, key).await?;
        }
        diagnostics::contract(contract);
        diagnostics::request(&attempt.identity.request_id);
        diagnostics::emit(
            state,
            session_id,
            "request",
            "dispatch_requested",
            None,
            None,
        )
        .await?;
        let result = dispatch_synthesis_request(
            state,
            session_id,
            selection,
            request,
            &mut attempt,
            commands.as_deref_mut(),
            cancel,
        )
        .await;
        if let Some((checkpoint, key)) = checkpoint.as_mut()
            && let Some(lifecycle) = checkpoint.request_state.as_mut()
        {
            if matches!(&result, Err(CompactionError::ProviderFailure(_))) {
                lifecycle
                    .settled_failures
                    .insert(attempt.identity.request_id.clone());
            }
            if result.is_ok() {
                lifecycle.consecutive = 0;
                lifecycle.attempts.clear();
            }
            lifecycle.outcome = match &result {
                Ok(_) => Outcome::Returned,
                Err(CompactionError::ProviderFailure(_)) => Outcome::ProviderFailed,
                _ => Outcome::Blocked,
            };
            checkpoint.save(state, session_id, key).await?;
        }
        let outcome = match &result {
            Ok(_) => "returned",
            Err(CompactionError::Cancelled) => "cancelled",
            Err(CompactionError::OperationBlocked) => "blocked",
            _ => "failed",
        };
        diagnostics::emit(
            state,
            session_id,
            "request",
            outcome,
            None,
            result.as_ref().ok().map(String::as_str),
        )
        .await?;
        if let Err(CompactionError::Provider(message)) = &result {
            let reason = match message.as_str() {
                "compaction output exceeds bounded synthesis budget" => Some("wire_budget"),
                "incomplete compaction output" => Some("incomplete_output"),
                _ => None,
            };
            if let Some(reason) = reason {
                diagnostics::emit(
                    state,
                    session_id,
                    "contract",
                    "rejected",
                    Some(reason),
                    None,
                )
                .await?;
            }
        }
        let Err(CompactionError::ProviderFailure(error)) = &result else {
            if let Ok(text) = &result {
                record_response_rejection(state, session_id, contract, text).await?;
            }
            if let Some((checkpoint, _)) = checkpoint.as_ref()
                && matches!(&result, Err(CompactionError::OperationBlocked))
            {
                checkpoint
                    .observe(
                        state,
                        session_id,
                        bcode_session_models::CompactionInspectionResume::ValidatedCheckpoint,
                        bcode_session_models::CompactionInspectionPhase::Blocked,
                    )
                    .await;
            }
            return result;
        };
        // Classification and delay remain shared with ordinary provider requests. Compaction's
        // independent cumulative allowance bounds even an indefinite provider retry policy.
        let config = state.session_config(session_id).await;
        let provider_rules =
            provider_retry_rules(state, selection.provider_plugin_id.as_deref()).await;
        let remote_rules = remote_catalog_retry_rules(
            state,
            selection.provider_plugin_id.as_deref(),
            &config.model.retry,
        )
        .await;
        let Some(retry_policy) = matching_provider_retry_policy(
            &config.model.retry,
            error,
            selection,
            &provider_rules,
            &remote_rules,
        ) else {
            return result;
        };
        let Some((checkpoint, key)) = checkpoint.as_mut() else {
            return result;
        };
        let Some(lifecycle) = checkpoint.request_state.as_mut() else {
            return result;
        };
        let attempts = lifecycle
            .attempts
            .get(&retry_policy.id)
            .copied()
            .unwrap_or_default();
        if !retry_policy.allows_attempt(attempts) {
            return result;
        }
        let consecutive = lifecycle.consecutive.saturating_add(1);
        let delay = provider_retry_delay(&retry_policy, error, consecutive);
        if lifecycle.extra_requests >= MAX_EXTRA_REQUESTS {
            return Err(CompactionError::OperationBlocked);
        }
        let deadline = lifecycle.deadline_ms;
        if current_unix_millis()
            .saturating_add(u64::try_from(delay.as_millis()).unwrap_or(u64::MAX))
            >= deadline
        {
            return result;
        }
        let lifecycle = checkpoint
            .request_state
            .as_mut()
            .expect("validated lifecycle");
        lifecycle
            .attempts
            .insert(retry_policy.id.clone(), attempts.saturating_add(1));
        lifecycle.consecutive = consecutive;
        // Save the extra charge BEFORE waiting/dispatch. Losing this process cannot replay it.
        checkpoint
            .reserve_provider_retry(state, session_id, key)
            .await?;
        append_context_compaction_trace(
            state,
            session_id,
            CompactionTraceKind::Diagnostic,
            "synthesis_provider_retry",
            0,
            false,
            Some(format!(
                "request_id={} retry={} delay_ms={}; reserved request retained",
                attempt.identity.request_id,
                consecutive,
                delay.as_millis()
            )),
        )
        .await;
        if let Some(context) = commands.as_deref_mut() {
            let wait = wait_for_provider_call(
                state,
                session_id,
                context,
                cancel,
                Box::pin(async {
                    tokio::time::sleep(delay).await;
                }),
            )
            .await;
            if matches!(wait, ProviderCallWait::Cancelled) {
                return Err(CompactionError::Cancelled);
            }
        } else {
            tokio::select! {
                () = cancel.cancelled() => return Err(CompactionError::Cancelled),
                () = tokio::time::sleep(delay) => {}
            }
        }
    }
}

pub(super) async fn record_semantic_rejection(
    state: &ServerState,
    session_id: SessionId,
    reason: synthesis::Rejection,
    summary: &str,
) -> Result<(), CompactionError> {
    diagnostics::emit(
        state,
        session_id,
        "merge",
        "rejected",
        Some(diagnostics::reason(reason)),
        Some(summary),
    )
    .await
}

async fn record_response_rejection(
    state: &ServerState,
    session_id: SessionId,
    contract: response::ResponseContract,
    text: &str,
) -> Result<(), CompactionError> {
    if let Err(reason) = contract.validate(text) {
        diagnostics::emit(
            state,
            session_id,
            "contract",
            "rejected",
            Some(diagnostics::reason(reason)),
            Some(text),
        )
        .await?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_or_unknown_request_state_cannot_authorize_dispatch() {
        let mut state = RequestState::new();
        assert!(state.valid());
        state.deadline_ms = 1;
        assert!(state.reserve_extra().is_err());
        assert!(state.blocked());
        let mut value = serde_json::to_value(&state).unwrap();
        value["version"] = serde_json::json!(2);
        let unknown: RequestState = serde_json::from_value(value).unwrap();
        assert!(!unknown.valid());
    }

    #[test]
    fn pending_request_and_cumulative_charge_survive_roundtrip() {
        let mut state = RequestState::new();
        state.reserve_extra().unwrap();
        let restored: RequestState =
            serde_json::from_str(&serde_json::to_string(&state).unwrap()).unwrap();
        assert!(restored.valid());
        assert!(restored.blocked());
        assert_eq!(restored.extra_requests, 1);
        assert!(restored.failures().is_empty());
    }
}
