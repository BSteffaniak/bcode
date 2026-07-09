//! Remote catalog overlay support for models.bmux.dev.

use crate::{Error, Result, merge_live_snapshots};
use bcode_model_catalog_models::{
    CatalogDocument, LiveCatalogSnapshot, LiveModelMetadata, ModelCatalogEntry, ProviderCatalog,
};
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

/// Default remote catalog API root.
pub const DEFAULT_REMOTE_CATALOG_URL: &str = "https://models.bmux.dev";

const DISABLE_REMOTE_ENV: &str = "BCODE_DISABLE_REMOTE_MODEL_CATALOG";
const REMOTE_URL_ENV: &str = "BCODE_MODEL_CATALOG_URL";
const CACHE_DIR_ENV: &str = "BCODE_MODEL_CATALOG_CACHE_DIR";
const DEFAULT_TIMEOUT_SECONDS: u64 = 3;
const DEFAULT_FRESH_SECONDS: u64 = 900;
const DEFAULT_MAX_STALE_SECONDS: u64 = 21_600;

/// Runtime remote catalog overlay configuration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteCatalogOptions {
    /// Base URL for the remote catalog API.
    pub base_url: String,
    /// Filesystem cache directory.
    pub cache_dir: PathBuf,
    /// HTTP request timeout.
    pub timeout: Duration,
    /// Cache age considered fresh.
    pub fresh_for: Duration,
    /// Maximum stale cache age used when refresh fails.
    pub max_stale: Duration,
    /// Whether remote overlays are disabled.
    pub disabled: bool,
}

impl RemoteCatalogOptions {
    /// Build options from environment variables and platform defaults.
    #[must_use]
    pub fn from_env() -> Self {
        Self {
            base_url: std::env::var(REMOTE_URL_ENV)
                .ok()
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| DEFAULT_REMOTE_CATALOG_URL.to_string()),
            cache_dir: std::env::var(CACHE_DIR_ENV)
                .map_or_else(|_| default_cache_dir(), PathBuf::from),
            timeout: Duration::from_secs(DEFAULT_TIMEOUT_SECONDS),
            fresh_for: Duration::from_secs(DEFAULT_FRESH_SECONDS),
            max_stale: Duration::from_secs(DEFAULT_MAX_STALE_SECONDS),
            disabled: std::env::var(DISABLE_REMOTE_ENV)
                .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES")),
        }
    }
}

impl Default for RemoteCatalogOptions {
    fn default() -> Self {
        Self::from_env()
    }
}

/// Fetch/cache client for the remote model catalog API.
#[derive(Debug, Clone)]
pub struct RemoteCatalogClient {
    options: RemoteCatalogOptions,
    http: reqwest::Client,
}

impl RemoteCatalogClient {
    /// Create a remote catalog client.
    ///
    /// # Errors
    ///
    /// Returns an error if the HTTP client cannot be constructed.
    pub fn new(options: RemoteCatalogOptions) -> Result<Self> {
        let http = reqwest::Client::builder()
            .timeout(options.timeout)
            .user_agent(concat!("bcode/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|error| Error::RemoteCatalog(error.to_string()))?;
        Ok(Self { options, http })
    }

    /// Read a cached catalog without performing network I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if no usable cache exists or cached JSON is invalid.
    pub fn cached_catalog(&self) -> Result<CatalogDocument> {
        self.read_usable_cache("catalog.json")
    }

    /// Read a cached provider-live snapshot without performing network I/O.
    ///
    /// # Errors
    ///
    /// Returns an error if no usable cache exists or cached JSON is invalid.
    pub fn cached_live_snapshot(&self, provider_id: &str) -> Result<LiveCatalogSnapshot> {
        self.read_usable_cache(&format!("live-{provider_id}.json"))
    }

    fn read_usable_cache<T>(&self, cache_name: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.options.disabled {
            return Err(Error::RemoteCatalog(
                "remote model catalog is disabled".to_string(),
            ));
        }
        let path = self.options.cache_dir.join(cache_name);
        if !cache_is_usable_stale(&path, self.options.max_stale) {
            return Err(Error::RemoteCatalog(
                "no usable remote model catalog cache".to_string(),
            ));
        }
        read_cached_json(&path)
    }

    /// Fetch the merged remote catalog, using a bounded cache fallback.
    ///
    /// # Errors
    ///
    /// Returns an error if no usable remote response or cache entry is available.
    pub async fn fetch_catalog(&self) -> Result<CatalogDocument> {
        self.fetch_json("catalog.json", "/api/v1/catalog").await
    }

    /// Fetch a provider live snapshot, using a bounded cache fallback.
    ///
    /// # Errors
    ///
    /// Returns an error if no usable remote response or cache entry is available.
    pub async fn fetch_live_snapshot(&self, provider_id: &str) -> Result<LiveCatalogSnapshot> {
        self.fetch_json(
            &format!("live-{provider_id}.json"),
            &format!("/api/v1/live/{provider_id}"),
        )
        .await
    }

    async fn fetch_json<T>(&self, cache_name: &str, path: &str) -> Result<T>
    where
        T: serde::de::DeserializeOwned,
    {
        if self.options.disabled {
            return Err(Error::RemoteCatalog(
                "remote model catalog is disabled".to_string(),
            ));
        }

        let cache_path = self.options.cache_dir.join(cache_name);
        if cache_is_fresh(&cache_path, self.options.fresh_for) {
            return read_cached_json(&cache_path);
        }

        match self.fetch_remote(path).await {
            Ok(body) => {
                if let Some(parent) = cache_path.parent() {
                    fs::create_dir_all(parent)?;
                }
                fs::write(&cache_path, body.as_bytes())?;
                serde_json::from_str(&body).map_err(Error::Json)
            }
            Err(error) if cache_is_usable_stale(&cache_path, self.options.max_stale) => {
                read_cached_json(&cache_path).map_err(|cache_error| {
                    Error::RemoteCatalog(format!(
                        "remote fetch failed ({error}); stale cache was unreadable ({cache_error})"
                    ))
                })
            }
            Err(error) => Err(error),
        }
    }

    async fn fetch_remote(&self, path: &str) -> Result<String> {
        let url = format!("{}{}", self.options.base_url.trim_end_matches('/'), path);
        let response = self
            .http
            .get(url)
            .send()
            .await
            .map_err(|error| Error::RemoteCatalog(error.to_string()))?
            .error_for_status()
            .map_err(|error| Error::RemoteCatalog(error.to_string()))?;
        response
            .text()
            .await
            .map_err(|error| Error::RemoteCatalog(error.to_string()))
    }
}

/// Overlay remote catalog data onto a bundled catalog document.
pub fn overlay_remote_catalog(local: &mut CatalogDocument, remote: &CatalogDocument) {
    for (provider_id, remote_provider) in &remote.providers {
        match local.providers.get_mut(provider_id) {
            Some(local_provider) => overlay_provider(local_provider, remote_provider),
            None => {
                local
                    .providers
                    .insert(provider_id.clone(), remote_provider.clone());
            }
        }
    }
}

/// Overlay remote live snapshots onto a catalog document.
pub fn overlay_remote_live(local: &mut CatalogDocument, snapshots: &[LiveCatalogSnapshot]) {
    merge_live_snapshots(local, snapshots);
    for snapshot in snapshots {
        if let Some(provider) = local.providers.get_mut(&snapshot.provider_id) {
            for model in provider.models.values_mut() {
                if let Some(live) = &mut model.live
                    && live.source.as_deref() == Some("provider_live")
                {
                    live.source = Some("remote_provider_live".to_string());
                }
            }
        }
    }
}

fn overlay_provider(local: &mut ProviderCatalog, remote: &ProviderCatalog) {
    if remote.website_url.is_some() {
        local.website_url.clone_from(&remote.website_url);
    }
    if remote.default_model_id.is_some() {
        local.default_model_id.clone_from(&remote.default_model_id);
    }
    if remote.default_codex_model_id.is_some() {
        local
            .default_codex_model_id
            .clone_from(&remote.default_codex_model_id);
    }
    if !remote.fallback_model_ids.is_empty() {
        local
            .fallback_model_ids
            .clone_from(&remote.fallback_model_ids);
    }
    if remote.defaults.is_some() {
        local.defaults.clone_from(&remote.defaults);
    }

    for (model_id, remote_entry) in &remote.models {
        if let Some(local_entry) = local.models.get_mut(model_id) {
            overlay_entry(local_entry, remote_entry);
        } else {
            let mut entry = remote_entry.clone();
            mark_remote_only(&mut entry);
            local.models.insert(model_id.clone(), entry);
        }
    }
}

fn overlay_entry(local: &mut ModelCatalogEntry, remote: &ModelCatalogEntry) {
    let local_supported_by = local.supported_by.clone();
    local.display_name.clone_from(&remote.display_name);
    local.aliases.clone_from(&remote.aliases);
    local.status = remote.status;
    local.bcode_support = remote.bcode_support;
    local.context_window = remote.context_window.or(local.context_window);
    local.max_output_tokens = remote.max_output_tokens.or(local.max_output_tokens);
    if remote.family.is_some() {
        local.family.clone_from(&remote.family);
    }
    if remote.provider_model_kind.is_some() {
        local
            .provider_model_kind
            .clone_from(&remote.provider_model_kind);
    }
    if remote.replaced_by.is_some() {
        local.replaced_by.clone_from(&remote.replaced_by);
    }
    if remote.notes.is_some() {
        local.notes.clone_from(&remote.notes);
    }
    if remote.documentation_url.is_some() {
        local
            .documentation_url
            .clone_from(&remote.documentation_url);
    }
    if remote.pricing.is_some() {
        local.pricing.clone_from(&remote.pricing);
    }
    local.capabilities = remote.capabilities.clone();
    if remote.reasoning.is_some() {
        local.reasoning.clone_from(&remote.reasoning);
    }
    local.supported_by.clone_from(&remote.supported_by);
    local.supported_by.extend(local_supported_by);
    local.live = Some(remote_live_metadata(
        remote.live.clone().unwrap_or_default(),
    ));
}

fn mark_remote_only(entry: &mut ModelCatalogEntry) {
    entry.live = Some(remote_live_metadata(entry.live.clone().unwrap_or_default()));
}

fn remote_live_metadata(mut live: LiveModelMetadata) -> LiveModelMetadata {
    live.source = Some(match live.source.as_deref() {
        Some("provider_live") => "remote_provider_live".to_string(),
        Some(source) if source.starts_with("remote_") => source.to_string(),
        Some(source) => format!("remote_{source}"),
        None => "remote_catalog".to_string(),
    });
    live
}

fn read_cached_json<T>(path: &PathBuf) -> Result<T>
where
    T: serde::de::DeserializeOwned,
{
    serde_json::from_str(&fs::read_to_string(path)?).map_err(Error::Json)
}

fn cache_is_fresh(path: &PathBuf, fresh_for: Duration) -> bool {
    cache_age(path).is_some_and(|age| age <= fresh_for)
}

fn cache_is_usable_stale(path: &PathBuf, max_stale: Duration) -> bool {
    cache_age(path).is_some_and(|age| age <= max_stale)
}

fn cache_age(path: &PathBuf) -> Option<Duration> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    modified.elapsed().ok()
}

fn default_cache_dir() -> PathBuf {
    if let Ok(value) = std::env::var("XDG_CACHE_HOME")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value).join("bcode/model-catalog/remote");
    }
    if let Ok(value) = std::env::var("HOME")
        && !value.trim().is_empty()
    {
        return PathBuf::from(value).join(".cache/bcode/model-catalog/remote");
    }
    std::env::temp_dir().join("bcode/model-catalog/remote")
}
