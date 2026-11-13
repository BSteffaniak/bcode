//! Portable contracts for explicitly approved, in-place reuse of credentials owned by other
//! local applications (for example another coding agent's `auth.json`).
//!
//! Providers declare where such documents live and how to interpret them. The host discovers
//! them by presence only, asks for consent, and reads them at dispatch. Nothing here carries
//! credential values; Bcode never copies, refreshes, or rewrites the source document.
use serde::{Deserialize, Serialize};

use crate::{
    AuthContractError, MAX_AUTH_LABEL_BYTES, MAX_AUTH_SECRET_FIELDS, MAX_AUTH_TEXT_BYTES,
    validate_count, validate_id, validate_storage_key, validate_text,
};

/// Application boundary for external connection consent. Implementations authorize requests
/// before mutation and never return credential material.
pub trait ExternalAuthApplication {
    /// Discover declared documents of enabled providers, by presence only.
    fn discover_external_auth(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ExternalAuthCandidate>, ExternalAuthError>> + Send;
    /// Persist approval of a discovered connection the user explicitly confirmed.
    fn approve_external_auth(
        &self,
        candidate: ExternalAuthCandidate,
    ) -> impl std::future::Future<Output = Result<ExternalAuthGrant, ExternalAuthError>> + Send;
    /// List approvals, including stale ones available for revocation.
    fn list_external_auth(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ExternalAuthGrant>, ExternalAuthError>> + Send;
    /// Revoke one approval without reading its source.
    fn revoke_external_auth(
        &self,
        binding_id: String,
    ) -> impl std::future::Future<Output = Result<bool, ExternalAuthError>> + Send;
    /// Approved connections with their current local status. Reads approved sources only.
    fn external_auth_status(
        &self,
    ) -> impl std::future::Future<Output = Result<Vec<ExternalAuthConnection>, ExternalAuthError>> + Send;
}

/// Version of the durable external-connection consent document.
pub const EXTERNAL_AUTH_SCHEMA_VERSION: u16 = 1;

/// Provider-declared credential document owned by another local application.
///
/// The declaration only describes a location and format. It neither reads nor authorizes
/// anything; the host reads it only after the user approves the resulting connection.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthExternalSource {
    /// Provider-local stable identity. Change it when the source's meaning changes, since
    /// approvals bind to it.
    pub source_id: String,
    /// Human-readable owning application, such as `Codex`.
    pub application: String,
    /// Provider method whose credential IDs this document supplies.
    pub method_id: String,
    /// Document location.
    pub location: AuthExternalLocation,
    /// Exact `(pointer, value)` the document must contain before it is interpreted.
    #[serde(default)]
    pub discriminator: Option<(String, String)>,
    /// JSON pointers that must be absent or null, rejecting alternate or mixed documents.
    #[serde(default)]
    pub required_absent_pointers: Vec<String>,
    /// Credentials read from the document.
    pub credentials: Vec<AuthExternalCredential>,
    /// Optional pointer to a stable account identity. Approval records a fingerprint of it;
    /// a different account behind the same source then requires renewed approval.
    #[serde(default)]
    pub account_pointer: Option<String>,
    /// Constraints every credential value must satisfy.
    pub literal_policy: AuthLiteralTokenPolicy,
    /// Optional expiry, checked locally so an expired sign-in is reported before a request.
    #[serde(default)]
    pub expiry: Option<AuthExternalExpiry>,
}

/// Where a document records when its credentials stop working.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthExternalExpiry {
    /// JSON pointer to the value.
    pub pointer: String,
    /// How to read the value.
    pub format: AuthExternalExpiryFormat,
}

/// Encoding of an [`AuthExternalExpiry`] value.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthExternalExpiryFormat {
    /// A JWT string whose payload `exp` claim is the expiry in Unix seconds.
    JwtExp,
    /// A number of Unix seconds.
    UnixSeconds,
}

/// Where an external document lives.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthExternalLocation {
    /// Path beneath the user's home directory.
    pub relative_path: String,
    /// Optional environment variable naming the owning application's configured root, such
    /// as `CODEX_HOME`. When set to an absolute directory it replaces the home-relative path.
    #[serde(default)]
    pub root_override: Option<AuthExternalRootOverride>,
}

/// Application-configured document root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthExternalRootOverride {
    /// Environment variable holding an absolute directory.
    pub env: String,
    /// Path beneath that directory.
    pub relative_path: String,
}

/// One credential read from an external document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthExternalCredential {
    /// Provider credential ID delivered to the method, such as `access_token`.
    pub credential_id: String,
    /// JSON pointer to a string value.
    pub pointer: String,
    /// Whether the document may omit this credential.
    #[serde(default)]
    pub optional: bool,
}

/// Provider-selected constraints for values that may contain dynamic references.
/// This policy only rejects values; it never expands variables or executes commands.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "independent additive rejection constraints, not mutually exclusive states"
)]
pub struct AuthLiteralTokenPolicy {
    /// Reject values starting with the command marker `!`.
    pub reject_command_prefix: bool,
    /// Reject any dollar sign, including shell/environment expansions.
    pub reject_dollar_expansions: bool,
    /// Reject Unicode whitespace or control characters anywhere in the value.
    pub reject_whitespace_or_control: bool,
    /// Reject bare ASCII environment identifiers (`[A-Za-z_][A-Za-z0-9_]*`).
    pub reject_environment_identifier: bool,
}

impl AuthLiteralTokenPolicy {
    /// Whether the value is a non-empty literal permitted by every declared constraint.
    #[must_use]
    pub fn permits(&self, value: &str) -> bool {
        let mut bytes = value.bytes();
        let environment_identifier = bytes
            .next()
            .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
            && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        !(value.trim().is_empty()
            || (self.reject_command_prefix && value.starts_with('!'))
            || (self.reject_dollar_expansions && value.contains('$'))
            || (self.reject_whitespace_or_control
                && value
                    .chars()
                    .any(|character| character.is_whitespace() || character.is_control()))
            || (self.reject_environment_identifier && environment_identifier))
    }
}

fn validate_relative_path(field: &'static str, path: &str) -> Result<(), AuthContractError> {
    validate_text(field, path, MAX_AUTH_TEXT_BYTES)?;
    if path.starts_with('/')
        || path.contains('\\')
        || path.split('/').any(|part| matches!(part, ".." | "." | ""))
    {
        return Err(AuthContractError::InvalidBounds {
            field,
            min: 1,
            max: 0,
        });
    }
    Ok(())
}

fn validate_pointer(field: &'static str, pointer: &str) -> Result<(), AuthContractError> {
    validate_text(field, pointer, MAX_AUTH_TEXT_BYTES)?;
    if pointer.starts_with('/') {
        Ok(())
    } else {
        Err(AuthContractError::InvalidFlowShape(
            "external source pointers must be absolute JSON pointers",
        ))
    }
}

impl AuthExternalSource {
    /// Validate bounds and shape. This does not touch the filesystem.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid identifiers, labels, paths, pointers, duplicate or
    /// missing credentials, or excessive collection sizes.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_id("source_id", &self.source_id)?;
        validate_id("method_id", &self.method_id)?;
        validate_text("application", &self.application, MAX_AUTH_LABEL_BYTES)?;
        validate_relative_path("relative_path", &self.location.relative_path)?;
        if let Some(root) = &self.location.root_override {
            validate_storage_key(&root.env)?;
            validate_relative_path("root_relative_path", &root.relative_path)?;
        }
        if let Some((pointer, value)) = &self.discriminator {
            validate_pointer("discriminator_pointer", pointer)?;
            validate_text("discriminator_value", value, MAX_AUTH_LABEL_BYTES)?;
        }
        validate_count(
            "required_absent_pointers",
            self.required_absent_pointers.len(),
            MAX_AUTH_SECRET_FIELDS,
        )?;
        for pointer in &self.required_absent_pointers {
            validate_pointer("required_absent_pointer", pointer)?;
        }
        if let Some(pointer) = &self.account_pointer {
            validate_pointer("account_pointer", pointer)?;
        }
        if let Some(expiry) = &self.expiry {
            validate_pointer("expiry_pointer", &expiry.pointer)?;
        }
        if !self
            .credentials
            .iter()
            .any(|credential| !credential.optional)
        {
            return Err(AuthContractError::EmptyCollection {
                field: "credentials",
            });
        }
        validate_count(
            "credentials",
            self.credentials.len(),
            MAX_AUTH_SECRET_FIELDS,
        )?;
        let mut ids = std::collections::BTreeSet::new();
        for credential in &self.credentials {
            validate_id("credential_id", &credential.credential_id)?;
            validate_pointer("credential_pointer", &credential.pointer)?;
            if !ids.insert(credential.credential_id.as_str()) {
                return Err(AuthContractError::DuplicateId {
                    field: "credentials",
                    id: credential.credential_id.clone(),
                });
            }
        }
        Ok(())
    }
}

/// Enabled provider declaration a connection binds to. Revalidated at every use.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct ExternalAuthTarget {
    /// Enabled plugin owner identity.
    pub plugin_id: String,
    /// Declared provider identity within that plugin.
    pub provider_id: String,
    /// Declared method identity.
    pub method_id: String,
    /// Declared external source identity.
    pub source_id: String,
}

/// Discovered connection; deliberately excludes secret-bearing data.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthCandidate {
    /// Stable digest of the target and resolved document location.
    pub binding_id: String,
    /// Current declaration target.
    pub target: ExternalAuthTarget,
    /// Display-only owning application.
    pub application: String,
    /// Whether the document is present. Presence is not format or remote validity.
    pub available: bool,
}

/// Approved connection persisted by Bcode. Contains no credential values.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthGrant {
    /// Approved binding digest.
    pub binding_id: String,
    /// Declaration target revalidated before any source read.
    pub target: ExternalAuthTarget,
    /// Display-only owning application.
    pub application: String,
    /// Truncated SHA-256 of the declared account identity at approval, when declared.
    #[serde(default)]
    pub account_fingerprint: Option<String>,
    /// Unix timestamp when approved.
    pub approved_at_unix: u64,
}

/// Local status of one approved connection; never remote validity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ExternalConnectionStatus {
    /// The document is present, matches the approved account, and has not expired.
    Ready {
        /// Declared expiry in Unix seconds, when the source declares one.
        expires_at_unix: Option<u64>,
    },
    /// The declared expiry has passed; the owning application must renew it.
    Expired,
    /// The document belongs to a different account than the one approved.
    AccountChanged,
    /// The document is missing or no longer in a supported format.
    SourceUnavailable,
    /// The provider no longer declares this connection.
    DeclarationChanged,
}

/// One approval with its current status.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExternalAuthConnection {
    /// The approval.
    pub grant: ExternalAuthGrant,
    /// Its current local status.
    pub status: ExternalConnectionStatus,
    /// Models the provider recommends for the approved method, when known.
    #[serde(default)]
    pub recommended_models: Vec<String>,
}

/// Durable, versioned non-secret consent state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExternalAuthState {
    /// Must equal [`EXTERNAL_AUTH_SCHEMA_VERSION`].
    pub schema_version: u16,
    /// Explicit approvals only.
    pub grants: Vec<ExternalAuthGrant>,
}

impl Default for ExternalAuthState {
    fn default() -> Self {
        Self {
            schema_version: EXTERNAL_AUTH_SCHEMA_VERSION,
            grants: Vec::new(),
        }
    }
}

/// Portable operation failure; secret-free and safe to present.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, thiserror::Error)]
#[serde(rename_all = "snake_case")]
pub enum ExternalAuthError {
    /// Durable state uses an unsupported or malformed version.
    #[error("external connection state is damaged or from an unsupported version")]
    UnsupportedState,
    /// No matching explicit approval exists.
    #[error(
        "this external connection is not approved; connect it from /model or with `bcode auth connect`"
    )]
    NotApproved,
    /// Source is absent or its contents do not match the declared format.
    #[error(
        "the external credential document is missing or in an unsupported format; sign in again in the owning application"
    )]
    SourceUnavailable,
    /// The declared expiry has passed.
    #[error(
        "the sign-in Bcode is using from the owning application has expired; open that application to renew it, then retry"
    )]
    Expired,
    /// The source now belongs to a different account than the one approved.
    #[error(
        "the external credentials now belong to a different account; approve them again from /model or with `bcode auth connect`"
    )]
    AccountChanged,
    /// Enabled plugin declaration is absent or changed.
    #[error(
        "the provider no longer declares this external connection; reconnect or re-enable the provider"
    )]
    DeclarationChanged,
    /// The selected model or endpoint is not a destination the provider recommends for it.
    #[error(
        "this model or endpoint is not one the provider recommends for this external connection; choose a model listed for the connection in /model"
    )]
    DestinationRejected,
    /// Consent state could not be safely persisted or read.
    #[error("external connection state is unavailable")]
    StateUnavailable,
    /// The request is malformed.
    #[error("invalid external connection request")]
    InvalidRequest,
}
