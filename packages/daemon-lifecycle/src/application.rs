//! Portable daemon runtime facts and explicit shutdown admission.

use std::future::Future;

// IPC owns the wire-compatible data boundary; lifecycle owns the application capability.
pub use bcode_ipc::{
    RuntimeIdleBlocker, RuntimeIdleBlockerKind, RuntimeInstanceIdentity, RuntimeQuiescence,
    RuntimeStopReceipt,
};

/// Exact instance's durable execution-authority evidence; never process-death proof.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeExecutionAuthority {
    Live,
    Released,
    Missing,
    Unverifiable,
}

/// Observe only the retained instance, independently of registry or endpoint replacement.
#[must_use]
pub fn runtime_execution_authority(
    state_root: &std::path::Path,
    instance: &RuntimeInstanceIdentity,
) -> RuntimeExecutionAuthority {
    match crate::execution_lifetime_status(
        state_root,
        &instance.instance_id,
        &instance.artifact_id,
        &instance.state_location_id,
    ) {
        crate::ExecutionLifetimeStatus::Live => RuntimeExecutionAuthority::Live,
        crate::ExecutionLifetimeStatus::Released => RuntimeExecutionAuthority::Released,
        crate::ExecutionLifetimeStatus::Missing => RuntimeExecutionAuthority::Missing,
        crate::ExecutionLifetimeStatus::Unverifiable => RuntimeExecutionAuthority::Unverifiable,
    }
}

/// Caller-bound, instance-fenced shutdown operations. Waiting does not reserve idleness.
pub trait RuntimeQuiescenceApplication: Sync {
    /// Wait for a bounded change in facts, or return current facts at the deadline.
    /// # Errors
    /// Rejects unsupported observations and changed/unverifiable instances.
    fn runtime_quiescence(
        &self,
        expected_instance: RuntimeInstanceIdentity,
        after: Option<RuntimeQuiescence>,
        wait_ms: u32,
    ) -> impl Future<Output = Result<RuntimeQuiescence, RuntimeApplicationFailure>> + Send;

    /// Atomically reserve and stop only the observed, still-quiescent instance.
    /// # Errors
    /// Rejects changed instance/generation or any live blocker.
    fn stop_quiescent_runtime(
        &self,
        expected: RuntimeQuiescence,
    ) -> impl Future<Output = Result<RuntimeStopReceipt, RuntimeApplicationFailure>> + Send;
}

/// Secret-safe extensible lifecycle failure.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeApplicationFailure {
    /// Stable category.
    pub code: String,
    /// Normalized diagnostic.
    pub message: String,
}

/// Runtime facts without transport endpoints, credentials or private persistence handles.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeStatus {
    /// Currently connected clients.
    pub connected_client_count: usize,
    /// Selected provider identity.
    pub selected_provider_plugin_id: Option<String>,
    /// Selected model identity.
    pub selected_model_id: Option<String>,
    /// Why idle shutdown is deferred.
    pub idle_shutdown_blocker: Option<String>,
    /// Why the optional workflow domain is unavailable.
    pub workflow_unavailable_reason: Option<String>,
}

/// Explicit existing shutdown policy; force is not inferred from missing input.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeShutdownMode {
    /// Request shutdown despite active work using the existing cancellation lifecycle.
    Force,
    /// Reject admission while work or ownership prevents idle shutdown.
    IfIdle,
}

/// Admission receipt, never proof the process stopped or work had no effects.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RuntimeShutdownAccepted {
    /// Admitted policy.
    pub mode: RuntimeShutdownMode,
}

/// A prepared, single-use shutdown operation supplied by the owning application.
///
/// Preparation reserves new admission but does not signal shutdown. Dropping an uncommitted
/// operation has no effect. This is not a serializable token or durable resume contract.
pub trait RuntimeShutdownOperation: Send {
    /// Read the admission receipt before publishing an adapter acknowledgement.
    #[must_use]
    fn admission(&self) -> RuntimeShutdownAccepted;
    /// Consume this operation and signal shutdown through the existing lifecycle owner.
    /// The receipt does not prove that the process stopped or cancellation completed.
    #[must_use]
    fn commit(self) -> RuntimeShutdownAccepted;
}

/// Host-side preparation boundary for adapters that acknowledge before committing.
/// Remote clients need not expose preparation without a corresponding wire contract.
pub trait RuntimeShutdownPreparationApplication: Sync {
    /// Non-transferable operation borrowing the verified owning application.
    type ShutdownOperation<'a>: RuntimeShutdownOperation + Send
    where
        Self: 'a;

    /// Validate the existing shutdown policy without signaling shutdown.
    /// # Errors
    /// Rejects unavailable applications, migration blockers and idle-policy blockers.
    fn prepare_runtime_shutdown(
        &self,
        mode: RuntimeShutdownMode,
    ) -> impl Future<Output = Result<Self::ShutdownOperation<'_>, RuntimeApplicationFailure>> + Send;
}

/// Caller-bound access to the serving runtime, not process registry maintenance.
pub trait RuntimeApplication: Sync {
    /// Inspect existing runtime facts without repairing state.
    /// # Errors
    /// Fails when the runtime or verified caller is unavailable.
    fn runtime_status(
        &self,
    ) -> impl Future<Output = Result<RuntimeStatus, RuntimeApplicationFailure>> + Send;
    /// Admit explicit shutdown through the owning lifecycle coordinator.
    /// # Errors
    /// Fails when idle policy blocks shutdown or the runtime is unavailable.
    fn request_runtime_shutdown(
        &self,
        mode: RuntimeShutdownMode,
    ) -> impl Future<Output = Result<RuntimeShutdownAccepted, RuntimeApplicationFailure>> + Send;
}
