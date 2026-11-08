//! Caller-bound execution discovery. Native model advertisement is not entitlement.
use std::sync::Arc;

use bcode_harness_models::{
    HARNESS_SCHEMA_VERSION, HarnessModelsRequest, HarnessModelsRequestKind,
    HarnessModelsResponseKind,
};
use bcode_ipc::harness::{
    ManagedExecutionCatalog, ManagedExecutionCatalogIssue, ManagedExecutionCatalogRequest,
    ManagedSelectedStartRequest, ManagedSelectedStartResponse,
};
use bcode_model::execution::{
    ExecutionBillingRoute, ExecutionModelProvenance, ExecutionModelReadiness,
};
use bcode_model_catalog::execution::{
    ManagedHarnessExecutionCatalog, ManagedHarnessModelObservation, ProviderExecutionCatalog,
};

use super::{HarnessPluginTransport, ManagedHarnessError};
use crate::harness_dispatch::{ManagedCaller, ManagedSessionAdmission};

pub async fn discover_candidates(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    query: bcode_ipc::harness::ManagedCandidatesRequest,
) -> Result<bcode_ipc::harness::ManagedCandidatesResponse, ManagedHarnessError> {
    query.validate().map_err(|_| ManagedHarnessError::Invalid)?;
    let ManagedCaller::Client(client_id) = caller else {
        return Err(ManagedHarnessError::Unsupported);
    };
    if state.client_runtime_context(*client_id).await.is_none() {
        return Err(ManagedHarnessError::StaleOwner);
    }
    let mut plugins = Vec::new();
    let mut complete = true;
    let transport = HarnessPluginTransport::with_view(
        Arc::clone(state),
        super::configuration_views::for_client(state, *client_id).await?,
    );
    let native = bcode_harness_models::HarnessCandidatesRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: query.request_id.clone(),
    };
    let producers: Vec<_> = state
        .plugins
        .registry()
        .manifests()
        .iter()
        .filter(|(_, manifest)| {
            manifest.services.iter().any(|service| {
                service.interface_id == bcode_harness_models::HARNESS_CANDIDATES_INTERFACE_ID
            })
        })
        .map(|(plugin_id, _)| plugin_id.clone())
        .take(17)
        .collect();
    if producers.len() > 16 {
        complete = false;
    }
    for plugin_id in producers.into_iter().take(16) {
        let response = transport.discover_candidates(&plugin_id, &native).await;
        plugins.push(match response {
            Ok(response) => response,
            Err(_) => bcode_harness_models::HarnessCandidatesResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: query.request_id.clone(),
                plugin_id,
                kind: bcode_harness_models::HarnessCandidatesResponseKind::Failure(
                    bcode_harness_models::HarnessFailure::Unavailable,
                ),
            },
        });
    }
    drop(transport);
    Ok(bcode_ipc::harness::ManagedCandidatesResponse {
        request_id: query.request_id,
        plugins,
        complete,
    })
}

pub async fn discover(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    request: ManagedExecutionCatalogRequest,
) -> Result<ManagedExecutionCatalog, ManagedHarnessError> {
    request
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let mut workflow_guard = None;
    let client_id = match caller {
        ManagedCaller::Client(client_id) => {
            if state.client_runtime_context(*client_id).await.is_none() {
                return Err(ManagedHarnessError::StaleOwner);
            }
            Some(*client_id)
        }
        ManagedCaller::Workflow {
            run_id,
            dispatch_identity,
            session_id,
            ..
        } => {
            if request.provider_plugin_id.is_some() {
                return Err(ManagedHarnessError::Unsupported);
            }
            workflow_guard = Some(
                crate::harness_workflow::authorize_selected_discovery(
                    state,
                    run_id,
                    dispatch_identity,
                    *session_id,
                )
                .await
                .map_err(|_| ManagedHarnessError::StaleOwner)?,
            );
            None
        }
        ManagedCaller::Invocation { .. } => return Err(ManagedHarnessError::Unsupported),
    };
    let _workflow_guard = workflow_guard;
    let mut issues = Vec::new();
    let mut providers = Vec::new();
    if let Some(provider) = request.provider_plugin_id.clone() {
        let client_id = client_id.ok_or(ManagedHarnessError::Unsupported)?;
        match crate::session_operations::list_models(state, client_id, Some(provider.clone())).await
        {
            Ok((_, list)) => providers.push(ProviderExecutionCatalog {
                provider_plugin_id: provider,
                list,
                selected_model_id: None,
                configured_model_id: None,
                account_reference: None,
                billing_route: ExecutionBillingRoute::ProviderApi,
                readiness: std::collections::BTreeMap::new(),
            }),
            Err(_) => issues.push(ManagedExecutionCatalogIssue {
                instance_id: None,
                code: "provider_catalog_unavailable".into(),
            }),
        }
    }
    let harnesses = discover_harnesses(
        state,
        &request.request_id,
        request.instances,
        &mut issues,
        super::configuration_views::for_caller(state, caller).await?,
    )
    .await;
    let models = state
        .model_catalog
        .resolve_execution_catalog(providers, harnesses)
        .await;
    Ok(ManagedExecutionCatalog {
        request_id: request.request_id,
        models,
        issues,
    })
}

/// Reconstruct a descriptive selection from exact canonical acknowledgment, not current
/// readiness or authority. No discovery or native control is repeated for a completed retry.
async fn resolve_recorded_selection(
    state: &Arc<crate::ServerState>,
    request: &ManagedSelectedStartRequest,
    response: &bcode_harness_models::HarnessModelsResponse,
) -> Result<bcode_model::execution::ResolvedExecutionModel, ManagedHarnessError> {
    let HarnessModelsResponseKind::Selected { evidence, .. } = &response.kind else {
        return Err(ManagedHarnessError::Unavailable);
    };
    let model = bcode_model::ModelInfo {
        model_id: evidence.requested_model_id.clone(),
        display_name: evidence.requested_model_id.clone(),
        is_default: false,
        context_window: None,
        max_output_tokens: None,
        max_image_input_base64_bytes: None,
        capabilities: std::collections::BTreeSet::default(),
        feature_support: bcode_model::ModelFeatureSupport::default(),
        reasoning: None,
        cache: bcode_model::ModelCacheInfo::default(),
        metadata_source: None,
        pricing: None,
        api_surface: None,
        visibility: bcode_model::ModelVisibility::default(),
    };
    let entries = state
        .model_catalog
        .resolve_execution_catalog(
            Vec::new(),
            vec![ManagedHarnessExecutionCatalog {
                instance_id: request.instance.instance_id.clone(),
                plugin_id: request.instance.plugin_id.clone(),
                account_reference: Some(request.instance.auth.profile.clone()),
                billing_route: ExecutionBillingRoute::Unknown,
                models: vec![ManagedHarnessModelObservation {
                    model,
                    native_model_id: evidence.requested_model_id.clone(),
                    confirmed_model_id: evidence.effective_model_id.clone(),
                    provenance: ExecutionModelProvenance::HarnessDiscovery,
                    // The retained selected acknowledgment proves historical route readiness only.
                    // This reconstruction is returned as evidence; it never authorizes another call.
                    readiness: ExecutionModelReadiness::RouteReady {
                        model_access: bcode_model::execution::ExecutionModelAccess::Unknown,
                    },
                }],
            }],
        )
        .await;
    bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
        &entries,
        request.selection.clone(),
    )
    .map_err(|_| ManagedHarnessError::Conflict)
}

async fn discover_harnesses(
    state: &Arc<crate::ServerState>,
    request_id: &str,
    instances: Vec<bcode_harness_models::HarnessInstance>,
    issues: &mut Vec<ManagedExecutionCatalogIssue>,
    view: bcode_plugin::PluginServiceConfigView,
) -> Vec<ManagedHarnessExecutionCatalog> {
    let mut harnesses = Vec::new();
    for instance in instances {
        let transport = HarnessPluginTransport::with_view(Arc::clone(state), view.clone());
        let native = HarnessModelsRequest {
            schema_version: HARNESS_SCHEMA_VERSION,
            request_id: request_id.to_owned(),
            instance: instance.clone(),
            kind: HarnessModelsRequestKind::DiscoverModels,
        };
        match transport.discover_models(&native).await {
            Ok(response) => match response.kind {
                HarnessModelsResponseKind::Models { models, .. } => {
                    let readiness = match transport.inspect_auth_readonly(&instance).await {
                        Ok(inspected)
                            if matches!(
                                inspected.status.readiness,
                                bcode_provider_auth_models::managed::AuthReadiness::Ready
                                    | bcode_provider_auth_models::managed::AuthReadiness::Verified
                            ) =>
                        {
                            bcode_model::execution::ExecutionModelReadiness::RouteReady {
                                model_access: bcode_model::execution::ExecutionModelAccess::Unknown,
                            }
                        }
                        _ => ExecutionModelReadiness::Unknown,
                    };
                    drop(transport);
                    let models = models
                        .into_iter()
                        .map(|model| {
                            // Missing capabilities, prices and context limits stay unknown.
                            let info = bcode_model::ModelInfo {
                                model_id: model.model_id.clone(),
                                display_name: model.display_name,
                                is_default: false,
                                context_window: None,
                                max_output_tokens: None,
                                max_image_input_base64_bytes: None,
                                capabilities: std::collections::BTreeSet::new(),
                                feature_support: bcode_model::ModelFeatureSupport::default(),
                                reasoning: None,
                                cache: bcode_model::ModelCacheInfo::default(),
                                metadata_source: None,
                                pricing: None,
                                api_surface: None,
                                visibility: bcode_model::ModelVisibility::default(),
                            };
                            ManagedHarnessModelObservation {
                                model: info,
                                native_model_id: model.model_id,
                                confirmed_model_id: None,
                                provenance: ExecutionModelProvenance::HarnessDiscovery,
                                readiness,
                            }
                        })
                        .collect();
                    harnesses.push(ManagedHarnessExecutionCatalog {
                        instance_id: instance.instance_id,
                        plugin_id: instance.plugin_id,
                        account_reference: Some(instance.auth.profile),
                        // Credential custody does not independently establish billing mode.
                        billing_route: ExecutionBillingRoute::Unknown,
                        models,
                    });
                }
                HarnessModelsResponseKind::Failure(_)
                | HarnessModelsResponseKind::Selected { .. } => {
                    issues.push(ManagedExecutionCatalogIssue {
                        instance_id: Some(instance.instance_id),
                        code: "native_catalog_unavailable".into(),
                    });
                }
            },
            Err(_) => issues.push(ManagedExecutionCatalogIssue {
                instance_id: Some(instance.instance_id),
                code: "native_catalog_unavailable".into(),
            }),
        }
    }
    harnesses
}

async fn recorded_selected_start(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    request: &ManagedSelectedStartRequest,
    admission: &super::CanonicalManagedAdmission,
    session_id: bcode_session_models::SessionId,
    native: &HarnessModelsRequest,
) -> Result<Option<ManagedSelectedStartResponse>, ManagedHarnessError> {
    if state.sessions.session_summary(session_id).await.is_ok() {
        admission
            .authorize_selected_history(caller, request)
            .await?;
        let retained = state
            .sessions
            .read_external_harness_retained_selected_request(session_id, native)
            .await
            .map_err(|_| ManagedHarnessError::Conflict)?;
        if let Some(retained) = retained {
            use bcode_session_models::external_harness::{
                ExternalHarnessClaim, ExternalHarnessRequest,
            };
            match retained {
                ExternalHarnessClaim::Recorded(response) => {
                    let durable = ExternalHarnessRequest::from_models_request(native)
                        .map_err(|_| ManagedHarnessError::Invalid)?;
                    let response = response
                        .models_response(&durable)
                        .map_err(|_| ManagedHarnessError::Conflict)?;
                    if let bcode_harness_models::HarnessModelsResponseKind::Failure(failure) =
                        &response.kind
                    {
                        return Err(match failure {
                            bcode_harness_models::HarnessFailure::Uncertain => {
                                ManagedHarnessError::Uncertain
                            }
                            _ => ManagedHarnessError::Unavailable,
                        });
                    }
                    let selection = resolve_recorded_selection(state, request, &response).await?;
                    return Ok(Some(ManagedSelectedStartResponse {
                        selection,
                        response,
                    }));
                }
                ExternalHarnessClaim::Uncertain => return Err(ManagedHarnessError::Uncertain),
                ExternalHarnessClaim::New => return Err(ManagedHarnessError::Conflict),
            }
        }
    }
    Ok(None)
}

const fn require_selected_storage_contract() -> Result<(), ManagedHarnessError> {
    if bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
        < bcode_session_models::external_harness::SELECTED_OPEN_EVENT_SCHEMA_VERSION
        || bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH
            < bcode_session_models::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH
    {
        return Err(ManagedHarnessError::Unsupported);
    }
    Ok(())
}

pub async fn start_selected(
    state: &Arc<crate::ServerState>,
    caller: &ManagedCaller,
    request: ManagedSelectedStartRequest,
) -> Result<ManagedSelectedStartResponse, ManagedHarnessError> {
    request
        .validate()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    let _workflow_guard = match caller {
        ManagedCaller::Client(client_id) => {
            if state.client_runtime_context(*client_id).await.is_none() {
                return Err(ManagedHarnessError::StaleOwner);
            }
            None
        }
        ManagedCaller::Workflow {
            run_id,
            dispatch_identity,
            session_id,
            plugin_id,
        } => Some(
            crate::harness_workflow::authorize_selected_start(
                state,
                run_id,
                dispatch_identity,
                *session_id,
                plugin_id,
                &request,
            )
            .await
            .map_err(|_| ManagedHarnessError::StaleOwner)?,
        ),
        ManagedCaller::Invocation { .. } => return Err(ManagedHarnessError::Unsupported),
    };
    require_selected_storage_contract()?;
    let backend = state
        .managed_sessions
        .get()
        .ok_or(ManagedHarnessError::Unavailable)?;
    // Compare immutable selected Open in canonical admission before discovery/auth/native I/O.
    // The same prepare is repeated atomically by execute_selected before claiming delivery.
    let admission = super::CanonicalManagedAdmission::new(super::CanonicalHarnessStore::new(state));
    let native = HarnessModelsRequest {
        schema_version: HARNESS_SCHEMA_VERSION,
        request_id: request.open.open_id.clone(),
        instance: request.instance.clone(),
        kind: HarnessModelsRequestKind::OpenSelected {
            open: Box::new(request.open.clone()),
            selection: bcode_harness_models::HarnessModelSelection {
                model_id: request.selection.model_id.clone(),
            },
        },
    };
    let session_id = request
        .open
        .session_id
        .parse()
        .map_err(|_| ManagedHarnessError::Invalid)?;
    if let Some(recorded) =
        recorded_selected_start(state, caller, &request, &admission, session_id, &native).await?
    {
        return Ok(recorded);
    }
    let owner = admission.create_selected(caller, &request).await?;
    drop(admission);
    // Discovery for an already prepared open uses its pinned view, not a later client reload.
    let mut issues = Vec::new();
    let harnesses = discover_harnesses(
        state,
        &request.open.open_id,
        vec![request.instance.clone()],
        &mut issues,
        super::configuration_views::for_owner(state, &owner).await?,
    )
    .await;
    let models = state
        .model_catalog
        .resolve_execution_catalog(Vec::new(), harnesses)
        .await;
    let selection = bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
        &models,
        request.selection.clone(),
    )
    .map_err(|_| ManagedHarnessError::Unavailable)?;
    backend.execute_selected(caller, request, selection).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_publication_follows_canonical_recovery_storage() {
        assert_eq!(
            super::super::recovery_published(),
            bcode_session_models::external_harness::recovery_storage_available()
        );
    }

    #[tokio::test]
    async fn candidate_discovery_requires_authenticated_context_without_native_inputs() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let result = discover_candidates(
            &state,
            &ManagedCaller::Client(bcode_session_models::ClientId::new()),
            bcode_ipc::harness::ManagedCandidatesRequest {
                request_id: "discover".into(),
            },
        )
        .await;
        assert!(matches!(result, Err(ManagedHarnessError::StaleOwner)));
        drop(state);
    }

    #[tokio::test]
    async fn empty_catalog_never_discovers_an_ambient_provider() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let client_id = bcode_session_models::ClientId::new();
        state
            .client_runtime_contexts
            .lock()
            .await
            .insert(client_id, crate::ClientRuntimeContext::default());
        let catalog = discover(
            &state,
            &ManagedCaller::Client(client_id),
            ManagedExecutionCatalogRequest {
                request_id: "explicit-empty".into(),
                instances: Vec::new(),
                provider_plugin_id: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(catalog.request_id, "explicit-empty");
        assert_eq!(
            catalog.models,
            [] as [bcode_model::execution::ExecutionModel; 0]
        );
        assert!(catalog.issues.is_empty());
        drop(state);
    }

    #[tokio::test]
    async fn selected_start_cannot_fall_through_to_unselected_creation() {
        if bcode_session_models::CURRENT_SESSION_EVENT_SCHEMA_VERSION
            >= bcode_session_models::external_harness::SELECTED_OPEN_EVENT_SCHEMA_VERSION
            && bcode_session_models::CURRENT_SESSION_STORAGE_WRITER_EPOCH
                >= bcode_session_models::external_harness::SELECTED_OPEN_STORAGE_WRITER_EPOCH
        {
            return;
        }
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let client_id = bcode_session_models::ClientId::new();
        state
            .client_runtime_contexts
            .lock()
            .await
            .insert(client_id, crate::ClientRuntimeContext::default());
        let session_id = bcode_session_models::SessionId::new();
        let instance = bcode_harness_models::HarnessInstance {
            instance_id: "configured".into(),
            plugin_id: "native".into(),
            executable: "/native".into(),
            config_namespace: "/account".into(),
            auth: bcode_harness_models::HarnessAuthBinding {
                provider_id: "native-auth".into(),
                profile: "account".into(),
                credential_namespace: "/account".into(),
                custody: bcode_harness_models::HarnessAuthCustody::NativeManaged,
            },
        };
        let request = ManagedSelectedStartRequest {
            selection: bcode_model::execution::ExecutionModelSelection {
                model_id: "advertised-key".into(),
                route: Some(bcode_model::execution::ExecutionRoute::ManagedHarness {
                    instance_id: instance.instance_id.clone(),
                    plugin_id: instance.plugin_id.clone(),
                }),
                account_reference: None,
            },
            instance,
            open: bcode_harness_models::HarnessOpenRequest {
                open_id: "selected-open".into(),
                session_id: session_id.to_string(),
                generation: 1,
                working_directory: "/workspace".into(),
                resume: None,
                parent_assistance: None,
            },
            parent_session_id: None,
        };
        assert!(matches!(
            start_selected(&state, &ManagedCaller::Client(client_id), request).await,
            Err(ManagedHarnessError::Unsupported)
        ));
        assert!(state.sessions.session_summary(session_id).await.is_err());
        drop(state);
    }

    #[tokio::test]
    async fn discovery_requires_authenticated_client_context() {
        let state = Arc::new(crate::tests::test_server_state(
            crate::SessionManager::default(),
        ));
        let result = discover(
            &state,
            &ManagedCaller::Client(bcode_session_models::ClientId::new()),
            ManagedExecutionCatalogRequest {
                request_id: "unowned".into(),
                instances: Vec::new(),
                provider_plugin_id: None,
            },
        )
        .await;
        assert!(matches!(result, Err(ManagedHarnessError::StaleOwner)));
        drop(state);
    }
}
