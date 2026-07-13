#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model catalog loading, validation, and static artifact generation.

use bcode_model::{
    ModelCacheCapability, ModelCacheInfo, ModelCapability, ModelInfo, ModelMetadataSource,
    ModelPricingInfo, ModelPricingSource, ModelPricingUnit, ModelReasoningCapabilitySource,
    ModelReasoningInfo, ModelTokenPrice,
};
use bcode_model_catalog_models::{
    BcodeSupportStatus, CatalogCapabilities, CatalogDocument, CatalogModelStatus, CatalogPricing,
    CatalogProviderKind, LiveCatalogSnapshot, LiveModelMetadata, ModelCatalogDefaults,
    ModelCatalogEntry, ModelSupportTarget, ProviderCatalog,
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
            bcode_model::ModelCatalogPolicy::EnrichOnly { provider_id, .. } => {
                catalog.merge_provider_models(provider_id, list.models, false)
            }
            bcode_model::ModelCatalogPolicy::ExpandAll { provider_id } => {
                catalog.merge_provider_models(provider_id, list.models, true)
            }
            bcode_model::ModelCatalogPolicy::ExpandSupported {
                provider_id,
                target,
                ..
            } => {
                let mut resolved = catalog.merge_provider_models(provider_id, list.models, false);
                let mut seen = resolved
                    .iter()
                    .map(|model| model.model_id.clone())
                    .collect::<std::collections::BTreeSet<_>>();
                let target = ModelSupportTarget::new(
                    target.provider.clone(),
                    target.auth_mode.clone(),
                    target.api_surface.clone(),
                    target.integration.clone(),
                );
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
            models.push(bcode_model::ModelInfo {
                model_id: model_id.to_string(),
                display_name: model_id.to_string(),
                is_default: false,
                context_window: None,
                max_output_tokens: None,
                capabilities: std::collections::BTreeSet::new(),
                reasoning: None,
                cache: bcode_model::ModelCacheInfo::default(),
                metadata_source: None,
                pricing: None,
                visibility: bcode_model::ModelVisibility::Visible,
            });
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
        if let Some(entry) = self.model(provider_id, &model.model_id) {
            enrich_from_entry(model, entry)
        } else {
            model
        }
    }

    /// Enrich a provider-discovered model with catalog metadata and provider defaults.
    #[must_use]
    pub fn enrich_model_with_defaults(&self, provider_id: &str, model: ModelInfo) -> ModelInfo {
        if let Some(entry) = self.model(provider_id, &model.model_id) {
            return enrich_from_entry(model, entry);
        }
        if let Some(defaults) = self
            .provider(provider_id)
            .and_then(|provider| provider.defaults.as_ref())
        {
            enrich_from_defaults(model, defaults)
        } else {
            model
        }
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
                    .map(model_info_from_catalog_entry)
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

fn model_matches_support_target(
    entry: &ModelCatalogEntry,
    target: &ModelSupportTarget,
    include_unknown: bool,
) -> bool {
    entry
        .supported_by
        .iter()
        .any(|supported| supported.matches(target))
        || (include_unknown && entry.supported_by.is_empty())
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
                if entry.aliases.contains(model_id) {
                    return Some((usize::MAX, entry));
                }
                entry.aliases.iter().find_map(|alias| {
                    if let Some(needle) = alias
                        .strip_prefix('*')
                        .and_then(|value| value.strip_suffix('*'))
                    {
                        return model_id.contains(needle).then_some((needle.len(), entry));
                    }
                    alias
                        .strip_suffix('*')
                        .filter(|prefix| model_id.starts_with(prefix))
                        .map(|prefix| (prefix.len(), entry))
                })
            })
            .max_by_key(|(prefix_len, _entry)| *prefix_len)
            .map(|(_prefix_len, entry)| entry)
    })
}

fn enrich_from_defaults(mut model: ModelInfo, defaults: &ModelCatalogDefaults) -> ModelInfo {
    if model.context_window.is_none() && defaults.context_window.is_some() {
        model.context_window = defaults.context_window;
        model.metadata_source = Some(ModelMetadataSource::ProviderDefault);
    }
    if model.max_output_tokens.is_none() && defaults.max_output_tokens.is_some() {
        model.max_output_tokens = defaults.max_output_tokens;
        model.metadata_source = Some(ModelMetadataSource::ProviderDefault);
    }
    model
        .capabilities
        .extend(capabilities_from_catalog(&defaults.capabilities));
    if model.cache.capabilities.is_empty() {
        model.cache = cache_info_from_catalog(&defaults.capabilities);
    }
    if model.reasoning.is_none() {
        model.reasoning = reasoning_from_catalog_parts(defaults.reasoning.as_ref());
    }
    model
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
    model
        .capabilities
        .extend(capabilities_from_catalog(&entry.capabilities));
    if model.cache.capabilities.is_empty() {
        model.cache = cache_info_from_catalog(&entry.capabilities);
    }
    if model.pricing.is_none()
        && let Some(pricing) = pricing_from_catalog(entry.pricing.as_ref(), remote)
    {
        model.pricing = Some(pricing);
    }
    if model.reasoning.is_none()
        && let Some(reasoning) = reasoning_from_catalog(entry)
    {
        model.reasoning = Some(reasoning);
    }
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
    model
}

fn model_info_from_catalog_entry(entry: &ModelCatalogEntry) -> ModelInfo {
    let mut model = ModelInfo {
        model_id: entry.model_id.clone(),
        display_name: entry.display_name.clone(),
        is_default: false,
        context_window: entry.context_window,
        max_output_tokens: entry.max_output_tokens,
        capabilities: capabilities_from_catalog(&entry.capabilities),
        reasoning: reasoning_from_catalog(entry),
        cache: cache_info_from_catalog(&entry.capabilities),
        metadata_source: Some(if entry_is_remote(entry) {
            ModelMetadataSource::RemoteCatalog
        } else {
            ModelMetadataSource::BundledCatalog
        }),
        pricing: pricing_from_catalog(entry.pricing.as_ref(), entry_is_remote(entry)),
        visibility: bcode_model::ModelVisibility::Visible,
    };
    if entry.bcode_support == BcodeSupportStatus::Unsupported {
        model.visibility = bcode_model::ModelVisibility::Unsupported {
            reason: "model is marked unsupported by Bcode catalog".to_string(),
        };
    }
    model
}

fn capabilities_from_catalog(
    capabilities: &CatalogCapabilities,
) -> std::collections::BTreeSet<ModelCapability> {
    let mut result = std::collections::BTreeSet::new();
    if capabilities.text_output {
        result.insert(ModelCapability::StreamingText);
    }
    if capabilities.tool_use {
        result.insert(ModelCapability::ToolCalls);
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

fn cache_info_from_catalog(capabilities: &CatalogCapabilities) -> ModelCacheInfo {
    let mut cache = ModelCacheInfo::default();
    if capabilities.prompt_cache {
        cache.capabilities.extend([
            ModelCacheCapability::PromptCacheKey,
            ModelCacheCapability::AutomaticPrefixCache,
            ModelCacheCapability::CacheUsageReporting,
        ]);
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
        source: if remote {
            ModelPricingSource::RemoteCatalog
        } else {
            ModelPricingSource::BundledCatalog
        },
    })
}

fn reasoning_from_catalog(entry: &ModelCatalogEntry) -> Option<ModelReasoningInfo> {
    reasoning_from_catalog_parts(entry.reasoning.as_ref())
}

fn reasoning_from_catalog_parts(
    reasoning: Option<&bcode_model_catalog_models::CatalogReasoning>,
) -> Option<ModelReasoningInfo> {
    let reasoning = reasoning?;
    Some(ModelReasoningInfo {
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

    validate_catalog(&catalog)?;
    Ok(catalog)
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
            write_json(
                &live_output_dir.join(format!("{}.json", snapshot.provider_id)),
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
///
/// # Panics
///
/// Panics if a provider referenced by a snapshot does not exist after auto-creation logic.
pub fn merge_live_snapshots(catalog: &mut CatalogDocument, snapshots: &[LiveCatalogSnapshot]) {
    for snapshot in snapshots {
        // Auto-create provider if it does not exist (live data is the source of truth for new providers)
        if !catalog.providers.contains_key(&snapshot.provider_id) {
            catalog.providers.insert(
                snapshot.provider_id.clone(),
                ProviderCatalog {
                    provider_id: snapshot.provider_id.clone(),
                    display_name: snapshot.provider_id.clone(),
                    kind: CatalogProviderKind::Other,
                    website_url: None,
                    default_model_id: None,
                    default_codex_model_id: None,
                    fallback_model_ids: Vec::new(),
                    defaults: None,
                    error_handling:
                        bcode_model_catalog_models::ProviderErrorHandlingMetadata::default(),
                    models: std::collections::BTreeMap::new(),
                },
            );
        }

        let provider = catalog
            .providers
            .get_mut(&snapshot.provider_id)
            .expect("provider was just inserted or already existed");

        for live_model in snapshot.models.values() {
            let entry = provider
                .models
                .entry(live_model.model_id.clone())
                .or_insert_with(|| live_model_entry(live_model, snapshot));
            if entry.display_name.trim().is_empty()
                && let Some(display_name) = &live_model.display_name
            {
                entry.display_name.clone_from(display_name);
            }
            entry.aliases.extend(live_model.aliases.iter().cloned());
            if entry.context_window.is_none() {
                entry.context_window = live_model.context_window;
            }
            if entry.max_output_tokens.is_none() {
                entry.max_output_tokens = live_model.max_output_tokens;
            }
            if entry.reasoning.is_none() {
                entry.reasoning.clone_from(&live_model.reasoning);
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

fn live_model_entry(
    live_model: &bcode_model_catalog_models::LiveModel,
    snapshot: &LiveCatalogSnapshot,
) -> ModelCatalogEntry {
    ModelCatalogEntry {
        model_id: live_model.model_id.clone(),
        display_name: live_model
            .display_name
            .clone()
            .unwrap_or_else(|| live_model.model_id.clone()),
        aliases: live_model.aliases.clone(),
        status: CatalogModelStatus::Unknown,
        bcode_support: BcodeSupportStatus::Unknown,
        context_window: live_model.context_window,
        max_output_tokens: live_model.max_output_tokens,
        family: None,
        provider_model_kind: None,
        replaced_by: None,
        notes: None,
        documentation_url: None,
        pricing: None,
        capabilities: live_model.capabilities.clone(),
        reasoning: live_model.reasoning.clone(),
        supported_by: std::collections::BTreeSet::new(),
        live: Some(LiveModelMetadata {
            status: live_model.status.clone(),
            regions: live_model.regions.clone(),
            last_seen_at: Some(snapshot.generated_at.clone()),
            source: Some("provider_live".to_string()),
        }),
        source: bcode_model_catalog_models::CatalogSourceMetadata::default(),
    }
}

pub(crate) const fn merge_capabilities(
    left: &CatalogCapabilities,
    right: &CatalogCapabilities,
) -> CatalogCapabilities {
    CatalogCapabilities {
        text_input: left.text_input || right.text_input,
        image_input: left.image_input || right.image_input,
        text_output: left.text_output || right.text_output,
        tool_use: left.tool_use || right.tool_use,
        structured_outputs: left.structured_outputs || right.structured_outputs,
        reasoning: left.reasoning || right.reasoning,
        prompt_cache: left.prompt_cache || right.prompt_cache,
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
            capabilities: std::collections::BTreeSet::default(),
            reasoning: None,
            cache: ModelCacheInfo::default(),
            metadata_source: None,
            pricing: None,
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
    fn openai_fallback_prefers_gpt_5_6_sol_then_terra_then_5_5() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let provider = catalog.provider("openai").expect("openai provider exists");

        assert_eq!(
            provider
                .fallback_model_ids
                .iter()
                .take(3)
                .collect::<Vec<_>>(),
            vec!["gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.5"]
        );
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
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.input_micros),
            Some(5_000_000)
        );
        assert_eq!(
            entry
                .pricing
                .as_ref()
                .and_then(|pricing| pricing.output_micros),
            Some(30_000_000)
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
        let entry = catalog
            .model("openai", "gpt-4o-2024-08-06")
            .expect("alias should resolve");

        assert_eq!(entry.model_id, "gpt-4o");
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
        entry.pricing.as_mut().expect("pricing exists").input_micros = Some(42);
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
            model.pricing.map(|pricing| pricing.source),
            Some(ModelPricingSource::RemoteCatalog)
        );
    }

    #[test]
    fn merge_can_include_catalog_only_models() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let merged = catalog.merge_provider_models("openai", Vec::new(), true);

        assert!(merged.iter().any(|model| model.model_id == "gpt-4o"));
    }
}
