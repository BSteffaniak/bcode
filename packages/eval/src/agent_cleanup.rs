//! Bounded direct-agent cleanup through public, non-starting client operations.
use bcode_client::{BcodeClient, DaemonAvailability};
use bcode_daemon_lifecycle::application::{
    RuntimeInstanceIdentity, RuntimeQuiescenceApplication, RuntimeStopReceipt,
};
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
    #[cfg(test)]
    EndpointUnavailableAfterAcknowledgement,
    InstanceFencedAcknowledgement,
}

#[derive(Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LifetimeEvidence {
    #[default]
    Unverified,
    /// The exact instance released its durable execution capability, not OS exit proof.
    ExecutionAuthorityReleased,
}

#[derive(Debug, Default, Serialize)]
pub struct CleanupEvidence {
    pub cancellation: CancellationEvidence,
    pub idle: IdleEvidence,
    pub stop: StopEvidence,
    pub execution_lifetime: LifetimeEvidence,
    /// Exact last verified daemon, retained even when settlement cannot be proved.
    pub last_observed_daemon: Option<bcode_ipc::DaemonStatus>,
    /// Last server-reported blocker; never inferred from an elapsed timeout.
    pub idle_shutdown_blocker: Option<String>,
    pub active_runtime_work_count: usize,
    pub stop_receipt: Option<RuntimeStopReceipt>,
    pub failure: Option<&'static str>,
}

impl CleanupEvidence {
    fn observe_status(&mut self, status: &bcode_ipc::ServerStatus) -> bool {
        if self
            .last_observed_daemon
            .as_ref()
            .is_some_and(|previous| previous != &status.daemon)
        {
            self.failure = Some("daemon_instance_changed");
            return false;
        }
        self.last_observed_daemon = Some(status.daemon.clone());
        self.idle_shutdown_blocker
            .clone_from(&status.idle_shutdown_blocker);
        self.active_runtime_work_count = status.active_runtime_work.len();
        true
    }

    pub fn execution_release_verified(&self) -> bool {
        self.stop == StopEvidence::InstanceFencedAcknowledgement
            && self.execution_lifetime == LifetimeEvidence::ExecutionAuthorityReleased
            && self.stop_receipt.is_some()
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

/// Await an instance-fenced stop receipt and independent execution-authority release.
/// Endpoint disappearance and cancellation acknowledgements never prove settlement.
pub async fn stop_idle(timeout: Duration) -> CleanupEvidence {
    stop_idle_until(tokio::time::Instant::now() + timeout).await
}

async fn stop_idle_until(deadline: tokio::time::Instant) -> CleanupEvidence {
    let client = probe();
    let mut evidence = CleanupEvidence::default();
    let operation = async {
        let Ok(status) = client.verified_server_status().await else {
            evidence.failure = Some("status_unverified");
            return;
        };
        if !evidence.observe_status(&status) {
            return;
        }
        let Some(artifact) = status.daemon.artifact_id.as_ref() else {
            evidence.failure = Some("instance_unverified");
            return;
        };
        let Some(location) = status.daemon.state_location_id.as_ref() else {
            evidence.failure = Some("instance_unverified");
            return;
        };
        let instance = RuntimeInstanceIdentity {
            instance_id: status.daemon.instance_id.clone(),
            artifact_id: artifact.to_string(),
            state_location_id: location.clone(),
        };
        let mut previous = None;
        let Ok(lifecycle) = client.runtime_lifecycle(instance.clone()).await else {
            evidence.failure = Some("quiescence_unverified");
            return;
        };
        loop {
            let Ok(snapshot) = lifecycle
                .runtime_quiescence(instance.clone(), previous, 1_000)
                .await
            else {
                evidence.failure = Some("quiescence_unverified");
                return;
            };
            if snapshot.blockers.is_empty() {
                evidence.idle = IdleEvidence::DaemonIdle;
                match lifecycle.stop_quiescent_runtime(snapshot.clone()).await {
                    Ok(receipt) => {
                        evidence.stop = StopEvidence::InstanceFencedAcknowledgement;
                        evidence.stop_receipt = Some(receipt);
                        break;
                    }
                    Err(error) if error.code == "daemon_busy" => {}
                    Err(_) => {
                        evidence.failure = Some("idle_stop_rejected");
                        return;
                    }
                }
            }
            previous = Some(snapshot);
        }
        // Release is verified by the lifecycle-owned exact-instance witness, not
        // transport probes. The wait is bounded by the original absolute budget.
        let receipt = evidence
            .stop_receipt
            .as_ref()
            .expect("accepted stop receipt");
        match client
            .wait_runtime_execution_authority(receipt, deadline)
            .await
        {
            bcode_daemon_lifecycle::application::RuntimeExecutionAuthority::Released => {
                evidence.execution_lifetime = LifetimeEvidence::ExecutionAuthorityReleased;
            }
            _ => evidence.failure = Some("execution_lifetime_unverified"),
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
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
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
                if !evidence.observe_status(&status) {
                    return;
                }
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
    if tokio::time::timeout_at(deadline, operation).await.is_err() {
        evidence.failure = Some("cleanup_deadline_exceeded");
    }
    if isolated && evidence.idle == IdleEvidence::DaemonIdle && evidence.failure.is_none() {
        let stopped = stop_idle_until(deadline).await;
        evidence.stop = stopped.stop;
        evidence.execution_lifetime = stopped.execution_lifetime;
        evidence.stop_receipt = stopped.stop_receipt;
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
    fn blocker_release_is_observed_without_inventing_cancellation_settlement() {
        let mut status: bcode_ipc::ServerStatus = serde_json::from_value(serde_json::json!({
            "connected_client_count": 1, "sessions": [],
            "daemon": {"instance_id": "original"},
            "idle_shutdown_blocker": "runtime work is settling"
        }))
        .unwrap();
        let mut evidence = CleanupEvidence::default();
        assert!(evidence.observe_status(&status));
        assert_eq!(quiescence(None, true, &status), IdleEvidence::NotObserved);
        assert_eq!(
            evidence.idle_shutdown_blocker.as_deref(),
            Some("runtime work is settling")
        );
        evidence.cancellation = CancellationEvidence::Acknowledged;
        assert_eq!(quiescence(None, true, &status), IdleEvidence::NotObserved);
        status.idle_shutdown_blocker = None;
        assert!(evidence.observe_status(&status));
        assert_eq!(quiescence(None, true, &status), IdleEvidence::DaemonIdle);
        assert!(evidence.idle_shutdown_blocker.is_none());
        assert_eq!(evidence.execution_lifetime, LifetimeEvidence::Unverified);
    }

    #[test]
    fn replacement_daemon_cannot_complete_original_cleanup() {
        let mut status: bcode_ipc::ServerStatus = serde_json::from_value(serde_json::json!({
            "connected_client_count": 1, "sessions": [],
            "daemon": {"instance_id": "original"}
        }))
        .unwrap();
        let mut evidence = CleanupEvidence::default();
        assert!(evidence.observe_status(&status));
        status.daemon.instance_id = "replacement".into();
        assert!(!evidence.observe_status(&status));
        assert_eq!(evidence.failure, Some("daemon_instance_changed"));
        assert_eq!(
            evidence.last_observed_daemon.unwrap().instance_id,
            "original"
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
        assert!(!evidence.execution_release_verified());
        evidence.stop = StopEvidence::InstanceFencedAcknowledgement;
        assert!(!evidence.execution_release_verified());
        evidence.execution_lifetime = LifetimeEvidence::ExecutionAuthorityReleased;
        assert!(!evidence.execution_release_verified());
        evidence.stop_receipt = Some(RuntimeStopReceipt {
            version: 1,
            instance: RuntimeInstanceIdentity {
                instance_id: "original".into(),
                artifact_id: "artifact".into(),
                state_location_id: "state".into(),
            },
            admission_generation: 1,
        });
        assert!(evidence.execution_release_verified());
        evidence.failure = Some("status_unverified");
        assert!(!evidence.execution_release_verified());
    }
}
