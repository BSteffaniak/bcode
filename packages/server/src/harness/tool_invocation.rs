//! Host-issued asynchronous ordinary-tool endpoint. No model turn is created here.
//!
//! The adapter receives only a bounded queue handle, not a grant constructor. The host drains
//! the queue outside the harness plugin invocation/lock and returns a correlated response.
//! Restart loses the live grant; canonical claims still prohibit replay of uncertain effects.
use super::{CanonicalHarnessStore, HarnessOwner, ManagedHarnessStore};
use crate::harness_dispatch::{ManagedCaller, ManagedSessionAdmission};
use crate::*;
use bcode_harness_models::{
    MANAGED_TOOL_BRIDGE_VERSION, MAX_MANAGED_TOOL_MESSAGE_BYTES, ManagedToolOperation,
    ManagedToolOutcome, ManagedToolRequest, ManagedToolResponse,
};
use bcode_ipc::harness::ManagedSessionCall;
use bcode_session_models::{ExternalHarnessOwner, ManagedToolClaimOutcome, ManagedToolIdentity};
use tokio::sync::oneshot;

const MAX_PREPARATIONS: usize = 128;
const QUEUE_CAPACITY: usize = 16;

type BridgeResult<T> = Result<T, ()>;

struct QueuedCall {
    request: ManagedToolRequest,
    response: oneshot::Sender<ManagedToolResponse>,
}

/// Capability bound to one host-authenticated producer/session/generation. Not serializable.
/// Cloning retains this exact grant, never authority to choose a different session or producer.
#[derive(Clone)]
pub struct ManagedToolEndpoint {
    sender: mpsc::Sender<QueuedCall>,
}
impl ManagedToolEndpoint {
    /// Queue a bounded request and await its correlated response. Never calls a plugin inline.
    /// # Errors
    /// Rejects malformed requests and closed/revoked host queues.
    pub async fn request(
        &self,
        request: ManagedToolRequest,
    ) -> Result<ManagedToolResponse, String> {
        request
            .validate()
            .map_err(|_| "invalid managed tool request".to_owned())?;
        let (response, receive) = oneshot::channel();
        self.sender
            .send(QueuedCall { request, response })
            .await
            .map_err(|_| "managed tool endpoint unavailable".to_owned())?;
        receive
            .await
            .map_err(|_| "managed tool response unavailable".to_owned())
    }
}

struct Grant {
    id: String,
    caller: ManagedCaller,
    call: ManagedSessionCall,
    owner: HarnessOwner,
    durable_owner: ExternalHarnessOwner,
    working_directory: PathBuf,
    producer: String,
    bootstrap_sha256: Option<String>,
    ceiling: TurnExecutionOptions,
    agent_id: String,
    cancellation: Arc<TurnCancelState>,
}

fn bridge_stage(stage: &'static str) {
    tracing::warn!(target: "bcode_server::harness", stage, "managed tool bridge progress");
}

fn grant_rejected(category: &'static str) {
    tracing::warn!(target: "bcode_server::harness", category, "managed tool grant check rejected");
}

fn owner_lookup_rejected(error: super::ManagedHarnessError) {
    let category = match error {
        super::ManagedHarnessError::Invalid => "invalid",
        super::ManagedHarnessError::Storage => "storage",
        super::ManagedHarnessError::StaleOwner => "stale_owner",
        super::ManagedHarnessError::Conflict => "conflict",
        super::ManagedHarnessError::Uncertain => "uncertain",
        super::ManagedHarnessError::Unavailable => "unavailable",
        super::ManagedHarnessError::Deadline => "deadline",
        super::ManagedHarnessError::Unsupported => "unsupported",
    };
    tracing::warn!(target: "bcode_server::harness", stage = "durable_owner_unavailable", category, "managed tool owner lookup rejected");
}

impl Grant {
    async fn check(
        &self,
        state: &Arc<ServerState>,
    ) -> BridgeResult<(
        bcode_session::SessionOwnershipGuard,
        Option<crate::workflow_operations::AuthorityGuard>,
    )> {
        if self.cancellation.is_cancelled() || state.shutdown_requested.load(Ordering::SeqCst) {
            grant_rejected("cancelled_or_shutdown");
            return Err(());
        }
        let workflow_guard = match &self.caller {
            ManagedCaller::Client(client) => {
                self.check_bootstrap(state, *client).await?;
                None
            }
            ManagedCaller::Workflow {
                run_id,
                dispatch_identity,
                session_id,
                plugin_id,
            } => {
                if *session_id != self.owner.session_id {
                    return Err(());
                }
                Some(
                    crate::harness_workflow::authorize_call(
                        state,
                        run_id,
                        dispatch_identity,
                        *session_id,
                        plugin_id,
                        &self.call,
                    )
                    .await
                    .map_err(|_| ())?,
                )
            }
            ManagedCaller::Invocation { .. } => return Err(()),
        };
        let hold = self.check_owner(state).await?;
        bridge_stage("grant_workspace_start");
        let directory = state
            .sessions
            .session_working_directory(self.owner.session_id)
            .await
            .map_err(|_| grant_rejected("workspace_unavailable"))?;
        if super::admission::canonical_directory(&directory)
            .await
            .map_err(|_| grant_rejected("workspace_invalid"))?
            != self.working_directory
        {
            grant_rejected("workspace_changed");
            return Err(());
        }
        // A session-derived profile is live authority, not a permanent grant. In particular,
        // switching build to plan must invalidate a retained queue and any pending approval.
        // Explicit execution profiles remain pinned by the admitted turn/workflow ceiling.
        if self.ceiling.agent_profile.is_none() {
            let selected = state
                .sessions
                .current_agent_selection(self.owner.session_id)
                .await
                .map_err(|_| grant_rejected("agent_selection_unavailable"))?;
            let selected = match selected {
                Some(agent) => agent,
                None => default_agent_id(&list_profiles(state, None).await),
            };
            if selected != self.agent_id {
                grant_rejected("agent_selection_changed");
                return Err(());
            }
        }
        if self.cancellation.is_cancelled() {
            grant_rejected("cancelled_after_admission");
            return Err(());
        }
        bridge_stage("grant_workspace_done");
        Ok((hold, workflow_guard))
    }

    async fn check_owner(
        &self,
        state: &Arc<ServerState>,
    ) -> BridgeResult<bcode_session::SessionOwnershipGuard> {
        let store = CanonicalHarnessStore::new(state);
        bridge_stage("grant_owner_start");
        if store
            .current_owner(self.owner.session_id)
            .await
            .map_err(owner_lookup_rejected)?
            != self.owner
        {
            grant_rejected("durable_owner_changed");
            return Err(());
        }
        bridge_stage("grant_owner_done");
        bridge_stage("grant_acquire_start");
        let hold = store
            .acquire(&self.owner)
            .await
            .map_err(|_| grant_rejected("owner_lease"))?;
        bridge_stage("grant_acquire_done");
        bridge_stage("grant_selected_start");
        let selected = store
            .selected(&self.owner)
            .await
            .map_err(|_| grant_rejected("owner_selection"))?;
        bridge_stage("grant_selected_done");
        if selected.instance != self.call.harness.instance
            || selected.instance.plugin_id != self.producer
        {
            grant_rejected("producer_or_instance_changed");
            return Err(());
        }
        let admission = super::CanonicalManagedAdmission::new(store.clone());
        drop(store);
        bridge_stage("grant_admission_start");
        if admission
            .existing_tool_authority(&self.caller, &self.call, true)
            .await
            .map_err(|_| {
                tracing::warn!(target: "bcode_server::harness", category = "retained_grant_admission", "managed tool authority rejected");
            })?
            != self.owner
        {
            return Err(());
        }
        bridge_stage("grant_admission_done");
        drop(admission);
        Ok(hold)
    }

    async fn check_bootstrap(&self, state: &ServerState, client: ClientId) -> BridgeResult<()> {
        let current = bootstrap_hash(state, client)
            .await
            .map_err(|()| grant_rejected("bootstrap_current_unavailable"))?;
        let expected = self
            .bootstrap_sha256
            .as_ref()
            .ok_or_else(|| grant_rejected("bootstrap_missing"))?;
        if &current != expected {
            grant_rejected("bootstrap_changed");
            return Err(());
        }
        Ok(())
    }

    fn provenance(&self) -> PluginInvocationScope {
        let scope = PluginInvocationScope::session(self.owner.session_id.to_string());
        if let ManagedCaller::Client(client) = self.caller {
            scope.with_client_id(client.to_string())
        } else {
            scope
        }
    }

    fn allows(&self, name: &str) -> bool {
        self.ceiling.tools != bcode_session_models::TurnToolPolicy::Disabled
            && self
                .ceiling
                .tool_allowlist
                .as_ref()
                .is_none_or(|names| names.iter().any(|allowed| allowed == name))
    }
}

async fn bootstrap_hash(state: &ServerState, client: ClientId) -> BridgeResult<String> {
    let context = state.client_runtime_context(client).await.ok_or(())?;
    // Hash retained host bootstrap identity/config; never export config or credentials.
    hash_value(&serde_json::to_value(context).map_err(|_| ())?)
}
fn hash_value(value: &serde_json::Value) -> BridgeResult<String> {
    bcode_workflow::workflow_canonical_value_sha256(value).map_err(|_| ())
}

struct RetainedPreparation {
    call: bcode_model::ToolCall,
    tool: RegisteredTool,
    preparation: ToolPreparationResponse,
    identity: ManagedToolIdentity,
    cancellation: Arc<TurnCancelState>,
}

/// Host-side queue driver.
///
/// Construct only at authenticated client bootstrap or exact managed
/// workflow receipt admission. Never construct from native arguments or a deserialized owner.
/// The adapter must not await `serve` while holding its plugin service lock.
pub struct ManagedToolBridge {
    state: Arc<ServerState>,
    grant: Arc<Grant>,
    receiver: mpsc::Receiver<QueuedCall>,
    retained: BTreeMap<String, Arc<RetainedPreparation>>,
}
impl Drop for ManagedToolBridge {
    fn drop(&mut self) {
        self.grant.cancellation.close();
        for preparation in self.retained.values() {
            preparation.cancellation.close();
        }
    }
}

impl ManagedToolBridge {
    /// Issue a live endpoint under the already host-admitted execution policy ceiling.
    /// `caller`, `ceiling` and cancellation must come from the daemon's authenticated admission,
    /// not the request payload. Bypass permission mode is deliberately not supported here.
    /// Exact workflow lineage is checked by `harness_workflow::authorize_call` at issuance and
    /// again after approval. Invocation parentage is not accepted as bootstrap authority.
    /// # Errors
    /// Rejects missing bootstrap, stale owners, wrong producer, unsupported policy and receipt mismatch.
    pub async fn issue(
        state: Arc<ServerState>,
        caller: ManagedCaller,
        call: ManagedSessionCall,
        ceiling: TurnExecutionOptions,
        cancellation: Arc<TurnCancelState>,
    ) -> Result<(ManagedToolEndpoint, Self), String> {
        Self::issue_inner(state, caller, call, ceiling, cancellation)
            .await
            .map_err(|()| "managed tool grant denied".to_owned())
    }

    async fn issue_inner(
        state: Arc<ServerState>,
        caller: ManagedCaller,
        call: ManagedSessionCall,
        ceiling: TurnExecutionOptions,
        cancellation: Arc<TurnCancelState>,
    ) -> BridgeResult<(ManagedToolEndpoint, Self)> {
        if matches!(caller, ManagedCaller::Invocation { .. })
            || ceiling.permission_mode != bcode_session_models::TurnPermissionMode::Enforce
        {
            return Err(());
        }
        let (owner, durable_owner) = {
            let store = CanonicalHarnessStore::new(&state);
            let admission = super::CanonicalManagedAdmission::new(store.clone());
            let owner = admission.existing(&caller, &call).await.map_err(|_| ())?;
            drop(admission);
            let durable_owner = store.durable(&owner).map_err(|_| ())?;
            drop(store);
            (owner, durable_owner)
        };
        let directory = state
            .sessions
            .session_working_directory(owner.session_id)
            .await
            .map_err(|_| ())?;
        let working_directory = super::admission::canonical_directory(&directory)
            .await
            .map_err(|_| ())?;
        let bootstrap_sha256 = if let ManagedCaller::Client(client) = caller {
            Some(bootstrap_hash(&state, client).await?)
        } else {
            None
        };
        let agent_id = ceiling
            .agent_profile
            .clone()
            .unwrap_or(session_agent_selection(&state, owner.session_id).await);
        let producer = call.harness.instance.plugin_id.clone();
        let grant = Arc::new(Grant {
            id: uuid::Uuid::new_v4().to_string(),
            caller,
            call,
            owner,
            durable_owner,
            working_directory,
            producer,
            bootstrap_sha256,
            ceiling,
            agent_id,
            cancellation,
        });
        let _authority = grant.check(&state).await?;
        let (sender, receiver) = mpsc::channel(QUEUE_CAPACITY);
        Ok((
            ManagedToolEndpoint { sender },
            Self {
                state,
                grant,
                receiver,
                retained: BTreeMap::new(),
            },
        ))
    }

    /// Drain the bounded asynchronous request route outside native/plugin locks.
    /// Cooperative cancellation closes admission; already-started effects remain canonically uncertain
    /// if physical settlement/result persistence cannot be proved. Dropping this future never grants retry.
    pub fn serve(
        mut self: Box<Self>,
    ) -> std::pin::Pin<Box<dyn Future<Output = ()> + Send + 'static>> {
        Box::pin(async move {
            let mut pending = tokio::task::JoinSet::<(
                oneshot::Sender<ManagedToolResponse>,
                ManagedToolResponse,
            )>::new();
            let mut accepting = true;
            loop {
                tokio::select! {
                    biased;
                    () = self.grant.cancellation.cancelled(), if accepting => {
                        accepting = false;
                        self.receiver.close();
                        for preparation in self.retained.values() { preparation.cancellation.close(); }
                    }
                    Some(completed) = pending.join_next(), if !pending.is_empty() => {
                        if let Ok((sender, response)) = completed {
                            let _ = sender.send(bounded_response(response));
                        } else {
                            self.grant.cancellation.close();
                            bridge_stage("invocation_task_failed");
                        }
                    }
                    queued = self.receiver.recv(), if accepting => {
                        let Some(queued) = queued else {
                            accepting = false;
                            for preparation in self.retained.values() { preparation.cancellation.close(); }
                            continue;
                        };
                        let id = queued.request.request_id.clone();
                        if let ManagedToolOperation::Invoke { invocation_id, preparation_sha256 } = &queued.request.operation {
                            let retained = self.retained.get(invocation_id).cloned();
                            if retained.is_none() {
                                let work = async {
                                    if queued.request.validate().is_ok() && self.grant.check(&self.state).await.is_ok() {
                                        self.recorded_invocation(invocation_id.clone(), preparation_sha256).await.unwrap_or(ManagedToolOutcome::Rejected)
                                    } else { ManagedToolOutcome::Rejected }
                                };
                                let outcome = work.await;
                                let _ = queued.response.send(bounded_response(ManagedToolResponse { version: MANAGED_TOOL_BRIDGE_VERSION, request_id: id, outcome }));
                                continue;
                            }
                            if let Some(retained) = retained.filter(|retained| queued.request.validate().is_ok()
                                && pending.len() < QUEUE_CAPACITY && retained.identity.preparation_sha256 == *preparation_sha256) {
                                let runner = InvocationRunner { state: Arc::clone(&self.state), grant: Arc::clone(&self.grant) };
                                pending.spawn(Box::pin(async move {
                                    let outcome = runner.invoke(retained).await
                                        .unwrap_or(ManagedToolOutcome::Rejected);
                                    (queued.response, ManagedToolResponse { version: MANAGED_TOOL_BRIDGE_VERSION, request_id: id, outcome })
                                }));
                                continue;
                            }
                            let _ = queued.response.send(ManagedToolResponse {
                                version: MANAGED_TOOL_BRIDGE_VERSION, request_id: id, outcome: ManagedToolOutcome::Rejected,
                            });
                            continue;
                        }
                        let outcome = self.handle(queued.request).await.unwrap_or(ManagedToolOutcome::Rejected);
                        bridge_stage("handle_done");
                        let _ = queued.response.send(bounded_response(ManagedToolResponse {
                            version: MANAGED_TOOL_BRIDGE_VERSION, request_id: id, outcome,
                        }));
                    }
                    else => break,
                }
                if !accepting && pending.is_empty() {
                    break;
                }
            }
        })
    }

    async fn recorded_invocation(
        &self,
        id: String,
        hash: &str,
    ) -> BridgeResult<ManagedToolOutcome> {
        let claim = self
            .state
            .sessions
            .inspect_managed_tool(&self.grant.durable_owner, id)
            .await
            .map_err(|_| ())?
            .ok_or(())?;
        if claim.identity.grant_id != self.grant.id || claim.identity.preparation_sha256 != hash {
            return Err(());
        }
        claim_outcome(
            self.state
                .sessions
                .claim_managed_tool(claim.identity)
                .await
                .map_err(|_| ())?,
        )
    }

    async fn handle(&mut self, request: ManagedToolRequest) -> BridgeResult<ManagedToolOutcome> {
        bridge_stage("handle_start");
        request.validate().map_err(|_| ())?;
        bridge_stage("handle_authority_start");
        let _authority = self.grant.check(&self.state).await?;
        bridge_stage("handle_authority_done");
        match request.operation {
            ManagedToolOperation::Catalog { after, limit } => {
                let catalog = collect_server_tool_catalog(&self.state)
                    .await
                    .map_err(|_| ())?;
                let mut tools = catalog
                    .tools()
                    .into_iter()
                    .filter(|tool| self.grant.allows(&tool.definition.name))
                    .filter(|tool| {
                        after
                            .as_ref()
                            .is_none_or(|after| tool.definition.name > *after)
                    })
                    .take(usize::from(limit) + 1)
                    .collect::<Vec<_>>();
                let overflow = tools.len() > usize::from(limit);
                tools.truncate(usize::from(limit));
                let next = overflow
                    .then(|| tools.last().map(|tool| tool.definition.name.clone()))
                    .flatten();
                let tools = tools
                    .into_iter()
                    .map(|tool| serde_json::to_value(tool.definition).map_err(|_| ()))
                    .collect::<BridgeResult<Vec<_>>>()?;
                Ok(ManagedToolOutcome::Catalog { tools, next })
            }
            ManagedToolOperation::Prepare {
                invocation_id,
                tool_name,
                arguments,
            } => self.prepare(invocation_id, tool_name, arguments).await,
            ManagedToolOperation::Invoke {
                invocation_id,
                preparation_sha256,
            } => {
                let Some(retained) = self.retained.get(&invocation_id).cloned() else {
                    return self
                        .recorded_invocation(invocation_id, &preparation_sha256)
                        .await;
                };
                if retained.identity.preparation_sha256 != preparation_sha256 {
                    return Err(());
                }
                InvocationRunner {
                    state: Arc::clone(&self.state),
                    grant: Arc::clone(&self.grant),
                }
                .invoke(retained)
                .await
            }
            ManagedToolOperation::Inspect { invocation_id } => {
                let claim = self
                    .state
                    .sessions
                    .inspect_managed_tool(&self.grant.durable_owner, invocation_id)
                    .await
                    .map_err(|_| ())?;
                let Some(claim) = claim else {
                    return Ok(ManagedToolOutcome::Missing);
                };
                if claim.identity.grant_id != self.grant.id {
                    return Err(());
                }
                claim_outcome(
                    self.state
                        .sessions
                        .claim_managed_tool(claim.identity)
                        .await
                        .map_err(|_| ())?,
                )
            }
            ManagedToolOperation::Cancel { invocation_id } => {
                self.retained
                    .get(&invocation_id)
                    .ok_or(())?
                    .cancellation
                    .close();
                Ok(ManagedToolOutcome::CancelRequested)
            }
        }
    }

    /// Reclaim only terminal canonical preparations or cancelled, never-claimed work.
    /// Uncertain claims stay pinned and exert explicit active-capacity pressure.
    async fn reclaim_preparations(&mut self) -> BridgeResult<()> {
        if self.retained.len() < MAX_PREPARATIONS {
            return Ok(());
        }
        let mut retired = Vec::new();
        for (id, retained) in &self.retained {
            if Arc::strong_count(retained) != 1 {
                continue;
            }
            let claim = self
                .state
                .sessions
                .inspect_managed_tool(&self.grant.durable_owner, id.clone())
                .await
                .map_err(|_| ())?;
            if claim
                .as_ref()
                .is_some_and(|claim| claim.result_sequence.is_some())
                || (claim.is_none() && retained.cancellation.is_cancelled())
            {
                retired.push(id.clone());
            }
        }
        for id in retired {
            self.retained.remove(&id);
        }
        if self.retained.len() >= MAX_PREPARATIONS {
            return Err(());
        }
        Ok(())
    }

    async fn prepare(
        &mut self,
        id: String,
        name: String,
        arguments: serde_json::Value,
    ) -> BridgeResult<ManagedToolOutcome> {
        bridge_stage("prepare_start");
        if !self.grant.allows(&name) {
            return Err(());
        }
        if let Some(retained) = self.retained.get(&id) {
            if retained.call.name != name || retained.call.arguments != arguments {
                return Err(());
            }
            return Ok(prepared_outcome(retained));
        }
        bridge_stage("prepare_inspect_start");
        if let Some(claim) = self
            .state
            .sessions
            .inspect_managed_tool(&self.grant.durable_owner, id.clone())
            .await
            .map_err(|_| ())?
        {
            if claim.identity.grant_id != self.grant.id
                || claim.identity.tool_name != name
                || claim.identity.arguments_sha256 != hash_value(&arguments)?
            {
                return Err(());
            }
            return Ok(ManagedToolOutcome::Prepared {
                invocation_id: id,
                preparation_sha256: claim.identity.preparation_sha256,
            });
        }
        bridge_stage("prepare_inspect_done");
        self.reclaim_preparations().await?;
        bridge_stage("prepare_reclaim_done");
        let call = bcode_model::ToolCall {
            id: id.clone(),
            name,
            arguments,
        };
        let tool = registered_server_tool(&self.state, &call.name)
            .await
            .map_err(|_| ())?;
        let ToolSource::Plugin { plugin_id } = &tool.source else {
            return Err(());
        };
        let producer_plugin_id = plugin_id.clone();
        let cancellation = Arc::new(TurnCancelState::default());
        bridge_stage("prepare_plugin_start");
        let preparation = tokio::time::timeout(
            Duration::from_millis(
                self.state
                    .startup_config
                    .tools
                    .execution
                    .preparation_timeout_ms
                    .max(1),
            ),
            prepare_registered_server_tool_with_provenance(
                &self.state,
                self.grant.owner.session_id,
                tool.clone(),
                &call,
                &cancellation,
                Some(self.grant.provenance()),
            ),
        )
        .await
        .map_err(|_| ())?
        .map_err(|_| ())?;
        bridge_stage("prepare_plugin_done");
        let preparation_sha256 =
            preparation_hash(&tool, &producer_plugin_id, &preparation, &self.grant)?;
        bridge_stage("prepare_revalidate_start");
        let _authority = self.grant.check(&self.state).await?;
        bridge_stage("prepare_revalidate_done");
        let identity = ManagedToolIdentity {
            owner: self.grant.durable_owner.clone(),
            grant_id: self.grant.id.clone(),
            invocation_id: id.clone(),
            producer_plugin_id,
            tool_name: call.name.clone(),
            arguments_sha256: hash_value(&call.arguments)?,
            preparation_sha256,
            working_directory: self.grant.working_directory.to_str().ok_or(())?.to_owned(),
        };
        identity.validate().map_err(|_| ())?;
        let retained = Arc::new(RetainedPreparation {
            call,
            tool,
            preparation,
            identity,
            cancellation,
        });
        let outcome = prepared_outcome(&retained);
        self.retained.insert(id, retained);
        bridge_stage("prepare_done");
        Ok(outcome)
    }
}

struct InvocationRunner {
    state: Arc<ServerState>,
    grant: Arc<Grant>,
}
impl InvocationRunner {
    async fn retained_outcome(
        &self,
        retained: &RetainedPreparation,
    ) -> BridgeResult<Option<ManagedToolOutcome>> {
        let Some(claim) = self
            .state
            .sessions
            .inspect_managed_tool(&self.grant.durable_owner, retained.call.id.clone())
            .await
            .map_err(|_| ())?
        else {
            return Ok(None);
        };
        if claim.identity != retained.identity {
            return Err(());
        }
        claim_outcome(
            self.state
                .sessions
                .claim_managed_tool(retained.identity.clone())
                .await
                .map_err(|_| ())?,
        )
        .map(Some)
    }

    async fn invoke(&self, retained: Arc<RetainedPreparation>) -> BridgeResult<ManagedToolOutcome> {
        bridge_stage("invoke_start");
        let _authority = self.grant.check(&self.state).await?;
        bridge_stage("invoke_authority_done");
        if !self.grant.allows(&retained.call.name) {
            return Err(());
        }
        // Read exact canonical evidence before asking permission again. A retry never reexecutes.
        bridge_stage("invoke_retained_start");
        if let Some(outcome) = self.retained_outcome(&retained).await? {
            return Ok(outcome);
        }
        bridge_stage("invoke_retained_done");
        if retained.cancellation.is_cancelled() {
            return Err(());
        }
        let mut catalog = UnifiedToolCatalog::new();
        catalog.insert(retained.tool.clone());
        bridge_stage("invoke_context_start");
        let host_context = invocation_service_host_context(
            &self.state,
            self.grant.owner.session_id,
            &self.grant.working_directory,
        )
        .await
        .map_err(|_| ())?;
        bridge_stage("invoke_context_done");
        let outcome = Mutex::new(None);
        {
            let invoker = PinnedInvoker {
                state: &self.state,
                grant: &self.grant,
                retained: &retained,
                outcome: &outcome,
            };
            let context = bcode_agent_runtime::RuntimePermissionContext {
                session_id: self.grant.owner.session_id,
                agent_id: self.grant.agent_id.clone(),
            };
            let scope = TurnScope::without_events(
                format!("managed-tool:{}", self.grant.id),
                TurnGeneration::new(self.grant.owner.generation),
            );
            bridge_stage("invoke_runtime_start");
            let runtime = AgentRuntime::new();
            let mut rounds = ToolRoundState::new(1);
            let calls = [retained.call.clone()];
            {
                let execution = async {
                    runtime
                        .execute_prepared_tool_batch_with_host_context(
                            &catalog,
                            &ServerAuthorizationCoordinator {
                                allow_user_questions: self.grant.ceiling.allow_user_questions,
                                ..ServerAuthorizationCoordinator::new(
                                    &self.state,
                                    self.grant.owner.session_id,
                                    &retained.cancellation,
                                    1,
                                    self.grant.ceiling.tools,
                                    bcode_session_models::TurnPermissionMode::Enforce,
                                    &self.grant.agent_id,
                                )
                            },
                            &invoker,
                            &calls,
                            &mut rounds,
                            &context,
                            &host_context,
                            bcode_tool::ToolExecutionOptions {
                                parallel: false,
                                ..bcode_tool::ToolExecutionOptions::default()
                            },
                            &scope,
                        )
                        .await
                };
                tokio::pin!(execution);
                tokio::select! {
                    biased;
                    () = self.grant.cancellation.cancelled() => {
                        retained.cancellation.close();
                        let _ = scope.control().begin_cancellation();
                        // Keep polling until physical settlement; never turn cancellation into replay.
                        let _ = execution.await;
                    }
                    result = &mut execution => { let _ = result; }
                }
            }
        }
        bridge_stage("invoke_runtime_done");
        Ok(outcome
            .lock()
            .await
            .take()
            .unwrap_or(ManagedToolOutcome::Denied))
    }
}

fn bounded_response(mut response: ManagedToolResponse) -> ManagedToolResponse {
    if serde_json::to_vec(&response)
        .map_or(true, |bytes| bytes.len() > MAX_MANAGED_TOOL_MESSAGE_BYTES)
    {
        // Never truncate a canonical result into a different purported result.
        response.outcome = ManagedToolOutcome::Uncertain;
    }
    response
}

fn preparation_hash(
    tool: &RegisteredTool,
    producer: &str,
    preparation: &ToolPreparationResponse,
    grant: &Grant,
) -> BridgeResult<String> {
    if serde_json::to_vec(preparation).map_err(|_| ())?.len() > MAX_MANAGED_TOOL_MESSAGE_BYTES {
        return Err(());
    }
    hash_value(&serde_json::json!({
        "tool": tool.definition, "producer": producer, "preparation": preparation,
        "ceiling": grant.ceiling, "agent": grant.agent_id,
    }))
}

fn prepared_outcome(retained: &RetainedPreparation) -> ManagedToolOutcome {
    ManagedToolOutcome::Prepared {
        invocation_id: retained.call.id.clone(),
        preparation_sha256: retained.identity.preparation_sha256.clone(),
    }
}
fn claim_outcome(outcome: ManagedToolClaimOutcome) -> BridgeResult<ManagedToolOutcome> {
    match outcome {
        ManagedToolClaimOutcome::Recorded { result, .. } => Ok(ManagedToolOutcome::Recorded {
            result: serde_json::to_value(result).map_err(|_| ())?,
        }),
        ManagedToolClaimOutcome::Uncertain(_) => Ok(ManagedToolOutcome::Uncertain),
        ManagedToolClaimOutcome::New(_) => Err(()),
    }
}

struct PinnedInvoker<'a> {
    state: &'a Arc<ServerState>,
    grant: &'a Arc<Grant>,
    retained: &'a RetainedPreparation,
    outcome: &'a Mutex<Option<ManagedToolOutcome>>,
}
impl PinnedInvoker<'_> {
    async fn record_result(
        &self,
        result: &ToolInvocationResponse,
        claim_sequence: u64,
    ) -> Result<(), RuntimeError> {
        let record = bcode_session_models::ToolInvocationResultRecord {
            invocation_id: self.retained.call.id.clone(),
            model_output: result.output.clone(),
            is_error: result.is_error,
            content: session_tool_result_content(&result.content),
            presentation: None,
            result: result.result.clone().map(service_tool_result_to_session),
        };
        self.state
            .sessions
            .complete_managed_tool(
                self.retained.identity.clone(),
                claim_sequence,
                record.clone(),
            )
            .await
            .map_err(|_| RuntimeError::Cancelled)?;
        *self.outcome.lock().await = Some(ManagedToolOutcome::Recorded {
            result: serde_json::to_value(record).map_err(|_| RuntimeError::Cancelled)?,
        });
        Ok(())
    }
}

impl ToolInvoker for PinnedInvoker<'_> {
    fn prepare_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        request: &'a ToolPreparationRequest,
        scope: &'a PreparationScope,
    ) -> RuntimeFuture<'a, ToolPreparationResponse> {
        Box::pin(async move {
            if !scope.accepts_work()
                || tool != &self.retained.tool
                || request.invocation.invocation_id != self.retained.call.id
                || request.invocation.tool_name != self.retained.call.name
                || request.invocation.arguments != self.retained.call.arguments
            {
                return Err(RuntimeError::Cancelled);
            }
            Ok(self.retained.preparation.clone())
        })
    }
    fn invoke_tool<'a>(
        &'a self,
        tool: &'a RegisteredTool,
        invocation: &'a PreparedToolInvocation,
        scope: &'a InvocationScope,
    ) -> RuntimeFuture<'a, ToolInvocationResponse> {
        Box::pin(async move {
            // This is after the ordinary coordinator's approval and before canonical claim/start.
            *self.outcome.lock().await = Some(ManagedToolOutcome::Rejected);
            bridge_stage("pinned_authority_start");
            let authority = self
                .grant
                .check(self.state)
                .await
                .map_err(|()| RuntimeError::Cancelled)?;
            bridge_stage("pinned_authority_done");
            if tool != &self.retained.tool
                || invocation.preparation != self.retained.preparation
                || invocation.invocation.invocation_id != self.retained.call.id
                || invocation.invocation.tool_name != self.retained.call.name
                || invocation.invocation.arguments != self.retained.call.arguments
                || !scope.accepts_work()
                || self.retained.cancellation.is_cancelled()
            {
                return Err(RuntimeError::Cancelled);
            }
            let claimed = self
                .state
                .sessions
                .claim_managed_tool(self.retained.identity.clone())
                .await
                .map_err(|_| RuntimeError::Cancelled)?;
            bridge_stage("pinned_claim_start");
            let claim = match claimed {
                ManagedToolClaimOutcome::New(claim) => claim,
                other => {
                    *self.outcome.lock().await = claim_outcome(other).ok();
                    return Err(RuntimeError::Cancelled);
                }
            };
            // Any failure from here leaves uncertainty, never an unclaimed retry opportunity.
            *self.outcome.lock().await = Some(ManagedToolOutcome::Uncertain);
            self.state
                .sessions
                .append_tool_call_requested(
                    self.grant.owner.session_id,
                    AppendToolCallRequestedInput {
                        tool_call_id: self.retained.call.id.clone(),
                        tool_name: self.retained.call.name.clone(),
                        arguments_json: serde_json::to_string(&self.retained.call.arguments)
                            .map_err(|_| RuntimeError::Cancelled)?,
                        producer_plugin_id: Some(self.retained.identity.producer_plugin_id.clone()),
                        working_directory: Some(self.grant.working_directory.clone()),
                    },
                )
                .await
                .map_err(|_| RuntimeError::Cancelled)?;
            bridge_stage("pinned_claim_done");
            let current_registration = registered_server_tool(self.state, &self.retained.call.name)
                .await
                .map_err(|_| RuntimeError::Cancelled)?;
            if current_registration != self.retained.tool {
                return Err(RuntimeError::Cancelled);
            }
            bridge_stage("pinned_final_authority_start");
            let final_authority = self
                .grant
                .check(self.state)
                .await
                .map_err(|()| RuntimeError::Cancelled)?;
            if self.retained.cancellation.is_cancelled() || !scope.accepts_work() {
                return Err(RuntimeError::Cancelled);
            }
            bridge_stage("pinned_start_start");
            self.state
                .sessions
                .start_managed_tool(self.retained.identity.clone(), claim.claim_sequence)
                .await
                .map_err(|_| RuntimeError::Cancelled)?;
            bridge_stage("pinned_start_done");
            let mut invoker = ServerToolInvoker::new(
                self.state,
                self.grant.owner.session_id,
                &self.grant.working_directory,
                &self.retained.cancellation,
            )
            .with_invocation_provenance(self.grant.provenance());
            invoker.record_dispatch_start = false;
            invoker.persist_lifecycle = false;
            bridge_stage("pinned_dispatch_start");
            let result = invoke_server_registered_tool(&invoker, tool, invocation, scope).await?;
            bridge_stage("pinned_dispatch_done");
            drop(invoker);
            if scope.settlement_unresolved() {
                return Err(RuntimeError::Cancelled);
            }
            bridge_stage("pinned_result_start");
            self.record_result(&result, claim.claim_sequence).await?;
            bridge_stage("pinned_result_done");
            drop(final_authority);
            drop(authority);
            Ok(result)
        })
    }
}

#[cfg(test)]
mod tests;
