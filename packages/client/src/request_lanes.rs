//! Sequential, cancellation-safe request lanes. No background tasks own these pools.

use super::{BcodeClient, ClientConnection, ClientError, Request, ResponsePayload};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::Semaphore;

const INTERACTIVE_CAPACITY: usize = 4;
const BACKGROUND_CAPACITY: usize = 1;
const MAX_IDLE: Duration = Duration::from_secs(30);
const NOT_HANDLED_RETRIES: u32 = 3;
const NOT_HANDLED_DELAY: Duration = Duration::from_millis(100);

/// A failure that proves the daemon never handled the request: it announced it stopped instead
/// of answering, or refused admission because it is stopping. Sending again (to the next
/// daemon, started for this real request) is no replay.
fn not_handled(error: &ClientError) -> bool {
    error.is_daemon_stopped()
        || matches!(error, ClientError::Server { code, .. } if code == "daemon_stopping")
}

#[derive(Debug)]
pub struct RequestLanes {
    interactive: Lane,
    background: Lane,
}

impl RequestLanes {
    /// Release only pooled connections; checked-out requests retain ownership.
    pub(super) fn clear_idle(&self) {
        for lane in [&self.interactive, &self.background] {
            lane.idle
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clear();
        }
    }
}

impl Default for RequestLanes {
    fn default() -> Self {
        Self {
            interactive: Lane::new(INTERACTIVE_CAPACITY),
            background: Lane::new(BACKGROUND_CAPACITY),
        }
    }
}

#[derive(Debug)]
struct Lane {
    capacity: Semaphore,
    idle: Mutex<Vec<IdleConnection>>,
}

#[derive(Debug)]
struct IdleConnection {
    connection: ClientConnection,
    returned: tokio::time::Instant,
}

impl Lane {
    fn new(capacity: usize) -> Self {
        Self {
            capacity: Semaphore::new(capacity),
            idle: Mutex::new(Vec::with_capacity(capacity)),
        }
    }
}

/// Only explicitly audited, connection-stateless operations may reuse a handshake.
/// New protocol requests remain dedicated by default. In particular, attachments,
/// subscriptions, diagnostic capabilities, commands/controls, context changes and
/// ownership-bearing mutations must not inherit a pooled client identity.
/// Server status is also dedicated: lifecycle preflight must not retain an idle
/// socket that makes a subsequent idle-only stop see an extra connected client.
const fn reusable(request: &Request) -> bool {
    matches!(
        request,
        Request::Ping
            | Request::ListSessions { .. }
            | Request::ListSessionsWithScope { .. }
            | Request::ListSessionsPageWithScope { .. }
            | Request::SessionHistory { .. }
            | Request::SessionHistoryPage { .. }
            | Request::SessionHistoryAround { .. }
            | Request::SessionProjectionHistoryPage { .. }
            | Request::GetSubmission { .. }
            | Request::PendingSubmissions { .. }
            | Request::SubmissionQueueStatus { .. }
            | Request::ListAgents
            | Request::ListSkills
            | Request::DescribeSkill { .. }
            | Request::ListPluginServices
            | Request::ListPluginContributions
            | Request::ModelCatalogDiagnostics
            | Request::SessionInspection { .. }
            | Request::SessionStorageUsage { .. }
            | Request::SessionUsage { .. }
            | Request::SessionSummary { .. }
            | Request::SessionActivity { .. }
            | Request::SessionChildren { .. }
            | Request::SessionProgressSnapshot { .. }
            | Request::IngestClientMetrics { .. }
    )
}

impl BcodeClient {
    pub(super) async fn send_on_request_lane(
        &self,
        request: Request,
        timeout: Duration,
    ) -> Result<ResponsePayload, ClientError> {
        if !reusable(&request) {
            let mut attempt = 0;
            loop {
                let mut connection = self.connect("bcode-cli").await?;
                connection.request_timeout = timeout;
                match connection.send_request_ref(&request).await {
                    // Sent again only when it was certainly not handled (see `not_handled`).
                    Err(error) if not_handled(&error) && attempt < NOT_HANDLED_RETRIES => {
                        attempt += 1;
                        // A stopping daemon is exiting: let it go before starting another.
                        tokio::time::sleep(NOT_HANDLED_DELAY).await;
                    }
                    result => return result,
                }
            }
        }
        // The request deadline includes capacity wait, handshake, writes and reads.
        // Dropping this future at ANY await drops its exclusively owned connection.
        // No uncertain frame is ever returned to the pool, and no request is replayed.
        tokio::time::timeout(timeout, self.send_on_request_lane_inner(request))
            .await
            .map_err(|_| ClientError::RequestTimeout { timeout })?
    }

    async fn send_on_request_lane_inner(
        &self,
        request: Request,
    ) -> Result<ResponsePayload, ClientError> {
        let lane = if matches!(&request, Request::IngestClientMetrics { .. }) {
            &self.request_lanes.background
        } else {
            &self.request_lanes.interactive
        };
        let _permit = lane
            .capacity
            .acquire()
            .await
            .map_err(|error| ClientError::Protocol(format!("request lane closed: {error}")))?;
        let idle = {
            let mut idle = lane.idle.lock().expect("request lane lock poisoned");
            // Lazy expiration keeps both retained sockets and bookkeeping bounded,
            // without timer tasks that outlive clients or retain their contexts.
            idle.retain(|entry| entry.returned.elapsed() < MAX_IDLE);
            idle.pop()
        };
        let pooled = idle.is_some();
        let mut connection = if let Some(idle) = idle {
            let connection = idle.connection;
            let daemon = connection
                .verified_daemon
                .as_ref()
                .ok_or(ClientError::UnexpectedResponse)?;
            if let Some(target) = &self.explicit_daemon_target {
                target
                    .verify_executable()
                    .map_err(|error| ClientError::IncompatibleDaemon {
                        message: error.to_string(),
                    })?;
            }
            self.verify_server_identity(daemon)?;
            connection
        } else {
            let mut connection = self.connect("bcode-cli").await?;
            // A pooled connection must not own a client clone which owns this pool.
            // It also must never use subscription reconnect-and-restore machinery.
            connection.reconnect_client = None;
            connection
        };
        let retry = pooled.then(|| request.clone());
        let mut result = connection.send_lane_request(request).await;
        // A pooled socket outlives a daemon that stopped for idleness. When the daemon said so
        // before answering, it never handled this request, so sending it once more on a fresh
        // connection (which starts the daemon again for this real request) is no replay. Any
        // other failure stays uncertain and is not retried.
        if let Some(request) = retry
            && result.as_ref().is_err_and(ClientError::is_daemon_stopped)
        {
            connection = self.connect("bcode-cli").await?;
            connection.reconnect_client = None;
            result = connection.send_lane_request(request).await;
        }
        if result.is_ok() {
            lane.idle
                .lock()
                .expect("request lane lock poisoned")
                .push(IdleConnection {
                    connection,
                    returned: tokio::time::Instant::now(),
                });
        }
        result
    }

    /// Detach this builder from sibling clones before changing handshake context.
    pub(super) fn reset_request_lanes(&mut self) {
        self.request_lanes = Arc::new(RequestLanes::default());
        self.activity_watches = Arc::default();
    }
}

impl ClientConnection {
    /// A one-request exchange met by the daemon's shutdown announcement instead of an answer:
    /// the request was not handled. The client goes dormant.
    pub(super) fn observe_shutdown(
        &self,
        envelope: &bcode_ipc::Envelope,
    ) -> Result<(), ClientError> {
        if envelope.kind == super::EnvelopeKind::Event
            && let Ok(super::Event::DaemonShutdown { reason }) =
                super::decode_event(&envelope.payload)
        {
            self.presence.stopped(reason);
            return Err(ClientError::DaemonStopped { reason });
        }
        Ok(())
    }

    /// Strict one-request/one-response framing for connections with no subscriptions.
    /// An unsolicited frame is uncertainty, not something to skip or buffer forever.
    pub(super) async fn send_lane_request(
        &mut self,
        request: Request,
    ) -> Result<ResponsePayload, ClientError> {
        let request_id = self.next_request_id;
        self.next_request_id = request_id
            .checked_add(1)
            .ok_or_else(|| ClientError::Protocol("request ID exhausted".into()))?;
        let envelope = super::request_envelope(request_id, &request)?;
        super::send_envelope(&mut self.stream, &envelope).await?;
        let envelope = super::recv_envelope(&mut self.stream).await?;
        self.observe_shutdown(&envelope)?;
        if envelope.kind != super::EnvelopeKind::Response || envelope.request_id != request_id {
            return Err(ClientError::UnexpectedResponse);
        }
        match super::decode_response(&envelope.payload)? {
            super::Response::Ok(ResponsePayload::SessionCompactionAccepted) => {
                Err(ClientError::UnexpectedResponse)
            }
            super::Response::Ok(payload) => Ok(payload),
            // Conservatively discard even a clean application-error response.
            super::Response::Err(error) => Err(error.into()),
        }
    }
}

#[cfg(all(test, unix))]
mod tests;
