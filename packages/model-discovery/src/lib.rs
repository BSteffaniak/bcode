#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Generated live model catalog discovery.

pub mod bedrock;
pub mod xai;

use bcode_model_catalog_models::LiveCatalogSnapshot;
use std::path::Path;

/// Result type used by model discovery operations.
pub type Result<T> = std::result::Result<T, Error>;

/// Discovery operation error.
#[derive(Debug)]
pub enum Error {
    /// Filesystem error.
    Io(std::io::Error),
    /// JSON serialization error.
    Json(serde_json::Error),
    /// Provider discovery error.
    Provider(String),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Io(error) => write!(f, "I/O error: {error}"),
            Self::Json(error) => write!(f, "JSON error: {error}"),
            Self::Provider(message) => write!(f, "provider discovery error: {message}"),
        }
    }
}

impl std::error::Error for Error {}

impl From<std::io::Error> for Error {
    fn from(value: std::io::Error) -> Self {
        Self::Io(value)
    }
}

impl From<serde_json::Error> for Error {
    fn from(value: serde_json::Error) -> Self {
        Self::Json(value)
    }
}

/// Write a live snapshot JSON file.
///
/// # Errors
///
/// Returns an error if serialization or file writes fail.
pub fn write_snapshot(path: &Path, snapshot: &LiveCatalogSnapshot) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_vec_pretty(snapshot)?)?;
    Ok(())
}

/// Timestamp used for generated snapshots.
#[must_use]
pub fn generated_at() -> String {
    std::env::var("BCODE_MODEL_DISCOVERY_GENERATED_AT")
        .or_else(|_| std::env::var("BCODE_MODEL_CATALOG_GENERATED_AT"))
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string())
}
