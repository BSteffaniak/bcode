//! Bounded direct-agent cleanup through public, non-starting client operations.
use bcode_client::{BcodeClient, DaemonAvailability};
use bcode_session_models::SessionId;
use serde::Serialize;
use std::time::Duration;

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CancellationEvidence {
    #[default]
    NotAcknowledged,
    Acknowledged,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum IdleEvidence {
    #[default]
    NotObserved,
    SessionQuiescent,
    DaemonIdle,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopEvidence {
    #[default]
    NotAcknowledged,
    Acknowledged,
    EndpointUnavailableAfterAcknowledgement,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifetimeEvidence {
    #[default]
    Unverified,
}

#[derive(Debug, Default, Serialize)]
pub struct CleanupEvidence {
    pub cancellation: CancellationEvidence,
    pub idle: IdleEvidence,
    pub stop: StopEvidence,
    pub execution_lifetime: LifetimeEvidence,
    pub failure: Option<&'static str>,
}

impl CleanupEvidence {
    pub fn endpoint_stop_observed(&self) -> bool {
        self.stop == StopEvidence::EndpointUnavailableAfterAcknowledgement
            && self.idle == IdleEvidence::DaemonIdle
            && self.failure.is_none()
    }
}

fn probe() -> BcodeClient {
    BcodeClient::default_endpoint()
        .with_daemon_availability(DaemonAvailability::RequireRunning)
        .with_connect_timeout(Duration::from_millis(500))
        .with_request_timeout(Duration::from_secs(1))
}

fn quiescence(
    session: Option<SessionId>,
    isolated: bool,
    status: &bcode_ipc::ServerStatus,
) -> IdleEvidence {
    // Every session-scoped active registration counts, including lifetime work.
    if session.is_some_and(|id| {
        status
            .active_runtime_work
            .iter()
            .any(|work| work.session_id == id)
    }) {
        return IdleEvidence::NotObserved;
    }
    if isolated {
        if status.active_runtime_work.is_empty() && status.idle_shutdown_blocker.is_none() {
            IdleEvidence::DaemonIdle
        } else {
            IdleEvidence::NotObserved
        }
    } else if status.idle_shutdown_blocker.is_none() {
        IdleEvidence::SessionQuiescent
    } else {
        // The blocker is daemon-wide and cannot safely be attributed to another session.
        IdleEvidence::NotObserved
    }
}

/// Await idle-only shutdown acknowledgement and bounded endpoint disappearance.
/// Never treats protocol, identity or arbitrary status failures as exit evidence.
pub async fn stop_idle(timeout: Duration) -> CleanupEvidence {
    let client = probe();
    let mut evidence = CleanupEvidence::default();
    let deadline = tokio::time::Instant::now() + timeout;
    let mut unavailable_observations = 0;
    let operation = async {
        if let Ok(status) = client.verified_server_status().await {
            evidence.idle = quiescence(None, true, &status);
            if evidence.idle != IdleEvidence::DaemonIdle {
                evidence.failure = Some("daemon_not_idle");
                return;
            }
        } else {
            evidence.failure = Some("status_unverified");
            return;
        }
        if client.server_stop_if_idle().await.is_err() {
            evidence.failure = Some("idle_stop_rejected");
            return;
        }
        evidence.stop = StopEvidence::Acknowledged;
        loop {
            match client.verified_server_status().await {
                Err(error) if error.is_daemon_unavailable() => {
                    unavailable_observations += 1;
                    if unavailable_observations >= 2 {
                        evidence.stop = StopEvidence::EndpointUnavailableAfterAcknowledgement;
                        return;
                    }
                }
                Err(_) => {
                    evidence.failure = Some("stop_status_unverified");
                    return;
                }
                Ok(_) => unavailable_observations = 0,
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    if tokio::time::timeout_at(deadline, operation).await.is_err() {
        evidence.failure = Some("cleanup_deadline_exceeded");
    }
    evidence
}

pub async fn cleanup(session: Option<SessionId>, isolated: bool) -> CleanupEvidence {
    let client = probe();
    let mut evidence = CleanupEvidence::default();
    let operation = async {
        if let Some(session) = session {
            if client.cancel_session_turn(session).await.is_ok() {
                evidence.cancellation = CancellationEvidence::Acknowledged;
            } else {
                evidence.failure = Some("cancellation_unverified");
                return;
            }
        }
        loop {
            if let Ok(status) = client.verified_server_status().await {
                evidence.idle = quiescence(session, isolated, &status);
                if evidence.idle != IdleEvidence::NotObserved {
                    break;
                }
            } else {
                evidence.failure = Some("status_unverified");
                return;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    };
    if tokio::time::timeout(Duration::from_secs(5), operation)
        .await
        .is_err()
    {
        evidence.failure = Some("cleanup_deadline_exceeded");
    }
    if isolated && evidence.idle == IdleEvidence::DaemonIdle && evidence.failure.is_none() {
        let stopped = stop_idle(Duration::from_secs(5)).await;
        evidence.stop = stopped.stop;
        evidence.failure = stopped.failure;
    }
    evidence
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn arbitrary_status_errors_are_not_shutdown_evidence() {
        let error = bcode_client::ClientError::Server {
            code: "unavailable".into(),
            message: "synthetic error".into(),
        };
        assert!(!error.is_daemon_unavailable());
        assert!(!bcode_client::ClientError::UnexpectedResponse.is_daemon_unavailable());
        assert!(
            !bcode_client::ClientError::RequestTimeout {
                timeout: Duration::from_secs(1)
            }
            .is_daemon_unavailable()
        );
    }

    #[test]
    fn acknowledged_cancellation_does_not_override_active_work_or_idle_blockers() {
        let session = SessionId::new();
        let mut status: bcode_ipc::ServerStatus = serde_json::from_value(serde_json::json!({
            "connected_client_count":0,"sessions":[]
        }))
        .unwrap();
        status
            .active_runtime_work
            .push(bcode_ipc::SessionRuntimeWork {
                session_id: session,
                work: bcode_session_models::RuntimeWorkSnapshot {
                    work_id: bcode_session_models::WorkId::new("fixture"),
                    kind: bcode_session_models::RuntimeWorkKind::PluginInvocation,
                    label: "synthetic active work".into(),
                    tool_call_id: None,
                    status: bcode_session_models::RuntimeWorkStatus::Cancelling,
                    cancellable: true,
                    observation: None,
                },
            });
        assert_eq!(
            quiescence(Some(session), false, &status),
            IdleEvidence::NotObserved
        );
        assert_eq!(
            quiescence(Some(session), true, &status),
            IdleEvidence::NotObserved
        );
        status.active_runtime_work.clear();
        status.idle_shutdown_blocker = Some("synthetic queued turn".into());
        assert_eq!(
            quiescence(Some(session), true, &status),
            IdleEvidence::NotObserved
        );
        status.idle_shutdown_blocker = None;
        assert_eq!(
            quiescence(Some(session), true, &status),
            IdleEvidence::DaemonIdle
        );
    }

    #[test]
    fn endpoint_disappearance_never_claims_execution_lifetime_termination() {
        let mut evidence = CleanupEvidence {
            stop: StopEvidence::EndpointUnavailableAfterAcknowledgement,
            idle: IdleEvidence::DaemonIdle,
            ..CleanupEvidence::default()
        };
        assert_eq!(evidence.execution_lifetime, LifetimeEvidence::Unverified);
        assert_eq!(evidence.cancellation, CancellationEvidence::NotAcknowledged);
        assert!(evidence.endpoint_stop_observed());
        evidence.failure = Some("status_unverified");
        assert!(!evidence.endpoint_stop_observed());
    }
}
