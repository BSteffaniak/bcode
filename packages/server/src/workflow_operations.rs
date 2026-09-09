/// Host policy for executable run graph publication, distinct from staging approval.
#[derive(Clone)]
pub struct WorkflowRunGraphPublicationPolicy {
    /// Evaluate canonical publication facts before ownership or persistence effects.
    pub evaluator: std::sync::Arc<
        dyn Fn(
                &bcode_workflow::WorkflowRunGraphPublicationFacts,
            ) -> WorkflowApplicationAuthorizationDecision
            + Send
            + Sync,
    >,
}

impl std::fmt::Debug for WorkflowRunGraphPublicationPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowRunGraphPublicationPolicy")
            .finish_non_exhaustive()
    }
}

/// Host-configured policy for staging run edits. It is never supplied by request payloads.
#[derive(Clone)]
pub struct WorkflowRunGraphEditPolicy {
    /// Evaluate canonical caller and candidate facts before any ownership or persistence effects.
    pub evaluator: std::sync::Arc<
        dyn Fn(
                &bcode_workflow::WorkflowRunGraphEditFacts,
            ) -> WorkflowApplicationAuthorizationDecision
            + Send
            + Sync,
    >,
}

impl std::fmt::Debug for WorkflowRunGraphEditPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowRunGraphEditPolicy")
            .finish_non_exhaustive()
    }
}

/// Host-bound authoring execution context. Adapters provide domain requests, not store handles.
/// The borrow prevents this context from outliving its host; authorization is evaluated per call.
#[derive(Clone, Copy)]
pub struct WorkflowAuthoringApplication<'a> {
    state: &'a std::sync::Arc<ServerState>,
    client_id: super::ClientId,
}

impl<'a> WorkflowAuthoringApplication<'a> {
    #[must_use]
    pub(crate) const fn new(
        state: &'a std::sync::Arc<ServerState>,
        client_id: super::ClientId,
    ) -> Self {
        Self { state, client_id }
    }

    /// Bind this caller's delivery sink only after subscription state is verified.
    /// The transport retains framing and disconnect cleanup ownership.
    pub(crate) async fn subscribe_runs(
        &self,
        event_sink: ClientEventSink,
    ) -> Result<u64, bcode_workflow::WorkflowRunOperationFailure> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        subscribe_runs(self.state, self.client_id, event_sink)
            .await
            .map_err(|error| run_operation_failure(error.into()))
    }

    pub(crate) fn apply_draft_edits(
        &self,
        request: bcode_workflow::ApplyWorkflowDraftEditsRequest,
    ) -> Result<bcode_workflow::WorkflowDraftEditResult, bcode_workflow::WorkflowAuthoringFailure>
    {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        apply_draft_edits(self.state, self.client_id, request).map_err(authoring_failure)
    }

    pub(crate) fn update_draft(
        &self,
        request: &bcode_workflow::UpdateWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftUpdateResult, bcode_workflow::WorkflowAuthoringFailure>
    {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        update_draft(self.state, self.client_id, request).map_err(authoring_failure)
    }

    pub(crate) fn activate_revision(
        &self,
        request: &bcode_workflow::ActivateWorkflowRevisionRequest,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringMutationResult,
        bcode_workflow::WorkflowAuthoringFailure,
    > {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        activate_revision(self.state, self.client_id, request).map_err(authoring_failure)
    }

    pub(crate) fn discard_draft(
        &self,
        request: &bcode_workflow::DiscardWorkflowDraftRequest,
    ) -> Result<
        bcode_workflow::WorkflowAuthoringMutationResult,
        bcode_workflow::WorkflowAuthoringFailure,
    > {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        discard_draft(self.state, self.client_id, request).map_err(authoring_failure)
    }
}

fn run_operation_failure(error: super::ServerError) -> bcode_workflow::WorkflowRunOperationFailure {
    let failure = super::request_error_response(&error);
    drop(error);
    bcode_workflow::WorkflowRunOperationFailure {
        code: failure.code,
        message: failure.message,
    }
}

impl bcode_workflow::WorkflowRunApplication for WorkflowAuthoringApplication<'_> {
    async fn list_all_workflow_mutation_approvals(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        list_mutation_approvals_all(self.state, limit)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn list_workflow_mutation_approvals(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowMutationApprovalInspection>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        list_mutation_approvals(self.state, &run_id, limit)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn resolve_workflow_mutation_approval(
        &self,
        approval_id: String,
        decision: bcode_workflow::WorkflowMutationApprovalDecision,
    ) -> Result<bcode_workflow::WorkflowMutationApprovalResolution, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        resolve_mutation_approval(self.state, &approval_id, decision)
            .await
            .map_err(run_operation_failure)
    }
    async fn workflow_attempt_history(
        &self,
        run_id: String,
        cursor: Option<bcode_workflow::AttemptCursor>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::AttemptSummary>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        attempt_history(self.state, &run_id, cursor.as_ref(), limit)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn workflow_event_history(
        &self,
        run_id: String,
        after_sequence: Option<u64>,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        event_history(self.state, &run_id, after_sequence, limit)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn retry_workflow_node(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        failed_attempt: u32,
    ) -> Result<bcode_workflow::WorkflowNodeRetryResult, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        retry_node(
            self.state,
            &run_id,
            &node_id,
            &activation_id,
            failed_attempt,
        )
        .await
        .map_err(run_operation_failure)
    }
    async fn list_workflow_waits(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WaitingActivation>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        list_waits(self.state, &run_id, limit).map_err(|error| run_operation_failure(error.into()))
    }
    async fn provide_workflow_input(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        value: serde_json::Value,
    ) -> Result<bcode_workflow::WaitingResolutionResult, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        provide_input(self.state, &run_id, &node_id, &activation_id, value)
            .await
            .map_err(run_operation_failure)
    }
    async fn resolve_workflow_approval(
        &self,
        run_id: String,
        node_id: String,
        activation_id: String,
        approved: bool,
    ) -> Result<bcode_workflow::WaitingResolutionResult, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        resolve_approval(self.state, &run_id, &node_id, &activation_id, approved)
            .await
            .map_err(run_operation_failure)
    }
    async fn start_workflow_run(
        &self,
        request: bcode_workflow::WorkflowRunStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        start_run(self.state, request, None)
            .await
            .map_err(run_operation_failure)
    }
    async fn start_workflow(
        &self,
        request: bcode_workflow::WorkflowStartRequest,
    ) -> Result<bcode_workflow::WorkflowRunStartResponse, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        start(self.state, request)
            .await
            .map_err(run_operation_failure)
    }
    async fn workflow_live_event_catch_up(
        &self,
        after_sequence: u64,
        limit: usize,
    ) -> Result<bcode_workflow_view_models::WorkflowLiveEventPage, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        live_event_catch_up(self.state, after_sequence, limit)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
    ) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        associated_run(self.state, &binding_key(key))
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn inspect_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        limit: usize,
    ) -> Result<Option<bcode_workflow::WorkflowRunInspection>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        inspect_associated_run(self.state, &binding_key(key), limit)
            .await
            .map(|value| value.map(|inspection| *inspection))
            .map_err(run_operation_failure)
    }
    async fn control_associated_workflow_run(
        &self,
        key: bcode_workflow::WorkflowRunBindingLookup,
        action: bcode_workflow::WorkflowRunControlAction,
    ) -> Result<(Option<bcode_workflow::WorkflowRunSummary>, bool), Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        control_associated_run(self.state, &binding_key(key), action)
            .await
            .map_err(run_operation_failure)
    }
    async fn inspect_workflow_run_graph(
        &self,
        request: bcode_workflow::WorkflowRunGraphPageRequest,
    ) -> Result<bcode_workflow::WorkflowRunGraphInspection, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        inspect_graph_page(self.state, &request)
            .map_err(|error| run_operation_failure(error.into()))
    }
    async fn list_workflow_runs(
        &self,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        list_runs(self.state, limit).map_err(|error| run_operation_failure(error.into()))
    }

    async fn workflow_run_outputs(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        run_outputs(self.state, &run_id, limit).map_err(|error| run_operation_failure(error.into()))
    }
    async fn workflow_run_status(
        &self,
        run_id: String,
    ) -> Result<Option<bcode_workflow::WorkflowRunSummary>, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        run_status(self.state, &run_id).map_err(|error| run_operation_failure(error.into()))
    }
    async fn pause_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        Box::pin(pause_run(self.state, &run_id))
            .await
            .map_err(run_operation_failure)
    }

    async fn resume_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(run_operation_failure)?;
        Box::pin(resume_run(self.state, &run_id))
            .await
            .map_err(run_operation_failure)
    }
    async fn cancel_workflow_run(&self, run_id: String) -> Result<bool, Self::Error> {
        let result = match self.state.require_workflow_store() {
            Ok(()) => Box::pin(cancel_run(self.state, &run_id)).await,
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            let failure = super::request_error_response(&error);
            bcode_workflow::WorkflowRunOperationFailure {
                code: failure.code,
                message: failure.message,
            }
        })
    }
    async fn inspect_workflow_run(
        &self,
        run_id: String,
        limit: usize,
    ) -> Result<bcode_workflow::WorkflowRunInspection, Self::Error> {
        let result = match self.state.require_workflow_store() {
            Ok(()) => Box::pin(inspect_run(self.state, &run_id, limit)).await,
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            let failure = super::request_error_response(&error);
            bcode_workflow::WorkflowRunAdmissionFailure {
                code: failure.code,
                message: failure.message,
            }
        })
    }
    type Error = bcode_workflow::WorkflowRunOperationFailure;

    async fn start_authored_workflow(
        &self,
        request: bcode_workflow::StartAuthoredWorkflowRequest,
    ) -> Result<bcode_workflow::AuthoredWorkflowRunStartResponse, Self::Error> {
        let result = match self.state.require_workflow_store() {
            Ok(()) => Box::pin(start_authored(self.client_id, self.state, request, true)).await,
            Err(error) => Err(error),
        };
        result.map_err(|error| {
            let failure = super::request_error_response(&error);
            bcode_workflow::WorkflowRunAdmissionFailure {
                code: failure.code,
                message: failure.message,
            }
        })
    }
}

impl bcode_workflow::WorkflowAuthoringApplication for WorkflowAuthoringApplication<'_> {
    async fn publish_and_start_workflow(
        &self,
        request: bcode_workflow::PublishAndStartWorkflowRequest,
    ) -> Result<bcode_workflow::WorkflowPublishAndStartResult, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        Box::pin(publish_and_start(
            String::new(),
            self.client_id,
            self.state,
            request,
        ))
        .await
        .map_err(authoring_failure)
    }
    async fn publish_workflow_draft(
        &self,
        request: bcode_workflow::PublishWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowPublicationResult, Self::Error> {
        self.state
            .require_workflow_store()
            .map_err(authoring_failure)?;
        publish_draft(
            String::new(),
            self.client_id,
            self.state,
            request,
            bcode_workflow::WorkflowApplicationOperation::PublishDraft,
        )
        .await
        .map_err(authoring_failure)
    }
    type Error = bcode_workflow::WorkflowAuthoringFailure;

    async fn apply_workflow_draft_edits(
        &self,
        request: bcode_workflow::ApplyWorkflowDraftEditsRequest,
    ) -> Result<bcode_workflow::WorkflowDraftEditResult, Self::Error> {
        self.apply_draft_edits(request)
    }

    async fn update_workflow_draft(
        &self,
        request: bcode_workflow::UpdateWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowDraftUpdateResult, Self::Error> {
        self.update_draft(&request)
    }

    async fn activate_workflow_revision(
        &self,
        request: bcode_workflow::ActivateWorkflowRevisionRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, Self::Error> {
        self.activate_revision(&request)
    }

    async fn discard_workflow_draft(
        &self,
        request: bcode_workflow::DiscardWorkflowDraftRequest,
    ) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, Self::Error> {
        self.discard_draft(&request)
    }
}

#[cfg(test)]
mod authoring_boundary_tests {
    #[test]
    fn failures_hide_host_details_and_preserve_authorization_code() {
        let denied = super::authoring_failure(
            super::super::ServerError::WorkflowApplicationOperationUnauthorized(
                "secret-marker".into(),
            ),
        );
        assert_eq!(
            denied,
            bcode_workflow::WorkflowAuthoringFailure::Unauthorized
        );
        let response = super::super::request_error_response(&denied.into());
        assert_eq!(response.code, "workflow_operation_unauthorized");
        assert!(!response.message.contains("secret-marker"));
        let invalid = super::authoring_failure(super::super::ServerError::WorkflowStore(
            bcode_workflow_store::WorkflowStoreError::InvalidData("secret-marker".into()),
        ));
        assert_eq!(
            invalid,
            bcode_workflow::WorkflowAuthoringFailure::StateUnavailable
        );
        assert!(!invalid.to_string().contains("secret-marker"));
    }
}

fn authoring_failure(error: super::ServerError) -> bcode_workflow::WorkflowAuthoringFailure {
    use super::ServerError;
    use bcode_workflow::WorkflowAuthoringFailure as Failure;
    match error {
        ServerError::WorkflowComputationTimedOut(_) => Failure::TimedOut,
        ServerError::WorkflowComputationCancelled(_) => Failure::Cancelled,
        ServerError::WorkflowComputationControlInvalid(_) => Failure::InvalidControl,
        ServerError::Workflow(_) => Failure::InvalidContract,
        ServerError::WorkflowDefinitionUnsupported(_) => Failure::UnsupportedDefinition,
        ServerError::WorkflowCapabilityUnavailable(_)
        | ServerError::WorkflowStorageUnavailable(_) => Failure::CapabilityUnavailable,
        ServerError::WorkflowApplicationOperationUnauthorized(_) => Failure::Unauthorized,
        ServerError::WorkflowStore(error) => match error {
            bcode_workflow_store::WorkflowStoreError::AuthoringConflict { .. } => Failure::Conflict,
            bcode_workflow_store::WorkflowStoreError::UpgradeOwnershipUnavailable => {
                Failure::UpgradeBlocked
            }
            bcode_workflow_store::WorkflowStoreError::UnsupportedStore { .. } => {
                Failure::MaintenanceRequired
            }
            _ => Failure::StateUnavailable,
        },
        _ => Failure::Failed,
    }
}

#[derive(Clone)]
pub struct WorkflowApplicationAuthorizationPolicy {
    pub evaluator: std::sync::Arc<
        dyn Fn(
                &bcode_workflow::WorkflowApplicationOperationFacts,
            ) -> WorkflowApplicationAuthorizationDecision
            + Send
            + Sync,
    >,
}

impl std::fmt::Debug for WorkflowApplicationAuthorizationPolicy {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("WorkflowApplicationAuthorizationPolicy")
            .finish_non_exhaustive()
    }
}

impl WorkflowApplicationAuthorizationPolicy {
    pub fn evaluate(
        &self,
        facts: &bcode_workflow::WorkflowApplicationOperationFacts,
    ) -> WorkflowApplicationAuthorizationDecision {
        (self.evaluator)(facts)
    }
}

/// Decision returned by the daemon application-operation authorization boundary.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowApplicationAuthorizationDecision {
    /// The normalized operation may proceed to its side-effect boundary.
    Allow,
    /// The normalized operation is denied with a secret-safe reason.
    Deny { reason: String },
}

pub fn authorize_local_workflow_application_operation(
    facts: &bcode_workflow::WorkflowApplicationOperationFacts,
) -> WorkflowApplicationAuthorizationDecision {
    match facts.actor.kind {
        bcode_workflow::WorkflowApplicationActorKind::LocalClient => {
            WorkflowApplicationAuthorizationDecision::Allow
        }
        bcode_workflow::WorkflowApplicationActorKind::Plugin => {
            WorkflowApplicationAuthorizationDecision::Deny {
                reason: "plugins require an explicitly registered authored-workflow application capability"
                    .to_string(),
            }
        }
        bcode_workflow::WorkflowApplicationActorKind::Service => {
            WorkflowApplicationAuthorizationDecision::Deny {
                reason: "services require an explicitly configured authored-workflow maintenance policy"
                    .to_string(),
            }
        }
    }
}

/// Apply explicit startup plugin grants in addition to local-client admission.
/// Execution relationship and durable authority are verified separately before staging.
/// Authorize publication for explicitly granted authenticated plugin or local client identities.
/// Staging grants and local-client staging privileges never grant publication.
pub fn authorize_configured_run_graph_publication(
    facts: &bcode_workflow::WorkflowRunGraphPublicationFacts,
    plugins: &std::collections::BTreeSet<String>,
    local_clients: bool,
) -> WorkflowApplicationAuthorizationDecision {
    let granted = match facts.actor.kind {
        bcode_workflow::WorkflowApplicationActorKind::LocalClient => local_clients,
        bcode_workflow::WorkflowApplicationActorKind::Plugin => {
            plugins.contains(&facts.actor.actor_id)
        }
        bcode_workflow::WorkflowApplicationActorKind::Service => false,
    };
    if facts.validate().is_ok() && granted {
        WorkflowApplicationAuthorizationDecision::Allow
    } else {
        WorkflowApplicationAuthorizationDecision::Deny {
            reason: "workflow publication requires an explicit publication grant".to_owned(),
        }
    }
}

pub fn authorize_configured_run_graph_edit(
    facts: &bcode_workflow::WorkflowRunGraphEditFacts,
    plugins: &std::collections::BTreeSet<String>,
) -> WorkflowApplicationAuthorizationDecision {
    if facts.validate().is_err() {
        return WorkflowApplicationAuthorizationDecision::Deny {
            reason: "invalid run graph edit facts".to_owned(),
        };
    }
    if facts.actor.kind == bcode_workflow::WorkflowApplicationActorKind::Plugin
        && plugins.contains(&facts.actor.actor_id)
    {
        WorkflowApplicationAuthorizationDecision::Allow
    } else {
        authorize_local_run_graph_edit(facts)
    }
}

/// Authorize candidate persistence for authenticated local clients only.
///
/// This follows local authored-workflow admission policy but does not authorize publication or
/// execution. Plugin and service actors require their own explicitly configured capabilities.
pub fn authorize_local_run_graph_edit(
    facts: &bcode_workflow::WorkflowRunGraphEditFacts,
) -> WorkflowApplicationAuthorizationDecision {
    if facts.actor.kind == bcode_workflow::WorkflowApplicationActorKind::LocalClient {
        WorkflowApplicationAuthorizationDecision::Allow
    } else {
        WorkflowApplicationAuthorizationDecision::Deny {
            reason: "run graph staging requires an authorized local application client".to_string(),
        }
    }
}

/// Authorize and stage a run edit without publishing executable topology.
///
/// Policy is configured on the application host, never decoded from a client request. The actor
/// is derived from the accepted connection identity. Policy runs before ownership acquisition,
/// which can itself transfer durable authority, as well as before candidate persistence.
///
/// # Errors
///
/// Returns an error for invalid facts, policy denial, unavailable workflow storage, missing or
/// foreign execution authority, conflicting revisions/duplicates, or candidate persistence failure.
pub async fn stage_run_graph_edit(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<bool, super::ServerError> {
    let facts = bcode_workflow::WorkflowRunGraphEditFacts {
        version: bcode_workflow::WORKFLOW_RUN_GRAPH_EDIT_FACTS_VERSION,
        actor: bcode_workflow::WorkflowApplicationActor {
            kind: bcode_workflow::WorkflowApplicationActorKind::LocalClient,
            actor_id: client_id.to_string(),
        },
        request,
    };
    facts.validate().map_err(|error| {
        super::ServerError::WorkflowApplicationOperationUnauthorized(error.to_string())
    })?;
    let policy = state
        .workflow_run_graph_edit_policy
        .as_ref()
        .ok_or_else(|| {
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "run graph edit policy is not configured".to_string(),
            )
        })?;
    if let WorkflowApplicationAuthorizationDecision::Deny { reason } = (policy.evaluator)(&facts) {
        return Err(super::ServerError::WorkflowApplicationOperationUnauthorized(reason));
    }
    state.require_workflow_store()?;
    let guard = execution_authority(state, &facts.request.run_id)
        .await?
        .ok_or_else(|| {
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "run edit requires durable execution authority".to_string(),
            )
        })?;
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .stage_run_graph_edit(&facts.request, &guard.authority, super::current_time_ms())
        .map_err(Into::into)
}

/// Require plugin-owned policy approval in addition to the host publication grant.
pub async fn authorize_publication_plugin(
    state: &ServerState,
    facts: &bcode_workflow::WorkflowRunGraphPublicationFacts,
) -> Result<(), super::ServerError> {
    let decision = state
        .plugins
        .invoke_service_by_interface_json::<_, bcode_workflow::WorkflowPublicationPolicyDecision>(
            bcode_workflow::WORKFLOW_PUBLICATION_POLICY_INTERFACE_ID,
            bcode_workflow::OP_AUTHORIZE_WORKFLOW_PUBLICATION,
            facts,
        )
        .await
        .map_err(|_| {
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "workflow publication policy service unavailable or incompatible".to_owned(),
            )
        })?;
    match decision {
        bcode_workflow::WorkflowPublicationPolicyDecision::Allow => Ok(()),
        bcode_workflow::WorkflowPublicationPolicyDecision::Deny => Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "workflow publication denied by policy service".to_owned(),
            ),
        ),
    }
}

/// Authorize executable publication separately from staging.
///
/// # Errors
/// Returns an error for policy denial, invalid facts, unavailable authority, or store rejection.
pub async fn publish_run_graph_edit(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<u64, super::ServerError> {
    let facts = bcode_workflow::WorkflowRunGraphPublicationFacts {
        version: 1,
        actor: bcode_workflow::WorkflowApplicationActor {
            kind: bcode_workflow::WorkflowApplicationActorKind::LocalClient,
            actor_id: client_id.to_string(),
        },
        request,
    };
    facts.validate().map_err(|error| {
        super::ServerError::WorkflowApplicationOperationUnauthorized(error.to_string())
    })?;
    let policy = state
        .workflow_run_graph_publication_policy
        .as_ref()
        .ok_or_else(|| {
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "run graph publication policy is not configured".to_string(),
            )
        })?;
    if let WorkflowApplicationAuthorizationDecision::Deny { reason } = (policy.evaluator)(&facts) {
        return Err(super::ServerError::WorkflowApplicationOperationUnauthorized(reason));
    }
    authorize_publication_plugin(state, &facts).await?;
    state.require_workflow_store()?;
    let guard = execution_authority(state, &facts.request.run_id)
        .await?
        .ok_or_else(|| {
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "publication requires durable execution authority".to_string(),
            )
        })?;
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let staged = store.staged_run_graph_edit(
        &facts.request.run_id,
        &facts.request.mutation_id,
        &guard.authority,
    )?;
    if staged.as_ref() != Some(&facts.request) {
        return Err(
            super::ServerError::WorkflowApplicationOperationUnauthorized(
                "publication candidate does not match authorized facts".to_string(),
            ),
        );
    }
    store
        .publish_retained_leaf_run_graph_edit(
            &facts.request.run_id,
            &facts.request.mutation_id,
            &guard.authority,
            super::current_time_ms(),
        )
        .map_err(Into::into)
}

/// Server-owned input used to derive authenticated local-client operation facts.
pub struct LocalApplicationOperationRequest {
    pub operation: bcode_workflow::WorkflowApplicationOperation,
    pub workflow_id: String,
    pub draft_id: Option<String>,
    pub revision: Option<u64>,
    pub preset_id: Option<String>,
    pub producer: Option<bcode_workflow::WorkflowProducerProvenance>,
    pub requirements: bcode_workflow::WorkflowRequirementSummary,
    pub effects: bcode_workflow::WorkflowEffectSummary,
    pub activates: bool,
    pub executes: bool,
}

use super::{ClientEventSink, ServerState};
use std::collections::BTreeMap;

/// Resolve and start one exact published package export without renderer coupling.
///
/// # Errors
///
/// Returns an error for malformed selection, missing or drifted publication facts, an unpublished
/// export, or authored-workflow admission failure.
pub async fn start_package_export(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::StartWorkflowPackageExportRequest,
) -> Result<bcode_workflow::WorkflowPackageExportRunStartResponse, super::ServerError> {
    request.package_export.validate()?;
    let receipt = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_package_publication(
            &request.package_export.package_id,
            request.package_export.package_lock_digest_sha256.as_deref(),
        )?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow package not found: {}",
                request.package_export.package_id
            ))
        })?;
    let exported = receipt
        .exports
        .iter()
        .find(|export| export.export == request.package_export.export)
        .cloned()
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow package export not found: {}/{}",
                request.package_export.package_id, request.package_export.export
            ))
        })?;
    let revision = exported.published_revision.as_ref().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "published workflow package export has no exact revision".to_string(),
        )
    })?;
    let started = start_authored(
        client_id,
        state,
        bcode_workflow::StartAuthoredWorkflowRequest {
            selection: bcode_workflow::AuthoredWorkflowRunSelection::Revision {
                workflow_id: revision.workflow_id.clone(),
                revision: revision.revision,
            },
            run_id: request.run_id,
            parent_session_id: request.parent_session_id,
            workspace_snapshot: request.workspace_snapshot,
            parent_session_generation: request.parent_session_generation,
            configuration: request.configuration,
            input: request.input,
        },
        true,
    )
    .await?;
    Ok(bcode_workflow::WorkflowPackageExportRunStartResponse {
        package_export: request.package_export,
        package_lock_digest_sha256: receipt.package_lock_digest_sha256,
        exported,
        started,
    })
}

#[derive(Debug)]
pub struct ComputationCancellation {
    cancelled: std::sync::atomic::AtomicBool,
    notify: tokio::sync::Notify,
}

impl ComputationCancellation {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
            notify: tokio::sync::Notify::new(),
        }
    }

    pub fn cancel(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        self.notify.notify_waiters();
    }
}

pub fn record_authoring_duration(
    metrics: &bcode_metrics::MetricsRegistry,
    metric: &'static str,
    started_at: std::time::Instant,
    outcome: &'static str,
) {
    metrics.record_histogram_with_exact_labels(
        metric,
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        BTreeMap::from([("outcome".to_string(), outcome.to_string())]),
    );
}

pub fn record_authoring_conflict(
    metrics: &bcode_metrics::MetricsRegistry,
    operation: &'static str,
) {
    metrics.add_counter_with_exact_labels(
        "workflow.authoring.conflicts_total",
        1,
        BTreeMap::from([("operation".to_string(), operation.to_string())]),
    );
}

pub async fn run_computation<T, F>(
    state: &ServerState,
    control: bcode_workflow::WorkflowComputationControl,
    fallback_operation_id: String,
    compute: F,
) -> Result<T, super::ServerError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let operation_id = if control.operation_id.is_empty() {
        fallback_operation_id
    } else {
        control.operation_id
    };
    if operation_id.len() > 512
        || operation_id.is_empty()
        || control.timeout_ms == 0
        || control.timeout_ms > bcode_workflow::MAX_WORKFLOW_COMPUTATION_TIMEOUT_MS
    {
        return Err(super::ServerError::WorkflowComputationControlInvalid(
            "operation identity or timeout is outside supported bounds".to_string(),
        ));
    }
    let cancellation = std::sync::Arc::new(ComputationCancellation::new());
    {
        let mut computations = state
            .workflow_computations
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if computations.contains_key(&operation_id) {
            return Err(super::ServerError::WorkflowComputationControlInvalid(
                format!("workflow computation operation is already active: {operation_id}"),
            ));
        }
        computations.insert(operation_id.clone(), std::sync::Arc::clone(&cancellation));
    }
    let mut task = tokio::task::spawn_blocking(compute);
    let outcome = tokio::select! {
        result = &mut task => result.map_err(super::ServerError::BlockingTask),
        () = cancellation.notify.notified() => {
            Err(super::ServerError::WorkflowComputationCancelled(operation_id.clone()))
        }
        () = tokio::time::sleep(std::time::Duration::from_millis(control.timeout_ms)) => {
            cancellation.cancel();
            Err(super::ServerError::WorkflowComputationTimedOut(operation_id.clone()))
        }
    };
    state
        .workflow_computations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&operation_id);
    outcome
}

pub fn cancel_computation(state: &ServerState, operation_id: &str) -> bool {
    let cancellation = state
        .workflow_computations
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(operation_id)
        .cloned();
    cancellation.is_some_and(|cancellation| {
        cancellation.cancel();
        true
    })
}

pub fn authoring_conflict_result(
    error: bcode_workflow_store::WorkflowStoreError,
) -> Result<bcode_workflow::WorkflowAuthoringConflict, bcode_workflow_store::WorkflowStoreError> {
    match error {
        bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        } => Ok(bcode_workflow::WorkflowAuthoringConflict {
            entity_id,
            expected_generation: expected,
            current_generation: current,
        }),
        error => Err(error),
    }
}

fn workflow_preset_from_mutation(
    mutation: bcode_workflow::WorkflowPresetMutation,
    generation: u64,
    created_at_ms: u64,
) -> bcode_workflow_store::WorkflowPreset {
    bcode_workflow_store::WorkflowPreset {
        workflow_id: mutation.workflow_id,
        preset_id: mutation.preset_id,
        revision: mutation.revision,
        name: mutation.name,
        generation,
        configuration: mutation.configuration,
        run_limits: mutation.run_limits,
        producer: mutation.producer,
        created_at_ms,
        updated_at_ms: created_at_ms,
    }
}

pub async fn import_revision(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::ImportWorkflowRevisionRequest,
) -> Result<bcode_workflow::WorkflowRevisionImportResult, super::ServerError> {
    if request.collision_policy
        != bcode_workflow::WorkflowImportCollisionPolicy::RequireExistingWorkflowNextRevision
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "revision import requires require_existing_workflow_next_revision collision policy"
                .to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        fallback_operation_id,
        request.bundle,
        request.workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "revision import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportRevision,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(format!("import-revision-{}", request.revision)),
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: request.activate,
            executes: false,
        },
    )?;
    let publication = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .import_workflow_revision(
            &request.workflow_id,
            request.revision,
            &document,
            &document.producer,
            &preview.compilation,
            request.activate,
            request.expected_active_revision,
            super::current_time_ms(),
        );
    let result = match publication {
        Ok(publication) => bcode_workflow::WorkflowRevisionImportResult::Imported {
            revision: Box::new(workflow_revision_snapshot(publication.revision)),
            active_revision: publication.active_revision,
        },
        Err(error @ bcode_workflow_store::WorkflowStoreError::AuthoringConflict { .. }) => {
            bcode_workflow::WorkflowRevisionImportResult::Conflict(authoring_conflict_result(
                error,
            )?)
        }
        Err(error) => return Err(error.into()),
    };
    Ok(result)
}

pub async fn publish_draft(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::PublishWorkflowDraftRequest,
    operation: bcode_workflow::WorkflowApplicationOperation,
) -> Result<bcode_workflow::WorkflowPublicationResult, super::ServerError> {
    let draft = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_draft(&request.workflow_id, &request.draft_id)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow draft not found: {}/{}",
                request.workflow_id, request.draft_id
            ))
        })?;
    let catalog = authoring_catalog(state).await?;
    let configuration = request.configuration.clone();
    let preview = run_computation(state, request.control.clone(), fallback_operation_id, {
        let document = draft.document.clone();
        move || document.compilation_preview(&catalog, configuration.as_ref())
    })
    .await?;
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "draft publication requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(draft.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: request.activate,
            executes: operation == bcode_workflow::WorkflowApplicationOperation::PublishAndStart,
        },
    )?;
    let publication_started_at = std::time::Instant::now();
    let publication = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .publish_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
            &preview,
            request.activate,
            request.expected_active_revision,
            super::current_time_ms(),
        );
    let (result, outcome) = match publication {
        Ok(publication) => (
            bcode_workflow::WorkflowPublicationResult::Published {
                revision: Box::new(workflow_revision_snapshot(publication.revision)),
                active_revision: publication.active_revision,
            },
            "published",
        ),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "publish");
            (
                bcode_workflow::WorkflowPublicationResult::Conflict(authoring_conflict_result(
                    error,
                )?),
                "conflict",
            )
        }
    };
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.publication.duration_ms",
        publication_started_at,
        outcome,
    );
    Ok(result)
}

pub async fn publish_and_start(
    fallback_operation_id: String,
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::PublishAndStartWorkflowRequest,
) -> Result<bcode_workflow::WorkflowPublishAndStartResult, super::ServerError> {
    let bcode_workflow::PublishAndStartWorkflowRequest {
        publication,
        run_id,
        parent_session_id,
        workspace_snapshot,
    } = request;
    let configuration = publication.configuration.clone();
    let workflow_id = publication.workflow_id.clone();
    let publication = publish_draft(
        fallback_operation_id,
        client_id,
        state,
        publication,
        bcode_workflow::WorkflowApplicationOperation::PublishAndStart,
    )
    .await?;
    let result = match publication {
        bcode_workflow::WorkflowPublicationResult::Conflict(conflict) => {
            bcode_workflow::WorkflowPublishAndStartResult::PublicationConflict(conflict)
        }
        bcode_workflow::WorkflowPublicationResult::Published {
            revision,
            active_revision,
        } => {
            let run_admission = match start_authored(
                client_id,
                state,
                bcode_workflow::StartAuthoredWorkflowRequest {
                    selection: bcode_workflow::AuthoredWorkflowRunSelection::Revision {
                        workflow_id,
                        revision: revision.identity.revision,
                    },
                    run_id,
                    parent_session_id,
                    workspace_snapshot,
                    parent_session_generation: None,
                    configuration,
                    input: None,
                },
                false,
            )
            .await
            {
                Ok(started) => {
                    bcode_workflow::WorkflowRunAdmissionResult::Started(Box::new(started))
                }
                Err(error) => {
                    let failure = super::request_error_response(&error);
                    bcode_workflow::WorkflowRunAdmissionResult::Failed(
                        bcode_workflow::WorkflowRunAdmissionFailure {
                            code: failure.code,
                            message: failure.message,
                        },
                    )
                }
            };
            bcode_workflow::WorkflowPublishAndStartResult::Published {
                revision,
                active_revision,
                run_admission,
            }
        }
    };
    Ok(result)
}

pub fn resolve_authored_run(
    state: &std::sync::Arc<ServerState>,
    selection: &bcode_workflow::AuthoredWorkflowRunSelection,
) -> Result<
    (
        String,
        u64,
        bcode_workflow_store::PublishedWorkflowRevision,
        Option<bcode_workflow_store::WorkflowPreset>,
    ),
    super::ServerError,
> {
    let (workflow_id, revision_number, preset) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        match selection {
            bcode_workflow::AuthoredWorkflowRunSelection::Revision {
                workflow_id,
                revision,
            } => {
                drop(store);
                (workflow_id.clone(), *revision, None)
            }
            bcode_workflow::AuthoredWorkflowRunSelection::Active { workflow_id } => {
                let workflow = store.authored_workflow(workflow_id)?.ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "authored workflow not found: {workflow_id}"
                    ))
                })?;
                let revision = workflow.active_revision.ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "authored workflow has no active revision: {workflow_id}"
                    ))
                })?;
                drop(store);
                (workflow_id.clone(), revision, None)
            }
            bcode_workflow::AuthoredWorkflowRunSelection::Preset {
                workflow_id,
                preset_id,
                preset_generation,
            } => {
                let preset = store
                    .workflow_preset(workflow_id, preset_id)?
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                            "workflow preset not found: {workflow_id}/{preset_id}"
                        ))
                    })?;
                if preset.generation != *preset_generation {
                    return Err(
                        bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
                            entity_id: format!("{workflow_id}/{preset_id}"),
                            expected: *preset_generation,
                            current: preset.generation,
                        }
                        .into(),
                    );
                }
                drop(store);
                (workflow_id.clone(), preset.revision, Some(preset))
            }
        }
    };
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(&workflow_id, revision_number)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow revision not found: {workflow_id} v{revision_number}"
            ))
        })?;
    Ok((workflow_id, revision_number, revision, preset))
}

#[allow(clippy::too_many_lines, clippy::significant_drop_tightening)]
pub async fn start_authored(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::StartAuthoredWorkflowRequest,
    authorize: bool,
) -> Result<bcode_workflow::AuthoredWorkflowRunStartResponse, super::ServerError> {
    let resolution_started_at = std::time::Instant::now();
    let (workflow_id, revision_number, revision, preset) =
        resolve_authored_run(state, &request.selection)?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.start_resolution.duration_ms",
        resolution_started_at,
        match &request.selection {
            bcode_workflow::AuthoredWorkflowRunSelection::Revision { .. } => "revision",
            bcode_workflow::AuthoredWorkflowRunSelection::Active { .. } => "active",
            bcode_workflow::AuthoredWorkflowRunSelection::Preset { .. } => "preset",
        },
    );
    let configuration = preset
        .as_ref()
        .map(|preset| preset.configuration.clone())
        .or(request.configuration)
        .or_else(|| revision.document.configuration_defaults.clone())
        .unwrap_or_else(|| serde_json::json!({}));
    let catalog = authoring_catalog(state).await?;
    let preview = revision
        .document
        .compilation_preview(&catalog, Some(&configuration));
    let compiled = preview.compiled.ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "published authored workflow is no longer executable on this host".to_string(),
        )
    })?;
    if compiled.definition_identity != revision.definition_identity {
        return Err(super::ServerError::WorkflowDefinitionUnsupported(
            "runtime configuration changes immutable compiled definition identity".to_string(),
        ));
    }
    if authorize {
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: match request.selection {
                    bcode_workflow::AuthoredWorkflowRunSelection::Revision { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartRevision
                    }
                    bcode_workflow::AuthoredWorkflowRunSelection::Active { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartActiveRevision
                    }
                    bcode_workflow::AuthoredWorkflowRunSelection::Preset { .. } => {
                        bcode_workflow::WorkflowApplicationOperation::StartPreset
                    }
                },
                workflow_id: workflow_id.clone(),
                draft_id: None,
                revision: matches!(
                    request.selection,
                    bcode_workflow::AuthoredWorkflowRunSelection::Revision { .. }
                )
                .then_some(revision_number),
                preset_id: preset.as_ref().map(|preset| preset.preset_id.clone()),
                producer: Some(revision.producer.clone()),
                requirements: compiled.requirements.clone(),
                effects: compiled.effects.clone(),
                activates: false,
                executes: true,
            },
        )?;
    }
    let limits = preset
        .as_ref()
        .and_then(|preset| preset.run_limits.as_ref())
        .unwrap_or(&compiled.run_limits);
    let deadline_at_ms = limits
        .maximum_duration_ms
        .map(|duration| super::current_time_ms().saturating_add(duration));
    let provenance = bcode_workflow_store::AuthoredWorkflowRunProvenance::new(
        workflow_id.clone(),
        revision_number,
        revision.definition_identity.clone(),
        preset.as_ref().map(|preset| preset.preset_id.clone()),
        preset.as_ref().map(|preset| preset.generation),
        configuration.clone(),
    );
    let run_input = request
        .input
        .unwrap_or_else(|| compiled.input_defaults.clone());
    jsonschema::validator_for(&compiled.definition.input.schema)
        .map_err(|error| super::ServerError::WorkflowDefinitionUnsupported(error.to_string()))?
        .validate(&run_input)
        .map_err(|error| {
            super::ServerError::WorkflowDefinitionUnsupported(format!(
                "workflow run input does not match published interface: {error}"
            ))
        })?;
    let started = start_run(
        state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: revision.definition_identity.definition_id.clone(),
            definition_version: revision.definition_identity.definition_version,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot.unwrap_or_default(),
            parent_session_id: request.parent_session_id,
            parent_session_generation: request.parent_session_generation,
            binding: Some(bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: "bcode.authored-workflow".to_string(),
                workflow_kind: workflow_id.clone(),
                scope_key: format!("revision/{revision_number}"),
                display_label: Some(revision.document.metadata.title.clone()),
                single_active: false,
            }),
            input: Some(run_input),
            limits: bcode_workflow_store::WorkflowRunLimits {
                deadline_at_ms,
                node_execution_cap: limits.node_execution_cap,
                concurrency_cap: limits.concurrency_cap,
                cycle_cap: limits.cycle_cap,
                retry_cap: limits.retry_cap,
            },
        },
        Some(provenance),
    )
    .await?;
    Ok(bcode_workflow::AuthoredWorkflowRunStartResponse {
        started,
        workflow_id,
        revision: revision_number,
        definition_identity: revision.definition_identity,
        preset_id: preset.as_ref().map(|preset| preset.preset_id.clone()),
        preset_generation: preset.as_ref().map(|preset| preset.generation),
        configuration,
    })
}

const WORKFLOW_RUN_VIEW_COLLECTION_LIMIT_MAX: usize = 1_000;

#[must_use]
pub fn run_view_collection_limit(requested: usize) -> usize {
    requested.min(WORKFLOW_RUN_VIEW_COLLECTION_LIMIT_MAX)
}

#[allow(clippy::too_many_lines)]
pub fn run_view(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow_view_models::WorkflowRunView, super::ServerError> {
    let limit = run_view_collection_limit(limit);
    let view = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run = store.run_summary(run_id)?.ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow run not found: {run_id}"
            ))
        })?;
        let definition = store
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition not found: {} v{}",
                    run.definition_id, run.definition_version
                ))
            })?;
        let outputs = store.output_summaries(run_id, limit)?;
        let output_values = store
            .validated_outputs(run_id, limit)?
            .into_iter()
            .map(|output| (output.output_id, output.value))
            .collect::<BTreeMap<_, _>>();
        let activations = store.activations_for_run(run_id, limit)?;
        let waits = store.waiting_activations(run_id, limit)?;
        let mutation_approvals = store.pending_mutation_approvals(run_id, limit)?;
        let attempts = store.attempt_history(run_id, None, limit)?;
        let retry_schedules = attempts
            .iter()
            .map(|attempt| {
                store.automatic_retry_schedule(run_id, &attempt.node_id, &attempt.activation_id)
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?
            .into_iter()
            .flatten()
            .map(|schedule| {
                (
                    (schedule.node_id.clone(), schedule.activation_id.clone()),
                    schedule,
                )
            })
            .collect::<BTreeMap<_, _>>();
        let failure_events = store
            .failure_history(run_id, limit)?
            .into_iter()
            .map(|event| verified_history_event(&store, event))
            .collect::<Result<Vec<_>, _>>()?;
        let descendant_runs = store.descendant_run_summaries(run_id, limit)?;
        let child_sessions = store.execution_session_links_for_run(run_id, limit)?;
        let run_item = run_list_item(&store, &run)?;
        let parsed_definition: bcode_workflow::WorkflowDefinition =
            serde_json::from_str(&definition.definition_json).map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition is not valid current-format JSON: {error}"
                ))
            })?;
        let wait_views = waits
            .into_iter()
            .map(|wait| {
                let node = parsed_definition.node(&wait.node_id).ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                        "workflow wait references missing node: {}",
                        wait.node_id
                    ))
                })?;
                let kind = match wait.kind {
                    bcode_workflow::WorkflowWaitKind::Input => {
                        bcode_workflow_view_models::WorkflowWaitKind::Input
                    }
                    bcode_workflow::WorkflowWaitKind::Approval => {
                        bcode_workflow_view_models::WorkflowWaitKind::Approval
                    }
                };
                Ok(bcode_workflow_view_models::WorkflowWaitView {
                    node_id: wait.node_id,
                    activation_id: wait.activation_id,
                    kind,
                    prompt: node.name.clone(),
                    expected_schema: (kind == bcode_workflow_view_models::WorkflowWaitKind::Input)
                        .then(|| node.output.schema.clone()),
                    input: wait.input,
                    requested_at_ms: wait.requested_at_ms,
                })
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?;
        let mutation_approval_views = mutation_approvals
            .into_iter()
            .map(|approval| {
                let warning = match approval.scope.reconciliation {
                    bcode_workflow::WorkflowBlockReconciliation::IdempotentReplay => None,
                    bcode_workflow::WorkflowBlockReconciliation::ReceiptStatus => Some(
                        "Execution is reconciled through an owner receipt after restart."
                            .to_string(),
                    ),
                    bcode_workflow::WorkflowBlockReconciliation::RepairRequired => Some(
                        "An ambiguous accepted execution requires explicit repair.".to_string(),
                    ),
                };
                bcode_workflow_view_models::WorkflowMutationApprovalView {
                    approval_id: approval.approval_id,
                    node_id: approval.node_id,
                    activation_id: approval.activation_id,
                    plugin_id: approval.scope.plugin_id,
                    block_id: approval.scope.block_id,
                    block_version: approval.scope.block_version,
                    operation: approval.scope.operation,
                    effect: bcode_workflow_view_models::WorkflowOperationEffect::Mutating,
                    input_summary: approval.scope.input_summary,
                    resource_claims: approval
                        .scope
                        .resource_claims
                        .into_iter()
                        .map(
                            |claim| bcode_workflow_view_models::WorkflowResourceClaimView {
                                resource: claim.resource,
                                access: match claim.access {
                                    bcode_workflow::ResourceAccess::Read => "read".to_string(),
                                    bcode_workflow::ResourceAccess::Write => "write".to_string(),
                                },
                            },
                        )
                        .collect(),
                    workspace_snapshot: approval.scope.workspace_snapshot,
                    reconciliation_warning: warning,
                    requested_at_ms: approval.requested_at_ms,
                    expires_at_ms: approval.expires_at_ms,
                }
            })
            .collect();
        let descendant_views = descendant_runs
            .into_iter()
            .map(|descendant| {
                Ok(bcode_workflow_view_models::WorkflowDescendantRunView {
                    run: run_list_item(&store, &descendant.run)?,
                    parent_run_id: descendant.link.parent_run_id,
                    parent_node_id: descendant.link.parent_node_id,
                    depth: descendant.link.depth,
                })
            })
            .collect::<Result<Vec<_>, bcode_workflow_store::WorkflowStoreError>>()?;
        drop(store);
        bcode_workflow_view::project_run(bcode_workflow_view::WorkflowRunProjectionInput {
            run: run_item,
            terminal_output_id: run.terminal_output_id,
            definition: bcode_workflow_view::WorkflowDefinitionProjectionInput {
                definition_json: definition.definition_json,
            },
            activations: activations
                .into_iter()
                .map(
                    |activation| bcode_workflow_view::WorkflowActivationProjectionInput {
                        node_id: activation.node_id,
                        activation_id: activation.activation_id,
                        dependency_generation: activation.dependency_generation,
                        status: activation.status,
                        has_output: activation.has_output,
                        input_summary: match activation.input_summary {
                            bcode_workflow::WorkflowActivationInputSummary::Absent => {
                                bcode_workflow_view_models::WorkflowInputSummary::Absent
                            }
                            bcode_workflow::WorkflowActivationInputSummary::Inline {
                                value,
                            } => bcode_workflow_view_models::WorkflowInputSummary::Inline { value },
                            bcode_workflow::WorkflowActivationInputSummary::Omitted {
                                byte_count,
                            } => bcode_workflow_view_models::WorkflowInputSummary::Omitted {
                                byte_count,
                            },
                        },
                        created_at_ms: activation.created_at_ms,
                    },
                )
                .collect(),
            waits: wait_views,
            mutation_approvals: mutation_approval_views,
            attempts: attempts
                .into_iter()
                .map(|attempt| bcode_workflow_view_models::WorkflowAttemptView {
                    node_id: attempt.node_id,
                    activation_id: attempt.activation_id,
                    attempt: attempt.attempt,
                    dispatch_identity: attempt.dispatch_identity,
                    status: attempt.status,
                    has_receipt: attempt.has_receipt,
                    prepared_at_ms: attempt.prepared_at_ms,
                    admitted_at_ms: attempt.admitted_at_ms,
                    terminal_at_ms: attempt.terminal_at_ms,
                })
                .collect(),
            retry_schedules: retry_schedules
                .into_values()
                .map(|schedule| bcode_workflow_view_models::WorkflowRetryScheduleView {
                    node_id: schedule.node_id,
                    activation_id: schedule.activation_id,
                    failed_attempt: schedule.failed_attempt,
                    next_attempt: schedule.next_attempt,
                    failure_kind: match schedule.failure_kind {
                        bcode_workflow::AutomaticRetryFailureKind::OwnerUnavailableBeforeAcceptance => bcode_workflow_view_models::WorkflowRetryFailureKind::OwnerUnavailableBeforeAcceptance,
                        bcode_workflow::AutomaticRetryFailureKind::OwnerReportedRetryable => bcode_workflow_view_models::WorkflowRetryFailureKind::OwnerReportedRetryable,
                        bcode_workflow::AutomaticRetryFailureKind::Cancellation => bcode_workflow_view_models::WorkflowRetryFailureKind::Cancellation,
                        bcode_workflow::AutomaticRetryFailureKind::TerminalTimeout => bcode_workflow_view_models::WorkflowRetryFailureKind::TerminalTimeout,
                        bcode_workflow::AutomaticRetryFailureKind::ApprovalDenied => bcode_workflow_view_models::WorkflowRetryFailureKind::ApprovalDenied,
                        bcode_workflow::AutomaticRetryFailureKind::SchemaFailure => bcode_workflow_view_models::WorkflowRetryFailureKind::SchemaFailure,
                        bcode_workflow::AutomaticRetryFailureKind::AmbiguousMutation => bcode_workflow_view_models::WorkflowRetryFailureKind::AmbiguousMutation,
                        bcode_workflow::AutomaticRetryFailureKind::TerminalFailure => bcode_workflow_view_models::WorkflowRetryFailureKind::TerminalFailure,
                    },
                    backoff_ms: schedule.backoff_ms,
                    next_attempt_at_ms: schedule.next_attempt_at_ms,
                    scheduled_at_ms: schedule.scheduled_at_ms,
                })
                .collect(),
            outputs: outputs
                .into_iter()
                .map(
                    |output| bcode_workflow_view::WorkflowOutputProjectionInput {
                        value: output_values.get(&output.output_id).cloned(),
                        output_id: output.output_id,
                        node_id: output.node_id,
                        activation_id: output.activation_id,
                        schema_id: output.schema_id,
                        schema_version: output.schema_version,
                        checksum_sha256: output.checksum_sha256,
                        artifact_reference: output.artifact_reference,
                        created_at_ms: output.created_at_ms,
                    },
                )
                .collect(),
            failure_events: failure_events
                .into_iter()
                .map(
                    |event| bcode_workflow_view::WorkflowFailureEventProjectionInput {
                        event_sequence: event.event_seq,
                        event_type: event.event_type,
                        payload: event.payload,
                        created_at_ms: event.created_at_ms,
                    },
                )
                .collect(),
            descendant_runs: descendant_views,
            child_sessions: child_sessions
                .into_iter()
                .map(
                    |link| bcode_workflow_view_models::WorkflowChildSessionView {
                        node_id: link.node_id,
                        activation_id: link.activation_id,
                        attempt: link.attempt,
                        session_id: link.session_id,
                    },
                )
                .collect(),
        })
    };
    Ok(view)
}

fn run_list_item(
    store: &bcode_workflow_store::WorkflowStore,
    run: &bcode_workflow_store::WorkflowRunSummary,
) -> Result<bcode_workflow_view_models::WorkflowRunListItem, bcode_workflow_store::WorkflowStoreError>
{
    let summary = store.run_catalog_summary(&run.run_id)?;
    run_list_item_with_summary(store, run, &summary)
}

fn run_list_item_with_summary(
    store: &bcode_workflow_store::WorkflowStore,
    run: &bcode_workflow_store::WorkflowRunSummary,
    summary: &bcode_workflow_store::WorkflowRunCatalogSummary,
) -> Result<bcode_workflow_view_models::WorkflowRunListItem, bcode_workflow_store::WorkflowStoreError>
{
    let authored_workflow = run
        .authored_provenance
        .as_ref()
        .map(|source| store.authored_workflow(&source.workflow_id))
        .transpose()?
        .flatten();
    let parent_run_id = store
        .parent_run_link(&run.run_id)?
        .map(|link| link.parent_run_id);
    let editable_draft_id = if let Some(source) = &run.authored_provenance {
        store
            .list_workflow_drafts(&source.workflow_id, 1)?
            .first()
            .map(|draft| draft.draft_id.clone())
    } else {
        None
    };
    let display_title = authored_workflow.as_ref().map_or_else(
        || {
            run.binding
                .as_ref()
                .and_then(|binding| binding.display_label.clone())
                .unwrap_or_else(|| run.definition_id.clone())
        },
        |workflow| workflow.title.clone(),
    );
    let definition_disposition = run.authored_provenance.as_ref().map_or(
        bcode_workflow_view_models::WorkflowDefinitionDisposition::CompiledOnly,
        |source| bcode_workflow_view_models::WorkflowDefinitionDisposition::Published {
            workflow_id: source.workflow_id.clone(),
            revision: source.revision,
            editable_draft_id,
        },
    );
    Ok(bcode_workflow_view_models::WorkflowRunListItem {
        run_id: run.run_id.clone(),
        display_title,
        binding_label: run
            .binding
            .as_ref()
            .and_then(|binding| binding.display_label.clone()),
        definition_id: run.definition_id.clone(),
        definition_version: run.definition_version,
        authored_source: run.authored_provenance.as_ref().map(|source| {
            bcode_workflow_view_models::WorkflowAuthoredSourceView {
                workflow_id: source.workflow_id.clone(),
                revision: source.revision,
            }
        }),
        definition_disposition,
        parent_run_id,
        descendant_count: summary.descendant_count,
        progress: bcode_workflow_view_models::WorkflowRunProgress {
            total_nodes: summary.total_nodes,
            not_started: summary.not_started,
            active: summary.active,
            blocked: summary.blocked,
            completed: summary.completed,
            failed: summary.failed,
            cancelled: summary.cancelled,
            skipped: summary.skipped,
            repair_required: summary.repair_required,
        },
        attention: bcode_workflow_view_models::WorkflowAttentionSummary {
            pending_inputs: summary.pending_inputs,
            pending_approvals: summary.pending_approvals,
            pending_mutation_approvals: summary.pending_mutation_approvals,
            retryable_failures: summary.retryable_failures,
            repair_required: run.status == bcode_workflow_store::RunStatus::RepairRequired
                || summary.repair_required > 0,
        },
        status: match run.status {
            bcode_workflow_store::RunStatus::Running => {
                bcode_workflow_view_models::WorkflowRunStatus::Running
            }
            bcode_workflow_store::RunStatus::Paused => {
                bcode_workflow_view_models::WorkflowRunStatus::Paused
            }
            bcode_workflow_store::RunStatus::Completed => {
                bcode_workflow_view_models::WorkflowRunStatus::Completed
            }
            bcode_workflow_store::RunStatus::Failed => {
                bcode_workflow_view_models::WorkflowRunStatus::Failed
            }
            bcode_workflow_store::RunStatus::Cancelled => {
                bcode_workflow_view_models::WorkflowRunStatus::Cancelled
            }
            bcode_workflow_store::RunStatus::RepairRequired => {
                bcode_workflow_view_models::WorkflowRunStatus::RepairRequired
            }
        },
        created_at_ms: run.created_at_ms,
        updated_at_ms: run.updated_at_ms,
    })
}

fn authored_workflow_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::AuthoredWorkflow>,
    _limit: usize,
) -> bcode_workflow::WorkflowAuthoringPage<
    bcode_workflow::AuthoredWorkflowSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: workflows,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = workflows
            .last()
            .expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.workflow_id.clone(),
        }
    });
    bcode_workflow::WorkflowAuthoringPage {
        items: workflows
            .into_iter()
            .map(authored_workflow_snapshot)
            .collect(),
        next_cursor,
    }
}

fn workflow_draft_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::WorkflowDraft>,
    _limit: usize,
) -> bcode_workflow::WorkflowAuthoringPage<
    bcode_workflow::WorkflowDraftSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: drafts,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = drafts.last().expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.draft_id.clone(),
        }
    });
    bcode_workflow::WorkflowAuthoringPage {
        items: drafts.into_iter().map(workflow_draft_snapshot).collect(),
        next_cursor,
    }
}

fn workflow_revision_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<
        bcode_workflow_store::PublishedWorkflowRevision,
    >,
    _limit: usize,
) -> bcode_workflow::WorkflowAuthoringPage<
    bcode_workflow::WorkflowRevisionSnapshot,
    bcode_workflow::WorkflowRevisionListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: revisions,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| bcode_workflow::WorkflowRevisionListCursor {
        revision: revisions
            .last()
            .expect("a page with more items is non-empty")
            .revision,
    });
    bcode_workflow::WorkflowAuthoringPage {
        items: revisions
            .into_iter()
            .map(workflow_revision_snapshot)
            .collect(),
        next_cursor,
    }
}

fn workflow_preset_page(
    page: bcode_workflow_store::WorkflowAuthoringStorePage<bcode_workflow_store::WorkflowPreset>,
    _limit: usize,
) -> bcode_workflow::WorkflowAuthoringPage<
    bcode_workflow::WorkflowPresetSnapshot,
    bcode_workflow::WorkflowAuthoringListCursor,
> {
    let bcode_workflow_store::WorkflowAuthoringStorePage {
        items: presets,
        has_more,
    } = page;
    let next_cursor = has_more.then(|| {
        let last = presets.last().expect("a page with more items is non-empty");
        bcode_workflow::WorkflowAuthoringListCursor {
            updated_at_ms: last.updated_at_ms,
            entity_id: last.preset_id.clone(),
        }
    });
    bcode_workflow::WorkflowAuthoringPage {
        items: presets.into_iter().map(workflow_preset_snapshot).collect(),
        next_cursor,
    }
}

fn authored_workflow_inspection(
    state: &ServerState,
    workflow_id: &str,
    limit: usize,
) -> Result<Option<bcode_workflow::AuthoredWorkflowInspection>, super::ServerError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(workflow) = store.authored_workflow(workflow_id)? else {
        return Ok(None);
    };
    let drafts = store
        .list_workflow_drafts(workflow_id, limit)?
        .into_iter()
        .map(|draft| bcode_workflow::WorkflowDraftInspectionSummary {
            identity: bcode_workflow::WorkflowDraftIdentity {
                workflow_id: draft.workflow_id,
                draft_id: draft.draft_id,
            },
            base_revision: draft.base_revision,
            generation: draft.generation,
            checksum_sha256: draft.checksum_sha256,
            created_at_ms: draft.created_at_ms,
            updated_at_ms: draft.updated_at_ms,
        })
        .collect();
    let revisions = store
        .list_workflow_revisions(workflow_id, limit)?
        .into_iter()
        .map(
            |revision| bcode_workflow::WorkflowRevisionInspectionSummary {
                identity: bcode_workflow::WorkflowRevisionIdentity {
                    workflow_id: revision.workflow_id,
                    revision: revision.revision,
                },
                source_checksum_sha256: revision.source_checksum_sha256,
                executable_source_checksum_sha256: revision.executable_source_checksum_sha256,
                definition_identity: revision.definition_identity,
                published_at_ms: revision.published_at_ms,
            },
        )
        .collect();
    let presets = store
        .list_workflow_presets(workflow_id, limit)?
        .into_iter()
        .map(|preset| bcode_workflow::WorkflowPresetInspectionSummary {
            workflow_id: preset.workflow_id,
            preset_id: preset.preset_id,
            revision: preset.revision,
            generation: preset.generation,
            has_run_limit_override: preset.run_limits.is_some(),
            created_at_ms: preset.created_at_ms,
            updated_at_ms: preset.updated_at_ms,
        })
        .collect();
    let events = store
        .workflow_authoring_events(workflow_id, None, limit)?
        .into_iter()
        .map(|event| bcode_workflow::WorkflowAuthoringEventSnapshot {
            event_seq: event.event_seq,
            workflow_id: event.workflow_id,
            event_type: event.event_type,
            revision: event
                .payload
                .get("revision")
                .and_then(serde_json::Value::as_u64),
            definition_id: event
                .payload
                .get("definition_id")
                .and_then(serde_json::Value::as_str)
                .map(ToString::to_string),
            definition_version: event
                .payload
                .get("definition_version")
                .and_then(serde_json::Value::as_u64)
                .and_then(|version| u32::try_from(version).ok()),
            activated: event
                .payload
                .get("activated")
                .and_then(serde_json::Value::as_bool),
            created_at_ms: event.created_at_ms,
        })
        .collect();
    let issues = store
        .diagnose_authored_workflow(workflow_id, limit)?
        .into_iter()
        .map(workflow_authoring_issue_snapshot)
        .collect();
    drop(store);
    Ok(Some(bcode_workflow::AuthoredWorkflowInspection {
        workflow: authored_workflow_snapshot(workflow),
        drafts,
        revisions,
        presets,
        events,
        issues,
    }))
}

fn workflow_authoring_issue_snapshot(
    issue: bcode_workflow_store::WorkflowAuthoringIssue,
) -> bcode_workflow::WorkflowAuthoringIssueSnapshot {
    match issue {
        bcode_workflow_store::WorkflowAuthoringIssue::InvalidActiveRevision { revision } => {
            bcode_workflow::WorkflowAuthoringIssueSnapshot::InvalidActiveRevision { revision }
        }
        bcode_workflow_store::WorkflowAuthoringIssue::MissingCompiledDefinition {
            revision,
            definition_id,
            definition_version,
        } => bcode_workflow::WorkflowAuthoringIssueSnapshot::MissingCompiledDefinition {
            revision,
            definition_id,
            definition_version,
        },
        bcode_workflow_store::WorkflowAuthoringIssue::OrphanedPreset {
            preset_id,
            revision,
        } => bcode_workflow::WorkflowAuthoringIssueSnapshot::OrphanedPreset {
            preset_id,
            revision,
        },
        bcode_workflow_store::WorkflowAuthoringIssue::StaleDraftBase {
            draft_id,
            base_revision,
        } => bcode_workflow::WorkflowAuthoringIssueSnapshot::StaleDraftBase {
            draft_id,
            base_revision,
        },
    }
}

pub fn authored_workflow_snapshot(
    workflow: bcode_workflow_store::AuthoredWorkflow,
) -> bcode_workflow::AuthoredWorkflowSnapshot {
    bcode_workflow::AuthoredWorkflowSnapshot {
        workflow_id: workflow.workflow_id,
        title: workflow.title,
        description: workflow.description,
        archived: workflow.archived,
        active_revision: workflow.active_revision,
        created_at_ms: workflow.created_at_ms,
        updated_at_ms: workflow.updated_at_ms,
    }
}

pub fn workflow_draft_snapshot(
    draft: bcode_workflow_store::WorkflowDraft,
) -> bcode_workflow::WorkflowDraftSnapshot {
    bcode_workflow::WorkflowDraftSnapshot {
        identity: bcode_workflow::WorkflowDraftIdentity {
            workflow_id: draft.workflow_id,
            draft_id: draft.draft_id,
        },
        base_revision: draft.base_revision,
        generation: draft.generation,
        checksum_sha256: draft.checksum_sha256,
        document: draft.document,
        producer: draft.producer,
        created_at_ms: draft.created_at_ms,
        updated_at_ms: draft.updated_at_ms,
    }
}

pub fn workflow_revision_snapshot(
    revision: bcode_workflow_store::PublishedWorkflowRevision,
) -> bcode_workflow::WorkflowRevisionSnapshot {
    bcode_workflow::WorkflowRevisionSnapshot {
        identity: bcode_workflow::WorkflowRevisionIdentity {
            workflow_id: revision.workflow_id,
            revision: revision.revision,
        },
        source_checksum_sha256: revision.source_checksum_sha256,
        executable_source_checksum_sha256: revision.executable_source_checksum_sha256,
        definition_identity: revision.definition_identity,
        document: revision.document,
        producer: revision.producer,
        published_at_ms: revision.published_at_ms,
    }
}

pub fn workflow_preset_snapshot(
    preset: bcode_workflow_store::WorkflowPreset,
) -> bcode_workflow::WorkflowPresetSnapshot {
    bcode_workflow::WorkflowPresetSnapshot {
        workflow_id: preset.workflow_id,
        preset_id: preset.preset_id,
        revision: preset.revision,
        name: preset.name,
        generation: preset.generation,
        configuration: preset.configuration,
        run_limits: preset.run_limits,
        producer: preset.producer,
        created_at_ms: preset.created_at_ms,
        updated_at_ms: preset.updated_at_ms,
    }
}

/// Return one bounded page of authored workflows.
pub fn list_authored_workflows(
    state: &ServerState,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_workflow::WorkflowAuthoringPage<
        bcode_workflow::AuthoredWorkflowSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_authored_workflows_page(cursor, limit)?;
    Ok(authored_workflow_page(page, limit))
}

/// Return one authored workflow description when it exists.
pub fn authored_workflow(
    state: &ServerState,
    workflow_id: &str,
) -> Result<
    Option<bcode_workflow::AuthoredWorkflowSnapshot>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authored_workflow(workflow_id)
        .map(|workflow| workflow.map(authored_workflow_snapshot))
}

/// Return a bounded authored-workflow inspection when the workflow exists.
pub fn inspect_authored_workflow(
    state: &ServerState,
    workflow_id: &str,
    limit: usize,
) -> Result<Option<bcode_workflow::AuthoredWorkflowInspection>, super::ServerError> {
    authored_workflow_inspection(state, workflow_id, limit)
}

/// Return one bounded page of drafts for an authored workflow.
pub fn list_drafts(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_workflow::WorkflowAuthoringPage<
        bcode_workflow::WorkflowDraftSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_drafts_page(workflow_id, cursor, limit.saturating_add(1))?;
    Ok(workflow_draft_page(page, limit))
}

/// Return one authored workflow draft when it exists.
pub fn draft(
    state: &ServerState,
    workflow_id: &str,
    draft_id: &str,
) -> Result<Option<bcode_workflow::WorkflowDraftSnapshot>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_draft(workflow_id, draft_id)
        .map(|draft| draft.map(workflow_draft_snapshot))
}

/// Return one bounded page of published revisions for an authored workflow.
pub fn list_revisions(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<bcode_workflow::WorkflowRevisionListCursor>,
    limit: usize,
) -> Result<
    bcode_workflow::WorkflowAuthoringPage<
        bcode_workflow::WorkflowRevisionSnapshot,
        bcode_workflow::WorkflowRevisionListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_revisions_page(workflow_id, cursor, limit)?;
    Ok(workflow_revision_page(page, limit))
}

/// Return one published workflow revision when it exists.
pub fn revision(
    state: &ServerState,
    workflow_id: &str,
    revision: u64,
) -> Result<
    Option<bcode_workflow::WorkflowRevisionSnapshot>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(workflow_id, revision)
        .map(|revision| revision.map(workflow_revision_snapshot))
}

/// Return one bounded page of presets for an authored workflow.
pub fn list_presets(
    state: &ServerState,
    workflow_id: &str,
    cursor: Option<&bcode_workflow::WorkflowAuthoringListCursor>,
    limit: usize,
) -> Result<
    bcode_workflow::WorkflowAuthoringPage<
        bcode_workflow::WorkflowPresetSnapshot,
        bcode_workflow::WorkflowAuthoringListCursor,
    >,
    bcode_workflow_store::WorkflowStoreError,
> {
    let page = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_workflow_presets_page(workflow_id, cursor, limit.saturating_add(1))?;
    Ok(workflow_preset_page(page, limit))
}

/// Return one authored workflow preset when it exists.
pub fn preset(
    state: &ServerState,
    workflow_id: &str,
    preset_id: &str,
) -> Result<Option<bcode_workflow::WorkflowPresetSnapshot>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_preset(workflow_id, preset_id)
        .map(|preset| preset.map(workflow_preset_snapshot))
}

/// Return the latest durable publication receipt for one workflow package.
pub fn package_publication(
    state: &ServerState,
    package_id: &str,
) -> Result<
    Option<bcode_workflow::WorkflowPackagePublicationReceipt>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_package_publication(package_id, None)
}

/// Inspect one exact workflow launch target without mutation.
pub async fn launch_detail(
    state: &ServerState,
    request: &bcode_workflow::WorkflowLaunchDetailRequest,
) -> Result<bcode_workflow::WorkflowLaunchDetail, super::ServerError> {
    request.validate()?;
    match &request.source {
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource {
            source_path,
            source_format,
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(source_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source,
                source_format: actual_format,
                source_path,
                ..
            } = source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "explicit launch detail identity expected a standalone source".to_string(),
                )
                .into());
            };
            if actual_format != *source_format {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "explicit launch source format changed".to_string(),
                )
                .into());
            }
            let catalog = authoring_catalog(state).await?;
            let lowering =
                bcode_workflow::lower_workflow_authoring_source(&source, actual_format, &catalog)?;
            let preview = lowering.document.compilation_preview(&catalog, None);
            let item = standalone_launch_item(
                "explicit".to_string(),
                0,
                source_path.clone(),
                actual_format,
                &lowering,
                &preview,
                true,
            );
            Ok(bcode_workflow::WorkflowLaunchDetail {
                version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
                item,
                document: lowering.document,
                package_plan: None,
            })
        }
        source => {
            let page = launch_catalog(
                state,
                &bcode_workflow::WorkflowLaunchCatalogRequest {
                    version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
                    workspace: request.workspace.clone(),
                    limit: bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE,
                    cursor: None,
                    search: None,
                    source_kind: None,
                    readiness: None,
                },
            )
            .await?;
            let item = page
                .items
                .into_iter()
                .find(|item| &item.source == source)
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "workflow launch target is no longer discoverable".to_string(),
                    )
                })?;
            launch_detail_for_catalog_item(state, &request.workspace, item).await
        }
    }
}

#[allow(clippy::too_many_lines)]
async fn launch_detail_for_catalog_item(
    state: &ServerState,
    workspace: &std::path::Path,
    item: bcode_workflow::WorkflowLaunchCatalogItem,
) -> Result<bcode_workflow::WorkflowLaunchDetail, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let (document, package_plan) = match &item.source {
        bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
            export,
            manifest_path,
            ..
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(manifest_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Package { closure, .. } =
                source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "package launch identity no longer resolves to a package".to_string(),
                )
                .into());
            };
            let plan = bcode_workflow::plan_workflow_package_closure(&closure, &catalog)?;
            let entry = plan
                .packages
                .iter()
                .find(|entry| entry.package_id == plan.entry_package_id)
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package plan has no entry package".to_string(),
                    )
                })?;
            let member_id = entry
                .plan
                .lock
                .exports
                .iter()
                .find(|candidate| candidate.export == *export)
                .map(|candidate| candidate.member_id.as_str())
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package export is no longer available".to_string(),
                    )
                })?;
            let document = entry
                .plan
                .members
                .iter()
                .find(|member| member.member_id == member_id)
                .map(|member| member.lowering.document.clone())
                .ok_or_else(|| {
                    bcode_workflow_store::WorkflowStoreError::InvalidData(
                        "package export member is missing".to_string(),
                    )
                })?;
            (document, Some(plan))
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
            source_path,
            source_format,
            ..
        } => {
            let source = bcode_workflow_discovery::inspect_explicit_source(source_path)?;
            let bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source,
                source_format: actual_format,
                ..
            } = source
            else {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "standalone launch identity no longer resolves to a source".to_string(),
                )
                .into());
            };
            if actual_format != *source_format {
                return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "standalone launch source format changed".to_string(),
                )
                .into());
            }
            (
                bcode_workflow::lower_workflow_authoring_source(&source, actual_format, &catalog)?
                    .document,
                None,
            )
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::Template {
            owner_plugin_id,
            template_id,
            template_version,
        } => {
            let template =
                describe_template(state, owner_plugin_id, template_id, *template_version)?
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(
                            "workflow template is no longer available".to_string(),
                        )
                    })?;
            let document = template.authoring_document.ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "workflow template has no maintainable authoring document".to_string(),
                )
            })?;
            (document, None)
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { .. } => {
            return Err(
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "explicit source detail must use its direct path operation in {}",
                    workspace.display()
                ))
                .into(),
            );
        }
    };
    Ok(bcode_workflow::WorkflowLaunchDetail {
        version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
        item,
        document,
        package_plan,
    })
}

/// Discover and semantically preview one bounded workflow launch-catalog page.
///
/// Discovery is read-only. It never applies, publishes, repairs, or starts a workflow.
#[allow(clippy::too_many_lines)]
pub async fn launch_catalog(
    state: &ServerState,
    request: &bcode_workflow::WorkflowLaunchCatalogRequest,
) -> Result<bcode_workflow::WorkflowLaunchCatalogPage, super::ServerError> {
    request.validate()?;
    let config = &state.startup_config.workflows;
    let discovery = bcode_workflow_discovery::discover_workflows(
        &request.workspace,
        config,
        request.limit.saturating_add(1),
    )?;
    let catalog = authoring_catalog(state).await?;
    let mut items = Vec::new();
    for source in discovery.sources {
        match source {
            bcode_workflow_discovery::DiscoveredWorkflowSource::Package {
                source_label,
                precedence,
                manifest_path,
                closure,
                ..
            } => {
                let plan = bcode_workflow::plan_workflow_package_closure(&closure, &catalog)?;
                let entry_index = plan
                    .packages
                    .iter()
                    .position(|entry| entry.package_id == plan.entry_package_id)
                    .ok_or_else(|| {
                        bcode_workflow_store::WorkflowStoreError::InvalidData(
                            "planned workflow package closure has no entry package".to_string(),
                        )
                    })?;
                let entry = &plan.packages[entry_index];
                let mut preview_catalog = catalog.clone();
                for dependency in &plan.packages[..entry_index] {
                    for member in &dependency.plan.members {
                        preview_catalog.workflow_definitions.insert(
                            member.definition_identity.definition_id.clone(),
                            member.lowering.document.definition.clone(),
                        );
                    }
                }
                let preview = bcode_workflow::preview_workflow_package(
                    &entry.plan,
                    &preview_catalog,
                    &BTreeMap::new(),
                )?;
                let receipt = package_publication(state, &entry.package_id)?;
                let lock_digest = preview.lock.digest_sha256()?;
                let readiness = receipt.as_ref().map_or(
                    bcode_workflow::WorkflowLaunchReadiness::Unpublished,
                    |receipt| {
                        if receipt.package_lock_digest_sha256 == lock_digest {
                            bcode_workflow::WorkflowLaunchReadiness::Ready
                        } else {
                            bcode_workflow::WorkflowLaunchReadiness::Drifted
                        }
                    },
                );
                for locked_export in &entry.plan.lock.exports {
                    let export = &locked_export.export;
                    let member_id = &locked_export.member_id;
                    let member = preview
                        .members
                        .iter()
                        .find(|member| &member.member_id == member_id)
                        .ok_or_else(|| {
                            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                                "workflow package export '{export}' references missing member '{member_id}'"
                            ))
                        })?;
                    let planned = entry
                        .plan
                        .members
                        .iter()
                        .find(|planned| &planned.member_id == member_id)
                        .ok_or_else(|| {
                            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                                "workflow package export '{export}' has no planned member '{member_id}'"
                            ))
                        })?;
                    let compiled = member.compilation.compiled.as_ref();
                    items.push(bcode_workflow::WorkflowLaunchCatalogItem {
                        source: bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
                            package_id: entry.package_id.clone(),
                            export: export.clone(),
                            manifest_path: manifest_path.clone(),
                        },
                        source_label: source_label.clone(),
                        precedence,
                        title: planned.lowering.document.metadata.title.clone(),
                        description: planned.lowering.document.metadata.description.clone(),
                        readiness,
                        unavailable_reason: match readiness {
                            bcode_workflow::WorkflowLaunchReadiness::Ready => None,
                            bcode_workflow::WorkflowLaunchReadiness::Unpublished => {
                                Some("package must be explicitly applied and published".to_string())
                            }
                            bcode_workflow::WorkflowLaunchReadiness::Drifted => Some(
                                "published package lock differs from discovered source".to_string(),
                            ),
                            _ => Some("workflow package is unavailable".to_string()),
                        },
                        package_lock_digest_sha256: Some(lock_digest.clone()),
                        publication: receipt
                            .as_ref()
                            .and_then(|receipt| {
                                receipt
                                    .exports
                                    .iter()
                                    .find(|candidate| candidate.export == *export)
                            })
                            .and_then(|published| {
                                published.published_revision.as_ref().map(|revision| {
                                    bcode_workflow::WorkflowLaunchPublicationIdentity {
                                        workflow_id: revision.workflow_id.clone(),
                                        revision: revision.revision,
                                        definition_identity: published.definition_identity.clone(),
                                    }
                                })
                            }),
                        actions: package_launch_actions(readiness),
                        requirements: compiled
                            .map(|compiled| compiled.requirements.clone())
                            .unwrap_or_default(),
                        effects: compiled
                            .map(|compiled| compiled.effects.clone())
                            .unwrap_or_default(),
                        permissions: compiled
                            .map(|compiled| compiled.permissions.clone())
                            .unwrap_or_default(),
                        input_schema: planned.lowering.document.definition.input.clone(),
                        configuration_schema: planned
                            .lowering
                            .document
                            .configuration_schema
                            .clone(),
                        diagnostics: member.compilation.validation.diagnostics.clone(),
                    });
                }
            }
            bcode_workflow_discovery::DiscoveredWorkflowSource::Standalone {
                source_label,
                precedence,
                source_path,
                source_format,
                source,
            } => {
                let lowering = bcode_workflow::lower_workflow_authoring_source(
                    &source,
                    source_format,
                    &catalog,
                )?;
                let preview = lowering.document.compilation_preview(&catalog, None);
                items.push(standalone_launch_item(
                    source_label,
                    precedence,
                    source_path,
                    source_format,
                    &lowering,
                    &preview,
                    false,
                ));
            }
        }
    }
    for template in list_templates(
        state,
        request
            .limit
            .saturating_add(1)
            .min(bcode_workflow::MAX_WORKFLOW_LAUNCH_CATALOG_PAGE_SIZE),
    )? {
        let Some(document) = template.authoring_document else {
            continue;
        };
        let preview = document.compilation_preview(&catalog, None);
        let compiled = preview.compiled.as_ref();
        let readiness = if template.diagnostics.is_empty() && preview.is_compiled() {
            bcode_workflow::WorkflowLaunchReadiness::Ready
        } else {
            bcode_workflow::WorkflowLaunchReadiness::Unavailable
        };
        items.push(bcode_workflow::WorkflowLaunchCatalogItem {
            source: bcode_workflow::WorkflowLaunchSourceIdentity::Template {
                owner_plugin_id: template.owner_plugin_id.clone(),
                template_id: template.template.template_id.clone(),
                template_version: template.template.template_version,
            },
            source_label: format!("plugin:{}", template.owner_plugin_id),
            precedence: 200,
            title: document.metadata.title.clone(),
            description: document.metadata.description.clone(),
            readiness,
            unavailable_reason: (!template.diagnostics.is_empty()).then(|| {
                template
                    .diagnostics
                    .iter()
                    .map(|diagnostic| diagnostic.message.as_str())
                    .collect::<Vec<_>>()
                    .join("; ")
            }),
            package_lock_digest_sha256: None,
            publication: None,
            actions: template_launch_actions(readiness),
            requirements: compiled.map_or_else(
                || document.requirements.clone(),
                |compiled| compiled.requirements.clone(),
            ),
            effects: compiled
                .map(|compiled| compiled.effects.clone())
                .unwrap_or_default(),
            permissions: compiled
                .map(|compiled| compiled.permissions.clone())
                .unwrap_or_default(),
            input_schema: document.definition.input.clone(),
            configuration_schema: document.configuration_schema.clone(),
            diagnostics: preview.validation.diagnostics,
        });
    }
    Ok(project_launch_catalog_page(
        items,
        discovery
            .diagnostics
            .into_iter()
            .map(|diagnostic| bcode_workflow::WorkflowLaunchDiagnostic {
                source_label: diagnostic.source_label,
                path: diagnostic.path,
                code: diagnostic.code,
                message: diagnostic.message,
            })
            .collect(),
        request,
    ))
}

fn project_launch_catalog_page(
    mut items: Vec<bcode_workflow::WorkflowLaunchCatalogItem>,
    diagnostics: Vec<bcode_workflow::WorkflowLaunchDiagnostic>,
    request: &bcode_workflow::WorkflowLaunchCatalogRequest,
) -> bcode_workflow::WorkflowLaunchCatalogPage {
    let search = request.search.as_ref().map(|search| search.to_lowercase());
    items.retain(|item| {
        request.source_kind.is_none_or(|kind| {
            matches!(
                (kind, &item.source),
                (
                    bcode_workflow::WorkflowLaunchSourceKind::PackageExport,
                    bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport { .. }
                ) | (
                    bcode_workflow::WorkflowLaunchSourceKind::StandaloneSource,
                    bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource { .. }
                        | bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { .. }
                ) | (
                    bcode_workflow::WorkflowLaunchSourceKind::Template,
                    bcode_workflow::WorkflowLaunchSourceIdentity::Template { .. }
                )
            )
        }) && request
            .readiness
            .is_none_or(|readiness| readiness == item.readiness)
            && search.as_ref().is_none_or(|search| {
                item.title.to_lowercase().contains(search)
                    || item
                        .description
                        .as_ref()
                        .is_some_and(|description| description.to_lowercase().contains(search))
                    || item.source_label.to_lowercase().contains(search)
            })
    });
    items.sort_by(|left, right| {
        (&left.title, launch_source_key(&left.source))
            .cmp(&(&right.title, launch_source_key(&right.source)))
    });
    if let Some(cursor) = &request.cursor {
        items.retain(|item| {
            (&item.title, launch_source_key(&item.source))
                > (&cursor.title, cursor.source_key.clone())
        });
    }
    let has_more = items.len() > request.limit;
    items.truncate(request.limit);
    let next_cursor = has_more.then(|| items.last()).flatten().map(|item| {
        bcode_workflow::WorkflowLaunchCatalogCursor {
            title: item.title.clone(),
            source_key: launch_source_key(&item.source),
        }
    });
    bcode_workflow::WorkflowLaunchCatalogPage {
        version: bcode_workflow::WORKFLOW_LAUNCH_CATALOG_VERSION,
        items,
        diagnostics,
        next_cursor,
    }
}

fn standalone_launch_item(
    source_label: String,
    precedence: u32,
    source_path: std::path::PathBuf,
    source_format: bcode_workflow::WorkflowSourceFormat,
    lowering: &bcode_workflow::WorkflowSourceLoweringResult,
    preview: &bcode_workflow::WorkflowCompilationPreview,
    explicit: bool,
) -> bcode_workflow::WorkflowLaunchCatalogItem {
    let compiled = preview.compiled.as_ref();
    bcode_workflow::WorkflowLaunchCatalogItem {
        source: if explicit {
            bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource {
                source_path,
                source_format,
            }
        } else {
            bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
                workflow_id: lowering.document.workflow_id.clone(),
                source_path,
                source_format,
            }
        },
        source_label,
        precedence,
        title: lowering.document.metadata.title.clone(),
        description: lowering.document.metadata.description.clone(),
        readiness: if preview.is_compiled() {
            bcode_workflow::WorkflowLaunchReadiness::Unpublished
        } else {
            bcode_workflow::WorkflowLaunchReadiness::Invalid
        },
        unavailable_reason: Some(
            "source must be explicitly applied and published before start".to_string(),
        ),
        package_lock_digest_sha256: None,
        publication: None,
        actions: source_launch_actions(preview.is_compiled()),
        requirements: compiled.map_or_else(
            || lowering.document.requirements.clone(),
            |compiled| compiled.requirements.clone(),
        ),
        effects: compiled
            .map(|compiled| compiled.effects.clone())
            .unwrap_or_default(),
        permissions: compiled
            .map(|compiled| compiled.permissions.clone())
            .unwrap_or_default(),
        input_schema: lowering.document.definition.input.clone(),
        configuration_schema: lowering.document.configuration_schema.clone(),
        diagnostics: preview.validation.diagnostics.clone(),
    }
}

fn package_launch_actions(
    readiness: bcode_workflow::WorkflowLaunchReadiness,
) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    let available = |kind| Action {
        kind,
        enabled: true,
        unavailable_reason: None,
    };
    let unavailable = |kind, reason: &str| Action {
        kind,
        enabled: false,
        unavailable_reason: Some(reason.to_string()),
    };
    match readiness {
        bcode_workflow::WorkflowLaunchReadiness::Ready => vec![
            available(Kind::Validate),
            available(Kind::Preview),
            available(Kind::Start),
            available(Kind::OpenAuthoring),
            available(Kind::OpenSource),
        ],
        bcode_workflow::WorkflowLaunchReadiness::Unpublished
        | bcode_workflow::WorkflowLaunchReadiness::Drifted => vec![
            available(Kind::Validate),
            available(Kind::Preview),
            available(Kind::Apply),
            unavailable(
                Kind::Publish,
                "apply the exact discovered package drafts first",
            ),
            unavailable(Kind::Start, "publish the exact package lock first"),
            available(Kind::OpenAuthoring),
            available(Kind::OpenSource),
        ],
        _ => vec![
            available(Kind::Validate),
            unavailable(Kind::Start, "workflow package is not launchable"),
            available(Kind::OpenSource),
        ],
    }
}

fn source_launch_actions(compiled: bool) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    [
        (Kind::Validate, true, None),
        (
            Kind::Preview,
            compiled,
            Some("source validation must succeed"),
        ),
        (
            Kind::Apply,
            compiled,
            Some("source validation must succeed"),
        ),
        (Kind::Publish, false, Some("apply the source draft first")),
        (
            Kind::Start,
            false,
            Some("publish an immutable revision first"),
        ),
        (Kind::OpenAuthoring, true, None),
        (Kind::OpenSource, true, None),
    ]
    .into_iter()
    .map(|(kind, enabled, reason)| Action {
        kind,
        enabled,
        unavailable_reason: (!enabled).then(|| reason.unwrap_or("unavailable").to_string()),
    })
    .collect()
}

fn template_launch_actions(
    readiness: bcode_workflow::WorkflowLaunchReadiness,
) -> Vec<bcode_workflow::WorkflowLaunchActionAffordance> {
    use bcode_workflow::{
        WorkflowLaunchActionAffordance as Action, WorkflowLaunchActionKind as Kind,
    };
    let ready = readiness == bcode_workflow::WorkflowLaunchReadiness::Ready;
    vec![
        Action {
            kind: Kind::Preview,
            enabled: ready,
            unavailable_reason: (!ready)
                .then(|| "template requirements are unavailable".to_string()),
        },
        Action {
            kind: Kind::Start,
            enabled: ready,
            unavailable_reason: (!ready)
                .then(|| "template requirements are unavailable".to_string()),
        },
        Action {
            kind: Kind::OpenAuthoring,
            enabled: true,
            unavailable_reason: None,
        },
    ]
}

fn launch_source_key(source: &bcode_workflow::WorkflowLaunchSourceIdentity) -> String {
    match source {
        bcode_workflow::WorkflowLaunchSourceIdentity::PackageExport {
            package_id, export, ..
        } => format!("package:{package_id}:{export}"),
        bcode_workflow::WorkflowLaunchSourceIdentity::StandaloneSource {
            workflow_id,
            source_path,
            ..
        } => format!("source:{workflow_id}:{}", source_path.display()),
        bcode_workflow::WorkflowLaunchSourceIdentity::ExplicitSource { source_path, .. } => {
            format!("explicit:{}", source_path.display())
        }
        bcode_workflow::WorkflowLaunchSourceIdentity::Template {
            owner_plugin_id,
            template_id,
            template_version,
        } => format!("template:{owner_plugin_id}:{template_id}:{template_version}"),
    }
}

/// Atomically apply one validated workflow package to the canonical workflow store.
pub fn apply_package(
    state: &ServerState,
    request: &bcode_workflow::ApplyWorkflowPackageRequest,
) -> Result<bcode_workflow::WorkflowPackageMutationResult, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .apply_workflow_package(&request.request, request.applied_at_ms)
}

/// Atomically publish one workflow package in the canonical workflow store.
pub fn publish_package(
    state: &ServerState,
    request: &bcode_workflow::PublishWorkflowPackageRequest,
) -> Result<bcode_workflow::WorkflowPackageMutationResult, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .publish_workflow_package(&request.request, request.published_at_ms)
}

/// Validate and plan one bounded workflow package closure.
pub async fn validate_package(
    state: &ServerState,
    request: bcode_workflow::WorkflowPackageComputationRequest,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowPackageValidationResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let plan = run_computation(state, request.control, fallback_operation_id, move || {
        bcode_workflow::plan_workflow_package_closure(&request.closure, &catalog)
    })
    .await??;
    Ok(bcode_workflow::WorkflowPackageValidationResult { plan })
}

/// Preview one already planned workflow package without persistence side effects.
pub async fn preview_package(
    state: &ServerState,
    request: bcode_workflow::WorkflowPackagePreviewRequest,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowPackagePreview, super::ServerError> {
    let mut catalog = authoring_catalog(state).await?;
    for dependency in &request.dependency_plans {
        for member in &dependency.members {
            catalog.workflow_definitions.insert(
                member.definition_identity.definition_id.clone(),
                member.lowering.document.definition.clone(),
            );
        }
    }
    run_computation(state, request.control, fallback_operation_id, move || {
        bcode_workflow::preview_workflow_package(&request.plan, &catalog, &request.configurations)
    })
    .await?
    .map_err(super::ServerError::from)
}

/// Lower and validate one bounded authored-workflow source document.
pub async fn validate_source(
    state: &ServerState,
    request: bcode_workflow::WorkflowSourceComputationRequest,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowSourceValidationResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let source_format = request.source_format;
    let lowering = run_computation(state, request.control, fallback_operation_id, move || {
        bcode_workflow::lower_workflow_authoring_source(&request.source, source_format, &catalog)
    })
    .await??;
    Ok(bcode_workflow::WorkflowSourceValidationResult {
        source_format,
        lowering,
    })
}

/// Lower and preview compilation for one bounded authored-workflow source document.
pub async fn preview_source(
    state: &ServerState,
    request: bcode_workflow::WorkflowSourcePreviewRequest,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowSourcePreviewResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let source_format = request.source_format;
    let configuration = request.configuration;
    let (lowering, preview) =
        run_computation(state, request.control, fallback_operation_id, move || {
            let lowering = bcode_workflow::lower_workflow_authoring_source(
                &request.source,
                source_format,
                &catalog,
            )?;
            let preview = lowering
                .document
                .compilation_preview(&catalog, configuration.as_ref());
            Ok::<_, bcode_workflow::WorkflowError>((lowering, preview))
        })
        .await??;
    Ok(bcode_workflow::WorkflowSourcePreviewResult {
        source_format,
        lowering,
        preview,
    })
}

/// Validate one bounded authored-workflow document.
pub async fn validate_authoring(
    state: &ServerState,
    document: bcode_workflow::WorkflowAuthoringDocument,
    control: bcode_workflow::WorkflowComputationControl,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowValidationReport, super::ServerError> {
    let started_at = std::time::Instant::now();
    let report = run_computation(state, control, fallback_operation_id, move || {
        document.validation_report()
    })
    .await?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.validation.duration_ms",
        started_at,
        if report.valid { "valid" } else { "invalid" },
    );
    Ok(report)
}

/// Preview compilation for one bounded authored-workflow document.
pub async fn preview_compilation(
    state: &ServerState,
    document: bcode_workflow::WorkflowAuthoringDocument,
    configuration: Option<serde_json::Value>,
    control: bcode_workflow::WorkflowComputationControl,
    fallback_operation_id: String,
) -> Result<bcode_workflow::WorkflowCompilationPreview, super::ServerError> {
    let started_at = std::time::Instant::now();
    let catalog = authoring_catalog(state).await?;
    let preview = run_computation(state, control, fallback_operation_id, move || {
        document.compilation_preview(&catalog, configuration.as_ref())
    })
    .await?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.compilation.duration_ms",
        started_at,
        if preview.compiled.is_some() {
            "compiled"
        } else {
            "rejected"
        },
    );
    Ok(preview)
}

pub struct AuthorityGuard {
    pub authority: bcode_workflow_store::WorkflowExecutionAuthority,
    pub _session_ownership: Option<bcode_session::SessionOwnershipGuard>,
}

/// How the current daemon relates to a run's recorded coordinator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PriorOwnerLiveness {
    /// Canonical evidence shows the recorded coordinator is still live or cannot be classified.
    LiveOrUnverifiable,
    /// A session-owner observation positively classified the recorded coordinator as stale.
    ObservedEnded,
    /// No lease observation and no live daemon record name the recorded coordinator instance.
    NoLiveTrace,
}

/// Classify whether the run's recorded coordinator daemon still exists.
///
/// Two independent canonical sources are consulted: session-owner lease observations for the
/// run's parent session, and the daemon registry's live records. A daemon whose lease was lost
/// but whose process is still registered as live counts as live. Ambiguity fails closed.
async fn prior_owner_liveness(
    state: &ServerState,
    session_id: super::SessionId,
    current: &bcode_workflow_store::WorkflowExecutionAuthority,
) -> Result<PriorOwnerLiveness, super::ServerError> {
    let root = state.sessions.session_store_root().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow ownership transfer requires a persistent session store".to_string(),
        )
    })?;
    let observations = bcode_session::lease::session_owner_observations(&root, session_id)
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let mut observed_ended = false;
    for observation in observations {
        if observation.owner.daemon_instance_id.as_deref()
            != Some(current.daemon_instance_id.as_str())
        {
            continue;
        }
        match observation.liveness {
            bcode_session::lease::SessionOwnerLiveness::Live
            | bcode_session::lease::SessionOwnerLiveness::Unverifiable => {
                return Ok(PriorOwnerLiveness::LiveOrUnverifiable);
            }
            bcode_session::lease::SessionOwnerLiveness::Stale => observed_ended = true,
        }
    }
    // A lease can be lost while the daemon process survives, so consult the daemon registry
    // before concluding the owner is gone. Only the record naming the recorded coordinator is
    // probed: classifying every registry record costs a bounded endpoint probe per record.
    for (_, record) in bcode_daemon_lifecycle::read_records(&state.state_root) {
        if record.instance_id != current.daemon_instance_id {
            continue;
        }
        match bcode_daemon_lifecycle::classify_daemon_record(&record).await {
            bcode_daemon_lifecycle::DaemonRecordClassification::CurrentHealthy
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalExactResponsive
            | bcode_daemon_lifecycle::DaemonRecordClassification::HistoricalProcessVerifiedProtocolUnsupported
            | bcode_daemon_lifecycle::DaemonRecordClassification::ResponsiveIdentityMismatch
            | bcode_daemon_lifecycle::DaemonRecordClassification::Unverifiable => {
                return Ok(PriorOwnerLiveness::LiveOrUnverifiable);
            }
            bcode_daemon_lifecycle::DaemonRecordClassification::UnreachableStale => {
                observed_ended = true;
            }
        }
    }
    Ok(if observed_ended {
        PriorOwnerLiveness::ObservedEnded
    } else {
        PriorOwnerLiveness::NoLiveTrace
    })
}

fn current_artifact_id(state: &ServerState) -> String {
    state.daemon_status.artifact_id.as_ref().map_or_else(
        || state.daemon_status.build_fingerprint.clone(),
        ToString::to_string,
    )
}

fn run_parent_session_id(
    state: &ServerState,
    run_id: &str,
) -> Result<super::SessionId, super::ServerError> {
    let run = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)?
        .ok_or_else(|| bcode_workflow_store::WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    run.parent_session_id
        .as_deref()
        .and_then(|value| value.parse::<super::SessionId>().ok())
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow run has no canonical parent session for ownership transfer".to_string(),
            )
            .into()
        })
}

/// Resolve the current daemon's durable execution authority for one run.
///
/// * Same daemon instance: returns the recorded authority.
/// * Same artifact, different (ended) instance: same-artifact compare-and-swap transfer.
/// * Different artifact: allowed only when the recorded coordinator verifiably ended and the run
///   holds no live attempt. The run is then reassigned to this artifact with an audited evidence
///   record. Live or unverifiable prior owners are refused with an error that names them.
///
/// # Errors
///
/// Returns an error when the recorded coordinator is live or unverifiable, when the run still has
/// live attempts on another artifact, when session ownership cannot be acquired, or when the
/// compare-and-swap loses a race.
pub async fn execution_authority(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<Option<AuthorityGuard>, super::ServerError> {
    let current = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .execution_authority(run_id)?;
    let Some(current) = current else {
        return Ok(None);
    };
    if current.daemon_instance_id == state.daemon_status.instance_id {
        return Ok(Some(AuthorityGuard {
            authority: current,
            _session_ownership: None,
        }));
    }
    let artifact_id = current_artifact_id(state);
    let session_id = run_parent_session_id(state, run_id)?;
    let liveness = prior_owner_liveness(state, session_id, &current).await?;
    if liveness == PriorOwnerLiveness::LiveOrUnverifiable {
        return Err(super::ServerError::WorkflowOwnedByLiveDaemon {
            run_id: run_id.to_string(),
            daemon_instance_id: current.daemon_instance_id.clone(),
            target_artifact_id: current.target_artifact_id.clone(),
            same_artifact: current.target_artifact_id == artifact_id,
        });
    }
    let session_ownership = state
        .sessions
        .acquire_session_ownership(session_id, bcode_session::SessionOwnershipKind::RuntimeWork)
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let replacement = bcode_workflow_store::WorkflowExecutionAuthority {
        target_artifact_id: artifact_id.clone(),
        daemon_instance_id: state.daemon_status.instance_id.clone(),
        generation: current.generation.saturating_add(1),
        fencing_token: uuid::Uuid::new_v4().to_string(),
    };
    let now_ms = super::current_unix_millis();
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if current.target_artifact_id == artifact_id {
        store.transfer_execution_authority(run_id, &current, &replacement, now_ms)?;
    } else {
        let evidence = bcode_workflow_store::EndedOwnerEvidence {
            ended_daemon_instance_id: current.daemon_instance_id.clone(),
            ended_target_artifact_id: current.target_artifact_id.clone(),
            liveness: match liveness {
                PriorOwnerLiveness::ObservedEnded => {
                    bcode_workflow_store::EndedOwnerLiveness::ObservedEnded
                }
                PriorOwnerLiveness::NoLiveTrace | PriorOwnerLiveness::LiveOrUnverifiable => {
                    bcode_workflow_store::EndedOwnerLiveness::NoLiveTrace
                }
            },
            artifact_image_available: bcode_daemon_lifecycle::artifact_image_is_available(
                &state.state_root,
                &current.target_artifact_id,
            ),
        };
        store.reassign_execution_authority_from_ended_owner(
            run_id,
            &current,
            &replacement,
            &evidence,
            now_ms,
        )?;
        tracing::info!(
            target: "bcode_server::workflow",
            run_id,
            from_artifact = %current.target_artifact_id,
            from_instance = %current.daemon_instance_id,
            liveness = ?evidence.liveness,
            "reassigned quiescent workflow run from an ended coordinator on another artifact"
        );
    }
    drop(store);
    Ok(Some(AuthorityGuard {
        authority: replacement,
        _session_ownership: Some(session_ownership),
    }))
}

pub fn validate_workflow_definition_for_production(
    state: &ServerState,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), super::ServerError> {
    let capabilities = bcode_workflow::WorkflowProductionCapabilities::current();
    let admission = definition
        .production_admission(&capabilities)
        .map_err(|error| super::ServerError::WorkflowDefinitionUnsupported(error.to_string()))?;
    if !admission.is_supported() {
        let summary = admission
            .diagnostics
            .iter()
            .map(|diagnostic| {
                diagnostic.node_id.as_ref().map_or_else(
                    || format!("{}: {}", diagnostic.code, diagnostic.message),
                    |node_id| {
                        format!(
                            "{} at node '{}': {}",
                            diagnostic.code, node_id, diagnostic.message
                        )
                    },
                )
            })
            .collect::<Vec<_>>()
            .join("; ");
        return Err(super::ServerError::WorkflowDefinitionUnsupported(summary));
    }
    for node in definition.nodes.values() {
        if node.kind != bcode_workflow::NodeKind::PluginBlock {
            continue;
        }
        let block: bcode_workflow::WorkflowBlockDefinition =
            serde_json::from_value(node.configuration.clone()).map_err(|error| {
                super::ServerError::WorkflowDefinitionUnsupported(format!(
                    "plugin block node '{}' has invalid configuration: {error}",
                    node.id
                ))
            })?;
        block.validate().map_err(|error| {
            super::ServerError::WorkflowDefinitionUnsupported(format!(
                "plugin block node '{}' is invalid: {error}",
                node.id
            ))
        })?;
        let declared = state
            .plugins
            .registry()
            .workflow_blocks()
            .into_iter()
            .any(|candidate| candidate == block);
        if !declared {
            return Err(super::ServerError::WorkflowCapabilityUnavailable(format!(
                "plugin block node '{}' requires unavailable exact contract {}:{} v{} ({})",
                node.id, block.plugin_id, block.block_id, block.block_version, block.operation
            )));
        }
    }
    Ok(())
}

fn compile_workflow_template(
    template: &bcode_plugin::WorkflowTemplateContribution,
    _configuration: &serde_json::Value,
) -> Result<bcode_workflow::WorkflowDefinition, super::ServerError> {
    let mut definition = template.definition().clone();
    let uses_configuration_envelope =
        definition.input.type_name == template.configuration_schema().type_name;
    if !uses_configuration_envelope {
        return Ok(definition);
    }
    definition.input = template.configuration_schema().clone();
    definition.output = template.configuration_schema().clone();
    for (node_id, node) in &mut definition.nodes {
        if node.kind != bcode_workflow::NodeKind::Agent {
            continue;
        }
        let original_input = node.input.clone();
        let mut agent = serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
            node.configuration.clone(),
        )
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow template entry agent configuration is invalid: {error}"
            ))
        })?;
        if original_input.type_name == template.configuration_schema().type_name {
            node.input = template.configuration_schema().clone();
            for edge in definition
                .edges
                .iter_mut()
                .filter(|edge| edge.to == *node_id)
            {
                if let Some(transform) = &mut edge.transform
                    && transform.output.type_name == original_input.type_name
                {
                    transform.output = template.configuration_schema().clone();
                }
            }
        }
        if node.output.type_name == template.configuration_schema().type_name {
            node.output = template.configuration_schema().clone();
            if let bcode_workflow::WorkflowPromptOutputPolicy::Structured { result } =
                &mut agent.output
            {
                result.schema = template.configuration_schema().clone();
            }
        }
        node.configuration = serde_json::to_value(agent).map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow template entry agent configuration cannot be serialized: {error}"
            ))
        })?;
    }
    definition.validate().map_err(|error| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
    })?;
    Ok(definition)
}

fn find_workflow_template<'a>(
    state: &'a ServerState,
    owner_plugin_id: &str,
    template_id: &str,
    template_version: u32,
) -> Option<&'a bcode_plugin::WorkflowTemplateContribution> {
    state
        .plugins
        .registry()
        .workflow_templates()
        .into_iter()
        .find(|(owner, template)| {
            *owner == owner_plugin_id
                && template.template_id == template_id
                && template.template_version == template_version
        })
        .map(|(_, template)| template)
}

pub async fn instantiate_template(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::WorkflowTemplateInstantiationRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    let template = find_workflow_template(
        state,
        &request.owner_plugin_id,
        &request.template_id,
        request.template_version,
    )
    .ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow template not found or disabled".to_string(),
        )
    })?;
    let description = template_description(state, &request.owner_plugin_id, template)?;
    if !description.diagnostics.is_empty() {
        return Err(super::ServerError::WorkflowCapabilityUnavailable(
            description
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let source = template.authoring_document().ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "only standard authoring-document templates can be instantiated as drafts".to_string(),
        )
    })?;
    let mut document = source.clone();
    document.workflow_id.clone_from(&request.workflow_id);
    document.definition.name.clone_from(&request.workflow_id);
    document.producer = bcode_workflow::WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Plugin,
        producer_id: Some(request.owner_plugin_id.clone()),
        source_revision: None,
    };
    document.validate()?;
    let catalog = authoring_catalog(state).await?;
    let preview = document.compilation_preview(&catalog, None);
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(format!(
            "template instantiation requires a successful compilation preview: {:?}",
            preview.validation.diagnostics
        ))
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let workflow = bcode_workflow_store::AuthoredWorkflow {
        workflow_id: request.workflow_id.clone(),
        title: document.metadata.title.clone(),
        description: document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.workflow_id,
        draft_id: request.draft_id,
        base_revision: None,
        generation: 1,
        checksum_sha256: document.source_digest_sha256()?,
        producer: document.producer.clone(),
        document,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
    Ok((workflow, draft))
}

fn persist_exact_template_call_dependencies(
    state: &ServerState,
    definition: &bcode_workflow::WorkflowDefinition,
) -> Result<(), super::ServerError> {
    let dependencies = bcode_workflow::workflow_dependency_manifest(definition)?;
    for dependency in dependencies {
        let identity = dependency.target.definition_identity();
        let Some((owner_plugin_id, template)) = state
            .plugins
            .registry()
            .workflow_templates()
            .into_iter()
            .find(|(owner, template)| {
                template
                    .definition_identity(owner)
                    .is_ok_and(|candidate| &candidate == identity)
            })
        else {
            continue;
        };
        let child_definition = template.definition();
        let actual = template
            .definition_identity(owner_plugin_id)
            .map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
            })?;
        if &actual != identity {
            return Err(
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "template dependency identity mismatch for {}",
                    identity.kind
                ))
                .into(),
            );
        }
        state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .persist_definition(
                &actual.definition_id,
                actual.definition_version,
                child_definition,
            )?;
        persist_exact_template_call_dependencies(state, child_definition)?;
    }
    Ok(())
}

#[allow(clippy::too_many_lines)]
pub async fn start_run(
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::WorkflowRunStartRequest,
    authored_provenance: Option<bcode_workflow_store::AuthoredWorkflowRunProvenance>,
) -> Result<bcode_workflow::WorkflowRunStartResponse, super::ServerError> {
    let stored_definition = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .definition(&request.definition_id, request.definition_version)?
        .ok_or_else(|| {
            super::ServerError::WorkflowCapabilityUnavailable(format!(
                "workflow definition not found: {} v{}",
                request.definition_id, request.definition_version
            ))
        })?;
    let definition: bcode_workflow::WorkflowDefinition =
        serde_json::from_str(&stored_definition.definition_json)?;
    validate_workflow_definition_for_production(state, &definition)?;
    let uses_fixed_generation = definition.nodes.values().any(|node| {
        node.kind == bcode_workflow::NodeKind::Agent
            && serde_json::from_value::<bcode_workflow::WorkflowPromptConfiguration>(
                node.configuration.clone(),
            )
            .is_ok_and(|configuration| {
                configuration.execution_target
                    == bcode_workflow::PromptContextTarget::FixedGenerationFork
            })
    });
    if uses_fixed_generation && request.parent_session_generation.is_none() {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "fixed-generation workflow prompts require parent_session_generation at start"
                .to_string(),
        )
        .into());
    }
    let parent_session = state
        .sessions
        .session_summary(request.parent_session_id)
        .await?;
    if let Some(expected_generation) = request.parent_session_generation {
        let current_generation = state
            .sessions
            .current_session_generation(request.parent_session_id)
            .await?;
        if current_generation != expected_generation {
            return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "parent session generation changed before workflow admission: expected {expected_generation}, current {current_generation}"
            ))
            .into());
        }
    }
    let run_id = request
        .run_id
        .clone()
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let created_at_ms = super::current_unix_millis();
    let workspace_snapshot = if request.workspace_snapshot.is_empty() {
        parent_session
            .working_directory
            .to_string_lossy()
            .into_owned()
    } else {
        request.workspace_snapshot
    };
    let authorization_profile = state
        .plugins
        .invoke_service_by_interface_json::<_, super::AgentPolicyProfileIdentity>(
            super::AGENT_PROFILE_INTERFACE_ID,
            super::OP_RESOLVE_POLICY_PROFILE_IDENTITY,
            &super::ResolveAgentPolicyProfileIdentityRequest {
                profile_id: super::session_agent_selection(state, parent_session.id).await,
                effective_config_toml: bcode_config::encode_effective_config(
                    &state.session_config(parent_session.id).await,
                )
                .ok()
                .map(Box::new),
            },
        )
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow authorization profile resolution failed closed: {error}"
            ))
        })?;
    let authorization_profile = bcode_workflow::WorkflowAuthorizationProfileIdentity {
        version: authorization_profile.version,
        provider_id: authorization_profile.provider_id,
        profile_id: authorization_profile.profile_id,
        policy_digest_sha256: authorization_profile.policy_digest_sha256,
    };
    authorization_profile.validate().map_err(|error| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
    })?;
    let workflow_session_ownership = state
        .sessions
        .acquire_session_ownership(
            parent_session.id,
            bcode_session::SessionOwnershipKind::RuntimeWork,
        )
        .await
        .map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
        })?;
    let new_run = bcode_workflow_store::NewWorkflowRun {
        run_id: run_id.clone(),
        definition_id: request.definition_id.clone(),
        definition_version: request.definition_version,
        workspace_snapshot,
        parent_session_id: Some(request.parent_session_id.to_string()),
        parent_session_generation: request.parent_session_generation,
        binding: request.binding,
        authored_provenance,
        input: request.input,
        execution_authority: Some(bcode_workflow_store::WorkflowExecutionAuthority {
            target_artifact_id: state.daemon_status.artifact_id.as_ref().map_or_else(
                || state.daemon_status.build_fingerprint.clone(),
                ToString::to_string,
            ),
            daemon_instance_id: state.daemon_status.instance_id.clone(),
            generation: 1,
            fencing_token: uuid::Uuid::new_v4().to_string(),
        }),
        created_at_ms,
        authorization_profile,
        authorization_ceiling: bcode_workflow::WorkflowToolCapability::Mutating,
        limits: request.limits,
    };
    let run = {
        let mut store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let _created = store.create_run_idempotent(&new_run)?;
        store
            .run_summary(&run_id)?
            .expect("created or existing workflow run must be readable")
    };
    let runtime_work_id = super::register_workflow_runtime_work(
        state,
        parent_session.id,
        &run_id,
        format!(
            "workflow {} v{}",
            request.definition_id, request.definition_version
        ),
    )
    .await;
    super::drive_workflow_run(state, &run_id).await?;
    drop(workflow_session_ownership);
    Ok(bcode_workflow::WorkflowRunStartResponse {
        run,
        runtime_work_id,
    })
}

/// Validate, persist, and admit one exact workflow definition.
pub async fn start(
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::WorkflowStartRequest,
) -> Result<bcode_workflow::WorkflowRunStartResponse, super::ServerError> {
    let started_at = std::time::Instant::now();
    validate_workflow_definition_for_production(state, &request.definition)?;
    if request.identity.kind != request.binding.workflow_kind {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow logical identity does not match its binding kind".to_string(),
        )
        .into());
    }
    let expected_identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        request.identity.kind.clone(),
        &request.definition,
    )
    .map_err(|error| bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string()))?;
    if expected_identity != request.identity {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow exact identity does not match its compiled definition".to_string(),
        )
        .into());
    }
    let stored = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .persist_definition(
            &request.identity.definition_id,
            request.identity.definition_version,
            &request.definition,
        )?;
    if stored.definition_id != request.identity.definition_id
        || stored.version != request.identity.definition_version
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow exact identity does not match persisted definition".to_string(),
        )
        .into());
    }
    let result = start_run(
        state,
        bcode_workflow::WorkflowRunStartRequest {
            definition_id: request.identity.definition_id,
            definition_version: request.identity.definition_version,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot.unwrap_or_default(),
            parent_session_id: request.parent_session_id,
            parent_session_generation: None,
            binding: Some(request.binding),
            input: Some(request.input),
            limits: request.limits,
        },
        None,
    )
    .await;
    state.metrics.record_histogram_with_labels(
        "workflow.admission.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "outcome".to_string(),
            if result.is_ok() { "ok" } else { "error" }.to_string(),
        )]),
    );
    result
}

/// Compile and start one exact enabled workflow template.
pub async fn start_template(
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::WorkflowTemplateStartRequest,
) -> Result<bcode_workflow::WorkflowRunStartResponse, super::ServerError> {
    let template = find_workflow_template(
        state,
        &request.owner_plugin_id,
        &request.template_id,
        request.template_version,
    )
    .ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow template not found or disabled".to_string(),
        )
    })?;
    let description = template_description(state, &request.owner_plugin_id, template)?;
    if !description.diagnostics.is_empty() {
        return Err(super::ServerError::WorkflowCapabilityUnavailable(
            description
                .diagnostics
                .iter()
                .map(|diagnostic| diagnostic.message.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        ));
    }
    let validator =
        jsonschema::validator_for(&template.configuration_schema().schema).map_err(|error| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "invalid template configuration schema: {error}"
            ))
        })?;
    if let Err(error) = validator.validate(&request.configuration) {
        return Err(
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "template configuration is invalid: {error}"
            ))
            .into(),
        );
    }
    let binding_kind = description.identity.kind.clone();
    let definition = compile_workflow_template(template, &request.configuration)?;
    persist_exact_template_call_dependencies(state, &definition)?;
    let identity = bcode_workflow::WorkflowDefinitionIdentity::for_definition(
        description.identity.kind,
        &definition,
    )
    .map_err(|error| bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string()))?;
    start(
        state,
        bcode_workflow::WorkflowStartRequest {
            identity,
            definition,
            run_id: request.run_id,
            workspace_snapshot: request.workspace_snapshot,
            parent_session_id: request.parent_session_id,
            input: request.configuration,
            binding: bcode_workflow_store::WorkflowRunBinding {
                owner_plugin_id: request.owner_plugin_id,
                workflow_kind: binding_kind,
                scope_key: request.template_version.to_string(),
                display_label: Some(template.title.clone()),
                single_active: false,
            },
            limits: request.limits,
        },
    )
    .await
}

/// Request cancellation of a workflow tree while holding its durable execution authority.
pub async fn cancel_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "active workflow has no durable execution authority".to_string(),
        )
    })?;
    let (recorded, attempts) = {
        let mut store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (recorded, cancelled_run_ids) =
            store.request_cancellation_tree(run_id, super::current_unix_millis())?;
        let mut attempts = Vec::new();
        for cancelled_run_id in cancelled_run_ids {
            let remaining = 1_000_usize.saturating_sub(attempts.len());
            if remaining == 0 {
                break;
            }
            attempts.extend(store.active_attempt_cancellations(&cancelled_run_id, remaining)?);
        }
        drop(store);
        (recorded, attempts)
    };
    super::propagate_persisted_workflow_cancellation(state, attempts).await?;
    super::settle_workflow_runtime_work(state, run_id).await?;
    Ok(recorded)
}

/// Explicit maintenance: reconcile nonterminal runs whose recorded coordinator verifiably ended.
///
/// Each nonterminal run is classified with the same evidence `execution_authority` uses. Runs
/// whose coordinator is live or unverifiable, runs that still hold live attempts on another
/// artifact, and runs already owned by this daemon are reported as skipped. When `apply` is set,
/// every orphaned run is reassigned to this daemon (audited) and cancelled; a `repair_required`
/// run keeps that status because it still needs explicit attempt-level repair.
///
/// # Errors
///
/// Returns an error when the workflow store cannot enumerate runs.
pub async fn reconcile_orphaned_runs(
    state: &std::sync::Arc<ServerState>,
    apply: bool,
    limit: usize,
) -> Result<bcode_workflow::OrphanedWorkflowRunReport, super::ServerError> {
    let runs = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .nonterminal_runs(limit)?;
    let mut report = bcode_workflow::OrphanedWorkflowRunReport {
        applied: apply,
        ..Default::default()
    };
    for run in runs {
        let skip = |reason: String| bcode_workflow::SkippedWorkflowRun {
            run_id: run.run_id.clone(),
            status: run.status,
            reason,
        };
        let authority = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .execution_authority(&run.run_id)?;
        let Some(authority) = authority else {
            report
                .skipped
                .push(skip("run has no durable execution authority".to_string()));
            continue;
        };
        if authority.daemon_instance_id == state.daemon_status.instance_id {
            report
                .skipped
                .push(skip("run is owned by this daemon".to_string()));
            continue;
        }
        let Ok(session_id) = run_parent_session_id(state, &run.run_id) else {
            report
                .skipped
                .push(skip("run has no canonical parent session".to_string()));
            continue;
        };
        match prior_owner_liveness(state, session_id, &authority).await? {
            PriorOwnerLiveness::LiveOrUnverifiable => {
                report.skipped.push(skip(format!(
                    "coordinator daemon {} (artifact {}) is live or unverifiable",
                    authority.daemon_instance_id, authority.target_artifact_id
                )));
                continue;
            }
            PriorOwnerLiveness::ObservedEnded | PriorOwnerLiveness::NoLiveTrace => {}
        }
        let orphan = bcode_workflow::OrphanedWorkflowRun {
            run_id: run.run_id.clone(),
            workflow_kind: run
                .binding
                .as_ref()
                .map(|binding| binding.workflow_kind.clone()),
            parent_session_id: run.parent_session_id.clone(),
            status: run.status,
            ended_daemon_instance_id: authority.daemon_instance_id.clone(),
            ended_target_artifact_id: authority.target_artifact_id.clone(),
            artifact_image_available: bcode_daemon_lifecycle::artifact_image_is_available(
                &state.state_root,
                &authority.target_artifact_id,
            ),
            updated_at_ms: run.updated_at_ms,
        };
        if !apply {
            report.reconciled.push(orphan);
            continue;
        }
        // Reassignment goes through the same fenced path every control operation uses, so a
        // run that still holds live attempts on the ended artifact is refused here too.
        if let Err(error) = execution_authority(state, &run.run_id).await {
            report
                .skipped
                .push(skip(format!("authority reassignment refused: {error}")));
            continue;
        }
        if run.status == bcode_workflow_store::RunStatus::RepairRequired {
            // Ownership is now local; the run still needs explicit attempt repair.
            report.reconciled.push(orphan);
            continue;
        }
        if let Err(error) = cancel_run(state, &run.run_id).await {
            report.skipped.push(skip(format!(
                "cancellation after reassignment failed: {error}"
            )));
            continue;
        }
        report.reconciled.push(orphan);
    }
    Ok(report)
}

/// Pause a workflow run while holding its durable execution authority.
pub async fn pause_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "active workflow has no durable execution authority".to_string(),
        )
    })?;
    let changed = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pause_run(run_id, super::current_unix_millis())?;
    if changed {
        // An explicitly paused run with in-flight attempts keeps its node work registered until
        // those attempts observe the pause; the run-level registration is suspended now so a
        // fully paused run never pins the daemon.
        super::settle_workflow_runtime_work(state, run_id).await?;
    }
    Ok(changed)
}

/// Resume a workflow run and continue scheduling it without transport framing.
pub async fn resume_run(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
) -> Result<bool, super::ServerError> {
    let run = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)?
        .ok_or_else(|| bcode_workflow_store::WorkflowStoreError::RunNotFound {
            run_id: run_id.to_string(),
        })?;
    if !matches!(
        run.status,
        bcode_workflow_store::RunStatus::Running | bcode_workflow_store::RunStatus::Paused
    ) {
        return state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .resume_run(run_id, super::current_unix_millis())
            .map_err(super::ServerError::from);
    }
    let _authority = execution_authority(state, run_id).await?.ok_or_else(|| {
        bcode_workflow_store::WorkflowStoreError::InvalidData(
            "active workflow has no durable execution authority".to_string(),
        )
    })?;
    let changed = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resume_run(run_id, super::current_unix_millis())?;
    if changed
        && let Some(parent_session_id) = run
            .parent_session_id
            .as_deref()
            .and_then(|value| value.parse::<super::SessionId>().ok())
    {
        // Pausing suspended the run-level runtime work; a resumed run is live again.
        super::register_workflow_runtime_work(
            state,
            parent_session_id,
            run_id,
            format!("workflow {} v{}", run.definition_id, run.definition_version),
        )
        .await;
    }
    super::drive_workflow_run(state, run_id).await?;
    Ok(changed)
}

/// Inspect one workflow run for bounded structural inconsistencies without mutation.
pub fn doctor_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow_store::WorkflowDoctorReport, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .doctor_run(run_id, limit)
}

/// Retry one exact failed workflow node and resume scheduling.
pub async fn retry_node(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    failed_attempt: u32,
) -> Result<bcode_workflow_store::WorkflowNodeRetryResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retry_failed_node(
            run_id,
            node_id,
            activation_id,
            failed_attempt,
            super::current_unix_millis(),
        )?;
    state.metrics.record_histogram(
        "workflow.retry.admission.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    super::drive_workflow_run(state, run_id).await?;
    Ok(result)
}

/// Return bounded durable input and approval waits without transport framing.
pub fn list_waits(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_workflow::WaitingActivation>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .waiting_activations(run_id, limit)
}

/// Resolve one exact workflow input wait and continue runnable descendants.
pub async fn provide_input(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    value: serde_json::Value,
) -> Result<bcode_workflow_store::WaitingResolutionResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .provide_input(
            run_id,
            node_id,
            activation_id,
            value,
            super::current_unix_millis(),
        )?;
    super::drive_workflow_run_and_parents(state, run_id).await?;
    state.metrics.record_histogram(
        "workflow.input.wait_resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
    );
    Ok(result)
}

/// Resolve one exact workflow approval wait and continue runnable descendants.
pub async fn resolve_approval(
    state: &std::sync::Arc<ServerState>,
    run_id: &str,
    node_id: &str,
    activation_id: &str,
    approved: bool,
) -> Result<bcode_workflow_store::WaitingResolutionResult, super::ServerError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .resolve_approval(
            run_id,
            node_id,
            activation_id,
            approved,
            super::current_unix_millis(),
        )?;
    super::drive_workflow_run_and_parents(state, run_id).await?;
    state.metrics.record_histogram_with_labels(
        "workflow.approval.resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "decision".to_string(),
            if approved { "approve" } else { "deny" }.to_string(),
        )]),
    );
    Ok(result)
}

/// Return bounded pending mutation approvals across all workflow runs.
pub fn list_mutation_approvals_all(
    state: &ServerState,
    limit: usize,
) -> Result<
    Vec<bcode_workflow::WorkflowMutationApprovalInspection>,
    bcode_workflow_store::WorkflowStoreError,
> {
    Ok(state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_mutation_approvals_all(limit)?
        .into_iter()
        .map(mutation_approval_inspection)
        .collect())
}

/// Return bounded pending mutation approvals for one workflow run.
pub fn list_mutation_approvals(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<
    Vec<bcode_workflow::WorkflowMutationApprovalInspection>,
    bcode_workflow_store::WorkflowStoreError,
> {
    Ok(state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .pending_mutation_approvals(run_id, limit)?
        .into_iter()
        .map(mutation_approval_inspection)
        .collect())
}

/// Resolve one mutation approval and continue an approved workflow.
pub async fn resolve_mutation_approval(
    state: &std::sync::Arc<ServerState>,
    approval_id: &str,
    decision: bcode_workflow::WorkflowMutationApprovalDecision,
) -> Result<bcode_workflow::WorkflowMutationApprovalResolution, super::ServerError> {
    resolve_mutation_approval_with_continuation(state, approval_id, decision, |run_id| async move {
        super::drive_workflow_run_and_parents(state, &run_id).await
    })
    .await
}

// Keep durable resolution and post-commit outcome handling shared across execution adapters.
// The continuation is invoked only after approval admits pending work; it must retain
// the runtime's ownership fencing and must not independently resolve the approval.
pub async fn resolve_mutation_approval_with_continuation<F, Fut>(
    state: &std::sync::Arc<ServerState>,
    approval_id: &str,
    decision: bcode_workflow::WorkflowMutationApprovalDecision,
    continue_run: F,
) -> Result<bcode_workflow::WorkflowMutationApprovalResolution, super::ServerError>
where
    F: FnOnce(String) -> Fut,
    Fut: std::future::Future<Output = Result<(), super::ServerError>>,
{
    let started_at = std::time::Instant::now();
    let approval_context = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .mutation_approval_context(approval_id)?;
    // Resolution mutates active workflow state even when no continuation follows
    // (denial or expiration). Hold verified execution ownership across both steps.
    let authority = if let Some((run_id, _)) = approval_context.as_ref() {
        execution_authority(state, run_id).await?
    } else {
        None
    };
    let mut resolved_at_ms = 0;
    let mut result = {
        let mut store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store.resolve_mutation_approval_with_clock(
            approval_id,
            decision,
            authority.as_ref().map(|guard| &guard.authority),
            || {
                resolved_at_ms = super::current_unix_millis();
                resolved_at_ms
            },
        )?
    };
    result.continuation = Some(if result.admits_continuation() {
        if let Some((run_id, _)) = approval_context.as_ref() {
            if continue_run(run_id.clone()).await.is_ok() {
                bcode_workflow::WorkflowApprovalContinuation::Driven
            } else {
                bcode_workflow::WorkflowApprovalContinuation::Failed
            }
        } else {
            bcode_workflow::WorkflowApprovalContinuation::Failed
        }
    } else {
        bcode_workflow::WorkflowApprovalContinuation::NotRequired
    });
    let decision_label = match decision {
        bcode_workflow::WorkflowMutationApprovalDecision::Approve => "approve",
        bcode_workflow::WorkflowMutationApprovalDecision::Deny => "deny",
    };
    state.metrics.record_histogram_with_labels(
        "workflow.approval.wait.duration_ms",
        approval_context.map_or(0, |(_, requested_at_ms)| {
            resolved_at_ms.saturating_sub(requested_at_ms)
        }),
        std::collections::BTreeMap::from([
            ("decision".to_string(), decision_label.to_string()),
            ("status".to_string(), result.status.clone()),
        ]),
    );
    state.metrics.record_histogram_with_labels(
        "workflow.approval.resolution.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([("decision".to_string(), decision_label.to_string())]),
    );
    Ok(result)
}

/// Return bounded keyset-paged attempt history for one workflow run.
pub fn attempt_history(
    state: &ServerState,
    run_id: &str,
    cursor: Option<&bcode_workflow_store::AttemptCursor>,
    limit: usize,
) -> Result<Vec<bcode_workflow::AttemptSummary>, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .attempt_history(run_id, cursor, limit)
}

fn mutation_approval_inspection(
    approval: bcode_workflow::WorkflowMutationApproval,
) -> bcode_workflow::WorkflowMutationApprovalInspection {
    let scope = approval.scope;
    bcode_workflow::WorkflowMutationApprovalInspection {
        approval_id: approval.approval_id,
        run_id: approval.run_id,
        node_id: approval.node_id,
        activation_id: approval.activation_id,
        requested_at_ms: approval.requested_at_ms,
        expires_at_ms: approval.expires_at_ms,
        scope: mutation_scope_inspection(scope),
    }
}

fn mutation_scope_inspection(
    scope: bcode_workflow::WorkflowMutationGrantScope,
) -> bcode_workflow::WorkflowMutationApprovalScopeInspection {
    bcode_workflow::WorkflowMutationApprovalScopeInspection {
        plugin_id: scope.plugin_id,
        block_id: scope.block_id,
        block_version: scope.block_version,
        operation: scope.operation,
        workspace_snapshot: scope.workspace_snapshot,
        input_summary: scope.input_summary,
        resource_claims: scope.resource_claims,
        reconciliation: scope.reconciliation,
        capability: scope.capability,
    }
}

fn decision_inspection(
    decision: bcode_workflow::WorkflowDecision,
) -> bcode_workflow::WorkflowDecisionInspection {
    let value = mutation_approval_decision_disclosure(&decision);
    bcode_workflow::WorkflowDecisionInspection {
        decision_id: decision.decision_id,
        run_id: decision.run_id,
        node_id: decision.node_id,
        decision_type: decision.decision_type,
        value,
        created_at_ms: decision.created_at_ms,
    }
}

fn mutation_approval_decision_disclosure(
    decision: &bcode_workflow::WorkflowDecision,
) -> bcode_workflow::WorkflowDecisionValueDisclosure {
    let observation = (|| {
        if decision.decision_type != "mutation_approval" {
            return None;
        }
        let approval_id = decision.value.get("approval_id")?.as_str()?;
        if approval_id.trim().is_empty()
            || decision.decision_id != format!("mutation-approval:{approval_id}")
        {
            return None;
        }
        let approved = match decision.value.get("decision")?.as_str()? {
            "approved" => true,
            "denied" => false,
            _ => return None,
        };
        let scope: bcode_workflow::WorkflowMutationGrantScope =
            serde_json::from_value(decision.value.get("scope")?.clone()).ok()?;
        if scope.validate().is_err()
            || scope.run_id != decision.run_id
            || decision.node_id.as_ref() != Some(&scope.node_id)
        {
            return None;
        }
        Some(
            bcode_workflow::WorkflowDecisionValueDisclosure::MutationApproval {
                approval_id: approval_id.to_owned(),
                approved,
            },
        )
    })();
    observation.unwrap_or(bcode_workflow::WorkflowDecisionValueDisclosure::Withheld)
}

fn grant_inspection(
    grant: bcode_workflow::WorkflowGrant,
) -> bcode_workflow::WorkflowGrantInspection {
    let scope = if grant.scope.get("version").is_some() {
        serde_json::from_value::<bcode_workflow::WorkflowMutationGrantScope>(grant.scope.clone())
            .ok()
            .filter(|scope| {
                scope.validate().is_ok()
                    && scope.run_id == grant.run_id
                    && scope.node_id == grant.node_id
            })
            .map_or(
                bcode_workflow::WorkflowGrantScopeDisclosure::Withheld,
                |scope| bcode_workflow::WorkflowGrantScopeDisclosure::Mutation {
                    scope: Box::new(mutation_scope_inspection(scope)),
                },
            )
    } else {
        policy_grant_disclosure(&grant)
    };
    bcode_workflow::WorkflowGrantInspection {
        grant_id: grant.grant_id,
        run_id: grant.run_id,
        node_id: grant.node_id,
        scope,
        granted_at_ms: grant.granted_at_ms,
        expires_at_ms: grant.expires_at_ms,
        max_uses: grant.max_uses,
        uses_consumed: grant.uses_consumed,
    }
}

fn policy_grant_disclosure(
    grant: &bcode_workflow::WorkflowGrant,
) -> bcode_workflow::WorkflowGrantScopeDisclosure {
    // This unversioned representation has exactly these known fields. Do not guess an
    // extended/future representation or reinterpret a malformed mutation scope as policy.
    let known = grant.scope.as_object().is_some_and(|object| {
        object.len() == 3
            && object
                .keys()
                .all(|key| matches!(key.as_str(), "grant_id" | "scope" | "capability"))
    }) && grant
        .scope
        .get("scope")
        .and_then(serde_json::Value::as_object)
        .is_some_and(|object| {
            object.keys().all(|key| {
                matches!(
                    key.as_str(),
                    "definition" | "definition_version" | "workspace" | "node" | "run"
                )
            })
        });
    if !known {
        return bcode_workflow::WorkflowGrantScopeDisclosure::Withheld;
    }
    serde_json::from_value::<bcode_workflow::WorkflowPolicyGrant>(grant.scope.clone())
        .ok()
        .filter(|policy| {
            policy.validate().is_ok()
                && policy.grant_id == grant.grant_id
                && policy.scope.node == grant.node_id
                && policy
                    .scope
                    .run
                    .as_ref()
                    .is_none_or(|run| run == &grant.run_id)
        })
        .map_or(
            bcode_workflow::WorkflowGrantScopeDisclosure::Withheld,
            |policy| bcode_workflow::WorkflowGrantScopeDisclosure::Policy {
                scope: policy.scope,
                capability: policy.capability,
            },
        )
}

fn verified_history_event(
    store: &bcode_workflow_store::WorkflowStore,
    row: bcode_workflow_store::WorkflowEventRow,
) -> Result<bcode_workflow::WorkflowHistoryEvent, bcode_workflow_store::WorkflowStoreError> {
    let mut event = history_event(row);
    if let Some(identity) = event
        .payload
        .get("dispatch_identity")
        .and_then(serde_json::Value::as_str)
    {
        let attempt = store.attempt_by_dispatch_identity(identity)?;
        correlate_history_attempt(&mut event, attempt);
    }
    Ok(event)
}

fn correlate_history_attempt(
    event: &mut bcode_workflow::WorkflowHistoryEvent,
    attempt: Option<bcode_workflow::AttemptSummary>,
) {
    if let Some(attempt) = attempt.filter(|attempt| attempt.run_id == event.run_id) {
        event.payload["attempt_correlation"] =
            serde_json::json!(bcode_workflow::WorkflowHistoryAttemptCorrelation {
                run_id: attempt.run_id,
                node_id: attempt.node_id,
                activation_id: attempt.activation_id,
                attempt: attempt.attempt,
            });
    } else {
        event
            .payload
            .as_object_mut()
            .expect("projected payload object")
            .remove("dispatch_identity");
        event.payload["correlation_unavailable"] = serde_json::json!("unverified_attempt");
    }
}

fn run_lifecycle_observation(kind: &str) -> Option<serde_json::Value> {
    use bcode_workflow::RunStatus;
    let status = match kind {
        "run_created" | "run_resumed" => RunStatus::Running,
        "run_paused" => RunStatus::Paused,
        "run_completed" => RunStatus::Completed,
        "run_cancelled" => RunStatus::Cancelled,
        _ => return None,
    };
    Some(serde_json::json!(
        bcode_workflow::WorkflowRunLifecycleObservation { status }
    ))
}

fn output_validation_observation(payload: &serde_json::Value) -> serde_json::Value {
    let facts = (|| {
        let schema_version = u32::try_from(payload.get("schema_version")?.as_u64()?).ok()?;
        let artifact = payload.get("artifact_reference")?;
        if !artifact.is_null() && !artifact.is_string() {
            return None;
        }
        Some(bcode_workflow::WorkflowOutputValidationObservation {
            schema_version,
            has_artifact: !artifact.is_null(),
            created_at_ms: payload.get("created_at_ms")?.as_u64()?,
        })
    })();
    facts.map_or_else(
        || serde_json::json!({"unavailable": "invalid_output_validation"}),
        |facts| serde_json::json!(facts),
    )
}

fn authority_transfer_observation(payload: &serde_json::Value) -> serde_json::Value {
    // Only audited numeric facts cross the boundary, never fencing credentials or evidence.
    let facts = (|| {
        Some(bcode_workflow::WorkflowAuthorityTransferObservation {
            previous_generation: payload.get("from")?.get("generation")?.as_u64()?,
            generation: payload.get("to")?.get("generation")?.as_u64()?,
            reassigned_at_ms: payload.get("reassigned_at_ms")?.as_u64()?,
        })
    })();
    facts.map_or_else(
        || serde_json::json!({"unavailable": "invalid_authority_transfer"}),
        |facts| serde_json::json!(facts),
    )
}

fn mutation_approval_observation(
    row: &bcode_workflow_store::WorkflowEventRow,
) -> serde_json::Value {
    serde_json::from_value::<bcode_workflow::WorkflowMutationApproval>(row.payload.clone())
        .ok()
        .filter(|approval| {
            approval.scope.validate().is_ok()
                && approval.run_id == row.run_id
                && approval.scope.run_id == approval.run_id
                && approval.scope.node_id == approval.node_id
                && approval.scope.activation_id == approval.activation_id
                && !approval.approval_id.trim().is_empty()
        })
        .map_or_else(
            || serde_json::json!({"unavailable":"invalid_mutation_approval"}),
            |approval| serde_json::json!(mutation_approval_inspection(approval)),
        )
}

fn activation_observation(row: &bcode_workflow_store::WorkflowEventRow) -> serde_json::Value {
    let facts = (|| {
        let activation = row.payload.get("activation")?;
        let node_id = activation.get("node_id")?.as_str()?;
        let activation_id = activation.get("activation_id")?.as_str()?;
        let generation = activation.get("dependency_generation")?.as_u64()?;
        let waiting = row.event_type == "activation_waiting";
        let status = row.payload.get("status")?.as_str()?;
        let wait_kind = match status {
            "waiting_input" => Some(bcode_workflow::WorkflowWaitKind::Input),
            "waiting_approval" => Some(bcode_workflow::WorkflowWaitKind::Approval),
            "pending" => None,
            _ => return None,
        };
        if activation.get("run_id")?.as_str()? != row.run_id
            || activation_id
                != bcode_workflow_store::activation_identity(&row.run_id, node_id, generation)
            || (waiting && !matches!(status, "waiting_input" | "waiting_approval"))
            || (!waiting && status != "pending")
        {
            return None;
        }
        Some(bcode_workflow::WorkflowActivationObservation {
            node_id: node_id.to_owned(),
            activation_id: activation_id.to_owned(),
            dependency_generation: generation,
            created_at_ms: activation.get("created_at_ms")?.as_u64()?,
            waiting,
            wait_kind,
        })
    })();
    facts.map_or_else(
        || serde_json::json!({"unavailable":"invalid_activation"}),
        |facts| serde_json::json!(facts),
    )
}

fn attempt_admission_observation(payload: &serde_json::Value) -> serde_json::Value {
    serde_json::from_value::<bcode_workflow::WorkflowAttemptAdmissionObservation>(payload.clone())
        .ok()
        .filter(|observation| observation.attempt > 0)
        .map_or_else(
            || serde_json::json!({"unavailable":"invalid_attempt_admission"}),
            |observation| serde_json::json!(observation),
        )
}

fn attempt_preparation_observation(payload: &serde_json::Value) -> serde_json::Value {
    serde_json::from_value::<bcode_workflow::WorkflowAttemptPreparationObservation>(payload.clone())
        .ok()
        .filter(|observation| observation.attempt > 0)
        .map_or_else(
            || serde_json::json!({"unavailable": "invalid_attempt_preparation"}),
            |observation| serde_json::json!(observation),
        )
}

fn approval_resolution_history(row: &bcode_workflow_store::WorkflowEventRow) -> serde_json::Value {
    let approval_id = row
        .payload
        .get("approval_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    let decision = bcode_workflow::WorkflowDecision {
        decision_id: format!("mutation-approval:{approval_id}"),
        run_id: row.run_id.clone(),
        node_id: row
            .payload
            .get("scope")
            .and_then(|scope| scope.get("node_id"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        decision_type: "mutation_approval".to_owned(),
        value: row.payload.clone(),
        created_at_ms: row.created_at_ms,
    };
    let observation = mutation_approval_decision_disclosure(&decision);
    let mut payload = serde_json::json!({});
    if row.event_type == "mutation_approval_denied" {
        payload["reason"] =
            serde_json::json!(bcode_workflow::WorkflowHistoryDiagnostic::MutationApprovalDenied);
    }
    if let bcode_workflow::WorkflowDecisionValueDisclosure::MutationApproval { approved, .. } =
        &observation
        && *approved == (row.event_type == "mutation_approval_approved")
    {
        payload["resolution"] = serde_json::json!(observation);
    } else {
        payload["unavailable"] = serde_json::json!("invalid_mutation_approval_resolution");
    }
    payload
}

fn history_diagnostic(
    row: &bcode_workflow_store::WorkflowEventRow,
) -> Option<bcode_workflow::WorkflowHistoryDiagnostic> {
    use bcode_workflow::WorkflowHistoryDiagnostic as Diagnostic;
    match row.event_type.as_str() {
        "attempt_failed" => Some(Diagnostic::AttemptFailed),
        "fan_out_member_failed" => Some(Diagnostic::FanOutMemberFailed),
        "run_failed" => Some(Diagnostic::RunFailed),
        "parallel_join_failed" => Some(Diagnostic::ParallelJoinFailed),
        "mutation_approval_denied" => Some(Diagnostic::MutationApprovalDenied),
        "mutation_approval_expired" => Some(Diagnostic::MutationApprovalExpired),
        "run_deadline_elapsed" => Some(Diagnostic::RunDeadlineElapsed),
        "fan_out_failed" => Some(Diagnostic::FanOutFailed),
        "attempt_paused" => Some(
            match row
                .payload
                .get("reason")
                .and_then(serde_json::Value::as_str)
            {
                Some("provider_unavailable") => Diagnostic::ProviderUnavailable,
                Some("idle_timeout") => Diagnostic::IdleTimeout,
                Some("tool_round_limit_reached") => Diagnostic::ToolRoundLimitReached,
                Some("steering") => Diagnostic::Steering,
                _ => Diagnostic::PauseReasonUnavailable,
            },
        ),
        _ => None,
    }
}

/// Adapt a durable diagnostic row without interpreting opaque producer content.
fn history_event(
    row: bcode_workflow_store::WorkflowEventRow,
) -> bcode_workflow::WorkflowHistoryEvent {
    let diagnostic = history_diagnostic(&row);
    let payload = if matches!(
        row.event_type.as_str(),
        "mutation_approval_approved" | "mutation_approval_denied"
    ) {
        approval_resolution_history(&row)
    } else if let Some(diagnostic) = diagnostic {
        // Keep the established reason field consumable by the portable failure projection,
        // but expose only a closed semantic classification, never owner-supplied text.
        serde_json::json!({"reason": diagnostic})
    } else if let Some(lifecycle) = run_lifecycle_observation(&row.event_type) {
        lifecycle
    } else if row.event_type == "output_validated" {
        output_validation_observation(&row.payload)
    } else if row.event_type == "authority_reassigned" {
        authority_transfer_observation(&row.payload)
    } else if row.event_type == "waiting_activation_resolved" {
        serde_json::from_value::<bcode_workflow::WorkflowWaitResolutionObservation>(
            row.payload.clone(),
        )
        .map_or_else(
            |_| serde_json::json!({"unavailable": "invalid_wait_resolution"}),
            |observation| serde_json::json!(observation),
        )
    } else if matches!(
        row.event_type.as_str(),
        "activation_created" | "activation_waiting"
    ) {
        activation_observation(&row)
    } else if row.event_type == "mutation_approval_requested" {
        mutation_approval_observation(&row)
    } else if row.event_type == "attempt_prepared" {
        attempt_preparation_observation(&row.payload)
    } else if row.event_type == "attempt_admitted" {
        attempt_admission_observation(&row.payload)
    } else if matches!(row.event_type.as_str(), "grant_recorded" | "grant_consumed") {
        serde_json::from_value::<bcode_workflow::WorkflowGrantUseObservation>(row.payload.clone())
            .ok()
            .filter(|grant| {
                grant
                    .max_uses
                    .is_none_or(|limit| limit > 0 && grant.uses_consumed <= limit)
            })
            .map_or_else(
                || serde_json::json!({"unavailable": "invalid_grant_use"}),
                |observation| serde_json::json!(observation),
            )
    } else if row.event_type == "control_node_settled" {
        serde_json::from_value::<bcode_workflow::WorkflowRepeatSettlementObservation>(
            row.payload.clone(),
        )
        .map_or_else(
            |_| serde_json::json!({"unavailable": "invalid_repeat_settlement"}),
            |observation| serde_json::json!(observation),
        )
    } else if row.event_type == "fan_out_materialized" {
        serde_json::from_value::<bcode_workflow::WorkflowFanOutObservation>(row.payload.clone())
            .map_or_else(
                |_| serde_json::json!({"unavailable": "invalid_fan_out_materialization"}),
                |observation| serde_json::json!(observation),
            )
    } else {
        // New producer kinds must be reviewed before their payloads cross this boundary.
        // Preserve the row/cursor, but explicitly report unavailable diagnostic details.
        serde_json::json!({"unavailable": "unreviewed_event_payload"})
    };
    let mut payload = payload;
    // Dispatch identities are SHA-256 digests generated by the store, not owner prose.
    // Preserve this bounded correlation fact for existing failure-view consumers.
    if let Some(identity) = row
        .payload
        .get("dispatch_identity")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
        && diagnostic.is_some()
    {
        payload["dispatch_identity"] = serde_json::Value::String(identity.to_owned());
    }
    bcode_workflow::WorkflowHistoryEvent {
        event_seq: row.event_seq,
        run_id: row.run_id,
        event_type: row.event_type,
        payload,
        created_at_ms: row.created_at_ms,
    }
}

#[cfg(test)]
#[test]
fn history_failure_and_pause_diagnostics_exclude_owner_content() {
    for kind in [
        "attempt_failed",
        "fan_out_member_failed",
        "run_failed",
        "parallel_join_failed",
        "mutation_approval_denied",
        "mutation_approval_expired",
        "run_deadline_elapsed",
        "fan_out_failed",
        "attempt_paused",
    ] {
        for payload in [
            serde_json::json!({"message": "SECRET", "reason": "SECRET", "dispatch_identity": "SECRET"}),
            serde_json::Value::Null,
        ] {
            let observation = history_event(bcode_workflow_store::WorkflowEventRow {
                event_seq: 1,
                run_id: "run".to_owned(),
                event_type: kind.to_owned(),
                payload,
                created_at_ms: 2,
            });
            assert!(
                !serde_json::to_string(&observation)
                    .unwrap()
                    .contains("SECRET")
            );
            let _: bcode_workflow::WorkflowHistoryDiagnostic =
                serde_json::from_value(observation.payload["reason"].clone()).unwrap();
        }
    }
    for reason in [
        "provider_unavailable",
        "idle_timeout",
        "tool_round_limit_reached",
        "steering",
    ] {
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 1,
            run_id: "run".to_owned(),
            event_type: "attempt_paused".to_owned(),
            payload: serde_json::json!({"reason": reason, "message": "SECRET"}),
            created_at_ms: 2,
        });
        assert_eq!(event.payload, serde_json::json!({"reason": reason}));
    }
}

#[cfg(test)]
#[test]
fn history_authority_transfer_excludes_credentials_and_fails_closed() {
    for payload in [
        serde_json::json!({"from": {"generation": 1, "fencing_token": "SECRET"},
            "to": {"generation": 2, "fencing_token": "SECRET"},
            "evidence": {"private": "SECRET"}, "reassigned_at_ms": 123}),
        serde_json::json!({"from": "SECRET", "reassigned_at_ms": "SECRET"}),
    ] {
        let valid = payload["from"].is_object();
        let observation = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 7,
            run_id: "run".to_owned(),
            event_type: "authority_reassigned".to_owned(),
            payload,
            created_at_ms: 123,
        });
        assert!(
            !serde_json::to_string(&observation)
                .unwrap()
                .contains("SECRET")
        );
        if valid {
            let transfer: bcode_workflow::WorkflowAuthorityTransferObservation =
                serde_json::from_value(observation.payload).unwrap();
            assert_eq!(transfer.previous_generation, 1);
            assert_eq!(transfer.generation, 2);
            assert_eq!(transfer.reassigned_at_ms, 123);
        } else {
            assert_eq!(
                observation.payload,
                serde_json::json!({"unavailable": "invalid_authority_transfer"})
            );
        }
    }
}

#[cfg(test)]
#[test]
fn history_lifecycle_uses_recorded_kind_not_arbitrary_payload_status() {
    for (kind, status) in [
        ("run_created", "running"),
        ("run_resumed", "running"),
        ("run_paused", "paused"),
        ("run_completed", "completed"),
        ("run_cancelled", "cancelled"),
    ] {
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            run_id: "run".to_owned(),
            event_seq: 1,
            event_type: kind.to_owned(),
            payload: serde_json::json!({"status": "SECRET", "input": "SECRET"}),
            created_at_ms: 2,
        });
        assert_eq!(event.payload, serde_json::json!({"status": status}));
    }
    assert!(run_lifecycle_observation("future_transition").is_none());
}

#[cfg(test)]
#[test]
fn history_output_validation_omits_value_and_artifact_location() {
    for artifact in [None, Some("SECRET".to_owned())] {
        let output = bcode_workflow_store::ValidatedOutput {
            output_id: "output".to_owned(),
            run_id: "run".to_owned(),
            node_id: "node".to_owned(),
            activation_id: "activation".to_owned(),
            schema_id: "schema".to_owned(),
            schema_version: 1,
            value: serde_json::json!({"private": "SECRET"}),
            artifact_reference: artifact.clone(),
            created_at_ms: 42,
        };
        let observed = output_validation_observation(&serde_json::to_value(output).unwrap());
        assert_eq!(
            observed,
            serde_json::json!({"schema_version": 1, "has_artifact": artifact.is_some(), "created_at_ms": 42})
        );
    }
    for payload in [
        serde_json::Value::Null,
        serde_json::json!({"schema_version": 1, "artifact_reference": 123, "created_at_ms": 42}),
    ] {
        assert_eq!(
            output_validation_observation(&payload),
            serde_json::json!({"unavailable": "invalid_output_validation"})
        );
    }
}

#[cfg(test)]
#[test]
fn history_wait_resolution_rejects_unknown_kind_and_preserves_denial() {
    for (kind, accepted) in [("approval", false), ("input", true), ("future", true)] {
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 1,
            run_id: "run".to_owned(),
            event_type: "waiting_activation_resolved".to_owned(),
            payload: serde_json::json!({"kind": kind, "accepted": accepted, "input": "SECRET"}),
            created_at_ms: 2,
        });
        if kind == "future" {
            assert_eq!(
                event.payload,
                serde_json::json!({"unavailable": "invalid_wait_resolution"})
            );
        } else {
            assert_eq!(
                event.payload,
                serde_json::json!({"kind": kind, "accepted": accepted})
            );
        }
    }
}

#[cfg(test)]
#[test]
fn history_admission_excludes_receipt_and_rejects_zero_attempt() {
    for attempt in [0, 1, 2] {
        let receipt = bcode_workflow_store::DispatchReceipt {
            run_id: "run".to_owned(),
            node_id: "node".to_owned(),
            activation_id: "activation".to_owned(),
            attempt,
            dispatch_identity: "identity".to_owned(),
            receipt: serde_json::json!({"private": "SECRET"}),
            admitted_at_ms: 42,
        };
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            run_id: "run".to_owned(),
            event_seq: 3,
            event_type: "attempt_admitted".to_owned(),
            payload: serde_json::to_value(receipt).unwrap(),
            created_at_ms: 42,
        });
        assert!(!event.payload.to_string().contains("SECRET"));
        if attempt == 0 {
            assert!(event.payload.get("unavailable").is_some());
        } else {
            assert_eq!(
                event.payload,
                serde_json::json!({"attempt": attempt, "admitted_at_ms": 42})
            );
        }
    }
}

#[cfg(test)]
fn assert_mutation_approval_history(private: &bcode_workflow::WorkflowMutationApproval) {
    for (status, approved) in [("approved", true), ("denied", false)] {
        let decision = bcode_workflow::WorkflowDecision {
            decision_id: format!("mutation-approval:{}", private.approval_id),
            run_id: private.run_id.clone(),
            node_id: Some(private.node_id.clone()),
            decision_type: "mutation_approval".to_owned(),
            value: serde_json::json!({"approval_id":private.approval_id,"decision":status,"scope":private.scope}),
            created_at_ms: 9,
        };
        let view = decision_inspection(decision.clone());
        assert_eq!(
            view.value,
            bcode_workflow::WorkflowDecisionValueDisclosure::MutationApproval {
                approval_id: private.approval_id.clone(),
                approved
            }
        );
        assert!(
            !serde_json::to_string(&view)
                .expect("wire")
                .contains("PRIVATE_FACT")
        );
        let event = bcode_workflow_store::WorkflowEventRow {
            event_seq: 1,
            run_id: decision.run_id.clone(),
            event_type: format!("mutation_approval_{status}"),
            payload: decision.value.clone(),
            created_at_ms: decision.created_at_ms,
        };
        let history = history_event(event.clone());
        assert_eq!(
            history.payload["resolution"],
            serde_json::to_value(&view.value).expect("outcome")
        );
        assert!(!history.payload.to_string().contains("PRIVATE_FACT"));
        if !approved {
            assert_eq!(history.payload["reason"], "mutation_approval_denied");
        }
        let mut conflicting = event;
        conflicting.event_type = format!(
            "mutation_approval_{}",
            if approved { "denied" } else { "approved" }
        );
        assert!(
            history_event(conflicting)
                .payload
                .get("resolution")
                .is_none()
        );
        let mut invalid = decision;
        invalid.value["scope"]["run_id"] = serde_json::json!("foreign");
        assert_eq!(
            decision_inspection(invalid).value,
            bcode_workflow::WorkflowDecisionValueDisclosure::Withheld
        );
    }
    let row = bcode_workflow_store::WorkflowEventRow {
        event_seq: 1,
        run_id: private.run_id.clone(),
        event_type: "mutation_approval_requested".to_owned(),
        payload: serde_json::to_value(private).expect("private event"),
        created_at_ms: private.requested_at_ms,
    };
    let observed_event = history_event(row.clone());
    let review: bcode_workflow::WorkflowMutationApprovalInspection =
        serde_json::from_value(observed_event.payload.clone()).expect("history review consumer");
    assert_eq!(review, mutation_approval_inspection(private.clone()));
    assert!(!observed_event.payload.to_string().contains("PRIVATE_FACT"));
    for key in ["run_id", "node_id", "activation_id"] {
        let mut invalid = row.clone();
        invalid.payload["scope"][key] = serde_json::json!("foreign");
        assert_eq!(
            history_event(invalid).payload,
            serde_json::json!({"unavailable":"invalid_mutation_approval"})
        );
    }
    let mut invalid = row;
    invalid.payload["scope"]["version"] = serde_json::json!(999);
    assert_eq!(
        history_event(invalid).payload,
        serde_json::json!({"unavailable":"invalid_mutation_approval"})
    );
}

#[cfg(test)]
#[test]
fn mutation_approval_projection_preserves_review_content_not_authorization_facts() {
    let private = bcode_workflow::WorkflowMutationApproval {
        approval_id: "approval".to_owned(),
        run_id: "run".to_owned(),
        node_id: "node".to_owned(),
        activation_id: "activation".to_owned(),
        requested_at_ms: 7,
        expires_at_ms: Some(10),
        scope: bcode_workflow::WorkflowMutationGrantScope {
            version: bcode_workflow::WORKFLOW_MUTATION_GRANT_SCOPE_VERSION,
            definition_id: "definition".to_owned(),
            definition_version: 1,
            run_id: "run".to_owned(),
            node_id: "node".to_owned(),
            activation_id: "activation".to_owned(),
            workspace_snapshot: "/workspace".to_owned(),
            plugin_id: "owner".to_owned(),
            block_id: "write".to_owned(),
            block_version: 1,
            operation: "write".to_owned(),
            input_checksum_sha256: "a".repeat(64),
            operation_facts: Some(serde_json::json!({"credential": "PRIVATE_FACT"})),
            preparation_descriptor_sha256: Some("b".repeat(64)),
            input_summary: serde_json::json!({"path": "review.txt"}),
            resource_claims: vec![],
            reconciliation: bcode_workflow::WorkflowBlockReconciliation::RepairRequired,
            capability: bcode_workflow::WorkflowToolCapability::Mutating,
        },
    };
    private.scope.validate().expect("valid private scope");
    assert_mutation_approval_history(&private);
    let grant = bcode_workflow::WorkflowGrant {
        grant_id: "grant".to_owned(),
        run_id: private.run_id.clone(),
        node_id: private.node_id.clone(),
        scope: serde_json::to_value(&private.scope).expect("scope"),
        granted_at_ms: 8,
        expires_at_ms: Some(10),
        max_uses: Some(1),
        uses_consumed: 0,
    };
    let grant_view = grant_inspection(grant.clone());
    let bcode_workflow::WorkflowGrantScopeDisclosure::Mutation { scope } = &grant_view.scope else {
        panic!("valid mutation scope must have review content");
    };
    assert_eq!(scope.input_summary, private.scope.input_summary);
    assert_eq!(scope.operation, private.scope.operation);
    let grant_wire = serde_json::to_value(&grant_view).expect("grant wire");
    assert!(!grant_wire.to_string().contains("PRIVATE_FACT"));
    assert_eq!(
        serde_json::from_value::<bcode_workflow::WorkflowGrantInspection>(grant_wire)
            .expect("grant consumer"),
        grant_view
    );
    for (key, value) in [
        ("version", serde_json::json!(999)),
        ("run_id", serde_json::json!("foreign")),
        ("node_id", serde_json::json!("foreign")),
        ("input_checksum_sha256", serde_json::json!("invalid")),
    ] {
        let mut invalid = grant.clone();
        invalid.scope[key] = value;
        assert_eq!(
            grant_inspection(invalid).scope,
            bcode_workflow::WorkflowGrantScopeDisclosure::Withheld
        );
    }
    let observed = mutation_approval_inspection(private.clone());
    assert_eq!(observed.approval_id, private.approval_id);
    assert_eq!(observed.run_id, private.run_id);
    assert_eq!(observed.node_id, private.node_id);
    assert_eq!(observed.activation_id, private.activation_id);
    assert_eq!(observed.requested_at_ms, private.requested_at_ms);
    assert_eq!(observed.expires_at_ms, private.expires_at_ms);
    assert_eq!(observed.scope.input_summary, private.scope.input_summary);
    assert_eq!(observed.scope.reconciliation, private.scope.reconciliation);
    assert_eq!(observed.scope.capability, private.scope.capability);
    let wire = serde_json::to_value(&observed).expect("public wire");
    assert!(wire["scope"].get("operation_facts").is_none());
    assert!(wire["scope"].get("preparation_descriptor_sha256").is_none());
    assert!(wire["scope"].get("input_checksum_sha256").is_none());
    assert!(!wire.to_string().contains("PRIVATE_FACT"));
    let decoded: bcode_workflow::WorkflowMutationApprovalInspection =
        serde_json::from_value(wire.clone()).expect("public consumer");
    assert_eq!(decoded, observed);
    assert!(serde_json::from_value::<bcode_workflow::WorkflowMutationApproval>(wire).is_err());
}

#[cfg(test)]
#[test]
fn policy_grant_inspection_discloses_only_known_correlated_scopes() {
    let mut grant = bcode_workflow::WorkflowGrant {
        grant_id: "grant".to_owned(),
        run_id: "run".to_owned(),
        node_id: "node".to_owned(),
        scope: serde_json::json!({"grant_id":"grant", "capability":"mutating", "scope": {
            "definition":"definition", "definition_version":1, "workspace":"workspace", "node":"node", "run":"run"
        }}),
        granted_at_ms: 1,
        expires_at_ms: None,
        max_uses: Some(1),
        uses_consumed: 0,
    };
    let view = grant_inspection(grant.clone());
    assert!(matches!(
        view.scope,
        bcode_workflow::WorkflowGrantScopeDisclosure::Policy { .. }
    ));
    let wire = serde_json::to_value(&view).expect("wire");
    assert_eq!(wire["scope"]["capability"], "mutating");
    assert_eq!(
        serde_json::from_value::<bcode_workflow::WorkflowGrantInspection>(wire).expect("consumer"),
        view
    );
    for (key, value) in [
        ("grant_id", serde_json::json!("foreign")),
        ("version", serde_json::json!(999)),
        ("extension", serde_json::json!(true)),
    ] {
        let mut invalid = grant.clone();
        invalid.scope[key] = value;
        assert_eq!(
            grant_inspection(invalid).scope,
            bcode_workflow::WorkflowGrantScopeDisclosure::Withheld
        );
    }
    for (key, value) in [
        ("run", serde_json::json!("foreign")),
        ("node", serde_json::json!("foreign")),
        ("definition_version", serde_json::json!(0)),
        ("workspace", serde_json::json!("")),
        ("extension", serde_json::json!(true)),
    ] {
        let mut invalid = grant.clone();
        invalid.scope["scope"][key] = value;
        assert_eq!(
            grant_inspection(invalid).scope,
            bcode_workflow::WorkflowGrantScopeDisclosure::Withheld
        );
    }
    grant.scope["scope"]
        .as_object_mut()
        .expect("scope")
        .remove("run");
    assert!(matches!(
        grant_inspection(grant).scope,
        bcode_workflow::WorkflowGrantScopeDisclosure::Policy { .. }
    ));
}

#[cfg(test)]
#[test]
fn history_attempt_correlation_uses_verified_coordinates_and_withholds_foreign_links() {
    let attempt = bcode_workflow::AttemptSummary {
        run_id: "run".to_owned(),
        node_id: "node".to_owned(),
        activation_id: "activation".to_owned(),
        attempt: 2,
        dispatch_identity: "a".repeat(64),
        side_effect: bcode_workflow::DispatchSideEffect::ReadOnly,
        status: "prepared".to_owned(),
        has_receipt: false,
        prepared_at_ms: 1,
        admitted_at_ms: None,
        terminal_at_ms: None,
    };
    for (candidate, expected) in [
        (Some(attempt.clone()), true),
        (None, false),
        (
            Some(bcode_workflow::AttemptSummary {
                run_id: "foreign".to_owned(),
                ..attempt
            }),
            false,
        ),
    ] {
        let mut event = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 1,
            run_id: "run".to_owned(),
            event_type: "attempt_failed".to_owned(),
            payload: serde_json::json!({"dispatch_identity": "a".repeat(64), "node_id": "untrusted", "attempt_correlation": {"node_id":"untrusted"}}),
            created_at_ms: 3,
        });
        correlate_history_attempt(&mut event, candidate);
        assert_eq!(event.payload.get("dispatch_identity").is_some(), expected);
        assert_eq!(event.payload.get("attempt_correlation").is_some(), expected);
        assert_eq!(
            event.payload.get("correlation_unavailable").is_some(),
            !expected
        );
        assert!(!event.payload.to_string().contains("untrusted"));
        if expected {
            let correlation: bcode_workflow::WorkflowHistoryAttemptCorrelation =
                serde_json::from_value(event.payload["attempt_correlation"].clone())
                    .expect("typed correlation");
            assert_eq!(correlation.node_id, "node");
            assert_eq!(correlation.activation_id, "activation");
            assert_eq!(correlation.attempt, 2);
        }
    }
}

#[cfg(test)]
#[test]
fn activation_history_preserves_wait_kind_and_rejects_inconsistent_coordinates() {
    for (kind, status, expected) in [
        ("activation_created", "pending", None),
        (
            "activation_waiting",
            "waiting_input",
            Some(bcode_workflow::WorkflowWaitKind::Input),
        ),
        (
            "activation_waiting",
            "waiting_approval",
            Some(bcode_workflow::WorkflowWaitKind::Approval),
        ),
    ] {
        let row = bcode_workflow_store::WorkflowEventRow {
            event_seq: 1,
            run_id: "run".to_owned(),
            event_type: kind.to_owned(),
            created_at_ms: 1,
            payload: serde_json::json!({"status": status, "activation": {
                "run_id":"run", "node_id":"node", "activation_id": bcode_workflow_store::activation_identity("run", "node", 1),
                "dependency_generation":1, "created_at_ms":1, "input":{"private":"SECRET"}
            }}),
        };
        let event = history_event(row.clone());
        let observation: bcode_workflow::WorkflowActivationObservation =
            serde_json::from_value(event.payload.clone()).expect("observation");
        assert_eq!(observation.wait_kind, expected);
        assert_eq!(observation.waiting, expected.is_some());
        assert!(!event.payload.to_string().contains("SECRET"));
        for (key, value) in [
            ("run_id", serde_json::json!("foreign")),
            ("activation_id", serde_json::json!("wrong")),
            ("dependency_generation", serde_json::json!(2)),
            ("created_at_ms", serde_json::json!(-1)),
        ] {
            let mut invalid = row.clone();
            invalid.payload["activation"][key] = value;
            assert_eq!(
                history_event(invalid).payload,
                serde_json::json!({"unavailable":"invalid_activation"})
            );
        }
        let mut invalid = row;
        invalid.payload["status"] = serde_json::json!(if expected.is_some() {
            "pending"
        } else {
            "waiting_input"
        });
        assert_eq!(
            history_event(invalid).payload,
            serde_json::json!({"unavailable":"invalid_activation"})
        );
    }
}

#[cfg(test)]
#[test]
fn history_preparation_exposes_facts_without_dispatch_intent() {
    let row = bcode_workflow_store::WorkflowEventRow {
        event_seq: 1,
        run_id: "run".to_owned(),
        event_type: "attempt_prepared".to_owned(),
        payload: serde_json::json!({"attempt":2, "side_effect":"mutating", "prepared_at_ms":7, "intent":{"credential":"SECRET"}}),
        created_at_ms: 7,
    };
    let observed = history_event(row.clone());
    assert_eq!(
        observed.payload,
        serde_json::json!({"attempt":2,"side_effect":"mutating","prepared_at_ms":7})
    );
    let typed: bcode_workflow::WorkflowAttemptPreparationObservation =
        serde_json::from_value(observed.payload).expect("public observation");
    assert_eq!(typed.attempt, 2);
    for (key, value) in [
        ("attempt", serde_json::json!(0)),
        ("attempt", serde_json::json!(u64::MAX)),
        ("side_effect", serde_json::json!("future")),
        ("prepared_at_ms", serde_json::json!(-1)),
    ] {
        let mut invalid = row.clone();
        invalid.payload[key] = value;
        assert_eq!(
            history_event(invalid).payload,
            serde_json::json!({"unavailable":"invalid_attempt_preparation"})
        );
    }
}

#[cfg(test)]
#[test]
fn aggregate_decision_projection_withholds_private_value() {
    let observed = decision_inspection(bcode_workflow::WorkflowDecision {
        decision_id: "decision".to_owned(),
        run_id: "run".to_owned(),
        node_id: Some("node".to_owned()),
        decision_type: "branch".to_owned(),
        value: serde_json::json!({"private": "SECRET"}),
        created_at_ms: 7,
    });
    let wire = serde_json::to_value(observed).unwrap();
    assert_eq!(wire["decision_id"], "decision");
    assert_eq!(wire["decision_type"], "branch");
    assert_eq!(wire["created_at_ms"], 7);
    assert_eq!(
        wire["value"],
        serde_json::json!({"availability": "withheld"})
    );
    assert!(!wire.to_string().contains("SECRET"));
}

#[cfg(test)]
#[test]
fn aggregate_grant_projection_preserves_identity_and_withholds_scope() {
    let observed = grant_inspection(bcode_workflow::WorkflowGrant {
        grant_id: "grant".to_owned(),
        run_id: "run".to_owned(),
        node_id: "node".to_owned(),
        scope: serde_json::json!({"operation_facts": "SECRET", "input_summary": "SECRET"}),
        granted_at_ms: 1,
        expires_at_ms: Some(10),
        max_uses: Some(3),
        uses_consumed: 2,
    });
    assert_eq!(observed.grant_id, "grant");
    assert_eq!(observed.run_id, "run");
    assert_eq!(observed.node_id, "node");
    assert_eq!(observed.uses_consumed, 2);
    let wire = serde_json::to_value(observed).unwrap();
    assert_eq!(
        wire["scope"],
        serde_json::json!({"availability": "withheld"})
    );
    assert!(!wire.to_string().contains("SECRET"));
}

#[cfg(test)]
#[test]
fn history_grant_use_preserves_limits_without_scope() {
    for kind in ["grant_recorded", "grant_consumed"] {
        for limit in [None, Some(3)] {
            let grant = bcode_workflow::WorkflowGrant {
                grant_id: "grant".to_owned(),
                run_id: "run".to_owned(),
                node_id: "node".to_owned(),
                scope: serde_json::json!({"credential": "SECRET"}),
                granted_at_ms: 2,
                expires_at_ms: Some(10),
                max_uses: limit,
                uses_consumed: 2,
            };
            let event = history_event(bcode_workflow_store::WorkflowEventRow {
                event_seq: 3,
                run_id: "run".to_owned(),
                event_type: kind.to_owned(),
                payload: serde_json::to_value(grant).unwrap(),
                created_at_ms: 4,
            });
            assert!(!event.payload.to_string().contains("SECRET"));
            let observed: bcode_workflow::WorkflowGrantUseObservation =
                serde_json::from_value(event.payload).unwrap();
            assert_eq!(observed.max_uses, limit);
            assert_eq!(observed.uses_consumed, 2);
            assert_eq!(observed.expires_at_ms, Some(10));
        }
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 3,
            run_id: "run".to_owned(),
            event_type: kind.to_owned(),
            payload: serde_json::json!({"granted_at_ms": 2, "max_uses": 1, "uses_consumed": 2}),
            created_at_ms: 4,
        });
        assert_eq!(
            event.payload,
            serde_json::json!({"unavailable": "invalid_grant_use"})
        );
    }
}

#[cfg(test)]
#[test]
fn history_composition_projects_progress_without_private_content() {
    for (kind, facts) in [
        (
            "fan_out_materialized",
            serde_json::json!({"member_count": 8, "max_concurrency": 3}),
        ),
        (
            "control_node_settled",
            serde_json::json!({"repeat": false, "generation": 4,
            "iterations_completed": 5, "next_generation": null, "max_iterations": 5,
            "cycle_cap": 10, "effective_iteration_bound": 5, "iteration_bound_exhausted": true,
            "exhaustion_policy": "fail", "outcome": null}),
        ),
    ] {
        let mut payload = facts.clone();
        payload["private"] = serde_json::json!({"token": "SECRET"});
        let event = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 11,
            run_id: "run".to_owned(),
            event_type: kind.to_owned(),
            payload,
            created_at_ms: 12,
        });
        assert_eq!(event.payload, facts);
        let invalid = history_event(bcode_workflow_store::WorkflowEventRow {
            event_seq: 11,
            run_id: "run".to_owned(),
            event_type: kind.to_owned(),
            payload: serde_json::json!({"private": "SECRET"}),
            created_at_ms: 12,
        });
        assert!(invalid.payload.get("unavailable").is_some());
        assert!(!invalid.payload.to_string().contains("SECRET"));
    }
}

#[cfg(test)]
#[test]
fn history_failure_preserves_digest_correlation_only() {
    let identity = bcode_workflow_store::dispatch_identity("run", "node", "activation", 1);
    let event = history_event(bcode_workflow_store::WorkflowEventRow {
        event_seq: 1,
        run_id: "run".to_owned(),
        event_type: "attempt_failed".to_owned(),
        payload: serde_json::json!({"dispatch_identity": identity, "message": "SECRET"}),
        created_at_ms: 2,
    });
    assert_eq!(event.payload["dispatch_identity"], identity);
    assert_eq!(event.payload["reason"], "attempt_failed");
    assert!(event.payload.get("message").is_none());
}

#[cfg(test)]
#[test]
fn history_observation_withholds_unreviewed_payload_and_preserves_cursor() {
    let row = bcode_workflow_store::WorkflowEventRow {
        event_seq: 17,
        run_id: "run-history".to_owned(),
        event_type: "future_diagnostic".to_owned(),
        payload: serde_json::json!({"unknown": {"number": 42, "text": "diagnostic"}}),
        created_at_ms: 123,
    };
    let observation = history_event(row);
    assert_eq!(observation.event_seq, 17);
    assert_eq!(observation.run_id, "run-history");
    assert_eq!(observation.created_at_ms, 123);
    assert_eq!(observation.event_type, "future_diagnostic");
    assert_eq!(
        observation.payload,
        serde_json::json!({"unavailable": "unreviewed_event_payload"})
    );
}

/// Return bounded keyset-paged diagnostic event observations for one workflow run.
///
/// # Errors
/// Returns store availability, query, or decoding errors without repairing history.
pub fn event_history(
    state: &ServerState,
    run_id: &str,
    after_sequence: Option<u64>,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowHistoryEvent>, bcode_workflow_store::WorkflowStoreError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store
        .event_history(run_id, after_sequence, limit)?
        .into_iter()
        .map(|event| verified_history_event(&store, event))
        .collect()
}

/// Return bounded workflow live-event catch-up state without transport framing.
pub fn live_event_catch_up(
    state: &ServerState,
    after_sequence: u64,
    limit: usize,
) -> Result<
    bcode_workflow_view_models::WorkflowLiveEventPage,
    bcode_workflow_store::WorkflowStoreError,
> {
    if limit == 0 || limit > 1_000 {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow live catch-up limit must be in 1..=1000".to_string(),
        ));
    }
    let (positions, latest_sequence) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        (
            store.event_positions_after(after_sequence, limit)?,
            store.latest_global_event_sequence()?,
        )
    };
    let resync_required = positions
        .last()
        .is_some_and(|(event_sequence, _, _)| *event_sequence < latest_sequence);
    let events = positions
        .into_iter()
        .map(|(event_sequence, run_id, changed_at_ms)| {
            bcode_workflow_view_models::WorkflowLiveEvent {
                version: bcode_workflow_view_models::WORKFLOW_LIVE_EVENT_VERSION,
                run_id,
                event_sequence,
                changed_at_ms,
            }
        })
        .collect();
    Ok(bcode_workflow_view_models::WorkflowLiveEventPage {
        events,
        resync_required,
    })
}

/// Return one workflow run summary without transport framing.
pub fn run_status(
    state: &ServerState,
    run_id: &str,
) -> Result<
    Option<bcode_workflow_store::WorkflowRunSummary>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .run_summary(run_id)
}

fn binding_key(
    key: bcode_workflow::WorkflowRunBindingLookup,
) -> bcode_workflow_store::WorkflowRunBindingKey {
    bcode_workflow_store::WorkflowRunBindingKey {
        owner_plugin_id: key.owner_plugin_id,
        workflow_kind: key.workflow_kind,
        scope_key: key.scope_key,
    }
}

/// Return the workflow run associated with one plugin-owned binding.
pub fn associated_run(
    state: &ServerState,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
) -> Result<
    Option<bcode_workflow_store::WorkflowRunSummary>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .associated_run(key)
}

/// Inspect the workflow run associated with one plugin-owned binding.
pub async fn inspect_associated_run(
    state: &ServerState,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
    limit: usize,
) -> Result<Option<Box<bcode_workflow::WorkflowRunInspection>>, super::ServerError> {
    let Some(run) = associated_run(state, key)? else {
        return Ok(None);
    };
    Ok(Some(Box::new(
        inspect_run(state, &run.run_id, limit).await?,
    )))
}

/// Control the workflow run associated with one plugin-owned binding.
pub async fn control_associated_run(
    state: &std::sync::Arc<ServerState>,
    key: &bcode_workflow_store::WorkflowRunBindingKey,
    action: bcode_workflow::WorkflowRunControlAction,
) -> Result<(Option<bcode_workflow_store::WorkflowRunSummary>, bool), super::ServerError> {
    let run = associated_run(state, key)?;
    let changed = if let Some(run) = &run {
        match action {
            bcode_workflow::WorkflowRunControlAction::Pause => {
                pause_run(state, &run.run_id).await?
            }
            bcode_workflow::WorkflowRunControlAction::Resume => {
                resume_run(state, &run.run_id).await?
            }
            bcode_workflow::WorkflowRunControlAction::Cancel => {
                let _authority =
                    execution_authority(state, &run.run_id)
                        .await?
                        .ok_or_else(|| {
                            bcode_workflow_store::WorkflowStoreError::InvalidData(
                                "active workflow has no durable execution authority".to_string(),
                            )
                        })?;
                let (recorded, attempts) = {
                    let mut store = state
                        .workflow_store
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let recorded =
                        store.request_cancellation(&run.run_id, super::current_unix_millis())?;
                    let attempts = store.active_attempt_cancellations(&run.run_id, 1_000)?;
                    drop(store);
                    (recorded, attempts)
                };
                super::propagate_persisted_workflow_cancellation(state, attempts).await?;
                // A quiescent run cancels immediately; settle its runtime work so the daemon does
                // not keep a phantom registration for a run that is already terminal.
                super::settle_workflow_runtime_work(state, &run.run_id).await?;
                recorded
            }
        }
    } else {
        false
    };
    Ok((associated_run(state, key)?, changed))
}

/// Apply one explicit repair resolution to an exact workflow attempt.
pub fn repair_attempt(
    state: &ServerState,
    dispatch_identity: &str,
    resolution: &bcode_workflow_store::RepairResolution,
) -> Result<bcode_workflow_store::RepairResult, bcode_workflow_store::WorkflowStoreError> {
    let started_at = std::time::Instant::now();
    let result = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .repair_attempt(dispatch_identity, resolution, super::current_unix_millis())?;
    let resolution_label = match resolution {
        bcode_workflow_store::RepairResolution::ConfirmSucceeded { .. } => "confirm_succeeded",
        bcode_workflow_store::RepairResolution::ConfirmFailed { .. } => "confirm_failed",
        bcode_workflow_store::RepairResolution::ConfirmCancelled { .. } => "confirm_cancelled",
        bcode_workflow_store::RepairResolution::AbandonForExplicitRetry { .. } => {
            "abandon_for_explicit_retry"
        }
    };
    state.metrics.record_histogram_with_labels(
        "workflow.reconciliation.duration_ms",
        u64::try_from(started_at.elapsed().as_millis()).unwrap_or(u64::MAX),
        std::collections::BTreeMap::from([(
            "resolution".to_string(),
            resolution_label.to_string(),
        )]),
    );
    Ok(result)
}

/// Project a bounded workflow catalog page without transport framing.
pub fn catalog_view(
    state: &ServerState,
    request: &bcode_workflow_view_models::WorkflowCatalogRequest,
) -> Result<bcode_workflow_view_models::WorkflowCatalogView, bcode_workflow_store::WorkflowStoreError>
{
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let query = bcode_workflow_store::WorkflowRunCatalogQuery {
        limit: request.limit,
        cursor: request.cursor.as_ref().map(|cursor| {
            bcode_workflow_store::WorkflowRunCatalogCursor {
                sort: match cursor.sort {
                    bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt => {
                        bcode_workflow_store::WorkflowRunCatalogSort::UpdatedAt
                    }
                    bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt => {
                        bcode_workflow_store::WorkflowRunCatalogSort::CreatedAt
                    }
                    bcode_workflow_view_models::WorkflowCatalogSort::Status => {
                        bcode_workflow_store::WorkflowRunCatalogSort::Status
                    }
                },
                timestamp_ms: cursor.timestamp_ms,
                status_rank: cursor.status_rank,
                run_id: cursor.run_id.clone(),
            }
        }),
        filter: match request.filter {
            bcode_workflow_view_models::WorkflowCatalogFilter::Active => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Active
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::NeedsAttention => {
                bcode_workflow_store::WorkflowRunCatalogFilter::NeedsAttention
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::Failed => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Failed
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::Completed => {
                bcode_workflow_store::WorkflowRunCatalogFilter::Completed
            }
            bcode_workflow_view_models::WorkflowCatalogFilter::All => {
                bcode_workflow_store::WorkflowRunCatalogFilter::All
            }
        },
        sort: match request.sort {
            bcode_workflow_view_models::WorkflowCatalogSort::UpdatedAt => {
                bcode_workflow_store::WorkflowRunCatalogSort::UpdatedAt
            }
            bcode_workflow_view_models::WorkflowCatalogSort::CreatedAt => {
                bcode_workflow_store::WorkflowRunCatalogSort::CreatedAt
            }
            bcode_workflow_view_models::WorkflowCatalogSort::Status => {
                bcode_workflow_store::WorkflowRunCatalogSort::Status
            }
        },
        search: request.search.clone(),
    };
    let page = store.workflow_run_catalog_page(&query)?;
    let items = page
        .entries
        .iter()
        .map(|entry| run_list_item_with_summary(&store, &entry.run, &entry.summary))
        .collect::<Result<Vec<_>, _>>()?;
    drop(store);
    Ok(bcode_workflow_view::project_catalog(
        items,
        request,
        page.has_more,
    ))
}

#[allow(clippy::too_many_lines)]
pub async fn apply_source(
    client_id: super::ClientId,
    state: &std::sync::Arc<ServerState>,
    request: bcode_workflow::ApplyWorkflowSourceRequest,
) -> Result<bcode_workflow::WorkflowSourceApplyResult, super::ServerError> {
    let catalog = authoring_catalog(state).await?;
    let lowering = bcode_workflow::lower_workflow_authoring_source(
        &request.source,
        request.source_format,
        &catalog,
    )?;
    let source_map = lowering.source_map;
    let document = lowering.document;
    let mut validation = lowering.validation;
    validation.diagnostics = source_map.remap_diagnostics(&validation.diagnostics);
    let source_profile = lowering.profile;
    document.validate()?;
    let preview = document.compilation_preview(&catalog, None);
    let compiled = preview.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "source apply requires a successful canonical compilation preview".to_string(),
        )
    })?;
    let requirements = compiled.requirements.clone();
    let effects = compiled.effects.clone();
    let permissions = compiled.permissions.clone();
    let workflow_id = document.workflow_id.clone();
    let canonical_digest_sha256 = document.source_digest_sha256()?;
    let existing = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .authored_workflow(&workflow_id)?;
    let (outcome, generation) = if existing.is_none() {
        let producer = document.producer.clone();
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
                workflow_id: workflow_id.clone(),
                draft_id: None,
                revision: None,
                preset_id: None,
                producer: Some(producer.clone()),
                requirements: document.requirements.clone(),
                effects: bcode_workflow::WorkflowEffectSummary::default(),
                activates: false,
                executes: false,
            },
        )?;
        let now = super::current_time_ms();
        let workflow = bcode_workflow_store::AuthoredWorkflow {
            workflow_id: workflow_id.clone(),
            title: document.metadata.title.clone(),
            description: document.metadata.description.clone(),
            archived: false,
            active_revision: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let draft = bcode_workflow_store::WorkflowDraft {
            workflow_id: workflow_id.clone(),
            draft_id: request.draft_id.clone(),
            base_revision: None,
            generation: 1,
            checksum_sha256: canonical_digest_sha256.clone(),
            document,
            producer,
            created_at_ms: now,
            updated_at_ms: now,
        };
        state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
        (bcode_workflow::WorkflowSourceApplyOutcome::Created, 1)
    } else {
        let current = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .workflow_draft(&workflow_id, &request.draft_id)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "source draft not found: {workflow_id}/{}",
                    request.draft_id
                ))
            })?;
        state.authorize_local_workflow_application_operation(
            client_id,
            LocalApplicationOperationRequest {
                operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
                workflow_id: workflow_id.clone(),
                draft_id: Some(request.draft_id.clone()),
                revision: None,
                preset_id: None,
                producer: Some(document.producer.clone()),
                requirements: document.requirements.clone(),
                effects: bcode_workflow::WorkflowEffectSummary::default(),
                activates: false,
                executes: false,
            },
        )?;
        let expected_generation = current.generation;
        let update = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .update_workflow_draft(
                &workflow_id,
                &request.draft_id,
                expected_generation,
                &document,
                &document.producer,
                super::current_time_ms(),
            );
        match update {
            Ok(draft) => (
                bcode_workflow::WorkflowSourceApplyOutcome::Updated,
                draft.generation,
            ),
            Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
                expected,
                current,
                ..
            }) => (
                bcode_workflow::WorkflowSourceApplyOutcome::Conflict {
                    expected_generation: expected,
                    current_generation: current,
                },
                current,
            ),
            Err(error) => return Err(error.into()),
        }
    };
    Ok(bcode_workflow::WorkflowSourceApplyResult {
        version: bcode_workflow::WORKFLOW_SOURCE_APPLY_RESULT_VERSION,
        source_format: request.source_format,
        source_profile,
        workflow_id,
        draft_id: request.draft_id,
        generation,
        canonical_digest_sha256,
        validation,
        source_map,
        requirements,
        effects,
        permissions,
        outcome,
    })
}

/// Register one connection-scoped workflow event consumer and return its snapshot boundary.
///
/// The caller owns transport framing and response encoding; the supplied sink owns bounded event
/// delivery and connection lifecycle.
pub async fn subscribe_runs(
    state: &ServerState,
    client_id: super::ClientId,
    event_sink: ClientEventSink,
) -> Result<u64, bcode_workflow_store::WorkflowStoreError> {
    // Hold the subscriber lock across the bounded watermark read and registration.
    // A publisher cannot snapshot subscribers between these operations, and a failed
    // read leaves any existing registration untouched.
    let mut clients = state.workflow_event_clients.lock().await;
    let after_sequence = latest_event_sequence(state)?;
    clients.insert(client_id, event_sink);
    drop(clients);
    Ok(after_sequence)
}

/// Return the current global workflow-event sequence for subscription setup.
pub fn latest_event_sequence(
    state: &ServerState,
) -> Result<u64, bcode_workflow_store::WorkflowStoreError> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .latest_global_event_sequence()
}

/// Validate and persist one exact workflow definition without transport framing.
pub fn register_definition(
    state: &ServerState,
    request: &bcode_ipc::WorkflowDefinitionRegistrationRequest,
) -> Result<bcode_workflow_store::StoredWorkflowDefinition, super::ServerError> {
    validate_workflow_definition_for_production(state, &request.definition)?;
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .persist_definition(&request.definition_id, request.version, &request.definition)
        .map_err(super::ServerError::from)
}

/// Build the validated workflow authoring catalog without transport framing.
pub async fn authoring_catalog(
    state: &ServerState,
) -> Result<bcode_workflow::WorkflowAuthoringCatalogSnapshot, super::ServerError> {
    let plugins = state
        .plugins
        .registry()
        .manifests()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let blocks = state
        .plugins
        .registry()
        .workflow_blocks()
        .into_iter()
        .map(|block| (bcode_workflow::workflow_block_catalog_key(&block), block))
        .collect::<std::collections::BTreeMap<_, _>>();
    let authoring_actions = state
        .plugins
        .registry()
        .workflow_authoring_actions()
        .into_iter()
        .map(|action| (action.catalog_key(), action))
        .collect::<std::collections::BTreeMap<_, _>>();
    let profiles = super::list_profiles(state, None)
        .await
        .into_iter()
        .flat_map(|agent| std::iter::once(agent.id).chain(agent.aliases))
        .collect::<std::collections::BTreeSet<_>>();
    let workflow_definitions = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        store
            .list_definitions(1_000)?
            .into_iter()
            .map(|stored| {
                let definition: bcode_workflow::WorkflowDefinition =
                    serde_json::from_str(&stored.definition_json)?;
                Ok((stored.definition_id, definition))
            })
            .collect::<Result<
                std::collections::BTreeMap<_, _>,
                bcode_workflow_store::WorkflowStoreError,
            >>()?
    };
    let catalog = bcode_workflow::WorkflowAuthoringCatalogSnapshot {
        version: bcode_workflow::WORKFLOW_AUTHORING_CATALOG_VERSION,
        capabilities: bcode_workflow::WorkflowAuthoringCapabilitySummary::from(
            &bcode_workflow::WorkflowProductionCapabilities::current(),
        ),
        plugins,
        blocks,
        node_configuration_schemas: bcode_workflow::workflow_node_configuration_schemas(),
        workflow_definitions,
        agent_profiles: profiles,
        authoring_actions,
    };
    catalog
        .validate()
        .map_err(|error| super::ServerError::WorkflowCapabilityUnavailable(error.to_string()))?;
    Ok(catalog)
}

/// Describe one plugin-contributed workflow template and its current availability.
pub fn template_description(
    state: &ServerState,
    owner_plugin_id: &str,
    template: &bcode_plugin::WorkflowTemplateContribution,
) -> Result<bcode_ipc::WorkflowTemplateDescription, super::ServerError> {
    let loaded_plugins = state
        .plugins
        .registry()
        .manifests()
        .keys()
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let capabilities = bcode_workflow::WorkflowProductionCapabilities::current();
    let mut diagnostics = Vec::new();
    for requirement in &template.required_plugins {
        if !loaded_plugins.contains(requirement) {
            diagnostics.push(bcode_ipc::WorkflowTemplateDiagnostic {
                code: "missing_plugin".to_string(),
                requirement: requirement.clone(),
                message: format!("required plugin '{requirement}' is not loaded"),
            });
        }
    }
    let supported_capabilities = std::collections::BTreeSet::from([
        format!("workflow-production/v{}", capabilities.capability_version),
        format!(
            "workflow-block/v{}",
            capabilities.workflow_block_interface_version
        ),
    ]);
    for requirement in &template.required_capabilities {
        if !supported_capabilities.contains(requirement) {
            diagnostics.push(bcode_ipc::WorkflowTemplateDiagnostic {
                code: "unsupported_capability".to_string(),
                requirement: requirement.clone(),
                message: format!("required capability '{requirement}' is unsupported"),
            });
        }
    }
    Ok(bcode_ipc::WorkflowTemplateDescription {
        owner_plugin_id: owner_plugin_id.to_string(),
        identity: template
            .definition_identity(owner_plugin_id)
            .map_err(|error| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(error.to_string())
            })?,
        template: template.clone(),
        authoring_document: template.authoring_document().cloned(),
        diagnostics,
    })
}

/// Return bounded plugin-contributed workflow template descriptions.
pub fn list_templates(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_ipc::WorkflowTemplateDescription>, super::ServerError> {
    if limit == 0 || limit > 1_000 {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "workflow template limit must be in 1..=1000".to_string(),
        )
        .into());
    }
    state
        .plugins
        .registry()
        .workflow_templates()
        .into_iter()
        .take(limit)
        .map(|(owner, template)| template_description(state, owner, template))
        .collect()
}

/// Describe one exact plugin-contributed workflow template.
pub fn describe_template(
    state: &ServerState,
    owner_plugin_id: &str,
    template_id: &str,
    template_version: u32,
) -> Result<Option<Box<bcode_ipc::WorkflowTemplateDescription>>, super::ServerError> {
    state
        .plugins
        .registry()
        .workflow_templates()
        .into_iter()
        .find(|(owner, template)| {
            *owner == owner_plugin_id
                && template.template_id == template_id
                && template.template_version == template_version
        })
        .map(|(_, template)| template_description(state, owner_plugin_id, template))
        .transpose()
        .map(|template| template.map(Box::new))
}

/// Inspect one authored workflow revision against current requirement availability.
pub async fn revision_requirement_inspection(
    state: &ServerState,
    workflow_id: &str,
    revision: u64,
) -> Result<Option<bcode_workflow::WorkflowRevisionRequirementInspection>, super::ServerError> {
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(workflow_id, revision)?;
    let Some(revision) = revision else {
        return Ok(None);
    };
    let catalog = authoring_catalog(state).await?;
    let current_availability = bcode_workflow::workflow_requirement_availability(
        &revision.document.requirements,
        &catalog,
    )?;
    Ok(Some(
        bcode_workflow::WorkflowRevisionRequirementInspection {
            revision: Box::new(workflow_revision_snapshot(revision)),
            current_availability,
        },
    ))
}

/// Authorize and create one workflow preset without transport framing.
pub fn create_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::CreateWorkflowPresetRequest,
) -> Result<bcode_workflow_store::WorkflowPreset, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreatePreset,
            workflow_id: request.preset.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset.preset_id.clone()),
            producer: Some(request.preset.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let preset = workflow_preset_from_mutation(request.preset, 1, super::current_time_ms());
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_workflow_preset(&preset)?;
    Ok(preset)
}

/// Authorize and update one workflow preset without transport framing.
pub fn update_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::UpdateWorkflowPresetRequest,
) -> Result<bcode_workflow::WorkflowPresetUpdateResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdatePreset,
            workflow_id: request.preset.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset.preset_id.clone()),
            producer: Some(request.preset.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let preset = request.preset;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_preset(
            &preset.workflow_id,
            &preset.preset_id,
            request.expected_generation,
            &preset.name,
            &preset.configuration,
            preset.run_limits.as_ref(),
            &preset.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(preset) => Ok(bcode_workflow::WorkflowPresetUpdateResult::Updated(
            workflow_preset_snapshot(preset),
        )),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "update_preset");
            Ok(bcode_workflow::WorkflowPresetUpdateResult::Conflict(
                authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and delete one workflow preset without transport framing.
pub fn delete_preset(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_workflow::DeleteWorkflowPresetRequest,
) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::DeletePreset,
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: Some(request.preset_id.clone()),
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let deletion = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .delete_workflow_preset(
            &request.workflow_id,
            &request.preset_id,
            request.expected_generation,
        );
    match deletion {
        Ok(()) => Ok(bcode_workflow::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "delete_preset");
            Ok(bcode_workflow::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and archive or unarchive one authored workflow.
pub fn set_archived(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_workflow::SetAuthoredWorkflowArchivedRequest,
) -> Result<bcode_workflow_store::AuthoredWorkflow, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: if request.archived {
                bcode_workflow::WorkflowApplicationOperation::ArchiveWorkflow
            } else {
                bcode_workflow::WorkflowApplicationOperation::UnarchiveWorkflow
            },
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    store.set_authored_workflow_archived(
        &request.workflow_id,
        request.archived,
        super::current_time_ms(),
    )?;
    Ok(store
        .authored_workflow(&request.workflow_id)?
        .expect("updated authored workflow remains present"))
}

/// Authorize and activate one exact authored workflow revision.
fn activate_revision(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_workflow::ActivateWorkflowRevisionRequest,
) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ActivateRevision,
            workflow_id: request.workflow_id.clone(),
            draft_id: None,
            revision: Some(request.revision),
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: true,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .set_active_workflow_revision(
            &request.workflow_id,
            request.expected_active_revision,
            request.revision,
            super::current_time_ms(),
        );
    match update {
        Ok(()) => Ok(bcode_workflow::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "activate");
            Ok(bcode_workflow::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and discard one exact workflow draft.
fn discard_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_workflow::DiscardWorkflowDraftRequest,
) -> Result<bcode_workflow::WorkflowAuthoringMutationResult, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::DiscardDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let discard = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .discard_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
        );
    match discard {
        Ok(()) => Ok(bcode_workflow::WorkflowAuthoringMutationResult::Applied),
        Err(error) => {
            record_authoring_conflict(&state.metrics, "discard_draft");
            Ok(bcode_workflow::WorkflowAuthoringMutationResult::Conflict(
                authoring_conflict_result(error)?,
            ))
        }
    }
}

/// Authorize and fork one workflow draft or revision into a new draft.
pub fn fork_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::ForkWorkflowDraftRequest,
) -> Result<bcode_workflow_store::WorkflowDraft, super::ServerError> {
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ForkDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    match request.source {
        bcode_workflow::WorkflowDraftForkSource::Draft { draft_id } => store
            .fork_workflow_draft(
                &request.workflow_id,
                &draft_id,
                &request.draft_id,
                request.producer,
                super::current_time_ms(),
            )
            .map_err(super::ServerError::from),
        bcode_workflow::WorkflowDraftForkSource::Revision { revision } => store
            .fork_workflow_revision(
                &request.workflow_id,
                revision,
                &request.draft_id,
                request.producer,
                super::current_time_ms(),
            )
            .map_err(super::ServerError::from),
    }
}

/// Authorize and create one authored workflow with its initial draft.
pub fn create_authored_workflow(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::CreateAuthoredWorkflowRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    request.document.validate()?;
    let workflow_id = request.document.workflow_id.clone();
    let producer = request.document.producer.clone();
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::CreateWorkflow,
            workflow_id: workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: Some(producer.clone()),
            requirements: request.document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let workflow = bcode_workflow_store::AuthoredWorkflow {
        workflow_id: workflow_id.clone(),
        title: request.document.metadata.title.clone(),
        description: request.document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id,
        draft_id: request.draft_id,
        base_revision: None,
        generation: 1,
        checksum_sha256: request.document.source_digest_sha256()?,
        document: request.document,
        producer,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
    Ok((workflow, draft))
}

/// Authorize and update one complete workflow draft document.
fn update_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: &bcode_workflow::UpdateWorkflowDraftRequest,
) -> Result<bcode_workflow::WorkflowDraftUpdateResult, super::ServerError> {
    request.document.validate()?;
    if request.document.workflow_id != request.workflow_id {
        return Err(super::ServerError::WorkflowDefinitionUnsupported(
            "draft document workflow identity does not match the request".to_string(),
        ));
    }
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: request.document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.expected_generation,
            &request.document,
            &request.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(draft) => Ok(bcode_workflow::WorkflowDraftUpdateResult::Updated(
            Box::new(workflow_draft_snapshot(draft)),
        )),
        Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        }) => {
            record_authoring_conflict(&state.metrics, "update_draft");
            Ok(bcode_workflow::WorkflowDraftUpdateResult::Conflict(
                bcode_workflow::WorkflowAuthoringConflict {
                    entity_id,
                    expected_generation: expected,
                    current_generation: current,
                },
            ))
        }
        Err(error) => Err(error.into()),
    }
}

/// Apply one validated semantic edit batch to an exact workflow draft generation.
fn apply_draft_edits(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    request: bcode_workflow::ApplyWorkflowDraftEditsRequest,
) -> Result<bcode_workflow::WorkflowDraftEditResult, super::ServerError> {
    request.batch.validate()?;
    let current = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_draft(&request.workflow_id, &request.draft_id)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow draft not found: {}/{}",
                request.workflow_id, request.draft_id
            ))
        })?;
    if current.generation != request.batch.expected_generation {
        return Ok(bcode_workflow::WorkflowDraftEditResult::Conflict(
            bcode_workflow::WorkflowAuthoringConflict {
                entity_id: request.draft_id,
                expected_generation: request.batch.expected_generation,
                current_generation: current.generation,
            },
        ));
    }
    let document = match bcode_workflow::apply_workflow_authoring_edits(
        &current.document,
        &request.batch,
    ) {
        Ok(document) => document,
        Err(bcode_workflow::WorkflowError::Build { path, message }) => {
            return Ok(bcode_workflow::WorkflowDraftEditResult::Rejected {
                diagnostics: vec![bcode_workflow::WorkflowValidationDiagnostic {
                    code: "semantic_edit_rejected".to_string(),
                    severity: bcode_workflow::WorkflowValidationSeverity::Error,
                    document_path: path,
                    message,
                    remediation: "Revise the addressed semantic edit and retry against the same draft generation."
                        .to_string(),
                }],
            });
        }
        Err(error) => {
            return Err(super::ServerError::WorkflowDefinitionUnsupported(
                error.to_string(),
            ));
        }
    };
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::UpdateDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(request.producer.clone()),
            requirements: document.requirements.clone(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
        },
    )?;
    let update = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .update_workflow_draft(
            &request.workflow_id,
            &request.draft_id,
            request.batch.expected_generation,
            &document,
            &request.producer,
            super::current_time_ms(),
        );
    match update {
        Ok(draft) => Ok(bcode_workflow::WorkflowDraftEditResult::Updated(Box::new(
            workflow_draft_snapshot(draft),
        ))),
        Err(bcode_workflow_store::WorkflowStoreError::AuthoringConflict {
            entity_id,
            expected,
            current,
        }) => Ok(bcode_workflow::WorkflowDraftEditResult::Conflict(
            bcode_workflow::WorkflowAuthoringConflict {
                entity_id,
                expected_generation: expected,
                current_generation: current,
            },
        )),
        Err(error) => Err(error.into()),
    }
}

/// Convert one stored published revision into its portable representation.
#[must_use]
pub fn portable_revision(
    revision: bcode_workflow_store::PublishedWorkflowRevision,
) -> bcode_workflow::WorkflowPortableRevision {
    bcode_workflow::WorkflowPortableRevision {
        identity: bcode_workflow::WorkflowRevisionIdentity {
            workflow_id: revision.workflow_id,
            revision: revision.revision,
        },
        source_checksum_sha256: revision.source_checksum_sha256,
        executable_source_checksum_sha256: revision.executable_source_checksum_sha256,
        definition_identity: revision.definition_identity,
        document: revision.document,
        producer: revision.producer,
        published_at_ms: revision.published_at_ms,
    }
}

/// Export one exact published workflow revision as a validated portable bundle.
pub fn export_revision(
    state: &ServerState,
    request: &bcode_workflow::ExportWorkflowRevisionRequest,
) -> Result<bcode_workflow::WorkflowExportBundle, super::ServerError> {
    let revision = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .workflow_revision(&request.workflow_id, request.revision)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "published workflow revision not found: {} v{}",
                request.workflow_id, request.revision
            ))
        })?;
    let dependencies = bcode_workflow::workflow_dependency_manifest(&revision.document.definition)?;
    let bundle = bcode_workflow::WorkflowExportBundle {
        version: bcode_workflow::WORKFLOW_EXPORT_BUNDLE_VERSION,
        revision: portable_revision(revision),
        dependencies,
    };
    bundle.validate()?;
    Ok(bundle)
}

/// Validate and preview one portable workflow import without transport framing.
pub async fn import_preview(
    state: &ServerState,
    operation_id: String,
    bundle: bcode_workflow::WorkflowExportBundle,
    target_workflow_id: String,
    control: bcode_workflow::WorkflowComputationControl,
) -> Result<
    (
        bcode_workflow::WorkflowImportPreview,
        bcode_workflow::WorkflowAuthoringDocument,
    ),
    super::ServerError,
> {
    bundle.validate()?;
    let mut document = bundle.revision.document.clone();
    document.workflow_id.clone_from(&target_workflow_id);
    document.producer = bcode_workflow::WorkflowProducerProvenance {
        kind: bcode_workflow::WorkflowProducerKind::Generated,
        producer_id: Some("workflow-import".to_string()),
        source_revision: Some(bundle.revision.identity.clone()),
    };
    let catalog = authoring_catalog(state).await?;
    let preview_document = document.clone();
    let started_at = std::time::Instant::now();
    let compilation = run_computation(state, control, operation_id, move || {
        preview_document.compilation_preview(&catalog, None)
    })
    .await?;
    record_authoring_duration(
        &state.metrics,
        "workflow.authoring.import_preview.duration_ms",
        started_at,
        if compilation.compiled.is_some() {
            "accepted"
        } else {
            "rejected"
        },
    );
    Ok((
        bcode_workflow::WorkflowImportPreview {
            version: bcode_workflow::WORKFLOW_IMPORT_PREVIEW_VERSION,
            bundle_version: bundle.version,
            source_identity: bundle.revision.identity,
            target_workflow_id,
            compilation,
        },
        document,
    ))
}

/// Persist a validated import as a new authored workflow and initial draft.
pub async fn import_new_workflow(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    operation_id: String,
    request: bcode_workflow::ImportWorkflowRequest,
) -> Result<
    (
        bcode_workflow_store::AuthoredWorkflow,
        bcode_workflow_store::WorkflowDraft,
    ),
    super::ServerError,
> {
    if request.collision_policy != bcode_workflow::WorkflowImportCollisionPolicy::RequireNewWorkflow
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "new-workflow import requires require_new_workflow collision policy".to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        operation_id,
        request.bundle,
        request.target_workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportWorkflow,
            workflow_id: request.target_workflow_id.clone(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let workflow = bcode_workflow_store::AuthoredWorkflow {
        workflow_id: request.target_workflow_id.clone(),
        title: document.metadata.title.clone(),
        description: document.metadata.description.clone(),
        archived: false,
        active_revision: None,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.target_workflow_id,
        draft_id: request.draft_id,
        base_revision: None,
        generation: 1,
        checksum_sha256: document.source_digest_sha256()?,
        producer: document.producer.clone(),
        document,
        created_at_ms: now,
        updated_at_ms: now,
    };
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .create_authored_workflow_with_initial_draft(&workflow, &draft)?;
    Ok((workflow, draft))
}

/// Persist a validated import as a new draft on an existing authored workflow.
pub async fn import_draft(
    state: &std::sync::Arc<ServerState>,
    client_id: super::ClientId,
    operation_id: String,
    request: bcode_workflow::ImportWorkflowDraftRequest,
) -> Result<bcode_workflow::WorkflowDraftImportResult, super::ServerError> {
    if request.collision_policy
        != bcode_workflow::WorkflowImportCollisionPolicy::RequireExistingWorkflowNewDraft
    {
        return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
            "existing-workflow import requires require_existing_workflow_new_draft collision policy"
                .to_string(),
        )
        .into());
    }
    let (preview, document) = import_preview(
        state,
        operation_id,
        request.bundle,
        request.workflow_id.clone(),
        request.control,
    )
    .await?;
    let compiled = preview.compilation.compiled.as_ref().ok_or_else(|| {
        super::ServerError::WorkflowDefinitionUnsupported(
            "import requires a successful compilation preview".to_string(),
        )
    })?;
    state.authorize_local_workflow_application_operation(
        client_id,
        LocalApplicationOperationRequest {
            operation: bcode_workflow::WorkflowApplicationOperation::ImportDraft,
            workflow_id: request.workflow_id.clone(),
            draft_id: Some(request.draft_id.clone()),
            revision: None,
            preset_id: None,
            producer: Some(document.producer.clone()),
            requirements: compiled.requirements.clone(),
            effects: compiled.effects.clone(),
            activates: false,
            executes: false,
        },
    )?;
    let now = super::current_time_ms();
    let draft = bcode_workflow_store::WorkflowDraft {
        workflow_id: request.workflow_id.clone(),
        draft_id: request.draft_id.clone(),
        base_revision: None,
        generation: 1,
        checksum_sha256: document.source_digest_sha256()?,
        producer: document.producer.clone(),
        document,
        created_at_ms: now,
        updated_at_ms: now,
    };
    let mut store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let workflow = store
        .authored_workflow(&request.workflow_id)?
        .ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "authored workflow not found: {}",
                request.workflow_id
            ))
        })?;
    if store
        .workflow_draft(&request.workflow_id, &request.draft_id)?
        .is_some()
    {
        return Ok(
            bcode_workflow::WorkflowDraftImportResult::DraftAlreadyExists {
                workflow_id: request.workflow_id,
                draft_id: request.draft_id,
            },
        );
    }
    let created = store.create_workflow_draft(&draft)?;
    drop(store);
    assert!(
        created,
        "draft absence was checked while holding the store lock"
    );
    Ok(bcode_workflow::WorkflowDraftImportResult::Imported {
        workflow: authored_workflow_snapshot(workflow),
        draft: Box::new(workflow_draft_snapshot(draft)),
    })
}

/// Return bounded workflow run summaries without transport framing.
pub fn list_runs(
    state: &ServerState,
    limit: usize,
) -> Result<Vec<bcode_workflow_store::WorkflowRunSummary>, bcode_workflow_store::WorkflowStoreError>
{
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_runs(limit)
}

/// Return checksum-verified bounded workflow outputs without transport framing.
pub fn run_outputs(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<Vec<bcode_workflow::WorkflowOutputInspection>, bcode_workflow_store::WorkflowStoreError>
{
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let checksums = store
        .output_summaries(run_id, limit)?
        .into_iter()
        .map(|output| (output.output_id, output.checksum_sha256))
        .collect::<std::collections::BTreeMap<_, _>>();
    Ok(store
        .validated_outputs(run_id, limit)?
        .into_iter()
        .map(|output| bcode_workflow::WorkflowOutputInspection {
            version: bcode_workflow::WORKFLOW_OUTPUT_INSPECTION_VERSION,
            checksum_sha256: checksums
                .get(&output.output_id)
                .cloned()
                .expect("validated output has a matching bounded summary"),
            output_id: output.output_id,
            run_id: output.run_id,
            node_id: output.node_id,
            activation_id: output.activation_id,
            schema_id: output.schema_id,
            schema_version: output.schema_version,
            value: output.value,
            artifact_reference: output.artifact_reference,
            created_at_ms: output.created_at_ms,
        })
        .collect())
}

pub fn inspect_graph_page(
    state: &ServerState,
    request: &bcode_workflow::WorkflowRunGraphPageRequest,
) -> Result<bcode_workflow::WorkflowRunGraphInspection, bcode_workflow_store::WorkflowStoreError> {
    let store = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    inspect_run_graph(
        &store,
        &request.run_id,
        request.after_node_id.as_deref(),
        request.after_edge_id,
        Some(request.expected_revision),
        request.limit,
    )
}

fn inspect_run_graph(
    store: &bcode_workflow_store::WorkflowStore,
    run_id: &str,
    after_node_id: Option<&str>,
    after_edge_id: Option<u64>,
    expected_revision: Option<u64>,
    limit: usize,
) -> Result<bcode_workflow::WorkflowRunGraphInspection, bcode_workflow_store::WorkflowStoreError> {
    let page = store.current_run_graph_page(
        run_id,
        expected_revision,
        after_node_id,
        after_edge_id,
        limit,
    )?;
    let revision = page.revision;
    let nodes = page.nodes;
    let edges = page.edges;
    let nodes_complete = page.nodes_complete;
    let edges_complete = page.edges_complete;
    Ok(bcode_workflow::WorkflowRunGraphInspection {
        revision,
        nodes: nodes
            .into_iter()
            .map(|record| bcode_workflow::WorkflowRunGraphNodeInspection {
                revision: record.revision,
                node: record.node,
                entry: record.entry,
                exit: record.exit,
            })
            .collect(),
        edges: edges
            .into_iter()
            .map(|record| bcode_workflow::WorkflowRunGraphEdgeInspection {
                revision: record.revision,
                edge_id: record.edge_id,
                edge: record.edge,
            })
            .collect(),
        nodes_complete,
        edges_complete,
    })
}

#[allow(clippy::too_many_lines)]
pub async fn inspect_run(
    state: &ServerState,
    run_id: &str,
    limit: usize,
) -> Result<bcode_workflow::WorkflowRunInspection, super::ServerError> {
    let (
        run,
        graph,
        definition,
        terminal_output,
        activations,
        waits,
        mutation_approvals,
        attempts,
        events,
        decisions,
        grants,
        resource_leases,
        outputs,
        child_run_links,
        descendant_runs,
        repeat_outcomes,
        execution_session_links,
    ) = {
        let store = state
            .workflow_store
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let run = store.run_summary(run_id)?.ok_or_else(|| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                "workflow run not found: {run_id}"
            ))
        })?;
        let definition = store
            .definition(&run.definition_id, run.definition_version)?
            .ok_or_else(|| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(format!(
                    "workflow definition not found: {} v{}",
                    run.definition_id, run.definition_version
                ))
            })?;
        let definition = bcode_workflow::WorkflowDefinitionSnapshot {
            definition: bcode_workflow::WorkflowDefinitionRepresentation::parse(
                definition.definition_json,
            )
            .map_err(|_| {
                bcode_workflow_store::WorkflowStoreError::InvalidData(
                    "invalid workflow definition representation".to_string(),
                )
            })?,
            definition_id: definition.definition_id,
            version: definition.version,
            checksum_sha256: definition.checksum_sha256,
        };
        let terminal_output = store.canonical_terminal_output(run_id)?.map(|output| {
            bcode_workflow::WorkflowTerminalOutputInspection {
                version: bcode_workflow::WORKFLOW_TERMINAL_OUTPUT_INSPECTION_VERSION,
                checksum_sha256: run
                    .terminal_output_checksum_sha256
                    .clone()
                    .expect("canonical terminal output has a checksum"),
                output_id: output.output_id,
                node_id: output.node_id,
                activation_id: output.activation_id,
                schema_id: output.schema_id,
                schema_version: output.schema_version,
                value: output.value,
                artifact_reference: output.artifact_reference,
                created_at_ms: output.created_at_ms,
            }
        });
        let descendant_runs = store.descendant_run_summaries(run_id, limit)?;
        let mut repeat_outcomes = store.repeat_outcomes(run_id, limit)?;
        for descendant in &descendant_runs {
            let remaining = limit.saturating_sub(repeat_outcomes.len());
            if remaining == 0 {
                break;
            }
            repeat_outcomes.extend(store.repeat_outcomes(&descendant.run.run_id, remaining)?);
        }
        let execution_session_links = store.execution_session_links_for_run(run_id, limit)?;
        (
            run,
            inspect_run_graph(&store, run_id, None, None, None, limit)?,
            definition,
            terminal_output,
            store.activations_for_run(run_id, limit)?,
            store.waiting_activations(run_id, limit)?,
            store
                .pending_mutation_approvals(run_id, limit)?
                .into_iter()
                .map(mutation_approval_inspection)
                .collect(),
            store.attempt_history(run_id, None, limit)?,
            store
                .event_history(run_id, None, limit)?
                .into_iter()
                .map(|event| verified_history_event(&store, event))
                .collect::<Result<Vec<_>, _>>()?,
            store
                .decisions_for_run(run_id, limit)?
                .into_iter()
                .map(decision_inspection)
                .collect(),
            store
                .grants_for_run(run_id, limit)?
                .into_iter()
                .map(grant_inspection)
                .collect(),
            store.resource_leases_for_run(run_id, limit)?,
            store.output_summaries(run_id, limit)?,
            store.child_run_links(run_id, limit)?,
            descendant_runs,
            repeat_outcomes,
            execution_session_links,
        )
    };
    let mut child_sessions = Vec::with_capacity(execution_session_links.len());
    for link in execution_session_links {
        if child_sessions.len() >= limit {
            break;
        }
        let session_id = link.session_id.parse().map_err(|_| {
            bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow execution-session link has invalid session identity".to_string(),
            )
        })?;
        let summary = state.sessions.session_summary(session_id).await?;
        if summary.execution.as_ref().is_none_or(|execution| {
            execution.provenance.run_id != run_id
                || execution.provenance.node_id != link.node_id
                || execution.provenance.activation_id.as_deref()
                    != Some(link.activation_id.as_str())
                || execution.provenance.attempt != link.attempt
        }) {
            return Err(bcode_workflow_store::WorkflowStoreError::InvalidData(
                "workflow execution-session link conflicts with session provenance".to_string(),
            )
            .into());
        }
        child_sessions.push(summary);
    }
    let authority = state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .execution_authority(run_id)?;
    let coordinator = match authority {
        Some(authority) => {
            let owned_by_this_daemon =
                authority.daemon_instance_id == state.daemon_status.instance_id;
            let controllable_from_this_daemon = if owned_by_this_daemon {
                true
            } else {
                match run
                    .parent_session_id
                    .as_deref()
                    .and_then(|value| value.parse::<super::SessionId>().ok())
                {
                    Some(session_id) => matches!(
                        prior_owner_liveness(state, session_id, &authority).await?,
                        PriorOwnerLiveness::ObservedEnded | PriorOwnerLiveness::NoLiveTrace
                    ),
                    None => false,
                }
            };
            Some(bcode_workflow::WorkflowCoordinatorStatus {
                target_artifact_id: authority.target_artifact_id,
                daemon_instance_id: authority.daemon_instance_id,
                owned_by_this_daemon,
                controllable_from_this_daemon,
            })
        }
        None => None,
    };
    Ok(bcode_workflow::WorkflowRunInspection {
        run,
        graph: Some(graph),
        definition,
        terminal_output,
        activations,
        waits,
        mutation_approvals,
        attempts,
        events,
        decisions,
        grants,
        resource_leases,
        outputs,
        child_run_links,
        descendant_runs,
        repeat_outcomes,
        child_sessions,
        coordinator,
    })
}

/// Return bounded workflow definitions without transport framing.
pub fn list_definitions(
    state: &ServerState,
    limit: usize,
) -> Result<
    Vec<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .list_definitions(limit)
}

/// Return one versioned workflow definition without transport framing.
pub fn describe_definition(
    state: &ServerState,
    definition_id: &str,
    version: u32,
) -> Result<
    Option<bcode_workflow_store::StoredWorkflowDefinition>,
    bcode_workflow_store::WorkflowStoreError,
> {
    state
        .workflow_store
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .definition(definition_id, version)
}
