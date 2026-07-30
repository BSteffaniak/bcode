#![cfg_attr(feature = "fail-on-warnings", deny(warnings))]
#![warn(clippy::all, clippy::pedantic, clippy::nursery, clippy::cargo)]
#![allow(clippy::multiple_crate_versions)]

//! Portable, versioned contracts for plugin-owned authentication providers.
//!
//! These types describe provider registration and normalized interactive authentication flows.
//! They intentionally contain no vault access, plugin loading, networking, prompting, or UI logic.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};

/// Current schema version for authentication provider contributions.
pub const AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION: u16 = 1;
/// Plugin service interface for normalized interactive authentication flows.
pub const AUTH_INTERFACE_ID: &str = "bcode.auth/v1";
/// Operation used to begin, continue, or cancel an interactive authentication flow.
pub const OP_AUTH_FLOW: &str = "flow";
/// Current schema version for interactive authentication flow messages.
pub const AUTH_FLOW_SCHEMA_VERSION: u16 = 1;
/// Maximum UTF-8 bytes in an identifier.
pub const MAX_AUTH_ID_BYTES: usize = 64;
/// Maximum UTF-8 bytes in a human-readable label.
pub const MAX_AUTH_LABEL_BYTES: usize = 128;
/// Maximum UTF-8 bytes in descriptive or diagnostic text.
pub const MAX_AUTH_TEXT_BYTES: usize = 4 * 1024;
/// Maximum authentication methods in one provider contribution.
pub const MAX_AUTH_METHODS: usize = 16;
/// Maximum secret fields in one generic enrollment method.
pub const MAX_AUTH_SECRET_FIELDS: usize = 16;
/// Maximum UTF-8 bytes in opaque interactive-flow state.
pub const MAX_AUTH_FLOW_STATE_BYTES: usize = 64 * 1024;
/// Maximum normalized effects returned by one interactive-flow step.
pub const MAX_AUTH_FLOW_EFFECTS: usize = 16;
/// Maximum wait requested by one normalized interactive-flow effect.
pub const MAX_AUTH_WAIT_MILLIS: u64 = 5 * 60 * 1_000;

/// Stable provider registration contributed by one plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthProviderContribution {
    /// Contract schema version.
    pub schema_version: u16,
    /// Globally unique provider ID, such as `exa`.
    pub provider_id: String,
    /// Human-readable provider name.
    pub display_name: String,
    /// Provider-owned authentication methods.
    pub methods: Vec<AuthMethodContribution>,
}

impl AuthProviderContribution {
    /// Validate this contribution against the current contract and bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for an unsupported schema, invalid identifier, invalid text bound,
    /// duplicate method, empty method list, excessive method count, or invalid nested method.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_schema(
            "auth provider contribution",
            self.schema_version,
            AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
        )?;
        validate_id("provider_id", &self.provider_id)?;
        validate_text("display_name", &self.display_name, MAX_AUTH_LABEL_BYTES)?;
        if self.methods.is_empty() {
            return Err(AuthContractError::EmptyCollection { field: "methods" });
        }
        validate_count("methods", self.methods.len(), MAX_AUTH_METHODS)?;
        let mut ids = std::collections::BTreeSet::new();
        for method in &self.methods {
            method.validate()?;
            if !ids.insert(method.method_id()) {
                return Err(AuthContractError::DuplicateId {
                    field: "methods",
                    id: method.method_id().to_owned(),
                });
            }
        }
        Ok(())
    }
}

/// One authentication method exposed by a provider.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthMethodContribution {
    /// Host-prompted enrollment of one or more secret fields.
    SecretFields {
        /// Provider-local method ID, such as `api_key`.
        method_id: String,
        /// Human-readable method name.
        display_name: String,
        /// Secret fields prompted and stored by the host.
        fields: Vec<AuthSecretField>,
        /// Whether this method supports explicit remote verification after local validation.
        #[serde(default)]
        supports_verification: bool,
        /// Whether this method supports explicit remote revocation before local deletion.
        #[serde(default)]
        supports_revocation: bool,
    },
    /// Plugin-driven normalized interactive flow, such as browser OAuth or device code.
    Interactive {
        /// Provider-local method ID.
        method_id: String,
        /// Human-readable method name.
        display_name: String,
        /// Plugin service operation used to begin and continue the flow.
        operation: String,
        /// Credentials this flow may return for host-owned storage.
        #[serde(default)]
        credentials: Vec<AuthCredentialStorage>,
        /// Whether this method supports explicit remote revocation.
        #[serde(default)]
        supports_revocation: bool,
    },
}

impl AuthMethodContribution {
    /// Return the provider-local method ID.
    #[must_use]
    pub fn method_id(&self) -> &str {
        match self {
            Self::SecretFields { method_id, .. } | Self::Interactive { method_id, .. } => method_id,
        }
    }

    /// Validate this method and all nested fields.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid IDs, text, field counts, duplicate credentials, or operations.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        match self {
            Self::SecretFields {
                method_id,
                display_name,
                fields,
                ..
            } => {
                validate_id("method_id", method_id)?;
                validate_text("method.display_name", display_name, MAX_AUTH_LABEL_BYTES)?;
                if fields.is_empty() {
                    return Err(AuthContractError::EmptyCollection { field: "fields" });
                }
                validate_count("fields", fields.len(), MAX_AUTH_SECRET_FIELDS)?;
                let mut credentials = std::collections::BTreeSet::new();
                for field in fields {
                    field.validate()?;
                    if !credentials.insert(field.credential_id.as_str()) {
                        return Err(AuthContractError::DuplicateId {
                            field: "fields",
                            id: field.credential_id.clone(),
                        });
                    }
                }
            }
            Self::Interactive {
                method_id,
                display_name,
                operation,
                credentials,
                ..
            } => {
                validate_id("method_id", method_id)?;
                validate_text("method.display_name", display_name, MAX_AUTH_LABEL_BYTES)?;
                validate_id("operation", operation)?;
                validate_count("credentials", credentials.len(), MAX_AUTH_SECRET_FIELDS)?;
                let mut ids = std::collections::BTreeSet::new();
                for credential in credentials {
                    credential.validate()?;
                    if !ids.insert(credential.credential_id.as_str()) {
                        return Err(AuthContractError::DuplicateId {
                            field: "credentials",
                            id: credential.credential_id.clone(),
                        });
                    }
                }
            }
        }
        Ok(())
    }
}

/// Host-owned storage declaration for a credential returned by an interactive flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthCredentialStorage {
    /// Canonical credential ID returned by the plugin flow.
    pub credential_id: String,
    /// Backend key used in the selected vault profile.
    pub storage_key: String,
}

impl AuthCredentialStorage {
    /// Validate this credential storage declaration.
    ///
    /// # Errors
    ///
    /// Returns an error for an invalid credential ID or storage key.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_id("credential_id", &self.credential_id)?;
        validate_storage_key(&self.storage_key)
    }
}

/// Host-managed secret field declared by a plugin-owned authentication method.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSecretField {
    /// Canonical credential ID delivered to the owning plugin, such as `api_key`.
    pub credential_id: String,
    /// Backend key used in the selected vault profile, such as `PROVIDER_API_KEY`.
    pub storage_key: String,
    /// Human-readable hidden prompt.
    pub prompt: String,
    /// Whether an empty submitted value is accepted.
    #[serde(default)]
    pub optional: bool,
    /// Optional local validation applied before storage.
    #[serde(default)]
    pub validation: AuthSecretValidation,
}

impl AuthSecretField {
    /// Validate this field.
    ///
    /// # Errors
    ///
    /// Returns an error for invalid credential/storage IDs, invalid prompt bounds, or invalid
    /// validation bounds.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_id("credential_id", &self.credential_id)?;
        validate_storage_key(&self.storage_key)?;
        validate_text("prompt", &self.prompt, MAX_AUTH_LABEL_BYTES)?;
        self.validation.validate()
    }
}

/// Local secret validation performed by the host before storage.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthSecretValidation {
    /// Minimum accepted UTF-8 byte length.
    #[serde(default)]
    pub min_bytes: Option<usize>,
    /// Maximum accepted UTF-8 byte length.
    #[serde(default)]
    pub max_bytes: Option<usize>,
    /// Optional required literal prefix.
    #[serde(default)]
    pub required_prefix: Option<String>,
}

impl AuthSecretValidation {
    /// Validate the validation policy itself.
    ///
    /// # Errors
    ///
    /// Returns an error when bounds are inverted or a prefix is empty or exceeds the maximum.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        if let (Some(min), Some(max)) = (self.min_bytes, self.max_bytes)
            && min > max
        {
            return Err(AuthContractError::InvalidBounds {
                field: "secret_validation",
                min,
                max,
            });
        }
        if let Some(prefix) = &self.required_prefix {
            validate_text("required_prefix", prefix, MAX_AUTH_LABEL_BYTES)?;
            if let Some(max) = self.max_bytes
                && prefix.len() > max
            {
                return Err(AuthContractError::LengthExceeded {
                    field: "required_prefix",
                    actual: prefix.len(),
                    max,
                });
            }
        }
        Ok(())
    }

    /// Validate one submitted secret without exposing it in errors.
    ///
    /// # Errors
    ///
    /// Returns a non-secret validation error when the value violates configured bounds or prefix.
    pub fn validate_secret(&self, value: &str) -> Result<(), AuthSecretValidationError> {
        if let Some(min) = self.min_bytes
            && value.len() < min
        {
            return Err(AuthSecretValidationError::TooShort { min });
        }
        if let Some(max) = self.max_bytes
            && value.len() > max
        {
            return Err(AuthSecretValidationError::TooLong { max });
        }
        if let Some(prefix) = &self.required_prefix
            && !value.starts_with(prefix)
        {
            return Err(AuthSecretValidationError::MissingRequiredPrefix);
        }
        Ok(())
    }
}

/// Non-secret local validation failure for a submitted credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum AuthSecretValidationError {
    /// Secret is shorter than allowed.
    TooShort { min: usize },
    /// Secret is longer than allowed.
    TooLong { max: usize },
    /// Secret lacks the provider-declared prefix.
    MissingRequiredPrefix,
}

impl Display for AuthSecretValidationError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::TooShort { min } => write!(formatter, "credential must be at least {min} bytes"),
            Self::TooLong { max } => write!(formatter, "credential must be at most {max} bytes"),
            Self::MissingRequiredPrefix => formatter.write_str("credential has an invalid prefix"),
        }
    }
}

impl std::error::Error for AuthSecretValidationError {}

/// Request to begin or continue one plugin-owned interactive authentication flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthFlowRequest {
    /// Contract schema version.
    pub schema_version: u16,
    /// Provider ID selected through the host registry.
    pub provider_id: String,
    /// Provider-local method ID.
    pub method_id: String,
    /// Auth profile selected by the host.
    pub profile: String,
    /// Flow operation.
    pub operation: AuthFlowOperation,
    /// Opaque plugin-owned continuation state returned by the preceding step.
    #[serde(default)]
    pub state: Option<String>,
    /// Non-secret user response keyed by the prompt ID emitted by the preceding step.
    #[serde(default)]
    pub input: Option<AuthFlowInput>,
    /// Whether the user explicitly requested provider verification.
    #[serde(default)]
    pub verify: bool,
    /// Whether the user explicitly requested remote revocation.
    #[serde(default)]
    pub revoke: bool,
}

impl AuthFlowRequest {
    /// Validate this flow request against current bounds.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, invalid IDs, excessive state/input, or an
    /// inconsistent begin/continue/cancel shape.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_schema(
            "auth flow request",
            self.schema_version,
            AUTH_FLOW_SCHEMA_VERSION,
        )?;
        validate_id("provider_id", &self.provider_id)?;
        validate_id("method_id", &self.method_id)?;
        validate_profile_id(&self.profile)?;
        if let Some(state) = &self.state {
            validate_optional_text("state", state, MAX_AUTH_FLOW_STATE_BYTES)?;
        }
        if let Some(input) = &self.input {
            input.validate()?;
        }
        match self.operation {
            AuthFlowOperation::Begin if self.state.is_some() || self.input.is_some() => Err(
                AuthContractError::InvalidFlowShape("begin requests cannot contain state or input"),
            ),
            AuthFlowOperation::Continue if self.state.is_none() => Err(
                AuthContractError::InvalidFlowShape("continue requests require state"),
            ),
            AuthFlowOperation::Cancel if self.input.is_some() => Err(
                AuthContractError::InvalidFlowShape("cancel requests cannot contain input"),
            ),
            _ => Ok(()),
        }
    }
}

/// Interactive authentication flow operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFlowOperation {
    /// Start a new flow.
    Begin,
    /// Continue a flow from plugin-owned state.
    Continue,
    /// Cancel a flow and release provider-owned transient resources.
    Cancel,
}

/// Non-secret response to a normalized interactive prompt.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthFlowInput {
    /// Prompt ID emitted by the plugin.
    pub prompt_id: String,
    /// User-selected value. Secret inputs use host-managed secret fields instead.
    pub value: String,
}

impl AuthFlowInput {
    fn validate(&self) -> Result<(), AuthContractError> {
        validate_id("prompt_id", &self.prompt_id)?;
        validate_optional_text("input.value", &self.value, MAX_AUTH_TEXT_BYTES)
    }
}

/// Result of one bounded interactive authentication flow step.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthFlowResponse {
    /// Contract schema version.
    pub schema_version: u16,
    /// Flow lifecycle state after applying this step.
    pub status: AuthFlowStatus,
    /// Opaque state required for a subsequent continue or cancel request.
    #[serde(default)]
    pub state: Option<String>,
    /// Normalized effects rendered by the host frontend.
    #[serde(default)]
    pub effects: Vec<AuthFlowEffect>,
    /// Credentials produced by a successful terminal flow, keyed by canonical credential ID.
    /// Values are transient and must be handed directly to host-owned secure storage.
    #[serde(default)]
    pub credentials: BTreeMap<String, String>,
    /// Non-secret diagnostics for the user or host.
    #[serde(default)]
    pub diagnostics: Vec<AuthDiagnostic>,
}

impl AuthFlowResponse {
    /// Validate this response and its lifecycle invariants.
    ///
    /// # Errors
    ///
    /// Returns an error for unsupported versions, excessive payloads, invalid effects or
    /// diagnostics, secret-bearing diagnostics, invalid credential IDs, or inconsistent terminal
    /// state.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        validate_schema(
            "auth flow response",
            self.schema_version,
            AUTH_FLOW_SCHEMA_VERSION,
        )?;
        if let Some(state) = &self.state {
            validate_optional_text("state", state, MAX_AUTH_FLOW_STATE_BYTES)?;
        }
        validate_count("effects", self.effects.len(), MAX_AUTH_FLOW_EFFECTS)?;
        for effect in &self.effects {
            effect.validate()?;
        }
        validate_count("diagnostics", self.diagnostics.len(), MAX_AUTH_FLOW_EFFECTS)?;
        for diagnostic in &self.diagnostics {
            diagnostic.validate()?;
        }
        validate_count(
            "credentials",
            self.credentials.len(),
            MAX_AUTH_SECRET_FIELDS,
        )?;
        for credential_id in self.credentials.keys() {
            validate_id("credential_id", credential_id)?;
        }
        match self.status {
            AuthFlowStatus::Pending if self.state.is_none() => Err(
                AuthContractError::InvalidFlowShape("pending responses require continuation state"),
            ),
            AuthFlowStatus::Succeeded if self.state.is_some() => Err(
                AuthContractError::InvalidFlowShape("successful responses cannot retain state"),
            ),
            AuthFlowStatus::Failed | AuthFlowStatus::Cancelled
                if self.state.is_some() || !self.credentials.is_empty() =>
            {
                Err(AuthContractError::InvalidFlowShape(
                    "failed or cancelled responses cannot retain state or credentials",
                ))
            }
            AuthFlowStatus::Pending if !self.credentials.is_empty() => Err(
                AuthContractError::InvalidFlowShape("pending responses cannot contain credentials"),
            ),
            _ => Ok(()),
        }
    }
}

/// Lifecycle state for an interactive authentication flow.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthFlowStatus {
    /// More host interaction or polling is required.
    Pending,
    /// Flow completed and returned credentials for host-owned storage.
    Succeeded,
    /// Flow reached a stable failure.
    Failed,
    /// Flow was cancelled and cannot be continued.
    Cancelled,
}

/// Renderer-neutral effect produced by an interactive authentication flow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthFlowEffect {
    /// Ask the frontend to open a URL.
    OpenBrowser { url: String },
    /// Display a device authorization URL and user code.
    DisplayDeviceCode {
        verification_url: String,
        user_code: String,
        #[serde(default)]
        expires_in_seconds: Option<u64>,
    },
    /// Ask for a non-secret selection or confirmation.
    Prompt {
        prompt_id: String,
        message: String,
        #[serde(default)]
        choices: Vec<String>,
    },
    /// Wait before continuing the flow.
    Wait { millis: u64 },
    /// Display non-secret informational text.
    Message { message: String },
}

impl AuthFlowEffect {
    fn validate(&self) -> Result<(), AuthContractError> {
        match self {
            Self::OpenBrowser { url } => validate_url(url),
            Self::DisplayDeviceCode {
                verification_url,
                user_code,
                ..
            } => {
                validate_url(verification_url)?;
                validate_text("user_code", user_code, MAX_AUTH_LABEL_BYTES)
            }
            Self::Prompt {
                prompt_id,
                message,
                choices,
            } => {
                validate_id("prompt_id", prompt_id)?;
                validate_text("prompt.message", message, MAX_AUTH_TEXT_BYTES)?;
                validate_count("prompt.choices", choices.len(), MAX_AUTH_SECRET_FIELDS)?;
                for choice in choices {
                    validate_text("prompt.choice", choice, MAX_AUTH_LABEL_BYTES)?;
                }
                Ok(())
            }
            Self::Wait { millis } if *millis > MAX_AUTH_WAIT_MILLIS => {
                Err(AuthContractError::WaitExceeded {
                    actual: *millis,
                    max: MAX_AUTH_WAIT_MILLIS,
                })
            }
            Self::Wait { .. } => Ok(()),
            Self::Message { message } => {
                validate_text("effect.message", message, MAX_AUTH_TEXT_BYTES)
            }
        }
    }
}

/// Structured non-secret authentication diagnostic.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuthDiagnostic {
    /// Stable diagnostic code.
    pub code: String,
    /// Diagnostic severity.
    pub severity: AuthDiagnosticSeverity,
    /// Non-secret user-facing message.
    pub message: String,
    /// Optional non-secret remediation.
    #[serde(default)]
    pub remediation: Option<String>,
}

impl AuthDiagnostic {
    fn validate(&self) -> Result<(), AuthContractError> {
        validate_id("diagnostic.code", &self.code)?;
        validate_text("diagnostic.message", &self.message, MAX_AUTH_TEXT_BYTES)?;
        if let Some(remediation) = &self.remediation {
            validate_text("diagnostic.remediation", remediation, MAX_AUTH_TEXT_BYTES)?;
        }
        Ok(())
    }
}

/// Authentication diagnostic severity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthDiagnosticSeverity {
    /// Informational state.
    Info,
    /// Degraded but potentially usable state.
    Warning,
    /// Operation-blocking state.
    Error,
}

/// Validation error for portable authentication contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AuthContractError {
    /// Contract schema is not supported by this host.
    UnsupportedSchema {
        contract: &'static str,
        expected: u16,
        actual: u16,
    },
    /// Identifier is empty or contains unsupported characters.
    InvalidId { field: &'static str },
    /// A storage key is not a valid environment-shaped vault key.
    InvalidStorageKey,
    /// Required collection is empty.
    EmptyCollection { field: &'static str },
    /// Collection exceeds a contract bound.
    CountExceeded {
        field: &'static str,
        actual: usize,
        max: usize,
    },
    /// Text exceeds a contract bound.
    LengthExceeded {
        field: &'static str,
        actual: usize,
        max: usize,
    },
    /// A collection contains a duplicate local ID.
    DuplicateId { field: &'static str, id: String },
    /// Configured minimum exceeds configured maximum.
    InvalidBounds {
        field: &'static str,
        min: usize,
        max: usize,
    },
    /// Interactive-flow shape conflicts with its lifecycle operation or status.
    InvalidFlowShape(&'static str),
    /// Wait effect exceeds the host contract bound.
    WaitExceeded { actual: u64, max: u64 },
    /// URL is empty, oversized, or not HTTP(S).
    InvalidUrl,
}

impl Display for AuthContractError {
    fn fmt(&self, formatter: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnsupportedSchema {
                contract,
                expected,
                actual,
            } => write!(
                formatter,
                "unsupported {contract} schema version {actual}; expected {expected}"
            ),
            Self::InvalidId { field } => write!(formatter, "invalid {field}"),
            Self::InvalidStorageKey => formatter.write_str("invalid credential storage key"),
            Self::EmptyCollection { field } => write!(formatter, "{field} must not be empty"),
            Self::CountExceeded { field, actual, max } => {
                write!(formatter, "{field} count {actual} exceeds maximum {max}")
            }
            Self::LengthExceeded { field, actual, max } => {
                write!(formatter, "{field} length {actual} exceeds maximum {max}")
            }
            Self::DuplicateId { field, id } => {
                write!(formatter, "duplicate {field} id '{id}'")
            }
            Self::InvalidBounds { field, min, max } => {
                write!(
                    formatter,
                    "invalid {field} bounds: minimum {min} exceeds maximum {max}"
                )
            }
            Self::InvalidFlowShape(message) => formatter.write_str(message),
            Self::WaitExceeded { actual, max } => {
                write!(formatter, "wait {actual}ms exceeds maximum {max}ms")
            }
            Self::InvalidUrl => formatter.write_str("invalid authentication URL"),
        }
    }
}

impl std::error::Error for AuthContractError {}

const fn validate_schema(
    contract: &'static str,
    actual: u16,
    expected: u16,
) -> Result<(), AuthContractError> {
    if actual == expected {
        Ok(())
    } else {
        Err(AuthContractError::UnsupportedSchema {
            contract,
            expected,
            actual,
        })
    }
}

fn validate_id(field: &'static str, value: &str) -> Result<(), AuthContractError> {
    if value.is_empty()
        || value.len() > MAX_AUTH_ID_BYTES
        || !value.bytes().all(|byte| {
            byte.is_ascii_lowercase() || byte.is_ascii_digit() || matches!(byte, b'.' | b'-' | b'_')
        })
    {
        Err(AuthContractError::InvalidId { field })
    } else {
        Ok(())
    }
}

fn validate_profile_id(value: &str) -> Result<(), AuthContractError> {
    validate_id("profile", value)
}

fn validate_storage_key(value: &str) -> Result<(), AuthContractError> {
    if value.is_empty()
        || value.len() > MAX_AUTH_LABEL_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_uppercase() || byte.is_ascii_digit() || byte == b'_')
        || value.as_bytes()[0].is_ascii_digit()
    {
        Err(AuthContractError::InvalidStorageKey)
    } else {
        Ok(())
    }
}

fn validate_text(field: &'static str, value: &str, max: usize) -> Result<(), AuthContractError> {
    if value.trim().is_empty() {
        return Err(AuthContractError::LengthExceeded {
            field,
            actual: value.len(),
            max,
        });
    }
    validate_optional_text(field, value, max)
}

const fn validate_optional_text(
    field: &'static str,
    value: &str,
    max: usize,
) -> Result<(), AuthContractError> {
    if value.len() > max {
        Err(AuthContractError::LengthExceeded {
            field,
            actual: value.len(),
            max,
        })
    } else {
        Ok(())
    }
}

const fn validate_count(
    field: &'static str,
    actual: usize,
    max: usize,
) -> Result<(), AuthContractError> {
    if actual > max {
        Err(AuthContractError::CountExceeded { field, actual, max })
    } else {
        Ok(())
    }
}

fn validate_url(value: &str) -> Result<(), AuthContractError> {
    if value.len() > MAX_AUTH_TEXT_BYTES
        || !(value.starts_with("https://") || value.starts_with("http://"))
    {
        Err(AuthContractError::InvalidUrl)
    } else {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exa_contribution() -> AuthProviderContribution {
        AuthProviderContribution {
            schema_version: AUTH_PROVIDER_CONTRIBUTION_SCHEMA_VERSION,
            provider_id: "exa".to_owned(),
            display_name: "Exa".to_owned(),
            methods: vec![AuthMethodContribution::SecretFields {
                method_id: "api_key".to_owned(),
                display_name: "API key".to_owned(),
                fields: vec![AuthSecretField {
                    credential_id: "api_key".to_owned(),
                    storage_key: "PROVIDER_API_KEY".to_owned(),
                    prompt: "Exa API key".to_owned(),
                    optional: false,
                    validation: AuthSecretValidation {
                        min_bytes: Some(1),
                        max_bytes: Some(512),
                        required_prefix: None,
                    },
                }],
                supports_verification: true,
                supports_revocation: false,
            }],
        }
    }

    fn browser_contribution() -> AuthMethodContribution {
        AuthMethodContribution::Interactive {
            method_id: "browser".to_owned(),
            display_name: "Browser OAuth".to_owned(),
            operation: "flow".to_owned(),
            credentials: vec![
                AuthCredentialStorage {
                    credential_id: "access_token".to_owned(),
                    storage_key: "BCODE_OPENAI_CODEX_ACCESS_TOKEN".to_owned(),
                },
                AuthCredentialStorage {
                    credential_id: "refresh_token".to_owned(),
                    storage_key: "BCODE_OPENAI_CODEX_REFRESH_TOKEN".to_owned(),
                },
            ],
            supports_revocation: false,
        }
    }

    #[test]
    fn interactive_credential_storage_round_trips_and_validates() {
        let method = browser_contribution();
        method.validate().expect("valid interactive credentials");
        let encoded = serde_json::to_vec(&method).expect("serialize interactive method");
        let decoded = serde_json::from_slice::<AuthMethodContribution>(&encoded)
            .expect("deserialize interactive method");
        assert_eq!(decoded, method);
    }

    #[test]
    fn interactive_credential_storage_rejects_duplicates_and_invalid_keys() {
        let mut method = browser_contribution();
        let AuthMethodContribution::Interactive { credentials, .. } = &mut method else {
            unreachable!();
        };
        credentials.push(credentials[0].clone());
        assert!(matches!(
            method.validate(),
            Err(AuthContractError::DuplicateId {
                field: "credentials",
                ..
            })
        ));

        let mut method = browser_contribution();
        let AuthMethodContribution::Interactive { credentials, .. } = &mut method else {
            unreachable!();
        };
        credentials[0].storage_key = "not a storage key".to_owned();
        assert!(matches!(
            method.validate(),
            Err(AuthContractError::InvalidStorageKey)
        ));
    }

    #[test]
    fn contribution_round_trips_and_validates() {
        let contribution = exa_contribution();
        contribution.validate().expect("valid contribution");
        let encoded = serde_json::to_vec(&contribution).expect("serialize contribution");
        let decoded = serde_json::from_slice::<AuthProviderContribution>(&encoded)
            .expect("deserialize contribution");
        assert_eq!(decoded, contribution);
    }

    #[test]
    fn unknown_contribution_schema_is_rejected() {
        let mut contribution = exa_contribution();
        contribution.schema_version += 1;
        assert!(matches!(
            contribution.validate(),
            Err(AuthContractError::UnsupportedSchema { .. })
        ));
    }

    #[test]
    fn invalid_and_duplicate_ids_are_rejected() {
        let mut contribution = exa_contribution();
        contribution.provider_id = "Exa".to_owned();
        assert!(matches!(
            contribution.validate(),
            Err(AuthContractError::InvalidId {
                field: "provider_id"
            })
        ));

        let mut contribution = exa_contribution();
        contribution.methods.push(contribution.methods[0].clone());
        assert!(matches!(
            contribution.validate(),
            Err(AuthContractError::DuplicateId {
                field: "methods",
                ..
            })
        ));
    }

    #[test]
    fn contribution_bounds_are_enforced() {
        let mut contribution = exa_contribution();
        contribution.display_name = "x".repeat(MAX_AUTH_LABEL_BYTES + 1);
        assert!(matches!(
            contribution.validate(),
            Err(AuthContractError::LengthExceeded {
                field: "display_name",
                ..
            })
        ));

        let mut contribution = exa_contribution();
        contribution.methods = (0..=MAX_AUTH_METHODS)
            .map(|index| AuthMethodContribution::Interactive {
                method_id: format!("method-{index}"),
                display_name: "Method".to_owned(),
                operation: "auth-flow".to_owned(),
                credentials: Vec::new(),
                supports_revocation: false,
            })
            .collect();
        assert!(matches!(
            contribution.validate(),
            Err(AuthContractError::CountExceeded {
                field: "methods",
                ..
            })
        ));
    }

    #[test]
    fn validation_errors_never_include_secret_values() {
        let validation = AuthSecretValidation {
            min_bytes: Some(20),
            max_bytes: Some(30),
            required_prefix: Some("exa-".to_owned()),
        };
        let secret = "reflected-secret";
        let error = validation
            .validate_secret(secret)
            .expect_err("secret should fail validation");
        assert!(!error.to_string().contains(secret));
        let encoded = serde_json::to_string(&error).expect("serialize validation error");
        assert!(!encoded.contains(secret));
    }

    #[test]
    fn begin_continue_and_cancel_shapes_are_bounded() {
        let begin = AuthFlowRequest {
            schema_version: AUTH_FLOW_SCHEMA_VERSION,
            provider_id: "openai".to_owned(),
            method_id: "browser".to_owned(),
            profile: "openai".to_owned(),
            operation: AuthFlowOperation::Begin,
            state: None,
            input: None,
            verify: false,
            revoke: false,
        };
        begin.validate().expect("valid begin");

        let mut invalid_continue = begin.clone();
        invalid_continue.operation = AuthFlowOperation::Continue;
        assert!(matches!(
            invalid_continue.validate(),
            Err(AuthContractError::InvalidFlowShape(_))
        ));

        let mut cancel = begin;
        cancel.operation = AuthFlowOperation::Cancel;
        cancel.state = Some("opaque-state".to_owned());
        cancel.validate().expect("valid cancel");
    }

    #[test]
    fn terminal_flow_outcomes_cannot_be_reopened() {
        for status in [AuthFlowStatus::Failed, AuthFlowStatus::Cancelled] {
            let response = AuthFlowResponse {
                schema_version: AUTH_FLOW_SCHEMA_VERSION,
                status,
                state: Some("stale-state".to_owned()),
                effects: Vec::new(),
                credentials: BTreeMap::new(),
                diagnostics: Vec::new(),
            };
            assert!(matches!(
                response.validate(),
                Err(AuthContractError::InvalidFlowShape(_))
            ));
        }

        let response = AuthFlowResponse {
            schema_version: AUTH_FLOW_SCHEMA_VERSION,
            status: AuthFlowStatus::Succeeded,
            state: None,
            effects: Vec::new(),
            credentials: BTreeMap::from([("api_key".to_owned(), "secret".to_owned())]),
            diagnostics: Vec::new(),
        };
        response.validate().expect("valid terminal success");
    }

    #[test]
    fn interactive_effect_bounds_are_enforced() {
        let response = AuthFlowResponse {
            schema_version: AUTH_FLOW_SCHEMA_VERSION,
            status: AuthFlowStatus::Pending,
            state: Some("state".to_owned()),
            effects: vec![AuthFlowEffect::Wait {
                millis: MAX_AUTH_WAIT_MILLIS + 1,
            }],
            credentials: BTreeMap::new(),
            diagnostics: Vec::new(),
        };
        assert!(matches!(
            response.validate(),
            Err(AuthContractError::WaitExceeded { .. })
        ));
    }

    #[test]
    fn unknown_flow_schema_is_rejected_after_round_trip() {
        let response = AuthFlowResponse {
            schema_version: AUTH_FLOW_SCHEMA_VERSION + 1,
            status: AuthFlowStatus::Failed,
            state: None,
            effects: Vec::new(),
            credentials: BTreeMap::new(),
            diagnostics: vec![AuthDiagnostic {
                code: "provider_failed".to_owned(),
                severity: AuthDiagnosticSeverity::Error,
                message: "Provider authentication failed".to_owned(),
                remediation: None,
            }],
        };
        let encoded = serde_json::to_vec(&response).expect("serialize response");
        let decoded =
            serde_json::from_slice::<AuthFlowResponse>(&encoded).expect("deserialize response");
        assert!(matches!(
            decoded.validate(),
            Err(AuthContractError::UnsupportedSchema { .. })
        ));
    }
}
