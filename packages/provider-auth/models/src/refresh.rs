//! Host-coordinated credential refresh contracts. Credential values never appear in Debug.

use crate::{AuthContractError, AuthCredentialUpdateRequest};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Begin a serialized remote refresh before any external token exchange.
pub const OP_BEGIN_REFRESH: &str = "begin_refresh";
/// Durably commit a complete rotation before using or probing the new credentials.
pub const OP_COMMIT_REFRESH: &str = "commit_refresh";

/// Snapshot the invocation used to decide that refresh is necessary.
/// All registered credentials must be present; absent optional values are `None`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRefreshBeginRequest {
    /// Credential-update compatibility version.
    pub schema_version: u16,
    /// Semantic provider identity.
    pub provider_id: String,
    /// Host-owned profile selection.
    pub profile: String,
    /// Complete pre-exchange snapshot, checked against current custody.
    pub credentials: BTreeMap<String, Option<String>>,
}

/// Opaque host-issued identity. It grants no authority outside the bound profile and owner.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRefreshBeginResponse {
    /// Credential-update compatibility version.
    pub schema_version: u16,
    /// Single-use refresh identity; do not log it.
    pub attempt: String,
}

/// Complete replacement of the registered credential record for one admitted attempt.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthRefreshCommitRequest {
    /// Credential-update compatibility version.
    pub schema_version: u16,
    /// Semantic provider identity.
    pub provider_id: String,
    /// Host-owned profile selection.
    pub profile: String,
    /// Identity returned by begin, never a caller-selected lease or revision.
    pub attempt: String,
    /// Every registered credential, including explicit removal of absent optional values.
    pub credentials: BTreeMap<String, Option<String>>,
}

impl AuthRefreshBeginRequest {
    /// Validate bounded input before host ownership checks.
    ///
    /// # Errors
    /// Rejects unsupported versions, invalid identifiers, or oversized credential records.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        AuthCredentialUpdateRequest {
            schema_version: self.schema_version,
            provider_id: self.provider_id.clone(),
            profile: self.profile.clone(),
            credentials: self.credentials.clone(),
        }
        .validate()
    }
}

impl AuthRefreshCommitRequest {
    /// Validate bounded input before host ownership checks.
    ///
    /// # Errors
    /// Rejects invalid attempt identities and invalid credential records.
    pub fn validate(&self) -> Result<(), AuthContractError> {
        super::validate_id("refresh_attempt", &self.attempt)?;
        AuthRefreshBeginRequest {
            schema_version: self.schema_version,
            provider_id: self.provider_id.clone(),
            profile: self.profile.clone(),
            credentials: self.credentials.clone(),
        }
        .validate()
    }
}

macro_rules! redacted_debug {
    ($type:ty, $name:literal) => {
        impl std::fmt::Debug for $type {
            fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(concat!($name, " { <redacted> }"))
            }
        }
    };
}
redacted_debug!(AuthRefreshBeginRequest, "AuthRefreshBeginRequest");
redacted_debug!(AuthRefreshBeginResponse, "AuthRefreshBeginResponse");
redacted_debug!(AuthRefreshCommitRequest, "AuthRefreshCommitRequest");

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION;

    #[test]
    fn refresh_payload_debug_redacts_credentials_and_attempts() {
        let begin = AuthRefreshBeginRequest {
            schema_version: AUTH_CREDENTIAL_UPDATE_SCHEMA_VERSION,
            provider_id: "test".into(),
            profile: "test".into(),
            credentials: BTreeMap::from([("token".into(), Some("secret-value".into()))]),
        };
        assert!(begin.validate().is_ok());
        assert!(!format!("{begin:?}").contains("secret-value"));
        let commit = AuthRefreshCommitRequest {
            schema_version: begin.schema_version,
            provider_id: begin.provider_id,
            profile: begin.profile,
            attempt: "opaque-attempt".into(),
            credentials: begin.credentials,
        };
        assert!(commit.validate().is_ok());
        let debug = format!("{commit:?}");
        assert!(!debug.contains("secret-value"));
        assert!(!debug.contains("opaque-attempt"));
    }
}
