//! Client-side routing to review-source providers (`bcode.review_source/v1`).
//!
//! Plugin services cannot call each other: the host's nested-service bridge serves host
//! interfaces only. Like publisher routing, provider routing therefore happens in the client.
//! Before resolving a workspace, the client discovers the enabled providers, asks the matching
//! provider to resolve stacks and fetch pull requests for every pull-request, stack-layer, and
//! stack source, and passes the answers to `review.sources.resolve` as
//! [`ReviewSourceProviderResolution`] values. Without a matching provider (for example when the
//! GitHub plugin is disabled) a source gets no answer and resolves to a `provider_required`
//! diagnostic; every other source keeps working.

use bcode_code_review_models::{
    OP_REVIEW_SOURCE_MANIFEST, OP_REVIEW_SOURCE_PULL_FETCH, OP_REVIEW_SOURCE_PULLS_LIST,
    OP_REVIEW_SOURCE_STACK_RESOLVE, REVIEW_SOURCE_ERROR_FAILED, REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
    REVIEW_SOURCE_INTERFACE_ID, ResolveReviewSourcesRequest, ReviewPullFetchResult, ReviewSource,
    ReviewSourceKind, ReviewSourceProviderManifest, ReviewSourceProviderOutcome,
    ReviewSourceProviderResolution, ReviewSourcePullFetchRequest, ReviewSourcePullsListRequest,
    ReviewSourcePullsListResponse, ReviewSourceStackResolveRequest,
    ReviewSourceStackResolveResponse, ReviewStack,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::Future;
use std::path::Path;

/// A failed provider call: a service error code and message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderCallError {
    /// Stable error code.
    pub code: String,
    /// Human-readable message.
    pub message: String,
}

impl ProviderCallError {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_string(),
            message: message.into(),
        }
    }
}

/// Access to plugin services from a client: the daemon client in production, fakes in tests.
pub trait ProviderTransport {
    /// Ids of enabled plugins that serve `interface_id`.
    fn plugins_serving(
        &self,
        interface_id: &str,
    ) -> impl Future<Output = Result<Vec<String>, ProviderCallError>> + Send;

    /// Invoke `operation` of `interface_id` on `plugin_id` and return the response payload.
    fn invoke(
        &self,
        plugin_id: &str,
        interface_id: &str,
        operation: &str,
        payload: Vec<u8>,
    ) -> impl Future<Output = Result<Vec<u8>, ProviderCallError>> + Send;
}

/// One enabled review-source provider.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderRoute {
    /// Plugin serving the provider.
    pub plugin_id: String,
    /// The provider's manifest.
    pub manifest: ReviewSourceProviderManifest,
}

/// Discover the enabled review-source providers. A provider whose manifest cannot be read is
/// skipped (its sources then report `provider_required`).
///
/// # Errors
///
/// * Returns the transport error when the plugin services cannot be listed.
pub async fn discover_providers<T: ProviderTransport + Sync>(
    transport: &T,
) -> Result<Vec<ProviderRoute>, ProviderCallError> {
    let mut routes = Vec::new();
    for plugin_id in transport
        .plugins_serving(REVIEW_SOURCE_INTERFACE_ID)
        .await?
    {
        if let Ok(manifest) = call::<_, _, ReviewSourceProviderManifest>(
            transport,
            &plugin_id,
            OP_REVIEW_SOURCE_MANIFEST,
            &serde_json::json!({}),
        )
        .await
        {
            routes.push(ProviderRoute {
                plugin_id,
                manifest,
            });
        }
    }
    Ok(routes)
}

/// Ask the matching providers about every included provider-backed source.
///
/// * `PullRequest`: fetch the pull request.
/// * `StackLayer` / `Stack`: resolve the stack, then fetch every layer from the trunk up to the
///   reviewed layer (or the stack's top), each with its own base branch.
///
/// Sources whose provider is not among `routes` get no entry. Provider failures become
/// [`ReviewSourceProviderOutcome::Failed`] entries. Stacks are resolved with `max_layers` (see
/// [`ReviewSourceStackResolveRequest::max_layers`]).
pub async fn gather_provider_resolutions<T: ProviderTransport + Sync>(
    transport: &T,
    routes: &[ProviderRoute],
    repo_path: &Path,
    sources: &[ReviewSource],
    max_layers: Option<usize>,
) -> Vec<ReviewSourceProviderResolution> {
    let mut resolutions = Vec::new();
    for source in sources.iter().filter(|source| source.included) {
        let (provider, repository, number, stacked) = match &source.kind {
            ReviewSourceKind::PullRequest {
                provider,
                repository,
                number,
            } => (provider, repository, *number, false),
            ReviewSourceKind::StackLayer {
                provider,
                repository,
                number,
            } => (provider, repository, *number, true),
            ReviewSourceKind::Stack {
                provider,
                repository,
                top,
            } => (provider, repository, *top, true),
            _ => continue,
        };
        let Some(route) = routes.iter().find(|route| route.manifest.id == *provider) else {
            continue;
        };
        let target = Target {
            transport,
            plugin_id: &route.plugin_id,
            repo_path,
            repository,
            max_layers,
        };
        let outcome = if stacked {
            target.stacked(number).await
        } else {
            target
                .fetch(number, None)
                .await
                .map(|fetch| (None, vec![fetch]))
        };
        resolutions.push(ReviewSourceProviderResolution {
            source_id: source.id.clone(),
            outcome: match outcome {
                Ok((stack, fetched)) => ReviewSourceProviderOutcome::Resolved { stack, fetched },
                Err(error) => ReviewSourceProviderOutcome::Failed {
                    code: error.code,
                    message: error.message,
                },
            },
        });
    }
    resolutions
}

/// Build the `review.sources.resolve` request for a workspace, asking the enabled providers
/// about its provider-backed sources first (stacks with `max_layers`).
///
/// # Errors
///
/// * Returns the transport error when the plugin services cannot be listed.
pub async fn resolve_request_with_providers<T: ProviderTransport + Sync>(
    transport: &T,
    repo_path: &Path,
    sources: Vec<ReviewSource>,
    max_layers: Option<usize>,
) -> Result<ResolveReviewSourcesRequest, ProviderCallError> {
    let needs_provider = sources.iter().any(|source| {
        source.included && bcode_code_review_models::source_kind_requires_provider(&source.kind)
    });
    let provider_resolutions = if needs_provider {
        let routes = discover_providers(transport).await?;
        gather_provider_resolutions(transport, &routes, repo_path, &sources, max_layers).await
    } else {
        Vec::new()
    };
    Ok(ResolveReviewSourcesRequest {
        repo_path: repo_path.to_path_buf(),
        sources,
        provider_resolutions,
    })
}

/// List the open pull requests of `repository` (detected from the remotes when `None`) through
/// `route`: review requested from me first, then most recently updated.
///
/// # Errors
///
/// * The provider's error (for example `unauthenticated` or `unsupported_repository`).
pub async fn list_pulls<T: ProviderTransport + Sync>(
    transport: &T,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
) -> Result<ReviewSourcePullsListResponse, ProviderCallError> {
    call(
        transport,
        &route.plugin_id,
        OP_REVIEW_SOURCE_PULLS_LIST,
        &ReviewSourcePullsListRequest {
            repo_path: repo_path.to_path_buf(),
            repository,
            query: None,
        },
    )
    .await
}

/// Look up the open pull requests `query` names through `route`: a number (`123`, `#123`) or URL,
/// or an exact head branch name (see [`ReviewSourcePullsListRequest::query`]).
///
/// # Errors
///
/// * The provider's error (for example `unauthenticated` or `unsupported_repository`).
pub async fn lookup_pulls<T: ProviderTransport + Sync>(
    transport: &T,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    query: &str,
) -> Result<ReviewSourcePullsListResponse, ProviderCallError> {
    call(
        transport,
        &route.plugin_id,
        OP_REVIEW_SOURCE_PULLS_LIST,
        &ReviewSourcePullsListRequest {
            repo_path: repo_path.to_path_buf(),
            repository,
            query: Some(query.to_owned()),
        },
    )
    .await
}

/// Resolve the stack containing pull request `number` through `route`, cut at `max_layers` when
/// given.
///
/// # Errors
///
/// * The provider's error (for example `pull_not_found`).
pub async fn resolve_stack<T: ProviderTransport + Sync>(
    transport: &T,
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    number: u64,
    max_layers: Option<usize>,
) -> Result<ReviewSourceStackResolveResponse, ProviderCallError> {
    call(
        transport,
        &route.plugin_id,
        OP_REVIEW_SOURCE_STACK_RESOLVE,
        &ReviewSourceStackResolveRequest {
            repo_path: repo_path.to_path_buf(),
            repository,
            number,
            max_layers,
        },
    )
    .await
}

/// One provider and repository being asked about a source.
struct Target<'a, T> {
    transport: &'a T,
    plugin_id: &'a str,
    repo_path: &'a Path,
    repository: &'a str,
    max_layers: Option<usize>,
}

impl<T: ProviderTransport + Sync> Target<'_, T> {
    async fn fetch(
        &self,
        number: u64,
        base_branch: Option<String>,
    ) -> Result<ReviewPullFetchResult, ProviderCallError> {
        call(
            self.transport,
            self.plugin_id,
            OP_REVIEW_SOURCE_PULL_FETCH,
            &ReviewSourcePullFetchRequest {
                repo_path: self.repo_path.to_path_buf(),
                repository: Some(self.repository.to_string()),
                number,
                base_branch,
            },
        )
        .await
    }

    async fn stacked(
        &self,
        number: u64,
    ) -> Result<(Option<ReviewStack>, Vec<ReviewPullFetchResult>), ProviderCallError> {
        let response: ReviewSourceStackResolveResponse = call(
            self.transport,
            self.plugin_id,
            OP_REVIEW_SOURCE_STACK_RESOLVE,
            &ReviewSourceStackResolveRequest {
                repo_path: self.repo_path.to_path_buf(),
                repository: Some(self.repository.to_string()),
                number,
                max_layers: self.max_layers,
            },
        )
        .await?;
        let layers = response.stack.layers_through(number).ok_or_else(|| {
            ProviderCallError::new(
                REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
                format!("pull request #{number} is not a layer of its stack"),
            )
        })?;
        let mut fetched = Vec::with_capacity(layers.len());
        for layer in layers {
            fetched.push(
                self.fetch(layer.pull.number, Some(layer.pull.base_branch.clone()))
                    .await?,
            );
        }
        Ok((Some(response.stack), fetched))
    }
}

async fn call<T, Req, Resp>(
    transport: &T,
    plugin_id: &str,
    operation: &str,
    request: &Req,
) -> Result<Resp, ProviderCallError>
where
    T: ProviderTransport + Sync,
    Req: Serialize + Sync,
    Resp: DeserializeOwned,
{
    let payload = serde_json::to_vec(request)
        .map_err(|error| ProviderCallError::new(REVIEW_SOURCE_ERROR_FAILED, error.to_string()))?;
    let response = transport
        .invoke(plugin_id, REVIEW_SOURCE_INTERFACE_ID, operation, payload)
        .await?;
    serde_json::from_slice(&response).map_err(|error| {
        ProviderCallError::new(
            REVIEW_SOURCE_ERROR_FAILED,
            format!("unexpected {operation} response from {plugin_id}: {error}"),
        )
    })
}

#[cfg(test)]
mod tests;
