//! Bounded payload preparation/publication, independent of consumer state locks.
use super::{
    Arc, AtomicU64, Context, Digest, Instant, LossCause, Ordering, Poll, Sha256, State,
    StoredPayload, Uuid, XRAY_SCHEMA_VERSION, XrayCorrelation, XrayError, XrayHub, XrayPayloadRef,
    ingress, mpsc,
};

pub struct Preparation {
    targets: Vec<ingress::Target>,
    stored: StoredPayload,
    permit: mpsc::OwnedPermit<Command>,
    issued: Arc<AtomicU64>,
    count: ingress::Operation,
    bytes: ingress::Bytes,
    _reporter: PreparationReporter,
}

struct PreparationReporter {
    hub: XrayHub,
    _guard: ingress::Reporter,
}

impl Drop for PreparationReporter {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.hub.record_loss(LossCause::PayloadPreflight, 1);
        }
    }
}

pub struct Command {
    stored: StoredPayload,
    targets: Vec<ingress::Target>,
    _count: ingress::Operation,
    bytes: ingress::Bytes,
}

impl Drop for Command {
    fn drop(&mut self) {
        for target in &self.targets {
            target.admission.pending.fetch_sub(1, Ordering::Release);
        }
        self.bytes.notify();
    }
}

impl XrayHub {
    fn payload_targets(
        &self,
        correlation: &XrayCorrelation,
    ) -> Result<Vec<ingress::Target>, XrayError> {
        let _reporter = self.inner.ingress_budget.reporter();
        let activity = self.inner.activity.try_read().map_err(|_| {
            self.record_loss(LossCause::ActivityHint, 1);
            XrayError::Busy
        })?;
        let now = Instant::now();
        let targets = activity
            .iter()
            .filter(|(_, hint)| {
                hint.deep_capture
                    && hint.correlation.session_id == correlation.session_id
                    && hint
                        .correlation
                        .run_id
                        .as_ref()
                        .is_none_or(|run| correlation.run_id.as_ref() == Some(run))
                    && hint.deadline.is_none_or(|deadline| now < deadline)
                    && !hint.admission.closed.load(Ordering::Acquire)
            })
            .map(|(id, hint)| ingress::Target {
                id: id.clone(),
                admission: Arc::clone(&hint.admission),
                deliver: true,
                admitted_at: now,
            })
            .collect();
        drop(activity);
        Ok(targets)
    }

    pub(super) fn prepare_payload(
        &self,
        correlation: &XrayCorrelation,
        content_type: &str,
        bytes: &[u8],
    ) -> Result<Option<Preparation>, XrayError> {
        let reporter = PreparationReporter {
            hub: self.clone(),
            _guard: self.inner.ingress_budget.reporter(),
        };
        let targets = self.payload_targets(correlation)?;
        if targets.is_empty() {
            return Ok(None);
        }
        let reserve = || -> Option<_> {
            let count = self
                .inner
                .payload_budget
                .operation(self.inner.config.max_pending_payloads)?;
            let charge = self
                .inner
                .payload_budget
                .bytes(bytes.len(), self.inner.config.max_pending_payload_bytes)?;
            let sender = self.inner.payload_sender.try_read().ok()?.clone();
            let issued = Arc::clone(&*self.inner.payload_issued.try_read().ok()?);
            let permit = sender.try_reserve_owned().ok()?;
            Some((count, charge, permit, issued))
        };
        let Some((count, charge, permit, issued)) = reserve() else {
            self.record_loss(LossCause::PayloadPreflight, 1);
            return Err(XrayError::Busy);
        };
        // Both preparation count and bytes are charged before hashing/copying.
        let reference = XrayPayloadRef {
            schema_version: XRAY_SCHEMA_VERSION,
            payload_id: Uuid::new_v4().to_string(),
            session_id: correlation.session_id.clone(),
            content_type: content_type.to_owned(),
            byte_len: u64::try_from(bytes.len()).map_err(|_| XrayError::PayloadTooLarge)?,
            sha256: hex::encode(Sha256::digest(bytes)),
        };
        Ok(Some(Preparation {
            targets,
            stored: StoredPayload {
                reference,
                bytes: Arc::from(bytes),
            },
            permit,
            issued,
            count,
            bytes: charge,
            _reporter: reporter,
        }))
    }

    pub(super) fn commit_payload(
        &self,
        preparation: Preparation,
        correlation: &XrayCorrelation,
    ) -> Result<Option<XrayPayloadRef>, XrayError> {
        let _reporter = self.inner.ingress_budget.reporter();
        let activity = self.inner.activity.try_read().map_err(|_| {
            self.record_loss(LossCause::PayloadCommit, 1);
            XrayError::Busy
        })?;
        let admitted_at = Instant::now();
        let targets = preparation
            .targets
            .iter()
            .filter(|target| {
                activity.get(&target.id).is_some_and(|hint| {
                    Arc::ptr_eq(&hint.admission, &target.admission)
                        && hint.deep_capture
                        && hint.correlation.session_id == correlation.session_id
                        && hint
                            .correlation
                            .run_id
                            .as_ref()
                            .is_none_or(|run| correlation.run_id.as_ref() == Some(run))
                        && hint.deadline.is_none_or(|deadline| admitted_at < deadline)
                        && !hint.admission.closed.load(Ordering::Acquire)
                })
            })
            .map(|target| ingress::Target {
                id: target.id.clone(),
                admission: Arc::clone(&target.admission),
                deliver: true,
                admitted_at,
            })
            .collect::<Vec<_>>();
        if targets.is_empty() {
            return Ok(None);
        }
        let reference = preparation.stored.reference.clone();
        // Reserve a lifetime coordinate before any pending ownership or send.
        // Exhaustion never returns a reference or strands a published command.
        if preparation
            .issued
            .try_update(Ordering::AcqRel, Ordering::Acquire, |issued| {
                issued.checked_add(1)
            })
            .is_err()
        {
            self.record_loss(LossCause::PayloadCommit, 1);
            return Err(XrayError::PayloadProgressExhausted);
        }
        // No fallible allocation remains once pending counts are incremented.
        for target in &targets {
            target.admission.pending.fetch_add(1, Ordering::Release);
        }
        let command = Command {
            stored: preparation.stored,
            targets,
            _count: preparation.count,
            bytes: preparation.bytes,
        };
        // Keep lifecycle read ownership through synchronous publication: close
        // cannot freeze between registration and the guaranteed reserved send.
        preparation.permit.send(command);
        drop(activity);
        self.inner.ingress_budget.notify.notify_waiters();
        Ok(Some(reference))
    }

    /// Pump a finite prior FIFO prefix; continuous later captures cannot extend it.
    pub(super) fn pump_payloads(
        &self,
        state: &mut State,
        boundary: u64,
    ) -> Result<bool, XrayError> {
        let mut context = Context::from_waker(&self.inner.ingress_budget.waker);
        let mut processed = 0;
        while state.payload_consumed < boundary
            && processed < self.inner.config.max_pending_payloads
        {
            let Poll::Ready(Some(command)) = state.payload_ingress.poll_recv(&mut context) else {
                return Ok(false);
            };
            for target in &command.targets {
                if let Some(observer) = state.observers.get_mut(&target.id)
                    && Arc::ptr_eq(&observer.admission, &target.admission)
                    && observer.finalization.is_none()
                    && observer.deep_capture
                    && observer.session_id == command.stored.reference.session_id
                    && observer
                        .deadline
                        .is_none_or(|deadline| target.admitted_at < deadline)
                {
                    observer.retain_payload(&command.stored, &self.inner.config);
                }
            }
            let next = state
                .payload_consumed
                .checked_add(1)
                .ok_or(XrayError::PayloadProgressExhausted)?;
            state.payload_consumed = next;
            processed += 1;
            drop(command);
        }
        Ok(state.payload_consumed < boundary)
    }

    pub(super) fn payload_boundary(state: &State) -> Result<u64, XrayError> {
        let pending = u64::try_from(state.payload_ingress.len())
            .map_err(|_| XrayError::PayloadProgressExhausted)?;
        state
            .payload_consumed
            .checked_add(pending)
            .ok_or(XrayError::PayloadProgressExhausted)
    }
}
