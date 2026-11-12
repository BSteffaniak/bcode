//! Passive native Codex selection metadata. No process, file, or credential access.
use std::path::{Component, Path, PathBuf};

/// Why copying Codex subscription tokens into Bcode native model execution is blocked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NativeProviderGate {
    /// No shared foreign-writer refresh lease, compare/write protocol, or
    /// supported read-only token broker is verified across Codex and Bcode.
    ForeignRefreshOwnershipUnverified,
}

/// A possible native harness namespace, not evidence of installation or login.
///
/// Present this as an explicit alternative; do not silently select it in place
/// of native Bcode execution. Harness admission must still enforce executable
/// provenance, protocol compatibility, permission placement, tool isolation, and
/// namespace binding through the existing `Launch` / `CodexSession` contracts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeAuthCandidate {
    /// Exact default credential/config namespace under the caller-authorized home.
    pub codex_home: PathBuf,
    /// Existing plugin capable of separately admitted native execution.
    pub harness_plugin_id: &'static str,
    /// Native Bcode subscription reuse is not currently safe to enable.
    pub native_provider_gate: NativeProviderGate,
}

/// Describe the default Codex namespace without probing or launching anything.
///
/// The caller supplies an already authorized home, not an ambient environment
/// override. Custom `CODEX_HOME` requires its own explicit namespace admission.
/// Returned paths still require confinement and ownership checks before access.
/// This API never claims credentials, installation, readiness, or refresh support.
// Repository convention: Option already carries must_use.
#[allow(clippy::must_use_candidate)]
pub fn native_auth_candidate(authorized_home: &Path) -> Option<NativeAuthCandidate> {
    if !authorized_home.is_absolute()
        || authorized_home
            .components()
            .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return None;
    }
    Some(NativeAuthCandidate {
        codex_home: authorized_home.join(".codex"),
        harness_plugin_id: "bcode.codex-harness",
        native_provider_gate: NativeProviderGate::ForeignRefreshOwnershipUnverified,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passive_candidate_is_not_native_provider_authority() {
        let home = std::env::temp_dir().join("synthetic-home-not-probed");
        let candidate = native_auth_candidate(&home).unwrap();
        assert_eq!(candidate.codex_home, home.join(".codex"));
        assert_eq!(candidate.harness_plugin_id, "bcode.codex-harness");
        assert_eq!(
            candidate.native_provider_gate,
            NativeProviderGate::ForeignRefreshOwnershipUnverified
        );
        assert!(native_auth_candidate(Path::new("relative-home")).is_none());
        assert!(native_auth_candidate(&home.join("..")).is_none());
    }
}
