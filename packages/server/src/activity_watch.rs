//! Shared non-owning activity projections and hosted observation admission.

use crate::ServerState;
use bcode_session_models::{
    ClientId, SessionActivityWatchDelivery as Delivery, SessionActivityWatchHandle as Handle,
    SessionActivityWatchScope as Scope, SessionActivityWatchSnapshot as Snapshot,
    SessionActivityWatchUpdate as Update, SessionId,
    application::SessionApplicationFailure as Failure,
};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex, Weak},
};
use tokio::sync::watch;

const MAX_HANDLES: usize = 256;
const MAX_GROUPS: usize = 128;
const MAX_BYTES: usize = 262_144;

type Projection = Option<Result<ProjectionVersion, Failure>>;
#[derive(Clone)]
struct ProjectionVersion {
    revision: u64,
    reset: u64,
    snapshot: Arc<Snapshot>,
}
struct ActivitySources {
    mutations: tokio::sync::broadcast::Receiver<bcode_session::SessionMutationCommitted>,
    changes: tokio::sync::broadcast::Receiver<Option<SessionId>>,
    resident_changes: tokio::sync::broadcast::Receiver<SessionId>,
    shutdown: tokio::sync::broadcast::Receiver<()>,
}

struct Group {
    scope: Scope,
    receiver: watch::Receiver<Projection>,
    worker: tokio::task::AbortHandle,
}
impl Drop for Group {
    fn drop(&mut self) {
        self.worker.abort();
    }
}
struct Entry {
    owner: ClientId,
    handle: Handle,
    group: Arc<Group>,
    last_revision: Option<u64>,
    last_reset: u64,
    sequence: u64,
    busy: bool,
    closed: watch::Sender<bool>,
}
#[derive(Default)]
struct Registry {
    entries: BTreeMap<SessionId, Entry>,
    groups: Vec<Weak<Group>>,
}
pub struct ActivityWatchRegistry {
    inner: Arc<Mutex<Registry>>,
    replies: Arc<tokio::sync::Semaphore>,
    #[cfg(test)]
    computations: Arc<std::sync::atomic::AtomicUsize>,
}

impl std::fmt::Debug for ActivityWatchRegistry {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ActivityWatchRegistry")
            .finish_non_exhaustive()
    }
}

impl Default for ActivityWatchRegistry {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Registry::default())),
            replies: Arc::new(tokio::sync::Semaphore::new(MAX_HANDLES)),
            #[cfg(test)]
            computations: Arc::default(),
        }
    }
}

#[must_use]
pub fn failure(code: &str, message: &str) -> Failure {
    Failure {
        code: code.into(),
        message: message.into(),
    }
}
fn closed() -> Failure {
    failure(
        "activity_watch_closed",
        "Activity watch is closed or belongs to another caller.",
    )
}

pub fn validate(scope: &Scope) -> Result<(), Failure> {
    let count = scope.activity.len() + scope.children.len();
    let mut budget = 0usize;
    let valid = count > 0
        && scope.activity.len() <= 16
        && scope.children.len() <= 8
        && scope.activity.iter().all(|target| {
            budget = budget.saturating_add(target.query.limit.saturating_add(1));
            (1..=128).contains(&target.query.limit)
        })
        && scope.children.iter().all(|query| {
            budget = budget.saturating_add(query.limit);
            (1..=128).contains(&query.limit)
        })
        && budget <= 512;
    if valid {
        Ok(())
    } else {
        Err(failure(
            "activity_watch_invalid_scope",
            "Watch requires 1..16 activity targets, at most 8 child windows, and at most 512 total candidates (1..128 per window).",
        ))
    }
}

impl ActivityWatchRegistry {
    // Retain the permit until transport serialization/send completes or is cancelled.
    pub fn reserve_reply(&self) -> Result<tokio::sync::OwnedSemaphorePermit, Failure> {
        self.replies.clone().try_acquire_owned().map_err(|_| {
            failure(
                "activity_watch_capacity",
                "Activity reply admission is full.",
            )
        })
    }

    pub fn open(
        &self,
        state: &Arc<ServerState>,
        owner: ClientId,
        scope: Scope,
    ) -> Result<Handle, Failure> {
        validate(&scope)?;
        let mut registry = self.inner.lock().map_err(|_| closed())?;
        if registry.entries.len() >= MAX_HANDLES {
            return Err(failure(
                "activity_watch_capacity",
                "Activity watch admission is full.",
            ));
        }
        registry.groups.retain(|group| group.strong_count() > 0);
        let existing = registry
            .groups
            .iter()
            .filter_map(Weak::upgrade)
            .find(|group| group.scope == scope);
        let group = if let Some(group) = existing {
            group
        } else {
            if registry.groups.len() >= MAX_GROUPS {
                return Err(failure(
                    "activity_watch_capacity",
                    "Activity projection admission is full.",
                ));
            }
            // Install both domain subscriptions before the first non-owning read.
            let mutations = state.sessions.subscribe_mutations();
            let changes = state.activity_changes.subscribe();
            let resident_changes = state.sessions.subscribe_activity_changes();
            let shutdown = state.subscribe_shutdown();
            let (sender, receiver) = watch::channel(None);
            let worker = tokio::spawn(project(
                Arc::downgrade(state),
                scope.clone(),
                sender,
                ActivitySources {
                    mutations,
                    changes,
                    resident_changes,
                    shutdown,
                },
                #[cfg(test)]
                Arc::clone(&self.computations),
            ));
            let group = Arc::new(Group {
                scope,
                receiver,
                worker: worker.abort_handle(),
            });
            registry.groups.push(Arc::downgrade(&group));
            group
        };
        let handle = Handle {
            watch_id: SessionId::new(),
            epoch: SessionId::new(),
        };
        registry.entries.insert(
            handle.watch_id,
            Entry {
                owner,
                handle,
                group,
                last_revision: None,
                last_reset: 0,
                sequence: 0,
                busy: false,
                closed: watch::channel(false).0,
            },
        );
        drop(registry);
        Ok(handle)
    }

    pub fn close(&self, owner: ClientId, handle: Handle) -> bool {
        let Ok(mut registry) = self.inner.lock() else {
            return false;
        };
        if !registry
            .entries
            .get(&handle.watch_id)
            .is_some_and(|entry| entry.owner == owner && entry.handle == handle)
        {
            return false;
        }
        if let Some(entry) = registry.entries.remove(&handle.watch_id) {
            entry.closed.send_replace(true);
        }
        true
    }
    pub fn remove_client(&self, owner: ClientId) {
        if let Ok(mut registry) = self.inner.lock() {
            registry.entries.retain(|_, entry| {
                if entry.owner == owner {
                    entry.closed.send_replace(true);
                    false
                } else {
                    true
                }
            });
        }
    }

    pub async fn pull(&self, owner: ClientId, handle: Handle) -> Result<Delivery, Failure> {
        let (mut receiver, mut cancelled, last) = {
            let mut registry = self.inner.lock().map_err(|_| closed())?;
            let entry = registry
                .entries
                .get_mut(&handle.watch_id)
                .filter(|entry| entry.owner == owner && entry.handle == handle)
                .ok_or_else(closed)?;
            if entry.busy {
                return Err(failure(
                    "activity_watch_busy",
                    "Only one activity watch pull may be outstanding.",
                ));
            }
            entry.busy = true;
            let state = (
                entry.group.receiver.clone(),
                entry.closed.subscribe(),
                entry.last_revision,
            );
            drop(registry);
            state
        };
        let mut lease = PullLease {
            registry: self.inner.clone(),
            handle,
            complete: false,
        };
        loop {
            let current = receiver.borrow_and_update().clone();
            if let Some(result) = current {
                let revision = result?;
                if last != Some(revision.revision) {
                    let mut registry = self.inner.lock().map_err(|_| closed())?;
                    let entry = registry
                        .entries
                        .get_mut(&handle.watch_id)
                        .ok_or_else(closed)?;
                    let update = match entry.last_revision {
                        None => Update::Initial,
                        Some(previous)
                            if revision.revision != previous + 1
                                || revision.reset != entry.last_reset =>
                        {
                            Update::Reset
                        }
                        Some(_) => Update::Changed,
                    };
                    let delivery = Delivery {
                        handle,
                        sequence: entry.sequence,
                        update,
                        snapshot: (*revision.snapshot).clone(),
                    };
                    entry.sequence = entry.sequence.checked_add(1).ok_or_else(closed)?;
                    entry.last_revision = Some(revision.revision);
                    entry.last_reset = revision.reset;
                    entry.busy = false;
                    drop(registry);
                    lease.complete = true;
                    return Ok(delivery);
                }
            }
            tokio::select! {
                result = receiver.changed() => { result.map_err(|_| closed())?; }
                _ = cancelled.changed() => return Err(closed()),
            }
        }
    }
}
struct PullLease {
    registry: Arc<Mutex<Registry>>,
    handle: Handle,
    complete: bool,
}
impl Drop for PullLease {
    fn drop(&mut self) {
        if !self.complete
            && let Ok(mut registry) = self.registry.lock()
            && let Some(entry) = registry.entries.remove(&self.handle.watch_id)
        {
            entry.closed.send_replace(true);
        }
    }
}

async fn snapshot(state: &ServerState, scope: &Scope) -> Snapshot {
    let mut activity = Vec::with_capacity(scope.activity.len());
    for target in &scope.activity {
        let result =
            crate::session_operations::activity(state, target.session_id, target.query.clone())
                .await
                .map_err(|_| {
                    failure(
                        "activity_unavailable",
                        "Canonical session activity is unavailable; no idle state is inferred.",
                    )
                });
        activity.push(result);
    }
    let mut children = Vec::with_capacity(scope.children.len());
    for query in &scope.children {
        children.push(
            state
                .sessions
                .query_child_sessions(query.clone())
                .await
                .map_err(|_| {
                    failure(
                        "children_unavailable",
                        "Canonical child discovery is unavailable; no empty tree is inferred.",
                    )
                }),
        );
    }
    Snapshot { activity, children }
}
fn semantic_equal(left: &Snapshot, right: &Snapshot) -> bool {
    // Checkpoints and wall-clock observation times describe evidence freshness, not
    // semantic transitions. New state transfers still carry their actual values.
    let mut left = left.clone();
    let mut right = right.clone();
    for snapshot in [&mut left, &mut right] {
        for page in snapshot
            .activity
            .iter_mut()
            .filter_map(|page| page.as_mut().ok())
        {
            page.observed_at_ms = 0;
            for activity in std::iter::once(&mut page.own).chain(&mut page.descendants) {
                if let Some(evidence) = &mut activity.evidence {
                    evidence.observed_event_sequence = None;
                }
            }
        }
        for page in snapshot
            .children
            .iter_mut()
            .filter_map(|page| page.as_mut().ok())
        {
            for summary in &mut page.children {
                summary.updated_at_ms = 0;
            }
        }
    }
    left == right
}

fn mutation_relevant(
    scope: &Scope,
    snapshot: Option<&Snapshot>,
    mutation: &bcode_session::SessionMutationCommitted,
) -> bool {
    // Streaming content cannot change activity or relationship windows. The
    // evidence reducer keeps its independent canonical feed for contiguous coverage.
    if matches!(
        mutation.event.kind,
        bcode_session_models::SessionEventKind::AssistantDelta { .. }
            | bcode_session_models::SessionEventKind::AssistantMessage { .. }
            | bcode_session_models::SessionEventKind::AssistantReasoningDelta { .. }
            | bcode_session_models::SessionEventKind::AssistantReasoningMessage { .. }
            | bcode_session_models::SessionEventKind::AssistantReasoningActivity { .. }
            | bcode_session_models::SessionEventKind::AssistantResponseSegment { .. }
            | bcode_session_models::SessionEventKind::PositionedAssistantResponseSegment { .. }
            | bcode_session_models::SessionEventKind::PositionedAssistantReasoningActivity { .. }
            | bcode_session_models::SessionEventKind::ModelUsage { .. }
    ) {
        return false;
    }
    relevant(scope, snapshot, mutation.session_id)
        || mutation
            .summary
            .parent_id
            .is_some_and(|id| relevant(scope, snapshot, id))
}

fn relevant(scope: &Scope, snapshot: Option<&Snapshot>, id: SessionId) -> bool {
    scope.activity.iter().any(|target| target.session_id == id)
        || scope.children.iter().any(|query| query.parent_id == id)
        || snapshot.is_some_and(|snapshot| {
            snapshot
                .activity
                .iter()
                .filter_map(|page| page.as_ref().ok())
                .any(|page| page.descendants.iter().any(|child| child.session_id == id))
                || snapshot
                    .children
                    .iter()
                    .filter_map(|page| page.as_ref().ok())
                    .any(|page| page.children.iter().any(|child| child.id == id))
        })
}

fn drain_changes<T: Clone>(
    receiver: &mut tokio::sync::broadcast::Receiver<T>,
    relevant: impl Fn(T) -> bool,
) -> (bool, bool) {
    use tokio::sync::broadcast::error::TryRecvError;
    let mut dirty = false;
    let mut invalidated = false;
    for _ in 0..1024 {
        match receiver.try_recv() {
            Ok(value) => dirty |= relevant(value),
            Err(TryRecvError::Lagged(_)) => {
                dirty = true;
                invalidated = true;
            }
            Err(TryRecvError::Empty | TryRecvError::Closed) => break,
        }
    }
    (dirty, invalidated)
}

async fn project(
    state: Weak<ServerState>,
    scope: Scope,
    sender: watch::Sender<Projection>,
    sources: ActivitySources,
    #[cfg(test)] computations: Arc<std::sync::atomic::AtomicUsize>,
) {
    use tokio::sync::broadcast::error::RecvError;
    let ActivitySources {
        mut mutations,
        mut changes,
        mut resident_changes,
        mut shutdown,
    } = sources;
    let mut previous: Option<Arc<Snapshot>> = None;
    let mut revision = 0u64;
    let mut reset = 0u64;
    let mut dirty = true;
    let mut invalidated = false;
    loop {
        // Drain only before reading. Mutations during the read stay queued for the
        // next pass, including creation that changes loaded-window membership.
        for (changed, lagged) in [
            drain_changes(&mut mutations, |mutation| {
                mutation_relevant(&scope, previous.as_deref(), &mutation)
            }),
            drain_changes(&mut changes, |id| {
                id.is_none_or(|id| relevant(&scope, previous.as_deref(), id))
            }),
            drain_changes(&mut resident_changes, |id| {
                relevant(&scope, previous.as_deref(), id)
            }),
        ] {
            dirty |= changed;
            invalidated |= lagged;
        }
        if dirty {
            let Some(state) = state.upgrade() else {
                return;
            };
            #[cfg(test)]
            computations.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            let next = snapshot(&state, &scope).await;
            drop(state);
            if crate::runtime_work::encoded_len(&next, MAX_BYTES).is_none() {
                sender.send_replace(Some(Err(failure("activity_watch_limit", "Activity snapshot exceeds the bounded delivery budget; narrow the watch scope."))));
                return;
            }
            if invalidated {
                reset = reset.wrapping_add(1);
            }
            if invalidated
                || previous
                    .as_ref()
                    .is_none_or(|previous| !semantic_equal(previous, &next))
            {
                revision += 1;
                let next = Arc::new(next);
                sender.send_replace(Some(Ok(ProjectionVersion {
                    revision,
                    reset,
                    snapshot: next.clone(),
                })));
                previous = Some(next);
            }
            dirty = false;
            invalidated = false;
        }
        // A bounded drain may leave backlog. Yield rather than strand it or scan
        // unbounded source history. No timer or idle recomputation is installed.
        if !mutations.is_empty() || !changes.is_empty() || !resident_changes.is_empty() {
            tokio::task::yield_now().await;
            continue;
        }
        tokio::select! {
            _ = shutdown.recv() => return,
            result = changes.recv() => match result {
                Ok(id) => dirty = id.is_none_or(|id| relevant(&scope, previous.as_deref(), id)),
                Err(RecvError::Lagged(_)) => { dirty = true; invalidated = true; },
                Err(RecvError::Closed) => return,
            },
            result = resident_changes.recv() => match result {
                Ok(id) => dirty = relevant(&scope, previous.as_deref(), id),
                Err(RecvError::Lagged(_)) => { dirty = true; invalidated = true; },
                Err(RecvError::Closed) => return,
            },
            result = mutations.recv() => match result {
                Ok(mutation) => dirty = mutation_relevant(&scope, previous.as_deref(), &mutation),
                Err(RecvError::Lagged(_)) => { dirty = true; invalidated = true; },
                Err(RecvError::Closed) => return,
            },
        }
    }
}

#[cfg(test)]
#[path = "activity_watch_tests.rs"]
mod tests;
