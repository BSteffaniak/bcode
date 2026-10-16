//! Plugin-owned subscription pooling compatibility contracts.

use serde::{Deserialize, Serialize};

/// Optional authentication service operation. Unsupported means automatic pooling is unavailable.
pub const OP_AUTH_POOL_COMPATIBILITY: &str = "pool_compatibility";

/// Request to the enabled authentication owner, not an arbitrary provider plugin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthPoolCompatibilityRequest {
    /// Supported version is one.
    pub schema_version: u16,
    /// Registered authentication provider.
    pub provider_id: String,
}

/// Provider-owned compatibility group. Only listed interactive methods qualify.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AuthPoolCompatibility {
    /// Supported version is one.
    pub schema_version: u16,
    /// Registered authentication provider.
    pub provider_id: String,
    /// Stable provider-local account compatibility group.
    pub group: String,
    /// Compatible interactive enrollment methods; secret-field methods never qualify.
    pub methods: Vec<String>,
}

impl AuthPoolCompatibilityRequest {
    /// Validate the request before capability lookup.
    ///
    /// # Errors
    /// Returns an error for unsupported versions or invalid provider identifiers.
    pub fn validate(&self) -> Result<(), super::AuthContractError> {
        super::validate_schema("auth pool compatibility request", self.schema_version, 1)?;
        super::validate_id("provider_id", &self.provider_id)
    }
}

impl AuthPoolCompatibility {
    /// Validate bounded identifiers and the exact schema version.
    ///
    /// # Errors
    /// Returns a contract error for unsupported or malformed declarations.
    pub fn validate(&self) -> Result<(), super::AuthContractError> {
        super::validate_schema("auth pool compatibility", self.schema_version, 1)?;
        super::validate_id("provider_id", &self.provider_id)?;
        super::validate_id("group", &self.group)?;
        if self.methods.is_empty() {
            return Err(super::AuthContractError::EmptyCollection { field: "methods" });
        }
        super::validate_count("methods", self.methods.len(), super::MAX_AUTH_METHODS)?;
        let mut methods = std::collections::BTreeSet::new();
        for method in &self.methods {
            super::validate_id("method_id", method)?;
            if !methods.insert(method) {
                return Err(super::AuthContractError::DuplicateId {
                    field: "methods",
                    id: method.clone(),
                });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bounded_versioned_compatibility_rejects_unknown_and_duplicate_methods() {
        let request = AuthPoolCompatibilityRequest {
            schema_version: 1,
            provider_id: "example".to_owned(),
        };
        assert!(request.validate().is_ok());
        assert!(
            AuthPoolCompatibilityRequest {
                schema_version: 2,
                ..request.clone()
            }
            .validate()
            .is_err()
        );
        assert!(
            AuthPoolCompatibilityRequest {
                provider_id: String::new(),
                ..request
            }
            .validate()
            .is_err()
        );
        let mut capability = AuthPoolCompatibility {
            schema_version: 1,
            provider_id: "example".to_owned(),
            group: "subscription".to_owned(),
            methods: vec!["browser".to_owned(), "device".to_owned()],
        };
        assert!(capability.validate().is_ok());
        let encoded = serde_json::to_vec(&capability).unwrap();
        assert_eq!(
            serde_json::from_slice::<AuthPoolCompatibility>(&encoded).unwrap(),
            capability
        );
        capability.methods.push("browser".to_owned());
        assert!(capability.validate().is_err());
        assert!(
            serde_json::from_str::<AuthPoolCompatibilityRequest>(
                r#"{"schema_version":1,"provider_id":"example","future":true}"#
            )
            .is_err()
        );
    }
}
