//! Optional provider-owned image file lifecycle verification.
use serde::{Deserialize, Serialize};

/// Optional operation on compatible model-provider interfaces. Older providers reject it.
pub const OP_VERIFY_IMAGE_UPLOAD: &str = "verify_image_upload";

/// An explicit authorization for one bounded image upload/download/delete probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyImageUploadRequest {
    /// Representation version. Only version 1 is supported.
    pub schema_version: u32,
    /// Provider/account selection, resolved normally by the caller.
    pub provider_context: crate::ProviderRequestContext,
    /// Request-only nonsensitive fixture; never persisted by this operation.
    pub image: crate::ImageContent,
    /// Explicit authorization to create and delete a remote file for this probe.
    pub allow_remote_storage: bool,
}

/// Secret-safe lifecycle observation. No file ID or URL leaves provider ownership.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifyImageUploadResponse {
    /// Report representation version (1).
    pub schema_version: u32,
    /// Whether retrieved bytes exactly matched the authorized fixture.
    pub bytes_verified: bool,
    /// Whether the provider confirmed deletion of the created file.
    pub deletion_confirmed: bool,
    /// Raw image payload bytes, excluding multipart and transport overhead.
    pub image_bytes: u64,
    /// Normalized diagnostic code, never upstream response text.
    pub diagnostic: Option<String>,
    /// Whether upload dispatch began; absent in older reports or when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub upload_attempted: Option<bool>,
    /// Whether the receipt confirmed a positive lifetime no longer than the requested hour.
    /// Absent means unverified, including reports from older implementations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expiry_confirmed: Option<bool>,
}
