//! Centralized model request-target resolution.
//!
//! Every production model invocation resolves its effective model identity and normalized provider
//! API surface here before constructing a provider request. Request-purpose behavior remains in the
//! owning caller; this module owns only the application-level target shared by those requests.

use super::*;

/// Unresolved inputs needed to select one provider model request target.
pub struct ModelRequestTargetInput<'a> {
    pub provider_plugin_id: Option<&'a str>,
    pub selected_model_id: Option<&'a str>,
    pub provider_context: &'a bcode_model::ProviderRequestContext,
}

/// Complete application-level target for one provider model request.
#[derive(Debug, Clone)]
pub struct ResolvedModelRequestTarget {
    pub provider_plugin_id: Option<String>,
    pub requested_model_id: Option<String>,
    pub model_id: String,
    pub provider_context: bcode_model::ProviderRequestContext,
    pub catalog_provider_id: Option<String>,
    pub catalog_identity: Option<bcode_model_catalog::ModelCatalogIdentity>,
}

/// Failure to resolve a usable model request target.
#[derive(Debug, Error)]
pub enum ModelRequestTargetError {
    /// The provider model view could not be resolved.
    #[error("failed to resolve provider models: {0}")]
    ModelList(String),
    /// The resolved provider model view contained no usable model.
    #[error("model provider has no usable models")]
    NoUsableModel,
}

/// Resolve one request's effective model, normalized API surface, and request-time auth context.
///
/// Model identity and API surface come from the same resolved provider-model snapshot. When live
/// discovery has not supplied a surface, the application catalog is consulted; an existing
/// context surface is retained only when neither source knows the model's surface.
pub async fn resolve_model_request_target(
    state: &ServerState,
    input: ModelRequestTargetInput<'_>,
) -> Result<ResolvedModelRequestTarget, ModelRequestTargetError> {
    let models = resolved_provider_models(
        state,
        input.provider_plugin_id.map(ToOwned::to_owned),
        bcode_model::ModelListRequest {
            provider_context: input.provider_context.clone(),
            selected_model_id: input.selected_model_id.map(ToOwned::to_owned),
        },
    )
    .await
    .map_err(ModelRequestTargetError::ModelList)?;
    let model = models
        .models
        .iter()
        .find(|model| model.is_default)
        .or_else(|| models.models.first())
        .ok_or(ModelRequestTargetError::NoUsableModel)?;
    let model_id = model.model_id.clone();
    let api_surface = resolve_request_api_surface(
        &state.model_catalog,
        &models.catalog.policy,
        &model_id,
        model.api_surface,
        input.provider_context.api_surface,
    )
    .await;
    let mut provider_context = input.provider_context.clone();
    provider_context.api_surface = api_surface;
    select_host_auth_pool_candidate(state, input.provider_plugin_id, &mut provider_context).await;
    let catalog_provider_id = catalog_provider_id_for_policy(&models.catalog.policy);
    let catalog_identity = if let Some(provider_id) = catalog_provider_id.as_deref() {
        state
            .model_catalog
            .model_identity(provider_id, &model_id)
            .await
    } else {
        None
    };
    Ok(ResolvedModelRequestTarget {
        provider_plugin_id: input.provider_plugin_id.map(ToOwned::to_owned),
        requested_model_id: input.selected_model_id.map(ToOwned::to_owned),
        model_id,
        provider_context,
        catalog_provider_id,
        catalog_identity,
    })
}

async fn resolve_request_api_surface(
    catalog: &bcode_model_catalog::ModelCatalogResolver,
    policy: &bcode_model::ModelCatalogPolicy,
    model_id: &str,
    resolved_surface: Option<bcode_model::ModelApiSurface>,
    existing_surface: Option<bcode_model::ModelApiSurface>,
) -> Option<bcode_model::ModelApiSurface> {
    if resolved_surface.is_some() {
        return resolved_surface;
    }
    let Some(catalog_provider_id) = catalog_provider_id_for_policy(policy) else {
        return existing_surface;
    };
    catalog
        .model_api_surface(&catalog_provider_id, model_id)
        .await
        .or(existing_surface)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn catalog_surface_overrides_stale_context_and_unknown_models_keep_context() {
        let catalog = bcode_model_catalog::ModelCatalogResolver::embedded();
        let policy = bcode_model::ModelCatalogPolicy::ExpandAll {
            provider_id: "bedrock".to_string(),
        };

        let known = resolve_request_api_surface(
            &catalog,
            &policy,
            "openai.gpt-5.6-sol",
            None,
            Some(bcode_model::ModelApiSurface::Messages),
        )
        .await;
        let unknown = resolve_request_api_surface(
            &catalog,
            &policy,
            "openai.unknown-future-model",
            None,
            Some(bcode_model::ModelApiSurface::Messages),
        )
        .await;

        assert_eq!(known, Some(bcode_model::ModelApiSurface::Responses));
        assert_eq!(unknown, Some(bcode_model::ModelApiSurface::Messages));
    }
}
