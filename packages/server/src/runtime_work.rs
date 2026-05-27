use bcode_ipc::RuntimeWorkSnapshot;
use bcode_plugin::PluginInvocationCancelHandle;
use bcode_session_models::{RuntimeWorkId, RuntimeWorkKind, RuntimeWorkStatus, SessionId};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::TurnCancelState;

/// Server-owned cancellation handle for active runtime work.
#[derive(Debug, Clone)]
pub enum CancellationHandle {
    /// Cancel a whole session/model turn.
    SessionTurn(Arc<TurnCancelState>),
    /// Cancel an in-flight plugin invocation.
    PluginInvocation(PluginInvocationCancelHandle),
}

impl CancellationHandle {
    /// Request cancellation through the underlying handle.
    pub fn cancel(&self) {
        match self {
            Self::SessionTurn(cancel_state) => cancel_state.cancel(),
            Self::PluginInvocation(cancel) => cancel.cancel(),
        }
    }

    /// Runtime work with a handle is cancellable.
    pub const fn is_cancellable(&self) -> bool {
        matches!(self, Self::SessionTurn(_) | Self::PluginInvocation(_))
    }
}

/// Specification for starting runtime work.
#[derive(Debug, Clone)]
pub struct RuntimeWorkSpec {
    pub work_id: RuntimeWorkId,
    pub kind: RuntimeWorkKind,
    pub label: String,
    pub tool_call_id: Option<String>,
    pub plugin_id: Option<String>,
    pub service_interface: Option<String>,
    pub operation: Option<String>,
    pub cancellation: CancellationHandle,
}

impl RuntimeWorkSpec {
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        work_id: RuntimeWorkId,
        kind: RuntimeWorkKind,
        label: String,
        cancellation: CancellationHandle,
    ) -> Self {
        Self {
            work_id,
            kind,
            label,
            tool_call_id: None,
            plugin_id: None,
            service_interface: None,
            operation: None,
            cancellation,
        }
    }

    pub fn with_tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }
}

#[derive(Debug, Clone)]
struct ActiveRuntimeWork {
    spec: RuntimeWorkSpec,
    cancelled: bool,
}

/// Central server registry and cancellation router for active runtime work.
#[derive(Debug, Default)]
pub struct RuntimeWorkManager {
    active: Mutex<BTreeMap<(SessionId, RuntimeWorkId), ActiveRuntimeWork>>,
}

impl RuntimeWorkManager {
    /// Register active work and return whether it should be advertised as cancellable.
    pub async fn start(&self, session_id: SessionId, spec: RuntimeWorkSpec) -> bool {
        let cancellable = spec.cancellation.is_cancellable();
        self.active.lock().await.insert(
            (session_id, spec.work_id.clone()),
            ActiveRuntimeWork {
                spec,
                cancelled: false,
            },
        );
        cancellable
    }

    /// Replace the cancellation handle for existing active work.
    pub async fn replace_cancellation(
        &self,
        session_id: SessionId,
        work_id: &RuntimeWorkId,
        cancellation: CancellationHandle,
    ) -> bool {
        let mut active = self.active.lock().await;
        let replaced = if let Some(work) = active.get_mut(&(session_id, work_id.clone())) {
            work.spec.cancellation = cancellation;
            true
        } else {
            false
        };
        drop(active);
        replaced
    }

    /// Cancel active work by exact ID. Returns false if no such active work exists.
    pub async fn cancel(&self, session_id: SessionId, work_id: &RuntimeWorkId) -> bool {
        let mut active = self.active.lock().await;
        let Some(work) = active.get_mut(&(session_id, work_id.clone())) else {
            return false;
        };
        if work.cancelled {
            return true;
        }
        work.cancelled = true;
        let cancellation = work.spec.cancellation.clone();
        drop(active);
        cancellation.cancel();
        true
    }

    /// Finish work and remove it from the active registry.
    pub async fn finish(&self, session_id: SessionId, work_id: &RuntimeWorkId) {
        self.active
            .lock()
            .await
            .remove(&(session_id, work_id.clone()));
    }

    /// Return active work snapshots for a session.
    pub async fn active_for_session(&self, session_id: SessionId) -> Vec<RuntimeWorkSnapshot> {
        self.active
            .lock()
            .await
            .iter()
            .filter(|((active_session_id, _), _)| *active_session_id == session_id)
            .map(|((_, work_id), work)| RuntimeWorkSnapshot {
                work_id: work_id.clone(),
                kind: work.spec.kind,
                label: work.spec.label.clone(),
                tool_call_id: work.spec.tool_call_id.clone(),
                status: if work.cancelled {
                    RuntimeWorkStatus::Cancelling
                } else {
                    RuntimeWorkStatus::Running
                },
                cancellable: work.spec.cancellation.is_cancellable(),
            })
            .collect()
    }
}
