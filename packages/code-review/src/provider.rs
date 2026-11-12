//! Routing to review-source providers and discovery contributors.
//!
//! A [`ProviderRegistry`] holds the providers and contributors a review can ask: the built-in
//! ones the application links and calls in-process (GitHub, git history, Graphite), plus any a
//! running Bcode offers through its plugins ([`plugin_manifests`] finds them; the application
//! wraps them as [`JsonReviewSource`] / [`JsonDiscovery`]). Built-in ones win over plugin ones with
//! the same id, so nothing built in depends on Bcode.
//!
//! Every call here blocks (providers run git and talk to the network); callers run them on
//! blocking threads. Before resolving a workspace, the client asks the matching provider to
//! resolve stacks and fetch pull requests for every pull-request, stack-layer, and stack source,
//! and passes the answers to `review.sources.resolve` as [`ReviewSourceProviderResolution`]
//! values. Without a matching provider a source gets no answer and resolves to a
//! `provider_required` diagnostic; every other source keeps working.

pub use bcode_code_review_provider::{
    DiscoveryContributor, JsonCall, JsonDiscovery, JsonReviewSource, ProviderCallError,
    ProviderResult, ReviewSourceProvider,
};

use bcode_code_review_models::{
    OP_REVIEW_DISCOVERY_MANIFEST, OP_REVIEW_SOURCE_MANIFEST, REVIEW_DISCOVERY_INTERFACE_ID,
    REVIEW_SOURCE_ERROR_FAILED, REVIEW_SOURCE_ERROR_PULL_NOT_FOUND, REVIEW_SOURCE_INTERFACE_ID,
    ResolveReviewSourcesRequest, ReviewDiscoveryManifest, ReviewPullFetchResult,
    ReviewPullFetchTarget, ReviewSource, ReviewSourceKind, ReviewSourceProviderManifest,
    ReviewSourceProviderOutcome, ReviewSourceProviderResolution, ReviewSourcePullFetchRequest,
    ReviewSourcePullsFetchRequest, ReviewSourcePullsListRequest, ReviewSourcePullsListResponse,
    ReviewSourceStackResolveRequest, ReviewSourceStackResolveResponse, ReviewStack,
};
use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::path::Path;
use std::sync::Arc;

/// Access to a running Bcode's plugin services, for finding plugin-backed providers.
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

/// One review-source provider of a registry.
#[derive(Clone)]
pub struct ProviderRoute {
    /// The provider's manifest.
    pub manifest: ReviewSourceProviderManifest,
    /// The provider.
    pub provider: Arc<dyn ReviewSourceProvider>,
}

impl std::fmt::Debug for ProviderRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ProviderRoute")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

impl PartialEq for ProviderRoute {
    fn eq(&self, other: &Self) -> bool {
        self.manifest == other.manifest && Arc::ptr_eq(&self.provider, &other.provider)
    }
}

impl Eq for ProviderRoute {}

/// One discovery contributor of a registry.
#[derive(Clone)]
pub struct ContributorRoute {
    /// The contributor's manifest.
    pub manifest: ReviewDiscoveryManifest,
    /// The contributor.
    pub contributor: Arc<dyn DiscoveryContributor>,
}

impl std::fmt::Debug for ContributorRoute {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContributorRoute")
            .field("manifest", &self.manifest)
            .finish_non_exhaustive()
    }
}

/// How long a resolved stack is reused without asking its provider again.
pub const STACK_REUSE: std::time::Duration = std::time::Duration::from_mins(10);

/// Stacks resolved recently, by provider and repository, so a review opened right after the
/// stack was resolved (from the wizard, the home screen, or another open) does not resolve it
/// again. Shared by every clone of a registry.
#[derive(Debug, Default)]
struct KnownStacks(std::sync::Mutex<BTreeMap<StackScope, Vec<KnownStack>>>);

/// The provider and repository stacks are remembered under.
type StackScope = (String, String);

/// A stack and when it was resolved.
#[derive(Debug)]
struct KnownStack {
    resolved: std::time::Instant,
    stack: ReviewStack,
}

impl KnownStacks {
    fn remember(&self, provider: &str, repository: &str, stack: &ReviewStack) {
        let numbers: BTreeSet<u64> = stack.layers.iter().map(|layer| layer.pull.number).collect();
        let entry = KnownStack {
            resolved: std::time::Instant::now(),
            stack: stack.clone(),
        };
        let mut known = self.lock();
        let stacks = known
            .entry((provider.to_owned(), repository.to_owned()))
            .or_default();
        // A newer resolution of any of its layers replaces an older stack.
        stacks.retain(|old| {
            !old.stack
                .layers
                .iter()
                .any(|layer| numbers.contains(&layer.pull.number))
        });
        stacks.push(entry);
        drop(known);
    }

    fn containing(&self, provider: &str, repository: &str, number: u64) -> Option<ReviewStack> {
        let mut known = self.lock();
        let stacks = known.get_mut(&(provider.to_owned(), repository.to_owned()))?;
        stacks.retain(|known| known.resolved.elapsed() < STACK_REUSE);
        let found = stacks
            .iter()
            .rev()
            .find(|known| known.stack.layer(number).is_some())
            .map(|known| known.stack.clone());
        drop(known);
        found
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<StackScope, Vec<KnownStack>>> {
        self.0
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// The providers and contributors a review asks, in preference order, and the stacks they
/// resolved recently.
#[derive(Debug, Clone, Default)]
pub struct ProviderRegistry {
    sources: Vec<ProviderRoute>,
    contributors: Vec<ContributorRoute>,
    known_stacks: Arc<KnownStacks>,
}

impl ProviderRegistry {
    /// An empty registry.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add `provider` unless a provider with its id is already present. Returns whether it was
    /// added.
    pub fn add_source(&mut self, provider: Arc<dyn ReviewSourceProvider>) -> bool {
        let manifest = provider.manifest();
        if self.source(&manifest.id).is_some() {
            return false;
        }
        self.sources.push(ProviderRoute { manifest, provider });
        true
    }

    /// Add `contributor` unless a contributor with its id is already present. Returns whether it
    /// was added.
    pub fn add_contributor(&mut self, contributor: Arc<dyn DiscoveryContributor>) -> bool {
        let manifest = contributor.manifest();
        if self
            .contributors
            .iter()
            .any(|route| route.manifest.id == manifest.id)
        {
            return false;
        }
        self.contributors.push(ContributorRoute {
            manifest,
            contributor,
        });
        true
    }

    /// [`Self::add_source`], builder style.
    #[must_use]
    pub fn with_source(mut self, provider: Arc<dyn ReviewSourceProvider>) -> Self {
        self.add_source(provider);
        self
    }

    /// [`Self::add_contributor`], builder style.
    #[must_use]
    pub fn with_contributor(mut self, contributor: Arc<dyn DiscoveryContributor>) -> Self {
        self.add_contributor(contributor);
        self
    }

    /// Review-source providers, in preference order.
    #[must_use]
    pub fn sources(&self) -> &[ProviderRoute] {
        &self.sources
    }

    /// Discovery contributors, in preference order.
    #[must_use]
    pub fn contributors(&self) -> &[ContributorRoute] {
        &self.contributors
    }

    /// The provider with id `id`.
    #[must_use]
    pub fn source(&self, id: &str) -> Option<&ProviderRoute> {
        self.sources.iter().find(|route| route.manifest.id == id)
    }

    /// Remember `stack`, resolved by provider `provider` in `repository`, for [`STACK_REUSE`].
    pub fn remember_stack(&self, provider: &str, repository: &str, stack: &ReviewStack) {
        self.known_stacks.remember(provider, repository, stack);
    }

    /// A stack resolved within [`STACK_REUSE`] that has pull request `number` as a layer.
    #[must_use]
    pub fn known_stack(
        &self,
        provider: &str,
        repository: &str,
        number: u64,
    ) -> Option<ReviewStack> {
        self.known_stacks.containing(provider, repository, number)
    }
}

/// The providers and contributors a running Bcode's plugins offer, by plugin id.
#[derive(Debug, Clone, Default)]
pub struct PluginManifests {
    /// Review-source providers: plugin id and manifest.
    pub sources: Vec<(String, ReviewSourceProviderManifest)>,
    /// Discovery contributors: plugin id and manifest.
    pub contributors: Vec<(String, ReviewDiscoveryManifest)>,
}

/// Manifests of the review-source providers and discovery contributors a running Bcode's plugins
/// offer. A plugin whose manifest cannot be read is skipped.
///
/// # Errors
///
/// * The transport's error when the plugin services cannot be listed.
pub async fn plugin_manifests<T: ProviderTransport + Sync>(
    transport: &T,
) -> Result<PluginManifests, ProviderCallError> {
    let mut sources = Vec::new();
    for plugin_id in transport
        .plugins_serving(REVIEW_SOURCE_INTERFACE_ID)
        .await?
    {
        if let Ok(payload) = transport
            .invoke(
                &plugin_id,
                REVIEW_SOURCE_INTERFACE_ID,
                OP_REVIEW_SOURCE_MANIFEST,
                b"{}".to_vec(),
            )
            .await
            && let Ok(manifest) = serde_json::from_slice(&payload)
        {
            sources.push((plugin_id, manifest));
        }
    }
    let mut contributors = Vec::new();
    for plugin_id in transport
        .plugins_serving(REVIEW_DISCOVERY_INTERFACE_ID)
        .await?
    {
        if let Ok(payload) = transport
            .invoke(
                &plugin_id,
                REVIEW_DISCOVERY_INTERFACE_ID,
                OP_REVIEW_DISCOVERY_MANIFEST,
                b"{}".to_vec(),
            )
            .await
            && let Ok(manifest) = serde_json::from_slice(&payload)
        {
            contributors.push((plugin_id, manifest));
        }
    }
    Ok(PluginManifests {
        sources,
        contributors,
    })
}

/// Ask the matching providers about every included provider-backed source.
///
/// * `PullRequest`: fetch the pull request.
/// * `StackLayer` / `Stack`: resolve the stack (with discovery, once per stack), then fetch every
///   layer from the trunk up to the reviewed layer (or the stack's top), each with its own base
///   branch, every layer of every source in one batched fetch where the provider can.
///
/// Sources whose provider is not in `registry` get no entry. Provider failures become
/// [`ReviewSourceProviderOutcome::Failed`] entries. Stacks are resolved with `max_layers` (see
/// [`ReviewSourceStackResolveRequest::max_layers`]).
#[must_use]
pub fn gather_provider_resolutions(
    registry: &ProviderRegistry,
    repo_path: &Path,
    sources: &[ReviewSource],
    max_layers: Option<usize>,
) -> Vec<ReviewSourceProviderResolution> {
    // 1. Resolve each source's stack (once per stack) and note the pull requests it needs.
    let mut targets: BTreeMap<(String, String), (Target<'_>, Asked)> = BTreeMap::new();
    let mut plans = Vec::new();
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
        let Some(route) = registry.source(provider) else {
            continue;
        };
        let key = (route.manifest.id.clone(), repository.clone());
        let (target, asked) = targets.entry(key.clone()).or_insert_with(|| {
            (
                Target {
                    registry,
                    route,
                    repo_path,
                    repository,
                    max_layers,
                    batch_fetch: route.manifest.capabilities.batched.pulls_fetch,
                },
                Asked::default(),
            )
        });
        let needs = if stacked {
            target.stack(asked, number)
        } else {
            Ok((None, vec![(number, None)]))
        };
        plans.push((source.id.clone(), key, needs));
    }
    // 2. Fetch every needed pull request: once each, batched where the provider can.
    for (key, (target, asked)) in &mut targets {
        let needed: BTreeSet<FetchKey> = plans
            .iter()
            .filter(|(_, plan_key, _)| plan_key == key)
            .filter_map(|(_, _, needs)| needs.as_ref().ok())
            .flat_map(|(_, keys)| keys.iter().cloned())
            .collect();
        target.fetch_all(asked, &needed);
    }
    // 3. Answer each source from what was asked.
    plans
        .into_iter()
        .map(|(source_id, key, needs)| {
            let asked = &targets[&key].1;
            let outcome = needs.and_then(|(stack, keys)| {
                let fetched = keys
                    .iter()
                    .map(|fetch_key| {
                        asked.fetched.get(fetch_key).cloned().unwrap_or_else(|| {
                            Err(ProviderCallError::new(
                                REVIEW_SOURCE_ERROR_FAILED,
                                format!("pull request #{} was not fetched", fetch_key.0),
                            ))
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok((stack, fetched))
            });
            ReviewSourceProviderResolution {
                source_id,
                outcome: match outcome {
                    Ok((stack, fetched)) => {
                        ReviewSourceProviderOutcome::Resolved { stack, fetched }
                    }
                    Err(error) => ReviewSourceProviderOutcome::Failed {
                        code: error.code,
                        message: error.message,
                    },
                },
            }
        })
        .collect()
}

/// Build the `review.sources.resolve` request for a workspace, asking `registry`'s providers
/// about its provider-backed sources first (stacks with `max_layers`).
#[must_use]
pub fn resolve_request_with_providers(
    registry: &ProviderRegistry,
    repo_path: &Path,
    sources: Vec<ReviewSource>,
    max_layers: Option<usize>,
) -> ResolveReviewSourcesRequest {
    let needs_provider = sources.iter().any(|source| {
        source.included && bcode_code_review_models::source_kind_requires_provider(&source.kind)
    });
    let provider_resolutions = if needs_provider {
        gather_provider_resolutions(registry, repo_path, &sources, max_layers)
    } else {
        Vec::new()
    };
    ResolveReviewSourcesRequest {
        repo_path: repo_path.to_path_buf(),
        sources,
        provider_resolutions,
    }
}

/// The open pull requests of `repository` (detected from the remotes when `None`) through
/// `route`: review requested from me first, then most recently updated.
///
/// # Errors
///
/// * The provider's error (for example `unauthenticated` or `unsupported_repository`).
pub fn list_pulls(
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
) -> ProviderResult<ReviewSourcePullsListResponse> {
    route.provider.list_pulls(&ReviewSourcePullsListRequest {
        repo_path: repo_path.to_path_buf(),
        repository,
        query: None,
    })
}

/// Look up the open pull requests `query` names through `route`: a number (`123`, `#123`) or URL,
/// or an exact head branch name (see [`ReviewSourcePullsListRequest::query`]).
///
/// # Errors
///
/// * The provider's error (for example `unauthenticated` or `unsupported_repository`).
pub fn lookup_pulls(
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    query: &str,
) -> ProviderResult<ReviewSourcePullsListResponse> {
    route.provider.list_pulls(&ReviewSourcePullsListRequest {
        repo_path: repo_path.to_path_buf(),
        repository,
        query: Some(query.to_owned()),
    })
}

/// Resolve the stack containing pull request `number` through `route`, cut at `max_layers` when
/// given, with discovery `hints` (see [`ReviewSourceStackResolveRequest::hints`]).
///
/// # Errors
///
/// * The provider's error (for example `pull_not_found`).
pub fn resolve_stack(
    route: &ProviderRoute,
    repo_path: &Path,
    repository: Option<String>,
    number: u64,
    max_layers: Option<usize>,
    hints: Vec<String>,
) -> ProviderResult<ReviewSourceStackResolveResponse> {
    route
        .provider
        .resolve_stack(&ReviewSourceStackResolveRequest {
            repo_path: repo_path.to_path_buf(),
            repository,
            number,
            max_layers,
            hints,
        })
}

/// One provider and repository being asked about sources.
struct Target<'a> {
    registry: &'a ProviderRegistry,
    route: &'a ProviderRoute,
    repo_path: &'a Path,
    repository: &'a str,
    max_layers: Option<usize>,
    batch_fetch: bool,
}

/// A pull request fetch: its number and the base branch fetched with it (`None`: its own).
type FetchKey = (u64, Option<String>);

impl Target<'_> {
    /// The stack containing `number` and the fetches its layers up to `number` need. A stack
    /// already resolved for another source that has `number` as a layer is reused, so one open
    /// resolves each stack once.
    fn stack(
        &self,
        asked: &mut Asked,
        number: u64,
    ) -> ProviderResult<(Option<ReviewStack>, Vec<FetchKey>)> {
        let known = asked
            .stacks
            .iter()
            .find(|stack| stack.layer(number).is_some())
            .cloned()
            .or_else(|| {
                self.registry
                    .known_stack(&self.route.manifest.id, self.repository, number)
            });
        let reused = known.is_some();
        let stack = if let Some(stack) = known {
            stack
        } else {
            let response = crate::discovery::resolve_stack_with_discovery(
                self.registry,
                self.route,
                self.repo_path,
                Some(self.repository.to_string()),
                number,
                self.max_layers,
                None,
            )?;
            for layer in &response.stack.layers {
                asked.layers.insert(
                    layer.pull.number,
                    ReviewPullFetchTarget {
                        number: layer.pull.number,
                        base_branch: layer.pull.base_branch.clone(),
                        head_oid: Some(layer.pull.head_oid.clone()).filter(|oid| !oid.is_empty()),
                        base_oid: layer.pull.base_oid.clone(),
                    },
                );
            }
            response.stack
        };
        if reused {
            for layer in &stack.layers {
                asked
                    .layers
                    .entry(layer.pull.number)
                    .or_insert_with(|| ReviewPullFetchTarget {
                        number: layer.pull.number,
                        base_branch: layer.pull.base_branch.clone(),
                        head_oid: Some(layer.pull.head_oid.clone()).filter(|oid| !oid.is_empty()),
                        base_oid: layer.pull.base_oid.clone(),
                    });
            }
        }
        if !asked.stacks.iter().any(|known| known == &stack) {
            asked.stacks.push(stack.clone());
        }
        let layers = stack.layers_through(number).ok_or_else(|| {
            ProviderCallError::new(
                REVIEW_SOURCE_ERROR_PULL_NOT_FOUND,
                format!("pull request #{number} is not a layer of its stack"),
            )
        })?;
        let keys = layers
            .iter()
            .map(|layer| (layer.pull.number, Some(layer.pull.base_branch.clone())))
            .collect();
        Ok((Some(stack), keys))
    }

    /// Fetch every pull request of `needed` not fetched yet: stack layers in one
    /// `review_source.pulls.fetch` when the provider supports it, the rest one at a time.
    /// Results (and failures) are kept in `asked`.
    fn fetch_all(&self, asked: &mut Asked, needed: &BTreeSet<FetchKey>) {
        let missing: Vec<&FetchKey> = needed
            .iter()
            .filter(|key| !asked.fetched.contains_key(*key))
            .collect();
        let batch: Vec<(&FetchKey, ReviewPullFetchTarget)> = if self.batch_fetch {
            missing
                .iter()
                .filter_map(|key| {
                    let base = key.1.as_ref()?;
                    let target = asked.layers.get(&key.0)?;
                    Some((
                        *key,
                        ReviewPullFetchTarget {
                            base_branch: base.clone(),
                            ..target.clone()
                        },
                    ))
                })
                .collect()
        } else {
            Vec::new()
        };
        if !batch.is_empty() {
            let response = self
                .route
                .provider
                .fetch_pulls(&ReviewSourcePullsFetchRequest {
                    repo_path: self.repo_path.to_path_buf(),
                    repository: Some(self.repository.to_string()),
                    pulls: batch.iter().map(|(_, target)| target.clone()).collect(),
                });
            match response {
                Ok(response) if response.fetched.len() == batch.len() => {
                    for ((key, _), fetched) in batch.iter().zip(response.fetched) {
                        asked.fetched.insert((*key).clone(), Ok(fetched));
                    }
                }
                Ok(response) => {
                    let error = ProviderCallError::new(
                        REVIEW_SOURCE_ERROR_FAILED,
                        format!(
                            "the provider answered {} of {} pull requests",
                            response.fetched.len(),
                            batch.len()
                        ),
                    );
                    for (key, _) in &batch {
                        asked.fetched.insert((*key).clone(), Err(error.clone()));
                    }
                }
                Err(error) => {
                    for (key, _) in &batch {
                        asked.fetched.insert((*key).clone(), Err(error.clone()));
                    }
                }
            }
        }
        for key in missing {
            if asked.fetched.contains_key(key) {
                continue;
            }
            let fetched = self
                .route
                .provider
                .fetch_pull(&ReviewSourcePullFetchRequest {
                    repo_path: self.repo_path.to_path_buf(),
                    repository: Some(self.repository.to_string()),
                    number: key.0,
                    base_branch: key.1.clone(),
                });
            asked.fetched.insert(key.clone(), fetched);
        }
    }
}

/// What one open asked one provider about, so sources naming the same pull requests (for
/// example one source per layer of a stack) share a single resolution and fetch.
#[derive(Debug, Default)]
struct Asked {
    /// Stacks resolved so far.
    stacks: Vec<ReviewStack>,
    /// The layers of those stacks as fetch targets (with the commits the provider reported).
    layers: BTreeMap<u64, ReviewPullFetchTarget>,
    /// Fetch results and failures.
    fetched: BTreeMap<FetchKey, Result<ReviewPullFetchResult, ProviderCallError>>,
}

#[cfg(test)]
mod tests;
