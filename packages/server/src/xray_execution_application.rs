//! Defined application boundary for authenticated execution diagnostics.
//!
//! Transport adapters pass the registered connection identity, never an actor or
//! authorization grant supplied by request JSON. Policy precedes capture admission.

use crate::{ClientId, ServerState};
use bcode_workflow::{
    WorkflowApplicationActor, WorkflowApplicationActorKind, WorkflowApplicationOperation,
    WorkflowApplicationOperationFacts, WorkflowExecutionObservationFacts,
};
use bcode_xray_models::execution_watch as dto;

type ExecutionObservationCaller = (String, String);
type ExecutionObservationAdmission = (ExecutionObservationCaller, Option<std::time::Instant>);

impl ServerState {
    /// Start a fresh run with an explicitly authorized observation reservation installed
    /// before the run becomes visible to any scheduler. Normal starts are unchanged.
    /// This method performs the same ordinary start prerequisite and application call
    /// as normal starts. Diagnostic approval never grants execution permission.
    /// A stable caller-selected run ID is required and preserved exactly. Existing
    /// runs/duplicate reservations are rejected, not re-driven or silently re-identified.
    ///
    /// # Errors
    /// Rejects unsupported options, missing run IDs, denied policy or namespace mismatch.
    /// Admission failures revoke the reservation; errors never imply no execution effects.
    /// After ambiguous failure inspect the supplied run ID; never retry with a new ID.
    pub async fn start_workflow_run_observed(
        self: &std::sync::Arc<Self>,
        client_id: ClientId,
        start: bcode_workflow::WorkflowRunStartRequest,
        observation: dto::ExecutionWatchStartOptions,
    ) -> Result<
        (
            bcode_workflow::WorkflowRunStartResponse,
            dto::ExecutionWatchReady,
        ),
        dto::ExecutionWatchError,
    > {
        version(observation.schema_version)?;
        // Same availability/admission prerequisite as the normal application start,
        // completed before any diagnostic reservation side effect.
        self.require_workflow_store()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        let run_id = start
            .run_id
            .as_ref()
            .filter(|id| !id.is_empty() && id.len() <= 256 && !id.chars().any(char::is_control))
            .ok_or(dto::ExecutionWatchError::InvalidRequest)?
            .clone();
        let scope = dto::ExecutionWatchScope {
            root_run_id: run_id,
            recursive: observation.recursive,
            deep_capture: observation.deep_capture,
        };
        let (selected, deadline) = self
            .authorize_execution_observation_scope(
                client_id,
                WorkflowApplicationOperation::ObserveExecution,
                &scope,
            )
            .await?;
        if self
            .active_session_namespace_mismatch(start.parent_session_id, client_id)
            .await
            .is_some()
        {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        if self.execution_observation_caller(client_id).await? != selected {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        // A duplicate may already have dispatched. Never relabel it as gap-free or
        // drive it again through observed admission. Inspect its exact identity instead.
        if crate::workflow_representation::status(self, &scope.root_run_id)
            .await
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?
            .is_some()
        {
            return Err(dto::ExecutionWatchError::InvalidRequest);
        }
        // Canonical discovery awaited above; a disconnected/rebound caller must not
        // acquire a reservation under the earlier authenticated namespace.
        if self.execution_observation_caller(client_id).await? != selected {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        let mut ready = self.xray_execution_watches.reserve_admission_until(
            &client_id.to_string(),
            dto::OpenExecutionWatchRequest {
                schema_version: 1,
                scope: scope.clone(),
            },
            deadline,
        )?;
        ready.root_session_id = Some(start.parent_session_id.to_string());
        if let Ok(started) = bcode_workflow::WorkflowRunApplication::start_workflow_run(
            &crate::workflow_operations::WorkflowAuthoringApplication::new(self, client_id),
            start,
        )
        .await
        {
            Ok((started, ready))
        } else {
            let _ = self.xray_execution_watches.revoke_authorized(
                &client_id.to_string(),
                dto::RevokeExecutionWatchRequest {
                    schema_version: 1,
                    watch_id: ready.watch_id,
                },
            );
            Err(dto::ExecutionWatchError::Unavailable)
        }
    }

    async fn authorize_execution_observation_scope(
        &self,
        client_id: ClientId,
        operation: WorkflowApplicationOperation,
        scope: &dto::ExecutionWatchScope,
    ) -> Result<ExecutionObservationAdmission, dto::ExecutionWatchError> {
        let (namespace, state_location_id) = self.execution_observation_caller(client_id).await?;
        let selected = (namespace.clone(), state_location_id.clone());
        let facts = WorkflowApplicationOperationFacts {
            version: bcode_workflow::WORKFLOW_APPLICATION_OPERATION_FACTS_VERSION,
            operation,
            actor: WorkflowApplicationActor {
                kind: WorkflowApplicationActorKind::LocalClient,
                actor_id: client_id.to_string(),
            },
            definition_registration: None,
            workflow_id: String::new(),
            draft_id: None,
            revision: None,
            preset_id: None,
            producer: None,
            requirements: bcode_workflow::WorkflowRequirementSummary::default(),
            effects: bcode_workflow::WorkflowEffectSummary::default(),
            activates: false,
            executes: false,
            execution_observation: Some(WorkflowExecutionObservationFacts {
                root_run_id: scope.root_run_id.clone(),
                recursive: scope.recursive,
                deep_capture: scope.deep_capture,
                state_location_id,
                namespace,
            }),
        };
        let deadline = self.authorize_execution_observation_policy(&facts).await?;
        // Plugin policy is asynchronous: registration and selected state/namespace
        // must still match before any diagnostic reservation or retained read.
        if self.execution_observation_caller(client_id).await? != selected {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        Ok((selected, deadline))
    }

    async fn authorize_execution_observation_policy(
        &self,
        facts: &WorkflowApplicationOperationFacts,
    ) -> Result<Option<std::time::Instant>, dto::ExecutionWatchError> {
        facts
            .validate()
            .map_err(|_| dto::ExecutionWatchError::Forbidden)?;
        match self.authorize_workflow_application_operation(facts) {
            Ok(()) => return Ok(None),
            Err(_) if !self.workflow_execution_plugin_policy => {
                // Explicit custom denial is authoritative; only the default denial
                // can fall back to configured plugin-owned diagnostic grants.
                return Err(dto::ExecutionWatchError::Forbidden);
            }
            Err(_) => {}
        }
        let decision = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.plugins.invoke_service_by_interface_json::<
                _, bcode_workflow::WorkflowExecutionObservationPolicyDecision,
            >(
                bcode_workflow::WORKFLOW_EXECUTION_OBSERVATION_POLICY_INTERFACE_ID,
                bcode_workflow::OP_AUTHORIZE_EXECUTION_OBSERVATION,
                facts,
            ),
        )
        .await
        .map_err(|_| dto::ExecutionWatchError::Forbidden)?
        .map_err(|_| dto::ExecutionWatchError::Forbidden)?;
        match decision {
            bcode_workflow::WorkflowExecutionObservationPolicyDecision::Allow {
                expires_at_unix_ms,
            } => {
                let monotonic = std::time::Instant::now();
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| dto::ExecutionWatchError::Forbidden)?;
                let remaining = std::time::Duration::from_millis(expires_at_unix_ms)
                    .checked_sub(now)
                    .filter(|remaining| !remaining.is_zero())
                    .ok_or(dto::ExecutionWatchError::Forbidden)?;
                monotonic
                    .checked_add(remaining)
                    .map(Some)
                    .ok_or(dto::ExecutionWatchError::Forbidden)
            }
            bcode_workflow::WorkflowExecutionObservationPolicyDecision::Deny => {
                Err(dto::ExecutionWatchError::Forbidden)
            }
        }
    }

    /// Authorize an explicit run/recursive/deep scope before installing future capture.
    /// Readiness must be received before the caller starts observed work. Existing
    /// execution is not replayed, repaired or taken over.
    ///
    /// # Errors
    /// Rejects unregistered callers, namespace/location mismatch, unsupported requests,
    /// denied diagnostic policy, missing canonical runs and capture overload.
    pub async fn open_execution_watch(
        &self,
        client_id: ClientId,
        request: dto::OpenExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchReady, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        let (root_session_id, deadline) = self
            .authorize_execution_observation(
                client_id,
                WorkflowApplicationOperation::ObserveExecution,
                &request.scope,
            )
            .await?;
        let mut ready = self.xray_execution_watches.open_authorized_until(
            &client_id.to_string(),
            request,
            deadline,
        )?;
        ready.root_session_id = Some(root_session_id);
        Ok(ready)
    }

    /// Read a bounded destructive page after rechecking diagnostic policy.
    ///
    /// # Errors
    /// Rejects unregistered/foreign callers, withdrawn authorization and invalid cursors.
    pub async fn read_execution_watch(
        &self,
        client_id: ClientId,
        request: dto::ReadExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchPage, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        let scope = self
            .xray_execution_watches
            .scope_for_principal(&client_id.to_string(), &request.watch_id)?;
        self.authorize_execution_observation(
            client_id,
            WorkflowApplicationOperation::ReadExecutionObservation,
            &scope,
        )
        .await?;
        self.xray_execution_watches
            .read_authorized(&client_id.to_string(), request)
    }

    /// Stop future capture, retain bytes for export, and return bounded engine cutoffs.
    /// Does not cancel/settle execution or certify transport delivery/disk flush.
    ///
    /// # Errors
    /// Rejects foreign/unregistered callers, denied policy and malformed evidence pages.
    /// Returns unavailable if completion and authorization recheck exceed five seconds
    /// (or the earlier authorization expiry). The cutoff remains resumable.
    pub async fn finalize_execution_watch(
        &self,
        client_id: ClientId,
        request: dto::FinalizeExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchFinalEvidence, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        let scope = self
            .xray_execution_watches
            .scope_for_principal(&client_id.to_string(), &request.watch_id)?;
        let (_, authorization_deadline) = self
            .authorize_execution_observation(
                client_id,
                WorkflowApplicationOperation::FinalizeExecutionObservation,
                &scope,
            )
            .await?;
        let mut deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        if let Some(expiry) = authorization_deadline {
            deadline = deadline.min(tokio::time::Instant::from_std(expiry));
        }
        tokio::time::timeout_at(deadline, async {
            let evidence = self
                .xray_execution_watches
                .finalize_authorized_complete(&client_id.to_string(), request.clone())
                .await?;
            // Recheck after waiting, before exporting any completed evidence.
            let (_, expiry) = self
                .authorize_execution_observation(
                    client_id,
                    WorkflowApplicationOperation::FinalizeExecutionObservation,
                    &scope,
                )
                .await?;
            if expiry.is_some_and(|expiry| std::time::Instant::now() >= expiry) {
                return Err(dto::ExecutionWatchError::Forbidden);
            }
            self.xray_execution_watches
                .scope_for_principal(&client_id.to_string(), &request.watch_id)?;
            if tokio::time::Instant::now() >= deadline {
                return Err(dto::ExecutionWatchError::Unavailable);
            }
            Ok(evidence)
        })
        .await
        .map_err(|_| dto::ExecutionWatchError::Unavailable)?
    }

    /// Export a bounded retained byte range after separate sensitive-export authorization.
    /// Capture credentials are not a valid payload and never enter this response.
    ///
    /// # Errors
    /// Rejects shallow/foreign/unregistered scopes, denied export policy and invalid ranges.
    pub async fn export_execution_watch_payload(
        &self,
        client_id: ClientId,
        request: dto::ExportExecutionWatchPayloadRequest,
    ) -> Result<bcode_xray_models::XrayPayloadRange, dto::ExecutionWatchError> {
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(5);
        // One budget includes both policy checks; a read timeout never revokes
        // the watch, poisons retention, or claims a missing payload is empty.
        let mut shutdown = self.shutdown.subscribe();
        let work = Box::pin(tokio::time::timeout_at(deadline, async {
            version(request.schema_version)?;
            let principal = client_id.to_string();
            let selected = self.execution_observation_caller(client_id).await?;
            let scope = self
                .xray_execution_watches
                .scope_for_principal(&principal, &request.watch_id)?;
            if !scope.deep_capture {
                return Err(dto::ExecutionWatchError::Forbidden);
            }
            let (root_session, expiry) = self
                .authorize_execution_observation(
                    client_id,
                    WorkflowApplicationOperation::ExportExecutionObservation,
                    &scope,
                )
                .await?;
            let result = self
                .xray_execution_watches
                .export_authorized_complete(&self.xray, &principal, &request, expiry)
                .await;
            // Reauthorize even an engine error: withdrawal/expiry takes precedence.
            let (current_root, current_expiry) = self
                .authorize_execution_observation(
                    client_id,
                    WorkflowApplicationOperation::ExportExecutionObservation,
                    &scope,
                )
                .await?;
            if current_root != root_session
                || self.execution_observation_caller(client_id).await? != selected
                || expiry.is_some_and(|expiry| std::time::Instant::now() >= expiry)
                || current_expiry.is_some_and(|expiry| std::time::Instant::now() >= expiry)
                || self
                    .xray_execution_watches
                    .scope_for_principal(&principal, &request.watch_id)?
                    != scope
            {
                return Err(dto::ExecutionWatchError::Forbidden);
            }
            // Policy awaits can race child removal/revocation: recheck the exact
            // retained link synchronously immediately before releasing bytes.
            self.xray_execution_watches
                .validate_payload_export(&principal, &request)?;
            if tokio::time::Instant::now() >= deadline {
                return Err(dto::ExecutionWatchError::Unavailable);
            }
            result
        }));
        tokio::select! {
            biased;
            _ = shutdown.recv() => Err(dto::ExecutionWatchError::Unavailable),
            result = work => result.map_err(|_| dto::ExecutionWatchError::Unavailable)?,
        }
    }

    /// Release diagnostic captures under the registered opening caller. Revocation does
    /// not require continued diagnostic policy approval and never mutates execution.
    ///
    /// # Errors
    /// Rejects unsupported versions and unregistered/foreign callers.
    pub async fn revoke_execution_watch(
        &self,
        client_id: ClientId,
        request: dto::RevokeExecutionWatchRequest,
    ) -> Result<dto::ExecutionWatchRevoked, dto::ExecutionWatchError> {
        version(request.schema_version)?;
        self.execution_observation_caller(client_id).await?;
        self.xray_execution_watches
            .revoke_authorized(&client_id.to_string(), request)
    }

    /// Recheck each exact matching grant immediately before attaching a dispatch.
    /// Withdrawal discards diagnostic retention only, never execution authority.
    pub(crate) async fn reauthorize_execution_dispatch(
        &self,
        run_id: &str,
        ancestors: &[String],
    ) -> bool {
        let Ok(grants) = self
            .xray_execution_watches
            .dispatch_grants(run_id, ancestors)
        else {
            return false;
        };
        for grant in grants {
            let authorized = if let Ok(id) = uuid::Uuid::parse_str(grant.principal()) {
                self.authorize_execution_observation_inner(
                    ClientId(id),
                    WorkflowApplicationOperation::ObserveExecution,
                    &grant.scope(),
                )
                .await
                .is_ok()
            } else {
                false
            };
            if !authorized {
                let _ = self.xray_execution_watches.revoke(&grant);
            }
        }
        true
    }

    async fn execution_observation_caller(
        &self,
        client_id: ClientId,
    ) -> Result<(String, String), dto::ExecutionWatchError> {
        if self
            .shutdown_requested
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return Err(dto::ExecutionWatchError::Unavailable);
        }
        // Namespace exists only after artifact/build/state-matching Hello admission.
        if !self.clients.lock().await.contains(&client_id) {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        let namespace = self
            .client_session_namespaces
            .lock()
            .await
            .get(&client_id)
            .cloned()
            .ok_or(dto::ExecutionWatchError::Forbidden)?;
        let location = self
            .daemon_status
            .state_location_id
            .clone()
            .filter(|value| !value.is_empty())
            .ok_or(dto::ExecutionWatchError::Unavailable)?;
        Ok((namespace, location))
    }

    async fn authorize_execution_observation(
        &self,
        client_id: ClientId,
        operation: WorkflowApplicationOperation,
        scope: &dto::ExecutionWatchScope,
    ) -> Result<(String, Option<std::time::Instant>), dto::ExecutionWatchError> {
        let result = self
            .authorize_execution_observation_inner(client_id, operation, scope)
            .await;
        if result.is_err() {
            // Authorization withdrawal ends retention rather than leaving deep capture
            // enabled until the next materialization hook. No execution control occurs.
            let _ = self
                .xray_execution_watches
                .revoke_principal(&client_id.to_string());
        }
        result
    }

    async fn authorize_execution_observation_inner(
        &self,
        client_id: ClientId,
        operation: WorkflowApplicationOperation,
        scope: &dto::ExecutionWatchScope,
    ) -> Result<(String, Option<std::time::Instant>), dto::ExecutionWatchError> {
        let (selected_context, deadline) = self
            .authorize_execution_observation_scope(client_id, operation, scope)
            .await?;
        // Discovery is only a bounded consistency check AFTER explicit policy approval;
        // it never grants diagnostic access and never resolves another state location.
        let run = crate::workflow_representation::status(self, &scope.root_run_id)
            .await
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?
            .ok_or(dto::ExecutionWatchError::Forbidden)?;
        let parent = run
            .parent_session_id
            .ok_or(dto::ExecutionWatchError::Forbidden)?;
        let root_session_id = parent.clone();
        let parent = parent
            .parse()
            .map_err(|_| dto::ExecutionWatchError::Unavailable)?;
        if self
            .active_session_namespace_mismatch(parent, client_id)
            .await
            .is_some()
        {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        // Recheck registration after asynchronous canonical reads, before capture/read.
        if self.execution_observation_caller(client_id).await? != selected_context {
            return Err(dto::ExecutionWatchError::Forbidden);
        }
        Ok((root_session_id, deadline))
    }
}

const fn version(value: u16) -> Result<(), dto::ExecutionWatchError> {
    if value == dto::EXECUTION_WATCH_SCHEMA_VERSION {
        Ok(())
    } else {
        Err(dto::ExecutionWatchError::InvalidRequest)
    }
}
