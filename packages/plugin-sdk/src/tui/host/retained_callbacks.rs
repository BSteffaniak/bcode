//! Author-owned callback retention over the independently negotiated product session.
use super::{CONTRACT, MAX_BYTES, PluginHostServices};
use crate::definition::host_session::{HostSessionClientError, ProductHostSessionClient};
use crate::definition::host_session_models::{
    HostSessionRequest as Q, HostSessionResponse as R, JobReservation, SessionJobRecord,
    SessionOutcome,
};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
use uuid::Uuid;

/// Keeps the recovery client and original identities alive after observer loss.
pub struct RetainedCallbacks {
    client: ProductHostSessionClient,
    jobs: Mutex<BTreeMap<Uuid, CallbackRecord>>,
}

/// Original callback admission facts retained through observation loss.
/// These records contain no replacement authority and do not authorize replay.
#[derive(Clone)]
pub struct CallbackRecord {
    /// Original Begin identity, reserved before awaiting admission.
    pub begin_request: Uuid,
    /// Original Start identity, never regenerated during recovery.
    pub start_request: Uuid,
    /// Exact worst-case reservation used for the Begin request.
    pub reservation: JobReservation,
    /// Actual job identity, absent while Begin is uncertain.
    pub job: Option<Uuid>,
    /// Actual operation identity, absent while Start is uncertain.
    pub operation: Option<Uuid>,
    /// Exact observed terminal receipt, not evidence of consumer commitment.
    pub terminal: Option<(Uuid, SessionOutcome)>,
    /// Explicit originating response; absent on legacy ordinary frontend-v2 calls.
    pub response: Option<crate::tui::portable::settlement::ResponseId>,
    /// Last confirmed settlement transition. Uncertain replies freeze further mutations.
    pub settlement: CallbackSettlement,
}

/// Confirmed callback retirement progress, never inferred from observer delivery.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum CallbackSettlement {
    /// Original callback still requires exact receipt acknowledgment.
    Retained,
    /// Exact terminal receipt accepted.
    Acknowledged,
    /// Job admissions confirmed closed.
    Closed,
    /// Underlying job confirmed joined, not merely cancelled or finished-looking.
    Joined,
    /// Release positively confirmed; original identity remains until response retirement.
    Released,
    /// Mutation reply lost or inconsistent. Only negotiated lookup recovery may advance it.
    RecoveryRequired,
}
impl RetainedCallbacks {
    pub fn new(client: ProductHostSessionClient) -> Arc<Self> {
        Arc::new(Self {
            client,
            jobs: Mutex::new(BTreeMap::new()),
        })
    }
    pub fn services(self: &Arc<Self>) -> PluginHostServices {
        self.services_for(None)
    }

    /// Capture an explicit originating response in descendant service closures.
    /// This carries correlation only and never upgrades a V1 port to recovery authority.
    pub fn services_for(
        self: &Arc<Self>,
        response: Option<crate::tui::portable::settlement::ResponseId>,
    ) -> PluginHostServices {
        let owner = Arc::clone(self);
        PluginHostServices::new(|_| Err(unavailable())).with_async(move |request| {
            let owner = Arc::clone(&owner);
            Box::pin(async move { owner.invoke(request, response).await })
        })
    }
    async fn invoke(
        &self,
        request: bmux_plugin_sdk::HostServiceOwnedRequest,
        response: Option<crate::tui::portable::settlement::ResponseId>,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        if request.capability != CONTRACT
            || request.interface != CONTRACT
            || request.kind != bmux_plugin_sdk::ServiceKind::Command
            || request.operation != "invoke"
            || request.payload.len() > MAX_BYTES
        {
            return Err(unavailable());
        }
        let budget = self.client.budget();
        let request_bytes = request
            .capability
            .len()
            .checked_add(request.interface.len())
            .and_then(|bytes| bytes.checked_add(request.operation.len()))
            .and_then(|bytes| bytes.checked_add(request.payload.len()))
            .ok_or_else(unavailable)?;
        if request_bytes > usize::try_from(budget.max_request_bytes).unwrap_or(0) {
            return Err(unavailable());
        }
        let request_id = Uuid::new_v4();
        let reservation = JobReservation {
            operations: 1,
            request_bytes: u32::try_from(request_bytes.max(1)).map_err(|_| unavailable())?,
            response_bytes: budget.max_response_bytes,
        };
        let start_request = Uuid::new_v4();
        {
            let mut jobs = self.jobs.lock().map_err(|_| unavailable())?;
            let reserved = jobs
                .values()
                .try_fold(0_u64, |total, record| {
                    total.checked_add(u64::from(record.reservation.response_bytes) + 256)
                })
                .ok_or_else(unavailable)?;
            let next_reserved = reserved
                .checked_add(u64::from(reservation.response_bytes) + 256)
                .ok_or_else(unavailable)?;
            if jobs.len() >= usize::try_from(budget.max_jobs).unwrap_or(0)
                || next_reserved > budget.retained_bytes
            {
                return Err(unavailable());
            }
            // Preserve both identities before either operation can have effects.
            jobs.insert(
                request_id,
                CallbackRecord {
                    begin_request: request_id,
                    start_request,
                    reservation,
                    job: None,
                    operation: None,
                    terminal: None,
                    response,
                    settlement: CallbackSettlement::Retained,
                },
            );
        }
        let R::JobBegun { job } = self
            .client
            .begin_job(request_id, reservation)
            .await
            .map_err(client_error)?
        else {
            return Err(unavailable());
        };
        self.update_record(request_id, |record| record.job = Some(job))?;
        let R::Started { operation } = self
            .client
            .request(Q::Start {
                job,
                request_id: start_request,
                capability: request.capability,
                kind: request.kind,
                interface: request.interface,
                operation: request.operation,
                payload: request.payload,
            })
            .await
            .map_err(client_error)?
        else {
            return Err(unavailable());
        };
        self.update_record(request_id, |record| record.operation = Some(operation))?;
        self.observe_callback(request_id, job, operation).await
    }

    async fn observe_callback(
        &self,
        request_id: Uuid,
        job: Uuid,
        operation: Uuid,
    ) -> bmux_plugin_sdk::Result<Vec<u8>> {
        let mut delay = 1;
        loop {
            match self
                .client
                .observe(job, operation)
                .await
                .map_err(client_error)?
            {
                R::Pending => {
                    tokio::time::sleep(std::time::Duration::from_millis(delay)).await;
                    delay = (delay * 2).min(32);
                }
                R::Terminal { receipt, outcome } => {
                    self.update_record(request_id, |record| {
                        record.terminal = Some((receipt, outcome.clone()));
                    })?;
                    // Observation and callback return never acknowledge this receipt.
                    match outcome {
                        SessionOutcome::Completed { payload } if payload.len() <= MAX_BYTES => {
                            return Ok(payload);
                        }
                        _ => return Err(unavailable()),
                    }
                }
                _ => return Err(unavailable()),
            }
        }
    }
    fn update_record(
        &self,
        request_id: Uuid,
        update: impl FnOnce(&mut CallbackRecord),
    ) -> bmux_plugin_sdk::Result<()> {
        let mut jobs = self.jobs.lock().map_err(|_| unavailable())?;
        let record = jobs.get_mut(&request_id).ok_or_else(unavailable)?;
        update(record);
        drop(jobs);
        Ok(())
    }

    /// Bounded diagnostic/recovery snapshot of original identities, without effects or acknowledgment.
    ///
    /// # Errors
    /// Returns a protocol error if the retained ledger is unavailable.
    pub fn recovery_records(&self) -> bmux_plugin_sdk::Result<Vec<CallbackRecord>> {
        let jobs = self.jobs.lock().map_err(|_| unavailable())?;
        Ok(jobs.values().cloned().collect())
    }

    /// Advance one confirmed retirement transition for a committed response.
    ///
    /// The caller must first join the response's author tasks. Publication completion alone
    /// does not prove joining; this operation therefore requires the author task owner to call it.
    /// Lost mutation replies mark recovery-required and are never automatically retried.
    ///
    /// # Errors
    /// Rejects missing commitment/publication evidence, uncertain jobs, and protocol failures.
    pub(in crate::tui) async fn settle_response(
        &self,
        tasks: &crate::tui::portable::AuthorTasks,
        ledger: &crate::tui::portable::settlement::ResponseLedger,
        context: &crate::tui::portable::settlement::ResponseContext,
    ) -> bmux_plugin_sdk::Result<bool> {
        let id = context.id();
        if !context.publication_finished() || !ledger.is_committed(id).map_err(|_| unavailable())? {
            return Err(unavailable());
        }
        tasks.settle().map_err(|_| unavailable())?;
        let records = self.recovery_records()?;
        if let Some(record) = records.iter().find(|record| {
            record.response == Some(id)
                && (record.job.is_none()
                    || record.operation.is_none()
                    || record.settlement == CallbackSettlement::RecoveryRequired)
        }) {
            return self
                .recover_record(record.begin_request)
                .await
                .map(|_| false);
        }
        let Some(record) = records.into_iter().find(|record| {
            record.response == Some(id) && record.settlement != CallbackSettlement::Released
        }) else {
            return Ok(true);
        };
        let job = record.job.ok_or_else(unavailable)?;
        let (request, confirmed) = match record.settlement {
            CallbackSettlement::Retained => {
                let operation = record.operation.ok_or_else(unavailable)?;
                let (receipt, outcome) = record.terminal.as_ref().ok_or_else(unavailable)?;
                if matches!(outcome, SessionOutcome::EffectsUnresolved { .. }) {
                    return Err(unavailable());
                }
                (
                    Q::Acknowledge {
                        job,
                        operation,
                        receipt: *receipt,
                    },
                    CallbackSettlement::Acknowledged,
                )
            }
            CallbackSettlement::Acknowledged => (Q::CloseJob { job }, CallbackSettlement::Closed),
            CallbackSettlement::Closed => (Q::JoinJob { job }, CallbackSettlement::Joined),
            CallbackSettlement::Joined => (Q::ReleaseJob { job }, CallbackSettlement::Released),
            CallbackSettlement::Released | CallbackSettlement::RecoveryRequired => {
                return Err(unavailable());
            }
        };
        // Reserve uncertainty before await. Cancellation must not make a mutation retryable.
        self.update_record(record.begin_request, |stored| {
            stored.settlement = CallbackSettlement::RecoveryRequired;
        })?;
        let reply = self.client.request(request).await.map_err(client_error)?;
        let next = match (confirmed, reply) {
            (CallbackSettlement::Acknowledged, R::Acknowledged)
            | (CallbackSettlement::Closed, R::AdmissionsClosed)
            | (CallbackSettlement::Joined, R::Joined)
            | (CallbackSettlement::Released, R::Released) => confirmed,
            (CallbackSettlement::Joined, R::Pending) => CallbackSettlement::Closed,
            _ => return Err(unavailable()),
        };
        self.update_record(record.begin_request, |stored| {
            stored.settlement = next;
        })?;
        Ok(false)
    }

    /// Resolve only the original Begin identity within the negotiated job-page budget.
    /// Absence and conflicting retained facts fail closed without replay or acknowledgment.
    async fn lookup_original_job(
        &self,
        record: &CallbackRecord,
    ) -> bmux_plugin_sdk::Result<SessionJobRecord> {
        let mut after = None;
        let mut pages = 0_u32;
        loop {
            pages += 1;
            if pages > self.client.budget().max_jobs.div_ceil(32) {
                return Err(unavailable());
            }
            let R::Jobs { jobs, next } = self
                .client
                .request(Q::ListJobs { after, limit: 32 })
                .await
                .map_err(client_error)?
            else {
                return Err(unavailable());
            };
            if let Some(job) = jobs
                .into_iter()
                .find(|job| job.request_id == record.begin_request)
            {
                if job.reservation != record.reservation
                    || record.job.is_some_and(|id| id != job.job)
                {
                    return Err(unavailable());
                }
                return Ok(job);
            }
            match next {
                Some(next) if Some(next) != after => after = Some(next),
                _ => return Err(unavailable()),
            }
        }
    }

    /// Recover one original callback by bounded host-ledger pages without Begin/Start replay.
    /// Missing records, inconsistent facts or uncertain observations never authorize acknowledgment.
    pub(in crate::tui) async fn recover_record(
        &self,
        request_id: Uuid,
    ) -> bmux_plugin_sdk::Result<bool> {
        let record = self
            .recovery_records()?
            .into_iter()
            .find(|record| record.begin_request == request_id)
            .ok_or_else(unavailable)?;
        let job = self.lookup_original_job(&record).await?;
        self.update_record(request_id, |stored| {
            stored.job = Some(job.job);
        })?;
        let mut after = None;
        let mut pages = 0_u32;
        let operation = loop {
            pages += 1;
            if pages > self.client.budget().max_operations.div_ceil(32) {
                return Err(unavailable());
            }
            let R::Operations { operations, next } = self
                .client
                .request(Q::ListOperations {
                    job: job.job,
                    after,
                    limit: 32,
                })
                .await
                .map_err(client_error)?
            else {
                return Err(unavailable());
            };
            if let Some(operation) = operations
                .into_iter()
                .find(|operation| operation.request_id == record.start_request)
            {
                if operation.job != job.job
                    || record.operation.is_some_and(|id| id != operation.operation)
                {
                    return Err(unavailable());
                }
                break operation;
            }
            match next {
                Some(next) if Some(next) != after => after = Some(next),
                _ => return Err(unavailable()),
            }
        };
        self.update_record(request_id, |stored| {
            stored.operation = Some(operation.operation);
        })?;
        if job.released {
            // Confirmed release is trustworthy only with our retained exact terminal receipt
            // and acknowledged operation. Missing result state is not proof of clean effects.
            if !operation.acknowledged || record.terminal.is_none() {
                return Err(unavailable());
            }
            self.update_record(request_id, |stored| {
                stored.settlement = CallbackSettlement::Released;
            })?;
            return Ok(true);
        }
        match self
            .client
            .observe(job.job, operation.operation)
            .await
            .map_err(client_error)?
        {
            R::Pending => Ok(false),
            R::Terminal { receipt, outcome } => {
                if record
                    .terminal
                    .as_ref()
                    .is_some_and(|original| original != &(receipt, outcome.clone()))
                {
                    return Err(unavailable());
                }
                let progress = if job.joined && operation.acknowledged {
                    CallbackSettlement::Joined
                } else if job.admissions_closed && operation.acknowledged {
                    CallbackSettlement::Closed
                } else if operation.acknowledged {
                    CallbackSettlement::Acknowledged
                } else {
                    CallbackSettlement::Retained
                };
                self.update_record(request_id, |stored| {
                    stored.terminal = Some((receipt, outcome));
                    stored.settlement = progress;
                })?;
                Ok(false)
            }
            _ => Err(unavailable()),
        }
    }

    /// Unacknowledged callback outcomes require an explicit consumer settlement handshake.
    pub fn ensure_settled(&self) -> bmux_plugin_sdk::Result<()> {
        let records = self.recovery_records()?;
        if records.is_empty() {
            return Ok(());
        }
        // Read and validate the original recovery facts, never infer success from missing IDs.
        // No receipt is acknowledged here, including after ordinary callback return.
        for record in &records {
            let identity_valid = !record.begin_request.is_nil()
                && !record.start_request.is_nil()
                && record.reservation.operations == 1
                && record.reservation.request_bytes > 0;
            let correlation_valid = record
                .response
                .is_none_or(|id| !id.session_id.is_nil() && id.sequence > 0);
            if !identity_valid || !correlation_valid {
                return Err(unavailable());
            }
        }
        if records
            .iter()
            .all(|record| record.settlement == CallbackSettlement::Released)
        {
            Ok(())
        } else {
            Err(unavailable())
        }
    }
}
fn client_error(_: HostSessionClientError) -> bmux_plugin_sdk::PluginError {
    unavailable()
}
fn unavailable() -> bmux_plugin_sdk::PluginError {
    bmux_plugin_sdk::PluginError::ServiceProtocol {
        details: "retained frontend callback requires settlement; do not replay".to_owned(),
    }
}
