//! Optional dispatcher. The caller must route only the independently negotiated contract here.
use super::{
    AuthorExecutor, AuthorFrontend, AuthorTasks, MAX_BYTES, Request, SettlementSession,
    SettlementWork, failure, settlement,
};
use crate::definition::{PluginCallContext, PluginFuture, PluginResult};
use settlement::{FrontendResult, LedgerError, Observation, Request as Q, Response as R};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

struct Lease<'a> {
    owner: &'a AuthorFrontend,
    session: Option<SettlementSession>,
}
impl Drop for Lease<'_> {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.owner.settlement.lock() {
            *slot = self.session.take();
        }
        self.owner.settlement_busy.store(false, Ordering::Release);
    }
}
struct Busy<'a>(Option<&'a AtomicBool>);
impl Drop for Busy<'_> {
    fn drop(&mut self) {
        if let Some(flag) = self.0 {
            flag.store(false, Ordering::Release);
        }
    }
}
impl AuthorFrontend {
    /// Dispatch the optional settlement contract, never ordinary frontend-v2 payloads.
    /// Actual V2 capture occurs while the original call is live, with no V1 fallback.
    ///
    /// # Errors
    /// Rejects invalid envelopes, unavailable V2 authority, concurrent nested admission,
    /// failed negotiation or uncertain execution. Retains admitted identities on cancellation.
    pub fn invoke_settlement(
        &self,
        call: PluginCallContext,
        payload: Vec<u8>,
    ) -> PluginFuture<'_, PluginResult<Vec<u8>>> {
        Box::pin(async move {
            let envelope = settlement::RequestEnvelope::decode(&payload)
                .map_err(|_| failure("invalid optional frontend settlement envelope"))?;
            let retained = self.capture_settlement_session(&call)?;
            let executor = self
                .executor
                .get_or_init(AuthorExecutor::new)
                .as_ref()
                .map_err(|message| failure(message))?;
            let execution = self.dispatch_settlement(envelope.request, retained);
            tokio::pin!(execution);
            let response = std::future::poll_fn(|cx| {
                let _entered = executor.handle.enter();
                std::future::Future::poll(execution.as_mut(), cx)
            })
            .await?;
            let bytes = serde_json::to_vec(&settlement::ResponseEnvelope {
                schema: settlement::VERSION,
                response,
            })
            .map_err(|_| failure("frontend settlement encoding failed"))?;
            if bytes.len() > MAX_BYTES {
                return Err(failure("frontend settlement response exceeds budget"));
            }
            Ok(bytes)
        })
    }

    async fn dispatch_settlement(
        &self,
        request: Q,
        retained: Arc<crate::definition::host_session::RetainedProductHostSession>,
    ) -> PluginResult<R> {
        self.settlement_busy
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| failure("frontend settlement busy; no nested admission"))?;
        let mut busy = Busy(Some(&self.settlement_busy));
        let session_id = match &request {
            Q::Execute { id, .. } | Q::ObserveResponse { id } | Q::CommitResponse { id } => {
                id.session_id
            }
            Q::Settle { session_id, .. } | Q::CloseAdmission { session_id } => *session_id,
        };
        let first = self
            .settlement
            .lock()
            .map_err(|_| failure("frontend settlement unavailable"))?
            .is_none();
        if first {
            if !matches!(request, Q::Execute { id, .. } if id.sequence == 1) {
                return Ok(R::Rejected {
                    error: LedgerError::UnknownResponse,
                });
            }
            let client = retained
                .negotiate()
                .await
                .map_err(|_| failure("frontend V2 negotiation requires recovery"))?;
            let mut slot = self
                .settlement
                .lock()
                .map_err(|_| failure("frontend settlement unavailable"))?;
            if slot.is_none() {
                *slot = Some(SettlementSession {
                    session_id,
                    ledger: settlement::ResponseLedger::new(
                        session_id,
                        32 * (MAX_BYTES + crate::SERVICE_BRIDGE_MAX_REQUEST_BYTES + 256),
                    )
                    .map_err(|_| failure("invalid frontend settlement session"))?,
                    responses: BTreeMap::new(),
                    callbacks: super::super::host::RetainedCallbacks::new(client),
                });
            }
        }
        let session = self
            .settlement
            .lock()
            .map_err(|_| failure("frontend settlement unavailable"))?
            .take()
            .ok_or_else(|| failure("frontend settlement busy; no nested admission"))?;
        let mut lease = Lease {
            owner: self,
            session: Some(session),
        };
        // Lease now owns reset of admission even if the execute observer disappears.
        busy.0 = None;
        let state = lease
            .session
            .as_mut()
            .ok_or_else(|| failure("frontend settlement unavailable"))?;
        let response = match request {
            Q::Execute { id, request } => self.execute_correlated(state, id, *request).await?,
            Q::ObserveResponse { id } => observed(&state.ledger, id),
            Q::CommitResponse { id } => {
                let already = state.ledger.is_committed(id).unwrap_or(false);
                match state.ledger.commit(id) {
                    Ok(()) => R::Committed {
                        id,
                        already_committed: already,
                    },
                    Err(error) => R::Rejected { error },
                }
            }
            Q::CloseAdmission { session_id } => {
                // A foreign session must never close the actual bound ledger.
                let probe = settlement::ResponseId {
                    session_id,
                    sequence: 1,
                };
                if state.ledger.observe(probe) == Err(LedgerError::InvalidIdentity) {
                    R::Rejected {
                        error: LedgerError::InvalidIdentity,
                    }
                } else {
                    state.ledger.close_admission();
                    R::AdmissionClosed { session_id }
                }
            }
            Q::Settle { session_id, limit } => {
                if state.session_id == session_id {
                    settle(state, session_id, limit).await
                } else {
                    R::Rejected {
                        error: LedgerError::InvalidIdentity,
                    }
                }
            }
        };
        Ok(response)
    }

    async fn execute_correlated(
        &self,
        state: &mut SettlementSession,
        id: settlement::ResponseId,
        request: Request,
    ) -> PluginResult<R> {
        match state.ledger.admit(id, &request, MAX_BYTES / 2) {
            Ok(false) => return Ok(observed(&state.ledger, id)),
            Err(error) => return Ok(R::Rejected { error }),
            Ok(true) => {}
        }
        let resource = match &request {
            Request::Input { resource, .. }
            | Request::Edit { resource, .. }
            | Request::EditStatus { resource }
            | Request::NavigationFinished { resource, .. }
            | Request::Effects { resource } => Some(*resource),
            _ => None,
        };
        let drains_effects = matches!(&request, Request::Effects { .. } | Request::DrainText);
        let effects_required = matches!(
            &request,
            Request::Command { .. }
                | Request::Input { .. }
                | Request::Edit { .. }
                | Request::EditStatus { .. }
                | Request::NavigationFinished { .. }
        );
        let context = settlement::ResponseContext::new(id);
        let tasks = Arc::new(AuthorTasks {
            context: Some(Arc::clone(&context)),
            ..AuthorTasks::new()
        });
        state.responses.insert(
            id.sequence,
            SettlementWork {
                context: Arc::clone(&context),
                tasks: Arc::clone(&tasks),
                failed: true,
                effects_required,
                resource,
                published_in: None,
            },
        );
        if drains_effects {
            for work in state.responses.values_mut() {
                if work.resource == resource
                    && work.effects_required
                    && work.published_in.is_none()
                    && !work.failed
                    && work.tasks.seal_joined_publication().unwrap_or(false)
                {
                    work.published_in = Some(id);
                }
            }
        }
        let result = self
            .execute_with_tasks(request, Some(state.callbacks.services_for(Some(id))), tasks)
            .await;
        // Async descendants retain the explicit original context and services. A cancelled
        // execute observer never reaches sealing, so its effects cannot be falsely settled.
        if result.is_ok()
            && let Some(work) = state.responses.get_mut(&id.sequence)
        {
            work.failed = false;
        }
        // Returning the initial response does not seal lazy descendant publication.
        // Settlement seals only after the response-local task owner joins every handle.
        let result = result.map_or(FrontendResult::Failed(LedgerError::Unsettled), |response| {
            FrontendResult::Completed(Box::new(response))
        });
        let bytes = serde_json::to_vec(&result)
            .map_err(|_| failure("frontend correlated result encoding failed"))?;
        state
            .ledger
            .complete(id, bytes)
            .map_err(|_| failure("frontend response reservation exceeded; recovery required"))?;
        Ok(observed(&state.ledger, id))
    }
}
fn observed(ledger: &settlement::ResponseLedger, id: settlement::ResponseId) -> R {
    match ledger.observe(id) {
        Ok(Observation::Pending) => R::Pending { id },
        Ok(Observation::Retired) => R::Retired { id },
        Ok(Observation::Response { bytes, committed }) => serde_json::from_slice(&bytes).map_or(
            R::Rejected {
                error: LedgerError::Unsettled,
            },
            |result| R::Result {
                id,
                result,
                committed,
            },
        ),
        Err(error) => R::Rejected { error },
    }
}
async fn settle(state: &mut SettlementSession, session_id: uuid::Uuid, limit: u32) -> R {
    let mut status = settlement::SettlementState::Settled;
    let keys: Vec<_> = state
        .responses
        .keys()
        .copied()
        .take(limit as usize)
        .collect();
    for sequence in keys {
        let Some(work) = state.responses.get(&sequence) else {
            continue;
        };
        if work.context.id().session_id != session_id {
            return R::Rejected {
                error: LedgerError::InvalidIdentity,
            };
        }
        if work.failed {
            status = settlement::SettlementState::RecoveryRequired;
            break;
        }
        if work.effects_required
            && !work
                .published_in
                .is_some_and(|publication| state.ledger.is_committed(publication).unwrap_or(false))
        {
            status = settlement::SettlementState::Pending;
            continue;
        }
        match work.tasks.seal_joined_publication() {
            Ok(true) => {}
            Ok(false) => {
                status = settlement::SettlementState::Pending;
                continue;
            }
            Err(_) => {
                status = settlement::SettlementState::RecoveryRequired;
                break;
            }
        }
        if !work.context.publication_finished()
            || !state
                .ledger
                .is_committed(work.context.id())
                .unwrap_or(false)
        {
            status = settlement::SettlementState::Pending;
            continue;
        }
        match state
            .callbacks
            .settle_response(&work.tasks, &state.ledger, &work.context)
            .await
        {
            Ok(true) => {
                if state.ledger.confirm_released(work.context.id()).is_err() {
                    status = settlement::SettlementState::RecoveryRequired;
                } else {
                    state.responses.remove(&sequence);
                }
            }
            Ok(false) => status = settlement::SettlementState::Pending,
            Err(_) => {
                status = settlement::SettlementState::RecoveryRequired;
                break;
            }
        }
    }
    if !state.responses.is_empty() && status == settlement::SettlementState::Settled {
        status = settlement::SettlementState::Pending;
    }
    R::Settlement {
        session_id,
        state: status,
    }
}
