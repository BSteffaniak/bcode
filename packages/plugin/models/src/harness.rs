//! Typed registration metadata for enabled harness plugins.
use bcode_harness_models::{
    HARNESS_INTERFACE_ID, HARNESS_SCHEMA_VERSION, HarnessContractError, MAX_HARNESS_ID_BYTES,
};
use serde::{Deserialize, Serialize};

/// Manifest-owned portable harness declaration. Host attaches ownership, never trusts payload owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessContribution {
    pub schema_version: u16,
    pub harness_id: String,
    pub display_name: String,
    pub service_interface_id: String,
}
impl HarnessContribution {
    /// Validate portable discovery metadata. Does not load or inspect native programs.
    /// # Errors
    /// Rejects unsupported versions/interfaces, blank or excessive identifiers/labels.
    pub fn validate(&self) -> Result<(), HarnessContractError> {
        if self.schema_version != HARNESS_SCHEMA_VERSION
            || self.service_interface_id != HARNESS_INTERFACE_ID
        {
            return Err(HarnessContractError::UnsupportedVersion);
        }
        for value in [&self.harness_id, &self.display_name] {
            if value.trim().is_empty()
                || value.trim() != value
                || value.len() > MAX_HARNESS_ID_BYTES
                || value.chars().any(char::is_control)
            {
                return Err(HarnessContractError::InvalidIdentity);
            }
        }
        Ok(())
    }
}
/// Canonical plugin identity attached by the host after enabled-plugin discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginOwnedHarnessContribution {
    pub plugin_id: String,
    pub contribution: HarnessContribution,
}
/// Bounded lexicographic discovery; cursor is plugin identity, not a snapshot token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HarnessDiscoveryPage {
    pub harnesses: Vec<PluginOwnedHarnessContribution>,
    pub next_after: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn registration_versions_and_labels_fail_closed() {
        let mut value = HarnessContribution {
            schema_version: 1,
            harness_id: "native".into(),
            display_name: "Native".into(),
            service_interface_id: HARNESS_INTERFACE_ID.into(),
        };
        assert!(value.validate().is_ok());
        value.schema_version = 2;
        assert_eq!(
            value.validate(),
            Err(HarnessContractError::UnsupportedVersion)
        );
        value.schema_version = 1;
        value.display_name = "unsafe\nlabel".into();
        assert_eq!(value.validate(), Err(HarnessContractError::InvalidIdentity));
    }
}
