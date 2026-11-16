//! Typed author client for the one retained product host-session contract.
//!
//! This is not an executor, recovery ledger or native ABI. A trusted host binds
//! a separate retained port; invocation-local host services cannot implement it.
//! Transport failures never prove that an admitted operation had no effects.

pub mod retained;
pub use retained::{
    ProductAuthorContext, RetainedProductHostSession, RetainedSessionWait, SessionNegotiation,
};

/// Bound a real independent retained capability while `call` is valid.
///
/// Returns the recovery-owning adapter before negotiation is awaited, so a
/// cancelled/failed negotiation observer cannot lose its transport identity.
/// Call `RetainedProductHostSession::negotiate` on the returned adapter, then
/// attach that client through `ProductExtensions::with_product_host_session`.
///
/// # Errors
/// Returns unsupported for missing negotiated openers; never captures an expired
/// invocation port as the deferred transport.
pub fn open_retained_session(
    scope: crate::definition::PluginCreateContext,
    call: &crate::definition::PluginCallContext,
    wait: Arc<dyn RetainedSessionWait>,
) -> Result<Arc<RetainedProductHostSession>, HostSessionClientError> {
    ProductAuthorContext::capture(scope, call)?.retained_session(wait)
}

/// Open only the selected recovery-V2 capability for deferred settlement mode.
///
/// # Errors
/// Missing/rejected V2 acquisition fails closed without trying V1. The returned
/// recovery owner is published before negotiation is awaited; keep it on failure.
pub fn open_retained_session_v2(
    scope: crate::definition::PluginCreateContext,
    call: &crate::definition::PluginCallContext,
    wait: Arc<dyn RetainedSessionWait>,
) -> Result<Arc<RetainedProductHostSession>, HostSessionClientError> {
    ProductAuthorContext::capture_recovery_v2(scope, call)?.retained_session(wait)
}

use super::host_session_models::{
    HostSessionRequest, HostSessionResponse, JobReservation, MAX_SESSION_ENVELOPE_BYTES,
    MAX_SESSION_PAGE_SIZE, PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION,
    SessionBudget, SessionCode, SessionOutcome,
};
use std::{future::Future, pin::Pin, sync::Arc};
use uuid::Uuid;

/// Async result independent of the frontend executor or native packaging.
pub type HostSessionFuture<'a> =
    Pin<Box<dyn Future<Output = Result<HostSessionResponse, HostSessionClientError>> + Send + 'a>>;

/// No public diagnostics can contain host payloads, credentials or private errors.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HostSessionClientError {
    /// Contract was not negotiated; no synchronous/invocation fallback exists.
    Unsupported,
    /// Invalid local facts; no request was dispatched by this client.
    Invalid(SessionCode),
    /// Port call failed or could not be verified. Recover by original request ID;
    /// do not reissue the effect with a fresh identity.
    Uncertain(SessionCode),
}
impl std::fmt::Display for HostSessionClientError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Unsupported => "retained product host session unsupported",
            Self::Invalid(_) => "retained product host session request rejected locally",
            Self::Uncertain(_) => "retained product host session outcome requires recovery",
        })
    }
}
impl std::error::Error for HostSessionClientError {}

/// Same Rust author interface for typed static dispatch and encoded native adapters.
///
/// Implementations are host-bound to exactly one session/activation
/// and retain its execution/image owner until explicit joined release succeeds.
/// No serialized request or reference can select another port or mint authority.
///
/// The B4 implementation owns reservations, producer lifetimes, idempotency,
/// tombstones, byte accounting and actual cleanup. The generic M1 retained port
/// adapter supplies native byte transport only, never a second product ledger.
pub trait ProductHostSessionPort: Send + Sync {
    /// Exact negotiated contract, independent of invocation service availability.
    fn contract(&self) -> (&str, u32);
    /// Host-established policy for this session (not caller-supplied grants).
    fn budget(&self) -> SessionBudget;
    /// Execute one bounded protocol operation. Observer drop never cancels work,
    /// acknowledges an outcome, or releases producer/image authority.
    fn request(&self, request: HostSessionRequest) -> HostSessionFuture<'_>;
}

/// Retained author capability. Cloning retains the same owner, not a new session.
///
/// There is deliberately no serde implementation, raw-handle constructor, global
/// resolver, automatic retry, or Drop implementation that claims settlement.
#[derive(Clone)]
pub struct ProductHostSessionClient {
    port: Arc<dyn ProductHostSessionPort>,
    budget: SessionBudget,
}
impl ProductHostSessionClient {
    /// Bind an explicitly negotiated host-created session port.
    ///
    /// # Errors
    /// Returns unsupported for a different contract/version or invalid for bad
    /// host budget. This method does not open a session or admit any producer.
    pub fn bind(port: Arc<dyn ProductHostSessionPort>) -> Result<Self, HostSessionClientError> {
        if port.contract() != (PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION) {
            return Err(HostSessionClientError::Unsupported);
        }
        let budget = port.budget();
        budget.validate().map_err(HostSessionClientError::Invalid)?;
        Ok(Self { port, budget })
    }
    /// Observe the host-established policy; metadata also consumes this budget.
    #[must_use]
    pub const fn budget(&self) -> SessionBudget {
        self.budget
    }
    /// Issue one exact operation with no automatic effect replay.
    ///
    /// # Errors
    /// Returns invalid before dispatch for local validation errors. Port/protocol
    /// failures are uncertain: use original request IDs and recovery operations.
    pub async fn request(
        &self,
        request: HostSessionRequest,
    ) -> Result<HostSessionResponse, HostSessionClientError> {
        request
            .validate()
            .map_err(HostSessionClientError::Invalid)?;
        if let HostSessionRequest::BeginJob { reservation, .. } = &request {
            reservation
                .validate_for(self.budget)
                .map_err(HostSessionClientError::Invalid)?;
        }
        if let HostSessionRequest::Start { payload, .. } = &request
            && u64::try_from(payload.len()).unwrap_or(u64::MAX)
                > u64::from(self.budget.max_request_bytes)
        {
            return Err(HostSessionClientError::Invalid(
                SessionCode::RequestLimitExceeded,
            ));
        }
        bounded_encoded_size(&request, SessionCode::RequestLimitExceeded)
            .map_err(HostSessionClientError::Invalid)?;
        // Keep the original request for exact response-family/identity validation.
        // The bounded copy is not retained by a second executor or ledger.
        let response = self.port.request(request.clone()).await?;
        bounded_encoded_size(&response, SessionCode::ResponseLimitExceeded)
            .map_err(HostSessionClientError::Uncertain)?;
        validate_response(&request, &response).map_err(HostSessionClientError::Uncertain)?;
        Ok(response)
    }
    /// Begin or recover the exact job for this caller's idempotency identity.
    ///
    /// # Errors
    /// Same validation/uncertainty behavior as `request`; conflicts are returned
    /// as typed rejected responses, never retried with a fresh request ID.
    pub async fn begin_job(
        &self,
        request_id: Uuid,
        reservation: JobReservation,
    ) -> Result<HostSessionResponse, HostSessionClientError> {
        self.request(HostSessionRequest::BeginJob {
            request_id,
            reservation,
        })
        .await
    }
    /// Observe without consuming the retained outcome, even after closure.
    ///
    /// # Errors
    /// Returns invalid local IDs or uncertain transport/protocol failure.
    pub async fn observe(
        &self,
        job: Uuid,
        operation: Uuid,
    ) -> Result<HostSessionResponse, HostSessionClientError> {
        self.request(HostSessionRequest::Observe { job, operation })
            .await
    }
}

// Count the exact encoded envelope without allocating a second unbounded byte
// buffer. Native adapters must enforce the same bound before decode/allocation.
fn bounded_encoded_size(
    value: &impl serde::Serialize,
    failure: SessionCode,
) -> Result<(), SessionCode> {
    struct Counter(usize);
    impl std::io::Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_add(bytes.len())
                .ok_or_else(|| std::io::Error::other("limit"))?;
            if self.0 > MAX_SESSION_ENVELOPE_BYTES {
                return Err(std::io::Error::other("limit"));
            }
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Counter(0), value).map_err(|_| failure)
}
fn validate_response(
    request: &HostSessionRequest,
    response: &HostSessionResponse,
) -> Result<(), SessionCode> {
    use HostSessionRequest as Q;
    use HostSessionResponse as R;
    if matches!(response, R::Rejected { .. }) {
        return Ok(());
    }
    let valid = match (request, response) {
        (Q::BeginJob { .. }, R::JobBegun { job }) => !job.is_nil(),
        (Q::Start { .. }, R::Started { operation }) => !operation.is_nil(),
        (Q::Observe { .. }, R::Pending)
        | (Q::CloseJob { .. } | Q::CloseSession, R::AdmissionsClosed)
        | (Q::JoinJob { .. } | Q::JoinSession, R::Pending | R::Joined)
        | (Q::Acknowledge { .. }, R::Acknowledged)
        | (Q::ReleaseJob { .. } | Q::ReleaseSession, R::Released) => true,
        (Q::Observe { .. }, R::Terminal { receipt, outcome }) => {
            !receipt.is_nil()
                && match outcome {
                    SessionOutcome::Completed { payload } => {
                        payload.len() <= MAX_SESSION_ENVELOPE_BYTES
                    }
                    SessionOutcome::RejectedBeforeEffects { code } => {
                        *code != SessionCode::ResponseLimitExceeded
                    }
                    SessionOutcome::EffectsUnresolved { .. } => true,
                }
        }
        (Q::ListJobs { after, limit }, R::Jobs { jobs, next }) => {
            jobs.len() <= (*limit).min(MAX_SESSION_PAGE_SIZE) as usize
                && valid_page(jobs.iter().map(|record| record.job), *after, *next)
                && jobs.iter().all(|record| !record.request_id.is_nil())
        }
        (Q::ListOperations { job, after, limit }, R::Operations { operations, next }) => {
            operations.len() <= (*limit).min(MAX_SESSION_PAGE_SIZE) as usize
                && valid_page(
                    operations.iter().map(|record| record.operation),
                    *after,
                    *next,
                )
                && operations
                    .iter()
                    .all(|record| record.job == *job && !record.request_id.is_nil())
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(SessionCode::InvalidResponse)
    }
}
fn valid_page(ids: impl Iterator<Item = Uuid>, after: Option<Uuid>, next: Option<Uuid>) -> bool {
    let mut previous = after;
    let mut last = None;
    for id in ids {
        if id.is_nil() || previous.is_some_and(|previous| id <= previous) {
            return false;
        }
        previous = Some(id);
        last = Some(id);
    }
    next.is_none() || next == last
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    struct Port {
        calls: AtomicUsize,
        reply: HostSessionResponse,
    }
    impl ProductHostSessionPort for Port {
        fn contract(&self) -> (&str, u32) {
            (PRODUCT_HOST_SESSION_CONTRACT, PRODUCT_HOST_SESSION_VERSION)
        }
        fn budget(&self) -> SessionBudget {
            SessionBudget {
                max_jobs: 2,
                max_operations: 4,
                max_request_bytes: 1024,
                max_response_bytes: 1024,
                retained_bytes: 8192,
            }
        }
        fn request(&self, _request: HostSessionRequest) -> HostSessionFuture<'_> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(std::future::ready(Ok(self.reply.clone())))
        }
    }
    fn ready<T>(future: impl Future<Output = T>) -> T {
        let mut future = std::pin::pin!(future);
        match future
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
        {
            std::task::Poll::Ready(value) => value,
            std::task::Poll::Pending => panic!("test port is ready"),
        }
    }
    #[test]
    fn invalid_admission_never_reaches_port() {
        let port = Arc::new(Port {
            calls: AtomicUsize::new(0),
            reply: HostSessionResponse::Pending,
        });
        let client = ProductHostSessionClient::bind(port.clone()).unwrap();
        assert_eq!(
            ready(client.observe(Uuid::nil(), Uuid::nil())),
            Err(HostSessionClientError::Invalid(SessionCode::InvalidRequest))
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn invalid_outcome_is_uncertain_and_never_retried() {
        let port = Arc::new(Port {
            calls: AtomicUsize::new(0),
            reply: HostSessionResponse::Joined,
        });
        let client = ProductHostSessionClient::bind(port.clone()).unwrap();
        assert_eq!(
            ready(client.observe(Uuid::from_u128(1), Uuid::from_u128(2))),
            Err(HostSessionClientError::Uncertain(
                SessionCode::InvalidResponse
            ))
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 1);
    }
    #[test]
    fn response_family_and_receipt_are_exact() {
        let job = Uuid::from_u128(1);
        let operation = Uuid::from_u128(2);
        let observe = HostSessionRequest::Observe { job, operation };
        assert_eq!(
            validate_response(&observe, &HostSessionResponse::Joined),
            Err(SessionCode::InvalidResponse)
        );
        assert_eq!(
            validate_response(
                &observe,
                &HostSessionResponse::Terminal {
                    receipt: Uuid::nil(),
                    outcome: SessionOutcome::Completed { payload: vec![] },
                }
            ),
            Err(SessionCode::InvalidResponse)
        );
        assert!(
            validate_response(
                &HostSessionRequest::JoinJob { job },
                &HostSessionResponse::Pending
            )
            .is_ok()
        );
        assert_eq!(
            validate_response(
                &HostSessionRequest::CloseJob { job },
                &HostSessionResponse::Joined
            ),
            Err(SessionCode::InvalidResponse)
        );
    }
    #[test]
    fn oversized_request_never_reaches_port() {
        let port = Arc::new(Port {
            calls: AtomicUsize::new(0),
            reply: HostSessionResponse::Pending,
        });
        let client = ProductHostSessionClient::bind(port.clone()).unwrap();
        let request = HostSessionRequest::Start {
            job: Uuid::from_u128(1),
            request_id: Uuid::from_u128(2),
            capability: "bcode.product".into(),
            kind: super::super::host_session_models::SessionServiceKind::Command,
            interface: "example/v1".into(),
            operation: "run".into(),
            payload: vec![0; 1025],
        };
        assert_eq!(
            ready(client.request(request)),
            Err(HostSessionClientError::Invalid(
                SessionCode::RequestLimitExceeded
            ))
        );
        assert_eq!(port.calls.load(Ordering::SeqCst), 0);
    }
    #[test]
    fn pages_reject_duplicate_foreign_and_nonadvancing_records() {
        assert!(!valid_page(
            [Uuid::from_u128(1), Uuid::from_u128(1)].into_iter(),
            None,
            None
        ));
        assert!(!valid_page(
            [Uuid::from_u128(1)].into_iter(),
            Some(Uuid::from_u128(2)),
            None
        ));
        assert!(!valid_page(
            [Uuid::from_u128(3)].into_iter(),
            None,
            Some(Uuid::from_u128(2))
        ));
        assert!(valid_page(
            [Uuid::from_u128(3)].into_iter(),
            None,
            Some(Uuid::from_u128(3))
        ));
    }
}
