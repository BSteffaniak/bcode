//! Exact workflow selection preparation. Discovery and native delivery remain host-owned.
//!
//! A prepared selection is not a receipt, native acceptance, or permission to dispatch. The
//! prompt owner must retain it in its canonical intent before selected Open or assignment I/O.

use bcode_model::execution::{ExecutionModel, ExecutionRoute, ResolvedExecutionModel};
use bcode_workflow::{PromptContextTarget, WorkflowPromptConfiguration};
use bcode_workflow_store::WorkflowStoreError;

fn invalid(message: &str) -> WorkflowStoreError {
    WorkflowStoreError::InvalidData(message.into())
}

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Instances {
    default_instance: Option<String>,
    instances: std::collections::BTreeMap<String, bcode_harness_models::HarnessInstance>,
}

async fn plan_provider(
    state: &std::sync::Arc<crate::ServerState>,
    parent: bcode_session_models::SessionId,
    configuration: &WorkflowPromptConfiguration,
    selection: &bcode_workflow::ExecutionModelSelection,
    provider_plugin_id: &str,
) -> Result<
    Option<(
        ResolvedExecutionModel,
        Option<bcode_harness_models::HarnessInstance>,
    )>,
    WorkflowStoreError,
> {
    let runtime = crate::workflow_runtime_context(state, parent).await;
    let inherited =
        crate::session_model_selection_with_runtime_context(state, parent, runtime).await;
    if inherited.provider_plugin_id.as_deref() != Some(provider_plugin_id) {
        return Err(invalid(
            "selected provider workflow requires an authenticated context for that provider",
        ));
    }
    // Do not convert an account reference into an auth override or pick a pool candidate.
    // Omission inherits the complete ordinary auth/pool context, including runtime choice.
    if let Some(account) = &selection.account_reference
        && (inherited.provider_context.auth_profile.as_ref() != Some(account)
            || !inherited.provider_context.auth_candidates.is_empty())
    {
        return Err(invalid(
            "selected provider account does not match the inherited exact account",
        ));
    }
    let list = crate::resolved_provider_models_view(
        state,
        Some(provider_plugin_id.to_owned()),
        bcode_model::ModelListRequest {
            provider_context: inherited.provider_context,
            selected_model_id: inherited.model_id,
        },
        bcode_model_catalog::ModelListView::UserVisible,
    )
    .await
    .map_err(|_| invalid("selected provider catalog unavailable"))?;
    let readiness = list
        .models
        .iter()
        .map(|model| {
            (
                model.model_id.clone(),
                bcode_model::execution::ExecutionModelReadiness::Ready,
            )
        })
        .collect();
    let entries = state
        .model_catalog
        .resolve_execution_catalog(
            vec![bcode_model_catalog::execution::ProviderExecutionCatalog {
                provider_plugin_id: provider_plugin_id.to_owned(),
                list,
                selected_model_id: None,
                configured_model_id: None,
                account_reference: selection.account_reference.clone(),
                billing_route: bcode_model::execution::ExecutionBillingRoute::ProviderApi,
                readiness,
            }],
            Vec::new(),
        )
        .await;
    let resolved =
        resolve(configuration, &entries)?.ok_or_else(|| invalid("provider selection missing"))?;

    Ok(Some((resolved, None)))
}

fn require_explicit_route(
    selection: &bcode_workflow::ExecutionModelSelection,
) -> Result<(), WorkflowStoreError> {
    if selection.route.is_none() {
        // This workflow discovery API cannot enumerate every authenticated provider account.
        // A harness-only subset must not conceal a same-model provider ambiguity.
        return Err(invalid(
            "workflow execution selection requires an explicit route until unified caller inventory is available",
        ));
    }
    Ok(())
}

/// Resolve selection during canonical prompt planning, before child creation or native mutation.
/// Host discovery retains this workflow caller; it must not substitute an attached client.
pub async fn plan(
    state: &std::sync::Arc<crate::ServerState>,
    request: &bcode_workflow_store::PreparedActivationDispatch,
    configuration: &WorkflowPromptConfiguration,
) -> Result<
    Option<(
        ResolvedExecutionModel,
        Option<bcode_harness_models::HarnessInstance>,
    )>,
    WorkflowStoreError,
> {
    use crate::harness_dispatch::ManagedCaller;
    let activation = &request.activation;
    let Some(selection) = &configuration.execution_model_selection else {
        return Ok(None);
    };
    require_explicit_route(selection)?;
    let run = crate::workflow_representation::status(state, &activation.run_id)
        .await
        .map_err(|_| invalid("workflow selection run unavailable"))?
        .ok_or_else(|| invalid("workflow selection run missing"))?;
    let parent = run
        .parent_session_id
        .as_deref()
        .ok_or_else(|| invalid("workflow selection parent missing"))?
        .parse()
        .map_err(|_| invalid("invalid workflow selection parent"))?;
    let guard = crate::harness_workflow::authorize_selected_discovery(
        state,
        &activation.run_id,
        &request.dispatch_identity,
        parent,
    )
    .await?;
    if let Some(ExecutionRoute::ModelProvider { provider_plugin_id }) = &selection.route {
        let result =
            plan_provider(state, parent, configuration, selection, provider_plugin_id).await;
        drop(guard);
        return result;
    }
    let (instances, provider) =
        if let Some(ExecutionRoute::ModelProvider { provider_plugin_id }) = &selection.route {
            (Vec::new(), Some(provider_plugin_id.clone()))
        } else {
            let catalog: Instances = state
                .plugins
                .invoke_service_by_interface_json(
                    "bcode.harness.configuration/v1",
                    "catalog",
                    &serde_json::json!({}),
                )
                .await
                .map_err(|_| invalid("configured harness inventory unavailable"))?;
            let _ = catalog.default_instance;
            (catalog.instances.into_values().collect::<Vec<_>>(), None)
        };
    let plugin_id = match &selection.route {
        Some(ExecutionRoute::ManagedHarness { plugin_id, .. }) => plugin_id.clone(),
        _ => "bcode.workflow".into(),
    };
    let caller = ManagedCaller::Workflow {
        run_id: activation.run_id.clone(),
        dispatch_identity: request.dispatch_identity.clone(),
        session_id: parent,
        plugin_id,
    };
    let catalog = crate::harness::execution_catalog::discover(
        state,
        &caller,
        bcode_ipc::harness::ManagedExecutionCatalogRequest {
            request_id: match &caller {
                ManagedCaller::Workflow {
                    dispatch_identity, ..
                } => dispatch_identity.clone(),
                _ => unreachable!(),
            },
            instances: instances.clone(),
            provider_plugin_id: provider,
        },
    )
    .await
    .map_err(|_| invalid("workflow execution catalog unavailable"))?;
    if !catalog.issues.is_empty() {
        return Err(invalid("workflow execution discovery is incomplete"));
    }
    let resolved = resolve(configuration, &catalog.models)?
        .ok_or_else(|| invalid("workflow selection missing"))?;
    let instance = match &resolved.target.route {
        ExecutionRoute::ManagedHarness {
            instance_id,
            plugin_id,
        } => Some(
            instances
                .into_iter()
                .find(|instance| {
                    &instance.instance_id == instance_id
                        && &instance.plugin_id == plugin_id
                        && resolved.target.account_reference.as_deref()
                            == Some(instance.auth.profile.as_str())
                })
                .ok_or_else(|| invalid("resolved harness instance unavailable"))?,
        ),
        ExecutionRoute::ModelProvider { .. } => None,
    };
    drop(guard);
    Ok(Some((resolved, instance)))
}

/// Resolve one authenticated, host-discovered inventory without backend/account fallback.
///
/// The caller must supply the catalog for its retained configuration/auth context. This does not
/// discover an ambient provider or materialize credentials. Unknown model capabilities must be
/// checked by host execution admission; they are never promoted to supported capabilities here.
///
/// # Errors
/// Rejects invalid configuration, ambiguous/unready selection and managed non-fresh execution.
pub fn resolve(
    configuration: &WorkflowPromptConfiguration,
    catalog: &[ExecutionModel],
) -> Result<Option<ResolvedExecutionModel>, WorkflowStoreError> {
    configuration
        .validate()
        .map_err(|_| invalid("invalid workflow execution configuration"))?;
    let Some(selection) = configuration.execution_model_selection.clone() else {
        return Ok(None);
    };
    let resolved =
        bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(catalog, selection)
            .map_err(|_| {
                invalid("workflow execution selection is unavailable, ambiguous or unready")
            })?;
    if matches!(
        &resolved.target.route,
        ExecutionRoute::ManagedHarness { .. }
    ) && configuration.execution_target != PromptContextTarget::FreshIsolated
    {
        return Err(invalid(
            "selected managed execution requires fresh isolated context",
        ));
    }
    Ok(Some(resolved))
}

/// Prepare an exact managed fresh Start from a resolved catalog member.
///
/// The host must validate the supplied instance against the authenticated configuration and
/// admit tool/output capabilities before creation. The workflow owner must publish exact Open
/// correlation under its execution authority before delivery; calling this function has no I/O.
///
/// # Errors
/// Rejects provider routes, foreign instance/account, unsupported selected Open or invalid IDs.
pub fn selected_start_request(
    resolved: &ResolvedExecutionModel,
    instance: bcode_harness_models::HarnessInstance,
    open: bcode_harness_models::HarnessOpenRequest,
    parent_session_id: bcode_session_models::SessionId,
) -> Result<bcode_ipc::harness::ManagedSelectedStartRequest, WorkflowStoreError> {
    let ExecutionRoute::ManagedHarness {
        instance_id,
        plugin_id,
    } = &resolved.target.route
    else {
        return Err(invalid(
            "provider execution cannot use managed selected Start",
        ));
    };
    if instance.instance_id != *instance_id
        || instance.plugin_id != *plugin_id
        || resolved.target.account_reference.as_deref() != Some(instance.auth.profile.as_str())
    {
        return Err(invalid("selected managed Start instance/account changed"));
    }
    let request = bcode_ipc::harness::ManagedSelectedStartRequest {
        selection: bcode_model::execution::ExecutionModelSelection {
            model_id: resolved.target.model.model_id.clone(),
            route: Some(resolved.target.route.clone()),
            account_reference: resolved.target.account_reference.clone(),
        },
        instance,
        open,
        parent_session_id: Some(parent_session_id),
    };
    request
        .validate()
        .map_err(|_| invalid("invalid workflow selected Start"))?;
    Ok(request)
}

/// Validate selected Open evidence before the owner associates it with an assignment.
///
/// A prepared target remains prepared; the returned target does not synthesize a native session
/// or turn ID. The original resolved request must remain separately retained in dispatch intent.
///
/// # Errors
/// Rejects changed route/account/model membership, stale Open correlation or invalid native evidence.
pub fn accepted_start_target(
    resolved: &ResolvedExecutionModel,
    request: &bcode_ipc::harness::ManagedSelectedStartRequest,
    response: &bcode_ipc::harness::ManagedSelectedStartResponse,
) -> Result<bcode_harness_models::HarnessSessionTarget, WorkflowStoreError> {
    request
        .validate()
        .map_err(|_| invalid("invalid workflow selected Start"))?;
    if request.selection.model_id != resolved.target.model.model_id
        || request.selection.route.as_ref() != Some(&resolved.target.route)
        || request.selection.account_reference != resolved.target.account_reference
    {
        return Err(invalid("workflow selected Start request changed"));
    }
    let checked = bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
        std::slice::from_ref(&resolved.target),
        resolved.requested.clone(),
    )
    .map_err(|_| invalid("invalid retained workflow selection"))?;
    let acknowledged = bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
        std::slice::from_ref(&response.selection.target),
        request.selection.clone(),
    )
    .map_err(|_| invalid("invalid selected Start acknowledgment"))?;
    if response.selection.requested != request.selection
        || response.selection.target.route != checked.target.route
        || response.selection.target.account_reference != checked.target.account_reference
        || response.selection.target.model.model_id != checked.target.model.model_id
        || response.selection.target.native_model_id != checked.target.native_model_id
        || response.selection.eligibility != acknowledged.eligibility
    {
        return Err(invalid("workflow selected Start resolution changed"));
    }
    let native = bcode_harness_models::HarnessModelsRequest {
        schema_version: bcode_harness_models::HARNESS_SCHEMA_VERSION,
        request_id: request.open.open_id.clone(),
        instance: request.instance.clone(),
        kind: bcode_harness_models::HarnessModelsRequestKind::OpenSelected {
            open: Box::new(request.open.clone()),
            selection: bcode_harness_models::HarnessModelSelection {
                model_id: resolved.target.native_model_id.clone(),
            },
        },
    };
    response
        .response
        .validate_for(&native)
        .map_err(|_| invalid("workflow selected Start evidence is not exact"))?;
    let bcode_harness_models::HarnessModelsResponseKind::Selected { target, evidence } =
        &response.response.kind
    else {
        return Err(invalid("workflow selected Start has no selected target"));
    };
    if response.selection.target.confirmed_model_id != evidence.effective_model_id {
        return Err(invalid(
            "workflow selected Start effective-model evidence changed",
        ));
    }
    Ok(target.clone())
}

/// Validate the selected prompt owner's supported dispatch contract.
///
/// Selected native execution retains exact Open/assignment receipts; provider execution uses
/// the ordinary authenticated provider path. Discovery and admission remain dispatch-time checks.
///
/// # Errors
/// Rejects malformed configurations and selections without an explicit execution route.
pub fn require_dispatch_support(
    configuration: &WorkflowPromptConfiguration,
) -> Result<(), WorkflowStoreError> {
    configuration
        .validate()
        .map_err(|error| invalid(&error.to_string()))?;
    if let Some(selection) = &configuration.execution_model_selection {
        require_explicit_route(selection)?;
    }
    Ok(())
}

/// Apply the selected-dispatch compatibility gate to newly executable graph edits.
///
/// # Errors
/// Rejects malformed prompt contracts and selected prompts without an explicit route.
pub fn require_edit_dispatch_support(
    batch: &bcode_workflow::WorkflowRunGraphEditBatch,
) -> Result<(), WorkflowStoreError> {
    for edit in &batch.edits {
        let (bcode_workflow::WorkflowRunGraphEdit::AddNode { node, .. }
        | bcode_workflow::WorkflowRunGraphEdit::ReplaceNode { node, .. }) = edit
        else {
            continue;
        };
        if node.kind == bcode_workflow::NodeKind::Agent {
            let configuration: WorkflowPromptConfiguration =
                serde_json::from_value(node.configuration.clone())?;
            configuration
                .validate()
                .map_err(|_| invalid("invalid workflow prompt configuration"))?;
            require_dispatch_support(&configuration)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::execution::{
        ExecutionBillingRoute, ExecutionModelProvenance, ExecutionModelReadiness,
        ExecutionModelSelection,
    };

    fn entry() -> ExecutionModel {
        ExecutionModel {
            model: bcode_model::ModelInfo {
                model_id: "observed-model".into(),
                display_name: "Observed".into(),
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
                visibility: bcode_model::ModelVisibility::Visible,
            },
            route: ExecutionRoute::ManagedHarness {
                instance_id: "configured-instance".into(),
                plugin_id: "adapter".into(),
            },
            account_reference: Some("native-account".into()),
            billing_route: ExecutionBillingRoute::Unknown,
            native_model_id: "native-selector".into(),
            confirmed_model_id: None,
            provenance: ExecutionModelProvenance::HarnessDiscovery,
            readiness: ExecutionModelReadiness::Ready,
        }
    }

    #[test]
    fn selected_open_keeps_prepared_evidence_and_rejects_changed_correlation() {
        use bcode_harness_models::*;
        let entry = entry();
        let configuration = WorkflowPromptConfiguration::preserve_input("build", "task")
            .with_execution_model_selection(ExecutionModelSelection {
                model_id: entry.model.model_id.clone(),
                route: Some(entry.route.clone()),
                account_reference: entry.account_reference.clone(),
            });
        let resolved = resolve(&configuration, &[entry]).unwrap().unwrap();
        let session = bcode_session_models::SessionId::new();
        let instance = HarnessInstance {
            instance_id: "configured-instance".into(),
            plugin_id: "adapter".into(),
            executable: "/configured/native".into(),
            config_namespace: "configured".into(),
            auth: HarnessAuthBinding {
                provider_id: "native-auth".into(),
                profile: "native-account".into(),
                credential_namespace: "native-namespace".into(),
                custody: HarnessAuthCustody::NativeManaged,
            },
        };
        let request = selected_start_request(
            &resolved,
            instance,
            HarnessOpenRequest {
                open_id: "dispatch-open".into(),
                session_id: session.to_string(),
                generation: 1,
                working_directory: "/workspace".into(),
                resume: None,
                parent_assistance: None,
            },
            bcode_session_models::SessionId::new(),
        )
        .unwrap();
        let target = HarnessSessionTarget::Prepared(HarnessPreparedSession {
            open_id: request.open.open_id.clone(),
            session_id: session.to_string(),
            instance_id: request.instance.instance_id.clone(),
            generation: 1,
            native_version: "observed-version".into(),
            requested_native_session_id: None,
        });
        let selected = bcode_model_catalog::ModelCatalogResolver::resolve_execution_selection(
            std::slice::from_ref(&resolved.target),
            request.selection.clone(),
        )
        .unwrap();
        let mut response = bcode_ipc::harness::ManagedSelectedStartResponse {
            selection: selected,
            response: HarnessModelsResponse {
                schema_version: HARNESS_SCHEMA_VERSION,
                request_id: request.open.open_id.clone(),
                instance_id: request.instance.instance_id.clone(),
                kind: HarnessModelsResponseKind::Selected {
                    target: target.clone(),
                    evidence: HarnessModelEvidence {
                        requested_model_id: resolved.target.native_model_id.clone(),
                        effective_model_id: None,
                    },
                },
            },
        };
        assert_eq!(
            accepted_start_target(&resolved, &request, &response).unwrap(),
            target
        );
        response.response.request_id = "different-open".into();
        assert!(accepted_start_target(&resolved, &request, &response).is_err());
        response.response.request_id = request.open.open_id.clone();
        response.selection.target.account_reference = Some("foreign-account".into());
        assert!(accepted_start_target(&resolved, &request, &response).is_err());
    }

    #[test]
    fn exact_resolution_never_substitutes_route_account_or_context() {
        let entry = entry();
        let selection = ExecutionModelSelection {
            model_id: entry.model.model_id.clone(),
            route: Some(entry.route.clone()),
            account_reference: entry.account_reference.clone(),
        };
        let configuration = WorkflowPromptConfiguration::preserve_input("build", "perform task")
            .with_execution_model_selection(selection.clone());
        let resolved = resolve(&configuration, std::slice::from_ref(&entry))
            .unwrap()
            .unwrap();
        assert_eq!(resolved.target, entry);
        assert_eq!(resolved.requested, selection);
        assert!(resolve(&configuration, &[entry.clone(), entry.clone()]).is_err());
        let mut wrong = configuration.clone();
        wrong
            .execution_model_selection
            .as_mut()
            .unwrap()
            .account_reference = Some("foreign".into());
        assert!(resolve(&wrong, std::slice::from_ref(&entry)).is_err());
        for target in [
            PromptContextTarget::FixedGenerationFork,
            PromptContextTarget::SharedParentSequential,
        ] {
            assert!(
                resolve(
                    &configuration.clone().with_execution_target(target),
                    std::slice::from_ref(&entry)
                )
                .is_err()
            );
        }
        assert!(require_dispatch_support(&configuration).is_ok());
        let mut missing_route = configuration;
        missing_route
            .execution_model_selection
            .as_mut()
            .unwrap()
            .route = None;
        assert!(require_dispatch_support(&missing_route).is_err());
        assert!(
            resolve(
                &WorkflowPromptConfiguration::preserve_input("build", "legacy"),
                &[]
            )
            .unwrap()
            .is_none()
        );
    }
}
