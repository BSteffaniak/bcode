//! Graph-edit routing for authenticated execution sessions. Policy remains at application entry.
use super::{
    RunLookup, ServerError, ServerState, lookup_at, open, representation, retained_store,
    unavailable, unsupported,
};
use bcode_session_models::ExecutionSessionProvenance;
use bcode_workflow_store::{WorkflowExecutionAuthority, WorkflowExecutionSessionLink};

#[path = "graph_diagnostics.rs"]
pub mod diagnostics;

pub struct AuthenticatedExecution {
    pub link: WorkflowExecutionSessionLink,
    pub authority: WorkflowExecutionAuthority,
    pub source: bcode_workflow::NodeDefinition,
    // Retain shared maintenance ownership across all async graph operations.
    retained: std::sync::Mutex<bcode_workflow_store::WorkflowStore>,
    path: std::path::PathBuf,
}

/// Return None only for a verified legacy identity; malformed/new unsupported state never falls back.
pub async fn authenticate(
    state: &ServerState,
    session_id: crate::SessionId,
    provenance: &ExecutionSessionProvenance,
) -> Result<Option<AuthenticatedExecution>, ServerError> {
    let retained = retained_store(state).await?;
    match lookup_at(retained.path(), &provenance.run_id).await? {
        RunLookup::Legacy => return Ok(None),
        RunLookup::Available(_) => {}
        RunLookup::Missing | RunLookup::UnsupportedRepresentation => return Err(unsupported()),
        RunLookup::Degraded => return Err(unavailable()),
    }
    let activation = provenance
        .activation_id
        .as_deref()
        .ok_or_else(unsupported)?;
    let db = open(retained.path(), false)?;
    let link = representation::attempts::owner_boundary::execution_session_link(
        &db,
        &provenance.run_id,
        &provenance.node_id,
        activation,
        provenance.attempt,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unsupported)?;
    if provenance.version != bcode_session_models::EXECUTION_SESSION_PROVENANCE_VERSION
        || provenance.owner != "bcode.workflow"
        || link.session_id != session_id.to_string()
        || Some(link.workspace_snapshot.as_str()) != provenance.workspace_snapshot.as_deref()
    {
        return Err(unsupported());
    }
    let authority =
        representation::attempts::owner_boundary::execution_authority(&db, &link.run_id)
            .await
            .map_err(|_| unavailable())?
            .ok_or_else(unsupported)?;
    if !crate::workflow_operations::authority_targets_current_daemon(state, &authority) {
        return Err(unsupported());
    }
    let identity = bcode_workflow_store::dispatch_identity(
        &link.run_id,
        &link.node_id,
        &link.activation_id,
        link.attempt,
    );
    let receipt = representation::attempts::owner_boundary::receipt(&db, &identity)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unsupported)?;
    if receipt.run_id != link.run_id
        || receipt.activation_id != link.activation_id
        || receipt.attempt != link.attempt
        || representation::attempts::owner_boundary::terminal_outcome(&db, &link.run_id, &identity)
            .await
            .map_err(|_| unavailable())?
            .is_some()
    {
        return Err(unsupported());
    }
    let latest = representation::attempts::owner_boundary::latest_prepared(
        &db,
        &link.run_id,
        &link.node_id,
        &link.activation_id,
    )
    .await
    .map_err(|_| unavailable())?
    .ok_or_else(unsupported)?;
    if latest.attempt != link.attempt || latest.dispatch_identity() != identity {
        return Err(unsupported());
    }
    let revision = representation::graph_pages::revision(&db, &link.run_id)
        .await
        .map_err(|_| unavailable())?;
    let source = representation::graph_pages::node(&db, &link.run_id, revision, &link.node_id)
        .await
        .map_err(|_| unavailable())?
        .ok_or_else(unavailable)?;
    if representation::graph::revalidate(&db, &link.run_id, &link.node_id, &link.activation_id)
        .await
        .map_err(|_| unavailable())?
        != representation::graph::ActivationValidity::Ready
    {
        return Err(unsupported());
    }
    drop(db);
    Ok(Some(AuthenticatedExecution {
        link,
        authority,
        source,
        path: retained.path().to_path_buf(),
        retained: std::sync::Mutex::new(retained),
    }))
}

impl AuthenticatedExecution {
    fn verify_retained_path(&self) -> Result<(), ServerError> {
        let retained = self
            .retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let matches = retained.path() == self.path;
        drop(retained);
        if !matches {
            return Err(unavailable());
        }
        Ok(())
    }
    pub async fn context(
        &self,
        request: &bcode_workflow::WorkflowExecutionContextRequest,
    ) -> Result<bcode_workflow::WorkflowExecutionContext, ServerError> {
        self.verify_retained_path()?;
        let db = open(&self.path, false)?;
        let (page, output, outputs) =
            representation::execution_graph::execution_context_graph_page(
                &db,
                &self.link,
                &self.authority,
                request,
            )
            .await
            .map_err(|_| unavailable())?;
        Ok(bcode_workflow::WorkflowExecutionContext {
            run_id: self.link.run_id.clone(),
            node_id: self.link.node_id.clone(),
            activation_id: self.link.activation_id.clone(),
            attempt: self.link.attempt,
            graph: crate::workflow_operations::graph_page_inspection(page),
            execution_allowance: None,
            output,
            outputs,
        })
    }

    pub async fn direct_contributor(
        &self,
        expected_revision: u64,
    ) -> Result<representation::contributor_evidence::DirectContributorObservation, ServerError>
    {
        self.verify_retained_path()?;
        let db = open(&self.path, false)?;
        representation::contributor_evidence::direct_contributor(
            &db,
            &self.authority,
            &self.link,
            expected_revision,
        )
        .await
        .map_err(|_| unavailable())
    }

    pub async fn generated_contributors(
        &self,
        expected_revision: u64,
        after_node_id: Option<&str>,
        limit: u16,
    ) -> Result<representation::contributor_evidence::GeneratedContributorObservation, ServerError>
    {
        self.verify_retained_path()?;
        let db = open(&self.path, false)?;
        representation::contributor_evidence::generated_contributors(
            &db,
            &self.authority,
            &self.link,
            expected_revision,
            after_node_id,
            limit,
        )
        .await
        .map_err(|_| unavailable())
    }

    pub async fn stage(
        &self,
        batch: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<bool, ServerError> {
        if self.source.kind == bcode_workflow::NodeKind::Agent {
            let configuration: crate::WorkflowPromptConfiguration =
                serde_json::from_value(self.source.configuration.clone())
                    .map_err(|_| unsupported())?;
            crate::validate_user_question_inheritance(configuration.allow_user_questions, batch)
                .map_err(ServerError::WorkflowApplicationOperationUnauthorized)?;
        }
        self.verify_retained_path()?;
        let db = open(&self.path, true)?;
        representation::execution_graph::stage_run_graph_edit_from_execution(
            &db,
            batch,
            &self.authority,
            &self.link,
            crate::current_time_ms(),
        )
        .await
        .map_err(|_| unavailable())
    }

    pub async fn accept(
        &self,
        batch: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<bcode_workflow::WorkflowRunGraphPublicationStatus, ServerError> {
        self.verify_retained_path()?;
        let db = open(&self.path, true)?;
        super::backpressure::queued(&self.path, || {
            representation::execution_graph::accept_publication_from_execution(
                &db,
                batch,
                &self.authority,
                &self.link,
            )
        })
        .await
        .map_err(|error| diagnostics::normalize(&error))
    }

    pub async fn publish(
        &self,
        batch: &bcode_workflow::WorkflowRunGraphEditBatch,
    ) -> Result<u64, ServerError> {
        self.verify_retained_path()?;
        let db = open(&self.path, true)?;
        representation::execution_graph::publication_from_execution(
            &db,
            batch,
            &self.authority,
            &self.link,
        )
        .await
        .map_err(|error| diagnostics::normalize(&error))
    }
}

pub async fn candidate(
    state: &ServerState,
    run_id: &str,
    mutation_id: &str,
) -> Result<Option<bcode_workflow::WorkflowRunGraphEditBatch>, ServerError> {
    let retained = retained_store(state).await?;
    let db = open(retained.path(), false)?;
    match lookup_at(retained.path(), run_id).await? {
        RunLookup::Legacy => {
            let authority = retained
                .execution_authority(run_id)?
                .ok_or_else(unsupported)?;
            Ok(retained.staged_run_graph_edit(run_id, mutation_id, &authority)?)
        }
        RunLookup::Available(_) => {
            let authority =
                representation::attempts::owner_boundary::execution_authority(&db, run_id)
                    .await
                    .map_err(|_| unavailable())?
                    .ok_or_else(unsupported)?;
            if !crate::workflow_operations::authority_targets_current_daemon(state, &authority) {
                return Err(unsupported());
            }
            representation::execution_graph::staged(&db, run_id, mutation_id, &authority)
                .await
                .map_err(|_| unavailable())
        }
        _ => Err(unsupported()),
    }
}

/// The invoking application already holds publication policy and cancellation commit guards.
pub async fn publish_authenticated(
    state: &ServerState,
    session_id: crate::SessionId,
    provenance: &ExecutionSessionProvenance,
    batch: &bcode_workflow::WorkflowRunGraphEditBatch,
    accept_pending: bool,
) -> Result<Option<bcode_workflow::WorkflowRunGraphPublicationStatus>, ServerError> {
    let Some(execution) = authenticate(state, session_id, provenance).await? else {
        return Ok(None);
    };
    let permit = state
        .workflow_driver_sender
        .get()
        .ok_or_else(unsupported)?
        .try_reserve()
        .map_err(|_| unsupported())?;
    let status = if accept_pending {
        execution.accept(batch).await?
    } else {
        bcode_workflow::WorkflowRunGraphPublicationStatus::Committed {
            revision: execution.publish(batch).await?,
        }
    };
    permit.send(batch.run_id.clone());
    Ok(Some(status))
}
