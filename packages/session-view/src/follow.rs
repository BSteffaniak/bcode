//! Following one session's semantic view through the daemon.
//!
//! [`follow_session_view`] attaches to a session's bounded projection window, applies session,
//! live, and runtime-work events to a [`SessionView`], and delivers each changed snapshot to a
//! [`SessionViewSink`]. When the daemon asks for a resynchronization or the event stream fails, it
//! reports `Reconnecting`, reattaches with backoff, and continues from the fresh attachment. It
//! returns when the sink closes.

use std::sync::Arc;
use std::time::Duration;

use bcode_client::{BcodeClient, ClientConnection, ClientError};
use bcode_ipc::{Event as BcodeEvent, PermissionSummary};
use bcode_session_models::{ProjectionWindowRequest, SessionId, ToolExchangeResponsePolicy};
use bcode_session_view_models::{
    InteractionViewState, InteractionViewSummary, PermissionBatchView, PermissionView,
    ReasoningPresentationPolicy, SessionConnectionViewStatus, SessionViewSnapshot,
};

use crate::SessionView;

/// How long one event wait lasts before the sink is checked for closure.
const EVENT_POLL: Duration = Duration::from_millis(250);
/// First delay between reattachment attempts; it doubles up to [`MAX_RECONNECT_DELAY`].
const INITIAL_RECONNECT_DELAY: Duration = Duration::from_millis(100);
/// Longest delay between reattachment attempts.
const MAX_RECONNECT_DELAY: Duration = Duration::from_secs(2);

/// Names the interaction kind of a pending tool exchange from its producer id, schema, and
/// schema version. `None` keeps the schema as the kind.
pub type InteractionKind = dyn Fn(&str, &str, u32) -> Option<String> + Send + Sync;

/// Shared [`InteractionKind`].
pub type InteractionKindResolver = Arc<InteractionKind>;

/// What to follow and how to present it.
#[derive(Clone)]
pub struct SessionViewFollow {
    /// Session to observe.
    pub session_id: SessionId,
    /// Bounded projection window for the initial attachment and every resynchronization.
    pub projection: ProjectionWindowRequest,
    /// Renderer-local reasoning presentation policy.
    pub reasoning_policy: ReasoningPresentationPolicy,
    /// Name of the daemon connection, shown in daemon diagnostics.
    pub client_name: String,
    /// Interaction kinds of pending tool exchanges; `None` keeps each exchange's schema.
    pub interaction_kind: Option<InteractionKindResolver>,
}

impl std::fmt::Debug for SessionViewFollow {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionViewFollow")
            .field("session_id", &self.session_id)
            .field("projection", &self.projection)
            .field("reasoning_policy", &self.reasoning_policy)
            .field("client_name", &self.client_name)
            .field("interaction_kind", &self.interaction_kind.is_some())
            .finish()
    }
}

/// One delivery to a [`SessionViewSink`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionViewFollowUpdate {
    /// Complete authoritative state; it replaces any earlier snapshot of the session.
    Snapshot(Box<SessionViewSnapshot>),
    /// Following stopped because the first attachment failed.
    Disconnected {
        /// Human-readable failure message.
        message: String,
    },
}

/// Receiver of a followed session's updates.
pub trait SessionViewSink: Send {
    /// Deliver `update`. Returns `false` when the receiver is gone, which stops following.
    fn deliver(
        &mut self,
        update: SessionViewFollowUpdate,
    ) -> impl std::future::Future<Output = bool> + Send;

    /// Whether the receiver is gone.
    fn is_closed(&self) -> bool;
}

impl SessionViewSink for tokio::sync::mpsc::Sender<SessionViewFollowUpdate> {
    async fn deliver(&mut self, update: SessionViewFollowUpdate) -> bool {
        self.send(update).await.is_ok()
    }

    fn is_closed(&self) -> bool {
        Self::is_closed(self)
    }
}

/// Follow a session until `sink` closes.
///
/// When the first attachment fails, `sink` receives [`SessionViewFollowUpdate::Disconnected`]
/// and following stops. Later failures reattach with backoff instead.
pub async fn follow_session_view(
    client: BcodeClient,
    follow: SessionViewFollow,
    sink: &mut impl SessionViewSink,
) {
    if let Err(error) = Box::pin(follow_attached(&client, &follow, sink)).await {
        let _ = sink
            .deliver(SessionViewFollowUpdate::Disconnected {
                message: error.to_string(),
            })
            .await;
    }
}

async fn deliver_snapshot(sink: &mut impl SessionViewSink, view: &SessionView) -> bool {
    sink.deliver(SessionViewFollowUpdate::Snapshot(Box::new(
        view.snapshot().clone(),
    )))
    .await
}

async fn follow_attached(
    client: &BcodeClient,
    follow: &SessionViewFollow,
    sink: &mut impl SessionViewSink,
) -> Result<(), ClientError> {
    let session_id = follow.session_id;
    let mut connection = client.connect(&follow.client_name).await?;
    let mut view = attach(client, &mut connection, follow).await?;
    if !deliver_snapshot(sink, &view).await {
        return Ok(());
    }
    loop {
        let event = match tokio::time::timeout(EVENT_POLL, connection.recv_event()).await {
            Ok(event) => event,
            Err(_) if sink.is_closed() => return Ok(()),
            Err(_) => continue,
        };
        let needs_resync = match event {
            Ok(BcodeEvent::SessionViewResyncRequired {
                session_id: required,
            }) if required == session_id => true,
            Ok(event) => {
                let changed = match event {
                    BcodeEvent::Session(event) | BcodeEvent::RuntimeWork(event)
                        if event.session_id == session_id =>
                    {
                        view.apply_event(&event);
                        true
                    }
                    BcodeEvent::SessionLive(event) if event.session_id == session_id => {
                        view.apply_live_event(&event);
                        true
                    }
                    BcodeEvent::Session(_)
                    | BcodeEvent::SessionLive(_)
                    | BcodeEvent::RuntimeWork(_)
                    | BcodeEvent::Workflow(_)
                    | BcodeEvent::SessionViewResyncRequired { .. }
                    | BcodeEvent::SessionCatalogUpdated { .. } => false,
                };
                if changed && !deliver_snapshot(sink, &view).await {
                    return Ok(());
                }
                false
            }
            Err(_error) => true,
        };
        if !needs_resync {
            continue;
        }

        view.set_connection_status(SessionConnectionViewStatus::Reconnecting);
        if !deliver_snapshot(sink, &view).await {
            return Ok(());
        }
        drop(connection);
        let mut delay = INITIAL_RECONNECT_DELAY;
        loop {
            if sink.is_closed() {
                return Ok(());
            }
            if let Ok(mut next_connection) = client.connect(&follow.client_name).await
                && let Ok(next_view) = attach(client, &mut next_connection, follow).await
            {
                view = next_view;
                if !deliver_snapshot(sink, &view).await {
                    return Ok(());
                }
                connection = next_connection;
                break;
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(MAX_RECONNECT_DELAY);
        }
    }
}

/// Attach to the session and build its view from the projection window, submissions, pending
/// permissions, runtime work, and pending interactions.
async fn attach(
    client: &BcodeClient,
    connection: &mut ClientConnection,
    follow: &SessionViewFollow,
) -> Result<SessionView, ClientError> {
    let attached = connection
        .attach_session_projection_window_with_input_history(
            follow.session_id,
            follow.projection.clone(),
        )
        .await?;
    let runtime = attached.runtime_selection;
    let mut view = SessionView::new();
    view.set_session_summary(attached.session);
    view.set_runtime_selection(
        runtime.provider_plugin_id,
        runtime.requested_model_id.or(runtime.model_id),
        runtime.effective_model_id,
        runtime.reasoning_effort,
        runtime.reasoning_summary,
        None,
    );
    view.set_agent_id(runtime.agent_id);
    view.set_reasoning_presentation_policy(follow.reasoning_policy);
    if let Some(window) = attached.projection_window.as_ref() {
        view.set_history_window_metadata(
            window.source_range.map(|range| range.start_sequence),
            window.source_range.map(|range| range.end_sequence),
            window.has_older,
            window.has_newer,
        );
    }
    view.apply_history(&attached.history);
    for record in &attached.submissions {
        view.apply_submission_record(record);
    }
    let permissions = permission_views(
        client.list_permissions().await.unwrap_or_default(),
        follow.session_id,
    );
    view.set_pending_permissions(permissions);
    if let Ok(runtime_work) = client.list_runtime_work(follow.session_id).await {
        view.set_runtime_work_snapshots(&runtime_work);
    }
    if let Ok(interactions) = load_pending_interactions(
        client,
        follow.session_id,
        follow.interaction_kind.as_deref(),
    )
    .await
    {
        view.set_pending_interactions(interactions);
    }
    view.set_connection_status(SessionConnectionViewStatus::Attached);
    Ok(view)
}

/// Pending permission requests addressed to `session_id`, as view state.
#[must_use]
pub fn permission_views(
    permissions: Vec<PermissionSummary>,
    session_id: SessionId,
) -> Vec<PermissionView> {
    permissions
        .into_iter()
        .filter(|permission| permission.is_addressed_to(session_id))
        .map(|permission| PermissionView {
            permission_id: permission.permission_id,
            session_id: Some(permission.session_id),
            tool_call_id: permission.tool_call_id,
            tool_name: permission.tool_name,
            producer_plugin_id: None,
            arguments_json: permission.arguments_json,
            batch: permission.batch.map(|batch| PermissionBatchView {
                batch_id: batch.batch_id,
                call_index: batch.call_index,
                call_count: batch.call_count,
            }),
            agent_id: permission.agent_id,
            title: Some("Permission requested".to_string()),
            policy_source: permission.policy_source,
            detail: permission.policy_reason,
            resolved: false,
            approved: None,
            can_remember: permission.can_remember_policy,
        })
        .collect()
}

/// Pending tool exchanges addressed to `session_id`, as interaction view state.
///
/// `interaction_kind` names each exchange's interaction kind; without it, or when it returns
/// `None`, the exchange's schema is the kind.
///
/// # Errors
///
/// * Returns the client error when the pending exchanges cannot be listed.
pub async fn load_pending_interactions(
    client: &BcodeClient,
    session_id: SessionId,
    interaction_kind: Option<&InteractionKind>,
) -> Result<Vec<InteractionViewSummary>, ClientError> {
    let mut interactions = Vec::new();
    for request in client
        .list_pending_tool_exchanges()
        .await?
        .into_iter()
        .filter(|request| request.is_addressed_to(session_id))
    {
        let exchange = request.request;
        let kind = interaction_kind
            .and_then(|resolve| {
                resolve(
                    &exchange.producer_id,
                    &exchange.schema,
                    exchange.schema_version,
                )
            })
            .unwrap_or_else(|| exchange.schema.clone());
        interactions.push(InteractionViewSummary {
            interaction_id: exchange.exchange_id.clone(),
            producer_id: Some(exchange.producer_id.clone()),
            exchange_schema: Some(exchange.schema.clone()),
            exchange_schema_version: Some(exchange.schema_version),
            kind,
            tool_call_id: Some(exchange.invocation_id),
            title: Some(exchange.producer_id),
            required: exchange.response_policy == ToolExchangeResponsePolicy::Required,
            snapshot: Some(exchange.payload),
            state: InteractionViewState::Pending,
            status_detail: None,
            resolved: false,
            resolution: None,
        });
    }
    Ok(interactions)
}
