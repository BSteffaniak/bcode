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

    /// Load the checked-in bundled catalog source.
    ///
    /// # Errors
    ///
    /// Returns an error if catalog source loading or validation fails.
    pub fn load_bundled() -> Result<Self> {
        load_catalog(&default_source_dir()).map(Self::new)
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
        let mut document = load_catalog(&default_source_dir())?;
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
    model.display_name.clone_from(&entry.display_name);
    if model.context_window.is_none() && entry.context_window.is_some() {
        model.context_window = entry.context_window;
        model.metadata_source = Some(ModelMetadataSource::BundledCatalog);
    }
    if model.max_output_tokens.is_none() && entry.max_output_tokens.is_some() {
        model.max_output_tokens = entry.max_output_tokens;
        model.metadata_source = Some(ModelMetadataSource::BundledCatalog);
    }
    model
        .capabilities
        .extend(capabilities_from_catalog(&entry.capabilities));
    if model.cache.capabilities.is_empty() {
        model.cache = cache_info_from_catalog(&entry.capabilities);
    }
    if model.pricing.is_none()
        && let Some(pricing) = pricing_from_catalog(entry.pricing.as_ref())
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
        metadata_source: Some(ModelMetadataSource::BundledCatalog),
        pricing: pricing_from_catalog(entry.pricing.as_ref()),
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

fn pricing_from_catalog(pricing: Option<&CatalogPricing>) -> Option<ModelPricingInfo> {
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
        source: ModelPricingSource::BundledCatalog,
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

/// Load a catalog document from a source directory containing provider TOML files.
///
/// # Errors
///
/// Returns an error if:
///
/// * the source directory cannot be read;
/// * a provider TOML file cannot be parsed;
/// * catalog validation fails.
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
        let provider: ProviderCatalog = toml::from_str(&contents)?;
        if provider.provider_id.trim().is_empty() {
            return Err(Error::Validation(format!(
                "provider id is empty in {}",
                path.display()
            )));
        }
        let previous = catalog
            .providers
            .insert(provider.provider_id.clone(), provider);
        if previous.is_some() {
            return Err(Error::Validation(format!(
                "duplicate provider id in {}",
                path.display()
            )));
        }
    }

    validate_catalog(&catalog)?;
    Ok(catalog)
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
    fn catalog_alias_prefixes_match_model_variants() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let entry = catalog
            .model("openai", "gpt-4o-2024-08-06")
            .expect("alias should resolve");

        assert_eq!(entry.model_id, "gpt-4o");
    }

    #[test]
    fn merge_can_include_catalog_only_models() {
        let catalog = ModelCatalog::load_bundled().expect("catalog should load");
        let merged = catalog.merge_provider_models("openai", Vec::new(), true);

        assert!(merged.iter().any(|model| model.model_id == "gpt-4o"));
    }
}
