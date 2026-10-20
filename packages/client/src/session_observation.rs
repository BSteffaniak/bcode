//! Persistent client semantic observation. Projection remains entirely server-owned.
//!
//! A receive owns at most one retained pull future. Cancelling `recv` leaves that
//! future intact; closing or dropping releases the socket and all ephemeral host
//! resources. Lost transport ends explicitly, never silently reconnects or replays.

use std::future::Future;
use std::pin::Pin;

use bcode_session_models::observation::{
    MAX_HOSTED_OBSERVATION_BYTES, MAX_HOSTED_OBSERVATION_UPDATES, MAX_OBSERVATION_PULL_WAIT_MS,
    SESSION_OBSERVATION_SCHEMA_VERSION, SessionObservationApplication, SessionObservationEnd,
    SessionObservationEnvelope, SessionObservationError, SessionObservationEvent,
    SessionObservationHandle, SessionObservationPullRequest, SessionObservationPullResult,
    SessionObservationRequest, SessionObservationStream,
};
use bcode_session_view_models::SessionViewSnapshot;

use crate::{BcodeClient, ClientConnection, ClientError, Request, ResponsePayload};

type Delivery = SessionObservationEnvelope<SessionViewSnapshot>;
type PullFuture = Pin<
    Box<
        dyn Future<
                Output = (
                    ClientConnection,
                    Result<SessionObservationPullResult<SessionViewSnapshot>, ClientError>,
                ),
            > + Send,
    >,
>;

/// Connection-scoped normalized semantic stream; no durable reconnect is promised.
pub struct ClientSessionObservation {
    connection: Option<ClientConnection>,
    pull: Option<PullFuture>,
    handle: SessionObservationHandle,
    next_sequence: u64,
    source_sequence: Option<u64>,
    max_snapshot_bytes: usize,
    pending_end: Option<SessionObservationEnd>,
    ended: bool,
}

impl std::fmt::Debug for ClientSessionObservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientSessionObservation")
            .field("sequence", &self.next_sequence)
            .field("ended", &self.ended)
            .finish_non_exhaustive()
    }
}

impl SessionObservationApplication for BcodeClient {
    type Observation = ClientSessionObservation;

    async fn observe_session(
        &self,
        request: SessionObservationRequest,
    ) -> Result<Self::Observation, SessionObservationError> {
        if request.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION {
            return Err(SessionObservationError::UnsupportedSchema);
        }
        if request.max_snapshot_bytes == 0
            || request.max_updates == 0
            || request.max_snapshot_bytes > MAX_HOSTED_OBSERVATION_BYTES
            || request.max_updates > MAX_HOSTED_OBSERVATION_UPDATES
            || request.window.limits.max_bytes > MAX_HOSTED_OBSERVATION_BYTES
            || request.window.limits.max_items > request.max_updates
            || request.window.limits.max_events_scanned > request.max_updates
        {
            return Err(SessionObservationError::InvalidRequest);
        }
        let mut connection = self
            .connect("semantic-session-observation")
            .await
            .map_err(|_| SessionObservationError::Unavailable)?;
        let response = connection
            .send_request(Request::OpenSessionObservation {
                request: request.clone(),
            })
            .await
            .map_err(|error| normalize_admission_error(&error))?;
        let ResponsePayload::SessionObservationOpened { handle } = response else {
            return Err(SessionObservationError::Unavailable);
        };
        if handle.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION
            || handle.session_id != request.session_id
            || handle.handle_id.is_empty()
            || handle.handle_id.len() > 64
        {
            return Err(SessionObservationError::Unavailable);
        }
        Ok(ClientSessionObservation {
            connection: Some(connection),
            pull: None,
            handle,
            next_sequence: 0,
            source_sequence: None,
            max_snapshot_bytes: request.max_snapshot_bytes,
            pending_end: None,
            ended: false,
        })
    }
}

fn normalize_admission_error(error: &ClientError) -> SessionObservationError {
    let ClientError::Server { code, .. } = error else {
        return SessionObservationError::Unavailable;
    };
    match code.as_str() {
        "session_observation_unsupported_schema" => SessionObservationError::UnsupportedSchema,
        "session_observation_invalid_request" => SessionObservationError::InvalidRequest,
        "session_observation_not_found" => SessionObservationError::NotFound,
        "session_observation_unauthorized" => SessionObservationError::Unauthorized,
        "session_observation_conflict" => SessionObservationError::Conflict,
        "session_observation_limit_reached" => SessionObservationError::LimitReached,
        _ => SessionObservationError::Unavailable,
    }
}

fn pull_error_end(error: &ClientError) -> SessionObservationEnd {
    if normalize_admission_error(error) == SessionObservationError::LimitReached {
        SessionObservationEnd::LimitReached
    } else {
        SessionObservationEnd::Unavailable
    }
}

impl ClientSessionObservation {
    /// Receive ordered semantic state or exactly one terminal outcome. Cancelling
    /// this future retains the same in-flight request and cannot discard its reply.
    pub async fn recv(&mut self) -> Option<Delivery> {
        if self.ended {
            return None;
        }
        if let Some(reason) = self.pending_end.take() {
            return Some(self.end_delivery(reason));
        }
        loop {
            if self.pull.is_none() {
                let Some(mut connection) = self.connection.take() else {
                    return Some(self.end_delivery(SessionObservationEnd::Unavailable));
                };
                let request = SessionObservationPullRequest {
                    schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
                    handle_id: self.handle.handle_id.clone(),
                    expected_sequence: self.next_sequence,
                    wait_ms: MAX_OBSERVATION_PULL_WAIT_MS,
                };
                self.pull = Some(Box::pin(async move {
                    let response = connection
                        .send_request(Request::PullSessionObservation { request })
                        .await;
                    let result = response.and_then(|response| match response {
                        ResponsePayload::SessionObservationPulled { result } => Ok(*result),
                        _ => Err(ClientError::UnexpectedResponse),
                    });
                    (connection, result)
                }));
            }
            let (connection, result) = self.pull.as_mut()?.await;
            self.pull = None;
            self.connection = Some(connection);
            let result = match result {
                Ok(result) => result,
                Err(error) => return Some(self.end_delivery(pull_error_end(&error))),
            };
            if validate_result(
                &self.handle,
                self.next_sequence,
                self.max_snapshot_bytes,
                &result,
            )
            .is_err()
            {
                return Some(self.end_delivery(SessionObservationEnd::IncompatibleSource));
            }
            if let Some(delivery) = result.delivery {
                self.next_sequence = self.next_sequence.checked_add(1)?;
                self.source_sequence = delivery.source_sequence;
                if matches!(delivery.event, SessionObservationEvent::Ended { .. }) {
                    self.ended = true;
                    self.connection = None;
                }
                return Some(delivery);
            }
            if result.finished {
                return Some(self.end_delivery(SessionObservationEnd::Unavailable));
            }
        }
    }

    /// Release the remote observation explicitly on this connection.
    ///
    /// # Errors
    /// Returns normalized unavailable when the transport cannot confirm closure.
    /// Local resources are always released, without cancelling session execution.
    pub async fn close_remote(&mut self) -> Result<(), SessionObservationError> {
        // Retained pulls cannot be interleaved with another request on this socket.
        // Dropping that socket is the supported disconnect cleanup path.
        if self.pull.is_some() {
            self.close();
            return Ok(());
        }
        let result = if let Some(connection) = self.connection.as_mut() {
            connection
                .send_request(Request::CloseSessionObservation {
                    handle: self.handle.clone(),
                })
                .await
                .map_err(|_| SessionObservationError::Unavailable)
                .and_then(|response| match response {
                    ResponsePayload::SessionObservationClosed { .. } => Ok(()),
                    _ => Err(SessionObservationError::Unavailable),
                })
        } else {
            Ok(())
        };
        self.close();
        result
    }

    /// Close without changing an established terminal outcome; drop releases the
    /// dedicated socket, and its host connection owns cleanup of the observation.
    pub fn close(&mut self) {
        self.pull = None;
        self.connection = None;
        if !self.ended && self.pending_end.is_none() {
            self.pending_end = Some(SessionObservationEnd::Closed);
        }
    }

    fn end_delivery(&mut self, reason: SessionObservationEnd) -> Delivery {
        self.pull = None;
        self.connection = None;
        self.ended = true;
        Delivery {
            schema_version: SESSION_OBSERVATION_SCHEMA_VERSION,
            session_id: self.handle.session_id,
            sequence: self.next_sequence,
            source_sequence: self.source_sequence,
            event: SessionObservationEvent::Ended { reason },
        }
    }
}

impl SessionObservationStream for ClientSessionObservation {
    type Snapshot = SessionViewSnapshot;
    async fn recv(&mut self) -> Option<Delivery> {
        Self::recv(self).await
    }
    fn close(&mut self) {
        Self::close(self);
    }
}

fn validate_result(
    handle: &SessionObservationHandle,
    expected_sequence: u64,
    max_snapshot_bytes: usize,
    result: &SessionObservationPullResult<SessionViewSnapshot>,
) -> Result<(), SessionObservationError> {
    if result.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION
        || result.handle_id != handle.handle_id
    {
        return Err(SessionObservationError::Unavailable);
    }
    if let Some(delivery) = &result.delivery {
        if result.finished
            || delivery.schema_version != SESSION_OBSERVATION_SCHEMA_VERSION
            || delivery.session_id != handle.session_id
            || delivery.sequence != expected_sequence
            || delivery.sequence == u64::MAX
        {
            return Err(SessionObservationError::Conflict);
        }
        if let SessionObservationEvent::Snapshot { initial, snapshot } = &delivery.event
            && (*initial != (expected_sequence == 0) || !encoded_fits(snapshot, max_snapshot_bytes))
        {
            return Err(SessionObservationError::InvalidRequest);
        }
    }
    Ok(())
}

fn encoded_fits(value: &impl serde::Serialize, limit: usize) -> bool {
    struct Budget(usize);
    impl std::io::Write for Budget {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 = self
                .0
                .checked_sub(bytes.len())
                .ok_or_else(|| std::io::Error::other("semantic observation exceeds size limit"))?;
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    serde_json::to_writer(Budget(limit), value).is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_session_models::SessionId;

    fn handle() -> SessionObservationHandle {
        SessionObservationHandle {
            schema_version: 1,
            handle_id: "opaque-token".into(),
            session_id: SessionId::new(),
        }
    }
    #[test]
    fn delivery_validation_rejects_wrong_owner_session_version_and_sequence() {
        let handle = handle();
        let delivery = Delivery {
            schema_version: 1,
            session_id: handle.session_id,
            sequence: 0,
            source_sequence: Some(7),
            event: SessionObservationEvent::Ended {
                reason: SessionObservationEnd::SourceClosed,
            },
        };
        let mut result = SessionObservationPullResult {
            schema_version: 1,
            handle_id: handle.handle_id.clone(),
            delivery: Some(delivery),
            finished: false,
        };
        assert!(validate_result(&handle, 0, 1024, &result).is_ok());
        result.delivery.as_mut().unwrap().sequence = u64::MAX;
        assert!(validate_result(&handle, u64::MAX, 1024, &result).is_err());
        result.delivery.as_mut().unwrap().sequence = 0;
        assert!(validate_result(&handle, 1, 1024, &result).is_err());
        result.schema_version = 2;
        assert!(validate_result(&handle, 0, 1024, &result).is_err());
        result.schema_version = 1;
        result.handle_id = "foreign-token".into();
        assert!(validate_result(&handle, 0, 1024, &result).is_err());
    }
    #[test]
    fn pull_admission_denial_is_not_concealed_as_transport_loss() {
        let error = |code: &str| ClientError::Server {
            code: code.to_owned(),
            message: "untrusted server detail".to_owned(),
        };
        assert_eq!(
            pull_error_end(&error("session_observation_limit_reached")),
            SessionObservationEnd::LimitReached,
        );
        assert_eq!(
            pull_error_end(&error("session_observation_not_found")),
            SessionObservationEnd::Unavailable,
        );
        assert_eq!(
            pull_error_end(&ClientError::UnexpectedResponse),
            SessionObservationEnd::Unavailable,
        );
    }

    #[tokio::test]
    async fn closed_stream_delivers_one_stable_terminal_without_reopening() {
        let mut observation = ClientSessionObservation {
            connection: None,
            pull: None,
            handle: handle(),
            next_sequence: 4,
            source_sequence: Some(8),
            max_snapshot_bytes: 1024,
            pending_end: None,
            ended: false,
        };
        observation.close();
        observation.close();
        let end = observation.recv().await.unwrap();
        assert_eq!(end.sequence, 4);
        assert!(matches!(
            end.event,
            SessionObservationEvent::Ended {
                reason: SessionObservationEnd::Closed
            }
        ));
        observation.close();
        assert!(observation.recv().await.is_none());
    }
}
