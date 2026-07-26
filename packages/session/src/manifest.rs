//! Current bounded session-manifest metadata.

use bcode_session_models::SessionSummary;
use serde::{Deserialize, Serialize};

/// Current bounded session-manifest metadata schema.
pub const SESSION_MANIFEST_SCHEMA_VERSION: u32 = 2;
/// Current session-format family recorded in bounded manifests.
pub const SESSION_FORMAT_FAMILY: &str = "bcode.session";
/// Current bounded session-format epoch.
pub const CURRENT_SESSION_FORMAT_EPOCH: u32 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFormatMarker {
    pub family: String,
    pub epoch: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionManifest {
    pub schema_version: u32,
    pub session_format: SessionFormatMarker,
    pub summary: SessionSummary,
}
