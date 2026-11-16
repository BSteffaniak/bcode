//! Portable activation contributions for the shared plugin host.
//!
//! These are observations, not registration authority. The host associates a
//! snapshot with its actual loaded instance/activation and publishes it only
//! after activation succeeds. Unknown versions must be rejected, never reset.

use serde::{Deserialize, Serialize};

/// Compatibility version of the product activation contribution snapshot.
pub const ACTIVATION_CONTRIBUTIONS_VERSION: u16 = 1;

/// Product-owned contribution payloads collected from existing author traits.
///
/// Payload bytes retain the owning command/auth contract's representation.
/// No CLI parser, renderer, callback or implementation layout crosses this boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActivationContributions {
    /// Independently evolving product extension schema version.
    pub version: u16,
    /// Serialized `bcode_command::CommandContribution` values.
    pub commands: Vec<Vec<u8>>,
    /// Serialized authentication-provider contributions.
    pub auth_providers: Vec<Vec<u8>>,
}

impl Default for ActivationContributions {
    fn default() -> Self {
        Self {
            version: ACTIVATION_CONTRIBUTIONS_VERSION,
            commands: Vec::new(),
            auth_providers: Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn contribution_bytes_preserve_owning_contract() {
        let snapshot = ActivationContributions {
            commands: vec![vec![0, 255, 128]],
            ..ActivationContributions::default()
        };
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        assert_eq!(
            serde_json::from_slice::<ActivationContributions>(&encoded).unwrap(),
            snapshot
        );
    }

    #[test]
    fn future_versions_remain_observable() {
        let snapshot = ActivationContributions {
            version: 999,
            ..ActivationContributions::default()
        };
        let encoded = serde_json::to_vec(&snapshot).unwrap();
        let decoded: ActivationContributions = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.version, 999);
    }
}
