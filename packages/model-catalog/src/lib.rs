#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Model catalog loading, validation, and static artifact generation.

use bcode_model_catalog_models::{CatalogDocument, ProviderCatalog};
use serde_json::json;
use std::fmt::{Display, Formatter};
use std::fs;
use std::path::{Path, PathBuf};

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
    /// Validation error.
    Validation(String),
}

impl Display for Error {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Toml(error) => write!(f, "TOML error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
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

/// Output format for generated artifacts.
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
    let catalog = load_catalog(source_dir)?;
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
    PathBuf::from("catalog/models")
}
