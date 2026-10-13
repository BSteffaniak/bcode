use bcode_metrics::{MetricLabels, MetricsRegistry};
use bcode_plugin::PluginInvocationCancelHandle;
use bcode_session::SessionOwnershipGuard;
pub use bcode_session_models::RuntimeWorkObservedStage as ObservedWorkStage;
use bcode_session_models::{RuntimeWorkKind, RuntimeWorkStatus, SessionId, WorkId};
use bcode_session_models::{RuntimeWorkObservation, RuntimeWorkSnapshot};
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
    /// Cancel a typed application service operation.
    ServiceCall(bcode_plugin_sdk::ServiceCancellation),
    /// Cancel a Ralph autonomous runner.
    RalphRun {
        /// Store containing the run record.
        store: bcode_ralph::RalphStateStore,
        /// Run ID to mark cancellation-requested.
        run_id: String,
    },
    /// Cancel a server-hosted durable workflow run.
    WorkflowRun(crate::WorkflowRunCancellationHandle),
    /// Server-hosted workflow node whose active model turn is routed by a child handle.
    WorkflowNode(Arc<TurnCancelState>),
    /// Test/no-op cancellation hook.
    #[cfg(test)]
    Test(Arc<std::sync::atomic::AtomicUsize>),
    /// Test cancellation hook that never returns.
    #[cfg(test)]
    TestBlocked(Arc<tokio::sync::Notify>),
    /// Test cancellation hook that reports cleanup failure.
    #[cfg(test)]
    TestFailed(Arc<std::sync::atomic::AtomicUsize>),
}

impl CancellationHandle {
    /// Request cancellation through the underlying handle.
    pub async fn cancel(&self) -> Result<(), String> {
        match self {
            Self::SessionTurn(cancel_state) | Self::WorkflowNode(cancel_state) => {
                cancel_state.cancel().await;
                Ok(())
            }
            Self::ServiceCall(cancellation) => {
                cancellation.cancel();
                Ok(())
            }
            Self::PluginInvocation(cancel) => {
                cancel.cancel();
                Ok(())
            }
            Self::RalphRun { store, run_id } => store
                .request_run_cancel(run_id)
                .map_err(|error| error.to_string()),
            Self::WorkflowRun(cancellation) => cancellation.cancel().await,
            #[cfg(test)]
            Self::Test(count) => {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            }
            #[cfg(test)]
            Self::TestBlocked(blocked) => {
                blocked.notified().await;
                Ok(())
            }
            #[cfg(test)]
            Self::TestFailed(count) => {
                count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("test cleanup failed".to_string())
            }
        }
    }

    /// Runtime work with a handle is cancellable.
    pub const fn is_cancellable(&self) -> bool {
        match self {
            Self::SessionTurn(_)
            | Self::ServiceCall(_)
            | Self::PluginInvocation(_)
            | Self::RalphRun { .. }
            | Self::WorkflowRun(_)
            | Self::WorkflowNode(_) => true,
            #[cfg(test)]
            Self::Test(_) | Self::TestBlocked(_) | Self::TestFailed(_) => true,
        }
    }
}

/// Specification for starting runtime work.
#[derive(Debug, Clone)]
pub struct RuntimeWorkSpec {
    pub work_id: WorkId,
    pub kind: RuntimeWorkKind,
    pub label: String,
    pub tool_call_id: Option<String>,
    pub plugin_id: Option<String>,
    pub service_interface: Option<String>,
    pub operation: Option<String>,
    pub parent_work_id: Option<WorkId>,
    pub cancellation: CancellationHandle,
    pub ownership: Option<SessionOwnershipGuard>,
}

impl RuntimeWorkSpec {
    #[allow(clippy::missing_const_for_fn)]
    pub fn new(
        work_id: WorkId,
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
            parent_work_id: None,
            cancellation,
            ownership: None,
        }
    }

    pub fn with_tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }

    pub fn with_parent_work_id(mut self, parent_work_id: Option<WorkId>) -> Self {
        self.parent_work_id = parent_work_id;
        self
    }
}

/// Capability bound to one registration and one observed stage revision.
#[derive(Debug, Clone)]
pub struct WorkObservationToken {
    session_id: SessionId,
    work_id: WorkId,
    owner: Arc<()>,
    revision: Arc<()>,
}

/// Bounded local observation, not execution authority or durable history.
#[derive(Debug)]
pub struct WorkObservation {
    pub work_id: WorkId,
    pub stage: Option<ObservedWorkStage>,
    pub elapsed: Option<std::time::Duration>,
    pub cancellation_requested: bool,
}

/// Live pagination is best effort; removed/replaced entries require a fresh scan.
#[derive(Debug)]
pub struct WorkObservationPage {
    pub entries: Vec<WorkObservation>,
    pub next_after: Option<WorkId>,
}

#[derive(Debug, Clone)]
struct ActiveRuntimeWork {
    observation_owner: Arc<()>,
    observation_revision: Arc<()>,
    stage: Option<(ObservedWorkStage, std::time::Instant)>,
    spec: RuntimeWorkSpec,
    cancelled: bool,
    shared_execution: Option<std::sync::Weak<bcode_session::SharedExecutionSessionObservation>>,
}

/// Active runtime-work identity returned by recursive cancellation routing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CancelledRuntimeWork {
    pub session_id: SessionId,
    pub work_id: WorkId,
}

/// Central server registry and cancellation router for active runtime work.
#[derive(Debug, Default)]
pub struct RuntimeWorkManager {
    active: Mutex<BTreeMap<(SessionId, WorkId), ActiveRuntimeWork>>,
    metrics: MetricsRegistry,
}

impl RuntimeWorkManager {
    /// Capture only an existing local registration; never admit missing work.
    pub(super) async fn observation_token(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
    ) -> Option<WorkObservationToken> {
        let active = self.active.lock().await;
        let work = active.get(&(session_id, work_id.clone()))?;
        let result = Some(WorkObservationToken {
            session_id,
            work_id: work_id.clone(),
            owner: Arc::clone(&work.observation_owner),
            revision: Arc::clone(&work.observation_revision),
        });
        drop(active);
        result
    }

    /// Advance the observation only while both owner and revision still match.
    /// Returned token supersedes the old token, fencing delayed stage updates.
    pub(super) async fn observe_stage(
        &self,
        token: &WorkObservationToken,
        stage: ObservedWorkStage,
    ) -> Option<WorkObservationToken> {
        let mut active = self.active.lock().await;
        let work = active.get_mut(&(token.session_id, token.work_id.clone()))?;
        if !Arc::ptr_eq(&token.owner, &work.observation_owner)
            || !Arc::ptr_eq(&token.revision, &work.observation_revision)
        {
            return None;
        }
        work.observation_revision = Arc::new(());
        work.stage = Some((stage, std::time::Instant::now()));
        let result = Some(WorkObservationToken {
            session_id: token.session_id,
            work_id: token.work_id.clone(),
            owner: Arc::clone(&work.observation_owner),
            revision: Arc::clone(&work.observation_revision),
        });
        drop(active);
        result
    }

    /// Observe at most 128 entries through an ordered per-session range.
    /// No labels, arguments, durable rows or foreign owners are inspected.
    pub(super) async fn observation_page(
        &self,
        session_id: SessionId,
        after: Option<&WorkId>,
        limit: usize,
    ) -> WorkObservationPage {
        use std::ops::Bound::{Excluded, Included, Unbounded};
        let limit = limit.clamp(1, 128);
        let active = self.active.lock().await;
        let lower = after.map_or_else(
            || Included((session_id, WorkId::new(""))),
            |id| Excluded((session_id, id.clone())),
        );
        let mut rows = active
            .range((lower, Unbounded))
            .take_while(|((session, _), _)| *session == session_id);
        let now = std::time::Instant::now();
        let entries: Vec<_> = rows
            .by_ref()
            .take(limit)
            .map(|((_, id), work)| WorkObservation {
                work_id: id.clone(),
                stage: work.stage.map(|(stage, _)| stage),
                elapsed: work
                    .stage
                    .map(|(_, started)| now.saturating_duration_since(started)),
                cancellation_requested: work.cancelled,
            })
            .collect();
        let next_after = rows
            .next()
            .and_then(|_| entries.last().map(|entry| entry.work_id.clone()));
        drop(active);
        WorkObservationPage {
            entries,
            next_after,
        }
    }

    /// Create a runtime work manager with aggregate cleanup diagnostics.
    #[must_use]
    pub fn with_metrics(metrics: MetricsRegistry) -> Self {
        Self {
            active: Mutex::default(),
            metrics,
        }
    }

    /// Register active work and return whether it should be advertised as cancellable.
    ///
    /// The boolean tuple is `(cancellable, newly_registered)`. Re-registering an existing exact
    /// identity replaces its cancellation handle without creating another durable start event.
    pub async fn start(&self, session_id: SessionId, spec: RuntimeWorkSpec) -> (bool, bool) {
        let cancellable = spec.cancellation.is_cancellable();
        let mut active = self.active.lock().await;
        let inherited_cancellation = spec.parent_work_id.as_ref().is_some_and(|parent| {
            active
                .get(&(session_id, parent.clone()))
                .is_some_and(|work| work.cancelled)
        });
        let prior_cancellation = active
            .get(&(session_id, spec.work_id.clone()))
            .is_some_and(|work| work.cancelled);
        let cancelled = inherited_cancellation || prior_cancellation;
        let cancellation = spec.cancellation.clone();
        let replaced = active.insert(
            (session_id, spec.work_id.clone()),
            ActiveRuntimeWork {
                observation_owner: Arc::new(()),
                observation_revision: Arc::new(()),
                stage: None,
                spec,
                cancelled,
                shared_execution: None,
            },
        );
        drop(active);
        if cancelled {
            if let CancellationHandle::ServiceCall(token) = &cancellation {
                token.cancel();
            } else {
                tokio::spawn(async move {
                    let _ = cancellation.cancel().await;
                });
            }
        }
        (cancellable, replaced.is_none())
    }

    /// Bind a held sequential permit to an exact active workflow dispatch.
    pub async fn bind_shared_execution(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
        permit: &bcode_session::SharedExecutionSessionPermit,
    ) -> bool {
        if permit.session_id() != session_id {
            return false;
        }
        let mut active = self.active.lock().await;
        let Some(work) = active.get_mut(&(session_id, work_id.clone())) else {
            return false;
        };
        if work.spec.kind != RuntimeWorkKind::WorkflowNode {
            return false;
        }
        work.shared_execution = Some(permit.observation());
        drop(active);
        true
    }

    /// Verify exact shared-parent provenance while both dispatch and permit remain active.
    pub async fn observes_shared_execution(
        &self,
        session_id: SessionId,
        link: &bcode_workflow_store::WorkflowExecutionSessionLink,
    ) -> bool {
        if link.session_id != session_id.to_string() {
            return false;
        }
        let identity = bcode_workflow_store::dispatch_identity(
            &link.run_id,
            &link.node_id,
            &link.activation_id,
            link.attempt,
        );
        let active = self.active.lock().await;
        let Some(work) = active.get(&(session_id, WorkId::new(identity))) else {
            return false;
        };
        let verified = work.spec.kind == RuntimeWorkKind::WorkflowNode
            && work.spec.parent_work_id == Some(WorkId::new(format!("workflow:{}", link.run_id)))
            && work
                .shared_execution
                .as_ref()
                .and_then(std::sync::Weak::upgrade)
                .is_some_and(|observation| {
                    let Some(proof) = observation.provenance() else {
                        return false;
                    };
                    proof.context_mode
                        == bcode_session_models::ExecutionSessionContextMode::SharedSequential
                        && proof.parent_session_id == session_id
                        && proof.run_id == link.run_id
                        && proof.node_id == link.node_id
                        && proof.activation_id.as_deref() == Some(link.activation_id.as_str())
                        && proof.attempt == link.attempt
                        && proof.workspace_snapshot.as_deref()
                            == Some(link.workspace_snapshot.as_str())
                });
        drop(active);
        verified
    }

    /// Replace the cancellation handle for existing active work.
    pub async fn replace_cancellation(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
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

    /// Cancel active work by exact ID across sessions and return every work item newly signalled.
    pub async fn cancel_global_with_children(&self, work_id: &WorkId) -> Vec<CancelledRuntimeWork> {
        let session_ids = self
            .active
            .lock()
            .await
            .keys()
            .filter(|(_, active_work_id)| active_work_id == work_id)
            .map(|(session_id, _)| *session_id)
            .collect::<Vec<_>>();
        let mut cancelled = Vec::new();
        for session_id in session_ids {
            cancelled.extend(
                self.cancel_with_children(session_id, work_id)
                    .await
                    .into_iter()
                    .map(|work_id| CancelledRuntimeWork {
                        session_id,
                        work_id,
                    }),
            );
        }
        cancelled
    }

    /// Cancel active work by exact ID and return every work item newly signalled.
    pub async fn cancel_with_children(
        &self,
        session_id: SessionId,
        work_id: &WorkId,
    ) -> Vec<WorkId> {
        let mut active = self.active.lock().await;
        if !active.contains_key(&(session_id, work_id.clone())) {
            return Vec::new();
        }
        let mut pending = vec![work_id.clone()];
        let mut cancellations = Vec::new();
        let mut cancelled_work_ids = Vec::new();
        while let Some(next_work_id) = pending.pop() {
            let key = (session_id, next_work_id.clone());
            let Some(work) = active.get_mut(&key) else {
                continue;
            };
            if work.cancelled {
                continue;
            }
            work.cancelled = true;
            cancelled_work_ids.push(next_work_id.clone());
            cancellations.push((
                next_work_id.clone(),
                work.spec.kind,
                work.spec.cancellation.clone(),
            ));
            let children = active
                .iter()
                .filter(|((child_session_id, _), child)| {
                    *child_session_id == session_id
                        && child.spec.parent_work_id.as_ref() == Some(&next_work_id)
                })
                .map(|((_, child_work_id), _)| child_work_id.clone())
                .collect::<Vec<_>>();
            pending.extend(children);
        }
        drop(active);
        for (cleanup_work_id, kind, cancellation) in cancellations {
            let metrics = self.metrics.clone();
            tokio::spawn(async move {
                let started_at = std::time::Instant::now();
                let result = cancellation.cancel().await;
                let outcome = if result.is_ok() {
                    "completed"
                } else {
                    "failed"
                };
                let mut labels = MetricLabels::new();
                labels.insert(
                    "work_kind".to_owned(),
                    runtime_work_kind_label(kind).to_owned(),
                );
                labels.insert("outcome".to_owned(), outcome.to_owned());
                metrics.add_counter_with_labels("runtime_work.cleanup_total", 1, labels.clone());
                metrics.record_histogram_with_labels(
                    "runtime_work.cleanup_duration_ms",
                    u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
                    labels,
                );
                match result {
                    Ok(()) => tracing::debug!(
                        target: "bcode_server::cleanup",
                        %session_id,
                        work_id = %cleanup_work_id,
                        "detached runtime cleanup completed"
                    ),
                    Err(error) => tracing::warn!(
                        target: "bcode_server::cleanup",
                        %session_id,
                        work_id = %cleanup_work_id,
                        %error,
                        "detached runtime cleanup failed"
                    ),
                }
            });
        }
        cancelled_work_ids
    }

    /// Return active work identifiers for a session.
    pub async fn active_ids_for_session(
        &self,
        session_id: SessionId,
    ) -> std::collections::BTreeSet<WorkId> {
        self.active
            .lock()
            .await
            .keys()
            .filter_map(|(active_session_id, work_id)| {
                (*active_session_id == session_id).then_some(work_id.clone())
            })
            .collect()
    }

    /// Retire only the registration generation captured by this token.
    ///
    /// Stage revisions do not affect retirement authority: an older-stage token
    /// may retire its same owner, but can never remove a replacement owner.
    /// Missing or stale generations return `None` without changing live work.
    pub(super) async fn take_observed(
        &self,
        token: &WorkObservationToken,
    ) -> Option<RuntimeWorkSpec> {
        let mut active = self.active.lock().await;
        let key = (token.session_id, token.work_id.clone());
        let matches_owner = active
            .get(&key)
            .is_some_and(|work| Arc::ptr_eq(&token.owner, &work.observation_owner));
        let owned = if matches_owner {
            active.remove(&key).map(|work| work.spec)
        } else {
            None
        };
        drop(active);
        owned
    }

    /// Remove active work and return the owned registration, if present.
    pub async fn take(&self, session_id: SessionId, work_id: &WorkId) -> Option<RuntimeWorkSpec> {
        self.active
            .lock()
            .await
            .remove(&(session_id, work_id.clone()))
            .map(|work| work.spec)
    }

    /// Remove active work and return whether this manager owned it.
    pub async fn finish(&self, session_id: SessionId, work_id: &WorkId) -> bool {
        self.take(session_id, work_id).await.is_some()
    }

    /// Return the number of active work registrations across all sessions.
    pub async fn active_count(&self) -> usize {
        self.active.lock().await.len()
    }

    /// Return active work snapshots across every session, tagged with their owning session.
    pub async fn active_all(&self) -> Vec<(SessionId, RuntimeWorkSnapshot)> {
        let active = self.active.lock().await;
        let now = std::time::Instant::now();
        let snapshots = active
            .iter()
            .map(|((session_id, work_id), work)| {
                (
                    *session_id,
                    RuntimeWorkSnapshot {
                        work_id: work_id.clone(),
                        kind: work.spec.kind,
                        label: work.spec.label.clone(),
                        tool_call_id: work.spec.tool_call_id.clone(),
                        status: if work.cancelled {
                            RuntimeWorkStatus::Cancelling
                        } else {
                            RuntimeWorkStatus::Running
                        },
                        observation: observed_work_at(work, now),
                        cancellable: work.spec.cancellation.is_cancellable(),
                    },
                )
            })
            .collect();
        drop(active);
        snapshots
    }

    /// Return active work snapshots for a session.
    /// Count observed local work within one session key range, bounded by `budget`.
    /// The completeness flag is false when more work exists than the observation budget.
    pub async fn activity_count_for_session(
        &self,
        session_id: SessionId,
        budget: u32,
    ) -> (u32, bool) {
        let active = self.active.lock().await;
        let result = {
            let mut rows = active
                .range((session_id, WorkId::new(""))..)
                .take_while(|((id, _), _)| *id == session_id);
            let mut count = 0;
            while count < budget && rows.next().is_some() {
                count += 1;
            }
            (count, rows.next().is_none())
        };
        drop(active);
        result
    }

    pub async fn active_for_session(&self, session_id: SessionId) -> Vec<RuntimeWorkSnapshot> {
        let active = self.active.lock().await;
        let now = std::time::Instant::now();
        let snapshots = active
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
                observation: observed_work_at(work, now),
                cancellable: work.spec.cancellation.is_cancellable(),
            })
            .collect();
        drop(active);
        snapshots
    }
}

fn observed_work_at(
    work: &ActiveRuntimeWork,
    now: std::time::Instant,
) -> Option<RuntimeWorkObservation> {
    work.stage.map(|(stage, started)| RuntimeWorkObservation {
        stage,
        elapsed_ms: u64::try_from(now.saturating_duration_since(started).as_millis())
            .unwrap_or(u64::MAX),
    })
}

const fn runtime_work_kind_label(kind: RuntimeWorkKind) -> &'static str {
    match kind {
        RuntimeWorkKind::ModelTurn => "model_turn",
        RuntimeWorkKind::Tool => "tool",
        RuntimeWorkKind::PluginInvocation => "plugin_invocation",
        RuntimeWorkKind::EventDelivery => "event_delivery",
        RuntimeWorkKind::Workflow => "workflow",
        RuntimeWorkKind::WorkflowNode => "workflow_node",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn start_observation_fixture(
        manager: &RuntimeWorkManager,
        session: SessionId,
        id: &WorkId,
    ) {
        manager
            .start(
                session,
                RuntimeWorkSpec::new(
                    id.clone(),
                    RuntimeWorkKind::PluginInvocation,
                    "not diagnostic content".into(),
                    CancellationHandle::Test(Arc::new(AtomicUsize::new(0))),
                ),
            )
            .await;
    }

    #[tokio::test]
    async fn observation_stages_survive_cancellation_until_retirement() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        let id = WorkId::new("observed");
        assert!(manager.observation_token(session, &id).await.is_none());
        start_observation_fixture(&manager, session, &id).await;
        let initial = manager.observation_page(session, None, 1).await;
        assert_eq!(initial.entries[0].stage, None);
        assert_eq!(initial.entries[0].elapsed, None);
        let mut token = manager.observation_token(session, &id).await.unwrap();
        for stage in [
            ObservedWorkStage::Dispatch,
            ObservedWorkStage::ReceiptWait,
            ObservedWorkStage::Cleanup,
        ] {
            token = manager.observe_stage(&token, stage).await.unwrap();
        }
        manager.cancel_with_children(session, &id).await;
        let cancelled = manager.observation_page(session, None, 1).await;
        assert_eq!(cancelled.entries[0].stage, Some(ObservedWorkStage::Cleanup));
        assert!(cancelled.entries[0].elapsed.is_some());
        assert!(cancelled.entries[0].cancellation_requested);
        let session_snapshot = manager.active_for_session(session).await;
        let all_snapshot = manager.active_all().await;
        assert_eq!(
            session_snapshot[0].observation.as_ref().unwrap().stage,
            ObservedWorkStage::Cleanup
        );
        assert_eq!(
            all_snapshot[0].1.observation.as_ref().unwrap().stage,
            ObservedWorkStage::Cleanup
        );
        assert_eq!(session_snapshot[0].status, RuntimeWorkStatus::Cancelling);
        token = manager
            .observe_stage(&token, ObservedWorkStage::Retirement)
            .await
            .unwrap();
        assert!(manager.finish(session, &id).await);
        assert!(
            manager
                .observe_stage(&token, ObservedWorkStage::Dispatch)
                .await
                .is_none()
        );
        assert!(
            manager
                .observation_page(session, None, 1)
                .await
                .entries
                .is_empty()
        );
    }

    #[tokio::test]
    async fn observation_tokens_fence_stale_revisions_and_replaced_owners() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        let id = WorkId::new("replaced");
        start_observation_fixture(&manager, session, &id).await;
        let old = manager.observation_token(session, &id).await.unwrap();
        let current = manager
            .observe_stage(&old, ObservedWorkStage::Cleanup)
            .await
            .unwrap();
        assert!(
            manager
                .observe_stage(&old, ObservedWorkStage::Dispatch)
                .await
                .is_none()
        );
        start_observation_fixture(&manager, session, &id).await;
        assert!(
            manager
                .observe_stage(&current, ObservedWorkStage::Retirement)
                .await
                .is_none()
        );
        assert_eq!(
            manager.observation_page(session, None, 1).await.entries[0].stage,
            None
        );
    }

    #[tokio::test]
    async fn observed_retirement_rejects_replaced_owner_without_removing_replacement() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        let id = WorkId::new("retirement-generation");
        start_observation_fixture(&manager, session, &id).await;
        let original = manager.observation_token(session, &id).await.unwrap();
        start_observation_fixture(&manager, session, &id).await;
        let replacement = manager.observation_token(session, &id).await.unwrap();
        assert!(manager.take_observed(&original).await.is_none());
        assert_eq!(manager.active_for_session(session).await.len(), 1);
        assert!(manager.take_observed(&replacement).await.is_some());
        assert!(manager.take_observed(&original).await.is_none());
        assert!(manager.take_observed(&replacement).await.is_none());
    }

    #[tokio::test]
    async fn observed_retirement_accepts_same_owner_with_expired_stage_revision() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        let id = WorkId::new("retirement-revision");
        start_observation_fixture(&manager, session, &id).await;
        let original = manager.observation_token(session, &id).await.unwrap();
        let current = manager
            .observe_stage(&original, ObservedWorkStage::Retirement)
            .await
            .unwrap();
        assert!(
            manager
                .observe_stage(&original, ObservedWorkStage::Dispatch)
                .await
                .is_none()
        );
        let retired = manager.take_observed(&original).await.unwrap();
        assert_eq!(retired.work_id, id);
        assert!(manager.take_observed(&current).await.is_none());
        assert!(manager.active_for_session(session).await.is_empty());
    }

    #[tokio::test]
    async fn observation_pages_are_bounded_session_scoped_and_exclusive() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        for index in 0..130 {
            start_observation_fixture(&manager, session, &WorkId::new(format!("work-{index:03}")))
                .await;
        }
        start_observation_fixture(&manager, SessionId::new(), &WorkId::new("foreign")).await;
        let page = manager.observation_page(session, None, usize::MAX).await;
        assert_eq!(page.entries.len(), 128);
        let cursor = page.next_after.unwrap();
        let tail = manager.observation_page(session, Some(&cursor), 128).await;
        assert_eq!(tail.entries.len(), 2);
        assert!(tail.entries.iter().all(|entry| entry.work_id > cursor));
        assert!(tail.next_after.is_none());
        assert_eq!(
            manager
                .observation_page(session, None, 0)
                .await
                .entries
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn typed_service_cancellation_remains_routable_until_settlement() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let parent = WorkId::new("model-parent");
        let child = WorkId::new("auxiliary-child");
        let cancellation = bcode_plugin_sdk::ServiceCancellation::default();
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    parent.clone(),
                    RuntimeWorkKind::ModelTurn,
                    "parent".into(),
                    CancellationHandle::Test(Arc::new(AtomicUsize::new(0))),
                ),
            )
            .await;
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    child.clone(),
                    RuntimeWorkKind::PluginInvocation,
                    "auxiliary".into(),
                    CancellationHandle::ServiceCall(cancellation.clone()),
                )
                .with_parent_work_id(Some(parent.clone())),
            )
            .await;
        assert!(!cancellation.is_cancelled());
        let cancelled = manager.cancel_with_children(session_id, &parent).await;
        assert!(cancelled.contains(&child));
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while !cancellation.is_cancelled() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached cancellation delivery");
        assert!(cancellation.is_cancelled());
        assert_eq!(manager.active_count().await, 2);
        assert!(manager.finish(session_id, &child).await);
        assert!(manager.finish(session_id, &parent).await);
        assert_eq!(manager.active_count().await, 0);
    }

    #[tokio::test]
    async fn active_ids_are_removed_exactly_once_on_finish() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let work_id = WorkId::new("model-turn");
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    work_id.clone(),
                    RuntimeWorkKind::ModelTurn,
                    "model turn".to_owned(),
                    CancellationHandle::Test(Arc::new(AtomicUsize::new(0))),
                ),
            )
            .await;

        assert!(
            manager
                .active_ids_for_session(session_id)
                .await
                .contains(&work_id)
        );
        assert!(manager.finish(session_id, &work_id).await);
        assert!(!manager.finish(session_id, &work_id).await);
        assert!(manager.active_ids_for_session(session_id).await.is_empty());
    }

    async fn wait_for_count(count: &AtomicUsize, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while count.load(Ordering::SeqCst) != expected {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("detached cancellation should be dispatched");
    }

    fn test_spec(work_id: &str, count: Arc<AtomicUsize>) -> RuntimeWorkSpec {
        RuntimeWorkSpec::new(
            WorkId::new(work_id),
            RuntimeWorkKind::Tool,
            work_id.to_string(),
            CancellationHandle::Test(count),
        )
    }

    #[tokio::test]
    async fn child_registration_inherits_parent_cancellation() {
        let manager = RuntimeWorkManager::default();
        let session = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        manager
            .start(session, test_spec("parent", Arc::clone(&count)))
            .await;
        manager
            .cancel_with_children(session, &WorkId::new("parent"))
            .await;
        wait_for_count(&count, 1).await;
        manager
            .start(
                session,
                test_spec("child", Arc::clone(&count))
                    .with_parent_work_id(Some(WorkId::new("parent"))),
            )
            .await;
        wait_for_count(&count, 2).await;
        manager
            .start(session, test_spec("child", Arc::clone(&count)))
            .await;
        wait_for_count(&count, 3).await;
    }

    #[tokio::test]
    async fn global_cancellation_finds_exact_work_across_sessions() {
        let manager = RuntimeWorkManager::default();
        let first_session = SessionId::new();
        let second_session = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("shared-work");
        manager
            .start(first_session, test_spec("shared-work", Arc::clone(&count)))
            .await;
        manager
            .start(second_session, test_spec("other-work", Arc::clone(&count)))
            .await;
        let cancelled = manager.cancel_global_with_children(&work_id).await;
        assert_eq!(cancelled.len(), 1);
        assert_eq!(cancelled[0].session_id, first_session);
        assert_eq!(cancelled[0].work_id, work_id);
        wait_for_count(&count, 1).await;
    }

    #[tokio::test]
    async fn shared_execution_proof_is_exact_and_release_bound() {
        let sessions = bcode_session::SessionManager::default();
        let parent = sessions
            .create_session(None, std::env::temp_dir())
            .await
            .unwrap();
        let provenance = bcode_session_models::ExecutionSessionProvenance {
            version: bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION,
            owner: "bcode.workflow".into(),
            run_id: "run".into(),
            node_id: "node".into(),
            activation_id: Some("activation".into()),
            attempt: 1,
            parent_session_id: parent.id,
            context_mode: bcode_session_models::ExecutionSessionContextMode::SharedSequential,
            workspace_snapshot: Some("workspace".into()),
            parent_generation: None,
        };
        let permit = sessions
            .admit_shared_execution_session(parent.id, &provenance)
            .await
            .unwrap();
        let link = bcode_workflow_store::WorkflowExecutionSessionLink::new(
            "run".into(),
            "node".into(),
            "activation".into(),
            1,
            parent.id.to_string(),
            "workspace".into(),
            1,
        );
        let manager = RuntimeWorkManager::default();
        let work_id = WorkId::new(bcode_workflow_store::dispatch_identity(
            "run",
            "node",
            "activation",
            1,
        ));
        manager
            .start(
                parent.id,
                RuntimeWorkSpec::new(
                    work_id.clone(),
                    RuntimeWorkKind::WorkflowNode,
                    "node".into(),
                    CancellationHandle::Test(Arc::new(AtomicUsize::new(0))),
                )
                .with_parent_work_id(Some(WorkId::new("workflow:run"))),
            )
            .await;
        assert!(!manager.observes_shared_execution(parent.id, &link).await);
        assert!(
            manager
                .bind_shared_execution(parent.id, &work_id, &permit)
                .await
        );
        assert!(manager.observes_shared_execution(parent.id, &link).await);
        let mut spoofed = link.clone();
        spoofed.attempt = 2;
        assert!(!manager.observes_shared_execution(parent.id, &spoofed).await);
        spoofed = link.clone();
        spoofed.workspace_snapshot = "other".into();
        assert!(!manager.observes_shared_execution(parent.id, &spoofed).await);
        assert!(
            !manager
                .observes_shared_execution(SessionId::new(), &link)
                .await
        );
        let retained_observation = permit.observation().upgrade().unwrap();
        drop(permit);
        assert!(retained_observation.provenance().is_none());
        assert!(!manager.observes_shared_execution(parent.id, &link).await);
        assert!(
            sessions
                .session_summary_non_owning(parent.id)
                .await
                .unwrap()
                .execution
                .is_none()
        );
    }

    #[tokio::test]
    async fn start_cancel_finish_tracks_active_work() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        assert_eq!(
            manager
                .start(session_id, test_spec("work", Arc::clone(&count)))
                .await,
            (true, true)
        );
        assert_eq!(manager.active_for_session(session_id).await.len(), 1);
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        wait_for_count(&count, 1).await;
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling
        );
        manager.finish(session_id, &work_id).await;
        assert!(manager.active_for_session(session_id).await.is_empty());
    }

    #[tokio::test]
    async fn duplicate_cancel_only_signals_once() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let count = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        manager
            .start(session_id, test_spec("work", Arc::clone(&count)))
            .await;
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        assert!(
            manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        wait_for_count(&count, 1).await;
    }

    #[tokio::test]
    async fn replace_cancellation_changes_signal_target() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let work_id = WorkId::new("work");

        manager
            .start(session_id, test_spec("work", Arc::clone(&first)))
            .await;
        assert!(
            manager
                .replace_cancellation(
                    session_id,
                    &work_id,
                    CancellationHandle::Test(Arc::clone(&second))
                )
                .await
        );
        assert!(
            !manager
                .cancel_with_children(session_id, &work_id)
                .await
                .is_empty()
        );
        assert_eq!(first.load(Ordering::SeqCst), 0);
        wait_for_count(&second, 1).await;
    }
    #[tokio::test]
    async fn cancelling_parent_cancels_child() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let parent_count = Arc::new(AtomicUsize::new(0));
        let child_count = Arc::new(AtomicUsize::new(0));
        let parent_id = WorkId::new("parent");
        let child_id = WorkId::new("child");

        manager
            .start(session_id, test_spec("parent", Arc::clone(&parent_count)))
            .await;
        manager
            .start(
                session_id,
                test_spec("child", Arc::clone(&child_count))
                    .with_parent_work_id(Some(parent_id.clone())),
            )
            .await;

        assert!(
            !manager
                .cancel_with_children(session_id, &parent_id)
                .await
                .is_empty()
        );
        wait_for_count(&parent_count, 1).await;
        wait_for_count(&child_count, 1).await;
        assert_eq!(
            manager
                .active_for_session(session_id)
                .await
                .into_iter()
                .find(|work| work.work_id == child_id)
                .expect("child remains active until finished")
                .status,
            RuntimeWorkStatus::Cancelling
        );
    }
    #[tokio::test]
    async fn non_returning_cleanup_cannot_delay_local_cancellation() {
        let manager = RuntimeWorkManager::default();
        let session_id = SessionId::new();
        let work_id = WorkId::new("blocked");
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    work_id.clone(),
                    RuntimeWorkKind::Tool,
                    "blocked cleanup".to_string(),
                    CancellationHandle::TestBlocked(Arc::new(tokio::sync::Notify::new())),
                ),
            )
            .await;

        let cancelled = tokio::time::timeout(
            std::time::Duration::from_millis(100),
            manager.cancel_with_children(session_id, &work_id),
        )
        .await
        .expect("local cancellation must not await cleanup");

        assert_eq!(cancelled, vec![work_id]);
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling
        );
    }
    #[tokio::test]
    async fn cleanup_failure_is_diagnostic_only_after_local_cancellation() {
        let metrics = MetricsRegistry::default();
        let manager = RuntimeWorkManager::with_metrics(metrics.clone());
        let session_id = SessionId::new();
        let work_id = WorkId::new("failed-cleanup");
        let attempts = Arc::new(AtomicUsize::new(0));
        manager
            .start(
                session_id,
                RuntimeWorkSpec::new(
                    work_id.clone(),
                    RuntimeWorkKind::Tool,
                    "failed cleanup".to_string(),
                    CancellationHandle::TestFailed(Arc::clone(&attempts)),
                ),
            )
            .await;

        let cancelled = manager.cancel_with_children(session_id, &work_id).await;
        assert_eq!(cancelled, vec![work_id]);
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling
        );
        wait_for_count(&attempts, 1).await;
        tokio::time::timeout(std::time::Duration::from_secs(1), async {
            while metrics
                .snapshot()
                .counters
                .get("runtime_work.cleanup_total")
                .copied()
                != Some(1)
            {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup diagnostic should be recorded");
        let report = metrics.report();
        assert!(report.events.iter().any(|event| {
            event.name == "runtime_work.cleanup_total"
                && event.labels.get("work_kind").map(String::as_str) == Some("tool")
                && event.labels.get("outcome").map(String::as_str) == Some("failed")
                && !event.labels.contains_key("work_id")
        }));
        assert!(
            metrics
                .snapshot()
                .histograms
                .contains_key("runtime_work.cleanup_duration_ms")
        );
        assert_eq!(
            manager.active_for_session(session_id).await[0].status,
            RuntimeWorkStatus::Cancelling,
            "cleanup failure must not reverse local cancellation"
        );
    }
}
