impl ManagedSessionAdmission for CanonicalManagedAdmission {
    fn retained_result<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, Option<HarnessServiceResponse>> {
        Box::pin(async move {
            call.harness
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            // Read-only/auth requests are not retained mutations. Fresh open continues through
            // creation admission, which owns ordinary session publication and parent checks.
            if !matches!(
                &call.harness.kind,
                HarnessRequestKind::Submit { .. }
                    | HarnessRequestKind::Respond { .. }
                    | HarnessRequestKind::Interrupt(_)
                    | HarnessRequestKind::Close(_)
                    | HarnessRequestKind::Configure(_)
            ) {
                return Ok(None);
            }
            if call.assistance_destination_session_id.is_some() {
                return Err(ManagedHarnessError::Unsupported);
            }
            let session_id = target(call)?;
            let (cwd, workflow_hold) = self
                .authorize_existing_call(caller, call, session_id)
                .await?;
            let state = self.store.state()?;
            let summary = state
                .sessions
                .session_summary(session_id)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
            if canonical_directory(&summary.working_directory).await? != cwd {
                return Err(ManagedHarnessError::StaleOwner);
            }
            if call.parent_session_id.as_ref().is_some_and(|parent| {
                summary.parent_id.map(|id| id.to_string()).as_ref() != Some(parent)
            }) {
                return Err(ManagedHarnessError::Conflict);
            }
            let selected = state
                .sessions
                .session_external_harness_state(session_id)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?
                .ok_or(ManagedHarnessError::Conflict)?;
            // Validate actual durable association without translating the historical daemon
            // into this daemon's execution authority. Prepared retries may predate confirmation.
            CanonicalHarnessStore::owner_from_snapshot(session_id, &selected)?;
            let supplied = match &call.harness.kind {
                HarnessRequestKind::Submit { target, .. }
                | HarnessRequestKind::Respond { target, .. } => target,
                HarnessRequestKind::Interrupt(control) | HarnessRequestKind::Close(control) => {
                    &control.target
                }
                HarnessRequestKind::Configure(configure) => &configure.target,
                _ => return Err(ManagedHarnessError::Invalid),
            };
            supplied
                .validate_owner(&session_id.to_string(), selected.owner.generation)
                .map_err(|_| ManagedHarnessError::StaleOwner)?;
            if selected.instance != call.harness.instance {
                return Err(ManagedHarnessError::Conflict);
            }
            validate_retained_target(&selected, supplied)?;
            Self::enabled(&state, &selected.instance)?;
            let retained = state
                .sessions
                .read_external_harness_retained_request(session_id, &call.harness)
                .await
                .map_err(|error| match error {
                    bcode_session::SessionError::EventSerialization(_) => {
                        ManagedHarnessError::Conflict
                    }
                    _ => ManagedHarnessError::Storage,
                })?;
            drop(state);
            drop(workflow_hold);
            // Recheck exact caller authority after canonical I/O; historical evidence never
            // transfers execution ownership or authorizes replay.
            let (_, rechecked) = self
                .authorize_existing_call(caller, call, session_id)
                .await?;
            drop(rechecked);
            match retained {
                Some(bcode_session_models::ExternalHarnessClaim::Recorded(response)) => {
                    let response = response
                        .response()
                        .map_err(|_| ManagedHarnessError::Conflict)?;
                    response
                        .validate_for(&call.harness)
                        .map_err(|_| ManagedHarnessError::Conflict)?;
                    Ok(Some(response))
                }
                Some(bcode_session_models::ExternalHarnessClaim::Uncertain) => {
                    Err(ManagedHarnessError::Uncertain)
                }
                Some(bcode_session_models::ExternalHarnessClaim::New) => {
                    Err(ManagedHarnessError::Invalid)
                }
                None => Ok(None),
            }
        })
    }

    fn existing<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, HarnessOwner> {
        self.existing_tool_authority(caller, call, false)
    }

    fn create_selected<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        request: &'a bcode_ipc::harness::ManagedSelectedStartRequest,
    ) -> Task<'a, HarnessOwner> {
        Box::pin(async move {
            let owner = self.selected_open_owner(caller, request).await?;
            let native = bcode_harness_models::HarnessModelsRequest {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.open.open_id.clone(),
                instance: request.instance.clone(),
                kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
                    open: Box::new(request.open.clone()),
                    selection: bcode_harness_models::HarnessModelSelection {
                        model_id: request.selection.model_id.clone(),
                    },
                },
            };
            if self
                .store
                .prepared_selected_open(&owner, &native, request.parent_session_id)
                .await?
            {
                return Ok(owner);
            }
            let inspection = self.inspect_selected_creation(request).await?;
            self.store
                .prepare_selected_open(&owner, &native, &inspection, request.parent_session_id)
                .await?;
            Ok(owner)
        })
    }

    fn create<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
    ) -> Task<'a, HarnessOwner> {
        Box::pin(self.prepare_open(caller, call))
    }

    fn auth_scope<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        session_id: SessionId,
        instance_id: &'a str,
    ) -> Task<'a, crate::harness_dispatch::ManagedAuthAdmission> {
        Box::pin(async move { self.authorize_auth(caller, session_id, instance_id).await })
    }
}

impl CanonicalManagedAdmission {
    // Only already-issued tool grants may follow a verified confirmation of their
    // original prepared open. Fresh dispatch continues to require an exact target.
    pub(super) fn existing_tool_authority<'a>(
        &'a self,
        caller: &'a ManagedCaller,
        call: &'a ManagedSessionCall,
        retained_grant: bool,
    ) -> Task<'a, HarnessOwner> {
        Box::pin(async move {
            call.harness
                .validate()
                .map_err(|_| ManagedHarnessError::Invalid)?;
            if call.assistance_destination_session_id.is_some() {
                // A destination hint is not a host-admitted route. Do not silently discard it.
                return Err(ManagedHarnessError::Unsupported);
            }
            let session_id = target(call)?;
            let (cwd, workflow_hold) = self
                .authorize_existing_call(caller, call, session_id)
                .await.inspect_err(|_| { tracing::warn!(target: "bcode_server::harness", category = "retained_caller_authority", "managed admission rejected"); })?;
            let state = self.store.state()?;
            let summary = state
                .sessions
                .session_summary(session_id)
                .await
                .map_err(|_| ManagedHarnessError::Storage)?;
            if canonical_directory(&summary.working_directory).await? != cwd {
                tracing::warn!(target: "bcode_server::harness", category = "retained_workspace", "managed admission rejected");
                return Err(ManagedHarnessError::StaleOwner);
            }
            if call.parent_session_id.as_ref().is_some_and(|parent| {
                summary.parent_id.map(|id| id.to_string()).as_ref() != Some(parent)
            }) {
                return Err(ManagedHarnessError::Conflict);
            }
            let generation = match &call.harness.kind {
                HarnessRequestKind::Open(value) => value.generation,
                HarnessRequestKind::Submit { submission, .. } => submission.generation,
                HarnessRequestKind::Respond { response, .. } => response.generation,
                HarnessRequestKind::Observe(value) => value.target.generation(),
                HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
                    value.target.generation()
                }
                HarnessRequestKind::Configure(value) => value.target.generation(),
                HarnessRequestKind::Reconcile(value) => value.generation,
                HarnessRequestKind::Inspect => {
                    self.store.current_owner(session_id).await?.generation
                }
                _ => return Err(ManagedHarnessError::Unsupported),
            };
            // Supplied generation is only a lookup candidate; selected() compares it with
            // canonical full owner in the serialized actor before granting any operation.
            let owner = Self::daemon_owner(&state, session_id, generation)?;
            let selected = self.store.selected(&owner).await.inspect_err(|error| {
                tracing::warn!(target: "bcode_server::harness", stage = "existing_selected_owner", category = %error, "managed canonical admission failed");
            })?;
            let supplied_target = match &call.harness.kind {
                HarnessRequestKind::Submit { target, .. }
                | HarnessRequestKind::Respond { target, .. } => Some(target),
                HarnessRequestKind::Observe(value) => Some(&value.target),
                HarnessRequestKind::Interrupt(value) | HarnessRequestKind::Close(value) => {
                    Some(&value.target)
                }
                HarnessRequestKind::Configure(value) => Some(&value.target),
                _ => None,
            };
            let supplied_binding = match &call.harness.kind {
                HarnessRequestKind::Open(value) => value.resume.as_ref(),
                HarnessRequestKind::Reconcile(value) => value.binding.as_ref(),
                _ => None,
            };
            if selected.instance != call.harness.instance
                || supplied_binding.is_some_and(|value| selected.binding.as_ref() != Some(value))
            {
                tracing::warn!(target: "bcode_server::harness", category = "retained_instance_or_binding", "managed admission rejected");
                return Err(ManagedHarnessError::Conflict);
            }
            if let Some(value) = supplied_target {
                if retained_grant {
                    validate_retained_target(&selected, value).inspect_err(|_| { tracing::warn!(target: "bcode_server::harness", category = "retained_target_confirmation", "managed admission rejected"); })?;
                } else if selected.target.as_ref() != Some(value) {
                    return Err(ManagedHarnessError::Conflict);
                }
            }
            Self::enabled(&state, &selected.instance).inspect_err(|_| { tracing::warn!(target: "bcode_server::harness", category = "retained_plugin_enabled", "managed admission rejected"); })?;
            drop(state);
            // Acquire is mandatory here as well as at runtime: admission never converts a
            // foreign daemon binding into this daemon's authority merely by rewriting its ID.
            let _ownership = self.store.acquire(&owner).await.inspect_err(|_| { tracing::warn!(target: "bcode_server::harness", category = "retained_owner_acquire", "managed admission rejected"); })?;
            drop(workflow_hold);
            Ok(owner)
        })
    }
}
