//! Typed adaptation of the selected session's existing reservation/outcome ledger.
#[cfg(test)]
mod tests;

use super::{
    Arc, ProductHostJob, ProductHostOperationKey, ProductHostSession, SessionBudget, SessionCode,
};
use bcode_plugin_sdk::definition::{
    host_session::{
        HostSessionClientError, HostSessionFuture, ProductHostSessionClient, ProductHostSessionPort,
    },
    host_session_models::{
        HostSessionRequest as Q, HostSessionResponse as R, PRODUCT_HOST_SESSION_CONTRACT,
        PRODUCT_HOST_SESSION_VERSION, SessionJobRecord, SessionOperationRecord,
    },
};
use std::sync::atomic::Ordering;
use uuid::Uuid;

const fn rejected(code: SessionCode) -> R {
    R::Rejected { code }
}
const fn uncertain() -> HostSessionClientError {
    HostSessionClientError::Uncertain(SessionCode::Unavailable)
}

impl ProductHostSession {
    /// Bind the typed author client to this exact selected session and its ledger.
    /// No invocation-local service lookup or process-global resolver is used.
    ///
    /// # Errors
    /// Reports an invalid host-established session budget.
    pub fn typed_client(&self) -> Result<ProductHostSessionClient, HostSessionClientError> {
        ProductHostSessionClient::bind(Arc::new(self.clone()))
    }

    fn typed_job(&self, id: Uuid) -> Result<Option<ProductHostJob>, HostSessionClientError> {
        let requests = self.inner.requests.lock().map_err(|_| uncertain())?;
        let ids = self.inner.job_ids.lock().map_err(|_| uncertain())?;
        let Some(request) = ids.get(&id) else {
            return Ok(None);
        };
        let job = requests.get(request).cloned();
        drop(ids);
        drop(requests);
        Ok(job)
    }

    async fn typed_request(&self, request: Q) -> Result<R, HostSessionClientError> {
        request
            .validate()
            .map_err(HostSessionClientError::Invalid)?;
        super::retained_transport::validate_encoding(
            &request,
            bcode_plugin_sdk::definition::host_session_models::MAX_SESSION_ENVELOPE_BYTES as u64
                + 16,
        )
        .map_err(|_| HostSessionClientError::Invalid(SessionCode::RequestLimitExceeded))?;
        match request {
            Q::BeginJob {
                request_id,
                reservation,
            } => {
                if let Ok(job) =
                    self.begin_reserved_job_once(*request_id.as_bytes(), Arc::new(()), reservation)
                {
                    Ok(R::JobBegun {
                        job: job.inner.scope.id().as_uuid(),
                    })
                } else {
                    let known = self
                        .inner
                        .requests
                        .lock()
                        .map_err(|_| uncertain())?
                        .contains_key(request_id.as_bytes());
                    Ok(rejected(if known {
                        SessionCode::ConflictingRequest
                    } else if self.inner.admissions_closed.load(Ordering::Acquire) {
                        SessionCode::AdmissionsClosed
                    } else {
                        SessionCode::CapacityExceeded
                    }))
                }
            }
            Q::ListJobs { after, limit } => self.typed_jobs(after, limit),
            Q::CloseSession => {
                self.close().map_err(|_| uncertain())?;
                Ok(R::AdmissionsClosed)
            }
            Q::JoinSession => {
                if self.inner.joined.load(Ordering::Acquire) {
                    return Ok(R::Joined);
                }
                self.close().map_err(|_| uncertain())?;
                let jobs = self
                    .inner
                    .jobs
                    .lock()
                    .map_err(|_| uncertain())?
                    .values()
                    .cloned()
                    .map(|inner| ProductHostJob { inner })
                    .collect::<Vec<_>>();
                for job in jobs {
                    if !job.observe_join().await.map_err(|_| uncertain())? {
                        return Ok(R::Pending);
                    }
                }
                self.join().await.map_err(|_| uncertain())?;
                Ok(R::Joined)
            }
            Q::ReleaseSession => {
                if !self.inner.joined.load(Ordering::Acquire) {
                    return Ok(rejected(SessionCode::NotJoined));
                }
                self.release().map_or_else(
                    |_| Ok(rejected(SessionCode::UnacknowledgedOutcomes)),
                    |()| Ok(R::Released),
                )
            }
            request => self.typed_job_request(request).await,
        }
    }

    fn typed_jobs(&self, after: Option<Uuid>, limit: u32) -> Result<R, HostSessionClientError> {
        let requests = self.inner.requests.lock().map_err(|_| uncertain())?;
        let ids = self.inner.job_ids.lock().map_err(|_| uncertain())?;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let jobs = ids
            .range((start, std::ops::Bound::Unbounded))
            .take(limit as usize + 1)
            .map(|(id, request)| (*id, request, &requests[request]))
            .collect::<Vec<_>>();
        let more = jobs.len() > limit as usize;
        let jobs = jobs
            .into_iter()
            .take(limit as usize)
            .map(|(job, request_id, owner)| {
                let reservation = owner
                    .inner
                    .reservation
                    .lock()
                    .map_err(|_| uncertain())?
                    .as_ref()
                    .map(|r| r.budget)
                    .ok_or_else(uncertain)?;
                Ok(SessionJobRecord {
                    job,
                    request_id: Uuid::from_bytes(*request_id),
                    reservation,
                    admissions_closed: owner.inner.admissions_closed.load(Ordering::Acquire),
                    joined: owner.inner.joined.load(Ordering::Acquire),
                    released: owner.inner.retired.load(Ordering::Acquire),
                })
            })
            .collect::<Result<Vec<_>, HostSessionClientError>>()?;
        drop(ids);
        drop(requests);
        let next = more.then(|| jobs.last().map(|job| job.job)).flatten();
        Ok(R::Jobs { jobs, next })
    }

    async fn typed_job_request(&self, request: Q) -> Result<R, HostSessionClientError> {
        let id = match &request {
            Q::Start { job, .. }
            | Q::Observe { job, .. }
            | Q::CloseJob { job }
            | Q::JoinJob { job }
            | Q::Acknowledge { job, .. }
            | Q::ReleaseJob { job }
            | Q::ListOperations { job, .. } => *job,
            _ => return Ok(rejected(SessionCode::InvalidRequest)),
        };
        let Some(job) = self.typed_job(id)? else {
            return Ok(rejected(SessionCode::UnknownJob));
        };
        match request {
            Q::Start {
                request_id,
                capability,
                kind,
                interface,
                operation,
                payload,
                ..
            } => {
                let request = bmux_plugin_sdk::HostServiceOwnedRequest {
                    capability,
                    interface,
                    operation,
                    payload,
                    kind,
                };
                let _executor = self
                    .inner
                    .executor
                    .as_ref()
                    .map(tokio::runtime::Handle::enter);
                if job
                    .start_callback_once(*request_id.as_bytes(), request)
                    .is_ok()
                {
                    let requests = job
                        .inner
                        .operation_requests
                        .lock()
                        .map_err(|_| uncertain())?;
                    let operation = requests
                        .get(request_id.as_bytes())
                        .ok_or_else(uncertain)?
                        .operation;
                    drop(requests);
                    Ok(R::Started { operation })
                } else {
                    let known = job
                        .inner
                        .operation_requests
                        .lock()
                        .map_err(|_| uncertain())?
                        .contains_key(request_id.as_bytes());
                    Ok(rejected(if known {
                        SessionCode::ConflictingRequest
                    } else if job.inner.admissions_closed.load(Ordering::Acquire)
                        || job.inner.session_closed.load(Ordering::Acquire)
                    {
                        SessionCode::AdmissionsClosed
                    } else {
                        SessionCode::CapacityExceeded
                    }))
                }
            }
            Q::Observe { operation, .. } => job.typed_observe(operation),
            Q::Acknowledge {
                operation, receipt, ..
            } => job.typed_acknowledge(operation, receipt),
            Q::CloseJob { .. } => {
                job.close().map_err(|_| uncertain())?;
                Ok(R::AdmissionsClosed)
            }
            Q::JoinJob { .. } => Ok(if job.observe_join().await.map_err(|_| uncertain())? {
                R::Joined
            } else {
                R::Pending
            }),
            Q::ReleaseJob { .. } => {
                if !job.inner.joined.load(Ordering::Acquire) {
                    return Ok(rejected(SessionCode::NotJoined));
                }
                Ok(self.release_job(&job).map_or_else(
                    |_| rejected(SessionCode::UnacknowledgedOutcomes),
                    |()| R::Released,
                ))
            }
            Q::ListOperations { after, limit, .. } => job.typed_operations(id, after, limit),
            _ => Ok(rejected(SessionCode::InvalidRequest)),
        }
    }
}
impl ProductHostSessionPort for ProductHostSession {
    fn contract(&self) -> (&str, u32) {
        (PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION)
    }
    fn budget(&self) -> SessionBudget {
        self.inner
            .reservations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .limits
            .into()
    }
    fn request(&self, request: Q) -> HostSessionFuture<'_> {
        Box::pin(async move {
            let response = self.typed_request(request).await?;
            super::retained_transport::validate_encoding(
                &response,
                bcode_plugin_sdk::definition::host_session_models::MAX_SESSION_ENVELOPE_BYTES
                    as u64
                    + 16,
            )
            .map_err(|_| HostSessionClientError::Uncertain(SessionCode::ResponseLimitExceeded))?;
            Ok(response)
        })
    }
}
impl ProductHostJob {
    fn typed_key(
        &self,
        operation: Uuid,
    ) -> Result<Option<ProductHostOperationKey>, HostSessionClientError> {
        let requests = self
            .inner
            .operation_requests
            .lock()
            .map_err(|_| uncertain())?;
        let ids = self.inner.operation_ids.lock().map_err(|_| uncertain())?;
        Ok(ids
            .get(&operation)
            .and_then(|request| requests.get(request))
            .copied())
    }
    fn typed_observe(&self, operation: Uuid) -> Result<R, HostSessionClientError> {
        let Some(key) = self.typed_key(operation)? else {
            return Ok(rejected(SessionCode::UnknownOperation));
        };
        let outcome = self
            .typed_callback_outcome(key.index)
            .map_err(|_| uncertain())?;
        Ok(outcome.map_or(R::Pending, |outcome| R::Terminal {
            receipt: key.receipt,
            outcome,
        }))
    }
    fn typed_acknowledge(
        &self,
        operation: Uuid,
        receipt: Uuid,
    ) -> Result<R, HostSessionClientError> {
        let Some(key) = self.typed_key(operation)? else {
            return Ok(rejected(SessionCode::UnknownOperation));
        };
        if key.receipt != receipt {
            return Ok(rejected(SessionCode::StaleReceipt));
        }
        let Some(receipt) = self.callback_receipt(key.index).map_err(|_| uncertain())? else {
            return Ok(rejected(SessionCode::StaleReceipt));
        };
        self.acknowledge_callback(key.index, receipt)
            .map_err(|_| uncertain())?;
        Ok(R::Acknowledged)
    }
    fn typed_operations(
        &self,
        job: Uuid,
        after: Option<Uuid>,
        limit: u32,
    ) -> Result<R, HostSessionClientError> {
        let requests = self
            .inner
            .operation_requests
            .lock()
            .map_err(|_| uncertain())?;
        let callbacks = self.inner.callbacks.lock().map_err(|_| uncertain())?;
        let ids = self.inner.operation_ids.lock().map_err(|_| uncertain())?;
        let start = after.map_or(std::ops::Bound::Unbounded, std::ops::Bound::Excluded);
        let mut records = ids
            .range((start, std::ops::Bound::Unbounded))
            .take(limit as usize + 1)
            .map(|(_, request_id)| {
                let key = &requests[request_id];
                SessionOperationRecord {
                    job,
                    operation: key.operation,
                    request_id: Uuid::from_bytes(*request_id),
                    acknowledged: callbacks[key.index].acknowledged.load(Ordering::Acquire),
                }
            })
            .collect::<Vec<_>>();
        drop(ids);
        drop(callbacks);
        drop(requests);
        let more = records.len() > limit as usize;
        records.truncate(limit as usize);
        let next = more
            .then(|| records.last().map(|record| record.operation))
            .flatten();
        Ok(R::Operations {
            operations: records,
            next,
        })
    }
}
