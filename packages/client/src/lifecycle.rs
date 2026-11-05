//! Instance-fenced idle stop and durable execution-authority observations.

use crate::{BcodeClient, ClientError, Request, ResponsePayload};
use bcode_daemon_lifecycle::application::{
    RuntimeApplicationFailure, RuntimeExecutionAuthority, RuntimeInstanceIdentity,
    RuntimeQuiescence, RuntimeQuiescenceApplication, RuntimeStopReceipt,
};

fn failure(error: &ClientError) -> RuntimeApplicationFailure {
    let code = match error {
        ClientError::Server { code, .. }
            if matches!(
                code.as_str(),
                "daemon_busy"
                    | "runtime_instance_changed"
                    | "runtime_observation_changed"
                    | "unsupported_runtime_observation"
                    | "runtime_identity_unavailable"
                    | "runtime_admission_unavailable"
            ) =>
        {
            code.as_str()
        }
        _ => "runtime_lifecycle_unavailable",
    };
    RuntimeApplicationFailure {
        code: code.into(),
        message: "runtime lifecycle request failed or returned incompatible evidence".into(),
    }
}

/// A single verified connection retained across observation and stop admission.
/// It never reconnects, starts a daemon, or silently follows a replacement instance.
pub struct RuntimeLifecycleClient {
    connection: tokio::sync::Mutex<Option<crate::ClientConnection>>,
    instance: RuntimeInstanceIdentity,
}

impl BcodeClient {
    /// Observe only the receipt's exact execution authority at this client's retained location.
    /// Endpoint-only clients without resolved location authority fail closed.
    #[must_use]
    pub fn runtime_execution_authority(
        &self,
        receipt: &RuntimeStopReceipt,
    ) -> RuntimeExecutionAuthority {
        if let Some(target) = &self.explicit_daemon_target {
            if receipt.instance.state_location_id != target.state_scope_id() {
                return RuntimeExecutionAuthority::Unverifiable;
            }
            return execution_authority(target.state_location().root(), receipt);
        }
        let Some(location) = &self.lifecycle_state_location else {
            return RuntimeExecutionAuthority::Unverifiable;
        };
        if self.lifecycle_scope_id.as_deref() != Some(receipt.instance.state_location_id.as_str()) {
            return RuntimeExecutionAuthority::Unverifiable;
        }
        execution_authority(location.root(), receipt)
    }

    /// Await exact execution-authority release under one caller-owned absolute deadline.
    /// Uses bounded nonblocking evidence observations with a yielding 25ms polling policy;
    /// this is not a gapless operating-system event stream or process-death proof.
    /// Returns the last evidence at the deadline, including `Live` when not released.
    pub async fn wait_runtime_execution_authority(
        &self,
        receipt: &RuntimeStopReceipt,
        deadline: tokio::time::Instant,
    ) -> RuntimeExecutionAuthority {
        loop {
            let evidence = self.runtime_execution_authority(receipt);
            if evidence != RuntimeExecutionAuthority::Live
                || tokio::time::Instant::now() >= deadline
            {
                return evidence;
            }
            let next =
                (tokio::time::Instant::now() + std::time::Duration::from_millis(25)).min(deadline);
            tokio::time::sleep_until(next).await;
        }
    }

    /// Open lifecycle control bound to an already verified exact daemon identity.
    /// # Errors
    /// Rejects unavailable daemons, absent identity, and replacement instances.
    pub async fn runtime_lifecycle(
        &self,
        expected: RuntimeInstanceIdentity,
    ) -> Result<RuntimeLifecycleClient, RuntimeApplicationFailure> {
        let client = self
            .clone()
            .with_daemon_availability(crate::DaemonAvailability::RequireRunning);
        let mut connection = client
            .connect("bcode-lifecycle")
            .await
            .map_err(|error| failure(&error))?;
        let identity = instance_identity(
            connection
                .verified_daemon
                .as_ref()
                .ok_or_else(|| failure(&ClientError::UnexpectedResponse))?,
        )?;
        if identity != expected {
            return Err(failure(&ClientError::UnexpectedResponse));
        }
        connection.reconnect_client = None;
        Ok(RuntimeLifecycleClient {
            connection: tokio::sync::Mutex::new(Some(connection)),
            instance: identity,
        })
    }
}

impl RuntimeLifecycleClient {
    async fn request(
        &self,
        request: Request,
    ) -> Result<ResponsePayload, RuntimeApplicationFailure> {
        let mut slot = self.connection.lock().await;
        let mut connection = slot
            .take()
            .ok_or_else(|| failure(&ClientError::UnexpectedResponse))?;
        let request_timeout = match &request {
            Request::RuntimeQuiescence { wait_ms, .. } => connection.request_timeout.max(
                std::time::Duration::from_millis(u64::from(*wait_ms).min(30_000))
                    + std::time::Duration::from_secs(1),
            ),
            _ => connection.request_timeout,
        };
        connection.request_timeout = request_timeout;
        // Cancellation drops the owned connection; partial framing is never reused.
        // One deadline covers write and response frames; it is never renewed per frame.
        let deadline = tokio::time::Instant::now() + request_timeout;
        let result = tokio::time::timeout_at(deadline, connection.send_request(request))
            .await
            .map_err(|_| ClientError::RequestTimeout {
                timeout: request_timeout,
            })
            .and_then(std::convert::identity);
        if result.is_ok() || matches!(&result, Err(ClientError::Server { .. })) {
            *slot = Some(connection);
        }
        drop(slot);
        result.map_err(|error| failure(&error))
    }
}

impl RuntimeQuiescenceApplication for RuntimeLifecycleClient {
    async fn runtime_quiescence(
        &self,
        expected_instance: RuntimeInstanceIdentity,
        after: Option<RuntimeQuiescence>,
        wait_ms: u32,
    ) -> Result<RuntimeQuiescence, RuntimeApplicationFailure> {
        if expected_instance != self.instance {
            return Err(failure(&ClientError::UnexpectedResponse));
        }
        let response = self
            .request(Request::RuntimeQuiescence {
                expected_instance: expected_instance.clone(),
                after,
                wait_ms: wait_ms.min(30_000),
            })
            .await?;
        match response {
            ResponsePayload::RuntimeQuiescence { snapshot }
                if snapshot.version == 1 && snapshot.instance == expected_instance =>
            {
                Ok(snapshot)
            }
            _ => Err(failure(&ClientError::UnexpectedResponse)),
        }
    }

    async fn stop_quiescent_runtime(
        &self,
        expected: RuntimeQuiescence,
    ) -> Result<RuntimeStopReceipt, RuntimeApplicationFailure> {
        let generation = expected.admission_generation;
        let instance = expected.instance.clone();
        if instance != self.instance {
            return Err(failure(&ClientError::UnexpectedResponse));
        }
        let response = self
            .request(Request::StopQuiescentRuntime { expected })
            .await?;
        match response {
            ResponsePayload::RuntimeStopped { receipt }
                if receipt.version == 1
                    && receipt.instance == instance
                    && receipt.admission_generation == generation =>
            {
                Ok(receipt)
            }
            _ => Err(failure(&ClientError::UnexpectedResponse)),
        }
    }
}

/// Validate and retain an exact identity from a verified daemon status.
/// # Errors
/// Rejects missing instance, artifact, or state-location identity.
pub fn instance_identity(
    daemon: &bcode_ipc::DaemonStatus,
) -> Result<RuntimeInstanceIdentity, RuntimeApplicationFailure> {
    let artifact = daemon.artifact_id.as_ref();
    let location = daemon.state_location_id.as_deref();
    match (artifact, location) {
        (Some(artifact), Some(location))
            if !daemon.instance_id.is_empty() && !location.is_empty() =>
        {
            Ok(RuntimeInstanceIdentity {
                instance_id: daemon.instance_id.clone(),
                artifact_id: artifact.to_string(),
                state_location_id: location.into(),
            })
        }
        _ => Err(RuntimeApplicationFailure {
            code: "runtime_identity_unavailable".into(),
            message: "daemon exact identity is unavailable".into(),
        }),
    }
}

/// Independently observe the exact instance named by an accepted stop receipt.
/// A released execution capability is not proof of operating-system process death.
#[must_use]
pub fn execution_authority(
    state_root: &std::path::Path,
    receipt: &RuntimeStopReceipt,
) -> RuntimeExecutionAuthority {
    if receipt.version != 1 {
        return RuntimeExecutionAuthority::Unverifiable;
    }
    bcode_daemon_lifecycle::application::runtime_execution_authority(state_root, &receipt.instance)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_only_client_never_substitutes_ambient_state_authority() {
        let client = BcodeClient::new(bcode_ipc::default_endpoint());
        let receipt = RuntimeStopReceipt {
            version: 1,
            instance: RuntimeInstanceIdentity {
                instance_id: "instance".into(),
                artifact_id: "artifact".into(),
                state_location_id: "location".into(),
            },
            admission_generation: 0,
        };
        assert_eq!(
            client.runtime_execution_authority(&receipt),
            RuntimeExecutionAuthority::Unverifiable
        );
    }

    #[test]
    fn exact_lifetime_release_is_independent_of_replacement_and_not_process_death() {
        use bcode_daemon_lifecycle::{DaemonRecord, ExecutionLifetime};
        let root = tempfile::tempdir().unwrap();
        let mut record = DaemonRecord::current(
            &bcode_ipc::default_endpoint(),
            root.path().join("daemon.log"),
            None,
            "retained-instance".into(),
        )
        .unwrap();
        record.artifact_id = Some(bcode_ipc::ArtifactId::parse("test-artifact").unwrap());
        record.state_location_id = Some("test-state".into());
        let receipt = RuntimeStopReceipt {
            version: 1,
            instance: RuntimeInstanceIdentity {
                instance_id: record.instance_id.clone(),
                artifact_id: "test-artifact".into(),
                state_location_id: "test-state".into(),
            },
            admission_generation: 7,
        };
        assert_eq!(
            execution_authority(root.path(), &receipt),
            RuntimeExecutionAuthority::Missing
        );
        let lifetime = ExecutionLifetime::begin(root.path(), &record).unwrap();
        assert_eq!(
            execution_authority(root.path(), &receipt),
            RuntimeExecutionAuthority::Live
        );
        let mut replacement = record;
        replacement.instance_id = "replacement-instance".into();
        let _replacement = ExecutionLifetime::begin(root.path(), &replacement).unwrap();
        // Neither removal of registry/endpoint information nor another instance can release this guard.
        assert_eq!(
            execution_authority(root.path(), &receipt),
            RuntimeExecutionAuthority::Live
        );
        drop(lifetime);
        assert_eq!(
            execution_authority(root.path(), &receipt),
            RuntimeExecutionAuthority::Released
        );
        let mut unsupported = receipt;
        unsupported.version = 2;
        assert_eq!(
            execution_authority(root.path(), &unsupported),
            RuntimeExecutionAuthority::Unverifiable
        );
    }
}
