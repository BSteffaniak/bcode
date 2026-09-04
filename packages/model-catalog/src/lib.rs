#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model catalog loading, validation, and static artifact generation.

use bcode_model::{
    CapabilitySource, CapabilitySupport, MediaInputFeature, ModelCacheCapability, ModelCacheInfo,
    ModelCapability, ModelInfo, ModelMetadataSource, ModelPricingInfo, ModelPricingSource,
    ModelPricingUnit, ModelReasoningCapabilitySource, ModelReasoningInfo, ModelTokenPrice,
    StructuredOutputMode, ToolChoiceMode, ToolSchemaMode,
};
use bcode_model_catalog_models::{
    BcodeSupportStatus, CatalogCapabilities, CatalogDocument, CatalogModelStatus, CatalogPricing,
    CatalogProviderKind, LiveCatalogSnapshot, LiveModelMetadata, ModelCatalogEntry,
    ModelDeployment, ModelSupportTarget, ProviderCatalog,
};
use serde_json::json;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

/// Environment variable pointing to a directory of local live snapshots.
const LOCAL_LIVE_DIR_ENV: &str = "BCODE_MODEL_CATALOG_LIVE_DIR";

const EMBEDDED_PROVIDER_CATALOGS: &[(&str, &str)] = &[
    (
        "bedrock.toml",
        include_str!("../../../catalog/models/providers/bedrock.toml"),
    ),
    (
        "openai.toml",
        include_str!("../../../catalog/models/providers/openai.toml"),
    ),
];

mod remote;
mod verification;

pub use remote::{
    DEFAULT_REMOTE_CATALOG_URL, RemoteCatalogClient, RemoteCatalogOptions, overlay_remote_catalog,
    overlay_remote_live,
};
pub use verification::{
    DEFAULT_OPENAI_BASE_URL, DEFAULT_VERIFY_PROMPT, VerificationAuthMode, VerificationOptions,
    VerificationReport, VerificationResult, VerificationStatus, run_verification,
};

/// Result type used by catalog operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Catalog operation error.
#[derive(Debug)]
pub enum Error {
    /// Filesystem error.
    Io(std::io::Error),
    /// TOML parse error.
    Toml(toml::de::Error),
    /// JSON serialization error.
    Json(serde_json::Error),
    /// Remote catalog overlay error.
    RemoteCatalog(String),
    /// Validation error.
    Validation(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Toml(error) => write!(f, "TOML error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::RemoteCatalog(message) => write!(f, "remote catalog error: {message}"),
            Self::Validation(message) => write!(f, "catalog validation error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<toml::de::Error> for Error {
    fn from(value: toml::de::Error) -> Self {
        Self::Toml(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogDiagnostics {
    /// Embedded catalog revision.
    pub embedded_revision: String,
    /// Effective remote catalog revision, when cached or refreshed data is active.
    pub remote_revision: Option<String>,
    /// Whether remote catalog use is enabled.
    pub remote_enabled: bool,
    /// State of the remote catalog cache at startup.
    pub cache_state: remote::CatalogCacheState,
    /// Age of the remote catalog cache at startup.
    pub cache_age: Option<std::time::Duration>,
    /// Last refresh attempt time.
    pub last_refresh_attempt: Option<std::time::SystemTime>,
    /// Last successful refresh time.
    pub last_refresh_success: Option<std::time::SystemTime>,
    /// Last refresh failure.
    pub last_refresh_error: Option<String>,
    /// Whether a refresh is currently running.
    pub refresh_in_progress: bool,
}

/// Model list projection requested by a consumer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelListView {
    /// Complete resolved membership, including explicitly usable hidden models.
    Complete,
    /// Models intended for ordinary picker/list presentation.
    UserVisible,
}

/// Stable identity facts for one catalog-resolved model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelCatalogIdentity {
    /// Catalog provider used for resolution.
    pub provider_id: String,
    /// Stable entry key within the provider catalog.
    pub catalog_entry_id: String,
    /// Catalog model family, when declared.
    pub family: Option<String>,
    /// Required provider API surface, when declared.
    pub api_surface: Option<bcode_model::ModelApiSurface>,
}

#[derive(Debug, Clone)]
pub struct ModelCatalogResolver {
    catalog: std::sync::Arc<tokio::sync::RwLock<std::sync::Arc<ModelCatalog>>>,
    diagnostics: std::sync::Arc<tokio::sync::RwLock<ModelCatalogDiagnostics>>,
    refresh_gate: std::sync::Arc<tokio::sync::Mutex<()>>,
    options: RemoteCatalogOptions,
}

impl ModelCatalogResolver {
    /// Create a resolver from embedded and usable cached catalog data.
    ///
    /// This constructor performs no network I/O.
    ///
    /// # Errors
    ///
    /// Returns an error when the embedded catalog or HTTP client configuration is invalid.
    pub fn new(options: RemoteCatalogOptions) -> Result<Self> {
        let mut document = load_embedded_catalog()?;
        let embedded_revision = document.catalog_revision.clone();
        let mut remote_revision = None;
        let mut cache_state = remote::CatalogCacheState::Disabled;
        let mut cache_age = None;
        if !options.disabled {
            let client = RemoteCatalogClient::new(options.clone())?;
            let cached = client.inspect_cached_catalog();
            cache_state = cached.state;
            cache_age = cached.age;
            if let Some(remote) = cached.value {
                remote_revision = Some(remote.catalog_revision.clone());
                overlay_remote_catalog(&mut document, &remote);
            }
            let provider_ids = document.providers.keys().cloned().collect::<Vec<_>>();
            let snapshots = provider_ids
                .iter()
                .filter_map(|provider_id| client.cached_live_snapshot(provider_id).ok())
                .collect::<Vec<_>>();
            overlay_remote_live(&mut document, &snapshots);
        }
        Ok(Self {
            catalog: std::sync::Arc::new(tokio::sync::RwLock::new(std::sync::Arc::new(
                ModelCatalog::new(document),
            ))),
            diagnostics: std::sync::Arc::new(tokio::sync::RwLock::new(ModelCatalogDiagnostics {
                embedded_revision,
                remote_revision,
                remote_enabled: !options.disabled,
                cache_state,
                cache_age,
                last_refresh_attempt: None,
                last_refresh_success: None,
                last_refresh_error: None,
                refresh_in_progress: false,
            })),
            refresh_gate: std::sync::Arc::new(tokio::sync::Mutex::new(())),
            options,
        })
    }

    /// Create a resolver using only the embedded catalog.
    ///
    /// # Panics
    ///
    /// Panics if the compile-time embedded catalog is invalid.
    #[must_use]
    pub fn embedded() -> Self {
        let options = RemoteCatalogOptions {
            disabled: true,
            ..RemoteCatalogOptions::default()
        };
        Self::new(options).expect("embedded model catalog must be valid")
    }

    /// Spawn a coalesced background refresh without delaying the caller.
    pub fn spawn_refresh(&self) {
        self.refresh_if_stale();
    }

    /// Spawn a background refresh when cached data is stale or the retry interval elapsed.
    pub fn refresh_if_stale(&self) {
        if self.options.disabled {
            return;
        }
        let Ok(diagnostics) = self.diagnostics.try_read() else {
            return;
        };
        let recently_attempted = diagnostics.last_refresh_attempt.is_some_and(|attempt| {
            attempt
                .elapsed()
                .is_ok_and(|elapsed| elapsed < std::time::Duration::from_mins(1))
        });
        if diagnostics.refresh_in_progress || recently_attempted {
            return;
        }
        drop(diagnostics);
        let resolver = self.clone();
        tokio::spawn(async move {
            resolver.refresh_now().await;
        });
    }

    /// Refresh remote data and atomically replace the active snapshot on success.
    pub async fn refresh_now(&self) {
        let Ok(_gate) = self.refresh_gate.try_lock() else {
            return;
        };
        {
            let mut diagnostics = self.diagnostics.write().await;
            diagnostics.refresh_in_progress = true;
            diagnostics.last_refresh_attempt = Some(std::time::SystemTime::now());
        }
        let result = self.fetch_refreshed_catalog().await;
        let mut diagnostics = self.diagnostics.write().await;
        diagnostics.refresh_in_progress = false;
        match result {
            Ok((catalog, revision)) => {
                *self.catalog.write().await = std::sync::Arc::new(catalog);
                diagnostics.remote_revision = Some(revision);
                diagnostics.cache_state = remote::CatalogCacheState::Fresh;
                diagnostics.cache_age = Some(std::time::Duration::ZERO);
                diagnostics.last_refresh_success = Some(std::time::SystemTime::now());
                diagnostics.last_refresh_error = None;
            }
            Err(error) => diagnostics.last_refresh_error = Some(error.to_string()),
        }
    }

    async fn fetch_refreshed_catalog(&self) -> Result<(ModelCatalog, String)> {
        let client = RemoteCatalogClient::new(self.options.clone())?;
        let mut document = load_embedded_catalog()?;
        let remote = client.fetch_catalog().await?;
        let revision = remote.catalog_revision.clone();
        overlay_remote_catalog(&mut document, &remote);
        let provider_ids = document.providers.keys().cloned().collect::<Vec<_>>();
        let mut snapshots = Vec::new();
        for provider_id in &provider_ids {
            if let Ok(snapshot) = client.fetch_live_snapshot(provider_id).await {
                snapshots.push(snapshot);
            }
        }
        overlay_remote_live(&mut document, &snapshots);
        Ok((ModelCatalog::new(document), revision))
    }

    /// Return current resolver diagnostics.
    pub async fn diagnostics(&self) -> ModelCatalogDiagnostics {
        self.diagnostics.read().await.clone()
    }

    /// Return the resolver's current catalog snapshot.
    ///
    /// Callers that need catalog-wide data should use this instead of reloading and re-parsing the
    /// embedded catalog, which is significant per-call work.
    pub async fn catalog_snapshot(&self) -> std::sync::Arc<ModelCatalog> {
        self.catalog.read().await.clone()
    }

    /// Resolve the catalog-known API surface for one model.
    ///
    /// This is a bounded single-entry lookup used when a live provider model list cannot confirm
    /// the selected model, so transport routing does not silently fall back to a provider default.
    pub async fn model_api_surface(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<bcode_model::ModelApiSurface> {
        self.catalog
            .read()
            .await
            .model(provider_id, model_id)
            .and_then(|entry| model_api_surface_from_catalog(entry.api_surface))
    }

    /// Resolve stable identity facts for one model with bounded catalog work.
    pub async fn model_identity(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<ModelCatalogIdentity> {
        let catalog = self.catalog.read().await;
        let identity = {
            let provider = catalog.provider(provider_id)?;
            let entry = find_provider_model(provider, model_id)?;
            let catalog_entry_id = provider
                .models
                .iter()
                .find_map(|(key, candidate)| std::ptr::eq(candidate, entry).then(|| key.clone()))?;
            ModelCatalogIdentity {
                provider_id: provider_id.to_string(),
                catalog_entry_id,
                family: entry.family.clone(),
                api_surface: model_api_surface_from_catalog(entry.api_surface),
            }
        };
        drop(catalog);
        Some(identity)
    }

    /// Resolve the catalog-known output-token limit for one model.
    ///
    /// This bounded lookup keeps request construction aligned with catalog identity when a live
    /// provider model snapshot omits limits or reports an alias/inference-profile identifier.
    pub async fn model_max_output_tokens(&self, provider_id: &str, model_id: &str) -> Option<u32> {
        self.catalog
            .read()
            .await
            .model(provider_id, model_id)
            .and_then(|entry| entry.max_output_tokens)
            .filter(|limit| *limit > 0)
    }

    /// Resolve catalog-known reasoning capabilities for one model.
    ///
    /// This is a bounded single-entry lookup used when a live provider model list cannot confirm
    /// the selected model. It lets callers recover provider-native reasoning semantics, such as an
    /// adaptive-only thinking control, without depending on discovery having completed.
    pub async fn model_reasoning(
        &self,
        provider_id: &str,
        model_id: &str,
    ) -> Option<bcode_model::ModelReasoningInfo> {
        self.catalog
            .read()
            .await
            .model(provider_id, model_id)
            .and_then(reasoning_from_catalog)
    }

    pub async fn resolve_view(
        &self,
        list: bcode_model::ModelList,
        selected_model_id: Option<&str>,
        configured_model_id: Option<&str>,
        view: ModelListView,
    ) -> bcode_model::ModelList {
        let mut resolved = self
            .resolve_selection(list, selected_model_id, configured_model_id)
            .await;
        if view == ModelListView::UserVisible {
            resolved
                .models
                .retain(|model| model.visibility == bcode_model::ModelVisibility::Visible);
        }
        resolved
    }

    /// Resolve provider-returned models through the shared catalog policy.
    pub async fn resolve(&self, list: bcode_model::ModelList) -> bcode_model::ModelList {
        self.resolve_selection(list, None, None).await
    }

    /// Resolve models, preserve selected/configured membership, and choose exactly one default.
    pub async fn resolve_selection(
        &self,
        list: bcode_model::ModelList,
        selected_model_id: Option<&str>,
        configured_model_id: Option<&str>,
    ) -> bcode_model::ModelList {
        let catalog = self.catalog.read().await.clone();
        let mut models = match &list.catalog.policy {
            bcode_model::ModelCatalogPolicy::Unmapped => return list,
            bcode_model::ModelCatalogPolicy::EnrichOnly {
                provider_id,
                target,
                ..
            } => {
                let target = target.as_ref().map(model_support_target_from_hint);
                catalog.merge_provider_models_for_target(
                    provider_id,
                    list.models,
                    false,
                    target.as_ref(),
                )
            }
            bcode_model::ModelCatalogPolicy::ExpandAll { provider_id } => {
                catalog.merge_provider_models(provider_id, list.models, true)
            }
            bcode_model::ModelCatalogPolicy::ExpandSupported {
                provider_id,
                target,
                ..
            } => {
                let mut resolved = catalog.merge_provider_models_for_target(
                    provider_id,
                    list.models,
                    false,
                    Some(&model_support_target_from_hint(target)),
                );
                let mut seen = resolved
                    .iter()
                    .map(|model| model.model_id.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let target = model_support_target_from_hint(target);
                resolved.extend(
                    catalog
                        .provider_models_for_support_target(provider_id, &target, false)
                        .into_iter()
                        .filter(|model| seen.insert(model.model_id.clone())),
                );
                resolved
            }
        };
        let preferred = configured_model_id
            .filter(|model_id| !model_id.trim().is_empty())
            .or_else(|| selected_model_id.filter(|model_id| !model_id.trim().is_empty()));
        if let Some(model_id) = preferred
            && !models.iter().any(|model| model.model_id == model_id)
        {
            let model = bcode_model::ModelInfo {
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
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
                visibility: bcode_model::ModelVisibility::Visible,
            };
            models.push(enrich_preserved_model(
                &catalog,
                &list.catalog.policy,
                model,
            ));
        }
        let provider_default = models
            .iter()
            .find(|model| model.is_default)
            .map(|model| model.model_id.clone());
        let effective_default = preferred
            .filter(|model_id| models.iter().any(|model| model.model_id == *model_id))
            .map(str::to_string)
            .or(provider_default)
            .or_else(|| models.first().map(|model| model.model_id.clone()));
        for model in &mut models {
            model.is_default = effective_default.as_deref() == Some(model.model_id.as_str());
        }
        bcode_model::ModelList {
            models,
            catalog: list.catalog,
        }
    }
}

fn enrich_preserved_model(
    catalog: &ModelCatalog,
    policy: &bcode_model::ModelCatalogPolicy,
    model: bcode_model::ModelInfo,
) -> bcode_model::ModelInfo {
    match policy {
        bcode_model::ModelCatalogPolicy::Unmapped => model,
        bcode_model::ModelCatalogPolicy::EnrichOnly {
            provider_id,
            target,
            ..
        } => match target {
            Some(target) => catalog.enrich_model_for_target(
                provider_id,
                model,
                &model_support_target_from_hint(target),
            ),
            None => catalog.enrich_model_with_defaults(provider_id, model),
        },
        bcode_model::ModelCatalogPolicy::ExpandAll { provider_id } => {
            catalog.enrich_model_with_defaults(provider_id, model)
        }
        bcode_model::ModelCatalogPolicy::ExpandSupported {
            provider_id,
            target,
            ..
        } => catalog.enrich_model_for_target(
            provider_id,
            model,
            &model_support_target_from_hint(target),
        ),
    }
}

/// Runtime wrapper around a model catalog document.
#[derive(Debug, Clone)]
pub struct ModelCatalog {
    document: CatalogDocument,
}

impl ModelCatalog {
    /// Create a catalog wrapper from a loaded document.
    #[must_use]
    pub const fn new(document: CatalogDocument) -> Self {
        Self { document }
    }

    /// Access the underlying catalog document.
    #[must_use]
    pub const fn document(&self) -> &CatalogDocument {
        &self.document
    }

    /// Return a catalog with live model snapshots applied.
    #[must_use]
    pub fn with_live_snapshots(mut self, snapshots: &[LiveCatalogSnapshot]) -> Self {
        merge_live_snapshots(&mut self.document, snapshots);
        self
    }

    /// Load the embedded bundled catalog source.
    ///
    /// # Errors
    ///
    /// Returns an error if embedded catalog source parsing or validation fails.
    pub fn load_bundled() -> Result<Self> {
        load_embedded_catalog().map(Self::new)
    }

    /// Load the bundled catalog and opportunistically overlay remote catalog data.
    ///
    /// Remote fetch/cache failures are ignored so the bundled catalog remains the
    /// reliable source of truth and Bcode stays usable offline.
    ///
    /// # Errors
    ///
    /// Returns an error if bundled catalog source loading or validation fails.
    pub async fn load_bundled_with_remote_overlay() -> Result<Self> {
        Self::load_bundled_with_remote_options(&RemoteCatalogOptions::default()).await
    }

    /// Load the bundled catalog and opportunistically overlay remote catalog data.
    ///
    /// Remote fetch/cache failures are ignored so the bundled catalog remains the
    /// reliable source of truth and Bcode stays usable offline.
    ///
    /// # Errors
    ///
    /// Returns an error if bundled catalog source loading or validation fails.
    pub async fn load_bundled_with_remote_options(options: &RemoteCatalogOptions) -> Result<Self> {
        let mut document = load_embedded_catalog()?;
        apply_remote_overlay_best_effort(&mut document, options).await;
        // Also apply any local live snapshots from the dedicated env var path
        apply_local_live_overlay_best_effort(&mut document);
        Ok(Self::new(document))
    }

    /// Get provider catalog data.
    #[must_use]
    pub fn provider(&self, provider_id: &str) -> Option<&ProviderCatalog> {
        self.document.providers.get(provider_id)
    }

    /// Get a model catalog entry by exact id or alias.
    #[must_use]
    pub fn model(&self, provider_id: &str, model_id: &str) -> Option<&ModelCatalogEntry> {
        self.provider(provider_id)
            .and_then(|provider| find_provider_model(provider, model_id))
    }

    /// Enrich a provider-discovered model with catalog metadata.
    #[must_use]
    pub fn enrich_model(&self, provider_id: &str, model: ModelInfo) -> ModelInfo {
        let model = if let Some(entry) = self.model(provider_id, &model.model_id) {
            enrich_from_entry(model, entry)
        } else {
            model
        };
        self.enrich_image_limit_from_provider_defaults(provider_id, model)
    }

    /// Enrich a provider-discovered model with metadata resolved for an active serving target.
    #[must_use]
    pub fn enrich_model_for_target(
        &self,
        provider_id: &str,
        model: ModelInfo,
        target: &ModelSupportTarget,
    ) -> ModelInfo {
        let model = if let Some(entry) = self.model(provider_id, &model.model_id) {
            enrich_from_entry_for_target(model, entry, target)
        } else {
            model
        };
        self.enrich_image_limit_from_provider_defaults(provider_id, model)
    }

    /// Enrich a provider-discovered model with catalog metadata and provider defaults.
    #[must_use]
    pub fn enrich_model_with_defaults(&self, provider_id: &str, model: ModelInfo) -> ModelInfo {
        let model = if let Some(entry) = self.model(provider_id, &model.model_id) {
            enrich_from_entry(model, entry)
        } else {
            model
        };
        self.enrich_image_limit_from_provider_defaults(provider_id, model)
    }

    fn enrich_image_limit_from_provider_defaults(
        &self,
        provider_id: &str,
        mut model: ModelInfo,
    ) -> ModelInfo {
        if model.max_image_input_base64_bytes.is_none() {
            model.max_image_input_base64_bytes = self
                .provider(provider_id)
                .and_then(|provider| provider.defaults.as_ref())
                .and_then(|defaults| defaults.max_image_input_base64_bytes);
        }
        model
    }

    /// Convert all catalog entries for a provider into `ModelInfo` values.
    #[must_use]
    pub fn provider_models_as_model_info(&self, provider_id: &str) -> Vec<ModelInfo> {
        self.provider(provider_id)
            .map(|provider| {
                provider
                    .models
                    .values()
                    .map(model_info_from_catalog_entry)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Convert catalog entries for a provider matching a support target into `ModelInfo` values.
    #[must_use]
    pub fn provider_models_for_support_target(
        &self,
        provider_id: &str,
        target: &ModelSupportTarget,
        include_unknown: bool,
    ) -> Vec<ModelInfo> {
        self.provider(provider_id)
            .map(|provider| {
                provider
                    .models
                    .values()
                    .filter(|entry| model_matches_support_target(entry, target, include_unknown))
                    .map(|entry| model_info_from_catalog_entry_for_target(entry, target))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Return fallback model ids matching a support target.
    #[must_use]
    pub fn fallback_model_ids_for_support_target(
        &self,
        provider_id: &str,
        target: &ModelSupportTarget,
    ) -> Vec<String> {
        self.provider(provider_id)
            .map(|provider| {
                provider
                    .fallback_model_ids
                    .iter()
                    .filter(|model_id| {
                        provider
                            .models
                            .get(*model_id)
                            .is_some_and(|entry| model_matches_support_target(entry, target, false))
                    })
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Merge discovered models with catalog models resolved for an active serving target.
    #[must_use]
    pub fn merge_provider_models_for_target(
        &self,
        provider_id: &str,
        discovered: Vec<ModelInfo>,
        include_catalog_only: bool,
        target: Option<&ModelSupportTarget>,
    ) -> Vec<ModelInfo> {
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for model in discovered {
            let model = if let Some(target) = target {
                self.enrich_model_for_target(provider_id, model, target)
            } else {
                self.enrich_model_with_defaults(provider_id, model)
            };
            seen.insert(model.model_id.clone());
            result.push(model);
        }

        if include_catalog_only {
            let catalog_models = target.map_or_else(
                || self.provider_models_as_model_info(provider_id),
                |target| self.provider_models_for_support_target(provider_id, target, true),
            );
            for model in catalog_models {
                if seen.insert(model.model_id.clone()) {
                    result.push(model);
                }
            }
        }

        result
    }

    /// Merge discovered provider models with catalog-only models.
    #[must_use]
    pub fn merge_provider_models(
        &self,
        provider_id: &str,
        discovered: Vec<ModelInfo>,
        include_catalog_only: bool,
    ) -> Vec<ModelInfo> {
        let mut result = Vec::new();
        let mut seen = std::collections::BTreeSet::new();
        for model in discovered {
            let model = self.enrich_model_with_defaults(provider_id, model);
            seen.insert(model.model_id.clone());
            result.push(model);
        }

        if include_catalog_only {
            for model in self.provider_models_as_model_info(provider_id) {
                if seen.insert(model.model_id.clone()) {
                    result.push(model);
                }
            }
        }

        result
    }
}

fn apply_local_live_overlay_best_effort(document: &mut CatalogDocument) {
    if let Ok(dir) = std::env::var(LOCAL_LIVE_DIR_ENV) {
        let path = PathBuf::from(dir);
        if let Ok(snapshots) = load_live_snapshots(&path)
            && !snapshots.is_empty()
        {
            merge_live_snapshots(document, &snapshots);
        }
    }
}

async fn apply_remote_overlay_best_effort(
    document: &mut CatalogDocument,
    options: &RemoteCatalogOptions,
) {
    if options.disabled {
        return;
    }
    let Ok(client) = RemoteCatalogClient::new(options.clone()) else {
        return;
    };
    if let Ok(remote_catalog) = client.fetch_catalog().await {
        overlay_remote_catalog(document, &remote_catalog);
    }
    let provider_ids = document.providers.keys().cloned().collect::<Vec<_>>();
    let mut snapshots = Vec::new();
    for provider_id in &provider_ids {
        if let Ok(snapshot) = client.fetch_live_snapshot(provider_id).await {
            snapshots.push(snapshot);
        }
    }
    if !snapshots.is_empty() {
        overlay_remote_live(document, &snapshots);
    }
    // Also apply local live snapshots
    apply_local_live_overlay_best_effort(document);
}

fn model_support_target_from_hint(
    hint: &bcode_model::ModelCatalogSupportHint,
) -> ModelSupportTarget {
    ModelSupportTarget::new(
        hint.provider.clone(),
        hint.auth_mode.clone(),
        hint.api_surface.clone(),
        hint.integration.clone(),
    )
}

fn model_matches_support_target(
    entry: &ModelCatalogEntry,
    target: &ModelSupportTarget,
    include_unknown: bool,
) -> bool {
    entry
        .deployments
        .iter()
        .any(|deployment| deployment.target.matches(target))
        || entry
            .supported_by
            .iter()
            .any(|supported| supported.matches(target))
        || (include_unknown && entry.deployments.is_empty() && entry.supported_by.is_empty())
}

fn find_provider_model<'a>(
    provider: &'a ProviderCatalog,
    model_id: &str,
) -> Option<&'a ModelCatalogEntry> {
    provider.models.get(model_id).or_else(|| {
        provider
            .models
            .values()
            .filter_map(|entry| {
                entry
                    .aliases
                    .iter()
                    .filter_map(|alias| alias_match_specificity(alias, model_id))
                    .max()
                    .map(|specificity| (specificity, entry))
            })
            .max_by_key(|(specificity, _entry)| *specificity)
            .map(|(_specificity, entry)| entry)
    })
}

fn alias_match_specificity(alias: &str, model_id: &str) -> Option<usize> {
    if alias == model_id {
        return Some(usize::MAX);
    }
    if let Some(needle) = alias
        .strip_prefix('*')
        .and_then(|value| value.strip_suffix('*'))
    {
        return model_id.contains(needle).then_some(needle.len());
    }
    alias
        .strip_suffix('*')
        .filter(|prefix| model_id.starts_with(prefix))
        .map(str::len)
}

fn matching_deployment<'a>(
    entry: &'a ModelCatalogEntry,
    target: &ModelSupportTarget,
) -> Option<&'a ModelDeployment> {
    entry
        .deployments
        .iter()
        .filter(|deployment| deployment.target.matches(target))
        .max_by_key(
            |deployment| match deployment.target.integration.as_deref() {
                Some(integration) if Some(integration) == target.integration.as_deref() => 2,
                None => 1,
                Some(_) => 0,
            },
        )
}

fn enrich_from_entry_for_target(
    mut model: ModelInfo,
    entry: &ModelCatalogEntry,
    target: &ModelSupportTarget,
) -> ModelInfo {
    let remote = entry_is_remote(entry);
    let catalog_source = if remote {
        ModelMetadataSource::RemoteCatalog
    } else {
        ModelMetadataSource::BundledCatalog
    };
    model.display_name.clone_from(&entry.display_name);
    let deployment = matching_deployment(entry, target);
    let legacy_target_match = entry.deployments.is_empty()
        && entry
            .supported_by
            .iter()
            .any(|supported| supported.matches(target));
    // A target the entry does not declare must not erase documented limits. Target-specific values
    // take precedence when present, then the entry's own documented values apply. Most catalog
    // entries declare neither `deployments` nor `supported_by`, so without this fallback a
    // target-aware merge would strip their limits and break context accounting.
    if model.context_window.is_none() {
        model.context_window = deployment
            .and_then(|deployment| deployment.context_window)
            .or_else(|| {
                legacy_target_match
                    .then_some(entry.context_window)
                    .flatten()
            })
            .or(entry.context_window);
        if model.context_window.is_some() && model.metadata_source.is_none() {
            model.metadata_source = Some(catalog_source);
        }
    }
    if model.max_output_tokens.is_none() {
        model.max_output_tokens = deployment
            .and_then(|deployment| deployment.max_output_tokens)
            .or(entry.max_output_tokens);
        if model.max_output_tokens.is_some() && model.metadata_source.is_none() {
            model.metadata_source = Some(catalog_source);
        }
    }
    if model.max_image_input_base64_bytes.is_none() {
        model.max_image_input_base64_bytes = entry.max_image_input_base64_bytes;
        if model.max_image_input_base64_bytes.is_some() && model.metadata_source.is_none() {
            model.metadata_source = Some(catalog_source);
        }
    }
    model
        .capabilities
        .extend(capabilities_from_catalog(&entry.capabilities));
    apply_catalog_feature_support(
        &mut model,
        &entry.capabilities,
        if remote {
            CapabilitySource::ProviderApi
        } else {
            CapabilitySource::BundledCatalog
        },
    );
    if let Some(deployment) = deployment {
        model
            .capabilities
            .extend(capabilities_from_catalog(&deployment.capabilities));
        apply_catalog_feature_support(
            &mut model,
            &deployment.capabilities,
            if remote {
                CapabilitySource::ProviderApi
            } else {
                CapabilitySource::BundledCatalog
            },
        );
    }
    merge_model_cache(&mut model.cache, &entry.capabilities);
    if let Some(deployment) = deployment {
        merge_model_cache(&mut model.cache, &deployment.capabilities);
    }
    if model.pricing.is_none() {
        model.pricing = pricing_from_catalog(
            deployment
                .and_then(|deployment| deployment.pricing.as_ref())
                .or(entry.pricing.as_ref()),
            remote,
        );
    }
    let catalog_reasoning = reasoning_from_catalog_parts(
        deployment
            .and_then(|deployment| deployment.reasoning.as_ref())
            .or(entry.reasoning.as_ref()),
        entry.thinking_mode,
    );
    model.reasoning = merge_model_reasoning(model.reasoning.take(), catalog_reasoning);
    if entry.status == CatalogModelStatus::Deprecated {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is deprecated in catalog".to_string(),
        };
    }
    if entry.bcode_support == BcodeSupportStatus::Unsupported {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is marked unsupported by Bcode catalog".to_string(),
        };
    }
    apply_api_surface_visibility(&mut model, entry);
    model
}

fn model_info_from_catalog_entry_for_target(
    entry: &ModelCatalogEntry,
    target: &ModelSupportTarget,
) -> ModelInfo {
    enrich_from_entry_for_target(
        ModelInfo {
            model_id: entry.model_id.clone(),
            display_name: entry.display_name.clone(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        },
        entry,
        target,
    )
}

fn enrich_from_entry(mut model: ModelInfo, entry: &ModelCatalogEntry) -> ModelInfo {
    let remote = entry_is_remote(entry);
    let catalog_source = if remote {
        ModelMetadataSource::RemoteCatalog
    } else {
        ModelMetadataSource::BundledCatalog
    };
    model.display_name.clone_from(&entry.display_name);
    if model.context_window.is_none() && entry.context_window.is_some() {
        model.context_window = entry.context_window;
        model.metadata_source = Some(catalog_source);
    }
    if model.max_output_tokens.is_none() && entry.max_output_tokens.is_some() {
        model.max_output_tokens = entry.max_output_tokens;
        model.metadata_source = Some(catalog_source);
    }
    if model.max_image_input_base64_bytes.is_none() && entry.max_image_input_base64_bytes.is_some()
    {
        model.max_image_input_base64_bytes = entry.max_image_input_base64_bytes;
        model.metadata_source = Some(catalog_source);
    }
    model
        .capabilities
        .extend(capabilities_from_catalog(&entry.capabilities));
    apply_catalog_feature_support(
        &mut model,
        &entry.capabilities,
        if remote {
            CapabilitySource::ProviderApi
        } else {
            CapabilitySource::BundledCatalog
        },
    );
    merge_model_cache(&mut model.cache, &entry.capabilities);
    if model.pricing.is_none()
        && let Some(pricing) = pricing_from_catalog(entry.pricing.as_ref(), remote)
    {
        model.pricing = Some(pricing);
    }
    let catalog_reasoning = reasoning_from_catalog(entry);
    model.reasoning = merge_model_reasoning(model.reasoning.take(), catalog_reasoning);
    if entry.status == CatalogModelStatus::Deprecated {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is deprecated in catalog".to_string(),
        };
    }
    if entry.bcode_support == BcodeSupportStatus::Unsupported {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is marked unsupported by Bcode catalog".to_string(),
        };
    }
    apply_api_surface_visibility(&mut model, entry);
    model
}

/// Record the provider API surface a model must be invoked through.
///
/// Bcode now supports both the Bedrock Converse and Anthropic Messages surfaces, so this records
/// the resolved surface on the model for the host to route the turn to the correct provider
/// adapter. It no longer hides Messages-only models.
const fn apply_api_surface_visibility(model: &mut ModelInfo, entry: &ModelCatalogEntry) {
    if let Some(api_surface) = model_api_surface_from_catalog(entry.api_surface) {
        model.api_surface = Some(api_surface);
    }
}

/// Map the catalog API surface onto the normalized model semantic.
const fn model_api_surface_from_catalog(
    api_surface: Option<bcode_model_catalog_models::CatalogApiSurface>,
) -> Option<bcode_model::ModelApiSurface> {
    match api_surface {
        Some(bcode_model_catalog_models::CatalogApiSurface::Converse) => {
            Some(bcode_model::ModelApiSurface::Converse)
        }
        Some(bcode_model_catalog_models::CatalogApiSurface::InvokeModel) => {
            Some(bcode_model::ModelApiSurface::InvokeModel)
        }
        Some(bcode_model_catalog_models::CatalogApiSurface::Messages) => {
            Some(bcode_model::ModelApiSurface::Messages)
        }
        Some(bcode_model_catalog_models::CatalogApiSurface::Responses) => {
            Some(bcode_model::ModelApiSurface::Responses)
        }
        None => None,
    }
}

fn model_info_from_catalog_entry(entry: &ModelCatalogEntry) -> ModelInfo {
    let mut model = ModelInfo {
        model_id: entry.model_id.clone(),
        display_name: entry.display_name.clone(),
        is_default: false,
        context_window: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
        max_image_input_base64_bytes: entry.max_image_input_base64_bytes,
        capabilities: capabilities_from_catalog(&entry.capabilities),
        feature_support: feature_support_from_catalog(
            &entry.capabilities,
            if entry_is_remote(entry) {
                CapabilitySource::ProviderApi
            } else {
                CapabilitySource::BundledCatalog
            },
        ),
        reasoning: reasoning_from_catalog(entry),
        cache: cache_info_from_catalog(&entry.capabilities),
        metadata_source: Some(if entry_is_remote(entry) {
            ModelMetadataSource::RemoteCatalog
        } else {
            ModelMetadataSource::BundledCatalog
        }),
        pricing: pricing_from_catalog(entry.pricing.as_ref(), entry_is_remote(entry)),
        api_surface: None,
        visibility: bcode_model::ModelVisibility::Visible,
    };
    if entry.bcode_support == BcodeSupportStatus::Unsupported {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is marked unsupported by Bcode catalog".to_string(),
        };
    }
    apply_api_surface_visibility(&mut model, entry);
    model
}

fn catalog_boolean_support(
    supported: bool,
    source: CapabilitySource,
    unsupported_reason: &str,
) -> CapabilitySupport {
    if supported {
        CapabilitySupport::supported(source)
    } else {
        CapabilitySupport::Unsupported {
            source,
            reason: unsupported_reason.to_string(),
        }
    }
}

fn feature_support_from_catalog(
    capabilities: &CatalogCapabilities,
    source: CapabilitySource,
) -> bcode_model::ModelFeatureSupport {
    let mut support = bcode_model::ModelFeatureSupport::default();
    if capabilities.image_input {
        support.media_input.extend([
            (
                MediaInputFeature::UserImage,
                CapabilitySupport::supported(source),
            ),
            (
                MediaInputFeature::ToolResultImage,
                CapabilitySupport::supported(source),
            ),
        ]);
    }
    if capabilities.tool_use {
        support.tool_schema.extend([
            (
                ToolSchemaMode::Permissive,
                CapabilitySupport::supported(source),
            ),
            (ToolSchemaMode::Strict, CapabilitySupport::supported(source)),
        ]);
    }
    if capabilities.structured_outputs {
        support.structured_output.extend([
            (
                StructuredOutputMode::JsonSchema,
                CapabilitySupport::supported(source),
            ),
            (
                StructuredOutputMode::StrictJsonSchema,
                CapabilitySupport::supported(source),
            ),
        ]);
    }
    if capabilities.prompt_cache {
        support.prompt_cache.extend([
            (
                bcode_model::PromptCacheFeature::ConversationPrefix,
                CapabilitySupport::supported(source),
            ),
            (
                bcode_model::PromptCacheFeature::ExplicitSystem,
                CapabilitySupport::supported(source),
            ),
            (
                bcode_model::PromptCacheFeature::ExplicitTools,
                CapabilitySupport::supported(source),
            ),
            (
                bcode_model::PromptCacheFeature::ExplicitMessage,
                CapabilitySupport::supported(source),
            ),
        ]);
        if !capabilities.prompt_cache_ttl_seconds.is_empty() {
            support.prompt_cache.insert(
                bcode_model::PromptCacheFeature::Ttl,
                CapabilitySupport::supported(source),
            );
        }
    }
    if let Some(required) = capabilities.required_tool_choice {
        support.tool_choice.insert(
            ToolChoiceMode::Required,
            catalog_boolean_support(
                required,
                source,
                "model catalog explicitly marks required tool choice unsupported",
            ),
        );
    }
    if let Some(named) = capabilities.named_tool_choice {
        support.tool_choice.insert(
            ToolChoiceMode::Named,
            catalog_boolean_support(
                named,
                source,
                "model catalog explicitly marks named tool choice unsupported",
            ),
        );
    }
    if let Some(parallel) = capabilities.parallel_tool_calls {
        support.tool_choice.insert(
            ToolChoiceMode::Parallel,
            catalog_boolean_support(
                parallel,
                source,
                "model catalog explicitly marks parallel tool calls unsupported",
            ),
        );
    }
    support
}

fn apply_catalog_feature_support(
    model: &mut ModelInfo,
    capabilities: &CatalogCapabilities,
    source: CapabilitySource,
) {
    let claims = feature_support_from_catalog(capabilities, source);
    model
        .feature_support
        .structured_output
        .extend(claims.structured_output);
    model.feature_support.tool_schema.extend(claims.tool_schema);
    model.feature_support.tool_choice.extend(claims.tool_choice);
    model
        .feature_support
        .prompt_cache
        .extend(claims.prompt_cache);
    model.feature_support.media_input.extend(claims.media_input);
}

fn capabilities_from_catalog(
    capabilities: &CatalogCapabilities,
) -> std::collections::BTreeSet<ModelCapability> {
    let mut result = std::collections::BTreeSet::new();
    if capabilities.text_output {
        result.insert(ModelCapability::StreamingText);
    }
    if capabilities.image_input {
        result.insert(ModelCapability::ImageInput);
    }
    if capabilities.tool_use {
        result.insert(ModelCapability::ToolCalls);
    }
    if capabilities.parallel_tool_calls == Some(true) {
        result.insert(ModelCapability::ParallelToolCalls);
    }
    if capabilities.prompt_cache {
        result.insert(ModelCapability::PromptCaching);
    }
    if capabilities.reasoning {
        result.insert(ModelCapability::Reasoning);
    }
    if capabilities.native_web_search {
        result.insert(ModelCapability::NativeWebSearch);
    }
    result
}

fn merge_model_cache(cache: &mut ModelCacheInfo, capabilities: &CatalogCapabilities) {
    let catalog = cache_info_from_catalog(capabilities);
    cache.capabilities.extend(catalog.capabilities);
    cache.ttl_seconds.extend(catalog.ttl_seconds);
}

fn cache_info_from_catalog(capabilities: &CatalogCapabilities) -> ModelCacheInfo {
    let mut cache = ModelCacheInfo {
        ttl_seconds: capabilities.prompt_cache_ttl_seconds.clone(),
        ..ModelCacheInfo::default()
    };
    if capabilities.prompt_cache {
        cache.capabilities.extend([
            ModelCacheCapability::PromptCacheKey,
            ModelCacheCapability::CacheUsageReporting,
        ]);
        cache
            .capabilities
            .insert(if capabilities.explicit_prompt_cache {
                ModelCacheCapability::ExplicitCachePoints
            } else {
                ModelCacheCapability::AutomaticPrefixCache
            });
    }
    cache
}

fn entry_is_remote(entry: &ModelCatalogEntry) -> bool {
    entry
        .live
        .as_ref()
        .and_then(|live| live.source.as_deref())
        .is_some_and(|source| source.starts_with("remote_"))
}

fn pricing_from_catalog(
    pricing: Option<&CatalogPricing>,
    remote: bool,
) -> Option<ModelPricingInfo> {
    let pricing = pricing?;
    Some(ModelPricingInfo {
        currency: pricing.currency.clone(),
        unit: ModelPricingUnit::PerMillionTokens,
        input: pricing.input_micros.map(ModelTokenPrice::from_micros),
        cached_input: pricing
            .cached_input_micros
            .map(ModelTokenPrice::from_micros),
        cache_write_input: pricing
            .cache_write_input_micros
            .map(ModelTokenPrice::from_micros),
        output: pricing.output_micros.map(ModelTokenPrice::from_micros),
        context_threshold_tokens: pricing.context_threshold_tokens,
        rules: catalog_pricing_rules(pricing),
        revision: pricing.revision.clone(),
        source: if remote {
            ModelPricingSource::RemoteCatalog
        } else {
            ModelPricingSource::BundledCatalog
        },
    })
}

fn catalog_pricing_rules(pricing: &CatalogPricing) -> Vec<bcode_model::ModelPricingRule> {
    use bcode_model::{
        ModelInvocationClass, ModelPricingBucket, ModelPricingRule, ModelTokenModality,
    };
    if !pricing.rules.is_empty() {
        return pricing
            .rules
            .iter()
            .map(|rule| ModelPricingRule {
                bucket: match rule.bucket {
                    bcode_model_catalog_models::CatalogPricingBucket::Input => {
                        ModelPricingBucket::Input
                    }
                    bcode_model_catalog_models::CatalogPricingBucket::CacheReadInput => {
                        ModelPricingBucket::CacheReadInput
                    }
                    bcode_model_catalog_models::CatalogPricingBucket::CacheWriteInput => {
                        ModelPricingBucket::CacheWriteInput
                    }
                    bcode_model_catalog_models::CatalogPricingBucket::Output => {
                        ModelPricingBucket::Output
                    }
                },
                modality: rule.modality.map(|modality| match modality {
                    bcode_model_catalog_models::CatalogTokenModality::Text => {
                        ModelTokenModality::Text
                    }
                    bcode_model_catalog_models::CatalogTokenModality::Image => {
                        ModelTokenModality::Image
                    }
                    bcode_model_catalog_models::CatalogTokenModality::Audio => {
                        ModelTokenModality::Audio
                    }
                    bcode_model_catalog_models::CatalogTokenModality::Video => {
                        ModelTokenModality::Video
                    }
                }),
                service_tier: rule.service_tier.clone(),
                invocation_class: rule.invocation_class.map(|class| match class {
                    bcode_model_catalog_models::CatalogInvocationClass::OnDemand => {
                        ModelInvocationClass::OnDemand
                    }
                    bcode_model_catalog_models::CatalogInvocationClass::Batch => {
                        ModelInvocationClass::Batch
                    }
                }),
                cache_ttl_seconds: rule.cache_ttl_seconds,
                min_request_input_tokens: rule.min_request_input_tokens,
                max_request_input_tokens: rule.max_request_input_tokens,
                billing_scope: rule.billing_scope.clone(),
                price: ModelTokenPrice::from_micros(rule.price_micros),
            })
            .collect();
    }
    flat_catalog_pricing_rules(pricing)
}

fn flat_catalog_pricing_rules(pricing: &CatalogPricing) -> Vec<bcode_model::ModelPricingRule> {
    use bcode_model::{ModelPricingBucket, ModelPricingRule, ModelTokenModality};
    [
        (ModelPricingBucket::Input, pricing.input_micros),
        (
            ModelPricingBucket::CacheReadInput,
            pricing.cached_input_micros,
        ),
        (
            ModelPricingBucket::CacheWriteInput,
            pricing.cache_write_input_micros,
        ),
        (ModelPricingBucket::Output, pricing.output_micros),
    ]
    .into_iter()
    .filter_map(|(bucket, micros)| {
        micros.map(|micros| ModelPricingRule {
            bucket,
            modality: Some(ModelTokenModality::Text),
            service_tier: None,
            invocation_class: None,
            cache_ttl_seconds: None,
            min_request_input_tokens: None,
            max_request_input_tokens: None,
            billing_scope: None,
            price: ModelTokenPrice::from_micros(micros),
        })
    })
    .collect()
}

fn merge_model_reasoning(
    discovered: Option<ModelReasoningInfo>,
    catalog: Option<ModelReasoningInfo>,
) -> Option<ModelReasoningInfo> {
    match (discovered, catalog) {
        (None, None) => None,
        (Some(reasoning), None) | (None, Some(reasoning)) => Some(reasoning),
        (Some(mut discovered), Some(catalog)) => {
            discovered.control = catalog.control.or(discovered.control);
            discovered.effort_values.extend(catalog.effort_values);
            discovered.effort_values =
                bcode_model::ordered_reasoning_effort_values(&discovered.effort_values);
            discovered.default_effort = catalog.default_effort.or(discovered.default_effort);
            discovered.visible_summary_supported |= catalog.visible_summary_supported;
            discovered.summary_values.extend(catalog.summary_values);
            discovered.summary_values.sort();
            discovered.summary_values.dedup();
            discovered.default_summary = catalog.default_summary.or(discovered.default_summary);
            discovered.raw_reasoning_supported |= catalog.raw_reasoning_supported;
            Some(discovered)
        }
    }
}

fn reasoning_from_catalog(entry: &ModelCatalogEntry) -> Option<ModelReasoningInfo> {
    reasoning_from_catalog_parts(entry.reasoning.as_ref(), entry.thinking_mode)
}

/// Map the catalog thinking-control shape onto the normalized model semantic.
const fn reasoning_control_from_catalog(
    thinking_mode: Option<bcode_model_catalog_models::CatalogThinkingMode>,
) -> Option<bcode_model::ReasoningControl> {
    match thinking_mode {
        Some(bcode_model_catalog_models::CatalogThinkingMode::Budget) => {
            Some(bcode_model::ReasoningControl::Budget)
        }
        Some(bcode_model_catalog_models::CatalogThinkingMode::Adaptive) => {
            Some(bcode_model::ReasoningControl::Adaptive)
        }
        None => None,
    }
}

/// Build normalized reasoning info from the catalog's reasoning table and thinking mode.
///
/// A declared `thinking_mode` is sufficient on its own: it determines the provider-native request
/// shape, so it must survive even when a model omits the optional `[reasoning]` effort table.
/// Dropping it here would silently downgrade an adaptive-only model to the budget request shape,
/// which such models reject.
fn reasoning_from_catalog_parts(
    reasoning: Option<&bcode_model_catalog_models::CatalogReasoning>,
    thinking_mode: Option<bcode_model_catalog_models::CatalogThinkingMode>,
) -> Option<ModelReasoningInfo> {
    let control = reasoning_control_from_catalog(thinking_mode);
    if reasoning.is_none() && control.is_none() {
        return None;
    }
    let default_reasoning = bcode_model_catalog_models::CatalogReasoning::default();
    let reasoning = reasoning.unwrap_or(&default_reasoning);
    Some(ModelReasoningInfo {
        control,
        effort_values: reasoning.effort_values.iter().cloned().collect(),
        default_effort: reasoning.default_effort.clone(),
        visible_summary_supported: !reasoning.summary_values.is_empty(),
        summary_values: reasoning.summary_values.iter().cloned().collect(),
        default_summary: reasoning
            .default_summary
            .clone()
            .or_else(|| reasoning.summary_values.iter().next().cloned()),
        raw_reasoning_supported: reasoning.raw_reasoning_supported,
        source: ModelReasoningCapabilitySource::KnownModelTable,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputFormat {
    /// Compact JSON.
    Json,
    /// Pretty-printed JSON.
    PrettyJson,
}

/// Load the embedded catalog bundled into this binary.
///
/// # Errors
///
/// Returns an error when embedded provider TOML parsing or validation fails.
pub fn load_embedded_catalog() -> Result<CatalogDocument> {
    let mut catalog = CatalogDocument::empty(catalog_revision(), generated_at());

    for (name, contents) in EMBEDDED_PROVIDER_CATALOGS {
        let provider = parse_provider_catalog(contents, name)?;
        insert_provider_catalog(&mut catalog, provider, name)?;
    }

    stamp_missing_pricing_revisions(&mut catalog);
    validate_catalog(&catalog)?;
    Ok(catalog)
}

/// Load a catalog from provider TOML files in a source directory.
///
/// # Errors
///
/// Returns an error when the source directory cannot be read, provider TOML cannot be parsed,
/// or catalog validation fails.
pub fn load_catalog(source_dir: &Path) -> Result<CatalogDocument> {
    let providers_dir = source_dir.join("providers");
    let mut catalog = CatalogDocument::empty(catalog_revision(), generated_at());

    for entry in fs::read_dir(&providers_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("toml") {
            continue;
        }
        let contents = fs::read_to_string(&path)?;
        let source = path.display().to_string();
        let provider = parse_provider_catalog(&contents, &source)?;
        insert_provider_catalog(&mut catalog, provider, &source)?;
    }

    stamp_missing_pricing_revisions(&mut catalog);
    validate_catalog(&catalog)?;
    Ok(catalog)
}

fn stamp_missing_pricing_revisions(catalog: &mut CatalogDocument) {
    let revision = catalog.catalog_revision.clone();
    for provider in catalog.providers.values_mut() {
        for entry in provider.models.values_mut() {
            if let Some(pricing) = &mut entry.pricing
                && pricing.revision.is_none()
            {
                pricing.revision = Some(revision.clone());
            }
            for deployment in &mut entry.deployments {
                if let Some(pricing) = &mut deployment.pricing
                    && pricing.revision.is_none()
                {
                    pricing.revision = Some(revision.clone());
                }
            }
        }
    }
}

fn parse_provider_catalog(contents: &str, source: &str) -> Result<ProviderCatalog> {
    let provider: ProviderCatalog = toml::from_str(contents)?;
    if provider.provider_id.trim().is_empty() {
        return Err(Error::Validation(format!(
            "provider id is empty in {source}"
        )));
    }
    Ok(provider)
}

fn insert_provider_catalog(
    catalog: &mut CatalogDocument,
    provider: ProviderCatalog,
    source: &str,
) -> Result<()> {
    let previous = catalog
        .providers
        .insert(provider.provider_id.clone(), provider);
    if previous.is_some() {
        return Err(Error::Validation(format!(
            "duplicate provider id in {source}"
        )));
    }
    Ok(())
}

/// Validate a catalog document.
///
/// # Errors
///
/// Returns an error when provider/model ids are inconsistent or required generated keys are duplicated.
pub fn validate_catalog(catalog: &CatalogDocument) -> Result<()> {
    for (provider_id, provider) in &catalog.providers {
        if provider_id != &provider.provider_id {
            return Err(Error::Validation(format!(
                "provider map key '{provider_id}' does not match provider_id '{}'",
                provider.provider_id
            )));
        }
        for (model_id, model) in &provider.models {
            if model_id != &model.model_id {
                return Err(Error::Validation(format!(
                    "model map key '{model_id}' does not match model_id '{}' for provider '{provider_id}'",
                    model.model_id
                )));
            }
            let mut deployment_targets = std::collections::BTreeSet::new();
            for deployment in &model.deployments {
                if !deployment_targets.insert(deployment.target.clone()) {
                    return Err(Error::Validation(format!(
                        "duplicate deployment target for model '{model_id}' and provider '{provider_id}': {}/{}/{:?}",
                        deployment.target.auth_mode,
                        deployment.target.api_surface,
                        deployment.target.integration,
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Build static catalog artifacts into an output directory.
///
/// # Errors
///
/// Returns an error if catalog loading, validation, serialization, or file writes fail.
pub fn build_artifacts(source_dir: &Path, output_dir: &Path, format: OutputFormat) -> Result<()> {
    build_artifacts_with_live(source_dir, None, output_dir, format)
}

/// Build static catalog artifacts with optional generated live snapshots.
///
/// # Errors
///
/// Returns an error if catalog loading, live snapshot loading, validation, serialization, or file writes fail.
pub fn build_artifacts_with_live(
    source_dir: &Path,
    live_dir: Option<&Path>,
    output_dir: &Path,
    format: OutputFormat,
) -> Result<()> {
    let mut catalog = load_catalog(source_dir)?;
    let live_snapshots = if let Some(live_dir) = live_dir {
        load_live_snapshots(live_dir)?
    } else {
        Vec::new()
    };
    merge_live_snapshots(&mut catalog, &live_snapshots);
    write_artifacts(&catalog, output_dir, format)?;

    if !live_snapshots.is_empty() {
        let live_output_dir = output_dir.join("live");
        fs::create_dir_all(&live_output_dir)?;
        for snapshot in &live_snapshots {
            let target_suffix = snapshot.target.as_ref().map_or_else(String::new, |target| {
                format!(
                    "-{}-{}-{}",
                    target.auth_mode,
                    target.api_surface,
                    target.integration.as_deref().unwrap_or("generic")
                )
            });
            write_json(
                &live_output_dir.join(format!("{}{target_suffix}.json", snapshot.provider_id)),
                snapshot,
                format,
            )?;
        }
    }

    Ok(())
}

/// Load generated live snapshots from a directory.
///
/// # Errors
///
/// Returns an error if the directory cannot be read or a snapshot cannot be parsed.
pub fn load_live_snapshots(live_dir: &Path) -> Result<Vec<LiveCatalogSnapshot>> {
    let mut snapshots = Vec::new();
    if !live_dir.exists() {
        return Ok(snapshots);
    }
    for entry in fs::read_dir(live_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|value| value.to_str()) != Some("json") {
            continue;
        }
        let contents = fs::read_to_string(&path)?;
        let snapshot: LiveCatalogSnapshot = serde_json::from_str(&contents)?;
        snapshots.push(snapshot);
    }
    Ok(snapshots)
}

/// Merge generated live snapshots into a catalog document.
pub fn merge_live_snapshots(catalog: &mut CatalogDocument, snapshots: &[LiveCatalogSnapshot]) {
    for snapshot in snapshots {
        // Auto-create provider if it does not exist (live data is the source of truth for new
        // providers).
        let provider = catalog
            .providers
            .entry(snapshot.provider_id.clone())
            .or_insert_with(|| ProviderCatalog {
                provider_id: snapshot.provider_id.clone(),
                display_name: snapshot.provider_id.clone(),
                kind: CatalogProviderKind::Other,
                website_url: None,
                default_model_id: None,
                default_codex_model_id: None,
                fallback_model_ids: Vec::new(),
                defaults: None,
                error_handling: bcode_model_catalog_models::ProviderErrorHandlingMetadata::default(
                ),
                models: std::collections::BTreeMap::new(),
            });

        for live_model in snapshot.models.values() {
            let live_target = live_model.target.as_ref().or(snapshot.target.as_ref());
            let matching_key = remote::matching_local_model_key(provider, &live_model.model_id)
                .unwrap_or_else(|| live_model.model_id.clone());
            let is_alias_match = matching_key != live_model.model_id;
            if is_alias_match
                && !is_versioned_form_of(&live_model.model_id, &matching_key)
                && let Some(entry) = priced_family_alias_entry(provider, &matching_key, live_model)
            {
                provider.models.insert(
                    live_model.model_id.clone(),
                    with_live_metadata(entry, live_model, snapshot),
                );
                continue;
            }
            let entry = provider
                .models
                .entry(matching_key)
                .or_insert_with(|| live_model_entry(live_model, snapshot));
            if is_alias_match {
                // Live deployment facts must not replace canonical semantic capabilities.
                entry.aliases.insert(live_model.model_id.clone());
                if entry.pricing.is_none()
                    && live_model.pricing.is_some()
                    && is_versioned_form_of(&live_model.model_id, &entry.model_id)
                {
                    entry.pricing.clone_from(&live_model.pricing);
                }
                entry.live = Some(LiveModelMetadata {
                    status: live_model.status.clone(),
                    regions: live_model.regions.clone(),
                    last_seen_at: Some(snapshot.generated_at.clone()),
                    source: Some("provider_live".to_string()),
                });
                continue;
            }
            if entry.display_name.trim().is_empty()
                && let Some(display_name) = &live_model.display_name
            {
                entry.display_name.clone_from(display_name);
            }
            entry.aliases.extend(live_model.aliases.iter().cloned());
            if let Some(target) = live_target {
                merge_live_deployment(entry, target, live_model);
            } else {
                if entry.context_window.is_none() {
                    entry.context_window = live_model.context_window;
                }
                if entry.max_output_tokens.is_none() {
                    entry.max_output_tokens = live_model.max_output_tokens;
                }
                if entry.reasoning.is_none() {
                    entry.reasoning.clone_from(&live_model.reasoning);
                }
                if live_model.pricing.is_some() {
                    entry.pricing.clone_from(&live_model.pricing);
                }
            }
            entry.capabilities = merge_capabilities(&entry.capabilities, &live_model.capabilities);
            entry.live = Some(LiveModelMetadata {
                status: live_model.status.clone(),
                regions: live_model.regions.clone(),
                last_seen_at: Some(snapshot.generated_at.clone()),
                source: Some("provider_live".to_string()),
            });
        }
    }
}

/// Merge a target-scoped live model into the matching deployment, creating one when absent.
fn merge_live_deployment(
    entry: &mut ModelCatalogEntry,
    target: &ModelSupportTarget,
    live_model: &bcode_model_catalog_models::LiveModel,
) {
    let deployment = entry
        .deployments
        .iter_mut()
        .find(|deployment| deployment.target == *target);
    if let Some(deployment) = deployment {
        deployment.context_window = live_model.context_window.or(deployment.context_window);
        deployment.max_output_tokens = live_model
            .max_output_tokens
            .or(deployment.max_output_tokens);
        if live_model.reasoning.is_some() {
            deployment.reasoning.clone_from(&live_model.reasoning);
        }
        if live_model.pricing.is_some() {
            deployment.pricing.clone_from(&live_model.pricing);
        }
        deployment.capabilities =
            merge_capabilities(&deployment.capabilities, &live_model.capabilities);
    } else {
        entry.deployments.push(ModelDeployment {
            target: target.clone(),
            context_window: live_model.context_window,
            max_output_tokens: live_model.max_output_tokens,
            capabilities: live_model.capabilities.clone(),
            reasoning: live_model.reasoning.clone(),
            pricing: live_model.pricing.clone(),
        });
    }
}

/// Build a dedicated catalog entry for a priced live model that only matched a curated family
/// alias glob.
///
/// Pricing is per SKU, while alias entries are typically family globs (`*nova*`, `*llama3*`) that
/// cover many differently priced SKUs. The priced live model gets its own entry that inherits the
/// family's curated semantics, so the exact-key match wins over the glob during enrichment
/// without mutating the family entry. Returns `None` when the live model carries no pricing or
/// the curated family already has pricing (for example Fable 5.1's per-scope rules), which stays
/// authoritative.
fn priced_family_alias_entry(
    provider: &ProviderCatalog,
    family_key: &str,
    live_model: &bcode_model_catalog_models::LiveModel,
) -> Option<ModelCatalogEntry> {
    live_model.pricing.as_ref()?;
    let family = provider.models.get(family_key)?;
    if family.pricing.is_some() {
        return None;
    }
    let mut entry = family.clone();
    entry.model_id.clone_from(&live_model.model_id);
    entry.aliases.clear();
    // The derived entry exists to enrich this live ID when discovered. It must not be expanded
    // into pickers as a catalog-only model, or the concrete family entry (for example the Mantle
    // `openai.gpt-oss-120b`) would appear twice.
    entry.supported_by.clear();
    entry.deployments.clear();
    entry.pricing.clone_from(&live_model.pricing);
    if entry.context_window.is_none() {
        entry.context_window = live_model.context_window;
    }
    if entry.max_output_tokens.is_none() {
        entry.max_output_tokens = live_model.max_output_tokens;
    }
    Some(entry)
}

/// Whether a live model ID is a versioned form of a concrete catalog model ID.
///
/// Bedrock appends `-<version>:<minor>` to on-demand IDs (`openai.gpt-oss-120b-1:0`), while the
/// catalog keys the model without it. Such an entry is a specific model rather than a family
/// glob, so live pricing belongs on the entry itself.
fn is_versioned_form_of(live_model_id: &str, catalog_model_id: &str) -> bool {
    live_model_id
        .strip_prefix(catalog_model_id)
        .is_some_and(|suffix| {
            suffix
                .strip_prefix('-')
                .is_some_and(|version| version.chars().all(|c| c.is_ascii_digit() || c == ':'))
        })
}

fn with_live_metadata(
    mut entry: ModelCatalogEntry,
    live_model: &bcode_model_catalog_models::LiveModel,
    snapshot: &LiveCatalogSnapshot,
) -> ModelCatalogEntry {
    entry.live = Some(LiveModelMetadata {
        status: live_model.status.clone(),
        regions: live_model.regions.clone(),
        last_seen_at: Some(snapshot.generated_at.clone()),
        source: Some("provider_live".to_string()),
    });
    entry
}

fn live_model_entry(
    live_model: &bcode_model_catalog_models::LiveModel,
    snapshot: &LiveCatalogSnapshot,
) -> ModelCatalogEntry {
    let target = live_model.target.as_ref().or(snapshot.target.as_ref());
    ModelCatalogEntry {
        model_id: live_model.model_id.clone(),
        display_name: live_model
            .display_name
            .clone()
            .unwrap_or_else(|| live_model.model_id.clone()),
        aliases: live_model.aliases.clone(),
        status: CatalogModelStatus::Unknown,
        bcode_support: BcodeSupportStatus::Unknown,
        context_window: target
            .is_none()
            .then_some(live_model.context_window)
            .flatten(),
        max_output_tokens: target
            .is_none()
            .then_some(live_model.max_output_tokens)
            .flatten(),
        max_image_input_base64_bytes: None,
        family: None,
        provider_model_kind: None,
        replaced_by: None,
        notes: None,
        documentation_url: None,
        pricing: live_model.pricing.clone(),
        capabilities: live_model.capabilities.clone(),
        reasoning: live_model.reasoning.clone(),
        api_surface: None,
        thinking_mode: None,
        supported_by: target.cloned().into_iter().collect(),
        deployments: target
            .cloned()
            .map(|target| ModelDeployment {
                target,
                context_window: live_model.context_window,
                max_output_tokens: live_model.max_output_tokens,
                capabilities: live_model.capabilities.clone(),
                reasoning: live_model.reasoning.clone(),
                pricing: live_model.pricing.clone(),
            })
            .into_iter()
            .collect(),
        live: Some(LiveModelMetadata {
            status: live_model.status.clone(),
            regions: live_model.regions.clone(),
            last_seen_at: Some(snapshot.generated_at.clone()),
            source: Some("provider_live".to_string()),
        }),
        source: bcode_model_catalog_models::CatalogSourceMetadata::default(),
    }
}

pub(crate) fn merge_capabilities(
    left: &CatalogCapabilities,
    right: &CatalogCapabilities,
) -> CatalogCapabilities {
    CatalogCapabilities {
        text_input: left.text_input || right.text_input,
        image_input: left.image_input || right.image_input,
        text_output: left.text_output || right.text_output,
        tool_use: left.tool_use || right.tool_use,
        parallel_tool_calls: right.parallel_tool_calls.or(left.parallel_tool_calls),
        required_tool_choice: right.required_tool_choice.or(left.required_tool_choice),
        named_tool_choice: right.named_tool_choice.or(left.named_tool_choice),
        structured_outputs: left.structured_outputs || right.structured_outputs,
        reasoning: left.reasoning || right.reasoning,
        prompt_cache: left.prompt_cache || right.prompt_cache,
        explicit_prompt_cache: left.explicit_prompt_cache || right.explicit_prompt_cache,
        prompt_cache_ttl_seconds: left
            .prompt_cache_ttl_seconds
            .union(&right.prompt_cache_ttl_seconds)
            .copied()
            .collect(),
        native_web_search: left.native_web_search || right.native_web_search,
    }
}

fn write_artifacts(
    catalog: &CatalogDocument,
    output_dir: &Path,
    format: OutputFormat,
) -> Result<()> {
    fs::create_dir_all(output_dir.join("providers"))?;

    write_json(&output_dir.join("catalog.json"), &catalog, format)?;

    let providers = catalog
        .providers
        .values()
        .map(|provider| {
            json!({
                "provider_id": provider.provider_id,
                "display_name": provider.display_name,
                "kind": provider.kind,
                "model_count": provider.models.len(),
                "website_url": provider.website_url,
            })
        })
        .collect::<Vec<_>>();
    write_json(&output_dir.join("providers.json"), &providers, format)?;

    let mut search_index = Vec::new();
    for provider in catalog.providers.values() {
        write_json(
            &output_dir
                .join("providers")
                .join(format!("{}.json", provider.provider_id)),
            provider,
            format,
        )?;
        for model in provider.models.values() {
            search_index.push(json!({
                "provider_id": provider.provider_id,
                "provider_display_name": provider.display_name,
                "model_id": model.model_id,
                "display_name": model.display_name,
                "status": model.status,
                "bcode_support": model.bcode_support,
                "context_window": model.context_window,
                "max_output_tokens": model.max_output_tokens,
                "capabilities": model.capabilities,
            }));
        }
    }
    write_json(&output_dir.join("search-index.json"), &search_index, format)?;
    Ok(())
}

fn write_json<T: serde::Serialize>(path: &Path, value: &T, format: OutputFormat) -> Result<()> {
    let bytes = match format {
        OutputFormat::Json => serde_json::to_vec(value)?,
        OutputFormat::PrettyJson => serde_json::to_vec_pretty(value)?,
    };
    fs::write(path, bytes)?;
    Ok(())
}

fn catalog_revision() -> String {
    option_env!("GIT_HASH").unwrap_or("unknown").to_string()
}

fn generated_at() -> String {
    std::env::var("BCODE_MODEL_CATALOG_GENERATED_AT")
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}

/// Default source directory for checked-in catalog TOML files.
#[must_use]
pub fn default_source_dir() -> PathBuf {
    let cwd_relative = PathBuf::from("catalog/models");
    if cwd_relative.exists() {
        return cwd_relative;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("catalog/models")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bcode_model::{ModelCacheInfo, ModelCapability, ModelVisibility};
    use std::collections::BTreeSet;

    #[tokio::test]
    async fn resolver_identity_maps_region_prefixed_opus_five_to_stable_entry() {
        let resolver = ModelCatalogResolver::embedded();
        let identity = resolver
            .model_identity("bedrock", "us.anthropic.claude-opus-5-v1:0")
            .await
            .expect("Opus 5 identity");
        assert_eq!(identity.catalog_entry_id, "anthropic.claude-opus-5");
        assert_eq!(identity.family.as_deref(), Some("claude"));
        assert_eq!(
            identity.api_surface,
            Some(bcode_model::ModelApiSurface::Messages)
        );
    }

    /// Bedrock exposes `OpenAI` models both as bare ids and as per-region or global inference
    /// profiles (`us.`, `eu.`, `apac.`, `global.`). All of them are `OpenAI` models.
    fn is_bedrock_openai_model_id(model_id: &str) -> bool {
        ["", "us.", "eu.", "apac.", "global."].iter().any(|prefix| {
            model_id
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.starts_with("openai."))
        })
    }

    #[test]
    fn bedrock_openai_responses_models_resolve_with_region_prefixes() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog
            .provider("bedrock")
            .expect("bedrock provider exists");

        // Regional inference profiles with their own pricing are exact catalog entries and win
        // outright. Prefixes without a dedicated entry (`eu.`, `apac.`, and any profile of a model
        // that only has a bare entry) resolve through the bare entry's `*needle*` alias. Every
        // form must stay on the OpenAI Responses surface.
        for (model_id, expected_entry) in [
            ("openai.gpt-6-astra", "openai.gpt-6-astra"),
            ("us.openai.gpt-6-astra", "us.openai.gpt-6-astra"),
            ("global.openai.gpt-6-astra", "global.openai.gpt-6-astra"),
            ("eu.openai.gpt-6-astra", "openai.gpt-6-astra"),
            ("openai.gpt-5.6-sol", "openai.gpt-5.6-sol"),
            ("us.openai.gpt-5.6-sol", "us.openai.gpt-5.6-sol"),
            ("global.openai.gpt-5.6-sol", "global.openai.gpt-5.6-sol"),
            ("eu.openai.gpt-5.6-terra", "openai.gpt-5.6-terra"),
            ("apac.openai.gpt-5.6-luna", "openai.gpt-5.6-luna"),
            ("global.openai.gpt-5.5", "global.openai.gpt-5.5"),
            ("us.openai.gpt-5.4", "us.openai.gpt-5.4"),
            ("eu.openai.gpt-5.4", "openai.gpt-5.4"),
            ("us.openai.gpt-oss-120b", "openai.gpt-oss-120b"),
            ("us.openai.gpt-oss-20b", "openai.gpt-oss-20b"),
        ] {
            let entry = find_provider_model(provider, model_id)
                .unwrap_or_else(|| panic!("{model_id} should resolve"));
            assert_eq!(
                entry.model_id, expected_entry,
                "{model_id} resolved to the wrong entry"
            );
            assert_eq!(
                entry.api_surface,
                Some(bcode_model_catalog_models::CatalogApiSurface::Responses),
                "{model_id} must route through the Responses surface"
            );
        }
    }

    #[test]
    fn bedrock_openai_entries_beat_the_broad_claude_fallback() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog
            .provider("bedrock")
            .expect("bedrock provider exists");

        // The provider file carries a broad `*claude*` fallback. OpenAI ids must never land on it,
        // whether they resolve to a bare entry, a regional entry, or through an alias.
        for model_id in [
            "openai.gpt-6-astra",
            "openai.gpt-5.6-sol",
            "us.openai.gpt-5.5",
            "eu.openai.gpt-5.5",
            "openai.gpt-oss-safeguard-120b",
        ] {
            let entry = find_provider_model(provider, model_id)
                .unwrap_or_else(|| panic!("{model_id} should resolve"));
            assert!(
                is_bedrock_openai_model_id(&entry.model_id),
                "{model_id} resolved to non-OpenAI entry {}",
                entry.model_id
            );
        }
    }

    #[test]
    fn bedrock_gpt_56_catalog_declares_explicit_cache_capabilities() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog
            .provider("bedrock")
            .expect("bedrock provider exists");
        let entry = find_provider_model(provider, "openai.gpt-5.6-sol")
            .expect("GPT-5.6 catalog entry should resolve");
        assert!(entry.capabilities.prompt_cache);
        assert!(entry.capabilities.explicit_prompt_cache);
        let cache = cache_info_from_catalog(&entry.capabilities);
        assert!(
            cache
                .capabilities
                .contains(&ModelCacheCapability::ExplicitCachePoints)
        );
        assert!(
            !cache
                .capabilities
                .contains(&ModelCacheCapability::AutomaticPrefixCache)
        );
    }

    #[test]
    fn explicit_cache_capabilities_survive_target_enrichment() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let target = bcode_model_catalog_models::ModelSupportTarget::new(
            "bedrock",
            "bearer_token",
            "responses",
            Some("bcode"),
        );
        let discovered = bcode_model::ModelInfo {
            model_id: "us.openai.gpt-5.6-sol".to_string(),
            display_name: "live model".to_string(),
            is_default: true,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        };

        let resolved = catalog.merge_provider_models_for_target(
            "bedrock",
            vec![discovered],
            false,
            Some(&target),
        );
        let model = resolved.first().expect("selected model remains available");

        assert_eq!(model.model_id, "us.openai.gpt-5.6-sol");
        assert!(
            model
                .cache
                .capabilities
                .contains(&ModelCacheCapability::PromptCacheKey)
        );
        assert!(
            model
                .cache
                .capabilities
                .contains(&ModelCacheCapability::ExplicitCachePoints)
        );
        assert!(
            model
                .cache
                .capabilities
                .contains(&ModelCacheCapability::CacheUsageReporting)
        );
    }

    #[test]
    fn catalog_enrichment_extends_provider_cache_capabilities() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let model = bcode_model::ModelInfo {
            model_id: "us.openai.gpt-5.6-sol".to_string(),
            display_name: "live model".to_string(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo {
                capabilities: BTreeSet::from([ModelCacheCapability::CacheUsageReporting]),
                ..ModelCacheInfo::default()
            },
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        };

        let enriched = catalog.enrich_model("bedrock", model);
        assert!(
            enriched
                .cache
                .capabilities
                .contains(&ModelCacheCapability::ExplicitCachePoints)
        );
        assert!(
            enriched
                .cache
                .capabilities
                .contains(&ModelCacheCapability::PromptCacheKey)
        );
        assert!(
            enriched
                .cache
                .capabilities
                .contains(&ModelCacheCapability::CacheUsageReporting)
        );
    }

    #[test]
    fn bedrock_openai_models_carry_documented_limits() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog
            .provider("bedrock")
            .expect("bedrock provider exists");

        for (model_id, context_window, max_output_tokens) in [
            ("openai.gpt-6-astra", 1_000_000, 128_000),
            ("openai.gpt-5.6-sol", 1_000_000, 128_000),
            ("openai.gpt-5.6-terra", 1_000_000, 128_000),
            ("openai.gpt-5.6-luna", 1_000_000, 128_000),
            ("openai.gpt-5.5", 272_000, 100_000),
            ("openai.gpt-5.4", 272_000, 100_000),
            ("openai.gpt-oss-120b", 128_000, 16_000),
            ("openai.gpt-oss-20b", 128_000, 16_000),
        ] {
            let entry = provider
                .models
                .get(model_id)
                .unwrap_or_else(|| panic!("{model_id} should exist"));
            assert_eq!(entry.context_window, Some(context_window), "{model_id}");
            assert_eq!(
                entry.max_output_tokens,
                Some(max_output_tokens),
                "{model_id}"
            );
        }
    }

    #[test]
    fn fable_5_1_pricing_covers_only_published_bedrock_ids_and_cache_ttls() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let model_info = |model_id: &str| bcode_model::ModelInfo {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };

        for (model_id, billing_scope, expected_five_minute, expected_one_hour) in [
            // Bare id bills at the signing region's (us-east-1) Standard rate, same as `us.`.
            ("anthropic.claude-fable-5-1", "in_region", 135_850, 144_100),
            ("us.anthropic.claude-fable-5-1", "geo", 135_850, 144_100),
            (
                "global.anthropic.claude-fable-5-1",
                "global",
                123_500,
                131_000,
            ),
        ] {
            let model = catalog.enrich_model_with_defaults("bedrock", model_info(model_id));
            assert_eq!(model.display_name, "Claude Fable 5.1", "{model_id}");
            assert_eq!(
                model.api_surface,
                Some(bcode_model::ModelApiSurface::Messages)
            );
            let pricing = model
                .pricing
                .expect("published Fable ID should have pricing");
            for (ttl, expected_total) in [(300, expected_five_minute), (3_600, expected_one_hour)] {
                let usage = bcode_model::TokenUsage {
                    input_tokens: Some(11_000),
                    output_tokens: Some(1_000),
                    cached_input_tokens: Some(4_000),
                    cache_write_input_tokens: Some(1_000),
                    details: vec![
                        bcode_model::ModelTokenUsageDetail {
                            bucket: bcode_model::ModelPricingBucket::Input,
                            modality: bcode_model::ModelTokenModality::Text,
                            tokens: 6_000,
                            cache_ttl_seconds: None,
                        },
                        bcode_model::ModelTokenUsageDetail {
                            bucket: bcode_model::ModelPricingBucket::CacheReadInput,
                            modality: bcode_model::ModelTokenModality::Text,
                            tokens: 4_000,
                            cache_ttl_seconds: None,
                        },
                        bcode_model::ModelTokenUsageDetail {
                            bucket: bcode_model::ModelPricingBucket::CacheWriteInput,
                            modality: bcode_model::ModelTokenModality::Text,
                            tokens: 1_000,
                            cache_ttl_seconds: Some(ttl),
                        },
                        bcode_model::ModelTokenUsageDetail {
                            bucket: bcode_model::ModelPricingBucket::Output,
                            modality: bcode_model::ModelTokenModality::Text,
                            tokens: 1_000,
                            cache_ttl_seconds: None,
                        },
                    ]
                    .into_boxed_slice(),
                    pricing_context: Box::new(bcode_model::ModelPricingContext {
                        invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
                        billing_scope: Some(billing_scope.to_string()),
                        request_input_tokens: Some(11_000),
                        cache_ttl_seconds: Some(ttl),
                        ..bcode_model::ModelPricingContext::default()
                    }),
                    ..bcode_model::TokenUsage::default()
                };
                let estimate = pricing.estimate_cost(&usage).expect("priced usage");
                assert_eq!(
                    estimate.total_micros, expected_total,
                    "{model_id} ttl={ttl}"
                );
            }
        }

        for model_id in [
            "eu.anthropic.claude-fable-5-1",
            "apac.anthropic.claude-fable-5-1",
            "anthropic.claude-fable-5-1-preview",
            "anthropic.claude-fable-5-10",
        ] {
            let model = catalog.enrich_model_with_defaults("bedrock", model_info(model_id));
            assert!(model.pricing.is_none(), "{model_id} must fail closed");
        }
    }

    #[test]
    fn bedrock_openai_cache_write_usage_has_complete_pricing() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");

        for model_id in [
            "openai.gpt-6-astra",
            "us.openai.gpt-6-astra",
            "global.openai.gpt-6-astra",
            "openai.gpt-5.6-sol",
            "us.openai.gpt-5.6-sol",
            "global.openai.gpt-5.6-sol",
            "openai.gpt-5.6-terra",
            "us.openai.gpt-5.6-terra",
            "global.openai.gpt-5.6-terra",
            "openai.gpt-5.6-luna",
            "us.openai.gpt-5.6-luna",
            "global.openai.gpt-5.6-luna",
        ] {
            let model = catalog.enrich_model_with_defaults(
                "bedrock",
                bcode_model::ModelInfo {
                    model_id: model_id.to_string(),
                    display_name: model_id.to_string(),
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
                    visibility: bcode_model::ModelVisibility::Visible,
                },
            );
            let pricing = model
                .pricing
                .unwrap_or_else(|| panic!("{model_id} should have pricing"));
            let usage = bcode_model::TokenUsage {
                input_tokens: Some(100),
                output_tokens: Some(10),
                cached_input_tokens: Some(0),
                cache_write_input_tokens: Some(90),
                ..bcode_model::TokenUsage::default()
            };
            assert!(
                pricing.estimate_cost(&usage).is_some(),
                "{model_id} should price cache-write usage"
            );
        }
    }

    #[test]
    fn bedrock_openai_safeguard_models_do_not_claim_the_responses_surface() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog
            .provider("bedrock")
            .expect("bedrock provider exists");

        // AWS model cards report `Responses: No` for the Safeguard variants; they are reachable
        // over Converse instead, so they must not be pinned to the Responses surface.
        for model_id in [
            "openai.gpt-oss-safeguard-120b",
            "openai.gpt-oss-safeguard-20b",
        ] {
            let entry = provider
                .models
                .get(model_id)
                .unwrap_or_else(|| panic!("{model_id} should exist"));
            assert_eq!(entry.api_surface, None, "{model_id}");
        }
    }

    #[test]
    fn non_matching_target_preserves_documented_entry_limits() {
        // Regression guard: enriching through a target a model does not declare must not erase the
        // entry's own documented limits. 146 of 157 Bedrock entries declare neither `deployments`
        // nor `supported_by`, so a target-aware merge that only consults target-specific values
        // silently dropped every Claude/Nova/Llama context window and broke context display.
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let responses_target = bcode_model_catalog_models::ModelSupportTarget::new(
            "bedrock",
            "bearer_token",
            "responses",
            Some("bcode"),
        );

        let discovered = vec![ModelInfo {
            model_id: "us.anthropic.claude-sonnet-5-20250929-v1:0".to_string(),
            display_name: "Claude Sonnet 5".to_string(),
            is_default: true,
            context_window: None,
            max_output_tokens: None,
            capabilities: std::collections::BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
            max_image_input_base64_bytes: None,
        }];

        let merged = catalog.merge_provider_models_for_target(
            "bedrock",
            discovered,
            false,
            Some(&responses_target),
        );

        let claude = merged
            .first()
            .expect("the discovered Claude model is preserved");
        assert_eq!(
            claude.context_window,
            Some(1_000_000),
            "a Claude model must keep its documented context window when enriched through a \
             target it does not declare"
        );
        assert_eq!(claude.max_output_tokens, Some(128_000));
        // The Messages surface must still be reported so routing stays correct.
        assert_eq!(
            claude.api_surface,
            Some(bcode_model::ModelApiSurface::Messages)
        );
    }

    #[tokio::test]
    async fn selected_fable_missing_from_active_provider_view_is_catalog_enriched() {
        let resolver = ModelCatalogResolver::embedded();
        let model_list = resolver
            .resolve_selection(
                bcode_model::ModelList {
                    models: Vec::new(),
                    catalog: bcode_model::ModelCatalogHints {
                        policy: bcode_model::ModelCatalogPolicy::ExpandSupported {
                            provider_id: "bedrock".to_string(),
                            target: bcode_model::ModelCatalogSupportHint {
                                provider: "bedrock".to_string(),
                                auth_mode: "bearer_token".to_string(),
                                api_surface: "responses".to_string(),
                                integration: Some("bcode".to_string()),
                            },
                            authority: bcode_model::ModelListAuthority::Authoritative,
                        },
                    },
                },
                Some("global.anthropic.claude-fable-5-1"),
                None,
            )
            .await;
        let fable = model_list
            .models
            .iter()
            .find(|model| model.model_id == "global.anthropic.claude-fable-5-1")
            .expect("selected Fable must remain present");

        assert_eq!(fable.display_name, "Claude Fable 5.1");
        assert_eq!(fable.context_window, Some(1_000_000));
        assert_eq!(fable.max_output_tokens, Some(128_000));
        assert_eq!(
            fable.api_surface,
            Some(bcode_model::ModelApiSurface::Messages)
        );
    }

    #[test]
    fn mantle_openai_resolution_expands_the_picker_beyond_the_configured_model() {
        // End-to-end guard for the reported bug: the plugin reports only the configured id, and the
        // resolver must expand that into the full Responses model set for the picker.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let options = RemoteCatalogOptions {
                disabled: true,
                ..RemoteCatalogOptions::default()
            };
            let resolver = ModelCatalogResolver::new(options).expect("resolver");
            let target = bcode_model::ModelCatalogSupportHint {
                provider: "bedrock".to_string(),
                auth_mode: "bearer_token".to_string(),
                api_surface: "responses".to_string(),
                integration: Some("bcode".to_string()),
            };

            let picker = resolver
                .resolve_selection(
                    bcode_model::ModelList {
                        models: vec![ModelInfo {
                            model_id: "openai.gpt-5.6-sol".to_string(),
                            display_name: "openai.gpt-5.6-sol".to_string(),
                            is_default: true,
                            context_window: None,
                            max_output_tokens: None,
                            max_image_input_base64_bytes: None,
                            capabilities: std::collections::BTreeSet::new(),
                            feature_support: bcode_model::ModelFeatureSupport::default(),
                            reasoning: None,
                            cache: ModelCacheInfo::default(),
                            metadata_source: None,
                            pricing: None,
                            api_surface: None,
                            visibility: ModelVisibility::Visible,
                        }],
                        catalog: bcode_model::ModelCatalogHints {
                            policy: bcode_model::ModelCatalogPolicy::ExpandSupported {
                                provider_id: "bedrock".to_string(),
                                target,
                                authority: bcode_model::ModelListAuthority::Authoritative,
                            },
                        },
                    },
                    None,
                    Some("openai.gpt-5.6-sol"),
                )
                .await;

            let ids = picker
                .models
                .iter()
                .map(|model| model.model_id.as_str())
                .collect::<std::collections::BTreeSet<_>>();
            for expected in [
                "openai.gpt-5.6-sol",
                "openai.gpt-5.6-terra",
                "openai.gpt-5.6-luna",
                "openai.gpt-5.5",
                "openai.gpt-5.4",
                "openai.gpt-oss-120b",
                "openai.gpt-oss-20b",
            ] {
                assert!(
                    ids.contains(expected),
                    "{expected} must be in the picker; got {ids:?}"
                );
            }

            // Enrichment must still apply to the configured model, and it stays the default.
            let sol = picker
                .models
                .iter()
                .find(|model| model.model_id == "openai.gpt-5.6-sol")
                .expect("Sol is present");
            assert_eq!(sol.context_window, Some(1_000_000));
            assert_eq!(
                sol.api_surface,
                Some(bcode_model::ModelApiSurface::Responses)
            );
            assert!(sol.is_default);
        });
    }

    #[test]
    fn mantle_openai_picker_expands_to_every_catalog_responses_model() {
        // Regression guard for the reported bug: the OpenAI Responses models exist only on Mantle,
        // so `ListFoundationModels` never returns them. Without catalog expansion the picker showed
        // only the single configured model (or, on the default transport, just the dual-surface
        // `gpt-oss` models that Converse does list).
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let target = bcode_model_catalog_models::ModelSupportTarget::new(
            "bedrock",
            "bearer_token",
            "responses",
            Some("bcode"),
        );

        let models = catalog.provider_models_for_support_target("bedrock", &target, false);
        let ids = models
            .iter()
            .map(|model| model.model_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();

        for expected in [
            "openai.gpt-5.6-sol",
            "openai.gpt-5.6-terra",
            "openai.gpt-5.6-luna",
            "openai.gpt-5.5",
            "openai.gpt-5.4",
            "openai.gpt-oss-120b",
            "openai.gpt-oss-20b",
        ] {
            assert!(
                ids.contains(expected),
                "{expected} must be reachable through the Mantle OpenAI support target; got {ids:?}"
            );
        }

        // The Converse-only Safeguard variants must not be advertised on this surface.
        for excluded in [
            "openai.gpt-oss-safeguard-120b",
            "openai.gpt-oss-safeguard-20b",
        ] {
            assert!(
                !ids.contains(excluded),
                "{excluded} reports `Responses: No` and must not appear on the Mantle OpenAI surface"
            );
        }

        // Claude entries must not leak onto the OpenAI surface either.
        assert!(
            ids.iter().all(|id| is_bedrock_openai_model_id(id)),
            "only OpenAI models declare the Mantle OpenAI target; got {ids:?}"
        );
    }

    #[test]
    fn catalog_loads_provider_error_handling_metadata() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog.provider("openai").expect("openai provider exists");

        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.upstream-retry-buffer-limit"
                        && pattern.scope.provider_plugin_id.as_deref()
                            == Some("bcode.openai-compatible")
                        && pattern.r#match.code.as_deref() == Some("http_507")
                        && pattern.r#match.message_contains.as_deref()
                            == Some("exceeded request buffer limit while retrying upstream")
                })
        );
        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.no-biscuit-no-service"
                        && pattern.scope.provider_plugin_id.as_deref()
                            == Some("bcode.openai-compatible")
                        && pattern.r#match.code.as_deref() == Some("responses_stream_failed")
                        && pattern.r#match.message_contains.as_deref()
                            == Some("no_biscuit_no_service")
                })
        );
        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.stream-read-decode-failed"
                        && pattern.scope.provider_plugin_id.as_deref()
                            == Some("bcode.openai-compatible")
                        && pattern.r#match.code.as_deref() == Some("stream_read_failed")
                        && pattern.r#match.message_contains.as_deref()
                            == Some("error decoding response body")
                })
        );
        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.unsupported-content-type"
                        && pattern.scope.provider_plugin_id.as_deref()
                            == Some("bcode.openai-compatible")
                        && pattern.r#match.code.as_deref() == Some("http_400")
                })
        );
        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.server-error"
                        && pattern.scope.provider_plugin_id.as_deref()
                            == Some("bcode.openai-compatible")
                        && pattern.r#match.code.as_deref() == Some("server_error")
                })
        );
        assert!(
            provider
                .error_handling
                .recoverable_error_patterns
                .iter()
                .any(|pattern| {
                    pattern.id == "bcode.openai-compatible.server-overloaded"
                        && pattern.r#match.category.as_deref() == Some("overloaded")
                        && pattern.r#match.code.as_deref() == Some("server_is_overloaded")
                })
        );
    }

    #[test]
    fn catalog_enriches_exact_model_metadata_and_pricing() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let model = ModelInfo {
            model_id: "gpt-4o".to_string(),
            display_name: "gpt-4o".to_string(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::default(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        };

        let enriched = catalog.enrich_model("openai", model);

        assert_eq!(enriched.display_name, "GPT-4o");
        assert_eq!(enriched.context_window, Some(128_000));
        assert_eq!(enriched.max_output_tokens, Some(16_384));
        assert_eq!(
            enriched.metadata_source,
            Some(ModelMetadataSource::BundledCatalog)
        );
        assert_eq!(
            enriched
                .pricing
                .and_then(|pricing| pricing.input)
                .map(|price| price.micros),
            Some(2_500_000)
        );
        assert!(enriched.capabilities.contains(&ModelCapability::ToolCalls));
        assert!(
            enriched
                .capabilities
                .contains(&ModelCapability::ParallelToolCalls)
        );
        assert_eq!(
            enriched
                .feature_support
                .tool_choice
                .get(&ToolChoiceMode::Parallel),
            Some(&CapabilitySupport::supported(
                CapabilitySource::BundledCatalog
            ))
        );
    }

    #[test]
    fn unknown_model_is_not_upgraded_to_parallel_tool_calls() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let model = ModelInfo {
            model_id: "custom-proxy-model".to_owned(),
            display_name: "custom-proxy-model".to_owned(),
            is_default: false,
            context_window: None,
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::from([ModelCapability::ToolCalls]),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        };

        let enriched = catalog.enrich_model("openai", model);
        assert!(
            !enriched
                .capabilities
                .contains(&ModelCapability::ParallelToolCalls)
        );
        assert_eq!(
            enriched
                .feature_support
                .tool_choice
                .get(&ToolChoiceMode::Parallel),
            None
        );
    }

    #[test]
    fn bundled_catalog_includes_gpt_5_6_models() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");

        for model_id in ["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert!(
                catalog.model("openai", model_id).is_some(),
                "{model_id} should be in the embedded OpenAI catalog"
            );
        }
    }

    #[test]
    fn bundled_catalog_includes_gpt_6_astra_as_codex_default() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog.provider("openai").expect("openai provider exists");

        let entry = catalog
            .model("openai", "gpt-6-astra")
            .expect("gpt-6-astra should be in the embedded OpenAI catalog");
        assert_eq!(entry.context_window, Some(1_050_000));
        assert_eq!(entry.max_output_tokens, Some(128_000));
        assert_eq!(
            entry
                .reasoning
                .as_ref()
                .map(|reasoning| reasoning.effort_values.clone()),
            Some(
                ["low", "medium", "high", "xhigh", "max"]
                    .into_iter()
                    .map(str::to_owned)
                    .collect::<BTreeSet<_>>()
            ),
            "Astra does not advertise a `none` effort"
        );
        assert_eq!(
            provider.default_codex_model_id.as_deref(),
            Some("gpt-6-astra")
        );

        // The `gpt-6` alias resolves to Astra, and both deployments carry the full window.
        let aliased = catalog
            .model("openai", "gpt-6")
            .expect("gpt-6 alias should resolve");
        assert_eq!(aliased.model_id, "gpt-6-astra");
        assert_eq!(entry.deployments.len(), 2);
        for deployment in &entry.deployments {
            assert_eq!(deployment.context_window, Some(1_050_000));
            assert_eq!(deployment.max_output_tokens, Some(128_000));
        }
    }

    #[test]
    fn openai_gpt_6_astra_uses_catalog_owned_long_context_rules() {
        let catalog = ModelCatalog::load_bundled().expect("bundled catalog");
        let model = catalog
            .provider_models_as_model_info("openai")
            .into_iter()
            .find(|model| model.model_id == "gpt-6-astra")
            .expect("GPT-6 Astra");
        let pricing = model.pricing.expect("catalog pricing");
        let usage = bcode_model::TokenUsage {
            input_tokens: Some(300_000),
            output_tokens: Some(10_000),
            pricing_context: Box::new(bcode_model::ModelPricingContext {
                request_input_tokens: Some(300_000),
                invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
                ..bcode_model::ModelPricingContext::default()
            }),
            ..bcode_model::TokenUsage::default()
        };
        let estimate = pricing
            .estimate_cost(&usage)
            .expect("long-context estimate");
        // 300K input at 2x ($20/M) plus 10K output at 1.5x ($75/M).
        assert_eq!(estimate.total_micros, 6_750_000);
        assert_eq!(pricing.rules.len(), 8);
    }

    #[test]
    fn openai_gpt_5_6_uses_catalog_owned_long_context_rules() {
        let catalog = ModelCatalog::load_bundled().expect("bundled catalog");
        let model = catalog
            .provider_models_as_model_info("openai")
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-terra")
            .expect("GPT-5.6 Terra");
        let pricing = model.pricing.expect("catalog pricing");
        let usage = bcode_model::TokenUsage {
            input_tokens: Some(300_000),
            output_tokens: Some(10_000),
            pricing_context: Box::new(bcode_model::ModelPricingContext {
                request_input_tokens: Some(300_000),
                invocation_class: Some(bcode_model::ModelInvocationClass::OnDemand),
                ..bcode_model::ModelPricingContext::default()
            }),
            ..bcode_model::TokenUsage::default()
        };
        let estimate = pricing
            .estimate_cost(&usage)
            .expect("long-context estimate");
        assert_eq!(estimate.total_micros, 1_380_000);
        assert_eq!(pricing.rules.len(), 8);
    }

    #[test]
    fn openai_fallback_prefers_gpt_6_astra_then_gpt_5_6_sol_then_terra() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog.provider("openai").expect("openai provider exists");

        assert_eq!(
            provider
                .fallback_model_ids
                .iter()
                .take(3)
                .collect::<Vec<_>>(),
            vec!["gpt-6-astra", "gpt-5.6-sol", "gpt-5.6-terra"]
        );
    }

    #[tokio::test]
    async fn bounded_output_limit_lookup_resolves_fable_inference_profile_alias() {
        let resolver = ModelCatalogResolver::embedded();
        assert_eq!(
            resolver
                .model_max_output_tokens("bedrock", "global.anthropic.claude-fable-5-1")
                .await,
            Some(128_000)
        );
    }

    #[test]
    fn remote_profile_id_uses_most_specific_embedded_model_alias() {
        let mut local = load_embedded_catalog().expect("embedded catalog should load");
        let mut remote = local.clone();
        remote
            .providers
            .retain(|provider_id, _| provider_id == "bedrock");
        let mut stale_entry = local.providers["bedrock"].models["anthropic.claude-fable-5"].clone();
        stale_entry.model_id = "global.anthropic.claude-fable-5-1".to_string();
        stale_entry.display_name = "stale broad Fable metadata".to_string();
        let remote_provider = remote
            .providers
            .get_mut("bedrock")
            .expect("remote Bedrock catalog");
        remote_provider.models.clear();
        remote_provider.models.insert(
            "global.anthropic.claude-fable-5-1".to_string(),
            stale_entry.clone(),
        );
        let mut stale_canonical = stale_entry;
        stale_canonical.model_id = "anthropic.claude-fable-5-1".to_string();
        stale_canonical.aliases.clear();
        stale_canonical.capabilities = CatalogCapabilities::default();
        stale_canonical.reasoning = None;
        remote_provider
            .models
            .insert("anthropic.claude-fable-5-1".to_string(), stale_canonical);

        overlay_remote_catalog(&mut local, &remote);
        let provider = &local.providers["bedrock"];
        assert!(
            !provider
                .models
                .contains_key("global.anthropic.claude-fable-5-1")
        );
        let catalog = ModelCatalog::new(local);
        let resolved = catalog
            .model("bedrock", "global.anthropic.claude-fable-5-1")
            .expect("profile id should resolve");
        assert_eq!(resolved.model_id, "anthropic.claude-fable-5-1");
        assert_eq!(
            resolved.capabilities.prompt_cache_ttl_seconds,
            BTreeSet::from([300, 3_600])
        );
        assert!(
            resolved
                .reasoning
                .as_ref()
                .is_some_and(|reasoning| reasoning.effort_values.contains("max"))
        );
    }

    #[test]
    fn gpt_5_6_sol_resolves_operational_limits_for_chatgpt_codex() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let target = ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        );
        let model = catalog
            .provider_models_for_support_target("openai", &target, false)
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("Sol should support ChatGPT Codex");

        assert_eq!(model.context_window, Some(372_000));
        assert_eq!(model.max_output_tokens, Some(128_000));

        let public_target =
            ModelSupportTarget::new("openai", "api_key", "responses_api", Some("bcode"));
        let public_model = catalog
            .provider_models_for_support_target("openai", &public_target, false)
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("Sol should support the public Responses API");
        assert_eq!(public_model.context_window, Some(1_050_000));
    }

    #[test]
    fn gpt_5_6_sol_uses_exact_metadata_not_broad_gpt_5_alias() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let entry = catalog
            .model("openai", "gpt-5.6-sol")
            .expect("gpt-5.6-sol should resolve exactly");

        assert_eq!(entry.model_id, "gpt-5.6-sol");
        assert_eq!(entry.context_window, Some(1_050_000));
        assert_eq!(entry.max_output_tokens, Some(128_000));
        assert_eq!(entry.deployments.len(), 2);
        assert!(
            entry
                .deployments
                .iter()
                .any(|deployment| deployment.context_window == Some(372_000))
        );
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.input_micros),
            Some(4_000_000)
        );
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.output_micros),
            Some(20_000_000)
        );
        assert!(
            entry
                .reasoning
                .as_ref()
                .is_some_and(|reasoning| reasoning.effort_values.contains("max"))
        );
    }

    #[test]
    fn bundled_catalog_ignores_stale_cwd_catalog() {
        let original_cwd = std::env::current_dir().expect("cwd should be available");
        let temp_dir =
            std::env::temp_dir().join(format!("bcode-stale-catalog-test-{}", std::process::id()));
        let catalog_dir = temp_dir.join("catalog/models/providers");
        std::fs::create_dir_all(&catalog_dir).expect("temp catalog dir should be created");
        std::fs::write(
            catalog_dir.join("openai.toml"),
            r#"
provider_id = "openai"
display_name = "Stale OpenAI"
kind = "open_ai_compatible"
fallback_model_ids = ["stale-model"]

[models."stale-model"]
model_id = "stale-model"
display_name = "Stale Model"
status = "stable"
"#,
        )
        .expect("stale catalog should be written");

        std::env::set_current_dir(&temp_dir).expect("cwd should switch to temp dir");
        let catalog = ModelCatalog::load_bundled().expect("embedded catalog should load");
        std::env::set_current_dir(original_cwd).expect("cwd should be restored");
        let _ = std::fs::remove_dir_all(&temp_dir);

        assert!(catalog.model("openai", "gpt-5.6-sol").is_some());
        assert!(catalog.model("openai", "stale-model").is_none());
    }

    #[test]
    fn catalog_alias_prefixes_match_model_variants() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let canonical = catalog
            .enrich_model("openai", test_model_info("gpt-4o"))
            .pricing
            .expect("canonical pricing");
        let aliased = catalog
            .enrich_model("openai", test_model_info("gpt-4o-2024-08-06"))
            .pricing
            .expect("alias pricing");
        let entry = catalog
            .model("openai", "gpt-4o-2024-08-06")
            .expect("alias should resolve");

        assert_eq!(entry.model_id, "gpt-4o");
        assert_eq!(aliased, canonical);
    }

    #[test]
    fn bedrock_region_prefixed_ids_resolve_to_the_same_catalog_price() {
        let mut document = ModelCatalog::load_bundled()
            .expect("catalog should load")
            .document()
            .clone();
        let provider = document.providers.get_mut("bedrock").expect("bedrock");
        let entry = provider
            .models
            .get_mut("anthropic.claude-sonnet-4")
            .expect("sonnet family");
        entry.pricing = Some(CatalogPricing {
            currency: "USD".to_string(),
            unit: bcode_model_catalog_models::CatalogPricingUnit::PerMillionTokens,
            input_micros: Some(3_000_000),
            cached_input_micros: None,
            cache_write_input_micros: None,
            output_micros: Some(15_000_000),
            context_threshold_tokens: None,
            revision: Some("test-revision".to_string()),
            rules: Vec::new(),
        });
        let catalog = ModelCatalog::new(document);

        let canonical = catalog
            .enrich_model(
                "bedrock",
                test_model_info("anthropic.claude-sonnet-4-20250514-v1:0"),
            )
            .pricing
            .expect("canonical pricing");
        let regional = catalog
            .enrich_model(
                "bedrock",
                test_model_info("us.anthropic.claude-sonnet-4-20250514-v1:0"),
            )
            .pricing
            .expect("regional pricing");

        assert_eq!(regional, canonical);
    }

    fn test_model_info(model_id: &str) -> bcode_model::ModelInfo {
        bcode_model::ModelInfo {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        }
    }

    #[test]
    fn bedrock_live_inference_profile_enriches_to_reasoning_family() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");

        // A live-discovered cross-region inference-profile ID (as returned by
        // `ListInferenceProfiles`) must enrich from the tier-specific Claude Sonnet glob, not
        // just the broad `*claude*` fallback, and gain reasoning metadata + a 200k window.
        let discovered = bcode_model::ModelInfo {
            model_id: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
            display_name: "us.anthropic.claude-sonnet-4-5-20250929-v1:0".to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };

        let enriched = catalog.enrich_model("bedrock", discovered);

        assert_eq!(enriched.context_window, Some(200_000));
        let reasoning = enriched
            .reasoning
            .expect("modern Claude tier should carry reasoning metadata");
        assert!(reasoning.effort_values.contains(&"medium".to_string()));
        assert!(reasoning.raw_reasoning_supported);
    }

    #[test]
    fn bedrock_legacy_claude_3_has_no_reasoning() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");

        // Claude 3 Haiku does not support extended thinking; it should match the broad
        // `*claude*` fallback (no reasoning) rather than a modern reasoning tier.
        let discovered = bcode_model::ModelInfo {
            model_id: "anthropic.claude-3-haiku-20240307-v1:0".to_string(),
            display_name: "anthropic.claude-3-haiku-20240307-v1:0".to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };

        let enriched = catalog.enrich_model("bedrock", discovered);

        assert_eq!(enriched.context_window, Some(200_000));
        assert!(enriched.reasoning.is_none());
    }

    #[test]
    fn bedrock_fable_5_1_uses_its_specific_capabilities() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let discovered_model = |model_id: &str| bcode_model::ModelInfo {
            model_id: model_id.to_string(),
            display_name: model_id.to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };
        for model_id in [
            "anthropic.claude-fable-5-1",
            "us.anthropic.claude-fable-5-1",
            "global.anthropic.claude-fable-5-1",
        ] {
            let mut discovered = discovered_model(model_id);
            discovered.reasoning = Some(bcode_model::ModelReasoningInfo {
                control: Some(bcode_model::ReasoningControl::Adaptive),
                effort_values: vec![
                    "low".to_string(),
                    "medium".to_string(),
                    "high".to_string(),
                    "xhigh".to_string(),
                ],
                source: bcode_model::ModelReasoningCapabilitySource::KnownModelTable,
                ..Default::default()
            });
            let enriched = catalog.enrich_model("bedrock", discovered);
            assert_eq!(enriched.context_window, Some(1_000_000));
            assert_eq!(enriched.max_output_tokens, Some(128_000));
            assert_eq!(
                enriched.api_surface,
                Some(bcode_model::ModelApiSurface::Messages)
            );
            let reasoning = enriched.reasoning.expect("reasoning metadata");
            assert!(reasoning.effort_values.iter().any(|value| value == "max"));
            assert_eq!(reasoning.default_effort.as_deref(), Some("high"));
            assert_eq!(enriched.cache.ttl_seconds, BTreeSet::from([300, 3_600]));
            assert!(matches!(
                enriched
                    .feature_support
                    .tool_choice(bcode_model::ToolChoiceMode::Required),
                bcode_model::CapabilitySupport::Unsupported { .. }
            ));
            assert!(matches!(
                enriched
                    .feature_support
                    .tool_choice(bcode_model::ToolChoiceMode::Named),
                bcode_model::CapabilitySupport::Unsupported { .. }
            ));
            assert!(
                enriched
                    .feature_support
                    .prompt_cache(bcode_model::PromptCacheFeature::Ttl)
                    .is_guaranteed()
            );
            // A loop evaluation node requests strict JSON-schema output; the model-side claim must
            // be present so negotiation with the Bedrock Messages adapter can succeed.
            for mode in [
                bcode_model::StructuredOutputMode::JsonSchema,
                bcode_model::StructuredOutputMode::StrictJsonSchema,
            ] {
                assert!(
                    enriched
                        .feature_support
                        .structured_output(mode)
                        .is_guaranteed(),
                    "{model_id} must declare structured output for {mode:?}"
                );
            }
        }
    }

    #[test]
    fn bedrock_messages_api_only_models_route_to_messages_surface() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");

        for model_id in [
            "global.anthropic.claude-opus-5",
            "global.anthropic.claude-fable-5",
            "global.anthropic.claude-fable-5-1",
            "us.anthropic.claude-opus-4-7-20260416-v1:0",
            "us.anthropic.claude-sonnet-5-20260101-v1:0",
            "anthropic.claude-mythos-5",
        ] {
            let discovered = bcode_model::ModelInfo {
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
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
                visibility: bcode_model::ModelVisibility::Visible,
            };
            let enriched = catalog.enrich_model("bedrock", discovered);
            assert_eq!(
                enriched.visibility,
                bcode_model::ModelVisibility::Visible,
                "{model_id} is now supported via the Messages surface"
            );
            assert_eq!(
                enriched.api_surface,
                Some(bcode_model::ModelApiSurface::Messages),
                "{model_id} must route to the Messages API surface"
            );
        }
    }

    #[test]
    fn bedrock_converse_models_remain_visible() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        for model_id in [
            "us.anthropic.claude-sonnet-4-5-20250929-v1:0",
            "us.anthropic.claude-opus-4-1-20250805-v1:0",
            "anthropic.claude-3-7-sonnet-20250219-v1:0",
        ] {
            let discovered = bcode_model::ModelInfo {
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
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
                visibility: bcode_model::ModelVisibility::Visible,
            };
            let enriched = catalog.enrich_model("bedrock", discovered);
            assert_eq!(
                enriched.visibility,
                bcode_model::ModelVisibility::Visible,
                "{model_id} must remain visible (Converse-compatible)"
            );
        }
    }

    #[test]
    fn catalog_structured_output_claims_are_explicit_and_disabled_defaults_stay_unknown() {
        let structured = CatalogCapabilities {
            structured_outputs: true,
            ..CatalogCapabilities::default()
        };
        let structured_support =
            feature_support_from_catalog(&structured, CapabilitySource::BundledCatalog);
        assert!(
            structured_support
                .structured_output(StructuredOutputMode::JsonSchema)
                .is_guaranteed()
        );
        assert!(
            structured_support
                .structured_output(StructuredOutputMode::StrictJsonSchema)
                .is_guaranteed()
        );

        assert!(matches!(
            structured_support.tool_schema(ToolSchemaMode::Strict),
            CapabilitySupport::Unknown
        ));

        let tools = CatalogCapabilities {
            tool_use: true,
            ..CatalogCapabilities::default()
        };
        let tool_support = feature_support_from_catalog(&tools, CapabilitySource::BundledCatalog);
        assert!(
            tool_support
                .tool_schema(ToolSchemaMode::Strict)
                .is_guaranteed()
        );

        let disabled = CatalogCapabilities::default();
        let disabled_support =
            feature_support_from_catalog(&disabled, CapabilitySource::BundledCatalog);
        assert!(matches!(
            disabled_support.structured_output(StructuredOutputMode::StrictJsonSchema),
            CapabilitySupport::Unknown
        ));
    }

    #[test]
    fn catalog_image_claims_are_explicit_and_non_vision_defaults_stay_unknown() {
        let vision = CatalogCapabilities {
            image_input: true,
            ..CatalogCapabilities::default()
        };
        let vision_support =
            feature_support_from_catalog(&vision, CapabilitySource::BundledCatalog);
        assert!(
            vision_support
                .media_input(MediaInputFeature::ToolResultImage)
                .is_guaranteed()
        );
        assert!(capabilities_from_catalog(&vision).contains(&ModelCapability::ImageInput));

        let text_only = CatalogCapabilities::default();
        let text_support =
            feature_support_from_catalog(&text_only, CapabilitySource::BundledCatalog);
        assert!(matches!(
            text_support.media_input(MediaInputFeature::ToolResultImage),
            CapabilitySupport::Unknown
        ));
        assert!(!capabilities_from_catalog(&text_only).contains(&ModelCapability::ImageInput));
    }

    #[test]
    fn bedrock_opus_5_uses_messages_with_adaptive_reasoning() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let discovered = bcode_model::ModelInfo {
            model_id: "global.anthropic.claude-opus-5".to_string(),
            display_name: "global.anthropic.claude-opus-5".to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };
        let enriched = catalog.enrich_model("bedrock", discovered);
        assert!(enriched.capabilities.contains(&ModelCapability::ImageInput));
        assert!(
            enriched
                .feature_support
                .media_input(MediaInputFeature::UserImage)
                .is_guaranteed()
        );
        assert!(
            enriched
                .feature_support
                .media_input(MediaInputFeature::ToolResultImage)
                .is_guaranteed()
        );
        assert_eq!(enriched.max_image_input_base64_bytes, Some(5_242_880));
        assert_eq!(
            enriched.api_surface,
            Some(bcode_model::ModelApiSurface::Messages),
            "Opus 5 must route through the Anthropic Messages adapter"
        );
        let reasoning = enriched
            .reasoning
            .expect("Opus 5 must advertise selectable thinking levels");
        assert_eq!(
            reasoning.control,
            Some(bcode_model::ReasoningControl::Adaptive),
            "Opus 5 rejects explicit thinking budgets"
        );
        assert!(
            reasoning.effort_values.iter().any(|value| value == "xhigh")
                && reasoning.effort_values.iter().any(|value| value == "max"),
            "Opus 5 accepts the extended effort levels: {:?}",
            reasoning.effort_values
        );
        assert_eq!(reasoning.default_effort.as_deref(), Some("high"));
    }

    #[test]
    fn bedrock_global_prefixed_opus_5_resolves_messages_surface_and_adaptive_control() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let discovered = bcode_model::ModelInfo {
            model_id: "global.anthropic.claude-opus-5".to_string(),
            display_name: "opus 5".to_string(),
            is_default: true,
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
            visibility: bcode_model::ModelVisibility::Visible,
        };
        // Mirrors the provider's `EnrichOnly { target: None }` hint, which routes through the
        // non-target enrichment path used for explicitly configured models.
        let merged =
            catalog.merge_provider_models_for_target("bedrock", vec![discovered], false, None);
        let enriched = merged.first().expect("model should survive enrichment");
        assert_eq!(
            enriched.api_surface,
            Some(bcode_model::ModelApiSurface::Messages),
            "Opus 5 must route to the Messages adapter, not Converse"
        );
        let reasoning = enriched
            .reasoning
            .as_ref()
            .expect("global-prefixed Opus 5 must advertise reasoning");
        assert_eq!(
            reasoning.control,
            Some(bcode_model::ReasoningControl::Adaptive)
        );
    }

    #[test]
    fn bedrock_target_enrichment_preserves_image_contract_through_remote_metadata() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let discovered = bcode_model::ModelInfo {
            model_id: "global.anthropic.claude-opus-5".to_owned(),
            display_name: "remote Opus 5".to_owned(),
            is_default: true,
            context_window: Some(1_000_000),
            max_output_tokens: Some(128_000),
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::from([ModelCapability::ImageInput]),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: bcode_model::ModelCacheInfo::default(),
            metadata_source: Some(ModelMetadataSource::RemoteCatalog),
            pricing: None,
            api_surface: None,
            visibility: bcode_model::ModelVisibility::Visible,
        };
        let target =
            ModelSupportTarget::new("bedrock", "aws_default_chain", "messages", None::<String>);

        let merged = catalog.merge_provider_models_for_target(
            "bedrock",
            vec![discovered],
            false,
            Some(&target),
        );
        let opus = merged.first().expect("Opus 5 survives target enrichment");
        assert_eq!(opus.max_image_input_base64_bytes, Some(5_242_880));
        assert!(opus.capabilities.contains(&ModelCapability::ImageInput));
        assert!(
            opus.feature_support
                .media_input(MediaInputFeature::ToolResultImage)
                .is_guaranteed()
        );
        assert!(
            opus.feature_support
                .media_input(MediaInputFeature::UserImage)
                .is_guaranteed()
        );
    }

    #[test]
    fn adaptive_thinking_mode_survives_without_a_reasoning_table() {
        // A `thinking_mode = "adaptive"` entry that omits the optional `[reasoning]` table must
        // still advertise adaptive control, otherwise the provider falls back to
        // `thinking.type = "enabled"` with a budget, which adaptive models reject with a
        // ValidationException. Exercised directly so the guarantee holds regardless of which
        // bundled entries currently declare an effort table.
        let reasoning = reasoning_from_catalog_parts(
            None,
            Some(bcode_model_catalog_models::CatalogThinkingMode::Adaptive),
        )
        .expect("an adaptive thinking mode must advertise reasoning on its own");
        assert_eq!(
            reasoning.control,
            Some(bcode_model::ReasoningControl::Adaptive)
        );
        assert!(
            reasoning.effort_values.is_empty(),
            "no effort table means no advertised effort values"
        );
    }

    #[test]
    fn bedrock_adaptive_models_advertise_adaptive_control_and_effort_values() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        // Adaptive Claude generations must advertise both adaptive control and effort values.
        // Without effort values the host cannot resolve a supported effort, so no
        // `output_config.effort` is sent and the model chooses an unrequested depth.
        for (model_id, expected_xhigh) in [
            ("us.anthropic.claude-opus-4-7-20250101-v1:0", true),
            ("us.anthropic.claude-sonnet-5-20250101-v1:0", true),
            ("us.anthropic.claude-fable-5-20250101-v1:0", true),
            ("global.anthropic.claude-fable-5-1", true),
            ("us.anthropic.claude-haiku-5-20250101-v1:0", false),
            ("us.anthropic.claude-mythos-5-20250101-v1:0", false),
        ] {
            let discovered = bcode_model::ModelInfo {
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
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
                visibility: bcode_model::ModelVisibility::Visible,
            };
            let reasoning = catalog
                .enrich_model("bedrock", discovered)
                .reasoning
                .unwrap_or_else(|| {
                    panic!("{model_id} declares an adaptive thinking mode and must advertise it")
                });
            assert_eq!(
                reasoning.control,
                Some(bcode_model::ReasoningControl::Adaptive),
                "{model_id} must request adaptive thinking"
            );
            assert!(
                !reasoning.effort_values.is_empty(),
                "{model_id} must advertise effort values so an effort can be requested"
            );
            assert_eq!(
                reasoning.default_effort.as_deref(),
                Some("high"),
                "{model_id} must declare a default effort"
            );
            assert!(
                reasoning.raw_reasoning_supported,
                "{model_id} exposes readable reasoning content"
            );
            assert_eq!(
                reasoning.effort_values.iter().any(|value| value == "xhigh"),
                expected_xhigh,
                "{model_id} must only advertise xhigh when the generation accepts it"
            );
        }
    }

    #[test]
    fn models_without_reasoning_or_thinking_mode_advertise_no_reasoning() {
        // Absent both signals, reasoning must stay `None` so non-reasoning models are unaffected.
        assert!(reasoning_from_catalog_parts(None, None).is_none());
    }

    #[test]
    fn bedrock_budget_thinking_models_declare_budget_control() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let discovered = bcode_model::ModelInfo {
            model_id: "us.anthropic.claude-opus-4-1-20250805-v1:0".to_string(),
            display_name: "opus 4.1".to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };
        let reasoning = catalog
            .enrich_model("bedrock", discovered)
            .reasoning
            .expect("Opus 4.1 advertises reasoning");
        assert_eq!(
            reasoning.control,
            Some(bcode_model::ReasoningControl::Budget)
        );
    }

    #[test]
    fn embedded_pricing_uses_bundled_catalog_revision() {
        let document = load_embedded_catalog().expect("embedded catalog should load");
        let revision = document.catalog_revision.clone();
        let catalog = ModelCatalog::new(document);
        let pricing = catalog
            .provider_models_as_model_info("openai")
            .into_iter()
            .find_map(|model| model.pricing)
            .expect("bundled OpenAI pricing exists");

        assert_eq!(pricing.source, ModelPricingSource::BundledCatalog);
        assert_eq!(pricing.revision, Some(revision));
    }

    #[test]
    fn overlay_marks_remote_models_and_remote_values_take_precedence() {
        let mut local = load_embedded_catalog().expect("embedded catalog should load");
        let mut remote = CatalogDocument::empty("remote", "2026-01-01T00:00:00Z");
        let mut provider = local
            .providers
            .get("openai")
            .expect("openai provider exists")
            .clone();
        provider.default_codex_model_id = Some("remote-default".to_string());
        let entry = provider.models.get_mut("gpt-5.6-sol").expect("sol exists");
        entry.display_name = "Remote Sol".to_string();
        entry.context_window = Some(999_999);
        let pricing = entry.pricing.as_mut().expect("pricing exists");
        pricing.input_micros = Some(42);
        pricing.revision = None;
        remote.providers.insert("openai".to_string(), provider);

        overlay_remote_catalog(&mut local, &remote);
        let catalog = ModelCatalog::new(local);
        let provider = catalog.provider("openai").expect("openai provider exists");
        let entry = catalog.model("openai", "gpt-5.6-sol").expect("sol exists");

        assert_eq!(
            provider.default_codex_model_id.as_deref(),
            Some("remote-default")
        );
        assert_eq!(entry.display_name, "Remote Sol");
        assert_eq!(entry.context_window, Some(999_999));
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.input_micros),
            Some(42)
        );
        assert!(entry_is_remote(entry));
        let model = catalog
            .provider_models_as_model_info("openai")
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("sol model info exists");
        assert_eq!(
            model.metadata_source,
            Some(ModelMetadataSource::RemoteCatalog)
        );
        assert_eq!(
            model.pricing.as_ref().map(|pricing| pricing.source),
            Some(ModelPricingSource::RemoteCatalog)
        );
        assert_eq!(
            model.pricing.and_then(|pricing| pricing.revision),
            Some("remote".to_string())
        );
    }

    #[test]
    fn resolver_applies_target_limits_to_explicit_and_expanded_lists() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .build()
            .expect("runtime");
        runtime.block_on(async {
            let options = RemoteCatalogOptions {
                disabled: true,
                ..RemoteCatalogOptions::default()
            };
            let resolver = ModelCatalogResolver::new(options).expect("resolver");
            let target = bcode_model::ModelCatalogSupportHint {
                provider: "openai".to_string(),
                auth_mode: "chatgpt_subscription".to_string(),
                api_surface: "chatgpt_codex".to_string(),
                integration: Some("bcode".to_string()),
            };
            let candidate = ModelInfo {
                model_id: "gpt-5.6-sol".to_string(),
                display_name: "Sol".to_string(),
                is_default: true,
                context_window: None,
                max_output_tokens: None,
                max_image_input_base64_bytes: None,
                capabilities: std::collections::BTreeSet::new(),
                feature_support: bcode_model::ModelFeatureSupport::default(),
                reasoning: None,
                cache: ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                api_surface: None,
                visibility: ModelVisibility::Visible,
            };
            let explicit = resolver
                .resolve(bcode_model::ModelList {
                    models: vec![candidate],
                    catalog: bcode_model::ModelCatalogHints {
                        policy: bcode_model::ModelCatalogPolicy::EnrichOnly {
                            provider_id: "openai".to_string(),
                            target: Some(target.clone()),
                            authority: bcode_model::ModelListAuthority::Explicit,
                        },
                    },
                })
                .await;
            assert_eq!(explicit.models[0].context_window, Some(372_000));

            let expanded = resolver
                .resolve(bcode_model::ModelList {
                    models: Vec::new(),
                    catalog: bcode_model::ModelCatalogHints {
                        policy: bcode_model::ModelCatalogPolicy::ExpandSupported {
                            provider_id: "openai".to_string(),
                            target,
                            authority: bcode_model::ModelListAuthority::Fallback,
                        },
                    },
                })
                .await;
            assert_eq!(
                expanded
                    .models
                    .iter()
                    .find(|model| model.model_id == "gpt-5.6-sol")
                    .and_then(|model| model.context_window),
                Some(372_000)
            );
        });
    }

    #[test]
    fn remote_overlay_updates_only_matching_deployment() {
        let mut local = load_embedded_catalog().expect("embedded catalog should load");
        let mut remote = CatalogDocument::empty("remote", "2026-01-01T00:00:00Z");
        let mut provider = local
            .providers
            .get("openai")
            .expect("openai provider exists")
            .clone();
        let entry = provider.models.get_mut("gpt-5.6-sol").expect("Sol exists");
        let mut chatgpt = entry
            .deployments
            .iter()
            .find(|deployment| deployment.target.api_surface == "chatgpt_codex")
            .expect("ChatGPT deployment")
            .clone();
        chatgpt.context_window = Some(365_000);
        entry.deployments = vec![chatgpt];
        remote.providers.insert("openai".to_string(), provider);

        overlay_remote_catalog(&mut local, &remote);
        let entry = local
            .providers
            .get("openai")
            .and_then(|provider| provider.models.get("gpt-5.6-sol"))
            .expect("merged Sol");
        assert_eq!(entry.deployments.len(), 2);
        assert_eq!(
            entry
                .deployments
                .iter()
                .find(|deployment| deployment.target.api_surface == "chatgpt_codex")
                .and_then(|deployment| deployment.context_window),
            Some(365_000)
        );
        assert_eq!(
            entry
                .deployments
                .iter()
                .find(|deployment| deployment.target.api_surface == "responses_api")
                .and_then(|deployment| deployment.context_window),
            Some(1_050_000)
        );
    }

    #[test]
    fn target_specific_pricing_precedes_model_pricing_and_keeps_revision() {
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let revision = document.catalog_revision.clone();
        let entry = document
            .providers
            .get_mut("openai")
            .and_then(|provider| provider.models.get_mut("gpt-5.6-sol"))
            .expect("Sol entry");
        let deployment = entry
            .deployments
            .iter_mut()
            .find(|deployment| deployment.target.api_surface == "chatgpt_codex")
            .expect("ChatGPT deployment");
        let mut deployment_pricing = entry.pricing.clone().expect("model pricing");
        deployment_pricing.input_micros = Some(123);
        deployment_pricing.revision = Some("deployment-v1".to_string());
        deployment.pricing = Some(deployment_pricing);
        let catalog = ModelCatalog::new(document);
        let target = ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        );

        let resolved = catalog
            .provider_models_for_support_target("openai", &target, false)
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("resolved Sol");
        let pricing = resolved.pricing.expect("resolved pricing");

        assert_eq!(pricing.input.map(|price| price.micros), Some(123));
        assert_eq!(pricing.revision.as_deref(), Some("deployment-v1"));
        assert_ne!(pricing.revision.as_deref(), Some(revision.as_str()));
    }

    #[test]
    fn target_enrichment_preserves_provider_limits_and_uses_deployment_fallback() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let target = ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        );
        let provider_model = ModelInfo {
            model_id: "gpt-5.6-sol".to_string(),
            display_name: "provider Sol".to_string(),
            is_default: false,
            context_window: Some(360_000),
            max_output_tokens: None,
            max_image_input_base64_bytes: None,
            capabilities: std::collections::BTreeSet::new(),
            feature_support: bcode_model::ModelFeatureSupport::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: Some(ModelMetadataSource::ProviderLive),
            pricing: None,
            api_surface: None,
            visibility: ModelVisibility::Visible,
        };

        let resolved = catalog
            .merge_provider_models_for_target("openai", vec![provider_model], false, Some(&target))
            .pop()
            .expect("resolved model");

        assert_eq!(resolved.context_window, Some(360_000));
        assert_eq!(resolved.max_output_tokens, Some(128_000));
        assert_eq!(
            resolved.metadata_source,
            Some(ModelMetadataSource::ProviderLive)
        );
    }

    #[test]
    fn target_aware_live_snapshot_does_not_overwrite_documented_limit() {
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let target = ModelSupportTarget::new(
            "openai",
            "chatgpt_subscription",
            "chatgpt_codex",
            Some("bcode"),
        );
        let mut snapshot = LiveCatalogSnapshot::empty("openai", "2026-01-01T00:00:00Z");
        snapshot.target = Some(target.clone());
        snapshot.models.insert(
            "gpt-5.6-sol".to_string(),
            serde_json::from_value(serde_json::json!({
                "model_id": "gpt-5.6-sol",
                "context_window": 365_000,
                "max_output_tokens": 120_000
            }))
            .expect("live model"),
        );

        merge_live_snapshots(&mut document, &[snapshot]);
        let catalog = ModelCatalog::new(document);
        let entry = catalog.model("openai", "gpt-5.6-sol").expect("Sol entry");
        assert_eq!(entry.context_window, Some(1_050_000));
        let resolved = catalog
            .provider_models_for_support_target("openai", &target, false)
            .into_iter()
            .find(|model| model.model_id == "gpt-5.6-sol")
            .expect("target model");
        assert_eq!(resolved.context_window, Some(365_000));
        assert_eq!(resolved.max_output_tokens, Some(120_000));
    }

    #[test]
    fn catalog_validation_rejects_duplicate_deployment_targets() {
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let model = document
            .providers
            .get_mut("openai")
            .and_then(|provider| provider.models.get_mut("gpt-5.6-sol"))
            .expect("Sol entry");
        model.deployments.push(model.deployments[0].clone());

        let error = validate_catalog(&document).expect_err("duplicate target must fail");
        assert!(error.to_string().contains("duplicate deployment target"));
    }

    #[test]
    fn bedrock_messages_surface_structured_output_decisions_are_inventoried() {
        // The Bedrock Messages adapter serves structured output through a tool-free synthetic
        // round, so an entry that omits `structured_outputs` negotiates as `Unknown` and every
        // structured-output request (for example a loop evaluation node) fails before any
        // provider call. Entries verified to work are pinned here; entries whose behaviour has
        // not been verified are listed explicitly so the gap is visible rather than silent, and
        // adding a new Messages-surface entry without updating this inventory fails the test.
        const VERIFIED: &[&str] = &["anthropic.claude-opus-5", "anthropic.claude-fable-5-1"];
        const UNVERIFIED: &[&str] = &[
            "anthropic.claude-opus-4-7",
            "anthropic.claude-sonnet-5",
            "anthropic.claude-haiku-5",
            "anthropic.claude-fable-5",
            "anthropic.claude-mythos-5",
        ];
        let document = load_embedded_catalog().expect("embedded catalog should load");
        let bedrock = document.providers.get("bedrock").expect("bedrock provider");
        let messages_models = bedrock
            .models
            .values()
            .filter(|model| {
                model.api_surface == Some(bcode_model_catalog_models::CatalogApiSurface::Messages)
                    && model.capabilities.tool_use
            })
            .map(|model| model.model_id.as_str())
            .collect::<std::collections::BTreeSet<_>>();
        let inventoried = VERIFIED
            .iter()
            .chain(UNVERIFIED)
            .copied()
            .collect::<std::collections::BTreeSet<_>>();
        assert_eq!(
            messages_models, inventoried,
            "every tool-using Messages-surface Bedrock entry must be classified as verified or \
             unverified for structured output"
        );
        for model_id in VERIFIED {
            assert!(
                bedrock.models[*model_id].capabilities.structured_outputs,
                "{model_id} is verified to serve structured output and must declare it"
            );
        }
        for model_id in UNVERIFIED {
            assert!(
                !bedrock.models[*model_id].capabilities.structured_outputs,
                "{model_id} declares structured_outputs; move it to VERIFIED once confirmed"
            );
        }
    }

    #[test]
    fn null_live_pricing_does_not_erase_catalog_pricing() {
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let expected = bcode_model_catalog_models::CatalogPricing {
            currency: "USD".to_string(),
            unit: bcode_model_catalog_models::CatalogPricingUnit::PerMillionTokens,
            input_micros: Some(3_000_000),
            cached_input_micros: None,
            cache_write_input_micros: None,
            output_micros: Some(15_000_000),
            context_threshold_tokens: None,
            revision: None,
            rules: Vec::new(),
        };
        document
            .providers
            .get_mut("bedrock")
            .expect("bedrock provider")
            .models
            .get_mut("anthropic.claude")
            .expect("Claude entry")
            .pricing = Some(expected.clone());
        let mut snapshot = LiveCatalogSnapshot::empty("bedrock", "2026-01-01T00:00:00Z");
        snapshot.models.insert(
            "anthropic.claude".to_string(),
            serde_json::from_value(serde_json::json!({
                "model_id": "anthropic.claude",
                "pricing": null
            }))
            .expect("live model"),
        );

        merge_live_snapshots(&mut document, &[snapshot]);

        assert_eq!(
            document.providers["bedrock"].models["anthropic.claude"].pricing,
            Some(expected)
        );
    }

    #[test]
    fn live_bedrock_pricing_reaches_glob_enriched_family_models() {
        // `ListFoundationModels` IDs match bundled family globs (`*nova*`, `*llama3*`, ...). Live
        // AWS price-list pricing must still reach the discovered model through that alias match
        // while the family's curated capabilities remain authoritative.
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let nova_pricing = bcode_model_catalog_models::CatalogPricing {
            currency: "USD".to_string(),
            unit: bcode_model_catalog_models::CatalogPricingUnit::PerMillionTokens,
            input_micros: Some(800_000),
            cached_input_micros: Some(200_000),
            cache_write_input_micros: None,
            output_micros: Some(3_200_000),
            context_threshold_tokens: None,
            revision: Some("2026-09-01T00:00:00Z".to_string()),
            rules: Vec::new(),
        };
        let mut snapshot = LiveCatalogSnapshot::empty("bedrock", "2026-09-01T00:00:00Z");
        snapshot.models.insert(
            "amazon.nova-pro-v1:0".to_string(),
            serde_json::from_value(serde_json::json!({
                "model_id": "amazon.nova-pro-v1:0",
                "display_name": "Nova Pro",
                "capabilities": {"text_input": true, "text_output": true, "tool_use": false},
                "pricing": nova_pricing
            }))
            .expect("live model"),
        );
        merge_live_snapshots(&mut document, &[snapshot]);
        let catalog = ModelCatalog::new(document);

        let discovered = bcode_model::ModelInfo {
            model_id: "amazon.nova-pro-v1:0".to_string(),
            display_name: "Nova Pro".to_string(),
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
            visibility: bcode_model::ModelVisibility::Visible,
        };
        let enriched = catalog.enrich_model("bedrock", discovered);
        let pricing = enriched
            .pricing
            .expect("live pricing reaches the Nova Pro model");
        assert_eq!(pricing.input.map(|price| price.micros), Some(800_000));
        assert_eq!(pricing.output.map(|price| price.micros), Some(3_200_000));
        // Family capabilities are curated, so live `tool_use: false` must not remove tool calls.
        assert!(
            enriched
                .capabilities
                .contains(&bcode_model::ModelCapability::ToolCalls)
        );
        assert_eq!(enriched.context_window, Some(300_000));
    }

    #[test]
    fn live_pricing_for_versioned_id_lands_on_the_concrete_catalog_model() {
        // `openai.gpt-oss-120b-1:0` is the on-demand form of the concrete catalog model
        // `openai.gpt-oss-120b`. Pricing must land on that entry (so both the bare Mantle ID and
        // the versioned Converse ID are priced) without creating a duplicate picker entry.
        let mut document = load_embedded_catalog().expect("embedded catalog should load");
        let mut snapshot = LiveCatalogSnapshot::empty("bedrock", "2026-09-01T00:00:00Z");
        snapshot.models.insert(
            "openai.gpt-oss-120b-1:0".to_string(),
            serde_json::from_value(serde_json::json!({
                "model_id": "openai.gpt-oss-120b-1:0",
                "display_name": "gpt-oss-120b",
                "pricing": {
                    "currency": "USD",
                    "unit": "per_million_tokens",
                    "input_micros": 150_000,
                    "output_micros": 600_000
                }
            }))
            .expect("live model"),
        );
        merge_live_snapshots(&mut document, &[snapshot]);
        assert!(
            !document.providers["bedrock"]
                .models
                .contains_key("openai.gpt-oss-120b-1:0")
        );
        let entry = &document.providers["bedrock"].models["openai.gpt-oss-120b"];
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.input_micros),
            Some(150_000)
        );
        assert!(entry.aliases.contains("openai.gpt-oss-120b-1:0"));
        assert!(is_versioned_form_of(
            "openai.gpt-oss-120b-1:0",
            "openai.gpt-oss-120b"
        ));
        assert!(!is_versioned_form_of("amazon.nova-pro-v1:0", "amazon.nova"));
        assert!(!is_versioned_form_of(
            "openai.gpt-oss-120b",
            "openai.gpt-oss-120b"
        ));
    }

    #[test]
    fn merge_can_include_catalog_only_models() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let merged = catalog.merge_provider_models("openai", Vec::new(), true);

        assert!(merged.iter().any(|model| model.model_id == "gpt-4o"));
    }
}
